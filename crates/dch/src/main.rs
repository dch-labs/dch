//! `dch` — a terminal-based agentic coding assistant built on `loopctl`.
//!
//! Run-mode dispatch: `--headless` routes to the headless runner (one task,
//! non-interactive, exit code to the shell); other modes arrive with later
//! milestones.

mod args;
mod done;
mod headless;

fn main() -> std::process::ExitCode {
    let args = args::parse_args();

    if args.headless.is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|err| {
                eprintln!("dch: failed to start tokio runtime: {err}");
                std::process::exit(1);
            });
        let code = runtime.block_on(headless::run_headless(&args));
        std::process::ExitCode::from(code)
    } else {
        eprintln!("interactive mode not yet implemented; use --headless");
        std::process::ExitCode::from(1)
    }
}
