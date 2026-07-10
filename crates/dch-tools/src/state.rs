//! Mutable session state shared across tools within a single agent run.

use std::collections::HashMap;
use std::time::SystemTime;

/// Mutable, in-memory session state shared between tools and the agent loop.
///
/// Stored behind a locked handle inside the runner context so every tool
/// invocation observes and mutates the same state.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    /// The agent's current todo list.
    pub todos: Vec<TodoEntry>,
    /// Record of files the agent has read this session.
    pub file_read_history: Vec<FileReadEntry>,
    /// Persistent notes the agent has stashed for later reference.
    pub memory: Vec<MemoryEntry>,
    /// Aggregate counts of tool invocations.
    pub tool_stats: ToolStats,
}

/// One entry in the agent's todo list.
#[derive(Debug, Clone)]
pub struct TodoEntry {
    /// Stable identifier for this entry.
    pub id: String,
    /// Short summary of the task.
    pub subject: String,
    /// Longer explanation of what the task entails.
    pub description: String,
    /// Current lifecycle status of the entry.
    pub status: TodoStatus,
    /// Optional present-continuous label, e.g. `"Fixing bug"`.
    pub active_form: Option<String>,
}

/// Lifecycle status of a single todo entry.
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
    /// The path that was read.
    pub path: String,
    /// When the read happened.
    pub read_at: SystemTime,
}

/// A persistent note the agent has stashed for later reference.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// Grouping label for the note.
    pub category: String,
    /// The note content.
    pub content: String,
}

/// Aggregate counts of tool invocations over a session.
#[derive(Debug, Clone, Default)]
pub struct ToolStats {
    /// Total invocations across all tools.
    pub total_calls: u64,
    /// Per-tool-name invocation counts.
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
