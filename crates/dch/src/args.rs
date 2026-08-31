//! CLI argument parsing for the `dch` binary.
//!
//! [`Args`] is the parsed input — flags and values only. It does not load
//! configuration, open sessions, or choose a run mode; those consume the
//! parsed struct downstream.

use std::path::PathBuf;

use clap::Parser;

/// dch — a terminal-based agentic coding assistant built on loopctl.
///
/// With no mode flags, launches the interactive TUI. Use `--headless` to run
/// a single task non-interactively, or `--resume`/`--list-sessions` for
/// session management. Run-mode selection happens in `main`, not here; this
/// struct is only the parsed input.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "dch",
    version,
    about = "A terminal-based agentic coding assistant built on loopctl",
    long_about = None
)]
pub struct Args {
    /// Run a single task non-interactively and print the result to stdout.
    ///
    /// The task may be supplied inline (`--headless "fix the bug"`) or, when
    /// the value is omitted, read from stdin by the runner
    /// (`echo "fix the bug" | dch --headless`). `None` means the flag is
    /// absent; `Some("")` means the flag is present with the value still to
    /// come from stdin.
    #[arg(
        long,
        value_name = "TASK",
        num_args = 0..=1,
        default_missing_value = "",
        help_heading = "Mode"
    )]
    pub headless: Option<String>,

    /// Resume a previously saved session by id.
    ///
    /// The id is validated here — a malformed value is a parse error rather
    /// than a later load failure. Long-only: a 36-character id has no useful
    /// short form.
    #[arg(
        long,
        value_name = "SESSION_ID",
        value_parser = clap::value_parser!(uuid::Uuid),
        help_heading = "Sessions"
    )]
    pub resume: Option<uuid::Uuid>,

    /// Print known sessions (id, model, last activity, message count) and exit.
    ///
    /// Long-only: a rare action that needs no short form.
    #[arg(long, help_heading = "Sessions")]
    pub list_sessions: bool,

    /// Override the configured theme.
    ///
    /// The override is applied when configuration is consumed, not here; an
    /// unknown name is the display layer's concern.
    #[arg(long, value_name = "NAME", help_heading = "Display")]
    pub theme: Option<String>,

    /// Override the configured model.
    ///
    /// The only provider-setting override on the CLI: the `[api]` config
    /// section remains the single source of truth for base URL, credentials,
    /// and provider type. The value is not validated here; an unknown model
    /// surfaces at the first API call.
    #[arg(long, short = 'm', value_name = "MODEL", help_heading = "Display")]
    pub model: Option<String>,

    /// Path to an alternate config file (overrides the default `~/.dch`
    /// config lookup). Points at a file, not a directory, and is kept
    /// exactly as given — relative paths resolve against the process's
    /// current directory when the file is loaded.
    #[arg(
        long = "config",
        value_name = "PATH",
        value_parser = clap::value_parser!(PathBuf),
        help_heading = "Config"
    )]
    pub config_path: Option<PathBuf>,

    /// Increase verbosity.
    ///
    /// How verbosity combines with `--quiet` is resolved when configuration
    /// is applied, not here.
    #[arg(short = 'v', long, help_heading = "Output")]
    pub verbose: bool,

    /// Decrease verbosity.
    ///
    /// Like `--verbose`, the combination is resolved when configuration is
    /// applied, not here.
    #[arg(short = 'q', long, help_heading = "Output")]
    pub quiet: bool,

    /// Write a JSON completion status to this path when a headless run
    /// finishes.
    ///
    /// Ignored outside headless mode. The path is kept exactly as given and
    /// is created on exit, so a not-yet-existing path is not an error here.
    #[arg(
        long,
        value_name = "PATH",
        value_parser = clap::value_parser!(PathBuf),
        help_heading = "Mode"
    )]
    pub done_file: Option<PathBuf>,
}

/// Parse the process's command-line arguments into [`Args`].
///
/// Thin wrapper over [`Args::parse`]. On parse failure clap prints a
/// formatted error to stderr and exits non-zero; on `--help`/`--version` it
/// prints and exits 0.
#[must_use]
pub fn parse_args() -> Args {
    Args::parse()
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
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("dch").chain(args.iter().copied()))
    }

    #[test]
    fn bare_invocation_yields_all_defaults() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.headless, None);
        assert_eq!(args.resume, None);
        assert!(!args.list_sessions);
        assert_eq!(args.theme, None);
        assert_eq!(args.model, None);
        assert_eq!(args.config_path, None);
        assert!(!args.verbose);
        assert!(!args.quiet);
        assert_eq!(args.done_file, None);
    }

    #[test]
    fn headless_captures_the_inline_task() {
        let args = parse(&["--headless", "fix the bug"]).unwrap();
        assert_eq!(args.headless, Some("fix the bug".to_string()));
    }

    #[test]
    fn headless_without_a_value_still_parses() {
        // The value-less form is the stdin pipeline shape; the runner reads
        // stdin when the flag is present with no text.
        let args = parse(&["--headless"]).unwrap();
        assert!(args.headless.is_some());
    }

    #[test]
    fn resume_parses_a_valid_uuid() {
        let id = uuid::Uuid::new_v4();
        let args = parse(&["--resume", &id.to_string()]).unwrap();
        assert_eq!(args.resume, Some(id));
    }

    #[test]
    fn list_sessions_flag_parses() {
        let args = parse(&["--list-sessions"]).unwrap();
        assert!(args.list_sessions);
    }

    #[test]
    fn theme_override_parses() {
        let args = parse(&["--theme", "dracula"]).unwrap();
        assert_eq!(args.theme, Some("dracula".to_string()));
    }

    #[test]
    fn model_parses_via_both_spellings() {
        let long = parse(&["--model", "gpt-4o"]).unwrap();
        let short = parse(&["-m", "gpt-4o"]).unwrap();
        assert_eq!(long.model, Some("gpt-4o".to_string()));
        assert_eq!(short.model, Some("gpt-4o".to_string()));
    }

    #[test]
    fn config_path_is_kept_relative_and_uncanonicalized() {
        let args = parse(&["--config", "./x.toml"]).unwrap();
        assert_eq!(args.config_path, Some(PathBuf::from("./x.toml")));
    }

    #[test]
    fn verbose_parses_via_both_spellings() {
        assert!(parse(&["--verbose"]).unwrap().verbose);
        assert!(parse(&["-v"]).unwrap().verbose);
    }

    #[test]
    fn quiet_parses_via_both_spellings() {
        assert!(parse(&["--quiet"]).unwrap().quiet);
        assert!(parse(&["-q"]).unwrap().quiet);
    }

    #[test]
    fn done_file_parses() {
        let args = parse(&["--done-file", "out.json"]).unwrap();
        assert_eq!(args.done_file, Some(PathBuf::from("out.json")));
    }

    #[test]
    fn flags_compose_in_any_order() {
        let args = parse(&["--model", "m", "--headless", "t", "-v", "--done-file", "d"]).unwrap();
        assert_eq!(args.model, Some("m".to_string()));
        assert_eq!(args.headless, Some("t".to_string()));
        assert!(args.verbose);
        assert_eq!(args.done_file, Some(PathBuf::from("d")));
    }

    #[test]
    fn headless_accepts_the_equals_form() {
        let args = parse(&["--headless=t"]).unwrap();
        assert_eq!(args.headless, Some("t".to_string()));
    }

    #[test]
    fn resume_rejects_a_malformed_uuid() {
        let err = parse(&["--resume", "not-a-uuid"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        assert!(
            err.render().to_string().contains("not-a-uuid"),
            "the error must name the bad value: {err}"
        );
    }

    #[test]
    fn unknown_flag_is_rejected() {
        let err = parse(&["--bogus"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn model_without_a_value_is_rejected() {
        let err = parse(&["--model"]).unwrap_err();
        assert!(
            err.render().to_string().contains("value is required"),
            "a missing value must be a loud error: {err}"
        );
    }

    #[test]
    fn theme_without_a_value_is_rejected() {
        let err = parse(&["--theme"]).unwrap_err();
        assert!(
            err.render().to_string().contains("value is required"),
            "{err}"
        );
    }

    #[test]
    fn help_lists_every_flag_and_the_version_line() {
        let err = parse(&["--help"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
        let help = err.render().to_string();
        for flag in [
            "--headless",
            "--resume",
            "--list-sessions",
            "--theme",
            "--model",
            "--config",
            "--verbose",
            "--quiet",
            "--done-file",
            "--version",
        ] {
            assert!(help.contains(flag), "help must mention {flag}:\n{help}");
        }
    }

    #[test]
    fn version_prints_the_crate_version() {
        let err = parse(&["--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert!(
            err.render().to_string().contains(env!("CARGO_PKG_VERSION")),
            "version output must carry the crate version: {err}"
        );
    }
}
