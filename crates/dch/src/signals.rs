//! OS-signal to loopctl cancellation bridge for the `dch` binary.
//!
//! [`install_cancel_handler`] connects SIGINT and SIGTERM to the agent's
//! shared [`CancelSignal`]: the first interrupt asks the loop to stop at
//! its next cooperative check point, and a second interrupt within the
//! coalescing window runs the force hook and exits immediately.

use std::sync::Arc;
use std::time::{Duration, Instant};

use loopctl::cancel::CancelSignal;
use tokio::signal;

/// Serializes tests that deliver real process signals: every installed
/// handler hears every signal, so concurrent signal tests would cancel
/// each other's runs.
#[cfg(test)]
pub(crate) static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    /// A first interrupt, asking the loop for a cooperative stop.
    ///
    /// The bridge trips the agent's cancel signal and arms the repeat
    /// window; the loop itself stops at its next check point and reports
    /// the run as cancelled rather than failed.
    Cancel,

    /// A repeat interrupt inside the window, declining further waiting.
    ///
    /// The force hook runs first — flushing whatever durable state the
    /// host attached — and the process then exits with the interrupt
    /// status instead of waiting out a possibly-stuck turn.
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

/// Persistent interrupt listeners, installed once for the bridge's whole
/// lifetime.
///
/// A signal that arrives while the bridge is between waits must not be
/// lost, so the listeners are not recreated per wait: on Unix both are
/// long-lived signal streams, and a signal delivered at any instant is
/// delivered to the same registration the next wait reads. A listener the
/// platform refuses to install becomes `None`, which never fires — the
/// bridge stays alive on the signals it does have.
struct InterruptListeners {
    /// The Windows Ctrl-C stream, `None` when registration failed.
    ///
    /// Created during [`install`](Self::install) — the platform's console
    /// handler registers on creation, not on first poll — so a Ctrl-C
    /// before the first wait is captured, mirroring the Unix fix.
    #[cfg(windows)]
    interrupt: Option<signal::windows::CtrlC>,

    /// The Ctrl-C stream, `None` when the platform refused the
    /// registration.
    ///
    /// A persistent stream rather than a per-wait future: a signal
    /// delivered between waits lands on this same registration and is
    /// observed by the next [`wait`](Self::wait).
    #[cfg(unix)]
    interrupt: Option<signal::unix::Signal>,

    /// The SIGTERM stream, `None` when the platform refused the
    /// registration.
    ///
    /// Present so a supervisor's graceful-shutdown request takes the same
    /// cooperative path as Ctrl-C instead of the default disposition.
    #[cfg(unix)]
    terminate: Option<signal::unix::Signal>,
}

impl InterruptListeners {
    /// Install the listeners for the bridge's whole lifetime.
    ///
    /// Registration happens here, before the bridge handle is handed to
    /// the caller, so a signal arriving immediately after installation is
    /// already captured. A platform refusal is reported to stderr and
    /// leaves that listener absent rather than failing the install.
    fn install() -> Self {
        #[cfg(unix)]
        {
            let install = |kind| match signal::unix::signal(kind) {
                Ok(stream) => Some(stream),
                Err(error) => {
                    eprintln!("dch: cannot install a signal listener: {error}");
                    None
                }
            };
            Self {
                interrupt: install(signal::unix::SignalKind::interrupt()),
                terminate: install(signal::unix::SignalKind::terminate()),
            }
        }
        #[cfg(windows)]
        {
            Self {
                // Registered synchronously so a Ctrl-C before the first
                // poll is captured, mirroring the Unix fix.
                interrupt: signal::windows::ctrl_c().ok(),
            }
        }
        #[cfg(not(any(unix, windows)))]
        Self {}
    }

    /// Block until the process receives an interrupt.
    ///
    /// Waits on Ctrl-C, and additionally on SIGTERM on Unix so a
    /// supervisor's graceful-shutdown request takes the same cooperative
    /// path. The listeners persist across calls, so no interrupt is lost
    /// to a re-registration gap.
    #[cfg(unix)]
    async fn wait(&mut self) {
        match (self.interrupt.as_mut(), self.terminate.as_mut()) {
            (Some(interrupt), Some(terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
            }
            (Some(interrupt), None) => {
                interrupt.recv().await;
            }
            (None, Some(terminate)) => {
                terminate.recv().await;
            }
            (None, None) => std::future::pending::<()>().await,
        }
    }

    /// Windows waits on the persistently registered Ctrl-C stream.
    #[cfg(windows)]
    async fn wait(&mut self) {
        match self.interrupt.as_mut() {
            Some(interrupt) => {
                interrupt.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    /// Platforms with neither persistent pair nor console stream cannot
    /// listen; the wait parks forever and says so.
    #[cfg(not(any(unix, windows)))]
    async fn wait(&mut self) {
        match signal::ctrl_c().await {
            Ok(()) => {}
            Err(error) => {
                eprintln!("dch: cannot install the Ctrl-C listener: {error}");
                std::future::pending::<()>().await;
            }
        }
    }
}

/// The bridge's interrupt source.
///
/// Production waits on the process's real signal listeners; tests drive
/// the loop through a channel, one item per simulated interrupt, so the
/// decision sequence is deterministic without real signals.
enum InterruptSource {
    /// The process's real listeners, installed once at bridge start.
    ///
    /// Every wait reads the same registration, so interrupts delivered at
    /// any point in the bridge's life are observed in order.
    Listeners(InterruptListeners),

    /// The test stand-in, one channel item per simulated interrupt.
    ///
    /// Present only in test builds; it lets the decision loop be driven
    /// deterministically without delivering real signals to the process.
    #[cfg(test)]
    Channel(ChannelSource),
}

impl InterruptSource {
    /// Block until this source's next interrupt.
    ///
    /// Dispatches to the variant's concrete wait, which for the real
    /// listeners selects across both registered signals. The returned
    /// future is `Send` because the bridge runs on a spawned task.
    async fn wait(&mut self) {
        match self {
            Self::Listeners(listeners) => listeners.wait().await,
            #[cfg(test)]
            Self::Channel(channel) => {
                let _ = channel.rx.recv().await;
            }
        }
    }
}

/// The test interrupt source: a channel receiving one item per interrupt.
///
/// Driving the bridge through the channel makes the decision loop's
/// first-cancel-then-force sequence deterministic without delivering real
/// signals to the test process.
#[cfg(test)]
struct ChannelSource {
    /// One item is received per simulated interrupt.
    ///
    /// The loop's wait resolves each time the test sends across the
    /// paired sender, standing in for the OS signal stream.
    rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

/// How a bridge task ended.
///
/// Distinguished so the caller treats an ordered shutdown differently from
/// a forced exit: only a forced exit terminates the process.
#[derive(Debug, PartialEq, Eq)]
enum BridgeOutcome {
    /// The run's owner stopped the bridge.
    ///
    /// An ordered shutdown: the force hook never ran, and the process
    /// carries on — the caller finalizes the run's own outcome.
    Stopped,

    /// A repeat interrupt landed inside the window and the hook ran.
    ///
    /// The caller must terminate the process with the interrupt status;
    /// everything up to the last cooperative check point is already
    /// durable at this point.
    Forced,
}

/// The interrupt-bridge decision loop, parameterized over the interrupt
/// source so tests can drive it without OS signals.
///
/// Runs until a repeat interrupt lands inside the window — the first
/// interrupt cooperatively cancels via `cancel` and arms the window with
/// the listener still attached, the window's expiry re-arms a fresh
/// first-interrupt, and a repeat inside the window runs `on_force` and
/// yields [`BridgeOutcome::Forced`] — or until the stop signal fires,
/// which yields [`BridgeOutcome::Stopped`].
async fn bridge_loop(
    source: &mut InterruptSource,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    cancel: Arc<CancelSignal>,
    on_force: impl Fn(),
) -> BridgeOutcome {
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
                () = source.wait() => {}
                _ = stop.changed() => return BridgeOutcome::Stopped,
            }
        } else {
            tokio::select! {
                () = source.wait() => {}
                _ = stop.changed() => return BridgeOutcome::Stopped,
            }
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
                return BridgeOutcome::Forced;
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
/// the process with 130. The listener stays armed across the window, so the
/// repeat interrupt is never lost; once the window elapses, a fresh
/// interrupt starts a new cooperative cancellation.
///
/// Call once, after constructing the runner and before awaiting its run,
/// so an early interrupt cannot be missed, and stop the returned bridge
/// when the run ends — a stopped bridge stops listening, so a later run
/// in the same process never inherits an older bridge's window state.
/// The signature carries only the signal and a hook — not the runner — so
/// any mode can install the same bridge against its own runner's signal.
pub fn install_cancel_handler(
    cancel: Arc<CancelSignal>,
    on_force: impl Fn() + Send + 'static,
) -> CancelBridge {
    let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
    let mut source = InterruptSource::Listeners(InterruptListeners::install());
    let task = tokio::spawn(async move {
        let outcome = bridge_loop(&mut source, &mut stop_rx, cancel, on_force).await;
        if outcome == BridgeOutcome::Forced {
            std::process::exit(130);
        }
    });
    CancelBridge { stop, task }
}

/// Install a construction-phase interrupt handler.
///
/// Before the runner exists there is nothing to cancel cooperatively, so
/// any SIGINT or SIGTERM during construction runs `on_interrupt` — a host
/// hook for durable state, such as the done-file marker — and exits the
/// process with 130. Listeners are registered before this function
/// returns, closing the default-disposition window an unaided spawn would
/// have.
///
/// Stop the returned bridge once the full bridge is installed; the two
/// may briefly overlap, where an interrupt fails closed through the
/// construction hook.
pub fn install_construction_handler(on_interrupt: impl Fn() + Send + 'static) -> CancelBridge {
    let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
    let mut source = InterruptSource::Listeners(InterruptListeners::install());
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = stop_rx.changed() => {}
            () = source.wait() => {
                eprintln!("dch: interrupt received during startup — exiting (130)");
                on_interrupt();
                std::process::exit(130);
            }
        }
    });
    CancelBridge { stop, task }
}

/// A handle over the interrupt bridge installed for one run.
///
/// Stopping the bridge ends its task: the listeners unsubscribe, the
/// window state dies with it, and signals after the stop are no longer
/// its to classify. Awaiting [`stop`](Self::stop) waits out the task so a
/// late forced exit can never race the run's own finalization.
#[derive(Debug)]
pub struct CancelBridge {
    /// Signals the bridge task to stop.
    ///
    /// The task selects on this channel beside its interrupt waits; when
    /// this handle drops the channel closes, ending any still-pending
    /// wait the same way.
    stop: tokio::sync::watch::Sender<bool>,

    /// The bridge task itself.
    ///
    /// Awaited by [`stop`](Self::stop) so the run's finalization happens
    /// only after the bridge is quiescent — a forced exit skips the wait
    /// because the process is already terminating.
    task: tokio::task::JoinHandle<()>,
}

impl CancelBridge {
    /// Stop the bridge and wait for its task to finish.
    ///
    /// Only a forced exit skips the wait — by definition the process is
    /// already terminating. A failed send just means the task is already
    /// gone.
    pub async fn stop(self) {
        self.stop.send(true).ok();
        let _ended = self.task.await;
    }
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
        let mut source = InterruptSource::Channel(ChannelSource { rx: received });
        let (_keep_open, mut stop_rx) = tokio::sync::watch::channel(false);
        let bridge = tokio::spawn(async move {
            bridge_loop(&mut source, &mut stop_rx, hook_cancel, move || {
                hook_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await
        });

        let _sent = interrupts.send(());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            cancel.is_cancelled(),
            "the first interrupt must cooperatively cancel"
        );

        let _sent = interrupts.send(());
        let outcome = bridge.await.unwrap();
        assert_eq!(
            outcome,
            BridgeOutcome::Forced,
            "a repeat inside the window forces"
        );
        assert!(
            hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "the repeat inside the window must run the force hook before returning"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_signal_during_startup_is_captured_by_the_installed_bridge() {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cancel = Arc::new(CancelSignal::new());
        let hook_cancel = Arc::clone(&cancel);
        let hook_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_flag = Arc::clone(&hook_fired);
        let bridge = install_cancel_handler(Arc::clone(&cancel), move || {
            hook_cancel.cancel();
            hook_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
        let cancelled = tokio::time::timeout(Duration::from_secs(5), cancel.notified()).await;
        assert!(
            cancelled.is_ok(),
            "a signal arriving immediately after installation must be captured"
        );
        bridge.stop().await;
        assert!(
            !hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "a single interrupt cooperatively cancels and must not force"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn consecutive_signals_are_all_delivered_by_the_persistent_listener() {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut listeners = InterruptListeners::install();

        unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
        let first = tokio::time::timeout(Duration::from_secs(5), listeners.wait()).await;
        assert!(first.is_ok(), "the first signal must arrive");

        // A repeat delivered immediately after the first wait returns: with
        // per-wait listeners this was the lost-signal window.
        unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
        let second = tokio::time::timeout(Duration::from_secs(5), listeners.wait()).await;
        assert!(
            second.is_ok(),
            "a signal arriving right after a wait returns must not be lost"
        );
    }
}
