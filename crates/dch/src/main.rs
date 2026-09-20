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

fn main() -> std::process::ExitCode {
    let args = args::parse_args();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            let message = format!("failed to start tokio runtime: {err}");
            // The marker precedes any stderr output: a broken stderr
            // panics `eprintln!`, and this path exists to guarantee both.
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
}
