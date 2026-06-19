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
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
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
    pub goal_list: usize,
    pub goal_text: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub selected_goal_id: Option<String>,
    pub focus: String,
    pub scroll_offsets: ScrollOffsets,
    pub queue: Vec<QueueEntry>,
    /// Per-goal token-usage metrics derived from logged per-call data.
    /// Keyed by goal id. Each entry tracks session count and average
    /// input/output tokens per session, isolating three cost drivers:
    /// triggering frequency (session_count), prompt size (avg_input),
    /// and verbosity (avg_output). Reset on tinker restart — the state
    /// file is a current snapshot, not a historical record (the event
    /// log is the durable source).
    #[serde(default)]
    pub token_metrics: HashMap<String, GoalTokenMetrics>,
}

impl Default for StateSnapshot {
    fn default() -> Self {
        Self {
            selected_goal_id: None,
            focus: "repl".to_string(),
            scroll_offsets: ScrollOffsets::default(),
            queue: Vec::new(),
            token_metrics: HashMap::new(),
        }
    }
}

/// Per-goal token-usage metrics for the state snapshot. The serialized
/// fields (`session_count`, `avg_input_tokens_per_session`,
/// `avg_output_tokens_per_session`) are derived from the internal running
/// totals (`#[serde(skip)]`) which accumulate as
/// `ApiTokenUsage` events arrive and `GoalSessionFinished` events
/// increment the session count.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GoalTokenMetrics {
    /// Number of completed dispatches for this goal.
    pub session_count: usize,
    /// Average prompt (input) tokens per session. Derived from the running
    /// total divided by session_count.
    #[serde(default)]
    pub avg_input_tokens_per_session: u64,
    /// Average completion (output) tokens per session. Derived from the
    /// running total divided by session_count.
    #[serde(default)]
    pub avg_output_tokens_per_session: u64,
    /// Internal: cumulative prompt tokens across all API calls for this goal.
    #[serde(skip)]
    pub total_prompt_tokens: u64,
    /// Internal: cumulative completion tokens across all API calls for this goal.
    #[serde(skip)]
    pub total_completion_tokens: u64,
}

impl GoalTokenMetrics {
    /// Recalculate the derived averages from the running totals. Called
    /// after every `ApiTokenUsage` and `GoalSessionFinished` update so
    /// the serialized fields stay current.
    fn recalc(&mut self) {
        let count = self.session_count.max(1) as u64;
        self.avg_input_tokens_per_session = self.total_prompt_tokens / count;
        self.avg_output_tokens_per_session = self.total_completion_tokens / count;
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
    /// Per-API-call token usage, fired once per LLM round-trip inside the
    /// tool loop. `goal_id` tags the originating goal; `prompt_tokens` and
    /// `completion_tokens` are the input/output token counts from the
    /// provider's `usage` object; `total_tokens` is the provider's reported
    /// total; `cached_tokens` is a provider-specific extension (e.g. Anthropic
    /// prompt caching, OpenAI `prompt_tokens_details.cached_tokens`) — `None`
    /// when the provider doesn't report it. The state file's per-goal
    /// `token_metrics` are derived from these events (accumulated and divided
    /// by session count).
    ApiTokenUsage {
        goal_id: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        cached_tokens: Option<u64>,
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
    /// or the caller's goal was not found in the goal tree).
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
    /// Emitted at startup when an eager-start precondition fails — the
    /// invariant `goal-agents` requires before it can run (today: "tend
    /// is in the loaded goals"). The variant is named for the invariant
    /// rather than the specific goal so future eager-start preconditions
    /// can reuse it without an enum change.
    ///
    /// `missing_goal_id` is the goal id the invariant expected to find
    /// (today always `"tend"`). `expected_sources` lists the
    /// `(tier_label, path)` pairs that should have produced that goal —
    /// paralleling `GoalCollision.contributors`'s shape so `jq` queries
    /// can move between the two without translation.
    /// `parse_errors_in_tier` carries any `(file_path, short_error)`
    /// pairs for malformed TOMLs in the relevant tier, so a single
    /// event carries both the symptom (goal missing) and the cause
    /// (malformed file that should have produced it).
    ///
    /// Emitted via the fatal-events writer (`start_fatal_logger`) so the
    /// event lands on disk before the panic that triggers it unwinds —
    /// the regular batched logger is not yet running at this point in
    /// startup, and any buffered events would be lost when the runtime
    /// tears down.
    EagerStartPreconditionMissing {
        missing_goal_id: String,
        expected_sources: Vec<(String, String)>,
        parse_errors_in_tier: Vec<(String, String)>,
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

/// Internal dispatch for `LogSender`. Two modes:
///
/// - `Async`: the normal path — events go through an unbounded channel
///   to a background task that batches and flushes asynchronously. This
///   is the only mode that touches the state file.
/// - `Sync`: the fatal path — events are serialized and written
///   synchronously to the on-disk JSONL on every emit, with a flush
///   before `emit` returns. Used for events emitted from panic-prone
///   startup paths (eager-start precondition failures) where the
///   background task isn't running yet or may be torn down by the
///   panic before its batch flushes. The state file is not touched in
///   this mode — fatal events are pure log records, not live UI facts.
///
/// Both modes can target the same on-disk JSONL safely: POSIX
/// `O_APPEND` positions each `write()` syscall at end-of-file
/// atomically, and the sync path's per-line writes are far smaller than
/// any kernel write-atomicity threshold.
/// Boxed synchronous writer for the fatal path. The trait object
/// keeps the underlying filesystem handle out of this module's
/// source, per the raw-effects confinement rule that confines direct
/// filesystem calls to `realfs.rs` and `native.rs`. `Filesystem::open_append`
/// is the intended caller-side opener.
type SyncWriter = Box<dyn std::io::Write + Send>;

#[derive(Clone)]
enum LogSenderInner {
    Async(mpsc::UnboundedSender<LogEntry>),
    Sync(Arc<std::sync::Mutex<SyncWriter>>),
}

#[derive(Clone)]
pub struct LogSender {
    inner: LogSenderInner,
}

impl LogSender {
    pub fn emit(&self, source: &str, event: LogEvent) {
        let entry = LogEntry {
            ts: now_iso8601(),
            source: source.to_string(),
            event,
        };
        match &self.inner {
            LogSenderInner::Async(tx) => {
                let _ = tx.send(entry);
            }
            LogSenderInner::Sync(writer) => {
                // Failure to serialize or write is silently dropped:
                // the async path does the same on a closed channel,
                // and the panic that triggered the fatal emit is
                // already in flight — a logging failure here cannot be
                // allowed to mask the underlying panic.
                if let Ok(json) = serde_json::to_string(&entry) {
                    use std::io::Write;
                    if let Ok(mut w) = writer.lock() {
                        let _ = writeln!(w, "{}", json);
                        let _ = w.flush();
                    }
                }
            }
        }
    }
}

/// No-op sender for tests and contexts that don't need logging.
#[cfg(test)]
pub fn noop_sender() -> LogSender {
    let (tx, _rx) = mpsc::unbounded_channel();
    LogSender {
        inner: LogSenderInner::Async(tx),
    }
}

// ── Startup ──────────────────────────────────────────────────────────────────

/// Per-goal token-usage stats reconstructed by replaying the event log on
/// startup. This is the durable-persistence surface: the event log is the
/// source of truth, and this function reconstructs the in-memory state from
/// it. Deleting the log file = clean slate (the user's reset mental model).
///
/// `session_count` is derived from `goal_session_finished` events;
/// `total_prompt_tokens` / `total_completion_tokens` / `total_cached_tokens`
/// are accumulated from `api_token_usage` events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoricTokenStats {
    pub session_count: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cached_tokens: u64,
}

/// Replay the event log to reconstruct per-goal token-usage stats for
/// restart persistence. Reads `runtime.jsonl` line-by-line, parsing each
/// line as JSON and extracting the `kind` discriminator to decide which
/// events contribute to token metrics.
///
/// Only two event kinds matter:
/// - `api_token_usage` → accumulate prompt/completion/cached tokens
/// - `goal_session_finished` → increment session_count
///
/// Malformed lines and unknown event kinds are silently skipped — the log
/// is append-only and may contain lines from older schema versions.
///
/// Returns an empty map when the log file doesn't exist (fresh install)
/// or contains no token-related events (pre-feature logs).
pub fn load_historic_token_metrics(log_path: &std::path::Path) -> std::io::Result<HashMap<String, HistoricTokenStats>> {
    let mut metrics: HashMap<String, HistoricTokenStats> = HashMap::new();

    let file = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(metrics);
        }
        Err(e) => return Err(e),
    };

    use std::io::BufRead;
    for line in std::io::BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        // Parse as a generic JSON value — we don't need the full typed
        // enum, just the kind discriminator and the token fields. This
        // is robust against schema changes (new fields are ignored,
        // removed fields just return None from .get()).
        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let kind = val.get("kind").and_then(|k| k.as_str()).unwrap_or("");

        match kind {
            "api_token_usage" => {
                let goal_id = match val.get("goal_id").and_then(|g| g.as_str()) {
                    Some(g) => g.to_string(),
                    None => continue,
                };
                let stats = metrics.entry(goal_id).or_default();
                if let Some(pt) = val.get("prompt_tokens").and_then(|v| v.as_u64()) {
                    stats.total_prompt_tokens += pt;
                }
                if let Some(ct) = val.get("completion_tokens").and_then(|v| v.as_u64()) {
                    stats.total_completion_tokens += ct;
                }
                if let Some(cached) = val.get("cached_tokens").and_then(|v| v.as_u64()) {
                    stats.total_cached_tokens += cached;
                }
            }
            "goal_session_finished" => {
                let goal_id = match val.get("goal_id").and_then(|g| g.as_str()) {
                    Some(g) => g.to_string(),
                    None => continue,
                };
                let stats = metrics.entry(goal_id).or_default();
                stats.session_count += 1;
            }
            _ => {}
        }
    }

    Ok(metrics)
}

/// Convert `HistoricTokenStats` (the replay result) into the logger's
/// `GoalTokenMetrics` (the state-snapshot type). Used to seed the logger
/// task's initial `StateSnapshot.token_metrics` on startup.
fn historic_to_metrics(stats: &HistoricTokenStats) -> GoalTokenMetrics {
    let mut m = GoalTokenMetrics {
        session_count: stats.session_count as usize,
        total_prompt_tokens: stats.total_prompt_tokens,
        total_completion_tokens: stats.total_completion_tokens,
        ..GoalTokenMetrics::default()
    };
    m.recalc();
    m
}

pub fn start_logger(log_path: PathBuf, state_path: PathBuf) -> LogSender {
    let (tx, rx) = mpsc::unbounded_channel::<LogEntry>();

    // Replay the event log to seed initial token metrics so historic data
    // survives restarts. The state snapshot's token_metrics start populated
    // from the log rather than empty. Deleting the log file = clean slate
    // (the user's reset mental model). A non-existent or empty log produces
    // an empty map (fresh install).
    let initial_metrics = load_historic_token_metrics(&log_path)
        .unwrap_or_default()
        .into_iter()
        .map(|(goal_id, stats)| (goal_id, historic_to_metrics(&stats)))
        .collect::<HashMap<String, GoalTokenMetrics>>();

    tokio::spawn(logger_task(rx, log_path, state_path, initial_metrics));
    LogSender {
        inner: LogSenderInner::Async(tx),
    }
}

/// Wrap an already-opened append-mode writer as a synchronous fatal-event
/// `LogSender` usable before `start_logger` runs.
///
/// Emits in this mode write directly to the supplied writer and flush
/// synchronously on every call, so an event emitted just before a
/// panic unwinds is durable on disk by the time `emit` returns. The
/// fatal path does not touch the state file — only the runtime event
/// log is updated, because fatal-path events describe startup
/// invariants that are not live UI facts.
///
/// The writer is passed in by the composition root rather than opened
/// here: this keeps direct filesystem references out of `logger.rs`
/// per the raw-effects confinement rule (see `Filesystem::open_append`,
/// which is the intended caller-side opener). The returned `LogSender`
/// is interchangeable with the one from `start_logger` for the
/// purposes of `emit`; the difference is only the durability
/// guarantee. Both handles can coexist — fatal emits during early
/// startup, normal startup of the batched logger after the fatal path
/// is no longer needed.
pub fn start_fatal_logger(writer: Box<dyn std::io::Write + Send>) -> LogSender {
    LogSender {
        inner: LogSenderInner::Sync(Arc::new(std::sync::Mutex::new(writer))),
    }
}

// ── Background task ──────────────────────────────────────────────────────────

async fn logger_task(
    mut rx: mpsc::UnboundedReceiver<LogEntry>,
    log_path: PathBuf,
    state_path: PathBuf,
    initial_token_metrics: HashMap<String, GoalTokenMetrics>,
) {
    let mut batch: Vec<String> = Vec::new();
    let mut state = StateSnapshot {
        token_metrics: initial_token_metrics,
        ..StateSnapshot::default()
    };
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
            // Increment session count and recalc token averages — a finished
            // dispatch is a completed session for metric purposes.
            let metrics = state.token_metrics.entry(goal_id.clone()).or_default();
            metrics.session_count += 1;
            metrics.recalc();
            true
        }
        LogEvent::ApiTokenUsage {
            goal_id,
            prompt_tokens,
            completion_tokens,
            ..
        } => {
            let metrics = state.token_metrics.entry(goal_id.clone()).or_default();
            metrics.total_prompt_tokens += prompt_tokens;
            metrics.total_completion_tokens += completion_tokens;
            metrics.recalc();
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

    // spec (send-message): the `SendMessageDispatched` log event serializes
    // with `kind = "send_message_dispatched"` and carries the sender, target,
    // and success fields.  The optional `error` field is set on failure with
    // the registry-miss reason.
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
    // pane in the state snapshot's scroll_offsets. All three pane names are handled;
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
    // `"ancestor global"`, `"packaged"`, `"binary-relative-packaged"`
    // are the current set; new tiers may be added by `goal::tier_label`
    // without changing this event's shape.
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

    // spec (tend-introspection / binary-relative-tier-loading): the
    // eager-start precondition failure emits a structured event so the
    // missing-goal symptom is queryable via `jq` rather than only
    // visible in the panic prose. The event is named for the invariant,
    // not the specific goal, so future eager-start preconditions can
    // reuse the variant without changing this enum.
    #[test]
    fn test_spec_eager_start_precondition_missing_event_serializes() {
        // Common case: the loader produced zero entries with the
        // expected id, and there's no underlying parse error to report.
        let event = LogEvent::EagerStartPreconditionMissing {
            missing_goal_id: "tend".to_string(),
            expected_sources: vec![(
                "binary-relative-packaged".to_string(),
                "<binary>/../../.tinker/goals/packaged-goals/tend.toml"
                    .to_string(),
            )],
            parse_errors_in_tier: vec![],
        };
        let entry = LogEntry {
            ts: "2026-07-20T00:00:00Z".to_string(),
            source: "harness".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(val["kind"], "eager_start_precondition_missing");
        assert_eq!(val["missing_goal_id"], "tend");
        let sources = val["expected_sources"]
            .as_array()
            .expect("expected_sources must serialize as a JSON array");
        assert_eq!(sources.len(), 1, "one expected source");
        assert_eq!(sources[0][0], "binary-relative-packaged");
        assert_eq!(
            sources[0][1],
            "<binary>/../../.tinker/goals/packaged-goals/tend.toml"
        );
        let errs = val["parse_errors_in_tier"]
            .as_array()
            .expect("parse_errors_in_tier must serialize as a JSON array");
        assert_eq!(errs.len(), 0, "no parse errors in this case");

        // Populated parse_errors_in_tier: the loader caught a malformed
        // TOML in the tier that should have produced the goal. A single
        // event must carry both the symptom and the cause so a `jq`
        // query recovers both without re-parsing the panic prose.
        let event_with_errs = LogEvent::EagerStartPreconditionMissing {
            missing_goal_id: "tend".to_string(),
            expected_sources: vec![(
                "binary-relative-packaged".to_string(),
                "<binary>/../../.tinker/goals/packaged-goals/tend.toml"
                    .to_string(),
            )],
            parse_errors_in_tier: vec![(
                "<binary>/../../.tinker/goals/packaged-goals/tend.toml"
                    .to_string(),
                "missing field `summary` at line 5".to_string(),
            )],
        };
        let entry_with_errs = LogEntry {
            ts: "2026-07-20T00:00:00Z".to_string(),
            source: "harness".to_string(),
            event: event_with_errs,
        };
        let json_with_errs = serde_json::to_string(&entry_with_errs).unwrap();
        let val_with_errs: serde_json::Value = serde_json::from_str(&json_with_errs).unwrap();
        let errs = val_with_errs["parse_errors_in_tier"].as_array().unwrap();
        assert_eq!(errs.len(), 1, "one parse error populated");
        assert_eq!(
            errs[0][0],
            "<binary>/../../.tinker/goals/packaged-goals/tend.toml"
        );
        assert!(errs[0][1].as_str().unwrap().contains("missing field"));

        // The event is not state-changing — apply_to_state returns false,
        // because eager-start failures are structural (deployment-level)
        // rather than live UI facts.
        let mut state = StateSnapshot::default();
        let changed = apply_to_state(&entry, &mut state);
        assert!(
            !changed,
            "EagerStartPreconditionMissing must not mark state dirty"
        );
    }

    // spec (tend-introspection): the fatal-events writer emits
    // synchronously to the on-disk JSONL on every call. This is the
    // durability guarantee that makes
    // `EagerStartPreconditionMissing` observable when it fires from
    // inside a panic-prone startup path — a regular batched logger
    // would lose buffered events when the runtime is torn down by the
    // panic. Verifies that the line is on disk before `emit` returns
    // and that subsequent emits append rather than truncate.
    #[test]
    fn test_spec_start_fatal_logger_writes_synchronously_on_emit() {
        // Per-test temp path — cargo runs tests in parallel, so the
        // filename includes the test name to avoid collisions.
        let tmp = std::env::temp_dir().join(
            "tinker-test-fatal-eager-start-precondition-missing.jsonl",
        );
        let _ = std::fs::remove_file(&tmp);

        // Tests live outside the raw-effects confinement rule, so
        // opening the file directly here mirrors what `realfs.rs`'s
        // `open_append` does at runtime.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp)
            .expect("test must be able to open the log file");
        let log = start_fatal_logger(Box::new(file));
        log.emit(
            "harness",
            LogEvent::EagerStartPreconditionMissing {
                missing_goal_id: "tend".to_string(),
                expected_sources: vec![(
                    "binary-relative-packaged".to_string(),
                    "<binary>/../../.tinker/goals/packaged-goals/tend.toml"
                        .to_string(),
                )],
                parse_errors_in_tier: vec![],
            },
        );

        // The event must be on disk before emit returns — that's the
        // whole point of the sync writer.
        let content = std::fs::read_to_string(&tmp)
            .expect("event must be written to disk before emit returns");
        assert!(
            content.contains("\"kind\":\"eager_start_precondition_missing\""),
            "fatal-writer line must contain the new event kind; got: {content}"
        );
        assert!(
            content.contains("\"missing_goal_id\":\"tend\""),
            "fatal-writer line must carry missing_goal_id; got: {content}"
        );
        assert!(
            content.contains("\"source\":\"harness\""),
            "fatal-writer line must carry the source field; got: {content}"
        );

        // Append-on-second-emit: the file is opened with `O_APPEND`,
        // so subsequent emits land after the first line rather than
        // truncating it. Mirrors the regular logger's no-rotation
        // guarantee from a different code path.
        log.emit(
            "harness",
            LogEvent::EagerStartPreconditionMissing {
                missing_goal_id: "tend".to_string(),
                expected_sources: vec![],
                parse_errors_in_tier: vec![],
            },
        );
        let after = std::fs::read_to_string(&tmp)
            .expect("second emit must also be on disk");
        let line_count = after.lines().count();
        assert_eq!(
            line_count, 2,
            "fatal writer must append, not truncate; got {line_count} lines"
        );

        // Cleanup.
        let _ = std::fs::remove_file(&tmp);
    }

    // ── ApiTokenUsage event tests ────────────────────────────────────────────

    // spec (tend-introspection): ApiTokenUsage serializes with kind =
    // "api_token_usage" and carries goal_id, prompt_tokens, completion_tokens,
    // total_tokens, and optional cached_tokens.
    #[test]
    fn test_spec_api_token_usage_serializes() {
        let event = LogEvent::ApiTokenUsage {
            goal_id: "tui".to_string(),
            prompt_tokens: 15000,
            completion_tokens: 3000,
            total_tokens: 18000,
            cached_tokens: Some(5000),
        };
        let entry = LogEntry {
            ts: "2026-06-15T00:00:00Z".to_string(),
            source: "backend".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["kind"], "api_token_usage");
        assert_eq!(val["source"], "backend");
        assert_eq!(val["goal_id"], "tui");
        assert_eq!(val["prompt_tokens"], 15000);
        assert_eq!(val["completion_tokens"], 3000);
        assert_eq!(val["total_tokens"], 18000);
        assert_eq!(val["cached_tokens"], 5000);
    }

    // spec (tend-introspection): ApiTokenUsage with no cached_tokens
    // serializes cached_tokens as null.
    #[test]
    fn test_spec_api_token_usage_no_cached_tokens() {
        let event = LogEvent::ApiTokenUsage {
            goal_id: "backends".to_string(),
            prompt_tokens: 2000,
            completion_tokens: 500,
            total_tokens: 2500,
            cached_tokens: None,
        };
        let entry = LogEntry {
            ts: "2026-06-15T00:00:00Z".to_string(),
            source: "backend".to_string(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["cached_tokens"], serde_json::Value::Null);
    }

    // spec (tend-introspection): ApiTokenUsage is a state-changing event —
    // it accumulates into token_metrics in the state snapshot.
    #[test]
    fn test_spec_api_token_usage_updates_state() {
        let mut state = StateSnapshot::default();
        let entry = LogEntry {
            ts: "2026-06-15T00:00:00Z".to_string(),
            source: "backend".to_string(),
            event: LogEvent::ApiTokenUsage {
                goal_id: "tui".to_string(),
                prompt_tokens: 10000,
                completion_tokens: 2000,
                total_tokens: 12000,
                cached_tokens: Some(3000),
            },
        };
        let changed = apply_to_state(&entry, &mut state);
        assert!(changed, "ApiTokenUsage must mark state dirty");
        let metrics = &state.token_metrics["tui"];
        assert_eq!(metrics.total_prompt_tokens, 10000);
        assert_eq!(metrics.total_completion_tokens, 2000);
        // Averages are 0 before any session finishes (session_count is 0,
        // recalc divides by max(1)).
        assert_eq!(metrics.session_count, 0);
        // With session_count=0, recalc uses max(1)=1, so averages reflect
        // raw totals — this is the pre-completion state.
        assert_eq!(metrics.avg_input_tokens_per_session, 10000);
        assert_eq!(metrics.avg_output_tokens_per_session, 2000);
    }

    // spec (tend-introspection): ApiTokenUsage events accumulate across
    // multiple calls, and GoalSessionFinished increments session_count so
    // averages are total/session_count.
    #[test]
    fn test_spec_token_metrics_accumulate_and_average() {
        let mut state = StateSnapshot::default();
        let mk = |ev: LogEvent| LogEntry {
            ts: "2026-06-15T00:00:00Z".to_string(),
            source: "backend".to_string(),
            event: ev,
        };

        // Two API calls in session 1.
        apply_to_state(
            &mk(LogEvent::ApiTokenUsage {
                goal_id: "tui".into(),
                prompt_tokens: 10000,
                completion_tokens: 1000,
                total_tokens: 11000,
                cached_tokens: None,
            }),
            &mut state,
        );
        apply_to_state(
            &mk(LogEvent::ApiTokenUsage {
                goal_id: "tui".into(),
                prompt_tokens: 12000,
                completion_tokens: 1500,
                total_tokens: 13500,
                cached_tokens: None,
            }),
            &mut state,
        );

        // Session 1 finishes.
        apply_to_state(
            &mk(LogEvent::GoalSessionFinished {
                goal_id: "tui".into(),
                exit_status: "clean".into(),
                duration_ms: 5000,
                files_modified_count: 0,
                files_modified: vec![],
                tool_calls: 2,
                summary_chars: 0,
                full_output: "".into(),
                backend: "native".into(),
            }),
            &mut state,
        );

        // Session 2: one call.
        apply_to_state(
            &mk(LogEvent::ApiTokenUsage {
                goal_id: "tui".into(),
                prompt_tokens: 18000,
                completion_tokens: 2000,
                total_tokens: 20000,
                cached_tokens: Some(5000),
            }),
            &mut state,
        );

        // Session 2 finishes.
        apply_to_state(
            &mk(LogEvent::GoalSessionFinished {
                goal_id: "tui".into(),
                exit_status: "clean".into(),
                duration_ms: 3000,
                files_modified_count: 1,
                files_modified: vec!["src/x.rs".into()],
                tool_calls: 1,
                summary_chars: 0,
                full_output: "".into(),
                backend: "native".into(),
            }),
            &mut state,
        );

        let m = &state.token_metrics["tui"];
        assert_eq!(m.session_count, 2);
        assert_eq!(m.total_prompt_tokens, 10000 + 12000 + 18000); // 40000
        assert_eq!(m.total_completion_tokens, 1000 + 1500 + 2000); // 4500
        // Averages: total / session_count
        assert_eq!(m.avg_input_tokens_per_session, 40000 / 2); // 20000
        assert_eq!(m.avg_output_tokens_per_session, 4500 / 2); // 2250
    }

    // spec (tend-introspection): multiple goals tracked independently in
    // the token_metrics map.
    #[test]
    fn test_spec_token_metrics_per_goal_isolation() {
        let mut state = StateSnapshot::default();
        let mk = |ev: LogEvent| LogEntry {
            ts: "2026-06-15T00:00:00Z".to_string(),
            source: "backend".to_string(),
            event: ev,
        };

        apply_to_state(
            &mk(LogEvent::ApiTokenUsage {
                goal_id: "tui".into(),
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: None,
            }),
            &mut state,
        );
        apply_to_state(
            &mk(LogEvent::ApiTokenUsage {
                goal_id: "backends".into(),
                prompt_tokens: 500,
                completion_tokens: 200,
                total_tokens: 700,
                cached_tokens: None,
            }),
            &mut state,
        );

        assert_eq!(state.token_metrics.len(), 2);
        assert_eq!(state.token_metrics["tui"].total_prompt_tokens, 100);
        assert_eq!(state.token_metrics["backends"].total_prompt_tokens, 500);
    }

    // spec (tend-introspection): the state snapshot's token_metrics
    // serializes to JSON with the derived average fields and without the
    // internal running totals (serde(skip)).
    #[test]
    fn test_spec_state_snapshot_serializes_token_metrics() {
        let mut state = StateSnapshot::default();
        state.token_metrics.insert("tui".into(), GoalTokenMetrics {
            session_count: 3,
            avg_input_tokens_per_session: 15000,
            avg_output_tokens_per_session: 3000,
            total_prompt_tokens: 45000,
            total_completion_tokens: 9000,
        });
        let json = serde_json::to_string(&state).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let m = &val["token_metrics"]["tui"];
        assert_eq!(m["session_count"], 3);
        assert_eq!(m["avg_input_tokens_per_session"], 15000);
        assert_eq!(m["avg_output_tokens_per_session"], 3000);
        // Internal fields must NOT be serialized.
        assert!(
            m.get("total_prompt_tokens").is_none(),
            "total_prompt_tokens must be serde(skip)"
        );
        assert!(
            m.get("total_completion_tokens").is_none(),
            "total_completion_tokens must be serde(skip)"
        );
    }

    // ── Historic replay tests ─────────────────────────────────────────────────

    // spec (tend-introspection): load_historic_token_metrics reads the
    // event log and reconstructs per-goal token stats from api_token_usage
    // and goal_session_finished events.
    #[test]
    fn test_spec_load_historic_token_metrics_replays_log() {
        let tmp = std::env::temp_dir().join(format!(
            "tinker_test_replay_{}.jsonl",
            std::process::id()
        ));

        // Write a log with mixed events.
        let lines = [
            r#"{"ts":"2026-01-01T00:00:00Z","source":"harness","kind":"tinker_system_message_received","content":"start"}"#,
            r#"{"ts":"2026-01-01T00:01:00Z","source":"backend","kind":"api_token_usage","goal_id":"tui","prompt_tokens":1000,"completion_tokens":200,"total_tokens":1200,"cached_tokens":null}"#,
            r#"{"ts":"2026-01-01T00:02:00Z","source":"backend","kind":"api_token_usage","goal_id":"tui","prompt_tokens":1500,"completion_tokens":300,"total_tokens":1800,"cached_tokens":500}"#,
            r#"{"ts":"2026-01-01T00:03:00Z","source":"goal_session","kind":"goal_session_finished","goal_id":"tui","exit_status":"clean","duration_ms":5000,"files_modified_count":0,"files_modified":[],"tool_calls":2,"summary_chars":0,"full_output":"...","backend":"native"}"#,
            r#"{"ts":"2026-01-01T00:04:00Z","source":"backend","kind":"api_token_usage","goal_id":"backends","prompt_tokens":5000,"completion_tokens":1000,"total_tokens":6000,"cached_tokens":null}"#,
        ];
        let content = lines.join("\n");
        std::fs::write(&tmp, &content).unwrap();

        let metrics = load_historic_token_metrics(&tmp).expect("replay must succeed");

        assert_eq!(metrics.len(), 2, "two goals must be tracked");

        let tui = &metrics["tui"];
        assert_eq!(tui.session_count, 1);
        assert_eq!(tui.total_prompt_tokens, 2500);
        assert_eq!(tui.total_completion_tokens, 500);
        assert_eq!(tui.total_cached_tokens, 500);

        let backends = &metrics["backends"];
        assert_eq!(backends.session_count, 0);
        assert_eq!(backends.total_prompt_tokens, 5000);
        assert_eq!(backends.total_completion_tokens, 1000);

        let _ = std::fs::remove_file(&tmp);
    }

    // spec (tend-introspection): load_historic_token_metrics returns an
    // empty map for a non-existent log file (fresh install).
    #[test]
    fn test_spec_load_historic_token_metrics_missing_file() {
        let path = std::path::Path::new("/nonexistent/tinker_test_does_not_exist.jsonl");
        let metrics = load_historic_token_metrics(path).expect("missing file must return Ok(empty)");
        assert!(metrics.is_empty(), "missing log file must produce empty metrics");
    }

    // spec (tend-introspection): load_historic_token_metrics skips
    // malformed lines silently — the log is append-only and may contain
    // lines from older schema versions or corrupted entries.
    #[test]
    fn test_spec_load_historic_token_metrics_skips_malformed() {
        let tmp = std::env::temp_dir().join(format!(
            "tinker_test_replay_malformed_{}.jsonl",
            std::process::id()
        ));

        let content = [
            r#"{"ts":"2026-01-01T00:00:00Z","source":"backend","kind":"api_token_usage","goal_id":"tui","prompt_tokens":1000,"completion_tokens":200,"total_tokens":1200,"cached_tokens":null}"#,
            "this is not valid json at all",
            "",
            r#"{"incomplete":"json"}"#,
            r#"{"ts":"2026-01-01T00:01:00Z","source":"backend","kind":"api_token_usage","goal_id":"tui","prompt_tokens":500,"completion_tokens":100,"total_tokens":600,"cached_tokens":null}"#,
        ]
        .join("\n");
        std::fs::write(&tmp, &content).unwrap();

        let metrics = load_historic_token_metrics(&tmp).expect("replay must succeed despite malformed lines");

        let tui = &metrics["tui"];
        assert_eq!(tui.total_prompt_tokens, 1500, "must accumulate across valid lines, skipping malformed");
        assert_eq!(tui.total_completion_tokens, 300);

        let _ = std::fs::remove_file(&tmp);
    }

    // spec (tend-introspection): historic_to_metrics converts the replay
    // result into GoalTokenMetrics with correct derived averages.
    #[test]
    fn test_spec_historic_to_metrics_derives_averages() {
        let stats = HistoricTokenStats {
            session_count: 2,
            total_prompt_tokens: 40000,
            total_completion_tokens: 6000,
            total_cached_tokens: 10000,
        };
        let metrics = historic_to_metrics(&stats);
        assert_eq!(metrics.session_count, 2);
        assert_eq!(metrics.total_prompt_tokens, 40000);
        assert_eq!(metrics.total_completion_tokens, 6000);
        assert_eq!(metrics.avg_input_tokens_per_session, 20000);
        assert_eq!(metrics.avg_output_tokens_per_session, 3000);
    }
}
