//! Check that every command-line flag Donut passes to a provider CLI is still one that CLI documents.
//!
//! The unit tests use fake executables, so this is the only check that exercises the real provider
//! contracts. It needs the provider CLIs installed, so it is ignored by default; run it with
//! `cargo test --test provider_contract -- --ignored --nocapture`. A missing CLI is reported and skipped.

use std::collections::BTreeSet;
use std::process::Command;

use donut::providers::command;
use serde_json::{Value, json};

const MODES: &[&str] = &["ask", "accept_edits", "auto", "full"];

fn profiles(engine: &str) -> Vec<Value> {
    MODES
        .iter()
        .map(|mode| json!({"engine": engine, "model": "x", "permission_mode": mode}))
        .collect()
}

/// The flag whose values must also be among the choices the CLI documents.
fn value_flag(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("--permission-mode"),
        "qwen" => Some("--approval-mode"),
        _ => None,
    }
}

fn help(cli: &str) -> Vec<&'static str> {
    match cli {
        "codex" => vec!["codex", "exec", "--help"],
        "claude" => vec!["claude", "--help"],
        _ => vec!["qwen", "--help"],
    }
}

fn flags_for(profiles: &[Value]) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    for profile in profiles {
        for (readonly, review) in [(false, false), (true, false), (false, true)] {
            let (args, _) = command(profile, "probe", readonly, None, review).expect("command");
            flags.extend(args.into_iter().skip(1).filter(|arg| arg.starts_with("--")));
        }
    }
    flags
}

fn values_for(cli: &str, profiles: &[Value]) -> BTreeSet<String> {
    let Some(flag) = value_flag(cli) else {
        return BTreeSet::new();
    };
    let mut values = BTreeSet::new();
    for profile in profiles {
        let (args, _) = command(profile, "probe", false, None, false).expect("command");
        if let Some(index) = args.iter().position(|arg| arg == flag) {
            values.insert(args[index + 1].clone());
        }
    }
    values
}

#[test]
#[ignore = "needs the real provider CLIs; run with --ignored"]
fn provider_flags_are_documented() {
    let mut failed = Vec::new();
    for (cli, engine) in [("codex", "codex"), ("claude", "claude"), ("qwen", "ollama")] {
        if which::which(cli).is_err() {
            println!("{cli}: not installed, skipped");
            continue;
        }
        let profiles = profiles(engine);
        let argv = help(cli);
        let output = Command::new(argv[0])
            .args(&argv[1..])
            .output()
            .expect("help");
        let text = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        let flags = flags_for(&profiles);
        let mut missing: Vec<String> = flags
            .iter()
            .filter(|flag| !text.contains(flag.as_str()))
            .cloned()
            .collect();
        if let Some(flag) = value_flag(cli) {
            missing.extend(
                values_for(cli, &profiles)
                    .into_iter()
                    .filter(|value| !text.contains(&format!("\"{value}\"")))
                    .map(|value| format!("{flag} {value}")),
            );
        }
        let version = Command::new(cli)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let version = version.unwrap_or_default();
        if missing.is_empty() {
            println!("{cli} {version}: all {} flags documented", flags.len());
        } else {
            println!(
                "{cli} {version}: no longer documents {}",
                missing.join(", ")
            );
            failed.push(cli);
        }
    }
    assert!(
        failed.is_empty(),
        "flags no longer documented by: {failed:?}"
    );
}
