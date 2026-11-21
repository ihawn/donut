//! The slash commands of the terminal (`Donut.command`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::text::Line;
use serde_json::{Value, json};

use super::app::{
    SessionHandle, bold, engine_of, fg, is_empty_profile, span, split_words, text_of,
};
use super::flows::{App, SessionRequest};
use super::render::{
    self, COMPLEX_COMMANDS, DIM, GRAY, NEEDS_MODEL, ORANGE, TEAL, VERBOSITY, model_name,
};
use super::widgets::MenuRow;
use crate::providers::PERMISSION_MODES;
use crate::summaries::{
    McpServer, claude_context, codex_context, is_connected, mcp_servers, parse_mcp_list, render_mcp,
};
use crate::{handholding, plugins, tickets};

const HELP_SIMPLE: &str = "/harder (a stronger model for this conversation) · /routing · /model · /resume · /clear (or /new) · /permissions [ask|accept_edits|auto|full] · /auto · /effort · /verbosity [quiet|normal|verbose|full] · /chrome [on|off] · /usage · /plugin · /mcp · /context · /copy · /project [PATH] · /rename · /login · /native · /complex (show every feature) · /exit\nDonut rates your first message in a conversation and picks the model for it; the model stays until /clear. !COMMAND runs a shell command in the project. Shift+Tab cycles Ask, Accept edits and Auto. Messages sent during a turn are queued.";
const HELP_COMPLEX: &str = "/plan · /work [next COUNT] [parallel N] [checkpoint] · /review · /solo · /resume · /session [NAME|new] · /next [COUNT] · /prev [COUNT] · /project [PATH] · /planner · /worker · /reviewer [ALIAS|planner|off] · /soloist · /handholding [low|medium|high] · /model · /permissions [ask|accept_edits|auto|full] · /auto · /verbosity [quiet|normal|verbose|full] · /chrome [on|off] · /usage · /plugin · /mcp · /context · /clear (or /new) · /ponytail band · /copy · /login · /native · /harder · /routing · /simple · /exit\n!COMMAND runs a shell command in the project; the model sees its output with your next message. Shift+Tab cycles Ask, Accept edits and Auto. Messages sent during a turn are queued. Installed plugin and skill commands (such as Ponytail's) are listed under / and forwarded to the selected session in its own syntax. Use /native for built-ins requiring the provider’s own UI.";

fn is_count(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|c| c.is_ascii_digit())
        && text.parse::<u64>().is_ok_and(|n| n > 0)
}

impl App {
    /// Run one slash command (or forward it to the provider session).
    pub fn command(&self, text: &str) -> Result<()> {
        let words = split_words(text);
        let Some((name, args)) = words.split_first() else {
            return Ok(());
        };
        let name = name.as_str();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (simple, role, auto, solo_set) = {
            let s = self.st();
            let role = s.role();
            (
                s.simple(),
                role.clone(),
                s.auto,
                !is_empty_profile(&s.prof("solo")),
            )
        };
        if simple && COMPLEX_COMMANDS.contains(&name) {
            self.say_dim(&format!(
                "  {name} is part of complex mode. /complex shows it."
            ));
            return Ok(());
        }
        if NEEDS_MODEL.contains(&name) && role == "solo" && auto && !solo_set {
            self.say_dim(&format!(
                "  Send a message first: auto picks the model then, and {name} applies to it."
            ));
            return Ok(());
        }
        match name {
            "/exit" | "/quit" => self.action_quit(),
            "/simple" | "/complex" => self.set_interface(&name[1..]),
            "/harder" => self.harder(),
            "/routing" => self.show_routing(),
            "/planner" | "/worker" | "/soloist" => {
                let role = match name {
                    "/planner" => "plan",
                    "/worker" => "work",
                    _ => "solo",
                };
                self.choose_model(role, args.first().copied())?;
            }
            "/reviewer" => self.choose_reviewer(args.first().copied(), true)?,
            "/solo" => {
                if !self.ensure_role("solo")? {
                    return Ok(());
                }
                let mut s = self.st();
                s.set_mode("solo");
                let label = s.solo_label();
                s.say_line(Line::from(vec![
                    span("  Solo mode", bold(TEAL)),
                    span(format!(" · {label}"), bold(ORANGE)),
                ]));
                s.say(
                    "  Messages go straight to the soloist, like Claude Code: no planner, tickets or review. \
                     /soloist changes its model; /plan goes back.",
                    Some(DIM),
                );
            }
            "/review" => {
                if !self.ensure_role("review")? {
                    return Ok(());
                }
                let mut s = self.st();
                s.set_mode("review");
                let who = model_name(&s.prof("review"), s.sel("review").as_deref());
                s.say_line(Line::from(vec![
                    span("  Review mode", bold(ORANGE)),
                    span(format!(" · {who}"), bold(ORANGE)),
                ]));
                s.say("  Ask the reviewer to check code or changes. It reads the project and never edits it.", Some(DIM));
            }
            "/plan" => {
                if !self.ensure_role("plan")? {
                    return Ok(());
                }
                let mut s = self.st();
                s.set_mode("plan");
                s.say("Planning mode. Describe what to plan.", Some(ORANGE));
            }
            "/work" => {
                if !self.ensure_role("work")? {
                    return Ok(());
                }
                self.st().set_mode("work");
                let choice: Option<String> = if !args.is_empty() {
                    Some(args.join(" "))
                } else {
                    let rows: Vec<MenuRow> = [
                        ("Next ticket", "next 1", "one ticket in a fresh context"),
                        (
                            "Next 3 tickets",
                            "next 3",
                            "up to three, one fresh context each",
                        ),
                        (
                            "All eligible tickets",
                            "next all",
                            "until the queue is empty or blocked",
                        ),
                        (
                            "Next ticket + checkpoint",
                            "next 1 checkpoint",
                            "git commit after it",
                        ),
                        (
                            "All tickets + checkpoints",
                            "next all checkpoint",
                            "git commit after each",
                        ),
                    ]
                    .iter()
                    .map(|(label, value, description)| {
                        MenuRow::new(*label, json!(value))
                            .described(description)
                            .command(&format!("/work {value}"))
                    })
                    .collect();
                    self.pick_with("Work tickets", rows, "or type /work next N for any number")
                        .and_then(|value| value.as_str().map(str::to_string))
                };
                if let Some(choice) = choice.filter(|choice| !choice.is_empty()) {
                    self.work_tickets(&choice)?;
                }
                // Back to planning, so the next message is a follow-up rather than refused.
                self.st().set_mode("plan");
            }
            "/session" => self.choose_session(args.first().copied())?,
            "/next" | "/prev" => {
                if let Some(first) = args.first()
                    && !is_count(first)
                {
                    self.say_dim(&format!(
                        "  Use {name} or {name} COUNT, where COUNT is a positive number."
                    ));
                    return Ok(());
                }
                let count = args.first().and_then(|n| n.parse::<usize>().ok());
                let (project, lanes) = {
                    let s = self.st();
                    (s.project.clone(), s.lanes())
                };
                let found = if name == "/next" {
                    tickets::upcoming(&project, Some(&lanes), count)
                } else {
                    tickets::finished(&project, Some(&lanes), count)
                };
                if found.is_empty() {
                    self.say_dim(if name == "/next" {
                        "  No tickets waiting."
                    } else {
                        "  No tickets worked yet."
                    });
                } else {
                    self.st().say_markdown(&tickets::table(&found));
                }
            }
            "/project" => {
                // The path exactly as typed: shell-style splitting would eat a Windows path's backslashes.
                let rest = text.trim()[name.len()..]
                    .trim()
                    .trim_matches(['"', '\''])
                    .to_string();
                self.open_project(if rest.is_empty() { None } else { Some(&rest) })?;
            }
            "/resume" => self.resume_conversation(args.first().copied())?,
            "/model" => {
                if role == "review" {
                    self.choose_reviewer(None, true)?;
                } else {
                    self.choose_model(&role, None)?;
                }
            }
            "/permissions" => {
                let chosen: Option<String> = match args.first() {
                    Some(mode) if PERMISSION_MODES.iter().any(|(key, _, _)| key == mode) => {
                        Some((*mode).to_string())
                    }
                    _ => {
                        let rows: Vec<MenuRow> = PERMISSION_MODES
                            .iter()
                            .map(|(mode, label, description)| {
                                MenuRow::new(*label, json!(mode)).described(description)
                            })
                            .collect();
                        let current = {
                            let s = self.st();
                            PERMISSION_MODES
                                .iter()
                                .find(|(key, _, _)| *key == s.permission)
                                .map(|(_, label, _)| *label)
                                .unwrap_or("")
                        };
                        self.pick(&format!("Permission mode · now {current}"), rows)
                            .and_then(|v| v.as_str().map(str::to_string))
                    }
                };
                if let Some(chosen) = chosen {
                    self.set_permission(&chosen, true);
                }
            }
            "/effort" => self.choose_effort(args.first().copied())?,
            "/focus" => {
                // Claude Code's /focus is unavailable in its headless mode, so Donut's Quiet level stands in for it.
                if let Some(first) = args.first()
                    && !matches!(*first, "on" | "off")
                {
                    self.say_dim("  Use /focus [on|off].");
                } else {
                    let on = {
                        let mut s = self.st();
                        s.focus_view = match args.first() {
                            Some(first) => *first == "on",
                            None => !s.focus_view,
                        };
                        s.focus_view
                    };
                    self.set_verbosity(if on { "quiet" } else { "normal" });
                }
            }
            "/chrome" => {
                if let Some(first) = args.first()
                    && !matches!(*first, "on" | "off")
                {
                    self.say_dim("  Use /chrome [on|off].");
                } else {
                    let on = match args.first() {
                        Some(first) => *first == "on",
                        None => !self.st().chrome_on(),
                    };
                    self.set_chrome(on);
                }
            }
            "/handholding" => {
                let current = handholding::level(&self.st().config);
                let chosen: Option<String> = match args.first() {
                    Some(level) if handholding::LEVELS.iter().any(|(key, _)| key == level) => {
                        Some((*level).to_string())
                    }
                    Some(_) => {
                        self.say_dim("  Use /handholding [low|medium|high].");
                        None
                    }
                    None => {
                        let rows: Vec<MenuRow> = handholding::LEVELS
                            .iter()
                            .map(|(key, (label, description, _))| {
                                MenuRow::new(*label, json!(key)).described(description)
                            })
                            .collect();
                        let label = handholding::LEVELS
                            .iter()
                            .find(|(key, _)| *key == current)
                            .map_or("", |(_, (label, _, _))| *label);
                        self.pick_with(
                            &format!("How much the planner's tickets hold the worker's hand · now {label}"),
                            rows,
                            "Higher means smaller, more exact tickets, for small or weak worker models",
                        )
                        .and_then(|v| v.as_str().map(str::to_string))
                    }
                };
                if let Some(chosen) = chosen {
                    self.set_handholding(&chosen);
                }
            }
            "/verbosity" => {
                let chosen: Option<String> = match args.first() {
                    Some(level) if VERBOSITY.iter().any(|(key, _)| key == level) => {
                        Some((*level).to_string())
                    }
                    _ => {
                        let rows: Vec<MenuRow> = VERBOSITY
                            .iter()
                            .map(|(level, (label, description))| {
                                MenuRow::new(*label, json!(level)).described(description)
                            })
                            .collect();
                        let now = {
                            let s = self.st();
                            render::verbosity_entry(&s.verbosity).map_or("", |(label, _)| label)
                        };
                        self.pick_with(
                            &format!("What Donut shows while models work · now {now}"),
                            rows,
                            "Display only: models think, work and answer exactly the same at every level",
                        )
                        .and_then(|v| v.as_str().map(str::to_string))
                    }
                };
                if let Some(chosen) = chosen {
                    self.set_verbosity(&chosen);
                }
            }
            "/auto" => {
                let (current, previous) = {
                    let s = self.st();
                    (s.permission.clone(), s.previous_permission.clone())
                };
                if current == "auto" {
                    let fallback = if previous != "auto" {
                        previous
                    } else {
                        "ask".to_string()
                    };
                    self.set_permission(&fallback, true);
                } else {
                    self.set_permission("auto", true);
                }
            }
            "/usage" => self.usage(),
            "/clear" | "/new" => self.start_fresh(),
            "/ponytail" if args.first() == Some(&"band") => {
                // Ponytail's band is a Claude Code UI module; in Donut the same switch shows or hides Donut's band.
                if args.get(1) == Some(&"import") {
                    self.say_dim("  /ponytail band import reads Claude Code's own session files; run it there (/native).");
                    return Ok(());
                }
                let hidden = self.st().config.get("ponytail_band") == Some(&Value::Bool(false));
                let shown = match args.get(1).copied() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => hidden,
                };
                if let Err(error) = self.save_config_key("ponytail_band", json!(shown)) {
                    self.say_dim(&format!(
                        "Band setting applies to this session only: {error}"
                    ));
                }
                let mut s = self.st();
                s.config["ponytail_band"] = json!(shown);
                s.say(
                    &format!(
                        "  Ponytail band {}. /ponytail band switches it back.",
                        if shown { "shown" } else { "hidden" }
                    ),
                    Some(DIM),
                );
                s.status(None);
            }
            "/copy" => {
                let last = self.st().last_answer.clone();
                self.copy_text(&last);
                self.st()
                    .notify("Copied last response (terminal clipboard support required).");
            }
            "/help" if simple => self.say(HELP_SIMPLE, None),
            "/help" => self.say(HELP_COMPLEX, None),
            "/plugin" | "/mcp" | "/context" => self.provider_info(name)?,
            "/login" => self.login()?,
            "/native" => self.native()?,
            "/rename" if engine_of(&self.st().prof(&role)) != Some("claude") => {
                // Only Claude Code has its own /rename; elsewhere Donut renames the session's tickets folder.
                if args.is_empty() {
                    self.say_dim("  Use /rename NAME. The tickets folder becomes .donut/NAME/.");
                    return Ok(());
                }
                self.st().rename_session(&args.join(" "));
            }
            _ => self.forward(text, name, &role)?,
        }
        Ok(())
    }

    /// A command the provider session itself answers, sent in its own syntax.
    fn forward(&self, text: &str, name: &str, role: &str) -> Result<()> {
        let (alias, profile, purpose, project) = {
            let s = self.st();
            (
                s.sel(role).unwrap_or_default(),
                s.prof(role),
                s.purpose(),
                s.project.clone(),
            )
        };
        let handle = self.session(
            &alias,
            &profile,
            purpose.as_deref(),
            SessionRequest::default(),
        )?;
        {
            let mut s = self.st();
            s.active = Some(handle);
            s.name_task(text);
            s.start_turn(role, "", "thinking");
            s.reset_turn();
        }
        let available = plugins::commands(&profile, Some(&project));
        self.ask(text, Some(&plugins::translate(text, &available)))?;
        self.st().answer();
        if name == "/rename" {
            // Claude Code renamed its session (to the name given, or one it made up): the folder follows.
            let reply = self.st().turn_text.clone();
            static RENAMED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r"Session renamed to:\s*(.+)").expect("regex")
            });
            if let Some(found) = RENAMED.captures(&reply) {
                let new = found[1]
                    .trim()
                    .trim_matches(['`', '*', '"', '\''])
                    .to_string();
                self.st().rename_session(&new);
            }
        }
        Ok(())
    }

    /// `claude mcp list` for this login: every server it is set up with, whether or not a session loaded them.
    pub fn configured_mcp(&self, profile: &Value) -> Vec<(String, String)> {
        let project = self.st().project.clone();
        let list = Arc::clone(&self.hooks().mcp_list);
        list(&project, profile)
    }

    /// /mcp for Claude: the session's servers, and any the login has that the session started without.
    ///
    /// A running Claude session never picks up a server connected after it started, such as a claude.ai
    /// connector added since. When one is missing, Donut reopens the session on the same conversation so the
    /// new process loads it.
    fn claude_mcp(&self, handle: SessionHandle, provider: &str) -> Result<()> {
        self.status(Some("checking MCP servers"));
        let mut handle = handle;
        let mut live = mcp_servers(&handle.session.control("mcp_status", json!({}))?);
        let profile = {
            let s = self.st();
            match s.rec(handle.id) {
                Some(rec) => rec.profile.clone(),
                None => s.prof(&s.role()),
            }
        };
        let configured = self.configured_mcp(&profile);
        let missing_from = |live: &[McpServer]| -> Vec<(String, String)> {
            configured
                .iter()
                .filter(|(name, _)| !live.iter().any(|server| &server.name == name))
                .cloned()
                .collect()
        };
        let mut missing = missing_from(&live);
        let ready: Vec<&str> = missing
            .iter()
            .filter(|(_, status)| is_connected(status))
            .map(|(name, _)| name.as_str())
            .collect();
        if !ready.is_empty() && self.st().active.is_none() {
            self.say_dim(&format!(
                "  {} connected after this session started; reopening it on the same conversation to load them.",
                ready.join(", ")
            ));
            handle = self.reopen(&handle)?;
            live = mcp_servers(&handle.session.control("mcp_status", json!({}))?);
            missing = missing_from(&live);
        }
        let mut s = self.st();
        for line in render_mcp(&live, provider) {
            s.say_line(line);
        }
        if !missing.is_empty() {
            s.say(
                "  Set up for this login but not in this session:",
                Some(GRAY),
            );
            for (name, status) in &missing {
                s.say(&format!("    ○ {name}  {status}"), Some(DIM));
            }
            if missing.iter().any(|(_, status)| is_connected(status)) {
                s.say(
                    "  Run /mcp again when the current turn ends to load them.",
                    Some(DIM),
                );
            }
        }
        Ok(())
    }

    /// Restart a Claude session's process on the same conversation, keeping Donut's own bookkeeping.
    fn reopen(&self, handle: &SessionHandle) -> Result<SessionHandle> {
        let (key, profile, conversation, seen, task) = {
            let s = self.st();
            let rec = s
                .rec(handle.id)
                .ok_or_else(|| anyhow::anyhow!("session vanished"))?;
            (
                rec.key.clone(),
                rec.profile.clone(),
                rec.conversation_id.clone(),
                rec.donut_seen,
                rec.task.clone(),
            )
        };
        let (alias, purpose) = match key.0.split_once('@') {
            Some((alias, purpose)) => (alias.to_string(), Some(purpose.to_string())),
            None => (key.0.clone(), None),
        };
        let fresh = self.session(
            &alias,
            &profile,
            purpose.as_deref(),
            SessionRequest {
                resume: handle.session.provider_id(),
                conversation: Some(conversation.clone().unwrap_or_default()),
                track: true,
            },
        )?;
        let mut s = self.st();
        if let Some(rec) = s.rec_mut(fresh.id) {
            rec.conversation_id = conversation; // The same /resume record.
            rec.donut_seen = seen;
            if task.is_some() {
                rec.task = task;
            }
        }
        for exchange in s.exchanges.iter_mut().filter(|e| e.by == handle.id) {
            exchange.by = fresh.id; // Its own turns stay its own: they are in the resumed provider session.
        }
        Ok(fresh)
    }

    /// /plugin, /mcp and /context: what the provider can tell about itself.
    fn provider_info(&self, name: &str) -> Result<()> {
        let (alias, profile, purpose) = {
            let s = self.st();
            let role = s.role();
            (s.sel(&role), s.prof(&role), s.purpose())
        };
        let handle = self.session(
            alias.as_deref().unwrap_or(""),
            &profile,
            purpose.as_deref(),
            SessionRequest::default(),
        )?;
        let provider = alias.clone().unwrap_or_else(|| "provider".to_string());
        let model = text_of(&profile, "model").unwrap_or("").to_string();
        match handle.session.family() {
            "claude" => match name {
                "/plugin" => {
                    let commands = handle
                        .session
                        .info()
                        .get("commands")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let rows: Vec<MenuRow> = commands
                        .iter()
                        .filter(|item| item.is_object())
                        .map(|item| {
                            let item_name = text_of(item, "name").unwrap_or("?");
                            let description: String = text_of(item, "description")
                                .unwrap_or("")
                                .chars()
                                .take(100)
                                .collect();
                            MenuRow::new(
                                format!("{item_name} · {description}"),
                                json!(format!("/{}", text_of(item, "name").unwrap_or(""))),
                            )
                        })
                        .collect();
                    if let Some(selected) =
                        self.pick("Claude plugin commands · /native for management", rows)
                        && let Some(text) = selected.as_str()
                    {
                        let mut s = self.st();
                        s.draft.load_text(&format!("{text} "));
                    }
                }
                "/mcp" => self.claude_mcp(handle, &provider)?,
                _ => {
                    let data = handle.session.control("get_context_usage", json!({}))?;
                    let mut s = self.st();
                    for line in claude_context(&data) {
                        s.say_line(line);
                    }
                }
            },
            "codex" => match name {
                "/mcp" => {
                    let data = handle.session.control("mcpServerStatus/list", json!({}))?;
                    let mut s = self.st();
                    for line in render_mcp(&mcp_servers(&data), &provider) {
                        s.say_line(line);
                    }
                }
                "/context" => {
                    let lines = codex_context(&handle.session.usage(), &model);
                    let mut s = self.st();
                    for line in lines {
                        s.say_line(line);
                    }
                }
                _ => {
                    let data = handle.session.control("plugin/list", json!({}))?;
                    let mut rows = Vec::new();
                    for market in data
                        .get("marketplaces")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        for plugin in market
                            .get("plugins")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            let state = if plugin
                                .get("installed")
                                .is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)))
                            {
                                "installed"
                            } else {
                                "not installed"
                            };
                            let description: String = text_of(plugin, "description")
                                .unwrap_or("")
                                .chars()
                                .take(70)
                                .collect();
                            rows.push(
                                MenuRow::new(
                                    text_of(plugin, "name").unwrap_or("?"),
                                    plugin.clone(),
                                )
                                .described(
                                    format!("{state} · {description}").trim_matches([' ', '·']),
                                ),
                            );
                        }
                    }
                    if let Some(selected) =
                        self.pick("Codex plugins · /native for management", rows)
                    {
                        let mut line = vec![span(
                            format!("  {}", text_of(&selected, "name").unwrap_or("?")),
                            bold(ORANGE),
                        )];
                        let installed = selected
                            .get("installed")
                            .is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)));
                        line.push(span(
                            if installed {
                                "  installed"
                            } else {
                                "  not installed"
                            },
                            fg(GRAY),
                        ));
                        if let Some(description) =
                            text_of(&selected, "description").filter(|d| !d.is_empty())
                        {
                            line.push(span(format!("  {description}"), fg(DIM)));
                        }
                        let mut s = self.st();
                        s.say_line(Line::from(line));
                        s.say("  Install or remove plugins with /native.", Some(DIM));
                    }
                }
            },
            _ => self.say(
                "Use /native for Qwen plugins, MCP and context controls.",
                None,
            ),
        }
        Ok(())
    }
}

/// The real `claude mcp list`: what this login reports, claude.ai connectors included.
pub fn claude_mcp_list(project: &Path, profile: &Value) -> Vec<(String, String)> {
    let mut command =
        std::process::Command::new(which::which("claude").unwrap_or_else(|_| "claude".into()));
    command.args(["mcp", "list"]).current_dir(project);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = text_of(profile, "claude_config_dir") {
        command.env("CLAUDE_CONFIG_DIR", super::flows::expand_home(dir));
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(command.output());
    });
    match receiver.recv_timeout(Duration::from_secs(90)) {
        Ok(Ok(output)) => parse_mcp_list(&String::from_utf8_lossy(&output.stdout)),
        _ => Vec::new(),
    }
}
