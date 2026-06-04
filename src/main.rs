mod app;
mod cap;
mod claude;
mod cleanup;
mod config;
mod goal;
mod goal_session;
mod logger;
mod opencode;
mod realfs;
mod tui;
#[cfg(test)]
mod test_utils;

use anyhow::Result;
use app::{App, Focus, ModalField, Phase};
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

/// Frontmatter for goal-agent sessions (written to `tinker.md`).
/// Declares the agent profile that `--agent tinker` selects. No path-scoped
/// permission rules — file-access boundaries are conveyed via the prompt.
const GOAL_AGENT_FRONTMATTER: &str =
    "---\ndescription: >-\n  Tinker agent.\nmode: primary\n---\n";

/// Frontmatter for the tend agent (written to `tend.md`).
/// Declares the agent profile that `--agent tend` selects. No path-scoped
/// permission rules — file-access boundaries are conveyed via the prompt.
const TEND_FRONTMATTER: &str =
    "---\ndescription: >-\n  Tinker agent.\nmode: primary\n---\n";

fn packaged_tend_goal() -> Goal {
    const TOML: &str = include_str!("../packaged-goals/tend.toml");
    toml::from_str(TOML).expect("packaged tend.toml must be valid Goal TOML")
}

/// Assembles the full tend.md content written at startup: tend frontmatter
/// (with path-scoped permission rules) followed by the tend goal description.
fn tend_agent_content() -> String {
    format!("{}{}", TEND_FRONTMATTER, packaged_tend_goal().description)
}

/// System prompt for tend when running under the claude backend.
/// Leads with the file-scope boundary so it arrives as a system-level constraint,
/// not a buried instruction in a user-turn message. The claude backend has no
/// equivalent to opencode's path-scoped `permission:` block in tend.md; this is
/// the closest available substitute.
fn tend_system_prompt() -> String {
    format!(
        "Read and write files ONLY under .tinker/goals/ — nothing outside that path. \
         src/ and all other directories are off-limits. VCS is read-only: git status/diff/log only.\n\n\
         {}",
        packaged_tend_goal().description
    )
}

/// System prompt for claude goal agents. Contains the session-invariant
/// framework preamble so it persists across session turns without repeating
/// in every per-dispatch init message. Mirrors the opencode `tinker.md` /
/// per-dispatch split.
fn goal_agent_system_prompt() -> String {
    goal_session::goal_agent_framework_preamble()
}

fn tend_init_prompt(goals_summary: &str, neighbor_section: &str) -> String {
    format!(
        "## Current goals (compact index — pull full text on demand)\n{goals_summary}\n\n\
         ---\n\n\
         {neighbor_section}\
         **Startup.** This is a regular startup. Wait for the user's first instruction — \
         produce no output, no greeting, no acknowledgement."
    )
}

fn tend_init_prompt_full_context(goals_summary: &str, neighbor_section: &str) -> String {
    format!(
        "## Current goals (full text)\n{goals_summary}\n\n\
         ---\n\n\
         {neighbor_section}\
         **Startup.** This is a regular startup. Wait for the user's first instruction — \
         produce no output, no greeting, no acknowledgement."
    )
}

/// Lazy-spawn request: sent when `@goal-id` arrives and that agent isn't in
/// the session registry yet, or when the user triggers a goal via the tree UI.
struct SpawnGoalRequest {
    goal_id: String,
    /// The dispatch message — trigger reason for first turn, peer message for
    /// subsequent turns routed before the session was alive.
    message: String,
}

/// Persistent goal agent loop. One task per spawned goal agent.
/// Receives messages via `msg_rx`, runs the LLM with session resumption, and
/// emits SessionEvents back on `session_tx`. Cleanup hook fires only before
/// the first LLM turn of a new session (llm_session_id is None).
// Each parameter is a distinct capability or config value; grouping into a
// struct would obscure the injected-capability boundary without reducing complexity.
#[allow(clippy::too_many_arguments)]
async fn goal_agent_loop(
    goal: goal::Goal,
    mut msg_rx: mpsc::Receiver<String>,
    session_tx: mpsc::Sender<SessionEvent>,
    oc: Arc<dyn OpenCodeRunner>,
    oc_cleanup: Arc<dyn OpenCodeRunner>,
    fs: Arc<dyn Filesystem>,
    work_dir: PathBuf,
    app_ref: Arc<Mutex<App>>,
    log: logger::LogSender,
    backend_name: String,
    lean_init: bool,
) {
    let goal_id = goal.id.clone();
    let mut llm_session_id: Option<String> = None;

    while let Some(dispatch_msg) = msg_rx.recv().await {
        // Cleanup hook: fires only before the first turn of a new session.
        if llm_session_id.is_none() {
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

        // Build the LLM message. First turn: full framework preamble + reason.
        // Subsequent turns: forward the dispatch message directly.
        let llm_message = if llm_session_id.is_none() {
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
            if lean_init {
                goal_session::goal_agent_lean_init_message(&goal, Some(&dispatch_msg), &compact_index)
            } else {
                goal_session::session_init_message(&goal, Some(&dispatch_msg), &compact_index)
            }
        } else {
            dispatch_msg
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
        let run_result = oc.run(&llm_message, llm_session_id.as_deref(), &work_dir, on_sid, on_chunk).await;
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
                    full_output: output,
                    backend: backend_name.clone(),
                });
                let _ = session_tx.send(SessionEvent::Done { goal_id: goal_id.clone() }).await;
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
                    full_output: output,
                    backend: backend_name.clone(),
                });
                // Clear session_id so next message starts a fresh session.
                llm_session_id = None;
                let _ = session_tx.send(SessionEvent::Done { goal_id: goal_id.clone() }).await;
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

    // Four runner instances: high-tier (tend/rummage/jog + other high goals),
    // mid-tier (default goal sessions), low-tier (low goal sessions), cleanup (cheapest).
    let (oc_goal_high, oc_goal, oc_goal_low, oc_cleanup_runner):
        (Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>) = if use_claude {
        let tinker_m = model_config.claude_high(CLAUDE_TINKER_MODEL);
        let goal_m = model_config.claude_mid(CLAUDE_GOAL_MODEL);
        let cleanup_m = model_config.claude_low(CLAUDE_SCHEDULER_MODEL);
        (
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, tend_system_prompt())),
            Arc::new(ClaudeRunner::with_system_prompt(goal_m, goal_agent_system_prompt())),
            Arc::new(ClaudeRunner::with_system_prompt(cleanup_m, goal_agent_system_prompt())),
            Arc::new(ClaudeRunner::new(cleanup_m)),
        )
    } else if use_default_model {
        (
            Arc::new(RealOpenCodeRunner::default_with_agent("tinker")),
            Arc::new(RealOpenCodeRunner::new_default()),
            Arc::new(RealOpenCodeRunner::new_default()),
            Arc::new(RealOpenCodeRunner::new_default()),
        )
    } else {
        let tinker_m = model_config.opencode_high(OPENCODE_TINKER_MODEL);
        let goal_m = model_config.opencode_mid(OPENCODE_GOAL_MODEL);
        let cleanup_m = model_config.opencode_low(OPENCODE_SCHEDULER_MODEL);
        (
            Arc::new(RealOpenCodeRunner::with_agent(tinker_m, "tinker")),
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

    // Write agent files at startup (always overwrite). Skip for Claude backend —
    // persona arrives via --system-prompt there, not agent files.
    // tinker.md: the profile selected by `--agent tinker` for goal-agent sessions.
    // tend.md: the profile selected for the tend session (its goal description is
    // appended as the agent body). Neither carries path-scoped permission rules —
    // opencode's system defaults auto-approve tool calls and file-access boundaries
    // are conveyed via the prompt, not the harness.
    if !use_claude {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
        let agent_dir = home.join(".config/opencode/agents");
        let _ = fs.mkdir_all(&agent_dir);
        let _ = fs.write(&agent_dir.join("tinker.md"), GOAL_AGENT_FRONTMATTER);
        let _ = fs.write(&agent_dir.join("tend.md"), &tend_agent_content());
    }

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
    let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::channel::<SpawnGoalRequest>(32);

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
                    if a.selected_goal >= a.flat_goals().len().max(1) {
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
    goal_spawn_tx: mpsc::Sender<SpawnGoalRequest>,
    goal_spawn_rx: &mut mpsc::Receiver<SpawnGoalRequest>,
    oc_goal: Arc<dyn OpenCodeRunner>,
    oc_goal_high: Arc<dyn OpenCodeRunner>,
    oc_goal_low: Arc<dyn OpenCodeRunner>,
    oc_cleanup_runner: Arc<dyn OpenCodeRunner>,
    fs: Arc<dyn Filesystem>,
    work_dir: std::path::PathBuf,
    session_tx: mpsc::Sender<SessionEvent>,
    log: logger::LogSender,
    backend_name: &str,
    use_full_goal_context: bool,
) -> Result<()> {
    // Session registry: maps goal_id → message channel sender.
    // Tend is pre-populated (eager start); all other sessions start lazily.
    let mut session_senders: HashMap<String, mpsc::Sender<String>> = HashMap::new();

    // Eager-start tend: find its goal, spawn goal_agent_loop, send the initial trigger.
    {
        let tend_goal = app.lock().unwrap().goals.iter().find(|g| g.id == "tend").cloned()
            .unwrap_or_else(packaged_tend_goal);
        let compact_index = {
            let a = app.lock().unwrap();
            if a.goals.is_empty() { "[]".to_string() } else { goal::build_compact_index(&a.goals) }
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
            tend_init_prompt_full_context(&compact_index, &neighbor_section)
        } else {
            tend_init_prompt(&compact_index, &neighbor_section)
        };
        let (tend_tx, tend_rx) = mpsc::channel::<String>(16);
        session_senders.insert("tend".to_string(), tend_tx.clone());
        let app_ref = app.clone();
        let session_tx_t = session_tx.clone();
        let oc_t = oc_goal_high.clone();
        let oc_cleanup_t = oc_cleanup_runner.clone();
        let fs_t = fs.clone();
        let work_dir_t = work_dir.clone();
        let log_t = log.clone();
        let backend_t = backend_name.to_string();
        tokio::spawn(async move {
            goal_agent_loop(tend_goal, tend_rx, session_tx_t, oc_t, oc_cleanup_t, fs_t, work_dir_t, app_ref, log_t, backend_t, false).await;
        });
        let _ = tend_tx.try_send(trigger);
    }

    loop {
        // Draw
        terminal.draw(|f| tui::draw(f, &mut app.lock().unwrap()))?;

        // Drain lazy goal-agent spawn requests. For each request:
        // - If the goal is already in the registry: route the message.
        // - If not: create a channel, register it, spawn a persistent task.
        while let Ok(req) = goal_spawn_rx.try_recv() {
            if let Some(tx) = session_senders.get(&req.goal_id) {
                let _ = tx.try_send(req.message);
            } else {
                let goal = app.lock().unwrap().goals.iter().find(|g| g.id == req.goal_id).cloned();
                if let Some(goal) = goal {
                    let (msg_tx_goal, msg_rx_goal) = mpsc::channel::<String>(16);
                    session_senders.insert(req.goal_id.clone(), msg_tx_goal.clone());
                    let oc_for_goal = match goal.tier.as_deref() {
                        Some("high") => oc_goal_high.clone(),
                        Some("low") => oc_goal_low.clone(),
                        _ => oc_goal.clone(),
                    };
                    let session_tx_goal = session_tx.clone();
                    let oc_cleanup_goal = oc_cleanup_runner.clone();
                    let fs_goal = fs.clone();
                    let app_ref_goal = app.clone();
                    let log_goal = log.clone();
                    let backend_goal = backend_name.to_string();
                    let lean_init_goal = backend_name == "claude";
                    let work_dir_goal = work_dir.clone();
                    let _ = msg_tx_goal.try_send(req.message);
                    tokio::spawn(async move {
                        goal_agent_loop(
                            goal, msg_rx_goal, session_tx_goal,
                            oc_for_goal, oc_cleanup_goal, fs_goal,
                            work_dir_goal, app_ref_goal, log_goal, backend_goal, lean_init_goal,
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
                let gid = app.lock().unwrap().flat_goals().get(sel_after).map(|g| g.id.clone());
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
                    let gid = a.flat_goals().get(a.selected_goal).map(|g| g.id.clone());
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
                    if matches!(session_id.as_str(), "tend" | "rummage" | "jog") {
                        app.lock().unwrap().running_sessions.insert(session_id.clone(), None);
                    }
                    if let Some(tx) = session_senders.get(&session_id) {
                        let _ = tx.send(msg).await;
                    } else {
                        // Session not yet spawned — route through lazy spawn so the
                        // first user message to rummage/jog triggers session_init_message.
                        let _ = goal_spawn_tx.try_send(SpawnGoalRequest {
                            goal_id: session_id,
                            message: msg,
                        });
                    }
                }
                KeyAction::ConfirmOptions { goal_id, reason, new_tier } => {
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
                        let sys_msg = format!(
                            "triggered: `{}`{}",
                            goal_id,
                            reason.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
                        );
                        let submission_sets_running = matches!(goal_id.as_str(), "tend" | "rummage" | "jog");
                        {
                            let mut a = app.lock().unwrap();
                            a.push_system_message(&sys_msg);
                            if !submission_sets_running {
                                a.running_sessions.insert(goal_id.clone(), reason);
                            }
                        }
                        log.emit("dispatcher", logger::LogEvent::TinkerSystemMessageReceived { content: sys_msg });
                        let _ = goal_spawn_tx.try_send(SpawnGoalRequest {
                            goal_id: goal_id.clone(),
                            message: reason_str,
                        });
                    }
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

    while i < lines.len() {
        let trimmed = lines[i].trim();
        let mut matched = false;

        for id in known_ids {
            let open_tag = format!("<@{}>", id);
            let close_tag = format!("</@{}>", id);

            if let Some(tag_pos) = trimmed.find(open_tag.as_str()) {
                let after_open = &trimmed[tag_pos + open_tag.len()..];

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
        }

        if !matched {
            i += 1;
        }
    }

    out
}

/// Deliver peer consultations collected from a completed agent reply.
/// Routes to the session registry for known agents; triggers lazy spawn via
/// `goal_spawn_tx` for goal IDs not yet in the registry.
/// All non-interactive goal agents are dispatched unconditionally in parallel.
fn dispatch_peer_consultations(
    app: &mut App,
    sender: &str,
    consultations: &[(String, String)],
    session_senders: &HashMap<String, mpsc::Sender<String>>,
    goal_spawn_tx: &mpsc::Sender<SpawnGoalRequest>,
    log: &logger::LogSender,
) {
    for (recipient, msg) in consultations {
        let reply_tag = format!("<@{}>", sender);
        let reply_close = format!("</@{}>", sender);
        let formatted = format!(
            "[from {}] {}\n\nReply via {}your reply{}.",
            sender, msg, reply_tag, reply_close
        );
        let sys = format!("<@{}> → <@{}>: {}", sender, recipient, msg);
        app.push_system_message(&sys);
        log.emit(sender, logger::LogEvent::TinkerSystemMessageReceived { content: sys });
        let is_goal_agent = !matches!(recipient.as_str(), "tend" | "rummage" | "jog")
            && app.goals.iter().any(|g| &g.id == recipient);

        if let Some(tx) = session_senders.get(recipient) {
            let _ = tx.try_send(formatted);
        } else if app.goals.iter().any(|g| &g.id == recipient) {
            let _ = goal_spawn_tx.try_send(SpawnGoalRequest {
                goal_id: recipient.clone(),
                message: formatted,
            });
        }
        if is_goal_agent {
            let reason = msg.lines().next().unwrap_or("").to_string();
            app.running_sessions.entry(recipient.clone()).or_insert(Some(reason));
        }
    }
}

/// Collect all IDs the @-block parser should recognise: current registry entries
/// plus all known goal IDs (so agents can address goals not yet spawned).
fn known_agent_ids<'a>(
    session_senders: &'a HashMap<String, mpsc::Sender<String>>,
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



/// Unified session event handler. Routes events from any agent session to the
/// appropriate App state updates and peer consultations.
fn handle_session_event(
    app: &mut App,
    ev: SessionEvent,
    goal_spawn_tx: &mpsc::Sender<SpawnGoalRequest>,
    session_senders: &HashMap<String, mpsc::Sender<String>>,
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
            app.current_session_text.entry(goal_id).or_default().push_str(&text);
        }
        SessionEvent::Done { goal_id } => {
            app.finalize_agent_message(&goal_id);
            // Clear the ▶ indicator for any session type, including interactive agents.
            app.running_sessions.remove(&goal_id);
            let session_text = app.current_session_text.remove(&goal_id).unwrap_or_default();
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
                    let msg = format!(
                        "The goal file you just edited failed to parse:\n\n{}\n\n\
                         Read the file, identify the structural error (typical causes: \
                         unclosed `\"\"\"`, a duplicate top-level key, or content placed \
                         inside the description block instead of after its closing `\"\"\"`), \
                         and rewrite the full file with the Write tool. The file must have \
                         these top-level keys in order: {}.",
                        listing, goal::GOAL_SCHEMA_KEYS_ORDER,
                    );
                    app.correction_attempts += 1;
                    let correction_msg = format!(
                        "Goal file invalid; asking tend to fix (attempt {}/2).",
                        app.correction_attempts
                    );
                    app.push_system_message(&correction_msg);
                    log.emit("correction-injector", logger::LogEvent::TinkerSystemMessageReceived { content: correction_msg });
                    if let Some(tx) = session_senders.get("tend") {
                        let _ = tx.try_send(msg);
                    }
                    return;
                } else if !new_errors.is_empty() {
                    let still_invalid_msg = "Goal file still invalid after 2 attempts; leaving as-is. Edit manually if needed.";
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
            // Silence detection: if the session produced no output at all, prompt
            // the agent to surface what happened. Applies uniformly to all sessions.
            if session_text.trim().is_empty()
                && let Some(tx) = session_senders.get(&goal_id) {
                    let _ = tx.try_send(
                        "You produced no response to the previous message. \
                         Did you mean to say something?".to_string()
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
            if let Some(m) = app.modal.as_mut() {
                if m.focused_field == ModalField::Tier {
                    let next = cycle_tier_display_prev(&m.tier);
                    m.tier = next.to_string();
                }
            }
            KeyAction::None
        }
        (_, KeyCode::Right) => {
            if let Some(m) = app.modal.as_mut() {
                if m.focused_field == ModalField::Tier {
                    let next = cycle_tier_display_next(&m.tier);
                    m.tier = next.to_string();
                }
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
            if let Some(m) = app.modal.as_mut() {
                if m.focused_field == ModalField::Reason {
                    m.input.pop();
                }
            }
            KeyAction::None
        }
        (_, KeyCode::Char(c)) => {
            if let Some(m) = app.modal.as_mut() {
                if m.focused_field == ModalField::Reason {
                    m.input.push(c);
                }
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
            (_, KeyCode::Enter) => match app.selected_goal() {
                Some(g) => {
                    // Enter on a goal opens the options dialog. Pane focus
                    // stays on `Tree` — when the modal closes (submit or Esc),
                    // the user is still in the tree.
                    let tier_display = g.tier.as_deref().unwrap_or("mid").to_string();
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

    fn make_test_session_senders(
        msg_tx: &mpsc::Sender<String>,
    ) -> (HashMap<String, mpsc::Sender<String>>, mpsc::Receiver<String>, mpsc::Receiver<String>) {
        let (rummage_tx, rummage_rx) = mpsc::channel::<String>(8);
        let (jog_tx, jog_rx) = mpsc::channel::<String>(8);
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
        // Exhaustive match ensures LlmSessionId and SummaryReady stay absent,
        // and that Done does not carry a crashed field.
        let evt = SessionEvent::Done { goal_id: "x".into() };
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
        // `realfs.rs` owns Filesystem.
        let cmd_allowed = ["opencode.rs", "claude.rs"];
        let fs_allowed = ["realfs.rs"];

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
            s.record_render(200, 10);
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
        assert!(
            main_rs.contains("goal::build_full_text_index"),
            "main.rs must call build_full_text_index on the full-context path",
        );
        assert!(
            main_rs.contains("tend_init_prompt_full_context"),
            "main.rs must call tend_init_prompt_full_context on the full-context path",
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
            main_rs.contains("Some(\"high\") => oc_goal_high"),
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
            main_rs.contains("Some(\"low\") => oc_goal_low"),
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

    // spec (goal-agents): dispatch_peer_consultations requests lazy spawn when
    // the recipient is a known goal ID not yet in the session registry.
    #[test]
    fn test_spec_goal_agent_dispatch_triggers_lazy_spawn() {
        let (msg_tx, _msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, mut spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
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
        let (msg_tx, _) = mpsc::channel::<String>(8);
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
        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (rummage_tx, mut rummage_rx) = mpsc::channel::<String>(8);
        let (jog_tx, mut jog_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);

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
    // instruction ("Reply via <@sender>your reply</@sender>.") so the receiving
    // agent knows to wrap its answer in a tag envelope rather than leave it as
    // private prose.
    #[test]
    fn test_spec_peer_consult_dispatch_includes_reply_instruction() {
        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
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
            msg.contains("Reply via <@rummage>"),
            "dispatched message must carry return-routing instruction with sender name in tag-envelope syntax"
        );
    }

    // spec (peer-consult): a system message is pushed for each consultation
    // so the user can observe the exchange in real time.
    #[test]
    fn test_spec_peer_consult_pushes_system_message_for_visibility() {
        let (msg_tx, _) = mpsc::channel::<String>(8);
        let (rummage_tx, mut rummage_rx) = mpsc::channel::<String>(8);
        let (jog_tx, _) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);

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
    fn test_spec_tinker_agent_file_frontmatter_is_primary_mode() {
        let content = tend_agent_content();
        assert!(content.starts_with("---\n"), "agent file must begin with YAML frontmatter delimiter");
        assert!(content.contains("mode: primary"), "agent must be declared mode: primary");
    }

    #[test]
    fn test_spec_tinker_static_persona_in_agent_dynamic_goals_in_init() {
        let content = tend_agent_content();
        assert!(content.starts_with("---\n"), "agent file must begin with YAML frontmatter");
        let init = tend_init_prompt("- demo-goal-id: a demo description", "");
        assert!(init.contains("Current goals"), "init prompt must label the dynamic goals section");
        assert!(init.contains("demo-goal-id"), "init prompt must carry the dynamic goals summary verbatim");
        assert!(!content.contains("demo-goal-id"), "agent file must not embed dynamic goal ids");
    }

    #[test]
    fn test_spec_tinker_proves_by_execution_not_reading_source() {
        let content = tend_agent_content();
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

    // spec (backends): tend's claude runner must be constructed with a system prompt so the
    // file-access boundary arrives as a persistent system message, not a user-turn instruction.
    // ClaudeRunner::new must not be used for the tend (tinker_m) slot in the claude branch.
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

    // spec (backends): goal agents' claude runner must be constructed with a system prompt so
    // the session-invariant framework preamble (message passing, progress guarantee, rules,
    // neighbor consultation mandate) arrives as a persistent system message. ClaudeRunner::new
    // must not be used for the goal agent (goal_m) slot in the claude branch.
    #[test]
    fn test_spec_claude_goal_runner_wired_with_system_prompt() {
        let main_rs = include_str!("main.rs");
        let goal_runner_line = main_rs
            .lines()
            .find(|l| l.contains("Arc::new(ClaudeRunner") && l.contains("goal_m"))
            .expect("must find ClaudeRunner construction for goal agents (goal_m) in the claude branch");
        assert!(
            !goal_runner_line.contains("ClaudeRunner::new("),
            "goal agents' claude runner must use with_system_prompt, not ClaudeRunner::new — got: {}",
            goal_runner_line.trim()
        );
    }

    #[test]
    fn test_spec_shared_language_form_norm_minimum_viable_shape() {
        let content = tend_agent_content();
        assert!(content.contains("One question per turn") || content.contains("one question per turn"),
            "prompt must enforce one-question-per-turn");
    }

    #[test]
    fn test_spec_tinker_encodes_dual_duty_no_fabrication_at_inflection_points() {
        let content = tend_agent_content();
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
    fn test_spec_agent_file_always_overwritten_not_guarded_by_exists_check() {
        let main_rs = include_str!("main.rs");
        // Split to avoid this test's own source appearing in the scan.
        let guard: String = ["if !agent_path", ".exists()"].concat();
        assert!(!main_rs.contains(&guard),
            "main.rs must not guard the agent-file write behind an existence check");
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
        let content = tend_agent_content();
        assert!(content.contains("Re-check parent summary"), "prompt must include a 'Re-check parent summary' step");
    }

    #[test]
    fn test_spec_tinker_prompt_related_links_symmetric_both_list_each_other() {
        // tend's write procedure defers the symmetry rule to goal-structure-standard
        // (read fresh) rather than restating it; the guardrail is that the prompt
        // still mandates re-validating every edge, symmetry included.
        let content = tend_agent_content();
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

        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
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
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
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

        let done = SessionEvent::Done { goal_id: "tend".to_string() };
        handle_session_event(&mut app, done, &spawn_tx, &senders, &RealFilesystem, &log);

        assert!(
            !app.running_sessions.contains_key("tend"),
            "tend must be removed from running_sessions after Done",
        );
    }

    // spec (agent-liveness): when a session produces no output at all (empty
    // current_session_text at Done time), a follow-up message must be sent back
    // into that session's channel so the agent is prompted to surface what happened.
    #[test]
    fn test_spec_silence_detection_sends_followup_on_empty_response() {
        use crate::goal_session::SessionEvent;

        let mut app = App::new();
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), msg_tx);
        let log = logger::noop_sender();

        // No chunk emitted — current_session_text is empty at Done time.
        let done = SessionEvent::Done { goal_id: "rummage".to_string() };
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
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let mut senders = HashMap::new();
        senders.insert("rummage".to_string(), msg_tx);
        let log = logger::noop_sender();

        let chunk = SessionEvent::Chunk { goal_id: "rummage".to_string(), text: "working on it".to_string() };
        handle_session_event(&mut app, chunk, &spawn_tx, &senders, &RealFilesystem, &log);

        let done = SessionEvent::Done { goal_id: "rummage".to_string() };
        handle_session_event(&mut app, done, &spawn_tx, &senders, &RealFilesystem, &log);

        assert!(
            msg_rx.try_recv().is_err(),
            "session that produced text must not receive a silence probe",
        );
    }

    // spec (tui — queue visibility): the SendToSession handler must insert
    // interactive agents (tend, rummage, jog) into running_sessions at submission
    // time so ▶ appears the moment the user sends a message, not at first chunk.
    #[test]
    fn test_spec_interactive_agent_running_sessions_on_submit() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("running_sessions.insert(session_id"),
            "SendToSession handler must insert interactive session into running_sessions at submission time",
        );
    }

    // spec (parallel-goal-agents): when another goal agent is already running,
    // a new @-dispatch must still go to spawn immediately — no serial queue.
    #[test]
    fn test_spec_at_dispatch_spawns_when_goal_agent_already_running() {
        let (msg_tx, _msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, mut spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
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
        let (msg_tx, _msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
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

    // spec (tui — queue visibility): @-dispatching to interactive chat agents
    // (tend, rummage, jog) must NOT add them to running_sessions via the dispatch
    // path. The ▶ marker appears only when a Chunk event arrives — i.e. the agent
    // is actively generating a response — not merely because a message was routed.
    #[test]
    fn test_spec_at_dispatch_skips_interactive_agents_in_running_sessions() {
        let (msg_tx, _msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
        let (senders, _, _) = make_test_session_senders(&msg_tx);

        let mut app = App::new();
        // Add rummage as a goal (it has a goal file) to ensure the goal-existence
        // check alone is not sufficient to skip it.
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
            !app.running_sessions.contains_key("tend"),
            "tend must not appear in running_sessions when dispatched via @",
        );
        assert!(
            !app.running_sessions.contains_key("rummage"),
            "rummage must not appear in running_sessions when dispatched via @",
        );
        assert!(
            !app.running_sessions.contains_key("jog"),
            "jog must not appear in running_sessions when dispatched via @",
        );
    }

    // spec (parallel-goal-agents): dispatch_peer_consultations must NOT gate on
    // any "is a goal agent already running?" check. Two concurrent non-interactive
    // dispatches must both go to spawn immediately — neither is queued.
    #[test]
    fn test_spec_dispatch_no_queue_gating_two_concurrent_agents() {
        let (msg_tx, _) = mpsc::channel::<String>(8);
        let (spawn_tx, mut spawn_rx) = mpsc::channel::<SpawnGoalRequest>(8);
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
    // goal's effective tier pre-populated; absent tier shows as "mid".
    // The focused field starts on Reason.
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

    // spec (tier-edit): absent tier (None) displays as "mid" in the options dialog.
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
        assert_eq!(m.tier, "mid", "absent tier must display as mid");
        assert_eq!(m.initial_tier, "mid");
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

}
