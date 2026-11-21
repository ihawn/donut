//! slash commands from the plugins and skills each provider has installed.
//!
//! Donut lists them in its / menu before any provider session is open, and rewrites them into the provider's
//! own syntax: Claude Code takes /name as typed, and Codex invokes a plugin skill as `$plugin:skill`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// One slash command: `name` is what the / menu shows ("/x"), `send` is how the provider takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub send: String,
}

/// The user's home folder.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expanduser(path: &str) -> PathBuf {
    if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The Claude configuration folder this profile logs in with, as Claude Code resolves it.
pub fn claude_dir(profile: &Value) -> PathBuf {
    match profile
        .get("claude_config_dir")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(dir) => expanduser(dir),
        None => env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".claude")),
    }
}

pub fn codex_dir() -> PathBuf {
    env_path("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex"))
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| Value::Object(Default::default()))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// name and description from a SKILL.md or command file's YAML header; folded (>) values are joined.
pub fn frontmatter(path: &Path) -> HashMap<String, String> {
    let mut fields: HashMap<String, String> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return fields;
    };
    if !text.starts_with("---") {
        return fields;
    }
    let mut key: Option<String> = None;
    for line in text.lines().skip(1) {
        if line.trim() == "---" {
            break;
        }
        let indented = line.chars().next().is_some_and(char::is_whitespace);
        if let (true, Some(k)) = (indented, key.as_ref()) {
            let joined = format!("{} {}", fields[k], line.trim());
            fields.insert(k.clone(), joined.trim().to_string());
        } else if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_string(), value.trim());
            let value = if matches!(value, ">" | "|" | ">-" | "|-") {
                String::new()
            } else {
                value.trim_matches(|c| c == '\'' || c == '"').to_string()
            };
            fields.insert(name.clone(), value);
            key = Some(name);
        }
    }
    fields
}

fn sorted_entries(folder: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(folder) {
        Ok(read) => read.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => Vec::new(),
    };
    entries.sort();
    entries
}

/// (name, description) for every skills/<name>/SKILL.md under folder.
fn skills(folder: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in sorted_entries(folder) {
        let skill = entry.join("SKILL.md");
        if !skill.is_file() {
            continue;
        }
        let meta = frontmatter(&skill);
        let name = meta
            .get("name")
            .filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                entry
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        found.push((name, meta.get("description").cloned().unwrap_or_default()));
    }
    found
}

fn command_files(folder: &Path) -> Vec<(String, String)> {
    sorted_entries(folder)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "md") && p.is_file())
        .map(|p| {
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            (
                stem,
                frontmatter(&p)
                    .get("description")
                    .cloned()
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// (plugin name, install folder) for each plugin installed and enabled in this Claude login.
pub fn claude_plugins(profile: &Value, project: Option<&Path>) -> Vec<(String, PathBuf)> {
    let base = claude_dir(profile);
    let mut enabled: HashMap<String, bool> = HashMap::new();
    let mut sources = vec![base.join("settings.json")];
    if let Some(project) = project {
        sources.push(project.join(".claude").join("settings.json"));
        sources.push(project.join(".claude").join("settings.local.json"));
    }
    for source in sources {
        if let Some(map) = read_json(&source)
            .get("enabledPlugins")
            .and_then(Value::as_object)
        {
            for (key, value) in map {
                enabled.insert(key.clone(), truthy(value));
            }
        }
    }
    let installed = read_json(&base.join("plugins").join("installed_plugins.json"));
    let entries = installed.get("plugins").unwrap_or(&installed);
    let mut plugins = Vec::new();
    for (key, records) in entries.as_object().into_iter().flatten() {
        if !enabled.get(key).copied().unwrap_or(false) {
            continue;
        }
        let list: Vec<&Value> = match records {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for record in list {
            let path = record
                .get("installPath")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty());
            if let Some(path) = path.filter(|p| Path::new(p).is_dir()) {
                plugins.push((
                    key.split('@').next().unwrap_or(key).to_string(),
                    PathBuf::from(path),
                ));
                break;
            }
        }
    }
    plugins
}

fn walk_codex(folder: &Path, depth: usize, plugins: &mut BTreeMap<String, PathBuf>) {
    if depth > 6 {
        return;
    }
    let manifest = folder.join(".codex-plugin").join("plugin.json");
    if manifest.is_file() {
        let name = read_json(&manifest)
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        plugins.entry(name).or_insert_with(|| folder.to_path_buf());
        return;
    }
    for entry in sorted_entries(folder) {
        if std::fs::symlink_metadata(&entry).is_ok_and(|m| m.is_dir()) {
            walk_codex(&entry, depth + 1, plugins);
        }
    }
}

/// (plugin name, install folder) for each plugin in the Codex plugin cache.
pub fn codex_plugins() -> Vec<(String, PathBuf)> {
    let root = codex_dir().join("plugins");
    let mut plugins = BTreeMap::new();
    walk_codex(&root, 0, &mut plugins);
    plugins.into_iter().collect()
}

/// The provider's own slash commands.
pub fn commands(profile: &Value, project: Option<&Path>) -> Vec<Command> {
    let mut found = Vec::new();
    match profile.get("engine").and_then(Value::as_str) {
        Some("claude") => {
            let mut folders = vec![claude_dir(profile)];
            if let Some(project) = project {
                folders.push(project.join(".claude"));
            }
            for (_, path) in claude_plugins(profile, project) {
                folders.push(path);
            }
            for folder in folders {
                let mut entries = skills(&folder.join("skills"));
                entries.extend(command_files(&folder.join("commands")));
                for (name, description) in entries {
                    found.push(Command {
                        name: format!("/{name}"),
                        description,
                        send: format!("/{name}"),
                    });
                }
            }
        }
        Some("codex") => {
            for (plugin, path) in codex_plugins() {
                let manifest = read_json(&path.join(".codex-plugin").join("plugin.json"));
                let folder = manifest
                    .get("skills")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("skills");
                for (name, description) in skills(&path.join(folder)) {
                    found.push(Command {
                        name: format!("/{name}"),
                        description,
                        send: format!("${plugin}:{name}"),
                    });
                }
            }
            let mut folders = vec![codex_dir().join("skills")];
            if let Some(project) = project {
                folders.push(project.join(".agents").join("skills"));
            }
            for folder in folders {
                for (name, description) in skills(&folder) {
                    found.push(Command {
                        name: format!("/{name}"),
                        description,
                        send: format!("${name}"),
                    });
                }
            }
        }
        _ => {}
    }
    let mut unique: Vec<Command> = Vec::new();
    for command in found {
        if !unique.iter().any(|c| c.name == command.name) {
            unique.push(command);
        }
    }
    unique
}

pub fn installed(profile: &Value, plugin: &str, project: Option<&Path>) -> bool {
    match profile.get("engine").and_then(Value::as_str) {
        Some("claude") => claude_plugins(profile, project)
            .iter()
            .any(|(name, _)| name == plugin),
        Some("codex") => codex_plugins().iter().any(|(name, _)| name == plugin),
        _ => false,
    }
}

/// Rewrite '/name args' into the provider's syntax when name is one of its commands; otherwise unchanged.
pub fn translate(text: &str, available: &[Command]) -> String {
    let (name, rest) = text.split_once(' ').unwrap_or((text, ""));
    for command in available {
        if command.name == name {
            return if rest.is_empty() {
                command.send.clone()
            } else {
                format!("{} {rest}", command.send).trim().to_string()
            };
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REVIEW_SKILL: &str = "---\nname: ponytail-review\ndescription: >\n  Quality review of a change: is the logic right,\n  is it safe.\n---\n\nReview a change.\n";

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn install_claude_ponytail(claude: &Path, enabled: bool) {
        let plugin = claude.join("plugins/cache/ponytail/ponytail/5.3.3");
        write(
            &plugin.join("skills/ponytail-review/SKILL.md"),
            REVIEW_SKILL,
        );
        write(
            &plugin.join("skills/ponytail/SKILL.md"),
            "---\nname: ponytail\ndescription: Lazy senior dev mode.\n---\n",
        );
        write(
            &claude.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {"ponytail@ponytail": [{"scope": "user", "installPath": plugin}]}}).to_string(),
        );
        write(
            &claude.join("settings.json"),
            &json!({"enabledPlugins": {"ponytail@ponytail": enabled}}).to_string(),
        );
    }

    struct Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        root: tempfile::TempDir,
    }

    impl Env {
        fn new() -> Env {
            let guard = crate::conversations::env_lock();
            let root = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
                std::env::set_var("CODEX_HOME", root.path().join("codex"));
            }
            Env {
                _guard: guard,
                root,
            }
        }
        fn path(&self) -> &Path {
            self.root.path()
        }
    }

    fn find<'a>(found: &'a [Command], name: &str) -> &'a Command {
        found.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn test_claude_plugin_skills_are_listed_before_any_session_opens() {
        let env = Env::new();
        let claude = env.path().join("claude");
        install_claude_ponytail(&claude, true);
        write(
            &claude.join("commands/deploy.md"),
            "---\ndescription: Ship it\n---\n",
        );
        let profile = json!({"engine": "claude"});
        let found = commands(&profile, Some(env.path()));
        assert_eq!(
            find(&found, "/ponytail-review").description,
            "Quality review of a change: is the logic right, is it safe."
        );
        assert_eq!(find(&found, "/ponytail-review").send, "/ponytail-review");
        assert_eq!(find(&found, "/deploy").description, "Ship it");
        assert!(installed(&profile, "ponytail", Some(env.path())));
        let other = json!({"engine": "claude", "claude_config_dir": env.path().join("other")});
        assert!(commands(&other, None).is_empty());
    }

    #[test]
    fn test_disabled_claude_plugins_are_left_out() {
        let env = Env::new();
        install_claude_ponytail(&env.path().join("claude"), false);
        let profile = json!({"engine": "claude"});
        assert!(commands(&profile, None).is_empty());
        assert!(!installed(&profile, "ponytail", None));
    }

    #[test]
    fn test_codex_plugin_skills_are_sent_in_codex_syntax() {
        let env = Env::new();
        let plugin = env
            .path()
            .join("codex/plugins/cache/ponytail/ponytail/5.3.3");
        write(
            &plugin.join(".codex-plugin/plugin.json"),
            &json!({"name": "ponytail", "skills": "./skills/"}).to_string(),
        );
        write(
            &plugin.join("skills/ponytail-review/SKILL.md"),
            REVIEW_SKILL,
        );
        let profile = json!({"engine": "codex"});
        let available = commands(&profile, Some(env.path()));
        assert_eq!(
            available
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["/ponytail-review"]
        );
        assert_eq!(
            translate("/ponytail-review staged", &available),
            "$ponytail:ponytail-review staged"
        );
        assert_eq!(
            translate("/ponytail-review", &available),
            "$ponytail:ponytail-review"
        );
        assert_eq!(translate("/other thing", &available), "/other thing");
        assert!(installed(&profile, "ponytail", None));
    }
}
