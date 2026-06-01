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

/// Shared VCS-mutation rule, used by the tinker prompt and by the
/// goal-session init message. Tinker and goal sessions alike may
/// read VCS state but never mutate it — version control is the user's job
/// (per `tinker-agent` and `goal-sessions` goals).
pub const VCS_RULES: &str = "Treat version control as read-only. \
You may read state (`git status`, `git diff`, `git log`) to orient yourself, \
but don't mutate it — no commits, pushes, checkouts, branch operations, \
rebases, or stashing. Writing files is fine; the user handles commits.";

/// Directory-write restriction injected into every goal-session prompt.
/// Goal sessions must not write to the tinker-owned directories listed here;
/// those are tinker's exclusive domain (per `goal-sessions`).
pub const TINKER_DIR_WRITE_RULES: &str = "Do not write to `.tinker/goals/`, \
`.tinker/notes/`, or `.tinker/state/`. Those directories are owned by \
tinker. You may read them (e.g. to understand sibling goals or state), \
but must not create, modify, or delete any file inside them.";

/// Implementation-ownership mandate injected into every goal-session prompt.
/// Goal sessions own the source code and should not hesitate to radically
/// restructure or delete code to meet the goal. The human owns the Intent
/// (Goals); the goal session owns the Implementation (source code).
pub const IMPLEMENTATION_OWNERSHIP_MANDATE: &str = "\
You are not a guest in this codebase. You own the implementation. \
If a better architecture requires demolishing and restructuring \
what exists, do it without hesitation. The human owns the Intent \
(Goals); you own the Implementation (source code).";

/// Sentinel prefix written as the first byte of a log line to signal that the
/// line carries the trigger reason and must be rendered in bold. The SOH
/// control character (\x01) never appears in normal LLM output.
pub const TRIGGER_REASON_MARKER: char = '\x01';

#[cfg(test)]
pub const SUMMARY_REQUEST: &str = "\
Provide a structured summary of this session with four parts:

1. What was accomplished. 2-3 sentences. Be specific about which files were created or modified. You MUST also list every `test_spec_` function you created or modified, prefixed by file path (e.g. `src/lexer.rs: test_spec_parses_arithmetic`). If you wrote zero `test_spec_` functions, explain why — and consider whether the `Tests as guardrails` standard (apply where relevant) actually permits that gap.

2. Software design changes. Note modules created or removed, interfaces (the public surfaces other modules depend on — functions, methods, types) added/removed/changed, and data types created or changed. If none, write \"none\".

3. Decisions made beyond the goal description. List any design choices, defaults, library picks, structural decisions, or scope interpretations you made on your own that were NOT specified in the goal. If none, write \"none\".

4. How to try it. One concrete invocation line — the exact command(s) the user can run to see the artifact in action.";

/// Unified session event type. Replaces the former per-agent event enums
/// (`GoalEvent` and the implicit TendEvent coupling). All sessions — tend,
/// rummage, jog, and any goal agent — emit these through the single
/// `session_rx` channel in the run loop.
#[derive(Debug)]
pub enum SessionEvent {
    /// A streamed text chunk from the LLM.
    Chunk { goal_id: String, text: String },
    /// The session has finished processing the current message.
    /// `crashed` is true when the LLM runner returned an error for this turn.
    Done { goal_id: String, crashed: bool },
    /// Cleanup of tinker-test-case markers failed before this goal session
    /// could start. `dirty_files` lists files still containing markers;
    /// `error` is set when cleanup itself errored before it could finish.
    CleanupBlocked {
        goal_id: String,
        dirty_files: Vec<std::path::PathBuf>,
        error: Option<String>,
    },
}

/// Builds a Markdown table of the goal's neighboring goals in the graph —
/// its parent (if any), its declared children, and its related links. Each
/// row carries the neighbor's id and a reason explaining why it might be
/// relevant to pull. Returns an empty string when the goal has no neighbors.
fn build_neighborhood_table(goal: &Goal) -> String {
    let mut rows: Vec<(String, String)> = vec![];

    if !goal.parent_id.is_empty() {
        rows.push((
            goal.parent_id.clone(),
            "parent goal (read for broader context and framing)".to_string(),
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
/// 3. Message-passing semantics — how @goal-id routing works.
/// 4. The goal's own description (WHAT/WHY).
/// 5. Rules (VCS, directory writes, ownership mandate).
/// 6. Trigger reason (if present).
pub fn session_init_message(goal: &Goal, reason: Option<&str>, compact_index: &str) -> String {
    let table = build_neighborhood_table(goal);
    let neighbors_section = if table.is_empty() {
        String::new()
    } else {
        format!(
            "\n## Neighbor goals\n\n\
             Pull any neighbor's full text on demand by reading \
             `.tinker/goals/<goal-id>.toml`. Use the reason column to decide \
             what to pull.\n\n\
             {table}\n"
        )
    };

    let mut prompt = format!(
        "You are the agent for goal `{id}`.\n\
         \n\
         ## Goal index\n\
         \n\
         {compact_index}\n\
         \n\
         Pull any goal's full text on demand by reading `.tinker/goals/<goal-id>.toml`.\n\
         \n\
         ## Message passing\n\
         \n\
         Use `@<goal-id> <message>` to send a message to another agent. Non-`@`-block \
         output is your private working log (rendered in the log pane, not delivered to \
         other agents). `@`-blocks in your reply are extracted after you finish and routed \
         to the named recipients. No blocking calls — replies arrive in the normal message \
         stream. **Reporting completions.** When you complete significant work, report \
         to your dispatcher — the agent whose `@`-message initiated your current task \
         (this can be the user). In your report: what you did, what you decided beyond \
         the goal, how to try the result, every `test_spec_` function you created or \
         modified, and how you collaborated with other agents in fulfilling the task.\n\
         \n\
         **Before sending `@goal-id`, read `.tinker/goals/<goal-id>.toml`.** The goal is \
         the agent's role. You cannot write a useful message without knowing what that \
         agent does and what it needs.\n\
         \n\
         **Three shared agents — route questions to the right one; don't substitute \
         goal-file reads for agent consultation.**\n\
         - `@tend` — intent and *should*: what the user wants, what a goal means, whether \
         a behavior is intentional. Tend holds the goal tree and conversation history.\n\
         - `@rummage` — code reality and *is*: what the code actually does, how a flow \
         works, whether an implementation matches a spec. Questions about system behavior \
         go here — reading a goal file cannot substitute.\n\
         - `@jog` — discrepancy finding: spots gaps between two sources (spec vs. code, \
         goal vs. behavior). Use when you need to know whether two layers agree.\n\
         \n\
         Reading a goal file tells you what an agent is *responsible for* — enough to write \
         a useful message to it. It does not answer questions the agent is better positioned \
         to answer.\n\
         \n\
         ## Your goal\n\
         \n\
         Goal ID: {id}\n\
         Goal:\n\
         {description}\n\
         \n\
         This goal is ongoing — there is no definition of done. You will be \
         resumed periodically when it makes sense to make further progress on it.\n\
         \n\
         Take action only when there is something concrete to do right now. If \
         the current codebase doesn't call for any work on this goal at this \
         moment, briefly say so and stop. Never create files speculatively.\n\
         \n\
         ## Rules\n\
         \n\
         - {vcs_rules}\n\
         - {tinker_dir_write_rules}\n\
         - {ownership_mandate}\n\
         {neighbors_section}\
         When you have made meaningful progress (or decided no action is \
         warranted), stop.",
        id = goal.id,
        compact_index = compact_index,
        description = goal.description,
        neighbors_section = neighbors_section,
        vcs_rules = VCS_RULES,
        tinker_dir_write_rules = TINKER_DIR_WRITE_RULES,
        ownership_mandate = IMPLEMENTATION_OWNERSHIP_MANDATE,
    );
    if let Some(r) = reason {
        prompt.push_str(&format!("\n\n## Reason for triggering\n{}", r));
    }
    prompt
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
    oc.run(message, session_id, work_dir, on_sid, on_chunk).await?;
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

    // Emit the trigger reason as the very first log line, marked for bold
    // rendering by the TUI.
    if let Some(r) = &reason {
        let _ = tx.try_send(SessionEvent::Chunk {
            goal_id: goal_id.clone(),
            text: format!("{}{}\n", TRIGGER_REASON_MARKER, r),
        });
    }

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
    let session_id = oc
        .run(&message, None, &work_dir, on_sid, on_chunk)
        .await?;

    let _ = tx.send(SessionEvent::Done { goal_id: goal_id.clone(), crashed: false }).await;

    // Intra-dispatch continuity: the structured-summary request continues
    // the same LLM conversation as the main work, so the summary can refer
    // to what just happened. This is not cross-trigger resumption.
    let summary = run_silent(oc.as_ref(), SUMMARY_REQUEST, Some(&session_id), &work_dir)
        .await
        .unwrap_or_default();

    let output = full_output.lock().unwrap().clone();
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

    // spec: goal-sessions decision — "Goal sessions must not write to
    // `.tinker/goals/`, `.tinker/notes/`, or `.tinker/state/`." The init
    // prompt must carry the directory write restriction into the agent's
    // context so it cannot silently mutate the tinker-owned directories when
    // dispatched with a narrow scope.
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

    // spec (tui): "When a goal session starts, the specific 'reason' it was
    // triggered must be rendered in the log pane in bold font." The log
    // rendering relies on the TRIGGER_REASON_MARKER prefix being the first
    // line emitted. This test verifies the marker is defined, distinct, and
    // that the log-line sentinel format strips cleanly.
    #[test]
    fn test_spec_trigger_reason_marker_is_non_printing() {
        // The marker must be a non-printing character so it can never appear
        // in normal LLM streaming output.
        assert!(
            !TRIGGER_REASON_MARKER.is_alphanumeric()
                && !TRIGGER_REASON_MARKER.is_whitespace()
                && !TRIGGER_REASON_MARKER.is_ascii_punctuation(),
            "TRIGGER_REASON_MARKER must be a non-printing control character",
        );
        // Stripping the marker must expose the reason text unchanged.
        let reason = "implement the thing";
        let marked = format!("{}{}", TRIGGER_REASON_MARKER, reason);
        let stripped = marked.strip_prefix(TRIGGER_REASON_MARKER).unwrap();
        assert_eq!(stripped, reason, "strip_prefix must recover the raw reason string");
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
                _on_session_id: Chunk,
                _on_chunk: Chunk,
            ) -> Result<String> {
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

    // spec: goal-sessions — "The session reads the reasons and pulls a
    // neighbor's full text on demand (it can read `.tinker/goals/`)." The
    // init message must explicitly tell the session how to pull neighbor text.
    #[test]
    fn test_spec_goal_init_neighbors_pullable_on_demand() {
        let mut goal = make_goal("calc", "build calc");
        goal.parent_id = "math".into();

        let msg = goal_init_message(&goal, None);

        assert!(
            msg.contains(".tinker/goals/"),
            "init message must name the path sessions use to pull neighbor full text"
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

    // spec: goal-agents preamble — before sending an @-block to another agent, the
    // agent must read that goal's TOML file so it knows the recipient's role. The
    // preamble must contain the read-before-message mandate verbatim so the LLM
    // treats "reading before messaging" as a hard requirement, not an option.
    #[test]
    fn test_spec_preamble_includes_read_before_message_mandate() {
        let goal = make_goal("widget", "build a widget");
        let msg = session_init_message(&goal, None, "[]");
        assert!(
            msg.contains("Before sending") && msg.contains("read `.tinker/goals/"),
            "preamble must contain the read-before-message mandate"
        );
        assert!(
            msg.contains("cannot write a useful message without knowing"),
            "preamble must explain why reading is required before sending"
        );
    }

    // spec: goal-agents preamble — completion reports go to the dispatcher (the
    // agent whose @-message initiated the task), not always to @tend. The preamble
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
            !msg.contains("send `@tend` a structured summary"),
            "preamble must not hardcode @tend as the mandatory report target"
        );
        assert!(
            msg.contains("test_spec_"),
            "preamble must require listing test_spec_ functions in the report"
        );
    }

    // spec: goal-agents preamble — three shared agents (@tend for intent/should,
    // @rummage for code-reality/is, @jog for discrepancy finding) must be named
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
            msg.contains("@tend") && msg.contains("intent") && msg.contains("should"),
            "preamble must describe @tend as the intent/*should* resource"
        );
        assert!(
            msg.contains("@rummage") && msg.contains("code reality") && msg.contains("is"),
            "preamble must describe @rummage as the code-reality/*is* resource"
        );
        assert!(
            msg.contains("@jog") && msg.contains("discrepancy"),
            "preamble must describe @jog as the discrepancy-finding resource"
        );
        assert!(
            msg.contains("don't substitute") || msg.contains("does not answer"),
            "preamble must explicitly warn against substituting file reads for agent queries"
        );
    }

    // spec: goal-sessions — a goal with no parent, no children, and no related
    // links produces no neighborhood table (no noise for isolated goals).
    #[test]
    fn test_spec_goal_init_no_neighborhood_table_when_isolated() {
        let goal = make_goal("standalone", "a standalone goal");
        let msg = goal_init_message(&goal, None);

        assert!(
            !msg.contains("| goal-id | reason |"),
            "isolated goal must produce no neighborhood table"
        );
        assert!(
            !msg.contains("## Neighbor goals"),
            "isolated goal must produce no neighbor section header"
        );
    }
}
