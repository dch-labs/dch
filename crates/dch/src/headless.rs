//! Headless runner: load config, build the agent, run one session, exit.
//!
//! This is the dispatch target for `dch --headless`. It owns no agent-loop
//! logic — everything turn/stream/dispatch lives in loopctl via the runner —
//! and prints nothing of its own: the model's answer reaches stdout through
//! the [`ConsoleObserver`] stream. This module resolves the prompt, maps the
//! outcome to the process exit code, and writes the `--done-file` marker.

use std::io::{IsTerminal, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dch_config::Verbosity;
use dch_loop::ConsoleObserver;
use loopctl::engine::Run;
use loopctl::error::LoopError;

use crate::args::Args;
use crate::done::{DoneStatus, write_done_file};

/// The done-file message the force path writes, distinguishing a repeated
/// interrupt from the cooperative cancellation a single one produces.
const FORCE_CANCEL_MESSAGE: &str = "cancelled by a repeated interrupt";

/// The done-file message the startup path writes, distinguishing an
/// interrupt that landed before the runner existed from the run-phase
/// cancellations above.
const STARTUP_CANCEL_MESSAGE: &str = "cancelled during startup";

/// The outcome of a headless run: the process exit code and any status
/// message for the done-file.
///
/// Both variants carry the same fields so the done-file writer can treat
/// them uniformly: counts appear only when a run completed — the engine's
/// error path carries no partial-run totals, so construction and run-level
/// failures alike report none.
struct HeadlessOutcome {
    /// The process exit code for `main` to hand to the shell.
    ///
    /// 0 on success, 1 on a construction or usage failure, 2 on a run
    /// failure, and 130 on cancellation.
    exit_code: u8,

    /// Whether the run produced a final answer.
    ///
    /// Drives which `DoneStatus` builder renders the status, and separates
    /// exit 0 from exit 2.
    success: bool,

    /// The model's final output, or the error description on failure.
    ///
    /// Becomes the done-file's `message` verbatim on every terminal path,
    /// so it should read sensibly in a CI log.
    message: String,

    /// Turns completed during the run.
    ///
    /// `Some` only when a run completed — the engine's error values carry
    /// no partial-run totals, so failures (construction and run-level
    /// alike) report none. The done-file writer turns `None` into a
    /// countless failure.
    turns: Option<usize>,

    /// Tool calls made during the run.
    ///
    /// Same lifecycle as `turns`: `Some` when a run completed, `None` on
    /// every failure path.
    tools_used: Option<usize>,
}

impl HeadlessOutcome {
    /// Map a completed run to a success or no-answer outcome.
    ///
    /// `output: Some` means the model produced a final answer (success);
    /// `output: None` means the run ended without producing one. Either way
    /// the run's turn and tool-call totals are preserved for the done-file.
    fn from_run(run: &Run) -> Self {
        match &run.output {
            Some(output) => Self {
                exit_code: 0,
                success: true,
                message: output.clone(),
                turns: Some(run.turn_count()),
                tools_used: Some(run.tool_call_count()),
            },
            None => Self {
                exit_code: 2,
                success: false,
                message: "run completed without a final answer".into(),
                turns: Some(run.turn_count()),
                tools_used: Some(run.tool_call_count()),
            },
        }
    }

    /// Build a construction-phase failure (exit 1, no counts).
    ///
    /// Used when the run cannot start — config, provider, or prompt
    /// resolution fails before the agent is built.
    fn failure(exit_code: u8, message: impl Into<String>) -> Self {
        Self {
            exit_code,
            success: false,
            message: message.into(),
            turns: None,
            tools_used: None,
        }
    }

    /// Map a `LoopError` to an outcome, honouring the cancel exit code.
    ///
    /// Cancelled runs exit 130; all other loop errors exit 2. The error
    /// value carries no partial-run totals, so the counts stay `None` —
    /// how far a failed run got is not recoverable from it.
    fn from_loop_error(error: &LoopError) -> Self {
        let code = if error.is_cancelled() { 130 } else { 2 };
        Self {
            exit_code: code,
            success: false,
            message: error.to_string(),
            turns: None,
            tools_used: None,
        }
    }
}

/// Run a single headless task and return the process exit code.
///
/// Loads config, builds a non-interactive runner with a `ConsoleObserver`,
/// resolves the prompt (from `--headless` or stdin), runs one full session,
/// writes the `--done-file` (if requested), and returns the exit code. The
/// caller (`main`) turns the code into the process exit status. The
/// startup bridge armed before this call is stopped once the run bridge is
/// installed; the two briefly overlap, where an interrupt fails closed
/// through the construction hook. A construction-phase failure stops the
/// startup bridge directly, after its marker is written.
///
/// # Errors
///
/// Returns the exit code for any failure: 1 for construction-phase errors,
/// 2 for run-level failures, 130 for cancellation.
pub async fn run_headless(args: &Args, startup_bridge: crate::signals::CancelBridge) -> u8 {
    let inherited_mode = capture_marker_mode(args.done_file.as_ref());
    let code = match run_headless_inner(args, startup_bridge, inherited_mode.clone()).await {
        Ok(outcome) | Err(outcome) => outcome.exit_code,
    };
    reapply_mode(inherited_mode.as_ref(), args.done_file.as_ref());
    code
}

/// An existing marker's permissions, captured before the run clears it.
///
/// The clear deletes the file the writer's mode preservation would
/// otherwise read, so the run carries the mode itself and reapplies it
/// after its terminal write — a restrictive marker stays restrictive
/// across runs.
pub(crate) fn capture_marker_mode(
    done_file: Option<&std::path::PathBuf>,
) -> Option<std::fs::Permissions> {
    done_file
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.permissions())
}

/// Best-effort mode restoration; a failed chmod leaves the platform
/// default in place.
fn reapply_mode(mode: Option<&std::fs::Permissions>, done_file: Option<&std::path::PathBuf>) {
    if let (Some(mode), Some(path)) = (mode, done_file) {
        drop(std::fs::set_permissions(path, mode.clone()));
    }
}

/// Run the headless pipeline and produce a structured outcome.
///
/// Returns `Ok(HeadlessOutcome)` on any terminal outcome (success or
/// run-level failure); `Err(HeadlessOutcome)` on construction-phase failure
/// (config, agent, or prompt resolution). Both variants carry the
/// done-file status info. Finalization — the done-file write — runs
/// before the run bridge is stopped, so an interrupt landing during the
/// write fails closed through the force hook rather than killing the
/// process with no marker. Once the run's own marker is written the
/// force hook stands down: a repeat interrupt still exits 130, but the
/// recorded outcome survives in the done-file.
///
/// # Errors
///
/// Returns the construction-phase failure outcome when the config, agent,
/// or prompt cannot be resolved.
async fn run_headless_inner(
    args: &Args,
    startup_bridge: crate::signals::CancelBridge,
    inherited_mode: Option<std::fs::Permissions>,
) -> Result<HeadlessOutcome, HeadlessOutcome> {
    if let Some(path) = &args.done_file {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                let outcome = construction_failure(
                    args,
                    format!(
                        "cannot clear the stale done-file at {}: {err}",
                        path.display()
                    ),
                );
                startup_bridge.stop().await;
                return Err(outcome);
            }
        }
    }
    let built = match construct_run(args).await {
        Ok(built) => built,
        Err(outcome) => {
            startup_bridge.stop().await;
            return Err(outcome);
        }
    };
    let ConstructedRun { prompt, mut runner } = built;

    let force_args = args.clone();
    let hook_mode = inherited_mode;
    let outcome_recorded = Arc::new(AtomicBool::new(false));
    let hook_recorded = Arc::clone(&outcome_recorded);
    let bridge = crate::signals::install_cancel_handler(runner.cancel_signal(), move || {
        write_force_marker_unless_recorded(&force_args, &hook_recorded);
        reapply_mode(hook_mode.as_ref(), force_args.done_file.as_ref());
    });
    startup_bridge.stop().await;

    let run = match runner.run(&prompt).await {
        Ok(run) => run,
        Err(err) => {
            let outcome = HeadlessOutcome::from_loop_error(&err);
            write_done_file_if_requested(args, &outcome);
            outcome_recorded.store(true, Ordering::SeqCst);
            bridge.stop().await;
            return Err(outcome);
        }
    };

    let outcome = HeadlessOutcome::from_run(&run);
    write_done_file_if_requested(args, &outcome);
    outcome_recorded.store(true, Ordering::SeqCst);
    bridge.stop().await;
    Ok(outcome)
}

/// The force hook's marker write, stood down once a run outcome exists.
///
/// The bridge stays armed through the run's own finalization so an
/// interrupt during the write fails closed; after that write completes,
/// the recorded outcome is authoritative — a repeat interrupt still
/// exits 130 through the force path, but no longer replaces the
/// done-file's real outcome with the fixed cancel message.
fn write_force_marker_unless_recorded(args: &Args, recorded: &AtomicBool) {
    if recorded.load(Ordering::SeqCst) {
        return;
    }
    write_done_file_if_requested(args, &HeadlessOutcome::failure(130, FORCE_CANCEL_MESSAGE));
}

/// The product of a successful construction phase.
///
/// Bundles everything the run needs so the phase reads as one fallible
/// unit the caller guards.
struct ConstructedRun {
    /// The resolved task text for the run.
    ///
    /// Fixed before any config or agent work begins, and handed to the
    /// runner unchanged once construction has succeeded.
    prompt: String,

    /// The built agent.
    ///
    /// Fully configured from the loaded config and the process working
    /// directory, observer attached, with its shared cancel signal ready
    /// for the run bridge to install.
    runner: dch_loop::Runner,
}

/// Resolve the prompt, load the config, and build the runner.
///
/// Every step before the run bridge exists; each failure maps through
/// [`construction_failure`], which writes the done-file marker before
/// the outcome leaves this function.
///
/// # Errors
///
/// Returns the construction-phase failure outcome when the prompt,
/// config, working directory, or agent construction fails.
async fn construct_run(args: &Args) -> Result<ConstructedRun, HeadlessOutcome> {
    let prompt = resolve_prompt_nonblocking(args)
        .await
        .map_err(|message| construction_failure(args, message))?;

    let mut config = load_config(args.config.config_path.as_deref())
        .map_err(|message| construction_failure(args, message))?;
    apply_cli_overrides(&mut config, args);

    let verbosity = resolve_verbosity(&config, args);
    let observer = Arc::new(ConsoleObserver::new(
        verbosity,
        ConsoleObserver::detect_color(),
    ));

    let workdir = std::env::current_dir()
        .map_err(|err| construction_failure(args, format!("cannot determine cwd: {err}")))?;

    let runner = dch_loop::Runner::builder(&config, &workdir)
        .with_observer(Arc::clone(&observer) as Arc<dyn loopctl::observer::LoopObserver>)
        .build()
        .await
        .map_err(|err| construction_failure(args, format!("agent construction: {err}")))?;
    Ok(ConstructedRun { prompt, runner })
}

/// Write the done-file for an interrupt that lands before the runner
/// exists — the startup bridge's hook.
///
/// Public to `main` so the handler can be armed the moment the runtime is
/// up, ahead of prompt resolution and agent construction. The mode
/// captured before the bridge was armed is restored after the write,
/// mirroring the run-phase force hook: the stale marker this hook's write
/// replaces was cleared while the hook waited.
pub(crate) fn write_startup_done_file(args: &Args, inherited_mode: Option<&std::fs::Permissions>) {
    write_done_file_if_requested(args, &HeadlessOutcome::failure(130, STARTUP_CANCEL_MESSAGE));
    reapply_mode(inherited_mode, args.done_file.as_ref());
}

/// Build a construction-phase failure outcome and write the done-file.
///
/// Every terminal path writes the marker when `--done-file` is supplied —
/// including these before-the-run failures — so an orchestrator polling for
/// the file's existence never hangs.
fn construction_failure(args: &Args, message: impl Into<String>) -> HeadlessOutcome {
    let outcome = HeadlessOutcome::failure(1, message);
    write_done_file_if_requested(args, &outcome);
    outcome
}

/// Resolve the prompt from the parsed arguments without blocking the
/// runtime thread.
///
/// The fast paths — explicit text, terminal-stdin usage errors — resolve
/// synchronously; a piped-stdin read, potentially unbounded, runs on the
/// blocking pool so the startup interrupt bridge stays live while it
/// waits. See [`resolve_prompt_with`] for the full rules.
///
/// # Errors
///
/// Returns an error message when no usable prompt exists.
async fn resolve_prompt_nonblocking(args: &Args) -> Result<String, String> {
    if has_explicit_prompt(args) || std::io::stdin().is_terminal() {
        return resolve_prompt(args);
    }
    let read = tokio::task::spawn_blocking(read_stdin_prompt);
    let piped = read
        .await
        .unwrap_or_else(|err| Err(format!("stdin read task failed: {err}")));
    resolve_prompt_with(args.headless.as_deref(), move || piped, || false)
}

/// Whether the `--headless` flag carries a usable task text.
///
/// Shared by the fast paths so the definition of "explicit prompt" cannot
/// drift between them.
fn has_explicit_prompt(args: &Args) -> bool {
    args.headless.as_deref().is_some_and(presentable)
}

/// Whether a prompt candidate can be used verbatim.
///
/// Leading and trailing whitespace is not content: a flag or stream that
/// trims to nothing falls through to the next source instead.
fn presentable(text: &str) -> bool {
    !text.trim().is_empty()
}

/// Resolve the prompt from the parsed arguments.
///
/// A non-empty `--headless "<text>"` wins and stdin is never touched; an
/// empty or whitespace-only value falls through to stdin, as does an absent
/// flag. See [`resolve_prompt_with`] for the full rules.
///
/// # Errors
///
/// Returns an error message when no usable prompt exists.
fn resolve_prompt(args: &Args) -> Result<String, String> {
    resolve_prompt_with(args.headless.as_deref(), read_stdin_prompt, || {
        std::io::stdin().is_terminal()
    })
}

/// The prompt-resolution rules, with the stdin side injected.
///
/// Precedence: non-empty `--headless` text is used verbatim and the stdin
/// closures are never called; otherwise stdin is probed and, when it is not
/// a terminal, read to end as one prompt with trailing line breaks trimmed.
/// A terminal stdin can never deliver a task, so that is a usage error, as
/// is an empty stream.
///
/// # Errors
///
/// Returns an error message when no usable prompt exists.
fn resolve_prompt_with(
    explicit: Option<&str>,
    read_stdin: impl FnOnce() -> Result<String, String>,
    stdin_is_terminal: impl FnOnce() -> bool,
) -> Result<String, String> {
    if let Some(text) = explicit.filter(|text| presentable(text)) {
        return Ok(text.to_string());
    }
    if stdin_is_terminal() {
        return Err(if explicit.is_some() {
            "stdin is a terminal and no task was given: use `--headless \"…\"` \
             or pipe a task on stdin"
                .into()
        } else {
            "no prompt: use `--headless \"…\"` or pipe a task on stdin".into()
        });
    }
    let raw = read_stdin()?;
    let prompt = raw.trim_end_matches(['\n', '\r']);
    if prompt.is_empty() {
        Err("empty prompt on stdin".into())
    } else {
        Ok(prompt.to_string())
    }
}

/// Read all of stdin as the raw prompt text.
///
/// Trailing line breaks are trimmed by the caller so `echo "x"` delivers
/// `"x"` without the shell-added newline; internal whitespace is preserved.
///
/// # Errors
///
/// Returns an error message when the read fails.
fn read_stdin_prompt() -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("failed to read stdin: {e}"))?;
    Ok(buf)
}

/// Load the config from the `--config` file or the default search path.
///
/// An explicit path is read and parsed verbatim, relative paths resolving
/// against the process's current directory; with no path, the default
/// config lookup applies.
///
/// # Errors
///
/// Returns an error message when the file cannot be read or parsed.
fn load_config(path: Option<&Path>) -> Result<dch_config::DchConfig, String> {
    match path {
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("config read error ({}): {e}", path.display()))?;
            toml::from_str(&content)
                .map_err(|e| format!("config parse error ({}): {e}", path.display()))
        }
        None => dch_config::DchConfig::load().map_err(|e| format!("config error: {e}")),
    }
}

/// Apply CLI overrides to the config before agent construction.
///
/// Deliberately small: `--model` is the only per-run provider override and
/// `--unsafe-paths` the only per-run access switch. Verbosity resolves
/// separately when the observer is built, and display preferences remain
/// config-file concerns.
fn apply_cli_overrides(config: &mut dch_config::DchConfig, args: &Args) {
    if let Some(model) = &args.model {
        config.api.model.clone_from(model);
    }
    if args.config.unsafe_paths {
        config.runner.unsafe_paths = true;
    }
}

/// Resolve the effective verbosity from config and CLI flags.
///
/// `--quiet` wins when both flags are passed; with neither, the configured
/// level stands.
fn resolve_verbosity(config: &dch_config::DchConfig, args: &Args) -> Verbosity {
    if args.quiet {
        Verbosity::Quiet
    } else if args.verbose {
        Verbosity::Verbose
    } else {
        config.display.verbosity
    }
}

/// Write the done-file when `--done-file` is supplied.
///
/// Called on every terminal path. Successful runs and answerless runs carry
/// their turn/tool counts; other failures carry the message alone. Write
/// failures are logged to stderr but do not change the exit code.
fn write_done_file_if_requested(args: &Args, outcome: &HeadlessOutcome) {
    let Some(path) = &args.done_file else {
        return;
    };
    let status = if outcome.success {
        DoneStatus::success(
            outcome.message.clone(),
            outcome.turns.unwrap_or(0),
            outcome.tools_used.unwrap_or(0),
        )
    } else if let (Some(turns), Some(tools_used)) = (outcome.turns, outcome.tools_used) {
        DoneStatus::failure_with_counts(outcome.message.clone(), turns, tools_used)
    } else {
        DoneStatus::failure(outcome.message.clone())
    };
    if let Err(err) = write_done_file(path, &status) {
        eprintln!(
            "warning: failed to write done-file {}: {err}",
            path.display()
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;
    use crate::signals::SIGNAL_TEST_LOCK;
    use clap::Parser as _;
    use loopctl::engine::RunConfig;
    use tokio::io::AsyncWriteExt as _;

    /// Parse a flag list into [`Args`], prefixing the program name.
    ///
    /// Clap requires the program name as the first argument, so tests pass
    /// flag lists exactly as `main` would receive them after the binary
    /// name.
    fn parse(args: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("dch").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn explicit_prompt_wins_and_stdin_is_never_touched() {
        let prompt = resolve_prompt_with(
            Some("do the thing"),
            || panic!("stdin must not be read when text is given"),
            || panic!("stdin must not be probed when text is given"),
        );
        assert_eq!(prompt.unwrap(), "do the thing");
    }

    #[test]
    fn empty_flag_value_falls_through_to_piped_stdin() {
        let prompt = resolve_prompt_with(Some(""), || Ok("hello world\n".into()), || false);
        assert_eq!(prompt.unwrap(), "hello world");
    }

    #[test]
    fn whitespace_only_flag_value_falls_through_to_stdin() {
        let prompt = resolve_prompt_with(Some("   "), || Ok("piped".into()), || false);
        assert_eq!(prompt.unwrap(), "piped");
    }

    #[test]
    fn only_trailing_line_breaks_are_trimmed() {
        let prompt = resolve_prompt_with(Some(""), || Ok("a\n\nb\n".into()), || false);
        assert_eq!(prompt.unwrap(), "a\n\nb");
    }

    #[test]
    fn terminal_stdin_with_a_valueless_flag_is_a_usage_error() {
        let err = resolve_prompt_with(Some(""), || panic!("a terminal is never read"), || true)
            .unwrap_err();
        assert!(err.contains("stdin is a terminal"), "{err}");
    }

    #[test]
    fn terminal_stdin_with_no_flag_names_the_flag() {
        let err =
            resolve_prompt_with(None, || panic!("a terminal is never read"), || true).unwrap_err();
        assert!(err.contains("--headless"), "{err}");
    }

    #[test]
    fn empty_piped_stdin_is_an_error() {
        let err = resolve_prompt_with(None, || Ok(String::new()), || false).unwrap_err();
        assert!(err.contains("empty prompt"), "{err}");
    }

    #[test]
    fn a_stdin_read_failure_surfaces() {
        let err = resolve_prompt_with(
            None,
            || Err("failed to read stdin: broken".into()),
            || false,
        )
        .unwrap_err();
        assert!(err.contains("broken"), "{err}");
    }

    #[test]
    fn a_run_with_output_maps_to_success() {
        let mut run = Run::new("task", &RunConfig::default());
        run.output = Some("the answer".into());
        let outcome = HeadlessOutcome::from_run(&run);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.success);
        assert_eq!(outcome.message, "the answer");
        assert_eq!(outcome.turns, Some(0));
        assert_eq!(outcome.tools_used, Some(0));
    }

    #[test]
    fn a_run_without_an_answer_maps_to_a_run_failure() {
        let run = Run::new("task", &RunConfig::default());
        let outcome = HeadlessOutcome::from_run(&run);
        assert_eq!(outcome.exit_code, 2);
        assert!(!outcome.success);
        assert_eq!(outcome.turns, Some(0), "counts survive for the done-file");
        assert_eq!(outcome.tools_used, Some(0));
    }

    #[test]
    fn the_force_path_writes_its_distinguishing_done_file_message() {
        // The e2e accepts either terminal path; this pins the content that
        // distinguishes the force hook's write from a cooperative cancel.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        let args = parse(&["--headless", "x", "--done-file", path.to_str().unwrap()]);
        write_done_file_if_requested(&args, &HeadlessOutcome::failure(130, FORCE_CANCEL_MESSAGE));
        let written: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written.message.as_deref(), Some(FORCE_CANCEL_MESSAGE));
        assert_eq!(written.turns, None);
    }

    #[test]
    fn the_force_hook_stands_down_once_an_outcome_is_recorded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        let args = parse(&["--headless", "x", "--done-file", path.to_str().unwrap()]);

        let recorded = AtomicBool::new(false);
        write_force_marker_unless_recorded(&args, &recorded);
        let first: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(first.message.as_deref(), Some(FORCE_CANCEL_MESSAGE));

        write_done_file_if_requested(&args, &HeadlessOutcome::failure(2, "engine error text"));
        recorded.store(true, Ordering::SeqCst);
        write_force_marker_unless_recorded(&args, &recorded);
        let final_status: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            final_status.message.as_deref(),
            Some("engine error text"),
            "a recorded outcome is authoritative — the hook must not replace it"
        );
    }

    #[test]
    fn the_startup_path_writes_its_distinguishing_done_file_message() {
        // Same pin for the startup hook: the message is what separates an
        // interrupt before the runner existed from the run-phase paths.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        let args = parse(&["--headless", "x", "--done-file", path.to_str().unwrap()]);
        write_done_file_if_requested(
            &args,
            &HeadlessOutcome::failure(130, STARTUP_CANCEL_MESSAGE),
        );
        let written: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written.message.as_deref(), Some(STARTUP_CANCEL_MESSAGE));
        assert_eq!(written.turns, None);
    }

    #[test]
    fn the_cancel_split_uses_is_cancelled() {
        assert!(LoopError::Cancelled.is_cancelled());
        assert!(!LoopError::InvalidInput("bad".into()).is_cancelled());
        assert_eq!(
            HeadlessOutcome::from_loop_error(&LoopError::Cancelled).exit_code,
            130
        );
        assert_eq!(
            HeadlessOutcome::from_loop_error(&LoopError::InvalidInput("bad".into())).exit_code,
            2
        );
    }

    #[test]
    fn the_done_file_is_written_on_the_construction_failure_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        let args = parse(&["--headless", "x", "--done-file", path.to_str().unwrap()]);
        let outcome = construction_failure(&args, "config unreadable");
        assert_eq!(outcome.exit_code, 1);
        let written: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!written.success);
        assert_eq!(written.turns, None);
        assert_eq!(written.tools_used, None);
        assert_eq!(written.message.as_deref(), Some("config unreadable"));
    }

    #[test]
    fn the_done_file_carries_a_successful_run_s_counts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        let args = parse(&["--headless", "x", "--done-file", path.to_str().unwrap()]);
        let mut run = Run::new("task", &RunConfig::default());
        run.output = Some("done".into());
        write_done_file_if_requested(&args, &HeadlessOutcome::from_run(&run));
        let written: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(written.success);
        assert_eq!(written.turns, Some(0));
        assert_eq!(written.tools_used, Some(0));
    }

    #[test]
    fn the_done_file_keeps_counts_when_the_run_ended_without_an_answer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        let args = parse(&["--headless", "x", "--done-file", path.to_str().unwrap()]);
        let run = Run::new("task", &RunConfig::default());
        write_done_file_if_requested(&args, &HeadlessOutcome::from_run(&run));
        let written: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!written.success);
        assert_eq!(written.turns, Some(0));
        assert_eq!(written.tools_used, Some(0));
    }

    #[test]
    fn no_done_file_flag_writes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let args = parse(&["--headless", "x"]);
        write_done_file_if_requested(&args, &HeadlessOutcome::failure(1, "boom"));
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "no marker may appear without --done-file"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_unwritable_done_file_path_does_not_panic_or_mask_the_outcome() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("locked");
        std::fs::create_dir(&dir).unwrap();
        let lock = |mode: u32| {
            let mut perms = std::fs::metadata(&dir).unwrap().permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(&dir, perms).unwrap();
        };
        lock(0o500);
        if std::fs::write(dir.join("probe"), "x").is_ok() {
            lock(0o700);
            return; // mode bits are ignored here (e.g. root); nothing to pin
        }
        let path = dir.join("done.json");
        let args = parse(&["--headless", "x", "--done-file", path.to_str().unwrap()]);
        write_done_file_if_requested(
            &args,
            &HeadlessOutcome::from_loop_error(&LoopError::Cancelled),
        );
        lock(0o700);
        assert!(!path.exists(), "the failed write leaves no marker");
    }

    #[test]
    fn the_model_flag_overrides_the_configured_model() {
        let mut config = dch_config::DchConfig::default();
        apply_cli_overrides(&mut config, &parse(&["--model", "m2"]));
        assert_eq!(config.api.model, "m2");

        let mut config = dch_config::DchConfig::default();
        let original = config.api.model.clone();
        apply_cli_overrides(&mut config, &parse(&[]));
        assert_eq!(config.api.model, original);
    }

    #[test]
    fn the_unsafe_paths_flag_overrides_the_config() {
        // Containment stays on unless the flag is passed; the flag can only
        // lift it, never re-impose it over a config that opted out.
        let mut config = dch_config::DchConfig::default();
        apply_cli_overrides(&mut config, &parse(&["--unsafe-paths"]));
        assert!(config.runner.unsafe_paths);

        let mut config = dch_config::DchConfig::default();
        apply_cli_overrides(&mut config, &parse(&[]));
        assert!(!config.runner.unsafe_paths);
    }

    #[test]
    fn verbosity_resolves_quiet_over_verbose_over_config() {
        let config = dch_config::DchConfig::default();
        assert_eq!(
            resolve_verbosity(&config, &parse(&["-q"])),
            Verbosity::Quiet
        );
        assert_eq!(
            resolve_verbosity(&config, &parse(&["-v"])),
            Verbosity::Verbose
        );
        assert_eq!(
            resolve_verbosity(&config, &parse(&["-v", "-q"])),
            Verbosity::Quiet,
            "--quiet wins when both flags are passed"
        );
        assert_eq!(
            resolve_verbosity(&config, &parse(&[])),
            config.display.verbosity
        );
    }

    #[tokio::test]
    #[ignore = "needs a configured provider; run manually to prove the pipeline"]
    #[allow(clippy::await_holding_lock)]
    async fn a_live_headless_run_succeeds() {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let args = parse(&["--headless", "Reply with exactly: ok"]);
        assert_eq!(
            run_headless(&args, crate::signals::install_construction_handler(|| {}),).await,
            0
        );
    }

    /// A local server that answers the agent's request with one streamed
    /// text delta and then holds the connection open, so the run is still
    /// mid-stream when the test delivers its signal.
    ///
    /// Unix-only like the mid-run signal tests it serves: the armed
    /// signal is delivered with `libc::kill`, which does not exist on
    /// other targets.
    #[cfg(unix)]
    struct HoldServer {
        /// Whether the delta has been written and the run is mid-stream.
        ///
        /// Flips to true only after the delta bytes reach the wire, so a
        /// signal armed on this flag is guaranteed to land during the run
        /// rather than before the agent request is made.
        streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,

        /// The ephemeral port the agent's requests arrive on.
        ///
        /// Rendered into the temp config's `base_url`, so pointing the
        /// run at the server needs no name resolution or fixed port.
        port: u16,
    }

    #[cfg(unix)]
    impl HoldServer {
        /// Bind the server and spawn its connection task.
        ///
        /// The port is bound before returning so the temp config can name
        /// it; the task parks forever after answering, dying with the test
        /// process.
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("ephemeral bind");
            let port = listener.local_addr().expect("bound address").port();
            let streaming = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let task_streaming = std::sync::Arc::clone(&streaming);
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("agent connects");
                let delta = serde_json::json!({
                    "id": "c1", "model": "test-model",
                    "choices": [{"delta": {"content": "partial"}, "finish_reason": null}]
                });
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {delta}\n\n"
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("delta written");
                task_streaming.store(true, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<()>().await;
            });
            Self { streaming, port }
        }

        /// A config pointing the agent at this server.
        ///
        /// OpenAI-compatible against the bound port, with a placeholder
        /// key and a generous timeout so the only thing that can end the
        /// run early is the signal under test.
        fn config_toml(&self) -> String {
            format!(
                "[api]\napi_type = \"openai\"\nbase_url = \"http://127.0.0.1:{}\"\
                 \napi_key = \"dummy\"\nmodel = \"test-model\"\nrequest_timeout_secs = 10\n",
                self.port
            )
        }

        /// Arm a signal to fire once the run is mid-stream.
        ///
        /// The returned task resolves to whether the run reached mid-stream;
        /// the signal is delivered only then, and a run that ends first —
        /// or a readiness window that elapses — is never signaled. The
        /// caller awaits the handle before returning so no stray task
        /// outlives the test.
        fn signal_when_streaming(&self, signal: i32) -> tokio::task::JoinHandle<bool> {
            let streaming = std::sync::Arc::clone(&self.streaming);
            tokio::spawn(async move {
                for _ in 0..200 {
                    if streaming.load(std::sync::atomic::Ordering::SeqCst) {
                        unsafe { libc::kill(libc::getpid(), signal) };
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                false
            })
        }
    }

    /// Drive one full headless run that receives `signal` mid-stream.
    ///
    /// The harness owns the scaffolding shared by the mid-run signal
    /// tests — signal serialization, the held server, the temp config, the
    /// argument plumbing — leaving each test its signal and done-file
    /// path. Returns the process exit code, asserting along the way that
    /// the run reached mid-stream: a signal fired earlier would exercise
    /// the startup path instead of the mid-run one under test.
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    async fn run_signaled_mid_stream(signal: i32, done_file: Option<&std::path::Path>) -> u8 {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::signals::assert_listeners_install();
        let server = HoldServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, server.config_toml()).unwrap();
        let mut flags = vec![
            "--headless",
            "hello",
            "--config",
            config_path.to_str().unwrap(),
        ];
        if let Some(path) = done_file {
            flags.push("--done-file");
            flags.push(path.to_str().unwrap());
        }
        let args = parse(&flags);

        let armed = server.signal_when_streaming(signal);

        let code = run_headless(&args, crate::signals::install_construction_handler(|| {})).await;
        assert!(
            armed.await.unwrap(),
            "the run must reach mid-stream for the signal to fire"
        );
        code
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigint_mid_run_resolves_cancelled_with_exit_130_and_cooperative_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let done_path = tmp.path().join("done.json");
        let code = run_signaled_mid_stream(libc::SIGINT, Some(&done_path)).await;
        assert_eq!(code, 130, "SIGINT must cancel the run and map to exit 130");
        let written: DoneStatus =
            serde_json::from_str(&std::fs::read_to_string(&done_path).unwrap()).unwrap();
        assert!(!written.success, "a cancelled run is not a success");
        assert_eq!(
            written.message,
            Some(LoopError::Cancelled.to_string()),
            "the cooperative path writes the engine's cancel message, not the force hook's"
        );
        assert_eq!(
            written.turns, None,
            "a cancelled run reports no turn count for the marker"
        );
        assert_eq!(written.tools_used, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_mid_run_resolves_cancelled_with_exit_130_like_sigint() {
        let code = run_signaled_mid_stream(libc::SIGTERM, None).await;
        assert_eq!(
            code, 130,
            "SIGTERM must take the same cooperative path as SIGINT, not a default kill"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_stale_done_file_is_cleared_before_the_run_proceeds() {
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::signals::assert_listeners_install();
        let server = HoldServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, server.config_toml()).unwrap();
        let done_path = dir.path().join("done.json");
        std::fs::write(
            &done_path,
            serde_json::to_string(&DoneStatus::success("a previous run", 3, 7)).unwrap(),
        )
        .unwrap();
        let flags = [
            "--headless",
            "hello",
            "--config",
            config_path.to_str().unwrap(),
            "--done-file",
            done_path.to_str().unwrap(),
        ];
        let args = parse(&flags);
        let run_args = args.clone();
        let run = tokio::spawn(async move {
            run_headless(
                &run_args,
                crate::signals::install_construction_handler(|| {}),
            )
            .await
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while done_path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            !done_path.exists(),
            "a marker left by a previous run must not report that run's \
             outcome for this one"
        );
        run.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn an_unclearable_stale_done_file_fails_the_run() {
        use std::os::unix::fs::PermissionsExt as _;
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::signals::assert_listeners_install();
        if unsafe { libc::getuid() } == 0 {
            // Root ignores directory permissions, so the removal this
            // test forces would succeed and the assert would misfire.
            return;
        }
        let server = HoldServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, server.config_toml()).unwrap();
        let done_path = dir.path().join("done.json");
        std::fs::write(
            &done_path,
            serde_json::to_string(&DoneStatus::success("a previous run", 3, 7)).unwrap(),
        )
        .unwrap();
        let mut locked = std::fs::metadata(dir.path()).unwrap().permissions();
        locked.set_mode(0o555);
        std::fs::set_permissions(dir.path(), locked).unwrap();
        let flags = [
            "--headless",
            "hello",
            "--config",
            config_path.to_str().unwrap(),
            "--done-file",
            done_path.to_str().unwrap(),
        ];
        let code = run_headless(
            &parse(&flags),
            crate::signals::install_construction_handler(|| {}),
        )
        .await;
        let mut writable = std::fs::metadata(dir.path()).unwrap().permissions();
        writable.set_mode(0o755);
        std::fs::set_permissions(dir.path(), writable).ok();
        assert_eq!(
            code, 1,
            "a stale marker this run cannot clear must fail the run, not \
             proceed under the previous run's outcome"
        );
        assert!(
            done_path.exists(),
            "the unreadable-marker failure path leaves the stale file — it \
             could not be replaced"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_restrictive_marker_mode_survives_the_stale_clear() {
        use std::os::unix::fs::PermissionsExt as _;
        let _signal_lock = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "not a valid [[toml document").unwrap();
        let done_path = dir.path().join("done.json");
        std::fs::write(&done_path, "stale").unwrap();
        std::fs::set_permissions(&done_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let flags = [
            "--headless",
            "hello",
            "--config",
            config_path.to_str().unwrap(),
            "--done-file",
            done_path.to_str().unwrap(),
        ];
        let code = run_headless(
            &parse(&flags),
            crate::signals::install_construction_handler(|| {}),
        )
        .await;
        assert_eq!(code, 1, "the malformed config must fail construction");
        let mode = std::fs::metadata(&done_path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the restrictive mode of a cleared marker must survive the \
             run that replaced it"
        );
    }
}
