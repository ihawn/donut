//! The parts of Python's `Donut` app that wait: on a provider, on the user's menu choice, on a subprocess.
//!
//! `App` is a cheap handle (`Arc`) to the shared [`State`]. Worker threads run the Python coroutines; they lock
//! the state for short steps and never hold the lock while they block.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ratatui::style::Color;
use ratatui::text::Line;
use serde_json::{Value, json};

use super::app::{
    AppOptions, Deferred, Queued, SessionHandle, SessionRec, SpendTracker, State, bold,
    clean_output_line, engine_of, fg, is_empty_profile, span, styled, terminate_group, text_of,
};
use super::render::{
    self, CORAL, DIM, GRAY, ORANGE, TEAL, model_name, role_command, role_title, usage_key,
};
use super::widgets::{MenuKind, MenuRow};
use crate::config::{self, config_path, forget_role, remember_role};
use crate::sessions::{Answers, Approve, Ask, Decision, Emit, Question, Session, SessionOptions};
use crate::{
    background, conversations, effort, handholding, plugins, ponytail, routing, spend, workspace,
};

/// What `new_session` is given, plus the alias and purpose so a test factory can tell sessions apart.
pub struct SessionSpec {
    pub alias: String,
    pub purpose: Option<String>,
    pub engine: String,
    pub project: std::path::PathBuf,
    pub profile: Value,
    pub options: SessionOptions,
    pub emit: Emit,
    pub approve: Approve,
}

/// Makes a provider session for a spec, or None when Donut has none for the engine.
pub type SessionFactory = Arc<dyn Fn(&SessionSpec) -> Option<Arc<dyn Session>> + Send + Sync>;

/// What `/work` starts.
pub struct RunnerSpec {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: std::path::PathBuf,
}

impl RunnerSpec {
    /// A command for `program` with this spec's arguments and environment, output piped, in its own group.
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .args(&self.args)
            .envs(self.env.iter().cloned())
            .current_dir(&self.cwd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::platform::process_options(&mut command);
        command
    }
}

/// Starts the batch runner. The default runs `donut --work ...` from this executable.
pub type RunnerLauncher = Arc<dyn Fn(&RunnerSpec) -> std::io::Result<Child> + Send + Sync>;
pub type Executor = Arc<dyn Fn(&str) + Send + Sync>;
/// Opens Donut for a project in a new terminal window and says where, or None when it could not.
pub type OpenWindow = Arc<dyn Fn(&Path) -> Option<String> + Send + Sync>;
pub type Installer = Arc<dyn Fn(&Path, Option<&str>) -> Result<()> + Send + Sync>;

#[derive(Debug)]
pub struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timed out")
    }
}

impl std::error::Error for TimedOut {}

/// What `App::session` can be asked besides the model.
#[derive(Clone, Debug)]
pub struct SessionRequest {
    pub resume: Option<String>,
    pub conversation: Option<String>,
    pub track: bool,
}

impl Default for SessionRequest {
    fn default() -> Self {
        SessionRequest {
            resume: None,
            conversation: None,
            track: true,
        }
    }
}

impl SessionRequest {
    pub fn untracked() -> Self {
        SessionRequest {
            track: false,
            ..SessionRequest::default()
        }
    }
}

pub type McpLister = Arc<dyn Fn(&Path, &Value) -> Vec<(String, String)> + Send + Sync>;

pub struct Hooks {
    pub factory: SessionFactory,
    pub runner: RunnerLauncher,
    pub executor: Option<Executor>,
    pub installer: Installer,
    pub open_url: Arc<dyn Fn(&str) + Send + Sync>,
    pub copy_native: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// `claude mcp list` for a project and profile: (server, status) rows.
    pub mcp_list: McpLister,
    pub open_window: OpenWindow,
    /// Leave (false) or re-enter (true) the full-screen interface around a child that owns the terminal.
    pub terminal: Option<Arc<dyn Fn(bool) + Send + Sync>>,
}

fn default_factory() -> SessionFactory {
    Arc::new(|spec: &SessionSpec| {
        crate::sessions::new_session(
            &spec.engine,
            &spec.project,
            spec.profile.clone(),
            spec.options.clone(),
            Arc::clone(&spec.emit),
            Arc::clone(&spec.approve),
        )
    })
}

fn default_runner() -> RunnerLauncher {
    Arc::new(|spec: &RunnerSpec| {
        let exe = std::env::current_exe()?;
        spec.command(exe).spawn()
    })
}

fn default_open_url(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

impl Hooks {
    fn new() -> Hooks {
        Hooks {
            factory: default_factory(),
            runner: default_runner(),
            executor: None,
            installer: Arc::new(|project, session| {
                crate::install_skills::install(project, &plugins::home_dir(), true, session)
                    .map(|_| ())
            }),
            open_url: Arc::new(default_open_url),
            copy_native: Arc::new(crate::platform::copy_native),
            mcp_list: Arc::new(super::commands::claude_mcp_list),
            open_window: Arc::new(crate::platform::open_window),
            terminal: None,
        }
    }
}

pub struct Shared {
    pub state: Mutex<State>,
    pub hooks: Mutex<Hooks>,
    pub workers: AtomicUsize,
    pub cancelled: AtomicBool,
    pub cancel_waits: Arc<AtomicBool>,
    pub quota_busy: AtomicBool,
    pub suspended: AtomicBool,
    pub wait_ids: AtomicU64,
}

/// The interactive terminal: a handle that is cheap to clone and share with worker threads.
#[derive(Clone)]
pub struct App(pub Arc<Shared>);

fn wrong(text: impl Into<String>) -> anyhow::Error {
    anyhow!(text.into())
}

impl App {
    /// `Donut(project, alias, session)`: loads the saved configuration. Call [`App::start`] to open the window.
    pub fn new(project: &Path, alias: Option<&str>, session: Option<&str>) -> Result<App> {
        App::with_options(project, alias, session, AppOptions::default())
    }

    pub fn with_options(
        project: &Path,
        alias: Option<&str>,
        session: Option<&str>,
        options: AppOptions,
    ) -> Result<App> {
        let config = config::load()?;
        let state = State::new(project, alias, session, &options, config)?;
        Ok(App(Arc::new(Shared {
            state: Mutex::new(state),
            hooks: Mutex::new(Hooks::new()),
            workers: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            cancel_waits: Arc::new(AtomicBool::new(false)),
            quota_busy: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            wait_ids: AtomicU64::new(0),
        })))
    }

    pub fn st(&self) -> MutexGuard<'_, State> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn hooks(&self) -> MutexGuard<'_, Hooks> {
        self.0
            .hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_session_factory(&self, factory: SessionFactory) {
        self.hooks().factory = factory;
    }

    pub fn set_runner(&self, runner: RunnerLauncher) {
        self.hooks().runner = runner;
    }

    pub fn set_executor(&self, executor: Executor) {
        self.hooks().executor = Some(executor);
    }

    pub fn set_installer(&self, installer: Installer) {
        self.hooks().installer = installer;
    }

    pub fn set_open_url(&self, open: Arc<dyn Fn(&str) + Send + Sync>) {
        self.hooks().open_url = open;
    }

    pub fn set_copy_native(&self, copy: Arc<dyn Fn(&str) -> bool + Send + Sync>) {
        self.hooks().copy_native = copy;
    }

    pub fn set_mcp_list(&self, list: McpLister) {
        self.hooks().mcp_list = list;
    }

    pub fn set_open_window(&self, open: OpenWindow) {
        self.hooks().open_window = open;
    }

    pub fn set_terminal_hook(&self, hook: Arc<dyn Fn(bool) + Send + Sync>) {
        self.hooks().terminal = Some(hook);
    }

    // ---- small shortcuts -------------------------------------------------------------------------------

    pub fn say(&self, text: &str, color: Option<Color>) {
        self.st().say(text, color);
    }

    pub fn say_dim(&self, text: &str) {
        self.st().say(text, Some(DIM));
    }

    pub fn say_line(&self, line: Line<'static>) {
        self.st().say_line(line);
    }

    pub fn status(&self, activity: Option<&str>) {
        self.st().status(activity);
    }

    pub fn busy(&self) -> bool {
        self.st().busy
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Run `work` on a worker thread, counted so tests (and quit) can wait for it.
    pub fn spawn<F: FnOnce(App) + Send + 'static>(&self, work: F) {
        /// Counts the worker out even when it panics, so nothing waits for it forever.
        struct Done(App);
        impl Drop for Done {
            fn drop(&mut self) {
                self.0.0.workers.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.0.workers.fetch_add(1, Ordering::SeqCst);
        let app = self.clone();
        std::thread::spawn(move || {
            let _done = Done(app.clone());
            work(app);
        });
    }

    pub fn workers(&self) -> usize {
        self.0.workers.load(Ordering::SeqCst)
    }

    /// Whether every worker is done or one waits on a menu choice.
    pub fn idle(&self) -> bool {
        let s = self.st();
        self.workers() == 0 || s.picking.is_some() || s.typing.is_some()
    }

    /// Run what the state asked for while it was locked.
    pub fn run_deferred(&self) {
        let deferred = std::mem::take(&mut self.st().deferred);
        for action in deferred {
            match action {
                Deferred::RunQueued => self.run_next_queued(),
                Deferred::RefreshQuotas => self.refresh_quotas_soon(false),
                Deferred::FinishWake(id) => self.finish_wake(id),
            }
        }
    }

    /// `Donut.event`: everything a provider session reports. `source` is the session that reported.
    pub fn event(&self, kind: &str, data: &Value, key: Option<&[String]>, source: Option<u64>) {
        self.st().event(kind, data, key, source);
        self.run_deferred();
    }

    // ---- opening ---------------------------------------------------------------------------------------

    /// `on_mount`: greet, restore the session's conversations, start the background checks.
    pub fn start(&self) {
        let (ask, auto_refresh) = {
            let mut s = self.st();
            s.terminal_out.push("\x1b[22;0t".to_string()); // Save the terminal's own title, restored on exit.
            conversations::remember_project(&s.project);
            s.welcome();
            s.refresh_spent_label();
            if s.entered_session {
                // Opened in a named session (donut -s NAME): its conversations go on, they do not start again.
                let name = s.session_name.clone();
                let mut mine: Vec<Value> = conversations::for_project(&s.project)
                    .into_iter()
                    .filter(|r| text_of(r, "session").map(str::to_string) == name)
                    .collect();
                if s.simple() {
                    mine.retain(|r| text_of(r, "mode") == Some("solo"));
                }
                s.continue_session(&mine, None);
                if !s.waiting_resume.is_empty() {
                    let modes = s
                        .waiting_resume
                        .iter()
                        .map(|(mode, _)| mode.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let session = name.unwrap_or_default();
                    s.say(
                        &format!("  Session {session} · its conversations continue ({modes})."),
                        Some(DIM),
                    );
                }
            }
            s.status(Some("Ready"));
            let configured = s.config.get("aliases").is_some_and(render::truthy);
            if !configured {
                s.say(
                    "No accounts configured. Exit and run donut setup.",
                    Some(CORAL),
                );
            }
            let ask = s.options.ask_interface
                && configured
                && s.config
                    .get("interface")
                    .and_then(Value::as_str)
                    .is_none_or(|name| !render::INTERFACES.iter().any(|(key, _)| *key == name));
            (ask, s.options.auto_refresh_quotas)
        };
        if ask {
            self.spawn(|app| app.first_open());
        }
        if auto_refresh {
            self.refresh_quotas_soon(true);
        }
    }

    pub fn refresh_quotas_soon(&self, connect: bool) {
        if self.0.quota_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        self.spawn(move |app| {
            app.refresh_quotas(connect);
            app.0.quota_busy.store(false, Ordering::SeqCst);
        });
    }

    /// Ask each account how much of its plan is left, without sending any prompt.
    ///
    /// `connect` starts a session for an account that has none yet; otherwise only open sessions are asked.
    pub fn refresh_quotas(&self, connect: bool) {
        let groups = self.st().usage_groups();
        for (key, (_label, alias, profile)) in groups {
            if key[0] == "local" {
                continue;
            }
            let existing = {
                let s = self.st();
                s.sessions
                    .iter()
                    .find(|rec| usage_key(&rec.profile) == key)
                    .map(SessionRec::handle)
            };
            let handle = match existing {
                Some(handle) => handle,
                None if connect => {
                    match self.session(
                        alias.as_deref().unwrap_or(""),
                        &profile,
                        None,
                        SessionRequest::untracked(),
                    ) {
                        Ok(handle) => handle,
                        Err(_) => continue, // Not logged in, offline, or the provider is not installed.
                    }
                }
                None => continue,
            };
            if let Some(windows) = handle.session.quota() {
                let mut s = self.st();
                s.quotas.insert(key, windows);
                s.status(None);
            }
        }
    }

    // ---- menus and approvals -----------------------------------------------------------------------------

    /// Show rows in the inline menu and wait for a choice. Returns the chosen value, or None.
    ///
    /// A "pick" uses the prompt as its filter, so the prompt is cleared around it. An "approve" leaves whatever
    /// the user is drafting alone.
    pub fn choose(
        &self,
        title: &str,
        rows: Vec<MenuRow>,
        kind: MenuKind,
        details: Option<Vec<Line<'static>>>,
        subtitle: &str,
    ) -> Option<Value> {
        if rows.is_empty() {
            return None;
        }
        let (sender, receiver) = mpsc::channel();
        {
            let mut s = self.st();
            s.picking = Some(sender);
            if kind == MenuKind::Pick {
                s.draft.clear();
            }
            s.menu.open(kind, title, rows, "", details, subtitle);
        }
        let answer = receiver.recv().ok().flatten();
        let mut s = self.st();
        s.picking = None;
        s.menu.close();
        if kind == MenuKind::Pick {
            s.draft.clear();
        }
        answer
    }

    pub fn pick(&self, title: &str, rows: Vec<MenuRow>) -> Option<Value> {
        self.choose(title, rows, MenuKind::Pick, None, "")
    }

    pub fn pick_with(&self, title: &str, rows: Vec<MenuRow>, subtitle: &str) -> Option<Value> {
        self.choose(title, rows, MenuKind::Pick, None, subtitle)
    }

    /// Ask about one tool call. Returns `Yes`, `Always` (for the rest of this session) or `No`.
    pub fn approve(&self, name: &str, details: &Value) -> Decision {
        let details = if details.is_object() {
            details.clone()
        } else {
            json!({"input": details})
        };
        let (title, short) = render::approval_title(name, &details);
        if self.st().always_allow.contains(&title) {
            return Decision::Always;
        }
        let rows = vec![
            MenuRow::new("Yes", json!("yes")),
            MenuRow::new(
                format!("Yes, and don't ask again for {short} this session"),
                json!("always"),
            ),
            MenuRow::new("No", json!("no")),
        ];
        let answer = self.choose(
            &title,
            rows,
            MenuKind::Approve,
            Some(render::approval_details(&details)),
            "Enter to choose · Esc to deny",
        );
        let decision = Decision::parse(answer.as_ref().and_then(Value::as_str).unwrap_or("no"));
        let mut s = self.st();
        if decision == Decision::Always {
            s.always_allow.insert(title.clone());
        }
        let summary = render::approval_summary(&details);
        let allowed = decision != Decision::No;
        let mut line = vec![
            span(
                if allowed { "  ✓ " } else { "  ✗ " },
                fg(if allowed { TEAL } else { CORAL }),
            ),
            span(
                format!("{}{}", if allowed { "Allowed " } else { "Denied " }, title),
                fg(GRAY),
            ),
        ];
        if !summary.is_empty() {
            line.push(span(format!("  {summary}"), fg(DIM)));
        }
        s.say_line(Line::from(line));
        decision
    }

    /// Show a model's questions one at a time and collect the answers. `None` when the user dismissed them,
    /// so the model is told no answer came. Options are numbered rows; a multi-select question toggles rows
    /// until Done; "Something else" (or a question with no options) takes a typed answer from the prompt.
    pub fn answer_questions(&self, questions: &[Question]) -> Option<Answers> {
        const OTHER: &str = "Something else (type your answer)";
        let mut answers = Answers::new();
        for question in questions {
            let title = match question.header.trim() {
                "" => "Question".to_string(),
                header => header.to_string(),
            };
            let details = vec![styled(question.question.clone(), bold(Color::White))];
            let subtitle = "Enter to choose · Esc to dismiss";
            let options = &question.options;
            let row =
                |at: usize, label: String| MenuRow::new(label, json!(at)).described(&options[at].1);
            let mut chosen = Vec::new();
            if options.is_empty() {
                chosen.push(self.read_answer(question)?);
            } else if question.multi {
                let mut picked = vec![false; options.len()];
                loop {
                    let mut rows: Vec<MenuRow> = (0..options.len())
                        .map(|at| {
                            let mark = if picked[at] { "[x] " } else { "[ ] " };
                            row(at, format!("{mark}{}", options[at].0))
                        })
                        .collect();
                    if question.other {
                        rows.push(MenuRow::new(OTHER, json!("other")));
                    }
                    let count = picked.iter().filter(|p| **p).count();
                    rows.push(
                        MenuRow::new("Done", json!("done")).described(&format!("{count} chosen")),
                    );
                    let choice = self.choose(
                        &title,
                        rows,
                        MenuKind::Approve,
                        Some(details.clone()),
                        subtitle,
                    )?;
                    match choice.as_str() {
                        Some("done") => break,
                        Some("other") => {
                            let typed = self.read_answer(question)?;
                            chosen.push(typed);
                            break;
                        }
                        _ => {
                            if let Some(at) = choice
                                .as_u64()
                                .map(|at| at as usize)
                                .filter(|at| *at < picked.len())
                            {
                                picked[at] = !picked[at];
                            }
                        }
                    }
                }
                let mut labels: Vec<String> = (0..options.len())
                    .filter(|at| picked[*at])
                    .map(|at| options[at].0.clone())
                    .collect();
                labels.append(&mut chosen);
                chosen = labels;
            } else {
                let mut rows: Vec<MenuRow> = (0..options.len())
                    .map(|at| row(at, options[at].0.clone()))
                    .collect();
                if question.other {
                    rows.push(MenuRow::new(OTHER, json!("other")));
                }
                let choice =
                    self.choose(&title, rows, MenuKind::Approve, Some(details), subtitle)?;
                match choice.as_u64() {
                    Some(at) if (at as usize) < options.len() => {
                        chosen.push(options[at as usize].0.clone())
                    }
                    _ => chosen.push(self.read_answer(question)?),
                }
            }
            let shown = if question.secret {
                "(hidden)".to_string()
            } else if chosen.is_empty() {
                "(none)".to_string()
            } else {
                chosen.join(", ")
            };
            self.say_line(Line::from(vec![
                span("  ? ", fg(TEAL)),
                span(format!("{title} · "), fg(GRAY)),
                span(shown, fg(ORANGE)),
            ]));
            answers.push((question.id.clone(), chosen));
        }
        Some(answers)
    }

    /// Wait for the user to type an answer in the prompt and press Enter. `None` if they cancelled.
    fn read_answer(&self, question: &Question) -> Option<String> {
        let asked = match question.question.trim() {
            "" => "The model asks a question.".to_string(),
            text => text.to_string(),
        };
        let (sender, receiver) = mpsc::channel();
        {
            let mut s = self.st();
            s.say_line(Line::from(vec![
                span("  ? ", fg(TEAL)),
                span(asked, bold(Color::White)),
            ]));
            s.say_line(styled(
                "    Type your answer and press Enter. Ctrl+C cancels.",
                fg(DIM),
            ));
            s.typing = Some(sender);
        }
        let answer = receiver.recv().ok().flatten();
        self.st().typing = None;
        answer
    }

    // ---- permission ------------------------------------------------------------------------------------

    /// Apply a permission mode to every open session through the provider's own mechanism, and remember it.
    pub fn set_permission(&self, mode: &str, announce: bool) {
        let sessions = {
            let mut s = self.st();
            if mode != s.permission {
                s.previous_permission = s.permission.clone();
            }
            s.permission = mode.to_string();
            let label = if s.busy { "Thinking…" } else { "Ready" };
            s.status(Some(label));
            s.sessions
                .iter()
                .map(|rec| Arc::clone(&rec.session))
                .collect::<Vec<_>>()
        };
        match self.save_config_key("permission_mode", json!(mode)) {
            Ok(()) => {
                self.st().config["permission_mode"] = json!(mode);
            }
            Err(error) => self.say_dim(&format!(
                "Permission mode applies to this session only: {error}"
            )),
        }
        let mut failures: Vec<String> = Vec::new();
        for session in sessions {
            if let Err(error) = session.set_permission(mode) {
                let text = error.to_string();
                if !failures.contains(&text) {
                    failures.push(text);
                }
            }
        }
        failures.sort();
        for error in failures {
            // Once per reason, not once per open session.
            self.say(
                &format!("Could not change the provider's permission mode: {error}"),
                Some(CORAL),
            );
        }
        if announce {
            let (label, description) = crate::providers::PERMISSION_MODES
                .iter()
                .find(|(key, _, _)| *key == mode)
                .map(|(_, label, description)| (*label, *description))
                .unwrap_or(("", ""));
            let engine = {
                let s = self.st();
                engine_of(&s.prof(&s.role())).map(str::to_string)
            };
            let mut s = self.st();
            let mut line = vec![
                span(
                    format!("  {label} mode"),
                    bold(if mode == "full" { CORAL } else { ORANGE }),
                ),
                span(format!("  {description}."), fg(GRAY)),
            ];
            if mode == "ask" && matches!(engine.as_deref(), Some("ollama" | "openai")) {
                line.push(span(
                    " Qwen can't show approval prompts here, so it edits files without asking.",
                    fg(DIM),
                ));
            }
            s.say_line(Line::from(line));
        }
    }

    pub fn cycle_permission(&self) {
        let following = {
            let s = self.st();
            let cycle = crate::providers::PERMISSION_CYCLE;
            match cycle.iter().position(|mode| *mode == s.permission) {
                Some(at) => cycle[(at + 1) % cycle.len()],
                None => "ask",
            }
        };
        self.spawn(move |app| app.set_permission(following, true));
    }

    /// Load the saved configuration, set one key and save it.
    pub fn save_config_key(&self, key: &str, value: Value) -> Result<()> {
        let mut config = config::load()?;
        config[key] = value;
        config::save(&config)
    }

    // ---- sessions --------------------------------------------------------------------------------------

    /// The persistent provider session for a model and purpose. Review-mode sessions are read-only.
    ///
    /// `resume` reopens a provider session or thread by its id (from /resume), replacing any open one. Each
    /// purpose (plan, solo, review) is one conversation whatever model holds it. When its model changes, the
    /// conversation carries over: in the same live session when the provider can switch models itself (Claude,
    /// or Codex, on the same login), otherwise by handing the new model the conversation so far. `track` false
    /// is for sessions opened only to list models or read usage.
    pub fn session(
        &self,
        alias: &str,
        profile: &Value,
        purpose: Option<&str>,
        request: SessionRequest,
    ) -> Result<SessionHandle> {
        let SessionRequest {
            mut resume,
            mut conversation,
            track,
        } = request;
        let thread = purpose.unwrap_or("plan").to_string();
        let key = State::session_key(alias, profile, purpose);
        let mut waiting: Option<Value> = None;
        let mut closing: Option<Arc<dyn Session>> = None;
        let previous: Option<u64>;
        {
            let mut s = self.st();
            if track && resume.is_none() && conversation.is_none() && s.rec_by_key(&key).is_none() {
                // First use of this mode since its session was resumed: continue its own saved conversation.
                if let Some(at) = s
                    .waiting_resume
                    .iter()
                    .position(|(name, _)| *name == thread)
                {
                    let record = s.waiting_resume.remove(at).1;
                    conversation = text_of(&record, "id").map(str::to_string);
                    // The provider's own session can be reopened only on the account that holds it.
                    resume = if text_of(&record, "alias") == Some(alias) {
                        text_of(&record, "provider_id").map(str::to_string)
                    } else {
                        None
                    };
                    waiting = Some(record);
                }
            }
            if conversation.is_some()
                && let Some(at) = s.sessions.iter().position(|rec| rec.key == key)
            {
                closing = Some(Arc::clone(&s.sessions[at].session));
                s.sessions.remove(at);
            }
            let mut prev = if track && conversation.is_none() {
                s.threads.get(&thread).copied()
            } else {
                None
            };
            if let Some(id) = prev {
                let live = s.rec(id).is_some();
                let same = s.rec_by_key(&key).is_some_and(|rec| rec.id == id);
                if !live || same {
                    prev = None;
                }
            }
            previous = prev;
        }
        if let Some(session) = closing {
            let _ = session.close();
        }
        if let Some(previous) = previous
            && self.st().rec_by_key(&key).is_none()
            && let Some(switched) = self.switch_in_place(previous, alias, profile, &key)
        {
            return Ok(switched);
        }
        if self.st().rec_by_key(&key).is_none() {
            let engine = engine_of(profile).unwrap_or("").to_string();
            let (project, permission, chrome_on, id) = {
                let mut s = self.st();
                s.next_session_id += 1;
                (
                    s.project.clone(),
                    s.permission.clone(),
                    s.chrome_on(),
                    s.next_session_id,
                )
            };
            let holder = Arc::new(AtomicU64::new(0));
            let emit: Emit = {
                let app = self.clone();
                let holder = Arc::clone(&holder);
                let usage = usage_key(profile);
                Arc::new(move |kind: &str, data: Value| {
                    let source = holder.load(Ordering::SeqCst);
                    app.event(kind, &data, Some(&usage), (source != 0).then_some(source));
                })
            };
            let approve: Approve = {
                let app = self.clone();
                Arc::new(move |name: &str, details: &Value| app.approve(name, details))
            };
            let spec = SessionSpec {
                alias: alias.to_string(),
                purpose: purpose.map(str::to_string),
                engine: engine.clone(),
                project: project.clone(),
                profile: profile.clone(),
                options: SessionOptions {
                    permission,
                    read_only: purpose == Some("review"),
                    bare: false,
                    // Claude in Chrome is on unless /chrome off was saved; review conversations never get it.
                    chrome: chrome_on && purpose != Some("review"),
                    resume_id: resume.clone(),
                    ask: Some({
                        let app = self.clone();
                        Ask(Arc::new(move |questions: &[Question]| {
                            app.answer_questions(questions)
                        }))
                    }),
                },
                emit,
                approve,
            };
            let factory = Arc::clone(&self.hooks().factory);
            let Some(session) = factory(&spec) else {
                bail!(
                    "Persistent sessions for this engine are not implemented yet. Use /native for its full interactive client."
                );
            };
            if let Err(error) = session.connect() {
                let _ = session.close();
                return Err(error);
            }
            holder.store(id, Ordering::SeqCst);
            let mut s = self.st();
            s.sessions.push(SessionRec {
                id,
                key: key.clone(),
                session,
                profile: profile.clone(),
                task: Some({
                    let mut task =
                        ponytail::Task::new(&project, &engine, text_of(profile, "model"));
                    task.level = ponytail::level(profile, &project);
                    task
                }),
                conversation_id: conversation.clone(),
                donut_seen: 0,
                wake_buffer: None,
                told_wait: false,
                donut_waits: false,
                spend: SpendTracker::default(),
                turn_spend: 0.0,
            });
        }
        let handle = self
            .st()
            .rec_by_key(&key)
            .map(SessionRec::handle)
            .ok_or_else(|| wrong("session vanished"))?;
        if let Some(previous) = previous.filter(|previous| *previous != handle.id) {
            // Another provider or login: give the new model the conversation so far, then retire the old one.
            let (old, old_name, has_exchanges) = {
                let mut s = self.st();
                let at = s.sessions.iter().position(|rec| rec.id == previous);
                match at {
                    Some(at) => {
                        let rec = s.sessions.remove(at);
                        let name = model_name(&rec.profile, rec.key.0.split('@').next());
                        (Some(rec.session), name, !s.exchanges.is_empty())
                    }
                    None => (None, String::new(), !s.exchanges.is_empty()),
                }
            };
            if let Some(old) = old {
                let _ = old.close();
            }
            if has_exchanges {
                let new_name = model_name(profile, Some(alias));
                self.say_dim(&format!(
                    "  {new_name} continues the conversation from {old_name}."
                ));
            }
        }
        if let Some(record) = waiting {
            let mut s = self.st();
            s.adopt(&record, handle.id);
            let turns = record.get("turns").and_then(Value::as_i64).unwrap_or(0);
            let mode = text_of(&record, "mode").unwrap_or(&thread).to_string();
            s.say(
                &format!(
                    "  Continues this session's {mode} conversation ({turns} turn{}).",
                    if turns == 1 { "" } else { "s" }
                ),
                Some(DIM),
            );
        }
        if track {
            self.st().threads.insert(thread, handle.id);
        }
        Ok(handle)
    }

    /// Change the model of a live session on the same login, keeping the provider's own full context.
    fn switch_in_place(
        &self,
        id: u64,
        alias: &str,
        profile: &Value,
        key: &super::app::SessionKey,
    ) -> Option<SessionHandle> {
        let (session, old_profile, old_name) = {
            let s = self.st();
            let rec = s.rec(id)?;
            (
                Arc::clone(&rec.session),
                rec.profile.clone(),
                rec.key.0.clone(),
            )
        };
        if usage_key(&old_profile) != usage_key(profile)
            || !matches!(engine_of(profile), Some("claude" | "codex"))
        {
            return None;
        }
        if let Err(error) = session.switch_model(text_of(profile, "model")) {
            self.say_dim(&format!("  Could not switch models in place ({error}); handing the conversation over instead."));
            return None;
        }
        let handle = {
            let mut s = self.st();
            let rec = s.rec_mut(id)?;
            rec.key = key.clone();
            rec.profile = profile.clone();
            rec.handle()
        };
        session.set_profile(profile.clone());
        let was = model_name(&old_profile, old_name.split('@').next());
        let now = model_name(profile, Some(alias));
        self.say_dim(&format!(
            "  {now} continues the same conversation (was {was})."
        ));
        Some(handle)
    }

    pub fn close_all_sessions(&self) {
        let sessions: Vec<Arc<dyn Session>> = {
            let mut s = self.st();
            std::mem::take(&mut s.sessions)
                .into_iter()
                .map(|rec| rec.session)
                .collect()
        };
        for session in sessions {
            let _ = session.close();
        }
    }

    // ---- one turn --------------------------------------------------------------------------------------

    /// Send one turn to the active session, then book it on the conversation's Ponytail task row.
    pub fn ask(&self, text: &str, prompt: Option<&str>) -> Result<()> {
        let handle = self
            .st()
            .active
            .clone()
            .ok_or_else(|| wrong("no active session"))?;
        let started = Instant::now();
        let mut prompt = prompt.unwrap_or(text).to_string();
        let family = handle.session.family();
        let (held, shell_note, context, told) = {
            let mut s = self.st();
            let held = s.rec_mut(handle.id).and_then(|rec| rec.wake_buffer.take());
            if let Some(held) = &held
                && !held.concat().trim().is_empty()
            {
                // Text held back for a wake-up that did not end: show it, so this turn's answer is not held back too.
                let title = s.wake_title(handle.id);
                s.say_line(styled(
                    format!("  ⏰ {title} · woken by a background task"),
                    bold(TEAL),
                ));
                let body = held.concat();
                s.say_markdown(body.trim());
            }
            let note = if s.shell_runs.is_empty() {
                String::new()
            } else {
                let note = background::shell_note(&s.shell_runs);
                s.shell_runs.clear();
                note
            };
            let context = s.context_for(handle.id, 100_000);
            let told = s.rec(handle.id).is_some_and(|rec| rec.told_wait);
            (held, note, context, told)
        };
        let _ = held;
        prompt = format!("{shell_note}{prompt}");
        prompt = format!("{context}{prompt}");
        let waits = family != "claude";
        if waits && !told {
            // Claude Code wakes itself when a background shell ends; the others learn Donut's DONUT-WAIT line.
            prompt.push_str(background::WAIT_NOTE);
            if let Some(rec) = self.st().rec_mut(handle.id) {
                rec.told_wait = true;
            }
        }
        prompt = crate::recap::with_note(text, prompt);
        let before = self.st().spent;
        handle.session.query(&prompt)?;
        self.add_spent_for(&handle);
        {
            let mut s = self.st();
            let spent = s.spent;
            if let Some(rec) = s.rec_mut(handle.id) {
                rec.turn_spend = spent - before; // Saved with the exchange, for donut stats.
            }
            // The recap is Donut's, not the answer's: keep it out of the history other models are given.
            let from = s.shown;
            if let Some(recap) = crate::recap::take(&mut s.turn_text, from) {
                s.turn_recap = Some(recap);
            }
            s.remember_exchange(handle.id, text);
        }
        let donut_waits = self.st().rec(handle.id).is_some_and(|rec| rec.donut_waits);
        if waits || donut_waits {
            let answer = self.st().turn_text.clone();
            if let Some(command) = background::wait_request(&answer) {
                self.start_wait(&handle, &command);
            }
        }
        let usage = handle.session.usage();
        let (profile, has_task) = {
            let s = self.st();
            s.rec(handle.id)
                .map(|rec| (rec.profile.clone(), rec.task.is_some()))
                .unwrap_or((json!({}), false))
        };
        if !has_task || !self.st().has_ponytail(&profile) {
            return Ok(());
        }
        let project = self.st().project.clone();
        let task = {
            let mut s = self.st();
            let Some(task) = s.rec_mut(handle.id).and_then(|rec| rec.task.as_mut()) else {
                return Ok(());
            };
            task.turn(text, started.elapsed().as_secs_f64(), &usage);
            task.level = ponytail::level(&profile, &project);
            task.clone()
        };
        if let Err(error) = ponytail::record(&task) {
            self.say_dim(&format!("Ponytail ledger not written: {error}"));
        }
        Ok(())
    }

    /// Add what a session's last turn cost to the amount on the input box.
    pub fn add_spent_for(&self, handle: &SessionHandle) {
        let usage = handle.session.usage();
        let profile = handle.session.profile();
        let account = handle.session.account();
        let mut s = self.st();
        let mut tracker = s
            .rec(handle.id)
            .map(|rec| rec.spend.clone())
            .unwrap_or_default();
        s.add_spent(Some(&usage), &profile, account.as_deref(), &mut tracker);
        if let Some(rec) = s.rec_mut(handle.id) {
            rec.spend = tracker;
        }
    }

    /// Save this exchange so /resume can reopen the conversation in the same mode and model.
    pub fn log_turn(&self, mode: &str, role: &str, text: &str) {
        let Some(handle) = self.st().active.clone() else {
            return;
        };
        let provider_id = handle.session.provider_id();
        let mut s = self.st();
        let (conversation, turn_spend) = s
            .rec(handle.id)
            .map(|rec| (rec.conversation_id.clone(), rec.turn_spend))
            .unwrap_or_default();
        let record_id = conversations::record_turn(
            conversation.as_deref(),
            &s.project,
            mode,
            role,
            &s.sel(role).unwrap_or_default(),
            text_of(&s.prof(role), "model"),
            provider_id.as_deref(),
            text,
            Some(&s.turn_text),
            s.session_name.as_deref(),
            Some((turn_spend * 1e6).round() / 1e6),
        );
        if let Some(rec) = s.rec_mut(handle.id) {
            rec.conversation_id = Some(record_id);
        }
    }

    /// Solo: send the message to the soloist as is, in its own conversation, the way Claude Code would.
    pub fn solo(&self, text: &str) -> Result<()> {
        let shown = render::label_images(text, &self.st().images);
        self.say_line(styled(format!("> {shown}"), bold(TEAL)));
        let need_route = {
            let s = self.st();
            s.auto && s.route.is_none()
        };
        if need_route && !self.choose_route(text)? {
            return Ok(());
        }
        let (alias, profile) = {
            let s = self.st();
            (s.sel("solo").unwrap_or_default(), s.prof("solo"))
        };
        let handle = self.session(&alias, &profile, Some("solo"), SessionRequest::default())?;
        {
            let mut s = self.st();
            s.active = Some(handle);
            s.name_task(text);
            s.start_turn("solo", "", "thinking");
            s.reset_turn();
        }
        self.ask(text, None)?;
        self.st().answer();
        self.log_turn("solo", "solo", text);
        Ok(())
    }

    /// Ask the reviewer to check something, in a read-only conversation of its own.
    pub fn review_chat(&self, text: &str) -> Result<()> {
        let (alias, profile) = {
            let s = self.st();
            (s.sel("review").unwrap_or_default(), s.prof("review"))
        };
        let handle = self.session(&alias, &profile, Some("review"), SessionRequest::default())?;
        {
            let mut s = self.st();
            s.active = Some(handle);
            s.name_task(text);
            s.start_turn("review", "", "reviewing");
            let shown = render::label_images(text, &s.images);
            s.say_line(styled(format!("> {shown}"), bold(ORANGE)));
            s.reset_turn();
        }
        self.ask(text, Some(&format!("{}{text}", render::REVIEW_MODE_PROMPT)))?;
        self.st().answer();
        self.log_turn("review", "review", text);
        Ok(())
    }

    /// Plan: the planner writes tickets.
    pub fn plan(&self, text: &str) -> Result<()> {
        // Install outside the display stream; existing project instructions are preserved.
        let (project, session_name) = {
            let s = self.st();
            (s.project.clone(), s.session_name.clone())
        };
        let installer = Arc::clone(&self.hooks().installer);
        installer(&project, session_name.as_deref())?;
        let lanes = self.st().lanes();
        let before = crate::planner_output::ticket_snapshot(&project, Some(&lanes))?;
        let folder = workspace::relative(&project, &lanes);
        let (alias, profile, config) = {
            let s = self.st();
            (
                s.sel("plan").unwrap_or_default(),
                s.prof("plan"),
                s.config.clone(),
            )
        };
        let handle = self.session(&alias, &profile, None, SessionRequest::default())?;
        {
            let mut s = self.st();
            s.active = Some(handle);
            s.name_task(text);
            s.start_turn("plan", "", "thinking");
        }
        let prompt = format!(
            "Read skills/donut-planner/SKILL.md. First decide what the request needs. A question, lookup or status \
             request: answer it yourself, read-only, and write no ticket. Small housekeeping the user asks of you \
             (move or edit a ticket, record a consent, delete a stray generated file): do it yourself. Work that \
             changes the project: research it and write scoped tickets; do not implement or commit that work. \
             This session's tickets live in {folder}: write new tickets in {folder}to_do/ and leave every other \
             tickets folder alone. Respect on_hold and blocked lanes and reserve their identities. Record standing \
             permissions and decisions the user gives in {folder}consents.md. \
             Reply with the answer or result first, in a few lines unless asked for detail; give status as one table; \
             do not narrate your steps. Donut lists created tickets. {} User request:\n{text}",
            handholding::instruction(&config)
        );
        {
            let mut s = self.st();
            let shown = render::label_images(text, &s.images);
            s.say_line(styled(format!("> {shown}"), bold(ORANGE)));
            s.reset_turn();
        }
        self.ask(text, Some(&prompt))?;
        self.st().answer();
        self.log_turn("plan", "plan", text);
        let after = crate::planner_output::ticket_snapshot(&project, Some(&lanes))?;
        let mut created: Vec<std::path::PathBuf> = after
            .into_iter()
            .filter(|(key, _)| !before.iter().any(|(known, _)| known == key))
            .map(|(_, path)| path)
            .collect();
        if !created.is_empty() {
            // A question the planner answered itself made no tickets: nothing to announce.
            let mut s = self.st();
            s.say(&format!("Tickets created: {}", created.len()), Some(CORAL));
            created.sort();
            for path in created {
                let shown = path
                    .strip_prefix(&project)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                s.say(&format!("  {shown}"), Some(GRAY));
            }
            s.say_line(styled("PLANNING COMPLETE · /work next 1", bold(ORANGE)));
        }
        Ok(())
    }

    // ---- submitting ------------------------------------------------------------------------------------

    /// Run a message now, or, during a turn, queue it; commands that only show something run at once.
    pub fn submit(&self, text: &str) {
        let busy = {
            let mut s = self.st();
            s.transcript.set_follow(true); // A new message returns to the end, wherever the view was left.
            s.busy
        };
        if !busy {
            self.0.cancelled.store(false, Ordering::SeqCst); // Ctrl+C from here on cancels this message.
            self.execute(text);
        } else if render::is_instant(text) {
            let text = text.to_string();
            self.spawn(move |app| app.instant(&text));
        } else {
            // Like Claude Code: a message sent during a turn waits and runs as soon as the turn ends.
            let mut s = self.st();
            s.queued.push(Queued::Text(text.to_string()));
            let first = render::label_images(text, &s.images)
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect::<String>();
            s.say_line(styled(format!("  queued · {first}"), fg(DIM)));
            s.status(Some("Thinking…"));
        }
    }

    /// A display-only command during a turn: its result shows now and the turn carries on undisturbed.
    fn instant(&self, text: &str) {
        if let Err(error) = self.command(text) {
            self.say(&format!("Error: {error}"), Some(CORAL));
        }
    }

    /// Start a message on a worker thread (Python's `@work execute`).
    pub fn execute(&self, text: &str) {
        let executor = self.hooks().executor.clone();
        if let Some(executor) = executor {
            executor(text);
            return;
        }
        self.execute_item(Queued::Text(text.to_string()));
    }

    pub fn execute_item(&self, item: Queued) {
        self.spawn(move |app| app.run_item(item));
    }

    fn run_item(&self, item: Queued) {
        match item {
            Queued::Wake(wake) => {
                {
                    let mut s = self.st();
                    s.busy = true;
                    s.running = None;
                    s.started = Instant::now();
                }
                if let Err(error) = self.wake_turn(&wake) {
                    self.say(&format!("Error: {error}"), Some(CORAL));
                }
                self.turn_finished();
            }
            Queued::Text(text) => self.run_text(&text),
        }
    }

    fn run_text(&self, text: &str) {
        if let Some(command) = text.strip_prefix('!') {
            {
                let mut s = self.st();
                s.busy = true;
                s.running = None;
                s.started = Instant::now();
                s.status(None);
            }
            if let Err(error) = self.shell(command.trim()) {
                self.say(&format!("Error: {error}"), Some(CORAL));
            }
            self.st().child_pid = None;
            self.turn_finished();
            return;
        }
        let (mode, mut unset, has_aliases) = {
            let s = self.st();
            let role = s.role();
            (
                s.mode.clone(),
                s.sel(&role).is_none() && !(role == "solo" && s.auto),
                s.config.get("aliases").is_some_and(render::truthy),
            )
        };
        if unset && mode == "solo" && has_aliases && !text.starts_with('/') {
            // Donut starts in solo, so the first message asks for a soloist when none is set.
            match self.ensure_role("solo") {
                Ok(set) => unset = !set,
                Err(error) => {
                    self.say(&format!("Error: {error}"), Some(CORAL));
                    return;
                }
            }
            if unset {
                return;
            }
        }
        const ALLOWED: [&str; 7] = [
            "/help",
            "/exit",
            "/quit",
            "/permissions",
            "/auto",
            "/simple",
            "/complex",
        ];
        if unset && !ALLOWED.iter().any(|prefix| text.starts_with(prefix)) {
            self.say("Run donut setup to configure an account first.", None);
            return;
        }
        {
            let mut s = self.st();
            s.busy = true;
            s.running = None;
            s.activity = "working".into();
            s.started = Instant::now();
            s.status(None);
        }
        let result = if text.starts_with('/') {
            self.command(text)
        } else if mode == "solo" {
            self.solo(text)
        } else if mode == "review" {
            self.review_chat(text)
        } else {
            if mode == "work" {
                // Conversation goes to the planner (or the worker in /solo); /work only runs tickets.
                let mut s = self.st();
                s.set_mode("plan");
                s.say("Switched to plan mode for this message.", Some(DIM));
            }
            self.plan(text)
        };
        if let Err(error) = result {
            self.say(&format!("Error: {error}"), Some(CORAL));
        }
        self.turn_finished();
    }

    /// Back to Ready after a turn, then run the next queued message or wake-up.
    pub fn turn_finished(&self) {
        {
            let mut s = self.st();
            s.active = None;
            s.busy = false;
            s.running = None;
            if s.options.auto_refresh_quotas {
                s.deferred.push(Deferred::RefreshQuotas);
            }
            s.status(Some("Ready"));
            if !s.queued.is_empty() {
                s.deferred.push(Deferred::RunQueued);
            }
        }
        self.run_deferred();
    }

    fn run_next_queued(&self) {
        let next = {
            let mut s = self.st();
            if s.queued.is_empty() {
                None
            } else {
                Some(s.queued.remove(0))
            }
        };
        if let Some(item) = next {
            self.0.cancelled.store(false, Ordering::SeqCst);
            match item {
                Queued::Text(text) => self.execute(&text),
                wake => self.execute_item(wake),
            }
        }
    }

    pub fn finish_wake(&self, id: u64) {
        let role = {
            let mut s = self.st();
            let role = s.role_of(id);
            s.answer();
            role
        };
        let mode = if matches!(role.as_str(), "solo" | "review") {
            role.clone()
        } else {
            "plan".to_string()
        };
        self.log_turn(&mode, &role, "(woken by a background task)");
        self.turn_finished();
    }

    // ---- background waits --------------------------------------------------------------------------------

    /// A model asked Donut to wake it when `command` exits. Ask first unless permissions are on Full access.
    fn start_wait(&self, handle: &SessionHandle, command: &str) {
        if self.st().permission != "full" {
            let decision = self.approve(
                "Bash",
                &json!({"command": command, "description": "wait in the background"}),
            );
            if decision == Decision::No {
                self.say_dim(
                    "  Not waiting; the model was not told. Ask it to continue when you are ready.",
                );
                return;
            }
        }
        let role = self.st().role_of(handle.id);
        let wait_id = self.0.wait_ids.fetch_add(1, Ordering::SeqCst) + 1;
        let title = {
            let mut s = self.st();
            s.waiting.push((wait_id, command.to_string()));
            s.wake_title(handle.id)
        };
        self.say_line(styled(
            format!("  ⏳ Waiting on `{command}` · {title} continues when it exits"),
            fg(TEAL),
        ));
        self.status(None);
        let (id, command) = (handle.id, command.to_string());
        self.spawn(move |app| app.wait_then_wake(wait_id, id, &role, &command));
    }

    fn wait_then_wake(&self, wait_id: u64, session: u64, role: &str, command: &str) {
        let project = self.st().project.clone();
        let result = background::wait_for_cancellable(
            command,
            &project,
            Duration::from_secs(background::WAIT_LIMIT_SECONDS),
            &self.0.cancel_waits,
        );
        let (code, output) = match result {
            Ok(done) => done,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                let mut s = self.st();
                s.waiting.retain(|(id, _)| *id != wait_id);
                return;
            }
            Err(error) => (Some(-1), format!("Could not run it: {error}")),
        };
        let mut wake = background::Wake::new(session, role, command, code, &output);
        wake.created = conversations::now_seconds();
        let busy = {
            let mut s = self.st();
            s.waiting.retain(|(id, _)| *id != wait_id);
            if s.busy {
                s.queued.push(Queued::Wake(wake.clone()));
            }
            s.status(None);
            s.busy
        };
        if !busy {
            self.execute_item(Queued::Wake(wake));
        }
    }

    /// Give the model the result of the command it waited on, in its own conversation.
    fn wake_turn(&self, wake: &background::Wake<u64>) -> Result<()> {
        let state = match wake.code {
            None => "timed out".to_string(),
            Some(code) => format!("exit {code}"),
        };
        let alive = self.st().rec(wake.session).map(SessionRec::handle);
        let Some(handle) = alive else {
            let mut s = self.st();
            s.say_line(styled(
                format!(
                    "  ⏰ `{}` finished ({state}), but its conversation was closed.",
                    wake.command
                ),
                fg(TEAL),
            ));
            s.say_line(styled(background::tail(&wake.output, 1500), fg(DIM)));
            return Ok(());
        };
        {
            let mut s = self.st();
            s.active = Some(handle);
            let title = s.wake_title(wake.session);
            s.say_line(styled(
                format!(
                    "  ⏰ `{}` finished ({state}) · waking {title}",
                    wake.command
                ),
                bold(TEAL),
            ));
            s.start_turn(&wake.role, "", "continuing");
            s.reset_turn();
        }
        self.ask(
            &format!("(background) {}", wake.command),
            Some(&wake.prompt()),
        )?;
        self.st().answer();
        let mode = if matches!(wake.role.as_str(), "solo" | "review") {
            wake.role.clone()
        } else {
            "plan".to_string()
        };
        self.log_turn(&mode, &wake.role, &format!("(background) {}", wake.command));
        Ok(())
    }

    // ---- shell and runner ------------------------------------------------------------------------------

    /// !command: run it in the project and show its output; the model gets it with the next message.
    pub fn shell(&self, command: &str) -> Result<()> {
        if command.is_empty() {
            self.say_dim("  Type ! then a shell command, for example: !git status");
            return Ok(());
        }
        let project = {
            let mut s = self.st();
            s.say_line(styled(format!("! {command}"), bold(TEAL)));
            s.activity = "running".into();
            s.detail = command.to_string();
            s.project.clone()
        };
        let background::Started { mut child, output } = background::start(command, &project)?;
        self.st().child_pid = Some(child.id());
        let mut lines: Vec<String> = Vec::new();
        let mut reader = BufReader::new(output);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = clean_output_line(&raw);
            if lines.len() < 2000 {
                self.say_line(styled(format!("  {line}"), fg(GRAY)));
            }
            lines.push(line);
        }
        let status = child.wait()?;
        let code = exit_code(status);
        self.st().child_pid = None;
        if lines.len() > 2000 {
            self.say_dim(&format!(
                "  … {} more lines (the model gets the end of the output)",
                lines.len() - 2000
            ));
        }
        self.say_line(styled(
            format!("  exit {code}"),
            fg(if code == 0 { TEAL } else { CORAL }),
        ));
        self.st()
            .shell_runs
            .push((command.to_string(), code, lines.join("\n")));
        Ok(())
    }

    /// /work: run tickets in a child Donut with this window's models, showing its output.
    pub fn work_tickets(&self, choice: &str) -> Result<()> {
        let words = render::work_arguments(choice).map_err(wrong)?;
        let (alias, project, session_name, permission, verbosity, base, work_profile, review, plan) = {
            let s = self.st();
            (
                s.sel("work"),
                s.project.clone(),
                s.session_name.clone(),
                s.permission.clone(),
                s.verbosity.clone(),
                s.config.clone(),
                s.prof("work"),
                s.prof("review"),
                s.prof("plan"),
            )
        };
        let Some(alias) = alias else {
            bail!("Run donut setup to configure a worker.")
        };
        let installer = Arc::clone(&self.hooks().installer);
        installer(&project, session_name.as_deref())?;
        let temp = tempfile::Builder::new().prefix("donut-worker-").tempdir()?;
        let mut config = base;
        if alias == routing::AUTO {
            if let Some(aliases) = config.get_mut("aliases").and_then(Value::as_object_mut) {
                for profile in aliases.values_mut() {
                    // The worker builds its auto steps from these.
                    profile["permission_mode"] = json!(permission);
                }
            }
        } else {
            let mut profile = work_profile.clone();
            profile["permission_mode"] = json!(permission);
            config["aliases"][&alias] = profile;
        }
        if !is_empty_profile(&review) {
            // A dedicated alias, so a reviewer on the same account as the worker keeps its own model.
            config["aliases"]["donut-reviewer"] = review;
            config["reviewer"] = json!("donut-reviewer");
        } else {
            config["reviewer"] = json!("off");
        }
        if !is_empty_profile(&plan) {
            // Blocked or stuck tickets go back to the planner, with the same permission mode as the worker.
            let mut profile = plan;
            profile["permission_mode"] = json!(permission);
            config["aliases"]["donut-planner"] = profile;
            config["recovery"] = json!("donut-planner");
        }
        let path = config_path(Some(temp.path())); // Where the worker, started in this window's profile, reads its configuration.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string(&config)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let mut args = vec![
            "-p".to_string(),
            project.to_string_lossy().into_owned(),
            "-m".to_string(),
            alias.clone(),
        ];
        if let Some(name) = session_name.filter(|name| !name.is_empty()) {
            args.push("--session".into());
            args.push(name);
        }
        args.push("--work".into());
        args.extend(words);
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.retain(|(key, _)| {
            !["XDG_CONFIG_HOME", "NO_COLOR", "DONUT_VERBOSITY"].contains(&key.as_str())
        });
        env.push((
            "XDG_CONFIG_HOME".into(),
            temp.path().to_string_lossy().into_owned(),
        ));
        env.push(("NO_COLOR".into(), "1".into()));
        env.push(("DONUT_VERBOSITY".into(), verbosity));
        let spec = RunnerSpec {
            args,
            env,
            cwd: project,
        };
        let launcher = Arc::clone(&self.hooks().runner);
        let mut child = launcher(&spec)?;
        self.st().child_pid = Some(child.id());
        let mut highlighter = crate::highlight::Highlighter::new();
        let mut ticket = String::new();
        self.st().start_turn("work", "", "starting");
        let (sender, receiver) = mpsc::channel::<Vec<u8>>();
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(pipe_lines(out, sender.clone()));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(pipe_lines(err, sender.clone()));
        }
        drop(sender);
        for raw in receiver {
            // Keep only what a terminal would finally show on this line: no escapes, no \r redraws.
            let line = clean_output_line(&raw);
            let line = line.trim_end();
            if !line.is_empty()
                && line
                    .chars()
                    .all(|c| ('\u{2800}'..='\u{28ff}').contains(&c) || c.is_whitespace())
            {
                continue; // Stray spinner frames.
            }
            if render::RUNNER_PROGRESS.is_match(line) {
                continue; // The spinner line already shows the worker and its elapsed time.
            }
            if let Some(found) = render::RUNNER_SPEND.captures(line) {
                // What one worker, review or planner call of the run cost: it joins the figure on the input box.
                let dollars = found
                    .name("dollars")
                    .and_then(|d| d.as_str().parse::<f64>().ok());
                let mut s = self.st();
                let model = found.name("model").map_or("", |m| m.as_str()).to_string();
                let whence = found.name("where").map_or("", |m| m.as_str()).to_string();
                s.count_spend(Some(&model), &whence, dollars);
                s.refresh_spent_label();
                continue;
            }
            let mut s = self.st();
            if let Some(found) = render::RUNNER_TICKET.captures(line) {
                ticket = found["ticket"].to_string();
                s.start_turn("work", &ticket, "working");
            }
            if let Some(found) = render::RUNNER_REVIEW.captures(line) {
                s.start_turn("review", &found["ticket"], "reviewing");
            }
            if line.starts_with("Review: changes requested") {
                s.start_turn("work", &ticket, "addressing review feedback");
            }
            if let Some(found) = render::RUNNER_RECOVER.captures(line) {
                s.start_turn("plan", &found["ticket"], "recovering a blocked ticket");
            }
            if line.starts_with("[repair queue]") {
                s.start_turn("plan", "", "repairing stuck tickets");
            }
            let styled_line = highlighter.line(line);
            s.say_line(styled_line);
        }
        for reader in readers {
            let _ = reader.join();
        }
        let code = exit_code(child.wait()?);
        self.st().child_pid = None;
        if code == 0 {
            self.say_line(styled("WORK COMPLETE", bold(ORANGE)));
        } else {
            self.say(
                &format!("Worker stopped with exit code {code}; inspect results above."),
                Some(CORAL),
            );
        }
        Ok(())
    }

    // ---- cancel, quit, copy ---------------------------------------------------------------------------

    /// Copy through the terminal (OSC 52) and this computer's own clipboard tool.
    pub fn copy_text(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        self.st().osc52(text);
        let copy = Arc::clone(&self.hooks().copy_native);
        copy(text);
        true
    }

    pub fn action_cancel(&self) {
        let selected = self.st().selected_text();
        if let Some(selected) = selected {
            // Ctrl+C with something selected copies it, as in any editor; with nothing selected it cancels.
            self.copy_text(&selected);
            let mut s = self.st();
            s.notify(format!("Copied {} characters", selected.chars().count()));
            s.transcript.clear_selection();
            s.draft.clear_selection();
            return;
        }
        self.0.cancelled.store(true, Ordering::SeqCst); // Seen by a step that runs before the turn itself.
        let (active, child) = {
            let mut s = self.st();
            if let Some(sender) = s.typing.take() {
                let _ = sender.send(None);
            }
            if let Some(sender) = s.picking.clone() {
                let _ = sender.send(None);
            } else if s.menu.is_open() {
                s.menu.close();
            }
            let typed: Vec<String> = s
                .queued
                .iter()
                .filter_map(|item| match item {
                    Queued::Text(text) => Some(text.clone()),
                    Queued::Wake(_) => None,
                })
                .collect();
            if !typed.is_empty() {
                // Put queued messages back in the prompt so nothing typed is lost; background wake-ups stay queued.
                let mut parts = typed;
                let current = s.draft.text();
                if !current.trim().is_empty() {
                    parts.push(current);
                }
                s.draft.load_text(&parts.join("\n"));
                s.draft.move_to_end();
                s.queued.retain(|item| !matches!(item, Queued::Text(_)));
            }
            (s.active.clone(), s.child_pid)
        };
        if let Some(active) = active {
            // A provider may take a moment to answer an interrupt; the keyboard must not wait for it.
            std::thread::spawn(move || {
                let _ = active.session.interrupt();
            });
        }
        if let Some(pid) = child {
            terminate_group(pid);
        }
    }

    pub fn action_quit(&self) {
        self.st().terminal_out.push("\x1b[23;0t".to_string()); // Put back the title the terminal had before Donut.
        self.0.cancel_waits.store(true, Ordering::SeqCst);
        self.action_cancel();
        self.close_all_sessions();
        self.st().quit = true;
    }

    /// /clear and /new: close every provider conversation and wipe the screen, as if Donut had just opened.
    pub fn start_fresh(&self) {
        let sessions: Vec<Arc<dyn Session>> = {
            let mut s = self.st();
            let old = std::mem::take(&mut s.sessions);
            s.threads.clear();
            s.exchanges.clear();
            s.waiting_resume.clear();
            s.forget_route();
            s.background_tasks.clear();
            s.ponytail_installed.clear();
            old.into_iter().map(|rec| rec.session).collect()
        };
        for session in sessions {
            let _ = session.close();
        }
        let mut s = self.st();
        s.last_answer.clear();
        s.transcript.clear();
        s.welcome();
        s.say_line(styled(
            "  New conversation. Existing tickets are kept.",
            fg(GRAY),
        ));
    }

    /// Run a child that owns the terminal (/login, /native), then come back.
    pub fn suspend<R>(&self, work: impl FnOnce() -> R) -> R {
        let hook = self.hooks().terminal.clone();
        self.0.suspended.store(true, Ordering::SeqCst);
        if let Some(hook) = &hook {
            hook(false);
        }
        let result = work();
        if let Some(hook) = &hook {
            hook(true);
        }
        self.0.suspended.store(false, Ordering::SeqCst);
        result
    }

    // ---- models, roles ---------------------------------------------------------------------------------

    /// Entering a mode asks for its model only when none is set. Returns whether one is set now.
    pub fn ensure_role(&self, role: &str) -> Result<bool> {
        let set = {
            let s = self.st();
            !is_empty_profile(&s.prof(role)) || (role == "solo" && s.auto)
        };
        if set {
            return Ok(true);
        }
        if role == "review" {
            self.choose_reviewer(None, false)?;
        } else {
            self.choose_model(role, None)?;
        }
        if !is_empty_profile(&self.st().prof(role)) {
            return Ok(true);
        }
        self.say_dim(&format!(
            "No {} chosen; pick one with {}.",
            role_title(role),
            role_command(role)
        ));
        Ok(false)
    }

    /// `(label, alias, profile)` for every model each configured account offers (of these engines only).
    pub fn model_rows(&self, engines: Option<&[&str]>) -> Vec<(String, String, Value)> {
        let aliases = self
            .st()
            .config
            .get("aliases")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut rows = Vec::new();
        for (alias, profile) in aliases {
            let engine = engine_of(&profile).unwrap_or("").to_string();
            if let Some(engines) = engines
                && !engines.contains(&engine.as_str())
            {
                continue;
            }
            self.status(Some(&format!("discovering models for {alias}")));
            let found: Result<Vec<(String, String)>> = (|| match engine.as_str() {
                "claude" | "codex" => {
                    let handle =
                        self.session(&alias, &profile, None, SessionRequest::untracked())?;
                    Ok(handle
                        .session
                        .models()?
                        .into_iter()
                        .map(|m| (m.id, m.name))
                        .collect())
                }
                "ollama" => {
                    let output = Command::new("ollama")
                        .arg("list")
                        .stderr(Stdio::null())
                        .output()?;
                    if !output.status.success() {
                        bail!("Ollama model discovery failed");
                    }
                    let text = String::from_utf8_lossy(&output.stdout).into_owned();
                    Ok(text
                        .lines()
                        .skip(1)
                        .filter_map(|line| line.split_whitespace().next())
                        .map(|name| (name.to_string(), name.to_string()))
                        .collect())
                }
                _ => {
                    let models = crate::catalog::endpoint_models(&profile)?;
                    Ok(models
                        .iter()
                        .map(|m| {
                            let id = m
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = m
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or(&id)
                                .to_string();
                            (id, name)
                        })
                        .collect())
                }
            })();
            match found {
                Ok(models) => {
                    for (id, name) in models {
                        let mut with_model = profile.clone();
                        with_model["model"] = json!(id);
                        rows.push((format!("{alias}  {name}"), alias.clone(), with_model));
                    }
                }
                Err(error) => self.say(&format!("{alias}: model discovery failed: {error}"), None),
            }
        }
        rows
    }

    fn model_menu_rows(rows: &[(String, String, Value)]) -> Vec<MenuRow> {
        rows.iter()
            .map(|(label, alias, profile)| MenuRow::new(label.clone(), json!([alias, profile])))
            .collect()
    }

    /// Pick the account and model for one role (planner, worker or soloist). Only that role changes.
    pub fn choose_model(&self, role: &str, alias: Option<&str>) -> Result<()> {
        let aliases = self
            .st()
            .config
            .get("aliases")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut automatic = json!([routing::AUTO, Value::Null]);
        let ladder = if role == "work" {
            routing::worker_ladder(&aliases)
        } else {
            Vec::new()
        };
        if let Some(first) = ladder.first() {
            automatic = json!([routing::AUTO, first]);
        }
        let selected: Option<Value> = if let Some(alias) = alias {
            if matches!(role, "solo" | "work")
                && alias == routing::AUTO
                && aliases.get(alias).is_none()
            {
                if role == "work" && ladder.is_empty() {
                    bail!("Worker auto needs a Claude or Codex alias. Run donut setup.");
                }
                Some(automatic.clone())
            } else if aliases.get(alias).is_none() {
                let command = role_command(role);
                bail!("Use {command} ALIAS with a configured alias, or {command} alone to pick.");
            } else {
                Some(json!([alias, aliases[alias]]))
            }
        } else {
            let found = self.model_rows(None);
            let mut rows = Self::model_menu_rows(&found);
            if role == "solo" && !rows.is_empty() {
                rows.push(MenuRow::new(
                    "auto  picks a model for each conversation from how hard its request is",
                    automatic.clone(),
                ));
            }
            if !ladder.is_empty() {
                let names: Vec<String> = ladder
                    .iter()
                    .map(|step| text_of(step, "model").unwrap_or("").to_string())
                    .collect();
                rows.push(MenuRow::new(
                    format!(
                        "auto  each ticket starts on {}; review feedback moves it up: {}",
                        names[0],
                        names.join(" → ")
                    ),
                    automatic.clone(),
                ));
            }
            let title = if self.st().simple() {
                "Model".to_string()
            } else {
                format!("Account and model for the {}", role_title(role))
            };
            if rows.is_empty() {
                None
            } else {
                self.pick(&title, rows)
            }
        };
        let Some(selected) = selected else {
            return Ok(());
        };
        if selected == automatic && role == "solo" {
            self.set_auto();
            return Ok(());
        }
        let name = selected[0].as_str().unwrap_or("").to_string();
        let profile = if selected[1].is_null() {
            json!({})
        } else {
            selected[1].clone()
        };
        {
            let mut s = self.st();
            if role == "solo" {
                s.auto = false;
                s.route = None;
            }
            s.selected.insert(role.to_string(), Some(name.clone()));
            s.profiles.insert(role.to_string(), profile.clone());
        }
        if let Err(error) = remember_role(role, &name, text_of(&profile, "model"), None) {
            self.say_dim(&format!(
                "Model choice applies to this session only: {error}"
            ));
        }
        let mut s = self.st();
        let title = role_title(role);
        let capital: String = title
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_default()
            + &title[1..];
        s.say_line(Line::from(vec![
            span(format!("  {capital}: "), fg(GRAY)),
            span(model_name(&profile, Some(&name)), bold(ORANGE)),
        ]));
        if role == "plan" && s.reviewer == "planner" {
            s.say(
                "  The reviewer follows the planner; choose another with /reviewer.",
                Some(DIM),
            );
        }
        s.sync_reviewer();
        s.status(None);
        Ok(())
    }

    /// Pick who reviews each finished ticket: the planner (default), any account and model, or off.
    pub fn choose_reviewer(&self, choice: Option<&str>, allow_off: bool) -> Result<()> {
        let aliases = self
            .st()
            .config
            .get("aliases")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let chosen: Value = match choice {
            None => {
                let planner = {
                    let s = self.st();
                    model_name(&s.prof("plan"), s.sel("plan").as_deref())
                };
                let mut rows = vec![
                    MenuRow::new("Same as planner (default)", json!("planner"))
                        .described(&format!("follows the planner · now {planner}")),
                ];
                rows.extend(Self::model_menu_rows(&self.model_rows(None)));
                if allow_off {
                    rows.push(
                        MenuRow::new("Off", json!("off"))
                            .described("no review; the worker's result stands"),
                    );
                }
                match self.pick(
                    "Reviewer · checks each ticket and answers in review mode",
                    rows,
                ) {
                    Some(value) => value,
                    None => return Ok(()),
                }
            }
            Some(choice @ ("planner" | "off")) => json!(choice),
            Some(choice) => {
                if aliases.get(choice).is_none() {
                    bail!(
                        "Use /reviewer planner, /reviewer off, or /reviewer ALIAS with a configured alias."
                    );
                }
                json!([choice, aliases[choice]])
            }
        };
        {
            let mut s = self.st();
            match chosen.as_str() {
                Some(word) => {
                    s.reviewer = word.to_string();
                    drop(s);
                    if let Err(error) = forget_role("review", Some(word)) {
                        self.say_dim(&format!("Reviewer applies to this session only: {error}"));
                    }
                }
                None => {
                    s.reviewer = "chosen".into();
                    let name = chosen[0].as_str().unwrap_or("").to_string();
                    s.selected.insert("review".into(), Some(name.clone()));
                    s.profiles.insert("review".into(), chosen[1].clone());
                    drop(s);
                    if let Err(error) =
                        remember_role("review", &name, text_of(&chosen[1], "model"), None)
                    {
                        self.say_dim(&format!("Reviewer applies to this session only: {error}"));
                    }
                }
            }
        }
        let mut s = self.st();
        s.sync_reviewer();
        if s.reviewer == "off" {
            s.say(
                "Review off. Finished tickets are not reviewed.",
                Some(ORANGE),
            );
        } else {
            let who = model_name(&s.prof("review"), s.sel("review").as_deref());
            s.say_line(styled(format!("  Reviewer: {who}"), bold(ORANGE)));
            s.say("  It checks each finished ticket in a fresh context and sends issues back to the worker.", Some(DIM));
        }
        s.status(None);
        Ok(())
    }

    /// Make the soloist auto: each conversation's model is picked from its first request.
    pub fn set_auto(&self) {
        {
            let mut s = self.st();
            s.auto = true;
            s.forget_route();
        }
        if let Err(error) = remember_role("solo", routing::AUTO, Some(""), None) {
            self.say_dim(&format!(
                "Model choice applies to this session only: {error}"
            ));
        }
        let mut s = self.st();
        let label = if s.simple() {
            "  Model: "
        } else {
            "  Soloist: "
        };
        s.say_line(Line::from(vec![
            span(label, fg(GRAY)),
            span("auto", bold(ORANGE)),
        ]));
        s.say(
            "  Donut rates the first message of each conversation from 1 to 10 and picks the model for it. \
             The model stays for the conversation; /harder moves one model up.",
            Some(DIM),
        );
        s.status(None);
    }

    /// The models auto chooses from, cheapest first: from the configuration, or from the accounts' models.
    pub fn routing_steps(&self) -> Vec<routing::Step> {
        if let Some(steps) = self.st().steps.clone() {
            return steps;
        }
        let config = self.st().config.clone();
        let mut steps = routing::configured(&config);
        if steps.is_empty() {
            let rows = self.model_rows(Some(&["claude", "codex"]));
            let models: Vec<(String, Value, String)> = rows
                .into_iter()
                .map(|(label, alias, profile)| {
                    let name = label
                        .split_once("  ")
                        .map_or(label.clone(), |(_, name)| name.to_string());
                    (alias, profile, name)
                })
                .collect();
            steps = routing::build(&models);
        }
        self.st().steps = Some(steps.clone());
        steps
    }

    /// Ask a model one question in a throwaway read-only session and return its answer.
    ///
    /// Nothing is shown in the transcript and the session takes no part in any conversation.
    pub fn one_shot(&self, profile: &Value, prompt: &str, limit: Duration) -> Result<String> {
        let engine = engine_of(profile).unwrap_or("").to_string();
        let parts: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let emit: Emit = {
            let parts = Arc::clone(&parts);
            Arc::new(move |kind: &str, data: Value| {
                if kind == "text" {
                    let text = data
                        .as_str()
                        .map_or_else(|| data.to_string(), str::to_string);
                    parts
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push_str(&text);
                }
            })
        };
        let approve: Approve = Arc::new(|_: &str, _: &Value| Decision::No);
        let project = self.st().project.clone();
        let spec = SessionSpec {
            alias: String::new(),
            purpose: Some("one-shot".into()),
            engine,
            project,
            profile: profile.clone(),
            options: SessionOptions {
                permission: "ask".into(),
                read_only: true,
                // One question, no tools: keep settings, plugins and connectors out of its prompt.
                bare: true,
                chrome: false,
                resume_id: None,
                ask: None,
            },
            emit,
            approve,
        };
        let factory = Arc::clone(&self.hooks().factory);
        let Some(session) = factory(&spec) else {
            bail!("No session for this engine.")
        };
        session.connect()?;
        let id = {
            let mut s = self.st();
            s.next_session_id += 1;
            let id = s.next_session_id;
            s.active = Some(SessionHandle {
                id,
                session: Arc::clone(&session),
            }); // So Ctrl+C can interrupt it.
            id
        };
        let _ = id;
        let (sender, receiver) = mpsc::channel();
        let runner = Arc::clone(&session);
        let prompt = prompt.to_string();
        std::thread::spawn(move || {
            let _ = sender.send(runner.query(&prompt));
        });
        let outcome = match receiver.recv_timeout(limit) {
            Ok(result) => result,
            Err(_) => {
                let _ = session.interrupt();
                Err(anyhow::Error::new(TimedOut))
            }
        };
        if outcome.is_ok() {
            let usage = session.usage();
            let profile = session.profile();
            let account = session.account();
            let mut s = self.st();
            let mut tracker = SpendTracker::default();
            s.add_spent(Some(&usage), &profile, account.as_deref(), &mut tracker);
        }
        self.st().active = None;
        let _ = session.close();
        outcome?;
        let text = parts.lock().unwrap_or_else(|p| p.into_inner()).clone();
        Ok(text)
    }

    /// Rate the conversation's first request and fix its model: the first one whose limit covers the score.
    ///
    /// Returns false when the user cancelled while the request was being rated; nothing is sent then.
    pub fn choose_route(&self, text: &str) -> Result<bool> {
        {
            let mut s = self.st();
            s.activity = "choosing a model".into();
            s.detail.clear();
            s.status(None);
        }
        let steps = self.routing_steps();
        if steps.is_empty() {
            bail!(
                "Auto needs a Claude or Codex account to choose from, or \"routing\": {{\"models\": [...]}} in the \
                 configuration. /model picks one model instead."
            );
        }
        {
            let mut s = self.st();
            s.activity = "choosing a model".into();
            s.status(None);
        }
        let config = self.st().config.clone();
        let mut score: Option<i64> = None;
        let mut problem = String::new();
        let mut last = (String::new(), json!({}));
        // A classifier that fails (a wrong model ID, an expired login) hands over to the next strongest model.
        for (alias, profile) in routing::classifiers(&config, &steps) {
            last = (alias.clone(), profile.clone());
            match self.one_shot(
                &profile,
                &routing::classifier_prompt(text),
                Duration::from_secs(90),
            ) {
                Ok(answer) => {
                    score = routing::parse_score(&answer);
                    problem = if score.is_some() {
                        String::new()
                    } else {
                        "the request could not be rated".to_string()
                    };
                    break;
                }
                Err(error) => {
                    let message: String = error.to_string().chars().take(80).collect();
                    problem = format!("the request could not be rated ({message})");
                    if self.is_cancelled() || error.downcast_ref::<TimedOut>().is_some() {
                        break; // A slow network would be slow for every model.
                    }
                }
            }
        }
        if self.is_cancelled() {
            self.say_dim("  Cancelled before a model was chosen; nothing was sent.");
            return Ok(false);
        }
        let index = match score {
            Some(score) => routing::pick(&steps, score),
            None => routing::fallback(&steps),
        };
        let step = self.st().use_step(index, score, 0);
        routing::log(
            "route",
            json!({
                "score": score,
                "model": step.profile.get("model"),
                "alias": step.alias,
                "position": index + 1,
                "of": steps.len(),
                "classifier": text_of(&last.1, "model").map(str::to_string).unwrap_or_else(|| last.0.clone()),
                "fallback": score.is_none(),
                "length": text.chars().count(),
            }),
        );
        let mut line = vec![span("  auto", bold(ORANGE))];
        match score {
            Some(score) => line.push(span(format!(" · difficulty {score}/10"), fg(GRAY))),
            None => line.push(span(
                format!(" · {problem}; using the middle model"),
                fg(GRAY),
            )),
        }
        line.push(span(" → ", fg(DIM)));
        line.push(span(step.name.clone(), bold(TEAL)));
        if index + 1 < steps.len() {
            line.push(span("   /harder for a stronger model", fg(DIM)));
        }
        self.say_line(Line::from(line));
        Ok(true)
    }

    /// /harder: move this conversation one model up the list. The conversation carries over.
    pub fn harder(&self) {
        let (auto, route) = {
            let s = self.st();
            (s.auto, s.route.clone())
        };
        if !auto {
            self.say_dim("  /harder works when the model is auto. /model lets you choose auto.");
            return;
        }
        let Some(route) = route else {
            self.say_dim("  Send a message first: auto picks this conversation's model then.");
            return;
        };
        let steps = self.routing_steps();
        let Some(index) = route.index else {
            self.say_dim(
                "  This conversation's model is not one auto chooses from. /model picks another.",
            );
            return;
        };
        if index + 1 >= steps.len() {
            self.say_dim(&format!(
                "  {} is already the strongest model auto chooses from.",
                steps[index].name
            ));
            return;
        }
        let before = steps[index].clone();
        let step = self.st().use_step(index + 1, route.score, route.harder + 1);
        routing::log(
            "harder",
            json!({
                "score": route.score,
                "model": step.profile.get("model"),
                "alias": step.alias,
                "position": index + 2,
                "of": steps.len(),
                "was": before.profile.get("model"),
            }),
        );
        self.say_line(Line::from(vec![
            span("  harder", bold(ORANGE)),
            span(" → ", fg(DIM)),
            span(step.name.clone(), bold(TEAL)),
            span(
                format!(
                    "  (was {}). It continues this conversation from your next message.",
                    before.name
                ),
                fg(DIM),
            ),
        ]));
    }

    /// /routing: the models auto chooses from, the scores each takes, and what this conversation got.
    pub fn show_routing(&self) {
        let waiting = {
            let s = self.st();
            s.active.is_some() && s.steps.is_none()
        };
        if waiting {
            self.say_dim("  The model list is still being found; try again in a moment.");
            return;
        }
        let steps = self.routing_steps();
        if steps.is_empty() {
            self.say_dim("  No models to choose from: add a Claude or Codex account, or \"routing\" in the configuration.");
            return;
        }
        let (config, auto, route) = {
            let s = self.st();
            (s.config.clone(), s.auto, s.route.clone())
        };
        let mut s = self.st();
        s.say_line(styled("  Auto chooses from (cheapest first)", bold(ORANGE)));
        let current = if auto {
            route.as_ref().and_then(|r| r.index)
        } else {
            None
        };
        let mut low = 1;
        for (index, step) in steps.iter().enumerate() {
            let here = Some(index) == current;
            let range = if low < step.up_to {
                format!("{low}–{}", step.up_to)
            } else {
                format!("{}", step.up_to)
            };
            s.say_line(Line::from(vec![
                span(
                    if here { "  ● " } else { "  ○ " },
                    if here { bold(ORANGE) } else { fg(DIM) },
                ),
                span(format!("{range:<6}"), fg(GRAY)),
                span(step.name.clone(), if here { bold(TEAL) } else { fg(GRAY) }),
                span(format!("  {}", step.alias), fg(DIM)),
            ]));
            low = step.up_to + 1;
        }
        if let Some((alias, profile)) = routing::classifier(&config, &steps) {
            let mut rated = vec![
                span("  Requests are rated by ", fg(DIM)),
                span(model_name(&profile, Some(&alias)), fg(GRAY)),
            ];
            if let Some(level) = text_of(&profile, "effort") {
                rated.push(span(format!(" at {level} effort"), fg(DIM)));
            }
            s.say_line(Line::from(rated));
        }
        if !auto {
            s.say(
                "  Auto is off: the model is the one you chose. /model lets you choose auto.",
                Some(DIM),
            );
        } else if let Some(score) = route.and_then(|r| r.score).filter(|score| *score != 0) {
            s.say(
                &format!("  This conversation was rated {score}/10."),
                Some(DIM),
            );
        }
    }

    /// The first time Donut opens, ask which version of the interface to show. Asked once and saved.
    pub fn first_open(&self) {
        let rows: Vec<MenuRow> = render::INTERFACES
            .iter()
            .map(|(key, (label, description))| {
                MenuRow::new(*label, json!(key)).described(description)
            })
            .collect();
        match self.pick("How do you want to use Donut?", rows) {
            Some(chosen) => {
                let name = chosen.as_str().unwrap_or("complex").to_string();
                {
                    let mut s = self.st();
                    s.interface = name.clone();
                    s.transcript.clear(); // Nothing has been said yet: greet again in the chosen version.
                    s.welcome();
                }
                self.set_interface(&name);
            }
            None => self.say_dim("  Showing everything for now; Donut asks again next time. /simple and /complex switch."),
        }
    }

    /// /simple and /complex: which version of the interface is shown. Models are told nothing of it.
    pub fn set_interface(&self, name: &str) {
        self.st().interface = name.to_string();
        if let Err(error) = self.save_config_key("interface", json!(name)) {
            self.say_dim(&format!("This applies to this session only: {error}"));
        }
        self.st().config["interface"] = json!(name);
        let (label, _) = render::INTERFACES
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, v)| *v)
            .unwrap_or(("", ""));
        let simple = self.st().simple();
        if simple {
            self.say_line(Line::from(vec![
                span(format!("  {label}"), bold(ORANGE)),
                span("  One conversation; Donut picks the model for each request. /complex shows everything.", fg(DIM)),
            ]));
            self.st().set_mode("solo");
            if !self.st().auto {
                self.set_auto();
            }
        } else {
            self.say_line(Line::from(vec![
                span(format!("  {label}"), bold(ORANGE)),
                span(
                    "  Planner, worker, reviewer and tickets are shown. /simple hides them again.",
                    fg(DIM),
                ),
            ]));
            let mode = self.st().mode.clone();
            self.st().set_mode(&mode);
        }
        self.status(None);
    }

    /// /effort [LEVEL]: how hard the current mode's model thinks, in its provider's own terms.
    pub fn choose_effort(&self, level: Option<&str>) -> Result<()> {
        let (role, profile, alias) = {
            let s = self.st();
            let role = s.role();
            (role.clone(), s.prof(&role), s.sel(&role))
        };
        if is_empty_profile(&profile) {
            self.say_dim(&format!(
                "No {} chosen; pick one with {}.",
                role_title(&role),
                role_command(&role)
            ));
            return Ok(());
        }
        if alias.as_deref() == Some(routing::AUTO) {
            self.say_dim(
                "  Auto sets each step's effort. Pick a model with /worker to set effort yourself.",
            );
            return Ok(());
        }
        let name = model_name(&profile, alias.as_deref());
        self.status(Some("checking effort levels"));
        let (term, levels, default) = self.effort_options(&role)?;
        if levels.is_empty() {
            self.say_dim(&format!("  {name} has no effort setting Donut can change."));
            return Ok(());
        }
        let current = text_of(&profile, "effort").map(str::to_string).or(default);
        let level = match level {
            Some(level) => level.to_string(),
            None => {
                let rows: Vec<MenuRow> = levels
                    .iter()
                    .map(|(value, description)| {
                        let text = format!(
                            "{description}{}",
                            if Some(value) == current.as_ref() {
                                " · now"
                            } else {
                                ""
                            }
                        );
                        MenuRow::new(value.clone(), json!(value))
                            .described(text.trim_matches([' ', '·']))
                    })
                    .collect();
                let title = format!(
                    "{term} for {name} · now {}",
                    current.clone().unwrap_or_else(|| "default".into())
                );
                match self.pick(&title, rows) {
                    Some(value) => value.as_str().unwrap_or("").to_string(),
                    None => return Ok(()),
                }
            }
        };
        if !levels.iter().any(|(value, _)| *value == level) {
            let all: Vec<&str> = levels.iter().map(|(value, _)| value.as_str()).collect();
            bail!("{name} takes {} {}", term.to_lowercase(), all.join(", "));
        }
        let mut updated = profile.clone();
        updated["effort"] = json!(level);
        let purpose = {
            let s = self.st();
            s.purpose()
        };
        let key = State::session_key(alias.as_deref().unwrap_or(""), &profile, purpose.as_deref());
        let session = {
            let mut s = self.st();
            s.profiles.insert(role.clone(), updated.clone());
            s.rec_by_key(&key).map(|rec| Arc::clone(&rec.session))
        };
        if let Some(session) = session {
            if session.set_effort(&level).is_ok() {
                if let Some(rec) = self.st().sessions.iter_mut().find(|rec| rec.key == key) {
                    rec.profile = updated.clone();
                }
            }
        }
        if let Err(error) = remember_role(
            &role,
            alias.as_deref().unwrap_or(""),
            text_of(&profile, "model"),
            Some(&level),
        ) {
            self.say_dim(&format!("Effort applies to this session only: {error}"));
        }
        let mut s = self.st();
        if role == "plan" {
            s.sync_reviewer();
        } else if role == "review" {
            s.reviewer = "chosen".into(); // Saved with its own effort, the reviewer no longer follows the planner.
        }
        s.say_line(Line::from(vec![
            span(format!("  {term} · {level}"), bold(ORANGE)),
            span(format!("  {name}, {}", role_title(&role)), fg(DIM)),
        ]));
        s.status(None);
        Ok(())
    }

    /// (what the provider calls it, [(level, description)], default level) for the role's model.
    fn effort_options(&self, role: &str) -> Result<effort::Options> {
        let (profile, alias) = {
            let s = self.st();
            (s.prof(role), s.sel(role))
        };
        let model = text_of(&profile, "model").unwrap_or("").to_string();
        match engine_of(&profile) {
            Some("claude") => {
                let purpose = if role == "review" {
                    Some("review")
                } else {
                    None
                };
                let handle = self.session(
                    alias.as_deref().unwrap_or(""),
                    &profile,
                    purpose,
                    SessionRequest::untracked(),
                )?;
                Ok(effort::claude_options(&handle.session.info(), &model))
            }
            Some("codex") => {
                let handle = self.session(
                    alias.as_deref().unwrap_or(""),
                    &profile,
                    None,
                    SessionRequest::untracked(),
                )?;
                let details = Value::Array(handle.session.model_details()?);
                Ok(effort::codex_options(&details, &model))
            }
            _ => Ok(("Effort".into(), Vec::new(), None)),
        }
    }

    // ---- verbosity, chrome, handholding ----------------------------------------------------------------

    pub fn set_chrome(&self, on: bool) {
        self.st().config["chrome"] = json!(on);
        if let Err(error) = self.save_config_key("chrome", json!(on)) {
            self.say_dim(&format!("Chrome applies to this session only: {error}"));
        }
        self.say_line(styled(
            format!("  Chrome · {}", if on { "on" } else { "off" }),
            bold(ORANGE),
        ));
        if on {
            self.say_dim(
                "  Claude models get the browser tools of the Claude in Chrome extension, in Chrome signed in to the \
                 same Claude account. Codex and local models have no such tools.",
            );
        }
        self.say_dim("  It applies to new conversations: /clear starts one. /work uses it from its next run.");
        self.status(None);
    }

    pub fn set_handholding(&self, level: &str) {
        self.st().config["handholding"] = json!(level);
        if let Err(error) = self.save_config_key("handholding", json!(level)) {
            self.say_dim(&format!(
                "Handholding applies to this session only: {error}"
            ));
        }
        if let Some((_, (label, description, _))) =
            handholding::LEVELS.iter().find(|(key, _)| *key == level)
        {
            self.say_line(Line::from(vec![
                span(format!("  Handholding · {label}"), bold(ORANGE)),
                span(format!("  {description}"), fg(DIM)),
            ]));
        }
        self.say_dim("  It applies to tickets planned from now on (/plan and donut --plan); existing tickets are kept.");
        self.status(None);
    }

    pub fn set_verbosity(&self, level: &str) {
        self.st().verbosity = level.to_string();
        match self.save_config_key("verbosity", json!(level)) {
            Ok(()) => {
                self.st().config["verbosity"] = json!(level);
            }
            Err(error) => self.say_dim(&format!("Verbosity applies to this session only: {error}")),
        }
        if let Some((label, description)) = render::verbosity_entry(level) {
            self.say_line(Line::from(vec![
                span(format!("  Verbosity · {label}"), bold(ORANGE)),
                span(format!("  {description}"), fg(DIM)),
            ]));
        }
        self.say_dim(
            "  This changes only what Donut shows; the models still think and answer in full.",
        );
        self.status(None);
    }

    // ---- sessions folders, projects, resume ------------------------------------------------------------

    /// Point this window at another tickets folder: an existing session, a new one, or older swim_lanes/.
    pub fn choose_session(&self, name: Option<&str>) -> Result<()> {
        let (project, current) = {
            let s = self.st();
            (s.project.clone(), s.session_name.clone())
        };
        let name: String = match name {
            Some(name) => name.to_string(),
            None => {
                let mut rows = vec![
                    MenuRow::new("New session", json!(""))
                        .described("start an empty tickets folder"),
                ];
                for session in workspace::sessions(&project) {
                    let counts = workspace::count(&project, Some(&session));
                    let summary = ["to_do", "in_progress", "done", "blocked"]
                        .iter()
                        .filter_map(|lane| {
                            let n = counts
                                .iter()
                                .find(|(name, _)| name == lane)
                                .map_or(0, |(_, n)| *n);
                            (n > 0).then(|| format!("{n} {}", lane.replace('_', " ")))
                        })
                        .collect::<Vec<_>>()
                        .join(" · ");
                    let here = if Some(&session) == current.as_ref() {
                        "current · "
                    } else {
                        ""
                    };
                    let shown = if summary.is_empty() {
                        "no tickets yet".to_string()
                    } else {
                        summary
                    };
                    rows.push(
                        MenuRow::new(session.clone(), json!(session))
                            .described(&format!("{here}{shown}")),
                    );
                }
                if workspace::lanes_dir(&project, None).join("to_do").is_dir() {
                    rows.push(
                        MenuRow::new("swim_lanes/", json!("-"))
                            .described("this project's tickets from before sessions"),
                    );
                }
                let title = {
                    let s = self.st();
                    format!(
                        "Tickets session · now {}",
                        workspace::relative(&project, &s.lanes())
                    )
                };
                match self.pick(&title, rows) {
                    Some(value) => value.as_str().unwrap_or("").to_string(),
                    None => return Ok(()),
                }
            }
        };
        let chosen: Option<String> = if name.is_empty() || name == "new" {
            let taken: Vec<String> = current.into_iter().collect();
            Some(workspace::new_name(&project, None, &taken))
        } else if matches!(name.as_str(), "-" | "swim_lanes" | "swim_lanes/") {
            None
        } else if !workspace::valid(&name) {
            bail!("Invalid session name {name:?}: use letters, numbers, '.', '_' or '-'");
        } else {
            Some(name)
        };
        self.st().session_name = chosen.clone();
        workspace::create(&project, chosen.as_deref())?;
        let folder = workspace::relative(&project, &self.st().lanes());
        self.say_line(styled(
            format!("  Tickets session · {folder}"),
            bold(ORANGE),
        ));
        self.say_dim("  /plan writes tickets here and /work runs them from here.");
        self.status(None);
        Ok(())
    }

    /// Open another project in a new terminal window, picked from the projects Donut has opened.
    pub fn open_project(&self, path: Option<&str>) -> Result<()> {
        let project = self.st().project.clone();
        let path: String = match path {
            Some(path) => path.to_string(),
            None => {
                let current = conversations::resolve(&project)
                    .to_string_lossy()
                    .into_owned();
                let mut rows = Vec::new();
                for other in conversations::recent_projects() {
                    if other == current {
                        continue;
                    }
                    let sessions = workspace::sessions(Path::new(&other));
                    let detail = if sessions.is_empty() {
                        "no sessions yet".to_string()
                    } else {
                        format!(
                            "{} session{}",
                            sessions.len(),
                            if sessions.len() == 1 { "" } else { "s" }
                        )
                    };
                    let name = Path::new(&other)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| other.clone());
                    rows.push(
                        MenuRow::new(name, json!(other)).described(&format!("{other} · {detail}")),
                    );
                }
                if rows.is_empty() {
                    self.say_dim("  No other projects yet. Use /project PATH to open one.");
                    return Ok(());
                }
                match self.pick("Open a project in a new window", rows) {
                    Some(value) => value.as_str().unwrap_or("").to_string(),
                    None => return Ok(()),
                }
            }
        };
        let expanded = expand_home(&path);
        let resolved = conversations::resolve(&expanded);
        if !resolved.is_dir() {
            bail!("No such folder: {}", resolved.display());
        }
        let opener = Arc::clone(&self.hooks().open_window);
        let name = resolved
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| resolved.display().to_string());
        match opener(&resolved) {
            Some(where_) => self.say_line(styled(
                format!("  Opened {name} in {where_}."),
                bold(ORANGE),
            )),
            None => {
                self.say(
                    "  No terminal app found to open a window with. Run this in a new terminal:",
                    Some(CORAL),
                );
                let command = crate::platform::window_command(&resolved)
                    .iter()
                    .map(|w| render::quote_path(w))
                    .collect::<Vec<_>>()
                    .join(" ");
                self.say(&format!("  {command}"), Some(GRAY));
            }
        }
        Ok(())
    }

    /// Reopen an earlier conversation: switch to its mode and model, then continue the provider's session.
    pub fn resume_conversation(&self, record_id: Option<&str>) -> Result<()> {
        let (project, simple) = {
            let s = self.st();
            (s.project.clone(), s.simple())
        };
        let mut records = conversations::for_project(&project);
        if simple {
            records.retain(|record| text_of(record, "mode") == Some("solo"));
        }
        if records.is_empty() {
            self.say_dim("No earlier conversations in this project yet.");
            return Ok(());
        }
        let sessions = conversations::by_session(&records);
        let record_id: String = match record_id {
            Some(id) => id.to_string(),
            None => {
                let mut rows = Vec::new();
                for group in sessions.iter().take(40) {
                    // One row for a session, whatever modes it used: its newest conversation speaks for it.
                    let record = &group[0];
                    let turns: i64 = group
                        .iter()
                        .map(|r| r.get("turns").and_then(Value::as_i64).unwrap_or(0))
                        .sum();
                    let mut modes: Vec<String> = Vec::new();
                    for r in group {
                        let mode = text_of(r, "mode").unwrap_or("?").to_string();
                        if !modes.contains(&mode) {
                            modes.push(mode);
                        }
                    }
                    let named = conversations::display_name(record);
                    let label: String = named
                        .clone()
                        .unwrap_or_else(|| {
                            text_of(record, "title").unwrap_or("(untitled)").to_string()
                        })
                        .chars()
                        .take(56)
                        .collect();
                    let title: String = text_of(record, "title")
                        .unwrap_or("")
                        .chars()
                        .take(40)
                        .collect();
                    let model = text_of(record, "model")
                        .or_else(|| text_of(record, "alias"))
                        .unwrap_or("None");
                    let updated = record.get("updated").and_then(Value::as_f64).unwrap_or(0.0);
                    let description = format!(
                        "{}{} · {} · {} · {} turn{}",
                        if named.is_some() {
                            format!("{title} · ")
                        } else {
                            String::new()
                        },
                        modes.join(" + "),
                        model,
                        conversations::ago(updated, None),
                        turns,
                        if turns == 1 { "" } else { "s" }
                    );
                    rows.push(
                        MenuRow::new(label, json!(text_of(record, "id").unwrap_or("")))
                            .described(&description),
                    );
                }
                match self.pick("Resume a conversation", rows) {
                    Some(value) => value.as_str().unwrap_or("").to_string(),
                    None => return Ok(()),
                }
            }
        };
        let Some(record) = records
            .iter()
            .find(|r| text_of(r, "id") == Some(record_id.as_str()))
            .cloned()
        else {
            bail!("That conversation is not in this project's history.");
        };
        let group = sessions
            .iter()
            .find(|group| group.contains(&record))
            .cloned()
            .unwrap_or_default();
        let aliases = self
            .st()
            .config
            .get("aliases")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let alias = text_of(&record, "alias").unwrap_or("").to_string();
        let role = text_of(&record, "role").unwrap_or("").to_string();
        let mode = text_of(&record, "mode").unwrap_or("").to_string();
        if aliases.get(&alias).is_none() {
            bail!("The account {alias} used by that conversation is no longer configured.");
        }
        if !matches!(mode.as_str(), "plan" | "work" | "review" | "solo")
            || !render::ROLE_TITLES.iter().any(|(name, _)| *name == role)
        {
            bail!("That conversation was saved in a mode this version cannot resume.");
        }
        let mut profile = aliases[&alias].clone();
        if let Some(model) = text_of(&record, "model") {
            profile["model"] = json!(model);
        }
        // Match the conversation: its role's model, then its mode.
        {
            let mut s = self.st();
            s.selected.insert(role.clone(), Some(alias.clone()));
            s.profiles.insert(role.clone(), profile.clone());
            if role == "review" {
                s.reviewer = "chosen".into();
            }
        }
        let auto_solo = role == "solo" && self.st().auto;
        if auto_solo {
            // The resumed conversation keeps the model it had; auto stays on for the next new conversation.
            let steps = self.routing_steps();
            self.st().route = Some(super::app::Route {
                index: routing::position(&steps, &alias, &profile),
                score: None,
                harder: 0,
            });
        } else {
            let _ = remember_role(&role, &alias, text_of(&profile, "model"), None);
        }
        {
            let mut s = self.st();
            s.sync_reviewer();
            s.set_mode(&mode);
            if let Some(name) = text_of(&record, "session").filter(|name| workspace::valid(name)) {
                s.session_name = Some(name.to_string());
                s.task_title.clear(); // The resumed session's own name, not a message from before, names the window.
            }
        }
        let purpose = if matches!(mode.as_str(), "solo" | "review") {
            Some(mode.clone())
        } else {
            None
        };
        self.status(Some("resuming"));
        let resumed = self.session(
            &alias,
            &profile,
            purpose.as_deref(),
            SessionRequest {
                resume: text_of(&record, "provider_id").map(str::to_string),
                conversation: text_of(&record, "id").map(str::to_string),
                track: true,
            },
        )?;
        {
            let mut s = self.st();
            s.adopt(&record, resumed.id);
            s.continue_session(&group, Some(purpose.as_deref().unwrap_or("plan")));
            let mut line = vec![span("  Resumed", bold(ORANGE))];
            line.push(span(
                format!(
                    " · {} · {}",
                    mode.to_uppercase(),
                    model_name(&profile, Some(&alias))
                ),
                bold(TEAL),
            ));
            let shown = conversations::display_name(&record)
                .unwrap_or_else(|| text_of(&record, "title").unwrap_or("").to_string());
            line.push(span(format!(" · {shown}"), fg(GRAY)));
            line.push(span(
                format!(" · tickets {}", workspace::relative(&s.project, &s.lanes())),
                fg(DIM),
            ));
            s.say_line(Line::from(line));
            if text_of(&record, "provider_id").is_none() {
                s.say(
                    "  The provider kept no session for it, so this continues in a fresh one.",
                    Some(DIM),
                );
            }
            for (you, answer) in conversations::transcript(&record) {
                s.say_line(styled(format!("> {you}"), bold(ORANGE)));
                let answer = answer.trim().to_string();
                s.say_markdown(if answer.is_empty() {
                    "(no reply)"
                } else {
                    &answer
                });
            }
            s.say("  Continue where you left off.", Some(DIM));
        }
        Ok(())
    }

    // ---- usage -----------------------------------------------------------------------------------------

    /// How much of each plan is left, as the usage bar shows it but with every window and reset time.
    pub fn usage(&self) {
        let mid_turn = {
            let s = self.st();
            s.active.is_some() || s.child_pid.is_some()
        };
        if mid_turn {
            // A model is mid-turn, and a Claude session answers one request at a time: asking now would wait.
            self.say_dim("  Latest figures; they refresh when the current turn ends.");
        } else {
            self.status(Some("checking usage"));
            self.refresh_quotas(true);
        }
        let mut s = self.st();
        for (key, (label, _, _)) in s.usage_groups() {
            let windows = s.quotas.get(&key).cloned();
            let mut line = vec![span(format!("  {label}"), bold(ORANGE))];
            if key[0] == "local" {
                line.push(span("   local model, no plan limits", fg(DIM)));
            } else if windows.is_none() {
                line.push(span(
                    "   usage unavailable (not signed in, or the provider did not report it)",
                    fg(DIM),
                ));
            } else if windows.as_ref().is_some_and(Vec::is_empty) {
                line.push(span("   no plan limits (API billing)", fg(DIM)));
            }
            s.say_line(Line::from(line));
            for window in windows.unwrap_or_default() {
                let left = render::remaining(window.percent);
                let mut row = vec![span("    ", Default::default())];
                row.extend(render::usage_bar(window.percent, 20).0);
                row.push(span(
                    format!("  {left}% left"),
                    fg(render::left_color(left)),
                ));
                row.push(span(
                    format!(
                        "  {}",
                        if window.window.is_empty() {
                            "window"
                        } else {
                            &window.window
                        }
                    ),
                    fg(GRAY),
                ));
                let reset = render::countdown(window.resets_at, None);
                if !reset.is_empty() {
                    row.push(span(format!("  resets in {reset}"), fg(DIM)));
                }
                s.say_line(Line::from(row));
            }
        }
        if !s.spend_by.is_empty() {
            // What the one figure on the input box is made of, by model and by how its work is paid for.
            let total = s.spent;
            s.say_line(styled(
                format!("  Spend in this window   ${total:.2}"),
                bold(ORANGE),
            ));
            let entries: Vec<((String, String), Option<f64>)> =
                s.spend_by.iter().map(|(k, v)| (k.clone(), *v)).collect();
            for ((model, whence), dollars) in entries {
                let mut row = vec![span(format!("    {model} · {whence}"), fg(GRAY))];
                match dollars {
                    None => row.push(span(
                        "   no price set: add it under \"prices\" in the configuration",
                        fg(CORAL),
                    )),
                    Some(_) if !spend::counted(&whence, Some(&s.config)) => row.push(span(
                        format!(
                            "   not counted{}",
                            if whence == "plan" {
                                " (covered by a plan)"
                            } else {
                                ""
                            }
                        ),
                        fg(DIM),
                    )),
                    Some(dollars) => row.push(span(format!("   ${dollars:.2}"), fg(TEAL))),
                }
                s.say_line(Line::from(row));
            }
        }
    }

    // ---- login and native ------------------------------------------------------------------------------

    pub fn login(&self) -> Result<()> {
        let (profile, project) = {
            let s = self.st();
            (s.prof(&s.role()), s.project.clone())
        };
        let Some(engine) = engine_of(&profile).map(str::to_string) else {
            bail!("'engine'")
        };
        if !matches!(engine.as_str(), "claude" | "codex") {
            self.say_dim(&format!(
                "/login: {engine} has no sign-in; set its key in your shell."
            ));
            return Ok(());
        }
        let mut command =
            Command::new(which::which(&engine).unwrap_or_else(|_| engine.clone().into()));
        if engine == "claude" {
            command.args(["auth", "login"]);
        } else {
            command.arg("login");
        }
        command.current_dir(&project);
        if let Some(dir) = text_of(&profile, "claude_config_dir") {
            command.env("CLAUDE_CONFIG_DIR", expand_home(dir));
        }
        let code = self.suspend(|| command.status().map(exit_code).unwrap_or(-1));
        if code == 0 {
            self.say("Signed in. Send your message again.", None);
        } else {
            self.say(&format!("Login failed (exit {code})."), None);
        }
        Ok(())
    }

    pub fn native(&self) -> Result<()> {
        let (profile, project) = {
            let s = self.st();
            (s.prof(&s.role()), s.project.clone())
        };
        let Some(engine) = engine_of(&profile).map(str::to_string) else {
            bail!("'engine'")
        };
        let executable = if matches!(engine.as_str(), "ollama" | "openai") {
            "qwen".to_string()
        } else {
            engine.clone()
        };
        let mut command =
            Command::new(which::which(&executable).unwrap_or_else(|_| executable.clone().into()));
        if let Some(dir) = text_of(&profile, "claude_config_dir") {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
        if let Some(model) = text_of(&profile, "model") {
            command.args(["--model", model]);
        }
        if matches!(engine.as_str(), "ollama" | "openai") {
            let key = if engine == "ollama" {
                "ollama".to_string()
            } else {
                std::env::var(text_of(&profile, "key_env").unwrap_or("")).unwrap_or_default()
            };
            command.env("OPENAI_API_KEY", key);
            let url = if engine == "ollama" {
                crate::providers::ollama_base_url()
            } else {
                text_of(&profile, "base_url")
                    .ok_or_else(|| wrong("'base_url'"))?
                    .to_string()
            };
            command.args(["--auth-type", "openai", "--openai-base-url", &url]);
        }
        command.current_dir(&project);
        let code = self.suspend(|| command.status().map(exit_code).unwrap_or(-1));
        self.say(
            &format!("Returned from native {executable} (exit {code})."),
            None,
        );
        Ok(())
    }
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .or_else(|| status.signal().map(|signal| -signal))
            .unwrap_or(-1)
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

/// Like `os.path.expanduser` for a leading `~`.
pub fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            plugins::home_dir().join(rest.trim_start_matches(['/', '\\']))
        }
        _ => std::path::PathBuf::from(path),
    }
}

fn pipe_lines<R: Read + Send + 'static>(
    source: R,
    sender: mpsc::Sender<Vec<u8>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(source);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if sender.send(raw.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    })
}
