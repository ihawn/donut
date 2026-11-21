//! Profile tests (config, platform and workspace), and the config behaviour that
//! other Python tests reached through the CLI. They change the environment, so they live in their own process and
//! take a lock.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use donut::{config, platform, workspace};
use serde_json::{Value, json};

static LOCK: Mutex<()> = Mutex::new(());

struct Env {
    _guard: MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
}

fn set(key: &str, value: &str) {
    // SAFETY: every test in this file holds LOCK while it touches the environment.
    unsafe { std::env::set_var(key, value) };
}

fn unset(key: &str) {
    // SAFETY: as above.
    unsafe { std::env::remove_var(key) };
}

fn setup() -> Env {
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    set("XDG_CONFIG_HOME", &root.join("config").to_string_lossy());
    set("XDG_STATE_HOME", &root.join("state").to_string_lossy());
    set("CLAUDE_CODE_USE_BEDROCK", "1");
    set("AWS_PROFILE", "work");
    set("ANTHROPIC_API_KEY", "key");
    unset("DONUT_PROFILE");
    std::fs::create_dir_all(root.join("project")).unwrap();
    Env {
        _guard: guard,
        temp,
    }
}

impl Env {
    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn project(&self) -> std::path::PathBuf {
        self.root().join("project")
    }
}

#[test]
fn test_each_profile_has_its_own_configuration_state_and_tickets() {
    let env = setup();
    let project = env.project();
    let mut places = Vec::new();
    for name in ["default", "personal"] {
        config::use_profile(Some(name)).unwrap();
        places.push((
            config::config_path(None),
            config::state_dir(),
            workspace::lanes_dir(&project, Some("one")),
        ));
    }
    assert_eq!(places[0].0, env.root().join("config/donut/config.json"));
    assert_eq!(
        places[1].0,
        env.root().join("config/donut/profiles/personal.json")
    );
    assert_eq!(places[0].1, env.root().join("state/donut"));
    assert_eq!(
        places[1].1,
        env.root().join("state/donut/profiles/personal")
    );
    assert_eq!(places[0].2, project.join(".donut/one"));
    assert_eq!(places[1].2, project.join(".donut/profiles/personal/one"));
    assert_ne!(places[0].0, places[1].0);
    assert_ne!(places[0].1, places[1].1);
    assert_ne!(places[0].2, places[1].2);
    // A session made in one profile is not listed in the other.
    workspace::create(&project, Some("one")).unwrap();
    assert_eq!(workspace::sessions(&project), ["one"]);
    config::use_profile(Some("default")).unwrap();
    assert!(workspace::sessions(&project).is_empty());
    assert!(!workspace::valid("profiles"));
    config::use_profile(None).unwrap();
}

#[test]
fn test_a_named_profile_drops_the_shells_provider_variables() {
    let env = setup();
    config::use_profile(None).unwrap();
    assert_eq!(std::env::var("CLAUDE_CODE_USE_BEDROCK").unwrap(), "1"); // The default profile keeps the shell as it is.
    config::use_profile(Some("personal")).unwrap();
    assert_eq!(std::env::var("DONUT_PROFILE").unwrap(), "personal");
    for key in [
        "CLAUDE_CODE_USE_BEDROCK",
        "AWS_PROFILE",
        "ANTHROPIC_API_KEY",
    ] {
        assert!(std::env::var(key).is_err(), "{key}");
    }
    let command = platform::window_command(&env.project());
    assert_eq!(command[command.len() - 2..], ["--profile", "personal"]);
    assert!(config::use_profile(Some("../other")).is_err());
    config::use_profile(None).unwrap();
}

#[test]
fn test_a_named_profiles_claude_alias_needs_its_own_login_folder() {
    let env = setup();
    config::use_profile(Some("personal")).unwrap();
    let path = config::config_path(None);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        json!({"aliases": {"claude": {"engine": "claude", "model": "opus"}}}).to_string(),
    )
    .unwrap();
    let error = config::load().unwrap_err();
    assert!(error.to_string().contains("claude_config_dir"), "{error}");
    let folder = env
        .root()
        .join("claude-personal")
        .to_string_lossy()
        .into_owned();
    config::save(&json!({"aliases": {"claude": {"engine": "claude", "model": "opus", "claude_config_dir": folder}}}))
        .unwrap();
    let loaded = config::load().unwrap();
    let names: Vec<&String> = loaded["aliases"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["claude"]);
    config::use_profile(None).unwrap();
}

#[test]
fn test_the_command_line_uses_only_the_chosen_profiles_aliases() {
    let _env = setup();
    config::save(&json!({"aliases": {"work": {"engine": "codex", "model": ""}}})).unwrap();
    config::use_profile(Some("personal")).unwrap();
    config::save(&json!({"aliases": {"mine": {"engine": "codex", "model": ""}}})).unwrap();
    let mine = config::load().unwrap();
    assert!(mine["aliases"].get("mine").is_some());
    assert!(mine["aliases"].get("work").is_none());
    config::use_profile(None).unwrap();
    assert!(config::load().unwrap()["aliases"].get("work").is_some());
}

#[test]
fn test_a_project_can_list_the_profiles_allowed_to_open_it() {
    let env = setup();
    let project = env.project();
    config::check_project(&project).unwrap(); // No file: every profile may open it.
    let pin = project.join(".donut").join("config.json");
    std::fs::create_dir_all(pin.parent().unwrap()).unwrap();
    std::fs::write(&pin, json!({"profiles": ["default"]}).to_string()).unwrap();
    config::check_project(&project).unwrap();
    config::use_profile(Some("personal")).unwrap();
    let error = config::check_project(&project).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("allows only the default profile"),
        "{error}"
    );
    config::use_profile(Some("default")).unwrap();
    config::check_project(&project).unwrap();
    std::fs::write(&pin, "not json").unwrap(); // A file that cannot be read refuses every profile.
    assert!(config::check_project(&project).is_err());
    config::use_profile(None).unwrap();
}

#[test]
fn test_saved_configuration_round_trips_in_pythons_format() {
    let _env = setup();
    assert_eq!(config::load().unwrap(), json!({"aliases": {}}));
    // Claude IDs spell versions with a hyphen.
    let alias = json!({"engine": "claude", "model": "claude-opus-5.5", "effort": "high"});
    config::save(&json!({"aliases": {"c": alias}, "planner": "c", "note": "caf\u{e9} \u{1f369}"}))
        .unwrap();
    let text = std::fs::read_to_string(config::config_path(None)).unwrap();
    assert!(
        text.starts_with("{\n  \"aliases\": {\n    \"c\": {\n      \"engine\": \"claude\""),
        "{text}"
    );
    assert!(
        text.contains("\"note\": \"caf\\u00e9 \\ud83c\\udf69\""),
        "{text}"
    );
    assert!(text.ends_with("}\n"));
    let loaded = config::load().unwrap();
    assert_eq!(loaded["aliases"]["c"]["model"], "claude-opus-5-5");
    // Credentials and unknown fields are refused, and so are odd alias names.
    let secret = json!({"aliases": {"c": {"engine": "codex", "model": "", "api_key": "x"}}});
    assert!(config::save(&secret).is_err());
    assert!(
        config::save(&json!({"aliases": {"bad name": {"engine": "codex", "model": ""}}})).is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(config::config_path(None).parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[test]
fn test_roles_follow_saved_choices_then_defaults() {
    let _env = setup();
    let aliases = json!({
        "big": {"engine": "claude", "model": "opus"},
        "small": {"engine": "ollama", "model": "q"},
    });
    config::save(&json!({"aliases": aliases, "planner": "big", "default_model": "small"})).unwrap();
    let config_now = config::load().unwrap();
    assert_eq!(config::role_choice(&config_now, "plan").unwrap().0, "big");
    assert_eq!(config::role_choice(&config_now, "work").unwrap().0, "small");
    assert_eq!(config::role_choice(&config_now, "review").unwrap().0, "big"); // Follows the planner.
    assert!(config::role_choice(&config_now, "solo").is_none());
    config::remember_role("solo", "small", Some("q2"), Some("high")).unwrap();
    config::remember_role("review", "small", None, None).unwrap();
    let config_now = config::load().unwrap();
    let (alias, profile) = config::role_choice(&config_now, "solo").unwrap();
    assert_eq!(
        (
            alias.as_str(),
            profile["model"].as_str(),
            profile["effort"].as_str()
        ),
        ("small", Some("q2"), Some("high"))
    );
    assert_eq!(
        config::role_choice(&config_now, "review").unwrap().0,
        "small"
    );
    config::forget_role("review", Some("off")).unwrap();
    let config_now = config::load().unwrap();
    assert!(config::role_choice(&config_now, "review").is_none());
    config::forget_role("review", None).unwrap();
    config::forget_role("solo", None).unwrap();
    let config_now: Value = config::load().unwrap();
    assert!(config_now.get("reviewer").is_none());
    assert!(config::role_choice(&config_now, "solo").is_none());
}
