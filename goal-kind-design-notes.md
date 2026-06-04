# Goal-kind design notes

## Attribution legend
- `[U]` — stated, asked, or confirmed by the user.
- `[C]` — suggested, framed, or proposed by Claude.
- Unattributed — neutral framing of the problem space, not a commitment.

## Goal
- `[U]` Every goal is optimized to be a good prompt (the existing goals-are-prompts principle). WHAT/WHY is the prompt-shape that makes features into good prompts — but for development processes and automations it produces a *worse* prompt. We want the structure(s) that make those into good prompts too. _(anchored)_

## Constraints
- `[U]` WHAT/WHY must remain the right structure for feature goals; the change is additive, not a replacement.
- `[U]` The optimization target is unchanged: a goal must be a good prompt. This is about the right prompt-shape per goal, not a new target.
- `[U]` Behavioral goals fire by **an agent interpreting the trigger prose and dispatching** — not by a mechanical scheduler. (So natural-language triggers are sufficient.)
- `[U]` Today the firing mechanism is effectively absent: `coding-standards` doesn't fire reliably or at all. Making behavioral goals fire is part of this work, not a solved baseline.

## Open questions raised, not yet answered
- `[C → awaiting U]` **tend's write path per kind** — `[C]` proposed resolution: one interview that branches *after* tend classifies the kind. Feature → WHAT/WHY judged by `feature-craft`; behavior → natural-language summary + trigger + good-prompt body judged by `behavior-craft`. Confirm or redirect.
- `[C → awaiting U]` **Migration** (execution, not design): which of the existing ~29 goals get `kind = behavior` vs `feature`, the `goal-craft`→`feature-craft` rename across all references, authoring `behavior-craft`, and rewriting summaries to natural language. A follow-up once the design is accepted.
- `[C]` Accepted boundary (flag to revisit): report-boundary firing won't catch purely peer-dispatched work that reports to a peer rather than tend.

## Design space

### Axis: the executed-later prompt-shape — status: DECIDED (no template; explicit `kind` only)
- `[U]` It needs a **summary that describes its trigger** — or a similar mechanism. (Tentative; the trigger is the first identified slot.)
- ⚠️ Possible overload: every goal already carries a `summary` with a `triggers:` clause (governed by `goal-structure-standard`), but today that names *situations where an agent should consult the goal* — a navigation index, not an *execution* trigger. Reusing it would conflate "when to read this" with "when to run this." Fork: extend/repurpose vs new field. `[C]` flag.

### Axis: summary descriptor differs by kind — status: DECIDED (natural language, kind-flavored)
- `[U]` The existing `triggers:` clause may simply be **badly named**. The two kinds are described by different things:
  - **Behavioral / executed-later goal → *triggered*** ("when does this run").
  - **Feature goal → *owns part of the artifact*** (a region of code; = goal-complementarity's distinct-domain ownership). Not triggered by anything.
- `[C]` So "owns vs triggered" is the same boundary as "change-to-code vs executed-later," restated. Implies the summary/index descriptor is **kind-dependent** rather than a fixed `governs:/triggers:` pair forced onto every goal — and the run-trigger isn't a bolted-on new field, it's the natural descriptor of the behavioral kind. _(to confirm)_
- `[U]` The body does **not** require spelled-out steps. A good prompt *sometimes chooses* to spell them out. The choice is a tradeoff:
  - **Repeatability** — explicit ordered steps (e.g. "after each feature: commit, push, open a PR"). Deterministic.
  - **Flexibility** — state intent, trust the agent (e.g. "review the code, take security.md into consideration"). Adaptive.
- `[C]` Implication: the executed-later shape is *permissive*, not a fixed template. The procedure is allowed but not mandated; picking the point on the repeatability↔flexibility axis is prompt-craft. So the only clearly-structural addition so far is the **trigger**; the body is prompt-crafted.

### Axis: which goals don't fit WHAT/WHY — status: DECIDED (the defining line)
- `[U]` The line: **is this a change the user wants to the artifact (code)?** → WHAT/WHY (feature). **Or is this something that needs to be executed at certain points in the future?** → the other shape (process/automation).
- `[U]` Classification is **tend's job** to figure out during the interview — not a label the user applies by hand.
- `[U]` The symptom is prompt quality: forcing an "executed-later" goal into WHAT/WHY yields a worse prompt. Ties back to the earlier discussion (goal-craft's no-procedure / synthesis-over-enumeration rules are spec-review rules and fight an operating-procedure prompt).

## Decisions made
- `[U]` The kind boundary is: **a desired change to the artifact (code)** vs **something to be executed at certain points in the future.**
- `[U]` The `summary` descriptor is kind-specific, and the semi-formal `governs:/triggers:` format can be dropped for **natural language**: feature → *"This goal owns…"*; behavioral → *"This goal runs when / checks that / ensures…"*. For a behavioral goal the one sentence is both the run-condition and the navigation hint (double duty).
- `[U]` Firing is **agent-interpreted, not scheduled** — an agent reads the trigger prose and dispatches. Confirms natural-language triggers suffice.
- `[U]` **tend** evaluates behavioral-goal triggers and fires the matching ones, as orchestrator (extends its existing post-write dispatch role).
- `[U]` tend classifies the goal's kind during the interview; the user does not hand-label it.
- `[U]` An executed-later goal does not require a spelled-out procedure. Explicit steps vs intent-only is a deliberate **repeatability ↔ flexibility** tradeoff the prompt-author makes per case.
- `[U]` **No template** for executed-later goals — the body is just good prompt engineering. The trigger is the only structural addition; WHY/steps are not mandated sections (a good prompt may still motivate or enumerate when that serves it).
- `[U]` **Explicit `kind` field** is the single structured addition; the summary, trigger, and body all stay natural-language prose. (Resolves the original `kind`-field proposal: yes, but it's the *only* structured bit.)
- `[U]` The two kind values are **`feature`** (owns part of the artifact) and **`behavior`** (US spelling; triggered, runs at certain points).
- `[U]` **Classification principle:** the craft/meta standards are themselves `behavior` goals — `feature-craft`, `behavior-craft`, `coding-standards`, `shared-language`, `goal-structure-standard` all *run at certain points* (a goal is written, code changes, user-facing text is produced) and own no code. `feature` goals are the ones owning artifact regions (TUI, backends, storage layer, agent runtime).

- `[U]` **The review standard splits by kind.** `goal-craft` reverts to the *feature* standard (WHAT/WHY, sparseness, synthesis) and sheds every carve-out accreted this session. A **new sibling standard** governs `behavior` goals as good prompt engineering (role, trigger, optional procedure, the repeatability↔flexibility tradeoff). Two single-purpose standards.
- `[U]` The new `behavior` standard **stands entirely on its own** — it embodies the prompt-engineering principles in its own words and does NOT reference `prompt-guide` (a Claude Code skill external to the tinker tree).
- `[U]` **Names: `feature-craft` + `behavior-craft`.** `goal-craft` is renamed to `feature-craft` (it's too generic once there are two kinds and implied it governed all goals); the new sibling is `behavior-craft`. Symmetric twins, both paired under the structural standard.
- `[C]` Cleanup this rename triggers (migration): every reference to `goal-craft` across the tree (e.g. `goal-structure-standard`'s "content twin" link, `tend`'s write procedure) updates to `feature-craft`, and the structural standard now pairs with *both* content standards. `goal-structure-standard` stays one standard — structure (tree/edges/summary/placement) applies to both kinds.

### Implication `[C]` (confirmed earlier, kept for the record)
- The **hybrid problem dissolves**: `coding-standards` isn't a spec/agent hybrid — under the feature/behavior line it's simply a `behavior` goal (it runs when code changes). Same for `@reviewer`-type user agents.

## Where we ended up
Goals gain an explicit **`kind`** field with two values:

- **`feature`** — owns a region of the artifact (code). Structured as **WHAT/WHY**, judged by **`feature-craft`** (the renamed `goal-craft`, reverted to sparseness/synthesis with its session-accreted carve-outs removed).
- **`behavior`** — runs at certain points (triggered). **No template**: a natural-language summary stating its trigger (*"runs when… / checks that… / ensures…"*) plus a body that is simply good prompt engineering — steps spelled out or intent-only per a deliberate **repeatability ↔ flexibility** tradeoff. Judged by a new standalone **`behavior-craft`** standard.

`kind` is the *only* new structured field; summary, trigger, and body are all prose. Summaries drop the semi-formal `governs:/triggers:` format for kind-flavored natural language (*"This goal owns…"* vs *"This goal runs when…"*).

**tend** classifies the kind during the interview (the user never hand-labels), writes the matching shape, and — as orchestrator — **fires behavior goals at the report boundary**: when tend-dispatched work reports back, tend evaluates the behavior goals' triggers and dispatches the matches. (Today this firing doesn't happen at all; `coding-standards` is the dead-trigger proof.)

Consequences: the **craft/meta standards are themselves `behavior` goals** (they run when a goal is written, code changes, or text is produced); `feature` goals are the code-owners. The earlier **spec/agent hybrid problem dissolves** — `coding-standards` and user-authored `@reviewer`-style agents are just `behavior` goals. `goal-structure-standard` stays a single standard (tree/edges/summary/placement apply to both kinds) and now pairs with both content standards.

**Follow-up (execution, post-acceptance):** label the ~29 existing goals, rename `goal-craft`→`feature-craft` across all references, author `behavior-craft`, rewrite summaries to natural language, and wire tend's report-boundary trigger evaluation.

## Things explicitly NOT assumed
- ~~That a `kind` field is the answer~~ — RESOLVED: explicit `kind` field, values `feature`/`behavior`; it is the *only* structured addition.
- The kind split is binary (`feature`/`behavior`); the earlier "standards" third-kind idea collapses into `behavior` (a standard is applied at certain points).
- NOT yet decided: whether `goal-craft` splits into two standards, how tend observes triggering moments, and the existing-goal migration.

## Conversation log
- `[U]` Surface request: goals could carry a `kind` variant (`requirement`/`behavior` or similar). Restarted the interview WHY-first, treating the field as a proposed solution rather than the goal.
- `[U]` Motivation: WHAT/WHY fits features but not development processes / automations. Set working goal; asked what specifically breaks when forcing a process into WHAT/WHY.
- `[U]` Confirmed the symptom is prompt quality — a goal must be a good prompt and WHAT/WHY produces a worse one for processes (per the earlier discussion). Anchored the goal on goals-are-prompts.
- `[U]` Gave the defining line: change-to-code vs executed-at-points-in-the-future; tend figures out the classification. Asked what the executed-later prompt-shape must contain.
- `[U]` First slot: a summary describing the trigger (or similar). Flagged possible overload with the existing `summary`/`triggers:` navigation index; asked whether the body needs steps spelled out or trusts the agent like a feature goal does.
- `[U]` Steps are optional — a repeatability↔flexibility tradeoff the prompt-author makes. Inferred the shape is permissive, trigger being the only clear structural addition; asked whether trigger is the single structural slot vs a defined template, and whether WHY survives.
- `[U]` No template — just good prompt engineering. Recorded; noted this resolves the review-standard split and dissolves the earlier spec/agent hybrid problem (coding-standards = executed-later goal). Asked about the trigger mechanism next.
- `[U]` Reframed the mechanism: the existing `triggers:` clause is badly named; behavioral goals are *triggered*, feature goals *own part of the artifact*. Same boundary, restated. Asked whether the summary descriptor should therefore become kind-dependent.
- `[U]` Yes — kind-specific summary; and proposed dropping the semi-formal format for natural language ("This goal owns…" vs "This goal runs when/checks/ensures…"). Recorded; flagged the terseness tradeoff and surfaced the upstream firing-mechanism question (scheduler vs agent-interpreted) that decides whether natural language survives.
- `[U]` Firing is agent-interpreted, not scheduled; coding-standards doesn't fire today. Recorded as constraints. Asked which agent evaluates triggers and when.
- `[U]` tend fires behavioral goals. Recorded; raised tend's cross-population visibility requirement, and asked whether tend-fires-them tips the explicit-`kind`-vs-implicit call.
- `[U]` Explicit `kind` field, natural language for everything else. Recorded; asked about naming the two kind values.
- `[U]` Kind values: `feature` / `behavior` (US spelling). Recorded; tidied the open-questions list to what remains (review-standard split, tend visibility, standards classification, tend write path, migration).
- `[U]` Split the standard: `goal-craft` = feature standard (sheds carve-outs); new sibling = behavior standard. Recorded; asked whether the new standard stands alone or leans on prompt-guide, and what to name it.
- `[U]` Asked what prompt-guide is (clarified: external Claude Code skill, not part of tinker). Confirmed the new standard stands on its own and doesn't reference it. Name still open.
- `[U]` Names settled: rename `goal-craft` → `feature-craft`, new standard `behavior-craft`. Recorded the rename migration (update all `goal-craft` references; structural standard pairs with both).
- `[U]` Confirmed the classification principle: standards = `behavior`, code-owners = `feature`. Recorded.
- `[U]` Firing happens at the **report boundary** — tend evaluates behavior triggers when tend-dispatched work reports back. Recorded with an explicit accepted-boundary flag (won't catch purely peer-dispatched work that reports to a peer). Wrote up "Where we ended up"; only the tend write-path confirmation and the migration follow-up remain.
