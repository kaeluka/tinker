//! Real `OpenCodeRunner` capability for the Claude CLI: shells out to `claude -p`.
//!
//! Implements the same trait as `opencode.rs` but using Claude's CLI instead.
//! Model tiers: claude-opus-4-8 pinned for tend (high tier), short aliases
//! for sonnet (goal sessions) and haiku (scheduler).

use crate::cap::{Chunk, OpenCodeRunner};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub const TINKER_MODEL: &str = "claude-opus-4-8";
pub const GOAL_MODEL: &str = "sonnet";
pub const SCHEDULER_MODEL: &str = "haiku";

#[derive(Debug, Deserialize)]
struct ClaudeEvent {
    #[serde(rename = "type")]
    event_type: String,
    subtype: Option<String>,
    session_id: Option<String>,
    message: Option<ClaudeMessage>,
    #[allow(dead_code)]
    result: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeMessage {
    content: Vec<ClaudeContent>,
}

#[derive(Debug, Deserialize)]
struct ClaudeContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
    content: Option<serde_json::Value>,
    is_error: Option<bool>,
}

/// Claude CLI runner bound to a specific model (opus/sonnet/haiku).
/// Optionally carries a system prompt (used for orchestrator).
pub struct ClaudeRunner {
    pub model: String,
    pub system_prompt: Option<String>,
}

impl ClaudeRunner {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system_prompt: None,
        }
    }

    pub fn with_system_prompt(model: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system_prompt: Some(system_prompt.into()),
        }
    }
}

#[async_trait]
impl OpenCodeRunner for ClaudeRunner {
    async fn run(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &Path,
        system_prompt: Option<&str>,
        mut on_session_id: Chunk,
        mut on_chunk: Chunk,
    ) -> Result<String> {
        // Per-call system_prompt (goal-specific) takes priority over the
        // struct-level one (used for tend, which has a fixed scope boundary).
        let effective_sp = system_prompt.or(self.system_prompt.as_deref());
        let mut cmd = claude_command(&self.model, effective_sp, session_id, work_dir);

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(message.as_bytes()).await?;
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
            let Ok(ev) = serde_json::from_str::<ClaudeEvent>(&line) else {
                continue;
            };

            // Emit session_id from the init event
            if !sid_emitted && ev.event_type == "system" && ev.subtype.as_deref() == Some("init")
                && let Some(id) = &ev.session_id {
                    returned_session_id = id.clone();
                    on_session_id(id.clone());
                    sid_emitted = true;
                }

            // Handle assistant message content
            if ev.event_type == "assistant"
                && let Some(msg) = &ev.message {
                    for content in &msg.content {
                        match content.content_type.as_str() {
                            "text" => {
                                if let Some(text) = &content.text {
                                    on_chunk(text.clone());
                                }
                            }
                            "tool_use" => {
                                let chunk = format_tool_use(content);
                                if !chunk.is_empty() {
                                    on_chunk(chunk);
                                }
                            }
                            _ => {}
                        }
                    }
                }

            // Surface tool_result errors (permission denials) from user messages.
            if ev.event_type == "user"
                && let Some(msg) = &ev.message {
                    for content in &msg.content {
                        if content.content_type == "tool_result" {
                            let chunk = format_tool_result_error(content);
                            if !chunk.is_empty() {
                                on_chunk(chunk);
                            }
                        }
                    }
                }
        }

        let status = child.wait().await?;
        let stderr_text = stderr_collector.await.unwrap_or_default();
        if !stderr_text.is_empty() {
            on_chunk(crate::prompts::stderr_prefix(stderr_text.trim()));
        }

        // On non-zero exit with stderr output, re-inject the error into the
        // active session so the agent can reason about recovery — e.g. routing
        // via a peer @-message rather than retrying a permission-denied path.
        if !status.success() && !stderr_text.is_empty() {
            let effective_sid = if !returned_session_id.is_empty() {
                Some(returned_session_id.as_str())
            } else {
                session_id
            };
            if let Some(sid) = effective_sid {
                let error_msg = crate::prompts::process_error(
                    status.code().unwrap_or(-1),
                    stderr_text.trim(),
                );
                // Error-reinjection resumes the session — use the struct-level
                // system prompt (for tend) or None (for goal agents).
                let mut follow_cmd = claude_command(&self.model, self.system_prompt.as_deref(), Some(sid), work_dir);
                if let Ok(mut follow_child) = follow_cmd.spawn() {
                    if let Some(mut stdin) = follow_child.stdin.take() {
                        let _ = stdin.write_all(error_msg.as_bytes()).await;
                    }
                    if let Some(follow_stderr) = follow_child.stderr.take() {
                        tokio::spawn(async move {
                            let mut lines = BufReader::new(follow_stderr).lines();
                            while let Ok(Some(_)) = lines.next_line().await {}
                        });
                    }
                    if let Some(follow_stdout) = follow_child.stdout.take() {
                        let mut follow_lines = BufReader::new(follow_stdout).lines();
                        while let Ok(Some(line)) = follow_lines.next_line().await {
                            if line.is_empty() {
                                continue;
                            }
                            let Ok(ev) = serde_json::from_str::<ClaudeEvent>(&line) else {
                                continue;
                            };
                            if ev.event_type == "assistant"
                                && let Some(msg) = &ev.message {
                                    for content in &msg.content {
                                        match content.content_type.as_str() {
                                            "text" => {
                                                if let Some(text) = &content.text {
                                                    on_chunk(text.clone());
                                                }
                                            }
                                            "tool_use" => {
                                                let chunk = format_tool_use(content);
                                                if !chunk.is_empty() {
                                                    on_chunk(chunk);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                        }
                    }
                    let _ = follow_child.wait().await;
                }
            }
        }

        Ok(returned_session_id)
    }
}

/// Render a tool_use content block as a single human-readable line.
fn format_tool_use(content: &ClaudeContent) -> String {
    let tool = match &content.name {
        Some(n) => n.clone(),
        None => return String::new(),
    };
    let summary = content
        .input
        .as_ref()
        .map(|v| short_tool_summary(&tool, v))
        .unwrap_or_default();
    if summary.is_empty() {
        crate::prompts::tool_completed_no_summary(&tool)
    } else {
        crate::prompts::tool_completed_with_summary(&tool, &summary)
    }
}

/// Render a tool_result error block as a visible ⚠ line.
/// Only fires when `is_error` is true; returns empty string otherwise.
/// Handles both string and array content formats.
fn format_tool_result_error(content: &ClaudeContent) -> String {
    if content.is_error != Some(true) {
        return String::new();
    }
    let text = match &content.content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return String::new(),
    };
    if text.is_empty() {
        return String::new();
    }
    let first_line = text.lines().next().unwrap_or(&text);
    crate::prompts::tool_result_error(first_line)
}

/// Pull a useful one-liner out of a tool's input args.
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
    // Claude uses capitalized tool names (Bash, Write, Edit, etc.)
    match tool {
        "Write" | "Edit" | "Read" | "write" | "edit" | "read" => {
            s("filePath").or_else(|| s("path")).unwrap_or_default()
        }
        "Bash" | "bash" => s("command").unwrap_or_default(),
        "Glob" | "glob" => s("pattern").unwrap_or_default(),
        "Grep" | "grep" => s("pattern").unwrap_or_default(),
        _ => {
            for (_, v) in obj.iter() {
                if let Some(s) = v.as_str() {
                    return s.lines().next().unwrap_or_default().trim().chars().take(80).collect();
                }
            }
            String::new()
        }
    }
}

/// Build the complete `claude -p` subprocess command.
pub fn claude_command(
    model: &str,
    system_prompt: Option<&str>,
    session_id: Option<&str>,
    work_dir: &Path,
) -> Command {
    let mut cmd = Command::new("claude");
    cmd.args(claude_args(model, system_prompt, session_id));
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(work_dir);
    cmd
}

/// Build the argv for `claude -p`.
pub fn claude_args(
    model: &str,
    system_prompt: Option<&str>,
    session_id: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--model".into(),
        model.into(),
    ];
    if let Some(sp) = system_prompt {
        args.push("--system-prompt".into());
        args.push(sp.into());
    }
    if let Some(id) = session_id {
        args.push("--resume".into());
        args.push(id.into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spec_short_tool_summary_crops_to_first_line() {
        let input = serde_json::json!({
            "command": "echo 1\necho 2"
        });
        let summary = short_tool_summary("Bash", &input);
        assert_eq!(summary, "echo 1");
    }

    #[test]
    fn test_spec_args_includes_model_and_resume() {
        let args = claude_args("opus", None, Some("ses_abc"));
        assert!(args.iter().any(|a| a == "-p"));
        assert!(args.iter().any(|a| a == "--model"));
        assert!(args.iter().any(|a| a == "opus"));
        assert!(args.iter().any(|a| a == "--resume"));
        assert!(args.iter().any(|a| a == "ses_abc"));
    }

    #[test]
    fn test_spec_args_no_resume_when_none() {
        let args = claude_args("sonnet", None, None);
        assert!(!args.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn test_spec_system_prompt_passed_when_set() {
        let args = claude_args("opus", Some("You are the orchestrator."), None);
        assert!(args.iter().any(|a| a == "--system-prompt"));
        assert!(args.iter().any(|a| a == "You are the orchestrator."));
    }

    #[test]
    fn test_spec_system_prompt_omitted_when_none() {
        let args = claude_args("sonnet", None, None);
        assert!(!args.iter().any(|a| a == "--system-prompt"));
    }

    #[test]
    fn test_spec_output_format_stream_json() {
        let args = claude_args("haiku", None, None);
        assert!(args.iter().any(|a| a == "--output-format"));
        assert!(args.iter().any(|a| a == "stream-json"));
        assert!(args.iter().any(|a| a == "--verbose"));
    }

    // spec (backends): high tier (tend) uses the pinned full model ID to
    // avoid silent alias resolution to an unvalidated version. Mid and low
    // tiers (goal sessions, scheduler) remain on short aliases.
    #[test]
    fn test_spec_model_tier_constants() {
        assert_eq!(TINKER_MODEL, "claude-opus-4-8");
        assert_eq!(GOAL_MODEL, "sonnet");
        assert_eq!(SCHEDULER_MODEL, "haiku");
    }

    // spec (backends): the orchestrator persona is passed via
    // --system-prompt when using Claude, not via agent file installation.
    // ClaudeRunner::with_system_prompt constructs a runner with the prompt.
    #[test]
    fn test_spec_with_system_prompt_stores_prompt() {
        let runner = ClaudeRunner::with_system_prompt("opus", "test prompt");
        assert_eq!(runner.model, "opus");
        assert_eq!(runner.system_prompt, Some("test prompt".to_string()));
    }

    // spec (backends): session resumption uses --resume <session-id>,
    // the Claude equivalent of opencode's -s.
    #[test]
    fn test_spec_resume_flag_for_session_resumption() {
        let args = claude_args("sonnet", None, Some("session-123"));
        let resume_idx = args.iter().position(|a| a == "--resume");
        assert!(resume_idx.is_some(), "--resume flag must be present");
        let resume_idx = resume_idx.unwrap();
        assert_eq!(
            args.get(resume_idx + 1),
            Some(&"session-123".to_string()),
            "--resume must be followed by the session id"
        );
    }

    // spec (backends): Claude output format requires
    // --output-format stream-json --verbose.
    #[test]
    fn test_spec_verbose_required_for_stream_json() {
        let args = claude_args("sonnet", None, None);
        assert!(args.contains(&"--verbose".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
    }

    // spec (backends): tool calls in the TUI are rendered as compact
    // one-liners, similar to opencode style (e.g., `→ bash echo hello`).
    // format_tool_use must produce this format.
    #[test]
    fn test_spec_format_tool_use_compact_one_liner() {
        let content = ClaudeContent {
            content_type: "tool_use".into(),
            text: None,
            name: Some("Bash".into()),
            input: Some(serde_json::json!({"command": "cargo test"})),
            content: None,
            is_error: None,
        };
        let formatted = format_tool_use(&content);
        assert!(formatted.starts_with("\u{2192} "), "must start with arrow");
        assert!(formatted.contains("Bash"), "must contain tool name");
        assert!(formatted.contains("cargo test"), "must contain summary");
        assert!(!formatted.contains('\n') || formatted.ends_with('\n'), "should be one line (plus trailing newline)");
    }

    // spec (backends): tool_result blocks with is_error=true in user messages must
    // emit a ⚠-prefixed chunk so permission denials surface to the conversation pane.
    // Content may be a plain string or an array of text blocks.
    #[test]
    fn test_spec_tool_result_error_string_emits_warning_chunk() {
        let content = ClaudeContent {
            content_type: "tool_result".into(),
            text: None,
            name: None,
            input: None,
            content: Some(serde_json::json!("Permission denied: .tinker/notes/notes.md")),
            is_error: Some(true),
        };
        let chunk = format_tool_result_error(&content);
        assert!(chunk.contains('\u{26A0}'), "tool_result error must emit \u{26A0} chunk");
        assert!(chunk.contains("Permission denied"), "chunk must include error text");
    }

    // spec (backends): tool_result errors can carry array-format content; the
    // text must still be extracted and surfaced.
    #[test]
    fn test_spec_tool_result_error_array_content_emits_warning_chunk() {
        let content = ClaudeContent {
            content_type: "tool_result".into(),
            text: None,
            name: None,
            input: None,
            content: Some(serde_json::json!([{"type": "text", "text": "Permission denied: .tinker/notes"}])),
            is_error: Some(true),
        };
        let chunk = format_tool_result_error(&content);
        assert!(chunk.contains('\u{26A0}'), "array-content tool_result error must emit \u{26A0} chunk");
        assert!(chunk.contains("Permission denied"), "chunk must include error text");
    }

    // spec (backends): tool_result blocks without is_error must not emit a chunk —
    // only error results (permission denials) are surfaced, not normal results.
    #[test]
    fn test_spec_tool_result_non_error_emits_nothing() {
        let content = ClaudeContent {
            content_type: "tool_result".into(),
            text: None,
            name: None,
            input: None,
            content: Some(serde_json::json!("file contents here")),
            is_error: None,
        };
        assert!(format_tool_result_error(&content).is_empty(), "non-error tool_result must not emit a chunk");
    }

    // security: → security.md T5 — Claude subprocess stderr is piped and
    // captured, not leaked to the terminal (which would corrupt the TUI
    // alternate screen). The captured output is re-injected into the session
    // on non-zero exit so the agent can reason about recovery. Not nulled
    // because error feedback requires the content.
    #[test]
    fn test_security_t5_stderr_is_captured_not_leaked() {
        use std::ffi::OsStr;
        let cmd = claude_command("haiku", None, None, Path::new("/tmp"));
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.contains(&OsStr::new("-p")), "must use `claude -p`");
        assert!(args.contains(&OsStr::new("--output-format")), "must request stream-json");
        assert!(args.contains(&OsStr::new("stream-json")), "format must be stream-json");
        assert!(args.contains(&OsStr::new("--verbose")), "verbose required for stream-json");
        assert!(args.contains(&OsStr::new("--model")), "model flag must be present");
        assert!(args.contains(&OsStr::new("haiku")), "model must match");
    }

    // spec (backends): the per-call system_prompt (goal-specific, supplied on
    // the first turn by goal_agent_loop) takes priority over the runner's
    // struct-level system_prompt (used for tend's fixed scope boundary).
    // When both are set, the per-call one wins. When only the struct-level is
    // set, the struct-level is used.
    #[test]
    fn test_spec_per_call_system_prompt_overrides_struct_level() {
        let runner = ClaudeRunner::with_system_prompt("sonnet", "struct-level prompt");

        // Per-call system_prompt provided — it must override struct-level.
        let effective = Some("per-call prompt").or(runner.system_prompt.as_deref());
        let args = claude_args(&runner.model, effective, None);
        assert!(
            args.contains(&"--system-prompt".to_string()),
            "--system-prompt flag must be present"
        );
        assert!(
            args.contains(&"per-call prompt".to_string()),
            "per-call system_prompt must appear in args"
        );
        assert!(
            !args.contains(&"struct-level prompt".to_string()),
            "struct-level system_prompt must NOT appear when per-call is set"
        );

        // No per-call system_prompt — struct-level must be used.
        let no_per_call: Option<&str> = None;
        let effective2 = no_per_call.or(runner.system_prompt.as_deref());
        let args2 = claude_args(&runner.model, effective2, None);
        assert!(
            args2.contains(&"struct-level prompt".to_string()),
            "struct-level system_prompt must be used when no per-call prompt is set"
        );
    }

    // spec (backends): error output from a subprocess that exits non-zero is
    // re-injected into the active session so the agent can reason about
    // recovery. Fires when: exit code != 0, stderr non-empty, session id
    // available. Each condition is independently necessary.
    #[test]
    fn test_spec_error_reinjection_requires_nonempty_stderr_and_session() {
        let empty_stderr = "";
        let nonempty_stderr = "permission denied: .tinker/goals/foo.toml";
        let with_sid: Option<&str> = Some("ses_abc");
        let without_sid: Option<&str> = None;

        let fires = |stderr: &str, sid: Option<&str>| !stderr.is_empty() && sid.is_some();

        assert!(!fires(empty_stderr, with_sid), "empty stderr must suppress re-injection");
        assert!(!fires(nonempty_stderr, without_sid), "missing session id must suppress re-injection");
        assert!(fires(nonempty_stderr, with_sid), "non-empty stderr + session id must trigger re-injection");
    }
}
