//! Interactive TUI runner: load config, build the agent behind a
//! streaming display, drive submissions until the user quits.
//!
//! This is the dispatch target for plain `dch`. It owns no agent-loop
//! logic — everything turn/stream/dispatch lives in loopctl via the
//! runner — and no rendering beyond the app it hosts: the mode's own
//! work is the plumbing that couples submitted input lines to agent
//! runs, routes run failures into the display, and tears everything
//! down when the user quits.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dch_tui::PermissionRequest;
use dch_tui::QuestionPrompt;
use dch_tui::Submission;
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
/// resumed control shapes each step: the restored conversation seeds
/// both the agent's history and the display, the engine runs on the
/// current configuration's model and provider — the session's
/// recorded model is provenance for the listing, not an instruction —
/// and the session id is the loaded one so auto-saves keep writing
/// the resumed file.
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
    apply_cli_overrides(&mut config, args)?;

    let workdir = std::env::current_dir().map_err(|err| format!("cannot determine cwd: {err}"))?;

    let (observer, state) = TuiObserverState::new()
        .with_secret_redaction(config.security.redact_secrets)
        .into_observer();
    if let ResumeControl::Resumed(outcome) = &control {
        let mut tokens = state
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens.cumulative_input = outcome.tokens.cumulative_input;
        tokens.cumulative_output = outcome.tokens.cumulative_output;
        tokens.input = outcome.tokens.last_input_tokens;
        tokens.context_tokens = outcome.tokens.context_tokens;
    }
    let (permission_resolver, permission_rx) = permission_bridge();
    let mut builder = dch_loop::Runner::builder(&config, &workdir)
        .with_observer(Arc::new(observer) as Arc<dyn loopctl::observer::LoopObserver>)
        .with_middleware(Arc::new(
            dch_tui::CapturingMiddleware::new_with_input_redaction(
                Arc::clone(&state.tool_captures),
                config.security.redact_secrets,
            ),
        ))
        .with_permission_resolver(permission_resolver);
    if let ResumeControl::Resumed(outcome) = &control {
        builder = builder.with_history(crate::resume::history_for_resume(
            outcome,
            config.security.redact_secrets,
        ));
    }
    let runner = builder
        .build()
        .await
        .map_err(|err| format!("agent construction: {err}"))?;
    state
        .context_window
        .store(runner.context_window(), Ordering::SeqCst);

    // Rebuild the read-before-write guard's baselines for the files
    // the restored transcript shows being read: without this, a
    // resumed Write treats every previously-read file as never-read.
    if let ResumeControl::Resumed(outcome) = &control {
        for path in crate::resume::resumed_read_paths_with_machine(
            &outcome.messages,
            outcome.machine.as_deref(),
        ) {
            let _recorded = runner.record_resumed_read(&path).await;
        }
    }

    // The cooperative interrupt bridge: the first external SIGINT or
    // SIGTERM cancels the run in flight; the forced second press
    // restores the terminal before the bridge's own exit.
    let signal_bridge = crate::signals::install_cancel_handler(runner.cancel_signal(), || {
        let _restored = dch_tui::restore_terminal();
    });

    // The asking path: the runner's asking channel and its bridge are
    // supervised, so the prompts the app's overlay renders keep
    // arriving for the session's whole life — each answer returns to
    // the asking tool through the bridge's own correlation.
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
    let questions = runner.question_path().supervise(prompt_tx, overlay_prompt);

    let (submit_tx, submit_rx) = mpsc::unbounded_channel();
    let model = config.api.model.clone();
    let mouse_capture = config.display.mouse_capture;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    app.set_submit_tx(submit_tx);
    app.set_guidance(Some(Arc::new(LoopGuidance {
        steering: runner.steering(),
    })));
    app.set_permission_requests(permission_rx);
    app.set_question_requests(prompt_rx);
    app.set_working_dir(&workdir);
    app.set_git_branch(git_branch(&workdir));

    // Seed the display from the control — the restored transcript, or
    // the note that a resume degraded to this fresh session — and take
    // the id the saver writes under: the resumed file's own identity.
    let mut resumed_machine: Option<Vec<loopctl::message::Message>> = None;
    let session_id = match control {
        ResumeControl::Resumed(outcome) => {
            app.seed_messages(outcome.messages);
            resumed_machine = outcome.machine;
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

    let saver = Arc::new(crate::session::SessionSaver::with_model(session_id, model));
    let (transcript_handle, transcript_worker) = TranscriptWorker::spawn(saver);
    let initial_transcript = transcript_handle.clone();
    install_turn_end_hook(&mut app, &state, transcript_handle);

    initial_transcript.send(app.conversation().to_vec(), snapshot_tokens(&state.tokens));

    TerminalGuard::install_panic_hook();
    let (guard, mut terminal) =
        TerminalGuard::new(mouse_capture).map_err(|err| format!("terminal init: {err}"))?;
    if !dch_tui::sync_default_background(app.theme.ui.background) {
        app.theme = app.theme.deferred_to_terminal();
    }
    dch_tui::arm_fatal_signal_restore();

    let cancel = runner.cancel_signal();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let cancel_for_app = Arc::clone(&cancel);
    app.set_run_canceller(Box::new(move || cancel_for_app.cancel()));
    app.set_shutdown_flag(Arc::clone(&shutting_down));
    if let Some(machine) = &resumed_machine {
        let adopted = Arc::new(machine.clone());
        *state
            .machine_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&adopted));
        let compactions = recorded_compactions(app.conversation());
        initial_transcript.send_machine(MachineSnapshot {
            messages: adopted,
            saved_at: chrono::Utc::now(),
            compactions,
        });
    }
    let driver = tokio::spawn(drive_submissions(
        runner,
        submit_rx,
        state,
        Arc::clone(&shutting_down),
        initial_transcript,
    ));

    let session = app.run(&mut terminal).await;
    shutting_down.store(true, Ordering::SeqCst);
    let unclaimed = app.waiting_prompt_texts_at_exit();
    app.settle_exit_stop(&mut terminal);
    app.echo_waiting_prompts_at_exit(unclaimed);
    questions.abort();
    cancel.cancel();
    tear_down_driver_bounded(driver).await;
    app.echo_rescued_corrections_at_exit();
    drop(app);
    signal_bridge.stop().await;
    dch_tui::enter_signal_deaf_window();
    transcript_worker.join();
    dch_tui::exit_signal_deaf_window();
    drop(guard);
    println!("{}", session_exit_line(session_id));
    session.map_err(|err| format!("terminal error: {err}"))
}

/// Await the driver with a bound, then tear it down.
///
/// The exit is prompt even when the run will not die politely: a
/// wedged tool or a stubborn stream gets a bounded wait, then the
/// task is torn down — the session's halves were persisted by the
/// settle above, so nothing more can be saved by waiting.
async fn tear_down_driver_bounded(driver: tokio::task::JoinHandle<()>) {
    let mut driver = driver;
    if tokio::time::timeout(std::time::Duration::from_millis(500), &mut driver)
        .await
        .is_err()
    {
        driver.abort();
    }
}

/// Install the session's turn-end persistence hook.
///
/// Every hook fire publishes the conversation and then the machine
/// half beside it — display first, machine second, so the machine
/// stamp stays the newer of the pair. The engine truth comes from the
/// fire's own history when a turn end just graduated, and otherwise
/// from the newest history however it was last known: a run's
/// trailing error rows re-stamp the display half after the engine
/// half's own publish, and without a re-stamp beside them the pair
/// rule would read the engine half stale and the next resume would
/// derive from the display transcript instead of adopting.
fn install_turn_end_hook(
    app: &mut TuiApp,
    state: &dch_tui::TuiObserverState,
    transcript_handle: TranscriptHandle,
) {
    let hook_tokens = Arc::clone(&state.tokens);
    let hook_state = state.clone();
    app.set_turn_end_hook(Box::new(move |conversation, machine| {
        transcript_handle.send(conversation.to_vec(), snapshot_tokens(&hook_tokens));
        let history = machine.map(Arc::clone).or_else(|| {
            hook_state
                .machine_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        });
        if let Some(history) = history {
            let compactions = recorded_compactions(conversation);
            transcript_handle.send_machine(MachineSnapshot {
                messages: history,
                saved_at: chrono::Utc::now(),
                compactions,
            });
        }
    }));
}

/// The session-file token totals for the live counters.
///
/// One mapping for both save sites — the turn-end hook and the
/// pre-first-frame snapshot — so the file always carries the totals
/// the status bar showed at the moment of the save, including the
/// ones a resume restored; a poisoned lock records zeros rather than
/// blocking a save.
fn snapshot_tokens(
    live: &Arc<std::sync::Mutex<dch_tui::TokenCounts>>,
) -> crate::session::SessionTokens {
    live.lock().map_or_else(
        |_| crate::session::SessionTokens::default(),
        |counts| crate::session::SessionTokens {
            cumulative_input: counts.cumulative_input,
            cumulative_output: counts.cumulative_output,
            last_input_tokens: counts.input,
            context_tokens: counts.context_tokens,
        },
    )
}

/// The two lines the TUI prints once a session ends.
///
/// A `Resume with:` label, then the copy-pasteable command on its own
/// line — nothing else, so the exit leaves exactly the instruction
/// behind. An initial snapshot persists before the first frame, so
/// the file the command names exists however the session ended;
/// printed after the transcript writer joins and the terminal is
/// restored, on clean exits and failures alike.
fn session_exit_line(session_id: uuid::Uuid) -> String {
    format!("Resume with:\ndch --resume {session_id}")
}

/// The workdir's checked-out branch, read from the repository's
/// `HEAD` file — no subprocess.
///
/// A symbolic ref yields its branch name (`ref: refs/heads/x` →
/// `x`), a detached head its seven-character commit id — a full
/// object id of either width git writes, SHA-1's 40 hex characters
/// or SHA-256's 64 — and a
/// linked worktree's `.git` pointer file resolves to the gitdir
/// holding the real `HEAD`. Anything else — no repository, an
/// unreadable head, a ref outside `refs/heads` — reads as no
/// branch, and the caller shows none.
fn git_branch(workdir: &Path) -> Option<String> {
    let dot_git = workdir.join(".git");
    // A linked worktree's `.git` is a file naming the real gitdir;
    // a plain repository's is the directory holding `HEAD` itself.
    let head = match std::fs::read_to_string(&dot_git) {
        Ok(pointer) => {
            let gitdir = pointer.trim().strip_prefix("gitdir: ")?;
            Path::new(gitdir).join("HEAD")
        }
        Err(_) => dot_git.join("HEAD"),
    };
    let head = std::fs::read_to_string(head).ok()?;
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_string());
    }
    let hex = head.chars().all(|c| c.is_ascii_hexdigit());
    let detached = hex && matches!(head.len(), 40 | 64);
    detached.then(|| head.chars().take(7).collect())
}

/// The handle a turn-end hook uses to hand transcripts to the writer.
///
/// Holds the shared one-slot mailbox: publishing a snapshot
/// overwrites whatever is still waiting, so at most one pending
/// transcript is ever retained — a stalled write cannot pile
/// snapshots up behind it. Publishing is all the hook does — cheap
/// enough to run inline on the render task.
struct TranscriptHandle {
    pending: Arc<std::sync::Mutex<PendingSaves>>,
    signal: Arc<std::sync::Condvar>,
}

impl Clone for TranscriptHandle {
    /// Clone the mailbox handle.
    ///
    /// Both halves are shared arcs, so a clone publishes through the
    /// same one-slot mailbox the original does — the host keeps one to
    /// seed the session file before the first frame.
    fn clone(&self) -> Self {
        Self {
            pending: Arc::clone(&self.pending),
            signal: Arc::clone(&self.signal),
        }
    }
}

impl TranscriptHandle {
    /// Publish one conversation snapshot for the writer.
    ///
    /// Overwrites a still-pending snapshot — only the newest
    /// transcript is ever worth writing — then wakes the writer.
    /// Never blocks and never fails visibly.
    fn send(&self, messages: Vec<TuiMessage>, tokens: crate::session::SessionTokens) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.display = Some(TranscriptSnapshot {
            messages,
            tokens,
            saved_at: chrono::Utc::now(),
        });
        drop(pending);
        self.signal.notify_one();
    }

    /// Publish one machine-history snapshot for the writer.
    ///
    /// Same overwrite-newest semantics as [`send`](Self::send), for
    /// the engine's committed history; the writer persists it under
    /// its own sibling file, so the display half in flight beside it
    /// is never held back by this half or vice versa.
    fn send_machine(&self, machine: MachineSnapshot) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.machine = Some(machine);
        drop(pending);
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

    /// When this snapshot's content became true.
    ///
    /// The publish moment — the hook's fire time — so the machine
    /// sibling's read-time stamp beside it compares content against
    /// content, never write order.
    saved_at: chrono::DateTime<chrono::Utc>,
}

/// One machine-history snapshot: the engine's committed conversation.
///
/// What the turn-end hook publishes at every turn boundary and the
/// submission driver re-publishes when a run resolves; the writer
/// persists it under the sibling `machine.json`. The stamp is the
/// moment the snapshot was taken for persistence, so the pair rule on
/// resume judges content age rather than write order.
struct MachineSnapshot {
    /// The engine's history at the snapshot's turn boundary.
    ///
    /// The event's own coherent set — full on a successful turn,
    /// committed-only on a failed one, engine truth either way —
    /// shared behind an Arc so publishing never clones the history.
    messages: Arc<Vec<loopctl::message::Message>>,

    /// When this snapshot was taken for persistence.
    ///
    /// Carried through to the file's `saved_at` unchanged.
    saved_at: chrono::DateTime<chrono::Utc>,

    /// How many compaction passes the conversation had recorded when
    /// this snapshot was published.
    ///
    /// Counted from the display transcript's Compacted rows — the
    /// TUI transcript carries them, so the count and the rows agree;
    /// the driver's post-run republish passes zero, which loses
    /// nothing because the transcript's rows carry the exemption on
    /// this path.
    compactions: u32,
}

/// Count the compaction rows a conversation has recorded.
///
/// The TUI's machine snapshots source their compaction count from the
/// display transcript's own Compacted rows, so the snapshot file and
/// the transcript beside it always tell the guard the same story.
fn recorded_compactions(conversation: &[dch_tui::TuiMessage]) -> u32 {
    u32::try_from(
        conversation
            .iter()
            .filter(|message| matches!(message, dch_tui::TuiMessage::Compacted { .. }))
            .count(),
    )
    .unwrap_or(u32::MAX)
}

/// The writer's pending halves, newest-wins per half.
///
/// Display and machine snapshots arrive from different tasks on
/// different schedules — the hook fires from observer events while
/// the run future is still resolving, the driver's machine read
/// follows the resolution — so each half overwrites only its own
/// kind and the writer persists whichever halves it finds.
#[derive(Default)]
struct PendingSaves {
    /// The newest display snapshot, when one is waiting.
    ///
    /// Overwritten by every publish, so a stalled writer always
    /// persists the newest transcript rather than a queue of them.
    display: Option<TranscriptSnapshot>,

    /// The newest machine-history snapshot, when one is waiting.
    ///
    /// The same newest-wins rule on the engine half; the two halves
    /// never block each other, only replace their own kind.
    machine: Option<MachineSnapshot>,
}

impl PendingSaves {
    /// Whether neither half is waiting.
    ///
    /// The writer's condvar predicate: sleep only when there is
    /// nothing of either kind to persist.
    fn is_empty(&self) -> bool {
        self.display.is_none() && self.machine.is_none()
    }
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
    pending: Arc<std::sync::Mutex<PendingSaves>>,
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
    dyn Fn(std::sync::MutexGuard<'_, PendingSaves>) -> std::sync::MutexGuard<'_, PendingSaves>
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
        let pending = Arc::new(std::sync::Mutex::new(PendingSaves::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_pending = Arc::clone(&pending);
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            loop {
                let mut guard = thread_pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while guard.is_empty() && !thread_shutdown.load(Ordering::SeqCst) {
                    guard = wait(guard);
                }
                let (display, machine) = (guard.display.take(), guard.machine.take());
                drop(guard);
                if display.is_none() && machine.is_none() {
                    break;
                }
                if let Some(snapshot) = display
                    && let Err(err) =
                        saver.save_at(&snapshot.messages, snapshot.tokens, snapshot.saved_at)
                {
                    tracing::warn!(error = %err, "session transcript could not be saved");
                }
                if let Some(snapshot) = machine
                    && let Err(err) = saver.save_machine(
                        &snapshot.messages,
                        snapshot.saved_at,
                        snapshot.compactions,
                    )
                {
                    tracing::warn!(error = %err, "machine history could not be saved");
                }
            }
        });
        let worker_pending = Arc::clone(&pending);
        (
            TranscriptHandle {
                pending,
                signal: Arc::clone(&signal),
            },
            TranscriptWorker {
                pending: worker_pending,
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
            let _pending = self
                .pending
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

/// The app's steering seam over the runner's slot.
///
/// The interface asks in text and answers in text; the slot speaks engine
/// messages, and this is that translation.
struct LoopGuidance {
    /// The session's slot, shared with the loop that drains it.
    ///
    /// An `Arc` rather than a borrow because the run holds the slot for as
    /// long as its future lives, and a key press arriving on another task
    /// cannot ask for it back for the duration. Both ends reach the same
    /// allocation: the loop reads it between turns, a steer writes into it.
    steering: Arc<loopctl::engine::Steering>,
}

impl dch_tui::Guidance for LoopGuidance {
    fn steer(&self, text: String) -> bool {
        // A user-role message, so the guidance enters the conversation as
        // something the user said — which is what it is. `false` when no
        // run is in flight, or ended that instant; the app then submits
        // the text as an ordinary prompt instead.
        self.steering.push(loopctl::message::Message::user(text))
    }

    fn take_returned(&self) -> Vec<String> {
        // `take_all`, not `take_leftovers`: a run that ends leaves its
        // undrained guidance in the live queue, and the sealed pocket is
        // only what an *earlier* run left behind. Asking for the pocket
        // alone would report a clean slot while the words the user sent
        // sat one shelf over, undelivered and unmentioned.
        self.steering
            .take_all()
            .iter()
            .map(loopctl::message::Message::text_content)
            .collect()
    }
}

/// Bridge the runner's permission gate to the app's overlay.
///
/// Returns the resolver the runner's `Ask` cells consult and the
/// receiver end of the channel the resolver hands requests to: each
/// consultation creates a oneshot reply, ships the request to the UI,
/// and awaits the answer — a dropped request or a closed channel
/// reads as a denial, and the gate itself races the run's cancel
/// signal so a cancelled prompt never waits on the UI.
fn permission_bridge() -> (
    loopctl::middleware::AskResolverFn,
    mpsc::UnboundedReceiver<PermissionRequest>,
) {
    let (requests_tx, requests_rx) = mpsc::unbounded_channel();
    let resolver: loopctl::middleware::AskResolverFn = Arc::new(move |prompt: &str, tool: &str| {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let sent = requests_tx.send(PermissionRequest {
            tool_name: tool.to_string(),
            prompt: prompt.to_string(),
            reply: reply_tx,
        });
        Box::pin(async move {
            if sent.is_ok() {
                reply_rx.await.unwrap_or(false)
            } else {
                false
            }
        })
    });
    (resolver, requests_rx)
}

/// Rewrite one asking-path prompt into the app's overlay type.
///
/// The mechanical field-for-field mapping between the composition
/// layer's prompt and the display's own type; the reply one-shot
/// rides through untouched, so an answer reaches the asking tool
/// without this mapping holding anything open.
fn overlay_prompt(prompted: dch_loop::PromptedQuestion) -> QuestionPrompt {
    QuestionPrompt {
        question: prompted.question,
        header: prompted.header,
        options: prompted
            .options
            .into_iter()
            .map(|option| dch_tui::QuestionChoice {
                label: option.label,
                description: option.description,
            })
            .collect(),
        multi_select: prompted.multi_select,
        reply: prompted.reply,
    }
}

/// Claim a prompt for the turn that asked for it, unless the session
/// is leaving.
///
/// The channel carries an id rather than the text so this read is the last
/// word on what gets sent: a prompt edited between its submit and its turn
/// runs as edited, and a prompt the user dropped in between yields `None`
/// and never runs at all. A prompt that yields nothing is not a failed run
/// and is not treated as one — there is simply no turn to start, and the
/// driver takes the next id the channel holds.
///
/// The shutdown flag is read under the queue's own lock, beside the
/// removal it gates, because the exit's unclaimed-rows snapshot reads
/// the same lock after raising the flag: the mutex orders the two, so
/// a prompt is either claimed here or still waiting when the snapshot
/// reads it — never both, which is what keeps the exit's transcript
/// echo from duplicating a claim that raced it. A claim that finds the
/// flag up declines like any other `None`: the entry is not spent,
/// and the exit's snapshot is what records it.
fn take_queued(state: &TuiObserverState, id: u64, shutting_down: &AtomicBool) -> Option<String> {
    let mut queue = state
        .queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if shutting_down.load(Ordering::SeqCst) {
        return None;
    }
    queue.remove(id)
}

/// Put a prompt back where the waiting section shows it.
///
/// The other half of `take_queued`: the queue is shared with the display, so
/// a prompt the driver can no longer send — a submission the shutdown
/// overtook — returns there to be read, edited, or dropped rather than
/// thrown away. It will not run in a session that has stopped taking
/// submissions; neither do its words vanish with one.
fn push_queued(state: &TuiObserverState, text: String) -> u64 {
    state
        .queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(text)
}

/// What the driver tells the user about a correction's history cut.
///
/// The correction names the committed length its superseded turn
/// started from, and every way the rewind can land divides into one of
/// six shapes. The engine's answer alone does not separate them —
/// `Ok(0)` is a history the run's compaction folded past the cut, one
/// a fold returned to exactly the cut, and one the superseded turn
/// never wrote into — so the verdict reads the length before and after
/// the cut beside the result, and every non-clean shape is something
/// the user should hear: a silent partial replace would pass as the
/// replacement.
///
/// Pure over its inputs so every shape is pinnable without a canned
/// provider; the driver feeds it the recorded length, the committed
/// lengths around the rewind, and the rewind's own result.
///
/// Returns the notice to post, or `None` when the cut landed exactly
/// where the correction said it would and there is nothing to say.
fn correction_cut_notice(
    before: usize,
    committed_before_cut: usize,
    removed: &Result<usize, loopctl::error::LoopError>,
    committed_after_cut: usize,
    interrupted: bool,
) -> Option<String> {
    match removed {
        // A refused rewind is never silent, whatever the lengths
        // beside it read as: the turn the correction meant to replace
        // is still in the conversation, and the error is the only
        // thing that says so.
        Err(error) => Some(format!(
            "the superseded turn stayed in the conversation: {error}"
        )),
        // A turn the correction itself stopped may commit nothing, and
        // the history already standing exactly where the cut would
        // land is not news: the corrected turn is the next story the
        // user reads. A history folded *below* the cut falls through
        // to the compacted arm below — interrupted or not, the
        // correction rides on top of the fold.
        _ if committed_before_cut == before && interrupted => None,
        // A turn the correction did not stop may still have written
        // nothing: a run the window refuses outright discards even its
        // own prompt, and the history then stands exactly where the
        // cut lands with nothing folded. The verdict must not claim a
        // compaction that never ran — and the lengths cannot separate
        // this shape from a fold that lands exactly back, so the row
        // names the stopped turn, the shape a session reaches.
        Ok(_)
            if committed_before_cut == before
                && committed_after_cut == before
                && !interrupted =>
        {
            Some(
                "the turn it superseded had stopped before it wrote anything; the fix was sent in its place"
                    .to_string(),
            )
        }
        _ if committed_before_cut <= before => Some(
            "the superseded turn had already been compacted away, so the correction followed it rather than replacing it"
                .to_string(),
        ),
        Ok(_) if committed_after_cut != before => Some(
            "the correction's cut landed on a different boundary than the turn it named"
                .to_string(),
        ),
        Ok(_) => None,
    }
}

/// Whether the correction `wanted` is already waiting in the channel.
///
/// Pulls submissions until it finds it or the channel runs dry; every
/// submission pulled — the matched correction included — is pushed
/// onto `revived`, which the loop drains ahead of the channel, so
/// peeking costs no submission its place in line. The display sends a
/// correction before raising the cancel that stops its turn and names
/// it on the shared flag, so by the time a cancelled run settles its
/// correction is already buffered.
fn pull_pending_correction(
    wanted: u64,
    receiver: &mut mpsc::UnboundedReceiver<Submission>,
    revived: &mut std::collections::VecDeque<Submission>,
) -> bool {
    loop {
        match receiver.try_recv() {
            Ok(submission @ Submission::Correct { id, .. }) if id == wanted => {
                revived.push_back(submission);
                return true;
            }
            Ok(submission) => revived.push_back(submission),
            Err(_) => return false,
        }
    }
}

/// The reminder a run that ended with an unfinished plan receives.
///
/// The tool contract and the system prompt tell the model to keep its
/// list current; this is the belt under them — the session's own
/// follow-up when the words were not enough. One nudge per claimed
/// prompt, never a loop: the nudged run's own list is final, whatever
/// it says, because a session that argues with itself forever is worse
/// than one that sometimes ends untidy.
fn todo_nudge(runner: &dch_loop::Runner) -> Option<String> {
    let todos = runner
        .context()
        .todos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let undone = todos
        .iter()
        .filter(|entry| !matches!(entry.status, dch_tools::TodoStatus::Completed))
        .count();
    (undone > 0).then(|| {
        format!(
            "reminder: {undone} todo item{} still not completed — finish the \
             remaining work, or mark the entries done / remove them with \
             TodoWrite, then finish",
            if undone == 1 { "" } else { "s" }
        )
    })
}

/// Post a run's failure as the transcript's error row.
///
/// The row is the session's record that the turn died and why: it
/// lands in the shared buffer the display drains on its next frame,
/// and the wake beside it makes the failure read as part of the run
/// that produced it rather than arriving a frame late.
fn post_run_error(state: &TuiObserverState, err: &loopctl::error::LoopError) {
    state
        .errors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(err.to_string());
    state.render_notify.notify(1);
}

/// Collect one correction's words for the exit's transcript echo.
///
/// The queue is process state and dies with the session, so the one
/// durable hand-back a correction's words can take is a transcript
/// row: the exit echo drains the shared buffer this fills.
fn rescue_correction_text(state: &TuiObserverState, text: String) {
    state
        .rescued_corrections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(text);
}

/// Hand every buffered correction to the exit's transcript echo.
///
/// The exit's cancel can land on a channel still holding corrections
/// that name other prompts — sent the moment their edits committed,
/// their words already gone from the queue with the claims — and the
/// exit can also win the race to the loop top over a correction whose
/// own run just landed. Every correction still buffered at the exit
/// drain is collected here rather than dropped with the channel: the
/// queue it once could wait in dies with the process, and an
/// unclaimed row is the one thing a resume can seed the waiting
/// section from. Returns how many wordings were collected.
fn rescue_stranded_corrections(
    revived: &mut std::collections::VecDeque<Submission>,
    receiver: &mut mpsc::UnboundedReceiver<Submission>,
    state: &TuiObserverState,
) -> usize {
    let mut handed = 0usize;
    while let Some(submission) = revived.pop_front() {
        if let Submission::Correct { text, .. } = submission {
            rescue_correction_text(state, text);
            handed = handed.saturating_add(1);
        }
    }
    while let Ok(submission) = receiver.try_recv() {
        if let Submission::Correct { text, .. } = submission {
            rescue_correction_text(state, text);
            handed = handed.saturating_add(1);
        }
    }
    handed
}

/// The corrected wording buffered for the stopped run's claim, if one
/// is already on its way.
///
/// A correction buffered for the interrupted claim carries the newest
/// wording — the words are the user's, and the newest ones win — so
/// the exit collects it for the transcript echo: an unclaimed row is
/// the one durable hand-back, and a resume seeds it back into the
/// waiting section. Without one, the claimed prompt's own text was
/// already sent, and its row already stands in the transcript — the
/// record the deliberate-stop rule reads — so nothing is collected.
fn pull_corrected_wording(
    claim: Option<(u64, usize)>,
    receiver: &mut mpsc::UnboundedReceiver<Submission>,
    revived: &mut std::collections::VecDeque<Submission>,
) -> Option<String> {
    claim.and_then(|(id, _)| {
        if pull_pending_correction(id, receiver, revived) {
            revived.pop_back().and_then(|submission| match submission {
                Submission::Correct { text, .. } => Some(text),
                Submission::Run { .. } => None,
            })
        } else {
            None
        }
    })
}

/// Whether a run's error is the exit's own cancel of it.
///
/// A `Cancelled` result alone cannot say whose hand stopped the turn:
/// an in-app cancel and the exit's stop resolve identically at the
/// engine. The shutdown flag separates them — the exit raises it
/// before its cancel fires — so a cancelled run under it is the exit
/// taking the turn with it, owing the exit's own record and requeue
/// rather than an error row.
fn exit_owns_cancel(err: &loopctl::error::LoopError, shutting_down: &Arc<AtomicBool>) -> bool {
    matches!(err, loopctl::error::LoopError::Cancelled) && shutting_down.load(Ordering::SeqCst)
}

/// Cut the superseded turn out of the committed history and post
/// the verdict's notice, if any.
///
/// The correction names the committed length its turn started from;
/// the cut is attempted wherever that claim still stands, and the
/// notice — or the verdict's silence — is the driver's honest word
/// about how the rewind landed.
fn apply_correction_cut(
    runner: &mut dch_loop::Runner,
    state: &TuiObserverState,
    before: usize,
    interrupted: bool,
) {
    let committed = runner.committed_history().len();
    let removed = runner.rewind_history_to(before);
    let notice = correction_cut_notice(
        before,
        committed,
        &removed,
        runner.committed_history().len(),
        interrupted,
    );
    if let Some(notice) = notice {
        state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(notice);
    }
}

/// Execute submitted tasks one at a time until the session ends.
///
/// Owns the runner for the session's whole lifetime, so runs never
/// overlap: each submitted line runs to completion or failure before
/// the next begins, preserving the order the user submitted in. A
/// failed run records its error into the shared buffer the display
/// drains and wakes the frame — the session continues, and the next
/// submission still runs. A cancelled run changes nothing beyond the
/// turn it stopped: the submissions queued behind it stand, and the
/// next is claimed and sent as soon as the cancelled one settles —
/// one cancel never takes the backlog with it.
/// A correction asks for the turn that already went out to be un-asked:
/// [`Submission::Correct`] cuts the superseded turn out of the committed
/// history — back to the length that turn started from, which this loop
/// records at each claim — and then sends the edited text, so the model
/// reads one prompt where the user asked a question twice. Only the most
/// recently claimed turn is cuttable: a later answer may rest on the turn
/// a correction names, and cutting it would take that answer with it, so
/// an older correction runs as an ordinary turn instead.
/// A correction stops the turn it names rather than queueing behind
/// it: the display raises the runner's cancel at the moment the edit
/// is saved, so the superseded turn resolves cancelled and the cut
/// lands on whatever it had committed by then — the cancel is the
/// correction's own hand, so it raises no error row of its own.
///
/// The loop ends when the channel closes or
/// the shutdown flag is set: the engine clears its cancel signal at
/// the end of every run, so the flag — raised by the display at the
/// very moment the quit ladder fires, before its cancel lands on
/// the run in flight, and stored once more after the display loop
/// as the backstop for every other route out — is what keeps
/// submissions queued behind a quitting user from ever
/// starting. The flag gates the loop condition, so it is checked
/// before a queued submission is admitted, not only after a run
/// returns — and once more when a parked receive wakes, so a
/// submission the flag overtakes mid-park is dropped unclaimed
/// rather than started — and the claim itself reads the flag under
/// the queue's lock, so a submission already past those checks still
/// leaves its entry waiting for the exit's snapshot rather than
/// spending it on a run the exit is stopping. Corrections still
/// buffered when the exit wins — sitting in the channel, or pulled
/// a moment before the flag rose — are collected by the exit drain
/// into the transcript echo's shared buffer: their words left the
/// queue with the claim, so an unclaimed row is the only durable
/// hand-back, and a resume seeds each one back into the waiting
/// section.
async fn drive_submissions(
    mut runner: dch_loop::Runner,
    mut receiver: mpsc::UnboundedReceiver<Submission>,
    state: TuiObserverState,
    shutting_down: Arc<AtomicBool>,
    transcript: TranscriptHandle,
) {
    let cancel = runner.cancel_signal();
    let mut last_claim: Option<(u64, usize)> = None;
    let mut revived: std::collections::VecDeque<Submission> = std::collections::VecDeque::new();
    let mut interrupted_for: Option<u64> = None;
    loop {
        if shutting_down.load(Ordering::SeqCst) {
            break;
        }
        let submission = match revived.pop_front() {
            Some(submission) => submission,
            None => {
                if let Ok(submission) = receiver.try_recv() {
                    submission
                } else {
                    state.mark_run_ended();
                    match receiver.recv().await {
                        Some(submission) => submission,
                        None => break,
                    }
                }
            }
        };
        if shutting_down.load(Ordering::SeqCst) {
            // The submission was pulled in good faith a moment before
            // the flag rose; hand it to the exit drain below rather
            // than deciding its fate here.
            revived.push_front(submission);
            break;
        }
        let text = match submission {
            Submission::Run { id } => {
                let Some(text) = take_queued(&state, id, &shutting_down) else {
                    continue;
                };
                last_claim = Some((id, runner.committed_history().len()));
                state.live_claim.store(id, Ordering::SeqCst);
                interrupted_for = None;
                text
            }
            Submission::Correct { id, text } => {
                let interrupted = interrupted_for
                    .take()
                    .is_some_and(|cancelled| cancelled == id);
                let superseded = last_claim
                    .take()
                    .filter(|(claimed, _)| *claimed == id)
                    .map(|(_, before)| before);
                if let Some(before) = superseded {
                    apply_correction_cut(&mut runner, &state, before, interrupted);
                }
                text
            }
        };
        state.mark_run_started();
        state.push_sent(text.clone());
        cancel.reset();
        state.correction_cancel.store(u64::MAX, Ordering::SeqCst);
        let result = runner.run(&text).await;
        // The todo nudge: a run that landed with work left on its own
        // plan gets exactly one follow-up reminding it to close the
        // list — the filing-and-forgetting shape the tool contract
        // forbids but a model mid-flow still falls into. The nudge is
        // not a new claim (nothing was queued for it) and not a plan
        // boundary (the plan it nudges about must keep standing), so
        // its echo rides the steered flag: a transcript row for
        // honesty, no retirement. An exit already begun owns the
        // session: the nudge never starts under it, its reminder
        // belonging to a session that is leaving.
        if !shutting_down.load(Ordering::SeqCst)
            && result.is_ok()
            && let Some(nudge) = todo_nudge(&runner)
        {
            state.push_steered(nudge.clone());
            state.render_notify.notify(1);
            let nudged = runner.run(&nudge).await;
            if let Err(err) = &nudged
                && !exit_owns_cancel(err, &shutting_down)
            {
                // A correction committed while the nudge ran names the
                // same live claim the nudge borrowed — the nudge is
                // simply the run in flight its cancel stopped, so the
                // buffered correction is the next story, not an error
                // to report: the same bookkeeping the turn's own
                // settle runs, at the nudge's settle.
                let claimed_by_correction = matches!(err, loopctl::error::LoopError::Cancelled)
                    && {
                        let interrupted_id =
                            state.correction_cancel.swap(u64::MAX, Ordering::SeqCst);
                        // Silence belongs to the run the flag names.
                        // Mid-nudge the claim the correction answers
                        // is still this prompt's, so a legitimate
                        // match stands; a flag left standing for some
                        // other turn cannot hush this settle — the
                        // same id rule the turn's own settle applies.
                        let names_this_turn =
                            last_claim.is_none_or(|(claimed, _)| claimed == interrupted_id);
                        let pulled = interrupted_id != u64::MAX
                            && names_this_turn
                            && pull_pending_correction(interrupted_id, &mut receiver, &mut revived);
                        if pulled {
                            interrupted_for = Some(interrupted_id);
                        }
                        pulled
                    };
                if !claimed_by_correction {
                    post_run_error(&state, err);
                }
            }
        }
        // The turn landed: no correction can name it as the one in
        // flight any more, so the mirror goes back to naming nothing.
        state.live_claim.store(u64::MAX, Ordering::SeqCst);
        let resolved = Arc::new(runner.committed_history());
        transcript.send_machine(MachineSnapshot {
            messages: Arc::clone(&resolved),
            saved_at: chrono::Utc::now(),
            compactions: 0,
        });
        *state
            .machine_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(resolved);
        if let Err(err) = &result {
            state
                .active_tools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            if exit_owns_cancel(err, &shutting_down) {
                // The record a cancel leaves, the exit leaves too:
                // the resumed session reads the same stopped-run
                // row either way. The words take their durable exits:
                // a buffered correction for this claim is collected
                // for the transcript echo, and without one the
                // claimed prompt's own row — already written at its
                // send — is the record that stands.
                post_run_error(&state, err);
                if let Some(corrected) =
                    pull_corrected_wording(last_claim, &mut receiver, &mut revived)
                {
                    rescue_correction_text(&state, corrected);
                } else {
                    push_queued(&state, text.clone());
                }
                state.mark_run_ended();
                break;
            }
            let interrupted_id = if matches!(err, loopctl::error::LoopError::Cancelled) {
                state.correction_cancel.swap(u64::MAX, Ordering::SeqCst)
            } else {
                u64::MAX
            };
            // Only the run the correction named may go silent: the
            // flag must carry this turn's own claim — the id set
            // beside it at claim time, in this very task, so no race
            // can blur it. A flag that outlived the display's
            // stand-down names some other turn, and this cancel keeps
            // its error row whatever order the two race in. A run
            // that started without a claim at all (a resumed
            // session's live pair) has no local id to check; the
            // display could only ever have matched the flag against
            // that pair, so its silence stands.
            let names_this_turn = last_claim.is_none_or(|(claimed, _)| claimed == interrupted_id);
            let correction = if interrupted_id != u64::MAX
                && names_this_turn
                && pull_pending_correction(interrupted_id, &mut receiver, &mut revived)
            {
                Some(interrupted_id)
            } else {
                None
            };
            if let Some(cancelled_for) = correction {
                interrupted_for = Some(cancelled_for);
            } else {
                post_run_error(&state, err);
            }
        }
    }
    if shutting_down.load(Ordering::SeqCst) {
        rescue_stranded_corrections(&mut revived, &mut receiver, &state);
    }
    state.mark_run_ended();
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::let_underscore_must_use
)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use dch_config::ApiType;

    /// Queue a prompt the way the display does, then hand its id out.
    ///
    /// The pair is the whole contract between the two halves: the entry
    /// lands in the shared queue before its id travels, because a driver
    /// that claimed an id naming nothing would have nothing to run. Every
    /// driver test seeds through here rather than sending a bare payload the
    /// display could never have produced.
    fn queue_prompt(
        state: &TuiObserverState,
        tx: &mpsc::UnboundedSender<Submission>,
        text: &str,
    ) -> u64 {
        let id = state
            .queued
            .lock()
            .expect("queue lock")
            .push(text.to_string());
        tx.send(Submission::Run { id })
            .expect("driver channel open");
        id
    }

    #[test]
    fn a_cut_that_lands_exactly_where_the_correction_named_says_nothing() {
        // The clean shape: the history held four messages of turn past
        // the recorded length, the rewind removed exactly those four,
        // and the committed history ends where the superseded turn
        // began. Nothing to report — the replacement happened.
        assert_eq!(
            correction_cut_notice(10, 14, &Ok(4), 10, false),
            None,
            "an exact cut is the correction doing what it said"
        );
    }

    #[test]
    fn a_history_folded_past_the_cut_says_compacted_away() {
        // The shrink shape: a compaction during the run dropped the
        // history below the recorded length, so the turn being
        // superseded is already gone and the correction follows the
        // summary that replaced it rather than swapping in place.
        let notice =
            correction_cut_notice(10, 8, &Ok(0), 8, false).expect("the shape is not silent");
        assert!(
            notice.contains("compacted away"),
            "the user learns the correction went on top, not in place: {notice}"
        );
    }

    #[test]
    fn an_uninterrupted_turn_that_committed_nothing_says_stopped_not_compacted() {
        // The equal shape without the correction's own hand: a turn
        // that ended having written nothing leaves the history
        // standing exactly where the cut lands, and `Ok(0)` reads the
        // same as a fold back to the recorded length. Nothing was
        // compacted, and the row must not say so — the verdict names
        // the stopped turn, the shape a session actually reaches.
        let notice =
            correction_cut_notice(10, 10, &Ok(0), 10, false).expect("the shape is not silent");
        assert!(
            notice.contains("stopped before it wrote anything"),
            "a turn that never wrote is named as stopped, not folded away: {notice}"
        );
    }

    #[test]
    fn an_interrupted_turn_that_committed_nothing_says_nothing() {
        // The stopped shape: the correction cancelled the turn mid-flight
        // and it committed nothing, so the history already stands where
        // the cut would land — the corrected turn is the next story, and
        // there is nothing to report about a swap that needed no cut.
        assert_eq!(
            correction_cut_notice(10, 10, &Ok(0), 10, true),
            None,
            "a stop the correction itself caused is not news"
        );
    }

    #[test]
    fn an_interrupted_turn_whose_history_folded_below_the_cut_says_compacted_away() {
        // The stopped-and-folded shape: the correction cancelled the
        // turn mid-flight, and a compaction during that same turn
        // dropped the history below the recorded length — the
        // superseded turn is gone into a summary, exactly the fold the
        // non-interrupted twin reports. Interrupted or not, the
        // correction goes on top of the fold rather than in place, and
        // silence would pass it as the replacement.
        let notice =
            correction_cut_notice(10, 8, &Ok(0), 8, true).expect("the shape is not silent");
        assert!(
            notice.contains("compacted away"),
            "a fold below the named length is news even when the run was \
             stopped: {notice}"
        );
    }

    #[test]
    fn a_refused_cut_under_the_interrupted_shape_still_reports_the_error() {
        // The refused-and-stopped shape: the machine declined the
        // rewind while the lengths beside it read as the silent
        // interrupted case. A refused cut is never silent — the turn
        // the correction meant to replace is still in the
        // conversation, and the error is the only thing that says so.
        let refused = loopctl::error::LoopError::InvalidInput("a turn is in flight".to_string());
        let notice = correction_cut_notice(10, 10, &Err(refused), 10, true)
            .expect("the shape is not silent");
        assert!(
            notice.contains("stayed in the conversation"),
            "the refusal outranks the lengths beside it: {notice}"
        );
    }

    #[test]
    fn a_cut_landing_on_a_different_boundary_names_it() {
        // The mismatch shape: the rewind removed six where the
        // correction named four — a boundary snap past a fold took more
        // than the turn — and a partial replace must not pass as the
        // replacement.
        let notice =
            correction_cut_notice(10, 14, &Ok(6), 8, false).expect("the shape is not silent");
        assert!(
            notice.contains("different boundary"),
            "the wrong suffix is named, not silently accepted: {notice}"
        );
    }

    #[test]
    fn a_refused_cut_reports_the_error_verbatim() {
        // Not reachable from the submission loop — the machine refuses
        // only while a turn is in flight — but the verdict has to say
        // so if it ever happens, because the correction still goes out
        // over the top of the turn it meant to replace.
        let refused = loopctl::error::LoopError::InvalidInput("a turn is in flight".to_string());
        let notice = correction_cut_notice(10, 14, &Err(refused), 14, false)
            .expect("the shape is not silent");
        assert!(
            notice.contains("stayed in the conversation") && notice.contains("a turn is in flight"),
            "the refusal names itself and its reason: {notice}"
        );
    }

    /// Tell the driver the prompt `id` was corrected after it went out.
    ///
    /// The panel's commit does this when the entry it was saving had
    /// already been claimed: the text has no queue slot left to go back to,
    /// so it travels with the id of the turn it replaces.
    fn correct_prompt(tx: &mpsc::UnboundedSender<Submission>, id: u64, text: &str) {
        tx.send(Submission::Correct {
            id,
            text: text.to_string(),
        })
        .expect("driver channel open");
    }

    /// What waits in the shared queue, as `(id, text)` pairs.
    ///
    /// The read a test needs after a run: whether a prompt is still
    /// waiting, and which text it waits with.
    fn queued_prompts(state: &TuiObserverState) -> Vec<(u64, String)> {
        let queue = state.queued.lock().expect("queue lock");
        (0..queue.len())
            .filter_map(|index| queue.get(index))
            .map(|entry| (entry.id, entry.text.clone()))
            .collect()
    }

    #[test]
    fn a_crash_between_the_pair_writes_degrades_to_derive_not_half_adoption() {
        let dir = tempfile::tempdir().expect("session tempdir");
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            root.clone(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        handle.send(
            vec![TuiMessage::User {
                text: "turn one".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
            }],
            crate::session::SessionTokens::default(),
        );
        worker.join();
        let (_, _, _, session_at) = crate::session::load_with_meta_in(id, &root).unwrap();
        assert!(
            crate::session::load_machine_in(id, &root, session_at).is_none(),
            "a display-only save never adopts a machine half it did not write"
        );

        let (handle, worker) = TranscriptWorker::spawn(saver);
        handle.send_machine(MachineSnapshot {
            messages: Arc::new(Vec::new()),
            saved_at: chrono::Utc::now(),
            compactions: 0,
        });
        worker.join();
        let (_, _, _, session_at) = crate::session::load_with_meta_in(id, &root).unwrap();
        assert!(
            crate::session::load_machine_in(id, &root, session_at).is_some(),
            "the machine half landing after restores the pair"
        );
    }

    /// A transcript handle parked on a throwaway directory.
    ///
    /// The drive tests need the driver's fifth argument without
    /// caring where its snapshots land; the worker behind this
    /// handle writes into a tempdir and parks on drop.
    fn parked_transcript() -> TranscriptHandle {
        let dir = tempfile::tempdir().expect("transcript tempdir");
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "m".to_string(),
            dir.keep(),
        ));
        let (handle, _worker) = TranscriptWorker::spawn(saver);
        handle
    }

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
    async fn the_overlay_prompt_carries_every_prompt_field_and_the_reply_channel() {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let prompt = dch_loop::PromptedQuestion {
            question: "Ship it?".to_string(),
            header: Some("Ship".to_string()),
            options: vec![dch_loop::PromptOption {
                label: "yes".to_string(),
                description: Some("now, at 3am".to_string()),
            }],
            multi_select: true,
            reply: reply_tx,
        };

        let overlay = overlay_prompt(prompt);

        assert_eq!(overlay.question, "Ship it?");
        assert_eq!(overlay.header.as_deref(), Some("Ship"));
        assert_eq!(overlay.options.len(), 1);
        assert_eq!(overlay.options[0].label, "yes");
        assert_eq!(
            overlay.options[0].description.as_deref(),
            Some("now, at 3am")
        );
        assert!(overlay.multi_select);
        overlay
            .reply
            .send(vec!["yes".to_string()])
            .expect("the asking path awaits the reply");
        assert_eq!(
            reply_rx.await.expect("the reply channel rode through"),
            vec!["yes".to_string()]
        );
    }

    #[test]
    fn the_workdir_branch_reads_the_repository_head() {
        // The branch comes from `HEAD` directly, no subprocess: a
        // symbolic ref names its branch, a detached head shows its
        // short commit id, a worktree's pointer file resolves to the
        // real gitdir, and no repository reads as no branch.
        let dir = tempfile::tempdir().expect("a temp workdir");
        assert_eq!(git_branch(dir.path()), None, "no repository: no branch");

        let git = dir.path().join(".git");
        std::fs::create_dir(&git).expect("the git dir");
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").expect("HEAD");
        assert_eq!(
            git_branch(dir.path()),
            Some("feature/x".to_string()),
            "a symbolic ref names its branch"
        );

        std::fs::write(git.join("HEAD"), "951121ea12375166642754d3766f2a046638e82e").expect("HEAD");
        assert_eq!(
            git_branch(dir.path()),
            Some("951121e".to_string()),
            "a detached head shows its short commit id"
        );

        let sha256 = "5".repeat(64);
        std::fs::write(git.join("HEAD"), sha256).expect("HEAD");
        assert_eq!(
            git_branch(dir.path()),
            Some("5555555".to_string()),
            "a SHA-256 repository's 64-character detached head also shows its short id"
        );

        let worktree = tempfile::tempdir().expect("a linked worktree");
        std::fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", git.display()),
        )
        .expect("the gitdir pointer");
        std::fs::write(git.join("HEAD"), "ref: refs/heads/master\n").expect("HEAD");
        assert_eq!(
            git_branch(worktree.path()),
            Some("master".to_string()),
            "a worktree's pointer file resolves to the real HEAD"
        );
    }

    #[tokio::test]
    async fn a_failing_run_routes_its_error_into_the_shared_buffer() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do a thing");
        drop(tx);
        let listener = state.render_notify.listen();

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert!(
            queued_prompts(&state).is_empty(),
            "the driver claims the submission, leaving nothing waiting"
        );
        assert_eq!(
            claimed(&state),
            ["do a thing"],
            "the claimed prompt is handed to the transcript once, as sent"
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
        queue_prompt(&state, &tx, "do a thing");
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
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
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
        queue_prompt(&state, &tx, "do a thing");
        drop(tx);

        let flag = Arc::clone(&state.agent_running);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert!(
            !flag.load(Ordering::SeqCst),
            "the running flag settles false once the run lands, failed or not"
        );
        assert!(
            queued_prompts(&state).is_empty(),
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
            drive_submissions(runner, rx, state, shutdown_flag(false), parked_transcript()),
        )
        .await;
        assert!(ended.is_ok(), "a closed channel ends the driver promptly");
    }

    #[test]
    fn a_claim_the_exit_flag_gates_never_takes_its_entry() {
        // The exit's unclaimed-rows snapshot and a claim racing it read
        // the queue under one lock; the claim's half of that ordering
        // is the flag check inside `take_queued` itself. Without it, a
        // submission that passed the loop's earlier checks in the same
        // instant the flag rose would take its entry — spending it on
        // a run the exit is stopping — while the snapshot still read
        // it waiting, and the exit would echo it as never spent: the
        // prompt told twice, and resurrected on resume past its own
        // cancel record. The pin drives the guard directly: the flag
        // up, the entry present, the claim declines and the entry
        // stays.
        let (_, state) = TuiObserverState::new().into_observer();
        let id = push_queued(&state, "waiting when the exit fired".to_string());

        assert_eq!(
            take_queued(&state, id, &shutdown_flag(true)),
            None,
            "the claim declines under the flag — the exit's snapshot must \
             be able to read this entry as never spent"
        );
        assert_eq!(
            queued_prompts(&state)
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["waiting when the exit fired"],
            "a declined claim leaves its entry waiting — declined, not spent"
        );
        assert_eq!(
            take_queued(&state, id, &shutdown_flag(false)),
            Some("waiting when the exit fired".to_string()),
            "the flag down, the same claim takes its entry — the guard is \
             the exit's alone"
        );
    }

    #[tokio::test]
    async fn queued_prompts_do_not_run_after_shutdown() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "first");
        queue_prompt(&state, &tx, "second");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(true),
                parked_transcript(),
            ),
        )
        .await
        .expect("the driver leaves the queue behind instead of draining it");

        let errors = state.errors.lock().unwrap();
        assert!(
            errors.is_empty(),
            "no submission is admitted once shutdown has begun — neither queued task ran"
        );
        assert_eq!(
            queued_prompts(&state)
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"],
            "unclaimed prompts stay queued, in order, for the panel to show — the queue outlives the driver"
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
            parked_transcript(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        queue_prompt(&state, &tx, "late");
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
            queued_prompts(&state)
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["late"],
            "the overtaken prompt stays queued as unclaimed"
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

    #[test]
    fn the_exit_lines_label_and_name_the_resume_command() {
        let id = uuid::Uuid::new_v4();
        let lines = session_exit_line(id);
        let mut printed = lines.split('\n');
        assert_eq!(
            printed.next(),
            Some("Resume with:"),
            "the first line is the bare label"
        );
        let command = format!("dch --resume {id}");
        assert_eq!(
            printed.next(),
            Some(command.as_str()),
            "the second line is the copy-pasteable command"
        );
        assert_eq!(printed.next(), None, "the exit leaves exactly two lines");
    }

    #[test]
    fn an_initial_snapshot_persists_a_loadable_session_file() {
        // The exit line's promise holds from the first frame: the
        // initial snapshot writes the session file even when no turn
        // ever ends, so the printed command resumes something real.
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        handle.send(Vec::new(), crate::session::SessionTokens::default());
        worker.join();
        let messages = saved_messages(dir.path(), id);
        assert!(
            messages.is_empty(),
            "the pre-first-frame snapshot loads back as the empty session"
        );
    }

    #[test]
    fn an_initial_snapshot_preserves_restored_token_totals() {
        // A resume seeds all three counters before the first frame; an
        // exit with no new turn must keep the triple in the file.
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        let state = TuiObserverState::new();
        {
            let mut tokens = state.tokens.lock().expect("the tokens lock");
            tokens.cumulative_input = 9_999;
            tokens.cumulative_output = 111;
            tokens.input = 777;
        }
        handle.send(Vec::new(), snapshot_tokens(&state.tokens));
        worker.join();
        let tokens = saved_tokens(dir.path(), id);
        assert_eq!(
            tokens.cumulative_input, 9_999,
            "the restored input total survives a turnless exit"
        );
        assert_eq!(
            tokens.cumulative_output, 111,
            "the restored output total survives a turnless exit"
        );
        assert_eq!(
            tokens.last_input_tokens, 777,
            "the restored last-turn figure survives a turnless exit"
        );
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

    /// Read a worker-written transcript's token totals back.
    ///
    /// Same direct-envelope parse as [`saved_messages`], so a pin
    /// proves what the file itself records.
    fn saved_tokens(dir: &std::path::Path, id: uuid::Uuid) -> crate::session::SessionTokens {
        let path = dir.join(id.to_string()).join("session.json");
        let json = std::fs::read_to_string(path).expect("the transcript file");
        let envelope: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        serde_json::from_value(envelope.get("tokens").expect("the tokens field").clone())
            .expect("the tokens parse back")
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
                steered: false,
            };
            100
        ];
        let final_turn = vec![TuiMessage::User {
            text: "final turn".to_string(),
            timestamp: now,
            steered: false,
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
                steered: false,
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
                steered: false,
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

    #[tokio::test]
    async fn the_bridge_delivers_requests_and_replies() {
        let (resolver, mut requests_rx) = permission_bridge();
        let pending = resolver("Allow 'Bash' (ShellExecute) in AcceptEdits mode?", "Bash");
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), requests_rx.recv())
            .await
            .expect("the request arrives")
            .expect("the channel is open");
        assert_eq!(request.tool_name, "Bash");
        assert!(
            request.prompt.contains("Bash") && request.prompt.contains("AcceptEdits"),
            "the request carries the gate's prompt: {}",
            request.prompt
        );
        request.reply.send(true).expect("the reply channel is live");
        let answered = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .expect("the resolver future settles once replied");
        assert!(answered, "a reply of true resolves the ask as allow");
    }

    #[tokio::test]
    async fn a_dropped_request_resolves_as_a_denial() {
        let (resolver, mut requests_rx) = permission_bridge();
        let pending = resolver("Allow 'Bash'?", "Bash");
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), requests_rx.recv())
            .await
            .expect("the request arrives")
            .expect("the channel is open");
        drop(request);
        let answered = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .expect("a dropped request still settles the resolver");
        assert!(!answered, "an unanswered request reads as a denial");
    }

    /// One canned SSE text turn, framed for the OpenAI streaming wire.
    ///
    /// Two `data:` chunks — the text delta, then the stop — plus the
    /// terminal `[DONE]` sentinel, so a run that consumes one body
    /// completes a single turn cleanly.
    fn sse_text_turn(text: &str) -> String {
        [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"content": text}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": null, "finish_reason": "stop"}]
            }),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect()
    }

    /// A canned-SSE provider endpoint on an ephemeral local port.
    ///
    /// Serves the queued bodies one per accepted connection in order,
    /// records each request it reads, and sleeps the given delay
    /// before answering — long enough that a test can cancel a run
    /// while its provider request is provably still in flight.
    struct SseServer {
        /// The port the canned endpoint listens on.
        ///
        /// Ephemeral, and bound before the first request is sent, so
        /// the config can point straight at it.
        port: u16,

        /// Every request body the endpoint has read, in order.
        ///
        /// Shared with the accept task; a test reads the count to
        /// prove how many runs actually reached a provider.
        requests: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl SseServer {
        /// Start the endpoint with one body per connection and the delay.
        ///
        /// The accept loop outlives the return; a connection past the
        /// last queued body is answered with a bare 500 so an
        /// unexpected extra run still settles instead of hanging.
        /// Start the endpoint where each response carries its own delay.
        ///
        /// The shape an interleaving needs: some turns answer at once
        /// while one sits held, so a mid-flight event lands against
        /// exactly the run the test names.
        async fn start_timed(responses: Vec<(String, std::time::Duration)>) -> Self {
            Self::start_shared(responses).await
        }

        async fn start_with_delay(responses: Vec<String>, delay: std::time::Duration) -> Self {
            Self::start_shared(responses.into_iter().map(|body| (body, delay)).collect()).await
        }

        /// The accept loop both starters share.
        async fn start_shared(responses: Vec<(String, std::time::Duration)>) -> Self {
            // Bind here (not inside the task) so the port is known
            // before the first request is sent.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("ephemeral bind");
            let port = listener
                .local_addr()
                .expect("bound socket has an address")
                .port();
            let requests: Arc<std::sync::Mutex<Vec<String>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let task_requests = Arc::clone(&requests);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let mut queue: std::collections::VecDeque<(String, std::time::Duration)> =
                    responses.into();
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let body = queue.pop_front();
                    if let (Some(req), Ok(mut recorded)) =
                        (read_http_request(&mut sock).await, task_requests.lock())
                    {
                        recorded.push(req);
                    }
                    if let Some((_, wait)) = body.as_ref()
                        && *wait > std::time::Duration::ZERO
                    {
                        tokio::time::sleep(*wait).await;
                    }
                    let bytes = body.map_or_else(
                        || {
                            "HTTP/1.1 500 No canned response\r\nContent-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                                .to_string()
                        },
                        |(body, _)| {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        },
                    );
                    let _ = sock.write_all(bytes.as_bytes()).await;
                }
            });
            Self { port, requests }
        }

        /// How many requests have reached the endpoint so far.
        ///
        /// Each accepted connection records exactly once, so the
        /// count is the number of provider requests runs have made.
        fn request_count(&self) -> usize {
            self.requests.lock().map_or(0, |reqs| reqs.len())
        }
    }

    /// Read one HTTP request off the socket, headers plus body.
    ///
    /// Returns `None` on a truncated read; the caller then records
    /// nothing for the connection.
    async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
        use tokio::io::AsyncReadExt as _;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_header_end(&buf) {
                break pos;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let total = header_end + 4 + content_length;
        while buf.len() < total {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    /// The offset of the header/body boundary, if the bytes hold one.
    ///
    /// Scans for the blank-line separator the wire framing uses;
    /// `None` until the full four-byte sequence has arrived, so a
    /// partial header keeps the read loop waiting for more bytes.
    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    /// The prompt echoes waiting to become transcript rows.
    ///
    /// Read off the graduation stream rather than any buffer of their own,
    /// because an echo's place in the transcript is its place among the
    /// replies: a test that checked them separately would pass however
    /// they were ordered against everything else.
    fn claimed(state: &TuiObserverState) -> Vec<String> {
        state
            .graduations
            .lock()
            .expect("graduation lock")
            .iter()
            .filter_map(|graduation| match graduation {
                dch_tui::observer::Graduation::Sent { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The assistant turns' text blocks across a resolved history.
    ///
    /// Reads the machine snapshot the driver publishes after each run,
    /// so a pin can speak of the answers that landed rather than the
    /// requests that went out.
    fn assistant_texts(messages: &[loopctl::message::Message]) -> Vec<&String> {
        messages
            .iter()
            .filter(|message| matches!(message.role, loopctl::message::Role::Assistant))
            .flat_map(|message| {
                message.parts.iter().filter_map(|part| match part {
                    loopctl::message::MessagePart::Text { text } => Some(text),
                    _ => None,
                })
            })
            .collect()
    }

    /// An OpenAI-wire config pointing at the canned endpoint's port.
    ///
    /// The same shape as [`unreachable_config`], but live: a run
    /// against it issues a real request the endpoint answers on its
    /// own held-back schedule.
    fn sse_config(port: u16) -> dch_config::DchConfig {
        dch_config::DchConfig {
            api: dch_config::ApiConfig {
                api_type: ApiType::OpenAi,
                base_url: format!("http://127.0.0.1:{port}"),
                api_key: Some("dummy".to_string()),
                model: "test-model".to_string(),
                request_timeout_secs: 10,
                ..dch_config::ApiConfig::default()
            },
            ..dch_config::DchConfig::default()
        }
    }

    #[tokio::test]
    async fn a_cancelled_run_still_runs_the_prompt_queued_behind_it() {
        // The response is held back far longer than the test runs, so the
        // first run is provably mid-request when the cancel lands. What
        // the pin observes is the queued prompt's claim — the seam every
        // submit's turn takes — rather than its provider connection: the
        // canned endpoint answers one connection at a time behind the
        // delay, so a second request's arrival is not observable inside
        // this test's lifetime.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("too late")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "slow");
        queue_prompt(&state, &tx, "queued behind the cancel");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();

        let mut second_taken = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "queued behind the cancel")
            {
                second_taken = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            second_taken,
            "the prompt queued behind a cancelled run is claimed for its own \
             turn — one cancel stops the turn in flight, not the backlog: \
             claimed {:?}, waiting {:?}, errors {:?}, requests {}",
            claimed(&state),
            queued_prompts(&state),
            state.errors.lock().expect("errors lock"),
            server.request_count(),
        );
        assert!(
            queued_prompts(&state).is_empty(),
            "both prompts are claimed — the waiting section keeps no orphan"
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert_eq!(
                errors.first().map(String::as_str),
                Some(loopctl::error::LoopError::Cancelled.to_string().as_str()),
                "the cancelled run records its own failure; the queued prompt's \
                 turn is the next story, not part of this one"
            );
        }
        // The queued prompt's own request has no answer coming inside this
        // test's lifetime; the driver is stopped rather than left to wait
        // on it — the claim was already observed at the seam every turn
        // takes.
        driver.abort();
    }

    #[tokio::test]
    async fn a_run_ending_with_a_clean_plan_gets_no_nudge() {
        // The nudge exists for the filing-and-forgetting shape, not as
        // a tax on every run: a list the run closed — or a session
        // that never planned at all — costs nothing. The echo trail
        // proves the negative: no reminder text ever reaches the
        // transcript.
        let clean_script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "completed"}]
                }),
            ),
            sse_text_turn("all done"),
        ];
        let server =
            SseServer::start_with_delay(clean_script, std::time::Duration::from_millis(50)).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do the thing");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;

        let echoes = claimed(&state);
        assert!(
            !echoes.iter().any(|text| text.contains("reminder:")),
            "a clean plan nudges nothing: {echoes:?}"
        );
        assert_eq!(
            server.request_count(),
            2,
            "the tool-call turn and the speak turn — no third request, \
             because there was nothing to remind about"
        );
    }

    #[tokio::test]
    async fn a_run_stamps_its_start_and_clears_the_stamp_at_rest() {
        // The live elapsed line is only as honest as the stamp it
        // reads: the stamp rises beside the running flag at the
        // claim, stands for the run's whole flight — the nudge
        // included, which rides the same window — and returns to
        // zero with the flag, so nothing ticks between turns.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("the answer")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "a prompt");
        let down = shutdown_flag(false);
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&down),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            state.run_started_ms.load(Ordering::SeqCst) != 0,
            "the stamp stands while the run is in flight"
        );
        assert!(
            state.agent_running.load(Ordering::SeqCst),
            "the flag stands beside it"
        );
        // The exit path clears both together.
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        assert_eq!(
            state.run_started_ms.load(Ordering::SeqCst),
            0,
            "the stamp returns to zero with the flag"
        );
    }

    #[tokio::test]
    async fn the_exit_cancel_of_a_nudge_is_silent_and_reclaims_nothing() {
        // The nudge is a follow-up the session owes itself, not work
        // the user queued: the exit stopping it owes no error row and
        // reclaims nothing — the prompt's own turn already landed.
        let script = vec![
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({
                        "todos": [{"id": "only", "subject": "the work",
                                   "status": "pending"}]
                    }),
                ),
                std::time::Duration::ZERO,
            ),
            (sse_text_turn("working on it"), std::time::Duration::ZERO),
            (
                sse_text_turn("noted, but not closing it"),
                std::time::Duration::from_secs(30),
            ),
        ];
        let server = SseServer::start_timed(script).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt");
        let down = shutdown_flag(false);
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&down),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if server.request_count() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 3,
            "the nudge's request must be in flight before the exit lands"
        );
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let errors = state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            errors.is_empty(),
            "the exit's own stop of the nudge posts no row: {errors:?}"
        );
        drop(errors);
        assert!(
            queued_prompts(&state).is_empty(),
            "the landed turn's prompt is not requeued — only stopped work is"
        );
    }

    #[tokio::test]
    async fn the_exit_hands_stranded_corrections_back_too() {
        // A correction naming some other prompt can be buffered when
        // the exit's cancel lands — the display sends it the moment
        // an edit commits, claim or no claim. Its words are the
        // user's, and the words are already gone from the queue (the
        // prompt was claimed), so the exit collects every stranded
        // correction for the transcript echo rather than dropping
        // them with the channel.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("never lands")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "the wording that went out");
        let down = shutdown_flag(false);
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&down),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tx.send(Submission::Correct {
            id: id + 100,
            text: "a different prompt's settled wording".to_string(),
        })
        .expect("the foreign correction sends");
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            rescued
                .iter()
                .any(|text| text == "a different prompt's settled wording"),
            "the stranded correction's words are collected for the transcript echo — never \
             dropped with the channel: {rescued:?}"
        );
        drop(rescued);
        let queued = queued_prompts(&state);
        assert!(
            queued
                .iter()
                .any(|(_, text)| text == "the wording that went out"),
            "and the stopped run's own wording waits too: {queued:?}"
        );
    }

    #[tokio::test]
    async fn the_exit_reclaims_the_newest_wording_for_the_stopped_run() {
        // The words are the user's, and the newest wording is the
        // one a buffered correction carries: when the exit stops a
        // run whose correction was already on its way, the collected
        // wording — the one the transcript echo writes back as an
        // unclaimed row — is the corrected text, not the wording it
        // was replacing.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("never lands")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "the wording that went out");
        let down = shutdown_flag(false);
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&down),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tx.send(Submission::Correct {
            id,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            rescued.iter().any(|text| text == "the wording settled on"),
            "the collected correction carries the newest wording — the \
             newest words win: {rescued:?}"
        );
    }

    #[tokio::test]
    async fn an_exit_before_any_run_collects_the_buffered_correction() {
        // The exit drain, not the flag check alone: with the flag
        // already up, the driver never admits a submission, and
        // everything still buffered — a waiting prompt's run and a
        // correction behind it — splits by what its words need. The
        // run's entry stays in the queue the exit's snapshot reads;
        // the correction's words left no entry anywhere, so they are
        // collected for the transcript echo.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let waiting = queue_prompt(&state, &tx, "never started");
        tx.send(Submission::Correct {
            id: waiting,
            text: "the settled wording".to_string(),
        })
        .expect("the correction sends");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(true),
                parked_transcript(),
            ),
        )
        .await
        .expect("the driver settles at once under a raised flag");

        assert!(
            claimed(&state).is_empty(),
            "nothing runs once the flag is up: {:?}",
            claimed(&state)
        );
        let queued = queued_prompts(&state);
        assert!(
            queued.iter().any(|(_, text)| text == "never started"),
            "the unclaimed prompt's entry stays for the exit's snapshot: {queued:?}"
        );
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            rescued.as_slice(),
            ["the settled wording".to_string()].as_slice(),
            "the buffered correction is collected for the transcript echo, not \
             dropped with the channel"
        );
    }

    #[tokio::test]
    async fn a_correction_the_exit_overtakes_a_landed_run_is_collected() {
        // The run landed Ok and the exit won the race to the loop
        // top before the driver dequeued the correction committed
        // over it: the fix's words travelled only in the channel, and
        // dropping them there would lose the user's newest words
        // with no row anywhere — the exit drain collects them for
        // the transcript echo instead.
        let server = SseServer::start_timed(vec![(
            sse_text_turn("the answer that landed"),
            std::time::Duration::from_millis(300),
        )])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the wording that went out");
        let down = shutdown_flag(false);
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&down),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tx.send(Submission::Correct {
            id: running,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        down.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        assert_eq!(
            claimed(&state),
            ["the wording that went out".to_string()],
            "the run itself completed once — only its correction was overtaken"
        );
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            rescued.as_slice(),
            ["the wording settled on".to_string()].as_slice(),
            "the overtaken correction is collected for the transcript echo, not \
             dropped with the channel"
        );
    }

    #[tokio::test]
    async fn the_exit_flag_stops_the_todo_nudge_from_starting() {
        // A run that lands with work left on its plan earns its nudge
        // only while the session is staying: an exit already begun
        // owns the loop, and a nudge started under it would run the
        // engine's bookkeeping — and any request it can reach —
        // during teardown. The flag is up before the run lands, so
        // the nudge never starts: one provider request, one claim.
        // The run itself files the plan: its first turn is a TodoWrite
        // call, so the store holds an undone entry when the run lands
        // — the exact condition the nudge exists for.
        let server = SseServer::start_timed(vec![
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({"todos": [
                        {"id": "t1", "subject": "close the list", "status": "pending"}
                    ]}),
                ),
                std::time::Duration::from_millis(300),
            ),
            (
                sse_text_turn("the answer that landed"),
                std::time::Duration::ZERO,
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the prompt that ran");
        let down = shutdown_flag(false);
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&down),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        down.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let posted = server
            .requests
            .lock()
            .expect("request lock")
            .iter()
            .filter(|request| request.starts_with("POST "))
            .count();
        assert_eq!(
            posted, 2,
            "the run's two turns reached the provider and the nudge never started"
        );
        assert_eq!(
            claimed(&state),
            ["the prompt that ran".to_string()],
            "no nudge text is sent — the exit owns the session"
        );
    }

    #[tokio::test]
    async fn quitting_mid_run_hands_the_prompt_back_to_the_queue() {
        // The exit's cancel must not eat the prompt it stops: the
        // run is killed by the shutdown, the turn never landed, and
        // the words go back into the queue the session dies holding.
        // That queue is process state — the durable record is the
        // claim row already in the transcript, and a resume reads a
        // recorded stop as deliberate: the words are not lost, and
        // they are not re-run either.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("never lands")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt in flight");
        let down = shutdown_flag(false);
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            Arc::clone(&down),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) != u64::MAX {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // The exit sequence exactly as run_tui performs it: the
        // shutdown flag, then the cancel, then the driver's settle.
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let queued = queued_prompts(&state);
        assert!(
            queued
                .iter()
                .any(|(_, text)| text == "the prompt in flight"),
            "the stopped run's prompt waits again in the dying queue — its claim \
             row is the record: {queued:?}"
        );
        let errors = state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            errors
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
            "the exit's stop lands as the same cancelled record an \
             in-app cancel leaves: {errors:?}"
        );
        assert!(
            state
                .machine_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "the exit's settle publishes the machine half — the \
             turn-end hook that persists the record fires on it"
        );
    }

    #[tokio::test]
    async fn a_correction_during_the_todo_nudge_is_the_next_story_without_an_error_row() {
        // The nudge runs while the display still names the original
        // prompt as the live claim, so a correction committed against
        // that claim stops the nudge — the run that happened to be in
        // flight, not the turn the correction replaces. The buffered
        // correction is the next story: it runs as the very next turn
        // and no bare "cancelled" row surfaces for a stop the user
        // caused by saving an edit.
        let script = vec![
            // turn 1: file a one-item plan with the item pending, then
            // speak — the shape that arms the nudge.
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({
                        "todos": [{"id": "only", "subject": "the work",
                                   "status": "pending"}]
                    }),
                ),
                std::time::Duration::ZERO,
            ),
            (sse_text_turn("working on it"), std::time::Duration::ZERO),
            // the nudge's reply, held: the correction lands mid-nudge
            (
                sse_text_turn("noted, but not closing it"),
                std::time::Duration::from_secs(30),
            ),
            // the corrected turn's reply, answered at once
            (
                sse_text_turn("the settled answer"),
                std::time::Duration::ZERO,
            ),
        ];
        let server = SseServer::start_timed(script).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let superseded_id = queue_prompt(&state, &tx, "the wording that went out");
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        // The nudge is provably the request in flight: the server has
        // read it and is holding its answer.
        for _ in 0..1800 {
            if server.request_count() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 3,
            "the nudge's request must be in flight before the correction lands"
        );
        tx.send(Submission::Correct {
            id: superseded_id,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        state
            .correction_cancel
            .store(superseded_id, Ordering::SeqCst);
        cancel.cancel();

        let mut corrected_claimed = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "the wording settled on")
            {
                corrected_claimed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            corrected_claimed,
            "the buffered correction runs as the very next turn after the \
             nudge it stopped: claimed {:?}, errors {:?}",
            claimed(&state),
            state.errors.lock().expect("errors lock"),
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert!(
                !errors
                    .iter()
                    .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
                "the nudge's cancel is the correction's own hand, not an \
                 error to report — the corrected turn is the next story: \
                 {errors:?}"
            );
        }
        drop(tx);
        driver.abort();
    }

    #[tokio::test]
    async fn a_run_ending_with_undone_todos_gets_exactly_one_nudge() {
        // The filing-and-forgetting shape the tool contract forbids
        // but a model mid-flow still falls into: a run that lands
        // with work left on its own plan receives exactly one
        // follow-up reminding it to close the list. The nudge's echo
        // rides the steered flag — a transcript row for honesty, but
        // not a plan boundary — and the nudged run's own list is
        // final: whatever it says, the driver moves on.
        let todo_script = vec![
            // turn 1: file the plan (1 pending), then speak
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("working on it"),
            // turn 2 (the nudge's reply): speak without touching the
            // list — the nudge had its chance and the driver moves on
            sse_text_turn("noted, but not closing it"),
        ];
        let server =
            SseServer::start_with_delay(todo_script, std::time::Duration::from_millis(50)).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do the thing");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        // Wait for the driver to drain: both runs (the prompt and the
        // nudge) complete against their canned turns.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;

        assert_eq!(
            server.request_count(),
            3,
            "the prompt's run took two requests (the tool-call turn, \
             then the speak turn) and the nudge one more — and only \
             one more: the nudged run left the list as it was and the \
             driver moved on"
        );
        // The nudge's echo is in the transcript, flagged as not a
        // plan boundary.
        let echoes = claimed(&state);
        assert!(
            echoes.iter().any(|text| text.contains("reminder: 1 todo")),
            "the nudge's wording is the transcript's record: {echoes:?}"
        );
        // The nudge's echo rode the steered flag, so the display's
        // plan survived it — the Graduation::Sent drain clears the
        // plan only on unsteered echoes, and this test's state never
        // saw one after the prompt's own claim.
    }

    #[tokio::test]
    async fn the_driver_names_its_live_claim_while_the_turn_runs() {
        // The mirror the display's stale-edit guard reads: a claim
        // names the turn it took, and the landing clears the name back
        // to nothing. Held behind the delay, the turn is provably
        // mid-request when the mirror is read, so the pin observes the
        // name while the run is live — the only state in which a
        // correction may raise a cancel against it.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("slow answer")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "slow");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        let mut named = false;
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                named = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            named,
            "the claim names the turn it took while that turn runs"
        );
        cancel.cancel();
        let mut cleared = false;
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == u64::MAX {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            cleared,
            "the landing clears the mirror — no correction can name a \
             finished turn as the one in flight"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_cancel_the_correction_flag_does_not_name_keeps_its_error_row() {
        // The state the display's guard cannot exclude by timing
        // alone: its stand-down loses the race, and a correction's
        // flag is still standing when a cancelled run settles —
        // while that run's own claim names someone else. The settle
        // matches ids instead of trusting the race, so the stopped
        // turn keeps its row and the buffered correction runs on as
        // a turn of its own behind it.
        let server = SseServer::start_timed(vec![
            (sse_text_turn("held"), std::time::Duration::from_secs(2)),
            (
                sse_text_turn("replacement answered"),
                std::time::Duration::ZERO,
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "the running prompt");
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        let mut live = false;
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                live = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(live, "the prompt's turn is provably in flight");
        // The raced state, raised directly: a flag naming a turn
        // other than the one running, with that other correction
        // already buffered ahead of the cancel — the order the
        // display's commit path uses.
        state.correction_cancel.store(id + 100, Ordering::SeqCst);
        tx.send(Submission::Correct {
            id: id + 100,
            text: "the unrelated replacement".into(),
        })
        .expect("channel open");
        cancel.cancel();
        let mut reported = false;
        for _ in 0..1800 {
            if state
                .errors
                .lock()
                .expect("errors lock")
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string())
            {
                reported = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            reported,
            "a flag naming another run cannot hush this cancel: the \
             stopped turn posts its own error row"
        );
        // The correction was not spent silently: it is claimed as
        // its own turn behind the row, unrewound (its id names no
        // claim), and its echo is the turn's own record. The claim
        // and echo are the driver's contract; the engine's wire send
        // for a turn started immediately after a cancel is its own
        // reset race, not this settle's, so the pin reads the echo.
        let mut echoed = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "the unrelated replacement")
            {
                echoed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            echoed,
            "the buffered correction is claimed and echoed as its own \
             turn: claimed {:?}",
            claimed(&state)
        );
        drop(tx);
        driver.await.expect("driver settles cleanly");
        assert!(
            state
                .errors
                .lock()
                .expect("errors lock")
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
            "the stopped turn's row stands — no hushing for a foreign flag"
        );
    }

    #[tokio::test]
    async fn a_cancel_under_a_foreign_flag_keeps_its_error_row() {
        // The residual sliver the guard's stand-down leaves: the flag
        // still names an older turn when the cancel it raised stops a
        // run claimed under a newer id. Silence belongs only to the
        // run the flag names, so this settle posts its own error row —
        // the documented guarantee, held by comparing ids rather than
        // by trusting the two atomics' order.
        // The foreign prompt completes at once; the live prompt's
        // answer is held, so the cancel lands on the live run while
        // the flag names the finished one.
        let server = SseServer::start_timed(vec![
            (
                sse_text_turn("the older prompt's answer"),
                std::time::Duration::ZERO,
            ),
            (
                sse_text_turn("the answer that never lands"),
                std::time::Duration::from_secs(30),
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        // A pinned window keeps the discovery probes off the script:
        // every canned response belongs to a turn.
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        // A foreign id no live claim ever held: an older turn's
        // correction, buffered the way the display buffers it.
        let foreign = queue_prompt(&state, &tx, "an older prompt's wording");
        let live = queue_prompt(&state, &tx, "the claim in flight");
        let _ = foreign;
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == live {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            state.live_claim.load(Ordering::SeqCst),
            live,
            "the run under test is the live claim before the cancel lands"
        );
        tx.send(Submission::Correct {
            id: foreign,
            text: "an older correction".to_string(),
        })
        .expect("the stale correction sends");
        state.correction_cancel.store(foreign, Ordering::SeqCst);
        cancel.cancel();

        let mut row_appeared = false;
        for _ in 0..1800 {
            if state
                .errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string())
            {
                row_appeared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            row_appeared,
            "a cancel whose flag names another turn is an error to report — \
             the wrongly cancelled run posts its own row"
        );
        drop(tx);
        driver.abort();
    }

    #[tokio::test]
    async fn a_nudge_cancel_under_a_foreign_flag_keeps_its_error_row() {
        // The nudge's settle answers to the same id rule: mid-nudge
        // the claim the correction answers is still the prompt's own,
        // so a flag left standing for some other turn cannot hush the
        // nudge's cancel — its row stands, exactly as the turn's own
        // settle would post it.
        // The foreign prompt completes at once; the live prompt files
        // the plan and lands, and its nudge — the request held — is
        // the run the stale flag's cancel stops.
        let script = vec![
            (
                sse_text_turn("the older prompt's answer"),
                std::time::Duration::ZERO,
            ),
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({
                        "todos": [{"id": "only", "subject": "the work",
                                   "status": "pending"}]
                    }),
                ),
                std::time::Duration::ZERO,
            ),
            (sse_text_turn("working on it"), std::time::Duration::ZERO),
            (
                sse_text_turn("noted, but not closing it"),
                std::time::Duration::from_secs(30),
            ),
        ];
        let server = SseServer::start_timed(script).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let foreign = queue_prompt(&state, &tx, "an older prompt's wording");
        // The live prompt files the plan; its id rides last_claim
        // through the nudge without the test naming it.
        queue_prompt(&state, &tx, "the claim in flight");
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        for _ in 0..1800 {
            if server.request_count() >= 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 4,
            "the nudge's request must be in flight before the cancel lands"
        );
        tx.send(Submission::Correct {
            id: foreign,
            text: "an older correction".to_string(),
        })
        .expect("the stale correction sends");
        state.correction_cancel.store(foreign, Ordering::SeqCst);
        cancel.cancel();

        let mut row_appeared = false;
        for _ in 0..1800 {
            if state
                .errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string())
            {
                row_appeared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            row_appeared,
            "the nudge's settle answers to the same id rule — a foreign \
             flag cannot hush its cancel row"
        );
        drop(tx);
        driver.abort();
    }

    #[tokio::test]
    async fn a_stale_edit_correction_spares_the_turn_in_flight() {
        // The interleaving: the edit opens on
        // waiting prompt A, A's turn runs and lands, and a later
        // prompt is in flight when Enter saves the stale wording. The
        // correction names A — not the turn going — so nothing is
        // cancelled: the live turn lands its own answer, the fix runs
        // as a turn of its own behind it, and no error row reports a
        // stop nobody raised.
        fn press(app: &mut TuiApp, code: KeyCode, mods: KeyModifiers) {
            app.handle_event(&Event::Key(KeyEvent::new(code, mods)));
        }
        fn type_into(app: &mut TuiApp, text: &str) {
            for c in text.chars() {
                press(app, KeyCode::Char(c), KeyModifiers::NONE);
            }
        }
        fn render(app: &mut TuiApp) {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
        }

        let server = SseServer::start_with_delay(
            vec![
                sse_text_turn("first answered"),
                sse_text_turn("second answered"),
                sse_text_turn("fixed answered"),
            ],
            std::time::Duration::from_millis(300),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        // An explicit window keeps discovery's probe requests away from
        // the canned bodies: every connection this test makes is a
        // turn's own request, so the count at the end is exact.
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (observer, state) = TuiObserverState::new().into_observer();
        drop(observer);
        let mut app = TuiApp::from_observer_state(config, state.clone());
        let cancels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&cancels);
        app.set_run_canceller(Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        let (tx, rx) = mpsc::unbounded_channel();
        app.set_submit_tx(tx);

        // Two prompts queue, then the edit opens on the first before
        // the driver takes either — the claim of A happens under the
        // open edit, exactly as production cannot help doing.
        type_into(&mut app, "the wording that landed");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_into(&mut app, "the prompt left running");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
        let ids: Vec<u64> = {
            let queue = state.queued.lock().expect("queue lock");
            (0..queue.len())
                .filter_map(|index| queue.get(index).map(|entry| entry.id))
                .collect()
        };
        assert_eq!(ids.len(), 2, "both prompts wait under the open edit");
        let later_id = ids[1];

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        // A lands; the later prompt becomes the turn in flight — the
        // delay holds each answer long enough that the claim is
        // observably live.
        let mut later_live = false;
        for _ in 0..3600 {
            if state.live_claim.load(Ordering::SeqCst) == later_id {
                later_live = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            later_live,
            "the later prompt is the turn in flight when the stale edit saves"
        );

        // Enter saves the stale wording while that unrelated turn runs.
        type_into(&mut app, " — fixed after the fact");
        render(&mut app);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            cancels.load(Ordering::SeqCst),
            0,
            "a correction naming a turn that already landed cancels \
             nothing — the run in flight is not its conversation"
        );
        assert_eq!(
            state.correction_cancel.load(Ordering::SeqCst),
            u64::MAX,
            "the suppression flag stays empty, so the turn in flight keeps \
             whatever error row is rightly its own"
        );

        // The spared turn lands its own answer, then the fix runs as a
        // turn of its own — the resolved history carries both answers,
        // exactly the three requests the turns made, and nothing
        // reports a cancel.
        let mut both_answered = false;
        for _ in 0..3600 {
            let history = state.machine_history.lock().expect("history lock").clone();
            let answered = history.is_some_and(|messages| {
                let texts = assistant_texts(&messages[..]);
                texts.iter().any(|text| text.as_str() == "second answered")
                    && texts.iter().any(|text| text.as_str() == "fixed answered")
            });
            if answered {
                both_answered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            both_answered,
            "the spared turn lands and the correction runs behind it: {:?}",
            state
                .machine_history
                .lock()
                .expect("history lock")
                .as_ref()
                .map(|messages| {
                    assistant_texts(&messages[..])
                        .iter()
                        .map(|text| text.as_str())
                        .collect::<Vec<_>>()
                })
        );
        assert_eq!(
            server.request_count(),
            3,
            "exactly the three turns' requests reached the provider — no \
             cancels, no retries"
        );
        assert_eq!(
            cancels.load(Ordering::SeqCst),
            0,
            "the stale correction never raised the runner's cancel"
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert!(
                errors.is_empty(),
                "no run was cancelled and nothing failed: {errors:?}"
            );
        }
        drop(app); // releases the submission channel; the driver parks
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_correction_interrupts_the_run_it_replaces_without_an_error_row() {
        // The first turn is held mid-request when the correction lands,
        // sent exactly the way the display sends it — the Correct first,
        // the runner's cancel raised beside it. The driver must read the
        // cancellation as the correction's own hand: the superseded turn
        // settles cancelled, the corrected wording runs in its place
        // without waiting the held response out, and no cancel error row
        // surfaces for a stop the user caused by saving an edit.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("superseded answer")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let superseded_id = queue_prompt(&state, &tx, "the wording that went out");
        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // The display's exact order: the correction is sent, then the
        // cancel is raised beside it.
        tx.send(Submission::Correct {
            id: superseded_id,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        drop(tx);
        // The display names the correction on the shared flag before
        // raising the cancel — its way of saying whose hand it was.
        state
            .correction_cancel
            .store(superseded_id, Ordering::SeqCst);
        cancel.cancel();

        let mut corrected_claimed = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "the wording settled on")
            {
                corrected_claimed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            corrected_claimed,
            "the corrected wording runs without waiting the held turn out: \
             claimed {:?}, errors {:?}, requests {}",
            claimed(&state),
            state.errors.lock().expect("errors lock"),
            server.request_count(),
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert!(
                !errors
                    .iter()
                    .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
                "a cancel the correction raised is not an error to report — \
                 the corrected turn is the next story: {errors:?}"
            );
        }
        driver.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_host_seam_reaches_a_running_loop_and_gives_back_what_it_missed() {
        // The response is held far longer than the test needs, so the run is
        // provably in flight on both sides of the seam. This is the only test
        // that crosses the whole host path — app trait object, adapter,
        // engine slot, live loop — and it is the one that earns the
        // translation: text in, engine message out, and what no run delivered
        // back out as text again.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("too late")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("runner construction");
        let cancel = runner.cancel_signal();
        // Installed exactly as the app holds it: a shared trait object, which
        // is also the pin that the seam stays object-safe.
        let seam: Arc<dyn dch_tui::Guidance> = Arc::new(LoopGuidance {
            steering: runner.steering(),
        });
        let task = tokio::spawn(async move {
            let mut runner = runner;
            drop(runner.run("go").await);
        });

        let mut accepted = false;
        for _ in 0..400 {
            if seam.steer("the report is in the other directory".to_string()) {
                accepted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(accepted, "the seam reached the loop while it ran");

        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("the cancelled run settles promptly")
            .expect("the driving task does not panic");

        assert_eq!(
            seam.take_returned(),
            ["the report is in the other directory"],
            "what the run never read comes back through the adapter as text — \
             including the guidance the ended run left in its live queue, not \
             only what an earlier run sealed away"
        );
        assert!(
            seam.take_returned().is_empty(),
            "taking it empties the slot, so a later sweep cannot show it twice"
        );
    }

    #[tokio::test]
    async fn a_failed_run_still_runs_prompts_queued_behind_it() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "first");
        queue_prompt(&state, &tx, "second");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("two refused runs settle without hanging");

        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(
                errors.len(),
                2,
                "a failed — not cancelled — run admits the submission queued behind it"
            );
        }
        assert!(
            queued_prompts(&state).is_empty(),
            "both prompts are claimed"
        );
    }

    #[tokio::test]
    async fn a_stale_cancel_trip_does_not_eat_the_next_submission() {
        let runner = unreachable_runner().await;
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();

        // Trip the signal while nothing is running — the shape of a
        // cancel pressed just as a run was ending. The pre-run reset
        // must clear it, or the next submission dies instantly.
        cancel.cancel();
        queue_prompt(&state, &tx, "still runs");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("the run settles without hanging");

        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(
                errors.len(),
                1,
                "the submission runs rather than being eaten by the stale trip"
            );
            assert_ne!(
                errors.first().map(String::as_str),
                Some(loopctl::error::LoopError::Cancelled.to_string().as_str()),
                "the run must reach its provider failure, not die on a \
                 cancel nobody pressed for it"
            );
        }
    }

    #[tokio::test]
    async fn a_queued_prompt_runs_with_the_text_it_was_edited_to() {
        // Two slow turns: while the first is held back, the second prompt is
        // provably still waiting, so its text can change the way the panel's
        // edit key changes it — while its id is already in flight. The
        // endpoint answers one body per connection and a run opens more than
        // one (the client probes the endpoint before it posts), so the canned
        // bodies outlast the runs: a short queue would answer a probe with a
        // 500 and turn a probe into a retry.
        let server = SseServer::start_with_delay(
            (0..8).map(|_| sse_text_turn("an answer")).collect(),
            std::time::Duration::from_millis(250),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt already running");
        let edited = queue_prompt(&state, &tx, "the wording first typed");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            state
                .queued
                .lock()
                .expect("queue lock")
                .replace(edited, "the wording it was edited to".to_string()),
            "the edited prompt is still waiting when the edit lands"
        );

        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("both runs settle without hanging")
            .expect("the driver task itself does not panic");

        let requests = server.requests.lock().expect("request lock");
        let posted: Vec<&String> = requests
            .iter()
            .filter(|request| request.starts_with("POST "))
            .collect();
        assert_eq!(
            posted.len(),
            2,
            "exactly the two prompts are posted to a provider — a third would \
             mean a run failed and retried rather than ran"
        );
        assert!(
            posted[0].contains("the prompt already running"),
            "the running prompt is posted as submitted"
        );
        assert!(
            posted[1].contains("the wording it was edited to"),
            "the prompt is posted as edited — the claim reads the queue at turn \
             time, not the text that travelled the channel"
        );
        assert!(
            !posted[1].contains("the wording first typed"),
            "the superseded wording is posted to nobody, so the model never \
             reads a text the user replaced"
        );
    }

    #[tokio::test]
    async fn a_corrected_prompt_replaces_the_turn_that_already_went_out() {
        // The case the rewind seam exists for: the prompt is claimed, its
        // turn is in flight, and the user commits an edit to it meanwhile.
        // The fix has to go out, and the superseded turn has to stop
        // standing in the conversation — which the recorded request bodies
        // settle, since every request carries the whole history in front of
        // the prompt it is asking about.
        let server = SseServer::start_with_delay(
            (0..12).map(|_| sse_text_turn("an answer")).collect(),
            std::time::Duration::from_millis(250),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("the run and the correction settle without hanging")
            .expect("the driver task itself does not panic");

        let requests = server.requests.lock().expect("request lock");
        let posted: Vec<&String> = requests
            .iter()
            .filter(|request| request.starts_with("POST "))
            .collect();
        assert_eq!(
            posted.len(),
            2,
            "the superseded prompt and its correction are the two turns asked \
             of a provider"
        );
        assert!(
            posted[0].contains("the question as first asked"),
            "the prompt goes out as submitted, its own turn unwitnessed by any \
             later edit"
        );
        assert!(
            posted[1].contains("the question as corrected"),
            "the committed edit runs"
        );
        assert!(
            !posted[1].contains("the question as first asked"),
            "and it runs over a conversation the turn it replaces has left: a \
             history still carrying the superseded wording would have the model \
             read a question and its replacement as two asks"
        );
    }

    #[tokio::test]
    async fn a_correction_of_an_older_turn_runs_without_cutting_a_newer_one() {
        // A correction that names a turn which is no longer the newest
        // cannot have that turn cut out: an answer has been built on top of
        // it, and taking it back would take that answer with it. The fix
        // still runs, as a turn of its own — the user's words are never
        // dropped to protect a history that has already been answered.
        let server = SseServer::start_with_delay(
            (0..16).map(|_| sse_text_turn("an answer")).collect(),
            std::time::Duration::from_millis(50),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let first = queue_prompt(&state, &tx, "the first question");
        queue_prompt(&state, &tx, "the second question");

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        // Both turns settle while the channel is still open, so the
        // correction provably arrives after a later prompt has been claimed.
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        correct_prompt(&tx, first, "a fix to the first question");
        drop(tx);

        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("the third turn settles without hanging")
            .expect("the driver task itself does not panic");

        let requests = server.requests.lock().expect("request lock");
        let posted: Vec<&String> = requests
            .iter()
            .filter(|request| request.starts_with("POST "))
            .collect();
        assert_eq!(
            posted.len(),
            3,
            "the two prompts and the correction each ask a provider once"
        );
        assert!(
            posted[2].contains("a fix to the first question"),
            "the correction runs whatever the history around it looks like"
        );
        assert!(
            posted[2].contains("the first question") && posted[2].contains("the second question"),
            "and nothing is taken out of the conversation to make room for it: \
             a second answer already rests on the older turn"
        );
        assert!(
            state.errors.lock().expect("errors lock").is_empty(),
            "declining the cut is not a failure, so nothing is reported"
        );
    }

    #[tokio::test]
    async fn a_correction_cancelled_before_it_ran_is_promised_its_turn_again() {
        // A correction submitted while its turn is out is waiting in
        // the channel when the cancel lands. The cancel stops the run
        // in flight and nothing else — the correction is received as
        // the very next submission, so the section's showing it and
        // its running next are the same fact rather than a hope.
        //
        // The endpoint holds its answer far past the test, so the run
        // is provably mid-request when the cancel lands — the shape
        // every long-delay cancel test uses — and the correction's
        // own claim is observable the instant the loop receives it,
        // without waiting on a transport of its own.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("too late")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the prompt that went out");
        correct_prompt(&tx, running, "the fixed wording");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        // The settled moment every long-delay cancel test presses at:
        // the run is provably mid-request on an answer that is not
        // coming inside this test's lifetime.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();

        let mut claimed_the_fix = false;
        for _ in 0..1800 {
            let settled = claimed(&state)
                .iter()
                .any(|sent| sent == "the fixed wording")
                && queued_prompts(&state).is_empty();
            if settled {
                claimed_the_fix = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            claimed_the_fix,
            "the revived correction was claimed by its own turn and left \
             nothing waiting — the section never shows a promise nothing \
             keeps: claimed {:?}, waiting {:?}, errors {:?}, requests {}",
            claimed(&state),
            queued_prompts(&state),
            state.errors.lock().expect("errors lock"),
            server.request_count(),
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert_eq!(
                errors.first().map(String::as_str),
                Some(loopctl::error::LoopError::Cancelled.to_string().as_str()),
                "the cancelled run's failure is the first thing the session \
                 learned; the revived turn is still waiting on its own \
                 request, and its story is not the promise's"
            );
        }
        // The revived turn's request has no answer coming inside this
        // test's lifetime; the driver is stopped rather than left to
        // time out — the promise was already observed at its claim.
        driver.abort();
    }

    #[tokio::test]
    async fn a_dropped_prompt_is_never_sent() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt that stays");
        let dropped = queue_prompt(&state, &tx, "the prompt taken out of the list");
        drop(tx);
        // The panel's delete key, issued as the display issues it: the entry
        // leaves the queue while its id is still travelling, which is the
        // only shape of drop the driver can be handed.
        state.queued.lock().expect("queue lock").remove(dropped);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("the surviving run settles without hanging");

        {
            let errors = state.errors.lock().expect("errors lock");
            assert_eq!(
                errors.len(),
                1,
                "one id finds its prompt and one finds nothing — an id naming \
                 nothing starts no run, so a dropped prompt never reaches a \
                 provider"
            );
        }
        assert!(
            queued_prompts(&state).is_empty(),
            "the surviving prompt is claimed and the dropped one leaves nothing \
             stranded behind it"
        );
        assert_eq!(
            claimed(&state),
            ["the prompt that stays"],
            "a prompt removed before its turn is never shown as sent"
        );
    }

    /// A canned streaming response that ends its turn on a tool call.
    ///
    /// The shape a run needs to span more than one turn: a text-only answer
    /// closes the run at once, and compaction is only ever considered
    /// between turns, so a single-turn test cannot reach it however large
    /// the answer is.
    fn sse_tool_call_turn(tool: &str, args: &serde_json::Value) -> String {
        let args = args.to_string();
        [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1",
                    "function": {"name": tool, "arguments": ""}
                }]}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": args}
                }]}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": null, "finish_reason": "tool_calls"}]
            }),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect()
    }

    #[tokio::test]
    async fn a_correction_to_a_turn_already_folded_away_says_the_turn_stood() {
        // The superseded turn can be gone before the correction reaches it.
        // A run whose own turns carry the context past the compaction
        // threshold folds the older history into a summary partway through,
        // and the committed count falls below the length recorded when the
        // prompt was claimed. The cut the correction then asks for has
        // nothing left to remove — which the machine answers as success,
        // truly and uselessly, since the summary may still carry the wording
        // the user meant to withdraw. Silence there reads as "the turn you
        // replaced is gone".
        let dir = tempfile::tempdir().expect("temp workdir");
        // Read in one turn: enough to carry the accumulated history over the
        // compaction threshold at the next turn's check, and small enough
        // that no single turn's request reaches the guard above it — a
        // message too big to fit is a failed run, and the newest message is
        // the one a pass keeps.
        std::fs::write(
            dir.path().join("payload.txt"),
            "a line of payload for the compactor\n".repeat(40),
        )
        .expect("payload file for the compactor");
        // A committed history for the fold to work on: without older messages
        // to remove, compaction has nothing to do and the count never drops.
        let seeds: Vec<loopctl::message::Message> = (0..20)
            .map(|turn| {
                if turn % 2 == 0 {
                    loopctl::message::Message::user(format!("earlier exchange {turn}"))
                } else {
                    loopctl::message::Message::assistant(format!("earlier answer {turn}"))
                }
            })
            .collect();
        let server = SseServer::start_with_delay(
            vec![
                sse_tool_call_turn("Read", &serde_json::json!({"path": "payload.txt"})),
                sse_text_turn("settled"),
                sse_text_turn("the corrected answer"),
            ],
            std::time::Duration::ZERO,
        )
        .await;
        let mut config = sse_config(server.port);
        // A window the opening request fits under its threshold and the
        // read-in payload does not: the guard above the threshold refuses a
        // request outright, so a window smaller than one turn's own size
        // would fail the run rather than compact it.
        config.api.context_window = Some(9_000);
        // The truncating compactor, so the pass is the machine's own work:
        // the summarizers phone the provider for their summaries, and a
        // canned endpoint serving one body per connection cannot tell a run
        // request from a summarizer one — which would fail the pass rather
        // than test what follows it.
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .with_history(seeds)
            .build()
            .await
            .expect("offline runner construction with a tiny window");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
        drop(tx);

        let driver = tokio::spawn(drive_submissions(
            runner,
            rx,
            state.clone(),
            shutdown_flag(false),
            parked_transcript(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(60), driver)
            .await
            .expect("the run and the correction settle without hanging")
            .expect("the driver task itself does not panic");

        let posted: Vec<String> = server
            .requests
            .lock()
            .expect("request lock")
            .iter()
            .filter(|request| request.starts_with("POST "))
            .cloned()
            .collect();
        let seeds_carried = |request: &str| {
            (0..20)
                .filter(|seed| {
                    request.contains(&format!("earlier exchange {seed}"))
                        || request.contains(&format!("earlier answer {seed}"))
                })
                .count()
        };
        assert!(
            posted
                .first()
                .is_some_and(|request| seeds_carried(request) == 20)
                && posted
                    .last()
                    .is_some_and(|request| seeds_carried(request) < 20),
            "the precondition this test exists for: the older exchanges went out \
             of the history while the run was in flight, which is what leaves the \
             later cut with nothing to remove"
        );
        let errors = state.errors.lock().expect("errors lock");
        assert!(
            errors.iter().any(|error| error.contains("compacted away")),
            "a correction whose turn the history had already shrunk past says so, rather than leaving the superseded wording standing inside a summary the user cannot see: {errors:?}"
        );
        drop(errors);
        let requests = server.requests.lock().expect("request lock");
        assert!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST "))
                .any(|request| request.contains("the question as corrected")),
            "the correction still runs: an unavailable cut is a reason to explain, not a reason to refuse"
        );
    }

    #[tokio::test]
    async fn a_correction_to_a_turn_the_window_refused_says_stopped_not_compacted() {
        // A run the context window refuses outright discards even its
        // own prompt — committing it back would wedge every later run
        // against the same overflow — so the superseded turn leaves
        // the committed history exactly as long as it found it. The
        // correction's cut then has nothing to remove, and the row it
        // earns names the stop rather than a compaction that never
        // ran, while the corrected wording still goes out in the
        // stopped turn's place.
        let server = SseServer::start_timed(vec![(
            sse_text_turn("never reached"),
            std::time::Duration::ZERO,
        )])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(1);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("the refused run and the correction settle without hanging");

        let errors = state.errors.lock().expect("errors lock");
        assert!(
            errors
                .first()
                .is_some_and(|error| error.contains("Context window exceeded")),
            "the precondition this test exists for: the turn died at the window guard, before any \
             provider request: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("stopped before it wrote anything")),
            "the correction names the stopped turn it replaces, not a compaction: {errors:?}"
        );
        assert!(
            !errors.iter().any(|error| error.contains("compacted")),
            "nothing folded — the history never shrank — so no row claims it did: {errors:?}"
        );
        drop(errors);
        assert_eq!(
            claimed(&state),
            ["the question as first asked", "the question as corrected"],
            "the corrected wording is sent in the stopped turn's place"
        );
    }

    #[tokio::test]
    async fn a_correction_after_a_failed_run_stays_silent_on_a_clean_cut() {
        // An ordinary failure salvages the turn's prompt into the
        // committed history — the run's own work is what never lands —
        // so the correction's cut removes exactly that salvaged prompt
        // and the replace is clean. Silence is the truth of it: the
        // compaction wording belongs to a history that shrank below
        // the cut, and a failed run shrinks nothing.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("the failed run and the correction settle without hanging");

        let errors = state.errors.lock().expect("errors lock");
        assert_eq!(
            errors.len(),
            2,
            "two failed runs, two failure rows — the correction between them posts no notice of \
             its own: {errors:?}"
        );
        assert!(
            !errors.iter().any(|error| error.contains("compacted")),
            "a clean cut of the salvaged prompt is not a fold, and must not be reported as one: {errors:?}"
        );
    }

    #[tokio::test]
    async fn a_claimed_prompt_echoes_into_the_transcript_exactly_once() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "first prompt");
        queue_prompt(&state, &tx, "second prompt");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("two refused runs settle without hanging");

        assert_eq!(
            claimed(&state),
            ["first prompt", "second prompt"],
            "one row per prompt that ran, in the order they ran — the display \
             echoes what the driver published, so a second publish would put \
             the same prompt in the transcript twice, and once is what the \
             model was actually given"
        );
    }

    #[test]
    fn a_peek_keeps_every_submission_its_place() {
        // The interrupt's channel peek: pulling the correction the display
        // named must not eat anything queued around it — a prompt behind
        // the correction rides `revived` back to the front of the queue,
        // a correction that names nothing walks the whole channel into
        // `revived` untouched, and the drain order puts every pulled
        // submission back ahead of what never left the channel.
        // The prompt queues first, the correction behind it — the interrupt
        // finds its name at the back of the line, and everything the peek
        // walked past to get there rides `revived` in the channel's own
        // order, the correction closing the line so the drain replays it
        // next.
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Submission::Run { id: 8 })
            .expect("the queued prompt sends");
        tx.send(Submission::Correct {
            id: 7,
            text: "the fix".to_string(),
        })
        .expect("the correction sends");
        let mut revived = std::collections::VecDeque::new();
        assert!(
            pull_pending_correction(7, &mut rx, &mut revived),
            "the named correction is found"
        );
        assert_eq!(
            revived.len(),
            2,
            "the peek ate nothing: what it walked past and the correction \
             both ride revived"
        );
        assert!(
            matches!(revived[0], Submission::Run { id: 8 }),
            "the prompt the peek walked past rides revived, not the void"
        );
        assert!(
            matches!(revived[1], Submission::Correct { id: 7, .. }),
            "the matched correction rides revived too — the drain replays \
             it next"
        );

        // A name the channel does not hold: the peek walks everything into
        // `revived` — the unnamed correction included — and answers false,
        // so the run records its cancel as the user's own.
        tx.send(Submission::Correct {
            id: 1,
            text: "someone else's fix".to_string(),
        })
        .expect("the stray correction sends");
        tx.send(Submission::Run { id: 9 })
            .expect("the later prompt sends");
        let mut revived = std::collections::VecDeque::new();
        assert!(
            !pull_pending_correction(123, &mut rx, &mut revived),
            "a name the channel does not hold is not the display's cancel"
        );
        assert_eq!(
            revived.len(),
            2,
            "the walk kept every pulled submission for the drain"
        );
        assert!(
            matches!(revived[0], Submission::Correct { id: 1, .. })
                && matches!(revived[1], Submission::Run { id: 9 }),
            "revived keeps the channel's own order"
        );
    }
}
