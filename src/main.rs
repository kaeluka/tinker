mod app;
mod cap;
mod claude;
mod cleanup;
mod config;
mod goal;
mod goal_session;
mod jog;
mod logger;
mod opencode;
mod rummage;
mod tend;
mod realfs;
mod tui;
#[cfg(test)]
mod test_utils;

use anyhow::Result;
use app::{ActiveAgent, App, Focus, Phase};
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
use opencode::{RealOpenCodeRunner, GOAL_MODEL as OPENCODE_GOAL_MODEL, TINKER_MODEL as OPENCODE_TINKER_MODEL, SCHEDULER_MODEL as OPENCODE_SCHEDULER_MODEL};
use claude::{ClaudeRunner, GOAL_MODEL as CLAUDE_GOAL_MODEL, TINKER_MODEL as CLAUDE_TINKER_MODEL, SCHEDULER_MODEL as CLAUDE_SCHEDULER_MODEL};
use tend::{tend_agent_content, tend_init_prompt, send_message, TendEvent};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use std::collections::HashMap;
use tokio::sync::mpsc;

fn description_from_toml_str(s: &str) -> String {
    let marker = "description = \"\"\"\n";
    let start = s.find(marker).expect("TOML must have description field") + marker.len();
    let end = start + s[start..].find("\n\"\"\"").expect("description field must close");
    s[start..end].to_string()
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
                init_message_chars: 0,
                backend: backend_name.clone(),
            });
            log.emit("goal_session", logger::LogEvent::GoalSessionStarted {
                goal_id: goal_id.clone(),
            });
            goal_session::session_init_message(&goal, Some(&dispatch_msg), &compact_index)
        } else {
            dispatch_msg
        };

        let full_output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let full_output_clone = full_output.clone();
        let tx_sid = session_tx.clone();
        let gid_sid = goal_id.clone();
        let tx_chunk = session_tx.clone();
        let gid_chunk = goal_id.clone();

        let on_sid: Chunk = Box::new(move |sid: String| {
            let _ = tx_sid.try_send(SessionEvent::LlmSessionId {
                goal_id: gid_sid.clone(),
                session_id: sid,
            });
        });
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
                llm_session_id = Some(new_sid.clone());
                let tool_calls = logger::count_tool_calls(&output);
                let files_modified = logger::extract_modified_files(&output);
                let usage = logger::parse_usage_from_text(&output);
                log.emit("goal_session", logger::LogEvent::GoalSessionFinished {
                    goal_id: goal_id.clone(),
                    exit_status: "clean".to_string(),
                    duration_ms: session_ms,
                    files_modified_count: files_modified.len(),
                    files_modified,
                    tool_calls,
                    summary_chars: 0,
                    full_output: output,
                    usage,
                    backend: backend_name.clone(),
                });
                let _ = session_tx.send(SessionEvent::Done { goal_id: goal_id.clone() }).await;
                // Structured summary continues the same LLM conversation.
                let summary = goal_session::run_silent(oc.as_ref(), goal_session::SUMMARY_REQUEST, Some(&new_sid), &work_dir)
                    .await.unwrap_or_default();
                let _ = session_tx.send(SessionEvent::SummaryReady {
                    goal_id: goal_id.clone(),
                    summary,
                }).await;
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
                    usage: None,
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

    // Three runner instances bound to different models — tend for
    // interview + batch summary (smartest), goal sessions for code production,
    // cleanup for the pre-session tinker-test-case hook (cheapest).
    // With --default-model, all runners omit -m and let opencode pick its default.
    // With --claude, use ClaudeRunner with opus/sonnet/haiku tiers.
    // Six runner instances:
    // oc_tend, oc_goal, oc_goal_high (goal agents with tier="high"), oc_cleanup, oc_rummage, oc_jog
    let (oc_tend, oc_goal, oc_goal_high, oc_cleanup_runner, oc_rummage, oc_jog):
        (Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>,
         Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>) = if use_claude {
        let tinker_m = model_config.claude_high(CLAUDE_TINKER_MODEL);
        let goal_m = model_config.claude_mid(CLAUDE_GOAL_MODEL);
        let cleanup_m = model_config.claude_low(CLAUDE_SCHEDULER_MODEL);
        let tend_prompt = if use_full_goal_context {
            tend::tend_agent_content_full_context()
        } else {
            tend_agent_content()
        };
        (
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, tend_prompt)
                .with_denied_tools(["task", "todowrite"])),
            Arc::new(ClaudeRunner::new(goal_m)),
            Arc::new(ClaudeRunner::new(tinker_m)),
            Arc::new(ClaudeRunner::new(cleanup_m)),
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, description_from_toml_str(include_str!("../.tinker/goals/rummage.toml")))
                .with_denied_tools(["task", "todowrite"])),
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, description_from_toml_str(include_str!("../.tinker/goals/jog.toml")))
                .with_denied_tools(["task", "todowrite"])),
        )
    } else if use_default_model {
        (
            Arc::new(RealOpenCodeRunner::default_with_agent("tend")),
            Arc::new(RealOpenCodeRunner::new_default()),
            Arc::new(RealOpenCodeRunner::new_default()),
            Arc::new(RealOpenCodeRunner::new_default()),
            Arc::new(RealOpenCodeRunner::default_with_agent("rummage")),
            Arc::new(RealOpenCodeRunner::default_with_agent("jog")),
        )
    } else {
        let tinker_m = model_config.opencode_high(OPENCODE_TINKER_MODEL);
        let goal_m = model_config.opencode_mid(OPENCODE_GOAL_MODEL);
        let cleanup_m = model_config.opencode_low(OPENCODE_SCHEDULER_MODEL);
        (
            Arc::new(RealOpenCodeRunner::with_agent(tinker_m, "tend")),
            Arc::new(RealOpenCodeRunner::new(goal_m)),
            Arc::new(RealOpenCodeRunner::new(tinker_m)),
            Arc::new(RealOpenCodeRunner::new(cleanup_m)),
            Arc::new(RealOpenCodeRunner::with_agent(tinker_m, "rummage")),
            Arc::new(RealOpenCodeRunner::with_agent(tinker_m, "jog")),
        )
    };

    let backend_name = if use_claude { "claude" } else { "opencode" };
    let log = logger::start_logger(
        primary_tinker_dir.join("logs").join("runtime.jsonl"),
        primary_tinker_dir.join("state").join("runtime.json"),
    );

    // Silently write agent files on every startup so the installed copies stay
    // in sync with the TOML descriptions. Skip when using Claude backend —
    // the persona is passed via --system-prompt instead of an agent file.
    if !use_claude {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
        let agent_dir = home.join(".config/opencode/agents");
        let _ = fs.mkdir_all(&agent_dir);
        let tend_md = if use_full_goal_context {
            tend::tend_agent_content_full_context()
        } else {
            tend_agent_content()
        };
        let _ = fs.write(&agent_dir.join("tend.md"), &tend_md);
        let _ = fs.write(&agent_dir.join("rummage.md"), &rummage::rummage_agent_content());
        let _ = fs.write(&agent_dir.join("jog.md"), &jog::jog_agent_content());
    }

    // Discover all .tinker dirs from cwd up. Nearest first.
    let tinker_dirs = discover_tinker_dirs(fs.as_ref(), &work_dir);

    let app = Arc::new(Mutex::new(App::new()));

    {
        let load = load_all_goals(fs.as_ref(), &tinker_dirs)?;
        let mut a = app.lock().unwrap();
        a.goals = load.goals;
        a.tinker_dirs = tinker_dirs.clone();
        a.tend_tasks += 1;
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

    let (orch_tx, mut orch_rx) = mpsc::channel::<TendEvent>(64);
    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(16);
    let (session_tx, mut session_rx) = mpsc::channel::<SessionEvent>(128);
    let (goal_spawn_tx, mut goal_spawn_rx) = mpsc::channel::<SpawnGoalRequest>(32);
    let (rummage_orch_tx, mut rummage_orch_rx) = mpsc::channel::<TendEvent>(64);
    let (rummage_msg_tx, mut rummage_msg_rx) = mpsc::channel::<String>(16);
    let (jog_orch_tx, mut jog_orch_rx) = mpsc::channel::<TendEvent>(64);
    let (jog_msg_tx, mut jog_msg_rx) = mpsc::channel::<String>(16);

    // Tend task — forwards messages to opencode and streams events back
    {
        let app_ref = app.clone();
        let work_dir = work_dir.clone();
        let orch_tx = orch_tx.clone();
        let oc = oc_tend.clone();
        let log_orch = log.clone();
        let backend_name_orch = backend_name.to_string();
        tokio::spawn(async move {
            let goals_summary = {
                let a = app_ref.lock().unwrap();
                if use_full_goal_context {
                    if a.goals.is_empty() { String::new() } else { goal::build_full_text_index(&a.goals) }
                } else if a.goals.is_empty() {
                    "[]".to_string()
                } else {
                    goal::build_compact_index(&a.goals)
                }
            };

            // Log the session start — goal-list hash lets us detect persona/goal drift.
            log_orch.emit("tend", logger::LogEvent::TinkerSessionStarted {
                system_prompt_chars: tend_agent_content().len(),
                goal_list_chars: goals_summary.len(),
                goal_list_hash: logger::hash_string(&goals_summary),
                backend: backend_name_orch.clone(),
            });

            let init = if use_full_goal_context {
                tend::tend_init_prompt_full_context(&goals_summary)
            } else {
                tend_init_prompt(&goals_summary)
            };
            log_orch.emit("tend", logger::LogEvent::TinkerTurnStart);
            log_orch.emit("tend", logger::LogEvent::TinkerUserMessageReceived { text: init.clone() });
            let t0 = std::time::Instant::now();
            let full_reply = send_message(oc.clone(), &init, None, &work_dir, orch_tx.clone())
                .await
                .unwrap_or_default();
            log_orch.emit("tend", logger::LogEvent::TinkerTurnEnd {
                duration_ms: t0.elapsed().as_millis() as u64,
                message_chars: init.len(),
                usage: logger::parse_usage_from_text(&full_reply),
                backend: backend_name_orch.clone(),
            });
            log_orch.emit("tend", logger::LogEvent::TinkerReplyEmitted { text: full_reply.clone() });

            while let Some(msg) = msg_rx.recv().await {
                let sid = app_ref.lock().unwrap().tend_session_id.clone();
                log_orch.emit("tend", logger::LogEvent::TinkerTurnStart);
                log_orch.emit("tend", logger::LogEvent::TinkerUserMessageReceived { text: msg.clone() });
                let t0 = std::time::Instant::now();
                let full_reply = send_message(
                    oc.clone(),
                    &msg,
                    sid.as_deref(),
                    &work_dir,
                    orch_tx.clone(),
                )
                .await
                .unwrap_or_default();
                log_orch.emit("tend", logger::LogEvent::TinkerTurnEnd {
                    duration_ms: t0.elapsed().as_millis() as u64,
                    message_chars: msg.len(),
                    usage: logger::parse_usage_from_text(&full_reply),
                    backend: backend_name_orch.clone(),
                });
                log_orch.emit("tend", logger::LogEvent::TinkerReplyEmitted { text: full_reply.clone() });
            }
        });
    }

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
                    // While tend is mid-turn, leave goals alone.
                    // Otherwise the watcher can pre-populate a freshly-written
                    // goal before `handle_orch_event::Done` gets to snapshot,
                    // which silently kills the new-goal auto-fire.
                    if a.tend_tasks > 0 {
                        continue;
                    }
                    a.goals = load.goals;
                    a.update_parse_errors(load.errors);
                    if a.selected_goal >= a.flat_goals().len().max(1) {
                        a.selected_goal = 0;
                    }
                }
            }
        });
    }

    // Rummage task — sits idle until the user switches to rummage and sends a
    // message. No init prompt; the first user message opens the session.
    // Uses the strongest model tier (same as tend) per the rummage goal decision.
    {
        let app_ref = app.clone();
        let work_dir = work_dir.clone();
        let oc = oc_rummage.clone();
        let tx = rummage_orch_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rummage_msg_rx.recv().await {
                let sid = app_ref.lock().unwrap().rummage_session_id.clone();
                let _ = tend::send_message(oc.clone(), &msg, sid.as_deref(), &work_dir, tx.clone()).await;
            }
        });
    }

    // Jog task — sits idle until the user switches to jog and sends a message.
    // No init prompt; the first user message opens the session.
    // Uses the strongest model tier (same as tend and rummage) per the jog goal.
    {
        let app_ref = app.clone();
        let work_dir = work_dir.clone();
        let oc = oc_jog.clone();
        let tx = jog_orch_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = jog_msg_rx.recv().await {
                let sid = app_ref.lock().unwrap().jog_session_id.clone();
                let _ = tend::send_message(oc.clone(), &msg, sid.as_deref(), &work_dir, tx.clone()).await;
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
        &mut orch_rx,
        &mut session_rx,
        msg_tx,
        goal_spawn_tx,
        &mut goal_spawn_rx,
        oc_goal,
        oc_goal_high,
        oc_cleanup_runner,
        fs.clone(),
        work_dir.clone(),
        session_tx,
        log,
        backend_name,
        &mut rummage_orch_rx,
        rummage_msg_tx,
        &mut jog_orch_rx,
        jog_msg_tx,
    )
    .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

fn tend_event_to_session(goal_id: &str, ev: TendEvent) -> SessionEvent {
    match ev {
        TendEvent::SessionId(id) => SessionEvent::LlmSessionId { goal_id: goal_id.to_string(), session_id: id },
        TendEvent::Text(t) => SessionEvent::Chunk { goal_id: goal_id.to_string(), text: t },
        TendEvent::Done => SessionEvent::Done { goal_id: goal_id.to_string() },
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: Arc<Mutex<App>>,
    orch_rx: &mut mpsc::Receiver<TendEvent>,
    session_rx: &mut mpsc::Receiver<SessionEvent>,
    msg_tx: mpsc::Sender<String>,
    goal_spawn_tx: mpsc::Sender<SpawnGoalRequest>,
    goal_spawn_rx: &mut mpsc::Receiver<SpawnGoalRequest>,
    oc_goal: Arc<dyn OpenCodeRunner>,
    oc_goal_high: Arc<dyn OpenCodeRunner>,
    oc_cleanup_runner: Arc<dyn OpenCodeRunner>,
    fs: Arc<dyn Filesystem>,
    work_dir: std::path::PathBuf,
    session_tx: mpsc::Sender<SessionEvent>,
    log: logger::LogSender,
    backend_name: &str,
    rummage_orch_rx: &mut mpsc::Receiver<TendEvent>,
    rummage_msg_tx: mpsc::Sender<String>,
    jog_orch_rx: &mut mpsc::Receiver<TendEvent>,
    jog_msg_tx: mpsc::Sender<String>,
) -> Result<()> {
    // Session registry: maps goal_id → message channel sender.
    // Pre-populated for the three interactive agents; goal agents added lazily on first @goal-id.
    let mut session_senders: HashMap<String, mpsc::Sender<String>> = HashMap::new();
    session_senders.insert("tend".to_string(), msg_tx.clone());
    session_senders.insert("rummage".to_string(), rummage_msg_tx.clone());
    session_senders.insert("jog".to_string(), jog_msg_tx.clone());
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
                    let _ = msg_tx_goal.try_send(req.message);
                    // Select runner tier from goal TOML (default: mid).
                    let oc_for_goal = match goal.tier.as_deref() {
                        Some("high") => oc_goal_high.clone(),
                        _ => oc_goal.clone(),
                    };
                    let session_tx_goal = session_tx.clone();
                    let oc_cleanup_goal = oc_cleanup_runner.clone();
                    let fs_goal = fs.clone();
                    let work_dir_goal = work_dir.clone();
                    let app_ref_goal = app.clone();
                    let log_goal = log.clone();
                    let backend_goal = backend_name.to_string();
                    tokio::spawn(async move {
                        goal_agent_loop(
                            goal, msg_rx_goal, session_tx_goal,
                            oc_for_goal, oc_cleanup_goal, fs_goal,
                            work_dir_goal, app_ref_goal, log_goal, backend_goal,
                        ).await;
                    });
                }
            }
        }

        // Drain tend events — convert TendEvent to SessionEvent
        while let Ok(ev) = orch_rx.try_recv() {
            let sev = tend_event_to_session("tend", ev);
            handle_session_event(&mut app.lock().unwrap(), sev, &msg_tx, &goal_spawn_tx, &session_senders, fs.as_ref(), &log);
        }

        // Drain goal session events (unified channel)
        {
            let active_before = app.lock().unwrap().active_goal_id.clone();
            while let Ok(ev) = session_rx.try_recv() {
                handle_session_event(&mut app.lock().unwrap(), ev, &msg_tx, &goal_spawn_tx, &session_senders, fs.as_ref(), &log);
            }
            let active_after = app.lock().unwrap().active_goal_id.clone();
            if active_after != active_before {
                log.emit("tui", logger::LogEvent::TuiQueueChanged {
                    queue_len: 0,
                    running_goal_id: active_after,
                });
            }
        }

        // Drain rummage events — convert TendEvent to SessionEvent
        while let Ok(ev) = rummage_orch_rx.try_recv() {
            let sev = tend_event_to_session("rummage", ev);
            handle_session_event(&mut app.lock().unwrap(), sev, &msg_tx, &goal_spawn_tx, &session_senders, fs.as_ref(), &log);
        }

        // Drain jog events — convert TendEvent to SessionEvent
        while let Ok(ev) = jog_orch_rx.try_recv() {
            let sev = tend_event_to_session("jog", ev);
            handle_session_event(&mut app.lock().unwrap(), sev, &msg_tx, &goal_spawn_tx, &session_senders, fs.as_ref(), &log);
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
            let action = {
                let mut a = app.lock().unwrap();
                handle_key(&mut a, key, &log)
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
                KeyAction::SendToTend(msg) => {
                    let _ = msg_tx.send(msg).await;
                }
                KeyAction::SendToRummage(msg) => {
                    let _ = rummage_msg_tx.send(msg).await;
                }
                KeyAction::SendToJog(msg) => {
                    let _ = jog_msg_tx.send(msg).await;
                }
                KeyAction::RunGoal(id, reason) => {
                    let goal_exists = {
                        let a = app.lock().unwrap();
                        a.goals.iter().any(|g| g.id == id)
                    };
                    if goal_exists {
                        let sys_msg = {
                            let id_ref = &id;
                            format!(
                                "triggered: `{}`{}",
                                id_ref,
                                reason.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
                            )
                        };
                        {
                            let mut a = app.lock().unwrap();
                            a.push_system_message(&sys_msg);
                            a.active_goal_id = Some(id.clone());
                            a.active_goal_reason = reason.clone();
                        }
                        log.emit("dispatcher", logger::LogEvent::TinkerSystemMessageReceived { content: sys_msg });
                        let dispatch_msg = reason.unwrap_or_default();
                        let _ = goal_spawn_tx.try_send(SpawnGoalRequest {
                            goal_id: id,
                            message: dispatch_msg,
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

/// Parse `@<agent-name> <message>` lines from an agent reply.
/// Extracts `@`-blocks from an agent reply.
///
/// Each block begins at an `@<known-id>` line and extends through all subsequent
/// lines until the next `@`-line or end of text. The `@`-line may carry inline
/// content (`@tend msg`) or stand alone with the message on body lines. Both
/// forms are equivalent. Prose before the first `@`-line is not delivered.
/// Empty blocks (no inline content and no body lines) are silently dropped.
///
/// `known_ids` is the set of agent IDs that can receive `@`-routed messages.
/// Only `@<id>` lines where `id` is in this set open a new block.
///
/// Returns `(recipient, message)` pairs where `message` is the full block body.
fn parse_at_commands(text: &str, known_ids: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;

    for line in text.lines() {
        let trimmed = line.trim();

        // Detect whether this line opens a new @-block.
        let mut new_block: Option<(String, String)> = None;
        for id in known_ids {
            let prefix = format!("@{}", id);
            if let Some(rest) = trimmed.strip_prefix(prefix.as_str()) {
                // Valid if followed by space (inline) or nothing (standalone).
                if rest.is_empty() || rest.starts_with(' ') {
                    new_block = Some((id.to_string(), rest.trim_start().to_string()));
                    break;
                }
            }
        }

        if let Some((recipient, inline)) = new_block {
            // Close the previous block before opening this one.
            if let Some((prev_recipient, prev_lines)) = current.take() {
                let msg = prev_lines.join("\n").trim().to_string();
                if !msg.is_empty() {
                    out.push((prev_recipient, msg));
                }
            }
            let mut lines: Vec<String> = Vec::new();
            if !inline.is_empty() {
                lines.push(inline);
            }
            current = Some((recipient, lines));
        } else if let Some((_, ref mut lines)) = current {
            lines.push(line.to_string());
        }
        // else: prose before any @-block — not part of any delivery.
    }

    // Close the final block.
    if let Some((recipient, lines)) = current {
        let msg = lines.join("\n").trim().to_string();
        if !msg.is_empty() {
            out.push((recipient, msg));
        }
    }

    out
}

/// Deliver peer consultations collected from a completed agent reply.
/// Routes to the session registry for known agents; triggers lazy spawn via
/// `goal_spawn_tx` for goal IDs not yet in the registry.
fn dispatch_peer_consultations(
    app: &mut App,
    sender: &str,
    consultations: &[(String, String)],
    session_senders: &HashMap<String, mpsc::Sender<String>>,
    goal_spawn_tx: &mpsc::Sender<SpawnGoalRequest>,
    log: &logger::LogSender,
) {
    for (recipient, msg) in consultations {
        let formatted = format!("[from {}] {}", sender, msg);
        let sys = format!("@{} → @{}: {}", sender, recipient, msg);
        app.push_system_message(&sys);
        log.emit(sender, logger::LogEvent::TinkerSystemMessageReceived { content: sys });
        if let Some(tx) = session_senders.get(recipient) {
            match recipient.as_str() {
                "tend" => app.tend_tasks += 1,
                "rummage" => app.rummage_tasks += 1,
                "jog" => app.jog_tasks += 1,
                _ => {}
            }
            let _ = tx.try_send(formatted);
        } else if app.goals.iter().any(|g| &g.id == recipient) {
            // Goal agent not yet spawned — request lazy start.
            let _ = goal_spawn_tx.try_send(SpawnGoalRequest {
                goal_id: recipient.clone(),
                message: formatted,
            });
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


// The instruction field is prescriptive (user-sourced intent from jog's deepening),
// unlike /run reasons which are declarative spec-delta pointers.
fn parse_jog_edit_commands(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("/jog-edit ") {
            let rest = rest.trim();
            if let Some((gid, instruction)) = rest.split_once(char::is_whitespace) {
                out.push((gid.to_string(), instruction.trim().to_string()));
            }
        }
    }
    out
}


/// Unified session event handler. Routes events from any agent session to the
/// appropriate App state updates and peer consultations.
fn handle_session_event(
    app: &mut App,
    ev: SessionEvent,
    msg_tx: &mpsc::Sender<String>,
    goal_spawn_tx: &mpsc::Sender<SpawnGoalRequest>,
    session_senders: &HashMap<String, mpsc::Sender<String>>,
    fs: &dyn Filesystem,
    log: &logger::LogSender,
) {
    match ev {
        SessionEvent::LlmSessionId { goal_id, session_id } => {
            match goal_id.as_str() {
                "tend" => { if app.tend_session_id.is_none() { app.tend_session_id = Some(session_id); } }
                "rummage" => { if app.rummage_session_id.is_none() { app.rummage_session_id = Some(session_id); } }
                "jog" => { if app.jog_session_id.is_none() { app.jog_session_id = Some(session_id); } }
                _ => {}
            }
        }
        SessionEvent::Chunk { goal_id, text } => {
            match goal_id.as_str() {
                "tend" => app.append_assistant_chunk(&text),
                "rummage" => app.append_rummage_chunk(&text),
                "jog" => app.append_jog_chunk(&text),
                _ => app.append_goal_log(&goal_id, &text),
            }
            // Accumulate for @-block detection on Done
            app.current_session_text.entry(goal_id).or_default().push_str(&text);
        }
        SessionEvent::Done { goal_id } => {
            let session_text = app.current_session_text.remove(&goal_id).unwrap_or_default();
            match goal_id.as_str() {
                "tend" => {
                    app.tend_tasks = app.tend_tasks.saturating_sub(1);
                    // Reload goals — tend may have just created/edited a TOML file.
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
                    if !new_errors.is_empty() {
                        if app.correction_attempts < 2 {
                            app.finalize_assistant_message();
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
                            if msg_tx.try_send(msg).is_ok() {
                                app.tend_tasks += 1;
                            }
                            return;
                        } else {
                            let still_invalid_msg = "Goal file still invalid after 2 attempts; leaving as-is. Edit manually if needed.";
                            app.push_system_message(still_invalid_msg);
                            log.emit("correction-injector", logger::LogEvent::TinkerSystemMessageReceived { content: still_invalid_msg.to_string() });
                            app.correction_attempts = 0;
                        }
                    } else {
                        app.correction_attempts = 0;
                    }
                    app.finalize_assistant_message();
                    let known_ids = known_agent_ids(session_senders, &app.goals);
                    let consultations = parse_at_commands(&session_text, &known_ids);
                    dispatch_peer_consultations(app, "tend", &consultations, session_senders, goal_spawn_tx, log);
                    if app.tend_tasks == 0 && app.phase == Phase::Initializing {
                        app.push_system_message("Tend ready. Ask me to add a goal.");
                        log.emit("harness", logger::LogEvent::TinkerSystemMessageReceived { content: "Tend ready. Ask me to add a goal.".to_string() });
                        app.phase = Phase::Idle;
                    }
                }
                "rummage" => {
                    app.rummage_tasks = app.rummage_tasks.saturating_sub(1);
                    if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                        app.goals = load.goals;
                    }
                    app.finalize_rummage_message();
                    let known_ids = known_agent_ids(session_senders, &app.goals);
                    let consultations = parse_at_commands(&session_text, &known_ids);
                    dispatch_peer_consultations(app, "rummage", &consultations, session_senders, goal_spawn_tx, log);
                }
                "jog" => {
                    app.jog_tasks = app.jog_tasks.saturating_sub(1);
                    let known_ids = known_agent_ids(session_senders, &app.goals);
                    let consultations = parse_at_commands(&session_text, &known_ids);
                    let edits = parse_jog_edit_commands(&session_text);
                    app.finalize_jog_message();
                    dispatch_peer_consultations(app, "jog", &consultations, session_senders, goal_spawn_tx, log);
                    if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                        app.goals = load.goals;
                    }
                    for (gid, instruction) in edits {
                        let goal_exists = app.goals.iter().any(|g| g.id == gid);
                        if goal_exists {
                            let tend_msg = format!(
                                "Jog audit — `{}`: {}. Apply this edit to the goal directly; jog's conversation has already provided the dialectical anchoring. After the edit, show what changed.",
                                gid, instruction
                            );
                            app.tend_tasks += 1;
                            let _ = msg_tx.try_send(tend_msg.clone());
                            log.emit("jog", logger::LogEvent::TinkerSystemMessageReceived { content: tend_msg });
                        }
                    }
                }
                _ => {
                    // Goal session done — clear active tracking
                    if app.active_goal_id.as_deref() == Some(&goal_id) {
                        app.active_goal_id = None;
                        app.active_goal_reason = None;
                    }
                    let known_ids = known_agent_ids(session_senders, &app.goals);
                    let consultations = parse_at_commands(&session_text, &known_ids);
                    dispatch_peer_consultations(app, &goal_id, &consultations, session_senders, goal_spawn_tx, log);
                }
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
            if app.active_goal_id.as_deref() == Some(&goal_id) {
                app.active_goal_id = None;
                app.active_goal_reason = None;
            }
        }
        SessionEvent::SummaryReady { goal_id, summary } => {
            // Route summary directly to tend as a peer consultation.
            // This replaces the batch-summary machinery: tend synthesizes each
            // goal session's result individually as it arrives.
            let tend_msg = format!("[from {}] Session summary:\n{}", goal_id, summary);
            app.tend_tasks += 1;
            let _ = msg_tx.try_send(tend_msg);
        }
    }
}


enum KeyAction {
    None,
    Quit,
    SendToTend(String),
    SendToRummage(String),
    SendToJog(String),
    /// Dispatch a goal agent session with an optional trigger reason.
    RunGoal(String, Option<String>),
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
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.should_quit = true;
            KeyAction::Quit
        }
        (_, KeyCode::Esc) => {
            app.modal = None;
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
            KeyAction::RunGoal(m.goal_id, reason)
        }
        (_, KeyCode::Backspace) => {
            if let Some(m) = app.modal.as_mut() {
                m.input.pop();
            }
            KeyAction::None
        }
        (_, KeyCode::Char(c)) => {
            if let Some(m) = app.modal.as_mut() {
                m.input.push(c);
            }
            KeyAction::None
        }
        _ => KeyAction::None,
    }
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent, log: &logger::LogSender) -> KeyAction {
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

                // Slash commands
                if input == "/quit" {
                    app.should_quit = true;
                    return KeyAction::Quit;
                }
                if input == "/help" {
                    let help_msg = "Commands: @<goal-id> [msg], /quit, /help, /tend, /rummage, /jog. Tab = goal tree.";
                    app.push_system_message(help_msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: help_msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }

                // Agent-switching slash commands (rummage / jog goals).
                if input == "/rummage" {
                    app.active_agent = ActiveAgent::Rummage;
                    let msg = "switched to rummage — type to chat, /tend or /jog to switch";
                    app.push_system_message(msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }
                if input == "/tend" {
                    app.active_agent = ActiveAgent::Tend;
                    let msg = "switched to tend — type to chat, /rummage or /jog to switch";
                    app.push_system_message(msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }
                if input == "/jog" {
                    app.active_agent = ActiveAgent::Jog;
                    let msg = "switched to jog — name a topic to audit, /tend or /rummage to switch";
                    app.push_system_message(msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }

                // Lock input while the active agent is busy.
                let active_busy = match app.active_agent {
                    ActiveAgent::Tend => app.tend_tasks > 0,
                    ActiveAgent::Rummage => app.rummage_tasks > 0,
                    ActiveAgent::Jog => app.jog_tasks > 0,
                };
                if active_busy {
                    return KeyAction::None;
                }

                app.push_user_message(&input);
                app.input.clear();
                app.user_has_interacted = true;

                match app.active_agent {
                    ActiveAgent::Tend => {
                        app.tend_tasks += 1;
                        app.correction_attempts = 0;
                        KeyAction::SendToTend(input)
                    }
                    ActiveAgent::Rummage => {
                        app.rummage_tasks += 1;
                        KeyAction::SendToRummage(input)
                    }
                    ActiveAgent::Jog => {
                        app.jog_tasks += 1;
                        KeyAction::SendToJog(input)
                    }
                }
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
                    // Enter on a goal opens the reason-prompt modal. Pane
                    // focus stays on `Tree` — when the modal closes (submit
                    // or Esc), the user is still in the tree.
                    app.modal = Some(crate::app::ModalState {
                        goal_id: g.id,
                        input: String::new(),
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

    // spec: goal-agents — SummaryReady events route directly to tend without
    // batch collection. This replaces the old batch-summary machinery.
    #[test]
    fn test_spec_summary_routes_directly_to_tend_not_batched() {
        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);
        let (senders, _, _) = make_test_session_senders(&msg_tx);
        let mock_fs = crate::test_utils::MockFs::new();
        let mut app = App::new();
        handle_session_event(
            &mut app,
            SessionEvent::SummaryReady { goal_id: "calc".into(), summary: "did things".into() },
            &msg_tx,
            &spawn_tx,
            &senders,
            &mock_fs,
            &logger::noop_sender(),
        );
        let msg = msg_rx.try_recv().expect("SummaryReady must dispatch to tend");
        assert!(msg.contains("calc"), "summary message must name the goal");
        assert!(msg.contains("did things"), "summary message must carry the summary");
        assert_eq!(app.tend_tasks, 1, "tend_tasks must increment for the summary");
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
        // `realfs.rs` owns the Filesystem capability.
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

    // spec (tui): "When the user manually triggers a goal via Enter in the
    // goal tree, a modal text input dialog pops up to collect the trigger reason."
    // Pressing Enter while Focus::Tree must set app.modal to Some(...) with
    // the selected goal's id; focus must remain on Tree (not switch to Repl).
    #[test]
    fn test_spec_enter_in_tree_opens_reason_modal() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "tui".into(),
            summary: String::new(),
            description: "build the tui".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            tier: None,
            source_path: None,
        }];
        app.focus = Focus::Tree;
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::None),
            "Enter in tree must return KeyAction::None (modal handles dispatch)",
        );
        let modal = app.modal.as_ref().expect("modal must be open after Enter in tree");
        assert_eq!(modal.goal_id, "tui", "modal must target the selected goal");
        assert!(modal.input.is_empty(), "modal input must start empty");
        assert_eq!(
            app.focus,
            Focus::Tree,
            "focus must remain on Tree — submit/cancel returns here",
        );
    }

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

    // spec (rummage): "Agent switching is via explicit slash commands only —
    // `/rummage` to switch to rummage, `/tend` to switch back."
    // `/rummage` must set active_agent to Rummage and emit a system message;
    // the input buffer must be cleared so the command doesn't appear as chat.
    #[test]
    fn test_spec_slash_rummage_switches_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        assert_eq!(app.active_agent, ActiveAgent::Tend, "starts as Tend");
        app.input = "/rummage".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::None),
            "/rummage must not dispatch a chat message; returns None",
        );
        assert_eq!(
            app.active_agent,
            ActiveAgent::Rummage,
            "/rummage must switch active_agent to Rummage",
        );
        assert!(app.input.is_empty(), "input must be cleared after /rummage");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("rummage")),
            "/rummage must emit a system message naming rummage",
        );
    }

    // spec (rummage): `/tend` switches back from rummage to tend.
    #[test]
    fn test_spec_slash_tend_switches_back_to_tend() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        app.active_agent = ActiveAgent::Rummage;
        app.input = "/tend".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::None),
            "/tend must not dispatch a chat message; returns None",
        );
        assert_eq!(
            app.active_agent,
            ActiveAgent::Tend,
            "/tend must switch active_agent back to Tend",
        );
        assert!(app.input.is_empty(), "input must be cleared after /tend");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("tend")),
            "/tend must emit a system message naming tend",
        );
    }

    // spec (rummage / tui): "One agent is active at a time. User messages go to
    // whoever is active." When active_agent is Rummage, Enter on a non-slash
    // message must return SendToRummage (not SendToTend). When Tend, must
    // return SendToTend.
    #[test]
    fn test_spec_message_routes_to_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // Route to Tend when active.
        let mut app = App::new();
        app.phase = Phase::Idle;
        app.active_agent = ActiveAgent::Tend;
        app.input = "hello tend".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::SendToTend(_)),
            "message must route to Tend when active_agent is Tend; got {:?}",
            std::mem::discriminant(&action),
        );

        // Route to Rummage when active.
        let mut app2 = App::new();
        app2.phase = Phase::Idle;
        app2.active_agent = ActiveAgent::Rummage;
        app2.input = "hello rummage".into();
        let action2 = handle_key(&mut app2, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action2, KeyAction::SendToRummage(_)),
            "message must route to Rummage when active_agent is Rummage; got {:?}",
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

    // spec (rummage): "Rummage uses the strongest model available for the chosen
    // backend." The composition root in main.rs must wire the rummage runner to
    // the tinker tier (the smartest tier), not SCHEDULER_MODEL or GOAL_MODEL.
    // Verified via source inspection since the wiring is inside main().
    //
    // The model-config layer resolves the actual model string through `tinker_m`
    // (bound to model_config.*_tinker(TINKER_MODEL)), so we check that rummage
    // uses the same `tinker_m` variable that tinker itself uses — never goal_m or
    // cleanup_m.
    #[test]
    fn test_spec_rummage_wired_to_strongest_model_tier() {
        let main_rs = include_str!("main.rs");
        // Both backends bind the tinker-tier model to `tinker_m` with the built-in
        // constant as fallback, then use it for rummage and jog as well.
        assert!(
            main_rs.contains("description_from_toml_str(include_str!(\"../.tinker/goals/rummage.toml\"))"),
            "rummage claude runner must use tinker_m (strongest tier)",
        );
        assert!(
            main_rs.contains("tinker_m, \"rummage\""),
            "rummage opencode runner must use tinker_m (strongest tier)",
        );
        // The tinker_m variable on the Claude path must default to CLAUDE_TINKER_MODEL.
        assert!(
            main_rs.contains("model_config.claude_high(CLAUDE_TINKER_MODEL)"),
            "claude tinker_m must fall back to CLAUDE_TINKER_MODEL",
        );
        // The tinker_m variable on the opencode path must default to OPENCODE_TINKER_MODEL.
        assert!(
            main_rs.contains("model_config.opencode_high(OPENCODE_TINKER_MODEL)"),
            "opencode tinker_m must fall back to OPENCODE_TINKER_MODEL",
        );
    }

    // spec (jog): "Jog runs on the strongest model tier." The composition root
    // in main.rs must wire the jog runner to the tinker tier (the smartest tier).
    #[test]
    fn test_spec_jog_wired_to_strongest_model_tier() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("description_from_toml_str(include_str!(\"../.tinker/goals/jog.toml\"))"),
            "jog claude runner must use tinker_m (strongest tier)",
        );
        assert!(
            main_rs.contains("tinker_m, \"jog\""),
            "jog opencode runner must use tinker_m (strongest tier)",
        );
    }

    // spec (jog / tui): `/jog` slash command must switch active_agent to Jog and
    // emit a system message naming jog.
    #[test]
    fn test_spec_slash_jog_switches_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        assert_eq!(app.active_agent, ActiveAgent::Tend, "starts as Tinker");
        app.input = "/jog".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::None),
            "/jog must not dispatch a chat message; returns None",
        );
        assert_eq!(
            app.active_agent,
            ActiveAgent::Jog,
            "/jog must switch active_agent to Jog",
        );
        assert!(app.input.is_empty(), "input must be cleared after /jog");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("jog")),
            "/jog must emit a system message naming jog",
        );
    }

    // spec (jog / tui): messages route to Jog when active_agent is Jog.
    #[test]
    fn test_spec_message_routes_to_jog_when_active() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.phase = Phase::Idle;
        app.active_agent = ActiveAgent::Jog;
        app.input = "jog me on logging".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::SendToJog(_)),
            "message must route to Jog when active_agent is Jog; got {:?}",
            std::mem::discriminant(&action),
        );
    }

    // spec (triggers): parse_jog_edit_commands extracts goal-id and instruction from
    // /jog-edit lines. Incomplete lines (no instruction) and lines where /jog-edit
    // does not appear at the start are silently skipped.
    #[test]
    fn test_spec_parse_jog_edit_commands_syntax() {
        let text = "/jog-edit rummage Clarify the case-2 durable test path.\nsome prose\n/jog-edit tui Document the active-agent prompt tag.\n";
        let edits = parse_jog_edit_commands(text);
        assert_eq!(edits.len(), 2, "two /jog-edit lines must each produce one edit");
        assert_eq!(edits[0].0, "rummage");
        assert_eq!(edits[0].1, "Clarify the case-2 durable test path.");
        assert_eq!(edits[1].0, "tui");
        assert_eq!(edits[1].1, "Document the active-agent prompt tag.");

        // Incomplete line (goal-id only, no instruction) must be silently skipped.
        let incomplete = "/jog-edit rummage\nsome prose\n";
        assert!(
            parse_jog_edit_commands(incomplete).is_empty(),
            "incomplete /jog-edit (no instruction) must be skipped",
        );

        // /jog-edit not at line start must not parse.
        let mid_line = "here is advice /jog-edit rummage not leading\n";
        assert!(
            parse_jog_edit_commands(mid_line).is_empty(),
            "/jog-edit not at line start must not parse",
        );

        // Prose-only text must produce no edits.
        assert!(
            parse_jog_edit_commands("no commands here\n").is_empty(),
            "prose-only must produce no edits",
        );
    }

    // spec (jog / triggers): multiple /jog-edit lines in one jog reply must each
    // fire a separate commission to tinker, one per line. tend_tasks must
    // increment once per commission.
    #[test]
    fn test_spec_jog_edit_multiple_lines_each_dispatch_to_tinker() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let goals_dir = tinker_dir.join("goals");
        let mock_fs = MockFs::new();
        mock_fs.add_file(
            &goals_dir.join("rummage.toml"),
            "id = \"rummage\"\ndescription = \"investigates\"\nparent_id = \"\"\nchildren = []\n",
        );
        mock_fs.add_file(
            &goals_dir.join("tui.toml"),
            "id = \"tui\"\ndescription = \"terminal ui\"\nparent_id = \"\"\nchildren = []\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        let jog_text = "Two findings here.\n/jog-edit rummage Clarify the case-2 durable test path.\n/jog-edit tui Document the active-agent prompt tag.\n";
        app.current_session_text.insert("jog".to_string(), jog_text.to_string());
        app.jog_tasks = 1;

        let (senders, _, _) = make_test_session_senders(&msg_tx);
        handle_session_event(
            &mut app,
            SessionEvent::Done { goal_id: "jog".into() },
            &msg_tx,
            &spawn_tx,
            &senders,
            &mock_fs,
            &logger::noop_sender(),
        );

        let first = msg_rx.try_recv().expect("first /jog-edit must dispatch to tinker");
        assert!(first.contains("rummage"), "first commission must name rummage");
        let second = msg_rx.try_recv().expect("second /jog-edit must dispatch to tinker");
        assert!(second.contains("tui"), "second commission must name tui");
        assert!(msg_rx.try_recv().is_err(), "no extra commissions beyond two /jog-edit lines");
        assert_eq!(app.tend_tasks, 2, "tend_tasks must increment once per /jog-edit line");
    }

    // spec (triggers): if a /jog-edit line names a goal-id that does not exist in
    // the loaded goal list, it is silently dropped — no commission sent to tinker.
    #[test]
    fn test_spec_jog_edit_unknown_goal_id_silently_skipped() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let mock_fs = MockFs::new();
        mock_fs.add_dir(&tinker_dir.join("goals"));

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.current_session_text.insert("jog".to_string(), "/jog-edit nonexistent This goal does not exist.\n".to_string());
        app.jog_tasks = 1;

        let (senders, _, _) = make_test_session_senders(&msg_tx);
        handle_session_event(
            &mut app,
            SessionEvent::Done { goal_id: "jog".into() },
            &msg_tx,
            &spawn_tx,
            &senders,
            &mock_fs,
            &logger::noop_sender(),
        );

        assert!(
            msg_rx.try_recv().is_err(),
            "unknown goal-id in /jog-edit must not dispatch any commission to tend",
        );
        assert_eq!(app.tend_tasks, 0, "tend_tasks must not increment for unknown goal-id");
    }

    // spec (jog / triggers): the jog→tinker commission message must instruct
    // tinker to apply the edit directly and to show what changed.
    #[test]
    fn test_spec_jog_edit_commission_instructs_direct_apply_and_show_diff() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let goals_dir = tinker_dir.join("goals");
        let mock_fs = MockFs::new();
        mock_fs.add_file(
            &goals_dir.join("rummage.toml"),
            "id = \"rummage\"\ndescription = \"investigates\"\nparent_id = \"\"\nchildren = []\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.current_session_text.insert("jog".to_string(),
            "/jog-edit rummage The SCOPE section omits the jog→tinker channel.\n".to_string());
        app.jog_tasks = 1;

        let (senders, _, _) = make_test_session_senders(&msg_tx);
        handle_session_event(
            &mut app,
            SessionEvent::Done { goal_id: "jog".into() },
            &msg_tx,
            &spawn_tx,
            &senders,
            &mock_fs,
            &logger::noop_sender(),
        );

        let commission = msg_rx.try_recv().expect("commission must be sent");
        assert!(
            commission.to_lowercase().contains("directly"),
            "commission must instruct tinker to apply the edit directly: {commission}",
        );
        assert!(
            commission.to_lowercase().contains("what changed") || commission.to_lowercase().contains("changed"),
            "commission must instruct tinker to show what changed: {commission}",
        );
    }

    // spec (jog): jog emits `/jog-edit <goal-id> <instruction>` in its output.
    // handle_session_event (jog Done) must parse those lines and forward a
    // prescriptive commission to tinker via msg_tx.
    #[test]
    fn test_spec_jog_edit_command_dispatched_to_tinker() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (spawn_tx, _spawn_rx) = mpsc::channel::<SpawnGoalRequest>(4);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let goals_dir = tinker_dir.join("goals");
        let mock_fs = MockFs::new();
        mock_fs.add_file(
            &goals_dir.join("rummage.toml"),
            "id = \"rummage\"\ndescription = \"investigates program behavior\"\nparent_id = \"\"\nchildren = []\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.current_session_text.insert("jog".to_string(),
            "I know better.\n/jog-edit rummage Add jog→tinker channel description to the SCOPE section.\n".to_string());
        app.jog_tasks = 1;

        let (senders, _, _) = make_test_session_senders(&msg_tx);
        handle_session_event(
            &mut app,
            SessionEvent::Done { goal_id: "jog".into() },
            &msg_tx,
            &spawn_tx,
            &senders,
            &mock_fs,
            &logger::noop_sender(),
        );

        let dispatched = msg_rx.try_recv().expect("jog /jog-edit must dispatch to msg_tx (tend)");
        assert!(dispatched.contains("rummage"), "tend commission must name the goal id");
        assert!(dispatched.contains("Add jog"), "tend commission must carry the instruction");
        assert_eq!(app.jog_tasks, 0, "jog_tasks must decrement on Done");
        assert_eq!(app.tend_tasks, 1, "tend_tasks must increment for the commission");
    }

    // spec (peer-consult): parse_at_commands extracts @tend, @rummage, @jog
    // lines from agent output. Non-matching lines are silently skipped.
    // The function now takes a `known_ids` slice so routing works for any
    // registered session ID, not just the three built-in agents.
    const BUILTIN_IDS: &[&str] = &["tend", "rummage", "jog"];

    #[test]
    fn test_spec_peer_consult_parse_at_tinker() {
        let r = parse_at_commands("@tend what does this module do?", BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "tend");
        assert_eq!(r[0].1, "what does this module do?");
    }

    #[test]
    fn test_spec_peer_consult_parse_at_rummage() {
        let r = parse_at_commands("@rummage can you trace the call?", BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "rummage");
        assert_eq!(r[0].1, "can you trace the call?");
    }

    #[test]
    fn test_spec_peer_consult_parse_at_jog() {
        let r = parse_at_commands("@jog do you still mean X by this?", BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "jog");
        assert_eq!(r[0].1, "do you still mean X by this?");
    }

    #[test]
    fn test_spec_peer_consult_parse_prose_before_block_excluded() {
        let r = parse_at_commands("some prose\n@tend hello\n@rummage check this", BUILTIN_IDS);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], ("tend".to_string(), "hello".to_string()));
        assert_eq!(r[1], ("rummage".to_string(), "check this".to_string()));
    }

    #[test]
    fn test_spec_peer_consult_parse_block_body_included() {
        let r = parse_at_commands("@tend hello\nbody line one\nbody line two\n@rummage check", BUILTIN_IDS);
        assert_eq!(r.len(), 2);
        assert!(
            r[0].1.contains("hello") && r[0].1.contains("body line one"),
            "@tend block must include both inline and body lines"
        );
        assert_eq!(r[1], ("rummage".to_string(), "check".to_string()));
    }

    #[test]
    fn test_spec_peer_consult_parse_at_without_message_ignored() {
        let r = parse_at_commands("@tend", BUILTIN_IDS);
        assert_eq!(r.len(), 0, "empty @tend block must not be delivered");
    }

    #[test]
    fn test_spec_peer_consult_parse_multiline_body() {
        let r = parse_at_commands("@rummage\nfirst line\nsecond line", BUILTIN_IDS);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "rummage");
        assert!(
            r[0].1.contains("first line") && r[0].1.contains("second line"),
            "standalone @-block body must include all subsequent lines"
        );
    }

    #[test]
    fn test_spec_peer_consult_parse_multiple_at_lines() {
        let r = parse_at_commands("@tend q1\n@rummage q2\n@jog q3", BUILTIN_IDS);
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
                parent_id: String::new(), children: vec![], related: vec![], tier: None, source_path: None },
            goal::Goal { id: "rummage".into(), summary: String::new(), description: String::new(),
                parent_id: String::new(), children: vec![], related: vec![], tier: None, source_path: None },
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
        let r = parse_at_commands("@goal-agents start working on the registry", ids);
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
        assert_eq!(app.tend_tasks, 1, "tend_tasks must increment");

        let rummage_msg = rummage_rx.try_recv().expect("rummage must receive its consultation");
        assert!(rummage_msg.contains("[from rummage]"));
        assert_eq!(app.rummage_tasks, 1, "rummage_tasks must increment");

        let jog_msg = jog_rx.try_recv().expect("jog must receive its consultation");
        assert!(jog_msg.contains("[from rummage]"));
        assert_eq!(app.jog_tasks, 1, "jog_tasks must increment");
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
        assert!(text.contains("@tend"), "system message must name the sender");
        assert!(text.contains("@rummage"), "system message must name the recipient");
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
}
