//! Terminal setup and teardown.
//!
//! The wrapping CLI keeps running after this process exits, so restoring the
//! terminal matters more than usual: a raw-mode terminal left behind corrupts
//! the caller's output and the developer's shell. Restoration therefore runs on
//! every exit path, including a panic.

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout, Write};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn setup() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

/// Puts the terminal back. Safe to call more than once.
pub fn restore() -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    stdout.flush()?;
    Ok(())
}

pub fn set_mouse_capture(enabled: bool) -> Result<()> {
    let mut stdout = io::stdout();
    if enabled {
        execute!(stdout, EnableMouseCapture)?;
    } else {
        execute!(stdout, DisableMouseCapture)?;
    }
    Ok(())
}

/// Restores the terminal *before* the default hook prints, so the panic message
/// lands on a sane screen instead of inside the alternate buffer.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        default_hook(info);
    }));
}

/// Copies text to the system clipboard.
///
/// OSC 52 goes first: it hands the text to the user's own terminal emulator, so
/// it survives this process exiting and works over SSH. On Linux the platform
/// clipboard would otherwise be lost the moment we quit, which for a TUI is
/// most of the time.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;

    // Best effort for terminals that don't honour OSC 52.
    #[cfg(feature = "clipboard-fallback")]
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(text.to_string());
    }
    Ok(())
}
