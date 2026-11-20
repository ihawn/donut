# Rust port rules (temporary; deleted before the PR merges)

Goal: replace the Python package `donut/` with a Rust crate that has the same behaviour, the same files on disk,
the same CLI flags and the same terminal UI. A user who upgrades must keep their config, conversations, tickets
and logs. The Python code stays in the tree until the port is done, as the reference. Do not edit it.

## Layout

- `donut/<name>.py` -> `src/<name>.rs`. One Rust module per Python module. Keep the Python names in snake_case
  (functions, constants), CamelCase for classes. Private helpers (`_name`) become private fns without the `_`.
- `tests/test_<name>.py` -> a `#[cfg(test)] mod tests` at the end of the matching `src/<name>.rs`, or
  `tests/<name>.rs` for tests that run the `donut` binary or fake provider executables. Port every test case;
  the test name stays the Python method name without `test_` prefix duplication (`fn test_foo` is fine).
- Skills are in `skills/` (moved from `donut/skills/`); embed them with `include_str!`.
- `src/main.rs` calls `donut::cli::main()`; `src/bin/qwen-build-next.rs` calls `donut::legacy::main()`.

## Types

- Config, profiles and any JSON read from disk or a provider: `serde_json::Value` (objects keep key order;
  `preserve_order` is on). A profile is `&serde_json::Value` (an object). Do not invent structs for
  user-editable JSON: unknown keys must survive a load and save.
- Paths: `&Path` in, `PathBuf` out. Errors: `anyhow::Result` where the Python raised; plain values otherwise.
- Python `None` -> `Option`. Keyword arguments with defaults -> trailing `Option`/`bool` params or a small
  options struct with `Default`.
- Regexes: `std::sync::LazyLock<regex::Regex>`. Python `re` features that `regex` lacks (lookaround,
  backreferences) -> rewrite the logic, or add `fancy-regex` only if a rewrite is unsafe.
- Processes: `std::process::Command`; call `platform::process_options(&mut cmd)` where Python passed
  `**process_options()`. Threads, not async, unless a module is clearly better with async.

## Shared pieces already written (use them, extend the files, do not rename)

- `ui::{COLORS, color, hex, paint, release_version}`
- `config::{profile_name, config_path, state_dir}`
- `platform::process_options`
- `providers::{provider_family, native_permission, NativePermission}`
- `checkpoints::SKIP`

## Dependencies

Add crates to `Cargo.toml` as needed; prefer well-known ones (ratatui, crossterm, pulldown-cmark, syntect,
ureq, uuid, sha2, csv, base64, chrono, shlex, which, unicode-width). Do not remove a crate someone else added.

## Done means

`cargo build`, `cargo test` and `cargo clippy --all-targets -- -D warnings` pass for your modules, with every
ported test. Other people work in this tree at the same time: do not edit modules you do not own, and if
their in-progress code breaks the build, wait a minute and retry instead of changing it.
