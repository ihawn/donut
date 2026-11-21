//! Provider CLIs own authentication and execution; no credentials in config.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;

/// "qwen" for ollama and openai engines, else the engine itself.
pub fn provider_family(engine: &str) -> &str {
    if engine == "ollama" || engine == "openai" {
        "qwen"
    } else {
        engine
    }
}

/// A provider's own form of a Donut permission mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePermission {
    /// Claude Code --permission-mode or Qwen Code --approval-mode value.
    Mode(&'static str),
    /// Codex app-server (approvalPolicy, sandbox, approvalsReviewer).
    Codex {
        approval: &'static str,
        sandbox: &'static str,
        reviewer: &'static str,
    },
}

/// The provider's own value for a Donut permission mode (ask, accept_edits, auto, full).
pub fn native_permission(engine: &str, mode: &str) -> NativePermission {
    use NativePermission::{Codex, Mode};
    match (provider_family(engine), mode) {
        ("claude", "ask") => Mode("default"),
        ("claude", "accept_edits") => Mode("acceptEdits"),
        ("claude", "auto") => Mode("auto"),
        ("claude", "full") => Mode("bypassPermissions"),
        ("qwen", "ask") => Mode("default"),
        ("qwen", "accept_edits") => Mode("auto-edit"),
        ("qwen", "auto") => Mode("auto"),
        ("qwen", "full") => Mode("yolo"),
        ("codex", "ask") => Codex {
            approval: "untrusted",
            sandbox: "workspace-write",
            reviewer: "user",
        },
        ("codex", "accept_edits") => Codex {
            approval: "on-request",
            sandbox: "workspace-write",
            reviewer: "user",
        },
        ("codex", "auto") => Codex {
            approval: "on-request",
            sandbox: "workspace-write",
            reviewer: "auto_review",
        },
        ("codex", "full") => Codex {
            approval: "never",
            sandbox: "danger-full-access",
            reviewer: "user",
        },
        (family, mode) => panic!("no native permission for {family} {mode}"),
    }
}

/// Donut's permission modes, applied through each provider's own mechanism: (key, label, description).
pub const PERMISSION_MODES: &[(&str, &str, &str)] = &[
    ("ask", "Ask", "Ask before file edits and commands"),
    (
        "accept_edits",
        "Accept edits",
        "Edit files without asking; ask before commands",
    ),
    (
        "auto",
        "Auto",
        "The provider's own reviewer approves safe actions and blocks risky ones",
    ),
    (
        "full",
        "Full access",
        "Run everything without asking; only in projects you trust",
    ),
];

/// Shift+Tab cycles these, as in Claude Code. Full access is only reachable from /permissions.
pub const PERMISSION_CYCLE: &[&str] = &["ask", "accept_edits", "auto"];

static EFFORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]{1,16}$").unwrap());
static KEY_ENV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());
static OLLAMA_NOISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("\u{1b}\\[[0-9;?]*[A-Za-z]|[\u{2800}-\u{28ff}]").unwrap());

/// The non-empty string at a profile key (Python's truthy `profile.get(key)` for strings).
fn text<'a>(profile: &'a Value, key: &str) -> Option<&'a str> {
    profile
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// The mode for an unattended run, which cannot answer prompts, so Ask behaves as Accept edits.
pub fn batch_permission(profile: &Value) -> String {
    let auto_approve = profile
        .get("auto_approve")
        .is_some_and(|value| match value {
            Value::Bool(flag) => *flag,
            Value::Null => false,
            Value::Number(number) => number.as_f64() != Some(0.0),
            Value::String(text) => !text.is_empty(),
            Value::Array(list) => !list.is_empty(),
            Value::Object(map) => !map.is_empty(),
        });
    let mode = text(profile, "permission_mode").unwrap_or(if auto_approve {
        "full"
    } else {
        "accept_edits"
    });
    if mode == "ask" {
        "accept_edits".into()
    } else {
        mode.into()
    }
}

/// The pieces of a URL that Python's urllib.parse.urlsplit reports.
#[derive(Debug, Default)]
pub struct UrlParts {
    pub scheme: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub query: String,
    pub fragment: String,
}

/// Split a URL the way urllib.parse.urlsplit does.
pub fn urlsplit(url: &str) -> UrlParts {
    let mut parts = UrlParts::default();
    let mut rest = url;
    if let Some(at) = rest.find(':')
        && at > 0
        && rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && rest[..at]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        parts.scheme = rest[..at].to_lowercase();
        rest = &rest[at + 1..];
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
    }
    if let Some((before, fragment)) = rest.split_once('#') {
        parts.fragment = fragment.to_string();
        rest = before;
    }
    if let Some((_, query)) = rest.split_once('?') {
        parts.query = query.to_string();
    }
    let (userinfo, hostinfo) = match netloc.rsplit_once('@') {
        Some((userinfo, hostinfo)) => (Some(userinfo), hostinfo),
        None => (None, netloc),
    };
    if let Some(userinfo) = userinfo {
        match userinfo.split_once(':') {
            Some((name, password)) => {
                parts.username = Some(name.to_string());
                parts.password = Some(password.to_string());
            }
            None => parts.username = Some(userinfo.to_string()),
        }
    }
    let (hostname, port) = match hostinfo.split_once('[') {
        Some((_, bracketed)) => {
            let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            (host, after.split_once(':').map_or("", |(_, port)| port))
        }
        None => hostinfo.split_once(':').unwrap_or((hostinfo, "")),
    };
    if !hostname.is_empty() {
        parts.hostname = Some(hostname.to_lowercase());
    }
    parts.port = port.parse::<u16>().ok();
    parts
}

/// Check a profile, raising the same complaints as the Python version.
pub fn validate(profile: &Value) -> Result<()> {
    let engine = profile.get("engine").and_then(Value::as_str);
    if !matches!(engine, Some("ollama" | "codex" | "claude" | "openai")) {
        bail!("Unsupported engine: {}", engine.unwrap_or("None"));
    }
    let engine = engine.unwrap_or_default();
    if profile.get("model").is_some_and(|model| !model.is_string()) {
        bail!("Model must be a string");
    }
    if matches!(engine, "ollama" | "openai") && text(profile, "model").is_none() {
        bail!("A model ID is required");
    }
    if let Some(mode) = profile.get("permission_mode")
        && !mode
            .as_str()
            .is_some_and(|mode| PERMISSION_MODES.iter().any(|(key, ..)| *key == mode))
    {
        bail!(
            "permission_mode must be one of: {}",
            PERMISSION_MODES
                .iter()
                .map(|(key, ..)| *key)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(effort) = profile.get("effort")
        && !effort
            .as_str()
            .is_some_and(|effort| EFFORT.is_match(effort))
    {
        bail!("effort must be a provider effort level such as low, medium or high");
    }
    if let Some(directory) = profile.get("claude_config_dir") {
        let usable = directory
            .as_str()
            .is_some_and(|dir| !dir.trim().is_empty() && !dir.contains('\0'));
        if engine != "claude" || !usable {
            bail!("claude_config_dir must be a nonempty Claude configuration path");
        }
    }
    if engine == "openai" {
        let url = urlsplit(
            profile
                .get("base_url")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let hostname = url.hostname.clone().unwrap_or_default();
        let bad = !matches!(url.scheme.as_str(), "http" | "https")
            || hostname.is_empty()
            || url.username.as_deref().is_some_and(|name| !name.is_empty())
            || url.password.as_deref().is_some_and(|name| !name.is_empty())
            || !url.query.is_empty()
            || !url.fragment.is_empty();
        if bad {
            bail!("Use a base URL without credentials, query parameters or fragments");
        }
        if url.scheme == "http" && !matches!(hostname.as_str(), "localhost" | "127.0.0.1" | "::1") {
            bail!("Remote provider URLs must use HTTPS");
        }
        if !KEY_ENV.is_match(profile.get("key_env").and_then(Value::as_str).unwrap_or("")) {
            bail!("key_env must be an environment variable name, never the key itself");
        }
    }
    Ok(())
}

/// Python's Path(text).expanduser().resolve().
fn expand_and_resolve(text: &str) -> String {
    let mut path = PathBuf::from(text);
    if (text == "~" || text.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        path = PathBuf::from(home).join(text.trim_start_matches('~').trim_start_matches('/'));
    }
    if path.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        path = cwd.join(path);
    }
    if let Ok(real) = path.canonicalize() {
        return real.to_string_lossy().into_owned();
    }
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            other => clean.push(other),
        }
    }
    clean.to_string_lossy().into_owned()
}

/// The command and environment for a provider: ([program, args...], full environment).
pub type Launch = (Vec<String>, HashMap<String, String>);

/// Build the provider command. permission is a Donut mode; None means the profile's unattended mode.
///
/// readonly gives no tools at all (run summaries). review allows reading the project but never changing it.
pub fn command(
    profile: &Value,
    prompt: &str,
    readonly: bool,
    permission: Option<&str>,
    review: bool,
) -> Result<Launch> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let which = |name: &str| {
        which::which(name)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    };
    command_with(profile, prompt, readonly, permission, review, &env, &which)
}

/// command() with the environment and the program lookup given, for tests and callers that own them.
pub fn command_with(
    profile: &Value,
    _prompt: &str,
    readonly: bool,
    permission: Option<&str>,
    review: bool,
    environment: &HashMap<String, String>,
    which: &dyn Fn(&str) -> Option<String>,
) -> Result<Launch> {
    validate(profile)?;
    let engine = profile["engine"].as_str().unwrap_or_default();
    let model = text(profile, "model");
    let permission = match permission {
        Some(mode) if !mode.is_empty() => mode.to_string(),
        _ => batch_permission(profile),
    };
    let mut env = environment.clone();
    if engine == "claude"
        && let Some(directory) = text(profile, "claude_config_dir")
    {
        env.insert("CLAUDE_CONFIG_DIR".into(), expand_and_resolve(directory));
    }
    let mut args: Vec<String> = Vec::new();
    let mut add = |items: &[&str]| args.extend(items.iter().map(|item| item.to_string()));
    if engine == "codex" {
        add(&[
            "codex",
            "exec",
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
        ]);
        if readonly || review {
            add(&["--sandbox", "read-only"]);
        } else if permission == "full" {
            add(&["--dangerously-bypass-approvals-and-sandbox"]);
        } else if permission == "auto" {
            add(&["--approve-for-me"]);
        } else {
            add(&["--sandbox", "workspace-write"]);
        }
        if let Some(model) = model {
            add(&["--model", model]);
        }
        if let Some(effort) = text(profile, "effort") {
            add(&["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
        }
        add(&["-"]);
    } else if engine == "claude" {
        add(&[
            "claude",
            "--print",
            "--verbose",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--no-session-persistence",
        ]);
        // With a fixed tool list, Claude Code cannot defer MCP tools: every connector's full definition goes into
        // the prompt (over 100,000 tokens with a few connectors). These calls use none, so they load none.
        if readonly {
            // A summary works from the evidence in its prompt alone: no tools, settings, plugins or connectors.
            add(&[
                "--permission-mode",
                "plan",
                "--tools",
                "",
                "--setting-sources",
                "",
                "--strict-mcp-config",
            ]);
        } else if review {
            add(&[
                "--tools",
                "Read,Grep,Glob",
                "--permission-prompts",
                "none",
                "--strict-mcp-config",
            ]);
        } else if permission == "full" {
            add(&["--dangerously-skip-permissions"]);
        } else if let NativePermission::Mode(mode) = native_permission("claude", &permission) {
            add(&["--permission-mode", mode, "--permission-prompts", "none"]);
        }
        if let Some(model) = model {
            add(&["--model", model]);
        }
        if let Some(effort) = text(profile, "effort") {
            add(&["--effort", effort]);
        }
        if environment.get("DONUT_CHROME").map(String::as_str) == Some("1") && !(readonly || review)
        {
            add(&["--chrome"]); // /chrome is on: workers get the Claude in Chrome tools too.
        }
        add(&["Follow the request on stdin."]);
    } else {
        add(&[
            "qwen",
            "--auth-type",
            "openai",
            "--model",
            model.unwrap_or(""),
            "--output-format",
            "stream-json",
            "--include-partial-messages",
        ]);
        if readonly {
            add(&["--approval-mode", "plan", "--max-tool-calls", "0"]);
        } else if review {
            add(&["--approval-mode", "plan"]);
        } else if permission == "full" {
            add(&["--yolo"]);
        } else if let NativePermission::Mode(mode) = native_permission("qwen", &permission) {
            add(&["--approval-mode", mode]);
        }
        if engine == "ollama" {
            env.insert("OPENAI_API_KEY".into(), "ollama".into());
            add(&[
                "--openai-base-url",
                &ollama_base_url_for(environment.get("OLLAMA_HOST").map_or("", String::as_str)),
            ]);
        } else {
            let key_env = text(profile, "key_env").unwrap_or_default();
            let key = environment.get(key_env).filter(|key| !key.is_empty());
            let Some(key) = key else {
                bail!("Set {key_env} before using this alias (Donut never saves its value).");
            };
            env.insert("OPENAI_API_KEY".into(), key.clone());
            add(&[
                "--openai-base-url",
                text(profile, "base_url").unwrap_or_default(),
            ]);
        }
        add(&["--prompt", "Follow the request on stdin."]);
    }
    let Some(binary) = which(&args[0]) else {
        bail!("Install {} before using this alias.", args[0]);
    };
    args[0] = binary;
    Ok((args, env))
}

/// The OpenAI-compatible endpoint of the local Ollama server, honoring OLLAMA_HOST like the Ollama CLI does.
pub fn ollama_base_url() -> String {
    ollama_base_url_for(&std::env::var("OLLAMA_HOST").unwrap_or_default())
}

/// ollama_base_url() for a given OLLAMA_HOST value.
pub fn ollama_base_url_for(host: &str) -> String {
    let host = host.trim();
    let mut host = if host.is_empty() {
        "127.0.0.1:11434".to_string()
    } else {
        host.to_string()
    };
    if !host.contains("://") {
        host = format!("http://{host}");
    }
    if urlsplit(&host).port.is_none_or(|port| port == 0) {
        host = format!("{}:11434", host.trim_end_matches('/'));
    }
    format!("{}/v1", host.trim_end_matches('/'))
}

/// What an `ollama` command printed and how it ended.
#[derive(Debug, Clone, Default)]
pub struct OllamaRun {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn run_ollama(args: &[&str]) -> Result<OllamaRun> {
    let output = std::process::Command::new("ollama")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(OllamaRun {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run an Ollama command quietly. Its progress spinner goes to stderr, so keep stderr for errors only.
fn ollama(run: &mut dyn FnMut(&[&str]) -> Result<OllamaRun>, args: &[&str]) -> Result<String> {
    let result = run(args)?;
    if result.code != 0 {
        let cleaned = OLLAMA_NOISE.replace_all(&result.stderr, "");
        let detail = cleaned.trim().lines().last().map(str::to_string);
        bail!(
            "ollama {} failed: {}",
            args[0],
            detail.unwrap_or_else(|| format!("exit {}", result.code))
        );
    }
    Ok(result.stdout)
}

/// Load the alias's Ollama model. Other loaded models are stopped only when the alias opts in.
pub fn activate(profile: &Value) -> Result<()> {
    activate_with(profile, &mut run_ollama)
}

/// activate() with the way to run `ollama` given, for tests.
pub fn activate_with(
    profile: &Value,
    run: &mut dyn FnMut(&[&str]) -> Result<OllamaRun>,
) -> Result<()> {
    if profile["engine"].as_str() != Some("ollama") {
        return Ok(());
    }
    let model = text(profile, "model").unwrap_or_default();
    let unload = profile
        .get("unload_others")
        .is_some_and(|value| value.as_bool().unwrap_or(!value.is_null()));
    if unload {
        let listing = ollama(run, &["ps"])?;
        for line in listing.lines().skip(1) {
            if let Some(name) = line.split_whitespace().next()
                && name != model
            {
                ollama(run, &["stop", name])?;
            }
        }
    }
    // Ollama's CLI sets a per-request residency timeout. Subsequent Qwen requests
    // use the server default; setup documents OLLAMA_KEEP_ALIVE=1h for that server.
    ollama(run, &["run", model, "--keepalive", "1h", ""])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    #[test]
    fn test_base_url_follows_ollama_host() {
        for (value, expected) in [
            ("", "http://127.0.0.1:11434/v1"),
            ("0.0.0.0", "http://0.0.0.0:11434/v1"),
            ("gpu-box:11435", "http://gpu-box:11435/v1"),
            (
                "https://ollama.example.com:443/",
                "https://ollama.example.com:443/v1",
            ),
        ] {
            assert_eq!(ollama_base_url_for(value), expected);
        }
    }

    fn run_with(
        results: Vec<OllamaRun>,
    ) -> (
        RefCell<Vec<Vec<String>>>,
        RefCell<std::vec::IntoIter<OllamaRun>>,
    ) {
        (RefCell::new(Vec::new()), RefCell::new(results.into_iter()))
    }

    #[test]
    fn test_ollama_spinner_never_reaches_output_and_errors_are_readable() {
        let ok = OllamaRun {
            code: 0,
            stdout: String::new(),
            stderr: "\u{280b} \u{2819} \u{2839} ".into(),
        };
        let bad = OllamaRun {
            code: 1,
            stdout: String::new(),
            stderr: "\u{280b} \x1b[?25l\u{2819} \nError: model 'm' not found\n".into(),
        };
        let (calls, results) = run_with(vec![ok, bad]);
        let mut run = |args: &[&str]| -> Result<OllamaRun> {
            calls
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect());
            Ok(results.borrow_mut().next().unwrap())
        };
        let profile = json!({"engine": "ollama", "model": "m"});
        activate_with(&profile, &mut run).unwrap();
        let error = activate_with(&profile, &mut run).unwrap_err();
        assert_eq!(
            error.to_string(),
            "ollama run failed: Error: model 'm' not found"
        );
    }

    #[test]
    fn test_other_models_are_only_unloaded_when_asked() {
        let calls = RefCell::new(Vec::new());
        let mut run = |args: &[&str]| -> Result<OllamaRun> {
            calls
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
            Ok(OllamaRun {
                code: 0,
                stdout: "NAME ID\nother:7b x\nmine:7b y\n".into(),
                stderr: String::new(),
            })
        };
        activate_with(&json!({"engine": "ollama", "model": "mine:7b"}), &mut run).unwrap();
        let heads: Vec<Vec<String>> = calls
            .borrow()
            .iter()
            .map(|call| call[..2].to_vec())
            .collect();
        assert_eq!(heads, [["run", "mine:7b"]]);
        calls.borrow_mut().clear();
        activate_with(
            &json!({"engine": "ollama", "model": "mine:7b", "unload_others": true}),
            &mut run,
        )
        .unwrap();
        let calls = calls.borrow();
        let heads: Vec<&str> = calls.iter().map(|call| call[0].as_str()).collect();
        assert_eq!(heads, ["ps", "stop", "run"]);
        assert_eq!(calls[1], ["stop", "other:7b"]);
    }

    fn build(profile: Value, permission: Option<&str>) -> Vec<String> {
        let env: HashMap<String, String> = [("KEY".to_string(), "x".to_string())].into();
        let which = |_: &str| Some("/bin/agent".to_string());
        command_with(&profile, "task", false, permission, false, &env, &which)
            .unwrap()
            .0
    }

    fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
        &args[args.iter().position(|a| a == flag).unwrap() + 1]
    }

    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    #[test]
    fn test_each_mode_uses_the_providers_own_mechanism() {
        let claude = json!({"engine": "claude", "model": "x"});
        let codex = json!({"engine": "codex", "model": "x"});
        let qwen = json!({"engine": "ollama", "model": "x"});
        assert_eq!(
            after(&build(claude.clone(), Some("auto")), "--permission-mode"),
            "auto"
        );
        assert!(has(
            &build(claude.clone(), Some("full")),
            "--dangerously-skip-permissions"
        ));
        assert_eq!(
            after(&build(claude, Some("accept_edits")), "--permission-mode"),
            "acceptEdits"
        );
        assert!(has(&build(codex.clone(), Some("auto")), "--approve-for-me"));
        assert!(has(
            &build(codex.clone(), Some("full")),
            "--dangerously-bypass-approvals-and-sandbox"
        ));
        assert_eq!(
            after(&build(codex, Some("accept_edits")), "--sandbox"),
            "workspace-write"
        );
        assert_eq!(
            after(&build(qwen.clone(), Some("auto")), "--approval-mode"),
            "auto"
        );
        assert!(has(&build(qwen, Some("full")), "--yolo"));
    }

    #[test]
    fn test_unattended_runs_never_wait_for_a_prompt() {
        assert_eq!(
            batch_permission(&json!({"permission_mode": "ask"})),
            "accept_edits"
        );
        assert_eq!(batch_permission(&json!({"auto_approve": true})), "full");
        assert_eq!(batch_permission(&json!({})), "accept_edits");
        assert_eq!(
            batch_permission(&json!({"permission_mode": "auto", "auto_approve": true})),
            "auto"
        );
        assert!(validate(&json!({"engine": "claude", "permission_mode": "yolo"})).is_err());
    }

    #[test]
    fn test_validate_checks_engine_url_and_key_name() {
        assert!(validate(&json!({"engine": "bogus"})).is_err());
        assert!(validate(&json!({"engine": "ollama"})).is_err());
        let openai = |url: &str, key: &str| json!({"engine": "openai", "model": "m", "base_url": url, "key_env": key});
        assert!(validate(&openai("https://example.com/v1", "KEY")).is_ok());
        assert!(validate(&openai("http://localhost:8080/v1", "KEY")).is_ok());
        assert!(validate(&openai("http://example.com/v1", "KEY")).is_err());
        assert!(validate(&openai("https://user:pw@example.com/v1", "KEY")).is_err());
        assert!(validate(&openai("https://example.com/v1?x=1", "KEY")).is_err());
        assert!(validate(&openai("https://example.com/v1", "sk-secret value")).is_err());
        assert!(validate(&json!({"engine": "codex", "claude_config_dir": "/x"})).is_err());
    }

    #[test]
    fn test_urlsplit_matches_python() {
        let url = urlsplit("https://User:pw@Example.COM:8443/a/b?q=1#frag");
        assert_eq!(url.scheme, "https");
        assert_eq!(
            (url.username.as_deref(), url.password.as_deref()),
            (Some("User"), Some("pw"))
        );
        assert_eq!(
            (url.hostname.as_deref(), url.port),
            (Some("example.com"), Some(8443))
        );
        assert_eq!((url.query.as_str(), url.fragment.as_str()), ("q=1", "frag"));
        assert_eq!(urlsplit("http://[::1]:9/").hostname.as_deref(), Some("::1"));
    }
}
