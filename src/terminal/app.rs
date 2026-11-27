//! The `State` of the interactive terminal: every field of Python's `Donut` app, and the logic that never
//! blocks (events from providers, status lines, history, transcript writes). Anything that waits on a provider
//! or on the user lives in [`super::flows`], on [`super::App`].
//!
//! Rule for this file: no method calls a provider session. The caller reads what it needs from the session
//! first and passes the values in, so the state lock is never held while a session takes its own locks.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use anyhow::{Result, bail};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Map, Value, json};

use super::RichLine;
use super::render::{
    self, ACTIVITY_NAMES, BADGE_DARK, COMMANDS, COMPLEX_COMMANDS, CORAL, DIM, GRAY, INTERFACES,
    ORANGE, ROLE_TITLES, TEAL, mode_badge, mode_placeholder, model_name, pack_entries, remaining,
    usage_bar, usage_key,
};
use super::widgets::{Draft, Menu, MenuKind, MenuRow, Transcript};
use crate::background::Wake;
use crate::config::{profile_name, role_choice};
use crate::providers::PERMISSION_MODES;
use crate::sessions::{Session, UsageWindow};
use crate::{conversations, handholding, plugins, ponytail, routing, spend, ui, workspace};

pub const SPINNER_TICK_MS: u128 = 100;
pub const QUOTA_REFRESH_SECONDS: u64 = 300;

/// What Python kept as class attributes so tests could switch them off.
#[derive(Clone, Debug)]
pub struct AppOptions {
    /// Ask for each account's usage in the background (starts providers); tests turn this off.
    pub auto_refresh_quotas: bool,
    /// Ask simple or complex the first time Donut opens.
    pub ask_interface: bool,
    /// plan, work, review or solo: the mode Donut opens in.
    pub start_mode: String,
}

impl Default for AppOptions {
    fn default() -> Self {
        AppOptions {
            auto_refresh_quotas: true,
            ask_interface: true,
            start_mode: "solo".into(),
        }
    }
}

impl AppOptions {
    /// The options the Python tests set: no background providers, no first-open question, open in plan mode.
    pub fn test() -> Self {
        AppOptions {
            auto_refresh_quotas: false,
            ask_interface: false,
            start_mode: "plan".into(),
        }
    }
}

/// `(alias[@purpose], model)`: how Python keyed `self.sessions`.
pub type SessionKey = (String, Option<String>);

/// One quota entry: the usage key, then the label, alias and profile that stand for it.
pub type UsageGroup = (Vec<String>, (String, Option<String>, Value));

/// A live provider session, as the worker threads hold it.
#[derive(Clone)]
pub struct SessionHandle {
    pub id: u64,
    pub session: Arc<dyn Session>,
}

/// What Python kept on a session object to count its spend once.
#[derive(Clone, Debug, Default)]
pub struct SpendTracker {
    pub reported: f64,
    pub last_usage: Option<Value>,
}

/// A session plus everything Donut remembers about it.
pub struct SessionRec {
    pub id: u64,
    pub key: SessionKey,
    pub session: Arc<dyn Session>,
    /// `self.session_profiles[key]`.
    pub profile: Value,
    pub task: Option<ponytail::Task>,
    pub conversation_id: Option<String>,
    pub donut_seen: usize,
    pub wake_buffer: Option<Vec<String>>,
    pub told_wait: bool,
    pub donut_waits: bool,
    pub spend: SpendTracker,
    pub turn_spend: f64,
}

impl SessionRec {
    pub fn handle(&self) -> SessionHandle {
        SessionHandle {
            id: self.id,
            session: Arc::clone(&self.session),
        }
    }
}

/// One exchange of the window, whatever the mode and model.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub mode: String,
    pub model: String,
    pub you: String,
    pub answer: String,
    pub by: u64,
}

/// The model auto picked for this conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub index: Option<usize>,
    pub score: Option<i64>,
    pub harder: i64,
}

/// `(role, alias, profile)` of the model doing the current turn.
#[derive(Clone, Debug)]
pub struct Running {
    pub role: String,
    pub alias: Option<String>,
    pub profile: Value,
}

/// A message typed during a turn, or a background wake-up, waiting for the turn to end.
#[derive(Clone)]
pub enum Queued {
    Text(String),
    Wake(Wake<u64>),
}

#[derive(Clone, Debug)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub started: Instant,
}

/// Work the state asks the app to do once the lock is released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deferred {
    /// Run the next queued message.
    RunQueued,
    /// Ask the accounts for their usage again.
    RefreshQuotas,
    /// A provider-started turn ended: book it (`log_turn`) and finish it.
    FinishWake(u64),
}

/// Where the last draw put the widgets, so mouse events can find them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Areas {
    pub transcript: Rect,
    pub draft: Rect,
}

pub struct State {
    pub project: PathBuf,
    pub options: AppOptions,
    pub session_name: Option<String>,
    pub entered_session: bool,
    pub task_title: String,
    pub config: Value,
    pub mode: String,
    pub selected: HashMap<String, Option<String>>,
    pub profiles: HashMap<String, Value>,
    pub auto: bool,
    pub route: Option<Route>,
    pub history: Vec<String>,
    pub history_at: Option<usize>,
    pub history_stash: String,
    /// Pasted or dropped image paths; the draft shows image N as `[screenshotN]` (render::image_label).
    pub images: Vec<String>,
    pub recalled: bool,
    pub steps: Option<Vec<routing::Step>>,
    pub interface: String,
    /// "planner", "off" or "chosen".
    pub reviewer: String,
    pub sessions: Vec<SessionRec>,
    pub next_session_id: u64,
    pub busy: bool,
    pub active: Option<SessionHandle>,
    /// Process id of the shell command or worker this window started, so Ctrl+C can stop its group.
    pub child_pid: Option<u32>,
    pub turn_text: String,
    /// The one-sentence recap taken out of this turn's answer, shown under it (see crate::recap).
    pub turn_recap: Option<String>,
    pub shown: usize,
    pub message_start: usize,
    pub turn_tools: Vec<String>,
    pub last_answer: String,
    pub picking: Option<Sender<Option<Value>>>,
    pub background_tasks: Vec<BackgroundTask>,
    pub threads: HashMap<String, u64>,
    pub waiting_resume: Vec<(String, Value)>,
    pub exchanges: Vec<Exchange>,
    pub after_tools: bool,
    pub verbosity: String,
    pub thinking_text: String,
    pub queued: Vec<Queued>,
    pub shell_runs: Vec<(String, i32, String)>,
    pub waiting: Vec<(u64, String)>,
    pub always_allow: HashSet<String>,
    pub permission: String,
    pub previous_permission: String,
    pub activity: String,
    pub running: Option<Running>,
    pub started: Instant,
    pub detail: String,
    pub quotas: HashMap<Vec<String>, Vec<UsageWindow>>,
    pub ponytail_installed: HashMap<Vec<String>, bool>,
    pub spent: f64,
    pub spend_by: BTreeMap<(String, String), Option<f64>>,
    pub estimates: HashMap<String, f64>,
    pub focus_view: bool,
    pub transcript: Transcript,
    pub response: Transcript,
    pub menu: Menu,
    pub draft: Draft,
    pub shown_title: Option<String>,
    /// Bytes for the terminal (title escapes, OSC 52 copies); the run loop writes and drains them.
    pub terminal_out: Vec<String>,
    pub quit: bool,
    /// Short messages (Textual's toasts): text and when they end.
    pub notices: Vec<(String, Instant)>,
    /// Tests record `start_turn` calls here when this is `Some`.
    pub turn_log: Option<Vec<(String, String, String)>>,
    pub deferred: Vec<Deferred>,
    pub areas: Areas,
}

pub fn hex_color(name: &str) -> Color {
    let (r, g, b) = ui::color(name);
    Color::Rgb(r, g, b)
}

pub fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

pub fn bold(color: Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
}

pub fn italic(color: Color) -> Style {
    fg(color).add_modifier(Modifier::ITALIC)
}

/// A line of one style.
pub fn styled(text: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(vec![span(text, style)])
}

pub fn join_lines(parts: Vec<Span<'static>>) -> Line<'static> {
    Line::from(parts)
}

/// `profile.get(key)` when it is a non-empty string (Python's `profile.get(key) or ...` for text).
pub fn text_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

pub fn engine_of(profile: &Value) -> Option<&str> {
    text_of(profile, "engine")
}

pub fn is_empty_profile(profile: &Value) -> bool {
    profile.as_object().is_none_or(Map::is_empty)
}

fn windows_from_value(value: &Value) -> Vec<UsageWindow> {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .map(|window| UsageWindow {
                    window: window
                        .get("window")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    percent: window.get("percent").and_then(Value::as_f64).unwrap_or(0.0),
                    resets_at: window.get("resets_at").and_then(Value::as_f64),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Python `shlex.split`, falling back to whitespace splitting when a quote is left open.
pub fn split_words(text: &str) -> Vec<String> {
    match shlex::split(text) {
        Some(words) => words,
        None => text.split_whitespace().map(str::to_string).collect(),
    }
}

impl State {
    /// The window as `Donut.__init__` makes it: models from the saved choices, reviewer, permission mode.
    pub fn new(
        project: &Path,
        alias: Option<&str>,
        session: Option<&str>,
        options: &AppOptions,
        config: Value,
    ) -> Result<State> {
        let aliases = config
            .get("aliases")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(alias) = alias
            && aliases.get(alias).is_none()
        {
            bail!("Unknown model alias: {alias}");
        }
        let mut selected: HashMap<String, Option<String>> = HashMap::new();
        let mut profiles: HashMap<String, Value> = HashMap::new();
        for (role, key) in [("plan", "planner"), ("work", "default_model")] {
            let saved = if alias.is_some() {
                None
            } else {
                role_choice(&config, role)
            };
            if let Some((name, profile)) = saved {
                selected.insert(role.into(), Some(name));
                profiles.insert(role.into(), profile);
            } else {
                let name = alias
                    .map(str::to_string)
                    .or_else(|| text_of(&config, key).map(str::to_string))
                    .or_else(|| {
                        aliases
                            .as_object()
                            .and_then(|map| map.keys().next().cloned())
                    });
                let profile = name
                    .as_deref()
                    .and_then(|name| aliases.get(name))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                selected.insert(role.into(), name);
                profiles.insert(role.into(), profile);
            }
        }
        let ladder = routing::worker_ladder(&aliases);
        let saved_work = config
            .get("roles")
            .and_then(|roles| roles.get("work"))
            .and_then(|work| text_of(work, "alias"))
            .map(str::to_string);
        if alias.is_none()
            && saved_work.as_deref() == Some(routing::AUTO)
            && aliases.get(routing::AUTO).is_none()
            && !ladder.is_empty()
        {
            selected.insert("work".into(), Some(routing::AUTO.into()));
            profiles.insert("work".into(), ladder[0].clone());
        }
        // No default soloist: the first solo message asks. donut -m ALIAS makes ALIAS the soloist too.
        let soloist = match alias {
            Some(alias) => Some((alias.to_string(), aliases[alias].clone())),
            None => role_choice(&config, "solo"),
        };
        match &soloist {
            Some((name, profile)) => {
                selected.insert("solo".into(), Some(name.clone()));
                profiles.insert("solo".into(), profile.clone());
            }
            None => {
                selected.insert("solo".into(), None);
                profiles.insert("solo".into(), json!({}));
            }
        }
        let saved_solo = config
            .get("roles")
            .and_then(|roles| roles.get("solo"))
            .and_then(|solo| text_of(solo, "alias"))
            .map(str::to_string);
        let auto = soloist.is_none() && saved_solo.as_deref() == Some(routing::AUTO);
        let interface = match text_of(&config, "interface") {
            Some(name) if INTERFACES.iter().any(|(key, _)| *key == name) => name.to_string(),
            _ => "complex".to_string(),
        };
        let mut mode = options.start_mode.clone();
        if interface == "simple" {
            mode = "solo".into();
        }
        let roles = config
            .get("roles")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let reviewer;
        if text_of(&config, "reviewer") == Some("off") {
            reviewer = "off";
        } else if roles.get("review").is_some_and(render::truthy)
            || text_of(&config, "reviewer").is_some_and(|name| aliases.get(name).is_some())
        {
            reviewer = "chosen";
            if let Some((name, profile)) = role_choice(&config, "review") {
                selected.insert("review".into(), Some(name));
                profiles.insert("review".into(), profile);
            }
        } else {
            reviewer = "planner";
        }
        let permission = match text_of(&config, "permission_mode") {
            Some(mode) if PERMISSION_MODES.iter().any(|(key, _, _)| *key == mode) => {
                mode.to_string()
            }
            _ => "ask".to_string(),
        };
        let verbosity = match text_of(&config, "verbosity") {
            Some(level) if render::verbosity_entry(level).is_some() => level.to_string(),
            _ => "normal".to_string(),
        };
        let mut state = State {
            project: project.to_path_buf(),
            options: options.clone(),
            session_name: Some(match session {
                Some(name) => name.to_string(),
                None => workspace::new_name(project, None, &[]),
            }),
            entered_session: session.is_some(),
            task_title: String::new(),
            config,
            mode,
            selected,
            profiles,
            auto,
            route: None,
            history: Vec::new(),
            history_at: None,
            history_stash: String::new(),
            images: Vec::new(),
            recalled: false,
            steps: None,
            interface,
            reviewer: reviewer.to_string(),
            sessions: Vec::new(),
            next_session_id: 0,
            busy: false,
            active: None,
            child_pid: None,
            turn_text: String::new(),
            turn_recap: None,
            shown: 0,
            message_start: 0,
            turn_tools: Vec::new(),
            last_answer: String::new(),
            picking: None,
            background_tasks: Vec::new(),
            threads: HashMap::new(),
            waiting_resume: Vec::new(),
            exchanges: Vec::new(),
            after_tools: false,
            verbosity,
            thinking_text: String::new(),
            queued: Vec::new(),
            shell_runs: Vec::new(),
            waiting: Vec::new(),
            always_allow: HashSet::new(),
            permission,
            previous_permission: "ask".into(),
            activity: String::new(),
            running: None,
            started: Instant::now(),
            detail: String::new(),
            quotas: HashMap::new(),
            ponytail_installed: HashMap::new(),
            spent: 0.0,
            spend_by: BTreeMap::new(),
            estimates: HashMap::new(),
            focus_view: false,
            transcript: Transcript::new(),
            response: Transcript::new(),
            menu: Menu::new(),
            draft: Draft::new(),
            shown_title: None,
            terminal_out: Vec::new(),
            quit: false,
            notices: Vec::new(),
            turn_log: None,
            deferred: Vec::new(),
            areas: Areas::default(),
        };
        state.sync_reviewer();
        state.draft.placeholder = state.placeholder();
        Ok(state)
    }

    // ---- small accessors ------------------------------------------------------------------------------

    pub fn simple(&self) -> bool {
        self.interface == "simple"
    }

    pub fn sel(&self, role: &str) -> Option<String> {
        self.selected.get(role).cloned().flatten()
    }

    pub fn prof(&self, role: &str) -> Value {
        self.profiles
            .get(role)
            .cloned()
            .unwrap_or_else(|| json!({}))
    }

    pub fn role(&self) -> String {
        self.mode.clone()
    }

    /// Solo and review keep conversations of their own, apart from the planner's, even on the same model.
    pub fn purpose(&self) -> Option<String> {
        matches!(self.mode.as_str(), "solo" | "review").then(|| self.mode.clone())
    }

    pub fn lanes(&self) -> PathBuf {
        workspace::lanes_dir(&self.project, self.session_name.as_deref())
    }

    pub fn chrome_on(&self) -> bool {
        self.config.get("chrome") != Some(&Value::Bool(false))
    }

    pub fn final_only(&self) -> bool {
        matches!(self.verbosity.as_str(), "quiet" | "normal")
    }

    pub fn placeholder(&self) -> String {
        if self.simple() {
            render::SIMPLE_PLACEHOLDER.to_string()
        } else {
            mode_placeholder(&self.mode).to_string()
        }
    }

    pub fn session_key(alias: &str, profile: &Value, purpose: Option<&str>) -> SessionKey {
        let name = match purpose {
            Some(purpose) => format!("{alias}@{purpose}"),
            None => alias.to_string(),
        };
        (name, text_of(profile, "model").map(str::to_string))
    }

    pub fn rec(&self, id: u64) -> Option<&SessionRec> {
        self.sessions.iter().find(|rec| rec.id == id)
    }

    pub fn rec_mut(&mut self, id: u64) -> Option<&mut SessionRec> {
        self.sessions.iter_mut().find(|rec| rec.id == id)
    }

    pub fn rec_by_key(&self, key: &SessionKey) -> Option<&SessionRec> {
        self.sessions.iter().find(|rec| &rec.key == key)
    }

    pub fn session_label(&self) -> String {
        if !self.task_title.is_empty() {
            return self.task_title.clone();
        }
        match self.session_name.as_deref() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => "swim_lanes".to_string(),
        }
    }

    pub fn window_title(&self) -> String {
        let name = self
            .project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = if name.is_empty() {
            self.project.display().to_string()
        } else {
            name
        };
        format!("{} · {} · Donut", self.session_label(), name)
    }

    pub fn show_title(&mut self) {
        let title = self.window_title();
        if self.shown_title.as_deref() == Some(title.as_str()) {
            return;
        }
        self.shown_title = Some(title.clone());
        // OSC 1 sets the tab title and OSC 2 the window title (iTerm2 ignores OSC 0 here); control characters
        // are left out so the title cannot escape.
        let clean: String = title.chars().filter(|c| !c.is_control()).collect();
        self.terminal_out
            .push(format!("\x1b]1;{clean}\x07\x1b]2;{clean}\x07"));
    }

    pub fn notify(&mut self, text: impl Into<String>) {
        self.notices.push((
            text.into(),
            Instant::now() + std::time::Duration::from_secs(2),
        ));
    }

    // ---- the transcript ---------------------------------------------------------------------------------

    /// Write one styled line; URLs in it become links.
    pub fn say_line(&mut self, line: Line<'static>) {
        self.transcript.push(render::link_text(line));
    }

    pub fn say_rich(&mut self, line: RichLine) {
        self.transcript.push(line);
    }

    /// Plain text, in a color or not.
    pub fn say(&mut self, text: &str, color: Option<Color>) {
        let style = color.map(fg).unwrap_or_default();
        self.say_line(styled(text, style));
    }

    pub fn say_dim(&mut self, text: &str) {
        self.say(text, Some(DIM));
    }

    /// Rendered Markdown, one transcript line per rendered line.
    pub fn say_markdown(&mut self, source: &str) {
        let lines = render::markdown(source);
        self.transcript.push_lines(lines);
    }

    pub fn blank(&mut self) {
        self.say_line(Line::default());
    }

    pub fn welcome(&mut self) {
        for runs in ui::logo_segments() {
            let mut spans = Vec::new();
            for (text, color) in runs {
                let style = color.map(|name| fg(hex_color(name))).unwrap_or_default();
                spans.push(span(text, style));
            }
            self.say_line(Line::from(spans));
        }
        let mut title = vec![
            span("  DONUT", bold(ORANGE)),
            span(format!("  /  {}", ui::release_version()), fg(CORAL)),
        ];
        title.push(span(format!("   {}", self.project.display()), fg(DIM)));
        let hints: Vec<(&str, &str)> = if self.simple() {
            self.say_line(Line::from(title));
            self.say(
                "  Ask for what you need. Donut picks the model for each conversation.",
                Some(GRAY),
            );
            vec![
                ("/harder", "stronger model"),
                ("/clear", "new conversation"),
                ("/", "commands"),
                ("Shift+Enter", "newline"),
            ]
        } else {
            title.push(span(
                format!(
                    "   tickets {}",
                    workspace::relative(&self.project, &self.lanes())
                ),
                fg(DIM),
            ));
            self.say_line(Line::from(title));
            self.say(
                "  The planner writes the tickets. The worker takes them one at a time.",
                Some(GRAY),
            );
            vec![
                ("/plan", "tickets"),
                ("/work", "build"),
                ("/review", "check"),
                ("/solo", "direct"),
                ("/", "commands"),
                ("!", "shell"),
                ("Shift+Enter", "newline"),
            ]
        };
        let mut hint = vec![span("  ", Style::default())];
        for (key, description) in hints {
            hint.push(span(key, bold(ORANGE)));
            hint.push(span(format!(" {description}   "), fg(DIM)));
        }
        self.say_line(Line::from(hint));
        self.say("", None);
    }

    /// Redraw bookkeeping: the activity, and the window title when it changed.
    pub fn status(&mut self, activity: Option<&str>) {
        if let Some(activity) = activity
            && activity != "Ready"
            && activity != "Thinking…"
        {
            self.activity = activity.to_string();
        }
        self.show_title();
    }

    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
        self.draft.placeholder = self.placeholder();
        self.status(None);
    }

    pub fn solo_label(&self) -> String {
        let alias = self.sel("solo");
        let profile = self.prof("solo");
        if !self.auto {
            return if is_empty_profile(&profile) {
                "not set".into()
            } else {
                model_name(&profile, alias.as_deref())
            };
        }
        let Some(route) = self.route.as_ref().filter(|_| !is_empty_profile(&profile)) else {
            return "auto".into();
        };
        let mut label = format!("auto → {}", model_name(&profile, alias.as_deref()));
        if let Some(score) = route.score.filter(|score| *score != 0) {
            label.push_str(&format!(" · {score}/10"));
        }
        label
    }

    /// Follow the planner while the reviewer is the default; a chosen reviewer never changes with it.
    pub fn sync_reviewer(&mut self) {
        if self.reviewer == "off" {
            self.selected.insert("review".into(), None);
            self.profiles.insert("review".into(), json!({}));
        } else if self.reviewer == "planner" {
            let plan = self.sel("plan");
            let profile = self.prof("plan");
            self.selected.insert("review".into(), plan);
            self.profiles.insert("review".into(), profile);
        }
    }

    /// Mark which model is now working, for the spinner and the status line.
    pub fn start_turn(&mut self, role: &str, detail: &str, activity: &str) {
        if let Some(log) = self.turn_log.as_mut() {
            log.push((role.to_string(), detail.to_string(), activity.to_string()));
        }
        self.running = Some(Running {
            role: role.to_string(),
            alias: self.sel(role),
            profile: self.prof(role),
        });
        self.detail = detail.to_string();
        self.activity = activity.to_string();
        self.started = Instant::now();
        self.status(None);
    }

    pub fn reset_turn(&mut self) {
        self.turn_text.clear();
        self.turn_recap = None;
        self.shown = 0;
        self.message_start = 0;
        self.turn_tools.clear();
    }

    // ---- widgets as lines -------------------------------------------------------------------------------

    pub fn thinking_line(&self) -> Option<Line<'static>> {
        if !self.busy {
            return None;
        }
        let elapsed = self.started.elapsed().as_secs();
        let frames: Vec<char> = render::SPINNER.chars().collect();
        let tick = (self.started.elapsed().as_millis() / SPINNER_TICK_MS) as usize;
        let frame = frames[tick % frames.len()];
        let mut line = vec![span(format!("{frame} "), bold(ORANGE))];
        if let Some(running) = &self.running {
            line.push(span(
                model_name(&running.profile, running.alias.as_deref()),
                bold(ORANGE),
            ));
            if !self.simple() {
                line.push(span(
                    format!("  {}", render::role_title(&running.role)),
                    fg(DIM),
                ));
            }
            let activity = if self.activity.is_empty() {
                "thinking"
            } else {
                &self.activity
            };
            line.push(span(format!("  is {activity}"), fg(GRAY)));
            if !self.detail.is_empty() {
                let detail = if self.detail.chars().count() <= 40 {
                    self.detail.clone()
                } else {
                    self.detail.chars().take(39).collect::<String>() + "…"
                };
                line.push(span(format!(" · {detail}"), fg(TEAL)));
            }
        } else {
            let activity = if self.activity.is_empty() {
                "working"
            } else {
                &self.activity
            };
            line.push(span(activity.to_string(), fg(GRAY)));
        }
        line.push(span(format!(" · {elapsed}s"), fg(CORAL)));
        line.push(span("   ctrl+c to cancel", fg(DIM)));
        Some(Line::from(line))
    }

    /// A line for each shell or agent a model left running, with how long it has run.
    pub fn background_lines(&self) -> Vec<Line<'static>> {
        let now = Instant::now();
        let mut rows: Vec<(String, String, Option<f64>)> = self
            .background_tasks
            .iter()
            .map(|task| {
                (
                    format!("{} running", task.kind),
                    task.description.clone(),
                    Some(now.duration_since(task.started).as_secs_f64()),
                )
            })
            .collect();
        rows.extend(
            self.waiting
                .iter()
                .map(|(_, command)| ("waiting for".to_string(), command.clone(), None)),
        );
        let mut lines = Vec::new();
        if rows.is_empty() {
            return lines;
        }
        if !self.busy {
            // Between turns nothing else moves on screen, so say that the work goes on and what happens next.
            lines.push(styled(
                "Still working in the background · the model continues when this finishes",
                fg(GRAY),
            ));
        }
        for (label, text, seconds) in rows.iter().take(3) {
            let mut line = vec![span(format!("⏳ {label}"), bold(TEAL))];
            if !text.is_empty() {
                let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
                line.push(span(format!("  {}", render::clip(&words, 80)), fg(GRAY)));
            }
            if let Some(seconds) = seconds {
                line.push(span(
                    format!(" · {}", ponytail::duration(*seconds)),
                    fg(CORAL),
                ));
            }
            lines.push(Line::from(line));
        }
        if rows.len() > 3 {
            lines.push(styled(
                format!("   and {} more in the background", rows.len() - 3),
                fg(DIM),
            ));
        }
        lines
    }

    /// "2 shells running · 1 agent running" for the status line, or "".
    pub fn background_summary(&self) -> String {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for task in &self.background_tasks {
            *counts.entry(task.kind.as_str()).or_insert(0) += 1;
        }
        let mut parts: Vec<String> = counts
            .iter()
            .rev()
            .map(|(kind, n)| format!("{n} {kind}{} running", if *n == 1 { "" } else { "s" }))
            .collect();
        if !self.waiting.is_empty() {
            parts.push(format!("{} waiting", self.waiting.len()));
        }
        parts.join(" · ")
    }

    /// One entry per piece: the mode badge, every role, the permission mode, the session.
    pub fn status_entries(&self) -> Vec<Line<'static>> {
        let mut entries: Vec<Line<'static>> = Vec::new();
        if !self.simple() {
            let fgc = if self.mode == "review" {
                Color::Rgb(255, 255, 255)
            } else {
                BADGE_DARK
            };
            let style = Style::default()
                .fg(fgc)
                .bg(mode_badge(&self.mode))
                .add_modifier(Modifier::BOLD);
            entries.push(Line::from(vec![span(
                format!(" {} ", self.mode.to_uppercase()),
                style,
            )]));
        }
        let running_role = if self.busy {
            self.running.as_ref().map(|r| r.role.clone())
        } else {
            None
        };
        let roles: Vec<(&str, &str)> = if self.simple() {
            vec![("solo", "model")]
        } else {
            ROLE_TITLES.to_vec()
        };
        for (role, title) in roles {
            let alias = self.sel(role);
            let profile = self.prof(role);
            let has_profile = !is_empty_profile(&profile);
            let mut value = if role == "solo" && (self.auto || !has_profile) {
                self.solo_label()
            } else if !has_profile {
                if role == "review" {
                    "off".to_string()
                } else {
                    "not set".to_string()
                }
            } else if role == "review" && self.reviewer == "planner" {
                format!("{} (= planner)", model_name(&profile, alias.as_deref()))
            } else {
                model_name(&profile, alias.as_deref())
            };
            if let Some(effort) = text_of(&profile, "effort") {
                value.push_str(&format!(" · {effort}"));
            }
            let mut entry = Vec::new();
            if running_role.as_deref() == Some(role) {
                entry.push(span("● ", bold(ORANGE)));
                entry.push(span(format!("{title} "), bold(ORANGE)));
                entry.push(span(value, bold(ORANGE)));
            } else {
                let focus = running_role.is_none() && role == self.mode;
                entry.push(span("○ ", if focus { bold(ORANGE) } else { fg(DIM) }));
                entry.push(span(
                    format!("{title} "),
                    if focus { fg(GRAY) } else { fg(DIM) },
                ));
                let known = has_profile || (role == "solo" && self.auto);
                entry.push(span(
                    value,
                    if focus && known {
                        bold(GRAY)
                    } else if known {
                        fg(GRAY)
                    } else {
                        fg(DIM)
                    },
                ));
            }
            entries.push(Line::from(entry));
        }
        let label = PERMISSION_MODES
            .iter()
            .find(|(key, _, _)| *key == self.permission)
            .map(|(_, label, _)| label.to_lowercase())
            .unwrap_or_default();
        let mut permission = Vec::new();
        if self.permission == "ask" {
            permission.push(span(format!("⏵ {label}"), fg(DIM)));
        } else {
            permission.push(span(
                format!("⏵⏵ {label} on"),
                bold(if self.permission == "full" {
                    CORAL
                } else {
                    ORANGE
                }),
            ));
        }
        permission.push(span("  (shift+tab)", fg(DIM)));
        if !self.queued.is_empty() {
            permission.push(span(format!("   {} queued", self.queued.len()), fg(CORAL)));
        }
        let running = self.background_summary();
        if !running.is_empty() {
            permission.push(span(format!("   ⏳ {running}"), bold(TEAL)));
        }
        if self.verbosity != "normal" {
            permission.push(span(format!("   {}", self.verbosity), fg(DIM)));
        }
        if !self.chrome_on() {
            permission.push(span("   chrome off", fg(DIM)));
        }
        let level = handholding::level(&self.config);
        if !self.simple() && level != handholding::DEFAULT {
            permission.push(span(format!("   handholding {level}"), fg(DIM)));
        }
        entries.push(Line::from(permission));
        if !self.simple() {
            entries.push(Line::from(vec![
                span("session ", fg(DIM)),
                span(self.session_label(), bold(ORANGE)),
            ]));
        }
        if profile_name() != "default" {
            entries.push(Line::from(vec![
                span("profile ", fg(DIM)),
                span(profile_name(), bold(TEAL)),
            ]));
        }
        entries
    }

    pub fn status_lines(&self, width: usize) -> Vec<Line<'static>> {
        pack_entries(&self.status_entries(), width, "  ")
    }

    /// One entry per quota: every configured Claude login and Codex, plus the local models in use.
    /// Maps usage_key -> (label, alias, profile), in the order Python's dict had.
    pub fn usage_groups(&self) -> Vec<UsageGroup> {
        let mut groups: Vec<UsageGroup> = Vec::new();
        let selected: Vec<Option<String>> =
            ["plan", "work"].iter().map(|role| self.sel(role)).collect();
        let aliases = self
            .config
            .get("aliases")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let put = |groups: &mut Vec<UsageGroup>, key: Vec<String>, value| match groups
            .iter_mut()
            .find(|(known, _)| *known == key)
        {
            Some(entry) => entry.1 = value,
            None => groups.push((key, value)),
        };
        for (alias, profile) in &aliases {
            let key = usage_key(profile);
            let is_selected = selected
                .iter()
                .any(|name| name.as_deref() == Some(alias.as_str()));
            if key[0] == "local" && !is_selected {
                continue;
            }
            let label = if key[0] != "local" {
                alias.clone()
            } else {
                model_name(profile, Some(alias))
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string()
            };
            if !groups.iter().any(|(known, _)| *known == key) || is_selected {
                put(
                    &mut groups,
                    key,
                    (label, Some(alias.clone()), profile.clone()),
                );
            }
        }
        for role in ["plan", "work", "review", "solo"] {
            // Models picked by hand may not be saved aliases.
            let profile = self.prof(role);
            let alias = self.sel(role);
            if is_empty_profile(&profile) {
                continue;
            }
            let key = usage_key(&profile);
            if groups.iter().any(|(known, _)| *known == key) {
                continue;
            }
            let label = if key[0] != "local" {
                alias.clone().unwrap_or_default()
            } else {
                model_name(&profile, None)
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string()
            };
            groups.push((key, (label, alias, profile)));
        }
        groups
    }

    pub fn usage_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut entries = Vec::new();
        let running = if self.busy {
            self.running.as_ref().map(|r| usage_key(&r.profile))
        } else {
            None
        };
        for (key, (label, _, _)) in self.usage_groups() {
            let mut line = Vec::new();
            line.push(span(
                label,
                if Some(&key) == running.as_ref() {
                    bold(ORANGE)
                } else {
                    fg(GRAY)
                },
            ));
            line.push(span(" ", Style::default()));
            if key[0] == "local" {
                line.push(span("local", fg(DIM)));
                entries.push(Line::from(line));
                continue;
            }
            match self.quotas.get(&key) {
                None => {
                    line.push(span("──────", fg(render::RULE)));
                    line.push(span(" –", fg(DIM)));
                }
                Some(windows) if windows.is_empty() => line.push(span("no plan limit", fg(DIM))),
                Some(windows) => {
                    let first = &windows[0];
                    let (bar, color) = usage_bar(first.percent, 6);
                    line.extend(bar);
                    line.push(span(
                        format!(" {}% left", remaining(first.percent)),
                        fg(color),
                    ));
                    if !first.window.is_empty() {
                        line.push(span(format!(" {}", first.window), fg(DIM)));
                    }
                    let reset = render::countdown(first.resets_at, None);
                    if !reset.is_empty() {
                        line.push(span(format!(" ↻{reset}"), fg(DIM)));
                    }
                    if let Some(window) = windows.get(1) {
                        let left = remaining(window.percent);
                        line.push(span(" · ", fg(DIM)));
                        line.push(span(format!("{left}%"), fg(render::left_color(left))));
                        if !window.window.is_empty() {
                            line.push(span(format!(" {}", window.window), fg(DIM)));
                        }
                    }
                }
            }
            entries.push(Line::from(line));
        }
        pack_entries(&entries, width, "   ")
    }

    /// Ponytail's band for the current mode's conversation, when Ponytail is installed for its login.
    pub fn band_line(&mut self) -> Option<Line<'static>> {
        let profile = self.prof(&self.role());
        if is_empty_profile(&profile)
            || self.focus_view
            || self.config.get("ponytail_band") == Some(&Value::Bool(false))
            || !self.has_ponytail(&profile)
        {
            return None;
        }
        let alias = self.sel(&self.role()).unwrap_or_default();
        let key = Self::session_key(&alias, &profile, self.purpose().as_deref());
        let mut task = self
            .rec_by_key(&key)
            .and_then(|rec| rec.task.clone())
            .unwrap_or_else(|| {
                ponytail::Task::new(
                    &self.project,
                    engine_of(&profile).unwrap_or(""),
                    text_of(&profile, "model"),
                )
            });
        if engine_of(&profile) == Some("claude") {
            task.level = ponytail::level(&profile, &self.project);
        }
        let mut line = Vec::new();
        for (index, (label, value)) in task.band().into_iter().enumerate() {
            if index > 0 {
                line.push(span("  ·  ", fg(DIM)));
            }
            line.push(span(
                format!("{label} "),
                if index == 0 { bold(ORANGE) } else { fg(DIM) },
            ));
            let tone = if value.ends_with("failed") {
                CORAL
            } else if value.ends_with("passed") {
                TEAL
            } else {
                GRAY
            };
            let style = if label == "tests" {
                fg(tone)
            } else if index == 0 {
                bold(GRAY)
            } else {
                fg(GRAY)
            };
            line.push(span(value, style));
        }
        Some(Line::from(line))
    }

    /// Whether Ponytail is installed and enabled for this login; checked once per login per Donut run.
    pub fn has_ponytail(&mut self, profile: &Value) -> bool {
        let key = usage_key(profile);
        if !self.ponytail_installed.contains_key(&key) {
            let installed = plugins::installed(profile, "ponytail", Some(&self.project));
            self.ponytail_installed.insert(key.clone(), installed);
        }
        self.ponytail_installed[&key]
    }

    // ---- spend ------------------------------------------------------------------------------------------

    /// Add dollars for work by a model on a platform to the one figure. None means the model has no price.
    pub fn count_spend(&mut self, model: Option<&str>, whence: &str, dollars: Option<f64>) {
        let key = (
            model
                .filter(|m| !m.is_empty())
                .unwrap_or("default model")
                .to_string(),
            whence.to_string(),
        );
        if !spend::counted(whence, Some(&self.config)) {
            self.spend_by.entry(key).or_insert(Some(0.0)); // A local model, or a plan counted as free: listed, adds nothing.
        } else if let Some(dollars) = dollars {
            let entry = self.spend_by.entry(key).or_insert(Some(0.0));
            *entry = Some(entry.unwrap_or(0.0) + dollars);
            self.spent += dollars;
        } else {
            self.spend_by.entry(key).or_insert(None);
        }
    }

    /// Add what a session's last turn cost to the amount on the input box, then refresh the box.
    ///
    /// Claude reports a cost; Codex and other models report tokens, which spend.rs prices. `None` for `usage`
    /// only refreshes the label.
    pub fn add_spent(
        &mut self,
        usage: Option<&Value>,
        profile: &Value,
        account: Option<&str>,
        tracker: &mut SpendTracker,
    ) {
        let empty = json!({});
        let usage_value = usage.filter(|u| render::truthy(u)).unwrap_or(&empty);
        let whence = spend::platform(profile, account);
        let model = text_of(profile, "model");
        let mut running = usage_value.get("reported_cost_usd").and_then(Value::as_f64);
        if engine_of(profile) == Some("codex")
            && let Some(total) = usage_value.get("total").filter(|t| render::truthy(t))
        {
            running = spend::cost(model.unwrap_or(""), total, Some(&self.config)); // The thread's total so far.
            if running.is_none() {
                self.count_spend(model, whence, None);
            }
        }
        if let Some(running) = running {
            // A running total for the session; a lower figure means a new provider process started over.
            let previous = tracker.reported;
            self.count_spend(
                model,
                whence,
                Some(if running >= previous {
                    running - previous
                } else {
                    running
                }),
            );
            tracker.reported = running;
            self.estimates.clear(); // The reported figure covers what was estimated.
        } else if render::truthy(usage_value)
            && !matches!(engine_of(profile), None | Some("claude") | Some("codex"))
            && tracker.last_usage.as_ref() != Some(usage_value)
        {
            // A report for one turn: count it once.
            tracker.last_usage = Some(usage_value.clone());
            let dollars = spend::cost(model.unwrap_or(""), usage_value, Some(&self.config));
            self.count_spend(model, whence, dollars);
        }
        if !spend::counted(whence, Some(&self.config)) {
            self.estimates.clear();
        }
        self.refresh_spent_label();
    }

    pub fn refresh_spent_label(&mut self) {
        let estimate: f64 = self.estimates.values().sum();
        // "~" marks a figure that includes an estimate for the turn in progress.
        self.draft.border_subtitle = format!(
            "{}${:.2} spent",
            if estimate != 0.0 { "~" } else { "" },
            self.spent + estimate
        );
    }

    // ---- exchanges and context ---------------------------------------------------------------------------

    pub fn role_of(&self, id: u64) -> String {
        if let Some(rec) = self.rec(id) {
            let name = &rec.key.0;
            let purpose = name
                .rsplit_once('@')
                .map(|(_, purpose)| purpose)
                .unwrap_or("");
            return if matches!(purpose, "solo" | "review") {
                purpose.to_string()
            } else {
                "plan".to_string()
            };
        }
        self.role()
    }

    pub fn running_name(&self) -> String {
        match &self.running {
            Some(running) => model_name(&running.profile, running.alias.as_deref()),
            None => String::new(),
        }
    }

    /// Record an exchange for the whole window, so every other mode and model can be given it.
    pub fn remember_exchange(&mut self, id: u64, text: &str) {
        let tools = self
            .turn_tools
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let answer = format!(
            "{}{}",
            self.turn_text.trim(),
            if tools.is_empty() {
                String::new()
            } else {
                format!("\n(Tools used: {tools})")
            }
        );
        let exchange = Exchange {
            mode: self.role_of(id),
            model: self.running_name(),
            you: text.to_string(),
            answer,
            by: id,
        };
        self.exchanges.push(exchange);
        let seen = self.exchanges.len();
        if let Some(rec) = self.rec_mut(id) {
            rec.donut_seen = seen;
        }
    }

    /// What happened in this window that this session has not seen: other modes' and models' exchanges.
    pub fn context_for(&self, id: u64, limit: usize) -> String {
        let seen = self.rec(id).map_or(0, |rec| rec.donut_seen);
        let missed: Vec<&Exchange> = self
            .exchanges
            .iter()
            .skip(seen)
            .filter(|entry| entry.by != id)
            .collect();
        if missed.is_empty() {
            return String::new();
        }
        let mut lines: Vec<String> = Vec::new();
        let mut size = 0;
        for entry in missed.iter().rev() {
            let place = format!(
                "{} mode{}",
                entry.mode,
                if entry.model.is_empty() {
                    String::new()
                } else {
                    format!(", {}", entry.model)
                }
            );
            let line = format!(
                "[{place}]\nUser: {}\nAssistant: {}",
                entry.you, entry.answer
            );
            if size + line.chars().count() > limit {
                lines.push("(earlier messages left out for length)".into());
                break;
            }
            size += line.chars().count();
            lines.push(line);
        }
        let first = seen == 0;
        let lead = if first {
            "This conversation started before you joined it"
        } else {
            "Other modes and models worked in this Donut window since your last turn"
        };
        lines.reverse();
        format!(
            "(Donut: {lead}. Here is what happened, oldest first. Treat it as shared context: you may be asked \
             about it. The project on disk reflects the work already done.)\n<conversation>\n{}\n</conversation>\n\n",
            lines.join("\n\n")
        )
    }

    /// A resumed conversation's saved recap joins the window's history, so other modes and models can be given
    /// it too.
    pub fn adopt(&mut self, record: &Value, id: u64) {
        let messages = record
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for pair in messages.chunks_exact(2) {
            self.exchanges.push(Exchange {
                mode: text_of(record, "mode").unwrap_or("").to_string(),
                model: text_of(record, "model").unwrap_or("").to_string(),
                you: text_of(&pair[0], "text").unwrap_or("").to_string(),
                answer: text_of(&pair[1], "text").unwrap_or("").to_string(),
                by: id,
            });
        }
        let seen = self.exchanges.len();
        if let Some(rec) = self.rec_mut(id) {
            rec.donut_seen = seen;
        }
    }

    pub fn wake_title(&self, id: u64) -> String {
        let role = self.role_of(id);
        format!(
            "{} ({})",
            model_name(&self.prof(&role), self.sel(&role).as_deref()),
            render::role_title(&role)
        )
    }

    pub fn begin_wake(&mut self, id: u64) {
        let role = self.role_of(id);
        self.busy = true;
        self.active = self.rec(id).map(SessionRec::handle);
        self.reset_turn();
        let title = self.wake_title(id);
        self.say_line(styled(
            format!("  ⏰ {title} · woken by a background task"),
            bold(TEAL),
        ));
        self.start_turn(&role, "", "continuing");
    }

    // ---- events from providers ---------------------------------------------------------------------------

    /// Everything a provider session reports while it works. `key` is the usage key of the session's account,
    /// `source` the session that reported.
    pub fn event(&mut self, kind: &str, text: &Value, key: Option<&[String]>, source: Option<u64>) {
        if kind == "spend" {
            let id = text
                .get("id")
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                .unwrap_or_default();
            self.estimates.insert(
                id,
                text.get("dollars").and_then(Value::as_f64).unwrap_or(0.0),
            );
            self.refresh_spent_label();
            return;
        }
        let buffered =
            source.is_some_and(|id| self.rec(id).is_some_and(|rec| rec.wake_buffer.is_some()));
        let as_text = |value: &Value| {
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string)
        };
        if kind == "wake_start" {
            if let Some(id) = source {
                if self.busy {
                    if let Some(rec) = self.rec_mut(id) {
                        rec.wake_buffer = Some(Vec::new()); // Another turn is on screen; show this one when it ends.
                    }
                } else {
                    self.begin_wake(id);
                }
            }
            return;
        }
        if kind == "wake_end" {
            let Some(id) = source else { return };
            if buffered {
                let held = self
                    .rec_mut(id)
                    .and_then(|rec| rec.wake_buffer.take())
                    .unwrap_or_default();
                let title = self.wake_title(id);
                self.say_line(styled(
                    format!("  ⏰ {title} · woken by a background task"),
                    bold(TEAL),
                ));
                let body = held.concat();
                let body = body.trim();
                self.say_markdown(if body.is_empty() { "(no reply)" } else { body });
            } else if self.active.as_ref().is_some_and(|active| active.id == id) {
                self.deferred.push(Deferred::FinishWake(id));
            }
            return;
        }
        if kind == "background" {
            self.say_line(styled(format!("  ⏰ {}", as_text(text)), fg(TEAL)));
            return;
        }
        if kind == "task" {
            self.track_task(text);
            return;
        }
        if buffered && kind == "text" {
            if let Some(buffer) = source
                .and_then(|id| self.rec_mut(id))
                .and_then(|rec| rec.wake_buffer.as_mut())
            {
                buffer.push(as_text(text));
            }
            return;
        }
        if matches!(kind, "tool" | "tool_result" | "break" | "text") {
            self.flush_thinking();
        }
        match kind {
            "break" => {
                if !self.turn_text.trim().is_empty() {
                    self.turn_text.push_str("\n\n");
                    self.message_start = self.turn_text.len();
                }
            }
            "thinking" => {
                if self.verbosity == "full" {
                    self.thinking_text.push_str(&as_text(text));
                }
            }
            "tool" => {
                if text.is_object() {
                    let name = text.get("name").and_then(Value::as_str);
                    let input = text.get("input").cloned().unwrap_or(Value::Null);
                    let summary = render::tool_summary(name, &input);
                    let entry = format!("{} {}", name.unwrap_or("Tool"), summary);
                    self.turn_tools
                        .push(entry.trim().chars().take(160).collect());
                    if matches!(self.verbosity.as_str(), "verbose" | "full") {
                        self.commit_paragraphs(true);
                        let mut line =
                            vec![span(format!("  ⏺ {}", name.unwrap_or("Tool")), bold(TEAL))];
                        if !summary.is_empty() {
                            line.push(span(format!("  {summary}"), fg(GRAY)));
                        }
                        self.say_line(Line::from(line));
                        self.after_tools = true;
                    }
                }
            }
            "tool_result" => {
                if matches!(self.verbosity.as_str(), "verbose" | "full") && text.is_object() {
                    self.show_tool_result(text);
                }
            }
            "text" => {
                self.turn_text.push_str(&as_text(text));
                self.activity = "writing".into();
                if !self.final_only() {
                    self.commit_paragraphs(false);
                }
                let start = if self.final_only() {
                    self.message_start
                } else {
                    self.shown
                };
                let from = start.max(self.shown).min(self.turn_text.len());
                let pending = crate::recap::hide(self.turn_text[from..].trim());
                self.show_pending(&pending);
            }
            "error" => self.say(&format!("Error: {}", as_text(text)), Some(CORAL)),
            "usage" => {
                if let (Some(key), true) = (key, text.is_array()) {
                    self.quotas.insert(key.to_vec(), windows_from_value(text));
                    self.status(None);
                }
            }
            "command" | "command_output" => {
                let id = self.active.as_ref().map(|a| a.id);
                if let (Some(id), true) = (id, text.is_object())
                    && let Some(task) = self.rec_mut(id).and_then(|rec| rec.task.as_mut())
                {
                    let identity = text.get("id").map(as_text).unwrap_or_default();
                    if kind == "command" {
                        task.command(
                            &identity,
                            text.get("command").and_then(Value::as_str).unwrap_or(""),
                        );
                    } else {
                        task.command_output(
                            &identity,
                            text.get("output").and_then(Value::as_str).unwrap_or(""),
                        );
                    }
                }
            }
            _ => {
                let mut name = as_text(text);
                if let Some(rest) = name.strip_prefix("mcp__") {
                    name = format!("using {}", rest.replace("__", " · "));
                }
                let friendly = match render::activity_name(&name) {
                    Some(friendly) => friendly.to_string(),
                    None if name.starts_with("using ") => name,
                    None => format!("using {name}"),
                };
                let _ = ACTIVITY_NAMES;
                self.status(Some(&friendly));
            }
        }
    }

    /// Background shells and agents a model left running: shown in the status line until they end.
    pub fn track_task(&mut self, task: &Value) {
        let Some(id) = task
            .get("id")
            .filter(|v| render::truthy(v))
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
        else {
            return;
        };
        if !task.is_object() {
            return;
        }
        let state = task.get("state").and_then(Value::as_str).unwrap_or("");
        if state == "running" {
            let kind = task
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("shell")
                .to_string();
            let description = task
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !self.background_tasks.iter().any(|t| t.id == id) {
                let mut line = vec![span(format!("  ⏳ Background {kind} started"), fg(TEAL))];
                if !description.is_empty() {
                    line.push(span(
                        format!("  {}", render::clip(&description, 100)),
                        fg(DIM),
                    ));
                }
                self.say_line(Line::from(line));
                self.background_tasks.push(BackgroundTask {
                    id,
                    kind,
                    description,
                    started: Instant::now(),
                });
            }
        } else if let Some(at) = self.background_tasks.iter().position(|t| t.id == id) {
            let finished = self.background_tasks.remove(at);
            let ok = state == "completed";
            let mut line = vec![span(
                format!(
                    "  {} Background {} {}",
                    if ok { "✓" } else { "✗" },
                    finished.kind,
                    state
                ),
                fg(if ok { TEAL } else { CORAL }),
            )];
            if !finished.description.is_empty() {
                line.push(span(
                    format!("  {}", render::clip(&finished.description, 100)),
                    fg(DIM),
                ));
            }
            self.say_line(Line::from(line));
        }
        self.status(None);
    }

    /// The part of the answer still being written, above the prompt.
    pub fn show_pending(&mut self, pending: &str) {
        self.response.clear();
        if !pending.is_empty() {
            let lines = render::markdown(pending);
            self.response.push_lines(lines);
        }
        self.response.scroll_end();
    }

    /// Write buffered thinking (Full verbosity) as one dim block before what comes next.
    pub fn flush_thinking(&mut self) {
        if self.thinking_text.trim().is_empty() {
            self.thinking_text.clear();
            return;
        }
        let text = self
            .thinking_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        self.thinking_text.clear();
        self.commit_paragraphs(true);
        self.say_line(styled(format!("  ✻ {text}"), italic(DIM)));
    }

    pub fn show_tool_result(&mut self, result: &Value) {
        let output = match result.get("output") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        let output = output.trim_end().to_string();
        let rows: Vec<&str> = output.lines().collect();
        if self.verbosity == "full" && !rows.is_empty() {
            for (index, row) in rows.iter().take(20).enumerate() {
                let mut line = vec![span(if index == 0 { "    ⎿ " } else { "      " }, fg(DIM))];
                line.push(span(render::clip(row, 160), fg(GRAY)));
                self.say_line(Line::from(line));
            }
            if rows.len() > 20 {
                self.say_line(styled(
                    format!("      … {} more lines", rows.len() - 20),
                    fg(DIM),
                ));
            }
            return;
        }
        let failed = result.get("error").is_some_and(render::truthy);
        let mut line = vec![span("    ⎿ ", fg(if failed { CORAL } else { DIM }))];
        if failed {
            line.push(span(
                render::clip(rows.first().copied().unwrap_or("failed"), 120),
                fg(CORAL),
            ));
        } else if rows.is_empty() {
            line.push(span("done", fg(DIM)));
        } else {
            line.push(span(
                format!(
                    "{} line{}",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" }
                ),
                fg(DIM),
            ));
        }
        self.say_line(Line::from(line));
    }

    /// Move finished paragraphs of the streaming answer into the transcript, like Claude Code.
    ///
    /// A paragraph ends at a blank line outside a code block; `final_` also writes the rest.
    pub fn commit_paragraphs(&mut self, final_: bool) {
        let shown = self.shown.min(self.turn_text.len());
        let pending = self.turn_text[shown..].to_string();
        let mut cut = if final_ { pending.len() as isize } else { -1 };
        if !final_ {
            let mut search = pending.len();
            loop {
                match pending[..search].rfind("\n\n") {
                    None => {
                        cut = -1;
                        break;
                    }
                    Some(found) => {
                        let fences = format!("{}{}", &self.turn_text[..shown], &pending[..found])
                            .matches("```")
                            .count();
                        if fences % 2 == 0 {
                            cut = found as isize;
                            break;
                        }
                        search = found;
                    }
                }
            }
        }
        if cut <= 0 {
            return;
        }
        let cut = cut as usize;
        let chunk = pending[..cut].trim().to_string();
        let first = self.turn_text[..shown].trim().is_empty();
        self.shown += cut;
        if !chunk.is_empty() {
            if first || self.after_tools {
                self.blank(); // A blank line after the request, or after a run of tool calls.
            }
            self.after_tools = false;
            self.say_markdown(&chunk);
            self.blank();
        }
    }

    /// End of a turn's text: the rest of the answer into the transcript, the reply area emptied.
    pub fn answer(&mut self) {
        self.flush_thinking();
        if self.final_only() && self.shown < self.message_start {
            self.shown = self.message_start; // Earlier messages of the turn were progress notes: keep only the last.
        }
        let from = self.shown;
        if let Some(recap) = crate::recap::take(&mut self.turn_text, from) {
            self.turn_recap = Some(recap);
        }
        self.commit_paragraphs(true);
        self.response.clear();
        if let Some(recap) = self.turn_recap.clone() {
            self.say_line(Line::from(vec![
                span(format!("  {}  ", crate::recap::LABEL), bold(DIM)),
                span(recap, italic(DIM)),
            ]));
            self.blank();
        }
        if self.turn_text.trim().is_empty() {
            // A turn with no words (the model only started or checked background work) must not look like a hang.
            let running = self.background_summary();
            let note = if running.is_empty() {
                "  The model ended its turn without a reply.".to_string()
            } else {
                format!(
                    "  No reply yet · {running} · the model continues when the background work finishes."
                )
            };
            self.say_line(styled(note, fg(DIM)));
        }
        if self.transcript.follow() {
            self.transcript.scroll_end();
        }
        self.last_answer = self.turn_text.clone();
    }

    // ---- first message names the window ------------------------------------------------------------------

    /// The first chat message of the window names its tab; later messages and /rename are not overridden.
    pub fn name_task(&mut self, text: &str) {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let named = conversations::display_name(&json!({"session": self.session_name}));
        // A named session keeps its name on the tab; only a session still named after its start time takes
        // the message.
        let starts = text.chars().next();
        if named.is_none()
            && self.task_title.is_empty()
            && starts.is_some_and(|c| c != '/' && c != '!')
        {
            self.task_title = render::clip(&text, 40);
            self.show_title();
        }
    }

    /// Name this window's session: its tickets folder becomes .donut/<name>/.
    pub fn rename_session(&mut self, name: &str) {
        let old = self.session_name.clone();
        let new = match workspace::rename(&self.project, old.as_deref(), name) {
            Ok(new) => new,
            Err(error) => {
                self.say(
                    &format!("  Tickets folder not renamed: {error}"),
                    Some(CORAL),
                );
                return;
            }
        };
        self.session_name = Some(new.clone());
        self.task_title.clear();
        let shown: String = name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        conversations::rename_session(&self.project, old.as_deref(), &new, Some(&shown));
        let folder = workspace::relative(&self.project, &self.lanes());
        self.say_line(styled(format!("  Tickets folder · {folder}"), bold(ORANGE)));
        if old.is_none() {
            self.say_dim("  The older swim_lanes/ folder stays as it was.");
        }
        self.status(None);
    }

    /// Line up a session's saved conversations (newest first), one for each mode, to continue when that mode is
    /// next used. `skip` is the mode already resumed.
    pub fn continue_session(&mut self, group: &[Value], skip: Option<&str>) {
        let aliases = self
            .config
            .get("aliases")
            .cloned()
            .unwrap_or_else(|| json!({}));
        self.waiting_resume.clear();
        for record in group {
            let mode = text_of(record, "mode").unwrap_or("");
            let thread = if matches!(mode, "solo" | "review") {
                mode
            } else {
                "plan"
            };
            let role = text_of(record, "role").unwrap_or("");
            if Some(thread) == skip
                || self.waiting_resume.iter().any(|(known, _)| known == thread)
                || !ROLE_TITLES.iter().any(|(name, _)| *name == role)
            {
                continue;
            }
            self.waiting_resume
                .push((thread.to_string(), record.clone()));
            let alias = text_of(record, "alias").unwrap_or("");
            if aliases.get(alias).is_some() && !(role == "solo" && self.auto) {
                let mut profile = aliases[alias].clone();
                if let Some(model) = text_of(record, "model") {
                    profile["model"] = json!(model);
                }
                self.selected
                    .insert(role.to_string(), Some(alias.to_string()));
                self.profiles.insert(role.to_string(), profile);
            }
        }
        self.sync_reviewer();
    }

    // ---- routing -----------------------------------------------------------------------------------------

    /// A new conversation: auto picks its model again from the next first message.
    pub fn forget_route(&mut self) {
        self.route = None;
        if self.auto {
            self.selected.insert("solo".into(), None);
            self.profiles.insert("solo".into(), json!({}));
        }
    }

    pub fn use_step(&mut self, index: usize, score: Option<i64>, harder: i64) -> routing::Step {
        let step = self
            .steps
            .as_ref()
            .expect("steps are found before they are used")[index]
            .clone();
        self.route = Some(Route {
            index: Some(index),
            score,
            harder,
        });
        self.selected
            .insert("solo".into(), Some(step.alias.clone()));
        self.profiles.insert("solo".into(), step.profile.clone());
        self.status(None);
        step
    }

    // ---- slash menu --------------------------------------------------------------------------------------

    /// The command rows of the slash menu. `claude_commands` are the commands a live Claude session reported.
    pub fn slash_rows(
        &self,
        claude_commands: &[Value],
        plugin_commands: &[plugins::Command],
    ) -> Vec<MenuRow> {
        let mut rows: Vec<MenuRow> = COMMANDS
            .iter()
            .filter(|(name, _)| {
                !(self.simple() && COMPLEX_COMMANDS.contains(name))
                    && *name != format!("/{}", self.interface)
            })
            .map(|(name, description)| MenuRow::new(*name, json!(name)).described(description))
            .collect();
        for command in claude_commands {
            let name = match command {
                Value::Object(_) => command.get("name").and_then(Value::as_str),
                other => other.as_str(),
            };
            if let Some(name) = name {
                let full = format!("/{}", name.trim_start_matches('/'));
                if !rows.iter().any(|row| row.label == full) {
                    let description = command
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    rows.push(
                        MenuRow::new(full.clone(), json!(full))
                            .described(&description.chars().take(80).collect::<String>()),
                    );
                }
            }
        }
        // Installed plugins and skills (Ponytail's /ponytail-review, ...), listed before any session is open.
        for command in plugin_commands {
            if !rows.iter().any(|row| row.label == command.name) {
                rows.push(
                    MenuRow::new(command.name.clone(), json!(command.name))
                        .described(&command.description.chars().take(80).collect::<String>()),
                );
            }
        }
        rows
    }

    /// What the prompt's text does to the menu: filter a picker, open or close the slash menu.
    pub fn on_text_changed(&mut self, slash_rows: &dyn Fn() -> Vec<MenuRow>) {
        let text = self.draft.text();
        let recalled = std::mem::take(&mut self.recalled);
        self.draft.shell = text.starts_with('!');
        match self.menu.kind {
            Some(MenuKind::Pick) => {
                self.menu.filter(&text);
            }
            Some(MenuKind::Approve) => {}
            _ => {
                if text.starts_with('/') && !text.chars().any(char::is_whitespace) && !recalled {
                    if !self.menu.is_open() {
                        let rows = slash_rows();
                        self.menu
                            .open(MenuKind::Slash, "Commands", rows, "", None, "");
                    }
                    if !self.menu.filter(&text) {
                        self.menu.close();
                    }
                } else if self.menu.is_open() {
                    self.menu.close();
                }
            }
        }
    }

    /// Up (-1) and Down (+1) through sent messages; false when the key should move the cursor instead.
    pub fn recall(&mut self, step: i32) -> bool {
        let (row, _) = self.draft.cursor_location();
        if step < 0 && row == 0 && !self.history.is_empty() {
            match self.history_at {
                None => {
                    self.history_stash = self.draft.text();
                    self.history_at = Some(self.history.len() - 1);
                }
                Some(at) => self.history_at = Some(at.saturating_sub(1)),
            }
        } else if step > 0 && self.history_at.is_some() && row + 1 == self.draft.line_count() {
            let next = self.history_at.unwrap_or(0) + 1;
            self.history_at = if next >= self.history.len() {
                None
            } else {
                Some(next)
            };
        } else {
            return false;
        }
        self.recalled = true; // one-shot: the change from this load must not open the slash menu
        let text = match self.history_at {
            None => self.history_stash.clone(),
            Some(at) => self.history[at].clone(),
        };
        self.draft.load_text(&text);
        self.draft.move_to_end();
        true
    }

    /// Copy through the terminal (OSC 52); the app also uses this computer's own clipboard tool.
    pub fn osc52(&mut self, text: &str) {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        self.terminal_out.push(format!("\x1b]52;c;{encoded}\x07"));
    }

    /// Text selected in the transcript or the prompt, if any.
    pub fn selected_text(&self) -> Option<String> {
        self.transcript
            .selected_text()
            .or_else(|| self.draft.selected_text())
    }
}

/// The ANSI-free, `\r`-redrawn text a terminal would finally show on a line of command output.
pub fn clean_output_line(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\r', '\n']);
    let text = render::ANSI_ESCAPE.replace_all(text, "");
    text.rsplit('\r').next().unwrap_or("").to_string()
}

/// Stop a process group (the shell command or worker this window started).
pub fn terminate_group(pid: u32) {
    #[cfg(unix)]
    // SAFETY: killpg(2) with a pid and a signal number has no memory-safety requirements.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}
