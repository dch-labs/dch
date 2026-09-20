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

use dch_tui::TuiMessage;
use dch_tui::{TerminalGuard, TuiApp, TuiObserverState};
use tokio::sync::mpsc;

use crate::args::Args;
use crate::headless::{apply_cli_overrides, load_config};
use crate::resume::ResumeControl;

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
    match run_tui_session(args, control).await {
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
/// the terminal, so the message never lands inside raw mode. A
/// resumed control shapes each step: the saved model applies unless
/// the CLI overrides it, the restored conversation seeds both the
/// agent's history and the display, and the session id is the loaded
/// one so auto-saves keep writing the resumed file.
///
/// # Errors
///
/// Returns the user-facing failure message for any construction or
/// session-level failure; the caller maps it to the exit code.
async fn run_tui_session(args: &Args, control: ResumeControl) -> Result<(), String> {
    if let ResumeControl::Fresh {
        warn: Some(warn), ..
    } = &control
    {
        crate::signals::report(&format!("dch: {warn}"));
    }
    let mut config = load_config(args.config.config_path.as_deref())?;
    apply_cli_overrides(&mut config, args);
    if let ResumeControl::Resumed(outcome) = &control {
        crate::resume::apply_resumed_model(&mut config, args, outcome);
    }

    let workdir = std::env::current_dir().map_err(|err| format!("cannot determine cwd: {err}"))?;

    let (observer, state) = TuiObserverState::new().into_observer();
    if let ResumeControl::Resumed(outcome) = &control {
        let mut tokens = state
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens.cumulative_input = outcome.tokens.cumulative_input;
        tokens.cumulative_output = outcome.tokens.cumulative_output;
    }
    let mut builder = dch_loop::Runner::builder(&config, &workdir)
        .with_observer(Arc::new(observer) as Arc<dyn loopctl::observer::LoopObserver>)
        .with_middleware(Arc::new(dch_tui::CapturingMiddleware::new(Arc::clone(
            &state.tool_captures,
        ))));
    if let ResumeControl::Resumed(outcome) = &control {
        builder = builder.with_history(crate::resume::tui_messages_to_loopctl(
            &outcome.messages,
            config.security.redact_secrets,
        ));
    }
    let runner = builder
        .build()
        .await
        .map_err(|err| format!("agent construction: {err}"))?;

    // Rebuild the read-before-write guard's baselines for the files
    // the restored transcript shows being read: without this, a
    // resumed Write treats every previously-read file as never-read.
    if let ResumeControl::Resumed(outcome) = &control {
        for path in crate::resume::resumed_read_paths(&outcome.messages) {
            let _recorded = runner.context().record_resumed_read(&path).await;
        }
    }

    let (submit_tx, submit_rx) = mpsc::unbounded_channel();
    let model = config.api.model.clone();
    let mouse_capture = config.display.mouse_capture;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    app.set_submit_tx(submit_tx);

    // Seed the display from the control — the restored transcript, or
    // the note that a resume degraded to this fresh session — and take
    // the id the saver writes under: the resumed file's own identity.
    let session_id = match control {
        ResumeControl::Resumed(outcome) => {
            app.seed_messages(outcome.messages);
            outcome.session_id
        }
        ResumeControl::Fresh {
            session_id,
            warn: Some(warn),
        } => {
            app.push_message(TuiMessage::System {
                text: warn,
                timestamp: chrono::Utc::now(),
            });
            session_id.unwrap_or_else(|| runner.session_id())
        }
        ResumeControl::Fresh {
            session_id,
            warn: None,
        } => session_id.unwrap_or_else(|| runner.session_id()),
    };
    app.set_session_id(session_id.to_string());

    // Save the transcript off the render thread whenever a turn
    // ends: one ordered writer receives the snapshots, so the newest
    // transcript lands last and teardown joins the final write.
    let saver = Arc::new(crate::session::SessionSaver::with_model(session_id, model));
    let (transcript_handle, transcript_worker) = TranscriptWorker::spawn(saver);
    let hook_tokens = Arc::clone(&state.tokens);
    app.set_turn_end_hook(Box::new(move |conversation| {
        let (cumulative_input, cumulative_output, last_input_tokens) =
            hook_tokens.lock().map_or((0, 0, 0), |counts| {
                (
                    counts.cumulative_input,
                    counts.cumulative_output,
                    counts.input,
                )
            });
        transcript_handle.send(
            conversation.to_vec(),
            crate::session::SessionTokens {
                cumulative_input,
                cumulative_output,
                last_input_tokens,
            },
        );
    }));

    TerminalGuard::install_panic_hook();
    let (guard, mut terminal) =
        TerminalGuard::new(mouse_capture).map_err(|err| format!("terminal init: {err}"))?;
    // Render draws the grid on the terminal's default background;
    // this points that default at the theme's canvas color for the
    // session's lifetime, so the margin around the grid matches it.
    dch_tui::sync_default_background(app.theme.ui.background);

    let cancel = runner.cancel_signal();
    {
        let cancel_for_app = Arc::clone(&cancel);
        app.set_run_canceller(Box::new(move || cancel_for_app.cancel()));
    }
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
    transcript_worker.join();
    drop(guard);
    session.map_err(|err| format!("terminal error: {err}"))
}

/// The handle a turn-end hook uses to hand transcripts to the writer.
///
/// Holds the shared one-slot mailbox: publishing a snapshot
/// overwrites whatever is still waiting, so at most one pending
/// transcript is ever retained — a stalled write cannot pile
/// snapshots up behind it. Publishing is all the hook does — cheap
/// enough to run inline on the render task.
struct TranscriptHandle {
    slot: Arc<std::sync::Mutex<Option<TranscriptSnapshot>>>,
    signal: Arc<std::sync::Condvar>,
}

impl TranscriptHandle {
    /// Publish one conversation snapshot for the writer.
    ///
    /// Overwrites a still-pending snapshot — only the newest
    /// transcript is ever worth writing — then wakes the writer.
    /// Never blocks and never fails visibly.
    fn send(&self, messages: Vec<TuiMessage>, tokens: crate::session::SessionTokens) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(TranscriptSnapshot { messages, tokens });
        drop(slot);
        self.signal.notify_one();
    }
}

/// One turn-end snapshot: the conversation and its accounting.
///
/// What the hook publishes and the writer persists together, so the
/// newest file always pairs the transcript with the totals the
/// status bar showed when it was taken — the two halves of a
/// session's state that a resume restores as one.
struct TranscriptSnapshot {
    /// The conversation at turn end.
    ///
    /// The display model verbatim, ordered as rendered; the writer
    /// serializes it as the file's `messages` array unchanged.
    messages: Vec<TuiMessage>,

    /// The cumulative token totals at turn end.
    ///
    /// The session's accounting — lifetime input and output plus
    /// the last turn's input — read from the shared counters at
    /// publish time, so the file never carries a total the bar did
    /// not show.
    tokens: crate::session::SessionTokens,
}

/// The single writer thread that persists turn-end snapshots.
///
/// One thread takes the newest published snapshot, writes it, and
/// waits for the next — so the newest transcript always lands last
/// and an older, slower save can never rename over a newer one.
/// The one-slot mailbox bounds retention at a single snapshot even
/// while a write is stalled. Joining waits out the in-flight write
/// and any snapshot still in the slot, so a quitting session
/// persists its last turn.
struct TranscriptWorker {
    slot: Arc<std::sync::Mutex<Option<TranscriptSnapshot>>>,
    signal: Arc<std::sync::Condvar>,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// How the writer waits for the next snapshot.
///
/// Takes the locked slot and returns it re-acquired — the condvar
/// wait in production, anything a test needs to interpose in
/// between.
type WaitFn = std::sync::Arc<
    dyn Fn(
            std::sync::MutexGuard<'_, Option<TranscriptSnapshot>>,
        ) -> std::sync::MutexGuard<'_, Option<TranscriptSnapshot>>
        + Send
        + Sync,
>;

impl TranscriptWorker {
    /// Spawn the writer for `saver` and return its publish handle.
    ///
    /// Constructs the session's single writer thread together with
    /// the handle the turn-end hook publishes through; the worker
    /// half is joined at teardown so the last transcript is on disk
    /// before the session returns.
    fn spawn(saver: Arc<crate::session::SessionSaver>) -> (TranscriptHandle, Self) {
        let signal = Arc::new(std::sync::Condvar::new());
        let wait_signal = Arc::clone(&signal);
        Self::spawn_inner(
            saver,
            signal,
            Arc::new(move |guard| {
                wait_signal
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }),
        )
    }

    /// Spawn a writer whose wait runs `park` first, holding the slot.
    ///
    /// Test seam: `park` runs while the slot lock is held, between
    /// the predicate check and the condvar wait — the exact state in
    /// which a notification fired without that lock would be lost.
    /// A test holds the writer there for exactly as long as it
    /// needs.
    #[cfg(test)]
    fn spawn_gated(
        saver: Arc<crate::session::SessionSaver>,
        park: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> (TranscriptHandle, Self) {
        let signal = Arc::new(std::sync::Condvar::new());
        let wait_signal = Arc::clone(&signal);
        Self::spawn_inner(
            saver,
            signal,
            Arc::new(move |guard| {
                park();
                wait_signal
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }),
        )
    }

    /// The writer body every spawn variant shares.
    ///
    /// Runs the loop — wait through the injected `wait`, take the
    /// newest snapshot, save it — and returns the handle/worker pair
    /// wired to one slot, signal, and shutdown flag. The variants
    /// differ only in the wait they inject.
    fn spawn_inner(
        saver: Arc<crate::session::SessionSaver>,
        signal: Arc<std::sync::Condvar>,
        wait: WaitFn,
    ) -> (TranscriptHandle, Self) {
        let slot = Arc::new(std::sync::Mutex::new(None::<TranscriptSnapshot>));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_slot = Arc::clone(&slot);
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            loop {
                let mut guard = thread_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while guard.is_none() && !thread_shutdown.load(Ordering::SeqCst) {
                    guard = wait(guard);
                }
                let snapshot = guard.take();
                drop(guard);
                let Some(snapshot) = snapshot else {
                    break;
                };
                if let Err(err) = saver.save(&snapshot.messages, snapshot.tokens) {
                    tracing::warn!(error = %err, "session transcript could not be saved");
                }
            }
        });
        let worker_slot = Arc::clone(&slot);
        (
            TranscriptHandle {
                slot,
                signal: Arc::clone(&signal),
            },
            TranscriptWorker {
                slot: worker_slot,
                signal,
                shutdown,
                handle: Some(handle),
            },
        )
    }

    /// Store the shutdown flag under the slot lock and wake the
    /// writer.
    ///
    /// The store must hold the lock — a notification fired between
    /// the writer's predicate check and its wait would otherwise be
    /// lost, and the writer would sleep forever. A snapshot still in
    /// the slot when the writer wakes is written before the thread
    /// exits.
    fn signal_shutdown(&self) {
        {
            let _slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.shutdown.store(true, Ordering::SeqCst);
        }
        self.signal.notify_all();
    }

    /// Wait for the writer to drain and exit.
    ///
    /// Runs the shutdown sequence and joins the thread; when this
    /// returns, the last published transcript is on disk.
    fn join(mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

impl Drop for TranscriptWorker {
    /// Park off the writer even when no join happens.
    ///
    /// The early-failure paths return without joining; without this
    /// the writer would wait on its condvar forever. Idempotent with
    /// [`TranscriptWorker::join`], which takes the handle first and
    /// leaves this nothing to do.
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
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
/// starting. The flag gates the loop condition, so it is checked
/// before a queued submission is admitted, not only after a run
/// returns — and once more when a parked receive wakes, so a
/// submission the flag overtakes mid-park is dropped unclaimed
/// rather than started.
async fn drive_submissions(
    mut runner: dch_loop::Runner,
    mut receiver: mpsc::UnboundedReceiver<String>,
    state: TuiObserverState,
    shutting_down: Arc<AtomicBool>,
) {
    let cancel = runner.cancel_signal();
    while !shutting_down.load(Ordering::SeqCst)
        && let Some(text) = receiver.recv().await
    {
        if shutting_down.load(Ordering::SeqCst) {
            break;
        }
        state.queued.fetch_sub(1, Ordering::SeqCst);
        cancel.reset();
        state.agent_running.store(true, Ordering::SeqCst);
        let result = runner.run(&text).await;
        state.agent_running.store(false, Ordering::SeqCst);
        let run_errored = result.is_err();
        if run_errored {
            state
                .active_tools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
        if let Err(err) = result {
            state
                .errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(err.to_string());
        }
        if cancel.is_cancelled() {
            while receiver.try_recv().is_ok() {
                state.queued.fetch_sub(1, Ordering::SeqCst);
            }
            state.render_notify.notify(1);
        } else if run_errored {
            state.render_notify.notify(1);
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
    /// loopback interface.
    ///
    /// A real run then fails fast without touching the network.
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
    ///
    /// Reads as the driver's loop condition, so a `true` flag makes
    /// the pins below exercise the quitting path without any
    /// terminal or user input.
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

        state.queued.store(1, Ordering::SeqCst);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(runner, rx, state.clone(), shutdown_flag(false)),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert_eq!(
            state.queued.load(Ordering::SeqCst),
            0,
            "the driver claims the submission, returning the depth to zero"
        );

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
    async fn a_failed_run_retires_stranded_tool_rows() {
        let runner = unreachable_runner().await;
        let (observer, state) = TuiObserverState::new().into_observer();
        drop(observer);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("do a thing".to_string()).unwrap();
        drop(tx);
        // A tool stranded mid-flight: the engine will not fire its
        // completion for a run that fails outright.
        state
            .active_tools
            .lock()
            .unwrap()
            .push(dch_tui::ActiveTool {
                call_id: "call-strand".to_string(),
                name: "Read".to_string(),
                input_summary: String::new(),
                start: std::time::Instant::now(),
            });
        state.queued.store(1, Ordering::SeqCst);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(runner, rx, state.clone(), shutdown_flag(false)),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert!(
            state.active_tools.lock().unwrap().is_empty(),
            "a failed run leaves no spinner spinning behind it"
        );
    }

    #[tokio::test]
    async fn a_run_marks_the_agent_running_until_it_lands() {
        let runner = unreachable_runner().await;
        let (observer, state) = TuiObserverState::new().into_observer();
        drop(observer);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("do a thing".to_string()).unwrap();
        drop(tx);

        state.queued.store(1, Ordering::SeqCst);
        let flag = Arc::clone(&state.agent_running);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(runner, rx, state.clone(), shutdown_flag(false)),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert!(
            !flag.load(Ordering::SeqCst),
            "the running flag settles false once the run lands, failed or not"
        );
        assert_eq!(
            state.queued.load(Ordering::SeqCst),
            0,
            "nothing stays queued behind the landed run"
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
        state.queued.fetch_add(1, Ordering::SeqCst);
        tx.send("second".to_string()).unwrap();
        state.queued.fetch_add(1, Ordering::SeqCst);
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(runner, rx, state.clone(), shutdown_flag(true)),
        )
        .await
        .expect("the driver leaves the queue behind instead of draining it");

        let errors = state.errors.lock().unwrap();
        assert!(
            errors.is_empty(),
            "no submission is admitted once shutdown has begun — neither queued task ran"
        );
        assert_eq!(
            state.queued.load(Ordering::SeqCst),
            2,
            "unclaimed submissions stay counted — the queue outlives the driver"
        );
    }

    #[tokio::test]
    async fn a_submission_overtaken_by_shutdown_mid_park_never_starts() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let flag = shutdown_flag(false);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&flag),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        state.queued.fetch_add(1, Ordering::SeqCst);
        tx.send("late".to_string()).unwrap();
        flag.store(true, Ordering::SeqCst);
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("the driver ends once the channel closes")
            .expect("the driver task itself does not panic");

        let errors = state.errors.lock().unwrap();
        assert!(
            errors.is_empty(),
            "a submission the shutdown flag overtakes mid-park never starts a run"
        );
        assert_eq!(
            state.queued.load(Ordering::SeqCst),
            1,
            "the overtaken send stays counted as unclaimed"
        );
    }

    /// A saver rooted at a throwaway directory plus its coordinates.
    ///
    /// The writer pins never touch the real sessions root: the
    /// saver's base is the temp dir, returned alongside it (kept
    /// alive by the caller) and the session id its file lands under.
    fn worker_saver() -> (
        Arc<crate::session::SessionSaver>,
        tempfile::TempDir,
        uuid::Uuid,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "worker-model".to_string(),
            dir.path().to_path_buf(),
        ));
        (saver, dir, id)
    }

    /// Read a worker-written transcript back from disk.
    ///
    /// Parses the envelope directly rather than going through the
    /// saver, so a pin proves what is actually in the file — not
    /// what the saver's own reader would reconstruct from it.
    fn saved_messages(dir: &std::path::Path, id: uuid::Uuid) -> Vec<TuiMessage> {
        let path = dir.join(id.to_string()).join("session.json");
        let json = std::fs::read_to_string(path).expect("the transcript file");
        let envelope: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        serde_json::from_value(
            envelope
                .get("messages")
                .expect("the messages field")
                .clone(),
        )
        .expect("the messages parse back")
    }

    #[test]
    fn an_older_snapshot_never_overwrites_a_newer_one() {
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        let now = chrono::Utc::now();
        let bulky = vec![
            TuiMessage::User {
                text: "x".repeat(20_000),
                timestamp: now,
            };
            100
        ];
        let final_turn = vec![TuiMessage::User {
            text: "final turn".to_string(),
            timestamp: now,
        }];
        handle.send(bulky, crate::session::SessionTokens::default());
        handle.send(final_turn, crate::session::SessionTokens::default());
        drop(handle);
        worker.join();
        let messages = saved_messages(dir.path(), id);
        assert_eq!(
            messages.len(),
            1,
            "the transcript is the newest snapshot, however the queue interleaved"
        );
        assert!(
            matches!(&messages.first().expect("the one message"), TuiMessage::User { text, .. } if text == "final turn"),
            "a slow older snapshot must not rename over a completed newer one"
        );
    }

    #[test]
    fn joining_the_writer_flushes_the_final_snapshot() {
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        handle.send(
            vec![TuiMessage::User {
                text: "the last turn".to_string(),
                timestamp: chrono::Utc::now(),
            }],
            crate::session::SessionTokens::default(),
        );
        drop(handle);
        worker.join();
        let messages = saved_messages(dir.path(), id);
        assert_eq!(
            messages.len(),
            1,
            "join returns only after the final write is on disk"
        );
        assert!(
            matches!(&messages.first().expect("the one message"), TuiMessage::User { text, .. } if text == "the last turn"),
            "the persisted transcript is the joined snapshot"
        );
    }

    /// Holds a thread at a chosen point until released.
    ///
    /// The gated-spawn pins park the writer on this between its
    /// predicate check and its wait — the state a lost notification
    /// lives in — for exactly as long as the test needs.
    struct Gate {
        open: std::sync::Mutex<bool>,
        signal: std::sync::Condvar,
        parked: std::sync::atomic::AtomicBool,
    }

    impl Gate {
        fn closed() -> Self {
            Self {
                open: std::sync::Mutex::new(false),
                signal: std::sync::Condvar::new(),
                parked: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn hold(&self) {
            self.parked.store(true, Ordering::SeqCst);
            let mut open = self.open.lock().expect("the gate lock");
            while !*open {
                open = self.signal.wait(open).expect("the gate wait");
            }
            self.parked.store(false, Ordering::SeqCst);
        }

        fn release(&self) {
            let mut open = self.open.lock().expect("the gate lock");
            *open = true;
            drop(open);
            self.signal.notify_all();
        }

        fn wait_parked(&self) {
            while !self.parked.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
    }

    #[test]
    fn a_shutdown_notification_is_never_lost_between_check_and_wait() {
        let (saver, dir, id) = worker_saver();
        let gate = Arc::new(Gate::closed());
        let hold = Arc::clone(&gate);
        let (_handle, worker) = TranscriptWorker::spawn_gated(saver, Arc::new(move || hold.hold()));

        // Park the writer in the exact state the race lives in:
        // predicate checked false, slot lock still held, not yet
        // waiting on the writer's own signal.
        gate.wait_parked();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let joiner = std::thread::spawn(move || {
            worker.join();
            // A failed send means the test already gave up on this
            // joiner.
            let _sent = done_tx.send(());
        });
        // Let join meet the held lock (or, on the unfixed shape,
        // fire its notification into the vacant window).
        std::thread::sleep(std::time::Duration::from_millis(100));
        gate.release();

        if done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            panic!(
                "join hung: the shutdown notification fired while the \
                 writer sat between its predicate check and its wait, and \
                 was lost — the flag must be stored under the slot lock"
            );
        }
        drop(joiner.join());
        drop((dir, id));
    }

    #[test]
    fn a_dropped_writer_takes_no_late_publish() {
        let (saver, dir, id) = worker_saver();
        let gate = Arc::new(Gate::closed());
        let hold = Arc::clone(&gate);
        let (handle, worker) = TranscriptWorker::spawn_gated(saver, Arc::new(move || hold.hold()));
        gate.wait_parked();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let dropper = std::thread::spawn(move || {
            // Dropping, not joining — the early-failure path's shape.
            // With the shutdown store under the lock, this blocks
            // until the gate opens, then parks the writer off.
            drop(worker);
            let _sent = done_tx.send(());
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        gate.release();
        if done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            panic!("drop hung: an unjoined worker must still be parked off");
        }
        drop(dropper.join());

        // The writer is gone: a publish after the drop has no
        // consumer and must never reach the disk.
        handle.send(
            vec![TuiMessage::User {
                text: "late".to_string(),
                timestamp: chrono::Utc::now(),
            }],
            crate::session::SessionTokens::default(),
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        let path = dir.path().join(id.to_string()).join("session.json");
        assert!(
            !path.exists(),
            "a dropped worker must not leave a writer behind to consume a late publish"
        );
    }
}
