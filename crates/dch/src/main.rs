//! `dch` — a terminal-based agentic coding assistant built on `loopctl`.
//!
//! Run-mode dispatch: a task argument or a piped stdin routes to the
//! single-run headless mode (one task, non-interactive, exit code to the
//! shell); otherwise the interactive TUI session is hosted.
//! `--list-sessions` prints the session table and exits before any
//! mode; `--resume` resolves to a control both modes continue from.

mod args;
mod done;
mod headless;
mod messages;
mod resume;
mod session;
mod signals;
mod tui;

use std::io::IsTerminal as _;

/// Whether this invocation selects the single-run headless mode.
///
/// A task argument always does; with none, a stdin that is not a
/// terminal (a pipe or a redirect) does too — so `dch "task"` and
/// `echo "task" | dch` both run headlessly, and only a bare
/// invocation on a terminal launches the TUI.
fn wants_headless(args: &args::Args) -> bool {
    selects_single_run(args.task.as_deref(), std::io::stdin().is_terminal())
}

/// Whether the runtime-failure path may write the done-file marker.
///
/// The normal dispatch serves the listing verb before the single-run
/// mode and never writes a marker for it; this bootstrap-time mirror
/// of that ordering keeps `--done-file`'s "ignored outside headless
/// mode" promise true even when the runtime itself fails to
/// construct. A resumed run is served by the mode it resumes into —
/// a task argument or a piped stdin still selects headless, so the
/// marker rules follow the mode, not the resume flag.
pub(crate) fn bootstrap_marker_applies(args: &args::Args, stdin_is_terminal: bool) -> bool {
    args.list_sessions.is_none() && selects_single_run(args.task.as_deref(), stdin_is_terminal)
}

/// The mode decision over the task argument and the stdin kind.
///
/// A present task selects the single-run mode whatever stdin is; with
/// no task, any non-terminal stdin (a pipe or a redirect) selects it,
/// and only a terminal leaves the invocation to the TUI.
fn selects_single_run(task: Option<&str>, stdin_is_terminal: bool) -> bool {
    task.is_some() || !stdin_is_terminal
}

/// Where a run's traces are written.
///
/// The two channels a run can honestly report on, chosen by
/// [`log_sink`] from the mode the invocation runs: the answer is a
/// decision the installer obeys rather than a writer it picks apart,
/// so the rule stays testable without a terminal or a clock.
#[derive(Debug, PartialEq, Eq)]
enum LogSink {
    /// The process's own stderr, styled when it is a terminal.
    ///
    /// The ordinary channel: a headless run has no screen standing in
    /// the way, and a stderr the operator redirected is a pipe the
    /// drawing never touches.
    Stderr,

    /// A file the run appends to, beside the sessions it describes.
    ///
    /// The channel a TUI attached to a terminal has to use, because
    /// its own drawing shares stderr's descriptor. The path is the
    /// config directory's, made when the log opens.
    File(std::path::PathBuf),
}

/// The TUI's log file, written in the config directory.
///
/// A stable name an operator can point a pager at between runs, and
/// the one file `log_sink` offers when the terminal itself cannot
/// carry the traces. The config directory is where the same run's
/// `sessions/` live, so the log sits beside the records it explains.
const LOG_FILE_NAME: &str = "dch.log";

/// Choose where the traces go, from the mode this invocation runs.
///
/// The TUI owns the terminal: it draws on the alternate screen over
/// the same descriptor stderr shares, so a frame-lane line written
/// there lands inside the drawing and corrupts it. A TUI attached to
/// a terminal therefore sends its traces to `dch.log` in the config
/// directory, beside `sessions/`. Every other case keeps stderr,
/// which is the honest channel: a headless run has no screen to
/// corrupt, and a redirected stderr is a pipe the drawing never
/// touches.
fn log_sink(tui_mode: bool, stderr_is_terminal: bool, config_dir: &std::path::Path) -> LogSink {
    if tui_mode && stderr_is_terminal {
        LogSink::File(config_dir.join(LOG_FILE_NAME))
    } else {
        LogSink::Stderr
    }
}

/// Open the log file for appending, making its directory when needed.
///
/// # Errors
///
/// The OS error behind a directory that cannot be made or a file that
/// cannot be opened. It reaches the caller on purpose: an unwritable
/// log must cost the run its log file and nothing else, so the
/// installer keeps stderr rather than losing the observability the
/// operator asked for.
fn open_log_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Err(err);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Whether `RUST_LOG` asks for a log at all.
///
/// Unset and set-but-empty both mean silence: an exported empty
/// `RUST_LOG=` is the shell's way of saying "no logging", and opening
/// the log file for it would break the quiet contract every ordinary
/// run keeps.
fn log_requested(rust_log: Option<&std::ffi::OsStr>) -> bool {
    rust_log.is_some_and(|value| !value.is_empty())
}

/// The level a set-but-unreadable `RUST_LOG` falls back to.
const LOG_FALLBACK: &str = "info";

/// The filter `RUST_LOG` names, `info` standing in when it names none.
///
/// A directive that fails to parse — a mistyped target, a stray
/// bracket — costs the run the *fine-grained* selection, not the
/// logging the variable was set to ask for: the fallback keeps `info`
/// rather than answering a request for observability with silence.
fn log_filter(rust_log: Option<&std::ffi::OsStr>) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    let directive = rust_log
        .filter(|value| !value.is_empty())
        .and_then(|value| value.to_str())
        .unwrap_or(LOG_FALLBACK);
    EnvFilter::try_new(directive).unwrap_or_else(|_| EnvFilter::new(LOG_FALLBACK))
}

/// Install a tracing subscriber when `RUST_LOG` asks for one.
///
/// Silent by default — no flag, no subscriber, no output — so the
/// quiet contract holds for every ordinary run. With `RUST_LOG` set
/// (`dch=debug` for everything this binary emits,
/// `dch_loop::todo_nudge=debug` for the belt alone — its events are
/// debug-level, so an `info` ceiling leaves them unheard), the
/// invisible machinery becomes observable: every reminder the
/// contributor contributes, every follow-up the hook offers or
/// declines, named with the turn and the count. The belt's whole
/// "is it nudging?" question answers from the log instead of a wire
/// proxy. The mode decides the channel through [`log_sink`], and a
/// log file that cannot be opened falls back to stderr with a word on
/// it rather than to silence.
fn install_log_when_asked(tui_mode: bool) {
    let rust_log = std::env::var_os("RUST_LOG");
    if !log_requested(rust_log.as_deref()) {
        return;
    }
    let filter = log_filter(rust_log.as_deref());
    let stderr_is_terminal = std::io::stderr().is_terminal();
    let sink = log_sink(tui_mode, stderr_is_terminal, &dch_config::config_dir());
    let format = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(matches!(sink, LogSink::Stderr) && stderr_is_terminal);
    match sink {
        LogSink::Stderr => format.with_writer(std::io::stderr).init(),
        LogSink::File(path) => match open_log_file(&path) {
            Ok(file) => format
                .with_writer(move || -> Box<dyn std::io::Write> {
                    match file.try_clone() {
                        Ok(clone) => Box::new(clone),
                        Err(_) => Box::new(std::io::stderr()),
                    }
                })
                .init(),
            Err(err) => {
                let notice = log_open_failure_notice(&path, &err);
                crate::signals::report(&notice);
                format.with_writer(std::io::stderr).init();
            }
        },
    }
}

/// The word about a log file that could not be opened.
///
/// Names the path that failed, the reason, and where the traces went
/// instead — the three facts that decide whether to fix permissions or
/// ignore the run. Kept apart from the branch that prints it so the
/// wording has a shape a test can hold.
fn log_open_failure_notice(path: &std::path::Path, err: &std::io::Error) -> String {
    format!(
        "cannot log to {}: {err} — tracing to stderr instead",
        path.display()
    )
}

/// Whether this invocation hosts the interactive TUI.
///
/// The mirror of [`wants_headless`]: no task, a terminal stdin, and no
/// listing verb — a table print leaves as soon as it has printed, so
/// its traces keep the ordinary channel.
fn hosts_tui(args: &args::Args) -> bool {
    args.list_sessions.is_none() && !wants_headless(args)
}

fn main() -> std::process::ExitCode {
    let args = args::parse_args();
    install_log_when_asked(hosts_tui(&args));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            let message = format!("failed to start tokio runtime: {err}");
            if bootstrap_marker_applies(&args, std::io::stdin().is_terminal())
                && let Some(path) = &args.done_file
                && let Err(write_err) =
                    done::write_done_file(path, &done::DoneStatus::failure(message.clone()), None)
            {
                signals::report(&format!(
                    "dch: cannot write the done-file at {}: {write_err}",
                    path.display()
                ));
            }
            signals::report(&format!("dch: {message}"));
            std::process::exit(1);
        });
    runtime.block_on(async move {
        if let Some(limit) = args.list_sessions {
            resume::run_list_sessions(limit)
        } else {
            let control = resume::resolve_resume(&args);
            if wants_headless(&args) {
                let startup_args = args.clone();
                let startup_mode = headless::capture_marker_mode(args.done_file.as_ref());
                let startup_bridge = signals::install_construction_handler(move || {
                    headless::write_startup_done_file(&startup_args, startup_mode.as_ref());
                });
                let code = headless::run_headless(&args, startup_bridge, control).await;
                std::process::ExitCode::from(code)
            } else {
                std::process::ExitCode::from(tui::run_tui(&args, control).await)
            }
        }
    })
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

    #[test]
    fn a_task_argument_selects_the_single_run_mode_on_any_stdin() {
        assert!(selects_single_run(Some("t"), true));
        assert!(selects_single_run(Some("t"), false));
    }

    #[test]
    fn a_piped_stdin_selects_the_single_run_mode_without_a_task() {
        assert!(selects_single_run(None, false));
    }

    #[test]
    fn only_a_terminal_stdin_without_a_task_launches_the_tui() {
        assert!(!selects_single_run(None, true));
    }

    #[test]
    fn the_tui_writes_its_log_beside_the_sessions() {
        // The TUI draws on the alternate screen over the very
        // descriptor stderr shares, so its traces go to the config
        // directory — the one `sessions/` lives in — rather than into
        // the drawing they would corrupt.
        let config = std::path::Path::new("/home/operator/.dch");
        let sink = log_sink(true, true, config);
        assert!(
            matches!(sink, LogSink::File(_)),
            "a TUI on a terminal must not write into its own frame"
        );
        if let LogSink::File(path) = sink {
            assert_eq!(
                path.parent(),
                Some(config),
                "the log lands beside the sessions it describes"
            );
            assert_eq!(
                path.file_name().and_then(std::ffi::OsStr::to_str),
                Some("dch.log")
            );
        }
    }

    #[test]
    fn a_headless_run_logs_to_stderr() {
        // No screen stands between the traces and the operator: the
        // single-run mode keeps the ordinary channel.
        let config = std::path::Path::new("/home/operator/.dch");
        assert_eq!(log_sink(false, true, config), LogSink::Stderr);
    }

    #[test]
    fn a_redirected_stderr_stays_the_log_channel() {
        // A TUI whose stderr is a pipe has no drawing on that
        // descriptor, so the file only hides the log the operator
        // pointed elsewhere.
        let config = std::path::Path::new("/home/operator/.dch");
        assert_eq!(log_sink(true, false, config), LogSink::Stderr);
    }

    #[test]
    fn the_log_file_makes_its_directory_and_keeps_appending() {
        // The file the TUI is promised: a config directory that does
        // not exist yet is no reason to lose the log, and a second
        // open adds to the first rather than starting over.
        use std::io::Write as _;
        let home = tempfile::TempDir::new().expect("a temporary config home");
        let path = home.path().join(".dch").join("dch.log");
        writeln!(
            open_log_file(&path).expect("the log opens, making its directory"),
            "first"
        )
        .expect("the log takes a line");
        writeln!(open_log_file(&path).expect("the log opens again"), "second")
            .expect("the log takes a second line");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the log is readable"),
            "first\nsecond\n",
            "the log appends across opens instead of truncating"
        );
    }

    #[test]
    fn logging_wakes_only_for_a_named_directive() {
        // The gate guards the quiet contract: an installed subscriber
        // means a log file opened and appended to under every later
        // TUI keystroke, so a stray `RUST_LOG=` in an operator's
        // environment must not turn that on by accident.
        assert!(!log_requested(None), "unset stays silent");
        assert!(
            !log_requested(Some(std::ffi::OsStr::new(""))),
            "an exported empty RUST_LOG means no logging, not the fallback"
        );
        assert!(
            log_requested(Some(std::ffi::OsStr::new("info"))),
            "a named directive wakes the machinery"
        );
    }

    #[test]
    fn an_unreadable_rust_log_keeps_info_rather_than_silence() {
        // A run explicitly asked to be observable does not go quiet
        // because the directive was mistyped: the unparsable case
        // keeps the fallback level.
        let named = |text: &str| format!("{:?}", log_filter(Some(std::ffi::OsStr::new(text))));
        let info = named("info");
        assert_eq!(format!("{:?}", log_filter(None)), info, "unset falls back");
        assert_eq!(named(""), info, "empty falls back");
        assert_eq!(
            named("dch["),
            info,
            "an unparsable directive keeps info rather than answering \
             the request with silence"
        );
        assert_ne!(named("debug"), info, "a named level is not the fallback");
    }

    #[test]
    fn session_listings_never_take_the_bootstrap_marker() {
        let list = args::Args::try_parse_from(["dch", "--list-sessions"]).unwrap();
        assert!(!bootstrap_marker_applies(&list, false));
    }

    #[test]
    fn a_resumed_run_takes_the_marker_of_its_mode() {
        let id = uuid::Uuid::new_v4().to_string();
        // A task argument (with or without a resume) is headless and
        // markers.
        let task = args::Args::try_parse_from(["dch", "probe"]).unwrap();
        assert!(bootstrap_marker_applies(&task, true));
        let resumed_task =
            args::Args::try_parse_from(["dch", "probe", "--resume", id.as_str()]).unwrap();
        assert!(
            bootstrap_marker_applies(&resumed_task, true),
            "a resumed headless run ran a task and markers it"
        );
        // A resume into the TUI (terminal stdin, no task) is not
        // headless and does not marker.
        let resumed_tui = args::Args::try_parse_from(["dch", "--resume", id.as_str()]).unwrap();
        assert!(!bootstrap_marker_applies(&resumed_tui, true));
    }

    #[test]
    fn single_run_invocations_take_the_bootstrap_marker() {
        let piped = args::Args::try_parse_from(["dch"]).unwrap();
        assert!(bootstrap_marker_applies(&piped, false));
    }

    #[test]
    fn the_log_open_failure_word_names_the_path_the_reason_and_the_channel() {
        // It is printed before any subscriber exists, so nothing else
        // carries these facts: which file could not be opened, why, and
        // where the traces are going instead of there.
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let notice =
            log_open_failure_notice(std::path::Path::new("/home/user/.config/dch/dch.log"), &err);
        assert!(
            notice.contains("/home/user/.config/dch/dch.log"),
            "the path that failed is named: {notice}"
        );
        assert!(notice.contains("permission denied"), "the reason: {notice}");
        assert!(
            notice.contains("stderr"),
            "the channel the traces fall back to: {notice}"
        );
    }
}
