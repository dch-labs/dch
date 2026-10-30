//! The waiting-prompt section: keys, editing, dropping, and rows.
//!
//! Everything here drives a real `TuiApp` over real shared queue state —
//! the same `PromptQueue` the mode driver reads prompts out of — because the
//! section's whole purpose is a seam: a prompt sits in one half's hands while
//! the other half's turn is running, and the interesting failures are all at
//! that seam. So the suite submits through the composer, presses keys through
//! `handle_event`, claims through the queue the way the driver claims, and
//! reads what the frame drew out of a `TestBackend` buffer.

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
use std::sync::atomic::Ordering;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dch_config::DchConfig;
use dch_tui::Graduation;
use dch_tui::TuiApp;
use dch_tui::message::TuiMessage;
use dch_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

/// The glyph marking the section's rows.
const HOURGLASS: &str = "\u{231b}";

fn config() -> DchConfig {
    let mut config = DchConfig::default();
    config.display.theme = "dracula".to_string();
    config.api.model = "testmodel".to_string();
    config
}

/// An app over observer state the test keeps a handle to, plus that state.
///
/// The queue is shared by construction — the app writes prompts into it and
/// the test, standing in for the driver, reads them back out — which is the
/// only way a test can claim a prompt the way a claim really happens.
fn app_with_state() -> (TuiApp, dch_tui::TuiObserverState) {
    let (observer, kept) = dch_tui::TuiObserverState::new().into_observer();
    drop(observer);
    (TuiApp::from_observer_state(config(), kept.clone()), kept)
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn plain(code: KeyCode) -> Event {
    key(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> Event {
    key(code, KeyModifiers::CONTROL)
}

/// Press a key, dropping the redraw verdict.
fn press(app: &mut TuiApp, event: &Event) {
    let _verdict = app.handle_event(event);
}

fn type_into(app: &mut TuiApp, text: &str) {
    for ch in text.chars() {
        press(app, &plain(KeyCode::Char(ch)));
    }
}

/// Compose `text` and press Enter — the submit the user performs.
fn submit(app: &mut TuiApp, text: &str) {
    type_into(app, text);
    press(app, &plain(KeyCode::Enter));
}

/// What the shared queue holds, as `(id, text)` pairs in send order.
fn waiting(kept: &dch_tui::TuiObserverState) -> Vec<(u64, String)> {
    let queue = kept.queued.lock().unwrap();
    (0..queue.len())
        .filter_map(|index| queue.get(index))
        .map(|entry| (entry.id, entry.text.clone()))
        .collect()
}

/// Take a prompt out of the queue by id, as the driver's claim does.
fn claim(kept: &dch_tui::TuiObserverState, id: u64) -> Option<String> {
    kept.queued.lock().unwrap().remove(id)
}

/// The waiting prompt a submission names.
///
/// Pressing Enter in the composer always hands over a `Run`; the channel's
/// other message is a different test's subject, and naming it beats a
/// blind unwrap that reports nothing when the wrong one arrives.
fn run_id(submission: dch_tui::Submission) -> u64 {
    match submission {
        dch_tui::Submission::Run { id } => id,
        other @ dch_tui::Submission::Correct { .. } => {
            panic!("a plain submit hands over a run, got {other:?}")
        }
    }
}

/// An app with `texts` prompts waiting in its queue and a live channel.
///
/// Submits go through the composer rather than into the queue directly, so
/// every prompt in these tests arrived the way a waiting prompt really
/// arrives — queued, then handed over by id. The receiver is returned alive:
/// a dropped one makes every later submit a failed send, which is a different
/// test's subject. The agent is staged as running so the mid-run shapes
/// these tests drive (steering into a live turn, chords that serve it)
/// stand on their real precondition. The waiting panel itself does not
/// hang on that flag: it rises for a prompt the *next* run still owes
/// (no run in flight to take it) and stays down for one already handed to
/// the in-flight run, which is a claim away from running rather than a
/// prompt waiting on nobody.
fn app_with_waiting(
    texts: &[&str],
) -> (
    TuiApp,
    dch_tui::TuiObserverState,
    tokio::sync::mpsc::UnboundedReceiver<dch_tui::Submission>,
) {
    let (mut app, kept) = app_with_state();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    kept.agent_running.store(true, Ordering::SeqCst);
    for text in texts {
        submit(&mut app, text);
    }
    (app, kept, rx)
}

/// Render and hand back the frame's rows as plain strings.
fn rows(app: &mut TuiApp, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        })
        .collect()
}

/// The row indices carrying the section's marker glyph.
fn marker_rows(rendered: &[String]) -> Vec<usize> {
    rendered
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains(HOURGLASS))
        .map(|(index, _)| index)
        .collect()
}

/// The row index carrying `needle`, or a failure naming the whole frame.
fn row_holding(rendered: &[String], needle: &str) -> usize {
    rendered
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("nothing on screen holds {needle:?}: {rendered:?}"))
}

#[test]
fn a_mid_run_submit_waits_in_the_queue_and_not_in_the_transcript() {
    let (mut app, kept, _rx) = app_with_waiting(&[]);
    kept.agent_running.store(true, Ordering::SeqCst);

    submit(&mut app, "fix the tests");

    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["fix the tests"],
        "the prompt waits in the queue while the run holds the runner"
    );
    assert!(
        app.conversation().is_empty(),
        "and it is not in the transcript yet: the row a user sees is the claim \
         echo, so a prompt nobody has run has no row to hold — the queue is \
         where it reads as waiting"
    );
    assert!(
        app.input().is_empty(),
        "the composer is empty either way — the prompt left it"
    );
}

#[test]
fn an_idle_submit_with_no_driver_still_echoes_at_once() {
    let mut app = TuiApp::new(config());

    submit(&mut app, "hello there");

    assert_eq!(
        app.conversation().len(),
        1,
        "with no channel to hand prompts to there is no claim to wait for, so \
         the submit echoes where it always has"
    );
    match app.conversation().first() {
        Some(TuiMessage::User { text, .. }) => assert_eq!(text, "hello there"),
        other => panic!("expected the submit's own row, got {other:?}"),
    }
}

#[test]
fn the_section_rises_for_an_owed_prompt_and_stays_down_for_a_claim_hop() {
    // The section is not the run's shadow. Prompts a resume owes wait
    // with no run up, and hiding them behind a run gate would put them
    // back in the invisible backlog the seeding exists to clear — so
    // anything genuinely waiting raises the panel, idle or not. The
    // one thing that does not raise it is the handoff window: a submit
    // into a session at rest is the driver's next claim, one channel
    // hop away, and its row stays hidden for that hop instead of
    // flashing under the title — the claim takes it exactly as before.
    let (mut app, kept) = app_with_state();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);

    submit(&mut app, "straight out");

    let rendered = rows(&mut app, 80, 24);
    assert!(
        marker_rows(&rendered).is_empty(),
        "the fresh submit is one claim hop from running — no flash \
         under the title: {rendered:?}"
    );
    assert_eq!(
        waiting(&kept).len(),
        1,
        "the prompt itself is still in the queue for the driver's claim"
    );

    // An owed prompt — seeded the way a resume seeds, not submitted —
    // waits with no claim coming for it, and the panel carries it.
    std::mem::drop(rx);
    let (mut app, kept) = app_with_state();
    kept.queued
        .lock()
        .unwrap()
        .push("owed by the resume".to_string());
    let rendered = rows(&mut app, 80, 24);
    assert!(
        !marker_rows(&rendered).is_empty(),
        "an owed prompt waits visibly, run or not: {rendered:?}"
    );
}

#[test]
fn a_queue_title_names_the_waiting_section_and_leaves_with_it() {
    let (mut app, kept, _rx) = app_with_waiting(&["first waiting"]);

    let rendered = rows(&mut app, 80, 24);
    let title = row_holding(&rendered, "QUEUE");
    let first_prompt = *marker_rows(&rendered)
        .first()
        .expect("a waiting prompt marks its own row");
    assert!(
        title < first_prompt,
        "the QUEUE title rides above the prompt it is showing: {rendered:?}"
    );
    assert_eq!(
        first_prompt - title,
        2,
        "a blank row stands the badge off the prompt it labels: {rendered:?}"
    );
    assert!(
        rendered[title + 1].trim().is_empty(),
        "the stand-off below the badge is blank — the pill does not rest on \
         the wording it labels: {rendered:?}"
    );
    assert!(
        title >= 1 && rendered[title - 1].trim().is_empty(),
        "a blank row stands the badge off the transcript above it — the \
         section opens with air, not the pill: {rendered:?}"
    );

    let id = waiting(&kept).first().map(|(id, _)| *id).unwrap();
    claim(&kept, id);
    let emptied = rows(&mut app, 80, 24);
    assert!(
        !emptied.iter().any(|row| row.contains("QUEUE")) && marker_rows(&emptied).is_empty(),
        "an emptied queue is no panel — title included: {emptied:?}"
    );
}

/// One rendered frame as `(symbol, fg, bg)` cells, row by row — the
/// same frame `rows` reads, kept for the pins that reason about paint
/// rather than wording.
///
/// The badge's contract lives in its colors: the fill is the theme's
/// primary accent, it covers exactly the pill, and in a deferring
/// theme it is no fill at all. Reading the colors off the rendered
/// cells is the only way to pin any of that.
fn painted_rows(app: &mut TuiApp, width: u16, height: u16) -> Vec<Vec<(String, Color, Color)>> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| {
                    let cell = &buffer[(x, y)];
                    (cell.symbol().to_string(), cell.fg, cell.bg)
                })
                .collect()
        })
        .collect()
}

/// One painted row's wording, joined the way `rows` joins a frame's.
fn painted_text(row: &[(String, Color, Color)]) -> String {
    row.iter().map(|(symbol, _, _)| symbol.as_str()).collect()
}

/// Where the title's badge sits on the frame: the row holding the
/// word, and the column of its first letter.
///
/// The pill spans one fill cell proud of the word on either side, so
/// callers index relative to the returned column: `column - 1` is the
/// lead fill, `column` through `column + 4` the letters, `column + 5`
/// the trail fill.
fn badge_at(painted: &[Vec<(String, Color, Color)>]) -> (usize, usize) {
    let row = painted
        .iter()
        .position(|row| painted_text(row).contains("QUEUE"))
        .unwrap_or_else(|| panic!("no row holds the title: {painted:?}"));
    let column = painted[row]
        .iter()
        .position(|(symbol, _, _)| symbol == "Q")
        .unwrap_or_else(|| panic!("the title row holds no Q: {painted:?}"));
    (row, column)
}

#[test]
fn the_title_rides_as_a_badge_of_the_theme_s_own_colors() {
    let (mut app, _kept, _rx) = app_with_waiting(&["first waiting"]);

    let painted = painted_rows(&mut app, 80, 24);
    let (row, column) = badge_at(&painted);
    for offset in 0..5 {
        let (symbol, fg, bg) = &painted[row][column + offset];
        assert_eq!(
            *bg, app.theme.ui.primary,
            "the word rides the accent fill the composer's edge wears: {symbol:?}"
        );
        assert_eq!(
            *fg, app.theme.ui.background,
            "the canvas color keeps the letters legible on the accent"
        );
    }
    for proud in [column - 1, column + 5] {
        let (_, _, bg) = &painted[row][proud];
        assert_eq!(
            *bg, app.theme.ui.primary,
            "one fill cell proud of the word on either side"
        );
    }
    // The pill rides flush with the section's rows: its left proud
    // edge shares the hourglass marker's column, not a stand-off to
    // the right of it.
    let marker_column = painted
        .iter()
        .flat_map(|row| row.iter().enumerate())
        .find(|(_, (symbol, _, _))| symbol == "\u{231b}")
        .map(|(x, _)| x)
        .expect("the waiting prompt carries the hourglass marker");
    assert_eq!(
        column.saturating_sub(1),
        marker_column,
        "the pill's left edge aligns with the marker's column"
    );
    let (_, _, past_bg) = &painted[row][column + 7];
    assert_eq!(
        *past_bg,
        Color::Reset,
        "the fill is a pill, not a panel — the row past the badge paints nothing"
    );
}

#[test]
fn the_keys_light_the_marker_and_leave_the_badge_s_accent_steady() {
    let (mut app, _kept, _rx) = app_with_waiting(&["first waiting"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    let painted = painted_rows(&mut app, 80, 24);
    let (row, column) = badge_at(&painted);
    for offset in 0..5 {
        let (_, fg, bg) = &painted[row][column + offset];
        assert_eq!(
            *bg, app.theme.ui.primary,
            "the badge keeps its accent — focus is the marker's to tell"
        );
        assert_eq!(
            *fg, app.theme.ui.background,
            "the canvas color keeps the letters legible on the accent"
        );
    }
    let marker = painted
        .iter()
        .flatten()
        .find(|(symbol, _, _)| symbol == "\u{231b}")
        .expect("the waiting prompt carries the hourglass marker");
    assert_eq!(
        marker.1, app.theme.ui.primary,
        "the keys light the marker's voice, not the label's fill"
    );
}

#[test]
fn a_deferring_theme_leaves_the_badge_unfilled() {
    let (mut app, _kept, _rx) = app_with_waiting(&["first waiting"]);
    app.theme = Theme::by_name("transparent").expect("the transparent theme is shipped");

    let painted = painted_rows(&mut app, 80, 24);
    let (row, column) = badge_at(&painted);
    for (_, _, bg) in painted[row].iter().skip(column - 1).take(7) {
        assert_eq!(
            *bg,
            Color::Reset,
            "the badge defers to the terminal's own canvas — no block of paint"
        );
    }
}

#[test]
fn ctrl_q_hands_the_keys_to_the_queue_and_the_arrows_walk_it() {
    let (mut app, kept, _rx) = app_with_waiting(&["first waiting", "second waiting"]);

    press(&mut app, &ctrl(KeyCode::Char('q')));
    // A letter typed while reading the backlog belongs to nobody.
    type_into(&mut app, "z");
    assert!(
        app.input().is_empty(),
        "the section holds the keyboard, so a letter reaches neither the \
         composer nor the entry it is drawn over"
    );

    press(&mut app, &plain(KeyCode::Right));
    let shown = rows(&mut app, 80, 24);
    assert!(
        shown.iter().any(|row| row.contains("[2/2]")),
        "the arrow walked to the last waiting prompt: {shown:?}"
    );
    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["first waiting", "second waiting"],
        "walking moves the view, never the queue's order"
    );
}

#[test]
fn a_paste_while_the_section_holds_the_keys_builds_nothing() {
    // A paste is composition in bulk, and the focused section swallows
    // composition so nothing assembles behind the user's back — a paste
    // that slipped through would strand an invisible draft under a
    // caret the section is not even showing.
    let (mut app, _kept, _rx) = app_with_waiting(&["something waiting"]);

    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(
        &mut app,
        &Event::Paste("pasted behind the backlog".to_string()),
    );

    assert!(
        app.input().is_empty(),
        "the focused section swallowed the paste the way it swallows typing"
    );
}

#[test]
fn a_question_sheet_keeps_the_waiting_section_visible() {
    // Submitting during a run and then being asked something is the
    // common shape: the ask takes the keyboard, and the waiting
    // section is the only place the queued prompt still reads as
    // waiting. A sheet that painted over its rows would break the
    // "you can see it waiting" guarantee for as long as it is up.
    let (mut app, kept, _rx) = app_with_waiting(&["waiting under the ask"]);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
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

    let rendered = rows(&mut app, 80, 24);
    assert!(
        !marker_rows(&rendered).is_empty(),
        "the section keeps its rows under the sheet: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("waiting under the ask")),
        "the prompt's wording is still on screen, not just its count: {rendered:?}"
    );
    assert_eq!(
        waiting(&kept).len(),
        1,
        "and the queue itself is untouched — the sheet hides nothing, it reserves rows"
    );
}

#[test]
fn ctrl_q_on_an_empty_queue_takes_nothing() {
    let (mut app, _kept, _rx) = app_with_waiting(&[]);

    press(&mut app, &ctrl(KeyCode::Char('q')));
    type_into(&mut app, "hi");

    assert_eq!(
        app.input(),
        "hi",
        "there is no section to focus with nothing waiting, so the chord \
         cannot swallow a session's keyboard away from the composer"
    );
}

#[test]
fn the_arrows_still_move_the_composer_caret_when_the_queue_is_not_focused() {
    let (mut app, _kept, _rx) = app_with_waiting(&["one waiting prompt"]);

    type_into(&mut app, "ab");
    press(&mut app, &plain(KeyCode::Left));
    type_into(&mut app, "X");

    assert_eq!(
        app.input(),
        "aXb",
        "a waiting prompt on screen must not cost the composer its caret: the \
         arrows belong to the text until Ctrl+Q asks for them"
    );
}

#[test]
fn the_walk_wraps_past_the_last_waiting_prompt() {
    let (mut app, _kept, _rx) = app_with_waiting(&["one", "two", "three"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    for _ in 0..3 {
        press(&mut app, &plain(KeyCode::Right));
    }
    let forward = rows(&mut app, 80, 24);
    assert!(
        forward.iter().any(|row| row.contains("[1/3]")),
        "three steps right from the front come back around to it: {forward:?}"
    );

    press(&mut app, &plain(KeyCode::Left));
    let backward = rows(&mut app, 80, 24);
    assert!(
        backward.iter().any(|row| row.contains("[3/3]")),
        "and one step left from the front lands on the tail — the walk is a \
         circle, not a wall"
    );
}

#[test]
fn e_lifts_the_prompt_into_the_composer_and_gives_the_keys_back() {
    let (mut app, kept, _rx) = app_with_waiting(&["rewrite the parser"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    press(&mut app, &plain(KeyCode::Char('e')));
    assert_eq!(
        app.input(),
        "rewrite the parser",
        "the waiting prompt's text is now on screen where it can be fixed"
    );

    type_into(&mut app, " from scratch");
    assert_eq!(
        app.input(),
        "rewrite the parser from scratch",
        "typing reaches the lifted text — the composer's keys came back with \
         the prompt"
    );
    assert_eq!(
        waiting(&kept),
        vec![(0, "rewrite the parser".to_string())],
        "and the queue still holds the prompt with the wording it had: an \
         edit is saved or discarded, never half-applied"
    );
}

#[test]
fn e_refuses_a_non_empty_composer_and_changes_nothing() {
    let (mut app, kept, _rx) = app_with_waiting(&["first waiting", "second waiting"]);
    type_into(&mut app, "a draft of my own");

    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));

    assert_eq!(
        app.input(),
        "a draft of my own",
        "the draft standing in the composer is not collateral for editing \
         somebody else's prompt"
    );
    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["first waiting", "second waiting"],
        "and the waiting prompt keeps its own text where it was"
    );
    let rendered = rows(&mut app, 100, 24);
    assert!(
        rendered.iter().any(|row| row.contains("draft")),
        "the refusal is said on the notice row rather than left as silence: \
         {rendered:?}"
    );
}

#[test]
fn enter_saves_the_edited_text_into_its_slot_and_keeps_the_order() {
    let (mut app, kept, _rx) = app_with_waiting(&["first task", "second task", "third task"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Right));
    press(&mut app, &plain(KeyCode::Char('e')));
    type_into(&mut app, " with the patch");

    press(&mut app, &plain(KeyCode::Enter));

    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["first task", "second task with the patch", "third task"],
        "the edit lands in the slot the prompt was submitted into — the order \
         the turns run in is the order they were typed in, not the order they \
         were fixed in"
    );
    assert!(
        app.input().is_empty(),
        "the composer is the composer again once the edit is saved"
    );
}

#[test]
fn saving_an_edit_leaves_the_input_history_alone() {
    let (mut app, _kept, _rx) = app_with_waiting(&["the real first task", "the real second"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    type_into(&mut app, " (fixed)");
    press(&mut app, &plain(KeyCode::Enter));
    // Back to the composer's own keys, then the recall the history answers.
    press(&mut app, &plain(KeyCode::Esc));

    press(&mut app, &plain(KeyCode::Up));

    assert_eq!(
        app.input(),
        "the real second",
        "Up recalls the last prompt actually submitted, not the edit text — a \
         saved edit was never sent, and a recall list of wordings that never \
         went out is not a history of what was asked"
    );
}

#[test]
fn a_blank_commit_saves_nothing_and_says_so() {
    let (mut app, kept, _rx) = app_with_waiting(&["a prompt worth keeping"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    for _ in 0.."a prompt worth keeping".len() {
        press(&mut app, &plain(KeyCode::Backspace));
    }
    assert!(app.input().is_empty(), "the draft is emptied out");

    press(&mut app, &plain(KeyCode::Enter));

    assert_eq!(
        waiting(&kept),
        vec![(0, "a prompt worth keeping".to_string())],
        "an entry emptied into a prompt of nothing is worse to find on the row \
         than the wrong wording that was there"
    );
    let rendered = rows(&mut app, 100, 24);
    assert!(
        rendered.iter().any(|row| row.contains("save needs")),
        "the refusal names itself: {rendered:?}"
    );
}

#[test]
fn esc_drops_the_draft_and_leaves_the_prompt_as_it_was() {
    let (mut app, kept, _rx) = app_with_waiting(&["the prompt as submitted"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    type_into(&mut app, "junk typed into the lifted text");

    press(&mut app, &plain(KeyCode::Esc));

    assert_eq!(
        waiting(&kept),
        vec![(0, "the prompt as submitted".to_string())],
        "Esc is the way out of the edit, not out of the queue: the prompt \
         keeps the wording it had before the composer took it"
    );
    assert!(
        app.input().is_empty(),
        "the borrowed text leaves with the mode that borrowed it"
    );
    type_into(&mut app, "z");
    assert!(
        app.input().is_empty(),
        "and the keys go back to the section, which still has a prompt on \
         screen to act on"
    );
}

#[test]
fn ctrl_c_while_editing_aborts_and_leaves_the_prompt_alone() {
    let (mut app, kept, _rx) = app_with_waiting(&["the untouched prompt"]);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    app.set_run_canceller(Box::new(move || flag.store(true, Ordering::SeqCst)));
    kept.agent_running.store(true, Ordering::SeqCst);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    type_into(&mut app, "mid-edit");

    press(&mut app, &ctrl(KeyCode::Char('c')));

    assert_eq!(
        waiting(&kept),
        vec![(0, "the untouched prompt".to_string())],
        "with a prompt out in front of the user the press lands on the text in \
         front of them"
    );
    assert!(
        !cancelled.load(Ordering::SeqCst),
        "the run's cancel is one Esc away rather than stolen from under the \
         edit in progress"
    );
    assert!(!app.is_quitting(), "and it quits nothing either");
    assert!(app.input().is_empty(), "the draft goes, the prompt stays");
}

#[test]
fn ctrl_q_after_an_edit_saves_it_before_focusing() {
    let (mut app, kept, _rx) = app_with_waiting(&["first waiting", "second waiting"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    type_into(&mut app, " corrected");

    press(&mut app, &ctrl(KeyCode::Char('q')));

    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["first waiting corrected", "second waiting"],
        "one chord does both jobs from where the keys are: the edit is saved \
         on the way out rather than dropped on the floor"
    );
    assert!(app.input().is_empty(), "the composer is empty again");
    type_into(&mut app, "z");
    assert!(
        app.input().is_empty(),
        "and the section holds the keys again, so the chord landed in the \
         mode it names"
    );
}

#[test]
fn a_refused_save_does_not_sweep_past_the_edit() {
    let (mut app, kept, _rx) = app_with_waiting(&["first waiting", "second waiting"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    // Wipe the draft: the save is refused, so the edit has to stay put with
    // its text rather than be filed away behind a focused list — the chord
    // out only means the section once there is nothing left to fix.
    for _ in 0.."first waiting".chars().count() {
        press(&mut app, &plain(KeyCode::Backspace));
    }

    press(&mut app, &ctrl(KeyCode::Char('q')));

    // The tell of which mode owns the keyboard: a letter. On the list it is
    // swallowed; in the open edit it lands as the new draft.
    type_into(&mut app, "x");
    assert_eq!(
        app.input(),
        "x",
        "a refused save leaves the keys with the text, not the list"
    );
    assert_eq!(
        waiting(&kept)[0].1,
        "first waiting",
        "and the prompt keeps its words"
    );
}

#[test]
fn delete_drops_the_prompt_on_screen_and_shows_the_next() {
    let (mut app, kept, mut rx) = app_with_waiting(&["drop this one", "keep this one"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    press(&mut app, &plain(KeyCode::Delete));
    press(&mut app, &plain(KeyCode::Char('y')));

    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["keep this one"],
        "the prompt on screen is the one that goes, and the one behind it \
         takes its row"
    );
    assert_eq!(
        run_id(rx.try_recv().expect("both ids were sent")),
        0,
        "the dropped prompt's id is still travelling — the driver resolves it \
         against the queue at its turn, where it finds nothing"
    );
    let rendered = rows(&mut app, 80, 24);
    assert!(
        !rendered.iter().any(|row| row.contains("drop this one")),
        "the section stops showing what is no longer waiting: {rendered:?}"
    );
}

#[test]
fn dropping_the_last_prompt_releases_the_focus() {
    let (mut app, kept, _rx) = app_with_waiting(&["the only one"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    press(&mut app, &plain(KeyCode::Delete));
    press(&mut app, &plain(KeyCode::Char('y')));

    assert!(waiting(&kept).is_empty(), "the queue is empty");
    type_into(&mut app, "typed after");
    assert_eq!(
        app.input(),
        "typed after",
        "with no section left to draw there is no mode left to hold the \
         keyboard: a focus kept past the last prompt would swallow every key"
    );
}

#[test]
fn s_and_d_ask_before_they_act() {
    // One keypress away from spending a prompt, the ask comes first:
    // the destructive keys hold their act until `y`, any other key
    // stands them down, and nothing leaves the queue while the ask
    // stands.
    let (mut app, kept, _rx) = app_with_waiting(&["first waiting", "second waiting"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    press(&mut app, &plain(KeyCode::Char('d')));
    assert_eq!(
        waiting(&kept).len(),
        2,
        "the ask spends nothing while it stands"
    );
    let asking = rows(&mut app, 80, 24);
    assert!(
        asking
            .iter()
            .any(|row| row.contains("drop this prompt? y/n")),
        "the ask is one short question with its keys: {asking:?}"
    );

    // Any other key stands the ask down without spending anything.
    press(&mut app, &plain(KeyCode::Char('n')));
    assert_eq!(waiting(&kept).len(), 2, "n cancels the ask, not the prompt");
    let stood_down = rows(&mut app, 80, 24);
    assert!(
        !stood_down
            .iter()
            .any(|row| row.contains("drop this prompt? y/n")),
        "the ask stands down: {stood_down:?}"
    );

    // Confirmed, the act is the one the ask named — and the section
    // keeps the keys it held.
    press(&mut app, &plain(KeyCode::Char('d')));
    press(&mut app, &plain(KeyCode::Char('y')));
    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["second waiting"],
        "y drops the prompt the ask named"
    );
    type_into(&mut app, "z");
    assert_eq!(
        app.input(),
        "",
        "the section still holds the keys after the act"
    );
}

#[test]
fn the_composer_greys_out_while_the_section_holds_the_keys() {
    // The composer stands while the section holds the keyboard, but it
    // is reading matter: its edge column and its text fall to the dim
    // register and its caret hides, so the frame itself says where the
    // keys are landing. An edit brings it back to full voice — the
    // lifted prompt is being typed there.
    let (mut app, kept, _rx) = app_with_waiting(&["waiting while greyed"]);
    let primary = app.theme.ui.primary;
    let dim = app.theme.ui.dim;

    let full = painted_rows(&mut app, 80, 24);
    let primary_edges = full
        .iter()
        .filter(|row| row.first().is_some_and(|(_, _, bg)| *bg == primary))
        .count();
    assert_eq!(
        primary_edges, 5,
        "the composer's edge speaks in the primary while the composer holds \
         the keys — four edge rows plus the badge, which rides flush with \
         the section's left edge and shares the composer's accent column"
    );
    assert!(
        app.composer_caret_visible(),
        "and its caret shows where typing lands"
    );

    press(&mut app, &ctrl(KeyCode::Char('q')));
    let greyed = painted_rows(&mut app, 80, 24);
    let dim_edges = greyed
        .iter()
        .filter(|row| row.first().is_some_and(|(_, _, bg)| *bg == dim))
        .count();
    assert_eq!(
        dim_edges, 4,
        "the section's focus greys the composer's edge out"
    );
    assert!(
        !app.composer_caret_visible(),
        "and the caret hides — the composer cannot be typed into"
    );

    // An edit is the composer's again: full voice, caret back.
    press(&mut app, &plain(KeyCode::Char('e')));
    let lifted = painted_rows(&mut app, 80, 24);
    let restored_edges = lifted
        .iter()
        .filter(|row| row.first().is_some_and(|(_, _, bg)| *bg == primary))
        .count();
    assert_eq!(
        restored_edges, 5,
        "editing lifts the prompt into the composer and the composer speaks \
         again — the section's badge stays up through the edit, sharing \
         the accent column"
    );
    assert!(
        app.composer_caret_visible(),
        "the caret returns with the edit — the draft is being typed"
    );
    let _ = waiting(&kept);
}

#[test]
fn the_queue_chords_are_swallowed_and_the_runs_chords_survive() {
    let (mut app, kept, _rx) = app_with_waiting(&["waiting one", "waiting two"]);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    app.set_run_canceller(Box::new(move || flag.store(true, Ordering::SeqCst)));
    kept.agent_running.store(true, Ordering::SeqCst);
    // A conversation tall enough that paging it is observable.
    app.seed_messages(vec![TuiMessage::Assistant {
        blocks: vec![dch_tui::ContentBlock::Text {
            text: "line\n".repeat(200),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    }]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    type_into(&mut app, "qx");
    // PageUp first: any key between the chord's two presses would
    // stand the exit down, so the scroll check rides ahead of them.
    press(&mut app, &plain(KeyCode::PageUp));
    press(&mut app, &ctrl(KeyCode::Char('c')));

    assert!(
        app.input().is_empty(),
        "the letters the section answers with reach no draft"
    );
    assert!(
        !cancelled.load(Ordering::SeqCst),
        "the first press through the section only arms, as everywhere"
    );
    assert!(!app.is_quitting(), "the arming press leaves nothing");
    assert!(
        app.scroll_offset() > 0,
        "and PageUp still scrolls the transcript they may be reading for \
         context"
    );
    assert_eq!(waiting(&kept).len(), 2, "none of it touched the queue");

    // The fire: one action through the section as everywhere — the
    // run cancels and the program terminates on the same press.
    press(&mut app, &ctrl(KeyCode::Char('c')));
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the fire cancels through the section, as everywhere"
    );
    assert!(
        app.is_quitting(),
        "and the same fire terminates — one action"
    );
}

#[test]
fn a_claimed_prompt_lands_in_the_transcript_and_leaves_the_section() {
    let (mut app, kept, mut rx) = app_with_waiting(&["the prompt that ran"]);
    let id = run_id(rx.try_recv().expect("the submission was sent"));
    let text = claim(&kept, id).expect("the queue held the prompt");
    // The driver hands the claim to the display as the run starts.
    kept.push_sent(text);

    let rendered = rows(&mut app, 80, 24);

    assert_eq!(
        app.conversation().len(),
        1,
        "the frame's drain turns the claim into the transcript's user row"
    );
    match app.conversation().first() {
        Some(TuiMessage::User { text, .. }) => assert_eq!(text, "the prompt that ran"),
        other => panic!("expected the claim's row, got {other:?}"),
    }
    assert!(
        marker_rows(&rendered).is_empty(),
        "and the section is gone with it — one place shows a prompt at a time"
    );
}

#[test]
fn a_commit_on_a_prompt_the_run_already_took_goes_out_as_a_correction() {
    let (mut app, kept, mut rx) = app_with_waiting(&["the prompt under edit"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    let (edited_from, _) = waiting(&kept)[0];
    type_into(&mut app, " — fixed after the fact");

    // The run takes the prompt while its text stands open in the composer:
    // the words it went out with already reached the model.
    let handed_over = run_id(rx.try_recv().expect("the id travelled first"));
    assert_eq!(
        handed_over, edited_from,
        "the id names the prompt open in the composer"
    );
    let _claimed = claim(&kept, edited_from);
    // Production cannot offer the bare sequence above: the claim notifies the
    // loop, and the loop's notify arm draws before the next key is read. The
    // frame is what stands a released mode down, so a correction that survives
    // only when nothing renders is a correction the user never gets.
    let _rendered = rows(&mut app, 80, 24);
    press(&mut app, &plain(KeyCode::Enter));

    assert_eq!(
        rx.try_recv().expect("the fix reaches the driver"),
        dch_tui::Submission::Correct {
            id: edited_from,
            text: "the prompt under edit — fixed after the fact".to_string(),
        },
        "the fix cannot be written back into a prompt already sent, so it goes \
         out as a correction naming the entry it came from: the driver is the \
         one holding the conversation the superseded wording sits in, so only \
         it can take that turn out and send the fix in its place"
    );
    assert!(
        waiting(&kept).is_empty(),
        "nothing waits in the corrected prompt's place — it is a turn, not \
         a second waiting prompt"
    );
    assert!(app.input().is_empty(), "and the composer is empty again");
}

#[test]
fn a_correction_stops_the_turn_it_names_in_flight() {
    let (mut app, kept, mut rx) = app_with_waiting(&["the prompt being fixed"]);
    let cancels = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&cancels);
    app.set_run_canceller(Box::new(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    let (id, _) = waiting(&kept)[0];
    let _handed_over = run_id(rx.try_recv().expect("the id travels first"));
    claim(&kept, id);
    // The driver names its live claim at the take: the edit's prompt is
    // the turn in flight when the save lands, which is what licenses
    // the cancel below.
    kept.live_claim
        .store(id, std::sync::atomic::Ordering::SeqCst);
    let _rendered = rows(&mut app, 80, 24);
    assert_eq!(
        cancels.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "opening the edit and losing its prompt to the run cancel nothing"
    );
    type_into(&mut app, " — fixed");
    press(&mut app, &plain(KeyCode::Enter));
    assert!(
        matches!(rx.try_recv(), Ok(dch_tui::Submission::Correct { .. })),
        "the save goes out as the correction"
    );
    assert_eq!(
        cancels.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the correction naming the live turn raises the runner's cancel \
         beside its send — the superseded turn is stopped, not waited out"
    );
}

#[test]
fn a_flag_left_up_by_the_settle_window_comes_back_down() {
    // A residual window, closed as far as it can
    // be: a correction read the mirror naming its prompt, and the
    // turn settled and the driver claimed the next one before the
    // cancel landed — the flag was stored against a run it never
    // named. The guard after the cancel re-reads the mirror and
    // stands the flag down when the claim has moved on, so the next
    // run keeps its own error row rather than being silenced as the
    // correction's hand.
    let (mut app, kept, mut rx) = app_with_waiting(&["the prompt being fixed"]);
    let cancels = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&cancels);
    let mirror = Arc::clone(&kept.live_claim);
    app.set_run_canceller(Box::new(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The driver side of the window: between the app's read and
        // the cancel's effect, the named turn settled and the next
        // prompt was claimed — the mirror has moved on.
        mirror.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
    }));
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    let (id, _) = waiting(&kept)[0];
    let _handed_over = run_id(rx.try_recv().expect("the id travels first"));
    claim(&kept, id);
    // The mirror names the edited prompt — the check will match.
    kept.live_claim
        .store(id, std::sync::atomic::Ordering::SeqCst);
    let _rendered = rows(&mut app, 80, 24);
    type_into(&mut app, " — fixed");
    press(&mut app, &plain(KeyCode::Enter));

    assert!(
        matches!(rx.try_recv(), Ok(dch_tui::Submission::Correct { .. })),
        "the save goes out as the correction"
    );
    assert_eq!(
        cancels.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the cancel fired once, against the turn the check named"
    );
    assert_eq!(
        kept.correction_cancel
            .load(std::sync::atomic::Ordering::SeqCst),
        u64::MAX,
        "the guard stood the flag down once the mirror moved on — a \
         wrongly cancelled run keeps its own error row"
    );
}

#[test]
fn a_correction_of_an_already_landed_prompt_leaves_the_run_alone() {
    // The stale arm of the same save: the edit's prompt ran and landed,
    // and a different prompt is now the turn in flight. The correction
    // still goes out — the fix deserves its own turn — but it names a
    // finished turn, so no cancel is raised against the run going and
    // the suppression flag stays empty: that run's fate is not this
    // correction's to decide.
    let (mut app, kept, mut rx) = app_with_waiting(&["the prompt already run"]);
    let cancels = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&cancels);
    app.set_run_canceller(Box::new(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    let (id, _) = waiting(&kept)[0];
    let _handed_over = run_id(rx.try_recv().expect("the id travels first"));
    claim(&kept, id);
    // Its turn landed (the mirror cleared); a later prompt is now the
    // turn in flight, the way a driver one claim ahead leaves it.
    kept.live_claim
        .store(id + 1, std::sync::atomic::Ordering::SeqCst);
    let _rendered = rows(&mut app, 80, 24);
    type_into(&mut app, " — fixed too late");
    press(&mut app, &plain(KeyCode::Enter));

    assert!(
        matches!(
            rx.try_recv(),
            Ok(dch_tui::Submission::Correct { id: sent_to, .. }) if sent_to == id
        ),
        "the fix still goes out as the correction naming its own prompt"
    );
    assert_eq!(
        cancels.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a correction naming a turn that already landed cancels nothing — \
         the run in flight is not its conversation"
    );
    assert_eq!(
        kept.correction_cancel
            .load(std::sync::atomic::Ordering::SeqCst),
        u64::MAX,
        "and the suppression flag stays empty, so the run in flight keeps \
         whatever error row is rightly its own"
    );
}

#[test]
fn the_frame_releases_a_focus_the_claim_emptied() {
    // The half of the rule the edit's survival must not erode: a focus is the
    // section holding keys for a panel that is no longer drawn, so the frame
    // that finds the queue empty takes it back. `e` is the probe — kept
    // focused, the section swallows it; released, it is an ordinary letter
    // landing in the composer, which is what the user sees happening.
    let (mut app, kept, mut rx) = app_with_waiting(&["the last waiting prompt"]);
    let taken = run_id(rx.try_recv().expect("the submission was sent"));
    press(&mut app, &ctrl(KeyCode::Char('q')));
    let _claimed = claim(&kept, taken);
    let rendered = rows(&mut app, 80, 24);
    press(&mut app, &plain(KeyCode::Char('e')));

    assert_eq!(
        app.input(),
        "e",
        "with nothing waiting there is no section to answer the letter, so the \
         composer takes it as text"
    );
    assert!(
        marker_rows(&rendered).is_empty(),
        "and the section is off the screen, which is what makes the focus \
         worthless to keep"
    );
}

#[test]
fn a_correction_the_driver_cannot_take_returns_to_the_composer() {
    // No receiver means no driver: nothing can cut a turn or run a prompt,
    // so the edited wording comes back where the user can still see it
    // rather than being accepted and then thrown away.
    let (mut app, kept, rx) = app_with_waiting(&["the prompt under edit"]);
    let (edited_from, _) = waiting(&kept)[0];
    drop(rx);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    type_into(&mut app, " — fixed too late");
    let _claimed = claim(&kept, edited_from);
    // Production cannot offer the bare sequence above: the claim notifies the
    // loop, and the loop's notify arm draws before the next key is read. The
    // frame is what stands a released mode down, so a correction that survives
    // only when nothing renders is a correction the user never gets.
    let _rendered = rows(&mut app, 80, 24);
    press(&mut app, &plain(KeyCode::Enter));

    assert_eq!(
        app.input(),
        "the prompt under edit — fixed too late",
        "a correction nobody can act on goes back to the composer, the same \
         way a submit whose channel has closed does"
    );
    assert!(
        waiting(&kept).is_empty(),
        "and it queues nothing, because nothing will ever claim it"
    );
}

#[test]
fn a_permission_prompt_outranks_the_queue_chords() {
    let (mut app, kept, _rx) = app_with_waiting(&["waiting under the sheet"]);
    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    requests_tx
        .send(dch_tui::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "allow bash?".to_string(),
            reply: reply_tx,
        })
        .expect("receiver alive");
    app.set_permission_requests(requests_rx);
    app.poll_permission_requests();

    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Delete));

    assert_eq!(
        waiting(&kept).len(),
        1,
        "the sheet owns the keyboard: the chords that would walk and drop a \
         waiting prompt are the sheet's to decline while an ask is up"
    );
}

#[test]
fn the_section_renders_above_the_notice_row_and_never_exceeds_two_rows() {
    let (mut app, _kept, _rx) = app_with_waiting(&["one", "two", "three"]);
    // The notice row is the anchor the section is defined against — the
    // furniture below it is pinned by the suite's other layout tests.
    app.post_notice("the notice anchor".to_string());

    let rendered = rows(&mut app, 80, 24);
    let notice = row_holding(&rendered, "notice anchor");
    let marked = marker_rows(&rendered);

    assert_eq!(
        marked,
        vec![notice - 1],
        "the marker rides the section's prompt row with the notice row directly \
         below: the section ends at its own last row — no blank is held beneath \
         a wording that ended early"
    );
    let shown = rendered[marked[0]].clone();
    let word = shown
        .split_whitespace()
        .last()
        .expect("the shown prompt's wording is on its own row")
        .to_string();
    let carriers = rendered.iter().filter(|row| row.contains(&word)).count();
    assert!(
        carriers <= 2,
        "three prompts waiting cost the same two rows one does — the rows come \
         out of the conversation, so a backlog cannot eat the screen"
    );
    assert!(
        rendered[..marked[0]].iter().all(|row| !row.contains(&word)),
        "and none of it spills upward into the transcript"
    );
}

#[test]
fn an_empty_queue_costs_no_rows_at_all() {
    let (mut app, _kept, _rx) = app_with_waiting(&[]);
    app.post_notice("the notice anchor".to_string());
    let empty = rows(&mut app, 80, 24);

    assert!(
        marker_rows(&empty).is_empty(),
        "no panel is drawn when nothing waits: an empty section would be a \
         block of nothing above the composer"
    );

    let (mut queued, _kept2, _rx2) = app_with_waiting(&["one waiting prompt"]);
    queued.post_notice("the notice anchor".to_string());
    let with_queue = rows(&mut queued, 80, 24);

    assert_eq!(
        row_holding(&with_queue, "notice anchor"),
        row_holding(&empty, "notice anchor"),
        "the notice, strip, composer, and status bar hold their rows whatever \
         the queue holds — the section pays for itself out of the conversation"
    );
}

#[test]
fn a_long_prompt_wraps_across_both_rows_and_marks_the_cut() {
    let long = "wrap ".repeat(60);
    let (mut app, _kept, _rx) = app_with_waiting(&[long.trim()]);

    let rendered = rows(&mut app, 80, 24);
    let marked = marker_rows(&rendered);
    assert_eq!(marked.len(), 1, "the marker rides the section's first row");
    let body = rendered[marked[0]..marked[0] + 2].join(" ");
    assert!(
        body.contains("wrap"),
        "the prompt's wording is on screen, not a bare count: {rendered:?}"
    );
    assert!(
        rendered[marked[0] + 1].contains('…'),
        "the second row ends at the cut the wrapper made, so what the section \
         could not fit reads as unfitted rather than as the whole prompt: \
         {rendered:?}"
    );
}

#[test]
fn the_section_numbers_the_prompt_and_names_the_count() {
    let (mut app, _kept, _rx) = app_with_waiting(&["first", "second", "third"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    let at_the_front = rows(&mut app, 80, 24);
    assert!(
        at_the_front.iter().any(|row| row.contains("[1/3]")),
        "the shown prompt's place in the send order, out of how many wait: \
         {at_the_front:?}"
    );

    press(&mut app, &plain(KeyCode::Right));
    let one_along = rows(&mut app, 80, 24);
    assert!(
        one_along.iter().any(|row| row.contains("[2/3]")),
        "and the number moves with the walk: {one_along:?}"
    );
}

#[test]
fn the_focused_section_shows_its_chords_and_the_plain_one_does_not() {
    let (mut app, _kept, _rx) = app_with_waiting(&["one waiting prompt"]);

    let unfocused = rows(&mut app, 80, 24);
    assert!(
        unfocused.iter().any(|row| row.contains("ctrl+q")),
        "unfocused, the row offers the way in: {unfocused:?}"
    );
    assert!(
        !unfocused.iter().any(|row| row.contains("drop")),
        "and promises nothing it would not do from here"
    );

    press(&mut app, &ctrl(KeyCode::Char('q')));
    let focused = rows(&mut app, 80, 24);
    assert!(
        focused.iter().any(|row| row.contains("walk")),
        "focused, it names the keys it answers — including the way out: \
         {focused:?}"
    );
    assert!(
        !focused.iter().any(|row| row.contains("ctrl+q")),
        "and stops advertising the chord the user already pressed"
    );
}

#[test]
fn a_narrow_terminal_keeps_the_hint_beside_the_badge_off_the_text() {
    let (mut app, _kept, _rx) = app_with_waiting(&["a prompt that wants room"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    let narrow = rows(&mut app, 28, 24);
    let marked = marker_rows(&narrow);
    let body = narrow
        .get(marked[0]..marked[0] + 2)
        .map(<[String]>::to_vec)
        .unwrap_or_default()
        .join(" ");

    assert!(
        body.contains("prompt") && body.contains("room"),
        "the prompt keeps its words at a width where they cannot all fit: \
         {narrow:?}"
    );
    // The chords ride beside the badge now, not on the prompt's rows:
    // at a narrow width the hint's head stays named on the badge row —
    // clipping with that row, never colliding with the wording below —
    // where the old right-edge placement vanished the moment the
    // wrapped wording filled its row.
    let badge_row = narrow
        .iter()
        .find(|row| row.contains("QUEUED"))
        .expect("the badge renders");
    assert!(
        badge_row.contains("walk"),
        "the chords stay named beside the badge, clipped not dropped: \
         {badge_row:?}"
    );
    assert!(
        !body.contains("walk"),
        "the hint never spends the prompt's columns: {body:?}"
    );
}

#[test]
fn the_waiting_count_is_the_queue_section_s_own() {
    let (mut app, _kept, _rx) = app_with_waiting(&["one", "two", "three"]);

    let rendered = rows(&mut app, 100, 24);

    // The queue counts itself in its own section — the marker row and
    // the QUEUED title — and no other surface repeats the count as
    // loose text: not the composer's identity row, not the footer's
    // right edge, which carries no state text in any case.
    assert!(
        rendered.iter().any(|row| row.contains("[1/3]")),
        "the queue's own section counts what waits: {rendered:?}"
    );

    // The badge rides flush with the rows it heads: the pill's left
    // fill edge shares the marker's column, not a stand-off to the
    // right of it.
    let badge = rendered
        .iter()
        .find(|row| row.contains("QUEUED"))
        .expect("the badge renders");
    let marker = rendered
        .iter()
        .find(|row| row.contains("[1/3]"))
        .expect("the marker row renders");
    let pill_edge = rendered
        .iter()
        .find(|row| row.contains("QUEUED"))
        .and_then(|row| row.find("QUEUED"))
        .map_or(usize::MAX, |byte| {
            badge[..byte].chars().count().saturating_sub(1)
        });
    let marker_col = marker
        .find(HOURGLASS)
        .map_or(usize::MAX, |byte| marker[..byte].chars().count());
    assert_eq!(
        pill_edge, marker_col,
        "the QUEUED pill aligns with the marker column — left, not stood off"
    );
    assert!(
        rendered.iter().any(|row| row.contains("QUEUED")),
        "the section titles itself: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|row| row.contains("queued")),
        "no row anywhere spells the count as tag text: {rendered:?}"
    );
    let footer = &rendered[rendered.len() - 3..];
    assert!(
        footer
            .iter()
            .all(|row| row.trim().is_empty() || row.trim_start().starts_with("0k/200k")),
        "the footer's rows carry the readout and nothing else: {footer:?}"
    );
}

#[test]
fn a_run_ending_leaves_the_waiting_prompts_alone() {
    let (mut app, kept, mut rx) = app_with_waiting(&["runs now", "waits after", "waits last"]);
    let first = run_id(rx.try_recv().expect("the first submission"));
    let text = claim(&kept, first).expect("the front prompt");
    kept.push_sent(text);
    kept.graduations.lock().unwrap().push(Graduation::Reply {
        text: "an answer".to_string(),
        thinking: None,
    });
    kept.agent_running.store(false, Ordering::SeqCst);

    rows(&mut app, 80, 24);

    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["waits after", "waits last"],
        "a run finishing takes the prompt it ran and nothing else: the rest \
         wait their turn, in the order they were sent"
    );
    assert_eq!(
        app.conversation().len(),
        2,
        "the claim's row and the reply it produced"
    );
}

#[test]
fn the_section_renders_on_a_theme_that_paints_no_surface() {
    let (mut app, _kept, _rx) = app_with_waiting(&["a prompt on a paintless theme"]);
    // The adaptation a session falls back to when the terminal never
    // answered the background query: the canvas colors go back to the
    // terminal, and nothing on screen may lean on a filled panel.
    app.theme = app.theme.deferred_to_terminal();

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let rendered: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        })
        .collect();
    let marked = marker_rows(&rendered);
    assert_eq!(marked.len(), 1, "the marker still rides its one row");
    let body = rendered[marked[0]..marked[0] + 2].join(" ");
    assert!(
        body.contains("paintless"),
        "with its text across both of its rows, whatever the palette: \
         {rendered:?}"
    );

    // And it paints nothing behind it: both of its cells carry the same canvas
    // the conversation carries, rather than a raised block of the theme's own.
    let section_row = u16::try_from(marked[0]).unwrap();
    let section_bg = buffer[(2, section_row)].style().bg;
    let held_row_bg = buffer[(2, section_row + 1)].style().bg;
    let conversation_bg = buffer[(2, 1)].style().bg;
    assert_eq!(
        held_row_bg, conversation_bg,
        "the row the short prompt leaves empty is still the canvas, not a panel"
    );
    assert_eq!(
        section_bg, conversation_bg,
        "the section shares the terminal's canvas — a filled panel would put \
         the theme's surface into a terminal configured to paint none"
    );
}

/// The kind of each transcript row, oldest first.
///
/// Row order is what these tests are about, and a row's kind is the
/// only thing that says where it came from — the text alone cannot
/// tell a prompt from the answer that followed it.
fn kinds(app: &TuiApp) -> Vec<&'static str> {
    app.conversation()
        .iter()
        .map(|row| match row {
            TuiMessage::User { .. } => "user",
            TuiMessage::Assistant { .. } => "assistant",
            _ => "other",
        })
        .collect()
}

#[test]
fn a_prompt_claimed_after_an_answer_lands_below_it() {
    // The inversion this pins is a scheduling accident that happens all
    // by itself: a run's reply graduates partway through the run, the
    // next prompt's echo is recorded when that run resolves, and frames
    // in the background are capped at the render interval — so a single
    // drain routinely finds one of each. Drained from a buffer of its
    // own, the newer prompt is emptied first and lands above the older
    // answer, and the misorder is written into every save after it.
    let (mut app, kept, mut rx) = app_with_waiting(&["asks again"]);
    let id = run_id(rx.try_recv().expect("the submission was sent"));
    kept.graduations.lock().unwrap().push(Graduation::Reply {
        text: "an answer to the prompt before it".to_string(),
        thinking: None,
    });
    let text = claim(&kept, id).expect("the queue held the prompt");
    kept.push_sent(text);

    rows(&mut app, 80, 24);

    assert_eq!(
        kinds(&app),
        ["assistant", "user"],
        "the answer that happened first keeps the row it happened in"
    );
}

#[test]
fn rows_keep_the_order_they_happened_in_however_they_arrive() {
    // The general case of the one above: prompt, answer, prompt, written
    // into the queue in the order they occurred. Total order is push
    // order, so the transcript cannot be assembled by kind — which is
    // what a per-kind buffer would do however the events interleaved.
    let (mut app, kept, mut rx) = app_with_waiting(&["first ask", "second ask"]);
    let first = run_id(rx.try_recv().expect("the first submission"));
    let second = run_id(rx.try_recv().expect("the second submission"));
    let first_text = claim(&kept, first).expect("the front prompt");
    kept.push_sent(first_text);
    kept.graduations.lock().unwrap().push(Graduation::Reply {
        text: "the first answer".to_string(),
        thinking: None,
    });
    let second_text = claim(&kept, second).expect("the next prompt");
    kept.push_sent(second_text);

    rows(&mut app, 80, 24);

    assert_eq!(
        kinds(&app),
        ["user", "assistant", "user"],
        "a conversation is assembled by what happened when, not by the kind \
         of each row"
    );
    assert_eq!(
        app.conversation()
            .iter()
            .filter_map(|row| match row {
                TuiMessage::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        ["first ask", "second ask"],
        "and the two prompts keep their own order within it"
    );
}

#[test]
fn ctrl_q_hands_the_keys_back_from_a_focused_section() {
    // The chord is a toggle: the press that enters takes the keys back
    // from focus too, mirroring Esc. The ownership check is `e` — with
    // the section it lifts the entry for editing, with the composer it
    // is only a letter, and after the second chord it must be only a
    // letter again.
    let (mut app, _kept, _rx) = app_with_waiting(&["first waiting"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    press(&mut app, &ctrl(KeyCode::Char('q')));

    press(&mut app, &plain(KeyCode::Char('e')));
    assert_eq!(
        app.input(),
        "e",
        "the second Ctrl+Q gave the keys back: `e` typed into the composer \
         instead of opening the entry for editing"
    );
}

#[test]
fn esc_is_the_way_out_of_a_focused_section() {
    // The other half of the same sentence: leaving is Esc's. Released, the
    // composer takes its keys back and `e` is only a letter again.
    let (mut app, kept, _rx) = app_with_waiting(&["first waiting"]);
    press(&mut app, &ctrl(KeyCode::Char('q')));

    press(&mut app, &plain(KeyCode::Esc));

    press(&mut app, &plain(KeyCode::Char('e')));
    assert_eq!(
        app.input(),
        "e",
        "the composer has the keys back, so `e` typed rather than opening an \
         edit"
    );
    assert_eq!(
        waiting(&kept)
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["first waiting"],
        "and releasing the keys leaves the waiting prompt exactly where it was"
    );
}

#[test]
fn a_submit_after_the_driver_is_gone_names_the_driver() {
    // The notice is the user's only account of where the words went, so it
    // has to name the thing they know. "Channel" is the pipe behind the
    // session, not a word the README or the user ever uses for it; the
    // sibling notice on this same path already says driver.
    let (mut app, _kept, rx) = app_with_waiting(&[]);
    drop(rx);

    submit(&mut app, "typed as the session ended");

    let rendered = rows(&mut app, 100, 24);
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("the driver has gone")),
        "the row says the driver is what left: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|row| row.contains("channel")),
        "and it says it in the user\'s words, not the mechanism\'s: {rendered:?}"
    );
}

#[test]
fn the_chord_that_enters_also_leaves() {
    // Ctrl+Q is a toggle: the press that hands the section the keys is
    // the press that takes them back, mirroring Esc — a key that opens
    // a door it then refuses to close reads broken, and the user who
    // entered on the chord reaches for the same chord to leave.
    let (mut app, _kept, _rx) = app_with_waiting(&["first waiting"]);
    let marker_fg = |app: &mut TuiApp| {
        let painted = painted_rows(app, 80, 24);
        painted
            .iter()
            .flatten()
            .find(|(symbol, _, _)| symbol == "\u{231b}")
            .map(|(_, fg, _)| *fg)
            .expect("the waiting prompt carries the hourglass marker")
    };

    let plain = marker_fg(&mut app);
    press(&mut app, &ctrl(KeyCode::Char('q')));
    assert_eq!(
        marker_fg(&mut app),
        app.theme.ui.primary,
        "the chord hands the section the keys — the marker lights"
    );

    press(&mut app, &ctrl(KeyCode::Char('q')));
    assert_eq!(
        marker_fg(&mut app),
        plain,
        "the same chord takes them back — the marker settles to dim, \u{2026}"
    );

    // And back in again: the toggle keeps working in both directions.
    press(&mut app, &ctrl(KeyCode::Char('q')));
    assert_eq!(
        marker_fg(&mut app),
        app.theme.ui.primary,
        "the chord re-enters — a toggle, not a one-way door"
    );
}
