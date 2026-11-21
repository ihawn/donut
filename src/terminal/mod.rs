//! Donut's interactive terminal, backed by provider-owned sessions.
//!
//! Layout: [`render`] holds the pure helpers (formatting, Markdown, approvals, constants), [`widgets`] the
//! Transcript, Menu and Draft widgets, [`app`] the `App` state and its logic, and [`commands`] the slash
//! commands. The CLI calls [`run`].

pub mod app;
pub mod commands;
pub mod draw;
pub mod flows;
pub mod input;
pub mod render;
#[cfg(test)]
mod tests;
pub mod widgets;

pub use app::{AppOptions, State};
pub use flows::App;

use ratatui::text::Line;

/// A link inside a line: a range of characters (not bytes) of the line's plain text and where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// A styled line plus the links in it (Rich's `Style(link=...)`, which ratatui styles cannot carry).
#[derive(Clone, Debug, Default)]
pub struct RichLine {
    pub line: Line<'static>,
    pub links: Vec<Link>,
}

impl RichLine {
    /// The line's text without styles.
    pub fn plain(&self) -> String {
        self.line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The URL under character column `column` of the plain text, if any.
    pub fn link_at(&self, column: usize) -> Option<&str> {
        self.links
            .iter()
            .find(|link| link.start <= column && column < link.end)
            .map(|link| link.url.as_str())
    }
}

impl From<Line<'static>> for RichLine {
    fn from(line: Line<'static>) -> Self {
        RichLine {
            line,
            links: Vec::new(),
        }
    }
}

/// `run_terminal(project, alias, session)`: open the interactive terminal and return the exit code.
pub fn run(
    project: &std::path::Path,
    alias: Option<&str>,
    session: Option<&str>,
) -> anyhow::Result<i32> {
    use std::io::{IsTerminal, Write};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use ratatui::crossterm::event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };
    use ratatui::crossterm::execute;
    use ratatui::crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    };

    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "Interactive Donut requires a terminal. Use --plan REQUEST or --work next COUNT in scripts."
        );
    }
    let app = App::new(project, alias, session)?;
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    let enter = move || -> std::io::Result<()> {
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        if enhanced {
            execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        Ok(())
    };
    let leave = move || {
        let mut out = std::io::stdout();
        if enhanced {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            out,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    };
    enter()?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        hook(info);
    }));
    app.set_terminal_hook(Arc::new(move |back| {
        if back {
            let _ = enter();
        } else {
            leave();
        }
    }));
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    app.start();
    let mut last_quota = Instant::now();
    let result: anyhow::Result<()> = (|| {
        loop {
            let out: Vec<String> = std::mem::take(&mut app.st().terminal_out);
            for text in out {
                write!(terminal.backend_mut(), "{text}")?;
            }
            terminal.backend_mut().flush()?;
            if app.0.suspended.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            terminal.draw(|frame| app.draw(frame))?;
            if app.st().quit {
                return Ok(());
            }
            if event::poll(Duration::from_millis(50))? {
                loop {
                    app.handle_event(event::read()?);
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            if last_quota.elapsed() >= Duration::from_secs(app::QUOTA_REFRESH_SECONDS)
                && app.st().options.auto_refresh_quotas
            {
                last_quota = Instant::now();
                app.refresh_quotas_soon(true);
            }
        }
    })();
    let out: Vec<String> = std::mem::take(&mut app.st().terminal_out);
    for text in out {
        let _ = write!(terminal.backend_mut(), "{text}");
    }
    let _ = terminal.backend_mut().flush();
    drop(terminal);
    execute!(std::io::stdout(), PopKeyboardEnhancementFlags).ok();
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    app.close_all_sessions();
    result?;
    Ok(0)
}
