//! Terminal lifecycle for full-screen TUI sessions.
//!
//! Raw mode and the alternate screen are session-wide state a TUI
//! must own for its whole run; the guard here makes teardown happen
//! on every exit path, including panics, so the user's terminal is
//! never left behind in a broken mode.

use std::io::{self, Stdout};

use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// Initialize the terminal for a full-screen TUI session.
///
/// Enables raw mode and enters the alternate screen, returning a
/// terminal bound to stdout. Pair with [`restore_terminal`] — or hold
/// a [`TerminalGuard`] so the pairing is automatic. A failure after
/// raw mode was enabled undoes the partial setup — raw mode off
/// first, the alternate screen best-effort — before returning the
/// error, so an unwritable stdout cannot strand the shell in raw
/// mode.
///
/// # Errors
/// Fails when the terminal mode or escape-sequence writes are
/// rejected, most commonly because stdout is not a terminal.
pub fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let initialized = (|| {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend)
    })();
    if initialized.is_err()
        && let Err(restore_err) = restore_terminal()
    {
        tracing::warn!("terminal restore after a failed init failed: {restore_err}");
    }
    initialized
}

/// Restore the terminal after a TUI session.
///
/// Disables raw mode first and leaves the alternate screen second:
/// raw mode is the state that breaks the user's shell, so its undo
/// never runs behind an escape-sequence write that can fail on an
/// unwritable stdout. Both attempts always run; the raw-mode error,
/// when present, is the one returned. Safe to call repeatedly and
/// safe when the terminal was never initialized — the escape
/// sequences are ignored by a terminal not in those modes.
///
/// # Errors
/// Fails only when the mode change or the escape-sequence write is
/// rejected by the underlying terminal.
pub fn restore_terminal() -> io::Result<()> {
    let raw_result = disable_raw_mode();
    let alt_result = execute!(io::stdout(), LeaveAlternateScreen);
    raw_result.and(alt_result)
}

/// RAII ownership of an initialized TUI terminal.
///
/// Constructing the guard initializes the terminal (see
/// [`init_terminal`]); dropping it restores the terminal, so every
/// exit path — early return, error propagation, unwind — leaves the
/// user's terminal usable.
pub struct TerminalGuard {
    /// Marker that keeps the guard unconstructible outside
    /// [`new`](Self::new).
    ///
    /// The guard owns no state — its drop behavior is the value.
    _private: (),
}

impl TerminalGuard {
    /// Initialize the terminal and take ownership of its restoration.
    ///
    /// Returns the guard together with the terminal to draw with;
    /// keep the guard alive for the whole session.
    ///
    /// # Errors
    /// Propagates [`init_terminal`]'s failure — nothing was changed, so
    /// there is nothing to restore.
    pub fn new() -> io::Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        let terminal = init_terminal()?;
        Ok((Self { _private: () }, terminal))
    }

    /// Install the panic hook that restores the terminal before unwinding.
    ///
    /// A panic unwinds through frames that may sit above any guard
    /// (or inside spawned tasks the guard never sees); the hook
    /// restores first so the backtrace prints to a sane terminal.
    /// Call exactly once at startup. Chained hooks are preserved.
    pub fn install_panic_hook() {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            if let Err(err) = restore_terminal() {
                tracing::warn!("terminal restore during panic failed: {err}");
            }
            previous_hook(panic_info);
        }));
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Err(err) = restore_terminal() {
            tracing::warn!("terminal restore failed: {err}");
        }
    }
}
