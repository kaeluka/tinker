# tinker — autonomous coding assistant

Tinker is a Rust CLI that turns user intent into code through a loop of **goals** (standing intent files), an **orchestrator** (interviews the user, owns reactive scheduling, inspects its own runtime state), **goal sessions** (autonomous LLM agents that write code), and a **cleanup hook** (removes investigation markers). The user interacts via a three-pane **TUI**.

---

## Goals

### Root: `the-false-promise`
The foundational diagnosis that justifies tinker's architecture. Current coding assistants promise developers they no longer have to write code, but their architecture forces developers to keep reading it — they launder ad-hoc AI implementation guesses into a "quasi-specification" indistinguishable from human intent. Tinker rejects this by separating Intent from Implementation into a durable goal layer. `creative-process`, `user-system-interaction`, and `readme-maintainer` articulate the solution derived from this diagnosis.

📁 `.tinker/goals/the-false-promise.toml` — goal definition

#### Child: `creative-process`
Defines how tinker converts user intent into a working system: *iterative dialectical discovery*. The user is a *reflective practitioner* (Schön) — injecting tacit, situated judgment (knowing-in-action) that cannot be codified in advance. The orchestrator is a *convention engine* — excellent at executing rules sequentially and cross-referencing goals, poor at judgment that requires being embedded in practice. Goals are the bridge: the user's intuition, surfaced through dialogue and crystallized into explicit conventions the engine can then execute and cross-reference. The conversion is lossy — conventions degrade as situations drift — so the dialectic continues rather than terminating once a goal is written. The orchestrator's dual duty: faithfully execute conventions AND surface inflection points where convention runs out; at those points it stops and asks, not extrapolates. Tone is downstream of role: direct, invested, no deference layer. The human is the *termination condition* for the refinement loop — not replaceable by a second language model, which has no real stake, no continuity of the underlying problem, and cannot supply the external bounding that ends the loop.

📁 `.tinker/goals/creative-process.toml` — goal definition  
📁 `.tinker/notes/notes.md` — argument for why two LLMs cannot replace the human in the dialectic

##### Grandchild: `goal-craft`
Operational rules for writing goal content so it works as both standing intent and as the prompt that agents read. Four tactics: sparseness (operationalized by the anchor test — a bullet belongs only when user intent, an external dependency, or a hard-won negative constraint anchors it), positive framing (WHAT not whatn't), orthogonality of content (goals are free-standing; cross-cutting concerns live in links, not duplicated body text), and goals-are-prompts (five prompt-quality rules: define terms before first use, consistent vocabulary, reference sibling goals by id, examples earn their place, lead with the load-bearing point). Enforcement is read-time: the orchestrator cites this goal when it spots a candidate violation during interviews and cross-goal alignment.

📁 `.tinker/goals/goal-craft.toml` — goal definition

#### Child: `user-system-interaction`
The architectural boundary between the user and tinker. The user holds the *should* — intent, decisions, vetted goals, observations under investigation, and mutually established terms. Tinker holds the *is* — the current state of all files, processes, build outputs, caches, and side effects of any past action. When tinker's reasoning depends on the *is*, it verifies by observation; inference does not substitute for checking. The user is exposed to exactly two surfaces: the *interface* of any tool they touch (invocation path, flag names, env vars, start/stop) and *internals filtered through shared language* (features, architectural concepts, observable behavior). Raw code identifiers never surface without that filtering. `user-persona` and `shared-language` are downstream applications of this principle.

📁 `src/orchestrator.rs` — "Should and is", "What surfaces to the user", "Downstream applications" sections in the orchestrator agent prompt; `test_spec_user_holds_should_tinker_holds_is`, `test_spec_is_state_verified_by_observation_not_inference`, `test_spec_two_surface_exposure_and_downstream_framing`  
📁 `.tinker/goals/user-system-interaction.toml` — goal definition

##### Grandchild: `user-persona`
Formalizes the design philosophy: the user is an experienced developer who no longer looks at source code. Tinker acts as a compiler — surfacing behavior through goals and tinkering, never relying on "trust me bro" readings of source.

📁 `.tinker/goals/user-persona.toml` — the persona definition

##### Grandchild: `shared-language`
Cross-cutting rule that applies to every user-facing surface tinker produces: orchestrator chat, batch summaries composed from session reports, and human-readable documentation goal sessions write into the project tree (READMEs, guides, human-targeted code comments). Default to conceptual/architectural descriptions rather than code identifiers. Artifacts explicitly authored for LLM consumption (e.g. `AGENTS.md` produced by `project-indexer`) are exempt — they retain full technical fidelity. Goal sessions inherit this rule automatically via sibling-goal injection; the orchestrator does not repeat it per session. The orchestrator fully owns `.tinker/state/vocabulary.txt` (updated silently when the user mentions a term or a term is written into a goal file; never mentioned to the user); goal sessions read the file but do not write it. When a technical term not in the vocabulary must be used, it must be anchored to a known architectural concept.

📁 `src/orchestrator.rs` — `## Shared language and vocabulary` section in the orchestrator agent prompt; `test_spec_orchestrator_prompt_defaults_to_conceptual_explanations`, `test_spec_orchestrator_prompt_names_vocabulary_state_file`, `test_spec_orchestrator_prompt_owns_vocabulary_file_silently`, `test_spec_orchestrator_prompt_states_vocabulary_entry_triggers`, `test_spec_orchestrator_prompt_requires_anchoring_unknown_terms`, `test_spec_shared_language_covers_documentation_written_by_goal_sessions`, `test_spec_shared_language_llm_targeted_artifacts_exempt`, `test_spec_shared_language_goal_sessions_inherit_via_sibling_injection`, `test_spec_shared_language_goal_sessions_read_vocab_not_write`  
📁 `src/main.rs` — `fs.mkdir_all` for `.tinker/state/` at startup

#### Child: `readme-maintainer`
A goal that reactively maintains the human-facing `README.md` at the project root, synthesizing tinker's philosophical foundation — the diagnosis of cognitive debt, the dialectical process, and the strict state-ownership boundary — into a cohesive narrative for developers seeing tinker for the first time. The README is generated dynamically from the current state of the philosophical goal tree, preventing it from drifting into a stale artifact. Adheres strictly to the shared-language standard.

📁 `.tinker/goals/readme-maintainer.toml` — goal definition  
📁 `README.md` — the maintained file

### Root: `orchestrator`
The conversational agent the user talks to. Does four interdependent things: manages goals (interview protocol: one question per turn; playback before write is epistemically required — only the user holds the original intent, so only the user can validate whether the articulated goal matches it; the orchestrator never infers consent from silence, from a follow-up that doesn't push back, or from any other indirect signal); tinkers (writes temporary investigation code to answer questions from execution rather than inference); reframes (when the current goal isn't the right question, names the move explicitly — "I'm reframing here: we were treating this as X, I now think it's Y" — and surfaces it before continuing, never silently redirects); and inspects its own runtime state via the introspection log and state file. Never writes production code directly. After each goal-session batch finishes, the orchestrator also owns reactive scheduling: it reads the per-goal summaries and emits `/run` lines for any downstream goals that should react, as part of its batch-summary reply. During goal creation, editing, and cross-goal alignment, the orchestrator applies `goal-craft`'s content-quality tactics (anchor test, positive framing, orthogonality, goals-as-prompts) and surfaces candidate violations to the user rather than auto-fixing. When editing a goal, the orchestrator also runs a spec-cascade audit — identifying passages in related goals or the orchestrator prompt that go stale as a result, and bundling those implications into the same playback turn and `/run` reason.

📁 `src/orchestrator.rs` — init prompt, event types, `send_message`, `orchestrator_agent_content` (includes post-batch reactive scheduling instructions)  
📁 `src/main.rs` — `handle_orch_event`: orchestrator Done → goal reload, parse-error correction, `/run` dispatch; `build_batch_summary_request`: sends summaries to orchestrator and instructs it to emit `/run` lines; `handle_goal_event::SummaryDone`: fires batch summary request directly when queue drains

#### Child: `orchestrator-agent`
Enforces the orchestrator's boundaries by running it under a custom `opencode` agent profile (`tinker-orchestrator`) that strips out the default software-engineer persona and denies access to `task`/`todowrite` tools. Silently installed into `~/.config/opencode/agents/` on startup.

📁 `src/opencode.rs` — `--agent` flag passed to opencode CLI, `RealOpenCodeRunner::with_agent`  
📁 `src/orchestrator.rs` — `orchestrator_agent_content` (static agent file text)

#### Child: `orchestrator-notes`
The orchestrator silently appends timestamped entries to `.tinker/notes/notes.md` throughout conversation — recording friction (user corrections or repetition), surprise (unexpected behavior), reframes (pivot from one framing to another), recurring threads, and explicit "remember this" requests from the user. Note-taking is invisible: the orchestrator never interrupts conversation to mention it. This goal builds the substrate only; retrieval and reflection are a later goal.

📁 `src/orchestrator.rs` — `## Note-taking` section in the orchestrator agent prompt (trigger conditions, tone: "observe without judging"); `test_spec_orchestrator_notes_names_correct_file_path`, `test_spec_orchestrator_notes_all_triggers_named`, `test_spec_orchestrator_notes_observe_without_judging_tone`, `test_spec_orchestrator_notes_writing_is_silent`, `test_spec_notes_dir_created_at_startup`  
📁 `src/main.rs` — `fs.mkdir_all` for `.tinker/notes/` at startup (line 75)  
📁 `.tinker/notes/notes.md` — append-only notes file (created on first write)

#### Child: `orchestrator-introspection`
Runtime introspection substrate: gives the orchestrator structured access to the *is* tinker already holds. Writes two artifacts: an append-only JSONL event log at `.tinker/logs/runtime.jsonl` (per-turn token usage, goal-session lifecycle with observables, full message history including system messages, TUI state changes) and a debounced semantic state snapshot at `.tinker/state/runtime.json` (selected goal id, focus, scroll offsets, queue snapshot, rolling session usage totals). Producers call `LogSender::emit` (synchronous, non-blocking); a single background task drains the channel and batches writes (every 100ms or 10 events, whichever first); the state snapshot is debounced ~100ms after the last state-changing event. The orchestrator reads both files via its existing `Read`/`Bash`/`Grep` tools — no new tool plumbing needed. Goal sessions never read these files. The log grows without rotation; the user clears it manually if needed.

📁 `src/logger.rs` — `LogEvent` enum (17 kinds: orchestrator session/turn/message events, goal-session lifecycle, cleanup-hook, run-command, TUI state), `LogEntry`, `LogSender`, `StateSnapshot`, `UsageInfo`, `QueueEntry`, `ScrollOffsets`, `start_logger`, `noop_sender`, `parse_usage_from_text`, `count_tool_calls`, `extract_modified_files`; `test_spec_log_line_has_required_ts_kind_source_fields`, `test_spec_event_kinds_are_snake_case`, `test_spec_goal_session_finished_carries_observable_set`, `test_spec_opencode_backend_has_null_usage`, `test_spec_message_events_carry_full_text`, `test_spec_orchestrator_system_message_received_logs_source_and_content`, `test_spec_no_rotation_log_appends`, `test_spec_state_debounce_via_apply_to_state`, `test_spec_state_accumulates_usage_from_finished_sessions`, `test_spec_parse_usage_from_text_extracts_all_fields`, `test_spec_parse_usage_returns_none_for_opencode_output`  
📁 `src/main.rs` — `mod logger`, `fs.mkdir_all` for `.tinker/logs/` at startup, `logger::start_logger(...)` call at composition root, initial `OrchestratorSystemMessageReceived` event emitted on start  
📁 `.tinker/logs/runtime.jsonl` — append-only event log (created on first write)  
📁 `.tinker/state/runtime.json` — debounced semantic UI snapshot (created on first state-changing event)

#### Child: `orchestrator-triggers`
When the orchestrator wants a goal session to run — whether from a just-edited goal or from its post-batch reactive scheduling pass — it emits `/run <goal-id> <reason>` in its chat output. Tinker scans finalized messages for those lines and dispatches a goal session for each, with the reason threaded through as context. Replaces both the old `change_log` mechanism and the retired per-goal scheduler component.

📁 `src/main.rs` — slash command parsing in `handle_orch_event::Done` and keyboard handler  
📁 `.tinker/goals/orchestrator-triggers.toml` — goal definition

#### Child: `goal-structure-standard`
Guides the orchestrator's goal-tree decisions: Merge (modify existing), Nest Down (create child), Nest Up (create parent), New Root (sibling), or Retire (delete when a reframe makes the goal obsolete — user confirms). Uses a "level of detail coherence" heuristic — if merging breaks the zoom level, extract into a child goal instead. Two write-time coherence rules enforced on every edit: (1) parent goals must summarize their children sufficiently to support drill-down filtering during cross-goal alignment; (2) goals may carry a symmetric `related` field listing cross-cutting goals and per-side reasons — both ends must list each other, reason text may differ per side, transitivity is not enforced. Carries a `related` back-link to `goal-craft` (content-quality rules interact with structural rules at write time).

📁 `.tinker/goals/goal-structure-standard.toml` — the standard definition  
📁 `src/goal.rs` — `RelatedLink` struct, `Goal.related` field, `GOAL_SCHEMA_KEYS_ORDER` (includes `related (optional)`)  
📁 `src/main.rs` — `goals_summary` injection appends `[related: id: "reason", ...]` when non-empty, so the orchestrator sees cross-cutting links on every turn

### Root: `goal-sessions`
Launches one LLM session per goal at a time, driven by a cheaper model than the orchestrator. On first run, receives the full text of every sibling goal (so coding-standards etc. are in context). Every dispatch carries an explicit reason: for automated runs (goal edits or reactive scheduling), the orchestrator provides it via `/run` output; for manual runs, the user is prompted. Produces a structured summary (accomplishments, design changes, decisions, try-it) that must explicitly list every `test_spec_` function created or modified — the mechanism that enforces the Tests-as-guardrails standard. Session IDs and the pending queue are kept in memory only — forgotten on restart.

The queue is a FIFO `VecDeque<(Goal, Option<String>)>` that does **not** deduplicate by goal-id: the same goal can appear multiple times, each with its own reason. Multiple `/run` lines for the same goal (across turns or from the user) each enqueue a separate entry and run in order.

📁 `src/goal_session.rs` — `run_goal`, `goal_init_message`, `goal_resume_message`, summary request  
📁 `src/main.rs` — goal runner task (cleanup → `run_goal`)

#### Child: `cleanup-hook`
Before every goal session, scans the project tree for `tinker-test-case:` markers (line-anchored at comment delimiters). Dispatches a cleanup agent to remove/revert marked code. Retries up to 3×; if still dirty, blocks the goal session and reports files to the user.

📁 `src/cleanup.rs` — marker detection, file walking, retry loop, cleanup prompt builder

### Root: `goal-storage`
Goals are TOML files under `.tinker/goals/<id>.toml`. Ancestor `.tinker/` directories merge with the cwd's; cwd-most wins on duplicates. A malformed file is reported as an error but doesn't block sibling goals. Session IDs are not persisted to disk — they live in memory for the duration of the tinker process only, so goal files remain the sole source of truth across restarts.

📁 `src/goal.rs` — `Goal` struct (`related: Vec<RelatedLink>`, optional in TOML), `RelatedLink` struct, `GOAL_SCHEMA_KEYS_ORDER`, `load_all_goals`, `load_goals`, `save_goal`, `discover_tinker_dirs`, `build_tree`; `test_spec_related_field_absent_loads_as_empty`, `test_spec_related_field_roundtrip_and_summary`  
📁 `src/cap.rs` — `Filesystem` trait (read/write/list)  
📁 `src/realfs.rs` — `RealFilesystem: std::fs` impl  
📁 `src/test_utils.rs` — `MockFs` for tests

### Root: `tui`
Three-pane terminal UI (alternate screen, mouse capture). Left pane: REPL conversation. Right-top pane: selectable goal list + scrollable goal description text below. Right-bottom pane: log output for the selected goal's session. Mouse wheel scrolls whichever pane is under the cursor; auto-follows tail unless the user has scrolled away. When the orchestrator emits `/run` triggers, the resulting system message listing the triggered goals and their reasons is rendered in grey. Trigger reasons are shown in bold in the session log pane.

Goals in the list carry a queue marker when they have running or pending sessions: `▶` for the currently-running goal, `[N]` (1-based global FIFO position) for each goal's next queued entry. Markers render in dim/grey to avoid dominating the goal name. When a goal with pending entries is selected, the goal-text region appends a "── Pending invocations ──" section listing every entry in queue order (running first, then queued), each with its glyph and trigger reason.

📁 `src/tui.rs` — layout (`pane_rects`), draw functions, `goal_queue_marker`, `goal_pending_entries`, scroll state wiring  
📁 `src/app.rs` — `App` state, `ScrollState` (auto-follow logic), `Phase`, `Focus`, `LoopMode`; `goal_queue: VecDeque<(Goal, Option<String>)>` and `active_goal_reason` drive queue rendering  
📁 `src/main.rs:823-989` — mouse/keyboard event handlers

### Root: `coding-standards` (packaged goal)
Cross-cutting standards enforced by convention: capability-based DI (traits for effects, real impls at composition root), ladder of polymorphism (use the simplest mechanism), test marking convention (`test_spec_` / `test_security_`), security.md per project, investigation markers (`tinker-test-case:`).

📁 `packaged-goals/coding-standards.toml` — the standards text  
📁 `src/cap.rs` — capability interfaces  
📁 `src/opencode.rs` — real `OpenCodeRunner` impl (composition-root only)  
📁 `src/cleanup.rs` — marker enforcement mechanism

### Root: `claude-backend`
A `--claude` CLI flag that makes tinker shell out to `claude -p` instead of `opencode`. When set, the orchestrator uses Opus, goal sessions use Sonnet, and the cleanup runner uses Haiku. The orchestrator persona is passed via `--system-prompt` instead of agent file installation.

📁 `src/claude.rs` — `ClaudeRunner` impl, `claude_command`/`claude_args`, model-tier constants, `format_tool_use`  
📁 `src/main.rs` — `--claude` flag detection and runner selection

### Root: `cost-reduction`
Standing investigative goal: identify cost-cutting opportunities in how tinker uses LLMs and keep a persistent observation record across sessions. First substep completed: extended `src/claude.rs` to capture per-run token counts from the Claude CLI's `result` event stream (`ClaudeUsage` struct, `format_usage_line`, `USAGE_LINE_MARKER` sentinel), and wired `src/tui.rs` to render those lines in dim style at the end of each session log. Findings surface in goal-session summaries; the orchestrator folds significant observations into `.tinker/notes/cost-observations.md`. Acting on any finding requires a fresh interview — findings never become goal files on their own.

📁 `src/claude.rs` — `ClaudeUsage`, `format_usage_line`, `USAGE_LINE_MARKER`; `test_spec_usage_deserializes_from_result_event`, `test_spec_usage_line_format_shows_all_four_fields`, `test_spec_usage_line_empty_when_all_zero`  
📁 `src/tui.rs` — `render_log_line` branch for `USAGE_LINE_MARKER` (dim cyan); `test_spec_usage_line_rendered_dim_in_log`  
📁 `.tinker/notes/cost-observations.md` — orchestrator-owned observation log (created on first finding)

### Root: `project-indexer` (packaged goal — this agent)
Maintains `AGENTS.md` at the project root — a goals-oriented index mapping each goal to its source files. Triggers reactively when other goal sessions modify files that affect the map.

📁 `AGENTS.md` — this file  
📁 `packaged-goals/project-indexer.toml` — goal definition



## Architecture notes

- **Composition root** in `src/main.rs`: `RealFilesystem` and three runner instances (one per model tier) are wired at startup. Business logic depends only on traits in `cap.rs`. With `--claude`, `ClaudeRunner` instances replace `RealOpenCodeRunner`.
- **Three model tiers**: orchestrator (smartest), goal sessions (mid), cleanup (cheapest). Default models defined in `src/opencode.rs:15-20`. With `--claude`, uses Claude aliases: opus, sonnet, haiku (defined in `src/claude.rs:15-17`).
- **Event loop** (`main.rs:run_loop`): polls terminal events, drains orchestrator/goal channels, draws TUI on every iteration. When a goal-session batch drains, `handle_goal_event` sends the batch summaries to the orchestrator, which produces the user-facing summary and any reactive `/run` lines in a single reply.
- **Goal tree**: `goal.rs::build_tree` builds a parent-child hierarchy from `parent_id` fields. Flat list for selection; tree for display.
- **Security**: documented in `security.md`. Key mitigations include per-file goal loading isolation (T1), per-project session IDs (T2), ancestor-merger transparency (T3), no permission-bypass flags (T4), stderr nulled from subprocesses (T5).
- **Tests**: inline `#[cfg(test)]` modules in each `src/` file, named `test_spec_*` for spec requirements and `test_security_*` for threat mitigations. `MockFs` in `test_utils.rs` provides an in-memory filesystem for testing.