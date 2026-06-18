//! Capability interfaces — every effect tinker performs is declared here.
//!
//! Business logic depends only on these traits. Real implementations live in
//! `main.rs` (the composition root) where they are wired into the running app.
//! Tests can inject mocks against the same interfaces.

use anyhow::Result;
use async_trait::async_trait;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Streaming text token callback. Synchronous so trait methods can be `dyn`-safe.
pub type Chunk = Box<dyn FnMut(String) + Send>;

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
/// an error to the model — the same harness signal that the feature is not
/// wired.  When `Some`, the callback fires a fresh sub-session of the
/// *caller's own goal* and returns the new sub-session id.  Unlike
/// `send_message`, no arbitrary target parameter is exposed to the model —
/// the routing target is implicit in the closure (the dispatcher's session
/// id) so the self-only constraint is enforced at the harness layer, not
/// the schema.  The runner just routes the tool call into the callback.
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
        send_message: Option<SendMessageFn>,
        spawn_session: Option<SpawnSessionFn>,
    ) -> Result<String>;
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
