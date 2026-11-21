//! Optional git safety net around each ticket: record what changed, and commit it
//! on request.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

/// Folders never searched for nested repositories or source files.
pub const SKIP: &[&str] = &[
    "node_modules",
    "venv",
    "env",
    "__pycache__",
    "dist",
    "build",
    "target",
    "swim_lanes",
];
/// How many folders below the project a nested repository may sit.
pub const DEPTH: usize = 3;
/// Default size limit of diff().
pub const DIFF_LIMIT: usize = 30000;

/// What a git command printed and how it ended (text decoded as UTF-8 with replacement, newlines universal).
#[derive(Debug, Clone, Default)]
pub struct GitOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GitOut {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Python's text mode: "\r\n" and "\r" both read as "\n".
pub fn universal_newlines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// The last n characters of text.
pub fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

/// The first n characters of text.
pub fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// Run git in a folder. A git that cannot start gives code -1 and the error in stderr.
pub fn git<I, S>(project: &Path, args: I) -> GitOut
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    match Command::new("git")
        .args(args)
        .current_dir(project)
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) => GitOut {
            code: output.status.code().unwrap_or(-1),
            stdout: universal_newlines(&output.stdout),
            stderr: universal_newlines(&output.stderr),
        },
        Err(error) => GitOut {
            code: -1,
            stdout: String::new(),
            stderr: error.to_string(),
        },
    }
}

/// git(), but a failing command is an error (Python's check=True).
pub fn git_checked<I, S>(project: &Path, args: I) -> Result<GitOut>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().collect();
    let shown: Vec<String> = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect();
    let result = git(project, &args);
    if result.code != 0 {
        bail!(
            "git {} failed with exit status {}: {}",
            shown.join(" "),
            result.code,
            result.stderr.trim()
        );
    }
    Ok(result)
}

pub fn is_repository(project: &Path) -> bool {
    let result = git(project, ["rev-parse", "--is-inside-work-tree"]);
    result.code == 0 && result.stdout.trim() == "true"
}

fn walk(project: &Path, folder: &Path, depth: usize, found: &mut Vec<(PathBuf, String)>) {
    if depth > 0 && folder.join(".git").exists() {
        let relative = folder.strip_prefix(project).unwrap_or(folder);
        let parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        found.push((folder.to_path_buf(), parts.join("/") + "/"));
        return;
    }
    if depth >= DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.') && !SKIP.contains(&name.as_str()))
        .collect();
    names.sort();
    for name in names {
        walk(project, &folder.join(name), depth + 1, found);
    }
}

/// The git repositories a run in this project can change: [(root, prefix)].
///
/// The repository holding the project (prefix "") comes first, then each repository nested up to three folders
/// below it (prefix "services/api/"), so one project folder can hold several repositories.
pub fn repositories(project: &Path) -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    if is_repository(project) {
        found.push((project.to_path_buf(), String::new()));
    }
    walk(project, project, 0, &mut found);
    found
}

/// Changed paths in one repository, relative to root, leaving out the nested repositories themselves.
fn repository_changes(root: &Path, nested: &[String]) -> Vec<String> {
    let prefix = git(root, ["rev-parse", "--show-prefix"])
        .stdout
        .trim()
        .to_string();
    let result = git(
        root,
        ["status", "--porcelain", "--untracked-files=all", "--", "."],
    );
    if result.code != 0 {
        return Vec::new();
    }
    let mut paths = BTreeSet::new();
    for line in crate::tickets::splitlines(&result.stdout) {
        if line.trim().is_empty() {
            continue;
        }
        let rest: String = line.chars().skip(3).collect();
        let last = rest.rsplit(" -> ").next().unwrap_or("");
        let path = last.trim_matches('"');
        if !prefix.is_empty() && !path.starts_with(&prefix) {
            continue;
        }
        let path = &path[prefix.len()..];
        if nested
            .iter()
            .any(|n| path == n.trim_end_matches('/') || path.starts_with(n.as_str()))
        {
            continue;
        }
        paths.insert(path.to_string());
    }
    paths.into_iter().collect()
}

/// Paths with uncommitted changes (staged, unstaged or untracked), relative to the project, across every
/// repository in it; None when the project holds no git repository.
pub fn changed_files(project: &Path) -> Option<Vec<String>> {
    let repos = repositories(project);
    if repos.is_empty() {
        return None;
    }
    let nested: Vec<&String> = repos
        .iter()
        .map(|(_, prefix)| prefix)
        .filter(|prefix| !prefix.is_empty())
        .collect();
    let mut files = BTreeSet::new();
    for (root, prefix) in &repos {
        let inner: Vec<String> = nested
            .iter()
            .filter(|n| **n != prefix && n.starts_with(prefix.as_str()))
            .map(|n| n[prefix.len()..].to_string())
            .collect();
        for path in repository_changes(root, &inner) {
            files.insert(format!("{prefix}{path}"));
        }
    }
    Some(files.into_iter().collect())
}

/// Each repository's current commit, {root: sha}, taken before a ticket so its own commits can be told apart.
pub fn heads(project: &Path) -> HashMap<PathBuf, String> {
    let mut found = HashMap::new();
    for (root, _) in repositories(project) {
        let result = git(&root, ["rev-parse", "--verify", "--quiet", "HEAD"]);
        if result.code == 0 {
            found.insert(root, result.stdout.trim().to_string());
        }
    }
    found
}

/// Project-relative paths in commits made since before (from heads()), for a worker that committed its work.
pub fn committed_files(project: &Path, before: Option<&HashMap<PathBuf, String>>) -> Vec<String> {
    let mut files = BTreeSet::new();
    for (root, prefix) in repositories(project) {
        if let Some(old) = before
            .and_then(|map| map.get(&root))
            .filter(|old| !old.is_empty())
        {
            for name in
                crate::tickets::splitlines(&git(&root, ["diff", "--name-only", old, "HEAD"]).stdout)
            {
                if !name.is_empty() {
                    files.insert(format!("{prefix}{name}"));
                }
            }
        }
    }
    files.into_iter().collect()
}

/// Split project-relative paths by repository: [(root, prefix, [paths relative to root])], deepest match wins.
pub fn group(project: &Path, files: &[String]) -> Vec<(PathBuf, String, Vec<String>)> {
    let mut repos = repositories(project);
    repos.sort_by_key(|(_, prefix)| std::cmp::Reverse(prefix.chars().count()));
    let mut groups: BTreeMap<String, (PathBuf, Vec<String>)> = BTreeMap::new();
    for path in files {
        if let Some((root, prefix)) = repos
            .iter()
            .find(|(_, prefix)| path.starts_with(prefix.as_str()))
        {
            groups
                .entry(prefix.clone())
                .or_insert_with(|| (root.clone(), Vec::new()))
                .1
                .push(path[prefix.len()..].to_string());
        }
    }
    groups
        .into_iter()
        .map(|(prefix, (root, mut paths))| {
            paths.sort();
            (root, prefix, paths)
        })
        .collect()
}

/// Commit every working-tree change as one checkpoint in each repository that changed.
///
/// Returns the short hash (several joined by ", " when more than one repository changed), or None when nothing
/// changed.
pub fn commit(project: &Path, message: &str) -> Result<Option<String>> {
    if repositories(project).is_empty() {
        bail!(
            "{} is not a git repository; drop --checkpoint or run git init",
            project.display()
        );
    }
    let mut hashes = Vec::new();
    for (root, _, paths) in group(project, &changed_files(project).unwrap_or_default()) {
        let mut add = vec!["add".to_string(), "--all".into(), "--".into()];
        add.extend(paths);
        git_checked(&root, &add)?;
        if git(&root, ["diff", "--cached", "--quiet"]).code == 0 {
            continue;
        }
        let mut args: Vec<&str> = Vec::new();
        if git(&root, ["config", "user.email"])
            .stdout
            .trim()
            .is_empty()
        {
            args.extend(["-c", "user.name=Donut", "-c", "user.email=donut@localhost"]);
        }
        args.extend(["commit", "--quiet", "--no-verify", "-m", message]);
        let result = git(&root, &args);
        if result.code != 0 {
            let said = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            bail!("git commit failed: {}", tail(said.trim(), 500));
        }
        hashes.push(
            git(&root, ["rev-parse", "--short", "HEAD"])
                .stdout
                .trim()
                .to_string(),
        );
    }
    Ok(if hashes.is_empty() {
        None
    } else {
        Some(hashes.join(", "))
    })
}

/// A unified diff of the given paths against HEAD, for a reviewer, one section per repository.
///
/// before (from heads()) is each repository's commit when the ticket started: the diff is taken against it, so
/// work the worker committed is still shown. New untracked files are listed by name.
pub fn diff(
    project: &Path,
    files: &[String],
    limit: usize,
    before: Option<&HashMap<PathBuf, String>>,
) -> String {
    if files.is_empty() {
        return String::new();
    }
    let groups = group(project, files);
    let mut text = String::new();
    for (root, prefix, paths) in &groups {
        if groups.len() > 1 || !prefix.is_empty() {
            let shown = if prefix.is_empty() { "./" } else { prefix };
            text += &format!("\n# Repository {shown}\n");
        }
        let mut list = vec!["ls-files".to_string(), "--".into()];
        list.extend(paths.iter().cloned());
        let listed = git(root, &list).stdout;
        let tracked: Vec<&str> = listed.split_whitespace().collect();
        let has_head = git(root, ["rev-parse", "--verify", "HEAD"]).code == 0;
        let base = before
            .and_then(|map| map.get(root))
            .map_or("HEAD", String::as_str);
        if !tracked.is_empty() && has_head {
            let mut args = vec!["diff".to_string(), base.to_string(), "--".into()];
            args.extend(tracked.iter().map(|path| path.to_string()));
            text += &git(root, &args).stdout;
        }
        let new: Vec<String> = paths
            .iter()
            .filter(|path| !tracked.contains(&path.as_str()))
            .map(|path| format!("{prefix}{path}"))
            .collect();
        if !new.is_empty() {
            text += "\nNew files (read them directly):\n";
            text += &new
                .iter()
                .map(|path| format!("  {path}"))
                .collect::<Vec<_>>()
                .join("\n");
            text += "\n";
        }
    }
    if text.chars().count() > limit {
        text = head(&text, limit)
            + &format!(
                "\n\u{2026} diff truncated at {limit} characters; read the files for the rest.\n"
            );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(project: &Path) {
        git_checked(project, ["init", "-q", "-b", "main"]).unwrap();
        git_checked(project, ["config", "user.email", "t@example.com"]).unwrap();
        git_checked(project, ["config", "user.name", "T"]).unwrap();
    }

    #[test]
    fn test_outside_git_is_reported_not_raised() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!is_repository(temp.path()));
        assert_eq!(changed_files(temp.path()), None);
        assert!(commit(temp.path(), "x").is_err());
    }

    #[test]
    fn test_the_review_sees_work_the_worker_committed() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        init(project);
        std::fs::write(project.join("app.py"), "print(1)\n").unwrap();
        git_checked(project, ["add", "-A"]).unwrap();
        git_checked(project, ["commit", "-q", "-m", "start"]).unwrap();
        let before = heads(project);
        git_checked(project, ["switch", "-q", "-c", "feature/two"]).unwrap();
        std::fs::write(project.join("app.py"), "print(2)\n").unwrap();
        git_checked(project, ["commit", "-q", "-am", "two"]).unwrap(); // Nothing is left uncommitted.
        assert_eq!(changed_files(project), Some(Vec::new()));
        assert_eq!(committed_files(project, Some(&before)), ["app.py"]);
        let files = vec!["app.py".to_string()];
        assert!(diff(project, &files, DIFF_LIMIT, Some(&before)).contains("+print(2)"));
        assert_eq!(diff(project, &files, DIFF_LIMIT, None), ""); // Against HEAD alone it would be empty.
    }

    #[test]
    fn test_a_repository_inside_a_repository_stays_separate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join("libs/shared")).unwrap();
        init(&root);
        init(&root.join("libs/shared"));
        std::fs::write(root.join("app.py"), "1\n").unwrap();
        std::fs::write(root.join("libs/shared/util.py"), "1\n").unwrap();
        assert_eq!(
            changed_files(&root).unwrap(),
            ["app.py", "libs/shared/util.py"]
        );
        commit(&root, "checkpoint").unwrap();
        // The outer repository did not swallow the inner one.
        let shown = git_checked(&root, ["show", "--name-only", "--format=", "HEAD"])
            .unwrap()
            .stdout;
        assert_eq!(shown.split_whitespace().collect::<Vec<_>>(), ["app.py"]);
    }

    #[test]
    fn test_group_splits_paths_by_repository_and_the_diff_is_cut_at_the_limit() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        for name in ["api", "web"] {
            std::fs::create_dir_all(workspace.join(name)).unwrap();
            init(&workspace.join(name));
            std::fs::write(workspace.join(name).join("a.txt"), "1\n").unwrap();
        }
        std::fs::write(workspace.join("notes.md"), "not in any repository\n").unwrap();
        assert!(!is_repository(workspace));
        assert_eq!(
            changed_files(workspace).unwrap(),
            ["api/a.txt", "web/a.txt"]
        );
        let files = vec!["api/a.txt".to_string(), "web/a.txt".to_string()];
        let groups = group(workspace, &files);
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
        let text = diff(workspace, &files, 20, None);
        assert!(text.contains("diff truncated at 20 characters"));
    }
}
