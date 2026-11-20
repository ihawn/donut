//! Port of donut/config.py.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The Donut profile in use: "default", or the name given with --profile (or DONUT_PROFILE).
pub fn profile_name() -> String {
    std::env::var("DONUT_PROFILE").ok().filter(|name| !name.is_empty()).unwrap_or_else(|| "default".into())
}

/// This profile's configuration file; home stands in for XDG_CONFIG_HOME.
pub fn config_path(home_dir: Option<&std::path::Path>) -> PathBuf {
    let base = match home_dir {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")),
    };
    let folder = base.join("donut");
    let name = profile_name();
    if name == "default" { folder.join("config.json") } else { folder.join("profiles").join(format!("{name}.json")) }
}

/// Where this profile's conversations and logs are kept.
pub fn state_dir() -> PathBuf {
    let folder = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("donut");
    let name = profile_name();
    if name == "default" { folder } else { folder.join("profiles").join(name) }
}
