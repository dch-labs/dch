//! Steering a run already in flight: `Ctrl+S`, and the guidance that comes back.
//!
//! The queue's contract is patience; this one is the opposite — text
//! reaching a loop that is mid-turn, through a slot the app holds a handle
//! to but does not own. So the fake below is deliberately two-valued: a slot
//! that takes offers and a slot that refuses them, because the whole
//! interesting half of this seam is what happens when an offer lands the
//! instant a run ends and has to come back the other way. The fake also
//! records, so a test can tell "the text was sent" from "the text was
//! typed", which is the distinction the transcript has to get right.

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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dch_config::DchConfig;
use dch_tui::Guidance;
use dch_tui::TuiApp;
use dch_tui::message::TuiMessage;
use dch_tui::observer::TuiObserver;
use loopctl::observer::{LoopObserver, TurnStartContext};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A steering slot a test can hold both ends of.
#[derive(Default)]
struct FakeGuidance {
    /// Whether a run is in flight to take an offer.
    live: AtomicBool,

    /// Everything offered, in the order it arrived.
    offered: Mutex<Vec<String>>,

    /// What the runs before this one never reached, given out on request.
    owed: Mutex<Vec<String>>,

    /// The host's "a run is in flight" flag, and whether the next take flips
    /// it on. Stand-in for the window the sweep cannot close from its side:
    /// the check happens, then a run starts, then the take runs.
    start_run_on_take: AtomicBool,
    running: Mutex<Option<Arc<AtomicBool>>>,

    /// Whether the run that starts has already passed the boundary where it
    /// would read guidance, so an offer back to it is refused.
    sealed_on_start: AtomicBool,
}

impl FakeGuidance {
    /// A slot with `texts` left over from an earlier run.
    fn returned(texts: &[&str]) -> Arc<Self> {
        let fake = Arc::new(Self::default());
        *fake.owed.lock().unwrap() = texts.iter().map(|text| (*text).to_string()).collect();
        fake
    }
}

impl FakeGuidance {
    /// Arm this slot so the next take starts a run, as the driver would — and
    /// from that moment the run answers offers, since a run mid-turn reads
    /// guidance at its next boundary.
    fn start_run_when_taken(&self, running: &Arc<AtomicBool>) -> &Self {
        *self.running.lock().unwrap() = Some(Arc::clone(running));
        self.start_run_on_take.store(true, Ordering::SeqCst);
        self
    }

    /// Say the run that starts has already sealed its boundary, so it refuses
    /// anything offered back to it.
    fn sealed_on_start(&self) -> &Self {
        self.sealed_on_start.store(true, Ordering::SeqCst);
        self
    }
}

impl Guidance for FakeGuidance {
    fn steer(&self, text: String) -> bool {
        if !self.live.load(Ordering::SeqCst) {
            return false;
        }
        self.offered.lock().unwrap().push(text);
        true
    }

    fn take_returned(&self) -> Vec<String> {
        if self.start_run_on_take.swap(false, Ordering::SeqCst) {
            if let Some(flag) = self.running.lock().unwrap().as_ref() {
                flag.store(true, Ordering::SeqCst);
            }
            let sealed = self.sealed_on_start.load(Ordering::SeqCst);
            self.live.store(!sealed, Ordering::SeqCst);
        }
        std::mem::take(&mut *self.owed.lock().unwrap())
    }
}

fn config() -> DchConfig {
    let mut config = DchConfig::default();
    config.display.theme = "dracula".to_string();
    config.api.model = "testmodel".to_string();
    config
}

/// An app wired for steering: shared state, a live submit channel, a slot.
fn app_steerable() -> (
    TuiApp,
    dch_tui::TuiObserverState,
    tokio::sync::mpsc::UnboundedReceiver<dch_tui::Submission>,
    Arc<FakeGuidance>,
) {
    let (app, kept, rx, guidance, _observer) = app_steerable_with_observer();
    (app, kept, rx, guidance)
}

/// An app wired for steering, the observer kept live beside it.
///
/// The real loop fires the turn boundaries the display graduates pins
/// at; a test that cares about that moment stands in for the loop with
/// the same cue, so the kept observer is the harness's loop.
fn app_steerable_with_observer() -> (
    TuiApp,
    dch_tui::TuiObserverState,
    tokio::sync::mpsc::UnboundedReceiver<dch_tui::Submission>,
    Arc<FakeGuidance>,
    TuiObserver,
) {
    let (observer, kept) = dch_tui::TuiObserverState::new().into_observer();
    let mut app = TuiApp::from_observer_state(config(), kept.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_submit_tx(tx);
    let guidance = Arc::new(FakeGuidance::default());
    app.set_guidance(Some(Arc::<FakeGuidance>::clone(&guidance)));
    (app, kept, rx, guidance, observer)
}

/// Hand the app the same allocation the test holds a handle to.
fn clone_guidance(fake: &Arc<FakeGuidance>) -> Arc<dyn Guidance> {
    Arc::clone(fake) as Arc<dyn Guidance>
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

fn press(app: &mut TuiApp, event: &Event) {
    let _verdict = app.handle_event(event);
}

fn type_into(app: &mut TuiApp, text: &str) {
    for ch in text.chars() {
        press(app, &plain(KeyCode::Char(ch)));
    }
}

fn submit(app: &mut TuiApp, text: &str) {
    type_into(app, text);
    press(app, &plain(KeyCode::Enter));
}

/// Steer `text`: the composer's chord, whatever mode the keys are in.
fn steer(app: &mut TuiApp, text: &str) {
    type_into(app, text);
    assert!(
        app.handle_event(&ctrl(KeyCode::Char('s'))),
        "the chord is consumed whether or not a run takes the text"
    );
}

/// What the shared queue holds, as texts in send order.
fn waiting(kept: &dch_tui::TuiObserverState) -> Vec<String> {
    let queue = kept.queued.lock().unwrap();
    (0..queue.len())
        .filter_map(|index| queue.get(index))
        .map(|entry| entry.text.clone())
        .collect()
}

/// The conversation's user rows, in order.
fn user_rows(app: &TuiApp) -> Vec<String> {
    app.conversation()
        .iter()
        .filter_map(|message| match message {
            TuiMessage::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Set whether a run is in flight, as the observer would.
fn running(kept: &dch_tui::TuiObserverState, value: bool) {
    kept.agent_running.store(value, Ordering::SeqCst);
}

/// The steers a run accepted but no boundary has read, in send order.
fn pinned(kept: &dch_tui::TuiObserverState) -> Vec<String> {
    kept.pending_steers.lock().unwrap().clone()
}

/// Fire the turn boundary the loop emits once it has drained its slot.
///
/// The loop emits this after draining its steering slot into the
/// turn's request and before sending it; the harness has no loop, so
/// the test fires the same cue at the same moment.
fn boundary(observer: &TuiObserver) {
    observer.on_turn_start(&TurnStartContext {
        turn: 1,
        query: String::new(),
    });
}

/// Render a frame and hand back its rows as plain strings.
///
/// Rendering is also what makes the app take from the shared lanes, so a
/// test that cares about a sweep renders first.
fn rows(app: &mut TuiApp, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect()
}

#[test]
fn a_steered_prompt_waits_at_the_edge_until_the_turn_reads_it() {
    // Acceptance is not reading. The text goes to the slot — the model
    // meets it at the run's next boundary — and the transcript row is
    // not written at the key: the model has not read the words, and a
    // row would claim it had. The words wait pinned below the live
    // lane, in the user's own styling, until the boundary drains them,
    // and graduate into the conversation there — after the tool or
    // thinking that was running when the key went down.
    let (mut app, kept, mut rx, guidance, observer) = app_steerable_with_observer();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);

    steer(&mut app, "the file it is reading is generated");

    assert_eq!(
        *guidance.offered.lock().unwrap(),
        ["the file it is reading is generated"],
        "the text went to the slot, not to a new turn"
    );
    assert!(waiting(&kept).is_empty(), "nothing is waiting to run");
    assert!(
        app.input().is_empty(),
        "the composer is empty again after the send"
    );
    assert!(
        rx.try_recv().is_err(),
        "no prompt was handed over for a turn"
    );
    let frame = rows(&mut app, 80, 12);
    assert!(
        user_rows(&app).is_empty(),
        "the model has not read the words, so the transcript holds no row"
    );
    assert_eq!(
        pinned(&kept),
        ["the file it is reading is generated"],
        "the row waits at the transcript's edge instead"
    );
    assert!(
        frame
            .iter()
            .any(|row| row.contains("the file it is reading")),
        "and the waiting words are on screen, in the user's own row: {frame:?}"
    );

    boundary(&observer);
    rows(&mut app, 80, 12);
    assert_eq!(
        user_rows(&app),
        ["the file it is reading is generated"],
        "the boundary that reads the words graduates the row into the \
         conversation — once, with the pin emptied"
    );
    assert!(
        pinned(&kept).is_empty(),
        "the pin empties with the graduation, so the row cannot land twice"
    );
}

#[test]
fn a_refused_offer_takes_the_ordinary_path_instead() {
    let (mut app, kept, mut rx, guidance) = app_steerable();
    // No run to steer: the slot answers every offer with `false`.
    running(&kept, true);

    steer(&mut app, "keep going");

    assert!(
        guidance.offered.lock().unwrap().is_empty(),
        "the refusal means the slot never saw the text"
    );
    assert_eq!(
        waiting(&kept),
        ["keep going"],
        "so the prompt queues like any other"
    );
    assert!(user_rows(&app).is_empty(), "an unrun prompt has no row yet");
    assert!(rx.try_recv().is_ok(), "and the driver is told to run it");
}

#[test]
fn a_steered_prompt_is_recallable_like_any_submission() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);

    steer(&mut app, "use the staging branch");
    press(&mut app, &plain(KeyCode::Up));

    assert_eq!(
        app.input(),
        "use the staging branch",
        "recall lists the prompts actually sent, however they were sent"
    );
}

#[test]
fn steering_a_prompt_out_for_editing_retires_its_slot() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    submit(&mut app, "first waiting");
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    assert_eq!(app.input(), "first waiting", "the prompt is in hand");

    assert!(app.handle_event(&ctrl(KeyCode::Char('s'))));

    assert_eq!(
        *guidance.offered.lock().unwrap(),
        ["first waiting"],
        "the words in hand go to the model now"
    );
    assert!(
        waiting(&kept).is_empty(),
        "and the waiting slot is retired with them, or the same words would \
         run again later"
    );
    rows(&mut app, 80, 12);
    assert_eq!(
        pinned(&kept),
        ["first waiting"],
        "the steered words wait at the edge like any steer — the \
         transcript row comes when the turn reads them"
    );
}

#[test]
fn guidance_a_run_never_reached_comes_back_once_the_slot_is_idle() {
    let (mut app, kept, mut rx, _default_slot) = app_steerable();
    let guidance = FakeGuidance::returned(&["scratch that", "not that file"]);
    app.set_guidance(Some(guidance));

    // While a run is in flight its guidance belongs to that run: taking it
    // out mid-run would relieve the loop of something it had not read yet.
    running(&kept, true);
    let _frame = rows(&mut app, 80, 24);
    assert!(
        waiting(&kept).is_empty(),
        "nothing is lifted out from under a running loop"
    );

    running(&kept, false);
    let frame = rows(&mut app, 80, 24);
    assert_eq!(
        waiting(&kept),
        ["scratch that", "not that file"],
        "in order, back where the user can read, fix, or drop them"
    );
    // The section's promise is that what stands in it runs, so a returned
    // prompt comes back with its turn promised too — the send the user
    // asked for, delivered as a turn of its own rather than deposited.
    let mut ids = Vec::new();
    while let Ok(submission) = rx.try_recv() {
        match submission {
            dch_tui::Submission::Run { id } => ids.push(id),
            other @ dch_tui::Submission::Correct { .. } => {
                panic!("a returned prompt is handed over as a run, got {other:?}")
            }
        }
    }
    assert_eq!(ids.len(), 2, "one turn promised per returned prompt");
    assert!(
        frame.iter().any(|row| row.contains("waiting again")),
        "and the notice row says where they came from, rather than a prompt \
         appearing unexplained"
    );
}

#[test]
fn an_empty_composer_sends_nothing() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);

    assert!(app.handle_event(&ctrl(KeyCode::Char('s'))));

    assert!(guidance.offered.lock().unwrap().is_empty());
    assert!(waiting(&kept).is_empty());
    assert!(user_rows(&app).is_empty());
}

#[test]
fn guidance_from_a_finished_run_waits_again_even_as_the_next_run_starts() {
    // The sweep's idle check and its take are two steps, and a run starts
    // between them. The texts in hand are the finished run's — advice
    // about turns that no longer stand — and the slot's own contract
    // refuses to hand them to the new run silently: they wait again,
    // with a turn promised, under the notice that says the model never
    // read them. A user who meant them for the new run can still send
    // them, seeing the notice.
    let (mut app, kept, mut rx, _default_slot) = app_steerable();
    let guidance = FakeGuidance::returned(&["aimed during the window"]);
    guidance.start_run_when_taken(&kept.agent_running);
    app.set_guidance(Some(clone_guidance(&guidance)));

    running(&kept, false);
    let frame = rows(&mut app, 80, 24);

    assert!(
        guidance.offered.lock().unwrap().is_empty(),
        "the new run was not steered with the finished run's words"
    );
    assert_eq!(
        waiting(&kept),
        ["aimed during the window"],
        "it waits again with a turn promised rather than riding the new run"
    );
    let mut ids = Vec::new();
    while let Ok(submission) = rx.try_recv() {
        if let dch_tui::Submission::Run { id } = submission {
            ids.push(id);
        }
    }
    assert_eq!(ids.len(), 1, "one turn promised");
    assert!(
        frame.iter().any(|row| row.contains("never read")),
        "the notice says where it went: {frame:?}"
    );
}

#[test]
fn guidance_a_run_started_over_and_cannot_take_still_waits() {
    // The other arm of the same re-check: the run that started has already
    // sealed its boundary, so the offer is refused and the text has to land
    // where guidance goes when no run can read it.
    let (mut app, kept, mut rx, _default_slot) = app_steerable();
    let guidance = FakeGuidance::returned(&["steered a heartbeat too late"]);
    guidance
        .start_run_when_taken(&kept.agent_running)
        .sealed_on_start();
    app.set_guidance(Some(clone_guidance(&guidance)));

    running(&kept, false);
    let frame = rows(&mut app, 80, 24);

    assert_eq!(
        waiting(&kept),
        ["steered a heartbeat too late"],
        "refused, it waits with a turn promised rather than vanishing"
    );
    let mut ids = Vec::new();
    while let Ok(submission) = rx.try_recv() {
        if let dch_tui::Submission::Run { id } = submission {
            ids.push(id);
        }
    }
    assert_eq!(ids.len(), 1, "one turn promised");
    assert!(
        frame.iter().any(|row| row.contains("waiting again")),
        "and the notice row says where it came from: {frame:?}"
    );
}

#[test]
fn a_draft_of_nothing_but_blanks_steers_nothing() {
    // Enter refuses a draft whose content is all blanks; the chord claims to
    // send what Enter would send, so it has to refuse the same draft. Left
    // unrefused, whitespace earns a transcript row and a recall entry, and the
    // blanks the user was about to overwrite are gone.
    let (mut app, kept, mut rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);

    steer(&mut app, "   ");

    assert!(
        guidance.offered.lock().unwrap().is_empty(),
        "nothing was offered to the run in flight"
    );
    assert_eq!(
        app.input(),
        "   ",
        "the refusal leaves the draft where it was, as Enter's does"
    );
    assert!(
        rx.try_recv().is_err(),
        "and no turn was asked for in its place"
    );
    press(&mut app, &plain(KeyCode::Up));
    assert_eq!(
        app.input(),
        "   ",
        "recall holds no phantom entry for the refused chord — Up walks the \
         prompts that were sent"
    );
}

#[test]
fn a_blank_draft_from_an_edit_leaves_the_waiting_prompt_waiting() {
    // The expensive half: from an edit the chord retires the waiting slot
    // before delivering, so a blank draft steering from here would delete a
    // real prompt and send whitespace in its place.
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    submit(&mut app, "a prompt worth keeping");
    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('e')));
    for _ in 0.."a prompt worth keeping".len() {
        press(&mut app, &plain(KeyCode::Backspace));
    }
    type_into(&mut app, "  ");

    assert!(app.handle_event(&ctrl(KeyCode::Char('s'))));

    assert_eq!(
        waiting(&kept),
        ["a prompt worth keeping"],
        "the prompt keeps its slot — nothing was sent to replace it"
    );
    assert!(
        guidance.offered.lock().unwrap().is_empty(),
        "whitespace reaches no model"
    );
    assert_eq!(
        app.input(),
        "  ",
        "and the edit is still open on its prompt, blanks and all"
    );
}

#[test]
fn s_guides_a_prompt_that_is_already_waiting() {
    let (mut app, kept, mut rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    // Both prompts went the patient way: Enter, queued, waiting their turn.
    submit(&mut app, "check the tests");
    submit(&mut app, "then the docs");

    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('s')));
    press(&mut app, &plain(KeyCode::Char('y')));

    assert_eq!(
        *guidance.offered.lock().unwrap(),
        ["check the tests"],
        "the shown prompt joins the turn in flight instead of waiting for it"
    );
    assert_eq!(
        waiting(&kept),
        ["then the docs"],
        "it leaves the queue as it goes, or the same words would also run \
         later as a turn"
    );
    rows(&mut app, 80, 12);
    assert_eq!(
        pinned(&kept),
        ["check the tests"],
        "the guided prompt waits at the edge for the turn to read it"
    );
    assert!(
        rx.try_recv().is_ok() && rx.try_recv().is_ok(),
        "the two queued prompts keep the turns they were promised, and the \
         guided one asked for no extra one"
    );
    assert!(
        rx.try_recv().is_err(),
        "exactly two submissions are in flight: guiding does not add a third"
    );
}

#[test]
fn s_refuses_to_guide_when_nothing_is_running() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    // A slot that would take the text: the refusal has to come from the
    // app's own gate, not from an empty slot. The waiting prompt is
    // seeded the way a resume seeds owed prompts — an idle submit's
    // row hides for its handoff window, and this harness has no
    // driver to retire it, while a seeded prompt genuinely waits.
    guidance.live.store(true, Ordering::SeqCst);
    kept.queued
        .lock()
        .unwrap()
        .push("queued while idle".to_string());

    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('s')));
    press(&mut app, &plain(KeyCode::Char('y')));
    let frame = rows(&mut app, 80, 24);

    assert!(
        guidance.offered.lock().unwrap().is_empty(),
        "between turns there is no turn to guide"
    );
    assert_eq!(
        waiting(&kept),
        ["queued while idle"],
        "and the prompt stays where it was, in its place in the order"
    );
    assert!(
        frame.iter().any(|row| row.contains("nothing is running")),
        "with the reason on the notice row"
    );
}

#[test]
fn s_guides_the_prompt_on_screen_not_the_one_behind_it() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    submit(&mut app, "first in order");
    submit(&mut app, "second in order");

    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Right));
    press(&mut app, &plain(KeyCode::Char('s')));
    press(&mut app, &plain(KeyCode::Char('y')));

    assert_eq!(
        *guidance.offered.lock().unwrap(),
        ["second in order"],
        "the chord guides what is on screen, not what happens to be first"
    );
    assert_eq!(
        waiting(&kept),
        ["first in order"],
        "and the prompt behind it keeps its place, still running in order"
    );
}

#[test]
fn guiding_the_last_waiting_prompt_gives_the_keys_back() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    submit(&mut app, "the only one waiting");

    press(&mut app, &ctrl(KeyCode::Char('q')));
    press(&mut app, &plain(KeyCode::Char('s')));
    press(&mut app, &plain(KeyCode::Char('y')));
    type_into(&mut app, "z");

    assert_eq!(*guidance.offered.lock().unwrap(), ["the only one waiting"]);
    assert!(waiting(&kept).is_empty(), "the section is empty now");
    assert_eq!(
        app.input(),
        "z",
        "with no panel left to hold them, the keys return to the composer"
    );
}

#[test]
fn a_multi_line_draft_is_steered_whole() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    type_into(&mut app, "stop before the deploy");
    press(&mut app, &ctrl(KeyCode::Char('j')));
    type_into(&mut app, "it restarts the service I am logged into");

    assert!(app.handle_event(&ctrl(KeyCode::Char('s'))));

    assert_eq!(
        *guidance.offered.lock().unwrap(),
        ["stop before the deploy\nit restarts the service I am logged into"],
        "the whole draft goes, newlines and all, exactly as Enter would send it"
    );
    assert!(waiting(&kept).is_empty());
}

#[test]
fn one_returned_prompt_is_reported_in_the_singular() {
    let (mut app, kept, _rx, _slot) = app_steerable();
    let guidance = FakeGuidance::returned(&["advice that arrived too late"]);
    app.set_guidance(Some(guidance));
    running(&kept, false);

    let frame = rows(&mut app, 80, 24);

    assert_eq!(waiting(&kept), ["advice that arrived too late"]);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("1 prompt the model never read is waiting again")),
        "the notice counts in the same words a person would use: {frame:?}"
    );
}

#[test]
fn returned_guidance_with_no_driver_says_it_cannot_run() {
    let (mut app, kept, rx, _slot) = app_steerable();
    let guidance = FakeGuidance::returned(&["words with nowhere to go"]);
    app.set_guidance(Some(guidance));
    drop(rx);
    running(&kept, false);

    let frame = rows(&mut app, 80, 24);

    assert_eq!(
        waiting(&kept),
        ["words with nowhere to go"],
        "a closed channel does not swallow the text: it stays where it can be read"
    );
    assert!(
        frame.iter().any(|row| row.contains("no driver left")),
        "and the notice admits it will never run: {frame:?}"
    );
}

#[test]
fn the_hint_names_the_chord_exactly_when_it_would_do_something() {
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);

    // The moment that looks like typing an ordinary prompt is the moment the
    // impatient chord is worth naming.
    type_into(&mut app, "wait, the service is mine");
    let frame = rows(&mut app, 90, 24);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("ctrl+s sends this to the running turn")),
        "the hint shows with a draft and a run in flight: {frame:?}"
    );

    // Idle, the same words would only queue — so the row says nothing.
    running(&kept, false);
    let frame = rows(&mut app, 90, 24);
    assert!(
        !frame.iter().any(|row| row.contains("ctrl+s")),
        "a hint for a key that would only queue is noise: {frame:?}"
    );

    // No slot installed: no chord to advertise either.
    running(&kept, true);
    let slot: Option<Arc<dyn dch_tui::Guidance>> = None;
    app.set_guidance(slot);
    let frame = rows(&mut app, 90, 24);
    assert!(
        !frame.iter().any(|row| row.contains("ctrl+s")),
        "a session that cannot interrupt a run does not advertise the chord"
    );
    app.set_guidance(Some(Arc::clone(&guidance) as Arc<dyn dch_tui::Guidance>));

    // A draft of nothing but blanks is no draft at all: the chord
    // would refuse it, so the hint refuses it too.
    // Clear the real draft first (Ctrl+C with a run in flight clears
    // the composer), then type nothing but blanks.
    press(&mut app, &ctrl(KeyCode::Char('c')));
    type_into(&mut app, "   ");
    let frame = rows(&mut app, 90, 24);
    assert!(
        !frame.iter().any(|row| row.contains("ctrl+s")),
        "a hint for a chord that would refuse the draft in front of it \
         is noise at exactly the wrong moment: {frame:?}"
    );
}

#[test]
fn without_a_slot_the_chord_is_an_ordinary_submit() {
    let (mut app, kept, mut rx, _guidance) = app_steerable();
    app.set_guidance(None);
    running(&kept, true);

    steer(&mut app, "no seam here");

    assert_eq!(waiting(&kept), ["no seam here"]);
    assert!(rx.try_recv().is_ok());
}

#[test]
fn a_steered_row_anchors_the_view() {
    // The row a steer writes is the newest fact the transcript holds, and a
    // reader scrolled back to an older exchange would otherwise get nothing
    // on screen to say the chord landed at all. Submitting a prompt already
    // brings the view to the end; guidance has to do the same, because its
    // whole point is that the words left immediately.
    let (mut app, kept, _rx, guidance) = app_steerable();
    // A slot that takes what it is offered: refused, the chord would fall
    // through to an ordinary submission, which re-anchors the view for its
    // own reasons and would prove nothing about this arm.
    guidance.live.store(true, Ordering::SeqCst);
    running(&kept, true);
    rows(&mut app, 80, 12);
    app.handle_event(&plain(KeyCode::PageUp));
    assert!(
        !app.auto_scroll(),
        "the precondition detached the view from the transcript's end"
    );

    steer(&mut app, "check the other file too");

    // The row is not in the transcript until the boundary reads it;
    // the pin the chord armed carries the words into view meanwhile.
    rows(&mut app, 80, 12);
    assert!(
        app.auto_scroll() && app.scroll_offset() == 0,
        "the guided row re-anchors the view, so the key press answers itself"
    );
    assert_eq!(
        pinned(&kept),
        ["check the other file too"],
        "the words are on screen at the edge — the transcript row comes \
         when the turn reads them"
    );
    assert_eq!(
        *guidance.offered.lock().unwrap(),
        ["check the other file too"],
        "the words reached the running turn"
    );
}

#[test]
fn a_steered_row_lands_below_a_reply_that_came_first() {
    // The inversion the pin exists to prevent, on the side a row written
    // at the key could not cover: the model finishes an answer, the user
    // steers a moment later, and both are still unwritten when the next
    // frame comes. A row written at the key would sit above the older
    // answer; the pin holds the words below everything until the
    // boundary reads them, and the graduation lands them after the work
    // that was running — so the answer that finished first keeps the
    // place it finished in, in the transcript and in every save after.
    let (mut app, kept, _rx, guidance, observer) = app_steerable_with_observer();
    guidance.live.store(true, Ordering::SeqCst);
    running(&kept, true);
    kept.graduations
        .lock()
        .unwrap()
        .push(dch_tui::Graduation::Reply {
            text: "an answer finished before the key press".to_string(),
            thinking: None,
        });

    steer(&mut app, "stop and read the other file");

    rows(&mut app, 80, 12);
    let kinds: Vec<&str> = app
        .conversation()
        .iter()
        .map(|row| match row {
            TuiMessage::User { .. } => "user",
            TuiMessage::Assistant { .. } => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["assistant"],
        "mid-run the transcript holds the settled answer and no steered \
         row — the words wait pinned below it"
    );

    boundary(&observer);
    rows(&mut app, 80, 12);
    let kinds: Vec<&str> = app
        .conversation()
        .iter()
        .map(|row| match row {
            TuiMessage::User { .. } => "user",
            TuiMessage::Assistant { .. } => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["assistant", "user"],
        "the answer that finished first keeps the place it finished in"
    );
}

#[test]
fn a_waiting_steer_carries_its_chrome_at_the_edge_and_keeps_it_when_it_lands() {
    // The waiting steer is drawn as a user turn from its first frame —
    // prompt box, chevron — on its own rows at the edge, not
    // edge-relative guesses that box the pane's top; and when the
    // boundary reads the words, the row graduates into the
    // conversation with the same chrome, where a reply baking below
    // it moves no mark.
    let (mut app, kept, _rx, guidance, observer) = app_steerable_with_observer();
    guidance.live.store(true, Ordering::SeqCst);
    running(&kept, true);
    kept.graduations
        .lock()
        .unwrap()
        .push(dch_tui::Graduation::Reply {
            text: "an earlier answer bakes below before the steer                    lands, so the guided row is not the only content \
                   the pane holds"
                .to_string(),
            thinking: None,
        });

    steer(&mut app, "check the other file too");

    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let steered_row = rows
        .iter()
        .position(|row| row.contains("check the other file too"))
        .expect("the waiting row");
    let steered_row_u16 = u16::try_from(steered_row).unwrap_or(0);
    let marks: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains('❯'))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        marks,
        vec![steered_row],
        "the waiting steer is the only user turn, and its chevron rides \
         its own row from the first frame: {rows:?}"
    );
    assert_eq!(
        buffer[(5, steered_row_u16)].bg,
        app.theme.ui.surface,
        "the waiting row rides the prompt box from its first frame"
    );

    boundary(&observer);
    kept.graduations
        .lock()
        .unwrap()
        .push(dch_tui::Graduation::Reply {
            text: "the answer that followed".to_string(),
            thinking: None,
        });
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let steered_row = rows
        .iter()
        .position(|row| row.contains("check the other file too"))
        .expect("the steered row after it lands");
    let marks: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains('❯'))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        marks,
        vec![steered_row],
        "graduated into the conversation, with the reply baking below \
         it, the mark never moves: {rows:?}"
    );
}

#[test]
fn the_waiting_steer_pins_just_above_the_running_turns_timer() {
    // While a turn runs, the pane's last row is the turn's own — the
    // ticking timer closes it — and a steer the model has not read
    // waits directly above that: in the user's own styling, in the
    // spot the boundary that reads it will land it in, so nothing
    // jumps when the row graduates.
    let (mut app, kept, _rx, guidance, observer) = app_steerable_with_observer();
    guidance.live.store(true, Ordering::SeqCst);
    kept.mark_run_started();
    steer(&mut app, "hold on, wrong branch");

    let frame = rows(&mut app, 80, 24);
    let timer_at = frame
        .iter()
        .position(|row| row.contains("elapsed"))
        .expect("the running turn's timer row");
    let steer_at = frame
        .iter()
        .position(|row| row.contains("hold on, wrong branch"))
        .expect("the waiting steer's row");
    assert!(
        steer_at < timer_at,
        "the waiting steer sits directly above the running turn's timer, \
         in the spot it will land in: {frame:?}"
    );
    assert!(
        user_rows(&app).is_empty(),
        "and the transcript holds no row for it yet"
    );

    boundary(&observer);
    rows(&mut app, 80, 24);
    assert_eq!(
        user_rows(&app),
        ["hold on, wrong branch"],
        "the boundary graduates the row into the conversation"
    );
}

#[test]
fn an_unread_steer_returns_to_the_waiting_section_and_leaves_the_pin() {
    // The run ended before any boundary read the words: the pin held
    // them at the edge, and the return hands them to the waiting
    // section — the pin empties with the return, and the transcript
    // never held a row for words the model never met.
    let (mut app, kept, mut rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    steer(&mut app, "wrong direction entirely");
    rows(&mut app, 80, 24);
    assert_eq!(
        pinned(&kept),
        ["wrong direction entirely"],
        "the precondition: the words wait at the edge"
    );

    // What the slot hands back at idle: the run left it undrained.
    *guidance.owed.lock().unwrap() = vec!["wrong direction entirely".to_string()];
    running(&kept, false);
    let frame = rows(&mut app, 80, 24);

    assert_eq!(
        waiting(&kept),
        ["wrong direction entirely"],
        "the unread words go back where the user can read, fix, or drop them"
    );
    assert!(
        pinned(&kept).is_empty(),
        "the pin empties with the return — the edge holds nothing the \
         queue does not"
    );
    assert!(
        user_rows(&app).is_empty(),
        "the transcript never held the row: the model never read the words"
    );
    assert!(
        frame.iter().any(|row| row.contains("never read")),
        "and the notice row says where they went: {frame:?}"
    );
    assert!(
        rx.try_recv().is_ok(),
        "the returned prompt comes back with its turn promised"
    );
}

#[test]
fn a_pinned_steer_survives_the_exit_as_a_waiting_prompt() {
    // The exit overtakes the run with the steer still at the edge:
    // the run never ended, so the seam never returned the words, and
    // they are in neither the queue nor the transcript. The exit's
    // snapshot takes the pin with the waiting prompts — typed words no
    // turn spent — and the echo writes each its unclaimed row, the
    // shape a resume seeds back into the waiting section.
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    kept.queued
        .lock()
        .unwrap()
        .push("queued before the steer".to_string());
    steer(&mut app, "steered just before the exit");
    assert_eq!(
        pinned(&kept),
        ["steered just before the exit"],
        "the precondition: the words wait at the edge"
    );

    let unclaimed = app.waiting_prompt_texts_at_exit();

    assert_eq!(
        unclaimed,
        ["queued before the steer", "steered just before the exit"],
        "the snapshot carries the pinned steer with the waiting prompts, \
         queue order first, edge order after"
    );
    assert!(
        pinned(&kept).is_empty(),
        "the snapshot takes the pin whole — the words have one home now"
    );
    app.echo_waiting_prompts_at_exit(unclaimed);
    assert_eq!(
        user_rows(&app),
        ["queued before the steer", "steered just before the exit"],
        "and the echo writes each its unclaimed row, the resume's seed"
    );
}

#[test]
fn an_exit_snapshot_never_duplicates_a_steer_the_seam_returned() {
    // The seam already returned the steer and the return's requeue put
    // it in the waiting section: the pin was pruned with that return,
    // so the exit's snapshot lists the words once, from the queue, and
    // the pin contributes nothing.
    let (mut app, kept, _rx, guidance) = app_steerable();
    running(&kept, true);
    guidance.live.store(true, Ordering::SeqCst);
    steer(&mut app, "returned before the exit");
    *guidance.owed.lock().unwrap() = vec!["returned before the exit".to_string()];
    running(&kept, false);
    rows(&mut app, 80, 24);
    assert!(
        pinned(&kept).is_empty(),
        "the precondition: the return emptied the pin"
    );
    assert_eq!(
        waiting(&kept),
        ["returned before the exit"],
        "and the requeue holds the words"
    );

    let unclaimed = app.waiting_prompt_texts_at_exit();
    assert_eq!(
        unclaimed,
        ["returned before the exit"],
        "once from the queue, never twice: {unclaimed:?}"
    );
}
