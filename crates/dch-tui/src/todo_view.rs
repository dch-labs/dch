//! Todo-list rendering for the conversation and the plan section.
//!
//! The `TodoWrite` tool's output is prose the model reads back; the
//! display's question is different — at a glance, which tasks stand,
//! which one runs, which finished with this update. This module reads
//! that rendered output back — the authoritative list, since the
//! enforced read-back names every entry the store holds, omission
//! included — with the call's submission JSON as the fallback when no
//! render survives, and renders checkbox rows:
//! `☑` completed, `◐` in progress, `☐` pending — pending rows plain,
//! completed rows dim with the strike on the words alone, moved rows
//! still standing in the moved register, so a scrolling reader sees
//! what moved without reading it as the rung in progress.

use std::collections::HashMap;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The title the plan section opens with.
///
/// Rendered as a badge — the waiting section's pill — over the
/// secondary accent rather than the primary, so the two panels read
/// as one family of chrome with two names. In a theme that defers
/// to the terminal the fill resets and the word degrades to plain
/// text, the same concession the queue's badge makes.
const TODO_TITLE: &str = "TODO";

/// One todo entry as the display reads it.
///
/// The parsed shape of a single item from the tool's submission:
/// stable id, the task's wording, and where it stands — everything
/// the checkbox rows and the diff accents are computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TodoRow {
    /// The stable id the tool assigns.
    ///
    /// The join key between snapshots: a status change is recognized
    /// by the same id carrying a different status, never by wording.
    pub id: String,

    /// The task's own wording.
    ///
    /// Rendered verbatim after the checkbox; long subjects wrap
    /// under their icon rather than truncating, because the wording
    /// is the task.
    pub subject: String,

    /// Where the entry stands.
    ///
    /// Parsed leniently — a missing or unrecognized spelling reads
    /// as pending, so a hand-written submission never drops a task.
    pub status: TodoStatus,
}

/// A todo entry's lifecycle status.
///
/// The three states the tool's checkbox rows carry; the icon and
/// the row's styling are chosen from it, and the diff accents are
/// the transitions between its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoStatus {
    /// Not started.
    ///
    /// The default a missing or unrecognized status reads as — the
    /// tool's own contract keeps an unspelled task on the list.
    Pending,

    /// The rung the agent is on.
    ///
    /// One entry at most per list in practice; the section accents
    /// it as the thing happening now.
    InProgress,

    /// Done.
    ///
    /// Rendered dimmed and struck through — present for the count,
    /// visually retired from the work.
    Completed,
}

impl TodoStatus {
    /// The status a submission names, defaulting to pending.
    ///
    /// The tool's own contract: a missing or unrecognized status
    /// reads as pending, so a hand-written submission never drops a
    /// task for spelling.
    fn from_name(name: &str) -> Self {
        match name {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => Self::Pending,
        }
    }
}

/// The subject an entry without one is read under.
///
/// The display's lenient twin of the tool's derivation: first non-blank
/// line of the description, trimmed, cut at fifty characters with an
/// ellipsis, the id when the description has nothing to say. The tool
/// itself now refuses entries with neither a subject nor a description,
/// so this fallback serves the transcripts recorded before that gate —
/// a replayed list the run accepted in an older build still renders
/// rows rather than no rows at all.
fn derived_subject(description: &str, id: &str) -> String {
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

/// One raw entry inside the submission JSON, as the tool spells it.
///
/// Field-for-field the tool's own shape, so the parse accepts
/// exactly what a real submission carries and nothing the display
/// invented. `subject` arrives optional the way the tool accepts
/// it: the schema asks for it, but the run repairs its absence from
/// the entry's `description`, and the display reads the same
/// recovery (see [`derived_subject`]) so a description-only list
/// renders the rows the run acted on rather than no rows at all;
/// the status stays a string because lenience is
/// [`TodoStatus::from_name`]'s job, not serde's. The tool's
/// remaining fields (`active_form`, `owner`, `blocked_by`,
/// `blocks`) ride along unheard: serde ignores them, the display
/// has no use for them, and rejecting them would reject the tool's
/// own schema.
#[derive(serde::Deserialize)]
struct TodoEntryInput {
    /// The stable id the tool assigns the entry.
    ///
    /// Carried straight through to [`TodoRow::id`]: the id is the
    /// join key between snapshots, so the display invents nothing
    /// here and refuses nothing — a submission without one fails the
    /// parse and renders as an ordinary tool block.
    id: String,

    /// The task's short wording, when the tool sent one.
    ///
    /// Optional the way the tool accepts it: the schema asks for it,
    /// the run repairs its absence, and the display reads the same
    /// recovery through [`derived_subject`].
    #[serde(default)]
    subject: String,

    /// The task's detailed wording, when the tool sent one.
    ///
    /// The source a missing subject is derived from — never shown
    /// directly, because the run read the derived form and the
    /// display shows the list the run acted on.
    #[serde(default)]
    description: String,

    /// The lifecycle status as the tool spelled it.
    ///
    /// A raw string, not an enum: lenience is
    /// [`TodoStatus::from_name`]'s job, and serde rejecting an
    /// unknown spelling would drop a task for wording the tool
    /// itself tolerates.
    #[serde(default)]
    status: String,
}

/// The submission's outer shape.
///
/// The whole list under one `todos` key — the tool always sends
/// the complete list, so one parse is the current state and no
/// merging of partial updates exists to get wrong.
#[derive(serde::Deserialize)]
struct TodoSubmission {
    /// The whole list, as the tool always sends it.
    ///
    /// Complete by contract — every call replaces the list, so the
    /// newest array is the current plan and older parses are
    /// snapshots, never patches to merge.
    todos: Vec<TodoEntryInput>,
}

/// Normalize a submission to the tool's accepted shape, in place.
///
/// The two repairs `TodoWrite` performs before its own parse, mirrored
/// here so the display reads exactly the list the run acted on: a
/// `todos` array that arrived double-encoded as a JSON string is
/// decoded to the array it names, and each entry whose subject is
/// absent, null, or blank has the derived subject written in — the
/// same derivation the entry's row would fall back to, applied
/// before the typed parse so the null spelling no longer fails it.
fn normalize_submission(value: &mut serde_json::Value) {
    if let Some(todos) = value.get_mut("todos")
        && let Some(encoded) = todos.as_str()
        && let Ok(decoded) = serde_json::from_str::<serde_json::Value>(encoded)
    {
        *todos = decoded;
    }
    let Some(todos) = value
        .get_mut("todos")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for entry in todos {
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        let subject_needs_deriving = object.get("subject").is_none_or(|subject| {
            subject.is_null()
                || subject
                    .as_str()
                    .is_none_or(|subject| subject.trim().is_empty())
        });
        if subject_needs_deriving {
            let description = object
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let id = object
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            object.insert(
                "subject".to_string(),
                serde_json::Value::String(derived_subject(description, id)),
            );
        }
    }
}

/// Parse a `TodoWrite` submission's input JSON into display rows.
///
/// The tool's input is the whole truth: every call carries the
/// complete list, so the newest call's rows are the current list and
/// any older call's rows are that moment's snapshot — the same parse
/// feeds the conversation's per-update blocks and the plan section's
/// standing list. The input is normalized to the tool's own
/// acceptance first, so every shape `TodoWrite` repairs rather than
/// rejects parses here too: a `todos` value that arrived JSON-encoded
/// as a string is decoded, and a subject that is absent, null, or
/// blank is derived from the entry's description — a submission the
/// run accepted must never leave the plan panel stale. `None` when
/// the text is not a submission the tool could have sent (not JSON,
/// no `todos` array even after decoding): the caller renders the
/// call the way any other tool renders.
pub(crate) fn parse_submission(input_json: &str) -> Option<Vec<TodoRow>> {
    let mut value: serde_json::Value = serde_json::from_str(input_json).ok()?;
    normalize_submission(&mut value);
    let parsed: TodoSubmission = serde_json::from_value(value).ok()?;
    Some(
        parsed
            .todos
            .into_iter()
            .map(|entry| {
                let subject = if entry.subject.trim().is_empty() {
                    derived_subject(&entry.description, &entry.id)
                } else {
                    entry.subject
                };
                TodoRow {
                    id: entry.id,
                    subject,
                    status: TodoStatus::from_name(&entry.status),
                }
            })
            .collect(),
    )
}

/// Parse a `TodoWrite` tool result's rendered list into display rows.
///
/// The enforced truth: after the tool applies the rung law, this is
/// the list the run actually holds — the submission may say `completed`
/// where the enforced list says in
/// progress, and the plan panel must not believe the request over the
/// result. Reads the render's own icons (`○` pending, `◐` in
/// progress, `●` completed) and ids from the entry lines; `None` when
/// the output carries no task list (a cleared list, an error) so the
/// caller falls back to whatever it can read of the request. An id
/// carrying `] ` is ambiguous on the page — the format escapes nothing —
/// and reads back as its shortest prefix. This is the read a caller
/// takes first where both are at hand — the request says what was
/// asked, the render what the tool settled for — and
/// [`parse_submission`] answers only where this one cannot, a resumed
/// transcript keeping the request and not the render.
pub(crate) fn parse_rendered(output: &str) -> Option<Vec<TodoRow>> {
    if !output.contains("# Task List") {
        return None;
    }
    let mut rows = Vec::new();
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
        rows.push(TodoRow {
            id: id.to_string(),
            subject: subject.to_string(),
            status,
        });
    }
    Some(rows)
}

/// The rows a submission changed, by id.
///
/// Every entry whose status is not what the previous snapshot
/// carried — newly finished, newly started, newly arrived. The empty
/// map is an update that moved nothing, which still renders its list
/// but accents nothing.
pub(crate) fn status_changes(
    rows: &[TodoRow],
    previous: &[TodoRow],
) -> HashMap<String, TodoStatus> {
    let before: HashMap<&str, TodoStatus> = previous
        .iter()
        .map(|row| (row.id.as_str(), row.status))
        .collect();
    rows.iter()
        .filter(|row| before.get(row.id.as_str()) != Some(&row.status))
        .map(|row| (row.id.clone(), row.status))
        .collect()
}

/// The colors the todo rows render in.
///
/// Four registers are all the rows need: the plain of standing work
/// (the terminal's own text color — a task not yet started is not
/// de-emphasized, it is simply waiting), the dim of finished work and
/// hints, the accent of the current rung, and the moved register a
/// row wears for the update that touched it. Passed in by the caller
/// so the module stays a renderer with no theme of its own.
pub(crate) struct TodoColors {
    /// De-emphasized text — completed subjects and hints.
    ///
    /// Finished work stays readable but retired; the summary line
    /// rides it too.
    pub dim: ratatui::style::Color,

    /// The accent — the in-progress mark alone.
    ///
    /// The register of the rung currently running, so a reader
    /// scanning for the live work finds exactly it; a row this update
    /// moved wears [`Self::moved`] instead, never this one.
    pub accent: ratatui::style::Color,

    /// The moved register — rows this update touched.
    ///
    /// The secondary accent: a touched-but-standing row (a newborn, a
    /// reopen) stays visible to a scrolling reader without borrowing
    /// the in-progress accent it never earned. Distinct from
    /// [`Self::accent`] so color never lies about the rung.
    pub moved: ratatui::style::Color,

    /// The fill the plan section's badge rides on.
    ///
    /// The secondary accent — the queue's badge wears the primary,
    /// and the two panels read as one family of chrome with two
    /// names. In a theme that defers to the terminal this is
    /// `Reset`, and the pill becomes plain text.
    pub badge_fill: ratatui::style::Color,

    /// The letters on the badge's fill.
    ///
    /// The canvas color, keeping the word legible on the accent —
    /// the same pairing the queue's badge uses.
    pub badge_letters: ratatui::style::Color,
}

/// Render the checkbox rows of one submission.
///
/// One row per entry, in the submission's own order: the icon, a
/// space, the subject — completed rows uniform in the dim register,
/// their subjects struck through and their checked boxes plain, so
/// finished work reads as retired the moment it finishes and the box
/// stays legible as the row's status. Pending rows read plain —
/// standing work is not retired work, and greying an untouched task
/// reads as demotion — unless this update moved them, in which case
/// they wear the moved register: still standing, still touched, and
/// never mistaken for the rung in progress. Long subjects wrap under
/// their icon, indented to the subject's column. `changes` may be
/// empty; the caller owns what a "change" is (it is
/// [`status_changes`] against the previous snapshot).
pub(crate) fn render_rows(
    rows: &[TodoRow],
    changes: &HashMap<String, TodoStatus>,
    colors: &TodoColors,
    width: u16,
) -> Vec<Line<'static>> {
    let wrap = usize::from(width.max(1)).saturating_sub(4).max(1);
    let mut lines = Vec::new();
    for row in rows {
        let moved = changes.contains_key(&row.id);
        let (icon, icon_style, subject_style) = match row.status {
            TodoStatus::Completed => (
                "\u{2611}",
                Style::default().fg(colors.dim),
                Style::default()
                    .fg(colors.dim)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
            TodoStatus::InProgress => (
                "\u{25d0}",
                Style::default().fg(colors.accent),
                Style::default().fg(colors.accent),
            ),
            TodoStatus::Pending => {
                let register = if moved {
                    colors.moved
                } else {
                    ratatui::style::Color::Reset
                };
                (
                    "\u{2610}",
                    Style::default().fg(register),
                    Style::default().fg(register),
                )
            }
        };
        // The subject wraps under its icon; the icon's style carries
        // the row's state, and the subject keeps the row's register.
        let mut wrapped = crate::app::plain_wrapped_lines_styled(&row.subject, wrap, subject_style);
        if let Some(first) = wrapped.first_mut() {
            // The wrapped line carries the subject's register as its
            // base style, and patch semantics smear a base modifier
            // onto every span laid over the line — the checkbox
            // included. The first line's spans are rebuilt explicit,
            // so the icon keeps its own plain register beside the
            // struck words.
            let carried = first
                .spans
                .pop()
                .map_or(String::new(), |span| span.content.to_string());
            first.spans = vec![
                Span::styled(format!("{icon} "), icon_style),
                Span::styled(carried, subject_style),
            ];
            first.style = Style::default();
        }
        for continuation in wrapped.iter_mut().skip(1) {
            continuation.spans.insert(0, Span::raw("  ".to_string()));
        }
        lines.extend(wrapped);
    }
    lines
}

/// The done count over the total, the section's own spelling of it.
///
/// One fraction shared by every register that names the plan — the
/// badge row, the folded stand-in — so the count reads the same
/// wherever the user meets it.
fn done_fraction(rows: &[TodoRow]) -> String {
    let done = rows
        .iter()
        .filter(|row| row.status == TodoStatus::Completed)
        .count();
    format!("{}/{}", done, rows.len())
}

/// The count line that heads a rendered list.
///
/// `2/5 done` — the fraction a scrolling reader wants first, in the
/// dim register so the checkbox rows below it carry the detail.
pub(crate) fn summary_line(rows: &[TodoRow], colors: &TodoColors) -> Line<'static> {
    Line::styled(
        format!("{} done", done_fraction(rows)),
        Style::default().fg(colors.dim),
    )
}

/// The folded section's rows: the air above, then the badge row.
///
/// The same stand-off the standing section keeps from the
/// transcript — one blank row, so the pill reads as a chip over the
/// pane rather than a line of it — with the pill naming the plan the
/// way the standing section does, the count in its own spelling, and
/// the chord that opens the list again dim beside it. A folded plan
/// stays out of sight without being out of mind: the user always
/// knows work stands and how to see it.
pub(crate) fn folded_lines(rows: &[TodoRow], colors: &TodoColors) -> Vec<Line<'static>> {
    vec![Line::from(Vec::new()), folded_hint(rows, colors)]
}

/// The one-row badge the folded section stands on.
///
/// The pill and the count, air above — the standing section's header
/// with the chord in place of the rows: [`folded_lines`] assembles
/// the pair, and this builds the row itself.
fn folded_hint(rows: &[TodoRow], colors: &TodoColors) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {TODO_TITLE} "),
            Style::default()
                .fg(colors.badge_letters)
                .bg(colors.badge_fill),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{} · ctrl+t", done_fraction(rows)),
            Style::default().fg(colors.dim),
        ),
    ])
}

/// The pinned plan section's rows: the air above, the badge row, as
/// many task rows as `body_cap` pays for, and an ellipsis line naming
/// however many tasks did not fit.
///
/// The section is the plan's window, not its archive — the
/// conversation's per-update blocks hold every task in full, so the
/// cap spends the conversation's rows on the tasks nearest the top
/// of the plan and says exactly what it cut. A single task whose
/// wording wraps past the whole budget keeps its head while the
/// budget pays for even one row; a budget that pays for none names
/// every task through the ellipsis rather than showing a count that
/// disagrees with the rows. The ellipsis line
/// may take one row beyond the cap — its arithmetic is the caller's
/// height's arithmetic, both counting what this returns.
pub(crate) fn section_lines(
    rows: &[TodoRow],
    changes: &HashMap<String, TodoStatus>,
    colors: &TodoColors,
    width: u16,
    body_cap: usize,
    chord_hint: Option<&'static str>,
) -> Vec<Line<'static>> {
    let mut badge_spans = vec![
        Span::styled(
            format!(" {TODO_TITLE} "),
            Style::default()
                .fg(colors.badge_letters)
                .bg(colors.badge_fill),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{} done", done_fraction(rows)),
            Style::default().fg(colors.dim),
        ),
    ];
    if let Some(chord) = chord_hint {
        badge_spans.push(Span::raw(" · "));
        badge_spans.push(Span::styled(
            chord.to_string(),
            Style::default().fg(colors.dim),
        ));
    }
    let mut lines = vec![Line::from(Vec::new()), Line::from(badge_spans)];
    let groups: Vec<Vec<Line<'static>>> = rows
        .iter()
        .map(|row| render_rows(std::slice::from_ref(row), changes, colors, width))
        .collect();
    let mut used = 0usize;
    let mut shown = 0usize;
    for (index, group) in groups.into_iter().enumerate() {
        if used.saturating_add(group.len()) > body_cap {
            if used == 0 && body_cap > 0 && !group.is_empty() {
                lines.extend(group.into_iter().take(body_cap));
                shown = index.saturating_add(1);
            }
            break;
        }
        used = used.saturating_add(group.len());
        shown = shown.saturating_add(1);
        lines.extend(group);
    }
    let more = rows.len().saturating_sub(shown);
    if more > 0 {
        lines.push(Line::styled(
            format!("… +{more} more (ctrl+t for all)"),
            Style::default().fg(colors.dim),
        ));
    }
    lines
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::let_underscore_must_use
)]
mod tests {
    use super::*;

    #[test]
    fn a_null_subject_parses_to_the_derived_row() {
        // The tool repairs a null subject rather than rejecting the
        // call, so the run acted on the derived wording — the display
        // must read the same list, or the plan panel keeps the last
        // snapshot standing while the run moved on.
        let rows = parse_submission(
            r#"{"todos": [{"id": "t1", "subject": null,
                          "description": "ship the fix\nthen rest",
                          "status": "pending"}]}"#,
        )
        .expect("a null subject parses the way the tool accepts it");
        assert_eq!(
            rows.first().map(|row| row.subject.as_str()),
            Some("ship the fix"),
            "the row carries the derived subject the run acted on"
        );
    }

    #[test]
    fn a_json_encoded_todos_array_parses_to_its_rows() {
        // A double-encoded list — the whole array riding as a JSON
        // string — is the other shape the tool repairs before its own
        // parse; the display decodes it the same way, so the plan
        // panel renders the update the run accepted instead of
        // skipping it as unparseable.
        let encoded = serde_json::to_string(&[serde_json::json!({
            "id": "t1",
            "subject": "close the list",
            "status": "in_progress"
        })])
        .expect("the fixture encodes");
        let input = serde_json::json!({ "todos": encoded }).to_string();
        let rows =
            parse_submission(&input).expect("an encoded array parses the way the tool accepts it");
        assert_eq!(
            rows.first().map(|row| row.subject.as_str()),
            Some("close the list"),
            "the decoded array's rows are the plan"
        );
    }

    #[test]
    fn a_submission_without_a_todos_key_still_refuses() {
        // The repairs widen acceptance only to what the tool itself
        // accepts: a payload with no `todos` at all — before or after
        // decoding — is not a submission, and the caller renders it
        // as an ordinary tool block.
        assert!(
            parse_submission(r#"{"note": "not a list"}"#).is_none(),
            "the normalization does not invent a list"
        );
    }

    #[test]
    fn the_rendered_result_parses_as_the_enforced_truth() {
        // A live session's shape: the model requested eight completed,
        // the rung law advanced six of them to in progress, and the
        // rendered result is the list the run holds — the panel reads
        // this, not the request that preceded it.
        let rendered = "Adjusted: 'modrs' requested completed from pending — \
advanced to in progress (one rung per call)\n\n# Task List\n\n\
Summary: 9 pending, 1 in progress, 2 completed\n\n\
◐ [origin] Strip origin metadata\n\n\
● [audit] Audit the tree\n\n\
○ [impl] Implement the list module\n\n";
        let rows = parse_rendered(rendered).expect("the task list renders rows");
        let statuses: Vec<(&str, TodoStatus)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("origin", TodoStatus::InProgress),
                ("audit", TodoStatus::Completed),
                ("impl", TodoStatus::Pending),
            ],
            "the enforced statuses ride the icons, the sorted order kept"
        );
        assert_eq!(
            rows.first().map(|row| row.subject.as_str()),
            Some("Strip origin metadata"),
            "the subject follows the id bracket"
        );
    }

    #[test]
    fn a_result_without_a_task_list_parses_to_none() {
        // The cleared list and any error answer None, so the caller
        // falls back to reading the request — never to inventing rows.
        assert!(parse_rendered("Todo list cleared.").is_none());
        assert!(parse_rendered("Invalid input: nope").is_none());
    }

    #[test]
    fn an_id_carrying_the_closing_bracket_reads_back_as_its_shortest_prefix() {
        // The render escapes nothing, so `○ [a] b] the tail` is the same
        // page for two different plans and no parse rule can prefer one.
        // The pin holds what this parser really does — close the id at the
        // first `] ` — rather than a promise the format cannot keep; the
        // ambiguity is this parser's to answer for, since a caller with
        // both reads takes this one.
        let rows = parse_rendered("# Task List\n\n\u{25cb} [a] b] the tail\n")
            .expect("the header marks a task list");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].id, "a", "the first `] ` closes the id");
        assert_eq!(rows[0].subject, "b] the tail");
    }

    #[test]
    fn the_four_registers_keep_standing_work_plain() {
        // Grey means retired: a completed row reads dim and struck, a
        // pending row the update did not touch reads in the terminal's
        // own register — standing work is not demoted — a moved row
        // wears the moved register, and the rung in progress wears the
        // accent alone. An earlier spelling dimmed unmoved pending rows
        // (a reader met a freshly-filed task going grey the moment the
        // next update landed), and a later one lent the accent to moved
        // rows too — a newborn read as running the moment it was filed.
        let rows = vec![
            TodoRow {
                id: "done".to_string(),
                subject: "finished work".to_string(),
                status: TodoStatus::Completed,
            },
            TodoRow {
                id: "idle".to_string(),
                subject: "standing work".to_string(),
                status: TodoStatus::Pending,
            },
            TodoRow {
                id: "new".to_string(),
                subject: "newly filed work".to_string(),
                status: TodoStatus::Pending,
            },
            TodoRow {
                id: "now".to_string(),
                subject: "current rung".to_string(),
                status: TodoStatus::InProgress,
            },
        ];
        let mut changes = HashMap::new();
        changes.insert("now".to_string(), TodoStatus::InProgress);
        changes.insert("new".to_string(), TodoStatus::Pending);
        let colors = TodoColors {
            dim: ratatui::style::Color::DarkGray,
            accent: ratatui::style::Color::Cyan,
            moved: ratatui::style::Color::Yellow,
            badge_fill: ratatui::style::Color::Reset,
            badge_letters: ratatui::style::Color::Reset,
        };
        let lines = render_rows(&rows, &changes, &colors, 60);
        let registers: Vec<(Option<ratatui::style::Color>, bool)> = lines
            .iter()
            .map(|line| {
                let icon = line.spans.first().expect("an icon leads each row");
                (
                    icon.style.fg,
                    icon.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::CROSSED_OUT),
                )
            })
            .collect();
        assert_eq!(
            registers,
            vec![
                (Some(ratatui::style::Color::DarkGray), false),
                (Some(ratatui::style::Color::Reset), false),
                (Some(ratatui::style::Color::Yellow), false),
                (Some(ratatui::style::Color::Cyan), false),
            ],
            "retired dim, standing plain, moved in its own register, \
             running accented — and no register borrows another's meaning"
        );
        let struck: Vec<bool> = lines
            .iter()
            .map(|line| {
                line.spans
                    .get(1)
                    .expect("a subject follows the icon")
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::CROSSED_OUT)
            })
            .collect();
        assert_eq!(
            struck,
            vec![true, false, false, false],
            "the strike rides the retired subject alone"
        );
    }

    #[test]
    fn a_budget_that_pays_for_no_rows_names_every_task() {
        // The ellipsis counts what the reader cannot see. At a zero
        // budget no task row is rendered at all, so every task must
        // stand behind the count — showing the ellipsis one short
        // would disagree with the empty body under it.
        let rows: Vec<TodoRow> = (0..3)
            .map(|index| TodoRow {
                id: format!("t{index}"),
                subject: format!("task {index}"),
                status: if index == 0 {
                    TodoStatus::Completed
                } else {
                    TodoStatus::Pending
                },
            })
            .collect();
        let colors = TodoColors {
            dim: ratatui::style::Color::Reset,
            accent: ratatui::style::Color::Reset,
            moved: ratatui::style::Color::Reset,
            badge_fill: ratatui::style::Color::Reset,
            badge_letters: ratatui::style::Color::Reset,
        };
        let lines = section_lines(&rows, &HashMap::new(), &colors, 60, 0, None);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            text.last().expect("an ellipsis stands last"),
            "… +3 more (ctrl+t for all)",
            "a zero budget hides nothing yet counts it as shown"
        );
        assert_eq!(
            lines.len(),
            3,
            "the section is air, badge, and the honest count"
        );
    }
}
