//! compatibility for existing qwen/qwen-faster shell launchers.

use std::path::PathBuf;

use clap::{Arg, Command};
use serde_json::json;

use crate::conversations::resolve;
use crate::runner::{BatchOptions, colored, emit, is_interrupt, run_batch};
use crate::ui::banner_text;

/// The argument parser of `qwen-build-next`.
pub fn command() -> Command {
    Command::new("qwen-build-next")
        .about("Legacy launcher; prefer donut -m ALIAS --work next COUNT.")
        .arg(
            Arg::new("model")
                .long("model")
                .value_name("MODEL")
                .required(true),
        )
        .arg(
            Arg::new("project")
                .short('p')
                .long("project")
                .value_name("PROJECT")
                .default_value("."),
        )
        .arg(Arg::new("build").required(true).value_parser(["build"]))
        .arg(Arg::new("next").required(true).value_parser(["next"]))
        .arg(Arg::new("count").required(true))
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

/// Run the launcher. Returns the exit code.
pub fn main() -> i32 {
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    run(&args)
}

/// main() with the arguments (argv[0] first) given.
pub fn run(args: &[String]) -> i32 {
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    let model = matches
        .get_one::<String>("model")
        .cloned()
        .unwrap_or_default();
    let project = resolve(&expanduser(
        matches
            .get_one::<String>("project")
            .map_or(".", String::as_str),
    ));
    let count = matches
        .get_one::<String>("count")
        .cloned()
        .unwrap_or_default();
    emit(&banner_text(colored()));
    let profile = json!({"engine": "ollama", "model": model, "auto_approve": true});
    match run_batch(&profile, &project, &count, BatchOptions::default()) {
        Ok(code) => code,
        Err(error) if is_interrupt(&error) => 130,
        Err(error) => {
            eprintln!("donut: {error}");
            1
        }
    }
}
