//! The agent's todo list: data model and the `TodoWrite` tool.
//!
//! [`TodoEntry`] and [`TodoStatus`] are the data model for the session's
//! todo list the [`TodoTool`] maintains; the list itself lives on
//! [`RunnerContext`](crate::RunnerContext) as a tool-facing store that each
//! call updates in place — entries the call carries advance or join, entries
//! it omits stand unchanged, and nothing is ever removed.
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
/// Stored as a `Vec<TodoEntry>` on [`RunnerContext`](crate::RunnerContext)
/// and updated in place by each `TodoWrite` call — entries the call
/// carries advance or join, entries it omits stand unchanged, and
/// nothing is ever removed. The stable id lets the agent and UI track
/// the same item across status transitions through that patch
/// semantics.
#[derive(Debug, Clone)]
pub struct TodoEntry {
    /// Stable identifier for this entry.
    ///
    /// Preserved across `TodoWrite` calls so the agent can track the
    /// same item as it moves through statuses. Set by the caller (the
    /// model) when it files or updates the entry, and carried across
    /// runs with the plan it belongs to.
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
/// The stored graph walks `Pending → InProgress → Completed`, with the
/// backwards edges (`InProgress → Pending`, `Completed → InProgress`)
/// for revisited work. The tool layer enforces it by adjustment, not
/// rejection — a forward skip lands one rung short, each adjustment is
/// named in the result, and any number of entries may stand
/// `InProgress` at once.
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
    /// The agent is actively executing this entry. Several entries may
    /// hold the rung at once — work on one often cannot finish without
    /// another — and the discipline is to mark an entry here the moment
    /// its work begins. Transitions to [`Completed`](Self::Completed)
    /// when done, or back to [`Pending`](Self::Pending) if deferred.
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
/// Updates the entries a call carries and adds the ones it names —
/// omitted entries stand unchanged, and nothing is ever removed. Not
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
    /// The list the render walks.
    ///
    /// Parsed straight from the submission on the way in; on the way
    /// out it is rebuilt from the enforced store — the truth the
    /// model reads back — with wire-only dependency lines joined in
    /// from the submission by id.
    todos: Vec<TodoEntryInput>,
}

/// One todo entry as the model submits it.
///
/// Carries two wire-only fields the stored [`TodoEntry`] does not keep:
/// `blocked_by` is validated against the submission's ids and `blocks` is
/// rendered in the summary; neither is stored. The schema additionally
/// advertises `owner`, which is accepted for compatibility and ignored.
#[derive(Debug, Clone, Deserialize)]
struct TodoEntryInput {
    /// Stable identifier for the entry.
    ///
    /// Must be non-empty and unique within the list: transitions are matched
    /// by id, and downstream consumers (the todo panel, session summaries)
    /// track entries by it.
    #[serde(deserialize_with = "deserialize_nonempty_trimmed_string")]
    id: String,

    /// Short imperative subject, when the submission carries one.
    ///
    /// Optional on the wire: a standing entry keeps its stored subject
    /// when a patch omits this, and a newborn entry must find identity
    /// here or in `description` — the identity gate rejects anything
    /// else rather than fabricate a name.
    #[serde(default)]
    subject: Option<String>,

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
    /// Rendered from the submission like `blocked_by`, but not validated:
    /// references to absent ids pass through unflagged.
    #[serde(default)]
    blocks: Vec<String>,
}

/// Deserializes a non-empty, trimmed string, rejecting null outright.
///
/// The hard backstop for the identifier contract: a blank or null id
/// is malformed beyond recovery, and every join the tool performs —
/// transitions, dependencies, the standing-name rule — keys on it.
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

/// Derive a subject from a description.
///
/// Takes the description's first non-blank line, trimmed, truncated to
/// roughly fifty characters when longer. The id fallback is the pure
/// helper's total arm — unreachable through the tool's identity gate,
/// which rejects an entry with nothing to derive from before this runs.
fn derive_subject(description: &str, id: &str) -> String {
    let first_line = description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty());
    let Some(first_line) = first_line else {
        return format!("Task {id}");
    };
    if first_line.chars().count() > 50 {
        format!("{}...", first_line.chars().take(47).collect::<String>())
    } else {
        first_line.to_string()
    }
}

/// Repair transport-level mistakes in the raw input before parsing.
///
/// Unwraps a doubly-serialized `todos` string — the model JSON-encoded
/// the array a second time — which needs no notice. Subjects are left
/// exactly as sent: the identity gate and the standing-name rule read
/// presence and absence as meaning, and a repair pass that filled them
/// would erase that meaning.
fn preprocess_todo_input(input: &mut Value) {
    if let Some(todos_val) = input.get_mut("todos")
        && let Some(s) = todos_val.as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
    {
        *todos_val = parsed;
    }
}

/// The repair hint matching a parse failure.
///
/// Selects on the parse error's wording so the model can repair its call
/// in one round trip; empty when no hint applies.
fn parse_hint(error: &serde_json::Error) -> &'static str {
    let error_msg = error.to_string();
    if error_msg.contains("missing field `todos`") {
        "\n\nHint: Provide the whole list under the 'todos' key. \
         Example: {\"todos\": [{\"id\": \"1\", \"subject\": \"Fix the bug\"}]}"
    } else if error_msg.contains("missing field") {
        "\n\nHint: Missing required field. 'id' is required; a new entry \
         also needs a 'subject' or a 'description'."
    } else if error_msg.contains("invalid type") {
        "\n\nHint: Check that 'todos' is an array of objects, not a string."
    } else {
        ""
    }
}

/// Parse the preprocessed input into the typed list.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] carrying the underlying parse error
/// and the matching repair hint.
fn parse_todo_input(input: Value) -> Result<TodoListInput, ToolError> {
    serde_json::from_value(input).map_err(|error| {
        ToolError::InvalidInput(format!(
            "Failed to parse todo input: {error}{}",
            parse_hint(&error)
        ))
    })
}

/// Reject submissions that reuse an entry id.
///
/// Ids are the join key for status transitions across calls, so a
/// duplicate would make the transition check ambiguous.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] naming the duplicated id.
fn reject_duplicate_ids(todos: &[TodoEntryInput]) -> Result<(), ToolError> {
    let mut seen_ids: Vec<&str> = Vec::new();
    for todo in todos {
        if seen_ids.contains(&todo.id.as_str()) {
            return Err(ToolError::InvalidInput(format!(
                "Duplicate todo ID '{}'. Every entry needs a unique id.",
                todo.id
            )));
        }
        seen_ids.push(&todo.id);
    }
    Ok(())
}

/// Reject `blocked_by` references to ids the plan does not hold.
///
/// A dependency may name an entry the submission carries or one the
/// standing store holds — a call that carries only its own update can
/// still depend on a standing entry (omission is not removal). Only
/// an id neither holds is dangling, and rejected rather than silently
/// resolved.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] naming the referencing entry and
/// the missing id.
fn reject_unknown_blocked_by(
    todos: &[TodoEntryInput],
    standing_ids: &[String],
) -> Result<(), ToolError> {
    let mut known: std::collections::HashSet<String> =
        todos.iter().map(|todo| todo.id.clone()).collect();
    known.extend(standing_ids.iter().cloned());
    for todo in todos {
        for blocked_id in &todo.blocked_by {
            if !known.contains(blocked_id) {
                return Err(ToolError::InvalidInput(format!(
                    "Todo '{}' is blocked by non-existent todo ID '{}'. \
                     Reference an entry this call carries or one the plan \
                     already holds.",
                    todo.id, blocked_id
                )));
            }
        }
    }
    Ok(())
}

/// Normalize the submission's status requests onto the rung law, then
/// merge the result into the stored list.
///
/// A structurally valid call never fails: the law is enforced by
/// adjustment, not refusal. Each standing entry advances at most one
/// rung per call (a `completed` request from pending lands as
/// `in_progress`; a `pending` request from completed reopens to
/// `in_progress` — the legal first rung of reopening), and a newborn id
/// filed `completed` onto a standing list lands `in_progress` — fresh
/// ids cannot skip rungs, though births on an empty store file as
/// submitted. Entries the submission omits stand unchanged — omission
/// is not removal, and no call removes anything. Every adjustment is
/// named in the returned notes, and the enforced entries return beside
/// them — the render's statuses, counts, and order answer to the
/// enforced list, never to the request, so the model reads back the
/// list the run actually holds.
fn normalize_and_store(
    todos: &Arc<std::sync::Mutex<Vec<TodoEntry>>>,
    mut entries: Vec<TodoEntry>,
) -> (Vec<String>, Vec<TodoEntry>) {
    let mut store = todos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let standing: std::collections::HashMap<String, TodoStatus> = store
        .iter()
        .map(|entry| (entry.id.clone(), entry.status))
        .collect();
    let mut notes = Vec::new();
    let submitted: std::collections::HashMap<String, TodoEntry> = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry.clone()))
        .collect();
    let mut merged: Vec<TodoEntry> = store
        .iter()
        .map(|standing| {
            submitted
                .get(standing.id.as_str())
                .cloned()
                .unwrap_or_else(|| standing.clone())
        })
        .collect();
    let standing_ids: std::collections::HashSet<&str> =
        store.iter().map(|entry| entry.id.as_str()).collect();
    merged.extend(
        entries
            .iter()
            .filter(|entry| !standing_ids.contains(entry.id.as_str()))
            .cloned(),
    );
    entries = merged;
    let store_was_empty = standing.is_empty();
    for entry in &mut entries {
        let Some(previous) = standing.get(entry.id.as_str()).copied() else {
            if !store_was_empty && entry.status == TodoStatus::Completed {
                notes.push(format!(
                    "'{}' filed completed onto a standing list — advanced to \
                     in progress (one rung per call; new ids do not skip it)",
                    entry.id
                ));
                entry.status = TodoStatus::InProgress;
            }
            continue;
        };
        if previous == entry.status {
            continue;
        }
        match (previous, entry.status) {
            (TodoStatus::Pending, TodoStatus::Completed) => {
                notes.push(format!(
                    "'{}' requested completed from pending — advanced to \
                     in progress (one rung per call — mark it in_progress \
                     when work begins, completed when work lands)",
                    entry.id
                ));
                entry.status = TodoStatus::InProgress;
            }
            (TodoStatus::Completed, TodoStatus::Pending) => {
                notes.push(format!(
                    "'{}' requested pending from completed — reopened to \
                     in progress (the first rung of reopening)",
                    entry.id
                ));
                entry.status = TodoStatus::InProgress;
            }
            _ => {}
        }
    }
    (*store).clone_from(&entries);
    (notes, entries)
}

/// Reject new entries the submission gives no identity.
///
/// A newborn id — one the standing store does not hold — must carry a
/// literal subject or a description to derive one from. The check is
/// the identity rule with teeth: a name fabricated from the id is no
/// identity at all, and every plan that accepted one has watched its
/// ids churn as the model guessed back at what its own entries meant.
/// Standing ids are exempt — a patch of id and status alone is the
/// taught shape, and the standing entry already has its name.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] naming the identity-free entry
/// and the exact repair shape.
fn reject_identity_free_entries(
    todos: &[TodoEntryInput],
    standing_ids: &[String],
) -> Result<(), ToolError> {
    for todo in todos {
        let named = todo
            .subject
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
            || !todo.description.trim().is_empty();
        if !named && !standing_ids.contains(&todo.id) {
            return Err(ToolError::InvalidInput(format!(
                "Todo '{}' has no subject and no description — a new entry \
                 needs one of them. Example: {{\"id\": \"{}\", \"subject\": \
                 \"Fix the bug\", \"status\": \"in_progress\"}} (entries already \
                 on the plan keep their subjects; send id and status alone \
                 to update them).",
                todo.id, todo.id
            )));
        }
    }
    Ok(())
}

/// Project the submission onto the stored entry shape, names first.
///
/// Drops the wire-only fields (`blocked_by`, `blocks`) and applies the
/// name-authority rule: a literal subject replaces the standing one —
/// an explicit reword — while a patch that omits it keeps the standing
/// subject, description, and active form untouched, so `{id, status}`
/// updates can never rename anything. Descriptions derive subjects
/// only for entries with no standing name; the identity gate
/// guarantees every newborn arrives with one or the other.
fn stored_entries(
    todos: &[TodoEntryInput],
    standing: &std::collections::HashMap<String, TodoEntry>,
) -> Vec<TodoEntry> {
    todos
        .iter()
        .map(|todo| {
            let base = standing.get(&todo.id);
            let literal = todo
                .subject
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let subject = literal
                .map(str::to_string)
                .or_else(|| base.map(|entry| entry.subject.clone()))
                .or_else(|| {
                    (!todo.description.trim().is_empty())
                        .then(|| derive_subject(&todo.description, &todo.id))
                })
                .unwrap_or_else(|| format!("Task {}", todo.id));
            let description = if todo.description.trim().is_empty() {
                base.map(|entry| entry.description.clone())
                    .unwrap_or_default()
            } else {
                todo.description.clone()
            };
            let active_form = todo
                .active_form
                .clone()
                .or_else(|| base.and_then(|entry| entry.active_form.clone()));
            TodoEntry {
                id: todo.id.clone(),
                subject,
                description,
                status: todo.status,
                active_form,
            }
        })
        .collect()
}

/// The summary-line indicator for a status.
///
/// Open circle for pending, half circle for in progress, filled circle for
/// completed — the render contract the status line prints.
fn status_icon(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "○",
        TodoStatus::InProgress => "◐",
        TodoStatus::Completed => "●",
    }
}

/// Sort position of a status: in-progress work leads, completed trails.
///
/// The render walks entries in this order so the rung the agent is
/// currently on is the first thing it reads back.
fn display_order(status: TodoStatus) -> u8 {
    match status {
        TodoStatus::InProgress => 0,
        TodoStatus::Pending => 1,
        TodoStatus::Completed => 2,
    }
}

/// How many entries in the submission carry the given status.
///
/// Feeds the summary line, which reports the pending, in-progress, and
/// completed counts of the submitted list.
fn count_in_status(todos: &[TodoEntryInput], status: TodoStatus) -> usize {
    todos.iter().filter(|todo| todo.status == status).count()
}

/// Append one entry block: the status line, then the optional description
/// and dependency lines, each only when the entry carries it.
fn push_entry(todo: &TodoEntryInput, output: &mut String) {
    writeln!(
        output,
        "{} [{}] {}",
        status_icon(todo.status),
        todo.id,
        todo.subject.as_deref().unwrap_or("")
    )
    .ok();
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

/// Render the summary the model reads back.
///
/// Emits the adjustment notice when the enforced list moved any
/// requested status (the law applied, not a refusal), the
/// subject-recovery notice when one is due, a status-count line, then
/// the entries sorted in progress → pending → completed so current
/// work leads the list.
fn render_task_list(parsed: TodoListInput, recovered: bool, adjustments: &[String]) -> String {
    let mut output = String::new();

    if !adjustments.is_empty() {
        writeln!(output, "Adjusted: {}\n", adjustments.join("; ")).ok();
    }

    if recovered {
        writeln!(
            output,
            "Auto-recovered: Some todo subjects were derived from their descriptions or ids.\n"
        )
        .ok();
    }

    writeln!(output, "# Task List\n").ok();
    write!(
        output,
        "Summary: {} pending, {} in progress, {} completed\n\n",
        count_in_status(&parsed.todos, TodoStatus::Pending),
        count_in_status(&parsed.todos, TodoStatus::InProgress),
        count_in_status(&parsed.todos, TodoStatus::Completed),
    )
    .ok();

    let mut sorted = parsed.todos;
    sorted.sort_by_key(|todo| display_order(todo.status));
    for todo in &sorted {
        push_entry(todo, &mut output);
    }
    output
}

/// Parse a rendered task list back into entries.
///
/// The inverse of the renderer: reads the entry lines the render prints
/// (`○ [id] subject`, `◐`, `●`) back into [`TodoEntry`] rows.
/// Description and active form do not survive the render and come back
/// empty. `None` when the output carries no task list (an error, a
/// standing-plan notice), so a caller falls back to whatever else it can
/// read. Hosts use this to recover the standing plan from a transcript's
/// last tool result.
#[must_use]
pub fn parse_task_list_render(output: &str) -> Option<Vec<TodoEntry>> {
    if !output.contains("# Task List") {
        return None;
    }
    let mut entries = Vec::new();
    for line in output.lines() {
        let (icon, rest) = if let Some(rest) = line.strip_prefix('○') {
            ('○', rest)
        } else if let Some(rest) = line.strip_prefix('◐') {
            ('◐', rest)
        } else if let Some(rest) = line.strip_prefix('●') {
            ('●', rest)
        } else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(" [") else {
            continue;
        };
        let Some((id, subject)) = rest.split_once("] ") else {
            continue;
        };
        let status = match icon {
            '●' => TodoStatus::Completed,
            '◐' => TodoStatus::InProgress,
            _ => TodoStatus::Pending,
        };
        entries.push(TodoEntry {
            id: id.to_string(),
            subject: subject.to_string(),
            description: String::new(),
            status,
            active_form: None,
        });
    }
    Some(entries)
}

impl TodoTool {
    /// Body of [`Tool::call`], separated so the trait method stays a
    /// one-liner.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when no runner context is installed,
    /// and [`ToolError::InvalidInput`] when the input cannot be parsed, an
    /// id is duplicated within the submission, a new entry carries neither
    /// a subject nor a description, or a `blocked_by` reference names an
    /// absent id. Status requests are never rejected — the store is
    /// normalize-and-merge: once those checks pass, each carried entry is
    /// adjusted to the rung law and merged into the standing list, every
    /// adjustment named in the result.
    fn todo_write_inner(
        mut input: Value,
        todos: Option<Arc<std::sync::Mutex<Vec<TodoEntry>>>>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(todos) = todos else {
            return Err(ToolError::Execution(
                "TodoWrite requires the runner context extension".to_string(),
            ));
        };

        preprocess_todo_input(&mut input);
        let parsed = parse_todo_input(input)?;
        let recovered = parsed.todos.iter().any(|todo| {
            let literal = todo
                .subject
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
            !literal && !todo.description.trim().is_empty()
        });

        if parsed.todos.is_empty() {
            // No removal exists — not even the empty call. An empty
            // submission over a standing plan is the escape hatch the
            // rung law kept meeting: wipe the list, dodge the walk.
            // It renders the standing plan unchanged with the law
            // named, so the model reads there is no way out but
            // through. The render carries the standing rows in the
            // same shape every other call returns — a resume seeder
            // reading the last result finds the standing plan here
            // exactly as it would after a patched call. The runner's
            // own per-run reset stands — that one is the host's, not
            // the model's.
            let entries = todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let undone = entries
                .iter()
                .filter(|entry| !matches!(entry.status, TodoStatus::Completed))
                .count();
            if entries.is_empty() {
                return Ok(ToolOutput::text(
                    "Empty plan — nothing stands, nothing to update.",
                ));
            }
            let plural = if undone == 1 { "" } else { "s" };
            let standing_rows = entries
                .iter()
                .map(|entry| TodoEntryInput {
                    id: entry.id.clone(),
                    subject: Some(entry.subject.clone()),
                    description: entry.description.clone(),
                    status: entry.status,
                    active_form: entry.active_form.clone(),
                    blocked_by: Vec::new(),
                    blocks: Vec::new(),
                })
                .collect();
            return Ok(ToolOutput::text(format!(
                "Adjusted: an empty list removes nothing — the plan only \
                 advances; {undone} item{plural} still open. Walk entries \
                 to completed with follow-up calls — the plan carries into \
                 the next run until every entry is completed.\n\n{}",
                render_task_list(
                    TodoListInput {
                        todos: standing_rows
                    },
                    false,
                    &[]
                )
            )));
        }

        reject_duplicate_ids(&parsed.todos)?;
        let (standing_ids, standing_by_id) = {
            let store = todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ids = store
                .iter()
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>();
            let by_id = store
                .iter()
                .cloned()
                .map(|entry| (entry.id.clone(), entry))
                .collect::<std::collections::HashMap<_, _>>();
            (ids, by_id)
        };
        reject_identity_free_entries(&parsed.todos, &standing_ids)?;
        reject_unknown_blocked_by(&parsed.todos, &standing_ids)?;
        let (adjustments, enforced) =
            normalize_and_store(&todos, stored_entries(&parsed.todos, &standing_by_id));
        let wire_by_id: std::collections::HashMap<String, (Vec<String>, Vec<String>)> = parsed
            .todos
            .iter()
            .map(|todo| {
                (
                    todo.id.clone(),
                    (todo.blocked_by.clone(), todo.blocks.clone()),
                )
            })
            .collect();
        let rendered_rows: Vec<TodoEntryInput> = enforced
            .iter()
            .map(|entry| {
                let (blocked_by, blocks) = wire_by_id
                    .get(entry.id.as_str())
                    .cloned()
                    .unwrap_or_default();
                TodoEntryInput {
                    id: entry.id.clone(),
                    subject: Some(entry.subject.clone()),
                    description: entry.description.clone(),
                    status: entry.status,
                    active_form: entry.active_form.clone(),
                    blocked_by,
                    blocks,
                }
            })
            .collect();

        Ok(ToolOutput::text(render_task_list(
            TodoListInput {
                todos: rendered_rows,
            },
            recovered,
            &adjustments,
        )))
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }

    fn description(&self) -> &'static str {
        "Track progress with a structured task list. Send only the entries \
         you are changing or adding — omitted entries stand unchanged, and \
         entries are never removed. Mark an entry in_progress the moment \
         you begin its work and completed the moment its work lands; never \
         batch plan updates to the end of the run — a completed request \
         for a pending entry lands in_progress (one rung per call, the \
         adjustment named in the result), and reopening completed work \
         goes through in_progress. New entries need a subject (or a \
         description to derive one from); entries already on the plan \
         keep theirs — id and status alone update them. The plan belongs \
         to the session: a run that ends with unfinished entries carries \
         them into the next run. The rendered list this tool returns is \
         the authoritative state; a finished run leaves every entry \
         completed."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The entries to update or add; entries omitted from this call stand unchanged",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "Stable identifier for the todo; entries keep their ids across calls and runs" },
                                "subject": { "type": "string", "description": "Short subject/title (imperative form). Required for new entries; entries already on the plan keep theirs — send id and status alone to update them" },
                                "description": { "type": "string", "description": "Detailed description" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Lifecycle status (pending → in_progress → completed; reopen via in_progress). Mark in_progress the moment work begins and completed the moment it lands. A completed request for a pending entry lands in_progress (one rung per call); adjustments are named in the rendered result.",
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
        )
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

    fn system_prompt(&self) -> Option<String> {
        Some(
            "For multi-step work, keep the todo list current at the moments \
             of work: mark an entry in_progress the moment you begin its \
             work and completed the moment its work lands — never batch \
             updates to the end of the run. Send only the entries that \
             changed; omitted entries stand unchanged, entries are never \
             removed, and a completed request from pending lands \
             in_progress first (one rung per call). New entries need a \
             subject; standing entries keep theirs. The rendered list the \
             tool returns is the authoritative state; it survives context \
             compaction and carries into the session's next run when \
             entries remain unfinished. A finished run leaves every entry \
             completed."
                .to_string(),
        )
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
            builtin_registry(
                &loopctl::tool::builtin::fs::FileSession::new(std::path::PathBuf::from(".")),
                crate::registry::ReadWindows::default()
            )
            .get("TodoWrite")
            .is_some(),
            "TodoWrite must ship in the builtin registry"
        );
    }

    #[test]
    fn system_prompt_present() {
        let prompt = TodoTool.system_prompt();
        assert!(prompt.is_some());
        assert!(prompt.unwrap().contains("todo list"));
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
    async fn a_new_call_updates_and_adds_without_losing_the_standing_plan() {
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
        assert_eq!(store.len(), 2, "the standing entry stays on the plan");
        assert_eq!(store.first().unwrap().id, "old");
        assert_eq!(
            store.get(1).unwrap().id,
            "new",
            "the newborn joins at the end"
        );
    }

    #[tokio::test]
    async fn an_empty_call_removes_nothing_from_a_standing_plan() {
        // The last escape hatch, closed: wiping the list to dodge the
        // rung walk removed the plan's memory in one call, and the
        // run's own bookkeeping lost everything it was holding the
        // model to. An empty submission renders the standing plan
        // unchanged with the law named — the only way out is through.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let out = TodoTool.call(json!({"todos": []}), &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("an empty list removes nothing"),
            "the law is named in the result: {}",
            out.text_content()
        );
        assert!(
            out.text_content().contains("1 item still open"),
            "the standing count names what stays: {}",
            out.text_content()
        );
        let store = todos.lock().unwrap();
        assert_eq!(store.len(), 1, "the standing plan is untouched");
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
            "descriptions omitted on the wire are stored empty"
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
        // Seeded store-side: three entries already in progress, so the
        // first submission must drain two of them to hold the
        // one-in-progress bound.
        seed(&todos, "c", TodoStatus::Completed);
        seed(&todos, "p", TodoStatus::Pending);
        seed(&todos, "f", TodoStatus::InProgress);
        seed(&todos, "b", TodoStatus::InProgress);
        seed(&todos, "s", TodoStatus::InProgress);
        let first = TodoTool
            .call(
                json!({"todos": [
                    {"id": "c", "subject": "Seed c", "status": "completed"},
                    {"id": "p", "subject": "Seed p", "status": "pending"},
                    {"id": "f", "subject": "Seed f", "status": "completed"},
                    {"id": "b", "subject": "Seed b", "status": "pending"},
                    {"id": "s", "subject": "Seed s", "status": "in_progress"},
                    {"id": "n", "subject": "Seed n", "status": "completed"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!first.is_error, "{}", first.text_content());
        let second = TodoTool
            .call(
                json!({"todos": [
                    {"id": "c", "subject": "Seed c", "status": "in_progress"},
                    {"id": "s", "subject": "Seed s", "status": "completed"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!second.is_error, "{}", second.text_content());
        let third = TodoTool
            .call(
                json!({"todos": [
                    {"id": "c", "subject": "Seed c", "status": "completed"},
                    {"id": "p", "subject": "Seed p", "status": "in_progress"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!third.is_error, "{}", third.text_content());
        // The legal edges across three calls: completed → in progress,
        // pending → in progress, in progress → completed, in progress →
        // pending, and the same-status resend that is the model's every-turn
        // case. New ids on a standing list take the rung law; several
        // entries may run at once.
    }

    #[tokio::test]
    async fn completed_to_pending_reopens_one_rung() {
        // The law enforced by adjustment: completed work asked to go
        // pending reopens to in progress — the legal first rung — and
        // the call succeeds, with the adjustment named beside the
        // rendered truth.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Completed);
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "1", "subject": "Seed 1", "status": "pending"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("reopened to in progress"),
            "the adjustment is named: {}",
            out.text_content()
        );
        assert_eq!(
            todos.lock().unwrap().first().unwrap().status,
            TodoStatus::InProgress,
            "the stored list holds the enforced rung"
        );
    }

    #[tokio::test]
    async fn a_dependency_on_a_standing_omitted_entry_is_accepted() {
        // The taught call shape: carry the update, omit what stands.
        // A dependency naming the standing entry is valid — the plan
        // holds it — while an id neither the submission nor the store
        // carries stays refused.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "2", "subject": "Depends on 1",
                                  "status": "pending",
                                  "blocked_by": ["1"]}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("Blocked by: 1"),
            "the standing dependency renders beside its entry: {}",
            out.text_content()
        );
        let err = TodoTool
            .call(
                json!({"todos": [{"id": "3", "subject": "Dangling",
                                  "status": "pending",
                                  "blocked_by": ["ghost"]}]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("non-existent todo ID 'ghost'"),
            "an id nothing holds stays refused: {err}"
        );
    }

    #[tokio::test]
    async fn two_in_progress_entries_store_as_submitted() {
        // Several entries may run at once — work on one often cannot
        // finish without another — so both in-progress requests land
        // in progress, with no adjustment notes.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::InProgress);
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "Seed 1", "status": "in_progress"},
                    {"id": "2", "subject": "Rival work", "status": "in_progress"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            !out.text_content().contains("held"),
            "nothing is held — several entries may run at once: {}",
            out.text_content()
        );
        let store = todos.lock().unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(
            store.first().unwrap().status,
            TodoStatus::InProgress,
            "the standing runner keeps its rung"
        );
        assert_eq!(
            store.get(1).unwrap().status,
            TodoStatus::InProgress,
            "the second runner files as submitted"
        );
    }

    #[tokio::test]
    async fn an_omitted_standing_runner_keeps_its_rung() {
        // The ordinary add-task call: the submission names no in-progress
        // entry, and the standing runner it omits must stand exactly as
        // it was — omitted entries are never adjusted.
        let (ctx, todos) = todo_context();
        seed(&todos, "a", TodoStatus::InProgress);
        seed(&todos, "b", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "c", "subject": "Newcomer", "status": "pending"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            !out.text_content().contains("demoted"),
            "an omitted entry is never demoted: {}",
            out.text_content()
        );
        let store = todos.lock().unwrap();
        assert_eq!(
            store.first().unwrap().status,
            TodoStatus::InProgress,
            "the omitted standing runner keeps its rung"
        );
        assert_eq!(
            store.get(1).unwrap().status,
            TodoStatus::Pending,
            "the omitted pending entry stands unchanged"
        );
        assert_eq!(
            store.get(2).map(|entry| entry.id.as_str()),
            Some("c"),
            "the newborn joins at the end"
        );
    }

    #[tokio::test]
    async fn pending_to_completed_advances_one_rung() {
        // The stumble every observed model made — the work is done,
        // mark it done — now succeeds as far as the law allows: the
        // entry lands in progress, the adjustment is named, and the
        // follow-up call completes it.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "1", "subject": "Seed 1", "status": "completed"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content()
                .contains("'1' requested completed from pending — advanced"),
            "the adjustment is named: {}",
            out.text_content()
        );
        assert_eq!(
            todos.lock().unwrap().first().unwrap().status,
            TodoStatus::InProgress,
            "one rung advanced, one rung kept for the follow-up call"
        );
        let done = TodoTool
            .call(
                json!({"todos": [{"id": "1", "subject": "Seed 1", "status": "completed"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!done.is_error, "{}", done.text_content());
        assert_eq!(
            todos.lock().unwrap().first().unwrap().status,
            TodoStatus::Completed,
            "the follow-up call walks the last rung"
        );
    }

    #[tokio::test]
    async fn an_omitted_entry_renders_in_its_enforced_rung() {
        // The taught norm's own shape: a call carries one standing
        // entry and omits the other. The merged list is store-ordered
        // ([1, 2]) while the submission names only 2 — a positional
        // projection would pair 2 with entry 1's status and render a
        // lying summary. By id: 2 shows in progress, and the render
        // carries both standing entries.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        seed(&todos, "2", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "2", "subject": "Seed 2", "status": "in_progress"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content()
                .contains("Summary: 1 pending, 1 in progress, 0 completed"),
            "the summary counts the enforced store, omission included: {}",
            out.text_content()
        );
        assert!(
            out.text_content().contains("◐ [2]"),
            "the carried entry renders in its enforced rung: {}",
            out.text_content()
        );
        assert!(
            out.text_content().contains("○ [1]"),
            "the omitted entry renders standing, not gone: {}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn a_reordered_carry_renders_each_entry_in_its_own_rung() {
        // Submission order is not store order: a carry that names 2
        // before 1 must not swap their rendered statuses — each entry
        // reads its own enforced rung, joined by id.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        seed(&todos, "2", TodoStatus::InProgress);
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "2", "subject": "Seed 2", "status": "completed"},
                    {"id": "1", "subject": "Seed 1", "status": "pending"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("● [2]"),
            "2 completes from in progress — one legal rung: {}",
            out.text_content()
        );
        assert!(
            out.text_content().contains("○ [1]"),
            "1 stands pending, named in its own right: {}",
            out.text_content()
        );
        assert!(
            out.text_content()
                .contains("Summary: 1 pending, 0 in progress, 1 completed"),
            "the summary counts each entry where the store holds it: {}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn an_omitted_entry_stands_unchanged() {
        // The two verbs, no third: a call updates the entries it
        // carries and adds the ones it names; anything it omits
        // stands exactly as it stood. The model cannot lose plan
        // history by forgetting to carry an entry — and the rendered
        // list hands the standing entries right back.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        seed(&todos, "2", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "1", "subject": "Seed 1", "status": "in_progress"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let (len, second) = {
            let store = todos.lock().unwrap();
            (store.len(), store.get(1).map(|entry| entry.status))
        };
        assert_eq!(len, 2, "the omitted entry is still on the plan");
        assert_eq!(
            second,
            Some(TodoStatus::Pending),
            "it stands unchanged, exactly as it stood"
        );
        // The one removal: the explicit empty-list clear, visible in
        // the result.
        let wiped = TodoTool.call(json!({"todos": []}), &ctx).await.unwrap();
        assert!(
            wiped.text_content().contains("removes nothing"),
            "the empty call is the same no-removal law: {}",
            wiped.text_content()
        );
        assert_eq!(
            todos.lock().unwrap().len(),
            2,
            "nothing left the plan — not even by an empty call"
        );
    }

    #[tokio::test]
    async fn a_fresh_list_over_a_standing_plan_appends_and_walks_the_rungs() {
        // A submission of all-new names over a standing plan is now an
        // addition, not a replacement: the standing entries stand, the
        // fresh ones join them — and a fresh id filed completed still
        // takes the rung law, named in the result.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "a", "subject": "Fresh a", "status": "completed"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content()
                .contains("'a' filed completed onto a standing list"),
            "the fresh-id skip is named: {}",
            out.text_content()
        );
        let store = todos.lock().unwrap();
        let statuses: Vec<TodoStatus> = store.iter().map(|entry| entry.status).collect();
        assert_eq!(
            statuses,
            vec![TodoStatus::Pending, TodoStatus::InProgress],
            "the standing entry stands in its place, the fresh one \
             joins advanced one rung"
        );
    }

    #[tokio::test]
    async fn a_first_filing_on_an_empty_plan_files_as_submitted() {
        // The honest shapes keep their shortcut: a run's first filing,
        // a re-file after resume, the small task already done — the
        // store is empty, so the filing is an assertion, not a
        // transition, and no rung is skipped.
        let (ctx, todos) = todo_context();
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "done", "subject": "Already did it", "status": "completed"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            todos.lock().unwrap().first().unwrap().status,
            TodoStatus::Completed,
            "births on an empty store file as submitted"
        );
    }

    #[tokio::test]
    async fn a_batch_complete_request_advances_each_standing_item_one_rung() {
        // The live-session shape: one item done, one running, one
        // waiting, and the model sends everything completed. The call
        // succeeds — the legal edge completes the runner, the skip
        // lands the waiter in progress — and the rendered truth shows
        // the model exactly what remains.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Completed);
        seed(&todos, "2", TodoStatus::InProgress);
        seed(&todos, "3", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "Seed 1", "status": "completed"},
                    {"id": "2", "subject": "Seed 2", "status": "completed"},
                    {"id": "3", "subject": "Seed 3", "status": "completed"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content()
                .contains("'3' requested completed from pending"),
            "the skip is named: {}",
            out.text_content()
        );
        assert!(
            out.text_content()
                .contains("Summary: 0 pending, 1 in progress, 2 completed"),
            "the summary counts the enforced list, not the request: {}",
            out.text_content()
        );
        assert!(
            out.text_content().contains("◐ [3]"),
            "the waiter's icon shows the rung it landed on: {}",
            out.text_content()
        );
        let store = todos.lock().unwrap();
        let statuses: Vec<TodoStatus> = store.iter().map(|entry| entry.status).collect();
        assert_eq!(
            statuses,
            vec![
                TodoStatus::Completed,
                TodoStatus::Completed,
                TodoStatus::InProgress
            ],
            "the legal edge completes, the skip advances one rung"
        );
    }

    #[tokio::test]
    async fn todo_write_teaches_the_moment_rule_on_every_surface() {
        let description = TodoTool.description();
        assert!(
            description.contains("omitted entries stand unchanged"),
            "tool description must state the patch semantics: {description}"
        );
        assert!(
            description.contains("the moment you begin its work"),
            "tool description must state the moment rule: {description}"
        );
        assert!(
            description.contains("never batch plan updates to the end of the run"),
            "tool description must forbid deferred updates: {description}"
        );
        assert!(
            description.contains("carries them into the next"),
            "tool description must state the session carry: {description}"
        );
        assert!(
            description.contains("New entries need a subject"),
            "tool description must state the identity rule: {description}"
        );
        assert!(
            description.contains("id and status alone update them"),
            "tool description must teach the minimal patch: {description}"
        );
        assert!(
            !description.contains("remove the"),
            "no removal instruction on any surface — the law is stated, \
             never offered: {description}"
        );
        let fragment = TodoTool.system_prompt().expect("fragment present");
        assert!(
            fragment.contains("the moment you begin its"),
            "the system-prompt fragment must carry the moment rule: {fragment}"
        );
        assert!(
            fragment.contains("authoritative state"),
            "fragment must state where the truth lives: {fragment}"
        );
        assert!(
            fragment.contains("never batch"),
            "fragment must forbid deferred updates: {fragment}"
        );
        assert!(
            fragment.contains("carries into the session's next run"),
            "fragment must state the carry: {fragment}"
        );
        assert!(
            !fragment.contains("remove the"),
            "no removal instruction in the fragment: {fragment}"
        );
        let schema = serde_json::to_string(&TodoTool.schema()).expect("schema serializes");
        assert!(
            schema.contains("stand unchanged"),
            "schema must state the patch semantics: {schema}"
        );
        assert!(
            schema.contains("the moment work begins"),
            "schema status description must state the moment rule: {schema}"
        );
        assert!(
            !schema.contains("at most one entry runs"),
            "the one-runner bound is gone from the schema: {schema}"
        );
        // The empty call's own read-back is a teaching surface too:
        // the wipe attempt is the moment the model most needs the
        // carry law, and the notice once said the opposite.
        let (ctx, todos) = todo_context();
        seed(&todos, "1", TodoStatus::Pending);
        let out = TodoTool
            .call(json!({"todos": []}), &ctx)
            .await
            .expect("the empty call succeeds");
        let notice = out.text_content();
        assert!(
            notice.contains("an empty list removes nothing"),
            "the empty call names the law it applied: {notice}"
        );
        assert!(
            notice.contains("the plan carries into the next run"),
            "the empty call teaches the carry, not a reset: {notice}"
        );
        assert!(
            !notice.contains("resets between runs"),
            "the reset claim contradicts the carry law every other \
             surface teaches: {notice}"
        );
        let standing = parse_task_list_render(&notice).expect("the empty call renders rows");
        assert!(
            standing
                .iter()
                .any(|entry| entry.id == "1" && matches!(entry.status, TodoStatus::Pending)),
            "the standing plan's own rows ride the notice — a resume \
             seeder reads the plan from the empty call's result: {notice}"
        );
    }

    #[tokio::test]
    async fn parse_task_list_render_round_trips_the_renderer_output() {
        let (ctx, _todos) = todo_context();
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "Done task", "status": "completed"},
                    {"id": "2", "subject": "Current task", "status": "in_progress"},
                    {"id": "3", "subject": "Waiting task"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        let parsed =
            parse_task_list_render(&out.text_content()).expect("the tool's own render parses back");
        let by_id: Vec<(&str, TodoStatus)> = parsed
            .iter()
            .map(|entry| (entry.id.as_str(), entry.status))
            .collect();
        assert_eq!(
            by_id,
            vec![
                ("2", TodoStatus::InProgress),
                ("3", TodoStatus::Pending),
                ("1", TodoStatus::Completed),
            ],
            "statuses round-trip; the render's sorted order is preserved"
        );
        assert!(
            parsed.iter().any(|entry| entry.subject == "Current task"),
            "subjects round-trip"
        );
        assert!(
            parse_task_list_render("Empty plan — nothing stands.").is_none(),
            "output without a task list parses to None"
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
    async fn a_new_entry_without_identity_is_rejected_with_a_teaching_hint() {
        // The identity rule with teeth: a newborn id carrying neither a
        // subject nor a description is refused — a name fabricated from
        // the id is no identity, and the plans that accepted one watched
        // their ids churn as the model guessed at its own entries. The
        // error names the entry and the exact repair shape.
        let (ctx, todos) = todo_context();
        let err = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": "Real work"},
                    {"id": "2", "status": "pending"}
                ]}),
                &ctx,
            )
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("'2'"), "the entry is named: {message}");
        assert!(
            message.contains("no subject and no description"),
            "the rule is stated: {message}"
        );
        assert!(
            message.contains("Example"),
            "the repair shape rides: {message}"
        );
        assert!(
            message.contains("keep their subjects"),
            "the patch shape is taught beside the rule: {message}"
        );
        assert!(
            todos.lock().unwrap().is_empty(),
            "a rejected call touches nothing"
        );
    }

    #[tokio::test]
    async fn a_subjectless_update_of_a_standing_entry_keeps_its_name() {
        // The name-authority rule: a patch of id and status alone is the
        // taught shape, and it can never rename — the standing subject,
        // description, and active form all stand untouched while the
        // status walks.
        let (ctx, todos) = todo_context();
        todos.lock().unwrap().push(TodoEntry {
            id: "7".to_string(),
            subject: "Fix the parser".to_string(),
            description: "the parser details".to_string(),
            status: TodoStatus::InProgress,
            active_form: Some("Fixing the parser".to_string()),
        });
        let out = TodoTool
            .call(json!({"todos": [{"id": "7", "status": "completed"}]}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            !out.text_content().contains("Auto-recovered"),
            "nothing was derived — the standing name stands: {}",
            out.text_content()
        );
        let store = todos.lock().unwrap();
        let entry = store.first().unwrap();
        assert_eq!(entry.status, TodoStatus::Completed, "the status walked");
        assert_eq!(entry.subject, "Fix the parser", "the name stands");
        assert_eq!(entry.description, "the parser details", "the detail stands");
        assert_eq!(
            entry.active_form.as_deref(),
            Some("Fixing the parser"),
            "the active form stands"
        );
    }

    #[tokio::test]
    async fn an_explicit_subject_rewords_a_standing_entry() {
        // The model's reword is its own move: a literal subject replaces
        // the standing one, named nowhere because nothing was adjusted.
        let (ctx, todos) = todo_context();
        seed(&todos, "7", TodoStatus::Pending);
        let out = TodoTool
            .call(
                json!({"todos": [{"id": "7", "subject": "Repair the parser",
                                  "status": "pending"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            todos.lock().unwrap().first().unwrap().subject,
            "Repair the parser",
            "an explicit subject is the model's reword"
        );
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
    async fn a_standing_entry_patch_needs_no_subject_but_a_new_one_does() {
        // The two halves of identity in one shape: the standing entry's
        // patch (id and status alone) is the taught minimal update, while
        // the newborn beside it must carry its own name — the call is
        // refused for the newborn alone.
        let (ctx, todos) = todo_context();
        seed(&todos, "7", TodoStatus::Pending);
        let err = TodoTool
            .call(
                json!({"todos": [
                    {"id": "7", "status": "in_progress"},
                    {"id": "8", "status": "pending"}
                ]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("'8'"),
            "the newborn is named, not the patch: {err}"
        );
        assert_eq!(
            todos.lock().unwrap().len(),
            1,
            "a rejected call touches nothing"
        );
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

    #[test]
    fn derived_subjects_truncate_long_first_lines() {
        let long = "x".repeat(60);
        let subject = derive_subject(&long, "1");
        assert_eq!(subject.chars().count(), 50, "{subject}");
        assert!(subject.ends_with("..."), "{subject}");

        let exact = "y".repeat(50);
        assert_eq!(derive_subject(&exact, "1"), exact);
    }

    #[test]
    fn a_blank_leading_line_derives_the_first_content_line() {
        assert_eq!(
            derive_subject("\nActual plan here", "7"),
            "Actual plan here"
        );
    }

    #[test]
    fn whitespace_only_description_falls_back_to_the_id() {
        assert_eq!(derive_subject("   ", "7"), "Task 7");
    }

    /// A parse failure for the given JSON, for feeding [`parse_hint`]
    /// directly.
    fn parse_error(json: &str) -> serde_json::Error {
        serde_json::from_str::<TodoListInput>(json).unwrap_err()
    }

    /// A typed submission parsed from the wire shape, for driving the
    /// validation and projection helpers as units.
    fn parse_input(json: Value) -> TodoListInput {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn parse_hint_names_the_missing_todos_key() {
        let hint = parse_hint(&parse_error("{}"));
        assert!(hint.contains("'todos' key"), "{hint}");
    }

    #[test]
    fn parse_hint_names_any_other_missing_field() {
        let hint = parse_hint(&parse_error(r#"{"todos": [{"subject": "x"}]}"#));
        assert!(hint.contains("'id' is required"), "{hint}");
    }

    #[test]
    fn parse_hint_names_the_wrong_container_type() {
        let hint = parse_hint(&parse_error(r#"{"todos": "nope"}"#));
        assert!(hint.contains("array of objects"), "{hint}");
    }

    #[test]
    fn parse_hint_stays_silent_for_value_rejections() {
        // The blank-id rejection carries none of the keywords the hint
        // selection matches on, so no hint is better than a wrong one.
        let hint = parse_hint(&parse_error(r#"{"todos": [{"id": " ", "subject": "x"}]}"#));
        assert_eq!(hint, "");
    }

    #[test]
    fn duplicate_id_check_passes_distinct_ids() {
        let parsed = parse_input(json!({"todos": [
            {"id": "1", "subject": "First"},
            {"id": "2", "subject": "Second"}
        ]}));
        assert!(reject_duplicate_ids(&parsed.todos).is_ok());
    }

    #[test]
    fn blocked_by_check_passes_declared_ids() {
        let parsed = parse_input(json!({"todos": [
            {"id": "1", "subject": "First", "blocked_by": ["2"]},
            {"id": "2", "subject": "Second"}
        ]}));
        assert!(reject_unknown_blocked_by(&parsed.todos, &[]).is_ok());
    }

    #[test]
    fn stored_entries_copy_every_stored_field() {
        let parsed = parse_input(json!({"todos": [
            {"id": "1", "subject": "Ship", "description": "Wire it",
             "status": "in_progress", "active_form": "Shipping"}
        ]}));
        let entries = stored_entries(&parsed.todos, &std::collections::HashMap::new());
        let entry = entries.first().unwrap();
        assert_eq!(entry.id, "1");
        assert_eq!(entry.subject, "Ship");
        assert_eq!(entry.description, "Wire it");
        assert_eq!(entry.status, TodoStatus::InProgress);
        assert_eq!(entry.active_form.as_deref(), Some("Shipping"));
    }

    #[test]
    fn normalize_and_store_replaces_the_list_and_names_nothing_when_legal() {
        let todos: Arc<std::sync::Mutex<Vec<TodoEntry>>> = Arc::default();
        seed(&todos, "1", TodoStatus::Pending);
        let (notes, enforced) =
            normalize_and_store(&todos, vec![entry(TodoStatus::InProgress, None)]);
        assert!(
            notes.is_empty(),
            "a legal advance needs no adjustment: {notes:?}"
        );
        assert_eq!(
            enforced.first().map(|entry| entry.status),
            Some(TodoStatus::InProgress),
            "the enforced entries return for the render to answer to"
        );
        let store = todos.lock().unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.first().unwrap().status, TodoStatus::InProgress);
    }

    #[test]
    fn status_icon_marks_each_status() {
        assert_eq!(status_icon(TodoStatus::Pending), "○");
        assert_eq!(status_icon(TodoStatus::InProgress), "◐");
        assert_eq!(status_icon(TodoStatus::Completed), "●");
    }

    #[test]
    fn display_order_puts_active_work_first_and_done_last() {
        assert!(display_order(TodoStatus::InProgress) < display_order(TodoStatus::Pending));
        assert!(display_order(TodoStatus::Pending) < display_order(TodoStatus::Completed));
    }

    #[test]
    fn count_in_status_counts_only_the_matching_status() {
        let parsed = parse_input(json!({"todos": [
            {"id": "1", "subject": "First", "status": "pending"},
            {"id": "2", "subject": "Second", "status": "pending"},
            {"id": "3", "subject": "Third", "status": "completed"}
        ]}));
        assert_eq!(count_in_status(&parsed.todos, TodoStatus::Pending), 2);
        assert_eq!(count_in_status(&parsed.todos, TodoStatus::InProgress), 0);
    }

    #[test]
    fn push_entry_prints_only_the_lines_the_entry_carries() {
        let full = parse_input(json!({"todos": [
            {"id": "2", "subject": "Current", "status": "in_progress",
             "description": "Details", "blocked_by": ["1"], "blocks": ["3"]}
        ]}));
        let mut out = String::new();
        push_entry(full.todos.first().unwrap(), &mut out);
        assert!(out.contains("◐ [2] Current\n"), "{out}");
        assert!(out.contains("  Details\n"), "{out}");
        assert!(out.contains("  Blocked by: 1\n"), "{out}");
        assert!(out.contains("  Blocks: 3\n"), "{out}");

        let bare = parse_input(json!({"todos": [{"id": "1", "subject": "Bare"}]}));
        let mut out = String::new();
        push_entry(bare.todos.first().unwrap(), &mut out);
        assert_eq!(out, "○ [1] Bare\n\n", "{out}");
    }

    #[test]
    fn render_task_list_assembles_notice_counts_and_sorted_entries() {
        let parsed = parse_input(json!({"todos": [
            {"id": "1", "subject": "Done", "status": "completed"},
            {"id": "2", "subject": "Now", "status": "in_progress"}
        ]}));
        let rendered = render_task_list(parsed, true, &[]);
        assert!(
            rendered.find("Auto-recovered:").unwrap() < rendered.find("# Task List").unwrap(),
            "{rendered}"
        );
        assert!(
            rendered.contains("Summary: 0 pending, 1 in progress, 1 completed"),
            "{rendered}"
        );
        assert!(
            rendered.find('◐').unwrap() < rendered.find('●').unwrap(),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn recovery_survives_a_blank_first_description_line() {
        // Derivation reads the first non-blank line: leading air in the
        // description is formatting, not meaning.
        let (ctx, todos) = todo_context();
        let out = TodoTool
            .call(
                json!({"todos": [
                    {"id": "1", "subject": null, "description": "\nActual plan"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            todos.lock().unwrap().first().unwrap().subject,
            "Actual plan",
            "the first content line carries the name"
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
