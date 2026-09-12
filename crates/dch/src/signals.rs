//! OS-signal to loopctl cancellation bridge for the `dch` binary.
//!
//! [`install_cancel_handler`] connects SIGINT and SIGTERM to the agent's
//! shared [`CancelSignal`]: the first interrupt asks the loop to stop at
//! its next cooperative check point, and a second interrupt within the
//! repeat window runs the force hook and exits immediately.

use std::sync::Arc;
use std::time::{Duration, Instant};

use loopctl::cancel::CancelSignal;
#[cfg(any(unix, windows))]
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
    /// host attached — and the process then exits with 130 instead of
    /// waiting out a possibly-stuck turn. The status is fixed rather
    /// than signal-derived: a forced SIGTERM reports the same 130 as a
    /// forced Ctrl-C, matching the module's one-code-per-meaning exit
    /// contract.
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
/// A signal that arrives while the bridge is between waits is captured
/// by the same registration the next wait reads — the listeners are
/// never recreated per wait, so no interrupt is lost to a
/// re-registration gap (the loss mode this structure exists to close).
/// One-event-per-signal is a different guarantee, and tokio's streams
/// provide it on neither platform: notifications arriving before the
/// stream is polled again coalesce into one, so a repeat landing inside
/// that gap merges with its predecessor. A human's double press is
/// spaced far beyond the gap; a script firing two kills back-to-back
/// can surface a single cooperative cancel instead of the forced exit,
/// so a reliable orchestrator force-quit belongs to SIGKILL, not to the
/// double-press hatch. A listener the platform refuses to install
/// becomes `None`, which never fires — the bridge stays alive on the
/// signals it does have.
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
            let install = |kind, name| match signal::unix::signal(kind) {
                Ok(stream) => Some(stream),
                Err(error) => {
                    report(&format!("dch: cannot install the {name} listener: {error}"));
                    None
                }
            };
            Self {
                interrupt: install(signal::unix::SignalKind::interrupt(), "SIGINT"),
                terminate: install(signal::unix::SignalKind::terminate(), "SIGTERM"),
            }
        }
        #[cfg(windows)]
        {
            let interrupt = match signal::windows::ctrl_c() {
                Ok(stream) => Some(stream),
                Err(error) => {
                    report(&format!("dch: cannot install the Ctrl-C listener: {error}"));
                    None
                }
            };
            Self { interrupt }
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
        report(
            "dch: this platform cannot listen for interrupts — the current \
             run cannot be cancelled",
        );
        std::future::pending::<()>().await;
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
                if channel.rx.recv().await.is_none() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

/// The test interrupt source: a channel receiving one item per interrupt.
///
/// Driving the bridge through the channel makes the decision loop's
/// first-cancel-then-force sequence deterministic without delivering real
/// signals to the test process. Items classify at dequeue time, mirroring
/// OS signals processed after a busy stretch: a burst queued in advance
/// classifies back-to-back — and forces — regardless of when it was sent,
/// so a test exercising window expiry must space its sends in real time
/// or pin the classification through [`classify_interrupt`] directly.
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

/// Report a message to stderr without letting a broken stream take the
/// caller down.
///
/// `eprintln!` panics when the write itself fails — a closed or piped
/// fd 2 being ordinary in CI — and such a panic on a terminal path
/// strands the run without its marker or exit code.
pub(crate) fn report(message: &str) {
    use std::io::Write as _;
    drop(writeln!(std::io::stderr(), "{message}"));
}

/// Run a host hook so its panic cannot void the terminal action.
///
/// The hook writes durable state on the way out; whatever it leaves
/// undone, the caller still performs the exit the interrupt promised.
fn run_hook_guarded(hook: impl Fn()) {
    drop(std::panic::catch_unwind(std::panic::AssertUnwindSafe(hook)));
}

/// The interrupt-bridge decision loop, parameterized over the interrupt
/// source so tests can drive it without OS signals.
///
/// Runs until a repeat interrupt lands inside the window — the first
/// interrupt cooperatively cancels via `cancel` and arms the window with
/// the listener still attached, the window's expiry re-arms a fresh
/// first-interrupt, and a repeat inside the window runs `on_force` and
/// yields [`BridgeOutcome::Forced`] — or until the stop signal fires,
/// which yields [`BridgeOutcome::Stopped`]. The selects are biased with
/// the stop request first, so a buffered interrupt — even a repeat that
/// would force — cannot preempt an ordered shutdown and override the
/// outcome the host has already finalized; among the rest, a ready
/// interrupt outranks the window's expiry, so a repeat landing as the
/// window closes still forces instead of being downgraded to a fresh
/// cancellation. The cancel trips before its
/// notice is written, and a panicking hook is contained — a broken
/// stderr or a failing hook cannot strand the run without either
/// exit path.
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
                biased;

                _ = stop.changed() => return BridgeOutcome::Stopped,
                () = source.wait() => {}
                () = window => {
                    previous = None;
                    continue;
                }
            }
        } else {
            tokio::select! {
                biased;

                _ = stop.changed() => return BridgeOutcome::Stopped,
                () = source.wait() => {}
            }
        }
        let now = Instant::now();
        match classify_interrupt(previous, now) {
            InterruptDecision::Cancel => {
                cancel.cancel();
                report(
                    "dch: interrupt received — cancelling the current turn \
                     (repeat the interrupt to quit immediately)",
                );
                previous = Some(now);
            }
            InterruptDecision::Force => {
                report("dch: second interrupt received — forcing exit (130)");
                run_hook_guarded(on_force);
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
/// the process with 130; the hook runs contained, so its panic cannot
/// void the exit. The listener stays armed across the window, so the
/// repeat is never lost to a re-registration gap — though a repeat
/// arriving before the previous interrupt is consumed can coalesce with
/// it (see `InterruptListeners`); once the window elapses, a fresh
/// interrupt starts a new cooperative cancellation.
///
/// Call once, after constructing the runner and before awaiting its run,
/// so an early interrupt cannot be missed, and stop the returned bridge
/// when the run ends — its window state dies with it, so a later run in
/// the same process never inherits an older bridge's state. Install the
/// successor bridge before stopping this one to stay interruptible
/// across the handoff on every platform; on Unix the OS handler
/// additionally stays registered for the runtime's lifetime (the
/// stray-signal test pins the gap's capture-and-discard), while on
/// Windows persistence across a dropped Ctrl-C stream is unverified.
/// The signature carries only the signal
/// and a hook — not the runner — so any mode can install the same
/// bridge against its own runner's signal.
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
/// process with 130; the hook runs contained, so its panic cannot void
/// the exit. Listeners are registered before this function
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
        if construction_loop(&mut source, &mut stop_rx, on_interrupt).await {
            std::process::exit(130);
        }
    });
    CancelBridge { stop, task }
}

/// The construction bridge's single decision step, parameterized over the
/// interrupt source so tests can drive it without OS signals.
///
/// One wait, two exits: an interrupt runs `on_interrupt` and reports that
/// the caller must exit with the interrupt status; a stop request ends
/// the wait and reports an orderly shutdown instead.
/// [`install_construction_handler`] keeps the process exit out of this
/// loop, so the test stand-in never terminates the test process.
async fn construction_loop(
    source: &mut InterruptSource,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    on_interrupt: impl Fn(),
) -> bool {
    tokio::select! {
        _ = stop.changed() => false,
        () = source.wait() => {
            report("dch: interrupt received during startup — exiting (130)");
            run_hook_guarded(on_interrupt);
            true
        }
    }
}

/// A handle over the interrupt bridge installed for one run.
///
/// Stopping the bridge ends its task and drops its listener streams,
/// and the window state dies with it. On Unix, the OS-level handler
/// installed on first registration stays installed for the
/// signal-enabled runtime's lifetime, so a signal arriving in a
/// stop→reinstall gap is captured and discarded rather than allowed to
/// kill by the default disposition — pinned by the stray-signal test;
/// on Windows the console handler's persistence across a dropped
/// Ctrl-C stream is not verified here. A host that must stay
/// interruptible across the gap installs the next bridge before
/// stopping this one — the one handoff that holds on every platform.
/// Awaiting [`stop`](Self::stop) waits out the task so a late forced
/// exit can never race the run's own finalization.
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
    /// gone. An abnormal task end — a panic inside the force hook is the
    /// realistic case, leaving the process alive with the force path
    /// dead — is logged to stderr rather than propagated, so a failed
    /// forced shutdown is at least observable during teardown.
    pub async fn stop(self) {
        self.stop.send(true).ok();
        if let Err(err) = self.task.await {
            report(&format!(
                "dch: the signal bridge task ended abnormally: {err}"
            ));
        }
    }
}

/// Refuse to run a real-signal test without a verified listener.
///
/// `InterruptListeners::install` tolerates a refused registration; a
/// kill with no listener installed takes the default disposition and
/// terminates the whole test binary, so each real-signal test first
/// proves this environment registers listeners at all — a registration
/// that succeeds here succeeds for the test's own install
/// microseconds later.
///
/// # Panics
///
/// When either listener fails to register — the calling test refuses
/// to run rather than default-kill the whole binary.
#[cfg(all(test, unix))]
pub(crate) fn assert_listeners_install() {
    let listeners = InterruptListeners::install();
    assert!(
        listeners.interrupt.is_some() && listeners.terminate.is_some(),
        "signal listeners refused to install — a real-signal kill would \
         terminate the whole test binary"
    );
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

    /// The plumbing a channel-driven `bridge_loop` test exercises.
    ///
    /// Assembled by [`spawn_bridge_loop`]; each field is one handle the
    /// tests drive or observe, so a test body holds only its
    /// distinguishing steps.
    struct BridgeHarness {
        /// Sender standing in for the OS signal stream.
        ///
        /// One queued item models one delivered interrupt; sending is
        /// the test's only way to advance the loop.
        interrupts: tokio::sync::mpsc::UnboundedSender<()>,
        /// The shared cancel signal the first interrupt trips.
        ///
        /// Observed through [`until_cancelled`] to confirm the
        /// cooperative leg before a test exercises the repeat.
        cancel: Arc<CancelSignal>,
        /// Whether the force hook has run.
        ///
        /// Set inside the hook closure itself, so an assertion on it
        /// proves the loop ran the hook before returning.
        hook_fired: Arc<std::sync::atomic::AtomicBool>,
        /// Stop sender for the loop's watch channel.
        ///
        /// Dropping it ends the loop the same way a stop request does,
        /// so a test that never stops must keep the harness alive.
        stop: tokio::sync::watch::Sender<bool>,
        /// Handle to the spawned loop.
        ///
        /// Awaiting it yields the loop's [`BridgeOutcome`], which every
        /// test asserts on.
        bridge: tokio::task::JoinHandle<BridgeOutcome>,
    }

    /// Spawn a `bridge_loop` driven by a channel interrupt source.
    ///
    /// Each test body is left with only its distinguishing steps — the
    /// channel pair, cancel signal, hook-flag plumbing, watch channel, and
    /// task spawn live here.
    fn spawn_bridge_loop() -> BridgeHarness {
        let (interrupts, received) = tokio::sync::mpsc::unbounded_channel::<()>();
        let cancel = Arc::new(CancelSignal::new());
        let hook_cancel = Arc::clone(&cancel);
        let hook_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_flag = Arc::clone(&hook_fired);
        let mut source = InterruptSource::Channel(ChannelSource { rx: received });
        let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
        let bridge = tokio::spawn(async move {
            bridge_loop(&mut source, &mut stop_rx, hook_cancel, move || {
                hook_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await
        });
        BridgeHarness {
            interrupts,
            cancel,
            hook_fired,
            stop,
            bridge,
        }
    }

    /// Wait, bounded, for the first interrupt's cooperative cancel.
    ///
    /// Yielding lets the bridge task process the sent interrupt on the
    /// same runtime; the timeout keeps a stalled schedule from failing the
    /// test where a fixed sleep would have to guess the task's pace.
    async fn until_cancelled(cancel: &Arc<CancelSignal>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first interrupt must cooperatively cancel");
    }

    #[tokio::test]
    async fn a_dropped_channel_sender_parks_the_source_instead_of_interrupting() {
        let (interrupts, received) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut source = InterruptSource::Channel(ChannelSource { rx: received });
        drop(interrupts);
        let decided = tokio::time::timeout(Duration::from_millis(150), source.wait()).await;
        assert!(
            decided.is_err(),
            "a closed source must park like a never-firing listener, not resolve"
        );
    }

    #[tokio::test]
    async fn a_repeat_interrupt_inside_the_window_runs_the_force_hook() {
        let harness = spawn_bridge_loop();

        // Both interrupts are queued before any waiting: the bridge
        // classifies them back-to-back, so the repeat lands inside the
        // window no matter how the scheduler interleaves the tasks.
        let _sent = harness.interrupts.send(());
        let _sent = harness.interrupts.send(());
        until_cancelled(&harness.cancel).await;

        let outcome = tokio::time::timeout(Duration::from_secs(5), harness.bridge)
            .await
            .expect("the bridge must decide within the test timeout")
            .unwrap();
        assert_eq!(
            outcome,
            BridgeOutcome::Forced,
            "a repeat inside the window forces"
        );
        assert!(
            harness.hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "the repeat inside the window must run the force hook before returning"
        );
    }

    #[tokio::test]
    async fn a_panicking_force_hook_still_yields_the_forced_outcome() {
        let (interrupts, received) = tokio::sync::mpsc::unbounded_channel::<()>();
        let cancel = Arc::new(CancelSignal::new());
        let mut source = InterruptSource::Channel(ChannelSource { rx: received });
        let (_keep_open, mut stop_rx) = tokio::sync::watch::channel(false);
        let bridge = tokio::spawn(async move {
            bridge_loop(&mut source, &mut stop_rx, cancel, || {
                panic!("the durable-state hook failed");
            })
            .await
        });
        let _sent = interrupts.send(());
        let _sent = interrupts.send(());
        let outcome = tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .expect("the bridge must decide within the test timeout")
            .unwrap();
        assert_eq!(
            outcome,
            BridgeOutcome::Forced,
            "a panicking hook must not void the forced exit"
        );
    }

    #[tokio::test]
    async fn a_panicking_construction_hook_still_reports_the_interrupt() {
        let (interrupts, received) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut source = InterruptSource::Channel(ChannelSource { rx: received });
        let (_keep_open, mut stop_rx) = tokio::sync::watch::channel(false);
        let _sent = interrupts.send(());
        let interrupted =
            construction_loop(&mut source, &mut stop_rx, || panic!("the hook failed")).await;
        assert!(
            interrupted,
            "a panicking hook must not void the startup exit"
        );
    }

    #[tokio::test]
    async fn a_stop_request_stops_the_bridge_without_forcing() {
        let harness = spawn_bridge_loop();

        let _sent = harness.interrupts.send(());
        until_cancelled(&harness.cancel).await;

        harness.stop.send(true).ok();
        let outcome = tokio::time::timeout(Duration::from_secs(5), harness.bridge)
            .await
            .expect("the bridge must stop within the test timeout")
            .unwrap();
        assert_eq!(
            outcome,
            BridgeOutcome::Stopped,
            "a stop request must end the loop as an orderly stop"
        );
        assert!(
            !harness.hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "an orderly stop must not fire the force hook"
        );
    }

    #[tokio::test]
    async fn a_pending_stop_outranks_a_buffered_repeat_interrupt() {
        let harness = spawn_bridge_loop();

        // Both interrupts are queued before the stop, so when the task is
        // first polled the repeat is ready to force and the stop is ready
        // to end the loop — the stop must win, or an interrupt landing
        // during the host's finalization would exit(130) over the outcome
        // the host just recorded.
        let _sent = harness.interrupts.send(());
        let _sent = harness.interrupts.send(());
        harness.stop.send(true).ok();

        let outcome = tokio::time::timeout(Duration::from_secs(5), harness.bridge)
            .await
            .expect("the bridge must decide within the test timeout")
            .unwrap();
        assert_eq!(
            outcome,
            BridgeOutcome::Stopped,
            "a pending stop outranks a buffered repeat that would force"
        );
        assert!(
            !harness.hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "the buffered repeat must not run the force hook once the stop \
             is pending"
        );
    }

    #[tokio::test]
    async fn a_stop_request_ends_the_construction_loop_without_running_the_hook() {
        let (_interrupts, received) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut source = InterruptSource::Channel(ChannelSource { rx: received });
        let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
        let hook_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_flag = Arc::clone(&hook_fired);
        let run = tokio::spawn(async move {
            construction_loop(&mut source, &mut stop_rx, move || {
                hook_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await
        });

        stop.send(true).ok();
        let interrupted = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("the loop must end within the test timeout")
            .unwrap();
        assert!(
            !interrupted,
            "a stop request is an orderly end, not an interrupt"
        );
        assert!(
            !hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "an orderly stop must not run the interrupt hook"
        );
    }

    #[tokio::test]
    async fn an_interrupt_runs_the_construction_hook_and_reports_the_exit() {
        let (interrupts, received) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut source = InterruptSource::Channel(ChannelSource { rx: received });
        let (_stop, mut stop_rx) = tokio::sync::watch::channel(false);
        let hook_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_flag = Arc::clone(&hook_fired);
        let run = tokio::spawn(async move {
            construction_loop(&mut source, &mut stop_rx, move || {
                hook_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await
        });

        let _sent = interrupts.send(());
        let interrupted = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("the loop must end within the test timeout")
            .unwrap();
        assert!(
            interrupted,
            "an interrupt must be reported so the caller exits with the interrupt status"
        );
        assert!(
            hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "the hook must run before the caller exits"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_signal_during_startup_is_captured_by_the_installed_bridge() {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_listeners_install();
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
    async fn a_stray_interrupt_after_the_bridge_stops_cannot_default_kill() {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_listeners_install();
        let bridge = install_cancel_handler(Arc::new(CancelSignal::new()), || {});
        bridge.stop().await;
        // The registration outlives the bridge: the platform handler is
        // never uninstalled once installed, so a signal arriving after
        // the listeners are gone is captured and discarded rather than
        // reverting to the default disposition. This test's survival —
        // and the harness's — is the assertion.
        unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn consecutive_signals_are_all_delivered_by_the_persistent_listener() {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_listeners_install();
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
