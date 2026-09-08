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
            eprintln!("dch: failed to start tokio runtime: {err}");
            std::process::exit(1);
        });
    runtime.block_on(async move {
        if args.headless.is_some() {
            let startup_args = args.clone();
            let startup_bridge = signals::install_construction_handler(move || {
                headless::write_startup_done_file(&startup_args);
            });
            let code = headless::run_headless(&args, startup_bridge).await;
            std::process::ExitCode::from(code)
        } else {
            eprintln!("interactive mode not yet implemented; use --headless");
            std::process::ExitCode::from(1)
        }
    })
}
