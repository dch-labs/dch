//! Runtime display and permission settings carried by the runner context.

/// How much output the agent emits to the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    /// Errors only.
    Quiet,
    /// Default informational output.
    #[default]
    Normal,
    /// Debug-level detail.
    Verbose,
}

/// When the agent prompts for permission before side-effecting actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Never prompts; all actions proceed autonomously.
    #[default]
    Auto,
    /// Proposes actions but executes nothing.
    Plan,
    /// Applies file edits automatically, prompts for everything else.
    AcceptEdits,
    /// Prompts before every side-effecting action.
    Interactive,
}

/// Runtime display and permission settings.
///
/// Built from configuration (and possibly CLI flags) and carried in the runner
/// context so every tool observes the active settings.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// How much output to emit.
    pub verbosity: Verbosity,
    /// Whether to disable ANSI color in output.
    pub no_color: bool,
    /// When to prompt for permission before side-effecting actions.
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
