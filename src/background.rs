//! shell commands outside a model's turn.
//!
//! `!command` from the user, and waits that wake the model when they end.
//!
//! Claude Code wakes its own model when a background shell finishes, and Donut keeps listening between turns so
//! those turns show up. Codex and Qwen have no such wake-up, so Donut offers one: a model ends its reply with
//! `DONUT-WAIT: <command that exits when the work is done>`, Donut runs it, and when it exits Donut sends the
//! model its exit code and output so it carries on from there.

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::platform::process_options;

static WAIT_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*`?DONUT-WAIT:\s*(.+?)`?\s*$").unwrap());
pub const OUTPUT_LIMIT: usize = 4000;
pub const WAIT_LIMIT_SECONDS: u64 = 6 * 3600;

pub const WAIT_NOTE: &str = "\n\n(Donut: when you need to wait for something slow, such as a build, a CI run or a deploy, you can stop \
instead of polling. Start it in the background, then end your reply with one line \
`DONUT-WAIT: <shell command that exits when it is done>`, for example `DONUT-WAIT: gh run watch 123` or \
`DONUT-WAIT: while kill -0 4242; do sleep 10; done`. Donut runs that command in the project and, when it \
exits, sends you its exit code and output so you can continue.)";

/// The command from the last DONUT-WAIT line of an answer, or None.
pub fn wait_request(answer: &str) -> Option<String> {
    let last = WAIT_LINE.captures_iter(answer).last()?;
    let text = last[1].trim().trim_matches('`').trim();
    Some(text.to_string())
}

/// The end of a text: all of it, or "…" and the last `limit` characters.
pub fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - limit).collect();
    format!("…{kept}")
}

fn or_none(text: String) -> String {
    if text.is_empty() {
        "(no output)".to_string()
    } else {
        text
    }
}

pub fn wake_prompt(command: &str, code: Option<i32>, output: &str) -> String {
    let state = match code {
        None => "timed out".to_string(),
        Some(code) => format!("exited with code {code}"),
    };
    format!(
        "Donut: the background command you asked to wait on {state}.\n$ {command}\nOutput (end):\n```\n{}\n```\nContinue where you left off.",
        or_none(tail(output, OUTPUT_LIMIT))
    )
}

/// What the user ran with !command since their last message, for the model's context: (command, exit code, output).
pub fn shell_note(runs: &[(String, i32, String)]) -> String {
    let blocks: Vec<String> = runs
        .iter()
        .map(|(command, code, output)| {
            format!(
                "$ {command}\n{}\n(exit {code})",
                or_none(tail(output, 2000))
            )
        })
        .collect();
    format!(
        "Shell commands I ran in the project before this message:\n```\n{}\n```\n\n",
        blocks.join("\n\n")
    )
}

/// A shell command running in the project, with its output (stderr merged into stdout) as a pipe.
pub struct Started {
    pub child: Child,
    pub output: ChildStdout,
}

/// Start a shell command in the project in its own process group (so Ctrl+C can stop the whole group).
pub fn start(command: &str, project: &Path) -> std::io::Result<Started> {
    let mut shell;
    #[cfg(windows)]
    {
        // Like Python's `create_subprocess_shell`: `cmd /c "<command>"`, with the command text passed
        // through untouched. Rust's usual argument quoting would escape the quotes inside the command.
        use std::os::windows::process::CommandExt;
        shell = Command::new("cmd");
        shell.arg("/C").raw_arg(format!("\"({command}) 2>&1\""));
    }
    #[cfg(not(windows))]
    {
        shell = Command::new("sh");
        shell.arg("-c").arg(format!("exec 2>&1\n{command}"));
    }
    shell
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    process_options(&mut shell);
    let mut child = shell.spawn()?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no output pipe"))?;
    Ok(Started { child, output })
}

fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .or_else(|| status.signal().map(|signal| -signal))
    }
    #[cfg(not(unix))]
    {
        status.code()
    }
}

/// Run a wait command to its end. Returns (exit code, or None when it ran past the limit; output).
pub fn wait_for(
    command: &str,
    project: &Path,
    limit: Duration,
) -> std::io::Result<(Option<i32>, String)> {
    wait_for_cancellable(command, project, limit, &AtomicBool::new(false))
}

/// Like `wait_for`, but kills the command and returns an `Interrupted` error when `cancel` becomes true.
pub fn wait_for_cancellable(
    command: &str,
    project: &Path,
    limit: Duration,
    cancel: &AtomicBool,
) -> std::io::Result<(Option<i32>, String)> {
    let Started {
        mut child,
        mut output,
    } = start(command, project)?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = output.read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    let begun = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break Some(exit_code(status));
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            ));
        }
        if begun.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // After a kill, a grandchild can still hold the pipe open: take what has arrived and move on.
    let grace = if code.is_some() {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(1)
    };
    let bytes = receiver.recv_timeout(grace).unwrap_or_default();
    Ok((code.flatten(), String::from_utf8_lossy(&bytes).into_owned()))
}

/// A turn Donut starts on its own, to give a model the result of the command it was waiting on.
/// `S` is the session type the caller uses.
#[derive(Debug, Clone)]
pub struct Wake<S> {
    pub session: S,
    pub role: String,
    pub command: String,
    pub code: Option<i32>,
    pub output: String,
    pub created: f64,
}

impl<S> Wake<S> {
    pub fn new(session: S, role: &str, command: &str, code: Option<i32>, output: &str) -> Self {
        Wake {
            session,
            role: role.into(),
            command: command.into(),
            code,
            output: output.into(),
            created: 0.0,
        }
    }

    pub fn prompt(&self) -> String {
        wake_prompt(&self.command, self.code, &self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wait_line_and_notes() {
        let answer = "Started the deploy.\n\nDONUT-WAIT: `gh run watch 42`\n";
        assert_eq!(wait_request(answer).as_deref(), Some("gh run watch 42"));
        assert_eq!(
            wait_request("No waiting here. DONUT-WAIT: inside a sentence"),
            None
        );
        assert!(wake_prompt("make", Some(3), "boom").contains("exited with code 3"));
        assert!(wake_prompt("make", None, "").contains("timed out"));
        let note = shell_note(&[("git status".into(), 0, "clean".into())]);
        assert!(note.contains("$ git status\nclean\n(exit 0)"));
        assert!(tail(&"x".repeat(5000), OUTPUT_LIMIT).starts_with('…'));
    }

    #[test]
    fn test_wait_for_returns_code_and_output() {
        let temp = tempfile::tempdir().unwrap();
        let (echo, sleep) = if cfg!(windows) {
            ("echo 42", "ping -n 6 127.0.0.1 > nul")
        } else {
            ("echo $((41 + 1))", "sleep 5")
        };
        let (code, output) = wait_for(echo, temp.path(), Duration::from_secs(60)).unwrap();
        assert_eq!((code, output.trim()), (Some(0), "42"));
        let (code, _) = wait_for(sleep, temp.path(), Duration::from_millis(300)).unwrap();
        assert_eq!(code, None);
    }

    #[test]
    fn test_wait_for_merges_stderr_and_reports_the_exit_code() {
        let temp = tempfile::tempdir().unwrap();
        let command = if cfg!(windows) {
            "echo out& echo err 1>&2& exit 3"
        } else {
            "echo out; echo err >&2; exit 3"
        };
        let (code, output) = wait_for(command, temp.path(), Duration::from_secs(60)).unwrap();
        assert_eq!(code, Some(3));
        assert!(output.contains("out") && output.contains("err"));
    }
}
