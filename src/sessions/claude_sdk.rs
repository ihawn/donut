//! The part of the Python `claude-agent-sdk` (0.2.185, `ClaudeSDKClient`) that Donut uses, in Rust.
//!
//! It spawns the `claude` CLI in stream-json mode and speaks the SDK's control protocol over its stdio:
//! the `initialize` request, `can_use_tool` requests answered from a callback, `interrupt`,
//! `set_permission_mode`, `set_model` and any other control request. A reader thread frames the CLI's NDJSON
//! output (with a message size limit), routes control traffic and hands every other message to the
//! [`MessageStream`] as a typed [`Message`].
//!
//! Hooks, SDK MCP servers, agents, session stores and the other SDK features Donut never turns on are not
//! ported; a control request of such a kind is answered with an error, as the SDK does when it has no handler.

use std::collections::HashMap;
use std::io::{BufReader, ErrorKind, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::{kill_pid, lock, py_truthy, read_line_limited, terminate_pid, wait_exit};

/// The SDK version Donut reports to the CLI as `CLAUDE_AGENT_SDK_VERSION`.
pub const SDK_VERSION: &str = "0.2.185";
/// The SDK's own default for the longest message; Donut raises it.
pub const DEFAULT_MAX_BUFFER_SIZE: usize = 1024 * 1024;

const DERIVED_CLIENT_APP_ENV: &str = "CLAUDE_AGENT_SDK_DERIVED_CLIENT_APP";
const READS_SESSION_STATE_ENV: &str = "CLAUDE_CODE_SDK_READS_SESSION_STATE";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

/// What the permission callback decides for one tool call.
#[derive(Debug, Clone)]
pub enum PermissionResult {
    /// Run the tool with this input (normally the input it asked with).
    Allow { updated_input: Value },
    /// Refuse, telling Claude why.
    Deny { message: String },
}

/// Answers `can_use_tool` control requests: `(tool name, tool input)`. It may block for as long as the user
/// takes; it runs on its own thread, so the reader keeps going.
pub type ToolApprover = Arc<dyn Fn(&str, &Value) -> PermissionResult + Send + Sync>;

/// The `ClaudeAgentOptions` fields Donut sets.
#[derive(Debug, Clone)]
pub struct ClaudeOptions {
    /// The `claude` executable; searched for when `None`.
    pub cli_path: Option<String>,
    /// Working directory of the CLI.
    pub cwd: Option<PathBuf>,
    /// Environment variables added to (and overriding) the inherited ones.
    pub env: Vec<(String, String)>,
    /// `--model`.
    pub model: Option<String>,
    /// `--setting-sources`; `Some(vec![])` passes an empty list (no settings at all).
    pub setting_sources: Option<Vec<String>>,
    /// `--permission-mode`.
    pub permission_mode: Option<String>,
    /// `--include-partial-messages`.
    pub include_partial_messages: bool,
    /// The longest accepted line of CLI output, in bytes.
    pub max_buffer_size: usize,
    /// `--resume=<session id>`.
    pub resume: Option<String>,
    /// `--effort`.
    pub effort: Option<String>,
    /// Further flags: `(name without dashes, optional value)`.
    pub extra_args: Vec<(String, Option<String>)>,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        ClaudeOptions {
            cli_path: None,
            cwd: None,
            env: Vec::new(),
            model: None,
            setting_sources: None,
            permission_mode: None,
            include_partial_messages: false,
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
            resume: None,
            effort: None,
            extra_args: Vec::new(),
        }
    }
}

impl ClaudeOptions {
    /// The CLI arguments (after the executable), in the order `SubprocessCLITransport._build_command` makes them.
    /// `can_use_tool` adds `--permission-prompt-tool stdio`, as `ClaudeSDKClient.connect` does.
    pub fn build_args(&self, can_use_tool: bool) -> Result<Vec<String>> {
        let mut args: Vec<String> = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--system-prompt",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(model) = self.model.as_deref().filter(|m| !m.is_empty()) {
            args.extend(["--model".into(), model.into()]);
        }
        if can_use_tool {
            args.extend(["--permission-prompt-tool".into(), "stdio".into()]);
        }
        if let Some(mode) = self.permission_mode.as_deref().filter(|m| !m.is_empty()) {
            args.extend(["--permission-mode".into(), mode.into()]);
        }
        // The equals form binds the value to the flag even when it starts with a dash.
        if let Some(resume) = self.resume.as_deref().filter(|r| !r.is_empty()) {
            reject_windows_cmd_metacharacters("resume", resume)?;
            args.push(format!("--resume={resume}"));
        }
        if self.include_partial_messages {
            args.push("--include-partial-messages".into());
        }
        if let Some(sources) = &self.setting_sources {
            args.push(format!("--setting-sources={}", sources.join(",")));
        }
        for (flag, value) in &self.extra_args {
            match value {
                None => args.push(format!("--{flag}")),
                Some(value) if value.starts_with('-') => args.push(format!("--{flag}={value}")),
                Some(value) => args.extend([format!("--{flag}"), value.clone()]),
            }
        }
        if let Some(effort) = &self.effort {
            args.extend(["--effort".into(), effort.clone()]);
        }
        args.extend(["--input-format".into(), "stream-json".into()]);
        Ok(args)
    }
}

/// Windows runs a .bat/.cmd through cmd.exe, which can run commands hidden in arguments: refuse such a CLI.
fn reject_windows_batch_cli(cli_path: &str) -> Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    let batch = cli_path.replace('\\', "/").split('/').any(|component| {
        component.split(':').any(|segment| {
            let segment = segment.trim_end_matches(['.', ' ']).to_lowercase();
            segment.ends_with(".bat") || segment.ends_with(".cmd")
        })
    });
    if batch {
        return Err(anyhow!(
            "Refusing to execute batch script {cli_path:?}: Windows runs .bat/.cmd files via cmd.exe, which can \
             execute commands injected through CLI arguments. Use a native claude executable instead."
        ));
    }
    Ok(())
}

fn reject_windows_cmd_metacharacters(option: &str, value: &str) -> Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    let mut bad: Vec<char> = value
        .chars()
        .filter(|c| "&|<>^%!\"\r\n".contains(*c))
        .collect();
    bad.sort_unstable();
    bad.dedup();
    if bad.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "{option} value {value:?} contains characters that are unsafe to pass on a Windows command line: {bad:?}"
    ))
}

/// Find the CLI: PATH first, then the places `npm`, `claude install` and friends leave it.
fn find_cli() -> Result<String> {
    if let Ok(path) = which::which("claude") {
        return Ok(path.to_string_lossy().into_owned());
    }
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let mut places: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        places.extend(home.iter().map(|h| h.join(".local/bin/claude.exe")));
    } else {
        places.extend(home.iter().map(|h| h.join(".npm-global/bin/claude")));
        places.push(PathBuf::from("/usr/local/bin/claude"));
        for tail in [
            ".local/bin/claude",
            "node_modules/.bin/claude",
            ".yarn/bin/claude",
            ".claude/local/claude",
        ] {
            places.extend(home.iter().map(|h| h.join(tail)));
        }
    }
    if let Some(found) = places.into_iter().find(|p| p.is_file()) {
        return Ok(found.to_string_lossy().into_owned());
    }
    Err(anyhow!(
        "Claude Code not found. Install with:\n  npm install -g @anthropic-ai/claude-code\n\nIf already installed \
         locally, try:\n  export PATH=\"$HOME/node_modules/.bin:$PATH\""
    ))
}

// ---------------------------------------------------------------------------------------------------------
// Messages

/// One content block of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Plain text.
    Text(String),
    /// Extended thinking.
    Thinking(String),
    /// A tool call.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool's result (`content` is the raw JSON: a string, a list of parts or null).
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: Option<bool>,
    },
    /// A block kind Donut does not use.
    Other,
}

/// What a user message carries.
#[derive(Debug, Clone, PartialEq)]
pub enum UserContent {
    /// A plain string: Claude Code's echo of a command of its own.
    Text(String),
    /// Blocks, such as tool results.
    Blocks(Vec<Block>),
}

/// The `result` message that ends a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultMessage {
    /// `success`, `error_max_turns`, ...
    pub subtype: String,
    /// Whether the turn failed.
    pub is_error: bool,
    /// The Claude Code session id.
    pub session_id: String,
    /// Token counts for the turn (null when absent).
    pub usage: Value,
    /// Claude's own cost figure.
    pub total_cost_usd: Option<f64>,
    /// The final text, or the error text.
    pub result: Option<String>,
}

/// A `rate_limit_event`'s `RateLimitInfo`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RateLimitInfo {
    /// The whole object as the CLI sent it.
    pub raw: Value,
    /// Fraction of the limit used (0.0 - 1.0).
    pub utilization: Option<f64>,
    /// `five_hour`, `seven_day`, ...
    pub rate_limit_type: Option<String>,
    /// Reset time (Unix seconds or an ISO 8601 string).
    pub resets_at: Option<Value>,
}

/// A message from Claude Code, as far as Donut tells kinds apart.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A user message (tool results, or Claude Code's echo of its own command).
    User {
        content: UserContent,
        uuid: Option<String>,
    },
    /// An assistant message.
    Assistant {
        content: Vec<Block>,
        model: String,
        usage: Option<Value>,
        message_id: Option<String>,
        uuid: Option<String>,
    },
    /// Any other `system` message (`init`, hook events, task progress, ...). `data` is the whole object.
    System { subtype: String, data: Value },
    /// A background shell or agent started.
    TaskStarted {
        task_id: String,
        description: String,
        task_type: Option<String>,
    },
    /// A background task changed; `status` is `patch.status`.
    TaskUpdated {
        task_id: String,
        status: Option<String>,
    },
    /// A background task ended.
    TaskNotification {
        task_id: String,
        status: String,
        summary: String,
    },
    /// The end of a turn.
    Result(ResultMessage),
    /// A rate limit change.
    RateLimit(RateLimitInfo),
    /// A message kind outside the system family that Donut ignores (stream events, conversation resets).
    Other,
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `data[key]`, or the SDK's "Missing required field" parse error.
fn required<'a>(data: &'a Value, key: &str, kind: &str) -> Result<&'a Value, String> {
    data.get(key)
        .ok_or_else(|| format!("Missing required field in {kind} message: '{key}'"))
}

fn text_of(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn opt_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn parse_block(block: &Value, kind: &str, assistant: bool) -> Result<Block, String> {
    if !block.is_object() {
        return Err(format!(
            "Invalid content block (expected dict, got {})",
            kind_name(block)
        ));
    }
    let field = |key: &str| required(block, key, kind);
    Ok(match text_of(field("type")?).as_str() {
        "text" => Block::Text(text_of(field("text")?)),
        "thinking" if assistant => {
            let thinking = text_of(field("thinking")?);
            field("signature")?;
            Block::Thinking(thinking)
        }
        "tool_use" => Block::ToolUse {
            id: text_of(field("id")?),
            name: text_of(field("name")?),
            input: field("input")?.clone(),
        },
        "tool_result" => Block::ToolResult {
            tool_use_id: text_of(field("tool_use_id")?),
            content: block.get("content").cloned().unwrap_or(Value::Null),
            is_error: block.get("is_error").and_then(Value::as_bool),
        },
        _ => Block::Other,
    })
}

/// Parse one message from the CLI. `Ok(None)` is a message type this SDK version skips.
pub fn parse_message(data: &Value) -> Result<Option<Message>, String> {
    if !data.is_object() {
        return Err(format!(
            "Invalid message data type (expected dict, got {})",
            kind_name(data)
        ));
    }
    let message_type = match data.get("type") {
        Some(t) if py_truthy(t) => text_of(t),
        _ => return Err("Message missing 'type' field".into()),
    };
    let str_of = |key: &str| opt_string(data.get(key));
    match message_type.as_str() {
        "system" => {
            let subtype = text_of(required(data, "subtype", "system")?);
            let need = |key: &str| required(data, key, "system").map(text_of);
            Ok(Some(match subtype.as_str() {
                "hook_started" | "hook_response" => Message::System {
                    subtype,
                    data: data.clone(),
                },
                "task_started" => {
                    let task_id = need("task_id")?;
                    let description = need("description")?;
                    need("uuid")?;
                    need("session_id")?;
                    Message::TaskStarted {
                        task_id,
                        description,
                        task_type: str_of("task_type"),
                    }
                }
                "task_notification" => {
                    let task_id = need("task_id")?;
                    let status = need("status")?;
                    need("output_file")?;
                    let summary = need("summary")?;
                    need("uuid")?;
                    need("session_id")?;
                    Message::TaskNotification {
                        task_id,
                        status,
                        summary,
                    }
                }
                "task_updated" => {
                    let status = data
                        .get("patch")
                        .and_then(|p| p.get("status"))
                        .and_then(Value::as_str);
                    Message::TaskUpdated {
                        task_id: str_of("task_id").unwrap_or_default(),
                        status: status.map(str::to_string),
                    }
                }
                _ => Message::System {
                    subtype,
                    data: data.clone(),
                },
            }))
        }
        "user" => {
            let message = required(data, "message", "user")?;
            let content = required(message, "content", "user")?;
            let content = match content {
                Value::Array(blocks) => UserContent::Blocks(
                    blocks
                        .iter()
                        .map(|b| parse_block(b, "user", false))
                        .collect::<Result<_, _>>()?,
                ),
                other => UserContent::Text(text_of(other)),
            };
            Ok(Some(Message::User {
                content,
                uuid: str_of("uuid"),
            }))
        }
        "assistant" => {
            let message = required(data, "message", "assistant")?;
            let raw = required(message, "content", "assistant")?;
            let Some(raw) = raw.as_array() else {
                return Err(format!(
                    "Invalid assistant content (expected list, got {})",
                    kind_name(raw)
                ));
            };
            let content = raw
                .iter()
                .map(|b| parse_block(b, "assistant", true))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(Message::Assistant {
                content,
                model: text_of(required(message, "model", "assistant")?),
                usage: message.get("usage").filter(|u| !u.is_null()).cloned(),
                message_id: opt_string(message.get("id")),
                uuid: str_of("uuid"),
            }))
        }
        "result" => {
            let need = |key: &str| required(data, key, "result");
            let subtype = text_of(need("subtype")?);
            need("duration_ms")?;
            need("duration_api_ms")?;
            let is_error = py_truthy(need("is_error")?);
            need("num_turns")?;
            let session_id = text_of(need("session_id")?);
            Ok(Some(Message::Result(ResultMessage {
                subtype,
                is_error,
                session_id,
                usage: data.get("usage").cloned().unwrap_or(Value::Null),
                total_cost_usd: data.get("total_cost_usd").and_then(Value::as_f64),
                result: opt_string(data.get("result")),
            })))
        }
        "stream_event" => {
            for key in ["uuid", "session_id", "event"] {
                required(data, key, "stream_event")?;
            }
            Ok(Some(Message::Other))
        }
        "rate_limit_event" => {
            let info = required(data, "rate_limit_info", "rate_limit_event")?;
            required(info, "status", "rate_limit_event")?;
            required(data, "uuid", "rate_limit_event")?;
            required(data, "session_id", "rate_limit_event")?;
            Ok(Some(Message::RateLimit(RateLimitInfo {
                raw: info.clone(),
                utilization: info.get("utilization").and_then(Value::as_f64),
                rate_limit_type: opt_string(info.get("rateLimitType")),
                resets_at: info.get("resetsAt").filter(|v| !v.is_null()).cloned(),
            })))
        }
        "conversation_reset" => {
            for key in ["new_conversation_id", "uuid", "session_id"] {
                required(data, key, "conversation_reset")?;
            }
            Ok(Some(Message::Other))
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------------------
// The client

/// What the reader thread passes on.
enum Incoming {
    Raw(Value),
    Error(String),
    End,
}

/// The messages of one client, in order. Ends (`None`) when the CLI is gone; a failure is the last item.
pub struct MessageStream {
    receiver: Receiver<Incoming>,
    done: bool,
}

impl MessageStream {
    /// Block for the next message. `Some(Err(text))` is a fatal stream error (bad JSON, a message over the
    /// size limit, a message that does not parse, the CLI exiting with an error); nothing follows it.
    pub fn next(&mut self) -> Option<Result<Message, String>> {
        while !self.done {
            match self.receiver.recv() {
                Ok(Incoming::Raw(data)) => match parse_message(&data) {
                    Ok(Some(message)) => return Some(Ok(message)),
                    Ok(None) => continue,
                    Err(error) => {
                        self.done = true;
                        return Some(Err(error));
                    }
                },
                Ok(Incoming::Error(error)) => {
                    self.done = true;
                    return Some(Err(error));
                }
                Ok(Incoming::End) | Err(_) => self.done = true,
            }
        }
        None
    }
}

type Pending = Sender<Result<Value, String>>;

struct Shared {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    ready: AtomicBool,
    closed: AtomicBool,
    pending: Mutex<HashMap<String, Pending>>,
    inflight: Mutex<HashMap<String, Arc<AtomicBool>>>,
    counter: AtomicU64,
    can_use_tool: Option<ToolApprover>,
}

impl Shared {
    /// Write one line to the CLI's stdin.
    fn write(&self, data: &str) -> Result<(), String> {
        let mut stdin = lock(&self.stdin);
        let Some(pipe) = stdin.as_mut() else {
            return Err("ProcessTransport is not ready for writing".into());
        };
        if !self.ready.load(Ordering::SeqCst) {
            return Err("ProcessTransport is not ready for writing".into());
        }
        if let Ok(Some(status)) = lock(&self.child).try_wait() {
            return Err(format!(
                "Cannot write to terminated process (exit code: {})",
                status.code().unwrap_or(-1)
            ));
        }
        pipe.write_all(data.as_bytes())
            .and_then(|()| pipe.flush())
            .map_err(|error| {
                self.ready.store(false, Ordering::SeqCst);
                format!("Failed to write to process stdin: {error}")
            })
    }

    fn control_request(&self, request: Value, timeout: Duration) -> Result<Value, String> {
        let number = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let request_id = format!(
            "req_{number}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let (sender, receiver) = mpsc::channel();
        lock(&self.pending).insert(request_id.clone(), sender);
        let line = json!({"type": "control_request", "request_id": request_id, "request": request});
        if let Err(error) = self.write(&format!("{line}\n")) {
            lock(&self.pending).remove(&request_id);
            return Err(error);
        }
        let outcome = receiver.recv_timeout(timeout);
        lock(&self.pending).remove(&request_id);
        match outcome {
            Ok(Ok(response)) => Ok(response
                .get("response")
                .filter(|r| r.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}))),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(format!(
                "Control request timeout: {}",
                request.get("subtype").map(text_of).unwrap_or_default()
            )),
        }
    }

    fn fail_pending(&self, error: &str) {
        for (_, sender) in lock(&self.pending).drain() {
            let _ = sender.send(Err(error.to_string()));
        }
    }

    fn handle_control_request(&self, request: &Value) {
        let request_id = request.get("request_id").map(text_of).unwrap_or_default();
        let cancelled = Arc::new(AtomicBool::new(false));
        lock(&self.inflight).insert(request_id.clone(), Arc::clone(&cancelled));
        let outcome = self.answer_control_request(request);
        lock(&self.inflight).remove(&request_id);
        if cancelled.load(Ordering::SeqCst) {
            return; // The CLI abandoned this request; it wants no answer.
        }
        let response = match outcome {
            Ok(data) => json!({"subtype": "success", "request_id": request_id, "response": data}),
            Err(error) => json!({"subtype": "error", "request_id": request_id, "error": error}),
        };
        let _ = self.write(&format!(
            "{}\n",
            json!({"type": "control_response", "response": response})
        ));
    }

    fn answer_control_request(&self, request: &Value) -> Result<Value, String> {
        let data = request.get("request").ok_or("'request'")?;
        let subtype = data.get("subtype").map(text_of).ok_or("'subtype'")?;
        if subtype != "can_use_tool" {
            return Err(format!("Unsupported control request subtype: {subtype}"));
        }
        let callback = self
            .can_use_tool
            .as_ref()
            .ok_or("canUseTool callback is not provided")?;
        let name = text_of(data.get("tool_name").ok_or("'tool_name'")?);
        let input = data.get("input").ok_or("'input'")?;
        Ok(match callback(&name, input) {
            PermissionResult::Allow { updated_input } => {
                json!({"behavior": "allow", "updatedInput": updated_input})
            }
            PermissionResult::Deny { message } => json!({"behavior": "deny", "message": message}),
        })
    }
}

/// The reader thread: frame lines, route control traffic, pass the rest on.
fn read_loop(
    shared: Arc<Shared>,
    stdout: ChildStdout,
    sender: Sender<Incoming>,
    max_buffer: usize,
) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut last_error_result: Option<Value> = None;
    let failure: Option<String> = loop {
        let line = match read_line_limited(&mut reader, max_buffer) {
            Ok(Some(line)) => line,
            Ok(None) => break None,
            Err(error) if error.kind() == ErrorKind::InvalidData => {
                break Some(format!(
                    "Failed to decode JSON: JSON message exceeded maximum buffer size of {max_buffer} bytes..."
                ));
            }
            Err(_) => break None,
        };
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if !text.starts_with('{') {
            continue; // Blank, or non-JSON output some CLI builds write to stdout.
        }
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            break Some(format!(
                "Failed to decode JSON: {}...",
                text.chars().take(100).collect::<String>()
            ));
        };
        if shared.closed.load(Ordering::SeqCst) {
            break None;
        }
        let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "control_response" => {
                let response = message
                    .get("response")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let id = response.get("request_id").map(text_of).unwrap_or_default();
                if let Some(waiter) = lock(&shared.pending).remove(&id) {
                    let _ = waiter.send(
                        if response.get("subtype").and_then(Value::as_str) == Some("error") {
                            Err(response
                                .get("error")
                                .map(text_of)
                                .unwrap_or_else(|| "Unknown error".into()))
                        } else {
                            Ok(response)
                        },
                    );
                }
                continue;
            }
            "control_request" => {
                let shared = Arc::clone(&shared);
                thread::spawn(move || shared.handle_control_request(&message));
                continue;
            }
            "control_cancel_request" => {
                if let Some(flag) = message
                    .get("request_id")
                    .map(text_of)
                    .and_then(|id| lock(&shared.inflight).remove(&id))
                {
                    flag.store(true, Ordering::SeqCst);
                }
                continue;
            }
            "transcript_mirror" => continue,
            // Frames the CLI sends only because the transport asked for them.
            "system"
                if message.get("subtype").and_then(Value::as_str)
                    == Some("session_state_changed")
                    && message.get("sdk_host_only") == Some(&Value::Bool(true)) =>
            {
                continue;
            }
            _ => {}
        }
        if kind == "result" {
            last_error_result = message
                .get("is_error")
                .is_some_and(py_truthy)
                .then(|| message.clone());
        } else if !(kind == "system"
            && message.get("subtype").and_then(Value::as_str) == Some("session_state_changed"))
        {
            last_error_result = None;
        }
        if sender.send(Incoming::Raw(message)).is_err() {
            break None;
        }
    };
    // The CLI exits non-zero on purpose after an error result; report that result, not the bare exit code.
    let failure = failure.or_else(|| {
        let status = wait_exit(&shared.child, Duration::from_secs(5))?;
        let code = status.code().unwrap_or(-1);
        if code == 0 {
            return None;
        }
        Some(match &last_error_result {
            Some(result) => format!("Claude Code returned an error result: {} (exit code: {code})", error_result_text(result)),
            None => format!("Command failed with exit code {code} (exit code: {code})\nError output: Check stderr output for details"),
        })
    });
    shared.ready.store(false, Ordering::SeqCst);
    shared.fail_pending(failure.as_deref().unwrap_or("Claude Code process exited"));
    if let Some(error) = failure {
        let _ = sender.send(Incoming::Error(error));
    }
    let _ = sender.send(Incoming::End);
}

/// The most informative text of a `result` frame with `is_error` (`_error_result_text`).
fn error_result_text(message: &Value) -> String {
    let errors: Vec<String> = match message.get("errors") {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let errors: Vec<&str> = errors
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .collect();
    if !errors.is_empty() {
        return errors.join("; ");
    }
    if let Some(result) = message
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        return result.to_string();
    }
    if let Some(subtype) = message
        .get("subtype")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && *s != "success")
    {
        return subtype.to_string();
    }
    match message.get("api_error_status") {
        Some(status) if !status.is_null() => format!("API error (HTTP {})", text_of(status)),
        _ => "unknown error".into(),
    }
}

/// A live `claude` CLI process in streaming mode. Every method takes `&self` and may be called from any
/// thread; writes to the CLI are serialized.
pub struct ClaudeClient {
    shared: Arc<Shared>,
    init: Value,
}

impl ClaudeClient {
    /// Start the CLI and run the `initialize` handshake. The returned stream is the only reader of messages.
    pub fn connect(
        options: &ClaudeOptions,
        can_use_tool: Option<ToolApprover>,
    ) -> Result<(ClaudeClient, MessageStream)> {
        let cli = match options.cli_path.clone() {
            Some(path) => path,
            None => find_cli()?,
        };
        reject_windows_batch_cli(&cli)?;
        let args = options.build_args(can_use_tool.is_some())?;
        let mut command = Command::new(&cli);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        // CLAUDECODE would make the child think it runs inside a Claude Code parent.
        command
            .env_remove("CLAUDECODE")
            .env_remove(DERIVED_CLIENT_APP_ENV);
        command.env("CLAUDE_CODE_ENTRYPOINT", "sdk-py");
        command.envs(options.env.iter().map(|(k, v)| (k, v)));
        command.env("CLAUDE_AGENT_SDK_VERSION", SDK_VERSION);
        let state_set = std::env::vars()
            .map(|(k, _)| k)
            .chain(options.env.iter().map(|(k, _)| k.clone()));
        if !state_set
            .into_iter()
            .any(|k| k.eq_ignore_ascii_case(READS_SESSION_STATE_ENV))
        {
            command.env(READS_SESSION_STATE_ENV, "1");
        }
        if let Some(cwd) = &options.cwd {
            command.env("PWD", cwd);
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                match &options.cwd {
                    Some(cwd) if !cwd.exists() => {
                        anyhow!("Working directory does not exist: {}", cwd.display())
                    }
                    _ => anyhow!("Claude Code not found at: {cli}"),
                }
            } else {
                anyhow!("Failed to start Claude Code: {error}")
            }
        })?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to start Claude Code: no stdout"))?;
        let shared = Arc::new(Shared {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            ready: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            can_use_tool,
        });
        let (sender, receiver) = mpsc::channel();
        let max_buffer = options.max_buffer_size;
        let reader_shared = Arc::clone(&shared);
        thread::spawn(move || read_loop(reader_shared, stdout, sender, max_buffer));
        let mut client = ClaudeClient {
            shared,
            init: Value::Null,
        };
        let timeout_ms: u64 = std::env::var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60_000);
        let timeout = Duration::from_millis(timeout_ms).max(Duration::from_secs(60));
        match client
            .shared
            .control_request(json!({"subtype": "initialize", "hooks": null}), timeout)
        {
            Ok(init) => client.init = init,
            Err(error) => {
                client.disconnect();
                return Err(anyhow!(error));
            }
        }
        Ok((
            client,
            MessageStream {
                receiver,
                done: false,
            },
        ))
    }

    /// The `initialize` response (commands, models, output styles, ...).
    pub fn server_info(&self) -> &Value {
        &self.init
    }

    /// Send a user prompt. Replies arrive on the message stream.
    pub fn query(&self, prompt: &str) -> Result<()> {
        let message = json!({
            "type": "user",
            "message": {"role": "user", "content": prompt},
            "parent_tool_use_id": null,
            "session_id": "default",
        });
        self.shared
            .write(&format!("{message}\n"))
            .map_err(|e| anyhow!(e))
    }

    /// Send any control request and wait for its response object (60 s limit).
    pub fn control_request(&self, request: Value) -> Result<Value> {
        self.control_request_within(request, CONTROL_TIMEOUT)
    }

    /// Like [`control_request`](Self::control_request) with another time limit.
    pub fn control_request_within(&self, request: Value, timeout: Duration) -> Result<Value> {
        self.shared
            .control_request(request, timeout)
            .map_err(|e| anyhow!(e))
    }

    /// Ask Claude Code to stop the running turn.
    pub fn interrupt(&self) -> Result<()> {
        self.control_request(json!({"subtype": "interrupt"}))
            .map(|_| ())
    }

    /// Switch Claude's permission mode (`default`, `acceptEdits`, `auto`, `bypassPermissions`, `plan`).
    pub fn set_permission_mode(&self, mode: &str) -> Result<()> {
        self.control_request(json!({"subtype": "set_permission_mode", "mode": mode}))
            .map(|_| ())
    }

    /// Switch the model; `None` returns to the default.
    pub fn set_model(&self, model: Option<&str>) -> Result<()> {
        self.control_request(json!({"subtype": "set_model", "model": model}))
            .map(|_| ())
    }

    /// Close stdin, give the CLI 5 s to save its session, then terminate (5 s) and kill it.
    pub fn disconnect(&self) {
        let shared = &self.shared;
        shared.closed.store(true, Ordering::SeqCst);
        shared.ready.store(false, Ordering::SeqCst);
        lock(&shared.stdin).take();
        if wait_exit(&shared.child, Duration::from_secs(5)).is_none() {
            let pid = lock(&shared.child).id();
            terminate_pid(pid);
            if wait_exit(&shared.child, Duration::from_secs(5)).is_none() {
                kill_pid(pid);
                wait_exit(&shared.child, Duration::from_secs(5));
            }
        }
        shared.fail_pending("Claude Code process closed");
    }
}
