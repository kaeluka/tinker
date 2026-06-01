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
If its `session_id` were stored only in that global TOML, every project
that picked it up via the multi-dir merge would resume the same opencode
session — leaking the history of one project into another.

Mitigations:
- Session IDs are stored in a per-project `<cwd>/.tinker/sessions.toml`
  map (`goal_id → session_id`), which overrides the goal TOML's
  `session_id` field on load.
- The goal TOML's `session_id` is only read as a fallback for projects
  that have not yet established their own session for a given goal.
  → test: `test_security_t2_session_per_project`.

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

### T4. opencode could be invoked with auto-approval of destructive tools

opencode supports `--dangerously-skip-permissions` which bypasses the
user's per-tool approval prompts. If tinker passed this flag, a goal
session could quietly delete or rewrite arbitrary files.

Mitigations:
- Tinker never passes `--dangerously-skip-permissions` in any of its
  three runner roles. The flag list in `opencode.rs::run` is fixed
  (`run`, `--format json`, `-m <model>`, `-s <session>`, and nothing
  else permission-relevant).
  → test: `test_security_t4_no_skip_permissions_flag`.

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
- The Claude backend subprocess nulls stderr (stderr not captured); errors
  are surfaced through the structured JSON stream.
  → test (claude): `test_security_t5_stderr_is_nulled`.
