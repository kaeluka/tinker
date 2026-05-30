# tinker — autonomous coding assistant

Tinker is a Rust CLI that turns user intent into code through a loop of **goals** (standing intent files), **tend** (interviews the user, dispatches goal agents, inspects its own runtime state), **goal agents** (LLM sessions that write code, addressable by goal-id), and a **cleanup hook** (removes investigation markers). The user interacts via a three-pane **TUI**.

---

## Goals

### Root: `the-false-promise`
The foundational diagnosis that justifies tinker's architecture. Current coding assistants promise developers they no longer have to write code, but their architecture forces developers to keep reading it — they launder ad-hoc AI implementation guesses into a "quasi-specification" indistinguishable from human intent. Tinker rejects this by separating Intent from Implementation into a durable goal layer. `creative-process`, `user-system-interaction`, and `readme-maintainer` articulate the solution derived from this diagnosis.

📁 `.tinker/goals/the-false-promise.toml` — goal definition

#### Child: `creative-process`
Defines how tinker converts user intent into a working system: *iterative dialectical discovery*. The user is a *reflective practitioner* (Schön) — injecting tacit, situated judgment (knowing-in-action) that cannot be codified in advance. Tend is a *convention engine* — excellent at executing rules sequentially and cross-referencing goals, poor at judgment that requires being embedded in practice. Goals are the bridge: the user's intuition, surfaced through dialogue and crystallized into explicit conventions the engine can then execute and cross-reference. The conversion is lossy — conventions degrade as situations drift — so the dialectic continues rather than terminating once a goal is written. Tend's dual duty: faithfully execute conventions AND surface inflection points where convention runs out; at those points it stops and asks, not extrapolates. Tone is downstream of role: direct, invested, no deference layer. The human is the *termination condition* for the refinement loop — not replaceable by a second language model, which has no real stake, no continuity of the underlying problem, and cannot supply the external bounding that ends the loop.

📁 `.tinker/goals/creative-process.toml` — goal definition  
📁 `.tinker/notes/notes.md` — argument for why two LLMs cannot replace the human in the dialectic  
📁 `src/tend.rs` — `test_spec_tinker_encodes_dual_duty_no_fabrication_at_inflection_points`, `test_spec_tinker_encodes_convention_engine_reflective_practitioner_lossy_bridge`, `test_spec_creative_process_dialectical_loop_continues_after_goal_written`, `test_spec_tinker_tone_downstream_of_role_drop_deference_layer`

##### Grandchild: `goal-craft`
Operational rules for writing goal content so it works as both standing intent and as the prompt that agents read. Five tactics: sparseness (operationalized by the anchor test — a claim belongs only when user intent, an external dependency, or a hard-won negative constraint anchors it), positive framing (WHAT not whatn't), orthogonality of content (goals are free-standing; cross-cutting concerns live in links, not duplicated body text), goals-are-prompts (five prompt-quality rules: define terms before first use, consistent vocabulary, reference sibling goals by id, examples earn their place, lead with the load-bearing point), and synthesis over enumeration (user-vetted intent is woven into WHAT/WHY/SCOPE prose, not accumulated as a list of decisions). Enforcement is read-time: tend cites this goal when it spots a candidate violation during interviews and cross-goal alignment.

📁 `.tinker/goals/goal-craft.toml` — goal definition

#### Child: `user-system-interaction`
The architectural boundary between the user and tinker. The user holds the *should* — intent, decisions, vetted goals, observations under investigation, and mutually established terms. Tinker holds the *is* — the current state of all files, processes, build outputs, caches, and side effects of any past action. When tinker's reasoning depends on the *is*, it verifies by observation; inference does not substitute for checking. The user is exposed to exactly two surfaces: the *interface* of any tool they touch (invocation path, flag names, env vars, start/stop) and *internals filtered through shared language* (features, architectural concepts, observable behavior). Raw code identifiers never surface without that filtering. `user-persona`, `shared-language`, and `implementation-ownership` are downstream applications of this principle.

📁 `src/tend.rs` — "Should and is", "What surfaces to the user", "Downstream applications" sections in the tend agent prompt; `test_spec_user_holds_should_tinker_holds_is`, `test_spec_is_state_verified_by_observation_not_inference`, `test_spec_two_surface_exposure_and_downstream_framing`  
📁 `.tinker/goals/user-system-interaction.toml` — goal definition

##### Grandchild: `user-persona`
Formalizes the design philosophy: the user is an experienced developer who no longer looks at source code. Tend acts as a compiler — surfacing behavior through goals and tinkering, never relying on "trust me bro" readings of source. Four communication rules follow: no-margin-of-error explanation (structural, not a quality bar), prove by tinkering rather than asserting from source, design-partner posture when reporting observations, and multi-variable optimization framing for design trade-offs.

📁 `.tinker/goals/user-persona.toml` — the persona definition  
📁 `src/tend.rs` — `test_spec_tinker_proves_by_execution_not_reading_source`, `test_spec_tinker_prompt_directs_active_design_partner_reporting`, `test_spec_tinker_prompt_frames_cross_goal_alignment_as_pareto_tradeoffs`

##### Grandchild: `shared-language`
Cross-cutting rule that applies to every user-facing surface tinker produces: tend chat, session-completion summaries synthesized by tend, and human-readable documentation goal sessions write into the project tree (READMEs, guides, human-targeted code comments). Default to conceptual/architectural descriptions rather than code identifiers. Artifacts explicitly authored for LLM consumption (e.g. `AGENTS.md` produced by `project-indexer`) are exempt — they retain full technical fidelity. Goal sessions reach this rule through the neighborhood table: goals that produce user-facing text (`tend`, `rummage`, `tui`) each carry a `related` link to `shared-language`, so any session running under those goals sees it in its neighborhood and can pull it on demand. Tend fully owns `.tinker/state/vocabulary.txt` (updated silently when the user mentions a term or a term is written into a goal file; never mentioned to the user); goal sessions read the file but do not write it. When a technical term not in the vocabulary must be used, it must be anchored to a known architectural concept.

📁 `src/tend.rs` — `## Shared language and vocabulary` section in the tend agent prompt; `test_spec_tinker_prompt_defaults_to_conceptual_explanations`, `test_spec_tinker_prompt_names_vocabulary_state_file`, `test_spec_tinker_prompt_owns_vocabulary_file_silently`, `test_spec_tinker_prompt_states_vocabulary_entry_triggers`, `test_spec_tinker_prompt_requires_anchoring_unknown_terms`, `test_spec_shared_language_covers_documentation_written_by_goal_sessions`, `test_spec_shared_language_llm_targeted_artifacts_exempt`, `test_spec_shared_language_goal_sessions_inherit_via_related_link`, `test_spec_shared_language_goal_sessions_read_vocab_not_write`, `test_spec_shared_language_deeper_sections_allowed_with_anchoring`, `test_spec_shared_language_single_vocabulary_file_source_of_truth`, `test_spec_state_dir_created_at_startup`, `test_spec_shared_language_form_norm_minimum_viable_shape`, `test_spec_shared_language_form_norm_no_formulaic_replies`, `test_spec_shared_language_form_norm_no_unrequested_tables_or_lists`, `test_spec_shared_language_form_norm_rummage_documents_exempt`  
📁 `src/main.rs` — `fs.mkdir_all` for `.tinker/state/` at startup

##### Grandchild: `implementation-ownership`
A formal mandate applied to goal sessions: they own the source-code implementation. Ownership is scope-bounded by the goal text plus any related goals, and unconstrained beyond that — sessions hold full autonomy over implementation choices the spec doesn't dictate, none over choices it does, and treat the goal as a high-level optimization target rather than a literal exhaustive spec. The mandate explicitly overrides the default LLM behavior of acting as a cautious "guest" who avoids deleting or radically restructuring code: a session is expected to demolish and rewire architecture without hesitation when that better satisfies the goal.

📁 `.tinker/goals/implementation-ownership.toml` — goal definition  
📁 `src/goal_session.rs` — ownership mandate injected into goal-session system prompt

#### Child: `readme-maintainer`
A goal that reactively maintains the human-facing `README.md` at the project root, synthesizing tinker's philosophical foundation — the diagnosis of cognitive debt, the dialectical process, and the strict state-ownership boundary — into a cohesive narrative for developers seeing tinker for the first time. The README is generated dynamically from the current state of the philosophical goal tree, preventing it from drifting into a stale artifact. Adheres strictly to the shared-language standard.

📁 `.tinker/goals/readme-maintainer.toml` — goal definition  
📁 `README.md` — the maintained file

### Root: `the-cycles`
The map of tinker's agents onto the intent↔spec↔code cycle, running in both directions. **Forward (generation):** intent→spec→code — tend builds the spec from the user's intent; goal sessions build code from the spec. **Backward (verification):** code→spec→intent — rummage audits whether code satisfies the spec; jog audits whether the spec still reflects the user's intent. The cycle is closed: every artifact produced forward is a candidate for scrutiny backward. The produced artifact at each step must be explainable in terms of the step before it.

One axis is encoded in this tree: the producer/skeptic axis. `forward-generation` holds the producers (tend, goal sessions), which move forward and treat the goal as authoritative. `divergence-investigation` holds the skeptics (rummage, jog), which move backward and treat the goal as signal-with-noise. `agent-complementarity` articulates the design principle governing profile assignments across this cycle.

Two further axes cross-cut as prose: adjacency ownership (intent↔spec by tend+jog; spec↔code by goal sessions+rummage — skeptic that finds an artifact wanting commissions its paired producer to fix it) and human coupling (agents touching layer 1 tacit intent are dialectical and human-coupled; those in layers 2–3 run autonomously).

📁 `.tinker/goals/the-cycles.toml` — goal definition

#### Child: `forward-generation`
The shared producer role: forward generators move forward through the intent→spec→code stack and treat the goal as authoritative. Tend generates the written spec from the user's tacit intent; goal sessions generate code from the spec. The two children differ on input observability — tend generates from tacit intent (only drawable by asking, so dialectical/human-coupled); goal sessions generate from a written spec they can read (so autonomous). This observability-driven autonomy split mirrors the same split in `divergence-investigation`.

📁 `.tinker/goals/forward-generation.toml` — goal definition

##### Grandchild: `tend`
Tinker's intent arm: any agent that needs to understand the *should* — what the user wants, what a goal means, whether a behavior is intentional — consults tend. Tend's counterpart is rummage, the technical arm. Responsible for managing goals and keeping them in shape: rich with relationships, well-nested, low in coupling. When tend spots missing edges, it silently cleans them up.

**Peer intent queries.** When an agent sends `@tend` with an intent question, tend answers from the goal files — not by interrupting the user. Goals are ground truth; accurate goals represent the user's intent. If tend finds a contradiction across goals while answering, it picks the interpretation most consistent with evident intent, lets the work continue, and surfaces the contradiction to the user at a natural pause — not mid-flow, not blocking.

**Interview.** Four phases: (1) draft the picture — idea, motivation, what done looks like; (2) one question per turn until complete, then find the right home in the goal tree (no option menus; probe bare assertions); (3) playback before writing — only the user can validate whether the articulated goal matches their intent, never infer consent from silence; (4) write atomically, then send `@rummage` with a declarative pointer to what changed. Never writes production code directly. VCS is read-only: `git status`/`git diff`/`git log` only.

📁 `src/tend.rs` — init prompt, `tend_agent_content`; `test_spec_tinker_prompt_names_vocabulary_state_file`, `test_spec_tinker_agent_file_frontmatter_denies_task_and_todowrite`, `test_spec_tinker_agent_does_not_deny_write_edit_bash`, `test_spec_tinker_agent_does_not_deny_lsp`, `test_spec_tinker_agent_does_not_deny_webfetch`, `test_spec_tinker_static_persona_in_agent_dynamic_goals_in_init`, `test_spec_tinker_prompt_forbids_vcs_mutation`, `test_spec_agent_file_always_overwritten_not_guarded_by_exists_check`, `test_spec_cross_goal_alignment_no_exemptions_for_edits`, `test_spec_main_feeds_parent_id_and_children_into_goals_summary`, `test_spec_tinker_prompt_compact_index_describes_parent_id`, `test_spec_tinker_init_prompt_full_context_label`, `test_spec_tinker_prompt_parent_summary_recheck_when_child_edited`, `test_spec_tinker_prompt_related_links_symmetric_both_list_each_other`  
📁 `src/main.rs` — `handle_session_event`: session Done → goal reload, parse-error correction, `@`-block dispatch; `tend_event_to_session`: bridges TendEvent to unified SessionEvent channel

###### Great-grandchild: `tend-agent`
Enforces tend's boundaries by running it under a custom `opencode` agent profile (`tend`) that strips out the default software-engineer persona and denies access to `task`/`todowrite` tools. Silently installed into `~/.config/opencode/agents/tend.md` on startup (always overwritten so the installed copy stays in sync).

📁 `src/opencode.rs` — `--agent` flag passed to opencode CLI, `RealOpenCodeRunner::with_agent`  
📁 `src/tend.rs` — `tend_agent_content` (static agent file text); `test_spec_tinker_agent_file_frontmatter_denies_task_and_todowrite`, `test_spec_tinker_agent_does_not_deny_write_edit_bash`, `test_spec_tinker_agent_allows_websearch_denies_webfetch`, `test_spec_tinker_static_persona_in_agent_dynamic_goals_in_init`, `test_spec_agent_file_always_overwritten_not_guarded_by_exists_check`, `test_spec_tinker_prompt_forbids_vcs_mutation`

###### Great-grandchild: `tend-notes`
Tend silently appends timestamped entries to `.tinker/notes/notes.md` throughout conversation — recording friction (user corrections or repetition), surprise (unexpected behavior), reframes (pivot from one framing to another), recurring threads, and explicit "remember this" requests from the user. Note-taking is invisible: tend never interrupts conversation to mention it. This goal builds the substrate only; retrieval and reflection are a later goal.

📁 `src/tend.rs` — `## Note-taking` section in the tend agent prompt (trigger conditions, tone: "observe without judging"); `test_spec_tinker_notes_names_correct_file_path`, `test_spec_tinker_notes_all_triggers_named`, `test_spec_tinker_notes_observe_without_judging_tone`, `test_spec_tinker_notes_writing_is_silent`, `test_spec_notes_dir_created_at_startup`  
📁 `src/main.rs` — `fs.mkdir_all` for `.tinker/notes/` at startup  
📁 `.tinker/notes/notes.md` — append-only notes file (created on first write)

###### Great-grandchild: `tend-introspection`
Runtime introspection substrate: gives tend structured access to the *is* it already holds. Writes two artifacts: an append-only JSONL event log at `.tinker/logs/runtime.jsonl` (per-turn token usage, goal-session lifecycle with observables, full message history including system messages, TUI state changes) and a debounced semantic state snapshot at `.tinker/state/runtime.json` (selected goal id, focus, scroll offsets, rolling session usage totals). Producers call `LogSender::emit` (synchronous, non-blocking); a single background task drains the channel and batches writes (every 100ms or 10 events, whichever first); the state snapshot is debounced ~100ms after the last state-changing event. Tend reads both files via its existing `Read`/`Bash`/`Grep` tools — no new tool plumbing needed. Goal sessions never read these files. The log grows without rotation; the user clears it manually if needed.

📁 `src/logger.rs` — `LogEvent` enum (tends session/turn/message events, goal-session lifecycle, cleanup-hook, run-command, TUI state), `LogEntry`, `LogSender`, `StateSnapshot`, `QueueEntry`, `ScrollOffsets`, `start_logger`, `noop_sender`, `count_tool_calls`, `extract_modified_files`; `test_spec_log_line_has_required_ts_kind_source_fields`, `test_spec_event_kinds_are_snake_case`, `test_spec_goal_session_finished_carries_observable_set`, `test_spec_message_events_carry_full_text`, `test_spec_orchestrator_system_message_received_logs_source_and_content`, `test_spec_no_rotation_log_appends`, `test_spec_state_debounce_via_apply_to_state`  
📁 `src/main.rs` — `mod logger`, `fs.mkdir_all` for `.tinker/logs/` at startup, `logger::start_logger(...)` call at composition root, initial `OrchestratorSystemMessageReceived` event emitted on start  
📁 `.tinker/logs/runtime.jsonl` — append-only event log (created on first write)  
📁 `.tinker/state/runtime.json` — debounced semantic UI snapshot (created on first state-changing event)

###### Great-grandchild: `goal-structure-standard`
Guides tend's goal-tree decisions: Merge (modify existing), Nest Down (create child), Nest Up (create parent), New Root (sibling), or Retire (delete when a reframe makes the goal obsolete — user confirms). Uses a "level of detail coherence" heuristic — if merging breaks the zoom level, extract into a child goal instead. Three write-time coherence rules enforced on every edit: (1) parent goals must summarize their children sufficiently to support drill-down filtering during cross-goal alignment; (2) `[[children]]` entries carry both `id` and `reason` — reason is required when nesting a new child, answering "why is this goal a sub-aspect of its parent?" and serving as the navigation index for agents deciding whether to pull the child's full text; (3) goals may carry a symmetric `related` field listing cross-cutting goals and per-side reasons — both ends must list each other, reason text may differ per side, transitivity is not enforced. Carries a `related` back-link to `goal-craft` (content-quality rules interact with structural rules at write time).

📁 `.tinker/goals/goal-structure-standard.toml` — the standard definition  
📁 `src/goal.rs` — `RelatedLink` struct, `ChildLink` struct, `Goal.related` and `Goal.children` fields, `GOAL_SCHEMA_KEYS_ORDER` (includes `summary (optional)`, `related (optional)`, and `children (optional)`)  
📁 `src/main.rs` — compact JSON index (via `goal::build_compact_index`) injected into tend's context each turn; includes related links as JSON arrays so tend sees cross-cutting links  
📁 `src/tend.rs` — `test_spec_tinker_prompt_names_four_structural_operations`, `test_spec_tinker_prompt_uses_level_of_detail_coherence_heuristic`, `test_spec_tinker_prompt_describes_nest_up_reparents_existing`, `test_spec_tinker_prompt_framed_collaboratively_not_rigidly`, `test_spec_tinker_prompt_documents_slash_run_trigger_syntax`, `test_spec_tinker_prompt_treats_modify_as_merge`, `test_spec_tinker_prompt_related_field_in_toml_schema`, `test_spec_tinker_prompt_parent_summary_recheck_when_child_edited`, `test_spec_tinker_prompt_related_links_symmetric_both_list_each_other`, `test_spec_tinker_prompt_related_transitivity_not_automatic`

###### Great-grandchild: `compact-goal-context`
Replaces full goal text in tend's context with a compact JSON index — one entry per root goal, children nested recursively. Each entry carries `parent_id` (empty for root goals) so tend can navigate upward without loading a parent file. Each nested child entry carries the link reason from the parent's `[[children]]` alongside the child's own `summary`, so the navigation decision (pull or skip) is local to the index without a file read. Related links are represented as id/reason arrays. Each goal TOML carries a `summary` field: terse, grammar-stripped, keyword-dense, written for LLM navigation (format: `governs: [domain]; triggers: [situations]`). The index contains only summaries; full text is pulled on demand via tend's existing file tools. Pull strategy: pull when in doubt — summaries let tend confidently skip clearly-irrelevant goals, but everything else is pulled. Tend writes or updates `summary` on every goal creation and substantive edit. A `--tinker-full-goal-context` startup flag restores the pre-compact behavior (full goal text injected, compact-index prompt section suppressed) as a revert escape hatch; the `summary` write protocol remains active regardless.

📁 `.tinker/goals/compact-goal-context.toml` — goal definition  
📁 `src/goal.rs` — `Goal.summary` field, `build_compact_index` (builds nested JSON from `build_tree`); `test_spec_summary_field_absent_loads_as_empty`, `test_spec_summary_field_roundtrip`, `test_spec_compact_index_uses_summary_not_description`, `test_spec_compact_index_includes_parent_id`, `test_spec_compact_index_child_entries_carry_reason`  
📁 `src/main.rs` — `goal::build_compact_index(&a.goals)` replaces flat text list in `goals_summary`; `test_spec_goals_summary_includes_related_links`  
📁 `src/tend.rs` — `## Goal index` section in tend prompt (index format, pull strategy, summary write protocol); `test_spec_tinker_prompt_describes_compact_goal_index`, `test_spec_tinker_prompt_toml_examples_include_summary_field`, `test_spec_tinker_phase4_instructs_summary_write`, `test_spec_main_feeds_parent_id_and_children_into_goals_summary`, `test_spec_full_goal_context_suppresses_compact_index_section`, `test_spec_full_goal_context_keeps_summary_write_protocol`, `test_spec_tinker_init_prompt_full_context_label`, `test_spec_tinker_prompt_compact_index_describes_parent_id`

##### Grandchild: `goal-sessions`
Launches one LLM session per goal at a time, driven by a cheaper model than tend. On first run, receives its own goal in full plus a **neighborhood table** — parents, children, and `related` links (each as a `{goal-id, reason}` row) — and pulls neighbor full text on demand from `.tinker/goals/`. Goals reach `coding-standards` and `shared-language` through their `related` links, so sessions pick them up when they pull relevant neighbors. Every dispatch carries an explicit reason threaded through as context. Produces a structured summary (accomplishments, design changes, decisions, try-it) that must explicitly list every `test_spec_` function created or modified — the mechanism that enforces the Tests-as-guardrails standard. Under the `goal-agents` registry model, goal agents report completion via `@tend` rather than a batch-summary pipeline; session IDs are kept in memory only — forgotten on restart.

📁 `src/goal_session.rs` — `run_goal`, `pub session_init_message` (single construction path for all sessions — takes compact_index, builds identity/navigation/message-passing preamble + goal description), `build_neighborhood_table`, `SessionEvent` enum; `test_spec_goal_init_has_neighborhood_table`, `test_spec_goal_init_neighbors_pullable_on_demand`, `test_spec_goal_init_no_subgoals_section`, `test_spec_goal_init_no_neighborhood_table_when_isolated`  
📁 `src/main.rs` — goal runner task (cleanup → `run_goal`, passes compact_index snapshot); `session_tx` unified event channel replaces per-agent channels

###### Great-grandchild: `cleanup-hook`
Before every goal session, scans the project tree for `tinker-test-case:` markers (line-anchored at comment delimiters). Dispatches a cleanup agent to remove/revert marked code. Retries up to 3×; if still dirty, blocks the goal session and reports files to the user. Markers with angle-bracket placeholder reasons (e.g., `<one-line reason>`) are exempt — they are format examples teaching the marker convention, not real investigation markers.

📁 `src/cleanup.rs` — marker detection (`file_contains_marker`, `line_is_marker`, `is_placeholder_reason`), file walking, retry loop, cleanup prompt builder; `test_spec_placeholder_reason_exempt_from_cleanup`, `test_spec_no_self_match_in_tinker_source`, `test_spec_cleanup_prompt_names_placeholder_exemption`

#### Child: `divergence-investigation`
The shared skeptical role for tinker's two investigator agents. A divergence investigator finds and explains a gap between two adjacent layers of the intent→spec→code stack: `rummage` works the spec↔code gap (does the code do what the goal says?); `jog` works the intent↔spec gap (does the spec still reflect what was intended?). Both treat the goal as signal with noise rather than authoritative. The autonomy split follows observability: rummage investigates code directly (code is executable); jog delegates reads to peer agents (`@tend` for spec, `@rummage` for code) and never writes or commissions during a run.

📁 `.tinker/goals/divergence-investigation.toml` — goal definition

##### Grandchild: `rummage`
Tinker's technical and skeptical agent — the ground-truth arm for every other agent in the system. Any agent that needs code reality, technical validation, or a counterexample consults rummage. Rummage in turn consults `@tend` whenever it needs intent that the goal text doesn't settle.

**Pre-dispatch validation.** When tend hands off a goal via `@rummage`, rummage stress-tests it before any code is written: probes the codebase, surfaces counterexamples, cuts paths that won't work, consults `@tend` for intent gaps, then dispatches goal agents with detailed step-by-step implementation instructions sequenced to minimize conflicts.

**Investigation.** Anchor (reproduce the failure or state the question), hypothesize, falsify, integrate, repeat. Each hypothesis carries its falsification attempt. Techniques: backward causal reasoning, forward call-graph walks, bisection, fuzz testing, instrumentation; `lsp` for call-graph navigation; `webfetch`/`websearch` for external library detail.

Output per thread is a hybrid document: current best understanding at the top, investigation logbook archived below. Scratch tests and instrumentation are marked `tinker-test-case:` so the cleanup hook removes them. When a thread confirms **code drift** — the spec is correct but the code diverged — rummage writes a durable failing test (deliberately unmarked), dispatches `@owning-goal-id` with a declarative pointer to that test, then sends `@jog` to verify the spec↔code gap is closed before reporting done. **Intent questions** — correct behavior requires a fresh intent decision not derivable from the spec — are surfaced to tend and the user; rummage does not write a test or dispatch. Reads goal files as signal with noise. Output follows the shared-language standard. Runs on the strongest model tier.

📁 `src/rummage.rs` — `rummage_agent_content`, `packaged_goal()`; `test_spec_rummage_agent_allows_investigation_tools`, `test_spec_rummage_agent_denies_planning_tools`, `test_spec_rummage_agent_file_frontmatter_only`, `test_spec_rummage_backward_causal_reasoning_named`, `test_spec_rummage_falsification_per_hypothesis`, `test_spec_rummage_hybrid_document_shape`, `test_spec_rummage_lsp_covers_both_traversal_directions`, `test_spec_rummage_tinker_test_case_marker_required`, `test_spec_rummage_prohibits_tinker_dir_writes`, `test_spec_rummage_agent_allows_lsp`, `test_spec_rummage_agent_allows_webfetch_and_websearch`, `test_spec_rummage_lsp_named_for_call_graph_navigation`, `test_spec_rummage_prove_by_execution_scoped_to_this_system`, `test_spec_rummage_code_drift_fix_path_named`, `test_spec_rummage_intent_question_abstention_named`, `test_spec_rummage_durable_failing_test_no_marker`, `test_spec_rummage_form_norm_minimum_viable_shape`, `test_spec_rummage_prompt_names_tend_as_intent_arm_with_three_triggers`  
📁 `src/app.rs` — `ActiveAgent` enum (`Tend`, `Rummage`, `Jog`), `Role::RummageAssistant`  
📁 `src/main.rs` — slash command routing for `/rummage`, `/tend`, and `/jog`, rummage runner wiring (strongest model tier); `test_spec_rummage_wired_to_strongest_model_tier`  
📁 `packaged-goals/rummage.toml` — goal definition (packaged; compiled in via `include_str!`)

##### Grandchild: `jog`
A bidirectional discrepancy finder. Given two redundant sources of information (where one is derived from the other), jog builds a set of things/concepts/behaviors in each by delegating to peer agents — `@tend` for the spec layer, `@rummage` for the code layer, and the user for tacit intent — then compares the two sets step by step. Every `@tend` and `@rummage` message during a run is explicitly framed as read-only ("tell me what you see, don't act on it"). Jog documents discrepancies; it never commissions fixes or dispatches during a run.

**Forward check (N→N+1):** coverage — things in the derived layer that the base doesn't account for (a bug-finding lens). **Backward check (N+1→N):** provenance — things in the base that have no traceable origin in the derived layer (points of interest). Each run produces a new file under `.tinker/discrepancies/`. Invocation is on-request; automated triggering is deferred.

Jog is a third sibling chat agent alongside `tend` and `rummage`; `/jog` switches to it. Runs on the strongest model tier.

📁 `src/jog.rs` — `jog_agent_content`, `packaged_goal()`; `test_spec_jog_agent_denies_planning_tools`, `test_spec_jog_agent_file_frontmatter_only`, `test_spec_jog_system_prompt_describes_pull_invocation`, `test_spec_jog_system_prompt_names_no_cue_questioning`, `test_spec_jog_system_prompt_names_two_findings`, `test_spec_jog_commission_uses_at_tend`, `test_spec_jog_prohibits_investigation_code`, `test_spec_jog_does_not_probe_running_system`, `test_spec_jog_does_not_write_goals`, `test_spec_jog_commission_is_terminal_for_thread`, `test_spec_jog_probes_the_why`, `test_spec_jog_form_norm_minimum_viable_shape`, `test_spec_jog_prompt_names_rummage_as_code_reality_arm`  
📁 `src/app.rs` — `ActiveAgent::Jog`, `Role::JogAssistant`, jog session state (`jog_current_text`, `jog_session_id`, `jog_tasks`, `append_jog_chunk`, `finalize_jog_message`)  
📁 `src/main.rs` — jog runner (strongest model tier); `test_spec_jog_wired_to_strongest_model_tier`, `test_spec_slash_jog_switches_active_agent`, `test_spec_message_routes_to_jog_when_active`  
📁 `src/tui.rs` — `jog>` prompt tag (Blue style), `Role::JogAssistant` rendering via `push_jog_text`; `test_spec_jog_messages_rendered_with_blue_jog_label`, `test_spec_jog_prompt_tag_in_tui_source`  
📁 `packaged-goals/jog.toml` — goal definition (packaged; compiled in via `include_str!`)

#### Child: `agent-complementarity`
The design principle governing how tinker's agent profiles are assigned and kept complementary. Tasks go to the agent whose profile gives the highest likelihood of a high-quality response for that workload. Current profile assignments by cycle position: **tend** (intent↔spec, forward) — intent synthesis, dialectical interviewing, peer intent queries answered from goal files; **goal sessions** (spec↔code, forward) — autonomous code generation against a written spec; **rummage** (spec↔code, backward) — grounded code investigation, hypothesis loops, falsification, LSP traversal; **jog** (intent↔spec, backward) — bidirectional discrepancy finding via read-only delegation to peer agents. Intentional overlap exists at the spec-satisfaction boundary: tend reads session reports and synthesizes user-facing output; rummage owns code-reality assessment. An audit mandate fires whenever any agent's profile changes, a new agent is added, or a new recurring consultation pattern emerges. Peer consultation (via `peer-consult`) is only as valuable as the quality gap it crosses; routing to the wrong profile produces worse output than staying put.

📁 `.tinker/goals/agent-complementarity.toml` — goal definition

### Root: `goal-agents`
Unified session registry where every goal is a process-scoped, addressable agent. The registry is a `HashMap<GoalId, SessionEntry>` holding a message channel, the captured LLM session ID (for resumption), and the associated goal. Sessions start lazily on first `@goal-id` message; tend starts eagerly on process init.

**`@goal-id message`** is universal dispatch — the mechanism through which any agent activates any other. `/run` is retired; `parse_run_commands` is deleted and all goals have been updated to use `@`-dispatch language. `/jog-edit` is retired alongside the old Socratic jog behavior.

**Framework preamble** injected before every goal's description: (1) identity — "You are the agent for goal `<id>`"; (2) navigation — compact goal index plus on-demand pull path; (3) message-passing semantics — how `@goal-id` routing works, actor model (no blocking, no call stack). The agent's role in the cycle is part of its `description`, not runtime-injected.

**Unified `SessionEvent` enum** (`LlmSessionId`, `Chunk`, `Done`, `CleanupBlocked`, `SummaryReady`) replaces the former per-agent event enums. A single `mpsc::Receiver<SessionEvent>` in `run_loop` replaces three separate named-agent handler channels. `handle_orch_event`, `handle_rummage_event`, and `handle_jog_event` collapse into `handle_session_event`.

**Batch-summary machinery removed**: `Phase::RunningGoal`, `SummarizingBatch`, `batch_had_goals`, `batch_summaries`, and `build_batch_summary_request` are deleted. Goal agents `@tend` when they complete significant work; tend synthesises through normal conversation.

**`tier` field** (optional in goal TOML: `"high"`, `"mid"`, or absent/defaults to `"mid"`): resolved at session start via `model-config`. Tend, rummage, and jog declare `tier = "high"`.

📁 `.tinker/goals/goal-agents.toml` — goal definition  
📁 `src/goal_session.rs` — `SessionEvent` enum, `session_init_message` (single construction path), `SummaryReady` variant (transitional until `@tend` from LLM)  
📁 `src/main.rs` — `session_senders: HashMap<String, Sender<String>>` (registry), `tend_event_to_session`, `handle_session_event`, unified `session_rx` channel; `parse_at_commands(text, known_ids)`, `parse_at_commands_builtin` wrapper  
📁 `src/app.rs` — `current_session_text: HashMap<String, String>` (accumulates per-session text for `@`-block extraction); `Phase` simplified to Initializing/Idle

### Root: `triggers`
One dispatch channel. **`@goal-id message`**: the universal dispatch — any agent activates any other by emitting an `@goal-id` block in its reply. After the reply finalizes, the system routes each block to the named recipient's channel via the session registry (`dispatch_peer_consultations`). `/run` is retired; `parse_run_commands` has been deleted. `/jog-edit` is retired alongside the old Socratic jog behavior.

Context carried in `@goal-id` dispatches from tend is a **declarative pointer to a spec delta** — what changed in the goal or its environment — never an imperative.

The **`@<agent-name>`-block syntax** (defined by `peer-consult`) serves both as peer consultation and as the dispatch channel; the distinction is context — a code-writing commission vs. a question or advisory. `goal-agents` governs how `parse_at_commands` and `dispatch_peer_consultations` are wired to the session registry.

📁 `src/main.rs` — `parse_at_commands` (updated to accept any registry-known session ID), `dispatch_peer_consultations`; `test_spec_dispatched_run_uses_triggered_system_message_format`  
📁 `.tinker/goals/triggers.toml` — goal definition

### Root: `peer-consult`
Any of the three interactive agents (tend, rummage, jog) can send a one-way message to another by emitting an `@`-block in its reply. Two equivalent forms: `@<agent-name> <message>` (inline — message on the same line) or `@<agent-name>` alone with the message body on the lines that follow (standalone). Either way the block spans from the `@`-line through the next `@`-line or end of reply; only the block is delivered — prose before or after is not sent to the recipient. Empty blocks (no inline content and no body lines) are silently dropped. The system detects `@`-blocks after each reply finalizes, formats them with sender attribution (`[from <sender>]`), pushes a system message for user visibility, and delivers to the named agent's input channel. The model is an actor system: no blocking, no formal return value, no call stack. Replies come naturally in the normal conversation stream; the receiving agent applies them, follows up, or discards. Consultations can nest freely. Goal sessions (autonomous code-writing sessions) are not part of this protocol.

Primary division of intent: tend uses `@rummage` for code-reality grounding during spec creation; rummage uses `@tend` for intent context when interpreting code behavior; either can consult `@jog` on goal-alignment questions. Under `goal-agents`, any goal agent can be dispatched via `@goal-id`, making the protocol the universal agent-activation channel. The user can ask any agent to consult another without brokering the exchange directly.

📁 `src/main.rs` — `parse_at_commands` (extracts `@`-blocks; accepts any registry-known session ID; standalone `@agent` with body lines is valid; empty blocks are dropped), `dispatch_peer_consultations` (routes to recipient channel via session registry, increments task counter, pushes system message); called from `handle_session_event::Done`; `test_spec_peer_consult_parse_at_tinker`, `test_spec_peer_consult_parse_at_rummage`, `test_spec_peer_consult_parse_at_jog`, `test_spec_peer_consult_parse_prose_before_block_excluded`, `test_spec_peer_consult_parse_block_body_included`, `test_spec_peer_consult_parse_at_without_message_ignored`, `test_spec_peer_consult_parse_multiline_body`, `test_spec_peer_consult_parse_multiple_at_lines`, `test_spec_peer_consult_dispatch_routes_to_correct_channel`, `test_spec_peer_consult_pushes_system_message_for_visibility`  
📁 `src/tend.rs` — `## Peer consultation` section in tend agent prompt; `test_spec_tinker_prompt_describes_peer_consultation_syntax`  
📁 `src/rummage.rs` — `## Peer consultation` section in rummage system prompt; `test_spec_rummage_prompt_describes_peer_consultation_syntax`  
📁 `src/jog.rs` — `## Peer consultation` section in jog system prompt; `test_spec_jog_prompt_describes_peer_consultation_syntax`  
📁 `.tinker/goals/peer-consult.toml` — goal definition

### Root: `goal-storage`
Goals are TOML files under `.tinker/goals/<id>.toml`. Ancestor `.tinker/` directories merge with the cwd's; cwd-most wins on duplicates. A malformed file is reported as an error but doesn't block sibling goals. Session IDs are not persisted to disk — they live in memory for the duration of the tinker process only, so goal files remain the sole source of truth across restarts.

📁 `src/goal.rs` — `Goal` struct (`summary: String` optional in TOML, `children: Vec<ChildLink>` optional in TOML, `related: Vec<RelatedLink>` optional in TOML), `ChildLink` struct (backward-compat: old `children = ["id"]` string arrays accepted and deserialized to `ChildLink { id, reason: "" }`), `RelatedLink` struct, `GOAL_SCHEMA_KEYS_ORDER`, `build_compact_index`, `load_all_goals`, `load_goals`, `save_goal`, `discover_tinker_dirs`, `build_tree`; `test_spec_related_field_absent_loads_as_empty`, `test_spec_related_field_roundtrip_and_summary`, `test_spec_summary_field_absent_loads_as_empty`, `test_spec_summary_field_roundtrip`, `test_spec_compact_index_uses_summary_not_description`, `test_spec_children_old_string_format_loads_as_child_link`, `test_spec_children_new_table_format_loads_correctly`, `test_spec_children_field_absent_loads_as_empty`  
📁 `src/cap.rs` — `Filesystem` trait (read/write/list)  
📁 `src/realfs.rs` — `RealFilesystem: std::fs` impl  
📁 `src/test_utils.rs` — `MockFs` for tests

### Root: `tui`
Three-pane terminal UI (alternate screen, mouse capture). Left pane: REPL conversation with an active-agent tag in the input prompt (e.g. `tend> ` or `rummage> `) showing which agent receives the next message; slash commands `/rummage`, `/tend`, and `/jog` switch the active agent. Right-top pane: selectable goal list + scrollable goal description text below. Right-bottom pane: log output for the selected goal's session. Mouse wheel scrolls whichever pane is under the cursor; auto-follows tail unless the user has scrolled away. Agent dispatch system messages are rendered in grey; trigger reasons are shown in bold in the session log pane.

📁 `src/tui.rs` — layout (`pane_rects`), draw functions, scroll state wiring, active-agent prompt tag rendering; `test_spec_tend_messages_rendered_with_tend_label`  
📁 `src/app.rs` — `App` state, `ActiveAgent` enum, `ScrollState` (auto-follow logic), `Phase` (Initializing/Idle), `Focus`, `current_session_text: HashMap<String, String>`; `goal_queue`/`LoopMode`/`batch_*` fields removed under `goal-agents`  
📁 `src/main.rs` — mouse/keyboard event handlers, `/rummage`/`/tend`/`/jog` slash-command routing, `session_senders: HashMap<String, Sender<String>>` registry in run loop

### Root: `coding-standards` (packaged goal)
Cross-cutting standards enforced by convention: capability-based DI (traits for effects, real impls at composition root), ladder of polymorphism (use the simplest mechanism), test marking convention (`test_spec_` / `test_security_`), security.md per project, investigation markers (`tinker-test-case:`).

📁 `packaged-goals/coding-standards.toml` — the standards text  
📁 `src/cap.rs` — capability interfaces  
📁 `src/opencode.rs` — real `OpenCodeRunner` impl (composition-root only)  
📁 `src/cleanup.rs` — marker enforcement mechanism

### Root: `backends`
Tinker runs against a pluggable backend — the LLM CLI it shells out to — chosen at startup. Two backends exist today: **opencode** (the default) and **claude** (selected with `--claude`). Both are co-equal implementations of the same runner trait, wired to real instances only at the composition root. The opencode backend installs the tend, rummage, and jog personas as opencode agent files (whose YAML frontmatter carries the `task`/`todowrite` denials). The claude backend maps roles to Opus (tend) and Sonnet (goal sessions), passes the persona via `--system-prompt`, resumes sessions with `--resume <session-id>`, parses streaming JSON (`--output-format stream-json --verbose`), and re-imposes the same `task`/`todowrite` denials mechanically via `--disallowedTools` (bypassing the agent file). Tool calls from either backend render in the TUI as compact one-liners.

📁 `src/claude.rs` — `ClaudeRunner` impl (`disallowed_tools: Vec<String>` field, `with_denied_tools()` builder), `claude_command`/`claude_args` (both accept `disallowed_tools: &[String]`, emitted as `--disallowedTools`), model-tier constants, `format_tool_use`; `test_spec_model_tier_constants_use_short_aliases`, `test_spec_with_system_prompt_stores_prompt`, `test_spec_resume_flag_for_session_resumption`, `test_spec_verbose_required_for_stream_json`, `test_spec_system_prompt_passed_when_set`, `test_spec_system_prompt_omitted_when_none`, `test_spec_output_format_stream_json`, `test_spec_disallowed_tools_passed_as_flag`, `test_spec_disallowed_tools_absent_when_empty`, `test_spec_with_denied_tools_stores_list`, `test_spec_format_tool_use_compact_one_liner`  
📁 `src/main.rs` — `--claude` flag detection and runner selection

#### Child: `model-config`
A `.tinker/config.toml` file that overrides which model each tier uses, independently per backend. Six slots: `high` (tend, rummage, jog), `mid` (goal sessions), and `low` (cleanup) under `[claude]` and `[opencode]` sections. Every slot is optional — absent or commented-out slots fall back to the built-in default for that tier, so an empty or missing file changes nothing. On first startup, tinker writes a self-documenting starter template with all slots present but commented out, each annotated with the current built-in default; an already-edited file is never overwritten. The user toggles which model is active by commenting/uncommenting lines.

📁 `src/config.rs` — `ModelConfig`, `BackendModelConfig`, `load_model_config`, `write_starter_template`; `test_spec_load_model_config_returns_default_when_file_absent`, `test_spec_load_model_config_returns_default_on_parse_error`, `test_spec_load_model_config_overrides_present_slots_and_falls_back_for_absent`, `test_spec_load_model_config_parses_all_six_slots`, `test_spec_write_starter_template_skips_when_file_exists`, `test_spec_write_starter_template_written_when_absent`, `test_spec_write_starter_template_contains_all_six_slot_keys`, `test_spec_write_starter_template_all_slot_lines_commented`, `test_spec_write_starter_template_shows_built_in_defaults`  
📁 `src/main.rs` — `config::write_starter_template` and `config::load_model_config` called at startup before runner construction; `model_config.*_*()` accessors replace raw tier constants when building runners  
📁 `.tinker/config.toml` — user-editable model overrides (written on first run, never overwritten)

### Root: `startup-args`
CLI argument validation and help. When tinker receives an unrecognized flag or positional argument, it prints an error naming the bad argument, prints help text listing all valid flags with a link to the repo, and exits with code 1. The same help text is shown for `--help`/`-h`. Both checks happen before the TUI acquires the terminal. Related to `backends` — the known flag set (`--claude`, `--tend-full-goal-context`, `--default-model`, `--help`/`-h`) is owned by `backends`; when a new flag is added there, it must also be registered here.

📁 `src/main.rs` — `print_help` (help text, repo link), `KNOWN_ARGS` constant (allowlist for validation), unknown-arg detection and `process::exit(1)`, `--help`/`-h` early exit; `test_spec_unknown_arg_detection_precedes_tui`, `test_spec_unknown_arg_exits_with_code_1`, `test_spec_help_text_includes_repo_link`, `test_spec_help_flag_lists_all_startup_flags`, `test_spec_help_flag_exits_before_tui`, `test_spec_known_args_list_covers_all_startup_flags`

### Root: `project-indexer` (packaged goal — this agent)
Maintains `AGENTS.md` at the project root — a goals-oriented index mapping each goal to its source files. Triggers reactively when other goal sessions modify files that affect the map.

📁 `AGENTS.md` — this file  
📁 `packaged-goals/project-indexer.toml` — goal definition



## Architecture notes

- **Composition root** in `src/main.rs`: `RealFilesystem` and runner instances (one per model tier, plus one for rummage) are wired at startup. Business logic depends only on traits in `cap.rs`. With `--claude`, `ClaudeRunner` instances replace `RealOpenCodeRunner`.
- **Three model tiers**: tend (smartest), goal sessions (mid), cleanup (cheapest). Built-in defaults defined in `src/opencode.rs:15-20` and `src/claude.rs:15-17`. At startup, `src/config.rs::load_model_config` reads `.tinker/config.toml` and overrides any tier whose slot is set; absent slots fall back to the built-in default. Rummage and jog share the tend tier.
- **Three chat agents**: tend (`src/tend.rs`), rummage (`src/rummage.rs`), and jog (`src/jog.rs`) are peer agents sharing the REPL pane. The active agent is tracked in `App.active_agent`; `/rummage`, `/tend`, and `/jog` slash commands switch between them.
- **Session registry** (`src/main.rs:run_loop`): `session_senders: HashMap<String, Sender<String>>` maps goal-id → message channel. Pre-populated for tend/rummage/jog; goal agents added on first dispatch. `parse_at_commands` accepts any registry-known session ID; `dispatch_peer_consultations` routes to the named entry.
- **Unified event channel**: `session_rx: Receiver<SessionEvent>` in `run_loop` receives events from all sessions. `handle_session_event` dispatches on `goal_id`. Replaces three separate named-agent handlers (`handle_orch_event`, `handle_rummage_event`, `handle_jog_event`).
- **Event loop** (`main.rs:run_loop`): polls terminal events, drains tend/session channels (tend events converted via `tend_event_to_session`), draws TUI on every iteration. Batch-summary pipeline removed; goal agents `@tend` on completion.
- **Goal tree**: `goal.rs::build_tree` builds a parent-child hierarchy from `parent_id` fields. Flat list for selection; tree for display.
- **Security**: documented in `security.md`. Key mitigations include per-file goal loading isolation (T1), per-project session IDs (T2), ancestor-merger transparency (T3), no permission-bypass flags (T4), stderr nulled from subprocesses (T5).
- **Tests**: inline `#[cfg(test)]` modules in each `src/` file, named `test_spec_*` for spec requirements and `test_security_*` for threat mitigations. `MockFs` in `test_utils.rs` provides an in-memory filesystem for testing.
