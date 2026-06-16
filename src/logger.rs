//! Runtime introspection substrate — append-only event log and debounced state snapshot.
//!
//! `.tinker/logs/runtime.jsonl` — JSONL event log (append-only, no rotation).
//! `.tinker/state/runtime.json` — current semantic UI snapshot (debounced writes).
//!
//! Producers call `LogSender::emit` (synchronous, non-blocking). A background
//! task drains the channel, serializes events as JSON lines, and batches writes
//! (every 100 ms or 10 events, whichever comes first). The state file is
//! debounced ~100 ms after the last state-changing event.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use tokio::sync::mpsc;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub goal_id: String,
    pub reason: Option<String>,
    pub status: String,
    pub queued_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScrollOffsets {
    pub repl: usize,
    pub log: usize,
    pub goal_list: usize,
    pub goal_text: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub selected_goal_id: Option<String>,
    pub focus: String,
    pub scroll_offsets: ScrollOffsets,
    pub queue: Vec<QueueEntry>,
}

impl Default for StateSnapshot {
    fn default() -> Self {
        Self {
            selected_goal_id: None,
            focus: "repl".to_string(),
            scroll_offsets: ScrollOffsets::default(),
            queue: Vec::new(),
        }
    }
}

// ── Log events ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogEvent {
    TinkerSystemMessageReceived {
        content: String,
    },
    GoalSessionDispatched {
        goal_id: String,
        reason: Option<String>,
        backend: String,
    },
    GoalSessionStarted {
        goal_id: String,
    },
    GoalSessionFinished {
        goal_id: String,
        exit_status: String,
        duration_ms: u64,
        files_modified_count: usize,
        files_modified: Vec<String>,
        tool_calls: usize,
        summary_chars: usize,
        full_output: String,
        backend: String,
    },
    CleanupHookRun {
        goal_id: String,
        outcome: String,
        duration_ms: u64,
    },
    GoalFileChanged {
        path: String,
    },
    TuiSelectionChanged {
        goal_id: Option<String>,
    },
    TuiFocusChanged {
        focus: String,
    },
    TuiScrollChanged {
        pane: String,
        y: usize,
    },
    TuiQueueChanged {
        running_goal_ids: Vec<String>,
    },
    /// Emitted when the system transitions between idle and active (a batch
    /// starts or all sessions complete). `direction` is `"idle_to_active"` or
    /// `"active_to_idle"`. Also emitted as a user-visible system message so
    /// batch boundaries are observable in the conversation pane.
    BatchTransition {
        direction: String,
    },
    /// Emitted when an agent calls the `send_message` tool to dispatch a
    /// message to another session. `sender` is the calling goal's ID;
    /// `target` is the goal ID the sender addressed; `success` reports
    /// whether the dispatch reached the registry; `error` is set when
    /// `success` is false and names the failure reason (typically
    /// "unknown target" when the target is not in the session registry).
    /// Tool-delivered dispatches share the same substrate as envelope
    /// dispatches — the event exists separately so introspection can
    /// distinguish the two delivery paths.
    SendMessageDispatched {
        sender: String,
        target: String,
        success: bool,
        error: Option<String>,
    },
    /// Emitted when an agent calls the `spawn_session` tool to spawn a
    /// fresh sub-session of its own goal. `sender` is the calling session's
    /// id (could be a permanent goal ID or an ephemeral coordinator id);
    /// `sub_session_id` is the new ephemeral session id (e.g. `"rummage~3"`
    /// or `"rummage~1~5"` for a coordinator-spawned sub-session);
    /// `label` is the caller's correlation tag, `None` when no label was
    /// provided; `success` reports whether the spawn was enqueued;
    /// `error` is set when `success` is false and names the failure
    /// reason (e.g. "no dispatcher configured" when the callback is `None`,
    /// or the caller's goal was not found in the goal tree). Tool-delivered
    /// fresh-spawns share the same substrate as envelope-delivered ones
    /// — the event exists separately so introspection can distinguish
    /// the two delivery paths.
    SpawnSessionDispatched {
        sender: String,
        sub_session_id: String,
        label: Option<String>,
        success: bool,
        error: Option<String>,
    },
    /// Emitted at session-registry population when a goal id is found at
    /// multiple `.tinker` discovery tiers. `goal_id` is the colliding id;
    /// `contributors` is the ordered list of `(tier_label, path)` pairs
    /// (the first entry is the winning copy; subsequent entries are
    /// duplicates that were ignored).
    ///
    /// Each contributor is `(tier_label, path)`. Tier labels are
    /// `goal`-side strings — `"project-local"`, `"ancestor global"`,
    /// `"packaged"`, `"binary-relative-packaged"` today — but the set
    /// is open: any new tier added by `goal::tier_label` (or a new
    /// `*_TIER` constant) flows through verbatim. The test at lines
    /// 1031-1033 pins this openness — the example list above is
    /// illustrative, not a closed universe.
    ///
    /// This event parallels the user-visible system message emitted by
    /// `goal-agents`'s startup diagnostic — both surface the same
    /// structural fact so silent overrides never go unnoticed, and both
    /// are emitted only when the contributing set actually changes
    /// (the watcher diffs against the last-seen set, so an unchanged
    /// collision produces no event and no system message on a re-cycle).
    /// The structured event carries the same data the system message
    /// renders, so a `jq` query can recover it without parsing prose.
    GoalCollision {
        goal_id: String,
        contributors: Vec<(String, String)>,
    },
}

#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub ts: String,
    pub source: String,
    #[serde(flatten)]
    pub event: LogEvent,
}

// ── LogSender ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct LogSender {
    tx: mpsc::UnboundedSender<LogEntry>,
}

impl LogSender {
    pub fn emit(&self, source: &str, event: LogEvent) {
        let entry = LogEntry {
            ts: now_iso8601(),
            source: source.to_string(),
            event,
        };
        let _ = self.tx.send(entry);
    }
}

/// No-op sender for tests and contexts that don't need logging.
#[cfg(test)]
pub fn noop_sender() -> LogSender {
    let (tx, _rx) = mpsc::unbounded_channel();
    LogSender { tx }
}

// ── Startup ──────────────────────────────────────────────────────────────────

pub fn start_logger(log_path: PathBuf, state_path: PathBuf) -> LogSender {
    let (tx, rx) = mpsc::unbounded_channel::<LogEntry>();
    tokio::spawn(logger_task(rx, log_path, state_path));
    LogSender { tx }
}

// ── Background task ──────────────────────────────────────────────────────────

async fn logger_task(
    mut rx: mpsc::UnboundedReceiver<LogEntry>,
    log_path: PathBuf,
    state_path: PathBuf,
) {
    let mut batch: Vec<String> = Vec::new();
    let mut state = StateSnapshot::default();
    let mut state_dirty = false;
    let mut last_state_change: Option<std::time::Instant> = None;

    let mut ticker = tokio::time::interval(tokio::time::Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            maybe_entry = rx.recv() => {
                match maybe_entry {
                    Some(entry) => {
                        let changed = apply_to_state(&entry, &mut state);
                        if changed {
                            state_dirty = true;
                            last_state_change = Some(std::time::Instant::now());
                        }
                        if let Ok(json) = serde_json::to_string(&entry) {
                            batch.push(json);
                        }
                        if batch.len() >= 10 {
                            flush_batch(&mut batch, &log_path).await;
                        }
                    }
                    None => {
                        flush_batch(&mut batch, &log_path).await;
                        if state_dirty {
                            write_state(&state, &state_path).await;
                        }
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    flush_batch(&mut batch, &log_path).await;
                }
                if state_dirty {
                    let elapsed = last_state_change
                        .map(|t| t.elapsed().as_millis() >= 100)
                        .unwrap_or(false);
                    if elapsed {
                        write_state(&state, &state_path).await;
                        state_dirty = false;
                        last_state_change = None;
                    }
                }
            }
        }
    }
}

fn apply_to_state(entry: &LogEntry, state: &mut StateSnapshot) -> bool {
    match &entry.event {
        LogEvent::TuiSelectionChanged { goal_id } => {
            state.selected_goal_id = goal_id.clone();
            true
        }
        LogEvent::TuiFocusChanged { focus } => {
            state.focus = focus.clone();
            true
        }
        LogEvent::TuiScrollChanged { pane, y } => {
            match pane.as_str() {
                "repl" => state.scroll_offsets.repl = *y,
                "log" => state.scroll_offsets.log = *y,
                "goal_list" => state.scroll_offsets.goal_list = *y,
                "goal_text" => state.scroll_offsets.goal_text = *y,
                _ => {}
            }
            true
        }
        LogEvent::TuiQueueChanged { running_goal_ids, .. } => {
            if running_goal_ids.is_empty() {
                state.queue.retain(|e| e.status != "running");
            }
            true
        }
        LogEvent::GoalSessionDispatched { goal_id, reason, .. } => {
            state.queue.push(QueueEntry {
                goal_id: goal_id.clone(),
                reason: reason.clone(),
                status: "running".to_string(),
                queued_at: entry.ts.clone(),
            });
            true
        }
        LogEvent::GoalSessionFinished { goal_id, .. } => {
            state.queue.retain(|e| &e.goal_id != goal_id || e.status != "running");
            true
        }
        _ => false,
    }
}

async fn flush_batch(batch: &mut Vec<String>, log_path: &std::path::Path) {
    if batch.is_empty() {
        return;
    }
    let mut content = batch.join("\n");
    content.push('\n');
    batch.clear();

    use tokio::io::AsyncWriteExt;
    if let Ok(mut f) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await
    {
        let _ = f.write_all(content.as_bytes()).await;
    }
}

async fn write_state(state: &StateSnapshot, state_path: &std::path::Path) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = tokio::fs::write(state_path, json).await;
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

pub fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let (y, mo, d) = days_to_ymd(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let mut d = days;
    let mut y = 1970u64;
    loop {
        let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
        let diy = if leap { 366 } else { 365 };
        if d < diy {
            break;
        }
        d -= diy;
        y += 1;
    }
    let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
    let dim = [
        31u64,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut mo = 1u64;
    for &days_in in &dim {
        if d < days_in {
            break;
        }
        d -= days_in;
        mo += 1;
    }
    (y, mo, d + 1)
}

pub fn hash_string(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Count tool-call lines (lines starting with the → arrow) in streamed output.
pub fn count_tool_calls(output: &str) -> usize {
    output
        .lines()
        .filter(|l| l.trim().starts_with('\u{2192}'))
        .count()
}

/// Extract file paths from Write/Edit tool-call lines in streamed output.
/// This matches the `→ Write <path>` / `→ Edit <path>` format the native
/// backend emits via `prompts::tool_completed_with_summary`.
pub fn extract_modified_files(output: &str) -> Vec<String> {
    let mut files = std::collections::BTreeSet::new();
    for line in output.lines() {
        let line = line.trim();
        // Format from format_tool_use: "→ Write path" or "→ Edit path"
        for prefix in &[
            "\u{2192} Write ",
            "\u{2192} Edit ",
            "→ Write ",
            "→ Edit ",
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let path = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !path.is_empty() {
                    files.insert(path);
                }
                break;
            }
        }
    }
    files.into_iter().collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // spec (tinker-introspection): every log line must carry the required
    // ts, kind, and source fields (DECISIONS: "required ts (ISO 8601 UTC),
    // kind, and source fields per line, plus event-specific payload").
    #[test]
    fn test_spec_log_line_has_required_ts_kind_source_fields() {
        let entry = LogEntry {
            ts: "2026-05-20T10:00:00Z".to_string(),
            source: "tinker".to_string(),
            event: LogEvent::TinkerSystemMessageReceived { content: "ready".to_string() },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(val.get("ts").is_some(), "log line must have 'ts' field");
        assert!(val.get("kind").is_some(), "log line must have 'kind' field");
        assert!(val.get("source").is_some(), "log line must have 'source' field");
        assert_eq!(val["ts"], "2026-05-20T10:00:00Z");
        assert_eq!(val["source"], "tinker");
        assert_eq!(val["kind"], "tinker_system_message_received");
    }

    // spec (tinker-introspection): event kinds serialize as snake_case
    // matching the DECISIONS enumeration.
    #[test]
    fn test_spec_event_kinds_are_snake_case() {
        let cases: &[(&str, LogEvent)] = &[
            (
                "tinker_system_message_received",
                LogEvent::TinkerSystemMessageReceived {
                    content: "hello".to_string(),
                },
            ),
            (
                "goal_session_finished",
                LogEvent::GoalSessionFinished {
                    goal_id: "x".to_string(),
                    exit_status: "clean".to_string(),
                    duration_ms: 1000,
                    files_modified_count: 0,
                    files_modified: vec![],
                    tool_calls: 3,
                    summary_chars: 200,
                    full_output: "output".to_string(),
                    backend: "opencode".to_string(),
                },
            ),
            (
                "tui_queue_changed",
                LogEvent::TuiQueueChanged {
                    running_goal_ids: vec![],
                },
            ),
        ];
        for (expected_kind, event) in cases {
            let entry = LogEntry {
                ts: "2026-05-20T00:00:00Z".to_string(),
                source: "test".to_string(),
                event: event.clone(),
            };
            let json = serde_json::to_string(&entry).unwrap();
            let val: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                val["kind"], *expected_kind,
                "kind must be snake_case for {}",
                expected_kind
            );
        }
    }

    // spec (tinker-introspection): goal_session_finished carries the
    // observable set — exit_status, duration_ms, files_modified, tool_calls,
    // summary_chars, full_output, backend. No session-declared outcome.
    #[test]
    fn test_spec_goal_session_finished_carries_observable_set() {
        let event = LogEvent::GoalSessionFinished {
            goal_id: "tui".to_string(),
            exit_status: "clean".to_string(),
            duration_ms: 12345,
            files_modified_count: 2,
            files_modified: vec!["src/tui.rs".to_string(), "src/main.rs".to_string()],
            tool_calls: 7,
            summary_chars: 350,
            full_output: "full session transcript".to_string(),
            backend: "claude".to_string(),
        };
        let entry = LogEntry {
            ts: "2026-05-20T00:00:00Z".to_string(),
            source: "goal_session".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(val.get("exit_status").is_some());
        assert!(val.get("duration_ms").is_some());
        assert!(val.get("files_modified").is_some());
        assert!(val.get("tool_calls").is_some());
        assert!(val.get("summary_chars").is_some());
        assert!(val.get("full_output").is_some());
        assert!(val.get("backend").is_some());
        // no session-declared outcome field
        assert!(
            val.get("outcome").is_none(),
            "goal_session_finished must not have an outcome field"
        );
    }

    // spec (tinker-introspection): message-level events carry full, untruncated
    // text. System-message events carry their `content` verbatim; finished-session
    // events carry the entire transcript in `full_output`.
    #[test]
    fn test_spec_message_events_carry_full_text() {
        let content = "Complete system message content.".to_string();
        let event = LogEvent::TinkerSystemMessageReceived { content: content.clone() };
        let entry = LogEntry {
            ts: "2026-05-20T00:00:00Z".to_string(),
            source: "tinker".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["content"], content);

        let transcript = "Full session transcript across many turns.".to_string();
        let event2 = LogEvent::GoalSessionFinished {
            goal_id: "tend".to_string(),
            exit_status: "clean".to_string(),
            duration_ms: 1,
            files_modified_count: 0,
            files_modified: vec![],
            tool_calls: 0,
            summary_chars: 0,
            full_output: transcript.clone(),
            backend: "claude".to_string(),
        };
        let entry2 = LogEntry {
            ts: "2026-05-20T00:00:00Z".to_string(),
            source: "goal_session".to_string(),
            event: event2,
        };
        let json2 = serde_json::to_string(&entry2).unwrap();
        let val2: serde_json::Value = serde_json::from_str(&json2).unwrap();
        assert_eq!(val2["full_output"], transcript);
    }

    // spec (tinker-introspection): tinker_system_message_received
    // logs the injecting component via LogEntry.source and the full message text
    // in the content field. It is not state-changing — apply_to_state returns false.
    #[test]
    fn test_spec_tinker_system_message_received_logs_source_and_content() {
        let event = LogEvent::TinkerSystemMessageReceived {
            content: "triggered: `tui`: implement the modal".to_string(),
        };
        let entry = LogEntry {
            ts: "2026-05-20T00:00:00Z".to_string(),
            source: "dispatcher".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["kind"], "tinker_system_message_received");
        assert_eq!(val["source"], "dispatcher", "source names the injecting component");
        assert_eq!(val["content"], "triggered: `tui`: implement the modal");

        // Not a state-changing event — must not trigger state snapshot writes.
        let mut state = StateSnapshot::default();
        let changed = apply_to_state(&entry, &mut state);
        assert!(!changed, "TinkerSystemMessageReceived must not mark state dirty");
    }

    // spec (tinker-introspection): no log rotation — the log grows
    // unboundedly. Mechanically verified: flush_batch must open in append mode,
    // never with truncate.
    #[test]
    fn test_spec_no_rotation_log_appends() {
        let source = include_str!("logger.rs");
        // Split needles so they don't self-match in this test body.
        let append_needle = [".append", "(true)"].concat();
        let truncate_needle = [".truncate", "(true)"].concat();
        assert!(
            source.contains(&append_needle),
            "log file must be opened in append mode (no rotation)"
        );
        // The only occurrence of truncate_needle must be in this test itself
        // (the assertion below). Production flush_batch must NOT use it.
        let count = source.match_indices(&truncate_needle).count();
        assert!(
            count <= 1,
            "flush_batch must never truncate the log file; found {} occurrences of .truncate(true)",
            count
        );
    }

    // spec (tinker-introspection): state is updated only for events that
    // carry semantic UI information — apply_to_state returns true for those and
    // false for non-state events (the caller uses this to gate the debounce timer).
    #[test]
    fn test_spec_state_debounce_via_apply_to_state() {
        let mut state = StateSnapshot::default();
        let entry = |ev: LogEvent| LogEntry {
            ts: "2026-05-20T00:00:00Z".to_string(),
            source: "tui".to_string(),
            event: ev,
        };

        let changed = apply_to_state(
            &entry(LogEvent::TuiSelectionChanged {
                goal_id: Some("tui".to_string()),
            }),
            &mut state,
        );
        assert!(changed, "TuiSelectionChanged must mark state dirty");
        assert_eq!(state.selected_goal_id, Some("tui".to_string()));

        let changed2 = apply_to_state(
            &entry(LogEvent::TuiFocusChanged {
                focus: "tree".to_string(),
            }),
            &mut state,
        );
        assert!(changed2, "TuiFocusChanged must mark state dirty");
        assert_eq!(state.focus, "tree");

        // Non-state events return false
        let changed3 = apply_to_state(
            &entry(LogEvent::TinkerSystemMessageReceived { content: "x".to_string() }),
            &mut state,
        );
        assert!(!changed3, "TinkerSystemMessageReceived must not mark state dirty");
    }

    // spec (tinker-introspection): count_tool_calls counts lines starting
    // with the → arrow, matching the format emitted by format_tool_use in
    // claude.rs and opencode.rs.
    #[test]
    fn test_spec_count_tool_calls_counts_arrow_lines() {
        let output = "\u{2192} Bash cargo build\nsome output\n\u{2192} Read src/main.rs\nmore output\n";
        assert_eq!(count_tool_calls(output), 2, "must count two → lines");
        assert_eq!(count_tool_calls("no tool calls here\nnormal text\n"), 0);
        // Indented arrow lines also count (tool calls may be indented).
        let indented = "  \u{2192} Write src/tui.rs arg\n";
        assert_eq!(count_tool_calls(indented), 1, "indented → must count");
    }

    // spec (tinker-introspection): extract_modified_files identifies Write
    // and Edit tool-call paths from streamed output. This is an approximation of
    // "files modified" used when a filesystem watcher is unavailable; it relies
    // on the compact one-liner format from format_tool_use. Deduplicates via
    // BTreeSet so repeated edits to the same path count once.
    #[test]
    fn test_spec_extract_modified_files_parses_write_and_edit_tool_lines() {
        let output = [
            "\u{2192} Write src/main.rs extra args",
            "\u{2192} Edit src/tui.rs",
            "some unrelated output line",
            "\u{2192} Bash cargo test",
            "\u{2192} Write src/main.rs",  // duplicate — must not double-count
        ].join("\n");
        let files = extract_modified_files(&output);
        assert!(files.contains(&"src/main.rs".to_string()), "Write path must be extracted");
        assert!(files.contains(&"src/tui.rs".to_string()), "Edit path must be extracted");
        assert!(!files.iter().any(|f| f.contains("cargo")), "Bash paths must not be extracted");
        assert_eq!(files.len(), 2, "BTreeSet must deduplicate repeated paths");
    }

    // spec (tend-introspection): BatchTransition serializes as snake_case kind
    // with a `direction` field set to "idle_to_active" or "active_to_idle".
    // It is NOT state-changing — apply_to_state must return false, so it never
    // triggers a state-snapshot write. Both directions must be representable.
    #[test]
    fn test_spec_batch_transition_serializes_and_is_not_state_changing() {
        for direction in &["idle_to_active", "active_to_idle"] {
            let event = LogEvent::BatchTransition { direction: direction.to_string() };
            let entry = LogEntry {
                ts: "2026-06-05T00:00:00Z".to_string(),
                source: "harness".to_string(),
                event: event.clone(),
            };
            let json = serde_json::to_string(&entry).unwrap();
            let val: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(val["kind"], "batch_transition", "kind must be batch_transition");
            assert_eq!(val["direction"], *direction, "direction field must round-trip");
            assert_eq!(val["source"], "harness");
            // Must not mark state dirty — batch state is not in the UI snapshot.
            let mut state = StateSnapshot::default();
            let changed = apply_to_state(&entry, &mut state);
            assert!(
                !changed,
                "BatchTransition({direction}) must not mark state dirty"
            );
        }
    }

    // spec (send-message): the new `SendMessageDispatched` log event serializes
    // with `kind = "send_message_dispatched"` and carries the sender, target,
    // and success fields.  The optional `error` field is set on failure with
    // the registry-miss reason.  This event is the introspection hook that
    // distinguishes tool-delivered dispatches from envelope-delivered ones.
    #[test]
    fn test_spec_send_message_dispatched_event_serializes() {
        // Success case
        let event = LogEvent::SendMessageDispatched {
            sender: "tend".to_string(),
            target: "rummage".to_string(),
            success: true,
            error: None,
        };
        let entry = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "tend".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["kind"], "send_message_dispatched");
        assert_eq!(val["sender"], "tend");
        assert_eq!(val["target"], "rummage");
        assert_eq!(val["success"], true);
        // On success the error field is None — serde serializes None as null
        // (or skips with skip_serializing_if); here we want the field present
        // so consumers can rely on it.
        assert!(val.get("error").is_some(), "error field must be present (even on success)");

        // Failure case — error carries the registry-miss reason.
        let event_fail = LogEvent::SendMessageDispatched {
            sender: "tend".to_string(),
            target: "ghost".to_string(),
            success: false,
            error: Some("target `ghost` is not in the session registry".to_string()),
        };
        let entry_fail = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "tend".to_string(),
            event: event_fail,
        };
        let json_fail = serde_json::to_string(&entry_fail).unwrap();
        let val_fail: serde_json::Value = serde_json::from_str(&json_fail).unwrap();
        assert_eq!(val_fail["kind"], "send_message_dispatched");
        assert_eq!(val_fail["success"], false);
        assert_eq!(
            val_fail["error"],
            "target `ghost` is not in the session registry"
        );
    }

    // spec (send-message): the self-send failure case is a first-class
    // observable alongside the unknown-target and channel-closed
    // failure cases. The event carries `target == sender` (the
    // model-supplied string, not a normalized name) and the descriptive
    // `error` reason. Consumers that want to filter for self-sends
    // structurally can match on `target == sender` without grepping
    // the error string. This test pins the round-trip — the event must
    // serialize with the sender string under `target` and the verbatim
    // error reason, so a logged `SendMessageDispatched` for a rejected
    // self-send is recoverable in full. The error string is
    // alternatives-aware: it points to spawn_session for sub-tasks of
    // self, a different goal id for a different agent, and the
    // continue-reasoning fallback — locking these phrases in here
    // pins the agent UX surface alongside the event shape.
    #[test]
    fn test_spec_sender_equals_target_failure_serializes() {
        let event = LogEvent::SendMessageDispatched {
            sender: "tend".to_string(),
            target: "tend".to_string(),
            success: false,
            error: Some(
                "send_message: cannot send a message to yourself (`tend` is the same as the sender). To dispatch a sub-task of your own goal, use the spawn_session tool; to reach a different agent, address their goal id; otherwise continue reasoning in the current turn"
                    .to_string(),
            ),
        };
        let entry = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "tend".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["kind"], "send_message_dispatched");
        // Both fields carry the same string — a consumer can detect
        // self-sends structurally with `target == sender`.
        assert_eq!(val["sender"], "tend");
        assert_eq!(val["target"], "tend");
        assert_eq!(val["sender"], val["target"], "self-send event must have sender == target");
        assert_eq!(val["success"], false);
        // The error string round-trips verbatim — the full reason
        // including the target name and every course-correction
        // alternative is recoverable from the log.
        let err = val["error"].as_str().expect("error field must be a string on failure");
        assert!(err.contains("tend"), "error must name the offending target");
        assert!(err.contains("yourself"), "error must surface the self-send condition");
        assert!(
            err.contains("spawn_session"),
            "error must point to spawn_session for sub-tasks of self"
        );
        assert!(
            err.contains("goal id"),
            "error must point to addressing a different agent's goal id"
        );
        assert!(
            err.contains("continue reasoning"),
            "error must include the continue-reasoning fallback"
        );
    }

    // spec (send-message): SendMessageDispatched is NOT state-changing —
    // it never triggers a state snapshot write.  Dispatch events are
    // observable but not part of the UI snapshot's queue/selection
    // surface.
    #[test]
    fn test_spec_send_message_dispatched_is_not_state_changing() {
        let entry = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "tend".to_string(),
            event: LogEvent::SendMessageDispatched {
                sender: "tend".to_string(),
                target: "rummage".to_string(),
                success: true,
                error: None,
            },
        };
        let mut state = StateSnapshot::default();
        let changed = apply_to_state(&entry, &mut state);
        assert!(
            !changed,
            "SendMessageDispatched must not mark state dirty"
        );
    }

    // spec (spawn-session, tend-introspection): the SpawnSessionDispatched
    // event serializes with `kind = "spawn_session_dispatched"` and carries
    // the sender, sub_session_id, label, and success fields. The optional
    // `error` field is set on failure with the goal-tree-miss reason
    // (or any other spawn failure). The `ts` field is the dispatch time
    // (LogEntry-level), not on the variant — keeps the schema flat and
    // avoids duplicating the timestamp the LogEntry envelope already
    // provides. Consumers can rely on `error` being present (even on
    // success, as null) — mirrors the SendMessageDispatched contract.
    #[test]
    fn test_spec_spawn_session_dispatched_event_serializes() {
        // Success case — label is provided, error is None.
        let event = LogEvent::SpawnSessionDispatched {
            sender: "rummage".to_string(),
            sub_session_id: "rummage~3".to_string(),
            label: Some("investigate-auth".to_string()),
            success: true,
            error: None,
        };
        let entry = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "rummage".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["kind"], "spawn_session_dispatched");
        assert_eq!(val["sender"], "rummage");
        assert_eq!(val["sub_session_id"], "rummage~3");
        assert_eq!(val["label"], "investigate-auth");
        assert_eq!(val["success"], true);
        // The error field is present even on success (serialized as null
        // since we do NOT use skip_serializing_if). Consumers can rely on
        // the field being present in every event — the same contract
        // SendMessageDispatched follows.
        assert!(val.get("error").is_some(), "error field must be present (even on success)");

        // Success case — no label. The label field is present and null,
        // matching the error-field contract.
        let event_no_label = LogEvent::SpawnSessionDispatched {
            sender: "rummage".to_string(),
            sub_session_id: "rummage~5".to_string(),
            label: None,
            success: true,
            error: None,
        };
        let entry_no_label = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "rummage".to_string(),
            event: event_no_label,
        };
        let json_no_label = serde_json::to_string(&entry_no_label).unwrap();
        let val_no_label: serde_json::Value = serde_json::from_str(&json_no_label).unwrap();
        assert!(
            val_no_label.get("label").is_some(),
            "label field must be present even when None"
        );

        // Failure case — error carries the goal-tree-miss reason.
        let event_fail = LogEvent::SpawnSessionDispatched {
            sender: "ghost".to_string(),
            sub_session_id: "ghost~1".to_string(),
            label: None,
            success: false,
            error: Some("spawn_session: caller goal `ghost` is not in the goal tree".to_string()),
        };
        let entry_fail = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "ghost".to_string(),
            event: event_fail,
        };
        let json_fail = serde_json::to_string(&entry_fail).unwrap();
        let val_fail: serde_json::Value = serde_json::from_str(&json_fail).unwrap();
        assert_eq!(val_fail["kind"], "spawn_session_dispatched");
        assert_eq!(val_fail["success"], false);
        assert_eq!(
            val_fail["error"],
            "spawn_session: caller goal `ghost` is not in the goal tree"
        );
    }

    // spec (spawn-session, tend-introspection): SpawnSessionDispatched is
    // NOT state-changing — it never triggers a state snapshot write.
    // Dispatch events are observable but not part of the UI snapshot's
    // queue/selection surface. Mirrors the SendMessageDispatched contract.
    #[test]
    fn test_spec_spawn_session_dispatched_is_not_state_changing() {
        let entry = LogEntry {
            ts: "2026-06-10T00:00:00Z".to_string(),
            source: "rummage".to_string(),
            event: LogEvent::SpawnSessionDispatched {
                sender: "rummage".to_string(),
                sub_session_id: "rummage~3".to_string(),
                label: Some("x".to_string()),
                success: true,
                error: None,
            },
        };
        let mut state = StateSnapshot::default();
        let changed = apply_to_state(&entry, &mut state);
        assert!(
            !changed,
            "SpawnSessionDispatched must not mark state dirty"
        );
    }

    // spec (tend-introspection): TuiScrollChanged updates the correct
    // pane in the state snapshot's scroll_offsets. All four pane names are handled;
    // unknown pane names are silently ignored (open extension point).
    #[test]
    fn test_spec_tui_scroll_changed_updates_correct_pane() {
        let mut state = StateSnapshot::default();
        let entry = |ev: LogEvent| LogEntry {
            ts: "2026-05-20T00:00:00Z".to_string(),
            source: "tui".to_string(),
            event: ev,
        };

        let changed = apply_to_state(
            &entry(LogEvent::TuiScrollChanged { pane: "repl".to_string(), y: 42 }),
            &mut state,
        );
        assert!(changed, "TuiScrollChanged must mark state dirty");
        assert_eq!(state.scroll_offsets.repl, 42);

        apply_to_state(
            &entry(LogEvent::TuiScrollChanged { pane: "log".to_string(), y: 7 }),
            &mut state,
        );
        assert_eq!(state.scroll_offsets.log, 7);

        apply_to_state(
            &entry(LogEvent::TuiScrollChanged { pane: "goal_list".to_string(), y: 3 }),
            &mut state,
        );
        assert_eq!(state.scroll_offsets.goal_list, 3);

        apply_to_state(
            &entry(LogEvent::TuiScrollChanged { pane: "goal_text".to_string(), y: 15 }),
            &mut state,
        );
        assert_eq!(state.scroll_offsets.goal_text, 15);

        // Repl value must be unchanged from the other pane writes.
        assert_eq!(state.scroll_offsets.repl, 42);
    }

    // spec (tinker-introspection): the goal_file_changed event is emitted
    // when the content hash of the loaded goals changes between polling cycles.
    // The implementation uses a content hash rather than inotify; the path field
    // carries the directory, not the specific file. Verified via source inspection.
    #[test]
    fn test_spec_goal_file_changed_emitted_on_content_hash_change() {
        let main_rs = include_str!("main.rs");
        assert!(
            main_rs.contains("GoalFileChanged"),
            "main.rs must emit GoalFileChanged when goal content changes",
        );
        assert!(
            main_rs.contains("hash_string"),
            "main.rs must use hash_string to detect goal content changes",
        );
        assert!(
            main_rs.contains("prev_goal_hash"),
            "main.rs must compare against a previous hash to detect changes",
        );
    }

    // spec (tend-introspection / goal-agents): cross-tier goal collisions are
    // captured as a structured `goal_collision` event paralleling the
    // user-visible system message that `goal-agents`'s startup diagnostic
    // emits. The event carries the colliding `goal_id` and an ordered
    // `contributors` list of `(tier_label, path)` pairs, where the first
    // entry is the winning copy and subsequent entries are duplicates that
    // were ignored. Tier labels are open: `"project-local"`,
    // `"ancestor global"`, `"packaged"` are the current set; new tiers may
    // be added by `goal::tier_label` without changing this event's shape.
    //
    // The event is not state-changing — apply_to_state must return false,
    // so it never triggers a state-snapshot write. The collision is a
    // historical record, not a live UI fact.
    #[test]
    fn test_spec_goal_collision_event_serializes_with_contributors() {
        let event = LogEvent::GoalCollision {
            goal_id: "shared".to_string(),
            contributors: vec![
                (
                    "project-local".to_string(),
                    "/proj/.tinker/goals/shared.toml".to_string(),
                ),
                (
                    "ancestor global".to_string(),
                    "/home/.tinker/goals/shared.toml".to_string(),
                ),
            ],
        };
        let entry = LogEntry {
            ts: "2026-07-15T00:00:00Z".to_string(),
            source: "goal-agents".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();

        // kind discriminator — snake_case, like every other event.
        assert_eq!(val["kind"], "goal_collision", "kind must be snake_case goal_collision");
        // ts + source pass through unchanged.
        assert_eq!(val["ts"], "2026-07-15T00:00:00Z");
        assert_eq!(val["source"], "goal-agents", "source names the emitting component");
        // goal_id round-trips.
        assert_eq!(val["goal_id"], "shared");
        // contributors is an array of [tier, path] pairs, in load order.
        // Tuples serialize as JSON arrays in serde_json.
        let contribs = val["contributors"].as_array()
            .expect("contributors must serialize as a JSON array");
        assert_eq!(contribs.len(), 2, "both contributing tiers must be present");
        assert_eq!(contribs[0][0], "project-local", "first contributor is the winner");
        assert_eq!(contribs[0][1], "/proj/.tinker/goals/shared.toml");
        assert_eq!(contribs[1][0], "ancestor global", "later entries are duplicates");
        assert_eq!(contribs[1][1], "/home/.tinker/goals/shared.toml");

        // Single-contributor edge case: a single-contributor collision is
        // structurally possible (a future tier-label consolidation could
        // produce one) — the array must serialize as a length-1 vec, not
        // be dropped. The "no collision" case (empty vec) cannot happen
        // because `load_all_goals` only emits GoalCollision entries when
        // there is at least one duplicate, but the event must still
        // round-trip cleanly if it ever does.
        let single = LogEvent::GoalCollision {
            goal_id: "only".to_string(),
            contributors: vec![(
                "project-local".to_string(),
                "/proj/.tinker/goals/only.toml".to_string(),
            )],
        };
        let entry_single = LogEntry {
            ts: "2026-07-15T00:00:00Z".to_string(),
            source: "goal-agents".to_string(),
            event: single,
        };
        let json_single = serde_json::to_string(&entry_single).unwrap();
        let val_single: serde_json::Value = serde_json::from_str(&json_single).unwrap();
        assert_eq!(val_single["kind"], "goal_collision");
        assert_eq!(val_single["goal_id"], "only");
        assert_eq!(val_single["contributors"].as_array().unwrap().len(), 1);

        // The event is not state-changing — apply_to_state returns false,
        // so emitting it never causes a state-snapshot write. The state
        // file only mirrors live UI facts; a goal-collision is a historical
        // record, not a UI fact.
        let mut state = StateSnapshot::default();
        let changed = apply_to_state(&entry, &mut state);
        assert!(
            !changed,
            "GoalCollision must not mark state dirty"
        );
    }
}
