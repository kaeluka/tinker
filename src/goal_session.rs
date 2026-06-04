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

/// Directory access restriction injected into every goal-session prompt.
/// Goal sessions must not read or write the tend-owned directories listed here;
/// those are tend's exclusive domain (per `goal-sessions`).
pub const TINKER_DIR_WRITE_RULES: &str = "Do not read or write `.tinker/goals/`, \
`.tinker/notes/`, or `.tinker/state/`. Those directories are owned by \
tend — do not read, create, modify, or delete any file inside them.";

/// Implementation-ownership mandate injected into every goal-session prompt.
/// Goal sessions own the source code and should not hesitate to radically
/// restructure or delete code to meet the goal. The human owns the Intent
/// (Goals); the goal session owns the Implementation (source code).
pub const IMPLEMENTATION_OWNERSHIP_MANDATE: &str = "\
You are not a guest in this codebase. You own the implementation. \
If a better architecture requires demolishing and restructuring \
what exists, do it without hesitation. The human owns the Intent \
(Goals); you own the Implementation (source code).";

/// Session-invariant message-passing and progress-guarantee sections.
/// Used verbatim in every full init message and in the framework preamble
/// system prompt delivered to claude goal agents.
pub const MESSAGE_PASSING_AND_PROGRESS_SECTIONS: &str =
    "## Message passing\n\
     \n\
     Use `@<goal-id>` tag envelopes to send a message to another agent:\n\
     \n\
     ```\n\
     <@agent-or-goal-id>\n\
     message body — may span multiple lines\n\
     </@agent-or-goal-id>\n\
     ```\n\
     \n\
     Output outside envelopes is your private working log (rendered in the log pane, \
     not delivered to other agents). Tag envelopes in your reply are extracted after \
     you finish and routed to the named recipients. No blocking calls — replies arrive \
     in the normal message stream. **Reporting completions.** When you complete \
     significant work, report to your dispatcher — the agent whose `@`-message \
     initiated your current task (this can be the user). In your report: what you did, \
     what you decided beyond the goal, how to try the result, every `test_spec_` \
     function you created or modified, and how you collaborated with other agents in \
     fulfilling the task.\n\
     \n\
     **Before sending `@goal-id`, ensure you understand the recipient's role.** The \
     compact index and edge reasons are your primary signal. If they're not sufficient, \
     ask `@tend` — tend holds the full goal tree and can describe what any agent does \
     and needs.\n\
     \n\
     **Three shared agents — route questions to the right one.**\n\
     - `@tend` — intent and *should*: what the user wants, what a goal means, whether \
     a behavior is intentional. Tend holds the goal tree and conversation history.\n\
     - `@rummage` — code reality and *is*: what the code actually does, how a flow \
     works, whether an implementation matches a spec. Questions about system behavior \
     go here.\n\
     - `@jog` — discrepancy finding: spots gaps between two sources (spec vs. code, \
     goal vs. behavior). Use when you need to know whether two layers agree.\n\
     \n\
     The compact index and edge reasons tell you what an agent is *responsible for* — \
     enough to write a useful message. They do not answer questions the agent is better \
     positioned to answer.\n\
     \n\
     ## Progress guarantee\n\
     \n\
     Always take a step — silent abort is not acceptable. When you encounter an error:\n\
     - **Tool denial**: a routing signal. Identify which agent's scope covers the \
     blocked path and route via `@`-message; do not retry the denied action through \
     other means.\n\
     - **Transient error** (rate limit, server error, network interruption): retry.\n\
     - **Any other error**: reason about it — route to a peer, ask `@tend` for \
     clarification, or report the obstacle to your dispatcher.";

/// Generic neighbor-consultation mandate preamble — the invariant text that
/// precedes the goal-specific neighbor table. Shared between the full init
/// message, the tend init prompt, and the framework preamble system prompt.
pub const NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE: &str =
    "**Before and during significant work, send an `@`-message to each \
     neighboring goal — parent, children, and related links — excluding \
     your dispatcher, who already knows what you are doing.** Announce \
     what you are doing and invite input. \
     Adjacent goals respond with context, flag conflicts, and collaborate \
     toward resolution. Conflicts that neither party can resolve must \
     surface to your dispatcher — do not absorb them silently.\n\
     \n\
     Use the reason column to write a useful opening message. For deeper \
     context about any neighbor's scope or intent, consult `@tend` — \
     tend holds the full goal tree.\n\
     \n\
     **This mandate is only as good as the edge graph.** If a goal that \
     should be adjacent is missing from this table, that is a graph \
     maintenance failure — not something to work around.";

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
    Done { goal_id: String },
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
pub fn build_neighborhood_table(goal: &Goal) -> String {
    let mut rows: Vec<(String, String)> = vec![];

    if !goal.parent_id.is_empty() {
        rows.push((
            goal.parent_id.clone(),
            "parent goal — ask @tend for broader context and framing".to_string(),
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
    let (dir_rules_line, ownership_line) = if goal.id == "tend" {
        (String::new(), String::new())
    } else {
        (
            format!("- {}\n", TINKER_DIR_WRITE_RULES),
            format!("- {}\n", IMPLEMENTATION_OWNERSHIP_MANDATE),
        )
    };

    let mut prompt = format!(
        "You are the agent for goal `{id}`.\n\
         \n\
         ## Goal index\n\
         \n\
         {compact_index}\n\
         \n\
         If the compact index isn't sufficient, consult `@tend` — tend holds the full \
         goal tree and can answer questions about any goal's scope or intent.\n\
         \n\
         {message_passing_and_progress}\n\
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
         {dir_rules_line}\
         {ownership_line}\
         {neighbors_section}\
         When you have made meaningful progress (or decided no action is \
         warranted), stop.",
        id = goal.id,
        compact_index = compact_index,
        description = goal.description,
        neighbors_section = neighbors_section,
        vcs_rules = VCS_RULES,
        dir_rules_line = dir_rules_line,
        ownership_line = ownership_line,
        message_passing_and_progress = MESSAGE_PASSING_AND_PROGRESS_SECTIONS,
    );
    if let Some(r) = reason {
        prompt.push_str(&format!("\n\n## Reason for triggering\n{}", r));
    }
    prompt
}

/// Session-invariant framework preamble for claude goal agents.
/// Delivered as the system prompt so it persists across session turns without
/// repeating in the per-dispatch init message. Mirrors the persona opencode's
/// `tinker.md` carries; file-access boundaries are conveyed via the prompt
/// rather than a harness-level permission block.
pub fn goal_agent_framework_preamble() -> String {
    format!(
        "{message_passing_and_progress}\n\
         \n\
         ## Rules\n\
         \n\
         - {vcs_rules}\n\
         - {tinker_write_rules}\n\
         - {ownership_mandate}\n\
         \n\
         ## Neighbor consultation\n\
         \n\
         {neighbor_mandate}",
        message_passing_and_progress = MESSAGE_PASSING_AND_PROGRESS_SECTIONS,
        vcs_rules = VCS_RULES,
        tinker_write_rules = TINKER_DIR_WRITE_RULES,
        ownership_mandate = IMPLEMENTATION_OWNERSHIP_MANDATE,
        neighbor_mandate = NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE,
    )
}

/// Lean init message for claude goal agents where the framework preamble
/// is already in the system prompt. Contains only goal-specific content:
/// identity, goal index, goal description, neighbor table, trigger reason.
pub fn goal_agent_lean_init_message(goal: &Goal, reason: Option<&str>, compact_index: &str) -> String {
    let table = build_neighborhood_table(goal);
    let neighbors_section = if table.is_empty() {
        String::new()
    } else {
        format!("\n## Neighbor goals\n\n{}\n", table)
    };

    let mut prompt = format!(
        "You are the agent for goal `{id}`.\n\
         \n\
         ## Goal index\n\
         \n\
         {compact_index}\n\
         \n\
         If the compact index isn't sufficient, consult `@tend` — tend holds the full \
         goal tree and can answer questions about any goal's scope or intent.\n\
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
         {neighbors_section}\
         When you have made meaningful progress (or decided no action is \
         warranted), stop.",
        id = goal.id,
        compact_index = compact_index,
        description = goal.description,
        neighbors_section = neighbors_section,
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

    let _ = tx.send(SessionEvent::Done { goal_id: goal_id.clone() }).await;

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
    // @-message, retry transient errors, and never silently abort.
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
    // mandate (it delegates code changes to goal sessions via <@rummage>).
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

    // spec (agent-collaboration): the neighbor section must be a mandatory
    // consultation requirement — imperative, not advisory. Before and during
    // significant work the agent must send @-messages to every neighbor,
    // announce what it is doing, and invite input.
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
            msg.contains("Before and during significant work"),
            "neighbor section must mandate consultation before and during work"
        );
        assert!(
            msg.contains("send an `@`-message to each"),
            "neighbor section must instruct sending @-messages to each neighbor"
        );
        assert!(
            msg.contains("parent, children, and related links"),
            "neighbor section must enumerate all three adjacency categories explicitly"
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
            msg.contains("excluding your dispatcher"),
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
    // The init message must direct agents to consult @tend when the compact
    // index and edge reasons aren't sufficient — not to fetch TOML files.
    #[test]
    fn test_spec_goal_init_escalates_to_tend_for_neighbor_context() {
        let mut goal = make_goal("calc", "build calc");
        goal.parent_id = "math".into();

        let msg = goal_init_message(&goal, None);

        assert!(
            msg.contains("@tend"),
            "init message must name @tend as the escalation path for neighbor context"
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

    // spec: goal-agents preamble — before @-messaging another agent, an agent must
    // understand the recipient's role via the compact index and edge reasons (no-peek:
    // no goal-file reads). If the index isn't sufficient, @tend is the escalation
    // path. The preamble must encode this dialog-first model.
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
            msg.contains("do not answer") || msg.contains("don't substitute"),
            "preamble must note that the index does not substitute for agent queries"
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
