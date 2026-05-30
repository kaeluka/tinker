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
/// Jog is a Socratic investigator: write/edit/bash are available for reading
/// goal files via the shell, but jog writes no investigation code and does not
/// probe the running system. task/todowrite are denied because jog is a chat
/// audit session, not a planner.
pub fn jog_agent_content() -> String {
    format!("{}{}", JOG_FRONTMATTER, packaged_goal().description)
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

    // spec (jog): pull-only invocation — the user names a topic and jog resolves
    // it to relevant claims. The system prompt must describe this pull pattern.
    #[test]
    fn test_spec_jog_system_prompt_describes_pull_invocation() {
        let prompt = jog_description();
        assert!(
            prompt.contains("Pull only") || prompt.contains("on demand") || prompt.contains("invokes you"),
            "jog system prompt must describe pull invocation by user-named topic",
        );
    }

    // spec (jog): Socratic no-cue questioning — jog never leads the witness.
    // The system prompt must name the no-cue discipline.
    #[test]
    fn test_spec_jog_system_prompt_names_no_cue_questioning() {
        let prompt = jog_description();
        assert!(
            prompt.contains("no-cue") || prompt.contains("No-cue") || prompt.contains("never lead") || prompt.contains("leading"),
            "jog system prompt must name the no-cue questioning discipline",
        );
    }

    // spec (jog): two findings — "I didn't know" (user drifted from correct goal)
    // and "I know better" (goal is thin or wrong). Both must be named.
    #[test]
    fn test_spec_jog_system_prompt_names_two_findings() {
        let prompt = jog_description();
        assert!(
            prompt.contains("I didn't know") || prompt.contains("didn't know"),
            "jog system prompt must name the 'I didn't know' finding",
        );
        assert!(
            prompt.contains("I know better") || prompt.contains("know better"),
            "jog system prompt must name the 'I know better' finding",
        );
    }

    // spec (jog): on an "I know better" finding, jog commissions the fix via an
    // @tend block. The system prompt must name this commission format.
    #[test]
    fn test_spec_jog_commission_uses_at_tend() {
        let prompt = jog_description();
        assert!(
            prompt.contains("@tend"),
            "jog system prompt must describe commissioning tend via an @tend block",
        );
        // The block must be self-contained — the prompt teaches what to include.
        assert!(
            prompt.contains("goal-id") || prompt.contains("goal `X`") || prompt.contains("self-contained"),
            "jog system prompt must say the @tend block must carry goal-id and change description",
        );
    }

    // spec (jog): jog does not write investigation code. The system prompt must
    // state that investigation code is not jog's tool.
    #[test]
    fn test_spec_jog_prohibits_investigation_code() {
        let prompt = jog_description();
        // jog's scope excludes running/probing the system; investigation code is out of scope
        assert!(
            prompt.contains("probing the running system") || prompt.contains("do not probe")
                || prompt.contains("Do not probe") || prompt.contains("investigation code"),
            "jog system prompt must state that probing the running system is out of scope for jog",
        );
    }

    // spec (jog): jog does not probe the running system — it reads goal files
    // for context but does not instrument or run code. The system prompt must
    // state this boundary.
    #[test]
    fn test_spec_jog_does_not_probe_running_system() {
        let prompt = jog_description();
        assert!(
            prompt.contains("do not probe") || prompt.contains("not probe") || prompt.contains("Do not probe")
                || prompt.contains("probing the running system"),
            "jog system prompt must state that jog does not probe the running system",
        );
    }

    // spec (jog): jog does not write goals — only tend writes goals; jog
    // commissions the edit via /jog-edit. The system prompt must name this boundary.
    #[test]
    fn test_spec_jog_does_not_write_goals() {
        let prompt = jog_description();
        assert!(
            prompt.contains("do not write goals") || prompt.contains("Do not write goals")
                || prompt.contains("Only `tend` writes goals") || prompt.contains("only `tend` writes goals")
                || prompt.contains("writing goals directly"),
            "jog system prompt must state that jog does not write goals itself",
        );
    }

    // spec (jog): a commission is terminal for a thread — an @tend block fires
    // the moment the reply finalises, so jog must never pose a question in the
    // same turn as a commission. The system prompt must state this constraint.
    #[test]
    fn test_spec_jog_commission_is_terminal_for_thread() {
        let prompt = jog_description();
        assert!(
            prompt.contains("terminal for a thread") || prompt.contains("no open question"),
            "jog system prompt must state that a commission is terminal for a thread",
        );
        assert!(
            prompt.contains("Never emit") || prompt.contains("never emit")
                || prompt.contains("open question") || prompt.contains("no open question"),
            "jog system prompt must prohibit posing a question and emitting an @tend commission in the same reply",
        );
    }

    // spec (jog): the why of a feature is where drift hides — the system prompt
    // must name probing the why as a core part of the deepening.
    #[test]
    fn test_spec_jog_probes_the_why() {
        let prompt = jog_description();
        assert!(
            prompt.contains("*why*") || prompt.contains("the why") || prompt.contains("why of"),
            "jog system prompt must name probing the why as part of the deepening",
        );
    }

    // spec (shared-language / form norm): jog's conversational replies default
    // to the minimum form the moment calls for — a direct statement or question.
    // The prompt must state this so jog does not produce unrequested surveys.
    #[test]
    fn test_spec_jog_form_norm_minimum_viable_shape() {
        let prompt = jog_description();
        assert!(
            prompt.contains("minimum form") || prompt.contains("minimum viable"),
            "jog system prompt must name the form norm: replies default to minimum form",
        );
    }

    // spec (jog): rummage is jog's code-reality arm — consulted when a deepening
    // thread surfaces a claim that may have drifted because the *implementation*
    // changed (not the user's intent). The prompt must name this specific trigger.
    #[test]
    fn test_spec_jog_prompt_names_rummage_as_code_reality_arm() {
        let prompt = jog_description();
        assert!(
            prompt.contains("code-reality") || prompt.contains("@rummage"),
            "jog prompt must name @rummage for code-reality grounding",
        );
        assert!(
            prompt.contains("implementation changed") || prompt.contains("implementation* changed")
                || prompt.contains("*implementation* changed"),
            "jog prompt must name implementation change as the trigger for consulting @rummage",
        );
    }
}
