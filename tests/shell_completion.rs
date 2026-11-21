//! Tab completes paths and programs in the !shell prompt.
//!
//! They change environment variables, so they live in their own process and take a lock.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use donut::terminal::app::AppOptions;
use donut::terminal::flows::App;
use donut::terminal::render::shell_candidates;
use donut::terminal::widgets::MenuKind;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

static LOCK: Mutex<()> = Mutex::new(());

struct Env {
    _guard: MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

fn set(key: &str, value: &std::ffi::OsStr) {
    // SAFETY: every test in this file holds LOCK while it touches the environment.
    unsafe { std::env::set_var(key, value) };
}

fn env() -> Env {
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let saved = ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "PATH"]
        .into_iter()
        .map(|key| (key, std::env::var_os(key)))
        .collect();
    set("XDG_CONFIG_HOME", temp.path().join("config").as_os_str());
    set("XDG_STATE_HOME", temp.path().join("state").as_os_str());
    Env {
        _guard: guard,
        temp,
        saved,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: as above.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

impl Env {
    fn root(&self) -> PathBuf {
        let root = self.temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        root
    }
}

/// The folder of the Python CandidateTests: docs/, dist/, .git/, a.txt and docs/guide.md.
fn candidates_project(env: &Env) -> PathBuf {
    let root = env.root();
    for name in ["docs", "dist", ".git"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    std::fs::write(root.join("a.txt"), "").unwrap();
    std::fs::write(root.join("docs/guide.md"), "").unwrap();
    root
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn test_paths_are_relative_to_the_project() {
    let env = env();
    let root = candidates_project(&env);
    assert_eq!(shell_candidates("!cd d", &root).1, ["dist/", "docs/"]);
    assert_eq!(shell_candidates("!cat docs/g", &root).1, ["docs/guide.md"]);
}

#[test]
fn test_dotfiles_only_when_asked() {
    let env = env();
    let root = candidates_project(&env);
    assert!(
        !shell_candidates("!ls ", &root)
            .1
            .contains(&".git/".to_string())
    );
    assert_eq!(shell_candidates("!ls .g", &root).1, [".git/"]);
}

#[cfg(unix)]
#[test]
fn test_first_word_completes_programs() {
    use std::os::unix::fs::PermissionsExt;
    let env = env();
    let root = candidates_project(&env);
    let bin_dir = root.join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let tool = bin_dir.join("donut-tool");
    std::fs::write(&tool, "").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    set("PATH", bin_dir.as_os_str());
    assert_eq!(
        shell_candidates("!donut-t", &root),
        ("donut-t".to_string(), strings(&["donut-tool"]))
    );
}

#[test]
fn test_missing_folder_gives_nothing() {
    let env = env();
    let root = candidates_project(&env);
    assert!(shell_candidates("!ls nope/x", &root).1.is_empty());
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Type text into the prompt, press Tab, report (draft, menu kind, menu rows), then press the keys in `then`
/// and report the draft again.
fn run_tab(
    text: &str,
    root: &Path,
    then: &[KeyCode],
) -> ((String, Option<MenuKind>, Vec<String>), String) {
    let app = App::with_options(root, None, None, AppOptions::test()).unwrap();
    app.set_installer(Arc::new(|_, _| Ok(())));
    app.type_text(text);
    app.handle_key(key(KeyCode::Tab));
    let state = {
        let s = app.st();
        (
            s.draft.text(),
            s.menu.kind,
            s.menu
                .visible_rows
                .iter()
                .map(|row| row.label.clone())
                .collect(),
        )
    };
    for code in then {
        app.handle_key(key(*code));
    }
    let after = app.st().draft.text();
    (state, after)
}

#[test]
fn test_tab_completes_single_match_and_lists_many() {
    let env = env();
    let root = env.root();
    for name in ["alpha", "apple", "beta"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    let ((text, kind, _), _) = run_tab("!ls b", &root, &[]);
    assert_eq!((text, kind), ("!ls beta/".to_string(), None));
    let ((text, kind, rows), after) = run_tab("!ls a", &root, &[KeyCode::Down, KeyCode::Tab]);
    assert_eq!(
        (text, kind, rows),
        (
            "!ls a".to_string(),
            Some(MenuKind::Shell),
            strings(&["alpha/", "apple/"])
        )
    );
    assert_eq!(after, "!ls apple/");
}

#[test]
fn test_tab_outside_shell_mode_is_untouched() {
    let env = env();
    let root = env.root();
    std::fs::create_dir_all(root.join("alpha")).unwrap();
    let ((_, kind, _), _) = run_tab("ls a", &root, &[]);
    assert_eq!(kind, None);
}
