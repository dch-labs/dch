//! The agent's todo list types.
//!
//! [`TodoEntry`] and [`TodoStatus`] are the data model for the per-run todo
//! list the `TodoWrite` tool maintains. The list itself lives on
//! [`RunnerContext`](crate::RunnerContext) as a per-run, tool-facing store.
//!
//! Session-scoped records (file-touch history for staleness detection, etc.)
//! are intentionally not defined here: they are owned by the outer runner
//! layer (`dch-loop`), recorded via an observer on tool dispatch, not by the
//! tools themselves.

/// One entry in the agent's todo list.
///
/// Stored as a `Vec<TodoEntry>` on [`RunnerContext`](crate::RunnerContext) and
/// replaced wholesale on each `TodoWrite` call. The stable id lets the agent
/// and UI track the same item across status transitions even though the list
/// itself is replaced, not patched.
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
