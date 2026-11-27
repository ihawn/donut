//! Terminal behaviour tests: commands, switching, rendering.
//!
//! Python drove a Textual app with a pilot; here the tests call `App::handle_key` and friends, wait for the
//! worker threads with `settle`, and read the state or a `TestBackend` screen. Provider sessions are fakes made
//! by a session factory, so the real `session()` bookkeeping runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::text::Line;
use serde_json::{Value, json};

use super::app::{AppOptions, Queued};
use super::flows::{App, SessionFactory, SessionRequest, SessionSpec};
use super::widgets::MenuKind;
use crate::conversations::{self, env_lock};
use crate::sessions::{Emit, ModelInfo, Session, UsageWindow};
use crate::workspace;

// ---------------------------------------------------------------------------------------------------------
// Fixtures

struct Env {
    _guard: MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

const ENV_KEYS: [&str; 9] = [
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "DONUT_PROFILE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "DONUT_VERBOSITY",
];

fn env() -> Env {
    let guard = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let saved = ENV_KEYS
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect();
    // Saved /model, /review and permission choices go to a throwaway config folder, never the real one; plugins
    // and skills are read from empty provider folders, never the real ~/.claude or ~/.codex.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join("config"));
        std::env::set_var("XDG_STATE_HOME", temp.path().join("state"));
        std::env::set_var("CLAUDE_CONFIG_DIR", temp.path().join("claude"));
        std::env::set_var("CODEX_HOME", temp.path().join("codex"));
        std::env::remove_var("DONUT_PROFILE");
    }
    Env {
        _guard: guard,
        temp,
        saved,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

impl Env {
    fn project(&self) -> PathBuf {
        let project = self.temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        project
    }
}

/// How a fake session answers.
#[derive(Clone, Copy, PartialEq)]
enum Answer {
    /// "answer N".
    Numbered,
    /// "{model} answered".
    Model,
}

type Hook = Arc<dyn Fn(&Fake, &str) + Send + Sync>;
/// Answers `Session::control`: (number of the fake in creation order, method, params).
type ControlFn = Arc<dyn Fn(usize, &str, &Value) -> Result<Value> + Send + Sync>;
/// (alias, model, purpose, resume id) of a session the factory made.
type Opened = (String, String, Option<String>, Option<String>);
type RunnerCall = (Vec<String>, Value);
type Roles = ((Option<String>, String), Option<(Option<String>, String)>);

struct Fake {
    alias: String,
    purpose: Option<String>,
    family: &'static str,
    emit: Emit,
    profile: Mutex<Value>,
    calls: Mutex<Vec<String>>,
    delay_ms: Arc<AtomicU64>,
    interrupted: AtomicBool,
    modes: Mutex<Vec<String>>,
    switched: Mutex<Vec<Option<String>>>,
    closed: AtomicBool,
    models: Vec<ModelInfo>,
    quota: Option<Vec<UsageWindow>>,
    provider_id: Mutex<Option<String>>,
    read_only: bool,
    resume_id: Option<String>,
    hook: Mutex<Option<Hook>>,
    answer: Answer,
    can_switch: bool,
    info: Value,
    number: usize,
    control_fn: Arc<Mutex<Option<ControlFn>>>,
    controls: Mutex<Vec<(String, Value)>>,
}

impl Fake {
    /// What was sent, without the recap note Donut adds to every chat message (see crate::recap).
    fn calls(&self) -> Vec<String> {
        self.raw_calls()
            .into_iter()
            .map(|call| {
                call.strip_suffix(crate::recap::NOTE)
                    .map_or(call.clone(), str::to_string)
            })
            .collect()
    }

    fn raw_calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn model(&self) -> String {
        self.profile
            .lock()
            .unwrap()
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    fn set_hook(&self, hook: impl Fn(&Fake, &str) + Send + Sync + 'static) {
        *self.hook.lock().unwrap() = Some(Arc::new(hook));
    }
}

impl Session for Fake {
    fn family(&self) -> &'static str {
        self.family
    }

    fn connect(&self) -> Result<()> {
        Ok(())
    }

    fn query(&self, text: &str) -> Result<()> {
        let number = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(text.to_string());
            calls.len()
        };
        let hook = self.hook.lock().unwrap().clone();
        match hook {
            Some(hook) => hook(self, text),
            None => {
                let reply = match self.answer {
                    Answer::Numbered => format!("answer {number}"),
                    Answer::Model => format!("{} answered", self.model()),
                };
                (self.emit)("text", json!(reply));
            }
        }
        let end = Instant::now() + Duration::from_millis(self.delay_ms.load(Ordering::SeqCst));
        while Instant::now() < end && !self.interrupted.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.interrupted.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn interrupt(&self) -> Result<()> {
        self.interrupted.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn set_permission(&self, mode: &str) -> Result<()> {
        self.modes.lock().unwrap().push(mode.to_string());
        Ok(())
    }

    fn quota(&self) -> Option<Vec<UsageWindow>> {
        self.quota.clone()
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(self.models.clone())
    }

    fn control(&self, method: &str, params: Value) -> Result<Value> {
        self.controls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        let answer = self.control_fn.lock().unwrap().clone();
        match answer {
            Some(answer) => answer(self.number, method, &params),
            None => anyhow::bail!("no raw requests"),
        }
    }

    fn account_usage(&self) -> Result<Value> {
        Ok(json!({}))
    }

    fn provider_id(&self) -> Option<String> {
        self.provider_id.lock().unwrap().clone()
    }

    fn usage(&self) -> Value {
        Value::Null
    }

    fn permission(&self) -> String {
        "ask".into()
    }

    fn profile(&self) -> Value {
        self.profile.lock().unwrap().clone()
    }

    fn set_profile(&self, profile: Value) {
        *self.profile.lock().unwrap() = profile;
    }

    fn switch_model(&self, model: Option<&str>) -> Result<()> {
        if !self.can_switch {
            anyhow::bail!("this provider cannot switch models in place");
        }
        self.switched
            .lock()
            .unwrap()
            .push(model.map(str::to_string));
        if let Some(model) = model {
            self.profile.lock().unwrap()["model"] = json!(model);
        }
        Ok(())
    }

    fn info(&self) -> Value {
        self.info.clone()
    }
}

/// Makes `Fake` sessions and remembers each one, in creation order.
#[derive(Clone)]
struct Fakes {
    made: Arc<Mutex<Vec<Arc<Fake>>>>,
    delay_ms: Arc<AtomicU64>,
    counter: Arc<AtomicUsize>,
    models: Vec<ModelInfo>,
    quotas: Arc<Mutex<HashMap<String, Option<Vec<UsageWindow>>>>>,
    answer: Answer,
    can_switch: bool,
    info: Value,
    opened: Arc<Mutex<Vec<Opened>>>,
    control_fn: Arc<Mutex<Option<ControlFn>>>,
}

impl Fakes {
    fn new() -> Fakes {
        Fakes {
            made: Arc::default(),
            delay_ms: Arc::new(AtomicU64::new(0)),
            counter: Arc::new(AtomicUsize::new(0)),
            models: Vec::new(),
            quotas: Arc::default(),
            answer: Answer::Numbered,
            can_switch: false,
            info: json!({}),
            opened: Arc::default(),
            control_fn: Arc::default(),
        }
    }

    fn on_control(
        &self,
        answer: impl Fn(usize, &str, &Value) -> Result<Value> + Send + Sync + 'static,
    ) {
        *self.control_fn.lock().unwrap() = Some(Arc::new(answer));
    }

    fn models(mut self, models: &[(&str, &str)]) -> Fakes {
        self.models = models
            .iter()
            .map(|(id, name)| ModelInfo {
                id: id.to_string(),
                name: name.to_string(),
            })
            .collect();
        self
    }

    fn delay(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    fn factory(&self) -> SessionFactory {
        let fakes = self.clone();
        Arc::new(move |spec: &SessionSpec| {
            let family = match spec.engine.as_str() {
                "claude" => "claude",
                "codex" => "codex",
                _ => "qwen",
            };
            let number = fakes.counter.fetch_add(1, Ordering::SeqCst) + 1;
            let fake = Arc::new(Fake {
                alias: spec.alias.clone(),
                purpose: spec.purpose.clone(),
                family,
                emit: Arc::clone(&spec.emit),
                profile: Mutex::new(spec.profile.clone()),
                calls: Mutex::default(),
                delay_ms: Arc::clone(&fakes.delay_ms),
                interrupted: AtomicBool::new(false),
                modes: Mutex::default(),
                switched: Mutex::default(),
                closed: AtomicBool::new(false),
                models: fakes.models.clone(),
                quota: fakes
                    .quotas
                    .lock()
                    .unwrap()
                    .get(&spec.alias)
                    .cloned()
                    .flatten(),
                provider_id: Mutex::new(Some(
                    spec.options
                        .resume_id
                        .clone()
                        .unwrap_or_else(|| format!("provider-{number}")),
                )),
                read_only: spec.options.read_only,
                resume_id: spec.options.resume_id.clone(),
                hook: Mutex::default(),
                answer: fakes.answer,
                can_switch: fakes.can_switch && family != "qwen",
                info: fakes.info.clone(),
                number,
                control_fn: Arc::clone(&fakes.control_fn),
                controls: Mutex::default(),
            });
            fakes.opened.lock().unwrap().push((
                spec.alias.clone(),
                spec.profile
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                spec.purpose.clone(),
                spec.options.resume_id.clone(),
            ));
            fakes.made.lock().unwrap().push(Arc::clone(&fake));
            Some(fake as Arc<dyn Session>)
        })
    }

    fn all(&self) -> Vec<Arc<Fake>> {
        self.made.lock().unwrap().clone()
    }

    fn last(&self) -> Arc<Fake> {
        self.all().last().cloned().expect("a session was made")
    }

    fn first(&self) -> Arc<Fake> {
        self.all().first().cloned().expect("a session was made")
    }

    fn find(&self, alias: &str, purpose: Option<&str>, model: &str) -> Arc<Fake> {
        self.all()
            .into_iter()
            .find(|f| f.alias == alias && f.purpose.as_deref() == purpose && f.model() == model)
            .unwrap_or_else(|| panic!("no session {alias} {purpose:?} {model}"))
    }

    fn opened(&self) -> Vec<Opened> {
        self.opened.lock().unwrap().clone()
    }
}

fn claude_config() -> Value {
    json!({"aliases": {"claude": {"engine": "claude"}}, "planner": "claude"})
}

fn create(
    env: &Env,
    config: &Value,
    options: AppOptions,
    alias: Option<&str>,
    session: Option<&str>,
) -> App {
    crate::config::save(config).unwrap();
    let app = App::with_options(&env.project(), alias, session, options).unwrap();
    app.set_installer(Arc::new(|_, _| Ok(())));
    app
}

/// A started app with fake sessions.
fn setup(env: &Env, config: Value) -> (App, Fakes) {
    setup_with(env, config, Fakes::new(), AppOptions::test())
}

fn setup_with(env: &Env, config: Value, fakes: Fakes, options: AppOptions) -> (App, Fakes) {
    let app = create(env, &config, options, None, None);
    app.set_session_factory(fakes.factory());
    app.start();
    (app, fakes)
}

fn wait_for(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached: {what}");
}

fn wait(what: &str, condition: impl FnMut() -> bool) {
    wait_for(what, Duration::from_secs(10), condition);
}

/// Wait until every worker is done, or one waits for a menu choice.
fn settle(app: &App) {
    let mut stable = 0;
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        if app.idle() {
            stable += 1;
            if stable >= 4 {
                return;
            }
        } else {
            stable = 0;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    panic!("the app did not settle");
}

fn key(name: &str) -> KeyEvent {
    let mut modifiers = KeyModifiers::empty();
    let mut rest = name;
    while let Some((prefix, tail)) = rest.split_once('+') {
        if tail.is_empty() {
            break;
        }
        match prefix {
            "ctrl" => modifiers |= KeyModifiers::CONTROL,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "alt" => modifiers |= KeyModifiers::ALT,
            _ => break,
        }
        rest = tail;
    }
    let code = match rest {
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "backspace" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "f2" => KeyCode::F(2),
        "f3" => KeyCode::F(3),
        other => KeyCode::Char(other.chars().next().unwrap()),
    };
    if name == "shift+tab" {
        return KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
    }
    let mut event = KeyEvent::new(code, modifiers);
    event.kind = KeyEventKind::Press;
    event
}

fn press(app: &App, keys: &[&str]) {
    for name in keys {
        app.handle_key(key(name));
    }
}

/// Type a line into the prompt and press Enter, without waiting.
fn send_now(app: &App, text: &str) {
    app.st().draft.clear();
    app.type_text(text);
    press(app, &["enter"]);
}

/// Type a line into the prompt and press Enter, then wait for the work to settle.
fn send(app: &App, text: &str) {
    send_now(app, text);
    settle(app);
}

fn body(app: &App) -> String {
    app.st().transcript.body()
}

fn status_text(app: &App) -> String {
    plain_lines(&app.st().status_lines(100))
}

fn usage_text(app: &App) -> String {
    plain_lines(&app.st().usage_lines(100))
}

fn plain_lines(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn menu_open(app: &App) -> bool {
    app.st().menu.is_open()
}

fn labels(app: &App) -> Vec<String> {
    app.st()
        .menu
        .visible_rows
        .iter()
        .map(|row| row.label.clone())
        .collect()
}

fn values(app: &App) -> Vec<Value> {
    app.st()
        .menu
        .visible_rows
        .iter()
        .map(|row| row.value.clone())
        .collect()
}

fn draft_text(app: &App) -> String {
    app.st().draft.text()
}

/// Draw the app on a test terminal and return its screen, one string per row.
fn screen(app: &App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn mouse(app: &App, kind: MouseEventKind, column: u16, row: u16) {
    app.handle_mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    });
}

fn load_config() -> Value {
    crate::config::load().unwrap()
}

fn spawn_approval(
    app: &App,
    name: &'static str,
    details: Value,
) -> std::sync::mpsc::Receiver<crate::sessions::Decision> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = sender.send(app.approve(name, &details));
    });
    receiver
}

fn recorded(app: &App) -> Arc<Mutex<Vec<String>>> {
    let sent: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&sent);
    app.set_executor(Arc::new(move |text: &str| {
        sink.lock().unwrap().push(text.to_string())
    }));
    sent
}

fn queued_texts(app: &App) -> Vec<String> {
    app.st()
        .queued
        .iter()
        .filter_map(|item| match item {
            Queued::Text(text) => Some(text.clone()),
            Queued::Wake(_) => None,
        })
        .collect()
}

fn history_config(extra: Value) -> Value {
    let mut config = json!({
        "aliases": {
            "claude": {"engine": "claude", "model": "claude-opus-5-5"},
            "local": {"engine": "ollama", "model": "qwen3.6"},
        },
        "planner": "claude",
        "default_model": "local",
    });
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        config[key] = value;
    }
    config
}

// ---------------------------------------------------------------------------------------------------------
// TerminalTests

#[test]
fn test_terminal_commands_and_mode_picker() {
    let env = env();
    let config = json!({
        "aliases": {
            "claude": {"engine": "claude", "model": "sonnet"},
            "claude-second": {"engine": "claude", "model": "sonnet"},
        },
        "planner": "claude",
        "default_model": "claude-second",
    });
    let (app, _) = setup(&env, config);
    assert_eq!(app.st().sel("plan").as_deref(), Some("claude"));
    assert_eq!(app.st().sel("work").as_deref(), Some("claude-second"));
    app.execute("/help");
    settle(&app);
    assert!(!app.busy());
    app.execute("/help this isn't a quotation"); // An apostrophe must not stop the command.
    settle(&app);
    assert!(!body(&app).contains("No closing quotation"));
    assert!(body(&app).contains("/plan · /work"));
    app.execute("/work");
    settle(&app);
    assert!(menu_open(&app));
    assert_eq!(app.st().menu.kind, Some(MenuKind::Pick));
    let commands: Vec<String> = app
        .st()
        .menu
        .visible_rows
        .iter()
        .map(|row| row.command.clone())
        .collect();
    assert_eq!(
        commands,
        [
            "/work next 1",
            "/work next 3",
            "/work next all",
            "/work next 1 checkpoint",
            "/work next all checkpoint"
        ]
    );
    let rendered = app.st().menu.row_text(1);
    assert!(rendered.contains("Next 3 tickets"), "{rendered}");
    assert!(rendered.trim_end().ends_with("/work next 3"), "{rendered}");
    assert!(app.st().menu.subtitle.contains("/work next N"));
    press(&app, &["escape"]);
    settle(&app);
    assert!(!menu_open(&app));
    assert!(!app.busy());
    assert_eq!(app.st().mode, "plan"); // /work returns to planning so follow-ups are accepted.
}

#[test]
fn test_model_picker_keeps_account_and_model_together() {
    let env = env();
    let config =
        json!({"aliases": {"claude": {"engine": "claude"}, "claude-second": {"engine": "claude"}}});
    let (app, _) = setup_with(
        &env,
        config,
        Fakes::new().models(&[("sonnet", "Sonnet 5.5")]),
        AppOptions::test(),
    );
    app.execute("/model");
    settle(&app);
    assert_eq!(
        labels(&app),
        ["claude  Sonnet 5.5", "claude-second  Sonnet 5.5"]
    );
    press(&app, &["down", "enter"]);
    settle(&app);
    assert_eq!(app.st().sel("plan").as_deref(), Some("claude-second"));
    assert_eq!(app.st().prof("plan")["model"], "sonnet");
    assert_eq!(app.st().sel("work").as_deref(), Some("claude"));
}

// ---------------------------------------------------------------------------------------------------------
// DraftTests

#[test]
fn test_enter_sends_and_newline_keys_insert() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let sent = recorded(&app);
    app.type_text("first");
    press(&app, &["ctrl+j"]);
    app.type_text("second");
    press(&app, &["shift+enter"]);
    app.type_text("third");
    assert_eq!(draft_text(&app), "first\nsecond\nthird");
    press(&app, &["enter"]);
    assert_eq!(*sent.lock().unwrap(), ["first\nsecond\nthird"]);
    assert_eq!(draft_text(&app), "");
    app.type_text("alt");
    press(&app, &["alt+enter"]);
    assert_eq!(draft_text(&app), "alt\n");
}

#[test]
fn test_up_down_walk_history() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    recorded(&app);
    for word in ["one", "two"] {
        app.type_text(word);
        press(&app, &["enter"]);
    }
    app.type_text("draft");
    for (name, want) in [
        ("up", "two"),
        ("up", "one"),
        ("up", "one"),
        ("down", "two"),
        ("down", "draft"),
    ] {
        press(&app, &[name]);
        assert_eq!(draft_text(&app), want);
    }
    app.st().draft.load_text("a\nb");
    app.st().draft.move_cursor((1, 0));
    press(&app, &["up"]);
    assert_eq!(app.st().draft.cursor_location().0, 0);
    assert_eq!(draft_text(&app), "a\nb");
}

#[test]
fn test_recalled_slash_entry_does_not_open_menu() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    recorded(&app);
    for word in ["one", "/focus "] {
        // The trailing space keeps the menu closed so Enter sends.
        app.type_text(word);
        press(&app, &["enter"]);
    }
    press(&app, &["up"]);
    assert_eq!(draft_text(&app), "/focus");
    assert!(!menu_open(&app));
    press(&app, &["up"]);
    assert_eq!(draft_text(&app), "one");
}

#[test]
fn test_slash_opens_menu_after_recall() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    recorded(&app);
    app.type_text("one");
    press(&app, &["enter"]);
    press(&app, &["up"]);
    app.st().draft.clear();
    app.type_text("/");
    assert!(menu_open(&app));
}

#[test]
fn test_ctrl_c_reaches_cancel_while_editing() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    app.type_text("draft");
    press(&app, &["ctrl+c"]);
    assert!(app.is_cancelled());
    assert_eq!(draft_text(&app), "draft");
}

#[test]
fn test_welcome_logo_opens_the_transcript() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let lines = app.st().transcript.plain();
    assert!(lines[0].contains("_.------"), "{}", lines[0]);
    assert!(
        lines
            .iter()
            .take(3)
            .any(|line| line.contains("do the thing."))
    );
    assert!(lines.iter().any(|line| line.contains("DONUT")));
}

#[test]
fn test_the_input_box_shows_what_the_turns_cost() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    assert_eq!(app.st().draft.border_subtitle, "$0.00 spent");
    let profile = json!({});
    let mut first = super::app::SpendTracker::default();
    let mut second = super::app::SpendTracker::default();
    let add = |usage: Value, tracker: &mut super::app::SpendTracker| {
        app.st().add_spent(Some(&usage), &profile, None, tracker);
    };
    add(json!({"reported_cost_usd": 0.25}), &mut first);
    add(json!({}), &mut second); // No cost reported (Codex, Ollama): nothing is added.
    add(json!({"reported_cost_usd": 0.75}), &mut first); // A running total: only the difference counts.
    add(json!({"reported_cost_usd": 0.10}), &mut first); // A new provider process starts over.
    assert_eq!(app.st().draft.border_subtitle, "$0.85 spent");
    // During a turn each response adds an estimate; the same response reported twice counts once.
    app.event("spend", &json!({"id": "a", "dollars": 0.10}), None, None);
    app.event("spend", &json!({"id": "a", "dollars": 0.10}), None, None);
    app.event("spend", &json!({"id": "b", "dollars": 0.05}), None, None);
    assert_eq!(app.st().draft.border_subtitle, "~$1.00 spent");
    app.st().add_spent(
        Some(&json!({"reported_cost_usd": 0.30})),
        &profile,
        None,
        &mut first,
    ); // The reported cost replaces the estimate.
    assert_eq!(app.st().draft.border_subtitle, "$1.05 spent");
}

// ---------------------------------------------------------------------------------------------------------
// DroppedImageTests and PastedImageTests (the pure parts are tested in render.rs)

#[test]
fn test_dropped_paste_inserts_quoted_path() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let shot = dunce::canonicalize(env.project())
        .unwrap()
        .join("shot one.png");
    std::fs::write(&shot, b"x").unwrap();
    let shot = shot.to_string_lossy().into_owned();
    app.handle_paste(&format!("\"{shot}\"")); // The quoted form every platform pastes.
    assert_eq!(draft_text(&app), "[screenshot1] ");
    app.handle_paste(&shot); // The same image again keeps its number.
    assert_eq!(draft_text(&app), "[screenshot1] [screenshot1] ");
    assert_eq!(
        super::render::expand_images("look [screenshot1]", &app.st().images),
        format!("look {}", super::render::quote_path(&shot))
    );
    app.st().draft.load_text("");
    app.handle_paste("hello world");
    assert_eq!(draft_text(&app), "hello world");
}

/// Point `std::env::temp_dir` somewhere else: TMPDIR on Unix, TMP and TEMP on Windows.
fn set_temp_dir(path: &Path) {
    for key in ["TMPDIR", "TMP", "TEMP"] {
        unsafe { std::env::set_var(key, path) };
    }
}

#[test]
fn test_pasted_image_paste_inserts_saved_path() {
    use base64::Engine;
    let env = env();
    set_temp_dir(env.temp.path());
    let (app, _) = setup(&env, claude_config());
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend(vec![b'0'; 200]);
    let png = base64::engine::general_purpose::STANDARD.encode(&bytes);
    app.handle_paste(&png);
    assert_eq!(draft_text(&app), "[screenshot1] ");
    let path = &app.st().images[0].clone();
    assert!(path.ends_with(".png"));
    assert!(
        std::fs::read(path)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    app.st().draft.load_text("");
    // A temp folder that cannot be made: the text goes in unchanged.
    let blocker = env.temp.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    set_temp_dir(&blocker.join("inner"));
    app.handle_paste(&png);
    assert_eq!(draft_text(&app), png);
}

// ---------------------------------------------------------------------------------------------------------
// SlashMenuTests

#[test]
fn test_typing_slash_filters_inline_and_enter_runs_the_top_match() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let sent = recorded(&app);
    press(&app, &["/"]);
    assert!(menu_open(&app));
    assert_eq!(app.st().menu.visible_rows[0].label, "/solo");
    press(&app, &["m", "o"]);
    assert_eq!(labels(&app), ["/model"]);
    assert_eq!(app.st().menu.highlighted(), Some(0));
    press(&app, &["enter"]);
    assert_eq!(*sent.lock().unwrap(), ["/model"]);
    assert!(!menu_open(&app));
    assert_eq!(draft_text(&app), "");
}

#[test]
fn test_arrows_move_tab_completes_and_escape_closes() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let sent = recorded(&app);
    press(&app, &["/", "c"]);
    assert_eq!(labels(&app)[..3], ["/context", "/clear", "/copy"]);
    press(&app, &["down", "tab"]);
    assert_eq!(draft_text(&app), "/clear ");
    assert!(!menu_open(&app));
    assert!(sent.lock().unwrap().is_empty());
    app.st().draft.clear();
    press(&app, &["/", "p"]);
    assert!(menu_open(&app));
    press(&app, &["escape"]);
    assert!(!menu_open(&app));
    assert_eq!(draft_text(&app), "/p");
}

#[test]
fn test_arguments_close_the_menu_so_enter_sends_the_line() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let sent = recorded(&app);
    press(&app, &["/", "w", "o", "r", "k", "space", "n"]);
    assert!(!menu_open(&app));
    press(&app, &["enter"]);
    assert_eq!(*sent.lock().unwrap(), ["/work n"]);
}

#[test]
fn test_unknown_command_hides_the_menu() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    recorded(&app);
    press(&app, &["/", "z", "z"]);
    assert!(!menu_open(&app));
}

#[test]
fn test_tool_approval_is_an_inline_panel_with_number_keys() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    use crate::sessions::Decision;
    let answer = spawn_approval(
        &app,
        "Bash",
        json!({"command": "ls -la", "description": "List files"}),
    );
    wait("menu open", || menu_open(&app));
    assert_eq!(app.st().menu.kind, Some(MenuKind::Approve));
    assert_eq!(app.st().menu.title, "Bash command");
    assert_eq!(values(&app), [json!("yes"), json!("always"), json!("no")]);
    let details = plain_lines(&app.st().menu.details.clone());
    assert!(details.contains("$ ls -la"), "{details}");
    assert!(!details.contains('{')); // Not a raw JSON dump.
    press(&app, &["3"]);
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(5)).unwrap(),
        Decision::No
    );
    let answer = spawn_approval(&app, "Bash", json!({"command": "ls"}));
    wait("menu open", || menu_open(&app));
    press(&app, &["enter"]);
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(5)).unwrap(),
        Decision::Yes
    );
    let answer = spawn_approval(&app, "Bash", json!({"command": "pwd"}));
    wait("menu open", || menu_open(&app));
    press(&app, &["2"]);
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(5)).unwrap(),
        Decision::Always
    );
    assert_eq!(
        app.approve("Bash", &json!({"command": "whoami"})),
        Decision::Always
    ); // No second prompt.
    assert!(!menu_open(&app));
    assert!(
        body(&app)
            .lines()
            .any(|line| line.contains("Denied Bash command") && line.contains("ls -la"))
    );
}

#[test]
fn test_escape_denies_and_keeps_the_draft() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    app.st().draft.insert("half-typed follow-up");
    let answer = spawn_approval(&app, "Write", json!({"file_path": "a.txt", "content": "x"}));
    wait("menu open", || menu_open(&app));
    press(&app, &["escape"]);
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(5)).unwrap(),
        crate::sessions::Decision::No
    );
    assert_eq!(draft_text(&app), "half-typed follow-up");
}

// ---------------------------------------------------------------------------------------------------------
// ConversationTests

#[test]
fn test_follow_ups_reach_the_same_session() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    send(&app, "first");
    send(&app, "second");
    assert_eq!(fakes.all().len(), 1);
    let calls = fakes.first().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].ends_with("second"));
}

#[test]
fn test_display_only_commands_run_at_once_during_a_turn() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    fakes.delay(1500);
    send_now(&app, "a long task");
    wait("a turn is active", || app.st().active.is_some());
    for text in ["/help", "/usage", "and then this"] {
        app.st().draft.clear();
        app.type_text(text);
        press(&app, &["enter"]);
    }
    wait("display-only answers", || {
        let text = body(&app);
        text.contains("/plan · /work")
            && text.contains("Latest figures; they refresh when the current turn ends.")
    });
    assert_eq!(queued_texts(&app), ["and then this"]); // A message to the model still waits.
    assert!(app.busy()); // The turn carries on.
    assert_eq!(fakes.first().calls().len(), 1);
    wait_for("the queued message ran", Duration::from_secs(10), || {
        fakes.first().calls().len() == 2
    });
}

#[test]
fn test_messages_sent_during_a_turn_are_queued_then_run() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    fakes.delay(1500);
    send_now(&app, "first");
    wait("first call", || {
        fakes.all().first().is_some_and(|f| f.calls().len() == 1)
    });
    assert!(app.busy());
    send_now(&app, "follow-up while busy");
    wait("queued", || queued_texts(&app) == ["follow-up while busy"]);
    assert_eq!(draft_text(&app), "");
    fakes.delay(0);
    wait("second call", || {
        fakes.first().calls().len() == 2 && !app.busy()
    });
    assert!(fakes.first().calls()[1].ends_with("follow-up while busy"));
    assert!(queued_texts(&app).is_empty());
}

#[test]
fn test_cancel_returns_queued_messages_to_the_prompt() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    fakes.delay(1500);
    send_now(&app, "first");
    wait("first call", || {
        fakes.all().first().is_some_and(|f| f.calls().len() == 1)
    });
    send_now(&app, "keep me");
    wait("queued", || queued_texts(&app) == ["keep me"]);
    press(&app, &["ctrl+c"]);
    assert!(queued_texts(&app).is_empty());
    assert_eq!(draft_text(&app), "keep me");
    wait("the turn ended", || !app.busy());
}

#[test]
fn test_free_text_in_work_mode_goes_to_the_planner() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    app.st().mode = "work".into();
    send(&app, "what about tests?");
    assert_eq!(fakes.first().calls().len(), 1);
    assert_eq!(app.st().mode, "plan");
}

#[test]
fn test_permissions_picker_auto_toggle_and_shift_tab() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    send(&app, "first");
    assert_eq!(app.st().permission, "ask");
    send(&app, "/permissions");
    assert_eq!(
        values(&app),
        [
            json!("ask"),
            json!("accept_edits"),
            json!("auto"),
            json!("full")
        ]
    );
    press(&app, &["down", "down", "enter"]);
    settle(&app);
    assert_eq!(app.st().permission, "auto");
    let fake = fakes.first();
    assert_eq!(*fake.modes.lock().unwrap(), ["auto"]);
    assert_eq!(load_config()["permission_mode"], "auto");
    send(&app, "/auto");
    assert_eq!(app.st().permission, "ask");
    press(&app, &["shift+tab"]);
    settle(&app);
    assert_eq!(app.st().permission, "accept_edits");
    press(&app, &["shift+tab"]);
    settle(&app);
    assert_eq!(app.st().permission, "auto");
    assert_eq!(
        *fake.modes.lock().unwrap(),
        ["auto", "ask", "accept_edits", "auto"]
    );
    assert!(status_text(&app).contains("auto on"));
}

#[test]
fn test_saved_permission_mode_is_restored() {
    let env = env();
    let config = json!({"aliases": {"claude": {"engine": "claude"}}, "planner": "claude", "permission_mode": "auto"});
    let app = create(&env, &config, AppOptions::test(), None, None);
    assert_eq!(app.st().permission, "auto");
}

// ---------------------------------------------------------------------------------------------------------
// WorkOutputTests

fn failing_runner(app: &App) -> Arc<Mutex<Vec<super::flows::RunnerSpec>>> {
    let seen: Arc<Mutex<Vec<super::flows::RunnerSpec>>> = Arc::default();
    let sink = Arc::clone(&seen);
    app.set_runner(Arc::new(move |spec| {
        sink.lock().unwrap().push(super::flows::RunnerSpec {
            args: spec.args.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
        });
        Err(std::io::Error::other("stop here"))
    }));
    seen
}

fn runner_home(spec: &super::flows::RunnerSpec) -> PathBuf {
    PathBuf::from(
        &spec
            .env
            .iter()
            .find(|(key, _)| key == "XDG_CONFIG_HOME")
            .unwrap()
            .1,
    )
}

fn json_files(home: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.extension().is_some_and(|e| e == "json") {
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(home, home, &mut out);
    out.sort();
    out
}

#[test]
fn test_the_worker_of_a_named_profile_finds_its_configuration() {
    let env = env();
    unsafe { std::env::set_var("DONUT_PROFILE", "personal") };
    let config = json!({"aliases": {"local": {"engine": "ollama", "model": "qwen3"}}, "default_model": "local"});
    let (app, _) = setup(&env, config);
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let sink = Arc::clone(&seen);
    app.set_runner(Arc::new(move |spec| {
        sink.lock().unwrap().push(json_files(&runner_home(spec)));
        Err(std::io::Error::other("stop here"))
    }));
    assert!(app.work_tickets("next 1").is_err());
    assert_eq!(seen.lock().unwrap()[0], ["donut/profiles/personal.json"]);
}

#[test]
fn test_worker_auto_is_saved_restored_and_launched_as_auto() {
    let env = env();
    let config = json!({"aliases": {"claude": {"engine": "claude"}}, "default_model": "claude"});
    let (app, _) = setup(&env, config.clone());
    app.choose_model("work", Some("auto")).unwrap();
    assert_eq!(app.st().sel("work").as_deref(), Some("auto"));
    let profile = app.st().prof("work");
    assert_eq!(
        (profile["model"].as_str(), profile["effort"].as_str()),
        (Some("haiku"), Some("max"))
    );
    assert_eq!(
        load_config()["roles"]["work"],
        json!({"alias": "auto", "model": "haiku"})
    );
    let configs: Arc<Mutex<Vec<RunnerCall>>> = Arc::default();
    let sink = Arc::clone(&configs);
    app.set_runner(Arc::new(move |spec| {
        let home = runner_home(spec);
        let path = home.join("donut").join("config.json");
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        sink.lock().unwrap().push((spec.args.clone(), parsed));
        Err(std::io::Error::other("stop here"))
    }));
    assert!(app.work_tickets("next 1").is_err());
    let (args, seen) = configs.lock().unwrap()[0].clone();
    let at = args.iter().position(|a| a == "-m").unwrap();
    assert_eq!(args[at + 1], "auto");
    assert!(seen["aliases"].get("auto").is_none()); // The worker builds its steps itself.
    assert_eq!(
        seen["aliases"]["claude"]["permission_mode"],
        json!(app.st().permission)
    );
    let again = App::with_options(&env.project(), None, None, AppOptions::test()).unwrap();
    assert_eq!(again.st().sel("work").as_deref(), Some("auto"));
}

fn script_runner(app: &App, script: String) {
    app.set_runner(Arc::new(move |spec| {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&script)
            .envs(spec.env.iter().cloned())
            .current_dir(&spec.cwd);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command.spawn()
    }));
}

#[test]
fn test_progress_updates_the_status_bar_and_output_is_highlighted() {
    let env = env();
    let config = json!({"aliases": {"local": {"engine": "ollama", "model": "qwen3"}}, "default_model": "local"});
    let (app, _) = setup(&env, config);
    app.st().turn_log = Some(Vec::new());
    script_runner(
        &app,
        "printf '%s\\n' 'Preparing local model…' '⠋' '⠙' '⠹' '[1/1] 047_sandbox-wall.md — fresh context' \
         '[qwen3 · Connected · preparing project context]' '[Working · 15s elapsed · waiting for agent output]' \
         '[Working · 30s elapsed · waiting for agent output]' 'Tests: 12 passed'; printf '\\033[32mcolored\\033[0m\\n'"
            .to_string(),
    );
    app.work_tickets("next 1").unwrap();
    let lines = app.st().transcript.plain();
    let text = lines.join("\n");
    assert!(!text.contains("elapsed"), "{text}");
    assert!(
        !lines
            .iter()
            .any(|line| matches!(line.trim(), "⠋" | "⠙" | "⠹"))
    );
    let turns = app.st().turn_log.clone().unwrap();
    assert!(
        turns
            .iter()
            .any(|(role, detail, _)| role == "work" && detail == "047_sandbox-wall.md"),
        "{turns:?}"
    );
    assert!(text.contains("Tests: 12 passed"));
    assert!(text.contains("colored"));
    assert!(!text.contains('\x1b'));
    assert!(text.contains("WORK COMPLETE"));
}

// ---------------------------------------------------------------------------------------------------------
// ActiveModelAndUsageTests

fn active_config() -> Value {
    json!({
        "aliases": {
            "claude-second": {"engine": "claude", "model": "claude-opus-5-5", "claude_config_dir": "/x"},
            "codex": {"engine": "codex", "model": "gpt-6"},
            "local": {"engine": "ollama", "model": "qwen3.6:35b"},
            "other-local": {"engine": "ollama", "model": "llama"},
        },
        "planner": "claude-second",
        "default_model": "local",
    })
}

#[test]
fn test_spinner_and_status_name_the_running_model() {
    let env = env();
    let (app, _) = setup(&env, active_config());
    let idle = status_text(&app);
    assert!(idle.contains("planner claude-opus-5-5"), "{idle}");
    assert!(idle.contains("worker qwen3.6:35b"));
    assert!(app.st().thinking_line().is_none());
    {
        let mut s = app.st();
        s.busy = true;
        s.start_turn("work", "047_wall.md", "working");
    }
    app.event("activity", &json!("Bash"), None, None);
    let spinner = plain_lines(&[app.st().thinking_line().expect("busy")]);
    for expected in [
        "qwen3.6:35b",
        "worker",
        "is running a command",
        "047_wall.md",
        "ctrl+c to cancel",
    ] {
        assert!(spinner.contains(expected), "{spinner}");
    }
    let lines = app.st().status_lines(100);
    let bold_spans: Vec<String> = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| {
            span.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        })
        .map(|span| span.content.to_string())
        .collect();
    assert!(
        bold_spans.iter().any(|text| text.contains("qwen3.6:35b")),
        "{bold_spans:?}"
    );
    assert!(
        !bold_spans
            .iter()
            .any(|text| text.contains("claude-opus-5-5"))
    );
}

#[test]
fn test_spinner_timer_tolerates_shutdown() {
    let env = env();
    let (app, _) = setup(&env, active_config());
    {
        let mut s = app.st();
        s.busy = true;
        s.start_turn("plan", "", "thinking");
    }
    app.action_quit();
    // The timer can still fire once the interface is gone.
    let _ = app.st().thinking_line();
    app.status(Some("writing"));
    let _ = screen(&app, 80, 24);
}

#[test]
fn test_usage_bar_shows_each_account_once() {
    let env = env();
    let (app, _) = setup(&env, active_config());
    let bar = usage_text(&app);
    assert!(bar.contains("claude-second"), "{bar}");
    assert!(bar.contains("codex"));
    assert!(bar.contains("qwen3.6 local"));
    assert!(!bar.contains("llama")); // Only local models in use are listed.
    app.event(
        "usage",
        &json!([{"window": "5h", "percent": 18.0}, {"window": "7d", "percent": 30.0}]),
        Some(&["claude".to_string(), "/x".to_string()]),
        None,
    );
    app.event(
        "usage",
        &json!([{"window": "5h", "percent": 91.0}]),
        Some(&["codex".to_string()]),
        None,
    );
    let bar = usage_text(&app);
    assert!(bar.contains("82% left 5h · 70% 7d"), "{bar}");
    assert!(bar.contains("9% left 5h"));
    assert!(bar.contains('━'));
}

// ---------------------------------------------------------------------------------------------------------
// UsageCommandTests

fn usage_config() -> Value {
    json!({
        "aliases": {
            "claude": {"engine": "claude"},
            "codex": {"engine": "codex"},
            "api": {"engine": "claude", "claude_config_dir": "/api"},
        },
        "planner": "claude",
    })
}

#[test]
fn test_usage_command_shows_what_is_left_for_each_account() {
    let env = env();
    let soon = conversations::now_seconds() + 2.0 * 3600.0 + 600.0;
    let fakes = Fakes::new();
    {
        let mut quotas = fakes.quotas.lock().unwrap();
        quotas.insert(
            "claude".into(),
            Some(vec![
                UsageWindow {
                    window: "5h".into(),
                    percent: 18.0,
                    resets_at: Some(soon),
                },
                UsageWindow {
                    window: "7d".into(),
                    percent: 30.0,
                    resets_at: None,
                },
            ]),
        );
        quotas.insert("codex".into(), None);
        quotas.insert("api".into(), Some(Vec::new()));
    }
    let (app, _) = setup_with(&env, usage_config(), fakes, AppOptions::test());
    app.execute("/usage");
    settle(&app);
    let text = body(&app);
    let bar = usage_text(&app);
    assert!(!text.contains("Error"), "{text}");
    assert!(text.contains("82% left  5h  resets in 2h"), "{text}");
    assert!(text.contains("70% left  7d"));
    assert!(text.contains("codex   usage unavailable"));
    assert!(text.contains("api   no plan limits (API billing)"));
    assert!(bar.contains("82% left 5h ↻2h"), "{bar}");
    assert!(bar.contains("api no plan limit"));
}

#[test]
fn test_refresh_without_connect_only_asks_open_sessions() {
    let env = env();
    let (app, fakes) = setup(&env, usage_config());
    app.refresh_quotas(false);
    assert!(fakes.all().is_empty(), "must not start a session");
    assert!(app.st().quotas.is_empty());
}

// ---------------------------------------------------------------------------------------------------------
// ReviewerTests

fn reviewer_config(extra: Value) -> Value {
    let mut config = json!({
        "aliases": {
            "claude": {"engine": "claude", "model": "claude-opus-5-5"},
            "codex": {"engine": "codex", "model": "gpt-6.1"},
            "local": {"engine": "ollama", "model": "qwen3.6"},
        },
        "planner": "claude",
        "default_model": "local",
    });
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        config[key] = value;
    }
    config
}

fn model_fakes() -> Fakes {
    Fakes::new().models(&[("sonnet-5-5", "Sonnet 5.5"), ("opus-5-5", "Opus 5.5")])
}

fn pick(app: &App, command: &str, label: &str) {
    app.execute(command);
    settle(app);
    let rows = labels(app);
    let at = rows
        .iter()
        .position(|row| row == label)
        .unwrap_or_else(|| panic!("{label} not in {rows:?}"));
    for _ in 0..at {
        press(app, &["down"]);
    }
    press(app, &["enter"]);
    settle(app);
}

fn roles_of(app: &App) -> Roles {
    let s = app.st();
    let model = |role: &str| {
        s.prof(role)
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    (
        (s.sel("plan"), model("plan")),
        if super::app::is_empty_profile(&s.prof("review")) {
            None
        } else {
            Some((s.sel("review"), model("review")))
        },
    )
}

fn pair(alias: &str, model: &str) -> (Option<String>, String) {
    (Some(alias.to_string()), model.to_string())
}

#[test]
fn test_reviewer_follows_the_planner_until_chosen_then_they_stay_independent() {
    let env = env();
    let (app, _) = setup_with(
        &env,
        reviewer_config(json!({})),
        model_fakes(),
        AppOptions::test(),
    );
    assert!(status_text(&app).contains("reviewer claude-opus-5-5 (= planner)"));
    // Default: changing the planner moves the reviewer with it.
    pick(&app, "/planner", "claude  Sonnet 5.5");
    assert_eq!(
        roles_of(&app),
        (
            pair("claude", "sonnet-5-5"),
            Some(pair("claude", "sonnet-5-5"))
        )
    );
    // Choosing a reviewer, down to the model, leaves the planner alone.
    pick(&app, "/reviewer", "codex  Opus 5.5");
    assert_eq!(
        roles_of(&app),
        (
            pair("claude", "sonnet-5-5"),
            Some(pair("codex", "opus-5-5"))
        )
    );
    assert!(status_text(&app).contains("reviewer opus-5-5"));
    // And changing the planner now leaves the chosen reviewer alone.
    pick(&app, "/planner", "claude  Opus 5.5");
    assert_eq!(
        roles_of(&app),
        (pair("claude", "opus-5-5"), Some(pair("codex", "opus-5-5")))
    );
    let saved = load_config();
    assert_eq!(
        saved["roles"]["plan"],
        json!({"alias": "claude", "model": "opus-5-5"})
    );
    assert_eq!(
        saved["roles"]["review"],
        json!({"alias": "codex", "model": "opus-5-5"})
    );
    app.execute("/reviewer off");
    settle(&app);
    assert!(status_text(&app).contains("reviewer off"));
    assert_eq!(load_config()["reviewer"], "off");
    app.execute("/reviewer planner");
    settle(&app);
    assert_eq!(roles_of(&app).1, Some(pair("claude", "opus-5-5")));
    let saved = load_config();
    assert_eq!(
        saved["roles"],
        json!({"plan": {"alias": "claude", "model": "opus-5-5"}})
    );
    assert!(saved.get("reviewer").is_none());
}

#[test]
fn test_choices_stick_across_restarts() {
    let env = env();
    let base = json!({
        "aliases": {"claude": {"engine": "claude", "model": "claude-opus-5-5"}, "codex": {"engine": "codex"}},
        "planner": "claude",
        "default_model": "claude",
    });
    crate::config::save(&base).unwrap();
    crate::config::remember_role("plan", "claude", Some("sonnet-5-5"), None).unwrap();
    crate::config::remember_role("review", "codex", Some("gpt-6.1"), None).unwrap();
    let app = App::with_options(&env.project(), None, None, AppOptions::test()).unwrap();
    assert_eq!(
        roles_of(&app),
        (pair("claude", "sonnet-5-5"), Some(pair("codex", "gpt-6.1")))
    );
    assert_eq!(app.st().reviewer, "chosen");
}

#[test]
fn test_work_passes_the_reviewer_and_the_spinner_follows_review_rounds() {
    let env = env();
    let (app, _) = setup(&env, reviewer_config(json!({"reviewer": "codex"})));
    app.st().turn_log = Some(Vec::new());
    app.set_runner(Arc::new(|spec| {
        let home = runner_home(spec);
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("donut/config.json")).unwrap())
                .unwrap();
        let model_of = |key: &str| {
            let alias = parsed[key].as_str().unwrap_or("None").to_string();
            let model = parsed["aliases"][&alias]["model"]
                .as_str()
                .unwrap_or("None")
                .to_string();
            format!("{alias} {model}")
        };
        let script = format!(
            "echo 'CONFIG {}'; echo '[1/1] 001_jump.md — fresh context'; \
             echo '[review 1/3] 001_jump.md — gpt-6.1, fresh context'; echo 'RECOVERY {}'; \
             echo '[recover] 001_jump.md — claude-opus-5-5, fresh context'; sleep 0.3; \
             echo 'VERDICT: APPROVED'; echo 'Review: approved'",
            model_of("reviewer"),
            model_of("recovery")
        );
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(script).current_dir(&spec.cwd);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command.spawn()
    }));
    app.work_tickets("next 1").unwrap();
    let lines = app.st().transcript.plain();
    assert!(
        lines
            .iter()
            .any(|line| line == "CONFIG donut-reviewer gpt-6.1"),
        "{lines:?}"
    );
    let turns: Vec<(String, String)> = app
        .st()
        .turn_log
        .clone()
        .unwrap()
        .into_iter()
        .map(|(r, _, a)| (r, a))
        .collect();
    let has =
        |role: &str, activity: &str| turns.iter().position(|(r, a)| r == role && a == activity);
    assert!(has("review", "reviewing").is_some(), "{turns:?}");
    assert!(
        lines
            .iter()
            .any(|line| line == "RECOVERY donut-planner claude-opus-5-5"),
        "{lines:?}"
    );
    assert!(has("plan", "recovering a blocked ticket").is_some());
    assert!(has("work", "working") < has("review", "reviewing"));
}

// ---------------------------------------------------------------------------------------------------------
// ModeAndRoleTests

fn role_app(env: &Env) -> (App, Fakes) {
    setup_with(
        env,
        history_config(json!({})),
        Fakes::new().models(&[("claude-sonnet-5-5", "Sonnet 5.5")]),
        AppOptions::test(),
    )
}

#[test]
fn test_donut_starts_in_solo_and_the_first_message_asks_for_a_soloist() {
    let env = env();
    let options = AppOptions {
        start_mode: "solo".into(),
        ..AppOptions::test()
    };
    let (app, fakes) = setup_with(
        &env,
        history_config(json!({})),
        Fakes::new().models(&[("claude-sonnet-5-5", "Sonnet 5.5")]),
        options,
    );
    assert_eq!(app.st().mode, "solo");
    assert!(status_text(&app).contains("SOLO"));
    send(&app, "rename jump() to leap()");
    assert!(menu_open(&app)); // No soloist yet, so the first message asks.
    assert!(app.st().menu.title.contains("soloist"));
    press(&app, &["enter"]);
    settle(&app);
    assert_eq!(
        load_config()["roles"]["solo"],
        json!({"alias": "claude", "model": "claude-sonnet-5-5"})
    );
    let solo = fakes.find("claude", Some("solo"), "claude-sonnet-5-5");
    assert_eq!(solo.calls(), ["rename jump() to leap()"]);
    send(&app, "now rename land too");
    assert!(!menu_open(&app)); // Chosen once; not asked again.
    assert_eq!(
        solo.calls(),
        ["rename jump() to leap()", "now rename land too"]
    );
}

#[test]
fn test_a_model_alias_on_the_command_line_is_the_soloist_too() {
    let env = env();
    let options = AppOptions {
        start_mode: "solo".into(),
        ..AppOptions::test()
    };
    let app = create(
        &env,
        &history_config(json!({})),
        options,
        Some("local"),
        None,
    );
    let s = app.st();
    assert_eq!(
        (
            s.mode.as_str(),
            s.sel("solo").as_deref(),
            s.prof("solo")["model"].as_str()
        ),
        ("solo", Some("local"), Some("qwen3.6"))
    );
}

#[test]
fn test_solo_asks_for_a_soloist_once_then_talks_to_it_directly() {
    let env = env();
    let (app, fakes) = role_app(&env);
    assert!(super::app::is_empty_profile(&app.st().prof("solo")));
    send(&app, "/solo");
    assert!(menu_open(&app)); // No soloist yet, so it asks.
    assert!(app.st().menu.title.contains("soloist"));
    press(&app, &["enter"]);
    settle(&app);
    assert_eq!(app.st().mode, "solo");
    assert_eq!(app.st().prof("solo")["model"], "claude-sonnet-5-5");
    assert_eq!(app.st().prof("work")["model"], "qwen3.6"); // The worker is untouched.
    assert_eq!(
        load_config()["roles"]["solo"],
        json!({"alias": "claude", "model": "claude-sonnet-5-5"})
    );
    let status = status_text(&app);
    assert!(status.contains("SOLO"));
    assert!(status.contains("soloist claude-sonnet-5-5"));
    assert!(status.contains("planner claude-opus-5-5")); // Every role stays visible in solo mode.
    send(&app, "rename jump() to leap()");
    assert_eq!(
        fakes
            .find("claude", Some("solo"), "claude-sonnet-5-5")
            .calls(),
        ["rename jump() to leap()"]
    );
    send(&app, "/plan");
    assert_eq!(app.st().mode, "plan");
    assert!(!menu_open(&app)); // A planner is set, so /plan does not ask.
    send(&app, "/solo");
    assert!(!menu_open(&app)); // Chosen once; not asked again.
    assert_eq!(app.st().mode, "solo");
}

#[test]
fn test_role_commands_only_change_their_own_role() {
    let env = env();
    let (app, _) = role_app(&env);
    send(&app, "/worker");
    press(&app, &["enter"]);
    settle(&app);
    assert_eq!(app.st().prof("work")["model"], "claude-sonnet-5-5");
    assert_eq!(app.st().prof("plan")["model"], "claude-opus-5-5");
    assert_eq!(app.st().mode, "plan"); // Picking a model does not change the mode.
    send(&app, "/soloist local");
    assert_eq!(app.st().prof("solo")["model"], "qwen3.6");
    assert_eq!(app.st().prof("work")["model"], "claude-sonnet-5-5");
    send(&app, "/planner local");
    assert_eq!(app.st().prof("plan")["model"], "qwen3.6");
    assert_eq!(app.st().prof("solo")["model"], "qwen3.6");
    assert_eq!(app.st().prof("review")["model"], "qwen3.6"); // The default reviewer follows.
}

#[test]
fn test_review_mode_talks_to_the_reviewer_read_only() {
    let env = env();
    let (app, fakes) = role_app(&env);
    send(&app, "/review");
    assert!(!menu_open(&app)); // The reviewer defaults to the planner, so it is set.
    assert_eq!(app.st().mode, "review");
    assert!(status_text(&app).contains("REVIEW"));
    send(&app, "check the wall jump for off-by-one errors");
    let review = fakes.find("claude", Some("review"), "claude-opus-5-5");
    let calls = review.calls();
    assert!(calls[0].starts_with("You are the reviewer."));
    assert!(calls[0].ends_with("check the wall jump for off-by-one errors"));
    assert!(review.read_only);
}

#[test]
fn test_every_role_is_shown_in_every_mode() {
    let env = env();
    let (app, _) = role_app(&env);
    send(&app, "/soloist local");
    for mode in ["plan", "work", "review", "solo"] {
        app.st().set_mode(mode);
        let status = status_text(&app);
        for expected in [
            mode.to_uppercase().as_str(),
            "planner claude-opus-5-5",
            "worker qwen3.6",
            "reviewer claude-opus-5-5 (= planner)",
            "soloist qwen3.6",
        ] {
            assert!(
                status.contains(expected),
                "{mode}: {expected} not in {status}"
            );
        }
    }
    send(&app, "/reviewer off");
    assert!(status_text(&app).contains("reviewer off"));
}

#[test]
fn test_an_unset_soloist_says_so() {
    let env = env();
    let (app, _) = role_app(&env);
    assert!(status_text(&app).contains("soloist not set"));
}

#[test]
fn test_review_asks_for_a_reviewer_when_review_is_off() {
    let env = env();
    let (app, _) = role_app(&env);
    send(&app, "/reviewer off");
    send(&app, "/review");
    assert!(menu_open(&app));
    assert!(!values(&app).contains(&json!("off")));
    press(&app, &["escape"]);
    settle(&app);
    assert_eq!(app.st().mode, "plan");
}

#[test]
fn test_f3_cycles_plan_work_review_and_solo() {
    let env = env();
    let (app, _) = role_app(&env);
    send(&app, "/soloist claude");
    for expected in ["work", "review", "solo", "plan"] {
        if menu_open(&app) {
            press(&app, &["escape"]); // Close the work picker.
            settle(&app);
        }
        press(&app, &["f3"]);
        settle(&app);
        assert_eq!(app.st().mode, expected);
        if expected == "work" {
            press(&app, &["escape"]);
            settle(&app);
            app.st().set_mode("work");
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// ReadOnlySessionTests

#[test]
fn test_review_mode_sessions_are_read_only() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    app.session(
        "claude",
        &json!({"engine": "claude"}),
        Some("review"),
        SessionRequest::default(),
    )
    .unwrap();
    app.session(
        "claude",
        &json!({"engine": "claude"}),
        Some("solo"),
        SessionRequest::default(),
    )
    .unwrap();
    let flags: Vec<bool> = fakes.all().iter().map(|f| f.read_only).collect();
    assert_eq!(flags, [true, false]);
}

// ---------------------------------------------------------------------------------------------------------
// ResumeTests

fn resume_config() -> Value {
    json!({
        "aliases": {
            "claude": {"engine": "claude", "model": "claude-opus-5-5"},
            "codex": {"engine": "codex", "model": "gpt-6.1"},
        },
        "planner": "claude",
        "default_model": "codex",
        "roles": {"solo": {"alias": "codex", "model": "gpt-6.1"}},
    })
}

fn resume_app(env: &Env) -> (App, Fakes) {
    setup_with(env, resume_config(), Fakes::new(), AppOptions::test())
}

#[test]
fn test_conversations_are_saved_with_their_mode_and_model() {
    let env = env();
    let (app, _) = resume_app(&env);
    send(&app, "plan the wall jump");
    send(&app, "/solo");
    send(&app, "fix the typo in README");
    send(&app, "and the other one");
    let records = conversations::for_project(&env.project());
    let summary: Vec<(String, String, String, i64)> = records
        .iter()
        .map(|r| {
            (
                r["mode"].as_str().unwrap().to_string(),
                r["model"].as_str().unwrap().to_string(),
                r["title"].as_str().unwrap().to_string(),
                r["turns"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                "solo".to_string(),
                "gpt-6.1".to_string(),
                "fix the typo in README".to_string(),
                2
            ),
            (
                "plan".to_string(),
                "claude-opus-5-5".to_string(),
                "plan the wall jump".to_string(),
                1
            ),
        ]
    );
    assert_eq!(records[0]["provider_id"], "provider-2");
    let messages = records[0]["messages"].as_array().unwrap();
    assert_eq!(messages[messages.len() - 2]["text"], "and the other one");
}

fn record(
    env: &Env,
    (mode, alias, model, provider): (&str, &str, &str, &str),
    text: &str,
    answer: &str,
    session: Option<&str>,
) -> String {
    conversations::record_turn(
        None,
        &env.project(),
        mode,
        mode,
        alias,
        Some(model),
        Some(provider),
        text,
        Some(answer),
        session,
        None,
    )
}

#[test]
fn test_resume_switches_mode_and_model_and_reopens_the_provider_session() {
    let env = env();
    let first = record(
        &env,
        ("solo", "claude", "claude-sonnet-5-5", "claude-sid"),
        "rename jump",
        "Renamed it.",
        None,
    );
    record(
        &env,
        ("plan", "codex", "gpt-6.1", "thread-9"),
        "plan sounds",
        "Wrote 2 tickets.",
        None,
    );
    let (app, fakes) = resume_app(&env);
    let (mode, model) = {
        let s = app.st();
        (s.mode.clone(), s.prof("solo")["model"].clone())
    };
    assert_eq!((mode, model), ("plan".to_string(), json!("gpt-6.1")));
    app.execute("/resume");
    settle(&app);
    assert_eq!(labels(&app), ["plan sounds", "rename jump"]); // Newest first.
    let rows = app.st().menu.visible_rows.clone();
    assert!(
        rows[1]
            .description
            .starts_with("solo · claude-sonnet-5-5 · "),
        "{}",
        rows[1].description
    );
    assert!(rows[1].description.ends_with("1 turn"));
    press(&app, &["down", "enter"]);
    settle(&app);
    assert_eq!(app.st().mode, "solo");
    assert_eq!(app.st().sel("solo").as_deref(), Some("claude"));
    assert_eq!(app.st().prof("solo")["model"], "claude-sonnet-5-5");
    assert!(fakes.opened().contains(&(
        "claude".to_string(),
        "claude-sonnet-5-5".to_string(),
        Some("solo".to_string()),
        Some("claude-sid".to_string())
    )));
    let text = body(&app);
    assert!(
        text.contains("Resumed · SOLO · claude-sonnet-5-5 · rename jump"),
        "{text}"
    );
    assert!(text.contains("> rename jump"));
    assert!(text.contains("Renamed it."));
    send(&app, "now rename land too");
    let saved = conversations::get(&first).unwrap();
    assert_eq!(saved["turns"], 2); // The resumed conversation keeps growing, not a new one.
    assert_eq!(
        load_config()["roles"]["solo"],
        json!({"alias": "claude", "model": "claude-sonnet-5-5"})
    );
}

#[test]
fn test_clear_and_new_start_completely_fresh() {
    let env = env();
    for command in ["/clear", "/new"] {
        let (app, _) = resume_app(&env);
        send(&app, &format!("plan the wall jump before {command}"));
        assert!(!app.st().sessions.is_empty());
        send(&app, command);
        let text = body(&app);
        assert!(!text.contains("wall jump"), "{text}"); // Nothing from the earlier conversation is left on screen.
        assert!(text.contains("do the thing.")); // The screen looks as it did when Donut opened.
        assert!(app.st().sessions.is_empty());
        assert_eq!(app.st().last_answer, "");
        send(&app, "plan sounds");
    }
    let titles: Vec<String> = conversations::for_project(&env.project())
        .iter()
        .map(|r| r["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titles.iter().filter(|t| *t == "plan sounds").count(), 2); // Each fresh start became its own conversation.
    assert_eq!(titles.len(), 4);
}

#[test]
fn test_each_window_plans_and_works_in_its_own_tickets_session() {
    let env = env();
    let (app, fakes) = resume_app(&env);
    let name = app.st().session_name.clone().unwrap();
    assert!(regex::Regex::new(r"^\d{8}-\d{4}").unwrap().is_match(&name));
    let folder = format!(".donut/{name}/");
    assert!(body(&app).contains(&format!("tickets {folder}")));
    send(&app, "plan the wall jump");
    let planner = fakes.first();
    assert!(
        planner
            .calls()
            .last()
            .unwrap()
            .contains(&format!("{folder}to_do/"))
    );
    assert_eq!(
        conversations::for_project(&env.project())[0]["session"],
        json!(name)
    );
    let seen = failing_runner(&app);
    send(&app, "/work next 1");
    let args = seen.lock().unwrap()[0].args.clone();
    let at = args.iter().position(|a| a == "--session").unwrap();
    assert_eq!(args[at + 1], name);
    let first = name;
    workspace::create(&env.project(), Some(&first)).unwrap(); // What the (patched out) install does on a real /plan.
    send(&app, "/session new");
    let now = app.st().session_name.clone().unwrap();
    assert_ne!(now, first);
    assert!(
        env.project()
            .join(".donut")
            .join(&now)
            .join("to_do")
            .is_dir()
    );
    send(&app, "/session");
    let rows = labels(&app);
    assert_eq!(rows[0], "New session");
    assert!(rows.contains(&first));
    press(&app, &["escape"]);
    settle(&app);
    send(&app, &format!("/session {first}"));
    assert_eq!(app.st().session_name.as_deref(), Some(first.as_str()));
    let lanes = app.st().lanes();
    assert_eq!(
        workspace::relative(&env.project(), &lanes),
        format!(".donut/{first}/")
    );
}

#[test]
fn test_a_session_is_one_resume_entry_and_each_mode_continues_its_own_conversation() {
    let env = env();
    record(
        &env,
        ("solo", "claude", "claude-sonnet-5-5", "old-sid"),
        "an earlier solo chat",
        "ok",
        Some("feature"),
    );
    let plan = record(
        &env,
        ("plan", "codex", "gpt-6.1", "thread-1"),
        "plan the feature",
        "ok",
        Some("feature"),
    );
    let solo = record(
        &env,
        ("solo", "claude", "claude-sonnet-5-5", "sid-1"),
        "do the feature",
        "ok",
        Some("feature"),
    );
    let (app, fakes) = resume_app(&env);
    app.execute("/resume");
    settle(&app);
    let rows = app.st().menu.visible_rows.clone();
    assert_eq!(rows.len(), 1); // Three saved conversations, one session, one entry.
    assert!(
        rows[0].description.contains("solo + plan"),
        "{}",
        rows[0].description
    );
    assert!(rows[0].description.contains("3 turns"));
    press(&app, &["enter"]);
    settle(&app);
    assert_eq!(app.st().mode, "solo");
    let resumed = fakes.find("claude", Some("solo"), "claude-sonnet-5-5");
    assert_eq!(resumed.resume_id.as_deref(), Some("sid-1"));
    {
        let s = app.st();
        let id = s.threads["solo"];
        assert_eq!(
            s.rec(id).unwrap().conversation_id.as_deref(),
            Some(solo.as_str())
        );
        // The plan conversation waits, with its own model, and continues when plan mode is next used.
        let waiting = s
            .waiting_resume
            .iter()
            .find(|(mode, _)| mode == "plan")
            .unwrap();
        assert_eq!(waiting.1["id"], json!(plan));
        assert_eq!(
            (s.sel("plan").as_deref(), s.prof("plan")["model"].as_str()),
            (Some("codex"), Some("gpt-6.1"))
        );
    }
    let (alias, profile) = {
        let s = app.st();
        (s.sel("plan").unwrap(), s.prof("plan"))
    };
    let handle = app
        .session(&alias, &profile, None, SessionRequest::default())
        .unwrap();
    let planner = fakes.find("codex", None, "gpt-6.1");
    assert_eq!(planner.resume_id.as_deref(), Some("thread-1"));
    assert_eq!(
        app.st().rec(handle.id).unwrap().conversation_id.as_deref(),
        Some(plan.as_str())
    );
    assert!(app.st().waiting_resume.is_empty());
    assert!(body(&app).contains("Continues this session's plan conversation (1 turn)."));
    // /clear starts fresh: nothing is left waiting to continue.
    app.st().waiting_resume = vec![("plan".to_string(), json!({"id": plan}))];
    app.start_fresh();
    assert!(app.st().waiting_resume.is_empty());
    // Opened again in the same session (donut -s feature): its conversations continue, no new entry.
    let again = create(
        &env,
        &resume_config(),
        AppOptions::test(),
        None,
        Some("feature"),
    );
    again.start();
    let ids: HashMap<String, String> = again
        .st()
        .waiting_resume
        .iter()
        .map(|(mode, record)| (mode.clone(), record["id"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        ids,
        HashMap::from([("solo".to_string(), solo), ("plan".to_string(), plan)])
    );
    assert_eq!(
        conversations::by_session(&conversations::for_project(&env.project())).len(),
        1
    );
}

#[test]
fn test_resume_switches_back_to_the_conversations_tickets_session() {
    let env = env();
    record(
        &env,
        ("plan", "claude", "claude-opus-5-5", "sid"),
        "jump",
        "ok",
        Some("older"),
    );
    let (app, _) = resume_app(&env);
    app.execute("/resume");
    settle(&app);
    press(&app, &["enter"]);
    settle(&app);
    assert_eq!(app.st().session_name.as_deref(), Some("older"));
}

#[test]
fn test_project_opens_another_project_in_a_new_window() {
    let env = env();
    let other = env.temp.path().join("other-app");
    std::fs::create_dir_all(&other).unwrap();
    conversations::remember_project(&other);
    let (app, _) = resume_app(&env);
    let opened: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let answer: Arc<Mutex<Option<String>>> =
        Arc::new(Mutex::new(Some("a new Terminal window".to_string())));
    {
        let (opened, answer) = (Arc::clone(&opened), Arc::clone(&answer));
        app.set_open_window(Arc::new(move |path: &Path| {
            opened.lock().unwrap().push(path.to_path_buf());
            answer.lock().unwrap().clone()
        }));
    }
    send(&app, "/project");
    assert_eq!(labels(&app), ["other-app"]); // Not this project.
    press(&app, &["enter"]);
    settle(&app);
    assert_eq!(opened.lock().unwrap()[0], conversations::resolve(&other));
    assert!(body(&app).contains("Opened other-app in a new Terminal window."));
    *answer.lock().unwrap() = None;
    send(&app, &format!("/project {}", other.display())); // Typed as is, backslashes and all on Windows.
    let text = body(&app);
    assert!(text.contains("No terminal app found"), "{text}");
    assert!(
        text.contains(
            &conversations::resolve(&other)
                .to_string_lossy()
                .into_owned()
        )
    );
    send(&app, r"/project C:\Users\me\my app");
    assert!(body(&app).contains(r"C:\Users\me\my app")); // Backslashes and spaces kept.
}

#[test]
fn test_streamed_updates_become_separate_paragraphs_in_the_transcript() {
    let env = env();
    let (app, _) = resume_app(&env);
    {
        let mut s = app.st();
        s.reset_turn();
        s.verbosity = "verbose".into(); // The level that keeps every message as it is written.
    }
    for piece in [
        "Status: found the job.",
        "\n\nConfirmed: it is slow.",
        "\n\n```sql\nSELECT 1\n\nFROM t\n",
    ] {
        app.event("text", &json!(piece), None, None);
    }
    let text = body(&app);
    assert!(text.contains("Status: found the job."));
    assert!(text.contains("Confirmed: it is slow."));
    assert!(!text.contains("SELECT")); // A code block is never split at its blank line.
    {
        let s = app.st();
        assert!(s.turn_text[s.shown..].contains("SELECT 1")); // Still being written, above the prompt.
    }
    app.st().answer();
    let text = body(&app);
    assert!(text.contains("FROM t"));
    assert_eq!(text.matches("Status: found the job.").count(), 1); // Committed once, not again at the end.
    let s = app.st();
    assert!(
        s.last_answer
            .starts_with("Status: found the job.\n\nConfirmed")
    );
    assert_eq!(s.shown, s.turn_text.len());
}

#[test]
fn test_each_reply_ends_with_a_labelled_context_recap_kept_out_of_the_answer() {
    let env = env();
    let (app, fakes) = setup(&env, claude_config());
    send(&app, "first");
    let fake = fakes.first();
    assert!(fake.raw_calls()[0].ends_with(crate::recap::NOTE));
    fake.set_hook(|fake, _| {
        (fake.emit)(
            "text",
            json!("Renamed it.\n\nDONUT-RECAP: We are renaming jump() to leap(); it is done."),
        )
    });
    send(&app, "rename jump() to leap()");
    let text = body(&app);
    assert!(text.contains("Renamed it."));
    assert!(text.contains(crate::recap::LABEL));
    assert!(text.contains("We are renaming jump() to leap(); it is done."));
    assert!(!text.contains("DONUT-RECAP"));
    let s = app.st();
    assert_eq!(s.last_answer, "Renamed it."); // /copy and the shared history get the reply only.
    assert!(!s.exchanges.last().unwrap().answer.contains("DONUT-RECAP"));
    drop(s);
    send(&app, "/rename");
    assert_eq!(fake.raw_calls().last().unwrap(), "/rename"); // Slash commands are sent as typed.
}

#[test]
fn test_links_are_clickable() {
    let env = env();
    let (app, _) = resume_app(&env);
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&opened);
    app.set_open_url(Arc::new(move |url: &str| {
        sink.lock().unwrap().push(url.to_string())
    }));
    app.st().transcript.clear();
    app.say("PR: https://github.com/example/project/pull/42", None);
    app.st()
        .say_markdown("Merged https://github.com/x/y/pull/2 today.");
    let _ = screen(&app, 100, 30);
    let area = app.st().areas.transcript;
    let lines = app.st().transcript.plain();
    let second = lines
        .iter()
        .position(|line| line.contains("Merged"))
        .unwrap() as u16;
    let click = |column: u16, row: u16| {
        mouse(
            &app,
            MouseEventKind::Down(MouseButton::Left),
            area.x + column,
            area.y + row,
        );
        mouse(
            &app,
            MouseEventKind::Up(MouseButton::Left),
            area.x + column,
            area.y + row,
        );
    };
    click(12, 0); // Inside the first URL (the log has 1 column of padding).
    assert_eq!(
        opened.lock().unwrap().last().unwrap(),
        "https://github.com/example/project/pull/42"
    );
    click(14, second);
    assert_eq!(
        opened.lock().unwrap().last().unwrap(),
        "https://github.com/x/y/pull/2"
    );
    let calls = opened.lock().unwrap().len();
    click(2, 0); // "PR:" is not a link.
    assert_eq!(opened.lock().unwrap().len(), calls);
}

#[test]
fn test_selected_text_is_copied() {
    let env = env();
    let (app, _) = resume_app(&env);
    let copied: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&copied);
    app.set_copy_native(Arc::new(move |text: &str| {
        sink.lock().unwrap().push(text.to_string());
        true
    }));
    app.st().transcript.clear();
    app.say("Output line one", None);
    app.say("PR https://github.com/x/y/pull/9", None);
    let _ = screen(&app, 100, 30);
    let area = app.st().areas.transcript;
    mouse(
        &app,
        MouseEventKind::Down(MouseButton::Left),
        area.x + 1,
        area.y,
    );
    mouse(
        &app,
        MouseEventKind::Drag(MouseButton::Left),
        area.x + 7,
        area.y + 1,
    );
    mouse(
        &app,
        MouseEventKind::Up(MouseButton::Left),
        area.x + 7,
        area.y + 1,
    );
    // Copied as soon as the mouse is released.
    assert_eq!(
        copied.lock().unwrap().last().unwrap(),
        "Output line one\nPR http"
    );
    let osc: Vec<String> = app
        .st()
        .terminal_out
        .iter()
        .filter(|text| text.starts_with("\x1b]52;c;"))
        .cloned()
        .collect();
    assert!(!osc.is_empty());
    copied.lock().unwrap().clear();
    press(&app, &["ctrl+c"]); // With a selection, Ctrl+C copies instead of cancelling.
    assert_eq!(
        copied.lock().unwrap().last().unwrap(),
        "Output line one\nPR http"
    );
    assert!(!app.is_cancelled());
}

#[test]
fn test_bang_runs_a_shell_command_and_the_model_sees_it() {
    let env = env();
    let (app, fakes) = resume_app(&env);
    send(&app, "!echo 'hello from the shell'");
    let text = body(&app);
    assert!(text.contains("hello from the shell"), "{text}");
    assert!(text.contains("exit 0"));
    assert!(fakes.opened().is_empty()); // No model was involved.
    send(&app, "what did that print?");
    let planner = fakes.first();
    let calls = planner.calls();
    let last = calls.last().unwrap();
    assert!(last.contains("Shell commands I ran in the project before this message"));
    assert!(last.contains("hello from the shell"));
    assert!(last.trim_end().ends_with("what did that print?"));
    send(&app, "and now?");
    assert!(
        !planner
            .calls()
            .last()
            .unwrap()
            .contains("Shell commands I ran")
    ); // Given once.
}

#[test]
fn test_shell_output_leaves_the_transcript_at_its_end() {
    let env = env();
    let (app, _) = resume_app(&env);
    let _ = screen(&app, 100, 30);
    send(&app, "!seq 0 199");
    wait("exit line", || {
        app.st()
            .transcript
            .plain()
            .last()
            .is_some_and(|line| line.contains("exit 0"))
    });
    let _ = screen(&app, 100, 30);
    {
        let s = app.st();
        assert_eq!(s.transcript.scroll_y(), s.transcript.max_scroll_y());
    }
    // When the turn ends, the usage line refreshes and can grow, which shrinks the transcript.
    let _ = screen(&app, 100, 27);
    let s = app.st();
    assert!(s.transcript.max_scroll_y() > 0);
    assert_eq!(s.transcript.scroll_y(), s.transcript.max_scroll_y()); // The exit line is still in view.
}

#[test]
fn test_a_model_waiting_on_a_command_is_woken_when_it_exits() {
    let env = env();
    let (app, fakes) = resume_app(&env);
    app.st().permission = "full".into();
    send(&app, "build it");
    let planner = fakes.first();
    let id = app.st().sessions[0].id;
    app.st().rec_mut(id).unwrap().donut_waits = true;
    let replies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![
        "The build passed.".to_string(),
        "Started the build.\n\nDONUT-WAIT: echo build ok".to_string(),
    ]));
    let sink = Arc::clone(&replies);
    planner.set_hook(move |fake, _| {
        let reply = sink.lock().unwrap().pop().unwrap();
        (fake.emit)("text", json!(reply));
    });
    send(&app, "build it again");
    wait("the wake-up turn", || {
        planner.calls().iter().any(|call| call.contains("build ok")) && !app.busy()
    });
    std::thread::sleep(Duration::from_millis(100));
    settle(&app);
    let calls = planner.calls();
    let wake = calls.last().unwrap();
    assert!(wake.contains("exited with code 0"), "{wake}");
    assert!(wake.contains("build ok"));
    let text = body(&app);
    assert!(text.contains("waking"), "{text}");
    assert!(text.contains("The build passed."));
    assert!(!app.busy());
}

#[test]
fn test_a_turn_claude_starts_itself_is_shown() {
    let env = env();
    let (app, _) = resume_app(&env);
    send(&app, "start the deploy");
    let id = app.st().sessions[0].id;
    app.event(
        "background",
        &json!("Background command finished"),
        None,
        Some(id),
    );
    app.event("wake_start", &json!(""), None, Some(id));
    assert!(app.busy());
    app.event(
        "text",
        &json!("The deploy finished; all checks green."),
        None,
        Some(id),
    );
    app.event("wake_end", &json!(""), None, Some(id));
    settle(&app);
    let text = body(&app);
    assert!(text.contains("woken by a background task"), "{text}");
    assert!(text.contains("The deploy finished; all checks green."));
    assert!(!app.busy());
}

#[test]
fn test_text_held_for_a_wake_up_that_never_ended_does_not_swallow_the_next_answer() {
    let env = env();
    let (app, _) = resume_app(&env);
    send(&app, "/solo");
    send(&app, "first question");
    let id = app.st().sessions.last().unwrap().id;
    app.st().rec_mut(id).unwrap().wake_buffer = Some(vec!["An earlier note.".to_string()]); // A wake-up began while busy and never ended.
    send(&app, "second question");
    assert!(app.st().rec(id).unwrap().wake_buffer.is_none());
    let text = body(&app);
    assert!(text.contains("An earlier note."), "{text}"); // Shown, not lost.
    assert!(text.contains("answer 2")); // And this turn's answer is on screen.
    assert!(!text.contains("ended its turn without a reply"));
}

#[test]
fn test_a_named_session_keeps_its_name_after_a_message() {
    let env = env();
    let (app, _) = resume_app(&env);
    let mut s = app.st();
    s.name_task("first question of a new window");
    assert_eq!(s.session_label(), "first question of a new window"); // Unnamed: the message names it.
    s.task_title.clear(); // What /resume or donut -s leaves.
    s.session_name = Some("tendercare-fax".into());
    s.name_task("can you finish 023");
    assert_eq!(s.session_label(), "tendercare-fax");
    assert!(s.window_title().starts_with("tendercare-fax · "));
}

#[test]
fn test_chrome_is_a_saved_switch_for_new_claude_conversations() {
    let env = env();
    let (app, _) = resume_app(&env);
    // On with no setting at all: a new Claude conversation gets the browser tools, a review one never.
    assert!(!status_text(&app).contains("chrome"));
    assert!(app.st().chrome_on());
    send(&app, "/chrome");
    assert_eq!(app.st().config.get("chrome"), Some(&json!(false)));
    assert_eq!(load_config().get("chrome"), Some(&json!(false))); // Saved for the next window.
    assert!(status_text(&app).contains("chrome off"));
    send(&app, "/chrome maybe");
    assert_eq!(app.st().config.get("chrome"), Some(&json!(false)));
    send(&app, "/chrome on");
    assert_eq!(load_config().get("chrome"), Some(&json!(true)));
    assert!(!status_text(&app).contains("chrome"));
}

#[test]
fn test_handholding_is_a_saved_level_that_every_plan_prompt_carries() {
    let env = env();
    let (app, fakes) = resume_app(&env);
    assert!(!status_text(&app).contains("handholding")); // Medium, the default.
    send(&app, "/handholding high");
    assert_eq!(app.st().config["handholding"], "high");
    assert_eq!(load_config()["handholding"], "high"); // Saved for the next window.
    assert!(status_text(&app).contains("handholding high"));
    send(&app, "/handholding extreme");
    assert_eq!(app.st().config["handholding"], "high"); // An unknown level changes nothing.
    send(&app, "plan the wall jump");
    let planner = fakes.first();
    let last = planner.calls().last().unwrap().clone();
    assert!(last.contains("Handholding is high"), "{last}");
    assert!(last.contains("quote the exact existing lines"));
    send(&app, "/handholding low");
    send(&app, "plan the dash");
    let last = planner.calls().last().unwrap().clone();
    assert!(last.contains("Handholding is low"));
    assert!(last.contains("Ticket scope can be larger"));
    send(&app, "/handholding medium");
    assert!(!status_text(&app).contains("handholding"));
}

#[test]
fn test_verbosity_changes_what_is_shown_never_what_is_sent() {
    let env = env();
    let mut seen: HashMap<&str, String> = HashMap::new();
    for level in ["quiet", "normal", "verbose", "full"] {
        let (app, _) = resume_app(&env);
        send(&app, &format!("/verbosity {level}"));
        app.st().transcript.clear();
        {
            app.st().reset_turn();
        }
        let event = |kind: &str, value: Value| app.event(kind, &value, None, None);
        event("thinking", json!("The user wants the status. "));
        event("thinking", json!("Check git first."));
        event("text", json!("Status: reading the repo."));
        event(
            "tool",
            json!({"id": "t1", "name": "Bash", "input": {"command": "git status --short"}}),
        );
        event(
            "tool_result",
            json!({"id": "t1", "output": " M a.py\n M b.py", "error": false}),
        );
        event("break", json!(""));
        event("text", json!("Two files changed."));
        app.st().answer();
        seen.insert(level, body(&app));
        assert_eq!(
            app.st().turn_text,
            "Status: reading the repo.\n\nTwo files changed."
        ); // Unchanged.
    }
    assert!(!seen["quiet"].contains("Status: reading"));
    assert!(seen["quiet"].contains("Two files changed."));
    assert!(!seen["normal"].contains("Status: reading")); // A progress note: shown while it works, not kept.
    assert!(seen["normal"].contains("Two files changed."));
    assert!(seen["verbose"].contains("Status: reading"));
    assert!(!seen["normal"].contains("⏺ Bash"));
    assert!(!seen["normal"].contains("Check git"));
    assert!(
        seen["verbose"].contains("⏺ Bash  git status --short"),
        "{}",
        seen["verbose"]
    );
    assert!(seen["verbose"].contains("⎿ 2 lines"));
    assert!(!seen["verbose"].contains("M a.py"));
    assert!(
        seen["full"].contains("✻ The user wants the status. Check git first."),
        "{}",
        seen["full"]
    );
    assert!(seen["full"].contains("M a.py"));
    assert_eq!(load_config().get("verbosity"), Some(&json!("full"))); // Saved for next time.
}

#[test]
fn test_focus_toggles_quiet_instead_of_reaching_the_provider() {
    let env = env();
    let (app, fakes) = resume_app(&env);
    send(&app, "/focus");
    assert_eq!(app.st().verbosity, "quiet");
    assert!(app.st().focus_view);
    let rows = screen(&app, 120, 40).join("\n");
    assert!(
        !rows.contains("○ planner") && !rows.contains("Permissions"),
        "{rows}"
    ); // The status line and keys are hidden.
    assert!(app.st().band_line().is_none());
    send(&app, "/focus");
    assert_eq!(app.st().verbosity, "normal");
    send(&app, "/focus on");
    assert_eq!(app.st().verbosity, "quiet");
    assert!(fakes.all().is_empty());
}

#[test]
fn test_the_transcript_stays_at_the_bottom_while_a_long_reply_streams() {
    let env = env();
    let (app, _) = resume_app(&env);
    let _ = screen(&app, 100, 30);
    for number in 0..80 {
        app.say(&format!("earlier line {number}"), None);
    }
    let _ = screen(&app, 100, 30);
    app.st().reset_turn();
    for number in 0..10 {
        // One long paragraph, still being written.
        app.event(
            "text",
            &json!(format!("{}\n", format!("word {number} ").repeat(30))),
            None,
            None,
        );
        let _ = screen(&app, 100, 30);
    }
    {
        let s = app.st();
        assert_eq!(s.transcript.scroll_y(), s.transcript.max_scroll_y());
        assert_eq!(s.response.scroll_y(), s.response.max_scroll_y()); // Shows the newest lines.
    }
    app.st().answer();
    let _ = screen(&app, 100, 30);
    {
        let s = app.st();
        assert_eq!(s.transcript.scroll_y(), s.transcript.max_scroll_y());
    }
    // Scrolled up to read: new output does not pull the view back down.
    let area = app.st().areas.transcript;
    for _ in 0..4 {
        mouse(&app, MouseEventKind::ScrollUp, area.x + 2, area.y + 2);
    }
    assert!(!app.st().transcript.follow());
    let where_ = app.st().transcript.scroll_y();
    app.say("a new line", None);
    let _ = screen(&app, 100, 30);
    assert_eq!(app.st().transcript.scroll_y(), where_);
    // Back at the end, however the view got there: new output is followed again.
    app.st().transcript.scroll_end();
    assert!(app.st().transcript.follow());
    app.say("another line", None);
    let _ = screen(&app, 100, 30);
    let s = app.st();
    assert_eq!(s.transcript.scroll_y(), s.transcript.max_scroll_y());
}

#[test]
fn test_renaming_the_session_renames_its_tickets_folder() {
    let env = env();
    let (app, fakes) = resume_app(&env);
    let first = app.st().session_name.clone().unwrap();
    send(&app, "plan the wall jump");
    workspace::create(&env.project(), Some(&first)).unwrap(); // What the (patched out) install does on a real /plan.
    let planner = fakes.first();
    planner.set_hook(|fake, _| (fake.emit)("text", json!("Session renamed to: fix login bug")));
    // The planner is Claude: its own /rename names the session, and the folder follows.
    send(&app, "/rename");
    assert_eq!(planner.calls().last().unwrap(), "/rename");
    assert_eq!(app.st().session_name.as_deref(), Some("fix-login-bug"));
    assert!(
        env.project()
            .join(".donut")
            .join("fix-login-bug")
            .join("to_do")
            .is_dir()
    );
    assert!(!env.project().join(".donut").join(&first).exists());
    assert_eq!(
        conversations::for_project(&env.project())[0]["session"],
        "fix-login-bug"
    );
    // /resume lists the conversation by its new name, with its first message beside it.
    send(&app, "/resume");
    let row = app.st().menu.visible_rows[0].clone();
    assert_eq!(row.label, "fix login bug");
    assert!(
        row.description.starts_with("plan the wall jump · plan · "),
        "{}",
        row.description
    );
    press(&app, &["escape"]);
    settle(&app);
    // The soloist is Codex, which has no /rename: Donut names the folder itself.
    send(&app, "/solo");
    send(&app, "/rename Wall jump: v2");
    assert_eq!(app.st().session_name.as_deref(), Some("Wall-jump-v2"));
    assert_eq!(planner.calls().len(), 2); // No model was asked.
}

fn title_escape(title: &str) -> String {
    format!("\x1b]1;{title}\x07\x1b]2;{title}\x07")
}

#[test]
fn test_the_session_name_titles_the_window_and_shows_in_the_status_line() {
    let env = env();
    let (app, _) = resume_app(&env);
    app.st().terminal_out.clear();
    app.st().rename_session("fix login bug");
    let project = env
        .project()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let expected = title_escape(&format!("fix-login-bug · {project} · Donut"));
    assert!(
        app.st().terminal_out.contains(&expected),
        "{:?}",
        app.st().terminal_out
    );
    assert!(status_text(&app).contains("session fix-login-bug"));
    app.st().terminal_out.clear();
    app.status(None);
    assert!(app.st().terminal_out.is_empty()); // Only written when it changes.
    app.st().session_name = Some("evil\x07\x1b]0;x".into());
    app.st().show_title();
    let written = app.st().terminal_out.last().unwrap().clone();
    assert!(!written.contains("\x1b]0;x")); // Control characters never reach the terminal.
    assert_eq!(written.matches('\x07').count(), 2); // Only the two terminators, none inside a title.
    assert_eq!(written.matches('\x1b').count(), 2);
}

#[test]
fn test_next_and_prev_list_this_sessions_tickets() {
    let env = env();
    let (app, _) = resume_app(&env);
    let name = app.st().session_name.clone();
    let lanes = workspace::create(&env.project(), name.as_deref()).unwrap();
    let ticket = |title: &str, slug: &str| {
        format!("# {title}\n\nAbout {title}.\n\n**Slug:** `{slug}`\n**Depends on:** None\n")
    };
    std::fs::write(lanes.join("done").join("001_a.md"), ticket("Old one", "a")).unwrap();
    std::fs::write(
        lanes.join("done").join("002_b.md"),
        ticket("Latest one", "b") + "PR: https://github.com/o/r/pull/7\n",
    )
    .unwrap();
    std::fs::write(
        lanes.join("to_do").join("003_c.md"),
        ticket("Coming up", "c"),
    )
    .unwrap();
    let shown = |command: &str| {
        app.st().transcript.clear();
        app.execute(command);
        settle(&app);
        body(&app)
    };
    let text = shown("/prev 1");
    assert!(text.contains("Latest one"), "{text}");
    assert!(text.contains("https://github.com/o/r/pull/7"));
    assert!(!text.contains("Old one"));
    assert!(!text.contains("Coming up"));
    let text = shown("/prev");
    assert!(text.find("Latest one").unwrap() < text.find("Old one").unwrap()); // Latest first.
    let text = shown("/next");
    assert!(text.contains("Coming up"));
    assert!(!text.contains("Latest one"));
    assert!(shown("/next zero").contains("COUNT is a positive number"));
}

#[test]
fn test_the_first_chat_message_names_the_window() {
    let env = env();
    let (app, _) = resume_app(&env);
    app.st().terminal_out.clear();
    let project = env
        .project()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for text in ["/foo", "   ", "!ls"] {
        app.st().name_task(text);
    }
    assert_eq!(app.st().task_title, "");
    let folder = app.st().session_name.clone();
    app.st().name_task("fix the login bug on mobile");
    let expected = title_escape(&format!("fix the login bug on mobile · {project} · Donut"));
    assert!(app.st().terminal_out.contains(&expected));
    app.st().terminal_out.clear();
    app.st().name_task("something else");
    assert!(app.st().terminal_out.is_empty());
    assert_eq!(app.st().session_name, folder);
    app.st().rename_session("x y");
    app.st().show_title();
    let written = app.st().terminal_out.clone();
    assert_eq!(
        written.last().unwrap(),
        &title_escape(&format!("x-y · {project} · Donut"))
    );
}

#[test]
fn test_running_background_shells_show_in_the_status_line() {
    let env = env();
    let (app, _) = resume_app(&env);
    let task = |id: &str, state: &str, kind: &str, description: &str| {
        app.event(
            "task",
            &json!({"id": id, "state": state, "kind": kind, "description": description}),
            None,
            None,
        );
    };
    task("t1", "running", "shell", "Poll the build");
    task("t2", "running", "shell", "Watch CI");
    task("a1", "running", "agent", "Explore");
    assert!(
        status_text(&app).contains("⏳ 2 shells running · 1 agent running"),
        "{}",
        status_text(&app)
    );
    // Each one has its own line above the prompt, with what it is and how long it has run.
    let lines = app.st().background_lines();
    assert!(!lines.is_empty());
    let shown = plain_lines(&lines);
    assert!(
        shown.contains("⏳ shell running  Poll the build · 0s"),
        "{shown}"
    );
    assert!(shown.contains("Still working in the background")); // Between turns, it says the work goes on.
    // A turn that ends with no words says why, so it does not look like a hang.
    app.st().reset_turn();
    app.st().answer();
    assert!(
        body(&app).contains("No reply yet · 2 shells running · 1 agent running"),
        "{}",
        body(&app)
    );
    assert!(shown.contains("⏳ agent running  Explore"));
    assert_eq!(
        body(&app)
            .matches("Background shell started  Poll the build")
            .count(),
        1
    );
    task("t1", "completed", "shell", "");
    task("t1", "completed", "shell", ""); // Reported twice: counted once.
    task("a1", "killed", "agent", "");
    assert!(status_text(&app).contains("⏳ 1 shell running"));
    let text = body(&app);
    assert_eq!(
        text.matches("Background shell completed  Poll the build")
            .count(),
        1
    );
    assert!(text.contains("Background agent killed  Explore"));
    assert!(!plain_lines(&app.st().background_lines()).contains("Poll the build"));
    task("t2", "completed", "shell", "");
    assert!(app.st().background_lines().is_empty()); // Nothing left: the line goes away.
    app.st().reset_turn();
    app.st().answer();
    assert!(body(&app).contains("The model ended its turn without a reply."));
}

#[test]
fn test_resume_with_nothing_saved() {
    let env = env();
    let (app, _) = resume_app(&env);
    app.execute("/resume");
    settle(&app);
    assert!(!menu_open(&app));
    assert!(body(&app).contains("No earlier conversations in this project yet."));
}

// ---------------------------------------------------------------------------------------------------------
// Switching models and sessions.

#[test]
fn test_a_switched_model_keeps_the_whole_conversation() {
    let env = env();
    let aliases = json!({
        "claude": {"engine": "claude", "model": "opus"},
        "codex": {"engine": "codex", "model": "gpt-6.1"},
        "local": {"engine": "ollama", "model": "qwen3.6"},
    });
    let mut fakes = Fakes::new();
    fakes.answer = Answer::Model;
    fakes.can_switch = true;
    let (app, fakes) = setup_with(
        &env,
        json!({"aliases": aliases, "planner": "claude"}),
        fakes,
        AppOptions::test(),
    );
    let used = |alias: &str, model: Option<&str>| {
        let mut profile = aliases[alias].clone();
        if let Some(model) = model {
            profile["model"] = json!(model);
        }
        let mut s = app.st();
        s.selected.insert("plan".into(), Some(alias.to_string()));
        s.profiles.insert("plan".into(), profile);
    };
    send(&app, "remember the number 7");
    let claude = fakes.last();
    // Another Claude model on the same login: the same live session switches, with its full context.
    used("claude", Some("sonnet"));
    send(&app, "what number did I say?");
    assert!(Arc::ptr_eq(&fakes.last(), &claude));
    assert_eq!(
        *claude.switched.lock().unwrap(),
        [Some("sonnet".to_string())]
    );
    assert!(!claude.calls().last().unwrap().contains("<conversation>"));
    // Another provider: Donut hands the conversation so far to the new model, then retires the old one.
    used("codex", None);
    send(&app, "and now?");
    let codex = fakes.last();
    assert_eq!(codex.family, "codex");
    let handoff = codex.calls()[0].clone();
    assert!(
        handoff.contains("This conversation started before you joined it"),
        "{handoff}"
    );
    assert!(
        handoff
            .contains("[plan mode, opus]\nUser: remember the number 7\nAssistant: opus answered"),
        "{handoff}"
    );
    assert!(
        handoff.contains(
            "[plan mode, sonnet]\nUser: what number did I say?\nAssistant: sonnet answered"
        ),
        "{handoff}"
    );
    assert!(handoff.find("</conversation>").unwrap() < handoff.find("and now?").unwrap()); // History, then the request.
    assert!(claude.closed.load(Ordering::SeqCst));
    assert!(
        app.st()
            .sessions
            .iter()
            .all(|rec| !Arc::ptr_eq(&rec.session, &(claude.clone() as Arc<dyn Session>)))
    );
    send(&app, "one more");
    assert!(!codex.calls().last().unwrap().contains("<conversation>")); // Handed over once.
    // And on to a local model, which gets everything, including what Codex said.
    used("local", None);
    send(&app, "local now");
    let qwen = fakes.last();
    assert_eq!(qwen.family, "qwen");
    let first = qwen.calls()[0].clone();
    assert!(
        first.contains("[plan mode, gpt-6.1]\nUser: and now?\nAssistant: gpt-6.1 answered"),
        "{first}"
    );
    assert!(first.contains("User: remember the number 7"));
}

#[test]
fn test_every_mode_sees_what_the_others_did() {
    let env = env();
    let aliases = json!({
        "claude": {"engine": "claude", "model": "opus"},
        "codex": {"engine": "codex", "model": "gpt-6.1"},
        "local": {"engine": "ollama", "model": "qwen3.6"},
    });
    let mut fakes = Fakes::new();
    fakes.answer = Answer::Model;
    fakes.can_switch = true;
    let (app, fakes) = setup_with(
        &env,
        json!({"aliases": aliases, "planner": "claude"}),
        fakes,
        AppOptions::test(),
    );
    send(&app, "plan a thing");
    let planner = fakes.last();
    {
        let mut s = app.st();
        s.selected.insert("review".into(), Some("codex".into()));
        s.profiles.insert("review".into(), aliases["codex"].clone());
        s.reviewer = "chosen".into();
    }
    send(&app, "/review");
    send(&app, "check the login change");
    let reviewer = fakes.last();
    assert!(
        reviewer.calls()[0].contains("[plan mode, opus]\nUser: plan a thing"),
        "{}",
        reviewer.calls()[0]
    );
    {
        let mut s = app.st();
        s.selected.insert("solo".into(), Some("local".into()));
        s.profiles.insert("solo".into(), aliases["local"].clone());
    }
    send(&app, "/solo");
    send(&app, "one sentence on what was just checked");
    let solo = fakes.last();
    assert!(
        solo.calls()[0].contains("[review mode, gpt-6.1]\nUser: check the login change"),
        "{}",
        solo.calls()[0]
    );
    assert!(solo.calls()[0].contains("[plan mode, opus]"));
    // Back in plan mode, the planner is told only what it missed, never its own turn again.
    send(&app, "/plan");
    send(&app, "carry on");
    let latest = planner.calls().last().unwrap().clone();
    assert!(latest.contains("since your last turn"), "{latest}");
    assert!(latest.contains("check the login change"));
    assert!(latest.contains("one sentence on what was just checked"));
    assert!(!latest.contains("plan a thing"));
    send(&app, "and again");
    assert!(!planner.calls().last().unwrap().contains("<conversation>")); // Nothing new since.
}

// ---------------------------------------------------------------------------------------------------------
// Rendering on a test terminal

#[test]
fn test_the_screen_shows_the_welcome_the_prompt_and_the_keys() {
    let env = env();
    let (app, _) = resume_app(&env);
    let rows = screen(&app, 120, 40);
    let text = rows.join("\n");
    assert!(text.contains("DONUT"), "{text}");
    assert!(text.contains("Describe what to plan, or type / for commands"));
    assert!(text.contains("$0.00 spent"));
    assert!(text.contains("planner claude-opus-5-5"));
    assert!(text.contains("shift+tab Permissions"));
    assert!(text.contains("claude"));
}

#[test]
fn test_the_slash_menu_is_drawn_inline_above_the_prompt() {
    let env = env();
    let (app, _) = resume_app(&env);
    press(&app, &["/"]);
    let rows = screen(&app, 120, 40);
    let menu_row = rows
        .iter()
        .position(|row| row.contains("Commands"))
        .expect("the menu title");
    let prompt_row = rows
        .iter()
        .position(|row| row.contains("spent"))
        .expect("the prompt border");
    assert!(menu_row < prompt_row);
    assert!(rows.iter().any(|row| row.contains("/plan")));
}

#[test]
fn test_the_draw_survives_tiny_terminals() {
    let env = env();
    let (app, _) = resume_app(&env);
    press(&app, &["/"]);
    for (width, height) in [(1, 1), (10, 3), (20, 5), (40, 8)] {
        let _ = screen(&app, width, height);
    }
}

#[test]
fn test_a_bare_terminal_run_needs_a_tty() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return; // Interactive: this check is only meaningful when piped.
    }
    let env = env();
    let error = super::run(&env.project(), None, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Interactive Donut requires a terminal")
    );
}

// ---------------------------------------------------------------------------------------------------------
// Extra checks of the pure parts the Python tests only reached through the interface

#[test]
fn test_command_output_lines_lose_escapes_and_redraws() {
    assert_eq!(
        super::app::clean_output_line(b"\x1b[32mgreen\x1b[0m\r\n"),
        "green"
    );
    assert_eq!(super::app::clean_output_line(b"10%\r50%\r100%\n"), "100%");
    assert_eq!(super::app::clean_output_line(b"plain\n"), "plain");
}

#[test]
fn test_an_open_quote_in_a_command_is_plain_words() {
    assert_eq!(
        super::app::split_words("/review don't skip tests"),
        ["/review", "don't", "skip", "tests"]
    );
    assert_eq!(
        super::app::split_words("/work next 3"),
        ["/work", "next", "3"]
    );
}

#[test]
fn test_the_status_line_wraps_between_entries_only() {
    let env = env();
    let (app, _) = setup(&env, active_config());
    let lines = app.st().status_lines(40);
    assert!(lines.len() > 1);
    let joined = plain_lines(&lines);
    for entry in [
        "planner claude-opus-5-5",
        "worker qwen3.6:35b",
        "⏵ ask  (shift+tab)",
    ] {
        assert!(joined.contains(entry), "{entry} split in {joined}");
    }
}

#[test]
fn test_usage_groups_list_each_login_once_and_only_local_models_in_use() {
    let env = env();
    let (app, _) = setup(&env, active_config());
    let groups = app.st().usage_groups();
    let labels: Vec<String> = groups
        .iter()
        .map(|(_, (label, _, _))| label.clone())
        .collect();
    assert_eq!(labels, ["claude-second", "codex", "qwen3.6"]);
}

#[test]
fn test_a_hundred_events_while_drawing_do_not_deadlock() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let drawer = {
        let app = app.clone();
        std::thread::spawn(move || {
            for _ in 0..30 {
                let _ = screen(&app, 80, 24);
            }
        })
    };
    for number in 0..100 {
        app.event("text", &json!(format!("line {number}\n\n")), None, None);
    }
    drawer.join().unwrap();
    app.st().answer();
    assert!(body(&app).contains("line 99"));
}

// ---------------------------------------------------------------------------------------------------------
// McpReloadTests and the raw provider requests behind /context, /mcp and /plugin

const MCP_LISTING: &str = "Checking MCP server health…\n\n\
plugin:postman:postman: https://mcp.example.com/mcp (HTTP) - ! Needs authentication\n\
claude.ai Notes: https://notes.example.com/mcp (HTTP) - \u{2713} Connected\n";

fn set_listing(app: &App, listing: &'static str) {
    app.set_mcp_list(Arc::new(move |_, _| {
        crate::summaries::parse_mcp_list(listing)
    }));
}

#[test]
fn test_mcp_reopens_a_session_that_started_without_a_connector() {
    assert_eq!(
        crate::summaries::parse_mcp_list(MCP_LISTING),
        [
            (
                "plugin:postman:postman".to_string(),
                "! Needs authentication".to_string()
            ),
            (
                "claude.ai Notes".to_string(),
                "\u{2713} Connected".to_string()
            ),
        ]
    );
    let env = env();
    let fakes = Fakes::new();
    // The first process started before the connector was connected; a new one loads it.
    fakes.on_control(|number, method, _| {
        assert_eq!(method, "mcp_status");
        let mut servers = vec![json!({"name": "plugin:postman:postman", "status": "needs-auth"})];
        if number > 1 {
            servers.push(json!({"name": "claude.ai Notes", "status": "connected"}));
        }
        Ok(json!({"mcpServers": servers}))
    });
    let (app, fakes) = setup_with(&env, claude_config(), fakes, AppOptions::test());
    set_listing(&app, MCP_LISTING);
    send(&app, "remember the number 7");
    let first = fakes.last();
    send(&app, "/mcp");
    let text = body(&app);
    assert_eq!(fakes.all().len(), 2, "{text}");
    assert!(first.closed.load(Ordering::SeqCst));
    assert!(
        text.contains("claude.ai Notes connected after this session started"),
        "{text}"
    );
    assert!(
        text.split("MCP servers")
            .last()
            .unwrap()
            .contains("claude.ai Notes"),
        "{text}"
    );
    assert!(!text.contains("not in this session"), "{text}");
    let fresh = fakes.last();
    assert_eq!(fresh.resume_id.as_deref(), Some("provider-1")); // The same conversation, reopened.
    assert_eq!(fresh.controls.lock().unwrap()[0].0, "mcp_status");
    assert!(
        app.st()
            .sessions
            .iter()
            .any(|rec| rec.session.provider_id().as_deref() == Some("provider-1"))
    );
    assert_eq!(app.st().sessions.len(), 1);
}

#[test]
fn test_mcp_lists_what_the_login_has_but_the_session_lacks() {
    let env = env();
    let fakes = Fakes::new();
    fakes.on_control(|_, _, _| {
        Ok(json!({"mcpServers": [{"name": "plugin:postman:postman", "status": "connected"}]}))
    });
    let (app, fakes) = setup_with(&env, claude_config(), fakes, AppOptions::test());
    set_listing(
        &app,
        "plugin:postman:postman: https://m (HTTP) - \u{2713} Connected\nold: https://o (HTTP) - \u{2717} Failed to connect\n",
    );
    send(&app, "/mcp");
    let text = body(&app);
    assert_eq!(fakes.all().len(), 1, "{text}");
    assert!(
        text.contains("Set up for this login but not in this session:"),
        "{text}"
    );
    assert!(text.contains("○ old  \u{2717} Failed to connect"), "{text}");
    assert!(!text.contains("Run /mcp again"), "{text}");
}

#[test]
fn test_mcp_does_not_reopen_during_a_turn() {
    let env = env();
    let fakes = Fakes::new();
    fakes.on_control(|_, _, _| Ok(json!({"mcpServers": []})));
    let (app, fakes) = setup_with(&env, claude_config(), fakes, AppOptions::test());
    set_listing(&app, MCP_LISTING);
    send(&app, "hello");
    // A turn is running: its session is the active one.
    let handle = app.st().sessions[0].handle();
    {
        let mut s = app.st();
        s.active = Some(handle);
        s.busy = true;
    }
    app.execute("/mcp");
    wait("the /mcp answer", || body(&app).contains("Run /mcp again"));
    let text = body(&app);
    assert_eq!(fakes.all().len(), 1, "{text}");
    assert!(
        text.contains("Run /mcp again when the current turn ends to load them."),
        "{text}"
    );
}

#[test]
fn test_context_for_claude_asks_the_session_for_its_usage() {
    let env = env();
    let fakes = Fakes::new();
    fakes.on_control(|_, method, _| {
        assert_eq!(method, "get_context_usage");
        Ok(json!({
            "categories": [
                {"name": "System prompt", "tokens": 3100},
                {"name": "Messages", "tokens": 41000},
                {"name": "Free space", "tokens": 150000},
            ],
            "totalTokens": 44100,
            "maxTokens": 200000,
            "percentage": 22.0,
            "model": "claude-opus-5-5",
        }))
    });
    let (app, fakes) = setup_with(&env, claude_config(), fakes, AppOptions::test());
    send(&app, "/context");
    let text = body(&app);
    assert!(text.contains("Context · claude-opus-5-5"), "{text}");
    assert!(text.contains("44.1k / 200.0k tokens  22% used"), "{text}");
    assert!(!text.contains("/native"), "{text}");
    assert_eq!(fakes.last().controls.lock().unwrap().len(), 1);
}

fn codex_config() -> Value {
    json!({"aliases": {"codex": {"engine": "codex", "model": "gpt-6"}}, "planner": "codex"})
}

#[test]
fn test_mcp_for_codex_lists_its_servers() {
    let env = env();
    let fakes = Fakes::new();
    fakes.on_control(|_, method, params| {
        assert_eq!((method, params), ("mcpServerStatus/list", &json!({})));
        Ok(json!({"data": [{
            "name": "postman-mcp",
            "authStatus": "notLoggedIn",
            "tools": {"a": {}},
            "resources": [],
            "resourceTemplates": [],
        }]}))
    });
    let (app, fakes) = setup_with(&env, codex_config(), fakes, AppOptions::test());
    send(&app, "/mcp");
    let text = body(&app);
    assert!(text.contains("MCP servers · codex"), "{text}");
    assert!(text.contains("postman-mcp  needs login"), "{text}");
    assert_eq!(fakes.last().controls.lock().unwrap().len(), 1);
}

#[test]
fn test_plugin_for_codex_lists_the_plugins_to_pick_from() {
    let env = env();
    let fakes = Fakes::new();
    fakes.on_control(|_, method, _| {
        assert_eq!(method, "plugin/list");
        Ok(json!({"marketplaces": [{"plugins": [
            {"name": "github", "installed": true, "description": "Pull requests"},
            {"name": "linear", "installed": false, "description": ""},
        ]}]}))
    });
    let (app, _) = setup_with(&env, codex_config(), fakes, AppOptions::test());
    app.execute("/plugin");
    settle(&app);
    assert_eq!(labels(&app), ["github", "linear"]);
    let rows = app.st().menu.visible_rows.clone();
    assert_eq!(rows[0].description, "installed · Pull requests");
    assert_eq!(rows[1].description, "not installed");
    press(&app, &["enter"]);
    settle(&app);
    let text = body(&app);
    assert!(
        text.contains("  github  installed  Pull requests"),
        "{text}"
    );
    assert!(
        text.contains("Install or remove plugins with /native."),
        "{text}"
    );
}

#[test]
fn test_qwen_still_points_to_native_for_raw_controls() {
    let env = env();
    let config =
        json!({"aliases": {"local": {"engine": "ollama", "model": "qwen3"}}, "planner": "local"});
    let (app, _) = setup(&env, config);
    send(&app, "/mcp");
    assert!(body(&app).contains("Use /native for Qwen plugins, MCP and context controls."));
}

// ---------------------------------------------------------------------------------------------------------
// Model questions (Claude's AskUserQuestion, Codex's request_user_input)

fn spawn_questions(
    app: &App,
    questions: Vec<crate::sessions::Question>,
) -> std::sync::mpsc::Receiver<Option<crate::sessions::Answers>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = sender.send(app.answer_questions(&questions));
    });
    receiver
}

fn question(id: &str, options: &[&str], multi: bool) -> crate::sessions::Question {
    crate::sessions::Question {
        id: id.into(),
        header: "Color".into(),
        question: "Which color do you like?".into(),
        options: options
            .iter()
            .map(|o| (o.to_string(), String::new()))
            .collect(),
        multi,
        other: true,
        secret: false,
    }
}

#[test]
fn test_questions_pick_an_option_toggle_several_or_type_something_else() {
    let env = env();
    let (app, _) = setup(&env, claude_config());
    let sent = recorded(&app);
    // One option, by number.
    let answer = spawn_questions(&app, vec![question("color", &["Red", "Blue"], false)]);
    wait("menu open", || menu_open(&app));
    assert_eq!(app.st().menu.title, "Color");
    assert_eq!(
        labels(&app),
        ["Red", "Blue", "Something else (type your answer)"]
    );
    let details = plain_lines(&app.st().menu.details.clone());
    assert_eq!(details, "Which color do you like?"); // The question, not a JSON dump.
    press(&app, &["2"]);
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(5)).unwrap(),
        Some(vec![("color".to_string(), vec!["Blue".to_string()])])
    );
    // Something else: the typed prompt answers it, and does not go to the model or the history.
    let answer = spawn_questions(&app, vec![question("color", &["Red", "Blue"], false)]);
    wait("menu open", || menu_open(&app));
    press(&app, &["3"]);
    wait("typing", || app.st().typing.is_some());
    app.st().draft.load_text("Green");
    press(&app, &["enter"]);
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(5)).unwrap(),
        Some(vec![("color".to_string(), vec!["Green".to_string()])])
    );
    assert!(sent.lock().unwrap().is_empty());
    assert!(!app.st().history.contains(&"Green".to_string()));
    // Multi-select: toggle two, untoggle one, then Done.
    let answer = spawn_questions(
        &app,
        vec![question("colors", &["Red", "Blue", "Gold"], true)],
    );
    let marks = |expected: [&str; 3]| {
        wait("rows", || {
            labels(&app).get(..3) == Some(&expected.map(String::from)[..])
        })
    };
    marks(["[ ] Red", "[ ] Blue", "[ ] Gold"]);
    press(&app, &["1"]);
    marks(["[x] Red", "[ ] Blue", "[ ] Gold"]);
    press(&app, &["3"]);
    marks(["[x] Red", "[ ] Blue", "[x] Gold"]);
    press(&app, &["1"]);
    marks(["[ ] Red", "[ ] Blue", "[x] Gold"]);
    press(&app, &["5"]); // Done, after the three options and Something else.
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(5)).unwrap(),
        Some(vec![("colors".to_string(), vec!["Gold".to_string()])])
    );
    // Esc dismisses; Ctrl+C cancels a typed answer.
    let answer = spawn_questions(&app, vec![question("color", &["Red"], false)]);
    wait("menu open", || menu_open(&app));
    press(&app, &["escape"]);
    assert_eq!(answer.recv_timeout(Duration::from_secs(5)).unwrap(), None);
    let answer = spawn_questions(&app, vec![question("name", &[], false)]);
    wait("typing", || app.st().typing.is_some());
    app.action_cancel();
    assert_eq!(answer.recv_timeout(Duration::from_secs(5)).unwrap(), None);
    assert!(app.st().typing.is_none());
}

// ---------------------------------------------------------------------------------------------------------
// /harness

#[test]
fn test_harness_runs_a_local_model_in_claude_code_and_is_saved_per_mode() {
    let env = env();
    let config =
        json!({"aliases": {"local": {"engine": "ollama", "model": "qwen3.6"}}, "planner": "local"});
    let (app, fakes) = setup(&env, config);
    let role = app.st().role();
    send(&app, "/harness claude");
    assert_eq!(load_config()["harness"][&role], "claude"); // Saved for the next window.
    assert_eq!(app.st().prof(&role)["harness"], "claude");
    send(&app, "hello");
    let made = fakes.made.lock().unwrap().last().cloned().unwrap();
    let profile = made.profile.lock().unwrap().clone();
    assert_eq!(made.family, "claude"); // Claude Code's session, not Donut's own loop for Ollama.
    assert_eq!(
        (
            profile["engine"].as_str(),
            profile["model_engine"].as_str(),
            profile["model"].as_str()
        ),
        (Some("claude"), Some("ollama"), Some("qwen3.6"))
    );
    assert_eq!(profile["env"]["ANTHROPIC_AUTH_TOKEN"], "ollama");
    send(&app, "/harness sideways");
    assert_eq!(load_config()["harness"][&role], "claude"); // An unknown harness changes nothing.
    send(&app, "/harness auto");
    assert!(load_config()["harness"].get(&role).is_none());
    assert!(app.st().prof(&role).get("harness").is_none());
}

#[test]
fn test_harness_refuses_a_cross_vendor_run_without_a_gateway() {
    let env = env();
    let config =
        json!({"aliases": {"gpt": {"engine": "codex", "model": "gpt-6.2"}}, "planner": "gpt"});
    let (app, _) = setup(&env, config);
    send(&app, "/harness claude");
    assert!(body(&app).contains("gateway_url"), "{}", body(&app));
    assert!(load_config().get("harness").is_none()); // Not saved.
}

// ---------------------------------------------------------------------------------------------------------
// Ponytail out of sight, and /ponytail-options

/// A Claude login with Ponytail installed and enabled, in a config dir of its own.
fn ponytail_config(env: &Env) -> Value {
    let claude = env.temp.path().join("claude-ponytail");
    let plugin = claude.join("plugins/cache/ponytail/ponytail/5.3.3");
    for (path, text) in [
        (plugin.join("skills/ponytail/SKILL.md"), "---\nname: ponytail\ndescription: Lazy senior dev mode.\n---\n".to_string()),
        (plugin.join("skills/ponytail-review/SKILL.md"), "---\nname: ponytail-review\ndescription: Review a change.\n---\n".to_string()),
        (
            claude.join("plugins/installed_plugins.json"),
            json!({"version": 2, "plugins": {"ponytail@ponytail": [{"scope": "user", "installPath": plugin}]}}).to_string(),
        ),
        (claude.join("settings.json"), json!({"enabledPlugins": {"ponytail@ponytail": true}}).to_string()),
        (claude.join(".ponytail-active"), "full".to_string()),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    json!({"aliases": {"claude": {"engine": "claude", "model": "claude-opus-5-5", "claude_config_dir": claude}}, "planner": "claude"})
}

#[test]
fn test_ponytail_runs_out_of_sight_and_ponytail_options_holds_its_menu() {
    let env = env();
    let (app, _) = setup(&env, ponytail_config(&env));
    let profile = app.st().prof("plan");
    assert!(app.st().has_ponytail(&profile));
    assert!(app.st().band_line().is_none()); // Installed, but no band by default.
    let names: Vec<String> = app.slash_rows().into_iter().map(|row| row.label).collect();
    assert!(
        names.contains(&"/ponytail-options".to_string()),
        "{names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("/ponytail") && name != "/ponytail-options"),
        "{names:?}"
    );
    // The menu: the four levels with the live one marked, Ponytail's commands, and the band switch.
    let (sender, done) = std::sync::mpsc::channel();
    {
        let app = app.clone();
        std::thread::spawn(move || {
            let _ = sender.send(app.command("/ponytail-options").is_ok());
        });
    }
    wait("menu open", || menu_open(&app));
    assert_eq!(app.st().menu.title, "Ponytail · now full");
    assert_eq!(
        labels(&app),
        [
            "Lite",
            "Full (default) · now",
            "Ultra",
            "Off",
            "/ponytail-review",
            "Show the band"
        ]
    );
    assert_eq!(values(&app)[0], json!("/ponytail lite"));
    press(&app, &["down", "down", "down", "down", "down"]);
    assert_eq!(
        app.st()
            .menu
            .current()
            .map(|row| row.label.clone())
            .as_deref(),
        Some("Show the band")
    );
    press(&app, &["enter"]);
    assert!(done.recv_timeout(Duration::from_secs(5)).unwrap());
    assert_eq!(load_config()["ponytail_band"], json!(true));
    assert!(app.st().band_line().is_some());
}
