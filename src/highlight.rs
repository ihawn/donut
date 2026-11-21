//! line-by-line styling for agent output, shared by the CLI stream and the
//! interactive terminal.
//!
//! Each call styles one complete line and returns a ratatui `Line`. State carries across lines so fenced code
//! blocks, thinking sections and error bodies keep their look until they end. Code blocks are lexed with
//! syntect in place of pygments, and mapped onto the same palette.

use std::sync::LazyLock;

use fancy_regex::Regex as FancyRegex;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

pub const ORANGE: Color = Color::Rgb(255, 168, 24);
pub const CORAL: Color = Color::Rgb(255, 94, 78);
pub const TEAL: Color = Color::Rgb(23, 184, 190);
pub const GRAY: Color = Color::Rgb(222, 224, 228);
pub const DIM: Color = Color::Rgb(0x8a, 0x96, 0xa3);
/// The unfilled part of a bar.
pub const RULE: Color = Color::Rgb(0x3b, 0x46, 0x52);

/// A plain foreground color.
pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

pub fn bold(color: Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
}

pub fn italic(color: Color) -> Style {
    fg(color).add_modifier(Modifier::ITALIC)
}

pub fn underline(color: Color) -> Style {
    fg(color).add_modifier(Modifier::UNDERLINED)
}

/// The text of a line, without its styles (Rich's `Text.plain`).
pub fn plain(line: &Line) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn span(text: &str, style: Style) -> Span<'static> {
    Span::styled(text.to_string(), style)
}

/// A line in one style.
fn whole(text: &str, style: Style) -> Line<'static> {
    Line::from(vec![span(text, style)])
}

static FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(`{3,}|~{3,})\s*([\w+#.-]*)").unwrap());
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\[[^\]\n]*\]\s*$").unwrap());
static ERROR_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\[Error\b|Error:|ERROR\b|Traceback \(|FAILED\b)").unwrap()
});
static HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:#{1,6}\s+.*|[A-Z][A-Z0-9 /·-]{5,}[A-Z0-9]\b.*|Run summary.*)$").unwrap()
});
static KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(\s*)(Project|Model|Planner|Worker|Reviewer|Review|Recovery|Repair|VERDICT|RECOVERY|Run logs|Logs and results|Logs|Verifying|Queue|Checkpoints|Checkpoint):(\s)",
    )
    .unwrap()
});
static TICKET_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\s*)(\[(?:review )?\d+/(?:\d+|all)\])(\s+)(\S+)(.*)$").unwrap()
});
static BULLET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*)([-*•]|\d+[.)])(\s+)").unwrap());
static TABLE_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\|?[\s:|-]+\|?\s*$").unwrap());

const GOOD: &str = r"REQUEUED|[Rr]equeued|APPROVED|[Aa]pproved|Done|PASS(?:ED)?|[Pp]assed|OK|[Ss]uccess(?:ful(?:ly)?)?|[Cc]omplete(?:d)?|[Aa]llowed|✓";
const BAD: &str = r"Blocked|BLOCKED|FAIL(?:ED|URE)?|[Ff]ailed|[Ff]ailure|Error|[Dd]enied|[Ss]topping|[Mm]issing|✗";
const WARN: &str = r"NEEDS HUMAN|CHANGES REQUESTED|[Cc]hanges requested|WARN(?:ING)?|[Ww]arning|[Ss]kipped|Human check|[Qq]ueued|TODO";
const PATH: &str = r"(?:~|\.{1,2})?/?(?:[\w.-]+/)+[\w.-]+|\b[\w-]+(?:\.[\w-]+)*\.(?:md|py|cs|js|ts|tsx|jsx|json|ya?ml|toml|txt|sh|go|rs|java|kt|rb|php|c|h|cpp|hpp|unity|prefab|asset)\b";

/// The prose tokens. It needs a look-behind and a look-ahead on numbers, which the `regex` crate lacks.
static TOKENS: LazyLock<FancyRegex> = LazyLock::new(|| {
    FancyRegex::new(&format!(
        r"(?P<code>`[^`\n]+`)|(?P<bracket>\[[^\[\]\n]+\])|(?P<paren>\([^()\n]*\)|\{{[^{{}}\n]*\}}|[(){{}}])|(?P<path>{PATH})|(?P<good>\b(?:{GOOD})\b|✓)|(?P<bad>\b(?:{BAD})\b|✗)|(?P<warn>\b(?:{WARN})\b)|(?P<number>(?<![\w.])-?\d+(?:[.,:/]\d+)*(?:%|ms|s|m|h|x)?(?![\w]))"
    ))
    .unwrap()
});
const TOKEN_GROUPS: [&str; 8] = [
    "code", "bracket", "paren", "path", "good", "bad", "warn", "number",
];

fn token_style(group: &str) -> Style {
    match group {
        "code" => fg(ORANGE),
        "bracket" | "paren" => fg(TEAL),
        "path" => underline(TEAL),
        "good" => bold(TEAL),
        "bad" => bold(CORAL),
        "warn" => bold(ORANGE),
        _ => fg(CORAL), // number
    }
}

/// Fallback for unlabeled code blocks: a few language-neutral tokens.
static CODE_TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?P<string>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')"#,
        r"|(?P<comment>//.*|#.*)",
        r"|(?P<number>\b\d+(?:\.\d+)?\b)",
        r"|(?P<keyword>\b(?:class|def|return|if|elif|else|for|while|import|from|const|let|var|function|public|",
        r"private|protected|static|void|new|true|false|null|None|True|False|await|async|using|namespace|struct)\b)"
    ))
    .unwrap()
});

fn code_style(group: &str) -> Style {
    match group {
        "string" => fg(TEAL),
        "comment" => italic(DIM),
        "number" => fg(ORANGE),
        _ => fg(CORAL), // keyword
    }
}

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Scope prefixes in the order they are tried, mapped onto the Donut palette like the pygments token types were.
static SCOPE_STYLES: LazyLock<Vec<(Scope, Style)>> = LazyLock::new(|| {
    [
        ("comment", italic(DIM)),
        ("entity.name.function.decorator", fg(ORANGE)),
        ("meta.annotation", fg(ORANGE)),
        ("variable.annotation", fg(ORANGE)),
        ("storage.type.annotation", fg(ORANGE)),
        ("entity.name.function", bold(ORANGE)),
        ("entity.name.class", bold(ORANGE)),
        ("entity.name.type", bold(ORANGE)),
        ("entity.name.struct", bold(ORANGE)),
        ("entity.name.enum", bold(ORANGE)),
        ("entity.name.interface", bold(ORANGE)),
        ("entity.name.tag", fg(CORAL)),
        ("entity.other.attribute-name", fg(ORANGE)),
        ("markup.inserted", fg(TEAL)),
        ("markup.deleted", fg(CORAL)),
        ("keyword.operator", fg(GRAY)),
        ("keyword", fg(CORAL)),
        ("storage", fg(CORAL)),
        ("constant.language", fg(CORAL)),
        ("support.function", fg(CORAL)),
        ("support.type", fg(CORAL)),
        ("support.constant", fg(CORAL)),
        ("string", fg(TEAL)),
        ("constant.numeric", fg(ORANGE)),
    ]
    .into_iter()
    .filter_map(|(scope, style)| Scope::new(scope).ok().map(|scope| (scope, style)))
    .collect()
});

/// Map the scopes of a token (innermost first) to the Donut palette.
fn scope_style(stack: &ScopeStack) -> Style {
    for scope in stack.as_slice().iter().rev() {
        for (prefix, style) in SCOPE_STYLES.iter() {
            if prefix.is_prefix_of(*scope) {
                return *style;
            }
        }
    }
    fg(GRAY)
}

/// The syntax for a code fence's language: pygments' names and aliases first, then syntect's own.
fn lexer(language: &str) -> Option<&'static SyntaxReference> {
    if language.is_empty() {
        return None;
    }
    let lowered = language.to_lowercase();
    let token = match lowered.as_str() {
        "csharp" | "c#" | "cs" => "cs",
        "python3" | "py3" | "python" => "py",
        "javascript" | "jsx" | "node" => "js",
        "shell" | "bash" | "zsh" | "sh" | "console" | "shellsession" => "sh",
        "yml" => "yaml",
        "golang" => "go",
        "rust" => "rs",
        "c++" | "cxx" => "cpp",
        "text" | "plain" | "plaintext" => "txt",
        other => other,
    };
    SYNTAXES.find_syntax_by_token(token)
}

/// Lex one line with a syntax. Lexers match line comments up to a newline, so lex with one and drop it afterwards.
fn lex_line(syntax: &SyntaxReference, line: &str) -> Option<Vec<Span<'static>>> {
    let text = format!("{line}\n");
    let mut state = ParseState::new(syntax);
    let ops = state.parse_line(&text, &SYNTAXES).ok()?;
    let mut stack = ScopeStack::new();
    let mut pieces: Vec<(String, Style)> = Vec::new();
    let mut push = |piece: &str, style: Style| {
        if piece.is_empty() {
            return;
        }
        match pieces.last_mut() {
            Some((last, last_style)) if *last_style == style => last.push_str(piece),
            _ => pieces.push((piece.to_string(), style)),
        }
    };
    let mut at = 0;
    for (position, op) in ops {
        push(&text[at..position], scope_style(&stack));
        at = position;
        stack.apply(&op).ok()?;
    }
    push(&text[at..], scope_style(&stack));
    // Drop the newline that was added.
    if let Some((last, _)) = pieces.last_mut() {
        last.pop();
    }
    Some(
        pieces
            .into_iter()
            .filter(|(text, _)| !text.is_empty())
            .map(|(text, style)| Span::styled(text, style))
            .collect(),
    )
}

/// Style one line at a time. State carries across lines.
#[derive(Default)]
pub struct Highlighter {
    /// The opening fence of the code block in progress, such as "```".
    pub fence: Option<String>,
    lexer: Option<&'static SyntaxReference>,
    pub error: bool,
    pub thinking: bool,
}

impl Highlighter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Style one line (without its newline).
    pub fn line(&mut self, line: &str) -> Line<'static> {
        if let Some(fence) = FENCE.captures(line) {
            let marker = &fence[1];
            let closes = match &self.fence {
                None => true,
                Some(open) => {
                    marker.chars().next() == open.chars().next() && marker.len() >= open.len()
                }
            };
            if closes {
                if self.fence.is_none() {
                    self.fence = Some(marker.to_string());
                    self.lexer = lexer(&fence[2]);
                } else {
                    self.fence = None;
                    self.lexer = None;
                }
                return whole(line, fg(DIM));
            }
        }
        if self.fence.is_some() {
            return self.code(line);
        }
        if line.trim().is_empty() || LABEL.is_match(line) {
            self.error = false;
        }
        if ERROR_START.is_match(line) {
            self.error = true;
        }
        if self.error {
            return whole(line, fg(CORAL));
        }
        if LABEL.is_match(line) {
            return self.label(line);
        }
        if self.thinking {
            return whole(line, italic(DIM));
        }
        self.prose(line)
    }

    /// Section labels the runner prints, such as [model], [Tool: read_file] or [model · Thinking].
    pub fn label(&mut self, line: &str) -> Line<'static> {
        let trimmed = line.trim();
        let inner: String = trimmed
            .chars()
            .skip(1)
            .take(trimmed.chars().count().saturating_sub(2))
            .collect();
        self.thinking = inner.contains("Thinking");
        if inner.starts_with("Error") || inner.contains("Error ·") {
            return whole(line, bold(CORAL));
        }
        if inner.starts_with("Tool result") {
            return whole(line, bold(DIM));
        }
        if inner.starts_with("Tool") {
            let start = line.find(':').map_or(0, |colon| colon + 1);
            let mut spans = Vec::new();
            if start > 0 {
                spans.push(span(&line[..start], bold(TEAL)));
            }
            spans.push(span(&line[start..], bold(ORANGE)));
            return Line::from(spans);
        }
        if self.thinking {
            return whole(line, italic(DIM));
        }
        whole(line, bold(ORANGE))
    }

    pub fn code(&mut self, line: &str) -> Line<'static> {
        if let Some(spans) = self.lexer.and_then(|syntax| lex_line(syntax, line)) {
            return Line::from(spans);
        }
        let mut spans = Vec::new();
        let mut position = 0;
        for found in CODE_TOKENS.captures_iter(line) {
            let whole_match = found.get(0).expect("group 0");
            push_text(&mut spans, &line[position..whole_match.start()], fg(GRAY));
            let group = ["string", "comment", "number", "keyword"]
                .into_iter()
                .find(|name| found.name(name).is_some())
                .unwrap_or("keyword");
            push_text(&mut spans, whole_match.as_str(), code_style(group));
            position = whole_match.end();
        }
        push_text(&mut spans, &line[position..], fg(GRAY));
        Line::from(spans)
    }

    pub fn prose(&mut self, line: &str) -> Line<'static> {
        if let Some(ticket) = TICKET_START.captures(line) {
            let mut spans = Vec::new();
            push_text(&mut spans, &ticket[1], Style::default());
            push_text(&mut spans, &ticket[2], bold(ORANGE));
            push_text(&mut spans, &ticket[3], Style::default());
            push_text(&mut spans, &ticket[4], bold(TEAL));
            spans.extend(self.tokens(&ticket[5], fg(DIM)).spans);
            return Line::from(spans);
        }
        let starts_table = line.trim_start().starts_with('|');
        if HEADING.is_match(line) && !starts_table {
            return whole(line, bold(ORANGE));
        }
        if starts_table {
            return self.table_row(line);
        }
        if let Some(key) = KEY_VALUE.captures(line) {
            let end = key.get(0).expect("group 0").end();
            let mut spans = Vec::new();
            push_text(&mut spans, &key[1], Style::default());
            push_text(&mut spans, &format!("{}:", &key[2]), fg(DIM));
            push_text(&mut spans, &key[3], Style::default());
            spans.extend(self.tokens(&line[end..], fg(GRAY)).spans);
            return Line::from(spans);
        }
        if let Some(bullet) = BULLET.captures(line) {
            let end = bullet.get(0).expect("group 0").end();
            let mut spans = Vec::new();
            push_text(&mut spans, &bullet[1], Style::default());
            push_text(&mut spans, &bullet[2], bold(ORANGE));
            push_text(&mut spans, &bullet[3], Style::default());
            spans.extend(self.tokens(&line[end..], fg(GRAY)).spans);
            return Line::from(spans);
        }
        self.tokens(line, fg(GRAY))
    }

    /// Markdown table rows: dim pipes and separators, bold header cells, highlighted values.
    pub fn table_row(&mut self, line: &str) -> Line<'static> {
        if TABLE_RULE.is_match(line) {
            return whole(line, fg(DIM));
        }
        let mut spans = Vec::new();
        for (index, cell) in line.split('|').enumerate() {
            if index > 0 {
                push_text(&mut spans, "|", fg(DIM));
            }
            if matches!(
                cell.trim(),
                "Task" | "Result" | "Status" | "Note" | "What changed" | "Tests" | "Human check"
            ) {
                push_text(&mut spans, cell, bold(GRAY));
            } else {
                spans.extend(self.tokens(cell, fg(GRAY)).spans);
            }
        }
        Line::from(spans)
    }

    /// Highlight the tokens of prose over a base style.
    pub fn tokens(&self, line: &str, base: Style) -> Line<'static> {
        let mut spans = Vec::new();
        let mut position = 0;
        for found in TOKENS.captures_iter(line).flatten() {
            let whole_match = found.get(0).expect("group 0");
            push_text(&mut spans, &line[position..whole_match.start()], base);
            let group = TOKEN_GROUPS
                .into_iter()
                .find(|name| found.name(name).is_some())
                .unwrap_or("number");
            push_text(&mut spans, whole_match.as_str(), token_style(group));
            position = whole_match.end();
        }
        push_text(&mut spans, &line[position..], base);
        Line::from(spans)
    }
}

fn push_text(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if !text.is_empty() {
        spans.push(span(text, style));
    }
}

/// Turn styled lines into ANSI escape codes for a plain terminal stream.
#[derive(Default)]
pub struct AnsiRenderer;

fn color_codes(color: Color, background: bool) -> Option<String> {
    let (base, bright) = if background { (40, 100) } else { (30, 90) };
    let named = |n: u8| Some(n.to_string());
    match color {
        Color::Reset => None,
        Color::Black => named(base),
        Color::Red => named(base + 1),
        Color::Green => named(base + 2),
        Color::Yellow => named(base + 3),
        Color::Blue => named(base + 4),
        Color::Magenta => named(base + 5),
        Color::Cyan => named(base + 6),
        Color::Gray => named(base + 7),
        Color::DarkGray => named(bright),
        Color::LightRed => named(bright + 1),
        Color::LightGreen => named(bright + 2),
        Color::LightYellow => named(bright + 3),
        Color::LightBlue => named(bright + 4),
        Color::LightMagenta => named(bright + 5),
        Color::LightCyan => named(bright + 6),
        Color::White => named(bright + 7),
        Color::Indexed(n) => Some(format!("{};5;{n}", if background { 48 } else { 38 })),
        Color::Rgb(r, g, b) => Some(format!(
            "{};2;{r};{g};{b}",
            if background { 48 } else { 38 }
        )),
    }
}

/// The SGR sequence for a style, in Rich's order: attributes, then the color, then the background.
fn sgr(style: Style) -> Option<String> {
    let mut codes: Vec<String> = Vec::new();
    for (modifier, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::RAPID_BLINK, "6"),
        (Modifier::REVERSED, "7"),
        (Modifier::HIDDEN, "8"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if style.add_modifier.contains(modifier) && !style.sub_modifier.contains(modifier) {
            codes.push(code.to_string());
        }
    }
    codes.extend(style.fg.and_then(|c| color_codes(c, false)));
    codes.extend(style.bg.and_then(|c| color_codes(c, true)));
    (!codes.is_empty()).then(|| format!("\x1b[{}m", codes.join(";")))
}

impl AnsiRenderer {
    pub fn new() -> Self {
        AnsiRenderer
    }

    /// One styled line as text with ANSI escapes (24-bit color), without a newline.
    pub fn render(&self, line: &Line) -> String {
        let mut out = String::new();
        for span in &line.spans {
            if span.content.is_empty() {
                continue;
            }
            match sgr(line.style.patch(span.style)) {
                Some(open) => {
                    out.push_str(&open);
                    out.push_str(&span.content);
                    out.push_str("\x1b[0m");
                }
                None => out.push_str(&span.content),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// {substring: style} for each styled span of a line.
    fn styles(line: &Line) -> HashMap<String, Style> {
        line.spans
            .iter()
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    fn line_style(line: &Line) -> Style {
        line.style
            .patch(line.spans.first().map(|s| s.style).unwrap_or_default())
    }

    #[test]
    fn test_palette_matches_the_shared_colors() {
        for (name, color) in [
            ("orange", ORANGE),
            ("coral", CORAL),
            ("teal", TEAL),
            ("gray", GRAY),
        ] {
            let (r, g, b) = crate::ui::color(name);
            assert_eq!(color, Color::Rgb(r, g, b));
        }
    }

    #[test]
    fn test_text_is_never_changed() {
        let lines = [
            "[qwen3 · Connected · preparing project context]",
            "[1/3] 047_20261108-0831_sandbox-wall-section.md — fresh context",
            "Project: /Users/me/doodle_quest",
            "```python",
            "def jump(height=2.5):  # comment",
            "    return f\"{height}\"",
            "```",
            "| 084 lightning-fire | Done | Fire burns 5s then smokes |",
            "|---|---|---|",
            "- Ran `pytest -q`: 12 passed, 1 failed in 3.2s",
            "[Error] Command failed",
        ];
        let mut highlighter = Highlighter::new();
        for line in lines {
            assert_eq!(plain(&highlighter.line(line)), line);
        }
    }

    #[test]
    fn test_code_blocks_use_the_language_lexer() {
        let mut highlighter = Highlighter::new();
        highlighter.line("```python");
        let spans = styles(&highlighter.line("def jump(height=2):  # go up"));
        assert_eq!(spans["def"], fg(CORAL));
        assert_eq!(spans["jump"], bold(ORANGE));
        assert_eq!(spans["2"], fg(ORANGE));
        assert!(spans["# go up"].add_modifier.contains(Modifier::ITALIC));
        highlighter.line("```");
        assert!(highlighter.fence.is_none());
    }

    #[test]
    fn test_line_comments_and_absolute_paths() {
        let mut highlighter = Highlighter::new();
        highlighter.line("```csharp");
        let comment = highlighter.line("    // One collider per section");
        assert_eq!(plain(&comment), "    // One collider per section");
        assert!(
            styles(&comment)["// One collider per section"]
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        highlighter.line("```");
        let spans = styles(&highlighter.line("Logs: /Users/me/.local/state/donut/run-1"));
        assert!(spans.contains_key("/Users/me/.local/state/donut/run-1"));
    }

    #[test]
    fn test_unknown_language_still_gets_basic_tokens() {
        let mut highlighter = Highlighter::new();
        highlighter.line("```");
        let spans = styles(&highlighter.line("return \"ok\" + 42"));
        assert_eq!(spans["return"], fg(CORAL));
        assert_eq!(spans["\"ok\""], fg(TEAL));
        assert_eq!(spans["42"], fg(ORANGE));
    }

    #[test]
    fn test_prose_highlights_brackets_code_numbers_and_status_words() {
        let spans = styles(
            &Highlighter::new().line("Ran `pytest -q` (unit): 12 passed, 1 failed [slow] in 3.2s"),
        );
        assert_eq!(spans["`pytest -q`"], fg(ORANGE));
        assert_eq!(spans["(unit)"], fg(TEAL));
        assert_eq!(spans["[slow]"], fg(TEAL));
        assert_eq!(spans["12"], fg(CORAL));
        assert_eq!(spans["3.2s"], fg(CORAL));
        assert_eq!(spans["passed"], bold(TEAL));
        assert_eq!(spans["failed"], bold(CORAL));
    }

    #[test]
    fn test_runner_lines_and_tables() {
        let mut highlighter = Highlighter::new();
        let ticket = styles(&highlighter.line("[1/3] 047_sandbox-wall.md — fresh context"));
        assert_eq!(ticket["[1/3]"], bold(ORANGE));
        assert_eq!(ticket["047_sandbox-wall.md"], bold(TEAL));
        let row =
            styles(&highlighter.line("| 086 install-thunderstorm | Done | Full storm cycle |"));
        assert_eq!(row["Done"], bold(TEAL));
        assert_ne!(row["|"].fg, Some(ORANGE));
        assert_eq!(
            line_style(&highlighter.line("PLANNING COMPLETE")),
            bold(ORANGE)
        );
    }

    #[test]
    fn test_thinking_sections_are_dimmed_until_the_next_label() {
        let mut highlighter = Highlighter::new();
        highlighter.line("[qwen3 · Thinking]");
        assert!(
            line_style(&highlighter.line("Let me look at the wall code."))
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        highlighter.line("[qwen3]");
        assert!(
            !line_style(&highlighter.line("Done."))
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn test_labels_errors_and_ansi_output() {
        let mut highlighter = Highlighter::new();
        let tool = highlighter.line("[Tool: read_file]");
        assert_eq!(tool.spans[0].style, bold(TEAL));
        assert_eq!(tool.spans[1].style, bold(ORANGE));
        highlighter.line("Traceback (most recent call last):");
        assert_eq!(line_style(&highlighter.line("  File x")), fg(CORAL));
        highlighter.line("");
        assert_ne!(line_style(&highlighter.line("fine")), fg(CORAL));
        let renderer = AnsiRenderer::new();
        assert_eq!(
            renderer.render(&whole("hi", bold(ORANGE))),
            "\x1b[1;38;2;255;168;24mhi\x1b[0m"
        );
        assert_eq!(renderer.render(&whole("hi", Style::default())), "hi");
    }
}
