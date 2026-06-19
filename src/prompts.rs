//! Prompt storage layer.
//!
//! All LLM-facing prompt strings live as reviewable files under `prompts/`.
//! Dynamic parts are templated with `{UPPER_CASE}` placeholders and filled
//! via `str::replace` in the helper functions below.
//!
//! Consumers own what goes into each prompt and when it is assembled into a
//! message; this module owns the storage format, directory convention, and
//! interpolation contract.

// ── Framework preamble constants (used by goal_session.rs) ──────────────────

pub const VCS_RULES: &str = include_str!("../prompts/goal_session/vcs_rules.md");
pub const TINKER_DIR_WRITE_RULES: &str = include_str!("../prompts/goal_session/tinker_dir_write_rules.md");
pub const IMPLEMENTATION_OWNERSHIP_MANDATE: &str = include_str!("../prompts/goal_session/implementation_ownership_mandate.md");
pub const MESSAGE_PASSING_AND_PROGRESS_SECTIONS: &str = include_str!("../prompts/goal_session/message_passing_and_progress.md");
pub const FRESH_DISPATCH_INSTRUCTIONS: &str = include_str!("../prompts/goal_session/fresh_dispatch.md");
pub const NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE: &str = include_str!("../prompts/goal_session/neighbor_consultation_mandate.md");

// ── Session init templates ──────────────────────────────────────────────────

const SESSION_INIT_MESSAGE_TEMPLATE: &str = include_str!("../prompts/goal_session/session_init_message.md");
const FRESH_SUBSESSION_INIT_MESSAGE_TEMPLATE: &str = include_str!("../prompts/goal_session/fresh_subsession_init_message.md");
const FRESH_SUBSESSION_LEAN_INIT_MESSAGE_TEMPLATE: &str = include_str!("../prompts/goal_session/fresh_subsession_lean_init_message.md");

/// Interpolates the session-init message template. `dir_rules` and `ownership`
/// are empty for the tend exemption; `neighbors_section` is empty when the
/// goal has no neighbors; `reason_section` is empty when no trigger reason.
#[allow(clippy::too_many_arguments)]
pub fn session_init_message(
    goal_id: &str,
    compact_index: &str,
    description: &str,
    vcs_rules: &str,
    dir_rules: &str,
    ownership_mandate: &str,
    neighbors_section: &str,
    message_passing_and_progress: &str,
    fresh_dispatch: &str,
    reason_section: &str,
) -> String {
    SESSION_INIT_MESSAGE_TEMPLATE
        .replace("{GOAL_ID}", goal_id)
        .replace("{COMPACT_INDEX}", compact_index)
        .replace("{DESCRIPTION}", description)
        .replace("{VCS_RULES}", vcs_rules)
        .replace("{DIR_RULES}", dir_rules)
        .replace("{OWNERSHIP_MANDATE}", ownership_mandate)
        .replace("{NEIGHBORS_SECTION}", neighbors_section)
        .replace("{MESSAGE_PASSING_AND_PROGRESS}", message_passing_and_progress)
        .replace("{FRESH_DISPATCH}", fresh_dispatch)
        .replace("{REASON_SECTION}", reason_section)
}

/// Interpolates the fresh-sub-session init message template.
#[allow(clippy::too_many_arguments)]
pub fn fresh_subsession_init_message(
    session_id: &str,
    dispatcher_id: &str,
    label_clause: &str,
    task: &str,
    compact_index: &str,
    goal_id: &str,
    description: &str,
    message_passing_and_progress: &str,
    vcs_rules: &str,
    tinker_write_rules: &str,
    ownership_mandate: &str,
    neighbors_section: &str,
) -> String {
    FRESH_SUBSESSION_INIT_MESSAGE_TEMPLATE
        .replace("{SESSION_ID}", session_id)
        .replace("{DISPATCHER_ID}", dispatcher_id)
        .replace("{LABEL_CLAUSE}", label_clause)
        .replace("{TASK}", task)
        .replace("{COMPACT_INDEX}", compact_index)
        .replace("{GOAL_ID}", goal_id)
        .replace("{DESCRIPTION}", description)
        .replace("{MESSAGE_PASSING_AND_PROGRESS}", message_passing_and_progress)
        .replace("{VCS_RULES}", vcs_rules)
        .replace("{TINKER_DIR_WRITE_RULES}", tinker_write_rules)
        .replace("{OWNERSHIP_MANDATE}", ownership_mandate)
        .replace("{NEIGHBORS_SECTION}", neighbors_section)
}

/// Interpolates the lean fresh-sub-session init message template.
#[allow(clippy::too_many_arguments)]
pub fn fresh_subsession_lean_init_message(
    session_id: &str,
    dispatcher_id: &str,
    label_clause: &str,
    task: &str,
    compact_index: &str,
    goal_id: &str,
    description: &str,
    neighbors_section: &str,
) -> String {
    FRESH_SUBSESSION_LEAN_INIT_MESSAGE_TEMPLATE
        .replace("{SESSION_ID}", session_id)
        .replace("{DISPATCHER_ID}", dispatcher_id)
        .replace("{LABEL_CLAUSE}", label_clause)
        .replace("{TASK}", task)
        .replace("{COMPACT_INDEX}", compact_index)
        .replace("{GOAL_ID}", goal_id)
        .replace("{DESCRIPTION}", description)
        .replace("{NEIGHBORS_SECTION}", neighbors_section)
}

// ── main.rs prompts ─────────────────────────────────────────────────────────

const TEND_INIT_PROMPT_TEMPLATE: &str = include_str!("../prompts/main/tend_init_prompt.md");
const TEND_INIT_PROMPT_FULL_CONTEXT_TEMPLATE: &str = include_str!("../prompts/main/tend_init_prompt_full_context.md");
const DELIVERY_MESSAGE_TEMPLATE: &str = include_str!("../prompts/main/delivery_message.md");
const SILENCE_NUDGE: &str = include_str!("../prompts/main/silence_nudge.md");
const DELIVERY_LOST_WARNING_TEMPLATE: &str = include_str!("../prompts/main/delivery_lost_warning.md");
const PARSE_CORRECTION_TEMPLATE: &str = include_str!("../prompts/main/parse_correction.md");
const PARSE_CORRECTION_SYSTEM_MSG_TEMPLATE: &str = include_str!("../prompts/main/parse_correction_system_msg.md");
const PARSE_CORRECTION_GAVE_UP: &str = include_str!("../prompts/main/parse_correction_gave_up.md");
const BATCH_ACTIVE_MSG: &str = include_str!("../prompts/main/batch_active.md");
const BATCH_IDLE_MSG: &str = include_str!("../prompts/main/batch_idle.md");
const TRIGGERED_SYSTEM_MSG_TEMPLATE: &str = include_str!("../prompts/main/triggered_system_msg.md");
const TEND_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/main/tend_system_prompt.md");

pub fn tend_init_prompt(goals_summary: &str, neighbor_section: &str) -> String {
    TEND_INIT_PROMPT_TEMPLATE
        .replace("{GOALS_SUMMARY}", goals_summary)
        .replace("{NEIGHBOR_SECTION}", neighbor_section)
}

pub fn tend_init_prompt_full_context(goals_summary: &str, neighbor_section: &str) -> String {
    TEND_INIT_PROMPT_FULL_CONTEXT_TEMPLATE
        .replace("{GOALS_SUMMARY}", goals_summary)
        .replace("{NEIGHBOR_SECTION}", neighbor_section)
}

pub fn delivery_message(sender: &str, message: &str) -> String {
    DELIVERY_MESSAGE_TEMPLATE
        .replace("{SENDER}", sender)
        .replace("{MESSAGE}", message)
}

pub fn silence_nudge() -> &'static str {
    SILENCE_NUDGE
}

pub fn delivery_lost_warning(sender: &str, recipient: &str) -> String {
    DELIVERY_LOST_WARNING_TEMPLATE
        .replace("{SENDER}", sender)
        .replace("{RECIPIENT}", recipient)
}

pub fn parse_correction(listing: &str, keys_order: &str) -> String {
    PARSE_CORRECTION_TEMPLATE
        .replace("{LISTING}", listing)
        .replace("{KEYS_ORDER}", keys_order)
}

pub fn parse_correction_system_msg(attempt: u8) -> String {
    PARSE_CORRECTION_SYSTEM_MSG_TEMPLATE
        .replace("{ATTEMPT}", &attempt.to_string())
}

pub fn parse_correction_gave_up() -> &'static str {
    PARSE_CORRECTION_GAVE_UP
}

pub fn batch_active_msg() -> &'static str {
    BATCH_ACTIVE_MSG
}

pub fn batch_idle_msg() -> &'static str {
    BATCH_IDLE_MSG
}

pub fn triggered_system_msg(goal_id: &str, reason: &str) -> String {
    let reason_suffix = if reason.is_empty() {
        String::new()
    } else {
        format!(": {}", reason)
    };
    TRIGGERED_SYSTEM_MSG_TEMPLATE
        .replace("{GOAL_ID}", goal_id)
        .replace("{REASON_SUFFIX}", &reason_suffix)
}

pub fn tend_system_prompt(tend_description: &str) -> String {
    TEND_SYSTEM_PROMPT_TEMPLATE.replace("{TEND_DESCRIPTION}", tend_description)
}

// ── cleanup.rs prompts ──────────────────────────────────────────────────────

const CLEANUP_PROMPT_TEMPLATE: &str = include_str!("../prompts/cleanup/cleanup_prompt.md");

pub fn build_cleanup_prompt(listing: &str) -> String {
    CLEANUP_PROMPT_TEMPLATE.replace("{LISTING}", listing)
}

// ── backend formatting strings ──────────────────────────────────────────────

const TOOL_ERROR_NO_SUMMARY_TEMPLATE: &str = include_str!("../prompts/backends/tool_error_no_summary.md");
const TOOL_ERROR_WITH_SUMMARY_TEMPLATE: &str = include_str!("../prompts/backends/tool_error_with_summary.md");
const TOOL_COMPLETED_NO_SUMMARY: &str = include_str!("../prompts/backends/tool_completed_no_summary.md");
const TOOL_COMPLETED_WITH_SUMMARY_TEMPLATE: &str = include_str!("../prompts/backends/tool_completed_with_summary.md");
const STREAM_ERROR_TEMPLATE: &str = include_str!("../prompts/backends/stream_error.md");

pub fn tool_error_no_summary(tool: &str, error: &str) -> String {
    TOOL_ERROR_NO_SUMMARY_TEMPLATE
        .replace("{TOOL}", tool)
        .replace("{ERROR}", error)
}

pub fn tool_error_with_summary(tool: &str, summary: &str, error: &str) -> String {
    TOOL_ERROR_WITH_SUMMARY_TEMPLATE
        .replace("{TOOL}", tool)
        .replace("{SUMMARY}", summary)
        .replace("{ERROR}", error)
}

pub fn tool_completed_no_summary(tool: &str) -> String {
    TOOL_COMPLETED_NO_SUMMARY.replace("{TOOL}", tool)
}

pub fn tool_completed_with_summary(tool: &str, summary: &str) -> String {
    TOOL_COMPLETED_WITH_SUMMARY_TEMPLATE
        .replace("{TOOL}", tool)
        .replace("{SUMMARY}", summary)
}

pub fn stream_error(msg: &str) -> String {
    STREAM_ERROR_TEMPLATE.replace("{ERROR}", msg)
}

const RETRY_NOTICE_TEMPLATE: &str = include_str!("../prompts/backends/retry_notice.md");

/// Format the user-visible retry notice emitted between HTTP attempts. The
/// `↻` prefix parallels `stream_error`'s `‰` (rendered as `⚠`) so both
/// transport-level notices share a recognizable visual family in the
/// conversation pane. `attempt` is 1-indexed against `max_retries`.
pub fn retry_notice(attempt: usize, max_retries: usize, delay_ms: u64, reason: &str) -> String {
    RETRY_NOTICE_TEMPLATE
        .replace("{ATTEMPT}", &attempt.to_string())
        .replace("{MAX_RETRIES}", &max_retries.to_string())
        .replace("{DELAY_MS}", &delay_ms.to_string())
        .replace("{REASON}", reason)
}

// ── config.rs prompts ───────────────────────────────────────────────────────

const CONFIG_STARTER_TEMPLATE: &str = include_str!("../prompts/config/starter_template.md");

/// Interpolates the per-tier (endpoint, model) defaults into the starter
/// template. Six slots: three endpoint URLs and three model identifiers,
/// one of each per tier (high/mid/low). Auth is intentionally not part of
/// the template — it's sourced from per-tier environment variables by
/// `backends`.
pub fn config_starter_template(
    native_high_endpoint: &str,
    native_high_model: &str,
    native_mid_endpoint: &str,
    native_mid_model: &str,
    native_low_endpoint: &str,
    native_low_model: &str,
) -> String {
    CONFIG_STARTER_TEMPLATE
        .replace("{NATIVE_HIGH_ENDPOINT}", native_high_endpoint)
        .replace("{NATIVE_HIGH_MODEL}", native_high_model)
        .replace("{NATIVE_MID_ENDPOINT}", native_mid_endpoint)
        .replace("{NATIVE_MID_MODEL}", native_mid_model)
        .replace("{NATIVE_LOW_ENDPOINT}", native_low_endpoint)
        .replace("{NATIVE_LOW_MODEL}", native_low_model)
}

// ── test-only prompts ───────────────────────────────────────────────────────

#[cfg(test)]
pub const SUMMARY_REQUEST: &str = include_str!("../prompts/goal_session/summary_request.md");

#[cfg(test)]
mod tests {
    use super::*;

    // spec (prompt-storage): every included prompt file must load successfully.
    // If a file is missing or the path is wrong, this test catches it at test
    // time (compile time would also fail for include_str, but this documents
    // the contract).
    #[test]
    fn test_spec_all_prompt_files_load() {
        // Framework preambles
        assert!(!VCS_RULES.is_empty());
        assert!(!TINKER_DIR_WRITE_RULES.is_empty());
        assert!(!IMPLEMENTATION_OWNERSHIP_MANDATE.is_empty());
        assert!(!MESSAGE_PASSING_AND_PROGRESS_SECTIONS.is_empty());
        assert!(!FRESH_DISPATCH_INSTRUCTIONS.is_empty());
        assert!(!NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE.is_empty());

        // Templates must be non-empty
        assert!(!SESSION_INIT_MESSAGE_TEMPLATE.is_empty());
        assert!(!FRESH_SUBSESSION_INIT_MESSAGE_TEMPLATE.is_empty());
        assert!(!FRESH_SUBSESSION_LEAN_INIT_MESSAGE_TEMPLATE.is_empty());
        assert!(!TEND_INIT_PROMPT_TEMPLATE.is_empty());
        assert!(!TEND_INIT_PROMPT_FULL_CONTEXT_TEMPLATE.is_empty());
        assert!(!DELIVERY_MESSAGE_TEMPLATE.is_empty());
        assert!(!SILENCE_NUDGE.is_empty());
        assert!(!DELIVERY_LOST_WARNING_TEMPLATE.is_empty());
        assert!(!PARSE_CORRECTION_TEMPLATE.is_empty());
        assert!(!PARSE_CORRECTION_SYSTEM_MSG_TEMPLATE.is_empty());
        assert!(!PARSE_CORRECTION_GAVE_UP.is_empty());
        assert!(!BATCH_ACTIVE_MSG.is_empty());
        assert!(!BATCH_IDLE_MSG.is_empty());
        assert!(!TRIGGERED_SYSTEM_MSG_TEMPLATE.is_empty());

        assert!(!CLEANUP_PROMPT_TEMPLATE.is_empty());

        assert!(!TOOL_ERROR_NO_SUMMARY_TEMPLATE.is_empty());
        assert!(!TOOL_ERROR_WITH_SUMMARY_TEMPLATE.is_empty());
        assert!(!TOOL_COMPLETED_NO_SUMMARY.is_empty());
        assert!(!TOOL_COMPLETED_WITH_SUMMARY_TEMPLATE.is_empty());

        assert!(!STREAM_ERROR_TEMPLATE.is_empty());

        assert!(!CONFIG_STARTER_TEMPLATE.is_empty());

        assert!(!TEND_SYSTEM_PROMPT_TEMPLATE.is_empty());

        assert!(!SUMMARY_REQUEST.is_empty());
    }

    // spec (prompt-storage): templating substitutes placeholders and leaves
    // non-placeholder text unchanged.
    #[test]
    fn test_spec_templating_substitutes_placeholders() {
        let result = session_init_message(
            "test-goal", "[]", "do stuff", "vcs line", "dir line\n", "own line\n",
            "## Neighbors\n", "msg passing", "fresh dispatch", "\n\n## Reason\nxyz",
        );
        assert!(result.contains("test-goal"), "goal-id must be substituted");
        assert!(result.contains("do stuff"), "description must be substituted");
        assert!(result.contains("vcs line"), "vcs_rules must be substituted");
        assert!(result.contains("msg passing"), "message_passing must be substituted");
        assert!(result.contains("## Neighbors"), "neighbors_section must be substituted");
        assert!(result.contains("## Reason"), "reason_section must be substituted");
    }

    // spec (prompt-storage): placeholders that are not present in the template
    // are silently dropped (str::replace is idempotent).
    #[test]
    fn test_spec_templating_idempotent_on_missing_placeholders() {
        let template = "Hello {NAME}!";
        let r1 = template.replace("{NAME}", "world");
        let r2 = r1.replace("{NAME}", "world"); // second replace is no-op
        assert_eq!(r1, r2);
    }

    // spec (prompt-storage): empty inputs produce valid output (no panic).
    #[test]
    fn test_spec_templating_handles_empty_inputs() {
        let result = session_init_message(
            "", "", "", "", "", "", "", "", "", "",
        );
        assert!(!result.is_empty(), "template with empty inputs must still produce non-empty output (boilerplate remains)");
    }

    // spec (prompt-storage): literal braces in prompt text are preserved when
    // they don't match a placeholder. Template variables use {UPPER_CASE}
    // placeholders; text that merely contains braces must pass through
    // unchanged. Verified with a synthetic template that has a literal brace
    // but no matching placeholder.
    #[test]
    fn test_spec_templating_preserves_literal_braces() {
        let template = "render {not_a_placeholder} here";
        let result = template.replace("{PLACEHOLDER}", "value");
        assert_eq!(
            result, "render {not_a_placeholder} here",
            "literal braces that don't match a placeholder must be preserved"
        );
    }

    // spec (prompt-storage): summary_request template must contain the four
    // structured parts.
    #[test]
    fn test_spec_summary_request_has_structured_parts() {
        let s = SUMMARY_REQUEST;
        assert!(s.contains("1."), "must request part 1 (accomplishments)");
        assert!(s.contains("2."), "must request part 2 (design changes)");
        assert!(s.contains("3."), "must request part 3 (decisions beyond the goal)");
        assert!(s.contains("4."), "must request part 4 (how to try it)");
        assert!(s.to_lowercase().contains("decision"));
        assert!(s.to_lowercase().contains("try"));
    }

    // spec (prompt-storage): fresh_subsession_init_message substitutes all 12
    // placeholders and leaves no raw placeholders in output.
    #[test]
    fn test_spec_fresh_subsession_init_message_substitutes_all_placeholders() {
        let result = fresh_subsession_init_message(
            "sid-1", "disp-1", " label", "do task", "[]", "goal-1", "desc",
            "msg passing", "vcs", "tinker", "ownership", "neighbors",
        );
        assert!(result.contains("sid-1"), "session_id must be substituted");
        assert!(result.contains("disp-1"), "dispatcher_id must be substituted");
        assert!(result.contains("do task"), "task must be substituted");
        assert!(result.contains("goal-1"), "goal_id must be substituted");
        assert!(result.contains("msg passing"), "message_passing must be substituted");
        assert!(result.contains("neighbors"), "neighbors_section must be substituted");
        assert!(!result.contains("{SESSION_ID}"), "no raw placeholders left");
        assert!(!result.contains("{DISPATCHER_ID}"), "no raw placeholders left");
        assert!(!result.contains("{TASK}"), "no raw placeholders left");
        assert!(!result.contains("{GOAL_ID}"), "no raw placeholders left");
        assert!(!result.contains("{MESSAGE_PASSING_AND_PROGRESS}"), "no raw placeholders left");
    }

    // spec (prompt-storage): fresh_subsession_lean_init_message substitutes all
    // 8 placeholders and leaves no raw placeholders in output.
    #[test]
    fn test_spec_fresh_subsession_lean_init_message_substitutes_all_placeholders() {
        let result = fresh_subsession_lean_init_message(
            "sid-2", "disp-2", " lbl", "task", "[idx]", "g-2", "d-2", "ns",
        );
        assert!(result.contains("sid-2"), "session_id must be substituted");
        assert!(result.contains("disp-2"), "dispatcher_id must be substituted");
        assert!(result.contains("task"), "task must be substituted");
        assert!(result.contains("g-2"), "goal_id must be substituted");
        assert!(!result.contains("{SESSION_ID}"), "no raw placeholders left");
        assert!(!result.contains("{DISPATCHER_ID}"), "no raw placeholders left");
        assert!(!result.contains("{TASK}"), "no raw placeholders left");
        assert!(!result.contains("{GOAL_ID}"), "no raw placeholders left");
    }

    // spec (prompt-storage): tend_init_prompt substitutes GOALS_SUMMARY and
    // NEIGHBOR_SECTION placeholders.
    #[test]
    fn test_spec_tend_init_prompt_substitutes_placeholders() {
        let result = tend_init_prompt("goals here", "neighbors here");
        assert!(result.contains("goals here"), "goals_summary must be substituted");
        assert!(result.contains("neighbors here"), "neighbor_section must be substituted");
        assert!(!result.contains("{GOALS_SUMMARY}"), "no raw placeholders left");
        assert!(!result.contains("{NEIGHBOR_SECTION}"), "no raw placeholders left");
    }

    // spec (prompt-storage): tend_init_prompt_full_context substitutes its two
    // placeholders.
    #[test]
    fn test_spec_tend_init_prompt_full_context_substitutes_placeholders() {
        let result = tend_init_prompt_full_context("full goals", "full neighbors");
        assert!(result.contains("full goals"), "goals_summary must be substituted");
        assert!(result.contains("full neighbors"), "neighbor_section must be substituted");
        assert!(!result.contains("{GOALS_SUMMARY}"), "no raw placeholders left");
        assert!(!result.contains("{NEIGHBOR_SECTION}"), "no raw placeholders left");
    }

    // spec (prompt-storage): config_starter_template substitutes all six
    // placeholders — three endpoint URLs and three model identifiers, one
    // of each per tier (high/mid/low).
    #[test]
    fn test_spec_config_starter_template_substitutes_placeholders() {
        let result = config_starter_template("eh", "mh", "em", "mm", "el", "ml");
        assert!(result.contains("eh"), "native_high_endpoint must be substituted");
        assert!(result.contains("mh"), "native_high_model must be substituted");
        assert!(result.contains("em"), "native_mid_endpoint must be substituted");
        assert!(result.contains("mm"), "native_mid_model must be substituted");
        assert!(result.contains("el"), "native_low_endpoint must be substituted");
        assert!(result.contains("ml"), "native_low_model must be substituted");
        assert!(!result.contains("{NATIVE_HIGH_ENDPOINT}"), "no raw placeholders left");
        assert!(!result.contains("{NATIVE_HIGH_MODEL}"), "no raw placeholders left");
        assert!(!result.contains("{NATIVE_MID_ENDPOINT}"), "no raw placeholders left");
        assert!(!result.contains("{NATIVE_MID_MODEL}"), "no raw placeholders left");
        assert!(!result.contains("{NATIVE_LOW_ENDPOINT}"), "no raw placeholders left");
        assert!(!result.contains("{NATIVE_LOW_MODEL}"), "no raw placeholders left");
    }

    #[test]
    fn test_spec_tool_error_no_summary_substitutes_placeholders() {
        let result = tool_error_no_summary("read", "denied");
        assert!(result.contains("read"), "tool must be substituted");
        assert!(result.contains("denied"), "error must be substituted");
        assert!(!result.contains("{TOOL}"), "no raw placeholders left");
        assert!(!result.contains("{ERROR}"), "no raw placeholders left");
    }

    #[test]
    fn test_spec_tool_error_with_summary_substitutes_placeholders() {
        let result = tool_error_with_summary("write", "src/main.rs", "fail");
        assert!(result.contains("write"), "tool must be substituted");
        assert!(result.contains("src/main.rs"), "summary must be substituted");
        assert!(result.contains("fail"), "error must be substituted");
        assert!(!result.contains("{TOOL}"), "no raw placeholders left");
        assert!(!result.contains("{SUMMARY}"), "no raw placeholders left");
        assert!(!result.contains("{ERROR}"), "no raw placeholders left");
    }

    #[test]
    fn test_spec_tool_completed_no_summary_substitutes_placeholders() {
        let result = tool_completed_no_summary("bash");
        assert!(result.contains("bash"), "tool must be substituted");
        assert!(!result.contains("{TOOL}"), "no raw placeholders left");
    }

    #[test]
    fn test_spec_tool_completed_with_summary_substitutes_placeholders() {
        let result = tool_completed_with_summary("read", "src/lib.rs");
        assert!(result.contains("read"), "tool must be substituted");
        assert!(result.contains("src/lib.rs"), "summary must be substituted");
        assert!(!result.contains("{TOOL}"), "no raw placeholders left");
        assert!(!result.contains("{SUMMARY}"), "no raw placeholders left");
    }

    #[test]
    fn test_spec_stream_error_substitutes_placeholders() {
        let result = stream_error("timed out");
        assert!(result.contains("timed out"), "msg must be substituted");
        assert!(!result.contains("{ERROR}"), "no raw placeholders left");
    }

    // spec (prompt-storage): retry_notice substitutes all four placeholders
    // (ATTEMPT, MAX_RETRIES, DELAY_MS, REASON) and leaves no raw template
    // markers. The ↻ prefix must be present so the notice is visually
    // distinct from the ⚠ error marker in the conversation pane.
    #[test]
    fn test_spec_retry_notice_substitutes_placeholders() {
        let result = retry_notice(2, 3, 2000, "HTTP 503");
        assert!(result.contains("2"), "attempt must be substituted");
        assert!(result.contains("3"), "max_retries must be substituted");
        assert!(result.contains("2000"), "delay_ms must be substituted");
        assert!(result.contains("HTTP 503"), "reason must be substituted");
        assert!(result.contains("↻"), "must carry the retry glyph");
        assert!(!result.contains("{ATTEMPT}"), "no raw placeholders left");
        assert!(!result.contains("{MAX_RETRIES}"), "no raw placeholders left");
        assert!(!result.contains("{DELAY_MS}"), "no raw placeholders left");
        assert!(!result.contains("{REASON}"), "no raw placeholders left");
    }

    // spec (prompt-storage): triggered_system_msg substitutes placeholders
    // correctly for both empty and non-empty reason paths.
    #[test]
    fn test_spec_triggered_system_msg_empty_reason() {
        let result = triggered_system_msg("goal-a", "");
        assert!(result.contains("goal-a"), "goal_id must be substituted");
        assert!(!result.contains("{GOAL_ID}"), "no raw placeholders left");
        // With empty reason, REASON_SUFFIX should be blank — output ends with the backtick
        assert!(
            result.ends_with("`goal-a`"),
            "empty reason must leave output ending in backtick-quoted goal id"
        );
    }

    #[test]
    fn test_spec_triggered_system_msg_non_empty_reason() {
        let result = triggered_system_msg("goal-b", "investigate");
        assert!(result.contains("goal-b"), "goal_id must be substituted");
        assert!(result.contains(": investigate"), "reason must appear with colon prefix");
        assert!(!result.contains("{GOAL_ID}"), "no raw placeholders left");
        assert!(!result.contains("{REASON_SUFFIX}"), "no raw placeholders left");
    }

    // spec (prompt-storage): tend_system_prompt substitutes TEND_DESCRIPTION.
    #[test]
    fn test_spec_tend_system_prompt_substitutes_placeholders() {
        let result = tend_system_prompt("tend does things");
        assert!(result.contains("tend does things"), "description must be substituted");
        assert!(!result.contains("{TEND_DESCRIPTION}"), "no raw placeholders left");
    }
}
