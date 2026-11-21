//! `donut setup`, model discovery and shell alias import.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::config::{load, save};
use crate::conversations::resolve;
use crate::install_skills::install;
use crate::plugins::home_dir;
use crate::providers::validate;
use crate::runner::{emit, read_line_prompt, sayln};
use crate::ui::{paint, release_version, welcome_text};

static ALIAS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]*$").unwrap());
static ALIAS_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*alias\s+").unwrap());
static OLLAMA_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.:/-]*$").unwrap());

/// What a program run printed and how it ended.
pub struct RunOutput {
    pub code: i32,
    pub stdout: String,
}

/// What setup asks of the machine: typed answers, programs and the skills install. Tests give their own where
/// Python patched `input`, `shutil.which`, `subprocess.run` and the module's own functions.
pub trait Host {
    /// A typed answer, or None at the end of input.
    fn input(&mut self, prompt: &str) -> Option<String> {
        read_line_prompt(prompt)
    }

    fn which(&mut self, name: &str) -> Option<PathBuf> {
        which::which(name).ok()
    }

    /// Run a program. env, when given, is its whole environment. capture keeps its output; check turns a failing
    /// exit into an error.
    fn run(
        &mut self,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        capture: bool,
        check: bool,
    ) -> Result<RunOutput> {
        run_program(args, env, capture, check)
    }

    fn discover(&mut self, root: Option<&str>) -> Result<(Vec<String>, Vec<String>)> {
        discover(root)
    }

    fn shell_aliases(&mut self) -> Map<String, Value> {
        shell_aliases(None)
    }

    fn install(&mut self, project: &Path) -> Result<()> {
        install(project, &home_dir(), false, None).map(|_| ())
    }
}

/// The real machine.
pub struct System;

impl Host for System {}

/// subprocess.run: with capture, a 15 second limit.
fn run_program(
    args: &[String],
    env: Option<&HashMap<String, String>>,
    capture: bool,
    check: bool,
) -> Result<RunOutput> {
    let mut command = Command::new(&args[0]);
    command.args(&args[1..]);
    if let Some(env) = env {
        command.env_clear().envs(env);
    }
    if !capture {
        let status = command.status()?;
        let code = status.code().unwrap_or(-1);
        if check && code != 0 {
            bail!(
                "Command '{}' returned non-zero exit status {code}.",
                args.join(" ")
            );
        }
        return Ok(RunOutput {
            code,
            stdout: String::new(),
        });
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut out = child.stdout.take().expect("stdout is piped");
    let mut err = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut bytes);
        bytes
    });
    let drain = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut bytes);
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Command '{}' timed out after 15 seconds", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = crate::checkpoints::universal_newlines(&reader.join().unwrap_or_default());
    let _ = drain.join();
    let code = status.code().unwrap_or(-1);
    if check && code != 0 {
        bail!(
            "Command '{}' returned non-zero exit status {code}.",
            args.join(" ")
        );
    }
    Ok(RunOutput { code, stdout })
}

/// The first word of each line after the header of `ollama list`.
fn model_names(listing: &str) -> Vec<String> {
    crate::tickets::splitlines(listing)
        .into_iter()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect()
}

fn ask<H: Host + ?Sized>(host: &mut H, label: &str, default: &str) -> Result<String> {
    let mut prompt = paint(&format!("  {label}"), "gray");
    if !default.is_empty() {
        prompt += &paint(&format!(" [{default}]"), "orange");
    }
    prompt += &paint(" > ", "teal");
    let Some(line) = host.input(&prompt) else {
        bail!("EOF when reading a line")
    };
    let answer = line.trim();
    Ok(if answer.is_empty() {
        default.to_string()
    } else {
        answer.to_string()
    })
}

fn yes<H: Host + ?Sized>(host: &mut H, label: &str, default: bool) -> Result<bool> {
    let answer = ask(
        host,
        &format!("{label} (y/n)"),
        if default { "y" } else { "n" },
    )?
    .to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn is_decimal(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

fn choose<H: Host + ?Sized>(
    host: &mut H,
    label: &str,
    options: &[String],
    default: Option<&str>,
) -> Result<String> {
    for (index, option) in options.iter().enumerate() {
        sayln!(
            "{}{option}",
            paint(&format!("  {:>2}  ", index + 1), "teal")
        );
    }
    loop {
        let fallback = default.filter(|d| !d.is_empty()).unwrap_or(&options[0]);
        let value = ask(host, &format!("{label} (number or name)"), fallback)?;
        if is_decimal(&value)
            && let Ok(number) = value.parse::<usize>()
            && (1..=options.len()).contains(&number)
        {
            return Ok(options[number - 1].clone());
        }
        if options.contains(&value) {
            return Ok(value);
        }
        sayln!("{}", paint("Choose one of the listed options.", "coral"));
    }
}

/// shlex.split in POSIX mode: words split at spaces, with quotes and backslashes. comments ends the line at an
/// unquoted `#`. None when a quote or an escape is left open.
fn shlex_split(line: &str, comments: bool) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Space,
        Word,
        Single,
        Double,
        EscapeWord,
        EscapeDouble,
    }
    let mut words = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut state = State::Space;
    for c in line.chars() {
        match state {
            State::Space | State::Word => {
                if " \t\r\n".contains(c) {
                    if state == State::Word {
                        words.push(std::mem::take(&mut token));
                        quoted = false;
                        state = State::Space;
                    }
                } else if comments && c == '#' {
                    if state == State::Word && (!token.is_empty() || quoted) {
                        words.push(std::mem::take(&mut token));
                    }
                    return Some(words);
                } else if c == '\'' {
                    state = State::Single;
                } else if c == '"' {
                    state = State::Double;
                } else if c == '\\' {
                    state = State::EscapeWord;
                } else {
                    token.push(c);
                    state = State::Word;
                }
            }
            State::Single => {
                if c == '\'' {
                    quoted = true;
                    state = State::Word;
                } else {
                    token.push(c);
                }
            }
            State::Double => {
                if c == '"' {
                    quoted = true;
                    state = State::Word;
                } else if c == '\\' {
                    state = State::EscapeDouble;
                } else {
                    token.push(c);
                }
            }
            State::EscapeWord => {
                token.push(c);
                state = State::Word;
            }
            State::EscapeDouble => {
                if c != '"' && c != '\\' {
                    token.push('\\');
                }
                token.push(c);
                state = State::Double;
            }
        }
    }
    match state {
        State::Space => {}
        State::Word => words.push(token),
        _ => return None,
    }
    Some(words)
}

/// Import simple provider aliases without sourcing or executing shell code.
pub fn shell_aliases(home: Option<&Path>) -> Map<String, Value> {
    let home = home.map(Path::to_path_buf).unwrap_or_else(home_dir);
    let mut aliases = Map::new();
    for filename in [".bash_profile", ".bashrc", ".zshrc"] {
        let path = home.join(filename);
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        for line in crate::tickets::splitlines(&content) {
            if !ALIAS_LINE.is_match(line) {
                continue;
            }
            if let Some((name, profile)) = parse_alias(line, &home) {
                aliases.insert(name, profile);
            }
        }
    }
    aliases
}

fn parse_alias(line: &str, home: &Path) -> Option<(String, Value)> {
    let declaration = shlex_split(line, true)?;
    if declaration.len() != 2 {
        return None;
    }
    let (name, value) = declaration[1].split_once('=')?;
    if !ALIAS.is_match(name) {
        return None;
    }
    let mut words = shlex_split(value, false)?;
    if words.first().map(String::as_str) == Some("env") {
        words.remove(0);
    }
    let mut directory: Option<String> = None;
    if words
        .first()
        .is_some_and(|word| word.starts_with("CLAUDE_CONFIG_DIR="))
    {
        directory = words
            .remove(0)
            .split_once('=')
            .map(|(_, value)| value.to_string());
    }
    if words.len() != 1 || !matches!(words[0].as_str(), "claude" | "codex") {
        return None;
    }
    if directory.is_some() && words[0] != "claude" {
        return None;
    }
    let mut profile = json!({"engine": words[0], "model": ""});
    if let Some(directory) = directory {
        let shown = home.to_string_lossy();
        let mut directory = directory
            .replace("${HOME}", &shown)
            .replace("$HOME", &shown);
        if let Some(rest) = directory.strip_prefix("~/") {
            directory = home.join(rest).to_string_lossy().into_owned();
        }
        if directory.is_empty()
            || directory.chars().any(|c| "$`\n\0".contains(c))
            || !Path::new(&directory).is_absolute()
        {
            return None;
        }
        profile["claude_config_dir"] = json!(resolve(Path::new(&directory)).to_string_lossy());
    }
    Some((name.to_string(), profile))
}

fn expanduser(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

fn claude_directory<H: Host + ?Sized>(host: &mut H) -> Result<String> {
    let home = home_dir();
    let configured = std::env::var("CLAUDE_CONFIG_DIR")
        .unwrap_or_else(|_| home.join(".claude").to_string_lossy().into_owned());
    let current = resolve(&expanduser(&configured))
        .to_string_lossy()
        .into_owned();
    let mut extra: Vec<PathBuf> = std::fs::read_dir(&home)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(".claude-"))
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    extra.sort();
    let mut paths: Vec<String> = Vec::new();
    for path in [
        current.clone(),
        home.join(".claude").to_string_lossy().into_owned(),
    ]
    .into_iter()
    .chain(extra.iter().map(|p| p.to_string_lossy().into_owned()))
    {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    sayln!("  Claude login is selected by its configuration folder, not the Donut alias.");
    paths.push("Custom path".to_string());
    let mut selected = choose(host, "Claude login folder", &paths, Some(&current))?;
    if selected == "Custom path" {
        selected = ask(
            host,
            "Configuration folder (existing login or a new folder)",
            "",
        )?;
        if selected.is_empty() {
            bail!("A configuration folder is required");
        }
    }
    Ok(resolve(&expanduser(&selected))
        .to_string_lossy()
        .into_owned())
}

/// Ollama models and model files in the usual places (or in root).
pub fn discover(root: Option<&str>) -> Result<(Vec<String>, Vec<String>)> {
    let mut models = Vec::new();
    if which::which("ollama").is_ok() {
        let result = run_program(
            &["ollama".to_string(), "list".to_string()],
            None,
            true,
            false,
        )?;
        if result.code == 0 {
            models = model_names(&result.stdout);
        }
    }
    Ok((models, discover_files(root, &home_dir())))
}

/// Model files by name only; never reads unrelated user documents or weights.
pub fn discover_files(root: Option<&str>, home: &Path) -> Vec<String> {
    let roots: Vec<PathBuf> = match root.filter(|r| !r.is_empty()) {
        Some(root) => vec![expanduser(root)],
        None => vec![
            home.join("Models"),
            home.join("Downloads"),
            home.join(".cache/huggingface/hub"),
            home.join(".lmstudio/models"),
            home.join(".ollama/models"),
        ],
    };
    let mut files: Vec<String> = Vec::new();
    for base in roots {
        if base.is_dir() {
            walk(&base, &mut files);
        }
    }
    files.sort();
    files.dedup();
    files
}

fn walk(folder: &Path, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.path().is_dir();
        let is_link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
        if is_dir {
            if !is_link && !matches!(name.as_str(), ".git" | "node_modules" | ".venv") {
                dirs.push(entry.path());
            }
        } else {
            names.push(name);
        }
    }
    for name in &names {
        if name.to_lowercase().ends_with(".gguf") {
            files.push(folder.join(name).to_string_lossy().into_owned());
        } else if name == "config.json" && names.iter().any(|n| n.ends_with(".safetensors")) {
            files.push(folder.to_string_lossy().into_owned());
        }
    }
    for dir in dirs {
        walk(&dir, files);
    }
}

fn add_alias<H: Host + ?Sized>(
    host: &mut H,
    config: &mut Value,
    profile: &Value,
    suggested: &str,
) -> Result<Option<String>> {
    loop {
        let alias = ask(
            host,
            if suggested.is_empty() {
                "Donut alias (Enter skips)"
            } else {
                "Donut alias (Enter keeps suggestion)"
            },
            suggested,
        )?;
        if alias.is_empty() {
            return Ok(None);
        }
        if !ALIAS.is_match(&alias) {
            sayln!("Use letters, numbers, hyphens or underscores.");
            continue;
        }
        if config["aliases"].get(&alias).is_some()
            && !yes(host, &format!("Replace alias {alias}?"), false)?
        {
            return Ok(None);
        }
        validate(profile)?;
        config["aliases"][&alias] = profile.clone();
        save(config)?;
        return Ok(Some(alias));
    }
}

fn configure_provider<H: Host + ?Sized>(
    host: &mut H,
    config: &mut Value,
    preset: Option<&Value>,
    suggested: Option<&str>,
) -> Result<()> {
    let engine = match preset {
        Some(preset) => preset["engine"].as_str().unwrap_or("").to_string(),
        None => {
            let options: Vec<String> = ["codex", "claude", "ollama", "grok", "openai"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            choose(host, "Provider", &options, Some("codex"))?
        }
    };
    let mut profile;
    if engine == "ollama" {
        for binary in ["ollama", "qwen"] {
            if host.which(binary).is_none() {
                bail!("Install {binary} to use a local orchestrator.");
            }
        }
        let result = host.run(
            &["ollama".to_string(), "list".to_string()],
            None,
            true,
            true,
        )?;
        let models = model_names(&result.stdout);
        if models.is_empty() {
            bail!("No Ollama models found. Pull or import a model, then rerun setup.");
        }
        let model = choose(host, "Local model", &models, None)?;
        if !models.contains(&model) {
            bail!("Select an installed model from the list.");
        }
        profile = json!({"engine": "ollama", "model": model});
        profile["unload_others"] = json!(yes(
            host,
            "Unload other Ollama models before this alias runs? (frees VRAM; disruptive on shared servers)",
            false
        )?);
    } else if engine == "codex" || engine == "claude" {
        if host.which(&engine).is_none() {
            bail!("Install {engine}, then rerun donut setup.");
        }
        let mut directory = preset
            .and_then(|p| p.get("claude_config_dir"))
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        if directory.is_none() && engine == "claude" {
            directory = Some(claude_directory(host)?).filter(|d| !d.is_empty());
        }
        if let Some(directory) = &directory {
            sayln!("  Claude login folder: {directory}");
        }
        let mut login_env: HashMap<String, String> = std::env::vars().collect();
        if let Some(directory) = &directory {
            login_env.insert("CLAUDE_CONFIG_DIR".into(), directory.clone());
        }
        let executable = host
            .which(&engine)
            .map_or_else(|| "None".to_string(), |p| p.to_string_lossy().into_owned());
        sayln!("  Executable: {executable}");
        if yes(
            host,
            &format!("Run {engine} login now? Choose no to reuse this login"),
            false,
        )? {
            let command: Vec<String> = if engine == "codex" {
                vec!["codex".into(), "login".into()]
            } else {
                vec!["claude".into(), "auth".into(), "login".into()]
            };
            host.run(&command, Some(&login_env), false, true)?;
        }
        let model = ask(host, "Planner model ID (blank uses provider default)", "")?;
        profile = json!({"engine": engine, "model": model});
        if let Some(directory) = directory {
            profile["claude_config_dir"] = json!(directory);
        }
    } else if engine == "grok" || engine == "openai" {
        if host.which("qwen").is_none() {
            bail!("Install Qwen Code to use OpenAI-compatible endpoints.");
        }
        sayln!(
            "API authentication: set the provider key in your shell or secret manager. Do not paste it here."
        );
        let base = ask(
            host,
            "OpenAI-compatible base URL",
            if engine == "grok" {
                "https://api.x.ai/v1"
            } else {
                ""
            },
        )?;
        let env = ask(
            host,
            "Environment variable containing the API key",
            if engine == "grok" {
                "XAI_API_KEY"
            } else {
                "OPENAI_API_KEY"
            },
        )?;
        let model = ask(host, "Planner model ID (exact provider ID)", "")?;
        profile = json!({"engine": "openai", "model": model, "base_url": base, "key_env": env});
        validate(&profile)?;
        if std::env::var(&env).unwrap_or_default().is_empty() {
            sayln!("Authentication pending: export {env} before running this alias.");
        }
    } else {
        bail!("Choose ollama, codex, claude, grok, or openai.");
    }
    profile["auto_approve"] = json!(yes(
        host,
        "Allow this agent to run tools without approval in projects you select?",
        false
    )?);
    sayln!("  An alias is a nickname for these settings; it does not change your account.");
    if let Some(alias) = add_alias(host, config, &profile, suggested.unwrap_or(&engine))? {
        config["planner"] = json!(alias);
        if config.get("default_model").is_none() {
            config["default_model"] = json!(alias);
        }
        save(config)?;
    }
    Ok(())
}

/// Interactive setup of the planner, the worker models and the project.
pub fn setup(project: &Path, root: Option<&str>) -> Result<()> {
    setup_with(&mut System, project, root)
}

fn alias_names(config: &Value) -> Vec<String> {
    config["aliases"]
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn setup_with<H: Host + ?Sized>(
    host: &mut H,
    project: &Path,
    root: Option<&str>,
) -> Result<()> {
    emit(&welcome_text(
        false,
        &release_version(),
        crate::runner::colored(),
    ));
    sayln!("  Planner: writes tickets. Worker: builds one ticket per fresh context.");
    sayln!("  One model can do both jobs. Existing aliases are kept unless you replace them.");
    sayln!(
        "{}",
        paint(
            &format!("\n  1 / 3  CHOOSE YOUR PLANNER\n  {}", "─".repeat(42)),
            "teal"
        )
    );
    let mut config = load()?;
    let discovered: Map<String, Value> = host
        .shell_aliases()
        .into_iter()
        .filter(|(name, _)| config["aliases"].get(name).is_none())
        .collect();
    if !alias_names(&config).is_empty() || !discovered.is_empty() {
        if !discovered.is_empty() {
            sayln!(
                "  Shell aliases found: {}",
                discovered.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        let mut options = alias_names(&config);
        options.extend(discovered.keys().cloned());
        options.push("Add new provider".to_string());
        let default = config
            .get("planner")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .unwrap_or("Add new provider")
            .to_string();
        let selected = choose(host, "Planner alias", &options, Some(&default))?;
        if selected == "Add new provider" {
            configure_provider(host, &mut config, None, None)?;
        } else if let Some(profile) = discovered.get(&selected) {
            configure_provider(host, &mut config, Some(profile), Some(&selected))?;
        } else {
            config["planner"] = json!(selected);
        }
    } else {
        configure_provider(host, &mut config, None, None)?;
    }
    sayln!(
        "{}",
        paint(
            &format!("\n  2 / 3  ADD WORKER MODELS\n  {}", "─".repeat(42)),
            "teal"
        )
    );
    sayln!("{}", paint("  Discovering local models…", "orange"));
    let (models, files) = host.discover(root)?;
    for (index, model) in models.iter().enumerate() {
        let aliases: Vec<String> = config["aliases"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter(|(_, p)| {
                        p.get("engine").and_then(Value::as_str) == Some("ollama")
                            && p.get("model").and_then(Value::as_str) == Some(model)
                    })
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let shown = if aliases.is_empty() {
            "not configured".to_string()
        } else {
            aliases.join(", ")
        };
        sayln!(
            "{}{model}{}",
            paint(&format!("  {:>2}  ", index + 1), "teal"),
            paint(&format!("  [{shown}]"), "orange")
        );
    }
    if !models.is_empty() {
        sayln!(
            "  Select only the models you want to add. Different names may refer to the same weights."
        );
        let numbers = loop {
            let selected = ask(host, "Model numbers, separated by spaces (Enter skips)", "")?;
            let numbers: Vec<String> = selected.split_whitespace().map(str::to_string).collect();
            let fine = numbers.iter().all(|n| {
                is_decimal(n)
                    && n.parse::<usize>()
                        .is_ok_and(|n| (1..=models.len()).contains(&n))
            });
            if fine {
                break numbers;
            }
            sayln!("{}", paint("Use model numbers from the list.", "coral"));
        };
        let mut seen: Vec<&String> = Vec::new();
        for number in &numbers {
            if seen.contains(&number) {
                continue;
            }
            seen.push(number);
            let model = &models[number.parse::<usize>().unwrap_or(1) - 1];
            sayln!("{}", paint(&format!("  {model}"), "orange"));
            add_alias(
                host,
                &mut config,
                &json!({"engine": "ollama", "model": model, "auto_approve": true}),
                "",
            )?;
        }
    }
    for filename in &files {
        sayln!("\nModel files: {filename}");
        if filename.ends_with(".gguf")
            && host.which("ollama").is_some()
            && yes(host, "Import this GGUF into Ollama?", false)?
        {
            let name = ask(host, "Ollama model name", "")?;
            if !OLLAMA_NAME.is_match(&name) {
                bail!("Invalid Ollama model name");
            }
            if filename.contains('\n') || filename.contains('"') {
                bail!("Unsupported characters in model path");
            }
            let temp = tempfile::tempdir()?;
            let modelfile = temp.path().join("Modelfile");
            std::fs::write(&modelfile, format!("FROM \"{filename}\"\n"))?;
            host.run(
                &[
                    "ollama".into(),
                    "create".into(),
                    name.clone(),
                    "-f".into(),
                    modelfile.to_string_lossy().into_owned(),
                ],
                None,
                false,
                true,
            )?;
            add_alias(
                host,
                &mut config,
                &json!({"engine": "ollama", "model": name, "auto_approve": true}),
                "",
            )?;
        } else if !filename.ends_with(".gguf") {
            sayln!(
                "Safetensors directory found. Serve it through an OpenAI-compatible runtime, then add its endpoint."
            );
        }
    }
    while yes(host, "Add another local or hosted model?", false)? {
        let planner = config.get("planner").cloned().unwrap_or(Value::Null);
        configure_provider(host, &mut config, None, None)?;
        config["planner"] = planner;
        save(&config)?;
    }
    sayln!(
        "{}",
        paint(
            &format!(
                "\n  3 / 3  CHOOSE YOUR DEFAULT WORKER\n  {}",
                "─".repeat(42)
            ),
            "teal"
        )
    );
    let names = alias_names(&config);
    if !names.is_empty() {
        let default = config
            .get("default_model")
            .and_then(Value::as_str)
            .unwrap_or(&names[0])
            .to_string();
        let chosen = choose(host, "Worker alias", &names, Some(&default))?;
        if !names.contains(&chosen) {
            bail!("Unknown default alias");
        }
        config["default_model"] = json!(chosen);
        save(&config)?;
    }
    host.install(project)?;
    sayln!("{}", paint("\n  READY  /  Setup saved", "teal"));
    for (role, key) in [("Planner", "planner"), ("Worker", "default_model")] {
        let Some(alias) = config
            .get(key)
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
        else {
            continue;
        };
        let Some(profile) = config["aliases"].get(alias) else {
            bail!("KeyError: {alias:?}")
        };
        let model = match profile.get("model").and_then(Value::as_str) {
            Some(model) if !model.is_empty() => model.to_string(),
            _ => format!("{} default", profile["engine"].as_str().unwrap_or("")),
        };
        sayln!("  {role:7} {alias}  →  {model}");
        if let Some(folder) = profile
            .get("claude_config_dir")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
        {
            sayln!("          Login folder: {folder}");
        }
    }
    sayln!(
        "For local residency, set OLLAMA_KEEP_ALIVE=1h on your Ollama server. Donut also refreshes it after runs."
    );
    sayln!("{}", paint("\nSETUP COMPLETE", "teal"));
    sayln!("Try running `donut`");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    use super::*;
    use crate::config::{config_path, save as save_config};
    use crate::conversations::env_lock;
    use crate::providers::{command_with, validate};
    use crate::runner::capture;

    struct Fixture {
        _lock: MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        root: PathBuf,
        saved: Option<OsString>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let lock = env_lock();
            let temp = tempfile::tempdir().unwrap();
            let root = resolve(temp.path());
            let saved = std::env::var_os("XDG_CONFIG_HOME");
            // SAFETY: every test that touches the environment holds env_lock.
            unsafe { std::env::set_var("XDG_CONFIG_HOME", root.join("config")) };
            Fixture {
                _lock: lock,
                _temp: temp,
                root,
                saved,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // SAFETY: as above.
            unsafe {
                match &self.saved {
                    Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    type Ran = (Vec<String>, Option<HashMap<String, String>>);

    /// Typed answers in order, programs that exist or not, and a record of what ran.
    #[derive(Default)]
    struct Script {
        answers: VecDeque<String>,
        programs: bool,
        runs: Vec<Ran>,
        listing: String,
        discovered: (Vec<String>, Vec<String>),
        aliases: Map<String, Value>,
        installs: usize,
    }

    impl Script {
        fn answering(answers: &[&str]) -> Script {
            Script {
                answers: answers.iter().map(|a| a.to_string()).collect(),
                programs: true,
                ..Default::default()
            }
        }
    }

    impl Host for Script {
        fn input(&mut self, _prompt: &str) -> Option<String> {
            Some(
                self.answers
                    .pop_front()
                    .expect("setup asked more than the test answered"),
            )
        }

        fn which(&mut self, name: &str) -> Option<PathBuf> {
            self.programs.then(|| PathBuf::from("/bin").join(name))
        }

        fn run(
            &mut self,
            args: &[String],
            env: Option<&HashMap<String, String>>,
            _: bool,
            _: bool,
        ) -> Result<RunOutput> {
            self.runs.push((args.to_vec(), env.cloned()));
            Ok(RunOutput {
                code: 0,
                stdout: self.listing.clone(),
            })
        }

        fn discover(&mut self, _root: Option<&str>) -> Result<(Vec<String>, Vec<String>)> {
            Ok(self.discovered.clone())
        }

        fn shell_aliases(&mut self) -> Map<String, Value> {
            self.aliases.clone()
        }

        fn install(&mut self, _project: &Path) -> Result<()> {
            self.installs += 1;
            Ok(())
        }
    }

    fn install_quiet(project: &Path, home: &Path) {
        install(project, home, true, None).unwrap();
    }

    #[test]
    fn test_install_idempotent_and_generic() {
        let fixture = Fixture::new();
        let root = &fixture.root;
        std::fs::write(root.join("QWEN.md"), "Keep my instructions\n").unwrap();
        install_quiet(root, root);
        let before = std::fs::read(root.join("QWEN.md")).unwrap();
        install_quiet(root, root);
        assert_eq!(before, std::fs::read(root.join("QWEN.md")).unwrap());
        assert!(root.join("skills/donut-planner/SKILL.md").exists());
        assert!(!root.join("AGENTS.md").exists());
        assert!(
            !std::fs::read_to_string(root.join(".donut/skills/swim-lane-worker/SKILL.md"))
                .unwrap()
                .contains("Doodle")
        );
        assert!(
            std::fs::read_to_string(root.join("QWEN.md"))
                .unwrap()
                .contains("@./.donut/skills/swim-lane-worker/SKILL.md")
        );
        assert!(!root.join(".qwen").exists());
    }

    #[test]
    fn test_existing_skills_get_hold_guidance_without_losing_custom_text() {
        let fixture = Fixture::new();
        let root = &fixture.root;
        let worker = root.join(".qwen/skills/swim-lane-worker/SKILL.md");
        let planner = root.join("skills/donut-planner/SKILL.md");
        for file in [&worker, &planner] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "My existing project instructions.\n").unwrap();
        }
        install_quiet(root, root);
        let once: Vec<Vec<u8>> = [&worker, &planner]
            .iter()
            .map(|file| std::fs::read(file).unwrap())
            .collect();
        install_quiet(root, root);
        assert!(root.join("swim_lanes/on_hold/.gitkeep").is_file());
        assert!(root.join("swim_lanes/blocked/.gitkeep").is_file());
        for (index, file) in [&worker, &planner].iter().enumerate() {
            let text = std::fs::read_to_string(file).unwrap();
            assert!(text.starts_with("My existing project instructions."));
            assert!(text.contains("on_hold/"));
            assert_eq!(std::fs::read(file).unwrap(), once[index]);
        }
    }

    #[test]
    fn test_existing_custom_hold_rules_are_preserved() {
        let fixture = Fixture::new();
        let root = &fixture.root;
        let worker = root.join(".qwen/skills/swim-lane-worker/SKILL.md");
        std::fs::create_dir_all(worker.parent().unwrap()).unwrap();
        std::fs::write(&worker, "Custom rules for swim_lanes/on_hold/\n").unwrap();
        let before = std::fs::read(&worker).unwrap();
        install_quiet(root, root);
        assert!(std::fs::read(&worker).unwrap().starts_with(&before));
        assert!(
            std::fs::read_to_string(&worker)
                .unwrap()
                .contains("blocked/")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_linked_skills_are_not_modified() {
        let fixture = Fixture::new();
        let root = &fixture.root;
        let external = root.join("external");
        std::fs::create_dir(&external).unwrap();
        let file = external.join("SKILL.md");
        std::fs::write(&file, "External custom instructions\n").unwrap();
        let dest = root.join(".qwen/skills/swim-lane-worker");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&external, &dest).unwrap();
        install_quiet(root, root);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "External custom instructions\n"
        );
    }

    fn lookup(_: &str) -> Option<String> {
        Some("/bin/qwen".to_string())
    }

    #[test]
    fn test_credentials_not_saved() {
        let _fixture = Fixture::new();
        let profile = json!({"engine": "openai", "model": "remote", "base_url": "https://example.com/v1", "key_env": "TEST_KEY"});
        save_config(&json!({"aliases": {"remote": profile}})).unwrap();
        let mut environment: HashMap<String, String> = std::env::vars().collect();
        environment.insert("TEST_KEY".into(), "secret-fixture".into());
        let (args, env) =
            command_with(&profile, "hello", false, None, false, &environment, &lookup).unwrap();
        let saved = std::fs::read_to_string(config_path(None)).unwrap();
        assert!(!saved.contains("secret-fixture"));
        assert!(!args.join(" ").contains("secret-fixture"));
        assert_eq!(env["OPENAI_API_KEY"], "secret-fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(config_path(None))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let mut bad = profile.clone();
        bad["api_key"] = json!("secret-fixture");
        assert!(save_config(&json!({"aliases": {"bad": bad}})).is_err());
    }

    #[test]
    fn test_credential_urls_rejected() {
        for url in [
            "https://user:password@example.com/v1",
            "https://example.com/v1?api_key=x",
            "http://example.com/v1",
        ] {
            assert!(
                validate(
                    &json!({"engine": "openai", "model": "x", "base_url": url, "key_env": "KEY"})
                )
                .is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn test_discover_only_model_locations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("model.gguf"), b"").unwrap();
        std::fs::write(root.join("private.txt"), "not read").unwrap();
        let files = discover_files(Some(&root.to_string_lossy()), root);
        assert_eq!(
            files,
            [root.join("model.gguf").to_string_lossy().into_owned()]
        );
    }

    fn finished(output: &str) -> bool {
        output.ends_with("SETUP COMPLETE\nTry running `donut`\n")
    }

    #[test]
    fn test_interactive_setup() {
        let fixture = Fixture::new();
        let mut host = Script::answering(&["codex", "n", "", "n", "planner", "", "n", "planner"]);
        host.discovered = (vec!["local-model".to_string()], Vec::new());
        let (result, output) = capture(|| setup_with(&mut host, &fixture.root, None));
        result.unwrap();
        assert!(finished(&output), "{output}");
        let saved = load().unwrap();
        assert_eq!(saved["planner"], "planner");
        assert_eq!(saved["aliases"]["planner"]["engine"], "codex");
    }

    #[test]
    fn test_local_orchestrator_setup_never_requests_cloud_login() {
        let fixture = Fixture::new();
        let mut host = Script::answering(&[
            "ollama",
            "local-model",
            "n",
            "y",
            "local-planner",
            "",
            "n",
            "local-planner",
        ]);
        host.listing = "NAME ID SIZE MODIFIED\nlocal-model abc 1GB now\n".to_string();
        host.discovered = (vec!["local-model".to_string()], Vec::new());
        let (result, output) = capture(|| setup_with(&mut host, &fixture.root, None));
        result.unwrap();
        assert!(finished(&output), "{output}");
        let saved = load().unwrap();
        assert_eq!(saved["planner"], "local-planner");
        assert_eq!(saved["aliases"]["local-planner"]["engine"], "ollama");
        let ran: Vec<Vec<String>> = host.runs.iter().map(|(args, _)| args.clone()).collect();
        assert_eq!(ran, [["ollama", "list"]]);
    }

    #[test]
    fn test_claude_login_folder_used_for_login_and_execution() {
        let fixture = Fixture::new();
        let personal = resolve(&fixture.root.join(".claude-personal"))
            .to_string_lossy()
            .into_owned();
        let mut host = Script::answering(&[
            "claude",
            "Custom path",
            &personal,
            "y",
            "opus",
            "n",
            "personal",
        ]);
        let mut config = json!({"aliases": {}});
        let (result, _) = capture(|| configure_provider(&mut host, &mut config, None, None));
        result.unwrap();
        let profile = load().unwrap()["aliases"]["personal"].clone();
        let environment: HashMap<String, String> = std::env::vars().collect();
        let (_, env) =
            command_with(&profile, "test", false, None, false, &environment, &lookup).unwrap();
        let login_env = host.runs[0].1.as_ref().unwrap();
        assert_eq!(login_env["CLAUDE_CONFIG_DIR"], personal);
        assert_eq!(env["CLAUDE_CONFIG_DIR"], personal);
        assert_eq!(profile["claude_config_dir"], personal);
    }

    #[test]
    fn test_existing_aliases_preserved_when_selecting_roles() {
        let fixture = Fixture::new();
        let aliases =
            json!({"planner": {"engine": "codex"}, "fast": {"engine": "ollama", "model": "local"}});
        save_config(&json!({"aliases": aliases, "planner": "planner", "default_model": "fast"}))
            .unwrap();
        let mut host = Script::answering(&["1", "", "n", "2"]);
        host.programs = false;
        host.discovered = (vec!["local".to_string()], Vec::new());
        let (result, _) = capture(|| setup_with(&mut host, &fixture.root, None));
        result.unwrap();
        assert_eq!(load().unwrap()["aliases"], aliases);
        assert_eq!(load().unwrap()["default_model"], "fast");
    }

    #[test]
    fn test_claude_directory_rejects_wrong_provider_and_invalid_path() {
        for profile in [
            json!({"engine": "codex", "claude_config_dir": "/tmp/x"}),
            json!({"engine": "claude", "claude_config_dir": ""}),
            json!({"engine": "claude", "claude_config_dir": 42}),
        ] {
            assert!(validate(&profile).is_err(), "{profile}");
        }
    }

    #[test]
    fn test_shell_alias_discovery_does_not_execute_code() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.root.join(".zshrc"),
            "alias claude-personal='CLAUDE_CONFIG_DIR=\"$HOME/.claude-personal\" claude'\n\
             alias coding='codex'\n\
             alias unsafe='CLAUDE_CONFIG_DIR=$(touch /tmp/no) claude'\n\
             alias chained='claude; echo no'\n\
             alias key='API_KEY=secret claude'\n",
        )
        .unwrap();
        let found = shell_aliases(Some(&fixture.root));
        let mut names: Vec<&String> = found.keys().collect();
        names.sort();
        assert_eq!(names, ["claude-personal", "coding"]);
        assert_eq!(
            found["claude-personal"]["claude_config_dir"],
            resolve(&fixture.root.join(".claude-personal"))
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn test_shell_alias_selected_as_planner() {
        let fixture = Fixture::new();
        let personal = resolve(&fixture.root.join(".claude-personal"))
            .to_string_lossy()
            .into_owned();
        let mut host = Script::answering(&[
            "claude-personal",
            "n",
            "opus",
            "n",
            "",
            "n",
            "claude-personal",
        ]);
        host.aliases.insert(
            "claude-personal".into(),
            json!({"engine": "claude", "model": "", "claude_config_dir": personal}),
        );
        let (result, _) = capture(|| setup_with(&mut host, &fixture.root, None));
        result.unwrap();
        let saved = load().unwrap();
        assert_eq!(saved["planner"], "claude-personal");
        assert_eq!(
            saved["aliases"]["claude-personal"]["claude_config_dir"],
            personal
        );
    }

    #[test]
    fn test_provider_commands_do_not_resume() {
        let environment: HashMap<String, String> = std::env::vars().collect();
        for engine in ["codex", "claude", "ollama"] {
            let lookup = |_: &str| Some("/bin/agent".to_string());
            let (args, _) = command_with(
                &json!({"engine": engine, "model": "x"}),
                "task",
                false,
                None,
                false,
                &environment,
                &lookup,
            )
            .unwrap();
            for banned in ["--resume", "--continue", "--dangerously-skip-permissions"] {
                assert!(!args.iter().any(|arg| arg == banned), "{engine} {banned}");
            }
        }
    }

    #[test]
    fn test_multiline_prompts_are_not_put_on_command_line() {
        let prompt = "First line\n\"quoted\" & second line";
        let environment: HashMap<String, String> = std::env::vars().collect();
        for engine in ["codex", "claude", "ollama"] {
            let lookup = |_: &str| Some("/bin/agent".to_string());
            let (args, _) = command_with(
                &json!({"engine": engine, "model": "test"}),
                prompt,
                false,
                None,
                false,
                &environment,
                &lookup,
            )
            .unwrap();
            assert!(!args.iter().any(|arg| arg == prompt));
            assert!(args.iter().all(|arg| !arg.contains('\n')));
        }
    }

    #[test]
    fn test_missing_key_rejected() {
        let profile = json!({"engine": "openai", "model": "x", "base_url": "https://example.com", "key_env": "MISSING_KEY"});
        let lookup = |_: &str| Some("/bin/qwen".to_string());
        assert!(
            command_with(
                &profile,
                "test",
                false,
                None,
                false,
                &HashMap::new(),
                &lookup
            )
            .is_err()
        );
    }
}
