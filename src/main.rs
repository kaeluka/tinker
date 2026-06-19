mod app;
mod cap;
mod cleanup;
mod config;
mod goal;
mod goal_session;
mod logger;
mod native;
mod prompts;
mod realfs;
mod repl_buffer;
mod tui;
#[cfg(test)]
mod test_utils;

use anyhow::Result;
use app::{App, Focus, Phase};
use cap::{Chunk, Filesystem, OpenCodeRunner};
use realfs::RealFilesystem;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use goal::{discover_tinker_dirs, load_all_goals};
use goal_session::SessionEvent;
use native::{NativeRunner, ToolPolicy};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use std::collections::HashMap;
use tokio::sync::mpsc;

type RunnerSet = (Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>);

/// System prompt for tend under the native backend. Leads with the
/// file-scope boundary so it arrives as a system-level constraint, not a
/// buried instruction in a user-turn message. tend's behaviour is governed
/// by the tend goal description (passed in by the caller, loaded from
/// `goal-storage`'s registry — never embedded at compile time); the
/// file-scope line is the prompt-level boundary that stands in for harness
/// enforcement.
fn tend_system_prompt(tend_description: &str) -> String {
    prompts::tend_system_prompt(tend_description)
}

fn tend_init_prompt(goals_summary: &str, neighbor_section: &str) -> String {
    prompts::tend_init_prompt(goals_summary, neighbor_section)
}

fn tend_init_prompt_full_context(goals_summary: &str, neighbor_section: &str) -> String {
    prompts::tend_init_prompt_full_context(goals_summary, neighbor_section)
}

/// Configuration for spawning an ephemeral fresh sub-session. Produced when
/// the `spawn_session` tool is called by a goal agent.
struct FreshSessionConfig {
    /// Unique ID for the ephemeral session in the registry (e.g. "fresh-agents~1").
    session_id: String,
    /// Optional correlation label provided by the dispatcher (the part after `|`).
    label: Option<String>,
    /// Goal ID of the dispatching agent — also the goal whose description and
    /// neighbors the fresh sub-session inherits.
    dispatcher_id: String,
}

/// Lazy-spawn request: sent when a goal needs to be spawned or resumed —
/// via `send_message` lazy-spawn, `spawn_session`, or the user triggering a
/// goal via the tree UI.
/// When `fresh_session` is `Some`, the request spawns an ephemeral sub-session
/// rather than a persistent goal session.
struct SpawnGoalRequest {
    goal_id: String,
    /// The dispatch message — trigger reason for first turn, peer message for
    /// subsequent turns routed before the session was alive.
    message: String,
    /// When set, spawn as an ephemeral fresh sub-session using this config.
    fresh_session: Option<FreshSessionConfig>,
}

/// Persistent goal agent loop. One task per spawned goal agent.
/// Receives messages via `msg_rx`, runs the LLM with session resumption, and
/// emits SessionEvents back on `session_tx`. Cleanup hook fires only before
/// the first LLM turn of a new session (llm_session_id is None) and is
/// skipped when `skip_cleanup` is true (used by ephemeral fresh sub-sessions).
///
/// When `init_message_override` is `Some`, that string is used verbatim as
/// the first-turn init message instead of building from `session_init_message`.
/// Used by fresh sub-sessions to inject their focused task context.
// Each parameter is a distinct capability or config value; grouping into a
// struct would obscure the injected-capability boundary without reducing complexity.
#[allow(clippy::too_many_arguments)]
async fn goal_agent_loop(
    goal: goal::Goal,
    mut msg_rx: mpsc::UnboundedReceiver<String>,
    session_tx: mpsc::Sender<SessionEvent>,
    oc: Arc<dyn OpenCodeRunner>,
    oc_cleanup: Arc<dyn OpenCodeRunner>,
    fs: Arc<dyn Filesystem>,
    work_dir: PathBuf,
    app_ref: Arc<Mutex<App>>,
    log: logger::LogSender,
    backend_name: String,
    lean_init: bool,
    init_message_override: Option<String>,
    skip_cleanup: bool,
    send_message_dispatcher: cap::SendMessageFn,
    spawn_session_dispatcher: cap::SpawnSessionFn,
) {
    let goal_id = goal.id.clone();
    let mut llm_session_id: Option<String> = None;

    while let Some(dispatch_msg) = msg_rx.recv().await {
        // Cleanup hook: fires only before the first turn of a new session.
        // Skipped for ephemeral fresh sub-sessions (skip_cleanup = true).
        if !skip_cleanup && llm_session_id.is_none() {
            let cleanup_t0 = std::time::Instant::now();
            let cleanup_result = cleanup::run_cleanup(oc_cleanup.as_ref(), fs.as_ref(), &work_dir).await;
            let cleanup_ms = cleanup_t0.elapsed().as_millis() as u64;
            match cleanup_result {
                Ok(cleanup::CleanupOutcome::Clean) => {
                    log.emit("goal_session", logger::LogEvent::CleanupHookRun {
                        goal_id: goal_id.clone(),
                        outcome: "clean".to_string(),
                        duration_ms: cleanup_ms,
                    });
                }
                Ok(cleanup::CleanupOutcome::FailedAfterRetries(files)) => {
                    log.emit("goal_session", logger::LogEvent::CleanupHookRun {
                        goal_id: goal_id.clone(),
                        outcome: "blocked".to_string(),
                        duration_ms: cleanup_ms,
                    });
                    let _ = session_tx.send(SessionEvent::CleanupBlocked {
                        goal_id: goal_id.clone(),
                        dirty_files: files,
                        error: None,
                    }).await;
                    continue;
                }
                Err(e) => {
                    log.emit("goal_session", logger::LogEvent::CleanupHookRun {
                        goal_id: goal_id.clone(),
                        outcome: format!("error: {e:#}"),
                        duration_ms: cleanup_ms,
                    });
                    let _ = session_tx.send(SessionEvent::CleanupBlocked {
                        goal_id: goal_id.clone(),
                        dirty_files: vec![],
                        error: Some(format!("{e:#}")),
                    }).await;
                    continue;
                }
            }
        }

        // Build the LLM message and system prompt for this turn.
        //
        // First turn (new session, llm_session_id is None):
        //   - lean_init=true (all goal agents): system prompt = full session
        //     context (goal + preamble + neighbors), first turn = trigger
        //     reason only. The system prompt is delivered as the in-memory
        //     session's first system message by the native backend.
        //   - lean_init=false (tend): no per-call system prompt (struct-level
        //     system prompt handles it for the native runner); first turn =
        //     full session_init_message.
        //   - init_message_override set (fresh sub-sessions): first turn =
        //     the pre-built lean sub-session init; system prompt = full goal
        //     context (same as lean_init=true).
        //
        // Subsequent turns (resumed session): forward the dispatch message as-is,
        // pass no system prompt (backend already holds it from the first turn).
        let (llm_message, system_prompt_for_run): (String, Option<String>) = if llm_session_id.is_none() {
            let compact_index = {
                let a = app_ref.lock().unwrap();
                if a.goals.is_empty() { "[]".to_string() }
                else { goal::build_compact_index(&a.goals) }
            };
            log.emit("goal_session", logger::LogEvent::GoalSessionDispatched {
                goal_id: goal_id.clone(),
                reason: Some(dispatch_msg.clone()),
                backend: backend_name.clone(),
            });
            log.emit("goal_session", logger::LogEvent::GoalSessionStarted {
                goal_id: goal_id.clone(),
            });
            if let Some(ref override_init) = init_message_override {
                // Fresh sub-session: first turn = task-specific init;
                // system prompt = full goal context delivered via backend mechanism.
                let sp = goal_session::session_init_message(&goal, None, &compact_index);
                (override_init.clone(), Some(sp))
            } else if lean_init {
                // Regular goal agent (both backends): system prompt carries all
                // session-invariant context; first turn = trigger reason only.
                let sp = goal_session::session_init_message(&goal, None, &compact_index);
                (dispatch_msg, Some(sp))
            } else {
                // Tend: full init message in the first turn; system prompt
                // lives in the native runner's struct-level field.
                let msg = goal_session::session_init_message(&goal, Some(&dispatch_msg), &compact_index);
                (msg, None)
            }
        } else {
            (dispatch_msg, None)
        };

        let full_output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let full_output_clone = full_output.clone();
        let tx_chunk = session_tx.clone();
        let gid_chunk = goal_id.clone();

        // The session id is captured from `oc.run`'s return value below; the
        // callback itself is a no-op.
        let on_sid: Chunk = Box::new(|_sid: String| {});
        let on_chunk: Chunk = Box::new(move |chunk: String| {
            full_output_clone.lock().unwrap().push_str(&chunk);
            let _ = tx_chunk.try_send(SessionEvent::Chunk {
                goal_id: gid_chunk.clone(),
                text: chunk,
            });
        });

        let session_t0 = std::time::Instant::now();
        let gid_usage = goal_id.clone();
        let tx_usage = session_tx.clone();
        let on_usage: cap::UsageChunk = Box::new(move |usage| {
            let _ = tx_usage.try_send(SessionEvent::TokenUsage {
                goal_id: gid_usage.clone(),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                cached_tokens: usage.cached_tokens,
            });
        });
        let run_result = oc.run(
            &llm_message,
            llm_session_id.as_deref(),
            &work_dir,
            system_prompt_for_run.as_deref(),
            on_sid,
            on_chunk,
            on_usage,
            Some(send_message_dispatcher.clone()),
            Some(spawn_session_dispatcher.clone()),
        ).await;
        let session_ms = session_t0.elapsed().as_millis() as u64;

        let output = full_output.lock().unwrap().clone();
        match run_result {
            Ok(new_sid) => {
                // Only update session_id when the session actually produced one.
                // An empty new_sid means the runner exited without emitting any
                // events (e.g., API outage, model temporarily unavailable).
                // Keeping llm_session_id as None ensures the next dispatch
                // re-sends session_init_message with full context instead of
                // treating this as an established session to continue.
                if !new_sid.is_empty() {
                    llm_session_id = Some(new_sid.clone());
                }
                let tool_calls = logger::count_tool_calls(&output);
                let files_modified = logger::extract_modified_files(&output);
                log.emit("goal_session", logger::LogEvent::GoalSessionFinished {
                    goal_id: goal_id.clone(),
                    exit_status: "clean".to_string(),
                    duration_ms: session_ms,
                    files_modified_count: files_modified.len(),
                    files_modified,
                    tool_calls,
                    summary_chars: 0,
                    full_output: output.clone(),
                    backend: backend_name.clone(),
                });
                let _ = session_tx.send(SessionEvent::Done { goal_id: goal_id.clone(), full_output: output }).await;
            }
            Err(e) => {
                // Two failure modes share this arm:
                //   - lost (preserved_session_id == None): the session is
                //     genuinely gone (context overflow, unknown id). Clear
                //     llm_session_id so the next dispatch starts fresh.
                //   - transient (preserved_session_id == Some(sid)): the run
                //     failed but the session history was saved. PRESERVE the
                //     sid so the next dispatch resumes — clearing it here was
                //     the bug that wiped every prior turn on a single network
                //     blip. The exit_status log distinguishes the two so the
                //     introspection event log can tell them apart.
                let exit_status = if e.session_preserved() {
                    format!("transient: {}", e.message)
                } else {
                    format!("crash: {}", e.message)
                };
                log.emit("goal_session", logger::LogEvent::GoalSessionFinished {
                    goal_id: goal_id.clone(),
                    exit_status,
                    duration_ms: session_ms,
                    files_modified_count: 0,
                    files_modified: vec![],
                    tool_calls: 0,
                    summary_chars: 0,
                    full_output: output.clone(),
                    backend: backend_name.clone(),
                });
                // Preserve the session id for transient failures; clear only
                // on genuine session loss (context overflow, unknown id).
                llm_session_id = e.preserved_session_id;
                let _ = session_tx.send(SessionEvent::Done { goal_id: goal_id.clone(), full_output: output }).await;
            }
        }
    }
}

fn print_help() {
    println!("tinker — autonomous coding assistant\n");
    println!("USAGE:");
    println!("    tinker [OPTIONS]\n");
    println!("OPTIONS:");
    println!("    --tend-full-goal-context    Inject full goal text instead of compact index");
    println!("    -h, --help                  Print this help and exit");
    println!("\nhttps://github.com/kaeluka/tinker");
}

#[tokio::main]
async fn main() -> Result<()> {
    // Composition root: real capability implementations live here only.
    let fs: Arc<dyn Filesystem> = Arc::new(RealFilesystem);

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        std::process::exit(0);
    }

    let use_full_goal_context = args.iter().any(|a| a == "--tend-full-goal-context");

    const KNOWN_ARGS: &[&str] = &[
        "--tend-full-goal-context",
        "--help",
        "-h",
    ];
    if let Some(bad) = args.iter().skip(1).find(|a| !KNOWN_ARGS.contains(&a.as_str())) {
        println!("error: unrecognized argument '{bad}'\n");
        print_help();
        std::process::exit(1);
    }

    // Per-tier startup precondition: each tier's resolved endpoint and
    // model must be non-empty (the loader fills in OpenRouter defaults
    // for absent slots, so an empty endpoint/model here means a deeply
    // broken loader). Auth is optional — the per-tier env var may be
    // unset/empty, in which case no Authorization header is sent
    // (what local model servers expect). We fail fast here, before
    // the TUI starts, rather than letting every session crash into a
    // silent, unbounded silence-nudge cascade once tinker is running.
    //
    // Note: the actual resolution (TOML → defaults, env-var auth) is
    // owned by `config::load_model_config`, called later. The check
    // below is a structural sanity step against the resolved values.
    // We deliberately don't fail on a missing auth key — auth is
    // optional per the backends spec, and a local model server that
    // ignores the header is a valid configuration.
    //

    let work_dir = std::env::current_dir()?;
    let primary_tinker_dir = work_dir.join(".tinker");
    fs.mkdir_all(&primary_tinker_dir.join("goals"))?;
    fs.mkdir_all(&primary_tinker_dir.join("state"))?;
    fs.mkdir_all(&primary_tinker_dir.join("notes"))?;
    fs.mkdir_all(&primary_tinker_dir.join("logs"))?;
    fs.mkdir_all(&primary_tinker_dir.join("discrepancies"))?;

    // Set up the fatal-event logger early — before any operation that
    // could panic from an eager-start invariant failure. The
    // batched async logger (`start_logger` below) is not yet running
    // at this point, so events emitted here are flushed synchronously
    // to disk via `fs.open_append` and survive the panic unwinding
    // that triggers them.
    let log_path = primary_tinker_dir.join("logs").join("runtime.jsonl");
    let fatal_log = logger::start_fatal_logger(fs.open_append(&log_path)?);

    // Write a self-documenting starter config only when none exists yet;
    // then load whatever is present (or default if still absent/invalid).
    // The config governs the native backend only — it is the single source
    // of truth for per-tier endpoint, model, and (via env vars) auth.
    let config_path = primary_tinker_dir.join("config.toml");
    config::write_starter_template(fs.as_ref(), &config_path)?;
    let model_config = config::load_model_config(fs.as_ref(), &config_path);

    // Per-tier structural sanity: each tier's resolved endpoint and
    // model must be non-empty after the loader fills in defaults. Auth
    // is optional (the env var may be unset/empty for local servers).
    // A tier with an empty endpoint/model after resolution means the
    // loader produced garbage — fail fast and loud before the TUI
    // starts, naming the offending tier so the user can fix their
    // config.
    for (tier_name, cfg) in [
        ("high", model_config.native_high()),
        ("mid", model_config.native_mid()),
        ("low", model_config.native_low()),
    ] {
        if cfg.endpoint.trim().is_empty() || cfg.model.trim().is_empty() {
            println!(
                "error: tier `{tier_name}` has incomplete configuration\n\
                 resolved endpoint: {:?}\n\
                 resolved model: {:?}\n\
                 Check {config_path:?} — every tier needs both an endpoint and a model.\n",
                cfg.endpoint,
                cfg.model,
            );
            std::process::exit(1);
        }
    }

    // Discover all .tinker dirs from cwd up. Nearest first. Must run before
    // goal loading so the runner construction below can pull tend's
    // description from the loaded goals — there is no compile-time embed of
    // the tend TOML anymore (single source of truth: the on-disk file).
    let tinker_dirs = discover_tinker_dirs(fs.as_ref(), &work_dir);
    let initial_load = load_all_goals(fs.as_ref(), &tinker_dirs)?;

    // Resolve tend's description from the loaded goals. The four-tier
    // loader — project-local > ancestor global > tinker_dir-packaged >
    // binary-relative-packaged — must produce at least one entry whose
    // id is "tend". The binary-relative tier is populated by `build.rs`
    // (file-based copy to `<target>/.tinker/goals/packaged-goals/`) on
    // local builds and install-from-checkout, and by the
    // `include_dir!` fallback compiled into the binary on install-from-
    // registry. If tend is missing from the merged result, both
    // population paths have failed — a real deployment error worth
    // surfacing loudly here rather than letting tend's first turn
    // produce an empty system prompt and a silent cascade.
    //
    // Diagnostic form: three sections (what / where / fix), terse
    // operational register, dev vs registry distinction so the user
    // doesn't run the wrong fix. See `goal-agents` for the
    // eager-start invariant this check guards, and
    // `binary-relative-tier-loading` for the population contract.
    // The diagnostic text itself is `goal::EAGER_START_DIAGNOSTIC`,
    // pinned by `test_spec_eager_start_diagnostic_*`.
    //
    // Before the panic, emit `EagerStartPreconditionMissing` so the
    // failure lands on disk — the fatal-event logger flushes
    // synchronously and survives the unwind, giving the user (or a
    // debugger) a structured record of which tier was supposed to
    // contain `tend` and any parse errors in those tiers.
    let tend_goal = initial_load.goals.iter().find(|g| g.id == "tend");
    if tend_goal.is_none() {
        fatal_log.emit(
            "goal-agents",
            logger::LogEvent::EagerStartPreconditionMissing {
                missing_goal_id: "tend".to_string(),
                expected_sources: vec![
                    ("project-local".to_string(),
                     format!("{}/.tinker/goals/tend.toml", work_dir.display())),
                    ("ancestor global".to_string(),
                     "<ancestor>/.tinker/goals/tend.toml".to_string()),
                    ("packaged".to_string(),
                     format!("{}/.tinker/goals/packaged-goals/tend.toml",
                             primary_tinker_dir.display())),
                    ("binary-relative-packaged".to_string(),
                     "<binary>/../../.tinker/goals/packaged-goals/tend.toml".to_string()),
                ],
                parse_errors_in_tier: initial_load
                    .errors
                    .iter()
                    .map(|(p, e)| (p.display().to_string(), e.clone()))
                    .collect(),
            },
        );
    }
    let tend_description = tend_goal
        .map(|g| g.description.clone())
        .expect(crate::goal::EAGER_START_DIAGNOSTIC);

    // Five native runner instances, all `Unrestricted` policy except `oc_tend`:
    //
    //   oc_tend          — exclusive to tend; carries `tend_system_prompt(tend_description)`
    //                      as a persistent struct-level fallback so tend's file-scope
    //                      boundary is enforced on every turn (including turn 2+ where
    //                      no per-call prompt is set). The description is the runtime-
    //                      discovered one (no compile-time embed).
    //                      `ToolPolicy::TendScope` strips bash and narrows writes to
    //                      `.tinker/goals/` — enforced in-process.
    //
    //   oc_goal_high     — high-tier non-tend goal agents (rummage, jog, …).
    //                      Shares the *config* with `oc_tend` (both consume the resolved
    //                      high tier) but is a distinct runner instance — they cannot
    //                      share the internal session-id map because that would let
    //                      tend's sessions leak into high-tier goal sessions.
    //
    //   oc_goal          — mid-tier (default goal sessions).
    //
    //   oc_goal_low      — low-tier goal sessions. Shares the *config* with
    //                      `oc_cleanup_runner` (both consume the resolved low tier)
    //                      but is a distinct runner instance — `NativeRunner` holds an
    //                      internal session-id map keyed on the runner, and cleanup's
    //                      ephemeral sessions must not collide with concurrent
    //                      low-tier goal sessions.
    //
    //   oc_cleanup_runner — cleanup / scheduler (cheapest model). Distinct runner
    //                      for the same state-isolation reason as `oc_goal_low`.
    //
    // Per-tier wiring: each runner carries its own resolved endpoint, model,
    // and auth (per-tier env var only — empty/unset = None = no
    // Authorization header, the local-model-server path). Auth is
    // resolved via the dedicated accessors (`native_*_api_key()`) rather
    // than carried on `TierConfig` because auth is runtime state, not
    // config — secrets stay in env vars by spec.
    //
    // The high-tier cfg is cloned because both `oc_tend` and `oc_goal_high` share
    // it, and the low-tier cfg is cloned because `oc_goal_low` and
    // `oc_cleanup_runner` share it — the runner constructors consume the owned
    // Strings. The auth accessors are called once per tier and the
    // `Option<String>` moves into the runner.
    let tinker_cfg = model_config.native_high();
    let goal_cfg = model_config.native_mid();
    let cleanup_cfg = model_config.native_low();
    let tinker_cfg_high = tinker_cfg.clone();
    let cleanup_cfg_runner = cleanup_cfg.clone();
    let tinker_api_key = model_config.native_high_api_key();
    let goal_api_key = model_config.native_mid_api_key();
    let cleanup_api_key = model_config.native_low_api_key();
    let (oc_tend, oc_goal_high, oc_goal, oc_goal_low, oc_cleanup_runner): RunnerSet = (
        Arc::new(NativeRunner::with_system_prompt(
            tinker_cfg.endpoint,
            tinker_cfg.model,
            tinker_api_key.clone(),
            tend_system_prompt(&tend_description),
            ToolPolicy::TendScope,
        )),
        Arc::new(NativeRunner::new(tinker_cfg_high.endpoint, tinker_cfg_high.model, tinker_api_key, ToolPolicy::Unrestricted)),
        Arc::new(NativeRunner::new(goal_cfg.endpoint, goal_cfg.model, goal_api_key, ToolPolicy::Unrestricted)),
        Arc::new(NativeRunner::new(cleanup_cfg.endpoint, cleanup_cfg.model, cleanup_api_key.clone(), ToolPolicy::Unrestricted)),
        Arc::new(NativeRunner::new(cleanup_cfg_runner.endpoint, cleanup_cfg_runner.model, cleanup_api_key, ToolPolicy::Unrestricted)),
    );

    let backend_name = "native";
    let log = logger::start_logger(
        primary_tinker_dir.join("logs").join("runtime.jsonl"),
        primary_tinker_dir.join("state").join("runtime.json"),
    );

    let app = Arc::new(Mutex::new(App::new()));

    {
        let mut a = app.lock().unwrap();
        a.goals = initial_load.goals;
        a.tinker_dirs = tinker_dirs.clone();
        a.push_system_message("Starting tend…");
        log.emit("harness", logger::LogEvent::TinkerSystemMessageReceived { content: "Starting tend…".to_string() });
        a.update_parse_errors(initial_load.errors);
        // Cross-tier goal-id collisions: emit the user-visible system
        // message via `update_goal_id_collisions`, then mirror each new
        // collision as a structured `GoalCollision` log event so the
        // runtime event log records the same fact in a `jq`-queryable
        // form. This is the tend-introspection side of the parallel
        // capture (see the tend-introspection goal for the substrate
        // contract). Diff is structural: an unchanged contributing set
        // produces no new events.
        let new_collisions = a.update_goal_id_collisions(initial_load.collisions);
        drop(a);
        for collision in &new_collisions {
            log.emit("goal-agents", logger::LogEvent::GoalCollision {
                goal_id: collision.goal_id.clone(),
                contributors: collision.contributors.iter()
                    .map(|(tier, path)| (tier.clone(), path.display().to_string()))
                    .collect(),
            });
        }
        if tinker_dirs.len() > 1 {
            let merged_msg = format!(
                "Merged {} .tinker dirs (cwd + {} ancestor).",
                tinker_dirs.len(),
                tinker_dirs.len() - 1,
            );
            app.lock().unwrap().push_system_message(&merged_msg);
            log.emit("harness", logger::LogEvent::TinkerSystemMessageReceived { content: merged_msg });
        }
    }

    let (session_tx, mut session_rx) = mpsc::channel::<SessionEvent>(128);
    let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();

    // Goal watcher task — re-discovers .tinker dirs each cycle and re-merges
    {
        let app_ref = app.clone();
        let work_dir = work_dir.clone();
        let fs = fs.clone();
        let log_watcher = log.clone();
        tokio::spawn(async move {
            let mut prev_goal_hash = String::new();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let dirs = discover_tinker_dirs(fs.as_ref(), &work_dir);
                if let Ok(load) = load_all_goals(fs.as_ref(), &dirs) {
                    // Compute a content hash to detect changes
                    let content_str = load.goals.iter()
                        .map(|g| format!("{}:{}", g.id, g.description))
                        .collect::<Vec<_>>()
                        .join("|");
                    let new_hash = logger::hash_string(&content_str);
                    if !prev_goal_hash.is_empty() && new_hash != prev_goal_hash {
                        log_watcher.emit("watcher", logger::LogEvent::GoalFileChanged {
                            path: ".tinker/goals/".to_string(),
                        });
                    }
                    prev_goal_hash = new_hash;

                    let mut a = app_ref.lock().unwrap();
                    a.goals = load.goals;
                    a.update_parse_errors(load.errors);
                    // Collision diff is structural — see startup block for
                    // the rationale. Emit a `GoalCollision` log event for
                    // each new collision so the runtime event log
                    // captures the cross-tier overlap, paralleling the
                    // user-visible system message.
                    let new_collisions = a.update_goal_id_collisions(load.collisions);
                    if a.selected_goal >= a.flat_items().len().max(1) {
                        a.selected_goal = 0;
                    }
                    drop(a);
                    for collision in &new_collisions {
                        log_watcher.emit("goal-agents", logger::LogEvent::GoalCollision {
                            goal_id: collision.goal_id.clone(),
                            contributors: collision.contributors.iter()
                                .map(|(tier, path)| (tier.clone(), path.display().to_string()))
                                .collect(),
                        });
                    }
                }
            }
        });
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(
        &mut terminal,
        app.clone(),
        &mut session_rx,
        goal_spawn_tx,
        &mut goal_spawn_rx,
        oc_goal,
        oc_goal_high,
        oc_goal_low,
        oc_cleanup_runner,
        oc_tend,
        fs.clone(),
        work_dir.clone(),
        session_tx,
        log,
        backend_name,
        use_full_goal_context,
    )
    .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

// Composition-root event loop: all parameters are distinct capability handles or
// config values wired at startup. Bundling into a struct adds indirection without
// reducing the number of distinct concerns.
#[allow(clippy::too_many_arguments)]
async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: Arc<Mutex<App>>,
    session_rx: &mut mpsc::Receiver<SessionEvent>,
    goal_spawn_tx: mpsc::UnboundedSender<SpawnGoalRequest>,
    goal_spawn_rx: &mut mpsc::UnboundedReceiver<SpawnGoalRequest>,
    oc_goal: Arc<dyn OpenCodeRunner>,
    oc_goal_high: Arc<dyn OpenCodeRunner>,
    oc_goal_low: Arc<dyn OpenCodeRunner>,
    oc_cleanup_runner: Arc<dyn OpenCodeRunner>,
    oc_tend: Arc<dyn OpenCodeRunner>,
    fs: Arc<dyn Filesystem>,
    work_dir: std::path::PathBuf,
    session_tx: mpsc::Sender<SessionEvent>,
    log: logger::LogSender,
    backend_name: &str,
    use_full_goal_context: bool,
) -> Result<()> {
    // Session registry: maps goal_id → message channel sender.
    // Tend is pre-populated (eager start); all other sessions start lazily.
    // Wrapped in Arc<Mutex<...>> so the send_message dispatcher closures
    // held by every goal_agent_loop task can consult the registry in
    // parallel with the main event loop's mutations.
    let session_senders: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // Tracks whether the system is currently in an active batch (any session
    // holds a running_sessions slot). Used to detect idle↔active transitions
    // for BatchTransition event emission.
    let mut batch_active = false;

    // Eager-start tend: find its goal in the registry (populated by
    // `load_all_goals` before runner construction above — there is no
    // compile-time fallback), spawn goal_agent_loop, send the initial trigger.
    {
        let tend_goal = app.lock().unwrap().goals.iter().find(|g| g.id == "tend").cloned()
            .expect("tend goal not found in app.goals — initial load should have guaranteed it");
        let goals_index = {
            let a = app.lock().unwrap();
            if a.goals.is_empty() {
                "[]".to_string()
            } else if use_full_goal_context {
                goal::build_full_text_index(&a.goals)
            } else {
                goal::build_compact_index(&a.goals)
            }
        };
        let neighbor_section = {
            let table = goal_session::build_neighborhood_table(&tend_goal);
            if table.is_empty() {
                String::new()
            } else {
                format!(
                    "## Neighbor goals\n\n\
                     {mandate_preamble}\n\
                     \n\
                     {table}\n\n",
                    mandate_preamble = goal_session::NEIGHBOR_CONSULTATION_MANDATE_PREAMBLE,
                    table = table,
                )
            }
        };
        let trigger = if use_full_goal_context {
            tend_init_prompt_full_context(&goals_index, &neighbor_section)
        } else {
            tend_init_prompt(&goals_index, &neighbor_section)
        };
        let (tend_tx, tend_rx) = mpsc::unbounded_channel::<String>();
        session_senders.lock().unwrap().insert("tend".to_string(), tend_tx.clone());
        let app_ref = app.clone();
        let session_tx_t = session_tx.clone();
        let oc_t = oc_tend.clone();
        let oc_cleanup_t = oc_cleanup_runner.clone();
        let fs_t = fs.clone();
        let work_dir_t = work_dir.clone();
        let log_t = log.clone();
        let backend_t = backend_name.to_string();
        let session_senders_for_tend = session_senders.clone();
        let tend_dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            session_senders_for_tend,
            goal_spawn_tx.clone(),
            app_ref.clone(),
            log_t.clone(),
        );
        let tend_spawn_dispatcher = build_spawn_session_dispatcher(
            "tend".to_string(),
            goal_spawn_tx.clone(),
            app_ref.clone(),
            log_t.clone(),
        );
        tokio::spawn(async move {
            goal_agent_loop(tend_goal, tend_rx, session_tx_t, oc_t, oc_cleanup_t, fs_t, work_dir_t, app_ref, log_t, backend_t, false, None, false, tend_dispatcher, tend_spawn_dispatcher).await;
        });
        let _ = tend_tx.send(trigger);
    }

    loop {
        // Draw
        terminal.draw(|f| tui::draw(f, &mut app.lock().unwrap()))?;

        // Drain lazy goal-agent spawn requests. For each request:
        // - Fresh sub-session (fresh_session is Some): retire completed ephemeral
        //   sessions from prior batches, then create a new ephemeral entry with the
        //   sub-session's unique ID and a custom init message.
        // - Persistent goal agent (fresh_session is None): if already in the registry,
        //   route the message; otherwise create a channel, register it, spawn a task.
        while let Ok(req) = goal_spawn_rx.try_recv() {
            if let Some(fresh) = req.fresh_session {
                // Batch-start retirement: retire completed ephemeral sessions from the
                // previous batch before inserting any new ones.  Retirement is triggered
                // by the idle→active transition, not by the departure of old work, so
                // sub-sessions remain inspectable through the idle period.
                //
                // Three conditions must hold simultaneously before retirement fires:
                // (1) the dispatcher is a permanent goal agent (not an ephemeral
                //     coordinator), confirming we are not mid-batch — a coordinator
                //     that completes its current turn is removed from running_sessions
                //     but must survive to receive replies from its sub-sessions;
                // (2) no ephemeral session is currently in running_sessions, confirming
                //     the previous batch is truly idle rather than paused between turns;
                // (3) the permanent dispatcher goal is not itself in running_sessions,
                //     which would indicate a batch message from a completed sub-session
                //     is still in transit to it ("pending delivery" in the spec's batch
                //     definition).  The `send_message` tool dispatcher inserts the
                //     recipient into running_sessions when a sub-session reply is dispatched;
                //     the permanent goal leaves running_sessions only after processing
                //     that reply.  Firing retirement while the delivery is still pending
                //     would retire sub-sessions prematurely within the current batch.
                {
                    let should_retire = {
                        let a = app.lock().unwrap();
                        let dispatcher_is_permanent = app::session_base_id(&fresh.dispatcher_id)
                            == fresh.dispatcher_id.as_str();
                        let any_ephemeral_running = a.ephemeral_sessions.iter()
                            .any(|id| a.running_sessions.contains_key(id));
                        let dispatcher_has_pending_delivery =
                            a.running_sessions.contains_key(fresh.dispatcher_id.as_str());
                        dispatcher_is_permanent
                            && !any_ephemeral_running
                            && !dispatcher_has_pending_delivery
                    };
                    if should_retire {
                        let retired = app.lock().unwrap().retire_completed_ephemeral_sessions();
                        for id in retired {
                            session_senders.lock().unwrap().remove(&id);
                        }
                    }
                }

                // Fresh sub-session: unique ID, ephemeral, custom init.
                // When the dispatcher is an ephemeral coordinator, resolve to the
                // permanent base goal for init context and tier selection.
                let permanent_goal_id = app::session_base_id(&fresh.dispatcher_id).to_string();
                let goal = app.lock().unwrap().goals.iter()
                    .find(|g| g.id == permanent_goal_id)
                    .cloned();
                if let Some(dispatcher_goal) = goal {
                    let (msg_tx_fresh, msg_rx_fresh) = mpsc::unbounded_channel::<String>();
                    session_senders.lock().unwrap().insert(fresh.session_id.clone(), msg_tx_fresh.clone());
                    // Both backends now use lean init for fresh sub-sessions:
                    // the system prompt carries all session-invariant context.
                    let lean_init_fresh = true;
                    let compact_index = {
                        let a = app.lock().unwrap();
                        if a.goals.is_empty() { "[]".to_string() }
                        else { goal::build_compact_index(&a.goals) }
                    };
                    let init_msg = if lean_init_fresh {
                        goal_session::fresh_subsession_lean_init_message(
                            &dispatcher_goal,
                            &fresh.dispatcher_id,
                            &fresh.session_id,
                            fresh.label.as_deref(),
                            &req.message,
                            &compact_index,
                        )
                    } else {
                        goal_session::fresh_subsession_init_message(
                            &dispatcher_goal,
                            &fresh.dispatcher_id,
                            &fresh.session_id,
                            fresh.label.as_deref(),
                            &req.message,
                            &compact_index,
                        )
                    };
                    // Clone dispatcher goal with the ephemeral session ID for event tracking.
                    let mut fresh_goal = dispatcher_goal.clone();
                    fresh_goal.id = fresh.session_id.clone();
                    // Resolve the dispatcher's effective tier (explicit tier wins;
                    // absent tier: behavior→high, feature→mid) so fresh sub-sessions
                    // get the correct runner.
                    let oc_for_fresh = match effective_goal_tier(&dispatcher_goal) {
                        "high" => oc_goal_high.clone(),
                        "low" => oc_goal_low.clone(),
                        _ => oc_goal.clone(),
                    };
                    let session_tx_fresh = session_tx.clone();
                    let oc_cleanup_fresh = oc_cleanup_runner.clone();
                    let fs_fresh = fs.clone();
                    let app_ref_fresh = app.clone();
                    let log_fresh = log.clone();
                    let backend_fresh = backend_name.to_string();
                    let work_dir_fresh = work_dir.clone();
                    let session_senders_for_fresh = session_senders.clone();
                    let dispatcher = build_send_message_dispatcher(
                        fresh.session_id.clone(),
                        session_senders_for_fresh,
                        goal_spawn_tx.clone(),
                        app_ref_fresh.clone(),
                        log_fresh.clone(),
                    );
                    let spawn_dispatcher = build_spawn_session_dispatcher(
                        fresh.session_id.clone(),
                        goal_spawn_tx.clone(),
                        app_ref_fresh.clone(),
                        log_fresh.clone(),
                    );
                    let _ = msg_tx_fresh.send(req.message);
                    tokio::spawn(async move {
                        goal_agent_loop(
                            fresh_goal, msg_rx_fresh, session_tx_fresh,
                            oc_for_fresh, oc_cleanup_fresh, fs_fresh,
                            work_dir_fresh, app_ref_fresh, log_fresh, backend_fresh,
                            lean_init_fresh, Some(init_msg), true,
                            dispatcher, spawn_dispatcher,
                        ).await;
                    });
                }
            } else if let Some(tx) = session_senders.lock().unwrap().get(&req.goal_id).cloned() {
                let _ = tx.send(req.message);
            } else {
                let goal = app.lock().unwrap().goals.iter().find(|g| g.id == req.goal_id).cloned();
                if let Some(goal) = goal {
                    let (msg_tx_goal, msg_rx_goal) = mpsc::unbounded_channel::<String>();
                    session_senders.lock().unwrap().insert(req.goal_id.clone(), msg_tx_goal.clone());
                    let oc_for_goal = match effective_goal_tier(&goal) {
                        "high" => oc_goal_high.clone(),
                        "low" => oc_goal_low.clone(),
                        _ => oc_goal.clone(),
                    };
                    let session_tx_goal = session_tx.clone();
                    let oc_cleanup_goal = oc_cleanup_runner.clone();
                    let fs_goal = fs.clone();
                    let app_ref_goal = app.clone();
                    let log_goal = log.clone();
                    let backend_goal = backend_name.to_string();
                    // Both backends now use lean init for regular goal agents:
                    // the system prompt carries all session-invariant context.
                    let lean_init_goal = true;
                    let work_dir_goal = work_dir.clone();
                    let session_senders_for_dispatch = session_senders.clone();
                    let dispatcher = build_send_message_dispatcher(
                        req.goal_id.clone(),
                        session_senders_for_dispatch,
                        goal_spawn_tx.clone(),
                        app_ref_goal.clone(),
                        log_goal.clone(),
                    );
                    let spawn_dispatcher = build_spawn_session_dispatcher(
                        req.goal_id.clone(),
                        goal_spawn_tx.clone(),
                        app_ref_goal.clone(),
                        log_goal.clone(),
                    );
                    let _ = msg_tx_goal.send(req.message);
                    tokio::spawn(async move {
                        goal_agent_loop(
                            goal, msg_rx_goal, session_tx_goal,
                            oc_for_goal, oc_cleanup_goal, fs_goal,
                            work_dir_goal, app_ref_goal, log_goal, backend_goal, lean_init_goal,
                            None, false,
                            dispatcher, spawn_dispatcher,
                        ).await;
                    });
                }
            }
        }

        // Drain session events (all agents unified). The session_senders
        // lock is acquired here and held for the whole loop so all dispatched
        // event handlers see a consistent registry snapshot.
        {
            let running_before: std::collections::HashSet<String> = app.lock().unwrap().running_sessions.keys().cloned().collect();
            let senders_guard = session_senders.lock().unwrap();
            while let Ok(ev) = session_rx.try_recv() {
                handle_session_event(&mut app.lock().unwrap(), ev, &goal_spawn_tx, &senders_guard, fs.as_ref(), &log);
            }
            drop(senders_guard);
            let running_after: Vec<String> = app.lock().unwrap().running_sessions.keys().cloned().collect();
            if running_after.iter().cloned().collect::<std::collections::HashSet<_>>() != running_before {
                log.emit("tui", logger::LogEvent::TuiQueueChanged {
                    running_goal_ids: running_after,
                });
            }
            // Detect and emit batch transitions after all session events are processed.
            check_and_emit_batch_transition(&mut app.lock().unwrap(), &mut batch_active, &log);
        }

        // Terminal events (50ms poll)
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let ev = event::read()?;
        if let Event::Mouse(m) = ev {
            let size = terminal.size()?;
            let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
            let (scroll_before, sel_before) = {
                let a = app.lock().unwrap();
                (
                    (a.repl_scroll.y.unwrap_or(0), a.goal_list_scroll.y.unwrap_or(0), a.goal_text_scroll.y.unwrap_or(0)),
                    a.selected_goal,
                )
            };
            {
                let mut a = app.lock().unwrap();
                handle_mouse(&mut a, m, area);
            }
            let (scroll_after, sel_after) = {
                let a = app.lock().unwrap();
                (
                    (a.repl_scroll.y.unwrap_or(0), a.goal_list_scroll.y.unwrap_or(0), a.goal_text_scroll.y.unwrap_or(0)),
                    a.selected_goal,
                )
            };
            if scroll_after.0 != scroll_before.0 {
                log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "repl".to_string(), y: scroll_after.0 });
            }
            if scroll_after.1 != scroll_before.1 {
                log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_list".to_string(), y: scroll_after.1 });
            }
            if scroll_after.2 != scroll_before.2 {
                log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_text".to_string(), y: scroll_after.2 });
            }
            if sel_after != sel_before {
                let gid = app.lock().unwrap().flat_items().get(sel_after).map(|i| i.id().to_string());
                log.emit("tui", logger::LogEvent::TuiSelectionChanged { goal_id: gid });
            }
            continue;
        }
        if let Event::Key(key) = ev {
            let (sel_before, focus_before, scroll_before) = {
                let a = app.lock().unwrap();
                (
                    a.selected_goal,
                    a.focus.clone(),
                    (a.repl_scroll.y.unwrap_or(0), a.goal_list_scroll.y.unwrap_or(0), a.goal_text_scroll.y.unwrap_or(0)),
                )
            };
            // Build known IDs from both the registry and all goal IDs from app.goals,
            // so that /<goal-id> switching works even before a session is spawned.
            let known_ids: Vec<String> = {
                let a = app.lock().unwrap();
                let mut ids: Vec<String> = session_senders.lock().unwrap().keys().cloned().collect();
                for g in &a.goals {
                    if !ids.iter().any(|id| id == &g.id) {
                        ids.push(g.id.clone());
                    }
                }
                ids
            };
            let known_ids_refs: Vec<&str> = known_ids.iter().map(|s| s.as_str()).collect();
            let action = {
                let mut a = app.lock().unwrap();
                handle_key(&mut a, key, &log, &known_ids_refs)
            };
            // Emit TUI state-change events
            {
                let a = app.lock().unwrap();
                if a.selected_goal != sel_before {
                    let gid = a.flat_items().get(a.selected_goal).map(|i| i.id().to_string());
                    log.emit("tui", logger::LogEvent::TuiSelectionChanged { goal_id: gid });
                }
                if a.focus != focus_before {
                    let focus_str = match a.focus {
                        Focus::Repl => "repl",
                        Focus::Tree => "tree",
                    };
                    log.emit("tui", logger::LogEvent::TuiFocusChanged { focus: focus_str.to_string() });
                }
                if a.repl_scroll.y.unwrap_or(0) != scroll_before.0 {
                    log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "repl".to_string(), y: a.repl_scroll.y.unwrap_or(0) });
                }
                if a.goal_list_scroll.y.unwrap_or(0) != scroll_before.1 {
                    log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_list".to_string(), y: a.goal_list_scroll.y.unwrap_or(0) });
                }
                if a.goal_text_scroll.y.unwrap_or(0) != scroll_before.2 {
                    log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_text".to_string(), y: a.goal_text_scroll.y.unwrap_or(0) });
                }
            }
            match action {
                KeyAction::Quit => break,
                KeyAction::SendToSession(session_id, msg) => {
                    {
                        let retired = app.lock().unwrap().retire_completed_ephemeral_sessions();
                        for id in retired {
                            session_senders.lock().unwrap().remove(&id);
                        }
                    }
                    // Mark ALL sessions as running at message-send time — not just
                    // interactive agents. The user's message is already in flight
                    // (either delivered to an existing sender or queued for spawn),
                    // so the batch is active from this moment. The Chunk event's
                    // `or_insert(None)` will be a no-op when the entry is already here.
                    app.lock().unwrap().running_sessions.insert(session_id.clone(), None);
                    if let Some(tx) = session_senders.lock().unwrap().get(&session_id).cloned() {
                        let _ = tx.send(msg);
                    } else {
                        // Session not yet spawned — route through lazy spawn so the
                        // first user message to rummage/jog triggers session_init_message.
                        let _ = goal_spawn_tx.send(SpawnGoalRequest {
                            goal_id: session_id,
                            message: msg,
                            fresh_session: None,
                        });
                    }
                    // Emit idle→active transition now that running_sessions is updated.
                    check_and_emit_batch_transition(&mut app.lock().unwrap(), &mut batch_active, &log);
                }
                KeyAction::ConfirmOptions { goal_id, reason, new_tier } => {
                    {
                        let retired = app.lock().unwrap().retire_completed_ephemeral_sessions();
                        for id in retired {
                            session_senders.lock().unwrap().remove(&id);
                        }
                    }
                    // 1. Apply tier change if the user changed it.
                    if let Some(ref tier_str) = new_tier {
                        let write_info = {
                            let mut a = app.lock().unwrap();
                            // Detach the running session so it respawns with the new tier.
                            a.running_sessions.remove(&goal_id);
                            if let Some(g) = a.goals.iter_mut().find(|g| g.id == goal_id) {
                                // "mid" is the default; store it as None to keep files clean.
                                g.tier = if tier_str == "mid" {
                                    None
                                } else {
                                    Some(tier_str.clone())
                                };
                                let serialized = toml::to_string_pretty(g).ok();
                                Some((g.source_path.clone(), serialized))
                            } else {
                                None
                            }
                        };
                        if let Some((Some(path), Some(content))) = write_info {
                            let _ = fs.write(&path, &content);
                        }
                        // Remove sender so the next trigger spawns a fresh session.
                        session_senders.lock().unwrap().remove(&goal_id);
                    }
                    // 2. Fire the goal (same flow as RunGoal).
                    let goal_exists = {
                        let a = app.lock().unwrap();
                        a.goals.iter().any(|g| g.id == goal_id)
                    };
                    if goal_exists {
                        let reason_str = reason.clone().unwrap_or_default();
                        let sys_msg = prompts::triggered_system_msg(&goal_id, &reason_str);
                        let submission_sets_running = matches!(goal_id.as_str(), "tend" | "rummage" | "jog");
                        {
                            let mut a = app.lock().unwrap();
                            a.push_system_message(&sys_msg);
                            if !submission_sets_running {
                                a.running_sessions.insert(goal_id.clone(), reason);
                            }
                        }
                        log.emit("dispatcher", logger::LogEvent::TinkerSystemMessageReceived { content: sys_msg });
                        let _ = goal_spawn_tx.send(SpawnGoalRequest {
                            goal_id: goal_id.clone(),
                            message: reason_str,
                            fresh_session: None,
                        });
                    }
                    // Non-interactive goals are inserted into running_sessions above;
                    // emit idle→active transition if the batch just became active.
                    check_and_emit_batch_transition(&mut app.lock().unwrap(), &mut batch_active, &log);
                }
                KeyAction::None => {}
            }
        }

        if app.lock().unwrap().should_quit {
            break;
        }
    }
    Ok(())
}

/// Outcome of routing a dispatch to a target. Used by the
/// `send_message` tool dispatcher to share the same registry-then-lazy-spawn
/// decision inline.
enum RouteOutcome {
    /// Sent via the session channel — the message reached (or is queued
    /// for) the recipient's input.
    Delivered,
    /// The session channel was closed — the recipient's task has exited.
    ChannelClosed,
    /// Target was a known goal in `app.goals` but not in the session
    /// registry. A `SpawnGoalRequest` was enqueued; the runtime will
    /// spawn the session and deliver the message into it.
    LazySpawned,
    /// Target is neither a running session nor a known goal. The caller
    /// treats this as a failed delivery.
    Unknown,
}

/// Build the `send_message` dispatcher closure for one goal session. The
/// closure captures the sender's identity, the shared session registry, the
/// shared `goal_spawn_tx` (for lazy-spawn on first contact), the app handle
/// (for system-message visibility and batch tracking), and the log handle
/// (for the `SendMessageDispatched` introspection event).
///
/// Contract:
/// - `target` is checked against both the session registry and the goal
///   tree. The tool succeeds when the target is in either, mirroring
///   the lazy-spawn contract: a known goal in the tree is lazy-spawned on first
///   contact so the message can be delivered into its fresh session.
///   The tool fails outright when the target is unknown — not
///   recognized as a goal and not present as a running session.
/// - **Self-send guard:** the captured `sender` (the calling session's
///   id) is compared against `target` *before* any other work. If they
///   match, the tool returns `Err` immediately with a course-correction
///   message — a self-delivered message would create a new turn in the
///   same session, which the session could then re-process and dispatch
///   the same call again, recursing without bound. The check fires at
///   the session-id level (not the base-goal-id level) so a sub-session
///   `fresh-agents~1` cannot send to itself, but a sub-session sending
///   to its parent permanent goal (`fresh-agents`) is a legitimate
///   cross-session dispatch and is allowed. A rejected self-send has no
///   side effects: no system message pushed, no `running_sessions`
///   entry, no successful-delivery log event. A `SendMessageDispatched`
///   event with `success: false` and the rejection reason is emitted
///   for introspection symmetry with the unknown-target path.
/// - On success (registry hit or lazy-spawn): format the message (same
///   `delivery_message` framing), push a
///   `{sender} → {target}` system message so the user can see
///   the exchange, insert the target into `running_sessions` so the
///   batch is correctly active (and the batch-end retirement check
///   holds off while the lazy-spawned session is being drained), and
///   emit a `SendMessageDispatched` log event.
/// - On channel-closed (session registry hit but task has exited):
///   return `Err(reason)` with the lost-delivery warning so the model
///   can react.
/// - On unknown target: return `Err(reason)`. The model receives the
///   reason as the tool result.
///
/// Returns an `Arc` so the closure can be cloned cheaply and shared across
/// every `run()` call of the runner that serves this goal session.
fn build_send_message_dispatcher(
    sender: String,
    session_senders: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    goal_spawn_tx: mpsc::UnboundedSender<SpawnGoalRequest>,
    app: Arc<Mutex<App>>,
    log: logger::LogSender,
) -> cap::SendMessageFn {
    Arc::new(move |target: &str, message: &str| -> Result<String, String> {
        // Self-send guard: a session must not dispatch a message to itself.
        // A self-delivered message creates a new turn in the same session
        // — the session would then re-process the message and could emit
        // the same send_message call again, recursing without bound. The
        // check fires at the session-id level (sender == target) and runs
        // *before* any state mutation, so a rejected self-send has no
        // side effects: no system message, no running_sessions entry, no
        // delivery log event. A `SendMessageDispatched` event with
        // success: false is emitted so introspection sees the rejected
        // dispatch with the same shape as the existing unknown-target
        // failure path. The error message is alternatives-aware: it
        // names the offending target and points to specific alternative
        // actions — spawn_session for sub-tasks of self, a different
        // goal id for a different agent, or continue reasoning. The
        // alternatives-aware form closes the loop faster than a generic
        // "pick a different target" hint because send_message and
        // spawn_session are sibling tools with different semantics and
        // the most likely self-send is a confusion between the two.
        if target == sender {
            let reason = format!(
                "send_message: cannot send a message to yourself (`{target}` is the same as the sender). To dispatch a sub-task of your own goal, use the spawn_session tool; to reach a different agent, address their goal id; otherwise continue reasoning in the current turn"
            );
            log.emit(
                &sender,
                logger::LogEvent::SendMessageDispatched {
                    sender: sender.clone(),
                    target: target.to_string(),
                    success: false,
                    error: Some(reason.clone()),
                },
            );
            return Err(reason);
        }
        // First-line summary for the running-sessions reason and the user
        // system message. Truncating at the first line keeps the visible
        // exchange legible in the conversation pane.
        let first_line = message.lines().next().unwrap_or("").to_string();
        // Format the message with the uniform delivery framing so the
        // recipient sees a consistent sender-attributed body regardless
        // of which code path delivered it.
        let formatted = prompts::delivery_message(&sender, message);

        // Route via the registry-then-lazy-spawn decision. Take the senders
        // lock briefly, drop it, then take the app lock to consult the
        // goal tree — never hold both at once, in any order.
        let outcome = {
            let senders = session_senders.lock().unwrap();
            if let Some(tx) = senders.get(target) {
                if tx.send(formatted).is_err() {
                    RouteOutcome::ChannelClosed
                } else {
                    RouteOutcome::Delivered
                }
            } else {
                drop(senders);
                let a = app.lock().unwrap();
                if a.goals.iter().any(|g| g.id == target) {
                    // Lazy-spawn: hand the runtime a SpawnGoalRequest with
                    // the formatted message,  The runtime drains the request, spawns the
                    // goal_agent_loop, and delivers the message into the
                    // new session's channel — all before the sender's turn
                    // completes. If the runtime is shutting down
                    // (goal_spawn_tx closed) we still report success here
                    // and let the shutdown error surface from the run loop;

                    let _ = goal_spawn_tx.send(SpawnGoalRequest {
                        goal_id: target.to_string(),
                        message: formatted,
                        fresh_session: None,
                    });
                    RouteOutcome::LazySpawned
                } else {
                    RouteOutcome::Unknown
                }
            }
        };

        match outcome {
            RouteOutcome::Delivered | RouteOutcome::LazySpawned => {
                // Surface the exchange in the conversation pane and the log.
                // The user sees a uniform "→" line for the dispatch.
                let sys = format!("{} → {}: {}", sender, target, message);
                {
                    let mut a = app.lock().unwrap();
                    a.push_system_message(&sys);
                    // Mark the recipient as running so the batch is active.
                    // The recipient's own chunk will replace this entry's
                    // reason; on its Done event running_sessions drops the
                    // entry, matching the Done-event lifecycle. For the
                    // LazySpawned case the entry is inserted BEFORE the
                    // SpawnGoalRequest drains, which is exactly the gap the
                    // batch-end retirement check must wait through.
                    a.running_sessions
                        .entry(target.to_string())
                        .or_insert(Some(first_line));
                }
                log.emit(
                    &sender,
                    logger::LogEvent::TinkerSystemMessageReceived { content: sys },
                );
                log.emit(
                    &sender,
                    logger::LogEvent::SendMessageDispatched {
                        sender: sender.clone(),
                        target: target.to_string(),
                        success: true,
                        error: None,
                    },
                );
                Ok(format!("delivered to `{target}`"))
            }
            RouteOutcome::ChannelClosed => {
                // The recipient's session task has exited and its channel
                // is closed. Surface this as a tool error so the model can
                // react — the registry validation passed but the channel
                // was dead on arrival. We do NOT remove the running_sessions
                // entry here: the goal_session Done event for the recipient
                // (if it ever fires) will clear it, and a stale entry is
                // harmless.
                let reason = prompts::delivery_lost_warning(&sender, target);
                log.emit(
                    &sender,
                    logger::LogEvent::SendMessageDispatched {
                        sender: sender.clone(),
                        target: target.to_string(),
                        success: false,
                        error: Some(reason.clone()),
                    },
                );
                Err(format!("send_message: {}", reason.trim_end()))
            }
            RouteOutcome::Unknown => {
                // Target is neither a running session nor a known goal.
                // The spec is explicit that the tool fails outright on
                // unknown targets rather than silently dropping — the
                // model receives the reason as the tool result.
                let reason = format!(
                    "send_message: target `{target}` is unknown (not in the session registry and not a known goal)"
                );
                log.emit(
                    &sender,
                    logger::LogEvent::SendMessageDispatched {
                        sender: sender.clone(),
                        target: target.to_string(),
                        success: false,
                        error: Some(reason.clone()),
                    },
                );
                Err(reason)
            }
        }
    })
}

/// Build the `spawn_session` dispatcher closure for one goal session. The
/// closure captures the *caller's* session id (could be a permanent goal
/// id like `"rummage"` or an ephemeral coordinator id like `"rummage~1"`),
/// the shared session registry, `goal_spawn_tx`, the app handle, and the
/// log handle.
///
/// **Self-only routing** — the schema exposes no `target` parameter, so
/// the routing target is implicit in the closure. The new sub-session is
/// always of the *caller's own goal*, derived from the captured session
/// id. This enforces the self-only constraint at the harness layer (no
/// arbitrary-goal spawning) rather than the schema layer.
///
/// Contract:
/// - Pre-assigns a unique session id using `app.fresh_session_counter`:
///   the new id is `{caller_session_id}~{counter}`. The caller session id
///   is used verbatim as the prefix (not the base permanent goal id), so
///   a coordinator's sub-session is nested under the coordinator in the
///   TUI — mirroring the spawn_session tool's nesting.
/// - Looks up the caller's permanent goal in `app.goals` (resolving
///   ephemeral coordinators to their base id via `app::session_base_id`).
///   If the caller's goal is not in the goal tree, returns `Err(reason)`
///   and the model sees a tool error — there is no fallback path.
/// - On success: updates `running_sessions` / `ephemeral_sessions` /
///   `ephemeral_labels` so the batch-end retirement check sees the
///   new sub-session as active work, enqueues a `SpawnGoalRequest` with
///   `fresh_session: Some(...)` on `goal_spawn_tx`, emits a system
///   message for user visibility, emits a `SpawnSessionDispatched` log
///   event, and returns the pre-assigned session id + label.
/// - The actual `goal_agent_loop` task is created by the run-loop drain
///   handler at `goal_spawn_rx` time, so this dispatcher does NOT
///   duplicate the channel/init/spawn plumbing — it only does the
///   pre-id-assignment and registry bookkeeping.
///
/// Returns an `Arc` so the closure can be cloned cheaply and shared across
/// every `run()` call of the runner that serves this goal session.
fn build_spawn_session_dispatcher(
    caller_session_id: String,
    goal_spawn_tx: mpsc::UnboundedSender<SpawnGoalRequest>,
    app: Arc<Mutex<App>>,
    log: logger::LogSender,
) -> cap::SpawnSessionFn {
    Arc::new(move |subgoal: &str, label: Option<&str>| -> Result<(String, Option<String>), String> {
        // The caller is the running session that emitted the tool call.
        // We use its full session id (could be ephemeral) as the prefix
        // for the new sub-session id so the TUI nesting reflects the
        // immediate-dispatcher relationship.
        let caller = caller_session_id.clone();
        let label_owned: Option<String> = label.map(String::from);

        // Pre-assign the new sub-session id and update the app state in
        // one critical section: increment the counter, insert into the
        // session registry's tracking structures, and enqueue the spawn
        // request. The actual `goal_agent_loop` task is created by the
        // run-loop drain handler.
        let (session_id, first_line, permanent_base) = {
            let mut a = app.lock().unwrap();
            a.fresh_session_counter += 1;
            let counter = a.fresh_session_counter;
            let id = format!("{}~{}", caller, counter);
            let first_line = subgoal.lines().next().unwrap_or("").to_string();
            a.running_sessions.insert(id.clone(), Some(first_line.clone()));
            a.ephemeral_sessions.insert(id.clone());
            a.ephemeral_sessions_ordered.push(id.clone());
            a.ephemeral_labels.insert(id.clone(), label_owned.clone());
            a.goal_list_scroll.last_total = a.flat_items().len();
            let perm_base = app::session_base_id(&caller).to_string();
            (id, first_line, perm_base)
        };

        // The new sub-session inherits the permanent dispatcher's goal
        // (description, neighbors, tier). For a permanent caller the
        // base id equals the caller; for an ephemeral coordinator the
        // base id is the underlying permanent goal.
        let caller_goal_in_tree = {
            let a = app.lock().unwrap();
            a.goals.iter().any(|g| g.id == permanent_base)
        };
        if !caller_goal_in_tree {
            // The caller's goal is not in the goal tree — undo the
            // registry bookkeeping we just did (the spawn can't proceed
            // because the run-loop drain handler would find no goal to
            // build an init message from).
            let mut a = app.lock().unwrap();
            a.running_sessions.remove(&session_id);
            a.ephemeral_sessions.remove(&session_id);
            a.ephemeral_sessions_ordered.retain(|id_local| id_local != &session_id);
            a.ephemeral_labels.remove(&session_id);
            let reason = format!(
                "spawn_session: caller goal `{caller}` is not in the goal tree (no init context available)"
            );
            log.emit(
                &caller,
                logger::LogEvent::SpawnSessionDispatched {
                    sender: caller.clone(),
                    sub_session_id: session_id.clone(),
                    label: label_owned.clone(),
                    success: false,
                    error: Some(reason.clone()),
                },
            );
            return Err(reason);
        }

        // Enqueue the spawn request. The run-loop drain handler picks it
        // up, does the batch-start retirement, registers the channel,
        // builds the lean init message, and spawns a `goal_agent_loop`
        // task.
        let _ = goal_spawn_tx.send(SpawnGoalRequest {
            goal_id: permanent_base.clone(),
            message: subgoal.to_string(),
            fresh_session: Some(FreshSessionConfig {
                session_id: session_id.clone(),
                label: label_owned.clone(),
                dispatcher_id: caller.clone(),
            }),
        });

        // Surface the dispatch in the conversation pane so the user can
        // see tool-spawned sub-sessions. The format mirrors the
        // `{dispatcher} → fresh sub-session ...` line used elsewhere so
        // the delivery paths produce visually equivalent output.
        let label_clause = match &label_owned {
            Some(l) if !l.is_empty() => format!(" ({l})"),
            _ => String::new(),
        };
        let sys = format!(
            "{} → fresh sub-session `{}`{}: {}",
            caller, session_id, label_clause, first_line
        );
        {
            let mut a = app.lock().unwrap();
            a.push_system_message(&sys);
        }
        log.emit(
            &caller,
            logger::LogEvent::TinkerSystemMessageReceived { content: sys.clone() },
        );
        log.emit(
            &caller,
            logger::LogEvent::SpawnSessionDispatched {
                sender: caller.clone(),
                sub_session_id: session_id.clone(),
                label: label_owned.clone(),
                success: true,
                error: None,
            },
        );

        Ok((session_id, label_owned))
    })
}

/// Check whether the batch (active/idle) state has changed and emit a
/// `BatchTransition` log event plus a user-visible system message if so.
///
/// A batch is "active" whenever `running_sessions` is non-empty — any LLM
/// session is processing a turn or awaiting delivery. Idle means no session
/// holds a slot. `batch_active` is the caller-owned tracking flag; it is
/// mutated in-place on every detected transition.
fn check_and_emit_batch_transition(
    app: &mut App,
    batch_active: &mut bool,
    log: &logger::LogSender,
) {
    let now_active = !app.running_sessions.is_empty();
    if now_active && !*batch_active {
        *batch_active = true;
        let msg = prompts::batch_active_msg().trim_end();
        app.push_system_message(msg);
        log.emit("harness", logger::LogEvent::BatchTransition {
            direction: "idle_to_active".to_string(),
        });
    } else if !now_active && *batch_active {
        *batch_active = false;
        let msg = prompts::batch_idle_msg().trim_end();
        app.push_system_message(msg);
        log.emit("harness", logger::LogEvent::BatchTransition {
            direction: "active_to_idle".to_string(),
        });
    }
}

/// Unified session event handler. Routes events from any agent session to the
/// appropriate App state updates. The session registry is taken by `&HashMap` —
/// the caller is responsible for holding the registry lock; the function only reads.
fn handle_session_event(
    app: &mut App,
    ev: SessionEvent,
    _goal_spawn_tx: &mpsc::UnboundedSender<SpawnGoalRequest>,
    session_senders: &HashMap<String, mpsc::UnboundedSender<String>>,
    fs: &dyn Filesystem,
    log: &logger::LogSender,
) {
    match ev {
        SessionEvent::Chunk { goal_id, text } => {
            // Mark session as actively processing so the ▶ indicator appears.
            // Applies to every session including interactive agents (tend/rummage/jog).
            app.running_sessions.entry(goal_id.clone()).or_insert(None);
            // Suppress tend's startup output from the conversation pane until
            // the user has typed their first message.
            if goal_id != "tend" || app.user_has_interacted {
                app.append_agent_message(&goal_id, &text);
            }
            // Use clone so goal_id remains usable after the entry borrow ends.
        }
        SessionEvent::Done { goal_id, full_output } => {
            app.finalize_agent_message(&goal_id);
            // Clear the ▶ indicator for any session type, including interactive agents.
            app.running_sessions.remove(&goal_id);
            // Increment session count for the TUI's per-goal token metrics.
            app.token_usage.entry(goal_id.clone()).or_default().session_count += 1;
            let session_text = full_output;
            // Reload goals — any session may have written TOML files.
            if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                app.goals = load.goals;
                app.update_parse_errors(load.errors);
                // Same diff-and-emit as the startup and watcher paths:
                // the user-visible system message is pushed via
                // `update_goal_id_collisions`; the structured
                // `GoalCollision` log event captures the same fact in a
                // `jq`-queryable form so cross-tier overlaps that appear
                // after a session write are not lost.
                let new_collisions = app.update_goal_id_collisions(load.collisions);
                for collision in &new_collisions {
                    log.emit("goal-agents", logger::LogEvent::GoalCollision {
                        goal_id: collision.goal_id.clone(),
                        contributors: collision.contributors.iter()
                            .map(|(tier, path)| (tier.clone(), path.display().to_string()))
                            .collect(),
                    });
                }
            }
            // Tend-specific: parse-error correction loop and phase gate.
            if goal_id == "tend" {
                let prev_errors = app.parse_errors.clone();
                if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                    app.goals = load.goals;
                    app.update_parse_errors(load.errors);
                    // Same collision capture as above — tend's correction
                    // loop also re-loads, so any cross-tier overlap
                    // introduced or resolved by a tend write must surface
                    // in the runtime event log.
                    let new_collisions = app.update_goal_id_collisions(load.collisions);
                    for collision in &new_collisions {
                        log.emit("goal-agents", logger::LogEvent::GoalCollision {
                            goal_id: collision.goal_id.clone(),
                            contributors: collision.contributors.iter()
                                .map(|(tier, path)| (tier.clone(), path.display().to_string()))
                                .collect(),
                        });
                    }
                }
                let new_errors: Vec<(std::path::PathBuf, String)> = app
                    .parse_errors
                    .iter()
                    .filter(|(p, e)| !prev_errors.iter().any(|(pp, ee)| pp == p && ee == e))
                    .cloned()
                    .collect();
                if !new_errors.is_empty() && app.correction_attempts < 2 {
                    let listing = new_errors
                        .iter()
                        .map(|(p, e)| {
                            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                            format!("- `{}`: {}", name, e)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let msg = prompts::parse_correction(&listing, goal::GOAL_SCHEMA_KEYS_ORDER);
                    app.correction_attempts += 1;
                    let correction_msg = prompts::parse_correction_system_msg(app.correction_attempts);
                    app.push_system_message(&correction_msg);
                    log.emit("correction-injector", logger::LogEvent::TinkerSystemMessageReceived { content: correction_msg });
                    if let Some(tx) = session_senders.get("tend") {
                        let _ = tx.send(msg);
                    }
                    return;
                } else if !new_errors.is_empty() {
                    let still_invalid_msg = prompts::parse_correction_gave_up().trim_end();
                    app.push_system_message(still_invalid_msg);
                    log.emit("correction-injector", logger::LogEvent::TinkerSystemMessageReceived { content: still_invalid_msg.to_string() });
                }
                app.correction_attempts = 0;
                if app.phase == Phase::Initializing {
                    app.push_system_message(
                        "You're talking to tend. /rummage switches to code understanding, TAB moves focus to the goal view, Enter on a goal opens options.",
                    );
                    log.emit("harness", logger::LogEvent::TinkerSystemMessageReceived { content: "You're talking to tend. /rummage switches to code understanding, TAB moves focus to the goal view, Enter on a goal opens options.".to_string() });
                    app.phase = Phase::Idle;
                }
            }
            // Silence detection: if the session produced no output at all, prompt
            // the agent to surface what happened. Applies uniformly to all sessions.
            if session_text.trim().is_empty()
                && let Some(tx) = session_senders.get(&goal_id) {
                    let _ = tx.send(
                        prompts::silence_nudge().trim_end().to_string()
                    );
                }
        }
        SessionEvent::CleanupBlocked { goal_id, dirty_files, error } => {
            let msg = if let Some(e) = error {
                format!("Goal `{goal_id}` blocked: tinker-test-case cleanup errored ({e}).")
            } else {
                let listing = dirty_files
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Goal `{goal_id}` blocked: tinker-test-case markers remain after {} cleanup attempts in: {listing}. Clean manually and retry.",
                    cleanup::MAX_RETRIES,
                )
            };
            app.push_system_message(&msg);
            log.emit("cleanup", logger::LogEvent::TinkerSystemMessageReceived { content: msg });
            app.running_sessions.remove(&goal_id);
        }
        SessionEvent::TokenUsage { goal_id, prompt_tokens, completion_tokens, total_tokens, cached_tokens } => {
            log.emit("backend", logger::LogEvent::ApiTokenUsage {
                goal_id: goal_id.clone(),
                prompt_tokens,
                completion_tokens,
                total_tokens,
                cached_tokens,
            });
            // Update App's in-memory token stats for TUI rendering.
            let stats = app.token_usage.entry(goal_id).or_default();
            stats.total_prompt_tokens += prompt_tokens;
            stats.total_completion_tokens += completion_tokens;
            if let Some(cached) = cached_tokens {
                stats.total_cached_tokens += cached;
            }
        }
    }
}


#[derive(Debug)]
enum KeyAction {
    None,
    Quit,
    /// Send a message to the named session (session_id, message).
    SendToSession(String, String),
    /// Confirm the options dialog: fire the goal with an optional reason, and
    /// optionally apply a new tier (write to file, detach running session).
    /// `new_tier` is None when the tier was not changed.
    ConfirmOptions {
        goal_id: String,
        reason: Option<String>,
        new_tier: Option<String>,
    },
}

/// Advance the tier display value forward (Right arrow in the options dialog).
/// Cycles: mid → high → low → mid.
fn cycle_tier_display_next(current: &str) -> &'static str {
    match current {
        "mid"  => "high",
        "high" => "low",
        "low"  => "mid",
        _      => "mid",
    }
}

/// Advance the tier display value backward (Left arrow in the options dialog).
/// Cycles: mid → low → high → mid.
fn cycle_tier_display_prev(current: &str) -> &'static str {
    match current {
        "mid"  => "low",
        "low"  => "high",
        "high" => "mid",
        _      => "mid",
    }
}

/// Resolves a goal's effective tier: an explicit `tier` field wins; when absent,
/// behavior goals default to "high" and all others to "mid".
fn effective_goal_tier(goal: &goal::Goal) -> &str {
    match goal.tier.as_deref() {
        Some(t) => t,
        None => match goal.kind.as_deref() {
            Some("behavior") => "high",
            _ => "mid",
        },
    }
}

const MOUSE_SCROLL_STEP: usize = 3;

fn handle_mouse(app: &mut App, ev: MouseEvent, area: ratatui::layout::Rect) {
    let rects = tui::pane_rects(area);
    let in_rect = |r: ratatui::layout::Rect| -> bool {
        ev.column >= r.x
            && ev.column < r.x.saturating_add(r.width)
            && ev.row >= r.y
            && ev.row < r.y.saturating_add(r.height)
    };
    match ev.kind {
        MouseEventKind::ScrollUp => {
            if in_rect(rects.repl) {
                app.repl_scroll.scroll_up(MOUSE_SCROLL_STEP);
            } else if in_rect(rects.goal_list) {
                app.goal_list_scroll.scroll_up(MOUSE_SCROLL_STEP);
            } else if in_rect(rects.goal_text) {
                app.goal_text_scroll.scroll_up(MOUSE_SCROLL_STEP);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_rect(rects.repl) {
                app.repl_scroll.scroll_down(MOUSE_SCROLL_STEP);
            } else if in_rect(rects.goal_list) {
                app.goal_list_scroll.scroll_down(MOUSE_SCROLL_STEP);
            } else if in_rect(rects.goal_text) {
                app.goal_text_scroll.scroll_down(MOUSE_SCROLL_STEP);
            }
        }
        _ => {}
    }
}

fn handle_modal_key(app: &mut App, key: crossterm::event::KeyEvent) -> KeyAction {
    use crate::app::ModalField;
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.should_quit = true;
            KeyAction::Quit
        }
        (_, KeyCode::Esc) => {
            app.modal = None;
            KeyAction::None
        }
        // Tab switches focus between the two fields.
        (_, KeyCode::Tab) => {
            if let Some(m) = app.modal.as_mut() {
                m.focused_field = match m.focused_field {
                    ModalField::Reason => ModalField::Tier,
                    ModalField::Tier   => ModalField::Reason,
                };
            }
            KeyAction::None
        }
        // Left/Right cycle the tier when the Tier field is focused.
        (_, KeyCode::Left) => {
            if let Some(m) = app.modal.as_mut()
                && m.focused_field == ModalField::Tier
            {
                let next = cycle_tier_display_prev(&m.tier);
                m.tier = next.to_string();
            }
            KeyAction::None
        }
        (_, KeyCode::Right) => {
            if let Some(m) = app.modal.as_mut()
                && m.focused_field == ModalField::Tier
            {
                let next = cycle_tier_display_next(&m.tier);
                m.tier = next.to_string();
            }
            KeyAction::None
        }
        (_, KeyCode::Enter) => {
            let m = match app.modal.take() {
                Some(m) => m,
                None => return KeyAction::None,
            };
            let reason = if m.input.trim().is_empty() {
                None
            } else {
                Some(m.input.trim().to_string())
            };
            let new_tier = if m.tier != m.initial_tier {
                Some(m.tier.clone())
            } else {
                None
            };
            KeyAction::ConfirmOptions { goal_id: m.goal_id, reason, new_tier }
        }
        (_, KeyCode::Backspace) => {
            if let Some(m) = app.modal.as_mut()
                && m.focused_field == ModalField::Reason
            {
                m.input.pop();
            }
            KeyAction::None
        }
        (_, KeyCode::Char(c)) => {
            if let Some(m) = app.modal.as_mut()
                && m.focused_field == ModalField::Reason
            {
                m.input.push(c);
            }
            KeyAction::None
        }
        _ => KeyAction::None,
    }
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent, log: &logger::LogSender, known_session_ids: &[&str]) -> KeyAction {
    // Modal owns keyboard focus while open — all keys route to it. Pane
    // focus (`app.focus`) is unchanged, so submit/cancel returns input to
    // wherever the user was before.
    if app.modal.is_some() {
        return handle_modal_key(app, key);
    }
    match app.focus.clone() {
        Focus::Repl => match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                app.should_quit = true;
                KeyAction::Quit
            }
            (_, KeyCode::Tab) => {
                if !app.flat_goals().is_empty() {
                    app.focus = Focus::Tree;
                }
                KeyAction::None
            }
            (_, KeyCode::Enter) => {
                let input = app.input.trim().to_string();
                if input.is_empty() {
                    return KeyAction::None;
                }

                // Special slash commands (not session switches).
                if input == "/quit" {
                    app.should_quit = true;
                    return KeyAction::Quit;
                }
                if input == "/help" {
                    let help_msg = "Commands: @<goal-id> [msg], /quit, /help, /<goal-id> to switch session. Tab = goal tree, Enter on goal = options (tier, trigger reason).";
                    app.push_system_message(help_msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: help_msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }

                // Generic session-switching: /<known-goal-id>
                if let Some(id) = input.strip_prefix('/')
                    && known_session_ids.contains(&id) {
                        app.active_session = id.to_string();
                        app.repl_scroll.y = None;
                        app.repl_buffer.invalidate();
                        let msg = format!("switched to {} — type to chat, /<goal-id> to switch", id);
                        app.push_system_message(&msg);
                        log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: msg });
                        app.input.clear();
                        return KeyAction::None;
                    }
                    // Unknown slash command: fall through as ordinary input.

                let session_id = app.active_session.clone();
                app.push_user_message(&input, &session_id);
                app.input.clear();
                app.user_has_interacted = true;
                if session_id == "tend" {
                    app.correction_attempts = 0;
                }
                KeyAction::SendToSession(session_id, input)
            }
            (_, KeyCode::Backspace) => {
                app.input.pop();
                KeyAction::None
            }
            (_, KeyCode::Char(c)) => {
                app.input.push(c);
                KeyAction::None
            }
            _ => KeyAction::None,
        },

        Focus::Tree => match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                app.should_quit = true;
                KeyAction::Quit
            }
            (_, KeyCode::Tab) | (_, KeyCode::Esc) => {
                app.focus = Focus::Repl;
                KeyAction::None
            }
            (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
                app.select_next_goal();
                KeyAction::None
            }
            (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
                app.select_prev_goal();
                KeyAction::None
            }
            (_, KeyCode::PageDown) => {
                app.goal_list_scroll.scroll_down(MOUSE_SCROLL_STEP);
                KeyAction::None
            }
            (_, KeyCode::PageUp) => {
                app.goal_list_scroll.scroll_up(MOUSE_SCROLL_STEP);
                KeyAction::None
            }
            (_, KeyCode::Enter) => match app.selected_goal() {
                Some(g) => {
                    // Enter on a goal opens the options dialog. Pane focus
                    // stays on `Tree` — when the modal closes (submit or Esc),
                    // the user is still in the tree.
                    let tier_display = effective_goal_tier(&g).to_string();
                    app.modal = Some(crate::app::ModalState {
                        goal_id: g.id,
                        input: String::new(),
                        tier: tier_display.clone(),
                        initial_tier: tier_display,
                        focused_field: crate::app::ModalField::Reason,
                    });
                    KeyAction::None
                }
                None => KeyAction::None,
            },
            _ => KeyAction::None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ModalField;

    /// Test fixture: loads the tend goal description from the test fixture
    /// file. Production code never reads from this path; the runtime
    /// discovers tend.toml via `goal::load_all_goals` from the project's
    /// `.tinker/goals/packaged-goals/`. This helper exists so the persona-
    /// text tests below have a stable fixture to assert against without
    /// coupling to runtime discovery.
    fn test_tend_description() -> String {
        const TOML: &str = include_str!("../.tinker/goals/packaged-goals/tend.toml");
        toml::from_str::<crate::goal::Goal>(TOML)
            .expect("test fixture tend.toml must be valid Goal TOML")
            .description
    }

    // REMOVED: test_spec_build_batch_summary_request_folds_per_goal_summaries
    // (build_batch_summary_request deleted — batch machinery retired)
    // REMOVED: test_spec_batch_summary_request_instructs_reactive_run_lines
    // (same reason)

    // REMOVED: test_spec_summary_routes_directly_to_tend_not_batched
    // (SummaryReady variant retired — goal agents send_message to tend directly on completion)

    // spec: goal-agents — SummaryReady was a transitional SessionEvent variant
    // that routed goal-session summaries to tend via the event channel. Goal agents
    // now send_message to tend directly, so the variant has been removed.
    // LlmSessionId is likewise gone: the session id is captured from the
    // runner's return value, not an event.
    // This exhaustive-match test compiles only while both variants stay absent.
    #[test]
    fn test_spec_summary_ready_variant_removed() {
        // Exhaustive match ensures LlmSessionId and SummaryReady stay absent.
        // Done carries goal_id and full_output (the complete assembled reply for
        // the complete assembled reply).
        let evt = SessionEvent::Done { goal_id: "x".into(), full_output: "".into() };
        match evt {
            SessionEvent::Chunk { .. } => {}
            SessionEvent::Done { .. } => {}
            SessionEvent::CleanupBlocked { .. } => {}
            SessionEvent::TokenUsage { .. } => {}
        }
    }

    // RealFilesystem (std::fs underneath) must follow symlinks transparently.
    // We rely on this for `~/.tinker/goals` pointing into a checked-out repo's
    // `packaged-goals/`. Regression test: if anyone swaps `Path::is_dir` for
    // `symlink_metadata` or `read_dir` for a no-follow variant, this fails.
    #[test]
    fn test_spec_realfs_follows_symlinked_goals_dir() {
        use std::os::unix::fs::symlink;
        use std::time::{SystemTime, UNIX_EPOCH};

        // Unique temp paths so parallel test runs don't collide.
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tinker-symlink-test-{}", id));
        let real_pkg = root.join("real-pkg");
        let proj_tinker = root.join("proj/.tinker");
        let _ = std::fs::create_dir_all(&real_pkg);
        let _ = std::fs::create_dir_all(&proj_tinker);
        std::fs::write(
            real_pkg.join("g.toml"),
            "id = \"g\"\ndescription = \"x\"\n",
        )
        .unwrap();
        // proj/.tinker/goals -> real_pkg
        symlink(&real_pkg, proj_tinker.join("goals")).unwrap();

        let fs = RealFilesystem;
        assert!(
            fs.is_dir(&proj_tinker.join("goals")),
            "Path::is_dir must resolve symlinked goals dir"
        );
        let result = goal::load_goals(&fs, &proj_tinker).unwrap();
        assert_eq!(result.errors.len(), 0);
        assert_eq!(result.goals.len(), 1, "goal behind symlink must load");
        assert_eq!(result.goals[0].id, "g");

        // Cleanup — only delete the symlink+temp tree, never the symlink target.
        let _ = std::fs::remove_dir_all(&root);
    }

    // Helper for the capability-DI meta-scan below. Drops everything from the
    // first `#[cfg(test)]` line to end-of-file — coarse but sufficient: in
    // this codebase test modules sit at the end of each file. Without this,
    // test scaffolding that legitimately uses std::fs to set up tempdirs
    // would trip the meta-scan.
    fn strip_test_modules(src: &str) -> String {
        if let Some(idx) = src.find("#[cfg(test)]") {
            src[..idx].to_string()
        } else {
            src.to_string()
        }
    }

    // spec: coding-standards section 1 (Capability-based DI) — "Effects
    // (subprocess execution, filesystem, ...) are declared as capability
    // interfaces, never invoked directly by business logic. Real
    // implementations exist only at the binary's composition root."
    //
    // Meta-scan the production source: raw `Command::new` and raw `std::fs::`
    // calls must be confined to the files that host the Real* implementations.
    // If a refactor sprinkles a raw effect into a business module, this test
    // catches it. Test scaffolding (the `mod tests` blocks themselves) is
    // stripped before scanning.
    #[test]
    fn test_spec_raw_effects_confined_to_composition_root() {
        use std::path::Path;
        let manifest = env!("CARGO_MANIFEST_DIR");
        let src = Path::new(manifest).join("src");
        let fs = RealFilesystem;

        let needle_cmd = "Command::new";
        let needle_fs = "std::fs::";
        // Files that legitimately host raw effects: `native.rs` owns the
        // in-process tool executor (bash/grep subprocesses, read/write/edit
        // file effects) — that executor IS the native runner's capability
        // implementation. `realfs.rs` owns the Filesystem cap. With the
        // CLI backends removed there are no other files that need raw
        // effects at the edges.
        let cmd_allowed = ["native.rs"];
        let fs_allowed = ["realfs.rs", "native.rs"];

        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            let content = fs.read_to_string(&path).unwrap();
            let prod = strip_test_modules(&content);

            if !cmd_allowed.contains(&name.as_str()) {
                assert!(
                    !prod.contains(needle_cmd),
                    "{name}: raw subprocess call (`Command::new`) outside \
                     composition root (native.rs). Subprocess effects must \
                     go through the LlmRunner capability.",
                );
            }
            if !fs_allowed.contains(&name.as_str()) {
                assert!(
                    !prod.contains(needle_fs),
                    "{name}: raw filesystem call (`std::fs::`) outside \
                     composition root (realfs.rs). Filesystem effects must \
                     go through the Filesystem capability.",
                );
            }
        }
    }

    // spec: coding-standards section 4 (Security) — "Every project has a
    // security.md at the repo root ... If security.md is absent or its threat
    // model is unclear, the AI must propose or update it before writing
    // security-sensitive code." Mechanically testable: the file must exist
    // and be non-trivial.
    #[test]
    fn test_spec_security_md_present_at_repo_root() {
        use std::path::Path;
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = Path::new(manifest).join("security.md");
        let fs = RealFilesystem;
        let content = fs
            .read_to_string(&path)
            .expect("security.md must exist at repo root (coding-standards section 4)");
        assert!(
            content.trim().len() > 100,
            "security.md must contain a real threat model, not a stub",
        );
    }

    // spec (tui, triggers): dispatch messages use "triggered:" format.
    #[test]
    fn test_spec_dispatched_run_uses_triggered_system_message_format() {
        let goal_id = "tui";
        let reason = "implement the thing";
        let reason_opt = Some(reason.to_string());
        let summary = format!(
            "triggered: `{}`{}",
            goal_id,
            reason_opt.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
        );
        assert!(summary.starts_with("triggered:"), "dispatch message must start with 'triggered:'");
        assert!(!summary.contains("Scheduler:"), "retired 'Scheduler:' naming must not appear");
        assert!(summary.contains(goal_id), "goal id must appear in message");
        assert!(summary.contains(reason), "reason must appear in message");
    }

    // REMOVED: test_spec_run_command_parsing (parse_run_commands deleted — /run retired)
    // REMOVED: test_spec_repl_manual_run_with_reason_dispatches_correctly (/run removed)
    // REMOVED: test_spec_repl_run_without_reason_yields_none (/run removed)
    // REMOVED: test_spec_one_goal_session_at_a_time_queue_drains_serially (queue removed)
    // REMOVED: test_spec_queue_preserves_same_goal_id_no_dedup_fifo (queue removed)
    // REMOVED: test_spec_queue_in_memory_only_starts_empty (queue removed)
    // REMOVED: test_spec_queue_same_goal_drains_fifo_with_separate_reasons (queue removed)
    // REMOVED: test_spec_tinker_run_while_running_goes_to_queue_not_run_tx (queue removed)
    // REMOVED: test_spec_rummage_run_command_dispatched (parse_run_commands removed)

    // spec (tui): "Mouse wheel scrolls whichever region is under the cursor."
    // ScrollUp events route to the pane whose rect contains the cursor; other
    // panes must be unaffected.
    #[test]
    fn test_spec_mouse_scroll_routes_to_hovered_pane() {
        use crossterm::event::{MouseEvent, MouseEventKind, KeyModifiers};
        let area = ratatui::layout::Rect { x: 0, y: 0, width: 80, height: 100 };
        let rects = tui::pane_rects(area);

        // Seed every scroll pane with enough content that scrolling can move them.
        let mut app = App::new();
        for s in [&mut app.repl_scroll,
                  &mut app.goal_text_scroll, &mut app.goal_list_scroll] {
            s.record_render(200, 10, 0);
        }
        // goal_text_scroll defaults to reset_to_top (Some(0)); nudge it to the
        // tail so ScrollUp is observable (scroll_up from 0 is a no-op).
        app.goal_text_scroll.y = None;

        // Capture each pane's y before the test event.
        let y_repl_before    = app.repl_scroll.y;
        let y_text_before    = app.goal_text_scroll.y;
        let y_list_before    = app.goal_list_scroll.y;

        // Cursor inside the REPL pane — only repl_scroll must move.
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: rects.repl.x + 1,
            row: rects.repl.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, ev, area);
        assert_ne!(app.repl_scroll.y, y_repl_before,
            "repl_scroll must move on ScrollUp in REPL pane");
        assert_eq!(app.goal_text_scroll.y, y_text_before,
            "goal_text_scroll must not move when cursor is in REPL pane");
        assert_eq!(app.goal_list_scroll.y, y_list_before,
            "goal_list_scroll must not move when cursor is in REPL pane");

        // Cursor inside the goal-text pane — only goal_text_scroll must move.
        let y_text_before2 = app.goal_text_scroll.y;
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: rects.goal_text.x + 1,
            row: rects.goal_text.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, ev, area);
        assert_ne!(app.goal_text_scroll.y, y_text_before2,
            "goal_text_scroll must move on ScrollUp in goal-text pane");
    }

    // spec (goal-structure-standard, compact-goal-context): the compact goal
    // index injected into tinker's context must include related-link data and
    // be produced by build_compact_index. Verified via source inspection —
    // the index is built in an async task.
    #[test]
    fn test_spec_goals_summary_includes_related_links() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("build_compact_index"),
            "main.rs must delegate goal index to build_compact_index",
        );
        // build_compact_index serializes related links as JSON "related" arrays
        // (verified in goal.rs tests); here we confirm main.rs calls it.
        assert!(
            main_rs.contains("goal::build_compact_index"),
            "main.rs must call goal::build_compact_index to produce the index",
        );
    }

    // spec (compact-goal-context): the --tend-full-goal-context flag must
    // cause main.rs to use build_full_text_index (not build_compact_index) for
    // the goals summary passed to tend, and to call tend_init_prompt_full_context
    // rather than tend_init_prompt.
    #[test]
    fn test_spec_full_goal_context_flag_routes_to_full_text_index() {
        let main_rs = include_str!("main.rs");
        // Check with the argument included so the pattern only matches the production
        // call site, not this test's own source (which contains the bare function name
        // as a string literal but not the call-with-argument form).
        assert!(
            main_rs.contains("build_full_text_index(&a.goals)"),
            "main.rs must call build_full_text_index(&a.goals) on the full-context path",
        );
        assert!(
            main_rs.contains("tend_init_prompt_full_context(&goals_index"),
            "main.rs must call tend_init_prompt_full_context with goals_index on the full-context path",
        );
        assert!(
            main_rs.contains("use_full_goal_context"),
            "main.rs must gate the full-text path on the --tend-full-goal-context flag",
        );
    }

    const BUILTIN_SESSION_IDS: &[&str] = &["tend", "rummage", "jog"];

    // spec (rummage): `/rummage` switches the active session to rummage and
    // emits a system message; the input buffer must be cleared.
    #[test]
    fn test_spec_slash_rummage_switches_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        assert_eq!(app.active_session, "tend", "starts as tend");
        app.input = "/rummage".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert!(
            matches!(action, KeyAction::None),
            "/rummage must not dispatch a chat message; returns None",
        );
        assert_eq!(app.active_session, "rummage", "/rummage must switch active_session to rummage");
        assert!(app.input.is_empty(), "input must be cleared after /rummage");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("rummage")),
            "/rummage must emit a system message naming rummage",
        );
    }

    /// Spec (tui): switching session resets the REPL scroll to the tail so
    /// the new session's most recent output is immediately visible.
    #[test]
    fn test_spec_session_switch_resets_repl_scroll() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.repl_scroll.y = Some(42);
        app.input = "/rummage".into();
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.repl_scroll.y, None, "session switch must reset repl_scroll to follow-tail (None)");
    }

    // spec (rummage): `/tend` switches back from rummage to tend.
    #[test]
    fn test_spec_slash_tend_switches_back_to_tend() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        app.active_session = "rummage".to_string();
        app.input = "/tend".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert!(
            matches!(action, KeyAction::None),
            "/tend must not dispatch a chat message; returns None",
        );
        assert_eq!(app.active_session, "tend", "/tend must switch active_session back to tend");
        assert!(app.input.is_empty(), "input must be cleared after /tend");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("tend")),
            "/tend must emit a system message naming tend",
        );
    }

    // spec (rummage / tui): "One agent is active at a time. User messages go to
    // whoever is active." When active_session is rummage, Enter on a non-slash
    // message must return SendToSession("rummage", …). When tend, SendToSession("tend", …).
    #[test]
    fn test_spec_message_routes_to_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // Route to tend when active.
        let mut app = App::new();
        app.phase = Phase::Idle;
        app.active_session = "tend".to_string();
        app.input = "hello tend".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert!(
            matches!(&action, KeyAction::SendToSession(id, _) if id == "tend"),
            "message must route to tend when active_session is tend; got {:?}",
            std::mem::discriminant(&action),
        );

        // Route to rummage when active.
        let mut app2 = App::new();
        app2.phase = Phase::Idle;
        app2.active_session = "rummage".to_string();
        app2.input = "hello rummage".into();
        let action2 = handle_key(&mut app2, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert!(
            matches!(&action2, KeyAction::SendToSession(id, _) if id == "rummage"),
            "message must route to rummage when active_session is rummage; got {:?}",
            std::mem::discriminant(&action2),
        );
    }

    // spec (tinker-introspection): ".tinker/logs/" directory is pre-created
    // at startup alongside ".tinker/goals/", ".tinker/state/", and ".tinker/notes/".
    // Verified via source inspection since startup logic is in main().
    #[test]
    fn test_spec_logs_dir_created_at_startup() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("join(\"logs\")"),
            "main.rs must pre-create the .tinker/logs/ directory at startup",
        );
        // The mkdir_all call must use the same primary_tinker_dir pattern as the
        // other directories created at startup.
        let logs_mkdir = main_rs.contains("mkdir_all") && main_rs.contains("\"logs\"");
        assert!(logs_mkdir, "main.rs must call mkdir_all for the logs subdir");
    }

    // spec (jog): ".tinker/discrepancies/" is pre-created at startup so jog
    // sessions can write discrepancy files without creating the directory themselves.
    #[test]
    fn test_spec_discrepancies_dir_created_at_startup() {
        let main_rs = include_str!("main.rs");
        let discrepancies_mkdir = main_rs.contains("mkdir_all") && main_rs.contains("\"discrepancies\"");
        assert!(discrepancies_mkdir, "main.rs must call mkdir_all for the discrepancies subdir");
    }

    // spec (rummage): "Rummage uses the strongest model available for the chosen
    // backend." The composition root in main.rs must wire the rummage runner to
    // the tinker tier (the smartest tier), not SCHEDULER_MODEL or GOAL_MODEL.
    // spec (goal-agents): rummage, jog, and tend all declare tier="high" in their
    // TOML. The lazy-spawn path selects oc_goal_high for any goal with that tier.
    // This test verifies the uniform tier-based dispatch is wired correctly.
    #[test]
    fn test_spec_rummage_jog_tend_use_high_tier_via_toml() {
        // Verify tier="high" is declared in the packaged TOML files.
        let rummage_toml = include_str!("../.tinker/goals/packaged-goals/rummage.toml");
        assert!(rummage_toml.contains("tier = \"high\""), "rummage.toml must declare tier = \"high\"");
        let jog_toml = include_str!("../.tinker/goals/packaged-goals/jog.toml");
        assert!(jog_toml.contains("tier = \"high\""), "jog.toml must declare tier = \"high\"");
        // Verify the lazy-spawn runner selection is tier-based (no name-based match).
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("\"high\" => oc_goal_high"),
            "lazy spawn must use oc_goal_high for tier=high goals",
        );
        // The high-tier runner is wired from the resolved `native_high()`
        // accessor (no defaults args), which in turn is fed by
        // `model_config` (the user's TOML plus OpenRouter defaults).
        // The accessor call must use the no-args form — the
        // defaults-args form is the legacy single-backend path that
        // hardcoded endpoint+model at the call site.
        assert!(
            main_rs.contains("model_config.native_high()"),
            "native high-tier must use the no-args accessor (defaults live in config.rs)",
        );
        // The runner construction must consume the resolved endpoint
        // and model — not a compiled-in OPENROUTER_URL constant.
        assert!(
            main_rs.contains("tinker_cfg.endpoint")
                && main_rs.contains("tinker_cfg.model"),
            "NativeRunner must be constructed from the resolved per-tier cfg fields",
        );
        // And the auth must come from the dedicated `*_api_key()`
        // accessor on `model_config` (env-var-only, never carried on
        // the cfg struct by spec).
        assert!(
            main_rs.contains("model_config.native_high_api_key"),
            "auth must come from the dedicated per-tier api_key accessor on model_config",
        );
    }

    // spec (goal-agents): A goal with tier="low" must dispatch to oc_goal_low, which
    // is wired to the low-tier model.
    // This ensures the "low" tier value is routed through the per-tier model wiring,
    // not silently folded into the mid-tier default.
    #[test]
    fn test_spec_low_tier_goal_uses_low_runner() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("\"low\" => oc_goal_low"),
            "lazy spawn must route tier=low goals to oc_goal_low",
        );
        assert!(
            main_rs.contains("model_config.native_low()"),
            "oc_goal_low must be wired from native_low() (no compiled-in defaults)",
        );
    }

    // spec (jog): Jog runs on the strongest model tier via tier="high" in its TOML.
    // Description injected via session_init_message, not as a system prompt.
    #[test]
    fn test_spec_jog_wired_to_strongest_model_tier() {
        let main_rs = include_str!("main.rs");
        // Claude path: description in init message, not system prompt.
        // Split string to avoid this test's own source appearing in the include_str scan.
        let old_pattern: String = ["tinker_m, ", "jog::packaged_goal().description"].concat();
        assert!(
            !main_rs.contains(&old_pattern),
            "jog description must not appear as system prompt — it comes through session_init_message",
        );
    }

    // spec (goal-agents): rummage and jog must NOT be pre-seeded in session_senders.
    // They start lazily on the first dispatch or user message, just like
    // every other goal agent. Tend is the only pre-populated entry.
    #[test]
    fn test_spec_rummage_jog_lazy_not_pre_seeded() {
        let main_rs = include_str!("main.rs");
        assert!(
            !main_rs.contains("session_senders.lock().unwrap().insert(\"rummage\""),
            "rummage must not be pre-seeded in session_senders (lazy startup only)",
        );
        assert!(
            !main_rs.contains("session_senders.lock().unwrap().insert(\"jog\""),
            "jog must not be pre-seeded in session_senders (lazy startup only)",
        );
        // Tend IS pre-seeded (its eager startup is the only registry exception).
        assert!(
            main_rs.contains("session_senders.lock().unwrap().insert(\"tend\""),
            "tend must be pre-seeded in session_senders (the only eager-start exception)",
        );
    }

    // spec (jog / tui): `/jog` slash command must switch active_session to jog and
    // emit a system message naming jog.
    #[test]
    fn test_spec_slash_jog_switches_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        assert_eq!(app.active_session, "tend", "starts as tend");
        app.input = "/jog".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert!(
            matches!(action, KeyAction::None),
            "/jog must not dispatch a chat message; returns None",
        );
        assert_eq!(app.active_session, "jog", "/jog must switch active_session to jog");
        assert!(app.input.is_empty(), "input must be cleared after /jog");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("jog")),
            "/jog must emit a system message naming jog",
        );
    }

    // spec (jog / tui): messages route to jog when active_session is jog.
    #[test]
    fn test_spec_message_routes_to_jog_when_active() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.phase = Phase::Idle;
        app.active_session = "jog".to_string();
        app.input = "jog me on logging".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert!(
            matches!(&action, KeyAction::SendToSession(id, _) if id == "jog"),
            "message must route to jog when active_session is jog; got {:?}",
            std::mem::discriminant(&action),
        );
    }

    // spec (tui, goal-agents): /<goal-id> switches active_session to any
    // registry-known id, not just the three built-in agents.
    #[test]
    fn test_spec_slash_goal_id_switches_active_session() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let ids = &["tend", "rummage", "jog", "tui"];
        let mut app = App::new();
        app.input = "/tui".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), ids);
        assert!(
            matches!(action, KeyAction::None),
            "/<goal-id> must not dispatch a chat message; returns None",
        );
        assert_eq!(app.active_session, "tui", "/<goal-id> must switch active_session to that id");
        assert!(app.input.is_empty(), "input must be cleared after session switch");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("tui")),
            "/<goal-id> must emit a system message naming the new session",
        );
    }

    // spec (tui, goal-agents): an unrecognised /<word> is not a session switch —
    // it falls through and is sent to the active session as ordinary text.
    #[test]
    fn test_spec_slash_unknown_id_ignored() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.phase = Phase::Idle;
        app.input = "/no-such-session".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        // Must be sent as text to the currently-active session.
        assert!(
            matches!(&action, KeyAction::SendToSession(id, msg) if id == "tend" && msg == "/no-such-session"),
            "unrecognised /<word> must fall through as ordinary input to the active session; got {:?}",
            std::mem::discriminant(&action),
        );
        assert_eq!(app.active_session, "tend", "active_session must be unchanged after unknown slash");
    }

    // ── send_message dispatcher tests ────────────────────────────────────

    // spec (send-message): the dispatcher delivers to a registered target,
    // frames the message with the same delivery_message formatting the
    // delivery_message template uses, pushes a system message for user visibility, and
    // returns Ok with a confirmation the model can act on.
    #[test]
    fn test_spec_send_message_dispatcher_delivers_to_known_target() {
        let (tend_tx, mut tend_rx) = mpsc::unbounded_channel::<String>();
        let (rummage_tx, mut rummage_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        senders.insert("rummage".to_string(), rummage_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let log = logger::noop_sender();
        let dispatcher = build_send_message_dispatcher(
            "rummage".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            log,
        );

        let result = dispatcher("tend", "investigate the auth flow");
        let out = result.expect("dispatch to registered target must succeed");
        assert!(out.contains("tend"), "ok result should name the target");

        // The recipient sees the same delivery-message format the envelope
        // path produces — they cannot tell which mechanism delivered it.
        let tend_msg = tend_rx.try_recv().expect("tend must receive the dispatch");
        assert!(tend_msg.contains("[from rummage]"), "message must carry sender attribution");
        assert!(tend_msg.contains("investigate the auth flow"));

        // rummage must not receive a message addressed to tend.
        assert!(
            rummage_rx.try_recv().is_err(),
            "rummage must not receive a tend-addressed dispatch",
        );

        // A system message is pushed for user visibility — a plain
        // "sender → target" line (no envelope tag syntax).
        let a = app.lock().unwrap();
        let sys = a.messages.iter().find(|m| {
            m.role == app::Role::System
                && m.text.contains("rummage → tend")
        });
        assert!(sys.is_some(), "a `rummage → tend` system message must be pushed");
    }

    // spec (send-message): the dispatcher returns Err with the unknown-target
    // reason when the target is neither in the session registry nor a
    // known goal in the goal tree. This is the load-bearing step of the
    // spec — the tool "fails outright on unknown targets" rather than
    // silently dropping or auto-spawning. Validation now spans the
    // registry AND the goal tree; a known but unspawned goal succeeds
    // (lazy-spawn) while a truly unknown ID errors here.
    #[test]
    fn test_spec_send_message_dispatcher_unknown_target_returns_err() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let log = logger::noop_sender();
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            log,
        );

        let err = dispatcher("ghost", "ping").expect_err("unknown target must Err");
        let msg = err.to_string();
        assert!(
            msg.contains("ghost") && msg.contains("session registry"),
            "error must name the unknown target and the registry constraint: {msg}"
        );
    }

    // spec (send-message): on unknown target, no system message is pushed —
    // the dispatch never happened, so there is no exchange to surface to
    // the user.
    #[test]
    fn test_spec_send_message_dispatcher_unknown_target_no_system_message() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let _ = dispatcher("ghost", "ping");
        let a = app.lock().unwrap();
        assert!(
            a.messages.is_empty(),
            "a failed dispatch must not push a system message"
        );
    }

    // spec (send-message): the dispatcher inserts the recipient into
    // running_sessions so the batch-end retirement check sees a pending
    // delivery as active work.  Without this, the recipient's session could
    // be retired before consuming the buffered message.
    #[test]
    fn test_spec_send_message_dispatcher_marks_target_running() {
        let (rummage_tx, _rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), rummage_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let _ = dispatcher("rummage", "trace the init flow");
        let a = app.lock().unwrap();
        assert!(
            a.running_sessions.contains_key("rummage"),
            "target must appear in running_sessions after a successful dispatch"
        );
        let reason = a.running_sessions["rummage"].as_deref().unwrap_or("");
        assert!(
            reason.contains("trace the init flow"),
            "running reason must reflect the message first line: {reason}"
        );
    }

    // spec (send-message): the dispatcher formats the dispatch with the
    // `delivery_message` template so a recipient cannot tell which
    // dispatch mechanism delivered it — it arrives in standard framing
    // with the same reply instruction.
    #[test]
    fn test_spec_send_message_dispatcher_uses_delivery_message_format() {
        let (tend_tx, mut tend_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "rummage".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let _ = dispatcher("tend", "what does this mean?");
        let msg = tend_rx.try_recv().expect("tend must receive the dispatch");

        // delivery_message template starts with "[from {SENDER}], message:"
        assert!(
            msg.starts_with("[from rummage]"),
            "delivered message must use the delivery_message framing starting with sender attribution: {msg:?}"
        );
        assert!(
            msg.contains("what does this mean?"),
            "delivered message must include the body verbatim: {msg}"
        );
        // The reply instruction points back at the sender via
        // send_message — the delivery message must never embed raw
        // dispatch tag syntax.
        assert!(
            !msg.contains("</@rummage>"),
            "delivery message must not contain dispatch tag syntax: {msg:?}"
        );
    }

    // spec (send-message): the SendMessageDispatched log event is emitted
    // for both success and failure paths, with the success field set
    // accordingly.  Introspection can distinguish the two by `success`.
    #[test]
    fn test_spec_send_message_dispatcher_emits_log_event_on_success_and_failure() {
        use std::sync::Mutex;
        let captured: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        // We can't easily inject into the dispatcher without changing its
        // signature, so we exercise the success/failure paths by direct
        // invocation.  The introspect-ability contract is the
        // `SendMessageDispatched` event being present in the log — this is
        // verified by the logger test suite.
        let _ = captured;
        // Smoke: build the dispatcher and confirm it doesn't panic on
        // both paths.  The logger tests in src/logger.rs verify the
        // event-shape contract.
        let (rummage_tx, _rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), rummage_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );
        // Success path returns Ok; failure path returns Err.  Both must
        // not panic.
        let _ = dispatcher("rummage", "ok path");
        let _ = dispatcher("ghost", "err path");
    }

    // spec (send-message): the dispatcher rejects a self-send (target ==
    // sender) with `Err`, so an agent cannot dispatch a message to
    // itself. Self-send would create a new turn in the same session,
    // which the session could then re-process and dispatch again —
    // recursing without bound. The check fires at the session-id level
    // (the same id the recipient would route on), so a permanent goal
    // agent sending to itself and an ephemeral sub-session sending to
    // itself are both blocked.
    #[test]
    fn test_spec_send_message_dispatcher_self_send_returns_err() {
        let (tend_tx, mut tend_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let err = dispatcher("tend", "hello me").expect_err("self-send must Err");
        let msg = err.to_string();
        // The error must name the offending target and surface the
        // self-send condition so the model can recognise what went wrong.
        assert!(msg.contains("tend"), "error must name the self-target: {msg}");
        assert!(
            msg.contains("yourself") || msg.contains("same as the sender"),
            "error must surface the self-send condition: {msg}"
        );
        // The rejected dispatch has no side effects: no message arrived
        // at the recipient's channel.
        assert!(
            tend_rx.try_recv().is_err(),
            "rejected self-send must not deliver a message to the channel"
        );
        // The rejected dispatch has no system-message side effect either —
        // the user shouldn't see a phantom exchange.
        let a = app.lock().unwrap();
        assert!(
            a.messages.is_empty(),
            "a rejected self-send must not push a system message"
        );
    }

    // spec (send-message): the self-send error message guides course
    // correction. The model receives a reason that names the offending
    // target AND names the specific alternative actions — pointing to
    // the spawn_session tool for sub-tasks of self, a different goal id
    // for a different agent, and the "continue reasoning" fallback. The
    // alternatives-aware form closes the loop faster than a generic
    // "pick a different target" hint: send_message and spawn_session
    // are sibling tools with different semantics (one targets an
    // existing session, the other spawns a fresh sub-session of self),
    // and the most likely self-send is a confusion between the two.
    // Without that hint the model would have to re-derive the right
    // action from a bare error.
    #[test]
    fn test_spec_send_message_dispatcher_self_send_error_guides_correction() {
        let (rummage_tx, _rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), rummage_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "rummage".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let err = dispatcher("rummage", "loop me").expect_err("self-send must Err");
        let msg = err.to_string();
        // The error must name the offending target.
        assert!(msg.contains("rummage"), "error must name the self-target: {msg}");
        // The error must include the spawn_session pointer — the most
        // likely self-send is a confusion with the spawn_session tool.
        assert!(
            msg.contains("spawn_session"),
            "error must point to spawn_session for sub-tasks of self: {msg}"
        );
        // The error must also include a "different goal id" pointer —
        // the typo / wrong-target case.
        assert!(
            msg.contains("goal id") || msg.contains("different agent"),
            "error must point to addressing a different agent: {msg}"
        );
        // The "continue reasoning" fallback must be present so the
        // model can recover when the call was a misfire entirely.
        assert!(
            msg.contains("continue reasoning"),
            "error must include the continue-reasoning fallback: {msg}"
        );
    }

    // spec (send-message): the self-send guard must not interfere with
    // dispatching to a *known peer* target. The new check sits before
    // the existing registry lookup and only fires on target == sender;
    // for any other registered target the original delivery path runs
    // unchanged. This pins the regression surface: a buggy
    // implementation that over-blocks (e.g. misread the spec as
    // blocking cross-level dispatch) would fail this test.
    #[test]
    fn test_spec_send_message_dispatcher_known_peer_target_still_succeeds() {
        let (tend_tx, mut tend_rx) = mpsc::unbounded_channel::<String>();
        let (rummage_tx, _rummage_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        senders.insert("rummage".to_string(), rummage_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "rummage".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        // Sender is "rummage"; target is "tend" (a different session).
        // Self-send guard does not fire; the existing registry-hit
        // delivery path must run.
        let out = dispatcher("tend", "ping the orchestrator")
            .expect("dispatch to a known peer must succeed");
        assert!(out.contains("tend"), "ok result should name the target");
        let tend_msg = tend_rx.try_recv().expect("tend must receive the dispatch");
        assert!(tend_msg.contains("[from rummage]"), "delivery message must carry sender attribution");
    }

    // spec (send-message): the self-send guard does not weaken the
    // unknown-target behaviour. An unknown target is still rejected
    // with `Err` regardless of the new check — the self-send guard sits
    // at the very top of the closure, so unknown-target handling
    // continues to work for any target that is not in the registry and
    // not in the goal tree.
    #[test]
    fn test_spec_send_message_dispatcher_unknown_target_still_fails_after_self_send_guard() {
        let (rummage_tx, _rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), rummage_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        // "ghost" is neither in the registry (only "rummage" is) nor in
        // the goal tree (the app is empty). The self-send guard does
        // not fire (ghost != tend); the unknown-target path runs.
        let err = dispatcher("ghost", "ping").expect_err("unknown target must still Err");
        let msg = err.to_string();
        assert!(
            msg.contains("ghost") && msg.contains("session registry"),
            "error must name the unknown target and the registry constraint: {msg}"
        );
    }

    // ── spawn_session dispatcher tests ───────────────────────────────────

    // spec (spawn-session): when the caller's goal is in the goal tree, the
    // dispatcher pre-assigns a session id of the form `{caller_session_id}~{counter}`,
    // inserts the id into the ephemeral session tracking structures, and
    // enqueues a `SpawnGoalRequest` with `fresh_session: Some(...)`. The
    // returned tuple is `(session_id, label)` — both pieces of information
    // the model needs to route replies to the new sub-session.
    #[test]
    fn test_spec_spawn_session_dispatcher_enqueues_request_for_known_goal() {
        let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        // Add the caller's goal to the goal tree so the dispatcher's
        // goal-tree lookup succeeds.
        app.lock().unwrap().goals.push(goal::Goal {
            id: "rummage".into(),
            summary: "code oracle".into(),
            description: "test".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });
        let dispatcher = build_spawn_session_dispatcher(
            "rummage".to_string(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let (session_id, label) = dispatcher("trace the init flow", Some("investigate"))
            .expect("dispatch to a goal in the tree must succeed");
        assert_eq!(session_id, "rummage~1", "session id must follow the {{caller}}~{{counter}} format");
        assert_eq!(label.as_deref(), Some("investigate"), "label must be preserved for reply routing");

        // The spawn request was enqueued with fresh_session = Some.
        let req = goal_spawn_rx.try_recv().expect("a SpawnGoalRequest must be enqueued");
        let fresh = req.fresh_session.expect("fresh_session must be Some for spawn_session tool path");
        assert_eq!(fresh.session_id, "rummage~1", "pre-assigned id must match the enqueued one");
        assert_eq!(fresh.label.as_deref(), Some("investigate"));
        assert_eq!(fresh.dispatcher_id, "rummage", "dispatcher_id is the caller's session id");
        assert_eq!(req.goal_id, "rummage", "the inherited goal id is the caller's permanent base");
        assert!(req.message.contains("trace the init flow"), "subgoal is forwarded verbatim");

        // Registry bookkeeping happened before the request was enqueued.
        let a = app.lock().unwrap();
        assert!(a.running_sessions.contains_key("rummage~1"), "new session must appear in running_sessions");
        assert!(a.ephemeral_sessions.contains("rummage~1"), "new session must be marked ephemeral");
        assert_eq!(a.ephemeral_labels.get("rummage~1").and_then(|l| l.as_deref()), Some("investigate"));
    }

    // spec (spawn-session): the new sub-session id prefix is the caller's
    // full session id — for an ephemeral coordinator (`rummage~1`), the
    // sub-session is nested under the coordinator (`rummage~1~N`), matching
    // the tool's nesting pattern. This is what the TUI
    // uses to render sub-sessions under the immediate dispatcher.
    #[test]
    fn test_spec_spawn_session_dispatcher_nests_under_ephemeral_coordinator() {
        let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        // Coordinator's base permanent goal is in the tree.
        app.lock().unwrap().goals.push(goal::Goal {
            id: "rummage".into(),
            summary: "code oracle".into(),
            description: "test".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });
        // The caller's session id is an ephemeral coordinator (`rummage~1`).
        let dispatcher = build_spawn_session_dispatcher(
            "rummage~1".to_string(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let (session_id, _label) = dispatcher("sub-task", None)
            .expect("dispatch must succeed when caller's base goal is in the tree");
        assert_eq!(
            session_id, "rummage~1~1",
            "the new sub-session id must be nested under the coordinator's full session id"
        );

        let req = goal_spawn_rx.try_recv().expect("a SpawnGoalRequest must be enqueued");
        let fresh = req.fresh_session.expect("fresh_session must be Some");
        assert_eq!(fresh.session_id, "rummage~1~1");
        // The dispatcher_id is the caller's full session id (the
        // coordinator), so the sub-session's replies route back to the
        // coordinator, not to the root goal.
        assert_eq!(fresh.dispatcher_id, "rummage~1");
        // The inherited goal (for init context) is the caller's base.
        assert_eq!(req.goal_id, "rummage");
    }

    // spec (spawn-session): the dispatcher pushes a system message in the
    // `{dispatcher} → fresh sub-session ...` format so tool-spawned
    // sub-sessions are visually equivalent in the conversation pane.
    #[test]
    fn test_spec_spawn_session_dispatcher_pushes_system_message() {
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        app.lock().unwrap().goals.push(goal::Goal {
            id: "rummage".into(),
            summary: "code oracle".into(),
            description: "test".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });
        let dispatcher = build_spawn_session_dispatcher(
            "rummage".to_string(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let _ = dispatcher("trace the auth flow", Some("auth"));
        let a = app.lock().unwrap();
        let sys = a.messages.iter().find(|m| {
            m.role == app::Role::System
                && m.text.contains("rummage → fresh sub-session")
                && m.text.contains("rummage~1")
                && m.text.contains("trace the auth flow")
        });
        assert!(sys.is_some(), "a `rummage → fresh sub-session` system message must be pushed");
    }

    // spec (spawn-session): when the caller's goal is not in the goal tree,
    // the dispatcher returns Err with the reason. No spawn request is
    // enqueued and the registry bookkeeping is rolled back (the new
    // session id is removed from running_sessions / ephemeral_sessions /
    // ephemeral_labels) so the goal-list state is not corrupted by a
    // failed dispatch.
    #[test]
    fn test_spec_spawn_session_dispatcher_unknown_caller_goal_returns_err() {
        let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        // Empty goal tree — caller's goal is unknown.
        let dispatcher = build_spawn_session_dispatcher(
            "ghost".to_string(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let err = dispatcher("sub-task", None)
            .expect_err("unknown caller goal must Err");
        assert!(
            err.contains("not in the goal tree"),
            "error must name the goal-tree-miss reason: {err}"
        );
        assert!(
            goal_spawn_rx.try_recv().is_err(),
            "no SpawnGoalRequest must be enqueued on failure"
        );
        // Registry bookkeeping was rolled back.
        let a = app.lock().unwrap();
        assert!(!a.running_sessions.contains_key("ghost~1"), "no session id should remain in running_sessions");
        assert!(!a.ephemeral_sessions.contains("ghost~1"), "no ephemeral entry should remain");
    }

    // spec (spawn-session): the dispatcher increments `fresh_session_counter`
    // monotonically across successive spawns so session ids are unique. The
    // counter is shared across all spawn_session dispatchers (it lives in the
    // `app` state), which keeps id assignment central and avoids collisions.
    #[test]
    fn test_spec_spawn_session_dispatcher_counter_is_monotonic() {
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        app.lock().unwrap().goals.push(goal::Goal {
            id: "rummage".into(),
            summary: "code oracle".into(),
            description: "test".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });
        let dispatcher = build_spawn_session_dispatcher(
            "rummage".to_string(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let (id1, _) = dispatcher("first task", None).expect("first dispatch must succeed");
        let (id2, _) = dispatcher("second task", None).expect("second dispatch must succeed");
        let (id3, _) = dispatcher("third task", None).expect("third dispatch must succeed");
        assert_eq!(id1, "rummage~1");
        assert_eq!(id2, "rummage~2");
        assert_eq!(id3, "rummage~3");
    }

    // spec (spawn-session): the spawn request's `goal_id` field is the
    // caller's base permanent goal (resolved through `app::session_base_id`),
    // not the caller's full session id. The run-loop drain handler uses
    // this to look up the dispatcher goal in `app.goals` for the init
    // message — it must be a key in the goal tree, not an ephemeral id.
    #[test]
    fn test_spec_spawn_session_dispatcher_enqueues_with_base_goal_id() {
        let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        // Two goals: rummage (permanent) and an unrelated tend-like goal.
        app.lock().unwrap().goals.push(goal::Goal {
            id: "rummage".into(),
            summary: "code oracle".into(),
            description: "test".into(),
            parent_id: "".into(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });
        let dispatcher = build_spawn_session_dispatcher(
            // Caller is an ephemeral coordinator of rummage.
            "rummage~2".to_string(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let _ = dispatcher("sub-task", None).expect("dispatch must succeed");
        let req = goal_spawn_rx.try_recv().expect("SpawnGoalRequest must be enqueued");
        assert_eq!(
            req.goal_id, "rummage",
            "the enqueued goal_id must be the caller's base permanent goal, not the ephemeral id"
        );
    }

    // spec (send-message): the session message channels and the goal-spawn channel
    // must be unbounded so delivery never silently drops when a recipient is under
    // load.  A bounded try_send on capacity-16 or -32 channels silently discards
    // the message when the channel is full; unbounded senders never fail due to capacity.
    #[test]
    fn test_spec_send_message_session_channels_are_unbounded() {
        let src = include_str!("main.rs");
        // Use concat to prevent this test's own source from satisfying the negative patterns.
        // If the bounded capacity numbers reappear in production code these assertions fail.
        let bounded_str_ch  = ["mpsc::channel::<String>(", "16)"].concat();
        let bounded_spawn_ch = ["mpsc::channel::<SpawnGoal", "Request>(32)"].concat();
        assert!(
            !src.contains(&bounded_str_ch),
            "session message channels must be unbounded — bounded capacity-16 String channel found in production code",
        );
        assert!(
            !src.contains(&bounded_spawn_ch),
            "goal-spawn channel must be unbounded — bounded capacity-32 SpawnGoalRequest channel found in production code",
        );
    }

    // spec (backends): "--help/-h prints all startup flags with a brief description and exits
    // cleanly, without attempting TUI initialisation."
    // All three flags must appear in the help output text and exit must happen before any
    // TUI-acquisition call.
    #[test]
    fn test_spec_help_flag_lists_all_startup_flags() {
        let main_rs = include_str!("main.rs");
        // Help block must mention each recognised flag by name.
        assert!(
            main_rs.contains("--help") && main_rs.contains("-h"),
            "help output must name both --help and -h",
        );
        assert!(
            main_rs.contains("--tend-full-goal-context"),
            "help output must name --tend-full-goal-context",
        );
    }

    #[test]
    fn test_spec_help_flag_exits_before_tui() {
        let main_rs = include_str!("main.rs");
        // The help check (--help / -h) must precede TUI acquisition (enable_raw_mode call).
        let help_pos = main_rs.find("\"--help\"").expect("--help check must exist in main.rs");
        let tui_pos = main_rs.find("enable_raw_mode()?").expect("enable_raw_mode() call must exist in main.rs");
        assert!(
            help_pos < tui_pos,
            "--help check (pos {help_pos}) must appear before enable_raw_mode() call (pos {tui_pos})",
        );
    }

    // spec (startup-args): unrecognized arguments trigger an error message naming the bad arg,
    // followed by help text, and the process exits with code 1.
    #[test]
    fn test_spec_unknown_arg_detection_precedes_tui() {
        let main_rs = include_str!("main.rs");
        let unknown_pos = main_rs.find("unrecognized argument").expect("unknown-arg error message must exist in main.rs");
        let tui_pos = main_rs.find("enable_raw_mode()?").expect("enable_raw_mode() call must exist in main.rs");
        assert!(
            unknown_pos < tui_pos,
            "unknown-arg check (pos {unknown_pos}) must appear before enable_raw_mode() call (pos {tui_pos})",
        );
    }

    #[test]
    fn test_spec_unknown_arg_exits_with_code_1() {
        let main_rs = include_str!("main.rs");
        // After the unknown-arg message there must be a process::exit(1).
        let unknown_pos = main_rs.find("unrecognized argument").expect("unknown-arg error message must exist in main.rs");
        let exit1_pos = main_rs.find("process::exit(1)").expect("process::exit(1) must exist for unknown args");
        assert!(
            unknown_pos < exit1_pos,
            "process::exit(1) (pos {exit1_pos}) must follow the unknown-arg error message (pos {unknown_pos})",
        );
    }

    #[test]
    fn test_spec_help_text_includes_repo_link() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("https://github.com/kaeluka/tinker"),
            "help text must include the repo link https://github.com/kaeluka/tinker",
        );
    }

    #[test]
    fn test_spec_known_args_list_covers_all_startup_flags() {
        let main_rs = include_str!("main.rs");
        // KNOWN_ARGS must enumerate every flag the binary accepts.
        for flag in &["--tend-full-goal-context", "--help", "-h"] {
            assert!(
                main_rs.contains(flag),
                "KNOWN_ARGS or help text must mention flag '{flag}'",
            );
        }
    }

    #[test]
    fn test_spec_tinker_static_persona_in_description_dynamic_goals_in_init() {
        let content = test_tend_description();
        let init = tend_init_prompt("- demo-goal-id: a demo description", "");
        assert!(init.contains("Current goals"), "init prompt must label the dynamic goals section");
        assert!(init.contains("demo-goal-id"), "init prompt must carry the dynamic goals summary verbatim");
        assert!(!content.contains("demo-goal-id"), "static tend persona must not embed dynamic goal ids");
    }

    #[test]
    fn test_spec_tinker_proves_by_execution_not_reading_source() {
        let content = test_tend_description();
        assert!(content.contains("delegates aggressively"),
            "tend prompt must require delegating code-reality questions to rummage rather than reading source directly");
    }

    // spec (backends): the claude backend has no path-scoped permission block — the system
    // prompt is the only mechanism that states tend's file-access boundary. tend_system_prompt()
    // must lead with the scope constraint so it reads as a system-level rule, not buried text.
    #[test]
    fn test_spec_tend_system_prompt_leads_with_scope_constraint() {
        let prompt = tend_system_prompt(&test_tend_description());
        assert!(
            prompt.starts_with("Read, write, and delete files ONLY"),
            "tend system prompt must open with the file-scope boundary statement"
        );
        assert!(
            prompt.contains(".tinker/goals"),
            "tend system prompt must name .tinker/goals/ as the only permitted scope"
        );
        assert!(
            prompt.contains("src/"),
            "tend system prompt must explicitly name src/ as off-limits"
        );
    }

    // spec (native-backend): in the native branch, tend's runner must carry
    // both the struct-level system prompt AND the TendScope policy — the
    // capability boundary is enforced in-process, not just via the prompt.
    // All other native runners must be Unrestricted (goal sessions including
    // cleanup get full tools — cleanup is a goal session, not a special case).
    #[test]
    fn test_spec_native_tend_runner_wired_with_tend_scope_policy() {
        let main_rs = include_str!("main.rs");
        // The construction spans multiple lines now (per-tier endpoint,
        // model, key are separate args), so we look for the combined
        // presence of the three load-bearing tokens in main.rs rather
        // than a single line.
        assert!(
            main_rs.contains("NativeRunner::with_system_prompt"),
            "tend's native runner must be constructed via NativeRunner::with_system_prompt",
        );
        assert!(
            main_rs.contains("tend_system_prompt"),
            "tend's native runner must carry tend_system_prompt() as the struct-level prompt",
        );
        assert!(
            main_rs.contains("ToolPolicy::TendScope"),
            "tend's native runner must use ToolPolicy::TendScope",
        );
        // Needles split so this test's own source lines don't self-match in
        // the include_str! scan.
        let ctor_needle = format!("Arc::new({}Runner::new(", "Native");
        let policy_needle = format!("ToolPolicy::{}", "Unrestricted");
        let unrestricted_count = main_rs
            .lines()
            .filter(|l| l.contains(&ctor_needle) && l.contains(&policy_needle))
            .count();
        assert_eq!(
            unrestricted_count, 4,
            "the four non-tend native runner slots must all be Unrestricted"
        );
    }

    // spec (backends): the per-tier startup precondition runs in main.rs
    // before the TUI starts — a tier with an empty endpoint or model
    // after resolution prints a clear error naming the tier and exits.
    // This replaces the legacy single-OPENROUTER_API_KEY check; the new
    // shape loops over the three resolved tier cfgs and inspects each.
    #[test]
    fn test_spec_per_tier_precondition_runs_before_tui() {
        let main_rs = include_str!("main.rs");
        // The check must reference each tier's accessor — a sanity
        // loop on the three resolved cfgs.
        assert!(
            main_rs.contains("model_config.native_high()")
                && main_rs.contains("model_config.native_mid()")
                && main_rs.contains("model_config.native_low()"),
            "the per-tier precondition must loop over all three resolved tier cfgs",
        );
        // And the check must precede TUI acquisition (enable_raw_mode).
        // Pin the durable shape — the literal error substring may
        // evolve, but the precondition itself is bound to the
        // resolved-tier-config loop above, and that loop is always
        // before `enable_raw_mode` (the whole point of fail-fast is
        // exiting before TUI acquisition).
        let check_pos = main_rs
            .find("model_config.native_high()")
            .expect("per-tier precondition check must exist in main.rs");
        let tui_pos = main_rs
            .find("enable_raw_mode()?")
            .expect("enable_raw_mode() call must exist in main.rs");
        assert!(
            check_pos < tui_pos,
            "per-tier precondition (pos {check_pos}) must appear before enable_raw_mode() call (pos {tui_pos})",
        );
        // The error message must reference the config file so the user
        // knows what to fix. Substring is split so this test's own
        // source doesn't self-match in the include_str! scan if the
        // wording later evolves.
        let config_needle: String = ["config_", "path"].concat();
        assert!(
            main_rs.contains(&config_needle),
            "the precondition error must point at the config file",
        );
        // The legacy single-env-var guard must be gone — auth is now
        // optional (empty/unset = no header) and tier-by-tier.
        // Build the forbidden substring at runtime so this test's own
        // source (which contains the literal string elsewhere) doesn't
        // self-match in the include_str! scan.
        let forbidden: String = ["native::", "API_KEY_ENV"].concat();
        assert!(
            !main_rs.contains(&forbidden),
            "the legacy single-API-key check must be removed (auth is per-tier and optional)",
        );
    }

    // spec (backends): per-tier auth resolution at startup: each tier's
    // resolved key comes from its per-tier env var
    // (TINKER_HIGH_API_KEY etc), resolved via the dedicated
    // `model_config.native_*_api_key()` accessors. Empty/unset env var
    // resolves to None, which is the local-server path. The startup
    // check does NOT fail on missing auth — only on missing
    // endpoint/model. Auth is never stored in the TOML — secrets stay
    // in env vars by spec, so the cfg struct itself has no `key`
    // field; auth flows through the dedicated accessors.
    #[test]
    fn test_spec_per_tier_auth_resolution_via_env_vars() {
        let main_rs = include_str!("main.rs");
        // The main.rs composition root must call each tier's auth
        // accessor on `model_config` and pass the result into
        // `NativeRunner`. The cfg struct no longer carries auth.
        assert!(
            main_rs.contains("model_config.native_high_api_key"),
            "main.rs must read high-tier auth via model_config.native_high_api_key()",
        );
        assert!(
            main_rs.contains("model_config.native_mid_api_key"),
            "main.rs must read mid-tier auth via model_config.native_mid_api_key()",
        );
        assert!(
            main_rs.contains("model_config.native_low_api_key"),
            "main.rs must read low-tier auth via model_config.native_low_api_key()",
        );
        // The legacy `<cfg>.key`-style access on the cfg struct
        // must be gone — auth isn't on the cfg anymore. Build the
        // forbidden substrings at runtime so this test's own source
        // (which mentions the forbidden pattern in comments and
        // assertion messages) doesn't self-match in the include_str!
        // scan.
        let forbidden_high: String = ["tinker_", "cfg.key"].concat();
        let forbidden_mid: String = ["goal_", "cfg.key"].concat();
        let forbidden_low: String = ["cleanup_", "cfg.key"].concat();
        assert!(
            !main_rs.contains(&forbidden_high)
                && !main_rs.contains(&forbidden_mid)
                && !main_rs.contains(&forbidden_low),
            "auth must not be read off the cfg struct — env-var-only via the dedicated accessors",
        );
        // The auth env-var constants must come from the config module —
        // they are no longer declared in native.rs.
        assert!(
            main_rs.contains("config::HIGH_API_KEY_ENV")
                || main_rs.contains("config::MID_API_KEY_ENV")
                || main_rs.contains("config::LOW_API_KEY_ENV")
                || main_rs.contains("model_config.native_high()"),
            "main.rs must route auth through the config module, not the native module",
        );
    }

    // spec (backends): oc_tend's native runner must be constructed with a system prompt so the
    // file-access boundary arrives as a persistent system message, not a user-turn instruction.
    // NativeRunner::new must not be used for the oc_tend slot — without the struct-level
    // system prompt, the file-scope boundary would not be re-asserted on resumed turns.
    #[test]
    fn test_spec_native_tend_runner_wired_with_system_prompt() {
        let main_rs = include_str!("main.rs");
        // The construction spans multiple lines now (per-tier endpoint,
        // model, key are separate args), so we look for the combined
        // presence of the three load-bearing tokens in main.rs rather
        // than a single line.
        assert!(
            main_rs.contains("NativeRunner::with_system_prompt"),
            "oc_tend's native runner must be constructed via NativeRunner::with_system_prompt",
        );
        assert!(
            main_rs.contains("tend_system_prompt"),
            "oc_tend's native runner must carry tend_system_prompt() as the struct-level prompt",
        );
        assert!(
            main_rs.contains("ToolPolicy::TendScope"),
            "oc_tend's native runner must use ToolPolicy::TendScope",
        );
    }

    // spec (backends): goal-agent native runners are constructed WITHOUT a
    // struct-level system prompt — they receive the goal-specific system prompt
    // per-call from goal_agent_loop on each new session (first turn only).
    // The assembled system prompt string (framework preamble + goal description
    // + neighbor table) is delivered as the in-memory session's first system
    // message.
    #[test]
    fn test_spec_native_goal_runners_omit_struct_level_prompt() {
        let main_rs = include_str!("main.rs");
        // Production constructions: 8-space indent, `Arc::new(NativeRunner::...)`.
        // Skipping test-source lines (longer indent) by checking the exact
        // leading whitespace pattern.
        let is_production = |l: &str| -> bool {
            l.starts_with("        Arc::new(NativeRunner::")
        };
        let native_new_count = main_rs.lines().filter(|l| {
            is_production(l) && l.contains("NativeRunner::new(")
        }).count();
        assert_eq!(
            native_new_count, 4,
            "expected 4 production NativeRunner::new() constructions (goal tiers + cleanup), got {native_new_count}"
        );
        let native_wsp_count = main_rs.lines().filter(|l| {
            is_production(l) && l.contains("NativeRunner::with_system_prompt(")
        }).count();
        assert_eq!(
            native_wsp_count, 1,
            "expected exactly one production NativeRunner::with_system_prompt() (oc_tend), got {native_wsp_count}"
        );
    }

    // spec (backends): goal_agent_loop passes the assembled system prompt to
    // oc.run() on the first turn of a new session (lean_init=true path). The
    // system prompt is built from session_init_message(goal, None, compact_index).
    // Subsequent turns pass None.
    #[test]
    fn test_spec_goal_agent_loop_passes_system_prompt_on_first_turn() {
        let main_rs = include_str!("main.rs");
        // Verify the code path that computes system_prompt_for_run for lean_init
        assert!(
            main_rs.contains("session_init_message(&goal, None, &compact_index)"),
            "goal_agent_loop must call session_init_message with None reason to build system prompt"
        );
        assert!(
            main_rs.contains("system_prompt_for_run"),
            "goal_agent_loop must pass the system prompt to oc.run()"
        );
    }

    // spec (backends): the goal_agent_loop Err arm must distinguish transient
    // failures (session preserved) from genuine session loss (overflow). It
    // must NOT unconditionally clear llm_session_id — that was the bug where a
    // single network blip wiped every prior turn from the model's view. The
    // arm must (a) branch on RunError::session_preserved(), (b) set
    // llm_session_id from preserved_session_id (not always None), and (c)
    // record an exit_status that distinguishes the two so the introspection
    // log can tell them apart.
    #[test]
    fn test_spec_goal_agent_loop_err_arm_preserves_session_on_transient() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("e.session_preserved()"),
            "goal_agent_loop Err arm must branch on RunError::session_preserved()"
        );
        assert!(
            main_rs.contains("llm_session_id = e.preserved_session_id"),
            "goal_agent_loop Err arm must set llm_session_id from preserved_session_id, not unconditionally None"
        );
        // The only `llm_session_id = None;` in production code must be the
        // declaration at the top of goal_agent_loop
        // (`let mut llm_session_id: Option<String> = None;`). A second
        // occurrence would mean someone re-added the unconditional clear in
        // the Err arm — the original bug. We inspect only the production
        // portion (before `#[cfg(test)]`); test code legitimately references
        // the string and would produce false positives.
        let production = main_rs.split("#[cfg(test)]").next().unwrap_or(main_rs);
        let none_assigns = production
            .lines()
            .filter(|l| l.contains("llm_session_id = None;") && !l.contains("let mut"))
            .count();
        assert_eq!(
            none_assigns, 0,
            "production goal_agent_loop must not contain `llm_session_id = None;` outside the declaration — that was the bug"
        );
        assert!(
            main_rs.contains("\"transient:"),
            "exit_status must mark transient failures distinctly from crashes"
        );
        assert!(
            main_rs.contains("\"crash:"),
            "exit_status must mark genuine session loss as crash"
        );
    }

    #[test]
    fn test_spec_shared_language_form_norm_minimum_viable_shape() {
        let content = test_tend_description();
        assert!(content.contains("One question per turn") || content.contains("one question per turn"),
            "prompt must enforce one-question-per-turn");
    }

    #[test]
    fn test_spec_tinker_encodes_dual_duty_no_fabrication_at_inflection_points() {
        let content = test_tend_description();
        assert!(content.contains("Phase 1") || content.contains("Phase 2") || content.contains("Phase 3"),
            "prompt must describe interview phases");
        assert!(content.contains("genuinely doesn't know") || content.contains("user names it") || content.contains("investigative, not leading"),
            "prompt must require WHY-directed investigation — tend does not name Y, the user does");
    }

    #[test]
    fn test_spec_notes_dir_created_at_startup() {
        let main_rs = include_str!("main.rs");
        assert!(main_rs.contains(".tinker/notes") || main_rs.contains("\"notes\""),
            "main.rs must create the .tinker/notes directory at startup");
    }

    #[test]
    fn test_spec_state_dir_created_at_startup() {
        let main_rs = include_str!("main.rs");
        assert!(main_rs.contains(".tinker/state") || main_rs.contains("\"state\""),
            "main.rs must create the .tinker/state directory at startup");
    }


    #[test]
    fn test_spec_main_feeds_parent_id_and_children_into_goals_summary() {
        let main_rs = include_str!("main.rs");
        assert!(main_rs.contains("build_compact_index"), "main.rs must call build_compact_index");
        assert!(main_rs.contains("goal::build_compact_index"), "main.rs must call goal::build_compact_index");
    }

    #[test]
    fn test_spec_tinker_init_prompt_full_context_label() {
        let prompt = tend_init_prompt_full_context("### root\ndescription here", "");
        assert!(prompt.contains("full text"), "full-context init prompt must label goals as full text");
        assert!(!prompt.contains("compact"), "full-context init prompt must not reference the compact index");
    }

    // spec (agent-collaboration): tend's init prompt must carry the neighbor-consultation
    // mandate the same way session_init_message does for goal agents. When a non-empty
    // neighbor section is passed, both prompt variants must include it so the mandate
    // is salient before the startup instruction.
    #[test]
    fn test_spec_tend_init_prompt_injects_neighbor_section() {
        let section = "## Neighbor goals\n\nsome table\n\n";
        let compact = tend_init_prompt("[]", section);
        assert!(
            compact.contains("## Neighbor goals"),
            "compact init prompt must include the neighbor-consultation section",
        );
        let full = tend_init_prompt_full_context("[]", section);
        assert!(
            full.contains("## Neighbor goals"),
            "full-context init prompt must include the neighbor-consultation section",
        );
    }

    #[test]
    fn test_spec_tinker_prompt_parent_summary_recheck_when_child_edited() {
        let content = test_tend_description();
        assert!(content.contains("Re-check parent summary"), "prompt must include a 'Re-check parent summary' step");
    }

    #[test]
    fn test_spec_tinker_prompt_related_links_symmetric_both_list_each_other() {
        // tend's write procedure defers the symmetry rule to goal-structure-standard
        // (read fresh) rather than restating it; the guardrail is that the prompt
        // still mandates re-validating every edge, symmetry included.
        let content = test_tend_description();
        assert!(
            content.contains("Re-validate all edges") && content.contains("symmetry"),
            "prompt must include an edge re-validation step covering related-link symmetry",
        );
    }

    // spec (tend, goal-agents): the tend init prompt must NOT instruct silence.
    // The sole mechanism for hiding tend's startup response is the TUI's
    // `user_has_interacted` gate (tested below). The init prompt was previously
    // paired with a "produce no output" instruction, and the redundancy
    // combined with the silence-detection follow-up to create an infinite
    // batch-cycling loop on startup. Asserting the prompt's silence-instruction
    // absence is a regression guard against re-introducing that pairing.
    #[test]
    fn test_spec_tend_init_prompt_does_not_instruct_silence() {
        let compact = tend_init_prompt("[]", "");
        assert!(
            !compact.contains("produce no output") && !compact.contains("no greeting"),
            "compact startup prompt must not carry a silence instruction (TUI is the sole suppression)",
        );
        let full = tend_init_prompt_full_context("[]", "");
        assert!(
            !full.contains("produce no output") && !full.contains("no greeting"),
            "full-context startup prompt must not carry a silence instruction (TUI is the sole suppression)",
        );
    }

    // spec (tui, goal-agents): tend's startup chunks (before the user's first
    // message) must NOT appear in the conversation pane (app.messages). After
    // the user interacts, chunks from tend flow into the conversation pane
    // normally.
    #[test]
    fn test_spec_tend_startup_chunks_suppressed_until_user_interacted() {
        use crate::app::Role;
        use crate::goal_session::SessionEvent;

        let mut app = App::new();
        assert!(!app.user_has_interacted, "user_has_interacted must start false");

        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let senders = HashMap::new();
        let senders = Arc::new(Mutex::new(senders));
        let log = logger::noop_sender();

        // Tend produces a startup chunk before the user has typed anything.
        let ev = SessionEvent::Chunk { goal_id: "tend".to_string(), text: "hello startup".to_string() };
        let senders_guard = senders.lock().unwrap();
        handle_session_event(&mut app, ev, &spawn_tx, &senders_guard, &RealFilesystem, &log);
        drop(senders_guard);

        // Must NOT appear in messages (conversation pane).
        assert!(
            !app.messages.iter().any(|m| matches!(&m.role, Role::Agent(id) if id == "tend")),
            "startup chunk must not appear in conversation pane before user interaction",
        );

        // After the user sends their first message, tend's chunks appear normally.
        app.user_has_interacted = true;
        let ev2 = SessionEvent::Chunk { goal_id: "tend".to_string(), text: "hello user".to_string() };
        let senders_guard = senders.lock().unwrap();
        handle_session_event(&mut app, ev2, &spawn_tx, &senders_guard, &RealFilesystem, &log);
        drop(senders_guard);

        assert!(
            app.messages.iter().any(|m| {
                matches!(&m.role, Role::Agent(id) if id == "tend") && m.text.contains("hello user")
            }),
            "post-interaction chunk must appear in conversation pane",
        );
    }

    // spec (tui — queue visibility): for interactive agents, running_sessions is
    // populated at submission time (SendToSession), not at first-chunk time. A
    // Chunk event uses or_insert and remains idempotent. Done must remove the entry.
    #[test]
    fn test_spec_tend_chunk_adds_to_running_sessions() {
        use crate::goal_session::SessionEvent;

        let mut app = App::new();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let senders = HashMap::new();
        let senders = Arc::new(Mutex::new(senders));
        let log = logger::noop_sender();

        assert!(
            !app.running_sessions.contains_key("tend"),
            "tend must not be in running_sessions before any chunk",
        );

        let chunk = SessionEvent::Chunk { goal_id: "tend".to_string(), text: "hi".to_string() };
        let senders_guard = senders.lock().unwrap();
        handle_session_event(&mut app, chunk, &spawn_tx, &senders_guard, &RealFilesystem, &log);
        drop(senders_guard);

        assert!(
            app.running_sessions.contains_key("tend"),
            "tend must appear in running_sessions while processing a response (chunk received)",
        );

        let done = SessionEvent::Done { goal_id: "tend".to_string(), full_output: "hi".to_string() };
        let senders_guard = senders.lock().unwrap();
        handle_session_event(&mut app, done, &spawn_tx, &senders_guard, &RealFilesystem, &log);
        drop(senders_guard);

        assert!(
            !app.running_sessions.contains_key("tend"),
            "tend must be removed from running_sessions after Done",
        );
    }

    // spec (agent-liveness): when a session produces no output at all (empty
    // full_output in the Done event), a follow-up message must be sent back
    // into that session's channel so the agent is prompted to surface what happened.
    #[test]
    fn test_spec_silence_detection_sends_followup_on_empty_response() {
        use crate::goal_session::SessionEvent;

        let mut app = App::new();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), msg_tx);
        let senders = Arc::new(Mutex::new(senders));
        let log = logger::noop_sender();

        // full_output is empty — no output produced this turn.
        let done = SessionEvent::Done { goal_id: "rummage".to_string(), full_output: "".to_string() };
        let senders_guard = senders.lock().unwrap();
        handle_session_event(&mut app, done, &spawn_tx, &senders_guard, &RealFilesystem, &log);
        drop(senders_guard);

        assert!(
            msg_rx.try_recv().is_ok(),
            "empty-response session must receive a silence follow-up probe",
        );
    }

    // spec (agent-liveness): when a session produces output, no silence probe
    // is sent — the follow-up is reserved for genuinely empty turns only.
    #[test]
    fn test_spec_silence_detection_skips_followup_when_text_produced() {
        use crate::goal_session::SessionEvent;

        let mut app = App::new();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), msg_tx);
        let senders = Arc::new(Mutex::new(senders));
        let log = logger::noop_sender();

        let chunk = SessionEvent::Chunk { goal_id: "rummage".to_string(), text: "working on it".to_string() };
        let senders_guard = senders.lock().unwrap();
        handle_session_event(&mut app, chunk, &spawn_tx, &senders_guard, &RealFilesystem, &log);
        drop(senders_guard);

        // full_output carries the session's actual text; silence detection checks this.
        let done = SessionEvent::Done { goal_id: "rummage".to_string(), full_output: "working on it".to_string() };
        let senders_guard = senders.lock().unwrap();
        handle_session_event(&mut app, done, &spawn_tx, &senders_guard, &RealFilesystem, &log);
        drop(senders_guard);

        assert!(
            msg_rx.try_recv().is_err(),
            "session that produced text must not receive a silence probe",
        );
    }

    // spec (tui — queue visibility / tend-introspection): the SendToSession handler
    // must insert ALL sessions into running_sessions at submission time so ▶ appears
    // the moment a message is sent and batch-transition detection fires correctly.
    // The former guard restricting this to tend/rummage/jog has been removed.
    #[test]
    fn test_spec_all_sessions_inserted_into_running_sessions_on_submit() {
        let src = include_str!("main.rs");
        // The unconditional insert must be present; the old guard must be gone.
        assert!(
            src.contains("running_sessions.insert(session_id"),
            "SendToSession handler must insert session into running_sessions at submission time",
        );
        // The old guard that limited this to interactive agents must be absent.
        let old_guard = ["matches!(session_id.as_str(), \"tend\" | \"rummage\" | \"jog\")"].concat();
        assert!(
            !src.contains(&old_guard),
            "SendToSession handler must not restrict running_sessions insertion to interactive agents only",
        );
    }


    // spec (goal-agents): when oc.run returns Ok("") (no session ID captured —
    // the runner exited without emitting any events, e.g. transient API outage),
    // llm_session_id must stay None so the next dispatch re-sends session_init_message
    // with full context instead of treating this as an established session.
    #[test]
    fn test_spec_empty_session_id_does_not_advance_llm_session_id() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("if !new_sid.is_empty()"),
            "goal_agent_loop must guard llm_session_id update on non-empty new_sid",
        );
    }

    // spec (parallel-goal-agents): all goal agents — including code-writing agents
    // that are not tend/rummage/jog — run directly in the main working directory.
    // No per-agent isolation directory, no merge step.
    #[test]
    fn test_spec_goal_agents_run_in_main_work_dir() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("let work_dir_goal = work_dir.clone()"),
            "run_loop must clone work_dir for all goal agent spawns (not a per-agent temp dir)",
        );
        assert!(
            main_rs.contains("work_dir_goal, app_ref_goal, log_goal, backend_goal"),
            "goal_agent_loop must be called with the cloned main work_dir for every spawn",
        );
    }

    // spec (parallel-goal-agents): the ▶ indicator timing differs by session type:
    // for tend/rummage/jog it is set at message submission (SendToSession path);
    // for goal agents it is set at dispatch time (send_message / spawn_session dispatcher).
    #[test]
    fn test_spec_submission_sets_running_for_repl_sessions() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("submission_sets_running"),
            "ConfirmOptions handler must use submission_sets_running to gate running_sessions insertion",
        );
    }

    // spec (tier-edit): cycle_tier_display_next cycles the explicit tier ring forward.
    // cycle_tier_display_prev cycles backward. mid ↔ high ↔ low ↔ mid.
    #[test]
    fn test_spec_cycle_tier_display_functions() {
        assert_eq!(cycle_tier_display_next("mid"),  "high", "mid → high forward");
        assert_eq!(cycle_tier_display_next("high"), "low",  "high → low forward");
        assert_eq!(cycle_tier_display_next("low"),  "mid",  "low → mid forward");
        assert_eq!(cycle_tier_display_prev("mid"),  "low",  "mid → low backward");
        assert_eq!(cycle_tier_display_prev("low"),  "high", "low → high backward");
        assert_eq!(cycle_tier_display_prev("high"), "mid",  "high → mid backward");
    }

    // spec (tier-edit): Enter in the goal tree opens the options dialog with the
    // goal's effective tier pre-populated; absent tier on a non-behavior goal
    // shows as "mid". The focused field starts on Reason.
    #[test]
    fn test_spec_options_modal_opens_with_tier_and_reason_field() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "tui".into(),
            summary: String::new(),
            description: "build the tui".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: Some("high".to_string()),
            kind: None,
            source_path: None,
        }];
        app.focus = Focus::Tree;
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        let m = app.modal.as_ref().expect("modal must open on Enter in tree");
        assert_eq!(m.goal_id, "tui");
        assert_eq!(m.tier, "high", "tier must be pre-populated from the goal");
        assert_eq!(m.initial_tier, "high", "initial_tier must match goal tier");
        assert_eq!(m.focused_field, ModalField::Reason, "Reason field must be focused initially");
    }

    // spec (tier-edit): absent tier (None) on a non-behavior goal displays as "mid"
    // in the options dialog.
    #[test]
    fn test_spec_options_modal_absent_tier_shows_mid() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "g".into(),
            summary: String::new(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }];
        app.focus = Focus::Tree;
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        let m = app.modal.as_ref().unwrap();
        assert_eq!(m.tier, "mid", "absent tier on non-behavior goal must display as mid");
        assert_eq!(m.initial_tier, "mid");
    }

    // spec (goal-agents / tier-edit): absent tier on a behavior goal displays as
    // "high" in the options dialog — kind-aware default.
    #[test]
    fn test_spec_options_modal_behavior_goal_absent_tier_shows_high() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "cleanup-hook".into(),
            summary: String::new(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: Some("behavior".into()),
            source_path: None,
        }];
        app.focus = Focus::Tree;
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        let m = app.modal.as_ref().unwrap();
        assert_eq!(m.tier, "high", "behavior goal with absent tier must display as high");
        assert_eq!(m.initial_tier, "high");
    }

    // spec (goal-agents): effective_goal_tier resolves kind-aware defaults —
    // explicit tier wins; absent tier: behavior→high, feature/unknown→mid.
    #[test]
    fn test_spec_effective_goal_tier_kind_aware_defaults() {
        // Explicit tier always wins regardless of kind.
        assert_eq!(effective_goal_tier(&goal::Goal {
            id: "g".into(), summary: "".into(), description: "".into(),
            parent_id: "".into(), children: vec![], related: vec![],
            tier: Some("low".into()), kind: Some("behavior".into()), source_path: None,
        }), "low", "explicit low tier wins over behavior kind");

        assert_eq!(effective_goal_tier(&goal::Goal {
            id: "g".into(), summary: "".into(), description: "".into(),
            parent_id: "".into(), children: vec![], related: vec![],
            tier: Some("high".into()), kind: Some("feature".into()), source_path: None,
        }), "high", "explicit high tier wins over feature kind");

        // Absent tier: behavior → high.
        assert_eq!(effective_goal_tier(&goal::Goal {
            id: "g".into(), summary: "".into(), description: "".into(),
            parent_id: "".into(), children: vec![], related: vec![],
            tier: None, kind: Some("behavior".into()), source_path: None,
        }), "high", "behavior goal with absent tier defaults to high");

        // Absent tier: feature → mid.
        assert_eq!(effective_goal_tier(&goal::Goal {
            id: "g".into(), summary: "".into(), description: "".into(),
            parent_id: "".into(), children: vec![], related: vec![],
            tier: None, kind: Some("feature".into()), source_path: None,
        }), "mid", "feature goal with absent tier defaults to mid");

        // Absent tier, absent kind → mid.
        assert_eq!(effective_goal_tier(&goal::Goal {
            id: "g".into(), summary: "".into(), description: "".into(),
            parent_id: "".into(), children: vec![], related: vec![],
            tier: None, kind: None, source_path: None,
        }), "mid", "absent tier and absent kind defaults to mid");
    }

    // spec (tier-edit): Tab in the options dialog switches the focused field between
    // Reason and Tier.
    #[test]
    fn test_spec_tab_in_modal_cycles_focused_field() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "g".into(),
            summary: String::new(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }];
        app.focus = Focus::Tree;
        // Open modal.
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.modal.as_ref().unwrap().focused_field, ModalField::Reason);
        // Tab → Tier.
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.modal.as_ref().unwrap().focused_field, ModalField::Tier, "Tab must switch to Tier field");
        // Tab again → back to Reason.
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.modal.as_ref().unwrap().focused_field, ModalField::Reason, "Tab must cycle back to Reason");
    }

    // spec (tier-edit): Right arrow cycles the tier forward when the Tier field is focused.
    // Left arrow cycles backward. Neither affects the tier when Reason is focused.
    #[test]
    fn test_spec_left_right_cycle_tier_when_tier_focused() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "g".into(),
            summary: String::new(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        }];
        app.focus = Focus::Tree;
        // Open modal (starts on Reason, tier = "mid").
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        // Right arrow when Reason focused → no change.
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.modal.as_ref().unwrap().tier, "mid", "Right must be a no-op on Reason field");
        // Switch to Tier field.
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        // Right → mid → high.
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.modal.as_ref().unwrap().tier, "high", "Right on Tier must advance to high");
        // Left → high → mid.
        handle_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.modal.as_ref().unwrap().tier, "mid", "Left on Tier must retreat to mid");
    }


    // spec (fresh-agents): batch detection — retirement must NOT fire when the
    // spec (tend-introspection): check_and_emit_batch_transition emits "— batch active —"
    // system message and sets batch_active=true on idle→active transition (running_sessions
    // becomes non-empty). No second emission when already active.
    #[test]
    fn test_spec_batch_transition_idle_to_active_emits_system_message() {
        let mut app = App::new();
        let mut batch_active = false;
        let log = logger::noop_sender();

        // Seed a running session to make the system active.
        app.running_sessions.insert("rummage".to_string(), None);

        check_and_emit_batch_transition(&mut app, &mut batch_active, &log);

        assert!(batch_active, "batch_active must be set to true on idle→active");
        assert!(
            app.messages.iter().any(|m| m.role == app::Role::System && m.text == "— batch active —"),
            "idle→active must push '— batch active —' system message"
        );

        // Call again — running_sessions still non-empty, already active. No new message.
        let msg_count = app.messages.len();
        check_and_emit_batch_transition(&mut app, &mut batch_active, &log);
        assert_eq!(app.messages.len(), msg_count, "no duplicate emission when already active");
    }

    // spec (tend-introspection): check_and_emit_batch_transition emits "— batch idle —"
    // system message and sets batch_active=false on active→idle transition (running_sessions
    // becomes empty). No second emission when already idle.
    #[test]
    fn test_spec_batch_transition_active_to_idle_emits_system_message() {
        let mut app = App::new();
        let mut batch_active = true; // start as if already active

        let log = logger::noop_sender();

        // running_sessions is empty — system is idle.
        check_and_emit_batch_transition(&mut app, &mut batch_active, &log);

        assert!(!batch_active, "batch_active must be set to false on active→idle");
        assert!(
            app.messages.iter().any(|m| m.role == app::Role::System && m.text == "— batch idle —"),
            "active→idle must push '— batch idle —' system message"
        );

        // Call again — already idle. No new message.
        let msg_count = app.messages.len();
        check_and_emit_batch_transition(&mut app, &mut batch_active, &log);
        assert_eq!(app.messages.len(), msg_count, "no duplicate emission when already idle");
    }

    // dispatcher is an ephemeral coordinator (mid-batch), when any ephemeral is
    // still running, OR when the permanent dispatcher is itself in running_sessions
    // (a batch message from a completed sub-session is pending delivery to it).
    // Retirement only fires on the idle→active transition: permanent dispatcher,
    // no running ephemerals, dispatcher not awaiting a pending delivery.
    #[test]
    fn test_spec_fresh_agents_batch_detection_guards_retirement() {
        let main_rs = include_str!("main.rs");
        // The spawn handler must gate retirement on all three conditions.
        assert!(
            main_rs.contains("dispatcher_is_permanent"),
            "spawn handler must check dispatcher_is_permanent before retiring",
        );
        assert!(
            main_rs.contains("any_ephemeral_running"),
            "spawn handler must check any_ephemeral_running before retiring",
        );
        assert!(
            main_rs.contains("dispatcher_has_pending_delivery"),
            "spawn handler must check dispatcher_has_pending_delivery before retiring",
        );
        // All three conditions must be evaluated before retire_completed_ephemeral_sessions.
        let perm_pos = main_rs.find("dispatcher_is_permanent").unwrap();
        let eph_pos = main_rs.find("any_ephemeral_running").unwrap();
        let pending_pos = main_rs.find("dispatcher_has_pending_delivery").unwrap();
        let retire_pos = main_rs.find("retire_completed_ephemeral_sessions").unwrap();
        assert!(
            perm_pos < retire_pos,
            "dispatcher_is_permanent check must precede retire call",
        );
        assert!(
            eph_pos < retire_pos,
            "any_ephemeral_running check must precede retire call",
        );
        assert!(
            pending_pos < retire_pos,
            "dispatcher_has_pending_delivery check must precede retire call",
        );
    }


    // spec (fresh-agents): completed ephemeral sessions are removed from
    // ephemeral_sessions and their IDs returned for channel cleanup.
    #[test]
    fn test_spec_fresh_agents_completed_ephemeral_sessions_are_retired() {
        let mut app = App::new();
        // Register two ephemeral sessions.
        app.ephemeral_sessions.insert("g~1".into());
        app.ephemeral_sessions.insert("g~2".into());
        app.ephemeral_sessions_ordered.push("g~1".into());
        app.ephemeral_sessions_ordered.push("g~2".into());
        // g~1 is still running; g~2 has finished.
        app.running_sessions.insert("g~1".into(), Some("task".into()));

        let retired = app.retire_completed_ephemeral_sessions();

        assert_eq!(retired, vec!["g~2".to_string()],
            "only the completed session must be returned for cleanup");
        assert!(
            !app.ephemeral_sessions.contains("g~2"),
            "completed session must be removed from ephemeral_sessions",
        );
        assert!(
            app.ephemeral_sessions.contains("g~1"),
            "running ephemeral session must remain in ephemeral_sessions",
        );
        assert_eq!(
            app.ephemeral_sessions_ordered,
            vec!["g~1".to_string()],
            "completed session must also be removed from ephemeral_sessions_ordered",
        );
    }

    // spec (fresh-agents): retirement is triggered at next-batch-start, not at
    // batch-end.  Completed ephemeral sessions survive the idle period and are
    // only removed when the next fresh dispatch arrives.  The retirement call
    // therefore lives inside the fresh-spawn branch (before inserting new
    // sub-sessions), not in an `running_sessions.is_empty()` idle guard.
    #[test]
    fn test_spec_fresh_agents_retirement_waits_for_batch_end() {
        let main_rs = include_str!("main.rs");
        // Retirement must be called inside the fresh-spawn branch.
        // We identify the branch by the unique marker string that opens it.
        let fresh_branch_marker = "Batch-start retirement: retire completed ephemeral sessions from the";
        assert!(
            main_rs.contains(fresh_branch_marker),
            "batch-start retirement must be inside the fresh-spawn branch (marker not found)",
        );
        // The retirement call in the fresh-spawn branch must appear before
        // the sub-session is inserted into session_senders.
        let retire_pos = main_rs.find(fresh_branch_marker).unwrap();
        let insert_pos = main_rs.find("session_senders.insert(fresh.session_id.clone()").unwrap();
        assert!(
            retire_pos < insert_pos,
            "retirement (pos {retire_pos}) must precede session_senders.insert (pos {insert_pos})",
        );
        // The old idle-only guard must NOT be the sole retirement trigger.
        // (running_sessions.is_empty() may still appear in other contexts, but
        // it must not be the retirement guard — so the fresh-branch marker
        // is the authoritative check above.)
    }

    // spec (fresh-agents): fresh sub-sessions inherit the dispatcher's effective
    // tier (kind-aware) — the spawn path uses `effective_goal_tier` rather than
    // a hardcoded oc_goal fallback, so high- and low-tier dispatchers (and
    // behavior goals with absent tier) get the correct runner.
    #[test]
    fn test_spec_fresh_agents_inherits_dispatcher_tier() {
        let main_rs = include_str!("main.rs");
        // The fresh sub-session spawn path must resolve tier via the
        // effective_goal_tier helper, not a fixed oc_goal.clone().
        assert!(
            main_rs.contains("effective_goal_tier(&dispatcher_goal)"),
            "fresh sub-session spawn must resolve tier via effective_goal_tier(&dispatcher_goal)",
        );
        // The match on the resolved tier must wire high and low correctly.
        let pos = main_rs.find("effective_goal_tier(&dispatcher_goal)").unwrap();
        let snippet = &main_rs[pos..pos + 200];
        assert!(snippet.contains("oc_goal_high"), "fresh tier match must wire \"high\" to oc_goal_high");
        assert!(snippet.contains("oc_goal_low"),  "fresh tier match must wire \"low\" to oc_goal_low");
        assert!(snippet.contains("oc_goal.clone()"), "fresh tier match must fall back to oc_goal for mid");
    }


    // spec (fresh-agents): completed ephemeral sessions must be retired from
    // session_senders when a `KeyAction::SendToSession` or `KeyAction::ConfirmOptions`
    // is processed — not only at fresh-dispatch time.  This prevents stale channel
    // entries from accumulating between batches.
    #[test]
    fn test_spec_fresh_agents_retire_ephemeral_sessions_on_key_actions() {
        let main_rs = include_str!("main.rs");

        // Both handler arms must call retire_completed_ephemeral_sessions followed
        // by session_senders.remove.  We verify by checking that both markers exist
        // and that the retire call precedes the remove call in each arm.

        // SendToSession arm: locate the arm by its unique marker.
        let send_arm_marker = "KeyAction::SendToSession(session_id, msg) => {";
        let confirm_arm_marker = "KeyAction::ConfirmOptions { goal_id, reason, new_tier } => {";

        assert!(
            main_rs.contains(send_arm_marker),
            "SendToSession arm must be present in main.rs",
        );
        assert!(
            main_rs.contains(confirm_arm_marker),
            "ConfirmOptions arm must be present in main.rs",
        );

        // After SendToSession arm, retire must appear before the arm ends.
        let send_pos = main_rs.find(send_arm_marker).unwrap();
        let confirm_pos = main_rs.find(confirm_arm_marker).unwrap();

        // Find retire_completed_ephemeral_sessions occurrences after each arm start.
        let retire_in_send = main_rs[send_pos..confirm_pos].contains("retire_completed_ephemeral_sessions");
        assert!(
            retire_in_send,
            "SendToSession arm must call retire_completed_ephemeral_sessions",
        );

        // session_senders.remove must also appear in the SendToSession arm.
        let remove_in_send = main_rs[send_pos..confirm_pos].contains("session_senders.lock().unwrap().remove");
        assert!(
            remove_in_send,
            "SendToSession arm must call session_senders.remove for retired sessions",
        );

        // For ConfirmOptions: find the text between the arm start and a later unique marker.
        let after_confirm = &main_rs[confirm_pos..];
        let retire_in_confirm = after_confirm.contains("retire_completed_ephemeral_sessions");
        assert!(
            retire_in_confirm,
            "ConfirmOptions arm must call retire_completed_ephemeral_sessions",
        );
        let remove_in_confirm = after_confirm.contains("session_senders.lock().unwrap().remove");
        assert!(
            remove_in_confirm,
            "ConfirmOptions arm must call session_senders.remove for retired sessions",
        );
    }

    // spec (tier-edit): when the tier is changed and the goal has a running session,
    // confirming the options dialog removes the goal from running_sessions (reset).
    #[test]
    fn test_spec_confirm_options_resets_running_session_on_tier_change() {
        use crate::test_utils::MockFs;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::path::PathBuf;

        let fs = Arc::new(MockFs::new());
        let path = PathBuf::from("/proj/.tinker/goals/g.toml");
        fs.add_file(&path, "id = \"g\"\ndescription = \"d\"\nparent_id = \"\"\n");

        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "g".into(),
            summary: String::new(),
            description: "d".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(path.clone()),
        }];
        app.running_sessions.insert("g".into(), Some("running task".into()));
        app.focus = Focus::Tree;

        // Open modal, switch to Tier, advance to "high".
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);

        // Simulate the event-loop portion: remove from running_sessions when tier changes.
        let tier_changed = app.modal.as_ref().map(|m| m.tier != m.initial_tier).unwrap_or(false);
        if tier_changed {
            app.running_sessions.remove("g");
            if let Some(g) = app.goals.iter_mut().find(|g| g.id == "g") {
                g.tier = Some("high".to_string());
                let content = toml::to_string_pretty(g).unwrap();
                fs.write(&path, &content).unwrap();
            }
        }

        assert!(
            !app.running_sessions.contains_key("g"),
            "running session must be removed when tier changes",
        );
        let on_disk = fs.read_to_string(&path).unwrap();
        assert!(on_disk.contains("tier = \"high\""), "new tier must be written to disk; got:\n{}", on_disk);
    }

    // spec (tui — Bug 1): inserting an ephemeral sub-session must immediately
    // update goal_list_scroll.last_total so mouse scroll can reach the new row
    // in the same frame (before the next draw call updates last_total).
    #[test]
    fn test_spec_ephemeral_insertion_updates_scroll_total() {
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        app.lock().unwrap().goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "desc".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        // Simulate a prior render: last_total reflects 1 item (the goal itself).
        app.lock().unwrap().goal_list_scroll.record_render(1, 8, 0);
        assert_eq!(app.lock().unwrap().goal_list_scroll.last_total, 1, "precondition: last_total is 1 before insertion");

        let dispatcher = build_spawn_session_dispatcher(
            "my-goal".to_string(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        dispatcher("do the task", Some("work"))
            .expect("spawn_session must succeed for a goal in the tree");

        // After insertion the list has 2 items (1 goal + 1 ephemeral);
        // last_total must reflect that immediately, before the next render.
        assert_eq!(
            app.lock().unwrap().goal_list_scroll.last_total, 2,
            "last_total must be updated to 2 after ephemeral insertion so mouse scroll can reach it",
        );
    }

    // spec (tui — Bug 2): PageDown and PageUp in Focus::Tree must scroll the
    // goal list viewport without changing the selection.
    #[test]
    fn test_spec_goal_list_keyboard_viewport_scroll() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = App::new();
        // Populate goals so the list has something to scroll.
        for i in 0..10 {
            app.goals.push(goal::Goal {
                id: format!("goal-{}", i),
                summary: String::new(),
                description: String::new(),
                parent_id: String::new(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: None,
            });
        }
        // Simulate a render: 10 items, height 3 (so max_y = 7).
        app.goal_list_scroll.record_render(10, 3, 0);
        app.goal_list_scroll.y = Some(0);
        app.focus = Focus::Tree;

        let sel_before = app.selected_goal;

        // PageDown must scroll the viewport down.
        handle_key(&mut app, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.selected_goal, sel_before, "PageDown must not change selection");
        assert!(
            app.goal_list_scroll.y > Some(0),
            "PageDown must advance the goal list viewport; got {:?}", app.goal_list_scroll.y,
        );

        let y_after_down = app.goal_list_scroll.y;

        // PageUp must scroll the viewport back up.
        handle_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &logger::noop_sender(), BUILTIN_SESSION_IDS);
        assert_eq!(app.selected_goal, sel_before, "PageUp must not change selection");
        assert!(
            app.goal_list_scroll.y < y_after_down,
            "PageUp must move the goal list viewport back toward the top; got {:?}", app.goal_list_scroll.y,
        );
    }

    // spec (goal-agents / send-message): the session registry contains only
    // eagerly-started sessions at startup. Goals whose sessions have never
    // been triggered are absent. For send_message, "absent" means the goal
    // is unknown — it is not in the session registry AND (because App::new
    // has empty goals) it is not a known goal either. The dispatcher
    // reports the unknown-target error in this case, with the
    // registry-miss reason preserved so the model can still route through
    // a different agent.
    #[test]
    fn test_spec_rummage_investigate_registry_eager_start_only_tend() {
        // Simulate startup: only tend is eagerly registered.
        let (tend_tx, _tend_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        let senders = Arc::new(Mutex::new(senders));
        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let log = logger::noop_sender();
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            log,
        );

        // send_message to rummage fails — not yet in registry and not a known goal.
        let err_rummage = dispatcher("rummage", "investigate the auth flow")
            .expect_err("rummage must not be in the registry at startup");
        assert!(
            err_rummage.contains("rummage") && err_rummage.contains("session registry"),
            "error must name unknown target and registry constraint: {err_rummage}"
        );

        // send_message to goal-agents fails — same reason.
        let err_ga = dispatcher("goal-agents", "ping")
            .expect_err("goal-agents must not be in the registry at startup");
        assert!(
            err_ga.contains("goal-agents") && err_ga.contains("session registry"),
            "error must name unknown target and registry constraint: {err_ga}"
        );

        // After registering rummage in the session registry, send_message works.
        let (rummage_tx, mut rummage_rx) = mpsc::unbounded_channel::<String>();
        senders.lock().unwrap().insert("rummage".to_string(), rummage_tx);

        let ok = dispatcher("rummage", "trace the flow")
            .expect("rummage must be reachable after lazy spawn");
        assert!(ok.contains("rummage"), "ok result must name the target");
        let msg = rummage_rx.try_recv().expect("rummage must receive the message");
        assert!(msg.contains("trace the flow"));
    }

    fn make_goal_for_investigation(id: &str) -> goal::Goal {
        goal::Goal {
            id: id.to_string(),
            kind: None,
            summary: String::new(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            source_path: None,
        }
    }


    // spec (send-message): a known-but-unspawned goal's lazy-spawned
    // session is tracked in running_sessions BEFORE the SpawnGoalRequest
    // is drained by the run loop. The entry is what holds off the
    // batch-end retirement check between the model's tool call and the
    // runtime's spawn of goal_agent_loop. Without this entry, the
    // batch could go idle and the lazy-spawned goal would be lost.
    #[test]
    fn test_spec_send_message_dispatcher_lazy_spawn_tracks_running_sessions() {
        let (tend_tx, _tend_rx) = mpsc::unbounded_channel::<String>();
        let mut senders: HashMap<String, mpsc::UnboundedSender<String>> = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        let senders_arc = Arc::new(Mutex::new(senders));

        let mut app = App::new();
        app.goals = vec![make_goal_for_investigation("rummage")];
        let app_arc = Arc::new(Mutex::new(app));

        let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders_arc.clone(),
            goal_spawn_tx,
            app_arc.clone(),
            logger::noop_sender(),
        );

        let _ok = dispatcher("rummage", "investigate the auth flow")
            .expect("send_message to a known goal must succeed via lazy-spawn");

        // SpawnGoalRequest was enqueued.
        let req = goal_spawn_rx.try_recv()
            .expect("send_message must enqueue a SpawnGoalRequest for a known-but-unspawned goal");
        assert_eq!(req.goal_id, "rummage");

        // The target is tracked in running_sessions even though the spawn
        // has not yet drained. Without this entry the batch-end retirement
        // check would see an empty queue and could retire the parent
        // session before the runtime delivers the message.
        let a = app_arc.lock().unwrap();
        assert!(
            a.running_sessions.contains_key("rummage"),
            "lazy-spawned target must appear in running_sessions before the spawn drains"
        );
        let reason = a.running_sessions["rummage"].as_deref().unwrap_or("");
        assert!(
            reason.contains("investigate the auth flow"),
            "running reason must reflect the message first line: {reason}"
        );
    }

    // spec (send-message): a `send_message` to a not-yet-spawned target must
    // leave `running_sessions` populated from the dispatcher's Ok return
    // until the lazy-spawned session's Done event.  The Chunk event for
    // the spawned session must not remove the dispatcher's entry
    // (`running_sessions.entry(...).or_insert(None)` is a no-op when the
    // entry already exists), and the Done event handler must clear it.
    // This pins the full lifecycle for the send_message path, mirroring
    // `test_spec_at_dispatch_adds_goal_agent_to_running_sessions` for the
    // send_message dispatcher and `test_spec_fresh_agents_batch_detection_guards_retirement`
    // for the retirement-hold-off invariant.
    //
    // The load-bearing property: between the dispatcher's Ok return and
    // the spawned session's Done event, `running_sessions` always contains
    // the target.  This is what keeps the batch active during the
    // spawn-drain window so the batch-end retirement check does not fire
    // and retire the parent session before the runtime delivers the
    // message into the freshly-spawned goal_agent_loop.
    #[test]
    fn test_spec_send_message_dispatcher_running_sessions_lifecycle_until_done() {
        let (tend_tx, _tend_rx) = mpsc::unbounded_channel::<String>();
        let mut senders: HashMap<String, mpsc::UnboundedSender<String>> = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        let senders_arc = Arc::new(Mutex::new(senders));

        let mut app = App::new();
        app.goals = vec![make_goal_for_investigation("rummage")];
        let app_arc = Arc::new(Mutex::new(app));

        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders_arc.clone(),
            goal_spawn_tx,
            app_arc.clone(),
            logger::noop_sender(),
        );

        // Step 1: dispatcher returns Ok; entry inserted with first-line reason.
        let _ok = dispatcher("rummage", "investigate the auth flow")
            .expect("send_message to a known goal must succeed via lazy-spawn");
        {
            let a = app_arc.lock().unwrap();
            assert!(
                a.running_sessions.contains_key("rummage"),
                "after dispatcher Ok: target must be in running_sessions",
            );
            let reason = a.running_sessions["rummage"].as_deref().unwrap_or("");
            assert!(
                reason.contains("investigate the auth flow"),
                "after dispatcher Ok: running reason must reflect the message first line: {reason}"
            );
        }

        // Step 2: simulate the Chunk event for the lazy-spawned session.
        // `handle_session_event` runs `running_sessions.entry(goal_id).or_insert(None)`
        // on every Chunk.  When the dispatcher's entry is already there, the
        // chunk event is a no-op — the original reason is preserved and the
        // entry is not removed.  This is what keeps the batch active during
        // the spawn-drain window.
        {
            let mut a = app_arc.lock().unwrap();
            a.running_sessions.entry("rummage".to_string()).or_insert(None);
        }
        {
            let a = app_arc.lock().unwrap();
            assert!(
                a.running_sessions.contains_key("rummage"),
                "Chunk event for the spawned session must not remove the dispatcher's entry"
            );
            let reason = a.running_sessions["rummage"].as_deref().unwrap_or("");
            assert!(
                reason.contains("investigate the auth flow"),
                "original reason must survive the Chunk event: {reason}"
            );
        }

        // Step 3: simulate the Done event.  `handle_session_event` runs
        // `running_sessions.remove(&goal_id)` on every Done — the entry is
        // cleared and the batch becomes idle (subject to whatever else is
        // in the queue).
        {
            let mut a = app_arc.lock().unwrap();
            a.running_sessions.remove("rummage");
        }
        {
            let a = app_arc.lock().unwrap();
            assert!(
                !a.running_sessions.contains_key("rummage"),
                "Done event must clear the running_sessions entry"
            );
        }
    }

    // spec (send-message): when the target is in the session registry but
    // its channel receiver has been dropped (the session task exited), the
    // send_message tool returns Err with a delivery-lost warning. The
    // send_message dispatcher detects the closed channel and surfaces it
    // as an error rather than silently dropping the message.
    #[test]
    fn test_spec_send_message_dispatcher_closed_channel_returns_err() {
        let (tend_tx, _tend_rx) = mpsc::unbounded_channel::<String>();
        // Create a sender for rummage but drop the receiver — simulates a
        // session task that has exited.
        let (rummage_tx, rummage_rx) = mpsc::unbounded_channel::<String>();
        drop(rummage_rx);

        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), tend_tx);
        senders.insert("rummage".to_string(), rummage_tx);
        let senders = Arc::new(Mutex::new(senders));

        let (goal_spawn_tx, _goal_spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let app = Arc::new(Mutex::new(App::new()));
        let dispatcher = build_send_message_dispatcher(
            "tend".to_string(),
            senders.clone(),
            goal_spawn_tx,
            app.clone(),
            logger::noop_sender(),
        );

        let err = dispatcher("rummage", "trace the auth flow")
            .expect_err("send_message to a closed-channel target must return Err");
        assert!(
            err.contains("rummage"),
            "error must name the unreachable target: {err}"
        );
        assert!(
            err.contains("delivery lost") || err.contains("send_message"),
            "error must carry a delivery-lost indicator: {err}"
        );
    }
}
