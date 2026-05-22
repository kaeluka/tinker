# Coding Assistant — Design Notes

## Attribution legend
- `[U]` = stated, asked, or confirmed by the user
- `[C]` = suggested or proposed by Claude
- Unattributed = neutral framing of the problem space, not a commitment

---

## Goal

`[U]` A CLI coding assistant called **tinker** that autonomously works toward user-defined goals, using `opencode` as its LLM backend.

---

## Constraints

- `[U]` Written in Rust.
- `[U]` Must call out to the `opencode` CLI — no direct LLM API calls.
- `[U]` At most 2 `opencode` processes at any time (orchestrator + one goal session).

---

## Open questions raised, not yet answered

_(none — all resolved)_

---

## Design space

### Goal lifecycle
- `[U]` Created interactively via `/goal` (Q&A flow, similar to `/spec-it`).
- `[U]` Optional "definition of done" — met → auto-retired.
- `[U]` No definition of done → "forever goal" (e.g., "keep README up to date"), never auto-retired.
- `[U]` Subgoals can be created autonomously by a parent goal.
- `[U]` Goals form a tree.

### Scheduling
- `[U]` Orchestrator loops automatically: pick a relevant goal → run it → collect summary → re-evaluate → repeat.
- `[U]` Relevance (not blocking) governs scheduling — the orchestrator's LLM session decides.
- `[U]` When multiple goals are relevant, the LLM picks which to run next.
- `[U]` Serial: one goal session at a time.
- `[U]` When no goals are relevant, the orchestrator idles — waits for `/quit`, `ctrl-c`, or new user input.
- `[U]` A slash command toggles manual mode, where the user must confirm each loop iteration.

### Sessions
- `[U]` Each goal has a dedicated `opencode` session (session ID stored in its TOML file), reused across invocations — history accumulates.
- `[U]` The orchestrator has its own `opencode` session: handles relevance evaluation, goal scheduling, and REPL queries — all interleaved.
- `[U]` Goal knowledge accumulates in orchestrator session history; goals are "introduced" on creation and re-introduced from TOML files on restart.
- `[U]` Trigger: after each goal session completes, the orchestrator asks that same session for a summary of what changed. That summary drives the next scheduling decision.
- ~~`[U]` Filesystem watcher (inotify) as trigger — retracted.~~

### Persistence
- `[U]` `.tinker/` directory at repo root; goals stored as TOML files in `.tinker/goals/`.
- `[U]` Goal file fields (at minimum): description, `opencode` session ID, optional definition of done, parent/child links, retirement status.
- `[U]` Human-readable/editable, but not the primary use case.
- `[U]` On restart: fresh orchestrator session; all goals re-introduced from TOML files.

### UX / TUI
- `[U]` Single terminal: three-pane TUI using a modern Rust TUI framework.
- `[U]` Layout:
  ```
  ┌─────────────┬──────────────┐
  │             │  goal tree   │
  │    REPL     ├──────────────┤
  │             │ session log  │
  └─────────────┴──────────────┘
  ```
- `[U]` Goal tree: `tree`-style, selectable. Active goal is bold/highlighted. Default selection: first goal.
- `[U]` Log pane: shows the selected goal's session log.
- `[U]` REPL: talks to the orchestrator session. CRUD goals, ask questions. Free-form natural language + slash commands. No direct access to individual goal sessions.
- `[U]` Slash commands: `/goal` (CRUD), `/help`, manual-mode toggle.
- `[U]` REPL does not trigger building — all building is orchestrator-driven after goals are defined.

---

## Decisions made

_(folded into Design space above)_

---

## Things explicitly NOT assumed

- No assumption about parallelism beyond "serial for now" — may revisit.
- No assumption about what the manual-mode toggle slash command is named.
- Goal sessions are not assumed to be interruptible.

---

## Where we ended up

**tinker** is a Rust CLI tool that lets the user define high-level goals (e.g., "build a TUI calculator", "keep README up to date") via a `/goal` slash command. Goals are persisted as TOML files in `.tinker/goals/`, each owning a dedicated `opencode` session that accumulates history.

An orchestrator (itself an `opencode` session) runs a loop: pick the most relevant unfinished goal, run its session, collect a summary of what changed, decide what to do next. At most one goal session runs at a time alongside the always-on orchestrator. When nothing is relevant, it idles.

The TUI has three panes: REPL on the left (talks to the orchestrator), goal tree upper-right, and session log lower-right. The user shapes the work by defining goals; all building happens autonomously.

---

## Conversation log

- `[U]` Initial pitch: CLI coding assistant, "goals" as crosscutting concerns, `opencode` CLI as LLM backend, goal tree, post-change goal evaluation and steering.
- `[U]` ~~Trigger: filesystem watcher (inotify).~~ Retracted — bad idea.
- `[U]` Trigger replaced with: each `opencode` call returns a summary used to determine which goals to fire.
- `[U]` Goal creation via `/goal` Q&A; subgoals can be auto-created; optional definition of done; forever goals.
- `[U]` Each goal + orchestrator get dedicated `opencode` sessions; goal sessions reused (same ID) across invocations.
- `[U]` Persistence: `.tinker/goals/` as TOML files.
- `[U]` Language: Rust. Tool name: `tinker`.
- `[U]` TUI: REPL left; upper-right goal tree; lower-right log of selected goal. Active goal highlighted. Default: first goal.
- `[U]` Max 2 opencode processes at once; goal sessions not interruptible.
- `[U]` Orchestrator auto-loops; slash command for manual mode; idles when nothing is relevant.
- `[U]` Slash commands: `/goal`, `/help`, manual-mode toggle.
- `[U]` REPL shares orchestrator session; user can CRUD goals while a goal session is running.
- `[U]` On restart: fresh orchestrator session; re-introduce goals from TOML.
- `[U]` Default tree selection on startup: first goal.
