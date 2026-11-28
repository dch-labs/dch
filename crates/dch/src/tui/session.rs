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
    store_machine_history(state, &adopted);
    let compactions = recorded_compactions(conversation);
    transcript.send_machine(machine_snapshot(adopted, compactions));
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
        seed_token_counters(state, outcome.tokens);
    }
}

/// Continue the shared token counters from a session's own figures.
///
/// The accounting core both resume paths drive — the launch through
/// [`seed_resumed_tokens`], a mid-session `/resume` directly — so the
/// four figures continue as one set: the cumulative totals, the last
/// turn's input, and the context the display's bar reads.
fn seed_token_counters(state: &TuiObserverState, from: crate::session::SessionTokens) {
    let mut tokens = state
        .tokens
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokens.cumulative_input = from.cumulative_input;
    tokens.cumulative_output = from.cumulative_output;
    tokens.input = from.last_input_tokens;
    tokens.context_tokens = from.context_tokens;
}

/// The machine half of a save, stamped at the moment it publishes.
///
/// Every publish of the adopted history — the launch's seed, a
/// mid-session resume's re-publish, the turn-end hook's own — stamps
/// `saved_at` at publication and leaves the save target the
/// transcript already holds: the snapshot travels without retargeting
/// the writer.
fn machine_snapshot(
    messages: Arc<Vec<loopctl::message::Message>>,
    compactions: u32,
) -> MachineSnapshot {
    MachineSnapshot {
        messages,
        saved_at: chrono::Utc::now(),
        compactions,
        saver: None,
    }
}

/// Publish the adopted machine history into the shared state.
///
/// The cell both resume paths hand the display and the driver to read
/// the engine's history from; the filter is the caller's, so what
/// stands in the cell is exactly what the caller adopted.
fn store_machine_history(state: &TuiObserverState, adopted: &Arc<Vec<loopctl::message::Message>>) {
    *state
        .machine_history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(adopted));
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
/// The launch's saver, built in one call so the setup reads as the
/// wiring it is; the title is the resumed session's own — `None` on a
/// fresh start, where the first claimed prompt's naming call coins
/// one.
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

/// The launch identity and the save target it writes through.
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

/// Say the degraded fresh start's reason before the terminal opens.
///
/// A fresh control built from a resume that degraded carries its
/// reason; the user hears it on the normal terminal, where a failure
/// reported before the alternate screen can still be read.
fn report_startup_warn(control: &ResumeControl) {
    if let ResumeControl::Fresh {
        warn: Some(warn), ..
    } = control
    {
        crate::signals::report(&format!("dch: {warn}"));
    }
}

/// Bring up the shared observer state a session publishes into.
///
/// The display's cells and the engine's events share this one state
/// from the first frame; the runner builder derives its own observer
/// from a clone of it (see [`build_session_runner`]), so nothing is
/// split off here.
fn new_display_state(redact_secrets: bool) -> TuiObserverState {
    TuiObserverState::new().with_secret_redaction(redact_secrets)
}

/// Build the launch's runner, resumed reads already re-recorded.
///
/// The launch's construction as one step: the shared build mounts
/// whatever machine history the resumed control brought (a fresh
/// start mounts none), and once the runner stands, the files the
/// resumed conversation had read are re-recorded on it — so the
/// launch owes the same guard re-seeding a mid-session resume
/// performs, and cannot forget it.
///
/// # Errors
///
/// Propagates the shared build's construction failure; the launch
/// words it as its own agent-construction message.
async fn build_launch_runner(
    config: &dch_config::DchConfig,
    workdir: &std::path::Path,
    state: &TuiObserverState,
    lanes: &InteractiveLanes,
    shutting_down: &Arc<AtomicBool>,
    control: &ResumeControl,
) -> Result<dch_loop::Runner, dch_loop::RunnerError> {
    let history = resume_history_for_launch(control, config.security.redact_secrets);
    let runner =
        build_session_runner(config, workdir, state, lanes, shutting_down, history).await?;
    if let ResumeControl::Resumed(outcome) = control {
        record_resumed_reads(&runner, outcome).await;
    }
    Ok(runner)
}

/// Install the cooperative interrupt bridge over the live cell.
///
/// The first external SIGINT or SIGTERM cancels the run in flight;
/// the forced second press restores the terminal before the bridge's
/// own exit. The bridge relays through the shared cancel-signal cell,
/// so a press always reaches the CURRENT runner — whoever adoption
/// last named.
fn arm_interrupt_bridge(state: &TuiObserverState) -> crate::signals::CancelBridge {
    crate::signals::install_live_cancel_handler(Arc::clone(&state.cancel_signal), || {
        let _restored = dch_tui::restore_terminal();
    })
}

/// Wire the display whole, and open the run's channels.
///
/// Both halves of the app's wiring in one place — the persisting half
/// (both config halves, the submit and save channels, the directory,
/// persist target, and branch the place row shows) and the
/// interactive half (the permission lane's receiver and memory beside
/// it, the lane cells the display mirrors and steers through, and the
/// question receiver the panel answers back along). The steer share
/// is the parent's to install. The run's two channels open here: the
/// half the driver consumes and the half the driver publishes the
/// session's updates through come back; their display ends ride onto
/// the app.
fn wire_display_app(
    app: &mut TuiApp,
    config_base: dch_config::DchConfig,
    config_file: dch_config::DchConfig,
    permission_rx: mpsc::UnboundedReceiver<dch_tui::PermissionRequest>,
    prompt_rx: mpsc::UnboundedReceiver<dch_tui::QuestionPrompt>,
    lanes: &InteractiveLanes,
    workdir: &Path,
) -> (
    mpsc::UnboundedReceiver<dch_tui::Submission>,
    mpsc::UnboundedSender<SessionUpdate>,
) {
    let (submit_tx, submit_rx) = mpsc::unbounded_channel();
    let (session_tx, session_rx) = mpsc::unbounded_channel();
    app.set_config_base(config_base);
    app.set_config_file(config_file);
    app.set_submit_tx(submit_tx);
    app.set_session_requests(session_rx);
    app.set_working_dir(workdir);
    app.set_config_persist_dir(Some(dch_config::config_dir()));
    app.set_git_branch(git_branch(workdir));
    app.set_permission_requests(permission_rx);
    app.set_permission_memory(lanes.memory.clone());
    app.set_permission_mode_cell(Some(Arc::clone(&lanes.mode_cell)));
    app.set_permission_config_rules_cell(Some(Arc::clone(&lanes.config_rules_cell)));
    app.set_permission_preset_cell(Some(Arc::clone(&lanes.preset_cell)));
    app.set_permission_adoption_guard(Some(Arc::clone(&lanes.adoption_guard)));
    app.set_question_requests(prompt_rx);
    (submit_rx, session_tx)
}

/// Open the transcript worker and stage the session's first save.
///
/// The persistence half of the launch's staging: the worker opens on
/// the launch saver; the saved permission memory is replayed into the
/// lane cells — and when its file was damaged the session closes the
/// permission lane outright before the first frame, the same refusal a
/// headless run makes; the turn-end hook goes onto the app beside the
/// handle it publishes through; and the opening snapshot rides out
/// immediately, so the file the exit lines name exists before the first
/// frame can fail. Returns the handle the driver and the machine seed
/// publish through, the worker to join at the exit, and the saver cell
/// the exit lines read the final id from.
fn stage_transcript_worker(
    app: &mut TuiApp,
    driver_config: &mut dch_config::DchConfig,
    state: &TuiObserverState,
    lanes: &InteractiveLanes,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    saver: Arc<crate::session::SessionSaver>,
    permission_memory: &dch_tui::PermissionMemory,
) -> (
    TranscriptHandle,
    TranscriptWorker,
    Arc<std::sync::Mutex<Arc<crate::session::SessionSaver>>>,
) {
    let (transcript_handle, transcript_worker) = TranscriptWorker::spawn(saver);
    let initial_transcript = transcript_handle.clone();
    let permission_restore = wire_permission_memory(permission_memory, &initial_transcript);
    if permission_restore.damage_stands() {
        fail_permission_restore_closed_at_startup(app, driver_config, lanes, session_tx);
    }
    let exit_saver = Arc::clone(&initial_transcript.saver);
    install_turn_end_hook(app, state, transcript_handle);
    initial_transcript.send(app.conversation().to_vec(), snapshot_tokens(&state.tokens));
    (initial_transcript, transcript_worker, exit_saver)
}

/// Bring the alternate screen up and settle the theme onto it.
///
/// The panic hook first, then the guard and canvas together, then the
/// theme's own decision: a terminal that answers the default-
/// background query keeps its canvas as authored, one that does not
/// gets the deferred palette — and the fatal-signal restore net goes
/// up the moment the screen is entered, so an uncatchable death still
/// hands the user a sane terminal.
///
/// # Errors
///
/// A terminal-initialization failure is reported as `terminal init:`
/// beside the system's own reason, before the alternate screen is on.
fn open_session_terminal(
    app: &mut TuiApp,
    mouse_capture: bool,
) -> Result<(TerminalGuard, dch_tui::SessionTerminal), String> {
    TerminalGuard::install_panic_hook();
    let (guard, terminal) =
        TerminalGuard::new(mouse_capture).map_err(|err| format!("terminal init: {err}"))?;
    if dch_tui::sync_default_background(app.theme().ui.background) {
        dch_tui::set_cursor_color(app.theme().ui.primary);
    } else {
        app.set_theme(app.theme().deferred_to_terminal());
    }
    dch_tui::arm_fatal_signal_restore();
    Ok((guard, terminal))
}

/// Spawn the driver loop that owns the runner for the session's
/// life.
///
/// The engine enters on the boxed future the session runs it
/// through, holding the runner, both channel halves, and the
/// context it drives under; the handle comes back for the exit's
/// bounded join — nothing else waits on the run.
fn spawn_session_driver(
    runner: dch_loop::Runner,
    submit_rx: mpsc::UnboundedReceiver<dch_tui::Submission>,
    driver_context: DriverContext,
    initial_transcript: TranscriptHandle,
    session_tx: mpsc::UnboundedSender<SessionUpdate>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(Box::pin(run_driver_loop(
        runner,
        submit_rx,
        driver_context,
        initial_transcript,
        session_tx,
    )))
}

/// Wire the display's own stop onto the live runner's signal.
///
/// The canceller the display's Esc,Esc reaches, and the shutdown flag
/// a begun exit raises so a reminder turn declines before it is paid
/// for, are armed together from the runner the screen now owns.
fn arm_run_cancellation(
    app: &mut TuiApp,
    runner: &dch_loop::Runner,
    shutting_down: &Arc<AtomicBool>,
) {
    let cancel = Arc::clone(&runner.cancel_signal());
    app.set_run_canceller(Box::new(move || cancel.cancel()));
    app.set_shutdown_flag(Arc::clone(shutting_down));
}

/// Run the display to its end, then close every lane in order.
///
/// The session's whole teardown as one ordered close: the run loop
/// ends; the shutdown flag goes up before any later claim can race a
/// reminder turn; the settled frame stops with its waiting prompts
/// re-echoed — then the question supervision is pulled, the LIVE runner
/// is cancelled through the shared cell, the driver joins bounded, the
/// late graduations and rescued corrections land in the transcript's
/// final writes, the app drops with the terminal still guarded, and the
/// signal bridge stands down. The run's own result comes back last: a
/// draw failure is reported only after every one of those has run.
///
/// # Errors
///
/// Propagates the draw failure the display's run ended with, after
/// every lane has closed.
async fn close_the_session_run(
    mut app: TuiApp,
    terminal: &mut dch_tui::SessionTerminal,
    state: &TuiObserverState,
    shutting_down: &Arc<AtomicBool>,
    questions: tokio::task::JoinHandle<()>,
    driver: tokio::task::JoinHandle<()>,
    signal_bridge: crate::signals::CancelBridge,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = app.run(terminal).await;
    shutting_down.store(true, Ordering::SeqCst);
    let unclaimed = app.waiting_prompt_texts_at_exit();
    app.settle_exit_stop(terminal);
    app.echo_waiting_prompts_at_exit(unclaimed);
    questions.abort();
    cancel_the_live_runner(state);
    tear_down_driver_bounded(driver).await;
    app.drain_late_graduations_at_exit();
    app.echo_rescued_corrections_at_exit();
    drop(app);
    signal_bridge.stop().await;
    session
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
    report_startup_warn(&control);
    let (config, config_base, config_file) = resolve_session_config_pair(args, &control)?;
    let flag_mode = config.runner.permission_mode;
    let workdir = std::env::current_dir().map_err(|err| format!("cannot determine cwd: {err}"))?;
    let state = new_display_state(config.security.redact_secrets);
    seed_resumed_tokens(&state, &control);
    let (lanes, permission_rx, prompt_rx, permission_memory) = InteractiveLanes::bridges(&config);
    let shutting_down = Arc::new(AtomicBool::new(false));
    let runner = build_launch_runner(&config, &workdir, &state, &lanes, &shutting_down, &control)
        .await
        .map_err(|err| format!("agent construction: {err}"))?;
    let signal_bridge = arm_interrupt_bridge(&state);
    let guidance = adopt_live_runner(&state, &runner);
    let questions = lanes.open(&runner);
    let model = config.api.model.clone();
    let mut driver_config = config.clone();
    let driver_base = Arc::new(config_base.clone());
    let file_permission_default = config_file.runner.permission_mode;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    app.set_guidance(guidance);
    let (submit_rx, session_tx) = wire_display_app(
        &mut app,
        config_base,
        config_file,
        permission_rx,
        prompt_rx,
        &lanes,
        &workdir,
    );
    let (saver, _session_id, resumed_machine) =
        launch_identity(&mut app, control, &runner, args, flag_mode, model, &workdir);
    let (initial_transcript, transcript_worker, exit_saver) = stage_transcript_worker(
        &mut app,
        &mut driver_config,
        &state,
        &lanes,
        &session_tx,
        saver,
        &permission_memory,
    );
    let (guard, mut terminal) =
        open_session_terminal(&mut app, driver_config.display.mouse_capture)?;

    arm_run_cancellation(&mut app, &runner, &shutting_down);
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
    let driver = spawn_session_driver(
        runner,
        submit_rx,
        driver_context,
        initial_transcript,
        session_tx,
    );

    let session = close_the_session_run(
        app,
        &mut terminal,
        &state,
        &shutting_down,
        questions,
        driver,
        signal_bridge,
    )
    .await;
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
/// Assemble the driver's context from the session's resolved pieces.
///
/// The session builder hands over its live and base configs, its
/// shared state, and the run's CLI statements, and the sessions root
/// and flag overrides land here — the one place that knows where the
/// driver loads resumes from and which flags it owes precedence for
/// the session's whole life.
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
/// Join the transcript writer, restore the terminal, and print the
/// exit lines.
///
/// The transcript writer joins with fatal signals deaf so a straggler
/// interrupt cannot take the process out mid-join, and before the
/// terminal restore stands the fatal net down outright — its
/// `_exit(130)` would drop whatever stdout still buffers, the resume
/// command included. The exit lines carry the session's last fact
/// beside the command: plan entries the final run left undone, stated
/// for the user who decides whether to resume. `None` means no run
/// settled a figure to state — the exit says the plan was never read
/// rather than claiming it finished clean.
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

/// The machine history a resumed launch seeds the engine with.
///
/// The engine half of the control's shape: only a resumed control
/// carries a machine record to seed from, and a fresh start runs on
/// an empty history by definition.
fn resume_history_for_launch(
    control: &ResumeControl,
    redact_secrets: bool,
) -> Option<Vec<loopctl::message::Message>> {
    match control {
        ResumeControl::Resumed(outcome) => {
            Some(crate::resume::history_for_resume(outcome, redact_secrets))
        }
        ResumeControl::Fresh { .. } => None,
    }
}

/// Build the session's runner with every host axis attached.
///
/// Both session entries construct the live agent through here: a
/// fresh launch builds its first runner and a mid-session `/resume`
/// rebuilds the live one the same way — the registration set
/// assembled, the machine history mounted only when the resume's own
/// record brought one, and the build awaited. The observer rides the
/// shared state's own clone, so the engine's events land in the cells
/// the display already reads. Each caller words its own failure; the
/// construction itself says nothing about which entry asked.
///
/// # Errors
///
/// Propagates the engine's own construction failure, verbatim; the
/// caller words it for its path.
async fn build_session_runner(
    config: &dch_config::DchConfig,
    workdir: &std::path::Path,
    state: &TuiObserverState,
    lanes: &InteractiveLanes,
    shutting_down: &Arc<AtomicBool>,
    history: Option<Vec<loopctl::message::Message>>,
) -> Result<dch_loop::Runner, dch_loop::RunnerError> {
    let observer = state.clone().into_observer().0;
    let mut builder =
        assemble_runner_builder(config, workdir, observer, state, lanes, shutting_down);
    if let Some(history) = history {
        builder = builder.with_history(history);
    }
    builder.build().await
}

/// Re-record on the runner the files the resumed conversation had
/// already read.
///
/// The read-guard half both resumes owe the write guard: a file the
/// session read before it was saved gets its baseline re-recorded on
/// the rebuilt runner, so an edit-resume may still write it. Every
/// path's recording failure is soft — a vanished file costs nothing
/// but its own baseline.
async fn record_resumed_reads(runner: &dch_loop::Runner, outcome: &crate::resume::ResumeOutcome) {
    for path in crate::resume::resumed_read_paths_with_machine(
        &outcome.messages,
        outcome.machine.as_deref(),
    ) {
        let _recorded = runner.record_resumed_read(&path).await;
    }
}

/// Hand the shared state the live runner's handles and return the
/// display's guidance share.
///
/// The adoption both resumes perform, once per runner so it is never
/// half-done: the context-window cell takes the new runner's window,
/// the cancel-signal cell its stop wire, and the guidance cell the
/// panel's own steer handle — stored together, so the display's
/// Esc,Esc and Ctrl+S always reach the CURRENT runner. The guidance
/// comes back for the launch's own app wiring; a mid-session resume
/// lets the stored share stand.
fn adopt_live_runner(
    state: &TuiObserverState,
    runner: &dch_loop::Runner,
) -> Option<Arc<dyn dch_tui::Guidance>> {
    state
        .context_window
        .store(runner.context_window(), Ordering::SeqCst);
    let guidance: Option<Arc<dyn dch_tui::Guidance>> = Some(Arc::new(LoopGuidance {
        steering: runner.steering(),
    }));
    state.adopt_runner_handles(runner.cancel_signal(), guidance.clone());
    guidance
}

/// Await the driver with a bound, then tear it down.
///
/// The exit is prompt even when the run will not die politely: a
/// wedged tool or a stubborn stream gets a bounded wait, then the
/// task is torn down — the session's halves were persisted by the
/// settle above, so nothing more can be saved by waiting. Whatever the
/// run had already settled for the exit lines stands: the bound is on
/// the join, not on what the run reported before it.
///
/// The abort is reaped under its own short bound: a poll already in
/// flight on another worker finishes the synchronous tail it is in —
/// the exit-owned cancel's publication into the error lane, which the
/// late drain that follows strips by name — before the session reads
/// the lanes, so no publication can outlive the strip and land as a
/// twin record. A driver wedged inside a synchronous section still
/// costs only the reap's bound, never the wait the first bound refused.
pub(crate) async fn tear_down_driver_bounded(driver: tokio::task::JoinHandle<()>) {
    let mut driver = driver;
    if tokio::time::timeout(std::time::Duration::from_millis(500), &mut driver)
        .await
        .is_err()
    {
        driver.abort();
        if tokio::time::timeout(std::time::Duration::from_millis(100), &mut driver)
            .await
            .is_err()
        {
            tracing::debug!("the driver stayed wedged through the abort's reap");
        }
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

/// Load the session a `/resume` asks for, or say why it cannot.
///
/// The reading half of the mid-session resume: an id that is not one,
/// a file that will not load — each posts its notice and comes back
/// `None`, leaving the loop's decision to the caller. A session with
/// a newer queued snapshot than the file's own adopts it first, so
/// the pair the resume works from is the freshest the save lane ever
/// settled, never the file alone.
fn load_resumable_session(
    id: &str,
    transcript: &TranscriptHandle,
    sessions_root: &Path,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
) -> Option<(uuid::Uuid, crate::resume::ResumeOutcome)> {
    let Ok(target) = uuid::Uuid::parse_str(id) else {
        post_session_notice(
            session_tx,
            format!("{id} is not a session id — nothing was resumed"),
        );
        return None;
    };
    let queued = transcript.latest_queued_for(target);
    let machine_floor = queued.as_ref().map(|waiting| waiting.saved_at);
    let Ok(mut outcome) =
        crate::resume::load_for_resume_in_with_floor(target, sessions_root, machine_floor)
    else {
        post_session_notice(
            session_tx,
            format!("session {id} could not be loaded — the loop kept its conversation"),
        );
        return None;
    };
    if let Some(queued) = queued {
        adopt_queued_snapshots(&mut outcome, queued);
    }
    Some((target, outcome))
}

/// The config a resumed run executes on, from the file's own record.
///
/// The model half of a resume's shaping, worded the way the launch
/// shapes its own: the session continues on the model it ended on —
/// the envelope's record, selected through the base config so a
/// profile's own entry is honored outright — unless the run's
/// `--model` flag named one, and the run's reasoning-effort flag
/// reapplies over the resumed file, exactly as a fresh resolution
/// would.
fn resumed_run_config(
    context: &DriverContext,
    outcome: &crate::resume::ResumeOutcome,
) -> dch_config::DchConfig {
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
    config
}

/// Adopt the rebuilt runner for the resumed session, or refuse.
///
/// The adoption decision of a mid-session resume, in one gate: the
/// shared build mounts the resumed machine history and awaits the new
/// runner; a success attaches the live lanes to it before it takes
/// the loop's place, and a failure says so beside the engine's own
/// reason and leaves the loop exactly as it stood — the caller keeps
/// its conversation and reports the refusal.
async fn adopt_resumed_runner(
    context: &DriverContext,
    config: &dch_config::DchConfig,
    machine: &[loopctl::message::Message],
    id: &str,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
) -> Option<dch_loop::Runner> {
    let built = build_session_runner(
        config,
        &context.workdir,
        &context.state,
        &context.lanes,
        &context.shutting_down,
        Some(machine.to_vec()),
    )
    .await;
    match built {
        Ok(built) => {
            context.lanes.attach(&built);
            Some(built)
        }
        Err(err) => {
            post_session_notice(
                session_tx,
                format!(
                    "session {id} could not be adopted — the loop kept its conversation ({err})"
                ),
            );
            None
        }
    }
}

/// Enforce a resumed session's remembered permissions, or close the
/// lane.
///
/// The enforcement half of a mid-session resume: the saved rules are
/// adopted into the live lane; when their file was damaged, the mode
/// the engine will actually run under replaces the resumed standing
/// in the session's config, and the repair notice — worded once here
/// — says what stands until `/permissions` repair.
fn enforce_resumed_permissions(
    transcript: &TranscriptHandle,
    context: &mut DriverContext,
    standing_mode: dch_config::PermissionMode,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
) -> dch_config::PermissionMode {
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
    enforced_mode
}

/// The display's report of a resume that happened.
///
/// The update that re-seeds the conversation after the swap: the
/// loaded transcript whole, the continued token figures, and the mode
/// the session now runs under — under the id the save target moved
/// onto, so a later `/resume` names exactly this session.
fn resumed_session_update(
    target: uuid::Uuid,
    outcome: crate::resume::ResumeOutcome,
    permission_mode: dch_config::PermissionMode,
) -> SessionUpdate {
    SessionUpdate::Resumed {
        id: target.to_string(),
        transcript: outcome.messages,
        tokens: ResumedTokens {
            cumulative_input: outcome.tokens.cumulative_input,
            cumulative_output: outcome.tokens.cumulative_output,
            last_input: outcome.tokens.last_input_tokens,
            context_tokens: outcome.tokens.context_tokens,
        },
        permission_mode,
    }
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
    let Some((target, outcome)) =
        load_resumable_session(&id, transcript, &context.sessions_root, session_tx)
    else {
        return false;
    };
    let machine =
        crate::resume::history_for_resume(&outcome, context.config.security.redact_secrets);
    let mut config = resumed_run_config(context, &outcome);
    let (standing_mode, recorded_mode) = resumed_modes(context, &outcome);
    config.runner.permission_mode = standing_mode;
    let Some(built) = adopt_resumed_runner(context, &config, &machine, &id, session_tx).await
    else {
        return false;
    };
    record_resumed_reads(&built, &outcome).await;
    *runner = built;
    adopt_live_runner(&context.state, runner);
    context.config = config;
    *context
        .lanes
        .mode_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = standing_mode;
    seed_token_counters(&context.state, outcome.tokens);
    let adopted = filtered_machine_history(&Arc::new(machine));
    store_machine_history(&context.state, &adopted);
    transcript.retarget(
        target,
        &context.config.api.model,
        outcome.title.clone(),
        recorded_mode,
    );
    let enforced_mode = enforce_resumed_permissions(transcript, context, standing_mode, session_tx);
    transcript.send_machine(machine_snapshot(
        adopted,
        recorded_compactions(&outcome.messages),
    ));
    let update = resumed_session_update(target, outcome, enforced_mode);
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

    #[test]
    fn the_machine_snapshot_stamps_now_and_carries_the_pair() {
        // The save's machine half travels with the count it was read
        // at and a stamp taken at publication — the pair rule the next
        // resume compares on. The saver stays off the snapshot: the
        // snapshot publishes, it does not retarget the writer.
        let messages = std::sync::Arc::new(Vec::new());
        let before = chrono::Utc::now();
        let snapshot = machine_snapshot(std::sync::Arc::clone(&messages), 7);
        let after = chrono::Utc::now();
        assert_eq!(
            snapshot.compactions, 7,
            "the snapshot carries the count it was read at"
        );
        assert!(
            snapshot.saved_at >= before && snapshot.saved_at <= after,
            "the stamp is taken at publication, between the two reads of the clock"
        );
        assert!(
            snapshot.saver.is_none(),
            "the snapshot publishes; it never retargets the writer"
        );
        assert!(
            std::sync::Arc::ptr_eq(&snapshot.messages, &messages),
            "the history rides the caller's own allocation, not a copy"
        );
    }

    #[test]
    fn the_store_publishes_the_adopted_history_verbatim() {
        // Whatever the caller adopted is exactly what the shared cell
        // hands the display and the driver — the store filters nothing
        // on the way in.
        let (_observer, state) = TuiObserverState::new().into_observer();
        let adopted = std::sync::Arc::new(Vec::new());
        store_machine_history(&state, &adopted);
        let cell = state
            .machine_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("the publish lands");
        assert!(
            std::sync::Arc::ptr_eq(&cell, &adopted),
            "the shared cell hands back exactly the history adopted — no copy, no filter"
        );
    }

    #[test]
    fn the_token_counters_continue_from_the_file_s_figures() {
        // Both resume paths drive this one set: the cumulative totals,
        // the last turn's input, and the context figure — so the bar
        // and /cost read the resumed conversation's numbers from the
        // first frame.
        let (_observer, state) = TuiObserverState::new().into_observer();
        seed_token_counters(
            &state,
            crate::session::SessionTokens {
                cumulative_input: 11,
                cumulative_output: 7,
                last_input_tokens: 5,
                context_tokens: 9,
            },
        );
        let tokens = state
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            tokens.cumulative_input, 11,
            "the cumulative input continues from the file's figure"
        );
        assert_eq!(
            tokens.cumulative_output, 7,
            "the cumulative output continues from the file's figure"
        );
        assert_eq!(
            tokens.input, 5,
            "the last turn's input reads the file's figure"
        );
        assert_eq!(
            tokens.context_tokens, 9,
            "the context figure reads the file's figure"
        );
    }

    #[test]
    fn only_a_resumed_control_carries_a_launch_history() {
        // A fresh start mounts no history by definition; a resumed
        // control's own machine record is what the shared build rides.
        assert!(
            resume_history_for_launch(
                &ResumeControl::Fresh {
                    session_id: None,
                    warn: None,
                },
                false,
            )
            .is_none(),
            "a fresh start mounts no history"
        );
        let outcome = crate::resume::ResumeOutcome {
            session_id: uuid::Uuid::new_v4(),
            model: "model-a".into(),
            messages: Vec::new(),
            tokens: crate::session::SessionTokens::default(),
            machine: Some(Vec::new()),
            machine_compactions: 0,
            title: None,
            permission_mode: None,
        };
        let history = resume_history_for_launch(&ResumeControl::Resumed(outcome), false);
        assert!(
            history.is_some_and(|history| history.is_empty()),
            "a resumed control mounts its own machine record, whatever it holds"
        );
    }

    #[test]
    fn the_resumed_update_carries_the_file_whole() {
        // The display re-seeds from this one update: the id the target
        // moved onto, the transcript, the continued figures, and the
        // standing mode — all mapped as the file reads them.
        let target = uuid::Uuid::new_v4();
        let outcome = crate::resume::ResumeOutcome {
            session_id: target,
            model: "model-a".into(),
            messages: Vec::new(),
            tokens: crate::session::SessionTokens {
                cumulative_input: 3,
                cumulative_output: 4,
                last_input_tokens: 5,
                context_tokens: 6,
            },
            machine: None,
            machine_compactions: 0,
            title: None,
            permission_mode: None,
        };
        let update = resumed_session_update(target, outcome, dch_config::PermissionMode::default());
        match update {
            SessionUpdate::Resumed {
                id,
                transcript,
                tokens,
                permission_mode,
            } => {
                assert_eq!(
                    id,
                    target.to_string(),
                    "the update names the target it moved onto"
                );
                assert_eq!(
                    transcript,
                    Vec::new(),
                    "the transcript maps as the file reads it"
                );
                assert_eq!(
                    tokens.cumulative_input, 3,
                    "the cumulative input rides the file"
                );
                assert_eq!(
                    tokens.cumulative_output, 4,
                    "the cumulative output rides the file"
                );
                assert_eq!(tokens.last_input, 5, "the last turn's input rides the file");
                assert_eq!(
                    tokens.context_tokens, 6,
                    "the context figure rides the file"
                );
                assert_eq!(
                    permission_mode,
                    dch_config::PermissionMode::default(),
                    "the standing mode rides the file"
                );
            }
            _ => panic!("a resume posts exactly one update"),
        }
    }
}
