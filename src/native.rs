//! Native `OpenCodeRunner` capability: talks directly to the OpenRouter
//! chat-completions API and runs the tool loop in-process.
//!
//! There is no subprocess: tinker owns tool execution, so per-role
//! capability policy (`ToolPolicy`) is enforced in code rather than via
//! system prompts. Sessions are in-memory only — a tinker restart starts
//! everyone fresh.
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

/// Resolve the permission check against the filesystem when possible.
///
/// `path` is the user-supplied string from the tool call (may be relative,
/// absolute, or contain `..`). `work_dir` is the session's working directory,
/// and `scope` is the directory the write must stay within.
///
/// **Canonical resolution** — when both the target and scope directories exist
/// on disk, `fs::canonicalize` resolves symlinks and `..` to the real path
/// before comparison. This means `.tinker/goals/../goals/foo.toml`, the
/// absolute equivalent, and a symlink pointing at the same file all produce
/// the same permission outcome.
///
/// **Lexical fallback** — when the target file doesn't exist yet (the normal
/// `write` case), canonicalize fails for it. In that case the scope is still
/// canonicalized (it must exist), and the target's parent directory is
/// canonicalized if possible; the remaining path components are compared
/// lexically. If even the parent doesn't exist, a pure lexical normalization
/// (resolving `.` and `..` components) is used — preserving prior behavior
/// rather than introducing new denial cases.
fn path_within(path: &str, work_dir: &Path, scope: &Path) -> bool {
    if path.is_empty() {
        return false;
    }
    let p = Path::new(path);
    let abs: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_dir.join(p)
    };

    // Best-effort canonical resolution: resolves symlinks, `..`, and `.`
    // against the real filesystem. Both sides canonicalized is the gold
    // standard — identical path strings for the same on-disk file.
    if let (Ok(target_canon), Ok(scope_canon)) =
        (std::fs::canonicalize(&abs), std::fs::canonicalize(scope))
    {
        return target_canon.starts_with(&scope_canon);
    }

    // Target doesn't exist yet (normal for `write`). Try to canonicalize
    // its parent and scope, then compare the canonical parent + remaining
    // tail against the canonical scope.
    if let Some(parent) = abs.parent() {
        if let (Ok(parent_canon), Ok(scope_canon)) =
            (std::fs::canonicalize(parent), std::fs::canonicalize(scope))
        {
            // Re-join the file name onto the canonical parent and check.
            let target = abs
                .file_name()
                .map(|name| parent_canon.join(name))
                .unwrap_or(parent_canon.clone());
            return target.starts_with(&scope_canon);
        }
        // Scope is canonicalized but parent isn't — compare lexical target
        // against the canonical scope.
        if let Ok(scope_canon) = std::fs::canonicalize(scope) {
            let target_lex = lexical_normalize(&abs);
            return target_lex.starts_with(&scope_canon);
        }
    }

    // Pure lexical fallback: resolve `.` and `..` without any filesystem
    // access, then check prefix.
    let target_lex = lexical_normalize(&abs);
    let scope_lex = lexical_normalize(scope);
    target_lex.starts_with(&scope_lex)
}

/// Normalize a path by resolving `.` and `..` components purely lexically
/// (no filesystem access). This is the fallback when `canonicalize` can't
/// run — the path doesn't exist yet and neither does its parent.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => {
                result.push(other);
            }
        }
    }
    result
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
/// The optional struct-level system prompt is used only for tend, whose
/// scope framing must be present from session creation. Per-call
/// `system_prompt` (goal agents, first turn) takes priority.
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

/// Extract the textual content from an assistant message, handling both
/// string and array formats as defined by the OpenAI spec.
fn extract_assistant_text(assistant: &Value) -> String {
    if let Some(content) = assistant.get("content") {
        // String form
        if let Some(s) = content.as_str() {
            return s.to_string();
        }
        // Array form – each element may be an object with a "type" and "text"
        if let Some(arr) = content.as_array() {
            let mut result = String::new();
            for part in arr {
                if part.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(txt) = part.get("text").and_then(|t| t.as_str())
                {
                    result.push_str(txt);
                }
            }
            return result;
        }
    }
    String::new()
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

            // Extract assistant text, handling both string and array formats.
            let text = extract_assistant_text(assistant);
            if !text.is_empty() {
                on_chunk(text);
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
    fn test_spec_tend_policy_rejects_escape_via_parent_traversal() {
        // .tinker/goals/../../src/main.rs resolves to /proj/src/main.rs —
        // outside scope. The `..` happens to escape; that's what's denied.
        let policy = ToolPolicy::TendScope;
        let wd = Path::new("/proj");
        let result = policy.check(
            "edit",
            &json!({"path": ".tinker/goals/../../src/main.rs", "old_string": "a", "new_string": "b"}),
            wd,
        );
        assert!(result.is_err(), ".. traversal that escapes scope must be denied");
    }

    // spec (tend-write-restriction): equivalent filesystem paths must
    // produce the same permission outcome. A `..` that resolves back
    // within scope is allowed — the check is on the resolved form, not
    // the textual representation. This uses a real tempdir so the
    // canonical resolution branch of `path_within` can kick in.
    #[test]
    fn test_spec_tend_policy_canonical_path_equivalence() {
        let dir = tempfile::tempdir().unwrap();
        // Create the scope directory so canonicalize works.
        let goals_dir = dir.path().join(".tinker/goals");
        std::fs::create_dir_all(&goals_dir).unwrap();

        let policy = ToolPolicy::TendScope;

        // ".." that resolves back within scope — previously blanket-denied,
        // now correctly allowed because the canonical form is in scope.
        let result = policy.check(
            "write",
            &json!({"path": ".tinker/goals/../goals/foo.toml", "content": "x"}),
            dir.path(),
        );
        assert!(
            result.is_ok(),
            "canonical-equivalent path with .. must be allowed, got: {result:?}"
        );

        // Absolute path with .. that resolves in scope.
        let abs_with_dotdot = format!("{}/.tinker/goals/../goals/bar.toml", dir.path().display());
        let result = policy.check(
            "write",
            &json!({"path": abs_with_dotdot, "content": "x"}),
            dir.path(),
        );
        assert!(
            result.is_ok(),
            "absolute path with .. resolved to in-scope must be allowed"
        );
    }

    // spec (native-backend): handle array‑format content from the model.
    #[test]
    fn test_spec_native_backend_handles_array_content_format() {
        // Simulate an assistant message with array content.
        let assistant = json!({
            "content": [
                {"type": "text", "text": "First part. "},
                {"type": "text", "text": "Second part."}
            ]
        });
        let extracted = extract_assistant_text(&assistant);
        assert_eq!(extracted, "First part. Second part.");
    }

    // spec (native-backend): ensure string content still works.
    #[test]
    fn test_spec_native_backend_string_content() {
        let assistant = json!({"content": "Just a simple string."});
        let extracted = extract_assistant_text(&assistant);
        assert_eq!(extracted, "Just a simple string.");
    }
}
