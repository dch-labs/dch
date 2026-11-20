//! The permission vocabulary: tool categories and the mode × category
//! decision matrix.
//!
//! The permission system classifies every tool into a [`ToolCategory`], and
//! resolves a [`PermissionMode`] × category pair into a [`PermissionOutcome`]
//! through [`decide`]. Both halves are pure and synchronous; the prompting
//! I/O belongs to the enforcement layer that consumes them, which turns
//! [`Ask`](PermissionOutcome::Ask) into a prompt for the user — or a denial
//! when no one is there to ask.

use crate::mode::PermissionMode;
use tracing::warn;

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
    /// accepts: `Read`, `ReadFiles`, `FileViewer`, `Glob`, `Grep`,
    /// `CodeSearch`, `Tree`, and `LSP` all observe the workspace
    /// without changing it.
    FileRead,

    /// Creates, overwrites, or edits files.
    ///
    /// `Write`, `Edit`, and `MultiEdit` land here; the shipped tools pair
    /// this category with a linter gate at the tool layer, but the
    /// permission system treats the write itself as the side-effect.
    FileWrite,

    /// Executes shell commands, which can touch anything the shell can.
    ///
    /// Bash is the whole category: its reach is unbounded, so outside
    /// Auto mode a call runs only through the reader gate's judgment
    /// of what the command claims or a covering rule — the matrix
    /// itself asks or blocks.
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
    /// and may run tests, `AskUserQuestion` blocks for user input, and
    /// `Jobs` manages the background-job table — `cleanup_jobs` mutates it.
    /// Never read-only: the side-effects are at the session level, not the
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
    /// todo list, a question tool blocks for user input — and treating the
    /// category as read-only would let read-only mode run `Submit` and `Jobs`,
    /// the tools that reach the session's outsides, defeating the point
    /// of the mode.
    #[must_use]
    pub fn is_read_only(self) -> bool {
        matches!(self, ToolCategory::FileRead)
    }
}

/// Map a tool's registered name to its [`ToolCategory`].
///
/// Matching is exact and case-sensitive against the registered tool
/// names as the dependency tree builds them: every builtin, dch's own
/// and the framework's file reader alike, is `PascalCase`. Unknown
/// names — a misconfigured registry, a typo, a tool added before its
/// entry — classify as [`ToolCategory::Unclassified`] and emit a warning
/// so the gap is observable in logs. `Unclassified` fails closed: only
/// `Auto` runs it, every other mode asks or blocks.
#[must_use]
pub fn tool_category(name: &str) -> ToolCategory {
    match name {
        "Read" | "ReadFiles" | "Glob" | "Grep" | "CodeSearch" | "Tree" | "FileViewer" | "LSP" => {
            ToolCategory::FileRead
        }
        "Write" | "Edit" | "MultiEdit" => ToolCategory::FileWrite,
        "Bash" => ToolCategory::ShellExecute,
        "WebFetch" => ToolCategory::Network,
        "TodoWrite" | "Submit" | "AskUserQuestion" | "Jobs" => ToolCategory::Meta,
        _ => {
            warn!(tool = %name, "unclassified tool — failing closed");
            ToolCategory::Unclassified
        }
    }
}

/// The synchronous decision for a given (mode, category) pair, reached
/// before any user prompting.
///
/// [`Ask`](PermissionOutcome::Ask) resolves through the enforcement
/// layer's prompt resolver; a host that supplies no resolver (headless)
/// sees it become a denial rather than a prompt that will never come.
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
    /// The consumer is expected to resolve it through its prompt
    /// resolver — running the tool on approval, denying on refusal; a
    /// host that cannot ask (headless) converts this outcome into a
    /// denial.
    Ask,
}

/// Resolve the mode × category decision matrix into an outcome.
///
/// The matrix is the behavioral contract of the whole permission system:
///
/// | Mode ＼ Category    | `FileRead` | `FileWrite` | `ShellExecute` | `Network` | `Meta` | `Unclassified` |
/// |--------------------|------------|-------------|----------------|-----------|--------|----------------|
/// | `Auto`             | allow      | allow       | allow          | allow     | allow  | allow          |
/// | `ReadOnly`         | allow      | block       | block          | block     | block  | block          |
/// | `Interactive`      | allow      | ask         | ask            | ask       | ask    | ask            |
///
/// `ReadOnly`'s line is absolute — nothing loosens it, though remembered
/// deny rules still tighten (a user's "never" outranks even a read).
/// `Interactive`'s asks resolve through the memory first: a rule covering
/// the call answers it without prompting. Two carve-outs run ahead of
/// this matrix: the session's furniture (todo, question, jobs reads) is
/// free in all three modes, and a shell command the reader gate judges
/// to only look is allowed — the gate sits in the engine's evaluation,
/// before this table is consulted.
///
/// [`PermissionOutcome::Allow`] runs the tool; [`PermissionOutcome::Block`]
/// refuses it; [`PermissionOutcome::Ask`] prompts the user (or blocks, in a
/// host that cannot ask).
#[must_use]
pub fn decide(mode: PermissionMode, category: ToolCategory) -> PermissionOutcome {
    use PermissionMode::{Auto, Interactive, ReadOnly};
    use PermissionOutcome::{Allow, Ask, Block};
    use ToolCategory::{FileRead, FileWrite, Meta, Network, ShellExecute, Unclassified};
    match (mode, category) {
        (Auto, _) | (ReadOnly | Interactive, FileRead) => Allow,
        (ReadOnly, FileWrite | ShellExecute | Network | Meta | Unclassified) => Block,
        (Interactive, FileWrite | ShellExecute | Network | Meta | Unclassified) => Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every v1 tool with the category this table prescribes.
    ///
    /// One row per tool: adding a tool means adding a row here and an arm in
    /// [`tool_category`], and the tests iterate this table so the two cannot
    /// drift.
    const TOOL_TABLE: &[(&str, ToolCategory)] = &[
        ("Read", ToolCategory::FileRead),
        ("ReadFiles", ToolCategory::FileRead),
        ("Write", ToolCategory::FileWrite),
        ("Edit", ToolCategory::FileWrite),
        ("MultiEdit", ToolCategory::FileWrite),
        ("Bash", ToolCategory::ShellExecute),
        ("Jobs", ToolCategory::Meta),
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
        assert!(!ToolCategory::Unclassified.is_read_only());
    }

    #[test]
    fn unknown_names_fail_closed_as_unclassified() {
        for name in ["DefinitelyNotATool", "", "READS", "Bash ", "TODO_WRITE"] {
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
        for mode in [PermissionMode::ReadOnly, PermissionMode::Interactive] {
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
        use PermissionMode::{Auto, Interactive, ReadOnly};
        use PermissionOutcome::{Allow, Ask, Block};
        use ToolCategory::{FileRead, FileWrite, Meta, Network, ShellExecute, Unclassified};

        const MATRIX: &[(&str, PermissionMode, ToolCategory, PermissionOutcome)] = &[
            ("auto × file_read", Auto, FileRead, Allow),
            ("auto × file_write", Auto, FileWrite, Allow),
            ("auto × shell_execute", Auto, ShellExecute, Allow),
            ("auto × network", Auto, Network, Allow),
            ("auto × meta", Auto, Meta, Allow),
            ("auto × unclassified", Auto, Unclassified, Allow),
            ("read_only × file_read", ReadOnly, FileRead, Allow),
            ("read_only × file_write", ReadOnly, FileWrite, Block),
            ("read_only × shell_execute", ReadOnly, ShellExecute, Block),
            ("read_only × network", ReadOnly, Network, Block),
            ("read_only × meta", ReadOnly, Meta, Block),
            ("read_only × unclassified", ReadOnly, Unclassified, Block),
            ("interactive × file_read", Interactive, FileRead, Allow),
            ("interactive × file_write", Interactive, FileWrite, Ask),
            (
                "interactive × shell_execute",
                Interactive,
                ShellExecute,
                Ask,
            ),
            ("interactive × network", Interactive, Network, Ask),
            ("interactive × meta", Interactive, Meta, Ask),
            ("interactive × unclassified", Interactive, Unclassified, Ask),
        ];
        assert_eq!(MATRIX.len(), 18, "the matrix is 3 modes × 6 categories");
        for (cell, mode, category, expected) in MATRIX {
            assert_eq!(decide(*mode, *category), *expected, "{cell}");
        }
    }

    #[test]
    fn read_only_mode_allow_exactly_matches_is_read_only() {
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
                decide(PermissionMode::ReadOnly, category) == PermissionOutcome::Allow,
                category.is_read_only()
            );
        }
    }
}
