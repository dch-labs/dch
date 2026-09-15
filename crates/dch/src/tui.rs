//! Interactive TUI runner: load config, build the agent behind a
//! streaming display, drive submissions until the user quits.
//!
//! This is the dispatch target for plain `dch`. It owns no agent-loop
//! logic — everything turn/stream/dispatch lives in loopctl via the
//! runner — and no rendering beyond the app it hosts: the mode's own
//! work is the plumbing that couples submitted input lines to agent
//! runs, routes run failures into the display, and tears everything
//! down when the user quits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dch_tui::{TerminalGuard, TuiApp, TuiObserverState};
use tokio::sync::mpsc;

use crate::args::Args;
use crate::headless::{apply_cli_overrides, load_config};

/// Run the interactive session and return the process exit code.
///
/// Loads config, builds a streaming runner with the TUI observer
/// attached, initializes the terminal, and hosts the app's event loop
/// until the user quits. Submissions travel the app's submit channel
/// to a driver task that owns the runner and executes them strictly
/// one at a time; a failed run surfaces as an error message in the
/// conversation instead of ending the session. Quitting cancels an
/// in-flight run, stops the driver from starting any submissions
/// still queued behind it, waits for the driver, and restores the
/// terminal.
///
/// # Errors
///
/// Returns the exit code for any failure: 1 for construction or
/// session-level failures, 0 for a clean quit.
pub async fn run_tui(args: &Args) -> u8 {
    match run_tui_session(args).await {
        Ok(()) => 0,
        Err(message) => {
            crate::signals::report(&format!("dch: {message}"));
            1
        }
    }
}

/// Host one interactive session end to end.
///
/// Every construction step — config, working directory, agent,
/// terminal — runs before the alternate screen is entered, so a
/// failure reports to the user's normal terminal. Draw failures after
/// the session started are reported only once the guard has restored
/// the terminal, so the message never lands inside raw mode.
///
/// # Errors
///
/// Returns the user-facing failure message for any construction or
/// session-level failure; the caller maps it to the exit code.
async fn run_tui_session(args: &Args) -> Result<(), String> {
    let mut config = load_config(args.config.config_path.as_deref())?;
    apply_cli_overrides(&mut config, args);

    let workdir = std::env::current_dir().map_err(|err| format!("cannot determine cwd: {err}"))?;

    let (observer, state) = TuiObserverState::new().into_observer();
    let runner = dch_loop::Runner::builder(&config, &workdir)
        .with_observer(Arc::new(observer) as Arc<dyn loopctl::observer::LoopObserver>)
        .build()
        .await
        .map_err(|err| format!("agent construction: {err}"))?;

    let (submit_tx, submit_rx) = mpsc::unbounded_channel();
    let mut app = TuiApp::from_observer_state(config, state.clone());
    app.set_submit_tx(submit_tx);

    TerminalGuard::install_panic_hook();
    let (guard, mut terminal) =
        TerminalGuard::new().map_err(|err| format!("terminal init: {err}"))?;

    let cancel = runner.cancel_signal();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let driver = tokio::spawn(drive_submissions(
        runner,
        submit_rx,
        state,
        Arc::clone(&shutting_down),
    ));

    let session = app.run(&mut terminal).await;
    drop(app);
    shutting_down.store(true, Ordering::SeqCst);
    cancel.cancel();
    drop(driver.await);
    drop(guard);
    session.map_err(|err| format!("terminal error: {err}"))
}

/// Execute submitted tasks one at a time until the session ends.
///
/// Owns the runner for the session's whole lifetime, so runs never
/// overlap: each submitted line runs to completion or failure before
/// the next begins, preserving the order the user submitted in. A
/// failed run records its error into the shared buffer the display
/// drains and wakes the frame — the session continues, and the next
/// submission still runs. The loop ends when the channel closes or
/// the shutdown flag is set: the engine clears its cancel signal at
/// the end of every run, so the flag — raised before the cancel — is
/// what keeps submissions queued behind a quitting user from ever
/// starting.
async fn drive_submissions(
    mut runner: dch_loop::Runner,
    mut receiver: mpsc::UnboundedReceiver<String>,
    state: TuiObserverState,
    shutting_down: Arc<AtomicBool>,
) {
    while let Some(text) = receiver.recv().await {
        if let Err(err) = runner.run(&text).await {
            state
                .errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(err.to_string());
            state.render_notify.notify(1);
        }
        if shutting_down.load(Ordering::SeqCst) {
            break;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use dch_config::ApiType;

    /// A config whose provider endpoint refuses connections on the
    /// loopback interface, so a real run fails fast without touching
    /// the network.
    fn unreachable_config() -> dch_config::DchConfig {
        let mut config = dch_config::DchConfig::default();
        config.api.api_type = ApiType::OpenAi;
        config.api.base_url = "http://127.0.0.1:1".to_string();
        config.api.api_key = Some("dummy".to_string());
        config.api.model = "test-model".to_string();
        config
    }

    /// Build a real runner against the unreachable endpoint.
    ///
    /// Construction is offline — the builder only assembles the
    /// provider client; the first request, and with it the failure,
    /// happens inside `run`.
    async fn unreachable_runner() -> dch_loop::Runner {
        let config = unreachable_config();
        let dir = tempfile::tempdir().expect("a temp workdir");
        dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction")
    }

    /// A driver shutdown flag in the given state.
    fn shutdown_flag(set: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(set))
    }

    #[tokio::test]
    async fn a_failing_run_routes_its_error_into_the_shared_buffer() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("do a thing".to_string()).unwrap();
        drop(tx);
        let listener = state.render_notify.listen();

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(runner, rx, state.clone(), shutdown_flag(false)),
        )
        .await
        .expect("a refused connection resolves without hanging");

        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(errors.len(), 1, "exactly one failure is recorded");
            assert!(!errors[0].is_empty(), "the failure carries its message");
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener)
                .await
                .is_ok(),
            "the driver wakes the frame, so the error row renders without a keypress"
        );
    }

    #[tokio::test]
    async fn the_driver_ends_when_the_channel_closes() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);

        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            drive_submissions(runner, rx, state, shutdown_flag(false)),
        )
        .await;
        assert!(ended.is_ok(), "a closed channel ends the driver promptly");
    }

    #[tokio::test]
    async fn queued_submissions_do_not_run_after_shutdown() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("first".to_string()).unwrap();
        tx.send("second".to_string()).unwrap();
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(runner, rx, state.clone(), shutdown_flag(true)),
        )
        .await
        .expect("the driver leaves the queue behind instead of draining it");

        let errors = state.errors.lock().unwrap();
        assert_eq!(
            errors.len(),
            1,
            "only the in-flight run executed; the submission queued behind shutdown never started"
        );
    }
}
