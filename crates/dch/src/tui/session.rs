//! Session assembly: the path from parsed args and a resume
//! control to a running TUI session — runner assembly, seeding
//! of restored history and tokens, teardown, and the exit lines
//! the user copies when the session ends.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dch_tui::ResumedTokens;
use dch_tui::SessionUpdate;
use dch_tui::TerminalGuard;
use dch_tui::TuiApp;
use dch_tui::TuiObserverState;
use tokio::sync::mpsc;

use crate::args::Args;
use crate::headless::resolve_session_config_pair;
use crate::resume::ResumeControl;

use super::commands::{adopt_resumed_permissions, post_session_notice};
use super::driver::{DriverContext, RunOverrides, retirement_publisher, run_driver_loop};
use super::permissions_bridge::{
    InteractiveLanes, LoopGuidance, fail_permission_restore_closed_at_startup, report_fresh_warn,
    wire_permission_memory,
};
use super::transcript::{
    MachineSnapshot, TranscriptHandle, TranscriptWorker, recorded_compactions, snapshot_tokens,
};

/// Publish the resumed machine history to the shared state and the
/// transcript worker.
///
/// The adoption half of a resume: the loaded machine snapshot is
/// filtered (a file saved before a hide may carry the nudge's prompt
/// as a user message), published to the shared state the display and
/// the driver read, and sent to the transcript worker so the first
/// save stamps the same history.
fn seed_resumed_machine(
    machine: Option<&[loopctl::message::Message]>,
    state: &TuiObserverState,
    conversation: &[dch_tui::TuiMessage],
    transcript: &TranscriptHandle,
) {
    let Some(machine) = machine else {
        return;
    };
    let adopted = filtered_machine_history(&std::sync::Arc::new(machine.to_vec()));
    *state
        .machine_history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&adopted));
    let compactions = recorded_compactions(conversation);
    transcript.send_machine(MachineSnapshot {
        messages: adopted,
        saved_at: chrono::Utc::now(),
        compactions,
        saver: None,
    });
}

/// Seed the shared token counters from a resumed session's envelope.
///
/// The display half of a resume's accounting: the session's
/// cumulative figures continue from the file's own record instead of
/// restarting at zero, so the context bar and `/cost` read the
/// conversation's totals from the first frame. A fresh control
/// leaves the counters untouched — a new session starts at zero by
/// definition.
fn seed_resumed_tokens(state: &TuiObserverState, control: &ResumeControl) {
    if let ResumeControl::Resumed(outcome) = control {
        let mut tokens = state
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens.cumulative_input = outcome.tokens.cumulative_input;
        tokens.cumulative_output = outcome.tokens.cumulative_output;
        tokens.input = outcome.tokens.last_input_tokens;
        tokens.context_tokens = outcome.tokens.context_tokens;
    }
}

/// Cancel the runner the session ends on, through the shared cell.
///
/// The exit's cooperative stop: the cell a rebuild swapped names the
/// runner in flight at exit, which the launch-captured signal may no
/// longer be, so the stop goes to the live handle the session holds.
fn cancel_the_live_runner(state: &TuiObserverState) {
    state
        .cancel_signal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .cancel();
}

/// The launch saver: the resolved identity under the model the loop
/// starts with, in the workdir the session serves, carrying the name
/// a launch resume adopts.
///
/// One constructor call lifted out of the session setup so the setup
/// reads as the wiring it is; the title is the resumed session's own —
/// `None` on a fresh start, where the first claimed prompt's naming
/// call coins one.
pub(crate) fn launch_saver(
    session_id: uuid::Uuid,
    model: String,
    workdir: &std::path::Path,
    title: Option<String>,
    permission_mode: Option<dch_config::PermissionMode>,
) -> Arc<crate::session::SessionSaver> {
    Arc::new(
        crate::session::SessionSaver::with_model(session_id, model)
            .with_workdir(workdir)
            .with_title(title)
            .with_permission_mode(permission_mode),
    )
}

/// Lifted for length: the launch identity and the save target it
/// writes through.
///
/// Consumes the resume control into the session id — a resumed display
/// is seeded with its transcript, a degraded fresh start says its warn
/// — and builds the saver: model, workdir, resumed title, and the
/// session's standing permission override recorded together. The
/// override is the CLI flag's choice when it named one — from this run
/// forward the session's own — otherwise the resumed file's record,
/// and none at all on a fresh start without the flag. Returns the
/// saver, the id the status bar shows, and the machine snapshot the
/// engine seeding consumes later.
pub(crate) fn launch_identity(
    app: &mut TuiApp,
    control: ResumeControl,
    runner: &dch_loop::Runner,
    args: &Args,
    flag_mode: dch_config::PermissionMode,
    model: String,
    workdir: &std::path::Path,
) -> (
    Arc<crate::session::SessionSaver>,
    uuid::Uuid,
    Option<Vec<loopctl::message::Message>>,
) {
    let mut resumed_machine: Option<Vec<loopctl::message::Message>> = None;
    let mut resumed_title: Option<String> = None;
    let mut resumed_permission_mode: Option<dch_config::PermissionMode> = None;
    let session_id = match control {
        ResumeControl::Resumed(outcome) => {
            resumed_machine = outcome.machine;
            resumed_title = outcome.title;
            resumed_permission_mode = outcome.permission_mode;
            app.seed_messages(outcome.messages);
            outcome.session_id
        }
        ResumeControl::Fresh { session_id, warn } => {
            report_fresh_warn(app, warn);
            session_id.unwrap_or_else(|| runner.session_id())
        }
    };
    app.set_session_id(session_id.to_string());
    let permission_override = if args.permission_mode.is_some() {
        Some(flag_mode)
    } else {
        resumed_permission_mode
    };
    (
        launch_saver(
            session_id,
            model,
            workdir,
            resumed_title,
            permission_override,
        ),
        session_id,
        resumed_machine,
    )
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
/// model the session last ran — the envelope's own record — unless
/// the CLI names a model outright, and the session id is the loaded
/// one so auto-saves keep writing the resumed file.
///
/// # Errors
///
/// Returns a human-readable message when terminal initialization
/// fails or the session's runner cannot be constructed.
pub(crate) async fn run_tui_session(args: &Args, control: ResumeControl) -> Result<(), String> {
    if let ResumeControl::Fresh {
        warn: Some(warn), ..
    } = &control
    {
        crate::signals::report(&format!("dch: {warn}"));
    }
    let (config, config_base, config_file) = resolve_session_config_pair(args, &control)?;
    let flag_mode = config.runner.permission_mode;
    let workdir = std::env::current_dir().map_err(|err| format!("cannot determine cwd: {err}"))?;
    let (observer, state) = TuiObserverState::new()
        .with_secret_redaction(config.security.redact_secrets)
        .into_observer();
    seed_resumed_tokens(&state, &control);
    let (lanes, permission_rx, prompt_rx, permission_memory) = InteractiveLanes::bridges(&config);
    let shutting_down = Arc::new(AtomicBool::new(false));
    let mut builder =
        assemble_runner_builder(&config, &workdir, observer, &state, &lanes, &shutting_down);
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
    // restores the terminal before the bridge's own exit. The live
    // handles the display reads — stored once here and after every
    // runner rebuild, so Esc,Esc and Ctrl+S always reach the CURRENT
    // runner. The guidance share is the panel's own steer handle.
    let guidance: Option<Arc<dyn dch_tui::Guidance>> = Some(Arc::new(LoopGuidance {
        steering: runner.steering(),
    }));
    state.adopt_runner_handles(runner.cancel_signal(), guidance.clone());
    let signal_bridge =
        crate::signals::install_live_cancel_handler(Arc::clone(&state.cancel_signal), || {
            let _restored = dch_tui::restore_terminal();
        });

    let questions = lanes.open(&runner);
    let (submit_tx, submit_rx) = mpsc::unbounded_channel();
    let (session_tx, session_rx) = mpsc::unbounded_channel();
    let model = config.api.model.clone();
    let mut driver_config = config.clone();
    let driver_base = Arc::new(config_base.clone());
    let mut app = TuiApp::from_observer_state(config, state.clone());
    app.set_config_base(config_base);
    let file_permission_default = config_file.runner.permission_mode;
    app.set_config_file(config_file);
    app.set_submit_tx(submit_tx);
    app.set_guidance(guidance.clone());
    app.set_permission_requests(permission_rx);
    app.set_permission_memory(permission_memory.clone());
    app.set_permission_mode_cell(Some(Arc::clone(&lanes.mode_cell)));
    app.set_permission_config_rules_cell(Some(Arc::clone(&lanes.config_rules_cell)));
    app.set_permission_preset_cell(Some(Arc::clone(&lanes.preset_cell)));
    app.set_permission_adoption_guard(Some(Arc::clone(&lanes.adoption_guard)));
    app.set_question_requests(prompt_rx);
    app.set_session_requests(session_rx);
    app.set_working_dir(&workdir);
    app.set_config_persist_dir(Some(dch_config::config_dir()));
    app.set_git_branch(git_branch(&workdir));
    if let Some(notice) = dch_tui::terminal::tmux_color_notice() {
        app.post_notice(notice);
    }
    let (saver, _session_id, resumed_machine) =
        launch_identity(&mut app, control, &runner, args, flag_mode, model, &workdir);
    let (transcript_handle, transcript_worker) = TranscriptWorker::spawn(saver);
    let initial_transcript = transcript_handle.clone();
    let permission_restore = wire_permission_memory(&permission_memory, &initial_transcript);
    if permission_restore.damage_stands() {
        fail_permission_restore_closed_at_startup(
            &mut app,
            &mut driver_config,
            &lanes,
            &session_tx,
        );
    }
    let exit_saver = Arc::clone(&initial_transcript.saver);
    install_turn_end_hook(&mut app, &state, transcript_handle);

    initial_transcript.send(app.conversation().to_vec(), snapshot_tokens(&state.tokens));

    TerminalGuard::install_panic_hook();
    let (guard, mut terminal) = TerminalGuard::new(driver_config.display.mouse_capture)
        .map_err(|err| format!("terminal init: {err}"))?;
    let canvas_local = dch_tui::terminal::canvas_painted_locally();
    app.set_canvas_painted_locally(canvas_local);
    if !canvas_local {
        if dch_tui::sync_default_background(app.theme.ui.background) {
            dch_tui::set_cursor_color(app.theme.ui.primary);
        } else {
            app.theme = app.theme.deferred_to_terminal();
        }
    }
    dch_tui::arm_fatal_signal_restore();

    let cancel = runner.cancel_signal();
    let cancel_for_app = Arc::clone(&cancel);
    app.set_run_canceller(Box::new(move || cancel_for_app.cancel()));
    app.set_shutdown_flag(Arc::clone(&shutting_down));
    seed_resumed_machine(
        resumed_machine.as_deref(),
        &state,
        app.conversation(),
        &initial_transcript,
    );
    let mut driver_context = session_driver_context(
        driver_config,
        driver_base,
        workdir.clone(),
        state.clone(),
        Arc::clone(&shutting_down),
        lanes,
        args,
    );
    driver_context.overrides.permission_default = file_permission_default;
    let driver = tokio::spawn(Box::pin(run_driver_loop(
        runner,
        submit_rx,
        driver_context,
        initial_transcript,
        session_tx,
    )));

    let session = app.run(&mut terminal).await;
    shutting_down.store(true, Ordering::SeqCst);
    let unclaimed = app.waiting_prompt_texts_at_exit();
    app.settle_exit_stop(&mut terminal);
    app.echo_waiting_prompts_at_exit(unclaimed);
    questions.abort();
    cancel_the_live_runner(&state);
    tear_down_driver_bounded(driver).await;
    app.echo_rescued_corrections_at_exit();
    drop(app);
    signal_bridge.stop().await;
    restore_and_print_exit(
        transcript_worker,
        guard,
        &exit_saver,
        state.todos_undone_captured(),
    );
    session.map_err(|err| format!("terminal error: {err}"))
}

/// Assemble the driver's context from the session's resolved pieces.
///
/// Lifted for length: the session builder hands over its live and base
/// configs, its shared state, and the run's CLI statements, and the
/// sessions root and flag overrides land here — the one place that
/// knows where the driver loads resumes from and which flags it owes
/// precedence for the session's whole life.
pub(crate) fn session_driver_context(
    config: dch_config::DchConfig,
    config_base: Arc<dch_config::DchConfig>,
    workdir: std::path::PathBuf,
    state: TuiObserverState,
    shutting_down: Arc<AtomicBool>,
    lanes: InteractiveLanes,
    args: &Args,
) -> DriverContext {
    DriverContext {
        config,
        config_base,
        workdir,
        sessions_root: crate::session::sessions_dir(),
        state,
        shutting_down,
        lanes,
        overrides: RunOverrides {
            model: args.model.clone(),
            reasoning_effort: args.config.reasoning_effort.clone(),
            permission_named: args.permission_mode.is_some(),
            permission_default: dch_config::PermissionMode::default(),
        },
    }
}

/// Join the writer inside the deaf window, then print the exit lines
/// as one write.
///
/// The teardown tail, lifted for length: the transcript writer joins
/// with fatal signals deaf so a straggler interrupt cannot take the
/// process out mid-join, and before the terminal restore stands the
/// fatal net down outright — its `_exit(130)` would drop whatever
/// stdout still buffers, the resume command included. The exit lines
/// carry the session's last fact beside the command: plan entries the
/// final run left undone, stated for the user who decides whether to
/// resume. `None` means no run settled a figure to state — the exit says
/// the plan was never read rather than claiming it finished clean.
/// The command's id is read from the save target after the join — the
/// session the LAST save wrote, whatever a `/clear` rotation or a
/// `/resume` retarget left in the cell, never the id the session
/// started under.
fn restore_and_print_exit(
    transcript_worker: TranscriptWorker,
    guard: dch_tui::TerminalGuard,
    saver: &Arc<std::sync::Mutex<Arc<crate::session::SessionSaver>>>,
    todos_undone: Option<usize>,
) {
    dch_tui::enter_signal_deaf_window();
    transcript_worker.join();
    dch_tui::exit_signal_deaf_window();
    dch_tui::ignore_fatal_signals();
    drop(guard);
    let session_id = saver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .session_id();
    print_session_exit(session_id, todos_undone);
}

/// Assemble the session's runner builder with every host axis attached.
///
/// The one place the TUI's registration set lives: the display
/// observer, the input-capturing middleware, the permission bridge,
/// the todo nudge — whose echo is deliberately the no-op (the
/// reminder must never surface as the user's words anywhere a user
/// reads; the turn's own answer is the only trace it leaves) and
/// whose leave-to-extend predicate answers from the session's
/// shutdown flag, so an exit already begun declines the reminder
/// turn before any request is spent on it — and the retirement
/// publisher, so each claim's dropped plan entries reach the display
/// the moment the claim makes them, while the run it precedes is
/// still in flight. A quit that races between the predicate's
/// acceptance and its own cancel may still pay for a reminder turn
/// the teardown then stops — the accepted residual of checking a
/// flag beside any engine turn.
pub(crate) fn assemble_runner_builder<'a>(
    config: &'a dch_config::DchConfig,
    workdir: &std::path::Path,
    observer: dch_tui::TuiObserver,
    state: &dch_tui::TuiObserverState,
    lanes: &InteractiveLanes,
    shutting_down: &Arc<AtomicBool>,
) -> dch_loop::RunnerBuilder<'a> {
    let nudge_gate = Arc::clone(shutting_down);
    let retirements = retirement_publisher(state);
    dch_loop::Runner::builder(config, workdir)
        .with_observer(Arc::new(observer) as Arc<dyn loopctl::observer::LoopObserver>)
        .with_middleware(Arc::new(
            dch_tui::CapturingMiddleware::new_with_input_redaction(
                Arc::clone(&state.tool_captures),
                config.security.redact_secrets,
            ),
        ))
        .with_permission_resolver(Arc::clone(&lanes.permission))
        .with_permission_mode_cell(Arc::clone(&lanes.mode_cell))
        .with_permission_config_rules(lanes.config_rules_view())
        .with_permission_memory(lanes.memory_snapshot())
        .with_retirement_sink(retirements)
        .with_todo_nudge(
            move |_reminder| {
                // No row: the reminder must not surface as the user's
                // words anywhere a user reads — not the transcript, not
                // the saved files, not the listing. The turn itself is
                // the engine's to run; its answer lands in the
                // transcript on its own.
            },
            move || !nudge_gate.load(Ordering::SeqCst),
        )
}

/// Await the driver with a bound, then tear it down.
///
/// The exit is prompt even when the run will not die politely: a
/// wedged tool or a stubborn stream gets a bounded wait, then the
/// task is torn down — the session's halves were persisted by the
/// settle above, so nothing more can be saved by waiting. Whatever the
/// run had already settled for the exit lines stands: the bound is on
/// the join, not on what the run reported before it.
pub(crate) async fn tear_down_driver_bounded(driver: tokio::task::JoinHandle<()>) {
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
                messages: filtered_machine_history(&history),
                saved_at: chrono::Utc::now(),
                compactions,
                saver: None,
            });
        }
    }));
}

/// The machine history a save persists, minus the nudge's follow-up
/// prompt.
///
/// The end-of-run nudge's reminder reaches the engine's committed
/// history as a user message — the engine's only post-run mechanism
/// works that way — and persisting it would replay the reminder as the
/// user's own words on every resume. The display owes it no row and
/// the file owes it no message: the shared filter drops exactly the
/// wording the hook emits (see
/// [`dch_loop::history_without_nudge`]) and merges the follow-up
/// turn's answer into the assistant message it now directly follows,
/// so the saved history keeps the alternating shape providers
/// replay.
pub(crate) fn filtered_machine_history(
    history: &Arc<Vec<loopctl::message::Message>>,
) -> Arc<Vec<loopctl::message::Message>> {
    Arc::new(dch_loop::history_without_nudge(history))
}

/// The exit lines the TUI prints once a session ends.
///
/// A `Resume with:` label, then the copy-pasteable command on its own
/// line — and, when the session ends with unfinished plan entries, a
/// third line stating that work as a fact (`N todo item(s) left
/// unfinished`). Nothing else, so the exit leaves exactly the
/// instruction and the standing work behind. An initial snapshot
/// persists before the first frame, so the file the command names
/// exists however the session ended; printed after the transcript
/// writer joins and the terminal is restored, on clean exits and
/// failures alike.
fn session_exit_line(session_id: uuid::Uuid, todos_undone: Option<usize>) -> String {
    let resume = format!("Resume with:\ndch --resume {session_id}");
    match todos_undone {
        Some(0) => resume,
        Some(undone) => {
            let plural = if undone == 1 { "" } else { "s" };
            format!("{resume}\n{undone} todo item{plural} left unfinished")
        }
        None => format!(
            "{resume}\nthe run ended before its plan was read — dch cannot \
             say whether todo items stand open"
        ),
    }
}

/// Print the exit lines as one write, once the terminal is restored.
///
/// `println!` would send the label and the command as separate
/// writes — the formatter emits the argument and its newline as two
/// pieces, and the line writer flushes only through the first
/// newline, leaving the command buffered in userspace between them.
/// Anything that ends the process in that gap — above all the deaf
/// fatal handler's `_exit(130)` — drops the buffered half, leaving
/// the label standing over a command that never printed. One
/// `write_all` of the whole two-line string leaves through a single
/// flush, so the lines reach the terminal together or not at all.
fn print_session_exit(session_id: uuid::Uuid, todos_undone: Option<usize>) {
    use std::io::Write as _;

    let mut lines = session_exit_line(session_id, todos_undone);
    lines.push('\n');
    let mut stdout = std::io::stdout().lock();
    drop(stdout.write_all(lines.as_bytes()));
    drop(stdout.flush());
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

/// Fold the identity's newest queued snapshots into a loaded resume.
///
/// A snapshot still waiting on the writer is newer than the file it
/// will become: the adopted outcome reads the queued display, totals,
/// and machine history — compaction count included — instead of the
/// stale file contents, so the resumed runner cannot step back over
/// a publish the stalled writer has not flushed yet. The queued
/// state's own model, permission mode, and recorded name ride with
/// it — each is newer than the file's record whenever the queued
/// snapshot exists — and a launch-flag-named mode still outranks the
/// queued one.
fn adopt_queued_snapshots(
    outcome: &mut crate::resume::ResumeOutcome,
    queued: crate::tui::transcript::QueuedForResume,
) {
    outcome.messages = queued.messages;
    outcome.tokens = queued.tokens;
    outcome.model = queued.model;
    outcome.permission_mode = queued.permission_mode;
    if queued.title.is_some() {
        outcome.title = queued.title;
    }
    if let Some((machine, compactions)) = queued.machine {
        outcome.machine = Some(
            std::sync::Arc::try_unwrap(machine).unwrap_or_else(|shared| shared.as_ref().clone()),
        );
        outcome.machine_compactions = compactions;
    }
}

/// The mode a resumed session runs under, and the mode it records.
///
/// A flag named at launch governs both: the live config's mode stands
/// and is recorded onto the save target. Otherwise the resumed file's
/// own mode stands — falling back to the file default the launch
/// recorded when the file names none — and only a saved mode is
/// recorded, since an absent one means the configuration governs.
fn resumed_modes(
    context: &DriverContext,
    outcome: &crate::resume::ResumeOutcome,
) -> (
    dch_config::PermissionMode,
    Option<dch_config::PermissionMode>,
) {
    let standing_mode = if context.overrides.permission_named {
        context.config.runner.permission_mode
    } else {
        outcome
            .permission_mode
            .unwrap_or(context.overrides.permission_default)
    };
    let recorded_mode = if context.overrides.permission_named {
        Some(standing_mode)
    } else {
        outcome.permission_mode
    };
    (standing_mode, recorded_mode)
}

/// The live resume: adopt a saved session's history, file, and display.
///
/// The driver alone can load: it parses the id, reads the saved pair
/// from the sessions root, rebuilds the runner on the restored machine
/// history — under the saved session's own model, adopted through its
/// `[[models]]` entry exactly as a launch resume adopts it, unless the
/// run's `--model` flag named one outright — with the same read-guard
/// baselines a launch resume builds, restores the token accounting into
/// the shared state, retargets the
/// save target onto the session's own file, and publishes the machine
/// snapshot under it — the target must own the pin before the adopted
/// history publishes, or the write lands under the outgoing session
/// and its newer stamp hands a later resume the wrong pair. Only then
/// does the loaded transcript travel to the display, which re-seeds
/// from it. Every failure — a malformed id, an unreadable session, a
/// refused rebuild — posts a notice and leaves the loop exactly as it
/// stood. Returns whether the resumed runner replaced the loop's
/// conversation — every refusal keeps it and reports `false`, so the
/// driver can tell a boundary the old conversation named from one the
/// new one owns.
pub(crate) async fn resume_into_session(
    runner: &mut dch_loop::Runner,
    transcript: &TranscriptHandle,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    context: &mut DriverContext,
    id: String,
) -> bool {
    let Ok(target) = uuid::Uuid::parse_str(&id) else {
        post_session_notice(
            session_tx,
            format!("{id} is not a session id — nothing was resumed"),
        );
        return false;
    };
    let queued = transcript.latest_queued_for(target);
    let machine_floor = queued.as_ref().map(|waiting| waiting.saved_at);
    let Ok(mut outcome) =
        crate::resume::load_for_resume_in_with_floor(target, &context.sessions_root, machine_floor)
    else {
        post_session_notice(
            session_tx,
            format!("session {id} could not be loaded — the loop kept its conversation"),
        );
        return false;
    };
    if let Some(queued) = queued {
        adopt_queued_snapshots(&mut outcome, queued);
    }
    let machine =
        crate::resume::history_for_resume(&outcome, context.config.security.redact_secrets);
    let mut config = context.config.clone();
    if context.overrides.model.is_none() && !outcome.model.trim().is_empty() {
        let mut fresh = context.config_base.as_ref().clone();
        if fresh.select_model(Some(&outcome.model)).is_err() {
            fresh.api.model.clone_from(&outcome.model);
        }
        config.api = fresh.api;
    }
    crate::headless::reapply_reasoning_effort(
        &mut config,
        context.overrides.reasoning_effort.as_deref(),
    );
    let (standing_mode, recorded_mode) = resumed_modes(context, &outcome);
    config.runner.permission_mode = standing_mode;
    let observer = context.state.clone().into_observer().0;
    let new_runner = assemble_runner_builder(
        &config,
        &context.workdir,
        observer,
        &context.state,
        &context.lanes,
        &context.shutting_down,
    )
    .with_history(machine.clone())
    .build()
    .await;
    let built = match new_runner {
        Ok(built) => {
            context.lanes.attach(&built);
            built
        }
        Err(err) => {
            post_session_notice(
                session_tx,
                format!(
                    "session {id} could not be adopted — the loop kept its conversation ({err})"
                ),
            );
            return false;
        }
    };
    for path in crate::resume::resumed_read_paths_with_machine(
        &outcome.messages,
        outcome.machine.as_deref(),
    ) {
        let _recorded = built.record_resumed_read(&path).await;
    }
    let window = built.context_window();
    *runner = built;
    context.state.context_window.store(window, Ordering::SeqCst);
    context.state.adopt_runner_handles(
        runner.cancel_signal(),
        Some(Arc::new(LoopGuidance {
            steering: runner.steering(),
        })),
    );
    context.config = config;
    *context
        .lanes
        .mode_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = standing_mode;
    {
        let mut tokens = context
            .state
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens.cumulative_input = outcome.tokens.cumulative_input;
        tokens.cumulative_output = outcome.tokens.cumulative_output;
        tokens.input = outcome.tokens.last_input_tokens;
        tokens.context_tokens = outcome.tokens.context_tokens;
    }
    let adopted = filtered_machine_history(&Arc::new(machine));
    *context
        .state
        .machine_history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&adopted));
    transcript.retarget(
        target,
        &context.config.api.model,
        outcome.title.clone(),
        recorded_mode,
    );
    let (enforced_mode, permissions_damaged) =
        adopt_resumed_permissions(transcript, context, standing_mode);
    if permissions_damaged {
        context.config.runner.permission_mode = enforced_mode;
        post_session_notice(
            session_tx,
            "the saved permissions are damaged — remembered rules were dropped; \
             side effects ask, and the configured rules stay tightened, until \
             /permissions repair (the damage marker rides inside the permissions \
             file — hand-fixing the file leaves it standing)"
                .to_string(),
        );
    }
    transcript.send_machine(MachineSnapshot {
        messages: adopted,
        saved_at: chrono::Utc::now(),
        compactions: recorded_compactions(&outcome.messages),
        saver: None,
    });
    let update = SessionUpdate::Resumed {
        id: target.to_string(),
        transcript: outcome.messages,
        tokens: ResumedTokens {
            cumulative_input: outcome.tokens.cumulative_input,
            cumulative_output: outcome.tokens.cumulative_output,
            last_input: outcome.tokens.last_input_tokens,
            context_tokens: outcome.tokens.context_tokens,
        },
        permission_mode: enforced_mode,
    };
    if session_tx.send(update).is_err() {
        tracing::warn!("session resumed but the display is gone");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::transcript::TranscriptWorker;
    use clap::Parser as _;
    use std::sync::Arc;

    #[test]
    fn a_flag_preset_typo_refuses_the_interactive_startup() {
        // The interactive startup shares the headless resolution, so
        // the flag's typo gets the same named refusal — the error
        // lists the presets that do exist, and run_tui carries it to
        // its own report-and-exit rather than opening a session that
        // silently runs no preset at all.
        let tmp = tempfile::TempDir::new().expect("config tempdir");
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            concat!(
                "[permissions.presets.rust-dev]\n",
                "rules = [{ outcome = \"allow\", shell_prefix = \"cargo test\" }]\n",
            ),
        )
        .expect("the config file lands");
        let args = Args::try_parse_from([
            "dch",
            "--config",
            path.to_str().expect("a utf-8 tempdir path"),
            "--permissions-preset",
            "rust-ev",
        ])
        .expect("the flags parse");
        let control = crate::resume::ResumeControl::Fresh {
            session_id: None,
            warn: None,
        };
        let err = resolve_session_config_pair(&args, &control)
            .expect_err("a typo'd selection refuses the startup");
        assert!(
            err.contains("rust-dev"),
            "the refusal names the preset that does exist: {err}"
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

    #[test]
    fn the_exit_lines_state_unfinished_work_as_a_fact() {
        // The engine offers one follow-up per run and the follow-up's
        // own end never another, so a long run can settle with plan
        // entries still undone and no further nudge coming — the exit
        // line owes the user that fact, beside the resume command.
        let id = uuid::Uuid::new_v4();
        let lines = session_exit_line(id, Some(4));
        let command = format!("dch --resume {id}");
        assert!(
            lines.contains(&command),
            "the resume command keeps its place: {lines}"
        );
        assert!(
            lines.contains("4 todo items left unfinished"),
            "the unfinished count rides the exit lines: {lines}"
        );
        let single = session_exit_line(id, Some(1));
        assert!(
            single.contains("1 todo item left unfinished"),
            "the singular spelling for one entry: {single}"
        );
        let clean = session_exit_line(id, Some(0));
        assert!(
            !clean.contains("unfinished"),
            "a clean plan's exit stays the plain two lines: {clean}"
        );
    }

    #[test]
    fn an_unread_plan_is_never_reported_as_a_clean_one() {
        // The figure the exit lines print comes from the driver, and a
        // driver taken by the exit's bounded join records nothing. The
        // absence has to arrive as its own answer: printed as zero it
        // would tell the user the plan closed clean over a session the
        // save still carries as unfinished.
        let id = uuid::Uuid::new_v4();
        let unknown = session_exit_line(id, None);
        let command = format!("dch --resume {id}");
        assert!(
            unknown.contains(&command),
            "the resume command survives an unread plan: {unknown}"
        );
        assert!(
            !unknown.contains("left unfinished") && !unknown.contains("clean"),
            "the exit states its uncertainty rather than either verdict: {unknown}"
        );
        assert!(
            unknown.contains("before its plan was read"),
            "the unknown names itself as such: {unknown}"
        );
    }

    #[test]
    fn the_exit_lines_label_and_name_the_resume_command() {
        let id = uuid::Uuid::new_v4();
        let lines = session_exit_line(id, Some(0));
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
    fn the_exit_command_names_the_session_the_last_save_wrote() {
        // The exit line's id follows the save target: a `/clear`
        // rotation and a `/resume` retarget both swap the cell, and the
        // command a user copies at exit must name the session those
        // saves went to — never the one the session started under.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let first = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            first,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let exit_reads = || {
            handle
                .saver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_id()
        };
        assert_eq!(
            exit_reads(),
            first,
            "before any move, the opening id stands"
        );
        let rotated = handle.rotate("m");
        worker.join();
        assert_eq!(
            exit_reads().to_string(),
            rotated,
            "a cleared session's exit names the fresh id"
        );
        let adopted = uuid::Uuid::new_v4();
        handle.retarget(adopted, "m", None, None);
        assert_eq!(
            exit_reads(),
            adopted,
            "a resumed session's exit names the adopted id"
        );
    }
}
