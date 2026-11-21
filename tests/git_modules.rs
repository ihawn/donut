//! Git-backed tests for checkpoints and pull requests. They set
//! GIT_CONFIG_* and PATH, so they live in their own process and take a lock.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use donut::{checkpoints, pullrequest};

static LOCK: Mutex<()> = Mutex::new(());

fn set(key: &str, value: &str) {
    // SAFETY: every test in this file holds LOCK while it touches the environment.
    unsafe { std::env::set_var(key, value) };
}

fn guard() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn identity(root: &Path, extra: &str) {
    std::fs::write(
        root.join("gitconfig"),
        format!("[user]\n\tname = T\n\temail = t@example.com\n{extra}"),
    )
    .unwrap();
    set(
        "GIT_CONFIG_GLOBAL",
        &root.join("gitconfig").to_string_lossy(),
    );
    set("GIT_CONFIG_SYSTEM", &root.join("none").to_string_lossy());
}

#[test]
fn test_commit_records_changes() {
    let _lock = guard();
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("p");
    std::fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-q"]);
    set(
        "GIT_CONFIG_GLOBAL",
        &project.join("no-gitconfig").to_string_lossy(),
    );
    set(
        "GIT_CONFIG_SYSTEM",
        &project.join("no-gitconfig").to_string_lossy(),
    );
    assert_eq!(checkpoints::commit(&project, "nothing").unwrap(), None);
    std::fs::write(project.join("a.txt"), "one\n").unwrap();
    assert_eq!(checkpoints::changed_files(&project).unwrap(), ["a.txt"]);
    let first = checkpoints::commit(&project, "donut: done 001_a.md").unwrap();
    assert!(first.is_some_and(|hash| !hash.is_empty()));
    assert_eq!(
        checkpoints::changed_files(&project).unwrap(),
        Vec::<String>::new()
    );
    let log = git(&project, &["log", "--format=%s"]);
    assert_eq!(log.lines().next().unwrap(), "donut: done 001_a.md");
}

#[test]
fn test_files_group_by_repository_and_checkpoints_commit_each() {
    let _lock = guard();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    identity(root, "");
    let workspace = root.join("workspace");
    for name in ["api", "web"] {
        std::fs::create_dir_all(workspace.join(name)).unwrap();
        git(&workspace.join(name), &["init", "-q"]);
        std::fs::write(workspace.join(name).join("a.txt"), "1\n").unwrap();
    }
    std::fs::write(workspace.join("notes.md"), "not in any repository\n").unwrap();
    assert!(!checkpoints::is_repository(&workspace));
    assert_eq!(
        checkpoints::changed_files(&workspace).unwrap(),
        ["api/a.txt", "web/a.txt"]
    );
    let files = vec!["api/a.txt".to_string(), "web/a.txt".to_string()];
    let groups = checkpoints::group(&workspace, &files);
    let shown: Vec<(&str, &[String])> = groups
        .iter()
        .map(|(_, p, f)| (p.as_str(), f.as_slice()))
        .collect();
    assert_eq!(
        shown,
        [
            ("api/", &["a.txt".to_string()][..]),
            ("web/", &["a.txt".to_string()][..])
        ]
    );
    let hashes = checkpoints::commit(&workspace, "donut: done 001_a.md")
        .unwrap()
        .unwrap();
    assert_eq!(hashes.split(", ").count(), 2);
    assert_eq!(
        checkpoints::changed_files(&workspace).unwrap(),
        Vec::<String>::new()
    );
    git(
        &workspace.join("api"),
        &[
            "remote",
            "add",
            "origin",
            &root.join("api.git").to_string_lossy(),
        ],
    );
    std::fs::write(workspace.join("api/a.txt"), "2\n").unwrap();
    std::fs::write(workspace.join("web/a.txt"), "2\n").unwrap();
    let changed = checkpoints::changed_files(&workspace).unwrap();
    let (found, skipped) = pullrequest::targets(&workspace, &changed);
    let named: Vec<(&str, &[String])> = found
        .iter()
        .map(|t| (t.name.as_str(), t.files.as_slice()))
        .collect();
    assert_eq!(named, [("api", &["a.txt".to_string()][..])]);
    assert_eq!(
        skipped,
        [("web".to_string(), "no origin remote to push to".to_string())]
    );
}

struct Repo {
    _temp: tempfile::TempDir,
    remote: PathBuf,
    project: PathBuf,
    calls: PathBuf,
}

fn repo() -> Repo {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let (remote, project, bin) = (
        root.join("remote.git"),
        root.join("project"),
        root.join("bin"),
    );
    identity(&root, "[init]\n\tdefaultBranch = main\n");
    git(&root, &["init", "-q", "--bare", &remote.to_string_lossy()]);
    std::fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    std::fs::write(project.join("app.py"), "print(1)\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "start"]);
    git(
        &project,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    git(&project, &["remote", "set-head", "origin", "main"]);
    std::fs::create_dir_all(&bin).unwrap();
    let calls = root.join("gh_calls");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\ncat >> '{calls}'\n\
         if [ \"$1 $2\" = \"pr view\" ]; then\n  echo \"$EXISTING_PR\"\n  [ -n \"$EXISTING_PR\" ] && exit 0\n  exit 1\nfi\n\
         echo https://github.com/example/project/pull/9\n",
        calls = calls.display()
    );
    let gh = bin.join("gh");
    std::fs::write(&gh, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::var("DONUT_TEST_REAL_PATH").unwrap_or_else(|_| {
        let real = std::env::var("PATH").unwrap_or_default();
        set("DONUT_TEST_REAL_PATH", &real);
        real
    });
    set("PATH", &format!("{}:{path}", bin.display()));
    Repo {
        _temp: temp,
        remote,
        project,
        calls,
    }
}

fn without_gh() {
    set(
        "PATH",
        &std::env::var("DONUT_TEST_REAL_PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|dir| !Path::new(dir).join("gh").exists())
            .collect::<Vec<_>>()
            .join(":"),
    );
}

#[cfg(unix)]
#[test]
fn test_a_branch_whose_worker_already_opened_a_pull_request_gets_no_second_one() {
    let _lock = guard();
    let repo = repo();
    git(&repo.project, &["switch", "-q", "-c", "feature/print-two"]);
    std::fs::write(repo.project.join("app.py"), "print(2)\n").unwrap();
    git(&repo.project, &["commit", "-q", "-am", "Print two"]); // The worker committed, as its ticket allowed.
    std::fs::write(repo.project.join("app.py"), "print(2)  # reviewed\n").unwrap(); // A fix made after the review.
    let existing = "https://github.com/example/project/pull/4";
    set("EXISTING_PR", existing);
    let url = pullrequest::raise_pull_request(
        &repo.project,
        "Print two",
        "body",
        &["app.py".to_string()],
    )
    .unwrap();
    set("EXISTING_PR", "");
    assert_eq!(url, existing);
    assert!(
        !std::fs::read_to_string(&repo.calls)
            .unwrap()
            .contains("pr create")
    );
    assert!(git(&repo.remote, &["show", "feature/print-two:app.py"]).contains("reviewed")); // The fix was pushed.
}

#[cfg(unix)]
#[test]
fn test_commits_only_the_tickets_files_on_a_new_branch_and_opens_the_pr() {
    let _lock = guard();
    let repo = repo();
    set("EXISTING_PR", "");
    std::fs::write(repo.project.join("app.py"), "print(2)\n").unwrap();
    std::fs::write(repo.project.join("notes.txt"), "my own unrelated edit\n").unwrap();
    let url = pullrequest::raise_pull_request(
        &repo.project,
        "Print two",
        "## Summary\nPrints 2.",
        &["app.py".to_string()],
    )
    .unwrap();
    assert_eq!(url, "https://github.com/example/project/pull/9");
    assert_eq!(
        git(&repo.project, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "donut/print-two"
    );
    let shown = git(
        &repo.project,
        &["show", "--name-only", "--format=%s", "HEAD"],
    );
    assert_eq!(
        shown.split_whitespace().collect::<Vec<_>>(),
        ["Print", "two", "app.py"]
    );
    assert!(git(&repo.project, &["status", "--porcelain"]).contains("notes.txt")); // Left alone, uncommitted.
    assert!(git(&repo.remote, &["branch"]).contains("donut/print-two"));
    let call = std::fs::read_to_string(&repo.calls).unwrap();
    assert!(
        call.contains("pr create --base main --head donut/print-two --title Print two"),
        "{call}"
    );
    assert!(call.contains("## Summary\nPrints 2."), "{call}");
}

#[cfg(unix)]
#[test]
fn test_without_a_remote_or_gh_it_says_why() {
    let _lock = guard();
    let repo = repo();
    git(&repo.project, &["remote", "remove", "origin"]);
    let error = pullrequest::raise_pull_request(&repo.project, "t", "b", &[]).unwrap_err();
    assert!(error.to_string().contains("no origin remote"), "{error}");
    git(
        &repo.project,
        &["remote", "add", "origin", &repo.remote.to_string_lossy()],
    );
    without_gh();
    let error = pullrequest::raise_pull_request(&repo.project, "t", "b", &[]).unwrap_err();
    assert!(error.to_string().contains("gh"), "{error}");
}
