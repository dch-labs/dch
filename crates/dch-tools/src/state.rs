//! Mutable session state shared across tools within a single agent run.

use std::collections::HashMap;
use std::time::SystemTime;

/// Mutable, in-memory session state shared between tools and the agent loop.
///
/// Stored behind a locked handle inside the runner context so every tool
/// invocation observes and mutates the same state.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    /// The agent's current todo list. Replaced wholesale by the `TodoWrite` tool
    /// (not appended to); the TUI reads this for display.
    pub todos: Vec<TodoEntry>,
    /// Record of files the agent has read this session. Appended by the Read
    /// tool on each successful read; used to detect stale re-reads.
    pub file_read_history: Vec<FileReadEntry>,
    /// Persistent notes the agent has stashed for later reference. Appended by
    /// tools that want to carry context forward across turns.
    pub memory: Vec<MemoryEntry>,
    /// Aggregate counts of tool invocations over the session. Incremented per
    /// dispatch; read for observability and summaries.
    pub tool_stats: ToolStats,
}

/// One entry in the agent's todo list.
#[derive(Debug, Clone)]
pub struct TodoEntry {
    /// Stable identifier for this entry. Preserved across `TodoWrite` replacements
    /// so the agent can track the same item as it moves through statuses.
    pub id: String,
    /// Short summary of the task.
    pub subject: String,
    /// Longer explanation of what the task entails.
    pub description: String,
    /// Current lifecycle status of the entry. See [`TodoStatus`] for the
    /// allowed transitions.
    pub status: TodoStatus,
    /// Optional present-continuous label, e.g. `"Fixing bug"`. Shown by the UI
    /// while the entry is [`TodoStatus::InProgress`] to describe current work.
    pub active_form: Option<String>,
}

/// Lifecycle status of a single todo entry.
///
/// Valid transitions: `Pending → InProgress`, `InProgress → Completed`, and
/// backwards (`InProgress → Pending`, `Completed → InProgress`) when the agent
/// revisits work. `Completed → Pending` is not allowed (restart via
/// `InProgress`). Enforced at the tool layer, not by the type system, since
/// the list is replaced wholesale by `TodoWrite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    /// Not yet started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished.
    Completed,
}

/// Record of a single file the agent has read.
#[derive(Debug, Clone)]
pub struct FileReadEntry {
    /// The path that was read, as supplied to the Read tool (pre-resolution).
    /// Used to match subsequent reads for staleness detection.
    pub path: String,
    /// When the read happened. Compared against file modification times to
    /// detect whether the cached content is stale.
    pub read_at: SystemTime,
}

/// A persistent note the agent has stashed for later reference.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// Free-form grouping label for the note (e.g. `"decision"`, `"constraint"`),
    /// letting the agent bucket recalled context.
    pub category: String,
    /// The note content.
    pub content: String,
}

/// Aggregate counts of tool invocations over a session.
#[derive(Debug, Clone, Default)]
pub struct ToolStats {
    /// Total invocations across all tools this session. The sum of every value
    /// in [`per_tool`](Self::per_tool).
    pub total_calls: u64,
    /// Per-tool-name invocation counts, keyed by the tool's registered name
    /// (e.g. `"Read"`, `"Edit"`). Incremented on each dispatch.
    pub per_tool: HashMap<String, u64>,
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
    fn default_session_state_is_empty() {
        let s = SessionState::default();
        assert!(s.todos.is_empty());
        assert!(s.file_read_history.is_empty());
        assert!(s.memory.is_empty());
        assert_eq!(s.tool_stats.total_calls, 0);
        assert!(s.tool_stats.per_tool.is_empty());
    }
}
