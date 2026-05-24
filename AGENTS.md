# tinker — autonomous coding assistant

Tinker is a Rust CLI that turns user intent into code through a loop of **goals** (standing intent files), **tinker** (interviews the user, owns reactive scheduling, inspects its own runtime state), **goal sessions** (autonomous LLM agents that write code), and a **cleanup hook** (removes investigation markers). The user interacts via a three-pane **TUI**.

---

## Goals

### Root: `the-false-promise`
The foundational diagnosis that justifies tinker's architecture. Current coding assistants promise developers they no longer have to write code, but their architecture forces developers to keep reading it — they launder ad-hoc AI implementation guesses into a "quasi-specification" indistinguishable from human intent. Tinker rejects this by separating Intent from Implementation into a durable goal layer. `creative-process`, `user-system-interaction`, and `readme-maintainer` articulate the solution derived from this diagnosis.

📁 `.tinker/goals/the-false-promise.toml` — goal definition

#### Child: `creative-process`
Defines how tinker converts user intent into a working system: *iterative dialectical discovery*. The user is a *reflective practitioner* (Schön) — injecting tacit, situated judgment (knowing-in-action) that cannot be codified in advance. Tinker is a *convention engine* — excellent at executing rules sequentially and cross-referencing goals, poor at judgment that requires being embedded in practice. Goals are the bridge: the user's intuition, surfaced through dialogue and crystallized into explicit conventions the engine can then execute and cross-reference. The conversion is lossy — conventions degrade as situations drift — so the dialectic continues rather than terminating once a goal is written. Tinker's dual duty: faithfully execute conventions AND surface inflection points where convention runs out; at those points it stops and asks, not extrapolates. Tone is downstream of role: direct, invested, no deference layer. The human is the *termination condition* for the refinement loop — not replaceable by a second language model, which has no real stake, no continuity of the underlying problem, and cannot supply the external bounding that ends the loop.

📁 `.tinker/goals/creative-process.toml` — goal definition  
📁 `.tinker/notes/notes.md` — argument for why two LLMs cannot replace the human in the dialectic

##### Grandchild: `goal-craft`
Operational rules for writing goal content so it works as both standing intent and as the prompt that agents read. Four tactics: sparseness (operationalized by the anchor test — a bullet belongs only when user intent, an external dependency, or a hard-won negative constraint anchors it), positive framing (WHAT not whatn't), orthogonality of content (goals are free-standing; cross-cutting concerns live in links, not duplicated body text), and goals-are-prompts (five prompt-quality rules: define terms before first use, consistent vocabulary, reference sibling goals by id, examples earn their place, lead with the load-bearing point). Enforcement is read-time: tinker cites this goal when it spots a candidate violation during interviews and cross-goal alignment.

📁 `.tinker/goals/goal-craft.toml` — goal definition

#### Child: `user-system-interaction`
The architectural boundary between the user and tinker. The user holds the *should* — intent, decisions, vetted goals, observations under investigation, and mutually established terms. Tinker holds the *is* — the current state of all files, processes, build outputs, caches, and side effects of any past action. When tinker's reasoning depends on the *is*, it verifies by observation; inference does not substitute for checking. The user is exposed to exactly two surfaces: the *interface* of any tool they touch (invocation path, flag names, env vars, start/stop) and *internals filtered through shared language* (features, architectural concepts, observable behavior). Raw code identifiers never surface without that filtering. `user-persona` and `shared-language` are downstream applications of this principle.

📁 `src/tinker.rs` — "Should and is", "What surfaces to the user", "Downstream applications" sections in the tinker agent prompt; `test_spec_user_holds_should_tinker_holds_is`, `test_spec_is_state_verified_by_observation_not_inference`, `test_spec_two_surface_exposure_and_downstream_framing`  
📁 `.tinker/goals/user-system-interaction.toml` — goal definition

##### Grandchild: `user-persona`
Formalizes the design philosophy: the user is an experienced developer who no longer looks at source code. Tinker acts as a compiler — surfacing behavior through goals and tinkering, never relying on "trust me bro" readings of source.

📁 `.tinker/goals/user-persona.toml` — the persona definition

##### Grandchild: `shared-language`
Cross-cutting rule that applies to every user-facing surface tinker produces: tinker chat, batch summaries composed from session reports, and human-readable documentation goal sessions write into the project tree (READMEs, guides, human-targeted code comments). Default to conceptual/architectural descriptions rather than code identifiers. Artifacts explicitly authored for LLM consumption (e.g. `AGENTS.md` produced by `project-indexer`) are exempt — they retain full technical fidelity. Goal sessions inherit this rule automatically via sibling-goal injection; tinker does not repeat it per session. Tinker fully owns `.tinker/state/vocabulary.txt` (updated silently when the user mentions a term or a term is written into a goal file; never mentioned to the user); goal sessions read the file but do not write it. When a technical term not in the vocabulary must be used, it must be anchored to a known architectural concept.

📁 `src/tinker.rs` — `## Shared language and vocabulary` section in the tinker agent prompt; `test_spec_tinker_prompt_defaults_to_conceptual_explanations`, `test_spec_tinker_prompt_names_vocabulary_state_file`, `test_spec_tinker_prompt_owns_vocabulary_file_silently`, `test_spec_tinker_prompt_states_vocabulary_entry_triggers`, `test_spec_tinker_prompt_requires_anchoring_unknown_terms`, `test_spec_shared_language_covers_documentation_written_by_goal_sessions`, `test_spec_shared_language_llm_targeted_artifacts_exempt`, `test_spec_shared_language_goal_sessions_inherit_via_sibling_injection`, `test_spec_shared_language_goal_sessions_read_vocab_not_write`  
📁 `src/main.rs` — `fs.mkdir_all` for `.tinker/state/` at startup

#### Child: `readme-maintainer`
A goal that reactively maintains the human-facing `README.md` at the project root, synthesizing tinker's philosophical foundation — the diagnosis of cognitive debt, the dialectical process, and the strict state-ownership boundary — into a cohesive narrative for developers seeing tinker for the first time. The README is generated dynamically from the current state of the philosophical goal tree, preventing it from drifting into a stale artifact. Adheres strictly to the shared-language standard.

📁 `.tinker/goals/readme-maintainer.toml` — goal definition  
📁 `README.md` — the maintained file

### Root: `tinker`
The conversational agent the user talks to. Does four interdependent things: manages goals (interview protocol: one question per turn; playback before write is epistemically required — only the user holds the original intent, so only the user can validate whether the articulated goal matches it; tinker never infers consent from silence, from a follow-up that doesn't push back, or from any other indirect signal); tinkers (writes temporary investigation code to answer questions from execution rather than inference); reframes (when the current goal isn't the right question, names the move explicitly — "I'm reframing here: we were treating this as X, I now think it's Y" — and surfaces it before continuing, never silently redirects); and inspects its own runtime state via the introspection log and state file. Never writes production code directly. After each goal-session batch finishes, tinker also owns reactive scheduling: it reads the per-goal summaries and emits `/run` lines for any downstream goals that should react, as part of its batch-summary reply. During goal creation, editing, and cross-goal alignment, tinker applies `goal-craft`'s content-quality tactics (anchor test, positive framing, orthogonality, goals-as-prompts) and surfaces candidate violations to the user rather than auto-fixing. When editing a goal, tinker also runs a spec-cascade audit — identifying passages in related goals or the tinker prompt that go stale as a result, and bundling those implications into the same playback turn and `/run` reason.

📁 `src/tinker.rs` — init prompt, event types, `send_message`, `tinker_agent_content` (includes post-batch reactive scheduling instructions)  
📁 `src/main.rs` — `handle_orch_event`: tinker Done → goal reload, parse-error correction, `/run` dispatch; `build_batch_summary_request`: sends summaries to tinker and instructs it to emit `/run` lines; `handle_goal_event::SummaryDone`: fires batch summary request directly when queue drains

#### Child: `tinker-agent`
Enforces tinker's boundaries by running it under a custom `opencode` agent profile (`tinker`) that strips out the default software-engineer persona and denies access to `task`/`todowrite` tools. Silently installed into `~/.config/opencode/agents/` on startup (always overwritten so the installed copy stays in sync).

📁 `src/opencode.rs` — `--agent` flag passed to opencode CLI, `RealOpenCodeRunner::with_agent`  
📁 `src/tinker.rs` — `tinker_agent_content` (static agent file text); `test_spec_tinker_agent_file_frontmatter_denies_task_and_todowrite`, `test_spec_tinker_agent_does_not_deny_write_edit_bash`, `test_spec_tinker_agent_allows_websearch_denies_webfetch`, `test_spec_tinker_static_persona_in_agent_dynamic_goals_in_init`, `test_spec_agent_file_always_overwritten_not_guarded_by_exists_check`

#### Child: `tinker-notes`
Tinker silently appends timestamped entries to `.tinker/notes/notes.md` throughout conversation — recording friction (user corrections or repetition), surprise (unexpected behavior), reframes (pivot from one framing to another), recurring threads, and explicit "remember this" requests from the user. Note-taking is invisible: tinker never interrupts conversation to mention it. This goal builds the substrate only; retrieval and reflection are a later goal.

📁 `src/tinker.rs` — `## Note-taking` section in the tinker agent prompt (trigger conditions, tone: "observe without judging"); `test_spec_tinker_notes_names_correct_file_path`, `test_spec_tinker_notes_all_triggers_named`, `test_spec_tinker_notes_observe_without_judging_tone`, `test_spec_tinker_notes_writing_is_silent`, `test_spec_notes_dir_created_at_startup`  
📁 `src/main.rs` — `fs.mkdir_all` for `.tinker/notes/` at startup  
📁 `.tinker/notes/notes.md` — append-only notes file (created on first write)

#### Child: `tinker-introspection`
Runtime introspection substrate: gives tinker structured access to the *is* it already holds. Writes two artifacts: an append-only JSONL event log at `.tinker/logs/runtime.jsonl` (per-turn token usage, goal-session lifecycle with observables, full message history including system messages, TUI state changes) and a debounced semantic state snapshot at `.tinker/state/runtime.json` (selected goal id, focus, scroll offsets, queue snapshot, rolling session usage totals). Producers call `LogSender::emit` (synchronous, non-blocking); a single background task drains the channel and batches writes (every 100ms or 10 events, whichever first); the state snapshot is debounced ~100ms after the last state-changing event. Tinker reads both files via its existing `Read`/`Bash`/`Grep` tools — no new tool plumbing needed. Goal sessions never read these files. The log grows without rotation; the user clears it manually if needed.

📁 `src/logger.rs` — `LogEvent` enum (17 kinds: tinker session/turn/message events, goal-session lifecycle, cleanup-hook, run-command, TUI state), `LogEntry`, `LogSender`, `StateSnapshot`, `UsageInfo`, `QueueEntry`, `ScrollOffsets`, `start_logger`, `noop_sender`, `parse_usage_from_text`, `count_tool_calls`, `extract_modified_files`; `test_spec_log_line_has_required_ts_kind_source_fields`, `test_spec_event_kinds_are_snake_case`, `test_spec_goal_session_finished_carries_observable_set`, `test_spec_opencode_backend_has_null_usage`, `test_spec_message_events_carry_full_text`, `test_spec_orchestrator_system_message_received_logs_source_and_content`, `test_spec_no_rotation_log_appends`, `test_spec_state_debounce_via_apply_to_state`, `test_spec_state_accumulates_usage_from_finished_sessions`, `test_spec_parse_usage_from_text_extracts_all_fields`, `test_spec_parse_usage_returns_none_for_opencode_output`  
📁 `src/main.rs` — `mod logger`, `fs.mkdir_all` for `.tinker/logs/` at startup, `logger::start_logger(...)` call at composition root, initial `OrchestratorSystemMessageReceived` event emitted on start  
📁 `.tinker/logs/runtime.jsonl` — append-only event log (created on first write)  
📁 `.tinker/state/runtime.json` — debounced semantic UI snapshot (created on first state-changing event)

#### Child: `tinker-triggers`
When tinker wants a goal session to run — whether from a just-edited goal or from its post-batch reactive scheduling pass — it emits `/run <goal-id> <reason>` in its chat output. Tinker scans finalized messages for those lines and dispatches a goal session for each, with the reason threaded through as context. Replaces both the old `change_log` mechanism and the retired per-goal scheduler component.

📁 `src/main.rs` — slash command parsing in `handle_orch_event::Done` and keyboard handler  
📁 `.tinker/goals/tinker-triggers.toml` — goal definition

#### Child: `goal-structure-standard`
Guides tinker's goal-tree decisions: Merge (modify existing), Nest Down (create child), Nest Up (create parent), New Root (sibling), or Retire (delete when a reframe makes the goal obsolete — user confirms). Uses a "level of detail coherence" heuristic — if merging breaks the zoom level, extract into a child goal instead. Two write-time coherence rules enforced on every edit: (1) parent goals must summarize their children sufficiently to support drill-down filtering during cross-goal alignment; (2) goals may carry a symmetric `related` field listing cross-cutting goals and per-side reasons — both ends must list each other, reason text may differ per side, transitivity is not enforced. Carries a `related` back-link to `goal-craft` (content-quality rules interact with structural rules at write time).

📁 `.tinker/goals/goal-structure-standard.toml` — the standard definition  
📁 `src/goal.rs` — `RelatedLink` struct, `Goal.related` field, `GOAL_SCHEMA_KEYS_ORDER` (includes `related (optional)`)  
📁 `src/main.rs` — `goals_summary` injection appends `[related: id: "reason", ...]` when non-empty, so tinker sees cross-cutting links on every turn

### Root: `rummage`
A sibling chat agent to tinker dedicated to understanding program behavior. The user opens a rummage session when something needs explaining — a thrown exception, surprising output, a flow they don't fully trust, or code they're about to change and want to understand first. Rummage operates in one of three explicit modes — debugging (a failure to explain), reconnaissance (code the user is about to change), or exploration (open-ended) — declaring the active mode at thread start and re-declaring whenever the mode shifts.

Process is a hypothesis loop: anchor the investigation (reproduce the failure, or state the question explicitly), hypothesize, attempt to falsify, integrate the result into the document, repeat. Techniques including backward causal reasoning, bisection, fuzz testing, instrumentation, and LSP-driven call-graph traversal (find definitions, find references) are tools rummage reaches for inside this loop — no single technique is primary. Each hypothesis recorded in the document carries its falsification attempt alongside it.

Output per thread is a hybrid document: current best understanding at the top, investigation logbook (falsified hypotheses, abandoned branches, supporting evidence) archived below. Rummage is an active investigator: it writes scratch tests, fuzz harnesses, and instrumentation, all marked with `tinker-test-case:` so the cleanup hook removes them before the next goal session runs. Reads goal files as noise-aware signal. Output follows the shared-language standard — translates technical material rather than echoing jargon. The active agent is shown in the REPL prompt tag; switching is via `/rummage` and `/tinker` slash commands. Runs on the strongest model tier (opus on the Claude backend).

📁 `src/rummage.rs` — `rummage_system_prompt`, `rummage_agent_content` (opencode agent file); `test_spec_rummage_agent_allows_investigation_tools`, `test_spec_rummage_agent_denies_planning_tools`, `test_spec_rummage_backward_causal_reasoning_named`, `test_spec_rummage_hypothesis_loop_named`, `test_spec_rummage_three_modes_named`, `test_spec_rummage_mode_declared_on_shift`, `test_spec_rummage_falsification_per_hypothesis`, `test_spec_rummage_hybrid_document_shape`, `test_spec_rummage_lsp_covers_both_traversal_directions`, `test_spec_rummage_tinker_test_case_marker_required`, `test_spec_rummage_prohibits_tinker_dir_writes`, `test_spec_rummage_agent_content_embeds_system_prompt`, `test_spec_rummage_agent_allows_lsp`, `test_spec_rummage_agent_allows_webfetch_and_websearch`, `test_spec_rummage_lsp_named_for_call_graph_navigation`, `test_spec_rummage_prove_by_execution_scoped_to_this_system`  
📁 `src/app.rs` — `ActiveAgent` enum (`Tinker`, `Rummage`), `Role::RummageAssistant`  
📁 `src/main.rs` — slash command routing for `/rummage` and `/tinker`, rummage runner wiring (strongest model tier)  
📁 `.tinker/goals/rummage.toml` — goal definition

### Root: `goal-sessions`
Launches one LLM session per goal at a time, driven by a cheaper model than tinker. On first run, receives the full text of every sibling goal (so coding-standards etc. are in context). Every dispatch carries an explicit reason: for automated runs (goal edits or reactive scheduling), tinker provides it via `/run` output; for manual runs, the user is prompted. Produces a structured summary (accomplishments, design changes, decisions, try-it) that must explicitly list every `test_spec_` function created or modified — the mechanism that enforces the Tests-as-guardrails standard. Session IDs and the pending queue are kept in memory only — forgotten on restart.

The queue is a FIFO `VecDeque<(Goal, Option<String>)>` that does **not** deduplicate by goal-id: the same goal can appear multiple times, each with its own reason. Multiple `/run` lines for the same goal (across turns or from the user) each enqueue a separate entry and run in order.

📁 `src/goal_session.rs` — `run_goal`, `goal_init_message`, `goal_resume_message`, summary request  
📁 `src/main.rs` — goal runner task (cleanup → `run_goal`)

#### Child: `cleanup-hook`
Before every goal session, scans the project tree for `tinker-test-case:` markers (line-anchored at comment delimiters). Dispatches a cleanup agent to remove/revert marked code. Retries up to 3×; if still dirty, blocks the goal session and reports files to the user. Markers with angle-bracket placeholder reasons (e.g., `<one-line reason>`) are exempt — they are format examples teaching the marker convention, not real investigation markers.

📁 `src/cleanup.rs` — marker detection (`file_contains_marker`, `line_is_marker`, `is_placeholder_reason`), file walking, retry loop, cleanup prompt builder; `test_spec_placeholder_reason_exempt_from_cleanup`, `test_spec_no_self_match_in_tinker_source`, `test_spec_cleanup_prompt_names_placeholder_exemption`

### Root: `goal-storage`
Goals are TOML files under `.tinker/goals/<id>.toml`. Ancestor `.tinker/` directories merge with the cwd's; cwd-most wins on duplicates. A malformed file is reported as an error but doesn't block sibling goals. Session IDs are not persisted to disk — they live in memory for the duration of the tinker process only, so goal files remain the sole source of truth across restarts.

📁 `src/goal.rs` — `Goal` struct (`related: Vec<RelatedLink>`, optional in TOML), `RelatedLink` struct, `GOAL_SCHEMA_KEYS_ORDER`, `load_all_goals`, `load_goals`, `save_goal`, `discover_tinker_dirs`, `build_tree`; `test_spec_related_field_absent_loads_as_empty`, `test_spec_related_field_roundtrip_and_summary`  
📁 `src/cap.rs` — `Filesystem` trait (read/write/list)  
📁 `src/realfs.rs` — `RealFilesystem: std::fs` impl  
📁 `src/test_utils.rs` — `MockFs` for tests

### Root: `tui`
Three-pane terminal UI (alternate screen, mouse capture). Left pane: REPL conversation with an active-agent tag in the input prompt (e.g. `tinker> ` or `rummage> `) showing which agent receives the next message; slash commands `/rummage` and `/tinker` switch the active agent. Right-top pane: selectable goal list + scrollable goal description text below. Right-bottom pane: log output for the selected goal's session. Mouse wheel scrolls whichever pane is under the cursor; auto-follows tail unless the user has scrolled away. When tinker emits `/run` triggers, the resulting system message listing the triggered goals and their reasons is rendered in grey. Trigger reasons are shown in bold in the session log pane.

Goals in the list carry a queue marker when they have running or pending sessions: `▶` for the currently-running goal, `[N]` (1-based global FIFO position) for each goal's next queued entry. Markers render in dim/grey to avoid dominating the goal name. When a goal with pending entries is selected, the goal-text region appends a "── Pending invocations ──" section listing every entry in queue order (running first, then queued), each with its glyph and trigger reason.

📁 `src/tui.rs` — layout (`pane_rects`), draw functions, `goal_queue_marker`, `goal_pending_entries`, scroll state wiring, active-agent prompt tag rendering  
📁 `src/app.rs` — `App` state, `ActiveAgent` enum, `ScrollState` (auto-follow logic), `Phase`, `Focus`, `LoopMode`; `goal_queue: VecDeque<(Goal, Option<String>)>` and `active_goal_reason` drive queue rendering  
📁 `src/main.rs:823-989` — mouse/keyboard event handlers, `/rummage`/`/tinker` slash-command routing

### Root: `coding-standards` (packaged goal)
Cross-cutting standards enforced by convention: capability-based DI (traits for effects, real impls at composition root), ladder of polymorphism (use the simplest mechanism), test marking convention (`test_spec_` / `test_security_`), security.md per project, investigation markers (`tinker-test-case:`).

📁 `packaged-goals/coding-standards.toml` — the standards text  
📁 `src/cap.rs` — capability interfaces  
📁 `src/opencode.rs` — real `OpenCodeRunner` impl (composition-root only)  
📁 `src/cleanup.rs` — marker enforcement mechanism

### Root: `claude-backend`
A `--claude` CLI flag that makes tinker shell out to `claude -p` instead of `opencode`. When set, tinker uses Opus, goal sessions use Sonnet, and the cleanup runner uses Haiku. The tinker persona is passed via `--system-prompt` instead of agent file installation.

📁 `src/claude.rs` — `ClaudeRunner` impl (`disallowed_tools: Vec<String>` field, `with_denied_tools()` builder), `claude_command`/`claude_args` (both accept `disallowed_tools: &[String]`, emitted as `--disallowedTools`), model-tier constants, `format_tool_use`; `test_spec_disallowed_tools_passed_as_flag`, `test_spec_disallowed_tools_absent_when_empty`, `test_spec_with_denied_tools_stores_list`, `test_spec_format_tool_use_compact_one_liner`  
📁 `src/main.rs` — `--claude` flag detection and runner selection

### Root: `cost-reduction`
Standing investigative goal: identify cost-cutting opportunities in how tinker uses LLMs and keep a persistent observation record across sessions. First substep completed: extended `src/claude.rs` to capture per-run token counts from the Claude CLI's `result` event stream (`ClaudeUsage` struct, `format_usage_line`, `USAGE_LINE_MARKER` sentinel), and wired `src/tui.rs` to render those lines in dim style at the end of each session log. Findings surface in goal-session summaries; tinker folds significant observations into `.tinker/notes/cost-observations.md`. Acting on any finding requires a fresh interview — findings never become goal files on their own.

📁 `src/claude.rs` — `ClaudeUsage`, `format_usage_line`, `USAGE_LINE_MARKER`; `test_spec_usage_deserializes_from_result_event`, `test_spec_usage_line_format_shows_all_four_fields`, `test_spec_usage_line_empty_when_all_zero`  
📁 `src/tui.rs` — `render_log_line` branch for `USAGE_LINE_MARKER` (dim cyan); `test_spec_usage_line_rendered_dim_in_log`  
📁 `.tinker/notes/cost-observations.md` — tinker-owned observation log (created on first finding)

### Root: `project-indexer` (packaged goal — this agent)
Maintains `AGENTS.md` at the project root — a goals-oriented index mapping each goal to its source files. Triggers reactively when other goal sessions modify files that affect the map.

📁 `AGENTS.md` — this file  
📁 `packaged-goals/project-indexer.toml` — goal definition



## Architecture notes

- **Composition root** in `src/main.rs`: `RealFilesystem` and runner instances (one per model tier, plus one for rummage) are wired at startup. Business logic depends only on traits in `cap.rs`. With `--claude`, `ClaudeRunner` instances replace `RealOpenCodeRunner`.
- **Three model tiers**: tinker (smartest), goal sessions (mid), cleanup (cheapest). Default models defined in `src/opencode.rs:15-20`. With `--claude`, uses Claude aliases: opus, sonnet, haiku (defined in `src/claude.rs:15-17`). Rummage uses the smartest tier (same as tinker) — it is an active investigator requiring full reasoning capability.
- **Two chat agents**: tinker (`src/tinker.rs`) and rummage (`src/rummage.rs`) are peer agents sharing the REPL pane. The active agent is tracked in `App.active_agent`; `/rummage` and `/tinker` slash commands switch between them.
- **Event loop** (`main.rs:run_loop`): polls terminal events, drains tinker/goal channels, draws TUI on every iteration. When a goal-session batch drains, `handle_goal_event` sends the batch summaries to tinker, which produces the user-facing summary and any reactive `/run` lines in a single reply.
- **Goal tree**: `goal.rs::build_tree` builds a parent-child hierarchy from `parent_id` fields. Flat list for selection; tree for display.
- **Security**: documented in `security.md`. Key mitigations include per-file goal loading isolation (T1), per-project session IDs (T2), ancestor-merger transparency (T3), no permission-bypass flags (T4), stderr nulled from subprocesses (T5).
- **Tests**: inline `#[cfg(test)]` modules in each `src/` file, named `test_spec_*` for spec requirements and `test_security_*` for threat mitigations. `MockFs` in `test_utils.rs` provides an in-memory filesystem for testing.
