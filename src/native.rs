//! Native `OpenCodeRunner` capability: talks directly to any OpenAI-protocol
//! chat-completions endpoint and runs the tool loop in-process.
//!
//! There is no subprocess: tinker owns tool execution, so per-role
//! capability policy (`ToolPolicy`) is enforced in code rather than via
//! system prompts. Sessions are in-memory only — a tinker restart starts
//! everyone fresh.
//!
//! Per-tier endpoint, model, and auth are wired at construction by the
//! composition root (`main.rs`) from `model-config`'s `.tinker/config.toml`
//! plus per-tier environment variables. An unset/empty auth value means
//! no Authorization header is sent — what local model servers expect.
//!
//! Context overflow fails visibly: the API error is surfaced as a ⚠ chunk,
//! the session is dropped from the store, and `run` returns
//! `RunError::lost(...)` (no preserved session id) — which makes
//! `goal_agent_loop` clear its session id and start the next dispatch as a
//! fresh session. Transient failures (network errors, unparseable responses,
//! non-overflow API errors, missing-choices responses) save the session
//! history before surfacing and return `RunError::transient(sid, ...)` — the
//! caller resumes from `sid` on the next dispatch instead of starting fresh,
//! so a single network blip no longer wipes every prior turn from the
//! model's view (see the `Err` arm in `goal_agent_loop`).
//!
//! Transient HTTP failures are retried in-runner with exponential backoff
//! (`MAX_RETRIES` attempts at 1s/2s/4s) before surfacing: connection errors,
//! 429 rate limits, and 5xx server errors get up to three transparent
//! retries with a user-visible `↻ retry` notice between attempts. Most
//! blips never escape the runner. Non-retryable conditions (4xx except 429,
//! context overflow, unparseable bodies) surface immediately so a broken
//! server or an oversized prompt doesn't hang the session for ~7s per turn.

use crate::cap::{Chunk, OpenCodeRunner, RunError};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
#[cfg(test)]
use std::sync::Arc;
use tokio::process::Command;

/// Tool results larger than this are truncated before being fed back to the
/// model, so one verbose `cargo test` run can't blow the context window.
const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;
/// Hard cap on tool-loop iterations per `run` call — a runaway model that
/// never stops calling tools is cut off visibly instead of looping forever.
const MAX_TURNS: usize = 100;
/// Maximum HTTP request retries on transient failures (connection errors,
/// 429 rate limits, 5xx server errors) before surfacing the error. Total
/// attempts per turn = 1 + MAX_RETRIES. Retries are transparent to the
/// caller: the request body is rebuilt from `messages` each attempt and
/// `messages` is only mutated after a successful response, so a retry
/// neither duplicates turns nor corrupts the session history.
const MAX_RETRIES: usize = 3;
/// Base delay (ms) for exponential backoff between retries. Doubles each
/// attempt: 1s, 2s, 4s — total worst-case wait ~7s before giving up. Kept
/// modest so a genuinely-down endpoint surfaces quickly rather than hanging
/// the session for minutes.
const RETRY_BASE_DELAY_MS: u64 = 1000;
/// Bash commands are killed after this long.
const BASH_TIMEOUT_SECS: u64 = 300;
/// Glob results are capped at this many paths.
const MAX_GLOB_RESULTS: usize = 200;

/// True for HTTP statuses worth retrying: 429 (Too Many Requests / rate
/// limit) and 5xx (server errors — Internal Server Error, Bad Gateway,
/// Service Unavailable, Gateway Timeout). 4xx (except 429) are not
/// retryable: they indicate a client-side problem (auth, bad request, not
/// found) that will not resolve by repeating the identical request.
fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Exponential backoff delay for retry `attempt` (1-indexed: the retry
/// number about to start after attempt N failed). Returns base, 2×base,
/// 4×base for attempts 1, 2, 3 — saturating shift guards against overflow
/// if `max_retries` is ever raised beyond the bit-width of the base.
fn retry_delay_ms(attempt: usize, base_ms: u64) -> u64 {
    base_ms << attempt.saturating_sub(1)
}

/// Per-role capability policy, enforced in-process at the tool-call layer.
/// This is the reason the native backend exists: the boundary is code, not
/// a system prompt the model may or may not respect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolPolicy {
    /// Goal sessions (rummage, jog, regular sessions, cleanup): full bash,
    /// unrestricted read/write.
    Unrestricted,
    /// Tend: no bash at all; write/edit/delete only under `.tinker/goals/`.
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
                "write" | "edit" | "delete" => {
                    let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let scope = work_dir.join(".tinker").join("goals");
                    if path_within(path, work_dir, &scope) {
                        Ok(())
                    } else {
                        Err(format!(
                            "policy: tend may only {tool} under .tinker/goals/ — denied: {path}"
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
            "delete",
            "Delete a file or directory. If the path is a directory, deletes it recursively.",
            json!({"path": {"type": "string", "description": "File or directory path to delete"}}),
            &["path"],
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
    // send_message is available to every session — including tend —
    // because the spec mandates it for "all agent sessions".  The tool
    // does not touch the filesystem, so the policy layer is irrelevant;
    // the runtime enforces registry validation in the callback.  The
    // callback also enforces a self-send guard: passing the sender's
    // own id as the target returns an explicit error (to prevent
    // unbounded recursion).  The schema description calls this out so
    // the model does not waste a turn attempting the call.
    tools.push(tool_schema(
        "send_message",
        "Send a message to another agent session identified by its goal ID. The target is validated against both the session registry and the goal tree: a running session in the registry is delivered to directly; a known goal in the goal tree that is not yet running is lazy-spawned and the message is delivered into the new session. This tool does not spawn fresh sub-sessions of the caller (use the spawn_session tool for that). The recipient session starts processing the message immediately during this turn — it runs in parallel rather than waiting for this session to complete. Returns an error only when the target is unknown — not in the session registry and not in the goal tree — so the sender can route through a different agent. Also returns an error if the target is the sender's own id (a self-send would create a new turn in the same session and recurse without bound) — use spawn_session for sub-tasks of your own goal, address a different goal id for a different agent, or continue reasoning in the current turn.",
        json!({
            "target": {"type": "string", "description": "Goal ID of the recipient (e.g. 'tend', 'rummage'). Validated against both the session registry and the goal tree: a running session is delivered to directly, a known-but-unspawned goal is lazy-spawned and the message is delivered into the new session. Must not be the sender's own id (self-send is rejected to prevent unbounded recursion)."},
            "message": {"type": "string", "description": "The message body to deliver to the recipient. May span multiple lines."}
        }),
        &["target", "message"],
    ));
    // spawn_session is available to every session — including tend —
    // for the same reason send_message is.  The tool does not touch
    // the filesystem, so the policy layer is irrelevant; the runtime
    // enforces the self-only routing constraint (the new sub-session is
    // always of the *caller's* own goal) in the callback.  The schema has
    // no target parameter: the routing target is implicit in the caller.
    tools.push(tool_schema(
        "spawn_session",
        "Spawn a fresh sub-session of your own goal. The new sub-session runs concurrently with your current turn (in-turn, not at end-of-turn) — it starts processing the task immediately while you keep reasoning. Use this whenever you can decompose a sub-task into a focused unit of work: each sub-session stays focused on its task and keeps your own context tight. The sub-session may itself act as a coordinator, dispatching further sub-sessions. The `subgoal` parameter must be self-contained: the sub-session starts cold and only sees the text you pass here, so include all the context it needs without referring to 'above' or 'earlier'. Returns the sub-session id and the label (preserved as-is) on success.",
        json!({
            "subgoal": {"type": "string", "description": "The task description for the sub-session. Self-contained: include all the context the sub-session needs to complete the task without referring to your surrounding reply or earlier turns."},
            "label": {"type": "string", "description": "Optional short correlation tag (e.g. 'investigate-auth') the sub-session echoes back in its reply. Use an empty string or omit to dispatch without a label."}
        }),
        &["subgoal"],
    ));
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

/// Native runner bound to a chat-completions endpoint, a model, an
/// optional API key, and a capability policy.
///
/// The endpoint and api_key are per-instance rather than module-level
/// constants so the composition root can wire different combinations
/// per tier — a different OpenAI-protocol endpoint or no auth at all
/// (the case for local model servers). The optional struct-level
/// system prompt is used only for tend, whose scope framing must be
/// present from session creation. Per-call `system_prompt` (goal agents,
/// first turn) takes priority.
pub struct NativeRunner {
    pub endpoint: String,
    pub model: String,
    /// `None` means no Authorization header is sent (local model servers).
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub policy: ToolPolicy,
    /// Maximum HTTP retries on transient failures before surfacing. Defaults
    /// to `MAX_RETRIES`; exposed so tests can fast-fail (set to 0) and so
    /// future per-tier retry tuning can override without touching the
    /// constructors.
    pub max_retries: usize,
    /// Base delay (ms) for exponential backoff between retries. Defaults to
    /// `RETRY_BASE_DELAY_MS`; exposed for the same reasons as `max_retries`.
    pub retry_base_delay_ms: u64,
    sessions: Mutex<HashMap<String, Vec<Value>>>,
    client: reqwest::Client,
}

impl NativeRunner {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
        policy: ToolPolicy,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            api_key,
            system_prompt: None,
            policy,
            max_retries: MAX_RETRIES,
            retry_base_delay_ms: RETRY_BASE_DELAY_MS,
            sessions: Mutex::new(HashMap::new()),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_system_prompt(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
        system_prompt: impl Into<String>,
        policy: ToolPolicy,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            api_key,
            system_prompt: Some(system_prompt.into()),
            policy,
            max_retries: MAX_RETRIES,
            retry_base_delay_ms: RETRY_BASE_DELAY_MS,
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
        send_message: Option<crate::cap::SendMessageFn>,
        spawn_session: Option<crate::cap::SpawnSessionFn>,
    ) -> std::result::Result<String, RunError> {
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
                        // The session is genuinely gone — preserved_session_id
                        // is None, so the caller's Err arm clears its cached id.
                        return Err(RunError::lost(format!(
                            "unknown native session {id} — session store has no history"
                        )));
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

            // HTTP round-trip with bounded retry on transient failures
            // (connection errors, 429 rate limits, 5xx server errors).
            // Retries are transparent: the request body is rebuilt from
            // `messages` each attempt and `messages` is only mutated after
            // a successful response, so a retry neither duplicates turns
            // nor corrupts the session history. Non-retryable conditions
            // (4xx except 429, context overflow, unparseable bodies)
            // surface immediately as RunError::transient or RunError::lost
            // so the caller preserves the session for the next dispatch.
            let (status, payload): (u16, Value) = {
                let mut attempt = 0;
                loop {
                    attempt += 1;
                    let body = request_body(&self.model, &messages, &tools);
                    let mut req = self.client.post(&self.endpoint).json(&body);
                    if let Some(key) = self.api_key.as_deref() {
                        req = req.bearer_auth(key);
                    }
                    match req.send().await {
                        Ok(r) => {
                            let status = r.status().as_u16();
                            if is_retryable_status(status) && attempt <= self.max_retries {
                                let delay = retry_delay_ms(attempt, self.retry_base_delay_ms);
                                on_chunk(crate::prompts::retry_notice(
                                    attempt,
                                    self.max_retries,
                                    delay,
                                    &format!("HTTP {status}"),
                                ));
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                                continue;
                            }
                            match r.json::<Value>().await {
                                Ok(v) => break (status, v),
                                Err(e) => {
                                    // Don't retry unparseable bodies: a broken
                                    // server returning HTML error pages would
                                    // loop forever. Surface as transient so
                                    // the caller preserves the session.
                                    let msg = format!(
                                        "backend returned unparseable response (HTTP {status}): {e}"
                                    );
                                    on_chunk(crate::prompts::stream_error(&msg));
                                    self.save_session(&sid, messages);
                                    return Err(RunError::transient(sid, msg));
                                }
                            }
                        }
                        Err(e) => {
                            if attempt <= self.max_retries {
                                let delay = retry_delay_ms(attempt, self.retry_base_delay_ms);
                                on_chunk(crate::prompts::retry_notice(
                                    attempt,
                                    self.max_retries,
                                    delay,
                                    &format!("{e}"),
                                ));
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                                continue;
                            }
                            let msg = format!(
                                "backend request failed after {} retries: {e}",
                                self.max_retries
                            );
                            on_chunk(crate::prompts::stream_error(&msg));
                            self.save_session(&sid, messages);
                            return Err(RunError::transient(sid, msg));
                        }
                    }
                }
            };

            if let Some(err_msg) = api_error_message(&payload, status) {
                on_chunk(crate::prompts::stream_error(&err_msg));
                if is_context_overflow(&err_msg) {
                    // Fail visibly and drop the session: the next dispatch
                    // starts fresh (goal_agent_loop clears its sid on Err).
                    self.sessions.lock().unwrap().remove(&sid);
                    return Err(RunError::lost(format!("context overflow: {err_msg}")));
                }
                self.save_session(&sid, messages);
                return Err(RunError::transient(sid, err_msg));
            }

            let Some(assistant) = payload
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
            else {
                let msg = "backend response has no choices[0].message".to_string();
                on_chunk(crate::prompts::stream_error(&msg));
                self.save_session(&sid, messages);
                return Err(RunError::transient(sid, msg));
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
                // send_message and spawn_session are meta-tools — they do
                // not touch the filesystem, so the policy layer is
                // irrelevant and the callback is always consulted (when
                // present).  Every other tool goes through the policy
                // check first; the meta-tools skip that check entirely.
                let result = if name == "send_message" {
                    match execute_send_message(&args, send_message.as_ref()).await {
                        Ok(output) => {
                            on_chunk(format_tool_ok(&name, &summary));
                            Ok(output)
                        }
                        Err(e) => {
                            let e = format!("{e:#}");
                            on_chunk(format_tool_error(&name, &summary, &e));
                            Err(e)
                        }
                    }
                } else if name == "spawn_session" {
                    match execute_spawn_session(&args, spawn_session.as_ref()).await {
                        Ok(output) => {
                            on_chunk(format_tool_ok(&name, &summary));
                            Ok(output)
                        }
                        Err(e) => {
                            let e = format!("{e:#}");
                            on_chunk(format_tool_error(&name, &summary, &e));
                            Err(e)
                        }
                    }
                } else {
                    match self.policy.check(&name, &args, work_dir) {
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
                    }
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

/// Extract a human-readable error from an OpenAI-protocol error payload, if any.
fn api_error_message(payload: &Value, http_status: u16) -> Option<String> {
    if let Some(err) = payload.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown API error");
        return Some(format!("backend error (HTTP {http_status}): {msg}"));
    }
    if http_status >= 400 {
        return Some(format!("backend error: HTTP {http_status}"));
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
        "read" | "write" | "edit" | "delete" => s("path").unwrap_or_default(),
        "bash" => s("command").unwrap_or_default(),
        "glob" | "grep" => s("pattern").unwrap_or_default(),
        "send_message" => {
            // For send_message, "target" is the most useful one-liner — it
            // names the recipient. The message body is potentially long and
            // would be truncated by the format helper anyway.
            s("target").unwrap_or_default()
        }
        "spawn_session" => {
            // For spawn_session, the label is the most useful one-liner when
            // present (it's the correlation tag the model can use to route
            // replies). Fall back to the first line of the subgoal when no
            // label is provided so the `→ spawn_session` log line is still
            // informative about the dispatched task.
            s("label")
                .filter(|l| !l.is_empty())
                .or_else(|| s("subgoal"))
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// Execute the `send_message` tool call. Validates that `target` and
/// `message` are present, then delegates to the dispatcher callback which
/// handles registry validation and channel delivery. When the callback is
/// `None` (e.g. a test mock that does not care about dispatch), the model
/// receives an explicit error so it can route through an alternative
/// mechanism — the same error surface the registry uses for unknown targets.
async fn execute_send_message(
    args: &Value,
    send_message: Option<&crate::cap::SendMessageFn>,
) -> Result<String> {
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required argument: target"))?;
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required argument: message"))?;
    if target.is_empty() {
        return Err(anyhow!("send_message: target must be a non-empty string"));
    }
    if message.is_empty() {
        return Err(anyhow!("send_message: message must be a non-empty string"));
    }
    match send_message {
        Some(cb) => {
            // The callback returns Ok(confirmation) on successful dispatch
            // and Err(reason) on unknown target / channel failure.  We
            // forward the result as the tool output so the model sees a
            // clean acknowledgement on success and an actionable error on
            // failure.
            cb(target, message).map_err(|e| anyhow!("{e}"))
        }
        None => Err(anyhow!(
            "send_message: no dispatcher configured (this runner was started without one)"
        )),
    }
}

/// Execute the `spawn_session` tool call. Validates that `subgoal` is
/// present and non-empty, normalises the optional `label`, then delegates
/// to the dispatcher callback which fires a fresh sub-session of the
/// *caller's own goal* (the routing target is implicit in the closure —
/// the schema exposes no target parameter to the model). When the callback
/// is `None` (e.g. a test mock that does not care about dispatch), the
/// model receives an explicit error — the same error surface the runner
/// uses for `send_message`.
///
/// `label` normalisation: an explicit empty string is treated as "no
/// label" (None) so the model's "omit the label" and "pass empty string"
/// forms are equivalent — both produce a `None` label downstream and
/// the same tool result. The `subgoal` argument is forwarded verbatim;
/// the callback handles its self-containment contract.
async fn execute_spawn_session(
    args: &Value,
    spawn_session: Option<&crate::cap::SpawnSessionFn>,
) -> Result<String> {
    let subgoal = args
        .get("subgoal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required argument: subgoal"))?;
    if subgoal.is_empty() {
        return Err(anyhow!("spawn_session: subgoal must be a non-empty string"));
    }
    // Normalise the label: absent, null, and empty-string are all "no
    // label" so the model has three equivalent ways to dispatch without
    // a correlation tag. A non-empty label is preserved verbatim for the
    // callback (and ultimately for the reply).
    let label_arg = args.get("label").and_then(|v| v.as_str());
    let label: Option<&str> = match label_arg {
        Some(l) if !l.is_empty() => Some(l),
        _ => None,
    };
    match spawn_session {
        Some(cb) => {
            // The callback returns Ok((session_id, label)) on successful
            // enqueue and Err(reason) on failure. We surface both as
            // clean tool output: success returns the session id and label
            // in a model-readable form; failure forwards the reason so
            // the model can react (typically by retrying with a different
            // subgoal or by routing to a peer).
            let (session_id, returned_label) = cb(subgoal, label).map_err(|e| anyhow!("{e}"))?;
            let label_clause = match returned_label.as_deref() {
                Some(l) if !l.is_empty() => format!(" (label: `{l}`)"),
                _ => String::new(),
            };
            Ok(format!("spawned sub-session `{session_id}`{label_clause}"))
        }
        None => Err(anyhow!(
            "spawn_session: no dispatcher configured (this runner was started without one)"
        )),
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
        "delete" => {
            let path = resolve(str_arg("path")?, work_dir);
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
                    .map_err(|e| anyhow!("delete {}: {e}", path.display()))?;
                Ok(format!("deleted directory {}", path.display()))
            } else {
                std::fs::remove_file(&path)
                    .map_err(|e| anyhow!("delete {}: {e}", path.display()))?;
                Ok(format!("deleted {}", path.display()))
            }
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

    // spec (backends): retryable status predicate. 429 and 5xx are the only
    // statuses worth retrying — they indicate a transient server/rate-limit
    // condition. 4xx (except 429) are permanent client errors that will not
    // resolve by repeating the identical request.
    #[test]
    fn test_spec_is_retryable_status_covers_rate_limit_and_server_errors() {
        assert!(is_retryable_status(429), "429 rate limit must be retryable");
        assert!(is_retryable_status(500), "500 must be retryable");
        assert!(is_retryable_status(502), "502 bad gateway must be retryable");
        assert!(is_retryable_status(503), "503 service unavailable must be retryable");
        assert!(is_retryable_status(504), "504 gateway timeout must be retryable");
        assert!(is_retryable_status(599), "upper 5xx bound must be retryable");
    }

    #[test]
    fn test_spec_is_retryable_status_rejects_client_errors_and_success() {
        assert!(!is_retryable_status(200), "200 success must not be retryable");
        assert!(!is_retryable_status(400), "400 bad request must not be retryable");
        assert!(!is_retryable_status(401), "401 unauthorized must not be retryable");
        assert!(!is_retryable_status(403), "403 forbidden must not be retryable");
        assert!(!is_retryable_status(404), "404 not found must not be retryable");
        assert!(!is_retryable_status(413), "413 payload too large (overflow-adjacent) must not be retryable");
    }

    // spec (backends): exponential backoff. Each retry doubles the delay so
    // a flapping endpoint gets progressively more breathing room without
    // hanging the session for minutes. Saturating shift prevents overflow
    // if max_retries is ever raised high.
    #[test]
    fn test_spec_retry_delay_ms_doubles_each_attempt() {
        let base = 1000u64;
        assert_eq!(retry_delay_ms(1, base), 1000, "first retry: base");
        assert_eq!(retry_delay_ms(2, base), 2000, "second retry: 2× base");
        assert_eq!(retry_delay_ms(3, base), 4000, "third retry: 4× base");
        assert_eq!(retry_delay_ms(4, base), 8000, "fourth retry: 8× base");
    }

    #[test]
    fn test_spec_retry_delay_ms_respects_configured_base() {
        // Tests (and future per-tier config) can set a smaller base for
        // faster failure. The doubling ratio is preserved regardless of base.
        assert_eq!(retry_delay_ms(1, 100), 100);
        assert_eq!(retry_delay_ms(2, 100), 200);
        assert_eq!(retry_delay_ms(3, 100), 400);
    }

    // spec (backends): default retry config on freshly-constructed runners
    // must match the module constants, so production behavior is stable
    // unless a caller explicitly overrides.
    #[test]
    fn test_spec_native_runner_defaults_match_constants() {
        let r = NativeRunner::new("http://x", "m", None, ToolPolicy::Unrestricted);
        assert_eq!(r.max_retries, MAX_RETRIES, "new() must default max_retries to MAX_RETRIES");
        assert_eq!(r.retry_base_delay_ms, RETRY_BASE_DELAY_MS, "new() must default retry_base_delay_ms to RETRY_BASE_DELAY_MS");
        let r2 = NativeRunner::with_system_prompt("http://x", "m", None, "sp", ToolPolicy::TendScope);
        assert_eq!(r2.max_retries, MAX_RETRIES, "with_system_prompt() must default max_retries to MAX_RETRIES");
        assert_eq!(r2.retry_base_delay_ms, RETRY_BASE_DELAY_MS, "with_system_prompt() must default retry_base_delay_ms to RETRY_BASE_DELAY_MS");
    }

    // spec (backends): a transient network failure must NOT lose the session.
    // The runner saves the session history before surfacing the error and
    // returns RunError::transient(sid, ...) so goal_agent_loop can resume
    // from sid on the next dispatch. This is the regression test for the bug
    // where a single network blip wiped every prior turn: the runner now
    // signals preservation, the store holds the history, and the caller's
    // Err arm honors the signal.
    //
    // Port 1 on loopback refuses connections immediately (ECONNREFUSED), so
    // this test does not depend on timeouts and is stable under load.
    #[tokio::test]
    async fn test_spec_native_transient_network_failure_preserves_session() {
        let mut runner = NativeRunner::new(
            "http://127.0.0.1:1/v1/chat/completions",
            "test-model",
            None,
            ToolPolicy::Unrestricted,
        );
        // Fast-fail: skip retries so this test stays sub-second. The
        // connection-refused endpoint would otherwise burn the full
        // backoff budget (1s+2s+4s) on every run.
        runner.max_retries = 0;
        let on_sid: Chunk = Box::new(|_| {});
        let on_chunk: Chunk = Box::new(|_| {});
        let result = runner
            .run("hi", None, Path::new("."), None, on_sid, on_chunk, None, None)
            .await;
        let err = result.expect_err("connection-refused endpoint must return Err");
        assert!(
            err.session_preserved(),
            "transient network failure must mark the session as preserved; got message: {}",
            err.message
        );
        let sid = err
            .preserved_session_id
            .as_ref()
            .expect("transient failure must carry a preserved sid")
            .clone();
        assert!(
            sid.starts_with("nat_"),
            "preserved sid must be the runner-generated nat_<uuid>, got {sid}"
        );
        // The history must actually be in the store so a resume works —
        // the contract is not just "we returned the sid" but "the sid is
        // loadable on the next dispatch".
        let store = runner.sessions.lock().unwrap();
        assert!(
            store.contains_key(&sid),
            "transient failure must leave the session history in the store for resume"
        );
    }

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

    // spec (tend-write-restriction): deletion follows the same permission
    // boundary as write/edit — allowed under .tinker/goals/, denied outside.
    #[test]
    fn test_spec_tend_policy_delete_scope() {
        let policy = ToolPolicy::TendScope;
        let wd = Path::new("/proj");
        let check = |p: &str| policy.check("delete", &json!({"path": p}), wd);
        assert!(check(".tinker/goals/foo.toml").is_ok(), "goal file delete must be allowed");
        assert!(check("/proj/.tinker/goals/bar.toml").is_ok(), "absolute in-scope delete must be allowed");
        assert!(check("src/main.rs").is_err(), "src delete must be denied");
        assert!(check(".tinker/notes/notes.md").is_err(), "notes delete must be denied");
        assert!(check("/etc/passwd").is_err(), "out-of-tree delete must be denied");
        assert!(check("").is_err(), "empty path must be denied");
    }

    // spec (tend-write-restriction): delete executes for files and directories
    // under scope, and path-equivalence recognition applies to deletion too.
    #[tokio::test]
    async fn test_spec_tend_policy_delete_canonical_equivalence() {
        let dir = tempfile::tempdir().unwrap();
        let goals_dir = dir.path().join(".tinker/goals");
        std::fs::create_dir_all(&goals_dir).unwrap();
        // Create a file to delete.
        std::fs::write(goals_dir.join("retired.toml"), "x").unwrap();

        let policy = ToolPolicy::TendScope;

        // .. that resolves back in scope — must be allowed by the policy.
        let result = policy.check(
            "delete",
            &json!({"path": ".tinker/goals/../goals/retired.toml"}),
            dir.path(),
        );
        assert!(result.is_ok(), "canonical-equivalent delete path must be allowed");

        // Actual execution via execute_tool.
        let result = execute_tool(
            "delete",
            &json!({"path": ".tinker/goals/retired.toml"}),
            dir.path(),
        )
        .await;
        assert!(result.is_ok(), "delete execution must succeed");
        assert!(!goals_dir.join("retired.toml").exists(), "file must be gone");
    }

    // spec (tend-write-restriction): delete tool is in the definitions for
    // every policy variant (it's a base filesystem tool, like write/edit).
    #[test]
    fn test_spec_delete_tool_in_definitions_for_every_policy() {
        for policy in [ToolPolicy::Unrestricted, ToolPolicy::TendScope] {
            let tools = tool_definitions(&policy);
            let names: Vec<&str> = tools
                .iter()
                .filter_map(|t| t.pointer("/function/name").and_then(|v| v.as_str()))
                .collect();
            assert!(
                names.contains(&"delete"),
                "delete must be in tool definitions for {policy:?}"
            );
        }
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

    // ── send_message tool tests ──────────────────────────────────────────

    // spec (send-message): the send_message tool must appear in the function
    // schema for every ToolPolicy variant, including TendScope.  The tool is
    // a meta-tool — it does not touch the filesystem, so the policy layer
    // is irrelevant and the schema must be uniformly available.
    #[test]
    fn test_spec_send_message_tool_in_definitions_for_every_policy() {
        for policy in [ToolPolicy::Unrestricted, ToolPolicy::TendScope] {
            let tools = tool_definitions(&policy);
            let found = tools.iter().any(|t| {
                t.pointer("/function/name").and_then(|n| n.as_str()) == Some("send_message")
            });
            assert!(
                found,
                "send_message must be in tool definitions for policy {policy:?}"
            );
        }
    }

    // spec (send-message): the send_message schema must require both `target`
    // and `message` parameters.  Missing either would let the model produce
    // a tool call that the runner accepts but cannot dispatch — the exact
    // format-fragility failure mode the tool replaces.
    #[test]
    fn test_spec_send_message_schema_requires_target_and_message() {
        let tools = tool_definitions(&ToolPolicy::Unrestricted);
        let tool = tools
            .iter()
            .find(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some("send_message"))
            .expect("send_message tool must be present");
        let required = tool
            .pointer("/function/parameters/required")
            .and_then(|r| r.as_array())
            .expect("send_message must declare required parameters");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"target"), "target must be a required parameter");
        assert!(names.contains(&"message"), "message must be a required parameter");
    }

    // spec (send-message): tool description must explicitly mention the
    // session registry and that the tool fires immediately during the
    // turn, so the model knows the dispatch is synchronous and in-turn.
    #[test]
    fn test_spec_send_message_schema_documents_registry_and_in_turn_dispatch() {
        let tools = tool_definitions(&ToolPolicy::Unrestricted);
        let tool = tools
            .iter()
            .find(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some("send_message"))
            .expect("send_message tool must be present");
        let desc = tool
            .pointer("/function/description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        assert!(
            desc.contains("session registry"),
            "description must mention the session registry requirement: {desc}"
        );
        assert!(
            desc.to_lowercase().contains("immediately")
                || desc.to_lowercase().contains("during this turn"),
            "description must explain that the dispatch is in-turn: {desc}"
        );
    }

    // spec (send-message): the schema description must reflect the
    // registry-or-lazy-spawn behavior — a known goal that has no
    // running session is a valid target, the runtime spawns the
    // session and delivers into it. Earlier versions of the
    // description said "the recipient must already exist in the
    // session registry", which contradicts the lazy-spawn path and
    // would mislead a model into thinking a first-contact message to
    // a known goal would fail. This test pins the description to the
    // actual behavior: the description must mention both the session
    // registry and the goal tree, must include the lazy-spawn /
    // known-goal framing, and must NOT contain the "must already
    // exist" framing. The error path is also pinned: an error fires
    // only when the target is unknown — not in either the registry
    // or the goal tree — not just when the target is not a "known
    // session". This locks the description to the registry-then-
    // goal-tree validation the dispatcher actually implements.
    #[test]
    fn test_spec_send_message_schema_documents_registry_or_lazy_spawn() {
        let tools = tool_definitions(&ToolPolicy::Unrestricted);
        let tool = tools
            .iter()
            .find(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some("send_message"))
            .expect("send_message tool must be present");
        let desc = tool
            .pointer("/function/description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let target_desc = tool
            .pointer("/function/parameters/properties/target/description")
            .and_then(|d| d.as_str())
            .unwrap_or("");

        // Both descriptions must mention the session registry (it's
        // one half of the validation surface).
        assert!(
            desc.contains("session registry"),
            "tool description must mention the session registry: {desc}"
        );
        assert!(
            target_desc.contains("session registry"),
            "target property description must mention the session registry: {target_desc}"
        );
        // Both descriptions must mention the goal tree (it's the
        // other half — the lazy-spawn source).
        assert!(
            desc.contains("goal tree"),
            "tool description must mention the goal tree (the lazy-spawn source): {desc}"
        );
        assert!(
            target_desc.contains("goal tree"),
            "target property description must mention the goal tree: {target_desc}"
        );
        // Both descriptions must mention the lazy-spawn behaviour
        // so the model knows a known goal with no running session is
        // a valid target.
        let mentions_lazy_spawn = |s: &str| -> bool {
            let lower = s.to_lowercase();
            lower.contains("lazy-spawn")
                || lower.contains("lazy spawn")
                || lower.contains("spawns the session")
                || lower.contains("spawned and the message is delivered")
                || lower.contains("is lazy-spawned")
        };
        assert!(
            mentions_lazy_spawn(desc),
            "tool description must mention the lazy-spawn behaviour: {desc}"
        );
        assert!(
            mentions_lazy_spawn(target_desc),
            "target property description must mention the lazy-spawn behaviour: {target_desc}"
        );
        // The error framing must say "unknown" (not just "not a
        // known session") — an error fires only when the target is
        // in neither the registry nor the goal tree.
        assert!(
            desc.contains("unknown"),
            "tool description must say the error fires when the target is unknown (not just not in the registry): {desc}"
        );
        // The descriptions must NOT contain the misleading framing
        // that the recipient must already exist in the session
        // registry — that wording contradicts the lazy-spawn path.
        assert!(
            !desc.contains("must already exist in the session registry"),
            "tool description must not say the recipient must already exist in the session registry (a known-but-unspawned goal is a valid target): {desc}"
        );
        assert!(
            !target_desc.contains("must already exist in the session registry"),
            "target property description must not say the recipient must already exist in the session registry: {target_desc}"
        );
    }

    // spec (send-message): short_tool_summary on send_message returns the
    // target name, so the `→ send_message target` log line is informative
    // (mirrors the `→ Write path` / `→ Bash command` form the other tools
    // produce).
    #[test]
    fn test_spec_send_message_short_summary_returns_target() {
        let args = json!({"target": "rummage", "message": "investigate the auth flow"});
        let s = short_tool_summary("send_message", &args);
        assert_eq!(s, "rummage", "short summary must name the target");
        // Empty target: summary must be empty (no spurious content).
        let args_empty = json!({"target": "", "message": "x"});
        let s_empty = short_tool_summary("send_message", &args_empty);
        assert_eq!(s_empty, "", "empty target must produce empty summary");
    }

    // spec (send-message): execute_send_message delegates to the callback
    // when one is provided.  Successful callback invocation returns Ok with
    // the callback's confirmation string — the model sees a clean ack.
    #[tokio::test]
    async fn test_spec_execute_send_message_delegates_to_callback() {
        let captured: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: crate::cap::SendMessageFn = Arc::new(move |target: &str, message: &str| {
            *captured_clone.lock().unwrap() = Some((target.to_string(), message.to_string()));
            Ok(format!("delivered to `{target}`"))
        });
        let result = execute_send_message(
            &json!({"target": "rummage", "message": "trace the init"}),
            Some(&cb),
        )
        .await;
        let out = result.expect("callback success must yield Ok result");
        assert!(out.contains("rummage"), "ok result should confirm the recipient");
        let (t, m) = captured.lock().unwrap().clone().expect("callback was invoked");
        assert_eq!(t, "rummage");
        assert_eq!(m, "trace the init");
    }

    // spec (send-message): when the callback returns Err, the runner surfaces
    // the error as an Err to the model — the failure path is explicit, never
    // a silent loss.
    #[tokio::test]
    async fn test_spec_execute_send_message_surfaces_callback_error() {
        let cb: crate::cap::SendMessageFn = Arc::new(|_t: &str, _m: &str| {
            Err("target `rummage` is not in the session registry".to_string())
        });
        let err = execute_send_message(
            &json!({"target": "rummage", "message": "go"}),
            Some(&cb),
        )
        .await
        .expect_err("callback Err must produce Err result");
        assert!(
            err.to_string().contains("not in the session registry"),
            "error must surface the registry-miss reason verbatim: {err}"
        );
    }

    // spec (send-message): no callback configured returns an explicit error
    // naming the misconfiguration.  This is the harness's signal that the
    // feature is not wired (e.g. a test mock that does not care about
    // dispatch) — the model can route through a different mechanism.
    #[tokio::test]
    async fn test_spec_execute_send_message_no_callback_returns_error() {
        let err = execute_send_message(
            &json!({"target": "rummage", "message": "go"}),
            None,
        )
        .await
        .expect_err("no callback must yield Err");
        assert!(
            err.to_string().contains("no dispatcher configured"),
            "error must name the misconfiguration: {err}"
        );
    }

    // spec (send-message): missing arguments produce explicit errors, not
    // silent acceptance.  The runner must reject malformed tool calls
    // rather than accepting them and producing an empty dispatch.
    #[tokio::test]
    async fn test_spec_execute_send_message_missing_target_errors() {
        let cb: crate::cap::SendMessageFn = Arc::new(|_, _| Ok("ok".to_string()));
        let err = execute_send_message(
            &json!({"message": "go"}),
            Some(&cb),
        )
        .await
        .expect_err("missing target must error");
        assert!(err.to_string().contains("target"), "error must name the missing arg");
    }

    // spec (send-message): missing message argument errors.
    #[tokio::test]
    async fn test_spec_execute_send_message_missing_message_errors() {
        let cb: crate::cap::SendMessageFn = Arc::new(|_, _| Ok("ok".to_string()));
        let err = execute_send_message(
            &json!({"target": "rummage"}),
            Some(&cb),
        )
        .await
        .expect_err("missing message must error");
        assert!(err.to_string().contains("message"), "error must name the missing arg");
    }

    // spec (send-message): empty target is rejected.  An empty target
    // string would be a tool call the model produced by accident — refusing
    // it explicitly is safer than treating it as a generic send.
    #[tokio::test]
    async fn test_spec_execute_send_message_empty_target_rejected() {
        let cb: crate::cap::SendMessageFn = Arc::new(|_, _| Ok("ok".to_string()));
        let err = execute_send_message(
            &json!({"target": "", "message": "go"}),
            Some(&cb),
        )
        .await
        .expect_err("empty target must error");
        assert!(err.to_string().contains("non-empty"), "error must name the empty-arg invariant");
    }

    // spec (send-message): empty message is rejected.  An accidental
    // empty-string send would be a silent zero-content delivery.
    #[tokio::test]
    async fn test_spec_execute_send_message_empty_message_rejected() {
        let cb: crate::cap::SendMessageFn = Arc::new(|_, _| Ok("ok".to_string()));
        let err = execute_send_message(
            &json!({"target": "rummage", "message": ""}),
            Some(&cb),
        )
        .await
        .expect_err("empty message must error");
        assert!(err.to_string().contains("non-empty"), "error must name the empty-arg invariant");
    }

    // spec (send-message): a self-send rejection surfaces in the
    // conversation pane as a ⚠ line the user can read. The runner
    // calls `format_tool_error(&name, &summary, &e)` to build the
    // streamed chunk, which substitutes the first newline-bounded
    // line of the dispatcher's Err string into the `⚠ {TOOL}
    // {SUMMARY}: {ERROR}` template. Because the dispatcher's
    // self-send error is a single line (no embedded newlines), the
    // entire string — the self-send condition, the offending target
    // id, AND the alternatives-aware course-correction pointers —
    // appears in the streamed ⚠ line. The same string also lands
    // in the tool result for the model, so both audiences see the
    // full content. This test pins the conversation-pane surface:
    // the ⚠ line must start with the tool name and target summary,
    // and must contain the self-send condition, the offending
    // target id, and all three course-correction pointers.
    #[test]
    fn test_spec_send_message_self_send_warning_line_surfaces_target_and_condition() {
        // The dispatcher's self-send Err string. A single line —
        // everything appears in the ⚠ line.
        let dispatcher_err = "send_message: cannot send a message to yourself (`tend` is the same as the sender). To dispatch a sub-task of your own goal, use the spawn_session tool; to reach a different agent, address their goal id; otherwise continue reasoning in the current turn";

        // short_tool_summary("send_message", ...) returns the target.
        // For self-send the target equals the sender, so the summary
        // is the sender's id — visible in the ⚠ line.
        let summary = "tend";
        let line = format_tool_error("send_message", summary, dispatcher_err);

        // The ⚠ line is the template `⚠ {TOOL} {SUMMARY}: {ERROR}`,
        // where ERROR is the first newline-bounded line of the
        // dispatcher's string. The error has no newlines, so the
        // full string flows through.
        assert!(
            line.starts_with("⚠ send_message tend:"),
            "warning line must start with `⚠ send_message tend:` — tool name and target summary visible to the user: {line:?}"
        );
        // The warning line must surface the self-send condition so
        // the user can recognise what happened.
        assert!(
            line.contains("cannot send a message to yourself"),
            "warning line must surface the self-send condition: {line:?}"
        );
        // The warning line must name the offending target (the
        // summary's value re-appears inside the error text, so the
        // user sees it twice — once in the position reserved for
        // the target, once in the descriptive text).
        assert!(
            line.contains("`tend`"),
            "warning line must name the offending target id: {line:?}"
        );
        // Because the full error string flows through the first
        // newline-bounded line, the alternatives-aware pointers
        // (spawn_session, different goal id, continue reasoning)
        // also appear in the streamed ⚠ line. Locking this in keeps
        // the human-visible surface aligned with the model's
        // tool-result surface — both see the full course-correction
        // content. If the dispatcher error is later split into
        // multiple lines, this assertion will fail and the test
        // will need to be re-thought for the new line structure.
        assert!(
            line.contains("spawn_session"),
            "warning line must include the spawn_session pointer: {line:?}"
        );
        assert!(
            line.contains("goal id"),
            "warning line must include the goal-id pointer: {line:?}"
        );
        assert!(
            line.contains("continue reasoning"),
            "warning line must include the continue-reasoning fallback: {line:?}"
        );
    }

    // spec (send-message): the send_message tool is available to tend too
    // (its ToolPolicy is TendScope which strips bash).  Defense in depth
    // applies: the schema inclusion is the first line; the policy check
    // would also deny bash, but send_message must NOT be subject to any
    // policy check at all (it's a meta-tool).
    #[test]
    fn test_spec_send_message_for_tend_includes_tool_definitions() {
        let tools = tool_definitions(&ToolPolicy::TendScope);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"send_message"),
            "tend must see send_message: tools were {names:?}"
        );
        // bash is the tool tend's policy strips — make sure the comparison
        // is meaningful: tend's tool list genuinely differs from the
        // unrestricted list in this one place.
        let tools_full = tool_definitions(&ToolPolicy::Unrestricted);
        let full_names: Vec<&str> = tools_full
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            !names.contains(&"bash"),
            "tend's policy must still strip bash (control test): {names:?}"
        );
        assert!(
            full_names.contains(&"bash"),
            "unrestricted policy must include bash (control test): {full_names:?}"
        );
    }

    // ── spawn_session tool tests ─────────────────────────────────────────

    // spec (spawn-session): the spawn_session tool must appear in the
    // function schema for every ToolPolicy variant, including TendScope.
    // Like send_message, it is a meta-tool — the policy layer does not
    // apply and the schema must be uniformly available.  tend uses
    // spawn_session for delegating work to goal agents, so the tool must
    // be visible to tend.
    #[test]
    fn test_spec_spawn_session_tool_in_definitions_for_every_policy() {
        for policy in [ToolPolicy::Unrestricted, ToolPolicy::TendScope] {
            let tools = tool_definitions(&policy);
            let found = tools.iter().any(|t| {
                t.pointer("/function/name").and_then(|n| n.as_str()) == Some("spawn_session")
            });
            assert!(
                found,
                "spawn_session must be in tool definitions for policy {policy:?}"
            );
        }
    }

    // spec (spawn-session): the spawn_session schema must require `subgoal`
    // — the sub-task body is the load-bearing argument, and a tool call
    // without it would produce a sub-session with no task. `label` is
    // optional (correlation tag is convenience, not load-bearing).
    #[test]
    fn test_spec_spawn_session_schema_requires_subgoal_only() {
        let tools = tool_definitions(&ToolPolicy::Unrestricted);
        let tool = tools
            .iter()
            .find(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some("spawn_session"))
            .expect("spawn_session tool must be present");
        let required = tool
            .pointer("/function/parameters/required")
            .and_then(|r| r.as_array())
            .expect("spawn_session must declare required parameters");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"subgoal"), "subgoal must be a required parameter");
        // label is optional; the spec does not require it. The schema
        // must NOT mandate it — that would block the bare-task form
        // (e.g. when the label is omitted) where no label is
        // wanted.
        assert!(!names.contains(&"label"), "label must NOT be a required parameter");
    }

    // spec (spawn-session): the spawn_session schema must explicitly
    // document the two load-bearing properties of the tool: (1) it spawns
    // a sub-session of the *caller's* own goal (no arbitrary target) and
    // (2) it fires in-turn, not at end-of-turn. Both must be in the
    // description so the model picks the tool correctly.
    #[test]
    fn test_spec_spawn_session_schema_documents_self_only_and_in_turn_dispatch() {
        let tools = tool_definitions(&ToolPolicy::Unrestricted);
        let tool = tools
            .iter()
            .find(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some("spawn_session"))
            .expect("spawn_session tool must be present");
        let desc = tool
            .pointer("/function/description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        // Self-only: the description must say the sub-session is "of your
        // own goal" — the model should not expect to address an arbitrary
        // target through this tool.
        assert!(
            desc.contains("your own goal"),
            "description must state the sub-session is of the caller's own goal: {desc}"
        );
        // The schema must NOT expose a target parameter; the routing
        // target is implicit in the caller.  A target parameter would
        // generalise the tool to send-message's lazy-spawn territory and
        // break the domain split.
        let properties = tool
            .pointer("/function/parameters/properties")
            .expect("spawn_session must declare properties");
        let prop_names: Vec<&str> = properties
            .as_object()
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        assert!(
            !prop_names.contains(&"target"),
            "spawn_session must NOT expose a target parameter (self-only constraint): {prop_names:?}"
        );
        // In-turn dispatch: the description must explain that the spawn
        // fires immediately during the caller's turn, so the model
        // knows the dispatch is synchronous and in-turn.
        assert!(
            desc.to_lowercase().contains("in-turn")
                || desc.to_lowercase().contains("immediately during")
                || desc.to_lowercase().contains("concurrently"),
            "description must explain in-turn dispatch: {desc}"
        );
    }

    // spec (spawn-session): short_tool_summary on spawn_session returns
    // the label when one is provided, so the `→ spawn_session label`
    // log line mirrors the `→ send_message target` form. When no label
    // is provided, the summary falls back to the first line of the
    // subgoal so the log line still names the dispatched task.
    #[test]
    fn test_spec_spawn_session_short_summary_prefers_label() {
        // Label present: summary must be the label, not the subgoal.
        let args = json!({
            "subgoal": "Trace the auth flow through the login handler",
            "label": "investigate-auth"
        });
        let s = short_tool_summary("spawn_session", &args);
        assert_eq!(s, "investigate-auth", "label must take priority over subgoal");

        // No label (key absent): summary must fall back to first line
        // of subgoal so the log line is still informative.
        let args_no_label = json!({
            "subgoal": "Trace the auth flow through the login handler"
        });
        let s_no_label = short_tool_summary("spawn_session", &args_no_label);
        assert_eq!(
            s_no_label, "Trace the auth flow through the login handler",
            "fallback must be the first line of subgoal"
        );

        // Empty-string label: treated as "no label" and falls back to
        // subgoal first line. Mirrors the executor's label normalisation.
        let args_empty_label = json!({
            "subgoal": "Trace the auth flow",
            "label": ""
        });
        let s_empty = short_tool_summary("spawn_session", &args_empty_label);
        assert_eq!(
            s_empty, "Trace the auth flow",
            "empty-string label must fall back to subgoal first line"
        );
    }

    // spec (spawn-session): execute_spawn_session delegates to the
    // callback when one is provided. The callback's return value
    // (session id + label) is surfaced as a clean tool result the model
    // can route replies to.
    #[tokio::test]
    async fn test_spec_execute_spawn_session_delegates_to_callback() {
        // Capture cell for the (subgoal, label) the callback received.
        type Captured = Arc<Mutex<Option<(String, Option<String>)>>>;
        let captured: Captured = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: crate::cap::SpawnSessionFn = Arc::new(move |subgoal: &str, label: Option<&str>| {
            *captured_clone.lock().unwrap() = Some((subgoal.to_string(), label.map(String::from)));
            Ok(("rummage~3".to_string(), label.map(String::from)))
        });
        let result = execute_spawn_session(
            &json!({"subgoal": "trace the init", "label": "investigate-auth"}),
            Some(&cb),
        )
        .await;
        let out = result.expect("callback success must yield Ok result");
        assert!(out.contains("rummage~3"), "ok result should name the spawned sub-session");
        assert!(
            out.contains("investigate-auth"),
            "ok result should preserve the label for reply routing: {out}"
        );
        let (sub, lbl) = captured.lock().unwrap().clone().expect("callback was invoked");
        assert_eq!(sub, "trace the init");
        assert_eq!(lbl, Some("investigate-auth".to_string()));
    }

    // spec (spawn-session): execute_spawn_session with no label still
    // delegates to the callback and surfaces the session id in the
    // result. The label clause is omitted from the human-readable
    // string when no label was provided.
    #[tokio::test]
    async fn test_spec_execute_spawn_session_no_label() {
        let cb: crate::cap::SpawnSessionFn = Arc::new(|_subgoal, _label| {
            Ok(("rummage~5".to_string(), None))
        });
        let result = execute_spawn_session(
            &json!({"subgoal": "trace the init"}),
            Some(&cb),
        )
        .await;
        let out = result.expect("callback success must yield Ok result");
        assert!(out.contains("rummage~5"), "ok result should name the spawned sub-session");
        // No label clause when label is None.
        assert!(
            !out.contains("label:"),
            "ok result must omit label clause when no label was provided: {out}"
        );
    }

    // spec (spawn-session): when the callback returns Err, the runner
    // surfaces the error as an Err to the model — the failure path is
    // explicit, never a silent loss.
    #[tokio::test]
    async fn test_spec_execute_spawn_session_surfaces_callback_error() {
        let cb: crate::cap::SpawnSessionFn = Arc::new(|_subgoal, _label| {
            Err("caller goal `ghost` is not in the goal tree".to_string())
        });
        let err = execute_spawn_session(
            &json!({"subgoal": "trace the init"}),
            Some(&cb),
        )
        .await
        .expect_err("callback Err must produce Err result");
        assert!(
            err.to_string().contains("not in the goal tree"),
            "error must surface the goal-tree-miss reason verbatim: {err}"
        );
    }

    // spec (spawn-session): no callback configured returns an explicit
    // error naming the misconfiguration. The harness's signal that the
    // feature is not wired — the model must route to a peer instead.
    #[tokio::test]
    async fn test_spec_execute_spawn_session_no_callback_returns_error() {
        let err = execute_spawn_session(
            &json!({"subgoal": "trace the init"}),
            None,
        )
        .await
        .expect_err("no callback must yield Err");
        assert!(
            err.to_string().contains("no dispatcher configured"),
            "error must name the misconfiguration: {err}"
        );
    }

    // spec (spawn-session): missing subgoal produces an explicit error
    // — defence-in-depth against malformed tool calls. A tool call
    // without subgoal would otherwise produce a sub-session with no
    // task.
    #[tokio::test]
    async fn test_spec_execute_spawn_session_missing_subgoal_errors() {
        let cb: crate::cap::SpawnSessionFn = Arc::new(|_, _| Ok(("x~1".to_string(), None)));
        let err = execute_spawn_session(
            &json!({"label": "no-subgoal"}),
            Some(&cb),
        )
        .await
        .expect_err("missing subgoal must error");
        assert!(err.to_string().contains("subgoal"), "error must name the missing arg");
    }

    // spec (spawn-session): empty subgoal is rejected. An empty-string
    // dispatch would produce a sub-session with no task — refusing
    // explicitly is safer than treating it as a generic spawn.
    #[tokio::test]
    async fn test_spec_execute_spawn_session_empty_subgoal_rejected() {
        let cb: crate::cap::SpawnSessionFn = Arc::new(|_, _| Ok(("x~1".to_string(), None)));
        let err = execute_spawn_session(
            &json!({"subgoal": ""}),
            Some(&cb),
        )
        .await
        .expect_err("empty subgoal must error");
        assert!(err.to_string().contains("non-empty"), "error must name the empty-arg invariant");
    }

    // spec (spawn-session): empty-string label is normalised to None
    // before reaching the callback, so the model's "omit the label"
    // and "pass empty string" forms are equivalent.
    #[tokio::test]
    async fn test_spec_execute_spawn_session_empty_label_normalised_to_none() {
        let captured: Arc<Mutex<Option<Option<String>>>> = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let cb: crate::cap::SpawnSessionFn = Arc::new(move |_subgoal, label: Option<&str>| {
            *captured_clone.lock().unwrap() = Some(label.map(String::from));
            Ok(("x~1".to_string(), label.map(String::from)))
        });
        let result = execute_spawn_session(
            &json!({"subgoal": "trace the init", "label": ""}),
            Some(&cb),
        )
        .await
        .expect("empty-string label must not error");
        let observed = captured.lock().unwrap().clone().expect("callback was invoked");
        assert!(
            observed.is_none(),
            "empty-string label must be normalised to None before the callback, got {observed:?}"
        );
        // The result string omits the label clause when label is None.
        assert!(!result.contains("label:"), "result must omit label clause when label is None: {result}");
    }
}
