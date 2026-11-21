//! Configuration: profiles, the config file, the state folder and saved role choices.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::providers::validate;

/// What selects a provider account. A named profile starts without them, so it cannot inherit the shell's account:
/// set them in that profile's Claude login folder (settings.json "env") instead.
pub const PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "AWS_PROFILE",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
];

/// Profile fields a saved alias may hold.
const ALLOWED_FIELDS: &[&str] = &[
    "engine",
    "model",
    "base_url",
    "key_env",
    "auto_approve",
    "claude_config_dir",
    "unload_others",
    "permission_mode",
    "effort",
    "fallback",
];

/// Which config key names the default alias of a role.
pub const ROLE_DEFAULT_KEYS: &[(&str, &str)] = &[("plan", "planner"), ("work", "default_model")];

static PROFILE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$").unwrap());
static ALIAS_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]*$").unwrap());
static CLAUDE_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d)\.(\d)").unwrap());

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The Donut profile in use: "default", or the name given with --profile (or DONUT_PROFILE).
pub fn profile_name() -> String {
    std::env::var("DONUT_PROFILE")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".into())
}

/// Select the profile for this process and the windows and workers it starts.
///
/// Each profile has its own configuration, conversations, logs and tickets. A named profile also drops the
/// shell's provider variables, so its accounts come only from its own configuration.
pub fn use_profile(name: Option<&str>) -> Result<()> {
    match name {
        None | Some("") | Some("default") => {
            // SAFETY: called while the process is still single-threaded or from tests that hold their own lock.
            unsafe { std::env::remove_var("DONUT_PROFILE") };
            Ok(())
        }
        Some(name) => {
            if !PROFILE_NAME.is_match(name) {
                bail!("A profile name may contain letters, numbers, underscores and hyphens.");
            }
            // SAFETY: as above.
            unsafe {
                std::env::set_var("DONUT_PROFILE", name);
                for key in PROVIDER_ENV {
                    std::env::remove_var(key);
                }
            }
            Ok(())
        }
    }
}

/// This profile's configuration file; home stands in for XDG_CONFIG_HOME.
pub fn config_path(home_dir: Option<&Path>) -> PathBuf {
    let base = match home_dir {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config")),
    };
    let folder = base.join("donut");
    let name = profile_name();
    if name == "default" {
        folder.join("config.json")
    } else {
        folder.join("profiles").join(format!("{name}.json"))
    }
}

/// Where this profile's conversations and logs are kept.
pub fn state_dir() -> PathBuf {
    let folder = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("donut");
    let name = profile_name();
    if name == "default" {
        folder
    } else {
        folder.join("profiles").join(name)
    }
}

/// Refuse a project that does not allow this profile.
///
/// .donut/config.json in the project, {"profiles": ["default"]}, lists the profiles that may open it. It guards
/// against opening a project with the wrong account by mistake; it does not stop a model that edits the file.
pub fn check_project(project: &Path) -> Result<()> {
    let path = project.join(".donut").join("config.json");
    if !path.exists() {
        return Ok(());
    }
    let allowed: Option<Value> = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
    {
        Some(object) => object
            .get("profiles")
            .filter(|value| !value.is_null())
            .cloned(),
        None => Some(Value::Bool(false)),
    };
    let allowed = match allowed {
        None => return Ok(()),
        Some(value) => value,
    };
    let names: Option<Vec<&str>> = allowed
        .as_array()
        .and_then(|list| list.iter().map(Value::as_str).collect());
    let Some(names) = names else {
        bail!(
            "{} must hold {{\"profiles\": [\"NAME\", ...]}}. Correct it before Donut opens this project.",
            path.display()
        );
    };
    let current = profile_name();
    if !names.contains(&current.as_str()) {
        let list = if names.is_empty() {
            "no".to_string()
        } else {
            names.join(" and ")
        };
        bail!(
            "This project allows only the {list} profile, not {current} ({}). Open another folder with -p PATH, or \
             add \"{current}\" to that file's list.",
            path.display()
        );
    }
    Ok(())
}

/// The saved configuration, with Claude model IDs normalized. A missing file gives {"aliases": {}}.
pub fn load() -> Result<Value> {
    let path = config_path(None);
    let mut config: Value = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)?
    } else {
        json!({"aliases": {}})
    };
    if let Some(aliases) = config.get_mut("aliases").and_then(Value::as_object_mut) {
        for profile in aliases.values_mut() {
            let Some(object) = profile.as_object_mut() else {
                continue;
            };
            let model = object
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let is_claude = object.get("engine").and_then(Value::as_str) == Some("claude");
            if let (true, Some(model)) = (is_claude, model)
                && model.starts_with("claude-")
            {
                let fixed = CLAUDE_VERSION.replace_all(&model, "$1-$2").into_owned();
                object.insert("model".into(), Value::String(fixed));
            }
            own_login(profile)?;
        }
    }
    Ok(config)
}

/// Refuse a named profile's Claude alias that has no login folder: it would use ~/.claude, the default's.
pub fn own_login(profile: &Value) -> Result<()> {
    let has_dir = profile
        .get("claude_config_dir")
        .is_some_and(|value| match value {
            Value::Null => false,
            Value::String(text) => !text.is_empty(),
            Value::Bool(flag) => *flag,
            Value::Array(list) => !list.is_empty(),
            Value::Object(map) => !map.is_empty(),
            Value::Number(number) => number.as_f64() != Some(0.0),
        });
    if profile_name() != "default"
        && profile.get("engine").and_then(Value::as_str) == Some("claude")
        && !has_dir
    {
        bail!(
            "Every Claude alias in the {} profile needs \"claude_config_dir\" in {}, so it does not share the default \
             profile's login, skills and history.",
            profile_name(),
            config_path(None).display()
        );
    }
    Ok(())
}

/// Python's json.dumps(value, indent=indent): ensure_ascii on, items separated by "," and keys by ": ".
pub fn dumps(value: &Value, indent: usize) -> String {
    use serde::Serialize;
    let pad = vec![b' '; indent];
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(&pad);
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    value
        .serialize(&mut serializer)
        .expect("a JSON value always serializes");
    let text = String::from_utf8(out).expect("JSON is UTF-8");
    let mut ascii = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                ascii.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    ascii
}

/// Write the configuration atomically, after checking every alias.
pub fn save(config: &Value) -> Result<()> {
    let empty = Map::new();
    let aliases = config
        .get("aliases")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (alias, profile) in aliases {
        if !ALIAS_NAME.is_match(alias) {
            bail!("Aliases may contain letters, numbers, underscores and hyphens.");
        }
        validate(profile)?;
        own_login(profile)?;
        if profile.as_object().is_some_and(|object| {
            object
                .keys()
                .any(|key| !ALLOWED_FIELDS.contains(&key.as_str()))
        }) {
            bail!("Unexpected profile fields; credentials cannot be saved.");
        }
    }
    let path = config_path(None);
    let parent = path.parent().expect("a config path has a parent");
    if !parent.exists() {
        if let Some(grand) = parent.parent() {
            std::fs::create_dir_all(grand)?;
        }
        make_private_dir(parent)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    {
        use std::io::Write;
        file.write_all(dumps(config, 2).as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

fn make_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

fn truthy_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// (alias, profile) for "plan", "work", "review" or "solo", including a model picked with /planner and the like.
///
/// A choice saved under config["roles"] wins. Otherwise plan and work use their default alias, and review
/// follows the planner unless config["reviewer"] is "off" or names an alias. None means nothing is configured
/// (or review is off).
pub fn role_choice(config: &Value, role: &str) -> Option<(String, Value)> {
    let aliases = config.get("aliases").and_then(Value::as_object);
    let lookup = |name: &str| aliases.and_then(|map| map.get(name));
    if let Some(saved) = config
        .get("roles")
        .and_then(|roles| roles.get(role))
        .and_then(Value::as_object)
        && let Some(alias) = saved.get("alias").and_then(Value::as_str)
        && let Some(base) = lookup(alias)
    {
        let mut profile = base.clone();
        if let Some(model) = truthy_str(saved.get("model")) {
            profile["model"] = json!(model);
        }
        if let Some(effort) = truthy_str(saved.get("effort")) {
            profile["effort"] = json!(effort);
        }
        return Some((alias.to_string(), profile));
    }
    if role == "review" {
        let reviewer = config.get("reviewer").and_then(Value::as_str);
        if reviewer == Some("off") {
            return None;
        }
        if let Some(name) = reviewer
            && let Some(profile) = lookup(name)
        {
            return Some((name.to_string(), profile.clone()));
        }
        return role_choice(config, "plan");
    }
    let key = ROLE_DEFAULT_KEYS.iter().find(|(name, _)| *name == role)?.1;
    let alias = config.get(key).and_then(Value::as_str)?;
    lookup(alias).map(|profile| (alias.to_string(), profile.clone()))
}

/// Save which alias, model and effort a role uses, so /model, /review and /effort choices stick.
pub fn remember_role(
    role: &str,
    alias: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<()> {
    let mut config = load()?;
    let object = config
        .as_object_mut()
        .expect("the configuration is an object");
    let roles = object.entry("roles").or_insert_with(|| json!({}));
    let mut entry = json!({"alias": alias, "model": model.unwrap_or("")});
    if let Some(effort) = effort.filter(|text| !text.is_empty()) {
        entry["effort"] = json!(effort);
    }
    roles[role] = entry;
    if role == "review" {
        object.shift_remove("reviewer");
    }
    save(&config)
}

/// Drop a saved role choice. For review, reviewer="off" turns review off; None makes it follow the planner.
pub fn forget_role(role: &str, reviewer: Option<&str>) -> Result<()> {
    let mut config = load()?;
    if let Some(roles) = config.get_mut("roles").and_then(Value::as_object_mut) {
        roles.shift_remove(role);
    }
    if role == "review" {
        let object = config
            .as_object_mut()
            .expect("the configuration is an object");
        if reviewer == Some("off") {
            object.insert("reviewer".into(), json!("off"));
        } else {
            object.shift_remove("reviewer");
        }
    }
    save(&config)
}
