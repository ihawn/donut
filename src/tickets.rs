//! Read swim-lane tickets tolerantly and explain why queued work is not runnable.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

pub const LANES: [&str; 5] = ["to_do", "in_progress", "on_hold", "blocked", "done"];
pub const RUNNABLE_LANES: [&str; 2] = ["in_progress", "to_do"];
pub const HELD_LANES: [&str; 2] = ["on_hold", "blocked"];

static NONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:(?:none|n/a|-)?\.?\s*(?:\(.*\))?)$").unwrap());
static SLUG_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9][A-Za-z0-9_.-]*").unwrap());
static PULL_REQUEST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:[-*]\s*)?PR:\s*<?(https?://[^\s>)]+)").unwrap());
static NOISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|/)__pycache__/|\.py[co]$|(^|/)\.DS_Store$").unwrap());
static DO_NOT_TOUCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:[-*]\s*)?\**do not touch").unwrap());
static BACKTICKED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\s]+)`").unwrap());
static UNTICKED_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[-*]\s*\[ \]").unwrap());
static UNTICKED_BOX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-*]\s*\[ \]\s*").unwrap());
static NEEDS_HUMAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"NEEDS HUMAN:\s*(.+)").unwrap());
static NEXT_SECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^##\s").unwrap());
static LEADING_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+").unwrap());
static SLUG_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+_(?:\d{8}-\d{4}_)?").unwrap());

/// Python's str.splitlines: split at \n, \r\n, \r and the other Unicode line boundaries.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if matches!(
            ch,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            lines.push(&text[start..at]);
            let mut end = at + ch.len_utf8();
            if ch == '\r'
                && let Some(&(next, '\n')) = chars.peek()
            {
                chars.next();
                end = next + 1;
            }
            start = end;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The pull requests a ticket records: its `PR: <url>` lines, in order, each once.
pub fn pull_requests(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in PULL_REQUEST.captures_iter(text) {
        let url = caps[1].to_string();
        if !found.contains(&url) {
            found.push(url);
        }
    }
    found
}

/// Record pull requests Donut opened in the ticket, as `PR: <url>` lines, so /prev can list them.
pub fn add_pull_requests(path: &Path, urls: &[String]) {
    // The pull request exists either way; only its line in the ticket is missing if this fails.
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let known = pull_requests(&text);
    let new: Vec<&String> = urls.iter().filter(|url| !known.contains(url)).collect();
    if !new.is_empty() {
        let mut out = text.trim_end_matches('\n').to_string() + "\n";
        for url in new {
            out += &format!("PR: {url}\n");
        }
        let _ = std::fs::write(path, out);
    }
}

/// The body of a `## Title` section, up to the next `## ` heading; "" when the ticket has none.
pub fn section(text: &str, title: &str) -> String {
    let heading = Regex::new(&format!(r"(?mi)^##\s+{}\b[^\n]*\n", regex::escape(title)))
        .expect("a valid heading pattern");
    let Some(found) = heading.find(text) else {
        return String::new();
    };
    let end = NEXT_SECTION
        .find_at(text, found.end())
        .map_or(text.len(), |next| next.start());
    text[found.end()..end].to_string()
}

/// The paths a ticket may change: the backtick-quoted paths of its Files section, without Do not touch lines.
pub fn scope_of(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in splitlines(&section(text, "Files")) {
        if DO_NOT_TOUCH.is_match(line) {
            continue;
        }
        for caps in BACKTICKED.captures_iter(line) {
            let token = caps[1].trim();
            if token.contains('/') || token.contains('.') {
                paths.push(token.to_string());
            }
        }
    }
    paths
}

/// Acceptance boxes the worker left unticked, apart from Human check items, which stay the user's.
pub fn unticked(text: &str) -> Vec<String> {
    splitlines(&section(text, "Acceptance criteria"))
        .into_iter()
        .filter(|line| UNTICKED_LINE.is_match(line) && !line.to_lowercase().contains("human check"))
        .map(|line| UNTICKED_BOX.replace(line, "").trim().to_string())
        .collect()
}

/// (files the ticket lists, other files) from project-relative paths.
///
/// A project folder can hold many repositories and other sessions, so a file that changed while a worker ran is
/// not always that worker's. Generated noise (bytecode, .DS_Store) is dropped. With no listed scope, every file
/// counts as the ticket's.
pub fn split_scope(
    project: &Path,
    scope: &[String],
    files: &[String],
) -> (Vec<String>, Vec<String>) {
    let files: Vec<String> = files
        .iter()
        .filter(|path| !NOISE.is_match(path))
        .cloned()
        .collect();
    if scope.is_empty() {
        return (files, Vec::new());
    }
    let resolved = dunce::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let prefix = resolved.to_string_lossy().trim_end_matches('/').to_string() + "/";
    let wanted: Vec<String> = scope
        .iter()
        .map(|path| match path.strip_prefix(&prefix) {
            Some(rest) => rest.to_string(),
            None => path.trim_start_matches(['.', '/']).to_string(),
        })
        .collect();
    let listed = |path: &str| {
        wanted.iter().any(|want| {
            if want.ends_with('/') {
                format!("/{path}").contains(&format!("/{want}"))
            } else {
                path == want
                    || path.ends_with(&format!("/{want}"))
                    || want.ends_with(&format!("/{path}"))
            }
        })
    };
    let mine: Vec<String> = files.iter().filter(|path| listed(path)).cloned().collect();
    let others = files
        .iter()
        .filter(|path| !mine.contains(path))
        .cloned()
        .collect();
    (mine, others)
}

/// Whether a file name matches the glob `[0-9]*_*.md`.
pub fn is_ticket_name(name: &str) -> bool {
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    let rest = chars.as_str();
    rest.find('_')
        .is_some_and(|at| rest.len() - (at + 1) >= 3 && rest.ends_with(".md"))
}

/// The ticket files of a folder, sorted by name.
pub fn ticket_files(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| is_ticket_name(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

/// A lane's tickets; lanes is the session's lanes folder (default: the project's swim_lanes/).
pub fn tasks(project: &Path, lane: &str, lanes: Option<&Path>) -> Vec<PathBuf> {
    let base = lanes.map_or_else(|| project.join("swim_lanes"), Path::to_path_buf);
    ticket_files(&base.join(lane))
}

/// The value of a `**Name:** value` line, or None when the line is absent.
pub fn field(text: &str, name: &str) -> Option<String> {
    let pattern = format!(
        r"(?m)^\s*(?:[-*]\s*)?\*\*{}:\*\*\s*(.*)$",
        regex::escape(name)
    );
    let found = Regex::new(&pattern).expect("a valid field pattern");
    found.captures(text).map(|caps| caps[1].trim().to_string())
}

/// Fallback identity for tickets without Slug metadata: `012_20240101-1200_my-slug.md` -> `my-slug`.
pub fn filename_slug(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    SLUG_PREFIX.replace(&stem, "").into_owned()
}

/// Accept backtick-quoted slugs, bare comma-separated slugs, or None.
pub fn parse_dependencies(value: Option<&str>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if NONE.is_match(value) {
        return Vec::new();
    }
    let quoted: Vec<String> = BACKTICKED_ANY
        .captures_iter(value)
        .map(|caps| caps[1].trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    if BACKTICKED_ANY.is_match(value) {
        return quoted;
    }
    SLUG_TOKEN
        .find_iter(value)
        .map(|m| m.as_str().to_string())
        .filter(|token| token.to_lowercase() != "none")
        .collect()
}

static BACKTICKED_ANY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());

/// A ticket file plus the metadata the runner needs. Never fails for malformed files.
#[derive(Debug, Clone)]
pub struct Ticket {
    pub path: PathBuf,
    pub lane: String,
    pub warnings: Vec<String>,
    pub pull_requests: Vec<String>,
    pub scope: Vec<String>,
    pub unticked: Vec<String>,
    pub needs_human: Vec<String>,
    pub title: String,
    pub summary: String,
    pub slug: String,
    pub dependencies: Vec<String>,
    pub verify: String,
}

impl Ticket {
    pub fn new(path: &Path, lane: &str) -> Ticket {
        let mut warnings = Vec::new();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                let message = error.to_string();
                let message = message
                    .split(" (os error")
                    .next()
                    .unwrap_or(&message)
                    .to_string();
                warnings.push(format!("unreadable ({message})"));
                String::new()
            }
        };
        let needs_human = NEEDS_HUMAN
            .captures_iter(&text)
            .map(|caps| caps[1].split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        // The heading, and the short description under it, for the /next and /prev tables.
        let lines: Vec<&str> = splitlines(&text).into_iter().map(str::trim).collect();
        let heading = lines.iter().position(|line| line.starts_with("# "));
        let title = heading.map_or("", |at| lines[at][2..].trim()).to_string();
        let summary = heading
            .map(|at| &lines[at + 1..])
            .unwrap_or(&[])
            .iter()
            .find(|line| {
                !line.is_empty()
                    && !["#", "**", "- **", "* **"]
                        .iter()
                        .any(|start| line.starts_with(start))
            })
            .map_or(String::new(), |line| line.to_string());
        let mut slug = field(&text, "Slug")
            .map(|value| value.trim_matches(['`', ' ']).to_string())
            .unwrap_or_default();
        if slug.is_empty() {
            slug = filename_slug(path);
            warnings.push(format!(
                "missing **Slug:**; using `{slug}` from the filename"
            ));
        }
        let depends = field(&text, "Depends on");
        if depends.is_none() {
            warnings.push("missing **Depends on:**; assuming none".to_string());
        }
        let dependencies = parse_dependencies(depends.as_deref());
        let mut verify = field(&text, "Verify")
            .map(|value| value.trim_matches(['`', ' ']).to_string())
            .unwrap_or_default();
        if !verify.is_empty() && NONE.is_match(&verify) {
            verify.clear();
        }
        Ticket {
            path: path.to_path_buf(),
            lane: lane.to_string(),
            warnings,
            pull_requests: pull_requests(&text),
            scope: scope_of(&text),
            unticked: unticked(&text),
            needs_human,
            title,
            summary,
            slug,
            dependencies,
            verify,
        }
    }

    /// The ticket's file name.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn cell(text: &str, width: usize) -> String {
    let text = text
        .replace('|', "/")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= width {
        text
    } else {
        text.chars().take(width - 1).collect::<String>() + "\u{2026}"
    }
}

/// A Markdown table of tickets: number, title, lane, overview and pull requests.
pub fn table(tickets: &[Ticket]) -> String {
    let mut rows = vec![
        "| # | Ticket | Status | Overview | PR |".to_string(),
        "|---|---|---|---|---|".to_string(),
    ];
    for ticket in tickets {
        let name = ticket.name();
        let number = LEADING_NUMBER.find(&name).map_or("", |m| m.as_str());
        let title = if ticket.title.is_empty() {
            &ticket.slug
        } else {
            &ticket.title
        };
        let overview = cell(&ticket.summary, 160);
        let prs = ticket.pull_requests.join(" ");
        rows.push(format!(
            "| {number} | {} | {} | {} | {} |",
            cell(title, 60),
            ticket.lane.replace('_', " "),
            if overview.is_empty() { "-" } else { &overview },
            if prs.is_empty() { "-" } else { &prs }
        ));
    }
    rows.join("\n")
}

fn first(found: Vec<Ticket>, count: Option<usize>) -> Vec<Ticket> {
    match count.filter(|count| *count > 0) {
        Some(count) => found.into_iter().take(count).collect(),
        None => found,
    }
}

/// Tickets still to do, in the order they run: in progress first, then to do. count keeps the first few.
pub fn upcoming(project: &Path, lanes: Option<&Path>, count: Option<usize>) -> Vec<Ticket> {
    let mut found: Vec<Ticket> = load(project, lanes)
        .into_iter()
        .filter(|ticket| RUNNABLE_LANES.contains(&ticket.lane.as_str()))
        .collect();
    found.sort_by_key(|ticket| RUNNABLE_LANES.iter().position(|lane| *lane == ticket.lane));
    first(found, count)
}

/// Tickets already worked (done or blocked), the latest number first. count keeps the latest few.
pub fn finished(project: &Path, lanes: Option<&Path>, count: Option<usize>) -> Vec<Ticket> {
    let mut found: Vec<Ticket> = load(project, lanes)
        .into_iter()
        .filter(|ticket| ticket.lane == "done" || ticket.lane == "blocked")
        .collect();
    found.sort_by_key(|ticket| std::cmp::Reverse(ticket.name()));
    first(found, count)
}

/// Every ticket in every lane, in lane order then number order.
pub fn load(project: &Path, lanes: Option<&Path>) -> Vec<Ticket> {
    LANES
        .iter()
        .flat_map(|lane| {
            tasks(project, lane, lanes)
                .into_iter()
                .map(move |path| Ticket::new(&path, lane))
        })
        .collect()
}

fn index(tickets: &[Ticket]) -> (HashSet<String>, HashSet<String>) {
    let done = tickets
        .iter()
        .filter(|t| t.lane == "done")
        .map(|t| t.slug.clone())
        .collect();
    let held = tickets
        .iter()
        .filter(|t| HELD_LANES.contains(&t.lane.as_str()))
        .map(|t| t.slug.clone())
        .collect();
    (done, held)
}

/// Human-readable reasons a runnable-lane ticket cannot start right now.
pub fn blockers(
    ticket: &Ticket,
    tickets: &[Ticket],
    done: &HashSet<String>,
    held: &HashSet<String>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if held.contains(&ticket.slug) {
        reasons.push(format!(
            "slug `{}` is also in on_hold/ or blocked/",
            ticket.slug
        ));
    }
    let known: HashSet<&str> = tickets.iter().map(|t| t.slug.as_str()).collect();
    for dependency in &ticket.dependencies {
        if done.contains(dependency) && !held.contains(dependency) {
            continue;
        }
        if held.contains(dependency) {
            reasons.push(format!(
                "depends on `{dependency}`, which is on hold or blocked"
            ));
        } else if known.contains(dependency.as_str()) {
            reasons.push(format!("waiting for `{dependency}` to reach done/"));
        } else {
            reasons.push(format!("depends on unknown slug `{dependency}`"));
        }
    }
    reasons
}

/// The next runnable ticket: in-progress work first, then the lowest numbered eligible ticket.
pub fn pick(project: &Path, attempted: &HashSet<String>, lanes: Option<&Path>) -> Option<PathBuf> {
    let tickets = load(project, lanes);
    let (done, held) = index(&tickets);
    for lane in RUNNABLE_LANES {
        for ticket in &tickets {
            if ticket.lane != lane || attempted.contains(&ticket.name()) {
                continue;
            }
            if blockers(ticket, &tickets, &done, &held).is_empty() {
                return Some(ticket.path.clone());
            }
        }
    }
    None
}

/// Lines explaining every unrun ticket in to_do/ and in_progress/, plus metadata warnings in any lane.
pub fn queue_report(
    project: &Path,
    attempted: &HashSet<String>,
    lanes: Option<&Path>,
) -> Vec<String> {
    let tickets = load(project, lanes);
    let (done, held) = index(&tickets);
    let mut lines = Vec::new();
    for ticket in &tickets {
        for warning in &ticket.warnings {
            lines.push(format!("{}/{}: {warning}", ticket.lane, ticket.name()));
        }
    }
    for ticket in &tickets {
        if !RUNNABLE_LANES.contains(&ticket.lane.as_str()) || attempted.contains(&ticket.name()) {
            continue;
        }
        let reasons = blockers(ticket, &tickets, &done, &held);
        if !reasons.is_empty() {
            lines.push(format!(
                "{}/{}: {}",
                ticket.lane,
                ticket.name(),
                reasons.join("; ")
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Project {
        _temp: tempfile::TempDir,
        path: PathBuf,
    }

    fn project() -> Project {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_path_buf();
        for lane in LANES {
            std::fs::create_dir_all(path.join("swim_lanes").join(lane)).unwrap();
        }
        Project { _temp: temp, path }
    }

    impl Project {
        fn ticket(&self, name: &str, text: &str, lane: &str) -> PathBuf {
            let path = self.path.join("swim_lanes").join(lane).join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    fn numbers(tickets: &[Ticket]) -> Vec<String> {
        tickets.iter().map(|t| t.name()[..3].to_string()).collect()
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn test_next_and_prev_list_tickets_with_overview_and_pull_requests() {
        let project = project();
        let lanes = project.path.join("swim_lanes");
        for (number, lane) in [
            (1, "done"),
            (2, "blocked"),
            (3, "done"),
            (4, "in_progress"),
            (5, "to_do"),
            (6, "to_do"),
        ] {
            let text = format!(
                "# Ticket {number}\n\nDoes thing {number} | safely.\n\n**Slug:** `t{number}`\n**Depends on:** None\n\n## Goal\nx\n\n## Completion notes\n"
            );
            project.ticket(&format!("00{number}_t{number}.md"), &text, lane);
        }
        let done = lanes.join("done/003_t3.md");
        let text = std::fs::read_to_string(&done).unwrap();
        std::fs::write(&done, text + "- PR: https://github.com/o/r/pull/7\n").unwrap(); // Written by the worker.
        add_pull_requests(
            &done,
            &[
                "https://github.com/o/r/pull/7".to_string(),
                "https://github.com/o/r/pull/8".to_string(),
            ],
        );
        assert_eq!(
            Ticket::new(&done, "done").pull_requests,
            [
                "https://github.com/o/r/pull/7",
                "https://github.com/o/r/pull/8"
            ]
        );
        assert_eq!(
            numbers(&upcoming(&project.path, None, None)),
            ["004", "005", "006"]
        );
        assert_eq!(
            numbers(&upcoming(&project.path, None, Some(2))),
            ["004", "005"]
        );
        assert_eq!(
            numbers(&finished(&project.path, None, None)),
            ["003", "002", "001"]
        );
        assert_eq!(numbers(&finished(&project.path, None, Some(1))), ["003"]);
        let table = table(&finished(&project.path, None, Some(2)));
        let rows: Vec<&str> = table.lines().collect();
        assert_eq!(rows[0], "| # | Ticket | Status | Overview | PR |");
        assert_eq!(
            rows[2],
            "| 003 | Ticket 3 | done | Does thing 3 / safely. | https://github.com/o/r/pull/7 https://github.com/o/r/pull/8 |"
        );
        assert_eq!(
            rows[3],
            "| 002 | Ticket 2 | blocked | Does thing 2 / safely. | - |"
        );
    }

    #[test]
    fn test_a_ticket_with_unticked_checks_is_not_plain_done_and_others_files_are_not_its_own() {
        let project = project();
        let text = "# T\n\n**Slug:** `t1`\n**Depends on:** None\n\n## Files\n\n\
            - Modify: `api/app.py`, `api/tests/test_app.py` (in the worktree).\n\
            - Do not touch: `api/legacy.py`\n\n## Acceptance criteria\n\n\
            - [x] Unit tests pass.\n- [ ] Live check passes.\n- [ ] **Human check**: looks right.\n\n\
            ## Completion notes\n- NEEDS HUMAN: allow a second login today?\n";
        let ticket = Ticket::new(&project.ticket("001_t1.md", text, "blocked"), "blocked");
        assert_eq!(ticket.scope, ["api/app.py", "api/tests/test_app.py"]);
        assert_eq!(ticket.unticked, ["Live check passes."]); // The Human check stays the user's.
        assert_eq!(ticket.needs_human, ["allow a second login today?"]);
        let changed: Vec<String> = [
            "api/app.py",
            "api/helper.py",
            "api/__pycache__/app.cpython-313.pyc",
            "other/notes.py",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (mine, others) = split_scope(&project.path, &ticket.scope, &changed);
        assert_eq!(mine, ["api/app.py"]);
        assert_eq!(others, ["api/helper.py", "other/notes.py"]); // Bytecode is dropped.
        let (_, nothing) =
            split_scope(&project.path, &["nothing/matches.py".to_string()], &changed);
        assert_eq!(nothing, ["api/app.py", "api/helper.py", "other/notes.py"]);
        let (all, left) = split_scope(&project.path, &[], &changed);
        assert_eq!(all, ["api/app.py", "api/helper.py", "other/notes.py"]);
        assert!(left.is_empty());
    }

    #[test]
    fn test_dependency_forms() {
        assert!(parse_dependencies(None).is_empty());
        assert!(parse_dependencies(Some("None")).is_empty());
        assert!(parse_dependencies(Some("none.")).is_empty());
        assert!(parse_dependencies(Some("None (first step)")).is_empty());
        assert_eq!(
            parse_dependencies(Some("`a-slug`, `b.slug`")),
            ["a-slug", "b.slug"]
        );
        assert_eq!(
            parse_dependencies(Some("a-slug, b_slug")),
            ["a-slug", "b_slug"]
        );
    }

    #[test]
    fn test_missing_metadata_falls_back_and_warns() {
        let project = project();
        let path = project.ticket(
            "007_20240101-1200_fix-login.md",
            "# Fix login\n\nNo metadata at all.\n",
            "to_do",
        );
        let ticket = Ticket::new(&path, "to_do");
        assert_eq!(ticket.slug, "fix-login");
        assert!(ticket.dependencies.is_empty());
        assert_eq!(ticket.warnings.len(), 2);
        assert_eq!(pick(&project.path, &none(), None), Some(path));
    }

    #[test]
    fn test_bulleted_metadata_and_verify_field() {
        let project = project();
        let path = project.ticket(
            "001_a.md",
            "- **Slug:** `a`\n- **Depends on:** None\n- **Verify:** `pytest -q`\n",
            "to_do",
        );
        let ticket = Ticket::new(&path, "to_do");
        assert_eq!(
            (ticket.slug.as_str(), ticket.verify.as_str()),
            ("a", "pytest -q")
        );
        let other = project.ticket("002_b.md", "**Slug:** `b`\n**Verify:** none\n", "to_do");
        assert_eq!(Ticket::new(&other, "to_do").verify, "");
    }

    #[test]
    fn test_malformed_ticket_does_not_stop_other_work() {
        let project = project();
        project.ticket(
            "001_broken.md",
            "**Slug:** `broken`\n**Depends on:** `ghost`\n",
            "to_do",
        );
        project.ticket(
            "002_held.md",
            "**Slug:** `held`\n**Depends on:** None\n",
            "on_hold",
        );
        project.ticket(
            "003_waits.md",
            "**Slug:** `waits`\n**Depends on:** `held`\n",
            "to_do",
        );
        project.ticket(
            "004_later.md",
            "**Slug:** `later`\n**Depends on:** `fine`\n",
            "to_do",
        );
        let fine = project.ticket(
            "005_fine.md",
            "**Slug:** `fine`\n**Depends on:** None\n",
            "to_do",
        );
        project.ticket("006_nometa.md", "nothing\n", "done");
        assert_eq!(pick(&project.path, &none(), None), Some(fine));
        let report = queue_report(&project.path, &none(), None).join("\n");
        assert!(report.contains("001_broken.md: depends on unknown slug `ghost`"));
        assert!(report.contains("003_waits.md: depends on `held`, which is on hold or blocked"));
        assert!(report.contains("004_later.md: waiting for `fine` to reach done/"));
        assert!(report.contains("done/006_nometa.md: missing **Slug:**"));
        assert!(!report.contains("005_fine.md"));
        assert!(!report.replace("done/006", "").contains("002_held.md:"));
    }

    #[test]
    fn test_sections_and_names() {
        let text = "# T\n\n## Files\n- `a.py`\n\n## Goal\nx\n";
        assert_eq!(section(text, "files"), "- `a.py`\n\n");
        assert_eq!(section(text, "Missing"), "");
        assert!(is_ticket_name("001_a.md"));
        assert!(!is_ticket_name("a_001.md"));
        assert!(!is_ticket_name("001.md"));
        assert_eq!(splitlines("a\r\nb\rc\n"), ["a", "b", "c"]);
    }
}
