//! Interactive TUI runner: load config, build the agent behind a
//! streaming display, drive submissions until the user quits.
//!
//! This is the dispatch target for plain `dch`. It owns no agent-loop
//! logic — everything turn/stream/dispatch lives in loopctl via the
//! runner — and no rendering beyond the app it hosts: the mode's own
//! work is the plumbing that couples submitted input lines to agent
//! runs, routes run failures into the display, and tears everything
//! down when the user quits.

use self::session::run_tui_session;
use crate::args::Args;
use crate::resume::ResumeControl;

mod commands;
mod corrections;
mod driver;
mod permissions_bridge;
mod rebuilds;
mod session;
#[cfg(test)]
mod test_support;
mod transcript;

/// Run the interactive session and return the process exit code.
///
/// Loads config, builds a streaming runner with the TUI observer
/// attached, initializes the terminal, and hosts the app's event loop
/// until the user quits — continuing from `control` when it carries a
/// resumed session: the restored transcript seeds the display, the
/// agent's history, and the file further auto-saves write to.
/// Submissions travel the app's submit channel to a driver task that
/// owns the runner and executes them strictly one at a time; a failed
/// run surfaces as an error message in the conversation instead of
/// ending the session. Quitting cancels an in-flight run, stops the
/// driver from starting any submissions still queued behind it,
/// waits for the driver, and restores the terminal.
///
/// # Errors
///
/// Returns the exit code for any failure: 1 for construction or
/// session-level failures, 0 for a clean quit.
pub async fn run_tui(args: &Args, control: ResumeControl) -> u8 {
    match Box::pin(run_tui_session(args, control)).await {
        Ok(()) => 0,
        Err(message) => {
            crate::signals::report(&format!("dch: {message}"));
            1
        }
    }
}
