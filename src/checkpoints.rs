//! Port of donut/checkpoints.py.

/// Folders never searched for nested repositories or source files.
pub const SKIP: &[&str] = &["node_modules", "venv", "env", "__pycache__", "dist", "build", "target", "swim_lanes"];
