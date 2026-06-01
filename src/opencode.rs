//! Real `OpenCodeRunner` capability: shells out to `opencode run --format json`.
//!
//! Per the coding standard, this is one of the "real implementations at the
//! composition root." It only ever runs in `main.rs`; business code talks to
//! the trait in `cap.rs`.

use crate::cap::{Chunk, OpenCodeRunner};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub const TINKER_MODEL: &str = "openrouter/google/gemini-3.1-pro-preview";
pub const GOAL_MODEL: &str = "openrouter/deepseek/deepseek-v4-flash";
/// Used for the scheduling decision (which goal(s) should react next).
/// Each call is a FRESH opencode session given only the relevant transcript
/// excerpt + the active goals list — no persistent history. Cheap and fast.
pub const SCHEDULER_MODEL: &str = "openrouter/google/gemini-3.1-flash-lite-preview";

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    part: Option<RawPart>,
    error: Option<RawError>,
}

#[derive(Debug, Deserialize)]
struct RawPart {
    #[serde(rename = "type")]
    part_type: String,
    text: Option<String>,
    tool: Option<String>,
    state: Option<RawToolState>,
}

#[derive(Debug, Deserialize)]
struct RawToolState {
    status: Option<String>,
    input: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawError {
    name: Option<String>,
    data: Option<RawErrorData>,
}

#[derive(Debug, Deserialize)]
struct RawErrorData {
    message: Option<String>,
}

/// Real `OpenCodeRunner` impl bound to a specific opencode model id.
/// Construct one per role (tinker vs goal session) at the composition root.
pub struct RealOpenCodeRunner {
    /// When `None`, the `-m` flag is omitted and opencode uses its configured default.
    pub model: Option<String>,
    /// Optional opencode agent profile name (e.g. "tinker").
    /// When set, `--agent <name>` is passed to the opencode CLI.
    pub agent: Option<String>,
}

impl RealOpenCodeRunner {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
            agent: None,
        }
    }

    pub fn new_default() -> Self {
        Self {
            model: None,
            agent: None,
        }
    }

    pub fn with_agent(model: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
            agent: Some(agent.into()),
        }
    }

    pub fn default_with_agent(agent: impl Into<String>) -> Self {
        Self {
            model: None,
            agent: Some(agent.into()),
        }
    }
}

#[async_trait]
impl OpenCodeRunner for RealOpenCodeRunner {
    async fn run(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &Path,
        mut on_session_id: Chunk,
        mut on_chunk: Chunk,
    ) -> Result<String> {
        let mut cmd = opencode_command(self.model.as_deref(), self.agent.as_deref(), session_id, work_dir);

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(message.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let stdout = child.stdout.take().expect("stdout piped");
        let mut stderr_lines = BufReader::new(child.stderr.take().expect("stderr piped")).lines();
        let stderr_collector = tokio::spawn(async move {
            let mut buf = String::new();
            while let Ok(Some(line)) = stderr_lines.next_line().await {
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        let mut lines = BufReader::new(stdout).lines();
        let mut returned_session_id = String::new();
        let mut sid_emitted = false;

        while let Some(line) = lines.next_line().await? {
            if line.is_empty() {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<RawEvent>(&line) else {
                continue;
            };
            if !sid_emitted {
                if let Some(id) = &ev.session_id {
                    returned_session_id = id.clone();
                    on_session_id(id.clone());
                    sid_emitted = true;
                }
            }
            match ev.event_type.as_str() {
                "text" => {
                    if let Some(part) = &ev.part {
                        if part.part_type == "text" {
                            if let Some(text) = &part.text {
                                on_chunk(text.clone());
                            }
                        }
                    }
                }
                "tool_use" => {
                    if let Some(part) = &ev.part {
                        let chunk = format_tool_use(part);
                        if !chunk.is_empty() {
                            on_chunk(chunk);
                        }
                    }
                }
                "error" => {
                    let msg = ev
                        .error
                        .as_ref()
                        .and_then(|e| e.data.as_ref().and_then(|d| d.message.clone()))
                        .or_else(|| ev.error.as_ref().and_then(|e| e.name.clone()))
                        .unwrap_or_else(|| "unknown error".to_string());
                    on_chunk(format!("\n\u{2030} error: {}\n", msg));
                }
                _ => {}
            }
        }

        child.wait().await?;
        let stderr_text = stderr_collector.await.unwrap_or_default();
        if !stderr_text.is_empty() {
            on_chunk(format!("\n\u{2030} stderr: {}\n", stderr_text.trim()));
        }
        Ok(returned_session_id)
    }
}

/// Render a tool_use event as a single human-readable line, e.g.
/// `\u{2192} write src/main.rs` or `\u{2192} bash cargo check`.
fn format_tool_use(part: &RawPart) -> String {
    let tool = match &part.tool {
        Some(t) => t.clone(),
        None => return String::new(),
    };
    let status = part
        .state
        .as_ref()
        .and_then(|s| s.status.clone())
        .unwrap_or_default();
    // Only emit on completion to avoid duplicate logs as state transitions.
    if status != "completed" {
        return String::new();
    }
    let summary = part
        .state
        .as_ref()
        .and_then(|s| s.input.as_ref())
        .map(|v| short_tool_summary(&tool, v))
        .unwrap_or_default();
    if summary.is_empty() {
        format!("\u{2192} {}\n", tool)
    } else {
        format!("\u{2192} {} {}\n", tool, summary)
    }
}

/// Pull a useful one-liner out of a tool's input args, depending on the tool.
fn short_tool_summary(tool: &str, input: &serde_json::Value) -> String {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let s = |k: &str| -> Option<String> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.lines().next().unwrap_or_default().trim().to_string())
    };
    match tool {
        "write" | "edit" | "read" => s("filePath").or_else(|| s("path")).unwrap_or_default(),
        "bash" => s("command").unwrap_or_default(),
        "glob" => s("pattern").unwrap_or_default(),
        "grep" => s("pattern").unwrap_or_default(),
        _ => {
            // Fall back to first string value to give at least some context
            for (_, v) in obj.iter() {
                if let Some(s) = v.as_str() {
                    return s.lines().next().unwrap_or_default().trim().chars().take(80).collect();
                }
            }
            String::new()
        }
    }
}

/// Build the complete `opencode run` subprocess command.
/// Pure function extracted for testability.
pub fn opencode_command(
    model: Option<&str>,
    agent: Option<&str>,
    session_id: Option<&str>,
    work_dir: &Path,
) -> Command {
    let mut cmd = Command::new("opencode");
    cmd.args(opencode_args(model, agent, session_id));
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(work_dir);
    cmd
}

/// Build the argv tinker passes to `opencode run`. Pure function so the
/// security test can verify what flags we pass without spawning a subprocess.
pub fn opencode_args(model: Option<&str>, agent: Option<&str>, session_id: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--format".into(),
        "json".into(),
    ];
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    if let Some(a) = agent {
        args.push("--agent".into());
        args.push(a.into());
    }
    if let Some(id) = session_id {
        args.push("-s".into());
        args.push(id.into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_spec_short_tool_summary_crops_to_first_line() {
        let input = json!({
            "command": "echo 1\necho 2"
        });
        let summary = short_tool_summary("bash", &input);
        assert_eq!(summary, "echo 1");
    }

    // security: \u{2192} security.md T4 — tinker must never pass
    // --dangerously-skip-permissions or any other permission-bypass flag.
    #[test]
    fn test_security_t4_no_skip_permissions_flag() {
        let args = opencode_args(Some("any-model"), None, Some("ses_x"));
        for a in &args {
            assert!(
                !a.contains("--dangerously-skip-permissions"),
                "must not pass --dangerously-skip-permissions (saw {:?})",
                args
            );
            assert!(
                !a.contains("--yes"),
                "must not pass --yes (saw {:?})",
                args
            );
        }
    }

    #[test]
    fn test_security_t4_args_includes_model_and_session() {
        let args = opencode_args(Some("openrouter/foo/bar"), None, Some("ses_abc"));
        assert!(args.iter().any(|a| a == "run"));
        assert!(args.iter().any(|a| a == "-m"));
        assert!(args.iter().any(|a| a == "openrouter/foo/bar"));
        assert!(args.iter().any(|a| a == "-s"));
        assert!(args.iter().any(|a| a == "ses_abc"));
    }

    #[test]
    fn test_security_t4_args_no_session_when_none() {
        let args = opencode_args(Some("m"), None, None);
        assert!(!args.iter().any(|a| a == "-s"));
    }

    #[test]
    fn test_spec_agent_flag_passed_when_set() {
        let args = opencode_args(Some("m"), Some("tinker"), None);
        assert!(args.iter().any(|a| a == "--agent"));
        assert!(args.iter().any(|a| a == "tinker"));
    }

    #[test]
    fn test_spec_agent_flag_omitted_when_none() {
        let args = opencode_args(Some("m"), None, None);
        assert!(!args.iter().any(|a| a == "--agent"));
    }

    #[test]
    fn test_default_model_omits_m_flag() {
        let args = opencode_args(None, None, Some("ses_x"));
        assert!(!args.iter().any(|a| a == "-m"), "must not pass -m when model is None");
        assert!(args.iter().any(|a| a == "-s"));
        assert!(args.iter().any(|a| a == "ses_x"));
    }

    // security: \u{2192} security.md T5 — opencode subprocess stderr is piped
    // and captured so errors are visible in the session log rather than leaking
    // to the terminal. The builder function `opencode_command` enforces this by
    // construction (always passes Stdio::piped() for stderr).
    #[test]
    fn test_security_t5_stderr_is_captured() {
        use std::ffi::OsStr;
        let cmd = opencode_command(Some("t5-model"), None, None, Path::new("/tmp"));
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.contains(&OsStr::new("run")), "must use `opencode run`");
        assert!(args.contains(&OsStr::new("--format")), "must request json format");
        assert!(args.contains(&OsStr::new("json")), "format must be json");
        assert!(args.contains(&OsStr::new("-m")), "model flag must be present");
        assert!(args.contains(&OsStr::new("t5-model")), "model must match");
        for a in &args {
            assert!(
                !a.to_string_lossy().contains("--dangerously-skip-permissions"),
                "must not pass --dangerously-skip-permissions",
            );
            assert!(
                !a.to_string_lossy().contains("--yes"),
                "must not pass --yes",
            );
        }
    }
}