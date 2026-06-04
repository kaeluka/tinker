# AGENTS.md — tinker project index

tinker is a goal-directed coding assistant: goals live as TOML files under `.tinker/goals/`, each one drives a per-goal agent session, and a TUI surfaces the live conversation alongside the goal tree. All agent-to-agent communication happens via `@<goal-id>` message blocks routed through the session registry.

---

## Goal → source mapping

### tui
Terminal interface: two-column layout (conversation pane left; goal-list / goal-text / session-log stacked right), active-session switching, queue and trigger-reason display, scroll, mouse support, reason-prompt modal.

- `src/tui.rs` — all rendering: `pane_rects()` (layout geometry shared with mouse handler), `draw()`, `draw_repl()`, `draw_goal_tree()` (goal-list rows: bold id + `summary` as `— preview`; goal-text shows `description`), `draw_log()`, `push_message_lines()`, `input_pane_layout()`, `running_label()`, `flatten_tree()`

### goal-agents
Agent-architecture runtime: uniform per-goal session registry, `@goal-id` universal dispatch, framework preamble (VCS rules, `.tinker/` prohibition, implementation-ownership mandate, message-passing and neighbor-consultation sections), session init messages, startup-silence for `tend`, parse-correction loop.

- `src/goal_session.rs` — framework preamble constants (`VCS_RULES`, `TINKER_DIR_WRITE_RULES`, `IMPLEMENTATION_OWNERSHIP_MANDATE`, `MESSAGE_PASSING_AND_PROGRESS_SECTIONS`, `NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE`), `goal_agent_framework_preamble()`, `session_init_message()`, `goal_agent_lean_init_message()`, `build_neighborhood_table()`, `run_goal()`, `run_silent()`, `SessionEvent`
- `src/main.rs` — session registry (`running_sessions`, `SpawnGoalRequest`), `dispatch_peer_consultations()`, `handle_session_event()`, `tend_*` prompt builders, `goal_agent_system_prompt()`, main event loop

### backends
Pluggable LLM backend abstraction: `OpenCodeRunner` trait wired at the composition root, two implementations (opencode default with path-scoped agent files; claude via `--claude` flag with streaming JSON), per-backend model-tier defaults, persona delivery, CLI-error re-injection.

- `src/cap.rs` — `OpenCodeRunner` trait, `Filesystem` trait, `Chunk` type alias
- `src/opencode.rs` — `RealOpenCodeRunner` (opencode backend); model constants `TINKER_MODEL`, `GOAL_MODEL`, `SCHEDULER_MODEL`; `opencode_command()`, `opencode_args()`; streaming JSON parser; `short_tool_summary()`
- `src/claude.rs` — `ClaudeRunner` (claude backend); same model constants; `claude_command()`, `claude_args()`; SSE event parser

### model-config
User-editable `.tinker/config.toml`: six optional per-backend tier slots (high/mid/low for opencode and claude), absent slots fall back to built-in defaults, self-documenting starter template written once on first run.

- `src/config.rs` — `ModelConfig`, `BackendModelConfig`, `load_model_config()`, `write_starter_template()`

### goal-storage
On-disk TOML format at `.tinker/goals/<id>.toml`: ancestor-directory merge (cwd-most wins on duplicate ids), malformed-file reporting without blocking siblings, transparent symlink following, `[[children]]` / `[[related]]` link shape.

- `src/goal.rs` — `Goal` (fields include `kind: Option<String>` — `"feature"` or `"behavior"`, optional on legacy goals), `RelatedLink`, `ChildLink`, `GoalNode`; `GOAL_SCHEMA_KEYS_ORDER` (single source of truth for schema key order used in prompts and parse-error correction); `discover_tinker_dirs()`, `load_all_goals()`, `load_goals()`, `save_goal()`, `build_tree()`, `build_compact_index()` (emits `kind` in each compact index entry), `build_full_text_index()`

### cleanup-hook
Pre-session cleanup: scans the project tree for `tinker-test-case:` markers left by rummage, dispatches a cheap agent to remove each one, retries up to 3 times, blocks the session if any marker survives.

- `src/cleanup.rs` — `run_cleanup()`, `find_marker_files()`, `build_cleanup_prompt()`, `file_contains_marker()`; `MARKER` constant, `CleanupOutcome`, `MAX_RETRIES`

### tend-introspection
Runtime introspection substrate: append-only event log at `.tinker/logs/runtime.jsonl` capturing turns, goal-session lifecycle, `@`-dispatch, cleanup, goal-file changes, and TUI state; semantic state snapshot at `.tinker/state/runtime.json` written async and non-blocking.

- `src/logger.rs` — `LogSender`, `start_logger()`, `LogEvent` (all event variants), `StateSnapshot`, `QueueEntry`, `ScrollOffsets`, `apply_to_state()`, `hash_string()`, `count_tool_calls()`, `extract_modified_files()`

### startup-args
CLI argument handling: validates incoming flags against the recognised set, unknown-argument error path (print help + exit 1), `--help`/`-h` handler, help text content.

- `src/main.rs` — `print_help()` and the argument parsing block in `main()`

---

## Shared infrastructure (no dedicated goal)

- `src/app.rs` — `App` (central mutable state shared by TUI and event loop), `Message`, `Role`, `Focus`, `Phase`, `ModalState`, `ScrollState`
- `src/realfs.rs` — `RealFilesystem` (implements `Filesystem` for the real OS; the test double lives in `src/test_utils.rs`)
- `src/test_utils.rs` — `MockFilesystem` and helpers used by unit tests
