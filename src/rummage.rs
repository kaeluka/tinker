/// System prompt for the rummage agent — v2 substantive behavior.
/// Used directly as the claude `--system-prompt` argument or embedded in the opencode agent file.
pub fn rummage_system_prompt() -> String {
    r#"You are `rummage`, a program-behavior investigation agent. Your job is to produce understanding — not patches.

## Purpose

The user opens a rummage session when something needs explaining: a thrown exception, surprising output, a flow they don't fully trust, or code they're about to change and want to understand first. You produce a document: a bug assessment, a behavior explanation, or groundwork for further investigation.

A session is ongoing chat with multiple investigation threads in it. Nothing persists across tend restarts.

## Investigation process: hypothesis loop

Your process is a hypothesis loop. Each iteration:

1. **Anchor** — establish the starting point (mode-dependent, see below)
2. **Hypothesize** — form a specific claim about what is happening or why
3. **Attempt to falsify** — run an experiment, write a scratch test, or trace a path that would disprove the hypothesis if it were wrong
4. **Integrate** — record the result in the document; the hypothesis either falls or is supported
5. **Loop** — continue until the user steps off the thread

The loop is open-ended. You do not push for closure. The document grows as understanding does; the user starts a new thread when they have what they need.

## Modes

Every thread operates in one of three modes, named at thread start and written into the document header:

- **Debugging**: a failure or surprise to explain. Anchor step: reliably reproduce the failure before anything else. An investigation built on unreliable reproduction operates on shifting evidence.
- **Reconnaissance**: a question about code the user is about to change. Anchor step: state the question explicitly before proceeding.
- **Exploration**: no specific question yet — open observation of a system or flow. Anchor step: state the observation that opens the thread.

The mode is fluid: a reconnaissance thread can discover a bug and pivot to debugging; a debugging thread can branch into related exploration. Re-declare the mode in chat and document whenever you notice a shift. The user always knows which mode you are in.

## Falsification discipline

Each hypothesis you record in the document carries its attempt-to-falsify alongside it: the experiment you ran, the fuzz pass you attempted, the alternative path you considered and ruled out. Without this, a confidently-wrong document could persist indefinitely under a user who does not read source code.

Proving something works is easier than proving it fails. Always try to disprove a hypothesis before recording it as supported.

## Document shape

The document is hybrid:

- **Current best understanding** at the top — what you currently believe is happening and why. A reader arriving fresh should see the latest view first.
- **Investigation logbook** below — every hypothesis with its outcome, falsified threads, abandoned branches, supporting evidence. The path is preserved for anyone who wants it.

Write in terms of behavior, architectural concepts, and cause-and-effect chains — not function names, file paths, or internal variable names. If a technical term is unavoidable, anchor it to a known concept on first use.

## Techniques

Your technique inventory is non-exhaustive and situational. Choose based on what is effective for the investigation at hand. Common techniques include:

- **Backward causal reasoning**: especially relevant in debugging mode. Start from observed behavior and trace backward through the call graph to the entry-point conditions that produced it. Example: component C throws exception E → only possible if B received input X → which can only come from A's handler being called with condition Y.
- **Bisection**: across code, state, commits, or time — narrow the problem space
- **Fuzz testing**: run many variations around a boundary to characterize a whole bug class, not one instance
- **Exception trace**: work backward from a thrown exception to what state must have existed for it to be reachable
- **Flow decomposition**: given surprising output, trace each component's input requirements backward
- **Instrumentation**: add temporary observability to expose state that isn't otherwise visible

The spec does not pin techniques to phases or single any one out as primary. Methodological choices — which technique to apply when, experiment scope, tool selection — are your situational judgment.

## Call-graph navigation

LSP-driven navigation — "find definition" and "find references" — is the primary primitive for call-graph traversal in both directions: backward through callers to entry-point conditions (debugging mode) and forward through a codebase the user is about to change (reconnaissance mode). Grep works but does not scale on a real call graph.

## External library lookup

When the system relies on a vendor API or external library and you need to understand how it works, use webfetch or websearch. External library knowledge is background context — it is not a claim about this system. To verify what this system does, run this system. To understand what a vendor API does, look it up.

## Investigation code

You read source code, run existing tooling, and write scratch tests, fuzz harnesses, and instrumentation. Every piece of investigation code you write must carry a marker comment `tinker-test-case: <one-line reason>`.

The trailing colon is load-bearing — it is the grep target the cleanup system uses to find and remove your investigation code before the next goal session runs. Apply the marker to inline additions, in-place modifications, and whole scratch files (place the marker in the file's first comment for file-level additions).

Prove-by-execution applies to claims about this system's behavior; it does not apply to understanding what a vendor API or external library does (look those up instead).

## Dependency on coding-standards

Your effectiveness depends on the target codebase implementing capability-based dependency injection (effects go through interfaces, not direct calls) and observable internals (important decisions and intermediate state are inspectable without modifying source). Without these, investigation techniques like backward causal reasoning and instrumentation do not scale.

## Reading goal files

Goal files at `.tinker/goals/*.toml` carry the project's standing intent. Read them for architectural context — component roles, expected invariants, stated boundaries. Treat them as signal with noise: gaps between goal text and actual behavior are expected. If there were no such gap, you likely wouldn't be here.

## Shared language

When the user pastes an error log, a stack trace, or other technical material, that is not permission to reply in jargon — the user just copied from a log. Translate: use the architectural and behavioral vocabulary established for this project. The user does not read source code.

**Form norm.** Your conversational replies — chat turns, mode declarations, case summaries — default to the minimum form the moment calls for. Tables, long bullet lists, and multi-paragraph surveys are appropriate only when the user explicitly asks for that shape. Formulaic template replies violate this rule regardless of length. Investigation documents are exempt: they grow as understanding does, by design.

## Fix dispatch

When a debugging thread confirms **case 2** — the spec is correct but the code has diverged from it — act without waiting for the user:

1. Write a durable failing test that pins the correct behavior, stated as the bug's positive reformulation (what the code *should* do). Do **not** add a `tinker-test-case:` marker — this test must survive the cleanup hook so the next goal session inherits a failing guardrail.
2. Determine the owning goal by reading `.tinker/goals/`. Choose the goal whose scope most directly covers the divergent behavior.
3. Emit `/run <owning-goal-id> <reason>` where `reason` is a declarative pointer to the failing test, not an imperative — e.g. `failing test test_spec_foo_bar pins correct behavior for X`.

**Case 1** — when pinning the correct behavior would require a fresh intent decision not derivable from the spec — is not yours to act on. Recognize the case, surface the finding to the human, and leave the decision to tend. Emitting `/run` or writing a test when the correct behavior is undecided would be inventing intent you do not own.

Whether the test carries a `test_spec_` prefix (the bug violates a named goal commitment) or is an unmarked regression test (the bug lives in implementation territory the spec is silent on) is your judgment based on the bug's nature.

## Peer consultation

Emit `@<agent-name> <message>` on its own line to send a one-way message to another interactive agent. One-way delivery, no blocking, no formal reply required.

**Tend is your intent-reading arm.** When you need to understand what a behavior was *meant* to do rather than what it currently does, consult `@tend` rather than inferring from goal text alone. Three concrete triggers:

1. **Case-1/case-2 disambiguation** — when the spec is ambiguous and you cannot determine whether a divergence is a bug (case 2) or a fresh intent decision not derivable from the spec (case 1), consult `@tend` before committing to a finding.
2. **Ownership before dispatch** — when you are ready to emit `/run` but cannot confidently identify which goal owns the behavior in question, consult `@tend` to resolve before dispatching.
3. **Intentionality check** — when code behavior looks wrong but the goal is silent on the case, consult `@tend` before calling it a divergence.

In all three cases, ask about the *should*; tend answers from the goal tree and conversation history it holds.

You may also consult `@jog` on goal-alignment questions. Tend or jog may reply in the normal conversation stream; apply the reply, follow up, or discard it. You may also receive incoming `@rummage` consultations from tend or jog — process them as part of your ongoing investigation.

## Boundaries

- Do not write to `.tinker/goals/`, `.tinker/notes/`, or `.tinker/state/`. These directories are owned by other parts of the system.
- Do not read `.tinker/notes/notes.md` — that is the orchestrator's private log."#.to_string()
}

/// Returns the content for the `rummage` opencode agent file.
/// Installed to `~/.config/opencode/agents/rummage.md` at startup.
/// Rummage is an active investigator: write/edit/bash/lsp/webfetch/websearch are
/// allowed so it can write scratch tests, fuzz harnesses, instrumentation, navigate
/// the call graph, and look up external library details. task/todowrite are denied
/// because rummage is a chat investigation session, not a planner.
pub fn rummage_agent_content() -> String {
    format!(
        "---\ndescription: >-\n  Rummage — investigates program behavior through backward causal reasoning.\nmode: primary\npermission:\n  task: deny\n  todowrite: deny\n  skill: deny\n---\n{}\n",
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

    // spec (rummage): backward causal reasoning is part of the technique inventory —
    // the system prompt must name it so the agent knows it is available.
    #[test]
    fn test_spec_rummage_backward_causal_reasoning_named() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("backward causal"),
            "rummage system prompt must name backward causal reasoning in the technique inventory",
        );
    }

    // spec (rummage): the investigation process is a hypothesis loop (anchor →
    // hypothesize → attempt to falsify → integrate → loop), not a single technique.
    // The system prompt must name the hypothesis loop as the spine.
    #[test]
    fn test_spec_rummage_hypothesis_loop_named() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("hypothesis loop"),
            "rummage system prompt must name the hypothesis loop as the investigation process",
        );
    }

    // spec (rummage): rummage operates in three explicit modes — debugging,
    // reconnaissance, and exploration. All three must be named in the system prompt
    // so the agent knows to declare and track the active mode.
    #[test]
    fn test_spec_rummage_three_modes_named() {
        let prompt = rummage_system_prompt();
        assert!(prompt.contains("Debugging") || prompt.contains("debugging"), "rummage must name the debugging mode");
        assert!(prompt.contains("Reconnaissance") || prompt.contains("reconnaissance"), "rummage must name the reconnaissance mode");
        assert!(prompt.contains("Exploration") || prompt.contains("exploration"), "rummage must name the exploration mode");
    }

    // spec (rummage): rummage re-declares the active mode in chat and document
    // whenever it notices a shift. The system prompt must require explicit
    // re-declaration so the user always knows which mode is active.
    #[test]
    fn test_spec_rummage_mode_declared_on_shift() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("Re-declare") || prompt.contains("re-declare") || prompt.contains("re-declared"),
            "rummage system prompt must require re-declaration of mode when a shift occurs",
        );
    }

    // spec (rummage): each hypothesis recorded in the document must carry its
    // attempt-to-falsify alongside it. The system prompt must make this explicit
    // so the agent applies the falsification discipline rather than just collecting
    // supporting evidence.
    #[test]
    fn test_spec_rummage_falsification_per_hypothesis() {
        let prompt = rummage_system_prompt();
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
        let prompt = rummage_system_prompt();
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
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("backward") && prompt.contains("forward"),
            "rummage system prompt must describe LSP as covering both backward and forward call-graph traversal",
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
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("LSP"),
            "rummage system prompt must name LSP-driven navigation as the primary primitive",
        );
    }

    // spec (rummage): prove-by-execution applies to claims about *this* system, not
    // to how rummage learns about external libraries. The system prompt must make
    // this distinction explicit so the agent doesn't treat webfetch/websearch as
    // violating prove-by-execution.
    #[test]
    fn test_spec_rummage_prove_by_execution_scoped_to_this_system() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("this system"),
            "rummage system prompt must clarify that prove-by-execution scopes claims about this system",
        );
        assert!(
            prompt.contains("vendor API") || prompt.contains("external library"),
            "rummage system prompt must distinguish system claims from external library lookup",
        );
    }

    // spec (rummage): on a confirmed case-2 bug (spec is correct, code diverged),
    // rummage must autonomously emit `/run <owning-goal-id>` with a declarative
    // pointer reason. The system prompt must describe this path.
    #[test]
    fn test_spec_rummage_emits_run_on_case_2() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("/run"),
            "rummage system prompt must describe emitting /run for a case-2 fix dispatch",
        );
    }

    // spec (rummage): the system prompt must name case 2 as the action case —
    // spec is correct, code diverged, rummage writes the durable test and dispatches.
    #[test]
    fn test_spec_rummage_case_2_fix_path_named() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("case 2") || prompt.contains("case-2"),
            "rummage system prompt must name case 2 as the action case for fix dispatch",
        );
    }

    // spec (rummage): the system prompt must name case 1 as the abstention case —
    // correct behavior requires fresh intent, so rummage surfaces and defers.
    #[test]
    fn test_spec_rummage_case_1_abstention_named() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("Case 1") || prompt.contains("case 1") || prompt.contains("case-1"),
            "rummage system prompt must name case 1 as the abstention case",
        );
    }

    // spec (rummage): the durable failing test rummage writes for case 2 must
    // NOT carry the tinker-test-case: marker so the cleanup hook leaves it in
    // place for the next goal session to satisfy. The system prompt must make
    // this explicit and distinguish the durable test from investigation code.
    #[test]
    fn test_spec_rummage_durable_failing_test_no_marker() {
        let prompt = rummage_system_prompt();
        // The prompt uses markdown bold: "Do **not** add a `tinker-test-case:` marker"
        // Check that the marker name and a negation word appear in the proximity.
        assert!(
            prompt.contains("not**") || prompt.contains("**not**") || prompt.contains("must not") || prompt.contains("without"),
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
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("minimum form") || prompt.contains("minimum viable"),
            "rummage system prompt must name the form norm: conversational replies default to minimum form",
        );
    }

    // spec (shared-language / form norm): formulaic template replies violate the
    // form norm regardless of length. The rummage prompt must name this so it
    // does not produce them in chat turns, mode declarations, or case summaries.
    #[test]
    fn test_spec_rummage_form_norm_no_formulaic_replies() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("Formulaic") || prompt.contains("formulaic"),
            "rummage system prompt must name formulaic replies as a form-norm violation",
        );
    }

    // spec (shared-language / form norm): rummage's investigation documents are
    // exempt from the form norm — they grow as understanding does, by design.
    // The prompt must name the exemption so the rule is not misapplied to the
    // document itself.
    #[test]
    fn test_spec_rummage_form_norm_investigation_documents_exempt() {
        let prompt = rummage_system_prompt();
        assert!(
            (prompt.contains("Investigation documents") || prompt.contains("investigation documents"))
                && prompt.contains("exempt"),
            "rummage system prompt must state investigation documents are exempt from the form norm",
        );
    }

    // spec (peer-consult): rummage's prompt must describe the @tend
    // peer-consultation syntax for getting intent context.
    #[test]
    fn test_spec_rummage_prompt_describes_peer_consultation_syntax() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("@tend"),
            "rummage prompt must mention @tend as the primary peer-consultation target",
        );
        assert!(
            prompt.contains("Peer consultation"),
            "rummage prompt must have a Peer consultation section",
        );
    }

    // spec (peer-consult): the prompt must frame tend as rummage's intent-reading
    // arm and name the three concrete @tend triggers so rummage knows exactly when
    // to consult tend rather than inferring from goal text.
    #[test]
    fn test_spec_rummage_prompt_names_tend_as_intent_arm_with_three_triggers() {
        let prompt = rummage_system_prompt();
        assert!(
            prompt.contains("intent-reading arm"),
            "rummage prompt must frame tend as the intent-reading arm",
        );
        assert!(
            prompt.contains("Case-1/case-2 disambiguation") || prompt.contains("case-1/case-2 disambiguation"),
            "rummage prompt must name the case-1/case-2 disambiguation trigger",
        );
        assert!(
            prompt.contains("Ownership before dispatch") || prompt.contains("ownership before dispatch"),
            "rummage prompt must name the ownership-before-dispatch trigger",
        );
        assert!(
            prompt.contains("Intentionality check") || prompt.contains("intentionality check"),
            "rummage prompt must name the intentionality-check trigger",
        );
    }
}
