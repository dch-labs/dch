//! The permission vocabulary: tool categories and the mode × category
//! decision matrix.
//!
//! The permission system classifies every tool into a [`ToolCategory`], and
//! resolves a [`PermissionMode`] × category pair into a [`PermissionOutcome`]
//! through [`decide`]. Both halves are pure and synchronous: the runner's
//! permission hook (which owns the prompting I/O) calls [`decide`] and turns
//! `Ask` into a question for the user.

use tracing::warn;

/// The runner's autonomy level, mirrored for mode-aware logic in dch-tools.
///
/// dch-config owns the serde-bearing canonical definition (selected by the
/// `[runner] permission_mode` config key); this copy exists so dch-tools can
/// express mode-aware classification without depending on dch-config. The
/// hook that consumes it converts the config-side mode into this mirror at
/// dispatch time — one conversion point, and the variants stay ordered and
/// named identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Never prompts.
    ///
    /// Lets every side-effecting action run without confirmation. The most
    /// autonomous mode.
    Auto,

    /// Proposes but never executes.
    ///
    /// Produces a plan of intended actions without performing any of them.
    Plan,

    /// Auto-applies file edits; prompts for everything else.
    ///
    /// A middle ground that trusts edits but still asks before other
    /// side-effecting work.
    AcceptEdits,

    /// Prompts before every side-effecting action.
    ///
    /// Only genuinely read-only work runs without confirmation.
    Interactive,
}

/// Coarse classification of a tool, used by the permission system to decide
/// whether to prompt.
///
/// A category is a rendering-safety contract, not a description: two tools in
/// one category are equivalent from the permission system's point of view,
/// whatever else they differ in. Map a registered tool name with
/// [`tool_category`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    /// Reads files and directory structure; never mutates anything.
    ///
    /// The only category [`is_read_only`](ToolCategory::is_read_only)
    /// accepts: `Read`, `FileViewer`, `Glob`, `Grep`, `CodeSearch`, `Tree`,
    /// and `LSP` all
    /// observe the workspace without changing it.
    FileRead,

    /// Creates, overwrites, or edits files.
    ///
    /// `Write`, `Edit`, and `MultiEdit` land here; shipped palettes pair the
    /// category with a linter gate at the tool layer, but the permission
    /// system treats the write itself as the side-effect.
    FileWrite,

    /// Executes shell commands, which can touch anything the shell can.
    ///
    /// Bash is the whole category: its reach is unbounded, so it never
    /// auto-runs outside Auto mode regardless of what the command claims
    /// to do.
    ShellExecute,

    /// Performs network egress.
    ///
    /// `WebFetch` is the v1 member. Egress is its own category — distinct
    /// from file writes — because the risk is exfiltration and untrusted
    /// content, not local mutation.
    Network,

    /// Mutates session or workflow state.
    ///
    /// `TodoWrite` replaces the shared todo list, `Submit` produces a patch
    /// and may run tests, and `AskUserQuestion` blocks for user input. Never
    /// read-only: the side-effects are at the session level, not the
    /// filesystem, which is exactly why they evade file-based checks.
    Meta,
}

impl ToolCategory {
    /// Whether actions in this category never require a prompt.
    ///
    /// Only [`FileRead`](ToolCategory::FileRead) qualifies. `Meta` tools are
    /// deliberately excluded even though they never touch the filesystem:
    /// they carry real session-level side-effects — submitting a task
    /// produces a patch and may run tests, `TodoWrite` mutates the shared
    /// todo list, a question tool blocks for user input — and treating them
    /// as read-only would let Plan mode execute them, defeating the point
    /// of Plan.
    #[must_use]
    pub fn is_read_only(self) -> bool {
        matches!(self, ToolCategory::FileRead)
    }
}

/// Map a tool's registered name to its [`ToolCategory`].
///
/// Matching is exact and case-sensitive: the names are the registered tool
/// names, in their `PascalCase` registry form. Unknown names — a misconfigured
/// registry, a typo, a future tool added before its entry — default to
/// [`ToolCategory::FileWrite`], the most restrictive write category, and
/// emit a warning so the gap is observable in logs: an unclassified tool is
/// over-gated (it prompts where it might not need to) rather than silently
/// under-gated.
#[must_use]
pub fn tool_category(name: &str) -> ToolCategory {
    match name {
        "Read" | "Glob" | "Grep" | "CodeSearch" | "Tree" | "FileViewer" | "LSP" => {
            ToolCategory::FileRead
        }
        "Write" | "Edit" | "MultiEdit" => ToolCategory::FileWrite,
        "Bash" => ToolCategory::ShellExecute,
        "WebFetch" => ToolCategory::Network,
        "TodoWrite" | "Submit" | "AskUserQuestion" => ToolCategory::Meta,
        _ => {
            warn!(tool = %name, "unclassified tool — defaulting to FileWrite");
            ToolCategory::FileWrite
        }
    }
}

/// The synchronous decision the permission hook reaches for a given
/// (mode, category) pair, before any user prompting.
///
/// [`Ask`](PermissionOutcome::Ask) means the hook must route through the
/// question channel; a headless host has no channel, so its hook turns `Ask`
/// into a block rather than hanging on a prompt that will never come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Run the tool without confirmation.
    ///
    /// The hook lets the dispatch proceed as if no permission system
    /// existed.
    Allow,

    /// Refuse the tool call.
    ///
    /// The hook turns the call into a soft error addressed to the model,
    /// naming the mode that blocked it.
    Block,

    /// Ask the user before running the tool.
    ///
    /// The hook routes through the question channel; a host that cannot
    /// ask (headless) converts this outcome into a block.
    Ask,
}

/// Resolve the mode × category decision matrix into an outcome.
///
/// The matrix is the behavioral contract of the whole permission system:
///
/// | Mode ＼ Category   | `FileRead` | `FileWrite` | `ShellExecute` | `Network` | `Meta` |
/// |-------------------|------------|-------------|----------------|-----------|--------|
/// | `Auto`            | allow      | allow       | allow          | allow     | allow  |
/// | `Plan`            | allow      | block       | block          | block     | block  |
/// | `AcceptEdits`     | allow      | allow       | ask            | ask       | ask    |
/// | `Interactive`     | ask        | ask         | ask            | ask       | ask    |
///
/// [`PermissionOutcome::Allow`] runs the tool; [`PermissionOutcome::Block`]
/// refuses it; [`PermissionOutcome::Ask`] prompts the user (or blocks, in a
/// host that cannot ask).
#[must_use]
pub fn decide(mode: PermissionMode, category: ToolCategory) -> PermissionOutcome {
    use PermissionMode::{AcceptEdits, Auto, Interactive, Plan};
    use PermissionOutcome::{Allow, Ask, Block};
    match (mode, category) {
        (Auto, _)
        | (Plan, ToolCategory::FileRead)
        | (AcceptEdits, ToolCategory::FileRead | ToolCategory::FileWrite) => Allow,
        (Plan, _) => Block,
        (AcceptEdits | Interactive, _) => Ask,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]
mod tests {
    use super::*;

    /// Every v1 tool with the category the reference table prescribes.
    ///
    /// One row per tool: adding a tool means adding a row here and an arm in
    /// [`tool_category`], and the tests iterate this table so the two cannot
    /// drift.
    const TOOL_TABLE: &[(&str, ToolCategory)] = &[
        ("Read", ToolCategory::FileRead),
        ("Write", ToolCategory::FileWrite),
        ("Edit", ToolCategory::FileWrite),
        ("MultiEdit", ToolCategory::FileWrite),
        ("Bash", ToolCategory::ShellExecute),
        ("FileViewer", ToolCategory::FileRead),
        ("Glob", ToolCategory::FileRead),
        ("Grep", ToolCategory::FileRead),
        ("CodeSearch", ToolCategory::FileRead),
        ("Tree", ToolCategory::FileRead),
        ("TodoWrite", ToolCategory::Meta),
        ("Submit", ToolCategory::Meta),
        ("WebFetch", ToolCategory::Network),
        ("AskUserQuestion", ToolCategory::Meta),
        ("LSP", ToolCategory::FileRead),
    ];

    #[test]
    fn every_v1_tool_maps_to_its_reference_category() {
        for (name, expected) in TOOL_TABLE {
            assert_eq!(
                tool_category(name),
                *expected,
                "{name} must classify as {expected:?}"
            );
        }
    }

    #[test]
    fn is_read_only_is_true_for_file_read_only() {
        // The regression guard: Meta tools carry session-level side-effects
        // (submission produces a patch, TodoWrite mutates state, questions
        // block), so they must never count as read-only.
        assert!(ToolCategory::FileRead.is_read_only());
        assert!(!ToolCategory::FileWrite.is_read_only());
        assert!(!ToolCategory::ShellExecute.is_read_only());
        assert!(!ToolCategory::Network.is_read_only());
        assert!(!ToolCategory::Meta.is_read_only());
    }

    #[test]
    fn unknown_names_default_to_the_restrictive_write_category() {
        for name in ["DefinitelyNotATool", "", "read", "Bash ", "TODO_WRITE"] {
            assert_eq!(
                tool_category(name),
                ToolCategory::FileWrite,
                "{name:?} must fall through to FileWrite"
            );
        }
    }

    #[test]
    fn the_mode_category_matrix_resolves_every_cell() {
        // The behavioral contract, one row per cell: any single-cell
        // regression names its (mode, category) pair in the failure.
        use PermissionMode::{AcceptEdits, Auto, Interactive, Plan};
        use PermissionOutcome::{Allow, Ask, Block};
        use ToolCategory::{FileRead, FileWrite, Meta, Network, ShellExecute};

        const MATRIX: &[(&str, PermissionMode, ToolCategory, PermissionOutcome)] = &[
            ("auto × file_read", Auto, FileRead, Allow),
            ("auto × file_write", Auto, FileWrite, Allow),
            ("auto × shell_execute", Auto, ShellExecute, Allow),
            ("auto × network", Auto, Network, Allow),
            ("auto × meta", Auto, Meta, Allow),
            ("plan × file_read", Plan, FileRead, Allow),
            ("plan × file_write", Plan, FileWrite, Block),
            ("plan × shell_execute", Plan, ShellExecute, Block),
            ("plan × network", Plan, Network, Block),
            ("plan × meta", Plan, Meta, Block),
            ("accept_edits × file_read", AcceptEdits, FileRead, Allow),
            ("accept_edits × file_write", AcceptEdits, FileWrite, Allow),
            (
                "accept_edits × shell_execute",
                AcceptEdits,
                ShellExecute,
                Ask,
            ),
            ("accept_edits × network", AcceptEdits, Network, Ask),
            ("accept_edits × meta", AcceptEdits, Meta, Ask),
            ("interactive × file_read", Interactive, FileRead, Ask),
            ("interactive × file_write", Interactive, FileWrite, Ask),
            (
                "interactive × shell_execute",
                Interactive,
                ShellExecute,
                Ask,
            ),
            ("interactive × network", Interactive, Network, Ask),
            ("interactive × meta", Interactive, Meta, Ask),
        ];
        assert_eq!(MATRIX.len(), 20, "the matrix is 4 modes × 5 categories");
        for (cell, mode, category, expected) in MATRIX {
            assert_eq!(decide(*mode, *category), *expected, "{cell}");
        }
    }

    #[test]
    fn plan_mode_allow_exactly_matches_is_read_only() {
        // The predicate and the matrix agree by construction; this pins the
        // agreement at the matrix level, so a repeat of the old bug is caught
        // even if the predicate itself drifted.
        for category in [
            ToolCategory::FileRead,
            ToolCategory::FileWrite,
            ToolCategory::ShellExecute,
            ToolCategory::Network,
            ToolCategory::Meta,
        ] {
            assert_eq!(
                decide(PermissionMode::Plan, category) == PermissionOutcome::Allow,
                category.is_read_only()
            );
        }
    }
}
