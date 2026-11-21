//! Small OS-specific locking and process-lifecycle operations.

use std::process::Command;

/// Start the child in its own process group (session on Unix), so stop_process can end its whole tree.
pub fn process_options(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    command
}

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

/// Python's sys.platform for this build: "darwin", "win32" or "linux".
pub fn system() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        _ => "linux",
    }
}

/// Run a command with the given bytes on its standard input and collect what it prints.
pub fn run_with_input(command: &mut Command, input: &[u8]) -> std::io::Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = stdin.write_all(input);
        });
        child.wait_with_output()
    })
}

fn wait_for(child: &mut Child, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn kill_group(pid: u32, signal: i32) {
    // SAFETY: killpg only sends a signal to the process group the child started with process_options.
    unsafe {
        libc::killpg(pid as i32, signal);
    }
}

/// End the child and everything it started: SIGTERM to its group (taskkill on Windows), SIGKILL after ten seconds.
pub fn stop_process(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    kill_group(child.id(), libc::SIGTERM);
    if !wait_for(child, Duration::from_secs(10)) {
        #[cfg(windows)]
        let _ = child.kill();
        #[cfg(unix)]
        kill_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
    }
}

/// The command a new terminal window runs: this Donut, in project.
pub fn window_command(project: &Path) -> Vec<String> {
    let exe = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "donut".into());
    let mut command = vec![
        exe,
        "-p".to_string(),
        project.to_string_lossy().into_owned(),
    ];
    // A new terminal window may not inherit this one's environment, so the profile is passed in the command.
    let profile = crate::config::profile_name();
    if profile != "default" {
        command.push("--profile".into());
        command.push(profile);
    }
    command
}

/// What open_window starts: the program and arguments, where it runs, and whether it needs a new console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub new_console: bool,
}

fn spawn_detached(request: &Spawn) {
    let mut command = Command::new(&request.args[0]);
    command.args(&request.args[1..]);
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    if request.new_console {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0010);
    }
    let _ = command.spawn();
}

/// Python's shlex.quote.
pub fn shell_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".into();
    }
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Open Donut for project in a new terminal window. Returns where it opened, or None when no way was found.
///
/// Inside tmux it is a new tmux window; on macOS a Terminal window (iTerm when Donut runs in iTerm); on Windows a
/// new console (Windows Terminal when installed); on Linux $TERMINAL or the first terminal emulator found.
pub fn open_window(project: &Path) -> Option<String> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let which = |name: &str| which::which(name).is_ok();
    open_window_with(project, &env, &which, system(), &mut spawn_detached)
}

/// open_window() with the environment, program lookup, platform and process start given.
pub fn open_window_with(
    project: &Path,
    env: &HashMap<String, String>,
    which: &dyn Fn(&str) -> bool,
    system: &str,
    spawn: &mut dyn FnMut(&Spawn),
) -> Option<String> {
    let command = window_command(project);
    let folder = project.to_string_lossy().into_owned();
    let start = |args: Vec<String>, cwd: Option<PathBuf>, new_console: bool| Spawn {
        args,
        cwd,
        new_console,
    };
    let with = |head: &[&str], tail: &[String]| -> Vec<String> {
        head.iter()
            .map(|item| item.to_string())
            .chain(tail.iter().cloned())
            .collect()
    };
    if env.get("TMUX").is_some_and(|value| !value.is_empty()) && which("tmux") {
        spawn(&start(
            with(&["tmux", "new-window", "-c", &folder], &command),
            None,
            false,
        ));
        return Some("a new tmux window".into());
    }
    if system == "darwin" {
        let line = format!("cd {} && {}", shell_quote(&folder), shell_join(&command));
        let quoted = format!("\"{}\"", line.replace('\\', "\\\\").replace('"', "\\\""));
        if env.get("TERM_PROGRAM").map(String::as_str) == Some("iTerm.app") {
            let script = format!(
                "tell application \"iTerm\" to create window with default profile command {quoted}"
            );
            spawn(&start(
                with(&["osascript", "-e", &script], &[]),
                None,
                false,
            ));
            return Some("a new iTerm window".into());
        }
        let script = format!("tell application \"Terminal\" to do script {quoted}");
        spawn(&start(
            with(
                &[
                    "osascript",
                    "-e",
                    &script,
                    "-e",
                    "tell application \"Terminal\" to activate",
                ],
                &[],
            ),
            None,
            false,
        ));
        return Some("a new Terminal window".into());
    }
    if system == "win32" {
        if which("wt") {
            spawn(&start(with(&["wt", "-d", &folder], &command), None, false));
            return Some("a new Windows Terminal window".into());
        }
        spawn(&start(command, Some(project.to_path_buf()), true));
        return Some("a new console window".into());
    }
    let mut candidates: Vec<String> = env
        .get("TERMINAL")
        .filter(|name| !name.is_empty())
        .cloned()
        .into_iter()
        .collect();
    candidates.extend(
        [
            "x-terminal-emulator",
            "gnome-terminal",
            "konsole",
            "kitty",
            "alacritty",
            "wezterm",
            "xterm",
        ]
        .iter()
        .map(|name| name.to_string()),
    );
    for name in candidates {
        if !which(&name) {
            continue;
        }
        let base = Path::new(&name)
            .file_name()
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or(name.clone());
        let args = match base.as_str() {
            "gnome-terminal" => with(&[&name, "--working-directory", &folder, "--"], &command),
            "wezterm" => with(&[&name, "start", "--cwd", &folder, "--"], &command),
            "kitty" => with(&[&name, "--directory", &folder], &command),
            _ => with(&[&name, "-e"], &command),
        };
        spawn(&start(args, Some(project.to_path_buf()), false));
        return Some(format!("a new {base} window"));
    }
    None
}

/// Put text on this computer's clipboard with its own tool. Returns whether one ran.
///
/// Terminals copy for an app only when they support OSC 52 (macOS Terminal does not), so Donut also uses
/// pbcopy, clip, wl-copy, xclip or xsel when present.
pub fn copy_native(text: &str) -> bool {
    copy_native_with(text, system(), &|name: &str| which::which(name).is_ok())
}

/// copy_native() with the platform and program lookup given.
pub fn copy_native_with(text: &str, system: &str, which: &dyn Fn(&str) -> bool) -> bool {
    let commands: Vec<Vec<&str>> = match system {
        "darwin" => vec![vec!["pbcopy"]],
        "win32" => vec![vec!["clip"]],
        _ => vec![
            vec!["wl-copy"],
            vec!["xclip", "-selection", "clipboard"],
            vec!["xsel", "--clipboard", "--input"],
        ],
    };
    for command in commands {
        if !which(command[0]) {
            continue;
        }
        let data: Vec<u8> = if command[0] == "clip" {
            text.encode_utf16()
                .flat_map(|unit| unit.to_le_bytes())
                .collect()
        } else {
            text.as_bytes().to_vec()
        };
        let Ok(mut child) = Command::new(command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&data);
        }
        if wait_for(&mut child, Duration::from_secs(3))
            && child.wait().is_ok_and(|status| status.success())
        {
            return true;
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn sleeper(seconds: &str) -> Child {
        let mut command = Command::new("sleep");
        command.arg(seconds);
        process_options(&mut command).spawn().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_running_process() {
        let mut process = sleeper("60");
        stop_process(&mut process);
        assert!(process.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_stopping_finished_process_is_safe() {
        let mut command = Command::new("true");
        let mut process = process_options(&mut command).spawn().unwrap();
        let status = process.wait().unwrap();
        stop_process(&mut process);
        assert_eq!(status.code(), Some(0));
    }

    fn open(
        system: &str,
        env: &[(&str, &str)],
        found: &[&str],
    ) -> (Option<String>, Option<Vec<String>>) {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let which = |name: &str| found.contains(&name);
        let mut started = Vec::new();
        let where_ = open_window_with(Path::new("/work/app"), &env, &which, system, &mut |spawn| {
            started.push(spawn.args.clone())
        });
        (where_, started.into_iter().next())
    }

    #[test]
    fn test_each_platform_opens_donut_in_its_own_terminal() {
        let command = window_command(Path::new("/work/app"));
        let app = Path::new("/work/app").to_string_lossy().into_owned();
        let (where_, args) = open("linux", &[("TMUX", "/tmp/t")], &["tmux", "gnome-terminal"]);
        assert_eq!(where_.as_deref(), Some("a new tmux window"));
        assert_eq!(
            args.unwrap()[..4],
            ["tmux", "new-window", "-c", app.as_str()]
        );
        let (where_, args) = open("darwin", &[], &[]);
        assert_eq!(where_.as_deref(), Some("a new Terminal window"));
        let script = &args.unwrap()[2];
        assert!(
            script.starts_with("tell application \"Terminal\" to do script \"cd "),
            "{script}"
        );
        assert!(script.contains(" -p "), "{script}");
        let (where_, _) = open("darwin", &[("TERM_PROGRAM", "iTerm.app")], &[]);
        assert_eq!(where_.as_deref(), Some("a new iTerm window"));
        let (_, args) = open("win32", &[], &["wt"]);
        let expected: Vec<String> = ["wt", "-d", app.as_str()]
            .iter()
            .map(|s| s.to_string())
            .chain(command.clone())
            .collect();
        assert_eq!(args.unwrap(), expected);
        let (_, args) = open("linux", &[], &["gnome-terminal", "xterm"]);
        assert_eq!(
            args.unwrap()[..4],
            ["gnome-terminal", "--working-directory", app.as_str(), "--"]
        );
        let (where_, args) = open("linux", &[("TERMINAL", "foot")], &["foot", "xterm"]);
        assert_eq!(where_.as_deref(), Some("a new foot window"));
        assert_eq!(args.unwrap()[..2], ["foot", "-e"]);
        assert_eq!(open("linux", &[], &[]), (None, None));
        assert_eq!(command[command.len() - 2..], ["-p", app.as_str()]);
    }

    #[test]
    fn test_shell_quote_matches_python() {
        assert_eq!(shell_quote("plain/path-1.txt"), "plain/path-1.txt");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's here"), "'it'\"'\"'s here'");
    }

    #[test]
    fn test_clipboard_uses_the_first_tool_found() {
        assert!(!copy_native_with("x", "linux", &|_| false));
    }
}
