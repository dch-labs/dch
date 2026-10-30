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

use unicode_width::UnicodeWidthChar as _;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use dch_config::DchConfig;
use dch_tui::TuiApp;
use dch_tui::message::{ActiveTool, ContentBlock, TuiMessage};
use dch_tui::theme::Theme;
use dch_tui::tool_render::{SPINNER_FRAME_MS, spinner_frame_at};
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

fn config_with_strip_top() -> DchConfig {
    let mut config = config_with_theme("dracula");
    config.display.strip_position = dch_config::StripPosition::Top;
    config
}

fn app() -> TuiApp {
    TuiApp::new(config_with_theme("dracula"))
}

/// An app over observer state the test keeps a handle to, plus that state.
///
/// The shared queue is the seam between the two halves of a session, so a
/// test that cares what is waiting reads it off the state rather than off
/// what the channel happens to carry.
fn app_with_state() -> (TuiApp, dch_tui::TuiObserverState) {
    let (observer, kept) = dch_tui::TuiObserverState::new().into_observer();
    drop(observer);
    (
        TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone()),
        kept,
    )
}

/// The waiting prompt a submission names.
///
/// Enter hands the driver a `Run`; the channel's other message is a
/// different test's subject, and naming it here beats a bare unwrap that
/// says nothing when the wrong one turns up.
fn run_id(submission: dch_tui::Submission) -> u64 {
    match submission {
        dch_tui::Submission::Run { id } => id,
        other @ dch_tui::Submission::Correct { .. } => {
            panic!("a plain submit hands over a run, got {other:?}")
        }
    }
}

/// What the shared queue holds, as `(id, text)` pairs in send order.
fn waiting(kept: &dch_tui::TuiObserverState) -> Vec<(u64, String)> {
    let queue = kept.queued.lock().unwrap();
    (0..queue.len())
        .filter_map(|index| queue.get(index))
        .map(|entry| (entry.id, entry.text.clone()))
        .collect()
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
            steered: false,
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

    let terminal = render_to_buffer(&mut app, 80, 31);
    let buffer = terminal.backend().buffer();
    let row: String = (0..buffer.area.width)
        .map(|x| buffer[(x, 25)].symbol().to_string())
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
    let (mut app, kept) = app_with_state();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    for c in ['h', 'i'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    let handed_over = run_id(rx.try_recv().unwrap());
    assert_eq!(
        waiting(&kept),
        vec![(handed_over, "hi".to_string())],
        "Enter hands the driver an id and leaves the text itself in the shared \
         queue for the driver to read at its turn's start — the id is the \
         handle, the queue holds the words, and this is what lets a prompt be \
         fixed after it has been handed over"
    );

    for c in [' ', '\t'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert!(
        rx.try_recv().is_err(),
        "a whitespace-only submit sends nothing"
    );
    assert_eq!(waiting(&kept).len(), 1, "and it queues nothing either");
}

#[test]
fn submissions_queue_in_order_while_the_driver_is_busy() {
    let (mut app, kept) = app_with_state();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    for text in ["first task", "second task"] {
        for c in text.chars() {
            app.handle_event(&plain(KeyCode::Char(c)));
        }
        app.handle_event(&plain(KeyCode::Enter));
    }

    let first = run_id(rx.try_recv().unwrap());
    let second = run_id(rx.try_recv().unwrap());
    assert!(
        first < second,
        "ids travel the channel in the order their prompts were submitted"
    );
    assert_eq!(
        waiting(&kept),
        vec![
            (first, "first task".to_string()),
            (second, "second task".to_string()),
        ],
        "submits buffer while nothing drains the channel, and queued submits \
         keep their arrival order"
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
            steered: false,
        });
    }
    app.handle_event(&plain(KeyCode::PageDown));
    assert_eq!(
        app.scroll_offset(),
        0,
        "back at the bottom before rendering"
    );
    let at_bottom = render_to_buffer(&mut app, 40, 16);
    let bottom_view = view_text(&at_bottom, 40, 16);
    assert!(
        bottom_view.contains("line 59"),
        "the bottom view shows the newest message: {bottom_view:?}"
    );

    assert!(app.handle_event(&plain(KeyCode::PageUp)));
    let scrolled = render_to_buffer(&mut app, 40, 16);
    let scrolled_view = view_text(&scrolled, 40, 16);
    // Each user turn renders pad, text, pad — three lines — so at
    // three content rows (under the pane's own padding row) the
    // PageUp lift of ten lines lands the window inside turns three
    // back from the newest.
    assert!(
        !scrolled_view.contains("line 59") && scrolled_view.contains("line 56"),
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
fn ctrl_c_ctrl_c_under_a_run_is_one_cancel_and_exit_action() {
    // The Esc-Esc shape with the exit riding the fire: the first
    // press only arms — nothing stops, nothing leaves — and the
    // second press is one action: the run cancels exactly as Esc-Esc
    // cancels it, and the program terminates. A stray key between
    // the presses stands the whole thing down.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cancels: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&cancels);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(move || {
        sink.lock().expect("sink").push(());
    }));

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        0,
        "the first press only arms — the run keeps going"
    );
    assert!(!app.is_quitting(), "the arming press leaves nothing");
    let armed = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&armed)
            .iter()
            .any(|row| row.contains("press ctrl+c again to cancel and exit")),
        "the arming press names the one action it arms: {:?}",
        row_texts(&armed)
    );

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        1,
        "the second press fires the cancel — the Esc-Esc cancel itself"
    );
    assert!(
        app.is_quitting(),
        "and the same press terminates the program — one action"
    );

    // A stray key between the presses stands it down: the second
    // pair re-arms rather than firing on a stale intent.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cancels: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&cancels);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(move || {
        sink.lock().expect("sink").push(());
    }));
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    app.handle_event(&plain(KeyCode::Char('x')));
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        0,
        "the stray key stood the action down"
    );
    assert!(!app.is_quitting(), "nothing fires from stale intent");
}
#[test]
fn the_ladder_press_raises_the_shutdown_flag_before_the_cancel() {
    // The exit's own cancel releases the driver's next claim: the
    // run resolves cancelled, and the driver reads the shutdown
    // flag before it takes the queue's next prompt. The ladder
    // therefore raises the flag in the same press, ahead of the
    // canceller — and the pin is that order itself: the canceller,
    // called by the fire, must find the flag already up. Raised
    // only after the loop instead, a prompt queued behind the
    // quitting user is claimed after the fire, missing from the
    // exit's own snapshot, and resumed forever as a stop no one
    // asked for.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_shutdown_flag(Arc::clone(&flag));
    let saw: Arc<std::sync::Mutex<Vec<bool>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&saw);
    let at_cancel = Arc::clone(&flag);
    app.set_run_canceller(Box::new(move || {
        sink.lock()
            .expect("sink")
            .push(at_cancel.load(std::sync::atomic::Ordering::SeqCst));
    }));

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        !flag.load(std::sync::atomic::Ordering::SeqCst),
        "the arming press raises nothing — the flag belongs to the fire"
    );
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        *saw.lock().expect("sink"),
        [true],
        "the canceller was called once, and found the flag already raised beside it"
    );
    assert!(
        flag.load(std::sync::atomic::Ordering::SeqCst) && app.is_quitting(),
        "the flag stands with the exit requested"
    );
}

#[test]
fn the_esc_cancel_leaves_the_shutdown_flag_alone() {
    // Cancel is not exit. Esc-Esc stops the run in flight and the
    // session stays up, so the prompts queued behind the cancelled
    // one are the designed next turns — the driver must keep
    // claiming them. A flag raised by this press would strand the
    // very backlog the cancel exists to release.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_shutdown_flag(Arc::clone(&flag));
    let cancels: Arc<std::sync::Mutex<usize>> = Arc::new(std::sync::Mutex::new(0));
    let sink = Arc::clone(&cancels);
    app.set_run_canceller(Box::new(move || {
        *sink.lock().expect("sink") += 1;
    }));

    app.handle_event(&key(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_event(&key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        *cancels.lock().expect("sink"),
        1,
        "Esc-Esc cancelled the run as ever"
    );
    assert!(
        !flag.load(std::sync::atomic::Ordering::SeqCst),
        "a cancel that is not an exit never raises the driver's gate"
    );
    assert!(
        !app.is_quitting(),
        "and the session stays up to run what waits"
    );
}

#[test]
fn quitting_at_rest_raises_the_shutdown_flag_too() {
    // The no-run half of the chord requests the exit with no run to
    // cancel — and the flag still belongs up whatever the display
    // sent last: a correction or prompt the driver has not pulled
    // yet must not start into an exit that has already begun.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_shutdown_flag(Arc::clone(&flag));

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.is_quitting(), "the idle chord quits as ever");
    assert!(
        flag.load(std::sync::atomic::Ordering::SeqCst),
        "and the same press gates the driver"
    );
}

#[test]
fn ctrl_c_clears_the_draft_before_arming_a_running_exit() {
    // The ladder under a run is clear, arm, fire — the same order it
    // keeps at rest. A press that armed the exit over a standing
    // draft would lose the draft to the second press: the paste was
    // never offered to the exit, and clearing is not consenting, so
    // the composer empties first and the exit needs its presses
    // behind an empty buffer like everywhere else.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cancels: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&cancels);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(move || {
        sink.lock().expect("sink").push(());
    }));
    for character in "a paste with somewhere to go".chars() {
        app.handle_event(&plain(KeyCode::Char(character)));
    }

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        app.input().is_empty(),
        "the first press clears the draft — the run never sees it"
    );
    assert_eq!(
        cancels.lock().expect("sink").len(),
        0,
        "nothing cancels on the clearing press"
    );
    assert!(!app.is_quitting(), "clearing arms nothing under a run");
    let cleared = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&cleared)
            .iter()
            .any(|row| row.contains("press ctrl+c twice to cancel and exit")),
        "the cleared draft names the ladder it did not arm: {:?}",
        row_texts(&cleared)
    );

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        0,
        "the second press only arms — the run keeps going"
    );
    assert!(!app.is_quitting(), "the arming press leaves nothing");

    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        1,
        "the third press fires the cancel"
    );
    assert!(app.is_quitting(), "and terminates beside it");
}

#[test]
fn the_status_bar_shows_the_model_and_the_session_id() {
    // One status line, two stable facts: which model answers and
    // which session this is — the id a later `--resume` wants,
    // readable straight off the bar.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_session_id("f005d097-c4ce-477a-88c0-2fc2cec5a466".to_string());
    let bar = render_to_buffer(&mut app, 100, 24);
    let rows = row_texts(&bar);
    let status = rows
        .iter()
        .find(|r| r.contains("f005d097-c4ce-477a-88c0-2fc2cec5a466"))
        .expect("the bar renders");
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
    app.handle_event(&plain(KeyCode::Esc));
    app.handle_event(&plain(KeyCode::Esc));
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
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
fn the_composer_info_row_names_the_current_permission_mode() {
    // The mode segment rides the composer's info row with the model
    // and its effort, in the config's own spelling, so what the box
    // shows is exactly what the user set.
    for (mode, segment) in [
        (dch_config::PermissionMode::Auto, "mode: auto"),
        (dch_config::PermissionMode::Plan, "mode: plan"),
        (
            dch_config::PermissionMode::AcceptEdits,
            "mode: accept_edits",
        ),
        (dch_config::PermissionMode::Interactive, "mode: interactive"),
    ] {
        let mut config = config_with_theme("dracula");
        config.runner.permission_mode = mode;
        let mut app = TuiApp::new(config);
        let rows = row_texts(&render_to_buffer(&mut app, 80, 30));
        let info = rows
            .iter()
            .find(|row| row.contains("testmodel"))
            .expect("the composer's info row renders");
        assert!(
            info.contains(segment),
            "the info row names `{segment}`: {info:?}"
        );
    }
}

#[test]
fn the_status_bar_omits_the_turn_counter() {
    // The engine's turn count is budget bookkeeping, not something
    // the driver needs in view — the bar carries no turn segment
    // even mid-run, and no context-size segment either: the usage
    // dch receives carries only the uncached input slice, which is
    // not the conversation size, so no honest number exists.
    let state = dch_tui::TuiObserverState::new();
    *state.current_turn.lock().unwrap() = Some(36);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), state);
    let terminal = render_to_buffer(&mut app, 100, 30);
    let buffer = terminal.backend().buffer();
    let status_row: String = (0..100)
        .map(|x| buffer[(x, 29)].symbol().to_string())
        .collect();
    assert!(
        !status_row.contains("turn "),
        "no turn segment rides the bar: {status_row:?}"
    );
    assert!(
        !status_row.contains("CTX:"),
        "no context segment renders without honest data: {status_row:?}"
    );
}

#[test]
fn clicking_the_session_id_segment_copies_it() {
    // The id segment in the status bar is a click target: a plain
    // click inside it routes the id through the selection transport
    // and posts the copy notice, exactly as a text selection would.
    // The span is pinned at both boundaries — its final cell copies,
    // the separator cell just before it does not.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let id = "f005d097-c4ce-477a-88c0-2fc2cec5a466";
    app.set_session_id(id.to_string());
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));
    let probe = render_to_buffer(&mut app, 100, 24);
    let rows = row_texts(&probe);
    let status = rows
        .iter()
        .find(|r| r.contains("f005d097"))
        .expect("the bar renders the id");
    let id_start = cell_col(status, id);
    let id_width: usize = id.chars().map(|c| c.width().unwrap_or(0)).sum();
    let last_cell = u16::try_from(id_start.saturating_add(id_width).saturating_sub(1)).unwrap_or(0);
    let separator_cell = u16::try_from(id_start.saturating_sub(1)).unwrap_or(0);
    let row = rows
        .iter()
        .position(|r| r.contains("f005d097"))
        .unwrap_or(0);
    let row = u16::try_from(row).unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        last_cell,
        row,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        last_cell,
        row,
    ));
    let recorded = copied.lock().expect("sink").clone();
    assert_eq!(
        recorded,
        vec![id.to_string()],
        "the click on the segment's final cell copies the id"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        separator_cell,
        row,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        separator_cell,
        row,
    ));
    let recorded = copied.lock().expect("sink").clone();
    assert_eq!(
        recorded.len(),
        1,
        "a click on the separator before the id copies nothing"
    );
}

#[test]
fn the_composer_info_row_parenthesizes_the_reasoning_effort_beside_the_model() {
    // The model segment carries the effort — one glance, one fact —
    // with the level capitalized beside the model name and `Auto`
    // standing in when no level is configured: the provider's own
    // thinking defaults apply then, not none.
    for (level, label) in [
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("max", "Max"),
    ] {
        let mut config = config_with_theme("dracula");
        config.api.reasoning_effort = Some(level.to_string());
        let mut app = TuiApp::new(config);
        let rows = row_texts(&render_to_buffer(&mut app, 80, 30));
        let info = rows
            .iter()
            .find(|row| row.contains("testmodel"))
            .expect("the composer's info row renders");
        assert!(
            info.contains(&format!("testmodel ({label})")),
            "the model names its effort `({label})`: {info:?}"
        );
    }
    let config = config_with_theme("dracula");
    let mut app = TuiApp::new(config);
    let rows = row_texts(&render_to_buffer(&mut app, 80, 30));
    let info = rows
        .iter()
        .find(|row| row.contains("testmodel"))
        .expect("the composer's info row renders");
    assert!(
        info.contains("testmodel (Auto)"),
        "an unset effort reads as `(Auto)`: {info:?}"
    );
}

#[test]
fn the_place_row_names_the_working_directory_above_the_composer() {
    // The session's where rides its own row directly above the
    // composer, outside the box: the path spelled per the config's
    // `path_style` — full by default, `~/`-relative on request — with
    // the git branch in parentheses when the host named one, and a
    // row the frame never spends until a directory is named.
    let mut app = app();
    let unset = render_to_buffer(&mut app, 100, 24);
    assert!(
        !row_texts(&unset).iter().any(|row| row.contains("repos")),
        "no place row renders until the host names a directory"
    );

    app.set_working_dir(std::path::Path::new("/home/tester/repos/dch"));
    app.set_git_branch(Some("master".to_string()));
    let terminal = render_to_buffer(&mut app, 100, 24);
    let buffer = terminal.backend().buffer();
    let rows = row_texts(&terminal);
    let place_at = rows
        .iter()
        .position(|row| row.contains("/home/tester/repos/dch (master)"))
        .expect("the place row renders the full path with its branch");
    assert!(
        !rows[place_at].contains("/home/tester/repos/dch/"),
        "the branch closes the path's parenthesis, not the path: {:?}",
        rows[place_at]
    );
    let place_row = u16::try_from(place_at).unwrap_or(0);
    assert_eq!(
        buffer[(10, place_row)].bg,
        ratatui::style::Color::Reset,
        "the place row sits on the plain background, outside the box"
    );
    assert_eq!(
        buffer[(10, place_row + 1)].bg,
        app.theme.ui.surface,
        "the caption closes tight against the box: the surface opens on the very next row"
    );
    let numbers_at = rows
        .iter()
        .position(|row| row.contains("0k/200k"))
        .expect("the footer's bottom row renders");
    assert!(
        !rows[numbers_at].contains("repos"),
        "the footer carries no directory: {:?}",
        rows[numbers_at]
    );

    app.set_git_branch(None);
    let bare = render_to_buffer(&mut app, 100, 24);
    assert!(
        row_texts(&bare)
            .iter()
            .any(|row| row.trim() == "/home/tester/repos/dch"),
        "no branch renders as the bare path"
    );

    // The relative style spells the path from home: a directory under
    // the user's actual home keeps its tail under the tilde, and one
    // outside home falls back to the full path.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_string());
    let mut relative_config = config_with_theme("dracula");
    relative_config.display.path_style = dch_config::PathStyle::Relative;
    let mut relative = TuiApp::new(relative_config);
    relative.set_working_dir(std::path::Path::new(&format!("{home}/repos/dch")));
    relative.set_git_branch(Some("master".to_string()));
    let spelled = row_texts(&render_to_buffer(&mut relative, 100, 24));
    assert!(
        spelled
            .iter()
            .any(|row| row.contains("~/repos/dch (master)")),
        "the relative style spells the path from home: {spelled:?}"
    );
    let mut outside_config = config_with_theme("dracula");
    outside_config.display.path_style = dch_config::PathStyle::Relative;
    let mut outside = TuiApp::new(outside_config);
    outside.set_working_dir(std::path::Path::new("/srv/work"));
    let spelled = row_texts(&render_to_buffer(&mut outside, 100, 24));
    assert!(
        spelled.iter().any(|row| row.contains("/srv/work")),
        "a directory outside home falls back to the full path: {spelled:?}"
    );
}

#[test]
fn layout_shows_three_panes() {
    let mut app = app();
    let terminal = render_to_buffer(&mut app, 80, 30);
    let buffer = terminal.backend().buffer();

    let info_row: String = (0..80)
        .map(|x| buffer[(x, 26)].symbol().to_string())
        .collect();
    assert!(
        info_row.contains("testmodel"),
        "the composer's identity row names the model: {info_row:?}"
    );

    // A blank spacer row separates the conversation from the input
    // field; the field is the four rows above the footer — a padding
    // row, the text row, another padding row, and the identity row
    // on the box's bottom edge — a pure background tint with glyphs
    // nowhere on its edge, no text of its own in the middle.
    let spacer_row: String = (0..80)
        .map(|x| buffer[(x, 22)].symbol().to_string())
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
    let top_pad = field_row(23);
    let bottom_pad = field_row(25);
    for row in [top_pad, bottom_pad] {
        assert!(
            row.iter().all(|c| *c == ' '),
            "the field's vertical padding rows are pure blank tint: {row:?}"
        );
    }
    // Square corners: the fill reaches every corner cell — the tint runs
    // to the pane's left edge, where the primary accent column opens
    // the composer; no clipping and no glyphs.
    assert_eq!(
        buffer[(1, 23)].bg,
        buffer[(2, 23)].bg,
        "the fill beside the accent edge carries one continuous tint"
    );
    assert_eq!(
        buffer[(0, 23)].bg,
        app.theme.ui.primary,
        "the corner cell opens the composer's primary edge column"
    );
    let first_text = field_row(24);
    assert!(
        first_text.iter().take(2).all(|c| *c == ' '),
        "the field's first text row starts after horizontal padding: {:?}",
        first_text
    );
    for row in [field_row(23), field_row(24), field_row(25)] {
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

/// The cell column where `needle` starts in a rendered row.
///
/// Rendered rows carry chrome glyphs (the user-turn chevron) whose
/// byte length differs from their cell width, and `str::find` answers
/// in bytes while clicks address cells — this walks display width so a
/// multibyte glyph before the text cannot skew a click's column.
fn cell_col(row: &str, needle: &str) -> usize {
    row.find(needle).map_or(0, |byte| {
        row[..byte].chars().map(|c| c.width().unwrap_or(0)).sum()
    })
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
        .push(dch_tui::Graduation::Reply {
            text: "the finished reply".to_string(),
            thinking: None,
        });
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let snapshots: Arc<std::sync::Mutex<Snapshots>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&snapshots);
    app.set_turn_end_hook(Box::new(move |conversation, machine| {
        snapshots_lock(&sink).push(conversation.to_vec());
        assert!(machine.is_none(), "no turn-end history rides this drain");
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
fn a_turn_end_hook_receives_the_newest_turn_end_history() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Reply {
            text: "mid-run reply".to_string(),
            thinking: None,
        });
    let older = Arc::new(vec![loopctl::message::Message::user("the prompt")]);
    let newer = Arc::new(vec![
        loopctl::message::Message::user("the prompt"),
        loopctl::message::Message::assistant("mid-run reply"),
    ]);
    kept.turn_histories
        .lock()
        .expect("the histories lock")
        .extend([older, newer]);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let histories: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&histories);
    app.set_turn_end_hook(Box::new(move |_conversation, machine| {
        let length = machine.map_or(usize::MAX, |history| history.len());
        sink.lock().expect("the sink lock").push(length);
    }));
    drop(render_to_buffer(&mut app, 80, 30));
    let seen = histories.lock().expect("the sink lock").clone();
    assert_eq!(
        *seen,
        vec![2],
        "the hook fired once, with the newest turn-end history: {seen:?}"
    );
}

#[test]
fn a_history_without_display_events_still_fires_the_hook() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.turn_histories
        .lock()
        .expect("the histories lock")
        .push(Arc::new(vec![loopctl::message::Message::user(
            "the prompt",
        )]));
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    let snapshots: Arc<std::sync::Mutex<Snapshots>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&snapshots);
    app.set_turn_end_hook(Box::new(move |conversation, machine| {
        snapshots_lock(&sink).push(conversation.to_vec());
        assert!(machine.is_some(), "the history reaches the host");
    }));
    drop(render_to_buffer(&mut app, 80, 30));
    assert_eq!(
        snapshots_lock(&snapshots).len(),
        1,
        "a turn end with nothing to graduate still publishes its history"
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
    app.set_turn_end_hook(Box::new(move |conversation, machine| {
        snapshots_lock(&sink).push(conversation.to_vec());
        assert!(machine.is_none(), "no turn-end history rides this drain");
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
    app.set_turn_end_hook(Box::new(move |conversation, machine| {
        snapshots_lock(&sink).push(conversation.to_vec());
        assert!(machine.is_none(), "no turn-end history rides this drain");
    }));
    drop(render_to_buffer(&mut app, 80, 30));
    assert!(
        snapshots_lock(&snapshots).is_empty(),
        "a frame that graduates nothing fires nothing"
    );

    kept.graduations
        .lock()
        .expect("the queue lock")
        .push(dch_tui::Graduation::Reply {
            text: "a late reply".to_string(),
            thinking: None,
        });
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
        steered: false,
    });
    app.push_message(TuiMessage::User {
        text: "klmnopqrst".to_string(),
        timestamp: now,
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
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
    let col = cell_col(&rows[row], "abcdefghij");

    let press = mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    app.handle_event(&press);
    let second = rows
        .iter()
        .position(|r| r.contains("klmnopqrst"))
        .unwrap_or(0);
    let drag = mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(second).unwrap_or(0),
    );
    app.handle_event(&drag);
    let up = mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(second).unwrap_or(0),
    );
    app.handle_event(&up);

    let copied = copied.lock().expect("sink").clone();
    assert_eq!(copied.len(), 1, "exactly one silent copy");
    // the turns' padding rows ride the span as blank content lines
    let expected = "cdefghij\n\n\nklmnop".to_string();
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
fn a_user_turn_opens_with_the_primary_chevron() {
    // The turn marker is chrome, not content: the chevron overlays a
    // reserved gutter beside the user turn's first row in the theme's
    // primary, so each agent answer starts where a marked block ends —
    // and a copy never carries the glyph.
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "first line of the question\nsecond line".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "the answer".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let marked = rows
        .iter()
        .position(|r| r.contains("first line of the question"))
        .expect("the user turn's first row");
    let buffer = probe.backend().buffer();
    let chevron = &buffer[(2, u16::try_from(marked).unwrap_or(0))];
    assert_eq!(
        chevron.symbol(),
        "❯",
        "the chevron rides the gutter beside the turn's first row"
    );
    assert_eq!(
        chevron.fg, app.theme.ui.primary,
        "the chevron takes the theme's primary accent"
    );
    let marked_row = u16::try_from(marked).unwrap_or(0);
    let answer = rows
        .iter()
        .position(|r| r.contains("the answer"))
        .expect("the answer row");
    for (x, label) in [(5u16, "at the text"), (77, "at the box's right edge")] {
        assert_eq!(
            buffer[(x, marked_row)].bg,
            app.theme.ui.surface,
            "the user turn paints the prompt box's surface {label}"
        );
    }
    assert_eq!(
        buffer[(0, marked_row)].bg,
        ratatui::style::Color::Reset,
        "the box keeps its margin outside its left edge"
    );
    assert_eq!(
        buffer[(78, marked_row)].bg,
        ratatui::style::Color::Reset,
        "the box keeps the same margin outside its right edge"
    );
    let answer_row = u16::try_from(answer).unwrap_or(0);
    assert_eq!(
        buffer[(40, answer_row)].bg,
        ratatui::style::Color::Reset,
        "assistant rows stay on the terminal's own background"
    );

    // The paintless chrome paints nothing: no surface block, the turn
    // keeps the terminal's own background beside its chevron glyph.
    let mut ruled = TuiApp::new(config_with_theme("transparent"));
    let frame = render_to_buffer(&mut ruled, 80, 24);
    let buffer = frame.backend().buffer();
    assert_eq!(
        buffer[(40, marked_row)].bg,
        ratatui::style::Color::Reset,
        "the paintless theme draws no user-turn surface"
    );
}

#[test]
fn a_drag_shows_the_highlight_before_release() {
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });
    let _ = render_to_buffer(&mut app, 80, 24);
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
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
    // padding row below — thin and solid on every terminal, glyphs
    // and paint nowhere. The bottom rule rides one row above the
    // identity line, so the model and mode hang below the box rather
    // than sitting on their own border.
    let mut ruled = TuiApp::new(config_with_theme("transparent"));
    let frame = render_to_buffer(&mut ruled, 80, 25);
    let buffer = frame.backend().buffer();
    for x in [1u16, 40, 78] {
        let top = buffer[(x, 17)].style();
        assert!(
            top.add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "the spacer row carries the top rule"
        );
        assert_eq!(top.fg, Some(ratatui::style::Color::Indexed(8)));
        let bottom = buffer[(x, 20)].style();
        assert!(
            bottom
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "the composer's padding row carries the bottom rule"
        );
    }
    // The identity row hangs below the rule — outside the box — and
    // keeps the terminal's own canvas, no underline crossing it.
    let identity = buffer[(40, 21)].style();
    assert!(
        !identity
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED),
        "the identity text sits clear of the rule it once rode"
    );
    for cell in [(40u16, 19u16), (40, 21)] {
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
    let plain_frame = render_to_buffer(&mut plain, 80, 25);
    let buffer = plain_frame.backend().buffer();
    let style = buffer[(40, 20)].style();
    assert_eq!(style.bg, Some(ratatui::style::Color::Rgb(68, 71, 90)));
    assert_eq!(buffer[(1, 19)].symbol(), " ");
}

#[test]
fn the_transparent_theme_stands_its_captions_outside_the_rules() {
    // The transparent theme's rules draw a box around the composer,
    // and captions that lived inside it read as crossing their own
    // borders: the directory's name sat between the top rule and the
    // box, and the model and mode sat on the underlined bottom row
    // itself. Ruled themes seat both outside — the caption above the
    // top rule, the identity below the bottom one — while painted
    // themes keep the caption tight against the composer's surface,
    // where nothing crosses anything.
    let mut ruled = TuiApp::new(config_with_theme("transparent"));
    ruled.set_working_dir(std::path::Path::new("/home/tester/repos/dch"));
    let rows = row_texts(&render_to_buffer(&mut ruled, 80, 25));
    let caption_row = rows
        .iter()
        .position(|row| row.contains("/home/tester/repos/dch"))
        .expect("the caption renders");
    let spacer_row = rows
        .iter()
        .position(|row| row.contains("testmodel"))
        .map(|identity| identity.saturating_sub(3))
        .expect("the identity renders");
    assert!(
        caption_row < spacer_row,
        "the caption stands above the top rule, outside the box: {rows:?}"
    );

    // The identity hangs below the bottom rule: the row above it is
    // underlined, its own row is not.
    let frame = render_to_buffer(&mut ruled, 80, 25);
    let buffer = frame.backend().buffer();
    let identity_row: u16 = u16::try_from(
        rows.iter()
            .position(|row| row.contains("testmodel"))
            .expect("the identity renders"),
    )
    .unwrap_or(0);
    let above = buffer[(40, identity_row.saturating_sub(1))].style();
    assert!(
        above
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED),
        "the bottom rule rides the padding row above the identity"
    );
    let identity = buffer[(40, identity_row)].style();
    assert!(
        !identity
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED),
        "the model and mode hang below the rule, clear of it"
    );

    // Painted themes keep the caption against the composer's paint,
    // unchanged: the surface opens on the very row after the caption.
    let mut painted = app();
    painted.set_working_dir(std::path::Path::new("/home/tester/repos/dch"));
    let frame = render_to_buffer(&mut painted, 80, 25);
    let buffer = frame.backend().buffer();
    let rows = row_texts(&frame);
    let caption_row = rows
        .iter()
        .position(|row| row.contains("/home/tester/repos/dch"))
        .expect("the caption renders");
    let caption_row = u16::try_from(caption_row).unwrap_or(0);
    assert_eq!(
        buffer[(10, caption_row.saturating_add(1))].bg,
        painted.theme.ui.surface,
        "painted themes keep the caption tight against the composer's surface"
    );
}

#[test]
fn the_composer_carries_a_solid_primary_column_on_its_left_edge() {
    // 25 rows put the composer pane on rows 18..=21 with the text
    // window on row 19, starting two columns in — the edge column owns
    // column 0 alone, thick by construction (paint, not glyphs).
    let mut plain = app();
    let frame = render_to_buffer(&mut plain, 80, 25);
    let buffer = frame.backend().buffer();
    for y in 18..=21u16 {
        assert_eq!(
            buffer[(0, y)].bg,
            plain.theme.ui.primary,
            "every composer row opens with the primary edge"
        );
    }
    assert_eq!(
        buffer[(1, 18)].bg,
        plain.theme.ui.surface,
        "the composer body beside the edge keeps the surface color"
    );

    // The composer's bottom margin is air: the row beneath it carries
    // no paint at all — the terminal's own background — and the
    // numbers row at the bottom sits on the bar's canvas, skipping the
    // bar's row between them, which carries the track's own fill.
    assert_ne!(
        plain.theme.ui.background, plain.theme.ui.surface,
        "the palettes keep the bar's canvas distinct from the raised surface"
    );
    assert_eq!(
        buffer[(0, 22)].bg,
        ratatui::style::Color::Reset,
        "the air beneath the composer stays unpainted — a margin, not a band"
    );
    assert_eq!(
        buffer[(0, 24)].bg,
        plain.theme.ui.background,
        "the numbers row sits on the bar's canvas at the frame's bottom"
    );

    // The paintless chrome stays paintless: no edge column, the pane
    // keeps the terminal's own background down its whole left side
    // (its primary is Reset — a bg of Reset would be the invisible
    // null mark, so the theme opts out of the edge entirely).
    let mut ruled = TuiApp::new(config_with_theme("transparent"));
    let frame = render_to_buffer(&mut ruled, 80, 25);
    let buffer = frame.backend().buffer();
    for y in 18..=21u16 {
        assert_eq!(
            buffer[(0, y)].bg,
            ratatui::style::Color::Reset,
            "the paintless composer paints nothing, edge included"
        );
    }
    assert_eq!(
        buffer[(1, 18)].bg,
        ratatui::style::Color::Reset,
        "beside the absent edge the paintless composer stays unpainted"
    );
}

#[test]
fn a_press_in_the_composer_places_the_caret_where_it_landed() {
    // 25 rows put the composer pane on rows 18..=21 with its text
    // window on row 19, text starting two columns in.
    let mut app = app();
    app.handle_event(&Event::Paste("hello world".to_string()));
    let mut first = render_to_buffer(&mut app, 80, 25);
    let caret = first.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (2 + 11, 19),
        "after the paste the caret sits at the text's end"
    );

    // a press between the two ls lands the caret mid-word
    assert!(app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2 + 4,
        19,
    )));
    let mut clicked = render_to_buffer(&mut app, 80, 25);
    let caret = clicked.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (6, 19),
        "the caret lands on the pressed cell"
    );

    // a press far past the text clamps to the row's end
    assert!(app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        70,
        20,
    )));
    let mut clamped = render_to_buffer(&mut app, 80, 25);
    let caret = clamped.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (2 + 11, 19),
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

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
fn a_released_selection_copy_confirms_on_the_notice_row() {
    // The release hands the covered span to the clipboard and says so:
    // the notice row above the composer carries the confirmation while
    // its hold lasts.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

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
        copied.lock().expect("sink").as_slice(),
        ["cdef"],
        "the release copied the covered span"
    );
    let noticed = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&noticed)
            .iter()
            .any(|r| r.contains("selection copied")),
        "the copy confirms on the notice row: {:?}",
        row_texts(&noticed)
    );
}

#[test]
fn a_click_without_travel_copies_and_notices_nothing() {
    // A click is a toggle, not a copy: the clipboard stays alone and
    // the notice row stays blank.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 2).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));

    assert!(
        copied.lock().expect("sink").is_empty(),
        "a click without travel copies nothing"
    );
    let noticed = render_to_buffer(&mut app, 80, 24);
    assert!(
        !row_texts(&noticed)
            .iter()
            .any(|r| r.contains("selection copied")),
        "the notice row stays blank without a copy: {:?}",
        row_texts(&noticed)
    );
}

#[test]
fn a_copy_no_transport_delivers_notices_the_failure() {
    // A copier that reports nothing delivered flips the wording to
    // name the escape hatch instead of claiming success.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });
    app.set_selection_copier(Box::new(|_attempt, _text| dch_tui::CopyAnswer::Failed));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

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

    let noticed = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&noticed)
            .iter()
            .any(|r| r.contains("copy failed — set display.copy_command")),
        "an undelivered copy names the escape hatch: {:?}",
        row_texts(&noticed)
    );
    assert!(
        !row_texts(&noticed)
            .iter()
            .any(|r| r.contains("selection copied")),
        "it does not also claim success: {:?}",
        row_texts(&noticed)
    );
}

#[test]
fn a_hanging_copy_command_never_blocks_the_render_task() {
    // The release must not wait on the helper: the chain runs off the
    // render path under a kill deadline, and the unverified verdict
    // posts nothing — an escape whose delivery nothing confirmed is
    // silence, not a claim.
    let mut config = config_with_theme("dracula");
    config.display.copy_command = Some("sleep 30".to_string());
    let mut app = TuiApp::new(config);
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

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
    let began = std::time::Instant::now();
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    ));
    assert!(
        began.elapsed() < std::time::Duration::from_secs(1),
        "the release returns without waiting on the hanging helper"
    );

    let mut silenced = true;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let frame = render_to_buffer(&mut app, 80, 24);
        if row_texts(&frame)
            .iter()
            .any(|r| r.contains("copied") || r.contains("copy failed"))
        {
            silenced = false;
            break;
        }
    }
    assert!(
        silenced,
        "a killed-helper verdict that nothing confirmed never claims the notice row"
    );
}

#[test]
fn a_pending_transport_copy_posts_no_row_until_the_verdict() {
    // Transports in flight owe a verdict: the release posts no
    // wording of its own, so nothing flashes ahead of the outcome.
    let mut config = config_with_theme("dracula");
    config.display.copy_command = Some("sleep 30".to_string());
    let mut app = TuiApp::new(config);
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

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

    let frame = render_to_buffer(&mut app, 80, 24);
    assert!(
        !row_texts(&frame)
            .iter()
            .any(|r| r.contains("copied") || r.contains("copy failed")),
        "the pending window posts nothing: {:?}",
        row_texts(&frame)
    );
}

#[test]
fn the_copy_chord_confirms_on_the_notice_row() {
    // Ctrl+Shift+C delivers through the same funnel as the release,
    // so its copy lands the same confirmation on the notice row.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

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
    assert!(
        app.handle_event(&key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        "the chord is handled"
    );

    assert_eq!(
        copied.lock().expect("sink").as_slice(),
        ["cdef"],
        "the chord copied the covered span"
    );
    let noticed = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&noticed)
            .iter()
            .any(|r| r.contains("selection copied")),
        "the chord's copy confirms on the notice row: {:?}",
        row_texts(&noticed)
    );
}

#[test]
fn a_shift_arrow_recopy_confirms_on_the_notice_row() {
    // The keyboard walk re-copies as the head moves; each delivery
    // refreshes the same confirmation row rather than stacking rows.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "abcdefghij".to_string(),
        timestamp: now,
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        u16::try_from(col).unwrap_or(0),
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
    app.handle_event(&key(KeyCode::Right, KeyModifiers::SHIFT));

    assert_eq!(
        copied.lock().expect("sink").as_slice(),
        ["abc", "abcd"],
        "the release and the walk's step each copied once"
    );
    let noticed = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&noticed)
            .iter()
            .any(|r| r.contains("selection copied")),
        "the walk's re-copy confirms on the notice row: {:?}",
        row_texts(&noticed)
    );
}

#[test]
fn a_drag_running_past_the_bottom_edge_scrolls_with_the_selection() {
    // 25 rows put the conversation pane on rows 0..=14.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
            steered: false,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 25);

    for _ in 0..5 {
        app.handle_event(&wheel_event(MouseEventKind::ScrollUp));
    }
    let _ = render_to_buffer(&mut app, 80, 25);
    assert_eq!(app.scroll_offset(), 5);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        3,
    ));
    assert!(
        app.handle_event(&mouse_event(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            5,
            15,
        )),
        "the edge drag reports a redraw"
    );
    assert_eq!(
        app.scroll_offset(),
        4,
        "sitting on the pane's bottom line steps the view down one line"
    );
    let scrolled = render_to_buffer(&mut app, 80, 25);
    assert!(
        reversed_cells_on_row(&scrolled, 15) >= 1,
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
            steered: false,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 25);
    for _ in 0..5 {
        app.handle_event(&wheel_event(MouseEventKind::ScrollUp));
    }
    let _ = render_to_buffer(&mut app, 80, 25);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        5,
        15,
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
        let _ = render_to_buffer(&mut app, 80, 25);
    }
    assert_eq!(
        app.scroll_offset(),
        0,
        "the parked push walks the view back to the bottom"
    );
    assert!(app.auto_scroll(), "reaching the bottom re-arms stickiness");
    let settled = render_to_buffer(&mut app, 80, 25);
    assert!(
        reversed_cells_on_row(&settled, 14) >= 1,
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
            steered: false,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        5,
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
            steered: false,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 25);

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        4,
    ));
    assert!(
        app.handle_event(&mouse_event(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            5,
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
    let scrolled = render_to_buffer(&mut app, 80, 25);
    // The selection runs from the pane's top edge down to its anchor:
    // the wording rows the reverse paint reaches, and the anchor's
    // moved down one line with the view's step.
    assert!(
        reversed_cells_on_row(&scrolled, 3) >= 1,
        "the selection reaches from the pane's top edge to its anchor as \\
         the view moves"
    );
    // the push parks: ticks carry it upward, bounded by the
    // document's first line — 30 three-line turns over 15 content
    // rows end at offset 75
    let mut ticks = 0;
    while app.tick_wake(std::time::Instant::now()) {
        ticks += 1;
        assert!(
            ticks < 100,
            "the parked push reaches the document top in bounded ticks"
        );
        let _ = render_to_buffer(&mut app, 80, 25);
    }
    assert_eq!(app.scroll_offset(), 75, "the walk ends pinned at the top");
    let settled = render_to_buffer(&mut app, 80, 25);
    assert!(
        reversed_cells_on_row(&settled, 2) >= 1,
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
        steered: false,
    });
    app.push_message(TuiMessage::User {
        text: "klmnopqrst".to_string(),
        timestamp: now,
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
    let second = rows
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
        u16::try_from(second).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        u16::try_from(col + 5).unwrap_or(0),
        u16::try_from(second).unwrap_or(0),
    ));

    // right extends the head's line; three ups pull the head back
    // onto the first line — copying the padding rows it crosses as
    // the blank content lines they are — shrinking the selection
    // onto it; left shrinks within the line
    app.handle_event(&key(KeyCode::Right, KeyModifiers::SHIFT));
    app.handle_event(&key(KeyCode::Up, KeyModifiers::SHIFT));
    app.handle_event(&key(KeyCode::Up, KeyModifiers::SHIFT));
    app.handle_event(&key(KeyCode::Up, KeyModifiers::SHIFT));
    app.handle_event(&key(KeyCode::Left, KeyModifiers::SHIFT));

    let copied = copied.lock().expect("sink").clone();
    assert_eq!(
        copied,
        vec![
            "cdefghij\n\n\nklmnop".to_string(),
            "cdefghij\n\n\nklmnopq".to_string(),
            "cdefghij\n\n".to_string(),
            "cdefghij\n".to_string(),
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
            steered: false,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 25);
    for _ in 0..5 {
        app.handle_event(&wheel_event(MouseEventKind::ScrollUp));
    }
    let _ = render_to_buffer(&mut app, 80, 25);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        3,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        5,
        4,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
        4,
    ));
    assert_eq!(app.scroll_offset(), 5);

    for _ in 0..20 {
        app.handle_event(&key(KeyCode::Down, KeyModifiers::SHIFT));
        let _ = render_to_buffer(&mut app, 80, 25);
    }
    assert_eq!(
        app.scroll_offset(),
        0,
        "the view followed the head all the way down"
    );
    assert!(app.auto_scroll(), "the walk re-arms at the bottom");
    let settled = render_to_buffer(&mut app, 80, 25);
    assert!(
        reversed_cells_on_row(&settled, 14) >= 1,
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
            steered: false,
        });
    }
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));
    let probe = render_to_buffer(&mut app, 80, 25);
    let rows = row_texts(&probe);
    let text_row =
        u16::try_from(rows.iter().rposition(|r| r.contains("line2")).unwrap_or(0)).unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        text_row,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        5,
        text_row + 1,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
        text_row + 1,
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
            steered: false,
        });
    }
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
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
    let col = cell_col(&rows[row], "abcdefghij");
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
        steered: false,
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
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
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
        copied[0], "defghij\n",
        "the press clamps onto the turn's trailing pad, anchoring where a copy can walk"
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
        5,
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
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
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
    let col = cell_col(&rows[row], "streaming reply");
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
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
    let col = cell_col(&rows[row], "abcdefghij");
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows.iter().position(|r| r.contains('x')).unwrap_or(0);
    let col = cell_col(&rows[row], "e");

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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[row], "abcdefghij");
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let row = rows.iter().position(|r| r.contains("日")).unwrap_or(0);
    let col = cell_col(&rows[row], "日");
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
    let (mut app, kept) = app_with_state();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    for c in "hi".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    let handed_over = run_id(rx.try_recv().unwrap());
    assert_eq!(
        kept.queued.lock().unwrap().len(),
        1,
        "the send deepens the queue the driver draws prompts from"
    );
    assert_eq!(
        waiting(&kept),
        vec![(handed_over, "hi".to_string())],
        "and the entry it added is the one the handed-over id names"
    );
}

#[test]
fn the_input_field_grows_with_its_buffer_up_to_the_cap() {
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);

    // empty composer: one text row — the pane is four rows (a
    // padding row, the text row, another padding row, and the
    // identity row on the box's bottom edge), rows 19..22, and the
    // field is blank on 20.
    let empty = render_to_buffer(&mut app, 40, 26);
    let rows = row_texts(&empty);
    assert!(
        !rows[20].contains("enter"),
        "the empty field shows its single blank row: {}",
        rows[20]
    );

    // two lines: the pane grows to five rows and the conversation
    // yields one — the composer now starts a row higher (18) with
    // its text on 19 and 20
    app.handle_event(&Event::Paste("one\ntwo".to_string()));
    let two = render_to_buffer(&mut app, 40, 26);
    let rows = row_texts(&two);
    assert!(
        rows[19].contains("one") && rows[20].contains("two"),
        "a two-line buffer shows both lines: {} / {}",
        rows[19],
        rows[20]
    );
    let identity_at = rows
        .iter()
        .position(|row| row.contains("testmodel"))
        .expect("the identity row renders");
    assert!(
        !rows[identity_at].contains("2/2"),
        "a buffer that fits carries no line-position tag: {}",
        rows[identity_at]
    );

    // five lines: the pane caps at three text rows, the window keeps
    // the caret's tail visible, and the tag reports the overflow
    app.handle_event(&Event::Paste("\nfour\nfive\nsix".to_string()));
    let capped = render_to_buffer(&mut app, 40, 26);
    let rows = row_texts(&capped);
    assert!(
        rows[20].contains("six"),
        "the window keeps the caret's line visible: {}",
        rows[20]
    );
    let identity_at = rows
        .iter()
        .position(|row| row.contains("testmodel"))
        .expect("the identity row renders");
    assert!(
        rows[identity_at].contains("5/5") || rows[identity_at].contains("5/6"),
        "a buffer past the cap carries the line-position tag at the identity row's right: {}",
        rows[identity_at]
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
    let mut terminal = render_to_buffer(&mut app, 12, 14);
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
/// `composer_rows` is the pane's height for the buffer the test staged
/// — margin, place row, padding, the text rows, padding, margin,
/// identity row — and the status chunk below it is the footer's own
/// three.
fn composer_chunk(width: u16, height: u16, composer_rows: u16) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(composer_rows),
            Constraint::Length(3),
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
        let composer = composer_chunk(40, height, 5);
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
fn the_status_bar_paints_only_its_own_rows_in_undersized_terminals() {
    // Whatever the height, the bar's paint is one contiguous block
    // anchored to the frame's bottom — the rows it was granted and no
    // more — so a starved composer keeps every row it was given and
    // never cedes its own last row to the bar. The bar is detected by
    // its own paint, foreground and background together: the
    // composer's rows share the bar's surface background on their text
    // cells, so the pair — and the padding cells the composer leaves
    // at a Reset foreground — is what isolates the bar's rows. The
    // block grows with the width's demand — air, the context bar, the
    // numbers, and wrapped status text above them on narrow frames.
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
        let count = u16::try_from(bar_rows.len()).unwrap_or(0);
        let expected: Vec<u16> = (height.saturating_sub(count)..height).collect();
        assert_eq!(
            bar_rows, expected,
            "h={height}: the bar paints one bottom-anchored block, no row above its own"
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
    let terminal = render_to_buffer(&mut app, 40, 14);
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(5, 2)].bg,
        ratatui::style::Color::Reset,
        "the conversation pane leaves its cells to the terminal default — \
         painting them is what shows a seam against the margin"
    );
    let composer = composer_chunk(40, 14, 4);
    assert_eq!(
        buffer[(1, composer.y)].bg,
        app.theme.ui.surface,
        "the composer body beside its primary edge draws on the surface color"
    );
    assert_eq!(
        buffer[(0, 13)].bg,
        app.theme.ui.status_bar_bg,
        "the status bar paints its own row"
    );
}

#[test]
fn the_state_tag_claims_the_identity_row_s_right_edge() {
    // The composer's state — submissions queued behind the driver,
    // or the caret's line over the buffer's total — closes the
    // identity row at its very right. The identity clips short of
    // the tag's columns so the two never paint over each other; the
    // tag — state the user can act on — never clips first. The
    // footer carries none of it: its numbers run unclipped at any
    // width the frame can give them, its right edge holds no state
    // text in any case — not the tag, not the waiting count. The
    // queue counts itself in its own section.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    let mut config = config_with_theme("dracula");
    config.api.model = "a-very-long-model-name".to_string();
    let mut app = TuiApp::from_observer_state(config, kept.clone());
    {
        // Two waiting prompts — the queue's own section counts these.
        let mut queue = kept.queued.lock().unwrap();
        queue.push("one waiting".to_string());
        queue.push("two waiting".to_string());
    }
    app.handle_event(&Event::Paste(
        "a prompt\nthat spans\nseveral\nlines\nnow".to_string(),
    ));

    let terminal = render_to_buffer(&mut app, 20, 14);
    let rows = row_texts(&terminal);
    let identity_at = rows
        .iter()
        .position(|row| row.contains("a-very"))
        .expect("the identity row renders its readable prefix");
    assert!(
        rows[identity_at].contains("5/5") || rows[identity_at].contains("5/6"),
        "the line-position tag closes the identity row: {:?}",
        rows[identity_at]
    );
    assert!(
        !rows.iter().any(|row| row.contains("queued")),
        "the waiting count is the queue section's own, never a tag: {rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("a-very-long-model-name")),
        "a model name this long never renders whole: {rows:?}"
    );
    // The footer's bottom row carries the readout whole — the tag
    // no longer takes columns from it.
    let status = rows.last().expect("the footer's bottom row");
    assert!(
        status.contains("0k/200k (0%)"),
        "the readout runs unclipped, whatever the composer holds: {status}"
    );
    assert!(
        !status.contains("5/5"),
        "the footer's rows carry no state text at their right edge: {status}"
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
            steered: false,
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
    let _ = render_to_buffer(&mut app, 40, 16);
    app.push_message(TuiMessage::User {
        text: "arrived between frames".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    let terminal = render_to_buffer(&mut app, 40, 16);
    assert!(
        view_text(&terminal, 40, 16).contains("arrived between frames"),
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
    let wide = render_to_buffer(&mut app, 60, 16);
    assert!(
        row_texts(&wide)[1].contains("dddddddddd"),
        "at width 60 the message fits the first content row"
    );

    let narrow = render_to_buffer(&mut app, 24, 18);
    let narrow_rows = row_texts(&narrow);
    assert!(
        !narrow_rows[1].contains("dddddddddd")
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
    let (mut app, kept) = app_with_state();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    drop(rx);

    for c in "gone".chars() {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert!(
        waiting(&kept).is_empty(),
        "a send no receiver takes must not stay counted as queued — an entry \
         behind a dead channel would sit in the waiting section inviting edits \
         to a prompt that can never run"
    );
    assert!(
        app.conversation().is_empty(),
        "and it is not recorded as sent: the transcript's user row is the \
         claim echo now, so a prompt no driver ever claimed has no row"
    );
    assert_eq!(
        app.input(),
        "gone",
        "the draft comes back rather than vanishing with a driver that quit"
    );
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("the driver has gone")),
        "the reason is said out loud, in the user\'s words rather than the \
         mechanism\'s: {rows:?}"
    );
}

#[test]
fn multiline_user_and_system_echoes_render_on_separate_rows() {
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "first\nsecond".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
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
    app.set_turn_end_hook(Box::new(move |conversation, machine| {
        snapshots_lock(&sink).push(conversation.to_vec());
        assert!(machine.is_none(), "no turn-end history rides this drain");
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
fn a_selectionless_release_still_toggles_a_tool_row_and_reports_the_redraw() {
    // A release arrives with no selection in play when the app never saw
    // the press — it was claimed by another surface or landed before the
    // loop listened. The release still runs the click handlers, and its
    // verdict must fold every one of them: a toggle that redraws but
    // reports false leaves the block's open state sitting undrawn until
    // some later event happens to force a frame.
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

    let released = app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    assert!(
        released,
        "the toggle's redraw verdict must survive the selectionless release"
    );
    let open = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&open);
    assert!(
        rows.iter().any(|r| r.contains("▾")),
        "the marker flips open: {rows:?}"
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        5,
        u16::try_from(oldest).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        steered: false,
    });
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("THE-TAIL-IS-HERE")),
        "the line's tail is visible past the pane width: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("  ❯  x")),
        "the line's head starts the turn's first row, behind the marker"
    );
    let spanned = rows.iter().filter(|row| row.contains('x')).count();
    assert!(
        spanned >= 2,
        "the line flows across rows instead of clipping: {spanned} rows, {rows:?}"
    );
}

#[test]
fn a_pad_row_and_a_blank_line_breathe_between_user_turns_and_agent_material() {
    // The boundary contract: a user turn's box ends on its own tinted
    // pad row, one untinted blank line follows, and the agent
    // material beside it starts after that — in either order. The
    // agent side carries no tint of its own.
    let mut app = app();
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "the question".to_string(),
        timestamp: now,
        steered: false,
    });
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "the answer".to_string(),
        }],
        timestamp: now,
        duration_ms: None,
    });
    let terminal = render_to_buffer(&mut app, 40, 17);
    let buffer = terminal.backend().buffer();
    let rows = row_texts(&terminal);
    let question = rows
        .iter()
        .position(|r| r.contains("the question"))
        .expect("the question's row");
    let answer = rows
        .iter()
        .position(|r| r.contains("the answer"))
        .expect("the answer's row");
    assert_eq!(
        answer.saturating_sub(question),
        3,
        "the box's pad and one blank line sit between the turn and the reply: {rows:?}"
    );
    assert!(
        rows[question + 1].trim().is_empty() && rows[question + 2].trim().is_empty(),
        "the boundary rows are blank: {rows:?}"
    );
    assert_eq!(
        buffer[(10, u16::try_from(question + 1).unwrap_or(0))].bg,
        app.theme.ui.surface,
        "the first boundary row is the box's own closing pad"
    );
    assert_eq!(
        buffer[(10, u16::try_from(question + 2).unwrap_or(0))].bg,
        ratatui::style::Color::Reset,
        "the second boundary row stays untinted"
    );
    assert_eq!(
        buffer[(10, u16::try_from(answer).unwrap_or(0))].bg,
        ratatui::style::Color::Reset,
        "the reply sits on the regular background"
    );

    app.push_message(TuiMessage::User {
        text: "the follow-up".to_string(),
        timestamp: now,
        steered: false,
    });
    let follow_terminal = render_to_buffer(&mut app, 40, 18);
    let follow_rows = row_texts(&follow_terminal);
    let answer_now = follow_rows
        .iter()
        .position(|r| r.contains("the answer"))
        .expect("the answer's row in the scrolled view");
    let follow_up = follow_rows
        .iter()
        .position(|r| r.contains("the follow-up"))
        .expect("the follow-up's row");
    assert_eq!(
        follow_up.saturating_sub(answer_now),
        3,
        "the blank line and the box's opening pad sit between the reply and the next turn: {follow_rows:?}"
    );
}

#[test]
fn an_agent_reply_s_code_block_sits_on_the_regular_background() {
    // The prompt box owns the surface color; agent material never
    // shares it. Code blocks frame their lines with painted space
    // columns, and those columns fall back to the terminal's own
    // background unless the theme names a panel color — so a reply's
    // code reads as framed text on the regular background, not as a
    // second prompt box.
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "intro\n\n```rust\nfn main() {}\n```\n".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let terminal = render_to_buffer(&mut app, 40, 18);
    let buffer = terminal.backend().buffer();
    let rows = row_texts(&terminal);
    let code_row = rows
        .iter()
        .position(|r| r.contains("fn main()"))
        .expect("the code row");
    let code_row = u16::try_from(code_row).unwrap_or(0);
    for x in 0..buffer.area.width {
        assert_eq!(
            buffer[(x, code_row)].bg,
            ratatui::style::Color::Reset,
            "column {x} of the code row stays on the regular background"
        );
    }
}

#[test]
fn an_agent_sentence_s_inline_code_sits_on_the_regular_background() {
    // The prompt box owns the surface color; agent prose never
    // shares it. Inline code carries its accent in the foreground
    // alone — no chip tint — so a sentence with a code span reads on
    // the terminal's own background like the words around it.
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "run `make test` today".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let terminal = render_to_buffer(&mut app, 40, 14);
    let buffer = terminal.backend().buffer();
    let rows = row_texts(&terminal);
    let sentence_row = rows
        .iter()
        .position(|r| r.contains("make test"))
        .expect("the sentence's row");
    let sentence_row = u16::try_from(sentence_row).unwrap_or(0);
    for x in 0..buffer.area.width {
        assert_eq!(
            buffer[(x, sentence_row)].bg,
            ratatui::style::Color::Reset,
            "column {x} of the sentence stays on the regular background"
        );
    }
    assert!(
        (0..buffer.area.width).any(|x| buffer[(x, sentence_row)].symbol() != " "),
        "the sentence carries its text"
    );
}

#[test]
fn the_conversation_opens_one_row_below_the_pane_s_top_edge() {
    // Inner padding at the top: the transcript's first line sits one
    // breathing row below the pane's edge, and a user turn adds its
    // own tinted pad row before its text — the same convention the
    // composer's text follows inside its fill.
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "the first line".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    let terminal = render_to_buffer(&mut app, 40, 16);
    let buffer = terminal.backend().buffer();
    for row in [0u16, 1] {
        let text: String = (0..buffer.area.width)
            .map(|x| buffer[(x, row)].symbol().to_string())
            .collect();
        assert!(
            text.trim().is_empty(),
            "row {row} above the text stays blank: {text:?}"
        );
    }
    assert_eq!(
        buffer[(10, 1)].bg,
        app.theme.ui.surface,
        "the row above the text carries the box's tint"
    );
    assert_eq!(
        buffer[(10, 0)].bg,
        ratatui::style::Color::Reset,
        "the pane's own padding row stays on the terminal background"
    );
    let first: String = (0..buffer.area.width)
        .map(|x| buffer[(x, 2)].symbol().to_string())
        .collect();
    assert!(
        first.contains("the first line"),
        "the transcript starts below the pane edge and the box pad: {first:?}"
    );
}

#[test]
fn wrapped_lines_keep_a_right_margin_before_the_scrollbar_gutter() {
    // Inner padding on the right: a wrapped user row fills the pane's
    // full text width and stops, leaving the pane-level blank columns
    // before the gutter the scrollbar owns — the same margin every
    // message shares, at any terminal width.
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "x".repeat(120),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    let terminal = render_to_buffer(&mut app, 40, 18);
    let buffer = terminal.backend().buffer();
    let wrapped_row = (0..buffer.area.height)
        .find(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].symbol() == "x"))
        .expect("a row carrying the wrapped text");
    assert_eq!(
        buffer[(34, wrapped_row)].symbol(),
        "x",
        "the row fills the text width up to the box's inner padding on row {wrapped_row}"
    );
    for x in 35..40u16 {
        assert_eq!(
            buffer[(x, wrapped_row)].symbol(),
            " ",
            "column {x} stays blank through the box's padding and margin on row {wrapped_row}"
        );
    }
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        5,
        4,
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        steered: false,
    });
    let copied: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        sink.lock().expect("sink").push(text.to_string());
        dch_tui::CopyAnswer::Verified
    }));

    let probe = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&probe);
    let user_row = rows
        .iter()
        .position(|r| r.contains("abcdefghij"))
        .unwrap_or(0);
    let col = cell_col(&rows[user_row], "abcdefghij");
    let tool_row = rows
        .iter()
        .position(|r| r.contains("Running: make"))
        .unwrap_or(0);
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
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
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
        5,
        u16::try_from(output_row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
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
    assert_eq!(
        row, 1,
        "sanity: the short document starts at the top content row"
    );

    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
        u16::try_from(row).unwrap_or(0),
    ));
    let open = render_to_buffer(&mut app, 80, 24);
    let open_rows = row_texts(&open);
    assert!(
        open_rows
            .get(1)
            .is_some_and(|r| r.contains("Running: make build")),
        "the clicked row stays at the pane's top content row: {open_rows:?}"
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
        .push(dch_tui::Graduation::Reply {
            text: "here is what I found".to_string(),
            thinking: None,
        });
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
            steered: false,
        });
    }
    // 20 one-line messages over the conversation pane make the
    // document scrollable; the pinned view sits at its bottom.
    let terminal = render_to_buffer(&mut app, 20, 15);
    let buffer = terminal.backend().buffer();
    let thumb = app.theme.ui.scrollbar_thumb;
    let track = app.theme.ui.scrollbar_track;
    // 20 rows: 6 conversation rows — 5 content rows for the
    // conversation below its padding row, above the notice, spacer,
    // composer, and footer. thumb = 5 * 5 / 60 clamps to 1 row,
    // flush with the track's bottom.
    for y in 1..6u16 {
        let cell = &buffer[(19, y)];
        let in_thumb = y >= 5;
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
    let top_row: String = (0..20u16)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect();
    assert!(
        top_row.trim().is_empty(),
        "the pane's padding row stays blank: {top_row:?}"
    );
    assert_eq!(
        buffer[(19, 0)].bg,
        ratatui::style::Color::Reset,
        "the scrollbar track starts below the padding row"
    );
    for y in 1..6u16 {
        for x in 0..19u16 {
            let cell = &buffer[(x, y)];
            assert!(
                cell.symbol() == " " || cell.bg != thumb,
                "wrapped text never takes the scrollbar's thumb fill ({x},{y})"
            );
        }
    }
    assert!(
        (1..6u16).any(|y| (0..19u16).any(|x| buffer[(x, y)].symbol() != " ")),
        "the text area still carries content beside the gutter"
    );
}

#[test]
fn the_gutter_stays_blank_when_the_document_fits() {
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "hello".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    let terminal = render_to_buffer(&mut app, 20, 18);
    let buffer = terminal.backend().buffer();
    for y in 0..7u16 {
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

/// An app with staged question prompts already landed in the queue.
///
/// One prompt per entry, each with its own answer receiver; the
/// options follow the `(label, description)` pairs of the entry.
/// One staged question's shape for [`question_app`].
struct StagedQuestion {
    /// The question text the prompt carries.
    question: &'static str,
    /// The option rows, each a label with an optional description.
    options: &'static [(&'static str, Option<&'static str>)],
    /// Whether the prompt allows several answers.
    multi_select: bool,
}

/// An app with staged question prompts already landed in the queue.
///
/// One prompt per entry, each with its own answer receiver.
fn question_app(
    questions: &[StagedQuestion],
) -> (TuiApp, Vec<tokio::sync::oneshot::Receiver<Vec<String>>>) {
    let mut app = app();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut receivers = Vec::new();
    for staged in questions {
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(dch_tui::QuestionPrompt {
            question: staged.question.to_string(),
            header: Some("Ask".to_string()),
            options: staged
                .options
                .iter()
                .map(|(label, description)| dch_tui::QuestionChoice {
                    label: (*label).to_string(),
                    description: description.map(std::string::ToString::to_string),
                })
                .collect(),
            multi_select: staged.multi_select,
            reply,
        })
        .expect("receiver alive");
        receivers.push(reply_rx);
    }
    app.set_question_requests(rx);
    app.poll_question_requests();
    (app, receivers)
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

/// The column at which `needle` starts on screen.
///
/// Counts cells rather than bytes — the box's own borders are
/// multi-byte — so two rows asked about here are comparable.
/// Panics when `needle` is nowhere on screen.
fn column_of(terminal: &Terminal<TestBackend>, needle: &str) -> usize {
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        if let Some(found) = row.find(needle) {
            return row[..found].chars().count();
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
        steered: false,
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
    let status_row = row_of(&terminal, "0k/200k");
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
fn the_question_sheet_reserves_its_rows_above_the_conversation() {
    // The ask box grows with its options, and a tall one used to
    // paint over the conversation's bottom rows — the run's last
    // words hidden behind the very question asking about them. The
    // frame now holds the sheet's rows aside: the conversation ends
    // above the box, its tail visible, whatever the options' count.
    let question = StagedQuestion {
        question: "Deploy the release now?",
        options: &[
            ("yes", Some("ships immediately")),
            ("no", None),
            ("later", Some("asks again tomorrow")),
            ("staging first", Some("promote after smoke")),
            ("force", Some("skip the checks")),
            ("abort", None),
        ],
        multi_select: false,
    };
    let (mut app, _replies) = question_app(&[question]);
    for step in 0..30 {
        let _ = step;
        app.push_message(dch_tui::TuiMessage::System {
            text: format!("conversation row {step}"),
            timestamp: chrono::Utc::now(),
        });
    }
    app.push_message(dch_tui::TuiMessage::System {
        text: "TAIL MARKER the last word".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let terminal = render_to_buffer(&mut app, 80, 25);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("TAIL MARKER"),
        "the conversation's tail stays visible above the ask: {screen}"
    );
    assert!(
        screen.contains("Type your answer"),
        "the compose row rides after the options: {screen}"
    );
}

#[test]
fn the_compose_row_sends_a_typed_answer_the_options_never_offered() {
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    // Walk to the compose row (after the last option), open it, type,
    // send: the reply is the typed text, exactly as typed.
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    for ch in "ship friday".chars() {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let _ = app.handle_event(&plain(KeyCode::Enter));
    let answer = replies.remove(0).try_recv().unwrap();
    assert_eq!(
        answer,
        vec!["ship friday".to_string()],
        "the typed text is the answer"
    );
    // The question resolved: the next key reaches no overlay — the
    // composer answers instead, the one surface the overlay had
    // swallowed the keyboard from.
    let _ = app.handle_event(&plain(KeyCode::Char('z')));
    assert_eq!(app.input(), "z", "the keyboard returned to the composer");
}

#[test]
fn escape_from_compose_steps_back_to_the_options_with_the_draft_kept() {
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    for ch in "maybe".chars() {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    // Esc leaves the draft, not the question; Up returns to a real
    // option and Enter answers with its label.
    let _ = app.handle_event(&plain(KeyCode::Esc));
    let _ = app.handle_event(&plain(KeyCode::Up));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    let answer = replies.remove(0).try_recv();
    assert_eq!(
        answer.unwrap(),
        vec!["no".to_string()],
        "Esc stepped back, the option row answered, the question lives no longer"
    );
}

#[test]
fn a_pending_question_keeps_the_status_bar() {
    // The rows the sheet reserves sit between the conversation and the
    // bottom furniture, so the notice, strip, composer, and status bar
    // each claim one chunk further down while an ask is up. Miss that
    // step and the status bar paints over the composer's rows — which
    // the sheet clears anyway — while its own rows draw nothing at all.
    let (mut app, _replies) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("0k/200k"),
        "the status bar's readout survives a pending ask: {screen}"
    );
}

#[test]
fn space_marks_a_row_on_every_ask_and_never_answers() {
    // Space used to do nothing at all on a question that takes one
    // answer, and the hint line kept quiet about it. It marks a row on
    // every ask now: the single mark moves on a single-select, rows add
    // and drop on a multi-select. What it never does is answer — a
    // stray press cannot send a reply, and Enter stays the one press
    // that sends.
    let (mut single, mut receivers) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    let screen = screen_text(&render_to_buffer(&mut single, 80, 24));
    assert!(
        screen.contains("[space] pick"),
        "a single-select ask names what Space does: {screen}"
    );

    let _ = single.handle_event(&plain(KeyCode::Char(' ')));
    let _ = single.handle_event(&plain(KeyCode::Down));
    let marked = screen_text(&render_to_buffer(&mut single, 80, 24));
    assert!(
        marked.contains("\u{2022} yes"),
        "Space marks the row it stands on, and the mark stays as the cursor moves on: {marked}"
    );
    assert!(
        receivers[0].try_recv().is_err(),
        "marking a row answers nothing"
    );

    let reply = receivers.remove(0);
    let _ = single.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        reply.blocking_recv(),
        Ok(vec!["yes".to_string()]),
        "Enter sends the marked row, not the row the cursor ended on"
    );

    let (mut multi, _multi_rx) = question_app(&[StagedQuestion {
        question: "Which environments?",
        options: &[("staging", None), ("prod", None)],
        multi_select: true,
    }]);
    let screen = screen_text(&render_to_buffer(&mut multi, 80, 24));
    assert!(
        screen.contains("[space] toggle"),
        "a multi-select ask advertises the control it honors: {screen}"
    );
    let _ = multi.handle_event(&plain(KeyCode::Char(' ')));
    let marked = screen_text(&render_to_buffer(&mut multi, 80, 24));
    assert!(
        marked.contains('\u{2022}'),
        "space marks the cursor row it stands on: {marked}"
    );
}

#[test]
fn space_marks_the_compose_row_and_enter_is_what_writes_it() {
    // One key, one meaning everywhere: Space puts the dot on the compose
    // row exactly as it puts it on an option, and does not slip into the
    // draft on the way. Enter is the press that opens the draft — and a
    // marked row with nothing written yet has no reply to send, so Enter
    // goes and writes one instead of answering with half of one.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "How should this ship?",
        options: &[("now", None), ("later", None)],
        multi_select: false,
    }]);
    render_to_buffer(&mut app, 80, 30);
    for code in [KeyCode::Down, KeyCode::Down] {
        app.handle_event(&plain(code)); // past "later", onto the compose row
    }
    app.handle_event(&plain(KeyCode::Char(' ')));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("\u{2022} \u{270e}"),
        "Space marks the compose row with the same dot an option wears: {screen}"
    );
    assert!(
        screen.contains("[space] pick") && !screen.contains("[\u{2190}\u{2192}] caret"),
        "marking the row is not writing it, so the draft stays shut: {screen}"
    );
    assert!(
        screen.contains("How should this ship?") && screen.contains("Question"),
        "Space marks without answering: the ask is still on screen: {screen}"
    );

    app.handle_event(&plain(KeyCode::Enter));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("[\u{2190}\u{2192}] caret"),
        "Enter on the marked row opens the draft to be written: {screen}"
    );
    for ch in ['t', 'o', 'm', 'o', 'r', 'r', 'o', 'w'] {
        app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let answer = replies.remove(0);
    app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        answer.blocking_recv(),
        Ok(vec!["tomorrow".to_string()]),
        "a marked draft answers on its own against a single-select"
    );
}

#[test]
fn a_marked_draft_answers_beside_the_options_marked_with_it() {
    // The draft used to answer alone: whatever a multi-select had marked
    // beside it, the Enter taken inside the caret sent the typed text and
    // dropped the marks. One answer builder serves both Enters now, so
    // writing a reply is not a way of forgetting the rows marked for it.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "What runs before the push?",
        options: &[
            ("lint", Some("the rust lints")),
            ("tests", Some("the whole suite")),
        ],
        multi_select: true,
    }]);
    render_to_buffer(&mut app, 80, 30);
    app.handle_event(&plain(KeyCode::Char(' '))); // mark "lint"
    for code in [KeyCode::Down, KeyCode::Down] {
        app.handle_event(&plain(code)); // to the compose row
    }
    app.handle_event(&plain(KeyCode::Char(' '))); // and mark it too
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("\u{2022} \u{270e}"),
        "the draft's row carries a mark of its own: {screen}"
    );

    // Typing opens the draft the mark says the reply belongs in. Enter
    // would answer with what is marked, and an empty compose row has
    // nothing to add to the options marked beside it.
    for ch in ['t', 'w', 'i', 'c', 'e'] {
        app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let answer = replies.remove(0);
    app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        answer.blocking_recv(),
        Ok(vec!["lint".to_string(), "twice".to_string()]),
        "the marked options answer in the sheet's order, the draft last"
    );
}
#[test]
fn typing_at_the_rows_opens_the_draft_with_that_letter() {
    // A letter typed with the sheet up is a reply being started. It used
    // to be swallowed outright, so the words simply failed to appear and
    // the only way in was to know about Enter first.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Say something?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    render_to_buffer(&mut app, 80, 30);
    for ch in ['h', 'e', 'l', 'l', 'o'] {
        app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("\u{270e} hello"),
        "the letters land in the draft, not the void: {screen}"
    );
    assert!(
        screen.contains("[\u{2190}\u{2192}] caret"),
        "and the sheet says the draft has the keys: {screen}"
    );
    assert_eq!(app.input(), "", "the composer never saw a keystroke");

    let answer = replies.remove(0);
    app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        answer.blocking_recv(),
        Ok(vec!["hello".to_string()]),
        "what was typed is what answers"
    );
}

#[test]
fn a_written_reply_shadows_the_marked_option_and_gives_it_back() {
    // The trade the user is making has to be on the screen while they
    // make it. Writing against a question that wants one answer shadows
    // the marked row's dot — the text is what Enter inside the draft
    // would send — and stepping out with Esc hands the dot back, because
    // the mark was never taken, only put aside while other words were
    // being chosen.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    render_to_buffer(&mut app, 80, 30);
    app.handle_event(&plain(KeyCode::Char(' '))); // mark "yes"
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("\u{2022} yes"),
        "Space marks the row: {screen}"
    );

    app.handle_event(&plain(KeyCode::Char('x'))); // start a reply instead
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        !screen.contains("\u{2022} yes"),
        "while the reply is being written, its text has the ask: {screen}"
    );

    app.handle_event(&plain(KeyCode::Esc)); // step back out of the draft
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("\u{2022} yes"),
        "and the mark is where it was left: {screen}"
    );

    let answer = replies.remove(0);
    app.handle_event(&plain(KeyCode::Enter)); // Enter over the rows
    assert_eq!(
        answer.blocking_recv(),
        Ok(vec!["yes".to_string()]),
        "stepping back out answers with the marked row, not the abandoned words"
    );
}

#[test]
fn the_draft_answers_over_the_marked_option_when_entered_inside_it() {
    // The other half of the same trade: Enter taken *inside* the draft
    // sends the text, and the shadowed mark does not ride along to make a
    // single-select answer two things at once.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    render_to_buffer(&mut app, 80, 30);
    app.handle_event(&plain(KeyCode::Char(' '))); // mark "yes"
    for ch in ['t', 'o', 'm', 'o', 'r', 'r', 'o', 'w'] {
        app.handle_event(&plain(KeyCode::Char(ch))); // and write instead
    }
    let answer = replies.remove(0);
    app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        answer.blocking_recv(),
        Ok(vec!["tomorrow".to_string()]),
        "the drafted reply answers alone against a single-select"
    );
}
#[test]
fn a_walked_question_keeps_its_answer_in_progress() {
    // Walking a batch with the arrows used to start every question
    // fresh, so a form filled out of order lost each unfinished answer
    // on the way past. Each question carries its own cursor, marks, and
    // draft now: the one walked away from keeps what was left there for
    // the return visit, and the answer sent is only ever its own.
    let (mut app, mut receivers) = question_app(&[
        StagedQuestion {
            question: "Which gates block the merge?",
            options: &[("lint", None), ("tests", None)],
            multi_select: true,
        },
        StagedQuestion {
            question: "Which deploys follow?",
            options: &[("staging", None), ("prod", None)],
            multi_select: true,
        },
    ]);

    // Mark the first question's second option, then walk to the second.
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Char(' ')));
    let _ = app.handle_event(&plain(KeyCode::Right));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("Which deploys follow?"),
        "Right walks to the next question: {screen}"
    );
    assert!(
        !screen.contains("\u{2022}"),
        "the second question arrives with nothing marked: {screen}"
    );

    // Mark its first option, then walk back.
    let _ = app.handle_event(&plain(KeyCode::Char(' ')));
    let _ = app.handle_event(&plain(KeyCode::Left));
    let back = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        back.contains("Which gates block the merge?"),
        "Left walks back to the question it came from: {back}"
    );
    assert!(
        back.contains("\u{2022} tests"),
        "the mark left there is still there: {back}"
    );

    let first = receivers.remove(0);
    let _ = app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        first.blocking_recv(),
        Ok(vec!["tests".to_string()]),
        "the answered question sends its own marks, not the other one's"
    );
    assert!(
        receivers[0].try_recv().is_err(),
        "the question left mid-answer still waits"
    );
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("\u{2022} staging"),
        "and it is still standing where it was left: {screen}"
    );
}

#[test]
fn a_capital_letter_types_into_the_open_draft() {
    // A capital reaches a terminal as the letter with Shift held, which is
    // how the composer's own editor receives it too. The draft dropping
    // that shape lost the capital out of the middle of a word — "Deploy"
    // arrived as "eploy" — and nothing on the screen said so.
    let (mut app, _replies) = question_app(&[StagedQuestion {
        question: "which migration runs first?",
        options: &[("none", None)],
        multi_select: false,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Enter)); // open the draft
    let _ = app.handle_event(&key(KeyCode::Char('D'), KeyModifiers::SHIFT));
    for ch in ['b', 'c'] {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("Dbc"),
        "the capital lands where it was typed: {screen}"
    );
}

#[test]
fn a_capital_letter_starts_a_draft_from_the_rows() {
    // Typing at the rows starts the answer; a shifted letter is the same
    // invitation. The word has to arrive whole, because the answer is
    // what was written and not the letters that happened to be unshifted.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Deploy the release now?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    let _ = app.handle_event(&key(KeyCode::Char('Y'), KeyModifiers::SHIFT));
    for ch in ['e', 's'] {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("Yes"),
        "a shifted letter opens the draft with its capital in it: {screen}"
    );

    let mut reply = replies.remove(0);
    let _ = app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        reply.try_recv().expect("the drafted answer goes at once"),
        vec!["Yes".to_string()],
        "the capital is in the answer, not dropped on the way to it"
    );
}

#[test]
fn the_draft_edits_at_its_caret() {
    // The drafted answer was append-only: a word in the wrong place
    // meant deleting back to it. The arrows move a caret inside the
    // draft, Home and End jump its ends, and the terminal cursor shows
    // where the next character lands.
    let (mut app, _rx) = question_app(&[StagedQuestion {
        question: "Anything to add?",
        options: &[("yes", None)],
        multi_select: false,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    for ch in ['c', 'd'] {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let _ = app.handle_event(&plain(KeyCode::Home));
    for ch in ['a', 'b'] {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let mut terminal = render_to_buffer(&mut app, 80, 24);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("abcd"),
        "typing lands at the caret: {screen}"
    );
    assert_eq!(
        terminal.get_cursor_position().unwrap().x,
        7,
        "the caret sits after the two characters typed at its start"
    );

    // Back from the end, one character back, delete: the character left
    // of the caret goes, not the row's tail.
    let _ = app.handle_event(&plain(KeyCode::End));
    let _ = app.handle_event(&plain(KeyCode::Home));
    let _ = app.handle_event(&plain(KeyCode::Right));
    let _ = app.handle_event(&plain(KeyCode::Right));
    let _ = app.handle_event(&plain(KeyCode::Backspace));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("acd") && !screen.contains("abcd"),
        "Backspace eats the character before the caret: {screen}"
    );

    // At the start there is nothing before the caret to eat.
    let _ = app.handle_event(&plain(KeyCode::Home));
    let _ = app.handle_event(&plain(KeyCode::Backspace));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("acd"),
        "a caret at the draft's start deletes nothing: {screen}"
    );
}

#[test]
fn the_arrows_belong_to_the_draft_while_it_is_open() {
    // The same two keys mean two things, split by whether the draft is
    // being typed: between the rows they walk the queue, inside the
    // draft they move its caret. A walk mid-sentence would send the
    // text looking like nothing at all.
    let (mut app, mut receivers) = question_app(&[
        StagedQuestion {
            question: "First question?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Second question?",
            options: &[("yes", None)],
            multi_select: false,
        },
    ]);

    // Between the rows: the arrows walk.
    let _ = app.handle_event(&plain(KeyCode::Right));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("Second question?"),
        "with no draft open, Right walks the queue: {screen}"
    );

    // Inside the draft: the same keys move the caret and leave the
    // queue where it stands.
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    for ch in ['h', 'i'] {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let _ = app.handle_event(&plain(KeyCode::Home));
    let mut before = render_to_buffer(&mut app, 80, 24);
    let _ = app.handle_event(&plain(KeyCode::Right));
    let mut after = render_to_buffer(&mut app, 80, 24);
    assert!(
        after.get_cursor_position().unwrap().x > before.get_cursor_position().unwrap().x,
        "Right moves the caret right inside the draft"
    );
    let screen = screen_text(&after);
    assert!(
        screen.contains("Second question?"),
        "and does not walk away from the question being answered: {screen}"
    );

    let second = receivers.remove(1);
    let _ = app.handle_event(&plain(KeyCode::End));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    assert_eq!(
        second.blocking_recv(),
        Ok(vec!["hi".to_string()]),
        "the draft answers the question it was typed for"
    );
}

#[test]
fn the_compose_row_hangs_at_the_options_indent() {
    // Option descriptions and the compose row's subtext hang at one
    // column. The indent used to ride inside the text handed to
    // wrap_prompt_rows, which splits on spaces and drops a leading run
    // of them — the descriptions landed flush-left while the compose
    // subtext kept its spaces, and the extra row read as shifted right.
    let (mut app, _replies) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", Some("ships immediately")), ("no", None)],
        multi_select: false,
    }]);
    let terminal = render_to_buffer(&mut app, 80, 24);
    let label = column_of(&terminal, "yes");
    let description = column_of(&terminal, "ships immediately");
    let subtext = column_of(&terminal, "a reply the options");
    assert_eq!(
        description, label,
        "a description hangs under its own option's text, not beside it"
    );
    assert_eq!(
        subtext, label,
        "the compose row's subtext hangs where the descriptions do"
    );
}

#[test]
fn a_draft_kept_out_of_compose_shows_on_its_row() {
    // Esc steps out of the draft and back to the rows, and the words
    // stay — but staying in the state while the row redrew its empty
    // invitation meant a half-typed answer vanished from sight, with
    // nothing to show it was still there to send.
    let (mut app, _replies) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Down));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    for ch in "friday night".chars() {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let _ = app.handle_event(&plain(KeyCode::Esc));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("friday night"),
        "the draft left behind shows on its row: {screen}"
    );
}

#[test]
fn the_arrow_keys_walk_the_queued_questions() {
    // A batch arrives as a queue, and only the head was reachable: Left
    // and Right landed on the overlay's swallow-all arm, so a second
    // question was invisible and unreachable until the first was
    // dismissed. They now rotate the queue, so whichever question is on
    // screen is the one Enter answers and Esc dismisses.
    let (mut app, mut replies) = question_app(&[
        StagedQuestion {
            question: "Ship the release tonight?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Run the migration too?",
            options: &[("yes", None)],
            multi_select: false,
        },
    ]);

    let _ = app.handle_event(&plain(KeyCode::Right));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("Run the migration too?"),
        "Right walks to the next question: {screen}"
    );

    let _ = app.handle_event(&plain(KeyCode::Left));
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("Ship the release tonight?"),
        "Left walks back to the one it came from: {screen}"
    );

    let _ = app.handle_event(&plain(KeyCode::Right));
    let _ = app.handle_event(&plain(KeyCode::Enter));
    let answered = replies[1]
        .try_recv()
        .expect("the question on screen took the answer");
    assert_eq!(
        answered,
        vec!["yes".to_string()],
        "the visible question answers"
    );
    assert!(
        replies[0].try_recv().is_err(),
        "the question walked past still waits, unanswered"
    );
}

#[test]
fn the_sheet_titles_which_question_is_up() {
    // Being able to walk a queue is useless without knowing where you
    // stand in it: the count rides the title, and the hint line names
    // the keys that move between questions.
    let (mut single, _single_rx) = question_app(&[StagedQuestion {
        question: "Ship it?",
        options: &[("yes", None)],
        multi_select: false,
    }]);
    let screen = screen_text(&render_to_buffer(&mut single, 80, 30));
    assert!(
        !screen.contains("of "),
        "a lone question counts nothing: {screen}"
    );
    assert!(
        !screen.contains("[\u{2190}\u{2192}]"),
        "nothing to walk between, nothing advertised: {screen}"
    );

    let (mut queued, _queued_rx) = question_app(&[
        StagedQuestion {
            question: "Ship it?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Migrate too?",
            options: &[("yes", None)],
            multi_select: false,
        },
    ]);
    let screen = screen_text(&render_to_buffer(&mut queued, 80, 30));
    assert!(
        screen.contains("Question 1 of 2"),
        "the title says which question this is: {screen}"
    );
    assert!(
        screen.contains("[\u{2190}\u{2192}] question"),
        "the hint line names the keys that walk the queue: {screen}"
    );
}

#[test]
fn the_title_counts_the_question_the_walk_arrived_at() {
    // The count tracks the walk, not the queue's shape.
    //
    // Selecting a question by rotating the queue would leave every one of
    // them reading `1 of N`: true of the batch, useless about the sheet on
    // screen. Walking is what a user does with these keys, so the count has
    // to move with the walk — and say something true again after a wrap and
    // after an answer shrinks the batch.
    let (mut app, _rx) = question_app(&[
        StagedQuestion {
            question: "first?",
            options: &[("ok", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "second?",
            options: &[("ok", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "third?",
            options: &[("ok", None)],
            multi_select: false,
        },
    ]);
    let screen = |app: &mut TuiApp| screen_text(&render_to_buffer(app, 80, 24));

    assert!(
        screen(&mut app).contains("Question 1 of 3"),
        "the batch opens on its first question"
    );

    app.handle_event(&plain(KeyCode::Right));
    let walked = screen(&mut app);
    assert!(
        walked.contains("Question 2 of 3") && walked.contains("second?"),
        "the walk carries its number with it: {walked}"
    );

    app.handle_event(&plain(KeyCode::Right));
    let walked = screen(&mut app);
    assert!(
        walked.contains("Question 3 of 3") && walked.contains("third?"),
        "the batch's last question counts itself last: {walked}"
    );

    // Wrapping is the same walk past the end: the first question comes
    // round with its own number, not a fresh `1` for whatever is up.
    app.handle_event(&plain(KeyCode::Right));
    let wrapped = screen(&mut app);
    assert!(
        wrapped.contains("Question 1 of 3") && wrapped.contains("first?"),
        "wrapping restarts at the batch's own first: {wrapped}"
    );
    app.handle_event(&plain(KeyCode::Left));
    assert!(
        screen(&mut app).contains("Question 3 of 3"),
        "and steps back over the same edge"
    );

    // Answering shrinks the batch the count is measuring.
    app.handle_event(&plain(KeyCode::Enter));
    let answered = screen(&mut app);
    assert!(
        answered.contains("Question 2 of 2") && answered.contains("second?"),
        "the answered question leaves the count with it: {answered}"
    );
}

#[test]
fn a_batch_lands_through_the_run_loop_as_one_walkable_queue() {
    // A batch arrives through the run loop's own landing path, not the
    // test seam.
    //
    // Production hands `run` one prompt with the rest of the batch still
    // in the channel, and [`land_question_requests`](TuiApp::land_question_requests)
    // drains what sits beside it. Staging prompts one at a time — what
    // [`question_app`] does — can't tell a queue that fills from a batch
    // apart from a queue that only ever holds the question on screen, and
    // that blind spot is precisely how an asking bridge handing its batch
    // over a question at a time kept every question of a batch but the
    // first off the terminal, the walk and the count along with it.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut waiting = Vec::new();
    for question in ["first?", "second?", "third?"] {
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(dch_tui::QuestionPrompt {
            question: question.to_string(),
            header: None,
            options: vec![dch_tui::QuestionChoice {
                label: "ok".to_string(),
                description: None,
            }],
            multi_select: false,
            reply,
        })
        .expect("receiver alive");
        waiting.push(reply_rx);
    }
    let first = rx.try_recv().expect("the batch's first prompt");
    let mut app = app();
    app.land_question_requests(first, &mut rx);

    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("Question 1 of 3"),
        "the sheet counts the batch that landed together: {screen}"
    );

    // The count is not decoration: two presses reach the batch's end,
    // and every question still waits on its own answer.
    for _ in 0..2 {
        app.handle_event(&plain(KeyCode::Right));
    }
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("third?"),
        "the batch's last question is two presses from its first: {screen}"
    );
    assert!(
        waiting.iter_mut().all(|answer| answer.try_recv().is_err()),
        "walking the queue answers nothing"
    );

    app.handle_event(&plain(KeyCode::Enter));
    let answered = waiting
        .pop()
        .expect("the batch's last question")
        .try_recv()
        .expect("the question on screen is the one that answers");
    assert_eq!(answered, vec!["ok".to_string()]);
}

#[test]
fn queued_questions_keep_the_conversation_clear() {
    // The queue segment lengthens the hints line, and at a narrow width
    // that line wraps into a row the frame never reserved — the box grew
    // upward past the rows held aside and covered the very transcript it
    // reserves them to uncover. The reservation counts the same `more`
    // the sheet renders, so the two cannot disagree.
    let (mut app, _replies) = question_app(&[
        StagedQuestion {
            question: "Ship the release to production tonight?",
            options: &[("yes", None), ("no", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "And the migration?",
            options: &[("yes", None), ("no", None)],
            multi_select: false,
        },
    ]);
    for step in 0..30 {
        app.push_message(dch_tui::TuiMessage::System {
            text: format!("conversation row {step}"),
            timestamp: chrono::Utc::now(),
        });
    }
    app.push_message(dch_tui::TuiMessage::System {
        text: "TAIL MARKER".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let terminal = render_to_buffer(&mut app, 60, 24);
    let tail = row_of(&terminal, "TAIL MARKER");
    let sheet = row_of(&terminal, "Question");
    assert!(
        tail < sheet,
        "the queued hints row costs a reserved row: tail at {tail}, sheet tops at {sheet}"
    );
}

#[test]
fn a_pending_question_renders_the_overlay() {
    let (mut app, _replies) = question_app(&[StagedQuestion {
        question: "Deploy the release now?",
        options: &[
            ("yes", Some("ships immediately")),
            ("no", None),
            ("later", Some("asks again tomorrow")),
        ],
        multi_select: false,
    }]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(screen.contains("Question"), "the title renders: {screen}");
    assert!(
        screen.contains("[Ask]"),
        "the header chip renders: {screen}"
    );
    assert!(
        screen.contains("Deploy the release now?"),
        "the question text renders: {screen}"
    );
    assert!(screen.contains("yes"), "the first option renders: {screen}");
    assert!(
        screen.contains("later"),
        "the third option renders: {screen}"
    );
    assert!(
        screen.contains("asks again tomorrow"),
        "an option's description renders: {screen}"
    );
    assert!(
        screen.contains("[enter] confirm"),
        "the key hints render: {screen}"
    );
}

#[test]
fn arrows_space_and_enter_answer_a_multi_select_question() {
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Which checks run?",
        options: &[("lint", None), ("tests", None), ("fmt", None)],
        multi_select: true,
    }]);
    assert!(app.handle_event(&plain(KeyCode::Char(' '))));
    assert!(app.handle_event(&plain(KeyCode::Down)));
    assert!(app.handle_event(&plain(KeyCode::Char(' '))));
    assert!(app.handle_event(&plain(KeyCode::Down)));
    assert!(app.handle_event(&plain(KeyCode::Char(' '))));
    let reply = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        reply.blocking_recv(),
        Ok(vec![
            "lint".to_string(),
            "tests".to_string(),
            "fmt".to_string()
        ]),
        "Enter confirms the toggled set in option order"
    );
}

#[test]
fn toggling_a_row_twice_returns_it_to_untoggled() {
    // Space's contract is the mark and nothing else: one press puts the
    // dot on the row, the next takes it off. The answer Enter makes
    // afterwards is its own subject (pinned beside it), so this test
    // reads the row rather than the reply.
    let (mut app, _replies) = question_app(&[StagedQuestion {
        question: "Which checks run?",
        options: &[("lint", None), ("tests", None)],
        multi_select: true,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Char(' ')));
    let marked = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        marked.contains("\u{2022} lint"),
        "the first Space marks the row: {marked}"
    );

    let _ = app.handle_event(&plain(KeyCode::Char(' ')));
    let cleared = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        !cleared.contains("\u{2022} lint"),
        "the second Space takes the mark off again: {cleared}"
    );
}

#[test]
fn enter_confirms_the_cursor_row_for_a_single_select_question() {
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Deploy now?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    assert!(app.handle_event(&plain(KeyCode::Down)));
    let reply = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        reply.blocking_recv(),
        Ok(vec!["no".to_string()]),
        "Enter confirms the row the cursor moved to"
    );
}

#[test]
fn enter_confirms_the_cursor_row_for_a_multi_select_question() {
    // Enter over the rows is a confirmation on an ask that takes several
    // answers exactly as it is on one that takes one. An empty reply is
    // what a dismissal sends, and the ask tool reads the two alike — so a
    // row the user walked onto and confirmed cannot answer with nothing.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Which checks run?",
        options: &[("lint", None), ("tests", None)],
        multi_select: true,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Down));
    let mut reply = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        reply.try_recv().expect("the confirmed row answers at once"),
        vec!["tests".to_string()],
        "the row the cursor reached answers an unmarked multi-select"
    );
}

#[test]
fn a_kept_draft_stands_aside_when_enter_confirms_an_option() {
    // Stepping out of the draft keeps the words, and Enter on an option
    // row is the later decision — the pointer's, not the text's. Which of
    // the two wins is a decision worth a pin: the row answers, and the
    // draft that was never marked stays unasked rather than answering in
    // the option's place.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Which checks run?",
        options: &[("lint", None), ("tests", None)],
        multi_select: true,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Down)); // to "tests"
    let _ = app.handle_event(&plain(KeyCode::Down)); // to the compose row
    let _ = app.handle_event(&plain(KeyCode::Enter)); // open the draft
    for ch in "just the lints".chars() {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let _ = app.handle_event(&plain(KeyCode::Esc)); // keep the text, back to rows
    let _ = app.handle_event(&plain(KeyCode::Up)); // onto "tests"
    let screen = screen_text(&render_to_buffer(&mut app, 80, 24));
    assert!(
        screen.contains("just the lints"),
        "Esc keeps what was written: {screen}"
    );

    let mut reply = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        reply.try_recv().expect("the confirmed row answers at once"),
        vec!["tests".to_string()],
        "the row Enter stood on answers, not the draft left unmarked"
    );
}

#[test]
fn enter_on_the_compose_row_sends_the_draft_it_stands_on() {
    // The same confirmation one row further down: the compose row's
    // content is its words, so Enter where it stands sends them. Before
    // this it sent nothing at all and took the written reply down with
    // the resolved question.
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Anything to add?",
        options: &[("nothing", None)],
        multi_select: false,
    }]);
    let _ = app.handle_event(&plain(KeyCode::Down)); // to the compose row
    let _ = app.handle_event(&plain(KeyCode::Enter)); // open the draft
    for ch in "run the lints first".chars() {
        let _ = app.handle_event(&plain(KeyCode::Char(ch)));
    }
    let _ = app.handle_event(&plain(KeyCode::Esc)); // step back, cursor stays put

    let mut reply = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        reply
            .try_recv()
            .expect("the row Enter stands on answers at once"),
        vec!["run the lints first".to_string()],
        "the compose row answers with the words it holds"
    );
}

#[test]
fn esc_dismisses_the_pending_question_with_no_answers() {
    let (mut app, mut replies) = question_app(&[StagedQuestion {
        question: "Deploy now?",
        options: &[("yes", None)],
        multi_select: false,
    }]);
    let reply = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Esc)));
    assert_eq!(
        reply.blocking_recv(),
        Ok(Vec::<String>::new()),
        "a dismissal answers with no labels"
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    assert!(
        !screen_text(&terminal).contains("Question"),
        "the overlay leaves with the question: {}",
        screen_text(&terminal)
    );
}

#[test]
fn a_batch_of_questions_advances_one_at_a_time() {
    let (mut app, mut replies) = question_app(&[
        StagedQuestion {
            question: "First: continue?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Second: really?",
            options: &[("ok", None)],
            multi_select: false,
        },
    ]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("First: continue?") && !screen.contains("Second: really?"),
        "only the first question of a batch shows: {screen}"
    );
    assert!(
        screen.contains("Question 1 of 2"),
        "the title counts the batch the hint line used to: {screen}"
    );

    let first = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        first.blocking_recv(),
        Ok(vec!["yes".to_string()]),
        "the first question answers"
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let screen = screen_text(&terminal);
    assert!(
        screen.contains("Second: really?") && !screen.contains("First: continue?"),
        "answering advances to the next question: {screen}"
    );
    let second = replies.remove(0);
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        second.blocking_recv(),
        Ok(vec!["ok".to_string()]),
        "the second question answers"
    );
}

#[test]
fn a_question_no_one_asks_leaves_the_screen() {
    // A prompt's reply channel is the asking side's stake in it, so a
    // closed one is the only notice the overlay gets that a question has
    // been withdrawn — the dispatch cancelled mid-ask, or its answers
    // otherwise abandoned. Read it, or the user goes on being shown a
    // question nobody will ever read, over the reply that replaced it.
    // (A dismissal is *not* that notice: one `Esc` answers one question,
    // and the questions behind it stay asked.)
    let (mut app, mut replies) = question_app(&[
        StagedQuestion {
            question: "First: continue?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Second: really?",
            options: &[("ok", None)],
            multi_select: false,
        },
    ]);
    render_to_buffer(&mut app, 80, 30);

    // The asking side goes away while the second is still on its way to
    // the screen: its receiver drops, and the prompt with it.
    app.handle_event(&plain(KeyCode::Esc));
    drop(replies.remove(1));
    app.poll_question_requests();

    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        !screen.contains("Second: really?"),
        "the withdrawn question leaves with the ask: {screen}"
    );
    assert!(
        !screen.contains("Question"),
        "and the overlay goes with the last live question: {screen}"
    );
}

#[test]
fn the_walk_keeps_its_question_when_the_one_ahead_is_withdrawn() {
    // Withdrawal shrinks the queue ahead of the walk as often as behind
    // it. The view counts questions, not the numbers they wore on their
    // way in, so a question taken away up front moves the count under the
    // one on screen without moving the screen off it.
    let (mut app, mut replies) = question_app(&[
        StagedQuestion {
            question: "First: continue?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Second: really?",
            options: &[("ok", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Third: surely?",
            options: &[("hm", None)],
            multi_select: false,
        },
    ]);
    render_to_buffer(&mut app, 80, 30);
    app.handle_event(&plain(KeyCode::Right)); // onto the second
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("Second: really?") && screen.contains("Question 2 of 3"),
        "the walk arrives at the second of three: {screen}"
    );

    drop(replies.remove(0)); // the ask gives up on the first
    app.poll_question_requests();
    let screen = screen_text(&render_to_buffer(&mut app, 80, 30));
    assert!(
        screen.contains("Second: really?"),
        "the question under the walk survives the withdrawal: {screen}"
    );
    assert!(
        screen.contains("Question 1 of 2") && !screen.contains("First: continue?"),
        "the count is the questions that are left and the place this one
        holds among them, not the numbers they wore arriving: {screen}"
    );
}
#[test]
fn the_batch_hint_narrows_esc_to_the_question_under_it() {
    // Over a queue, `[esc] dismiss` reads as the queue's own dismissal —
    // one press and everything behind the question on screen goes with
    // it, which is exactly what it never does. The hint now names the
    // question `Esc` answers, and says so plainly when there is nothing
    // else to dismiss anyway.
    let two = [
        StagedQuestion {
            question: "First: continue?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Second: really?",
            options: &[("ok", None)],
            multi_select: false,
        },
    ];
    let (mut app, _replies) = question_app(&two);
    let screen = screen_text(&render_to_buffer(&mut app, 100, 30));
    assert!(
        screen.contains("[esc] dismiss one"),
        "with a queue behind it, Esc is said for one question: {screen}"
    );

    let one = [StagedQuestion {
        question: "Just one: continue?",
        options: &[("yes", None)],
        multi_select: false,
    }];
    let (mut app, _replies) = question_app(&one);
    let screen = screen_text(&render_to_buffer(&mut app, 100, 30));
    assert!(
        screen.contains("[esc] dismiss") && !screen.contains("[esc] dismiss one"),
        "with nothing behind it there is no one to qualify: {screen}"
    );
}
#[test]
fn the_question_sheet_owns_the_composer() {
    let mut app = app();
    for c in ['d', 'r', 'a', 'f', 't'] {
        app.handle_event(&plain(KeyCode::Char(c)));
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(dch_tui::QuestionPrompt {
        question: "Answer me?".to_string(),
        header: None,
        options: vec![dch_tui::QuestionChoice {
            label: "yes".to_string(),
            description: None,
        }],
        multi_select: false,
        reply,
    })
    .expect("receiver alive");
    app.set_question_requests(rx);
    app.poll_question_requests();
    assert!(
        !app.composer_caret_visible(),
        "the sheet owns the composer — no cursor under it"
    );

    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(submit_tx);
    assert!(
        app.handle_event(&plain(KeyCode::Char('x'))),
        "a letter is the sheet's, and opens the draft rather than the composer"
    );
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert!(
        submit_rx.try_recv().is_err(),
        "Enter must not submit a drafted composer while a question is up"
    );
    assert_eq!(
        reply_rx.blocking_recv(),
        Ok(vec!["x".to_string()]),
        "the typed reply answers the question; the composer kept its own words"
    );
    assert_eq!(
        app.input(),
        "draft",
        "the draft survives the question untouched"
    );
    assert!(
        app.composer_caret_visible(),
        "the caret returns once the question is answered"
    );
}

#[test]
fn run_end_clears_pending_questions() {
    let (mut app, replies) = question_app(&[
        StagedQuestion {
            question: "First: continue?",
            options: &[("yes", None)],
            multi_select: false,
        },
        StagedQuestion {
            question: "Second: really?",
            options: &[("ok", None)],
            multi_select: false,
        },
    ]);
    assert!(
        app.tick_wake(std::time::Instant::now()),
        "retiring questions on an idle agent must request a redraw"
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    assert!(
        !screen_text(&terminal).contains("Question"),
        "no overlay survives the run's end: {}",
        screen_text(&terminal)
    );
    for reply in replies {
        assert!(
            reply.blocking_recv().is_err(),
            "a retired question's reply channel must close, reading as a dismissal"
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
fn a_paste_under_a_question_never_reaches_the_composer() {
    // The question overlay owns the composer exactly as the permission
    // overlay does — the caret is hidden and the draft invisible — so a
    // paste while it is up must not seed text the user finds later. The
    // paste itself is answered: it opens the question's own draft and
    // lands there (pinned separately below); this pin holds only the
    // composer's side of that boundary.
    let (mut app, _replies) = question_app(&[StagedQuestion {
        question: "Deploy the release now?",
        options: &[("yes", None), ("no", None)],
        multi_select: false,
    }]);
    assert!(app.handle_event(&Event::Paste("pasted behind the question".to_string())));
    assert_eq!(
        app.input(),
        "",
        "a paste while a question is up must not reach the composer"
    );

    // Answer the question — the overlay retires — and pasting works.
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert!(app.handle_event(&Event::Paste("after the answer".to_string())));
    assert_eq!(
        app.input(),
        "after the answer",
        "with the question gone, paste reaches the composer again"
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
        19,
    )));

    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        reply_rx.blocking_recv(),
        Ok(true),
        "the prompt still answers after the swallowed click"
    );
    let mut answered = render_to_buffer(&mut app, 80, 25);
    let caret = answered.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (2 + 11, 19),
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
            steered: false,
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
fn ctrl_c_ctrl_c_under_a_pending_prompt_is_one_action_as_ever() {
    // A permission ask passes the chord (it owns Esc, not Ctrl+C):
    // the double action behaves exactly as it does without the
    // sheet — arm, then the one fire that cancels and exits.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cancels: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&cancels);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(move || {
        sink.lock().expect("sink").push(());
    }));

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
        "through the sheet the first press still only arms"
    );
    assert!(!app.is_quitting(), "the arming press leaves nothing");

    assert!(app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert_eq!(
        cancels.lock().expect("sink").len(),
        1,
        "the fire cancels through the sheet as without it"
    );
    assert!(app.is_quitting(), "and the same fire terminates");
}
#[test]
fn an_empty_reply_text_never_becomes_an_empty_text_block() {
    let (observer, mut app) = thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "tool deliberation"));
    observer.on_response(&responded(0, ""));
    let terminal = render_to_buffer(&mut app, 80, 24);
    drop(terminal);
    let TuiMessage::Assistant { blocks, .. } = app.conversation().last().expect("a reply message")
    else {
        panic!("the graduated reply must land as an assistant message");
    };
    assert!(
        blocks
            .iter()
            .all(|block| !matches!(block, ContentBlock::Text { text } if text.is_empty())),
        "an empty reply text must not record a Text block — the wire rejects it"
    );
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking { .. })),
        "the turn's reasoning still graduates onto the message"
    );
}

#[test]
fn the_expanded_live_body_caps_rendered_lines() {
    let (observer, mut app) = live_thinking_app();
    // One logical line, far wider than the pane: it wraps into far
    // more rendered rows than the cap, so only the cap's tail may
    // render while the stream is live.
    let wall = "word ".repeat(4000);
    observer.on_thinking_delta(&thinking_delta(0, &wall));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let body = rows.iter().filter(|row| row.contains("word")).count();
    assert!(
        body <= 500,
        "the live body renders at most the cap's wrapped lines, got {body}"
    );
}

#[test]
fn redacted_reasoning_renders_a_placeholder_body() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, ""));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter()
            .any(|row| row.contains("reasoning withheld by the provider")),
        "withheld reasoning shows its placeholder, not blanks: {rows:?}"
    );
}

#[test]
fn a_live_thinking_stream_renders_expanded_and_collapses_at_graduation() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "pondering the thing"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("▾ Thinking")),
        "Follow renders the live stream expanded: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("pondering the thing")),
        "the live body is on display: {rows:?}"
    );
    drop(terminal);
    observer.on_response(&responded(0, "the answer"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("▸ Thinking")),
        "graduation auto-collapses the section: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("the answer")),
        "the reply renders beside its reasoning: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("pondering the thing")),
        "the folded body is gone from the frame: {rows:?}"
    );
}

#[test]
fn collapsing_a_live_thinking_stream_suppresses_the_next() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "first thoughts"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let row = rows
        .iter()
        .position(|row| row.contains("▾ Thinking"))
        .expect("the live row renders in Follow");
    let column = cell_col(&rows[row], "Thinking");
    drop(terminal);
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    observer.on_response(&responded(0, "answer one"));
    observer.on_thinking_delta(&thinking_delta(1, "second thoughts"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("▸ Thinking")),
        "a suppressed session renders the next stream collapsed from its first delta: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("second thoughts")),
        "the suppressed stream's body never shows: {rows:?}"
    );
}

#[test]
fn expanding_a_live_thinking_stream_returns_to_follow() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "first thoughts"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let row = rows
        .iter()
        .position(|row| row.contains("▾ Thinking"))
        .expect("the live row renders in Follow");
    let column = cell_col(&rows[row], "Thinking");
    drop(terminal);
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    observer.on_response(&responded(0, "answer one"));
    observer.on_thinking_delta(&thinking_delta(1, "second thoughts"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("▾ Thinking"))
            && rows.iter().any(|row| row.contains("second thoughts")),
        "a live expand returns the session to Follow: {rows:?}"
    );
    drop(terminal);
    observer.on_response(&responded(1, "answer two"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    assert!(
        !row_texts(&terminal)
            .iter()
            .any(|row| row.contains("second thoughts")),
        "back in Follow, graduation auto-collapses again"
    );
}

#[test]
fn completed_thinking_clicks_toggle_only_that_section() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "first thoughts"));
    observer.on_response(&responded(0, "answer one"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let row = rows
        .iter()
        .position(|row| row.contains("▸ Thinking"))
        .expect("the graduated section renders collapsed");
    let column = cell_col(&rows[row], "Thinking");
    drop(terminal);
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("▾ Thinking")),
        "the completed section expands on its own click: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("first thoughts")),
        "the completed section's body shows on expand: {rows:?}"
    );
    observer.on_thinking_delta(&thinking_delta(1, "live thoughts"));
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("live thoughts")),
        "the completed click left the session in Follow — the live stream still expands: {rows:?}"
    );
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        !rows.iter().any(|row| row.contains("first thoughts")),
        "the second completed click folds the same section back, locally: {rows:?}"
    );
}

#[test]
fn the_pulse_strip_sits_above_the_composer_in_the_border_color() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "mulling"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let edge = theme.ui.primary;
    let buffer = terminal.backend().buffer();
    // The composer announces itself by its left border strip: the
    // leftmost cells of its rows carry the border color; the strip
    // renders one row above the run's top.
    let mut composer_bottom = None;
    for row in (1..24).rev() {
        if let Some(cell) = buffer.cell((0, row))
            && cell.bg == edge
        {
            composer_bottom = Some(row);
            break;
        }
    }
    let mut composer_top = composer_bottom.expect("the composer's edge renders");
    while composer_top > 0
        && let Some(cell) = buffer.cell((0, composer_top.saturating_sub(1)))
        && cell.bg == edge
    {
        composer_top = composer_top.saturating_sub(1);
    }
    let strip_row = composer_top.saturating_sub(1);
    let mut saw_run = false;
    for column in 0..80u16 {
        if let Some(cell) = buffer.cell((column, strip_row))
            && cell.symbol() == "─"
        {
            assert_eq!(
                cell.fg, edge,
                "the strip paints in the prompt-box border color"
            );
            saw_run = true;
        }
    }
    assert!(saw_run, "the breathing bar renders in the strip row");
}

#[test]
fn an_idle_frame_draws_no_pulse_strip() {
    let (_observer, mut app) = thinking_app();
    let terminal = render_to_buffer(&mut app, 80, 24);
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let buffer = terminal.backend().buffer();
    for row in 0..24u16 {
        for column in 0..80u16 {
            if let Some(cell) = buffer.cell((column, row))
                && cell.symbol() == "─"
                && cell.fg == theme.ui.primary
            {
                panic!("no strip cell paints while no reasoning streams, at {column},{row}");
            }
        }
    }
}

#[test]
fn the_animation_wake_runs_only_while_the_agent_runs() {
    let (_observer, state) = dch_tui::TuiObserverState::new().into_observer();
    let idle = TuiApp::from_observer_state(config_with_theme("dracula"), state.clone());
    assert!(
        !idle.pulse_animation_living(),
        "an idle session never pays the animation cadence"
    );
    drop(idle);
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let running = TuiApp::from_observer_state(config_with_theme("dracula"), state);
    assert!(
        running.pulse_animation_living(),
        "a running agent drives the fast frame cadence, reasoning or not"
    );
}

#[test]
fn an_idle_running_session_beats_on_its_own_heartbeat() {
    // The heartbeat is unconditional and its gate is strip-blind: a
    // run in flight with nothing else rendering — no tool, no
    // streaming chunk, no keystroke, strip configured off — still
    // redraws at the animation step, so the spinner never waits on
    // another lane's traffic to move.
    let (_observer, state) = dch_tui::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Off;
    let app = TuiApp::from_observer_state(config, state.clone());
    assert!(
        !app.pulse_animation_living(),
        "agent idle — nothing animates, strip off included"
    );
    drop(app);
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Off;
    let running = TuiApp::from_observer_state(config, state);
    assert!(
        running.pulse_animation_living(),
        "agent running with the strip off — the heartbeat still draws: \
         the spinner owns its cadence, borrowed from nobody"
    );
}

#[test]
fn a_running_tool_rides_the_fast_cadence_whatever_the_strip_says() {
    // The tool rows' spinners turn on the animation step, not the
    // coarse tick: with the strip off — no bar to animate — a call in
    // flight still wakes the fast cadence on its own, or its dot
    // beats against the frame clock and the tenth-of-a-second lands
    // late.
    let (_observer, state) = dch_tui::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Off;
    let app = TuiApp::from_observer_state(config, state.clone());
    assert!(
        !app.pulse_animation_living(),
        "idle with the strip off pays no cadence at all"
    );
    drop(app);
    state
        .active_tools
        .lock()
        .unwrap()
        .push(dch_tui::message::ActiveTool {
            call_id: "call-1".to_string(),
            name: "Bash".to_string(),
            input_summary: String::new(),
            start: std::time::Instant::now(),
        });
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Off;
    let working = TuiApp::from_observer_state(config, state);
    assert!(
        working.pulse_animation_living(),
        "a tool in flight, run idle, strip off — the call still drives \
         the fast cadence and its elapsed row refreshes at the \
         animation step"
    );
}

#[test]
fn a_stale_living_buffer_after_the_run_draws_nothing() {
    let (observer, state) = dch_tui::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), state.clone());
    observer.on_thinking_delta(&thinking_delta(
        0,
        "orphaned by a cancel that fired no event",
    ));
    state
        .agent_running
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().all(|row| !row.contains("Thinking")),
        "no phantom thinking row after the run ends, even with a living buffer"
    );
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let edge = theme.ui.primary;
    let buffer = terminal.backend().buffer();
    let mut strip_cells = 0;
    for row in 1..23u16 {
        for column in 0..80u16 {
            if let Some(cell) = buffer.cell((column, row))
                && cell.symbol() == "─"
                && cell.fg == edge
            {
                strip_cells += 1;
            }
        }
    }
    assert_eq!(
        strip_cells, 0,
        "no phantom pulse strip after the run ends, even with a living buffer"
    );
}

#[test]
fn the_pulse_strip_shows_while_the_agent_runs_without_thinking() {
    let (_observer, state) = dch_tui::TuiObserverState::new().into_observer();
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), state.clone());
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let terminal = render_to_buffer(&mut app, 80, 24);
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let edge = theme.ui.primary;
    let buffer = terminal.backend().buffer();
    let mut saw_run = false;
    for row in 1..23u16 {
        for column in 0..80u16 {
            if let Some(cell) = buffer.cell((column, row))
                && cell.symbol() == "─"
                && cell.fg == edge
            {
                saw_run = true;
            }
        }
    }
    assert!(
        saw_run,
        "the strip breathes for the whole run, reasoning or not"
    );
}

#[test]
fn the_strip_padding_carries_no_style() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "mulling"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let edge = theme.ui.primary;
    let buffer = terminal.backend().buffer();
    let mut strip_row = None;
    for row in 1..23u16 {
        for column in 0..80u16 {
            if let Some(cell) = buffer.cell((column, row))
                && cell.symbol() == "─"
                && cell.fg == edge
            {
                strip_row = Some(row);
            }
        }
    }
    let Some(strip_row) = strip_row else {
        panic!("the strip must be on screen to pin its padding");
    };
    for column in 0..80u16 {
        if let Some(cell) = buffer.cell((column, strip_row))
            && cell.symbol() == " "
            && (cell.fg != ratatui::prelude::Color::Reset
                || cell.bg != ratatui::prelude::Color::Reset)
        {
            panic!(
                "strip padding stays unstyled — styled spaces read as a tinted band: \
                 ({column},{strip_row}) fg {:?} bg {:?}",
                cell.fg, cell.bg
            );
        }
    }
}

#[test]
fn the_strip_owns_the_very_top_row_when_configured() {
    let (_observer, state) = dch_tui::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut app = TuiApp::from_observer_state(config_with_strip_top(), state);
    let terminal = render_to_buffer(&mut app, 80, 24);
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let edge = theme.ui.primary;
    let buffer = terminal.backend().buffer();
    let mut top_bar = 0;
    for column in 0..80u16 {
        if let Some(cell) = buffer.cell((column, 0))
            && cell.symbol() == "─"
            && cell.fg == edge
        {
            top_bar += 1;
        }
    }
    assert!(
        top_bar > 0,
        "the strip owns the terminal's first row when configured to the top"
    );
    let mut composer_top = None;
    for row in (1..24).rev() {
        if let Some(cell) = buffer.cell((0, row))
            && cell.bg == edge
        {
            composer_top = Some(row);
            break;
        }
    }
    let composer_top = composer_top.expect("the composer's edge renders");
    let spacer_row = composer_top.saturating_sub(1);
    let mut spacer_bar = 0;
    for column in 0..80u16 {
        if let Some(cell) = buffer.cell((column, spacer_row))
            && cell.symbol() == "─"
        {
            spacer_bar += 1;
        }
    }
    assert_eq!(
        spacer_bar, 0,
        "the row above the composer carries no bar in top mode"
    );
}

#[test]
fn an_idle_top_frame_draws_no_strip() {
    let (_observer, state) = dch_tui::TuiObserverState::new().into_observer();
    let mut app = TuiApp::from_observer_state(config_with_strip_top(), state);
    let terminal = render_to_buffer(&mut app, 80, 24);
    let buffer = terminal.backend().buffer();
    for column in 0..80u16 {
        if let Some(cell) = buffer.cell((column, 0))
            && cell.symbol() == "─"
        {
            panic!("no strip cell paints in the top row while the session idles, at {column},0");
        }
    }
}

#[test]
fn the_folded_thinking_row_rides_a_running_tool_phase() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "working through the search"));
    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 0,
        delta: "checking the tree now".to_string(),
    });
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(ActiveTool {
            call_id: "call_1".to_string(),
            name: "Grep".to_string(),
            input_summary: "\"pattern\"".to_string(),
            start: std::time::Instant::now(),
        });
    let rows = row_texts(&render_to_buffer(&mut app, 80, 30));
    let folded = rows
        .iter()
        .position(|row| row.contains("▸ Thinking"))
        .expect("the folded row rides the tool phase");
    let tool = rows
        .iter()
        .position(|row| row.contains("Grep \"pattern\""))
        .expect("the running tool renders its row");
    assert!(
        folded < tool,
        "the folded block sits above the tool rows: {rows:?}"
    );
    let column = cell_col(&rows[folded], "Thinking");
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(folded).unwrap_or(0),
    );
    let rows = row_texts(&render_to_buffer(&mut app, 80, 30));
    assert!(
        rows.iter().any(|row| row.contains("▸ Thinking"))
            && !rows
                .iter()
                .any(|row| row.contains("working through the search")),
        "a click on the folded row changes nothing — the transcript's \
         graduated block owns the toggle: {rows:?}"
    );
}

#[test]
fn the_folded_thinking_row_sits_above_the_streaming_reply() {
    // The live lane reads in conversation order: the reasoning
    // arrived before the reply it introduced, so the folded row
    // renders above the streaming text, not below it.
    let (observer, state) = dch_tui::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), state);
    observer.on_thinking_delta(&thinking_delta(0, "reasoning phase"));
    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 0,
        delta: "the reply streams now".to_string(),
    });
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    let thinking = rows
        .iter()
        .position(|row| row.contains("▸ Thinking"))
        .expect("the folded row renders");
    let reply = rows
        .iter()
        .position(|row| row.contains("the reply streams now"))
        .expect("the streaming reply renders");
    assert!(
        thinking < reply,
        "the folded thinking row sits above the reply it precedes: {rows:?}"
    );
}

#[test]
fn the_graduated_duration_matches_the_frozen_live_span() {
    // The live row freezes at the fold and the graduated section
    // measures the same instant, so the number does not visibly
    // rewrite when the reply settles into the conversation.
    let (observer, state) = dch_tui::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let thinking = Arc::clone(&state.thinking);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), state);
    observer.on_thinking_delta(&thinking_delta(0, "reasoning phase"));
    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 0,
        delta: "the reply".to_string(),
    });
    {
        let mut stream = thinking.lock().expect("the thinking lock");
        stream.began = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(10));
        stream.text_began_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(5));
    }
    let live = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        live.iter().any(|row| row.contains("▸ Thinking · 5s")),
        "the frozen live span reads five seconds: {live:?}"
    );
    observer.on_response(&responded(0, "the reply"));
    let settled = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        settled.iter().any(|row| row.contains("Thinking · 5s")),
        "the graduated section carries the same five-second span: {settled:?}"
    );
}

#[test]
fn the_folded_thinking_timer_freezes_at_the_fold() {
    // The folded row measures the thinking phase, which ended when
    // the reply began: the displayed span freezes at that instant and
    // no longer runs on beside the streaming text.
    let (observer, state) = dch_tui::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), state.clone());
    observer.on_thinking_delta(&thinking_delta(0, "reasoning phase"));
    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 0,
        delta: "the reply takes over".to_string(),
    });
    {
        let mut stream = state.thinking.lock().expect("the thinking lock");
        stream.began = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(10));
        stream.text_began_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(5));
    }
    let first = row_texts(&render_to_buffer(&mut app, 80, 24));
    let row = first
        .iter()
        .find(|row| row.contains("▸ Thinking"))
        .expect("the folded row renders");
    assert!(
        row.contains("· 5s"),
        "the folded timer shows the frozen thinking-phase span, not the elapsed \
         since the stream began: {row:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = row_texts(&render_to_buffer(&mut app, 80, 24));
    let row = second
        .iter()
        .find(|row| row.contains("▸ Thinking"))
        .expect("the folded row still renders");
    assert!(
        row.contains("· 5s"),
        "the frozen span does not grow while the reply streams: {row:?}"
    );
}

#[test]
fn the_live_lane_folds_when_the_replys_text_begins() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "first stretch of reasoning"));
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("▾ Thinking")),
        "the lane renders while only reasoning streams: {rows:?}"
    );
    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 0,
        delta: "the answer takes shape".to_string(),
    });
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("▸ Thinking")),
        "the lane folds to its collapsed row at the reply's first text delta: {rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("first stretch of reasoning")),
        "the folded lane shows no body: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("the answer takes shape")),
        "the reply streams on beside the folded lane: {rows:?}"
    );
    observer.on_thinking_delta(&thinking_delta(0, " and a late interleave"));
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("▸ Thinking")),
        "the collapsed row rides the lane for the rest of the turn: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("and a late interleave")),
        "late interleaved deltas keep accruing off-display: {rows:?}"
    );
    observer.on_response(&responded(0, "the answer takes shape"));
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    let row = rows
        .iter()
        .position(|row| row.contains("▸ Thinking"))
        .expect("the section graduates folded");
    let column = cell_col(&rows[row], "Thinking");
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(row).unwrap_or(0),
    );
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter()
            .any(|row| row.contains("first stretch of reasoning"))
            && rows.iter().any(|row| row.contains("and a late interleave")),
        "reasoning from both sides of the fold graduates intact: {rows:?}"
    );
}

#[test]
fn a_stale_stream_folds_when_a_new_reply_streams() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "orphaned by a cancel"));
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("Thinking")),
        "the orphan renders until something supersedes it: {rows:?}"
    );
    observer.on_text_delta(&loopctl::observer::TextDeltaContext {
        turn: 1,
        delta: "a fresh reply".to_string(),
    });
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        !rows.iter().any(|row| row.contains("Thinking")),
        "a stale stream leaves the display the moment a new reply streams: {rows:?}"
    );
}

#[test]
fn a_body_click_collapses_a_live_thinking_stream() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "reasoning laid bare"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let marker = rows
        .iter()
        .position(|row| row.contains("▾ Thinking"))
        .expect("the live row renders in Follow");
    let body = marker.saturating_add(1);
    let body_row = rows
        .get(body)
        .expect("the body row sits directly under the marker");
    assert!(
        body_row.contains("reasoning laid bare"),
        "the live body is on display: {rows:?}"
    );
    let column = cell_col(body_row, "reasoning");
    drop(terminal);
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(body).unwrap_or(0),
    );
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("▸ Thinking")),
        "a body click collapses the live stream: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("reasoning laid bare")),
        "the body leaves the frame on collapse: {rows:?}"
    );
}

#[test]
fn a_body_click_folds_a_completed_section_locally() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "the recorded reasoning"));
    observer.on_response(&responded(0, "the answer"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let summary = rows
        .iter()
        .position(|row| row.contains("▸ Thinking"))
        .expect("the graduated section renders collapsed");
    let column = cell_col(&rows[summary], "Thinking");
    drop(terminal);
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(summary).unwrap_or(0),
    );
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let body = rows
        .iter()
        .position(|row| row.contains("the recorded reasoning"))
        .expect("the expanded body shows");
    let column = cell_col(&rows[body], "recorded");
    drop(terminal);
    click(
        &mut app,
        u16::try_from(column).unwrap_or(0),
        u16::try_from(body).unwrap_or(0),
    );
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("the recorded reasoning")),
        "a body click folds the completed section: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("▸ Thinking")),
        "the section's summary row returns collapsed: {rows:?}"
    );
    observer.on_thinking_delta(&thinking_delta(1, "later live thoughts"));
    let rows = row_texts(&render_to_buffer(&mut app, 80, 24));
    assert!(
        rows.iter().any(|row| row.contains("later live thoughts")),
        "the body click left the session in Follow — later streams still expand: {rows:?}"
    );
}

#[test]
fn the_strip_and_live_row_survive_a_graduation_and_new_stream() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "the first stretch"));
    observer.on_response(&responded(0, "the reply"));
    observer.on_thinking_delta(&thinking_delta(1, "the second stretch"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let edge = theme.ui.primary;
    let buffer = terminal.backend().buffer();
    let mut strip_cells = 0;
    for row in 1..23u16 {
        for column in 0..80u16 {
            if let Some(cell) = buffer.cell((column, row))
                && cell.symbol() == "─"
                && cell.fg == edge
            {
                strip_cells += 1;
            }
        }
    }
    assert!(
        strip_cells > 0,
        "the strip keeps breathing after a graduation and across a new stream"
    );
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("▾ Thinking")),
        "the new stream's live row renders after the previous one graduated"
    );
}

#[test]
fn the_live_thinking_row_reads_the_running_register() {
    let (observer, mut app) = live_thinking_app();
    observer.on_thinking_delta(&thinking_delta(0, "dim thoughts"));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let row = rows
        .iter()
        .position(|row| row.contains("Thinking ·"))
        .expect("the live row renders");
    let column = cell_col(&rows[row], "Thinking");
    let buffer = terminal.backend().buffer();
    let column_u16 = u16::try_from(column).unwrap_or(0);
    let row_u16 = u16::try_from(row).unwrap_or(0);
    let cell = buffer
        .cell((column_u16, row_u16))
        .expect("the probed cell renders");
    assert_eq!(
        cell.fg,
        ratatui::style::Color::Reset,
        "while the stream runs, the title reads the running register — \
         the default foreground a running tool's title carries"
    );
    let elapsed_col = cell_col(&rows[row], "·");
    let elapsed_cell = buffer
        .cell((u16::try_from(elapsed_col).unwrap_or(0), row_u16))
        .expect("the elapsed span renders");
    let light = dch_tui::theme::Theme::by_name("dracula")
        .expect("the theme exists")
        .ui
        .assistant_message_fg;
    assert_eq!(
        elapsed_cell.fg, light,
        "the elapsed reads light while the stream runs — the theme's \
         reply color; grey comes with completion"
    );
}

const STATUS_BAR_ROWS: usize = 2;

fn write(app: &mut TuiApp, text: &str) {
    for ch in text.chars() {
        app.handle_event(&plain(KeyCode::Char(ch)));
    }
}

fn staged_question(
    app: &mut TuiApp,
    question: &str,
    options: &[(&str, Option<&str>)],
    multi_select: bool,
) -> tokio::sync::oneshot::Receiver<Vec<String>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(dch_tui::QuestionPrompt {
        question: question.to_string(),
        header: None,
        options: options
            .iter()
            .map(|(label, description)| dch_tui::QuestionChoice {
                label: (*label).to_string(),
                description: description.map(std::string::ToString::to_string),
            })
            .collect(),
        multi_select,
        reply,
    })
    .expect("receiver alive");
    app.set_question_requests(rx);
    app.poll_question_requests();
    let _ = tx;
    reply_rx
}

#[test]
fn a_click_on_the_question_sheet_leaves_the_hidden_caret_alone() {
    // The sheet covers the composer; a press on the rows it covers would
    // otherwise move a caret the user cannot see, and the draft would come
    // back edited in place once the question was answered.
    let mut app = app();
    app.handle_event(&Event::Paste("hello world".to_string()));
    let _answer = staged_question(&mut app, "which one?", &[("one", None)], false);
    let mut before = render_to_buffer(&mut app, 80, 25);
    let before_caret = before.backend_mut().get_cursor_position().unwrap();

    assert!(app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        2 + 4,
        19,
    )));
    let mut clicked = render_to_buffer(&mut app, 80, 25);
    let clicked_caret = clicked.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (clicked_caret.x, clicked_caret.y),
        (before_caret.x, before_caret.y),
        "the press under the sheet moved nothing"
    );

    assert!(app.handle_event(&plain(KeyCode::Enter)));
    let mut answered = render_to_buffer(&mut app, 80, 25);
    let caret = answered.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        (caret.x, caret.y),
        (2 + 11, 19),
        "the composer comes back exactly where the press landed on the sheet"
    );
}

#[test]
fn the_wheel_still_pages_under_a_question() {
    // Reading what the run said above the box is how a question gets
    // answered, so the wheel stays live while the sheet holds the presses.
    let (mut app, _answers) = question_app(&[StagedQuestion {
        question: "which one?",
        options: &[("one", None)],
        multi_select: false,
    }]);
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
            steered: false,
        });
    }
    let _ = render_to_buffer(&mut app, 80, 24);
    let before = app.scroll_offset();

    assert!(app.handle_event(&wheel_event(MouseEventKind::ScrollUp)));
    assert!(
        app.scroll_offset() > before,
        "the wheel pages the transcript under the sheet"
    );
    assert_eq!(
        app.scroll_offset(),
        before.saturating_add(1),
        "one notch, one line, as everywhere else"
    );
}

#[test]
fn page_keys_page_the_transcript_under_a_question() {
    // Paging is not an answer to anything; swallowing it left a user
    // scrolling for context with a keyboard that answered nothing.
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which one?",
        options: &[("one", None)],
        multi_select: false,
    }]);
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
            steered: false,
        });
    }
    let mut before_render = render_to_buffer(&mut app, 80, 24);
    let _ = before_render.backend_mut();
    let before = app.scroll_offset();

    assert!(app.handle_event(&plain(KeyCode::PageUp)));
    assert!(
        app.scroll_offset() > before,
        "PageUp pages the conversation while the question waits"
    );
    let terminal = render_to_buffer(&mut app, 80, 24);
    assert!(
        screen_text(&terminal).contains("Question"),
        "the sheet is still up after the paging key"
    );

    assert!(app.handle_event(&plain(KeyCode::PageDown)));
    assert!(app.handle_event(&plain(KeyCode::PageUp)));
    assert!(
        app.scroll_offset() > 0,
        "the paging keys keep working under a question"
    );

    assert!(app.handle_event(&plain(KeyCode::Enter)));
    let answered = answers.remove(0).try_recv().expect("answer sent");
    assert_eq!(answered, vec!["one".to_string()]);
}

#[test]
fn the_verbosity_key_still_cycles_under_a_question() {
    let (mut app, _answers) = question_app(&[StagedQuestion {
        question: "which one?",
        options: &[("one", None)],
        multi_select: false,
    }]);
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
    let normal = row_texts(&render_to_buffer(&mut app, 80, 30)).join("\n");
    assert!(
        normal.contains("✓ Reading a.rs… (0.4s)"),
        "Normal shows the summary while a question waits: {normal:?}"
    );

    app.handle_event(&key(KeyCode::F(2), KeyModifiers::NONE));
    let verbose = row_texts(&render_to_buffer(&mut app, 80, 30)).join("\n");
    assert!(
        verbose.contains("    Input:"),
        "F2 cycles verbosity under the sheet: {verbose:?}"
    );
    assert!(
        verbose.contains("Question"),
        "the sheet survives the app-level key"
    );
}

#[test]
fn page_keys_page_the_transcript_behind_a_permission_ask() {
    // The same finding for the other overlay: only y, n, and Esc answer a
    // permission ask, and the sheet has no claim on the paging keys.
    let mut app = app();
    let now = chrono::Utc::now();
    for i in 0..30 {
        app.push_message(TuiMessage::User {
            text: format!("line{i:02}"),
            timestamp: now,
            steered: false,
        });
    }
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
    let _ = render_to_buffer(&mut app, 80, 24);
    let before = app.scroll_offset();

    assert!(app.handle_event(&plain(KeyCode::PageUp)));
    assert!(
        app.scroll_offset() > before,
        "PageUp pages the conversation behind the permission sheet"
    );
    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    assert_eq!(
        reply_rx.try_recv(),
        Ok(true),
        "the ask still answers after the paging key"
    );
}

#[test]
fn a_click_behind_the_permission_sheet_moves_nothing() {
    // The permission sheet answers y, n, and Esc and holds the rest of the
    // keyboard; the pointer is its business too. A press on the row the
    // composer occupies used to fall through to it, moving a caret inside
    // a box the user could not see — the draft came back edited at a place
    // nobody had pointed at, once the sheet was gone.
    let stage = |app: &mut TuiApp| {
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
    };
    let click_the_composer = |app: &mut TuiApp| {
        app.handle_event(&mouse_event(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            6,
            19,
        ))
    };

    // The map first: with nothing on the screen this press is a click on
    // the composer's text, and the caret goes where it was pointed.
    let mut bare = app();
    bare.handle_event(&Event::Paste("hello world".to_string()));
    let _ = render_to_buffer(&mut bare, 80, 25);
    assert!(click_the_composer(&mut bare));
    let mut moved = render_to_buffer(&mut bare, 80, 25);
    let landed = moved
        .backend_mut()
        .get_cursor_position()
        .expect("the composer owns the caret");
    assert_eq!(
        (landed.x, landed.y),
        (6, 19),
        "the same press is the composer's whenever nothing is on screen"
    );

    // With the sheet over it, the press stays on the sheet: the caret the
    // user could not see is not the one the pointer was aimed at.
    let mut app = app();
    app.handle_event(&Event::Paste("hello world".to_string()));
    stage(&mut app);
    let _ = render_to_buffer(&mut app, 80, 25);
    assert!(click_the_composer(&mut app));
    assert!(app.handle_event(&plain(KeyCode::Char('y'))));
    let mut after = render_to_buffer(&mut app, 80, 25);
    let held = after
        .backend_mut()
        .get_cursor_position()
        .expect("the composer owns the caret");
    assert_eq!(
        (held.x, held.y),
        (2 + 11, 19),
        "the caret the sheet hid never moved"
    );
}

#[test]
fn a_paste_under_a_question_opens_the_draft_with_it() {
    // A paste is a burst of keystrokes, and a keystroke at the rows opens
    // the draft; dropping the paste lost the likeliest free-form answer.
    let (mut app, _answers) = question_app(&[StagedQuestion {
        question: "which path?",
        options: &[("repo", None)],
        multi_select: false,
    }]);

    assert!(app.handle_event(&Event::Paste("/tmp/dch".to_string())));

    let screen = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        screen.contains("/tmp/dch"),
        "the paste lands in the draft: {screen}"
    );
}

#[test]
fn a_paste_into_an_open_draft_lands_at_its_caret() {
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which path?",
        options: &[("repo", None)],
        multi_select: false,
    }]);
    write(&mut app, "AB");
    app.handle_event(&plain(KeyCode::Home));

    app.handle_event(&Event::Paste("12".to_string()));

    let screen = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        screen.contains("12AB"),
        "the paste joins the draft where the caret stands: {screen}"
    );
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        answers.remove(0).try_recv().expect("answer sent"),
        vec!["12AB".to_string()]
    );
}

#[test]
fn a_pasted_block_lands_on_the_draft_as_one_line() {
    // A draft answers one question in one row: the breaks a pasted block
    // carries become the spaces between its parts, and nothing is lost.
    let (mut app, _answers) = question_app(&[StagedQuestion {
        question: "what failed?",
        options: &[("tests", None)],
        multi_select: false,
    }]);

    app.handle_event(&Event::Paste("first\r\nsecond\r\n".to_string()));

    let screen = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        screen.contains("first second"),
        "the pasted lines join on one row: {screen}"
    );
}

#[test]
fn a_short_terminal_keeps_the_row_being_written_on() {
    // A tall option list on a short terminal used to cut its own bottom
    // rows off: the hints went first, then the compose row, and the draft
    // was written blind into a box that showed none of it.
    let (mut app, _answers) = question_app(
        &[StagedQuestion {
            question: "pick a target for the release",
            options: &[
                ("alpha", Some("the first description")),
                ("beta", Some("the second description")),
                ("gamma", Some("the third description")),
                ("delta", Some("the fourth description")),
            ],
            multi_select: false,
        }; 1],
    );
    let _ = render_to_buffer(&mut app, 60, 12);

    write(&mut app, "ship it");

    let mut typed = render_to_buffer(&mut app, 60, 12);
    let screen = screen_text(&typed);
    assert!(
        screen.contains("ship it"),
        "the draft is on screen on a terminal too short for the whole sheet:\n{screen}"
    );
    assert!(
        screen.contains("[esc]"),
        "the key hints survive beside it:\n{screen}"
    );
    let caret = typed.backend_mut().get_cursor_position().unwrap();
    assert_eq!(
        usize::from(caret.y),
        row_of(&typed, "ship it"),
        "the caret rides the row the draft is drawn on"
    );
    assert!(
        usize::from(caret.y) < 12,
        "the caret is inside the terminal, not below it"
    );
}

#[test]
fn a_draft_longer_than_the_box_continues_on_the_row_below() {
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which path?",
        options: &[("repo", None)],
        multi_select: false,
    }]);

    write(
        &mut app,
        "/very/long/path/that/keeps/going/into/the/far/distance/end",
    );

    let mut typed = render_to_buffer(&mut app, 30, 20);
    let screen = screen_text(&typed);
    assert!(
        screen.contains("distance/end"),
        "the draft's tail wraps onto a row the box shows:\n{screen}"
    );
    let caret = typed.backend_mut().get_cursor_position().unwrap();
    let first_draft_row = row_of(&typed, "\u{270e}");
    assert!(
        usize::from(caret.y) > first_draft_row,
        "the caret follows the wrap to the continuation row, not the box's edge"
    );
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        answers.remove(0).try_recv().expect("answer sent"),
        vec!["/very/long/path/that/keeps/going/into/the/far/distance/end".to_string()],
        "wrapping is display only: the answer is the text the user wrote"
    );
}

#[test]
fn a_marked_option_answers_although_an_empty_compose_row_is_marked_too() {
    // The mark on a row with nothing to say adds nothing; it does not veto
    // the marks the user chose beside it.
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which of these?",
        options: &[("alpha", None), ("beta", None)],
        multi_select: true,
    }]);
    app.handle_event(&plain(KeyCode::Char(' ')));
    app.handle_event(&plain(KeyCode::Down));
    app.handle_event(&plain(KeyCode::Down));
    assert_eq!(
        screen_text(&render_to_buffer(&mut app, 60, 20))
            .matches('\u{2022}')
            .count(),
        1,
        "alpha is marked before the compose row joins it"
    );
    app.handle_event(&plain(KeyCode::Char(' ')));

    assert!(app.handle_event(&plain(KeyCode::Enter)));

    let screen = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        !screen.contains("Question"),
        "the question is answered, not opened for writing:\n{screen}"
    );
    assert_eq!(
        answers.remove(0).try_recv().expect("answer sent"),
        vec!["alpha".to_string()],
        "the marked option is the answer an empty compose row cannot hold up"
    );
}

#[test]
fn a_marked_empty_compose_row_on_its_own_opens_the_draft() {
    // With nothing else marked, the empty compose row is the whole answer
    // the user asked for, and it has no reply yet: Enter writes one.
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which of these?",
        options: &[("alpha", None), ("beta", None)],
        multi_select: true,
    }]);
    app.handle_event(&plain(KeyCode::Down));
    app.handle_event(&plain(KeyCode::Down));
    app.handle_event(&plain(KeyCode::Char(' ')));

    assert!(app.handle_event(&plain(KeyCode::Enter)));

    let screen = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        screen.contains("Question"),
        "the draft opened instead of an empty answer:\n{screen}"
    );
    assert!(
        matches!(
            answers[0].try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "nothing was sent while the draft was opened"
    );
}

#[test]
fn a_narrow_sheet_keeps_the_key_that_leaves_it() {
    // A narrow terminal cuts the hints' tail, so the key that cancels
    // leads the row: last on the list, it was the first thing lost exactly
    // when the queue's semantics most needed explaining.
    // A queue is where the hints run longest, and where the semantics the
    // row explains matter most: one press dismisses one question, not the
    // batch behind it.
    let (mut app, _answers) = question_app(&[
        StagedQuestion {
            question: "which of these?",
            options: &[("alpha", None), ("beta", None)],
            multi_select: true,
        },
        StagedQuestion {
            question: "and then?",
            options: &[("yes", None), ("no", None)],
            multi_select: true,
        },
    ]);

    // The proof is read off the hints themselves, and read in order: on a
    // terminal this narrow the row wraps past the rows the sheet gives it,
    // and the parts past the last one are dropped. Which parts those are
    // is the whole of the finding, so the assertion names the survivor and
    // the casualty rather than a glyph either.
    let rows = row_texts(&render_to_buffer(&mut app, 26, 20));
    let hints: String = rows
        .iter()
        .filter(|row| row.contains("[e"))
        .map(String::as_str)
        .collect();
    let esc = hints
        .find("[esc]")
        .expect("the key that leaves the sheet survives the cut");
    assert!(
        !hints.contains("[space]"),
        "what a narrow terminal loses is the row's tail: {hints:?}"
    );
    let enter = hints
        .find("[enter]")
        .expect("the key that confirms is on the sheet");
    assert!(
        esc < enter,
        "the key that ends the exchange leads, so the cut falls elsewhere: {hints:?}"
    );
}

#[test]
fn delete_erases_the_character_right_of_the_draft_caret() {
    let (mut app, _answers) = question_app(&[StagedQuestion {
        question: "which path?",
        options: &[("repo", None)],
        multi_select: false,
    }]);
    write(&mut app, "abc");
    app.handle_event(&plain(KeyCode::Home));

    app.handle_event(&plain(KeyCode::Delete));

    let screen = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        screen.contains("bc"),
        "Delete took the character to the right of the caret: {screen}"
    );
    assert!(
        !screen.contains("abc"),
        "and left the caret where it stood: {screen}"
    );
}

#[test]
fn word_chords_edit_the_draft_a_word_at_a_time() {
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which path?",
        options: &[("repo", None)],
        multi_select: false,
    }]);
    write(&mut app, "keep drop");

    app.handle_event(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));

    let screen = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        screen.contains("keep"),
        "Ctrl+W took the word left of the caret: {screen}"
    );
    assert!(
        !screen.contains("keep drop"),
        "and left the caret with it: {screen}"
    );

    write(&mut app, "more");
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        answers.remove(0).try_recv().expect("answer sent"),
        vec!["keep more".to_string()]
    );
}

#[test]
fn a_modified_enter_sends_the_draft_like_a_plain_one() {
    // Terminals that spell Shift+Enter as its own event made the hinted
    // `[enter] send` a lie; the draft holds one line, so a modifier adds
    // nothing the answer could carry and the key means what it says.
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which path?",
        options: &[("repo", None)],
        multi_select: false,
    }]);
    write(&mut app, "ok");

    assert!(app.handle_event(&key(KeyCode::Enter, KeyModifiers::SHIFT)));

    assert_eq!(
        answers.remove(0).try_recv().expect("answer sent"),
        vec!["ok".to_string()],
        "Shift+Enter sends, as the hint promises"
    );
}

#[test]
fn an_empty_draft_says_why_it_cannot_send_yet() {
    // Refusing to answer with whitespace is right; refusing unheard is a
    // dropped keystroke wearing the hint's own wording. The sheet's hint
    // row carries the refusal, because the notice row above the composer
    // is a row the sheet is drawn over.
    let (mut app, mut answers) = question_app(&[StagedQuestion {
        question: "which path?",
        options: &[("repo", None)],
        multi_select: false,
    }]);
    write(&mut app, "x");
    app.handle_event(&plain(KeyCode::Backspace));
    app.handle_event(&plain(KeyCode::Char(' ')));

    let blank = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        blank.contains("needs a written answer"),
        "the hint that promised a send says what is missing:\n{blank}"
    );

    assert!(app.handle_event(&plain(KeyCode::Enter)));

    let still = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        still.contains("Question"),
        "the question is still being asked:\n{still}"
    );
    assert!(
        matches!(
            answers[0].try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "nothing left the app on a refused send"
    );

    write(&mut app, "y");
    let written = screen_text(&render_to_buffer(&mut app, 60, 20));
    assert!(
        written.contains("[enter] send"),
        "with a word in it the key means send again:\n{written}"
    );
    assert!(app.handle_event(&plain(KeyCode::Enter)));
    assert_eq!(
        answers.remove(0).try_recv().expect("answer sent"),
        vec!["y".to_string()],
        "the reply travels without the whitespace it was padded with"
    );
}

#[test]
fn a_strip_at_the_top_keeps_its_row_from_the_sheet() {
    // The strip's row is not space the sheet may spend. Ask for more sheet
    // than the terminal holds — a long question with described options in a
    // short window — and the box takes every row it is offered, growing
    // upward into the strip's own row and drawing over the bar that says
    // the run is still alive.
    let (_observer, state) = dch_tui::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut app = TuiApp::from_observer_state(config_with_strip_top(), state);
    let _answer = staged_question(
        &mut app,
        // Asked for more than the window holds, which is the only case
        // where the sheet's height is a decision rather than a wish.
        "pick a target for the release with a long question line that wraps twice more",
        &[
            ("alpha", Some("the first description")),
            ("beta", Some("the second description")),
            ("gamma", Some("the third description")),
            ("delta", Some("the fourth description")),
            ("epsilon", Some("the fifth description")),
        ],
        false,
    );

    let rendered = render_to_buffer(&mut app, 40, 12);
    let rows = row_texts(&rendered);
    let screen = rows.join("\n");
    let theme = dch_tui::theme::Theme::by_name("dracula").expect("the theme exists");
    let edge = theme.ui.primary;
    let mut strip_bar = 0;
    for column in 0..40u16 {
        if let Some(cell) = rendered.backend().buffer().cell((column, 0))
            && cell.symbol() == "─"
            && cell.fg == edge
        {
            strip_bar += 1;
        }
    }
    assert!(
        strip_bar > 0,
        "the strip keeps its row while the sheet is clamped to the rest:\n{screen}"
    );
    assert!(
        screen.contains("Question"),
        "the sheet is drawn on a short terminal:\n{screen}"
    );
    for row in &rows[rows.len().saturating_sub(STATUS_BAR_ROWS)..] {
        assert!(
            !row.contains('╰') && !row.contains('╯') && !row.contains('╭'),
            "the sheet's box never reaches into the status bar:\n{screen}"
        );
    }
}

/// Feed one `TodoWrite` graduation through the shared state the way the
/// observer would, with a submission naming the whole list — the
/// display's drain picks it up on the next frame.
fn todo_graduation(app: &TuiApp, call_id: &str, submission: &str) {
    todo_graduation_with_output(app, call_id, submission, "");
}

/// Feed one `TodoWrite` graduation carrying the tool's rendered result,
/// the enforced truth a rung-law adjustment produces.
fn todo_graduation_with_output(app: &TuiApp, call_id: &str, submission: &str, output: &str) {
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Tool(
            dch_tui::observer::ToolResultDisplay {
                name: "TodoWrite".to_string(),
                call_id: call_id.to_string(),
                is_error: false,
                duration: std::time::Duration::ZERO,
                input_summary: String::new(),
                output_preview: String::new(),
                full_input: submission.to_string(),
                full_output: output.to_string(),
            },
        ));
}

#[test]
fn a_todo_update_renders_as_checkbox_rows_in_the_conversation() {
    // The list is the point of the call: the block opens with the done
    // fraction and one checkbox row per task — ☑ completed, ◐ running,
    // ☐ pending — and the rung in progress alone wears the accent,
    // while rows an update touched still standing wear the moved
    // register, so a scrolling reader sees what moved without reading
    // it as the rung running.
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"survey the land","status":"completed"},
            {"id":"2","subject":"dig the trench","status":"pending"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let flat: String = rows.join("\n");
    assert!(
        flat.contains("1/2 done"),
        "the block heads with the done fraction: {flat:?}"
    );
    assert!(
        flat.contains("\u{2611} survey the land"),
        "completed tasks render as checked boxes: {flat:?}"
    );
    assert!(
        flat.contains("\u{2610} dig the trench"),
        "pending tasks render as empty boxes: {flat:?}"
    );

    // The second update moves the trench to in-progress: the rung
    // running wears the accent, and only it does.
    todo_graduation(
        &app,
        "call-2",
        r#"{"todos":[
            {"id":"1","subject":"survey the land","status":"completed"},
            {"id":"2","subject":"dig the trench","status":"in_progress"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let buffer = terminal.backend().buffer();
    let trench_row = rows
        .iter()
        .position(|row| row.contains("\u{25d0}") && row.contains("dig the trench"))
        .expect("the moved trench's row");
    let accent_cells = (0..80u16)
        .filter(|&x| buffer[(x, u16::try_from(trench_row).unwrap())].fg == app.theme.ui.primary)
        .count();
    assert!(accent_cells > 0, "the rung in progress wears the accent");

    // A third update files a newborn beside standing work: the newborn
    // wears the moved register for the update that touched it — never
    // the in-progress accent — while the untouched standing row reads
    // plain, so color never claims a rung nobody is running.
    todo_graduation(
        &app,
        "call-3",
        r#"{"todos":[
            {"id":"1","subject":"survey the land","status":"completed"},
            {"id":"2","subject":"dig the trench","status":"in_progress"},
            {"id":"3","subject":"lay the pipe","status":"pending"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let buffer = terminal.backend().buffer();
    let newborn_row = rows
        .iter()
        .position(|row| row.contains("\u{2610}") && row.contains("lay the pipe"))
        .expect("the newborn's row");
    let newborn_row = u16::try_from(newborn_row).unwrap_or(0);
    let newborn_moved = (0..80u16)
        .filter(|&x| buffer[(x, newborn_row)].fg == app.theme.ui.secondary)
        .count();
    let newborn_accent = (0..80u16)
        .filter(|&x| buffer[(x, newborn_row)].fg == app.theme.ui.primary)
        .count();
    assert!(
        newborn_moved > 0 && newborn_accent == 0,
        "the newborn wears the moved register, never the in-progress \
         accent: {} moved cells, {} accent cells",
        newborn_moved,
        newborn_accent
    );
}

#[test]
fn the_conversation_block_shows_the_enforced_fraction() {
    // The closing-sweep shape the live sessions exposed: the model
    // submits three completed, the rung law lands the skipper one rung
    // short, and the conversation block renders the enforced list —
    // "2/3 done" with the skipper half-filled — never the request that
    // would read "3/3 done".
    let mut app = app();
    todo_graduation_with_output(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"first thing","status":"completed"},
            {"id":"2","subject":"second thing","status":"completed"},
            {"id":"3","subject":"third thing","status":"completed"}
        ]}"#,
        "# Task List\n\nSummary: 0 pending, 1 in progress, 2 completed\n\n\u{25d0} [3] third thing\n\n\u{25cf} [1] first thing\n\n\u{25cf} [2] second thing\n\n",
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let flat: String = rows.join("\n");
    assert!(
        flat.contains("2/3 done"),
        "the fraction reports the enforced list, not the request: {flat:?}"
    );
    assert!(
        !flat.contains("3/3 done"),
        "the model's requested statuses never render as the plan's truth: {flat:?}"
    );
    assert!(
        flat.contains("\u{25d0} third thing"),
        "the skipped entry renders in the rung it landed on: {flat:?}"
    );
}

#[test]
fn the_plan_stands_pinned_while_tasks_do() {
    // The plan is furniture, not a sheet: while tasks stand, the
    // section holds its rows above the composer with no key asked or
    // taken — the fraction and the checkbox rows are simply part of
    // the frame — and a display with no tasks spends no rows on it
    // at all: an empty plan is no plan, not a plan that says
    // nothing.
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"first thing","status":"completed"},
            {"id":"2","subject":"second thing","status":"in_progress"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        2,
        "the fraction stands twice — the transcript's own block and the \
         pinned section above the composer: {flat:?}"
    );
    assert!(
        flat.contains("\u{25d0} second thing"),
        "the section carries the run's current task: {flat:?}"
    );

    // Ctrl+T walks the section's postures — capped, full, hidden. A
    // list that already fits the cap opens without changing a thing;
    // the next press folds the section away (the transcript's block
    // keeps the record, the conversation takes the rows back); the
    // press after that stands it back up.
    assert!(app.handle_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    ))));
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        2,
        "a list that fits the cap opens unchanged: {flat:?}"
    );
    assert!(app.handle_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    ))));
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        1,
        "the next press hides the section — the fraction stands in the \
         transcript's block alone: {flat:?}"
    );
    assert!(
        flat.contains("1/2 · ctrl+t") && flat.contains(" TODO "),
        "the folded section leaves its badge row: the pill, the count, \
         and the chord that opens the list again: {flat:?}"
    );
    assert!(app.handle_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    ))));
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        2,
        "and the walk returns to the capped posture: {flat:?}"
    );

    // A display with no tasks filed spends no rows on a plan.
    let mut bare = TuiApp::new(config_with_theme("dracula"));
    bare.post_notice("the anchor".to_string());
    let bare_rows = row_texts(&render_to_buffer(&mut bare, 80, 30));
    assert!(
        !bare_rows.iter().any(|row| row.contains("done")),
        "no tasks, no section: {bare_rows:?}"
    );
}

#[test]
fn a_rejected_todo_update_is_not_state() {
    // The tool refused the submission — an illegal transition, a
    // duplicate in-progress, whatever the validation caught — so its
    // list never became the run's state. The display must not take
    // the captured input as truth: the block reads as the failure it
    // was (a plain error line, not auto-opened checkbox rows), and
    // the sheet keeps the last accepted list instead of flipping to
    // the rejected one — a rejected
    // "complete everything" update must never render
    // as if it had completed everything.
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"one thing","status":"in_progress"},
            {"id":"2","subject":"another thing","status":"pending"}
        ]}"#,
    );

    // The rejected update: everything completed in one call — the
    // shape the tool's one-rung rule exists to refuse.
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Tool(
            dch_tui::observer::ToolResultDisplay {
                name: "TodoWrite".to_string(),
                call_id: "call-rejected".to_string(),
                is_error: true,
                duration: std::time::Duration::ZERO,
                input_summary: String::new(),
                output_preview: String::new(),
                full_input: r#"{"todos":[
                {"id":"1","subject":"one thing","status":"completed"},
                {"id":"2","subject":"another thing","status":"completed"}
            ]}"#
                .to_string(),
                full_output: "Todo '1' cannot move from in_progress to completed".to_string(),
            },
        ));
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert!(
        !flat.contains("\u{2611} one thing"),
        "the rejected update renders as the failure it was, not as ··· checkbox rows claiming completion: {flat:?}"
    );

    // The pinned section keeps the last accepted list — the run's
    // actual state — standing where it stood.
    assert!(
        flat.contains("\u{25d0} one thing"),
        "the last accepted list stands, one thing still in progress: {flat:?}"
    );
    assert_eq!(
        flat.matches("0/2 done").count(),
        1,
        "the section counts the accepted list, not the rejected one — \
         the accepted block folded at the rejected update's row: {flat:?}"
    );
}

#[test]
fn a_description_only_todo_list_still_fills_the_display() {
    // The schema asks for `subject`, the run repairs its absence from
    // `description`, and the display used to refuse what the run had
    // accepted: the conversation rendered a bare tool block and
    // Ctrl+T answered "no tasks filed" while the run kept the tasks.
    // The display now reads the same recovery the run applies — this
    // payload is a real session's submission shape, extra schema
    // fields included.
    let mut app = app();
    todo_graduation(
        &app,
        "call-desc",
        r#"{"todos":[
            {"description":"Run the baseline gate","id":"gate","status":"in_progress","active_form":"Running the gate","owner":"lead"},
            {"description":"Read every changed file\nend to end, no skimming","id":"read","status":"pending"},
            {"id":"lone","status":"pending"},
            {"id":"bare","subject":"already subjectful","status":"pending"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert!(
        flat.contains("\u{25d0} Run the baseline gate"),
        "a description-only task renders under its derived subject: {flat:?}"
    );
    assert!(
        flat.contains("\u{2610} Read every changed file"),
        "the description's first line is the subject, the rest is not: {flat:?}"
    );
    assert!(
        flat.contains("\u{2610} Task lone"),
        "an entry with neither field names itself by its id: {flat:?}"
    );
    assert!(
        flat.contains("\u{2610} already subjectful"),
        "an explicit subject still wins over any recovery: {flat:?}"
    );
    assert!(
        flat.contains("0/4 done"),
        "the list counts the recovered rows with the spelled ones: {flat:?}"
    );
    assert_eq!(
        flat.matches("0/4 done").count(),
        2,
        "the pinned section carries the recovered list beside the \
         transcript's block: {flat:?}"
    );
}

#[test]
fn a_long_plan_caps_its_section_and_names_the_cut() {
    // The section is the plan's window, not its archive: past its four
    // task rows it says exactly what it cut, and the conversation
    // keeps the composer's furniture where it always stands — the cap
    // spends conversation rows, never the frame's own.
    let mut app = app();
    let tasks: Vec<String> = (0..9)
        .map(|index| {
            format!("{{\"id\":\"{index}\",\"subject\":\"task {index}\",\"status\":\"pending\"}}")
        })
        .collect();
    todo_graduation(
        &app,
        "call-long",
        &format!("{{\"todos\":[{}]}}", tasks.join(",")),
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert!(
        flat.contains("0/9 done"),
        "the fraction counts the whole plan, not the window: {flat:?}"
    );
    assert!(
        flat.contains("… +5 more (ctrl+t for all)"),
        "nine tasks, four shown: the section names its cut and the chord \
         that opens the whole list: {flat:?}"
    );
    assert!(
        flat.contains("\u{2610} task 3"),
        "the window holds the plan's first tasks in order: {flat:?}"
    );
    assert_eq!(
        flat.matches("\u{2610} task 8").count(),
        1,
        "the tasks past the cap live in the transcript's block alone — \
         the section is a window, not the archive: {flat:?}"
    );

    // The chord walks the postures: capped → the whole list → hidden.
    assert!(app.handle_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    ))));
    let terminal = render_to_buffer(&mut app, 80, 40);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("\u{2610} task 8").count(),
        2,
        "opened, the section carries the whole plan beside the block: {flat:?}"
    );
    assert!(
        !flat.contains("more (ctrl+t"),
        "nothing is cut when nothing is capped: {flat:?}"
    );
    assert!(app.handle_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    ))));
    let terminal = render_to_buffer(&mut app, 80, 40);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("0/9 done").count(),
        1,
        "hidden, the block alone carries the plan: {flat:?}"
    );
    assert_eq!(
        flat.matches("\u{2610} task 3").count(),
        1,
        "the folded section's rows are gone — the block alone carries \
         them: {flat:?}"
    );
    assert!(
        flat.contains("0/9 · ctrl+t") && flat.contains(" TODO "),
        "and the badge row stands in the section's place — the pill, the \
         count, and the chord: {flat:?}"
    );
    assert!(app.handle_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    ))));
    let terminal = render_to_buffer(&mut app, 80, 40);
    let flat: String = row_texts(&terminal).join("\n");
    assert!(
        flat.contains("… +5 more (ctrl+t for all)"),
        "the walk returns to the capped posture: {flat:?}"
    );
}

#[test]
fn a_steered_echo_surviving_in_a_transcript_keeps_the_plan_standing() {
    // The nudge fires on every run that lands with work left, and its
    // echo is a steer — mid-run, not a boundary — so the live display
    // keeps the plan standing through it. The resumed session owes
    // the same: the steered row in the transcript must not retire the
    // plan beside it, exactly as the live drain's flag rule holds.
    // Only a real claim — an unflagged user row — retires.
    let tool_block = TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            call_id: "call-1".to_string(),
            name: "TodoWrite".to_string(),
            input_preview: "1 item".to_string(),
            success: true,
            elapsed_secs: 0.1,
            output_preview: "Updated 1 todo task".to_string(),
            retained_input: r#"{"todos":[
                {"id":"1","subject":"the owed work","status":"pending"}
            ]}"#
            .to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    };
    let mut app = app();
    app.seed_messages(vec![
        tool_block,
        TuiMessage::User {
            text: "reminder: 1 todo item still not completed".to_string(),
            timestamp: chrono::Utc::now(),
            steered: true,
        },
        TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "noted, but not closing it".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        },
    ]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert!(
        flat.matches("0/1 done").count() >= 2,
        "the steered echo retires nothing — the plan a resumed session \
         owes stands beside it, block and section: {flat:?}"
    );

    // An unflagged user row is a claim — and a claim retires only a
    // finished plan. The owed work stands across it, carried exactly as
    // the runner carries its own store.
    app.seed_messages(vec![TuiMessage::User {
        text: "the next claim".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    }]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert!(
        flat.matches("0/1 done").count() >= 2,
        "a claim retires only a finished plan — the owed work stands \
         across it, carried like the store: {flat:?}"
    );
}

#[test]
fn a_claim_after_a_finished_plan_takes_the_section_down() {
    // The other side of the same edge, and the only one that clears
    // the section: a resumed session whose last run walked every entry
    // to completed stands with the plan still up, and the next claim —
    // an unflagged user row — is what retires it. The transcript's own
    // record of the call keeps its checkbox rows; only the standing
    // section goes.
    let tool_block = TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            call_id: "call-1".to_string(),
            name: "TodoWrite".to_string(),
            input_preview: "2 items".to_string(),
            success: true,
            elapsed_secs: 0.1,
            output_preview: "Updated 2 todo tasks".to_string(),
            retained_input: r#"{"todos":[
                {"id":"1","subject":"survey the land","status":"completed"},
                {"id":"2","subject":"dig the trench","status":"completed"}
            ]}"#
            .to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    };
    let mut app = app();
    app.seed_messages(vec![tool_block]);
    let before = render_to_buffer(&mut app, 80, 30);
    let before_flat: String = row_texts(&before).join("\n");
    assert_eq!(
        before_flat.matches("2/2 done").count(),
        2,
        "a finished plan still stands between runs, block and section: \
         {before_flat:?}"
    );

    app.seed_messages(vec![TuiMessage::User {
        text: "the next claim".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    }]);
    let after = render_to_buffer(&mut app, 80, 30);
    let after_flat: String = row_texts(&after).join("\n");
    assert_eq!(
        after_flat.matches("2/2 done").count(),
        1,
        "the claim retires the finished plan — the section is down and \
         only the call's own record still reads 2/2: {after_flat:?}"
    );
    assert!(
        after_flat.contains("dig the trench"),
        "the retired plan's transcript record keeps its rows to read: \
         {after_flat:?}"
    );
}

#[test]
fn a_cancelled_session_restores_into_its_own_plan() {
    // The cancelled-restore shape, exactly: a run lands cancelled with its plan
    // half-done, the session is saved and restored, and the user
    // prompts "resume". The transcript's error row is the
    // cancellation's record — the claim after it is a claim into owed
    // work, and the plan stands, so the display keeps naming what the
    // resumed session owes. Only a clean run's close retires a plan,
    // and the resumed run will file its own when it gets there.
    let tool_block = TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            call_id: "call-1".to_string(),
            name: "TodoWrite".to_string(),
            input_preview: "2 items".to_string(),
            success: true,
            elapsed_secs: 0.1,
            output_preview: "Updated 2 todo tasks".to_string(),
            retained_input: r#"{"todos":[
                {"id":"1","subject":"the finished half","status":"completed"},
                {"id":"2","subject":"the interrupted half","status":"in_progress"}
            ]}"#
            .to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    };
    let mut app = app();
    app.seed_messages(vec![
        tool_block,
        TuiMessage::Error {
            text: "Agent cancelled".to_string(),
            timestamp: chrono::Utc::now(),
        },
        TuiMessage::User {
            text: "resume".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
        },
    ]);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        2,
        "the cancelled session restores into its own plan — the claim \
         after the error row is a claim into owed work, not a \
         retirement: {flat:?}"
    );
    assert!(
        flat.contains("\u{25d0} the interrupted half"),
        "the section names what the resumed session still owes: {flat:?}"
    );
}

#[test]
fn a_restored_session_shows_the_last_run_s_plan() {
    // The resume path seeds the conversation directly — no graduations
    // to drain — and a restored session stands between runs, exactly
    // where a live one stands after its run lands: the last run's plan
    // is still up, its final state there to read, because the next
    // claim is what retires a plan. The transcript keeps the record
    // beside it: the seeded block renders its checkbox rows, accented
    // by the same replay the live drain uses.
    let tool_block = |call_id: &str, retained: &str| TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            call_id: call_id.to_string(),
            name: "TodoWrite".to_string(),
            input_preview: "2 items".to_string(),
            success: true,
            elapsed_secs: 0.1,
            output_preview: "Updated 2 todo tasks".to_string(),
            retained_input: retained.to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    };

    let mut restored_app = app();
    restored_app.seed_messages(vec![tool_block(
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"survey the land","status":"completed"},
            {"id":"2","subject":"dig the trench","status":"in_progress"}
        ]}"#,
    )]);
    let terminal = render_to_buffer(&mut restored_app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        2,
        "a restored session stands between runs — the last run's plan \
         stays up, block and section both: {flat:?}"
    );
    assert!(
        flat.contains("\u{25d0} dig the trench"),
        "the section carries the restored run's final state: {flat:?}"
    );
}

#[test]
fn the_plan_badge_names_the_section_in_the_queue_s_own_style() {
    // The plan opens with the waiting section's badge — a filled pill
    // stood in from the frame, the done fraction dim beside it — over
    // the secondary accent rather than the primary, so the two panels
    // read as one family of chrome with two names. A theme that defers
    // to the terminal fills nothing: the badge degrades to plain text,
    // the same concession the queue's makes.
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"first thing","status":"completed"},
            {"id":"2","subject":"second thing","status":"in_progress"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let badge_row = rows
        .iter()
        .position(|row| row.contains(" TODO "))
        .expect("the badge names the section");
    assert!(
        badge_row >= 1 && rows[badge_row - 1].trim().is_empty(),
        "one blank row stands the badge off the transcript above — the \
         same air the queue's pill keeps: {rows:?}"
    );
    assert!(
        rows[badge_row].contains("1/2 done"),
        "the fraction rides the badge's row, dim beside the pill: {:?}",
        rows[badge_row]
    );
    let column = u16::try_from(rows[badge_row].find(" TODO ").expect("located above")).unwrap_or(0);
    let row = u16::try_from(badge_row).unwrap_or(0);
    let buffer = terminal.backend().buffer();
    for offset in 0..6u16 {
        assert_eq!(
            buffer[(column + offset, row)].bg,
            app.theme.ui.secondary,
            "the word rides the secondary fill — the queue's pill, another name"
        );
    }
    for letters in 1..5u16 {
        assert_eq!(
            buffer[(column + letters, row)].fg,
            app.theme.ui.background,
            "the canvas color keeps the letters legible on the accent"
        );
    }
    // The badge rides flush with the rows it heads: the pill's left
    // fill edge shares the checkbox column, not a stand-off to the
    // right of it.
    let badge_untrimmed: String = (0..80u16)
        .map(|x| buffer[(x, row)].symbol().to_string())
        .collect();
    let pill_edge = badge_untrimmed.find("TODO").map_or(usize::MAX, |byte| {
        badge_untrimmed[..byte].chars().count().saturating_sub(1)
    });
    // The section's rows — below the badge; the transcript's own todo
    // block renders the same glyphs at a deeper inset above it.
    let task_row = rows
        .iter()
        .skip(badge_row.saturating_add(1))
        .position(|r| r.contains('\u{2610}') || r.contains('\u{25d0}'))
        .map(|offset| badge_row.saturating_add(1) + offset)
        .expect("a task row renders below the badge");
    let task_row = u16::try_from(task_row).unwrap_or(0);
    let task_untrimmed: String = (0..80u16)
        .map(|x| buffer[(x, task_row)].symbol().to_string())
        .collect();
    let icon_col = task_untrimmed
        .find(['\u{2610}', '\u{25d0}'])
        .map_or(usize::MAX, |byte| task_untrimmed[..byte].chars().count());
    assert_eq!(
        pill_edge, icon_col,
        "the TODO pill aligns with the checkbox column — left, not stood off"
    );

    // A deferring theme fills nothing: the badge becomes plain text.
    app.theme = Theme::by_name("transparent").expect("the transparent theme is shipped");
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let badge_row = rows
        .iter()
        .position(|row| row.contains(" TODO "))
        .expect("the badge still names the section");
    let column = u16::try_from(rows[badge_row].find(" TODO ").expect("located above")).unwrap_or(0);
    let row = u16::try_from(badge_row).unwrap_or(0);
    let buffer = terminal.backend().buffer();
    for offset in 0..6u16 {
        assert_eq!(
            buffer[(column + offset, row)].bg,
            ratatui::style::Color::Reset,
            "the badge defers to the terminal's own canvas — no block of paint"
        );
    }
}

/// A slot whose run always takes the offer — the steer path, live.
struct AcceptingGuidance;

impl dch_tui::Guidance for AcceptingGuidance {
    fn steer(&self, _text: String) -> bool {
        true
    }

    fn take_returned(&self) -> Vec<String> {
        Vec::new()
    }
}

#[test]
fn a_mid_run_steer_keeps_the_plan_standing() {
    // The steer's echo rode the same graduation shape as the claim's,
    // so the drain retired the plan on a steer — the section vanished
    // mid-run while the runner's own todo store kept standing, the
    // display out of step with the store. The steer's echo is flagged
    // apart now: the plan keeps standing through the steer exactly as
    // the store does, and only the claim's echo — a real run boundary —
    // retires it.
    let mut app = app();
    // The chord goes through the live slot — the delivery seam itself,
    // not a hand-pushed graduation standing in for it.
    app.set_guidance(Some(Arc::new(AcceptingGuidance)));
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"the only task","status":"pending"}
        ]}"#,
    );
    for character in "steered into the run".chars() {
        app.handle_event(&plain(KeyCode::Char(character)));
    }
    assert!(
        app.handle_event(&key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        "the chord is consumed whether or not a run takes the text"
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("0/1 done").count(),
        1,
        "a steer is not a run boundary — the plan keeps standing in the \
         section while the run it rode into continues, the transcript's \
         block folded at the steer's row (expand it to read the list \
         again): {flat:?}"
    );
    assert!(
        flat.contains("steered into the run"),
        "the steer's echo still lands its transcript row: {flat:?}"
    );

    // The claim's echo is the run boundary — but the plan is the
    // session's now: open work stands across it, carried like the
    // store, and only a finished plan leaves at a claim.
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Sent {
            text: "the next prompt".to_string(),
            steered: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("0/1 done").count(),
        1,
        "the claim's echo retires nothing — the open plan carries into \
         the next run, section standing: {flat:?}"
    );

    // Finish the work, and the very next claim retires the plan: the
    // section goes and the block folds; the list lives behind its
    // expand gesture.
    todo_graduation(
        &app,
        "call-2",
        r#"{"todos":[
            {"id":"1","subject":"the only task","status":"completed"}
        ]}"#,
    );
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Sent {
            text: "the prompt after".to_string(),
            steered: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("done").count(),
        0,
        "a finished plan leaves at the claim that follows it — the \
         section never ghosts done work across runs: {flat:?}"
    );
}

#[test]
fn an_interrupted_run_s_plan_survives_the_next_claim() {
    // The cancelled-and-resumed shape: a run lands cancelled with its
    // plan half-done, the user prompts again to continue the work —
    // and the plan must stand, because it is the map of the work the
    // session still owes. Only a clean run's plan retires at the next
    // claim; an interrupted run's plan stands until the resumed run
    // files its own (which it will — the model reads the transcript's
    // TodoWrite blocks and the system prompt says maintain the list).
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"the finished half","status":"completed"},
            {"id":"2","subject":"the interrupted half","status":"in_progress"}
        ]}"#,
    );
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::RunEnded {
            duration_ms: 4000,
            success: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        2,
        "the interrupted plan stays up after the cancelled run lands: {flat:?}"
    );

    // The resume prompt claims — and the plan survives the claim.
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Sent {
            text: "resume".to_string(),
            steered: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        1,
        "the claim after an interrupted run retires nothing — the plan \
         is the map of the work the session owes, standing in the \
         section, its transcript block folded at the claim's row: \
         {flat:?}"
    );

    // The resumed run ends cleanly with the plan still half-done —
    // and the claim after it retires nothing: the plan is the
    // session's, its open work carries, and the section keeps naming
    // what is owed until the work actually finishes.
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::RunEnded {
            duration_ms: 2000,
            success: true,
        });
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Sent {
            text: "the prompt after".to_string(),
            steered: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        1,
        "a clean close retires only finished work — the half-done plan \
         carries past its claim, section standing: {flat:?}"
    );

    // Finish the owed half, and the next claim retires the plan — the
    // ordinary rule, at its new edge.
    todo_graduation(
        &app,
        "call-2",
        r#"{"todos":[
            {"id":"1","subject":"the finished half","status":"completed"},
            {"id":"2","subject":"the interrupted half","status":"completed"}
        ]}"#,
    );
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Sent {
            text: "the last prompt".to_string(),
            steered: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("done").count(),
        0,
        "the finished plan leaves at the claim that follows it — block \
         folded and section gone: {flat:?}"
    );
}

#[test]
fn a_plan_stands_past_claims_until_its_work_is_finished() {
    // The plan stays up after its run lands — the final state, done or
    // not, is the thing to read in the quiet after the run — and it
    // stands across the claims that follow while work remains, carried
    // exactly as the runner carries its own store; a claim retires it
    // only once every entry stands completed, the same edge the runner
    // clears the carried store at. The transcript keeps every update in
    // full; only the pinned window goes. Steering delivers mid-run
    // without claiming, so a plan keeps updating through its own run.
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"finished thing","status":"completed"},
            {"id":"2","subject":"half-done thing","status":"in_progress"}
        ]}"#,
    );
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::RunEnded {
            duration_ms: 800,
            success: true,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        2,
        "the landing leaves the plan standing — the final state is the \
         thing to read in the quiet after the run, block and section \
         both, the fold waiting for the conversation's next step: \
         {flat:?}"
    );

    // The next claim retires nothing: the plan is the session's and
    // its open work carries into the next run, the section standing
    // over it so the reader never loses the map.
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Sent {
            text: "the next prompt".to_string(),
            steered: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("1/2 done").count(),
        1,
        "the claim carries the half-done plan — the section stands, the \
         transcript's block folded at the claim's row: {flat:?}"
    );

    // The work finishes, and the claim after that retires the plan —
    // the section goes, the block folded; the list lives behind its
    // expand key.
    todo_graduation(
        &app,
        "call-2",
        r#"{"todos":[
            {"id":"1","subject":"finished thing","status":"completed"},
            {"id":"2","subject":"half-done thing","status":"completed"}
        ]}"#,
    );
    app.graduations()
        .lock()
        .unwrap()
        .push(dch_tui::observer::Graduation::Sent {
            text: "the prompt after".to_string(),
            steered: false,
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let flat: String = row_texts(&terminal).join("\n");
    assert_eq!(
        flat.matches("done").count(),
        0,
        "a finished plan leaves at the claim that follows it: {flat:?}"
    );
}

#[test]
fn the_readout_leads_the_footer_and_the_session_id_centers_it() {
    // The footer's order: the context pair is its head — the bar at
    // the left edge of its row, standing exactly over the numbers
    // that lead the bottom row — and the session id rides that same
    // bar row, right-aligned: the footer's middle row, vertically
    // centered between the air above and the numbers below. A frame
    // too narrow for the id to clear the bar forgoes it rather than
    // truncating it, and the directory the bar once carried rides
    // the place row above the composer instead.
    let mut app = app();
    app.set_working_dir(std::path::Path::new("/home/tester/repos/dch"));
    app.set_git_branch(Some("master".to_string()));
    app.set_session_id("f005d097-c4ce-477a-88c0-2fc2fc000000".to_string());
    let rows = row_texts(&render_to_buffer(&mut app, 60, 30));
    let numbers_row = rows
        .iter()
        .position(|row| row.contains("0k/200k"))
        .expect("the numbers render");
    assert_eq!(
        numbers_row,
        rows.len() - 1,
        "the numbers stand on the bottom row: {rows:?}"
    );
    assert!(
        rows[numbers_row].trim_start().starts_with("0k/200k (0%)"),
        "the readout leads from the bar's left edge: {:?}",
        rows[numbers_row]
    );
    let bar_row = numbers_row - 1;
    let columns = rows[bar_row].chars().collect::<Vec<_>>();
    assert!(
        rows[bar_row]
            .trim_end()
            .ends_with("f005d097-c4ce-477a-88c0-2fc2fc000000"),
        "the id closes the bar's row at the right edge, centered in the footer: {:?}",
        rows[bar_row]
    );
    assert_eq!(
        columns.last(),
        Some(&' '),
        "the id stands a cell of air off the frame's right edge: {:?}",
        rows[bar_row]
    );
    assert!(
        !rows[bar_row].contains("dch (master)") && !rows[numbers_row].contains("dch (master)"),
        "the directory moved off the footer onto the place row: {rows:?}"
    );
    let place_row = rows
        .iter()
        .position(|row| row.contains("dch (master)"))
        .expect("the place row renders");
    assert!(
        place_row < bar_row - 1,
        "the place row rides above the composer, clear of the footer: {rows:?}"
    );

    // Narrower than the bar plus the id, the id steps aside whole —
    // never truncated, never run into the bar — and the readout
    // keeps its claim on the left edge.
    let narrow = row_texts(&render_to_buffer(&mut app, 20, 30));
    let narrow_numbers = narrow
        .iter()
        .position(|row| row.contains("0k/200k"))
        .expect("the numbers still render");
    assert_eq!(
        narrow_numbers,
        narrow.len() - 1,
        "the numbers still stand on the bottom row: {narrow:?}"
    );
    assert!(
        narrow[narrow_numbers].trim_start().starts_with("0k/200k"),
        "the readout keeps the left edge at any width: {:?}",
        narrow[narrow_numbers]
    );
    assert!(
        !narrow.iter().any(|row| row.contains("f005d097")),
        "an id without the room is forgone, not clipped into the bar: {narrow:?}"
    );
}

#[test]
fn autumn_night_keeps_its_track_a_visible_step_off_the_bar() {
    // The theme's track came upstream at (14,14,14) against a (9,9,9)
    // bar — five units apart, invisible: the context bar's empty half
    // read as nothing, and a usage bar whose unfilled half reports
    // nothing is not a bar. The lifted neutral must stay a perceptible
    // step — every channel at least sixteen off the bar's background —
    // and the empty track must actually paint on the bar's row.
    let mut app = TuiApp::new(config_with_theme("autumn_night"));
    let (track, bar_bg) = (app.theme.ui.scrollbar_track, app.theme.ui.status_bar_bg);
    let (track_rgb, bar_rgb) = (rgb_of(track), rgb_of(bar_bg));
    let delta = std::iter::zip(track_rgb, bar_rgb)
        .map(|(t, b)| t.abs_diff(b))
        .max();
    assert!(
        delta.is_some_and(|delta| delta >= 16),
        "the track stands a visible step off the bar's background: \
         {track:?} vs {bar_bg:?}"
    );
    let terminal = render_to_buffer(&mut app, 100, 30);
    let buffer = terminal.backend().buffer();
    let bar_row = 28u16;
    let track_cells = (0..100u16)
        .filter(|&x| buffer[(x, bar_row)].bg == track)
        .count();
    assert!(
        track_cells >= 16,
        "the empty track paints on the bar's row — the unfilled half \
         reports the bar's shape"
    );
}

/// An `Rgb` color as its channels, or a failure naming the shape that
/// broke the contrast pin.
fn rgb_of(color: ratatui::style::Color) -> [u8; 3] {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => [r, g, b],
        other => panic!("expected an Rgb track, got {other:?}"),
    }
}

#[test]
fn a_scrolled_view_never_severs_a_todo_block() {
    // The plan badge keeps the fraction on screen at every scroll, so
    // a transcript window that cuts an expanded todo block mid-list
    // shows a count and a list that disagree — the badge says twelve,
    // the frame carries eleven rows, and nothing marks the cut. The
    // scrolled view moves a fitting block's cut up to the block's own
    // head instead: the block renders whole or not at all.
    let mut app = app();
    let items: Vec<String> = (1..=12)
        .map(|n| format!(r#"{{"id":"t{n}","subject":"item {n} of the plan","status":"pending"}}"#))
        .collect();
    todo_graduation(
        &app,
        "call-1",
        &format!(r#"{{"todos":[{}]}}"#, items.join(",")),
    );
    // Long replies below the block, so the block sits well above the
    // bottom and every scroll step crosses it. They also fold its
    // auto-expansion — the contract elsewhere — so the walk below
    // tests the reader's own expansion: a click on the summary row
    // reopens the block, and a manual expansion is never auto-folded.
    for turn in 0..6 {
        app.graduations()
            .lock()
            .unwrap()
            .push(dch_tui::observer::Graduation::Reply {
                text: format!("reply {turn}: {}", "settled prose fills the width. ").repeat(8),
                thinking: None,
            });
    }
    // The replies pushed the folded block out of the bottom-anchored
    // frame — walk up until its summary row is on screen, then open
    // it with the reader's click.
    let mut block_row = None;
    for _ in 0..60 {
        app.handle_event(&plain(KeyCode::Up));
        let folded = render_to_buffer(&mut app, 80, 30);
        let folded_rows = row_texts(&folded);
        if let Some(row) = folded_rows
            .iter()
            .position(|row| row.contains("Updating todo list"))
        {
            block_row = Some(row);
            break;
        }
    }
    let block_row = block_row.expect("the folded summary row reaches the frame");
    app.handle_event(&mouse_event(
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        5,
        u16::try_from(block_row).unwrap_or(0),
    ));
    app.handle_event(&mouse_event(
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        5,
        u16::try_from(block_row).unwrap_or(0),
    ));
    // The walk below starts at the bottom — the hunt above left the
    // view scrolled up, so land it home before walking.
    app.handle_event(&plain(KeyCode::End));
    // Fold the plan section to its badge — the fraction the reader
    // reconciles the rows against stays on screen either way.
    app.handle_event(&key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    app.handle_event(&key(KeyCode::Char('t'), KeyModifiers::CONTROL));

    let fragments: Vec<String> = (1..=12).map(|n| format!("item {n} of the plan")).collect();
    let mut saw_whole = false;
    let mut saw_none = false;
    for step in 0..24u32 {
        let terminal = render_to_buffer(&mut app, 80, 30);
        let flat: String = row_texts(&terminal).join("\n");
        let visible = fragments
            .iter()
            .filter(|fragment| flat.contains(fragment.as_str()))
            .count();
        assert!(
            visible == 0 || visible == fragments.len(),
            "scroll step {step}: the todo block must render whole or not at all, \
             saw {visible} of {} rows in frame:\n{flat}",
            fragments.len()
        );
        saw_whole |= visible == fragments.len();
        saw_none |= visible == 0;
        app.handle_event(&plain(KeyCode::Up));
    }
    assert!(
        saw_whole && saw_none,
        "the walk must cross the block both ways for the pin to bite"
    );
}

#[test]
fn completed_rows_retire_uniformly_with_an_unstruck_box() {
    // Finished work reads as retired the moment it finishes: a row
    // completed in this update renders exactly like every older
    // completed row — no change-accent lingering on the words — and
    // the strike-through belongs to the subject alone, never the
    // checkbox, whose plain checked box stays legible as the row's
    // status at a glance.
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"the earlier work","status":"completed"},
            {"id":"2","subject":"the last stretch","status":"pending"}
        ]}"#,
    );
    todo_graduation(
        &app,
        "call-2",
        r#"{"todos":[
            {"id":"1","subject":"the earlier work","status":"completed"},
            {"id":"2","subject":"the last stretch","status":"completed"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let buffer = terminal.backend().buffer();
    let dim = app.theme.ui.dim;
    let accent = app.theme.ui.primary;
    for subject in ["the earlier work", "the last stretch"] {
        let row = rows
            .iter()
            .position(|row| row.contains('\u{2611}') && row.contains(subject))
            .unwrap_or_else(|| panic!("the completed row for {subject} renders"));
        let row = u16::try_from(row).unwrap_or(0);
        // The untrimmed row, cell by cell: the icon's column is where
        // the buffer actually holds it, inset and all.
        let untrimmed: String = (0..80u16)
            .map(|x| buffer[(x, row)].symbol().to_string())
            .collect();
        let icon_column = untrimmed.find('\u{2611}').map_or(0, |chars| {
            u16::try_from(untrimmed[..chars].chars().count()).unwrap_or(0)
        });
        let icon_cell = &buffer[(icon_column, row)];
        assert_eq!(
            icon_cell.fg, dim,
            "the checked box sits in the dim register: {subject}"
        );
        assert!(
            !icon_cell
                .modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT),
            "the checkbox itself is never struck through: {subject}"
        );
        let subject_cell = &buffer[(icon_column.saturating_add(2), row)];
        assert_eq!(
            subject_cell.fg, dim,
            "a completed row carries no accent, fresh or old: {subject}"
        );
        assert!(
            subject_cell
                .modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT),
            "the subject's words retire under the strike: {subject}"
        );
        assert!(
            subject_cell.fg != accent,
            "finishing is not the thing happening now: {subject}"
        );
    }
}

#[test]
fn the_capped_plan_badge_names_the_chord_that_opens_it() {
    // The capped posture is the section most sessions live in, and
    // its badge row is the one place every posture reads: a plan that
    // fits the cap shows no ellipsis, so without the chord on the
    // badge the reader has no way to learn the list opens. The full
    // view needs no such nudge — it is the open list.
    let mut app = app();
    todo_graduation(
        &app,
        "call-1",
        r#"{"todos":[
            {"id":"1","subject":"one thing","status":"pending"},
            {"id":"2","subject":"two thing","status":"pending"}
        ]}"#,
    );
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let badge_row = rows
        .iter()
        .find(|row| row.contains(" TODO "))
        .expect("the badge names the section");
    assert!(
        badge_row.contains("0/2 done"),
        "the fraction rides the badge: {badge_row:?}"
    );
    assert!(
        badge_row.contains("ctrl+t"),
        "the capped badge names the chord that opens the whole list, \
         even when the cap cut nothing: {badge_row:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("more (ctrl+t")),
        "a plan that fits the cap shows no ellipsis: {rows:?}"
    );

    app.handle_event(&key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let badge_row = rows
        .iter()
        .find(|row| row.contains(" TODO "))
        .expect("the badge still names the section");
    assert!(
        !badge_row.contains("ctrl+t"),
        "the open list is its own answer — the full view carries no \
         chord nudge: {badge_row:?}"
    );
}

/// A state-split app for the thinking-lane pins.
///
/// The tests below drive the observer directly — deltas and responses —
/// and render through the real frame path, so the lane's capture,
/// graduation, and interaction all exercise the shipped wiring.
fn thinking_app() -> (dch_tui::TuiObserver, TuiApp) {
    let (observer, state) = dch_tui::TuiObserverState::new().into_observer();
    (
        observer,
        TuiApp::from_observer_state(config_with_theme("dracula"), state),
    )
}

/// A thinking app whose agent run is live.
///
/// Live reasoning renders only while the run does — the flag this
/// helper sets before the app takes the state.
fn live_thinking_app() -> (dch_tui::TuiObserver, TuiApp) {
    let (observer, state) = dch_tui::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    (
        observer,
        TuiApp::from_observer_state(config_with_theme("dracula"), state),
    )
}

fn thinking_delta(turn: usize, delta: &str) -> loopctl::observer::ThinkingDeltaContext {
    loopctl::observer::ThinkingDeltaContext {
        turn,
        delta: delta.to_string(),
    }
}

fn responded(turn: usize, text: &str) -> loopctl::observer::ResponseContext {
    loopctl::observer::ResponseContext {
        turn,
        text: text.to_string(),
        usage: None,
    }
}

fn click(app: &mut TuiApp, column: u16, row: u16) {
    app.handle_event(&mouse_event(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column,
        row,
    ));
    app.handle_event(&mouse_event(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        column,
        row,
    ));
}

#[test]
fn both_chords_cancel_the_same_way_and_only_ctrl_c_exits() {
    // The two ladders share the cancel — the same canceller, the same
    // record — and differ only in what the fire carries: Esc-Esc's
    // fire stops the run and the session stands; Ctrl+C Ctrl+C's
    // fire stops the run and terminates. Both arm before they fire,
    // and a stray key between either ladder's presses stands it down.
    let (mut app, kept) = app_with_state();
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    app.set_run_canceller(Box::new(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }));
    app.handle_event(&plain(KeyCode::Esc));
    assert!(
        !cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "one Esc alone cancels nothing"
    );
    app.handle_event(&plain(KeyCode::Char('x')));
    app.handle_event(&plain(KeyCode::Esc));
    assert!(
        !cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "a stray key between the presses stands the cancel down"
    );
    app.handle_event(&plain(KeyCode::Esc));
    app.handle_event(&plain(KeyCode::Esc));
    assert!(
        cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "two Esc presses with nothing between fire the canceller"
    );
    assert!(!app.is_quitting(), "a cancel is not an exit");

    let (mut app, kept) = app_with_state();
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    app.set_run_canceller(Box::new(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }));
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        !cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "the first press only arms"
    );
    assert!(!app.is_quitting(), "the arming press leaves nothing");
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "the fire is the same cancel Esc-Esc fires"
    );
    assert!(app.is_quitting(), "and the same fire terminates");
}
#[test]
fn a_cleared_draft_with_no_run_names_the_exit_ladder() {
    // With nothing running, the clearing press is still the ladder's
    // first rung: the frame says the exit is two presses away rather
    // than leaving the cleared draft to guess.
    let mut app = app();
    for character in "words that will not be sent".chars() {
        app.handle_event(&plain(KeyCode::Char(character)));
    }
    app.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.input().is_empty(), "the draft clears");
    assert!(!app.is_quitting(), "clearing arms nothing");
    let cleared = render_to_buffer(&mut app, 80, 24);
    assert!(
        row_texts(&cleared)
            .iter()
            .any(|row| row.contains("press ctrl+c twice to quit")),
        "the cleared draft names the exit it did not arm: {:?}",
        row_texts(&cleared)
    );
}

#[test]
fn the_running_elapsed_line_opens_with_a_spinning_frame() {
    // The live line says two things at once: that something is
    // happening (the spinning dot, advancing with the elapsed clock
    // itself) and how long it has been happening (the duration, the
    // completed line's own format). The glyph leads, the number
    // follows, and both leave together when the run lands.
    let (mut app, kept) = app_with_state();
    kept.mark_run_started();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock reads")
        .as_millis();
    let started = u64::try_from(started).unwrap_or(u64::MAX);
    kept.run_started_ms.store(
        started.saturating_sub(65_000),
        std::sync::atomic::Ordering::SeqCst,
    );
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let timer_row = rows
        .iter()
        .find(|row| row.contains("1m 5s"))
        .expect("the elapsed line renders");
    let first = timer_row
        .trim_start()
        .chars()
        .next()
        .expect("a glyph leads");
    assert!(
        ('\u{2800}'..='\u{28ff}').contains(&first),
        "a braille spinner frame leads the duration — the prototype's \
         own orbit: {timer_row:?}"
    );
    kept.mark_run_ended();
}

#[test]
fn the_running_turn_carries_its_elapsed_time() {
    // The completed reply prints how long it took as a dim line at
    // its foot; the running turn owes the same line while it runs —
    // the total so far, in the completed line's own format, ticking
    // at the frame cadence — so the question "how long has this been
    // going" never waits for the end to be answered.
    let (mut app, kept) = app_with_state();
    kept.mark_run_started();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock reads")
        .as_millis();
    let started = u64::try_from(started).unwrap_or(u64::MAX);
    kept.run_started_ms.store(
        started.saturating_sub(65_000),
        std::sync::atomic::Ordering::SeqCst,
    );
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("1m 5s")),
        "the live elapsed line rides the running turn's tail: {rows:?}"
    );

    // The line leaves with the run: nothing ticks between turns.
    kept.mark_run_ended();
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        !rows.iter().any(|row| row.trim() == "1m 5s"),
        "no elapsed line stands over an idle composer: {rows:?}"
    );
}

#[test]
fn cancel_run_in_flight_fires_only_under_a_run() {
    // The exit handshake's own half: the method fires the canceller
    // exactly when a run is in flight and is a no-op at rest — an
    // exit with nothing running arms nothing and cancels nothing.
    let (mut app, kept) = app_with_state();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    app.set_run_canceller(Box::new(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }));
    assert!(
        !app.cancel_run_in_flight(),
        "at rest there is nothing to stop"
    );
    assert!(
        !cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "the idle call fires nothing"
    );
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        app.cancel_run_in_flight(),
        "under a run the handshake fires"
    );
    assert!(
        cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "the canceller ran"
    );
}

#[test]
fn a_resume_requeues_the_bare_unanswered_claims_standing_at_the_end() {
    // The queue a resume opens with is the truth, not a phantom: a
    // transcript ending on bare claims — the run in flight's prompt,
    // caught before it could record anything, and the waiting ones the
    // exit wrote there because no turn ever spent them — each waits
    // again in the section, in the order they were typed. They wait
    // for the user: the section shows them at rest, `e` fixes a
    // wording, `d` takes one out, `s` aims one at a later run — none of
    // them runs on its own. A claim followed by its cancellation
    // record is a deliberate stop and owes nothing, and an answered
    // claim is done. Resurrecting cancelled prompts would put words in
    // the queue the user explicitly took back.
    let now = chrono::Utc::now();

    // The bare claim: owed, requeued.
    let (mut app, kept) = app_with_state();
    app.seed_messages(vec![TuiMessage::User {
        text: "the interrupted run's prompt".to_string(),
        timestamp: now,
        steered: false,
    }]);
    let queued: Vec<String> = {
        let queue = kept
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (0..queue.len())
            .filter_map(|index| queue.get(index))
            .map(|entry| entry.text.clone())
            .collect()
    };
    assert_eq!(
        queued,
        ["the interrupted run's prompt".to_string()],
        "the bare unanswered claim waits again"
    );

    // A run of bare claims: every one owed, in the order they were
    // typed — the exit wrote the waiting backlog behind the record of
    // the run it stopped, and the resume carries all of it.
    let (mut app, kept) = app_with_state();
    app.seed_messages(vec![
        TuiMessage::User {
            text: "stopped mid-run".to_string(),
            timestamp: now,
            steered: false,
        },
        TuiMessage::Error {
            text: "Agent cancelled".to_string(),
            timestamp: now,
        },
        TuiMessage::User {
            text: "first waiting word".to_string(),
            timestamp: now,
            steered: false,
        },
        TuiMessage::User {
            text: "second waiting word".to_string(),
            timestamp: now,
            steered: false,
        },
    ]);
    let queued: Vec<String> = {
        let queue = kept
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (0..queue.len())
            .filter_map(|index| queue.get(index))
            .map(|entry| entry.text.clone())
            .collect()
    };
    assert_eq!(
        queued,
        [
            "first waiting word".to_string(),
            "second waiting word".to_string()
        ],
        "the trailing run waits whole — the recorded cancel above it stays \
         stopped, the unspent words behind it wait in their typed order"
    );

    // The cancelled claim: deliberate, owes nothing.
    let (mut app, kept) = app_with_state();
    app.seed_messages(vec![
        TuiMessage::User {
            text: "stopped mid-run".to_string(),
            timestamp: now,
            steered: false,
        },
        TuiMessage::Error {
            text: "Agent cancelled".to_string(),
            timestamp: now,
        },
    ]);
    let queue = kept
        .queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        queue.is_empty(),
        "a cancelled prompt is not resurrected into the queue"
    );

    // Answered claims owe nothing.
    let (mut app, kept) = app_with_state();
    app.seed_messages(vec![
        TuiMessage::User {
            text: "answered".to_string(),
            timestamp: now,
            steered: false,
        },
        TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "here is the answer".to_string(),
            }],
            timestamp: now,
            duration_ms: None,
        },
    ]);
    let queue = kept
        .queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(queue.is_empty(), "an answered claim owes nothing");

    // A steered echo was never a claim.
    let (mut app, kept) = app_with_state();
    app.seed_messages(vec![
        TuiMessage::Assistant {
            blocks: vec![],
            timestamp: now,
            duration_ms: None,
        },
        TuiMessage::User {
            text: "a steer's echo".to_string(),
            timestamp: now,
            steered: true,
        },
    ]);
    let queue = kept
        .queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(queue.is_empty(), "a steered echo claims nothing");
}

#[test]
fn the_exit_writes_waiting_prompts_into_the_transcript_as_unclaimed_rows() {
    // The queue dies with the process, so words waiting in it need a
    // row to survive: the exit reads the unclaimed entries inside the
    // window between the shutdown flag rising and the cancel firing —
    // the run in flight's entry is already out, the requeues have not
    // landed — and writes them, in queue order, as the rows a resume
    // seeds the waiting section from. Nothing waiting writes nothing:
    // the exit owes no row to a queue that held none.
    let (mut app, kept) = app_with_state();
    {
        let mut queue = kept
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.push("first waiting".to_string());
        queue.push("second waiting".to_string());
    }
    let texts = app.waiting_prompt_texts_at_exit();
    assert_eq!(
        texts,
        ["first waiting".to_string(), "second waiting".to_string()],
        "the snapshot is the unclaimed set, in the order they were typed"
    );
    let before = app.conversation().len();
    app.echo_waiting_prompts_at_exit(texts);
    let conversation = app.conversation();
    let echoed: Vec<&str> = conversation
        .get(before..)
        .unwrap_or_default()
        .iter()
        .filter_map(|message| match message {
            TuiMessage::User {
                text,
                steered: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        echoed,
        ["first waiting", "second waiting"],
        "each waiting prompt becomes an unclaimed user row, in queue order"
    );

    let (mut app, kept) = app_with_state();
    {
        let queue = kept
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(queue.is_empty(), "an idle exit with nothing waiting");
    }
    let before = app.conversation().len();
    app.echo_waiting_prompts_at_exit(Vec::new());
    assert_eq!(
        app.conversation().len(),
        before,
        "an empty queue leaves no rows — there were no words to owe"
    );
}

#[test]
fn the_exit_echoes_rescued_corrections_as_unclaimed_rows() {
    // The drain half of the exit's correction rescue: the driver
    // collects each correction still buffered at its exit breaks —
    // words that travelled only in the channel — and the exit writes
    // them back as unclaimed rows, in collection order, so the
    // transcript ends on everything the user typed that no turn
    // spent and a resume seeds each row into the waiting section. An
    // empty rescue writes nothing.
    let (mut app, kept) = app_with_state();
    {
        let mut rescued = kept
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rescued.push("the newest wording".to_string());
        rescued.push("a different prompt's wording".to_string());
    }
    let before = app.conversation().len();
    app.echo_rescued_corrections_at_exit();
    let conversation = app.conversation();
    let echoed: Vec<&str> = conversation
        .get(before..)
        .unwrap_or_default()
        .iter()
        .filter_map(|message| match message {
            TuiMessage::User {
                text,
                steered: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        echoed,
        ["the newest wording", "a different prompt's wording"],
        "each rescued correction becomes an unclaimed user row, in \
         collection order"
    );
    assert!(
        kept.rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "the drain takes the buffer whole — a second drain writes nothing"
    );
    let before = app.conversation().len();
    app.echo_rescued_corrections_at_exit();
    assert_eq!(
        app.conversation().len(),
        before,
        "an empty rescue leaves no rows"
    );
}

#[test]
fn the_timer_spinner_advances_with_the_elapsed_clock() {
    // The dot's frame is a function of run time alone: 80ms per
    // frame, the prototype's ten-cell orbit in order, sampled by
    // every redraw. A quiet tool lane cannot freeze it, and it never
    // desyncs from the ticking duration it leads.
    let (mut app, kept) = app_with_state();
    kept.mark_run_started();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock reads")
        .as_millis();
    let now = u64::try_from(now).unwrap_or(u64::MAX);
    // The elapsed values sit 20ms under a frame boundary, and the
    // render reads its own clock — always at least as late as the one
    // read here, since the frame work happens after the store. So the
    // cell shown is the one this elapsed maps to, or the next one
    // forward if the frame was slow; it is never the cell behind. The
    // literals below pin the mapping itself, which is what the orbit's
    // order is for.
    for (elapsed, expected) in [
        (60u64, "\u{2819}"),
        (100, "\u{2839}"),
        (180, "\u{283c}"),
        (260, "\u{2826}"),
        (380, "\u{280f}"),
    ] {
        let at = spinner_frame_at(std::time::Duration::from_millis(elapsed));
        assert_eq!(
            at, expected,
            "the elapsed-to-cell mapping the golden pins: {elapsed}ms"
        );
        let forward = spinner_frame_at(std::time::Duration::from_millis(
            elapsed.saturating_add(SPINNER_FRAME_MS),
        ));
        kept.run_started_ms.store(
            now.saturating_sub(elapsed),
            std::sync::atomic::Ordering::SeqCst,
        );
        let terminal = render_to_buffer(&mut app, 80, 24);
        let rows = row_texts(&terminal);
        let timer_row = rows
            .iter()
            .find(|row| {
                let first = row.trim_start().chars().next();
                first.is_some_and(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
            })
            .unwrap_or_else(|| panic!("the timer row renders at {elapsed}ms"));
        let first = timer_row.trim_start().chars().next().expect("a glyph");
        let shown = first.to_string();
        assert!(
            shown == expected || shown == forward,
            "at {elapsed}ms elapsed the orbit shows {expected:?} — or \
             {forward:?}, the next cell, since the render's clock only \
             runs ahead of this one: {timer_row:?}"
        );
    }
}

#[test]
fn the_timer_row_names_its_count_with_a_quiet_elapsed_word() {
    // The lane's other live rows carry their word — Thinking,
    // Compacting — and the bare figure beside them read as a stray
    // number. The word sits between the orbit and the figure; when
    // the run lands, the settled footer names the same figure as a
    // `duration` — the total is final, and a record's row says what
    // it holds. A breathing row of margin stands above the ticking
    // line, so it never touches the transcript it closes.
    let (mut app, kept) = app_with_state();
    app.seed_messages(vec![TuiMessage::User {
        text: "the turn above the timer".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    }]);
    kept.mark_run_started();
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let timer_at = rows
        .iter()
        .position(|row| {
            row.trim_start()
                .starts_with(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
        })
        .expect("the timer row renders");
    assert!(
        rows.get(timer_at.wrapping_sub(1))
            .is_some_and(|row| row.trim().is_empty()),
        "a breathing row of margin stands between the transcript and the ticking \
         figure: {rows:?}"
    );
    let timer_row = rows
        .get(timer_at)
        .expect("the position the finder just named");
    let after_orbit: String = timer_row
        .trim_start()
        .chars()
        .skip(1)
        .skip_while(|c| c.is_whitespace())
        .collect();
    assert!(
        after_orbit.starts_with("elapsed "),
        "the word names the figure, right after the orbit: {timer_row:?}"
    );

    let (mut app, _) = app_with_state();
    let now = chrono::Utc::now();
    app.seed_messages(vec![
        TuiMessage::User {
            text: "answered".to_string(),
            timestamp: now,
            steered: false,
        },
        TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "here is the answer".to_string(),
            }],
            timestamp: now,
            duration_ms: Some(84_000),
        },
    ]);
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    assert!(
        rows.iter().any(|row| row.trim() == "duration 1m 24s"),
        "the settled footer names its total — the duration word beside the figure: {rows:?}"
    );
}

#[test]
fn the_running_tool_stands_above_the_turn_s_timer_footer() {
    // The running turn reads in content order — the in-flight tool
    // rows, then the elapsed footer closing the turn exactly where
    // the completed reply's own duration line lands — and the tool
    // sits one breathing row under the content above it, no further.
    let (mut app, kept) = app_with_state();
    kept.mark_run_started();
    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(dch_tui::ActiveTool {
            call_id: "call-9".to_string(),
            name: "Bash".to_string(),
            input_summary: "cargo build".to_string(),
            start: std::time::Instant::now(),
        });
    let terminal = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&terminal);
    let tool_row = rows
        .iter()
        .position(|row| row.contains("Running: cargo build"))
        .expect("the running tool renders");
    let timer_row = rows
        .iter()
        .position(|row| {
            row.trim_start()
                .starts_with(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
        })
        .expect("the timer renders");
    assert!(
        tool_row < timer_row,
        "the tool stands above the timer footer: {rows:?}"
    );
    assert_eq!(
        timer_row,
        tool_row.saturating_add(2),
        "the timer closes the turn one breathing row beneath the tool"
    );
}

#[test]
fn the_notice_row_aligns_right_above_the_composer() {
    // The row is glance-worthy furniture, not transcript register:
    // its message closes at the frame's right edge, where the status
    // bar's own identity sits — never flush left under the composer's
    // edge column, where it reads as conversation continuing.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept);
    app.set_run_canceller(Box::new(|| {}));
    app.handle_event(&plain(KeyCode::Esc));
    app.handle_event(&plain(KeyCode::Esc));
    let terminal = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&terminal);
    let notice_row = rows
        .iter()
        .position(|row| row.contains("Agent cancelled"))
        .expect("the cancel's record shows on the notice row");
    let untrimmed: String = (0..80u16)
        .map(|x| {
            terminal.backend().buffer()[(x, u16::try_from(notice_row).unwrap())]
                .symbol()
                .to_string()
        })
        .collect();
    let first_text = untrimmed
        .trim_start()
        .chars()
        .next()
        .expect("the row carries text");
    assert!(
        first_text == 'A',
        "the message closes at the right edge — nothing trails it: {untrimmed:?}"
    );
    let leading = untrimmed
        .find('A')
        .map_or(0, |byte| untrimmed[..byte].chars().count());
    assert!(
        leading > 40,
        "the message sits in the row's right half, not flush left: {untrimmed:?}"
    );
}

#[test]
fn the_exit_record_lands_below_the_captured_work() {
    // The transcript reads in the order the run happened: the
    // prompt, the work that stood on screen when the fire landed
    // (the thinking the exit captures), and the cancel's record
    // closing the turn beneath it — never the record floating above
    // work that preceded it.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());
    app.set_run_canceller(Box::new(|| {}));
    // The run streamed thinking that never graduated — the same
    // writes the observer's thinking-delta hook makes.
    {
        let mut stream = kept
            .thinking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stream.text.push_str("partial reasoning that was on screen");
        stream.began = Some(std::time::Instant::now());
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    app.settle_exit_stop(&mut terminal);
    let conversation = app.conversation();
    let thinking_row = conversation
        .iter()
        .position(|message| {
            matches!(message, TuiMessage::Assistant { blocks, .. } if blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Thinking { text, .. } if text.contains("partial reasoning"))))
        })
        .expect("the captured thinking row landed");
    let record_row = conversation
        .iter()
        .position(|message| matches!(message, TuiMessage::Error { text, .. } if text == "Agent cancelled"))
        .expect("the record row landed");
    assert!(
        thinking_row < record_row,
        "the work precedes the record — the run's own order: {conversation:?}"
    );
}

#[test]
fn settle_exit_stop_at_rest_appends_nothing() {
    // With nothing running the exit handshake is a no-op: no captured
    // lanes, no record row, no flush — at rest the turn-end hook has
    // already fired from the live frames, and the exit owns nothing.
    let (mut app, kept) = app_with_state();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    let before = app.conversation().len();
    app.settle_exit_stop(&mut terminal);
    assert_eq!(
        app.conversation().len(),
        before,
        "nothing lands over an idle session"
    );
    drop(kept);
}

#[test]
fn settle_exit_stop_does_not_duplicate_an_instant_settles_record() {
    // A run whose cancel resolved before the exit got there posts
    // its own record row — the drain carries it, and the exit's
    // deterministic write stays silent: one row, never two.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());
    app.set_run_canceller(Box::new(|| {}));
    // The driver's settle already posted its row into the lane.
    kept.errors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(loopctl::error::LoopError::Cancelled.to_string());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    app.settle_exit_stop(&mut terminal);
    let records = app
        .conversation()
        .iter()
        .filter(|message| matches!(message, TuiMessage::Error { text, .. } if text == "Agent cancelled"))
        .count();
    assert_eq!(
        records, 1,
        "the settled run's own record lands — the exit adds no twin"
    );
}

#[test]
fn settle_exit_stop_captures_the_partial_reply_text_beside_the_thinking() {
    // Both unfinished lanes ride together in one assistant row — the
    // thinking and whatever reply text had begun — so a resumed
    // session reads the turn exactly as far as it got.
    let state = dch_tui::TuiObserverState::new();
    let (observer, kept) = state.into_observer();
    drop(observer);
    kept.agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut app = TuiApp::from_observer_state(config_with_theme("dracula"), kept.clone());
    app.set_run_canceller(Box::new(|| {}));
    {
        let mut stream = kept
            .thinking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stream.text.push_str("the reasoning so far");
        stream.began = Some(std::time::Instant::now());
    }
    kept.streaming_text
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push_str("the answer had begun");
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    app.settle_exit_stop(&mut terminal);
    let row = app
        .conversation()
        .iter()
        .find(
            |message| matches!(message, TuiMessage::Assistant { blocks, .. } if blocks.len() == 2),
        )
        .expect("one assistant row carries both lanes");
    if let TuiMessage::Assistant { blocks, .. } = row {
        assert!(
            blocks.iter().any(|b| matches!(b, ContentBlock::Thinking { text, .. } if text == "the reasoning so far")),
            "the thinking lane rides: {blocks:?}"
        );
        assert!(
            blocks.iter().any(
                |b| matches!(b, ContentBlock::Text { text, .. } if text == "the answer had begun")
            ),
            "the partial reply lane rides beside it: {blocks:?}"
        );
    }
}

#[test]
fn flush_turn_end_hands_the_worker_the_whole_conversation() {
    // The flush is the exit's persistence contract: whatever the
    // conversation holds at the moment of leaving — claim rows,
    // captured lanes, the record — is what the worker saves, run
    // settled or not.
    use std::sync::Mutex as StdMutex;
    let (mut app, _kept) = app_with_state();
    app.seed_messages(vec![TuiMessage::User {
        text: "the last row".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    }]);
    let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    app.set_turn_end_hook(Box::new(move |conversation, _machine| {
        let texts: Vec<String> = conversation
            .iter()
            .filter_map(|message| match message {
                TuiMessage::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        *sink.lock().unwrap() = texts;
    }));
    app.flush_turn_end();
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["the last row".to_string()],
        "the flush fires the hook with the conversation as it stands"
    );
}

#[test]
fn a_tail_user_row_paints_its_own_chrome() {
    // The newest user turn renders as the conversation's tail — the
    // steered shape mid-run, the moment after any submit — and its
    // chrome must land on its own rows the frame it appears: the box
    // and the chevron painted from the tail's rows rebased onto the
    // cache, never from tail-relative indexes that box the pane's top
    // instead and leave the real row unmarked.
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "the first question".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "an answer several lines tall so the tail begins well \
                   below the pane's top rows, wrapping on purpose to \
                   push the next turn down the screen"
                .to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    app.push_message(TuiMessage::User {
        text: "the tail question".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    let probe = render_to_buffer(&mut app, 80, 24);
    let rows = row_texts(&probe);
    let buffer = probe.backend().buffer();
    let tail_row = rows
        .iter()
        .position(|row| row.contains("the tail question"))
        .expect("the tail turn's row");
    let tail_row = u16::try_from(tail_row).unwrap_or(0);
    assert!(
        tail_row > 4,
        "the transcript stands above the tail: {rows:?}"
    );
    let marks: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains('❯'))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        marks,
        vec![
            rows.iter()
                .position(|row| row.contains("the first question"))
                .expect("the first turn's row"),
            usize::from(tail_row),
        ],
        "one chevron per user turn, each on its own turn's row — no \
         ghost beside the first turn: {rows:?}"
    );
    assert_eq!(
        buffer[(5, tail_row)].bg,
        app.theme.ui.surface,
        "the tail turn's row rides the prompt box the frame it lands"
    );
}

#[test]
fn a_layout_rebake_leaves_no_stale_chrome() {
    // A rebake re-renders the whole conversation — a verbosity switch,
    // an expansion click — and the chrome must follow the new layout:
    // the bake's index metadata is cleared with the bake, so no stale
    // range from the old layout paints its chevron or box at rows the
    // new layout gave to other content.
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "the first question".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            name: "Read".to_string(),
            call_id: "call-1".to_string(),
            input_preview: "a file".to_string(),
            success: true,
            elapsed_secs: 1.0,
            output_preview: "one\ntwo\nthree".to_string(),
            retained_input: "{}".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    app.push_message(TuiMessage::User {
        text: "the second question".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
    });
    let _ = render_to_buffer(&mut app, 80, 30);
    app.set_verbosity(dch_config::Verbosity::Quiet);
    let probe = render_to_buffer(&mut app, 80, 30);
    let rows = row_texts(&probe);
    let buffer = probe.backend().buffer();
    let marks: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains('❯'))
        .map(|(index, _)| index)
        .collect();
    let expected: Vec<usize> = ["the first question", "the second question"]
        .iter()
        .map(|needle| {
            rows.iter()
                .position(|row| row.contains(*needle))
                .expect("a user turn's row")
        })
        .collect();
    assert_eq!(
        marks, expected,
        "after the rebake every chevron sits on its own turn's row: \
         {rows:?}"
    );
    let second_row = u16::try_from(*expected.last().expect("the second turn's row")).unwrap_or(0);
    assert_eq!(
        buffer[(5, second_row)].bg,
        app.theme.ui.surface,
        "the rebaked layout boxes each turn on its new rows"
    );
}
