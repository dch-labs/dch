//! Workspace automation tasks for the dch monorepo.
//!
//! Run with `cargo run -p xtask -- <subcommand>`. Currently exposes:
//! - `check-boundary` — verify the workspace's crate-dependency boundary.

use std::process::ExitCode;

mod boundary;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check-boundary") => boundary::run_check(),
        Some(other) => {
            eprintln!("xtask: unknown subcommand '{other}'");
            eprintln!("available: check-boundary");
            ExitCode::from(2)
        }
        None => {
            eprintln!("xtask: missing subcommand");
            eprintln!("available: check-boundary");
            ExitCode::from(2)
        }
    }
}
