//! The agent's todo list: data model and the `TodoWrite` tool.
//!
//! [`TodoEntry`] and [`TodoStatus`] are the data model for the per-run todo
//! list the [`TodoTool`] maintains; the list itself lives on
//! [`RunnerContext`](crate::RunnerContext) as a per-run, tool-facing store,
//! replaced wholesale on every `TodoWrite` call.
//!
//! Session-scoped records (file-touch history for staleness detection, etc.)
//! are intentionally not defined here: they are owned by the outer runner
//! layer (`dch-loop`), recorded via an observer on tool dispatch, not by the
//! tools themselves.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use loopctl::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::context::runner_ctx;

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
///
/// Serialized as a `snake_case` string (`"pending"`, `"in_progress"`,
/// `"completed"`); a missing status defaults to
/// [`Pending`](TodoStatus::Pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not yet started.
    ///
    /// The entry has been planned but the agent has not begun work on it.
    /// Transitions to [`InProgress`](Self::InProgress) when work starts.
    #[default]
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

/// Track progress with a structured task list (the `TodoWrite` tool).
///
/// Replaces the agent's entire todo list on every call: the model sends the
/// complete desired list, and the stored list becomes exactly that. Not
/// read-only and not concurrency-safe — the call mutates the shared todo
/// store, so two calls must not run in parallel.
pub struct TodoTool;

/// The tool input parsed from the submitted JSON.
///
/// A thin typed skin over the raw value: the schema is hand-built (see
/// [`TodoTool::schema`]), and this struct performs the strict per-entry
/// validation the raw JSON cannot express.
#[derive(Debug, Deserialize)]
struct TodoListInput {
    /// The complete replacement list.
    ///
    /// Every entry the model sends replaces the stored list in full; there
    /// is no patch mode, so omitted entries are deletions.
    todos: Vec<TodoEntryInput>,
}

/// One todo entry as the model submits it.
///
/// Carries three fields the stored [`TodoEntry`] does not keep (`owner`,
/// `blocked_by`, `blocks`); they are validated, rendered in the summary, and
/// dropped when the list is stored.
#[derive(Debug, Clone, Deserialize)]
struct TodoEntryInput {
    /// Stable identifier for the entry.
    ///
    /// Must be non-empty and unique within the list: transitions are matched
    /// by id, and downstream consumers (the todo panel, session summaries)
    /// track entries by it.
    #[serde(deserialize_with = "deserialize_nonempty_trimmed_string")]
    id: String,

    /// Short imperative subject.
    ///
    /// Null, empty, and whitespace-only values are rejected; the recovery
    /// pass has usually already filled these in before parsing.
    #[serde(deserialize_with = "deserialize_nonempty_trimmed_string")]
    subject: String,

    /// Longer explanation of the task.
    ///
    /// Empty when the subject stands alone; rendered under the subject in
    /// the summary the model reads back.
    #[serde(default)]
    description: String,

    /// Lifecycle status.
    ///
    /// Defaults to pending when omitted; every change against the previously
    /// stored entry must follow the documented transition graph.
    #[serde(default)]
    status: TodoStatus,

    /// Present-continuous label shown while the entry is in progress.
    ///
    /// Optional; falls back to the subject for display when absent.
    #[serde(default)]
    active_form: Option<String>,

    /// Ids that must complete before this entry.
    ///
    /// Validated against the ids in the same submission, rendered in the
    /// summary, and not stored on the entry.
    #[serde(default)]
    blocked_by: Vec<String>,

    /// Ids waiting on this entry.
    ///
    /// Same lifecycle as `blocked_by`: rendered from the submission, not
    /// stored.
    #[serde(default)]
    blocks: Vec<String>,
}

/// Deserializes a non-empty, trimmed string, rejecting null outright.
///
/// The hard backstop for the identifier and subject contracts: the recovery
/// pass fills recoverable subjects before parsing, so anything reaching this
/// deserializer as null or blank is malformed beyond recovery.
///
/// # Errors
///
/// Returns a custom deserializer error when the value is null, or blank
/// after whitespace trimming.
fn deserialize_nonempty_trimmed_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Err(serde::de::Error::custom("value cannot be null or empty"))
            } else {
                Ok(trimmed.to_string())
            }
        }
        None => Err(serde::de::Error::custom("value cannot be null")),
    }
}

/// Derive a subject from a description, falling back to the entry id.
///
/// Takes the first line of the description, truncated to roughly fifty
/// characters when longer; entries without a description become
/// `"Task {id}"`.
fn derive_subject(description: &str, id: &str) -> String {
    if description.is_empty() {
        return format!("Task {id}");
    }
    let first_line = description.lines().next().unwrap_or(description);
    if first_line.chars().count() > 50 {
        format!("{}...", first_line.chars().take(47).collect::<String>())
    } else {
        first_line.to_string()
    }
}

/// Whether the raw input needed subject recovery before parsing.
///
/// Repair common model mistakes in the raw input before parsing: unwrap a
/// doubly-serialized `todos` string (the model JSON-encoded the array a
/// second time, which needs no notice), and fill a null, missing, or
/// whitespace-only `subject` from the entry's description or id — that one
/// is announced, because the model's own words were replaced. Returns
/// whether any subject was recovered.
fn preprocess_todo_input(input: &mut Value) -> bool {
    let mut subject_recovered = false;

    if let Some(todos_val) = input.get_mut("todos")
        && let Some(s) = todos_val.as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
    {
        *todos_val = parsed;
    }

    if let Some(todos) = input.get_mut("todos").and_then(|t| t.as_array_mut()) {
        for todo in todos {
            if let Some(obj) = todo.as_object_mut() {
                let subject_needs_fix = obj.get("subject").is_none_or(|s| {
                    s.is_null() || (s.is_string() && s.as_str().is_none_or(|s| s.trim().is_empty()))
                });

                if subject_needs_fix {
                    subject_recovered = true;
                    let description = obj
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    let id = obj.get("id").and_then(|i| i.as_str()).unwrap_or("unknown");

                    obj.insert(
                        "subject".to_string(),
                        Value::String(derive_subject(description, id)),
                    );
                }
            }
        }
    }

    subject_recovered
}

/// Whether a status change is allowed by the documented transition graph.
///
/// Same-status is always fine — the model resends unchanged entries on every
/// call. The forward path is `pending → in_progress → completed`; the
/// backwards edges `in_progress → pending` and `completed → in_progress`
/// support revisited work. `completed → pending` and the stage-skipping
/// `pending → completed` are rejected.
fn transition_allowed(previous: TodoStatus, next: TodoStatus) -> bool {
    previous == next
        || matches!(
            (previous, next),
            (
                TodoStatus::InProgress,
                TodoStatus::Completed | TodoStatus::Pending
            ) | (
                TodoStatus::Pending | TodoStatus::Completed,
                TodoStatus::InProgress
            )
        )
}

/// Human-readable status name for validation messages.
///
/// Wording matches the summary lines ("in progress", not the `in_progress`
/// identifier), so rejection text reads like the render.
fn status_name(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in progress",
        TodoStatus::Completed => "completed",
    }
}

impl TodoTool {
    /// Body of [`Tool::call`], separated so the trait method stays a
    /// one-liner.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when no runner context is installed,
    /// and [`ToolError::InvalidInput`] when the input cannot be parsed, a
    /// `blocked_by` reference names an absent id, or an entry's status
    /// change is not in the allowed transition set. The stored list is only
    /// mutated when every validation has passed.
    fn todo_write_inner(
        mut input: Value,
        todos: Option<Arc<std::sync::Mutex<Vec<TodoEntry>>>>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(todos) = todos else {
            return Err(ToolError::Execution(
                "TodoWrite requires the runner context extension".to_string(),
            ));
        };

        let recovered = preprocess_todo_input(&mut input);

        let parsed: TodoListInput = serde_json::from_value(input).map_err(|error| {
            let error_msg = error.to_string();
            let hint = if error_msg.contains("subject") {
                "\n\nHint: Each todo must have a non-empty 'subject' field. \
                 Example: {\"id\": \"1\", \"subject\": \"Fix the bug\"}"
            } else if error_msg.contains("missing field `todos`") {
                "\n\nHint: Provide the whole list under the 'todos' key. \
                 Example: {\"todos\": [{\"id\": \"1\", \"subject\": \"Fix the bug\"}]}"
            } else if error_msg.contains("missing field") {
                "\n\nHint: Missing required field. Required fields: 'id', 'subject'."
            } else if error_msg.contains("invalid type") {
                "\n\nHint: Check that 'todos' is an array of objects, not a string."
            } else {
                ""
            };
            ToolError::InvalidInput(format!("Failed to parse todo input: {error}{hint}"))
        })?;

        if parsed.todos.is_empty() {
            let mut store = todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            store.clear();
            drop(store);
            return Ok(ToolOutput::text("Todo list cleared."));
        }

        let mut seen_ids: Vec<&str> = Vec::new();
        for todo in &parsed.todos {
            if seen_ids.contains(&todo.id.as_str()) {
                return Err(ToolError::InvalidInput(format!(
                    "Duplicate todo ID '{}'. Every entry needs a unique id.",
                    todo.id
                )));
            }
            seen_ids.push(&todo.id);
        }

        for todo in &parsed.todos {
            for blocked_id in &todo.blocked_by {
                if !parsed.todos.iter().any(|other| &other.id == blocked_id) {
                    return Err(ToolError::InvalidInput(format!(
                        "Todo '{}' is blocked by non-existent todo ID '{}'. \
                         Ensure all referenced todos exist in the same call.",
                        todo.id, blocked_id
                    )));
                }
            }
        }

        let entries: Vec<TodoEntry> = parsed
            .todos
            .iter()
            .map(|todo| TodoEntry {
                id: todo.id.clone(),
                subject: todo.subject.clone(),
                description: todo.description.clone(),
                status: todo.status,
                active_form: todo.active_form.clone(),
            })
            .collect();

        {
            let mut store = todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for entry in &entries {
                if let Some(existing) = store.iter().find(|existing| existing.id == entry.id) {
                    let previous = existing.status;
                    if !transition_allowed(previous, entry.status) {
                        return Err(ToolError::InvalidInput(format!(
                            "Todo '{}' cannot move from {} to {} (restart a completed \
                             task via in progress).",
                            entry.id,
                            status_name(previous),
                            status_name(entry.status)
                        )));
                    }
                }
            }
            *store = entries;
        }

        let mut output = String::new();

        if recovered {
            writeln!(
                output,
                "Auto-recovered: Some todo subjects were derived from their descriptions or ids.\n"
            )
            .ok();
        }

        writeln!(output, "# Task List\n").ok();

        let pending_count = parsed
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Pending)
            .count();
        let in_progress_count = parsed
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .count();
        let completed_count = parsed
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Completed)
            .count();

        write!(
            output,
            "Summary: {pending_count} pending, {in_progress_count} in progress, {completed_count} completed\n\n"
        )
        .ok();

        let mut sorted_todos = parsed.todos;
        sorted_todos.sort_by(|a, b| {
            let order = |status: &TodoStatus| match status {
                TodoStatus::InProgress => 0,
                TodoStatus::Pending => 1,
                TodoStatus::Completed => 2,
            };
            order(&a.status).cmp(&order(&b.status))
        });

        for todo in &sorted_todos {
            let status_icon = match todo.status {
                TodoStatus::Pending => "○",
                TodoStatus::InProgress => "◐",
                TodoStatus::Completed => "●",
            };
            writeln!(output, "{} [{}] {}", status_icon, todo.id, todo.subject).ok();

            if !todo.description.is_empty() {
                writeln!(output, "  {}", todo.description).ok();
            }

            if !todo.blocked_by.is_empty() {
                writeln!(output, "  Blocked by: {}", todo.blocked_by.join(", ")).ok();
            }

            if !todo.blocks.is_empty() {
                writeln!(output, "  Blocks: {}", todo.blocks.join(", ")).ok();
            }

            output.push('\n');
        }

        Ok(ToolOutput::text(output))
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }

    fn description(&self) -> &'static str {
        "Track progress with a structured task list. Use this for complex multi-step tasks. \
         This helps track progress, organize work, and demonstrates thoroughness. \
         Create tasks with clear, actionable subjects in imperative form \
         (e.g., \"Fix bug\", \"Add feature\")."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete replacement todo list",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "Unique identifier for the todo" },
                                "subject": { "type": "string", "description": "Short subject/title (imperative form)" },
                                "description": { "type": "string", "description": "Detailed description" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Current status",
                                    "default": "pending"
                                },
                                "active_form": { "type": "string", "description": "Present continuous form for display (e.g., \"Running tests\")" },
                                "owner": { "type": "string", "description": "Agent that owns this task" },
                                "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Tasks that must complete before this one (IDs)" },
                                "blocks": { "type": "array", "items": { "type": "string" }, "description": "Tasks that wait for this one (IDs)" }
                            },
                            "required": ["id", "subject"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let todos = runner_ctx(ctx).map(|context| Arc::clone(&context.todos));
        Box::pin(async move { Self::todo_write_inner(input, todos) })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::context::RunnerContext;
    use crate::registry::builtin_registry;

    fn entry(status: TodoStatus, active_form: Option<&str>) -> TodoEntry {
        TodoEntry {
            id: "1".to_string(),
            subject: "Ship the tool".to_string(),
            description: "Wire it into the registry".to_string(),
            status,
            active_form: active_form.map(str::to_string),
        }
    }

    #[test]
    fn entry_construction_keeps_every_field() {
        let e = entry(TodoStatus::InProgress, Some("Shipping the tool"));
        assert_eq!(e.id, "1");
        assert_eq!(e.subject, "Ship the tool");
        assert_eq!(e.description, "Wire it into the registry");
        assert_eq!(e.status, TodoStatus::InProgress);
        assert_eq!(e.active_form.as_deref(), Some("Shipping the tool"));
    }

    #[test]
    fn active_form_none_is_a_valid_construction() {
        let e = entry(TodoStatus::Pending, None);
        assert!(e.active_form.is_none());
    }

    #[test]
    fn status_variants_are_distinct() {
        assert_ne!(TodoStatus::Pending, TodoStatus::InProgress);
        assert_ne!(TodoStatus::InProgress, TodoStatus::Completed);
        assert_ne!(TodoStatus::Pending, TodoStatus::Completed);
    }

    #[test]
    fn all_three_statuses_round_trip_through_an_entry() {
        // The type imposes no transitions: any status is constructible. The
        // documented Pending → InProgress → Completed progression is a
        // tool-layer convention, asserted here as construction-level facts.
        for status in [
            TodoStatus::Pending,
            TodoStatus::InProgress,
            TodoStatus::Completed,
        ] {
            assert_eq!(entry(status, None).status, status);
        }
    }

    #[test]
    fn clone_round_trips_the_entry() {
        let original = entry(TodoStatus::Completed, Some("Finishing"));
        let twin = original.clone();
        assert_eq!(twin.id, original.id);
        assert_eq!(twin.status, original.status);
        assert_eq!(twin.active_form, original.active_form);
    }

    /// A tool context carrying the runner extension, plus a handle on the
    /// todo store it shares, so tests can assert on the stored state.
    fn todo_context() -> (ToolContext, Arc<std::sync::Mutex<Vec<TodoEntry>>>) {
        let mut ctx = ToolContext::default();
        let context = RunnerContext::new(std::path::PathBuf::from("/tmp/todo-tool-test"));
        let todos = Arc::clone(&context.todos);
        ctx.set_extension(context);
        (ctx, todos)
    }

    fn seed(todos: &Arc<std::sync::Mutex<Vec<TodoEntry>>>, id: &str, status: TodoStatus) {
        todos.lock().unwrap().push(TodoEntry {
            id: id.to_string(),
            subject: format!("Seed {id}"),
            description: String::new(),
            status,
            active_form: None,
        });
    }

    #[test]
    fn todo_write_is_registered_under_its_name() {
        assert!(
            builtin_registry().get("TodoWrite").is_some(),
            "TodoWrite must ship in the builtin registry"
        );
    }

    #[test]
    fn schema_describes_the_todos_array() {
        let schema = TodoTool.schema();
        assert_eq!(schema.tool, "TodoWrite");
        assert_eq!(schema.input_schema["required"][0], "todos");
        let item = &schema.input_schema["properties"]["todos"]["items"];
        assert_eq!(item["required"][0], "id");
        assert_eq!(item["required"][1], "subject");
        assert_eq!(
            item["properties"]["status"]["enum"][1],
            json!("in_progress")
        );
    }

    #[test]
    fn todo_write_mutates_state_by_contract() {
        let tool = TodoTool;
        assert!(!Tool::is_read_only(&tool));
        assert!(!Tool::is_concurrency_safe(&tool));
    }

    #[test]
    fn status_serializes_as_snake_case_and_defaults_to_pending() {
        assert_eq!(TodoStatus::default(), TodoStatus::Pending);
        assert_eq!(
            serde_json::to_string(&TodoStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::from_str::<TodoStatus>("\"completed\"").unwrap(),
            TodoStatus::Completed
        );
    }

    #[tokio::test]
    async fn entries_persist_to_the_todo_store() {
        let (ctx, todos) = todo_context();
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "Fix bug", "status": "in_progress"},
                    {"id": "2", "subject": "Add feature"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let store = todos.lock().unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.first().unwrap().id, "1");
        assert_eq!(store.first().unwrap().status, TodoStatus::InProgress);
        assert_eq!(store.last().unwrap().status, TodoStatus::Pending);
    }

    #[tokio::test]
    async fn todo_write_replaces_the_whole_list() {
        let (ctx, todos) = todo_context();
        seed(&todos, "old", TodoStatus::Completed);
        TodoTool
            .call(
                json!({"todos": [{"id": "new", "subject": "New task"}]}),
                &ctx,
            )
            .await
            .unwrap();
        let store = todos.lock().unwrap();
        assert_eq!(store.len(), 1, "the previous list is gone");
        assert_eq!(store.first().unwrap().id, "new");
    }

    #[tokio::test]
    async fn empty_call_clears_the_store_and_reports_cleared() {
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let out = TodoTool.call(json!({"todos": []}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "Todo list cleared.");
        assert!(todos.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn extra_fields_render_but_are_not_stored() {
        let (ctx, todos) = todo_context();
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "First", "owner": "agent-7",
                     "blocked_by": ["2"], "blocks": ["2"]},
                    {"id": "2", "subject": "Second"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        let rendered = out.text_content();
        assert!(rendered.contains("Blocked by: 2"), "{rendered}");
        assert!(rendered.contains("Blocks: 2"), "{rendered}");
        let store = todos.lock().unwrap();
        assert_eq!(store.len(), 2);
        assert!(
            store.iter().all(|entry| entry.description.is_empty()),
            "the wire-only fields must not leak into the stored entries"
        );
    }

    #[tokio::test]
    async fn missing_context_errors_cleanly() {
        let ctx = ToolContext::default();
        let err = TodoTool.call(json!({"todos": []}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "{err:?}");
    }

    #[tokio::test]
    async fn allowed_status_transitions_are_accepted() {
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Completed);
        seed(&todos, "2", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "Seed 1", "status": "in_progress"},
                    {"id": "2", "subject": "Seed 2", "status": "in_progress"},
                    {"id": "3", "subject": "Seed 3", "status": "completed"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        // New entry "3" may start at any status; the resends of "1" and "2"
        // exercise the revisit and forward edges.
    }

    #[tokio::test]
    async fn completed_to_pending_is_rejected_without_touching_the_store() {
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Completed);
        let err = TodoTool
            .call(
                json!({"todos": [{"id": "1", "subject": "Seed 1", "status": "pending"}]}),
                &ctx,
            )
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("cannot move from completed to pending"),
            "{message}"
        );
        assert!(message.contains('1'), "{message}");
        assert_eq!(
            todos.lock().unwrap().first().unwrap().status,
            TodoStatus::Completed,
            "a rejected call must leave the stored list untouched"
        );
    }

    #[tokio::test]
    async fn pending_to_completed_skip_is_rejected() {
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let err = TodoTool
            .call(
                json!({"todos": [{"id": "1", "subject": "Seed 1", "status": "completed"}]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot move from pending to completed"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn invalid_blocked_by_reference_is_rejected() {
        let (ctx, _todos) = todo_context();
        let err = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "First", "blocked_by": ["ghost"]}
                ]}),
                &ctx,
            )
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("ghost"), "{message}");
        assert!(message.contains("blocked by non-existent"), "{message}");
    }

    #[tokio::test]
    async fn duplicate_ids_are_rejected_without_touching_the_store() {
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let err = TodoTool
            .call(
                json!({"todos": [
                    {"id": "2", "subject": "First"},
                    {"id": "2", "subject": "Second", "status": "completed"}
                ]}),
                &ctx,
            )
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Duplicate todo ID '2'"), "{message}");
        // The previously stored list survives the rejected call.
        assert_eq!(todos.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn blank_ids_are_rejected() {
        let (ctx, _todos) = todo_context();
        let err = TodoTool
            .call(json!({"todos": [{"id": "   ", "subject": "First"}]}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("value cannot be null or empty"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn full_render_lists_entries_summary_and_dependencies() {
        let (ctx, _todos) = todo_context();
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "Done task", "status": "completed"},
                    {"id": "2", "subject": "Current task", "status": "in_progress",
                     "description": "Details here"},
                    {"id": "3", "subject": "Pending task", "blocked_by": ["2"]}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        let rendered = out.text_content();
        assert!(rendered.contains("# Task List"), "{rendered}");
        assert!(
            rendered.contains("Summary: 1 pending, 1 in progress, 1 completed"),
            "{rendered}"
        );
        assert!(rendered.contains("◐ [2] Current task"), "{rendered}");
        assert!(rendered.contains("Details here"), "{rendered}");
        assert!(rendered.contains("Blocked by: 2"), "{rendered}");
        // In-progress work sorts ahead of pending, which sorts ahead of done.
        let in_progress = rendered.find("◐").unwrap();
        let pending = rendered.find("○").unwrap();
        let completed = rendered.find("●").unwrap();
        assert!(in_progress < pending && pending < completed, "{rendered}");
    }

    #[tokio::test]
    async fn null_subject_is_recovered_from_the_description() {
        let (ctx, todos) = todo_context();
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": null, "description": "Write the tests"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.text_content().contains("Auto-recovered"),
            "{}",
            out.text_content()
        );
        assert_eq!(
            todos.lock().unwrap().first().unwrap().subject,
            "Write the tests"
        );
    }

    #[tokio::test]
    async fn whitespace_only_subject_is_recovered() {
        let (ctx, todos) = todo_context();
        TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "   ", "description": "Write the tests"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            todos.lock().unwrap().first().unwrap().subject,
            "Write the tests"
        );
    }

    #[tokio::test]
    async fn null_subject_without_description_uses_the_id() {
        let (ctx, todos) = todo_context();
        TodoTool
            .call(json!({"todos": [{"id": "7", "subject": null}]}), &ctx)
            .await
            .unwrap();
        assert_eq!(todos.lock().unwrap().first().unwrap().subject, "Task 7");
    }

    #[tokio::test]
    async fn subject_is_trimmed() {
        let (ctx, todos) = todo_context();
        TodoTool
            .call(
                json!({"todos": [{"id": "1", "subject": "  Trimmed task  "}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            todos.lock().unwrap().first().unwrap().subject,
            "Trimmed task"
        );
    }

    #[tokio::test]
    async fn double_encoded_todos_unwrap_silently() {
        // The second encoding is a transport mistake, not a content change:
        // fixing it needs no notice when every subject was already real.
        let (ctx, todos) = todo_context();
        let inner = json!([{"id": "1", "subject": "Real subject"}]).to_string();
        let out = TodoTool.call(json!({"todos": inner}), &ctx).await.unwrap();
        assert!(
            !out.text_content().contains("Auto-recovered"),
            "{}",
            out.text_content()
        );
        assert_eq!(
            todos.lock().unwrap().first().unwrap().subject,
            "Real subject"
        );
    }

    #[tokio::test]
    async fn unwrap_plus_subject_recovery_still_announces_the_recovery() {
        let (ctx, todos) = todo_context();
        let inner = json!([{"id": "1", "subject": null, "description": "Write docs"}]).to_string();
        let out = TodoTool.call(json!({"todos": inner}), &ctx).await.unwrap();
        assert!(
            out.text_content().contains("Auto-recovered"),
            "{}",
            out.text_content()
        );
        assert_eq!(todos.lock().unwrap().first().unwrap().subject, "Write docs");
    }

    #[tokio::test]
    async fn missing_todos_key_hint_names_the_key() {
        let (ctx, _todos) = todo_context();
        let err = TodoTool.call(json!({}), &ctx).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("missing field `todos`"), "{message}");
        assert!(message.contains("'todos' key"), "{message}");
    }

    #[tokio::test]
    async fn parse_errors_carry_actionable_hints() {
        let (ctx, _todos) = todo_context();
        let err = TodoTool
            .call(json!({"todos": [{"subject": "no id here"}]}), &ctx)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Hint"), "{message}");
        assert!(message.contains("missing field"), "{message}");
    }
}
