use crate::goal::Goal;

const JOG_FRONTMATTER: &str = "---\ndescription: >-\n  Jog — audits intent\u{2194}spec alignment through Socratic deepening.\nmode: primary\npermission:\n  task: deny\n  todowrite: deny\n  skill: deny\n---\n";

/// Parses the bundled `jog.toml` into a `Goal` struct via the standard pipeline.
/// Uses the same `toml::from_str::<Goal>()` path as every other goal in the system.
pub fn packaged_goal() -> Goal {
    const TOML: &str = include_str!("../packaged-goals/jog.toml");
    toml::from_str(TOML).expect("packaged jog.toml must be valid Goal TOML")
}

/// Returns the content for the `jog` opencode agent file.
/// Installed to `~/.config/opencode/agents/jog.md` at startup.
/// Frontmatter only — the description is now injected via session_init_message
/// so jog receives the same framework preamble as every other goal agent.
/// The frontmatter carries the task/todowrite denials; the agent file body is empty.
pub fn jog_agent_content() -> String {
    JOG_FRONTMATTER.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jog_description() -> String {
        super::packaged_goal().description
    }

    // spec (jog): jog is a chat audit session, not a planner.
    // Planning tools must be denied in the agent file.
    #[test]
    fn test_spec_jog_agent_denies_planning_tools() {
        let content = jog_agent_content();
        assert!(content.contains("task: deny"), "jog must deny task");
        assert!(content.contains("todowrite: deny"), "jog must deny todowrite");
    }

    // spec (goal-agents): jog agent file must be frontmatter-only. The description
    // is now injected via session_init_message, giving jog the same framework preamble
    // as every other goal agent. Having the description in both the agent file body and
    // the init message would duplicate it.
    #[test]
    fn test_spec_jog_agent_file_frontmatter_only() {
        let content = jog_agent_content();
        let after_frontmatter = content.splitn(3, "---").nth(2).unwrap_or("").trim();
        assert!(
            after_frontmatter.is_empty(),
            "jog agent file must have no body — description comes through session_init_message, not the agent file",
        );
    }

    // spec (jog): jog invocation is on-request. The description must state that
    // triggering is on-request / not automated.
    #[test]
    fn test_spec_jog_system_prompt_describes_pull_invocation() {
        let prompt = jog_description();
        assert!(
            prompt.contains("On-request") || prompt.contains("on-request")
                || prompt.contains("on demand") || prompt.contains("on request"),
            "jog description must describe on-request invocation (not automated triggering)",
        );
    }

    // spec (jog): jog sends read-only @-messages to peer agents — it reads both
    // sources of truth by consulting agents, never by writing or modifying.
    // The description must name the read-only consultation discipline.
    #[test]
    fn test_spec_jog_system_prompt_names_no_cue_questioning() {
        let prompt = jog_description();
        assert!(
            prompt.contains("read-only") || prompt.contains("read only")
                || prompt.contains("don't act on it"),
            "jog description must name the read-only consultation discipline",
        );
    }

    // spec (jog): jog performs both a forward check (N→N+1 coverage: things in
    // the derived source that are missing from the base) and a backward check
    // (N+1→N provenance: things in the base with no origin). The description must
    // name both check directions.
    #[test]
    fn test_spec_jog_system_prompt_names_two_findings() {
        let prompt = jog_description();
        assert!(
            prompt.contains("forward") || prompt.contains("Forward"),
            "jog description must name the forward check direction",
        );
        assert!(
            prompt.contains("backward") || prompt.contains("Backward"),
            "jog description must name the backward check direction",
        );
    }

    // spec (jog): jog sends @tend and @rummage queries to read the spec and code
    // layers respectively. These consultations are always read-only — jog does
    // not commission fixes. The description must name both recipients and the
    // read-only constraint.
    #[test]
    fn test_spec_jog_commission_uses_at_tend() {
        let prompt = jog_description();
        assert!(
            prompt.contains("@tend"),
            "jog description must name @tend as a peer agent it consults",
        );
        // jog does NOT commission fixes — it documents discrepancies.
        assert!(
            prompt.contains("No writes") || prompt.contains("no commissions")
                || prompt.contains("doesn't dispatch") || prompt.contains("does not dispatch"),
            "jog description must state it has no commissions — jog documents, doesn't dispatch",
        );
    }

    // spec (jog): jog produces no writes or fixes during a run. The description
    // must state this constraint so jog does not cross into rummage's territory.
    #[test]
    fn test_spec_jog_prohibits_investigation_code() {
        let prompt = jog_description();
        assert!(
            prompt.contains("No writes") || prompt.contains("no fixes")
                || prompt.contains("no commissions") || prompt.contains("writing goals or code"),
            "jog description must state that writes, fixes, and commissions are out of scope",
        );
    }

    // spec (jog): jog reads but does not write — no code, no goal files, no
    // instrumentation during a run.
    #[test]
    fn test_spec_jog_does_not_probe_running_system() {
        let prompt = jog_description();
        assert!(
            prompt.contains("No writes") || prompt.contains("no fixes")
                || prompt.contains("read-only") || prompt.contains("read only"),
            "jog description must state it is read-only and does not write during a run",
        );
    }

    // spec (jog): jog does not write goals. SCOPE must name this as out of scope.
    #[test]
    fn test_spec_jog_does_not_write_goals() {
        let prompt = jog_description();
        assert!(
            prompt.contains("writing goals or code") || prompt.contains("write goals")
                || prompt.contains("Out of scope") || prompt.contains("out of scope"),
            "jog description must state that writing goals or code directly is out of scope",
        );
    }

    // spec (jog): jog documents discrepancies but does not dispatch fix commissions.
    // This is a hard boundary — a discrepancy run ends with a log, not with @-dispatch.
    #[test]
    fn test_spec_jog_commission_is_terminal_for_thread() {
        let prompt = jog_description();
        assert!(
            prompt.contains("doesn't dispatch") || prompt.contains("does not dispatch")
                || prompt.contains("jog documents") || prompt.contains("no commissions"),
            "jog description must state it documents discrepancies but doesn't dispatch fixes",
        );
    }

    // spec (jog): jog compares two sets step by step, finding things present in
    // one source but absent in the other. The description must name this comparison
    // loop as jog's core process.
    #[test]
    fn test_spec_jog_probes_the_why() {
        let prompt = jog_description();
        assert!(
            prompt.contains("compares") || prompt.contains("not in both")
                || prompt.contains("bidirectional"),
            "jog description must name step-by-step set comparison as the core process",
        );
    }

    // spec (jog): each run produces a discrepancy log under .tinker/discrepancies/.
    // The description must name the output location.
    #[test]
    fn test_spec_jog_form_norm_minimum_viable_shape() {
        let prompt = jog_description();
        assert!(
            prompt.contains(".tinker/discrepancies") || prompt.contains("discrepancy log"),
            "jog description must name the discrepancy log output location",
        );
    }

    // spec (jog): jog uses @rummage to read the code layer and @tend to read the
    // spec layer. The description must name @rummage as the code-reading arm.
    #[test]
    fn test_spec_jog_prompt_names_rummage_as_code_reality_arm() {
        let prompt = jog_description();
        assert!(
            prompt.contains("@rummage"),
            "jog description must name @rummage as the peer agent consulted for code-layer reading",
        );
        // @rummage queries are read-only during a jog run.
        assert!(
            prompt.contains("read-only") || prompt.contains("read only")
                || prompt.contains("don't act on it"),
            "jog description must state that @rummage consultations are read-only",
        );
    }
}
