//! Tickets side by side: each worker gets its own git worktree, and its changes go back
//! to the project as a patch.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::checkpoints::{git, git_checked, repositories, tail};
use crate::platform::run_with_input;
use crate::tickets::LANES;
use crate::ui::Progress;

/// What Donut installs that a project may not commit. A worktree only holds committed files, so these are copied.
pub const DONUT_FILES: [&str; 6] = [
    ".donut/skills",
    ".qwen/skills",
    "skills",
    "QWEN.md",
    "CLAUDE.md",
    "AGENTS.md",
];

/// Refuse a project that worktrees cannot copy: it must be the top folder of exactly one git repository.
pub fn check(project: &Path) -> Result<()> {
    let repos = repositories(project);
    let top = repos.len() == 1
        && repos[0].1.is_empty()
        && git(project, ["rev-parse", "--show-prefix"])
            .stdout
            .trim()
            .is_empty();
    if !top {
        bail!(
            "{} must be the top folder of one git repository to run tickets in parallel; drop --parallel, or run it \
             from the repository's top folder",
            project.display()
        );
    }
    Ok(())
}

/// The commit worktrees start from, or None in a repository with no commit yet.
pub fn base(project: &Path) -> Option<String> {
    let result = git(project, ["rev-parse", "--verify", "--quiet", "HEAD"]);
    (result.code == 0).then(|| result.stdout.trim().to_string())
}

/// Whether two tickets' file lists share a path, or one lists a folder that holds a path of the other.
pub fn overlap(first: &[String], second: &[String]) -> bool {
    first.iter().any(|a| {
        second.iter().any(|b| {
            a == b
                || a.starts_with(&format!("{}/", b.trim_end_matches('/')))
                || b.starts_with(&format!("{}/", a.trim_end_matches('/')))
        })
    })
}

/// Copy a file with its modification time (shutil.copy2).
pub fn copy_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::copy(source, target)?;
    if let Ok(modified) = std::fs::metadata(source).and_then(|meta| meta.modified()) {
        File::options()
            .write(true)
            .open(target)?
            .set_modified(modified)?;
    }
    Ok(())
}

/// Copy a folder with everything in it (shutil.copytree); the target must not exist yet.
pub fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    if target.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists", target.display()),
        ));
    }
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let (from, to) = (entry.path(), target.join(entry.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to)?
        } else {
            copy_file(&from, &to)?
        }
    }
    if let Ok(permissions) = std::fs::metadata(source).map(|meta| meta.permissions()) {
        let _ = std::fs::set_permissions(target, permissions);
    }
    Ok(())
}

/// A detached worktree of commit in folder, with the session's tickets and Donut's uncommitted files copied in.
///
/// Returns (worktree, copied): copied are the project-relative paths that came from the project and not from
/// git, which merge() leaves out of the patch.
pub fn add(
    project: &Path,
    lanes: &Path,
    folder: &Path,
    commit: &str,
) -> Result<(PathBuf, Vec<String>)> {
    if let Some(parent) = folder.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result = git(
        project,
        [
            "worktree",
            "add",
            "--detach",
            &folder.to_string_lossy(),
            commit,
        ],
    );
    if result.code != 0 {
        let said = if result.stderr.is_empty() {
            &result.stdout
        } else {
            &result.stderr
        };
        bail!("git worktree add failed: {}", tail(said.trim(), 500));
    }
    let tickets = lanes.strip_prefix(project)?;
    let _ = std::fs::remove_dir_all(folder.join(tickets)); // The project's copy is newer than the committed one.
    copy_tree(lanes, &folder.join(tickets))?;
    let parts: Vec<String> = tickets
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let mut copied = vec![parts.join("/")];
    for name in DONUT_FILES {
        let (source, target) = (project.join(name), folder.join(name));
        if source.exists() && !target.exists() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if source.is_dir() {
                copy_tree(&source, &target)?
            } else {
                copy_file(&source, &target)?
            }
            copied.push(name.to_string());
        }
    }
    Ok((folder.to_path_buf(), copied))
}

/// Apply what the worker changed in its worktree to the project's working tree. Returns whether it changed
/// anything; fails, with the project untouched, when the patch does not apply.
pub fn merge(project: &Path, worktree: &Path, commit: &str, copied: &[String]) -> Result<bool> {
    let mut paths = vec![".".to_string()];
    paths.extend(copied.iter().map(|path| format!(":(exclude){path}")));
    let mut add = vec!["add".to_string(), "--all".into(), "--".into()];
    add.extend(paths.iter().cloned());
    git_checked(worktree, &add)?;
    let mut diff = vec![
        "diff".to_string(),
        "--cached".into(),
        "--binary".into(),
        commit.to_string(),
        "--".into(),
    ];
    diff.extend(paths);
    let made = Command::new("git")
        .args(&diff)
        .current_dir(worktree)
        .output()?;
    if !made.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&made.stderr).trim()
        );
    }
    let patch = made.stdout;
    if patch.iter().all(u8::is_ascii_whitespace) {
        return Ok(false);
    }
    let result = run_with_input(
        Command::new("git")
            .args(["apply", "--binary", "--whitespace=nowarn", "-"])
            .current_dir(project),
        &patch,
    )?;
    if !result.status.success() {
        let said = String::from_utf8_lossy(&result.stderr).trim().to_string();
        bail!(
            "{}",
            if said.is_empty() {
                "git apply failed".to_string()
            } else {
                tail(&said, 500)
            }
        );
    }
    Ok(true)
}

/// Move the project's ticket to the lane its worker left it in, with the worker's notes.
pub fn take_ticket(lanes: &Path, worked: &Path, name: &str) {
    let found: Vec<PathBuf> = LANES
        .iter()
        .map(|lane| worked.join(lane).join(name))
        .filter(|path| path.exists())
        .collect();
    if found.len() != 1 {
        return;
    }
    for lane in LANES {
        let _ = std::fs::remove_file(lanes.join(lane).join(name));
    }
    let lane = found[0]
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    let _ = std::fs::create_dir_all(lanes.join(&lane));
    let _ = copy_file(&found[0], &lanes.join(&lane).join(name));
}

pub fn remove(project: &Path, worktree: &Path) {
    git(
        project,
        ["worktree", "remove", "--force", &worktree.to_string_lossy()],
    );
    let _ = std::fs::remove_dir_all(worktree);
    git(project, ["worktree", "prune"]);
}

/// One ticket to run in parallel: its name, the file its console output goes to, and the call that runs it.
///
/// Python swapped sys.stdout per thread; Rust code prints with println!, so the call gets its console file and
/// writes there instead.
pub struct Job<'a, T> {
    pub name: String,
    pub console: PathBuf,
    pub call: Box<dyn FnOnce(&mut File) -> T + Send + 'a>,
}

/// Run every job at once; returns each call's result, in order.
///
/// What a job prints goes to its console file, so the tickets' output does not mix. When interrupted becomes true
/// (Ctrl-C), stop() ends the running agents; the jobs are still waited for and the result is an error.
pub fn run<T: Send>(
    jobs: Vec<Job<'_, T>>,
    stop: &(dyn Fn() + Sync),
    interrupted: &AtomicBool,
) -> Result<Vec<T>> {
    let mut progress = Progress::new();
    let mut stopped = false;
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let Job {
                    name,
                    console,
                    call,
                } = job;
                let handle = scope.spawn(move || -> std::io::Result<T> {
                    let mut file = File::create(console)?;
                    Ok(call(&mut file))
                });
                (name, handle)
            })
            .collect();
        let mut reported = vec![false; handles.len()];
        let mut last_tick = Instant::now() - Duration::from_secs(1);
        loop {
            let mut pending = 0;
            for (at, (name, handle)) in handles.iter().enumerate() {
                if handle.is_finished() {
                    if !reported[at] {
                        reported[at] = true;
                        progress.clear();
                        println!("[parallel] {name}: worker finished");
                    }
                } else {
                    pending += 1;
                }
            }
            if pending == 0 {
                break;
            }
            if interrupted.load(Ordering::SeqCst) && !stopped {
                stopped = true;
                stop();
            }
            if last_tick.elapsed() >= Duration::from_millis(200) {
                last_tick = Instant::now();
                progress.tick(&format!("{pending} in parallel"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        progress.clear();
        handles
            .into_iter()
            .map(|(_, handle)| handle.join())
            .collect::<Vec<_>>()
    });
    if stopped {
        bail!("interrupted");
    }
    let mut out = Vec::new();
    for result in results {
        match result {
            Ok(value) => out.push(value?),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_overlap_of_ticket_file_lists() {
        assert!(overlap(&strings(&["a.py"]), &strings(&["b.py", "a.py"])));
        assert!(overlap(&strings(&["src/"]), &strings(&["src/a.py"])));
        assert!(overlap(&strings(&["src/a.py"]), &strings(&["src"])));
        assert!(!overlap(
            &strings(&["src/a.py"]),
            &strings(&["src/a.pyi", "srcs/a.py"])
        ));
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        git_checked(&project, ["init", "-q", "-b", "main"]).unwrap();
        git_checked(&project, ["config", "user.email", "t@example.com"]).unwrap();
        git_checked(&project, ["config", "user.name", "T"]).unwrap();
        // Windows git defaults to autocrlf, which would turn the patch's LF files into CRLF.
        git_checked(&project, ["config", "core.autocrlf", "false"]).unwrap();
        std::fs::write(project.join("app.py"), "print(1)\n").unwrap();
        git_checked(&project, ["add", "-A"]).unwrap();
        git_checked(&project, ["commit", "-q", "-m", "start"]).unwrap();
        (temp, project)
    }

    #[test]
    fn test_a_worktree_gets_the_tickets_and_its_changes_come_back_as_a_patch() {
        let (temp, project) = repo();
        check(&project).unwrap();
        let commit = base(&project).unwrap();
        let lanes = crate::workspace::create(&project, Some("s1")).unwrap();
        std::fs::write(lanes.join("to_do/001_a.md"), "ticket\n").unwrap();
        std::fs::write(project.join("QWEN.md"), "# notes\n").unwrap();
        let folder = temp.path().join("trees/one");
        let (worktree, copied) = add(&project, &lanes, &folder, &commit).unwrap();
        assert_eq!(copied, [".donut/s1", "QWEN.md"]);
        assert!(worktree.join(".donut/s1/to_do/001_a.md").is_file());
        std::fs::write(worktree.join("app.py"), "print(2)\n").unwrap();
        std::fs::write(worktree.join("new.py"), "x = 1\n").unwrap();
        std::fs::rename(
            worktree.join(".donut/s1/to_do/001_a.md"),
            worktree.join(".donut/s1/done/001_a.md"),
        )
        .unwrap();
        assert!(merge(&project, &worktree, &commit, &copied).unwrap());
        assert_eq!(
            std::fs::read_to_string(project.join("app.py")).unwrap(),
            "print(2)\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("new.py")).unwrap(),
            "x = 1\n"
        );
        take_ticket(&lanes, &worktree.join(".donut/s1"), "001_a.md");
        assert!(lanes.join("done/001_a.md").is_file());
        assert!(!lanes.join("to_do/001_a.md").exists());
        assert!(merge(&project, &worktree, &commit, &copied).is_err()); // The same patch does not apply twice.
        remove(&project, &worktree);
        assert!(!worktree.exists());
    }

    #[test]
    fn test_check_refuses_a_folder_that_is_not_a_repository_top() {
        let temp = tempfile::tempdir().unwrap();
        assert!(check(temp.path()).is_err());
    }

    #[test]
    fn test_run_returns_every_result_in_order_and_writes_each_console() {
        let temp = tempfile::tempdir().unwrap();
        let jobs: Vec<Job<'_, usize>> = (0..3)
            .map(|number| Job {
                name: format!("job{number}"),
                console: temp.path().join(format!("{number}.log")),
                call: Box::new(move |console: &mut File| {
                    use std::io::Write;
                    writeln!(console, "hello {number}").unwrap();
                    number * 2
                }),
            })
            .collect();
        let interrupted = AtomicBool::new(false);
        let results = run(jobs, &|| {}, &interrupted).unwrap();
        assert_eq!(results, [0, 2, 4]);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("1.log")).unwrap(),
            "hello 1\n"
        );
    }
}
