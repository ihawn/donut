//! Port of donut/ui.py. Terminal colors sampled approximately from the supplied palette.

use std::io::IsTerminal;

/// The palette, by name: orange, navy, coral, teal, gray.
pub const COLORS: &[(&str, (u8, u8, u8))] = &[
    ("orange", (255, 168, 24)),
    ("navy", (18, 77, 99)),
    ("coral", (255, 94, 78)),
    ("teal", (23, 184, 190)),
    ("gray", (222, 224, 228)),
];

/// The RGB value of a palette color. Panics on an unknown name, as the Python KeyError did.
pub fn color(name: &str) -> (u8, u8, u8) {
    COLORS.iter().find(|(n, _)| *n == name).map(|(_, rgb)| *rgb).unwrap_or_else(|| panic!("unknown color {name}"))
}

/// The color as "#rrggbb".
pub fn hex(name: &str) -> String {
    let (r, g, b) = color(name);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Text in a palette color when stdout is a terminal and NO_COLOR is not set.
pub fn paint(text: &str, color_name: &str) -> String {
    if !std::io::stdout().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
        return text.to_string();
    }
    let (r, g, b) = color(color_name);
    format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
}

/// The installed release, or "dev".
pub fn release_version() -> String {
    option_env!("DONUT_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")).to_string()
}
