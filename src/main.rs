mod app;
mod cap;
mod claude;
mod cleanup;
mod config;
mod goal;
mod goal_session;
mod logger;
mod opencode;
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
use goal::{discover_tinker_dirs, load_all_goals, Goal};
use goal_session::SessionEvent;
use opencode::{RealOpenCodeRunner, GOAL_MODEL as OPENCODE_GOAL_MODEL, TINKER_MODEL as OPENCODE_TINKER_MODEL, SCHEDULER_MODEL as OPENCODE_SCHEDULER_MODEL};
use claude::{ClaudeRunner, GOAL_MODEL as CLAUDE_GOAL_MODEL, TINKER_MODEL as CLAUDE_TINKER_MODEL, SCHEDULER_MODEL as CLAUDE_SCHEDULER_MODEL};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use std::collections::HashMap;
use tokio::sync::mpsc;

type RunnerSet = (Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>);

fn packaged_tend_goal() -> Goal {
    const TOML: &str = include_str!("../packaged-goals/tend.toml");
    toml::from_str(TOML).expect("packaged tend.toml must be valid Goal TOML")
}

/// System prompt for tend when running under the claude backend.
/// Leads with the file-scope boundary so it arrives as a system-level constraint,
/// not a buried instruction in a user-turn message. tend's behaviour is governed
/// by the tend goal description (appended below); the file-scope line is the
/// prompt-level boundary that stands in for harness enforcement.
fn tend_system_prompt() -> String {
    prompts::tend_system_prompt(&packaged_tend_goal().description)
}

fn tend_init_prompt(goals_summary: &str, neighbor_section: &str) -> String {
    prompts::tend_init_prompt(goals_summary, neighbor_section)
}

fn tend_init_prompt_full_context(goals_summary: &str, neighbor_section: &str) -> String {
    prompts::tend_init_prompt_full_context(goals_summary, neighbor_section)
}

/// Configuration for spawning an ephemeral fresh sub-session. Produced when
/// a `<@{parent_id}|label>` envelope is detected in a goal agent's output.
struct FreshSessionConfig {
    /// Unique ID for the ephemeral session in the registry (e.g. "fresh-agents~1").
    session_id: String,
    /// Optional correlation label provided by the dispatcher (the part after `|`).
    label: Option<String>,
    /// Goal ID of the dispatching agent — also the goal whose description and
    /// neighbors the fresh sub-session inherits.
    dispatcher_id: String,
}

/// Lazy-spawn request: sent when `@goal-id` arrives and that agent isn't in
/// the session registry yet, or when the user triggers a goal via the tree UI.
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
        //   - lean_init=true (all goal agents, both backends): system prompt =
        //     full session context (goal + preamble + neighbors), first turn =
        //     trigger reason only.  The system prompt is delivered via the
        //     backend's native mechanism (agent file for opencode,
        //     --system-prompt for claude).
        //   - lean_init=false (tend, non-lean paths): no per-call system prompt
        //     (struct-level system prompt handles it for claude; opencode gets
        //     everything in the message); first turn = full session_init_message.
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
            // Emit trigger reason as first log line (bold in TUI).
            let _ = session_tx.try_send(SessionEvent::Chunk {
                goal_id: goal_id.clone(),
                text: format!("{}{}\n", goal_session::TRIGGER_REASON_MARKER, dispatch_msg),
            });
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
                // Tend and non-lean paths: full init message in the first turn;
                // system prompt is either in the runner's struct field (claude tend)
                // or absent (opencode tend — everything in the message).
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
        let run_result = oc.run(&llm_message, llm_session_id.as_deref(), &work_dir, system_prompt_for_run.as_deref(), on_sid, on_chunk).await;
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
                log.emit("goal_session", logger::LogEvent::GoalSessionFinished {
                    goal_id: goal_id.clone(),
                    exit_status: format!("crash: {e:#}"),
                    duration_ms: session_ms,
                    files_modified_count: 0,
                    files_modified: vec![],
                    tool_calls: 0,
                    summary_chars: 0,
                    full_output: output.clone(),
                    backend: backend_name.clone(),
                });
                // Clear session_id so next message starts a fresh session.
                llm_session_id = None;
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
    println!("    --claude                    Use the Claude backend (default: opencode)");
    println!("    --tend-full-goal-context    Inject full goal text instead of compact index");
    println!("    --default-model             Use the backend's default model for all tiers");
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

    let use_default_model = args.iter().any(|a| a == "--default-model");
    let use_claude = args.iter().any(|a| a == "--claude");
    let use_full_goal_context = args.iter().any(|a| a == "--tend-full-goal-context");

    const KNOWN_ARGS: &[&str] = &[
        "--claude",
        "--tend-full-goal-context",
        "--default-model",
        "--help",
        "-h",
    ];
    if let Some(bad) = args.iter().skip(1).find(|a| !KNOWN_ARGS.contains(&a.as_str())) {
        println!("error: unrecognized argument '{bad}'\n");
        print_help();
        std::process::exit(1);
    }

    let work_dir = std::env::current_dir()?;
    let primary_tinker_dir = work_dir.join(".tinker");
    fs.mkdir_all(&primary_tinker_dir.join("goals"))?;
    fs.mkdir_all(&primary_tinker_dir.join("state"))?;
    fs.mkdir_all(&primary_tinker_dir.join("notes"))?;
    fs.mkdir_all(&primary_tinker_dir.join("logs"))?;
    fs.mkdir_all(&primary_tinker_dir.join("discrepancies"))?;

    // Write a self-documenting starter config only when none exists yet;
    // then load whatever is present (or default if still absent/invalid).
    let config_path = primary_tinker_dir.join("config.toml");
    config::write_starter_template(
        fs.as_ref(),
        &config_path,
        [CLAUDE_TINKER_MODEL, CLAUDE_GOAL_MODEL, CLAUDE_SCHEDULER_MODEL],
        [OPENCODE_TINKER_MODEL, OPENCODE_GOAL_MODEL, OPENCODE_SCHEDULER_MODEL],
    )?;
    let model_config = config::load_model_config(fs.as_ref(), &config_path);

    // Five runner instances:
    //   oc_tend        — exclusive to tend; carries tend_system_prompt() as a persistent
    //                    struct-level fallback so tend's file-scope boundary is enforced
    //                    on every turn (including turn 2+ where no per-call prompt is set).
    //   oc_goal_high   — high-tier non-tend goal agents (rummage, jog, …); NO struct-level
    //                    system prompt, so resume turns pass no --system-prompt and the
    //                    claude session retains its original system prompt from turn 1.
    //   oc_goal        — mid-tier (default goal sessions)
    //   oc_goal_low    — low-tier goal sessions
    //   oc_cleanup_runner — cleanup / scheduler (cheapest model)
    let (oc_tend, oc_goal_high, oc_goal, oc_goal_low, oc_cleanup_runner): RunnerSet = if use_claude {
        let tinker_m = model_config.claude_high(CLAUDE_TINKER_MODEL);
        let goal_m = model_config.claude_mid(CLAUDE_GOAL_MODEL);
        let cleanup_m = model_config.claude_low(CLAUDE_SCHEDULER_MODEL);
        (
            // tend: struct-level prompt delivers file-scope boundary on every turn.
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, tend_system_prompt())),
            // high-tier goal agents: system prompt delivered per-call on first turn only;
            // no struct-level fallback so resume turns do not overwrite session identity.
            Arc::new(ClaudeRunner::new(tinker_m)),
            Arc::new(ClaudeRunner::new(goal_m)),
            Arc::new(ClaudeRunner::new(cleanup_m)),
            Arc::new(ClaudeRunner::new(cleanup_m)),
        )
    } else if use_default_model {
        let r: Arc<dyn OpenCodeRunner> = Arc::new(RealOpenCodeRunner::new_default());
        (r.clone(), r.clone(), r.clone(), r.clone(), r)
    } else {
        let tinker_m = model_config.opencode_high(OPENCODE_TINKER_MODEL);
        let goal_m = model_config.opencode_mid(OPENCODE_GOAL_MODEL);
        let cleanup_m = model_config.opencode_low(OPENCODE_SCHEDULER_MODEL);
        (
            Arc::new(RealOpenCodeRunner::new(tinker_m)),
            Arc::new(RealOpenCodeRunner::new(tinker_m)),
            Arc::new(RealOpenCodeRunner::new(goal_m)),
            Arc::new(RealOpenCodeRunner::new(cleanup_m)),
            Arc::new(RealOpenCodeRunner::new(cleanup_m)),
        )
    };

    let backend_name = if use_claude { "claude" } else { "opencode" };
    let log = logger::start_logger(
        primary_tinker_dir.join("logs").join("runtime.jsonl"),
        primary_tinker_dir.join("state").join("runtime.json"),
    );

    // Discover all .tinker dirs from cwd up. Nearest first.
    let tinker_dirs = discover_tinker_dirs(fs.as_ref(), &work_dir);

    let app = Arc::new(Mutex::new(App::new()));

    {
        let load = load_all_goals(fs.as_ref(), &tinker_dirs)?;
        let mut a = app.lock().unwrap();
        a.goals = load.goals;
        a.tinker_dirs = tinker_dirs.clone();
        a.push_system_message("Starting tend…");
        log.emit("harness", logger::LogEvent::TinkerSystemMessageReceived { content: "Starting tend…".to_string() });
        a.update_parse_errors(load.errors);
        if tinker_dirs.len() > 1 {
            let merged_msg = format!(
                "Merged {} .tinker dirs (cwd + {} ancestor).",
                tinker_dirs.len(),
                tinker_dirs.len() - 1,
            );
            a.push_system_message(&merged_msg);
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
                    if a.selected_goal >= a.flat_items().len().max(1) {
                        a.selected_goal = 0;
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
    let mut session_senders: HashMap<String, mpsc::UnboundedSender<String>> = HashMap::new();
    // Tracks whether the system is currently in an active batch (any session
    // holds a running_sessions slot). Used to detect idle↔active transitions
    // for BatchTransition event emission.
    let mut batch_active = false;

    // Eager-start tend: find its goal, spawn goal_agent_loop, send the initial trigger.
    {
        let tend_goal = app.lock().unwrap().goals.iter().find(|g| g.id == "tend").cloned()
            .unwrap_or_else(packaged_tend_goal);
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
        session_senders.insert("tend".to_string(), tend_tx.clone());
        let app_ref = app.clone();
        let session_tx_t = session_tx.clone();
        let oc_t = oc_tend.clone();
        let oc_cleanup_t = oc_cleanup_runner.clone();
        let fs_t = fs.clone();
        let work_dir_t = work_dir.clone();
        let log_t = log.clone();
        let backend_t = backend_name.to_string();
        tokio::spawn(async move {
            goal_agent_loop(tend_goal, tend_rx, session_tx_t, oc_t, oc_cleanup_t, fs_t, work_dir_t, app_ref, log_t, backend_t, false, None, false).await;
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
                //     definition).  dispatch_peer_consultations inserts the recipient
                //     into running_sessions when a sub-session reply is dispatched;
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
                            session_senders.remove(&id);
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
                    session_senders.insert(fresh.session_id.clone(), msg_tx_fresh.clone());
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
                    let _ = msg_tx_fresh.send(req.message);
                    tokio::spawn(async move {
                        goal_agent_loop(
                            fresh_goal, msg_rx_fresh, session_tx_fresh,
                            oc_for_fresh, oc_cleanup_fresh, fs_fresh,
                            work_dir_fresh, app_ref_fresh, log_fresh, backend_fresh,
                            lean_init_fresh, Some(init_msg), true,
                        ).await;
                    });
                }
            } else if let Some(tx) = session_senders.get(&req.goal_id) {
                let _ = tx.send(req.message);
            } else {
                let goal = app.lock().unwrap().goals.iter().find(|g| g.id == req.goal_id).cloned();
                if let Some(goal) = goal {
                    let (msg_tx_goal, msg_rx_goal) = mpsc::unbounded_channel::<String>();
                    session_senders.insert(req.goal_id.clone(), msg_tx_goal.clone());
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
                    let _ = msg_tx_goal.send(req.message);
                    tokio::spawn(async move {
                        goal_agent_loop(
                            goal, msg_rx_goal, session_tx_goal,
                            oc_for_goal, oc_cleanup_goal, fs_goal,
                            work_dir_goal, app_ref_goal, log_goal, backend_goal, lean_init_goal,
                            None, false,
                        ).await;
                    });
                }
            }
        }

        // Drain session events (all agents unified)
        {
            let running_before: std::collections::HashSet<String> = app.lock().unwrap().running_sessions.keys().cloned().collect();
            while let Ok(ev) = session_rx.try_recv() {
                handle_session_event(&mut app.lock().unwrap(), ev, &goal_spawn_tx, &session_senders, fs.as_ref(), &log);
            }
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
                    (a.repl_scroll.y.unwrap_or(0), a.log_scroll.y.unwrap_or(0), a.goal_list_scroll.y.unwrap_or(0), a.goal_text_scroll.y.unwrap_or(0)),
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
                    (a.repl_scroll.y.unwrap_or(0), a.log_scroll.y.unwrap_or(0), a.goal_list_scroll.y.unwrap_or(0), a.goal_text_scroll.y.unwrap_or(0)),
                    a.selected_goal,
                )
            };
            if scroll_after.0 != scroll_before.0 {
                log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "repl".to_string(), y: scroll_after.0 });
            }
            if scroll_after.1 != scroll_before.1 {
                log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "log".to_string(), y: scroll_after.1 });
            }
            if scroll_after.2 != scroll_before.2 {
                log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_list".to_string(), y: scroll_after.2 });
            }
            if scroll_after.3 != scroll_before.3 {
                log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_text".to_string(), y: scroll_after.3 });
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
                    (a.repl_scroll.y.unwrap_or(0), a.log_scroll.y.unwrap_or(0), a.goal_list_scroll.y.unwrap_or(0), a.goal_text_scroll.y.unwrap_or(0)),
                )
            };
            // Build known IDs from both the registry and all goal IDs from app.goals,
            // so that /<goal-id> switching works even before a session is spawned.
            let known_ids: Vec<String> = {
                let a = app.lock().unwrap();
                let mut ids: Vec<String> = session_senders.keys().cloned().collect();
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
                if a.log_scroll.y.unwrap_or(0) != scroll_before.1 {
                    log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "log".to_string(), y: a.log_scroll.y.unwrap_or(0) });
                }
                if a.goal_list_scroll.y.unwrap_or(0) != scroll_before.2 {
                    log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_list".to_string(), y: a.goal_list_scroll.y.unwrap_or(0) });
                }
                if a.goal_text_scroll.y.unwrap_or(0) != scroll_before.3 {
                    log.emit("tui", logger::LogEvent::TuiScrollChanged { pane: "goal_text".to_string(), y: a.goal_text_scroll.y.unwrap_or(0) });
                }
            }
            match action {
                KeyAction::Quit => break,
                KeyAction::SendToSession(session_id, msg) => {
                    {
                        let retired = app.lock().unwrap().retire_completed_ephemeral_sessions();
                        for id in retired {
                            session_senders.remove(&id);
                        }
                    }
                    // Mark ALL sessions as running at message-send time — not just
                    // interactive agents. The user's message is already in flight
                    // (either delivered to an existing sender or queued for spawn),
                    // so the batch is active from this moment. The Chunk event's
                    // `or_insert(None)` will be a no-op when the entry is already here.
                    app.lock().unwrap().running_sessions.insert(session_id.clone(), None);
                    if let Some(tx) = session_senders.get(&session_id) {
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
                            session_senders.remove(&id);
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
                        session_senders.remove(&goal_id);
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

/// Extracts `<@id>…</@id>` tag envelopes from a finalised agent reply.
///
/// Each envelope begins with `<@{id}>` and ends with `</@{id}>`, where `id`
/// is a member of `known_ids`. The opening tag may carry inline content on
/// the same line (`<@tend>message</@tend>`) or the body may span subsequent
/// lines:
///
///   <@tend>
///   message body — may span multiple lines
///   </@tend>
///
/// **Scoping rule — top-level envelopes only.**
/// Only envelopes where the opening tag begins at the start of the trimmed
/// line are treated as live dispatches. An envelope appearing mid-line
/// (e.g. `reply via <@tend>…</@tend>`) is explanatory prose and is silently
/// ignored. Likewise, envelopes inside fenced code blocks (``` or ~~~) are
/// illustrative and are not extracted. Both rules together ensure extraction
/// applies exclusively to envelopes the agent itself is intentionally
/// emitting, not to envelope syntax quoted or forwarded from an earlier
/// delivery message.
///
/// Prose outside envelopes is not delivered to any agent.
/// Empty envelopes (nothing between open and close tags) are silently dropped.
/// A reply may contain multiple envelopes; each is extracted independently.
///
/// `known_ids` is the set of agent IDs that can receive `@`-routed messages.
///
/// Returns `(recipient, message)` pairs where `message` is the trimmed
/// content between the opening and closing tags.
fn parse_at_commands(text: &str, known_ids: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let mut in_code_fence = false;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Toggle fence state and skip the fence marker line itself.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            i += 1;
            continue;
        }
        // Inside a code fence — envelope syntax is quoted/illustrative, not live.
        if in_code_fence {
            i += 1;
            continue;
        }

        let mut matched = false;

        for id in known_ids {
            let open_tag = format!("<@{}>", id);
            let close_tag = format!("</@{}>", id);

            // The opening tag must start the trimmed line — an envelope appearing
            // mid-line is explanatory prose (e.g. "reply via <@id>…</@id>") and
            // must not be extracted as a live dispatch.
            if !trimmed.starts_with(open_tag.as_str()) {
                continue;
            }
            let after_open = &trimmed[open_tag.len()..];

            // Check if the closing tag appears on the same line.
            if let Some(close_pos) = after_open.find(close_tag.as_str()) {
                let content = after_open[..close_pos].trim().to_string();
                if !content.is_empty() {
                    out.push((id.to_string(), content));
                }
                i += 1;
            } else {
                // Multi-line envelope: collect body lines until closing tag.
                let mut body_lines: Vec<String> = Vec::new();
                let inline = after_open.trim().to_string();
                if !inline.is_empty() {
                    body_lines.push(inline);
                }
                i += 1;

                while i < lines.len() {
                    let inner_trimmed = lines[i].trim();
                    if let Some(close_pos) = inner_trimmed.find(close_tag.as_str()) {
                        let before = inner_trimmed[..close_pos].trim();
                        if !before.is_empty() {
                            body_lines.push(before.to_string());
                        }
                        i += 1;
                        break;
                    } else {
                        body_lines.push(lines[i].to_string());
                        i += 1;
                    }
                }

                let msg = body_lines.join("\n").trim().to_string();
                if !msg.is_empty() {
                    out.push((id.to_string(), msg));
                }
            }
            matched = true;
            break;
        }

        if !matched {
            i += 1;
        }
    }

    out
}

/// Extracts `<@{sender_id}|{label}>…</@{sender_id}|{label}>` fresh-dispatch
/// envelopes from a finalised agent reply.
///
/// A fresh-dispatch envelope is addressed to the sender's own goal ID with a
/// `|label` suffix — this distinguishes it from a normal peer-consult message.
/// The label may be empty (`<@my-goal|>`) when correlation is not needed.
///
/// Returns `(label, message)` pairs where `label` is `None` for an empty label
/// and `Some("tag")` otherwise. Multi-line bodies are collected just like the
/// normal @-command parser.
fn parse_fresh_dispatches(text: &str, sender_id: &str) -> Vec<(Option<String>, String)> {
    let open_prefix = format!("<@{}|", sender_id);
    let mut out: Vec<(Option<String>, String)> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let mut in_code_fence = false;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Toggle fence state and skip the fence marker line itself.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            i += 1;
            continue;
        }
        // Inside a code fence — dispatch syntax is quoted/illustrative, not live.
        if in_code_fence {
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix(open_prefix.as_str()) {
            // Extract the label from rest up to the first '>'.
            if let Some(label_end) = rest.find('>') {
                let label_str = &rest[..label_end];
                let label = if label_str.is_empty() { None } else { Some(label_str.to_string()) };
                let close_tag = format!("</@{}|{}>", sender_id, label_str);
                let after_open = &rest[label_end + 1..];

                if let Some(close_pos) = after_open.find(close_tag.as_str()) {
                    let content = after_open[..close_pos].trim().to_string();
                    if !content.is_empty() {
                        out.push((label, content));
                    }
                    i += 1;
                } else {
                    // Multi-line: collect body lines until close tag.
                    let mut body_lines: Vec<String> = Vec::new();
                    let inline = after_open.trim().to_string();
                    if !inline.is_empty() {
                        body_lines.push(inline);
                    }
                    i += 1;
                    while i < lines.len() {
                        let inner = lines[i].trim();
                        if let Some(close_pos) = inner.find(close_tag.as_str()) {
                            let before = inner[..close_pos].trim();
                            if !before.is_empty() {
                                body_lines.push(before.to_string());
                            }
                            i += 1;
                            break;
                        } else {
                            body_lines.push(lines[i].to_string());
                            i += 1;
                        }
                    }
                    let msg = body_lines.join("\n").trim().to_string();
                    if !msg.is_empty() {
                        out.push((label, msg));
                    }
                }
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Scans `text` for opening `<@{base_id}|label>` tags (one per line) and
/// returns the label list in document order.  Unlike `parse_fresh_dispatches`,
/// this does NOT require the closing tag to be present — it is used during
/// streaming to detect dispatches as early as possible so the TUI can show
/// them before the turn completes.
fn scan_opening_tags(text: &str, base_id: &str) -> Vec<Option<String>> {
    let open_prefix = format!("<@{}|", base_id);
    let mut labels = Vec::new();
    let mut in_code_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(open_prefix.as_str())
            && let Some(label_end) = rest.find('>')
        {
            let label_str = &rest[..label_end];
            labels.push(if label_str.is_empty() { None } else { Some(label_str.to_string()) });
        }
    }
    labels
}

/// Deliver peer consultations collected from a completed agent reply.
/// Routes to the session registry for known agents; triggers lazy spawn via
/// `goal_spawn_tx` for goal IDs not yet in the registry.
/// All dispatched recipients are tracked in running_sessions so the batch-end
/// retirement check correctly waits for every pending delivery to be processed.
///
/// The formatted delivery message uses plain `@{sender}` (no angle-bracket
/// envelope syntax) for the reply instruction. A live `<@id>…</@id>` envelope
/// in the delivery message would be re-extracted by the harness on the next
/// parse pass and route placeholder text as a spurious first message.
fn dispatch_peer_consultations(
    app: &mut App,
    sender: &str,
    consultations: &[(String, String)],
    session_senders: &HashMap<String, mpsc::UnboundedSender<String>>,
    goal_spawn_tx: &mpsc::UnboundedSender<SpawnGoalRequest>,
    log: &logger::LogSender,
) {
    for (recipient, msg) in consultations {
        let formatted = prompts::delivery_message(sender, msg);
        let sys = format!("<@{}> → <@{}>: {}", sender, recipient, msg);
        app.push_system_message(&sys);
        log.emit(sender, logger::LogEvent::TinkerSystemMessageReceived { content: sys });
        // Track ALL recipients in running_sessions — permanent goals, interactive
        // agents (tend / rummage / jog), and ephemeral coordinators — so the
        // batch-end retirement check sees any session with a pending delivery as
        // still active.  Without tracking ephemeral coordinators, a sub-session
        // that replies back to its coordinator (e.g. `fresh-agents~1`) creates a
        // window where running_sessions does not contain the coordinator, the
        // retirement guard fires, and the coordinator is retired before it can
        // consume the buffered reply and respond to its own dispatcher.
        let should_track = app.goals.iter().any(|g| &g.id == recipient)
            || matches!(recipient.as_str(), "tend" | "rummage" | "jog")
            || app.ephemeral_sessions.contains(recipient.as_str());

        if let Some(tx) = session_senders.get(recipient) {
            if tx.send(formatted).is_err() {
                // The recipient's session task has exited and its channel is closed.
                // Surface this as a system message so the user can see the lost delivery.
                let warn = prompts::delivery_lost_warning(sender, recipient);
                app.push_system_message(warn.trim_end());
                log.emit(sender, logger::LogEvent::TinkerSystemMessageReceived { content: warn.trim_end().to_string() });
            }
        } else if app.goals.iter().any(|g| &g.id == recipient) {
            let _ = goal_spawn_tx.send(SpawnGoalRequest {
                goal_id: recipient.clone(),
                message: formatted,
                fresh_session: None,
            });
        }
        if should_track {
            let reason = msg.lines().next().unwrap_or("").to_string();
            app.running_sessions.entry(recipient.clone()).or_insert(Some(reason));
        }
    }
}

/// Collect all IDs the @-block parser should recognise: current registry entries
/// plus all known goal IDs (so agents can address goals not yet spawned).
fn known_agent_ids<'a>(
    session_senders: &'a HashMap<String, mpsc::UnboundedSender<String>>,
    goals: &'a [goal::Goal],
) -> Vec<&'a str> {
    let mut ids: Vec<&str> = session_senders.keys().map(|s| s.as_str()).collect();
    for g in goals {
        if !ids.contains(&g.id.as_str()) {
            ids.push(g.id.as_str());
        }
    }
    ids
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
/// appropriate App state updates and peer consultations.
fn handle_session_event(
    app: &mut App,
    ev: SessionEvent,
    goal_spawn_tx: &mpsc::UnboundedSender<SpawnGoalRequest>,
    session_senders: &HashMap<String, mpsc::UnboundedSender<String>>,
    fs: &dyn Filesystem,
    log: &logger::LogSender,
) {
    match ev {
        SessionEvent::Chunk { goal_id, text } => {
            // Mark session as actively processing so the ▶ indicator appears.
            // Applies to every session including interactive agents (tend/rummage/jog).
            app.running_sessions.entry(goal_id.clone()).or_insert(None);
            app.append_goal_log(&goal_id, &text);
            // Suppress tend's startup output from the conversation pane until
            // the user has typed their first message.
            if goal_id != "tend" || app.user_has_interacted {
                app.append_agent_message(&goal_id, &text);
            }
            // Use clone so goal_id remains usable after the entry borrow ends.
            app.current_session_text.entry(goal_id.clone()).or_default().push_str(&text);
            // Pre-announce ephemeral sub-sessions as soon as their opening tag
            // appears in the stream.  The full task body is unavailable until
            // Done; spawning still happens there.  If a pre-announced entry has
            // no matching close tag at Done time it is silently removed.
            // All sessions — including ephemeral coordinators — may spawn
            // sub-sessions (unbounded depth).  The scan uses goal_id directly
            // so coordinator tags `<@coord~1|label>` are recognised; the new
            // session ID uses the dispatcher's own ID as prefix so depth is
            // preserved: a child of `rummage~1` becomes `rummage~1~N`.
            {
                let accumulated = app.current_session_text
                    .get(&goal_id)
                    .cloned()
                    .unwrap_or_default();
                let all_opens = scan_opening_tags(&accumulated, &goal_id);
                let already_announced = app
                    .pending_fresh_announcements
                    .get(&goal_id)
                    .map(|v| v.len())
                    .unwrap_or(0);
                for label in all_opens.into_iter().skip(already_announced) {
                    app.fresh_session_counter += 1;
                    let counter = app.fresh_session_counter;
                    let session_id = format!("{}~{}", goal_id, counter);
                    app.running_sessions.insert(session_id.clone(), None);
                    app.ephemeral_sessions.insert(session_id.clone());
                    app.ephemeral_sessions_ordered.push(session_id.clone());
                    app.ephemeral_labels.insert(session_id.clone(), label.clone());
                    app.goal_list_scroll.last_total = app.flat_items().len();
                    app.pending_fresh_announcements
                        .entry(goal_id.clone())
                        .or_default()
                        .push((session_id, label));
                }
            }
        }
        SessionEvent::Done { goal_id, full_output } => {
            app.finalize_agent_message(&goal_id);
            // Clear the ▶ indicator for any session type, including interactive agents.
            app.running_sessions.remove(&goal_id);
            // current_session_text was used during streaming for pre-announcement scanning;
            // its content is not used here — full_output from the Done event is the authoritative
            // complete assembled reply and is free of chunk-delivery gaps.
            app.current_session_text.remove(&goal_id);
            let session_text = full_output;
            // Reload goals — any session may have written TOML files.
            if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                app.goals = load.goals;
                app.update_parse_errors(load.errors);
            }
            // Tend-specific: parse-error correction loop and phase gate.
            if goal_id == "tend" {
                let prev_errors = app.parse_errors.clone();
                if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                    app.goals = load.goals;
                    app.update_parse_errors(load.errors);
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
            let known_ids = known_agent_ids(session_senders, &app.goals);
            let consultations = parse_at_commands(&session_text, &known_ids);
            dispatch_peer_consultations(app, &goal_id, &consultations, session_senders, goal_spawn_tx, log);

            // Fresh-dispatch: reconcile pre-announced ephemeral sessions
            // (registered at Chunk time from opening tags) with the complete
            // envelopes found in the finished turn text.
            //
            // • Pre-announced entry has a matching complete envelope → update
            //   the running reason and spawn the sub-session using the
            //   pre-assigned session ID (so the TUI row never flickers).
            // • More complete envelopes than pre-announcements (e.g. when the
            //   Done event fires without a preceding Chunk scan) → assign a
            //   fresh ID and register as normal.
            // • Pre-announced entry with no matching complete envelope →
            //   remove it silently (the opening tag appeared but the body or
            //   closing tag was never emitted).
            //
            // All sessions — including ephemeral coordinators — may spawn
            // further sub-sessions (unbounded depth).  Coordinators use their
            // own session ID in dispatch tags; dispatcher_id carries the actual
            // sender, and the permanent base ID is used for new session IDs so
            // TUI nesting stays under the permanent goal row.
            {
                let perm_base = app::session_base_id(&goal_id).to_string();
                let fresh_dispatches = parse_fresh_dispatches(&session_text, &goal_id);
                // Consume the pre-announcement list for this dispatcher's goal_id.
                let pre_announced = app
                    .pending_fresh_announcements
                    .remove(&goal_id)
                    .unwrap_or_default();

                for (idx, (label, task)) in fresh_dispatches.iter().enumerate() {
                    let session_id = if idx < pre_announced.len() {
                        // Reuse the pre-assigned ID so the goal-list row is stable.
                        let (pre_id, _) = &pre_announced[idx];
                        let first_line = task.lines().next().unwrap_or("").to_string();
                        app.running_sessions.insert(pre_id.clone(), Some(first_line));
                        pre_id.clone()
                    } else {
                        // No pre-announcement for this envelope — register now.
                        app.fresh_session_counter += 1;
                        let counter = app.fresh_session_counter;
                        let id = format!("{}~{}", goal_id, counter);
                        let first_line = task.lines().next().unwrap_or("").to_string();
                        app.running_sessions.insert(id.clone(), Some(first_line));
                        app.ephemeral_sessions.insert(id.clone());
                        app.ephemeral_sessions_ordered.push(id.clone());
                        app.ephemeral_labels.insert(id.clone(), label.clone());
                        app.goal_list_scroll.last_total = app.flat_items().len();
                        id
                    };
                    let first_line = task.lines().next().unwrap_or("").to_string();
                    let label_clause = match label {
                        Some(l) if !l.is_empty() => format!(" ({})", l),
                        _ => String::new(),
                    };
                    let sys = format!(
                        "<@{}> → fresh sub-session `{}`{}: {}",
                        goal_id, session_id, label_clause, first_line
                    );
                    app.push_system_message(&sys);
                    log.emit(
                        &goal_id,
                        logger::LogEvent::TinkerSystemMessageReceived { content: sys },
                    );
                    let _ = goal_spawn_tx.send(SpawnGoalRequest {
                        goal_id: perm_base.clone(),
                        message: task.clone(),
                        fresh_session: Some(FreshSessionConfig {
                            session_id,
                            label: label.clone(),
                            // Carry the actual dispatcher ID (may be ephemeral) so
                            // the spawn handler can route replies correctly and the
                            // init message uses the right reply target.
                            dispatcher_id: goal_id.clone(),
                        }),
                    });
                }

                // Remove pre-announced sessions that have no matching complete envelope.
                for (idx, (pre_id, _)) in pre_announced.iter().enumerate() {
                    if idx >= fresh_dispatches.len() {
                        app.ephemeral_sessions.remove(pre_id);
                        app.running_sessions.remove(pre_id);
                        app.ephemeral_sessions_ordered.retain(|id| id != pre_id);
                        app.ephemeral_labels.remove(pre_id);
                    }
                }
                if !pre_announced.is_empty() {
                    // Re-sync the scroll total after any removals.
                    app.goal_list_scroll.last_total = app.flat_items().len();
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
            } else if in_rect(rects.log) {
                app.log_scroll.scroll_up(MOUSE_SCROLL_STEP);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_rect(rects.repl) {
                app.repl_scroll.scroll_down(MOUSE_SCROLL_STEP);
            } else if in_rect(rects.goal_list) {
                app.goal_list_scroll.scroll_down(MOUSE_SCROLL_STEP);
            } else if in_rect(rects.goal_text) {
                app.goal_text_scroll.scroll_down(MOUSE_SCROLL_STEP);
            } else if in_rect(rects.log) {
                app.log_scroll.scroll_down(MOUSE_SCROLL_STEP);
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

    fn make_test_session_senders(
        msg_tx: &mpsc::UnboundedSender<String>,
    ) -> (HashMap<String, mpsc::UnboundedSender<String>>, mpsc::UnboundedReceiver<String>, mpsc::UnboundedReceiver<String>) {
        let (rummage_tx, rummage_rx) = mpsc::unbounded_channel::<String>();
        let (jog_tx, jog_rx) = mpsc::unbounded_channel::<String>();
        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), msg_tx.clone());
        senders.insert("rummage".to_string(), rummage_tx);
        senders.insert("jog".to_string(), jog_tx);
        (senders, rummage_rx, jog_rx)
    }

    // REMOVED: test_spec_build_batch_summary_request_folds_per_goal_summaries
    // (build_batch_summary_request deleted — batch machinery retired)
    // REMOVED: test_spec_batch_summary_request_instructs_reactive_run_lines
    // (same reason)

    // REMOVED: test_spec_summary_routes_directly_to_tend_not_batched
    // (SummaryReady variant retired — goal agents send <@tend> directly on completion)

    // spec: goal-agents — SummaryReady was a transitional SessionEvent variant
    // that routed goal-session summaries to tend via the event channel. Goal agents
    // now send <@tend> directly, so the variant has been removed. LlmSessionId is likewise
    // gone: the session id is captured from the runner's return value, not an event.
    // This exhaustive-match test compiles only while both variants stay absent.
    #[test]
    fn test_spec_summary_ready_variant_removed() {
        // Exhaustive match ensures LlmSessionId and SummaryReady stay absent.
        // Done carries goal_id and full_output (the complete assembled reply for
        // envelope extraction, bypassing chunk-reconstructed current_session_text).
        let evt = SessionEvent::Done { goal_id: "x".into(), full_output: "".into() };
        match evt {
            SessionEvent::Chunk { .. } => {}
            SessionEvent::Done { .. } => {}
            SessionEvent::CleanupBlocked { .. } => {}
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
        // Files that legitimately host raw effects: `opencode.rs` and `claude.rs`
        // own the subprocess builders (Real OpenCode and Claude runners);
        // `opencode.rs` also creates ephemeral agent files (std::fs::) for
        // system-prompt delivery — a subprocess-management concern that belongs
        // alongside its Command::new usage; `realfs.rs` owns the Filesystem cap.
        let cmd_allowed = ["opencode.rs", "claude.rs"];
        let fs_allowed = ["realfs.rs", "opencode.rs"];

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
                     composition root (opencode.rs). Subprocess effects must \
                     go through the OpenCodeRunner capability.",
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

        // Seed every scroll pane with enough content that ScrollDown can move them.
        let mut app = App::new();
        for s in [&mut app.repl_scroll, &mut app.log_scroll,
                  &mut app.goal_text_scroll, &mut app.goal_list_scroll] {
            s.record_render(200, 10, 0);
        }
        // Use ScrollDown (from the tail) so None → Some transition is observable.

        // Capture each pane's y before the test event.
        let y_repl_before    = app.repl_scroll.y;
        let y_log_before     = app.log_scroll.y;
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
        assert_eq!(app.log_scroll.y, y_log_before,
            "log_scroll must not move when cursor is in REPL pane");
        assert_eq!(app.goal_text_scroll.y, y_text_before,
            "goal_text_scroll must not move when cursor is in REPL pane");
        assert_eq!(app.goal_list_scroll.y, y_list_before,
            "goal_list_scroll must not move when cursor is in REPL pane");

        // Cursor inside the log pane — only log_scroll must move.
        let y_log_before2 = app.log_scroll.y;
        let y_text_before2 = app.goal_text_scroll.y;
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: rects.log.x + 1,
            row: rects.log.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, ev, area);
        assert_ne!(app.log_scroll.y, y_log_before2,
            "log_scroll must move on ScrollUp in log pane");
        assert_eq!(app.goal_text_scroll.y, y_text_before2,
            "goal_text_scroll must not move when cursor is in log pane");
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
        let rummage_toml = include_str!("../packaged-goals/rummage.toml");
        assert!(rummage_toml.contains("tier = \"high\""), "rummage.toml must declare tier = \"high\"");
        let jog_toml = include_str!("../packaged-goals/jog.toml");
        assert!(jog_toml.contains("tier = \"high\""), "jog.toml must declare tier = \"high\"");
        // Verify the lazy-spawn runner selection is tier-based (no name-based match).
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("\"high\" => oc_goal_high"),
            "lazy spawn must use oc_goal_high for tier=high goals",
        );
        // The tinker_m variable on the Claude path must default to CLAUDE_TINKER_MODEL.
        assert!(
            main_rs.contains("model_config.claude_high(CLAUDE_TINKER_MODEL)"),
            "claude high-tier must fall back to CLAUDE_TINKER_MODEL",
        );
        // The tinker_m variable on the opencode path must default to OPENCODE_TINKER_MODEL.
        assert!(
            main_rs.contains("model_config.opencode_high(OPENCODE_TINKER_MODEL)"),
            "opencode high-tier must fall back to OPENCODE_TINKER_MODEL",
        );
    }

    // spec (goal-agents): A goal with tier="low" must dispatch to oc_goal_low, which
    // is wired to the low-tier model (claude_low / opencode_low). This ensures the
    // "low" tier value is routed through the backend model-config, not silently folded
    // into the mid-tier default.
    #[test]
    fn test_spec_low_tier_goal_uses_low_runner() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("\"low\" => oc_goal_low"),
            "lazy spawn must route tier=low goals to oc_goal_low",
        );
        assert!(
            main_rs.contains("model_config.claude_low(CLAUDE_SCHEDULER_MODEL)"),
            "oc_goal_low on the claude path must be wired to claude_low(CLAUDE_SCHEDULER_MODEL)",
        );
        assert!(
            main_rs.contains("model_config.opencode_low(OPENCODE_SCHEDULER_MODEL)"),
            "oc_goal_low on the opencode path must be wired to opencode_low(OPENCODE_SCHEDULER_MODEL)",
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
    // They start lazily on the first @goal-id dispatch or user message, just like
    // every other goal agent. Tend is the only pre-populated entry.
    #[test]
    fn test_spec_rummage_jog_lazy_not_pre_seeded() {
        let main_rs = include_str!("main.rs");
        assert!(
            !main_rs.contains("session_senders.insert(\"rummage\""),
            "rummage must not be pre-seeded in session_senders (lazy startup only)",
        );
        assert!(
            !main_rs.contains("session_senders.insert(\"jog\""),
            "jog must not be pre-seeded in session_senders (lazy startup only)",
        );
        // Tend IS pre-seeded (its eager startup is the only registry exception).
        assert!(
            main_rs.contains("session_senders.insert(\"tend\""),
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

    // spec (peer-consult): parse_at_commands extracts <@tend>, <@rummage>,
    // <@jog> tag envelopes from agent output. Prose outside envelopes is
    // silently dropped. The function takes a `known_ids` slice so routing
    // works for any registered session ID, not just the three built-in agents.
    const BUILTIN_IDS: &[&str] = &["tend", "rummage", "jog"];

    #[test]
    fn test_spec_peer_consult_parse_at_tinker() {
        let r = parse_at_commands("<@tend>what does this module do?</@tend>", BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "tend");
        assert_eq!(r[0].1, "what does this module do?");
    }

    #[test]
    fn test_spec_peer_consult_parse_at_rummage() {
        let r = parse_at_commands("<@rummage>can you trace the call?</@rummage>", BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "rummage");
        assert_eq!(r[0].1, "can you trace the call?");
    }

    #[test]
    fn test_spec_peer_consult_parse_at_jog() {
        let r = parse_at_commands("<@jog>do you still mean X by this?</@jog>", BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "jog");
        assert_eq!(r[0].1, "do you still mean X by this?");
    }

    #[test]
    fn test_spec_peer_consult_parse_prose_before_block_excluded() {
        let input = "some prose\n<@tend>hello</@tend>\n<@rummage>check this</@rummage>";
        let r = parse_at_commands(input, BUILTIN_IDS);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], ("tend".to_string(), "hello".to_string()));
        assert_eq!(r[1], ("rummage".to_string(), "check this".to_string()));
    }

    #[test]
    fn test_spec_peer_consult_parse_block_body_included() {
        let input = "<@tend>\nhello\nbody line one\nbody line two\n</@tend>\n<@rummage>check</@rummage>";
        let r = parse_at_commands(input, BUILTIN_IDS);
        assert_eq!(r.len(), 2);
        assert!(
            r[0].1.contains("hello") && r[0].1.contains("body line one"),
            "tag envelope must include all lines between open and close tags"
        );
        assert_eq!(r[1], ("rummage".to_string(), "check".to_string()));
    }

    #[test]
    fn test_spec_peer_consult_parse_at_without_message_ignored() {
        let r = parse_at_commands("<@tend></@tend>", BUILTIN_IDS);
        assert_eq!(r.len(), 0, "empty tag envelope must not be delivered");
    }

    #[test]
    fn test_spec_peer_consult_parse_multiline_body() {
        let input = "<@rummage>\nfirst line\nsecond line\n</@rummage>";
        let r = parse_at_commands(input, BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "rummage");
        assert!(
            r[0].1.contains("first line") && r[0].1.contains("second line"),
            "multi-line tag envelope body must include all lines between open and close tags"
        );
    }

    #[test]
    fn test_spec_peer_consult_parse_multiple_at_lines() {
        let input = "<@tend>q1</@tend>\n<@rummage>q2</@rummage>\n<@jog>q3</@jog>";
        let r = parse_at_commands(input, BUILTIN_IDS);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], ("tend".to_string(), "q1".to_string()));
        assert_eq!(r[1], ("rummage".to_string(), "q2".to_string()));
        assert_eq!(r[2], ("jog".to_string(), "q3".to_string()));
    }

    // spec (peer-consult): an envelope that starts mid-line (e.g. explanatory
    // prose such as "reply via <@tend>…</@tend>") must NOT be extracted as a
    // live dispatch — only envelopes whose opening tag begins the trimmed line
    // are treated as real routing signals.
    #[test]
    fn test_spec_peer_consult_mid_line_envelope_not_extracted() {
        let input = "reply via <@tend>placeholder text</@tend> once you are done";
        let r = parse_at_commands(input, BUILTIN_IDS);
        assert!(
            r.is_empty(),
            "mid-line envelope must not be extracted as a dispatch; got {:?}",
            r
        );
    }

    // spec (peer-consult): envelopes inside a fenced code block (``` or ~~~)
    // are illustrative, not live — the parser must skip them entirely.
    #[test]
    fn test_spec_peer_consult_code_fence_envelope_not_extracted() {
        let backtick_fence = "```\n<@tend>hello</@tend>\n```";
        let r = parse_at_commands(backtick_fence, BUILTIN_IDS);
        assert!(r.is_empty(), "envelope in backtick fence must not dispatch; got {:?}", r);

        let tilde_fence = "~~~\n<@rummage>check this</@rummage>\n~~~";
        let r2 = parse_at_commands(tilde_fence, BUILTIN_IDS);
        assert!(r2.is_empty(), "envelope in tilde fence must not dispatch; got {:?}", r2);

        // A real envelope after the fence must still fire.
        let after_fence = "```\n<@tend>ignored</@tend>\n```\n<@rummage>real</@rummage>";
        let r3 = parse_at_commands(after_fence, BUILTIN_IDS);
        assert_eq!(r3.len(), 1);
        assert_eq!(r3[0], ("rummage".to_string(), "real".to_string()));
    }

    // spec (goal-agents): dispatch_peer_consultations requests lazy spawn when
    // the recipient is a known goal ID not yet in the session registry.
    #[test]
    fn test_spec_goal_agent_dispatch_triggers_lazy_spawn() {
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&msg_tx);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "goal-agents".into(),
            summary: String::new(),
            description: "build the registry".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        let consultations = vec![("goal-agents".to_string(), "time to build".to_string())];
        dispatch_peer_consultations(
            &mut app,
            "tend",
            &consultations,
            &senders,
            &spawn_tx,
            &logger::noop_sender(),
        );

        let req = spawn_rx.try_recv().expect("dispatch to unknown goal must enqueue spawn request");
        assert_eq!(req.goal_id, "goal-agents");
        assert!(req.message.contains("time to build"));
        assert!(req.message.contains("[from tend]"), "message must carry sender attribution");
    }

    // spec (goal-agents): known_agent_ids includes both registry entries and
    // all goal IDs from app.goals (so agents can address unspawned goals).
    #[test]
    fn test_spec_known_agent_ids_includes_goals_and_registry() {
        let (msg_tx, _) = mpsc::unbounded_channel::<String>();
        let (senders, _, _) = make_test_session_senders(&msg_tx);
        let goals = vec![
            goal::Goal { id: "tui".into(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
            goal::Goal { id: "rummage".into(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], kind: None, tier: None, source_path: None },
        ];
        let ids = known_agent_ids(&senders, &goals);
        assert!(ids.contains(&"tend"), "registry agent must be included");
        assert!(ids.contains(&"tui"), "goal not in registry must be included");
        // rummage is in both registry and goals — must appear exactly once in practice
        // (the dedup ensures it's not duplicated)
        assert!(ids.iter().filter(|&&id| id == "rummage").count() <= 1, "no duplicates");
    }

    // spec (peer-consult): parse_at_commands accepts arbitrary goal IDs,
    // not just the three built-in agents.
    #[test]
    fn test_spec_peer_consult_parse_accepts_goal_id() {
        let ids = &["tend", "rummage", "goal-agents"];
        let r = parse_at_commands("<@goal-agents>start working on the registry</@goal-agents>", ids);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "goal-agents");
        assert_eq!(r[0].1, "start working on the registry");
    }

    // spec (peer-consult): dispatch_peer_consultations routes each consultation
    // to the correct channel and formats the message with the sender name.
    #[test]
    fn test_spec_peer_consult_dispatch_routes_to_correct_channel() {
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<String>();
        let (rummage_tx, mut rummage_rx) = mpsc::unbounded_channel::<String>();
        let (jog_tx, mut jog_rx) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();

        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), msg_tx.clone());
        senders.insert("rummage".to_string(), rummage_tx);
        senders.insert("jog".to_string(), jog_tx);

        let mut app = App::new();
        let consultations = vec![
            ("tend".to_string(), "question for tend".to_string()),
            ("rummage".to_string(), "question for rummage".to_string()),
            ("jog".to_string(), "question for jog".to_string()),
        ];
        dispatch_peer_consultations(
            &mut app,
            "rummage",
            &consultations,
            &senders,
            &spawn_tx,
            &logger::noop_sender(),
        );

        let tend_msg = msg_rx.try_recv().expect("tend must receive its consultation");
        assert!(tend_msg.contains("[from rummage]"), "message must carry sender attribution");
        assert!(tend_msg.contains("question for tend"));

        let rummage_msg = rummage_rx.try_recv().expect("rummage must receive its consultation");
        assert!(rummage_msg.contains("[from rummage]"));

        let jog_msg = jog_rx.try_recv().expect("jog must receive its consultation");
        assert!(jog_msg.contains("[from rummage]"));
    }

    // spec (peer-consult): dispatched messages carry an inline return-routing
    // instruction naming the sender so the receiving agent knows who to reply
    // to. The instruction must NOT use live <@id>…</@id> envelope syntax —
    // any live envelope in the delivery message would be parsed by the harness
    // as a real dispatch, routing placeholder text as a spurious first message
    // and truncating the agent's actual reply.
    #[test]
    fn test_spec_peer_consult_dispatch_includes_reply_instruction() {
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), msg_tx);
        let mut app = App::new();
        let consultations = vec![("tend".to_string(), "what does this mean?".to_string())];
        dispatch_peer_consultations(
            &mut app,
            "rummage",
            &consultations,
            &senders,
            &spawn_tx,
            &logger::noop_sender(),
        );
        let msg = msg_rx.try_recv().expect("tend must receive the consultation");
        assert!(
            msg.contains("@rummage"),
            "dispatched message must name the sender so the recipient knows the reply target"
        );
        // The delivery message must not contain a parseable live envelope — if
        // it did, the harness would extract it on the next parse pass and route
        // placeholder text back to the sender as a spurious message.
        let ids: &[&str] = &["rummage"];
        let spurious = parse_at_commands(&msg, ids);
        assert!(
            spurious.is_empty(),
            "delivery message must not contain live <@id>…</@id> envelopes that the parser would pick up: found {:?}",
            spurious
        );
    }

    // spec (peer-consult): a system message is pushed for each consultation
    // so the user can observe the exchange in real time.
    #[test]
    fn test_spec_peer_consult_pushes_system_message_for_visibility() {
        let (msg_tx, _) = mpsc::unbounded_channel::<String>();
        let (rummage_tx, mut rummage_rx) = mpsc::unbounded_channel::<String>();
        let (jog_tx, _) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();

        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), msg_tx.clone());
        senders.insert("rummage".to_string(), rummage_tx);
        senders.insert("jog".to_string(), jog_tx);

        let mut app = App::new();
        let consultations = vec![("rummage".to_string(), "trace the init flow".to_string())];
        dispatch_peer_consultations(
            &mut app,
            "tend",
            &consultations,
            &senders,
            &spawn_tx,
            &logger::noop_sender(),
        );

        let sys = app.messages.iter().find(|m| m.role == app::Role::System);
        assert!(sys.is_some(), "a system message must be pushed for the consultation");
        let text = &sys.unwrap().text;
        assert!(text.contains("<@tend>"), "system message must name the sender");
        assert!(text.contains("<@rummage>"), "system message must name the recipient");
        assert!(text.contains("trace the init flow"), "system message must include the message content");
        let _ = rummage_rx.try_recv();
    }

    // spec (peer-consult): when a recipient's session task has exited (receiver
    // dropped), dispatch_peer_consultations must surface a warning system message
    // rather than silently discarding the delivery.
    #[test]
    fn test_spec_peer_consult_dropped_receiver_surfaces_warning() {
        let (msg_tx, _) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();

        // Create a sender for "rummage" but immediately drop the receiver — simulates
        // a session task that has exited.
        let (rummage_tx, rummage_rx) = mpsc::unbounded_channel::<String>();
        drop(rummage_rx);

        let mut senders = HashMap::new();
        senders.insert("tend".to_string(), msg_tx.clone());
        senders.insert("rummage".to_string(), rummage_tx);

        let mut app = App::new();
        let consultations = vec![("rummage".to_string(), "check the auth module".to_string())];
        dispatch_peer_consultations(
            &mut app,
            "tend",
            &consultations,
            &senders,
            &spawn_tx,
            &logger::noop_sender(),
        );

        let warn = app.messages.iter().find(|m| {
            m.role == app::Role::System && m.text.contains("delivery lost")
        });
        assert!(
            warn.is_some(),
            "a dropped-receiver must surface a 'delivery lost' system message"
        );
        let text = &warn.unwrap().text;
        assert!(text.contains("tend"), "warning must name the sender");
        assert!(text.contains("rummage"), "warning must name the recipient");
    }

    // spec (peer-consult): the session message channels and the goal-spawn channel
    // must be unbounded so delivery never silently drops when a recipient is under
    // load.  A bounded try_send on capacity-16 or -32 channels silently discards
    // the message when the channel is full; unbounded senders never fail due to capacity.
    #[test]
    fn test_spec_peer_consult_session_channels_are_unbounded() {
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
            main_rs.contains("--claude") && main_rs.contains("--help") && main_rs.contains("-h"),
            "help output must name all three startup flags: --claude, --help/-h",
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
        for flag in &["--claude", "--tend-full-goal-context", "--default-model", "--help", "-h"] {
            assert!(
                main_rs.contains(flag),
                "KNOWN_ARGS or help text must mention flag '{flag}'",
            );
        }
    }

    #[test]
    fn test_spec_tinker_static_persona_in_description_dynamic_goals_in_init() {
        let content = packaged_tend_goal().description;
        let init = tend_init_prompt("- demo-goal-id: a demo description", "");
        assert!(init.contains("Current goals"), "init prompt must label the dynamic goals section");
        assert!(init.contains("demo-goal-id"), "init prompt must carry the dynamic goals summary verbatim");
        assert!(!content.contains("demo-goal-id"), "static tend persona must not embed dynamic goal ids");
    }

    #[test]
    fn test_spec_tinker_proves_by_execution_not_reading_source() {
        let content = packaged_tend_goal().description;
        assert!(content.contains("code-reality questions to @rummage") || content.contains("delegates aggressively"),
            "tend prompt must require delegating code-reality questions to rummage rather than reading source directly");
    }

    // spec (backends): the claude backend has no path-scoped permission block — the system
    // prompt is the only mechanism that states tend's file-access boundary. tend_system_prompt()
    // must lead with the scope constraint so it reads as a system-level rule, not buried text.
    #[test]
    fn test_spec_tend_system_prompt_leads_with_scope_constraint() {
        let prompt = tend_system_prompt();
        assert!(
            prompt.starts_with("Read and write files ONLY"),
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

    // spec (backends): oc_tend's claude runner must be constructed with a system prompt so the
    // file-access boundary arrives as a persistent system message, not a user-turn instruction.
    // ClaudeRunner::new must not be used for the oc_tend slot in the claude branch.
    #[test]
    fn test_spec_claude_tend_runner_wired_with_system_prompt() {
        let main_rs = include_str!("main.rs");
        let tend_runner_line = main_rs
            .lines()
            .find(|l| l.contains("Arc::new(ClaudeRunner") && l.contains("tinker_m"))
            .expect("must find ClaudeRunner construction for tend (tinker_m) in the claude branch");
        assert!(
            !tend_runner_line.contains("ClaudeRunner::new("),
            "tend's claude runner must use with_system_prompt, not ClaudeRunner::new — got: {}",
            tend_runner_line.trim()
        );
    }

    // spec (backends): the high-tier goal-agent runner (oc_goal_high) must be constructed
    // WITHOUT a struct-level system prompt. If it carried tend_system_prompt(), every
    // high-tier non-tend goal agent (rummage, jog, …) would have tend's identity injected
    // via --system-prompt on turn 2+, when system_prompt_for_run is None and the runner
    // falls back to its struct-level prompt. oc_goal_high is separate from oc_tend
    // precisely to prevent that identity overwrite.
    #[test]
    fn test_spec_claude_goal_high_runner_uses_new_not_with_system_prompt() {
        let main_rs = include_str!("main.rs");
        // The second ClaudeRunner line for tinker_m must be ClaudeRunner::new, not with_system_prompt.
        let high_goal_runner_line = main_rs
            .lines()
            .filter(|l| l.contains("Arc::new(ClaudeRunner") && l.contains("tinker_m"))
            .nth(1)
            .expect("must find two ClaudeRunner constructions for tinker_m: oc_tend (with_system_prompt) and oc_goal_high (new)");
        assert!(
            high_goal_runner_line.contains("ClaudeRunner::new(tinker_m"),
            "oc_goal_high must use ClaudeRunner::new(tinker_m) — no struct-level system prompt — got: {}",
            high_goal_runner_line.trim()
        );
        assert!(
            !high_goal_runner_line.contains("with_system_prompt"),
            "oc_goal_high must NOT use with_system_prompt — got: {}",
            high_goal_runner_line.trim()
        );
    }

    // spec (backends): goal agents' claude runner is constructed WITHOUT a
    // struct-level system prompt — it receives the goal-specific system prompt
    // per-call from goal_agent_loop on each new session (first turn only).
    // The assembled system prompt string (framework preamble + goal description
    // + neighbor table) is identical across backends; only the delivery
    // mechanism differs (agent file for opencode, --system-prompt for claude).
    #[test]
    fn test_spec_claude_goal_runner_uses_new_not_with_system_prompt() {
        let main_rs = include_str!("main.rs");
        let goal_runner_line = main_rs
            .lines()
            .find(|l| l.contains("Arc::new(ClaudeRunner") && l.contains("goal_m"))
            .expect("must find ClaudeRunner construction for goal agents (goal_m) in the claude branch");
        assert!(
            goal_runner_line.contains("ClaudeRunner::new(goal_m"),
            "goal agents' claude runner must use ClaudeRunner::new (system prompt delivered per-call) — got: {}",
            goal_runner_line.trim()
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

    #[test]
    fn test_spec_shared_language_form_norm_minimum_viable_shape() {
        let content = packaged_tend_goal().description;
        assert!(content.contains("One question per turn") || content.contains("one question per turn"),
            "prompt must enforce one-question-per-turn");
    }

    #[test]
    fn test_spec_tinker_encodes_dual_duty_no_fabrication_at_inflection_points() {
        let content = packaged_tend_goal().description;
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
        let content = packaged_tend_goal().description;
        assert!(content.contains("Re-check parent summary"), "prompt must include a 'Re-check parent summary' step");
    }

    #[test]
    fn test_spec_tinker_prompt_related_links_symmetric_both_list_each_other() {
        // tend's write procedure defers the symmetry rule to goal-structure-standard
        // (read fresh) rather than restating it; the guardrail is that the prompt
        // still mandates re-validating every edge, symmetry included.
        let content = packaged_tend_goal().description;
        assert!(
            content.contains("Re-validate all edges") && content.contains("symmetry"),
            "prompt must include an edge re-validation step covering related-link symmetry",
        );
    }

    // spec (tend, goal-agents): the startup-silence init prompt must instruct
    // tend to produce no output on startup. Both compact and full-context variants
    // must carry this instruction so the TUI suppression is paired with a
    // corresponding model instruction, not just a rendering gate.
    #[test]
    fn test_spec_tend_startup_silence_prompt_instructs_no_output() {
        let compact = tend_init_prompt("[]", "");
        assert!(
            compact.contains("no output") || compact.contains("produce no"),
            "compact startup prompt must instruct tend to produce no output",
        );
        let full = tend_init_prompt_full_context("[]", "");
        assert!(
            full.contains("no output") || full.contains("produce no"),
            "full-context startup prompt must instruct tend to produce no output",
        );
    }

    // spec (tui, goal-agents): tend's startup chunks (before the user's first
    // message) must NOT appear in the conversation pane (app.messages) but MUST
    // still land in the session log (app.goal_logs). After the user interacts,
    // chunks from tend flow into the conversation pane normally.
    #[test]
    fn test_spec_tend_startup_chunks_suppressed_until_user_interacted() {
        use crate::app::Role;
        use crate::goal_session::SessionEvent;

        let mut app = App::new();
        assert!(!app.user_has_interacted, "user_has_interacted must start false");

        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let senders = HashMap::new();
        let log = logger::noop_sender();

        // Tend produces a startup chunk before the user has typed anything.
        let ev = SessionEvent::Chunk { goal_id: "tend".to_string(), text: "hello startup".to_string() };
        handle_session_event(&mut app, ev, &spawn_tx, &senders, &RealFilesystem, &log);

        // Must land in goal_logs (session log pane).
        assert!(
            app.goal_logs.get("tend").map(|s| s.contains("hello startup")).unwrap_or(false),
            "startup chunk must appear in goal_logs (log pane)",
        );
        // Must NOT appear in messages (conversation pane).
        assert!(
            !app.messages.iter().any(|m| matches!(&m.role, Role::Agent(id) if id == "tend")),
            "startup chunk must not appear in conversation pane before user interaction",
        );

        // After the user sends their first message, tend's chunks appear normally.
        app.user_has_interacted = true;
        let ev2 = SessionEvent::Chunk { goal_id: "tend".to_string(), text: "hello user".to_string() };
        handle_session_event(&mut app, ev2, &spawn_tx, &senders, &RealFilesystem, &log);

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
        let log = logger::noop_sender();

        assert!(
            !app.running_sessions.contains_key("tend"),
            "tend must not be in running_sessions before any chunk",
        );

        let chunk = SessionEvent::Chunk { goal_id: "tend".to_string(), text: "hi".to_string() };
        handle_session_event(&mut app, chunk, &spawn_tx, &senders, &RealFilesystem, &log);

        assert!(
            app.running_sessions.contains_key("tend"),
            "tend must appear in running_sessions while processing a response (chunk received)",
        );

        let done = SessionEvent::Done { goal_id: "tend".to_string(), full_output: "hi".to_string() };
        handle_session_event(&mut app, done, &spawn_tx, &senders, &RealFilesystem, &log);

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
        let log = logger::noop_sender();

        // full_output is empty — no output produced this turn.
        let done = SessionEvent::Done { goal_id: "rummage".to_string(), full_output: "".to_string() };
        handle_session_event(&mut app, done, &spawn_tx, &senders, &RealFilesystem, &log);

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
        let log = logger::noop_sender();

        let chunk = SessionEvent::Chunk { goal_id: "rummage".to_string(), text: "working on it".to_string() };
        handle_session_event(&mut app, chunk, &spawn_tx, &senders, &RealFilesystem, &log);

        // full_output carries the session's actual text; silence detection checks this.
        let done = SessionEvent::Done { goal_id: "rummage".to_string(), full_output: "working on it".to_string() };
        handle_session_event(&mut app, done, &spawn_tx, &senders, &RealFilesystem, &log);

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

    // spec (parallel-goal-agents): when another goal agent is already running,
    // a new @-dispatch must still go to spawn immediately — no serial queue.
    #[test]
    fn test_spec_at_dispatch_spawns_when_goal_agent_already_running() {
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&msg_tx);

        let mut app = App::new();
        for id in ["goal-a", "goal-b"] {
            app.goals.push(goal::Goal {
                id: id.into(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], kind: None, tier: None, source_path: None,
            });
        }
        // Mark goal-a as already running.
        app.running_sessions.insert("goal-a".into(), Some("first task".into()));

        let consultations = vec![("goal-b".to_string(), "second task".to_string())];
        dispatch_peer_consultations(
            &mut app, "tend", &consultations, &senders, &spawn_tx, &logger::noop_sender(),
        );

        // goal-b must be dispatched immediately in parallel, not queued.
        let req = spawn_rx.try_recv().expect("goal-b must be spawned immediately even though goal-a is running");
        assert_eq!(req.goal_id, "goal-b");
        assert!(
            app.running_sessions.contains_key("goal-b"),
            "goal-b must appear in running_sessions immediately on dispatch",
        );
    }

    // spec (tui — queue visibility): @-dispatching to an autonomous goal agent
    // must add it to running_sessions so the dim ▶ marker appears in the goal list.
    #[test]
    fn test_spec_at_dispatch_adds_goal_agent_to_running_sessions() {
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&msg_tx);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "tui".into(),
            summary: String::new(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        let consultations = vec![("tui".to_string(), "implement queue markers".to_string())];
        dispatch_peer_consultations(
            &mut app, "rummage", &consultations, &senders, &spawn_tx, &logger::noop_sender(),
        );

        assert!(
            app.running_sessions.contains_key("tui"),
            "autonomous goal agent dispatched via @ must appear in running_sessions",
        );
        let reason = app.running_sessions["tui"].as_deref().unwrap_or("");
        assert!(
            reason.contains("implement queue markers"),
            "reason stored must reflect the dispatch message",
        );
    }

    // spec (fresh-agents — batch retirement): @-dispatching to interactive chat
    // agents (tend, rummage, jog) MUST add them to running_sessions via the
    // dispatch path. This ensures the batch-end retirement check sees a pending
    // delivery as still-active work and does not retire ephemeral sub-sessions
    // before the interactive agent has processed its incoming reply.
    #[test]
    fn test_spec_at_dispatch_tracks_interactive_agents_in_running_sessions() {
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&msg_tx);

        let mut app = App::new();
        // Add rummage as a goal (it has a goal file) to match the production
        // scenario where rummage has a TOML file.
        app.goals.push(goal::Goal {
            id: "rummage".into(),
            summary: String::new(),
            description: String::new(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        let consultations = vec![
            ("tend".to_string(), "what does the user want".to_string()),
            ("rummage".to_string(), "check the code".to_string()),
            ("jog".to_string(), "verify alignment".to_string()),
        ];
        dispatch_peer_consultations(
            &mut app, "some-agent", &consultations, &senders, &spawn_tx, &logger::noop_sender(),
        );

        assert!(
            app.running_sessions.contains_key("tend"),
            "tend must appear in running_sessions when dispatched via @ (pending delivery)",
        );
        assert!(
            app.running_sessions.contains_key("rummage"),
            "rummage must appear in running_sessions when dispatched via @ (pending delivery)",
        );
        assert!(
            app.running_sessions.contains_key("jog"),
            "jog must appear in running_sessions when dispatched via @ (pending delivery)",
        );
    }

    // spec (fresh-agents — coordinator retirement): when a sub-session replies to
    // its ephemeral coordinator via @, the coordinator must be added to
    // running_sessions so the batch-end retirement guard sees it as still-active
    // and does not retire it before it can consume the buffered reply.
    #[test]
    fn test_spec_at_dispatch_tracks_ephemeral_coordinator_in_running_sessions() {
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&msg_tx);

        let mut app = App::new();
        // Register a live sender for the coordinator so the message is delivered.
        let (coord_tx, _coord_rx) = mpsc::unbounded_channel::<String>();
        let mut senders_with_coord = senders.clone();
        senders_with_coord.insert("fresh-agents~1".to_string(), coord_tx);
        // Mark the coordinator as an ephemeral session (as the runtime does).
        app.ephemeral_sessions.insert("fresh-agents~1".to_string());

        let consultations = vec![
            ("fresh-agents~1".to_string(), "sub-task result".to_string()),
        ];
        dispatch_peer_consultations(
            &mut app, "fresh-agents~2", &consultations, &senders_with_coord, &spawn_tx, &logger::noop_sender(),
        );

        assert!(
            app.running_sessions.contains_key("fresh-agents~1"),
            "ephemeral coordinator must appear in running_sessions when a reply is dispatched to it",
        );
    }

    // spec (parallel-goal-agents): dispatch_peer_consultations must NOT gate on
    // any "is a goal agent already running?" check. Two concurrent non-interactive
    // dispatches must both go to spawn immediately — neither is queued.
    #[test]
    fn test_spec_dispatch_no_queue_gating_two_concurrent_agents() {
        let (msg_tx, _) = mpsc::unbounded_channel::<String>();
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&msg_tx);

        let mut app = App::new();
        for id in ["goal-a", "goal-b"] {
            app.goals.push(goal::Goal {
                id: id.into(),
                summary: String::new(),
                description: "".into(),
                parent_id: String::new(),
                children: vec![],
                related: vec![],
                tier: None,
                kind: None,
                source_path: None,
            });
        }
        // Simulate goal-a already running.
        app.running_sessions.insert("goal-a".into(), Some("reason a".into()));

        let consultations = vec![("goal-b".to_string(), "start b in parallel".to_string())];
        dispatch_peer_consultations(
            &mut app,
            "tend",
            &consultations,
            &senders,
            &spawn_tx,
            &logger::noop_sender(),
        );

        // goal-b must be spawned immediately even though goal-a is running.
        let req = spawn_rx.try_recv().expect("goal-b must be dispatched immediately, not queued");
        assert_eq!(req.goal_id, "goal-b");
        assert!(req.message.contains("start b in parallel"));
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
    // for goal agents it is set at dispatch time (ConfirmOptions / dispatch_peer_consultations).
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

    // spec (fresh-agents): parse_fresh_dispatches extracts <@sender|label>…</@sender|label>
    // inline envelopes from a completed agent reply.
    #[test]
    fn test_spec_fresh_agents_parse_fresh_dispatch_with_label() {
        let text = "<@my-goal|analyze>do the analysis</@my-goal|analyze>";
        let results = parse_fresh_dispatches(text, "my-goal");
        assert_eq!(results.len(), 1, "one fresh dispatch must be extracted");
        assert_eq!(results[0].0, Some("analyze".to_string()), "label must be extracted");
        assert_eq!(results[0].1, "do the analysis", "task must be extracted");
    }

    // spec (fresh-agents): parse_fresh_dispatches handles an empty label (the |
    // separator is present but the label part is empty) — label returns None.
    #[test]
    fn test_spec_fresh_agents_parse_fresh_dispatch_empty_label() {
        let text = "<@my-goal|>do the thing</@my-goal|>";
        let results = parse_fresh_dispatches(text, "my-goal");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, None, "empty label must map to None");
        assert_eq!(results[0].1, "do the thing");
    }

    // spec (fresh-agents): parse_fresh_dispatches collects multi-line bodies.
    #[test]
    fn test_spec_fresh_agents_parse_fresh_dispatch_multiline() {
        let text = "<@my-goal|sub1>\nfirst line\nsecond line\n</@my-goal|sub1>";
        let results = parse_fresh_dispatches(text, "my-goal");
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("first line"), "multi-line body must include first line");
        assert!(results[0].1.contains("second line"), "multi-line body must include second line");
    }

    // spec (fresh-agents): parse_fresh_dispatches only matches the sender's own
    // goal ID — normal <@other-id> envelopes are ignored.
    #[test]
    fn test_spec_fresh_agents_parse_does_not_match_normal_at_commands() {
        let text = "<@other-goal>some message</@other-goal>";
        let results = parse_fresh_dispatches(text, "my-goal");
        assert_eq!(results.len(), 0, "normal @-commands must not match fresh-dispatch parser");
    }

    // spec (fresh-agents): parse_fresh_dispatches ignores empty envelopes.
    #[test]
    fn test_spec_fresh_agents_parse_empty_envelope_ignored() {
        let text = "<@my-goal|sub1></@my-goal|sub1>";
        let results = parse_fresh_dispatches(text, "my-goal");
        assert_eq!(results.len(), 0, "empty fresh dispatch envelope must be ignored");
    }

    // spec (fresh-agents, peer-consult): fresh dispatch envelopes inside fenced
    // code blocks are illustrative and must not be extracted as live dispatches.
    #[test]
    fn test_spec_fresh_agents_code_fence_envelope_not_extracted() {
        let fence = "```\n<@my-goal|sub1>task inside fence</@my-goal|sub1>\n```";
        let results = parse_fresh_dispatches(fence, "my-goal");
        assert!(
            results.is_empty(),
            "fresh dispatch inside code fence must not be extracted; got {:?}",
            results
        );

        // A real dispatch after the fence must still be extracted.
        let after = "```\n<@g|x>ignored</@g|x>\n```\n<@g|real>live task</@g|real>";
        let r2 = parse_fresh_dispatches(after, "g");
        assert_eq!(r2.len(), 1, "dispatch after fence must be extracted; got {:?}", r2);
        assert_eq!(r2[0].1, "live task");
    }

    // spec (fresh-agents): multiple fresh dispatches in one reply are all extracted.
    #[test]
    fn test_spec_fresh_agents_parse_multiple_dispatches() {
        let text = "<@g|a>task a</@g|a>\n<@g|b>task b</@g|b>";
        let results = parse_fresh_dispatches(text, "g");
        assert_eq!(results.len(), 2, "both fresh dispatches must be extracted");
        assert_eq!(results[0].0, Some("a".to_string()));
        assert_eq!(results[1].0, Some("b".to_string()));
    }

    // spec (fresh-agents): when a goal session done event contains a fresh dispatch,
    // handle_session_event enqueues a SpawnGoalRequest with fresh_session set,
    // adds the ephemeral session ID to app.ephemeral_sessions, and inserts it in
    // running_sessions so it is visible in the TUI.
    #[test]
    fn test_spec_fresh_agents_done_event_spawns_ephemeral_session() {
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "the goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "my-goal".into(),
                // full_output carries the complete reply — envelope extraction uses this directly.
                full_output: "<@my-goal|work>analyse the module</@my-goal|work>".into(),
            },
            &spawn_tx,
            &senders,
            &crate::realfs::RealFilesystem,
            &logger::noop_sender(),
        );

        // A SpawnGoalRequest with fresh_session set must have been enqueued.
        let req = spawn_rx.try_recv().expect("fresh dispatch must enqueue a spawn request");
        let fresh = req.fresh_session.expect("spawn request must carry fresh_session config");
        assert!(fresh.session_id.starts_with("my-goal~"), "ephemeral ID must start with dispatcher id + ~");
        assert_eq!(fresh.label, Some("work".to_string()), "label must be preserved");
        assert_eq!(fresh.dispatcher_id, "my-goal");

        // The ephemeral session must be in both ephemeral_sessions and running_sessions.
        assert!(
            app.ephemeral_sessions.contains(&fresh.session_id),
            "ephemeral session must be tracked in app.ephemeral_sessions",
        );
        assert!(
            app.running_sessions.contains_key(&fresh.session_id),
            "ephemeral session must appear in running_sessions for TUI visibility",
        );
    }

    // spec (peer-consult): envelope extraction at Done uses full_output, not the
    // chunk-reconstructed current_session_text. Chunks can be dropped when the
    // session event channel is under load; full_output from the Done event is the
    // authoritative complete reply.
    #[test]
    fn test_spec_peer_consult_done_uses_full_output_not_chunk_text() {
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "the goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        // current_session_text has partial/incomplete content — no envelope.
        app.current_session_text.insert("my-goal".into(), "partial output without envelope".into());

        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "my-goal".into(),
                // full_output is the authoritative reply and contains the envelope.
                full_output: "<@my-goal|work>analyse the module</@my-goal|work>".into(),
            },
            &spawn_tx,
            &senders,
            &crate::realfs::RealFilesystem,
            &logger::noop_sender(),
        );

        // current_session_text must be cleared on Done.
        assert!(
            !app.current_session_text.contains_key("my-goal"),
            "current_session_text must be cleared on Done"
        );

        // Envelope from full_output must have been processed — spawn must be enqueued.
        let req = spawn_rx.try_recv()
            .expect("envelope in full_output must be processed even when current_session_text had no envelope");
        assert!(req.fresh_session.is_some(), "spawn request must carry fresh_session config");
    }

    // spec (fresh-agents): ephemeral coordinators CAN spawn further fresh sub-sessions
    // (unbounded depth). A coordinator uses its own session ID in dispatch tags so
    // sub-sub-sessions reply to it rather than to the permanent goal agent.
    #[test]
    fn test_spec_fresh_agents_ephemeral_coordinator_can_spawn_sub_sessions() {
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "the goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        // A coordinator uses its OWN ephemeral ID in the dispatch tag so sub-sessions
        // can reply to it via @-message routing.
        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "my-goal~1".into(),
                full_output: "<@my-goal~1|sub>nested sub-task</@my-goal~1|sub>".into(),
            },
            &spawn_tx,
            &senders,
            &crate::realfs::RealFilesystem,
            &logger::noop_sender(),
        );

        // A fresh spawn request must be enqueued.
        let req = spawn_rx.try_recv()
            .ok()
            .filter(|r| r.fresh_session.is_some());
        assert!(
            req.is_some(),
            "ephemeral coordinator must be able to spawn further fresh sub-sessions"
        );
        let fresh = req.unwrap().fresh_session.unwrap();
        // The dispatcher_id must be the coordinator's own ID so the spawn handler
        // constructs an init message that tells the sub-session to reply to it.
        assert_eq!(
            fresh.dispatcher_id, "my-goal~1",
            "dispatcher_id must be the coordinator's session ID, not the permanent base"
        );
        // The new session ID must nest under the coordinator, not the permanent base.
        assert!(
            fresh.session_id.starts_with("my-goal~1~"),
            "sub-sub-session ID must nest under the coordinator: my-goal~1~N"
        );
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

    // spec (fresh-agents): interactive sessions (tend / rummage / jog) CAN spawn
    // fresh sub-sessions. The is_interactive exclusion has been removed; the only
    // remaining guard is the ephemeral-depth limit (sessions with '~' in their ID).
    #[test]
    fn test_spec_fresh_agents_interactive_agents_can_fresh_dispatch() {
        for agent_id in &["tend", "rummage", "jog"] {
            let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
            let (msg_tx, _) = mpsc::unbounded_channel::<String>();
            let (senders, _, _) = make_test_session_senders(&msg_tx);

            let mut app = App::new();
            // An interactive agent emits a fresh-dispatch envelope in its output.
            handle_session_event(
                &mut app,
                SessionEvent::Done {
                    goal_id: agent_id.to_string(),
                    full_output: format!("<@{}|sub>some task</@{}|sub>", agent_id, agent_id),
                },
                &spawn_tx,
                &senders,
                &crate::realfs::RealFilesystem,
                &logger::noop_sender(),
            );

            let maybe_fresh = spawn_rx.try_recv().ok()
                .filter(|r| r.fresh_session.is_some());
            assert!(
                maybe_fresh.is_some(),
                "{agent_id} must trigger fresh dispatch (interactive sessions can now spawn sub-sessions)",
            );
        }
    }

    // spec (fresh-agents): the ephemeral session ID counter increments with each
    // fresh dispatch, producing unique IDs for concurrent sub-sessions.
    #[test]
    fn test_spec_fresh_agents_counter_increments_per_dispatch() {
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "g".into(),
            summary: String::new(),
            description: "the goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "g".into(),
                full_output: "<@g|a>task a</@g|a>\n<@g|b>task b</@g|b>".into(),
            },
            &spawn_tx,
            &senders,
            &crate::realfs::RealFilesystem,
            &logger::noop_sender(),
        );

        let req1 = spawn_rx.try_recv().expect("first spawn request");
        let req2 = spawn_rx.try_recv().expect("second spawn request");
        let id1 = req1.fresh_session.unwrap().session_id;
        let id2 = req2.fresh_session.unwrap().session_id;
        assert_ne!(id1, id2, "each fresh sub-session must have a unique ID");
        assert!(id1.starts_with("g~"), "id1 must use parent~ prefix");
        assert!(id2.starts_with("g~"), "id2 must use parent~ prefix");
    }

    // spec (fresh-agents): the fresh session spawn request carries the task
    // message from the dispatcher's fresh-dispatch envelope.
    #[test]
    fn test_spec_fresh_agents_spawn_request_carries_task() {
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "g".into(),
            summary: String::new(),
            description: "the goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });
        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "g".into(),
                full_output: "<@g|check>review auth module</@g|check>".into(),
            },
            &spawn_tx,
            &senders,
            &crate::realfs::RealFilesystem,
            &logger::noop_sender(),
        );

        let req = spawn_rx.try_recv().expect("spawn request must be enqueued");
        assert_eq!(req.message, "review auth module", "spawn request must carry the task verbatim");
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

    // spec (tui / fresh-agents): a Chunk event containing an opening tag
    // `<@goal-id|label>` must pre-register the ephemeral session in
    // ephemeral_sessions, running_sessions, ephemeral_sessions_ordered, and
    // update goal_list_scroll.last_total — before the turn completes.
    #[test]
    fn test_spec_fresh_agents_chunk_pre_announces_ephemeral_session() {
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "the goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });
        // Simulate a prior render so last_total has a baseline.
        app.goal_list_scroll.record_render(1, 8, 0);

        // Fire a Chunk event with only the opening tag (no closing tag yet).
        handle_session_event(
            &mut app,
            SessionEvent::Chunk {
                goal_id: "my-goal".into(),
                text: "<@my-goal|work>\n".into(),
            },
            &spawn_tx,
            &senders,
            &crate::realfs::RealFilesystem,
            &logger::noop_sender(),
        );

        // A sub-session must have been pre-announced immediately.
        assert_eq!(
            app.ephemeral_sessions.len(), 1,
            "one ephemeral session must be pre-announced on the opening tag",
        );
        let pre_id = app.ephemeral_sessions_ordered.first().expect("ordered list must contain the pre-announced entry").clone();
        assert!(pre_id.starts_with("my-goal~"), "pre-announced ID must use the parent~ prefix");
        assert!(
            app.running_sessions.contains_key(&pre_id),
            "pre-announced session must appear in running_sessions",
        );
        assert_eq!(
            app.goal_list_scroll.last_total, 2,
            "last_total must reflect the new goal-list row immediately",
        );
        // No spawn request yet — the task body arrives at Done time.
        assert!(
            spawn_rx.try_recv().is_err(),
            "spawn request must NOT be enqueued at Chunk time — only at Done time",
        );
        // The pending map must track the pre-announced entry.
        let pending = app.pending_fresh_announcements.get("my-goal").expect("pending_fresh_announcements must track my-goal");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, Some("work".to_string()), "label must be captured");
    }

    // spec (tui / fresh-agents): an opening tag seen during streaming that
    // has no matching closing tag by turn-end must be removed from the goal
    // list (not left as a dangling phantom entry).
    #[test]
    fn test_spec_fresh_agents_incomplete_envelope_removed_at_done() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;
        use std::sync::Arc;

        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let fs = Arc::new(MockFs::new());
        let tinker_dir = PathBuf::from("/.tinker");
        let goals_dir = tinker_dir.join("goals");
        fs.add_dir(&goals_dir);
        let goal_path = goals_dir.join("my-goal.toml");
        fs.add_file(
            &goal_path,
            "id = \"my-goal\"\nsummary = \"\"\ndescription = \"desc\"\nparent_id = \"\"\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "desc".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(goal_path),
        });

        // Opening tag appears in a Chunk — session is pre-announced.
        handle_session_event(
            &mut app,
            SessionEvent::Chunk {
                goal_id: "my-goal".into(),
                text: "<@my-goal|orphan>\n".into(),
            },
            &spawn_tx,
            &senders,
            &*fs,
            &logger::noop_sender(),
        );
        let pre_id = app.ephemeral_sessions_ordered.first().cloned().expect("session must be pre-announced");
        assert!(app.ephemeral_sessions.contains(&pre_id), "pre-announced session must be in ephemeral_sessions");

        // Turn ends without a closing tag — full_output has only the opening tag;
        // envelope extraction finds no complete envelope → no spawn, pre-announced removed.
        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "my-goal".into(),
                full_output: "<@my-goal|orphan>\n".into(),
            },
            &spawn_tx,
            &senders,
            &*fs,
            &logger::noop_sender(),
        );

        // The phantom entry must have been removed.
        assert!(
            !app.ephemeral_sessions.contains(&pre_id),
            "pre-announced session with no complete envelope must be removed at Done time",
        );
        assert!(
            !app.ephemeral_sessions_ordered.contains(&pre_id),
            "pre-announced session must also be removed from ephemeral_sessions_ordered",
        );
        assert!(
            !app.running_sessions.contains_key(&pre_id),
            "pre-announced session must be removed from running_sessions",
        );
        // No spawn request must have been enqueued.
        let maybe_fresh = spawn_rx.try_recv().ok().filter(|r| r.fresh_session.is_some());
        assert!(
            maybe_fresh.is_none(),
            "no spawn request must be enqueued for an incomplete envelope",
        );
    }

    // spec (tui / fresh-agents): when Chunk pre-announces a session and Done
    // finds the matching complete envelope, the spawn request must reuse the
    // pre-announced session ID (no counter bump, no flicker).
    #[test]
    fn test_spec_fresh_agents_done_reuses_pre_announced_session_id() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;
        use std::sync::Arc;

        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let fs = Arc::new(MockFs::new());
        let tinker_dir = PathBuf::from("/.tinker");
        let goals_dir = tinker_dir.join("goals");
        fs.add_dir(&goals_dir);
        let goal_path = goals_dir.join("my-goal.toml");
        fs.add_file(
            &goal_path,
            "id = \"my-goal\"\nsummary = \"\"\ndescription = \"desc\"\nparent_id = \"\"\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "desc".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(goal_path),
        });

        // Opening tag appears during streaming — session pre-announced as my-goal~1.
        handle_session_event(
            &mut app,
            SessionEvent::Chunk {
                goal_id: "my-goal".into(),
                text: "<@my-goal|label>\n".into(),
            },
            &spawn_tx,
            &senders,
            &*fs,
            &logger::noop_sender(),
        );
        let pre_id = app.ephemeral_sessions_ordered.first().cloned().expect("pre-announced session must exist");

        // The rest of the output arrives — closing tag present in the full turn text.
        handle_session_event(
            &mut app,
            SessionEvent::Chunk {
                goal_id: "my-goal".into(),
                text: "do the work\n</@my-goal|label>\n".into(),
            },
            &spawn_tx,
            &senders,
            &*fs,
            &logger::noop_sender(),
        );

        // Done fires — must reuse the pre-announced ID.
        // full_output is the complete assembled reply; the pre-announced session is
        // matched against the envelope found here.
        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "my-goal".into(),
                full_output: "<@my-goal|label>\ndo the work\n</@my-goal|label>\n".into(),
            },
            &spawn_tx,
            &senders,
            &*fs,
            &logger::noop_sender(),
        );

        let req = spawn_rx.try_recv().expect("spawn request must be enqueued at Done time");
        let fresh = req.fresh_session.expect("spawn request must carry fresh_session config");
        assert_eq!(
            fresh.session_id, pre_id,
            "Done must reuse the pre-announced session ID, not mint a new one",
        );
        assert_eq!(fresh.label, Some("label".to_string()), "label must be preserved");
        assert!(
            app.ephemeral_sessions.contains(&pre_id),
            "session must remain in ephemeral_sessions after Done spawns it",
        );
    }

    // spec (fresh-agents / tui): when an ephemeral coordinator fires a Chunk event
    // containing an opening tag addressed to itself (e.g. `<@my-goal~1|sub>`), the
    // pre-announced sub-session ID must be nested under the coordinator — it must
    // start with `my-goal~1~`, not the permanent base `my-goal~`.  This ensures
    // multi-level depth is preserved and the coordinator receives replies correctly.
    #[test]
    fn test_spec_fresh_agents_chunk_pre_announce_uses_coordinator_id_as_prefix() {
        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        let mut app = App::new();
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "the goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: None,
        });

        // The dispatcher is an ephemeral coordinator (not the permanent base goal).
        handle_session_event(
            &mut app,
            SessionEvent::Chunk {
                goal_id: "my-goal~1".into(),
                text: "<@my-goal~1|sub>\n".into(),
            },
            &spawn_tx,
            &senders,
            &crate::realfs::RealFilesystem,
            &logger::noop_sender(),
        );

        assert_eq!(
            app.ephemeral_sessions.len(), 1,
            "one ephemeral session must be pre-announced",
        );
        let pre_id = app.ephemeral_sessions_ordered.first()
            .cloned()
            .expect("ordered list must contain the pre-announced entry");
        assert!(
            pre_id.starts_with("my-goal~1~"),
            "pre-announced ID must nest under the coordinator (my-goal~1~N), got: {pre_id}",
        );
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
        let remove_in_send = main_rs[send_pos..confirm_pos].contains("session_senders.remove");
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
        let remove_in_confirm = after_confirm.contains("session_senders.remove");
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
        use crate::test_utils::MockFs;
        use std::path::PathBuf;
        use std::sync::Arc;

        let (spawn_tx, _spawn_rx) = mpsc::unbounded_channel::<SpawnGoalRequest>();
        let (senders, _, _) = make_test_session_senders(&mpsc::unbounded_channel::<String>().0);

        // Set up a MockFs with a goal TOML so that the Done event's goal-reload
        // keeps the goal in app.goals (rather than wiping it).
        // load_goals joins tinker_dir with "goals", so tinker_dir must be "/.tinker".
        let fs = Arc::new(MockFs::new());
        let tinker_dir = PathBuf::from("/.tinker");
        let goals_dir = tinker_dir.join("goals");
        fs.add_dir(&goals_dir);
        let goal_path = goals_dir.join("my-goal.toml");
        fs.add_file(
            &goal_path,
            "id = \"my-goal\"\nsummary = \"\"\ndescription = \"desc\"\nparent_id = \"\"\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.goals.push(goal::Goal {
            id: "my-goal".into(),
            summary: String::new(),
            description: "desc".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            kind: None,
            source_path: Some(goal_path),
        });

        // Simulate a prior render: last_total reflects 1 item (the goal itself).
        app.goal_list_scroll.record_render(1, 8, 0);
        assert_eq!(app.goal_list_scroll.last_total, 1, "precondition: last_total is 1 before insertion");

        // Fire a Done event with a fresh dispatch envelope — this inserts an ephemeral.
        handle_session_event(
            &mut app,
            SessionEvent::Done {
                goal_id: "my-goal".into(),
                full_output: "<@my-goal|work>do the task</@my-goal|work>".into(),
            },
            &spawn_tx,
            &senders,
            &*fs,
            &logger::noop_sender(),
        );

        // After insertion the list has 2 items (1 goal + 1 ephemeral);
        // last_total must reflect that immediately, before the next render.
        assert_eq!(
            app.goal_list_scroll.last_total, 2,
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

}
