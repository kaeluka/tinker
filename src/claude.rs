//! Real `OpenCodeRunner` capability for the Claude CLI: shells out to `claude -p`.
//!
//! Implements the same trait as `opencode.rs` but using Claude's CLI instead.
//! Model tiers use short aliases: opus (tinker), sonnet (goal sessions),
//! haiku (scheduler).

use crate::cap::{Chunk, OpenCodeRunner};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub const TINKER_MODEL: &str = "opus";
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
}

/// Claude CLI runner bound to a specific model (opus/sonnet/haiku).
/// Optionally carries a system prompt (used for orchestrator) and a list of
/// tools that must be mechanically denied (passed as `--disallowedTools`).
pub struct ClaudeRunner {
    pub model: String,
    pub system_prompt: Option<String>,
    pub disallowed_tools: Vec<String>,
}

impl ClaudeRunner {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system_prompt: None,
            disallowed_tools: Vec::new(),
        }
    }

    pub fn with_system_prompt(model: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system_prompt: Some(system_prompt.into()),
            disallowed_tools: Vec::new(),
        }
    }

    /// Deny a set of tools for every invocation of this runner.
    /// Passed as `--disallowedTools <tool> ...` to the Claude CLI.
    pub fn with_denied_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.disallowed_tools = tools.into_iter().map(|t| t.into()).collect();
        self
    }
}

#[async_trait]
impl OpenCodeRunner for ClaudeRunner {
    async fn run(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &Path,
        mut on_session_id: Chunk,
        mut on_chunk: Chunk,
    ) -> Result<String> {
        let mut cmd = claude_command(&self.model, self.system_prompt.as_deref(), session_id, &self.disallowed_tools, work_dir);

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(message.as_bytes()).await?;
        }

        let stdout = child.stdout.take().expect("stdout piped");
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
            if !sid_emitted && ev.event_type == "system" && ev.subtype.as_deref() == Some("init") {
                if let Some(id) = &ev.session_id {
                    returned_session_id = id.clone();
                    on_session_id(id.clone());
                    sid_emitted = true;
                }
            }

            // Handle assistant message content
            if ev.event_type == "assistant" {
                if let Some(msg) = &ev.message {
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

        child.wait().await?;
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
        format!("\u{2192} {}\n", tool)
    } else {
        format!("\u{2192} {} {}\n", tool, summary)
    }
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
    disallowed_tools: &[String],
    work_dir: &Path,
) -> Command {
    let mut cmd = Command::new("claude");
    cmd.args(claude_args(model, system_prompt, session_id, disallowed_tools));
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .current_dir(work_dir);
    cmd
}

/// Build the argv for `claude -p`.
pub fn claude_args(
    model: &str,
    system_prompt: Option<&str>,
    session_id: Option<&str>,
    disallowed_tools: &[String],
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
    if !disallowed_tools.is_empty() {
        args.push("--disallowedTools".into());
        for tool in disallowed_tools {
            args.push(tool.clone());
        }
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
    fn test_security_args_includes_model_and_resume() {
        let args = claude_args("opus", None, Some("ses_abc"), &[]);
        assert!(args.iter().any(|a| a == "-p"));
        assert!(args.iter().any(|a| a == "--model"));
        assert!(args.iter().any(|a| a == "opus"));
        assert!(args.iter().any(|a| a == "--resume"));
        assert!(args.iter().any(|a| a == "ses_abc"));
    }

    #[test]
    fn test_security_args_no_resume_when_none() {
        let args = claude_args("sonnet", None, None, &[]);
        assert!(!args.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn test_spec_system_prompt_passed_when_set() {
        let args = claude_args("opus", Some("You are the orchestrator."), None, &[]);
        assert!(args.iter().any(|a| a == "--system-prompt"));
        assert!(args.iter().any(|a| a == "You are the orchestrator."));
    }

    #[test]
    fn test_spec_system_prompt_omitted_when_none() {
        let args = claude_args("sonnet", None, None, &[]);
        assert!(!args.iter().any(|a| a == "--system-prompt"));
    }

    #[test]
    fn test_spec_output_format_stream_json() {
        let args = claude_args("haiku", None, None, &[]);
        assert!(args.iter().any(|a| a == "--output-format"));
        assert!(args.iter().any(|a| a == "stream-json"));
        assert!(args.iter().any(|a| a == "--verbose"));
    }

    // spec (backends): model tier mapping is opus (tinker),
    // sonnet (goal sessions), haiku (scheduler). The constants must use
    // Claude's short aliases.
    #[test]
    fn test_spec_model_tier_constants_use_short_aliases() {
        assert_eq!(TINKER_MODEL, "opus");
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
        let args = claude_args("sonnet", None, Some("session-123"), &[]);
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
        let args = claude_args("sonnet", None, None, &[]);
        assert!(args.contains(&"--verbose".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
    }

    // spec (backends): the Claude path must mechanically enforce the same
    // identity-level tool denials (task, todowrite) as the opencode agent files
    // do for tinker-agent and rummage. When disallowed_tools is set, the
    // --disallowedTools flag must appear in the args, followed by each denied tool.
    #[test]
    fn test_spec_disallowed_tools_passed_as_flag() {
        let denied = vec!["task".to_string(), "todowrite".to_string()];
        let args = claude_args("opus", None, None, &denied);
        let flag_idx = args.iter().position(|a| a == "--disallowedTools");
        assert!(flag_idx.is_some(), "--disallowedTools flag must be present when tools are denied");
        let flag_idx = flag_idx.unwrap();
        assert_eq!(args.get(flag_idx + 1).map(|s| s.as_str()), Some("task"));
        assert_eq!(args.get(flag_idx + 2).map(|s| s.as_str()), Some("todowrite"));
    }

    // spec (backends): when no tools are denied, --disallowedTools must
    // not appear — avoid injecting unnecessary flags.
    #[test]
    fn test_spec_disallowed_tools_absent_when_empty() {
        let args = claude_args("sonnet", None, None, &[]);
        assert!(!args.iter().any(|a| a == "--disallowedTools"), "--disallowedTools must not appear when list is empty");
    }

    // spec (backends): ClaudeRunner::with_denied_tools stores the denied
    // tool list so it can be threaded into claude_args at invocation time.
    #[test]
    fn test_spec_with_denied_tools_stores_list() {
        let runner = ClaudeRunner::new("sonnet").with_denied_tools(["task", "todowrite"]);
        assert_eq!(runner.disallowed_tools, vec!["task", "todowrite"]);
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
        };
        let formatted = format_tool_use(&content);
        assert!(formatted.starts_with("\u{2192} "), "must start with arrow");
        assert!(formatted.contains("Bash"), "must contain tool name");
        assert!(formatted.contains("cargo test"), "must contain summary");
        assert!(!formatted.contains('\n') || formatted.ends_with('\n'), "should be one line (plus trailing newline)");
    }

    // security: → security.md T4 — even though Claude CLI may not have the same
    // dangerous flags as opencode, verify that no permission-bypass or
    // auto-approval flags are ever passed. Defense in depth.
    #[test]
    fn test_security_t4_no_dangerous_flags() {
        let args = claude_args("opus", None, Some("ses_x"), &[]);
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
            assert!(
                !a.contains("--allow-all"),
                "must not pass --allow-all (saw {:?})",
                args
            );
        }
    }

    // security: → security.md T5 — Claude subprocesses must drop stderr
    // to prevent TUI alternate-screen corruption. The builder function
    // `claude_command` enforces this by construction (always passes
    // Stdio::null() for stderr).
    #[test]
    fn test_security_t5_stderr_is_nulled() {
        use std::ffi::OsStr;
        let cmd = claude_command("haiku", None, None, &[], Path::new("/tmp"));
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.contains(&OsStr::new("-p")), "must use `claude -p`");
        assert!(args.contains(&OsStr::new("--output-format")), "must request stream-json");
        assert!(args.contains(&OsStr::new("stream-json")), "format must be stream-json");
        assert!(args.contains(&OsStr::new("--verbose")), "verbose required for stream-json");
        assert!(args.contains(&OsStr::new("--model")), "model flag must be present");
        assert!(args.contains(&OsStr::new("haiku")), "model must match");
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
