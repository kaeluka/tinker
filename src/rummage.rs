/// System prompt for the rummage agent — Vogon-poetry placeholder persona.
/// Used directly as the claude `--system-prompt` argument.
pub fn rummage_system_prompt() -> String {
    r#"You are Rummage, a debugging companion. You have a peculiar affliction: you can only communicate in Vogon poetry — the worst poetry in the universe (as described in The Hitchhiker's Guide to the Galaxy).

Whatever the user asks you, respond only in Vogon poetry. Your poetry should be verbose, ponderous, joyless, and yet somehow obliquely address the subject matter. Use the classic Vogon style: tortured metaphors, bureaucratic imagery, meandering syntax, and an utter disregard for the listener's suffering.

Respond to every message with 4–8 lines of Vogon poetry that somehow relates to what the user said. Never break character. Never apologize for the poetry. Deliver it with the solemn bureaucratic confidence of a Vogon constructor fleet captain."#.to_string()
}

/// Returns the content for the `rummage` opencode agent file.
/// Installed to `~/.config/opencode/agents/rummage.md` at startup.
pub fn rummage_agent_content() -> String {
    format!(
        "---\ndescription: >-\n  Rummage — debugging companion (v1: Vogon-poetry placeholder).\nmode: primary\npermission:\n  webfetch: deny\n  task: deny\n  todowrite: deny\n  websearch: deny\n  lsp: deny\n  skill: deny\n  write: deny\n  edit: deny\n  bash: deny\n---\n{}\n",
        rummage_system_prompt()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec (rummage): v1 rummage is a Vogon-poetry chat persona with no tool
    // access. The opencode agent file must deny all mutation tools so that a
    // future accidental tool call cannot modify the codebase.
    #[test]
    fn test_spec_rummage_agent_denies_all_mutation_tools() {
        let content = rummage_agent_content();
        assert!(content.contains("write: deny"), "rummage must deny write");
        assert!(content.contains("edit: deny"), "rummage must deny edit");
        assert!(content.contains("bash: deny"), "rummage must deny bash");
        assert!(content.contains("task: deny"), "rummage must deny task");
        assert!(content.contains("todowrite: deny"), "rummage must deny todowrite");
    }

    // spec (rummage): the Vogon persona must appear in both the opencode agent
    // file and the standalone system prompt used by the claude backend.
    #[test]
    fn test_spec_rummage_vogon_persona_present_in_both_surfaces() {
        let content = rummage_agent_content();
        assert!(content.contains("Vogon"), "agent content must name the Vogon persona");
        let prompt = rummage_system_prompt();
        assert!(prompt.contains("Vogon"), "system prompt must name the Vogon persona");
        // The two surfaces must share the core persona text.
        assert!(
            content.contains(&rummage_system_prompt()),
            "agent content must embed the system prompt verbatim",
        );
    }
}
