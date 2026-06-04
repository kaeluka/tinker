# tinker — security context and threats

## Security context

Tinker is a local single-user development tool. It runs on the user's own
machine, with the user's own filesystem and network permissions. There is
no remote attacker model in scope.

What is trusted:
- The user.
- The local `opencode` binary and its model providers (`openrouter.ai` in
  the current configuration).
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
Goal sessions run opencode with the user's filesystem permissions —
there is no sandboxing.

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

The `coding-standards` goal lives in `~/.tinker/`, a global location.
If session state were stored in the global TOML, every project that
picked it up via the multi-dir merge would resume the same opencode
session — leaking the history of one project into another.

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

### T4. opencode goal sessions run non-interactively and cannot respond to approval prompts

Tinker runs opencode as a subprocess with stdin piped to the session
message. opencode's interactive per-tool approval UI cannot receive
input in this configuration. Without pre-approval, opencode stalls on
the first file read and aborts the session.

Mitigations:
- Tinker passes `--dangerously-skip-permissions` to every opencode
  subprocess. This auto-approves tool calls that are not explicitly
  denied in the user's `opencode.json`. Explicit deny rules still apply.
- The actual protection boundary is the goal scope (agents work within
  their assigned goal) and the user's review of changes before commit.
  opencode's interactive prompt is redundant for an automated tool where
  every agent run is user-initiated.
  → test: `test_security_t4_skip_permissions_flag_present`.

### T5. Stderr from opencode could corrupt the TUI

`opencode` writes log lines to stderr. While tinker is in the alternate
screen mode (raw terminal, no scrollback), unfiltered stderr would
overwrite the rendered UI and break the user's ability to interact.

Mitigations:
- The opencode subprocess stderr is piped and captured. Any stderr content
  is appended to the session's chunk stream so it appears in the session
  log rather than leaking to the terminal. Error events also arrive through
  opencode's structured `--format json` stream and are surfaced as system
  messages in the REPL.
  → test: `test_security_t5_stderr_is_captured`.
- The Claude backend subprocess also pipes and captures stderr. Any
  captured content is re-injected into the session's error stream rather
  than leaking to the terminal.
  → test (claude): `test_security_t5_stderr_is_captured_not_leaked`.
