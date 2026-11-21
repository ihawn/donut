//! Tests that run the real `donut` binary.

use std::process::Command;

use donut::ui::release_version;

fn donut(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_donut"));
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("DONUT_PROFILE");
    command
}

#[test]
fn test_version_flags_need_no_project_or_config() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["-v", "--version"] {
        // Run from a folder that is no project, with no configuration saved.
        let output = donut(home.path())
            .arg(flag)
            .current_dir(home.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("donut {}\n", release_version())
        );
        assert!(!home.path().join("config").exists());
    }
}

#[test]
fn test_help_lists_the_modes_and_subcommands() {
    let home = tempfile::tempdir().unwrap();
    let output = donut(home.path()).arg("--help").output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    for word in [
        "--plan",
        "--work",
        "--checkpoint",
        "--parallel",
        "setup",
        "discover",
        "add-skills",
    ] {
        assert!(text.contains(word), "{word}");
    }
}

#[test]
fn test_usage_errors_exit_with_two() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["--work", "next"][..],
        &["--plan", "--work"],
        &["--review-rounds", "many", "models"],
    ] {
        let output = donut(home.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn test_the_legacy_launcher_needs_its_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_qwen-build-next"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let output = Command::new(env!("CARGO_BIN_EXE_qwen-build-next"))
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Legacy launcher; prefer donut -m ALIAS --work next COUNT.")
    );
}
