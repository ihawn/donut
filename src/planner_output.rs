//! compact planner presentation; detailed events remain in the run logs.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use regex::Regex;
use serde_json::Value;

use crate::ui::paint;

static ERROR_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(error|fatal|traceback)\b").unwrap());
static SLUG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\*\*Slug:\*\*\s*`?([^`\n]+)").unwrap());

/// The live progress line the planner view draws on: `tick` redraws it with an activity, `clear` wipes it.
pub trait Progress {
    fn tick(&mut self, activity: &str);
    fn clear(&mut self);
}

pub struct PlannerOutput<P: Progress> {
    pub progress: P,
    pub activity: String,
    pub seen: HashSet<String>,
    pub answer: String,
    last_tick: Option<Instant>,
    out: Box<dyn Write + Send>,
}

fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

impl<P: Progress> PlannerOutput<P> {
    /// A view that prints to stdout.
    pub fn new(progress: P) -> Self {
        Self::with_writer(progress, Box::new(std::io::stdout()))
    }

    pub fn with_writer(progress: P, out: Box<dyn Write + Send>) -> Self {
        PlannerOutput {
            progress,
            activity: "Thinking".into(),
            seen: HashSet::new(),
            answer: String::new(),
            last_tick: None,
            out,
        }
    }

    fn print(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        if self
            .last_tick
            .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(200))
        {
            let activity = self.activity.clone();
            self.progress.tick(&activity);
            self.last_tick = Some(now);
        }
    }

    pub fn set_activity(&mut self, text: &str) {
        self.activity = text.to_string();
        if !self.seen.contains(text) {
            self.progress.clear();
            self.print(&paint(&format!("  · {text}"), "teal"));
            self.seen.insert(text.to_string());
        }
    }

    pub fn feed(&mut self, line: &str) {
        let item: Value = match serde_json::from_str(line) {
            Ok(item) => item,
            Err(_) => {
                if ERROR_LINE.is_match(line) {
                    self.error(&Value::String(line.to_string()));
                }
                self.tick();
                return;
            }
        };
        if !item.is_object() {
            return;
        }
        let kind = str_of(&item, "type");
        let none = Value::Object(Default::default());
        if kind == "error" || kind == "turn.failed" || truthy(item.get("is_error")) {
            let default = Value::String("Planner reported an error; see logs.".into());
            let message = item
                .get("error")
                .or_else(|| item.get("result"))
                .unwrap_or(&default)
                .clone();
            self.error(&message);
        } else if kind == "system" && str_of(&item, "subtype") == "init" {
            self.set_activity("Reading project context");
        } else if kind == "item.started" || kind == "item.completed" {
            let entry = item.get("item").filter(|e| e.is_object()).unwrap_or(&none);
            match str_of(entry, "type") {
                "reasoning" => self.activity = "Thinking".into(),
                "agent_message" => self.answer = str_of(entry, "text").to_string(),
                "file_change" => self.set_activity("Writing tickets"),
                "command_execution" => self.set_activity("Inspecting the project"),
                _ => {}
            }
            let code = entry.get("exit_code");
            if !matches!(code, None | Some(Value::Null))
                && code.and_then(Value::as_f64) != Some(0.0)
            {
                self.error(&Value::String("A planner command failed; see logs.".into()));
            }
        } else if kind == "stream_event" {
            let event = item.get("event").filter(|e| e.is_object()).unwrap_or(&none);
            let block = event
                .get("content_block")
                .filter(|e| e.is_object())
                .unwrap_or(&none);
            let delta = event
                .get("delta")
                .filter(|e| e.is_object())
                .unwrap_or(&none);
            if str_of(event, "type") == "content_block_start" {
                match str_of(block, "type") {
                    "thinking" => self.activity = "Thinking".into(),
                    "text" => {
                        self.answer = str_of(block, "text").to_string();
                        self.activity = "Preparing the plan".into();
                    }
                    "tool_use" => {
                        let name = str_of(block, "name").to_lowercase();
                        let has = |words: &[&str]| words.iter().any(|word| name.contains(word));
                        let activity = if has(&["write", "edit", "patch"]) {
                            "Writing tickets"
                        } else if has(&["read", "search", "grep", "glob", "list"]) {
                            "Reading project files"
                        } else {
                            "Inspecting the project"
                        };
                        self.set_activity(activity);
                    }
                    _ => {}
                }
            } else if str_of(delta, "type") == "text_delta" {
                self.answer.push_str(str_of(delta, "text"));
            } else if str_of(delta, "type") == "thinking_delta" {
                self.activity = "Thinking".into();
            }
        } else if kind == "assistant" {
            if let Some(content) = item
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                let text: String = content
                    .iter()
                    .filter(|b| b.is_object() && str_of(b, "type") == "text")
                    .map(|b| str_of(b, "text"))
                    .collect();
                if !text.is_empty() {
                    self.answer = text;
                }
            }
        } else if kind == "result" && item.get("result").is_some_and(Value::is_string) {
            self.answer = str_of(&item, "result").to_string();
        } else if kind == "user" {
            let failed = item
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .is_some_and(|content| {
                    content
                        .iter()
                        .any(|b| b.is_object() && truthy(b.get("is_error")))
                });
            if failed {
                self.error(&Value::String("A planner tool failed; see logs.".into()));
            }
        }
        self.tick();
    }

    pub fn error(&mut self, message: &Value) {
        self.progress.clear();
        let text = match message {
            Value::Object(map) => map
                .get("message")
                .map(python_str)
                .unwrap_or_else(|| "Planner error; see logs.".into()),
            other => python_str(other),
        };
        let squeezed: String = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect();
        self.print(&paint(&format!("  Error: {squeezed}"), "coral"));
    }

    pub fn finish(&mut self, success: bool) {
        self.progress.clear();
        if !success {
            return;
        }
        let mut text = self.answer.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        // Split after . ! or ? when whitespace follows; the text has single spaces only.
        let mut sentences: Vec<&str> = Vec::new();
        let mut start = 0;
        let mut previous = ' ';
        for (index, c) in text.char_indices() {
            if c == ' ' && matches!(previous, '.' | '!' | '?') {
                sentences.push(&text[start..index]);
                start = index + 1;
            }
            previous = c;
        }
        sentences.push(&text[start..]);
        text = sentences.into_iter().take(2).collect::<Vec<_>>().join(" ");
        if text.chars().count() > 300 {
            let cut: String = text.chars().take(297).collect();
            let kept = cut.rsplit_once(' ').map(|(head, _)| head).unwrap_or(&cut);
            text = format!("{kept}…");
        }
        self.print(&format!("\n{text}"));
    }
}

/// Every ticket in the swim lanes: (identity, path), where the identity is the ticket's slug or its file name.
pub type Snapshot = Vec<(String, PathBuf)>;

fn is_ticket_name(name: &str) -> bool {
    // The glob "[0-9]*_*.md".
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_digit)
        && name.len() > 4
        && name.ends_with(".md")
        && name[1..name.len() - 3].contains('_')
}

pub fn ticket_snapshot(project: &Path, lanes: Option<&Path>) -> Result<Snapshot> {
    let mut tickets: Snapshot = Vec::new();
    let root = lanes
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project.join("swim_lanes"));
    for lane in ["to_do", "in_progress", "on_hold", "blocked", "done"] {
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(root.join(lane)) {
            Ok(read) => read
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| is_ticket_name(&n.to_string_lossy()))
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path)?;
            let identity = match SLUG.captures(&text) {
                Some(found) => found[1].trim().to_string(),
                None => path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            match tickets.iter_mut().find(|(known, _)| *known == identity) {
                Some(entry) => entry.1 = path,
                None => tickets.push((identity, path)),
            }
        }
    }
    Ok(tickets)
}

/// Print the tickets that are not in `before` to `out`.
pub fn write_created_tickets(
    out: &mut dyn Write,
    project: &Path,
    before: &Snapshot,
    lanes: Option<&Path>,
) -> Result<()> {
    let mut created: Vec<PathBuf> = ticket_snapshot(project, lanes)?
        .into_iter()
        .filter(|(identity, _)| !before.iter().any(|(known, _)| known == identity))
        .map(|(_, path)| path)
        .collect();
    writeln!(
        out,
        "{}",
        paint(&format!("\nTickets created: {}", created.len()), "teal")
    )?;
    created.sort();
    for path in created {
        let shown = path
            .strip_prefix(project)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        writeln!(out, "  - {shown}")?;
    }
    Ok(())
}

/// Print the tickets that are not in `before` to stdout.
pub fn show_created_tickets(project: &Path, before: &Snapshot, lanes: Option<&Path>) -> Result<()> {
    write_created_tickets(&mut std::io::stdout(), project, before, lanes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[derive(Clone, Default)]
    struct Mock(Arc<Mutex<Vec<String>>>);

    impl Progress for Mock {
        fn tick(&mut self, activity: &str) {
            self.0.lock().unwrap().push(activity.to_string());
        }
        fn clear(&mut self) {}
    }

    fn view() -> (PlannerOutput<Mock>, Shared, Mock) {
        let (out, mock) = (Shared::default(), Mock::default());
        (
            PlannerOutput::with_writer(mock.clone(), Box::new(out.clone())),
            out,
            mock,
        )
    }

    fn event(inner: Value) -> Value {
        json!({"type": "stream_event", "event": inner})
    }

    #[test]
    fn test_activity_is_concise_and_payloads_are_hidden() {
        let (mut view, output, progress) = view();
        let events = [
            event(
                json!({"type": "content_block_start", "content_block": {"type": "thinking", "thinking": "NOISY REASONING"}}),
            ),
            event(
                json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "MORE REASONING"}}),
            ),
            event(
                json!({"type": "content_block_start", "content_block": {"type": "tool_use", "name": "read_file"}}),
            ),
            event(
                json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "RAW ARGUMENTS"}}),
            ),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "HUGE FILE CONTENTS"}]}}),
        ];
        for item in events.iter().chain(events.iter()) {
            view.feed(&item.to_string());
        }
        let text = output.text();
        assert_eq!(text.matches("Reading project files").count(), 1);
        for payload in ["REASONING", "RAW ARGUMENTS", "HUGE FILE"] {
            assert!(!text.contains(payload), "{payload}");
        }
        assert!(progress.0.lock().unwrap().iter().any(|a| a == "Thinking"));
    }

    #[test]
    fn test_final_answer_is_brief_and_not_duplicated() {
        let (mut view, output, _) = view();
        let answer =
            "Planned a settings menu. Added focused tickets. Extra details should stay hidden.";
        let events = vec![
            event(json!({"type": "content_block_start", "content_block": {"type": "text"}})),
            event(
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": answer}}),
            ),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": answer}]}}),
            json!({"type": "result", "result": answer}),
        ];
        for item in &events {
            view.feed(&item.to_string());
        }
        assert_eq!(output.text(), "");
        view.finish(true);
        assert_eq!(
            output.text().trim(),
            "Planned a settings menu. Added focused tickets."
        );
    }

    #[test]
    fn test_codex_errors_and_summary_limits() {
        let (mut view, output, _) = view();
        view.feed(&json!({"type": "item.started", "item": {"type": "command_execution", "command": "VERBOSE COMMAND"}}).to_string());
        view.feed(
            &json!({"type": "item.completed", "item": {"type": "command_execution", "exit_code": 1, "aggregated_output": "VERBOSE OUTPUT"}})
                .to_string(),
        );
        view.feed(&json!({"type": "item.completed", "item": {"type": "agent_message", "text": "word ".repeat(200)}}).to_string());
        view.finish(true);
        let text = output.text();
        assert!(text.contains("Inspecting the project"));
        assert!(text.contains("Error:"));
        assert!(!text.contains("VERBOSE"));
        assert!(text.lines().last().unwrap().chars().count() <= 300);
        let before = text.len();
        view.finish(false);
        assert_eq!(output.text().len(), before);
    }

    #[test]
    fn test_plain_error_lines_are_shown_and_other_lines_are_ignored() {
        let (mut view, output, _) = view();
        view.feed("just some log line");
        view.feed("Traceback (most recent call last):");
        assert_eq!(output.text().matches("Error:").count(), 1);
    }

    #[test]
    fn test_ticket_list_ignores_old_edited_and_renumbered_tickets() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        let lane = project.join("swim_lanes/to_do");
        std::fs::create_dir_all(&lane).unwrap();
        let old = lane.join("001_existing.md");
        std::fs::write(&old, "**Slug:** `existing`\n").unwrap();
        let before = ticket_snapshot(project, None).unwrap();
        std::fs::rename(&old, lane.join("002_existing.md")).unwrap();
        std::fs::write(
            lane.join("002_existing.md"),
            "**Slug:** `existing`\nUpdated requirements\n",
        )
        .unwrap();
        std::fs::write(lane.join("001_new.md"), "**Slug:** `new`\n").unwrap();
        let mut output = Vec::new();
        write_created_tickets(&mut output, project, &before, None).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Tickets created: 1"));
        assert!(text.contains("001_new.md"));
        assert!(!text.contains("existing.md"));
        let mut unchanged = Vec::new();
        write_created_tickets(
            &mut unchanged,
            project,
            &ticket_snapshot(project, None).unwrap(),
            None,
        )
        .unwrap();
        assert!(
            String::from_utf8(unchanged)
                .unwrap()
                .contains("Tickets created: 0")
        );
    }

    #[test]
    fn test_released_hold_ticket_is_not_reported_as_new() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        let held = project.join("swim_lanes/blocked");
        let todo = project.join("swim_lanes/to_do");
        std::fs::create_dir_all(&held).unwrap();
        std::fs::create_dir_all(&todo).unwrap();
        let ticket = held.join("001_held.md");
        std::fs::write(&ticket, "**Slug:** `held`\n").unwrap();
        let before = ticket_snapshot(project, None).unwrap();
        std::fs::rename(&ticket, todo.join("001_held.md")).unwrap();
        let mut output = Vec::new();
        write_created_tickets(&mut output, project, &before, None).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Tickets created: 0")
        );
    }
}
