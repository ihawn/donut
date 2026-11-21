//! the `donut` command line.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches, Command};
use serde_json::{Value, json};

use crate::config::{check_project, load, role_choice, state_dir, use_profile};
use crate::conversations::{json_dumps, resolve};
use crate::install_skills::install;
use crate::planner_output::{ticket_snapshot, write_created_tickets};
use crate::providers::{activate, validate};
use crate::routing::{AUTO, worker_ladder};
use crate::runner::{
    AgentOpts, BatchOptions, Console, VERIFY_FIXES, colored, is_interrupt, read_line_prompt,
    run_agent, run_batch, sayln,
};
use crate::setup::{discover, setup};
use crate::ui::{banner_text, paint, release_version, welcome_text};
use crate::{handholding, workspace};

/// What the command line starts. The real one runs everything; tests give their own where Python patched a name.
pub trait Host {
    fn run_batch(
        &mut self,
        profile: &Value,
        project: &Path,
        count: &str,
        options: BatchOptions,
    ) -> Result<i32> {
        run_batch(profile, project, count, options)
    }

    fn plan_session(
        &mut self,
        profile: &Value,
        alias: &str,
        project: &Path,
        request: Option<&str>,
        session: Option<&str>,
    ) -> Result<i32>
    where
        Self: Sized,
    {
        plan_session(self, profile, alias, project, request, session)
    }

    /// Open the terminal UI.
    fn terminal(
        &mut self,
        project: &Path,
        alias: Option<&str>,
        session: Option<&str>,
    ) -> Result<i32> {
        open_terminal(project, alias, session)
    }

    fn setup(&mut self, project: &Path, scan: Option<&str>) -> Result<()> {
        setup(project, scan)
    }

    /// Install the skills and the session's lanes.
    fn install(&mut self, project: &Path, session: Option<&str>) -> Result<()> {
        install(project, &crate::plugins::home_dir(), false, session).map(|_| ())
    }

    fn activate(&mut self, profile: &Value) -> Result<()> {
        activate(profile)
    }

    fn run_agent(
        &mut self,
        profile: &Value,
        project: &Path,
        prompt: &str,
        log: &Path,
        planner: bool,
    ) -> Result<i32> {
        run_agent(
            profile,
            project,
            prompt,
            log,
            AgentOpts {
                planner,
                ..Default::default()
            },
        )
    }

    /// A line typed by the user, or None at the end of input.
    fn input(&mut self, prompt: &str) -> Option<String> {
        read_line_prompt(prompt)
    }

    fn state_dir(&mut self) -> PathBuf {
        state_dir()
    }
}

/// The real thing.
pub struct System;

impl Host for System {}

/// Where the terminal UI starts. The terminal module owns it; this is the one place that calls it.
fn open_terminal(project: &Path, alias: Option<&str>, session: Option<&str>) -> Result<i32> {
    crate::terminal::run(project, alias, session)
}

pub fn plan_session<H: Host + ?Sized>(
    host: &mut H,
    profile: &Value,
    alias: &str,
    project: &Path,
    request: Option<&str>,
    session: Option<&str>,
) -> Result<i32> {
    host.install(project, session)?;
    let lanes = workspace::lanes_dir(project, session);
    let folder = workspace::relative(project, &lanes);
    let state = host.state_dir();
    std::fs::create_dir_all(&state)?;
    let run = tempfile::Builder::new()
        .prefix("plan-")
        .tempdir_in(&state)?
        .keep();
    let engine = profile.get("engine").and_then(Value::as_str).unwrap_or("");
    let model = match profile.get("model").and_then(Value::as_str) {
        Some(model) if !model.is_empty() => model.to_string(),
        _ => format!("{engine} default"),
    };
    sayln!(
        "Planner: {alias} ({model})\nProject: {}\nTickets: {folder}\nLogs: {}",
        project.display(),
        run.display()
    );
    // /handholding: how small and exact the tickets are.
    let guidance = match load() {
        Ok(config) => handholding::instruction(&config),
        Err(_) => handholding::instruction(&json!({})),
    };
    let interactive = request.is_none();
    if interactive {
        sayln!(
            "Describe what to plan. Enter /exit to leave (or Ctrl-D; Windows: Ctrl-Z then Enter)."
        );
    }
    let mut request = request.map(str::to_string);
    let mut history: Vec<String> = Vec::new();
    loop {
        if interactive {
            let Some(line) = host.input(&paint("\nPlan > ", "teal")) else {
                return Ok(0);
            };
            let line = line.trim().to_string();
            if line == "/exit" || line == "/quit" {
                return Ok(0);
            }
            if line.is_empty() {
                continue;
            }
            request = Some(line);
        }
        let asked = request.clone().unwrap_or_default();
        let mut prompt = format!(
            "Planning only: read skills/donut-planner/SKILL.md and project instructions. \
             This explicit planner assignment overrides worker-only prohibitions on creating tickets. \
             This session's tickets live in {folder}; leave every other tickets folder alone. \
             Research the request and write scoped tickets in {folder}to_do/. Inspect existing tickets \
             before adding or updating them; preserve work already in progress or done. Inspect {folder}on_hold/ \
             to reserve held ticket numbers and avoid duplicates. Keep held specifications consistent, but \
             release them only on an explicit user request. Inspect {folder}blocked/ too: reserve its numbers, \
             investigate blockers, but requeue only when the user requests retry and the blocker is addressed. \
             Never queue work depending on held or blocked tickets. Do not implement, \
             commit or push. Your final response must be at most two short sentences describing what you did, \
             or a concise clarification question if needed. Do not repeat ticket lists or technical details; \
             Donut will list the created tickets. "
        );
        prompt += &guidance;
        prompt += " Earlier user requests in this planning session:\n";
        prompt += &json_dumps(&json!(history), None);
        prompt += "\nUser request:\n";
        prompt += &asked;
        if !history.is_empty() {
            prompt += &format!(
                "\nPrevious planner output (consult for follow-up context): {}",
                run.join(format!("{:03}.log", history.len())).display()
            );
        }
        let before = ticket_snapshot(project, Some(&lanes))?;
        host.activate(profile)?;
        let log = run.join(format!("{:03}.log", history.len() + 1));
        let ran = host.run_agent(profile, project, &prompt, &log, true);
        if engine == "ollama" {
            host.activate(profile)?;
        }
        let code = ran?;
        if code != 0 {
            sayln!(
                "{}",
                paint(
                    "PLANNING FAILED\nCheck the output and logs before running workers.",
                    "coral"
                )
            );
            return Ok(code);
        }
        history.push(asked);
        write_created_tickets(&mut Console, project, &before, Some(&lanes))?;
        sayln!("{}", paint("\nPLANNING COMPLETE", "teal"));
        let tail = match session {
            Some(name) if !name.is_empty() => format!(" --session {name}`"),
            _ => "`".to_string(),
        };
        sayln!("Try exiting the shell and running `donut --work next 1{tail}");
        if !interactive {
            return Ok(0);
        }
    }
}

/// clap keeps its strings for the life of the program; a parser is built a few times at most.
fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn shared_args() -> Vec<Arg> {
    vec![
        Arg::new("model")
            .short('m')
            .long("model")
            .value_name("MODEL")
            .global(true)
            .help("configured model alias"),
        Arg::new("project")
            .short('p')
            .long("project")
            .value_name("PROJECT")
            .global(true)
            .help("project path (default: current directory)"),
        Arg::new("session")
            .short('s')
            .long("session")
            .value_name("SESSION")
            .global(true)
            .help("tickets folder .donut/SESSION/ (default: the latest session, else swim_lanes/, else a new session)"),
        Arg::new("profile")
            .long("profile")
            .value_name("NAME")
            .global(true)
            .help("use the accounts, conversations and tickets of profile NAME only (default: the default profile)"),
    ]
}

/// The argument parser. release is what -v prints after "donut ".
pub fn command(release: &str) -> Command {
    let verify_fixes_help = format!(
        "with --work: times a failed Verify goes back to the worker before the ticket is blocked (default {VERIFY_FIXES}, 0 = never)"
    );
    let mut parser = Command::new("donut")
        .about(
            "Your planning model writes the tickets. Your worker model takes them one at a time. Do the thing. \
             Local or hosted coding agents.",
        )
        .version(leak(release.to_string()))
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .disable_help_flag(true)
        .arg(Arg::new("help").short('h').long("help").action(ArgAction::Help).global(true).help("show this help message and exit"))
        .arg(Arg::new("version").short('v').long("version").action(ArgAction::Version).help("show program's version number and exit"))
        .arg(
            Arg::new("plan")
                .long("plan")
                .value_name("REQUEST")
                .num_args(0..=1)
                .default_missing_value("")
                .help("open interactive planner; optionally plan one quoted request"),
        )
        .arg(
            Arg::new("work")
                .long("work")
                .value_name("NEXT_COUNT")
                .num_args(0..)
                .allow_negative_numbers(true)
                .help("work next COUNT tickets (for example: --work next 1); bare --work runs all"),
        )
        .group(ArgGroup::new("modes").args(["plan", "work"]).multiple(false))
        .arg(Arg::new("checkpoint").long("checkpoint").action(ArgAction::SetTrue).help(
            "with --work: git commit the working tree after every ticket so each step can be reverted",
        ))
        .arg(Arg::new("review").long("review").value_name("ALIAS").help(
            "with --work: alias that reviews each finished ticket, or off (default: the saved reviewer, else the planner)",
        ))
        .arg(Arg::new("recover").long("recover").value_name("ALIAS").help(
            "with --work: alias that revises blocked tickets and repairs stuck ones, or off (default: the planner)",
        ))
        .arg(
            Arg::new("no_pr")
                .long("no-pr")
                .action(ArgAction::SetTrue)
                .help("with --work: do not raise a pull request after the reviewer approves the run"),
        )
        .arg(
            Arg::new("review_rounds")
                .long("review-rounds")
                .value_name("N")
                .value_parser(clap::value_parser!(i64))
                .allow_negative_numbers(true)
                .help("with --work: reviews per ticket before it is blocked (default 3; with -m auto, one per auto step)"),
        )
        .arg(Arg::new("fallback").long("fallback").value_name("ALIAS").help(
            "with --work: alias that takes over when the worker's provider fails, or off (default: the worker alias's \"fallback\")",
        ))
        .arg(
            Arg::new("verify_fixes")
                .long("verify-fixes")
                .value_name("N")
                .value_parser(clap::value_parser!(i64))
                .allow_negative_numbers(true)
                .default_value(leak(VERIFY_FIXES.to_string()))
                .hide_default_value(true)
                .help(verify_fixes_help),
        )
        .arg(
            Arg::new("parallel")
                .long("parallel")
                .value_name("N")
                .value_parser(clap::value_parser!(i64))
                .allow_negative_numbers(true)
                .default_value("1")
                .hide_default_value(true)
                .help("with --work: run up to N independent tickets at once, each in its own git worktree (turns on --checkpoint)"),
        );
    for arg in shared_args() {
        parser = parser.arg(arg);
    }
    // The last of a repeated option wins, as in argparse.
    parser
        .mut_args(|arg| {
            let id = arg.get_id().clone();
            arg.overrides_with(id)
        })
        .subcommand(
            Command::new("setup")
                .about("choose local/hosted orchestrator, discover models and set up project")
                .arg(
                    Arg::new("scan")
                        .long("scan")
                        .value_name("SCAN")
                        .help("additional directory to scan for model files"),
                ),
        )
        .subcommand(Command::new("welcome").about("show the installation welcome screen"))
        .subcommand(Command::new("models").about("list configured aliases"))
        .subcommand(
            Command::new("discover")
                .about("find Ollama models and GGUF/Safetensors files")
                .arg(
                    Arg::new("scan")
                        .long("scan")
                        .value_name("SCAN")
                        .help("directory to scan instead of common model folders"),
                ),
        )
        .subcommand(
            Command::new("init")
                .about("install planner/worker skills and a session's ticket lanes"),
        )
        .subcommand(
            Command::new("add-skills")
                .about("install planner/worker skills and a session's ticket lanes"),
        )
        .subcommand(
            Command::new("do")
                .about("compatibility form of --work COUNT")
                .arg(Arg::new("next").required(true).value_parser(["next"]))
                .arg(
                    Arg::new("count")
                        .required(true)
                        .help("positive number or all"),
                ),
        )
        .subcommand(
            Command::new("plan")
                .about("compatibility form of --plan REQUEST")
                .arg(
                    Arg::new("request")
                        .required(true)
                        .help("quoted description of the work to plan"),
                ),
        )
}

fn expanduser(path: &str) -> PathBuf {
    if path == "~" {
        return crate::plugins::home_dir();
    }
    match path.strip_prefix("~/") {
        Some(rest) => crate::plugins::home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

fn set_env_default(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        // SAFETY: the CLI sets its own environment before it starts any thread that reads it.
        unsafe { std::env::set_var(key, value) };
    }
}

fn set_env(key: &str, value: &str) {
    // SAFETY: as above.
    unsafe { std::env::set_var(key, value) };
}

fn banner() {
    crate::runner::emit(&banner_text(colored()));
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// A usage error, printed the way argparse's parser.error does; returns the exit code 2.
fn usage_error(parser: &Command, message: &str) -> i32 {
    let error = parser
        .clone()
        .error(clap::error::ErrorKind::InvalidValue, message);
    let _ = error.print();
    error.exit_code()
}

fn text<'a>(matches: &'a ArgMatches, name: &str) -> Option<&'a str> {
    matches.get_one::<String>(name).map(String::as_str)
}

/// The command line. Returns the exit code.
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    main_with(&argv, &mut System)
}

/// main() with the arguments (argv[0] first) and the host given.
pub fn main_with<H: Host>(argv: &[String], host: &mut H) -> i32 {
    let parser = command(&release_version());
    let matches = match parser.clone().try_get_matches_from(argv) {
        Ok(matches) => matches,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    let plan = text(&matches, "plan").map(str::to_string);
    let work: Option<Vec<String>> =
        (matches.value_source("work") == Some(ValueSource::CommandLine)).then(|| {
            matches
                .get_many::<String>("work")
                .map(|values| values.cloned().collect())
                .unwrap_or_default()
        });
    let mut action: Option<String> = matches.subcommand_name().map(str::to_string);
    let mut request: Option<String> = None;
    let mut count: Option<String> = None;
    if plan.is_some() || work.is_some() {
        if action.is_some() {
            return usage_error(&parser, "Use --plan or --work without a subcommand.");
        }
        if let Some(wanted) = plan {
            action = Some("plan".into());
            request = Some(wanted).filter(|wanted| !wanted.is_empty());
        } else {
            let mut parts = work.unwrap_or_default();
            if parts.len() == 2 && parts[0] == "next" {
                parts.remove(0);
            }
            let positive = |value: &str| {
                !value.is_empty()
                    && value.chars().all(|c| c.is_ascii_digit())
                    && !value.trim_start_matches('0').is_empty()
            };
            if parts.is_empty() {
                count = Some("all".into());
            } else if parts.len() == 1 && (parts[0] == "all" || positive(&parts[0])) {
                count = Some(parts[0].clone());
            } else {
                return usage_error(
                    &parser,
                    "Use --work next COUNT, where COUNT is a positive integer or all.",
                );
            }
            action = Some("do".into());
        }
    } else if let Some((name, sub)) = matches.subcommand() {
        match name {
            "plan" => request = text(sub, "request").map(str::to_string),
            "do" => count = text(sub, "count").map(str::to_string),
            _ => {}
        }
    }
    let action = action.unwrap_or_else(|| "terminal".into());
    if action == "welcome" {
        crate::runner::emit(&welcome_text(true, &release_version(), colored()));
        return 0;
    }
    let scan = matches
        .subcommand()
        .filter(|(name, _)| matches!(*name, "setup" | "discover"))
        .and_then(|(_, sub)| text(sub, "scan"))
        .map(str::to_string);
    let project = resolve(&expanduser(text(&matches, "project").unwrap_or(".")));
    let outcome = start(host, &matches, &action, &project, request, count, scan);
    match outcome {
        Ok(code) => code,
        Err(error) if is_interrupt(&error) => {
            eprintln!("\nCancelled.");
            130
        }
        Err(error) => {
            eprintln!("{}", paint(&format!("donut: {error}"), "coral"));
            1
        }
    }
}

fn start<H: Host>(
    host: &mut H,
    matches: &ArgMatches,
    action: &str,
    project: &Path,
    request: Option<String>,
    count: Option<String>,
    scan: Option<String>,
) -> Result<i32> {
    if !project.is_dir() {
        bail!("Project directory does not exist: {}", project.display());
    }
    use_profile(
        text(matches, "profile")
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var("DONUT_PROFILE")
                    .ok()
                    .filter(|name| !name.is_empty())
            })
            .as_deref(),
    )?;
    // Connector, plugin and Chrome tools load by name only; a model fetches a tool's full definition when it
    // needs it. Every agent Donut starts inherits this. A value already set is the user's choice.
    set_env_default("ENABLE_TOOL_SEARCH", "true");
    check_project(project)?;
    let session = text(matches, "session").map(str::to_string);
    if let Some(name) = &session
        && !workspace::valid(name)
    {
        bail!("Invalid session name {name:?}: use letters, numbers, '.', '_' or '-'");
    }
    let model = text(matches, "model").map(str::to_string);
    if action == "terminal" {
        return host.terminal(project, model.as_deref(), session.as_deref());
    }
    if action == "setup" {
        host.setup(project, scan.as_deref())?;
        return Ok(0);
    }
    if action == "init" || action == "add-skills" {
        banner();
        let named = session.clone().or_else(|| workspace::default(project));
        host.install(project, named.as_deref())?;
        return Ok(0);
    }
    if action == "discover" {
        banner();
        let (models, files) = discover(scan.as_deref())?;
        for model in &models {
            sayln!("Ollama: {model}");
        }
        for file in &files {
            sayln!("Model file: {file}");
        }
        if models.is_empty() && files.is_empty() {
            sayln!("No models found in the searched locations.");
        }
        return Ok(0);
    }
    let config = load()?;
    // On unless /chrome off was saved. Read where the Claude command is built, for workers and planners.
    set_env(
        "DONUT_CHROME",
        if config.get("chrome") == Some(&Value::Bool(false)) {
            "0"
        } else {
            "1"
        },
    );
    let empty = serde_json::Map::new();
    let aliases = config
        .get("aliases")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    if action == "models" {
        for (name, profile) in aliases {
            let engine = profile.get("engine").and_then(Value::as_str).unwrap_or("");
            let model = profile
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or("provider default");
            sayln!("{name:<20} {engine:<10} {model}");
        }
        if aliases.is_empty() {
            sayln!("No aliases yet. Run donut setup.");
        }
        return Ok(0);
    }
    let role = if action == "plan" { "plan" } else { "work" };
    let wanted = model.clone().or_else(|| {
        config
            .get("roles")
            .and_then(|roles| roles.get(role))
            .and_then(|saved| saved.get("alias"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let mut ladder: Option<Vec<Value>> = None;
    let (alias, profile): (String, Value);
    if role == "work" && wanted.as_deref() == Some(AUTO) && !aliases.contains_key(AUTO) {
        let steps = worker_ladder(&config["aliases"]);
        if steps.is_empty() {
            bail!("Worker auto needs a Claude or Codex alias. Run donut setup.");
        }
        alias = AUTO.to_string();
        profile = steps[0].clone();
        ladder = Some(steps);
    } else if let Some(name) = &model {
        let Some(found) = aliases.get(name) else {
            bail!("Choose a configured alias with -m, or run donut setup.");
        };
        alias = name.clone();
        profile = found.clone();
    } else {
        // The alias and model last picked with /model in the terminal, or the role's default alias.
        let Some(chosen) = role_choice(&config, role) else {
            bail!("Choose a configured alias with -m, or run donut setup.");
        };
        (alias, profile) = chosen;
    }
    validate(&profile)?;
    banner();
    let session = session.or_else(|| workspace::default(project));
    if action == "do" {
        // An explicit alias or off, else the saved choice for the role.
        let resolve_alias =
            |flag: Option<&str>, saved_role: &str, name: &str| -> Result<Option<Value>> {
                match flag {
                    Some("off") => Ok(None),
                    Some(flag) if !flag.is_empty() => match aliases.get(flag) {
                        Some(found) => Ok(Some(found.clone())),
                        None => bail!(
                            "Unknown {name} alias: {flag}. Use --{name} ALIAS or --{name} off."
                        ),
                    },
                    _ => Ok(role_choice(&config, saved_role).map(|(_, chosen)| chosen)),
                }
            };
        let reviewer = resolve_alias(text(matches, "review"), "review", "review")?;
        let recover = text(matches, "recover")
            .filter(|v| !v.is_empty())
            .or_else(|| {
                config
                    .get("recovery")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
            });
        let planner = resolve_alias(recover, "plan", "recover")?;
        let fallback_flag = text(matches, "fallback")
            .filter(|v| !v.is_empty())
            .or_else(|| {
                profile
                    .get("fallback")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or("off");
        let fallback = resolve_alias(Some(fallback_flag), "work", "fallback")?;
        for chosen in [&reviewer, &planner, &fallback].into_iter().flatten() {
            validate(chosen)?;
        }
        // A worker model with no effort chosen thinks briefly; auto steps and an explicit effort are kept.
        let low = |chosen: Value| -> Value {
            let engine = chosen.get("engine").and_then(Value::as_str).unwrap_or("");
            if matches!(engine, "claude" | "codex") && !truthy(chosen.get("effort")) {
                let mut changed = chosen.clone();
                changed["effort"] = json!("low");
                changed
            } else {
                chosen
            }
        };
        let profile = low(profile);
        let fallback = fallback.map(low);
        let review_rounds = match matches.get_one::<i64>("review_rounds") {
            Some(rounds) => *rounds,
            None => 3.max(ladder.as_ref().map_or(0, Vec::len) as i64),
        };
        if review_rounds < 1 {
            bail!("--review-rounds must be at least 1");
        }
        let verify_fixes = matches
            .get_one::<i64>("verify_fixes")
            .copied()
            .unwrap_or(VERIFY_FIXES);
        let parallel = matches.get_one::<i64>("parallel").copied().unwrap_or(1);
        if verify_fixes < 0 || parallel < 1 {
            bail!("--verify-fixes must be 0 or more, and --parallel at least 1");
        }
        let options = BatchOptions {
            checkpoint: matches.get_flag("checkpoint"),
            reviewer,
            review_rounds,
            planner,
            lanes: Some(workspace::lanes_dir(project, session.as_deref())),
            pull_request: !matches.get_flag("no_pr")
                && config.get("pull_requests") != Some(&Value::Bool(false)),
            fallback,
            verify_fixes,
            repo_map: config.get("repo_map") != Some(&Value::Bool(false)),
            parallel,
            ladder,
        };
        return host.run_batch(
            &profile,
            project,
            count.as_deref().unwrap_or("all"),
            options,
        );
    }
    host.plan_session(
        &profile,
        &alias,
        project,
        request.as_deref(),
        session.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    use super::*;
    use crate::config::{config_path, save};
    use crate::conversations::env_lock;
    use crate::runner::{Launcher, capture};
    use crate::tickets::{Ticket, queue_report};

    /// A temporary home for the configuration (and state), with the environment put back afterwards.
    struct Fixture {
        _lock: MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        root: PathBuf,
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    fn set(key: &str, value: &str) {
        // SAFETY: every test that touches the environment holds env_lock.
        unsafe { std::env::set_var(key, value) };
    }

    impl Fixture {
        fn new() -> Fixture {
            let lock = env_lock();
            let temp = tempfile::tempdir().unwrap();
            let root = resolve(temp.path());
            let saved = ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "DONUT_PROFILE"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect();
            set("XDG_CONFIG_HOME", &root.join("config").to_string_lossy());
            set("XDG_STATE_HOME", &root.join("state").to_string_lossy());
            // SAFETY: as above.
            unsafe { std::env::remove_var("DONUT_PROFILE") };
            save(&json!({"aliases": {"worker": {"engine": "codex", "model": ""}}})).unwrap();
            Fixture {
                _lock: lock,
                _temp: temp,
                root,
                saved,
            }
        }

        fn project(&self) -> String {
            self.root.to_string_lossy().into_owned()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                // SAFETY: as above.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    type Batch = (Value, PathBuf, String);
    type Plan = (Value, String, PathBuf, Option<String>, Option<String>);

    /// A host that records what the command line starts, and that stands in where Python patched a name.
    #[derive(Default)]
    struct Mock {
        mock_batch: bool,
        mock_plan: bool,
        mock_install: bool,
        batches: Vec<Batch>,
        plans: Vec<Plan>,
        inputs: VecDeque<String>,
        activations: usize,
        agent_code: i32,
        agent_calls: Vec<(Value, String, PathBuf, bool)>,
        launcher: Option<String>,
    }

    impl Host for Mock {
        fn run_batch(
            &mut self,
            profile: &Value,
            project: &Path,
            count: &str,
            _options: BatchOptions,
        ) -> Result<i32> {
            assert!(self.mock_batch, "no batch expected");
            self.batches
                .push((profile.clone(), project.to_path_buf(), count.to_string()));
            Ok(0)
        }

        fn plan_session(
            &mut self,
            profile: &Value,
            alias: &str,
            project: &Path,
            request: Option<&str>,
            session: Option<&str>,
        ) -> Result<i32> {
            if !self.mock_plan {
                return plan_session(self, profile, alias, project, request, session);
            }
            self.plans.push((
                profile.clone(),
                alias.to_string(),
                project.to_path_buf(),
                request.map(str::to_string),
                session.map(str::to_string),
            ));
            Ok(0)
        }

        fn install(&mut self, project: &Path, session: Option<&str>) -> Result<()> {
            if self.mock_install {
                return Ok(());
            }
            install(project, &crate::plugins::home_dir(), true, session).map(|_| ())
        }

        fn activate(&mut self, _profile: &Value) -> Result<()> {
            self.activations += 1;
            Ok(())
        }

        fn run_agent(
            &mut self,
            profile: &Value,
            project: &Path,
            prompt: &str,
            log: &Path,
            planner: bool,
        ) -> Result<i32> {
            self.agent_calls.push((
                profile.clone(),
                prompt.to_string(),
                log.to_path_buf(),
                planner,
            ));
            let Some(script) = self.launcher.clone() else {
                return Ok(self.agent_code);
            };
            let launch =
                move |_: &Value, _: &str, _: bool, _: bool| -> Result<crate::providers::Launch> {
                    Ok((
                        vec!["sh".to_string(), "-c".to_string(), script.clone()],
                        std::env::vars().collect(),
                    ))
                };
            let launcher: Launcher<'_> = &launch;
            run_agent(
                profile,
                project,
                prompt,
                log,
                AgentOpts {
                    planner,
                    launcher: Some(launcher),
                    ..Default::default()
                },
            )
        }

        fn input(&mut self, _prompt: &str) -> Option<String> {
            self.inputs.pop_front()
        }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("donut")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn main_of(args: &[&str], host: &mut Mock) -> (i32, String) {
        let (code, output) = capture(|| main_with(&argv(args), host));
        (code, output)
    }

    #[test]
    fn test_load_corrects_a_dotted_claude_model_id() {
        let _fixture = Fixture::new();
        let aliases = json!({
            "claude": {"engine": "claude", "model": "claude-opus-5.5"},
            "codex": {"engine": "codex", "model": "gpt-6.1-mini"},
            "short": {"engine": "claude", "model": "opus"},
        });
        save(&json!({"aliases": aliases})).unwrap();
        let loaded = load().unwrap();
        let models: Vec<(String, String)> = loaded["aliases"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(alias, profile)| {
                (
                    alias.clone(),
                    profile["model"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = [
            ("claude", "claude-opus-5-5"),
            ("codex", "gpt-6.1-mini"),
            ("short", "opus"),
        ]
        .iter()
        .map(|(a, m)| (a.to_string(), m.to_string()))
        .collect();
        assert_eq!(models, expected);
    }

    #[test]
    fn test_donut_plan_tells_the_planner_the_saved_handholding_level() {
        let fixture = Fixture::new();
        save(&json!({"aliases": {"worker": {"engine": "codex", "model": ""}}, "handholding": "high"})).unwrap();
        let project = fixture.root.join("app");
        std::fs::create_dir(&project).unwrap();
        let mut host = Mock {
            mock_install: true,
            ..Default::default()
        };
        let (code, _) = capture(|| {
            plan_session(
                &mut host,
                &json!({"engine": "codex"}),
                "worker",
                &project,
                Some("add a dash"),
                None,
            )
            .unwrap()
        });
        assert_eq!(code, 0);
        assert!(host.agent_calls[0].1.contains("Handholding is high"));
        assert!(
            host.agent_calls[0]
                .1
                .contains("quote the exact existing lines")
        );
    }

    #[test]
    fn test_version_flags_need_no_project_or_config() {
        // The binary's -v and --version are checked in tests/cli.rs; here the text the parser prints.
        for flag in ["-v", "--version"] {
            let error = command("1.2.3")
                .try_get_matches_from(["donut", flag])
                .unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
            assert_eq!(error.exit_code(), 0);
            assert_eq!(error.render().to_string(), "donut 1.2.3\n");
        }
    }

    #[test]
    fn test_model_and_project_flags_before_or_after_command() {
        let fixture = Fixture::new();
        let project = fixture.project();
        let variants: [Vec<&str>; 2] = [
            vec!["-m", "worker", "-p", &project, "do", "next", "3"],
            vec![
                "do",
                "next",
                "3",
                "--model",
                "worker",
                "--project",
                &project,
            ],
        ];
        for args in variants {
            let mut host = Mock {
                mock_batch: true,
                ..Default::default()
            };
            let (code, _) = main_of(&args, &mut host);
            assert_eq!(code, 0, "{args:?}");
            assert_eq!(
                (host.batches[0].1.clone(), host.batches[0].2.clone()),
                (fixture.root.clone(), "3".to_string())
            );
        }
    }

    #[test]
    fn test_local_planner_uses_default_or_explicit_alias_and_model_lifecycle() {
        let fixture = Fixture::new();
        let profile = json!({"engine": "ollama", "model": "local-model", "auto_approve": true});
        save(&json!({"aliases": {"local": profile}, "planner": "local"})).unwrap();
        let project = fixture.project();
        for args in [
            vec!["plan", "a small task", "-p", &project],
            vec!["-m", "local", "plan", "a small task", "-p", &project],
        ] {
            let mut host = Mock {
                mock_install: true,
                ..Default::default()
            };
            let (code, _) = main_of(&args, &mut host);
            assert_eq!(code, 0, "{args:?}");
            assert_eq!(host.activations, 2);
            assert_eq!(host.agent_calls[0].0, profile);
            assert!(host.agent_calls[0].1.contains("Planning only"));
        }
    }

    #[test]
    fn test_plan_loop_creates_lanes_and_preserves_followup_context() {
        let fixture = Fixture::new();
        let profile = json!({"engine": "codex", "model": "planner-model"});
        save(&json!({"aliases": {"planner": profile}, "planner": "planner"})).unwrap();
        let mut host = Mock {
            inputs: ["", "Add a menu", "Make that keyboard accessible", "/exit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..Default::default()
        };
        let (code, output) = main_of(&["--plan", "-p", &fixture.project()], &mut host);
        assert_eq!(code, 0);
        // A new project gets its first session's tickets folder, .donut/<session>/.
        let sessions: Vec<PathBuf> = std::fs::read_dir(fixture.root.join(".donut"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.join("to_do").is_dir())
            .collect();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        for lane in ["to_do", "in_progress", "done"] {
            assert!(session.join(lane).is_dir());
        }
        assert!(!fixture.root.join("swim_lanes").exists());
        assert!(fixture.root.join("skills/donut-planner/SKILL.md").is_file());
        assert_eq!(host.agent_calls.len(), 2);
        let second = &host.agent_calls[1].1;
        assert!(second.contains("Add a menu"));
        assert!(second.contains("Make that keyboard accessible"));
        assert!(second.contains("001.log"));
        assert_ne!(host.agent_calls[0].2, host.agent_calls[1].2);
        assert!(host.agent_calls[1].3); // The planner view, not a summary.
        assert_eq!(output.matches("PLANNING COMPLETE").count(), 2);
        let name = session.file_name().unwrap().to_string_lossy();
        assert_eq!(
            output
                .matches(&format!(
                    "Try exiting the shell and running `donut --work next 1 --session {name}`"
                ))
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_interactive_planning_hides_thinking_and_lists_created_tickets() {
        let fixture = Fixture::new();
        save(&json!({"aliases": {"planner": {"engine": "codex"}}, "planner": "planner"})).unwrap();
        let reasoning = json!({"type": "item.completed", "item": {"type": "reasoning", "text": "Checking menu scope"}});
        let message = json!({"type": "item.completed", "item": {"type": "agent_message", "text": "Created the menu ticket"}});
        let script = format!(
            "prompt=$(cat); case \"$prompt\" in *.donut/first/to_do/*) ;; *) exit 9;; esac; \
             printf '**Slug:** `menu`\\n**Depends on:** None\\n' > .donut/first/to_do/001_menu.md; \
             printf '%s\\n' '{reasoning}'; printf '%s\\n' '{message}'"
        );
        let mut host = Mock {
            inputs: ["Plan a menu", "/exit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            launcher: Some(script),
            ..Default::default()
        };
        let (code, output) = main_of(
            &["--plan", "-p", &fixture.project(), "--session", "first"],
            &mut host,
        );
        assert_eq!(code, 0, "{output}");
        assert!(
            fixture
                .root
                .join(".donut/first/to_do/001_menu.md")
                .is_file()
        );
        assert!(!output.contains("Checking menu scope"));
        let logs: Vec<PathBuf> = std::fs::read_dir(fixture.root.join("state/donut"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path().join("001.log"))
            .filter(|path| path.is_file())
            .collect();
        assert_eq!(logs.len(), 1);
        assert!(
            std::fs::read_to_string(&logs[0])
                .unwrap()
                .contains("Checking menu scope")
        );
        assert!(output.contains("Tickets created: 1"));
        assert!(output.contains(".donut/first/to_do/001_menu.md"));
        assert!(output.contains("Created the menu ticket"));
        assert!(output.contains("PLANNING COMPLETE"));
    }

    #[test]
    fn test_new_modes_select_defaults_and_explicit_overrides() {
        let fixture = Fixture::new();
        let profiles = json!({
            "planner": {"engine": "codex", "model": "planning"},
            "worker": {"engine": "codex", "model": "working"},
            "override": {"engine": "claude", "model": "chosen"},
        });
        save(&json!({"aliases": profiles, "planner": "planner", "default_model": "worker"}))
            .unwrap();
        let cases: [(&[&str], &str, &str); 9] = [
            (&["--plan"], "planner", "plan"),
            (&["--plan", "-m", "override"], "override", "plan"),
            (&["--model", "override", "--plan"], "override", "plan"),
            (&["--work"], "worker", "work"),
            (&["--work", "next", "1"], "worker", "work"),
            (
                &["--work", "next", "3", "-m", "override"],
                "override",
                "work",
            ),
            (
                &["-m", "override", "--work", "next", "all"],
                "override",
                "work",
            ),
            (&["--work", "3", "--model", "override"], "override", "work"),
            (&["-m", "override", "--work", "1"], "override", "work"),
        ];
        for (args, alias, mode) in cases {
            let mut all: Vec<&str> = args.to_vec();
            let project = fixture.project();
            all.extend(["-p", &project]);
            let mut host = Mock {
                mock_batch: true,
                mock_plan: true,
                ..Default::default()
            };
            let (code, _) = main_of(&all, &mut host);
            assert_eq!(code, 0, "{args:?}");
            // A worker with no effort chosen runs at low effort; the planner keeps its provider default.
            let mut expected = profiles[alias].clone();
            if mode == "work" {
                expected["effort"] = json!("low");
                assert_eq!(host.batches[0].0, expected, "{args:?}");
                if args == ["--work"] {
                    assert_eq!(host.batches[0].2, "all");
                }
                if let Some(at) = args.iter().position(|a| *a == "next") {
                    assert_eq!(host.batches[0].2, args[at + 1]);
                }
            } else {
                assert_eq!(host.plans[0].0, expected, "{args:?}");
            }
        }
    }

    #[test]
    fn test_planning_failure_does_not_print_completion() {
        let fixture = Fixture::new();
        save(&json!({"aliases": {"planner": {"engine": "codex"}}, "planner": "planner"})).unwrap();
        let mut host = Mock {
            mock_install: true,
            agent_code: 7,
            ..Default::default()
        };
        let (code, output) = main_of(
            &["--plan", "Add a menu", "-p", &fixture.project()],
            &mut host,
        );
        assert_eq!(code, 7);
        assert!(output.contains("PLANNING FAILED"));
        assert!(!output.contains("PLANNING COMPLETE"));
    }

    #[test]
    fn test_interactive_eof_starts_no_model() {
        let fixture = Fixture::new();
        save(&json!({"aliases": {"planner": {"engine": "codex"}}, "planner": "planner"})).unwrap();
        let mut host = Mock {
            mock_install: true,
            ..Default::default()
        };
        let (code, _) = main_of(&["--plan", "-p", &fixture.project()], &mut host);
        assert_eq!(code, 0);
        assert!(host.agent_calls.is_empty());
    }

    #[test]
    fn test_invalid_next_counts_do_not_launch_worker() {
        let _fixture = Fixture::new();
        for values in [
            vec!["next"],
            vec!["next", "0"],
            vec!["next", "-1"],
            vec!["next", "many"],
            vec!["next", "1", "2"],
        ] {
            let mut host = Mock::default();
            let mut args = vec!["--work"];
            args.extend(values.iter().copied());
            let (code, _) = main_of(&args, &mut host);
            assert_eq!(code, 2, "{values:?}");
            assert!(host.batches.is_empty());
        }
    }

    #[test]
    fn test_conflicting_modes_rejected() {
        let _fixture = Fixture::new();
        for args in [
            vec!["--plan", "--work"],
            vec!["--plan", "hello", "models"],
            vec!["--work", "2", "setup"],
        ] {
            let mut host = Mock::default();
            let (code, _) = main_of(&args, &mut host);
            assert_eq!(code, 2, "{args:?}");
        }
    }

    #[test]
    fn test_unknown_alias_does_not_launch_agent() {
        let _fixture = Fixture::new();
        let mut host = Mock::default();
        let (code, _) = main_of(&["-m", "missing", "do", "next", "1"], &mut host);
        assert_eq!(code, 1);
        assert!(host.batches.is_empty());
    }

    #[test]
    fn test_invalid_profile_does_not_replace_config() {
        let _fixture = Fixture::new();
        let original = std::fs::read(config_path(None)).unwrap();
        assert!(
            save(&json!({"aliases": {"bad alias": {"engine": "codex", "model": ""}}})).is_err()
        );
        assert_eq!(std::fs::read(config_path(None)).unwrap(), original);
        let saved: Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(saved["aliases"]["worker"]["engine"], "codex");
    }

    #[test]
    fn test_malformed_dependency_fails_closed_without_stopping_the_queue() {
        let fixture = Fixture::new();
        let lane = fixture.root.join("swim_lanes").join("to_do");
        std::fs::create_dir_all(&lane).unwrap();
        let task = lane.join("001_task.md");
        std::fs::write(
            &task,
            "**Slug:** `task`\n**Depends on:** ambiguous freeform dependency\n",
        )
        .unwrap();
        let ticket = Ticket::new(&task, "to_do");
        assert_eq!(ticket.dependencies, ["ambiguous", "freeform", "dependency"]);
        let report = queue_report(&fixture.root, &std::collections::HashSet::new(), None);
        assert_eq!(report.len(), 1);
        assert!(report[0].contains("unknown slug `ambiguous`"));
    }

    #[test]
    fn test_missing_project_fails_without_creating_it() {
        let fixture = Fixture::new();
        let target = fixture.root.join("missing");
        let mut host = Mock::default();
        let (code, _) = main_of(&["init", "-p", &target.to_string_lossy()], &mut host);
        assert_eq!(code, 1);
        assert!(!target.exists());
    }
}
