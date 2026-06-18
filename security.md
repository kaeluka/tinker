# tinker — security context and threats

## Security context

Tinker is a local single-user development tool. It runs on the user's own
machine, with the user's own filesystem and network permissions. There is
no remote attacker model in scope.

What is trusted:
- The user.
- The native backend's HTTP client and the model providers it talks to
  — any OpenAI-protocol endpoint, configured per-tier in
  `.tinker/config.toml` (OpenRouter is the starter-template default; the
  user may point each tier at any OpenAI-protocol endpoint, including
  local model servers that ignore the `Authorization` header).
- The `~/.tinker/` and `<project>/.tinker/` directories the user controls.

What is NOT trusted, or only conditionally trusted:
- The text content of LLM responses. Models can hallucinate, follow
  prompt injections, or produce plausible-looking but wrong code.
- TOML files in `.tinker/goals/` once written. They may be malformed
  through bugs in the orchestrator's editing logic. The loader has to
  tolerate this without crashing or losing other goals.
- `.tinker/` directories in unexpected ancestor paths. The multi-dir
  merge climbs from `cwd` upward; if the user is `cd`'d to a directory
  that happens to be inside an unrelated `.tinker/`-bearing tree, those
  goals would be silently included.

The security model assumes the user reviews changes before committing.
Goal sessions run as native code in the user's process — there is no
subprocess sandbox. The capability boundary lives in-process at the
tool-call layer (`ToolPolicy` in `native.rs`): tend's writes are
restricted to `.tinker/goals/`, and tool calls are denied at the
boundary rather than relying on a subprocess approval UI.

## Threats

### T1. A malformed goal TOML file blocks loading of other goals

The orchestrator writes goal TOML files by hand and has historically
produced syntactically broken output (unclosed `"""`, duplicate top-level
keys, content placed inside the description block). If a single bad file
caused the entire goal directory to fail to parse, the user would lose
visibility of all their other goals.

Mitigations:
- Per-file isolation. `goal::load_goals` parses each TOML independently;
  a parse failure on one file is caught and reported via the
  `LoadResult.errors` channel, and all valid sibling goals still load.
  → test: `test_security_t1_parse_error_isolated`.
- Errors surface as system messages in the REPL, not stderr (stderr
  would corrupt the TUI's alternate screen).
- The orchestrator gets an automatic correction prompt for up to two
  attempts when its own edit introduces a new parse error.

### T2. Cross-project session contamination

Goal sessions are dispatched via the in-process native backend, not via
a subprocess that might persist state across projects. If session
state were written into a shared `.tinker/goals/<id>.toml`, every
project that picked the goal up via the multi-dir merge would resume
the same session — leaking one project's history into another.

Mitigations:
- Goal TOML files contain no session state. Session IDs are ephemeral
  runtime values held only in memory and never written back to disk.
  Each project starts a fresh session every time it runs, so there is
  no persistent cross-project session leak surface.
  → test: `test_security_t2_no_session_persistence_means_no_leak`.

### T3. Unintended ancestor `.tinker/` merge

Tinker walks up from `cwd` collecting every `.tinker/` directory it
finds. A user `cd`'d to a location nested inside an unrelated
`.tinker/`-bearing tree would silently inherit those goals.

Mitigations:
- All merged goals (and their source paths) are listed in the
  orchestrator's init prompt and rendered in the goal tree, so the user
  can see exactly which goals are in scope.
  → test: `test_spec_discover_tinker_dirs_walks_up` (verifies ancestor discovery).
- On launch the REPL emits a system message `"Merged N .tinker dirs
  (cwd + N-1 ancestor)."` when N > 1.
- No mitigation prevents the merge itself; the user is expected to
  notice.

### T4. Native backend tool calls run without a per-call approval prompt

The native backend executes tool calls (read, write, edit, glob, grep,
bash, send_message, spawn_session) in-process inside the user's tinker
process. Unlike a subprocess with an interactive approval UI, there is
no per-call prompt — every dispatched agent run is user-initiated and
the tool calls fire automatically.

Mitigations:
- The in-process capability boundary is `ToolPolicy` (`native.rs`):
  every tool call is checked before execution. The default policy
  (`Unrestricted`) lets a goal agent run bash and read/write anywhere
  on the filesystem; `TendScope` (used for tend) strips bash entirely
  and narrows writes to `.tinker/goals/`. A denied call returns the
  reason to the model as the tool result, so the model can adapt.
- The goal-scope boundary is the agent's authored goal description.
  Agents work within the goal they were dispatched to and the user
  reviews changes before committing.
- Tend's file-scope enforcement is encoded in `ToolPolicy::TendScope`
  and applies to every turn (the policy is a struct field on the
  runner, not a per-call value the model could omit).

### T5. Tool output written directly to the terminal could corrupt the TUI

The native backend runs in-process and writes chunk content to the TUI
session stream via an mpsc channel. While tinker is in the alternate
screen mode (raw terminal, no scrollback), any content that bypassed
the channel and was written directly to stdout/stderr would overwrite
the rendered UI and break the user's ability to interact.

Mitigations:
- The native backend's stdout path goes through the `Chunk` callback
  the session-task passes into `OpenCodeRunner::run`. Every piece of
  tool output and every error chunk flows through this callback into
  the session log, never directly to the terminal.
- Tool output is capped at `MAX_TOOL_OUTPUT_CHARS` (30 000 chars) in
  `native.rs` before being fed back to the model, so one verbose
  `cargo test` run cannot blow the context window or the session log.
- Errors surface as `⚠` chunks via the same channel — backend
  failures, context-overflow drops, and tool-call denials all reach
  the user through the session stream rather than leaking to the
  terminal.
