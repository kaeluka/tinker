//! Capability interfaces — every effect tinker performs is declared here.
//!
//! Business logic depends only on these traits. Real implementations live in
//! `main.rs` (the composition root) where they are wired into the running app.
//! Tests can inject mocks against the same interfaces.

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Streaming text token callback. Synchronous so trait methods can be `dyn`-safe.
pub type Chunk = Box<dyn FnMut(String) + Send>;

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
#[async_trait]
pub trait OpenCodeRunner: Send + Sync {
    async fn run(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &Path,
        system_prompt: Option<&str>,
        on_session_id: Chunk,
        on_chunk: Chunk,
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
}
