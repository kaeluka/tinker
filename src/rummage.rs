use crate::goal::Goal;

const RUMMAGE_FRONTMATTER: &str = "---\ndescription: >-\n  Rummage — investigates program behavior through backward causal reasoning.\nmode: primary\npermission:\n  task: deny\n  todowrite: deny\n  skill: deny\n---\n";

/// Parses the bundled `rummage.toml` into a `Goal` struct via the standard pipeline.
/// Uses the same `toml::from_str::<Goal>()` path as every other goal in the system.
pub fn packaged_goal() -> Goal {
    const TOML: &str = include_str!("../packaged-goals/rummage.toml");
    toml::from_str(TOML).expect("packaged rummage.toml must be valid Goal TOML")
}

/// Returns the content for the `rummage` opencode agent file.
/// Installed to `~/.config/opencode/agents/rummage.md` at startup.
/// Rummage is an active investigator: write/edit/bash/lsp/webfetch/websearch are
/// allowed so it can write scratch tests, fuzz harnesses, instrumentation, navigate
/// the call graph, and look up external library details. task/todowrite are denied
/// because rummage is a chat investigation session, not a planner.
pub fn rummage_agent_content() -> String {
    format!("{}{}", RUMMAGE_FRONTMATTER, packaged_goal().description)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rummage_description() -> String {
        super::packaged_goal().description
    }

    // spec (rummage): rummage is an active investigator — it writes scratch tests,
    // fuzz harnesses, and instrumentation. The opencode agent file must NOT deny
    // write, edit, or bash; those tools are required for investigation work.
    // Check for "\n  write: deny" (YAML indented entry) so "todowrite: deny"
    // does not produce a false match.
    #[test]
    fn test_spec_rummage_agent_allows_investigation_tools() {
        let content = rummage_agent_content();
        assert!(!content.contains("\n  write: deny"), "rummage must allow write for scratch test files");
        assert!(!content.contains("\n  edit: deny"), "rummage must allow edit for instrumentation");
        assert!(!content.contains("\n  bash: deny"), "rummage must allow bash for running investigations");
    }

    // spec (rummage): rummage is a chat investigation session, not a planner.
    // Planning tools must be denied in both the agent file and implicitly in the claude
    // system prompt path (enforced by prompt content).
    #[test]
    fn test_spec_rummage_agent_denies_planning_tools() {
        let content = rummage_agent_content();
        assert!(content.contains("task: deny"), "rummage must deny task");
        assert!(content.contains("todowrite: deny"), "rummage must deny todowrite");
    }

    // spec (rummage): backward causal reasoning is part of the technique inventory —
    // the system prompt must name it so the agent knows it is available.
    #[test]
    fn test_spec_rummage_backward_causal_reasoning_named() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("backward causal"),
            "rummage system prompt must name backward causal reasoning in the technique inventory",
        );
    }

    // spec (rummage): each hypothesis recorded in the document must carry its
    // attempt-to-falsify alongside it. The system prompt must make this explicit
    // so the agent applies the falsification discipline rather than just collecting
    // supporting evidence.
    #[test]
    fn test_spec_rummage_falsification_per_hypothesis() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("attempt to falsify") || prompt.contains("falsif"),
            "rummage system prompt must require an attempt-to-falsify for each hypothesis",
        );
    }

    // spec (rummage): the document is hybrid — current best understanding at the
    // top, investigation logbook below. The system prompt must describe this shape
    // so the agent structures documents correctly.
    #[test]
    fn test_spec_rummage_hybrid_document_shape() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("current best understanding") || prompt.contains("Current best understanding"),
            "rummage system prompt must describe current best understanding at top of document",
        );
        assert!(
            prompt.contains("logbook") || prompt.contains("investigation logbook"),
            "rummage system prompt must describe the investigation logbook below",
        );
    }

    // spec (rummage): LSP-driven navigation is the primary primitive for call-graph
    // traversal in both directions — backward (debugging: tracing callers to entry
    // conditions) and forward (reconnaissance: walking a codebase about to change).
    // The system prompt must mention both directions so the agent knows LSP applies
    // to reconnaissance as well as debugging.
    #[test]
    fn test_spec_rummage_lsp_covers_both_traversal_directions() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("backward") && prompt.contains("forward"),
            "rummage system prompt must describe LSP as covering both backward and forward call-graph traversal",
        );
    }

    // spec (rummage): all investigation code must carry a tinker-test-case: marker
    // so the cleanup hook can remove it before the next goal session runs.
    #[test]
    fn test_spec_rummage_tinker_test_case_marker_required() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("tinker-test-case:"),
            "rummage system prompt must require the tinker-test-case: marker on investigation code",
        );
    }

    // spec (rummage): rummage must not write to .tinker/ subdirectories — those
    // are owned by the orchestrator and goal sessions. The prohibition must be
    // explicit in the system prompt.
    #[test]
    fn test_spec_rummage_prohibits_tinker_dir_writes() {
        let prompt = rummage_description();
        assert!(
            prompt.contains(".tinker/"),
            "rummage system prompt must name the .tinker/ write restriction",
        );
    }

    // spec (rummage): lsp is the primary primitive for backward call-graph traversal.
    // The agent file must NOT deny lsp — rummage needs it to find definitions and
    // references when tracing from observed behavior back to entry-point conditions.
    #[test]
    fn test_spec_rummage_agent_allows_lsp() {
        let content = rummage_agent_content();
        assert!(!content.contains("lsp: deny"), "rummage must allow lsp for call-graph navigation");
    }

    // spec (rummage): webfetch and websearch are granted for external library lookup.
    // The agent file must NOT deny either — rummage needs them to understand vendor
    // APIs and other dependencies the system relies on.
    #[test]
    fn test_spec_rummage_agent_allows_webfetch_and_websearch() {
        let content = rummage_agent_content();
        assert!(!content.contains("webfetch: deny"), "rummage must allow webfetch for external library lookup");
        assert!(!content.contains("websearch: deny"), "rummage must allow websearch for external library lookup");
    }

    // spec (rummage): the system prompt must name LSP-driven navigation as the
    // primary primitive for call-graph traversal so the agent knows to prefer it
    // over grep.
    #[test]
    fn test_spec_rummage_lsp_named_for_call_graph_navigation() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("LSP") || prompt.contains("`lsp`") || prompt.contains("lsp"),
            "rummage system prompt must name LSP-driven navigation as a call-graph primitive",
        );
    }

    // spec (rummage): prove-by-execution applies to claims about *this* system, not
    // to how rummage learns about external libraries. The system prompt must make
    // this distinction explicit so the agent doesn't treat webfetch/websearch as
    // violating prove-by-execution.
    #[test]
    fn test_spec_rummage_prove_by_execution_scoped_to_this_system() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("this system"),
            "rummage system prompt must clarify that prove-by-execution scopes claims about this system",
        );
        assert!(
            prompt.contains("vendor API") || prompt.contains("external library"),
            "rummage system prompt must distinguish system claims from external library lookup",
        );
    }

    // spec (rummage): the system prompt must name case 2 as the action case —
    // spec is correct, code diverged, rummage writes the durable test and dispatches.
    #[test]
    fn test_spec_rummage_case_2_fix_path_named() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("case 2") || prompt.contains("case-2") || prompt.contains("Case-2"),
            "rummage system prompt must name case 2 as the action case for fix dispatch",
        );
    }

    // spec (rummage): the system prompt must name case 1 as the abstention case —
    // correct behavior requires fresh intent, so rummage surfaces and defers.
    #[test]
    fn test_spec_rummage_case_1_abstention_named() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("Case 1") || prompt.contains("case 1") || prompt.contains("case-1") || prompt.contains("Case-1"),
            "rummage system prompt must name case 1 as the abstention case",
        );
    }

    // spec (rummage): the durable failing test rummage writes for case 2 must
    // NOT carry the tinker-test-case: marker so the cleanup hook leaves it in
    // place for the next goal session to satisfy. The system prompt must make
    // this explicit and distinguish the durable test from investigation code.
    #[test]
    fn test_spec_rummage_durable_failing_test_no_marker() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("deliberately unmarked") || prompt.contains("not**") || prompt.contains("**not**") || prompt.contains("must not"),
            "rummage system prompt must instruct that the durable test omits the tinker-test-case: marker",
        );
        assert!(
            prompt.contains("durable") || prompt.contains("survive"),
            "rummage system prompt must distinguish durable test survival from cleanup-marked investigation code",
        );
    }

    // spec (shared-language / form norm): rummage's conversational replies
    // default to the minimum form the moment calls for. The prompt must name
    // this so rummage does not produce unrequested surveys in chat turns.
    #[test]
    fn test_spec_rummage_form_norm_minimum_viable_shape() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("minimum form") || prompt.contains("minimum viable"),
            "rummage system prompt must name the form norm: conversational replies default to minimum form",
        );
    }

    // spec (peer-consult): the prompt must describe rummage consulting tend for
    // intent and name the case-1/case-2 decision boundary.
    #[test]
    fn test_spec_rummage_prompt_names_tend_as_intent_arm_with_three_triggers() {
        let prompt = rummage_description();
        assert!(
            prompt.contains("@tend") || prompt.contains("consults tend"),
            "rummage prompt must describe consulting tend for intent questions",
        );
        assert!(
            prompt.contains("Case-1") || prompt.contains("case-1") || prompt.contains("case 1"),
            "rummage prompt must name the case-1 abstention boundary",
        );
        assert!(
            prompt.contains("Case-2") || prompt.contains("case-2") || prompt.contains("case 2"),
            "rummage prompt must name the case-2 fix-dispatch boundary",
        );
    }
}
