//! Ponytail's task telemetry inside Donut.
//!
//! Ponytail (github.com/DietrichGebert/ponytail) is a Claude Code and Codex plugin. Its band, the line of task
//! numbers above the prompt, is a Claude Code UI module that Donut cannot load. So Donut keeps the same numbers
//! per conversation (cost, turns, agent time, tokens, tests, level, model), shows them in its own band, and
//! writes the same row to the same ledger, `~/.config/ponytail/band/<project>.csv`, so Ponytail's board lists
//! Donut's tasks next to Claude Code's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::conversations::{now_seconds, resolve, write_private};
use crate::plugins::{claude_dir, codex_dir, home_dir};

pub const LEVELS: [&str; 3] = ["lite", "full", "ultra"];
pub const HEADER: &str = "key,started,project,name,cost_usd,turns,agent_minutes,tokens_total,tokens_output,\
reuse_pct,reused,new,callers_open,tests,level,model";

/// The same runners Ponytail's band recognizes, plus Python's unittest.
static TEST_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(npm (run )?test|pnpm (run )?test|yarn test|bun test|npx (jest|vitest)|jest|vitest|pytest|python3? -m pytest",
        r"|python3? -m unittest|unittest discover|go test|cargo test|mvn test|gradle test|rspec|phpunit|dotnet test",
        r"|node --test|deno test|make test)\b"
    ))
    .unwrap()
});

fn normpath(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// The live Ponytail level for this login and project, as its hooks wrote it; "off" when none.
/// Claude's hooks write under the Claude config dir; Codex's write under the plugin's data dir (`PLUGIN_DATA`).
pub fn level(profile: &Value, project: &Path) -> String {
    let base = if profile.get("engine").and_then(Value::as_str) == Some("codex") {
        codex_dir().join("plugins/data/ponytail-ponytail")
    } else {
        claude_dir(profile)
    };
    let digest: String = Sha256::digest(normpath(&project.to_string_lossy()).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    for flag in [
        base.join("ponytail-modes").join(digest),
        base.join(".ponytail-active"),
    ] {
        let Ok(value) = std::fs::read_to_string(flag) else {
            continue;
        };
        let value = value.trim();
        return if LEVELS.contains(&value) {
            value.to_string()
        } else {
            "off".to_string()
        };
    }
    "off".to_string()
}

fn number(patterns: &[&str], output: &str) -> Option<i64> {
    for pattern in patterns {
        if let Some(found) = Regex::new(pattern).unwrap().captures(output) {
            return found[1].parse().ok();
        }
    }
    None
}

/// (passed, failed) from common test runner summaries; None for a count it could not find.
pub fn test_counts(output: &str) -> (Option<i64>, Option<i64>) {
    let mut passed = number(
        &[r"(\d+) (?:passed|passing)\b", r"(?:ℹ |# )pass (\d+)"],
        output,
    );
    let failed = number(
        &[
            r"(\d+) (?:failed|failing)\b",
            r"(?:ℹ |# )fail (\d+)",
            r"FAILED \((?:failures|errors)=(\d+)",
        ],
        output,
    );
    let ran = number(&[r"Ran (\d+) tests?\b"], output); // unittest: "Ran 12 tests ... OK" or "FAILED (failures=1)".
    if let (Some(ran), None) = (ran, passed) {
        passed = Some(ran - failed.unwrap_or(0));
    }
    (passed, failed)
}

pub fn duration(seconds: f64) -> String {
    let seconds = seconds as i64;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    }
}

pub fn tokens(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{:.0}k", count as f64 / 1000.0)
    } else if count >= 1000 {
        let text = format!("{:.1}", count as f64 / 1000.0);
        format!("{}k", text.strip_suffix(".0").unwrap_or(&text))
    } else {
        count.to_string()
    }
}

fn int_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// One conversation's numbers, the row Ponytail's band keeps for a task. A /clear or /new starts a new one.
#[derive(Debug, Clone)]
pub struct Task {
    pub project: String,
    pub engine: String,
    pub model: String,
    pub started: f64,
    pub key: String,
    pub name: String,
    pub turns: i64,
    pub cost: f64,
    pub total: i64,
    pub output: i64,
    pub agent_seconds: f64,
    pub tests: String,
    pub level: String,
    pub reported_cost: Option<f64>,
    pub commands: HashMap<String, String>,
}

impl Task {
    pub fn new(project: &Path, engine: &str, model: Option<&str>) -> Task {
        let started = now_seconds();
        let id = uuid::Uuid::new_v4().simple().to_string();
        Task {
            project: resolve(project)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            engine: engine.to_string(),
            model: model.unwrap_or("").to_string(),
            started,
            key: format!("donut-{}-{}", &id[..8], (started * 1000.0) as i64),
            name: String::new(),
            turns: 0,
            cost: 0.0,
            total: 0,
            output: 0,
            agent_seconds: 0.0,
            tests: "not run".to_string(),
            level: "off".to_string(),
            reported_cost: None,
            commands: HashMap::new(),
        }
    }

    /// Add one finished turn. `usage` is the session's own report: Claude's per-turn tokens and running cost,
    /// Codex's thread totals, or Qwen's per-turn tokens.
    pub fn turn(&mut self, prompt: &str, seconds: f64, usage: &Value) {
        self.turns += 1;
        self.agent_seconds += seconds;
        if self.name.is_empty() {
            self.name = prompt
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(80)
                .collect();
        }
        if self.engine == "codex" {
            let total = usage.get("total");
            self.total = self
                .total
                .max(int_of(total.and_then(|t| t.get("totalTokens"))));
            self.output = self
                .output
                .max(int_of(total.and_then(|t| t.get("outputTokens"))));
            return;
        }
        let turn = if self.engine == "claude" {
            usage.get("tokens").unwrap_or(usage)
        } else {
            usage
        };
        if turn.is_object() {
            let output = int_of(turn.get("output_tokens"));
            self.output += output;
            self.total += output
                + [
                    "input_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                ]
                .iter()
                .map(|key| int_of(turn.get(*key)))
                .sum::<i64>();
        }
        if let Some(reported) = usage.get("reported_cost_usd").and_then(Value::as_f64) {
            // Claude reports the session's running cost; a lower figure means a new provider process started over.
            let previous = self.reported_cost.unwrap_or(0.0);
            self.cost += if reported >= previous {
                reported - previous
            } else {
                reported
            };
            self.reported_cost = Some(reported);
        }
    }

    pub fn command(&mut self, identity: &str, text: &str) {
        if TEST_COMMAND.is_match(text) {
            self.commands.insert(identity.to_string(), text.to_string());
        }
    }

    /// Turn a test command's output into the band's tests cell.
    pub fn command_output(&mut self, identity: &str, output: &str) {
        if self.commands.remove(identity).is_none() {
            return;
        }
        let (passed, failed) = test_counts(output);
        if let Some(failed) = failed.filter(|f| *f != 0) {
            self.tests = format!("{failed} failed");
        } else if let Some(passed) = passed {
            self.tests = format!("{passed} passed");
        }
    }

    /// The ledger row, one string for each column of `HEADER`.
    pub fn row(&self) -> Vec<String> {
        let started = chrono::DateTime::from_timestamp(self.started as i64, 0)
            .map(|utc| {
                utc.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        vec![
            self.key.clone(),
            started,
            self.project.clone(),
            self.name.clone(),
            format!("{:.2}", self.cost),
            self.turns.to_string(),
            format!("{:.1}", self.agent_seconds / 60.0),
            self.total.to_string(),
            self.output.to_string(),
            String::new(), // reuse_pct, reused, new and callers_open come from Claude Code's edit hooks, which Donut lacks.
            "0".into(),
            "0".into(),
            "0".into(),
            self.tests.clone(),
            self.level.clone(),
            self.model.clone(),
        ]
    }

    /// (label, value) pairs for Donut's band, in the order Ponytail's band shows them.
    pub fn band(&self) -> Vec<(String, String)> {
        let mut pairs = vec![("ponytail".to_string(), self.level.clone())];
        if self.engine == "claude" {
            pairs.push(("cost".into(), format!("${:.2}", self.cost)));
        }
        pairs.push(("turns".into(), self.turns.to_string()));
        pairs.push(("agent".into(), duration(self.agent_seconds)));
        pairs.push(("tokens".into(), tokens(self.total)));
        pairs.push(("tests".into(), self.tests.clone()));
        pairs
    }
}

pub fn ledger_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("ponytail")
        .join("band")
}

/// One CSV line, quoted the way Python's csv module quotes (only fields that need it).
fn csv_line(fields: &[String]) -> String {
    fields
        .iter()
        .map(|field| {
            if field.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", field.replace('"', "\"\""))
            } else {
                field.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Write the task's row into its project's ledger, replacing its earlier row. Returns the ledger path.
pub fn record(task: &Task) -> std::io::Result<PathBuf> {
    let folder = ledger_dir();
    std::fs::create_dir_all(&folder)?;
    let safe = Regex::new(r"[^\w.-]")
        .unwrap()
        .replace_all(&task.project, "_")
        .into_owned();
    let path = folder.join(format!("{safe}.csv"));
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let own = format!("{},", task.key);
    let mut lines: Vec<String> = vec![HEADER.to_string()];
    lines.extend(
        existing
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with("key,") && !line.starts_with(&own))
            .map(str::to_string),
    );
    lines.push(csv_line(&task.row()));
    write_private(&path, &(lines.join("\n") + "\n"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        root: tempfile::TempDir,
    }

    fn env() -> Env {
        let guard = crate::conversations::env_lock();
        let root = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", root.path().join("config")) };
        Env {
            _guard: guard,
            root,
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn claude_usage(cost: f64, output: i64) -> Value {
        json!({
            "tokens": {"input_tokens": 1000, "output_tokens": output, "cache_read_input_tokens": 900},
            "reported_cost_usd": cost,
        })
    }

    #[test]
    fn test_claude_turns_add_up_cost_tokens_and_time() {
        let env = env();
        let mut task = Task::new(
            &env.root.path().join("my-app"),
            "claude",
            Some("claude-opus-5-5"),
        );
        task.turn(
            "Add rate limiting\nto the API",
            30.0,
            &claude_usage(0.25, 100),
        );
        task.turn("and tests", 90.0, &claude_usage(0.40, 100));
        task.turn("after a resume", 10.0, &claude_usage(0.05, 100)); // A new provider process counts from zero.
        assert_eq!(
            (
                task.turns,
                (task.cost * 100.0).round() / 100.0,
                task.total,
                task.output
            ),
            (3, 0.45, 6000, 300)
        );
        assert_eq!(task.name, "Add rate limiting to the API");
        let band: Vec<(String, String)> = [
            ("ponytail", "off"),
            ("cost", "$0.45"),
            ("turns", "3"),
            ("agent", "2m 10s"),
            ("tokens", "6k"),
            ("tests", "not run"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(task.band(), band);
    }

    #[test]
    fn test_codex_reports_thread_totals() {
        let env = env();
        let mut task = Task::new(env.root.path(), "codex", Some("gpt-6.1"));
        task.turn(
            "one",
            5.0,
            &json!({"total": {"totalTokens": 5000, "outputTokens": 700}}),
        );
        task.turn(
            "two",
            5.0,
            &json!({"total": {"totalTokens": 9000, "outputTokens": 900}}),
        );
        assert_eq!((task.total, task.output), (9000, 900));
        assert!(!task.band().iter().any(|(label, _)| label == "cost"));
    }

    #[test]
    fn test_test_runs_fill_the_tests_cell() {
        let env = env();
        let mut task = Task::new(env.root.path(), "claude", Some("m"));
        task.command("a", "ls -la");
        task.command_output("a", "3 passed");
        assert_eq!(task.tests, "not run"); // Not a test command.
        task.command("b", "python -m pytest -q");
        task.command_output("b", "===== 12 passed in 0.3s =====");
        assert_eq!(task.tests, "12 passed");
        task.command("c", ".venv/bin/python -m unittest discover -s tests");
        task.command_output("c", "Ran 9 tests in 0.1s\n\nFAILED (failures=2)");
        assert_eq!(task.tests, "2 failed");
        task.command("d", "npm test");
        task.command_output("d", "Ran 9 tests in 0.1s\n\nOK");
        assert_eq!(task.tests, "9 passed");
    }

    #[test]
    fn test_ledger_row_matches_ponytails_format_and_is_replaced_each_turn() {
        let env = env();
        let other = env.root.path().join("config/ponytail/band/my-app.csv");
        write(
            &other,
            &format!(
                "{HEADER}\nclaude-1234,2026-11-01 09:00,my-app,from claude code,1.00,2,1.0,10,5,,0,0,0,not run,full,m\n"
            ),
        );
        let mut task = Task::new(
            &env.root.path().join("my-app"),
            "claude",
            Some("claude-opus-5-5"),
        );
        task.level = "full".into();
        task.turn("Say \"hi\", then stop", 60.0, &claude_usage(0.10, 100));
        record(&task).unwrap();
        task.turn("again", 60.0, &claude_usage(0.30, 100));
        let path = record(&task).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], HEADER);
        assert_eq!(lines.len(), 3); // Claude Code's row is kept; Donut's row is replaced, not repeated.
        assert!(lines[1].starts_with("claude-1234,"));
        assert!(
            lines[2].contains("\"Say \"\"hi\"\", then stop\""),
            "{}",
            lines[2]
        );
        let row = task.row();
        let column = |name: &str| row[HEADER.split(',').position(|h| h == name).unwrap()].clone();
        assert_eq!(column("name"), "Say \"hi\", then stop");
        assert_eq!(
            (column("cost_usd"), column("turns"), column("agent_minutes")),
            ("0.30".into(), "2".into(), "2.0".into())
        );
        assert_eq!(
            (column("level"), column("model"), column("project")),
            ("full".into(), "claude-opus-5-5".into(), "my-app".into())
        );
        assert!(lines[2].ends_with(",full,claude-opus-5-5"));
    }

    #[test]
    fn test_level_reads_the_project_flag_before_the_shared_one() {
        let env = env();
        let claude = env.root.path().join("claude");
        let project = env.root.path().join("app");
        let profile = json!({"engine": "claude", "claude_config_dir": claude});
        assert_eq!(level(&profile, &project), "off");
        write(&claude.join(".ponytail-active"), "ultra\n");
        assert_eq!(level(&profile, &project), "ultra");
        let digest: String = Sha256::digest(project.to_string_lossy().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        write(&claude.join("ponytail-modes").join(digest), "lite");
        assert_eq!(level(&profile, &project), "lite");
    }

    #[test]
    fn test_level_reads_codex_plugin_data_for_codex() {
        let env = env();
        let codex = env.root.path().join("codex");
        unsafe { std::env::set_var("CODEX_HOME", &codex) };
        let claude = env.root.path().join("claude");
        write(&claude.join(".ponytail-active"), "lite");
        let profile = json!({"engine": "codex", "claude_config_dir": claude});
        let project = env.root.path().join("app");
        assert_eq!(level(&profile, &project), "off");
        write(&codex.join("plugins/data/ponytail-ponytail/.ponytail-active"), "full\n");
        assert_eq!(level(&profile, &project), "full");
        unsafe { std::env::remove_var("CODEX_HOME") };
    }

    #[test]
    fn test_formats() {
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1000), "1k");
        assert_eq!(tokens(2500), "2.5k");
        assert_eq!(tokens(12_345), "12k");
        assert_eq!(tokens(2_500_000), "2.5M");
        assert_eq!(duration(5.0), "5s");
        assert_eq!(duration(3725.0), "1h 2m");
        assert_eq!(normpath("/a/./b/../c//"), "/a/c");
    }
}
