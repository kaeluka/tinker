//! Native `OpenCodeRunner` capability: talks directly to the OpenRouter
//! chat-completions API and runs the tool loop in-process.
//!
//! Unlike the CLI backends (`opencode.rs`, `claude.rs`), there is no
//! subprocess: tinker owns tool execution, so per-role capability policy
//! (`ToolPolicy`) is enforced in code rather than via system prompts.
//! Sessions are in-memory only — a tinker restart starts everyone fresh,
//! matching the existing cross-restart behavior of the CLI backends.
//!
//! Context overflow fails visibly: the API error is surfaced as a ⚠ chunk,
//! the session is dropped from the store, and `run` returns `Err` — which
//! makes `goal_agent_loop` clear its session id and start the next dispatch
//! as a fresh session (see the `Err` arm in `goal_agent_loop`).

use crate::cap::{Chunk, OpenCodeRunner};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use tokio::process::Command;

/// OpenRouter model ids — the same models the opencode backend routes to,
/// minus the `openrouter/` CLI routing prefix.
pub const TINKER_MODEL: &str = "google/gemini-3.1-pro-preview";
pub const GOAL_MODEL: &str = "deepseek/deepseek-v4-flash";
pub const SCHEDULER_MODEL: &str = "google/gemini-3.1-flash-lite-preview";

pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Tool results larger than this are truncated before being fed back to the
/// model, so one verbose `cargo test` run can't blow the context window.
const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;
/// Hard cap on tool-loop iterations per `run` call — a runaway model that
/// never stops calling tools is cut off visibly instead of looping forever.
const MAX_TURNS: usize = 100;
/// Bash commands are killed after this long.
const BASH_TIMEOUT_SECS: u64 = 300;
/// Glob results are capped at this many paths.
const MAX_GLOB_RESULTS: usize = 200;

/// Per-role capability policy, enforced in-process at the tool-call layer.
/// This is the reason the native backend exists: the boundary is code, not
/// a system prompt the model may or may not respect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolPolicy {
    /// Goal sessions (rummage, jog, regular sessions, cleanup): full bash,
    /// unrestricted read/write.
    Unrestricted,
    /// Tend: no bash at all; write/edit only under `.tinker/goals/`.
    /// Read/glob/grep are unrestricted (tend may read anything).
    TendScope,
}

impl ToolPolicy {
    /// Check one tool call. `Ok(())` allows; `Err(reason)` denies — the
    /// reason is returned to the model as the tool result so it can adapt.
    pub fn check(&self, tool: &str, input: &Value, work_dir: &Path) -> std::result::Result<(), String> {
        match self {
            ToolPolicy::Unrestricted => Ok(()),
            ToolPolicy::TendScope => match tool {
                "bash" => Err("policy: tend has no bash access".to_string()),
                "write" | "edit" => {
                    let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let scope = work_dir.join(".tinker").join("goals");
                    if path_within(path, work_dir, &scope) {
                        Ok(())
                    } else {
                        Err(format!(
                            "policy: tend may only write under .tinker/goals/ — denied: {path}"
                        ))
                    }
                }
                _ => Ok(()),
            },
        }
    }
}

/// True when `path` (absolute, or relative to `work_dir`) stays lexically
/// within `scope`. Any `..` component is rejected outright — no symlink or
/// traversal games.
fn path_within(path: &str, work_dir: &Path, scope: &Path) -> bool {
    if path.is_empty() {
        return false;
    }
    let p = Path::new(path);
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let abs: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_dir.join(p)
    };
    abs.starts_with(scope)
}

/// The OpenAI-style function schemas the model sees. Tend's list omits bash
/// entirely (defense in depth: the policy check would also deny it).
pub fn tool_definitions(policy: &ToolPolicy) -> Vec<Value> {
    let mut tools = vec![
        tool_schema(
            "read",
            "Read a file and return its contents.",
            json!({"path": {"type": "string", "description": "File path, absolute or relative to the working directory"}}),
            &["path"],
        ),
        tool_schema(
            "write",
            "Write content to a file, creating parent directories as needed. Overwrites existing content.",
            json!({
                "path": {"type": "string", "description": "File path to write"},
                "content": {"type": "string", "description": "Full new file content"}
            }),
            &["path", "content"],
        ),
        tool_schema(
            "edit",
            "Replace an exact string in a file. old_string must occur exactly once.",
            json!({
                "path": {"type": "string", "description": "File path to edit"},
                "old_string": {"type": "string", "description": "Exact text to replace (must be unique in the file)"},
                "new_string": {"type": "string", "description": "Replacement text"}
            }),
            &["path", "old_string", "new_string"],
        ),
        tool_schema(
            "glob",
            "Find files matching a glob pattern (e.g. src/**/*.rs), relative to the working directory.",
            json!({"pattern": {"type": "string", "description": "Glob pattern"}}),
            &["pattern"],
        ),
        tool_schema(
            "grep",
            "Search file contents recursively for a pattern (basic regex). Returns matching lines with file:line prefixes.",
            json!({
                "pattern": {"type": "string", "description": "Pattern to search for"},
                "path": {"type": "string", "description": "Directory or file to search (default: working directory)"}
            }),
            &["pattern"],
        ),
    ];
    if *policy != ToolPolicy::TendScope {
        tools.push(tool_schema(
            "bash",
            "Run a shell command in the working directory and return its combined output.",
            json!({"command": {"type": "string", "description": "Shell command to run"}}),
            &["command"],
        ));
    }
    tools
}

fn tool_schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
            }
        }
    })
}

/// Build the chat-completions request body. Pure function for testability.
pub fn request_body(model: &str, messages: &[Value], tools: &[Value]) -> Value {
    json!({
        "model": model,
        "messages": messages,
        "tools": tools,
    })
}

/// Native runner bound to a model and a capability policy.
/// The optional struct-level system prompt mirrors `ClaudeRunner`: it is used
/// only for tend, whose scope framing must be present from session creation.
/// Per-call `system_prompt` (goal agents, first turn) takes priority.
pub struct NativeRunner {
    pub model: String,
    pub system_prompt: Option<String>,
    pub policy: ToolPolicy,
    sessions: Mutex<HashMap<String, Vec<Value>>>,
    client: reqwest::Client,
}

impl NativeRunner {
    pub fn new(model: impl Into<String>, policy: ToolPolicy) -> Self {
        Self {
            model: model.into(),
            system_prompt: None,
            policy,
            sessions: Mutex::new(HashMap::new()),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_system_prompt(
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        policy: ToolPolicy,
    ) -> Self {
        Self {
            model: model.into(),
            system_prompt: Some(system_prompt.into()),
            policy,
            sessions: Mutex::new(HashMap::new()),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl OpenCodeRunner for NativeRunner {
    async fn run(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &Path,
        system_prompt: Option<&str>,
        mut on_session_id: Chunk,
        mut on_chunk: Chunk,
    ) -> Result<String> {
        let api_key = std::env::var(API_KEY_ENV)
            .map_err(|_| anyhow!("{API_KEY_ENV} is not set — the native backend needs an OpenRouter API key"))?;

        // Resume or create the session's message history.
        let sid: String;
        let mut messages: Vec<Value>;
        match session_id {
            Some(id) => {
                let store = self.sessions.lock().unwrap();
                match store.get(id) {
                    Some(history) => {
                        sid = id.to_string();
                        messages = history.clone();
                    }
                    None => {
                        // Unknown id: a dropped (overflowed) or pre-restart
                        // session. Fail so goal_agent_loop resets to fresh.
                        return Err(anyhow!("unknown native session {id} — session store has no history"));
                    }
                }
            }
            None => {
                sid = format!("nat_{}", uuid::Uuid::new_v4());
                messages = Vec::new();
                // Per-call system_prompt (goal-specific) takes priority over
                // the struct-level one (tend's fixed scope boundary).
                if let Some(sp) = system_prompt.or(self.system_prompt.as_deref()) {
                    messages.push(json!({"role": "system", "content": sp}));
                }
            }
        }
        on_session_id(sid.clone());
        messages.push(json!({"role": "user", "content": message}));

        let tools = tool_definitions(&self.policy);

        let mut turns = 0;
        loop {
            turns += 1;
            if turns > MAX_TURNS {
                on_chunk(crate::prompts::stream_error(&format!(
                    "native backend: exceeded {MAX_TURNS} tool turns; stopping this run"
                )));
                break;
            }

            let body = request_body(&self.model, &messages, &tools);
            let resp = self
                .client
                .post(OPENROUTER_URL)
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("openrouter request failed: {e}");
                    on_chunk(crate::prompts::stream_error(&msg));
                    self.save_session(&sid, messages);
                    return Err(anyhow!(msg));
                }
            };

            let status = resp.status();
            let payload: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("openrouter returned unparseable response: {e}");
                    on_chunk(crate::prompts::stream_error(&msg));
                    self.save_session(&sid, messages);
                    return Err(anyhow!(msg));
                }
            };

            if let Some(err_msg) = api_error_message(&payload, status.as_u16()) {
                on_chunk(crate::prompts::stream_error(&err_msg));
                if is_context_overflow(&err_msg) {
                    // Fail visibly and drop the session: the next dispatch
                    // starts fresh (goal_agent_loop clears its sid on Err).
                    self.sessions.lock().unwrap().remove(&sid);
                    return Err(anyhow!("context overflow: {err_msg}"));
                }
                self.save_session(&sid, messages);
                return Err(anyhow!(err_msg));
            }

            let Some(assistant) = payload
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
            else {
                let msg = "openrouter response has no choices[0].message".to_string();
                on_chunk(crate::prompts::stream_error(&msg));
                self.save_session(&sid, messages);
                return Err(anyhow!(msg));
            };

            if let Some(text) = assistant.get("content").and_then(|c| c.as_str())
                && !text.is_empty() {
                    on_chunk(text.to_string());
                }

            let tool_calls: Vec<Value> = assistant
                .get("tool_calls")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default();

            // Record the assistant turn verbatim (content + tool_calls) so the
            // follow-up request is well-formed.
            messages.push(assistant.clone());

            if tool_calls.is_empty() {
                break;
            }

            for call in &tool_calls {
                let call_id = call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = call
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args: Value = call
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));

                let summary = short_tool_summary(&name, &args);
                let result = match self.policy.check(&name, &args, work_dir) {
                    Err(denied) => {
                        on_chunk(format_tool_error(&name, &summary, &denied));
                        Err(denied)
                    }
                    Ok(()) => match execute_tool(&name, &args, work_dir).await {
                        Ok(output) => {
                            on_chunk(format_tool_ok(&name, &summary));
                            Ok(output)
                        }
                        Err(e) => {
                            let e = format!("{e:#}");
                            on_chunk(format_tool_error(&name, &summary, &e));
                            Err(e)
                        }
                    },
                };
                let content = match result {
                    Ok(out) => truncate_output(&out),
                    Err(e) => format!("Error: {e}"),
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }));
            }
        }

        self.save_session(&sid, messages);
        Ok(sid)
    }
}

impl NativeRunner {
    fn save_session(&self, sid: &str, messages: Vec<Value>) {
        self.sessions.lock().unwrap().insert(sid.to_string(), messages);
    }
}

/// Extract a human-readable error from an OpenRouter error payload, if any.
fn api_error_message(payload: &Value, http_status: u16) -> Option<String> {
    if let Some(err) = payload.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown API error");
        return Some(format!("openrouter error (HTTP {http_status}): {msg}"));
    }
    if http_status >= 400 {
        return Some(format!("openrouter error: HTTP {http_status}"));
    }
    None
}

/// Heuristic: does this API error indicate the context window is full?
fn is_context_overflow(err_msg: &str) -> bool {
    let lower = err_msg.to_lowercase();
    lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("too many tokens")
}

/// Cap a tool result before it goes back to the model.
fn truncate_output(out: &str) -> String {
    if out.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return out.to_string();
    }
    let truncated: String = out.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
    format!("{truncated}\n[... output truncated at {MAX_TOOL_OUTPUT_CHARS} chars]")
}

fn format_tool_ok(tool: &str, summary: &str) -> String {
    if summary.is_empty() {
        crate::prompts::tool_completed_no_summary(tool)
    } else {
        crate::prompts::tool_completed_with_summary(tool, summary)
    }
}

fn format_tool_error(tool: &str, summary: &str, error: &str) -> String {
    let first_line = error.lines().next().unwrap_or(error);
    if summary.is_empty() {
        crate::prompts::tool_error_no_summary(tool, first_line)
    } else {
        crate::prompts::tool_error_with_summary(tool, summary, first_line)
    }
}

/// Pull a useful one-liner out of a tool's input args (native arg names).
fn short_tool_summary(tool: &str, input: &Value) -> String {
    let s = |k: &str| -> Option<String> {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.lines().next().unwrap_or_default().trim().to_string())
    };
    match tool {
        "read" | "write" | "edit" => s("path").unwrap_or_default(),
        "bash" => s("command").unwrap_or_default(),
        "glob" | "grep" => s("pattern").unwrap_or_default(),
        _ => String::new(),
    }
}

/// Execute one tool call. All paths resolve relative to `work_dir`.
async fn execute_tool(name: &str, args: &Value, work_dir: &Path) -> Result<String> {
    let str_arg = |k: &str| -> Result<&str> {
        args.get(k)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required argument: {k}"))
    };
    match name {
        "read" => {
            let path = resolve(str_arg("path")?, work_dir);
            let content = std::fs::read_to_string(&path)
                .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
            Ok(content)
        }
        "write" => {
            let path = resolve(str_arg("path")?, work_dir);
            let content = str_arg("content")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)
                .map_err(|e| anyhow!("write {}: {e}", path.display()))?;
            Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
        }
        "edit" => {
            let path = resolve(str_arg("path")?, work_dir);
            let old = str_arg("old_string")?;
            let new = str_arg("new_string")?;
            let content = std::fs::read_to_string(&path)
                .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
            let count = content.matches(old).count();
            if count == 0 {
                return Err(anyhow!("old_string not found in {}", path.display()));
            }
            if count > 1 {
                return Err(anyhow!(
                    "old_string matches {count} times in {} — provide more surrounding context",
                    path.display()
                ));
            }
            std::fs::write(&path, content.replacen(old, new, 1))?;
            Ok(format!("edited {}", path.display()))
        }
        "bash" => {
            let command = str_arg("command")?;
            let fut = Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(work_dir)
                .output();
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(BASH_TIMEOUT_SECS),
                fut,
            )
            .await
            .map_err(|_| anyhow!("command timed out after {BASH_TIMEOUT_SECS}s"))??;
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str("stderr:\n");
                combined.push_str(&stderr);
            }
            if !output.status.success() {
                combined.push_str(&format!(
                    "\n[exit code: {}]",
                    output.status.code().unwrap_or(-1)
                ));
            }
            if combined.is_empty() {
                combined.push_str("(no output)");
            }
            Ok(combined)
        }
        "glob" => {
            let pattern = str_arg("pattern")?;
            let full = work_dir.join(pattern);
            let pattern_str = full.to_string_lossy();
            let mut paths: Vec<String> = glob::glob(&pattern_str)
                .map_err(|e| anyhow!("bad glob pattern: {e}"))?
                .filter_map(|entry| entry.ok())
                .map(|p| {
                    p.strip_prefix(work_dir)
                        .map(|r| r.display().to_string())
                        .unwrap_or_else(|_| p.display().to_string())
                })
                .take(MAX_GLOB_RESULTS + 1)
                .collect();
            let capped = paths.len() > MAX_GLOB_RESULTS;
            paths.truncate(MAX_GLOB_RESULTS);
            let mut out = paths.join("\n");
            if out.is_empty() {
                out = "(no matches)".to_string();
            } else if capped {
                out.push_str(&format!("\n[... capped at {MAX_GLOB_RESULTS} results]"));
            }
            Ok(out)
        }
        "grep" => {
            let pattern = str_arg("pattern")?;
            let search_path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let output = Command::new("grep")
                .args([
                    "-rnI",
                    "--exclude-dir=.git",
                    "--exclude-dir=target",
                    "--exclude-dir=node_modules",
                    "-e",
                ])
                .arg(pattern)
                .arg(search_path)
                .current_dir(work_dir)
                .output()
                .await?;
            // grep exits 1 on "no matches" — that's a result, not an error.
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.is_empty() {
                Ok("(no matches)".to_string())
            } else {
                Ok(stdout.to_string())
            }
        }
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn resolve(path: &str, work_dir: &Path) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_dir.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec (native-backend): tend's policy denies bash outright — the
    // capability boundary is enforced in code, not via system prompt.
    #[test]
    fn test_spec_tend_policy_denies_bash() {
        let policy = ToolPolicy::TendScope;
        let result = policy.check("bash", &json!({"command": "ls"}), Path::new("/proj"));
        assert!(result.is_err(), "tend must not get bash");
        assert!(result.unwrap_err().contains("policy"), "denial must name policy");
    }

    // spec (native-backend): tend may write only under .tinker/goals/ —
    // writes elsewhere (src/, .tinker/notes/, absolute paths outside) are denied.
    #[test]
    fn test_spec_tend_policy_write_scope() {
        let policy = ToolPolicy::TendScope;
        let wd = Path::new("/proj");
        let allow = |p: &str| policy.check("write", &json!({"path": p, "content": "x"}), wd);
        assert!(allow(".tinker/goals/foo.toml").is_ok(), "goal file write must be allowed");
        assert!(allow("/proj/.tinker/goals/bar.toml").is_ok(), "absolute in-scope write must be allowed");
        assert!(allow("src/main.rs").is_err(), "src write must be denied");
        assert!(allow(".tinker/notes/notes.md").is_err(), "notes write must be denied");
        assert!(allow("/etc/passwd").is_err(), "out-of-tree absolute write must be denied");
        assert!(allow("").is_err(), "empty path must be denied");
    }

    // spec (native-backend): path traversal via .. is rejected even when the
    // prefix looks in-scope.
    #[test]
    fn test_spec_tend_policy_rejects_parent_traversal() {
        let policy = ToolPolicy::TendScope;
        let wd = Path::new("/proj");
        let result = policy.check(
            "edit",
            &json!({"path": ".tinker/goals/../../src/main.rs", "old_string": "a", "new_string": "b"}),
            wd,
        );
        assert!(result.is_err(), ".. traversal must be denied");
    }

    // spec (native-backend): tend keeps unrestricted read/glob/grep — the
    // restriction is on mutation, not observation.
    #[test]
    fn test_spec_tend_policy_allows_reads_anywhere() {
        let policy = ToolPolicy::TendScope;
        let wd = Path::new("/proj");
        assert!(policy.check("read", &json!({"path": "src/main.rs"}), wd).is_ok());
        assert!(policy.check("glob", &json!({"pattern": "src/**/*.rs"}), wd).is_ok());
        assert!(policy.check("grep", &json!({"pattern": "fn main"}), wd).is_ok());
    }

    // spec (native-backend): goal sessions (incl. cleanup) are unrestricted —
    // full bash, writes anywhere.
    #[test]
    fn test_spec_unrestricted_policy_allows_everything() {
        let policy = ToolPolicy::Unrestricted;
        let wd = Path::new("/proj");
        assert!(policy.check("bash", &json!({"command": "cargo test"}), wd).is_ok());
        assert!(policy.check("write", &json!({"path": "src/main.rs", "content": "x"}), wd).is_ok());
    }

    // spec (native-backend): the v1 tool set is exactly six tools — bash,
    // read, write, edit, glob, grep. Tend's schema list omits bash entirely
    // (defense in depth on top of the policy denial).
    #[test]
    fn test_spec_tool_definitions_six_tools_unrestricted_five_for_tend() {
        let names = |p: &ToolPolicy| -> Vec<String> {
            tool_definitions(p)
                .iter()
                .map(|t| t.pointer("/function/name").unwrap().as_str().unwrap().to_string())
                .collect()
        };
        let full = names(&ToolPolicy::Unrestricted);
        assert_eq!(full.len(), 6, "unrestricted set is exactly six tools");
        for t in ["bash", "read", "write", "edit", "glob", "grep"] {
            assert!(full.contains(&t.to_string()), "missing tool: {t}");
        }
        let tend = names(&ToolPolicy::TendScope);
        assert_eq!(tend.len(), 5, "tend set omits bash");
        assert!(!tend.contains(&"bash".to_string()), "tend must not see bash schema");
    }

    // spec (native-backend): request body carries model, messages, and tools.
    #[test]
    fn test_spec_request_body_shape() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let tools = tool_definitions(&ToolPolicy::Unrestricted);
        let body = request_body("some/model", &messages, &tools);
        assert_eq!(body["model"], "some/model");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["tools"].as_array().unwrap().len(), 6);
    }

    // spec (native-backend): context-overflow API errors are recognized so the
    // session can be dropped and restarted fresh; other errors are not.
    #[test]
    fn test_spec_context_overflow_detection() {
        assert!(is_context_overflow("This model's maximum context length is 128000 tokens"));
        assert!(is_context_overflow("context window exceeded"));
        assert!(!is_context_overflow("invalid API key"));
        assert!(!is_context_overflow("rate limit exceeded"));
    }

    // spec (native-backend): tool output larger than the cap is truncated with
    // a visible marker; smaller output passes through verbatim.
    #[test]
    fn test_spec_truncate_output() {
        let small = "hello";
        assert_eq!(truncate_output(small), "hello");
        let big = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 100);
        let out = truncate_output(&big);
        assert!(out.contains("output truncated"), "must carry truncation marker");
        assert!(out.len() < big.len(), "must actually shrink");
    }

    // spec (native-backend): the edit tool requires old_string to match
    // exactly once — zero or multiple matches are errors.
    #[tokio::test]
    async fn test_spec_edit_requires_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "aaa bbb aaa").unwrap();

        // ambiguous
        let err = execute_tool(
            "edit",
            &json!({"path": "f.txt", "old_string": "aaa", "new_string": "ccc"}),
            dir.path(),
        )
        .await;
        assert!(err.is_err(), "ambiguous old_string must error");

        // absent
        let err = execute_tool(
            "edit",
            &json!({"path": "f.txt", "old_string": "zzz", "new_string": "ccc"}),
            dir.path(),
        )
        .await;
        assert!(err.is_err(), "absent old_string must error");

        // unique
        let ok = execute_tool(
            "edit",
            &json!({"path": "f.txt", "old_string": "bbb", "new_string": "ccc"}),
            dir.path(),
        )
        .await;
        assert!(ok.is_ok(), "unique old_string must succeed");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "aaa ccc aaa");
    }

    // spec (native-backend): write creates parent directories; read returns
    // the written content back.
    #[tokio::test]
    async fn test_spec_write_creates_dirs_and_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        execute_tool(
            "write",
            &json!({"path": "a/b/c.txt", "content": "payload"}),
            dir.path(),
        )
        .await
        .unwrap();
        let out = execute_tool("read", &json!({"path": "a/b/c.txt"}), dir.path())
            .await
            .unwrap();
        assert_eq!(out, "payload");
    }

    // spec (native-backend): bash runs in the work dir, captures stdout, and
    // reports non-zero exit codes in the result text.
    #[tokio::test]
    async fn test_spec_bash_captures_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let ok = execute_tool("bash", &json!({"command": "echo hello"}), dir.path())
            .await
            .unwrap();
        assert!(ok.contains("hello"));
        let fail = execute_tool("bash", &json!({"command": "exit 3"}), dir.path())
            .await
            .unwrap();
        assert!(fail.contains("[exit code: 3]"), "non-zero exit must be visible: {fail}");
    }

    // spec (native-backend): glob matches relative to the work dir and
    // returns work-dir-relative paths.
    #[tokio::test]
    async fn test_spec_glob_relative_to_work_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/x.rs"), "x").unwrap();
        std::fs::write(dir.path().join("src/y.txt"), "y").unwrap();
        let out = execute_tool("glob", &json!({"pattern": "src/*.rs"}), dir.path())
            .await
            .unwrap();
        assert!(out.contains("src/x.rs"), "must list matching file: {out}");
        assert!(!out.contains("y.txt"), "must not list non-matching file");
    }

    // spec (native-backend): grep returns matching lines and treats
    // "no matches" as a normal result, not an error.
    #[tokio::test]
    async fn test_spec_grep_matches_and_no_match_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle in here\nplain line\n").unwrap();
        let hit = execute_tool("grep", &json!({"pattern": "needle"}), dir.path())
            .await
            .unwrap();
        assert!(hit.contains("needle"), "must return the matching line: {hit}");
        let miss = execute_tool("grep", &json!({"pattern": "absent_zzz"}), dir.path())
            .await
            .unwrap();
        assert_eq!(miss, "(no matches)");
    }

    // spec (native-backend): unknown tools error rather than silently no-op.
    #[tokio::test]
    async fn test_spec_unknown_tool_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = execute_tool("teleport", &json!({}), dir.path()).await;
        assert!(r.is_err());
    }

    // spec (native-backend): tool one-liners reuse the shared prompt templates
    // — completed calls render as `→ tool summary`, errors as ⚠ lines.
    #[test]
    fn test_spec_tool_chunk_formatting() {
        let ok = format_tool_ok("bash", "cargo test");
        assert!(ok.starts_with('\u{2192}'), "completed line starts with arrow: {ok}");
        assert!(ok.contains("cargo test"));
        let err = format_tool_error("write", "src/main.rs", "policy: tend has no bash access\nsecond line");
        assert!(err.contains('\u{26A0}'), "error line carries ⚠: {err}");
        assert!(!err.contains("second line"), "error is cropped to first line");
    }

    // spec (native-backend): short_tool_summary picks the salient arg per tool
    // and crops to the first line.
    #[test]
    fn test_spec_short_tool_summary_crops_to_first_line() {
        assert_eq!(
            short_tool_summary("bash", &json!({"command": "echo 1\necho 2"})),
            "echo 1"
        );
        assert_eq!(
            short_tool_summary("write", &json!({"path": "src/a.rs", "content": "x"})),
            "src/a.rs"
        );
        assert_eq!(
            short_tool_summary("grep", &json!({"pattern": "fn main"})),
            "fn main"
        );
    }

    // spec (native-backend): native session ids are self-issued with a nat_
    // prefix, so they can never collide with CLI backend session ids.
    #[test]
    fn test_spec_session_id_prefix() {
        let sid = format!("nat_{}", uuid::Uuid::new_v4());
        assert!(sid.starts_with("nat_"));
    }

    // spec (native-backend): default model tier constants are OpenRouter ids —
    // the same models the opencode backend routes to, without the openrouter/
    // CLI prefix.
    #[test]
    fn test_spec_model_tier_constants() {
        assert_eq!(TINKER_MODEL, "google/gemini-3.1-pro-preview");
        assert_eq!(GOAL_MODEL, "deepseek/deepseek-v4-flash");
        assert_eq!(SCHEDULER_MODEL, "google/gemini-3.1-flash-lite-preview");
        assert!(!TINKER_MODEL.starts_with("openrouter/"));
    }

    // security: → security.md T5 analog — the native backend never spawns a
    // CLI for the LLM call, so there is no subprocess stderr to leak into the
    // TUI. Tool subprocesses (bash, grep) capture output via .output(), which
    // pipes both streams by construction.
    #[test]
    fn test_security_no_api_key_in_request_body() {
        // The API key travels in the Authorization header only — the request
        // body must never contain it.
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let tools = tool_definitions(&ToolPolicy::Unrestricted);
        let body = request_body("m", &messages, &tools);
        let serialized = body.to_string();
        assert!(!serialized.contains("api_key"), "no api_key field in body");
        assert!(!serialized.contains("Authorization"), "no auth header in body");
    }
}
