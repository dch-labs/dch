//! `dch` — a terminal-based agentic coding assistant built on `loopctl`.
//!
//! Today the binary parses its arguments and exits; clap reports bad input
//! itself. Run-mode selection and the real dispatch arrive with the headless
//! runner and the TUI.

mod args;

fn main() -> std::process::ExitCode {
    let args = args::parse_args();
    let _ = args;
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #[test]
    fn dch_binary_compiles() {}
}
