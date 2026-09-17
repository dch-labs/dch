//! Tool-render pins — the humanizer table and the per-verbosity
//! line shapes, driven as pure functions.

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]

use dch_config::Verbosity;
use dch_tui::ToolResultDisplay;
use dch_tui::message::ActiveTool;
use dch_tui::theme::Theme;
use dch_tui::tool_render::{
    ERR_GLYPH, OK_GLYPH, RUNNING_GLYPH, SPINNER_FRAMES, completed_tool_lines,
    humanize_tool_summary, running_tool_lines,
};
use unicode_width::UnicodeWidthStr;

fn result(name: &str, input: &str, is_error: bool, secs: f64) -> ToolResultDisplay {
    ToolResultDisplay {
        name: name.to_string(),
        is_error,
        duration: std::time::Duration::from_secs_f64(secs),
        input_summary: input.to_string(),
        output_preview: String::new(),
    }
}

fn active(name: &str, input: &str) -> ActiveTool {
    ActiveTool {
        call_id: String::new(),
        name: name.to_string(),
        input_summary: input.to_string(),
        start: std::time::Instant::now(),
    }
}

fn text_of(line: &ratatui::text::Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.to_string())
        .collect()
}

#[test]
fn each_tool_humanizes_to_its_verb() {
    let cases = [
        ("Read", r#"{"file_path":"/a/b.rs"}"#, "Reading /a/b.rs…"),
        ("Write", r#"{"file_path":"/a/b.rs"}"#, "Writing /a/b.rs…"),
        ("Edit", r#"{"file_path":"/a/b.rs"}"#, "Editing /a/b.rs…"),
        (
            "MultiEdit",
            "/a/b.rs (3 edits)",
            "Editing /a/b.rs (3 edits)…",
        ),
        (
            "Bash",
            r#"{"command":"cargo build","timeout":120}"#,
            "Running: cargo build",
        ),
        ("FileViewer", r#"{"file_path":"a.txt"}"#, "Viewing a.txt…"),
        ("Glob", r#"{"pattern":"**/*.rs"}"#, "Glob **/*.rs"),
        ("Grep", r#"{"pattern":"fn main"}"#, "Grep fn main"),
        (
            "CodeSearch",
            r#"{"pattern":"matcher"}"#,
            "Searching matcher",
        ),
        ("Tree", r#"{"path":"src"}"#, "Tree src"),
        ("TodoWrite", "2 items", "Updating todo list (2 items)"),
    ];
    for (name, input, expected) in cases {
        assert_eq!(
            humanize_tool_summary(name, input),
            expected,
            "{name} humanizes to its verb"
        );
    }
}

#[test]
fn extraction_prefers_the_named_field() {
    assert_eq!(
        humanize_tool_summary("Read", r#"{"pattern":"nope","file_path":"yes.rs"}"#),
        "Reading yes.rs…",
        "the named field wins over JSON order"
    );
    assert_eq!(
        humanize_tool_summary("Frobnicate", r#"{"count":3,"label":"outer"}"#),
        "Frobnicate: outer",
        "an unknown tool takes the first string field"
    );
    assert_eq!(
        humanize_tool_summary("Bash", r#"echo "note: hi""#),
        "Running: echo \"note: hi\"",
        "a value containing a key separator flows to the verb untouched"
    );
}

#[test]
fn unknown_and_empty_inputs_fall_back_sanely() {
    assert_eq!(
        humanize_tool_summary("Frobnicate", r#"{"x":"y"}"#),
        "Frobnicate: y",
        "an unknown tool names itself beside the first value"
    );
    assert_eq!(
        humanize_tool_summary("Read", ""),
        "Reading …",
        "an empty input still produces its verb"
    );
    assert_eq!(
        humanize_tool_summary("Frobnicate", ""),
        "Frobnicate",
        "an unknown tool with no input is just its name"
    );
}

#[test]
fn quiet_running_is_a_braille_frame_without_elapsed() {
    let theme = Theme::default();
    let lines = running_tool_lines(
        &active("Read", r#"{"file_path":"a.rs"}"#),
        0,
        &theme,
        Verbosity::Quiet,
    );
    assert_eq!(lines.len(), 1, "Quiet renders one line");
    let text = text_of(&lines[0]);
    assert!(
        SPINNER_FRAMES.contains(&text.chars().next().unwrap().to_string().as_str()),
        "the leading glyph is a spinner frame: {text:?}"
    );
    assert!(
        !text.contains("s)"),
        "Quiet shows no elapsed time: {text:?}"
    );
}

#[test]
fn normal_running_shows_the_hourglass_summary_and_elapsed() {
    let theme = Theme::default();
    let lines = running_tool_lines(
        &active("Read", r#"{"file_path":"a.rs"}"#),
        3,
        &theme,
        Verbosity::Normal,
    );
    assert_eq!(
        lines.len(),
        1,
        "Normal's running tool renders a single line"
    );
    let text = text_of(&lines[0]);
    assert!(text.contains(RUNNING_GLYPH), "the running glyph shows");
    assert!(
        text.contains("Reading a.rs…"),
        "the summary is humanized: {text:?}"
    );
    assert!(text.contains("s)"), "elapsed time shows: {text:?}");
}

#[test]
fn verbose_running_shows_the_raw_name_and_input_line() {
    let theme = Theme::default();
    let lines = running_tool_lines(
        &active("Read", r#"{"file_path":"a.rs"}"#),
        0,
        &theme,
        Verbosity::Verbose,
    );
    assert_eq!(lines.len(), 2, "the input block adds a line");
    let text = text_of(&lines[0]);
    assert!(text.contains("Read "), "the raw name shows: {text:?}");
    assert!(
        !text.contains("Reading"),
        "no humanization in Verbose: {text:?}"
    );
    assert!(
        text_of(&lines[1]).starts_with("    Input:"),
        "the input renders indented: {:?}",
        text_of(&lines[1])
    );
}

#[test]
fn quiet_completed_is_one_dim_line_without_duration() {
    let theme = Theme::default();
    let lines = completed_tool_lines(
        &result("Read", r#"{"file_path":"a.rs"}"#, false, 0.4),
        &theme,
        Verbosity::Quiet,
    );
    assert_eq!(lines.len(), 1);
    let text = text_of(&lines[0]);
    assert!(text.contains(OK_GLYPH), "the outcome glyph shows");
    assert!(
        text.contains("Reading a.rs…"),
        "the summary is humanized even when quiet: {text:?}"
    );
    assert!(!text.contains("s)"), "no duration in Quiet: {text:?}");
    assert_eq!(
        lines[0].spans[0].style.fg,
        Some(theme.ui.dim),
        "the Quiet line is dim throughout"
    );
}

#[test]
fn normal_completed_shows_the_humanized_summary_and_duration() {
    let theme = Theme::default();
    let lines = completed_tool_lines(
        &result("Read", r#"{"file_path":"a.rs"}"#, false, 0.4),
        &theme,
        Verbosity::Normal,
    );
    assert_eq!(lines.len(), 1);
    let text = text_of(&lines[0]);
    assert!(
        text.contains("✓ Reading a.rs… (0.4s)"),
        "Normal shows glyph, humanized summary, duration: {text:?}"
    );
}

#[test]
fn verbose_completed_shows_the_name_and_input() {
    let theme = Theme::default();
    let lines = completed_tool_lines(
        &result("Read", r#"{"file_path":"a.rs"}"#, false, 0.4),
        &theme,
        Verbosity::Verbose,
    );
    assert_eq!(
        lines.len(),
        2,
        "the input block adds its own line in Verbose"
    );
    let text = text_of(&lines[0]);
    assert!(text.contains("Read "), "the raw name shows: {text:?}");
    assert!(
        text_of(&lines[1]).contains(r#""file_path":"a.rs""#),
        "the full input renders: {:?}",
        text_of(&lines[1])
    );
}

#[test]
fn failures_carry_the_theme_error_color_and_success_is_themed() {
    let theme = Theme::default();
    let failed = completed_tool_lines(
        &result("Bash", r#"{"command":"false"}"#, true, 0.1),
        &theme,
        Verbosity::Normal,
    );
    assert_eq!(
        failed[0].spans[0].content, ERR_GLYPH,
        "a failure leads with the cross"
    );
    assert_eq!(
        failed[0].spans[0].style.fg,
        Some(theme.ui.status_error),
        "the failure glyph carries the theme error color"
    );
    let ok = completed_tool_lines(
        &result("Bash", r#"{"command":"true"}"#, false, 0.1),
        &theme,
        Verbosity::Normal,
    );
    assert_eq!(
        ok[0].spans[0].content, OK_GLYPH,
        "a success leads with the check"
    );
    assert_eq!(
        ok[0].spans[0].style.fg,
        Some(theme.ui.status_success),
        "the success glyph carries the theme success color"
    );
}

#[test]
fn long_summaries_truncate_to_the_width() {
    let long_path = format!("/very/long/path/{}.rs", "x".repeat(80));
    let summary = humanize_tool_summary("Read", &format!(r#"{{"file_path":"{long_path}"}}"#));
    assert!(summary.ends_with('…'), "the cut is marked: {summary:?}");
    assert!(
        summary.width() <= 48 + "Reading …".width(),
        "the truncated summary stays near the budget: {}",
        summary.width()
    );
}

#[test]
fn a_capped_stash_still_humanizes() {
    // The observer caps the stashed value at 60 chars with an
    // ellipsis; a capped bare value must render as itself, not as
    // an unparseable JSON fragment.
    let capped = format!("{}…", "cargo test --all-features --workspace".repeat(3));
    let summary = humanize_tool_summary("Bash", &capped);
    assert!(
        summary.starts_with("Running: cargo test"),
        "a capped value humanizes as the subject: {summary:?}"
    );
    assert!(
        !summary.contains('{'),
        "no raw JSON fragment leaks into the summary: {summary:?}"
    );
}

#[test]
fn the_spinner_index_drives_the_frame() {
    let theme = Theme::default();
    let first = running_tool_lines(&active("Read", "a"), 0, &theme, Verbosity::Quiet);
    let second = running_tool_lines(&active("Read", "a"), 1, &theme, Verbosity::Quiet);
    assert_ne!(
        text_of(&first[0]),
        text_of(&second[0]),
        "advancing the index advances the frame"
    );
}

#[test]
fn braille_frames_are_one_column_wide() {
    assert!(
        SPINNER_FRAMES.iter().all(|frame| frame.width() == 1),
        "every spinner frame is one display column"
    );
}
