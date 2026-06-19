//! Capability interfaces — every effect tinker performs is declared here.
//!
//! Business logic depends only on these traits. Real implementations live in
//! `main.rs` (the composition root) where they are wired into the running app.
//! Tests can inject mocks against the same interfaces.

use anyhow::Result;
use async_trait::async_trait;
use std::error::Error;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Streaming text token callback. Synchronous so trait methods can be `dyn`-safe.
pub type Chunk = Box<dyn FnMut(String) + Send>;

/// Per-API-call token usage from a single LLM round-trip. The OpenAI
/// chat-completions protocol returns a `usage` object alongside `choices`;
/// this struct carries the fields that matter for cost tracking.
///
/// `cached_tokens` is a provider-specific extension (e.g. Anthropic prompt
/// caching, OpenAI `prompt_tokens_details.cached_tokens`). `None` when the
/// provider doesn't report it — some local model servers omit `usage`
/// entirely, in which case the callback doesn't fire at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: Option<u64>,
}

/// Callback for per-API-call token usage. Fires once per LLM round-trip
/// inside the tool loop, after each successful response parse. Same pattern
/// as `Chunk` — synchronous, `FnMut` so the callback can close over mutable
/// state (e.g. an accumulator or a `LogSender`).
pub type UsageChunk = Box<dyn FnMut(TokenUsage) + Send>;

/// Error returned by [`OpenCodeRunner::run`].
///
/// Distinguishes two failure modes that previously shared a single `Err`:
///
/// - **Lost** (`preserved_session_id == None`): the session's history is no
///   longer recoverable — e.g. context-overflow, where the backend has dropped
///   the session from its in-memory store. The caller must start the next
///   dispatch as a fresh session.
/// - **Transient** (`preserved_session_id == Some(sid)`): the run failed but
///   the session's history was preserved (saved to the backend's store before
///   the failure surfaced). A subsequent dispatch can resume from `sid`
///   instead of rebuilding a fresh session — this is the fix for the bug where
///   a single network blip wiped every prior turn from the model's view.
///
/// `RunError` impls `std::error::Error`, so callers that propagate via `?`
/// into an `anyhow::Result` (e.g. one-shot test harnesses) get the conversion
/// for free via anyhow's blanket impl.
#[derive(Debug, Clone)]
pub struct RunError {
    /// Human-readable failure reason. Surfaced to the user via the ⚠ chunk
    /// before `run` returns and recorded in the run's `exit_status` log.
    pub message: String,
    /// `Some(sid)` when the session history was saved before the failure and
    /// can be resumed; `None` when the session is genuinely lost and the next
    /// dispatch must start fresh.
    pub preserved_session_id: Option<String>,
}

impl RunError {
    /// Construct a "session lost" error — the caller must start fresh next
    /// dispatch. Used for context-overflow and unknown-session-id paths.
    pub fn lost(message: impl Into<String>) -> Self {
        Self { message: message.into(), preserved_session_id: None }
    }

    /// Construct a "session preserved" error — the caller should resume from
    /// `sid` on the next dispatch. Used for network failures, unparseable
    /// responses, non-overflow API errors, and missing-choices responses,
    /// where the backend has already saved the session history.
    pub fn transient(sid: impl Into<String>, message: impl Into<String>) -> Self {
        Self { message: message.into(), preserved_session_id: Some(sid.into()) }
    }

    /// True when the session history was preserved and can be resumed.
    pub fn session_preserved(&self) -> bool {
        self.preserved_session_id.is_some()
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.preserved_session_id {
            Some(sid) => write!(f, "{} (session preserved: {sid})", self.message),
            None => write!(f, "{} (session lost)", self.message),
        }
    }
}

impl Error for RunError {}

/// Callback for the `send_message` tool. Invoked when an agent's tool loop
/// encounters a `send_message(target, message)` call. The implementation
/// validates `target` against the session registry and delivers the message
/// to the named agent's input channel. Returns `Ok(confirmation)` on
/// successful dispatch and `Err(reason)` on unknown target / delivery
/// failure.
///
/// The callback is wrapped in `Arc` so a single closure can be cloned and
/// shared across every `run()` call of the shared runner — the runner is
/// reused for many goal agents and many turns, and each invocation needs
/// access to the dispatcher without taking ownership of the original.
/// `Send + Sync` is required so a shared reference can be held across
/// `.await` points inside the runner's async tool loop.
///
/// Signature: `fn(target, message) -> Result<confirmation, reason>`.
pub type SendMessageFn = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// Callback for the `spawn_session` tool. Invoked when an agent's tool loop
/// encounters a `spawn_session(subgoal, label)` call. The implementation
/// fires a fresh sub-session of the *caller's own goal* — the routing target
/// is implicit in the closure (derived from the dispatcher's session id),
/// not a parameter the model supplies. The `label` is `Some(tag)` when the
/// caller passed a correlation tag and `None` when the call omitted one
/// (label is an optional parameter on the tool schema).
///
/// Returns `Ok((session_id, label))` on successful enqueue: `session_id` is
/// the new sub-session's id (e.g. `"rummage~3"` or `"rummage~1~5"` for a
/// coordinator), `label` is the caller's correlation tag (preserved as-is
/// for the model to use in reply routing). Returns `Err(reason)` on failure
/// (e.g. the spawn handler is unavailable, or the caller's goal is not
/// in the goal tree).
///
/// The same `Arc` sharing and `Send + Sync` rationale as `SendMessageFn`
/// applies: a single closure is cloned for every `run()` call of the
/// shared runner. Signature: `fn(subgoal, label) -> Result<(session_id, label), reason>`.
pub type SpawnSessionFn = Arc<dyn Fn(&str, Option<&str>) -> Result<(String, Option<String>), String> + Send + Sync>;

/// Capability for invoking the `opencode` CLI.
///
/// `on_session_id` is called once when a session ID is first seen on the stream.
/// `on_chunk` is called for each streamed text chunk.
/// `on_usage` is called once per LLM round-trip with the token usage
/// (`prompt_tokens`, `completion_tokens`, `total_tokens`, optional
/// `cached_tokens`) from the API response. If the response has no `usage`
/// object (some local model servers), the callback doesn't fire — so
/// endpoints that don't report usage produce no token-usage log events.
/// Returns the session ID seen during the run (or empty string if none).
///
/// `system_prompt` is used only when `session_id` is `None` (new session) to
/// deliver the session-invariant context via the backend's system-prompt
/// mechanism (agent file for opencode, `--system-prompt` for claude).  Pass
/// `None` on resumed sessions — the backend already holds that context.
///
/// `send_message` is the dispatcher the runner uses when the model emits a
/// `send_message(target, message)` tool call.  When `None`, the tool returns
/// an error to the model — this is the harness's signal that the feature is
/// not wired (e.g. in a test mock that does not care about dispatch).  When
/// `Some`, the callback handles registry validation and delivery; the runner
/// just routes the tool call into it.
///
/// `spawn_session` is the dispatcher the runner uses when the model emits a
/// `spawn_session(subgoal, label)` tool call.  When `None`, the tool returns
/// an error to the model — the same harness signal that the feature is
/// not wired.  When `Some`, the callback fires a fresh sub-session of the
/// *caller's own goal* and returns the new sub-session id.  Unlike
/// `send_message`, no arbitrary target parameter is exposed to the model —
/// the routing target is implicit in the closure (the dispatcher's session
/// id) so the self-only constraint is enforced at the harness layer, not
/// the schema.  The runner just routes the tool call into the callback.
///
/// Returns `Ok(sid)` on success — `sid` is the session id seen during the
/// run (or empty string if the runner produced no events). On failure
/// returns [`RunError`], whose `preserved_session_id` field tells the
/// caller whether the session history was saved before the failure
/// (`Some(sid)` — next dispatch can resume) or genuinely lost (`None` —
/// next dispatch must start fresh). The caller MUST consult this field
/// rather than assuming every `Err` means a lost session: a transient
/// network failure preserves the session, and clearing the caller's
/// cached session id in that case silently throws away every prior turn.
#[async_trait]
pub trait OpenCodeRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &Path,
        system_prompt: Option<&str>,
        on_session_id: Chunk,
        on_chunk: Chunk,
        on_usage: UsageChunk,
        send_message: Option<SendMessageFn>,
        spawn_session: Option<SpawnSessionFn>,
    ) -> std::result::Result<String, RunError>;
}

/// Capability for filesystem operations on goal storage.
pub trait Filesystem: Send + Sync {
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn write(&self, path: &Path, content: &str) -> Result<()>;
    fn mkdir_all(&self, path: &Path) -> Result<()>;
    /// Returns paths of files ending in `.<ext>` (case-sensitive) directly under `dir`.
    fn list_files_with_ext(&self, dir: &Path, ext: &str) -> Result<Vec<PathBuf>>;
    /// Returns all entries (files and directories) directly under `dir`.
    /// Used for recursive tree walking.
    fn list_dir(&self, dir: &Path) -> Result<Vec<PathBuf>>;
    fn is_dir(&self, path: &Path) -> bool;
    /// Opens `path` in append mode for writing. Returns a boxed writer
    /// that owns the file handle. Used by the fatal-event logger
    /// (`logger::start_fatal_logger`) to keep direct filesystem
    /// references out of `logger.rs` — the composition root opens the
    /// log file via this method and hands the writer to
    /// `start_fatal_logger`.
    fn open_append(&self, path: &Path) -> Result<Box<dyn Write + Send>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec (backends): RunError distinguishes two failure modes — lost
    // (session genuinely gone, next dispatch must start fresh) and transient
    // (session history preserved, next dispatch can resume). The constructors
    // must set preserved_session_id accordingly so goal_agent_loop's Err arm
    // can branch on session_preserved() instead of guessing.
    #[test]
    fn test_spec_run_error_lost_has_no_preserved_session() {
        let e = RunError::lost("context overflow: too many tokens");
        assert_eq!(e.message, "context overflow: too many tokens");
        assert!(e.preserved_session_id.is_none(), "lost errors must not carry a sid");
        assert!(!e.session_preserved(), "session_preserved() must be false for lost");
    }

    #[test]
    fn test_spec_run_error_transient_carries_preserved_session() {
        let e = RunError::transient("nat_abc123", "backend request failed: connection refused");
        assert_eq!(e.message, "backend request failed: connection refused");
        assert_eq!(e.preserved_session_id.as_deref(), Some("nat_abc123"));
        assert!(e.session_preserved(), "session_preserved() must be true for transient");
    }

    // spec (backends): RunError must impl std::error::Error so it integrates
    // with anyhow's blanket From impl — callers that propagate via `?` into
    // an anyhow::Result (e.g. run_oc_oneshot) get the conversion for free.
    #[test]
    fn test_spec_run_error_implements_std_error_for_anyhow_compat() {
        fn asserts_std_error<E: std::error::Error>(_: &E) {}
        let e = RunError::lost("anything");
        asserts_std_error(&e);
        // Display must mention both the message and the preserved/lost state
        // so logs that format the error are self-explanatory.
        let display = format!("{}", e);
        assert!(display.contains("anything"), "Display must contain the message");
        assert!(display.contains("session lost"), "Display must mark lost errors");
        let t = RunError::transient("sid", "boom");
        let t_display = format!("{}", t);
        assert!(t_display.contains("session preserved"), "Display must mark transient errors");
    }
}
