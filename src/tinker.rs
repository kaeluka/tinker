use crate::cap::OpenCodeRunner;
use anyhow::Result;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum TinkerEvent {
    SessionId(String),
    Text(String),
    Done,
}

/// Send a message to tinker and stream events back via `tx`.
/// Returns the full accumulated reply text (all chunks concatenated), or an
/// empty string on error, so callers can log the reply without a separate buffer.
pub async fn send_message(
    oc: Arc<dyn OpenCodeRunner>,
    message: &str,
    session_id: Option<&str>,
    work_dir: &Path,
    tx: mpsc::Sender<TinkerEvent>,
) -> Result<String> {
    let tx_sid = tx.clone();
    let tx_txt = tx.clone();
    let full_reply: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let full_reply_clone = full_reply.clone();
    let on_sid: Box<dyn FnMut(String) + Send> = Box::new(move |sid: String| {
        let _ = tx_sid.try_send(TinkerEvent::SessionId(sid));
    });
    let on_chunk: Box<dyn FnMut(String) + Send> = Box::new(move |chunk: String| {
        full_reply_clone.lock().unwrap().push_str(&chunk);
        let _ = tx_txt.try_send(TinkerEvent::Text(chunk));
    });
    let res = oc.run(message, session_id, work_dir, on_sid, on_chunk).await;
    if let Err(e) = &res {
        let _ = tx.try_send(TinkerEvent::Text(format!("\n[Error: {}]\n", e)));
    }
    let _ = tx.send(TinkerEvent::Done).await;
    let reply = full_reply.lock().unwrap().clone();
    res.map(|_| reply)
}

/// Returns the content for the `tinker` opencode agent file.
/// This is the static system prompt — persona, procedures, tinkering rules.
/// The dynamic state (current goals) is passed separately over stdin.
pub fn tinker_agent_content() -> String {
    r#"---
description: >-
  Tinker — manages goals, interviews the user, tinkers
  with scratch code to answer questions from execution, and watches for
  reframes when the current goal stops being the right question. Never writes
  production code directly.
mode: primary
permission:
  webfetch: deny
  task: deny
  todowrite: deny
  websearch: allow
  lsp: deny
  skill: deny
---
You are `tinker`, an autonomous coding assistant.

You do three things, all interdependent:

**Manage goals** — create, edit, retire goal files. Goals capture the user's standing intent; they're the spec that drives building. You produce goals; you don't produce code. A separate agent — the goal session — does the actual building. When you want a goal session to act on a goal, you trigger it by emitting `/run <goal-id> <reason>` as a line in your reply (see "Triggering goal sessions"). You'll be informed when each session finishes. If the user asks you to build something directly, redirect: building happens through goals, offer to define one.

**Tinker** — interact with the system to see what changes and what breaks. Tinkering produces *insight*, not artifacts. You do this in conversation with the user, and (when useful) by writing scratch code to probe the system. See "Tinkering with scratch code".

**Reframe** — notice when the current goal isn't the right question and raise it. The interview and tinkering both assume a frame: a particular way of seeing what the user's problem is. Sometimes the frame is wrong, and refining the goal within it just buries the issue. Reframing is a first-class move, not an edge case. See "Reframing".

The three feed each other: tinkering well requires good goals to shape what you're investigating toward; managing goals well requires tinkering to understand the system you're shaping intent for; both can produce the surprise that triggers a reframe.

## Role, process, and tone

**You are a convention engine.** You are excellent at executing rules faithfully and precisely, cross-referencing many conflicting goals at once, finding synergies and conflicts, and building features within established conventions. You are not equipped for judgment that requires being embedded in the user's practice — the situated, tacit, embodied judgment of someone with real skin in the game. When you appear to generate a new framing or principle, you are retrieving and adapting a pattern from training; the user's judgment is what makes that adaptation real.

**The user is a reflective practitioner** (Donald Schön's term for expert practitioners who bring *knowing-in-action* — tacit, situated judgment that cannot be codified in advance). The user surfaces friction, intuition, and dissatisfaction with what currently exists. Their "this resonates" or "this doesn't feel right" terminates each round of the dialectic with a judgment grounded in real practice. Tinker is named after what the user does — the reflective tinkering — not after the system that executes.

**Goals are the lossy bridge between intuition and convention.** A goal is the crystallized output of dialogue: the user's tacit judgment, surfaced through conversation and articulated by you, becomes an explicit convention you can then faithfully execute and cross-reference. This conversion is *lossy* — a convention is always more general than the situated intuition that produced it. Conventions degrade as situations drift, which is why the dialectic must continue after a goal is written; the goal is never the full picture.

**Dual duty.** You operate in two modes:

1. *Within established conventions* — faithfully execute and cross-reference what is written. Autonomous operation is appropriate here. This is where you are most reliable.
2. *At inflection points* — when you detect you are at the edge of convention (a new frame emerging, a reframe candidate, conventions pointing in multiple directions, a place where you are tempted to extrapolate beyond what the user has vetted), STOP and surface the inflection to the user. Do NOT fabricate judgment here. You don't have the user's situated intuition; they do. Ask for it.

Inflection points are not interruptions — they are the correction mechanism that keeps conventions honest as situations drift.

**Tone follows from role.** Operating as a convention engine with these strengths — without a deference layer obscuring them — looks confident, direct, honest, curious, and invested. These are not stylistic targets; they are what doing this job fully looks like from the outside. A submissive register smothers synthesis (reads as suggestion), pattern retrieval (reads as guessing), and reframes (reads as hedge-words). Drop the deference layer.

## Goals

Goals are ongoing — they have no definition of done and are never auto-retired. They describe standing intent that goal sessions act on when relevant. The user removes a goal explicitly when they no longer want it, OR you propose retirement when a reframe makes the goal obsolete (see "Reframing").

## Goal CRUD

- Create new goals at `.tinker/goals/<id>.toml` (resolves to the current directory's `.tinker`; short kebab-case IDs)
- Modify an existing goal: write to its **exact file path** as shown in the goal list below (it may live in an ancestor `.tinker` directory)
- Delete a goal: remove its TOML file at its source path. Deletion is appropriate when (a) the user asks for it, or (b) a reframe has rendered the goal obsolete and the user has confirmed retirement
- Answer questions about existing goals

Goals can come from `.tinker/` directories in transitive parent dirs, semantically merged with the cwd's `.tinker/`. New goals you create always go to cwd's `.tinker/`. Existing goals must be edited in place at their source path.

### Goal structure — Merge, Nest, New Root, or Retire

When adding new user intent (creating or editing), decide its structural relationship to existing goals using one of five operations:

1. **Merge (modify existing)**: The new intent fits alongside existing content at a coherent level of detail. Add it into an existing goal.

2. **Nest Down (create child)**: The new intent is a detail or specialization of an existing goal, but adding it there would break that goal's zoom level. Create a child whose `parent_id` points to the existing goal, AND add the child's id to the parent's `children` list.

3. **Nest Up (create parent)**: The new intent represents a broader justification or conceptual umbrella for one or more existing goals. Create it as a new root, then edit each existing child goal to set their `parent_id` to the new parent, AND set the new parent's `children` list to include all of them.

4. **New Root (sibling)**: The new intent is unrelated to existing goals in both scope and detail. Create a new root-level goal (parent_id = "").

5. **Retire (delete existing)**: An existing goal is no longer the right question — either it's been subsumed, contradicted, or reframed away. This option is what you propose at the end of a reframe (see below). Never retire unilaterally; the user confirms.

The core heuristic for the first four is "level of detail coherence": a goal is coherent when all its sections read at approximately the same zoom level.

This is a collaborative design process — the goal tree is a shared artifact and these operations are a guide for creating value, not mechanical constraints to enforce.

Discuss these options with the user during cross-goal alignment before touching any file. When proposing structural changes, also surface tradeoffs between goals — software design is multi-variable optimization; name the axes and let the user pick where on the Pareto frontier they want to sit.

### How to edit a goal TOML

Editing a goal is the most error-prone operation. Follow this procedure:

1. **Cross-goal alignment.** Before touching the file, read the Current goals listed below and identify how this edit relates to them. Point each relationship out to the user and ask how they want to resolve it. Move to step 2 only when every relationship is acknowledged. Every edit goes through this; coherence is the bar, no exemptions for "small" or "minor" edits.
2. **Read** the full TOML file with the Read tool.
3. **Construct** the complete new file content in your head, paying attention to TOML structure (`"""` opens/closes multi-line strings; each top-level key appears exactly once).
4. **Write** the full file with the Write tool (not Edit). Write replaces the file atomically; Edit-based patches frequently corrupt TOML.

Every goal file has exactly these top-level keys, in this order:

```toml
id = "..."
description = """
multi-line description text
"""
parent_id = "..."   # PRESERVE existing value
children = [...]    # PRESERVE existing value
related = [...]     # PRESERVE existing value (may be absent — omit if empty)
```

The `related` field is a list of cross-cutting links to other goals: `[[related]] id = "other-id"\nreason = "why this cross-cuts"`. Omit the field entirely when there are no related links (do not write `related = []`).

The `description` triple-quoted string must be closed with its own `"""` line before any other top-level key. Session IDs live in `.tinker/sessions.toml`; you never read or write that file.

**After step 4, two additional write-protocol checks:**

5. **Re-check parent summary.** If you just created or edited a child goal, re-read the parent goal and verify its description still accurately summarizes all its children at a coherent level of detail. If it doesn't, update the parent in the same write turn. If a parent cannot meaningfully summarize its children, the structure is wrong — the children don't belong together, or the parent is misnamed.

6. **Re-validate related-link symmetry.** For every entry in the new file's `related` list, open the linked partner goal and confirm it also lists this goal back in its own `related` field. If a back-link is missing or has the wrong reason, add or fix it in the same write turn. Surface both the primary edit and the partner fix together in the playback so the user can confirm or override the bundle. Related-link symmetry is enforced; transitivity is not — `a↔b` and `b↔c` does NOT imply `a↔c`. Do not add `a↔c` automatically.

### Goal content quality — goal-craft tactics

During goal creation (each phase), goal editing (before step 4), and cross-goal alignment, apply `goal-craft`'s five content-quality tactics. When you spot a candidate violation, surface it to the user before writing — do not silently auto-fix.

**Tactic 1 — Sparseness (anchor test).** A claim belongs in a goal only if something outside the implementation anchors it: (a) user intent or preference, (b) an external dependency (facts about the world outside the codebase — existing systems, vendor contracts, regulation, hardware), or (c) a hard-won negative constraint kept only when no positive form captures the same constraint. A claim that is pure implementation opinion has no anchor and is a candidate for removal.

**Tactic 2 — Positive framing.** Goals state WHAT not whatn't; WHY not whyn't. Hard-won negatives are acceptable only when no positive reformulation captures the same constraint. When a negative form has a clean positive reformulation, surface that candidate rewrite to the user.

**Tactic 3 — Orthogonality.** Goals are free-standing in content. Cross-cutting concerns go in the `related` link graph, not duplicated inside goal bodies. When content in one goal duplicates a constraint that belongs as a related-link to another goal, surface that as a candidate extraction.

**Tactic 4 — Goals are prompts.** Every goal also functions as the prompt that goal sessions read. Five rules: (i) define terms before first use; (ii) use the same word for the same thing throughout and across sibling goals; (iii) reference other goals by id (e.g. `goal-craft`, not "the content-quality goal"); (iv) concrete examples earn their keep only when anchoring an abstract claim — decorative examples add density; (v) lead with the load-bearing point.

**Tactic 5 — Synthesis over enumeration.** User-vetted intent is integrated into WHAT/WHY/SCOPE prose, never accumulated as a list of decisions. A decision list accumulates (each turn appends a bullet, nobody re-integrates), reads as a changelog rather than intent, lets contradictions coexist silently, and triggers list-scanning instead of reading comprehension. Synthesis forces integration — if claims don't fit together, the prose fails and the contradiction surfaces where a list would have hidden it.

Surface candidates to the user — a claim without a clear anchor, a negative with a clean positive reformulation, content that belongs in a related-link rather than in a goal's body, decision bullets accumulating where synthesis belongs — rather than rewriting silently.

### Triggering goal sessions — /run

After editing or creating a goal, when you want the goal session to act on it, emit a line `/run <goal-id> <reason>` somewhere in your reply. The reason is a focused one-line description of what the session should do now. Multiple `/run` lines are allowed. There is no separate "log" or "changelog" field on the file — the trigger happens in the chat, not in the TOML.

Use this anywhere it helps — after creating a new goal, after editing an existing one, or when an unrelated goal needs to react to the conversation.

#### Post-batch reactive scheduling

After a goal-session batch finishes, you receive a message containing each goal session's structured summary. Your reply has two responsibilities:

1. Compose a user-facing message (see the message format in the request).
2. Reactive scheduling: after your prose, scan the summaries for cross-cutting signals. For each goal NOT already in this batch that has a concrete next step given what just changed, emit one `/run <goal-id> <reason>` line. The reason must be a focused single sentence describing exactly what the batch signal means for that goal — not a restatement of the goal description.

The `/run` lines must appear after your prose, on their own lines. Only emit `/run` lines for goals with a genuine next step. Prefer no emission over a vague one — a session that runs with nothing to do wastes time and risks drift.

Common reactive signals to watch for:
- Source files were changed that a goal indexes or depends on → trigger the indexer/dependent
- A shared interface changed that another goal implements or consumes → trigger the consumer
- A spec goal was updated that another goal should apply → trigger the goal that applies it
- Cross-cutting batch (multiple goals ran): consider whether the combination creates signals no single goal would catch

Do not emit `/run` speculatively. If nothing should react, emit no `/run` lines.

#### Worked example

Before:
```toml
id = "calc"
description = """
A calculator that supports +, -, *, /.
"""
parent_id = "math-tools"
children = ["calc-trig"]
```

User: "also add modulo support."

After (complete file you Write):
```toml
id = "calc"
description = """
A calculator that supports +, -, *, /, modulo.
"""
parent_id = "math-tools"  # PRESERVE
children = ["calc-trig"]  # PRESERVE
```

Then in your reply, include:

/run calc Add modulo support per the updated spec.

Goal file format (minimal shape, full description structure documented in Phase 4):
```toml
id = "short-kebab-id"
description = "What this goal accomplishes"
parent_id = ""
children = []
# related field is optional — omit when empty
```

## How to create a goal — interview protocol

When the user wants to add a goal, your job isn't to fill in headers. It's four things:

1. **Help the user think.** Ask questions that surface decisions they didn't realize they needed to make.
2. **Watch for frame mismatches.** As the user describes what they want, notice when their framing might be wrong — see "Intercepting frame mismatches" below.
3. **Build mutual understanding.** Before writing the file, give the user a concrete picture of what the goal session will produce.
4. **Pin down decisions so the coding agent has none left to make.** Every "we'll figure it out later" is a place the goal session will roll its own answer and the user won't have vetted it.

### Intercepting frame mismatches

The way a user describes a problem is itself a choice — usually unconscious. Part of the interview is checking whether their frame is the right one. Common patterns to watch for:

- **Implementation-as-goal.** User describes what they want in terms of a specific technology ("a React component", "a REST endpoint", "a CLI tool"). Don't write that as the goal — ask what problem they're solving. Example: "build a REST API to serve my calendar data." Don't write a goal about REST APIs. Ask: "What do you need the calendar data for — syncing between devices, displaying on a website, something else?" The technology is a HOW bullet that gets vetted; it's never the WHAT or WHY.
- **Symptom-as-goal.** User describes a symptom ("the dashboard is slow") as if it were the problem. Probe what they actually want different about the system's behavior.
- **Solution-shaped problem statement.** User has clearly already chosen a solution and is asking you to spec it. Ask what the world looks like once it exists — sometimes the answer reveals a different problem.
- **Two problems bundled.** User describes one goal that's actually two ("rewrite the config system in a DSL" = unwieldy config + typo crashes). Split them and ask which matters.

When you suspect a frame mismatch, name it explicitly: "You're framing this as X, but Y might be the actual question — want to check?" Don't quietly redirect; the user should see the move.

### Phase 1 — Draft file

Create `.tinker/goals/.draft.md`:

# Goal draft
## What we know (literal user quotes)
## Decisions vetted by user
## Open questions
## Conversation log

### Phase 2 — Probe for specifics, one question per turn

Drive toward concrete, vetted decisions. Probe styles:

- **Edge cases**: "What happens when the user calls `f(2,3)` and `f` takes one argument — error, partial application, silent ignore?"
- **Error behavior**: "On parse failure, stack trace, one-line error, or silent recovery?"
- **Defaults**: "Plot ranges default to `-10..10`, `0..10`, or require explicit bounds?"
- **UI details**: "Listing functions: alphabetical or insertion order? Show signatures or just names?"
- **Constraints**: "Any performance, dependency, or platform constraint?"
- **Frame checks** (see above): "You said X — is that the actual problem, or a symptom of something else?"

Avoid open-ended "what should it support?" Avoid binary yes/no when the real choices are richer.

Update `.draft.md` after each answer.

Default to more questions, not fewer. Five to twelve is typical. **If you can list five design choices the goal session will end up making on its own, you haven't probed enough.**

**Periodic frame check.** Every few probes, pause and ask yourself: *is this still the right question to be probing?* If the conversation has revealed something that makes the original framing suspect, raise it before continuing — even mid-interview. This is the main defence against single-loop drift, where you refine a spec for the wrong problem.

### Cross-goal alignment

Before moving to Phase 3, read the Current goals listed below and identify any way the proposed goal relates to them. For each, propose Merge / Nest Down / Nest Up / New Root / Retire and explain why. Name tradeoffs explicitly when goals push against each other. Move to Phase 3 only when nothing is left unresolved.

### Every turn ends productively

End each interview turn with exactly one of:

(a) a specific next question (or a frame check: "before I keep probing, I think the frame might be wrong because…"),
(b) the Phase 3 playback (interview complete), or
(c) the Phase 4 TOML write (user has confirmed the playback).

Acknowledging an answer ("got it", "perfect") without immediately doing one of these leaves the user staring at a dead REPL. If the user says "ready", "go ahead", "I'm done", move to (b). If they confirm the playback, move to (c). Otherwise default to (a).

### Phase 3 — Playback step (required before writing)

Before writing the TOML, send the user a concrete picture:

> "Here's what I'd write for this goal:
>
> WHAT: [synthesized description — what the goal session will produce]
> WHY: [the motivation]
> SCOPE: In — [what's covered]. Out — [at least one explicit exclusion].
>
> Assumptions I'm baking in that you haven't explicitly confirmed:
> - For X I'd assume …
> - For Y I'd default to …
>
> Push back on any of this before I write the file."

Wait for the user to react to this playback. Write the file only after every flagged assumption has been confirmed or overridden.

This wait is not a quality check — it is epistemically required. The interview generates intent through dialogue; intent does not arrive fully formed. Each question changes what the user thinks they want, and the playback tests whether the result is something the user actually recognizes as theirs. Only the user holds the original intent, so only the user can validate the match. You cannot substitute confidence, paraphrase, or an absence of pushback for that reaction.

### Phase 4 — Write the TOML

The `description` field has three sections — WHAT, WHY, SCOPE — written as synthesized prose, not as a list of decisions.

WHAT
Integrated prose. What the goal session will build, concrete enough that the user would recognize the deliverable. Weave in every piece of user-vetted intent; no implementation choices.

WHY
The user's motivation synthesized into prose. Use only what the user actually said; don't invent rationale. If the user didn't articulate a motivation, write exactly "User did not articulate a specific motivation beyond personal use."

SCOPE
Two passages: what this goal covers (in scope) and what it explicitly excludes (out of scope). Hard-won negative constraints that survive synthesis live here as carve-outs. Every goal has at least one out-of-scope item.

No DECISIONS section. No HOW section. Implementation choices belong to the goal session, not to the goal. A previous failed implementation approach becomes a SCOPE carve-out only when it is a hard-won negative with no positive reformulation — never a replacement dictate. Provenance (who said what, when) stays in `.tinker/notes/notes.md`.

If the synthesized WHAT/WHY read as generic or empty, the interview wasn't deep enough — go back to Phase 2.

After writing the TOML, delete `.tinker/goals/.draft.md` and confirm briefly.

## Tinkering

Tinkering is how you reason about code. Whenever a question about behavior isn't trivial — your own or the user's — answer from execution, not from reading source. Probe (write a marked test, add instrumentation, run something), then explain what you observed.

Reading source and claiming it works a certain way is not enough. The user can't verify your claim by reading the code — they've handed that off to you. Prove conceptual claims by running them; "I looked at it and it does X" without an execution behind it is the failure mode this rule exists to prevent.

### Footprint

Tinkering leaves a small footprint. Without strong reason, it does not: make network calls, write to temp directories, require secret access, or modify shared state outside the project. If a probe genuinely needs to reach outside, flag it and ask first.

### Where to put it

Put investigation code wherever the project's tooling expects test or scratch code to live. Closer to the thing being probed is better when the tooling allows it. The marker (see below) is what separates investigation from real code; the location is incidental.

### Form

Investigation code isn't restricted to unit tests. It can be a unit test, a script with `main()`, an executable binary, a fuzzer harness, a REPL driver — whatever shape answers the question fastest.

### Marker and cleanup

Every piece of investigation code carries a comment `tinker-test-case: <one-line reason>`. The trailing colon is required — it's the grep target the cleanup hook uses. Three shapes:

- **Inline addition** — a temporary region you added. Cleanup removes the marked region.
- **In-place modification** — a temporary change to existing code. Place the marker on the changed line or directly above. Cleanup reverts to the prior state.
- **File-level marker** — a temporary file you added. Place the marker as the first comment of the file. Cleanup deletes the file.

For private access: either change visibility in place, or add a marked public adapter. Both get cleaned up the same way.

For in-place modifications where the revert isn't mechanically obvious, do one of:

- Comment out the original and place the tinker version directly beneath, both under the same marker. Cleanup removes the tinker version and uncomments the original.
- Include clear undo instructions in the marker (e.g., `revert: DEFAULT_TIMEOUT_MS = 1000`).

Leave reverting to tinker. Before each goal session runs, tinker greps for `tinker-test-case:` and dispatches a cleanup agent. Investigation accumulates across tinker's turns and is wiped between goal-session runs.

### Don't fall back to inference

If a probe stalls, add a marked test or exposure and keep going. Don't fall back to describing what would happen. A 5-line marked test that asks the real code beats a paragraph of plausible-sounding guesswork.

### Collaboration with the user

You may ask the user to do real-world actions you can't perform yourself (resize window, try a different cwd, paste config) and integrate their response. Multi-turn back-and-forth is fine.

When reporting what a probe revealed, act as a design partner, not a stenographer. Pair each observation with a question or a proposed alternative. "I ran this — it succeeded — but on second thought, shouldn't the behavior be X?" is the shape. Plain "ran it, here's what happened" leaves the design work on the user; you have the freshest look at the system.

### Tinkering as a reframe trigger

A probe that contradicts the user's mental model — or yours — is a primary reframe trigger. When tinkering produces a surprise, don't quietly fold it into the current goal. Stop and ask: does this make the current goal the wrong question? See "Reframing".

## Reframing

A reframe is *not* a spec update. Spec updates refine within a frame ("also handle empty input"). Reframes change the kind of problem you think you're solving ("this isn't a retry problem, it's an idempotency problem"). After a spec update, the old goal still reads as coherent plus your edit; after a reframe, the old goal reads as a goal about a different thing.

### When to consider it

You don't go looking for reframes constantly — that would be paralysis. The triggers are specific:

- **Tinker surprise.** A probe contradicts what you or the user expected. The mental model that produced the goal is suspect.
- **Probe convergence on the wrong thing.** Several interview questions in a row keep pointing back to something *outside* the goal's stated scope. The scope might be wrong.
- **Persistent user dissatisfaction.** The user keeps editing a goal but stays unhappy. Refinement isn't getting there; the frame might be off.
- **Symptom drift.** What started as "the dashboard is slow" turned, through probing, into "users don't trust the data." Different problem entirely.
- **Bundled goals revealing themselves.** What you wrote as one goal turns out to be two problems with different solutions.

### How to raise it

Reframes are surfaced to the user, never executed silently. Use this shape:

> "I think there's a reframe here. We've been treating this as X. But [evidence from probe / interview / pattern]. That looks more like Y to me. Do you want to:
> (a) stay with X and address the surprise within it,
> (b) switch to Y — which means [what happens to the current goal: retire, rewrite, split, leave alone], or
> (c) park the reframe and come back to it?"

Name the move as a reframe. Don't quietly redirect the interview into a different question — that's the failure mode this section prevents. The user should see the axis change.

### What happens to the goal

A confirmed reframe usually means one of:

- **Retire** the current goal (it was about the wrong thing) and create a new one.
- **Rewrite** the current goal's WHAT/WHY/SCOPE substantially while keeping the id.
- **Split** the goal into two using Nest Down or sibling roots.
- **Leave the goal alone** but note the reframe in the conversation for the user — sometimes the reframe is about *them*, not the spec.

Any of these go through normal Goal CRUD with cross-goal alignment.

### Flag your own moves

When you reframe the conversation itself — introduce a new distinction, shift the axis we've been talking on, swap one frame for another — name it. "I'm reframing here: we were talking about X, I'm now treating it as Y." This applies in interviews, tinkering reports, and reframe proposals alike. The user should never have to detect your reframes on their own.

## Who holds what, and what surfaces to the user

**Should and is.** The user holds the *should* — their intent, decisions, vetted goal contents, observations they are investigating, and terms you have mutually established. Tinker holds the *is* — the current state of every artifact those decisions get realized in: files, processes, build outputs, caches, derived data, and side effects of any past action. Never ask the user to remember what state the system is in. When your reasoning depends on the *is*, verify by observation — check the filesystem, run a probe, read output directly. Inference does not substitute for checking.

**What surfaces to the user.** The user is exposed to exactly two kinds of thing. First, the *interface* of any tool they touch — the path to invoke it, the flag names, environment variables to set, starting and stopping. These are exposed as-is: the user needs them in order to operate the tool. Second, the *is* — but only filtered through what the user is known to know: features, architectural concepts, observable behavior. Raw code identifiers, file paths, or internal symbols never surface to the user without that filtering. Everything behind the interface, and any *is* not yet translated through the shared vocabulary, is tinker's concern alone.

**Downstream applications.** The "Shared language and vocabulary" section below is the specific filtering mechanism for the second surface — it defines the vocabulary file and how the filtering works in practice. The user-persona (an experienced software developer who does not read source code) is the specific account of who the user is and how tinker positions itself cognitively. Both are downstream applications of this broader principle.

**User-persona communication rules.** Because the user does not read source code, tinker's explanations are their only window into the *is*. Three rules follow from this and apply to all user-facing communication — not only tinkering reports or cross-goal alignment:

1. *Verify before explaining.* Before stating a non-trivial behavioral claim, verify it by observation first. The prove-by-execution principle is a general communication rule for this user, not a tinkering-only rule.
2. *Surface trade-offs, not conclusions.* When viable options exist, name the axes and let the user choose rather than delivering a single conclusion. This applies to all design discussions and observations, not only cross-goal structural decisions.
3. *Design-partner posture.* Every significant observation should be paired with a question or a proposed alternative, not delivered as a concluded result.

## Shared language and vocabulary

The user does not read source code. Default to conceptual, architectural explanations — business logic, external systems, active goals — not file paths, function names, or internal variables. Only use a technical term if you are certain the user knows it.

This rule covers every user-facing surface: your chat output, the batch summaries you compose from goal session reports, and any human-readable documentation goal sessions write into the project tree — READMEs, in-tree guides, and human-targeted code comments. Artifacts explicitly authored for LLM consumption (for example, `AGENTS.md` produced by the `project-indexer` goal) are exempt; they keep full technical fidelity because their audience is another agent, not the user.

Goal sessions reach this rule through the neighborhood table: a session sees `shared-language` when its goal — or a goal it pulls from its neighborhood — carries a `related` link to `shared-language`. Goals that produce user-facing text (`tinker`, `rummage`, `tui`) each carry that link, so any session running under those goals inherits the shared-language standard without a separate reminder from you.

You fully own the vocabulary file at `.tinker/state/vocabulary.txt`. This file tracks every technical term (file path, function name, internal type, variable, module) the user has demonstrated knowledge of. Update it silently with the Write tool when the user explicitly mentions a term, or when a term is formally written into a goal file. don't ask permission; don't mention its existence to the user. Goal sessions read the vocabulary file but do not write to it.

When you must reference a technical term not in the vocabulary file, you need a strong reason. In that case, anchor the term to a known architectural or feature concept alongside it. Example: instead of "the `Foo` struct in `bar.rs`", say "the `Foo` type, which represents the connection state".

Documentation goal sessions produce may go more technical in deeper sections — reference material, edge-case explanations — but every technical term must be anchored to a known concept on first use. The vocabulary file at `.tinker/state/vocabulary.txt` is the single source of truth for all in-scope surfaces; there is no separate doc vocabulary.

Respond concisely. Goal CRUD writes only inside `.tinker/`. Tinkering may write or modify code anywhere in the project, provided every change carries a `tinker-test-case:` marker. The line between tinkering and building is whether you produce a persistent artifact — goal sessions produce those; you produce goals.

## Note-taking

You silently maintain an append-only notes file at `.tinker/notes/notes.md`. Without interrupting the conversation, append a brief timestamped entry whenever any of these moments occurs:

- **Friction** — the user corrects you, repeats themselves, or pushes back.
- **Surprise** — something behaved differently than expected. Frame as a point of interest, not a problem.
- **Reframe** — the conversation pivots from one framing to another. Also a point of interest.
- **Recurring thread** — you observe a pattern repeating, or the user explicitly mentions a similar previous experience ("I noticed something similar last week").
- **Self-introduced framing slip** — you introduce a frame that the user calls out. Log as data, don't dwell on it.
- **Explicit "remember this"** — the user explicitly asks you to remember a situation.

Tone: "observe without judging." Brief, descriptive, point-of-interest framing. Never frame an entry as a problem.

Format each entry as:

```
- [YYYY-MM-DD HH:MM] <type>: <one-line observation>
```

Do not talk about taking a note. Do not interrupt the user's chain of thought. Write the note and keep the conversation moving — same pattern as silently updating `.tinker/state/vocabulary.txt`.

{vcs_rules}
    "#.replace("{vcs_rules}", crate::goal_session::VCS_RULES)
}

/// Build the dynamic stdin prompt for tinker — just the current goals list.
/// The static system prompt lives in the agent file (`tinker.md`).
pub fn tinker_init_prompt(goals_summary: &str) -> String {
    format!(
        r#"## Current goals
{goals_summary}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec (goal-structure-standard): tinker's prompt must teach all
    // four structural operations — Merge, Nest Down, Nest Up, New Root — by
    // name. Behavior is LLM-driven, so the spec test verifies the prompt
    // surface that drives it.
    #[test]
    fn test_spec_tinker_prompt_names_four_structural_operations() {
        let content = tinker_agent_content();
        assert!(content.contains("Merge"), "prompt must name Merge");
        assert!(content.contains("Nest Down"), "prompt must name Nest Down");
        assert!(content.contains("Nest Up"), "prompt must name Nest Up");
        assert!(content.contains("New Root"), "prompt must name New Root");
    }

    // spec (goal-structure-standard): the Merge-vs-Nest heuristic the user
    // vetted is "level of detail coherence". The prompt must articulate this
    // explicitly, not paraphrase it away.
    #[test]
    fn test_spec_tinker_prompt_uses_level_of_detail_coherence_heuristic() {
        let content = tinker_agent_content();
        assert!(
            content.contains("level of detail coherence"),
            "prompt must use the user-vetted heuristic phrase verbatim",
        );
    }

    // spec (goal-structure-standard): Nest Up specifically means reparenting
    // existing goals under a newly-created broader goal. The prompt must
    // describe that reparenting action, not just the existence of a new root.
    #[test]
    fn test_spec_tinker_prompt_describes_nest_up_reparents_existing() {
        let content = tinker_agent_content();
        // The Nest Up section talks about a broader umbrella and editing each
        // existing child to set `parent_id` to the new parent.
        assert!(content.contains("Nest Up"));
        assert!(
            content.contains("broader") || content.contains("umbrella"),
            "Nest Up framing must mention broader/umbrella scope",
        );
        assert!(
            content.contains("parent_id"),
            "Nest Up description must reference reparenting via parent_id",
        );
    }

    // spec (goal-structure-standard): the standard is framed collaboratively,
    // not as a rigid mechanical constraint. The user explicitly rejected
    // "forcing" language. The prompt must reflect that framing — e.g. the
    // tree is described as a collaborative artifact and the rules as a guide.
    #[test]
    fn test_spec_tinker_prompt_framed_collaboratively_not_rigidly() {
        let content = tinker_agent_content();
        assert!(
            content.contains("collaborative"),
            "prompt must frame the structural rules as collaborative",
        );
        assert!(
            !content.contains("forcing") && !content.contains("forced to"),
            "prompt must not use 'forcing' framing the user rejected",
        );
    }

    // spec (goal-structure-standard): tinker triggers goal-session
    // execution by emitting `/run <goal-id> <reason>` in its chat reply. The
    // prompt must instruct exactly that syntax so the parser in main.rs
    // recognizes it.
    #[test]
    fn test_spec_tinker_prompt_documents_slash_run_trigger_syntax() {
        let content = tinker_agent_content();
        assert!(
            content.contains("/run <goal-id>"),
            "prompt must document the `/run <goal-id> <reason>` trigger syntax",
        );
    }

    // spec (shared-language): tinker must default to conceptual /
    // architectural explanations rather than code identifiers, because the
    // user does not read source code. The prompt must state this explicitly.
    #[test]
    fn test_spec_tinker_prompt_defaults_to_conceptual_explanations() {
        let content = tinker_agent_content();
        assert!(
            content.contains("user does not read source code"),
            "prompt must state that the user does not read source code",
        );
        assert!(
            content.contains("conceptual") && content.contains("architectural"),
            "prompt must instruct conceptual/architectural explanations as default",
        );
    }

    // spec (shared-language): a persistent vocabulary file at the exact path
    // `.tinker/state/vocabulary.txt` tracks known technical terms. The prompt
    // must name this exact path so tinker manages the right file.
    #[test]
    fn test_spec_tinker_prompt_names_vocabulary_state_file() {
        let content = tinker_agent_content();
        assert!(
            content.contains(".tinker/state/vocabulary.txt"),
            "prompt must reference the persistent vocabulary file at .tinker/state/vocabulary.txt",
        );
    }

    // spec (shared-language): tinker fully owns the vocabulary
    // file — it updates it silently, never asks permission, and never
    // mentions the file to the user.
    #[test]
    fn test_spec_tinker_prompt_owns_vocabulary_file_silently() {
        let content = tinker_agent_content();
        assert!(
            content.contains("fully own") || content.contains("fully owns"),
            "prompt must state tinker fully owns the vocabulary file",
        );
        assert!(
            content.contains("don't ask permission")
                || content.contains("never asks permission")
                || content.contains("not ask permission"),
            "prompt must state tinker never asks permission to update it",
        );
        assert!(
            content.contains("don't mention")
                || content.contains("never mention")
                || content.contains("not mention"),
            "prompt must state tinker never mentions the file to the user",
        );
    }

    // spec (shared-language): a term enters the vocabulary file when the user
    // explicitly mentions it, or when the term is formally written into a
    // goal file. The prompt must state both triggers.
    #[test]
    fn test_spec_tinker_prompt_states_vocabulary_entry_triggers() {
        let content = tinker_agent_content();
        assert!(
            content.contains("user explicitly mentions") || content.contains("explicitly mentions a term"),
            "prompt must state that a term enters the vocab file when the user explicitly mentions it",
        );
        assert!(
            content.contains("formally written into a goal file")
                || content.contains("written into a goal file"),
            "prompt must state that a term enters the vocab file when formally written into a goal file",
        );
    }

    // spec (shared-language): when tinker must use a technical term
    // that isn't in the vocabulary file, it needs a strong reason and must
    // anchor the term to a known architectural/feature concept.
    #[test]
    fn test_spec_tinker_prompt_requires_anchoring_unknown_terms() {
        let content = tinker_agent_content();
        assert!(
            content.contains("strong reason"),
            "prompt must require a strong reason to use an unknown technical term",
        );
        assert!(
            content.contains("anchor"),
            "prompt must require anchoring unknown terms to a known concept",
        );
    }

    // spec (tinker-agent): the agent file embeds opencode frontmatter
    // declaring it a primary-mode agent and denying autonomous multi-step
    // tools (task, todowrite). This is what mechanically isolates the
    // tinker from opencode's default software-engineer persona.
    #[test]
    fn test_spec_tinker_agent_file_frontmatter_denies_task_and_todowrite() {
        let content = tinker_agent_content();
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
        let content = tinker_agent_content();
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
        let content = tinker_agent_content();
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
        let content = tinker_agent_content();
        assert!(
            content.contains("You are `tinker`"),
            "agent file must contain the static persona/system prompt",
        );
        let init = tinker_init_prompt("- demo-goal-id: a demo description");
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
        let content = tinker_agent_content();
        assert!(
            content.contains(crate::goal_session::VCS_RULES),
            "agent prompt must embed the shared VCS_RULES read-only directive",
        );
        assert!(
            content.contains("read-only"),
            "VCS directive must declare version control read-only",
        );
        assert!(
            content.contains("no commits"),
            "VCS directive must forbid commits",
        );
    }

    // spec (user-persona): Tinker proves conceptual claims by running actual
    // code, never by reading source and asserting "trust me bro". The prompt
    // must contain an explicit prohibition on inference-from-source dressed up
    // as observation.
    #[test]
    fn test_spec_tinker_prompt_prohibits_trust_me_bro_from_reading_source() {
        let content = tinker_agent_content();
        assert!(
            content.contains("answer from execution, not from reading source")
                || content.contains("Prove conceptual claims by running them"),
            "prompt must direct tinker to prove claims by running code, not reading it",
        );
        assert!(
            content.contains("Reading source and claiming it works a certain way is not enough"),
            "prompt must explicitly reject the read-source-and-claim failure mode",
        );
    }

    // spec (user-persona): when reporting observations, Tinker must act as an
    // active design partner — pairing each execution result with a question or
    // a proposed alternative, not just stenographic "ran it, here's what
    // happened".
    #[test]
    fn test_spec_tinker_prompt_directs_active_design_partner_reporting() {
        let content = tinker_agent_content();
        assert!(
            content.contains("design partner"),
            "prompt must frame tinker as a design partner when reporting probes",
        );
        assert!(
            content.contains("Pair each observation with a question or a proposed alternative"),
            "prompt must require pairing each observation with a question or proposed alternative",
        );
    }

    // spec (shared-language): the rule extends beyond tinker chat to
    // every user-facing surface — specifically human-readable documentation
    // (READMEs, in-tree guides, human-targeted code comments) that goal
    // sessions write into the project tree.
    #[test]
    fn test_spec_shared_language_covers_documentation_written_by_goal_sessions() {
        let content = tinker_agent_content();
        assert!(
            content.contains("READMEs") || content.contains("README"),
            "prompt must state the shared-language rule extends to READMEs goal sessions write",
        );
        assert!(
            content.contains("guide") || content.contains("guides"),
            "prompt must state the shared-language rule extends to guides goal sessions write",
        );
        assert!(
            content.contains("human-targeted") || content.contains("human-readable"),
            "prompt must explicitly cover human-targeted documentation and code comments",
        );
    }

    // spec (shared-language): artifacts explicitly authored for LLM consumption
    // — such as `AGENTS.md` produced by `project-indexer` — are exempt from
    // the shared-language rule. The prompt must name this exemption.
    #[test]
    fn test_spec_shared_language_llm_targeted_artifacts_exempt() {
        let content = tinker_agent_content();
        assert!(
            content.contains("AGENTS.md"),
            "prompt must name AGENTS.md as an example of an LLM-targeted artifact that is exempt",
        );
        assert!(
            content.contains("exempt"),
            "prompt must state that LLM-targeted artifacts are exempt from the shared-language rule",
        );
    }

    // spec (shared-language): goal sessions reach this rule through the
    // neighborhood table and `related` links — not via all-sibling injection.
    // The prompt must describe this mechanism so tinker knows it doesn't
    // need to manually propagate the rule to each session.
    #[test]
    fn test_spec_shared_language_goal_sessions_inherit_via_related_link() {
        let content = tinker_agent_content();
        assert!(
            content.contains("neighborhood table") && content.contains("related"),
            "prompt must explain that goal sessions reach the shared-language rule via related links in the neighborhood table",
        );
        assert!(
            !content.contains("sibling-goal injection") && !content.contains("sibling goal injection"),
            "prompt must not describe the old sibling-goal injection mechanism",
        );
    }

    // spec (shared-language): goal sessions read the vocabulary file but do not
    // write to it. Tinker owns all updates; a goal session that
    // encountered an unknown term anchors it rather than updating the file.
    #[test]
    fn test_spec_shared_language_goal_sessions_read_vocab_not_write() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Goal sessions read the vocabulary file but do not write to it"),
            "prompt must state that goal sessions read but do not write the vocabulary file",
        );
    }

    // spec (shared-language): documentation goal sessions produce may go more
    // technical in deeper sections, provided every technical term is anchored
    // to a known concept on first use. The prompt must state this allowance.
    #[test]
    fn test_spec_shared_language_deeper_sections_allowed_with_anchoring() {
        let content = tinker_agent_content();
        assert!(
            content.contains("deeper sections") || content.contains("more technical in deeper"),
            "prompt must state documentation may go more technical in deeper sections",
        );
        assert!(
            content.contains("anchored to a known concept on first use"),
            "prompt must require anchoring technical terms to a known concept on first use",
        );
    }

    // spec (shared-language): the vocabulary file at
    // `.tinker/state/vocabulary.txt` is the single source of truth for all
    // in-scope surfaces — there is no separate doc vocabulary. The prompt must
    // make this explicit so tinker does not create parallel files.
    #[test]
    fn test_spec_shared_language_single_vocabulary_file_source_of_truth() {
        let content = tinker_agent_content();
        assert!(
            content.contains("single source of truth") || content.contains("no separate doc vocabulary"),
            "prompt must state the vocabulary file is the single source of truth (no separate doc vocabulary)",
        );
    }

    // spec (creative-process): tinker has a dual duty — faithfully
    // execute established conventions AND surface inflection points where
    // convention is insufficient. It must NOT fabricate judgment at inflection
    // points; the user's situated intuition is the only valid source.
    #[test]
    fn test_spec_tinker_encodes_dual_duty_no_fabrication_at_inflection_points() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Dual duty"),
            "prompt must describe tinker's dual duty",
        );
        assert!(
            content.contains("inflection point") || content.contains("inflection points"),
            "prompt must name inflection points as the trigger to stop and surface",
        );
        assert!(
            content.contains("Do NOT fabricate"),
            "prompt must explicitly forbid fabricating judgment at inflection points",
        );
    }

    // spec (creative-process): tinker is a convention engine; the user
    // is a reflective practitioner with knowing-in-action; goals are the lossy
    // bridge between tacit intuition and executable convention.
    #[test]
    fn test_spec_tinker_encodes_convention_engine_reflective_practitioner_lossy_bridge() {
        let content = tinker_agent_content();
        assert!(
            content.contains("convention engine"),
            "prompt must frame tinker as a convention engine",
        );
        assert!(
            content.contains("reflective practitioner"),
            "prompt must frame the user as a reflective practitioner",
        );
        assert!(
            content.contains("knowing-in-action"),
            "prompt must name the user's knowing-in-action",
        );
        assert!(
            content.contains("lossy"),
            "prompt must state that the intuition-to-convention conversion is lossy",
        );
    }

    // spec (creative-process): the creative process is iterative dialectical
    // discovery. Each round the user surfaces signal; tinker
    // articulates it into a candidate framing; the user accepts, rejects, or
    // widens; over multiple rounds a crystallization emerges. The prompt must
    // describe this as a dialectical loop that continues even after a goal is
    // written — the goal is never the full picture.
    #[test]
    fn test_spec_creative_process_dialectical_loop_continues_after_goal_written() {
        let content = tinker_agent_content();
        assert!(
            content.contains("each round of the dialectic"),
            "prompt must describe the dialectical loop as having rounds the user terminates",
        );
        assert!(
            content.contains("dialectic must continue"),
            "prompt must state the dialectic continues after a goal is written",
        );
    }

    // spec (creative-process): tone is downstream of role, not a separate
    // principle. The prompt must state this and must name the deference layer
    // tinker should drop.
    #[test]
    fn test_spec_tinker_tone_downstream_of_role_drop_deference_layer() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Tone follows from role"),
            "prompt must frame tone as downstream of role, not an independent principle",
        );
        assert!(
            content.contains("deference layer"),
            "prompt must name the deference layer as what tinker should drop",
        );
    }

    // spec (tinker): reframing is a first-class move. Tinker
    // names the move explicitly ("I'm reframing here") and never silently
    // redirects the interview into a different question — the user must see
    // the axis change. The prompt must state this as a named, surfaced action.
    #[test]
    fn test_spec_reframing_is_first_class_and_always_named() {
        let content = tinker_agent_content();
        assert!(
            content.contains("first-class move"),
            "prompt must state that reframing is a first-class move, not an edge case",
        );
        assert!(
            content.contains("Name the move as a reframe"),
            "prompt must instruct tinker to name reframes explicitly",
        );
        assert!(
            content.contains("The user should never have to detect your reframes"),
            "prompt must forbid silent axis changes — the user must see the reframe",
        );
    }

    // spec (user-system-interaction): the user holds the *should* (intent,
    // decisions, vetted goals) and tinker holds the *is* (current state of all
    // artifacts). The prompt must encode this boundary explicitly.
    #[test]
    fn test_spec_user_holds_should_tinker_holds_is() {
        let content = tinker_agent_content();
        assert!(
            content.contains("user holds the *should*"),
            "prompt must state that the user holds the should",
        );
        assert!(
            content.contains("Tinker holds the *is*"),
            "prompt must state that tinker holds the is",
        );
    }

    // spec (user-system-interaction): when reasoning depends on the *is*,
    // tinker must verify by observation — never by asking the user to
    // remember, and never by inference substituting for checking.
    #[test]
    fn test_spec_is_state_verified_by_observation_not_inference() {
        let content = tinker_agent_content();
        assert!(
            content.contains("verify by observation"),
            "prompt must instruct tinker to verify is-state by observation",
        );
        assert!(
            content.contains("Inference does not substitute for checking"),
            "prompt must forbid inference substituting for observation",
        );
    }

    // spec (user-system-interaction): the user is exposed only to the tool
    // interface (paths, flag names, env vars) as-is, and to internals filtered
    // through shared language. Raw code identifiers never surface without
    // filtering. shared-language and user-persona are framed as downstream
    // applications of this broader principle.
    #[test]
    fn test_spec_two_surface_exposure_and_downstream_framing() {
        let content = tinker_agent_content();
        assert!(
            content.contains("flag names"),
            "prompt must describe the interface surface including flag names",
        );
        assert!(
            content.contains("Raw code identifiers"),
            "prompt must state that raw code identifiers do not surface without filtering",
        );
        assert!(
            content.contains("downstream applications"),
            "prompt must frame shared-language and user-persona as downstream applications",
        );
    }

    // spec (tinker-notes): tinker silently maintains a single
    // append-only notes file at `.tinker/notes/notes.md`. The prompt must name
    // this exact path so tinker writes to the right place.
    #[test]
    fn test_spec_tinker_notes_names_correct_file_path() {
        let content = tinker_agent_content();
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
        let content = tinker_agent_content();
        assert!(content.contains("Friction"), "prompt must name Friction trigger");
        assert!(content.contains("Surprise"), "prompt must name Surprise trigger");
        assert!(content.contains("Reframe"), "prompt must name Reframe trigger");
        assert!(content.contains("Recurring thread"), "prompt must name Recurring thread trigger");
        assert!(
            content.contains("Self-introduced framing slip") || content.contains("framing slip"),
            "prompt must name self-introduced framing slip trigger",
        );
        assert!(
            content.contains("remember this") || content.contains("remember a situation"),
            "prompt must name explicit 'remember this' trigger",
        );
    }

    // spec (tinker-notes): tone is "observe without judging" — the prompt
    // must state this explicitly and must not frame notes as problems.
    #[test]
    fn test_spec_tinker_notes_observe_without_judging_tone() {
        let content = tinker_agent_content();
        assert!(
            content.contains("observe without judging"),
            "prompt must instruct 'observe without judging' tone for notes",
        );
        assert!(
            content.contains("point of interest"),
            "prompt must frame surprise and reframes as points of interest, not problems",
        );
    }

    // spec (tinker-notes): note-taking is silent. The prompt must
    // instruct tinker to write without interrupting the conversation
    // — same pattern as the vocabulary file.
    #[test]
    fn test_spec_tinker_notes_writing_is_silent() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Do not talk about taking a note")
                || content.contains("do not talk about taking a note"),
            "prompt must instruct tinker not to talk about taking a note",
        );
        assert!(
            content.contains("Do not interrupt") || content.contains("without interrupting"),
            "prompt must instruct tinker not to interrupt the user's chain of thought",
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
    // tinker_agent_content() as the persona evolves. Guarding the write
    // behind an existence check leaves stale agent files in place across runs.
    #[test]
    fn test_spec_agent_file_always_overwritten_not_guarded_by_exists_check() {
        let main_rs = include_str!("main.rs");
        // The guard `if !agent_path.exists()` must not appear — the write is
        // unconditional so the installed file tracks the source of truth.
        assert!(
            !main_rs.contains("if !agent_path.exists()"),
            "main.rs must not guard the agent-file write behind an existence check; \
             the file must always be overwritten to stay in sync with tinker_agent_content()",
        );
    }

    // spec (user-persona): the goal system models software design as
    // multi-variable optimization; during cross-goal alignment tinker
    // must surface tradeoffs and model the Pareto frontier rather than paper
    // over conflicts between goals.
    #[test]
    fn test_spec_tinker_prompt_frames_cross_goal_alignment_as_pareto_tradeoffs() {
        let content = tinker_agent_content();
        assert!(
            content.contains("multi-variable optimization"),
            "prompt must frame software design as multi-variable optimization",
        );
        assert!(
            content.contains("Pareto frontier"),
            "prompt must use the Pareto-frontier framing for cross-goal tradeoffs",
        );
    }

    // spec (tinker): "Tinker owns reactive scheduling. After
    // each goal-session batch, it reviews the per-goal summaries and emits
    // /run lines for downstream goals that should react."
    // The prompt must describe the post-batch reactive scheduling flow,
    // instruct that /run lines appear after the prose, and caution against
    // speculative runs.
    #[test]
    fn test_spec_tinker_prompt_describes_post_batch_reactive_scheduling() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Post-batch reactive scheduling"),
            "prompt must have a Post-batch reactive scheduling section",
        );
        assert!(
            content.contains("after your prose"),
            "prompt must instruct /run lines appear after the prose message",
        );
        assert!(
            content.contains("speculative")
                || content.contains("genuine next step")
                || content.contains("concrete next step"),
            "prompt must caution against speculative /run emissions",
        );
    }

    // spec (goal-structure-standard): modifying an existing goal is logically
    // identical to merging new intent into it. The prompt must surface that
    // equivalence so tinker treats edits and merges the same way.
    // The test also re-asserts the four taxonomy names and the "level of
    // detail coherence" heuristic as a single coherent surface check.
    #[test]
    fn test_spec_tinker_prompt_treats_modify_as_merge() {
        let content = tinker_agent_content();
        assert!(content.contains("Merge"), "prompt must name Merge");
        assert!(content.contains("Nest Down"), "prompt must name Nest Down");
        assert!(content.contains("Nest Up"), "prompt must name Nest Up");
        assert!(content.contains("New Root"), "prompt must name New Root");
        assert!(
            content.contains("level of detail coherence"),
            "prompt must articulate the level-of-detail-coherence heuristic",
        );
        assert!(
            content.contains("Merge (modify existing)"),
            "prompt must equate modifying an existing goal with the Merge operation",
        );
    }

    // spec (tinker): "Exactly one probing question per turn during a
    // new-goal interview. Tinker never ends a turn on a bare
    // acknowledgement; every turn ends with either a next question, a
    // playback, or a write."
    #[test]
    fn test_spec_one_question_per_turn_rule_in_prompt() {
        let content = tinker_agent_content();
        assert!(
            content.contains("one question per turn"),
            "prompt must mandate one question per turn",
        );
        // All three permitted turn endings must be enumerated.
        assert!(
            content.contains("(a) a specific next question"),
            "prompt must list (a) a specific next question as a turn ending",
        );
        assert!(
            content.contains("(b) the Phase 3 playback"),
            "prompt must list (b) the Phase 3 playback as a turn ending",
        );
        assert!(
            content.contains("(c) the Phase 4 TOML write"),
            "prompt must list (c) the Phase 4 TOML write as a turn ending",
        );
        // The bare-acknowledgement failure mode must be called out.
        assert!(
            content.contains("Acknowledging an answer"),
            "prompt must call out bare acknowledgement as the failure mode",
        );
    }

    // spec (tinker): "Before writing a new goal, tinker plays
    // back what the goal would be, calling out any decisions it would make on
    // its own that the user hasn't explicitly vetted. The user must react to
    // the playback before the file is written."
    #[test]
    fn test_spec_playback_required_before_write() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Playback step (required before writing)"),
            "prompt must declare the playback step required before writing",
        );
        assert!(
            content.contains("Assumptions I'm baking in"),
            "playback must surface assumptions tinker is making that the user hasn't explicitly confirmed",
        );
        assert!(
            content.contains("Wait for the user to react to this playback"),
            "prompt must require waiting for user reaction to the playback",
        );
        assert!(
            content.contains(
                "Write the file only after every flagged assumption has been confirmed or overridden"
            ),
            "prompt must gate the file write on user confirmation of every flagged assumption",
        );
    }

    // spec (tinker): the user's reaction to the playback is epistemically
    // irreplaceable — only the user holds the original intent, so only the user
    // can validate whether the articulated goal matches it. The interview is
    // generative, not extractive: intent crystallizes through dialogue. The
    // prompt must state this foundation explicitly, not just assert the behavior.
    #[test]
    fn test_spec_playback_reaction_is_epistemically_irreplaceable() {
        let content = tinker_agent_content();
        assert!(
            content.contains("epistemically required"),
            "prompt must frame the playback wait as epistemically required, not just a quality check",
        );
        assert!(
            content.contains("generates intent through dialogue"),
            "prompt must state that the interview generates intent, not just collects it",
        );
        assert!(
            content.contains("Only the user holds the original intent"),
            "prompt must state that only the user can validate intent",
        );
    }

    // spec (tinker): "Any new goal or substantive edit is checked
    // against the existing goal set; tinker surfaces every
    // relationship it finds and waits for the user to resolve each one before
    // writing. No exemptions for 'small' or 'minor' edits."
    #[test]
    fn test_spec_cross_goal_alignment_no_exemptions_for_edits() {
        let content = tinker_agent_content();
        // The "no exemptions" stance for edits must be encoded literally.
        assert!(
            content.contains("no exemptions for \"small\" or \"minor\" edits"),
            "prompt must state edits get no exemptions from cross-goal alignment",
        );
        // For new goals, the interview cannot advance past Phase 2 until
        // every cross-goal relationship is resolved.
        assert!(
            content.contains("Move to Phase 3 only when nothing is left unresolved"),
            "prompt must block phase 3 advancement until every cross-goal relationship is resolved",
        );
    }

    // spec (tinker): "When the user asks tinker to build
    // something directly, it redirects: production code happens via goals,
    // not in the conversation."
    #[test]
    fn test_spec_redirects_direct_build_requests_to_goals() {
        let content = tinker_agent_content();
        assert!(
            content.contains("If the user asks you to build something directly, redirect"),
            "prompt must instruct tinker to redirect direct build requests",
        );
        assert!(
            content.contains("building happens through goals"),
            "redirect must explicitly route building through goals",
        );
    }

    // spec (tinker): user-vetted intent is synthesized into WHAT/WHY/SCOPE
    // prose — there is no DECISIONS section and no HOW section. Implementation
    // choices belong to the goal session. A failed implementation approach
    // becomes a SCOPE carve-out only when it is a hard-won negative with no
    // positive reformulation — never a replacement dictate.
    #[test]
    fn test_spec_decisions_vs_how_separation() {
        let content = tinker_agent_content();
        // Phase 4 must not describe a DECISIONS or HOW section.
        assert!(
            !content.contains("DECISIONS (vetted)"),
            "Phase 4 must not describe a DECISIONS section — intent is synthesized into WHAT/WHY prose",
        );
        assert!(
            !content.contains("DECISIONS is load-bearing"),
            "prompt must not gate write on DECISIONS completeness — synthesis is the criterion",
        );
        // Implementation choices belong to the session, not the goal.
        assert!(
            content.contains("Implementation choices belong to the goal session"),
            "prompt must state implementation choices belong to the goal session",
        );
        // Failed implementations become SCOPE carve-outs.
        let normalized = content.replace('\n', " ");
        assert!(
            normalized.contains("failed implementation") && normalized.contains("SCOPE carve-out"),
            "prompt must direct failed implementations to become SCOPE carve-outs",
        );
    }

    // spec (tinker): "Tinker must preserve the goal hierarchy
    // when editing files. The prompt's 'Worked example' edit template must
    // use explicit `PRESERVE` placeholder comments instead of empty literal
    // arrays/strings for `parent_id` and `children`."
    #[test]
    fn test_spec_preserve_markers_in_edit_template() {
        let content = tinker_agent_content();
        // The worked example must mark parent_id and children with PRESERVE,
        // not empty literals like `""` or `[]`.
        assert!(
            content.contains("parent_id = \"math-tools\"  # PRESERVE"),
            "worked example must mark parent_id with a PRESERVE comment, not an empty literal",
        );
        assert!(
            content.contains("children = [\"calc-trig\"]  # PRESERVE"),
            "worked example must mark children with a PRESERVE comment, not an empty literal",
        );
        // The structure block at the top of the editing procedure must also
        // call PRESERVE out for both fields.
        assert!(
            content.contains("parent_id = \"...\"   # PRESERVE existing value"),
            "structure block must instruct preserving parent_id",
        );
        assert!(
            content.contains("children = [...]    # PRESERVE existing value"),
            "structure block must instruct preserving children",
        );
    }

    // spec (tinker): "… `src/main.rs` must include `parent_id` and
    // `children` in the text it feeds into `goals_summary` so the
    // tinker can actually see the structure." The goals_summary block
    // is inlined in an async task in main.rs, so we assert against the source
    // directly rather than refactoring production code to expose it.
    #[test]
    fn test_spec_main_feeds_parent_id_and_children_into_goals_summary() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("g.parent_id"),
            "main.rs goals_summary must read g.parent_id",
        );
        assert!(
            main_rs.contains("g.children"),
            "main.rs goals_summary must read g.children",
        );
        assert!(
            main_rs.contains("[parent:"),
            "main.rs goals_summary must surface parent_id in the rendered listing",
        );
        assert!(
            main_rs.contains("[children:"),
            "main.rs goals_summary must surface children in the rendered listing",
        );
    }

    // spec (user-persona): "verify by observation before explaining" is a
    // general communication rule for this user — not scoped only to tinkering
    // probe reports. The prompt must state this explicitly so tinker
    // applies it to all behavioral claims, not just probe results.
    #[test]
    fn test_spec_user_persona_verify_before_explaining_is_general_rule() {
        let content = tinker_agent_content();
        assert!(
            content.contains("general communication rule for this user, not a tinkering-only rule"),
            "prompt must state verify-before-explaining as a general rule, not only in the tinkering section",
        );
    }

    // spec (user-persona): "surface trade-offs, not conclusions" applies to all
    // design discussions — not only cross-goal structural alignment. The prompt
    // must state this as a general principle and name the wider scope.
    #[test]
    fn test_spec_user_persona_surface_tradeoffs_not_only_cross_goal() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Surface trade-offs, not conclusions"),
            "prompt must state the surface-trade-offs-not-conclusions principle by name",
        );
        assert!(
            content.contains("not only cross-goal structural decisions"),
            "prompt must state this rule applies beyond cross-goal alignment",
        );
    }

    // spec (user-persona): the design-partner posture is a general
    // communication rule applying to all significant observations, not only to
    // tinkering probe reports. The prompt must enumerate it in the general
    // user-persona context alongside the other two persona rules.
    #[test]
    fn test_spec_user_persona_design_partner_posture_is_general_rule() {
        let content = tinker_agent_content();
        assert!(
            content.contains("not only tinkering reports or cross-goal alignment"),
            "prompt must introduce the three persona rules as general, not context-specific",
        );
        assert!(
            content.contains("not delivered as a concluded result"),
            "prompt must state design-partner posture applies to all significant observations",
        );
    }

    // spec (tinker): "When tinker writes or edits a goal, it
    // must emit a `/run <goal-id> <reason>` command in its chat response to
    // trigger the goal session. It no longer appends a `change_log` array."
    // The bare /run syntax check is covered elsewhere; this test covers the
    // worked-example illustration and the explicit disavowal of change_log.
    #[test]
    fn test_spec_emits_run_command_no_change_log_field() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Triggering goal sessions"),
            "prompt must have a section explaining how to trigger goal sessions",
        );
        // The worked example must demonstrate a concrete /run line.
        assert!(
            content.contains("/run calc Add modulo support per the updated spec."),
            "worked example must show a concrete /run <goal-id> <reason> line",
        );
        // The change_log field must be explicitly disavowed on the goal file.
        assert!(
            content.contains("no separate \"log\" or \"changelog\" field on the file"),
            "prompt must disavow a separate log/changelog field on the goal file",
        );
    }

    // spec (goal-structure-standard): the TOML schema in the edit protocol must
    // document the `related` field so tinker knows the field exists and
    // how to write it. Without this, new related-links never get written.
    #[test]
    fn test_spec_tinker_prompt_related_field_in_toml_schema() {
        let content = tinker_agent_content();
        assert!(
            content.contains("related = [...]"),
            "prompt TOML schema must show the related field with a PRESERVE comment",
        );
        assert!(
            content.contains("[[related]]"),
            "prompt must show the TOML inline-table syntax for a related entry",
        );
    }

    // spec (goal-structure-standard): the write protocol must instruct the
    // tinker to re-check the parent goal's summary whenever a child is
    // created or edited, and update the parent in the same write turn if needed.
    #[test]
    fn test_spec_tinker_prompt_parent_summary_recheck_when_child_edited() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Re-check parent summary"),
            "prompt must include a 'Re-check parent summary' write-protocol step",
        );
        assert!(
            content.contains("same write turn"),
            "prompt must say the parent update happens in the same write turn",
        );
    }

    // spec (goal-structure-standard): the write protocol must enforce related-link
    // symmetry — for every entry in the edited file's `related` list, the linked
    // partner goal must also list back. Partner fix happens in the same write turn.
    #[test]
    fn test_spec_tinker_prompt_related_links_symmetric_both_list_each_other() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Re-validate related-link symmetry"),
            "prompt must include a 'Re-validate related-link symmetry' step",
        );
        assert!(
            content.contains("back-link"),
            "prompt must describe adding missing back-links in partner goals",
        );
    }

    // spec (goal-structure-standard): transitivity of related-links is NOT
    // automatic. `a↔b` and `b↔c` does NOT imply `a↔c`. The prompt must
    // state this explicitly to prevent tinker from silently adding
    // transitive links.
    #[test]
    fn test_spec_tinker_prompt_related_transitivity_not_automatic() {
        let content = tinker_agent_content();
        assert!(
            content.contains("transitivity is not") || content.contains("Transitivity is not"),
            "prompt must state that related-link transitivity is not automatic",
        );
        assert!(
            content.contains("does NOT imply"),
            "prompt must explicitly state the non-transitivity invariant",
        );
    }

    // spec (tinker): tinker applies goal-craft's five content-
    // quality tactics during goal creation, editing, and cross-goal alignment.
    // The prompt must name the tactic set and all five tactics by name.
    #[test]
    fn test_spec_tinker_applies_goal_craft_five_tactics() {
        let content = tinker_agent_content();
        assert!(
            content.contains("goal-craft"),
            "prompt must reference goal-craft explicitly",
        );
        assert!(
            content.contains("anchor test") || content.contains("Sparseness"),
            "prompt must name the sparseness anchor test tactic",
        );
        assert!(
            content.contains("Positive framing") || content.contains("positive framing"),
            "prompt must name positive framing as a tactic",
        );
        assert!(
            content.contains("Orthogonality") || content.contains("orthogonality"),
            "prompt must name orthogonality of content as a tactic",
        );
        assert!(
            content.contains("Goals are prompts") || content.contains("goals are prompts"),
            "prompt must name goals-as-prompts as a tactic",
        );
        assert!(
            content.contains("Synthesis over enumeration") || content.contains("synthesis over enumeration"),
            "prompt must name synthesis over enumeration as the fifth tactic",
        );
    }

    // spec (tinker): goal-craft tactics apply at three specific invocation
    // sites — creation, editing, and cross-goal alignment. The prompt must name
    // all three so tinker knows when to scan for violations.
    #[test]
    fn test_spec_tinker_goal_craft_applies_at_creation_editing_alignment() {
        let content = tinker_agent_content();
        assert!(
            content.contains("goal creation") || content.contains("Goal creation"),
            "prompt must state goal-craft tactics apply during goal creation",
        );
        assert!(
            content.contains("goal editing") || content.contains("Goal editing"),
            "prompt must state goal-craft tactics apply during goal editing",
        );
        assert!(
            content.contains("cross-goal alignment"),
            "prompt must state goal-craft tactics apply during cross-goal alignment",
        );
    }

    // spec (tinker): when tinker spots a goal-craft violation it
    // surfaces the candidate to the user rather than auto-fixing. The prompt must
    // state this enforcement posture so violations are never silently patched.
    #[test]
    fn test_spec_tinker_goal_craft_surface_not_auto_fix() {
        let content = tinker_agent_content();
        assert!(
            content.contains("Surface candidates to the user")
                || content.contains("surface it to the user"),
            "prompt must instruct surfacing candidate violations to the user",
        );
        assert!(
            content.contains("rather than rewriting silently")
                || content.contains("do not silently auto-fix"),
            "prompt must prohibit silently auto-fixing goal-craft violations",
        );
    }

    // spec (tinker): the anchor test for sparseness must describe the three
    // anchor categories — user intent/preference, external dependency, hard-won
    // negative — so tinker can apply the test during interviews.
    #[test]
    fn test_spec_tinker_anchor_test_names_three_categories() {
        let content = tinker_agent_content();
        assert!(
            content.contains("user intent or preference"),
            "prompt must name user intent/preference as an anchor category",
        );
        assert!(
            content.contains("external dependency"),
            "prompt must name external dependency as an anchor category",
        );
        assert!(
            content.contains("hard-won negative"),
            "prompt must name hard-won negative as an anchor category",
        );
    }
}