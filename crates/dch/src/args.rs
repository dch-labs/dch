//! CLI argument parsing for the `dch` binary.
//!
//! [`Args`] is the parsed input — flags and values only. It does not load
//! configuration, open sessions, or choose a run mode; those consume the
//! parsed struct downstream.

use std::path::PathBuf;

use clap::Parser;

/// How much of the session listing to show.
///
/// The `--list-sessions` flag's optional value: absent means the
/// recent default, a number widens the window, `all` removes it —
/// one type so the flag's parsing, the listing's cap, and the
/// truncated-table footer all agree on what was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListCount {
    /// The most recent N sessions.
    ///
    /// The newest-first listing keeps its head — the sessions a
    /// user is most likely reaching for — and reports how many
    /// older ones stayed hidden.
    Count(usize),

    /// Every saved session.
    ///
    /// No cap and no footer: the table runs as long as the history
    /// does.
    All,
}

/// Parse a `--list-sessions` value: a positive count or `all`.
///
/// # Errors
///
/// Refuses anything that is neither a positive whole number nor
/// `all` (case-insensitive) — clap surfaces the message as the
/// flag's usage error.
fn parse_list_count(value: &str) -> Result<ListCount, String> {
    if value.eq_ignore_ascii_case("all") {
        return Ok(ListCount::All);
    }
    match value.parse::<usize>() {
        Ok(count) if count > 0 => Ok(ListCount::Count(count)),
        _ => Err(format!("expected a positive count or 'all', got '{value}'")),
    }
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "dch",
    version,
    about = "A terminal-based agentic coding assistant built on loopctl",
    long_about = None
)]
/// dch — a terminal-based agentic coding assistant built on loopctl.
///
/// A task argument (`dch "fix the bug"`) or a piped stdin
/// (`echo "fix the bug" | dch`) selects the single-run non-interactive
/// mode; with neither, launches the interactive TUI. `--resume`/
/// `--list-sessions` manage sessions. Run-mode selection happens in
/// `main`, not here; this struct is only the parsed input.
pub struct Args {
    /// The task for a single non-interactive run.
    ///
    /// A non-empty value is the task verbatim; an empty one defers to
    /// stdin, exactly like omitting the argument with stdin piped.
    /// Composes with `--resume` — a task plus a session id continues
    /// that session headless.
    #[arg(
        value_name = "TASK",
        conflicts_with_all = ["list_sessions"],
        help_heading = "Mode"
    )]
    pub task: Option<String>,

    /// Resume a previously saved session by id.
    ///
    /// The id is validated here — a malformed value is a parse error rather
    /// than a later load failure. Long-only: a 36-character id has no useful
    /// short form. Combines with a task argument or a piped stdin to resume
    /// headless, and with neither to resume in the TUI.
    #[arg(
        long,
        value_name = "SESSION_ID",
        value_parser = clap::value_parser!(uuid::Uuid),
        conflicts_with_all = ["list_sessions"],
        help_heading = "Sessions"
    )]
    pub resume: Option<uuid::Uuid>,

    /// Print known sessions and exit.
    ///
    /// Newest first, limited by default to the ten most recent —
    /// enough to find what was just closed without paging past a
    /// long history. A value widens it: a count
    /// (`--list-sessions 30`) or `all` for everything.
    #[arg(
        long,
        value_name = "COUNT|all",
        value_parser = parse_list_count,
        num_args = 0..=1,
        default_missing_value = "10",
        help_heading = "Sessions"
    )]
    pub list_sessions: Option<ListCount>,

    /// Continue the most recently saved session.
    ///
    /// Resolves to whatever session was saved last, sparing the
    /// `--list-sessions` and `--resume <id>` round trip. Composes
    /// like `--resume`: with a task argument or piped stdin it
    /// continues headless, with neither in the TUI. No saved
    /// session exists means a fresh start with a note.
    #[arg(
        long = "continue",
        action = clap::ArgAction::SetTrue,
        help_heading = "Sessions",
        conflicts_with_all = ["resume", "list_sessions"]
    )]
    pub continue_session: Option<bool>,

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

    /// Config-facing switches, flattened into [`Args`].
    ///
    /// The flags a run can layer over the loaded config file — an
    /// alternate path, a contained-paths opt-out — grouped so the
    /// top-level struct stays a small bag of mode flags while each
    /// switch keeps its own help heading and docs.
    #[command(flatten)]
    pub config: ConfigArgs,

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

/// The config-facing switches, flattened into [`Args`].
///
/// One group so the top-level struct stays a small bag of mode flags; the
/// individual flags keep their own help headings and docs.
#[derive(Debug, Clone, Parser)]
pub struct ConfigArgs {
    /// Path to an alternate config file.
    ///
    /// Overrides the default `~/.dch` lookup. Points at a file, not a
    /// directory, and is kept exactly as given — relative paths resolve
    /// against the process's current directory when the file is loaded.
    #[arg(
        long = "config",
        value_name = "PATH",
        value_parser = clap::value_parser!(PathBuf),
        help_heading = "Config"
    )]
    pub config_path: Option<PathBuf>,

    /// Let file tools reach paths outside the working directory for this
    /// run.
    ///
    /// Forces `[runner] unsafe_paths` on, overriding a `false` (or absent)
    /// config value; the flag cannot revoke a config-level opt-out. File
    /// tools can then read, write, and scan system paths (`/etc/nginx`,
    /// `/home/you/.ssh`, `/var/log`); paths are used verbatim and `~` is
    /// not expanded — use it when the task reaches outside the project
    /// directory. The default keeps every file tool confined to the working
    /// directory.
    #[arg(long, help_heading = "Config")]
    pub unsafe_paths: bool,
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
        assert_eq!(args.task, None);
        assert_eq!(args.resume, None);
        assert_eq!(args.list_sessions, None);
        assert_eq!(args.theme, None);
        assert_eq!(args.model, None);
        assert_eq!(args.config.config_path, None);
        assert!(!args.config.unsafe_paths);
        assert!(!args.verbose);
        assert!(!args.quiet);
        assert_eq!(args.done_file, None);
    }

    #[test]
    fn task_argument_captures_the_inline_task() {
        let args = parse(&["fix the bug"]).unwrap();
        assert_eq!(args.task, Some("fix the bug".to_string()));
    }

    #[test]
    fn an_empty_task_argument_parses_and_defers_to_stdin() {
        // The prompt resolver treats `Some("")` as "read the task from
        // stdin", so the parser must hand an empty positional through
        // verbatim rather than normalizing it to absence.
        let args = parse(&[""]).unwrap();
        assert_eq!(args.task, Some(String::new()));
    }

    #[test]
    fn the_task_argument_conflicts_with_the_listing_verb() {
        let err = parse(&["t", "--list-sessions"]).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::ArgumentConflict,
            "a task cannot combine with the listing verb: {err}"
        );
    }

    #[test]
    fn continue_parses_and_conflicts_with_resume() {
        let args = parse(&["--continue"]).unwrap();
        assert!(args.continue_session.is_some());
        assert_eq!(args.resume, None);

        assert!(
            parse(&[
                "--continue",
                "--resume",
                "05ce48c6-34cd-4d03-b68c-e5d54cc6f62c"
            ])
            .is_err(),
            "--continue and --resume are mutually exclusive"
        );
    }

    #[test]
    fn the_task_argument_composes_with_resume() {
        let id = uuid::Uuid::new_v4();
        let args = parse(&["fix the tests", "--resume", &id.to_string()]).unwrap();
        assert_eq!(args.task, Some("fix the tests".to_string()));
        assert_eq!(args.resume, Some(id));
    }

    #[test]
    fn resume_conflicts_with_the_listing_verb() {
        let id = uuid::Uuid::new_v4();
        let err = parse(&["--resume", &id.to_string(), "--list-sessions"]).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::ArgumentConflict,
            "resuming and listing cannot combine: {err}"
        );
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
        assert_eq!(args.list_sessions, Some(ListCount::Count(10)));
        let args = parse(&["--list-sessions", "3"]).unwrap();
        assert_eq!(args.list_sessions, Some(ListCount::Count(3)));
        let args = parse(&["--list-sessions", "all"]).unwrap();
        assert_eq!(args.list_sessions, Some(ListCount::All));
        assert!(
            parse(&["--list-sessions", "0"]).is_err(),
            "zero is not a count"
        );
        assert!(
            parse(&["--list-sessions", "many"]).is_err(),
            "junk is refused"
        );
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
        assert_eq!(args.config.config_path, Some(PathBuf::from("./x.toml")));
    }

    #[test]
    fn unsafe_paths_flag_parses_and_defaults_absent() {
        assert!(!parse(&[]).unwrap().config.unsafe_paths);
        assert!(parse(&["--unsafe-paths"]).unwrap().config.unsafe_paths);
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
        let args = parse(&["--model", "m", "t", "-v", "--done-file", "d"]).unwrap();
        assert_eq!(args.model, Some("m".to_string()));
        assert_eq!(args.task, Some("t".to_string()));
        assert!(args.verbose);
        assert_eq!(args.done_file, Some(PathBuf::from("d")));
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
            "TASK",
            "--resume",
            "--list-sessions",
            "--theme",
            "--model",
            "--config",
            "--unsafe-paths",
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
