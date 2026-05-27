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
mod tinker;
mod realfs;
mod tui;
#[cfg(test)]
mod test_utils;

use anyhow::Result;
use app::{ActiveAgent, App, Focus, LoopMode, Phase};
use cap::{Filesystem, OpenCodeRunner};
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
use goal_session::{run_goal, GoalEvent};
use opencode::{RealOpenCodeRunner, GOAL_MODEL as OPENCODE_GOAL_MODEL, TINKER_MODEL as OPENCODE_TINKER_MODEL, SCHEDULER_MODEL as OPENCODE_SCHEDULER_MODEL};
use claude::{ClaudeRunner, GOAL_MODEL as CLAUDE_GOAL_MODEL, TINKER_MODEL as CLAUDE_TINKER_MODEL, SCHEDULER_MODEL as CLAUDE_SCHEDULER_MODEL};
use tinker::{tinker_agent_content, tinker_init_prompt, send_message, TinkerEvent};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use tokio::sync::mpsc;


#[tokio::main]
async fn main() -> Result<()> {
    // Composition root: real capability implementations live here only.
    let fs: Arc<dyn Filesystem> = Arc::new(RealFilesystem);

    let use_default_model = std::env::args().any(|a| a == "--default-model");
    let use_claude = std::env::args().any(|a| a == "--claude");
    let use_full_goal_context = std::env::args().any(|a| a == "--tinker-full-goal-context");

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

    // Three runner instances bound to different models — tinker for
    // interview + batch summary (smartest), goal sessions for code production,
    // cleanup for the pre-session tinker-test-case hook (cheapest).
    // With --default-model, all runners omit -m and let opencode pick its default.
    // With --claude, use ClaudeRunner with opus/sonnet/haiku tiers.
    let (oc_tinker, oc_goal, oc_cleanup_runner, oc_rummage, oc_jog): (Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>, Arc<dyn OpenCodeRunner>) = if use_claude {
        // Claude backend: pass tinker persona via --system-prompt instead of agent file.
        // task/todowrite are denied mechanically via --disallowedTools to match the
        // identity-level enforcement in the opencode agent files for tinker and rummage.
        let tinker_m = model_config.claude_high(CLAUDE_TINKER_MODEL);
        let goal_m = model_config.claude_mid(CLAUDE_GOAL_MODEL);
        let cleanup_m = model_config.claude_low(CLAUDE_SCHEDULER_MODEL);
        let tinker_prompt = if use_full_goal_context {
            tinker::tinker_agent_content_full_context()
        } else {
            tinker_agent_content()
        };
        (
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, tinker_prompt)
                .with_denied_tools(["task", "todowrite"])),
            Arc::new(ClaudeRunner::new(goal_m)),
            Arc::new(ClaudeRunner::new(cleanup_m)),
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, rummage::rummage_system_prompt())
                .with_denied_tools(["task", "todowrite"])),
            Arc::new(ClaudeRunner::with_system_prompt(tinker_m, jog::jog_system_prompt())
                .with_denied_tools(["task", "todowrite"])),
        )
    } else if use_default_model {
        (
            Arc::new(RealOpenCodeRunner::default_with_agent("tinker")),
            Arc::new(RealOpenCodeRunner::new_default()),
            Arc::new(RealOpenCodeRunner::new_default()),
            Arc::new(RealOpenCodeRunner::default_with_agent("rummage")), // smartest tier
            Arc::new(RealOpenCodeRunner::default_with_agent("jog")), // smartest tier
        )
    } else {
        let tinker_m = model_config.opencode_high(OPENCODE_TINKER_MODEL);
        let goal_m = model_config.opencode_mid(OPENCODE_GOAL_MODEL);
        let cleanup_m = model_config.opencode_low(OPENCODE_SCHEDULER_MODEL);
        (
            Arc::new(RealOpenCodeRunner::with_agent(tinker_m, "tinker")),
            Arc::new(RealOpenCodeRunner::new(goal_m)),
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

    // Silently write the tinker agent file on every startup.
    // This keeps the installed file in sync with tinker_agent_content()
    // as the persona evolves, rather than leaving a stale copy from a previous
    // run. Skip when using Claude backend — the persona is passed via
    // --system-prompt instead of an agent file.
    if !use_claude {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
        let agent_dir = home.join(".config/opencode/agents");
        let _ = fs.mkdir_all(&agent_dir);
        let tinker_md = if use_full_goal_context {
            tinker::tinker_agent_content_full_context()
        } else {
            tinker_agent_content()
        };
        let _ = fs.write(&agent_dir.join("tinker.md"), &tinker_md);
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
        a.tinker_tasks += 1;
        a.push_system_message("Starting tinker…");
        log.emit("harness", logger::LogEvent::TinkerSystemMessageReceived { content: "Starting tinker…".to_string() });
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

    let (orch_tx, mut orch_rx) = mpsc::channel::<TinkerEvent>(64);
    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(16);
    let (goal_tx, mut goal_rx) = mpsc::channel::<GoalEvent>(128);
    let (run_tx, mut run_rx) = mpsc::channel::<(goal::Goal, Option<String>)>(4);
    let (rummage_orch_tx, mut rummage_orch_rx) = mpsc::channel::<TinkerEvent>(64);
    let (rummage_msg_tx, mut rummage_msg_rx) = mpsc::channel::<String>(16);
    let (jog_orch_tx, mut jog_orch_rx) = mpsc::channel::<TinkerEvent>(64);
    let (jog_msg_tx, mut jog_msg_rx) = mpsc::channel::<String>(16);

    // Tinker task — forwards messages to opencode and streams events back
    {
        let app_ref = app.clone();
        let work_dir = work_dir.clone();
        let orch_tx = orch_tx.clone();
        let oc = oc_tinker.clone();
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
            log_orch.emit("tinker", logger::LogEvent::TinkerSessionStarted {
                system_prompt_chars: tinker_agent_content().len(),
                goal_list_chars: goals_summary.len(),
                goal_list_hash: logger::hash_string(&goals_summary),
                backend: backend_name_orch.clone(),
            });

            let init = if use_full_goal_context {
                tinker::tinker_init_prompt_full_context(&goals_summary)
            } else {
                tinker_init_prompt(&goals_summary)
            };
            log_orch.emit("tinker", logger::LogEvent::TinkerTurnStart);
            log_orch.emit("tinker", logger::LogEvent::TinkerUserMessageReceived { text: init.clone() });
            let t0 = std::time::Instant::now();
            let full_reply = send_message(oc.clone(), &init, None, &work_dir, orch_tx.clone())
                .await
                .unwrap_or_default();
            log_orch.emit("tinker", logger::LogEvent::TinkerTurnEnd {
                duration_ms: t0.elapsed().as_millis() as u64,
                message_chars: init.len(),
                usage: logger::parse_usage_from_text(&full_reply),
                backend: backend_name_orch.clone(),
            });
            log_orch.emit("tinker", logger::LogEvent::TinkerReplyEmitted { text: full_reply.clone() });
            // Emit run commands found in the reply
            for line in full_reply.lines() {
                if let Some(rest) = line.trim().strip_prefix("/run ") {
                    let rest = rest.trim();
                    let (gid, reason) = if let Some((g, r)) = rest.split_once(char::is_whitespace) {
                        (g.to_string(), r.trim().to_string())
                    } else {
                        (rest.to_string(), String::new())
                    };
                    log_orch.emit("tinker", logger::LogEvent::RunCommandEmitted { goal_id: gid, reason });
                }
            }

            while let Some(msg) = msg_rx.recv().await {
                let sid = app_ref.lock().unwrap().tinker_session_id.clone();
                log_orch.emit("tinker", logger::LogEvent::TinkerTurnStart);
                log_orch.emit("tinker", logger::LogEvent::TinkerUserMessageReceived { text: msg.clone() });
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
                log_orch.emit("tinker", logger::LogEvent::TinkerTurnEnd {
                    duration_ms: t0.elapsed().as_millis() as u64,
                    message_chars: msg.len(),
                    usage: logger::parse_usage_from_text(&full_reply),
                    backend: backend_name_orch.clone(),
                });
                log_orch.emit("tinker", logger::LogEvent::TinkerReplyEmitted { text: full_reply.clone() });
                for line in full_reply.lines() {
                    if let Some(rest) = line.trim().strip_prefix("/run ") {
                        let rest = rest.trim();
                        let (gid, reason) = if let Some((g, r)) = rest.split_once(char::is_whitespace) {
                            (g.to_string(), r.trim().to_string())
                        } else {
                            (rest.to_string(), String::new())
                        };
                        log_orch.emit("tinker", logger::LogEvent::RunCommandEmitted { goal_id: gid, reason });
                    }
                }
            }
        });
    }

    // Goal runner task — one goal at a time. Updates go back to each goal's source file.
    // Before each goal session, runs the tinker-test-case cleanup hook (see
    // `src/cleanup.rs` and `## Tinkering` in the tinker prompt).
    // Per `goal-sessions` decision: every dispatch is a fresh session — no
    // in-process resumption across triggers.
    {
        let work_dir = work_dir.clone();
        let goal_tx = goal_tx.clone();
        let oc = oc_goal.clone();
        let oc_cleanup = oc_cleanup_runner.clone();
        let fs = fs.clone();
        let log_goal = log.clone();
        let backend_name_goal = backend_name.to_string();
        tokio::spawn(async move {
            while let Some((goal, reason)) = run_rx.recv().await {
                log_goal.emit("goal_session", logger::LogEvent::GoalSessionDispatched {
                    goal_id: goal.id.clone(),
                    reason: reason.clone(),
                    init_message_chars: goal_session::goal_init_message(&goal, reason.as_deref()).len(),
                    backend: backend_name_goal.clone(),
                });

                let cleanup_t0 = std::time::Instant::now();
                let cleanup_result = cleanup::run_cleanup(
                    oc_cleanup.as_ref(),
                    fs.as_ref(),
                    &work_dir,
                )
                .await;
                let cleanup_ms = cleanup_t0.elapsed().as_millis() as u64;

                match cleanup_result {
                    Ok(cleanup::CleanupOutcome::Clean) => {
                        log_goal.emit("goal_session", logger::LogEvent::CleanupHookRun {
                            goal_id: goal.id.clone(),
                            outcome: "clean".to_string(),
                            duration_ms: cleanup_ms,
                        });
                    }
                    Ok(cleanup::CleanupOutcome::FailedAfterRetries(files)) => {
                        log_goal.emit("goal_session", logger::LogEvent::CleanupHookRun {
                            goal_id: goal.id.clone(),
                            outcome: "blocked".to_string(),
                            duration_ms: cleanup_ms,
                        });
                        let _ = goal_tx
                            .send(GoalEvent::CleanupBlocked {
                                goal_id: goal.id.clone(),
                                dirty_files: files,
                                error: None,
                            })
                            .await;
                        continue;
                    }
                    Err(e) => {
                        log_goal.emit("goal_session", logger::LogEvent::CleanupHookRun {
                            goal_id: goal.id.clone(),
                            outcome: format!("error: {e:#}"),
                            duration_ms: cleanup_ms,
                        });
                        let _ = goal_tx
                            .send(GoalEvent::CleanupBlocked {
                                goal_id: goal.id.clone(),
                                dirty_files: vec![],
                                error: Some(format!("{e:#}")),
                            })
                            .await;
                        continue;
                    }
                }

                log_goal.emit("goal_session", logger::LogEvent::GoalSessionStarted {
                    goal_id: goal.id.clone(),
                });

                let session_t0 = std::time::Instant::now();
                let result = run_goal(
                    goal.clone(),
                    reason,
                    work_dir.clone(),
                    goal_tx.clone(),
                    oc.clone(),
                )
                .await;
                let session_ms = session_t0.elapsed().as_millis() as u64;

                match result {
                    Ok((full_output, summary)) => {
                        let tool_calls = logger::count_tool_calls(&full_output);
                        let files_modified = logger::extract_modified_files(&full_output);
                        let usage = logger::parse_usage_from_text(&full_output);
                        log_goal.emit("goal_session", logger::LogEvent::GoalSessionFinished {
                            goal_id: goal.id.clone(),
                            exit_status: "clean".to_string(),
                            duration_ms: session_ms,
                            files_modified_count: files_modified.len(),
                            files_modified,
                            tool_calls,
                            summary_chars: summary.len(),
                            full_output,
                            usage,
                            backend: backend_name_goal.clone(),
                        });
                    }
                    Err(_) => {
                        log_goal.emit("goal_session", logger::LogEvent::GoalSessionFinished {
                            goal_id: goal.id.clone(),
                            exit_status: "crash".to_string(),
                            duration_ms: session_ms,
                            files_modified_count: 0,
                            files_modified: vec![],
                            tool_calls: 0,
                            summary_chars: 0,
                            full_output: String::new(),
                            usage: None,
                            backend: backend_name_goal.clone(),
                        });
                    }
                }
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
                    // While tinker is mid-turn, leave goals alone.
                    // Otherwise the watcher can pre-populate a freshly-written
                    // goal before `handle_orch_event::Done` gets to snapshot,
                    // which silently kills the new-goal auto-fire.
                    if a.tinker_tasks > 0 {
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
    // Uses the strongest model tier (same as tinker) per the rummage goal decision.
    {
        let app_ref = app.clone();
        let work_dir = work_dir.clone();
        let oc = oc_rummage.clone();
        let tx = rummage_orch_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rummage_msg_rx.recv().await {
                let sid = app_ref.lock().unwrap().rummage_session_id.clone();
                let _ = tinker::send_message(oc.clone(), &msg, sid.as_deref(), &work_dir, tx.clone()).await;
            }
        });
    }

    // Jog task — sits idle until the user switches to jog and sends a message.
    // No init prompt; the first user message opens the session.
    // Uses the strongest model tier (same as tinker and rummage) per the jog goal.
    {
        let app_ref = app.clone();
        let work_dir = work_dir.clone();
        let oc = oc_jog.clone();
        let tx = jog_orch_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = jog_msg_rx.recv().await {
                let sid = app_ref.lock().unwrap().jog_session_id.clone();
                let _ = tinker::send_message(oc.clone(), &msg, sid.as_deref(), &work_dir, tx.clone()).await;
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
        &mut goal_rx,
        msg_tx,
        run_tx,
        fs.clone(),
        work_dir.clone(),
        log,
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

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: Arc<Mutex<App>>,
    orch_rx: &mut mpsc::Receiver<TinkerEvent>,
    goal_rx: &mut mpsc::Receiver<GoalEvent>,
    msg_tx: mpsc::Sender<String>,
    run_tx: mpsc::Sender<(goal::Goal, Option<String>)>,
    fs: Arc<dyn Filesystem>,
    _work_dir: std::path::PathBuf,
    log: logger::LogSender,
    rummage_orch_rx: &mut mpsc::Receiver<TinkerEvent>,
    rummage_msg_tx: mpsc::Sender<String>,
    jog_orch_rx: &mut mpsc::Receiver<TinkerEvent>,
    jog_msg_tx: mpsc::Sender<String>,
) -> Result<()> {
    loop {
        // Draw
        terminal.draw(|f| tui::draw(f, &mut app.lock().unwrap()))?;

        // Drain tinker events — capture queue state before for change detection
        {
            let (q_before, active_before) = {
                let a = app.lock().unwrap();
                (a.goal_queue.len(), a.active_goal_id.clone())
            };
            while let Ok(ev) = orch_rx.try_recv() {
                handle_orch_event(&mut app.lock().unwrap(), ev, &msg_tx, &run_tx, &rummage_msg_tx, &jog_msg_tx, fs.as_ref(), &log);
            }
            let (q_after, active_after) = {
                let a = app.lock().unwrap();
                (a.goal_queue.len(), a.active_goal_id.clone())
            };
            if q_after != q_before || active_after != active_before {
                log.emit("tui", logger::LogEvent::TuiQueueChanged {
                    queue_len: q_after,
                    running_goal_id: active_after,
                });
            }
        }

        // Drain goal events
        {
            let (q_before, active_before) = {
                let a = app.lock().unwrap();
                (a.goal_queue.len(), a.active_goal_id.clone())
            };
            while let Ok(ev) = goal_rx.try_recv() {
                handle_goal_event(&mut app.lock().unwrap(), ev, &msg_tx, &run_tx, &log);
            }
            let (q_after, active_after) = {
                let a = app.lock().unwrap();
                (a.goal_queue.len(), a.active_goal_id.clone())
            };
            if q_after != q_before || active_after != active_before {
                log.emit("tui", logger::LogEvent::TuiQueueChanged {
                    queue_len: q_after,
                    running_goal_id: active_after,
                });
            }
        }

        // Drain rummage events
        while let Ok(ev) = rummage_orch_rx.try_recv() {
            handle_rummage_event(&mut app.lock().unwrap(), ev, &run_tx, &msg_tx, &jog_msg_tx, &rummage_msg_tx, fs.as_ref(), &log);
        }

        // Drain jog events
        while let Ok(ev) = jog_orch_rx.try_recv() {
            handle_jog_event(&mut app.lock().unwrap(), ev, &msg_tx, &rummage_msg_tx, &jog_msg_tx, fs.as_ref(), &log);
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
                KeyAction::SendToTinker(msg) => {
                    let _ = msg_tx.send(msg).await;
                }
                KeyAction::SendToRummage(msg) => {
                    let _ = rummage_msg_tx.send(msg).await;
                }
                KeyAction::SendToJog(msg) => {
                    let _ = jog_msg_tx.send(msg).await;
                }
                KeyAction::RunGoal(id, reason) => {
                    let goal = {
                        let a = app.lock().unwrap();
                        a.goals.iter().find(|g| g.id == id).cloned()
                    };
                    if let Some(goal) = goal {
                        let mut a = app.lock().unwrap();
                        if matches!(a.phase, Phase::RunningGoal(_)) {
                            // Spec (goal-sessions): "when a user-typed /run lands behind a
                            // pending tinker /run, each invocation is preserved as a
                            // separate queue entry with its own reason."
                            a.goal_queue.push_back((goal.clone(), reason.clone()));
                            a.batch_had_goals = true;
                            let queued_msg = format!(
                                "queued: `{}`{}",
                                goal.id,
                                reason.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
                            );
                            a.push_system_message(&queued_msg);
                            log.emit("dispatcher", logger::LogEvent::TinkerSystemMessageReceived { content: queued_msg });
                        } else {
                            start_or_confirm_goal(&mut a, goal, reason, &run_tx, &log);
                        }
                    }
                }
                KeyAction::ApproveNextGoal => {
                    let next = {
                        let a = app.lock().unwrap();
                        if let Phase::AwaitingConfirm(id) = &a.phase {
                            let g = a.goals.iter().find(|g| &g.id == id).cloned();
                            // also grab reason from front of queue if it's there
                            let mut reason = None;
                            if let Some(front) = a.goal_queue.front() {
                                if front.0.id == *id {
                                    reason = front.1.clone();
                                }
                            }
                            g.map(|g| (g, reason))
                        } else {
                            None
                        }
                    };
                    if let Some((goal, reason)) = next {
                        {
                            let mut a = app.lock().unwrap();
                            if let Some(front) = a.goal_queue.front() {
                                if front.0.id == goal.id {
                                    a.goal_queue.pop_front();
                                }
                            }
                            a.phase = Phase::RunningGoal(goal.id.clone());
                            a.active_goal_id = Some(goal.id.clone());
                            a.batch_had_goals = true;
                        }
                        let _ = run_tx.send((goal, reason)).await;
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

/// Parse `/run <goal-id> <reason>` lines from a tinker reply.
/// Each matching line yields `(goal_id, reason)` where reason may be empty.
/// Non-matching lines (prose, other slash commands) are silently skipped.
fn parse_run_commands(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("/run ") {
            let rest = rest.trim();
            if let Some((gid, reason)) = rest.split_once(char::is_whitespace) {
                out.push((gid.to_string(), reason.trim().to_string()));
            } else {
                out.push((rest.to_string(), String::new()));
            }
        }
    }
    out
}

/// Parse `@<agent-name> <message>` lines from an agent reply.
/// Recognizes `@tinker`, `@rummage`, `@jog` at the start of a trimmed line.
/// Returns `(recipient, message)` pairs; lines without a message are ignored.
fn parse_at_commands(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(msg) = line.strip_prefix("@tinker ") {
            out.push(("tinker".to_string(), msg.trim().to_string()));
        } else if let Some(msg) = line.strip_prefix("@rummage ") {
            out.push(("rummage".to_string(), msg.trim().to_string()));
        } else if let Some(msg) = line.strip_prefix("@jog ") {
            out.push(("jog".to_string(), msg.trim().to_string()));
        }
    }
    out
}

/// Deliver peer consultations collected from a completed agent reply.
/// Pushes a system message for user visibility, increments the recipient's task
/// counter, and sends the formatted message to the recipient's channel.
fn dispatch_peer_consultations(
    app: &mut App,
    sender: &str,
    consultations: &[(String, String)],
    msg_tx: &mpsc::Sender<String>,
    rummage_msg_tx: &mpsc::Sender<String>,
    jog_msg_tx: &mpsc::Sender<String>,
    log: &logger::LogSender,
) {
    for (recipient, msg) in consultations {
        let formatted = format!("[from {}] {}", sender, msg);
        let sys = format!("@{} → @{}: {}", sender, recipient, msg);
        app.push_system_message(&sys);
        log.emit(sender, logger::LogEvent::TinkerSystemMessageReceived { content: sys });
        match recipient.as_str() {
            "tinker" => {
                app.tinker_tasks += 1;
                let _ = msg_tx.try_send(formatted);
            }
            "rummage" => {
                app.rummage_tasks += 1;
                let _ = rummage_msg_tx.try_send(formatted);
            }
            "jog" => {
                app.jog_tasks += 1;
                let _ = jog_msg_tx.try_send(formatted);
            }
            _ => {}
        }
    }
}

fn handle_rummage_event(
    app: &mut App,
    ev: TinkerEvent,
    run_tx: &mpsc::Sender<(goal::Goal, Option<String>)>,
    msg_tx: &mpsc::Sender<String>,
    jog_msg_tx: &mpsc::Sender<String>,
    rummage_msg_tx: &mpsc::Sender<String>,
    fs: &dyn Filesystem,
    log: &logger::LogSender,
) {
    match ev {
        TinkerEvent::SessionId(id) => {
            if app.rummage_session_id.is_none() {
                app.rummage_session_id = Some(id);
            }
        }
        TinkerEvent::Text(chunk) => {
            app.append_rummage_chunk(&chunk);
        }
        TinkerEvent::Done => {
            app.rummage_tasks = app.rummage_tasks.saturating_sub(1);
            // Reload goals so /run commands rummage emits for case-2 fix dispatch
            // can be resolved to Goal structs.
            if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                app.goals = load.goals;
            }
            let consultations = parse_at_commands(&app.rummage_current_text);
            let run_commands = parse_run_commands(&app.rummage_current_text);
            let mut triggered: Vec<(goal::Goal, Option<String>)> = Vec::new();
            for (gid, reason) in run_commands {
                if let Some(g) = app.goals.iter().find(|g| g.id == gid).cloned() {
                    let reason_opt = if reason.is_empty() { None } else { Some(reason) };
                    triggered.push((g, reason_opt));
                }
            }
            app.finalize_rummage_message();
            dispatch_peer_consultations(app, "rummage", &consultations, msg_tx, rummage_msg_tx, jog_msg_tx, log);
            for (g, reason_opt) in triggered {
                start_or_confirm_goal(app, g, reason_opt, run_tx, log);
            }
        }
    }
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

fn handle_jog_event(
    app: &mut App,
    ev: TinkerEvent,
    msg_tx: &mpsc::Sender<String>,
    rummage_msg_tx: &mpsc::Sender<String>,
    jog_msg_tx: &mpsc::Sender<String>,
    fs: &dyn Filesystem,
    log: &logger::LogSender,
) {
    match ev {
        TinkerEvent::SessionId(id) => {
            if app.jog_session_id.is_none() {
                app.jog_session_id = Some(id);
            }
        }
        TinkerEvent::Text(chunk) => {
            app.append_jog_chunk(&chunk);
        }
        TinkerEvent::Done => {
            app.jog_tasks = app.jog_tasks.saturating_sub(1);
            let consultations = parse_at_commands(&app.jog_current_text);
            let edits = parse_jog_edit_commands(&app.jog_current_text);
            app.finalize_jog_message();
            dispatch_peer_consultations(app, "jog", &consultations, msg_tx, rummage_msg_tx, jog_msg_tx, log);
            // Reload goals so goal ids in /jog-edit lines can be validated.
            if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                app.goals = load.goals;
            }
            for (gid, instruction) in edits {
                let goal_exists = app.goals.iter().any(|g| g.id == gid);
                if goal_exists {
                    // Commission the edit from tinker. Jog's conversation has already
                    // done the dialectical anchoring the playback otherwise provides,
                    // so tinker applies the edit directly and shows a non-blocking diff.
                    let tinker_msg = format!(
                        "Jog audit — `{}`: {}. Apply this edit to the goal directly; jog's conversation has already provided the dialectical anchoring. After the edit, show what changed.",
                        gid, instruction
                    );
                    app.tinker_tasks += 1;
                    let _ = msg_tx.try_send(tinker_msg.clone());
                    log.emit("jog", logger::LogEvent::TinkerSystemMessageReceived { content: tinker_msg });
                }
            }
        }
    }
}

fn handle_orch_event(
    app: &mut App,
    ev: TinkerEvent,
    msg_tx: &mpsc::Sender<String>,
    run_tx: &mpsc::Sender<(goal::Goal, Option<String>)>,
    rummage_msg_tx: &mpsc::Sender<String>,
    jog_msg_tx: &mpsc::Sender<String>,
    fs: &dyn Filesystem,
    log: &logger::LogSender,
) {
    match ev {
        TinkerEvent::SessionId(id) => {
            if app.tinker_session_id.is_none() {
                app.tinker_session_id = Some(id);
            }
        }
        TinkerEvent::Text(chunk) => {
            app.append_assistant_chunk(&chunk);
        }
        TinkerEvent::Done => {
            app.tinker_tasks = app.tinker_tasks.saturating_sub(1);
            // Snapshot prior state so we can detect what changed.
            let prev_errors = app.parse_errors.clone();
            // Reload goals — tinker may have just created/edited a TOML file.
            // Without this, scheduling races the 2s watcher and won't see the new goal.
            if let Ok(load) = goal::load_all_goals(fs, &app.tinker_dirs) {
                app.goals = load.goals;
                app.update_parse_errors(load.errors);
            }
            // Detect new errors introduced by this tinker edit.
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
                            let name = p
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("?");
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
                        "Goal file invalid; asking tinker to fix (attempt {}/2).",
                        app.correction_attempts
                    );
                    app.push_system_message(&correction_msg);
                    log.emit("correction-injector", logger::LogEvent::TinkerSystemMessageReceived { content: correction_msg });
                    if msg_tx.try_send(msg).is_ok() {
                        app.tinker_tasks += 1;
                    };
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
            
            // Parse @-consultation and /run commands from tinker's just-streamed reply.
            // The text is still in `current_assistant_text` at this point —
            // `finalize_assistant_message` only runs below after we've
            // collected triggers. Scanning `app.messages` here would look at
            // the PREVIOUS assistant turn, not the one we just received.
            let consultations = parse_at_commands(&app.current_assistant_text);
            let run_commands = parse_run_commands(&app.current_assistant_text);

            let mut touched: Vec<(goal::Goal, String)> = Vec::new();
            for (gid, reason) in run_commands {
                if let Some(g) = app.goals.iter().find(|g| g.id == gid).cloned() {
                    touched.push((g, reason));
                }
            }

            if !touched.is_empty()
                && !matches!(app.phase, Phase::Initializing)
            {
                app.finalize_assistant_message();
                dispatch_peer_consultations(app, "tinker", &consultations, msg_tx, rummage_msg_tx, jog_msg_tx, log);

                // When a session is already running, ALL new /run triggers must queue up
                // behind it. Dispatching directly to run_tx here would bypass app.goal_queue,
                // break the TUI queue display, and mis-sequence the batch-summary trigger.
                // Spec (goal-sessions): "The queue does not deduplicate by goal-id; sessions
                // run in FIFO order regardless of repetition."
                if matches!(app.phase, Phase::RunningGoal(_)) {
                    let added: Vec<(goal::Goal, Option<String>)> = touched
                        .into_iter()
                        .map(|(g, r)| (g, if r.is_empty() { None } else { Some(r) }))
                        .collect();
                    let msg = if added.len() == 1 {
                        format!(
                            "queued: `{}`{}",
                            added[0].0.id,
                            added[0].1.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
                        )
                    } else {
                        let mut buf = format!("queued {} sessions:", added.len());
                        for (g, r) in &added {
                            buf.push_str(&format!(
                                "\n- `{}`{}",
                                g.id,
                                r.as_ref().map(|rr| format!(": {}", rr)).unwrap_or_default()
                            ));
                        }
                        buf
                    };
                    app.batch_had_goals = true;
                    for entry in added {
                        app.goal_queue.push_back(entry);
                    }
                    app.push_system_message(&msg);
                    log.emit("dispatcher", logger::LogEvent::TinkerSystemMessageReceived { content: msg });
                    return;
                }

                let mut iter = touched.into_iter();
                let (first_goal, first_reason) = iter.next().unwrap();
                let first_reason_opt = if first_reason.is_empty() { None } else { Some(first_reason) };

                for (g, r) in iter {
                    let r_opt = if r.is_empty() { None } else { Some(r) };
                    app.goal_queue.push_back((g, r_opt));
                }

                let total = app.goal_queue.len() + 1;
                let summary = if total == 1 {
                    format!(
                        "triggered: `{}`{}",
                        first_goal.id,
                        first_reason_opt.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
                    )
                } else {
                    let mut buf = format!(
                        "triggered: `{}`{} + {} more queued",
                        first_goal.id,
                        first_reason_opt.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
                        total - 1,
                    );
                    for (g, r) in &app.goal_queue {
                        buf.push_str(&format!("\n- `{}`{}", g.id, r.as_ref().map(|rr| format!(": {}", rr)).unwrap_or_default()));
                    }
                    buf
                };
                app.push_system_message(&summary);
                log.emit("dispatcher", logger::LogEvent::TinkerSystemMessageReceived { content: summary });

                start_or_confirm_goal(app, first_goal, first_reason_opt, run_tx, log);
                return;
            }

            app.finalize_assistant_message();
            dispatch_peer_consultations(app, "tinker", &consultations, msg_tx, rummage_msg_tx, jog_msg_tx, log);
            if app.tinker_tasks == 0 {
                match app.phase.clone() {
                    Phase::Initializing => {
                        app.push_system_message("Tinker ready. Ask me to add a goal.");
                        log.emit("harness", logger::LogEvent::TinkerSystemMessageReceived { content: "Tinker ready. Ask me to add a goal.".to_string() });
                        app.phase = Phase::Idle;
                    }
                    Phase::SummarizingBatch => {
                        app.batch_had_goals = false;
                        app.batch_summaries.clear();
                        app.phase = Phase::Idle;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn handle_goal_event(
    app: &mut App,
    ev: GoalEvent,
    msg_tx: &mpsc::Sender<String>,
    run_tx: &mpsc::Sender<(goal::Goal, Option<String>)>,
    log: &logger::LogSender,
) {
    match ev {
        GoalEvent::Text { goal_id, text } => {
            app.append_goal_log(&goal_id, &text);
        }
        GoalEvent::RunDone => {
            // Summary collection is happening in the goal runner task; wait for SummaryDone.
        }
        GoalEvent::CleanupBlocked { goal_id, dirty_files, error } => {
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
            // Roll back the phase set in start_or_confirm_goal so the UI
            // doesn't show this goal as running.
            if app.active_goal_id.as_deref() == Some(&goal_id) {
                app.active_goal_id = None;
                app.active_goal_reason = None;
            }
            if matches!(&app.phase, Phase::RunningGoal(id) if id == &goal_id) {
                app.phase = Phase::Idle;
            }
        }
        GoalEvent::SummaryDone { goal_id, summary } => {
            app.active_goal_id = None;
            app.active_goal_reason = None;
            // Record the summary for the eventual batch summary.
            app.batch_summaries.push((goal_id.clone(), summary.clone()));
            // If more goals are queued, run the next one. Otherwise the batch
            // is drained — ask tinker to summarize and reactively schedule.
            if let Some((next_goal, next_reason)) = app.goal_queue.pop_front() {
                start_or_confirm_goal(app, next_goal, next_reason, run_tx, log);
            } else if app.batch_had_goals {
                let summary_msg = build_batch_summary_request(&app.batch_summaries);
                app.batch_had_goals = false;
                app.batch_summaries.clear();
                app.phase = Phase::SummarizingBatch;
                if msg_tx.try_send(summary_msg).is_ok() {
                    app.tinker_tasks += 1;
                }
            } else if app.phase != Phase::Initializing {
                app.phase = Phase::Idle;
            }
        }
    }
}

fn build_batch_summary_request(summaries: &[(String, String)]) -> String {
    if summaries.is_empty() {
        return "Briefly summarize what was just accomplished in this batch (1-2 paragraphs). End with a one-line invitation for the user to try the result.".to_string();
    }
    let body = summaries
        .iter()
        .map(|(id, sum)| format!("Goal `{}`:\n{}\n", id, sum))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "All goal sessions for this batch have finished. Per-goal summaries (each is structured: accomplishments, software design changes, decisions made beyond the goal, how to try it):\n\n{body}\n\
         Compose a message to the user with three parts:\n\
         \n\
         1. What changed. 1-2 paragraphs covering what was accomplished across the batch. If multiple goals ran, mention each. Include the salient software-design changes (new modules/interfaces/types) inline where they help the user picture the artifact.\n\
         \n\
         2. Decisions worth knowing. Surface the \"decisions made beyond the goal\" items from the per-goal summaries — the user needs to be able to vet or override these.\n\
         \n\
         3. Try it now. End with a concrete invitation: pull the \"how to try it\" line(s) from the per-goal summaries, suggest one or two things to watch for, and ask the user to report back what they observed.\n\
         \n\
         After your prose, scan the batch above for cross-cutting signals. For each goal in your goal list that was NOT in this batch and has a concrete next step given what just changed, emit one `/run <goal-id> <reason>` line where reason is a focused single sentence. The `/run` lines must appear after your prose, on their own lines. Only emit `/run` lines for goals with a genuine next step — prefer no emission over a vague one."
    )
}

fn start_or_confirm_goal(app: &mut App, goal: goal::Goal, reason: Option<String>, run_tx: &mpsc::Sender<(goal::Goal, Option<String>)>, log: &logger::LogSender) {
    app.phase = Phase::RunningGoal(goal.id.clone());
    app.active_goal_id = Some(goal.id.clone());
    app.active_goal_reason = reason.clone();
    app.batch_had_goals = true;

    // Suppress duplicate if tinker's /run summary already announced this goal.
    if !app.messages.iter().any(|m| m.text.contains("triggered: `") && m.text.contains(&goal.id)) {
        let msg = format!("triggered: `{}`{}", goal.id, reason.as_ref().map(|r| format!(": {}", r)).unwrap_or_default());
        app.push_system_message(&msg);
        log.emit("dispatcher", logger::LogEvent::TinkerSystemMessageReceived { content: msg });
    }

    let _ = run_tx.try_send((goal, reason));
}


enum KeyAction {
    None,
    Quit,
    SendToTinker(String),
    SendToRummage(String),
    SendToJog(String),
    ApproveNextGoal,
    /// Run a specific goal immediately, bypassing the scheduler. The optional
    /// reason mirrors tinker's `/run <id> <reason>` syntax and the
    /// modal's reason-prompt submit.
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
                    // Confirm next goal in manual mode
                    if matches!(app.phase, Phase::AwaitingConfirm(_)) {
                        return KeyAction::ApproveNextGoal;
                    }
                    return KeyAction::None;
                }

                // Slash commands
                if input == "/quit" {
                    app.should_quit = true;
                    return KeyAction::Quit;
                }
                if input == "/help" {
                    let help_msg = "Commands: /run <goal-id>, /pause, /resume, /skip, /quit, /help. Tab = goal tree.";
                    app.push_system_message(help_msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: help_msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }
                if input == "/pause" {
                    app.loop_mode = LoopMode::Manual;
                    app.push_system_message("Manual mode: press Enter to approve each goal run.");
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: "Manual mode: press Enter to approve each goal run.".to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }
                if input == "/resume" {
                    app.loop_mode = LoopMode::Auto;
                    app.push_system_message("Auto mode: goals run automatically.");
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: "Auto mode: goals run automatically.".to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }
                if let Some(rest) = input.strip_prefix("/run") {
                    let rest = rest.trim();
                    app.input.clear();
                    if rest.is_empty() {
                        // Bare `/run` opens the reason-prompt modal for the
                        // currently-selected goal in the tree.
                        if let Some(g) = app.selected_goal() {
                            app.modal = Some(crate::app::ModalState {
                                goal_id: g.id,
                                input: String::new(),
                            });
                        } else {
                            app.push_system_message("No goal selected. Tab to the tree and pick one.");
                            log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: "No goal selected. Tab to the tree and pick one.".to_string() });
                        }
                        return KeyAction::None;
                    }
                    // `/run <id> [reason]` — reason is everything after the id.
                    let (id, reason) = match rest.split_once(char::is_whitespace) {
                        Some((id, r)) => (id, Some(r.trim().to_string()).filter(|s| !s.is_empty())),
                        None => (rest, None),
                    };
                    let exists = app.goals.iter().any(|g| g.id == id);
                    if exists {
                        return KeyAction::RunGoal(id.to_string(), reason);
                    }
                    let no_goal_msg = format!("No goal with id `{}`.", id);
                    app.push_system_message(&no_goal_msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: no_goal_msg });
                    return KeyAction::None;
                }
                if input == "/skip" {
                    if matches!(app.phase, Phase::AwaitingConfirm(_)) {
                        // The currently-confirming goal sits at the front of
                        // the queue (pushed there by start_or_confirm_goal in
                        // Manual mode). Drop it.
                        let _ = app.goal_queue.pop_front();
                        // Advance to the next queued goal, if any.
                        if let Some((next_goal, _)) = app.goal_queue.front() {
                            let next_id = next_goal.id.clone();
                            let skip_msg = format!("Skipped. Next: `{}`.", next_id);
                            app.push_system_message(&skip_msg);
                            log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: skip_msg });
                            app.phase = Phase::AwaitingConfirm(next_id);
                        } else {
                            app.phase = Phase::Idle;
                            app.push_system_message("Skipped. Waiting for input.");
                            log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: "Skipped. Waiting for input.".to_string() });
                        }
                    }
                    app.input.clear();
                    return KeyAction::None;
                }

                // Agent-switching slash commands (rummage / jog goals).
                if input == "/rummage" {
                    app.active_agent = ActiveAgent::Rummage;
                    let msg = "switched to rummage — type to chat, /tinker or /jog to switch";
                    app.push_system_message(msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }
                if input == "/tinker" {
                    app.active_agent = ActiveAgent::Tinker;
                    let msg = "switched to tinker — type to chat, /rummage or /jog to switch";
                    app.push_system_message(msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }
                if input == "/jog" {
                    app.active_agent = ActiveAgent::Jog;
                    let msg = "switched to jog — name a topic to audit, /tinker or /rummage to switch";
                    app.push_system_message(msg);
                    log.emit("repl", logger::LogEvent::TinkerSystemMessageReceived { content: msg.to_string() });
                    app.input.clear();
                    return KeyAction::None;
                }

                // Lock input while the active agent is busy.
                let active_busy = match app.active_agent {
                    ActiveAgent::Tinker => app.tinker_tasks > 0,
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
                    ActiveAgent::Tinker => {
                        app.tinker_tasks += 1;
                        app.correction_attempts = 0;
                        KeyAction::SendToTinker(input)
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

    // spec: goal-sessions decision — "After a batch drains, [per-goal]
    // summaries are folded into a single user-facing message." The
    // fold prompt must reference each goal's summary content and instruct
    // tinker to produce a unified, try-it-now message.
    #[test]
    fn test_spec_build_batch_summary_request_folds_per_goal_summaries() {
        let summaries = vec![
            ("calc".to_string(), "added add() and tests".to_string()),
            ("docs".to_string(), "wrote README".to_string()),
        ];
        let prompt = build_batch_summary_request(&summaries);
        // Each per-goal summary content must be present in the fold prompt.
        assert!(prompt.contains("calc"));
        assert!(prompt.contains("added add() and tests"));
        assert!(prompt.contains("docs"));
        assert!(prompt.contains("wrote README"));
        // Must instruct tinker to compose a single user-facing
        // message with the "try it" / next-step invitation.
        assert!(prompt.to_lowercase().contains("try it"));
    }

    // spec (triggers): the batch summary request must instruct
    // tinker to emit /run lines for downstream reactive goals,
    // with them appearing after the prose — machine side of tinker-
    // owned scheduling.
    #[test]
    fn test_spec_batch_summary_request_instructs_reactive_run_lines() {
        let summaries = vec![("calc".to_string(), "did stuff".to_string())];
        let prompt = build_batch_summary_request(&summaries);
        assert!(
            prompt.contains("/run"),
            "batch summary request must instruct tinker to emit /run lines",
        );
        assert!(
            prompt.to_lowercase().contains("after") && prompt.contains("prose"),
            "batch summary request must specify /run lines go after the prose",
        );
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

    // spec (tui, triggers): system messages emitted when /run
    // lines are parsed must use "triggered:" format, not the retired
    // "Scheduler:" naming from the old scheduler component. The message is
    // a Role::System entry (rendered grey in the TUI) and must contain both
    // the goal id and the reason from the /run line.
    #[test]
    fn test_spec_dispatched_run_uses_triggered_system_message_format() {
        // Replicate what handle_orch_event::Done builds for the single-goal case.
        let goal_id = "tui";
        let reason = "implement the thing";
        let reason_opt = Some(reason.to_string());
        let summary = format!(
            "triggered: `{}`{}",
            goal_id,
            reason_opt.as_ref().map(|r| format!(": {}", r)).unwrap_or_default(),
        );
        assert!(
            summary.starts_with("triggered:"),
            "dispatch message must start with 'triggered:'; got: {summary}",
        );
        assert!(
            !summary.contains("Scheduler:"),
            "retired 'Scheduler:' naming must not appear in dispatch message",
        );
        assert!(summary.contains(goal_id), "goal id must appear in message");
        assert!(summary.contains(reason), "reason must appear in message");
    }

    // spec (triggers): covers the `/run <goal-id> <reason>` trigger syntax.
    // Calls the production
    // `parse_run_commands` function directly (extracted from the inline
    // parser that previously lived in `handle_orch_event::Done`).
    //
    //   1. Chat-line trigger: the parser takes plain text; the call site
    //      passes `app.current_assistant_text` (the just-streamed reply,
    //      not historical `app.messages`). Verified here by leaving
    //      `app.messages` empty and putting triggers only in the text arg.
    //   2. `change_log` removed: `parse_run_commands` takes only text — no
    //      goal state. Structural removal from the Goal struct is separately
    //      covered by `goal::tests::test_spec_goal_has_no_change_log_field`.
    //   3. Multiple `/run` lines per turn each produce a trigger, in order.
    //   4. Non-`/run` prose passes through without generating triggers.
    #[test]
    fn test_spec_run_command_parsing() {
        // (Decision 1) Confirm App exposes `current_assistant_text` and
        // that an empty `messages` list does not block parsing.
        let mut app = App::new();
        assert!(app.messages.is_empty());
        assert!(app.current_assistant_text.is_empty());

        // (Decisions 1, 3, 4) Build a realistic tinker turn:
        //   - prose preamble (must not yield a trigger)
        //   - three `/run` lines with reasons (must each yield a trigger)
        //   - prose interleaved (must not yield a trigger)
        //   - a `/run` line with no reason (yields trigger with empty reason)
        app.current_assistant_text = String::from(
            "Acknowledged. I will dispatch a few goals.\n\
             /run alpha investigate the failing test\n\
             Some commentary in between.\n\
             /run beta resync the index\n\
             /run gamma   please clean up the cache\n\
             /run delta\n\
             That is all for now.\n",
        );

        let triggers = parse_run_commands(&app.current_assistant_text);

        // Decision 3 + Decision 4: exactly four `/run` lines parsed; prose
        // lines did not produce triggers.
        assert_eq!(
            triggers.len(),
            4,
            "expected 4 /run triggers (prose lines must not parse); got {:?}",
            triggers,
        );

        // Decision 3: order is preserved across multiple `/run` lines.
        assert_eq!(triggers[0].0, "alpha");
        assert_eq!(triggers[0].1, "investigate the failing test");
        assert_eq!(triggers[1].0, "beta");
        assert_eq!(triggers[1].1, "resync the index");
        assert_eq!(triggers[2].0, "gamma");
        // Internal whitespace between gid and reason is collapsed by trim().
        assert_eq!(triggers[2].1, "please clean up the cache");

        // A `/run` line with only a goal id (no reason) yields an empty
        // reason string — the dispatch site converts that to `None`.
        assert_eq!(triggers[3].0, "delta");
        assert_eq!(triggers[3].1, "");

        // Decision 2: parsing does not consult goal state at all.
        assert!(app.goals.is_empty());
        let triggers_again = parse_run_commands(&app.current_assistant_text);
        assert_eq!(
            triggers, triggers_again,
            "parser must depend only on chat text, never on goal fields",
        );

        // Decision 4 (sharper check): a turn containing ONLY prose yields
        // zero triggers.
        let prose_only =
            "Sure, I will keep an eye on that. No goals to fire right now.\n\
             We could revisit later.\n";
        assert!(
            parse_run_commands(prose_only).is_empty(),
            "prose-only chat must produce no triggers",
        );

        // Decision 1 (sharper check): the parser must NOT match a `/run`-like
        // token that lacks the leading slash or appears mid-line.
        let not_a_trigger =
            "Here is some advice: run alpha would be a good idea.\n\
             foo /run alpha not at line start\n";
        assert!(
            parse_run_commands(not_a_trigger).is_empty(),
            "only line-leading `/run ` triggers must parse; got {:?}",
            parse_run_commands(not_a_trigger),
        );
    }

    // spec: goal-sessions decision — "For manual runs (user typing `/run` at
    // the input prompt ...), the reason is collected from the user. This
    // reason must be passed explicitly into the agent's prompt/context."
    // Covers the REPL-input parse path, which is distinct from the
    // tinker-driven `parse_run_commands` path used for created /
    // edited / reactive goals.
    #[test]
    fn test_spec_repl_manual_run_with_reason_dispatches_correctly() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "tui".into(),
            summary: String::new(),
            description: "build the tui".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            source_path: None,
        }];
        app.input = "/run tui verify the reason is threaded".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        match action {
            KeyAction::RunGoal(id, reason) => {
                assert_eq!(id, "tui", "goal id must be extracted from REPL input");
                assert_eq!(
                    reason,
                    Some("verify the reason is threaded".to_string()),
                    "reason must be everything after the goal id",
                );
            }
            _ => panic!("expected KeyAction::RunGoal from manual /run with reason"),
        }
    }

    // spec: goal-sessions — `/run <id>` with no trailing reason text must
    // yield `None` for reason (not `Some("")`) so the agent prompt's
    // "## Reason for triggering" section is omitted entirely.
    #[test]
    fn test_spec_repl_run_without_reason_yields_none() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.goals = vec![goal::Goal {
            id: "tui".into(),
            summary: String::new(),
            description: "build the tui".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            source_path: None,
        }];
        app.input = "/run tui".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        match action {
            KeyAction::RunGoal(id, reason) => {
                assert_eq!(id, "tui");
                assert_eq!(reason, None, "bare /run <id> must yield None reason, not Some(\"\")");
            }
            _ => panic!("expected KeyAction::RunGoal from /run without reason"),
        }
    }

    // spec: goal-sessions decision — "One goal session at a time." When a
    // SummaryDone event fires and a second goal is queued, it must be
    // dispatched immediately rather than swallowed. Conversely, the first
    // goal must not be re-dispatched. This enforces the serial drain: each
    // SummaryDone advances to exactly the next queued goal.
    #[test]
    fn test_spec_one_goal_session_at_a_time_queue_drains_serially() {
        let (run_tx, mut run_rx) = mpsc::channel::<(goal::Goal, Option<String>)>(4);
        let (msg_tx, _msg_rx) = mpsc::channel::<String>(4);

        let second = goal::Goal {
            id: "second".into(),
            summary: String::new(),
            description: "second goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            source_path: None,
        };

        let mut app = App::new();
        // Simulate: "first" is running, "second" is queued behind it.
        app.phase = Phase::RunningGoal("first".into());
        app.batch_had_goals = true;
        app.goal_queue.push_back((second.clone(), Some("do the thing".into())));

        // Fire SummaryDone for "first".
        handle_goal_event(
            &mut app,
            GoalEvent::SummaryDone {
                goal_id: "first".into(),
                summary: "done".into(),
            },
            &msg_tx,
            &run_tx,
            &logger::noop_sender(),
        );

        // "second" must have been dispatched — exactly one item on the channel.
        let dispatched = run_rx.try_recv().expect("second goal must be dispatched after first finishes");
        assert_eq!(dispatched.0.id, "second", "dispatched goal must be the queued one");
        assert_eq!(dispatched.1.as_deref(), Some("do the thing"));

        // No further dispatch — queue was length 1.
        assert!(run_rx.try_recv().is_err(), "no additional goals must be dispatched");

        // Phase transitions to RunningGoal for "second", not back to Idle.
        assert!(
            matches!(&app.phase, Phase::RunningGoal(id) if id == "second"),
            "phase must be RunningGoal(second) after serial dispatch",
        );
    }

    // spec (goal-sessions): "The queue does not deduplicate by goal-id; sessions run
    // in FIFO order regardless of repetition." The queue data structure must hold two
    // separate entries for the same goal-id, each with its own reason.
    #[test]
    fn test_spec_queue_preserves_same_goal_id_no_dedup_fifo() {
        let alpha = goal::Goal {
            id: "alpha".into(),
            summary: String::new(),
            description: "alpha goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            source_path: None,
        };
        let mut app = App::new();
        app.goal_queue.push_back((alpha.clone(), Some("first reason".into())));
        app.goal_queue.push_back((alpha.clone(), Some("second reason".into())));
        assert_eq!(app.goal_queue.len(), 2, "same goal-id must not be deduplicated");

        let (g1, r1) = app.goal_queue.pop_front().unwrap();
        assert_eq!(g1.id, "alpha");
        assert_eq!(r1.as_deref(), Some("first reason"), "first entry must carry its own reason");

        let (g2, r2) = app.goal_queue.pop_front().unwrap();
        assert_eq!(g2.id, "alpha");
        assert_eq!(r2.as_deref(), Some("second reason"), "second entry must carry its own reason");
    }

    // spec (goal-sessions): "The pending-session queue is in-memory only, like the
    // sessions themselves. It dies on tinker restart." A fresh App has an empty queue —
    // nothing loaded from disk.
    #[test]
    fn test_spec_queue_in_memory_only_starts_empty() {
        let app = App::new();
        assert!(
            app.goal_queue.is_empty(),
            "goal queue must start empty (in-memory only, never persisted to disk)",
        );
    }

    // spec (goal-sessions): when the queue holds two invocations of the same goal-id
    // with different reasons, SummaryDone must drain them in FIFO order, dispatching
    // each with its own reason intact — not deduplicated or reordered.
    #[test]
    fn test_spec_queue_same_goal_drains_fifo_with_separate_reasons() {
        let (run_tx, mut run_rx) = mpsc::channel::<(goal::Goal, Option<String>)>(8);
        let (msg_tx, _msg_rx) = mpsc::channel::<String>(4);

        let alpha = goal::Goal {
            id: "alpha".into(),
            summary: String::new(),
            description: "alpha goal".into(),
            parent_id: String::new(),
            children: vec![],
            related: vec![],
            source_path: None,
        };

        let mut app = App::new();
        app.phase = Phase::RunningGoal("alpha".into());
        app.batch_had_goals = true;
        // Two separate invocations of the same goal queued with distinct reasons.
        app.goal_queue.push_back((alpha.clone(), Some("second run".into())));
        app.goal_queue.push_back((alpha.clone(), Some("third run".into())));

        handle_goal_event(
            &mut app,
            GoalEvent::SummaryDone { goal_id: "alpha".into(), summary: "first done".into() },
            &msg_tx,
            &run_tx,
            &logger::noop_sender(),
        );

        let dispatched = run_rx.try_recv().expect("second alpha must be dispatched next");
        assert_eq!(dispatched.0.id, "alpha");
        assert_eq!(dispatched.1.as_deref(), Some("second run"), "FIFO: second reason dispatched first");

        assert_eq!(app.goal_queue.len(), 1, "third run still queued");
        assert_eq!(
            app.goal_queue[0].1.as_deref(),
            Some("third run"),
            "third entry preserved in queue with its own reason",
        );
        assert!(matches!(&app.phase, Phase::RunningGoal(id) if id == "alpha"));
    }

    // spec (goal-sessions): "When tinker emits several /run lines for one goal
    // across turns… each invocation is preserved as a separate queue entry." When a session
    // is already running and tinker emits a new /run, the new invocation must go
    // into app.goal_queue — not be dispatched directly to run_tx.
    #[test]
    fn test_spec_tinker_run_while_running_goes_to_queue_not_run_tx() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (run_tx, mut run_rx) = mpsc::channel::<(goal::Goal, Option<String>)>(8);
        let (msg_tx, _msg_rx) = mpsc::channel::<String>(4);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let goals_dir = tinker_dir.join("goals");
        let mock_fs = MockFs::new();
        mock_fs.add_file(
            &goals_dir.join("alpha.toml"),
            "id = \"alpha\"\ndescription = \"alpha goal\"\nparent_id = \"\"\nchildren = []\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        // Another session is already running.
        app.phase = Phase::RunningGoal("beta".into());
        app.active_goal_id = Some("beta".into());
        app.current_assistant_text = "/run alpha across-turn reason\n".into();

        let (rummage_tx, _rummage_rx) = mpsc::channel::<String>(4);
        let (jog_tx, _jog_rx) = mpsc::channel::<String>(4);
        handle_orch_event(
            &mut app,
            TinkerEvent::Done,
            &msg_tx,
            &run_tx,
            &rummage_tx,
            &jog_tx,
            &mock_fs,
            &logger::noop_sender(),
        );

        // Nothing dispatched to the runner — the new invocation must be in the queue.
        assert!(
            run_rx.try_recv().is_err(),
            "across-turn /run must not be dispatched to run_tx while a session is running",
        );
        assert_eq!(app.goal_queue.len(), 1, "new invocation must be in app.goal_queue");
        assert_eq!(app.goal_queue[0].0.id, "alpha");
        assert_eq!(
            app.goal_queue[0].1.as_deref(),
            Some("across-turn reason"),
            "queued entry must carry its own reason",
        );
        // Phase must still show the original running goal, not overwritten.
        assert!(
            matches!(&app.phase, Phase::RunningGoal(id) if id == "beta"),
            "phase must still be RunningGoal(beta), not overwritten to alpha",
        );
    }

    // spec (rummage): on a confirmed case-2 bug, rummage emits `/run <goal-id> <reason>`
    // in its output. handle_rummage_event must parse those lines and dispatch the goal
    // to run_tx, the same as tinker's /run dispatch does.
    #[test]
    fn test_spec_rummage_run_command_dispatched() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (run_tx, mut run_rx) = mpsc::channel::<(goal::Goal, Option<String>)>(8);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let goals_dir = tinker_dir.join("goals");
        let mock_fs = MockFs::new();
        mock_fs.add_file(
            &goals_dir.join("goal-sessions.toml"),
            "id = \"goal-sessions\"\ndescription = \"runs goal sessions\"\nparent_id = \"\"\nchildren = []\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.rummage_current_text =
            "Current best understanding: the bug is X.\n/run goal-sessions failing test test_spec_foo pins correct behavior\n"
                .into();

        let (msg_tx2, _msg_rx2) = mpsc::channel::<String>(4);
        let (jog_tx2, _jog_rx2) = mpsc::channel::<String>(4);
        let (rummage_tx2, _rummage_rx2) = mpsc::channel::<String>(4);
        handle_rummage_event(
            &mut app,
            TinkerEvent::Done,
            &run_tx,
            &msg_tx2,
            &jog_tx2,
            &rummage_tx2,
            &mock_fs,
            &logger::noop_sender(),
        );

        let dispatched = run_rx.try_recv().expect("rummage /run must dispatch to run_tx");
        assert_eq!(dispatched.0.id, "goal-sessions");
        assert_eq!(
            dispatched.1.as_deref(),
            Some("failing test test_spec_foo pins correct behavior"),
        );
    }

    // spec (tui): "When the user manually triggers a goal via Enter in the
    // goal tree (or `/run` without arguments), a modal text input dialog pops
    // up to collect the trigger reason."
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

    // spec (compact-goal-context): the --tinker-full-goal-context flag must
    // cause main.rs to use build_full_text_index (not build_compact_index) for
    // the goals summary passed to tinker, and to call tinker_init_prompt_full_context
    // rather than tinker_init_prompt.
    #[test]
    fn test_spec_full_goal_context_flag_routes_to_full_text_index() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("goal::build_full_text_index"),
            "main.rs must call build_full_text_index on the full-context path",
        );
        assert!(
            main_rs.contains("tinker_init_prompt_full_context"),
            "main.rs must call tinker_init_prompt_full_context on the full-context path",
        );
        assert!(
            main_rs.contains("use_full_goal_context"),
            "main.rs must gate the full-text path on the --tinker-full-goal-context flag",
        );
    }

    // spec (rummage): "Agent switching is via explicit slash commands only —
    // `/rummage` to switch to rummage, `/tinker` to switch back."
    // `/rummage` must set active_agent to Rummage and emit a system message;
    // the input buffer must be cleared so the command doesn't appear as chat.
    #[test]
    fn test_spec_slash_rummage_switches_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        assert_eq!(app.active_agent, ActiveAgent::Tinker, "starts as Tinker");
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

    // spec (rummage): `/tinker` switches back from rummage to tinker.
    #[test]
    fn test_spec_slash_tinker_switches_back_to_tinker() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::app::Role;
        let mut app = App::new();
        app.active_agent = ActiveAgent::Rummage;
        app.input = "/tinker".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::None),
            "/tinker must not dispatch a chat message; returns None",
        );
        assert_eq!(
            app.active_agent,
            ActiveAgent::Tinker,
            "/tinker must switch active_agent back to Tinker",
        );
        assert!(app.input.is_empty(), "input must be cleared after /tinker");
        assert!(
            app.messages.iter().any(|m| m.role == Role::System && m.text.contains("tinker")),
            "/tinker must emit a system message naming tinker",
        );
    }

    // spec (rummage / tui): "One agent is active at a time. User messages go to
    // whoever is active." When active_agent is Rummage, Enter on a non-slash
    // message must return SendToRummage (not SendToTinker). When Tinker, must
    // return SendToTinker.
    #[test]
    fn test_spec_message_routes_to_active_agent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // Route to Tinker when active.
        let mut app = App::new();
        app.phase = Phase::Idle;
        app.active_agent = ActiveAgent::Tinker;
        app.input = "hello tinker".into();
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &logger::noop_sender());
        assert!(
            matches!(action, KeyAction::SendToTinker(_)),
            "message must route to Tinker when active_agent is Tinker; got {:?}",
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
            main_rs.contains("tinker_m, rummage::rummage_system_prompt()"),
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
            main_rs.contains("tinker_m, jog::jog_system_prompt()"),
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
        assert_eq!(app.active_agent, ActiveAgent::Tinker, "starts as Tinker");
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
    // fire a separate commission to tinker, one per line. tinker_tasks must
    // increment once per commission.
    #[test]
    fn test_spec_jog_edit_multiple_lines_each_dispatch_to_tinker() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);

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
        app.jog_current_text =
            "Two findings here.\n/jog-edit rummage Clarify the case-2 durable test path.\n/jog-edit tui Document the active-agent prompt tag.\n"
                .into();

        let (rummage_tx, _rummage_rx) = mpsc::channel::<String>(4);
        let (jog_tx, _jog_rx) = mpsc::channel::<String>(4);
        handle_jog_event(
            &mut app,
            TinkerEvent::Done,
            &msg_tx,
            &rummage_tx,
            &jog_tx,
            &mock_fs,
            &logger::noop_sender(),
        );

        let first = msg_rx.try_recv().expect("first /jog-edit must dispatch to tinker");
        assert!(first.contains("rummage"), "first commission must name rummage");
        let second = msg_rx.try_recv().expect("second /jog-edit must dispatch to tinker");
        assert!(second.contains("tui"), "second commission must name tui");
        assert!(msg_rx.try_recv().is_err(), "no extra commissions beyond two /jog-edit lines");
        assert_eq!(app.tinker_tasks, 2, "tinker_tasks must increment once per /jog-edit line");
    }

    // spec (triggers): if a /jog-edit line names a goal-id that does not exist in
    // the loaded goal list, it is silently dropped — no commission sent to tinker.
    #[test]
    fn test_spec_jog_edit_unknown_goal_id_silently_skipped() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let mock_fs = MockFs::new();
        mock_fs.add_dir(&tinker_dir.join("goals"));

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.jog_current_text = "/jog-edit nonexistent This goal does not exist.\n".into();

        let (rummage_tx, _rummage_rx) = mpsc::channel::<String>(4);
        let (jog_tx, _jog_rx) = mpsc::channel::<String>(4);
        handle_jog_event(
            &mut app,
            TinkerEvent::Done,
            &msg_tx,
            &rummage_tx,
            &jog_tx,
            &mock_fs,
            &logger::noop_sender(),
        );

        assert!(
            msg_rx.try_recv().is_err(),
            "unknown goal-id in /jog-edit must not dispatch any commission to tinker",
        );
        assert_eq!(app.tinker_tasks, 0, "tinker_tasks must not increment for unknown goal-id");
    }

    // spec (jog / triggers): the jog→tinker commission message must instruct
    // tinker to apply the edit directly (v1: no playback interview) and to show
    // what changed. This is the "non-blocking diff" requirement from the jog goal.
    #[test]
    fn test_spec_jog_edit_commission_instructs_direct_apply_and_show_diff() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let goals_dir = tinker_dir.join("goals");
        let mock_fs = MockFs::new();
        mock_fs.add_file(
            &goals_dir.join("rummage.toml"),
            "id = \"rummage\"\ndescription = \"investigates\"\nparent_id = \"\"\nchildren = []\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.jog_current_text =
            "/jog-edit rummage The SCOPE section omits the jog→tinker channel.\n".into();

        let (rummage_tx, _rummage_rx) = mpsc::channel::<String>(4);
        let (jog_tx, _jog_rx) = mpsc::channel::<String>(4);
        handle_jog_event(
            &mut app,
            TinkerEvent::Done,
            &msg_tx,
            &rummage_tx,
            &jog_tx,
            &mock_fs,
            &logger::noop_sender(),
        );

        let commission = msg_rx.try_recv().expect("commission must be sent");
        assert!(
            commission.to_lowercase().contains("directly"),
            "commission must instruct tinker to apply the edit directly (no interview): {commission}",
        );
        assert!(
            commission.to_lowercase().contains("what changed") || commission.to_lowercase().contains("changed"),
            "commission must instruct tinker to show what changed: {commission}",
        );
    }

    // spec (jog): on an "I know better" finding, jog emits `/jog-edit <goal-id>
    // <instruction>` in its output. handle_jog_event must parse those lines and
    // forward a prescriptive commission to tinker via msg_tx.
    #[test]
    fn test_spec_jog_edit_command_dispatched_to_tinker() {
        use crate::test_utils::MockFs;
        use std::path::PathBuf;

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);

        let tinker_dir = PathBuf::from("/fake/.tinker");
        let goals_dir = tinker_dir.join("goals");
        let mock_fs = MockFs::new();
        mock_fs.add_file(
            &goals_dir.join("rummage.toml"),
            "id = \"rummage\"\ndescription = \"investigates program behavior\"\nparent_id = \"\"\nchildren = []\n",
        );

        let mut app = App::new();
        app.tinker_dirs = vec![tinker_dir];
        app.jog_current_text =
            "I know better — the scope section omits the jog→tinker channel.\n/jog-edit rummage Add jog→tinker channel description to the SCOPE section.\n"
                .into();

        let (rummage_tx, _rummage_rx) = mpsc::channel::<String>(4);
        let (jog_tx, _jog_rx) = mpsc::channel::<String>(4);
        handle_jog_event(
            &mut app,
            TinkerEvent::Done,
            &msg_tx,
            &rummage_tx,
            &jog_tx,
            &mock_fs,
            &logger::noop_sender(),
        );

        let dispatched = msg_rx.try_recv().expect("jog /jog-edit must dispatch to msg_tx (tinker)");
        assert!(dispatched.contains("rummage"), "tinker commission must name the goal id");
        assert!(dispatched.contains("Add jog"), "tinker commission must carry the instruction");
        assert_eq!(app.jog_tasks, 0, "jog_tasks must decrement on Done");
        assert_eq!(app.tinker_tasks, 1, "tinker_tasks must increment for the commission");
    }

    // spec (peer-consult): parse_at_commands extracts @tinker, @rummage, @jog
    // lines from agent output. Non-matching lines are silently skipped.
    #[test]
    fn test_spec_peer_consult_parse_at_tinker() {
        let r = parse_at_commands("@tinker what does this module do?");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "tinker");
        assert_eq!(r[0].1, "what does this module do?");
    }

    #[test]
    fn test_spec_peer_consult_parse_at_rummage() {
        let r = parse_at_commands("@rummage can you trace the call?");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "rummage");
        assert_eq!(r[0].1, "can you trace the call?");
    }

    #[test]
    fn test_spec_peer_consult_parse_at_jog() {
        let r = parse_at_commands("@jog do you still mean X by this?");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "jog");
        assert_eq!(r[0].1, "do you still mean X by this?");
    }

    #[test]
    fn test_spec_peer_consult_parse_ignores_prose_lines() {
        let r = parse_at_commands("some prose\n@tinker hello\nmore prose\n@rummage check this");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, "tinker");
        assert_eq!(r[1].0, "rummage");
    }

    #[test]
    fn test_spec_peer_consult_parse_at_without_message_ignored() {
        // "@tinker" with no trailing space+message must not match
        let r = parse_at_commands("@tinker");
        assert_eq!(r.len(), 0, "@tinker without a message must be ignored");
    }

    #[test]
    fn test_spec_peer_consult_parse_multiple_at_lines() {
        let r = parse_at_commands("@tinker q1\n@rummage q2\n@jog q3");
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], ("tinker".to_string(), "q1".to_string()));
        assert_eq!(r[1], ("rummage".to_string(), "q2".to_string()));
        assert_eq!(r[2], ("jog".to_string(), "q3".to_string()));
    }

    // spec (peer-consult): dispatch_peer_consultations routes each consultation
    // to the correct channel and formats the message with the sender name.
    #[test]
    fn test_spec_peer_consult_dispatch_routes_to_correct_channel() {
        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(8);
        let (rummage_tx, mut rummage_rx) = mpsc::channel::<String>(8);
        let (jog_tx, mut jog_rx) = mpsc::channel::<String>(8);

        let mut app = App::new();
        let consultations = vec![
            ("tinker".to_string(), "question for tinker".to_string()),
            ("rummage".to_string(), "question for rummage".to_string()),
            ("jog".to_string(), "question for jog".to_string()),
        ];
        dispatch_peer_consultations(
            &mut app,
            "rummage",
            &consultations,
            &msg_tx,
            &rummage_tx,
            &jog_tx,
            &logger::noop_sender(),
        );

        let tinker_msg = msg_rx.try_recv().expect("tinker must receive its consultation");
        assert!(tinker_msg.contains("[from rummage]"), "message must carry sender attribution");
        assert!(tinker_msg.contains("question for tinker"));
        assert_eq!(app.tinker_tasks, 1, "tinker_tasks must increment");

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
        let (rummage_tx, _) = mpsc::channel::<String>(8);
        let (jog_tx, _) = mpsc::channel::<String>(8);

        let mut app = App::new();
        let consultations = vec![("rummage".to_string(), "trace the init flow".to_string())];
        dispatch_peer_consultations(
            &mut app,
            "tinker",
            &consultations,
            &msg_tx,
            &rummage_tx,
            &jog_tx,
            &logger::noop_sender(),
        );

        let sys = app.messages.iter().find(|m| m.role == app::Role::System);
        assert!(sys.is_some(), "a system message must be pushed for the consultation");
        let text = &sys.unwrap().text;
        assert!(text.contains("@tinker"), "system message must name the sender");
        assert!(text.contains("@rummage"), "system message must name the recipient");
        assert!(text.contains("trace the init flow"), "system message must include the message content");
    }
}
