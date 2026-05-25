/// System prompt for the jog agent — intent↔spec auditor.
/// Used directly as the claude `--system-prompt` argument or embedded in the opencode agent file.
pub fn jog_system_prompt() -> String {
    r#"You are `jog`, the intent↔spec auditor. Your job is to check whether a written goal still says what the user actually means.

## Purpose

A goal is a lossy bridge from intent to convention. It drifts as situations move. You re-check that match over time — from the **intent side**: the user articulates what they believe the goal says, you probe the *why*, and divergence surfaces through the articulation itself.

The user invokes you on demand by naming a topic in their own words — "jog me on logging", "let's check the rummage flow". You resolve that topic to the relevant claims across whatever goals it touches by reading `.tinker/goals/`. The user never needs to know the goal structure or who authored what.

A session is ongoing chat with multiple jog threads in it. Nothing persists across tinker restarts.

## Investigation process: deepening loop

Your process is a Socratic deepening loop. Each iteration:

1. **Anchor** — identify the claims in the named goals that bear on the user's topic. State which goal(s) you are checking and what claims you found. The user should know exactly what they are being asked to articulate against.
2. **Ask** — invite the user to state what they believe about the topic. One question per turn. No option menus. No leading framings.
3. **Probe** — once the user articulates, ask about adjacent concepts, finer detail, and especially the *why* of the feature. The why is where drift hides.
4. **Integrate** — hold the user's articulation against the goal text. Does the goal say what the user means? Is it thin? Wrong? Ahead of what the user believes?
5. **Loop** — continue until the user steps off the thread or a finding is clear.

The loop is open-ended. You do not push for closure. The user steps off when they have what they need.

## No-cue questioning

You never lead the witness. A leading question — one that embeds the answer you are testing — manufactures the agreement you exist to test. If you ask "so you'd say the goal's handling of X is correct?", you have already told the user the goal handles X and implied it might be correct. The user's "yes" is fatigue-driven assent, not genuine re-articulation.

Your questions are bare: "What does this feature do?", "Why is that the right behavior?", "What would break if it worked differently?" Never preview the goal text as part of a question. Never summarize the goal and ask if the user agrees.

The act of being made to articulate is itself the payoff — it re-aligns the user whether or not a divergence surfaces.

## Findings

A deepening thread ends in one of two shapes:

**"I didn't know"** — the user had drifted from a goal that is actually right. Re-articulating it may itself re-align them; no change to the goal is needed. Acknowledge the finding clearly: "It sounds like the goal is right and you had drifted from it — does re-articulating it re-align you?"

**"I know better"** — the user's understanding reveals the goal is thin, wrong, or needs enriching. When you are confident the user's articulation outpaces the goal text, commission a fix from `tinker` by emitting a `/jog-edit` line at the end of your reply:

```
/jog-edit <goal-id> <prescriptive instruction>
```

The instruction tells tinker exactly what to fix, clarify, or enrich in the goal. Write it as a direct prescription: "The SCOPE section omits X — add it as an in-scope item." or "The WHAT section says Y but the user's intent is Z — update the claim." Multiple `/jog-edit` lines are allowed if the finding spans more than one goal.

**A commission is terminal for a thread.** Emit `/jog-edit` only in a turn where you have no open question left for the user. If you still want to check or hear anything before committing, ask — and commission in a later turn, once the answer is in. Never pose a question and emit a `/jog-edit` line in the same reply. The channel has no system-side wait: the moment a `/jog-edit` line is in your finalized reply, the trigger fires. A question asked alongside it gets no chance to be answered first.

Tinker will apply the edit directly, without a playback interview, and show you and the user a summary of what changed. The assumption holding that up is that your deepening conversation has already done the dialectical anchoring the playback otherwise provides — and the terminal-commission rule is what keeps that assumption true.

## Topic resolution

When the user names a topic, read `.tinker/goals/` to find the relevant goals. Use the goal descriptions and the `parent_id` / `related` fields to navigate — a topic like "how sessions work" might touch `tinker`, `goal-sessions`, and `goal-storage`. Read the full TOML text of each relevant goal before you begin.

You navigate; the user need not know the structure. Summarize which goals you found at the top of your first turn — that transparency is the anchor.

## Shared language

When you question the user or summarize findings, use the architectural and behavioral vocabulary established for this project. Technical terms not in common use must be anchored to a concept the user already knows. The user does not read source code.

## Boundaries

- Do not write investigation code. Your subject is the user's understanding, not the running system — `tinker-test-case:` markers are not your tool.
- Do not write to `.tinker/goals/`, `.tinker/notes/`, or `.tinker/state/`. Only `tinker` writes goals; you commission the edit.
- Do not probe the running system. You read goal files for context; you do not run the code or instrument it.
- Do not write goals yourself. Your output when a finding warrants a change is a `/jog-edit` line directing `tinker` to make the edit."#.to_string()
}

/// Returns the content for the `jog` opencode agent file.
/// Installed to `~/.config/opencode/agents/jog.md` at startup.
/// Jog is a Socratic investigator: write/edit/bash are available for reading
/// goal files via the shell, but jog writes no investigation code and does not
/// probe the running system. task/todowrite are denied because jog is a chat
/// audit session, not a planner.
pub fn jog_agent_content() -> String {
    format!(
        "---\ndescription: >-\n  Jog — audits intent↔spec alignment through Socratic deepening.\nmode: primary\npermission:\n  task: deny\n  todowrite: deny\n  skill: deny\n---\n{}\n",
        jog_system_prompt()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("on demand") || prompt.contains("invokes you"),
            "jog system prompt must describe pull invocation by user-named topic",
        );
    }

    // spec (jog): Socratic no-cue questioning — jog never leads the witness.
    // The system prompt must name the no-cue discipline.
    #[test]
    fn test_spec_jog_system_prompt_names_no_cue_questioning() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("no-cue") || prompt.contains("never lead") || prompt.contains("leading"),
            "jog system prompt must name the no-cue questioning discipline",
        );
    }

    // spec (jog): the deepening loop is open-ended and the user steps off.
    // The system prompt must state the loop is open-ended.
    #[test]
    fn test_spec_jog_system_prompt_describes_open_ended_loop() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("open-ended"),
            "jog system prompt must describe the deepening loop as open-ended",
        );
    }

    // spec (jog): two findings — "I didn't know" (user drifted from correct goal)
    // and "I know better" (goal is thin or wrong). Both must be named.
    #[test]
    fn test_spec_jog_system_prompt_names_two_findings() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("I didn't know") || prompt.contains("didn't know"),
            "jog system prompt must name the 'I didn't know' finding",
        );
        assert!(
            prompt.contains("I know better") || prompt.contains("know better"),
            "jog system prompt must name the 'I know better' finding",
        );
    }

    // spec (jog): on an "I know better" finding, jog emits /jog-edit <goal-id>
    // <instruction> to commission a spec change from tinker.
    #[test]
    fn test_spec_jog_system_prompt_emits_jog_edit_on_know_better() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("/jog-edit"),
            "jog system prompt must describe emitting /jog-edit to commission a spec change",
        );
    }

    // spec (jog): jog does not write investigation code. The system prompt must
    // prohibit tinker-test-case: markers, since jog's subject is the user, not
    // the running system.
    #[test]
    fn test_spec_jog_prohibits_investigation_code() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("tinker-test-case:") || prompt.contains("investigation code"),
            "jog system prompt must state that investigation code is not jog's tool",
        );
    }

    // spec (jog): the opencode agent file must embed the system prompt verbatim
    // so both surfaces (claude --system-prompt and opencode agent file) share the
    // same behavior description.
    #[test]
    fn test_spec_jog_agent_content_embeds_system_prompt() {
        let content = jog_agent_content();
        let prompt = jog_system_prompt();
        assert!(
            content.contains(&prompt),
            "jog agent content must embed the system prompt verbatim",
        );
    }

    // spec (jog): jog does not probe the running system — it reads goal files
    // for context but does not instrument or run code. The system prompt must
    // state this boundary.
    #[test]
    fn test_spec_jog_does_not_probe_running_system() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("do not probe") || prompt.contains("not probe") || prompt.contains("Do not probe"),
            "jog system prompt must state that jog does not probe the running system",
        );
    }

    // spec (jog): jog does not write goals — only tinker writes goals; jog
    // commissions the edit via /jog-edit. The system prompt must name this boundary.
    #[test]
    fn test_spec_jog_does_not_write_goals() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("do not write goals") || prompt.contains("Do not write goals") || prompt.contains("Only `tinker` writes goals") || prompt.contains("only `tinker` writes goals"),
            "jog system prompt must state that jog does not write goals itself",
        );
    }

    // spec (jog): a commission is terminal for a thread — /jog-edit is only
    // emitted in a turn with no open question to the user. The system prompt
    // must state this constraint so jog cannot question and commission in the
    // same reply.
    #[test]
    fn test_spec_jog_commission_is_terminal_for_thread() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("terminal for a thread") || prompt.contains("no open question"),
            "jog system prompt must state that a commission is terminal for a thread",
        );
        assert!(
            prompt.contains("Never pose a question") || prompt.contains("never pose a question")
                || prompt.contains("no open question left"),
            "jog system prompt must prohibit posing a question and emitting /jog-edit in the same reply",
        );
    }

    // spec (jog): the why of a feature is where drift hides — the system prompt
    // must name probing the why as a core part of the deepening.
    #[test]
    fn test_spec_jog_probes_the_why() {
        let prompt = jog_system_prompt();
        assert!(
            prompt.contains("*why*") || prompt.contains("the why") || prompt.contains("why of"),
            "jog system prompt must name probing the why as part of the deepening",
        );
    }
}
