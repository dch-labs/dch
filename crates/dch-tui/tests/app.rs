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
use ratatui::backend::TestBackend;

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
    assert_eq!(app.theme.name, Theme::default().name);
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
        .map(|x| buffer[(x, 27)].symbol().to_string())
        .collect();
    assert!(row.contains("hello"), "input box shows the typed text");
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
        caret.x, 4,
        "one border column plus three display columns of text"
    );

    let mut wide_app = TuiApp::new(config_with_theme("dracula"));
    for c in ['h', '😀'] {
        wide_app.handle_event(&plain(KeyCode::Char(c)));
    }
    let mut wide_terminal = render_to_buffer(&mut wide_app, 80, 30);
    let caret = wide_terminal.get_cursor_position().unwrap();
    assert_eq!(
        caret.x, 4,
        "one border column plus one column and a double-width glyph"
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
    assert!(
        !scrolled_view.contains("line 59") && scrolled_view.contains("line 44"),
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
fn quit_keys_set_quitting() {
    for quit_event in [
        key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        key(KeyCode::Char('d'), KeyModifiers::CONTROL),
        plain(KeyCode::Esc),
    ] {
        let mut app = app();
        assert!(app.handle_event(&quit_event));
        assert!(app.is_quitting());
    }

    let mut app = app();
    app.handle_event(&plain(KeyCode::Char('x')));
    assert!(!app.is_quitting());
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

    let input_left_border: Vec<char> = (25..30)
        .map(|y| {
            buffer[(0, y)]
                .symbol()
                .to_string()
                .chars()
                .next()
                .unwrap_or(' ')
        })
        .collect();
    assert!(
        input_left_border.contains(&'│'),
        "input box borders rows 26-28: {input_left_border:?}"
    );
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

    let heading_fg = Theme::default().markdown.header1.fg.unwrap_or_default();
    let bold_modifier = Theme::default().markdown.bold.add_modifier;
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
                name: "Read".to_string(),
                input_preview: "src/main.rs".to_string(),
                success: true,
                elapsed_secs: 0.42,
                output_preview: String::new(),
            },
            ContentBlock::Tool {
                name: "Grep".to_string(),
                input_preview: "\"todo\"".to_string(),
                success: false,
                elapsed_secs: 61.4,
                output_preview: String::new(),
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
        joined.contains("✓ Read src/main.rs (0.4s)"),
        "a successful tool renders its summary and elapsed stamp: {joined:?}"
    );
    assert!(
        joined.contains("✗ Grep \"todo\" (1m1s)"),
        "a failed tool renders the error marker and a minute-scale stamp: {joined:?}"
    );

    let theme = Theme::default();
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
                name: "Read".to_string(),
                input_preview: "a.rs".to_string(),
                success: true,
                elapsed_secs: 90.0,
                output_preview: String::new(),
            },
            ContentBlock::Tool {
                name: "Grep".to_string(),
                input_preview: "\"x\"".to_string(),
                success: true,
                elapsed_secs: 119.6,
                output_preview: String::new(),
            },
            ContentBlock::Tool {
                name: "Bash".to_string(),
                input_preview: "true".to_string(),
                success: true,
                elapsed_secs: 59.6,
                output_preview: String::new(),
            },
        ],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let joined = rows.join("\n");
    assert!(
        joined.contains("Read a.rs (1m30s)"),
        "90s rounds to 1m30s, not 2m30s: {joined:?}"
    );
    assert!(
        joined.contains("Grep \"x\" (2m0s)"),
        "119.6s rounds once to 2m0s, not 1m60s or 2m60s: {joined:?}"
    );
    assert!(
        joined.contains("Bash true (1m0s)"),
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
fn completed_tool_results_drain_into_a_bounded_history() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());

    for index in 0..25 {
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
        "the newest completed tool renders after its active line retires: {view:?}"
    );
    assert!(
        !view.contains("✓ tool-4"),
        "history older than the display depth drops off instead of scrolling everything: {view:?}"
    );
    assert!(
        kept.tool_results
            .lock()
            .expect("the results lock")
            .is_empty(),
        "the shared buffer drains on redraw, so it cannot accumulate across a session"
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

    let poisoned_replies = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = kept.completed_replies.lock().expect("the replies lock");
        panic!("poison the replies lock");
    }));
    assert!(
        poisoned_replies.is_err(),
        "the replies poisoning must unwind"
    );
    let poisoned_results = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = kept.tool_results.lock().expect("the results lock");
        panic!("poison the results lock");
    }));
    assert!(
        poisoned_results.is_err(),
        "the results poisoning must unwind"
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
        "poisoned tool results still drain into the history: {view:?}"
    );
    let assistant_count = app
        .conversation()
        .iter()
        .filter(|message| matches!(message, dch_tui::TuiMessage::Assistant { .. }))
        .count();
    assert_eq!(
        assistant_count, 2,
        "both replies graduated into the conversation through the poisoned lock"
    );
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

    let theme = Theme::default();
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

#[test]
fn the_mouse_wheel_scrolls_three_lines_per_event() {
    let mut app = app();
    assert!(app.handle_event(&wheel_event(MouseEventKind::ScrollUp)));
    assert_eq!(app.scroll_offset(), 3);
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
fn the_input_box_grows_with_wrapped_lines_and_shows_the_hint() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());

    let single = render_to_buffer(&mut app, 40, 24);
    let rows = row_texts(&single);
    assert!(
        rows[20].contains("shift+enter"),
        "the hint rides the single-row input box's border: {}",
        rows[20]
    );

    app.handle_event(&Event::Paste("one\ntwo".to_string()));
    let grown = render_to_buffer(&mut app, 40, 24);
    let rows = row_texts(&grown);
    assert!(
        rows[19].contains("shift+enter"),
        "a second row moves the border and its title up: {}",
        rows[19]
    );
    assert!(rows[20].contains("one"), "the first wrapped row renders");

    kept.queued.store(2, std::sync::atomic::Ordering::SeqCst);
    let queued = render_to_buffer(&mut app, 40, 24);
    assert!(
        row_texts(&queued)[19].contains("2 queued"),
        "the queued count prefixes the title while the driver has unclaimed sends"
    );
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
