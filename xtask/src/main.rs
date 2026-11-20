//! Workspace automation tasks for the dch monorepo.
//!
//! Run with `cargo run -p xtask -- <subcommand>`. Currently exposes:
//! - `check-boundary` — verify the workspace's crate-dependency boundary.

// Tests assert on internals and fixtures: panicking helpers,
// `unwrap` and `expect` on states the fixture itself built, and
// docs-free test fns are the test canon, so the denied-lint set
// stands down for the whole crate under `cfg(test)` — one
// justification, in one place, instead of one per module.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        clippy::unwrap_used,
    )
)]

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
