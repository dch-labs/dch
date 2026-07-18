//! Runtime display and permission settings carried by the runner context.

/// How much output the agent emits to the console.
///
/// Read by the console observer and TUI to decide which lifecycle events and
/// tool details to print. Higher levels are inclusive of lower ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    /// Errors only; suppresses informational and debug output.
    Quiet,
    /// Default informational output (tool announcements, streaming text).
    /// This is the default when no override is given.
    #[default]
    Normal,
    /// Debug-level detail; adds timing, internal decisions, and extra context.
    Verbose,
}

/// When the agent prompts for permission before side-effecting actions.
///
/// Each mode is a policy over the five tool categories (`FileRead`, `FileWrite`,
/// `ShellExecute`, Network, Meta). The full mode × category matrix lives in the
/// architecture doc; in short: `Auto` allows everything, `Plan` blocks all
/// side-effects, `AcceptEdits` auto-allows `FileWrite` only, and `Interactive`
/// prompts for every side-effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Never prompts; all actions proceed autonomously. This is the default.
    #[default]
    Auto,
    /// Proposes actions but executes nothing — all side-effecting tools are
    /// blocked.
    Plan,
    /// Applies file edits automatically, prompts for everything else (shell,
    /// network, and meta tools).
    AcceptEdits,
    /// Prompts before every side-effecting action, including file reads.
    Interactive,
}

/// Runtime display and permission settings.
///
/// Built from configuration (and possibly CLI flags) and carried in the runner
/// context so every tool observes the active settings.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// How much output to emit. Read by observers to filter lifecycle events
    /// and tool-detail announcements.
    pub verbosity: Verbosity,
    /// Whether to disable ANSI color in output. Read by the diff renderer and
    /// any tool that emits colored text; derived from config or a `--no-color`
    /// flag.
    pub no_color: bool,
    /// When to prompt for permission before side-effecting actions. Read by
    /// the permission hook to gate each tool dispatch by its category.
    pub permission_mode: PermissionMode,
}

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::expect_used,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    #[test]
    fn default_runtime_config() {
        let r = RuntimeConfig::default();
        assert_eq!(r.verbosity, Verbosity::Normal);
        assert!(!r.no_color);
        assert_eq!(r.permission_mode, PermissionMode::Auto);
    }
}
