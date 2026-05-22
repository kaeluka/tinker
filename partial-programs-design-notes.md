# Partial Programs / Live Programmable Mocks — Design Notes

A future direction parked out of `scratch-tests-design-notes.md`. Not
being specced right now. Captured so we can resume later without
starting from scratch.

## Attribution legend

- `[U]` = stated, asked, or confirmed by the user
- `[C]` = suggested or proposed by Claude
- Unattributed = neutral framing of the problem space, not a commitment

---

## Direction (sketched, not specced)

`[U]` Tinker can write **partial code** — e.g., a module that handles
only the happy path of its concern. The partial code **compiles
against a mock module** that satisfies its dependency surface. The
mock module is **wired to the orchestrator REPL**: every call to a
mock function is surfaced to the orchestrator as a message. The
orchestrator **synthesizes a return value** to resume execution. The
synthesis can happen autonomously OR via an orchestrator-user
discussion at that moment.

Closer in spirit to pry / Common Lisp restarts / algebraic effects
than to a test runner. Each undefined behavior becomes an interactive
question.

---

## Decisions made (loose, while sketching)

- `[U]` Start **without memoization**. Every call surfaces fresh. See
  how that feels. Memoization can be added later if the live-question
  shape becomes annoying.

---

## Open questions (for when we return)

- How does the partial code get compiled / loaded / re-loaded as
  changes happen?
- How does the mock surface a call to the orchestrator —
  in-process channel, IPC, file, HTTP?
- Per-language story: Python's dynamism makes this easy; Rust needs
  trait stubs that route through some shared transport.
- When the orchestrator returns a value, how is it deserialized into
  the function's native return type?
- What's the user-facing UX when the program is paused waiting for a
  return value? Does the goal-session pane go quiet, or does it show
  "blocked on `f(x=2)`"?
- If the orchestrator can't decide and asks the user, what is the
  shape of that question (free-form chat, structured form, JSON
  example, etc.)?
- When does the partial code become "real"? Is there an explicit
  promotion step, or does it happen by writing real impls of
  previously-mocked functions?

---

## Conversation log

- `[U]` Sketched the direction in the middle of the scratch-tests
  spec; explicitly said this is bigger than what fits in that spec.
- `[U]` Asked to park it: "this is too big a step to be specing now."
  Removed from scratch-tests-design-notes.md and moved here. To pick
  up when ready.
