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

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dch_config::DchConfig;
use dch_tui::TuiApp;
use dch_tui::message::{ActiveTool, ContentBlock, TuiMessage};
use dch_tui::theme::Theme;
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
    assert_eq!(app.cursor(), 0);
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
    assert_eq!(app.cursor(), 5);

    let terminal = render_to_buffer(&mut app, 80, 30);
    let buffer = terminal.backend().buffer();
    let row: String = (0..buffer.area.width)
        .map(|x| buffer[(x, 27)].symbol().to_string())
        .collect();
    assert!(row.contains("hello"), "input box shows the typed text");
}

#[test]
fn backspace_deletes_whole_characters() {
    let mut app = app();
    for c in ['h', 'é', 'l', 'l', 'o'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert_eq!(app.input(), "héllo");
    assert_eq!(app.cursor(), 6, "the cursor is a byte index");

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
    assert_eq!(accent_app.cursor(), 1);
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
    assert_eq!(app.cursor(), 0);
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
