use crate::cap::{Filesystem, OpenCodeRunner};
use crate::goal::Goal;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// In-memory map of `goal_id -> session_id`. Lives for the duration of the
/// tinker process and is forgotten on restart (per `goal-sessions.toml`
/// decision: "Goal sessions only persist in memory for the duration of the
/// Tinker process"). Shared between `main()` and the goal-runner task; the
/// goal-runner reads on each dispatch and writes after each session
/// completes.
pub type SessionStore = Arc<Mutex<HashMap<String, String>>>;

/// Shared VCS-mutation rule, used by the orchestrator prompt and by both
/// goal-session init/resume messages. The orchestrator and goal sessions
/// alike may read VCS state but never mutate it — version control is the
/// user's job (per `orchestrator-agent` and `goal-sessions` goals).
pub const VCS_RULES: &str = "Treat version control as read-only. \
You may read state (`git status`, `git diff`, `git log`) to orient yourself, \
but don't mutate it — no commits, pushes, checkouts, branch operations, \
rebases, or stashing. Writing files is fine; the user handles commits.";

/// Directory-write restriction injected into every goal-session prompt.
/// Goal sessions must not write to the tinker-owned directories listed here;
/// those are the orchestrator's exclusive domain (per `goal-sessions` decision:
/// "Goal sessions must not write to `.tinker/goals/`, `.tinker/notes/`, or
/// `.tinker/state/`"). Reading them is fine; writing is not.
pub const TINKER_DIR_WRITE_RULES: &str = "Do not write to `.tinker/goals/`, \
`.tinker/notes/`, or `.tinker/state/`. Those directories are owned by the \
orchestrator. You may read them (e.g. to understand sibling goals or state), \
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

const SUMMARY_REQUEST: &str = "\
Provide a structured summary of this session with four parts:

1. What was accomplished. 2-3 sentences. Be specific about which files were created or modified. You MUST also list every `test_spec_` function you created or modified, prefixed by file path (e.g. `src/lexer.rs: test_spec_parses_arithmetic`). If you wrote zero `test_spec_` functions, explain why — and consider whether the `Tests as guardrails` standard (apply where relevant) actually permits that gap.

2. Software design changes. Note modules created or removed, interfaces (the public surfaces other modules depend on — functions, methods, types) added/removed/changed, and data types created or changed. If none, write \"none\".

3. Decisions made beyond the goal description. List any design choices, defaults, library picks, structural decisions, or scope interpretations you made on your own that were NOT specified in the goal. If none, write \"none\".

4. How to try it. One concrete invocation line — the exact command(s) the user can run to see the artifact in action.";

#[derive(Debug)]
pub enum GoalEvent {
    Text { goal_id: String, text: String },
    RunDone,
    SummaryDone { goal_id: String, summary: String },
    /// Cleanup of tinker-test-case markers failed before this goal session
    /// could start. `dirty_files` lists files still containing markers;
    /// `error` is set when cleanup itself errored before it could finish.
    CleanupBlocked {
        goal_id: String,
        dirty_files: Vec<std::path::PathBuf>,
        error: Option<String>,
    },
}

fn goal_init_message(goal: &Goal, reason: Option<&str>, sibling_goals: &[Goal]) -> String {
    let siblings_section = if sibling_goals.is_empty() {
        String::new()
    } else {
        let body = sibling_goals
            .iter()
            .filter(|g| g.id != goal.id)
            .map(|g| format!("### Goal `{}`\n\n{}\n", g.id, g.description))
            .collect::<Vec<_>>()
            .join("\n");
        if body.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n## Other goals in this project — apply where relevant\n\n{body}\n"
            )
        }
    };
    let mut prompt = format!(
        r#"You are a goal session for `tinker`, an autonomous coding assistant.

## Your goal

Goal ID: {id}
Goal:
{description}

This goal is ongoing — there is no definition of done. You will be resumed periodically when it makes sense to make further progress on it.

Take action only when there is something concrete to do right now. If the current codebase doesn't call for any work on this goal at this moment, briefly say so and stop. Never create files speculatively.

## Rules

- {vcs_rules}
- {tinker_dir_write_rules}
- {ownership_mandate}
{siblings_section}
## Subgoals

You may create subgoals by writing TOML files to `.tinker/goals/<subgoal-id>.toml`:
```toml
id = "<subgoal-id>"
description = "What this subgoal accomplishes"
parent_id = "{id}"
children = []
```

When you have made meaningful progress (or decided no action is warranted), stop."#,
        id = goal.id,
        description = goal.description,
        siblings_section = siblings_section,
        vcs_rules = VCS_RULES,
        tinker_dir_write_rules = TINKER_DIR_WRITE_RULES,
        ownership_mandate = IMPLEMENTATION_OWNERSHIP_MANDATE,
    );
    if let Some(r) = reason {
        prompt.push_str(&format!("\n\n## Reason for triggering\n{}", r));
    }
    prompt
}

fn goal_resume_message(goal: &Goal, reason: Option<&str>) -> String {
    let mut msg = format!(
        "Continue working on: {}\n\nYour session history has full context.",
        goal.description
    );
    if let Some(r) = reason {
        msg.push_str(&format!("\n\n## Reason for triggering\n{}", r));
    }
    msg.push_str(&format!(
        "\n\nPick up where you left off, or stop if nothing currently needs doing.\n\n## Rules\n- {}\n- {}\n- {}",
        VCS_RULES,
        TINKER_DIR_WRITE_RULES,
        IMPLEMENTATION_OWNERSHIP_MANDATE,
    ));
    msg
}

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
pub async fn run_goal(
    goal: Goal,
    reason: Option<String>,
    _tinker_dir: PathBuf,
    work_dir: PathBuf,
    tx: mpsc::Sender<GoalEvent>,
    oc: Arc<dyn OpenCodeRunner>,
    fs: Arc<dyn Filesystem>,
    sessions: SessionStore,
) -> Result<(String, String)> {
    let goal_id = goal.id.clone();
    // Sessions are ephemeral — per `goal-sessions.toml`, they live in memory
    // for the lifetime of the tinker process and are forgotten on restart.
    let existing_sid = {
        let map = sessions.lock().unwrap();
        map.get(&goal.id).filter(|s| !s.is_empty()).cloned()
    };
    // On first run, give the session the FULL CONTENT of every sibling goal —
    // including coding-standards and other standing concerns — so it doesn't
    // have to rely on the orchestrator copying snippets into the description.
    let message = if existing_sid.is_none() {
        let dirs = crate::goal::discover_tinker_dirs(fs.as_ref(), &work_dir);
        let siblings: Vec<Goal> = crate::goal::load_all_goals(fs.as_ref(), &dirs)
            .map(|l| l.goals)
            .unwrap_or_default();
        goal_init_message(&goal, reason.as_deref(), &siblings)
    } else {
        goal_resume_message(&goal, reason.as_deref())
    };

    // Emit the trigger reason as the very first log line, marked for bold
    // rendering by the TUI (tui-goal decision: "the specific 'reason' it was
    // triggered must be rendered in the log pane in bold font").
    if let Some(r) = &reason {
        let _ = tx.try_send(GoalEvent::Text {
            goal_id: goal_id.clone(),
            text: format!("{}{}\n", TRIGGER_REASON_MARKER, r),
        });
    }

    // Accumulate full output for the logger's goal_session_finished observable.
    let full_output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let full_output_clone = full_output.clone();
    let tx_txt = tx.clone();
    let gid2 = goal_id.clone();
    let on_sid: Box<dyn FnMut(String) + Send> = Box::new(|_| {});
    let on_chunk: Box<dyn FnMut(String) + Send> = Box::new(move |chunk: String| {
        full_output_clone.lock().unwrap().push_str(&chunk);
        let _ = tx_txt.try_send(GoalEvent::Text {
            goal_id: gid2.clone(),
            text: chunk,
        });
    });
    let session_id = oc
        .run(&message, existing_sid.as_deref(), &work_dir, on_sid, on_chunk)
        .await?;

    // Record the new session ID in the in-memory map so a subsequent run of
    // the same goal in this tinker process can resume it. Nothing hits disk;
    // on restart the map is empty.
    if !session_id.is_empty() {
        sessions
            .lock()
            .unwrap()
            .insert(goal.id.clone(), session_id.clone());
    }

    let effective_sid = if session_id.is_empty() {
        existing_sid.unwrap_or_default()
    } else {
        session_id
    };

    let _ = tx.send(GoalEvent::RunDone).await;

    let summary = run_silent(oc.as_ref(), SUMMARY_REQUEST, Some(&effective_sid), &work_dir)
        .await
        .unwrap_or_default();

    let _ = tx
        .send(GoalEvent::SummaryDone {
            goal_id,
            summary: summary.clone(),
        })
        .await;

    let output = full_output.lock().unwrap().clone();
    Ok((output, summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_goal(id: &str, description: &str) -> Goal {
        Goal {
            id: id.into(),
            description: description.into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            source_path: None,
        }
    }

    // spec: goal-sessions decision — "Every goal session dispatch must be
    // accompanied by a reason string … This reason must be passed explicitly
    // into the agent's prompt/context." Both fresh-start and resume messages
    // must surface the reason verbatim when one is given.
    #[test]
    fn test_spec_goal_init_message_includes_reason() {
        let calc = make_goal("calc", "build calc");
        let reason = "user edited the description to mention rounding";
        let init = goal_init_message(&calc, Some(reason), &[]);
        assert!(
            init.contains(reason),
            "init message must surface the trigger reason"
        );
        let resume = goal_resume_message(&calc, Some(reason));
        assert!(
            resume.contains(reason),
            "resume message must surface the trigger reason"
        );
    }

    // spec: goal-sessions decision — "Goal sessions may read version control
    // state … but are strictly prohibited from mutating it (no commit, push,
    // checkout)." Both init and resume prompts must carry the VCS_RULES
    // policy text into the agent's context.
    #[test]
    fn test_spec_goal_messages_carry_vcs_read_only_rule() {
        let calc = make_goal("calc", "build calc");
        let init = goal_init_message(&calc, None, &[]);
        let resume = goal_resume_message(&calc, None);
        assert!(
            init.contains(VCS_RULES),
            "init message must carry VCS read-only rule verbatim"
        );
        assert!(
            resume.contains(VCS_RULES),
            "resume message must carry VCS read-only rule verbatim"
        );
        // Sanity: the rule itself must actually forbid mutation.
        assert!(VCS_RULES.contains("read"));
        assert!(VCS_RULES.to_lowercase().contains("don't mutate")
            || VCS_RULES.to_lowercase().contains("do not mutate")
            || VCS_RULES.to_lowercase().contains("read-only"));
    }

    // spec: goal-sessions decision — "Goal sessions must not write to
    // `.tinker/goals/`, `.tinker/notes/`, or `.tinker/state/`." Both init and
    // resume prompts must carry the directory write restriction into the
    // agent's context so it cannot silently mutate the orchestrator-owned
    // directories when dispatched with a narrow scope.
    #[test]
    fn test_spec_goal_messages_carry_tinker_dir_write_restriction() {
        let calc = make_goal("calc", "build calc");
        let init = goal_init_message(&calc, None, &[]);
        let resume = goal_resume_message(&calc, None);
        for (label, msg) in [("init", &init), ("resume", &resume)] {
            assert!(
                msg.contains(TINKER_DIR_WRITE_RULES),
                "{label} message must carry tinker-dir write restriction verbatim"
            );
            // The restriction must name the three protected directories.
            assert!(
                msg.contains(".tinker/goals/"),
                "{label} message must mention .tinker/goals/"
            );
            assert!(
                msg.contains(".tinker/notes/"),
                "{label} message must mention .tinker/notes/"
            );
            assert!(
                msg.contains(".tinker/state/"),
                "{label} message must mention .tinker/state/"
            );
        }
    }

    // spec: implementation-ownership decision — "Goal sessions own the
    // implementation and are expected to demolish and restructure existing
    // code when a better architecture satisfies the goal." Both init and
    // resume prompts must carry the ownership mandate into the agent's
    // context so the structural taboo is lifted.
    #[test]
    fn test_spec_goal_messages_carry_ownership_mandate() {
        let calc = make_goal("calc", "build calc");
        let init = goal_init_message(&calc, None, &[]);
        let resume = goal_resume_message(&calc, None);
        for (label, msg) in [("init", &init), ("resume", &resume)] {
            assert!(
                msg.contains(IMPLEMENTATION_OWNERSHIP_MANDATE),
                "{label} message must carry ownership mandate verbatim"
            );
            assert!(
                msg.to_lowercase().contains("you own the"),
                "{label} message must convey ownership framing"
            );
        }
        // The mandate must actually assert ownership, not just mention it.
        assert!(
            IMPLEMENTATION_OWNERSHIP_MANDATE
                .to_lowercase()
                .contains("own"),
            "IMPLEMENTATION_OWNERSHIP_MANDATE must use 'own' language"
        );
    }

    // spec: goal-sessions decision — "After each goal session finishes, the
    // orchestrator produces a structured summary covering what was done,
    // design decisions made beyond the goal, and how to try the result."
    // The session-end prompt must request all four structured parts.
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

    // spec: design notes — "each goal session's init message includes the
    // full content of every other (sibling) goal." This lets a goal session
    // apply shared concerns (standards, security) without relying on the
    // orchestrator copying snippets into the description.
    #[test]
    fn test_spec_goal_init_includes_sibling_goal_content() {
        let calc = make_goal("calc", "build calc");
        let standards = make_goal("coding-standards", "MUST use DI everywhere.");
        let msg = goal_init_message(&calc, None, &[standards.clone(), calc.clone()]);
        assert!(msg.contains("build calc"));
        assert!(msg.contains("coding-standards"));
        assert!(msg.contains("MUST use DI everywhere."));
        // own goal must not appear in the siblings section (filtered out)
        let after_siblings = msg.split("Other goals in this project").nth(1);
        if let Some(rest) = after_siblings {
            // Make sure `### Goal `calc`` doesn't appear in the siblings block
            assert!(
                !rest.contains("### Goal `calc`"),
                "current goal must not be listed among its own siblings"
            );
        }
    }
}
