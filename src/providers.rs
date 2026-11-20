//! Port of donut/providers.py. Provider CLIs own authentication and execution; no credentials in config.

/// "qwen" for ollama and openai engines, else the engine itself.
pub fn provider_family(engine: &str) -> &str {
    if engine == "ollama" || engine == "openai" { "qwen" } else { engine }
}

/// A provider's own form of a Donut permission mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePermission {
    /// Claude Code --permission-mode or Qwen Code --approval-mode value.
    Mode(&'static str),
    /// Codex app-server (approvalPolicy, sandbox, approvalsReviewer).
    Codex { approval: &'static str, sandbox: &'static str, reviewer: &'static str },
}

/// The provider's own value for a Donut permission mode (ask, accept_edits, auto, full).
pub fn native_permission(engine: &str, mode: &str) -> NativePermission {
    use NativePermission::{Codex, Mode};
    match (provider_family(engine), mode) {
        ("claude", "ask") => Mode("default"),
        ("claude", "accept_edits") => Mode("acceptEdits"),
        ("claude", "auto") => Mode("auto"),
        ("claude", "full") => Mode("bypassPermissions"),
        ("qwen", "ask") => Mode("default"),
        ("qwen", "accept_edits") => Mode("auto-edit"),
        ("qwen", "auto") => Mode("auto"),
        ("qwen", "full") => Mode("yolo"),
        ("codex", "ask") => Codex { approval: "untrusted", sandbox: "workspace-write", reviewer: "user" },
        ("codex", "accept_edits") => Codex { approval: "on-request", sandbox: "workspace-write", reviewer: "user" },
        ("codex", "auto") => Codex { approval: "on-request", sandbox: "workspace-write", reviewer: "auto_review" },
        ("codex", "full") => Codex { approval: "never", sandbox: "danger-full-access", reviewer: "user" },
        (family, mode) => panic!("no native permission for {family} {mode}"),
    }
}
