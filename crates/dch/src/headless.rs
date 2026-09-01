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

use dch_config::Verbosity;
use dch_loop::ConsoleObserver;
use loopctl::engine::Run;
use loopctl::error::LoopError;

use crate::args::Args;
use crate::done::{DoneStatus, write_done_file};

/// The outcome of a headless run: the process exit code and any status
/// message for the done-file.
///
/// Both variants carry the same fields so the done-file writer can treat
/// them uniformly: construction failures have no turn or tool counts, and
/// run-level failures carry whatever the runner produced before the error.
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
    /// `None` only on construction-phase failures, where no run existed to
    /// count; the done-file writer turns `None` into a countless failure.
    turns: Option<usize>,

    /// Tool calls made during the run.
    ///
    /// Same lifecycle as `turns`: always `Some` once a run started, `None`
    /// before one could.
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
    /// Cancelled runs exit 130; all other loop errors exit 2.
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
/// caller (`main`) turns the code into the process exit status.
///
/// # Errors
///
/// Returns the exit code for any failure: 1 for construction-phase errors,
/// 2 for run-level failures, 130 for cancellation.
pub async fn run_headless(args: &Args) -> u8 {
    match run_headless_inner(args).await {
        Ok(outcome) | Err(outcome) => outcome.exit_code,
    }
}

/// Run the headless pipeline and produce a structured outcome.
///
/// Returns `Ok(HeadlessOutcome)` on any terminal outcome (success or
/// run-level failure); `Err(HeadlessOutcome)` on construction-phase failure
/// (config, agent, or prompt resolution). Both variants carry the
/// done-file status info.
///
/// # Errors
///
/// Returns the construction-phase failure outcome when the config, agent,
/// or prompt cannot be resolved.
async fn run_headless_inner(args: &Args) -> Result<HeadlessOutcome, HeadlessOutcome> {
    let prompt = resolve_prompt(args).map_err(|message| construction_failure(args, message))?;

    let mut config = load_config(args.config_path.as_deref())
        .map_err(|message| construction_failure(args, message))?;
    apply_cli_overrides(&mut config, args);

    let verbosity = resolve_verbosity(&config, args);
    let observer = Arc::new(ConsoleObserver::new(
        verbosity,
        ConsoleObserver::detect_color(),
    ));

    let workdir = std::env::current_dir()
        .map_err(|err| construction_failure(args, format!("cannot determine cwd: {err}")))?;

    let mut runner = dch_loop::Runner::builder(&config, &workdir)
        .with_observer(Arc::clone(&observer) as Arc<dyn loopctl::observer::LoopObserver>)
        .build()
        .await
        .map_err(|err| construction_failure(args, format!("agent construction: {err}")))?;

    let run = runner.run(&prompt).await.map_err(|err| {
        let outcome = HeadlessOutcome::from_loop_error(&err);
        write_done_file_if_requested(args, &outcome);
        outcome
    })?;

    let outcome = HeadlessOutcome::from_run(&run);
    write_done_file_if_requested(args, &outcome);
    Ok(outcome)
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
    if let Some(text) = explicit.filter(|text| !text.trim().is_empty()) {
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
/// Deliberately small: `--model` is the only per-run provider override.
/// Verbosity resolves separately when the observer is built, and display
/// preferences remain config-file concerns.
fn apply_cli_overrides(config: &mut dch_config::DchConfig, args: &Args) {
    if let Some(model) = &args.model {
        config.api.model.clone_from(model);
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
    use clap::Parser as _;
    use loopctl::engine::RunConfig;

    /// Parse a flag list into [`Args`], prefixing the program name.
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
    async fn a_live_headless_run_succeeds() {
        let args = parse(&["--headless", "Reply with exactly: ok"]);
        assert_eq!(run_headless(&args).await, 0);
    }
}
