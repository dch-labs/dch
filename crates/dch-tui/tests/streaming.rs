//! Streaming display tests — deltas append through the observer,
//! the frame gate coalesces background redraws, and the streaming
//! region renders markdown per completed line with the tail as
//! plaintext. Driven against ratatui's `TestBackend`; no real
//! terminal, no network, no provider.

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dch_config::DchConfig;
use dch_tui::message::{ActiveTool, ContentBlock, TuiMessage};
use dch_tui::theme::Theme;
use dch_tui::{TuiApp, TuiObserver, TuiObserverState};
use event_listener::Listener as _;
use loopctl::observer::{LoopObserver as _, ResponseContext, TextDeltaContext, TurnEndContext};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn config() -> DchConfig {
    let mut config = DchConfig::default();
    config.api.model = "testmodel".to_string();
    config
}

fn streaming_app() -> (TuiObserver, TuiApp) {
    let (observer, state) = TuiObserverState::new().into_observer();
    (observer, TuiApp::from_observer_state(config(), state))
}

fn delta(turn: usize, text: &str) -> TextDeltaContext {
    TextDeltaContext {
        turn,
        delta: text.to_string(),
    }
}

fn response(turn: usize, text: &str) -> ResponseContext {
    ResponseContext {
        turn,
        text: text.to_string(),
        usage: None,
    }
}

fn failed_turn(turn: usize) -> TurnEndContext {
    TurnEndContext {
        turn,
        success: false,
        error: Some("cancelled".to_string()),
        duration_ms: 0,
        input_tokens: 0,
        output_tokens: 0,
    }
}

fn plain(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn paras(from: usize, to: usize) -> String {
    let mut text = String::new();
    for i in from..=to {
        text.push_str("para");
        text.push_str(&i.to_string());
        text.push_str("\n\n");
    }
    text
}

fn seed(app: &TuiApp, text: &str) {
    app.streaming_text()
        .lock()
        .expect("the streaming lock")
        .push_str(text);
}

fn render_to_buffer(app: &mut TuiApp, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal
}

fn view_text(terminal: &Terminal<TestBackend>, width: u16, height: u16) -> String {
    let buffer = terminal.backend().buffer();
    (0..height)
        .flat_map(|y| (0..width).map(move |x| buffer[(x, y)].symbol().to_string()))
        .collect()
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
fn appends_are_lossless_and_ordered() {
    let (observer, app) = streaming_app();
    for chunk in ["H", "e", "l", "l", "o", " ", "world"] {
        observer.on_text_delta(&delta(0, chunk));
    }
    assert_eq!(
        &*app.streaming_text().lock().unwrap(),
        "Hello world",
        "the buffer is the verbatim, ordered concatenation"
    );
}

#[test]
fn every_delta_notifies_one_wakeup() {
    let (observer, app) = streaming_app();
    let notify = Arc::clone(app.render_notify());
    for _ in 0..5 {
        let listener = notify.listen();
        observer.on_text_delta(&delta(0, "x"));
        let woken = listener.wait_timeout(Duration::from_millis(500)).is_some();
        assert!(woken, "each delta wakes one registered listener");
    }
    assert_eq!(&*app.streaming_text().lock().unwrap(), "xxxxx");
}

#[test]
fn the_frame_cap_coalesces_redraws_not_appends() {
    let (observer, mut app) = streaming_app();
    let t0 = Instant::now();
    assert!(app.redraw_due(t0), "the first background frame renders");

    for _ in 0..50 {
        observer.on_text_delta(&delta(0, "ab"));
    }
    assert_eq!(
        app.streaming_text().lock().unwrap().len(),
        100,
        "every capped delta still reaches the buffer"
    );
    assert!(
        !app.redraw_due(t0 + Duration::from_millis(5)),
        "a request inside the frame interval is capped"
    );
    assert!(
        !app.redraw_due(t0 + Duration::from_millis(10)),
        "repeated requests inside one interval stay capped"
    );
    assert!(
        app.redraw_due(t0 + Duration::from_millis(17)),
        "the next interval renders"
    );
    assert!(
        !app.redraw_due(t0 + Duration::from_millis(20)),
        "the new interval's early request parks"
    );
    assert!(
        app.take_pending_redraw(t0 + Duration::from_millis(21)),
        "the tick claims the parked tail"
    );
    assert!(
        !app.take_pending_redraw(t0 + Duration::from_millis(22)),
        "a claimed tail is not claimed twice"
    );
}

#[test]
fn a_finalized_reply_forces_a_redraw_despite_the_cap() {
    let (observer, mut app) = streaming_app();
    let t0 = Instant::now();
    assert!(app.redraw_due(t0));
    assert!(!app.redraw_due(t0 + Duration::from_millis(1)));

    observer.on_response(&response(0, "done"));
    assert!(
        app.redraw_due(t0 + Duration::from_millis(2)),
        "a queued finalized reply forces the frame without waiting out the cap"
    );
}

#[test]
fn a_notify_wake_re_registers_the_listener_during_handling() {
    let (observer, mut app) = streaming_app();
    drop(observer);
    let notify = Arc::clone(app.render_notify());
    let mut listener = notify.listen();
    notify.notify(1);
    assert!(
        app.notify_wake(&notify, &mut listener),
        "the first background frame renders"
    );

    notify.notify(1);
    assert!(
        listener.wait_timeout(Duration::from_millis(100)).is_some(),
        "the listener the wake re-registered catches the next notification"
    );
}

#[test]
fn a_tick_redraws_for_parked_requests_or_running_tools_only() {
    let (_, mut app) = streaming_app();
    let t0 = Instant::now();
    assert!(
        !app.tick_wake(t0),
        "an idle tick with nothing parked draws nothing"
    );
    assert!(app.redraw_due(t0));
    assert!(!app.redraw_due(t0 + Duration::from_millis(1)));
    assert!(
        app.tick_wake(t0 + Duration::from_millis(2)),
        "a parked background request is claimed by the tick"
    );
    assert!(
        !app.tick_wake(t0 + Duration::from_millis(3)),
        "a claimed request is not claimed twice"
    );

    app.active_tools().lock().unwrap().push(ActiveTool {
        call_id: String::new(),
        name: "Grep".to_string(),
        input_summary: String::new(),
        start: Instant::now(),
    });
    assert!(
        app.tick_wake(t0 + Duration::from_millis(4)),
        "a running tool keeps its elapsed stamps live"
    );
}

#[test]
fn scrolling_down_rearms_only_near_the_bottom() {
    let (_, mut app) = streaming_app();
    seed(&app, &paras(1, 35));
    render_to_buffer(&mut app, 40, 24);

    for _ in 0..4 {
        app.handle_event(&plain(KeyCode::Up));
    }
    assert_eq!(app.scroll_offset(), 4);
    assert!(!app.auto_scroll(), "scrolling up detaches");

    app.handle_event(&plain(KeyCode::Down));
    assert_eq!(app.scroll_offset(), 3);
    assert!(
        !app.auto_scroll(),
        "landing outside the stick tolerance stays detached"
    );

    app.handle_event(&plain(KeyCode::Down));
    assert_eq!(app.scroll_offset(), 2);
    assert!(
        app.auto_scroll(),
        "landing within the stick tolerance re-arms"
    );
}

#[test]
fn a_poisoned_reply_buffer_still_forces_the_frame() {
    let (observer, state) = TuiObserverState::new().into_observer();
    let mut app = TuiApp::from_observer_state(config(), state.clone());
    let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = state.graduations.lock().unwrap();
        panic!("poison the replies lock");
    }));
    assert!(panicked.is_err(), "the poisoning panic must unwind");

    let t0 = Instant::now();
    assert!(app.redraw_due(t0));
    assert!(!app.redraw_due(t0 + Duration::from_millis(1)));
    observer.on_response(&response(0, "queued under poison"));
    assert!(
        app.redraw_due(t0 + Duration::from_millis(2)),
        "a queued reply forces the frame through the poisoned lock's recovery"
    );
}

#[test]
fn an_open_fence_renders_as_a_growing_framed_code_block() {
    let (observer, mut app) = streaming_app();
    drop(observer);
    seed(&app, "Intro:\n```rust\nfn main(){}\n");
    let growing = render_to_buffer(&mut app, 80, 24);
    let view = view_text(&growing, 80, 24);
    assert!(
        view.contains("fn main(){}"),
        "in-flight code inside an open fence renders as the code block"
    );
    assert!(
        !view.contains("```"),
        "the fence markers never render as literal text mid-stream"
    );

    seed(&app, "let x = 1;\n");
    let grown = render_to_buffer(&mut app, 80, 24);
    assert!(
        view_text(&grown, 80, 24).contains("let x = 1;"),
        "the framed block grows line by line"
    );
    assert!(
        !view_text(&grown, 80, 24).contains("```"),
        "the synthesized closer stays out of the rendered content"
    );
}

#[test]
fn tables_render_per_completed_row_never_mid_row() {
    let (observer, mut app) = streaming_app();
    drop(observer);
    seed(&app, "| a | b |\n|---|---|\n| 1 | 2 |\n| 30");
    let mid_row = render_to_buffer(&mut app, 80, 24);
    let view = view_text(&mid_row, 80, 24);
    assert!(
        view.contains('a') && view.contains('b'),
        "the header renders"
    );
    assert!(
        view.contains('1') && view.contains('2'),
        "the completed row renders as a table row"
    );
    assert!(
        view.contains("| 30"),
        "the mid-row text renders as the raw tail, outside the table"
    );

    seed(&app, " |\n");
    let completed = render_to_buffer(&mut app, 80, 24);
    let view = view_text(&completed, 80, 24);
    assert!(
        view.contains("30"),
        "the row joins the table once its line completes"
    );
    assert!(
        !view.contains('|'),
        "no raw pipes remain: every row passed through the table render"
    );
}

#[test]
fn the_unterminated_tail_renders_as_styled_plaintext() {
    let (observer, mut app) = streaming_app();
    drop(observer);
    seed(&app, "**bold** line\n\npartial **wo");
    let terminal = render_to_buffer(&mut app, 80, 24);
    let buffer = terminal.backend().buffer();

    let bold_modifier = Theme::default().markdown.bold.add_modifier;
    let mut saw_bold = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.symbol() == "b" && cell.modifier.contains(bold_modifier) {
                saw_bold = true;
            }
        }
    }
    assert!(
        saw_bold,
        "the completed line renders as markdown (bold styled)"
    );
    let view = view_text(&terminal, 80, 24);
    assert!(
        view.contains("partial **wo"),
        "the unterminated tail shows its markup exactly as typed"
    );
}

#[test]
fn incremental_streaming_converges_to_the_batch_render() {
    let document = "# Title\n\nSome **bold** prose here.\n\n```rust\nfn answer() -> u32 {\n    42\n}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- one\n- two\n";
    let (observer, mut app) = streaming_app();

    seed(&app, document);
    let streamed = render_to_buffer(&mut app, 80, 30);

    observer.on_response(&response(0, document));
    let graduated = render_to_buffer(&mut app, 80, 30);

    assert_eq!(
        row_texts(&streamed),
        row_texts(&graduated),
        "a fully-streamed document renders exactly as its graduated batch render"
    );
}

fn ends_inside_open_fence(text: &str) -> bool {
    let mut open: Option<&'static str> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']).trim_start_matches(' ');
        let marker = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        if let Some(marker) = marker {
            if open == Some(marker) {
                open = None;
            } else if open.is_none() {
                open = Some(marker);
            }
        }
    }
    open.is_some()
}

#[test]
fn line_by_line_streaming_matches_the_batch_render_at_every_prefix() {
    let documents = [
        "para one\n\npara two\n\npara three\n",
        "\npara one\n\npara two\n",
        "para one\n\n\npara two\n\n\n\npara three\n",
        "# Title\n\nintro **bold** text\n\n- one\n- two\n\n> quoted line\n",
        "*em* start\n\n_more_ italic after a blank\n",
        "before code\n\n```rust\nfn a() {}\nfn b() {}\n```\n\nafter code\n",
        "table intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nclosing\n",
    ];
    for document in documents {
        let (observer, mut streaming) = streaming_app();
        drop(observer);
        let mut seen = String::new();
        for line in document.split_inclusive('\n') {
            seen.push_str(line);
            seed(&streaming, line);
            let streamed = render_to_buffer(&mut streaming, 80, 30);
            let complete_lines = seen.rsplit_once('\n').map_or(String::new(), |(head, _)| {
                let mut complete = String::from(head);
                complete.push('\n');
                complete
            });
            if ends_inside_open_fence(&complete_lines) {
                continue;
            }

            let (_, mut batch) = streaming_app();
            batch.push_message(TuiMessage::Assistant {
                blocks: vec![ContentBlock::Text { text: seen.clone() }],
                timestamp: chrono::Utc::now(),
                duration_ms: None,
            });
            let batch_view = render_to_buffer(&mut batch, 80, 30);
            assert_eq!(
                row_texts(&streamed),
                row_texts(&batch_view),
                "document {document:?} diverged at prefix {seen:?}"
            );
        }
    }
}

#[test]
fn a_cleared_and_refilled_buffer_renders_only_the_new_turn() {
    let (observer, mut app) = streaming_app();
    drop(observer);
    seed(&app, "turn one alpha\n\nturn one beta\n\n");
    let first = render_to_buffer(&mut app, 80, 24);
    assert!(
        view_text(&first, 80, 24).contains("turn one alpha"),
        "the first turn freezes and renders"
    );

    let mut buffer = app.streaming_text().lock().unwrap();
    buffer.clear();
    buffer.push_str("turn two carries enough bytes to pass the old frozen length by a wide margin");
    drop(buffer);
    let second = render_to_buffer(&mut app, 80, 24);
    let view = view_text(&second, 80, 24);
    assert!(
        !view.contains("turn one"),
        "a refilled buffer must not keep the previous turn's frozen chunk: {view:?}"
    );
    assert!(view.contains("turn two"), "the refill renders on its own");
}

#[test]
fn leading_underscore_emphasis_renders_styled_while_live() {
    let (_, mut app) = streaming_app();
    seed(&app, "*em* then\n\n_more_ italic\n");
    let terminal = render_to_buffer(&mut app, 80, 24);
    let view = view_text(&terminal, 80, 24);
    assert!(
        !view.contains("_more_"),
        "the live segment parses underscore emphasis instead of rendering it literally: {view:?}"
    );
    assert!(view.contains("more"), "the emphasized word renders");
}

#[test]
fn blank_separated_tables_stream_as_two_tables() {
    let (_, mut app) = streaming_app();
    seed(&app, "| a |\n|---|\n| 1 |\n\n| b |\n|---|\n| 2 |\n");
    let terminal = render_to_buffer(&mut app, 80, 24);
    let view = view_text(&terminal, 80, 24);
    assert!(
        view.contains('a') && view.contains('b') && view.contains('1') && view.contains('2'),
        "both tables' content renders: {view:?}"
    );
    assert!(
        !view.contains('|'),
        "no raw pipes remain: the streamed render keeps the tables separate and framed: {view:?}"
    );
}

#[test]
fn on_response_clears_the_buffer_and_graduates_one_assistant_message() {
    let (observer, mut app) = streaming_app();
    observer.on_text_delta(&delta(0, "Hello "));
    observer.on_text_delta(&delta(0, "world"));
    observer.on_response(&response(0, "Hello world"));

    assert!(
        app.streaming_text().lock().unwrap().is_empty(),
        "the buffer clears when the reply graduates"
    );
    let terminal = render_to_buffer(&mut app, 80, 24);
    drop(terminal);
    assert_eq!(app.conversation().len(), 1, "exactly one message graduates");
    let TuiMessage::Assistant { blocks, .. } = app.conversation().first().unwrap() else {
        panic!("the graduated message is an assistant message");
    };
    let Some(ContentBlock::Text { text }) = blocks.first() else {
        panic!("the graduated message carries a text block");
    };
    assert_eq!(text, "Hello world", "the authoritative response text wins");
}

#[test]
fn an_empty_response_clears_the_buffer_without_graduating() {
    let (observer, mut app) = streaming_app();
    observer.on_text_delta(&delta(0, "partial"));
    observer.on_response(&response(0, ""));

    assert!(
        app.streaming_text().lock().unwrap().is_empty(),
        "an empty-text turn clears the partial buffer"
    );
    let terminal = render_to_buffer(&mut app, 80, 24);
    drop(terminal);
    assert!(
        app.conversation().is_empty(),
        "an empty-text turn graduates nothing"
    );
}

#[test]
fn auto_scroll_pins_the_newest_line_while_streaming() {
    let (observer, mut app) = streaming_app();
    seed(&app, &paras(1, 25));
    for line in 26..=35 {
        observer.on_text_delta(&delta(0, &format!("para{line}\n\n")));
        let terminal = render_to_buffer(&mut app, 40, 24);
        let rows = row_texts(&terminal);
        assert!(
            rows[19].contains(&format!("para{line}")),
            "the newest buffered line stays visible: {}",
            rows[19]
        );
    }
    assert!(app.auto_scroll(), "a pinned view stays pinned");
}

#[test]
fn scrolling_up_during_a_stream_holds_the_viewport() {
    let (observer, mut app) = streaming_app();
    seed(&app, &paras(1, 30));
    render_to_buffer(&mut app, 40, 24);

    app.handle_event(&plain(KeyCode::PageUp));
    app.handle_event(&plain(KeyCode::Up));
    let held = render_to_buffer(&mut app, 40, 24);
    let top_before = row_texts(&held)[0].clone();
    assert!(
        top_before.trim().starts_with("para"),
        "the viewport sits on a content row: {top_before:?}"
    );
    assert!(!app.auto_scroll(), "scrolling up detaches stickiness");

    observer.on_text_delta(&delta(0, &paras(31, 35)));
    let still_held = render_to_buffer(&mut app, 40, 24);
    let rows = row_texts(&still_held);
    assert_eq!(
        rows[0], top_before,
        "the top visible line does not move while tokens arrive below"
    );
    assert!(
        !rows.iter().any(|row| row.contains("para35")),
        "the newest tokens arrive off-screen"
    );
}

#[test]
fn a_detached_view_stays_anchored_when_the_layout_shrinks() {
    let (observer, mut app) = streaming_app();
    drop(observer);
    for i in 0..40 {
        app.push_message(TuiMessage::User {
            text: format!("line{i}"),
            timestamp: chrono::Utc::now(),
        });
    }
    seed(&app, &paras(1, 9));
    render_to_buffer(&mut app, 40, 24);

    app.handle_event(&plain(KeyCode::PageUp));
    app.handle_event(&plain(KeyCode::PageUp));
    assert!(!app.auto_scroll(), "the view is detached");
    let held = render_to_buffer(&mut app, 40, 24);
    let top_before = row_texts(&held)[0].trim().to_string();
    assert!(
        top_before.starts_with("line"),
        "the detached viewport sits on a conversation row: {top_before:?}"
    );

    let mut buffer = app.streaming_text().lock().unwrap();
    buffer.clear();
    drop(buffer);
    let shrunk = render_to_buffer(&mut app, 40, 24);
    let rows = row_texts(&shrunk);
    assert_eq!(
        rows[0].trim(),
        top_before,
        "the viewport holds its anchor when the streaming region collapses"
    );
    assert!(
        !view_text(&shrunk, 40, 24).contains("line0"),
        "a shrinking layout never pins the detached view at the document start"
    );
    assert!(
        !app.auto_scroll(),
        "collapsing content does not silently re-arm stickiness"
    );
}

#[test]
fn end_rearms_stickiness_and_snaps_to_the_newest_line() {
    let (_, mut app) = streaming_app();
    seed(&app, &paras(1, 35));
    render_to_buffer(&mut app, 40, 24);
    app.handle_event(&plain(KeyCode::PageUp));
    render_to_buffer(&mut app, 40, 24);
    assert!(!app.auto_scroll());

    assert!(app.handle_event(&plain(KeyCode::End)));
    assert!(app.auto_scroll(), "End re-arms stickiness");
    let terminal = render_to_buffer(&mut app, 40, 24);
    assert!(
        row_texts(&terminal)[19].contains("para35"),
        "the next render snaps to the newest line"
    );
}

#[test]
fn stickiness_survives_region_growth_without_detaching() {
    let (observer, mut app) = streaming_app();
    for line in 1..=20 {
        observer.on_text_delta(&delta(0, &format!("line{line}\n")));
        let terminal = render_to_buffer(&mut app, 40, 24);
        drop(terminal);
        assert!(
            app.auto_scroll(),
            "growth alone never detaches (line {line})"
        );
        assert_eq!(app.scroll_offset(), 0, "a pinned view holds offset zero");
    }
}

#[test]
fn a_failed_turn_discards_the_partial_stream_text() {
    let (observer, mut app) = streaming_app();
    observer.on_text_delta(&delta(0, "half a sentence"));
    observer.on_turn_end(&failed_turn(0));

    assert!(
        app.streaming_text().lock().unwrap().is_empty(),
        "a failed turn discards its partial text"
    );
    let terminal = render_to_buffer(&mut app, 80, 24);
    drop(terminal);
    assert!(
        app.conversation().is_empty(),
        "discarded text never graduates"
    );
}

#[test]
fn a_zero_width_pane_renders_content_without_panicking() {
    let (_, mut app) = streaming_app();
    seed(&app, &paras(1, 20));
    let terminal = render_to_buffer(&mut app, 0, 10);
    assert_eq!(
        terminal.backend().buffer().area.width,
        0,
        "the degenerate pane renders without the scrollbar's empty-area panic"
    );
}

#[test]
fn an_overlong_single_block_bounds_the_live_parse() {
    let (_, mut app) = streaming_app();
    let mut block = String::new();
    for i in 0..450 {
        block.push_str("parahead fill paraTail");
        block.push_str(&i.to_string());
        block.push('\n');
    }
    seed(&app, &block);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let view = view_text(&terminal, 80, 30);
    assert!(
        view.contains("parahead"),
        "the block's head stays visible as plaintext: {view:?}"
    );
    assert!(
        view.contains("paraTail"),
        "the block's recent lines keep parsing as markdown"
    );
}

#[test]
fn an_overlong_open_fence_keeps_its_head_as_plaintext() {
    let (_, mut app) = streaming_app();
    let mut block = String::from("```rust\n");
    for i in 0..450 {
        block.push_str("let codeHead = codeTail");
        block.push_str(&i.to_string());
        block.push_str(";\n");
    }
    seed(&app, &block);
    let terminal = render_to_buffer(&mut app, 80, 30);
    let view = view_text(&terminal, 80, 30);
    assert!(
        view.contains("codeHead"),
        "the fence's head code stays visible: {view:?}"
    );
    assert!(
        view.contains("codeTail449"),
        "the fence's recent code renders in the framed block"
    );
    assert!(
        !view.contains("```"),
        "the synthetic fence markers never render as content"
    );
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33)
            .unwrap_or(0)
            .checked_rem(bound.max(1))
            .unwrap_or(0)
    }
}

#[test]
fn adversarial_streams_never_lose_text() {
    let mut rng = Lcg(0x5eed_1234);
    for doc in 0..6 {
        let mut text = String::new();
        let mut words = Vec::new();
        for construct in 0..8 {
            let word = format!("w{doc}x{construct}keep");
            words.push(word.clone());
            match rng.below(7) {
                0 => {
                    text.push_str(&word);
                    text.push_str(" filler text\n");
                }
                1 => {
                    text.push_str("\n\n## ");
                    text.push_str(&word);
                    text.push('\n');
                }
                2 => {
                    text.push_str("\n\n- ");
                    text.push_str(&word);
                    text.push_str("\n- plain item\n");
                }
                3 => {
                    text.push_str("\n\n> ");
                    text.push_str(&word);
                    text.push_str(" quoted\n");
                }
                4 => {
                    text.push_str("\n\n```\nlet ");
                    text.push_str(&word);
                    text.push_str(" = 1;\n```\n");
                }
                5 => {
                    text.push_str("\n\n| a | b |\n|---|---|\n| ");
                    text.push_str(&word);
                    text.push_str(" | y |\n");
                }
                _ => {
                    text.push_str("\n\n\n");
                    text.push_str(&word);
                    text.push_str(" after a wide blank run\n");
                }
            }
        }
        let (_, mut streaming) = streaming_app();
        let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
        while !lines.is_empty() {
            let chunk = 1_usize.saturating_add(rng.below(3.min(lines.len())));
            let taken: String = lines.drain(..chunk).collect();
            seed(&streaming, &taken);
            let terminal = render_to_buffer(&mut streaming, 80, 60);
            drop(terminal);
        }
        let terminal = render_to_buffer(&mut streaming, 80, 60);
        let view = view_text(&terminal, 80, 60);
        for word in words {
            assert!(
                view.contains(&word),
                "streaming lost the word {word} in document {doc}: {text:?}"
            );
        }
    }
}

#[test]
fn an_overlong_single_line_bounds_the_live_parse() {
    let (_, mut app) = streaming_app();
    let mut block = String::from("headmarker ");
    block.push_str(&"x".repeat(200_000));
    block.push_str(" tailmarker\n");
    seed(&app, &block);
    let terminal = render_to_buffer(&mut app, 80, 30);
    assert!(
        view_text(&terminal, 80, 30).contains("tailmarker"),
        "the byte cap keeps the trailing markdown part rendering"
    );

    for _ in 0..300 {
        app.handle_event(&plain(KeyCode::PageUp));
    }
    let scrolled = render_to_buffer(&mut app, 80, 30);
    assert!(
        view_text(&scrolled, 80, 30).contains("headmarker"),
        "the oversized line's plaintext head stays reachable above the fold"
    );
}

#[test]
fn fence_close_spacing_residuals_keep_every_content_row() {
    let documents = [
        "intro\n\n```rust\nfn closeA() {}\n```\n## after the fence\n",
        "- one\n\n  ```rust\n  fn closeB() {}\n  ```\n- two\n",
    ];
    for document in documents {
        let (_, mut streaming) = streaming_app();
        seed(&streaming, document);
        let streamed = render_to_buffer(&mut streaming, 80, 30);

        let (_, mut batch) = streaming_app();
        batch.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: document.to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        let batch_view = render_to_buffer(&mut batch, 80, 30);

        let content_rows = |terminal: &Terminal<TestBackend>| {
            row_texts(terminal)
                .into_iter()
                .filter(|row| !row.trim().is_empty())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            content_rows(&streamed),
            content_rows(&batch_view),
            "the fence-close spacing residual shifts only blank rows: {document:?}"
        );
    }
}

#[test]
fn a_poisoned_stream_buffer_still_appends() {
    let (observer, app) = streaming_app();
    let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = app.streaming_text().lock().unwrap();
        panic!("poison the streaming lock");
    }));
    assert!(panicked.is_err(), "the poisoning panic must unwind");

    observer.on_text_delta(&delta(0, "x"));
    let recovered = app
        .streaming_text()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        recovered.ends_with('x'),
        "the recovered guard still appends after poisoning"
    );
}

#[test]
fn a_poisoned_stream_buffer_still_renders() {
    let (observer, mut app) = streaming_app();
    seed(&app, &paras(1, 3));
    let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = app.streaming_text().lock().unwrap();
        panic!("poison the streaming lock");
    }));
    assert!(panicked.is_err(), "the poisoning panic must unwind");

    let terminal = render_to_buffer(&mut app, 40, 24);
    let view = view_text(&terminal, 40, 24);
    assert!(
        view.contains("para1") && view.contains("para3"),
        "the live region renders through the poisoned lock instead of going blank: {view:?}"
    );

    observer.on_text_delta(&delta(0, "post-poison tail"));
    drop(observer);
    let second = render_to_buffer(&mut app, 40, 24);
    assert!(
        view_text(&second, 40, 24).contains("post-poison tail"),
        "deltas arriving after the poisoning keep rendering on later frames"
    );
}

#[test]
fn append_path_stays_well_inside_the_frame_budget() {
    let (observer, _app) = streaming_app();
    let start = Instant::now();
    for _ in 0..10_000 {
        observer.on_text_delta(&delta(0, "x"));
    }
    let mean_micros = start.elapsed().as_secs_f64() * 100.0;
    assert!(
        mean_micros < 100.0,
        "mean append+notify cost {mean_micros:.2} µs exceeds the 100 µs budget"
    );
}
