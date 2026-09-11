//! `dch` — a terminal-based agentic coding assistant built on `loopctl`.
//!
//! Run-mode dispatch: `--headless` routes to the headless runner (one task,
//! non-interactive, exit code to the shell); other modes arrive with later
//! milestones.

mod args;
mod done;
mod headless;
mod signals;

fn main() -> std::process::ExitCode {
    let args = args::parse_args();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            let message = format!("failed to start tokio runtime: {err}");
            // The marker precedes any stderr output: a broken stderr
            // panics `eprintln!`, and this path exists to guarantee both.
            if args.headless.is_some()
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
        if args.headless.is_some() {
            let startup_args = args.clone();
            let startup_mode = headless::capture_marker_mode(args.done_file.as_ref());
            let startup_bridge = signals::install_construction_handler(move || {
                headless::write_startup_done_file(&startup_args, startup_mode.as_ref());
            });
            let code = headless::run_headless(&args, startup_bridge).await;
            std::process::ExitCode::from(code)
        } else if args.list_sessions {
            eprintln!("--list-sessions is not yet available");
            std::process::ExitCode::from(1)
        } else if args.resume.is_some() {
            eprintln!("--resume is not yet available");
            std::process::ExitCode::from(1)
        } else {
            eprintln!("interactive mode not yet implemented; use --headless");
            std::process::ExitCode::from(1)
        }
    })
}
