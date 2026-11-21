//! Terminal colors sampled approximately from the supplied palette.

use std::io::{IsTerminal, Write};
use std::time::Instant;

/// The palette, by name: orange, navy, coral, teal, gray.
pub const COLORS: &[(&str, (u8, u8, u8))] = &[
    ("orange", (255, 168, 24)),
    ("navy", (18, 77, 99)),
    ("coral", (255, 94, 78)),
    ("teal", (23, 184, 190)),
    ("gray", (222, 224, 228)),
];

/// The RGB value of a palette color. Panics on an unknown name, as the Python KeyError did.
pub fn color(name: &str) -> (u8, u8, u8) {
    COLORS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, rgb)| *rgb)
        .unwrap_or_else(|| panic!("unknown color {name}"))
}

/// The color as "#rrggbb".
pub fn hex(name: &str) -> String {
    let (r, g, b) = color(name);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Whether output is colored: stdout is a terminal and NO_COLOR is not set.
pub fn colors_enabled() -> bool {
    colors_for(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// The color rule alone: a terminal, and no NO_COLOR.
pub fn colors_for(is_terminal: bool, no_color: bool) -> bool {
    is_terminal && !no_color
}

/// Text in a palette color when stdout is a terminal and NO_COLOR is not set.
pub fn paint(text: &str, color_name: &str) -> String {
    paint_if(text, color_name, colors_enabled())
}

/// Text in a palette color when enabled.
pub fn paint_if(text: &str, color_name: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    let (r, g, b) = color(color_name);
    format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
}

/// The installed release, or "dev".
pub fn release_version() -> String {
    option_env!("DONUT_RELEASE_VERSION")
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string()
}

/// The mascot: a donut telling a donut hole to do the thing.
pub const LOGO: &str = r#"            _.------------._           .-----------------.
        .-''   *    +    o  ''-.       |  do the thing.  |
      .'  +    o     *     +    '.     '.  .-------------'
     /   o  +   .-''''-.     *    \   / .'      .----.
    |   *      /        \     +    | /.'        | ok |
    |    +    |          |    o    |            '-. .'
    |  o   +   \        /    *     |              |/
     \   *      '-.__.-'  o   +   /           .-''''''-.
      '.    +    o    *    o    .'           / o   *  + \
        '-._   *    +   o   _.-'            | +  *  +  o |
            '-.__________.-'                 \ *   o  + /
                                              '-.____.-'
"#;

/// Left of this column is the big donut; to the right are the speech bubbles and, from HOLE_ROW down, the hole.
const BUBBLE: usize = 36;
const HOLE_ROW: usize = 7;
const SPRINKLE_COLORS: [&str; 5] = ["coral", "teal", "gray", "coral", "teal"];
const SPRINKLES: &str = "*+o";
const WORDS: [&str; 2] = ["do the thing.", "ok"];

/// A run of text and its palette color (None for spaces).
pub type Run = (String, Option<&'static str>);

/// The logo as lines of (text, color) runs, shared by the ANSI printer and the terminal UI.
///
/// The outline avoids characters such as ~ whose shape varies between terminal fonts, and sprinkles use
/// characters the outline never does, so the edge stays solid in any monospace font.
pub fn logo_segments() -> Vec<Vec<Run>> {
    let mut lines = Vec::new();
    let mut sprinkle = 0;
    for (row, line) in LOGO.lines().enumerate() {
        let mut runs: Vec<Run> = Vec::new();
        let mut spoken = vec![false; line.len()];
        for word in WORDS {
            if let Some(at) = line.get(BUBBLE..).and_then(|tail| tail.find(word)) {
                for flag in &mut spoken[BUBBLE + at..BUBBLE + at + word.len()] {
                    *flag = true;
                }
            }
        }
        for (col, ch) in line.chars().enumerate() {
            let col_is_donut = col < BUBBLE || row >= HOLE_ROW;
            let color = if ch == ' ' {
                None
            } else if spoken[col] {
                Some("orange")
            } else if col_is_donut && SPRINKLES.contains(ch) {
                let chosen = SPRINKLE_COLORS[sprinkle % SPRINKLE_COLORS.len()];
                sprinkle += 1;
                Some(chosen)
            } else if col_is_donut {
                Some("orange")
            } else {
                Some("gray")
            };
            match runs.last_mut() {
                Some(last) if last.1 == color => last.0.push(ch),
                _ => runs.push((ch.to_string(), color)),
            }
        }
        lines.push(runs);
    }
    lines
}

/// The logo as one string, in ANSI colors when the terminal shows them.
pub fn painted_logo() -> String {
    painted_logo_if(colors_enabled())
}

/// painted_logo() with the color decision given.
pub fn painted_logo_if(enabled: bool) -> String {
    logo_segments()
        .iter()
        .map(|runs| {
            runs.iter()
                .map(|(text, color)| match color {
                    Some(name) => paint_if(text, name, enabled),
                    None => text.clone(),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The logo as an SVG image in the terminal's colors, for the README, where a code block has no color.
///
/// Every run of characters is placed and stretched to its column, so the art lines up in whatever monospace
/// font the viewer's system has. write_mascot writes it to docs/images/mascot.svg.
pub fn logo_svg() -> String {
    let (cell, line, pad) = (9usize, 19usize, 18usize);
    let rows = logo_segments();
    let width = LOGO
        .lines()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0)
        * cell
        + 2 * pad;
    let height = rows.len() * line + 2 * pad;
    let mut out = vec![
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\" width=\"{width}\" \
             height=\"{height}\" role=\"img\" aria-label=\"A roughly drawn donut tells a donut hole: do the \
             thing. The donut hole says ok.\">"
        ),
        format!("<rect width=\"{width}\" height=\"{height}\" rx=\"10\" fill=\"#101820\"/>"),
        "<g font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\" font-size=\"15\" \
         xml:space=\"preserve\">"
            .to_string(),
    ];
    for (row, runs) in rows.iter().enumerate() {
        let mut column = 0;
        for (text, color) in runs {
            let length = text.chars().count();
            // Leading spaces only move the start: a stretched run must begin and end on a drawn character.
            let start = column + length - text.trim_start_matches(' ').chars().count();
            let shown = text.trim_matches(' ');
            column += length;
            let Some(color) = color else { continue };
            if shown.is_empty() {
                continue;
            }
            let (x, y) = (pad + start * cell, pad + row * line + 14);
            let fill = hex(color);
            out.push(format!(
                "<text x=\"{x}\" y=\"{y}\" textLength=\"{}\" lengthAdjust=\"spacingAndGlyphs\" \
                 fill=\"{fill}\">{}</text>",
                shown.chars().count() * cell,
                xml_escape(shown)
            ));
        }
    }
    out.push("</g>".into());
    out.push("</svg>".into());
    out.join("\n") + "\n"
}

/// Write the README mascot, docs/images/mascot.svg, under the given project root.
pub fn write_mascot(root: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(root.join("docs/images/mascot.svg"), logo_svg())
}

const TAGLINE: &str = "  Your planning model writes the tickets. Your worker model takes them one at a time. Do the thing.";

/// The one-line banner with the palette rule beneath it.
pub fn banner() {
    print!("{}", banner_text(colors_enabled()));
}

/// banner() as text.
pub fn banner_text(enabled: bool) -> String {
    let rule: String = COLORS
        .iter()
        .map(|(name, _)| paint_if(&"\u{2501}".repeat(7), name, enabled))
        .collect();
    format!(
        "{}{}{}\n{rule}\n",
        paint_if(" \u{25c9} ", "coral", enabled),
        paint_if("DONUT", "orange", enabled),
        paint_if(TAGLINE, "gray", enabled)
    )
}

/// The welcome screen: logo, release and next steps.
pub fn welcome(installed: bool) {
    print!(
        "{}",
        welcome_text(installed, &release_version(), colors_enabled())
    );
}

/// welcome() as text, for a given release and color decision.
pub fn welcome_text(installed: bool, release: &str, enabled: bool) -> String {
    let paint = |text: &str, name: &str| paint_if(text, name, enabled);
    let mut out = String::new();
    out += &painted_logo_if(enabled);
    out += "\n";
    out += &format!(
        "{}{}\n",
        paint("  DONUT", "orange"),
        paint(&format!("  /  {release}"), "coral")
    );
    out += &format!(
        "{}\n",
        paint(
            "  Your planning model writes the tickets. Your worker model takes them one at a time.",
            "gray"
        )
    );
    out += &format!("{}\n", paint(&format!("  {}", "-".repeat(38)), "coral"));
    if installed {
        out += &format!("{}\n", paint("  [OK] Installation complete.", "orange"));
        out += "  Open a new terminal in your project, then:\n";
        out += &format!("{}\n", paint("    donut setup", "coral"));
        out += "  Explore commands: donut --help\n";
    } else {
        out += &format!(
            "{}\n",
            paint("  SETUP  /  Local or hosted. Your choice.", "orange")
        );
    }
    out += "\n";
    out
}

/// A spinner line while an agent works; on a pipe, a plain status line every 15 seconds.
#[derive(Debug)]
pub struct Progress {
    started: Instant,
    last: u64,
    frame: usize,
    visible: bool,
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress {
    pub fn new() -> Self {
        Progress {
            started: Instant::now(),
            last: 0,
            frame: 0,
            visible: false,
        }
    }

    /// Erase the spinner line, if one is showing.
    pub fn clear(&mut self) {
        let mut out = std::io::stdout();
        self.clear_to(&mut out);
    }

    pub fn clear_to(&mut self, out: &mut impl Write) {
        if self.visible {
            self.visible = false;
            let _ = write!(out, "\r\x1b[2K");
            let _ = out.flush();
        }
    }

    pub fn tick(&mut self, label: &str) {
        let mut out = std::io::stdout();
        let tty = out.is_terminal();
        self.tick_to(&mut out, tty, label);
    }

    pub fn tick_to(&mut self, out: &mut impl Write, tty: bool, label: &str) {
        let elapsed = self.started.elapsed().as_secs();
        if tty {
            self.visible = true;
            let frames = ['\u{25d0}', '\u{25d3}', '\u{25d1}', '\u{25d2}'];
            let enabled = colors_for(true, std::env::var_os("NO_COLOR").is_some());
            let _ = write!(
                out,
                "\r{} {label} \u{b7} {elapsed}s elapsed \u{b7} waiting for agent output",
                paint_if(&frames[self.frame % 4].to_string(), "orange", enabled)
            );
            let _ = out.flush();
            self.frame += 1;
        } else if elapsed.saturating_sub(self.last) >= 15 {
            let _ = writeln!(
                out,
                "[{label} \u{b7} {elapsed}s elapsed \u{b7} waiting for agent output]"
            );
            let _ = out.flush();
            self.last = elapsed;
        }
    }
}

/// Highlight streamed agent output a line at a time, including lines split across stream deltas.
pub struct CodeStream {
    pending: String,
    pub highlighter: crate::highlight::Highlighter,
    renderer: crate::highlight::AnsiRenderer,
}

impl Default for CodeStream {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeStream {
    pub fn new() -> Self {
        CodeStream {
            pending: String::new(),
            highlighter: crate::highlight::Highlighter::new(),
            renderer: crate::highlight::AnsiRenderer::new(),
        }
    }

    /// The open code fence, if the stream is inside one.
    pub fn fence(&self) -> Option<&str> {
        self.highlighter.fence.as_deref()
    }

    /// Add streamed text; returns the finished lines, colored when stdout shows colors.
    pub fn feed(&mut self, text: &str, final_: bool) -> String {
        self.feed_with(text, final_, colors_enabled())
    }

    /// feed() with the color decision given.
    pub fn feed_with(&mut self, text: &str, final_: bool, colored: bool) -> String {
        self.pending.push_str(text);
        let mut output = String::new();
        loop {
            let chars = self.pending.chars().count();
            if !(self.pending.contains('\n') || (final_ && chars > 0) || chars >= 4096) {
                break;
            }
            let end = match self.pending.find('\n') {
                Some(at) => at + 1,
                None => self
                    .pending
                    .char_indices()
                    .nth(chars.min(4096))
                    .map_or(self.pending.len(), |(at, _)| at),
            };
            let line: String = self.pending.drain(..end).collect();
            let (body, newline) = match line.strip_suffix('\n') {
                Some(body) => (body, "\n"),
                None => (line.as_str(), ""),
            };
            let styled = self.highlighter.line(body);
            if colored {
                output.push_str(&self.renderer.render(&styled));
            } else {
                output.push_str(body);
            }
            output.push_str(newline);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;

    fn strip(text: &str) -> String {
        Regex::new("\x1b\\[[0-9;]*m")
            .unwrap()
            .replace_all(text, "")
            .into_owned()
    }

    #[test]
    fn test_clear_does_not_erase_streamed_tokens() {
        let mut out = Vec::new();
        let mut progress = Progress::new();
        progress.clear_to(&mut out);
        assert!(out.is_empty());
        progress.tick_to(&mut out, true, "Working");
        assert!(String::from_utf8_lossy(&out).contains("Working"));
        progress.clear_to(&mut out);
        assert!(String::from_utf8_lossy(&out).ends_with("\r\x1b[2K"));
    }

    #[test]
    fn test_no_color() {
        assert!(!colors_for(true, true));
        assert!(!colors_for(false, false));
        assert!(colors_for(true, false));
        assert_eq!(paint_if("hello", "teal", false), "hello");
        assert_eq!(
            paint_if("hello", "orange", true),
            "\x1b[38;2;255;168;24mhello\x1b[0m"
        );
    }

    #[test]
    fn test_welcome_is_ascii_and_plain_when_redirected() {
        let output = welcome_text(true, "1.2.3", false);
        assert!(output.is_ascii());
        assert!(!output.contains('\x1b'));
        assert!(output.contains("do the thing"));
        assert!(output.contains("1.2.3"));
        assert!(output.contains("donut setup"));
        assert!(LOGO.lines().map(|line| line.len()).max().unwrap() <= 72); // Fits an 80-column terminal with its indent.
    }

    #[test]
    fn test_logo_renders_the_same_in_any_monospace_font() {
        let lines: Vec<&str> = LOGO.lines().collect();
        // ~ is drawn differently by different fonts, which broke up the outline.
        assert!(!LOGO.contains('~'));
        // The speech bubble's three rows share one right edge.
        let edges: std::collections::HashSet<usize> = lines[..3]
            .iter()
            .map(|line| line.trim_end().len())
            .collect();
        assert_eq!(edges.len(), 1);
        // Each donut row is mirror-symmetric apart from its sprinkles.
        for line in &lines {
            let donut = line[..36.min(line.len())].trim_end();
            let outline: String = donut
                .chars()
                .map(|c| if "*+o".contains(c) { ' ' } else { c })
                .collect();
            let swapped: String = outline
                .chars()
                .map(|c| match c {
                    '/' => '\\',
                    '\\' => '/',
                    other => other,
                })
                .rev()
                .collect();
            assert_eq!(outline.trim(), swapped.trim(), "{line}");
        }
    }

    #[test]
    fn test_donut_hole_answers_ok_in_the_warm_color() {
        let runs: Vec<Run> = logo_segments().into_iter().flatten().collect();
        assert!(runs.contains(&("ok".to_string(), Some("orange"))));
        assert!(runs.contains(&("thing.".to_string(), Some("orange"))));
        // The little one is a donut hole: solid all through, sprinkles and no hole of its own.
        let lines: Vec<&str> = LOGO.lines().collect();
        let hole: Vec<&str> = lines[lines.len() - 5..]
            .iter()
            .map(|line| &line[36.min(line.len())..])
            .collect();
        assert!(
            !hole
                .iter()
                .any(|line| line.contains('(') || line.contains(')')),
            "{hole:?}"
        );
        assert!(hole[2].contains('*'));
    }

    #[test]
    fn test_the_readme_shows_the_same_mascot_as_the_terminal() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        assert!(readme.contains("src=\"docs/images/mascot.svg\""));
        // The image is generated from the terminal's art and colors: write_mascot rewrites it.
        assert_eq!(
            std::fs::read_to_string(root.join("docs/images/mascot.svg")).unwrap(),
            logo_svg()
        );
        let svg = logo_svg();
        assert!(svg.contains("fill=\"#ffa818\">ok</text>")); // The same warm color as in the terminal.
        assert!(!svg.contains(">()</text>")); // The little one is a donut hole, with no hole of its own.
    }

    #[test]
    fn test_setup_welcome_does_not_claim_new_installation() {
        let output = welcome_text(false, "dev", false);
        assert!(output.contains("SETUP"));
        assert!(!output.contains("Installation complete"));
    }

    #[test]
    fn test_code_highlighting_survives_split_deltas() {
        let text = "Explanation `inline`\n```python\ndef example():\n    return \"hello\" + 42\n```\nDone\n";
        let mut renderer = CodeStream::new();
        let mut output = String::new();
        for character in text.chars() {
            output += &renderer.feed_with(&character.to_string(), false, true);
        }
        output += &renderer.feed_with("", true, true);
        assert_eq!(strip(&output), text);
        assert!(output.contains("38;2;255;94;78mdef"));
        assert_eq!(renderer.fence(), None);
    }

    #[test]
    fn test_code_highlighting_respects_no_color_and_flushes() {
        let mut renderer = CodeStream::new();
        let output = renderer.feed_with("```cs\nreturn 1;", false, false)
            + &renderer.feed_with("", true, false);
        assert_eq!(output, "```cs\nreturn 1;");
    }

    #[test]
    fn test_thinking_tokens_and_errors_use_palette() {
        let mut renderer = CodeStream::new();
        let output = renderer.feed_with(
            "Use `file.py` with 42 (optional) {value}\n[Error] Failed 3 tests\n",
            false,
            true,
        );
        assert!(output.contains("38;2;255;168;24m`file.py`"));
        assert!(output.contains("38;2;255;94;78m42"));
        assert!(output.contains("38;2;23;184;190m(optional)"));
        assert!(output.contains("38;2;23;184;190m{value}"));
        assert!(output.contains("38;2;255;94;78m[Error] Failed 3 tests"));
        assert_eq!(
            strip(&output),
            "Use `file.py` with 42 (optional) {value}\n[Error] Failed 3 tests\n"
        );
    }

    #[test]
    fn test_multiline_error_body_stays_red_until_next_section() {
        let output = CodeStream::new().feed_with(
            "[Error \u{b7} Tool result]\nCommand failed\n[model]\nRetrying\n",
            false,
            true,
        );
        assert!(output.contains("38;2;255;94;78mCommand failed"));
        assert!(!output.contains("38;2;255;94;78mRetrying"));
    }

    #[test]
    fn test_banner_is_plain_when_redirected() {
        let text = banner_text(false);
        assert!(text.starts_with(" \u{25c9} DONUT"));
        assert!(!text.contains('\x1b'));
        assert_eq!(text.lines().nth(1).unwrap().chars().count(), 35);
    }
}
