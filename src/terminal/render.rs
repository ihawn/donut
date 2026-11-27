//! Pure helpers of the terminal: constants, formatting, Markdown, approvals, shell completion and images.
//!
//! Terminal helpers that need no app state.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

pub use crate::highlight::{CORAL, DIM, GRAY, ORANGE, RULE, TEAL};

use super::{Link, RichLine};

/// The palette's navy (`ui::color("navy")`).
pub const NAVY: Color = Color::Rgb(18, 77, 99);
/// The terminal background, also the dark text on a bright badge.
pub const BADGE_DARK: Color = Color::Rgb(0x10, 0x18, 0x20);

// ---------------------------------------------------------------------------------------------------------
// Regexes

/// A progress line the batch runner prints; it belongs in the status bar, not the transcript.
pub static RUNNER_PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(?P<label>[^\]]*?) · (?P<seconds>\d+)s elapsed · waiting for agent output\]$")
        .expect("regex")
});
pub static RUNNER_SPEND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Spend: (?:\$(?P<dollars>[0-9.]+)|not priced) \((?P<model>.*), (?P<where>\w+)\)$")
        .expect("regex")
});
pub static RUNNER_TICKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\d+/(?:\d+|all)\] (?P<ticket>\S+)").expect("regex"));
pub static RUNNER_REVIEW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[review \d+/\d+\] (?P<ticket>\S+)").expect("regex"));
pub static RUNNER_RECOVER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[recover\] (?P<ticket>\S+)").expect("regex"));
pub static ANSI_ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("regex"));
pub static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s<>()\[\]{}`'"]*[^\s<>()\[\]{}`'".,;:!?*]"#).expect("regex")
});

// ---------------------------------------------------------------------------------------------------------
// Constants

/// Each mode talks to the model of the role with the same key.
pub const ROLE_TITLES: &[(&str, &str)] = &[
    ("plan", "planner"),
    ("work", "worker"),
    ("review", "reviewer"),
    ("solo", "soloist"),
];
pub const ROLE_COMMANDS: &[(&str, &str)] = &[
    ("plan", "/planner"),
    ("work", "/worker"),
    ("review", "/reviewer"),
    ("solo", "/soloist"),
];

/// The title of a role, or the role itself when unknown.
pub fn role_title(role: &str) -> &str {
    ROLE_TITLES
        .iter()
        .find(|(key, _)| *key == role)
        .map_or(role, |(_, title)| title)
}

pub fn role_command(role: &str) -> &'static str {
    ROLE_COMMANDS
        .iter()
        .find(|(key, _)| *key == role)
        .map_or("", |(_, command)| command)
}

pub fn mode_placeholder(mode: &str) -> &'static str {
    match mode {
        "review" => {
            "Ask the reviewer to check something (it reads, never edits), or type / for commands"
        }
        "solo" => "Ask the soloist anything or tell it what to change, or type / for commands",
        _ => "Describe what to plan, or type / for commands",
    }
}

pub fn mode_badge(mode: &str) -> Color {
    match mode {
        "plan" => ORANGE,
        "work" => CORAL,
        "review" => NAVY,
        _ => TEAL,
    }
}

pub const REVIEW_MODE_PROMPT: &str = "You are the reviewer. Review what is asked below by reading the project; run read-only checks if useful. \
Do not edit, create, move or delete files, and do not commit. Point out real problems with file and line, \
say how serious each is, and say plainly when something looks right.\n\nReview request:\n";

pub const SIMPLE_PLACEHOLDER: &str = "Ask anything or say what to change, or type / for commands";

pub const SPINNER: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/// Friendlier names for the activity events providers report.
pub const ACTIVITY_NAMES: &[(&str, &str)] = &[
    ("reasoning", "thinking"),
    ("agentMessage", "writing"),
    ("commandExecution", "running a command"),
    ("fileChange", "editing files"),
    ("mcpToolCall", "calling a tool"),
    ("webSearch", "searching the web"),
    ("Bash", "running a command"),
    ("Read", "reading files"),
    ("Glob", "searching files"),
    ("Grep", "searching files"),
    ("Edit", "editing files"),
    ("MultiEdit", "editing files"),
    ("Write", "writing a file"),
    ("WebFetch", "fetching a page"),
    ("WebSearch", "searching the web"),
    ("TodoWrite", "updating its plan"),
    ("Task", "running a subagent"),
    ("read_file", "reading files"),
    ("write_file", "writing a file"),
    ("edit", "editing files"),
    ("run_shell_command", "running a command"),
];

pub fn activity_name(name: &str) -> Option<&'static str> {
    ACTIVITY_NAMES
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
}

/// Commands that only show something (or change only what is shown), so they run at once even mid-turn.
pub const INSTANT_COMMANDS: &[&str] = &[
    "/usage",
    "/help",
    "/copy",
    "/context",
    "/mcp",
    "/plugin",
    "/verbosity",
    "/routing",
    "/focus",
    "/chrome",
    "/handholding",
    "/next",
    "/prev",
];

/// The two versions of the interface: (key, (label, what it says beside it)).
pub const INTERFACES: &[(&str, (&str, &str))] = &[
    (
        "simple",
        (
            "Simple mode",
            "I just want to use this like I would use Claude Code or Codex",
        ),
    ),
    (
        "complex",
        (
            "Complex mode",
            "I know what I'm doing. Show all complex features",
        ),
    ),
];

/// What simple mode leaves out: the roles, modes and tickets.
pub const COMPLEX_COMMANDS: &[&str] = &[
    "/plan",
    "/work",
    "/review",
    "/solo",
    "/planner",
    "/worker",
    "/reviewer",
    "/soloist",
    "/handholding",
    "/session",
    "/next",
    "/prev",
];

/// Commands that talk to the current model's provider, so they need a model to have been picked.
pub const NEEDS_MODEL: &[&str] = &[
    "/plugin", "/mcp", "/context", "/login", "/native", "/effort",
];

pub const COMMANDS: &[(&str, &str)] = &[
    (
        "/plan",
        "Plan mode: describe work and the planner writes tickets",
    ),
    (
        "/work",
        "Run tickets with the worker, one fresh context each",
    ),
    (
        "/review",
        "Review mode: ask the reviewer to check code; it reads, never edits",
    ),
    (
        "/solo",
        "Solo mode: talk to the soloist directly, like Claude Code",
    ),
    ("/planner", "Choose the planner's account and model"),
    ("/worker", "Choose the worker's account and model"),
    (
        "/reviewer",
        "Choose who reviews: same as planner, any model, or off",
    ),
    ("/soloist", "Choose the soloist's account and model"),
    (
        "/handholding",
        "How small and spelled-out the planner makes tickets: low, medium or high (for weak workers)",
    ),
    ("/model", "Choose the model for the current mode"),
    (
        "/harder",
        "Auto picked too small a model: move this conversation one model up",
    ),
    (
        "/routing",
        "Show the models auto chooses from and what it picked",
    ),
    (
        "/simple",
        "Simple mode: use Donut like Claude Code or Codex; it picks the model for you",
    ),
    (
        "/complex",
        "Complex mode: show every feature, with the planner, worker, reviewer and tickets",
    ),
    (
        "/resume",
        "Resume an earlier conversation in its mode, model and tickets session",
    ),
    (
        "/session",
        "Switch this window's tickets folder, .donut/<session>/, or start a new one",
    ),
    (
        "/next",
        "List the tickets still to do, with overview and pull requests (/next 3 for the next three)",
    ),
    (
        "/prev",
        "List the tickets already worked, latest first (/prev 3 for the last three)",
    ),
    (
        "/rename",
        "Name this session; its tickets folder becomes .donut/<name>/",
    ),
    ("/project", "Open another project in a new window"),
    (
        "/permissions",
        "Choose what the agent may do without asking (Shift+Tab cycles)",
    ),
    (
        "/auto",
        "Turn auto mode on or off: the provider's reviewer approves safe actions",
    ),
    (
        "/focus",
        "Focus view: show only each turn's final answer (/focus off restores)",
    ),
    (
        "/verbosity",
        "How much of the work Donut shows you mid-turn; models think and answer the same",
    ),
    (
        "/effort",
        "How hard the current mode's model thinks, in its provider's own levels",
    ),
    ("/usage", "Show usage for each configured connection"),
    ("/plugin", "Browse the provider's plugin commands"),
    ("/mcp", "Show MCP server status"),
    ("/context", "Show context usage for this conversation"),
    (
        "/clear",
        "Start completely fresh: new conversation, empty screen; tickets are kept",
    ),
    ("/new", "Same as /clear"),
    ("/copy", "Copy the last answer"),
    (
        "/chrome",
        "Claude models use the Claude in Chrome browser tools by default (/chrome off removes them)",
    ),
    (
        "/harness",
        "Run the current mode's model in Claude Code or Codex, whatever its provider (auto: its own)",
    ),
    (
        "/login",
        "Sign in again to the current mode's provider account",
    ),
    ("/native", "Open the provider's own client, then come back"),
    ("/help", "List commands and keys"),
    ("/exit", "Quit Donut"),
];

const CLAUDE_TOOL_TITLES: &[(&str, (&str, &str))] = &[
    ("Bash", ("Bash command", "Bash")),
    ("Edit", ("Edit file", "edits")),
    ("MultiEdit", ("Edit file", "edits")),
    ("Write", ("Write file", "file writes")),
    ("NotebookEdit", ("Edit notebook", "notebook edits")),
    ("WebFetch", ("Fetch", "fetches")),
    ("WebSearch", ("Web search", "web searches")),
];

/// Display only: every level gets the same model, thinking and answers; it changes what Donut shows mid-turn.
pub const VERBOSITY: &[(&str, (&str, &str))] = &[
    (
        "quiet",
        (
            "Quiet",
            "show only each turn's final answer; /work hides the worker's messages",
        ),
    ),
    (
        "normal",
        (
            "Normal",
            "show messages while the model works, keep only its final answer (default)",
        ),
    ),
    (
        "verbose",
        (
            "Verbose",
            "keep every message; also show every tool call and a line on each result",
        ),
    ),
    (
        "full",
        ("Full", "also show its thinking and each tool's output"),
    ),
];

pub fn verbosity_entry(level: &str) -> Option<(&'static str, &'static str)> {
    VERBOSITY
        .iter()
        .find(|(key, _)| *key == level)
        .map(|(_, entry)| *entry)
}

pub const IMAGE_SUFFIXES: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic", ".bmp", ".tiff",
];

// ---------------------------------------------------------------------------------------------------------
// Small value helpers

/// Python truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str(value)` for the JSON types.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn get_truthy<'a>(details: &'a Value, key: &str) -> Option<&'a Value> {
    details.get(key).filter(|v| truthy(v))
}

fn style(color: Color) -> Style {
    Style::default().fg(color)
}

fn bold(color: Color) -> Style {
    style(color).add_modifier(Modifier::BOLD)
}

// ---------------------------------------------------------------------------------------------------------
// Usage and quota

/// Which quota a profile draws from: one per Claude login folder, one for Codex, one per local model.
pub fn usage_key(profile: &Value) -> Vec<String> {
    let engine = profile.get("engine").and_then(Value::as_str).unwrap_or("");
    let text = |key: &str| {
        profile
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match engine {
        "claude" => vec![
            "claude".into(),
            text("claude_config_dir").unwrap_or_else(|| "default".into()),
        ],
        "codex" => vec!["codex".into()],
        _ => vec![
            "local".into(),
            text("model").unwrap_or_else(|| engine.to_string()),
        ],
    }
}

pub fn remaining(percent_used: f64) -> i64 {
    ((100.0 - percent_used).round_ties_even() as i64).clamp(0, 100)
}

/// Teal while plenty is left, orange under half, coral under a fifth.
pub fn left_color(left: i64) -> Color {
    if left > 50 {
        TEAL
    } else if left > 20 {
        ORANGE
    } else {
        CORAL
    }
}

/// A thin bar showing how much of the window is left.
pub fn usage_bar(percent_used: f64, width: usize) -> (Vec<Span<'static>>, Color) {
    let left = remaining(percent_used);
    let filled = ((left as f64 / 100.0 * width as f64).round_ties_even() as i64)
        .clamp(0, width as i64) as usize;
    let color = left_color(left);
    let spans = vec![
        Span::styled("━".repeat(filled), style(color)),
        Span::styled("─".repeat(width - filled), style(RULE)),
    ];
    (spans, color)
}

/// Seconds since the epoch -> "2h10m", "3d4h", "45m"; empty when unknown or past.
pub fn countdown(resets_at: Option<f64>, now: Option<f64>) -> String {
    let Some(at) = resets_at.filter(|v| *v != 0.0) else {
        return String::new();
    };
    let now = now.unwrap_or_else(crate::conversations::now_seconds);
    let seconds = (at - now).trunc() as i64;
    if seconds <= 0 {
        return String::new();
    }
    let (days, hours, minutes) = (seconds / 86400, seconds % 86400 / 3600, seconds % 3600 / 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{}m", minutes.max(1))
    }
}

fn line_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.as_ref().width())
        .sum()
}

/// Join entries on rows, starting a new row before an entry that would not fit, so no entry splits.
pub fn pack_entries(entries: &[Line<'static>], width: usize, gap: &str) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    let gap_width = gap.chars().count();
    for entry in entries {
        let size = line_width(entry);
        if used > 0 && width > 0 && used + gap_width + size > width {
            rows.push(Vec::new());
            used = 0;
        } else if used > 0 {
            rows.last_mut()
                .expect("row")
                .push(Span::raw(gap.to_string()));
            used += gap_width;
        }
        rows.last_mut()
            .expect("row")
            .extend(entry.spans.iter().cloned());
        used += size;
    }
    rows.into_iter().map(Line::from).collect()
}

// ---------------------------------------------------------------------------------------------------------
// Links and Markdown

static CODE_SPLIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```.*?(?:```|\n?\z)|`[^`\n]*`").expect("regex"));

/// Turn bare URLs into Markdown autolinks so they render as clickable links; code is left alone.
pub fn link_markdown(source: &str) -> String {
    let autolink = |text: &str| {
        let mut out = String::new();
        let mut last = 0;
        for found in URL.find_iter(text) {
            out.push_str(&text[last..found.start()]);
            let before: Vec<char> = text[..found.start()].chars().rev().take(2).collect();
            let linked = before.first() == Some(&'<')
                || (before.len() == 2 && before[0] == '(' && before[1] == ']');
            if linked {
                out.push_str(found.as_str());
            } else {
                out.push('<');
                out.push_str(found.as_str());
                out.push('>');
            }
            last = found.end();
        }
        out.push_str(&text[last..]);
        out
    };
    let mut out = String::new();
    let mut last = 0;
    for code in CODE_SPLIT.find_iter(source) {
        out.push_str(&autolink(&source[last..code.start()]));
        out.push_str(code.as_str());
        last = code.end();
    }
    out.push_str(&autolink(&source[last..]));
    out
}

/// Give each URL in a line an underline and a link, which Donut opens on click.
pub fn link_text(line: Line<'static>) -> RichLine {
    let plain: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let mut links = Vec::new();
    for found in URL.find_iter(&plain) {
        let start = plain[..found.start()].chars().count();
        links.push(Link {
            start,
            end: start + found.as_str().chars().count(),
            url: found.as_str().to_string(),
        });
    }
    if links.is_empty() {
        return RichLine { line, links };
    }
    let mut spans = Vec::new();
    let mut offset = 0;
    for span in &line.spans {
        let chars: Vec<char> = span.content.chars().collect();
        let mut piece = String::new();
        let mut piece_linked = false;
        for (index, ch) in chars.iter().enumerate() {
            let at = offset + index;
            let linked = links.iter().any(|link| link.start <= at && at < link.end);
            if !piece.is_empty() && linked != piece_linked {
                spans.push(Span::styled(
                    std::mem::take(&mut piece),
                    marked(span.style, piece_linked),
                ));
            }
            piece_linked = linked;
            piece.push(*ch);
        }
        if !piece.is_empty() {
            spans.push(Span::styled(piece, marked(span.style, piece_linked)));
        }
        offset += chars.len();
    }
    RichLine {
        line: Line { spans, ..line },
        links,
    }
}

fn marked(base: Style, linked: bool) -> Style {
    if linked {
        base.add_modifier(Modifier::UNDERLINED)
    } else {
        base
    }
}

/// What the Markdown renderer carries while it walks the events.
struct Renderer {
    lines: Vec<RichLine>,
    cur: Vec<Span<'static>>,
    cur_chars: usize,
    cur_links: Vec<Link>,
    open_link: Option<(String, usize)>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    items: Vec<usize>,
    pending_marker: Option<String>,
    quotes: usize,
    code: Option<(String, String)>,
    table: Option<Vec<Vec<Vec<Span<'static>>>>>,
    table_row: Vec<Vec<Span<'static>>>,
    in_head: bool,
    head_rows: usize,
}

const CODE_BG: Color = Color::Rgb(0x1a, 0x24, 0x2e);

impl Renderer {
    fn new() -> Self {
        Renderer {
            lines: Vec::new(),
            cur: Vec::new(),
            cur_chars: 0,
            cur_links: Vec::new(),
            open_link: None,
            styles: Vec::new(),
            lists: Vec::new(),
            items: Vec::new(),
            pending_marker: None,
            quotes: 0,
            code: None,
            table: None,
            table_row: Vec::new(),
            in_head: false,
            head_rows: 0,
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, s| acc.patch(*s))
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cur_chars += text.chars().count();
        self.cur.push(Span::styled(text.to_string(), self.style()));
    }

    fn emit(&mut self, spans: Vec<Span<'static>>, links: Vec<Link>) {
        let mut prefix: Vec<Span<'static>> = Vec::new();
        for _ in 0..self.quotes {
            prefix.push(Span::styled("▌ ", style(DIM)));
        }
        if let Some(inner) = self.items.len().checked_sub(1) {
            let outer: usize = self.items[..inner].iter().sum();
            let marker = self
                .pending_marker
                .take()
                .unwrap_or_else(|| " ".repeat(self.items[inner]));
            prefix.push(Span::raw(format!("{}{}", " ".repeat(outer), marker)));
        }
        let shift: usize = prefix.iter().map(|s| s.content.chars().count()).sum();
        let mut all = prefix;
        all.extend(spans);
        let links = links
            .into_iter()
            .map(|link| Link {
                start: link.start + shift,
                end: link.end + shift,
                url: link.url,
            })
            .collect();
        self.lines.push(RichLine {
            line: Line::from(all),
            links,
        });
    }

    fn flush(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        if let Some((url, start)) = self.open_link.as_mut() {
            if *start < self.cur_chars {
                self.cur_links.push(Link {
                    start: *start,
                    end: self.cur_chars,
                    url: url.clone(),
                });
            }
            *start = 0;
        }
        let spans = std::mem::take(&mut self.cur);
        let links = std::mem::take(&mut self.cur_links);
        self.cur_chars = 0;
        self.emit(spans, links);
    }

    fn blank(&mut self) {
        self.lines.push(RichLine::default());
    }

    fn gap(&mut self) {
        if self.pending_marker.is_none() && self.lines.last().is_some_and(|l| !l.plain().is_empty())
        {
            self.blank();
        }
    }

    fn start_code(&mut self, kind: &CodeBlockKind) {
        self.gap();
        let lang = match kind {
            CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
            CodeBlockKind::Indented => String::new(),
        };
        self.code = Some((lang, String::new()));
    }

    fn end_code(&mut self) {
        let Some((lang, body)) = self.code.take() else {
            return;
        };
        let mut highlighter = crate::highlight::Highlighter::new();
        highlighter.line(&format!("```{lang}"));
        let body = body.strip_suffix('\n').unwrap_or(&body);
        for text in body.split('\n') {
            let line = highlighter.code(text);
            let mut spans = vec![Span::styled("  ", Style::default().bg(CODE_BG))];
            spans.extend(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.bg(CODE_BG))),
            );
            self.emit(spans, Vec::new());
        }
    }

    fn end_table(&mut self) {
        let Some(rows) = self.table.take() else {
            return;
        };
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let cell_width = |cell: &Vec<Span<'static>>| {
            cell.iter()
                .map(|s| s.content.as_ref().width())
                .sum::<usize>()
        };
        let widths: Vec<usize> = (0..columns)
            .map(|c| {
                rows.iter()
                    .filter_map(|r| r.get(c))
                    .map(cell_width)
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        self.gap();
        for (index, row) in rows.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, width) in widths.iter().enumerate() {
                if c > 0 {
                    spans.push(Span::styled(" │ ", style(DIM)));
                }
                let cell = row.get(c).cloned().unwrap_or_default();
                let used = cell_width(&cell);
                spans.extend(cell.into_iter().map(|s| {
                    if index < self.head_rows {
                        Span::styled(s.content, s.style.add_modifier(Modifier::BOLD))
                    } else {
                        s
                    }
                }));
                if c + 1 < columns {
                    spans.push(Span::raw(" ".repeat(width - used)));
                }
            }
            self.emit(spans, Vec::new());
            if index + 1 == self.head_rows {
                let rule = widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                self.emit(vec![Span::styled(rule, style(DIM))], Vec::new());
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                if self.table.is_none() {
                    self.gap();
                }
            }
            Tag::Heading { level, .. } => {
                self.gap();
                let level = level as usize;
                let mut heading = bold(ORANGE);
                if level == 1 {
                    heading = heading.add_modifier(Modifier::UNDERLINED);
                } else if level > 2 {
                    heading = Style::default().add_modifier(Modifier::BOLD);
                }
                self.styles.push(heading);
            }
            Tag::BlockQuote(_) => {
                if self.items.is_empty() {
                    self.gap();
                }
                self.quotes += 1;
                self.styles.push(style(DIM));
            }
            Tag::CodeBlock(kind) => self.start_code(&kind),
            Tag::List(start) => {
                self.flush();
                if self.items.is_empty() {
                    self.gap();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                self.items.push(marker.chars().count());
                self.pending_marker = Some(marker);
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(Vec::new());
                self.head_rows = 0;
            }
            Tag::TableHead => {
                self.in_head = true;
                self.table_row = Vec::new();
            }
            Tag::TableRow => self.table_row = Vec::new(),
            Tag::TableCell => {
                self.cur.clear();
                self.cur_chars = 0;
            }
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.open_link = Some((dest_url.to_string(), self.cur_chars));
                self.styles
                    .push(Style::default().fg(TEAL).add_modifier(Modifier::UNDERLINED));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quotes = self.quotes.saturating_sub(1);
                self.styles.pop();
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
                self.pending_marker = None;
            }
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => {
                let row = std::mem::take(&mut self.table_row);
                if let Some(table) = self.table.as_mut() {
                    table.push(row);
                }
                self.in_head = false;
                self.head_rows = 1;
            }
            TagEnd::TableRow => {
                let row = std::mem::take(&mut self.table_row);
                if let Some(table) = self.table.as_mut() {
                    table.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.cur);
                self.cur_chars = 0;
                self.cur_links.clear();
                self.table_row.push(cell);
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, start)) = self.open_link.take() {
                    if start < self.cur_chars {
                        self.cur_links.push(Link {
                            start,
                            end: self.cur_chars,
                            url,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn event(&mut self, event: Event) {
        if let Some((_, body)) = self.code.as_mut() {
            match event {
                Event::Text(text) => body.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.end_code(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                self.cur_chars += text.chars().count();
                let code = self
                    .style()
                    .patch(Style::default().fg(GRAY).bg(Color::Rgb(0x2a, 0x35, 0x40)));
                self.cur.push(Span::styled(text.to_string(), code));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush(),
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::Rule => {
                self.gap();
                self.emit(vec![Span::styled("─".repeat(40), style(DIM))], Vec::new());
            }
            Event::TaskListMarker(checked) => self.text(if checked { "☑ " } else { "☐ " }),
            _ => {}
        }
    }
}

/// Render Markdown into unwrapped lines, with the links in them; the transcript wraps at draw time.
pub fn markdown(source: &str) -> Vec<RichLine> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer::new();
    for event in Parser::new_ext(&link_markdown(source), options) {
        renderer.event(event);
    }
    renderer.flush();
    while renderer.lines.last().is_some_and(|l| l.plain().is_empty()) {
        renderer.lines.pop();
    }
    renderer.lines
}

// ---------------------------------------------------------------------------------------------------------
// Commands and names

/// What follows --work on the command line for a /work choice such as "next all parallel 3 checkpoint".
pub fn work_arguments(choice: &str) -> Result<Vec<String>, String> {
    let usage = "Use /work next COUNT [parallel N] [checkpoint] [no-pr] (COUNT is a positive integer or all).";
    let mut words: Vec<String> = choice.split_whitespace().map(str::to_string).collect();
    let no_pr = words.iter().any(|w| w == "no-pr");
    words.retain(|w| w != "no-pr");
    let mut parallel: Option<String> = None;
    if words.len() > 1 && words[..words.len() - 1].iter().any(|w| w == "parallel") {
        let at = words.iter().position(|w| w == "parallel").unwrap_or(0);
        parallel = words.get(at + 1).cloned();
        words.drain(at..(at + 2).min(words.len()));
    }
    let checkpoint = words.last().is_some_and(|w| w == "checkpoint");
    if checkpoint {
        words.pop();
    }
    let positive = |text: &str| {
        !text.is_empty()
            && text.chars().all(|c| c.is_ascii_digit())
            && text.chars().any(|c| c != '0')
    };
    if words.len() != 2
        || words[0] != "next"
        || !(words[1] == "all" || positive(&words[1]))
        || !parallel.as_deref().is_none_or(positive)
    {
        return Err(usage.to_string());
    }
    if let Some(count) = parallel {
        words.push("--parallel".into());
        words.push(count);
    }
    if checkpoint {
        words.push("--checkpoint".into());
    }
    if no_pr {
        words.push("--no-pr".into());
    }
    Ok(words)
}

pub fn is_instant(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    words.first().is_some_and(|w| INSTANT_COMMANDS.contains(w))
        || words.starts_with(&["/ponytail", "band"])
}

pub fn model_name(profile: &Value, alias: Option<&str>) -> String {
    if alias == Some(crate::routing::AUTO) {
        return "auto".into();
    }
    let text = |value: Option<&str>| value.filter(|s| !s.is_empty()).map(str::to_string);
    text(profile.get("model").and_then(Value::as_str))
        .or_else(|| text(alias))
        .unwrap_or_else(|| "provider default".into())
}

// ---------------------------------------------------------------------------------------------------------
// Approvals

/// (panel title, short phrase for "don't ask again for ...") for a Claude tool name or a Codex request method.
pub fn approval_title(name: &str, _details: &Value) -> (String, String) {
    if name.contains("commandExecution") {
        return ("Codex command".into(), "commands".into());
    }
    if name.contains("fileChange") {
        return ("Codex file change".into(), "file changes".into());
    }
    if let Some((_, (title, short))) = CLAUDE_TOOL_TITLES.iter().find(|(key, _)| *key == name) {
        return (title.to_string(), short.to_string());
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
        let short = if tool.is_empty() { server } else { tool };
        return (format!("{server} · {tool}"), short.to_string());
    }
    (name.to_string(), name.to_string())
}

pub fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub fn command_text(details: &Value) -> String {
    match details.get("command") {
        Some(Value::Array(parts)) => parts.iter().map(py_str).collect::<Vec<_>>().join(" "),
        Some(Value::Null) | None => String::new(),
        Some(other) => py_str(other),
    }
}

/// One short line naming what a tool call does: its command, file, URL or query.
pub fn tool_summary(_name: Option<&str>, details: &Value) -> String {
    let empty = Value::Object(Default::default());
    let details = if details.is_object() { details } else { &empty };
    let mut summary = approval_summary(details);
    if summary.is_empty() {
        if let Some(changes) = details
            .get("changes")
            .and_then(Value::as_array)
            .filter(|c| !c.is_empty())
        {
            summary = changes[0].get("path").map(py_str).unwrap_or_default();
            if changes.len() > 1 {
                summary += &format!(" (+{} more)", changes.len() - 1);
            }
        }
    }
    if summary.is_empty() {
        if let Some(tool) = get_truthy(details, "tool") {
            let server = details.get("server").map(py_str).unwrap_or_default();
            summary = format!("{server} · {}", py_str(tool));
        }
    }
    summary
}

/// One short line for the transcript after a decision.
pub fn approval_summary(details: &Value) -> String {
    if details.get("command").is_some() {
        return clip(command_text(details).lines().next().unwrap_or(""), 100);
    }
    for key in [
        "file_path",
        "notebook_path",
        "path",
        "url",
        "query",
        "grantRoot",
    ] {
        if let Some(value) = get_truthy(details, key) {
            return clip(&py_str(value), 100);
        }
    }
    String::new()
}

/// A rich Text with embedded newlines, kept as rows.
struct Body {
    rows: Vec<Vec<Span<'static>>>,
}

impl Body {
    fn new() -> Self {
        Body {
            rows: vec![Vec::new()],
        }
    }

    fn append(&mut self, text: &str, style: Style) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.rows.push(Vec::new());
            }
            if !part.is_empty() {
                self.rows
                    .last_mut()
                    .expect("row")
                    .push(Span::styled(part.to_string(), style));
            }
        }
    }

    fn plain(&self) -> String {
        self.rows
            .iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn lines(&mut self, text: &str, prefix: &str, style: Style, limit: usize) {
        let mut rows: Vec<&str> = text.lines().collect();
        if rows.is_empty() {
            rows.push("");
        }
        for row in rows.iter().take(limit) {
            self.append(&format!("{prefix}{}\n", clip(row, 160)), style);
        }
        if rows.len() > limit {
            self.append(
                &format!("  … {} more lines\n", rows.len() - limit),
                self::style(DIM),
            );
        }
    }
}

/// What the agent wants to do, shaped like Claude Code's permission prompt rather than raw JSON.
pub fn approval_details(details: &Value) -> Vec<Line<'static>> {
    let mut body = Body::new();
    if details.get("command").is_some() {
        body.append("$ ", style(DIM));
        body.lines(&command_text(details), "", bold(ORANGE), 6);
        if let Some(description) = get_truthy(details, "description") {
            body.append(
                &format!("{}\n", clip(&py_str(description), 160)),
                style(DIM),
            );
        }
        if let Some(cwd) = get_truthy(details, "cwd") {
            body.append(&format!("in {}\n", clip(&py_str(cwd), 160)), style(DIM));
        }
    }
    let path = ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| get_truthy(details, key))
        .map(py_str);
    if let Some(path) = &path {
        body.append(&format!("{}\n", clip(path, 160)), bold(ORANGE));
    }
    let edits = details.get("edits").and_then(Value::as_array);
    let text_of = |value: &Value, key: &str| value.get(key).map(py_str).unwrap_or_default();
    if details.get("old_string").is_some() || details.get("new_string").is_some() {
        body.lines(&text_of(details, "old_string"), "- ", style(CORAL), 5);
        body.lines(&text_of(details, "new_string"), "+ ", style(TEAL), 5);
    } else if let Some(edits) = edits.filter(|e| !e.is_empty()) {
        body.append(&format!("{} edits\n", edits.len()), style(GRAY));
        for edit in edits.iter().take(2).filter(|e| e.is_object()) {
            body.lines(&text_of(edit, "old_string"), "- ", style(CORAL), 3);
            body.lines(&text_of(edit, "new_string"), "+ ", style(TEAL), 3);
        }
    } else if details.get("content").is_some() && path.is_some() {
        body.lines(&text_of(details, "content"), "  ", style(GRAY), 8);
    }
    for (key, label) in [
        ("url", ""),
        ("query", ""),
        ("prompt", ""),
        ("grantRoot", "write access to "),
        ("reason", ""),
    ] {
        if let Some(value) = get_truthy(details, key) {
            if key == "prompt" && details.get("url").is_none() {
                continue;
            }
            let color = if key == "reason" { DIM } else { GRAY };
            body.append(
                &format!("{label}{}\n", clip(&py_str(value), 160)),
                style(color),
            );
        }
    }
    if body.plain().trim().is_empty() {
        let mut shown = 0;
        if let Some(map) = details.as_object() {
            for (key, value) in map {
                if shown == 6 {
                    break;
                }
                let simple = matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_));
                if simple && !py_str(value).trim().is_empty() {
                    body.append(&format!("{key}: "), style(DIM));
                    body.append(&format!("{}\n", clip(&py_str(value), 160)), style(GRAY));
                    shown += 1;
                }
            }
        }
        if shown == 0 {
            body.append(
                &format!("{}\n", clip(&details.to_string(), 300)),
                style(GRAY),
            );
        }
    }
    body.append("\nDo you want to proceed?", bold(GRAY));
    body.rows.into_iter().map(Line::from).collect()
}

// ---------------------------------------------------------------------------------------------------------
// Shell completion and images

/// Like `os.path.expanduser`: a leading `~` is the home directory.
fn expanduser(path: &str) -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Names that could finish the last word of a !command: programs for the first word, else paths.
pub fn shell_candidates(line: &str, project: &Path) -> (String, Vec<String>) {
    let rest = line.trim_start_matches('!').trim_start();
    let (head, word) = match rest.rfind(' ') {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => ("", rest),
    };
    if head.trim().is_empty() && !word.contains('/') {
        let mut names = std::collections::BTreeSet::new();
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path).filter(|d| d.is_dir()) {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if is_executable(&entry.path()) {
                        names.insert(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
        return (
            word.to_string(),
            names.into_iter().filter(|n| n.starts_with(word)).collect(),
        );
    }
    let (folder, prefix) = match word.rfind('/') {
        Some(at) => (&word[..at], &word[at + 1..]),
        None => ("", word),
    };
    let dir_part = if !folder.is_empty() || word.starts_with('/') {
        format!("{folder}/")
    } else {
        String::new()
    };
    let listing = project.join(expanduser(if dir_part.is_empty() {
        "."
    } else {
        &dir_part
    }));
    let Ok(entries) = std::fs::read_dir(listing) else {
        return (word.to_string(), Vec::new());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        if name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.')) {
            let full = format!("{dir_part}{name}");
            let slash = if project.join(expanduser(&full)).is_dir() {
                "/"
            } else {
                ""
            };
            out.push(format!("{full}{slash}"));
        }
    }
    (word.to_string(), out)
}

fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (
            bytes[index],
            hex.and_then(|h| u8::from_str_radix(h, 16).ok()),
        ) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Windows: a backslash separates folders, so only quotes group a path.
fn split_quotes_only(text: &str) -> Vec<String> {
    let (mut parts, mut current, mut quote) = (Vec::new(), String::new(), None);
    for ch in text.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn has_image_suffix(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        let suffix = format!(".{}", ext.to_string_lossy().to_lowercase());
        IMAGE_SUFFIXES.contains(&suffix.as_str())
    })
}

/// Absolute paths if the whole paste is existing image files (escaped, quoted or file:// form), else empty.
pub fn dropped_images(text: &str, project: &Path) -> Vec<String> {
    let text = text.trim();
    let mut parts: Vec<String> = if let Some(rest) = text.strip_prefix("file://") {
        vec![unquote(rest)]
    } else if cfg!(windows) {
        split_quotes_only(text)
    } else {
        shlex::split(text).unwrap_or_else(|| vec![text.to_string()])
    };
    let whole = text.trim_matches(|c| c == '"' || c == '\'');
    if !whole.is_empty() && project.join(expanduser(whole)).is_file() {
        parts = vec![whole.to_string()];
    }
    let mut paths = Vec::new();
    for part in parts {
        let path = project.join(expanduser(&part));
        if !has_image_suffix(&path) || !path.is_file() {
            return Vec::new();
        }
        paths.push(path.to_string_lossy().into_owned());
    }
    paths
}

static DATA_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*data:image/[\w.+-]+;base64,").expect("regex"));
static BASE64_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9+/]+={0,2}$").expect("regex"));

/// (extension, bytes) if the paste is base64 image data (optional data: prefix, wrapped lines), else None.
pub fn pasted_image(text: &str) -> Option<(String, Vec<u8>)> {
    let text = DATA_PREFIX.replace(text, "");
    let text: String = text.split_whitespace().collect();
    if text.chars().count() <= 100 || !BASE64_TEXT.is_match(&text) {
        return None;
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    let magic: [(&[u8], &str); 3] = [
        (b"\x89PNG\r\n\x1a\n", "png"),
        (b"\xff\xd8\xff", "jpg"),
        (b"GIF8", "gif"),
    ];
    for (prefix, ext) in magic {
        if data.starts_with(prefix) {
            return Some((ext.to_string(), data));
        }
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some(("webp".to_string(), data));
    }
    None
}

/// Write the bytes under the temp folder and return the absolute path.
pub fn save_pasted_image(ext: &str, data: &[u8]) -> std::io::Result<String> {
    let folder = std::env::temp_dir().join("donut-images");
    std::fs::create_dir_all(&folder)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut n = 1;
    let path = loop {
        let path = folder.join(format!("paste-{stamp}-{n}.{ext}"));
        if !path.exists() {
            break path;
        }
        n += 1;
    };
    std::fs::write(&path, data)?;
    Ok(dunce::canonicalize(&path)?.to_string_lossy().into_owned())
}

/// Python's `shlex.quote`.
/// What the draft and transcript show for the Nth pasted image (1-based).
pub fn image_label(n: usize) -> String {
    format!("[screenshot{n}]")
}

/// The text to send: each `[screenshotN]` label becomes image N's quoted path. Unknown labels stay as typed.
pub fn expand_images(text: &str, images: &[String]) -> String {
    images
        .iter()
        .enumerate()
        .rev()
        .fold(text.to_string(), |text, (at, path)| {
            text.replace(&image_label(at + 1), &quote_path(path))
        })
}

/// The reverse of expand_images, for showing a sent message: each known quoted path becomes its label.
pub fn label_images(text: &str, images: &[String]) -> String {
    images
        .iter()
        .enumerate()
        .fold(text.to_string(), |text, (at, path)| {
            text.replace(&quote_path(path), &image_label(at + 1))
        })
}

pub fn quote_path(path: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !path.is_empty() && path.chars().all(safe) {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::plain;
    use serde_json::json;

    fn joined(lines: &[RichLine]) -> String {
        lines
            .iter()
            .map(RichLine::plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_countdown_and_remaining() {
        assert_eq!(
            countdown(Some(1000.0 + 7200.0 + 600.0), Some(1000.0)),
            "2h10m"
        );
        assert_eq!(
            countdown(Some(1000.0 + 3.0 * 86400.0 + 4.0 * 3600.0), Some(1000.0)),
            "3d4h"
        );
        assert_eq!(countdown(Some(1030.0), Some(1000.0)), "1m");
        assert_eq!(countdown(Some(500.0), Some(1000.0)), "");
        assert_eq!(countdown(None, None), "");
        assert_eq!(
            (remaining(18.4), remaining(120.0), remaining(-5.0)),
            (82, 0, 100)
        );
    }

    #[test]
    fn test_lines_break_only_between_entries() {
        let entries: Vec<Line<'static>> = [
            "PLAN",
            "planner claude-opus-5-5",
            "reviewer claude-opus-5-5 (= planner)",
            "ask",
        ]
        .iter()
        .map(|t| Line::from(t.to_string()))
        .collect();
        let rows: Vec<String> = pack_entries(&entries, 40, "  ").iter().map(plain).collect();
        assert_eq!(
            rows,
            [
                "PLAN  planner claude-opus-5-5",
                "reviewer claude-opus-5-5 (= planner)",
                "ask"
            ]
        );
        assert_eq!(pack_entries(&entries, 200, "  ").len(), 1);
    }

    #[test]
    fn test_work_choices_become_command_line_arguments() {
        assert_eq!(work_arguments("next 1").unwrap(), ["next", "1"]);
        assert_eq!(
            work_arguments("next all parallel 3 checkpoint no-pr").unwrap(),
            ["next", "all", "--parallel", "3", "--checkpoint", "--no-pr"]
        );
        for bad in [
            "next all parallel",
            "next all parallel 0",
            "next all parallel x",
            "all",
        ] {
            assert!(work_arguments(bad).is_err(), "{bad}");
        }
    }

    struct Fixture {
        _guard: std::sync::MutexGuard<'static, ()>,
        temp: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let guard = crate::conversations::env_lock();
        let temp = tempfile::tempdir().unwrap();
        Fixture {
            _guard: guard,
            temp,
        }
    }

    fn shot(temp: &Path) -> String {
        let dir = dunce::canonicalize(temp).unwrap();
        let shot = dir.join("shot one.png");
        std::fs::write(&shot, b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        shot.to_string_lossy().into_owned()
    }

    #[test]
    fn test_dropped_forms() {
        let f = fixture();
        let shot = shot(f.temp.path());
        let project = dunce::canonicalize(f.temp.path()).unwrap();
        let mut forms = vec![
            format!("'{shot}'"),
            format!("\"{shot}\""),
            format!("file://{}", shot.replace(' ', "%20")),
            shot.clone(),
            "shot one.png".to_string(),
        ];
        if !cfg!(windows) {
            forms.push(shot.replace(' ', "\\ "));
            forms.push("shot\\ one.png".to_string());
        }
        for text in forms {
            assert_eq!(
                dropped_images(&text, &project),
                vec![shot.clone()],
                "{text}"
            );
        }
        let both = format!("\"{shot}\" \"{shot}\"");
        assert_eq!(dropped_images(&both, &project), [shot.clone(), shot]);
    }

    #[test]
    fn test_dropped_rejects_non_images() {
        let f = fixture();
        let shot = shot(f.temp.path());
        let temp = f.temp.path();
        for text in [
            temp.join("notes.txt").to_string_lossy().into_owned(),
            temp.join("a.png").to_string_lossy().into_owned(),
            format!("look at {}", quote_path(&shot)),
            "plain text".to_string(),
        ] {
            assert!(dropped_images(&text, temp).is_empty(), "{text}");
        }
    }

    fn png() -> String {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'0', 200));
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn test_pasted_image_decodes_wrapped_and_prefixed() {
        let png = png();
        let wrapped = png
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        for text in [
            png.clone(),
            wrapped.clone(),
            format!("data:image/png;base64,{wrapped}"),
        ] {
            let (ext, data) = pasted_image(&text).unwrap();
            assert_eq!((ext.as_str(), &data[..4]), ("png", &b"\x89PNG"[..]));
        }
    }

    #[test]
    fn test_image_labels_expand_to_paths_and_back() {
        let images = vec!["/tmp/a.png".to_string(), "/tmp/shot two.png".to_string()];
        let sent = expand_images(
            "see [screenshot2] and [screenshot1], not [screenshot3]",
            &images,
        );
        assert_eq!(
            sent,
            "see '/tmp/shot two.png' and /tmp/a.png, not [screenshot3]"
        );
        assert_eq!(
            label_images(&sent, &images),
            "see [screenshot2] and [screenshot1], not [screenshot3]"
        );
        let many: Vec<String> = (1..=12).map(|n| format!("/tmp/{n}.png")).collect();
        assert_eq!(
            expand_images("[screenshot12] [screenshot1]", &many),
            "/tmp/12.png /tmp/1.png"
        );
    }

    #[test]
    fn test_pasted_image_rejects_other_text() {
        let hello = base64::engine::general_purpose::STANDARD.encode("hello world ".repeat(30));
        for text in [
            "plain text".to_string(),
            "A".repeat(300),
            "iVBORw0KGgo".to_string(),
            hello,
        ] {
            assert!(pasted_image(&text).is_none(), "{text}");
        }
    }

    #[test]
    fn test_pasted_image_is_saved_under_the_temp_folder() {
        let f = fixture();
        let old = std::env::var_os("TMPDIR");
        unsafe { std::env::set_var("TMPDIR", f.temp.path()) };
        let saved = save_pasted_image("png", b"\x89PNG\r\n\x1a\nxx");
        match old {
            Some(v) => unsafe { std::env::set_var("TMPDIR", v) },
            None => unsafe { std::env::remove_var("TMPDIR") },
        }
        let path = saved.unwrap();
        assert!(path.ends_with(".png") && path.contains("donut-images"));
        assert!(
            std::fs::read(&path)
                .unwrap()
                .starts_with(b"\x89PNG\r\n\x1a\n")
        );
    }

    #[test]
    fn test_quote_path() {
        assert_eq!(quote_path("/tmp/a b.png"), "'/tmp/a b.png'");
        assert_eq!(quote_path("/tmp/a.png"), "/tmp/a.png");
        assert_eq!(quote_path("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn test_links_are_found_in_markdown_source() {
        assert_eq!(
            link_markdown(
                "See https://github.com/x/y/pull/42. Also [PR](https://a.b/c) and <https://d.e>"
            ),
            "See <https://github.com/x/y/pull/42>. Also [PR](https://a.b/c) and <https://d.e>"
        );
        let code = "Run `curl https://a.b/c` then\n```\nhttps://in.code/block\n```\n";
        assert_eq!(link_markdown(code), code);
    }

    #[test]
    fn test_link_text_marks_urls() {
        let rich = link_text(Line::from("PR: https://github.com/example/project/pull/42"));
        assert_eq!(
            rich.link_at(12),
            Some("https://github.com/example/project/pull/42")
        );
        assert_eq!(rich.link_at(2), None);
        assert!(
            rich.line
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
        );
        assert_eq!(
            rich.plain(),
            "PR: https://github.com/example/project/pull/42"
        );
    }

    #[test]
    fn markdown_heading_paragraph_and_inline() {
        let lines = markdown("# Title\n\nSome **bold** and *it* and `code` here.");
        assert_eq!(joined(&lines), "Title\n\nSome bold and it and code here.");
        let bold_span = lines[2]
            .line
            .spans
            .iter()
            .find(|s| s.content == "bold")
            .unwrap();
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        let code = lines[2]
            .line
            .spans
            .iter()
            .find(|s| s.content == "code")
            .unwrap();
        assert!(code.style.bg.is_some());
    }

    #[test]
    fn markdown_lists_nest_and_number() {
        let text = joined(&markdown("- one\n- two\n  - inner\n\n1. a\n2. b"));
        assert_eq!(text, "• one\n• two\n  • inner\n\n1. a\n2. b");
    }

    #[test]
    fn markdown_code_block_is_highlighted_and_unfenced() {
        let lines = markdown("Before\n\n```sql\nSELECT 1\n\nFROM t\n```\n\nAfter");
        let text = joined(&lines);
        assert!(text.contains("  SELECT 1"), "{text}");
        assert!(text.contains("  FROM t"));
        assert!(!text.contains("```"));
        let code = lines.iter().find(|l| l.plain().contains("SELECT")).unwrap();
        assert!(code.line.spans.len() > 2, "tokens are styled separately");
    }

    #[test]
    fn markdown_table_quote_and_rule() {
        let text = joined(&markdown(
            "| a | bb |\n|---|---|\n| 1 | 2 |\n\n> quoted\n\n---\n",
        ));
        assert!(text.contains("a │ bb"), "{text}");
        assert!(text.contains("─┼─"));
        assert!(text.contains("1 │ 2"));
        assert!(text.contains("▌ quoted"));
        assert!(text.contains("────"));
    }

    #[test]
    fn markdown_links_carry_their_targets() {
        let lines =
            markdown("Merged https://github.com/x/y/pull/2 today. See [PR](https://a.b/c).");
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        let plain = line.plain();
        assert_eq!(plain, "Merged https://github.com/x/y/pull/2 today. See PR.");
        assert_eq!(line.link_at(14), Some("https://github.com/x/y/pull/2"));
        let at = plain.find("PR.").unwrap();
        assert_eq!(line.link_at(at), Some("https://a.b/c"));
        assert_eq!(line.link_at(2), None);
    }

    #[test]
    fn approvals_read_like_claude_codes_prompt() {
        let rows = approval_details(&json!({"command": "ls -la", "description": "List files"}));
        let text: Vec<String> = rows.iter().map(plain).collect();
        assert_eq!(
            text,
            ["$ ls -la", "List files", "", "Do you want to proceed?"]
        );
        assert!(!text.join("").contains('{'));
        assert_eq!(
            approval_title("mcp__github__create_issue", &json!({})),
            ("github · create_issue".into(), "create_issue".into())
        );
        assert_eq!(approval_title("Bash", &json!({})).0, "Bash command");
        assert_eq!(
            approval_summary(&json!({"command": ["git", "status"]})),
            "git status"
        );
        assert_eq!(
            tool_summary(None, &json!({"changes": [{"path": "a"}, {"path": "b"}]})),
            "a (+1 more)"
        );
        assert_eq!(clip("abcdef", 4), "abc…");
    }

    #[test]
    fn small_helpers_follow_python() {
        assert!(
            is_instant("/help now") && is_instant("/ponytail band off") && !is_instant("/plan")
        );
        assert_eq!(model_name(&json!({}), Some("auto")), "auto");
        assert_eq!(model_name(&json!({}), None), "provider default");
        assert_eq!(
            usage_key(&json!({"engine": "claude"})),
            ["claude", "default"]
        );
        assert_eq!(
            usage_key(&json!({"engine": "ollama", "model": "q"})),
            ["local", "q"]
        );
        assert!(RUNNER_SPEND.is_match("Spend: $0.12 (opus, plan)"));
        assert!(RUNNER_PROGRESS.is_match("[Working · 15s elapsed · waiting for agent output]"));
        assert_eq!(
            &RUNNER_TICKET.captures("[1/1] 047_x.md — fresh").unwrap()["ticket"],
            "047_x.md"
        );
        let (spans, _) = usage_bar(18.0, 6);
        assert_eq!(spans[0].content.chars().count(), 5);
    }

    #[test]
    fn shell_candidates_complete_paths() {
        let f = fixture();
        std::fs::create_dir(f.temp.path().join("src")).unwrap();
        std::fs::write(f.temp.path().join("setup.py"), b"").unwrap();
        std::fs::write(f.temp.path().join(".hidden"), b"").unwrap();
        let (word, found) = shell_candidates("!cat s", f.temp.path());
        assert_eq!(
            (word.as_str(), found),
            ("s", vec!["setup.py".to_string(), "src/".to_string()])
        );
        assert!(
            shell_candidates("!cat .", f.temp.path())
                .1
                .contains(&".hidden".to_string())
        );
    }
}
