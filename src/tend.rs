use crate::cap::OpenCodeRunner;
use anyhow::Result;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum TendEvent {
    SessionId(String),
    Text(String),
    Done,
}

/// Send a message to tend and stream events back via `tx`.
/// Returns the full accumulated reply text (all chunks concatenated), or an
/// empty string on error, so callers can log the reply without a separate buffer.
pub async fn send_message(
    oc: Arc<dyn OpenCodeRunner>,
    message: &str,
    session_id: Option<&str>,
    work_dir: &Path,
    tx: mpsc::Sender<TendEvent>,
) -> Result<String> {
    let tx_sid = tx.clone();
    let tx_txt = tx.clone();
    let full_reply: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let full_reply_clone = full_reply.clone();
    let on_sid: Box<dyn FnMut(String) + Send> = Box::new(move |sid: String| {
        let _ = tx_sid.try_send(TendEvent::SessionId(sid));
    });
    let on_chunk: Box<dyn FnMut(String) + Send> = Box::new(move |chunk: String| {
        full_reply_clone.lock().unwrap().push_str(&chunk);
        let _ = tx_txt.try_send(TendEvent::Text(chunk));
    });
    let res = oc.run(message, session_id, work_dir, on_sid, on_chunk).await;
    if let Err(e) = &res {
        let _ = tx.try_send(TendEvent::Text(format!("\n[Error: {}]\n", e)));
    }
    let _ = tx.send(TendEvent::Done).await;
    let reply = full_reply.lock().unwrap().clone();
    res.map(|_| reply)
}

const TEND_FRONTMATTER: &str = "---\ndescription: >-\n  Tend — manages goals, interviews the user, and watches for\n  reframes when the current goal stops being the right question. Never writes\n  production code directly.\nmode: primary\npermission:\n  webfetch: deny\n  task: deny\n  todowrite: deny\n  websearch: allow\n  lsp: deny\n  skill: deny\n---\n";

fn description_from_toml_str(s: &str) -> String {
    let marker = "description = \"\"\"\n";
    let start = s.find(marker).expect("TOML must have description field") + marker.len();
    let end = start + s[start..].find("\n\"\"\"").expect("description field must close");
    s[start..end].to_string()
}

/// Returns the content for the `tend` opencode agent file (compact-index mode).
/// Reads the description from tend.toml and wraps it with opencode agent frontmatter.
pub fn tend_agent_content() -> String {
    const TOML: &str = include_str!("../.tinker/goals/tend.toml");
    format!("{}{}", TEND_FRONTMATTER, description_from_toml_str(TOML))
}

/// Same as `tend_agent_content` but suppresses the compact-index section.
/// When --tend-full-goal-context is set, full goal text is injected instead.
/// The summary write protocol is kept regardless.
pub fn tend_agent_content_full_context() -> String {
    let full = tend_agent_content();
    suppress_compact_index_section_for_tend(&full)
}

fn suppress_compact_index_section_for_tend(content: &str) -> String {
    // Remove "## Goal index" through the Pull strategy paragraph.
    // "Write protocol for `summary`" stays — it's active regardless of context mode.
    let section_start = "## Goal index\n";
    let keep_from = "**Write protocol for `summary`.**";
    match (content.find(section_start), content.find(keep_from)) {
        (Some(s), Some(e)) if e > s => format!("{}{}", &content[..s], &content[e..]),
        _ => content.to_string(),
    }
}

/// Build the dynamic stdin prompt for tend — the compact goal index.
/// The static system prompt lives in the agent file (`tend.md`).
pub fn tend_init_prompt(goals_summary: &str) -> String {
    format!(
        r#"## Current goals (compact index — pull full text on demand)
{goals_summary}"#
    )
}

/// Build the dynamic stdin prompt for tend in full-context mode.
/// Used with --tend-full-goal-context: full goal text is already included,
/// so the label reflects that rather than referencing the compact index.
pub fn tend_init_prompt_full_context(goals_summary: &str) -> String {
    format!(
        r#"## Current goals (full text)
{goals_summary}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec (shared-language): a persistent vocabulary file at the exact path
    // `.tinker/state/vocabulary.txt` tracks known technical terms. The prompt
    // must name this exact path so tinker manages the right file.
    #[test]
    fn test_spec_tinker_prompt_names_vocabulary_state_file() {
        let content = tend_agent_content();
        assert!(
            content.contains(".tinker/state/vocabulary.txt"),
            "prompt must reference the persistent vocabulary file at .tinker/state/vocabulary.txt",
        );
    }

    // spec (tinker-agent): the agent file embeds opencode frontmatter
    // declaring it a primary-mode agent and denying autonomous multi-step
    // tools (task, todowrite). This is what mechanically isolates the
    // tinker from opencode's default software-engineer persona.
    #[test]
    fn test_spec_tinker_agent_file_frontmatter_denies_task_and_todowrite() {
        let content = tend_agent_content();
        assert!(
            content.starts_with("---\n"),
            "agent file must begin with YAML frontmatter delimiter",
        );
        assert!(
            content.contains("mode: primary"),
            "agent must be declared mode: primary",
        );
        assert!(
            content.contains("task: deny"),
            "agent must deny the `task` tool (autonomous multi-step)",
        );
        assert!(
            content.contains("todowrite: deny"),
            "agent must deny the `todowrite` tool (autonomous multi-step)",
        );
    }

    // spec (tinker-agent): tinker retains allow permission for
    // write/edit/bash globally — we rely on the prompt (the tinker-test-case
    // marker requirement) to constrain bad writes, not on tool-level denial.
    // The frontmatter must therefore NOT deny these tools.
    #[test]
    fn test_spec_tinker_agent_does_not_deny_write_edit_bash() {
        let content = tend_agent_content();
        let after = content
            .strip_prefix("---\n")
            .expect("agent file starts with frontmatter");
        let end = after.find("\n---").expect("frontmatter has closing delimiter");
        let frontmatter = &after[..end];
        // Match per-line so `write: deny` doesn't accidentally hit
        // `todowrite: deny`. Permission lines render as `  <tool>: <decision>`
        // (two-space indent under the `permission:` key).
        for tool in ["write", "edit", "bash"] {
            let denied_line = format!("  {}: deny", tool);
            for line in frontmatter.lines() {
                assert_ne!(
                    line.trim_end(),
                    denied_line.as_str(),
                    "frontmatter must not deny `{}` — tinker relies on prompt-level marker rule, not permission denial",
                    tool,
                );
            }
        }
    }

    // spec (tinker-agent): tinker is granted websearch so it can surface
    // external context (library naming, design pattern lookups, terms the user
    // mentions) during interviews without the user having to context-switch to
    // a browser. webfetch stays denied. The frontmatter must allow websearch
    // and deny webfetch.
    #[test]
    fn test_spec_tinker_agent_allows_websearch_denies_webfetch() {
        let content = tend_agent_content();
        let after = content
            .strip_prefix("---\n")
            .expect("agent file starts with frontmatter");
        let end = after.find("\n---").expect("frontmatter has closing delimiter");
        let frontmatter = &after[..end];
        assert!(
            !frontmatter.contains("websearch: deny"),
            "frontmatter must not deny websearch — tinker uses it to surface external context during interviews",
        );
        assert!(
            frontmatter.contains("webfetch: deny"),
            "frontmatter must deny webfetch — websearch precedes it; revisit only if a concrete URL-first use surfaces",
        );
    }

    // spec (tinker-agent): static rules (persona, procedures) live in the
    // agent file as the system prompt; dynamic state (current goals) is passed
    // separately via stdin through `tinker_init_prompt`. The agent file
    // must contain the persona but must NOT carry the current goals list; the
    // init prompt must carry the goals.
    #[test]
    fn test_spec_tinker_static_persona_in_agent_dynamic_goals_in_init() {
        let content = tend_agent_content();
        assert!(
            content.starts_with("---\n"),
            "agent file must begin with YAML frontmatter (static persona content)",
        );
        let init = tend_init_prompt("- demo-goal-id: a demo description");
        assert!(
            init.contains("Current goals"),
            "init prompt must label the dynamic goals section",
        );
        assert!(
            init.contains("demo-goal-id"),
            "init prompt must carry the dynamic goals summary verbatim",
        );
        assert!(
            !content.contains("demo-goal-id"),
            "agent file must not embed dynamic goal ids",
        );
    }

    // spec (tinker-agent): tinker is explicitly forbidden from
    // mutating VCS state in its system prompt (it may read git status/diff/log
    // but never commit, push, checkout, rebase, etc.). This is enforced via
    // the shared `VCS_RULES` constant substituted into the agent prompt.
    #[test]
    fn test_spec_tinker_prompt_forbids_vcs_mutation() {
        let content = tend_agent_content();
        assert!(
            content.contains("Read-only") || content.contains("read-only"),
            "VCS directive must declare version control read-only",
        );
        assert!(
            content.contains("Never commit") || content.contains("never commit") || content.contains("no commits"),
            "VCS directive must forbid commits",
        );
    }

    // spec (tinker/rummage-arm): tinker never reads source for comprehension and
    // never writes probe code — all code-reality grounding is delegated to rummage.
    // The prompt must state the rule and the explicit failure mode it prevents.
    #[test]
    fn test_spec_tinker_proves_by_execution_not_reading_source() {
        let content = tend_agent_content();
        let normalized = content.replace('\n', " ");
        assert!(
            normalized.contains("Never read source for comprehension")
                || normalized.contains("never read source for comprehension")
                || normalized.contains("reading source for comprehension"),
            "prompt must direct tinker never to read source for comprehension",
        );
    }

    // spec (user-persona): when relaying rummage findings, Tinker must act as an
    // active design partner — pairing each finding with a question or a proposed
    // alternative, not just stenographic "rummage found this, here's what it means".
    #[test]
    fn test_spec_tinker_prompt_directs_active_design_partner_reporting() {
        let content = tend_agent_content();
        assert!(
            content.contains("design partner"),
            "prompt must frame tinker as a design partner when reporting rummage findings",
        );
        assert!(
            content.contains("pair each finding") || content.contains("Pair each finding"),
            "prompt must require pairing each rummage finding with a question or proposed alternative",
        );
    }

    // spec (shared-language): the form norm — replies default to the minimum
    // form the moment calls for. The prompt must name this constraint so tinker
    // does not default to tables and bullet surveys when a sentence would serve.
    #[test]
    fn test_spec_shared_language_form_norm_minimum_viable_shape() {
        let content = tend_agent_content();
        assert!(
            content.contains("minimum form") || content.contains("minimum viable"),
            "prompt must name the form norm: replies default to the minimum form",
        );
    }

    // spec (shared-language): tables, long bullet lists, and multi-paragraph
    // surveys are gated on an explicit user request. The prompt must state the
    // gate so tinker does not produce them by default.
    #[test]
    fn test_spec_shared_language_form_norm_no_unrequested_tables_or_lists() {
        let content = tend_agent_content();
        assert!(
            content.contains("explicitly requests") || content.contains("user asks") || content.contains("explicitly asks"),
            "prompt must state tables/lists are appropriate only when the user explicitly requests them",
        );
        assert!(
            content.contains("Tables") || content.contains("bullet lists") || content.contains("long lists"),
            "prompt must name tables or bullet lists as the forms gated on user request",
        );
    }

    // spec (shared-language): formulaic template replies violate the form norm
    // regardless of length. The prompt must name this explicitly.
    #[test]
    fn test_spec_shared_language_form_norm_no_formulaic_replies() {
        let content = tend_agent_content();
        assert!(
            content.contains("Formulaic") || content.contains("formulaic"),
            "prompt must name formulaic template replies as a form-norm violation",
        );
    }


    // spec (creative-process): tinker has a dual duty — faithfully
    // execute established conventions AND surface inflection points where
    // convention is insufficient. It must NOT fabricate judgment at inflection
    // points; the user's situated intuition is the only valid source.
    #[test]
    fn test_spec_tinker_encodes_dual_duty_no_fabrication_at_inflection_points() {
        let content = tend_agent_content();
        assert!(
            content.contains("Dual duty"),
            "prompt must describe tinker's dual duty",
        );
        assert!(
            content.contains("inflection point") || content.contains("inflection points"),
            "prompt must name inflection points as the trigger to stop and surface",
        );
        assert!(
            content.contains("Do NOT fabricate") || content.contains("never fabricate"),
            "prompt must explicitly forbid fabricating judgment at inflection points",
        );
    }

    // spec (creative-process): tone is downstream of role, not a separate
    // principle. The prompt must state this and must name the deference layer
    // tinker should drop.
    #[test]
    fn test_spec_tinker_tone_downstream_of_role_drop_deference_layer() {
        let content = tend_agent_content();
        assert!(
            content.contains("Tone follows from role"),
            "prompt must frame tone as downstream of role, not an independent principle",
        );
        assert!(
            content.contains("deference layer"),
            "prompt must name the deference layer as what tinker should drop",
        );
    }

    // spec (user-system-interaction): the user holds the *should* (intent,
    // decisions, vetted goals) and tinker holds the *is* (current state of all
    // artifacts). The prompt must encode this boundary explicitly.
    #[test]
    fn test_spec_user_holds_should_tinker_holds_is() {
        let content = tend_agent_content();
        assert!(
            content.contains("user holds the *should*"),
            "prompt must state that the user holds the should",
        );
        assert!(
            content.contains("Tend holds the *is*"),
            "prompt must state that tend holds the is",
        );
    }

    // spec (user-system-interaction): when reasoning depends on the *is*,
    // tinker must verify by observation — never by asking the user to
    // remember, and never by inference substituting for checking.
    #[test]
    fn test_spec_is_state_verified_by_observation_not_inference() {
        let content = tend_agent_content();
        assert!(
            content.contains("verify by observation"),
            "prompt must instruct tinker to verify is-state by observation",
        );
    }

    // spec (tinker-notes): tinker silently maintains a single
    // append-only notes file at `.tinker/notes/notes.md`. The prompt must name
    // this exact path so tinker writes to the right place.
    #[test]
    fn test_spec_tinker_notes_names_correct_file_path() {
        let content = tend_agent_content();
        assert!(
            content.contains(".tinker/notes/notes.md"),
            "prompt must reference the notes file at .tinker/notes/notes.md",
        );
    }

    // spec (tinker-notes): all six trigger types must be present in the
    // prompt — friction, surprise, reframe, recurring thread, self-introduced
    // framing slip, and explicit "remember this".
    #[test]
    fn test_spec_tinker_notes_all_triggers_named() {
        let content = tend_agent_content();
        let lower = content.to_lowercase();
        assert!(lower.contains("friction"), "prompt must name friction trigger");
        assert!(lower.contains("surprise"), "prompt must name surprise trigger");
        assert!(lower.contains("reframe"), "prompt must name reframe trigger");
        assert!(lower.contains("recurring thread"), "prompt must name recurring thread trigger");
        assert!(
            content.contains("framing slip"),
            "prompt must name framing slip trigger",
        );
        assert!(
            content.contains("remember this") || content.contains("remember a situation"),
            "prompt must name explicit 'remember this' trigger",
        );
    }

    // spec (tinker-notes): the `.tinker/notes/` directory must be
    // created at startup. Tested by asserting that main.rs contains the
    // mkdir call for the notes subdirectory.
    #[test]
    fn test_spec_notes_dir_created_at_startup() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains(".tinker/notes") || main_rs.contains("\"notes\""),
            "main.rs must create the .tinker/notes directory at startup",
        );
    }

    // spec (shared-language): the `.tinker/state/` directory — which holds
    // the vocabulary file — must be created at startup so tinker
    // can write to it on the very first run. Tested by asserting that main.rs
    // contains the mkdir call for the state subdirectory.
    #[test]
    fn test_spec_state_dir_created_at_startup() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains(".tinker/state") || main_rs.contains("\"state\""),
            "main.rs must create the .tinker/state directory at startup",
        );
    }

    // spec (tinker-agent): the agent file is always overwritten on startup
    // (not only when missing) so the installed copy stays in sync with
    // tend_agent_content() as the persona evolves. Guarding the write
    // behind an existence check leaves stale agent files in place across runs.
    #[test]
    fn test_spec_agent_file_always_overwritten_not_guarded_by_exists_check() {
        let main_rs = include_str!("main.rs");
        // The guard `if !agent_path.exists()` must not appear — the write is
        // unconditional so the installed file tracks the source of truth.
        assert!(
            !main_rs.contains("if !agent_path.exists()"),
            "main.rs must not guard the agent-file write behind an existence check; \
             the file must always be overwritten to stay in sync with tend_agent_content()",
        );
    }

    // spec (tinker): "Any new goal or substantive edit is checked
    // against the existing goal set; tinker surfaces every
    // relationship it finds and waits for the user to resolve each one before
    // writing. No exemptions for 'small' or 'minor' edits."
    #[test]
    fn test_spec_cross_goal_alignment_no_exemptions_for_edits() {
        let content = tend_agent_content();
        // The "no exemptions" stance for edits must be encoded.
        assert!(
            content.contains("no exemptions for \"small\" or \"minor\" edits")
                || content.contains("no exemptions for \"small\" edits"),
            "prompt must state edits get no exemptions from cross-goal alignment",
        );
    }

    // spec (tend): When editing any agent goal (tend, rummage, or jog),
    // agent-complementarity is always pulled in cross-goal alignment —
    // profile assignments are a structural concern that must hold across
    // any profile change.
    #[test]
    fn test_spec_agent_goal_edit_pulls_agent_complementarity() {
        let content = tend_agent_content();
        assert!(
            content.contains("agent-complementarity"),
            "prompt must name `agent-complementarity` as the goal to pull during cross-goal alignment on agent goal edits",
        );
        assert!(
            content.contains("agent") && content.contains("complementarity"),
            "prompt must associate agent edits with pulling agent-complementarity",
        );
    }

    // spec (compact-goal-context): main.rs must delegate goal index building to
    // build_compact_index, which produces a nested JSON structure representing
    // the parent/child hierarchy (children nested) and related links.
    #[test]
    fn test_spec_main_feeds_parent_id_and_children_into_goals_summary() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("build_compact_index"),
            "main.rs must call build_compact_index to build the goal index",
        );
        // build_compact_index uses build_tree (which uses parent_id) and
        // serializes children nested and related as arrays — verified in goal.rs.
        assert!(
            main_rs.contains("goal::build_compact_index"),
            "main.rs must call goal::build_compact_index with the goals list",
        );
    }

    // spec (compact-goal-context): the compact index prompt section must
    // document the `parent_id` field so tend knows it can navigate upward
    // without loading the parent's full file.
    #[test]
    fn test_spec_tinker_prompt_compact_index_describes_parent_id() {
        let content = tend_agent_content();
        assert!(
            content.contains("`parent_id`"),
            "compact index description must document the parent_id field",
        );
    }

    // spec (compact-goal-context): full-context init prompt labels the block
    // as full text, not as a compact index.
    #[test]
    fn test_spec_tinker_init_prompt_full_context_label() {
        let prompt = tend_init_prompt_full_context("### root\ndescription here");
        assert!(
            prompt.contains("full text"),
            "full-context init prompt must label goals as full text",
        );
        assert!(
            !prompt.contains("compact"),
            "full-context init prompt must not reference the compact index",
        );
    }

    // spec (tend): before emitting any /run line, tend must update the relevant
    // goal spec first. The session reads the updated spec and derives its own
    // work from it — the trigger without the spec update gives the session
    // nothing authoritative to act on.
    #[test]
    fn test_spec_run_discipline_spec_first_required() {
        let content = tend_agent_content();
        assert!(
            content.contains("Update the relevant goal spec before")
                || content.contains("update the relevant goal spec first")
                || content.contains("Spec-first discipline"),
            "prompt must instruct updating the spec before dispatching",
        );
    }

    // spec (tend): the reason in every tend-emitted /run must be a declarative
    // pointer to a spec delta, not an imperative describing what to build.
    // The prompt must state this rule and show the contrast explicitly.
    #[test]
    fn test_spec_run_discipline_declarative_reason() {
        let content = tend_agent_content();
        assert!(
            content.contains("declarative pointer to a spec delta"),
            "prompt must describe the reason as a declarative spec-delta pointer",
        );
        assert!(
            content.contains("Never an imperative") || content.contains("never an imperative"),
            "prompt must explicitly prohibit imperative dispatch context",
        );
    }

    // spec (goal-structure-standard): the write protocol must instruct the
    // tinker to re-check the parent goal's summary whenever a child is
    // created or edited, and update the parent in the same write turn if needed.
    #[test]
    fn test_spec_tinker_prompt_parent_summary_recheck_when_child_edited() {
        let content = tend_agent_content();
        assert!(
            content.contains("Re-check parent summary"),
            "prompt must include a 'Re-check parent summary' write-protocol step",
        );
        // "same write turn" may span a line break with leading whitespace; normalize.
        let normalized: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains("same write turn") || normalized.contains("same turn"),
            "prompt must say the update happens in the same write turn",
        );
    }

    // spec (goal-structure-standard): the write protocol must enforce related-link
    // symmetry — for every entry in the edited file's `related` list, the linked
    // partner goal must also list back. Partner fix happens in the same write turn.
    #[test]
    fn test_spec_tinker_prompt_related_links_symmetric_both_list_each_other() {
        let content = tend_agent_content();
        assert!(
            content.contains("Re-validate related-link symmetry"),
            "prompt must include a 'Re-validate related-link symmetry' step",
        );
        assert!(
            content.contains("back-link"),
            "prompt must describe adding missing back-links in partner goals",
        );
    }

    // spec (batch-review): tend must describe the optional post-batch compliance
    // review with @rummage — when to invoke it and how to incorporate findings.
    #[test]
    fn test_spec_tinker_prompt_describes_batch_review_with_rummage() {
        let content = tend_agent_content();
        assert!(
            content.contains("compliance review") || content.contains("Compliance review"),
            "tend prompt must describe the optional compliance review step",
        );
    }

}