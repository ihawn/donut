//! Drawing: the layout of Donut's screen, top to bottom, as `Donut.compose` stacked it.

use std::time::Instant;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::app::{State, bold, fg};
use super::flows::App;
use super::render::{DIM, ORANGE};
use super::widgets::BG;

const FOOTER: [(&str, &str); 7] = [
    ("enter", "Send"),
    ("^c", "Cancel"),
    ("^q", "Quit"),
    ("^l", "Clear view"),
    ("f2", "Models"),
    ("f3", "Mode"),
    ("shift+tab", "Permissions"),
];

fn paragraph(lines: Vec<Line<'static>>, area: Rect, buf: &mut Buffer, inset: u16) {
    if area.width <= inset * 2 || area.height == 0 {
        return;
    }
    let inner = Rect {
        x: area.x + inset,
        y: area.y,
        width: area.width - inset * 2,
        height: area.height,
    };
    Paragraph::new(lines)
        .style(Style::default().bg(BG))
        .render(inner, buf);
}

fn footer_line(simple: bool) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, description) in FOOTER {
        if simple && description == "Mode" {
            continue; // Simple mode has one mode, so the mode key is neither shown nor active.
        }
        spans.push(Span::styled(format!(" {key} "), bold(ORANGE)));
        spans.push(Span::styled(format!("{description} "), fg(DIM)));
    }
    Line::from(spans)
}

impl App {
    /// Draw the whole interface into the frame.
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let cursor = {
            let mut s = self.st();
            draw_state(&mut s, area, frame.buffer_mut())
        };
        if let Some((x, y)) =
            cursor.filter(|_| !self.0.suspended.load(std::sync::atomic::Ordering::SeqCst))
        {
            frame.set_cursor_position((x, y));
        }
    }
}

/// Lay the widgets out in `area` and draw them. Returns where the cursor goes.
pub fn draw_state(s: &mut State, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)]
                .set_symbol(" ")
                .set_style(Style::default().bg(BG));
        }
    }
    s.notices.retain(|(_, until)| *until > Instant::now());
    let focus = s.focus_view;
    let width = area.width;
    let inner = width.saturating_sub(4) as usize;
    let usage = if focus {
        Vec::new()
    } else {
        s.usage_lines(inner).into_iter().take(2).collect::<Vec<_>>()
    };
    let status = if focus {
        Vec::new()
    } else {
        s.status_lines(inner)
            .into_iter()
            .take(3)
            .collect::<Vec<_>>()
    };
    let band = s.band_line();
    let thinking = s.thinking_line();
    let background: Vec<Line<'static>> = s.background_lines().into_iter().take(5).collect();
    let footer = if focus {
        None
    } else {
        Some(footer_line(s.simple()))
    };
    let draft_h = s.draft.desired_height(width.saturating_sub(2));
    let menu_h = s.menu.height(width.saturating_sub(2));
    let response_h = (s
        .response
        .wrapped_plain(width.saturating_sub(3) as usize)
        .len() as u16)
        .min(12);

    let fixed = usage.len() as u16
        + status.len() as u16
        + u16::from(band.is_some())
        + u16::from(thinking.is_some())
        + background.len() as u16
        + u16::from(footer.is_some())
        + draft_h
        + menu_h
        + response_h;
    let transcript_h = area.height.saturating_sub(fixed).max(1);
    let mut y = area.y;
    let mut take = |height: u16| -> Rect {
        let height = height.min(area.bottom().saturating_sub(y));
        let rect = Rect {
            x: area.x,
            y,
            width,
            height,
        };
        y += height;
        rect
    };
    let transcript_area = take(transcript_h);
    let response_area = take(response_h);
    let thinking_area = take(u16::from(thinking.is_some()));
    let background_area = take(background.len() as u16);
    let menu_area = take(menu_h);
    let draft_area = take(draft_h);
    let status_area = take(status.len() as u16);
    let band_area = take(u16::from(band.is_some()));
    let usage_area = take(usage.len() as u16);
    let footer_area = take(u16::from(footer.is_some()));

    s.areas.transcript = transcript_area;
    s.areas.draft = Rect {
        x: draft_area.x + 1,
        y: draft_area.y,
        width: draft_area.width.saturating_sub(2),
        height: draft_area.height,
    };
    s.transcript.render(transcript_area, buf);
    if response_h > 0 {
        s.response.render(response_area, buf);
    }
    if let Some(line) = thinking {
        paragraph(vec![line], thinking_area, buf, 2);
    }
    paragraph(background, background_area, buf, 2);
    if menu_h > 0 {
        let rect = Rect {
            x: menu_area.x + 1,
            y: menu_area.y,
            width: menu_area.width.saturating_sub(2),
            height: menu_area.height,
        };
        s.menu.render(rect, buf);
    }
    let draft_rect = s.areas.draft;
    let cursor = s.draft.render(draft_rect, buf, true);
    paragraph(status, status_area, buf, 2);
    if let Some(line) = band {
        paragraph(vec![line], band_area, buf, 2);
    }
    paragraph(usage, usage_area, buf, 2);
    if let Some(line) = footer {
        paragraph(vec![line], footer_area, buf, 0);
    }
    if let Some((text, _)) = s.notices.last() {
        let text = format!(" {text} ");
        let len = text.chars().count() as u16;
        if len < width {
            let rect = Rect {
                x: area.right() - len - 1,
                y: area.y,
                width: len,
                height: 1,
            };
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(BG).bg(ORANGE),
            )))
            .render(rect, buf);
        }
    }
    cursor
}
