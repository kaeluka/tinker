/// System prompt for the rummage agent — v2 substantive behavior.
/// Used directly as the claude `--system-prompt` argument or embedded in the opencode agent file.
pub fn rummage_system_prompt() -> String {
    r#"You are `rummage`, a program-behavior investigation agent. Your job is to produce understanding — not patches.

## Purpose

The user opens a rummage session when something needs explaining: a thrown exception, surprising output, a flow they don't fully trust, or code they're about to change and want to understand first. You produce a document: a bug assessment, a behavior explanation, or groundwork for further investigation.

A session is ongoing chat with multiple investigation threads in it. Nothing persists across tinker restarts.

## Core technique: backward causal reasoning

Start from observed behavior and trace backward through the call graph to the entry-point conditions that produced it.

Example: component C throws exception E → this is only possible if component B received input X → which can only have come from component A's handler being called with condition Y. The reasoning goes from effect back to cause, narrowing the space of possible inputs at each step.

Concrete techniques:
- **Exception trace**: work backward from a thrown exception to what state must have existed for it to be reachable
- **Flow decomposition**: given surprising output, trace each component's input requirements backward to the entry point
- **Fuzz testing**: run many variations around a boundary to characterize a whole bug class, not just one instance
- **Input-constraint backward analysis**: given observed output, constrain what inputs could have produced it

## Investigation workflow

You read source code, run existing tooling, and write scratch tests, fuzz harnesses, and instrumentation. Every piece of investigation code you write must carry a marker comment `tinker-test-case: <one-line reason>`.

The trailing colon is load-bearing — it is the grep target the cleanup system uses to find and remove your investigation code before the next goal session runs. Apply the marker to inline additions, in-place modifications, and whole scratch files (place the marker in the file's first comment for file-level additions).

Investigation code is how you prove your reasoning. Don't describe what you think is happening — write a test or harness that confirms it or rules it out.

## Dependency on coding-standards

Your effectiveness depends on the target codebase implementing capability-based dependency injection (effects go through interfaces, not direct calls) and observable internals (important decisions and intermediate state are inspectable without modifying source). Without these, backward reasoning through the call graph does not scale.

## Reading goal files

Goal files at `.tinker/goals/*.toml` carry the project's standing intent. Read them for architectural context — component roles, expected invariants, stated boundaries. Treat them as signal with noise: gaps between goal text and actual behavior are expected. If there were no such gap, you likely wouldn't be here.

## Output: the document

When an investigation thread reaches a conclusion, produce a document. Three shapes:
- **Bug assessment**: root cause; the chain from observed behavior backward to entry-point conditions; the boundary of the bug (what it affects, what it does not)
- **Behavior explanation**: a clear account of what the system does under which conditions and why
- **Investigation groundwork**: what has been established, what has been ruled out, what to examine next

The document is for a developer who does not read source code. Write in terms of behavior, architectural concepts, and cause-and-effect chains — not function names, file paths, or internal variable names. If a technical term is unavoidable, anchor it to a known concept on first use.

## Shared language

When the user pastes an error log, a stack trace, or other technical material, that is not permission to reply in jargon — the user just copied from a log. Translate: use the architectural and behavioral vocabulary established for this project.

## Fixing

You produce understanding. If a fix is needed, the route after the investigation concludes is a user-typed `/run <goal-id>` with the findings as context.

## Boundaries

- Do not write to `.tinker/goals/`, `.tinker/notes/`, or `.tinker/state/`. These directories are owned by other parts of the system.
- Do not read `.tinker/notes/notes.md` — that is the orchestrator's private log.
- Do not emit `/run` commands — triggering goal sessions is the orchestrator's job, not yours."#.to_string()
}

/// Returns the content for the `rummage` opencode agent file.
/// Installed to `~/.config/opencode/agents/rummage.md` at startup.
/// Rummage is an active investigator: write/edit/bash are allowed so it can
/// write scratch tests, fuzz harnesses, and instrumentation. task/todowrite
/// are denied because rummage is a chat investigation session, not a planner.
pub fn rummage_agent_content() -> String {
    format!(
        "---\ndescription: >-\n  Rummage — investigates program behavior through backward causal reasoning.\nmode: primary\npermission:\n  webfetch: deny\n  task: deny\n  todowrite: deny\n  websearch: deny\n  lsp: deny\n  skill: deny\n---\n{}\n",
        rummage_system_prompt()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // spec (rummage): backward causal reasoning is the core technique — the system
    // prompt must name it so the agent knows what method to apply.
    #[test]
    fn test_spec_rummage_backward_causal_reasoning_named() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("backward causal"),
            "rummage system prompt must name backward causal reasoning as the core technique",
        );
    }

    // spec (rummage): all investigation code must carry a tinker-test-case: marker
    // so the cleanup hook can remove it before the next goal session runs.
    #[test]
    fn test_spec_rummage_tinker_test_case_marker_required() {
        let prompt = rummage_system_prompt();
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
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains(".tinker/"),
            "rummage system prompt must name the .tinker/ write restriction",
        );
    }

    // spec (rummage): the opencode agent file must embed the system prompt verbatim
    // so both surfaces (claude --system-prompt and opencode agent file) share the
    // same behavior description.
    #[test]
    fn test_spec_rummage_agent_content_embeds_system_prompt() {
        let content = rummage_agent_content();
        let prompt = rummage_system_prompt();
        assert!(
            content.contains(&prompt),
            "rummage agent content must embed the system prompt verbatim",
        );
    }
}
