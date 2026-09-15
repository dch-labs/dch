//! `dch` — a terminal-based agentic coding assistant built on `loopctl`.
//!
//! Run-mode dispatch: a task argument or a piped stdin routes to the
//! single-run headless mode (one task, non-interactive, exit code to the
//! shell); otherwise the interactive TUI session is hosted. `--resume` and
//! `--list-sessions` are not implemented and exit with an error.

mod args;
mod done;
mod headless;
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
            if wants_headless(&args)
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
        if args.list_sessions {
            eprintln!("--list-sessions is not yet available");
            std::process::ExitCode::from(1)
        } else if args.resume.is_some() {
            eprintln!("--resume is not yet available");
            std::process::ExitCode::from(1)
        } else if wants_headless(&args) {
            let startup_args = args.clone();
            let startup_mode = headless::capture_marker_mode(args.done_file.as_ref());
            let startup_bridge = signals::install_construction_handler(move || {
                headless::write_startup_done_file(&startup_args, startup_mode.as_ref());
            });
            let code = headless::run_headless(&args, startup_bridge).await;
            std::process::ExitCode::from(code)
        } else {
            std::process::ExitCode::from(tui::run_tui(&args).await)
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
}
