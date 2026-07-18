//! Mutable session state shared across tools within a single agent run.

use std::collections::HashMap;
use std::time::SystemTime;

/// Mutable, in-memory session state shared between tools and the agent loop.
///
/// Stored behind a locked handle (`Arc<Mutex<SessionState>>`) inside the
/// [`RunnerContext`](crate::RunnerContext) so every tool invocation in a single
/// agent run observes and mutates the same state. Cloning the context shares
/// the store rather than copying it.
///
/// Not persisted to disk — this is per-run ephemeral state. Session persistence
/// (conversation history, saved turns) is a separate concern handled by the
/// session-saver at the binary layer, not by this type.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    /// The agent's current todo list.
    ///
    /// Replaced wholesale by the `TodoWrite` tool on each call — the model
    /// sends the complete desired list, not a delta. The TUI reads this to
    /// render its todo panel; the agent reads it to decide what's next.
    pub todos: Vec<TodoEntry>,

    /// Record of files the agent has read this session.
    ///
    /// Appended by the Read tool on each successful read, keyed by the
    /// caller-supplied path. Used to detect stale re-reads: if a file's
    /// modification time is newer than the recorded read time, the cached
    /// content should be considered stale.
    pub file_read_history: Vec<FileReadEntry>,

    /// Persistent notes the agent has stashed for later reference.
    ///
    /// Appended by tools that want to carry context forward across turns —
    /// design decisions, user constraints, codebase facts learned mid-run.
    /// Unlike [`todos`](Self::todos), these are append-only and never
    /// replaced; the agent recalls them when relevant.
    pub memory: Vec<MemoryEntry>,

    /// Aggregate counts of tool invocations over the session.
    ///
    /// Incremented per dispatch by the runner. Read for observability (the TUI
    /// status bar, headless summaries) and as a signal for cost/usage tracking.
    /// Keyed by tool name inside [`ToolStats::per_tool`].
    pub tool_stats: ToolStats,
}

/// One entry in the agent's todo list.
#[derive(Debug, Clone)]
pub struct TodoEntry {
    /// Stable identifier for this entry.
    ///
    /// Preserved across `TodoWrite` replacements so the agent can track the
    /// same item as it moves through statuses. Set by the caller (the model)
    /// when it replaces the list.
    pub id: String,

    /// Short summary of the task, in imperative form (e.g. `"Fix the bug"`).
    ///
    /// Shown as the primary label in the TUI todo panel and in headless
    /// summaries. Should be concise enough to fit one line.
    pub subject: String,

    /// Longer explanation of what the task entails.
    ///
    /// Optional detail beyond the [`subject`](Self::subject); the TUI may show
    /// it beneath the subject or on expand. May be empty when the subject is
    /// self-descriptive.
    pub description: String,

    /// Current lifecycle status of the entry.
    ///
    /// Drives the UI indicator (☐/◐/☑) and the allowed transitions. See
    /// [`TodoStatus`] for the transition rules; the tool layer enforces them
    /// when the list is replaced.
    pub status: TodoStatus,

    /// Optional present-continuous label, e.g. `"Fixing bug"`.
    ///
    /// Shown by the UI while the entry is [`TodoStatus::InProgress`] to
    /// describe *current* work in real time (as opposed to the imperative
    /// [`subject`](Self::subject), which describes the goal). `None` falls back
    /// to the subject.
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
    ///
    /// The entry has been planned but the agent has not begun work on it.
    /// Transitions to [`InProgress`](Self::InProgress) when work starts.
    Pending,

    /// Currently being worked on.
    ///
    /// The agent is actively executing this entry. Only one entry should be
    /// `InProgress` at a time (enforced by convention at the tool layer).
    /// Transitions to [`Completed`](Self::Completed) when done, or back to
    /// [`Pending`](Self::Pending) if deferred.
    InProgress,

    /// Finished.
    ///
    /// The agent considers this entry done. Transitions back to
    /// [`InProgress`](Self::InProgress) if the work is revisited; cannot go
    /// directly to [`Pending`](Self::Pending) (restart via `InProgress`).
    Completed,
}

/// Record of a single file the agent has read.
///
/// Appended to [`SessionState::file_read_history`](crate::state::SessionState::file_read_history)
/// by the Read tool on each successful read. The pair of path + timestamp lets
/// the agent detect whether a cached read is still fresh: if the file's
/// modification time is newer than `read_at`, the content may have changed and
/// should be re-read.
#[derive(Debug, Clone)]
pub struct FileReadEntry {
    /// The path that was read.
    ///
    /// Stored exactly as supplied to the Read tool (pre-resolution), so it
    /// matches the string the model used and can be compared against
    /// subsequent read requests without resolving paths.
    pub path: String,

    /// When the read happened.
    ///
    /// Captured at read time via [`SystemTime::now`]. Compared against the
    /// file's modification time to detect staleness: if the file was modified
    /// after this point, the previously-read content is stale and should be
    /// re-read before acting on it.
    pub read_at: SystemTime,
}

/// A persistent note the agent has stashed for later reference.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// Free-form grouping label for the note.
    ///
    /// Common values include `"decision"`, `"constraint"`, `"preference"` —
    /// letting the agent bucket recalled context by type. There is no fixed
    /// vocabulary; the agent invents categories as needed.
    pub category: String,
    /// The note content, in free-form text.
    ///
    /// Whatever the agent decided was worth stashing: a design decision, a
    /// user constraint, a fact learned about the codebase. Recalled into the
    /// conversation when relevant.
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
