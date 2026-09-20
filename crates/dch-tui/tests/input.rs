//! Input-editor tests — pure logic against `InputEditor` and
//! `InputHistory`, no terminal, no backend.

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dch_tui::input::{InputAction, InputEditor, InputHistory};

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

/// The wrap width most editor keys run at in these tests.
const WRAP: u16 = 80;

fn plain(code: KeyCode) -> KeyEvent {
    key(code, KeyModifiers::NONE)
}

fn char(c: char) -> KeyEvent {
    plain(KeyCode::Char(c))
}

fn type_str(editor: &mut InputEditor, text: &str) {
    for c in text.chars() {
        editor.handle_key(char(c), WRAP);
    }
}

/// Drive the editor to a submitted entry and return its text.
fn submit(editor: &mut InputEditor, text: &str) -> String {
    type_str(editor, text);
    match editor.handle_key(plain(KeyCode::Enter), WRAP) {
        InputAction::Submit(sent) => sent,
        other => panic!("expected a submit for {text:?}, got {other:?}"),
    }
}

#[test]
fn push_then_prev_returns_the_entry_and_stops_at_oldest() {
    let mut history = InputHistory::new(10);
    history.push("first".to_string());
    history.push("second".to_string());
    assert_eq!(
        history.older(),
        Some("second"),
        "prev from live hits the newest"
    );
    assert_eq!(history.older(), Some("first"));
    assert_eq!(history.older(), None, "already at the oldest — stays put");
    assert_eq!(history.older(), None);
}

#[test]
fn next_past_the_oldest_returns_to_live() {
    let mut history = InputHistory::new(10);
    history.push("only".to_string());
    assert_eq!(history.older(), Some("only"));
    assert_eq!(history.newer(), None, "arriving back at live reports None");
    assert_eq!(history.newer(), None);
    assert_eq!(history.older(), Some("only"), "live can browse again");
}

#[test]
fn fifo_eviction_caps_the_entries() {
    let mut history = InputHistory::new(3);
    for entry in ["one", "two", "three", "four"] {
        history.push(entry.to_string());
    }
    assert_eq!(history.older(), Some("four"));
    assert_eq!(history.older(), Some("three"));
    assert_eq!(history.older(), Some("two"));
    assert_eq!(history.older(), None, "the evicted oldest is gone");
}

#[test]
fn consecutive_duplicates_collapse_but_distinct_neighbors_stay() {
    let mut history = InputHistory::new(10);
    history.push("hi".to_string());
    history.push("hi".to_string());
    history.push("other".to_string());
    history.push("other".to_string());
    assert_eq!(history.older(), Some("other"));
    assert_eq!(history.older(), Some("hi"));
    assert_eq!(history.older(), None, "two distinct entries total");
}

#[test]
fn backspace_mid_buffer_deletes_whole_characters() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "héllo");
    editor.handle_key(plain(KeyCode::Left), WRAP);
    editor.handle_key(plain(KeyCode::Backspace), WRAP);
    assert_eq!(editor.text(), "hélo", "one backspace removes one character");

    editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL), WRAP);
    editor.handle_key(plain(KeyCode::Right), WRAP);
    editor.handle_key(plain(KeyCode::Right), WRAP);
    editor.handle_key(plain(KeyCode::Backspace), WRAP);
    assert_eq!(
        editor.text(),
        "hlo",
        "the multi-byte é leaves as one character"
    );
}

#[test]
fn backslash_enter_inserts_a_newline_and_consumes_the_backslash() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "ab\\");
    assert_eq!(
        editor.handle_key(plain(KeyCode::Enter), WRAP),
        InputAction::None,
        "a trailing backslash turns Enter into a newline, not a submit"
    );
    type_str(&mut editor, "cd");
    assert_eq!(editor.text(), "ab\ncd");
    assert!(editor.is_multiline());
    assert_eq!(editor.cursor_cell(40).map(|(row, _)| row), Some(1));
}

#[test]
fn shift_enter_inserts_a_newline_where_reported() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "ab");
    editor.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT), WRAP);
    type_str(&mut editor, "cd");
    assert_eq!(editor.text(), "ab\ncd");
    assert!(editor.is_multiline());
}

#[test]
fn enter_submits_clears_and_records_history() {
    let mut editor = InputEditor::new();
    assert_eq!(submit(&mut editor, "hello"), "hello");
    assert!(editor.is_empty());
    assert!(editor.has_history());
    editor.handle_key(plain(KeyCode::Up), WRAP);
    assert_eq!(editor.text(), "hello", "the submit is recallable");
}

#[test]
fn enter_on_an_empty_buffer_is_a_noop() {
    let mut editor = InputEditor::new();
    assert_eq!(
        editor.handle_key(plain(KeyCode::Enter), WRAP),
        InputAction::None
    );
    assert!(editor.is_empty());
    assert!(!editor.has_history());
}

#[test]
fn whitespace_only_enter_clears_without_submitting() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "  \t ");
    assert_eq!(
        editor.handle_key(plain(KeyCode::Enter), WRAP),
        InputAction::None
    );
    assert!(editor.is_empty(), "the buffer clears");
    assert!(!editor.has_history(), "nothing is recorded");
}

#[test]
fn alt_enter_behaves_as_plain_enter() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hi");
    assert_eq!(
        editor.handle_key(key(KeyCode::Enter, KeyModifiers::ALT), WRAP),
        InputAction::Submit("hi".to_string()),
        "Alt+Enter queues like Enter — the interrupt variant is not built"
    );
    assert!(editor.is_empty());
}

#[test]
fn paste_lands_atomically_and_never_partial_submits() {
    let mut editor = InputEditor::new();
    editor.insert_str("line1\nline2\n");
    assert_eq!(
        editor.handle_key(plain(KeyCode::Enter), WRAP),
        InputAction::Submit("line1\nline2\n".to_string()),
        "a pasted block submits whole — the embedded newlines never fire mid-paste"
    );
}

#[test]
fn home_and_end_move_within_the_current_line() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "ab\\");
    editor.handle_key(plain(KeyCode::Enter), WRAP);
    type_str(&mut editor, "cdef");
    editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL), WRAP);
    assert_eq!(editor.cursor_cell(40).map(|(_, col)| col), Some(0));
    editor.handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL), WRAP);
    assert_eq!(editor.cursor_cell(40).map(|(_, col)| col), Some(4));
    editor.handle_key(plain(KeyCode::Home), WRAP);
    assert_eq!(editor.cursor_cell(40), Some((1, 0)));
    editor.handle_key(plain(KeyCode::End), WRAP);
    assert_eq!(editor.cursor_cell(40), Some((1, 4)));
}

#[test]
fn up_navigates_history_when_single_line_and_moves_lines_when_multiline() {
    let mut editor = InputEditor::new();
    submit(&mut editor, "recorded");
    editor.handle_key(plain(KeyCode::Up), WRAP);
    assert_eq!(editor.text(), "recorded", "single-line Up recalls history");

    let mut multiline = InputEditor::new();
    type_str(&mut multiline, "one\\");
    multiline.handle_key(plain(KeyCode::Enter), WRAP);
    type_str(&mut multiline, "two");
    multiline.handle_key(plain(KeyCode::Up), WRAP);
    assert_eq!(multiline.cursor_cell(40).map(|(row, _)| row), Some(0));
    assert_eq!(
        multiline.text(),
        "one\ntwo",
        "multi-line Up moves the cursor, not history"
    );
    multiline.handle_key(plain(KeyCode::Down), WRAP);
    assert_eq!(multiline.cursor_cell(40).map(|(row, _)| row), Some(1));
}

#[test]
fn history_browsing_restores_the_stashed_draft() {
    let mut editor = InputEditor::new();
    submit(&mut editor, "sent");
    type_str(&mut editor, "draf");
    editor.handle_key(plain(KeyCode::Up), WRAP);
    assert_eq!(editor.text(), "sent");
    editor.handle_key(plain(KeyCode::Down), WRAP);
    assert_eq!(
        editor.text(),
        "draf",
        "returning to live restores the draft"
    );
}

#[test]
fn ctrl_u_and_ctrl_k_kill_within_the_line_and_survive_later_keys() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL), WRAP);
    assert_eq!(editor.text(), "");
    editor.handle_key(plain(KeyCode::Char('x')), WRAP);
    assert_eq!(
        editor.text(),
        "x",
        "typing after Ctrl-U must not panic — the cursor follows the drain"
    );

    let mut editor = InputEditor::new();
    type_str(&mut editor, "ab\\");
    editor.handle_key(plain(KeyCode::Enter), WRAP);
    type_str(&mut editor, "cdef");
    editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL), WRAP);
    editor.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL), WRAP);
    assert_eq!(
        editor.text(),
        "ab\n",
        "kill-to-end stays within the current line"
    );
    editor.handle_key(plain(KeyCode::Char('y')), WRAP);
    assert_eq!(
        editor.text(),
        "ab\ny",
        "typing after Ctrl-K must not panic either"
    );

    let mut editor = InputEditor::new();
    type_str(&mut editor, "héllo");
    editor.handle_key(plain(KeyCode::Right), WRAP);
    editor.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL), WRAP);
    editor.handle_key(plain(KeyCode::Char('x')), WRAP);
    assert_eq!(
        editor.text(),
        "x",
        "a mid-line Ctrl-U before a multi-byte character survives later keys"
    );
}

#[test]
fn ctrl_c_is_not_bound_in_the_editor() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "draft");
    assert_eq!(
        editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), WRAP),
        InputAction::None
    );
    assert_eq!(
        editor.text(),
        "draft",
        "quit and cancel own Ctrl-C at the app level"
    );
}

#[test]
fn the_stamp_moves_only_on_mutations() {
    // The wrap cache's key: queries leave it alone, every edit and
    // every caret move advances it, so a cached grid is reused
    // exactly until the editor changes.
    let mut editor = InputEditor::new();
    let fresh = editor.stamp();
    let queried = (
        editor.text().to_string(),
        editor.display_rows(WRAP),
        editor.cursor_cell(WRAP),
    );
    assert!(
        queried.1.len() == 1 && queried.2.is_some(),
        "the queries answer while leaving the stamp alone"
    );
    assert_eq!(editor.stamp(), fresh, "queries do not mutate");

    type_str(&mut editor, "hi");
    let after_typing = editor.stamp();
    assert!(after_typing > fresh, "typing moves the stamp");

    editor.handle_key(plain(KeyCode::Left), WRAP);
    assert!(
        editor.stamp() > after_typing,
        "a caret move counts too — the caret shares the cache"
    );
}

#[test]
fn a_realistic_huge_paste_lands_whole_and_submits_whole() {
    let mut editor = InputEditor::new();
    let huge = "x".repeat(200_001);
    editor.insert_str(&huge);
    assert_eq!(
        editor.text().chars().count(),
        200_001,
        "a paste far past the old cap lands in full"
    );
    assert_eq!(
        editor.handle_key(plain(KeyCode::Enter), WRAP),
        InputAction::Submit("x".repeat(200_001)),
        "Enter submits the whole thing"
    );
}

#[test]
fn max_chars_still_caps_a_pathological_paste() {
    let mut editor = InputEditor::new();
    let huge = "x".repeat(2_000_001);
    editor.insert_str(&huge);
    assert_eq!(editor.text().chars().count(), 2_000_000);
}

#[test]
fn a_cap_rejected_insert_leaves_the_stashed_draft_recoverable() {
    let mut editor = InputEditor::new();
    editor.insert_str(&"x".repeat(2_000_000));
    assert_eq!(
        editor.handle_key(plain(KeyCode::Enter), WRAP),
        InputAction::Submit("x".repeat(2_000_000)),
        "the full-cap entry submits and records"
    );
    type_str(&mut editor, "draf");
    editor.handle_key(plain(KeyCode::Up), WRAP);
    assert_eq!(
        editor.text().chars().count(),
        2_000_000,
        "Up recalls the full-cap entry over the stashed draft"
    );
    editor.handle_key(plain(KeyCode::Char('x')), WRAP);
    editor.handle_key(plain(KeyCode::Down), WRAP);
    assert_eq!(
        editor.text(),
        "draf",
        "a keystroke the cap rejects must not cost the half-typed line"
    );
}

#[test]
fn cursor_cell_tracks_display_width_not_chars() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "ab😀");
    assert_eq!(
        editor.cursor_cell(40).map(|(_, col)| col),
        Some(4),
        "the end sits at display column 4 — a char count would say 3"
    );
    editor.handle_key(plain(KeyCode::Left), WRAP);
    assert_eq!(editor.cursor_cell(40).map(|(_, col)| col), Some(2));
    editor.handle_key(plain(KeyCode::Right), WRAP);
    assert_eq!(editor.cursor_cell(40).map(|(_, col)| col), Some(4));
}

#[test]
fn cursor_cell_lands_on_the_wrapped_row() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "alpha beta gamma delta epsilon");
    assert_eq!(editor.cursor_cell(10).map(|(row, _)| row), Some(3));
    assert!(editor.cursor_cell(10).is_some_and(|(_, col)| col < 10));
}

#[test]
fn display_rows_grows_the_carets_continuation_row_at_an_exact_fill() {
    let mut editor = InputEditor::new();
    editor.set_text("abcdefghij".to_string());
    assert_eq!(
        editor.wrapped_lines(10),
        vec!["abcdefghij".to_string()],
        "the raw wrap fits the text on one row"
    );
    assert_eq!(
        editor.display_rows(10),
        vec!["abcdefghij".to_string(), String::new()],
        "an exactly full final row gains the caret's continuation row"
    );
    assert_eq!(
        editor.cursor_cell(10),
        Some((1, 0)),
        "the caret names the continuation row the renderer draws"
    );
    assert_eq!(
        editor.display_rows(9).len(),
        editor.wrapped_lines(9).len(),
        "a caret inside the wrapped grid grows nothing"
    );
}

#[test]
fn the_caret_stays_inside_the_rendered_grid_across_the_wrap_corpus() {
    let mut editor = InputEditor::new();
    for (buffer, width) in [
        ("abcdefghij", 10_u16),
        ("  😀  é  x  ", 10),
        ("ab\ncdef", 4),
        ("a", 1),
        ("abcdefghij", 9),
    ] {
        editor.set_text(buffer.to_string());
        let rows = editor.display_rows(width);
        if let Some((row, _)) = editor.cursor_cell(width) {
            assert!(
                usize::from(row) < rows.len(),
                "the end caret lands in the grid for {buffer:?} at width {width}: {rows:?}"
            );
        }
    }

    editor.set_text("x".repeat(200));
    let rows = editor.display_rows(40);
    if let Some((row, _)) = editor.cursor_cell(40) {
        assert_eq!(
            usize::from(row),
            rows.len() - 1,
            "a width multiple puts the end caret on the continuation row"
        );
    }
    for _ in 0..3 {
        editor.handle_key(plain(KeyCode::Left), WRAP);
        let rows = editor.display_rows(40);
        if let Some((row, _)) = editor.cursor_cell(40) {
            assert!(
                usize::from(row) < rows.len(),
                "a mid-buffer caret lands in the grid: {rows:?}"
            );
        }
    }
}

#[test]
fn tab_indents_with_spaces() {
    let mut editor = InputEditor::new();
    editor.handle_key(plain(KeyCode::Tab), WRAP);
    assert_eq!(editor.text(), "    ");
}

#[test]
fn ctrl_l_asks_for_a_redraw() {
    let mut editor = InputEditor::new();
    assert_eq!(
        editor.handle_key(key(KeyCode::Char('l'), KeyModifiers::CONTROL), WRAP),
        InputAction::Redraw
    );
}

#[test]
fn word_motion_and_deletion_cross_whitespace() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "one two");
    editor.handle_key(plain(KeyCode::Home), WRAP);
    editor.handle_key(key(KeyCode::Right, KeyModifiers::ALT), WRAP);
    assert_eq!(
        editor.cursor_cell(40).map(|(_, col)| col),
        Some(3),
        "word-right lands just past the word at the cursor, on the space"
    );
    editor.handle_key(key(KeyCode::Right, KeyModifiers::ALT), WRAP);
    assert_eq!(
        editor.cursor_cell(40).map(|(_, col)| col),
        Some(7),
        "from whitespace, word-right skips the run and the word after it"
    );
    editor.handle_key(key(KeyCode::Left, KeyModifiers::ALT), WRAP);
    assert_eq!(
        editor.cursor_cell(40).map(|(_, col)| col),
        Some(4),
        "word-left lands after the space, before \"two\""
    );
    editor.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL), WRAP);
    assert_eq!(
        editor.text(),
        "two",
        "delete-word-back kills \"one \" whole"
    );
}

#[test]
fn vertical_moves_never_land_mid_character_and_survive_inserts() {
    let mut editor = InputEditor::new();
    editor.set_text("aé\nab".to_string());
    editor.handle_key(plain(KeyCode::Up), WRAP);
    editor.handle_key(plain(KeyCode::Char('x')), WRAP);
    assert_eq!(
        editor.text(),
        "aéx\nab",
        "Up lands on a character boundary — past the é — and typing is safe"
    );

    let mut editor = InputEditor::new();
    editor.set_text("ab\naé".to_string());
    editor.handle_key(plain(KeyCode::Home), WRAP);
    editor.handle_key(plain(KeyCode::Left), WRAP);
    editor.handle_key(plain(KeyCode::Down), WRAP);
    editor.handle_key(plain(KeyCode::Char('x')), WRAP);
    assert_eq!(
        editor.text(),
        "ab\naéx",
        "Down clamps the carried column to the row's width — past the é — and typing is safe"
    );
}

#[test]
fn up_and_down_walk_the_wrapped_rows_of_a_single_line() {
    // A long line the composer wraps: vertical keys walk the
    // rendered rows, not history.
    let mut editor = InputEditor::new();
    type_str(&mut editor, "aaaa bbbb cccc dddd eeee");
    let width = 10;
    // "aaaa bbbb" / "cccc dddd" / "eeee"
    assert_eq!(editor.display_rows(width).len(), 3);
    assert_eq!(editor.cursor_cell(width), Some((2, 4)));
    editor.handle_key(plain(KeyCode::Up), width);
    assert_eq!(
        editor.cursor_cell(width),
        Some((1, 4)),
        "one row up, column kept"
    );
    editor.handle_key(plain(KeyCode::Up), width);
    assert_eq!(editor.cursor_cell(width), Some((0, 4)), "another row up");
    editor.handle_key(plain(KeyCode::Down), width);
    assert_eq!(editor.cursor_cell(width), Some((1, 4)), "back down a row");
}

#[test]
fn history_recalls_from_the_edges_of_the_wrap_grid() {
    let mut editor = InputEditor::new();
    assert_eq!(submit(&mut editor, "recorded"), "recorded");
    type_str(&mut editor, "aaaa bbbb cccc dddd eeee");
    let width = 10;
    editor.handle_key(plain(KeyCode::Up), width);
    editor.handle_key(plain(KeyCode::Up), width);
    assert_eq!(
        editor.cursor_cell(width),
        Some((0, 4)),
        "caret at the top row"
    );
    editor.handle_key(plain(KeyCode::Up), width);
    assert_eq!(
        editor.text(),
        "recorded",
        "Up at the top row recalls history"
    );
    editor.handle_key(plain(KeyCode::Down), width);
    assert_eq!(
        editor.text(),
        "aaaa bbbb cccc dddd eeee",
        "Down at the bottom row steps back and the draft returns"
    );
}

#[test]
fn ctrl_j_inserts_a_newline_on_every_terminal() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "one");
    editor.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL), WRAP);
    type_str(&mut editor, "two");
    assert_eq!(
        editor.text(),
        "one\ntwo",
        "Ctrl+J is the terminal's own newline key — raw mode reports it as control-j everywhere"
    );
}

#[test]
fn ctrl_m_submits_where_the_terminal_reports_the_modifier() {
    // Ctrl+M arrives under three spellings: plain Enter on legacy
    // terminals, Enter with the modifier on modifyOtherKeys ones,
    // and control-m on kitty-protocol ones. All must submit.
    let mut editor = InputEditor::new();
    type_str(&mut editor, "send me");
    assert_eq!(
        editor.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL), WRAP),
        InputAction::Submit("send me".to_string()),
        "the modifyOtherKeys spelling submits"
    );
    assert!(editor.text().is_empty(), "a submitted buffer clears");
    type_str(&mut editor, "again");
    assert_eq!(
        editor.handle_key(key(KeyCode::Char('m'), KeyModifiers::CONTROL), WRAP),
        InputAction::Submit("again".to_string()),
        "the kitty-protocol control-m spelling submits"
    );
    assert!(editor.text().is_empty(), "a submitted buffer clears");
}
