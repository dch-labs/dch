//! Tool-call rendering for the conversation view.
//!
//! Pure helpers that turn tool records into styled lines: a
//! humanized one-line summary built from the tool's name and input,
//! plus running and completed line builders shaped by the display
//! [`Verbosity`]. No lifecycle logic, no loopctl, no I/O — every
//! function is a mapping from records to lines and back to nothing
//! else.

use dch_config::Verbosity;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ToolResultDisplay;
use crate::message::ActiveTool;
use crate::theme::Theme;

/// Braille spinner frames, advanced by the render tick while a tool
/// runs.
///
/// One animation beat for every running indicator; each frame is
/// one display column wide (pinned by test).
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The running indicator for the per-tool line.
///
/// The static hourglass the spec names for in-flight calls; the
/// animated braille frames are the Quiet mode's quieter pulse.
pub const RUNNING_GLYPH: &str = "⏳";

/// The completed-success indicator.
///
/// Rendered in the theme's success color; the line under it says
/// what the call did.
pub const OK_GLYPH: &str = "✓";

/// The completed-failure indicator.
///
/// Rendered in the theme's error color; a failed call keeps its
/// summary so the reader still sees what was attempted.
pub const ERR_GLYPH: &str = "✗";

/// The character budget tool summaries truncate to.
///
/// Wide enough for a real path or command, narrow enough that the
/// summary stays the one glanceable line the conversation view is
/// built around, whatever the input's size.
const SUMMARY_WIDTH: usize = 48;

/// The primary value of a tool call's input, from the full JSON.
///
/// The extraction [`humanize_tool_summary`] applies to strings,
/// taken before any capping — the observer stashes the result at
/// dispatch time, so a capped stash never loses a parseability it
/// never had. Batch tools carry their count in the value ("a.rs (3
/// edits)", "12 items"): the count lives in the full JSON, so it is
/// decided here or not at all.
#[must_use]
pub fn display_input(name: &str, input: &serde_json::Value) -> String {
    let compact = input.to_string();
    let value = extract_primary_value(name, &compact);
    match name {
        "MultiEdit" => match edit_count(&compact) {
            Some(count) => format!("{value} ({count} edits)"),
            None => value,
        },
        "TodoWrite" => match todo_count(&compact) {
            Some(count) => format!("{count} items"),
            None => value,
        },
        _ => value,
    }
}

/// Build a one-line, human-readable summary of a tool invocation.
///
/// The verb comes from the registered tool name (Read→Reading,
/// Bash→Running, …) and the subject from the call's input — the
/// primary value the observer extracted at dispatch or a JSON
/// object. Unknown tools fall back to
/// `"{Name}: {value}"`. Long subjects truncate to
/// a fixed character budget.
#[must_use]
pub fn humanize_tool_summary(name: &str, input: &str) -> String {
    let value = extract_primary_value(name, input);
    match name {
        "Read" => format!("Reading {value}…"),
        "Write" => format!("Writing {value}…"),
        "Edit" | "MultiEdit" => format!("Editing {value}…"),
        "Bash" => format!("Running: {value}"),
        "FileViewer" => format!("Viewing {value}…"),
        "Glob" => format!("Glob {value}"),
        "Grep" => format!("Grep {value}"),
        "CodeSearch" => format!("Searching {value}"),
        "Tree" => format!("Tree {value}"),
        "TodoWrite" => {
            let count = if value.is_empty() {
                "several items"
            } else {
                &value
            };
            format!("Updating todo list ({count})")
        }
        _ => {
            if value.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {value}")
            }
        }
    }
}

/// The salient input value for a tool call.
///
/// Parses the input as JSON and prefers the field the tool's
/// summary is about (`file_path` for the file tools, `command` for
/// Bash, `pattern` for the search tools); otherwise takes the
/// first string field; a non-JSON input loses any leading
/// `"key: "` prefix. Empty when nothing string-valued exists.
fn extract_primary_value(name: &str, input: &str) -> String {
    let preferred: &[&str] = match name {
        "Read" | "Write" | "Edit" | "MultiEdit" | "FileViewer" | "Tree" => &["file_path", "path"],
        "Bash" => &["command"],
        "Glob" | "Grep" | "CodeSearch" => &["pattern", "query"],
        _ => &[],
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) else {
        return truncate(input);
    };
    let Some(object) = parsed.as_object() else {
        return truncate(input);
    };
    for field in preferred {
        if let Some(value) = object.get(*field).and_then(serde_json::Value::as_str) {
            return truncate(value);
        }
    }
    // Batch tools nest their target inside the first edit.
    if let Some(value) = parsed
        .get("edits")
        .and_then(serde_json::Value::as_array)
        .and_then(|edits| edits.first())
        .and_then(|first| first.get("file_path").or_else(|| first.get("path")))
        .and_then(serde_json::Value::as_str)
    {
        return truncate(value);
    }
    let first_string = object
        .values()
        .find_map(serde_json::Value::as_str)
        .unwrap_or("");
    truncate(first_string)
}

/// How many edits a `MultiEdit` batch carries, when countable.
///
/// The `MultiEdit` summary appends "(n edits)" so a batch reads as a
/// batch; `None` — for a non-JSON or edit-less input — leaves the
/// plain summary instead of a wrong count.
fn edit_count(input: &str) -> Option<usize> {
    let parsed = serde_json::from_str::<serde_json::Value>(input).ok()?;
    parsed.get("edits")?.as_array().map(Vec::len)
}

/// How many items a `TodoWrite` list carries, when countable.
///
/// The `TodoWrite` summary names the list's size; `None` lets the
/// caller's vaguer "several" stand in rather than guess.
fn todo_count(input: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(input).ok()?;
    parsed
        .get("todos")
        .and_then(|todos| todos.as_array())
        .map(|items| items.len().to_string())
}

/// Trim a value to the summary width on a character boundary.
///
/// Counts characters, never bytes, so a multi-byte path cannot be
/// cut mid-glyph; the ellipsis marks that a cut happened.
fn truncate(value: &str) -> String {
    if value.chars().count() <= SUMMARY_WIDTH {
        return value.to_string();
    }
    let cut: String = value.chars().take(SUMMARY_WIDTH - 1).collect();
    format!("{cut}…")
}

/// The lines for a tool currently in flight.
///
/// Quiet shows a dim braille frame and summary; Normal the running
/// glyph, humanized summary, and elapsed time; Verbose the raw
/// name and the call's primary input on its own indented line.
/// Running lines
/// are rebuilt every frame — their elapsed time and spinner frame
/// change — so caching them is never worthwhile.
#[must_use]
pub fn running_tool_lines(
    tool: &ActiveTool,
    spinner_idx: usize,
    theme: &Theme,
    verbosity: Verbosity,
) -> Vec<Line<'static>> {
    let elapsed = format_elapsed(tool.start.elapsed().as_secs_f64());
    let frame = SPINNER_FRAMES
        .get(spinner_idx)
        .or_else(|| SPINNER_FRAMES.first())
        .unwrap_or(&"");
    match verbosity {
        Verbosity::Quiet => vec![Line::from(Span::styled(
            format!(
                "{frame} {}",
                humanize_tool_summary(&tool.name, &tool.input_summary)
            ),
            Style::default().fg(theme.ui.dim),
        ))],
        Verbosity::Normal => vec![Line::from(vec![
            Span::styled(RUNNING_GLYPH, Style::default().fg(theme.ui.dim)),
            Span::raw(format!(
                " {} ",
                humanize_tool_summary(&tool.name, &tool.input_summary)
            )),
            Span::styled(format!("({elapsed})"), Style::default().fg(theme.ui.dim)),
        ])],
        Verbosity::Verbose => vec![
            Line::from(vec![
                Span::styled(RUNNING_GLYPH, Style::default().fg(theme.ui.dim)),
                Span::styled(
                    format!(" {} ", tool.name),
                    Style::default()
                        .fg(theme.ui.assistant_message_fg)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Span::styled(format!("({elapsed})"), Style::default().fg(theme.ui.dim)),
            ]),
            Line::styled(
                format!("    Input: {}", tool.input_summary),
                Style::default().fg(theme.ui.dim),
            ),
        ],
    }
}

/// The lines for a completed tool call.
///
/// Quiet renders one dim summary line; Normal the outcome glyph,
/// humanized summary, and duration; Verbose the raw name plus the
/// input. No mode renders an output preview: nothing in the
/// pipeline populates one.
#[must_use]
pub fn completed_tool_lines(
    result: &ToolResultDisplay,
    theme: &Theme,
    verbosity: Verbosity,
) -> Vec<Line<'static>> {
    let (glyph, glyph_color) = if result.is_error {
        (ERR_GLYPH, theme.ui.status_error)
    } else {
        (OK_GLYPH, theme.ui.status_success)
    };
    let duration = format_elapsed(result.duration.as_secs_f64());
    let summary = humanize_tool_summary(&result.name, &result.input_summary);
    match verbosity {
        Verbosity::Quiet => vec![Line::from(Span::styled(
            format!("{glyph} {summary}"),
            Style::default().fg(theme.ui.dim),
        ))],
        Verbosity::Normal => vec![Line::from(vec![
            Span::styled(glyph, Style::default().fg(glyph_color)),
            Span::styled(format!(" {summary} "), Style::default().fg(theme.ui.dim)),
            Span::styled(format!("({duration})"), Style::default().fg(theme.ui.dim)),
        ])],
        Verbosity::Verbose => vec![
            Line::from(vec![
                Span::styled(glyph, Style::default().fg(glyph_color)),
                Span::styled(
                    format!(" {} ", result.name),
                    Style::default()
                        .fg(theme.ui.assistant_message_fg)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Span::styled(format!("({duration})"), Style::default().fg(theme.ui.dim)),
            ]),
            Line::styled(
                format!("    Input: {}", result.input_summary),
                Style::default().fg(theme.ui.dim),
            ),
        ],
    }
}

/// Render an elapsed or duration span compactly.
///
/// Sub-minute spans show tenths; longer ones round once to whole
/// seconds and read as minutes and seconds.
#[must_use]
pub fn format_elapsed(secs: f64) -> String {
    let total = secs.round();
    if total >= 60.0 {
        format!("{}m{}s", (total / 60.0).floor(), total % 60.0)
    } else {
        format!("{secs:.1}s")
    }
}
