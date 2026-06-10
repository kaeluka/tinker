# AGENTS.md — tinker project index

tinker is a goal-directed coding assistant: goals live as TOML files under `.tinker/goals/`, each one drives a per-goal agent session, and a TUI surfaces the live conversation alongside the goal tree. All agent-to-agent communication happens via `<@goal-id>…</@goal-id>` tag envelopes routed through the session registry.

---

## Core agents

These shared agents have no source-file mapping — they are the agents themselves, reachable via `<@id>…</@id>` from any goal session.

| agent | role |
|-------|------|
| `@tend` | Intent and *should*: holds the goal tree, answers what a goal means, conducts the interview, dispatches goal agents. Route intent questions and goal-content queries here. |
| `@rummage` | Precision backstop for code reality, technical validation, and counterexamples: every input is hypothesis material; execution is the only thing that closes a claim. Works the cycle Anchor → Hypothesize → Falsify → Integrate; falsification dispatched to a fresh sub-session. Scratch tests, fuzz harnesses, and instrumentation marked `tinker-test-case:`. Preauthorized to run code. Prefers whole-program runs with DI mocks over isolated modules; uses `lsp` for call-graph navigation, `webfetch`/`websearch` for external library detail. Never classifies intent-gap vs code-drift and never dispatches repairs — surfaces behavioral truth with precise locators to the triggering agent and to `@tend` when a goal claim is implicated. Output is hybrid: behavioral understanding (with execution citation) at the top, investigation logbook below; residual limits stated honestly. Route code-reality questions and spec↔code validation here. |
| `@jog` | Discrepancy finding: reads two redundant sources via read-only peer queries, runs forward coverage and backward provenance checks, and writes findings to `.tinker/discrepancies/`. Triggered unconditionally after each rummage dispatch. |

---

## Goal → source mapping

### tui
Terminal interface: two-column layout (conversation pane left; goal-list / goal-text / session-log stacked right), active-session switching, queue and trigger-reason display, scroll, mouse support, reason-prompt modal. Ephemeral sub-sessions appear in the goal list as soon as their opening `<@id|label>` tag is detected during streaming (not at turn completion).

- `src/repl_buffer.rs` — retained REPL buffer (`ReplBuffer`): incremental `Message`→`Line<'static>` conversion with per-message caching (`CachedEntry`), lazy recomputation of wrapped-line counts for the streaming last entry, and `viewport_lines()` culling so `draw_repl` never calls `Paragraph::line_count` on the full history. `build_message_lines()` is the single source of truth for message rendering (user/system/agent labels and styles). Includes spec tests for caching, idempotency, streaming updates, session-switch invalidation, and security edge cases (empty messages, zero width, very long text).
- `src/tui.rs` — all rendering: `pane_rects()` (layout geometry shared with mouse handler), `draw()`, `draw_repl()` (uses the retained `ReplBuffer` for incremental build and viewport culling; scroll state tracked via `ScrollState::cache_key`), `draw_goal_tree()` (goal-list rows rendered from `app.flat_items()`, indented via a `depth_by_id` map seeded from `flatten_tree()` for permanent goals then extended with ephemeral depths derived from `session_parent_id()`; `session_base_id()` used to mark running goals; goal-text pane: `summary + [kind · tier]` header in muted gray above the `description` body), `draw_log()` (log pane title uses goal `id` directly), `input_pane_layout()`, `running_label()`, `flatten_tree()` (depth-first traversal of the permanent goal tree, used internally to seed the depth map; not used directly for the goal list). The non-functional spec requires that REPL scroll behaviour remain smooth even when the conversation history grows large; this is delivered by `ReplBuffer`'s incremental cache and by avoiding full-buffer line-count recomputation every frame.

### goal-agents
Agent-architecture runtime: uniform per-goal session registry, `@goal-id` universal dispatch, framework preamble (VCS rules, `.tinker/` prohibition, implementation-ownership mandate, message-passing and neighbor-consultation sections), session init messages, startup-silence for `tend`, parse-correction loop.

- `src/goal_session.rs` — framework preamble constants (`VCS_RULES`, `TINKER_DIR_WRITE_RULES`, `IMPLEMENTATION_OWNERSHIP_MANDATE`, `MESSAGE_PASSING_AND_PROGRESS_SECTIONS`, `NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE`, `FRESH_DISPATCH_INSTRUCTIONS`), `TRIGGER_REASON_MARKER` (sentinel `\x01` char prefixed to trigger-reason messages for TUI identification), `session_init_message()` (assembles the full system prompt — goal description + framework preamble + neighbors — delivered via the backend's native mechanism on the first turn), `build_neighborhood_table()`, `run_goal()`, `run_silent()`, `SessionEvent`; also `fresh_subsession_init_message()` and `fresh_subsession_lean_init_message()` (init builders for ephemeral sub-sessions). `goal_agent_framework_preamble()` and `goal_agent_lean_init_message()` are retained for tests only (`#[cfg_attr(not(test), allow(dead_code))]`) — not called in production. `MESSAGE_PASSING_AND_PROGRESS_SECTIONS` includes an "Acknowledgements close the exchange — no reply needed" instruction: receiving a pure acknowledgement means the exchange is complete; replying again invites a loop. Note: tier defaults are now kind-aware — behavior goals default to high tier, feature goals default to mid tier, and low tier is used only when explicitly set.
- `src/main.rs` — session registry (`running_sessions`, `SpawnGoalRequest`), `FreshSessionConfig` (ephemeral sub-session spawn config), `parse_fresh_dispatches()` (extracts `<@goal-id|label>…` envelopes), ephemeral-session spawn/retire logic in the main event loop (`ephemeral_sessions` tracking, `fresh_session_counter`), `dispatch_peer_consultations()` (tracks ALL dispatched recipients — including interactive agents tend/rummage/jog — in `running_sessions` so the batch-end retirement check waits for pending deliveries; delivery message uses plain text with a conditional reply prompt — "If you complete work based on this message: once you are done with this, reply via @{}" — rather than a bare `<@id>…</@id>` envelope, which would be re-parsed as a spurious dispatch), `handle_session_event()`, `tend_*` prompt builders, main event loop; goal agents and fresh sub-sessions both use `lean_init = true`: the per-call system prompt carries all session-invariant context, and the first turn is the trigger reason only (no `session_init_message` as user text), except tend which uses a full init message in the user turn

### peer-consult
Agent-to-agent @-block messaging channel: envelope detection in the finalized reply, tag-boundary and code-fence scoping, guaranteed delivery to recognized IDs, and the system message surfacing each exchange to the user.

- `src/main.rs` — `parse_at_commands()` (extracts `<@id>…</@id>` envelopes from a finalized reply; code-fence and mid-line exclusion rules ensure only intentionally emitted envelopes are dispatched, not quoted or illustrative syntax); `dispatch_peer_consultations()` (routes non-fresh envelopes to each named session's input channel using plain-text delivery with a conditional reply prompt rather than a bare `<@id>` envelope, preventing re-parsing as a spurious dispatch; tracks all recipients in `running_sessions` so batch-end retirement waits for in-flight deliveries); `known_agent_ids()` (builds the routing table of recognized agent IDs from the current session registry)

### fresh-agents
Fresh-dispatch mechanism and global batch state: goal agents dispatch to ephemeral sub-sessions for tasks with inherent parallelism; each sub-session starts clean, may itself coordinate further sub-sessions (depth is unbounded), and remains inspectable until the next batch. This goal owns the batch state concept: the system is **active** whenever any work is in flight (a session is processing, a reply is being routed, or a user message is pending) and **idle** when nothing is outstanding; all completed ephemeral sub-sessions are retired at the idle→active transition that opens a new batch. Ephemeral sub-sessions appear in the TUI as soon as their opening tag is detected during streaming, not at turn completion.

- `src/goal_session.rs` — `FRESH_DISPATCH_INSTRUCTIONS` (protocol section injected into both regular goal-session init messages and fresh sub-session init messages, but excluded from the framework preamble constant; instructs agents to default to dispatching any decomposable sub-task to a fresh sub-session rather than doing it inline; includes an explicit prohibition on tool-level agent-spawning APIs such as the `Agent` tool — those calls bypass the harness entirely, making sub-sessions invisible to the goals pane and the event log and breaking batch accounting); `fresh_subsession_init_message()` and `fresh_subsession_lean_init_message()` (build the init for an ephemeral sub-session: ephemeral identity, parent-goal id, optional correlation label, task body, and `FRESH_DISPATCH_INSTRUCTIONS` so the sub-session can itself act as a coordinator and dispatch further sub-sessions)
- `src/main.rs` — `FreshSessionConfig` (carries `session_id`, `dispatcher_id`, `label`, neighbor list); `parse_fresh_dispatches()` (extracts complete `<@id|label>…</@id|label>` envelopes at Done time; **scoping rule**: only top-level envelopes count — envelopes inside fenced code blocks ` ``` `/`~~~` are ignored as illustrative, and envelopes that don't begin at the start of a trimmed line are treated as explanatory prose and skipped); `scan_opening_tags()` (detects opening `<@base_id|label>` tags during streaming without requiring a closing tag — used at Chunk time to pre-announce ephemeral sessions before the turn completes; applies the same code-fence and mid-line exclusion rules as `parse_fresh_dispatches()`); Chunk-time pre-announcement path in `handle_session_event()` (each new opening tag seen during streaming is pre-registered with a `~counter` ID into `running_sessions`, `ephemeral_sessions`, and `app.pending_fresh_announcements`); Done-time reconciliation path (matches complete envelopes to pre-announced IDs in order — reuses the pre-assigned ID so the TUI row never flickers; spawns a fresh ID for any envelope that arrived without a prior Chunk scan; silently removes pre-announced entries whose closing tag was never emitted); batch-start retirement at the **fresh-session spawn path**: fires only when all three conditions hold — (1) dispatcher is a permanent goal agent (not an ephemeral coordinator), (2) no ephemeral session is currently in `running_sessions`, (3) the dispatcher itself is not in `running_sessions` (no sub-session reply still in transit); retired IDs are removed from `session_senders`; `session_base_id()` resolves the permanent goal ID from an ephemeral coordinator dispatcher so init context and tier selection are always drawn from the root permanent goal

### backends
Pluggable LLM backend abstraction: `OpenCodeRunner` trait wired at the composition root, two implementations (opencode default with system prompt delivered via ephemeral agent file; claude via `--claude` flag with streaming JSON), per-backend model-tier defaults, persona delivery, CLI-error re-injection. `--default-model` bypasses tier resolution, using the opencode backend's built-in default model for every role.

- `src/cap.rs` — `OpenCodeRunner` trait (including the `system_prompt: Option<&str>` parameter on `run()` — passed on new sessions to deliver session-invariant context via the backend's native mechanism; `None` on resumed sessions), `Filesystem` trait, `Chunk` type alias
- `src/opencode.rs` — `RealOpenCodeRunner` (opencode backend); `create_agent_file_in_dir(agents_dir, system_prompt) -> (NamedTempFile, stem)` (`pub(crate)` helper — creates the `<work_dir>/agents/tinker-*.md` temp file and returns it alongside its stem; caller must keep the `NamedTempFile` alive until the subprocess exits, at which point it is auto-deleted by the tempfile crate); `RealOpenCodeRunner::run()` calls this helper when `system_prompt` is `Some` and `session_id` is `None`, then passes `--agent <stem>` to opencode; resumed sessions skip agent-file creation entirely; model constants `TINKER_MODEL`, `GOAL_MODEL`, `SCHEDULER_MODEL`; `opencode_command(model, session_id, work_dir, agent_name)`, `opencode_args(model, session_id, agent_name)`; streaming JSON parser (`opencode run --format json`); `short_tool_summary()`
- `src/claude.rs` — `ClaudeRunner` (claude backend); `run()` resolves `effective_sp = system_prompt.or(self.system_prompt.as_deref())` so the per-call prompt (goal-specific, supplied on the first turn by `goal_agent_loop`) takes priority over the struct-level one (used for tend's fixed scope boundary); error-reinjection path uses the struct-level prompt only (or `None` for goal agents); same model constants (`TINKER_MODEL` pinned to `claude-opus-4-8`); `claude_command()`, `claude_args()`; streaming JSON parser (`--output-format stream-json --verbose`)

### model-config
User-editable `.tinker/config.toml`: six optional per-backend tier slots (high/mid/low for opencode and claude), absent slots fall back to built-in defaults, self-documenting starter template written once on first run.

- `src/config.rs` — `ModelConfig`, `BackendModelConfig`, `load_model_config()`, `write_starter_template()`

### goal-storage
On-disk TOML format at `.tinker/goals/<id>.toml`: ancestor-directory merge (cwd-most wins on duplicate ids), malformed-file reporting without blocking siblings, transparent symlink following, `[[children]]` / `[[related]]` link shape.

- `src/goal.rs` — `Goal` (fields include `kind: Option<String>` — `"feature"` or `"behavior"`, optional on legacy goals), `RelatedLink`, `ChildLink`, `GoalNode`; `GOAL_SCHEMA_KEYS_ORDER` (single source of truth for schema key order used in prompts and parse-error correction); `discover_tinker_dirs()`, `load_all_goals()`, `load_goals()`, `save_goal()`, `build_tree()`, `build_compact_index()` (emits `kind` in each compact index entry), `build_full_text_index()`

### tier-edit

Tier defaults are now kind-aware: behavior goals default to high tier, feature goals default to mid tier, and low tier is used only when explicitly set.
TUI tier-change interaction: `t` on a focused goal cycles its tier (absent/mid → high → low → absent/mid) and writes the updated `tier` field immediately to the goal's source TOML file; the goal-detail header's `[kind · tier]` tag reflects the new value at once.

- `src/main.rs` — `cycle_tier()` (three-value cycle logic), `KeyAction::CycleTier(goal_id)` variant, key-event dispatch (`t` in the goal-tree returns `CycleTier` for any goal with a `source_path`), and the `CycleTier` event-loop handler that mutates `app.goals` in-place, serializes via `toml::to_string_pretty()`, and writes via `fs.write()`

### goal-placement
Placement reviewer: when tend creates or edits a goal, checks that it lives in the directory matching its scope — packaged (`~/.tinker/goals/`) for content valid across any tinker project, project-local (`.tinker/goals/`) for content specific to this project — and flags drift when edits make a packaged goal project-specific.

- No dedicated source file — pure behavior goal enforced through tend's session prompting at write time.

### cleanup-hook
Pre-session cleanup: scans the project tree for `tinker-test-case:` markers left by rummage, dispatches a cheap agent to remove each one, retries up to 3 times, blocks the session if any marker survives.

- `src/cleanup.rs` — `run_cleanup()`, `find_marker_files()`, `build_cleanup_prompt()`, `file_contains_marker()`; `MARKER` constant, `CleanupOutcome`, `MAX_RETRIES`

### tend-introspection
Runtime introspection substrate: append-only event log at `.tinker/logs/runtime.jsonl` capturing turns, goal-session lifecycle, `@`-dispatch, cleanup, goal-file changes, TUI state, and batch state transitions (idle→active and active→idle) as first-class events; batch transitions also emit as user-visible system messages; semantic state snapshot at `.tinker/state/runtime.json` written async and non-blocking.

- `src/logger.rs` — `LogSender`, `start_logger()`, `LogEvent` (all event variants), `StateSnapshot`, `QueueEntry`, `ScrollOffsets`, `apply_to_state()`, `hash_string()`, `count_tool_calls()`, `extract_modified_files()`

### startup-args
CLI argument handling: validates incoming flags against the recognised set, unknown-argument error path (print help + exit 1), `--help`/`-h` handler, help text content.

- `src/main.rs` — `print_help()` and the argument parsing block in `main()`

### prompt-storage
On-disk prompt storage: LLM-facing prompt strings extracted from inline source literals into the `prompts/` directory, embedded at compile time via `include_str!` and filled by `src/prompts.rs`.

- `src/prompts.rs` — prompt loading and templating module (`include_str!`s each file under `prompts/` and interpolates `{PLACEHOLDER}` variables via `str::replace`)
- `prompts/` directory — extracted prompt files grouped by consumer module (`goal_session/`, `backends/`, `main/`, `cleanup/`, `config/`), plain text with `{PLACEHOLDER}` interpolation syntax

---

## Shared infrastructure (no dedicated goal)

- `src/app.rs` — `GoalListItem` enum (`Goal(Goal)` | `Ephemeral(String)`) representing a single row in the goal list; `session_base_id(id)` (strips all `~{counter}` suffixes, returning the root permanent goal ID — used for running-marker lookup); `session_parent_id(id)` (strips only the trailing `~{counter}` segment, returning the immediate parent — used for nesting ephemeral rows); `flat_items()` (builds the depth-first ordered list of `GoalListItem` values with each ephemeral inserted immediately after its immediate parent, supporting unbounded nesting depth); `App` (central mutable state shared by TUI and event loop; includes `ephemeral_sessions: HashSet<String>`, `ephemeral_sessions_ordered: Vec<String>`, `fresh_session_counter: u64`, and `pending_fresh_announcements: HashMap<String, Vec<(String, Option<String>)>>` — a map from base goal ID to pre-announced ephemeral sessions detected during streaming before their turn completes), `Message`, `Role`, `Focus`, `Phase`, `ModalState`, `ScrollState`
- `src/realfs.rs` — `RealFilesystem` (implements `Filesystem` for the real OS; the test double lives in `src/test_utils.rs`)
- `src/test_utils.rs` — `MockFilesystem` and helpers used by unit tests
