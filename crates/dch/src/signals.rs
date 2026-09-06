//! OS-signal to loopctl cancellation bridge for the `dch` binary.
//!
//! [`install_cancel_handler`] connects SIGINT and SIGTERM to the agent's
//! shared [`CancelSignal`]: the first interrupt asks the loop to stop at
//! its next cooperative check point, and a second interrupt within the
//! coalescing window runs the force hook and exits immediately.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loopctl::cancel::CancelSignal;
use tokio::signal;

/// The window after a first interrupt inside which a repeat interrupt
/// forces an immediate exit.
///
/// A UX choice, not a correctness bound: after the window elapses the
/// handler treats a fresh interrupt as a new first one.
const DOUBLE_CTRL_C_WINDOW: Duration = Duration::from_secs(2);

/// What an arriving interrupt means for the run.
///
/// Separated from the OS edge so the decision rule is testable without
/// delivering a real signal.
#[derive(Debug, PartialEq, Eq)]
enum InterruptDecision {
    /// A first interrupt: cancel the in-flight turn cooperatively.
    Cancel,

    /// A repeat interrupt: the user has declined to wait for the
    /// cooperative check points; run the force hook and exit now.
    Force,
}

/// Classify an interrupt by the time since the previous one.
///
/// The classification is time-based because the engine resets the cancel
/// signal at the end of every run, so the signal's own state cannot carry
/// first-vs-repeat across a process lifetime — only the handler's clock
/// can.
fn classify_interrupt(previous: Option<Instant>, now: Instant) -> InterruptDecision {
    match previous {
        Some(previous) if now.duration_since(previous) <= DOUBLE_CTRL_C_WINDOW => {
            InterruptDecision::Force
        }
        _ => InterruptDecision::Cancel,
    }
}

/// Block until the process receives an interrupt.
///
/// Waits on Ctrl-C, and additionally on SIGTERM on Unix so a supervisor's
/// graceful-shutdown request takes the same cooperative path. A failure to
/// install a listener is reported and parks forever rather than returning,
/// which would spin the caller's loop.
async fn wait_for_interrupt() {
    #[cfg(unix)]
    {
        let sigterm = async {
            match signal::unix::signal(signal::unix::SignalKind::terminate()) {
                Ok(mut stream) => {
                    let _ = stream.recv().await;
                }
                Err(error) => {
                    eprintln!("dch: cannot install the SIGTERM listener: {error}");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            result = signal::ctrl_c() => {
                if let Err(error) = result {
                    eprintln!("dch: cannot install the Ctrl-C listener: {error}");
                    std::future::pending::<()>().await;
                }
            }
            () = sigterm => {}
        }
    }
    #[cfg(not(unix))]
    {
        match signal::ctrl_c().await {
            Ok(()) => {}
            Err(error) => {
                eprintln!("dch: cannot install the Ctrl-C listener: {error}");
                std::future::pending::<()>().await;
            }
        }
    }
}

/// The interrupt-bridge decision loop, parameterized over the interrupt
/// source so tests can drive it without OS signals.
///
/// Runs until a repeat interrupt lands inside the window: the first
/// interrupt cooperatively cancels via `cancel` and arms the window with
/// the listener still attached, the window's expiry re-arms a fresh
/// first-interrupt, and a repeat inside the window runs `on_force` and
/// returns.
async fn bridge_loop<W, F>(mut wait: F, cancel: Arc<CancelSignal>, on_force: impl Fn())
where
    F: FnMut() -> W,
    W: Future<Output = ()>,
{
    let mut previous: Option<Instant> = None;
    loop {
        if let Some(then) = previous {
            let window = tokio::time::sleep(
                then.checked_add(DOUBLE_CTRL_C_WINDOW)
                    .map_or(DOUBLE_CTRL_C_WINDOW, |deadline| {
                        deadline.saturating_duration_since(Instant::now())
                    }),
            );
            tokio::select! {
                () = window => {
                    previous = None;
                    continue;
                }
                () = wait() => {}
            }
        } else {
            wait().await;
        }
        let now = Instant::now();
        match classify_interrupt(previous, now) {
            InterruptDecision::Cancel => {
                eprintln!(
                    "dch: interrupt received — cancelling the current turn \
                     (press Ctrl-C again to quit immediately)"
                );
                cancel.cancel();
                previous = Some(now);
            }
            InterruptDecision::Force => {
                eprintln!("dch: second interrupt received — forcing exit (130)");
                on_force();
                return;
            }
        }
    }
}

/// Install the SIGINT/SIGTERM bridge to the agent's shared
/// [`CancelSignal`].
///
/// Spawns one background task on the current tokio runtime and returns
/// immediately. The first interrupt cancels the in-flight turn
/// cooperatively — the loop stops at its next check point and `run`
/// returns `LoopError::Cancelled`, which the host maps to exit code 130. A
/// second interrupt within [`DOUBLE_CTRL_C_WINDOW`] runs `on_force` — a
/// host hook for durable state, such as the done-file marker — and exits
/// with 130 directly. The listener stays armed across the window, so the
/// repeat interrupt is never lost; once the window elapses, a fresh
/// interrupt starts a new cooperative cancellation.
///
/// Call once, after constructing the runner and before awaiting its run,
/// so an early interrupt cannot be missed. The signature carries only the
/// signal and a hook — not the runner — so any mode can install the same
/// bridge against its own runner's signal.
pub fn install_cancel_handler(cancel: Arc<CancelSignal>, on_force: impl Fn() + Send + 'static) {
    tokio::spawn(async move {
        bridge_loop(wait_for_interrupt, cancel, on_force).await;
        std::process::exit(130);
    });
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]
mod tests {
    use super::*;

    #[test]
    fn a_first_interrupt_classifies_as_cancel() {
        let now = Instant::now();
        assert_eq!(
            classify_interrupt(None, now),
            InterruptDecision::Cancel,
            "no previous interrupt means this is a first one"
        );
    }

    #[test]
    fn an_interrupt_inside_the_window_classifies_as_force() {
        let now = Instant::now();
        let previous = now.checked_sub(Duration::from_millis(500)).unwrap_or(now);
        assert_eq!(
            classify_interrupt(Some(previous), now),
            InterruptDecision::Force,
            "a repeat inside the window forces"
        );
    }

    #[test]
    fn an_interrupt_after_the_window_classifies_as_cancel() {
        let now = Instant::now();
        let previous = now.checked_sub(Duration::from_secs(3)).unwrap_or(now);
        assert_eq!(
            classify_interrupt(Some(previous), now),
            InterruptDecision::Cancel,
            "past the window an interrupt starts a fresh cancellation"
        );
    }

    #[test]
    fn the_coalescing_window_is_two_seconds() {
        assert_eq!(
            DOUBLE_CTRL_C_WINDOW,
            Duration::from_secs(2),
            "the window is a deliberate, single-point tunable"
        );
    }

    #[tokio::test]
    async fn a_repeat_interrupt_inside_the_window_runs_the_force_hook() {
        let (interrupts, received) = tokio::sync::mpsc::unbounded_channel::<()>();
        let cancel = Arc::new(CancelSignal::new());
        let hook_cancel = Arc::clone(&cancel);
        let hook_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_flag = Arc::clone(&hook_fired);
        let received = Arc::new(tokio::sync::Mutex::new(received));
        let bridge = tokio::spawn(async move {
            let wait = || {
                let received = Arc::clone(&received);
                async move {
                    received.lock().await.recv().await;
                }
            };
            bridge_loop(wait, hook_cancel, move || {
                hook_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
        });

        let _sent = interrupts.send(());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            cancel.is_cancelled(),
            "the first interrupt must cooperatively cancel"
        );

        let _sent = interrupts.send(());
        bridge.await.unwrap();
        assert!(
            hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "the repeat inside the window must run the force hook before returning"
        );
    }
}
