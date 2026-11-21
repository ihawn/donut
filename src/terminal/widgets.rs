//! Transcript, Menu and Draft widgets: pure state plus `render(area, buf)` functions.
//!
//! These replace the Textual `RichLog`, `Menu` (an inline option list) and `TextArea` of the earlier Python terminal.
//! Nothing here reads the terminal; the app feeds key and mouse events in and draws with ratatui buffers.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthChar;

use super::RichLine;
use crate::highlight::{CORAL, DIM, GRAY, ORANGE, TEAL};

/// The screen background of the whole interface.
pub const BG: Color = Color::Rgb(0x10, 0x18, 0x20);
const SELECTION_BG: Color = Color::Rgb(0x2f, 0x5f, 0x8a);
const HIGHLIGHT_BG: Color = Color::Rgb(0x3a, 0x2a, 0x10);

fn mix(a: Color, b: Color, share: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let m =
                |x: u8, y: u8| (f32::from(x) * share + f32::from(y) * (1.0 - share)).round() as u8;
            Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
        }
        _ => a,
    }
}

fn cell_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else {
        ch.width().unwrap_or(0)
    }
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_symbol(" ").set_style(style);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Transcript

#[derive(Clone)]
struct Cell {
    ch: char,
    style: Style,
    /// Character offset in the logical line's plain text.
    idx: usize,
    x: usize,
    w: usize,
}

#[derive(Clone)]
struct Row {
    entry: usize,
    cells: Vec<Cell>,
}

impl Row {
    fn text(&self) -> String {
        self.cells
            .iter()
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}

fn wrap_entry(entry: usize, line: &RichLine, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows: Vec<Row> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut x = 0usize;
    let mut last_space: Option<usize> = None;
    let mut wrapped = false;
    let mut idx = 0usize;
    for span in &line.line.spans {
        let style = line.line.style.patch(span.style);
        for raw in span.content.chars() {
            let at = idx;
            idx += 1;
            let ch = if raw == '\t' { ' ' } else { raw };
            let w = if raw == '\t' { 1 } else { cell_width(raw) };
            if w == 0 {
                continue;
            }
            while ch != ' ' && x + w > width && !cur.is_empty() {
                match last_space {
                    Some(s) => {
                        let mut rest = cur.split_off(s + 1);
                        rows.push(Row {
                            entry,
                            cells: std::mem::take(&mut cur),
                        });
                        let mut nx = 0;
                        for cell in &mut rest {
                            cell.x = nx;
                            nx += cell.w;
                        }
                        x = nx;
                        cur = rest;
                    }
                    _ => {
                        rows.push(Row {
                            entry,
                            cells: std::mem::take(&mut cur),
                        });
                        x = 0;
                    }
                }
                last_space = None;
                wrapped = true;
            }
            if ch == ' ' && cur.is_empty() && wrapped {
                continue;
            }
            cur.push(Cell {
                ch,
                style,
                idx: at,
                x,
                w,
            });
            x += w;
            if ch == ' ' {
                last_space = Some(cur.len() - 1);
            }
        }
    }
    rows.push(Row { entry, cells: cur });
    rows
}

/// What a mouse event did to the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptEvent {
    None,
    /// The mouse was released after a drag: the selected text.
    Selected(String),
    /// A click without a drag on a link.
    Link(String),
    Scrolled,
}

type Spot = (usize, usize); // (column, wrapped row)

/// The conversation log, with mouse selection and link clicks.
pub struct Transcript {
    entries: Vec<RichLine>,
    rows: Vec<Row>,
    width: usize,
    height: usize,
    scroll: usize,
    follow: bool,
    selection: Option<(Spot, Spot)>,
    drag: Option<(Spot, bool)>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Transcript {
            entries: Vec::new(),
            rows: Vec::new(),
            width: 80,
            height: 24,
            scroll: 0,
            follow: true,
            selection: None,
            drag: None,
        }
    }

    pub fn push(&mut self, line: RichLine) {
        let entry = self.entries.len();
        self.rows.extend(wrap_entry(entry, &line, self.width));
        self.entries.push(line);
        if self.follow {
            self.scroll = self.max_scroll_y();
        }
    }

    pub fn push_lines(&mut self, lines: Vec<RichLine>) {
        for line in lines {
            self.push(line);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.rows.clear();
        self.scroll = 0;
        self.selection = None;
        self.drag = None;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn plain(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| e.plain().trim_end().to_string())
            .collect()
    }

    pub fn body(&self) -> String {
        self.plain().join("\n")
    }

    pub fn wrapped_plain(&self, width: usize) -> Vec<String> {
        self.entries
            .iter()
            .enumerate()
            .flat_map(|(i, e)| wrap_entry(i, e, width))
            .map(|r| r.text())
            .collect()
    }

    pub fn set_viewport(&mut self, width: u16, height: u16) {
        let (width, height) = (usize::from(width).max(1), usize::from(height));
        if width != self.width {
            self.width = width;
            self.rows = self
                .entries
                .iter()
                .enumerate()
                .flat_map(|(i, e)| wrap_entry(i, e, width))
                .collect();
            self.selection = None;
        }
        let resized = height != self.height;
        self.height = height;
        if resized && self.follow {
            self.scroll = self.max_scroll_y();
        }
        self.scroll = self.scroll.min(self.max_scroll_y());
    }

    pub fn scroll_y(&self) -> usize {
        self.scroll
    }

    pub fn max_scroll_y(&self) -> usize {
        self.rows.len().saturating_sub(self.height)
    }

    pub fn scroll_end(&mut self) {
        self.scroll = self.max_scroll_y();
        self.follow = true;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll_y();
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
        if self.scroll + 1 >= max {
            self.follow = true;
        }
    }

    pub fn follow(&self) -> bool {
        self.follow
    }

    pub fn set_follow(&mut self, on: bool) {
        self.follow = on;
    }

    fn content_width(area: Rect) -> u16 {
        area.width.saturating_sub(3)
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.set_viewport(Self::content_width(area), area.height);
        fill(buf, area, Style::default().bg(BG));
        if area.width == 0 || area.height == 0 {
            return;
        }
        let selection = self.selection.map(|(a, b)| {
            if (a.1, a.0) <= (b.1, b.0) {
                (a, b)
            } else {
                (b, a)
            }
        });
        let content = usize::from(Self::content_width(area));
        for i in 0..usize::from(area.height) {
            let Some(row) = self.rows.get(self.scroll + i) else {
                break;
            };
            let y = area.y + i as u16;
            for cell in &row.cells {
                if cell.x + cell.w > content {
                    break;
                }
                let mut style = cell.style;
                if style.bg.is_none() {
                    style = style.bg(BG);
                }
                if let Some(((c0, r0), (c1, r1))) = selection {
                    let r = self.scroll + i;
                    let inside =
                        r >= r0 && r <= r1 && (r > r0 || cell.x >= c0) && (r < r1 || cell.x <= c1);
                    if inside {
                        style = style.bg(SELECTION_BG);
                    }
                }
                let x = area.x + 1 + cell.x as u16;
                if x < area.right() {
                    buf.set_stringn(x, y, cell.ch.to_string(), cell.w, style);
                }
            }
        }
        // Scrollbar: a dim coral thumb on the background.
        let bar = area.x + area.width - 1;
        let total = self.rows.len();
        let height = usize::from(area.height);
        if total > height {
            let thumb = (height * height / total).max(1);
            let max = self.max_scroll_y().max(1);
            let top = self.scroll * (height - thumb) / max;
            let color = mix(CORAL, BG, 0.5);
            for i in top..(top + thumb).min(height) {
                buf[(bar, area.y + i as u16)]
                    .set_symbol(" ")
                    .set_style(Style::default().bg(color));
            }
        }
    }

    fn spot(&self, area: Rect, column: u16, row: u16) -> Spot {
        let col = usize::from(column.saturating_sub(area.x + 1));
        let line = (usize::from(row.saturating_sub(area.y)) + self.scroll)
            .min(self.rows.len().saturating_sub(1));
        (col, line)
    }

    pub fn on_mouse(&mut self, ev: MouseEvent, area: Rect) -> TranscriptEvent {
        let inside = ev.column >= area.x
            && ev.column < area.right()
            && ev.row >= area.y
            && ev.row < area.bottom();
        match ev.kind {
            MouseEventKind::ScrollUp if inside => {
                self.scroll_by(-3);
                if self.scroll + 1 < self.max_scroll_y() {
                    self.follow = false;
                }
                TranscriptEvent::Scrolled
            }
            MouseEventKind::ScrollDown if inside => {
                self.scroll_by(3);
                TranscriptEvent::Scrolled
            }
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let spot = self.spot(area, ev.column, ev.row);
                self.selection = None;
                self.drag = Some((spot, false));
                TranscriptEvent::None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let spot = self.spot(area, ev.column, ev.row);
                if let Some((anchor, moved)) = self.drag.as_mut() {
                    if spot != *anchor {
                        *moved = true;
                    }
                    let anchor = *anchor;
                    self.selection = Some((anchor, spot));
                }
                TranscriptEvent::None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some((anchor, moved)) = self.drag.take() else {
                    return TranscriptEvent::None;
                };
                let spot = self.spot(area, ev.column, ev.row);
                if moved || spot != anchor {
                    self.selection = Some((anchor, spot));
                    return match self.selected_text() {
                        Some(text) => TranscriptEvent::Selected(text),
                        None => TranscriptEvent::None,
                    };
                }
                self.selection = None;
                match self.link_at(area, ev.column, ev.row) {
                    Some(url) => TranscriptEvent::Link(url),
                    None => TranscriptEvent::None,
                }
            }
            _ => TranscriptEvent::None,
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let ((c0, r0), (c1, r1)) = if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        };
        let mut out = Vec::new();
        for r in r0..=r1.min(self.rows.len().saturating_sub(1)) {
            let row = self.rows.get(r)?;
            let text: String = row
                .cells
                .iter()
                .filter(|c| (r > r0 || c.x >= c0) && (r < r1 || c.x <= c1))
                .map(|c| c.ch)
                .collect();
            out.push(text.trim_end().to_string());
        }
        let text = out.join("\n");
        if text.is_empty() { None } else { Some(text) }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.drag = None;
    }

    pub fn link_at(&self, area: Rect, column: u16, row: u16) -> Option<String> {
        if column < area.x + 1 || row < area.y {
            return None;
        }
        let col = usize::from(column - area.x - 1);
        let row = self.rows.get(usize::from(row - area.y) + self.scroll)?;
        let cell = row.cells.iter().find(|c| c.x <= col && col < c.x + c.w)?;
        self.entries
            .get(row.entry)?
            .link_at(cell.idx)
            .map(str::to_string)
    }
}

// ---------------------------------------------------------------------------------------------------------
// Menu

#[derive(Clone, Debug, PartialEq)]
pub struct MenuRow {
    pub label: String,
    pub value: serde_json::Value,
    pub description: String,
    pub command: String,
}

impl MenuRow {
    pub fn new(label: impl Into<String>, value: serde_json::Value) -> Self {
        MenuRow {
            label: label.into(),
            value,
            description: String::new(),
            command: String::new(),
        }
    }

    pub fn described(mut self, description: &str) -> Self {
        self.description = description.to_string();
        self
    }

    pub fn command(mut self, command: &str) -> Self {
        self.command = command.to_string();
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    Slash,
    Pick,
    Approve,
    Shell,
}

/// An inline list under the transcript. Focus never leaves the prompt; the app routes keys here.
#[derive(Default)]
pub struct Menu {
    pub rows: Vec<MenuRow>,
    pub visible_rows: Vec<MenuRow>,
    pub kind: Option<MenuKind>,
    pub title: String,
    pub subtitle: String,
    pub details: Vec<Line<'static>>,
    open: bool,
    highlighted: usize,
}

impl Menu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(
        &mut self,
        kind: MenuKind,
        title: &str,
        rows: Vec<MenuRow>,
        term: &str,
        details: Option<Vec<Line<'static>>>,
        subtitle: &str,
    ) {
        self.kind = Some(kind);
        self.title = title.to_string();
        self.subtitle = subtitle.to_string();
        self.rows = rows;
        self.details = details.unwrap_or_default();
        self.open = true;
        self.filter(term);
    }

    pub fn close(&mut self) {
        self.kind = None;
        self.open = false;
    }

    pub fn filter(&mut self, term: &str) -> bool {
        let term = term.trim().to_lowercase();
        let folded: Vec<String> = self.rows.iter().map(|r| r.label.to_lowercase()).collect();
        let starts: Vec<usize> = (0..self.rows.len())
            .filter(|&i| folded[i].starts_with(&term))
            .collect();
        let contains =
            (0..self.rows.len()).filter(|&i| folded[i].contains(&term) && !starts.contains(&i));
        let order: Vec<usize> = starts.iter().copied().chain(contains).collect();
        self.visible_rows = order.into_iter().map(|i| self.rows[i].clone()).collect();
        self.highlighted = 0;
        !self.visible_rows.is_empty()
    }

    pub fn move_by(&mut self, step: isize) {
        let n = self.visible_rows.len() as isize;
        if n > 0 {
            self.highlighted = (self.highlighted as isize + step).rem_euclid(n) as usize;
        }
    }

    pub fn highlighted(&self) -> Option<usize> {
        (self.highlighted < self.visible_rows.len()).then_some(self.highlighted)
    }

    pub fn select(&mut self, index: usize) {
        if index < self.visible_rows.len() {
            self.highlighted = index;
        }
    }

    pub fn current(&self) -> Option<&MenuRow> {
        self.visible_rows.get(self.highlighted)
    }

    fn row_spans(&self, index: usize) -> Vec<Span<'static>> {
        let label_width = self
            .visible_rows
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0);
        let described = self
            .visible_rows
            .iter()
            .map(|r| r.description.chars().count())
            .max()
            .unwrap_or(0);
        let row = &self.visible_rows[index];
        let chosen = index == self.highlighted;
        let bold = |color: Color| Style::default().fg(color).add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(if chosen { "› " } else { "  " }, bold(CORAL))];
        if self.kind == Some(MenuKind::Approve) {
            spans.push(Span::styled(
                format!("{}. ", index + 1),
                Style::default().fg(if chosen { ORANGE } else { DIM }),
            ));
        }
        let label = format!("{:<label_width$}", row.label);
        spans.push(Span::styled(
            label,
            if chosen {
                bold(ORANGE)
            } else {
                Style::default().fg(ORANGE)
            },
        ));
        if !row.description.is_empty() || !row.command.is_empty() {
            let text = format!("   {:<described$}", row.description);
            spans.push(Span::styled(
                text,
                Style::default().fg(if chosen { Color::White } else { DIM }),
            ));
        }
        if !row.command.is_empty() {
            let style = if chosen {
                bold(TEAL)
            } else {
                Style::default().fg(TEAL)
            };
            spans.push(Span::styled(format!("   {}", row.command), style));
        }
        spans
    }

    pub fn row_text(&self, index: usize) -> String {
        if index >= self.visible_rows.len() {
            return String::new();
        }
        self.row_spans(index)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn details_height(&self, width: u16) -> usize {
        if self.kind != Some(MenuKind::Approve) {
            return 0;
        }
        let inner = usize::from(width.saturating_sub(4)).max(1);
        let rows: usize = self
            .details
            .iter()
            .map(|l| {
                let w: usize = l
                    .spans
                    .iter()
                    .flat_map(|s| s.content.chars())
                    .map(cell_width)
                    .sum();
                w.div_ceil(inner).max(1)
            })
            .sum();
        (rows + 1).min(16)
    }

    pub fn height(&self, width: u16) -> u16 {
        if !self.open {
            return 0;
        }
        let options = if self.visible_rows.is_empty() {
            1
        } else {
            self.visible_rows.len().min(12)
        };
        let cap = if self.kind == Some(MenuKind::Approve) {
            26
        } else {
            14
        };
        (self.details_height(width) + options + 2).min(cap) as u16
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if !self.open || area.width < 3 || area.height < 3 {
            return;
        }
        let approve = self.kind == Some(MenuKind::Approve);
        let color = if approve { ORANGE } else { CORAL };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color).bg(BG))
            .style(Style::default().bg(BG));
        if !self.title.is_empty() {
            let style = Style::default().fg(ORANGE).add_modifier(Modifier::BOLD);
            block = block.title(Line::from(Span::styled(format!(" {} ", self.title), style)));
        }
        if !self.subtitle.is_empty() {
            block = block.title_bottom(Line::from(Span::styled(
                format!(" {} ", self.subtitle),
                Style::default().fg(DIM),
            )));
        }
        let inner = block.inner(area);
        block.render(area, buf);
        let inner = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(2),
            ..inner
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let mut y = inner.y;
        let shown = self
            .details_height(area.width)
            .min(usize::from(inner.height));
        if shown > 0 {
            let part = Rect {
                height: shown.saturating_sub(1) as u16,
                y,
                ..inner
            };
            Paragraph::new(self.details.clone())
                .wrap(Wrap { trim: false })
                .render(part, buf);
            y += shown as u16;
        }
        let room = usize::from(inner.bottom().saturating_sub(y)).min(12);
        if self.visible_rows.is_empty() {
            if room > 0 {
                buf.set_stringn(
                    inner.x,
                    y,
                    "No matches",
                    usize::from(inner.width),
                    Style::default().fg(DIM).bg(BG),
                );
            }
            return;
        }
        let offset = if room > 0 && self.highlighted >= room {
            self.highlighted + 1 - room
        } else {
            0
        };
        for (line, index) in (offset..self.visible_rows.len()).take(room).enumerate() {
            let row = Rect {
                x: inner.x,
                y: y + line as u16,
                width: inner.width,
                height: 1,
            };
            let style = if index == self.highlighted {
                Style::default().bg(HIGHLIGHT_BG)
            } else {
                Style::default().bg(BG)
            };
            fill(buf, row, style);
            let spans: Vec<Span> = self
                .row_spans(index)
                .into_iter()
                .map(|s| Span::styled(s.content, style.patch(s.style)))
                .collect();
            buf.set_line(row.x, row.y, &Line::from(spans), row.width);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Draft

struct VRow {
    row: usize,
    start: usize,
    end: usize,
}

fn segments(chars: &[char], width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut segs = Vec::new();
    let (mut start, mut x, mut i) = (0usize, 0usize, 0usize);
    let mut last_break: Option<usize> = None;
    while i < chars.len() {
        let w = cell_width(chars[i]);
        if x + w > width && i > start {
            let cut = match last_break {
                Some(b) if b > start => b,
                _ => i,
            };
            segs.push((start, cut));
            start = cut;
            x = chars[start..i].iter().map(|&c| cell_width(c)).sum();
            last_break = chars[start..i]
                .iter()
                .rposition(|&c| c == ' ')
                .map(|p| start + p + 1)
                .filter(|&b| b > start);
            continue;
        }
        x += w;
        if chars[i] == ' ' {
            last_break = Some(i + 1);
        }
        i += 1;
    }
    segs.push((start, chars.len()));
    segs
}

type Pos = (usize, usize);

/// The multi-line prompt editor.
pub struct Draft {
    pub placeholder: String,
    pub border_subtitle: String,
    /// The text starts with "!": the border turns teal. The app keeps this in step.
    pub shell: bool,
    lines: Vec<Vec<char>>,
    cursor: Pos,
    anchor: Option<Pos>,
    preferred: Option<usize>,
    scroll: std::cell::Cell<usize>,
}

impl Default for Draft {
    fn default() -> Self {
        Self::new()
    }
}

impl Draft {
    pub fn new() -> Self {
        Draft {
            placeholder: String::new(),
            border_subtitle: String::new(),
            shell: false,
            lines: vec![Vec::new()],
            cursor: (0, 0),
            anchor: None,
            preferred: None,
            scroll: std::cell::Cell::new(0),
        }
    }

    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn set_lines(text: &str) -> Vec<Vec<char>> {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        text.split('\n').map(|l| l.chars().collect()).collect()
    }

    pub fn load_text(&mut self, text: &str) {
        self.lines = Self::set_lines(text);
        self.cursor = (0, 0);
        self.anchor = None;
        self.preferred = None;
    }

    pub fn clear(&mut self) {
        self.load_text("");
    }

    fn range(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    fn delete_selection(&mut self) -> bool {
        let Some(((r0, c0), (r1, c1))) = self.range() else {
            self.anchor = None;
            return false;
        };
        if r0 == r1 {
            self.lines[r0].drain(c0..c1);
        } else {
            let tail: Vec<char> = self.lines[r1][c1..].to_vec();
            self.lines[r0].truncate(c0);
            self.lines[r0].extend(tail);
            self.lines.drain(r0 + 1..=r1);
        }
        self.cursor = (r0, c0);
        self.anchor = None;
        true
    }

    pub fn insert(&mut self, text: &str) {
        self.delete_selection();
        self.preferred = None;
        let parts = Self::set_lines(text);
        let (row, col) = self.cursor;
        let col = col.min(self.lines[row].len());
        if parts.len() == 1 {
            let n = parts[0].len();
            self.lines[row].splice(col..col, parts[0].iter().copied());
            self.cursor = (row, col + n);
            return;
        }
        let tail = self.lines[row].split_off(col);
        self.lines[row].extend(parts[0].iter());
        let last = parts.len() - 1;
        let mut at = row;
        for (i, part) in parts.iter().enumerate().skip(1) {
            at += 1;
            let mut line = part.clone();
            if i == last {
                self.cursor = (at, line.len());
                line.extend(tail.iter());
            }
            self.lines.insert(at, line);
        }
    }

    pub fn cursor_location(&self) -> Pos {
        self.cursor
    }

    pub fn move_cursor(&mut self, loc: Pos) {
        let row = loc.0.min(self.lines.len() - 1);
        self.cursor = (row, loc.1.min(self.lines[row].len()));
        self.preferred = None;
    }

    pub fn move_to_end(&mut self) {
        let row = self.lines.len() - 1;
        self.cursor = (row, self.lines[row].len());
        self.anchor = None;
        self.preferred = None;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((r0, c0), (r1, c1)) = self.range()?;
        if r0 == r1 {
            return Some(self.lines[r0][c0..c1].iter().collect());
        }
        let mut out = vec![self.lines[r0][c0..].iter().collect::<String>()];
        for r in r0 + 1..r1 {
            out.push(self.lines[r].iter().collect());
        }
        out.push(self.lines[r1][..c1].iter().collect());
        Some(out.join("\n"))
    }

    pub fn selection_is_empty(&self) -> bool {
        self.range().is_none()
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    fn word_left(&mut self) {
        let (row, mut col) = self.cursor;
        if col == 0 {
            if row > 0 {
                self.cursor = (row - 1, self.lines[row - 1].len());
            }
            return;
        }
        let line = &self.lines[row];
        while col > 0 && line[col - 1].is_whitespace() {
            col -= 1;
        }
        while col > 0 && !line[col - 1].is_whitespace() {
            col -= 1;
        }
        self.cursor = (row, col);
    }

    fn word_right(&mut self) {
        let (row, mut col) = self.cursor;
        let line = &self.lines[row];
        if col >= line.len() {
            if row + 1 < self.lines.len() {
                self.cursor = (row + 1, 0);
            }
            return;
        }
        while col < line.len() && line[col].is_whitespace() {
            col += 1;
        }
        while col < line.len() && !line[col].is_whitespace() {
            col += 1;
        }
        self.cursor = (row, col);
    }

    fn step(&mut self, forward: bool) {
        let (row, col) = self.cursor;
        if forward {
            if col < self.lines[row].len() {
                self.cursor = (row, col + 1);
            } else if row + 1 < self.lines.len() {
                self.cursor = (row + 1, 0);
            }
        } else if col > 0 {
            self.cursor = (row, col - 1);
        } else if row > 0 {
            self.cursor = (row - 1, self.lines[row - 1].len());
        }
    }

    fn vertical(&mut self, down: bool) {
        let (row, col) = self.cursor;
        let want = *self.preferred.get_or_insert(col);
        if down {
            if row + 1 < self.lines.len() {
                self.cursor = (row + 1, want.min(self.lines[row + 1].len()));
            } else {
                self.cursor = (row, self.lines[row].len());
            }
        } else if row > 0 {
            self.cursor = (row - 1, want.min(self.lines[row - 1].len()));
        } else {
            self.cursor = (0, 0);
        }
    }

    fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (row, col) = self.cursor;
        if col > 0 {
            self.lines[row].remove(col - 1);
            self.cursor = (row, col - 1);
        } else if row > 0 {
            let line = self.lines.remove(row);
            let at = self.lines[row - 1].len();
            self.lines[row - 1].extend(line);
            self.cursor = (row - 1, at);
        }
    }

    fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (row, col) = self.cursor;
        if col < self.lines[row].len() {
            self.lines[row].remove(col);
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].extend(next);
        }
    }

    pub fn edit_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let word = ctrl || alt;
        let (row, col) = self.cursor;
        // Cursor movement keys: start or end a selection around the move.
        let moving = matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
        ) || (ctrl && matches!(key.code, KeyCode::Char('a' | 'e')));
        if moving {
            if shift && self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            } else if !shift {
                self.anchor = None;
            }
        }
        let vertical = matches!(key.code, KeyCode::Up | KeyCode::Down);
        match key.code {
            KeyCode::Left if word => self.word_left(),
            KeyCode::Right if word => self.word_right(),
            KeyCode::Left => self.step(false),
            KeyCode::Right => self.step(true),
            KeyCode::Up => self.vertical(false),
            KeyCode::Down => self.vertical(true),
            KeyCode::Home => self.cursor = (row, 0),
            KeyCode::End => self.cursor = (row, self.lines[row].len()),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Tab => self.insert("\t"),
            KeyCode::Char(c) if ctrl => match c {
                'a' => self.cursor = (row, 0),
                'e' => self.cursor = (row, self.lines[row].len()),
                'k' => {
                    if self.delete_selection() {
                    } else if col < self.lines[row].len() {
                        self.lines[row].truncate(col);
                    } else {
                        self.delete_forward();
                    }
                }
                'u' => {
                    if !self.delete_selection() {
                        self.lines[row].drain(..col);
                        self.cursor = (row, 0);
                    }
                }
                'w' => {
                    if !self.delete_selection() {
                        let end = self.cursor;
                        self.word_left();
                        if self.cursor.0 == end.0 {
                            self.lines[row].drain(self.cursor.1..end.1);
                        } else {
                            self.cursor = end;
                            self.backspace();
                        }
                    }
                }
                _ => return false,
            },
            KeyCode::Char(c) if alt => match c {
                'b' => self.word_left(),
                'f' => self.word_right(),
                _ => return false,
            },
            KeyCode::Char(c) => self.insert(&c.to_string()),
            _ => return false,
        }
        if !vertical {
            self.preferred = None;
        }
        true
    }

    fn layout(&self, width: usize) -> Vec<VRow> {
        let mut rows = Vec::new();
        for (row, chars) in self.lines.iter().enumerate() {
            for (start, end) in segments(chars, width) {
                rows.push(VRow { row, start, end });
            }
        }
        rows
    }

    fn visual_of(&self, rows: &[VRow], pos: Pos) -> (usize, usize) {
        let (row, col) = pos;
        let mut found = 0;
        for (i, v) in rows.iter().enumerate() {
            if v.row == row
                && (v.start <= col
                    && (col < v.end || i + 1 == rows.len() || rows[i + 1].row != row))
            {
                found = i;
                break;
            }
        }
        let v = &rows[found];
        let x = self.lines[row][v.start..col.min(self.lines[row].len()).max(v.start)]
            .iter()
            .map(|&c| cell_width(c))
            .sum();
        (found, x)
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        let rows = self
            .layout(usize::from(width.saturating_sub(2)))
            .len()
            .max(1);
        (rows + 2).clamp(3, 10) as u16
    }

    fn scroll_for(&self, rows: &[VRow], height: usize) -> usize {
        let (cursor_row, _) = self.visual_of(rows, self.cursor);
        let mut top = self.scroll.get();
        if cursor_row < top {
            top = cursor_row;
        }
        if height > 0 && cursor_row >= top + height {
            top = cursor_row + 1 - height;
        }
        top = top.min(rows.len().saturating_sub(height));
        self.scroll.set(top);
        top
    }

    pub fn on_mouse(&mut self, ev: MouseEvent, area: Rect) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        let width = usize::from(area.width - 2);
        let height = usize::from(area.height - 2);
        let rows = self.layout(width);
        let scroll = self.scroll_for(&rows, height);
        let position = |draft: &Draft| -> Pos {
            let y = usize::from(ev.row.saturating_sub(area.y + 1)) + scroll;
            let x = usize::from(ev.column.saturating_sub(area.x + 1));
            let v = &rows[y.min(rows.len() - 1)];
            let chars = &draft.lines[v.row];
            let mut used = 0;
            let mut col = v.start;
            while col < v.end {
                let w = cell_width(chars[col]);
                if used + w > x {
                    break;
                }
                used += w;
                col += 1;
            }
            (v.row, col)
        };
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.cursor = position(self);
                self.anchor = Some(self.cursor);
                self.preferred = None;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.cursor = position(self);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.cursor = position(self);
                if self.anchor == Some(self.cursor) {
                    self.anchor = None;
                }
            }
            _ => {}
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, focused: bool) -> Option<(u16, u16)> {
        if area.width < 3 || area.height < 3 {
            return None;
        }
        let color = if self.shell {
            TEAL
        } else if focused {
            ORANGE
        } else {
            mix(CORAL, BG, 0.6)
        };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color).bg(BG))
            .style(Style::default().bg(BG));
        if !self.border_subtitle.is_empty() {
            block = block.title_bottom(
                Line::from(Span::styled(
                    self.border_subtitle.clone(),
                    Style::default().fg(GRAY),
                ))
                .right_aligned(),
            );
        }
        let inner = block.inner(area);
        block.render(area, buf);
        let width = usize::from(inner.width);
        let height = usize::from(inner.height);
        let rows = self.layout(width);
        if self.text().is_empty() {
            buf.set_stringn(
                inner.x,
                inner.y,
                &self.placeholder,
                width,
                Style::default().fg(DIM).bg(BG),
            );
            return focused.then_some((inner.x, inner.y));
        }
        let scroll = self.scroll_for(&rows, height);
        let range = self.range();
        for (i, v) in rows.iter().enumerate().skip(scroll).take(height) {
            let y = inner.y + (i - scroll) as u16;
            let mut x = 0usize;
            for col in v.start..v.end {
                let ch = self.lines[v.row][col];
                let w = cell_width(ch);
                let mut style = Style::default().bg(BG);
                if let Some((a, b)) = range {
                    if (v.row, col) >= a && (v.row, col) < b {
                        style = style.bg(SELECTION_BG);
                    }
                }
                let shown = if ch == '\t' {
                    "    ".to_string()
                } else {
                    ch.to_string()
                };
                if x + w <= width {
                    buf.set_stringn(inner.x + x as u16, y, shown, w, style);
                }
                x += w;
            }
        }
        let (vrow, x) = self.visual_of(&rows, self.cursor);
        if !focused || vrow < scroll || vrow - scroll >= height {
            return None;
        }
        Some((
            inner.x + x.min(width.saturating_sub(1)) as u16,
            inner.y + (vrow - scroll) as u16,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use serde_json::json;

    fn rich(text: &str) -> RichLine {
        RichLine::from(Line::from(text.to_string()))
    }

    fn linked(text: &str, start: usize, end: usize, url: &str) -> RichLine {
        RichLine {
            line: Line::from(text.to_string()),
            links: vec![super::super::Link {
                start,
                end,
                url: url.to_string(),
            }],
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn type_text(draft: &mut Draft, text: &str) {
        for c in text.chars() {
            draft.edit_key(key(KeyCode::Char(c)));
        }
    }

    fn row_string(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect::<String>()
    }

    #[test]
    fn transcript_wraps_at_words_and_breaks_long_words() {
        let mut t = Transcript::new();
        t.push(rich("hello brave new world"));
        t.push(rich("abcdefghij"));
        assert_eq!(
            t.wrapped_plain(11),
            ["hello brave", "new world", "abcdefghij"]
        );
        assert_eq!(
            t.wrapped_plain(4),
            [
                "hell", "o", "brav", "e", "new", "worl", "d", "abcd", "efgh", "ij"
            ]
        );
        assert_eq!(t.plain(), ["hello brave new world", "abcdefghij"]);
        assert_eq!(t.body(), "hello brave new world\nabcdefghij");
        t.push(rich(""));
        assert_eq!(t.wrapped_plain(5).last().map(String::as_str), Some(""));
    }

    #[test]
    fn transcript_follow_semantics() {
        let mut t = Transcript::new();
        let area = Rect::new(0, 0, 100, 5);
        t.set_viewport(97, 5);
        for i in 0..20 {
            t.push(rich(&format!("line {i}")));
        }
        assert_eq!(t.scroll_y(), t.max_scroll_y());
        assert_eq!(t.max_scroll_y(), 15);
        t.scroll_by(-10);
        assert_eq!(t.scroll_y(), 5);
        t.on_mouse(mouse(MouseEventKind::ScrollUp, 3, 2), area);
        assert!(!t.follow());
        let where_ = t.scroll_y();
        t.push(rich("a new line"));
        assert_eq!(t.scroll_y(), where_);
        t.scroll_end();
        assert!(t.follow());
        t.push(rich("another"));
        assert_eq!(t.scroll_y(), t.max_scroll_y());
        t.on_mouse(mouse(MouseEventKind::ScrollUp, 3, 2), area);
        assert!(!t.follow());
        t.scroll_by(1000);
        assert!(t.follow());
    }

    #[test]
    fn shrinking_the_viewport_while_following_stays_at_the_end() {
        let mut t = Transcript::new();
        t.set_viewport(50, 10);
        for i in 0..30 {
            t.push(rich(&format!("{i}")));
        }
        t.set_viewport(50, 6);
        assert_eq!(t.scroll_y(), t.max_scroll_y());
        assert_eq!(t.max_scroll_y(), 24);
        t.clear();
        assert_eq!((t.len(), t.scroll_y()), (0, 0));
        assert!(t.follow());
    }

    #[test]
    fn dragging_selects_text_and_release_reports_it() {
        let mut t = Transcript::new();
        let area = Rect::new(0, 0, 100, 10);
        t.push(rich("Output line one"));
        t.push(rich("PR https://github.com/x/y/pull/9"));
        let mut buf = Buffer::empty(area);
        t.render(area, &mut buf);
        assert_eq!(
            t.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0), area),
            TranscriptEvent::None
        );
        t.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 7, 1), area);
        let done = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 7, 1), area);
        assert_eq!(
            done,
            TranscriptEvent::Selected("Output line one\nPR http".into())
        );
        assert_eq!(
            t.selected_text().as_deref(),
            Some("Output line one\nPR http")
        );
        t.render(area, &mut buf);
        assert_eq!(buf[(1, 0)].bg, SELECTION_BG);
        assert_eq!(buf[(8, 1)].bg, BG);
        t.clear_selection();
        assert_eq!(t.selected_text(), None);
        // Dragging backwards selects the same way.
        t.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 7, 1), area);
        let back = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0), area);
        assert_eq!(
            back,
            TranscriptEvent::Selected("Output line one\nPR http".into())
        );
    }

    #[test]
    fn clicking_a_link_opens_it_and_other_clicks_do_not() {
        let mut t = Transcript::new();
        let area = Rect::new(0, 0, 100, 10);
        let url = "https://github.com/example/project/pull/42";
        t.push(linked(&format!("PR: {url}"), 4, 4 + url.len(), url));
        t.push(linked(
            "Merged see PR today",
            11,
            13,
            "https://github.com/x/y/pull/2",
        ));
        let click = |t: &mut Transcript, x: u16, y: u16| {
            t.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y), area);
            t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, y), area)
        };
        assert_eq!(click(&mut t, 12, 0), TranscriptEvent::Link(url.into()));
        assert_eq!(click(&mut t, 2, 0), TranscriptEvent::None);
        assert_eq!(
            click(&mut t, 13, 1),
            TranscriptEvent::Link("https://github.com/x/y/pull/2".into())
        );
        assert_eq!(t.link_at(area, 2, 0), None);
        assert_eq!(t.selected_text(), None);
    }

    #[test]
    fn link_cells_survive_wrapping() {
        let mut t = Transcript::new();
        let area = Rect::new(0, 0, 14, 10); // content width 11
        t.push(linked(
            "see https://example.com/page",
            4,
            28,
            "https://example.com/page",
        ));
        let mut buf = Buffer::empty(area);
        t.render(area, &mut buf);
        assert!(t.rows.len() > 1);
        assert_eq!(
            t.link_at(area, 1, 1).as_deref(),
            Some("https://example.com/page")
        );
        assert_eq!(t.link_at(area, 1, 0), None);
    }

    #[test]
    fn transcript_renders_text_selection_and_scrollbar() {
        let mut t = Transcript::new();
        let area = Rect::new(0, 0, 20, 3);
        for i in 0..10 {
            t.push(RichLine::from(Line::styled(
                format!("row {i}"),
                Style::default().fg(ORANGE),
            )));
        }
        let mut buf = Buffer::empty(area);
        t.render(area, &mut buf);
        assert!(row_string(&buf, 0).starts_with(" row 7"));
        assert_eq!(buf[(1, 0)].fg, ORANGE);
        assert_eq!(buf[(1, 0)].bg, BG);
        assert_ne!(buf[(19, 2)].bg, BG);
        t.render(
            Rect::new(0, 0, 0, 0),
            &mut Buffer::empty(Rect::new(0, 0, 0, 0)),
        );
        t.render(
            Rect::new(0, 0, 1, 1),
            &mut Buffer::empty(Rect::new(0, 0, 1, 1)),
        );
    }

    #[test]
    fn menu_filter_puts_prefix_matches_first_and_wraps_the_highlight() {
        let mut menu = Menu::new();
        let rows = [
            "/plan",
            "/model",
            "/clear",
            "/context",
            "/copy",
            "/reviewer",
        ]
        .iter()
        .map(|l| MenuRow::new(*l, json!(l)))
        .collect();
        menu.open(MenuKind::Slash, "Commands", rows, "", None, "");
        assert!(menu.is_open());
        assert_eq!(menu.visible_rows.len(), 6);
        assert!(menu.filter("/c"));
        let labels: Vec<_> = menu.visible_rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["/clear", "/context", "/copy"]);
        assert!(menu.filter("e"));
        let labels: Vec<_> = menu.visible_rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["/model", "/clear", "/context", "/reviewer"]);
        assert!(!menu.filter("zz"));
        assert_eq!(menu.current(), None);
        menu.filter("/c");
        menu.move_by(-1);
        assert_eq!(menu.highlighted(), Some(2));
        menu.move_by(1);
        assert_eq!(menu.highlighted(), Some(0));
        menu.select(1);
        assert_eq!(menu.current().map(|r| r.label.as_str()), Some("/context"));
        menu.close();
        assert!(!menu.is_open());
        assert_eq!(menu.kind, None);
        assert_eq!(menu.height(80), 0);
    }

    #[test]
    fn menu_row_text_ends_with_the_command() {
        let mut menu = Menu::new();
        let rows = vec![
            MenuRow::new("Next ticket", json!("next 1"))
                .described("one ticket in a fresh context")
                .command("/work next 1"),
            MenuRow::new("Next 3 tickets", json!("next 3"))
                .described("up to three")
                .command("/work next 3"),
        ];
        menu.open(
            MenuKind::Pick,
            "Work tickets",
            rows,
            "",
            None,
            "or type /work next N for any number",
        );
        let text = menu.row_text(1);
        assert!(text.contains("Next 3 tickets"));
        assert!(text.ends_with("/work next 3"));
        assert!(menu.row_text(0).starts_with("› Next ticket     "));
        assert!(text.starts_with("  Next 3 tickets"));
        assert!(menu.subtitle.contains("/work next N"));
        let mut approve = Menu::new();
        approve.open(
            MenuKind::Approve,
            "Bash command",
            vec![
                MenuRow::new("Yes", json!("yes")),
                MenuRow::new("No", json!("no")),
            ],
            "",
            Some(vec![
                Line::from("$ ls -la"),
                Line::from("Do you want to proceed?"),
            ]),
            "Enter to choose",
        );
        assert_eq!(approve.row_text(1), "  2. No ");
        assert!(approve.row_text(0).starts_with("› 1. Yes"));
    }

    #[test]
    fn menu_height_is_bounded() {
        let mut menu = Menu::new();
        let rows: Vec<_> = (0..40)
            .map(|i| MenuRow::new(format!("row {i}"), json!(i)))
            .collect();
        menu.open(MenuKind::Pick, "Many", rows.clone(), "", None, "");
        assert_eq!(menu.height(80), 14);
        menu.open(MenuKind::Pick, "Few", rows[..3].to_vec(), "", None, "");
        assert_eq!(menu.height(80), 5);
        menu.filter("nothing");
        assert_eq!(menu.height(80), 3);
        let long: Vec<Line> = (0..30).map(|i| Line::from(format!("line {i}"))).collect();
        menu.open(MenuKind::Approve, "Write", rows, "", Some(long), "");
        assert_eq!(menu.height(80), 26);
    }

    #[test]
    fn menu_renders_border_title_rows_and_highlight() {
        let mut menu = Menu::new();
        let rows = vec![
            MenuRow::new("/plan", json!("/plan")).described("Plan mode"),
            MenuRow::new("/work", json!("/work")),
        ];
        menu.open(MenuKind::Slash, "Commands", rows, "", None, "hint");
        menu.move_by(1);
        let area = Rect::new(0, 0, 40, menu.height(40));
        let mut buf = Buffer::empty(area);
        menu.render(area, &mut buf);
        assert!(row_string(&buf, 0).contains("Commands"));
        assert!(row_string(&buf, 1).contains("/plan"));
        assert!(row_string(&buf, 1).contains("Plan mode"));
        assert!(row_string(&buf, 2).contains("› /work"));
        assert!(row_string(&buf, 3).contains("hint"));
        assert_eq!(buf[(5, 2)].bg, HIGHLIGHT_BG);
        assert_eq!(buf[(5, 1)].bg, BG);
        assert_eq!(buf[(0, 0)].symbol(), "╭");
        menu.filter("zzz");
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 3));
        menu.render(Rect::new(0, 0, 40, 3), &mut buf);
        assert!(row_string(&buf, 1).contains("No matches"));
        menu.render(
            Rect::new(0, 0, 1, 1),
            &mut Buffer::empty(Rect::new(0, 0, 1, 1)),
        );
    }

    #[test]
    fn draft_edits_text_and_newlines() {
        let mut d = Draft::new();
        type_text(&mut d, "first");
        d.insert("\nsecond\n");
        d.insert("third");
        assert_eq!(d.text(), "first\nsecond\nthird");
        assert_eq!(d.line_count(), 3);
        assert_eq!(d.cursor_location(), (2, 5));
        assert!(!d.edit_key(key(KeyCode::Enter)));
        d.edit_key(key(KeyCode::Backspace));
        assert_eq!(d.text(), "first\nsecond\nthir");
        d.move_cursor((1, 0));
        d.edit_key(key(KeyCode::Backspace));
        assert_eq!(d.text(), "firstsecond\nthir");
        assert_eq!(d.cursor_location(), (0, 5));
        d.edit_key(key(KeyCode::Delete));
        assert_eq!(d.text(), "firstecond\nthir");
        d.load_text("a\nb");
        assert_eq!(d.cursor_location(), (0, 0));
        d.move_to_end();
        assert_eq!(d.cursor_location(), (1, 1));
        d.clear();
        assert_eq!(d.text(), "");
        d.insert("a\r\nb");
        assert_eq!(d.text(), "a\nb");
        assert!(d.edit_key(key(KeyCode::Tab)));
        assert_eq!(d.text(), "a\nb\t");
    }

    #[test]
    fn draft_cursor_keys_and_line_editing() {
        let mut d = Draft::new();
        d.load_text("hello world\nxy\nlonger line");
        d.move_cursor((0, 8));
        d.edit_key(key(KeyCode::Down));
        assert_eq!(d.cursor_location(), (1, 2));
        d.edit_key(key(KeyCode::Down));
        assert_eq!(d.cursor_location(), (2, 8));
        d.edit_key(key(KeyCode::Down));
        assert_eq!(d.cursor_location(), (2, 11));
        d.edit_key(key(KeyCode::Up));
        d.edit_key(key(KeyCode::Up));
        d.edit_key(key(KeyCode::Up));
        assert_eq!(d.cursor_location(), (0, 0));
        d.edit_key(key(KeyCode::End));
        assert_eq!(d.cursor_location(), (0, 11));
        d.edit_key(key(KeyCode::Right));
        assert_eq!(d.cursor_location(), (1, 0));
        d.edit_key(key(KeyCode::Left));
        assert_eq!(d.cursor_location(), (0, 11));
        d.edit_key(with(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(d.cursor_location(), (0, 6));
        d.edit_key(with(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(d.text(), "hello \nxy\nlonger line");
        d.edit_key(with(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(d.text(), "\nxy\nlonger line");
        d.move_cursor((2, 6));
        d.edit_key(with(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(d.text(), "\nxy\n line");
        d.edit_key(with(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(d.cursor_location(), (2, 5));
        d.edit_key(with(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(d.cursor_location(), (2, 0));
        assert!(!d.edit_key(with(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn draft_selection_extends_copies_and_is_replaced_by_typing() {
        let mut d = Draft::new();
        d.load_text("hello world\nsecond");
        d.move_cursor((0, 6));
        for _ in 0..5 {
            d.edit_key(with(KeyCode::Right, KeyModifiers::SHIFT));
        }
        assert_eq!(d.selected_text().as_deref(), Some("world"));
        assert!(!d.selection_is_empty());
        d.edit_key(key(KeyCode::Char('X')));
        assert_eq!(d.text(), "hello X\nsecond");
        assert!(d.selection_is_empty());
        d.move_cursor((0, 3));
        d.edit_key(with(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(d.selected_text().as_deref(), Some("lo X\nsec"));
        d.insert("-");
        assert_eq!(d.text(), "hel-ond");
        d.edit_key(with(KeyCode::Home, KeyModifiers::SHIFT));
        d.edit_key(key(KeyCode::Backspace));
        assert_eq!(d.text(), "ond");
        d.edit_key(with(KeyCode::End, KeyModifiers::SHIFT));
        assert_eq!(d.selected_text().as_deref(), Some("ond"));
        d.clear_selection();
        assert_eq!(d.selected_text(), None);
    }

    #[test]
    fn draft_height_wraps_and_is_bounded() {
        let mut d = Draft::new();
        assert_eq!(d.desired_height(40), 3);
        d.load_text(&"word ".repeat(30));
        assert!(d.desired_height(22) > 3);
        d.load_text(&"x\n".repeat(30));
        assert_eq!(d.desired_height(40), 10);
        assert_eq!(d.desired_height(0), 10);
    }

    #[test]
    fn draft_renders_placeholder_text_subtitle_and_cursor() {
        let mut d = Draft::new();
        d.placeholder = "Ask anything".into();
        d.border_subtitle = "$0.00 spent".into();
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        let cursor = d.render(area, &mut buf, true);
        assert_eq!(cursor, Some((1, 1)));
        assert!(row_string(&buf, 1).contains("Ask anything"));
        assert!(row_string(&buf, 2).contains("$0.00 spent"));
        assert!(row_string(&buf, 2).trim_end().ends_with("╯"));
        assert_eq!(buf[(0, 0)].fg, ORANGE);
        let mut buf = Buffer::empty(area);
        assert_eq!(d.render(area, &mut buf, false), None);
        assert_ne!(buf[(0, 0)].fg, ORANGE);
        d.shell = true;
        let mut buf = Buffer::empty(area);
        d.render(area, &mut buf, true);
        assert_eq!(buf[(0, 0)].fg, TEAL);
        d.shell = false;
        d.load_text("one\ntwo");
        d.move_to_end();
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 4));
        let cursor = d.render(Rect::new(0, 0, 40, 4), &mut buf, true);
        assert_eq!(cursor, Some((4, 2)));
        assert!(row_string(&buf, 1).contains("one"));
        assert!(row_string(&buf, 2).contains("two"));
        d.render(
            Rect::new(0, 0, 2, 2),
            &mut Buffer::empty(Rect::new(0, 0, 2, 2)),
            true,
        );
    }

    #[test]
    fn draft_scrolls_to_keep_the_cursor_visible_and_clicks_place_it() {
        let mut d = Draft::new();
        d.load_text(
            &(0..20)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        d.move_to_end();
        let area = Rect::new(0, 0, 30, 5);
        let mut buf = Buffer::empty(area);
        let cursor = d.render(area, &mut buf, true);
        assert_eq!(cursor, Some((8, 3)));
        assert!(row_string(&buf, 3).contains("line 19"));
        d.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1), area);
        assert_eq!(d.cursor_location(), (17, 2));
        d.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 6, 2), area);
        assert_eq!(d.selected_text().as_deref(), Some("ne 17\nline "));
        d.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 6, 2), area);
        d.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1), area);
        d.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, 1), area);
        assert!(d.selection_is_empty());
    }
}
