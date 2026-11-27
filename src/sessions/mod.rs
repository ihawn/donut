//! persistent provider sessions for the interactive terminal.
//!
//! One session per provider conversation: [`CodexSession`] (the `codex app-server` JSON-RPC over stdio),
//! [`ClaudeSession`] (the `claude` CLI in stream-json mode, through the private `claude_sdk` module, a Rust
//! re-implementation of the Python `claude-agent-sdk` client) and [`QwenSession`] (one `qwen` process per
//! turn). The terminal builds one with [`new_session`] and drives it through the [`Session`] trait.
//!
//! Threading: every method takes `&self`, so a session lives in an `Arc<dyn Session>` and is shared between a
//! worker thread that runs [`Session::query`] (it blocks until the turn ends) and the UI thread, which may call
//! [`Session::interrupt`] at any time. The `emit` callback is called from whichever thread has news (the thread
//! in `query`, a reader thread, or the Claude wake-up pump); the `approve` callback is called on a thread of
//! its own and may block as long as the user takes. Both must be cheap to call from several threads at once.
//!
//! Like Python's single owner task, the Claude operations (`query`, `set_permission`, `switch_model`,
//! `set_effort`, `models`, `account_usage`, `close`) run one at a time: while a turn runs, the others wait for
//! it. `interrupt` and `quota` do not queue behind it (`quota` waits at most 30 s, then answers `None`).

mod claude_sdk;

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

use crate::providers::{NativePermission, native_permission};
use crate::ui::release_version;
use claude_sdk::{
    Block, ClaudeClient, ClaudeOptions, Message, MessageStream, PermissionResult, ResultMessage,
    ToolApprover, UserContent,
};
pub use claude_sdk::{RateLimitInfo, SDK_VERSION as CLAUDE_SDK_VERSION};

/// The longest line any provider process may write, as in Python (`limit=32 * 1024 * 1024`).
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------------------
// Small shared helpers

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Python truthiness of a JSON value (`x or y`).
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str(value)` for text a provider sends: strings as they are, null as empty, the rest as JSON.
fn display_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The first truthy value of `keys` in `object`, like `a.get(x) or a.get(y) or ""`.
fn first_truthy<'a>(object: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| object.get(*key))
        .find(|v| py_truthy(v))
}

/// A string profile value that is set (`profile.get(key) or None`).
fn profile_str(profile: &Value, key: &str) -> Option<String> {
    profile
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `dict(profile, key=value)`.
fn with_key(profile: &Value, key: &str, value: Value) -> Value {
    let mut map: Map<String, Value> = profile.as_object().cloned().unwrap_or_default();
    map.insert(key.to_string(), value);
    Value::Object(map)
}

/// Read one line (with its newline) of at most `limit` bytes; `None` at end of input. A longer line is an
/// `InvalidData` error, as asyncio's `StreamReader` limit is a `ValueError`.
fn read_line_limited<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let (done, used) = {
            let available = match reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if available.is_empty() {
                return Ok(if line.is_empty() { None } else { Some(line) });
            }
            match available.iter().position(|b| *b == b'\n') {
                Some(index) => {
                    line.extend_from_slice(&available[..=index]);
                    (true, index + 1)
                }
                None => {
                    line.extend_from_slice(available);
                    (false, available.len())
                }
            }
        };
        reader.consume(used);
        if line.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line exceeded {limit} bytes"),
            ));
        }
        if done {
            return Ok(Some(line));
        }
    }
}

/// Ask a process to end (SIGTERM; `taskkill` on Windows).
fn terminate_pid(pid: u32) {
    #[cfg(unix)]
    // SAFETY: kill(2) with a pid and a signal number has no memory-safety requirements.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .output();
    }
}

/// End a process for certain (SIGKILL; `taskkill /F` on Windows).
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    // SAFETY: kill(2) with a pid and a signal number has no memory-safety requirements.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
}

/// Wait up to `timeout` for the process to exit; `None` when it is still running.
fn wait_exit(child: &Mutex<Child>, timeout: Duration) -> Option<ExitStatus> {
    let end = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = lock(child).try_wait() {
            return Some(status);
        }
        if Instant::now() >= end {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Like `os.path.expanduser`: a leading `~` is the home directory.
fn expanduser(path: &str) -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

// ---------------------------------------------------------------------------------------------------------
// Usage and price helpers

/// One usage bar entry: `{"window", "percent", "resets_at"}`.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// `5h`, `7d`, `7d opus`, ...
    pub window: String,
    /// Percent used, 0-100.
    pub percent: f64,
    /// Unix seconds when the window resets, if known.
    pub resets_at: Option<f64>,
}

impl UsageWindow {
    /// The JSON the `usage` event carries.
    pub fn to_value(&self) -> Value {
        json!({"window": self.window, "percent": self.percent, "resets_at": self.resets_at})
    }
}

fn usage_value(windows: &[UsageWindow]) -> Value {
    Value::Array(windows.iter().map(UsageWindow::to_value).collect())
}

/// Reset times arrive as Unix seconds (Claude events, Codex) or ISO 8601 strings (Claude get_usage).
pub fn epoch_seconds(value: &Value) -> Option<f64> {
    use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
    let stamp = |seconds: i64, nanos: u32| seconds as f64 + f64::from(nanos) / 1e9;
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(text) if !text.is_empty() => {
            let text = text.replace('Z', "+00:00");
            for candidate in [text.clone(), text.replacen(' ', "T", 1)] {
                if let Ok(time) = DateTime::parse_from_rfc3339(&candidate) {
                    return Some(stamp(time.timestamp(), time.timestamp_subsec_nanos()));
                }
            }
            // No UTC offset: Python reads it as local time.
            let naive = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
                .iter()
                .find_map(|format| NaiveDateTime::parse_from_str(&text, format).ok())
                .or_else(|| {
                    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
                        .ok()
                        .and_then(|d| d.and_hms_opt(0, 0, 0))
                })?;
            let time = Local.from_local_datetime(&naive).earliest()?;
            Some(stamp(time.timestamp(), time.timestamp_subsec_nanos()))
        }
        _ => None,
    }
}

/// 300 -> "5h", 10080 -> "7d".
pub fn window_label(minutes: Option<f64>) -> String {
    match minutes {
        Some(m) if m != 0.0 => {
            if m >= 1440.0 {
                format!("{}d", (m / 1440.0).round_ties_even() as i64)
            } else {
                format!("{}h", (m / 60.0).round_ties_even() as i64)
            }
        }
        _ => String::new(),
    }
}

/// Usage windows from a Codex RateLimitSnapshot.
pub fn codex_usage(snapshot: &Value) -> Vec<UsageWindow> {
    ["primary", "secondary"]
        .iter()
        .filter_map(|key| {
            let window = snapshot.get(*key)?;
            let used = window.get("usedPercent")?.as_f64()?;
            Some(UsageWindow {
                window: window_label(window.get("windowDurationMins").and_then(Value::as_f64)),
                percent: used,
                resets_at: window.get("resetsAt").and_then(epoch_seconds),
            })
        })
        .collect()
}

const CLAUDE_WINDOWS: [(&str, &str); 4] = [
    ("five_hour", "5h"),
    ("seven_day", "7d"),
    ("seven_day_opus", "7d opus"),
    ("seven_day_sonnet", "7d sonnet"),
];

fn claude_window_name(key: &str) -> String {
    CLAUDE_WINDOWS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(key, |(_, name)| name)
        .to_string()
}

/// Dollars per million tokens (input, output, cache read) by family, to estimate a turn's cost while it runs.
/// Claude's own figure replaces the estimate when the turn ends. A cache write costs 1.25 times the input price.
pub const CLAUDE_PRICES: [(&str, (f64, f64, f64)); 4] = [
    ("fable", (8.0, 40.0, 0.2)),
    ("opus", (4.0, 20.0, 0.2)),
    ("sonnet", (2.0, 10.0, 0.1)),
    ("haiku", (0.1, 0.5, 0.01)),
];

/// Estimated dollars for one Claude response from its token counts; 0 for a model with no known price.
pub fn claude_cost(model: Option<&str>, usage: &Value) -> f64 {
    let model = model.unwrap_or("").to_lowercase();
    let Some((_, prices)) = CLAUDE_PRICES
        .iter()
        .find(|(family, _)| model.contains(family))
    else {
        return 0.0;
    };
    if !usage.is_object() {
        return 0.0;
    }
    let count = |key: &str| {
        usage
            .get(key)
            .filter(|v| py_truthy(v))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .trunc()
    };
    (count("input_tokens") * prices.0
        + count("cache_creation_input_tokens") * prices.0 * 1.25
        + count("output_tokens") * prices.1
        + count("cache_read_input_tokens") * prices.2)
        / 1_000_000.0
}

/// Usage windows from a Claude RateLimitInfo. `unifiedWindows` carries every window when present.
pub fn claude_usage(info: &RateLimitInfo) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    if let Some(unified) = info.raw.get("unifiedWindows").and_then(Value::as_object) {
        for (key, window) in unified {
            if let Some(utilization) = window.get("utilization").and_then(Value::as_f64) {
                windows.push(UsageWindow {
                    window: claude_window_name(key),
                    percent: utilization * 100.0,
                    resets_at: window.get("resetsAt").and_then(epoch_seconds),
                });
            }
        }
    }
    if windows.is_empty() {
        if let Some(utilization) = info.utilization {
            windows.push(UsageWindow {
                window: info
                    .rate_limit_type
                    .as_deref()
                    .map(claude_window_name)
                    .unwrap_or_default(),
                percent: utilization * 100.0,
                resets_at: info.resets_at.as_ref().and_then(epoch_seconds),
            });
        }
    }
    windows
}

/// Windows from the CLI's get_usage control request, the data behind Claude Code's /usage.
///
/// Returns an empty list when plan limits do not apply (API key, Bedrock, Vertex) and `None` when the answer is
/// unusable. Utilization there is a percentage, 0-100.
pub fn claude_plan_usage(result: &Value) -> Option<Vec<UsageWindow>> {
    if !result.is_object() {
        return None;
    }
    if result.get("rate_limits_available") == Some(&Value::Bool(false)) {
        return Some(Vec::new());
    }
    let limits = result.get("rate_limits").filter(|l| l.is_object())?;
    Some(
        CLAUDE_WINDOWS
            .iter()
            .filter_map(|(key, name)| {
                let window = limits.get(*key)?;
                Some(UsageWindow {
                    window: name.to_string(),
                    percent: window.get("utilization")?.as_f64()?,
                    resets_at: window.get("resets_at").and_then(epoch_seconds),
                })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------------------------------------
// The session interface

/// The terminal's answer to an approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow this once.
    Yes,
    /// Allow for the rest of the session.
    Always,
    /// Deny.
    No,
}

impl Decision {
    /// The Python answers: `"yes"` and `"always"`; anything else denies.
    pub fn parse(answer: &str) -> Decision {
        match answer {
            "yes" => Decision::Yes,
            "always" => Decision::Always,
            _ => Decision::No,
        }
    }
}

/// Called as `emit(kind, data)` with the kinds of the Python sessions: `text`, `thinking`, `break`, `activity`,
/// `tool`, `tool_result`, `command`, `command_output`, `usage`, `spend`, `task`, `background`, `wake_start`,
/// `wake_end`, `error`.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// Called as `approve(name, details)` when the provider wants permission: a tool name and its input for
/// Claude, the JSON-RPC method and its params for Codex. Blocks until the user answers.
pub type Approve = Arc<dyn Fn(&str, &Value) -> Decision + Send + Sync>;

/// One question a model asks the user: Claude's AskUserQuestion or Codex's request_user_input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Question {
    /// The key the answer goes back under: Codex's question id, Claude's question text.
    pub id: String,
    pub header: String,
    pub question: String,
    /// (label, description) pairs; empty when only a typed answer fits.
    pub options: Vec<(String, String)>,
    /// Several options may be chosen.
    pub multi: bool,
    /// A typed answer ("Something else") is allowed.
    pub other: bool,
    /// Keep the answer off the screen.
    pub secret: bool,
}

/// Answers keyed by question id, in the order asked.
pub type Answers = Vec<(String, Vec<String>)>;

/// Shows the model's questions and returns the answers, or `None` when the user dismissed them. Blocks.
#[derive(Clone)]
pub struct Ask(pub AskFn);

pub type AskFn = Arc<dyn Fn(&[Question]) -> Option<Answers> + Send + Sync>;

impl std::fmt::Debug for Ask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ask")
    }
}

fn option_pair(option: &Value) -> Option<(String, String)> {
    let label = match option {
        Value::String(label) => label.clone(),
        _ => option.get("label")?.as_str()?.to_string(),
    };
    let description = option
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    (!label.is_empty()).then(|| (label, description.to_string()))
}

fn options_of(question: &Value) -> Vec<(String, String)> {
    question
        .get("options")
        .and_then(Value::as_array)
        .map(|options| options.iter().filter_map(option_pair).collect())
        .unwrap_or_default()
}

/// Codex's `item/tool/requestUserInput` params as questions.
pub fn codex_questions(params: &Value) -> Vec<Question> {
    let text = |q: &Value, key: &str| q.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    params
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .map(|q| {
                    let options = options_of(q);
                    Question {
                        id: text(q, "id"),
                        header: text(q, "header"),
                        question: text(q, "question"),
                        multi: false,
                        other: q.get("isOther").and_then(Value::as_bool).unwrap_or(false)
                            || options.is_empty(),
                        secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
                        options,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Claude's AskUserQuestion input as questions. Claude Code keys each answer by the question text.
pub fn claude_questions(input: &Value) -> Vec<Question> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|q| {
                    let question = q.get("question")?.as_str()?.to_string();
                    Some(Question {
                        id: question.clone(),
                        header: q
                            .get("header")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        options: options_of(q),
                        multi: q
                            .get("multiSelect")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        other: true, // Claude Code always offers a typed answer ("Other").
                        secret: false,
                        question,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The permission answer for AskUserQuestion: the tool's input with the answers added, or a denial.
pub fn claude_answer(input: &Value, ask: Option<&Ask>) -> PermissionResult {
    let questions = claude_questions(input);
    match ask.and_then(|ask| (ask.0)(&questions)) {
        Some(answers) => {
            let mut updated = input.clone();
            let joined: serde_json::Map<String, Value> = answers
                .into_iter()
                .map(|(id, chosen)| (id, json!(chosen.join(", "))))
                .collect();
            if let Some(object) = updated.as_object_mut() {
                object.insert("answers".into(), Value::Object(joined));
            }
            PermissionResult::Allow {
                updated_input: updated,
            }
        }
        None => PermissionResult::Deny {
            message: "The user dismissed the question without answering.".into(),
        },
    }
}

/// What Python set on a session before `connect()`.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Donut permission mode: `ask`, `accept_edits`, `auto` or `full`.
    pub permission: String,
    /// Review mode: the session may read and run checks, never write.
    pub read_only: bool,
    /// One question, no tools: keep settings, plugins and connectors out of the prompt (Claude).
    pub bare: bool,
    /// Turn Claude in Chrome on (Claude).
    pub chrome: bool,
    /// The provider's session or thread id to reopen.
    pub resume_id: Option<String>,
    /// Answers the model's questions to the user; without it they are dismissed.
    pub ask: Option<Ask>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            permission: "ask".into(),
            read_only: false,
            bare: false,
            chrome: false,
            resume_id: None,
            ask: None,
        }
    }
}

/// A model a provider offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// The id to pass back to `switch_model` or a profile.
    pub id: String,
    /// The display name.
    pub name: String,
}

/// A persistent provider session. See the module docs for the threading rules.
pub trait Session: Send + Sync {
    /// The provider family: `claude`, `codex` or `qwen`.
    fn family(&self) -> &'static str;
    /// Start the provider process (and its handshake). Call once, before `query`.
    fn connect(&self) -> Result<()>;
    /// Send one turn and block until it ends. Text and tools arrive through `emit`. An error means the turn
    /// failed (the provider said so, or its process went away).
    fn query(&self, text: &str) -> Result<()>;
    /// Stop the running turn. Safe to call from another thread while `query` blocks.
    fn interrupt(&self) -> Result<()>;
    /// End the provider process. Idempotent; a never-connected session closes without error.
    fn close(&self) -> Result<()>;
    /// Use a Donut permission mode (`ask`, `accept_edits`, `auto`, `full`) from now on.
    fn set_permission(&self, mode: &str) -> Result<()>;
    /// Plan usage windows without sending a prompt, or `None` when the provider cannot say.
    fn quota(&self) -> Option<Vec<UsageWindow>>;
    /// The models the provider offers.
    fn models(&self) -> Result<Vec<ModelInfo>>;
    /// A raw provider request: a Claude SDK control request (`{"subtype": method, ..params}`) or a Codex
    /// JSON-RPC call. Returns the provider's answer.
    fn control(&self, method: &str, params: Value) -> Result<Value> {
        let _ = (method, params);
        Err(anyhow!("this provider takes no raw requests"))
    }
    /// Account or session usage as the provider reports it.
    fn account_usage(&self) -> Result<Value>;
    /// The provider's own id for this conversation (what `resume_id` reopens), once it has one.
    fn provider_id(&self) -> Option<String>;
    /// Usage of the last turn as the provider reported it (`Null` before any).
    fn usage(&self) -> Value;
    /// The current Donut permission mode.
    fn permission(&self) -> String;
    /// The profile this session runs with.
    fn profile(&self) -> Value;
    /// Replace the profile (Donut does this after an in-place model switch).
    fn set_profile(&self, profile: Value);
    /// Change the model of the live conversation (Claude, Codex); the provider keeps its context.
    fn switch_model(&self, _model: Option<&str>) -> Result<()> {
        Err(anyhow!("this provider cannot switch models in place"))
    }
    /// Change the reasoning effort of the live conversation (Claude, Codex).
    fn set_effort(&self, _level: &str) -> Result<()> {
        Err(anyhow!("this provider cannot change effort in place"))
    }
    /// Codex's own model list, with each model's reasoning efforts and descriptions.
    fn model_details(&self) -> Result<Vec<Value>> {
        Err(anyhow!("this provider has no model details"))
    }
    /// What the provider told at startup: for Claude the `initialize` response (`commands`, `models`), else `{}`.
    fn info(&self) -> Value {
        json!({})
    }
    /// Claude's `apiKeySource` from its `init` message: `"none"` is a plan login, anything else an API key.
    fn account(&self) -> Option<String> {
        None
    }
}

/// The session for an engine (`claude`, `codex`, `ollama`, `openai`), or `None` when Donut has none
/// (`terminal.session_class`).
pub fn new_session(
    engine: &str,
    project: &Path,
    profile: Value,
    options: SessionOptions,
    emit: Emit,
    approve: Approve,
) -> Option<Arc<dyn Session>> {
    match engine {
        "claude" => Some(Arc::new(ClaudeSession::new(
            project, profile, options, emit, approve,
        ))),
        "codex" => Some(Arc::new(CodexSession::new(
            project, profile, options, emit, approve,
        ))),
        "ollama" | "openai" => Some(Arc::new(QwenSession::new(project, profile, options, emit))),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------
// Codex

/// Codex item types that are tool calls, and the name Donut shows for each.
pub const CODEX_TOOLS: [(&str, &str); 5] = [
    ("commandExecution", "Bash"),
    ("fileChange", "Edit"),
    ("mcpToolCall", "MCP"),
    ("webSearch", "WebSearch"),
    ("dynamicToolCall", "Tool"),
];

fn codex_tool(kind: &str) -> Option<&'static str> {
    CODEX_TOOLS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, name)| *name)
}

fn codex_decision(decision: Decision) -> &'static str {
    match decision {
        Decision::Yes => "accept",
        Decision::Always => "acceptForSession",
        Decision::No => "decline",
    }
}

fn codex_sandbox(sandbox: &str) -> &'static str {
    match sandbox {
        "read-only" => "readOnly",
        "workspace-write" => "workspaceWrite",
        _ => "dangerFullAccess",
    }
}

type TurnDone = Sender<Result<(), String>>;
type Reply = Sender<Result<Value, String>>;

#[derive(Default)]
struct CodexState {
    profile: Value,
    permission: String,
    thread: Option<String>,
    permission_changed: bool,
    model_changed: bool,
    effort_changed: bool,
    effort_sent: bool,
    wrote: bool,
    usage: Value,
    read_error: Option<String>,
    reader_done: bool,
    turn_done: Option<TurnDone>,
}

#[cfg(test)]
type RequestHook = Box<dyn Fn(&CodexInner, &str, &Value) -> Result<Value> + Send + Sync>;

struct CodexInner {
    project: PathBuf,
    emit: Emit,
    approve: Approve,
    ask: Option<Ask>,
    read_only: bool,
    resume_id: Option<String>,
    program: Mutex<Option<PathBuf>>,
    state: Mutex<CodexState>,
    process: Mutex<Option<Arc<Mutex<Child>>>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<i64, Reply>>,
    serial: AtomicI64,
    reader: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    hook: Mutex<Option<RequestHook>>,
}

/// A Codex conversation: one `codex app-server --stdio` process and one thread in it.
pub struct CodexSession {
    inner: Arc<CodexInner>,
}

impl CodexSession {
    /// A session that is not connected yet. `approve` answers Codex's command and file-change approvals.
    pub fn new(
        project: &Path,
        profile: Value,
        options: SessionOptions,
        emit: Emit,
        approve: Approve,
    ) -> CodexSession {
        let state = CodexState {
            profile,
            permission: options.permission,
            reader_done: true,
            usage: Value::Null,
            ..CodexState::default()
        };
        CodexSession {
            inner: Arc::new(CodexInner {
                project: project.to_path_buf(),
                emit,
                approve,
                ask: options.ask,
                read_only: options.read_only,
                resume_id: options.resume_id,
                program: Mutex::new(None),
                state: Mutex::new(state),
                process: Mutex::new(None),
                stdin: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                serial: AtomicI64::new(0),
                reader: Mutex::new(None),
                #[cfg(test)]
                hook: Mutex::new(None),
            }),
        }
    }

    /// Run this executable instead of `codex` from the PATH.
    pub fn set_program(&self, program: impl Into<PathBuf>) {
        *lock(&self.inner.program) = Some(program.into());
    }
}

impl CodexInner {
    fn emit(&self, kind: &str, data: Value) {
        (self.emit)(kind, data);
    }

    /// The (approvalPolicy, sandbox, approvalsReviewer) for the current permission.
    fn policy(&self) -> (String, String, String) {
        if self.read_only {
            // Review mode: it may read and run checks, never write.
            return ("on-request".into(), "read-only".into(), "user".into());
        }
        let permission = lock(&self.state).permission.clone();
        match native_permission("codex", &permission) {
            NativePermission::Codex {
                approval,
                sandbox,
                reviewer,
            } => (approval.into(), sandbox.into(), reviewer.into()),
            NativePermission::Mode(_) => unreachable!("codex permissions are Codex policies"),
        }
    }

    fn send(&self, message: &Value) -> Result<()> {
        let mut stdin = lock(&self.stdin);
        let pipe = stdin
            .as_mut()
            .ok_or_else(|| anyhow!("Codex session is closed"))?;
        pipe.write_all(format!("{message}\n").as_bytes())?;
        pipe.flush()?;
        Ok(())
    }

    /// One JSON-RPC request; waits up to 90 s for the answer.
    fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        #[cfg(test)]
        if let Some(hook) = lock(&self.hook).as_ref() {
            return hook(self, method, &params.unwrap_or_else(|| json!({})));
        }
        let closed = |state: &CodexState| {
            anyhow!(
                state
                    .read_error
                    .clone()
                    .unwrap_or_else(|| "Codex session is closed".into())
            )
        };
        {
            let state = lock(&self.state);
            if state.reader_done {
                return Err(closed(&state));
            }
        }
        let identity = self.serial.fetch_add(1, Ordering::SeqCst) + 1;
        let (reply, answer) = mpsc::channel();
        lock(&self.pending).insert(identity, reply);
        let outcome = (|| {
            {
                let state = lock(&self.state);
                if state.reader_done {
                    return Err(closed(&state));
                }
            }
            let params = params.filter(py_truthy).unwrap_or_else(|| json!({}));
            self.send(&json!({"id": identity, "method": method, "params": params}))?;
            match answer.recv_timeout(Duration::from_secs(90)) {
                Ok(result) => result.map_err(|e| anyhow!(e)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    Err(anyhow!("Codex request timed out: {method}"))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(anyhow!("Codex session disconnected"))
                }
            }
        })();
        lock(&self.pending).remove(&identity);
        outcome
    }

    /// End the running turn successfully (the reader does this on `turn/completed`).
    fn complete_turn(&self, outcome: Result<(), String>) {
        if let Some(done) = lock(&self.state).turn_done.take() {
            let _ = done.send(outcome);
        }
    }

    fn read(self: &Arc<Self>, stdout: std::process::ChildStdout) {
        let mut reader = BufReader::with_capacity(64 * 1024, stdout);
        let error = loop {
            match read_line_limited(&mut reader, MAX_MESSAGE_BYTES) {
                Ok(Some(line)) => {
                    if let Ok(event) = serde_json::from_slice::<Value>(&line) {
                        self.handle_event(event);
                    }
                }
                Ok(None) => break None,
                Err(error) => break Some(format!("Codex stream failed: {error}")),
            }
        };
        let message = {
            let mut state = lock(&self.state);
            if error.is_some() {
                state.read_error = error;
            }
            state.reader_done = true;
            state
                .read_error
                .clone()
                .unwrap_or_else(|| "Codex session disconnected".into())
        };
        for (_, reply) in lock(&self.pending).drain() {
            let _ = reply.send(Err(message.clone()));
        }
        self.complete_turn(Err(message));
    }

    fn handle_event(self: &Arc<Self>, event: Value) {
        let has_method = event.get("method").is_some();
        if event.get("id").is_some() && !has_method {
            if let Some(reply) = event
                .get("id")
                .and_then(Value::as_i64)
                .and_then(|id| lock(&self.pending).remove(&id))
            {
                let _ = reply.send(match event.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .map(display_text)
                        .unwrap_or_else(|| "Provider error".into())),
                    None => Ok(event.get("result").cloned().unwrap_or_else(|| json!({}))),
                });
            }
        } else if event.get("id").is_some() {
            let inner = Arc::clone(self);
            thread::spawn(move || inner.handle_request(&event));
        } else {
            self.handle_notification(&event);
        }
    }

    fn handle_notification(&self, event: &Value) {
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        let params = event.get("params").cloned().unwrap_or_else(|| json!({}));
        let item = params.get("item").cloned().unwrap_or_else(|| json!({}));
        let item_type = item.get("type").and_then(Value::as_str);
        let delta = || params.get("delta").cloned().unwrap_or_else(|| json!(""));
        match method {
            "item/agentMessage/delta" => {
                self.emit("text", delta());
                lock(&self.state).wrote = true;
            }
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                self.emit("thinking", delta())
            }
            "item/started" => {
                let kind = item_type.unwrap_or("Working");
                if kind == "agentMessage" && lock(&self.state).wrote {
                    self.emit("break", json!("")); // A new message in the same turn.
                }
                self.emit("activity", json!(kind));
                if let Some(name) = codex_tool(kind) {
                    self.emit(
                        "tool",
                        json!({"id": item.get("id"), "name": name, "input": item}),
                    );
                }
            }
            _ => {}
        }
        let completed_tool =
            method == "item/completed" && item_type.is_some_and(|t| codex_tool(t).is_some());
        if completed_tool {
            let output = first_truthy(&item, &["aggregatedOutput", "result", "error"])
                .map(display_text)
                .unwrap_or_default();
            let exit_failed = item
                .get("exitCode")
                .is_some_and(|code| !code.is_null() && code.as_f64() != Some(0.0));
            let failed =
                item.get("status").and_then(Value::as_str) == Some("failed") || exit_failed;
            self.emit(
                "tool_result",
                json!({"id": item.get("id"), "output": output, "error": failed}),
            );
        }
        if method == "item/completed" && item_type == Some("commandExecution") {
            let command = match item.get("command") {
                Some(Value::Array(parts)) => {
                    parts.iter().map(display_text).collect::<Vec<_>>().join(" ")
                }
                Some(other) => display_text(other),
                None => String::new(),
            };
            self.emit("command", json!({"id": item.get("id"), "command": command}));
            let output = first_truthy(&item, &["aggregatedOutput"])
                .cloned()
                .unwrap_or_else(|| json!(""));
            self.emit(
                "command_output",
                json!({"id": item.get("id"), "output": output}),
            );
        } else if method == "thread/tokenUsage/updated" {
            lock(&self.state).usage = params
                .get("tokenUsage")
                .cloned()
                .unwrap_or_else(|| json!({}));
        } else if method == "account/rateLimits/updated" {
            let windows = codex_usage(params.get("rateLimits").unwrap_or(&Value::Null));
            if !windows.is_empty() {
                self.emit("usage", usage_value(&windows));
            }
        } else if method == "turn/completed" {
            let turn = params.get("turn").cloned().unwrap_or_else(|| json!({}));
            let status = turn.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "failed" || status == "interrupted" {
                let text = first_truthy(&turn, &["error"])
                    .map(display_text)
                    .unwrap_or_else(|| status.to_string());
                self.complete_turn(Err(text));
            } else {
                self.complete_turn(Ok(()));
            }
        }
    }

    /// A request from Codex to Donut: approvals are asked, anything else is refused.
    fn handle_request(&self, event: &Value) {
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        let id = event.get("id").cloned().unwrap_or(Value::Null);
        if method.ends_with("/requestApproval")
            && (method.contains("commandExecution") || method.contains("fileChange"))
        {
            let params = event.get("params").cloned().unwrap_or_else(|| json!({}));
            let decision = (self.approve)(method, &params);
            let _ = self.send(&json!({"id": id, "result": {"decision": codex_decision(decision)}}));
        } else if method == "item/tool/requestUserInput" {
            let questions = codex_questions(event.get("params").unwrap_or(&Value::Null));
            let answers = self.ask.as_ref().and_then(|ask| (ask.0)(&questions));
            // No answers (dismissed, or nothing to ask with) tells Codex the user did not answer.
            let result: serde_json::Map<String, Value> = answers
                .unwrap_or_default()
                .into_iter()
                .map(|(id, chosen)| (id, json!({"answers": chosen})))
                .collect();
            let _ = self.send(&json!({"id": id, "result": {"answers": result}}));
        } else {
            self.emit(
                "error",
                json!(format!(
                    "Unsupported provider interaction: {method}; use /native."
                )),
            );
            let _ = self.send(&json!({
                "id": id,
                "error": {"code": -32601, "message": "This interaction is not supported by Donut yet; use the native client."},
            }));
        }
    }

    fn quota(&self) -> Option<Vec<UsageWindow>> {
        let result = self.request("account/rateLimits/read", None).ok()?;
        let windows = codex_usage(result.get("rateLimits").unwrap_or(&Value::Null));
        (!windows.is_empty()).then_some(windows)
    }

    fn models_raw(&self) -> Result<Vec<Value>> {
        let mut models = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let result = self.request("model/list", Some(json!({"cursor": cursor})))?;
            if let Some(data) = result.get("data").and_then(Value::as_array) {
                models.extend(data.iter().cloned());
            }
            cursor = result.get("nextCursor").cloned().unwrap_or(Value::Null);
            if !py_truthy(&cursor) {
                return Ok(models);
            }
        }
    }
}

impl Session for CodexSession {
    fn family(&self) -> &'static str {
        "codex"
    }

    fn connect(&self) -> Result<()> {
        let inner = &self.inner;
        let program = lock(&inner.program)
            .clone()
            .or_else(|| which::which("codex").ok())
            .unwrap_or_else(|| "codex".into());
        let mut child = Command::new(program)
            // The -c flag lets the model ask questions outside plan mode as item/tool/requestUserInput, which
            // Donut answers. Without it Codex posts them as an async message and sleeps until a reply comes.
            .args([
                "app-server",
                "--stdio",
                "-c",
                "features.default_mode_request_user_input=true",
            ])
            .current_dir(&inner.project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("Failed to start Codex: {e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to start Codex: no stdout"))?;
        *lock(&inner.stdin) = child.stdin.take();
        *lock(&inner.process) = Some(Arc::new(Mutex::new(child)));
        {
            let mut state = lock(&inner.state);
            state.reader_done = false;
            state.read_error = None;
        }
        let reader = Arc::clone(inner);
        *lock(&inner.reader) = Some(thread::spawn(move || reader.read(stdout)));
        let handshake = inner
            .request(
                "initialize",
                Some(json!({
                    "clientInfo": {"name": "donut", "version": release_version()},
                    "capabilities": {"experimentalApi": true},
                })),
            )
            .and_then(|_| inner.send(&json!({"method": "initialized"})));
        if let Err(error) = handshake {
            let _ = self.close();
            return Err(error);
        }
        // Report account rate limits for the usage bar; failures only mean the bar stays empty.
        let usage = Arc::clone(inner);
        thread::spawn(move || {
            if let Some(windows) = usage.quota() {
                usage.emit("usage", usage_value(&windows));
            }
        });
        Ok(())
    }

    fn query(&self, text: &str) -> Result<()> {
        let inner = &self.inner;
        let (approval, sandbox, reviewer) = inner.policy();
        let (thread_known, profile) = {
            let state = lock(&inner.state);
            (state.thread.is_some(), state.profile.clone())
        };
        if !thread_known {
            let mut settings = json!({
                "cwd": inner.project.to_string_lossy(),
                "model": profile_str(&profile, "model"),
                "approvalPolicy": approval,
                "sandbox": sandbox,
                "approvalsReviewer": reviewer,
            });
            let method = match &inner.resume_id {
                // /resume: continue the saved thread with today's model and policies.
                Some(resume) if !resume.is_empty() => {
                    settings["threadId"] = json!(resume);
                    "thread/resume"
                }
                _ => "thread/start",
            };
            let result = inner.request(method, Some(settings))?;
            let id = result
                .get("thread")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("'thread'"))?;
            let mut state = lock(&inner.state);
            state.thread = Some(id.to_string());
            state.permission_changed = false;
        }
        let (done, finished) = mpsc::channel();
        let params = {
            let mut state = lock(&inner.state);
            let mut params = json!({
                "threadId": state.thread,
                "input": [{"type": "text", "text": text}],
            });
            if state.model_changed {
                // turn/start overrides persist for later turns.
                params["model"] = state.profile.get("model").cloned().unwrap_or(Value::Null);
                state.model_changed = false;
            }
            if let Some(effort) = state
                .profile
                .get("effort")
                .filter(|e| py_truthy(e))
                .cloned()
            {
                if state.effort_changed || !state.effort_sent {
                    params["effort"] = effort;
                    state.effort_changed = false;
                    state.effort_sent = true;
                }
            }
            if state.permission_changed {
                params["approvalPolicy"] = json!(approval);
                params["sandboxPolicy"] = json!({"type": codex_sandbox(&sandbox)});
                params["approvalsReviewer"] = json!(reviewer);
                state.permission_changed = false;
            }
            state.turn_done = Some(done);
            state.wrote = false;
            params
        };
        inner.request("turn/start", Some(params))?;
        match finished.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_) => Err(anyhow!("Codex session disconnected")),
        }
    }

    fn interrupt(&self) -> Result<()> {
        let inner = &self.inner;
        let Some(thread) = lock(&inner.state).thread.clone() else {
            return Ok(());
        };
        // The thread view supplies the currently running turn ID.
        let result = inner.request(
            "thread/read",
            Some(json!({"threadId": thread, "includeTurns": true})),
        )?;
        let last = result
            .get("thread")
            .and_then(|t| t.get("turns"))
            .and_then(Value::as_array)
            .and_then(|t| t.last().cloned());
        if let Some(turn) = last {
            inner.request(
                "turn/interrupt",
                Some(json!({"threadId": thread, "turnId": turn["id"]})),
            )?;
        }
        Ok(())
    }

    fn close(&self) -> Result<()> {
        let inner = &self.inner;
        let process = lock(&inner.process).clone();
        if let Some(process) = process {
            if lock(&process).try_wait().ok().flatten().is_none() {
                let pid = lock(&process).id();
                terminate_pid(pid);
                if wait_exit(&process, Duration::from_secs(5)).is_none() {
                    kill_pid(pid);
                    wait_exit(&process, Duration::from_secs(5));
                }
            }
        }
        lock(&inner.stdin).take();
        if let Some(reader) = lock(&inner.reader).take() {
            let _ = reader.join();
        }
        Ok(())
    }

    fn set_permission(&self, mode: &str) -> Result<()> {
        // Codex applies policy overrides on the next turn/start, and they persist for later turns.
        let mut state = lock(&self.inner.state);
        state.permission = mode.to_string();
        state.permission_changed = state.thread.is_some() && !self.inner.read_only;
        Ok(())
    }

    fn quota(&self) -> Option<Vec<UsageWindow>> {
        self.inner.quota()
    }

    fn control(&self, method: &str, params: Value) -> Result<Value> {
        self.inner.request(method, Some(params))
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        self.inner
            .models_raw()?
            .iter()
            .map(|m| {
                let id = m
                    .get("model")
                    .map(display_text)
                    .ok_or_else(|| anyhow!("'model'"))?;
                Ok(ModelInfo {
                    name: m
                        .get("displayName")
                        .map_or_else(|| id.clone(), display_text),
                    id,
                })
            })
            .collect()
    }

    fn account_usage(&self) -> Result<Value> {
        self.inner.request("account/rateLimits/read", None)
    }

    fn provider_id(&self) -> Option<String> {
        lock(&self.inner.state).thread.clone()
    }

    fn usage(&self) -> Value {
        lock(&self.inner.state).usage.clone()
    }

    fn permission(&self) -> String {
        lock(&self.inner.state).permission.clone()
    }

    fn profile(&self) -> Value {
        lock(&self.inner.state).profile.clone()
    }

    fn set_profile(&self, profile: Value) {
        lock(&self.inner.state).profile = profile;
    }

    /// Use another model from the next turn on, in the same thread, so Codex keeps its whole context.
    fn switch_model(&self, model: Option<&str>) -> Result<()> {
        let mut state = lock(&self.inner.state);
        state.profile = with_key(&state.profile, "model", json!(model));
        state.model_changed = state.thread.is_some();
        Ok(())
    }

    /// Use this reasoning effort from the next turn on; Codex keeps it for later turns.
    fn set_effort(&self, level: &str) -> Result<()> {
        let mut state = lock(&self.inner.state);
        state.profile = with_key(&state.profile, "effort", json!(level));
        state.effort_changed = true;
        Ok(())
    }

    fn model_details(&self) -> Result<Vec<Value>> {
        self.inner.models_raw()
    }
}

// ---------------------------------------------------------------------------------------------------------
// Claude

/// What the pump forwards to a running turn.
enum Inbox {
    Message(Message),
    Failed(String),
}

/// State of one turn (or one wake-up) while its messages are handled.
#[derive(Debug, Default, Clone)]
struct HandleState {
    wrote: bool,
    woken: bool,
}

/// How the pump routes messages.
#[derive(Default)]
struct Route {
    in_turn: bool,
    /// Not `None` while a turn Claude Code started by itself is running.
    waking: Option<HandleState>,
    /// Why the message stream ended, once it did.
    fatal: Option<String>,
}

struct ClaudeData {
    profile: Value,
    permission: String,
    usage: Value,
    account: Option<String>,
    session_id: Option<String>,
    info: Value,
    connected: bool,
    closed: bool,
}

struct ClaudeInner {
    project: PathBuf,
    emit: Emit,
    approve: Approve,
    ask: Option<Ask>,
    read_only: bool,
    bare: bool,
    chrome: bool,
    resume_id: Option<String>,
    program: Mutex<Option<PathBuf>>,
    data: Mutex<ClaudeData>,
    client: Mutex<Option<Arc<ClaudeClient>>>,
    route: Mutex<Route>,
    inbox_tx: Sender<Inbox>,
    inbox_rx: Mutex<Receiver<Inbox>>,
    /// Python's owner task: one operation at a time.
    operations: Mutex<()>,
}

/// A Claude Code conversation: one `claude` process in stream-json mode.
pub struct ClaudeSession {
    inner: Arc<ClaudeInner>,
}

impl ClaudeSession {
    /// A session that is not connected yet. `approve` answers Claude's tool permission requests.
    pub fn new(
        project: &Path,
        profile: Value,
        options: SessionOptions,
        emit: Emit,
        approve: Approve,
    ) -> ClaudeSession {
        let (inbox_tx, inbox_rx) = mpsc::channel();
        ClaudeSession {
            inner: Arc::new(ClaudeInner {
                project: project.to_path_buf(),
                emit,
                approve,
                ask: options.ask,
                read_only: options.read_only,
                bare: options.bare,
                chrome: options.chrome,
                resume_id: options.resume_id,
                program: Mutex::new(None),
                data: Mutex::new(ClaudeData {
                    profile,
                    permission: options.permission,
                    usage: json!({}),
                    account: None,
                    session_id: None,
                    info: json!({}),
                    connected: false,
                    closed: false,
                }),
                client: Mutex::new(None),
                route: Mutex::new(Route::default()),
                inbox_tx,
                inbox_rx: Mutex::new(inbox_rx),
                operations: Mutex::new(()),
            }),
        }
    }

    /// Run this executable instead of the `claude` found on the PATH.
    pub fn set_program(&self, program: impl Into<PathBuf>) {
        *lock(&self.inner.program) = Some(program.into());
    }
}

impl ClaudeInner {
    fn emit(&self, kind: &str, data: Value) {
        (self.emit)(kind, data);
    }

    fn client(&self) -> Result<Arc<ClaudeClient>> {
        lock(&self.client)
            .clone()
            .ok_or_else(|| anyhow!("Not connected. Call connect() first."))
    }

    fn live_client(&self) -> Option<Arc<ClaudeClient>> {
        let live = {
            let data = lock(&self.data);
            data.connected && !data.closed
        };
        if live {
            lock(&self.client).clone()
        } else {
            None
        }
    }

    fn options(&self) -> ClaudeOptions {
        let data = lock(&self.data);
        let profile = &data.profile;
        let mut env = Vec::new();
        if let Some(dir) = profile_str(profile, "claude_config_dir") {
            env.push((
                "CLAUDE_CONFIG_DIR".to_string(),
                expanduser(&dir).to_string_lossy().into_owned(),
            ));
        }
        let permission_mode = if self.read_only {
            "plan".to_string()
        } else {
            match native_permission("claude", &data.permission) {
                NativePermission::Mode(mode) => mode.to_string(),
                NativePermission::Codex { .. } => unreachable!("claude permissions are modes"),
            }
        };
        let flag = |name: &str| (name.to_string(), None);
        let extra_args = if self.read_only {
            if self.bare {
                vec![
                    flag("strict-mcp-config"),
                    ("tools".to_string(), Some(String::new())),
                ]
            } else {
                Vec::new()
            }
        } else {
            // Lets /permissions or Shift+Tab switch this live session to Full access later. It bypasses
            // nothing by itself; review sessions stay read-only and never get it. chrome is Claude Code's own
            // switch for the Claude in Chrome extension, turned on with /chrome.
            let mut args = vec![flag("allow-dangerously-skip-permissions")];
            if self.chrome {
                args.push(flag("chrome"));
            }
            args
        };
        ClaudeOptions {
            cli_path: lock(&self.program)
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .or_else(|| {
                    which::which("claude")
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                }),
            cwd: Some(self.project.clone()),
            env,
            model: profile_str(profile, "model"),
            // bare: a one-question session (auto's rating) needs no settings, plugins or connectors in its prompt.
            setting_sources: Some(if self.bare {
                Vec::new()
            } else {
                vec!["user".into(), "project".into(), "local".into()]
            }),
            permission_mode: Some(permission_mode),
            include_partial_messages: true,
            // The SDK stops at 1 MB for one message; a screenshot or a large tool result is larger than that.
            max_buffer_size: MAX_MESSAGE_BYTES,
            resume: self.resume_id.clone().filter(|r| !r.is_empty()),
            effort: profile_str(profile, "effort"),
            extra_args,
        }
    }

    /// Report background shells and agents Claude Code starts and ends. Returns whether message was one.
    fn track_task(&self, message: &Message) -> bool {
        match message {
            Message::TaskStarted {
                task_id,
                description,
                task_type,
            } => {
                let kind = if task_type
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("bash")
                {
                    "shell"
                } else {
                    "agent"
                };
                self.emit("task", json!({"id": task_id, "state": "running", "kind": kind, "description": description}));
                true
            }
            Message::TaskUpdated { task_id, status } => {
                if let Some(status) = status
                    .as_deref()
                    .filter(|s| ["completed", "failed", "stopped", "killed"].contains(s))
                {
                    self.emit("task", json!({"id": task_id, "state": status}));
                }
                true
            }
            Message::TaskNotification {
                task_id,
                status,
                summary,
            } => {
                self.emit(
                    "task",
                    json!({"id": task_id, "state": status, "summary": summary}),
                );
                false // The idle reader still announces it.
            }
            _ => false,
        }
    }

    /// Show one message from Claude. Returns `Ok(true)` when it ends a turn (its result message).
    fn handle(&self, message: Message, state: &mut HandleState) -> Result<bool> {
        if self.track_task(&message) {
            return Ok(false);
        }
        match message {
            Message::System { subtype, data } if subtype == "init" => {
                // "none" is a plan login; anything else is an API key. The spend figure needs to know which.
                lock(&self.data).account = data
                    .get("apiKeySource")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Message::RateLimit(info) => {
                let windows = claude_usage(&info);
                if !windows.is_empty() {
                    self.emit("usage", usage_value(&windows));
                }
            }
            Message::Assistant {
                content,
                model,
                usage,
                message_id,
                uuid,
            } => {
                if let Some(usage) = usage.filter(py_truthy) {
                    let dollars = claude_cost(Some(&model), &usage);
                    self.emit(
                        "spend",
                        json!({"id": message_id.or(uuid), "dollars": dollars}),
                    );
                }
                for block in content {
                    match block {
                        Block::Text(text) if !text.trim().is_empty() => {
                            if state.wrote {
                                self.emit("break", json!("")); // Separate text blocks are separate messages.
                            }
                            self.emit("text", json!(text));
                            state.wrote = true;
                        }
                        Block::Thinking(thinking) if !thinking.trim().is_empty() => {
                            self.emit("thinking", json!(thinking))
                        }
                        Block::ToolUse { id, name, input } => {
                            self.emit("activity", json!(name));
                            if name == "Bash" {
                                let command =
                                    input.get("command").map(display_text).unwrap_or_default();
                                self.emit("tool", json!({"id": id, "name": name, "input": input}));
                                self.emit("command", json!({"id": id, "command": command}));
                            } else {
                                self.emit("tool", json!({"id": id, "name": name, "input": input}));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::User {
                content: UserContent::Blocks(blocks),
                ..
            } => {
                for block in blocks {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = block
                    {
                        let output = match &content {
                            Value::Array(parts) => parts
                                .iter()
                                .filter(|p| p.is_object())
                                .map(|p| p.get("text").map(display_text).unwrap_or_default())
                                .collect::<Vec<_>>()
                                .join("\n"),
                            other => display_text(other),
                        };
                        self.emit(
                            "command_output",
                            json!({"id": tool_use_id, "output": output}),
                        );
                        self.emit("tool_result", json!({"id": tool_use_id, "output": output, "error": is_error.unwrap_or(false)}));
                    }
                }
            }
            Message::Result(ResultMessage {
                session_id,
                usage,
                total_cost_usd,
                is_error,
                result,
                ..
            }) => {
                {
                    let mut data = lock(&self.data);
                    if !session_id.is_empty() {
                        data.session_id = Some(session_id);
                    }
                    data.usage = json!({"tokens": usage, "reported_cost_usd": total_cost_usd});
                }
                if is_error && !state.woken {
                    return Err(anyhow!(
                        result
                            .filter(|r| !r.is_empty())
                            .unwrap_or_else(|| "Claude turn failed".into())
                    ));
                }
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    /// Read every message Claude sends, for the whole session.
    ///
    /// During a turn Donut asked for, messages go to that turn. Between turns, Claude Code may start a turn of its
    /// own, for example when a background shell it left running finishes; those are shown as a wake-up.
    fn pump(self: Arc<Self>, mut stream: MessageStream) {
        let failure = loop {
            let message = match stream.next() {
                Some(Ok(message)) => message,
                Some(Err(error)) => break error,
                None => break "Claude session disconnected".to_string(),
            };
            let mut route = lock(&self.route);
            if route.in_turn {
                let _ = self.inbox_tx.send(Inbox::Message(message));
                continue;
            }
            if self.track_task(&message) {
                continue;
            }
            match &message {
                Message::TaskNotification { summary, .. } => {
                    if route.waking.is_none() {
                        let text = if summary.is_empty() {
                            "A background task finished."
                        } else {
                            summary
                        };
                        self.emit("background", json!(text));
                    }
                    continue;
                }
                Message::System { .. }
                | Message::TaskStarted { .. }
                | Message::TaskUpdated { .. } => continue,
                // Claude Code's echo of a command of its own ("Set model to ..." after a model switch). It is
                // not a turn and no result follows it, so it must not start a wake-up that never ends.
                Message::User {
                    content: UserContent::Text(_),
                    ..
                } => continue,
                _ => {}
            }
            if route.waking.is_none() {
                route.waking = Some(HandleState {
                    wrote: false,
                    woken: true,
                });
                self.emit("wake_start", json!(""));
            }
            let mut state = route.waking.take().unwrap_or_default();
            let finished = match self.handle(message, &mut state) {
                Ok(finished) => finished,
                Err(error) => {
                    self.emit("error", json!(error.to_string()));
                    true
                }
            };
            if finished {
                self.emit("wake_end", json!(""));
            } else {
                route.waking = Some(state);
            }
        };
        let mut route = lock(&self.route);
        route.fatal = Some(failure.clone());
        if route.in_turn {
            let _ = self.inbox_tx.send(Inbox::Failed(failure));
        }
    }
}

impl Session for ClaudeSession {
    fn family(&self) -> &'static str {
        "claude"
    }

    fn connect(&self) -> Result<()> {
        let inner = &self.inner;
        let _operation = lock(&inner.operations);
        let options = inner.options();
        let approver: ToolApprover = {
            let approve = Arc::clone(&inner.approve);
            let ask = inner.ask.clone();
            Arc::new(move |name: &str, input: &Value| {
                if name == "AskUserQuestion" {
                    return claude_answer(input, ask.as_ref());
                }
                match approve(name, input) {
                    Decision::Yes | Decision::Always => PermissionResult::Allow {
                        updated_input: input.clone(),
                    },
                    Decision::No => PermissionResult::Deny {
                        message: "User declined".into(),
                    },
                }
            })
        };
        let (client, stream) = ClaudeClient::connect(&options, Some(approver))?;
        {
            let mut data = lock(&inner.data);
            data.info = if client.server_info().is_object() {
                client.server_info().clone()
            } else {
                json!({})
            };
            data.connected = true;
            data.closed = false;
        }
        *lock(&inner.client) = Some(Arc::new(client));
        *lock(&inner.route) = Route::default();
        let pump = Arc::clone(inner);
        thread::spawn(move || pump.pump(stream));
        Ok(())
    }

    fn query(&self, text: &str) -> Result<()> {
        let inner = &self.inner;
        let _operation = lock(&inner.operations);
        let client = inner.client()?;
        let inbox = lock(&inner.inbox_rx);
        {
            let mut route = lock(&inner.route);
            if let Some(error) = &route.fatal {
                return Err(anyhow!(error.clone()));
            }
            route.in_turn = true;
            route.waking = None; // A turn Claude Code had started by itself now ends inside this one.
        }
        struct EndTurn<'a>(&'a ClaudeInner);
        impl Drop for EndTurn<'_> {
            fn drop(&mut self) {
                lock(&self.0.route).in_turn = false;
            }
        }
        let _end = EndTurn(inner);
        client.query(text)?;
        let mut state = HandleState::default();
        loop {
            match inbox.recv() {
                Ok(Inbox::Message(message)) => {
                    if inner.handle(message, &mut state)? {
                        return Ok(());
                    }
                }
                Ok(Inbox::Failed(error)) => return Err(anyhow!(error)),
                Err(_) => return Err(anyhow!("Claude session closed")),
            }
        }
    }

    fn interrupt(&self) -> Result<()> {
        self.inner.client()?.interrupt()
    }

    fn close(&self) -> Result<()> {
        let inner = &self.inner;
        let _operation = lock(&inner.operations);
        {
            let mut data = lock(&inner.data);
            if !data.connected || data.closed {
                return Ok(());
            }
            data.closed = true;
        }
        if let Some(client) = lock(&inner.client).clone() {
            client.disconnect();
        }
        Ok(())
    }

    /// Switch Claude's native permission mode in the live session (applied when it is idle).
    fn set_permission(&self, mode: &str) -> Result<()> {
        let inner = &self.inner;
        lock(&inner.data).permission = mode.to_string();
        if inner.read_only {
            return Ok(()); // Review mode stays in Claude's read-only plan mode whatever the permission mode.
        }
        let _operation = lock(&inner.operations);
        if let Some(client) = inner.live_client() {
            match native_permission("claude", mode) {
                NativePermission::Mode(native) => client.set_permission_mode(native)?,
                NativePermission::Codex { .. } => unreachable!("claude permissions are modes"),
            }
        }
        Ok(())
    }

    /// Plan usage without sending a prompt, through the CLI's get_usage control request.
    fn quota(&self) -> Option<Vec<UsageWindow>> {
        let inner = &self.inner;
        let client = inner.live_client()?;
        let end = Instant::now() + Duration::from_secs(30);
        let _operation = loop {
            if let Ok(guard) = inner.operations.try_lock() {
                break guard;
            }
            if Instant::now() >= end {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let left = end.saturating_duration_since(Instant::now());
        let result = client
            .control_request_within(json!({"subtype": "get_usage"}), left)
            .ok()?;
        claude_plan_usage(&result)
    }

    fn control(&self, method: &str, params: Value) -> Result<Value> {
        let params = match params {
            Value::Null => Map::new(),
            Value::Object(map) => map,
            other => return Err(anyhow!("control parameters must be an object, got {other}")),
        };
        let mut request = Map::new();
        request.insert("subtype".into(), json!(method));
        request.extend(params);
        let _operation = lock(&self.inner.operations);
        self.inner.client()?.control_request(Value::Object(request))
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        let _operation = lock(&self.inner.operations);
        let info = lock(&self.inner.data).info.clone();
        info.get("models")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|m| {
                let id = m
                    .get("value")
                    .map(display_text)
                    .ok_or_else(|| anyhow!("'value'"))?;
                Ok(ModelInfo {
                    name: m
                        .get("displayName")
                        .map_or_else(|| id.clone(), display_text),
                    id,
                })
            })
            .collect()
    }

    fn account_usage(&self) -> Result<Value> {
        let _operation = lock(&self.inner.operations);
        Ok(json!({
            "session": lock(&self.inner.data).usage.clone(),
            "account_limits": "Not exposed by the supported SDK; available in native Claude /usage.",
        }))
    }

    fn provider_id(&self) -> Option<String> {
        lock(&self.inner.data).session_id.clone()
    }

    fn usage(&self) -> Value {
        lock(&self.inner.data).usage.clone()
    }

    fn permission(&self) -> String {
        lock(&self.inner.data).permission.clone()
    }

    fn profile(&self) -> Value {
        lock(&self.inner.data).profile.clone()
    }

    fn set_profile(&self, profile: Value) {
        lock(&self.inner.data).profile = profile;
    }

    /// Change the model of this live conversation; Claude Code keeps its whole context.
    fn switch_model(&self, model: Option<&str>) -> Result<()> {
        let inner = &self.inner;
        let _operation = lock(&inner.operations);
        inner.client()?.set_model(model.filter(|m| !m.is_empty()))?;
        let mut data = lock(&inner.data);
        data.profile = with_key(&data.profile, "model", json!(model));
        Ok(())
    }

    /// Change effort in the live session, as Claude Code's own /effort does.
    fn set_effort(&self, level: &str) -> Result<()> {
        let inner = &self.inner;
        let _operation = lock(&inner.operations);
        inner.client()?.control_request(
            json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": level}}),
        )?;
        let mut data = lock(&inner.data);
        data.profile = with_key(&data.profile, "effort", json!(level));
        Ok(())
    }

    fn info(&self) -> Value {
        lock(&self.inner.data).info.clone()
    }

    fn account(&self) -> Option<String> {
        lock(&self.inner.data).account.clone()
    }
}

// ---------------------------------------------------------------------------------------------------------
// Qwen

struct QwenData {
    profile: Value,
    permission: String,
    session_id: Option<String>,
    usage: Value,
}

struct QwenInner {
    project: PathBuf,
    emit: Emit,
    read_only: bool,
    resume_id: Option<String>,
    data: Mutex<QwenData>,
    /// The pid of the running turn's process.
    pid: Mutex<Option<u32>>,
}

/// Keep Qwen's native saved session across turns, including local models. Every turn is a new process.
pub struct QwenSession {
    inner: Arc<QwenInner>,
}

impl QwenSession {
    /// A session that is not connected yet.
    pub fn new(project: &Path, profile: Value, options: SessionOptions, emit: Emit) -> QwenSession {
        QwenSession {
            inner: Arc::new(QwenInner {
                project: project.to_path_buf(),
                emit,
                read_only: options.read_only,
                resume_id: options.resume_id,
                data: Mutex::new(QwenData {
                    profile,
                    permission: options.permission,
                    session_id: None,
                    usage: json!({}),
                }),
                pid: Mutex::new(None),
            }),
        }
    }

    /// Qwen cannot show approval prompts inside Donut, so Ask behaves as Accept edits for it.
    fn effective_permission(&self) -> String {
        let permission = lock(&self.inner.data).permission.clone();
        if permission == "ask" {
            "accept_edits".into()
        } else {
            permission
        }
    }
}

impl QwenInner {
    fn emit(&self, kind: &str, data: Value) {
        (self.emit)(kind, data);
    }

    fn handle_event(&self, event: &Value, wrote: &mut bool) -> Result<()> {
        match event.get("type").and_then(Value::as_str) {
            Some("system") => {
                if let Some(id) = event.get("session_id") {
                    lock(&self.data).session_id = id.as_str().map(str::to_string);
                }
            }
            Some("stream_event") => {
                let item = event.get("event").cloned().unwrap_or_else(|| json!({}));
                let block = item
                    .get("content_block")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let delta = item.get("delta").cloned().unwrap_or_else(|| json!({}));
                let block_type = block.get("type").and_then(Value::as_str);
                if block_type == Some("text") && *wrote {
                    self.emit("break", json!(""));
                }
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        self.emit(
                            "text",
                            delta.get("text").cloned().unwrap_or_else(|| json!("")),
                        );
                        *wrote = true;
                    }
                    Some("thinking_delta") => self.emit(
                        "thinking",
                        delta.get("thinking").cloned().unwrap_or_else(|| json!("")),
                    ),
                    _ => {}
                }
                if block_type == Some("tool_use") {
                    let name = block.get("name").cloned();
                    self.emit("activity", name.clone().unwrap_or_else(|| json!("Working")));
                    self.emit("tool", json!({"id": block.get("id"), "name": name.unwrap_or_else(|| json!("Tool")), "input": {}}));
                }
            }
            Some("result") => {
                lock(&self.data).usage = event.get("usage").cloned().unwrap_or_else(|| json!({}));
                if event.get("is_error").is_some_and(py_truthy) {
                    return Err(anyhow!("Qwen reported a failed turn"));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Session for QwenSession {
    fn family(&self) -> &'static str {
        "qwen"
    }

    fn connect(&self) -> Result<()> {
        let mut data = lock(&self.inner.data);
        data.session_id = self
            .inner
            .resume_id
            .clone()
            .filter(|r| !r.is_empty())
            .or_else(|| data.session_id.take());
        Ok(())
    }

    fn query(&self, text: &str) -> Result<()> {
        let inner = &self.inner;
        let (profile, session_id) = {
            let data = lock(&inner.data);
            (data.profile.clone(), data.session_id.clone())
        };
        crate::providers::activate(&profile)?;
        let (mut args, env) = crate::providers::command(
            &profile,
            text,
            false,
            Some(&self.effective_permission()),
            inner.read_only,
        )?;
        if let Some(id) = session_id {
            args.extend(["--resume".to_string(), id]);
        }
        let program = args
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("Qwen command is empty"))?;
        let mut child = Command::new(program)
            .args(&args[1..])
            .current_dir(&inner.project)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to start Qwen: {e}"))?;
        *lock(&inner.pid) = Some(child.id());
        let mut stdin = child.stdin.take();
        let prompt = text.to_string();
        thread::spawn(move || {
            if let Some(pipe) = stdin.as_mut() {
                let _ = pipe.write_all(prompt.as_bytes());
            }
            drop(stdin);
        });
        let stderr = child.stderr.take();
        let errors = thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = stderr {
                let _ = io::Read::read_to_end(&mut pipe, &mut bytes);
            }
            bytes
        });
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to start Qwen: no stdout"))?;
        let mut reader = BufReader::with_capacity(64 * 1024, stdout);
        let mut wrote = false;
        let outcome: Result<()> = loop {
            match read_line_limited(&mut reader, MAX_MESSAGE_BYTES) {
                Ok(Some(line)) => {
                    if let Ok(event) = serde_json::from_slice::<Value>(&line) {
                        if let Err(error) = inner.handle_event(&event, &mut wrote) {
                            break Err(error);
                        }
                    }
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(anyhow!("Qwen output unreadable: {error}")),
            }
        };
        if let Err(error) = outcome {
            // The turn is over for Donut; the process ends by itself, so reap it in the background.
            let inner = Arc::clone(inner);
            thread::spawn(move || {
                let _ = child.wait();
                *lock(&inner.pid) = None;
            });
            return Err(error);
        }
        let status = child.wait();
        *lock(&inner.pid) = None;
        let stderr = errors.join().unwrap_or_default();
        match status {
            Ok(status) if !status.success() => {
                let text = String::from_utf8_lossy(&stderr);
                let tail: Vec<char> = text.chars().collect();
                let tail: String = tail[tail.len().saturating_sub(500)..].iter().collect();
                Err(anyhow!("Qwen failed: {tail}"))
            }
            Err(error) => Err(anyhow!("Qwen failed: {error}")),
            Ok(_) => Ok(()),
        }
    }

    fn interrupt(&self) -> Result<()> {
        if let Some(pid) = *lock(&self.inner.pid) {
            terminate_pid(pid);
        }
        Ok(())
    }

    fn close(&self) -> Result<()> {
        self.interrupt()?;
        let end = Instant::now() + Duration::from_secs(10);
        while lock(&self.inner.pid).is_some() {
            if Instant::now() >= end {
                if let Some(pid) = *lock(&self.inner.pid) {
                    kill_pid(pid);
                }
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// Every Qwen turn is a new process, so the next turn picks this up.
    fn set_permission(&self, mode: &str) -> Result<()> {
        lock(&self.inner.data).permission = mode.to_string();
        Ok(())
    }

    /// Local and OpenAI-compatible models have no plan limits to report.
    fn quota(&self) -> Option<Vec<UsageWindow>> {
        None
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Err(anyhow!("Qwen sessions have no model list"))
    }

    fn account_usage(&self) -> Result<Value> {
        Ok(json!({"session": lock(&self.inner.data).usage.clone()}))
    }

    fn provider_id(&self) -> Option<String> {
        lock(&self.inner.data).session_id.clone()
    }

    fn usage(&self) -> Value {
        lock(&self.inner.data).usage.clone()
    }

    fn permission(&self) -> String {
        lock(&self.inner.data).permission.clone()
    }

    fn profile(&self) -> Value {
        lock(&self.inner.data).profile.clone()
    }

    fn set_profile(&self, profile: Value) {
        lock(&self.inner.data).profile = profile;
    }
}

#[cfg(test)]
mod tests;
