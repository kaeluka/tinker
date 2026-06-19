use crate::goal::Goal;
// Used only by the test-only `run_goal`/`run_silent` fixtures below; production
// runs sessions through `goal_agent_loop` in `main.rs`.
#[cfg(test)]
use crate::cap::OpenCodeRunner;
#[cfg(test)]
use anyhow::Result;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(test)]
use tokio::sync::mpsc;

// Re-export prompt constants so existing tests and consumers keep working.
pub use crate::prompts::{
    FRESH_DISPATCH_INSTRUCTIONS,
    IMPLEMENTATION_OWNERSHIP_MANDATE,
    MESSAGE_PASSING_AND_PROGRESS_SECTIONS,
    NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE,
    TINKER_DIR_WRITE_RULES,
    VCS_RULES,
};

#[cfg(test)]
pub use crate::prompts::SUMMARY_REQUEST;

/// Unified session event type. Replaces the former per-agent event enums
/// (`GoalEvent` and the implicit TendEvent coupling). All sessions — tend,
/// rummage, jog, and any goal agent — emit these through the single
/// `session_rx` channel in the run loop.
#[derive(Debug)]
pub enum SessionEvent {
    /// A streamed text chunk from the LLM.
    Chunk { goal_id: String, text: String },
    /// The session has finished processing the current message.
    /// `full_output` is the complete assembled reply from the LLM — the
    /// authoritative output, free of chunk-delivery gaps that can occur when
    /// the event channel is under load.
    Done { goal_id: String, full_output: String },
    /// Cleanup of tinker-test-case markers failed before this goal session
    /// could start. `dirty_files` lists files still containing markers;
    /// `error` is set when cleanup itself errored before it could finish.
    CleanupBlocked {
        goal_id: String,
        dirty_files: Vec<std::path::PathBuf>,
        error: Option<String>,
    },
    /// Per-API-call token usage, fired once per LLM round-trip inside the
    /// tool loop. Multiple `TokenUsage` events per dispatch are normal (one
    /// per tool-loop iteration). `cached_tokens` is provider-specific
    /// (OpenAI prompt caching, Anthropic) — `None` when not reported.
    TokenUsage {
        goal_id: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        cached_tokens: Option<u64>,
    },
}

/// Builds a Markdown table of the goal's neighboring goals in the graph —
/// its parent (if any), its declared children, and its related links. Each
/// row carries the neighbor's id and a reason explaining why it might be
/// relevant to pull. Returns an empty string when the goal has no neighbors.
pub fn build_neighborhood_table(goal: &Goal) -> String {
    let mut rows: Vec<(String, String)> = vec![];

    if !goal.parent_id.is_empty() {
        rows.push((
            goal.parent_id.clone(),
            "parent goal — ask tend for broader context and framing".to_string(),
        ));
    }

    for child in &goal.children {
        let reason = if child.reason.is_empty() {
            "child goal (read for sub-aspect details)".to_string()
        } else {
            child.reason.clone()
        };
        rows.push((child.id.clone(), reason));
    }

    for related in &goal.related {
        rows.push((related.id.clone(), related.reason.clone()));
    }

    if rows.is_empty() {
        return String::new();
    }

    let mut table = String::from("| goal-id | reason |\n|---------|--------|\n");
    for (id, reason) in &rows {
        table.push_str(&format!("| `{}` | {} |\n", id, reason));
    }
    table
}

/// Builds the init message for any goal session. This is the single
/// construction path for all sessions (goal agents, tend, rummage, jog —
/// once their descriptions migrate to TOML).
///
/// The message includes:
/// 1. Identity — "You are the agent for goal `<id>`."
/// 2. Navigation — compact goal index and on-demand pull path.
/// 3. Message-passing semantics — how send_message routing works.
/// 4. The goal's own description (WHAT/WHY).
/// 5. Rules (VCS, directory writes, ownership mandate).
/// 6. Neighbor goals — mandatory consultation table (agent-collaboration).
/// 7. Trigger reason (if present).
pub fn session_init_message(goal: &Goal, reason: Option<&str>, compact_index: &str) -> String {
    let table = build_neighborhood_table(goal);
    let neighbors_section = if table.is_empty() {
        String::new()
    } else {
        format!(
            "\n## Neighbor goals\n\n\
             {mandate_preamble}\n\
             \n\
             {table}\n",
            mandate_preamble = NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE,
            table = table,
        )
    };

    // Tend is the goal tree's keeper: it reads and writes `.tinker/` directly
    // and does not own source code. Both rules are wrong for it.
    let (dir_rules, ownership) = if goal.id == "tend" {
        (String::new(), String::new())
    } else {
        (
            format!("- {}\n", TINKER_DIR_WRITE_RULES),
            format!("- {}\n", IMPLEMENTATION_OWNERSHIP_MANDATE),
        )
    };

    let reason_section = reason.map_or(String::new(), |r| {
        format!("\n\n## Reason for triggering\n{}", r)
    });

    crate::prompts::session_init_message(
        &goal.id,
        compact_index,
        &goal.description,
        VCS_RULES,
        &dir_rules,
        &ownership,
        &neighbors_section,
        MESSAGE_PASSING_AND_PROGRESS_SECTIONS,
        FRESH_DISPATCH_INSTRUCTIONS,
        &reason_section,
    )
}

/// Builds the init message for an ephemeral fresh sub-session. Used by the
/// run loop when spawning a sub-session via the spawn_session tool.
///
/// The message includes:
/// 1. Ephemeral identity — session ID and actual dispatcher ID.
/// 2. Label (if provided) — correlation tag the dispatcher used when dispatching.
/// 3. Reply instruction — reply via `send_message` to the dispatcher.
/// 4. Fresh-dispatch capability — same as the dispatcher; sub-sessions may act
///    as coordinators, using `spawn_session` for their own sub-dispatches.
/// 5. Task — the subgoal the dispatcher enclosed in the spawn_session call.
/// 6. Navigation — compact goal index.
/// 7. Message-passing and progress sections.
/// 8. Goal context — the permanent dispatcher goal's description.
/// 9. Rules — VCS, directory writes, ownership mandate.
/// 10. Neighbor table — same adjacency as the dispatcher goal.
pub fn fresh_subsession_init_message(
    dispatcher_goal: &Goal,
    dispatcher_id: &str,
    session_id: &str,
    label: Option<&str>,
    task: &str,
    compact_index: &str,
) -> String {
    let label_clause = match label {
        Some(l) if !l.is_empty() => format!(" Your correlation label is `{l}`."),
        _ => String::new(),
    };

    let table = build_neighborhood_table(dispatcher_goal);
    let neighbors_section = if table.is_empty() {
        String::new()
    } else {
        format!(
            "\n## Neighbor goals\n\n\
             {mandate_preamble}\n\
             \n\
             {table}\n",
            mandate_preamble = NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE,
            table = table,
        )
    };

    crate::prompts::fresh_subsession_init_message(
        session_id,
        dispatcher_id,
        &label_clause,
        task,
        compact_index,
        &dispatcher_goal.id,
        &dispatcher_goal.description,
        MESSAGE_PASSING_AND_PROGRESS_SECTIONS,
        VCS_RULES,
        TINKER_DIR_WRITE_RULES,
        IMPLEMENTATION_OWNERSHIP_MANDATE,
        &neighbors_section,
    )
}

/// Lean variant of `fresh_subsession_init_message` for the Claude backend.
/// The framework preamble (message passing, progress guarantee, rules) is
/// already in the system prompt; this message contains only goal-specific
/// content: identity, label, reply instruction, fresh-dispatch capability,
/// task, compact index, goal context, and neighbor table.
pub fn fresh_subsession_lean_init_message(
    dispatcher_goal: &Goal,
    dispatcher_id: &str,
    session_id: &str,
    label: Option<&str>,
    task: &str,
    compact_index: &str,
) -> String {
    let label_clause = match label {
        Some(l) if !l.is_empty() => format!(" Your correlation label is `{l}`."),
        _ => String::new(),
    };

    let table = build_neighborhood_table(dispatcher_goal);
    let neighbors_section = if table.is_empty() {
        String::new()
    } else {
        format!("\n## Neighbor goals\n\n{}\n", table)
    };

    crate::prompts::fresh_subsession_lean_init_message(
        session_id,
        dispatcher_id,
        &label_clause,
        task,
        compact_index,
        &dispatcher_goal.id,
        &dispatcher_goal.description,
        &neighbors_section,
    )
}

/// Backward-compat alias used by goal-session logging before compact_index was added.
#[cfg(test)]
pub(crate) fn goal_init_message(goal: &Goal, reason: Option<&str>) -> String {
    session_init_message(goal, reason, "[]")
}

#[cfg(test)]
pub async fn run_silent(
    oc: &dyn OpenCodeRunner,
    message: &str,
    session_id: Option<&str>,
    work_dir: &Path,
) -> Result<String> {
    use std::sync::Mutex;
    let buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let buf_clone = buf.clone();
    let on_chunk: Box<dyn FnMut(String) + Send> = Box::new(move |chunk: String| {
        buf_clone.lock().unwrap().push_str(&chunk);
    });
    let on_sid: Box<dyn FnMut(String) + Send> = Box::new(|_| {});
    let on_usage: crate::cap::UsageChunk = Box::new(|_| {});
    oc.run(message, session_id, work_dir, None, on_sid, on_chunk, on_usage, None, None).await?;
    let s = buf.lock().unwrap().clone();
    Ok(s)
}

/// Run a goal session. Returns `(full_output, summary)` where `full_output`
/// is the concatenated text of every chunk the model produced during the main
/// session (before the summary request), and `summary` is the structured
/// summary collected at the end. Both are used by the logger for observability.
///
/// Superseded in production by `goal_agent_loop` in `main.rs`, which runs every
/// session (tend, rummage, jog, and goal agents) through a persistent per-goal
/// loop. Retained as a test fixture for the fresh-session / no-resumption spec.
#[cfg(test)]
pub async fn run_goal(
    goal: Goal,
    reason: Option<String>,
    compact_index: String,
    work_dir: PathBuf,
    tx: mpsc::Sender<SessionEvent>,
    oc: Arc<dyn OpenCodeRunner>,
) -> Result<(String, String)> {
    let goal_id = goal.id.clone();
    let message = session_init_message(&goal, reason.as_deref(), &compact_index);

    // Accumulate full output for the logger's goal_session_finished observable.
    let full_output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let full_output_clone = full_output.clone();
    let tx_txt = tx.clone();
    let gid2 = goal_id.clone();
    let on_sid: Box<dyn FnMut(String) + Send> = Box::new(|_sid: String| {});
    let on_chunk: Box<dyn FnMut(String) + Send> = Box::new(move |chunk: String| {
        full_output_clone.lock().unwrap().push_str(&chunk);
        let _ = tx_txt.try_send(SessionEvent::Chunk {
            goal_id: gid2.clone(),
            text: chunk,
        });
    });
    let on_usage: crate::cap::UsageChunk = Box::new(|_| {});
    let session_id = oc
        .run(&message, None, &work_dir, None, on_sid, on_chunk, on_usage, None, None)
        .await?;

    let output = full_output.lock().unwrap().clone();
    let _ = tx.send(SessionEvent::Done { goal_id: goal_id.clone(), full_output: output.clone() }).await;

    // Intra-dispatch continuity: the structured-summary request continues
    // the same LLM conversation as the main work, so the summary can refer
    // to what just happened. This is not cross-trigger resumption.
    let summary = run_silent(oc.as_ref(), SUMMARY_REQUEST, Some(&session_id), &work_dir)
        .await
        .unwrap_or_default();

    Ok((output, summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_goal(id: &str, description: &str) -> Goal {
        Goal {
            id: id.into(),
            summary: "".into(),
            description: description.into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }
    }

    // spec: goal-sessions decision — "Every goal session dispatch must be
    // accompanied by a reason string … This reason must be passed explicitly
    // into the agent's prompt/context." The init message must surface the
    // reason verbatim when one is given.
    #[test]
    fn test_spec_goal_init_message_includes_reason() {
        let calc = make_goal("calc", "build calc");
        let reason = "user edited the description to mention rounding";
        let init = goal_init_message(&calc, Some(reason));
        assert!(
            init.contains(reason),
            "init message must surface the trigger reason"
        );
    }

    // spec (agent-liveness): the framework preamble must include a Progress
    // guarantee section instructing agents to route denied tool calls via
    // send_message, retry transient errors, and never silently abort.
    #[test]
    fn test_spec_session_init_includes_progress_guarantee() {
        let goal = make_goal("test", "do something");
        let msg = session_init_message(&goal, None, "[]");
        assert!(
            msg.contains("Progress guarantee"),
            "init message must include Progress guarantee section header"
        );
        assert!(
            msg.contains("Tool denial"),
            "Progress guarantee must address tool denial routing"
        );
        assert!(
            msg.contains("silent abort"),
            "Progress guarantee must prohibit silent abort"
        );
        assert!(
            msg.contains("Transient error"),
            "Progress guarantee must address transient errors"
        );
    }

    // spec: goal-sessions decision — "Goal sessions may read version control
    // state … but are strictly prohibited from mutating it (no commit, push,
    // checkout)." The init prompt must carry the VCS_RULES policy text into
    // the agent's context.
    #[test]
    fn test_spec_goal_messages_carry_vcs_read_only_rule() {
        let calc = make_goal("calc", "build calc");
        let init = goal_init_message(&calc, None);
        assert!(
            init.contains(VCS_RULES),
            "init message must carry VCS read-only rule verbatim"
        );
        // Sanity: the rule itself must actually forbid mutation.
        assert!(VCS_RULES.contains("read"));
        assert!(VCS_RULES.to_lowercase().contains("don't mutate")
            || VCS_RULES.to_lowercase().contains("do not mutate")
            || VCS_RULES.to_lowercase().contains("read-only"));
    }

    // spec: goal-sessions decision — "Goal sessions must not read or write
    // `.tinker/goals/`, `.tinker/notes/`, or `.tinker/state/`." The init
    // prompt must carry the directory access restriction into the agent's
    // context so it cannot read or silently mutate the tend-owned directories
    // when dispatched with a narrow scope.
    #[test]
    fn test_spec_goal_messages_carry_tinker_dir_write_restriction() {
        let calc = make_goal("calc", "build calc");
        let init = goal_init_message(&calc, None);
        assert!(
            init.contains(TINKER_DIR_WRITE_RULES),
            "init message must carry tinker-dir write restriction verbatim"
        );
        // The restriction must name the three protected directories.
        assert!(init.contains(".tinker/goals/"));
        assert!(init.contains(".tinker/notes/"));
        assert!(init.contains(".tinker/state/"));
    }

    // spec: implementation-ownership decision — "Goal sessions own the
    // implementation and are expected to demolish and restructure existing
    // code when a better architecture satisfies the goal." The init prompt
    // must carry the ownership mandate so the structural taboo is lifted.
    #[test]
    fn test_spec_goal_messages_carry_ownership_mandate() {
        let calc = make_goal("calc", "build calc");
        let init = goal_init_message(&calc, None);
        assert!(
            init.contains(IMPLEMENTATION_OWNERSHIP_MANDATE),
            "init message must carry ownership mandate verbatim"
        );
        assert!(
            init.to_lowercase().contains("you own the"),
            "init message must convey ownership framing"
        );
        // The mandate must actually assert ownership, not just mention it.
        assert!(
            IMPLEMENTATION_OWNERSHIP_MANDATE
                .to_lowercase()
                .contains("own"),
            "IMPLEMENTATION_OWNERSHIP_MANDATE must use 'own' language"
        );
    }

    // spec: tend exemption — tend is the goal tree's keeper and owns no source
    // code. Its preamble must omit both the directory access restriction (it
    // legitimately reads/writes .tinker/) and the implementation-ownership
    // mandate (it delegates code changes to goal sessions via send_message to rummage).
    #[test]
    fn test_spec_tend_preamble_omits_dir_and_ownership_rules() {
        let mut tend = make_goal("tend", "manage the goal tree");
        tend.id = "tend".into();
        let msg = session_init_message(&tend, None, "[]");
        assert!(
            !msg.contains(TINKER_DIR_WRITE_RULES),
            "tend must not receive the directory access restriction"
        );
        assert!(
            !msg.contains(IMPLEMENTATION_OWNERSHIP_MANDATE),
            "tend must not receive the implementation-ownership mandate"
        );
        // VCS rules still apply to tend.
        assert!(msg.contains(VCS_RULES), "tend must still receive VCS rules");
    }

    // spec: goal-sessions — "After each goal session finishes, tinker folds
    // per-session summaries into a single user-facing message." The session-end
    // prompt must request all four structured parts.
    #[test]
    fn test_spec_summary_request_has_structured_parts() {
        let s = SUMMARY_REQUEST;
        assert!(s.contains("1."), "must request part 1 (accomplishments)");
        assert!(s.contains("2."), "must request part 2 (design changes)");
        assert!(
            s.contains("3."),
            "must request part 3 (decisions beyond the goal)"
        );
        assert!(s.contains("4."), "must request part 4 (how to try it)");
        // "decisions beyond the goal" and "how to try it" are the load-bearing
        // pieces the batch fold-message later consumes.
        assert!(s.to_lowercase().contains("decision"));
        assert!(s.to_lowercase().contains("try"));
    }

    // spec: goal-sessions decision — "The structured summary format MUST
    // require the goal session to explicitly list the `test_spec_` functions
    // it created or modified." This forces accountability for the
    // "Tests as guardrails" coding standard.
    #[test]
    fn test_spec_summary_request_demands_test_spec_listing() {
        let s = SUMMARY_REQUEST;
        assert!(
            s.contains("test_spec_"),
            "summary prompt must literally mention test_spec_ to force LLM to list them"
        );
        // The requirement must be phrased as a MUST so the model treats it as
        // load-bearing rather than optional.
        assert!(
            s.contains("MUST") || s.contains("must"),
            "test_spec_ listing must be expressed as a hard requirement"
        );
    }

    // spec: goal-sessions decision — "Every goal session starts fresh —
    // there is no in-process resumption across triggers." run_goal must
    // always pass None as the session_id for the main call (i.e. always
    // start a brand-new LLM conversation), never carrying over a session_id
    // from a previous dispatch. Intra-dispatch continuity (the summary call
    // reusing the same session) is allowed and tested separately.
    #[tokio::test]
    async fn test_spec_goal_session_always_starts_fresh_no_resumption() {
        use crate::cap::Chunk;
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct RecordingRunner {
            calls: AtomicUsize,
            // first call's session_id arg; second call is the summary continuation
            first_session_id: Mutex<Option<Option<String>>>,
        }

        #[async_trait]
        impl OpenCodeRunner for RecordingRunner {
            async fn run(
                &self,
                _message: &str,
                session_id: Option<&str>,
                _work_dir: &Path,
                _system_prompt: Option<&str>,
                _on_session_id: Chunk,
                _on_chunk: Chunk,
                _on_usage: crate::cap::UsageChunk,
                _send_message: Option<crate::cap::SendMessageFn>,
                _spawn_session: Option<crate::cap::SpawnSessionFn>,
            ) -> std::result::Result<String, crate::cap::RunError> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    *self.first_session_id.lock().unwrap() =
                        Some(session_id.map(String::from));
                }
                Ok("mock-session".into())
            }
        }

        let runner = Arc::new(RecordingRunner {
            calls: AtomicUsize::new(0),
            first_session_id: Mutex::new(None),
        });
        let (tx, _rx) = mpsc::channel(8);
        let goal = make_goal("widget", "build a widget");

        let _ = run_goal(
            goal,
            Some("reason".into()),
            "[]".into(),
            "/work".into(),
            tx,
            runner.clone(),
        )
        .await;

        let first_sid = runner.first_session_id.lock().unwrap().clone();
        assert_eq!(
            first_sid,
            Some(None),
            "run_goal must pass None as session_id for the main call (fresh session, no resumption)"
        );
    }

    // spec: goal-sessions — "On a fresh start the session receives its own
    // goal in full plus a neighborhood table — its parents, children, and
    // `related` goals, each as a `{goal-id, reason}` row." The init message
    // must include a Markdown table with the neighbor ids and reasons so the
    // session can decide what to pull on demand.
    #[test]
    fn test_spec_goal_init_has_neighborhood_table() {
        use crate::goal::{ChildLink, RelatedLink};

        let mut goal = make_goal("calc", "build calc");
        goal.parent_id = "math".into();
        goal.children = vec![ChildLink { id: "rounding".into(), reason: "".into() }];
        goal.related = vec![RelatedLink {
            id: "coding-standards".into(),
            reason: "apply these standards to all source code".into(),
        }];

        let msg = goal_init_message(&goal, None);

        // Own goal text must be present
        assert!(msg.contains("build calc"));

        // Neighborhood table must name all three relationship categories
        assert!(msg.contains("`math`"), "parent must appear in table");
        assert!(msg.contains("`rounding`"), "child must appear in table");
        assert!(msg.contains("`coding-standards`"), "related must appear in table");

        // Related reason must appear verbatim (it's the navigation index)
        assert!(
            msg.contains("apply these standards to all source code"),
            "related reason must appear verbatim so session can judge relevance"
        );

        // Table header must be present
        assert!(msg.contains("| goal-id | reason |"));
    }

    // spec (agent-collaboration): the neighbor section must be a mandatory
    // consultation requirement — imperative, not advisory. Before and during
    // any work that touches a neighbor's scope the agent must send a message
    // to each such neighbor (mechanism-neutral — the tool is taught in the
    // message-passing section), announce what it is doing with design
    // rationale, and await their response before finalizing changes.
    #[test]
    fn test_spec_neighbor_section_is_mandatory_consultation() {
        use crate::goal::RelatedLink;

        let mut goal = make_goal("calc", "build calc");
        goal.related = vec![RelatedLink {
            id: "coding-standards".into(),
            reason: "apply these".into(),
        }];

        let msg = goal_init_message(&goal, None);

        assert!(
            msg.contains("Before and during any work that touches a neighbor's scope"),
            "neighbor section must mandate consultation before and during work that touches a neighbor's scope"
        );
        assert!(
            msg.contains("send a message to each such neighbor"),
            "neighbor section must instruct sending a message to each such neighbor (mechanism-neutral, no @-prefix)"
        );
        assert!(
            msg.contains("Await their response before finalizing changes"),
            "neighbor section must require awaiting neighbor responses before finalizing changes"
        );
        assert!(
            msg.contains("Announce what you are doing"),
            "neighbor section must require announcing work and inviting input"
        );
    }

    // spec (agent-collaboration): the dispatcher is exempt from the consultation
    // mandate — it already knows what the agent is doing. The section must name
    // this exemption so agents don't redundantly notify their dispatcher.
    #[test]
    fn test_spec_neighbor_section_exempts_dispatcher() {
        use crate::goal::RelatedLink;

        let mut goal = make_goal("calc", "build calc");
        goal.related = vec![RelatedLink { id: "foo".into(), reason: "related".into() }];

        let msg = goal_init_message(&goal, None);

        assert!(
            msg.contains("Exclude your dispatcher"),
            "neighbor section must explicitly exempt the dispatcher from consultation"
        );
        assert!(
            msg.contains("already knows what you are doing"),
            "neighbor section must give the reason for the dispatcher exemption"
        );
    }

    // spec (agent-collaboration): conflicts that neither party can resolve must
    // surface to the dispatcher — the section must name this explicitly so
    // agents do not absorb conflicts silently.
    #[test]
    fn test_spec_neighbor_section_names_conflict_escalation() {
        use crate::goal::RelatedLink;

        let mut goal = make_goal("calc", "build calc");
        goal.related = vec![RelatedLink { id: "foo".into(), reason: "related".into() }];

        let msg = goal_init_message(&goal, None);

        assert!(
            msg.contains("dispatcher"),
            "neighbor section must name the dispatcher as the conflict escalation target"
        );
        assert!(
            msg.contains("do not absorb them silently"),
            "neighbor section must explicitly prohibit silent conflict absorption"
        );
    }

    // spec (agent-collaboration): the mandate is only as good as the edge graph.
    // The section must explicitly name missing edges as a graph maintenance
    // failure so agents do not treat absent neighbors as "nothing to consult."
    #[test]
    fn test_spec_neighbor_section_names_graph_maintenance_failure() {
        use crate::goal::RelatedLink;

        let mut goal = make_goal("calc", "build calc");
        goal.related = vec![RelatedLink { id: "foo".into(), reason: "related".into() }];

        let msg = goal_init_message(&goal, None);

        assert!(
            msg.contains("graph maintenance failure"),
            "neighbor section must name missing edges as a graph maintenance failure"
        );
        assert!(
            msg.contains("only as good as the edge graph"),
            "neighbor section must state the mandate is bounded by the edge graph"
        );
    }

    // spec: no-peek — agents do not read goal files to understand neighbors.
    // The init message must direct agents to ask tend (via send_message) when
    // the compact index and edge reasons aren't sufficient — not to fetch TOML files.
    #[test]
    fn test_spec_goal_init_escalates_to_tend_for_neighbor_context() {
        let mut goal = make_goal("calc", "build calc");
        goal.parent_id = "math".into();

        let msg = goal_init_message(&goal, None);

        assert!(
            msg.contains("send_message") && msg.contains("tend"),
            "init message must name send_message to tend as the escalation path for neighbor context"
        );
        assert!(
            !msg.contains("read `.tinker/goals/"),
            "init message must not instruct agents to read goal files"
        );
    }

    // spec: goal-sessions SCOPE — sessions must not write to `.tinker/goals/`.
    // The Subgoals section that previously invited writing TOML files there
    // contradicted this rule and has been removed.
    #[test]
    fn test_spec_goal_init_no_subgoals_section() {
        let goal = make_goal("calc", "build calc");
        let msg = goal_init_message(&goal, None);

        assert!(
            !msg.contains("## Subgoals"),
            "Subgoals section must not appear — it invited writing to .tinker/goals/ \
             which violates the tinker-dir write prohibition"
        );
        // The write prohibition in the Rules section is what blocks this, not a
        // section that both invites and then forbids.
        assert!(
            msg.contains(TINKER_DIR_WRITE_RULES),
            "write prohibition must still be present in Rules"
        );
    }

    // spec: goal-agents preamble — before messaging another agent, an agent must
    // understand the recipient's role via the compact index and edge reasons (no-peek:
    // no goal-file reads). If the index isn't sufficient, tend via send_message is the
    // escalation path. The preamble must encode this dialog-first model.
    #[test]
    fn test_spec_preamble_includes_read_before_message_mandate() {
        let goal = make_goal("widget", "build a widget");
        let msg = session_init_message(&goal, None, "[]");
        assert!(
            msg.contains("Before sending") && msg.contains("compact index"),
            "preamble must direct agents to use the compact index before messaging"
        );
        assert!(
            !msg.contains("read `.tinker/goals/"),
            "preamble must not instruct agents to read goal files directly"
        );
    }

    // spec: goal-agents preamble — completion reports go to the dispatcher (the
    // agent whose message initiated the task), not always to tend. The preamble
    // must name the dispatcher as the recipient and require test_spec_ listing.
    #[test]
    fn test_spec_preamble_reports_to_dispatcher_not_always_tend() {
        let goal = make_goal("widget", "build a widget");
        let msg = session_init_message(&goal, None, "[]");
        assert!(
            msg.contains("dispatcher"),
            "preamble must name the dispatcher as the completion-report target"
        );
        assert!(
            !msg.contains("@tend"),
            "preamble must not use @-notation routing to tend"
        );
        assert!(
            msg.contains("test_spec_"),
            "preamble must require listing test_spec_ functions in the report"
        );
    }

    // spec: goal-agents preamble — three shared agents (tend for intent/should,
    // rummage for code-reality/is, jog for discrepancy finding) must be named
    // as routing rules, not merely "available", so every agent knows when to
    // consult them and that goal-file reads don't substitute for agent queries.
    #[test]
    fn test_spec_preamble_includes_shared_agent_routing_rules() {
        let goal = make_goal("widget", "build a widget");
        let msg = session_init_message(&goal, None, "[]");
        assert!(
            msg.contains("Three shared agents"),
            "preamble must frame shared agents as a routing rule, not availability"
        );
        assert!(
            msg.contains("send_message(target=\"tend\"")
                && msg.contains("send_message(target=\"rummage\"")
                && msg.contains("send_message(target=\"jog\""),
            "preamble must show all three shared agents as send_message targets"
        );
        assert!(
            msg.contains("route questions"),
            "preamble must present shared agents as routing destinations"
        );
    }

    // spec (fresh-agents): the fresh-dispatch section must describe the
    // spawn_session tool, label semantics, and the coordinator model.
    #[test]
    fn test_spec_preamble_includes_fresh_dispatch_instructions() {
        let goal = make_goal("widget", "build a widget");
        let msg = session_init_message(&goal, None, "[]");
        assert!(
            msg.contains("Fresh dispatch"),
            "preamble must include Fresh dispatch section header"
        );
        assert!(
            msg.contains("spawn_session"),
            "fresh dispatch section must include the spawn_session tool"
        );
    }

    // spec (agent-liveness): the init message must contain the ongoing-goal
    // framing and the instruction not to create files speculatively.
    #[test]
    fn test_spec_goal_init_includes_ongoing_framing() {
        let goal = make_goal("widget", "build a widget");
        let msg = session_init_message(&goal, None, "[]");
        assert!(
            msg.contains("ongoing"),
            "init message must describe the goal as ongoing"
        );
        assert!(
            msg.contains("Never create files speculatively"),
            "init message must prohibit speculative file creation"
        );
    }

    // spec (coding-standards, goal-agents): the init message must carry the
    // VCS_RULES text verbatim so agents know they are read-only on version control.
    #[test]
    fn test_spec_vcs_rules_text_matches_storage() {
        // This test is a regression guard: if the prompt file drifts, this fails.
        assert!(
            VCS_RULES.contains("read-only"),
            "VCS_RULES must contain 'read-only' framing"
        );
        assert!(
            VCS_RULES.contains("git status"),
            "VCS_RULES must enumerate permitted git commands"
        );
    }

    // spec (coding-standards, goal-agents): the ownership mandate must be
    // present and assert ownership unambiguously.
    #[test]
    fn test_spec_ownership_mandate_text_matches_storage() {
        assert!(
            IMPLEMENTATION_OWNERSHIP_MANDATE.contains("You own the implementation"),
            "ownership mandate must contain the ownership assertion"
        );
    }

    // spec (coding-standards, goal-agents): the tinker-dir write restriction
    // must name the three protected directories.
    #[test]
    fn test_spec_tinker_dir_rules_text_matches_storage() {
        assert!(TINKER_DIR_WRITE_RULES.contains(".tinker/goals/"));
        assert!(TINKER_DIR_WRITE_RULES.contains(".tinker/notes/"));
        assert!(TINKER_DIR_WRITE_RULES.contains(".tinker/state/"));
    }

    // spec (coding-standards, goal-agents): the neighbor consultation preamble
    // must define the trigger as work touching a neighbor's scope, instruct
    // sending a message to each affected neighbor (mechanism-neutral — the
    // tool is taught in the message-passing section), and name the dispatcher
    // exemption.
    #[test]
    fn test_spec_neighbor_preamble_text_matches_storage() {
        assert!(
            NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE.contains("neighbor's scope"),
            "preamble must define the consultation trigger as work touching a neighbor's scope"
        );
        assert!(
            NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE.contains("Exclude your dispatcher"),
            "preamble must name the dispatcher exemption"
        );
    }
}
