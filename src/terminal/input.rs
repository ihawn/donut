//! Keys, mouse and paste: what the terminal events do to the app (`Draft._on_key` and the app's bindings).

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use serde_json::Value;

use super::app::State;
use super::flows::App;
use super::render;
use super::widgets::{MenuKind, MenuRow, TranscriptEvent};
use crate::plugins;

impl App {
    /// Everything a terminal can tell the app.
    pub fn handle_event(&self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => self.handle_paste(&text),
            _ => {}
        }
    }

    /// The command rows of the slash menu, with what a live Claude session and the installed plugins add.
    pub fn slash_rows(&self) -> Vec<MenuRow> {
        let (alias, profile, purpose, project, session) = {
            let s = self.st();
            let role = s.role();
            let alias = s.sel(&role).unwrap_or_default();
            let profile = s.prof(&role);
            let purpose = s.purpose();
            let key = State::session_key(&alias, &profile, purpose.as_deref());
            let session = s
                .rec_by_key(&key)
                .filter(|rec| rec.session.family() == "claude")
                .map(|rec| rec.session.clone());
            (alias, profile, purpose, s.project.clone(), session)
        };
        let _ = (alias, purpose);
        let commands: Vec<Value> = session
            .map(|session| {
                session
                    .info()
                    .get("commands")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let plugin_commands = plugins::commands(&profile, Some(&project));
        self.st().slash_rows(&commands, &plugin_commands)
    }

    /// What a change of the prompt's text does to the menu.
    pub fn after_edit(&self) {
        let want_rows = {
            let s = self.st();
            let text = s.draft.text();
            text.starts_with('/')
                && !text.chars().any(char::is_whitespace)
                && !s.recalled
                && !s.menu.is_open()
                && !matches!(s.menu.kind, Some(MenuKind::Pick | MenuKind::Approve))
        };
        let rows = if want_rows {
            self.slash_rows()
        } else {
            Vec::new()
        };
        self.st().on_text_changed(&|| rows.clone());
    }

    /// Insert text into the prompt as typing would.
    pub fn type_text(&self, text: &str) {
        self.st().draft.insert(text);
        self.after_edit();
    }

    pub fn handle_key(&self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Priority bindings: the editor would otherwise use ctrl+c (copy) and shift+tab itself.
        if ctrl && key.code == KeyCode::Char('c') {
            self.action_cancel();
            return;
        }
        if key.code == KeyCode::BackTab || (shift && key.code == KeyCode::Tab) {
            self.cycle_permission();
            return;
        }
        match key.code {
            KeyCode::Char('q') if ctrl => {
                self.spawn(|app| app.action_quit());
                return;
            }
            KeyCode::Char('l') if ctrl => {
                self.st().transcript.clear();
                return;
            }
            KeyCode::F(2) => {
                if !self.busy() {
                    self.execute("/model");
                }
                return;
            }
            KeyCode::F(3) => {
                let next = {
                    let s = self.st();
                    if s.busy || s.simple() {
                        None
                    } else {
                        Some(match s.mode.as_str() {
                            "plan" => "work",
                            "work" => "review",
                            "review" => "solo",
                            _ => "plan",
                        })
                    }
                };
                if let Some(next) = next {
                    self.execute(&format!("/{next}"));
                }
                return;
            }
            _ => {}
        }
        // The prompt has the focus: the menu gets the keys it uses first.
        let (menu_open, kind, visible) = {
            let s = self.st();
            (s.menu.is_open(), s.menu.kind, s.menu.visible_rows.len())
        };
        // Enter sends (Ctrl+Enter too, in terminals that tell it apart); Shift+Enter and Alt+Enter make a newline.
        let is_send = key.code == KeyCode::Enter && !shift && !alt;
        let menu_key = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Esc
        ) || is_send;
        if menu_open && menu_key {
            match key.code {
                KeyCode::Up => self.st().menu.move_by(-1),
                KeyCode::Down => self.st().menu.move_by(1),
                KeyCode::Esc => self.chosen(None, "escape"),
                KeyCode::Tab => {
                    let row = self.st().menu.current().cloned();
                    self.chosen(row, "tab");
                }
                _ => {
                    let row = self.st().menu.current().cloned();
                    self.chosen(row, "enter");
                }
            }
            return;
        }
        if kind == Some(MenuKind::Approve)
            && let KeyCode::Char(digit) = key.code
            && let Some(number) = digit.to_digit(10)
            && !ctrl
            && number >= 1
            && (number as usize) <= visible
        {
            let row = {
                let mut s = self.st();
                s.menu.select(number as usize - 1);
                s.menu.current().cloned()
            };
            self.chosen(row, "enter");
            return;
        }
        if key.code == KeyCode::Tab && !ctrl {
            let shell = {
                let s = self.st();
                s.draft.text().starts_with('!') && s.draft.selection_is_empty()
            };
            if shell {
                self.complete_shell();
                return;
            }
        }
        if matches!(key.code, KeyCode::Up | KeyCode::Down) && !ctrl && !alt && !shift {
            let step = if key.code == KeyCode::Up { -1 } else { 1 };
            let recalled = self.st().recall(step);
            if recalled {
                self.after_edit();
                return;
            }
        }
        if key.code == KeyCode::Enter {
            if is_send {
                self.action_send();
            } else {
                self.type_text("\n");
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('j') {
            self.type_text("\n");
            return;
        }
        let before = self.st().draft.text();
        let handled = self.st().draft.edit_key(key);
        if handled && self.st().draft.text() != before {
            self.after_edit();
        }
    }

    /// The menu's answer: a row chosen with Enter or Tab, or none (Escape).
    pub fn chosen(&self, row: Option<MenuRow>, key: &str) {
        let (kind, sender) = {
            let s = self.st();
            (s.menu.kind, s.picking.clone())
        };
        if matches!(kind, Some(MenuKind::Pick | MenuKind::Approve)) {
            if let Some(sender) = sender {
                let _ = sender.send(row.map(|row| row.value));
            }
            return;
        }
        let Some(kind) = kind else { return };
        self.st().menu.close();
        let Some(row) = row else { return };
        let command = row.value.as_str().unwrap_or("").to_string();
        if kind == MenuKind::Shell || key == "tab" {
            {
                let mut s = self.st();
                s.draft.load_text(&if kind == MenuKind::Shell {
                    command
                } else {
                    format!("{command} ")
                });
                s.draft.move_to_end();
            }
            self.after_edit();
            return;
        }
        self.st().draft.clear();
        self.after_edit();
        self.submit(&command);
    }

    /// Enter: send what is in the prompt.
    pub fn action_send(&self) {
        let text = {
            let mut s = self.st();
            let text = s.draft.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            s.draft.clear();
            if s.history.last() != Some(&text) {
                s.history.push(text.clone());
            }
            s.history_at = None;
            text
        };
        self.after_edit();
        self.submit(&text);
    }

    /// Tab in !mode: finish the last word, or extend it to the common prefix and list what is left.
    fn complete_shell(&self) {
        let (text, project) = {
            let s = self.st();
            (s.draft.text(), s.project.clone())
        };
        let (word, found) = render::shell_candidates(&text, &project);
        if found.is_empty() {
            return;
        }
        let prefix = common_prefix(&found);
        let mut s = self.st();
        if prefix.chars().count() > word.chars().count() {
            let extra: String = prefix.chars().skip(word.chars().count()).collect();
            s.draft.insert(&extra);
        } else if found.len() > 1 {
            let stem: String = text
                .chars()
                .take(text.chars().count() - word.chars().count())
                .collect();
            let rows: Vec<MenuRow> = found
                .iter()
                .take(200)
                .map(|name| {
                    let value =
                        format!("{stem}{name}{}", if name.ends_with('/') { "" } else { " " });
                    MenuRow::new(name.clone(), serde_json::json!(value))
                })
                .collect();
            s.menu
                .open(MenuKind::Shell, "Completions", rows, "", None, "");
        }
        if found.len() == 1 && !prefix.ends_with('/') {
            s.draft.insert(" ");
        }
    }

    pub fn handle_paste(&self, text: &str) {
        let project = self.st().project.clone();
        let mut paths = render::dropped_images(text, &project);
        if paths.is_empty()
            && let Some((extension, data)) = render::pasted_image(text)
            && let Ok(path) = render::save_pasted_image(&extension, &data)
        {
            paths = vec![path];
        }
        if paths.is_empty() {
            self.st().draft.insert(text);
        } else {
            let quoted: Vec<String> = paths.iter().map(|path| render::quote_path(path)).collect();
            self.st().draft.insert(&format!("{} ", quoted.join(" ")));
        }
        self.after_edit();
    }

    pub fn handle_mouse(&self, mouse: MouseEvent) {
        let (transcript, draft) = {
            let s = self.st();
            (s.areas.transcript, s.areas.draft)
        };
        let inside = |area: ratatui::layout::Rect| {
            mouse.column >= area.x
                && mouse.column < area.x + area.width
                && mouse.row >= area.y
                && mouse.row < area.y + area.height
        };
        if inside(transcript)
            || matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
                && self.st().transcript.selected_text().is_some()
        {
            let event = self.st().transcript.on_mouse(mouse, transcript);
            match event {
                TranscriptEvent::Selected(text) => {
                    if self.copy_text(&text) {
                        self.st()
                            .notify(format!("Copied {} characters", text.chars().count()));
                    }
                }
                TranscriptEvent::Link(url) => {
                    let open = std::sync::Arc::clone(&self.hooks().open_url);
                    open(&url);
                }
                TranscriptEvent::None | TranscriptEvent::Scrolled => {}
            }
        } else if inside(draft) {
            self.st().draft.on_mouse(mouse, draft);
            let selected = if matches!(mouse.kind, MouseEventKind::Up(_)) {
                self.st().draft.selected_text()
            } else {
                None
            };
            if let Some(text) = selected
                && self.copy_text(&text)
            {
                self.st()
                    .notify(format!("Copied {} characters", text.chars().count()));
            }
        }
    }
}

fn common_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for name in &names[1..] {
        let common = prefix
            .iter()
            .zip(name.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        prefix.truncate(common);
    }
    prefix.into_iter().collect()
}
