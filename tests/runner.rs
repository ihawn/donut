//! End-to-end runner tests: the `donut` binary runs tickets with a stand-in provider.
//!
//! The stand-in `qwen` and `claude` are shell scripts that start this test executable again, which runs
//! `fake_provider_entry` (the Python version of the stand-in was a script too). They need a POSIX shell, so the
//! whole file is Unix only.
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regex::Regex;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------------------------
// The stand-in provider.

fn fake_provider() -> i32 {
    let env = |key: &str| std::env::var(key).ok();
    if let Some(code) = env("FAKE_EXIT") {
        return code.parse().unwrap_or(1);
    }
    let file = env("DONUT_FAKE_ARGS").expect("the stand-in gets its arguments in a file");
    let raw = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_file(&file);
    let mut parts: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    parts.pop(); // Every argument ends in NUL.
    let mut args: Vec<String> = parts
        .iter()
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt).unwrap();
    if let Some(marker) = env("FAKE_FAIL_ONCE") {
        let marker = PathBuf::from(marker);
        if !marker.exists() {
            std::fs::write(marker, "").unwrap();
            return 7;
        }
    }
    match args.iter().position(|arg| arg == "--prompt") {
        Some(at) => args[at + 1] = prompt.clone(),
        None => args.extend(["--prompt".to_string(), prompt.clone()]), // Claude reads its prompt from stdin alone.
    }
    args.extend([
        "--tool-search".to_string(),
        env("ENABLE_TOOL_SEARCH").unwrap_or_default(),
    ]);
    let mut calls = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env("CALLS").unwrap())
        .unwrap();
    calls
        .write_all(format!("{}\n", serde_json::to_string(&args).unwrap()).as_bytes())
        .unwrap();
    let no_tools = args
        .windows(2)
        .any(|pair| pair[0] == "--tools" && pair[1].is_empty());
    if args.iter().any(|arg| arg == "--max-tool-calls") || no_tools {
        println!("| Task | Status | What changed | Tests | Human check |");
        return 0;
    }
    if prompt.contains("You are the planner, recovering") {
        let ticket = PathBuf::from(
            &Regex::new(r"Blocked ticket: (\S+)")
                .unwrap()
                .captures(&prompt)
                .unwrap()[1],
        );
        let text = std::fs::read_to_string(&ticket).unwrap();
        if text.contains("needs-human") {
            println!("RECOVERY: NEEDS HUMAN choose the chimney width");
        } else {
            let text = text.replace("MARKER: block-me", "MARKER: fixed").replacen(
                "## Completion notes",
                "## Addendum 1: planner recovery\n\nUse a 0.3u chimney.\n\n## Completion notes",
                1,
            );
            std::fs::write(&ticket, text).unwrap();
            println!("RECOVERY: REQUEUED corrected the chimney width");
        }
        return 0;
    }
    if prompt.contains("repairing the ticket queue") {
        let depends = Regex::new(r"\*\*Depends on:\*\*.*").unwrap();
        let mut folders = vec![PathBuf::from("swim_lanes")];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(&folder).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    folders.push(path);
                } else if path.extension().is_some_and(|ext| ext == "md") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    if text.contains("BuildAnnex.cs") {
                        std::fs::write(
                            &path,
                            depends.replace_all(&text, "**Depends on:** None").as_ref(),
                        )
                        .unwrap();
                    }
                }
            }
        }
        println!("REPAIR: DONE fixed one Depends on line");
        return 0;
    }
    if prompt.contains("You are the reviewer") {
        let path = &Regex::new(r"Ticket file: (\S+)")
            .unwrap()
            .captures(&prompt)
            .unwrap()[1];
        let ticket = std::fs::read_to_string(path).unwrap();
        if ticket.contains("always-reject")
            || (ticket.contains("reject-once") && !ticket.contains("Addendum 1: review feedback"))
        {
            println!("Looked at the change.");
            println!("VERDICT: CHANGES REQUESTED");
            println!("1. Add the missing jump test.");
        } else {
            println!("VERDICT: APPROVED");
        }
        return 0;
    }
    let named = Regex::new(r"ONE named task: (.*?)\. Read this").unwrap();
    let path = PathBuf::from(&named.captures(&prompt).unwrap()[1]);
    let text = std::fs::read_to_string(&path).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    // The fake worker writes each file its ticket names with WRITE:, so a ticket changes code.
    for found in Regex::new(r"WRITE: (\S+)").unwrap().captures_iter(&text) {
        std::fs::write(&found[1], &name).unwrap();
    }
    let blocked = text.contains("block-me") && !text.contains("MARKER: fixed");
    let lane = if blocked { "blocked" } else { "done" };
    let new = path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(lane)
        .join(&name);
    std::fs::rename(&path, &new).unwrap();
    let mut file = std::fs::OpenOptions::new().append(true).open(&new).unwrap();
    let notes = if blocked {
        "BLOCKED: test failure"
    } else {
        "Changed fixture. Tests: passed."
    };
    write!(file, "\n## Completion notes\n{notes}").unwrap();
    println!("finished {name}");
    0
}

/// Run by the stand-in scripts, in a process of its own. A normal test run skips it.
#[test]
fn fake_provider_entry() {
    if std::env::var_os("DONUT_FAKE_PROVIDER").is_none() {
        return;
    }
    let code = fake_provider();
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

// ---------------------------------------------------------------------------------------------------------------
// The fixture.

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    calls: PathBuf,
    env: Vec<(String, String)>,
}

#[derive(Default)]
struct Run<'a> {
    old_syntax: bool,
    fallback: Option<&'a str>,
    more: Option<Value>,
}

impl Fixture {
    fn new() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project with spaces");
        for lane in ["to_do", "in_progress", "blocked", "done"] {
            std::fs::create_dir_all(project.join("swim_lanes").join(lane)).unwrap();
        }
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let script = format!(
            "#!/bin/sh\nargs=\"${{TMPDIR:-/tmp}}/donut-fake-args.$$\"\nprintf '%s\\0' \"$@\" > \"$args\"\n\
             DONUT_FAKE_PROVIDER=1 DONUT_FAKE_ARGS=\"$args\" exec '{}' --exact fake_provider_entry --nocapture\n",
            std::env::current_exe().unwrap().display()
        );
        for name in ["qwen", "claude"] {
            let path = bin.join(name);
            std::fs::write(&path, &script).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let calls = root.join("calls");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let env = [
            ("PATH", path),
            ("CALLS", calls.to_string_lossy().into_owned()),
            ("HOME", root.to_string_lossy().into_owned()),
            (
                "XDG_CONFIG_HOME",
                root.join("config").to_string_lossy().into_owned(),
            ),
            (
                "XDG_STATE_HOME",
                root.join("state").to_string_lossy().into_owned(),
            ),
            ("TEST_DONUT_KEY", "fixture".to_string()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
        Fixture {
            _temp: temp,
            root,
            project,
            calls,
            env,
        }
    }

    fn set_env(&mut self, key: &str, value: &str) {
        self.env.retain(|(known, _)| known != key);
        self.env.push((key.to_string(), value.to_string()));
    }

    fn ticket(&self, number: u32, slug: &str, deps: &str, lane: &str) -> PathBuf {
        let path = self
            .project
            .join("swim_lanes")
            .join(lane)
            .join(format!("{number:03}_{slug}.md"));
        std::fs::write(
            &path,
            format!("**Slug:** `{slug}`\n**Depends on:** {deps}\n"),
        )
        .unwrap();
        path
    }

    fn append(&self, path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn write(&self, number: u32, slug: &str, body: &str, deps: &str, lane: &str) -> PathBuf {
        let path = self
            .project
            .join("swim_lanes")
            .join(lane)
            .join(format!("{number:03}_{slug}.md"));
        std::fs::write(
            &path,
            format!("**Slug:** `{slug}`\n**Depends on:** {deps}\n\n{body}\n"),
        )
        .unwrap();
        path
    }

    fn run_batch(&self, count: &str, extra: &[&str], options: Run<'_>) -> Output {
        let config = self.root.join("config/donut");
        std::fs::create_dir_all(&config).unwrap();
        let alias = json!({
            "engine": "openai", "model": "test", "base_url": "https://example.com/v1",
            "key_env": "TEST_DONUT_KEY", "auto_approve": true,
        });
        let mut aliases = json!({"test": alias});
        if let Some(fallback) = options.fallback {
            let mut main = alias.clone();
            main["fallback"] = json!(fallback);
            let mut backup = alias.clone();
            backup["model"] = json!(fallback);
            aliases = json!({"test": main, fallback: backup});
        }
        for (name, profile) in options
            .more
            .iter()
            .flat_map(|more| more.as_object().unwrap())
        {
            aliases[name] = profile.clone();
        }
        std::fs::write(
            config.join("config.json"),
            json!({"aliases": aliases}).to_string(),
        )
        .unwrap();
        let mode = if options.old_syntax {
            ["do", "next", count]
        } else {
            ["--work", "next", count]
        };
        Command::new(env!("CARGO_BIN_EXE_donut"))
            .args(["--model", "test"])
            .args(mode)
            .args(["-p", &self.project.to_string_lossy()])
            .args(extra)
            .env_remove("DONUT_PROFILE")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap()
    }

    /// run_batch in the new syntax, the common case.
    fn work(&self, count: &str, extra: &[&str]) -> Output {
        self.run_batch(count, extra, Run::default())
    }

    fn old(&self, count: &str, extra: &[&str]) -> Output {
        self.run_batch(
            count,
            extra,
            Run {
                old_syntax: true,
                ..Default::default()
            },
        )
    }

    fn tasks(&self, lane: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.project.join("swim_lanes").join(lane))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.retain(|name| name.ends_with(".md"));
        names.sort();
        names
    }

    fn runs(&self) -> Vec<PathBuf> {
        let mut runs: Vec<PathBuf> = std::fs::read_dir(self.root.join("state/donut"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("run-")
            })
            .collect();
        runs.sort();
        runs
    }

    fn results(&self) -> Vec<Value> {
        let runs = self.runs();
        serde_json::from_str(
            &std::fs::read_to_string(runs.last().unwrap().join("results.json")).unwrap(),
        )
        .unwrap()
    }

    fn calls_made(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.calls)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn worker_calls(&self) -> Vec<Vec<String>> {
        self.calls_made()
            .into_iter()
            .filter(|call| prompt(call).contains("ONE named task"))
            .collect()
    }

    fn git_project(&mut self) {
        let none = self
            .root
            .join("no-gitconfig")
            .to_string_lossy()
            .into_owned();
        self.set_env("GIT_CONFIG_GLOBAL", &none);
        self.set_env("GIT_CONFIG_SYSTEM", &none);
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&self.project)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.project)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn code_ticket(&self, number: u32, slug: &str, listed: &str, written: &str, deps: &str) {
        let path = self.ticket(number, slug, deps, "to_do");
        self.append(
            &path,
            &format!("\n## Files\n- `{listed}`\n\nWRITE: {written}\n"),
        );
    }
}

fn prompt(call: &[String]) -> String {
    let at = call.iter().position(|arg| arg == "--prompt").unwrap();
    call[at + 1].clone()
}

fn after(call: &[String], flag: &str) -> String {
    let at = call.iter().position(|arg| arg == flag).unwrap();
    call[at + 1].clone()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn shown(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap()
}

// ---------------------------------------------------------------------------------------------------------------
// The tests.

#[test]
fn test_work_next_one_executes_exactly_one_ticket() {
    let f = Fixture::new();
    for number in 1..4 {
        f.ticket(number, &format!("task{number}"), "None", "to_do");
    }
    let result = f.work("1", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert_eq!(f.tasks("done"), ["001_task1.md"]);
    assert_eq!(f.tasks("to_do").len(), 2);
    assert_eq!(f.calls_made().len(), 2); // One worker plus summary.
}

#[test]
fn test_a_session_works_only_its_own_tickets_folder() {
    let f = Fixture::new();
    f.ticket(1, "old", "None", "to_do"); // swim_lanes/, from before sessions.
    let lanes = donut::workspace::create(&f.project, Some("s1")).unwrap();
    std::fs::write(
        lanes.join("to_do/001_new.md"),
        "**Slug:** `new`\n**Depends on:** None\n",
    )
    .unwrap();
    let result = f.work("all", &["--session", "s1"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert!(stdout(&result).contains("Tickets: .donut/s1/"));
    assert!(lanes.join("done/001_new.md").exists());
    assert_eq!(f.tasks("to_do").len(), 1); // swim_lanes/ is left alone.
    let worker = f.calls_made()[0].clone();
    let text = prompt(&worker);
    assert!(text.contains(".donut/s1/on_hold/"));
    assert!(!text.contains("swim_lanes"));
    // Without --session, a command-line run uses the latest session.
    std::fs::write(
        lanes.join("to_do/002_more.md"),
        "**Slug:** `more`\n**Depends on:** None\n",
    )
    .unwrap();
    let result = f.work("1", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert_eq!(
        std::fs::read_dir(lanes.join("done"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
            .count(),
        2
    );
}

#[test]
fn test_count_and_unique_sessions() {
    let f = Fixture::new();
    for n in 1..4 {
        f.ticket(n, &format!("task{n}"), "None", "to_do");
    }
    let result = f.old("2", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert_eq!(f.tasks("done").len(), 2);
    let calls = f.calls_made();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .filter(|a| a.contains(&"--prompt".to_string()))
            .count(),
        3
    );
    assert!(
        calls
            .iter()
            .all(|a| !a.contains(&"--resume".to_string()) && !a.contains(&"--continue".to_string()))
    );
    assert!(calls[2].contains(&"--max-tool-calls".to_string()));
    let summary = prompt(&calls[2]);
    assert!(!summary.contains("003_task3"));
    // The final report is the table, then an overview, the pull requests and the order to deploy in.
    for heading in [
        "Task | Result | Note | PR",
        "OVERVIEW:",
        "PULL REQUESTS:",
        "DEPLOY ORDER:",
    ] {
        assert!(summary.contains(heading), "{heading}");
    }
    assert!(summary.contains("do not guess"));
}

#[test]
fn test_all_skips_blocked_dependencies() {
    let f = Fixture::new();
    f.ticket(1, "block-me", "None", "to_do");
    f.ticket(2, "dependent", "`block-me`", "to_do");
    f.ticket(3, "independent", "None", "to_do");
    let result = f.old("all", &[]);
    assert_eq!(code(&result), 1, "{}", shown(&result));
    assert_eq!(f.tasks("done"), ["003_independent.md"]);
    assert_eq!(f.tasks("blocked").len(), 1);
    assert_eq!(f.tasks("to_do").len(), 1);
}

#[test]
fn test_on_hold_and_dependent_work_are_skipped() {
    let f = Fixture::new();
    std::fs::create_dir(f.project.join("swim_lanes/on_hold")).unwrap();
    let held = f.ticket(1, "held", "None", "on_hold");
    let before = std::fs::read(&held).unwrap();
    f.ticket(2, "dependent", "`held`", "to_do");
    f.ticket(3, "independent", "None", "to_do");
    let result = f.work("all", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert_eq!(f.tasks("done"), ["003_independent.md"]);
    assert_eq!(std::fs::read(&held).unwrap(), before);
    assert!(f.project.join("swim_lanes/to_do/002_dependent.md").exists());
}

#[test]
fn test_blocked_tickets_and_their_dependents_stay_untouched() {
    let f = Fixture::new();
    let blocked = f.ticket(1, "blocked-work", "None", "blocked");
    let before = std::fs::read(&blocked).unwrap();
    f.ticket(2, "dependent", "`blocked-work`", "to_do");
    f.ticket(3, "independent", "None", "to_do");
    let result = f.work("all", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert_eq!(std::fs::read(&blocked).unwrap(), before);
    assert_eq!(f.tasks("done"), ["003_independent.md"]);
}

#[test]
fn test_invalid_count_and_empty() {
    let f = Fixture::new();
    assert_eq!(code(&f.old("0", &[])), 1);
    assert_eq!(code(&f.old("all", &[])), 0);
    assert!(!f.calls.exists());
}

#[test]
fn test_process_failure_stops_batch() {
    let mut f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    f.ticket(2, "second", "None", "to_do");
    f.set_env("FAKE_EXIT", "7");
    let result = f.old("all", &[]);
    assert_eq!(code(&result), 1);
    assert!(!stdout(&result).contains("[2/all]"));
    assert_eq!(f.tasks("to_do").len(), 2);
}

#[test]
fn test_bounded_summary_excludes_unattempted_dependencies() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    f.ticket(2, "waiting", "`missing-task`", "to_do");
    let result = f.old("1", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    let calls = f.calls_made();
    let summary = prompt(calls.last().unwrap());
    assert!(summary.contains("001_first"));
    assert!(!summary.contains("waiting"));
    assert!(!summary.contains("missing-task"));
}

#[test]
fn test_verify_command_failure_moves_ticket_to_blocked() {
    let f = Fixture::new();
    let path = f.ticket(1, "checked", "None", "to_do");
    f.append(&path, "**Verify:** `exit 3`\n");
    f.ticket(2, "dependent", "`checked`", "to_do");
    let path = f.ticket(3, "passing", "None", "to_do");
    f.append(&path, "**Verify:** `exit 0`\n");
    let result = f.work("all", &[]);
    assert_eq!(code(&result), 1, "{}", shown(&result));
    assert_eq!(f.tasks("done"), ["003_passing.md"]);
    assert_eq!(f.tasks("blocked"), ["001_checked.md"]);
    let blocked = read(f.project.join("swim_lanes/blocked/001_checked.md"));
    assert!(blocked.contains("BLOCKED: Donut ran the Verify command and it failed (exit 3)"));
    assert!(f.project.join("swim_lanes/to_do/002_dependent.md").exists());
    let out = stdout(&result);
    assert!(out.contains("Verify failed (exit 3)"));
    assert!(out.contains(
        "Queue: to_do/002_dependent.md: depends on `checked`, which is on hold or blocked"
    ));
    let verify: Vec<String> = f
        .results()
        .iter()
        .map(|r| r["verify"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(verify, ["failed (exit 3)", "passed"]);
    assert!(f.runs().last().unwrap().join("001.verify.log").exists());
}

#[test]
fn test_a_failed_verify_goes_back_to_the_worker_before_it_blocks() {
    let f = Fixture::new();
    let path = f.ticket(1, "checked", "None", "to_do");
    // The pattern is split so the Verify line itself does not hold the words it looks for.
    f.append(
        &path,
        "**Verify:** grep -q 'veri''fy failure' swim_lanes/done/001_checked.md || exit 3\n",
    );
    let result = f.work("all", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert!(stdout(&result).contains("Verify: sending the failure back to the worker (fix 1/2)"));
    let done = read(f.project.join("swim_lanes/done/001_checked.md"));
    assert!(done.contains("## Addendum 1: verify failure"));
    assert!(done.contains("(exit 3)"));
    let record = &f.results()[0];
    assert_eq!(
        (
            record["status"].as_str(),
            record["verify"].as_str(),
            record["verify_fixes"].as_i64()
        ),
        (Some("Done"), Some("passed"), Some(1))
    );
    let workers = f.worker_calls();
    assert_eq!(workers.len(), 2);
    let second = prompt(&workers[1]);
    assert!(second.contains("Addendum N: verify failure"));
    assert!(second.contains("swim_lanes/in_progress/001_checked.md"));
}

#[test]
fn test_verify_fixes_zero_blocks_at_the_first_failure() {
    let f = Fixture::new();
    let path = f.ticket(1, "checked", "None", "to_do");
    f.append(&path, "**Verify:** `exit 3`\n");
    let result = f.work("all", &["--verify-fixes", "0"]);
    assert_eq!(code(&result), 1);
    assert_eq!(f.calls_made().len(), 2); // One worker plus the summary.
    assert!(f.project.join("swim_lanes/blocked/001_checked.md").exists());
}

#[test]
fn test_the_fallback_alias_takes_over_when_the_worker_fails() {
    let mut f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    f.ticket(2, "second", "None", "to_do");
    let marker = f.root.join("failed-once").to_string_lossy().into_owned();
    f.set_env("FAKE_FAIL_ONCE", &marker); // Like a provider that is down, the first time.
    let result = f.run_batch(
        "all",
        &[],
        Run {
            fallback: Some("backup"),
            ..Default::default()
        },
    );
    assert_eq!(code(&result), 0, "{}", shown(&result));
    let out = stdout(&result);
    assert!(out.contains("Fallback: backup takes over if the worker fails"));
    assert!(out.contains("Worker failed (exit 7); backup takes over for the rest of the run."));
    assert_eq!(f.tasks("done").len(), 2);
    let records = f.results();
    assert_eq!(records[0]["fallback"], "backup");
    let models = |record: &Value| -> Vec<String> {
        record["worker_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["model"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(models(&records[0]), ["test", "backup"]);
    assert_eq!(models(&records[1]), ["backup"]); // It stays for the run.
}

#[test]
fn test_an_unknown_fallback_alias_is_refused() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    let result = f.work("all", &["--fallback", "nobody"]);
    assert_eq!(code(&result), 1);
    assert!(stderr(&result).contains("Unknown fallback alias: nobody"));
}

#[test]
fn test_parallel_tickets_run_in_worktrees_and_come_back_as_commits() {
    let mut f = Fixture::new();
    f.git_project();
    f.code_ticket(1, "a", "a.txt", "a.txt", "None");
    f.code_ticket(2, "b", "b.txt", "b.txt", "None");
    f.code_ticket(3, "c", "c.txt", "c.txt", "`a`");
    let result = f.work("all", &["--parallel", "2", "--review", "test", "--no-pr"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert!(stdout(&result).contains("Working 2 tickets in parallel"));
    let verdicts: Vec<String> = f
        .results()
        .iter()
        .map(|r| r["review"]["verdict"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(verdicts, ["approved"; 3]);
    assert_eq!(f.tasks("done").len(), 3);
    assert_eq!(f.tasks("to_do").len(), 0);
    for (name, ticket) in [
        ("a.txt", "001_a.md"),
        ("b.txt", "002_b.md"),
        ("c.txt", "003_c.md"),
    ] {
        assert_eq!(read(f.project.join(name)), ticket);
    }
    let log: Vec<String> = f
        .git(&["log", "--format=%s"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        log,
        [
            "donut: done 003_c.md",
            "donut: done 002_b.md",
            "donut: done 001_a.md",
            "donut: checkpoint before parallel tickets",
        ]
    );
    let shown_files: Vec<String> = f
        .git(&["show", "--name-only", "--format=", "HEAD~1"])
        .split_whitespace()
        .map(str::to_string)
        .collect();
    assert_eq!(shown_files, ["b.txt", "swim_lanes/done/002_b.md"]);
    assert_eq!(f.git(&["status", "--porcelain"]), "");
    assert_eq!(f.git(&["worktree", "list"]).lines().count(), 1); // The worktrees are removed.
    let statuses: Vec<String> = f
        .results()
        .iter()
        .map(|r| r["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["Done", "Done", "Done"]);
    assert!(read(f.runs().last().unwrap().join("001.console.log")).contains("finished 001_a.md"));
}

#[test]
fn test_a_parallel_ticket_that_does_not_merge_runs_again_alone() {
    let mut f = Fixture::new();
    f.git_project();
    f.code_ticket(1, "a", "a.txt", "shared.txt", "None");
    f.code_ticket(2, "b", "b.txt", "shared.txt", "None");
    let result = f.work("all", &["--parallel", "2"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert!(
        stdout(&result)
            .contains("002_b.md: its changes do not apply beside the others; it runs again alone.")
    );
    assert_eq!(f.tasks("done").len(), 2);
    assert_eq!(read(f.project.join("shared.txt")), "002_b.md");
    let statuses: Vec<String> = f
        .results()
        .iter()
        .map(|r| r["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["Done", "Requeued", "Done"]);
}

#[test]
fn test_tickets_with_overlapping_or_no_files_do_not_share_a_wave() {
    let mut f = Fixture::new();
    f.git_project();
    f.code_ticket(1, "a", "src/", "a.txt", "None");
    f.code_ticket(2, "b", "src/b.txt", "b.txt", "None");
    f.ticket(3, "unlisted", "None", "to_do");
    let result = f.work("all", &["--parallel", "3"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert!(!stdout(&result).contains("in parallel;"));
    assert_eq!(f.tasks("done").len(), 3);
}

#[test]
fn test_parallel_outside_one_git_repository_is_refused_before_any_work() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    let result = f.work("all", &["--parallel", "2"]);
    assert_eq!(code(&result), 1);
    assert!(stderr(&result).contains("must be the top folder of one git repository"));
    assert!(!f.calls.exists());
}

#[test]
fn test_checkpoint_commits_after_each_ticket() {
    let mut f = Fixture::new();
    f.git_project();
    f.ticket(1, "first", "None", "to_do");
    f.ticket(2, "second", "None", "to_do");
    let result = f.work("all", &["--checkpoint"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    let log: Vec<String> = f
        .git(&["log", "--format=%s"])
        .split_whitespace()
        .map(str::to_string)
        .collect();
    assert_eq!(
        log,
        [
            "donut:",
            "done",
            "002_second.md",
            "donut:",
            "done",
            "001_first.md"
        ]
    );
    assert_eq!(f.git(&["status", "--porcelain"]), "");
    let records = f.results();
    assert!(
        records
            .iter()
            .all(|r| !r["checkpoint"].is_null() && r["checkpoint"] != "")
    );
    assert_eq!(
        records[0]["changed_files"],
        json!(["swim_lanes/done/001_first.md"])
    );
}

#[test]
fn test_checkpoint_outside_git_is_refused_before_any_work() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    let result = f.work("all", &["--checkpoint"]);
    assert_eq!(code(&result), 1);
    assert!(stderr(&result).contains("not a git repository"));
    assert!(!f.calls.exists());
}

#[test]
fn test_dependency_order() {
    let f = Fixture::new();
    f.ticket(1, "dependent", "`parent`", "to_do");
    f.ticket(2, "parent", "None", "to_do");
    let result = f.old("all", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    let out = stdout(&result);
    assert!(out.find("002_parent.md").unwrap() < out.find("001_dependent.md").unwrap());
}

#[test]
fn test_unticked_acceptance_checks_make_a_ticket_partial_and_decisions_are_listed() {
    let f = Fixture::new();
    let gaps = "\n## Acceptance criteria\n\n- [x] Unit tests pass.\n- [ ] Live check passes.\n- [ ] **Human check**: ok.\n";
    let path = f.ticket(1, "half", "None", "to_do");
    f.append(&path, gaps);
    f.ticket(2, "whole", "None", "to_do");
    let waiting = f.ticket(3, "waiting", "None", "blocked");
    f.append(
        &waiting,
        "\n## Completion notes\nNEEDS HUMAN: allow a second login today?\n",
    );
    let result = f.old("all", &[]);
    assert_eq!(code(&result), 0, "{}", shown(&result)); // Partial is not a failure.
    let results = f.results();
    let pairs: Vec<(String, String)> = results
        .iter()
        .map(|r| {
            (
                r["task"].as_str().unwrap()[..3].to_string(),
                r["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        [
            ("001".to_string(), "Partial".to_string()),
            ("002".to_string(), "Done".to_string())
        ]
    );
    assert_eq!(results[0]["unticked"], json!(["Live check passes."]));
    let out = stdout(&result);
    assert!(out.contains("Partial: 1 acceptance check not ticked: Live check passes."));
    assert!(out.contains("Needs your decision:"));
    assert!(out.contains("003  allow a second login today?"));
}

#[test]
fn test_worker_auto_moves_up_a_model_on_each_review_that_requests_changes() {
    let f = Fixture::new();
    f.ticket(1, "reject-once", "None", "to_do");
    f.ticket(2, "always-reject", "None", "to_do");
    let result = f.run_batch(
        "all",
        &["--model", "auto", "--review", "test"],
        Run {
            more: Some(json!({"claude": {"engine": "claude"}})),
            ..Default::default()
        },
    );
    assert_eq!(code(&result), 1, "{}", shown(&result)); // The second ticket blocks.
    let out = stdout(&result);
    assert!(out.contains("Auto: haiku (max) → sonnet (low) → opus (low)"));
    assert!(out.contains("Auto: sonnet (low) takes the rework"));
    let steps: Vec<(String, String)> = f
        .worker_calls()
        .iter()
        .map(|c| (after(c, "--model"), after(c, "--effort")))
        .collect();
    let step = |model: &str, effort: &str| (model.to_string(), effort.to_string());
    let (haiku, sonnet, opus) = (
        step("haiku", "max"),
        step("sonnet", "low"),
        step("opus", "low"),
    );
    // Each ticket starts again on haiku.
    assert_eq!(steps, [haiku.clone(), sonnet.clone(), haiku, sonnet, opus]);
    assert_eq!(f.tasks("done").len(), 1);
    let rounds: Vec<i64> = f
        .results()
        .iter()
        .map(|r| r["review"]["rounds"].as_i64().unwrap())
        .collect();
    assert_eq!(rounds, [2, 3]); // One review per step.
    assert!(f.calls_made().iter().all(|c| c.last().unwrap() == "true")); // Tool search for every agent.
}

#[test]
fn test_a_worker_model_without_effort_thinks_low_and_an_explicit_effort_stays() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    f.ticket(2, "second", "None", "to_do");
    let more = || {
        Some(json!({
            "claude": {"engine": "claude", "model": "opus"},
            "deep": {"engine": "claude", "effort": "high"},
        }))
    };
    let first = f.run_batch(
        "1",
        &["--model", "claude"],
        Run {
            more: more(),
            ..Default::default()
        },
    );
    assert_eq!(code(&first), 0);
    let second = f.run_batch(
        "1",
        &["--model", "deep"],
        Run {
            more: more(),
            ..Default::default()
        },
    );
    assert_eq!(code(&second), 0);
    let efforts: Vec<String> = f
        .worker_calls()
        .iter()
        .map(|c| after(c, "--effort"))
        .collect();
    assert_eq!(efforts, ["low", "high"]);
}

#[test]
fn test_worker_auto_needs_a_claude_or_codex_alias() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    let result = f.work("all", &["--model", "auto"]);
    assert_eq!(code(&result), 1);
    assert!(stderr(&result).contains("Worker auto needs a Claude or Codex alias"));
}

#[test]
fn test_reviewer_approves_and_the_worker_moves_on() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    f.ticket(2, "second", "None", "to_do");
    let result = f.work("all", &["--review", "test"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert_eq!(f.tasks("done").len(), 2);
    let out = stdout(&result);
    assert!(out.contains("[review 1/3] 001_first.md"));
    assert!(out.contains("Review: approved"));
    let verdicts: Vec<String> = f
        .results()
        .iter()
        .map(|r| r["review"]["verdict"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(verdicts, ["approved", "approved"]);
    let reviews: Vec<Vec<String>> = f
        .calls_made()
        .into_iter()
        .filter(|c| prompt(c).contains("You are the reviewer"))
        .collect();
    assert_eq!(reviews.len(), 2);
    assert!(
        reviews
            .iter()
            .all(|c| after(c, "--approval-mode") == "plan")
    ); // Read-only.
}

#[test]
fn test_requested_changes_go_back_to_the_worker() {
    let f = Fixture::new();
    f.ticket(1, "reject-once", "None", "to_do");
    let result = f.work("all", &["--review", "test"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    let out = stdout(&result);
    assert!(out.contains("Review: changes requested; sending feedback to the worker (2/3)"));
    assert!(out.contains("[review 2/3] 001_reject-once.md"));
    let done = f.project.join("swim_lanes/done/001_reject-once.md");
    assert!(done.exists());
    let text = read(&done);
    assert!(text.contains("## Addendum 1: review feedback"));
    assert!(text.contains("1. Add the missing jump test."));
    // The addendum sits in the ticket body, before the completion notes.
    assert!(text.find("## Addendum 1").unwrap() < text.find("## Completion notes").unwrap());
    let prompts: Vec<String> = f.calls_made().iter().map(|c| prompt(c)).collect();
    let workers: Vec<&String> = prompts
        .iter()
        .filter(|p| p.contains("ONE named task"))
        .collect();
    assert_eq!(workers.len(), 2);
    assert!(workers[1].contains("This is a rework"));
    assert!(workers[1].contains("swim_lanes/in_progress/001_reject-once.md"));
    assert_eq!(f.results()[0]["review"]["rounds"], 2);
}

#[test]
fn test_ticket_is_blocked_when_the_reviewer_never_approves() {
    let f = Fixture::new();
    f.ticket(1, "always-reject", "None", "to_do");
    f.ticket(2, "dependent", "`always-reject`", "to_do");
    let result = f.work("all", &["--review", "test", "--review-rounds", "2"]);
    assert_eq!(code(&result), 1, "{}", shown(&result));
    let blocked = f.project.join("swim_lanes/blocked/001_always-reject.md");
    assert!(blocked.exists());
    assert!(read(&blocked).contains("BLOCKED: the reviewer did not approve after 2 reviews"));
    assert!(f.project.join("swim_lanes/to_do/002_dependent.md").exists());
    assert_eq!(f.results()[0]["status"], "Blocked");
}

#[test]
fn test_review_off_and_no_planner_mean_no_review() {
    let f = Fixture::new();
    f.ticket(1, "first", "None", "to_do");
    let result = f.work("all", &["--review", "off"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert!(!stdout(&result).contains("[review"));
    assert!(f.results()[0].get("review").is_none());
}

#[test]
fn test_planner_recovers_a_blocked_ticket_and_the_run_continues() {
    let f = Fixture::new();
    f.write(1, "chimney", "MARKER: block-me", "None", "to_do");
    f.ticket(2, "dependent", "`chimney`", "to_do");
    let result = f.work("all", &["--recover", "test"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    let out = stdout(&result);
    assert!(out.contains("[recover] 001_chimney.md"));
    assert!(out.contains("Recovery: ticket revised and requeued. corrected the chimney width"));
    assert_eq!(f.tasks("done"), ["001_chimney.md", "002_dependent.md"]);
    let text = read(f.project.join("swim_lanes/done/001_chimney.md"));
    assert!(
        text.find("## Addendum 1: planner recovery").unwrap()
            < text.find("## Completion notes").unwrap()
    );
    let records = f.results();
    let pairs: Vec<(String, String)> = records
        .iter()
        .take(2)
        .map(|r| {
            (
                r["task"].as_str().unwrap().to_string(),
                r["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        [
            ("001_chimney.md".to_string(), "Blocked".to_string()),
            ("001_chimney.md".to_string(), "Done".to_string())
        ]
    );
    // The run's records log, without any model call of their own, what each call ran on and why it blocked.
    let blocked = &records[0];
    assert_eq!(blocked["recovery"], "requeued");
    let models: Vec<&str> = blocked["worker_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["model"].as_str().unwrap())
        .collect();
    assert_eq!(models, ["test"]);
    assert_eq!(blocked["planner_calls"].as_array().unwrap().len(), 1);
    let run = f.runs().last().unwrap().clone();
    assert!(run.join("001.recover.log").exists());
    assert!(run.join("002.log").exists()); // The retry got its own log.
}

#[test]
fn test_recovery_retries_within_the_requested_count() {
    let f = Fixture::new();
    f.write(1, "chimney", "MARKER: block-me", "None", "to_do");
    let result = f.work("1", &["--recover", "test"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    assert!(f.project.join("swim_lanes/done/001_chimney.md").exists());
}

#[test]
fn test_planner_can_leave_a_ticket_for_a_human() {
    let f = Fixture::new();
    f.write(
        1,
        "chimney",
        "MARKER: block-me needs-human",
        "None",
        "to_do",
    );
    let result = f.work("all", &["--recover", "test"]);
    assert_eq!(code(&result), 1, "{}", shown(&result));
    let out = stdout(&result);
    assert!(out.contains("Recovery: needs a human decision. choose the chimney width"));
    assert!(f.project.join("swim_lanes/blocked/001_chimney.md").exists());
    assert_eq!(out.matches("[recover]").count(), 1); // Never more than once per ticket.
}

#[test]
fn test_planner_repairs_broken_dependencies_once() {
    let f = Fixture::new();
    f.ticket(1, "annex", "`BuildAnnex.cs` Section Done", "to_do");
    let result = f.work("all", &["--recover", "test"]);
    assert_eq!(code(&result), 0, "{}", shown(&result));
    let out = stdout(&result);
    assert!(out.contains("[repair queue] 1 stuck ticket(s)"));
    assert!(out.contains("Repair: 1 of 1 fixed."));
    assert!(f.project.join("swim_lanes/done/001_annex.md").exists());
}

#[test]
fn test_recovery_off() {
    let f = Fixture::new();
    f.write(1, "chimney", "MARKER: block-me", "None", "to_do");
    f.ticket(2, "annex", "`BuildAnnex.cs`", "to_do");
    let result = f.work("all", &["--recover", "off"]);
    let out = stdout(&result);
    assert!(!out.contains("[recover]"));
    assert!(!out.contains("[repair queue]"));
}
