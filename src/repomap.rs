//! A compact map of the project for a worker: the symbols near its ticket's files, then
//! the other folders.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

use crate::checkpoints::SKIP;
use crate::tickets::splitlines;

/// Characters, about a thousand tokens: small enough for a local model's context.
pub const LIMIT: usize = 4000;
pub const MAX_FILES: usize = 20000;
pub const MAX_BYTES: u64 = 200_000;

// shortcut: one regex for the common declaration keywords, no parser; C-style methods without a keyword are
// not listed. Upgrade to tree-sitter if workers miss symbols.
static SYMBOL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^\s*(?:export\s+)?(?:default\s+)?(?:pub(?:\([a-z]+\))?\s+)?(?:public\s+|private\s+|internal\s+)?",
        r"(?:static\s+)?(?:abstract\s+)?(?:async\s+)?",
        r"(class|def|function|func|fn|struct|enum|interface|trait|type|module)\s+([A-Za-z_][A-Za-z0-9_]*)"
    ))
    .unwrap()
});

fn dirname(path: &str) -> &str {
    path.rfind('/').map_or("", |at| &path[..at])
}

fn walk(folder: &Path, relative: &str, found: &mut Vec<String>) {
    if found.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let (mut dirs, mut names) = (Vec::new(), Vec::new());
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.path().is_dir();
        if is_dir {
            dirs.push(name);
        } else {
            names.push(name);
        }
    }
    dirs.retain(|d| !d.starts_with('.') && !SKIP.contains(&d.as_str()));
    dirs.sort();
    names.sort();
    for name in names {
        found.push(if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        });
    }
    for dir in dirs {
        if found.len() >= MAX_FILES {
            return;
        }
        let is_link = std::fs::symlink_metadata(folder.join(&dir))
            .is_ok_and(|meta| meta.file_type().is_symlink());
        if is_link {
            continue;
        }
        let child = if relative.is_empty() {
            dir.clone()
        } else {
            format!("{relative}/{dir}")
        };
        walk(&folder.join(&dir), &child, found);
    }
}

/// Project-relative paths: what git tracks or would track, else a walk that skips dependency folders.
pub fn files(project: &Path) -> Vec<String> {
    if let Ok(output) = Command::new("git")
        .args(["ls-files", "-co", "--exclude-standard"])
        .current_dir(project)
        .output()
        && output.status.success()
    {
        let text = crate::checkpoints::universal_newlines(&output.stdout);
        let mut listed: Vec<String> = splitlines(&text).into_iter().map(str::to_string).collect();
        listed.sort();
        listed.truncate(MAX_FILES);
        return listed;
    }
    let mut found = Vec::new();
    walk(project, "", &mut found);
    found.truncate(MAX_FILES);
    found
}

/// The declarations in one source file, in order: ["class Ticket", "def pick"].
pub fn symbols(path: &Path) -> Vec<String> {
    if std::fs::metadata(path).map_or(true, |meta| meta.len() > MAX_BYTES) {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut found: Vec<String> = Vec::new();
    for line in splitlines(&text) {
        if let Some(caps) = SYMBOL.captures(line) {
            let symbol = format!("{} {}", &caps[1], &caps[2]);
            if !found.contains(&symbol) {
                found.push(symbol);
            }
        }
    }
    found
}

/// The map as text, at most limit characters; "" for an empty project.
///
/// focus is the ticket's paths. Those files come first with their symbols, then the other files of the same
/// folders with theirs, then every other folder with its file count. skip is path prefixes to leave out (the
/// tickets folder).
pub fn build(project: &Path, focus: &[String], skip: &[String], limit: usize) -> String {
    let paths: Vec<String> = files(project)
        .into_iter()
        .filter(|p| !skip.iter().any(|s| p.starts_with(s.as_str())))
        .collect();
    let focus: Vec<&str> = focus.iter().map(|f| f.trim_matches('/')).collect();
    let wanted: Vec<&String> = paths
        .iter()
        .filter(|p| {
            focus
                .iter()
                .any(|f| *p == f || p.starts_with(&format!("{f}/")))
        })
        .collect();
    let wanted_set: HashSet<&String> = wanted.iter().copied().collect();
    let folders: HashSet<&str> = wanted.iter().map(|p| dirname(p)).collect();
    let near: Vec<&String> = paths
        .iter()
        .filter(|p| !wanted_set.contains(p) && folders.contains(dirname(p)))
        .collect();
    let (mut lines, mut used) = (Vec::new(), 0usize);
    let budget = limit * 3 / 4; // Keep a quarter for the folder overview.
    for path in wanted.iter().chain(near.iter()) {
        let found = symbols(&project.join(path.as_str()));
        let mut line = if found.is_empty() {
            path.to_string()
        } else {
            format!("{path}: {}", found.join(", "))
        };
        if used + line.chars().count() > budget {
            line = path.to_string();
            if used + line.chars().count() > budget {
                break;
            }
        }
        used += line.chars().count() + 1;
        lines.push(line);
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for path in &paths {
        let folder = dirname(path);
        if !folders.contains(folder) {
            *counts.entry(folder).or_insert(0) += 1;
        }
    }
    for (folder, count) in counts {
        let line = format!(
            "{}/ ({count} file{})",
            if folder.is_empty() { "." } else { folder },
            if count == 1 { "" } else { "s" }
        );
        if used + line.chars().count() > limit {
            lines.push("\u{2026} more folders not listed".to_string());
            break;
        }
        used += line.chars().count() + 1;
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let sources = [
            (
                "app/models.py",
                "class Ticket:\n    def pick(self):\n        pass\n\nasync def load():\n    pass\n",
            ),
            ("app/views.py", "def index():\n    pass\n"),
            (
                "web/main.ts",
                "export function start() {}\nexport default class App {}\ninterface Options {}\n",
            ),
            ("web/logo.png", ""),
            ("node_modules/x/index.js", "function hidden() {}\n"),
            (
                ".donut/s1/to_do/001_a.md",
                "**Slug:** `a`\n**Depends on:** None\n\n## Files\n- `app/models.py`\n",
            ),
        ];
        for (name, text) in sources {
            let path = temp.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        temp
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_the_tickets_files_come_first_with_symbols_then_neighbours_then_folders() {
        let temp = project();
        let built = build(temp.path(), &strings(&["app/models.py"]), &[], LIMIT);
        let lines: Vec<&str> = built.lines().collect();
        assert_eq!(lines[0], "app/models.py: class Ticket, def pick, def load");
        assert_eq!(lines[1], "app/views.py: def index");
        assert!(lines.contains(&"web/ (2 files)"));
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("node_modules") || line.contains(".donut"))
        );
    }

    #[test]
    fn test_a_folder_in_the_ticket_lists_its_files_and_the_limit_holds() {
        let temp = project();
        let found = build(temp.path(), &strings(&["web/"]), &[], LIMIT);
        assert!(found.contains("web/main.ts: function start, class App, interface Options"));
        assert!(
            build(temp.path(), &strings(&["app/models.py"]), &[], 40)
                .chars()
                .count()
                <= 80
        );
        assert_eq!(
            build(temp.path(), &[], &strings(&["app/", "web/"]), LIMIT),
            ""
        );
    }
}
