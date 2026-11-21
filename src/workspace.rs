//! Where a session's tickets live: .donut/<session>/{to_do,in_progress,on_hold,blocked,
//! done}.
//!
//! Each Donut terminal session gets its own folder, so two lines of work in one project never share a queue. Older
//! projects keep working from swim_lanes/ until they start a session.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use anyhow::{Result, bail};
use chrono::TimeZone;
use regex::Regex;

use crate::config::profile_name;

pub const ROOT: &str = ".donut";
pub const LEGACY: &str = "swim_lanes";
pub const LANES: [&str; 5] = ["to_do", "in_progress", "on_hold", "blocked", "done"];
/// .donut/skills holds the worker skill, .donut/profiles the named profiles.
pub const RESERVED: [&str; 2] = ["skills", "profiles"];

static VALID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$").unwrap());
static UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_.-]+").unwrap());
static DASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-{2,}").unwrap());

/// Where the sessions of the profile in use live: .donut/, or .donut/profiles/<name>/ for a named profile.
pub fn root(project: &Path) -> PathBuf {
    let name = profile_name();
    if name == "default" {
        project.join(ROOT)
    } else {
        project.join(ROOT).join("profiles").join(name)
    }
}

/// The folder holding the five lanes: .donut/<session>/, or swim_lanes/ when there is no session.
pub fn lanes_dir(project: &Path, session: Option<&str>) -> PathBuf {
    match session.filter(|name| !name.is_empty()) {
        Some(name) => root(project).join(name),
        None => project.join(LEGACY),
    }
}

/// The lanes folder as the agents see it, relative to the project with a trailing slash.
pub fn relative(project: &Path, lanes: &Path) -> String {
    let inside = lanes.strip_prefix(project).unwrap_or(lanes);
    let parts: Vec<String> = inside
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/") + "/"
}

pub fn valid(name: &str) -> bool {
    VALID.is_match(name) && !RESERVED.contains(&name)
}

/// A fresh session name from the time it started, such as 20261109-1712 (or 20261109-1712-2).
///
/// now is seconds since the epoch (the current time when None). taken holds names already in use whose folders
/// may not exist yet.
pub fn new_name(project: &Path, now: Option<i64>, taken: &[String]) -> String {
    let when = match now {
        Some(seconds) => chrono::Local
            .timestamp_opt(seconds, 0)
            .single()
            .unwrap_or_else(chrono::Local::now),
        None => chrono::Local::now(),
    };
    let base = when.format("%Y%m%d-%H%M").to_string();
    let (mut name, mut number) = (base.clone(), 1);
    while taken.contains(&name) || root(project).join(&name).exists() {
        number += 1;
        name = format!("{base}-{number}");
    }
    name
}

/// Make the session's five lanes (each with a .gitkeep, so an empty lane can be committed).
pub fn create(project: &Path, session: Option<&str>) -> Result<PathBuf> {
    if let Some(name) = session.filter(|name| !name.is_empty())
        && !valid(name)
    {
        bail!("Invalid session name {name:?}: use letters, numbers, '.', '_' or '-'");
    }
    let lanes = lanes_dir(project, session);
    for lane in LANES {
        std::fs::create_dir_all(lanes.join(lane))?;
        let keep = lanes.join(lane).join(".gitkeep");
        if !keep.exists() {
            std::fs::File::create(keep)?;
        }
    }
    Ok(lanes)
}

fn changed(path: &Path) -> SystemTime {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|meta| meta.modified()).ok();
    let mut newest = modified(path).unwrap_or(SystemTime::UNIX_EPOCH);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(time) = modified(&entry.path())
            {
                newest = newest.max(time);
            }
        }
    }
    newest
}

/// This project's sessions with tickets folders, most recently changed first.
pub fn sessions(project: &Path) -> Vec<String> {
    let base = root(project);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .filter(|(name, path)| path.is_dir() && valid(name) && path.join("to_do").is_dir())
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.sort_by_key(|(_, path)| std::cmp::Reverse(changed(path)));
    found.into_iter().map(|(name, _)| name).collect()
}

/// The session a command-line run uses when none is named: the latest one, then swim_lanes/, then a new one.
pub fn default(project: &Path) -> Option<String> {
    if let Some(latest) = sessions(project).into_iter().next() {
        return Some(latest);
    }
    if project.join(LEGACY).join("to_do").is_dir() {
        return None;
    }
    Some(new_name(project, None, &[]))
}

/// How many tickets each lane of a session holds, in lane order.
pub fn count(project: &Path, session: Option<&str>) -> Vec<(&'static str, usize)> {
    let lanes = lanes_dir(project, session);
    LANES
        .iter()
        .map(|lane| (*lane, crate::tickets::ticket_files(&lanes.join(lane)).len()))
        .collect()
}

/// A session name made safe for a folder: "fix login bug" -> "fix-login-bug".
pub fn slug(name: &str) -> String {
    let replaced = UNSAFE.replace_all(name.trim(), "-");
    let text = replaced.trim_matches(['-', '.']);
    let text = DASHES.replace_all(text, "-");
    let cut: String = text.chars().take(64).collect();
    let text = cut.trim_end_matches(['-', '.']).to_string();
    if valid(&text) { text } else { String::new() }
}

/// Move a session's tickets folder to a folder named after new_name. Returns the session's new name.
///
/// A name already taken gets -2, -3 and so on. A folder that does not exist yet just takes the new name.
pub fn rename(project: &Path, old: Option<&str>, new_name: &str) -> Result<String> {
    let mut new = slug(new_name);
    if new.is_empty() {
        bail!("Cannot make a folder name from {new_name:?}");
    }
    if Some(new.as_str()) == old {
        return Ok(new);
    }
    let folder = root(project);
    let (base, mut number) = (new.clone(), 1);
    while folder.join(&new).exists() {
        number += 1;
        new = format!("{base}-{number}");
    }
    if let Some(old) = old.filter(|name| !name.is_empty()) {
        let source = folder.join(old);
        if source.is_dir() {
            std::fs::rename(source, folder.join(&new))?;
        }
    }
    Ok(new)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn project() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_path_buf();
        (temp, path)
    }

    fn set_mtime(path: &Path, seconds: u64) {
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        std::fs::File::open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn test_each_session_gets_its_own_lanes_folder() {
        let (_keep, project) = project();
        let now = chrono::Local
            .with_ymd_and_hms(2026, 11, 9, 17, 12, 0)
            .unwrap()
            .timestamp();
        let first = new_name(&project, Some(now), &[]);
        assert_eq!(first, "20261109-1712");
        let lanes = create(&project, Some(&first)).unwrap();
        assert_eq!(lanes, project.join(".donut").join(&first));
        assert!(lanes.join("on_hold").join(".gitkeep").is_file());
        assert_eq!(new_name(&project, Some(now), &[]), "20261109-1712-2"); // Never reuses a folder.
        assert_eq!(relative(&project, &lanes), ".donut/20261109-1712/");
        assert_eq!(lanes_dir(&project, None), project.join("swim_lanes"));
        assert_eq!(
            new_name(&project, Some(now), &["20261109-1712-2".to_string()]),
            "20261109-1712-3"
        );
    }

    #[test]
    fn test_skills_folder_and_odd_names_are_not_sessions() {
        let (_keep, project) = project();
        std::fs::create_dir_all(project.join(".donut/skills/swim-lane-worker")).unwrap();
        for name in ["skills", "../up", "a b", ""] {
            assert!(!valid(name), "{name}");
        }
        assert!(create(&project, Some("skills")).is_err());
        assert!(sessions(&project).is_empty());
    }

    #[test]
    fn test_command_line_default_is_latest_then_legacy_then_new() {
        let (_keep, project) = project();
        let fresh = default(&project).unwrap();
        assert!(Regex::new(r"^\d{8}-\d{4}$").unwrap().is_match(&fresh));
        create(&project, None).unwrap(); // swim_lanes/
        assert_eq!(default(&project), None);
        let older = create(&project, Some("older")).unwrap();
        let newer = create(&project, Some("newer")).unwrap();
        set_mtime(&older.join("to_do"), 1);
        set_mtime(&older, 1);
        for entry in std::fs::read_dir(&older).unwrap() {
            set_mtime(&entry.unwrap().path(), 1);
        }
        std::fs::write(newer.join("to_do").join("001_x.md"), "x").unwrap();
        assert_eq!(sessions(&project), ["newer", "older"]);
        assert_eq!(default(&project).as_deref(), Some("newer"));
        let counts = count(&project, Some("newer"));
        assert_eq!(
            counts.iter().find(|(lane, _)| *lane == "to_do").unwrap().1,
            1
        );
    }

    #[test]
    fn test_names_become_safe_folder_names() {
        assert_eq!(slug("fix login bug"), "fix-login-bug");
        assert_eq!(slug("  Fix: PR #7 / login!  "), "Fix-PR-7-login");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("skills"), ""); // .donut/skills holds the worker skill.
        assert_eq!(slug("   "), "");
    }

    #[test]
    fn test_rename_moves_the_tickets_and_never_overwrites() {
        let (_keep, project) = project();
        let lanes = create(&project, Some("20261109-1424")).unwrap();
        std::fs::write(lanes.join("to_do").join("001_a.md"), "a").unwrap();
        assert_eq!(
            rename(&project, Some("20261109-1424"), "fix login bug").unwrap(),
            "fix-login-bug"
        );
        assert!(
            project
                .join(".donut/fix-login-bug/to_do/001_a.md")
                .is_file()
        );
        assert!(!project.join(".donut/20261109-1424").exists());
        create(&project, Some("other")).unwrap();
        assert_eq!(
            rename(&project, Some("other"), "fix login bug").unwrap(),
            "fix-login-bug-2"
        );
        assert_eq!(
            rename(&project, Some("not-made-yet"), "later").unwrap(),
            "later"
        ); // Nothing to move.
        assert!(rename(&project, Some("later"), "!!!").is_err());
    }
}
