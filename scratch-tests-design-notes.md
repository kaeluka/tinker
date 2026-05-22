# Scratch Code for Understanding Program Behavior — Design Notes

> **Status: superseded.** The per-language `.tinker/scratch/`
> directory model and the three-tier boundary preference
> (scratch > mock > real-source-change) described below were
> replaced by the `tinker-test-case` marker convention + cleanup
> hook. Investigation code now lives anywhere in the project,
> marked with a `tinker-test-case` comment, and is removed by a
> cleanup agent before each goal-session run. See section 5 of
> `packaged-goals/coding-standards.toml` and the `## Tinkering`
> section in `src/orchestrator.rs` for the live design. The notes
> below are retained as the record of how we got here.

The orchestrator can write temporary code in `.tinker/scratch/` (or a
language-native fallback) to investigate what the program actually
does — reproducing bugs, understanding behavior, surfacing edge cases
— without polluting the project's real test suite. The form is the
orchestrator's choice: it can be a unit test under a framework, a
script with `main()`, a one-off executable — whatever fits the
investigation and the language.

A broader, more ambitious direction (orchestrator writes partial code
that runs against live programmable mocks) was sketched mid-discussion
and parked into `partial-programs-design-notes.md`. This spec is
narrower: just temp tests for understanding behavior.

## Attribution legend

- `[U]` = stated, asked, or confirmed by the user
- `[C]` = suggested or proposed by Claude
- Unattributed = neutral framing of the problem space, not a commitment

---

## Goal

`[U]` The orchestrator should be able to write **temporary tests in
`.tinker/scratch/`** to understand how the program actually behaves.
The motivating context: tinkering as the iterative process of turning
intuitions into specifications — "I'll only be able to know what I
want after looking at something that isn't what I want."

The canonical use case is **diagnostic**: the user reports an
observation ("the goals section is empty"), the orchestrator
instruments / fuzzes / probes the program to reproduce or characterize
the problem, then explains what's going on.

---

## Constraints

- `[U]` **Footprint**: tinkering leaves a small footprint. Without
  strong reason, it does not make network calls, write to temp
  directories or other locations outside the scratch paths, require
  secret access, or modify shared state. The scratch code itself is
  the intended trail; anything else is unintended side effect. If a
  probe genuinely needs to reach outside (e.g., an HTTP call to
  characterize a bug), the orchestrator flags it and asks first.
- `[U]` **Probe before describe**: when the user asks "what does X
  do?" or "what happens if Y?", the orchestrator's default is to
  answer from execution, not inference. Probe first, then explain
  what was observed.

---

## Open questions raised, not yet answered

_(none)_

---

## Design space

### Diagnostic walkthrough (worked example, fully `[U]`-supplied)

```
U: I noticed the goals section is empty
O: Where did you run this from?
U: ~/code/test
O: huh, that's odd — should be picking up stuff in ~/.tinker/goals
O: let me investigate... [builds a temporary test case using a fuzzer library]
O: wow, I've noticed a few cases where this can happen. Let me confirm
   what exactly the problem is here. Could you try enlarging your
   window? Does text then appear?
U: oh, wow! I can see the goals now! what's the problem??
O: great! the problem is that when the goal text is too long to fit...
```

Affordances surfaced by this walkthrough:

- Orchestrator asks for environmental context before guessing.
- Orchestrator forms a hypothesis.
- Orchestrator can **write and run a temporary test case**, using a
  language-appropriate fuzzer library, against the actual project.
- Orchestrator can **collaborate with the user**: ask them to perform
  a real-world action ("enlarge your window") and integrate the result.
- The conversation is multi-turn, with the orchestrator carrying
  hypothesis state across turns.
- At the end, the orchestrator can explain the root cause in user terms.

---

## Decisions made

- `[U]` Scratch location is **per-language**, with `.tinker/scratch/`
  used wherever it works and a language-native fallback where the
  build tool's strict layout makes `.tinker/scratch/` invisible.
- `[U]` Boundary preference, in order from most to least preferred:
  1. **Scratch** — standalone code in `.tinker/scratch/` (or
     language-native fallback) that doesn't touch real source.
  2. **Mock** — write a mock implementation of a capability/interface
     the real code depends on, swap it in via the existing
     capability-based DI seam (Section 1 of `coding-standards`), and
     run real code paths against the mock. Reuses the DI architecture
     the project already has; doesn't modify real source.
  3. **Real code change** — temporarily edit real source files (e.g.,
     to add instrumentation that scratch + mock can't reach), and
     revert (or rely on git to revert).

  The orchestrator should escalate to the next tier only when the
  current one can't cover the investigation.
- `[U]` Scratch form is **not restricted to tests**. It can be a unit
  test, a script with `main()`, an executable target, an interactive
  REPL session — whatever shape lets the orchestrator answer the
  investigation question. "Scratch tests" is shorthand, not a constraint.
- `[U]` **Lifetime**: scratch files are NEVER auto-cleaned. They
  accumulate as a trail of evidence the orchestrator left behind. If
  the project is a git repository, the scratch paths must be
  gitignored — `.tinker/scratch/` whenever used, plus the
  language-native fallback patterns (e.g., `**/_scratch_*.rs`,
  `**/cmd/_scratch_*/`, `**/_scratch/`). The orchestrator (or tinker's
  startup logic) is responsible for ensuring those entries exist.

### Scratch location per language (open list — "examples include")

Flexible-layout (file just lives in `.tinker/scratch/`, run directly):
- **Python** — `.tinker/scratch/foo.py`, run with `python`.
- **JavaScript / TypeScript** — `.tinker/scratch/foo.ts`, run with
  `node` or `tsx`.
- **Ruby** — `.tinker/scratch/foo.rb`.
- **Shell, Lua, and other dynamic-loader languages** — same shape.

Strict-layout (scratch must live inside the build tool's expected
tree; gitignore it):
- **Rust** — `src/bin/_scratch_<name>.rs`, run with
  `cargo run --bin _scratch_<name>`.
- **Go** — `cmd/_scratch_<name>/main.go`, run with
  `go run ./cmd/_scratch_<name>`.
- **Java / Kotlin (Maven or Gradle)** — `src/test/java/_scratch/<Name>.java`
  or `src/test/kotlin/_scratch/<Name>.kt`.
- **C / C++ (CMake, Bazel, etc.)** — depends on the build system;
  typically a separate scratch executable target with files in a
  `scratch/` subdir referenced from the build file.
- **C# / .NET** — separate scratch console-app project (`.csproj`)
  under a `scratch/` folder.

The list is non-exhaustive. The orchestrator picks based on the
language and tooling it detects in the project; the underscore prefix
or `_scratch` marker keeps these visually separable from real code,
and the path is gitignored.

---

## Things explicitly NOT assumed

- No assumption that "slow" is the only problem the user is solving —
  they also said "clunky", which is a separate UX axis.
- No assumption about who runs the scratch tests (the user
  interactively? the goal session as part of its own loop? both?).
- No assumption about lifetime of scratch tests (truly throwaway vs.
  persistent fixtures vs. some halfway).
- No assumption that "orchestrator writes" is literal — could mean any
  AI in the system (the goal session has more context about the code
  it's writing, so it might be a more natural author).
- **No assumption that this spec needs to handle the partial-programs
  / programmable-mocks direction.** That's in `partial-programs-design-notes.md`.

---

## Conversation log

- `[U]` Proposed: scratch tests in `.tinker/scratch/` as an alternative
  to per-function CLI. Motivation: current approach is clunky and slow
  to build via AI ("slow" = AI build time, not runtime).
- `[C]` (retraction) Claude prematurely framed this as a replacement
  vs. addition decision; user reminded that it's a question first.
  Reopening as "what use case shape does the user want?"
- `[U]` User opened up the question: the real feature is the
  orchestrator being able to **execute code during a conversation** in
  service of iterating toward a clearer spec. Meta: "tinkering is the
  iterative process of turning intuitions into specifications."
- `[U]` Provided a concrete diagnostic walkthrough (goals-section-empty
  → orchestrator instruments + fuzzes → asks user to enlarge window →
  confirms cause). Recorded in Design space.
- `[U]` Wants to pause before writing the specing-time walkthrough —
  needs more time to develop intuitions.
- `[U]` Sketched a partial-programs / live-mock direction mid-thread.
- `[U]` Asked to **narrow the scope of this spec** to the
  "orchestrator uses temp tests to understand program behaviour" case.
  Partial-programs direction moved out to its own design-notes file.
- `[U]` Pointed out this isn't a goal TOML — it's an orchestrator
  prompt change. Right framing; goals are for project-level standards
  the orchestrator enforces on user code, not for tinker's own
  affordances.
- `[U]` Picked option (a) for scratch location: per-language, with
  `.tinker/scratch/` for flexible-layout languages and a language-
  native fallback for strict-layout ones (Rust/Go/Java/Kotlin/C/C++/
  C#). Open list of examples recorded.
- `[U]` Cleanup: no auto-cleanup. Scratch lives as a trail. In git
  repos the relevant paths must be gitignored.
- `[U]` Form: not restricted to "tests" — can be a script, a `main()`,
  a binary, whatever fits.
- `[U]` Sharpened boundary preference: **scratch > mock > real code
  change**. Mocks (via the existing DI seam) sit between standalone
  scratch and modifying real source.
- `[U]` Added "probe before describe" as the orchestrator's default
  for "what does X do?" / "what happens if Y?" questions.
- `[U]` Added "footprint" constraint: tinkering doesn't make network
  calls, write to temp dirs, require secrets, or modify shared state
  without strong reason and an ask-first.
