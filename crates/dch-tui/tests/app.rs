//! `TuiApp` shell tests — driven against ratatui's `TestBackend`, never
//! a real terminal: events go through `handle_event`, frames through
//! `render`.

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use dch_config::DchConfig;
use dch_tui::TuiApp;
use dch_tui::message::{ActiveTool, ContentBlock, TuiMessage};
use dch_tui::theme::Theme;
use loopctl::observer::LoopObserver as _;
use ratatui::Terminal;
use ratatui::backend::Backend as _;
use ratatui::backend::TestBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;

fn config_with_theme(name: &str) -> DchConfig {
    let mut config = DchConfig::default();
    config.display.theme = name.to_string();
    config.api.model = "testmodel".to_string();
    config
}

fn app() -> TuiApp {
    TuiApp::new(config_with_theme("dracula"))
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn plain(code: KeyCode) -> Event {
    key(code, KeyModifiers::NONE)
}

fn wheel_event(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
}

fn render_to_buffer(app: &mut TuiApp, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal
}

#[test]
fn new_constructs_with_empty_fresh_state() {
    let app = app();

    assert!(app.conversation().is_empty());
    assert!(app.input().is_empty());
    assert_eq!(app.scroll_offset(), 0);
    assert!(!app.is_quitting());

    let other = TuiApp::new(config_with_theme("dracula"));
    assert!(!Arc::ptr_eq(app.streaming_text(), other.streaming_text()));
    assert!(!Arc::ptr_eq(app.active_tools(), other.active_tools()));
    assert!(!Arc::ptr_eq(app.tokens(), other.tokens()));
    assert!(!Arc::ptr_eq(app.render_notify(), other.render_notify()));
    assert_eq!(app.theme.name, Theme::by_name("dracula").unwrap().name);
}

#[test]
fn seeded_messages_become_the_conversation_pinned_to_the_newest() {
    let mut app = app();
    let now = chrono::Utc::now();
    let restored = vec![
        dch_tui::TuiMessage::User {
            text: "earlier turn".to_string(),
            timestamp: now,
        },
        dch_tui::TuiMessage::Assistant {
            blocks: vec![dch_tui::ContentBlock::Text {
                text: "earlier answer".to_string(),
            }],
            timestamp: now,
            duration_ms: None,
        },
    ];
    // A scroll-up first, so the pin the seeding must restore is not
    // the constructor's default when it lands.
    app.handle_event(&plain(KeyCode::PageUp));
    assert!(!app.auto_scroll(), "the precondition detached the view");

    app.seed_messages(restored);

    assert_eq!(
        app.conversation().len(),
        2,
        "the restored transcript is the conversation"
    );
    assert!(
        matches!(app.conversation().first(), Some(dch_tui::TuiMessage::User { text, .. }) if text == "earlier turn"),
        "the messages land in order, oldest first"
    );
    assert!(
        app.auto_scroll() && app.scroll_offset() == 0,
        "a seeded view opens pinned to the newest line"
    );
}

#[test]
fn an_unknown_theme_falls_back_to_default() {
    let app = TuiApp::new(config_with_theme("no such theme"));
    assert_eq!(app.theme.name, Theme::default().name);
}

#[test]
fn a_known_theme_name_resolves() {
    let app = TuiApp::new(config_with_theme("nord"));
    assert_eq!(app.theme.name, "Nord");
}

#[test]
fn typing_echoes_into_the_input() {
    let mut app = app();
    for c in ['h', 'e', 'l', 'l', 'o'] {
        assert!(app.handle_event(&plain(KeyCode::Char(c))));
    }
    assert_eq!(app.input(), "hello");

    let terminal = render_to_buffer(&mut app, 80, 30);
    let buffer = terminal.backend().buffer();
    let row: String = (0..buffer.area.width)
        .map(|x| buffer[(x, 26)].symbol().to_string())
        .collect();
    assert!(row.contains("hello"), "input field shows the typed text");
}

#[test]
fn the_caret_follows_display_width_not_bytes() {
    let mut app = app();
    for c in ['h', 'é', 'x'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    let mut terminal = render_to_buffer(&mut app, 80, 30);
    let caret = terminal.get_cursor_position().unwrap();
    assert_eq!(
        caret.x, 5,
        "the padding columns plus three display columns of text"
    );

    let mut wide_app = TuiApp::new(config_with_theme("dracula"));
    for c in ['h', '😀'] {
        wide_app.handle_event(&plain(KeyCode::Char(c)));
    }
    let mut wide_terminal = render_to_buffer(&mut wide_app, 80, 30);
    let caret = wide_terminal.get_cursor_position().unwrap();
    assert_eq!(
        caret.x, 5,
        "the padding columns plus one column and a double-width glyph"
    );
}

#[test]
fn backspace_deletes_whole_characters() {
    let mut app = app();
    for c in ['h', 'é', 'l', 'l', 'o'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert_eq!(app.input(), "héllo");

    app.handle_event(&plain(KeyCode::Backspace));
    assert_eq!(app.input(), "héll");
    app.handle_event(&plain(KeyCode::Backspace));
    assert_eq!(app.input(), "hél");

    let mut accent_app = TuiApp::new(config_with_theme("dracula"));
    for c in ['h', 'é'] {
        accent_app.handle_event(&plain(KeyCode::Char(c)));
    }
    accent_app.handle_event(&plain(KeyCode::Backspace));
    assert_eq!(
        accent_app.input(),
        "h",
        "dropping the multi-byte é removes the whole character"
    );
}

#[test]
fn enter_submits_appends_and_clears() {
    let mut app = app();
    for c in ['h', 'i'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    app.handle_event(&plain(KeyCode::PageUp));
    assert!(app.handle_event(&plain(KeyCode::Enter)));

    assert!(app.input().is_empty());
    assert_eq!(app.conversation().len(), 1);
    match app.conversation().first() {
        Some(TuiMessage::User { text, .. }) => assert_eq!(text, "hi"),
        other => panic!("expected a user message, got {other:?}"),
    }
    assert_eq!(app.scroll_offset(), 0, "submit scrolls to bottom");

    for c in [' ', ' ', ' '] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        app.conversation().len(),
        1,
        "whitespace-only submit appends nothing"
    );
}

#[test]
fn enter_sends_the_submitted_text_through_the_channel() {
    let mut app = app();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    for c in ['h', 'i'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        rx.try_recv().unwrap(),
        "hi",
        "Enter forwards the submitted text to the agent driver"
    );

    for c in [' ', '\t'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert!(
        rx.try_recv().is_err(),
        "a whitespace-only submit sends nothing"
    );
}

#[test]
fn submissions_queue_in_order_while_the_driver_is_busy() {
    let mut app = app();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    for text in ["first task", "second task"] {
        for c in text.chars() {
            app.handle_event(&plain(KeyCode::Char(c)));
        }
        app.handle_event(&plain(KeyCode::Enter));
    }

    assert_eq!(
        rx.try_recv().unwrap(),
        "first task",
        "submits buffer while nothing drains the channel"
    );
    assert_eq!(
        rx.try_recv().unwrap(),
        "second task",
        "queued submits keep their arrival order"
    );
    assert!(rx.try_recv().is_err(), "two submits yield two messages");
}

#[test]
fn scroll_keys_adjust_without_underflow() {
    let mut app = app();
    assert!(app.handle_event(&plain(KeyCode::PageDown)));
    assert_eq!(app.scroll_offset(), 0, "scrolling down at bottom stays");

    app.handle_event(&plain(KeyCode::PageUp));
    assert_eq!(app.scroll_offset(), 10);
    app.handle_event(&plain(KeyCode::Down));
    assert_eq!(app.scroll_offset(), 9);
    app.handle_event(&plain(KeyCode::Up));
    assert_eq!(app.scroll_offset(), 10);

    for i in 0..60 {
        app.push_message(TuiMessage::User {
            text: format!("line {i}"),
            timestamp: chrono::Utc::now(),
        });
    }
    app.handle_event(&plain(KeyCode::PageDown));
    assert_eq!(
        app.scroll_offset(),
        0,
        "back at the bottom before rendering"
    );
    let at_bottom = render_to_buffer(&mut app, 40, 10);
    let bottom_view = view_text(&at_bottom, 40, 10);
    assert!(
        bottom_view.contains("line 59"),
        "the bottom view shows the newest message: {bottom_view:?}"
    );

    assert!(app.handle_event(&plain(KeyCode::PageUp)));
    let scrolled = render_to_buffer(&mut app, 40, 10);
    let scrolled_view = view_text(&scrolled, 40, 10);
    // Conversation is 4 rows at height 10 (spacer, padded two-row
    // field, status bar); PageUp lifts the window ten lines off the
    // bottom.
    assert!(
        !scrolled_view.contains("line 59") && scrolled_view.contains("line 47"),
        "scrolling up shifts the visible window: {scrolled_view:?}"
    );
}

fn view_text(terminal: &Terminal<TestBackend>, width: u16, height: u16) -> String {
    let buffer = terminal.backend().buffer();
    (0..height)
        .flat_map(|y| (0..width).map(move |x| buffer[(x, y)].symbol().to_string()))
        .collect()
}

#[test]
fn a_lone_ctrl_c_esc_and_ctrl_d_no_longer_quit() {
    // Quitting is a deliberate chord now: one Ctrl+C clears or arms,
    // Esc does nothing, and the old Ctrl-D exit is gone — none of
    // them takes the session down on a single slip.
    {
        let mut app = app();
        app.handle_event(&Event::Paste("draft".to_string()));
        assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(
            !app.is_quitting(),
            "the first press clears, it does not quit"
        );
        assert!(app.input().is_empty(), "the draft is gone");
    }

    {
        let mut app = app();
        app.handle_event(&plain(KeyCode::Esc));
        assert!(!app.is_quitting(), "Esc no longer quits");
        app.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(!app.is_quitting(), "Ctrl-D no longer exits");

        app.handle_event(&plain(KeyCode::Char('x')));
        assert!(!app.is_quitting());
    }
}

#[test]
fn the_second_ctrl_c_quits_and_any_other_key_disarms() {
    {
        let mut app = app();
        assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(
            !app.is_quitting(),
            "the first press on an empty buffer arms"
        );
        assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(app.is_quitting(), "the second press confirms the quit");
    }

    {
        let mut app = app();
        assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        app.handle_event(&plain(KeyCode::Char('x')));
        assert!(!app.is_quitting(), "typing between the presses disarms");
        assert!(
            app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            "the chord restarts from scratch"
        );
        assert!(!app.is_quitting(), "a fresh first press only re-arms");
    }
}

#[test]
fn clearing_the_draft_does_not_count_toward_the_quit_chord() {
    // Clearing is not consenting: the press that empties the
    // buffer arms nothing, so after a clear the exit still needs
    // its own two presses.
    let mut app = app();
    app.handle_event(&Event::Paste("draft".to_string()));
    assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert!(app.input().is_empty(), "the draft is cleared");
    assert!(!app.is_quitting(), "clearing does not quit");

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!app.is_quitting(), "the press after a clear only arms");

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.is_quitting(), "the second empty-buffer press confirms");
}

#[test]
fn seeded_tool_blocks_stay_expandable_after_a_resume() {
    // Retained input and output ride the session file, so a resumed
    // session's tool blocks seed the detail store: the disclosure
    // marker shows and the expansion carries the real command and
    // output across the restart.
    let mut app = app();
    let now = chrono::Utc::now();
    app.seed_messages(vec![TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            name: "Bash".to_string(),
            call_id: "call-saved".to_string(),
            input_preview: "make test".to_string(),
            success: true,
            elapsed_secs: 3.0,
            output_preview: "74 passed\n0 failed".to_string(),
            retained_input: "{\n  \"command\": \"make test\"\n}".to_string(),
        }],
        timestamp: now,
        duration_ms: None,
    }]);
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("Running: make test"))
        .unwrap_or(0);
    assert!(
        rows[row].contains("▸"),
        "a seeded block with retained detail shows its marker: {:?}",
        rows[row]
    );
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let open = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&open);
    assert!(
        rows.iter()
            .any(|r| r.contains("\"command\": \"make test\"")),
        "the seeded command expands: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("74 passed")),
        "the seeded output expands: {rows:?}"
    );
}

#[test]
fn ctrl_c_cancels_a_running_agent_without_touching_the_quit_chord() {
    // While a submission is in flight the press serves the run: a
    // draft clears first, then an empty-composer press cancels —
    // arming nothing, so exiting after the cancel takes its own two
    // presses.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let running = Arc::clone(&kept.agent_running);
    running.store(true, std::sync::atomic::Ordering::SeqCst);
    let cancels: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&cancels);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(move || {
        sink.lock().expect("sink").push(());
    }));

    app.handle_event(&Event::Paste("draft".to_string()));
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.input().is_empty(), "the draft clears first");
    assert_eq!(
        cancels.lock().expect("sink").len(),
        0,
        "the clear cancels nothing"
    );
    assert!(!app.is_quitting());

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        1,
        "the empty-composer press cancels the run"
    );
    assert!(!app.is_quitting(), "a cancel never quits");

    // The run lands; the quit chord resumes from scratch.
    running.store(false, std::sync::atomic::Ordering::SeqCst);
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!app.is_quitting(), "the first idle press only arms");
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.is_quitting(), "the second idle press quits");
}

#[test]
fn the_status_bar_shows_the_context_and_the_session_id() {
    // One status line, three facts: the model, the session's token
    // accounting labeled as context, and which session this is —
    // the id a later `--resume` wants, readable straight off the
    // bar.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    {
        let mut tokens = kept.tokens.lock().expect("tokens");
        tokens.cumulative_input = 4_494;
        tokens.cumulative_output = 156;
    }
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_session_id("f005d097-c4ce-477a-88c0-2fc2cec5a466".to_string());
    let bar = render_to_buffer(&mut app, 100, 24);
    let rows = row_texts(&bar);
    let status = rows
        .iter()
        .find(|r| r.contains("CTX:"))
        .expect("the bar renders");
    assert!(
        status.contains("CTX: 4650"),
        "the accounting reads as context: {status:?}"
    );
    assert!(
        status.contains("f005d097-c4ce-477a-88c0-2fc2cec5a466"),
        "the session id rides the bar: {status:?}"
    );
}

#[test]
fn the_notice_bar_carries_the_armed_quit_hint_above_the_composer() {
    // The hint lives on its own reserved row above the prompt box —
    // not the status bar — so the row can host glance-worthy
    // messages without shifting anything when one arrives.
    let mut app = app();
    let calm = render_to_buffer(&mut app, 80, 24);
    assert!(
        !view_text(&calm, 80, 24).contains("ctrl+c again"),
        "the hint is absent until the chord arms: {:?}",
        row_texts(&calm)
    );
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let armed = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&armed);
    let hint_row = rows
        .iter()
        .position(|row| row.contains("press ctrl+c again to quit"))
        .expect("the armed state shows its hint");
    // the reserved notice row sits between the conversation and the
    // spacer that tops the composer: the row below it is blank and
    // the composer's text starts below that
    assert!(
        rows.get(hint_row.saturating_add(1))
            .is_some_and(|r| r.trim().is_empty()),
        "the spacer row follows the notice: {rows:?}"
    );
    assert!(
        !rows.last().is_some_and(|r| r.contains("ctrl+c")),
        "the status bar no longer carries the hint: {:?}",
        rows.last()
    );
}

#[test]
fn a_cancelled_agent_notices_and_the_notice_expires() {
    // The cancel press lands its message on the notice row at once,
    // and the message leaves when its hold elapses — the tick that
    // retires it draws the blank row again.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(|| {}));
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let noticed = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&noticed)
            .iter()
            .any(|row| row.contains("Agent cancelled")),
        "the cancel shows on the notice row: {:?}",
        row_texts(&noticed)
    );
    let later = std::time::Instant::now() + std::time::Duration::from_secs(5);
    assert!(
        app.tick_wake(later),
        "the lap past the hold forces its redraw"
    );
    let blank = render_to_buffer(&mut app, 80, 24);
    assert!(
        !row_texts(&blank)
            .iter()
            .any(|row| row.contains("Agent cancelled")),
        "the notice leaves when its hold elapses"
    );
}

#[test]
fn ctrl_shift_c_copies_the_selection_without_quitting() {
    // The terminal-standard copy chord takes the live selection —
    // and never touches the quit chord, whatever the input holds.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&Event::Paste("draft".to_string()));
    assert!(
        app.handle_event(&key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        "the chord is handled"
    );
    assert!(!app.is_quitting(), "the copy chord never quits");
    assert_eq!(
        app.input(),
        "draft",
        "the copy chord does not clear the buffer either"
    );
    let copied = copied.lock().expect("sink").clone();
    assert_eq!(copied.as_slice(), ["cdef"], "the selection copies");
}

#[test]
fn key_releases_are_ignored() {
    let mut app = app();
    let release = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(!app.handle_event(&release));
    assert!(app.input().is_empty());

    let repeat = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));
    assert!(!app.handle_event(&repeat));
    assert!(app.input().is_empty());
}

#[test]
fn layout_shows_three_panes() {
    let mut app = app();
    let terminal = render_to_buffer(&mut app, 80, 30);
    let buffer = terminal.backend().buffer();

    let status_row: String = (0..80)
        .map(|x| buffer[(x, 29)].symbol().to_string())
        .collect();
    assert!(
        status_row.contains("testmodel"),
        "status bar names the model: {status_row:?}"
    );

    // A blank spacer row separates the conversation from the input
    // field; the field is the five rows above the status bar — a
    // pure background tint (one padding row, three text rows, one
    // padding row), no glyphs anywhere on its edge, no text of its
    // own.
    let spacer_row: String = (0..80)
        .map(|x| buffer[(x, 24)].symbol().to_string())
        .collect();
    assert!(
        spacer_row.trim().is_empty(),
        "a blank spacer row separates conversation from input: {spacer_row:?}"
    );
    let field_row = |y: u16| -> Vec<char> {
        (0..80)
            .map(|x| {
                buffer[(x, y)]
                    .symbol()
                    .to_string()
                    .chars()
                    .next()
                    .unwrap_or(' ')
            })
            .collect()
    };
    let top_pad = field_row(25);
    let bottom_pad = field_row(28);
    for row in [top_pad, bottom_pad] {
        assert!(
            row.iter().all(|c| *c == ' '),
            "the field's vertical padding rows are pure blank tint: {row:?}"
        );
    }
    // Square corners: the fill reaches every corner cell — the
    // padding row's tint and its corner cell are one continuous
    // rectangle, no clipping and no glyphs.
    assert_eq!(
        buffer[(0, 25)].bg,
        buffer[(2, 25)].bg,
        "the fill's corner cell carries the same tint as the padding row beside it"
    );
    let first_text = field_row(26);
    assert!(
        first_text.iter().take(2).all(|c| *c == ' '),
        "the field's first text row starts after horizontal padding: {:?}",
        first_text
    );
    for row in [field_row(25), field_row(26), field_row(28)] {
        assert!(
            !row.iter().any(|c| {
                matches!(
                    c,
                    '│' | '┌'
                        | '┐'
                        | '└'
                        | '┘'
                        | '─'
                        | '▗'
                        | '▖'
                        | '▝'
                        | '▘'
                        | '▐'
                        | '▌'
                )
            }),
            "the borderless field draws no box or corner glyphs: {row:?}"
        );
        assert!(
            !row.contains(&'⏎'),
            "the field carries no enter-key hint text: {row:?}"
        );
    }
}

#[test]
fn assistant_text_renders_through_markdown() {
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![
            ContentBlock::Text {
                text: "# Hi".to_string(),
            },
            ContentBlock::Text {
                text: "**bold**".to_string(),
            },
        ],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let buffer = terminal.backend().buffer();

    let heading_fg = Theme::by_name("dracula")
        .unwrap()
        .markdown
        .header1
        .fg
        .unwrap_or_default();
    let bold_modifier = Theme::by_name("dracula")
        .unwrap()
        .markdown
        .bold
        .add_modifier;
    let mut saw_heading = false;
    let mut saw_bold = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.symbol() == "H" && cell.fg == heading_fg {
                saw_heading = true;
            }
            if cell.symbol() == "b" && cell.modifier.contains(bold_modifier) {
                saw_bold = true;
            }
        }
    }
    assert!(saw_heading, "heading carries the header1 style");
    assert!(saw_bold, "bold text carries the bold modifier");
}

#[test]
fn restore_terminal_is_safe_without_a_session() {
    dch_tui::restore_terminal().expect("restore without init is a no-op");
    dch_tui::restore_terminal().expect("restore is idempotent");
}

#[test]
fn the_panic_hook_chains_and_unwinding_completes() {
    dch_tui::TerminalGuard::install_panic_hook();
    let result = std::panic::catch_unwind(|| panic!("hooked"));
    assert!(result.is_err(), "the hooked panic must still unwind");
}

#[test]
fn completed_tool_blocks_render_between_text() {
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![
            ContentBlock::Text {
                text: "alpha".to_string(),
            },
            ContentBlock::Tool {
                call_id: String::new(),
                name: "Read".to_string(),
                input_preview: "src/main.rs".to_string(),
                success: true,
                elapsed_secs: 0.42,
                output_preview: String::new(),
                retained_input: String::new(),
            },
            ContentBlock::Tool {
                call_id: String::new(),
                name: "Grep".to_string(),
                input_preview: "\"todo\"".to_string(),
                success: false,
                elapsed_secs: 61.4,
                output_preview: String::new(),
                retained_input: String::new(),
            },
            ContentBlock::Text {
                text: "omega".to_string(),
            },
        ],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let row_with = |needle: &str| rows.iter().position(|row| row.contains(needle));
    if let (Some(alpha), Some(read), Some(grep), Some(omega)) = (
        row_with("alpha"),
        row_with("✓ Read"),
        row_with("✗ Grep"),
        row_with("omega"),
    ) {
        assert!(
            alpha < read && read < grep && grep < omega,
            "tool lines sit between the assistant text blocks around them"
        );
    } else {
        panic!("expected alpha, both tool lines, and omega to render");
    }
    let joined = rows.join("\n");
    assert!(
        joined.contains("✓ Reading src/main.rs… (0.4s)"),
        "a successful tool renders its humanized summary and elapsed stamp: {joined:?}"
    );
    assert!(
        joined.contains("✗ Grep \"todo\" (1m1s)"),
        "a failed tool renders the error marker and a minute-scale stamp: {joined:?}"
    );

    let theme = Theme::by_name("dracula").unwrap();
    let buffer = terminal.backend().buffer();
    let mut success_styled = false;
    let mut failure_styled = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.symbol() == "✓" && cell.fg == theme.ui.status_success {
                success_styled = true;
            }
            if cell.symbol() == "✗" && cell.fg == theme.ui.status_error {
                failure_styled = true;
            }
        }
    }
    assert!(success_styled, "the success marker carries status_success");
    assert!(failure_styled, "the failure marker carries status_error");
}

#[test]
fn streaming_text_renders_when_nonempty() {
    let mut app = app();
    let empty = render_to_buffer(&mut app, 80, 30);
    assert!(
        !view_text(&empty, 80, 30).contains("partial"),
        "an empty streaming buffer renders nothing"
    );
    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str("partial reply");
    let streaming = render_to_buffer(&mut app, 80, 30);
    assert!(
        view_text(&streaming, 80, 30).contains("partial"),
        "a nonempty streaming buffer renders into the conversation pane"
    );
}

#[test]
fn a_stream_ending_on_an_unterminated_closing_fence_renders_it_once() {
    let mut app = app();
    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str("intro\n\n```rust\nlet x = 1;\n```");
    let closed = render_to_buffer(&mut app, 60, 20);
    let rows = row_texts(&closed);
    assert!(
        rows.iter().any(|row| row.contains("let x = 1;")),
        "the code inside the closed block still renders: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("```")),
        "the closer renders inside the framed block, never as a stray row: {rows:?}"
    );

    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push('\n');
    let terminated = render_to_buffer(&mut app, 60, 20);
    let rows = row_texts(&terminated);
    assert!(
        rows.iter().all(|row| !row.contains("```")),
        "the newline-terminated closer stays single-rendered: {rows:?}"
    );
}

#[test]
fn growth_after_a_frozen_closer_renders_without_waiting_for_a_newline() {
    let mut app = app();
    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str("intro\n\n```rust\nlet x = 1;\n```");
    let closed = render_to_buffer(&mut app, 60, 20);
    assert!(
        row_texts(&closed).iter().all(|row| !row.contains("```")),
        "the closer is consumed by the frozen block: {:?}",
        row_texts(&closed)
    );

    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str("tail-growth");
    let grown = render_to_buffer(&mut app, 60, 20);
    let rows = row_texts(&grown);
    assert!(
        rows.iter().any(|row| row.contains("tail-growth")),
        "a newline-free delta after the frozen closer renders on arrival: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("```")),
        "the growth does not resurrect the closer as a stray row: {rows:?}"
    );
}

#[test]
fn active_tools_render_as_running_lines() {
    let mut app = app();
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(ActiveTool {
            call_id: String::new(),
            name: "Grep".to_string(),
            input_summary: "\"todo\"".to_string(),
            start: std::time::Instant::now(),
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter()
            .any(|row| row.contains("⏳") && row.contains("Grep \"todo\"")),
        "in-flight tools render a running line: {rows:?}"
    );
}

fn row_texts(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn elapsed_stamps_round_once_before_splitting() {
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![
            ContentBlock::Tool {
                call_id: String::new(),
                name: "Read".to_string(),
                input_preview: "a.rs".to_string(),
                success: true,
                elapsed_secs: 90.0,
                output_preview: String::new(),
                retained_input: String::new(),
            },
            ContentBlock::Tool {
                call_id: String::new(),
                name: "Grep".to_string(),
                input_preview: "\"x\"".to_string(),
                success: true,
                elapsed_secs: 119.6,
                output_preview: String::new(),
                retained_input: String::new(),
            },
            ContentBlock::Tool {
                call_id: String::new(),
                name: "Bash".to_string(),
                input_preview: "true".to_string(),
                success: true,
                elapsed_secs: 59.6,
                output_preview: String::new(),
                retained_input: String::new(),
            },
        ],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let joined = rows.join("\n");
    assert!(
        joined.contains("Reading a.rs… (1m30s)"),
        "90s rounds to 1m30s, not 2m30s: {joined:?}"
    );
    assert!(
        joined.contains("Grep \"x\" (2m0s)"),
        "119.6s rounds once to 2m0s, not 1m60s or 2m60s: {joined:?}"
    );
    assert!(
        joined.contains("Running: true (1m0s)"),
        "59.6s crosses the minute boundary once rounded: {joined:?}"
    );
}

#[test]
fn an_app_built_from_observer_state_renders_observer_writes() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);

    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 0,
        delta: "live text".to_string(),
    });
    drop(observer);

    let terminal = render_to_buffer(&mut app, 80, 30);
    assert!(
        view_text(&terminal, 80, 30).contains("live text"),
        "observer writes reach an app built from the kept state"
    );
}

#[test]
fn a_finalized_reply_graduates_into_the_conversation() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    let mut streamed = TuiApp::from_observer_state(config_with_theme("dracula"), kept);

    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 0,
        delta: "the streamed".to_string(),
    });
    observer.on_response(&loopctl::observer::ResponseContext {
        turn: 0,
        text: "the streamed and final reply".to_string(),
        usage: None,
    });
    drop(observer);

    let terminal = render_to_buffer(&mut streamed, 80, 30);
    assert!(
        view_text(&terminal, 80, 30).contains("the streamed and final reply"),
        "the finalized reply renders in the conversation after the live buffer clears"
    );
    assert!(
        streamed.conversation().len() == 1,
        "graduation appends one assistant message"
    );
    assert!(
        matches!(
            streamed.conversation().first(),
            Some(dch_tui::TuiMessage::Assistant { .. })
        ),
        "the graduated message is an assistant message"
    );

    let (observer, kept) = dch_tui::TuiObserverState::new().into_observer();
    let mut unstreamed = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    observer.on_response(&loopctl::observer::ResponseContext {
        turn: 0,
        text: "a reply no delta announced".to_string(),
        usage: None,
    });
    drop(observer);
    let terminal = render_to_buffer(&mut unstreamed, 80, 30);
    assert!(
        view_text(&terminal, 80, 30).contains("a reply no delta announced"),
        "a non-streaming turn's reply reaches the conversation through its only copy"
    );
}

#[test]
fn completed_tools_graduate_inline_into_the_conversation() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());

    for index in 0..25 {
        kept.active_tools
            .lock()
            .expect("the tools lock")
            .push(dch_tui::ActiveTool {
                call_id: format!("call-{index}"),
                name: format!("tool-{index}"),
                input_summary: format!("{{\"n\":{index}}}"),
                start: std::time::Instant::now(),
            });
        observer.finish_tool(
            &format!("call-{index}"),
            &format!("tool-{index}"),
            false,
            std::time::Duration::from_millis(u64::try_from(index).unwrap_or(0)),
        );
    }
    drop(observer);

    let terminal = render_to_buffer(&mut app, 80, 30);
    let view = view_text(&terminal, 80, 30);
    assert!(
        view.contains("✓ tool-24"),
        "the newest completed tool renders inline: {view:?}"
    );
    assert_eq!(
        app.conversation().len(),
        25,
        "every completion graduates into the conversation — no display depth cap drops any"
    );
    assert!(
        kept.graduations.lock().expect("the queue lock").is_empty(),
        "the shared queue drains on redraw, so it cannot accumulate across a session"
    );
}

#[test]
fn poisoned_shared_buffers_still_graduate_through_render() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();

    observer.on_response(&loopctl::observer::ResponseContext {
        turn: 0,
        text: "reply under poison".to_string(),
        usage: None,
    });
    observer.finish_tool("call-1", "Grep", false, std::time::Duration::from_millis(3));

    let poisoned_graduations = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = kept.graduations.lock().expect("the queue lock");
        panic!("poison the queue lock");
    }));
    assert!(
        poisoned_graduations.is_err(),
        "the queue poisoning must unwind"
    );

    observer.on_response(&loopctl::observer::ResponseContext {
        turn: 1,
        text: "reply after poison".to_string(),
        usage: None,
    });
    observer.finish_tool("call-2", "Read", false, std::time::Duration::from_millis(4));
    drop(observer);

    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let view = view_text(&terminal, 80, 30);
    assert!(
        view.contains("reply under poison") && view.contains("reply after poison"),
        "the drain recovers through poison — both replies graduate: {view:?}"
    );
    assert!(
        view.contains("✓ Grep") && view.contains("✓ Read"),
        "poisoned tool results still graduate into the conversation: {view:?}"
    );
    let assistant_count = app
        .conversation()
        .iter()
        .filter(|message| matches!(message, dch_tui::TuiMessage::Assistant { .. }))
        .count();
    assert_eq!(
        assistant_count, 4,
        "both replies and both tool completions graduated through the poisoned locks"
    );
}

#[test]
fn a_turn_end_hook_receives_the_graduated_reply() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Reply("the finished reply".to_string()));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let snapshots: Arc<std::sync::Mutex<Snapshots>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&snapshots);
    app.set_turn_end_hook(Box::new(move |conversation| {
        snapshots_lock(&sink).push(conversation.to_vec());
    }));
    drop(render_to_buffer(&mut app, 80, 30));
    let seen = snapshots_lock(&snapshots);
    assert_eq!(seen.len(), 1, "the hook fired exactly once");
    assert!(
        seen.first()
            .expect("the one snapshot")
            .iter()
            .any(|message| matches!(
                message,
                TuiMessage::Assistant { blocks, .. }
                    if blocks.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text } if text == "the finished reply"
                    ))
            )),
        "the snapshot carries the graduated reply"
    );
}

#[test]
fn a_turn_end_hook_fires_for_surfaced_errors() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.errors
        .lock()
        .expect("the errors lock")
        .push("provider unreachable".to_string());
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let snapshots: Arc<std::sync::Mutex<Snapshots>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&snapshots);
    app.set_turn_end_hook(Box::new(move |conversation| {
        snapshots_lock(&sink).push(conversation.to_vec());
    }));
    drop(render_to_buffer(&mut app, 80, 30));
    let seen = snapshots_lock(&snapshots);
    assert_eq!(seen.len(), 1, "a surfaced failure is a turn end too");
    assert!(
        seen.first()
            .expect("the one snapshot")
            .iter()
            .any(|message| matches!(
                message,
                TuiMessage::Error { text, .. } if text == "provider unreachable"
            )),
        "the snapshot carries the failure"
    );
}

#[test]
fn a_quiet_frame_does_not_fire_the_turn_end_hook() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());
    let snapshots: Arc<std::sync::Mutex<Snapshots>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&snapshots);
    app.set_turn_end_hook(Box::new(move |conversation| {
        snapshots_lock(&sink).push(conversation.to_vec());
    }));
    drop(render_to_buffer(&mut app, 80, 30));
    assert!(
        snapshots_lock(&snapshots).is_empty(),
        "a frame that graduates nothing fires nothing"
    );

    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Reply("a late reply".to_string()));
    drop(render_to_buffer(&mut app, 80, 30));
    assert_eq!(
        snapshots_lock(&snapshots).len(),
        1,
        "the next graduation fires the hook"
    );
}

/// Conversation snapshots captured by a turn-end hook.
type Snapshots = Vec<Vec<TuiMessage>>;

/// Lock the hook sink for assertions.
fn snapshots_lock(sink: &Arc<std::sync::Mutex<Snapshots>>) -> std::sync::MutexGuard<'_, Snapshots> {
    sink.lock().expect("the snapshots lock")
}

#[test]
fn run_failures_drain_into_the_conversation_as_errors() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.errors
        .lock()
        .expect("the errors lock")
        .push("provider unreachable".to_string());
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());

    let terminal = render_to_buffer(&mut app, 80, 30);
    let view = view_text(&terminal, 80, 30);
    assert!(
        view.contains("provider unreachable"),
        "the failure surfaces as a conversation row: {view:?}"
    );
    assert!(
        app.conversation().len() == 1,
        "the drained failure becomes one message"
    );
    assert!(
        kept.errors.lock().expect("the errors lock").is_empty(),
        "the display drains the error buffer on redraw"
    );

    let theme = Theme::by_name("dracula").unwrap();
    let buffer = terminal.backend().buffer();
    let mut error_styled = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.symbol() == "p" && cell.fg == theme.ui.status_error {
                error_styled = true;
            }
        }
    }
    assert!(error_styled, "the error row carries status_error");
}

#[test]
fn long_error_messages_wrap_at_the_pane_width() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut text = String::from("API error: ");
    for i in 0..40 {
        text.push_str("chunk");
        text.push_str(&i.to_string());
        text.push(' ');
    }
    text.push_str("ENDMARK");
    kept.errors.lock().expect("the errors lock").push(text);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);

    let terminal = render_to_buffer(&mut app, 40, 24);
    let view = view_text(&terminal, 40, 24);
    assert!(
        view.contains("ENDMARK"),
        "the wrapped tail of a long error stays visible instead of clipping: {view:?}"
    );
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// Count the reversed-video cells on one rendered row.
fn reversed_cells_on_row(terminal: &Terminal<TestBackend>, row: u16) -> usize {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.width)
        .filter(|&x| buffer[(x, row)].style().add_modifier == ratatui::style::Modifier::REVERSED)
        .count()
}

/// Count every reversed-video cell in a rendered frame.
fn reversed_cells(terminal: &Terminal<TestBackend>) -> usize {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|row| reversed_cells_on_row(terminal, row))
        .sum()
}

#[test]
fn a_drag_selects_characters_and_copies_silently() {
    // The Helix-style contract: a press-drag-release over the
    // conversation selects exactly the covered characters — partial
    // lines included — and on release the text goes to the
    // clipboard. Nothing is appended to the conversation.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    app.push_message(TuiMessage::User {
        text: "klmnopqrst".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let _ = render_to_buffer(&mut app, 80, 24);
    let before = app.conversation().len();
    // how user text renders: find its row and leading offset
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);

    let press = mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    app.handle_event(&press);
    let drag = mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row + 1).unwrap_or(0),
    );
    app.handle_event(&drag);
    let up = mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row + 1).unwrap_or(0),
    );
    app.handle_event(&up);

    let copied = copied.lock().expect("sink").clone();
    assert_eq!(copied.len(), 1, "exactly one silent copy");
    let expected = "cdefghij\nklmnop".to_string();
    assert_eq!(
        copied[0], expected,
        "character-granular coverage, partial first and last lines"
    );
    assert_eq!(
        app.conversation().len(),
        before,
        "the copy is silent — nothing joins the conversation"
    );
}

#[test]
fn a_drag_shows_the_highlight_before_release() {
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    let _ = render_to_buffer(&mut app, 80, 24);
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let selecting = render_to_buffer(&mut app, 80, 24);
    let buffer = selecting.backend().buffer();
    let reversed = (0..80u16)
        .filter(|&x| {
            buffer[(x, u16::try_from(row).unwrap_or(0))]
                .style()
                .add_modifier
                == ratatui::style::Modifier::REVERSED
        })
        .count();
    assert!(reversed >= 1, "the anchored cell highlights immediately");
}

#[test]
fn the_transparent_theme_underlines_its_composer() {
    // The paintless chrome's composer marking: hairline rules the
    // terminal draws itself — underlined spacer above, underlined
    // last row below — thin and solid on every terminal, glyphs and
    // paint nowhere.
    let mut ruled = TuiApp::new(config_with_theme("transparent"));
    let frame = render_to_buffer(&mut ruled, 80, 24);
    let buffer = frame.backend().buffer();
    for x in [1u16, 40, 78] {
        let top = buffer[(x, 18)].style();
        assert!(
            top.add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "the spacer row carries the top rule"
        );
        assert_eq!(top.fg, Some(ratatui::style::Color::Indexed(8)));
        let bottom = buffer[(x, 21)].style();
        assert!(
            bottom
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "the composer's last row carries the bottom rule"
        );
        assert_eq!(bottom.fg, Some(ratatui::style::Color::Indexed(8)));
    }
    for cell in [(40u16, 19u16), (40, 20)] {
        let style = buffer[cell].style();
        assert_eq!(
            style.bg,
            Some(ratatui::style::Color::Reset),
            "the composer stays on the terminal's own background"
        );
        assert!(
            !style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
    }
    assert_eq!(buffer[(1, 19)].symbol(), " ", "no border glyphs remain");

    // opaque themes keep their painted, borderless composer
    let mut plain = app();
    let plain_frame = render_to_buffer(&mut plain, 80, 24);
    let buffer = plain_frame.backend().buffer();
    let style = buffer[(40, 20)].style();
    assert_eq!(style.bg, Some(ratatui::style::Color::Rgb(68, 71, 90)));
    assert_eq!(buffer[(1, 19)].symbol(), " ");
}

#[test]
fn a_press_in_the_composer_places_the_caret_where_it_landed() {
    // 24 rows put the composer pane on rows 19..=22 with its text
    // window on row 20, text starting two columns in.
    let mut app = app();
    app.handle_event(&Event::Paste("hello world".to_string()));
    let mut first = render_to_buffer(&mut app, 80, 24);
    let caret = first.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (2 + 11, 20),
        "after the paste the caret sits at the text's end"
    );

    // a press between the two ls lands the caret mid-word
    assert!(app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2 + 4,
        20,
    )));
    let mut clicked = render_to_buffer(&mut app, 80, 24);
    let caret = clicked.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (6, 20),
        "the caret lands on the pressed cell"
    );

    // a press far past the text clamps to the row's end
    assert!(app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        70,
        21,
    )));
    let mut clamped = render_to_buffer(&mut app, 80, 24);
    let caret = clamped.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (2 + 11, 20),
        "the padding row and the empty column clamp onto the text"
    );
}

#[test]
fn a_released_selection_stays_highlighted_until_the_next_press() {
    // Terminals keep a native selection up after the release; the
    // in-app selection does the same — the highlight survives the
    // copy, and a press-release without travel clears it.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));

    let released = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&released, u16::try_from(row).unwrap_or(0)) >= 4,
        "the highlight stays up after the release"
    );
    assert_eq!(
        copied.lock().expect("sink").len(),
        1,
        "the release copied exactly once"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let cleared = render_to_buffer(&mut app, 80, 24);
    assert_eq!(
        reversed_cells_on_row(&cleared, u16::try_from(row).unwrap_or(0)),
        0,
        "a click without travel clears the selection"
    );
    assert_eq!(
        copied.lock().expect("sink").len(),
        1,
        "the clearing click copies nothing"
    );
}

#[test]
fn a_drag_running_past_the_bottom_edge_scrolls_with_the_selection() {
    // 24 rows put the conversation pane on rows 0..=16.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);

    for _ in 0..5 {
        app.handle_event(&wheel_event(MouseEventKind::ScrollUp));
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    assert_eq!(app.scroll_offset(), 5);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        3,
    ));
    assert!(
        app.handle_event(&mouse_event(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            2,
            16,
        )),
        "the edge drag reports a redraw"
    );
    assert_eq!(
        app.scroll_offset(),
        4,
        "sitting on the pane's bottom line steps the view down one line"
    );
    let scrolled = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&scrolled, 16) >= 1,
        "the selection reaches the pane's bottom line as the view moves"
    );
}

#[test]
fn a_parked_edge_drag_keeps_scrolling_on_the_tick_until_the_document_ends() {
    // A pointer pushed against the pane's edge goes quiet — the
    // terminal reports a drag only while the reported cell changes —
    // so the tick carries an edge push between movements, and the
    // document's own end bounds the walk.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    for _ in 0..5 {
        app.handle_event(&wheel_event(MouseEventKind::ScrollUp));
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        2,
        16,
    ));
    assert_eq!(
        app.scroll_offset(),
        4,
        "the movement report itself releases the first line"
    );

    // the pointer parks: ticks carry the push the rest of the way,
    // with the frame between as the run loop draws it
    let mut ticks = 0;
    while app.tick_wake(std::time::Instant::now()) {
        ticks += 1;
        assert!(
            ticks < 10,
            "the parked push reaches the bottom in bounded ticks"
        );
        let _ = render_to_buffer(&mut app, 80, 24);
    }
    assert_eq!(
        app.scroll_offset(),
        0,
        "the parked push walks the view back to the bottom"
    );
    assert!(app.auto_scroll(), "reaching the bottom re-arms stickiness");
    let settled = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&settled, 16) >= 1,
        "the selection follows the edge all the way down"
    );
    for _ in 0..6 {
        // the blink phase may flip on any of these; the claim is
        // that nothing scrolls
        let _woke = app.tick_wake(std::time::Instant::now());
    }
    assert_eq!(
        app.scroll_offset(),
        0,
        "parked at the bottom with nothing left to move, ticks move nothing"
    );
}

#[test]
fn buttonless_motion_ends_a_parked_edge_drag() {
    // A release delivered outside the terminal's view never arrives
    // as an Up event; motion with no button held can only be sent
    // when no button is down, so it ends the push.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        2,
        0,
    ));
    assert_eq!(app.scroll_offset(), 1);
    assert!(app.tick_wake(std::time::Instant::now()));
    assert_eq!(app.scroll_offset(), 2, "the parked push steps on the tick");
    let _ = render_to_buffer(&mut app, 80, 24);

    // the lost release: no Up ever arrives; the next buttonless
    // motion ends the drag and the push stops mid-document
    assert!(
        !app.handle_event(&mouse_event(MouseEventKind::Moved, 2, 2)),
        "the healing motion redraws nothing"
    );
    assert!(
        !app.tick_wake(std::time::Instant::now()),
        "a healed drag claims no tick"
    );
    assert_eq!(
        app.scroll_offset(),
        2,
        "the push is over — no further scrolling"
    );
}

#[test]
fn a_drag_running_past_the_top_edge_scrolls_upward() {
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        5,
    ));
    assert!(
        app.handle_event(&mouse_event(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            2,
            0,
        )),
        "the edge drag reports a redraw"
    );
    assert_eq!(
        app.scroll_offset(),
        1,
        "sitting on the pane's top line steps the view up one line"
    );
    assert!(!app.auto_scroll(), "the upward edge drag detaches");
    let scrolled = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&scrolled, 0) >= 1,
        "the selection reaches the pane's top line as the view moves"
    );
    // the push parks: ticks carry it upward, bounded by the
    // document's first line — 30 lines over an 18-row pane end at
    // offset 12
    let mut ticks = 0;
    while app.tick_wake(std::time::Instant::now()) {
        ticks += 1;
        assert!(
            ticks < 20,
            "the parked push reaches the document top in bounded ticks"
        );
        let _ = render_to_buffer(&mut app, 80, 24);
    }
    assert_eq!(app.scroll_offset(), 13, "the walk ends pinned at the top");
    let settled = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&settled, 0) >= 1,
        "the selection follows the edge all the way up"
    );
}

#[test]
fn shift_arrows_extend_and_shrink_a_released_selection() {
    // The keyboard half of the editor convention: with a selection
    // up, Shift and an arrow moves the head while the anchor stays —
    // stepping away extends, stepping back shrinks — and each step
    // refreshes the clipboard copy the release made.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    app.push_message(TuiMessage::User {
        text: "klmnopqrst".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row + 1).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row + 1).unwrap_or(0),
    ));

    // right extends the head's line; up pulls the head back a line,
    // shrinking the selection onto its first line; left shrinks
    // within the line
    app.handle_event(&key(KeyCode::Right, KeyModifiers::SHIFT));
    app.handle_event(&key(KeyCode::Up, KeyModifiers::SHIFT));
    app.handle_event(&key(KeyCode::Left, KeyModifiers::SHIFT));

    let copied = copied.lock().expect("sink").clone();
    assert_eq!(
        copied,
        vec![
            "cdefghij\nklmnop".to_string(),
            "cdefghij\nklmnopq".to_string(),
            "cdefg".to_string(),
            "cdef".to_string(),
        ],
        "release copies, then each Shift-arrow step replaces the copy"
    );
}

#[test]
fn shift_arrows_walk_the_view_to_follow_the_head() {
    // A keyboard-walked head crosses the viewport under its own
    // power; the view follows and the walk ends pinned at the
    // document's last line.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    for _ in 0..5 {
        app.handle_event(&wheel_event(MouseEventKind::ScrollUp));
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        2,
        4,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        2,
        4,
    ));
    assert_eq!(app.scroll_offset(), 5);

    for _ in 0..20 {
        app.handle_event(&key(KeyCode::Down, KeyModifiers::SHIFT));
        let _ = render_to_buffer(&mut app, 80, 24);
    }
    assert_eq!(
        app.scroll_offset(),
        0,
        "the view followed the head all the way down"
    );
    assert!(app.auto_scroll(), "the walk re-arms at the bottom");
    let settled = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&settled, 16) >= 1,
        "the head sits on the document's last, visible line"
    );
}

#[test]
fn plain_arrows_still_scroll_while_a_selection_is_up() {
    // The user's standing arrow behavior is untouched: with the
    // input empty and nothing recallable, plain arrows scroll the
    // transcript — a selection up changes nothing about that.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
        });
    }
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));
    let _ = render_to_buffer(&mut app, 80, 24);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        2,
        4,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        2,
        4,
    ));
    assert_eq!(copied.lock().expect("sink").len(), 1);

    assert!(app.handle_event(&plain(KeyCode::Up)));
    assert_eq!(app.scroll_offset(), 1, "plain Up scrolls a line");
    assert!(app.handle_event(&plain(KeyCode::Down)));
    assert_eq!(app.scroll_offset(), 0, "plain Down scrolls back");
    assert_eq!(
        copied.lock().expect("sink").len(),
        1,
        "plain arrows copy nothing"
    );
}

#[test]
fn a_rebuilt_line_space_forfeits_the_selection() {
    // The selection's indexes name the rendered line space; a
    // verbosity switch or a resize rebuilds that space under them,
    // and stale numbers would highlight — and copy — text the user
    // never chose. A re-flowed document forfeits the selection.
    let mut app = app();
    let now = chrono::Utc::now();
    for text in ["abcdefghij", "klmnopqrst"] {
        app.push_message(TuiMessage::User {
            text: text.to_string(),
            timestamp: now,
        });
    }
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let selected = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells(&selected) >= 1,
        "sanity: the selection highlights before the rebuild"
    );

    app.handle_event(&plain(KeyCode::F(2)));
    let rebuilt = render_to_buffer(&mut app, 80, 24);
    assert_eq!(
        reversed_cells(&rebuilt),
        0,
        "a verbosity switch rebuilds the line space and forfeits the selection"
    );

    // a re-selected highlight survives renders and dies on the re-wrap
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let reselected = render_to_buffer(&mut app, 80, 24);
    assert!(reversed_cells(&reselected) >= 1, "sanity: re-selected");
    let rewrapped = render_to_buffer(&mut app, 60, 24);
    assert_eq!(
        reversed_cells(&rewrapped),
        0,
        "a re-wrapping resize forfeits the selection"
    );
}

#[test]
fn a_press_on_a_running_tool_row_anchors_on_the_selectable_transcript() {
    // Tool rows render below the transcript but never join the
    // selection's line space: a press on one — or on the empty pane
    // under short content — clamps onto the last line a copy can
    // walk, so the highlight never spans rows the copy cannot read.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(ActiveTool {
            call_id: String::new(),
            name: "Grep".to_string(),
            input_summary: "\"todo\"".to_string(),
            start: std::time::Instant::now(),
        });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    let tool_row = rows.iter().position(|r| r.contains("Grep")).unwrap_or(0);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(tool_row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 3).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 3).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));

    let copied = copied.lock().expect("sink").clone();
    assert_eq!(copied.len(), 1, "exactly one silent copy");
    assert_eq!(
        copied[0], "abcd",
        "the press clamps onto the message line, anchoring where a copy can walk"
    );
    let selected = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&selected, u16::try_from(row).unwrap_or(0)) >= 1,
        "the highlight covers the transcript text"
    );
    assert_eq!(
        reversed_cells_on_row(&selected, u16::try_from(tool_row).unwrap_or(0)),
        0,
        "the highlight never reaches the tool row"
    );
}

#[test]
fn a_press_with_nothing_selectable_starts_no_selection() {
    // With the transcript empty and only a tool running, the pane
    // offers no selectable line: a press there creates no anchor,
    // so nothing highlights and a later drag has nothing to grow.
    let mut app = app();
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(ActiveTool {
            call_id: String::new(),
            name: "Grep".to_string(),
            input_summary: "\"todo\"".to_string(),
            start: std::time::Instant::now(),
        });
    let _ = render_to_buffer(&mut app, 80, 24);
    let press = mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        2,
    );
    assert!(
        !app.handle_event(&press),
        "a press over unselectable rows creates no selection"
    );
    let frame = render_to_buffer(&mut app, 80, 24);
    assert_eq!(reversed_cells(&frame), 0, "nothing highlights");
}

#[test]
fn a_streaming_delta_forfeits_a_selection_reaching_into_the_live_region() {
    // The live region re-renders on every delta — re-wrapped,
    // re-numbered — so a selection stored against its old lines
    // would highlight and copy whatever moved under the indexes.
    // Like a rebuilt conversation, a re-flowed stream forfeits the
    // selection.
    let mut app = app();
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));
    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str("streaming reply");
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("streaming reply"))
        .unwrap_or(0);
    let col = rows[row].find("streaming reply").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 4).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let selected = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells(&selected) >= 1,
        "sanity: the live text highlights while it holds still"
    );

    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str(" grows");
    let grown = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&grown)
            .iter()
            .any(|r| r.contains("streaming reply grows")),
        "the delta renders: {:?}",
        row_texts(&grown)
    );
    assert_eq!(
        reversed_cells(&grown),
        0,
        "a re-rendered live region forfeits the selection stored against its old lines"
    );
}

#[test]
fn a_streaming_delta_preserves_a_selection_in_the_settled_transcript() {
    // The settled transcript keeps its line numbering while the
    // stream grows below it, so a selection made there survives the
    // re-render — the forfeit is scoped to what actually moved.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));
    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str("streaming reply");
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 3).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 3).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let copied = copied.lock().expect("sink").clone();
    assert_eq!(copied.len(), 1, "one copy of the settled text");
    assert_eq!(copied[0], "abcd", "the settled text copies");

    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str(" grows");
    let grown = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&grown);
    let live_row = rows
        .iter()
        .position(|r| r.contains("streaming reply grows"))
        .unwrap_or(0);
    assert!(
        reversed_cells_on_row(&grown, u16::try_from(row).unwrap_or(0)) >= 1,
        "the settled selection survives the stream's re-render"
    );
    assert_eq!(
        reversed_cells_on_row(&grown, u16::try_from(live_row).unwrap_or(0)),
        0,
        "the re-rendered live line is not highlighted"
    );
}

#[test]
fn a_release_over_blank_cells_copies_nothing() {
    // The press column travels raw, so a drag can sit entirely in
    // the blank cells right of a short line: distinct endpoints, no
    // covered text. Overwriting the clipboard with that emptiness
    // would wipe it for nothing — the release applies the same
    // guard the keyboard walk already does. Spanning two such rows
    // must not sneak past the guard as a lone row separator.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij\n\nklmnopqrst".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    let krow = rows
        .iter()
        .position(|r| r.contains("klmnopqrst"))
        .unwrap_or(0);
    let blank_row = krow.saturating_sub(1);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 20).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 30).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 30).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    assert!(
        copied.lock().expect("sink").is_empty(),
        "a span over blank cells never replaces the clipboard"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 20).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 30).unwrap_or(0),
        u16::try_from(blank_row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 30).unwrap_or(0),
        u16::try_from(blank_row).unwrap_or(0),
    ));
    assert!(
        copied.lock().expect("sink").is_empty(),
        "a two-row span over blank cells copies nothing — not even the row separator"
    );
}

#[test]
fn combining_marks_carry_with_their_base_and_line_up_with_the_rendered_cells() {
    // A combining mark paints no cell of its own — "e" + acute + "x"
    // renders two cells — so the selection walks clusters, not
    // characters: the cell that shows "x" copies "x", and a selected
    // base carries its mark along.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "e\u{301}x".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows.iter().position(|r| r.contains('x')).unwrap_or(0);
    let col = rows[row].find('e').unwrap_or(0);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 1).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let first = copied.lock().expect("sink").clone();
    assert_eq!(
        first.as_slice(),
        ["x"],
        "the second rendered cell copies x, not the mark"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 1).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 1).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    let again = copied.lock().expect("sink").clone();
    assert_eq!(
        again.get(1).map(String::as_str),
        Some("e\u{301}x"),
        "both cells copy with the mark riding its base — no phantom third cell"
    );
}

#[test]
fn a_press_outside_the_conversation_retires_the_selection_without_recopying() {
    // The selection's release copies; a press that anchors nothing —
    // into the composer, the status bar, or off the pane — starts a
    // new interaction instead. The stale selection is retired there,
    // so its release never re-copies a span over whatever the user
    // put on the clipboard in between.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    assert_eq!(
        copied.lock().expect("sink").len(),
        1,
        "the drag copied once"
    );

    let status_row = 23;
    assert!(
        app.handle_event(&mouse_event(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            status_row,
        )),
        "retiring the selection is itself a redraw — the highlight leaves now"
    );
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
        status_row,
    ));
    assert_eq!(
        copied.lock().expect("sink").len(),
        1,
        "the off-pane release copies nothing"
    );
    let after = render_to_buffer(&mut app, 80, 24);
    assert_eq!(
        reversed_cells(&after),
        0,
        "the highlight retired with the press"
    );
}

#[test]
fn an_interior_blank_line_stays_in_a_multi_row_copy() {
    // The covered-any flag only suppresses spans that covered no
    // characters anywhere; a genuine selection crossing a blank
    // line copies its text intact, separator rows included — they
    // are content, not emptiness.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij\n\nklmnopqrst".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[row].find("abcdefghij").unwrap_or(0);
    let krow = rows
        .iter()
        .position(|r| r.contains("klmnopqrst"))
        .unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(krow).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(krow).unwrap_or(0),
    ));
    let copied = copied.lock().expect("sink").clone();
    assert_eq!(copied.len(), 1, "the spanning selection copies");
    assert_eq!(
        copied[0], "cdefghij\n\nklmnop",
        "both partial lines and the blank row between them, verbatim"
    );
}

#[test]
fn a_selection_landing_on_a_wide_characters_second_cell_takes_it() {
    // A wide character's ink spans two cells, and a selection
    // starting on the second still covers half of it. Both the
    // highlight and the copy take the character, matching what a
    // terminal's native selection does with partially covered ink.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "日x".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows.iter().position(|r| r.contains("日x")).unwrap_or(0);
    let col = rows[row].find("日x").unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 1).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));

    let copied = copied.lock().expect("sink").clone();
    assert_eq!(copied.len(), 1, "one copy");
    assert_eq!(
        copied[0], "日x",
        "the half-covered wide character joins the copy"
    );
    let selected = render_to_buffer(&mut app, 80, 24);
    assert!(
        reversed_cells_on_row(&selected, u16::try_from(row).unwrap_or(0)) >= 2,
        "the wide character's own cell highlights alongside the covered one"
    );
}

#[test]
fn mouse_traffic_leaves_the_caret_blink_alone() {
    // Reading is not typing: wheel scrolls, hovers, and clicks in
    // the conversation keep arriving while the user reads, and each
    // one used to restart the blink phase — the caret sat solid
    // through a whole mouse-driven session. Only keyboard input
    // resolidifies the caret now.
    let mut app = app();
    let _ = render_to_buffer(&mut app, 80, 24);
    for _ in 0..4 {
        assert!(!app.tick_wake(std::time::Instant::now()));
    }
    assert!(
        app.tick_wake(std::time::Instant::now()),
        "the fifth tick flips the caret to its dark phase"
    );
    assert!(
        !app.caret_visible(),
        "sanity: the caret is dark before the mouse traffic"
    );

    app.handle_event(&mouse_event(MouseEventKind::ScrollUp, 5, 5));
    app.handle_event(&mouse_event(MouseEventKind::Moved, 6, 6));
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        7,
        7,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        7,
        7,
    ));
    assert!(
        !app.caret_visible(),
        "wheel, motion, and clicks leave the dark phase alone"
    );

    app.handle_event(&plain(KeyCode::Char('x')));
    assert!(app.caret_visible(), "typing still resolidifies the caret");
}

#[test]
fn the_caret_blinks_on_the_tick_and_input_resolidifies_it() {
    // The terminal's blinking-cursor request is often ignored or
    // preference-gated, so the app owns the blink: half a second
    // visible, half a second gone, restarted by any input.
    let mut app = app();
    app.handle_event(&Event::Paste("hi".to_string()));
    let _ = render_to_buffer(&mut app, 80, 24);

    for _ in 0..4 {
        assert!(
            !app.tick_wake(std::time::Instant::now()),
            "ticks between flips draw nothing"
        );
    }
    assert!(
        app.tick_wake(std::time::Instant::now()),
        "the fifth tick flips the caret to its dark phase"
    );
    for _ in 0..4 {
        assert!(!app.tick_wake(std::time::Instant::now()));
    }
    assert!(
        app.tick_wake(std::time::Instant::now()),
        "the next fifth tick flips it back"
    );

    // input during the dark phase resolidifies the caret and
    // restarts the count
    app.handle_event(&plain(KeyCode::Char('x')));
    for _ in 0..4 {
        assert!(!app.tick_wake(std::time::Instant::now()));
    }
    assert!(app.tick_wake(std::time::Instant::now()));
}

#[test]
fn the_mouse_wheel_scrolls_one_line_per_event() {
    let mut app = app();
    assert!(app.handle_event(&wheel_event(MouseEventKind::ScrollUp)));
    assert_eq!(app.scroll_offset(), 1);
    assert!(!app.auto_scroll(), "wheel up detaches stickiness");
    assert!(app.handle_event(&wheel_event(MouseEventKind::ScrollDown)));
    assert_eq!(app.scroll_offset(), 0, "one wheel down clears one wheel up");
    assert!(app.auto_scroll(), "landing at the bottom re-arms");
}

#[test]
fn a_paste_event_lands_atomically_in_the_editor() {
    let mut app = app();
    assert!(app.handle_event(&Event::Paste("line one\nline two".to_string())));
    assert_eq!(app.input(), "line one\nline two");
}

#[test]
fn a_submit_increments_the_shared_queue_depth() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    for c in "hi".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(rx.try_recv().unwrap(), "hi");
    assert_eq!(
        kept.queued.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the send bumps the depth the driver will decrement"
    );
}

#[test]
fn the_input_field_grows_with_its_buffer_up_to_the_cap() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);

    // empty composer: one text row — the pane is three rows
    // (padding, text, padding), rows 19..21, and the field is blank.
    let empty = render_to_buffer(&mut app, 40, 24);
    let rows = row_texts(&empty);
    assert!(
        !rows[20].contains("enter"),
        "the empty field shows its single blank row: {}",
        rows[20]
    );

    // two lines: the pane grows to four rows and the conversation
    // yields one — the composer now starts a row higher (18) with
    // its text on 19 and 20
    app.handle_event(&Event::Paste("one\ntwo".to_string()));
    let two = render_to_buffer(&mut app, 40, 24);
    let rows = row_texts(&two);
    assert!(
        rows[19].contains("one") && rows[20].contains("two"),
        "a two-line buffer shows both lines: {} / {}",
        rows[19],
        rows[20]
    );
    assert!(
        !rows[23].contains("2/2"),
        "a buffer that fits carries no line-position tag: {}",
        rows[23]
    );

    // five lines: the pane caps at three text rows, the window keeps
    // the caret's tail visible, and the tag reports the overflow
    app.handle_event(&Event::Paste("\nfour\nfive\nsix".to_string()));
    let capped = render_to_buffer(&mut app, 40, 24);
    let rows = row_texts(&capped);
    assert!(
        rows[20].contains("six"),
        "the window keeps the caret's line visible: {}",
        rows[20]
    );
    assert!(
        rows[23].contains("5/5") || rows[23].contains("5/6"),
        "a buffer past the cap carries the line-position tag: {}",
        rows[23]
    );
}

#[test]
fn an_exactly_filled_line_stays_visible_with_its_caret_at_the_end() {
    let mut app = app();
    for c in "abcdefgh".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    // Width 12: the padding leaves an 8-char text width filled
    // exactly, with the caret on the fresh continuation row behind
    // it. The multi-row window shows the filled line and the caret
    // on the row beneath it.
    let mut terminal = render_to_buffer(&mut app, 12, 10);
    let rows = row_texts(&terminal);
    let text_row = rows
        .iter()
        .position(|row| row.contains("abcdefgh"))
        .unwrap_or_else(|| panic!("the exactly filled line stays visible: {rows:?}"));
    terminal
        .backend_mut()
        .assert_cursor_position((2, u16::try_from(text_row).unwrap() + 1));
}

/// The composer chunk [`TuiApp::render`] lays out, mirrored here so the
/// undersize pins can name the rows each pane owns at a starved height.
fn composer_chunk(width: u16, height: u16) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(Rect {
            x: 0,
            y: 0,
            width,
            height,
        })[3]
}

#[test]
fn the_caret_stays_inside_the_composer_in_undersized_terminals() {
    // The layout starves the composer before the status bar, so under
    // six rows the field gets fewer rows than its padding plus text
    // window — at two rows, none at all. Unclamped, the cursor landed
    // on the status bar's row or past the frame's last row entirely;
    // the clamp parks it on a row the field owns, and a field with no
    // rows parks no cursor.
    for height in [2u16, 3, 4, 5] {
        let mut app = app();
        app.handle_event(&Event::Paste("one\ntwo".to_string()));
        let mut terminal = render_to_buffer(&mut app, 40, height);
        let composer = composer_chunk(40, height);
        let (_x, y) = {
            let position = terminal
                .backend_mut()
                .get_cursor_position()
                .expect("the backend reports a cursor position");
            (position.x, position.y)
        };
        assert!(
            y < height,
            "h={height}: the cursor must stay inside the frame, got row {y}"
        );
        if composer.height > 0 {
            assert!(
                y >= composer.y && y < composer.bottom(),
                "h={height}: the cursor must sit on a composer row ({composer:?}), got {y}"
            );
        }
    }
}

#[test]
fn the_status_bar_paints_only_its_own_row_in_undersized_terminals() {
    // Whatever the height, the bar may paint at most one row — the
    // frame's last — so a starved composer keeps every row it was
    // given and never cedes its last one to the bar. The bar is
    // detected by its own paint, foreground and background together:
    // the composer's rows share the bar's surface background on their
    // text cells, so the pair — and the padding cells the composer
    // leaves at a Reset foreground — is what isolates the bar's row.
    for height in [2u16, 3, 4, 5, 6] {
        let mut app = app();
        let terminal = render_to_buffer(&mut app, 40, height);
        let buffer = terminal.backend().buffer();
        let bar_rows: Vec<u16> = (0..height)
            .filter(|&y| {
                buffer[(0, y)].fg == app.theme.ui.status_bar_fg
                    && buffer[(0, y)].bg == app.theme.ui.status_bar_bg
            })
            .collect();
        assert!(
            bar_rows.iter().all(|&y| y + 1 == height),
            "h={height}: the bar must paint only the frame's last row, found {bar_rows:?}"
        );
    }
}

#[test]
fn the_conversation_pane_renders_on_the_terminal_s_own_background() {
    // The one-layer canvas contract: the session points the terminal's
    // default background at the theme's `background` (OSC 11), and the
    // conversation pane renders on that default instead of painting
    // cells — window margin and grid share one color on one layer, so
    // no per-cell paint can differ from the margin beside it. Only the
    // raised surfaces paint: the composer on `surface`, the bar on its
    // own row.
    let mut app = app();
    let terminal = render_to_buffer(&mut app, 40, 10);
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(5, 2)].bg,
        ratatui::style::Color::Reset,
        "the conversation pane leaves its cells to the terminal default — \
         painting them is what shows a seam against the margin"
    );
    let composer = composer_chunk(40, 10);
    assert_eq!(
        buffer[(0, composer.y)].bg,
        app.theme.ui.surface,
        "the composer draws on the surface color"
    );
    assert_eq!(
        buffer[(0, 9)].bg,
        app.theme.ui.status_bar_bg,
        "the status bar paints its own row"
    );
}

#[test]
fn the_status_tag_claims_its_columns_from_the_status_text() {
    // On a narrow frame the right-aligned tag owns its columns
    // outright: the model/tokens text clips one column short of the
    // tag, so a long model name never renders beneath the tag and a
    // blank column separates the two.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut config = config_with_theme("dracula");
    config.api.model = "a-very-long-model-name".to_string();
    let mut app = TuiApp::from_observer_state(config, kept.clone());
    kept.queued.store(2, std::sync::atomic::Ordering::SeqCst);

    let terminal = render_to_buffer(&mut app, 20, 8);
    let rows = row_texts(&terminal);
    let status = rows.last().expect("the status row");
    assert!(status.contains("2 queued"), "the tag renders: {status}");
    assert!(
        !status.contains("a-very-long-model-name"),
        "a model name this long must clip short of the tag: {status}"
    );
    // "2 queued" is 8 cells inside a 10-cell tag area anchored at the
    // right edge (columns 10-19); the left text clips at column 9, so
    // that column — the gutter, not the tag area's own right-alignment
    // padding — must be blank. Unclipped, the model name's tenth
    // character renders there, beneath the tag's edge.
    let columns = status.chars().collect::<Vec<_>>();
    let gap_at = 20 - 10 - 1;
    assert!(
        columns
            .get(gap_at)
            .is_some_and(|column| column.is_whitespace()),
        "a blank column must separate the text from the tag: {status}"
    );
    assert_eq!(
        columns.get(9),
        Some(&' '),
        "the clipped text ends one column short of the tag area: {status}"
    );
    assert!(
        status.starts_with(" a-very-l"),
        "the clipped model text keeps its readable prefix: {status}"
    );
}

#[test]
fn the_conversation_pane_holds_still_once_the_composer_reaches_its_cap() {
    // The stability contract: the composer grows with its buffer up
    // to the cap, and past that further typing moves nothing — the
    // transcript and its scrollbar hold still row for row.
    let mut app = app();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line {i:02}"),
            timestamp: chrono::Utc::now(),
        });
    }
    // fill the composer past its three-row cap first
    app.handle_event(&Event::Paste(
        "a prompt\nthat spans\nseveral\nlines".to_string(),
    ));
    let before = render_to_buffer(&mut app, 40, 24);
    app.handle_event(&Event::Paste(" and more words".to_string()));
    let after = render_to_buffer(&mut app, 40, 24);

    let earlier = before.backend().buffer();
    let later = after.backend().buffer();
    for y in 0..16u16 {
        assert_eq!(
            earlier[(39, y)].symbol(),
            later[(39, y)].symbol(),
            "row {y}: the scrollbar must not move once the cap is reached"
        );
        assert_eq!(
            earlier[(0, y)].symbol(),
            later[(0, y)].symbol(),
            "row {y}: the transcript's first column must not move once the cap is reached"
        );
    }
}

#[test]
fn up_recalls_history_instead_of_scrolling_the_transcript() {
    let mut app = app();
    for c in "earlier".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));

    assert!(app.handle_event(&plain(KeyCode::Up)));
    assert_eq!(app.input(), "earlier", "Up recalls the submitted entry");
    assert_eq!(app.scroll_offset(), 0, "the transcript does not move");
    assert!(app.auto_scroll(), "history recall does not detach the view");
}

#[test]
fn ctrl_p_recalls_history_from_a_multiline_buffer() {
    let mut app = app();
    for c in "earlier".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    for c in "ab".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&key(KeyCode::Enter, KeyModifiers::SHIFT)));
    app.handle_event(&plain(KeyCode::Char('c')));

    assert!(app.handle_event(&key(KeyCode::Char('p'), KeyModifiers::CONTROL)));
    assert_eq!(
        app.input(),
        "earlier",
        "Ctrl-P reaches history even while editing multiple lines"
    );
}

#[test]
fn late_pushed_messages_appear_in_the_next_render() {
    let mut app = app();
    let _ = render_to_buffer(&mut app, 40, 12);
    app.push_message(TuiMessage::User {
        text: "arrived between frames".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let terminal = render_to_buffer(&mut app, 40, 12);
    assert!(
        view_text(&terminal, 40, 12).contains("arrived between frames"),
        "a message pushed after a cached render must still show up"
    );
}

#[test]
fn a_width_change_reflows_the_cached_conversation() {
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let wide = render_to_buffer(&mut app, 60, 12);
    assert!(
        row_texts(&wide)[0].contains("dddddddddd"),
        "at width 60 the message fits one row"
    );

    let narrow = render_to_buffer(&mut app, 20, 12);
    let narrow_rows = row_texts(&narrow);
    assert!(
        !narrow_rows[0].contains("dddddddddd")
            && narrow_rows.iter().any(|row| row.contains("dddddddddd")),
        "a resize rebuilds the cache and the message reflows onto later rows"
    );
}

#[test]
fn a_large_session_typing_redraw_stays_inside_the_frame_budget() {
    // Per the render cache and the windowed viewport assembly, a
    // keystroke's cost tracks the pane height, not the session
    // length. A regression to per-frame O(session) work (the whole-
    // conversation clone this replaces) pushes the mean past this
    // budget by an order of magnitude at this session size.
    let mut app = app();
    for i in 0..150 {
        let text = (0..40)
            .map(|l| format!("message {i} line {l} with some words here"))
            .collect::<Vec<_>>()
            .join("\n\n");
        app.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text { text }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
    }
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();

    let chars: Vec<char> = "the quick brown fox jumps".chars().collect();
    let start = std::time::Instant::now();
    for i in 0..100 {
        app.handle_event(&plain(KeyCode::Char(chars[i % chars.len()])));
        terminal.draw(|frame| app.render(frame)).unwrap();
    }
    let mean_micros = start.elapsed().as_secs_f64() * 1e4;
    assert!(
        mean_micros < 10_000.0,
        "mean typing redraw {mean_micros:.0} µs exceeds the 10 ms frame-class budget"
    );
}

#[test]
fn typing_during_a_live_code_block_stays_inside_the_frame_budget() {
    // The live segment re-renders only when a delta moves its stamp;
    // a keystroke changes nothing about the streamed reply, so its
    // frame must be a cache hit. Re-parsing per keystroke (the shape
    // this guards against) measured two orders of magnitude past
    // this budget with a 300-line open fence in a debug build.
    let mut app = app();
    let mut reply = String::from("```rust\n");
    for i in 0..300 {
        reply.push_str("let value_");
        reply.push_str(&i.to_string());
        reply.push_str(" = compute_something(i) + other(i);\n");
    }
    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str(&reply);
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();

    let chars: Vec<char> = "the quick brown fox".chars().collect();
    let start = std::time::Instant::now();
    for i in 0..50 {
        app.handle_event(&plain(KeyCode::Char(chars[i % chars.len()])));
        terminal.draw(|frame| app.render(frame)).unwrap();
    }
    let mean_micros = start.elapsed().as_secs_f64() * 2e4;
    assert!(
        mean_micros < 10_000.0,
        "mean typing redraw {mean_micros:.0} µs with a live open fence exceeds the budget"
    );
}

#[test]
fn a_failed_send_rolls_the_queue_increment_back() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    drop(rx);

    for c in "gone".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        kept.queued.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a send no receiver takes must not stay counted as queued"
    );
    assert_eq!(app.conversation().len(), 1, "the echo still lands");
}

#[test]
fn multiline_user_and_system_echoes_render_on_separate_rows() {
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "first\nsecond".to_string(),
        timestamp: chrono::Utc::now(),
    });
    app.push_message(TuiMessage::System {
        text: "alpha\nbeta".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let terminal = render_to_buffer(&mut app, 40, 20);
    let rows = row_texts(&terminal);
    let row_of = |needle: &str| rows.iter().position(|row| row.contains(needle));
    let (Some(first), Some(second), Some(alpha), Some(beta)) = (
        row_of("first"),
        row_of("second"),
        row_of("alpha"),
        row_of("beta"),
    ) else {
        panic!("all four segments must render: {rows:?}");
    };
    assert!(
        first < second && alpha < beta,
        "each message's segments occupy distinct, ordered rows: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("firstsecond")),
        "ratatui must not smash the newline-stripped segments onto one row"
    );
}

#[test]
fn a_read_failure_propagates_from_the_event_drain() {
    let mut app = app();
    let failure = std::io::Error::other("terminal broke");
    let mut queued = std::collections::VecDeque::from(vec![
        Ok(plain(KeyCode::Char('x'))),
        Err(failure),
        Ok(plain(KeyCode::Char('y'))),
    ]);
    let result = app.drain_ready(move || queued.pop_front());
    assert!(
        result.is_err(),
        "a read failure must surface as an error, not a silent exit"
    );
    assert_eq!(
        app.input(),
        "x",
        "the trailing event never applies past the failure"
    );
}

#[test]
fn the_drain_stops_at_a_quit_event() {
    let mut app = app();
    app.handle_event(&plain(KeyCode::Char('h')));
    app.handle_event(&plain(KeyCode::Char('i')));
    let mut queued = std::collections::VecDeque::from(vec![
        Ok(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Ok(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Ok(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Ok(plain(KeyCode::Enter)),
        Ok(plain(KeyCode::Char('!'))),
    ]);
    let redraws = app
        .drain_ready(move || queued.pop_front())
        .expect("no failure in the batch");
    assert!(redraws, "the batch up to quit needs a redraw");
    assert!(app.is_quitting(), "the confirming press quits");
    assert_eq!(
        app.conversation().len(),
        0,
        "the Enter queued behind the quit never submits the buffered text"
    );
    assert_eq!(
        app.input(),
        "",
        "the chord cleared the draft; the trailing keystroke never applies"
    );
}

#[test]
fn a_submit_before_the_quit_in_one_burst_lands() {
    let mut app = app();
    app.handle_event(&plain(KeyCode::Char('h')));
    app.handle_event(&plain(KeyCode::Char('i')));
    let mut queued = std::collections::VecDeque::from(vec![
        Ok(plain(KeyCode::Enter)),
        Ok(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Ok(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Ok(plain(KeyCode::Char('x'))),
    ]);
    let redraws = app
        .drain_ready(move || queued.pop_front())
        .expect("no failure in the batch");
    assert!(redraws, "the landing submit redraws");
    assert!(app.is_quitting(), "the quit chord still lands after it");
    assert_eq!(
        app.conversation().len(),
        1,
        "a text-backed Enter ahead of the quit submits and echoes"
    );
    assert_eq!(
        app.input(),
        "",
        "the submit clears the buffer; only the post-quit keystroke is dropped"
    );
}

#[test]
fn f2_cycles_verbosity_and_rerenders_completed_tools() {
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            call_id: String::new(),
            name: "Read".to_string(),
            input_preview: r#"{"file_path":"a.rs"}"#.to_string(),
            success: true,
            elapsed_secs: 0.42,
            output_preview: String::new(),
            retained_input: String::new(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });

    let normal = render_to_buffer(&mut app, 80, 30);
    let normal_rows = row_texts(&normal).join("\n");
    assert!(
        normal_rows.contains("✓ Reading a.rs… (0.4s)"),
        "Normal shows the humanized summary and duration: {normal_rows:?}"
    );

    app.handle_event(&key(KeyCode::F(2), KeyModifiers::NONE));
    let verbose = render_to_buffer(&mut app, 80, 30);
    let verbose_rows = row_texts(&verbose).join("\n");
    assert!(
        verbose_rows.contains("Read "),
        "Verbose shows the raw name: {verbose_rows:?}"
    );
    assert!(
        verbose_rows.contains("    Input:"),
        "Verbose shows the input block after one F2 (Normal→Verbose via default order check): {verbose_rows:?}"
    );

    app.handle_event(&key(KeyCode::F(2), KeyModifiers::NONE));
    let quiet = render_to_buffer(&mut app, 80, 30);
    let quiet_rows = row_texts(&quiet).join("\n");
    assert!(
        quiet_rows.contains("✓ Reading a.rs…") && !quiet_rows.contains("(0.4s)"),
        "Quiet collapses to one dim line without duration — the cache re-rendered: {quiet_rows:?}"
    );
}

#[test]
fn running_tools_render_their_humanized_summary() {
    let mut app = app();
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(dch_tui::ActiveTool {
            call_id: "call-1".to_string(),
            name: "Read".to_string(),
            input_summary: r#"{"file_path":"src/lib.rs"}"#.to_string(),
            start: std::time::Instant::now(),
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("Reading src/lib.rs")),
        "a running tool humanizes its summary live: {rows:?}"
    );
}

#[test]
fn a_tool_graduation_fires_the_turn_end_hook() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: String::new(),
            full_input: String::new(),
            full_output: String::new(),
            name: "Read".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(2),
            input_summary: String::new(),
            output_preview: String::new(),
        }));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let snapshots: Arc<std::sync::Mutex<Snapshots>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&snapshots);
    app.set_turn_end_hook(Box::new(move |conversation| {
        snapshots_lock(&sink).push(conversation.to_vec());
    }));
    drop(render_to_buffer(&mut app, 80, 30));
    let seen = snapshots_lock(&snapshots);
    assert_eq!(
        seen.len(),
        1,
        "a graduating tool is turn progress — the hook fires so the transcript keeps it"
    );
    assert!(
        seen.first()
            .expect("the one snapshot")
            .iter()
            .any(|message| matches!(
                message,
                TuiMessage::Assistant { blocks, .. }
                    if blocks.iter().any(|block| matches!(block, ContentBlock::Tool { name, .. } if name == "Read"))
            )),
        "the snapshot carries the graduated tool"
    );
}

#[test]
fn a_click_expands_a_completed_tool_block_and_clicking_again_collapses_it() {
    // The summary row is a disclosure: a click with no travel opens
    // the block to its full command and output — indented, labeled,
    // wrapped — and a second click folds it back. The data rides the
    // graduation from the dispatch-side capture.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: "call-9".to_string(),
            name: "Bash".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(25),
            input_summary: r#"{"command":"make test"}"#.to_string(),
            output_preview: String::new(),
            full_input: "{\n  \"command\": \"make test\"\n}".to_string(),
            full_output: "running 74 tests\nfailures: 0".to_string(),
        }));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);

    let probe = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("Running: make test"))
        .unwrap_or(0);
    assert!(
        rows.iter().any(|r| r.contains("▸")),
        "a block with retained detail shows its closed marker: {rows:?}"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let open = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&open);
    assert!(
        rows.iter().any(|r| r.contains("▾")),
        "the marker flips open: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("\"command\": \"make test\"")),
        "the full command renders: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("running 74 tests")),
        "the full output renders: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("failures: 0")),
        "every output line renders: {rows:?}"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let shut = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&shut);
    assert!(
        rows.iter()
            .all(|r| !r.contains("\"command\": \"make test\"")),
        "the second click folds the block back: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("▸")),
        "the marker returns to closed: {rows:?}"
    );
}

#[test]
fn a_running_tool_row_expands_to_its_captured_input() {
    // The capture middleware records the input before the call runs,
    // so a running row is expandable the moment it appears — full
    // command now, output pending until the dispatch completes.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.tool_captures.lock().expect("the capture lock").insert(
        "call-4".to_string(),
        dch_tui::ToolCapture {
            input_json: "{\n  \"command\": \"cargo test\"\n}".to_string(),
            output: String::new(),
            is_error: false,
            done: false,
        },
    );
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(dch_tui::ActiveTool {
            call_id: "call-4".to_string(),
            name: "Bash".to_string(),
            input_summary: r#"{"command":"cargo test"}"#.to_string(),
            start: std::time::Instant::now(),
        });

    let probe = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("Running: cargo test"))
        .unwrap_or(0);
    assert!(
        rows[row].contains("▸"),
        "a captured running row is expandable: {:?}",
        rows[row]
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let open = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&open);
    assert!(
        rows.iter()
            .any(|r| r.contains("\"command\": \"cargo test\"")),
        "the running call's full command renders: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("… running")),
        "the output reads as pending: {rows:?}"
    );
}

#[test]
fn an_expansion_opened_while_running_survives_the_calls_completion() {
    // Running rows and their graduated block share the call id, so
    // an expansion opened mid-run hands over to the completed block
    // and the output fills in place.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let graduations = std::sync::Arc::clone(&kept.graduations);
    kept.tool_captures.lock().expect("the capture lock").insert(
        "call-7".to_string(),
        dch_tui::ToolCapture {
            input_json: "{\n  \"file_path\": \"src/lib.rs\" }".to_string(),
            output: String::new(),
            is_error: false,
            done: false,
        },
    );
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(dch_tui::ActiveTool {
            call_id: "call-7".to_string(),
            name: "Read".to_string(),
            input_summary: r#"{"file_path":"src/lib.rs"}"#.to_string(),
            start: std::time::Instant::now(),
        });

    let probe = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("Reading src/lib.rs"))
        .unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let running = render_to_buffer(&mut app, 80, 30);
    assert!(
        row_texts(&running).iter().any(|r| r.contains("… running")),
        "sanity: the expansion is open while the call runs"
    );

    app.active_tools().lock().expect("the tools lock").clear();
    graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: "call-7".to_string(),
            name: "Read".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(4),
            input_summary: r#"{"file_path":"src/lib.rs"}"#.to_string(),
            output_preview: String::new(),
            full_input: "{\n  \"file_path\": \"src/lib.rs\" }".to_string(),
            full_output: "pub fn answer() -> u32 { 42 }".to_string(),
        }));
    let done = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&done);
    assert!(
        rows.iter()
            .any(|r| r.contains("pub fn answer() -> u32 { 42 }")),
        "the completed block fills the output in place: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| !r.contains("… running")),
        "the pending note is gone: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("▾")),
        "the expansion stayed open across the handover: {rows:?}"
    );
}

#[test]
fn the_oldest_tool_details_retire_past_the_cap() {
    // The detail store is FIFO-bounded: past the cap the oldest
    // graduated call loses its expansion — the marker and the
    // click go with it — while the newest stay expandable.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let graduations = std::sync::Arc::clone(&kept.graduations);
    {
        let mut queue = graduations.lock().expect("the queue lock");
        for index in 0..257 {
            queue.push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
                call_id: format!("call-{index}"),
                name: "Bash".to_string(),
                is_error: false,
                duration: std::time::Duration::from_millis(1),
                input_summary: format!(r#"{{"command":"cmd {index}"}}"#),
                output_preview: String::new(),
                full_input: format!("{{\n  \"command\": \"cmd {index}\"\n}}"),
                full_output: format!("out {index}"),
            }));
        }
    }
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let probe = render_to_buffer(&mut app, 80, 300);
    let rows = row_texts(&probe);
    let oldest = rows
        .iter()
        .position(|r| r.contains("cmd 0"))
        .expect("the oldest block renders");
    assert!(
        !rows[oldest].contains("▸"),
        "the retired block carries no marker: {:?}",
        rows[oldest]
    );
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(oldest).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(oldest).unwrap_or(0),
    ));
    let clicked = render_to_buffer(&mut app, 80, 300);
    assert!(
        !row_texts(&clicked).iter().any(|r| r.contains("out 0")),
        "the retired block no longer expands"
    );

    let newest = rows
        .iter()
        .position(|r| r.contains("cmd 256"))
        .expect("the newest block renders");
    assert!(
        rows[newest].contains("▸"),
        "the newest block stays expandable: {:?}",
        rows[newest]
    );
}

#[test]
fn carriage_return_pastes_fold_to_line_breaks() {
    // Terminals spell pasted newlines differently — Alacritty sends
    // carriage returns, Ghostty line feeds. A CR that survives as
    // content welds the paste into one logical line: the composer's
    // window shows only the tail and the echo reads as one clipped
    // row. Both CR spellings fold to the editor's line feed.
    let mut app = app();
    let cr_text = "FIRST LINE\rx\r\nSECOND LINE\rTHIRD LINE".to_string();
    app.handle_event(&Event::Paste(cr_text));
    assert_eq!(
        app.input(),
        "FIRST LINE\nx\nSECOND LINE\nTHIRD LINE",
        "CRLF and bare CR both fold; nothing else changes"
    );
    assert!(
        !app.input().contains('\r'),
        "no carriage return survives as content"
    );

    app.handle_event(&plain(KeyCode::Enter));
    let echo = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&echo);
    let first = rows.iter().position(|r| r.contains("FIRST LINE"));
    let second = rows.iter().position(|r| r.contains("SECOND LINE"));
    let third = rows.iter().position(|r| r.contains("THIRD LINE"));
    assert!(
        rows.iter().any(|r| r.contains("FIRST LINE")),
        "the paste's head renders: {rows:?}"
    );
    assert!(
        first.is_some_and(|f| second.is_some_and(|s| third.is_some_and(|t| f < s && s < t))),
        "each source line renders on its own row, in order: {rows:?}"
    );
}

#[test]
fn long_user_lines_wrap_in_the_conversation_instead_of_clipping() {
    // A pasted line longer than the pane reads as missing text when
    // the row clips at the edge; wrapped, its tail is on screen and
    // the transcript shows everything that was submitted.
    let mut app = app();
    let now = chrono::Utc::now();
    let long = format!("{}{}", "x".repeat(90), "THE-TAIL-IS-HERE");
    app.push_message(TuiMessage::User {
        text: long,
        timestamp: now,
    });
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("THE-TAIL-IS-HERE")),
        "the line's tail is visible past the pane width: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with('x')),
        "the line's head starts the first row"
    );
    let spanned = rows.iter().filter(|row| row.contains('x')).count();
    assert!(
        spanned >= 2,
        "the line flows across rows instead of clipping: {spanned} rows, {rows:?}"
    );
}

#[test]
fn a_retried_call_ages_from_its_newest_attempt() {
    // A retry graduates again under the same call id; the eviction
    // queue must not hold the id twice — a duplicate would retire
    // another call's detail early and leave the set of expandable
    // blocks one short of the cap.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let graduations = std::sync::Arc::clone(&kept.graduations);
    {
        let mut queue = graduations.lock().expect("the queue lock");
        // ids 0..=254 stay single (255), then call-dup twice: 256
        // distinct calls, 257 graduations. Without the requeue the
        // queue would exceed the cap and evict call-0; with it, the
        // deduplicated queue sits exactly at capacity and nothing
        // evicts.
        for index in 0..255 {
            queue.push(graduation(format!("call-{index}")));
        }
        queue.push(graduation("call-dup".to_string()));
        queue.push(graduation("call-dup".to_string()));
    }
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let probe = render_to_buffer(&mut app, 80, 300);
    let rows = row_texts(&probe);
    assert!(
        rows.iter()
            .any(|r| r.contains("cmd call-0") && r.contains("▸")),
        "the duplicate did not push the oldest detail out early: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("cmd call-dup") && r.contains("▸")),
        "the retried call itself stays expandable"
    );
}

fn graduation(call_id: String) -> dch_tui::Graduation {
    dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
        name: "Bash".to_string(),
        is_error: false,
        duration: std::time::Duration::from_millis(1),
        input_summary: format!(r#"{{"command":"cmd {}"}}"#, call_id),
        output_preview: String::new(),
        full_input: format!("{{\"command\": \"cmd {call_id}\"}}"),
        full_output: format!("out {call_id}"),
        call_id,
    })
}

#[test]
fn a_captureless_completion_never_advertises_an_expansion() {
    // Refused-before-execution calls graduate with empty capture
    // halves; the marker is a promise of retained content, so no
    // detail means no glyph and no toggle — the block never opens
    // onto bare labels.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: "call-refused".to_string(),
            name: "Bash".to_string(),
            is_error: true,
            duration: std::time::Duration::from_millis(0),
            input_summary: r#"{"command":"rm -rf /"}"#.to_string(),
            output_preview: String::new(),
            full_input: String::new(),
            full_output: String::new(),
        }));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows.iter().position(|r| r.contains("rm -rf")).unwrap_or(0);
    assert!(
        !rows.iter().any(|r| r.contains("▸")),
        "no marker without retained content: {rows:?}"
    );
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let clicked = render_to_buffer(&mut app, 80, 24);
    assert!(
        !row_texts(&clicked).iter().any(|r| r.contains("input:")),
        "nothing expands: {:?}",
        row_texts(&clicked)
    );
}

#[test]
fn a_mouse_press_between_the_chord_disarms_it() {
    // The two Ctrl+C presses must be consecutive input: a click in
    // between starts a different interaction and disarms the chord.
    let mut app = app();
    assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        4,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        4,
    ));
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        !app.is_quitting(),
        "a click between the presses breaks the chord"
    );
}

#[test]
fn a_block_without_retained_detail_is_not_expandable() {
    // Blocks with nothing retained — calls that ran without a
    // capture, sessions resumed from before the field existed —
    // carry no marker and no toggle: their rows stay plain.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: String::new(),
            name: "Bash".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(3),
            input_summary: r#"{"command":"ls"}"#.to_string(),
            output_preview: String::new(),
            full_input: String::new(),
            full_output: String::new(),
        }));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);

    let probe = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("Running: ls"))
        .unwrap_or(0);
    assert!(
        !rows.iter().any(|r| r.contains("▸")),
        "no marker without retained detail: {rows:?}"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let clicked = render_to_buffer(&mut app, 80, 30);
    assert!(
        !row_texts(&clicked).iter().any(|r| r.contains("input:")),
        "the click does not open anything: {:?}",
        row_texts(&clicked)
    );
}

#[test]
fn dragging_from_a_summary_row_still_selects_instead_of_toggling() {
    // The summary row keeps its selection behavior: a press that
    // travels is a drag, not a click, so it selects text and copies
    // on release — the expansion never opens.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: "call-2".to_string(),
            name: "Bash".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(8),
            input_summary: r#"{"command":"make"}"#.to_string(),
            output_preview: String::new(),
            full_input: "{\n  \"command\": \"make\"\n}".to_string(),
            full_output: "built".to_string(),
        }));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |text| {
        sink.lock().expect("sink").push(text.to_string());
    }));

    let probe = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&probe);
    let user_row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = rows[user_row].find("abcdefghij").unwrap_or(0);
    let tool_row = rows
        .iter()
        .position(|r| r.contains("Running: make"))
        .unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(tool_row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 3).unwrap_or(0),
        u16::try_from(user_row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 3).unwrap_or(0),
        u16::try_from(user_row).unwrap_or(0),
    ));
    let copied = copied.lock().expect("sink").clone();
    assert!(
        !copied.is_empty(),
        "the drag selected and copied, not toggled"
    );
    let after = render_to_buffer(&mut app, 80, 30);
    assert!(
        row_texts(&after).iter().all(|r| !r.contains("input:")),
        "the block never expanded: {:?}",
        row_texts(&after)
    );
}

#[test]
fn finish_tool_pairs_the_graduation_with_its_capture() {
    // The lifecycle event carries no output text; the capture store
    // holds it. Completing a call takes its capture into the
    // graduation — keyed exactly by call id — and retires the entry.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    kept.tool_captures.lock().expect("the capture lock").insert(
        "call-3".to_string(),
        dch_tui::ToolCapture {
            input_json: "{\n  \"command\": \"ls\"\n}".to_string(),
            output: "src\ntarget".to_string(),
            is_error: false,
            done: true,
        },
    );
    kept.active_tools
        .lock()
        .expect("the tools lock")
        .push(dch_tui::ActiveTool {
            call_id: "call-3".to_string(),
            name: "Bash".to_string(),
            input_summary: r#"{"command":"ls"}"#.to_string(),
            start: std::time::Instant::now(),
        });
    observer.finish_tool("call-3", "Bash", false, std::time::Duration::from_millis(5));

    let graduations = kept.graduations.lock().expect("the queue lock").clone();
    let Some(dch_tui::Graduation::Tool(result)) = graduations.first() else {
        panic!("the completion graduates: {graduations:?}");
    };
    assert_eq!(result.call_id, "call-3", "the call id rides the graduation");
    assert_eq!(
        result.full_input, "{\n  \"command\": \"ls\"\n}",
        "the capture's input rides the graduation"
    );
    assert_eq!(
        result.full_output, "src\ntarget",
        "the output rides the graduation"
    );
    assert_eq!(
        result.output_preview, "src\ntarget",
        "the persisted preview carries the same retained output"
    );
    assert!(
        kept.tool_captures
            .lock()
            .expect("the capture lock")
            .get("call-3")
            .is_none(),
        "the entry retires with the graduation"
    );
}

#[test]
fn a_click_on_any_row_of_an_expanded_block_collapses_it() {
    // The whole open block is the toggle target, not just its
    // summary row: after a long expansion scrolls the summary out
    // of view, a click anywhere on the visible expansion folds it.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: "call-5".to_string(),
            name: "Bash".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(10),
            input_summary: r#"{"command":"make test"}"#.to_string(),
            output_preview: String::new(),
            full_input: "{\n  \"command\": \"make test\"\n}".to_string(),
            full_output: (0..30)
                .map(|i| format!("output line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("Running: make test"))
        .unwrap_or(0);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let open = render_to_buffer(&mut app, 80, 24);
    let open_rows = row_texts(&open);
    let output_row = open_rows
        .iter()
        .position(|r| r.contains("output line"))
        .expect("the expansion is on screen");
    assert!(
        open_rows.iter().any(|r| r.contains("Running: make test")),
        "the summary row held still while the block opened: {open_rows:?}"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(output_row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(output_row).unwrap_or(0),
    ));
    let shut = render_to_buffer(&mut app, 80, 24);
    let shut_rows = row_texts(&shut);
    assert!(
        shut_rows
            .iter()
            .all(|r| !r.contains("output line") && !r.contains("input:")),
        "a click on the expansion's own rows folds the block: {shut_rows:?}"
    );
}

#[test]
fn expanding_holds_the_clicked_row_steady_instead_of_chasing_the_tail() {
    // A pinned view follows the document's tail, so a naive expand
    // of a long block would scroll the block's own summary out of
    // view. Expanding detaches: the growth compensation below the
    // clicked row keeps it on screen with the expansion flowing
    // under it.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: "call-6".to_string(),
            name: "Bash".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(10),
            input_summary: r#"{"command":"make build"}"#.to_string(),
            output_preview: String::new(),
            full_input: "{\n  \"command\": \"make build\"\n}".to_string(),
            full_output: (0..30)
                .map(|i| format!("build line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("Running: make build"))
        .unwrap_or(0);
    assert_eq!(row, 0, "sanity: the short document starts at the top");

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        u16::try_from(row).unwrap_or(0),
    ));
    let open = render_to_buffer(&mut app, 80, 24);
    let open_rows = row_texts(&open);
    assert!(
        open_rows
            .first()
            .is_some_and(|r| r.contains("Running: make build")),
        "the clicked row stays at the top: {open_rows:?}"
    );
    assert!(
        open_rows.iter().any(|r| r.contains("build line 00")),
        "the expansion's start shows under it: {open_rows:?}"
    );
}

#[test]
fn hostile_elapsed_values_render_instead_of_panicking() {
    let mut app = app();
    for elapsed_secs in [-1.0, f64::NAN, 1.0e300] {
        app.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Tool {
                call_id: String::new(),
                name: "Read".to_string(),
                input_preview: "a.rs".to_string(),
                success: true,
                elapsed_secs,
                output_preview: String::new(),
                retained_input: String::new(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
    }
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("Reading a.rs"))
            .count(),
        3,
        "negative, NaN, and overflowing elapsed values all render as zero: {rows:?}"
    );
}

#[test]
fn a_reply_graduates_below_the_tool_that_preceded_it() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    // A tool completes, then the turn's reply arrives — no draw
    // between them, so both wait in the queue together.
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Tool(dch_tui::ToolResultDisplay {
            call_id: String::new(),
            full_input: String::new(),
            full_output: String::new(),
            name: "Read".to_string(),
            is_error: false,
            duration: std::time::Duration::from_millis(3),
            input_summary: String::new(),
            output_preview: String::new(),
        }));
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Reply(
            "here is what I found".to_string(),
        ));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    drop(render_to_buffer(&mut app, 80, 30));
    let kinds: Vec<&str> = app
        .conversation()
        .iter()
        .map(|message| match message {
            TuiMessage::Assistant { blocks, .. } => {
                if blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Tool { .. }))
                {
                    "tool"
                } else {
                    "text"
                }
            }
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["tool", "text"],
        "the conversation keeps the order the events happened — the tool that \
         produced the reply sits above it"
    );
}

#[test]
fn the_scrollbar_owns_its_gutter_and_never_touches_text() {
    let mut app = app();
    for i in 0..20 {
        app.push_message(TuiMessage::User {
            text: format!("message {i:02}"),
            timestamp: chrono::Utc::now(),
        });
    }
    // 20 one-line messages over the conversation pane make the
    // document scrollable; the pinned view sits at its bottom.
    let terminal = render_to_buffer(&mut app, 20, 12);
    let buffer = terminal.backend().buffer();
    let thumb = app.theme.ui.scrollbar_thumb;
    let track = app.theme.ui.scrollbar_track;
    // 12 rows: 5 for the conversation (notice row, spacer, padded
    // two-row input field, status bar). thumb = 5 * 5 / 20 = 1 row
    // flush with the track's bottom.
    for y in 0..5u16 {
        let cell = &buffer[(19, y)];
        let in_thumb = y >= 4;
        assert_eq!(
            cell.symbol(),
            " ",
            "row {y}: the scrollbar paints background fills, not glyphs"
        );
        assert_eq!(
            cell.bg,
            if in_thumb { thumb } else { track },
            "row {y}: the solid bar is the theme's thumb and rail colors as cell backgrounds"
        );
    }
    for y in 0..5u16 {
        for x in 0..19u16 {
            let cell = &buffer[(x, y)];
            assert!(
                cell.symbol() == " " || cell.bg != thumb,
                "wrapped text never takes the scrollbar's thumb fill ({x},{y})"
            );
        }
    }
    assert!(
        (0..19u16).any(|x| buffer[(x, 3)].symbol() != " "),
        "the text area still carries content beside the gutter"
    );
}

#[test]
fn the_gutter_stays_blank_when_the_document_fits() {
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "hello".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let terminal = render_to_buffer(&mut app, 20, 12);
    let buffer = terminal.backend().buffer();
    for y in 0..6u16 {
        let cell = &buffer[(19, y)];
        assert_eq!(
            cell.symbol(),
            " ",
            "row {y}: a document that fits its viewport draws no scrollbar"
        );
        assert_eq!(
            cell.bg,
            ratatui::style::Color::Reset,
            "row {y}: no rail fill either — the gutter is untouched"
        );
    }
}

/// Stage `requests` as pending permission asks on a fresh app.
///
/// Returns the app, the bridge's sender half (for staging more
/// requests mid-test), and each request's reply receiver in order.
fn permission_app(
    requests: &[(&str, &str)],
) -> (
    TuiApp,
    tokio::sync::mpsc::UnboundedSender<dch_tui::PermissionRequest>,
    Vec<tokio::sync::oneshot::Receiver<bool>>,
) {
    let mut app = app();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut receivers = Vec::new();
    for (tool, prompt) in requests {
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(dch_tui::PermissionRequest {
            tool_name: (*tool).to_string(),
            prompt: (*prompt).to_string(),
            reply,
        })
        .expect("receiver alive");
        receivers.push(reply_rx);
    }
    app.set_permission_requests(rx);
    app.poll_permission_requests();
    (app, tx, receivers)
}

/// The first row index whose rendered text contains `needle`.
///
/// Panics when `needle` is nowhere on screen — the position pins read
/// as failures naming what failed to render.
fn row_of(terminal: &Terminal<TestBackend>, needle: &str) -> usize {
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        if row.contains(needle) {
            return usize::from(y);
        }
    }
    panic!("{needle:?} is not on screen");
}

/// Every glyph the last frame drew, row by row.
///
/// One line per terminal row, newline-joined, so content assertions
/// can ask about the whole screen at once; position questions go
/// through `row_of` instead.
fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[test]
fn regaining_focus_requests_a_full_repaint() {
    // A terminal that skipped painting while its window was hidden
    // shows stale cells on reveal; the app must answer focus-regain
    // with the full-repaint claim the run loop clears on.
    let mut app = app();
    assert!(!app.handle_event(&Event::FocusLost));
    assert!(
        !app.take_full_repaint(),
        "losing focus repaints nothing — the window is not visible"
    );
    assert!(app.handle_event(&Event::FocusGained));
    assert!(
        app.take_full_repaint(),
        "regaining focus must claim a full repaint"
    );
    assert!(
        !app.take_full_repaint(),
        "the claim is one-shot — one clear, one full frame"
    );
}

#[test]
fn no_terminal_cursor_while_a_permission_ask_is_pending() {
    // The caret's blink is forced on first (any keypress resolidifies
    // it), so the pin reads the pending ask alone.
    let (mut app, _tx, mut replies) = permission_app(&[("Bash", "allow bash?")]);
    assert!(!app.handle_event(&plain(KeyCode::Char('x'))));
    assert!(
        !app.composer_caret_visible(),
        "the sheet owns the composer — no cursor under it"
    );

    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    drop(replies.remove(0));
    assert!(
        app.composer_caret_visible(),
        "the caret returns once the ask is answered"
    );
}

#[test]
fn the_permission_sheet_sits_over_the_prompt_box() {
    let (mut app, _tx, _replies) = permission_app(&[("Bash", "allow bash?")]);
    app.push_message(TuiMessage::User {
        text: "top of the conversation".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let title_row = row_of(&terminal, "Permission required");
    let message_row = row_of(&terminal, "top of the conversation");
    assert!(
        message_row < title_row,
        "the conversation must stay visible above the sheet ({message_row} vs {title_row})"
    );
    assert!(
        title_row >= 30 - 2 - 7,
        "the sheet must sit in the prompt-box region, not mid-conversation ({title_row})"
    );
    let status_row = row_of(&terminal, "CTX:");
    assert!(
        status_row > title_row,
        "the status bar must stay visible below the sheet ({status_row} vs {title_row})"
    );
}

#[test]
fn a_pending_permission_request_renders_the_overlay() {
    let (mut app, _tx, _replies) =
        permission_app(&[("Bash", "Allow 'Bash' (ShellExecute) in AcceptEdits mode?")]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("Permission required"),
        "the overlay's title must render: {screen}"
    );
    assert!(
        screen.contains("Allow 'Bash' (ShellExecute) in AcceptEdits mode?"),
        "the gate's prompt must render: {screen}"
    );
    assert!(
        screen.contains("[y] allow"),
        "the key hints must render: {screen}"
    );
}

#[test]
fn y_allows_the_pending_request_and_enter_is_swallowed() {
    let (mut app, _tx, mut replies) = permission_app(&[("Bash", "allow bash?")]);
    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        replies.pop().expect("one reply staged").blocking_recv(),
        Ok(true),
        "'y' must answer the ask with allow"
    );

    // Enter is submit intent everywhere else in the composer; under
    // the ask it must not resolve to an approval the user never made.
    let (mut app, _tx, mut replies) = permission_app(&[("Bash", "again?")]);
    let mut reply = replies.remove(0);
    assert!(
        !app.handle_event(&plain(KeyCode::Enter)),
        "a swallowed Enter asks for no redraw"
    );
    assert!(
        reply.try_recv().is_err(),
        "Enter must not consume the reply — the ask stays pending"
    );
    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        reply.blocking_recv(),
        Ok(true),
        "a later 'y' still answers the ask Enter left pending"
    );
}

#[test]
fn n_and_esc_deny_the_pending_request() {
    let (mut app, _tx, mut replies) = permission_app(&[("Bash", "allow bash?")]);
    assert!(app.handle_event(&plain(KeyCode::Char('n'))));
    assert_eq!(
        replies.pop().expect("one reply staged").blocking_recv(),
        Ok(false),
        "'n' must answer the ask with deny"
    );

    let (mut app, _tx, mut replies) = permission_app(&[("Bash", "allow bash?")]);
    assert!(app.handle_event(&plain(KeyCode::Esc)));
    assert_eq!(
        replies.pop().expect("one reply staged").blocking_recv(),
        Ok(false),
        "Esc must answer the ask with deny"
    );
}

#[test]
fn other_keys_are_inert_while_a_request_is_pending() {
    // The draft is written before the prompt lands, so the pins below
    // distinguish prompt ownership from an empty composer's no-submit.
    let mut app = app();
    for c in ['d', 'r', 'a', 'f', 't'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert_eq!(app.input(), "draft", "the draft is written before the ask");

    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    requests_tx
        .send(dch_tui::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "allow bash?".to_string(),
            reply,
        })
        .expect("receiver alive");
    app.set_permission_requests(requests_rx);
    app.poll_permission_requests();

    assert!(
        !app.handle_event(&plain(KeyCode::Char('x'))),
        "a swallowed key asks for no redraw"
    );
    assert_eq!(
        app.input(),
        "draft",
        "the composer must not take keystrokes while a prompt is up"
    );

    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(submit_tx);
    assert!(
        !app.handle_event(&plain(KeyCode::Enter)),
        "a swallowed Enter asks for no redraw"
    );
    assert!(
        reply_rx.try_recv().is_err(),
        "Enter neither answers the ask nor consumes its reply"
    );
    assert!(
        submit_rx.try_recv().is_err(),
        "Enter must not submit a drafted composer while a prompt is up"
    );
    assert_eq!(
        app.input(),
        "draft",
        "the draft survives the prompt untouched"
    );
}

#[test]
fn a_burst_of_ready_requests_lands_together() {
    // Two asks are already waiting when the loop's wakeup delivers the
    // first: one landing step must queue both, so the first frame
    // reports the true waiting count rather than one at a time.
    let mut app = app();
    let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
    for (tool, prompt) in [("Bash", "first ask?"), ("WebFetch", "second ask?")] {
        let (reply, _reply_rx) = tokio::sync::oneshot::channel();
        requests_tx
            .send(dch_tui::PermissionRequest {
                tool_name: tool.to_string(),
                prompt: prompt.to_string(),
                reply,
            })
            .expect("receiver alive");
    }
    let first = requests_rx.try_recv().expect("the wakeup's request");
    app.land_permission_requests(first, &mut requests_rx);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("first ask?") && screen.contains("1 more waiting"),
        "the burst lands together and the count reports it: {screen}"
    );
}

#[test]
fn queued_requests_render_front_first() {
    let (mut app, _tx, mut replies) = permission_app(&[
        ("Bash", "Allow the first ask?"),
        ("WebFetch", "Allow the second ask?"),
    ]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("Allow the first ask?") && !screen.contains("Allow the second ask?"),
        "only the front of the queue renders: {screen}"
    );
    assert!(
        screen.contains("1 more waiting"),
        "the overlay must count what waits behind: {screen}"
    );

    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        replies.remove(0).blocking_recv(),
        Ok(true),
        "the keypress answers the front request only"
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("Allow the second ask?"),
        "resolving the front reveals the next: {screen}"
    );
}

#[test]
fn run_end_clears_pending_requests() {
    let (mut app, _tx, replies) = permission_app(&[
        ("Bash", "Allow the first ask?"),
        ("WebFetch", "Allow the second ask?"),
    ]);
    assert!(
        app.tick_wake(std::time::Instant::now()),
        "retiring prompts on an idle agent must request a redraw"
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        !screen.contains("Allow the first ask?") && !screen.contains("Permission required"),
        "no overlay survives the run's end: {screen}"
    );
    for reply in replies {
        assert!(
            reply.blocking_recv().is_err(),
            "a retired request's reply channel must close, reading as a denial"
        );
    }
}

#[test]
fn a_long_prompt_wraps_without_hiding_the_hints() {
    // Long enough to need more rows than the overlay's cap, the way a
    // long MCP tool name inside the gate's question can.
    let prompt = format!(
        "Allow '{}' (Unclassified) in AcceptEdits mode? And some more question \
text so the string runs well past three wrapped rows of overlay body \
and would push the hints out of a fixed-height box.",
        "server__create_pull_request_review_from_template"
    );
    let (mut app, _tx, _replies) = permission_app(&[("WebFetch", prompt.as_str())]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("[y] allow"),
        "the hints must stay visible under a wrapping prompt: {screen}"
    );
    assert!(
        screen.contains('…'),
        "an over-long prompt is ellipsized, not hidden: {screen}"
    );

    // Narrow terminals re-wrap at their own inner width instead of
    // re-flowing rows wrapped for a wider box: the prompt ellipsizes
    // further and the keys stay on screen at mid and sub-hints widths.
    for (width, height) in [(50u16, 30u16), (24, 20)] {
        let (mut app, _tx, _replies) = permission_app(&[("WebFetch", prompt.as_str())]);
        let terminal = render_to_buffer(&mut app, width, height);
        let screen = screen_text(&terminal);
        assert!(
            screen.contains("[y] allow"),
            "at {width}×{height} the keys must stay visible: {screen}"
        );
        assert!(
            screen.contains("[n] deny"),
            "at {width}×{height} the deny key must stay visible too: {screen}"
        );
        assert!(
            screen.contains('…'),
            "at {width}×{height} the prompt is ellipsized, not hidden: {screen}"
        );
    }
}

#[test]
fn a_paste_behind_a_pending_prompt_lands_nowhere() {
    let (mut app, _tx, _replies) = permission_app(&[("Bash", "allow bash?")]);
    assert!(app.handle_event(&Event::Paste("pasted behind the overlay".to_string())));
    assert_eq!(
        app.input(),
        "",
        "a paste while a prompt is up must not reach the composer"
    );

    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert!(app.handle_event(&Event::Paste("after".to_string())));
    assert_eq!(
        app.input(),
        "after",
        "with the prompt gone, paste reaches the composer again"
    );
}

#[test]
fn a_click_behind_a_pending_prompt_leaves_the_hidden_caret_alone() {
    // The overlay hides the composer, but a click landing where the
    // composer sits would still move its caret — the draft would come
    // back edited in place once the ask is answered.
    let mut app = app();
    app.handle_event(&Event::Paste("hello world".to_string()));
    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, reply_rx) = tokio::sync::oneshot::channel();
    requests_tx
        .send(dch_tui::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "allow bash?".to_string(),
            reply,
        })
        .expect("receiver alive");
    app.set_permission_requests(requests_rx);
    app.poll_permission_requests();

    assert!(app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2 + 4,
        20,
    )));

    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        reply_rx.blocking_recv(),
        Ok(true),
        "the prompt still answers after the swallowed click"
    );
    let mut answered = render_to_buffer(&mut app, 80, 24);
    let caret = answered.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (2 + 11, 20),
        "the click behind the overlay never moved the draft's caret"
    );
}

#[test]
fn a_parked_drag_does_not_scroll_behind_a_pending_prompt() {
    // A drag pushed against the pane's edge scrolls on every tick; an
    // ask landing mid-push parks it, and the push never resumes once
    // the ask is gone.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    for _ in 0..5 {
        app.handle_event(&wheel_event(MouseEventKind::ScrollUp));
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        2,
        16,
    ));

    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
    requests_tx
        .send(dch_tui::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "allow bash?".to_string(),
            reply,
        })
        .expect("receiver alive");
    app.set_permission_requests(requests_rx);
    app.poll_permission_requests();

    let before = app.scroll_offset();
    for _ in 0..5 {
        let _ = app.tick_wake(std::time::Instant::now());
    }
    assert_eq!(
        app.scroll_offset(),
        before,
        "no tick scrolls the view while the ask is up or after it retires"
    );
}

#[test]
fn control_chords_stay_inert_behind_a_pending_prompt() {
    // The composer's own control chords — Ctrl-Enter and Ctrl-M submit,
    // Ctrl-W deletes a word — must not reach the hidden draft under the
    // overlay; a natural confirm reflex must neither approve the ask
    // nor consume the draft.
    let mut app = app();
    for c in ['d', 'r', 'a', 'f', 't'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert_eq!(app.input(), "draft", "the draft is written before the ask");

    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(submit_tx);
    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, reply_rx) = tokio::sync::oneshot::channel();
    requests_tx
        .send(dch_tui::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "allow bash?".to_string(),
            reply,
        })
        .expect("receiver alive");
    app.set_permission_requests(requests_rx);
    app.poll_permission_requests();

    for chord in [
        (KeyCode::Enter, KeyModifiers::CONTROL),
        (KeyCode::Char('m'), KeyModifiers::CONTROL),
        (KeyCode::Char('w'), KeyModifiers::CONTROL),
    ] {
        assert!(
            !app.handle_event(&key(chord.0, chord.1)),
            "{chord:?} is swallowed behind the prompt, redrawing nothing"
        );
    }
    assert!(
        submit_rx.try_recv().is_err(),
        "no chord submitted the draft behind the overlay"
    );
    assert_eq!(app.input(), "draft", "no chord edited the draft");

    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        reply_rx.blocking_recv(),
        Ok(true),
        "the prompt still answers after the chords"
    );
}

#[test]
fn modifier_decorated_keys_do_not_answer_the_prompt() {
    let (mut app, _tx, mut replies) = permission_app(&[("Bash", "allow bash?")]);
    let mut reply = replies.remove(0);
    assert!(!app.handle_event(&key(KeyCode::Char('y'), KeyModifiers::ALT)));
    assert!(reply.try_recv().is_err(), "Alt+y must not answer the ask");
    assert!(!app.handle_event(&key(KeyCode::Enter, KeyModifiers::ALT)));
    assert!(
        reply.try_recv().is_err(),
        "Alt+Enter must not answer either"
    );
    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        reply.blocking_recv(),
        Ok(true),
        "the plain key still answers"
    );
}

#[test]
fn ctrl_c_under_a_pending_prompt_serves_the_run_as_ever() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let running = Arc::clone(&kept.agent_running);
    running.store(true, std::sync::atomic::Ordering::SeqCst);
    let cancels: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&cancels);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(move || {
        sink.lock().expect("sink").push(());
    }));

    // A draft written while the run is in flight, before the ask
    // lands — the chord's first press must clear it, cancel comes on
    // the second, exactly as without a prompt.
    for c in ['n', 'o', 't', 'e'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert_eq!(app.input(), "note", "the draft predates the ask");

    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
    requests_tx
        .send(dch_tui::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "allow bash?".to_string(),
            reply,
        })
        .expect("receiver alive");
    app.set_permission_requests(requests_rx);
    app.poll_permission_requests();

    assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        0,
        "the first press clears the draft, cancelling nothing"
    );
    assert_eq!(app.input(), "", "the draft is gone");
    let terminal = render_to_buffer(&mut app, 80, 30);
    assert!(
        screen_text(&terminal).contains("allow bash?"),
        "the prompt stays up for the second press"
    );

    assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        1,
        "the second press passes through the prompt and cancels the run"
    );
    assert!(!app.is_quitting(), "a cancel never quits");

    running.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(
        app.tick_wake(std::time::Instant::now()),
        "the run's landing retires the prompt with a redraw"
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    assert!(
        !screen_text(&terminal).contains("Permission required"),
        "no prompt outlives its run"
    );
}
