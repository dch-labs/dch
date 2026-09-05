//! The permission vocabulary: tool categories and the mode × category
//! decision matrix.
//!
//! The permission system classifies every tool into a [`ToolCategory`], and
//! resolves a [`PermissionMode`] × category pair into a [`PermissionOutcome`]
//! through [`decide`]. Both halves are pure and synchronous; the prompting
//! I/O belongs to the enforcement hook that will consume them, which turns
//! [`Ask`](PermissionOutcome::Ask) into a question for the user.

use tracing::warn;

/// The runner's autonomy level, mirrored for mode-aware logic in dch-tools.
///
/// dch-config owns the serde-bearing canonical definition (selected by the
/// `[runner] permission_mode` config key); this copy exists so dch-tools can
/// express mode-aware classification without depending on dch-config.
/// Whatever consumes this mirror converts the config-side mode into it at
/// dispatch time — one conversion point — and the variants stay ordered and
/// named identically, keeping that conversion a pure relabeling.
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
    /// The most conservative mode, confirming each action individually.
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
    /// `Write`, `Edit`, and `MultiEdit` land here; the shipped tools pair
    /// this category with a linter gate at the tool layer, but the
    /// permission system treats the write itself as the side-effect.
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

    /// The fail-closed default for names the classification table does not
    /// know.
    ///
    /// Not a behavioral class of tool: it exists so an unclassified name —
    /// an MCP tool under an arbitrary name, a typo, a tool registered
    /// before its entry — can never run unconfirmed. Only `Auto` allows
    /// it; every other mode asks or blocks.
    Unclassified,
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
/// names, in their `PascalCase` registry form. Unknown names — a
/// misconfigured registry, a typo, a tool added before its entry —
/// classify as [`ToolCategory::Unclassified`] and emit a warning so the gap
/// is observable in logs. `Unclassified` fails closed: only `Auto` runs
/// it, every other mode asks or blocks.
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
            warn!(tool = %name, "unclassified tool — failing closed");
            ToolCategory::Unclassified
        }
    }
}

/// The synchronous decision for a given (mode, category) pair, reached
/// before any user prompting.
///
/// [`Ask`](PermissionOutcome::Ask) routes through the question channel; a
/// headless host has no channel, so it becomes a block rather than a prompt
/// that will never come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Run the tool without confirmation.
    ///
    /// The dispatch proceeds as if no permission system existed.
    Allow,

    /// Refuse the tool call.
    ///
    /// The consumer is expected to turn the call into a soft error
    /// addressed to the model, naming the mode that blocked it.
    Block,

    /// Ask the user before running the tool.
    ///
    /// The consumer is expected to route through the question channel; a
    /// host that cannot ask (headless) converts this outcome into a block.
    Ask,
}

/// Resolve the mode × category decision matrix into an outcome.
///
/// The matrix is the behavioral contract of the whole permission system:
///
/// | Mode ＼ Category   | `FileRead` | `FileWrite` | `ShellExecute` | `Network` | `Meta` | `Unclassified` |
/// |-------------------|------------|-------------|----------------|-----------|--------|----------------|
/// | `Auto`            | allow      | allow       | allow          | allow     | allow  | allow          |
/// | `Plan`            | allow      | block       | block          | block     | block  | block          |
/// | `AcceptEdits`     | allow      | allow       | ask            | ask       | ask    | ask            |
/// | `Interactive`     | ask        | ask         | ask            | ask       | ask    | ask            |
///
/// [`PermissionOutcome::Allow`] runs the tool; [`PermissionOutcome::Block`]
/// refuses it; [`PermissionOutcome::Ask`] prompts the user (or blocks, in a
/// host that cannot ask).
#[must_use]
pub fn decide(mode: PermissionMode, category: ToolCategory) -> PermissionOutcome {
    use PermissionMode::{AcceptEdits, Auto, Interactive, Plan};
    use PermissionOutcome::{Allow, Ask, Block};
    use ToolCategory::{FileRead, FileWrite, Meta, Network, ShellExecute, Unclassified};
    match (mode, category) {
        (Auto, _) | (Plan, FileRead) | (AcceptEdits, FileRead | FileWrite) => Allow,
        (Plan, FileWrite | ShellExecute | Network | Meta | Unclassified) => Block,
        (Interactive, FileRead | FileWrite)
        | (AcceptEdits | Interactive, ShellExecute | Network | Meta | Unclassified) => Ask,
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
    use crate::registry::builtin_registry;

    /// Every v1 tool with the category this table prescribes.
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
    fn every_registered_tool_has_a_category_row() {
        // The registry and the category table are linked only by convention;
        // this pins that a registered tool never silently falls to the
        // unknown-name default.
        let registry = builtin_registry();
        let registered: Vec<&str> = registry
            .all_tools()
            .into_iter()
            .map(loopctl::Tool::name)
            .collect();
        assert!(!registered.is_empty(), "the registry must not be empty");
        for name in registered {
            assert!(
                TOOL_TABLE.iter().any(|(row_name, _)| *row_name == name),
                "{name} is registered but has no category row"
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
        assert!(!ToolCategory::Unclassified.is_read_only());
    }

    #[test]
    fn unknown_names_fail_closed_as_unclassified() {
        for name in ["DefinitelyNotATool", "", "read", "Bash ", "TODO_WRITE"] {
            assert_eq!(
                tool_category(name),
                ToolCategory::Unclassified,
                "{name:?} must fail closed as Unclassified"
            );
        }
    }

    #[test]
    fn unclassified_never_allows_outside_auto() {
        // The fail-closed guarantee: only Auto runs an unclassified tool.
        for mode in [
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::Interactive,
        ] {
            assert_ne!(
                decide(mode, ToolCategory::Unclassified),
                PermissionOutcome::Allow,
                "{mode:?} × Unclassified must not allow"
            );
        }
        assert_eq!(
            decide(PermissionMode::Auto, ToolCategory::Unclassified),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn the_mode_category_matrix_resolves_every_cell() {
        // The behavioral contract, one row per cell: any single-cell
        // regression names its (mode, category) pair in the failure.
        use PermissionMode::{AcceptEdits, Auto, Interactive, Plan};
        use PermissionOutcome::{Allow, Ask, Block};
        use ToolCategory::{FileRead, FileWrite, Meta, Network, ShellExecute, Unclassified};

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
            ("auto × unclassified", Auto, Unclassified, Allow),
            ("plan × unclassified", Plan, Unclassified, Block),
            (
                "accept_edits × unclassified",
                AcceptEdits,
                Unclassified,
                Ask,
            ),
            ("interactive × unclassified", Interactive, Unclassified, Ask),
        ];
        assert_eq!(MATRIX.len(), 24, "the matrix is 4 modes × 6 categories");
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
            ToolCategory::Unclassified,
        ] {
            assert_eq!(
                decide(PermissionMode::Plan, category) == PermissionOutcome::Allow,
                category.is_read_only()
            );
        }
    }
}
