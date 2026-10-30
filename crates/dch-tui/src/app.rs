//! The terminal application shell.
//!
//! `TuiApp` owns the input buffer, the conversation, and the shared
//! state background producers write through; `run` drives the
//! render ↔ input ↔ scroll loop until the user quits.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthChar as _;
use unicode_width::UnicodeWidthStr as _;

use dch_config::DchConfig;
use dch_config::PathStyle;
use dch_config::StripPosition;
use dch_config::StripStyle;

use crate::Graduation;
use crate::events::TerminalEvents;
use crate::guidance::Guidance;
use crate::input::{InputAction, InputEditor};
use crate::markdown;
use crate::message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
use crate::observer::{HealthNotice, ToolResultDisplay, TuiObserverState};
use crate::permission::PermissionRequest;
use crate::question::QuestionPrompt;
use crate::queue::Submission;
use crate::theme::Theme;
use crate::todo_view;
use crate::tool_render::SPINNER_FRAMES;
use dch_config::Verbosity;

/// The callback fired when a turn's outcome joins the conversation.
///
/// Installed by the mode driver to persist the session transcript;
/// receives the conversation snapshot and, when a turn end arrived
/// since the last fire, the engine's own history snapshot for that
/// turn — full on success, committed-only on failure — so the host
/// persists both halves without reaching into display state.
type TurnEndHook =
    Box<dyn Fn(&[TuiMessage], Option<&Arc<Vec<loopctl::message::Message>>>) + Send + Sync>;

/// The largest elapsed span a persisted tool block may carry.
///
/// `Duration::from_secs_f64` panics past its own representable
/// range; a hostile or corrupted value beyond this bound renders as
/// zero instead of killing the first frame after resume.
const MAX_ELAPSED_SECS: f64 = 9.0e18;

/// The minimum spacing between background-initiated frames.
///
/// Redraws requested through the shared notify are coalesced to at
/// most one per interval, so a burst of stream deltas costs one
/// frame whatever the token rate.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// The frame lane's per-second counters.
///
/// Reset each time a second's line is logged; the second opens on the
/// first draw measured into it, so a lane that never draws reports
/// zeros honestly.
#[derive(Default)]
struct FrameStats {
    /// Animation-lane wakes this second.
    ///
    /// Only beats whose gate found something animating are counted,
    /// so the figure measures drawn frames rather than raw timer
    /// ticks; it feeds the per-second frame-lane debug line and
    /// drops back to zero once that line is logged.
    anim_wakes: u32,

    /// Terminal draws this second.
    ///
    /// Counted after each draw the run loop puts on screen, whatever
    /// lane requested the frame; the number feeds the per-second
    /// frame-lane debug line and resets to zero when that line is
    /// logged.
    draws: u32,

    /// The slowest single draw's duration this second.
    ///
    /// `None` until the second's first draw completes, then the
    /// running maximum over every measured draw; the debug line
    /// reports it in whole milliseconds — zero while `None` — and
    /// the per-second reset returns it to `None`.
    worst_draw: Option<std::time::Duration>,

    /// When the current second started.
    ///
    /// `None` until the run's first draw, whose instant becomes the
    /// epoch; each later draw compares its own clock against that
    /// instant, and the rollover that logs a second's line moves the
    /// epoch to the rollover instant so the next window measures a
    /// full second.
    second_start: Option<std::time::Instant>,
}

impl FrameStats {
    /// Count one draw, and hand out the frame lane when its second ends.
    ///
    /// Every draw joins the count and the running worst. The window
    /// opens on the draw that finds it shut, and its epoch is that
    /// draw's own instant: the window always measures a full span of
    /// draws, so a second of quiet reports an honest zero rather than
    /// closing on the draw that happened to stand there. Once an epoch
    /// has stood [`FRAME_REPORT_SECONDS`] its counters go out as a
    /// [`FrameLane`] and a fresh window opens at that draw; until then
    /// nothing is reported.
    fn note_draw(
        &mut self,
        took: std::time::Duration,
        now: std::time::Instant,
    ) -> Option<FrameLane> {
        self.draws = self.draws.saturating_add(1);
        self.worst_draw = Some(match self.worst_draw {
            Some(worst) if worst >= took => worst,
            _ => took,
        });
        let Some(epoch) = self.second_start else {
            self.second_start = Some(now);
            return None;
        };
        if now.duration_since(epoch) < FRAME_REPORT_SECONDS {
            return None;
        }
        let lane = FrameLane {
            anim_wakes: self.anim_wakes,
            draws: self.draws,
            worst_draw_ms: u64::try_from(self.worst_draw.map_or(0, |d| d.as_millis()))
                .unwrap_or(u64::MAX),
        };
        *self = FrameStats {
            second_start: Some(now),
            ..FrameStats::default()
        };
        Some(lane)
    }
}

/// One closed second of frame-lane counters, ready for the log.
///
/// The numbers the per-second line carries, read off the window as it
/// closes: what animated, what was drawn, and the slowest single draw
/// in whole milliseconds.
struct FrameLane {
    /// Wakes the animation lane asked for during the window.
    ///
    /// Counted only on beats whose gate found something on screen
    /// animating, so the figure measures frames the lane actually
    /// requested rather than raw timer ticks; it reaches the log as
    /// the window closes and the reopened window starts again at zero.
    anim_wakes: u32,

    /// Draws the run loop put on screen during the window.
    ///
    /// Counted after every draw whatever lane requested it, so the
    /// figure reads as painted frames rather than requested ones —
    /// the two differ whenever a beat's work coalesced into a single
    /// frame.
    draws: u32,

    /// The window's slowest single draw, in whole milliseconds.
    ///
    /// The maximum over every measured draw, not the last one, so a
    /// laggy frame early in the second is still what the line reports;
    /// whole milliseconds is the resolution the lane is worth reading
    /// at, and a window that measured no draw reports zero.
    worst_draw_ms: u64,
}

/// How long a frame-lane window stands before it reports and reopens.
///
/// One second: long enough that a slow frame shows up as a rate a
/// reader can act on rather than as one sample, short enough that the
/// lane answers a question asked about the moment just now. Chosen to
/// match the log line's own promise, which names the figure per second.
const FRAME_REPORT_SECONDS: std::time::Duration = std::time::Duration::from_secs(1);

/// The periodic beat the run loop wakes on.
///
/// The loop's coarse chores ride this beat: claiming a redraw a
/// capped background request parked, stepping a drag resting on the
/// conversation pane's edge, flipping the composer caret's blink
/// phase, and retiring lapsed notices and prompts. A tick with tool
/// calls in flight also draws, keeping their elapsed stamps' tenth
/// of a second live; every other idle tick draws nothing, and the
/// loop sleeps on.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The frame cadence the activity strip animates at while the agent
/// runs.
///
/// The loop's own tick is far too coarse for a moving bar, and deltas
/// alone leave it frozen between provider chunks; for as long as the
/// run lives the animation wake redraws at this step — fifty frames a
/// second, so even the fastest styles jump at most a couple of cells
/// per frame — whatever the wire's granularity.
const ANIMATION_INTERVAL: Duration = Duration::from_millis(20);

/// Ticks per caret blink phase.
///
/// Half a second on, half a second off — the cadence editors settle
/// on when they own the blink themselves. The terminal's own
/// blinking-cursor request may be ignored or preference-gated, so
/// the app toggles visibility on its tick instead: what the user
/// sees does not depend on the terminal's cooperation.
const CARET_BLINK_TICKS: u32 = 5;

/// How many graduated calls keep their expansion data.
///
/// The most recent calls stay expandable; older ones fold back to
/// plain summary rows once this many newer calls have graduated.
/// The retained set is bounded the way the capture store is, in
/// FIFO order rather than a wholesale drop — the newest tool
/// blocks, the ones a reader is still working through, are the
/// ones that stay openable.
const TOOL_DETAIL_CAP: usize = 256;

/// The most rows the waiting-prompt section can hold when drawn.
///
/// The cap, not the count: the section reserves only the rows it
/// paints — the air above it, the title row carrying the badge that
/// names it, the air standing that badge off the prompt below it,
/// and the rows the shown prompt wraps into. A one-line wording ends
/// the section at its own last row; no blank rides beneath it, so the
/// space under the queue is the frame's own furniture rather than the
/// section's margin. Two prompt rows are the most a wording may take —
/// one prompt at a time, counting the rest rather than growing
/// downward, because the rows it takes come straight out of the
/// conversation: a backlog must not cost a row per prompt of the
/// transcript the user is reading to decide what to do about it — and
/// the two blank rows are what stop a filled pill reading as riveted
/// to its neighbors: the transcript it overlays, the wording it
/// labels.
const QUEUE_ROWS: u16 = 5;

/// The most task rows the pinned plan section holds.
///
/// Four: the section is the plan's window, not its archive — the
/// conversation's per-update blocks carry every task in full, so the
/// section spends the conversation's rows on the tasks nearest the
/// plan's top and names the rest with one ellipsis line. The header
/// and that line ride outside the count, so a full section stands at
/// six rows at the very widest plan.
const TODO_SECTION_BODY_ROWS: usize = 4;

/// The plan section's inset from each of the frame's edges.
///
/// The section's own breath from the frame: the plan's rows read as
/// the transcript's register, plain lines rather than a panel, and
/// never flush against the frame — the badge rides flush with the
/// rows inside it.
const TODO_SECTION_INSET: u16 = 2;

/// The glyph marking the waiting-prompt section's rows.
///
/// Short on purpose: the marker shares its row with the prompt's wording,
/// and everything a prefix costs is wording squeezed off the end. Both the
/// renderer and the tests that read a frame locate the section by this
/// glyph, so it is the section's identity on screen rather than ornament.
const QUEUE_MARKER: &str = "\u{231b}";

/// The title row the waiting-prompt section opens with.
///
/// Rendered as a badge — the word on a filled pill, one cell proud of
/// its letters on either side, filled with the accent the composer's
/// edge column wears — flush with the section's left edge, so the
/// prompts waiting behind the turn in flight are a named region of
/// the screen rather than a strip a first-time reader must decode
/// from the hourglass alone. It rides the section's first row, flush
/// with the prompt rows' left edge, and exists only while prompts
/// wait — an empty queue is no panel, title included. The section's
/// chords ride beside it on the same row, dim after the pill: the
/// badge row is short and always drawn, where the prompt rows wrap
/// and a right-edge hint would vanish exactly when the terminal
/// narrows.
const QUEUE_TITLE: &str = "QUEUED";

/// The prefix the shown prompt's first row carries.
///
/// The marker alone when one prompt waits, the marker with its place
/// in the line when more do. Both the section's renderer and the
/// height the layout reserves for it measure the prompt's wrap
/// against this prefix, so the rows reserved are the rows drawn —
/// the wrap the height counted on is the wrap the frame sees.
fn queue_prompt_prefix(total: usize, shown: usize) -> String {
    if total > 1 {
        format!("{QUEUE_MARKER} [{}/{}] ", shown.saturating_add(1), total)
    } else {
        format!("{QUEUE_MARKER} ")
    }
}

/// The plan section's posture, as Ctrl+T leaves it.
///
/// One chord walks the three: capped at its four task rows with the
/// cut named, opened to the whole list however tall, or folded away
/// entirely. A posture is a view preference — the plan keeps updating
/// underneath every one of them, and the run's landing clears the
/// section out of whatever posture it was in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanView {
    /// Capped at [`TODO_SECTION_BODY_ROWS`] task rows, the ellipsis
    /// naming the rest and the chord that shows them.
    Capped,

    /// The whole list — the section grows to whatever the plan holds,
    /// because opening it is a deliberate act with the cost in plain
    /// sight.
    Full,

    /// Folded away: the conversation takes the rows back, one dim
    /// marker row left standing in the section's place — see
    /// [`folded_hint`](crate::todo_view::folded_hint).
    Hidden,
}

/// Which destructive act the section's confirmation is holding.
///
/// Named rather than a flag: the two acts spend different things —
/// one sends the prompt into the run in flight, the other takes it
/// out of the queue for good — and the ask's wording is the last
/// chance to say which before it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueAct {
    /// Send the shown prompt to the running turn as guidance.
    ///
    /// The gentler spend: the prompt leaves the queue and joins the
    /// conversation the model is already having, read at its next
    /// boundary rather than waiting for its own turn.
    Steer,

    /// Take the shown prompt out of the queue for good.
    ///
    /// The irreversible spend: the prompt never runs, its words never
    /// reach the model, and the queue closes up behind it.
    Drop,
}

/// Whose hands the keyboard is in, as far as waiting prompts go.
///
/// One value rather than a focus flag beside an edit flag, because the two
/// flags could stand together and nothing in the code would have been able
/// to tell what a press meant: the section cannot hold the keys while its
/// prompt is out being typed into — the typing is precisely what the section
/// would have to give up to let it happen. The states the keys actually have
/// are three, so three is what is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueMode {
    /// The composer holds the keys; the section only shows what waits.
    ///
    /// The resting state, and the only one a session with nothing waiting
    /// can be in: the keys go where they went before the section existed,
    /// and its rows, when there are any, are reading matter.
    Plain,

    /// The section holds the keys: the arrows walk, `e` lifts, `d` drops,
    /// and everything else is swallowed.
    ///
    /// Reachable only with something waiting — the chord is refused on an
    /// empty queue rather than parking the user on a list with no rows —
    /// and Esc gives the keys back. Nothing leaks from in here: a letter
    /// typed over the backlog has nowhere it should go, and assembling a
    /// draft behind the user's back is what the swallowing exists to stop.
    Focused,

    /// The ask between a destructive key and its act.
    ///
    /// `s` and `d` are one keypress away from sending a prompt into a
    /// running turn or spending it entirely, so the press asks first:
    /// `y` does the act the ask names, anything else stands it down
    /// without spending anything — a stray key can cancel, never
    /// confirm. The ask holds the keyboard while it stands, for the
    /// same reason the walk does: a key that moved what `y` was about
    /// to act on would change the answer under the finger already
    /// resting on it.
    Confirming {
        /// Which act the ask is holding.
        ///
        /// The answer `y` performs and the ask's wording names — the
        /// one fact the ask exists to settle before anything is spent.
        act: QueueAct,
    },

    /// A prompt's body is out in the composer, held for the prompt this id
    /// names.
    ///
    /// Enter saves into that slot. Esc and Ctrl+C put the draft away,
    /// leaving the prompt in the queue with the wording it had before the
    /// composer took it.
    /// On save the id may name a prompt the run took in the meantime — that
    /// edge is the commit's to settle, not a state to model. The claim that
    /// takes it often empties the queue outright, and the mode outlives that:
    /// the text in the composer is the user's, held for a prompt whose turn is
    /// already running, which is precisely when a correction is worth sending.
    Editing {
        /// The waiting prompt whose text the composer is holding.
        ///
        /// An entry, not a copy of one: the prompt stays in the queue while
        /// its body is out, so this id is how a save finds the slot to write
        /// back into. A run can claim it in the meantime, and then there is
        /// no slot to write to — the text goes out as a correction to the
        /// turn that took it, which is why the id outlives the entry.
        id: u64,
    },
}

/// How long a transient notice holds the row above the composer.
///
/// Long enough to be read at a glance, short enough that the row
/// reads as empty by default.
const NOTICE_HOLD: Duration = Duration::from_secs(4);

/// How close to the newest line a scroll action must land to count
/// as "back at the bottom".
///
/// The streaming region grows between a scroll and the next layout,
/// so an exact offset-zero check would detach a view that only fell
/// behind by growth.
const STICK_TOLERANCE: usize = 2;

/// Lines one wheel or touchpad scroll event moves.
///
/// One line per wheel event: a burst of events still accumulates
/// into fast travel, while a single notch lands the smallest useful
/// step.
const WHEEL_SCROLL_LINES: usize = 1;

/// The most lines of the live segment parsed as markdown per frame.
///
/// A single block that has not yet met a boundary is re-parsed whole
/// on every frame, so the parse cost grows with the entire unsettled
/// block; past this cap the block's head renders as plaintext until
/// it freezes, keeping the per-frame parse inside the frame budget.
const LIVE_PARSE_LINE_CAP: usize = 400;

/// The most bytes of the live segment parsed as markdown per frame.
///
/// Lines can be arbitrarily long — a minified code or data line
/// carries a whole block in one line — so the line cap alone does
/// not bound the parse. Past this byte cap the excess joins the
/// plaintext head, on a character boundary.
const LIVE_PARSE_BYTE_CAP: usize = 16_384;

/// One question's answer-in-progress: its cursor, its marks, and its
/// draft.
///
/// Everything about answering a question that is not the question
/// itself. It lives on the queued question — [`PendingQuestion`] —
/// rather than on the app, so walking a batch with Left and Right sets
/// an unfinished answer aside and finds it waiting on the return visit;
/// a question answers or retires, and its state leaves with it.
///
/// The answer has two modes and the keys split between them. Over the
/// rows, Space marks the row under the cursor and Enter confirms; in the
/// draft, the arrows and Home and End belong to the text and Enter sends
/// it. [`composing`](Self::composing) is which mode the question stands
/// in, and flipping it changes no answer itself — the marks stay, the
/// words stay, and only the keys' meaning moves. That is why the draft
/// can be stepped out of and back into without losing anything: the mode
/// is a layer over the answer, not the answer.
#[derive(Debug, Default)]
struct QuestionUi {
    /// The cursor row over the option rows plus the compose row.
    ///
    /// Indexes into the question's options, with one past the last
    /// naming the compose row. Enter confirms this row when nothing is
    /// marked.
    cursor: usize,

    /// The rows marked so far, by option index.
    ///
    /// A multi-select marks any number and Enter confirms the set. A
    /// single-select marks at most one — Space moves the mark, and
    /// Space on the marked row clears it — so Space always does
    /// something and never answers; Enter is the only send.
    toggled: std::collections::HashSet<usize>,

    /// Whether the compose row is open and the draft owns keys.
    ///
    /// While set, printable keys build the draft and the arrows move
    /// its caret instead of walking the queue; Enter sends the draft
    /// and Esc steps back to the rows with the text kept.
    composing: bool,

    /// The free answer being composed, if any.
    ///
    /// Survives the step back to the options — a half-typed reply is
    /// the user's text, not the overlay's to discard — and clears when
    /// the question resolves.
    draft: String,

    /// The draft's caret, counted in characters from its start.
    ///
    /// A character count, not a byte offset: the draft carries accents
    /// and double-width glyphs, and the caret moves by characters. The
    /// byte offsets an edit needs derive from this, and so does the
    /// screen column it draws at.
    caret: usize,
}

impl QuestionUi {
    /// The draft's length in characters — the unit every caret position
    /// in this type is counted in.
    ///
    /// `String::len` would answer in bytes and put the caret a keystroke
    /// inside an accent, so every bound the caret is clamped against goes
    /// through here. The byte offsets an edit needs are derived from this
    /// count rather than the other way round, because the user counts
    /// characters and only `String` counts bytes.
    fn draft_len(&self) -> usize {
        self.draft.chars().count()
    }

    /// Open the draft for editing, with the caret at its end.
    ///
    /// This is where a fresh draft and a held one meet, and the rule is
    /// that opening never discards: text left by an earlier edit stays,
    /// because coming back to add a word is not a request to lose the
    /// sentence. The caret therefore goes to the end rather than the
    /// start — carrying on is the ordinary move, and `Home` is one press
    /// away when the intent is the front.
    ///
    /// What opening *costs* is the caller's business, not this type's.
    /// Over the compose row it is free; over an option on a
    /// single-answer question it competes with a marked row, and the
    /// sheet shows that trade by shadowing the mark while the draft is
    /// open.
    fn open_composing(&mut self) {
        self.composing = true;
        self.caret = self.draft_len();
    }

    /// Type `ch` at the caret, pushing what stood there to the right.
    ///
    /// Insertion is the whole of typing here: there is no overtype mode,
    /// and the caret ends up just past the character it placed so the
    /// next keystroke follows it. That is what makes pausing to fix a
    /// word mid-sentence behave — step back, insert, and carry on writing
    /// forward without the caret wandering off.
    ///
    /// The character's byte width and the caret's position are two counts
    /// that meet only in `byte_index`: the string is edited by bytes, the
    /// caret is moved in characters, and neither is allowed to assume the
    /// other for the draft's whole length.
    fn type_char(&mut self, ch: char) {
        let byte = byte_index(&self.draft, self.caret);
        self.draft.insert(byte, ch);
        self.caret = self.caret.saturating_add(1);
    }

    /// Delete the character before the caret, leaving the text right of
    /// it exactly where it stands.
    ///
    /// A caret names a position *between* characters, so deleting takes
    /// from the left, as every editor does: the word ahead is untouched,
    /// which is what lets the cursor be parked in the middle of a
    /// sentence and work backwards through it.
    ///
    /// At the start there is nothing to the left, and nothing goes.
    /// Deleting the character to the right instead would borrow this key
    /// for a different one, and a draft empty in the first place is the
    /// same case by position — both leave the text and the caret alone.
    /// A grapheme cluster is not atomic here either: one backspace peels
    /// one character, so an accent is removed before the letter it sits
    /// on.
    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let Some((byte, ch)) = self.draft.char_indices().nth(self.caret.saturating_sub(1)) else {
            return;
        };
        self.draft
            .replace_range(byte..byte.saturating_add(ch.len_utf8()), "");
        self.caret = self.caret.saturating_sub(1);
    }

    /// Move the caret one character left, stopping at the draft's start.
    ///
    /// The stop is the substance of it. While the draft is open the
    /// arrows are the text's, and an arrow that walked off the end of the
    /// text would be two different keys sharing one press — the same
    /// reason the queue's own walking is unreachable from in here.
    fn caret_left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    /// Move the caret one character right, stopping at the draft's end.
    ///
    /// Clamped to the current length rather than to any past-the-end
    /// position, so the caret always sits on a real boundary between two
    /// characters and a type after the last one appends. A draft emptied
    /// from under the caret clamps it back to the single position the
    /// empty string has.
    fn caret_right(&mut self) {
        self.caret = (self.caret.saturating_add(1)).min(self.draft_len());
    }

    /// Put the caret at the draft's start.
    ///
    /// A jump, not a walk: `Home` and `End` are why the arrow keys can
    /// stop at the edges without stranding anyone at the far end of a
    /// long reply. Both are safe on an empty draft, where the two ends
    /// are the same single position.
    fn caret_home(&mut self) {
        self.caret = 0;
    }

    /// Put the caret at the draft's end — the position `open_composing`
    /// already puts it in, kept for the key that means the end from
    /// wherever the caret stands.
    ///
    /// Read off the current length rather than remembered, so text
    /// inserted or deleted elsewhere cannot leave it pointing past what
    /// is there.
    fn caret_end(&mut self) {
        self.caret = self.draft_len();
    }

    /// Insert `text` at the caret as one edit, the caret landing past it.
    ///
    /// The entry a paste takes: one gesture, one insertion, rather than a
    /// character at a time. Landing the caret past what arrived is the
    /// point — text dropped into the middle of a sentence leaves the caret
    /// where typing resumes, the way it does after typing that text by
    /// hand.
    fn insert_str(&mut self, text: &str) {
        let byte = byte_index(&self.draft, self.caret);
        self.draft.insert_str(byte, text);
        self.caret = self.caret.saturating_add(text.chars().count());
    }

    /// Delete the character right of the caret, leaving the caret and the
    /// text left of it exactly where they stand.
    ///
    /// `backspace`'s mirror: a caret names a position between characters,
    /// and this one takes from the right. At the draft's end there is
    /// nothing to the right and nothing goes, which is the same reason
    /// backspace does nothing at the start.
    fn delete_forward(&mut self) {
        let Some((byte, ch)) = self.draft.char_indices().nth(self.caret) else {
            return;
        };
        self.draft
            .replace_range(byte..byte.saturating_add(ch.len_utf8()), "");
    }

    /// Move the caret to the start of the word left of it, or of the word
    /// it stands inside.
    ///
    /// A word is a run of non-blank characters, and blanks are crossed
    /// before the word beyond them, so a caret parked in a gap lands at
    /// the previous word's start rather than at the gap's end — the
    /// backward-word spot the composer's own editor walks to. The step
    /// counts characters here because that is the unit the draft's caret
    /// is held in.
    fn caret_word_left(&mut self) {
        self.caret = self.word_boundary_left(self.caret.min(self.draft_len()));
    }

    /// Move the caret past the end of the word right of it.
    ///
    /// From inside a word the jump is to that word's end; from a gap it
    /// crosses the gap and the word after it, matching the forward-word
    /// convention the composer follows. The caret stops at the draft's end,
    /// where the arrows already stop.
    fn caret_word_right(&mut self) {
        let chars: Vec<char> = self.draft.chars().collect();
        let last = chars.len();
        let mut index = self.caret.min(last);
        while chars.get(index).copied().is_some_and(char::is_whitespace) {
            index = index.saturating_add(1);
        }
        while chars.get(index).is_some_and(|ch| !ch.is_whitespace()) {
            index = index.saturating_add(1);
        }
        self.caret = index;
    }

    /// Delete the word left of the caret, taking the caret with it.
    ///
    /// The text right of the caret keeps its place, so the word reads as
    /// lifted out rather than the tail shifted back. With nothing left of
    /// the caret the boundary is already where the caret stands and
    /// nothing goes.
    fn delete_word_left(&mut self) {
        let caret = self.caret.min(self.draft_len());
        let from = self.word_boundary_left(caret);
        let start = byte_index(&self.draft, from);
        let end = byte_index(&self.draft, caret);
        self.draft.replace_range(start..end, "");
        self.caret = from;
    }

    /// Delete everything between the caret and the draft's start.
    ///
    /// The line-head kill the composer's editor answers with the same
    /// chord: what was typed before the point the user meant to start at
    /// goes in one press, and the caret lands on the start it cleared.
    fn delete_to_start(&mut self) {
        let end = byte_index(&self.draft, self.caret.min(self.draft_len()));
        self.draft.replace_range(0..end, "");
        self.caret = 0;
    }

    /// The draft's character index at the start of the word standing left
    /// of `index`, crossing any blanks between the two.
    fn word_boundary_left(&self, index: usize) -> usize {
        let chars: Vec<char> = self.draft.chars().collect();
        let mut at = index.min(chars.len());
        // The `at > 0` guard is what ends these walks, and the only thing
        // that ends them: `0usize.saturating_sub(1)` is still 0, so the
        // saturated offsets the borrow checker asks for keep reading the
        // draft's first character, and a walk without the guard never
        // reaches the start it is walking to.
        while at > 0
            && chars
                .get(at.saturating_sub(1))
                .copied()
                .is_some_and(char::is_whitespace)
        {
            at = at.saturating_sub(1);
        }
        while at > 0
            && chars
                .get(at.saturating_sub(1))
                .is_some_and(|ch| !ch.is_whitespace())
        {
            at = at.saturating_sub(1);
        }
        at
    }
}

/// The byte offset of `text`'s `index`th character, or the end of the
/// string once the index is past the last character.
///
/// Caret positions count characters while `String` edits take byte
/// offsets, and the two meet here.
fn byte_index(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// A queued question and the answer being composed for it.
///
/// The answer's state rides with its question instead of sitting on the
/// app, so a batch behaves like a form: mark three answers, walk back to
/// the first, change it, and everything else is as it was left. A
/// question's state leaves with it when it answers or retires, and the
/// question behind it is still clean.
#[derive(Debug)]
struct PendingQuestion {
    /// What was asked, exactly as delivered.
    ///
    /// The asking side's own wording, with its options in the order the
    /// model wrote them: the sheet renders the question verbatim and the
    /// answer reads its labels off these rows, so the text the user chose
    /// is the text that was asked with. Nothing here is the display's to
    /// reword, reorder, or truncate.
    prompt: QuestionPrompt,

    /// This question's cursor, marks, and draft.
    ///
    /// The answer-in-progress rides with its question rather than on the
    /// app, which is what lets a batch behave like a form: walk away from
    /// a half-answered question and it waits, unwritten, for the return
    /// visit. It arrives clean and retires with the question, so the one
    /// behind an answered question has never worn the last answer.
    ui: QuestionUi,
}

impl PendingQuestion {
    /// Wrap a delivered prompt with a clean answer-in-progress.
    ///
    /// The only way a question enters the queue, which is what makes an
    /// inherited answer impossible: every prompt lands at row zero,
    /// unmarked, with an empty draft, whatever the question that left the
    /// queue just before it had been doing.
    fn new(prompt: QuestionPrompt) -> Self {
        Self {
            prompt,
            ui: QuestionUi::default(),
        }
    }
}

/// The question sheet's rows and the caret riding them.
///
/// Built by [`TuiApp::question_sheet`], the one walk both the render and
/// the height the frame reserves go through, so the box the user sees and
/// the rows held aside for it cannot disagree.
struct QuestionSheet {
    /// Every row from the header chip to the key hints.
    ///
    /// Already wrapped to the width asked for, in the order they draw.
    /// Their count *is* the sheet's height, which is why the layout that
    /// holds rows aside for the overlay counts this list instead of
    /// measuring anything on its own: one walk produces both, so the box
    /// and the space reserved for it cannot disagree.
    lines: Vec<Line<'static>>,

    /// The row holding the draft, and the caret's display column within
    /// its text — `Some` only while the draft owns the keys, since only
    /// then is anything on the sheet being edited.
    caret: Option<(usize, usize)>,
}

/// The columns the compose row spends before its draft text: the cursor
/// marker and the pencil, one display column each.
///
/// The caret's cell is the row's start, plus this, plus the display width
/// of the text left of the caret — so the row and the caret share one
/// spelling of the prefix.
const COMPOSE_PREFIX: &str = "\u{203a} \u{270e} ";

/// The column option descriptions and the compose row's subtext hang at:
/// the two marker columns an option row spends before its label — the
/// cursor's and the toggle's.
///
/// Detail rows share it so a description reads as belonging to the option
/// above it rather than as an unrelated indented row. The indent is
/// applied outside `wrap_prompt_rows`, which splits on spaces and would
/// drop a leading run of them as empty words.
const DETAIL_INDENT: &str = "    ";

/// Assemble the question sheet's blocks into at most `budget` rows.
///
/// The compose block and the hints are the two blocks a user cannot answer
/// without: the first is where a reply the options never offered gets
/// written, and the second is the only place the keys are explained. Both
/// keep their rows whatever the terminal asks, which is what stops a short
/// window from cutting the row the user is typing into and leaving them
/// writing blind. The question text follows them, clamped to the rows that
/// fit, and the option list is what gives way.
///
/// The options give way as a window rather than a truncation, anchored on
/// `cursor_option` and grown outward — see [`option_window`]. Returns the
/// assembled rows beside the index the compose block starts at, which is
/// where the caret's row within that block is measured from; a terminal so
/// short that the untouchable blocks do not fit loses the tail, the only
/// sacrifice left, and the caret with it.
fn fit_question_sheet(
    head: Vec<Line<'static>>,
    options: &[(usize, Vec<Line<'static>>)],
    compose: Vec<Line<'static>>,
    tail: Vec<Line<'static>>,
    cursor_option: usize,
    budget: usize,
) -> (Vec<Line<'static>>, usize) {
    // The blocks that never give way are paid for first; whatever the
    // budget still holds is what the question and the options may spend.
    let fixed = compose.len().saturating_add(tail.len());
    let spendable = budget.saturating_sub(fixed);
    let head_rows = head.len().min(spendable);
    let room = spendable.saturating_sub(head_rows);
    let (lo, hi) = option_window(options, cursor_option, room);
    let mut lines = Vec::with_capacity(budget.saturating_add(1));
    lines.extend(head.into_iter().take(head_rows));
    lines.extend(
        options
            .iter()
            .skip(lo)
            .take(hi.saturating_sub(lo))
            .flat_map(|(_, rows)| rows.clone()),
    );
    let compose_start = lines.len();
    lines.extend(compose);
    lines.extend(tail);
    lines.truncate(budget.saturating_add(1).max(1));
    (lines, compose_start)
}

/// The option groups that fit in `room` rows, as the `(lo, hi)` index
/// range into `options` that holds `cursor_option`.
///
/// The cursor's own group is taken first, because it is the row the sheet
/// has to show, and the window then grows downward before upward so a list
/// reads in the order it was asked. Walking with the arrows moves the
/// window with the cursor, so an option is never unreachable — only
/// undrawn until it is stood on. A budget that will not hold the cursor's
/// own group leaves the window empty: options have nothing to spare rather
/// than half a row between them.
fn option_window(
    options: &[(usize, Vec<Line<'static>>)],
    cursor_option: usize,
    room: usize,
) -> (usize, usize) {
    let last = options.len();
    if last == 0 || room == 0 {
        return (0, 0);
    }
    let rows_of = |index: usize| {
        options
            .get(index)
            .map_or(usize::MAX, |(_, rows)| rows.len())
    };
    let below = |index: usize| {
        if index < last {
            rows_of(index)
        } else {
            usize::MAX
        }
    };
    let above = |index: usize| {
        if index > 0 {
            rows_of(index.saturating_sub(1))
        } else {
            usize::MAX
        }
    };
    let start = cursor_option.min(last.saturating_sub(1));
    let mut used = rows_of(start);
    if used > room {
        return (0, 0);
    }
    let (mut lo, mut hi) = (start, start.saturating_add(1));
    loop {
        let down = below(hi);
        if used.saturating_add(down) <= room {
            used = used.saturating_add(down);
            hi = hi.saturating_add(1);
            continue;
        }
        let up = above(lo);
        if used.saturating_add(up) <= room {
            used = used.saturating_add(up);
            lo = lo.saturating_sub(1);
            continue;
        }
        break;
    }
    (lo, hi)
}

/// Whether the view follows the newest content or holds its ground.
///
/// The stickiness posture the scroll keys drive: pinned views
/// re-anchor to the bottom on every layout; any scroll-up detaches,
/// and scrolling back within [`STICK_TOLERANCE`] lines of the bottom
/// — or pressing End or submitting — re-arms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Stick {
    /// The view follows the newest content.
    ///
    /// The resting posture: every layout re-anchors the view to the
    /// newest line, so streamed growth and new turns arrive into
    /// view. A scroll up leaves it for [`Hold`], and the stick
    /// tolerance — or End, or a submit — returns to it.
    #[default]
    Follow,

    /// The view holds its offset.
    ///
    /// The reading posture: the offset stands still while content
    /// grows beneath it, so the lines being read never move; only a
    /// deliberate act — scrolling back within [`STICK_TOLERANCE`]
    /// lines of the bottom, pressing End, or submitting — re-arms
    /// [`Follow`].
    Hold,
}

impl Stick {
    /// Whether the posture re-anchors the view to the newest line.
    ///
    /// The one question every layout asks of the posture; callers
    /// read a boolean instead of matching the pair, so the anchor
    /// decision stays a single expression wherever the frame builds.
    #[must_use]
    fn follows(self) -> bool {
        matches!(self, Self::Follow)
    }
}

/// Which optional frame sections stand this frame.
///
/// A set, not a flag bundle: every combination occurs (the waiting
/// section and the plan row can stand together or alone), so the
/// frame asks each question it has separately — the same posture the
/// flags spelled one bit per section, kept as one value the frame
/// carries.
#[derive(Clone, Copy, Debug, Default)]
struct Sections(u8);

/// The plan section's bit.
///
/// The set's one value for the plan section: it stands while the
/// run's todo list holds any entry, and the frame asks after it
/// where the todo rows would render.
const SECTION_TODO: u8 = 0b001;

/// The waiting section's bit.
///
/// The set's one value for the waiting section: it stands whenever
/// anything waits in the queue — run or no run — exactly when the
/// panel itself renders.
const SECTION_QUEUE: u8 = 0b010;

/// The place row's bit.
///
/// The set's one value for the place row: it stands once the host
/// names a working directory, and nothing else gates it.
const SECTION_PLACE: u8 = 0b100;

impl Sections {
    /// No section stands.
    ///
    /// The frame's plain furniture only — conversation, notice,
    /// composer, status — with none of the optional sections between
    /// them; the frame starts here and adds what its questions earn.
    const EMPTY: Self = Sections(0);

    /// The set with the plan section added when it stands — the
    /// section that stands while any task does.
    #[must_use]
    const fn add_todo(self, stands: bool) -> Self {
        if stands && self.0 & SECTION_TODO == 0 {
            Sections(self.0 | SECTION_TODO)
        } else {
            self
        }
    }

    /// The set with the waiting section added when it stands — it
    /// stands whenever anything waits.
    #[must_use]
    const fn add_queue(self, stands: bool) -> Self {
        if stands && self.0 & SECTION_QUEUE == 0 {
            Sections(self.0 | SECTION_QUEUE)
        } else {
            self
        }
    }

    /// The set with the place row added when it stands — it stands
    /// once the host names a directory.
    #[must_use]
    const fn add_place(self, stands: bool) -> Self {
        if stands && self.0 & SECTION_PLACE == 0 {
            Sections(self.0 | SECTION_PLACE)
        } else {
            self
        }
    }

    /// Whether the section the bit names stands this frame.
    ///
    /// The read half of the set: the constraint walk asks after each
    /// section in its own order, and the answer is the one bit that
    /// section contributed.
    #[must_use]
    const fn stands(self, section: u8) -> bool {
        self.0 & section != 0
    }
}

/// The main TUI application.
///
/// Owns the event loop, the input buffer, and the shared state
/// background producers write through. Construct with
/// [`TuiApp::new`], which allocates the shared state fresh.
pub struct TuiApp {
    /// The active palette and element styles.
    ///
    /// Resolved from `config.display.theme` at construction; an
    /// unknown name warns and falls back to the default theme.
    pub theme: Theme,

    /// The conversation so far, oldest first.
    ///
    /// Submits append here; the conversation pane renders it and the
    /// scroll offset walks it from the bottom.
    conversation: Vec<TuiMessage>,

    /// The shared state the observer half of the display writes.
    ///
    /// Bundled as one value so the split pattern stays in a single
    /// place: an app built from externally-created state and its
    /// observer hold clones of these same allocations.
    state: TuiObserverState,

    /// Where submits travel to the agent.
    ///
    /// Set by the mode driver before the run loop; Enter sends each
    /// non-empty submit's id through it, the prompt itself staying in
    /// the shared queue for the driver to read at its turn's start.
    /// The channel carries the identity rather than the text because
    /// the text is what the user may still be editing when the id is
    /// handed over — which is also why a correction of a prompt that
    /// already went out carries its text: that wording has no other
    /// holder left. `None` on a bare app — submits render locally and
    /// reach no agent.
    submit_tx: Option<tokio::sync::mpsc::UnboundedSender<Submission>>,

    /// The prompt whose claim is one channel hop away, hidden from the
    /// queue section for that window.
    ///
    /// A submit into an idle session pushes the prompt into the shared
    /// queue and hands its id to the driver; the claim lands a moment
    /// later, and a frame drawn in that window would show the prompt
    /// under the QUEUED title — the flash every fresh session's first
    /// prompt makes. The recorded id is that window's row: the queue
    /// was empty when it was pushed, so it is the section's last row,
    /// and the display simply pays one fewer row until the driver
    /// takes it. The next submit clears it before pushing (at most one
    /// row hides at a time), and a claim or drop retires it through
    /// [`settle_handed`](Self::settle_handed) at the next frame.
    handed: Option<u64>,

    /// The running loop's steering slot, where this session has one.
    ///
    /// Installed by the host beside the submit channel. With it, `Ctrl+S`
    /// offers the composer's text to the turn already in flight instead of
    /// opening another one; without it the chord degrades to an ordinary
    /// submit, which is what a session that cannot interrupt a run should
    /// do with the key.
    guidance: Option<Arc<dyn Guidance>>,

    /// Notified when a turn's outcome lands in the conversation.
    ///
    /// Installed by the mode driver; receives the full conversation
    /// snapshot each time a reply graduates or a failure surfaces, so
    /// the host can persist it without reaching into display state.
    /// `None` on a bare app — turns come and go unpersisted.
    turn_end_hook: Option<TurnEndHook>,

    /// The display verbosity shaping tool lines.
    ///
    /// Initialized from the config and cycled at runtime; switching
    /// invalidates the conversation and live caches so completed
    /// bodies re-render in the new mode on the next frame.
    verbosity: Verbosity,

    /// The input editor: multi-line buffer, cursor, and history.
    ///
    /// Every non-app key routes here. Submits surface as
    /// [`InputAction::Submit`], which joins the prompt to the shared
    /// queue and hands its id down the submit channel; the composer's
    /// own text is empty from that moment, and what a prompt enters the
    /// conversation with is its claim, not this hand-off.
    input: InputEditor,

    /// The column count the composer last rendered at.
    ///
    /// The editor's vertical keys navigate the wrap grid this width
    /// defines; the event loop runs between frames, so the last
    /// rendered width is the geometry the user is looking at when a
    /// key lands.
    input_wrap_width: u16,

    /// What the last frame's composer pane looked like.
    ///
    /// A press inside the pane places the composer's caret where it
    /// landed — the pane rect to claim the press, the interior cell
    /// where the wrap grid begins, and the window of wrap rows the
    /// pane was showing when the user was looking at it.
    input_view: Option<InputView>,

    /// The composer caret's blink state.
    ///
    /// The app owns the caret's blink — the terminal's own
    /// blinking-cursor request is too often ignored or
    /// preference-gated to rely on — and this is the phase the frame
    /// renders from: the caret parks on its cell while visible and
    /// nowhere while dark. [`CaretBlink`] carries the cadence and the
    /// input-reset contract.
    caret_blink: CaretBlink,

    /// Lines scrolled up from the bottom of the view.
    ///
    /// Zero means pinned to the newest line; submitting resets it.
    scroll_offset: usize,

    /// The view's stickiness posture.
    ///
    /// Pinned views re-anchor to the bottom on every layout; any
    /// scroll-up detaches, and scrolling back within
    /// [`STICK_TOLERANCE`] lines of the bottom — or pressing End or
    /// submitting — re-arms.
    stick: Stick,

    /// Whether a capped background redraw is still owed.
    ///
    /// Set when a notify arrives inside the frame interval; the
    /// periodic tick claims it so a stream's tail renders even if no
    /// further delta arrives.
    render_pending: bool,

    /// When the last background-initiated frame rendered.
    ///
    /// Gates redraws to one per [`FRAME_INTERVAL`]; `None` until the
    /// first background frame.
    last_frame: Option<Instant>,

    /// Total lines the previous layout produced.
    ///
    /// While detached, each layout compensates the scroll offset for
    /// growth or shrinkage since this value, holding the viewport
    /// steady as the streaming region extends below it or collapses.
    last_layout_lines: usize,

    /// The mouse selection over the transcript, if any.
    ///
    /// Both ends are display cells — conversation line index plus
    /// column — in the coordinate space of the rendered lines, so
    /// the selection is character-granular and holds still while the
    /// view scrolls. It survives the release — the copy happens then,
    /// the highlight stays up — until the next press replaces it; a
    /// press-release without travel clears it.
    selection: Option<CellSelection>,

    /// The previous frame's conversation viewport.
    ///
    /// Mouse events arrive between frames carrying screen cells;
    /// this maps them onto conversation lines using the geometry of
    /// the frame the user is looking at.
    last_view: Option<ViewState>,

    /// The screen cell the drag last reported, while the button is
    /// held.
    ///
    /// Set between a press inside the conversation pane and its
    /// release. A terminal reports a drag only while the pointer
    /// moves, so a pointer pushed against the pane's edge goes quiet
    /// — the periodic tick reads this to keep scrolling there, one
    /// line per beat, until the release arrives or the document runs
    /// out. A release delivered outside the terminal's view never
    /// arrives as an event; buttonless motion (only sent with no
    /// button held) ends the drag instead, so a lost release cannot
    /// run the scroll for long.
    drag_position: Option<(u16, u16)>,

    /// Where a completed selection's text goes.
    ///
    /// Receives the copy's attempt id beside the text — the same id
    /// any later verdict lands under — and answers synchronously with
    /// what the copy path knows: for the production chain
    /// [`CopyAnswer::Unconfirmed`] while transports are in flight,
    /// its verdict landing later through the copy-outcomes drain; a
    /// test recorder reports its own delivery.
    copier: SelectionCopier,

    /// Verdicts from off-loaded copy transports, awaiting their notice.
    ///
    /// The default copier pushes here from its background task and
    /// pings the render wake; the frame drain turns each verdict into
    /// the definitive notice wording. Test-installed recorders report
    /// synchronously and never touch it.
    copy_outcomes: CopyOutcomes,

    /// The monotonic id source for copy attempts.
    ///
    /// One counter per app: every copy — scheduled or settled on the
    /// spot — takes its id here, so the immediate notice and any
    /// later verdict for the same copy carry the same number.
    copy_attempts: Arc<std::sync::atomic::AtomicU64>,

    /// The attempt whose wording last took the notice row.
    ///
    /// Zero until a copy posts. A drained verdict older than this is
    /// spent news about a copy the user has already walked past, so
    /// the drain drops it instead of overwriting a newer copy.
    posted_copy_attempt: u64,

    /// Rendered lines of the streaming buffer's frozen prefix.
    ///
    /// Blocks before the frozen offset parsed and rendered once, when
    /// their terminating boundary arrived; frames re-use these lines
    /// instead of re-parsing settled content.
    frozen_lines: Vec<Line<'static>>,

    /// Where the streaming buffer's frozen prefix ends, in bytes.
    ///
    /// A safe boundary: the start of the line after a blank line,
    /// outside any fence, where the surrounding blocks cannot
    /// continue each other. Reset alongside the cache whenever the
    /// frozen prefix stops matching the buffer or the pane width
    /// changes.
    frozen_upto: usize,

    /// Separator rows owed after the frozen prefix.
    ///
    /// The blank run an interior boundary consumed becomes empty
    /// rows at the next join, matching the batch render's spacing
    /// for blank-separated blocks; a boundary taken at a closed
    /// fence owes the renderer's one inter-component row even
    /// though the source carried no blank, so the count floors at
    /// one. A run that precedes any frozen content is the
    /// document-leading run and follows the batch's absorb-one rule
    /// instead. Footnote definitions, rule-adjacent boundaries,
    /// indented fences inside lists, and a fence closer followed
    /// directly by a heading or the next list item render
    /// one-to-two rows off their batch spacing at the join — an
    /// accepted mid-stream approximation that self-corrects at
    /// graduation.
    frozen_separators: usize,

    /// Identity stamp of the frozen prefix.
    ///
    /// Recomputed whenever the freeze advances and re-checked on
    /// every layout: a buffer cleared and refilled to a comparable
    /// length fails the stamp and resets the cache, which a length
    /// comparison alone cannot distinguish from growth.
    frozen_fingerprint: u64,

    /// The pane width the streaming cache was built for.
    ///
    /// A resize discards the cache; the next layout re-freezes the
    /// same content at the new width.
    stream_cache_width: u16,

    /// Rendered lines of the streaming buffer's live segment.
    ///
    /// The live segment — the unfrozen complete lines plus the
    /// unterminated tail — only changes when a delta arrives, the
    /// freeze advances, or the pane resizes; input keystrokes and
    /// idle ticks re-render it identically. Frames re-use these
    /// lines until the segment's stamp moves, so a keystroke never
    /// pays the markdown re-parse (a live open code fence
    /// re-highlighted per frame costs frame-budget-breaking time).
    live_lines: Vec<Line<'static>>,

    /// Stamp of the live segment the cached lines were built from.
    ///
    /// A fingerprint of the unfrozen buffer suffix — content, not
    /// just length, so a cleared-and-refilled turn invalidates.
    live_stamp: u64,

    /// The freeze offset the live cache was built at.
    ///
    /// A live rebuild is valid only for the frozen prefix it
    /// started from; when the freeze advances, the cached lines
    /// describe a different suffix and this key forces the rebuild.
    live_frozen_upto: usize,

    /// The pane width the live cache was built at.
    ///
    /// Wrapped live lines are width-shaped; a resize re-wraps
    /// rather than reusing, so a narrowed pane never clips rows
    /// baked for the old width.
    live_width: u16,

    /// Rendered lines of the settled conversation.
    ///
    /// The conversation changes only when a message graduates, a
    /// claimed prompt takes its row, a tool result drains, or an error
    /// lands —
    /// everything else a frame does to it is re-rendering identical
    /// markdown. The cache rebuilds on those mutations and on a width
    /// change; frames clone it, the same per-frame linear trait the
    /// streaming freeze cache has.
    conversation_cache: Vec<Line<'static>>,

    /// The baked prefix of [`conversation_cache`], growing by append.
    ///
    /// [`conversation_cache`] is this bake plus the last message's
    /// fresh tail, recomposed per rebuild — the bake is the only
    /// long-lived buffer, so a graduation never re-renders the baked
    /// lines' sources, only composes them beside a fresh tail.
    conversation_bake: Vec<Line<'static>>,

    /// The frame lane's own bookkeeping, for the log.
    ///
    /// One line per second under `RUST_LOG=dch_tui::frames=debug`:
    /// how many animation wakes fired, how many terminal draws ran,
    /// and the slowest draw's duration — the three numbers that
    /// separate a stalled wake lane (wakes low), a blocked loop
    /// (wakes high, draws low), and a slow terminal (draws high with
    /// a large worst draw) at a glance.
    frame_stats: FrameStats,

    /// The pane width the conversation cache was built for.
    ///
    /// Settled markdown re-flows with the pane; a width change
    /// invalidates the cache wholesale so the next frame rebuilds
    /// every block at the new wrap.
    conversation_cache_width: u16,

    /// Monotonic count of settled-conversation mutations.
    ///
    /// Every message push and verbosity switch bumps it; the render
    /// cache detects staleness by falling behind, which keeps the
    /// comparison against the width key uniform.
    conversation_generation: u64,

    /// The generation the conversation cache was built at.
    ///
    /// Paired with
    /// [`conversation_generation`](Self::conversation_generation):
    /// the cache is stale exactly when the live count has moved
    /// past this stamp, whatever caused the bump.
    conversation_cache_generation: u64,

    /// How many leading messages the cache has baked, never to
    /// re-render.
    ///
    /// Messages are immutable once a newer one follows — only the
    /// last is ever patched — so the bake grows by appending, and a
    /// graduation re-renders only the messages after it. A width
    /// change or a truncation below it resets the bake whole.
    conversation_baked_through: usize,

    /// The speaker of the last baked message, for the seam rule.
    ///
    /// The blank row between speaker changes is decided by the
    /// previous message's kind; the incremental bake needs the baked
    /// prefix's answer to render the next message's seam correctly.
    conversation_baked_prev_user: Option<bool>,

    /// The bake's own index metadata, in bake-absolute rows.
    ///
    /// Grown only as messages bake and cleared with the bake itself;
    /// the per-frame view products the painters and click paths read
    /// are rebuilt from it and the rebased tail every frame.
    conversation_bake_meta: BakeMeta,

    /// Conversation-cache line ranges spanned by user turns, as
    /// `(tint_start, tint_end, mark_row)`.
    ///
    /// The render paints each range as a full-width raised block on the
    /// theme's surface — the prompt box's own color — around the turn's
    /// text, its padding rows included, and overlays the chevron at the
    /// turn's first text row. Chrome beside and beneath the content,
    /// never spans inside it, so selections and copies walk only the
    /// text. Rebuilt together with the conversation cache: the bake's
    /// absolute ranges plus the tail's, rebased onto the cache's own
    /// rows, so a tail user turn is boxed the frame it lands.
    user_turn_ranges: Vec<(usize, usize, usize)>,

    /// The quit lifecycle.
    ///
    /// Exit is a deliberate chord, not a slip: the state carries
    /// both halves of it.
    quit: QuitState,

    /// The flag the engine driver gates every claim and start on.
    ///
    /// The exit's share of the quit contract: the press that fires
    /// the ladder raises this before calling the canceller, so the
    /// cancel the exit itself causes cannot release the driver's
    /// claim on the queue's next prompt — a claim made after the
    /// fire spends a prompt the exit still believes waiting, and
    /// resumes it forever as a stop no one asked for. Hosts that
    /// never install one (tests, headless) carry a flag nobody
    /// raises.
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,

    /// How a double Esc cancels the run in flight, when one is.
    ///
    /// Installed by the mode driver alongside the shared running
    /// flag; absent in tests and headless hosts, where a cancel
    /// press simply reports handled.
    run_canceller: Option<Box<dyn Fn()>>,

    /// Whether one Esc has armed the run cancel.
    ///
    /// True only between the arming press and the Esc that confirms
    /// it or any other key that stands it down — cancel is a
    /// deliberate double press, so one stray Esc cannot kill a run
    /// mid-flight.
    cancel_arm: bool,

    /// Tool calls whose blocks render expanded.
    ///
    /// Keyed by call id; a click on a summary row toggles membership.
    /// Running calls share the key with their completed block, so an
    /// expansion opened while the tool runs survives its graduation.
    expanded_tools: std::collections::HashSet<String>,

    /// The todo block the last update auto-expanded, folded again
    /// once the conversation moves on.
    ///
    /// A `TodoWrite` result expands so the reader sees the plan as it
    /// stands; the expansion is momentary — the next graduation, or
    /// the next tool starting, folds it back to its summary row (the
    /// standing plan panel carries the list from there). Manual
    /// expansions never ride this: the field tracks only the one
    /// auto-expanded block, and once folded it is done.
    auto_expanded_todo: Option<String>,

    /// The session's thinking-display preference.
    ///
    /// Follow renders live reasoning expanded and auto-collapses at
    /// graduation; Suppressed renders every stream collapsed from its
    /// first delta. Only clicks on the live row change it — completed
    /// sections toggle their own display and leave this untouched.
    /// Session-only: never persisted, every session starts in Follow.
    thinking_mode: ThinkingMode,

    /// Which completed thinking sections render expanded, by block key.
    ///
    /// Keys are message-and-block positions, stable for the session
    /// because the conversation only ever appends. Empty means every
    /// section renders collapsed — the default both after graduation
    /// and after loading a session.
    expanded_thinking: std::collections::HashSet<String>,

    /// Which compaction notices render expanded, by message key.
    ///
    /// Same positional-key scheme as the thinking sections, so the
    /// expand gesture covers the notices with one vocabulary.
    expanded_compactions: std::collections::HashSet<String>,

    /// Conversation lines that sit inside a thinking section, by key.
    ///
    /// Mirrors [`tool_summary_lines`](Self::tool_summary_lines): every
    /// line a click can toggle, mapped to the key it belongs to.
    thinking_summary_lines: HashMap<usize, String>,

    /// Transcript row to compaction-notice key, rebuilt per layout.
    ///
    /// The press handler's map for the notice rows, mirroring the
    /// thinking map beside it.
    compaction_summary_lines: HashMap<usize, String>,

    /// Where the activity strip renders while the agent runs.
    ///
    /// Carried from the display config: the spacer row directly above
    /// the composer, or a dedicated first row of the terminal. The
    /// strip's phase and gating are placement-independent.
    strip_position: StripPosition,

    /// The animation the activity strip plays.
    ///
    /// Carried from the display config: the centered breath, or the
    /// left-anchored fill that clears left-to-right. Style and
    /// position are orthogonal; the phase and gating are the same
    /// either way.
    strip_style: StripStyle,

    /// The live thinking block's absolute rows, when one renders.
    ///
    /// Every rendered row of the live section — the marker row and
    /// the expanded body alike — drives the Follow/Suppressed mode,
    /// matching tool blocks where any row toggles; `None` when no
    /// stream is living.
    thinking_live_rows: Option<std::ops::Range<usize>>,

    /// When the current run started, for the strip's anchor.
    ///
    /// Stamped the first frame the agent-running flag is observed
    /// set and cleared when it clears, so the bar breathes one
    /// continuous phase per run; no stream property ever moves it.
    run_anchor: Option<Instant>,

    /// Full command and output per tool call, keyed by call id.
    ///
    /// Filled from the dispatch-side capture as calls graduate; a
    /// block renders its expansion from here. Runtime-only — the
    /// serialized session keeps previews, not payloads — and
    /// bounded to the most recent `TOOL_DETAIL_CAP` calls in FIFO
    /// order, so a long session's oldest blocks fold back to plain
    /// summary rows instead of retaining payloads forever.
    tool_details: HashMap<String, ToolDetail>,

    /// The conversation row each retry episode's bar lives in, keyed
    /// by episode id.
    ///
    /// The first attempt of an episode pushes a `System` row and
    /// records its index; later attempts of the same episode rewrite
    /// that row in place, so one rough patch reads as one line that
    /// counts up. Rows are never removed, so the recorded indices
    /// stay stable for the session's lifetime.
    retry_row_index: HashMap<u64, usize>,

    /// Graduation order of the retained tool details, oldest first.
    ///
    /// The eviction queue behind the detail cap's FIFO: the front
    /// names the entry the next graduation retires.
    tool_detail_order: std::collections::VecDeque<String>,

    /// Conversation line index to call id, for tool block rows.
    ///
    /// The click targets: every line of each completed block that
    /// has retained detail — summary and expansion alike, so a
    /// click anywhere on an open block folds it. Rebuilt with the
    /// conversation cache.
    tool_summary_lines: HashMap<usize, String>,

    /// Line index to call id, for running tool block rows.
    ///
    /// The same click targets for in-flight calls — the row and its
    /// expansion — in the line space the tools segment occupies;
    /// recorded per frame.
    tool_running_lines: HashMap<usize, String>,

    /// The application configuration this shell was built from.
    ///
    /// The status bar reads the model name from it.
    config: DchConfig,

    /// The session's id, as the status bar shows it.
    ///
    /// Set by the host once the session's identity is known — the
    /// resumed file's own id, or the runner's fresh one — so the
    /// user can read (and copy out) which session they are in.
    session_id: Option<String>,

    /// Screen span of the session-id segment as last rendered.
    ///
    /// `(row, start column, end column)` in terminal coordinates, set
    /// during status-bar rendering — which borrows the app immutably,
    /// hence the `Cell` — and read by the click handler: a plain click
    /// inside the span copies the id through the selection transport.
    /// `None` when no id is displayed or the segment is fully clipped.
    session_id_span: std::cell::Cell<Option<(u16, u16, u16)>>,

    /// The session's standing todo list, for the plan section.
    ///
    /// Updated at every accepted `TodoWrite` graduation — the enforced
    /// render first, the submission as fallback — so the pinned section
    /// shows the plan the run actually holds, however many updates the
    /// transcript holds. A rejected update is not state: the tool
    /// refused the submission and the last accepted list stands. The
    /// plan belongs to the session, not the run: it stays up after its
    /// run lands and stands across claims while work remains — carried
    /// exactly as the runner carries its own store — and a claim
    /// retires it only when every entry stands completed, the same
    /// edge the runner clears its store at. Steering delivers mid-run
    /// and never claims, so a plan keeps updating through its own run.
    latest_todos: Vec<todo_view::TodoRow>,

    /// The rows each todo update changed, by the update's call id.
    ///
    /// The conversation's per-update block accents exactly these
    /// rows: the ids whose status moved in that call, against the
    /// snapshot the previous call left. Bounded with the transcript
    /// itself; the map rides in memory only — a resumed session
    /// accents nothing, which reads correctly as "no deltas known".
    todo_changes: HashMap<String, HashMap<String, todo_view::TodoStatus>>,

    /// The line ranges of the transcript's expanded todo blocks.
    ///
    /// Recorded while the conversation cache is assembled — one range
    /// per `TodoWrite` block whose checkbox rows are on screen — so the
    /// scrolled view can keep each block whole: a severed list next to
    /// the plan badge's standing fraction reads as a count the frame
    /// does not carry. Indexes are cache lines, rebuilt with the cache.
    todo_block_ranges: Vec<std::ops::Range<usize>>,

    /// The rows the newest todo update changed, for the plan section.
    ///
    /// The pinned section accents what the last update moved — the
    /// same deltas the newest conversation block carries, kept
    /// beside the list they name so the section can say "this is
    /// what just moved" without knowing which call id was newest.
    todo_section_changes: HashMap<String, todo_view::TodoStatus>,

    /// Whether the run that last landed was interrupted.
    ///
    /// An interrupted run's plan is owed work — the session resumes
    /// into it — so the next claim retires nothing: the plan stands
    /// until the resumed run files its own. Set by the [`RunEnded`](crate::observer::Graduation::RunEnded) arm
    /// of the drain when `success` is false, consumed and cleared by
    /// the Sent arm; a run that files no plan leaves the flag
    /// irrelevant either way.
    plan_interrupted: bool,

    /// The plan section's posture, as Ctrl+T leaves it.
    ///
    /// Capped by default — four task rows and the ellipsis — with one
    /// press opening the whole list and the next folding the section
    /// away. A view preference, not a mode: no keys are owned, the
    /// plan keeps updating underneath, and every claim starts the
    /// next run's plan from the capped posture whatever the last run
    /// left.
    plan_view: PlanView,

    /// The working directory's own name, as the status bar shows it.
    ///
    /// Set by the host at launch with the directory the runner
    /// operates in, reduced to its leaf — the path that leads to it
    /// never shows — so a frame paints it without touching the
    /// filesystem.
    working_dir: Option<String>,

    /// The git branch beside the working directory's name.
    ///
    /// Set by the host at launch from the workdir's repository; the
    /// bar renders it in parentheses after the folder's name, and
    /// nothing when the workdir sits in no working tree.
    git_branch: Option<String>,

    /// The transient notice on the row above the composer, with
    /// the instant its hold elapses.
    ///
    /// A glance-worthy event — a cancelled run — that is not a
    /// transcript row: it shows for
    /// [`NOTICE_HOLD`](self::NOTICE_HOLD) and leaves the reserved
    /// row blank again.
    transient_notice: Option<(String, Instant)>,

    /// Which waiting prompt the queue section shows.
    ///
    /// A display position rather than an identity: the ids that survive a
    /// shift are the queue's business, and this is only the row the arrow
    /// keys last stood on. [`clamp_queue_view`](Self::clamp_queue_view)
    /// keeps it inside the queue, because a prompt claimed at the front
    /// pulls every row behind it up a place.
    queue_view: usize,

    /// Whose hands the keyboard is in, as far as waiting prompts go.
    ///
    /// [`QueueMode`] is the whole story: the composer's, the section's, or
    /// a prompt out for editing. The mode cannot outlive the section — an
    /// emptied queue takes the keys back by itself, since a mode that
    /// swallows every key for a panel no longer drawn is indistinguishable
    /// from a hung terminal.
    queue_mode: QueueMode,

    /// The composer's cached wrap, if it is still fresh.
    ///
    /// Valid for one buffer mutation at one width — see
    /// [`composer_wrap`](Self::composer_wrap).
    input_wrap_cache: Option<InputWrapCache>,

    /// Permission asks waiting for the user's answer, oldest first.
    ///
    /// The gate resolves `Ask` cells by handing the UI one request per
    /// pending tool call; the front of the queue is what the overlay
    /// shows and what a keypress answers. A finished or cancelled run
    /// empties it — the gate has already denied anything unanswered
    /// against the cancel signal.
    pending_permissions: std::collections::VecDeque<PermissionRequest>,

    /// The channel permission requests arrive on, until `run` claims
    /// it.
    ///
    /// Installed by the host before the run loop starts; the loop's
    /// select arm moves requests into the queue, so between runs the
    /// slot simply holds the receiver.
    permission_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>>,

    /// Questions from the asking tool waiting for the user's answer,
    /// oldest first.
    ///
    /// The host converts the tool's batched requests into prompts and
    /// lands them here; the front of the queue is what the overlay
    /// shows and what the keys answer. Answering sends on the
    /// prompt's one-shot and advances to the next question of the
    /// batch. A finished or cancelled run empties the queue — the
    /// dispatches are gone, and dropping the prompts closes their
    /// reply channels, which the asking tools read as dismissals.
    ///
    /// Each entry carries the answer being composed for its own
    /// question, so walking the batch with Left and Right changes only
    /// which question is shown — nothing already marked or typed is
    /// left behind on the way.
    pending_questions: std::collections::VecDeque<PendingQuestion>,

    /// Which queued question is on screen: an index into
    /// [`pending_questions`](Self::pending_questions).
    ///
    /// The walk moves this and leaves the queue's arrival order alone, so
    /// a question keeps its number in the sheet's title while the user
    /// walks away from it and back, and the count names the batch the
    /// asking tool sent. Always a live index: landing only lengthens the
    /// queue, and answering lifts the shown entry out with the next one
    /// sliding into its place.
    question_view: usize,

    /// The channel question prompts arrive on, until `run` claims
    /// it.
    ///
    /// Installed by the host's bridge before the run loop starts; the
    /// loop's select arm moves prompts into the queue, mirroring the
    /// permission channel's shape.
    question_rx: Option<tokio::sync::mpsc::UnboundedReceiver<QuestionPrompt>>,

    /// The instant the terminal regained focus and a whole-screen
    /// repaint is still owed, if one is.
    ///
    /// Set when the terminal regains focus: some terminals skip
    /// painting while their window is hidden or occluded, so frames
    /// drawn during that time leave stale cells that the incremental
    /// diff cannot know about. The run loop claims it, clears the
    /// screen, and lets the next draw repaint every cell — the same
    /// full repaint a resize forces.
    full_repaint_due: Option<Instant>,
}

/// The accumulators one conversation render pass fills.
///
/// A render of a message range produces the styled rows themselves
/// and the five index structures the app's press resolution and
/// lane overlays read — one output, always filled together, so it
/// folds into a single struct the pass fills. The bake pass owns
/// the half the app keeps between frames; the tail pass starts
/// empty each time and merges its result over the baked half.
#[derive(Default)]
struct BakeOutputs {
    /// The rendered rows, in conversation order.
    ///
    /// One entry per visual line; every index structure below keys
    /// positions in this vector.
    lines: Vec<Line<'static>>,

    /// Row spans registered under their tool-call id.
    ///
    /// Every row a foldable tool block produced maps to the call it
    /// belongs to, so a press on any of them resolves the toggle.
    tool_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their thinking section's key.
    ///
    /// The completed thinking sections' fold rows, keyed the way the
    /// tool rows are for the same press resolution.
    thinking_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their compaction notice's key.
    ///
    /// A notice's rows map to its message key so a press anywhere on
    /// the notice resolves its own toggle.
    compaction_summary_lines: HashMap<usize, String>,

    /// Each user turn's row span, with its mark row.
    ///
    /// `(tint_start, turn_end, mark_row)` triples: the span the
    /// caller tints as the turn's box, and the row the prompt mark
    /// draws on.
    user_turn_ranges: Vec<(usize, usize, usize)>,

    /// The row spans of the conversation's inline todo blocks.
    ///
    /// Whole-block ranges, so the scroll can keep an inline plan
    /// render visible in full or not at all.
    todo_block_ranges: Vec<std::ops::Range<usize>>,
}

/// The bake's own index metadata, in bake-absolute rows.
///
/// What [`BakeOutputs`] carries for one render pass, kept for the
/// baked prefix alone: the index structures the frame's merged view
/// products are rebuilt from every frame, each rebased by the bake's
/// line count before the painters or the click paths ever see them.
#[derive(Default)]
struct BakeMeta {
    /// Row spans registered under their tool-call id.
    ///
    /// Every row a foldable baked tool block produced maps to the
    /// call it belongs to, so a press on any of them resolves the
    /// toggle.
    tool_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their thinking section's key.
    ///
    /// The baked thinking sections' fold rows, keyed the way the
    /// tool rows are for the same press resolution.
    thinking_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their compaction notice's key.
    ///
    /// A baked notice's rows map to its message key so a press
    /// anywhere on the notice resolves its own toggle.
    compaction_summary_lines: HashMap<usize, String>,

    /// Each baked user turn's row span, with its mark row.
    ///
    /// `(tint_start, turn_end, mark_row)` triples in bake-absolute
    /// rows, grown as turns bake and never holding the tail's
    /// relative rows.
    user_turn_ranges: Vec<(usize, usize, usize)>,

    /// The row spans of the baked conversation's inline todo blocks.
    ///
    /// Whole-block ranges in bake-absolute rows, for the scroll's
    /// whole-or-nothing todo-block rule.
    todo_block_ranges: Vec<std::ops::Range<usize>>,
}

impl TuiApp {
    /// Construct the shell with freshly-allocated, empty shared state.
    ///
    /// The TUI renders and responds to input; the shared buffers stay
    /// empty until a producer writes them.
    #[must_use]
    pub fn new(config: DchConfig) -> Self {
        Self::from_observer_state(config, TuiObserverState::new())
    }

    /// Construct the shell around externally-created shared state.
    ///
    /// The retained half of a state split through
    /// [`TuiObserverState::into_observer`] — the observer half goes
    /// to the agent — so writes from the running agent reach this
    /// app's display through the same allocations.
    pub fn from_observer_state(config: DchConfig, state: TuiObserverState) -> Self {
        let theme = if let Some(theme) = Theme::by_name(&config.display.theme) {
            theme
        } else {
            tracing::warn!("unknown theme '{}', using default", config.display.theme);
            Theme::default()
        };
        let copy_command = config.display.copy_command.clone();
        let copy_outcomes: CopyOutcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let copier_outcomes = Arc::clone(&copy_outcomes);
        let copy_attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let copy_wake = Arc::clone(&state.render_notify);
        Self {
            theme,
            conversation: Vec::new(),
            state,
            submit_tx: None,
            handed: None,
            guidance: None,
            turn_end_hook: None,
            verbosity: config.display.verbosity,
            input: InputEditor::new(),
            input_wrap_width: 80,
            input_view: None,
            caret_blink: CaretBlink::default(),
            scroll_offset: 0,
            stick: Stick::default(),
            render_pending: false,
            last_frame: None,
            last_layout_lines: 0,
            selection: None,
            last_view: None,
            drag_position: None,
            copier: Box::new(move |attempt, text| {
                copy_via_transports(
                    copy_command.as_deref(),
                    attempt,
                    text,
                    &copier_outcomes,
                    &copy_wake,
                )
            }),
            copy_outcomes,
            copy_attempts,
            posted_copy_attempt: 0,
            frozen_lines: Vec::new(),
            frozen_upto: 0,
            frozen_separators: 0,
            frozen_fingerprint: 0,
            stream_cache_width: 0,
            live_lines: Vec::new(),
            live_stamp: 0,
            live_frozen_upto: 0,
            live_width: 0,
            conversation_cache: Vec::new(),
            conversation_bake: Vec::new(),
            frame_stats: FrameStats::default(),
            conversation_cache_width: 0,
            user_turn_ranges: Vec::new(),
            conversation_generation: 1,
            conversation_cache_generation: 0,
            conversation_baked_through: 0,
            conversation_baked_prev_user: None,
            conversation_bake_meta: BakeMeta::default(),
            quit: QuitState::default(),
            shutdown_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            run_canceller: None,
            cancel_arm: false,
            expanded_tools: std::collections::HashSet::new(),
            auto_expanded_todo: None,
            thinking_mode: ThinkingMode::Follow,
            expanded_thinking: std::collections::HashSet::new(),
            expanded_compactions: std::collections::HashSet::new(),
            thinking_summary_lines: HashMap::new(),
            compaction_summary_lines: HashMap::new(),
            strip_position: config.display.strip_position,
            strip_style: config.display.strip_style,
            thinking_live_rows: None,
            run_anchor: None,
            tool_details: HashMap::new(),
            retry_row_index: HashMap::new(),
            tool_detail_order: std::collections::VecDeque::new(),
            tool_summary_lines: HashMap::new(),
            tool_running_lines: HashMap::new(),
            input_wrap_cache: None,
            session_id: None,
            session_id_span: std::cell::Cell::new(None),
            latest_todos: Vec::new(),
            todo_changes: HashMap::new(),
            todo_block_ranges: Vec::new(),
            todo_section_changes: HashMap::new(),
            plan_interrupted: false,
            plan_view: PlanView::Capped,
            working_dir: None,
            git_branch: None,
            transient_notice: None,
            queue_view: 0,
            queue_mode: QueueMode::Plain,
            pending_permissions: std::collections::VecDeque::new(),
            permission_rx: None,
            pending_questions: std::collections::VecDeque::new(),
            question_view: 0,
            question_rx: None,
            full_repaint_due: None,
            config,
        }
    }

    /// The conversation so far, oldest first.
    ///
    /// A borrowed view; append through [`push_message`](Self::push_message).
    #[must_use]
    pub fn conversation(&self) -> &[TuiMessage] {
        &self.conversation
    }

    /// Append a message to the conversation.
    ///
    /// The observer and the session-resume path use this to populate
    /// the view; submitting from the input line goes through the same
    /// path internally. A fold of the auto-expanded plan retires the
    /// rows it baked while the flag stood: a mid-conversation block is
    /// past the tail the incremental bake re-renders, so the fold
    /// invalidates the whole bake rather than leave an expanded shape
    /// standing on a flag it just cleared.
    pub fn push_message(&mut self, message: TuiMessage) {
        if let Some(id) = self.auto_expanded_todo.take()
            && self.expanded_tools.remove(&id)
        {
            self.rebake_whole();
        }
        self.conversation.push(message);
        self.conversation_generation = self.conversation_generation.saturating_add(1);
    }

    /// Post a transient notice above the composer.
    ///
    /// Replaces any notice still holding — one row, one message,
    /// the newest wins.
    pub fn post_notice(&mut self, text: String) {
        let until = Instant::now()
            .checked_add(NOTICE_HOLD)
            .unwrap_or_else(Instant::now);
        self.transient_notice = Some((text, until));
    }

    /// Name the session the status bar shows.
    ///
    /// The host calls this once the session's identity settles —
    /// before the run loop starts, so the first frame already
    /// carries it.
    pub fn set_session_id(&mut self, id: String) {
        self.session_id = Some(id);
    }

    /// Name the working directory the place row shows.
    ///
    /// The host calls this once at launch with the directory the
    /// runner operates in, and the row spells it per the config's
    /// [`PathStyle`] — the full path, or the `~/`-relative shape —
    /// with the branch in parentheses when one was named.
    pub fn set_working_dir(&mut self, dir: &Path) {
        self.working_dir = Some(dir.display().to_string());
    }

    /// Name the git branch shown beside the working directory.
    ///
    /// The host calls this once at launch with the workdir's
    /// checked-out branch, when it sits in a working tree; the place
    /// row renders it in parentheses after the path, and no
    /// parenthesis at all when no branch was named.
    pub fn set_git_branch(&mut self, branch: Option<String>) {
        self.git_branch = branch;
    }

    /// Persist the conversation now, whatever the run's state.
    ///
    /// The turn-end hook usually fires on the drain that observes a
    /// turn's end — but a run wedged on a silent stream never ends,
    /// and the exit must not wait for it: this fires the same hook
    /// with the conversation as it stands (the claim rows the run
    /// already wrote) and whatever machine history is newest, so the
    /// session file carries everything the user saw at the moment of
    /// leaving. Idempotent with the drain's own firing — the worker
    /// keeps the newest save.
    pub fn flush_turn_end(&mut self) {
        if let Some(hook) = &self.turn_end_hook {
            let machine = self
                .state
                .machine_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            hook(&self.conversation, machine.as_ref());
        }
    }

    /// The waiting prompts' texts, as the exit finds them.
    ///
    /// Read after the shutdown flag rises and before the exit's cancel
    /// fires, the one window in which every entry in the shared queue
    /// is a prompt no run ever claimed: the run in flight took its own
    /// entry when it started, the exit's requeues land only after the
    /// cancel, and a claim reaching the driver inside the window
    /// declines under the queue's own lock rather than taking its
    /// entry. The snapshot is therefore exactly the set of words the
    /// user typed that no turn spent — the set the transcript owes
    /// rows to, because the queue they wait in dies with the process.
    #[must_use]
    pub fn waiting_prompt_texts_at_exit(&self) -> Vec<String> {
        let queue = self
            .state
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (0..queue.len())
            .filter_map(|index| queue.get(index).map(|entry| entry.text.clone()))
            .collect()
    }

    /// Write the still-waiting prompts into the transcript as
    /// unclaimed rows.
    ///
    /// The one place a user row is written without a send behind it:
    /// the queue is process state, the process is leaving, and words
    /// the user typed would otherwise leave no trace anywhere — the
    /// interrupted claim keeps the row its send earned, but a prompt
    /// no turn took has nothing. The rows land after the stop's own
    /// record, in queue order, so a resume reads the transcript's end
    /// as exactly what still stood and seeds every one of them back
    /// into the waiting section. The flush beside them is the same
    /// unconditional persist the settle performs — an exit with no
    /// run in flight skips the settle entirely, and these rows must
    /// outlive that exit too.
    pub fn echo_waiting_prompts_at_exit(&mut self, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        for text in texts {
            self.conversation.push(TuiMessage::User {
                text,
                timestamp: chrono::Utc::now(),
                steered: false,
            });
        }
        self.flush_turn_end();
    }

    /// Write the corrections the exit collected into the transcript
    /// as unclaimed rows.
    ///
    /// The drain half of the exit's correction rescue: the driver
    /// collects each correction still buffered at its exit breaks —
    /// words that left the queue with the claim, travelled only in
    /// the channel, and would otherwise leave no trace anywhere — and
    /// this runs once the driver task has settled, after the waiting
    /// prompts' own echo, so the transcript ends on everything the
    /// user typed that no turn spent. The flush beside the rows is
    /// the same unconditional persist the settle performs; a resume
    /// seeds each row back into the waiting section.
    pub fn echo_rescued_corrections_at_exit(&mut self) {
        let texts: Vec<String> = std::mem::take(
            &mut *self
                .state
                .rescued_corrections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if texts.is_empty() {
            return;
        }
        for text in texts {
            self.conversation.push(TuiMessage::User {
                text,
                timestamp: chrono::Utc::now(),
                steered: false,
            });
        }
        self.flush_turn_end();
    }

    /// Land an exit-confirmed stop as the record a cancel leaves.
    ///
    /// An exit confirmed under a running agent is a cancel first:
    /// [`cancel_run_in_flight`](Self::cancel_run_in_flight) fires
    /// while the frame loop still lives, and frames pump until the
    /// run settles — the stop drains into the conversation as the
    /// same record an in-app cancel leaves, the turn-end hook
    /// persists it, and the resumed session reads one shape whichever
    /// way the run was stopped. With nothing running this is a no-op.
    pub fn settle_exit_stop(
        &mut self,
        terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
    ) {
        if !self.cancel_run_in_flight() {
            return;
        }
        // One frame the moment the fire lands: the screen shows the
        // cancel's record at once — the manual sequence's own
        // feedback, which is what makes its wait feel live — and the
        // wait that follows reads as the exit doing its work rather
        // than a freeze.
        // The snapshot precedes even the feedback frame: the frame's
        // own render drains, and a record that landed in that drain
        // must count as landed — the check below scans from here.
        let before = self.conversation.len();
        drop(terminal.draw(|frame| self.render(frame)));
        // The exit does not wait for the engine to unwind: the manual
        // sequence's cancel unwinds in the background of a live UI,
        // and the fire's exit owes the same immediacy. What the
        // unwind would have landed, the exit lands itself — the lanes
        // as they stood, the record row, the flush — and the
        // teardown's bounded abort handles whatever the engine is
        // still doing.
        //
        // The lanes first, exactly as they stood at the fire: the
        // thinking on screen rides ahead of whatever lands next, so
        // the transcript reads [prompt, the work so far, the record]
        // in the order the run actually happened.
        self.capture_live_lanes_at_exit();
        // One drain: whatever the cancel resolved before the exit got
        // here — an instant settle's record row, a completed turn's
        // graduation — lands now, behind the captured lanes.
        self.drain_shared_state();
        // The record the manual sequence always has, deterministically:
        // only when neither the drain nor anything else landed one.
        let record_landed = self.conversation.get(before..).is_some_and(|landed| {
            landed
                .iter()
                .any(|message| matches!(message, TuiMessage::Error { text, .. } if text == "Agent cancelled"))
        });
        if !record_landed {
            self.conversation.push(TuiMessage::Error {
                text: "Agent cancelled".to_string(),
                timestamp: chrono::Utc::now(),
            });
        }
        self.flush_turn_end();
    }

    /// Snapshot the run's unfinished lanes into the conversation.
    ///
    /// A cancelled run graduates nothing: the thinking it streamed
    /// and the partial reply text stay in the live lanes and would
    /// leave with the process. At the exit they join the transcript
    /// as their own assistant row — the honest shape of a turn that
    /// was stopped mid-flight, and what a resumed session reads
    /// instead of an empty gap between the prompt and the record.
    fn capture_live_lanes_at_exit(&mut self) {
        let mut blocks = Vec::new();
        let thinking = self
            .state
            .thinking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !thinking.text.trim().is_empty() {
            blocks.push(ContentBlock::Thinking {
                text: thinking.text.clone(),
                redacted: false,
                duration_ms: 0,
            });
        }
        drop(thinking);
        let stream = self
            .state
            .streaming_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !stream.trim().is_empty() {
            blocks.push(ContentBlock::Text { text: stream });
        }
        if !blocks.is_empty() {
            self.conversation.push(TuiMessage::Assistant {
                blocks,
                timestamp: chrono::Utc::now(),
                duration_ms: None,
            });
        }
    }

    /// Cancel the run in flight, when one is.
    ///
    /// The exit path's handshake: an exit confirmed under a running
    /// agent fires the canceller here — while the frame loop still
    /// drains — so the stop lands as the same record an in-app
    /// cancel leaves, with the turn's end hook persisting it, before
    /// the teardown owns the screen. Returns whether a run was in
    /// flight to stop.
    pub fn cancel_run_in_flight(&mut self) -> bool {
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        if let Some(cancel) = &self.run_canceller {
            cancel();
        }
        true
    }

    /// Install the run canceller.
    ///
    /// The mode driver's half of the Ctrl+C contract: while
    /// [`agent_running`](TuiObserverState::agent_running) is set, a
    /// Ctrl+C press on an empty composer calls this to stop the
    /// submission in flight.
    pub fn set_run_canceller(&mut self, canceller: Box<dyn Fn()>) {
        self.run_canceller = Some(canceller);
    }

    /// Install the shared shutdown flag the engine driver gates on.
    ///
    /// The display's half of the exit contract: the press that fires
    /// the quit ladder stores this flag before calling the run
    /// canceller, so the driver — whose next claim the exit's own
    /// cancel would otherwise release — refuses every submission
    /// queued behind a quitting user. An ordinary cancel, Esc-Esc
    /// or a correction's, never touches it: those release the queue
    /// by design.
    pub fn set_shutdown_flag(&mut self, flag: Arc<std::sync::atomic::AtomicBool>) {
        self.shutdown_flag = flag;
    }

    /// Retire the plan section when its work is finished.
    ///
    /// The plan belongs to the session, not the run: a claim retires it
    /// only when every entry stands completed — the same edge the runner
    /// clears its own carried store at — while an unfinished plan keeps
    /// standing across the claim exactly as the store carries it, so the
    /// section never desyncs from the plan the model and the belts are
    /// working against. An empty plan retires trivially; an interrupted
    /// run's caller never reaches here at all.
    fn retire_finished_plan(&mut self) {
        if self
            .latest_todos
            .iter()
            .all(|row| row.status == todo_view::TodoStatus::Completed)
        {
            self.latest_todos.clear();
            self.todo_section_changes.clear();
        }
    }

    /// Install a whole prior conversation as the session's starting
    /// state.
    ///
    /// The resume path's display seeding: the restored transcript
    /// becomes the conversation in one mutation, the view stays
    /// pinned to the newest line, and the first frame renders the
    /// full history as though it had always been there. Call before
    /// the run loop starts; a mid-session call would splice history
    /// into a live conversation.
    pub fn seed_messages(&mut self, messages: Vec<TuiMessage>) {
        // A transcript's error rows are its cancellation records: a
        // user row that follows one is a claim after an interrupted
        // run — the plan it stands over is owed work, and the claim
        // must not retire it. The live drain's `plan_interrupted`
        // flag, reconstructed from the transcript's own shape.
        let mut last_row_was_error = false;
        for message in &messages {
            let TuiMessage::Assistant { blocks, .. } = message else {
                // A steered row is not a claim: it rode into a run
                // already in flight, so the plan it lands beside kept
                // standing in the live session and keeps standing
                // here — the nudge's echo is this shape on every run
                // that lands with work left.
                if let TuiMessage::User { steered: false, .. } = message
                    && !last_row_was_error
                {
                    self.retire_finished_plan();
                }
                last_row_was_error = matches!(message, TuiMessage::Error { .. });
                continue;
            };
            last_row_was_error = false;
            // fall through: an assistant row answered what stood.
            for block in blocks {
                if let ContentBlock::Tool {
                    call_id,
                    retained_input,
                    output_preview,
                    ..
                } = block
                {
                    self.retain_tool_detail(call_id, retained_input, output_preview);
                }
                if let ContentBlock::Tool {
                    name,
                    call_id,
                    success,
                    retained_input,
                    ..
                } = block
                    && name == "TodoWrite"
                    && *success
                    && let Some(rows) = crate::todo_view::parse_submission(retained_input)
                {
                    let changes = crate::todo_view::status_changes(&rows, &self.latest_todos);
                    self.todo_changes.insert(call_id.clone(), changes.clone());
                    self.todo_section_changes = changes;
                    self.latest_todos = rows;
                    self.expanded_tools.insert(call_id.clone());
                    self.auto_expanded_todo = Some(call_id.clone());
                }
            }
        }
        self.conversation.extend(messages);
        // A transcript that ends on bare, unanswered claims is work
        // the exit left standing — the run in flight's prompt caught
        // before it could record anything, the waiting ones written
        // there by the exit itself because no turn ever spent them.
        // Each waits again in the section, in the order they were
        // typed. A claim followed by its cancellation record is a
        // deliberate stop and owes nothing: the user cancelled that
        // prompt, and resurrecting it into the queue would be a
        // phantom they never sent. A claim an assistant row answered
        // is done the same way.
        let owed: Vec<String> = self
            .conversation
            .iter()
            .rev()
            .map_while(|message| match message {
                TuiMessage::User {
                    text,
                    steered: false,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .filter(|text| !text.trim().is_empty())
            .collect();
        if !owed.is_empty() {
            let mut queue = self
                .state
                .queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for text in owed {
                queue.push(text);
            }
        }
        self.scroll_offset = 0;
        self.stick = Stick::Follow;
    }

    /// The input buffer's current text.
    ///
    /// Empty right after a submit; keystrokes and pastes edit it
    /// through the editor.
    #[must_use]
    pub fn input(&self) -> &str {
        self.input.text()
    }

    /// The scroll offset in lines above the bottom of the view.
    ///
    /// Zero means the newest line is visible.
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Whether the composer caret is in its visible blink phase.
    ///
    /// The terminal's own cursor stands in for the caret, so this is
    /// the app-side half of the blink contract: keyboard input
    /// resolidifies it, mouse traffic does not.
    #[must_use]
    pub fn caret_visible(&self) -> bool {
        self.caret_blink.on
    }

    /// Whether the view follows the newest content.
    ///
    /// True while pinned; any scroll-up detaches until the view
    /// returns near the bottom.
    #[must_use]
    pub fn auto_scroll(&self) -> bool {
        self.stick.follows()
    }

    /// Whether the run loop should exit.
    ///
    /// True after a quit key; the loop observes it between events.
    #[must_use]
    pub fn is_quitting(&self) -> bool {
        self.quit.requested
    }

    /// The shared streaming-text buffer.
    ///
    /// One allocation writers append to and the display reads; it
    /// reads empty until a writer starts.
    #[must_use]
    pub fn streaming_text(&self) -> &Arc<Mutex<String>> {
        &self.state.streaming_text
    }

    /// The shared in-flight tool list.
    ///
    /// Producers record entries per dispatched call and remove them
    /// on completion.
    #[must_use]
    pub fn active_tools(&self) -> &Arc<Mutex<Vec<ActiveTool>>> {
        &self.state.active_tools
    }

    /// The shared graduation queue, as the observer writes it.
    ///
    /// Exposed for tests that drive conversation arrivals the way the
    /// observer does — push a [`Graduation`] and render, exactly the
    /// path a live run takes.
    pub fn graduations(&self) -> &Arc<Mutex<Vec<crate::observer::Graduation>>> {
        &self.state.graduations
    }

    /// The shared token counters.
    ///
    /// Per-turn and cumulative usage as the observer records it; a
    /// host reads session accounting through this handle. The status
    /// bar renders no token total — the provider's usage figures do
    /// not state a conversation's size, so no honest number exists
    /// to show.
    #[must_use]
    pub fn tokens(&self) -> &Arc<Mutex<TokenCounts>> {
        &self.state.tokens
    }

    /// The shared redraw request.
    ///
    /// Waking it triggers a redraw on the loop's next iteration.
    #[must_use]
    pub fn render_notify(&self) -> &Arc<event_listener::Event> {
        &self.state.render_notify
    }

    /// Route submits to the agent driver.
    ///
    /// The sender half of the channel the mode driver receives on;
    /// once set, Enter queues every non-empty submit and forwards its
    /// id, and the prompt reaches the transcript when the driver
    /// claims it rather than when the key was pressed — so the row
    /// says what was sent, not what was typed. A send no receiver
    /// awaits puts the prompt back where it came from; the session is
    /// ending and nothing will claim it.
    pub fn set_submit_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<Submission>) {
        self.submit_tx = Some(tx);
    }

    /// Install the steering seam, or take it away.
    ///
    /// `None` is the bare-app state and the honest answer for a mode that
    /// cannot reach into a run: the chord stays bound and simply submits,
    /// rather than binding a key to nothing.
    pub fn set_guidance(&mut self, guidance: Option<Arc<dyn Guidance>>) {
        self.guidance = guidance;
    }

    /// Install the channel permission requests arrive on.
    ///
    /// The receiver half of the bridge the host builds around the
    /// runner's permission resolver; once set, pending asks render as
    /// an overlay and the run loop claims the channel into its select.
    pub fn set_permission_requests(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        self.permission_rx = Some(rx);
    }

    /// Land one delivered request and any others already waiting
    /// behind it.
    ///
    /// The run loop's select arm hands the request its wakeup
    /// delivered plus the receiver it owns: the request enters the
    /// queue and the receiver drains, so a burst of ready asks lands
    /// together and the first frame's overlay reports the true
    /// waiting count instead of admitting them one loop iteration at
    /// a time.
    pub fn land_permission_requests(
        &mut self,
        first: PermissionRequest,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        self.pending_permissions.push_back(first);
        while let Ok(request) = rx.try_recv() {
            self.pending_permissions.push_back(request);
        }
    }

    /// Move any waiting permission requests into the queue.
    ///
    /// The test seam for staging asks without the run loop:
    /// production keeps its receiver in a local and lands bursts
    /// through [`land_permission_requests`](Self::land_permission_requests)
    /// instead — once `run` claims the channel, this slot is empty and
    /// the method is a no-op.
    pub fn poll_permission_requests(&mut self) {
        while let Some(request) = self
            .permission_rx
            .as_mut()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.pending_permissions.push_back(request);
        }
    }

    /// Answer the front permission request, if one is pending.
    ///
    /// Sends `allow` on its reply channel and drops it from the queue;
    /// a send that finds no receiver (a resolver already cancelled
    /// away) is silently fine — the gate denied that call on its own.
    fn resolve_pending_permission(&mut self, allow: bool) {
        if let Some(front) = self.pending_permissions.pop_front()
            && front.reply.send(allow).is_err()
        {
            // The resolver stopped waiting (a cancelled dispatch); the
            // gate denied that call on its own.
            tracing::trace!("permission reply channel already closed");
        }
    }

    /// Apply a key press to a pending permission ask.
    ///
    /// `Some(redraw)` when a request is pending — the overlay owns the
    /// keyboard: plain `y` allows, plain `n`/Esc deny, and every
    /// other key is swallowed — Enter among them, since a press
    /// queued to submit the draft must never land as an approval — so
    /// nothing reaches the composer, whose control chords would
    /// otherwise submit or edit the hidden draft (Ctrl-Enter and
    /// Ctrl-M submit, Ctrl-W deletes a word).
    /// `None` only for the two `c` chords the app machinery owns —
    /// cancel/quit and copy — so those survive under a prompt.
    /// Modifier-decorated answers (Alt+y, Alt+Enter) do not answer:
    /// the documented keys are the plain ones.
    fn permission_key(&mut self, key: KeyEvent) -> Option<bool> {
        self.pending_permissions.front()?;
        if key_survives_the_sheet(key) {
            return None;
        }
        let bare = key.modifiers == KeyModifiers::NONE;
        let shift = key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Char('y' | 'Y') if bare || shift => {
                self.resolve_pending_permission(true);
                Some(true)
            }
            KeyCode::Char('n' | 'N') if bare || shift => {
                self.resolve_pending_permission(false);
                Some(true)
            }
            KeyCode::Esc if bare => {
                self.resolve_pending_permission(false);
                Some(true)
            }
            _ => Some(false),
        }
    }

    /// Install the channel question prompts arrive on.
    ///
    /// The receiver half of the bridge the host builds around the
    /// runner's question channel; once set, pending questions render
    /// as an overlay and the run loop claims the channel into its
    /// select.
    pub fn set_question_requests(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<QuestionPrompt>,
    ) {
        self.question_rx = Some(rx);
    }

    /// Land one delivered question and any others already waiting
    /// behind it.
    ///
    /// The run loop's select arm hands the prompt its wakeup
    /// delivered plus the receiver it owns: the prompt enters the
    /// queue and the receiver drains, so a multi-question batch lands
    /// together and the first frame's overlay reports the true
    /// waiting count.
    pub fn land_question_requests(
        &mut self,
        first: QuestionPrompt,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<QuestionPrompt>,
    ) {
        self.pending_questions
            .push_back(PendingQuestion::new(first));
        while let Ok(prompt) = rx.try_recv() {
            self.pending_questions
                .push_back(PendingQuestion::new(prompt));
        }
    }

    /// Refresh the queue: land what arrived, drop what nobody asks.
    ///
    /// Landing and withdrawal are one pass because they are the same
    /// question asked twice — is this prompt still live? — and the run
    /// loop does both on its own beat. The test seam for staging
    /// questions without the loop: production keeps its receiver in a
    /// local and lands batches through
    /// [`land_question_requests`](Self::land_question_requests) instead —
    /// once `run` claims the channel, this slot is empty and only the
    /// withdrawal half has anything left to say.
    pub fn poll_question_requests(&mut self) {
        while let Some(prompt) = self.question_rx.as_mut().and_then(|rx| rx.try_recv().ok()) {
            self.pending_questions
                .push_back(PendingQuestion::new(prompt));
        }
        self.retire_withdrawn_questions();
    }

    /// Answer the question on screen, wherever the walk left it.
    ///
    /// Sends `answers` on its one-shot and lifts it out of the queue —
    /// its cursor, marks, and draft going with it, so the question
    /// behind it is still clean rather than wearing the last one's
    /// answer. A send that finds no receiver (a dispatch already
    /// cancelled away) is silently fine — the asking tool read its
    /// dismissal when its future dropped.
    fn resolve_pending_question(&mut self, answers: Vec<String>) {
        let shown = self.question_view;
        if let Some(entry) = self.pending_questions.remove(shown)
            && entry.prompt.reply.send(answers).is_err()
        {
            tracing::trace!("question reply channel already closed");
        }
        // The entry behind the answered one slides into its place and is
        // shown by the same index; answering the batch's last question
        // leaves the view past the end, so it steps back onto what is
        // left.
        self.clamp_question_view();
    }

    /// What the shown question's marks and its draft add up to, as the
    /// answer Enter sends.
    ///
    /// Marked option rows answer in the order the sheet shows them and a
    /// draft answers last, from the row below every option. A single
    /// select answers one thing: the marked row — or the draft, where the
    /// draft is what the user chose to write — and with nothing marked at
    /// all, the row the cursor stands on. A multi-select asks the same
    /// question of its rows and, when none of them is marked, takes the
    /// cursor's row for its own: an empty reply is exactly what a
    /// dismissal sends, so Enter on a row the user walked onto cannot
    /// answer with nothing. Whichever row the cursor stands on is the row
    /// Enter confirms, the compose row included — there its words are its
    /// content, and a draft kept out of compose answers from no other row
    /// but that one.
    ///
    /// `draft_counts` is whether the typed text belongs to the answer on
    /// its own rather than only where its row is marked: the draft's own
    /// Enter says yes, because writing a reply *is* choosing it, while
    /// Enter over the rows asks the mark. Both paths come here, so a
    /// marked draft answers the same however it was reached and composing
    /// never silently drops the rows marked beside it.
    fn shown_answer(&self, draft_counts: bool) -> Vec<String> {
        let Some(entry) = self.shown_question() else {
            return Vec::new();
        };
        let ui = &entry.ui;
        let free_row = entry.prompt.options.len();
        let draft = ui.draft.trim().to_string();
        let draft_wanted = !draft.is_empty()
            && (draft_counts || ui.toggled.contains(&free_row) || ui.cursor == free_row);
        if entry.prompt.multi_select {
            let mut answers = entry
                .prompt
                .options
                .iter()
                .enumerate()
                .filter(|(index, _)| *index < free_row && ui.toggled.contains(index))
                .map(|(_, option)| option.label.clone())
                .collect::<Vec<String>>();
            if draft_wanted {
                answers.push(draft);
            }
            if answers.is_empty() {
                answers.extend(
                    entry
                        .prompt
                        .options
                        .get(ui.cursor)
                        .map(|option| option.label.clone()),
                );
            }
            answers
        } else if draft_wanted {
            vec![draft]
        } else {
            ui.toggled
                .iter()
                .copied()
                .find(|index| *index < free_row)
                .and_then(|index| entry.prompt.options.get(index))
                .or_else(|| entry.prompt.options.get(ui.cursor))
                .map(|option| vec![option.label.clone()])
                .unwrap_or_default()
        }
    }

    /// Take off the queue every question the asking side has stopped
    /// waiting for.
    ///
    /// A prompt's one-shot is the asker's stake in its question: the
    /// receiver lives exactly as long as the wait does, and ends with it.
    /// A batch's wait ends early on its first dismissal — one question
    /// coming back unanswered is the whole ask's answer to the model —
    /// and the questions behind it drop their receivers unheard, as they
    /// do when a dispatch is cancelled or the asking channel closes
    /// mid-wait. Nothing else says so: without this the user keeps
    /// looking at a question nobody will ever read, over the reply that
    /// question never waited for. A closed reply channel is the whole
    /// notice, and this is where it is read.
    ///
    /// This is the running half of a question's retirement: the tick
    /// already drops the whole queue when the run itself is gone, while
    /// this catches the single ask abandoned *inside* a live run — the
    /// model goes on asking and answering other things, and only these
    /// few questions stop being anyone's.
    ///
    /// The view follows the questions rather than their indices: it steps
    /// back by the ones withdrawn ahead of it, so the question the walk
    /// arrived at stays on screen for as long as it survives.
    fn retire_withdrawn_questions(&mut self) -> bool {
        let view = self.question_view;
        let withdrawn_ahead = self
            .pending_questions
            .iter()
            .take(view)
            .filter(|entry| entry.prompt.reply.is_closed())
            .count();
        let before = self.pending_questions.len();
        self.pending_questions
            .retain(|entry| !entry.prompt.reply.is_closed());
        if before == self.pending_questions.len() {
            return false;
        }
        self.question_view = view.saturating_sub(withdrawn_ahead);
        self.clamp_question_view();
        true
    }

    /// The question on screen: the one the sheet renders and the keys
    /// answer.
    ///
    /// Read through the view index rather than the queue's front, which
    /// is what lets a batch hold its arrival order — the count the title
    /// prints — while the user walks it: the third question of four is
    /// what this returns after two presses of Right, not the one that
    /// arrived third. `None` is a closed sheet, an empty queue, which
    /// every question path faces the moment the last answer is sent or
    /// the last prompt withdrawn.
    fn shown_question(&self) -> Option<&PendingQuestion> {
        self.pending_questions.get(self.question_view)
    }

    /// The question on screen, for the keys that move its cursor, mark a
    /// row, or compose a draft on it.
    ///
    /// The same index the render reads, so a key can only ever reach the
    /// question the user is looking at, and walking the queue leaves every
    /// other answer-in-progress exactly as it was. `None` means no sheet
    /// is open: a key that arrives in that moment has nothing to act on
    /// and does nothing, rather than borrowing the next question to
    /// arrive.
    fn shown_question_mut(&mut self) -> Option<&mut PendingQuestion> {
        self.pending_questions.get_mut(self.question_view)
    }

    /// Walk the shown question one step, wrapping at both ends.
    ///
    /// The walk moves the view and leaves the queue's arrival order
    /// alone, because that order is what the sheet counts: walking to the
    /// third question of four says `3 of 4`, and says it again after a
    /// walk away and back. It is a circle in both directions, so the
    /// question answered last sits one press from the one answered first.
    ///
    /// A batch of one has nowhere to go, and the step declines rather
    /// than reports: the key that asked for it is still swallowed, which
    /// keeps an arrow from falling through to the transcript behind the
    /// sheet.
    fn step_question_view(&mut self, forward: bool) {
        let count = self.pending_questions.len();
        if count < 2 {
            return;
        }
        self.question_view = if forward {
            // Past the batch's last question comes its first: the walk is
            // a circle, so the question answered last is one press from
            // the question answered first.
            let next = self.question_view.saturating_add(1);
            if next >= count { 0 } else { next }
        } else if self.question_view == 0 {
            count.saturating_sub(1)
        } else {
            self.question_view.saturating_sub(1)
        };
    }

    /// Keep [`question_view`](Self::question_view) a live index into the
    /// queue.
    ///
    /// Answering or withdrawing a question can shrink the queue under a
    /// user standing at its end, and an index past the last row is a
    /// sheet showing nothing rather than a sheet showing the next
    /// question. So the view is pulled back to the last question left; an
    /// emptied queue takes it home to zero, which is also where the next
    /// batch is met.
    fn clamp_question_view(&mut self) {
        self.question_view = self
            .question_view
            .min(self.pending_questions.len().saturating_sub(1));
    }

    /// Apply a key press to the question on screen.
    ///
    /// `Some(redraw)` when a question is pending — the overlay owns the
    /// keyboard, and which half of it answers depends on whether the
    /// draft is being edited. Over the rows, Up and Down move the cursor,
    /// Left and Right walk the queued questions (the walk moves the view
    /// and not the queue, so the question on screen is the one Enter
    /// answers and Esc dismisses the one question the walk left it on —
    /// never the batch behind it), Space marks
    /// the row it stands on — the compose row included — without ever
    /// answering, and Enter confirms: the marked rows, or the cursor row
    /// where nothing is marked. On the compose row Enter opens the draft,
    /// and a marked compose row still empty opens it too rather than
    /// answering without its text — as does typing a letter anywhere in
    /// the
    /// sheet, which opens the draft with that letter in it. Inside the
    /// draft the arrows move its caret and Home and
    /// End jump its ends — those keys belong to the text being typed, not
    /// to the queue — printable keys build it, Enter sends it, and Esc
    /// steps back to the rows with the text kept. Every other key is
    /// swallowed; `None` for the two `c` chords and the keys
    /// [`key_survives_the_sheet`] names — the machinery's own chords, and
    /// the paging and verbosity that read the conversation the sheet is
    /// parked over.
    fn question_key(&mut self, key: KeyEvent) -> Option<bool> {
        self.shown_question()?;
        if key_survives_the_sheet(key) {
            return None;
        }
        let bare = key.modifiers == KeyModifiers::NONE;
        let redraw = if self
            .shown_question()
            .is_some_and(|entry| entry.ui.composing)
        {
            self.question_draft_key(key, bare)
        } else {
            self.question_rows_key(key, bare)
        };
        Some(redraw)
    }

    /// The keys the compose draft answers, on the question on screen.
    ///
    /// The draft owns these while it is open: printable characters land
    /// at its caret, the arrows and Home/End move that caret, Enter sends
    /// the text as the answer the options never offered, and Esc steps
    /// back to the rows keeping the text. Editing follows the composer's
    /// line-editor conventions wherever the surface can honour them:
    /// `Delete` erases to the right, Ctrl with an arrow walks a whole
    /// word, and Ctrl+W and Ctrl+U erase a word and the line's head.
    /// Enter means send whatever modifier it arrives under — the draft
    /// holds one line, so a terminal that spells `Shift+Enter` as its own
    /// event is not asking for a newline the answer cannot carry. A draft
    /// with nothing in it is not an answer and does not go; it says so on
    /// the sheet's own hint row, which is where the key promised
    /// otherwise. A letter counts whether it is shifted, because that is
    /// how a capital arrives; only Ctrl takes a key off its letter.
    /// Everything else is swallowed.
    fn question_draft_key(&mut self, key: KeyEvent, bare: bool) -> bool {
        let mut send = false;
        if let Some(entry) = self.shown_question_mut() {
            let ui = &mut entry.ui;
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let letter = bare || key.modifiers == KeyModifiers::SHIFT;
            match key.code {
                KeyCode::Enter => {
                    // The written reply is the answer, so it counts here
                    // whether or not its row carries a mark. Whitespace
                    // alone is not a reply and does not go, and the hint
                    // row has already changed its wording to say what is
                    // missing — a refusal the user reads, not a key
                    // dropped without a word.
                    send = !ui.draft.trim().is_empty();
                }
                KeyCode::Esc => ui.composing = false,
                KeyCode::Left if ctrl => ui.caret_word_left(),
                KeyCode::Left if bare => ui.caret_left(),
                KeyCode::Right if ctrl => ui.caret_word_right(),
                KeyCode::Right if bare => ui.caret_right(),
                KeyCode::Home if bare => ui.caret_home(),
                KeyCode::End if bare => ui.caret_end(),
                KeyCode::Backspace | KeyCode::Char('w') if ctrl => ui.delete_word_left(),
                KeyCode::Backspace => ui.backspace(),
                KeyCode::Delete => ui.delete_forward(),
                KeyCode::Char('u') if ctrl => ui.delete_to_start(),
                KeyCode::Char(ch) if letter => ui.type_char(ch),
                _ => {}
            }
        }
        // The borrow above ends with the match; an answer leaves through
        // the queue's own removal, which takes the drafted question — and
        // its draft — out with it.
        if send {
            let answers = self.shown_answer(true);
            self.resolve_pending_question(answers);
        }
        true
    }

    /// Land a paste on the question on screen: into the open draft at its
    /// caret, or into a draft opened for it.
    ///
    /// A paste is a burst of keystrokes, and the overlay answers
    /// keystrokes — a letter typed at the rows opens the draft with that
    /// letter in it, so a paste arrives the same way rather than vanishing
    /// without a word. Dropping it would lose the likeliest thing a user
    /// has to say that the options never offered: a path, a hash, the line
    /// copied out of the conversation above the box.
    ///
    /// The text lands as one line whatever it held. A draft answers one
    /// question in one row, so the breaks a multi-line paste carries
    /// become the spaces between its parts; nothing is discarded.
    fn paste_into_the_question(&mut self, text: &str) -> bool {
        let Some(entry) = self.shown_question_mut() else {
            return true;
        };
        if !entry.ui.composing {
            entry.ui.open_composing();
        }
        entry.ui.insert_str(&paste_for_one_line(text));
        true
    }

    /// Whether Enter here means go and write a reply rather than answer.
    ///
    /// An empty compose row cannot be sent, so Enter opens the draft to
    /// fill it — whenever the row is the answer the user is pointing at
    /// (`on_compose_row`, where an unmarked compose row is exactly that
    /// invitation) or when it is the only mark on the sheet. Marks on real
    /// options beside it settle the matter the other way: the answer is
    /// chosen, and the empty row contributes nothing to it.
    fn needs_the_draft(&self, on_compose_row: bool) -> bool {
        self.shown_question().is_some_and(|entry| {
            let free_row = entry.prompt.options.len();
            let marks = entry.ui.toggled.len();
            let options_marked = entry
                .ui
                .toggled
                .iter()
                .filter(|index| **index < free_row)
                .count();
            entry.ui.draft.trim().is_empty()
                && if on_compose_row || entry.ui.toggled.contains(&free_row) {
                    options_marked == 0 && (on_compose_row || marks == 1)
                } else {
                    false
                }
        })
    }

    /// The keys the option rows answer, on the question on screen.
    ///
    /// The rows' half of the overlay: navigation, marking, walking the
    /// queue, confirming, dismissing. Marking is Space's whole job, on the
    /// compose row as on an option — it adds or removes the row it stands
    /// on for a multi-select, and moves the one mark there for a
    /// single-select — and it never answers, so a stray press cannot send
    /// a reply. Enter answers with the marked rows (the marked row, or the
    /// cursor row where nothing is marked), a marked draft answering with
    /// its own text, and the compose row answering with the words it
    /// holds. A letter typed here starts a draft, shifted or not, since a
    /// capital is the letter with Shift held.
    fn question_rows_key(&mut self, key: KeyEvent, bare: bool) -> bool {
        let Some(entry) = self.shown_question() else {
            return true;
        };
        let free_row = entry.prompt.options.len();
        let multi = entry.prompt.multi_select;
        let cursor = entry.ui.cursor;
        match key.code {
            KeyCode::Up if bare => {
                if let Some(entry) = self.shown_question_mut() {
                    entry.ui.cursor = entry.ui.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down if bare => {
                if let Some(entry) = self.shown_question_mut() {
                    entry.ui.cursor = (entry.ui.cursor.saturating_add(1)).min(free_row);
                }
            }
            // The walk moves the view and leaves the queue's arrival
            // order alone — that order is what the sheet counts, so
            // walking to the third question of four can say `3 of 4`, and
            // say it again after a walk away and back. Nothing resets on
            // the way: each entry carries its own cursor, marks, and
            // draft, so the question walked to still holds what was left
            // there and the one walked away from keeps it for the return.
            KeyCode::Right if bare => self.step_question_view(true),
            KeyCode::Left if bare => self.step_question_view(false),
            // Space's whole job is the dot, and the compose row is a row
            // like the others: marking it is the user saying the typed
            // reply is the answer. Opening the draft used to be Space's
            // trick on that one row, which made a single key mean two
            // things by where the cursor happened to stand; Enter is what
            // opens the draft, as it is everywhere it means onward.
            KeyCode::Char(' ') if bare => {
                if let Some(entry) = self.shown_question_mut() {
                    let ui = &mut entry.ui;
                    let marked = ui.toggled.contains(&cursor);
                    if multi {
                        if marked {
                            ui.toggled.remove(&cursor);
                        } else {
                            ui.toggled.insert(cursor);
                        }
                    } else {
                        // One mark, moved: the row Space stands on takes
                        // it and every other row loses it, and Space again
                        // on the marked row clears it.
                        ui.toggled.clear();
                        if !marked {
                            ui.toggled.insert(cursor);
                        }
                    }
                }
            }
            KeyCode::Enter if bare => {
                // An empty compose row has no reply to send, so Enter goes
                // and writes one — but only where that empty row is the
                // whole answer the user asked for. Beside marks on real
                // options, the marks are the answer already chosen, and a
                // row with nothing to say does not veto them: it simply
                // adds nothing, on the cursor's row as anywhere.
                if self.needs_the_draft(cursor == free_row) {
                    if let Some(entry) = self.shown_question_mut() {
                        entry.ui.open_composing();
                    }
                    return true;
                }
                let answers = self.shown_answer(false);
                self.resolve_pending_question(answers);
            }
            KeyCode::Esc if bare => {
                self.resolve_pending_question(Vec::new());
            }
            // A letter typed at the rows is a reply being started, and it
            // belongs to the draft rather than vanishing on the way to
            // nowhere. Space marks and Enter opens, but the fastest road
            // to a typed answer is to start typing it, and a keystroke
            // the overlay drops in silence is the one thing a keyboard
            // should never do.
            KeyCode::Char(ch) if bare || key.modifiers == KeyModifiers::SHIFT => {
                if let Some(entry) = self.shown_question_mut() {
                    entry.ui.open_composing();
                    entry.ui.type_char(ch);
                }
            }
            _ => return false,
        }
        true
    }

    /// Claim the pending full repaint, if one is due.
    ///
    /// One-shot: the caller (the run loop) owns acting on the claim —
    /// clearing the terminal so the next frame repaints every cell
    /// instead of diffing against a buffer the visible screen never
    /// matched.
    #[must_use]
    pub fn take_full_repaint(&mut self) -> bool {
        self.full_repaint_due.take().is_some()
    }

    /// Replace the selection copier.
    ///
    /// The instrumentation seam for the selection pins: a recorder
    /// observes what a release copies without touching any real
    /// clipboard, answering [`CopyAnswer::Verified`] so the notice
    /// reads as delivered. Production never calls this.
    pub fn set_selection_copier(&mut self, copier: SelectionCopier) {
        self.copier = copier;
    }

    /// Switch the display verbosity.
    ///
    /// Invalidates the conversation and live caches so every
    /// completed tool body re-renders in the new mode on the next
    /// frame — the shapes differ across modes, so stale lines must
    /// not linger until the next real event.
    pub fn set_verbosity(&mut self, verbosity: Verbosity) {
        self.verbosity = verbosity;
        self.conversation_generation = self.conversation_generation.saturating_add(1);
        self.rebake_whole();
        self.live_stamp = 0;
    }

    /// Advance to the next verbosity mode, wrapping around.
    ///
    /// F2's handler: one press moves one step along Quiet → Normal
    /// → Verbose → Quiet, each press taking effect on the next
    /// frame through [`set_verbosity`](Self::set_verbosity)'s cache
    /// invalidation.
    fn cycle_verbosity(&mut self) {
        let next = match self.verbosity {
            Verbosity::Quiet => Verbosity::Normal,
            Verbosity::Normal => Verbosity::Verbose,
            Verbosity::Verbose => Verbosity::Quiet,
        };
        self.set_verbosity(next);
    }

    /// Install the callback fired when a turn ends.
    ///
    /// The hook receives the conversation snapshot at the moment a
    /// completed reply or a surfaced failure joins it — the natural
    /// save point for a session transcript. It runs inline on the
    /// render task, so heavy work must move itself off-thread.
    pub fn set_turn_end_hook(&mut self, hook: TurnEndHook) {
        self.turn_end_hook = Some(hook);
    }

    /// Run the UI event loop until the user exits.
    ///
    /// Selects over terminal events, the shared notify (the path
    /// background state uses to request a redraw), and a periodic
    /// tick. A notify listener stays registered across draws and
    /// event handling, so a notification sent while the loop is busy
    /// is delivered on the next iteration rather than lost.
    /// Background redraws are frame-capped: a notify renders at most
    /// once per frame interval, a request arriving sooner parks
    /// itself for the tick to claim, and any queued graduation
    /// forces the frame. Input-driven redraws are not capped. The
    /// tick redraws while tool calls are in flight (keeping their
    /// elapsed stamps live), claims parked background requests, and
    /// otherwise draws nothing. The caller owns terminal setup and
    /// teardown.
    ///
    /// # Errors
    /// Propagates terminal draw failures; the caller's guard still
    /// restores the terminal.
    pub async fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.quit = QuitState::default();
        let mut events = TerminalEvents::spawn();
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        let mut animation = tokio::time::interval(ANIMATION_INTERVAL);
        animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let notify = Arc::clone(&self.state.render_notify);
        let mut listener = notify.listen();
        let mut permission_rx = self.permission_rx.take();
        let mut permission_closed = false;
        let mut question_rx = self.question_rx.take();
        let mut question_closed = false;

        terminal.draw(|frame| self.render(frame))?;

        while !self.quit.requested {
            let mut needs_redraw = tokio::select! {
                maybe_event = events.recv() => {
                    match maybe_event {
                        Some(result) => match result {
                            Ok(event) => {
                                let mut handled = self.handle_event(&event);
                                handled |= self.drain_ready(|| events.poll())?;
                                handled
                            }
                            Err(err) => return Err(err.into()),
                        },
                        None => return Ok(()),
                    }
                }
                () = &mut listener => self.notify_wake(&notify, &mut listener),
                maybe_question = async {
                    match question_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(prompt) = maybe_question {
                        match question_rx.as_mut() {
                            Some(rx) => self.land_question_requests(prompt, rx),
                            None => {
                                self.pending_questions
                                    .push_back(PendingQuestion::new(prompt));
                            }
                        }
                        true
                    } else {
                        question_closed = true;
                        false
                    }
                }
                _instant = tick.tick() => self.tick_wake(Instant::now()),
                // The animation heartbeat: unconditional. The tick
                // fires every 20ms whether or not anything else is
                // happening — streaming, typing, a stopwatch ticking —
                // and each beat draws whenever something animates.
                // The gate decides only whether THIS beat draws, so
                // an idle-but-running session re-renders the spinner
                // at the same cadence a running tool's stopwatch
                // enjoys, never waiting on another lane's traffic.
                _anim = animation.tick() => {
                    let animating = self.pulse_animation_living();
                    if animating {
                        self.frame_stats.anim_wakes =
                            self.frame_stats.anim_wakes.saturating_add(1);
                    }
                    animating
                }
                maybe_request = async {
                    match permission_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(request) = maybe_request {
                        match permission_rx.as_mut() {
                            Some(rx) => self.land_permission_requests(request, rx),
                            None => self.pending_permissions.push_back(request),
                        }
                        true
                    } else {
                        permission_closed = true;
                        false
                    }
                }
            };
            if permission_closed {
                permission_rx = None;
                permission_closed = false;
            }
            if question_closed {
                question_rx = None;
                question_closed = false;
            }
            if self.retire_withdrawn_questions() {
                needs_redraw = true;
            }
            if self.take_full_repaint() {
                if let Err(err) = terminal.clear() {
                    tracing::warn!("full repaint clear failed: {err}");
                }
                needs_redraw = true;
            }
            if needs_redraw {
                let started = Instant::now();
                terminal.draw(|frame| self.render(frame))?;
                let took = started.elapsed();
                if let Some(lane) = self.frame_stats.note_draw(took, Instant::now()) {
                    tracing::debug!(
                        target: "dch_tui::frames",
                        anim_wakes = lane.anim_wakes,
                        draws = lane.draws,
                        worst_draw_ms = lane.worst_draw_ms,
                        "frame lane"
                    );
                }
            }
        }
        Ok(())
    }

    /// Whether the run loop's animation wake should draw this beat.
    ///
    /// The lane draws while anything on screen animates. A running
    /// turn's elapsed row — the spinner and its counting figure —
    /// ticks from the claim that starts the run to the event that
    /// lands it, thinking phases included. A tool call's stopwatch
    /// and spinner tick while the call is in flight. The activity
    /// strip rides the same beats without ever gating them: its off
    /// style paints nothing, and the elapsed row still needs its
    /// frames. A fully idle session pays for none of it.
    #[must_use]
    pub fn pulse_animation_living(&self) -> bool {
        self.any_tools_running()
            || self
                .state
                .agent_running
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Render the pinned pulse strip into its row.
    ///
    /// While the agent runs, the strip's configured row becomes the
    /// activity strip: the bar, full width, in the theme's
    /// composer-border color (the primary edge on themes without one
    /// — the same color the prompt box's left border paints). The row
    /// is the spacer directly above the composer by default, or the
    /// terminal's dedicated first row when the config says so; the
    /// glow style holds a static full-width bar whose color breathes,
    /// and the off style yields the row back, nothing drawn. The bar
    /// anchors on the run's first frame and never re-anchors: streams
    /// beginning and graduating inside the run leave its phase
    /// untouched, so one continuous animation runs from the frame the
    /// run starts to the frame it ends. Returns whether the strip
    /// claimed the row, so the composer hairline can stand down for
    /// the frame when the row was its spacer.
    fn render_pulse_strip(&self, frame: &mut Frame, area: Rect) -> bool {
        if matches!(self.strip_style, StripStyle::Off) {
            return false;
        }
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::Relaxed)
            || area.height == 0
        {
            return false;
        }
        let now = Instant::now();
        let base = self
            .theme
            .ui
            .composer_border
            .unwrap_or(self.theme.ui.primary);
        let anchor = self.run_anchor.unwrap_or(now);
        let color = if matches!(self.strip_style, StripStyle::Glow) {
            scaled_color(base, glow_eighths(anchor, now))
        } else {
            base
        };
        let pulse = pulse_span(
            self.strip_style,
            anchor,
            now,
            usize::from(area.width),
            color,
        );
        frame.render_widget(pulse, area);
        true
    }

    /// Copy the live selection, if any.
    ///
    /// The terminal-standard copy chord. A span that covered no
    /// characters leaves the clipboard alone — the same discipline
    /// the release and the keyboard walk apply. A delivered copy
    /// lands its confirmation on the notice row, like every path.
    fn copy_selection_now(&mut self) {
        let covered = self
            .selection
            .as_ref()
            .map(|selection| self.selection_text(selection));
        if let Some((text, covered_any)) = covered
            && covered_any
        {
            self.copy_selection_and_notice(&text);
        }
    }

    /// Hand a completed selection's text to the clipboard and say so.
    ///
    /// The funnel every copy path — the mouse release, the copy chord,
    /// and the keyboard walk's re-copy — delivers through, so each one
    /// posts from the same vocabulary on the notice row above the
    /// composer. Every copy takes its attempt id here and records it
    /// as the wording that holds the row; the immediate post answers
    /// with what the copier knows right now, and when the production
    /// chain has transports in flight, the frame drain replaces it
    /// with the same attempt's verdict — never with one older than
    /// what a newer copy already posted.
    fn copy_selection_and_notice(&mut self, text: &str) {
        let attempt = next_attempt(&self.copy_attempts);
        let answer = (self.copier)(attempt, text);
        self.posted_copy_attempt = attempt;
        let wording = copy_answer_notice(answer);
        if !wording.is_empty() {
            self.post_notice(wording.to_string());
        }
    }

    /// Copy the session id when the click landed inside its segment.
    ///
    /// Reads the span recorded by the last status-bar render; a click
    /// inside it routes the id through the same transport and notice
    /// path a text selection uses. Returns whether the click consumed
    /// itself here, so the caller can treat a bar click as handled.
    fn copy_session_id_at(&mut self, column: u16, row: u16) -> bool {
        let Some((span_row, start, end)) = self.session_id_span.get() else {
            return false;
        };
        if row != span_row || column < start || column >= end {
            return false;
        }
        if let Some(id) = self.session_id.clone() {
            self.copy_selection_and_notice(&id);
        }
        true
    }

    /// Handle the events already waiting behind the first one.
    ///
    /// A touchpad's wheel momentum and a fast typist both deliver
    /// bursts far faster than a frame; handling one event per loop
    /// iteration makes each wait its own full render, so a
    /// direction reversal queues behind the backlog and the display
    /// keeps scrolling the old way. Draining the ready events first
    /// applies the whole burst to the state — the offsets simply
    /// accumulate — and the caller draws once afterwards. The drain
    /// stops at a quit event, so nothing queued behind the user's
    /// confirming Ctrl+C — an Enter, a send, an echo — is applied after the
    /// decision to leave. A read failure from the source propagates
    /// out instead of masquerading as a clean exit.
    ///
    /// Returns whether anything in the burst requires a redraw.
    ///
    /// # Errors
    ///
    /// Propagates a terminal read failure delivered by the source;
    /// the caller surfaces it as a session error rather than
    /// exiting silently.
    pub fn drain_ready<F>(&mut self, mut next: F) -> Result<bool, std::io::Error>
    where
        F: FnMut() -> Option<Result<Event, std::io::Error>>,
    {
        let mut needs_redraw = false;
        while !self.quit.requested
            && let Some(result) = next()
        {
            let event = result?;
            needs_redraw |= self.handle_event(&event);
        }
        Ok(needs_redraw)
    }

    /// Apply one terminal event to the app state.
    ///
    /// Returns whether the event requires a redraw. Key releases and
    /// repeats are ignored so a held key fires once per press. While a
    /// permission ask is pending, the overlay owns the keyboard —
    /// keystrokes and pastes alike.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) => {
                self.caret_blink.resolidify();
                if key.kind != KeyEventKind::Press {
                    return false;
                }
                self.handle_key(*key)
            }
            Event::Mouse(mouse) => self.handle_mouse(*mouse),
            Event::Paste(text) => {
                if self.pending_permissions.front().is_some() {
                    return true;
                }
                self.caret_blink.resolidify();
                if !self.pending_questions.is_empty() {
                    return self.paste_into_the_question(text);
                }
                if self.queue_mode == QueueMode::Focused {
                    return true;
                }
                self.input.insert_str(&normalize_pasted_newlines(text));
                true
            }
            Event::Resize(_, _) => true,
            Event::FocusGained => {
                self.full_repaint_due = Some(Instant::now());
                true
            }
            Event::FocusLost => false,
        }
    }

    /// Apply one key press: the quit chord, copy chord, and
    /// page-scroll keys stay app-level, everything else belongs to the
    /// input editor — unless a permission ask is pending, in which
    /// case the overlay owns the keyboard first (`y` allows,
    /// `n`/Esc deny, the rest swallowed) and only the two `c` chords and
    /// the keys [`key_survives_the_sheet`] names pass through to the
    /// machinery below.
    ///
    /// Under a running agent, Ctrl+C Ctrl+C is one action in the
    /// Esc-Esc shape: with an empty composer, the first press only
    /// arms — nothing stops, nothing leaves — and the second press
    /// fires the cancel exactly as Esc-Esc fires it (the same
    /// canceller, the same record) with the program's termination
    /// riding the same press. Text in the composer clears first,
    /// run or not — clearing is not consenting, and the fire needs
    /// its presses behind an empty buffer like everywhere else. A
    /// stray key between the presses stands the whole action down.
    /// At rest the chord is the exit alone: a non-empty buffer clears
    /// first, arming nothing; on an empty one the first press arms
    /// and the second quits; any other key disarms, so the chord
    /// never fires from stale intent. Each rung is named by a notice,
    /// so the ladder reads from the frame instead of from memory.
    /// Ctrl+Shift+C copies the live selection where the terminal
    /// reports the shift; on terminals that collapse it to a plain
    /// Ctrl+C it simply joins the chord's clearing behavior. Up,
    /// Down, Home, and End fall back to the transcript while the
    /// input sits empty — Home jumps to the oldest line, End snaps
    /// to the newest, and the arrows scroll by one once nothing is
    /// recallable. The arrows join the editor as soon as anything
    /// is typed or recallable; Home and End follow the input's
    /// emptiness alone, so with an empty draft they keep scrolling
    /// the transcript even while history is recallable. Shift and
    /// an arrow, with a transcript selection up, moves the
    /// selection's head instead: the editor's select-by-keyboard.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        ) {
            self.quit.disarm();
        }
        if !matches!(
            (key.code, key.modifiers),
            (KeyCode::Esc, KeyModifiers::NONE)
        ) {
            self.cancel_arm = false;
        }
        if let Some(resolved) = self.permission_key(key) {
            return resolved;
        }
        if let Some(resolved) = self.question_key(key) {
            return resolved;
        }
        if let Some(handled) = self.queue_key(key) {
            return handled;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => {
                if self
                    .state
                    .agent_running
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    if self.cancel_arm {
                        self.cancel_arm = false;
                        if let Some(cancel) = &self.run_canceller {
                            cancel();
                        }
                        self.post_notice("Agent cancelled".to_string());
                    } else {
                        self.cancel_arm = true;
                        self.post_notice("press Esc again to cancel the run".to_string());
                    }
                    true
                } else {
                    false
                }
            }
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => self.steer_now(),
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                self.plan_view = match self.plan_view {
                    PlanView::Capped => PlanView::Full,
                    PlanView::Full => PlanView::Hidden,
                    PlanView::Hidden => PlanView::Capped,
                };
                true
            }
            (KeyCode::Char('c'), mods) if mods == KeyModifiers::CONTROL | KeyModifiers::SHIFT => {
                self.copy_selection_now();
                true
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if !self.input.is_empty() {
                    self.input.clear();
                    if self
                        .state
                        .agent_running
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        self.post_notice(
                            "draft cleared — press ctrl+c twice to cancel and exit".to_string(),
                        );
                    } else {
                        self.post_notice("draft cleared — press ctrl+c twice to quit".to_string());
                    }
                } else if self
                    .state
                    .agent_running
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    if self.quit.armed {
                        self.shutdown_flag.store(true, Ordering::SeqCst);
                        if let Some(cancel) = &self.run_canceller {
                            cancel();
                        }
                        self.post_notice("Agent cancelled".to_string());
                        self.quit.requested = true;
                    } else {
                        self.post_notice("press ctrl+c again to cancel and exit".to_string());
                        self.quit.arm();
                    }
                } else if self.quit.armed {
                    self.shutdown_flag.store(true, Ordering::SeqCst);
                    self.quit.requested = true;
                } else {
                    self.quit.arm();
                }
                true
            }
            (KeyCode::F(2), _) => {
                self.cycle_verbosity();
                true
            }
            (KeyCode::PageUp, _) => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                self.stick = Stick::Hold;
                true
            }
            (KeyCode::PageDown, _) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                self.rearm_if_near_bottom();
                true
            }
            (KeyCode::Up, KeyModifiers::NONE)
                if self.input.is_empty() && !self.input.has_history() =>
            {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.stick = Stick::Hold;
                true
            }
            (KeyCode::Down, KeyModifiers::NONE)
                if self.input.is_empty() && !self.input.has_history() =>
            {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                self.rearm_if_near_bottom();
                true
            }
            (KeyCode::Home, _) if self.input.is_empty() => {
                self.scroll_to_top();
                true
            }
            (KeyCode::End, _) if self.input.is_empty() => {
                self.scroll_to_bottom();
                true
            }
            (KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right, KeyModifiers::SHIFT)
                if self.selection.is_some() =>
            {
                self.nudge_selection(key.code);
                true
            }
            _ => {
                let action = self.input.handle_key(key, self.input_wrap_width);
                self.apply_input_action(action)
            }
        }
    }

    /// Apply one mouse event.
    ///
    /// The wheel scrolls by [`WHEEL_SCROLL_LINES`]; a press-drag-
    /// release over the conversation pane selects rendered
    /// characters — inside the app — and the release hands the
    /// covered text to the clipboard, confirmed on the notice row,
    /// while the highlight stays up until the next press. A drag
    /// pushed against the pane's
    /// top or bottom row scrolls the view in the drag's direction,
    /// extending the selection — one line per movement report, and
    /// one per tick while the pointer stays parked there; a press
    /// over the composer pane places its caret where it landed.
    /// Anything else is ignored. While a permission ask is pending,
    /// the overlay owns the pointer as it owns the keyboard: every
    /// report is swallowed and any parked drag is dropped. A question
    /// sheet holds the presses for the same reason but not the wheel —
    /// reading the conversation above the box is how a question gets
    /// answered, and the wheel is how that reading happens.
    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.pending_permissions.front().is_some() {
            self.drag_position = None;
            return true;
        }
        if !self.pending_questions.is_empty() {
            return match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_the_wheel(true),
                MouseEventKind::ScrollDown => self.scroll_the_wheel(false),
                _ => {
                    self.drag_position = None;
                    true
                }
            };
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_the_wheel(true),
            MouseEventKind::ScrollDown => self.scroll_the_wheel(false),
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.quit.disarm();
                if let Some(cell) = self.cell_at(mouse.column, mouse.row) {
                    self.selection = Some(CellSelection {
                        anchor: cell,
                        head: cell,
                    });
                    self.drag_position = Some((mouse.column, mouse.row));
                    true
                } else {
                    let retired = self.selection.take().is_some();
                    self.drag_position = None;
                    self.place_input_caret(mouse.column, mouse.row) || retired
                }
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if self.selection.is_none() {
                    return false;
                }
                self.drag_position = Some((mouse.column, mouse.row));
                let mut handled = false;
                if let Some(cell) = self.cell_at(mouse.column, mouse.row)
                    && let Some(selection) = self.selection.as_mut()
                {
                    selection.head = cell;
                    handled = true;
                }
                self.drag_autoscroll() || handled
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.drag_position = None;
                if let Some(selection) = self.selection.take() {
                    if selection.anchor == selection.head {
                        self.toggle_thinking_at(mouse.column, mouse.row);
                        self.toggle_compaction_at(mouse.column, mouse.row);
                        self.toggle_tool_at(mouse.column, mouse.row);
                        self.copy_session_id_at(mouse.column, mouse.row);
                    } else {
                        let (text, covered_any) = self.selection_text(&selection);
                        if covered_any {
                            self.copy_selection_and_notice(&text);
                        }
                        self.selection = Some(selection);
                    }
                    true
                } else {
                    let toggled_thinking = self.toggle_thinking_at(mouse.column, mouse.row);
                    let toggled_compaction = self.toggle_compaction_at(mouse.column, mouse.row);
                    let toggled_tool = self.toggle_tool_at(mouse.column, mouse.row);
                    let copied_id = self.copy_session_id_at(mouse.column, mouse.row);
                    toggled_thinking || toggled_compaction || toggled_tool || copied_id
                }
            }
            MouseEventKind::Moved => {
                self.drag_position = None;
                false
            }
            _ => false,
        }
    }

    /// One wheel notch: up pages into the conversation, down toward the
    /// newest line.
    ///
    /// The plain transcript and both overlays take their scrolling from
    /// here, so a notch means the same distance and rearms auto-scroll on
    /// the same terms wherever it arrives.
    fn scroll_the_wheel(&mut self, up: bool) -> bool {
        if up {
            self.scroll_offset = self.scroll_offset.saturating_add(WHEEL_SCROLL_LINES);
            self.stick = Stick::Hold;
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(WHEEL_SCROLL_LINES);
            self.rearm_if_near_bottom();
        }
        true
    }

    /// Scroll one step while a drag sits at the pane's top or bottom
    /// row, extending the selection head to the edge's line after
    /// the step.
    ///
    /// Editors scroll the document under a selection dragged past
    /// the viewport, in the drag's direction. A terminal reports a
    /// drag only while the pointer moves, so a pointer parked at the
    /// edge goes quiet — the drag arm runs this for an immediate
    /// step per movement and the periodic tick keeps running it once
    /// per beat while the push continues. The scroll stops at the
    /// document's first or last line whatever the drag state says,
    /// and the head clamps into the selectable document, so an edge
    /// push never extends the highlight onto the tool rows. The
    /// state itself ends on the release or, when that
    /// arrived outside the terminal's view, on the first buttonless
    /// motion — so a lost release cannot carry the scroll far.
    /// Returns whether the view or the head moved.
    fn drag_autoscroll(&mut self) -> bool {
        let Some((column, row)) = self.drag_position else {
            return false;
        };
        let Some(view) = self.last_view.as_ref().copied() else {
            return false;
        };
        let height = usize::from(view.area.height);
        if height == 0 || view.total == 0 {
            return false;
        }
        let selectable_last = view.selectable.saturating_sub(1);
        let col = usize::from(column.saturating_sub(view.area.x))
            .min(usize::from(view.area.width.saturating_sub(1)));
        if row <= view.area.y {
            let head = CellPos {
                line: view.skip.saturating_sub(1).min(selectable_last),
                col,
            };
            let scrolled = view.skip > 0;
            if scrolled {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.stick = Stick::Hold;
            }
            let extended = self.extend_selection_head(head);
            scrolled || extended
        } else if usize::from(row).saturating_add(1) >= usize::from(view.area.bottom()) {
            let last = view
                .skip
                .saturating_add(1)
                .min(view.total.saturating_sub(height))
                .saturating_add(height)
                .saturating_sub(1)
                .min(view.total.saturating_sub(1))
                .min(selectable_last);
            let head = CellPos { line: last, col };
            let scrolled = self.scroll_offset > 0;
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
            self.rearm_if_near_bottom();
            let extended = self.extend_selection_head(head);
            scrolled || extended
        } else {
            false
        }
    }

    /// Point the selection head at `head`, reporting whether it moved.
    ///
    /// Exactly one selection is in play during a drag; with none —
    /// the button already released, say — there is nothing to extend.
    fn extend_selection_head(&mut self, head: CellPos) -> bool {
        match self.selection.as_mut() {
            Some(selection) => {
                let moved = selection.head != head;
                selection.head = head;
                moved
            }
            None => false,
        }
    }

    /// Move the selection head one step for Shift and an arrow key.
    ///
    /// The head moves and the anchor stays, so stepping away from the
    /// anchor extends the selection and stepping back over it shrinks
    /// it — the same select-by-keyboard every editor ships. Left and
    /// Right step one cell and wrap at line ends; Up and Down step
    /// one rendered line and keep the column. The viewport follows
    /// the head, and the adjusted text replaces the clipboard copy
    /// silently, exactly as a release does.
    fn nudge_selection(&mut self, direction: KeyCode) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let head = selection.head;
        let up_one = head.line.saturating_sub(1);
        let down_one = head.line.saturating_add(1);
        let nudged = match direction {
            KeyCode::Left => {
                if head.col > 0 {
                    CellPos {
                        line: head.line,
                        col: head.col.saturating_sub(1),
                    }
                } else if head.line > 0 {
                    let width = self.line_width(up_one).unwrap_or(0);
                    CellPos {
                        line: up_one,
                        col: width.saturating_sub(1),
                    }
                } else {
                    head
                }
            }
            KeyCode::Right => {
                let width = self.line_width(head.line).unwrap_or(0);
                if width > 0 && head.col.saturating_add(1) < width {
                    CellPos {
                        line: head.line,
                        col: head.col.saturating_add(1),
                    }
                } else if self.line_width(down_one).is_some() {
                    CellPos {
                        line: down_one,
                        col: 0,
                    }
                } else {
                    head
                }
            }
            KeyCode::Up => {
                if head.line > 0 {
                    CellPos {
                        line: up_one,
                        col: head.col,
                    }
                } else {
                    head
                }
            }
            KeyCode::Down => {
                if self.line_width(down_one).is_some() {
                    CellPos {
                        line: down_one,
                        col: head.col,
                    }
                } else {
                    head
                }
            }
            _ => head,
        };
        if nudged == head {
            return;
        }
        if let Some(selection) = self.selection.as_mut() {
            selection.head = nudged;
        }
        self.follow_head(nudged.line);
        let covered = self
            .selection
            .as_ref()
            .filter(|selection| selection.anchor != selection.head)
            .map(|selection| self.selection_text(selection));
        if let Some((text, covered_any)) = covered
            && covered_any
        {
            self.copy_selection_and_notice(&text);
        }
    }

    /// The rendered width, in cells, of one selectable conversation
    /// line.
    ///
    /// `None` past the end of the document the selection walks — the
    /// same line segments [`Self::selectable_window`] windows over,
    /// with running tools excluded. Left and Right use the width as
    /// their wrap boundary and Down uses existence as its floor.
    fn line_width(&self, index: usize) -> Option<usize> {
        self.selectable_window(index, 1)
            .into_iter()
            .next()
            .map(|line| line.spans.iter().map(|span| span.content.width()).sum())
    }

    /// A window over the lines a selection can cover.
    ///
    /// The settled conversation, the frozen streaming prefix, and
    /// the live region — running tools render after these but are
    /// not selectable, so they stay out of the selection's line
    /// space: what a copy walks and what the highlight spans are the
    /// same lines the pane shows above any tool block.
    fn selectable_window(&self, skip: usize, height: usize) -> Vec<Line<'_>> {
        visible_window(
            &[
                &self.conversation_cache,
                &self.frozen_lines,
                &self.live_lines,
                &NO_LINES,
                &NO_LINES,
                &NO_LINES,
                &NO_LINES,
            ],
            skip,
            height,
        )
    }

    /// Scroll the view just enough to keep `line` on screen.
    ///
    /// A keyboard-walked selection head moves through the document
    /// under its own power; the viewport follows so the moving end
    /// stays visible — stepping upward detaches the view, and
    /// stepping back within the stick tolerance re-arms it.
    fn follow_head(&mut self, line: usize) {
        let Some(view) = self.last_view.as_ref().copied() else {
            return;
        };
        let height = usize::from(view.area.height);
        if height == 0 {
            return;
        }
        if line < view.skip {
            self.scroll_offset = self
                .scroll_offset
                .saturating_add(view.skip.saturating_sub(line));
            self.stick = Stick::Hold;
        } else if line >= view.skip.saturating_add(height) {
            let excess = line
                .saturating_sub(view.skip.saturating_add(height))
                .saturating_add(1);
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
            self.rearm_if_near_bottom();
        }
    }

    /// The transcript cell under screen column/row, if the position
    /// lands inside the last frame's conversation pane.
    ///
    /// The line clamps into the selectable document: running tools
    /// render below it but never join the selection's line space,
    /// so a press on a tool row — or on the empty pane under short
    /// content — anchors on the last line a copy can walk, the way
    /// an editor clamps a click below a document's end. A pane with
    /// nothing selectable offers no cell at all.
    fn cell_at(&self, column: u16, row: u16) -> Option<CellPos> {
        let view = self.last_view.as_ref()?;
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return None;
        }
        let last = view.selectable.checked_sub(1)?;
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)))
            .min(last);
        let col = usize::from(column.saturating_sub(view.area.x));
        Some(CellPos { line, col })
    }

    /// Toggle the tool expansion at screen column/row, if any.
    ///
    /// A click — press and release without travel — on a tool
    /// summary row or a running tool row opens or closes that
    /// call's expansion. Toggling a completed block changes the
    /// rendered line count below it, so it bumps the conversation
    /// generation and forfeits the selection, the same way any
    /// renumbering mutation does; a running call's rows are
    /// rebuilt every frame and need no bump. Returns whether a
    /// tool row was hit at all.
    fn toggle_tool_at(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.last_view else {
            return false;
        };
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return false;
        }
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)));
        let call_id = self
            .tool_summary_lines
            .get(&line)
            .or_else(|| self.tool_running_lines.get(&line))
            .cloned()
            .unwrap_or_default();
        if call_id.is_empty() {
            return false;
        }
        let expanding = !self.expanded_tools.remove(&call_id);
        if expanding {
            self.expanded_tools.insert(call_id.clone());
        }
        if self.tool_details.contains_key(&call_id) {
            if expanding {
                self.stick = Stick::Hold;
            }
            self.conversation_generation = self.conversation_generation.saturating_add(1);
            self.rebake_whole();
            self.selection = None;
            self.drag_position = None;
        }
        true
    }

    /// The live thinking section's rows, and where its row sits.
    ///
    /// Renders while a reasoning stream is living and the turn's
    /// reply text has not begun: one row carrying the marker, label,
    /// and running duration, with the capped italic body below it under
    /// Follow and nothing under Suppressed — and every returned row a
    /// click target for the Follow/Suppressed mode, as tool rows are.
    /// The reply's first text delta folds the lane for the rest of
    /// the turn; reasoning deltas keep accumulating out of sight. The
    /// breathing bar itself lives in the pinned strip above the
    /// composer, not here. The second slot is the block's first row
    /// within the returned lines, for click registration.
    fn live_thinking_lines(&self, width: u16) -> (Vec<Line<'static>>, Option<usize>) {
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return (Vec::new(), None);
        }
        let stream = self
            .state
            .thinking
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !stream.is_living() {
            return (Vec::new(), None);
        }
        let folded = stream.text_began;
        if folded
            && stream.turn
                != self
                    .state
                    .reply_turn
                    .load(std::sync::atomic::Ordering::SeqCst)
        {
            return (Vec::new(), None);
        }
        let now = Instant::now();
        let phase_end = stream.text_began_at.unwrap_or(now);
        let elapsed = stream.began.map_or(0, |began| {
            u64::try_from(phase_end.duration_since(began).as_millis()).unwrap_or(0)
        });
        let dim = self.theme.ui.dim;
        let expanded = !folded && matches!(self.thinking_mode, ThinkingMode::Follow);
        let marker = if expanded { "▾" } else { "▸" };
        // The title rides the running register — the same foreground a
        // running tool's title carries — while the stream is live; the
        // completed block grey-outs in the dim tone, and so does every
        // auxiliary span here (marker, elapsed, body). Only the word
        // itself stands at full voice while there is thinking to read.
        let row = Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(dim)),
            Span::styled(
                "Thinking",
                Style::default().add_modifier(ratatui::style::Modifier::ITALIC),
            ),
            Span::styled(
                format!(" · {}", format_duration(elapsed)),
                // The elapsed reads light while the stream runs — the
                // theme's reply color — and grey-outs with the whole
                // block once it settles.
                Style::default().fg(self.theme.ui.assistant_message_fg),
            ),
        ]);
        let mut lines = vec![row];
        if expanded {
            let body_source: &str = if stream.redacted && stream.text.is_empty() {
                THINKING_REDACTED_PLACEHOLDER
            } else {
                stream.text.as_str()
            };
            let body_style = Style::default()
                .fg(dim)
                .add_modifier(ratatui::style::Modifier::ITALIC);
            let mut wrapped = Vec::new();
            for segment in body_source.lines() {
                wrapped.extend(plain_wrapped_lines_styled(
                    segment,
                    usize::from(width),
                    body_style,
                ));
            }
            let start = wrapped.len().saturating_sub(LIVE_THINKING_BODY_CAP);
            lines.extend(wrapped.into_iter().skip(start));
        }
        let toggleable = if folded { None } else { Some(0) };
        (lines, toggleable)
    }

    /// The running compaction pass's rows for the live lane.
    ///
    /// Renders while the pass-start cue holds — one row in the same
    /// register as the live thinking row: marker, italic label, and
    /// the running duration measured from the cue. The pass settles
    /// into the transcript's compacted row on its own; nothing
    /// renders here once it does.
    fn live_compaction_lines(&self) -> Vec<Line<'static>> {
        let began = {
            let health = self
                .state
                .health
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let HealthNotice::Compacting { began } = *health else {
                return Vec::new();
            };
            began
        };
        let dim = self.theme.ui.dim;
        let elapsed = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        vec![Line::from(vec![
            Span::styled("▸ ".to_string(), Style::default().fg(dim)),
            Span::styled(
                "Compacting",
                Style::default()
                    .fg(dim)
                    .add_modifier(ratatui::style::Modifier::ITALIC),
            ),
            Span::styled(
                format!(" · {}", format_duration(elapsed)),
                Style::default().fg(dim),
            ),
        ])]
    }

    /// Toggle the thinking section a press landed on.
    ///
    /// Every row of the live block — marker or expanded body — drives
    /// the session's Follow/Suppressed mode, as tool blocks do; a
    /// completed section's rows toggle that section's display alone.
    /// Returns whether the press belonged to a thinking section at
    /// all, so callers can fall through to the tool rows.
    fn toggle_thinking_at(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.last_view else {
            return false;
        };
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return false;
        }
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)));
        if self
            .thinking_live_rows
            .as_ref()
            .is_some_and(|rows| rows.contains(&line))
        {
            self.thinking_mode = match self.thinking_mode {
                ThinkingMode::Follow => ThinkingMode::Suppressed,
                ThinkingMode::Suppressed => ThinkingMode::Follow,
            };
            self.selection = None;
            self.drag_position = None;
            return true;
        }
        let Some(key) = self.thinking_summary_lines.get(&line).cloned() else {
            return false;
        };
        let expanding = !self.expanded_thinking.remove(&key);
        if expanding {
            self.expanded_thinking.insert(key);
            self.stick = Stick::Hold;
        }
        self.conversation_generation = self.conversation_generation.saturating_add(1);
        self.rebake_whole();
        self.selection = None;
        self.drag_position = None;
        true
    }

    /// Toggle the compaction notice a press landed on.
    ///
    /// Mirrors the completed thinking sections: every row of the
    /// notice toggles its own display between the collapsed summary
    /// and the pass's details. Returns whether the press belonged to
    /// a notice at all, so callers can fall through to the tool rows.
    fn toggle_compaction_at(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.last_view else {
            return false;
        };
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return false;
        }
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)));
        let Some(key) = self.compaction_summary_lines.get(&line).cloned() else {
            return false;
        };
        let expanding = !self.expanded_compactions.remove(&key);
        if expanding {
            self.expanded_compactions.insert(key);
            self.stick = Stick::Hold;
        }
        self.conversation_generation = self.conversation_generation.saturating_add(1);
        self.rebake_whole();
        self.selection = None;
        self.drag_position = None;
        true
    }

    /// Place the composer's caret where a press landed.
    ///
    /// Presses inside the composer pane claim the press — padding
    /// rows included, click-clamped the way editors clamp: the row
    /// clamps into the text window, and the column travels raw so
    /// the caret search clamps it to that row's own width. Returns
    /// whether the press belonged to the composer at all.
    fn place_input_caret(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.input_view else {
            return false;
        };
        if column < view.pane.x
            || column >= view.pane.right()
            || row < view.pane.y
            || row >= view.pane.bottom()
        {
            return false;
        }
        let grid_row = view.start.saturating_add(
            usize::from(row.saturating_sub(view.origin.1)).min(view.visible.saturating_sub(1)),
        );
        let grid_column = usize::from(column.saturating_sub(view.origin.0));
        self.input
            .move_caret_to_cell(self.input_wrap_width, grid_row, grid_column);
        true
    }

    /// The characters the selection covers, as plain text, with
    /// whether any character was covered at all.
    ///
    /// Walks the same rendered lines the highlight walks, so what is
    /// copied is exactly what is shown selected. The flag is false
    /// only when no row covered a single character — a span over
    /// blank cells — so a caller can leave the clipboard alone even
    /// though the separators make the text itself non-empty; an
    /// interior blank line inside a genuine selection is content
    /// and does not clear the flag.
    fn selection_text(&self, selection: &CellSelection) -> (String, bool) {
        let (from, to) = ordered(selection);
        let lines = self.selectable_window(
            from.line,
            to.line.saturating_sub(from.line).saturating_add(1),
        );
        let mut text = String::new();
        let mut covered_any = false;
        for (offset, line) in lines.iter().enumerate() {
            let line_index = from.line.saturating_add(offset);
            let start = if line_index == from.line { from.col } else { 0 };
            let end = if line_index == to.line {
                to.col
            } else {
                usize::MAX
            };
            let flat: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            let covered = covered_chars(&flat, start, end);
            if !covered.is_empty() {
                covered_any = true;
            }
            text.push_str(&covered);
            if line_index < to.line {
                text.push('\n');
            }
        }
        (text, covered_any)
    }

    /// Route an editor action.
    ///
    /// A submit hands the prompt to [`submit_text`](Self::submit_text)
    /// and re-anchors the view to the conversation's newest row; the rest
    /// need nothing beyond the redraw the caller grants.
    fn apply_input_action(&mut self, action: InputAction) -> bool {
        match action {
            InputAction::Submit(text) => {
                self.submit_text(text);
                true
            }
            InputAction::Redraw | InputAction::None => true,
        }
    }

    /// Send one submitted prompt to the agent driver.
    ///
    /// The prompt joins the shared queue before its id is sent, so the
    /// driver always finds the entry behind an id it was handed — and
    /// finds it as it stands then, which is what lets a prompt be fixed
    /// after the fact rather than spent. A send nothing will take puts
    /// the prompt back out of the queue: an entry no driver can claim
    /// would otherwise sit in the panel, offering edits to a prompt that
    /// can never leave.
    ///
    /// The transcript row is not written here. The driver records the
    /// claim with [`push_sent`](crate::observer::TuiObserverState::push_sent)
    /// and the drain turns it into the row, so what the transcript shows
    /// is the text that went out — an edit included, a cancel excluded —
    /// at the place in the transcript its claim time gives it. A bare
    /// app has no driver to claim, so its echo stays here, where it is
    /// the only record the prompt would ever get.
    ///
    /// A send the channel refuses is the session ending around the prompt:
    /// the entry leaves the queue, the notice row says why, and the words
    /// go back into the composer. They were never sent, so they take no
    /// transcript row; and they are the user's, so a driver's exit is not
    /// the thing that throws them away.
    fn submit_text(&mut self, text: String) {
        if self.submit_tx.is_some() {
            // The handoff window: an empty queue in a session at rest
            // — no claim standing, no run going — means this prompt is
            // the driver's very next claim, one channel hop away, and
            // the row hides until the claim takes it, so no frame draws
            // it as waiting. Anything else (a backlog, a claim, a run
            // in flight) is genuinely waiting and shows.
            let was_empty = self.queued_len() == 0;
            let idle = self.state.live_claim.load(Ordering::SeqCst) == u64::MAX
                && !self.state.agent_running.load(Ordering::SeqCst);
            self.handed = None;
            let id = self.push_queued(text.clone());
            if self.send_queued_id(id) {
                if was_empty && idle {
                    self.handed = Some(id);
                }
                self.scroll_to_bottom();
                return;
            }
            self.remove_queued(id);
            self.post_notice("the driver has gone — nothing more can be sent".to_string());
            self.input.set_text(text);
        } else {
            self.push_message(TuiMessage::User {
                text,
                timestamp: chrono::Utc::now(),
                steered: false,
            });
        }
        self.scroll_to_bottom();
    }

    /// Hand `id` to the driver, reporting whether the channel took it.
    ///
    /// The send is a [`Submission::Run`]: the driver resolves the id
    /// against the shared queue when the turn comes, so this never has to
    /// take anything back.
    fn send_queued_id(&self, id: u64) -> bool {
        self.submit_tx
            .as_ref()
            .is_some_and(|tx| tx.send(Submission::Run { id }).is_ok())
    }

    /// Tell the driver that the prompt `id` was corrected after it went out.
    ///
    /// The gone-arm of a commit: the entry left the queue while its text
    /// stood open in the composer, so the edited wording has to reach the
    /// agent as a turn of its own — and the turn that went out with the
    /// superseded wording has to stop standing in front of it, which only
    /// the driver's loop can do. The cancel and its flag are raised only
    /// when the correction names the turn in flight — the driver mirrors
    /// its live claim into shared state, and a correction committing
    /// while some other run is going must not cancel it: that run is an
    /// unrelated conversation the fix was never aimed at. The cut itself
    /// follows the most recent claim however it settled — a turn that
    /// already landed and still stands as the newest claim is taken out
    /// and the fix runs in its place, no cancel needed.
    /// With no driver (or a channel nobody listens to) there is no turn
    /// to cut and nothing to run it, so the correction goes into the
    /// transcript the way any prompt does when this app is standing
    /// alone. The named turn in flight is stopped, not waited out: the
    /// correction is sent first and the runner's cancel raised beside
    /// it, so a superseded turn stops spending its run on wording the
    /// user has already replaced — the driver clears the signal before
    /// the corrected turn starts.
    fn correct_claimed(&mut self, id: u64, text: String) {
        let taken = self.submit_tx.as_ref().is_some_and(|tx| {
            tx.send(Submission::Correct {
                id,
                text: text.clone(),
            })
            .is_ok()
        });
        if taken {
            if self.state.live_claim.load(Ordering::SeqCst) == id {
                self.state.correction_cancel.store(id, Ordering::SeqCst);
                if let Some(cancel) = &self.run_canceller {
                    cancel();
                }
                // The check-then-cancel above is not atomic, and the
                // window it leaves is narrow but real: between the
                // read that matched and the cancel landing, the named
                // turn can settle and the driver can claim the next
                // prompt — the cancel would then stop a run the
                // correction never named. Re-reading the mirror after
                // the cancel closes what it can: a mirror that has
                // moved on names a turn the flag must not ride, so
                // the flag comes down and the cancellation it caused
                // is the run's own to report. What remains beyond
                // this guard — a claim landing between the second
                // read and the driver's pre-run clear — cannot spend
                // a prompt silently: the driver's settle goes quiet
                // only for a run whose own claim the flag carries,
                // so the run resolves cancelled with its error row
                // and the buffered correction runs as its own turn
                // behind it.
                if self.state.live_claim.load(Ordering::SeqCst) != id {
                    self.state
                        .correction_cancel
                        .store(u64::MAX, Ordering::SeqCst);
                }
            }
        } else {
            self.submit_text(text);
        }
    }

    /// Add `text` to the shared queue and name it.
    ///
    /// The id this returns is the prompt's only handle from here on: the
    /// panel addresses its row through it and the channel carries nothing
    /// else, which is why an id is issued before the send rather than
    /// taken from whatever position the prompt happens to land at.
    fn push_queued(&self, text: String) -> u64 {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text)
    }

    /// Take the prompt `id` out of the shared queue.
    ///
    /// The user's drop, the rollback of a send nobody took, and the
    /// cancel's clearing all need the same act: the prompt is gone and
    /// can never run.
    fn remove_queued(&self, id: u64) -> Option<String> {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
    }

    /// Rewrite the prompt `id` holds, reporting whether it was still there.
    ///
    /// In place and by id: the saved text takes the same slot the prompt
    /// was submitted into, and a prompt the driver claimed mid-edit
    /// answers `false` rather than letting the text land on its neighbour.
    fn replace_queued(&self, id: u64, text: String) -> bool {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(id, text)
    }

    /// The queue's display length: every waiting prompt, minus the one
    /// whose claim is a channel hop away.
    ///
    /// The handoff hide is display-only — the shared queue still holds
    /// the prompt for the driver to claim, and the exit and resume
    /// paths read the queue directly, unaffected. The hidden row is
    /// the queue's last (its window only opens on a queue that was
    /// empty), so earlier rows keep their display positions.
    fn queued_display_len(&self) -> usize {
        let total = self.queued_len();
        if self
            .handed
            .is_some_and(|id| self.queued_index_of(id).is_some())
        {
            total.saturating_sub(1)
        } else {
            total
        }
    }

    /// Fold the auto-expanded todo block when the next tool is
    /// running.
    ///
    /// The graduation fold covers the conversation's next completed
    /// step; this covers its next *started* one — a tool in flight is
    /// the conversation moving on even before its result graduates.
    /// Manual expansions are untouched: only the tracked block folds.
    /// A fold that clears a flag rebakes the whole bake, the way an
    /// eviction does: the block sits mid-conversation, past the tail
    /// the incremental bake re-renders, so its expanded rows would
    /// otherwise keep painting a detail no toggle backs any more.
    fn settle_todo_expansion(&mut self) {
        let Some(id) = self.auto_expanded_todo.as_ref() else {
            return;
        };
        let running = self
            .state
            .active_tools
            .lock()
            .is_ok_and(|tools| !tools.is_empty());
        if running {
            let id = id.clone();
            let folded = self.expanded_tools.remove(&id);
            self.auto_expanded_todo = None;
            if folded {
                self.conversation_generation = self.conversation_generation.saturating_add(1);
                self.rebake_whole();
            }
        }
    }

    /// Retire the handoff hide once its prompt left the queue.
    ///
    /// A claimed or dropped prompt is gone from the shared queue, and
    /// the window its hide justified is over. A prompt still waiting
    /// keeps its hide — that shape means a driver that stopped
    /// claiming, and the next submit clears the field before its own
    /// push, so at most one row hides at a time.
    fn settle_handed(&mut self) {
        if let Some(id) = self.handed
            && self.queued_index_of(id).is_none()
        {
            self.handed = None;
        }
    }

    /// How many prompts wait.
    ///
    /// Two callers depend on this count agreeing with what the panel shows:
    /// the status tag, which names a number of prompts the user can go on to
    /// walk, and the layout, which holds rows for the section only while it
    /// is non-zero — a session with nothing waiting keeps its exact rows.
    /// A poisoned lock still answers from the queue it holds: a panic
    /// elsewhere in the process says nothing about what the user queued.
    fn queued_len(&self) -> usize {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The prompt at display position `index`, as `(id, text)`.
    ///
    /// The one conversion from a row on screen to an address, read fresh
    /// every frame: a prompt claimed or dropped since the last frame
    /// moves the rows underneath the cursor, and a remembered position
    /// would point at whoever slid into its place.
    fn queued_at(&self, index: usize) -> Option<(u64, String)> {
        let queue = self
            .state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        queue.get(index).map(|entry| (entry.id, entry.text.clone()))
    }

    /// Where the prompt `id` sits now, if it still waits.
    ///
    /// Asked after the fact rather than remembered: a save lands by id,
    /// and this only answers where to draw the row it landed in.
    fn queued_index_of(&self, id: u64) -> Option<usize> {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .index_of(id)
    }

    /// Apply a key press to the queue section while it holds the keys.
    ///
    /// `Some(redraw)` when the section owns the press — as the focused
    /// list, or as the prompt standing open in the composer. `None` while
    /// the queue holds neither, and for the keys
    /// [`key_survives_the_sheet`] names: the run's cancel, copy, and
    /// steering chords and the transcript's paging stay live over the
    /// section, the same courtesy the ask sheet extends, because a user
    /// checking what a waiting prompt refers to has to be able to read
    /// the conversation it came out of. Ctrl+Q is asked here rather than
    /// below for the two moments it does own: entering the section from
    /// the composer, and committing an edit from inside it. Leaving a
    /// focused section is `Esc`'s and the chord's both — the focused arm
    /// below takes the key and hands the keys back, so one press means
    /// enter and the next means leave.
    fn queue_key(&mut self, key: KeyEvent) -> Option<bool> {
        if matches!(self.queue_mode, QueueMode::Editing { .. }) {
            if let (KeyCode::Char('q'), KeyModifiers::CONTROL) = (key.code, key.modifiers) {
                self.commit_queue_edit();
                return Some(true);
            }
            return self.queue_edit_key(key);
        }
        if key_survives_the_sheet(key) {
            return None;
        }
        if let QueueMode::Confirming { act } = self.queue_mode {
            return Some(self.queue_confirm_key(key, act));
        }
        if self.queue_mode == QueueMode::Focused {
            return Some(self.queue_focused_key(key));
        }
        if let (KeyCode::Char('q'), KeyModifiers::CONTROL) = (key.code, key.modifiers) {
            return Some(self.focus_queue());
        }
        None
    }

    /// The keys the section takes back while a prompt is out in the
    /// composer.
    ///
    /// Typing, the caret, and pasting stay the composer's: the draft is
    /// fixed with the same keys a new prompt is written with, which is the
    /// point of lifting it there rather than into a box with its own
    /// editor. Three chords come back here — Enter saves into the prompt
    /// the text came from, and Esc and Ctrl+C both put the draft away.
    /// Ctrl+C is the one whose meaning moves: with a prompt out, the press
    /// lands on the text in front of the user rather than reaching past it
    /// to a run they have stopped looking at, and the run's cancel is one
    /// Esc away. Neither abort drops anything — a key taken as a way out
    /// of the mode leaves the waiting prompt exactly as it was found.
    /// Everything else is the composer's, and falls through.
    fn queue_edit_key(&mut self, key: KeyEvent) -> Option<bool> {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) | (KeyCode::Char('m'), KeyModifiers::CONTROL) => {
                self.commit_queue_edit();
                Some(true)
            }
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.abort_queue_edit();
                Some(true)
            }
            _ => None,
        }
    }

    /// The keys the focused list answers.
    ///
    /// Left and Right walk the waiting prompts, `e` lifts the shown one
    /// into the composer, `s` sends it to the turn in flight as guidance,
    /// Delete or `d` takes it out of the queue for good, and Esc hands the
    /// keyboard back. Whatever else arrives is
    /// swallowed here rather than handed down: a letter typed while the
    /// user reads the backlog has nowhere it should be going, and letting
    /// it reach the composer would build a draft behind their back.
    fn queue_focused_key(&mut self, key: KeyEvent) -> bool {
        let bare = key.modifiers == KeyModifiers::NONE;
        match key.code {
            KeyCode::Left if bare => self.step_queue_view(false),
            KeyCode::Right if bare => self.step_queue_view(true),
            KeyCode::Esc => self.queue_mode = QueueMode::Plain,
            KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
                self.queue_mode = QueueMode::Plain;
            }
            KeyCode::Char('e') if bare => self.open_queue_edit(),
            KeyCode::Delete | KeyCode::Char('d') if bare => {
                self.queue_mode = QueueMode::Confirming {
                    act: QueueAct::Drop,
                };
            }
            KeyCode::Char('s') if bare => {
                self.queue_mode = QueueMode::Confirming {
                    act: QueueAct::Steer,
                };
            }
            _ => {}
        }
        true
    }

    /// The keys the confirmation answers.
    ///
    /// `y` performs the act the ask holds — after the mode returns to
    /// `Focused`, so the act's own emptying of the queue still releases
    /// the focus the way the immediate keypress once did. Everything
    /// else stands the ask down: `n` and Esc plainly, and any stray key
    /// too, because a key that lands by accident must never be the one
    /// that spends the prompt.
    fn queue_confirm_key(&mut self, key: KeyEvent, act: QueueAct) -> bool {
        if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Char('y') {
            self.queue_mode = QueueMode::Focused;
            match act {
                QueueAct::Drop => self.drop_queue_entry(),
                QueueAct::Steer => self.steer_queued(),
            }
        } else {
            self.queue_mode = QueueMode::Focused;
        }
        true
    }

    /// Put the keyboard in the queue section's hands.
    ///
    /// `false` when nothing waits: the chord asks for a panel, and a
    /// session with nothing queued has none, so taking the keys then
    /// would leave a mode with nothing on screen to act on. A view left
    /// from an earlier walk is clamped back onto the queue rather than
    /// reset, so stepping out and in again returns to where the walk was;
    /// a first entry meets the queue at the front, the prompt that runs
    /// next.
    fn focus_queue(&mut self) -> bool {
        if self.queued_display_len() == 0 {
            return false;
        }
        self.queue_mode = QueueMode::Focused;
        self.clamp_queue_view();
        true
    }

    /// Lift the shown prompt into the composer for editing.
    ///
    /// What is already in the composer is not collateral: with a draft
    /// standing there the prompt stays in the queue and the reason goes on
    /// the notice row. The two texts have nowhere to meet — overwriting
    /// the user's unsent words to edit somebody else's prompt would trade
    /// one lost draft for two.
    ///
    /// The focus is released for the duration, which is the mechanism and
    /// not a courtesy: the list's keys and the composer's keys are
    /// different keys, and typing has to reach the text on screen.
    fn open_queue_edit(&mut self) {
        let Some((id, text)) = self.queued_at(self.queue_view) else {
            self.clamp_queue_view();
            return;
        };
        if !self.input.is_empty() {
            self.post_notice(
                "the composer holds a draft — empty it to edit a waiting prompt".to_string(),
            );
            return;
        }
        self.input.set_text(text);
        self.queue_mode = QueueMode::Editing { id };
    }

    /// Save the composer's text into the prompt it was opened from.
    ///
    /// A save is not a submission: nothing goes out, nothing enters the
    /// input history — the recall the Up key walks is the list of prompts
    /// actually sent, and an edit was never one — and the run in flight is
    /// untouched. What changes is the body the claim will read, in the
    /// slot the prompt was submitted into, so the order the prompts were
    /// typed in stays the order their turns run in.
    ///
    /// Two edges, and both say so on the notice row. A blank body is
    /// refused rather than saved: an entry emptied into a prompt of nothing
    /// is worse to find on the row than the wrong word that was there, and
    /// the draft stays out for another try. A prompt the run claimed while
    /// its text was out cannot be edited retroactively — the words it went
    /// with already reached the model — so the fix goes out as a prompt of
    /// its own instead of being quietly thrown away.
    fn commit_queue_edit(&mut self) {
        let QueueMode::Editing { id } = self.queue_mode else {
            return;
        };
        let text = self.input.text().to_string();
        if text.trim().is_empty() {
            self.post_notice(
                "the prompt keeps its words — a save needs something to send".to_string(),
            );
            return;
        }
        self.input.clear();
        if self.replace_queued(id, text.clone()) {
            if let Some(index) = self.queued_index_of(id) {
                self.queue_view = index;
            }
        } else {
            self.correct_claimed(id, text);
        }
        self.leave_queue_edit();
        self.clamp_queue_view();
    }

    /// Put the draft away, leaving its prompt exactly as it was.
    ///
    /// The way out of the mode, not out of the queue: the prompt keeps the
    /// wording it had before the composer took it and waits where it
    /// waited, and the composer's buffer — which held somebody else's
    /// prompt on loan, never the user's own draft — goes with it.
    fn abort_queue_edit(&mut self) {
        if !matches!(self.queue_mode, QueueMode::Editing { .. }) {
            return;
        }
        self.input.clear();
        self.clamp_queue_view();
        self.leave_queue_edit();
    }

    /// Take the shown prompt out of the queue for good.
    ///
    /// A waiting prompt is the only thing this touches: the run in flight
    /// keeps running, the prompts behind it keep their order, and the
    /// prompt itself simply never runs. There is no confirmation because
    /// there is nothing to lose that cannot be retyped — the entry is on
    /// screen at the press, and it is the only way to unload an entry a
    /// blank save refuses to empty.
    fn drop_queue_entry(&mut self) {
        let Some((id, _)) = self.queued_at(self.queue_view) else {
            self.clamp_queue_view();
            return;
        };
        self.remove_queued(id);
        self.clamp_queue_view();
        self.release_queue_if_empty();
    }

    /// Send the composer straight to the model, mid-turn: `Ctrl+S`.
    ///
    /// Enter is patient and Ctrl+S is not. With a run in flight and the
    /// seam installed the text goes to the steering slot and reaches the
    /// model at the run's next turn boundary, joining the conversation the
    /// model continues rather than waiting behind the queue. With no run to
    /// steer — or with a run that ended in the instant between the keypress
    /// and the offer — `steer` refuses and the text takes the ordinary
    /// path, so the chord is never a key that eats what was typed.
    ///
    /// A prompt open in the composer for editing belongs to the waiting
    /// section no longer: its slot is retired first, or the same words
    /// would reach the model now *and* run again from the queue later.
    ///
    /// A draft of nothing but blanks is refused, exactly as Enter refuses it:
    /// the chord sends what Enter would send, and Enter sends nothing here.
    /// From an edit the stakes are higher than a wasted row — the slot is
    /// retired before delivery — so a whitespace-only draft would drop a real
    /// waiting prompt to deliver text nobody typed.
    ///
    /// The row is owed from here rather than at a claim: unlike a
    /// queued prompt, this one is sent by the hand that writes it.
    /// The transcript itself records it only when the model reads it
    /// — the row waits pinned just above the timer until the boundary
    /// that drains it — so the send and the record are two moments,
    /// and both are the sender's own.
    fn steer_now(&mut self) -> bool {
        if self.input.text().trim().is_empty() {
            return true;
        }
        let text = self.input.text().to_string();
        if let QueueMode::Editing { id } = self.queue_mode {
            self.remove_queued(id);
            self.leave_queue_edit();
            self.clamp_queue_view();
            self.release_queue_if_empty();
        }
        self.input.clear();
        self.input.remember(text.clone());
        self.deliver_guidance(text);
        true
    }

    /// Hand `text` to the run in flight, or send it as a turn.
    ///
    /// The one delivery path behind both entrances to steering — the
    /// composer's `Ctrl+S` and the waiting section's `s` — so the two can
    /// never mean different things. Accepted, it is the loop's to read
    /// at its next boundary; its transcript row is not written at the
    /// send, though — the model has not read the words yet — but pinned
    /// just above the running turn's timer as a user row, in the spot
    /// the boundary that reads it will land it in, after the work that
    /// was running. A run that ends before any boundary reads it
    /// hands the text to the waiting section instead. Refused, it
    /// takes the ordinary path.
    ///
    /// Recording on recall stays with the callers: a prompt that arrives
    /// out of the queue was already recorded when it was submitted, and
    /// saying it twice would list it twice.
    fn deliver_guidance(&mut self, text: String) {
        if self
            .guidance
            .as_ref()
            .is_some_and(|guidance| guidance.steer(text.clone()))
        {
            self.state.push_steered(text);
            self.scroll_to_bottom();
            return;
        }
        self.submit_text(text);
    }

    /// Guide the waiting prompt on screen: `s` in the focused section.
    ///
    /// The hole `Ctrl+S` cannot reach: a prompt typed a minute ago, queued
    /// patiently, that the running turn has since made urgent. It leaves
    /// the queue and joins the turn in flight, which is also why it leaves
    /// first — the same words must not both reach the model now and run
    /// again from the queue later.
    ///
    /// Refused while nothing is running. Guidance is advice for a turn, and
    /// a session between turns has no turn to advise; the prompt stays
    /// where it is, in order, rather than being fired into a gap. If the
    /// run ends in the instant between the key and the offer, the fallback
    /// gives it a turn of its own — the words outrank the slot they came
    /// out of.
    fn steer_queued(&mut self) {
        let Some((id, text)) = self.queued_at(self.queue_view) else {
            self.clamp_queue_view();
            return;
        };
        if !self.state.agent_running.load(Ordering::SeqCst) {
            self.post_notice(
                "nothing is running to guide — the prompt waits in its place".to_string(),
            );
            return;
        }
        self.remove_queued(id);
        self.clamp_queue_view();
        self.release_queue_if_empty();
        self.deliver_guidance(text);
    }

    /// Deliver what the last run never reached.
    ///
    /// The user pressed the send-now chord and the run ended before reading
    /// it, so the intent to send stands: the text goes back to the waiting
    /// section *with its turn promised again*, the way any submit promises
    /// one — a prompt standing in a section whose whole meaning is "this
    /// runs next", with nothing to make it run, would be a decoration. It
    /// stays editable up to its own claim: `e` fixes the wording, `d`
    /// takes it out, and a cancel stops only the turn in flight — the
    /// waiting prompts, this one included, keep their place.
    ///
    /// It goes to the *back* of the queue rather than the front, though it
    /// is the older text: the prompts already waiting were promised their
    /// turns first, and reordering them behind an unsent aside would be the
    /// louder surprise.
    ///
    /// A closed channel is the session ending, and the entry simply stays
    /// in the queue — an unrun prompt still held in the queue is the honest last state.
    /// The take is not atomic with the caller's idle check, and the gap is
    /// the window a run starts in: the frame sees no run in flight, and by
    /// the time the slot is emptied a new run is mid-turn. The texts are
    /// the finished run's, though — advice about turns that no longer
    /// stand, which the slot's own contract refuses to hand to a later
    /// run silently — so whatever started in the window, they wait here
    /// with a turn promised, under the notice that says the model never
    /// read them. The new run is not steered with them: a user who meant
    /// them for it still can, seeing the notice, and the alternative
    /// would put words into a conversation their author never aimed at.
    fn return_guidance(&mut self) {
        let Some(guidance) = self.guidance.as_ref() else {
            return;
        };
        let taken = guidance.take_returned();
        if taken.is_empty() {
            return;
        }
        // The pin held the same words the seam returns: those rows
        // leave the edge with their texts, the waiting section their
        // one home now. Anything the seam did not return stays pinned
        // — a boundary may still owe it a row.
        self.state
            .pending_steers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|text| !taken.contains(text));
        let count = taken.len();
        let mut promised = 0usize;
        for text in taken {
            let id = self.push_queued(text);
            if self.send_queued_id(id) {
                promised = promised.saturating_add(1);
            }
        }
        self.clamp_queue_view();
        let (noun, verb) = if count == 1 {
            ("prompt", "is")
        } else {
            ("prompts", "are")
        };
        let mut notice = format!("{count} {noun} the model never read {verb} waiting again");
        if promised < count {
            notice.push_str(" — no driver left to run them");
        }
        self.post_notice(notice);
    }

    /// Come back out of an edit, into the mode the section stands in now.
    ///
    /// The keys go to the section, because that is where they were the
    /// moment before `e` lifted a prompt out: an edit is a detour off the
    /// walk, not a new departure, and a user who saved one prompt's fix
    /// expects to step to the next with an arrow rather than a chord. With
    /// nothing left to stand on there is no section to hold the keys, and
    /// the composer takes them back.
    fn leave_queue_edit(&mut self) {
        self.queue_mode = if self.queued_len() == 0 {
            QueueMode::Plain
        } else {
            QueueMode::Focused
        };
    }

    /// Stand the section's focus down when the section is gone.
    ///
    /// The focus is the only mode released. It holds keys for a panel that
    /// is no longer drawn, and keeping it over an empty queue would swallow
    /// every keystroke for nothing. An open edit is kept: it holds no keys
    /// of the section's — typing, the caret, and pasting stay the composer's
    /// in that mode whatever the queue looks like — and what stands in the
    /// composer is the user's own text mid-edit, not something the section
    /// can release on the queue's behalf.
    ///
    /// Which is the moment the edit matters most. A run claiming the last
    /// waiting prompt is exactly when the prompt open in the composer has
    /// already gone out with the older wording, and the frame that draws the
    /// claim lands before the Enter that fixes it. Releasing the mode there
    /// would turn the fix into an ordinary second turn and quietly drop the
    /// cut the section promises; keeping it lets
    /// [`commit_queue_edit`](Self::commit_queue_edit) find the entry gone and
    /// send the wording as a correction instead.
    fn release_queue_if_empty(&mut self) {
        if self.queued_len() == 0
            && matches!(
                self.queue_mode,
                QueueMode::Focused | QueueMode::Confirming { .. }
            )
        {
            self.queue_mode = QueueMode::Plain;
        }
    }

    /// Walk the shown prompt one step, wrapping at both ends.
    ///
    /// The walk moves the view and leaves the queue's order alone, for the
    /// reason the ask sheet's walk gives: the count the section prints is
    /// the shown prompt's place in the order the prompts were submitted,
    /// and it says the same thing after a walk away and back. It is a
    /// circle, so the prompt at the tail is one press from the one at the
    /// front rather than a walk through everything between. A queue of one
    /// has nowhere to go and the step declines — the key is still
    /// swallowed, so an arrow cannot fall through to the transcript.
    fn step_queue_view(&mut self, forward: bool) {
        let count = self.queued_len();
        if count < 2 {
            return;
        }
        self.queue_view = if forward {
            let next = self.queue_view.saturating_add(1);
            if next >= count { 0 } else { next }
        } else if self.queue_view == 0 {
            count.saturating_sub(1)
        } else {
            self.queue_view.saturating_sub(1)
        };
    }

    /// Keep [`queue_view`](Self::queue_view) a live position in the queue.
    ///
    /// A claim at the front and a drop anywhere pull the rows behind them
    /// up a place, and a view left past the end would show nothing rather
    /// than the prompt that slid into the row. An emptied queue takes the
    /// view home to zero, which is also where the next prompt is met.
    fn clamp_queue_view(&mut self) {
        self.queue_view = self
            .queue_view
            .min(self.queued_display_len().saturating_sub(1));
    }

    /// Reconcile the section's view and mode with what the queue holds.
    ///
    /// Once a frame, because the queue changes underneath the keyboard
    /// with no key pressed: a run's claim takes the front prompt the
    /// instant its turn starts — one claim per turn, so the backlog
    /// thins under the walk whenever a turn lands. Nothing here is a
    /// policy the keys do not already
    /// apply — it is the same two rules, applied by the frame rather than
    /// waiting for a press that may never come.
    ///
    /// The mode half of the release is the frame's most consequential
    /// contribution, because the claim that empties the queue is the event
    /// that makes an open edit a correction rather than a save, and it
    /// always lands a frame before the Enter that follows it. [`release_queue_if_empty`]
    /// is why the frame stands down a focus and leaves an edit alone.
    fn sync_queue_view(&mut self) {
        if self.queued_len() == 0 {
            self.queue_view = 0;
            self.release_queue_if_empty();
            return;
        }
        self.clamp_queue_view();
    }

    /// Whether any tool call is in flight.
    ///
    /// The tick consults this to keep in-flight elapsed stamps live;
    /// a poisoned lock reads as none, the same policy the tool
    /// indicator rows apply.
    fn any_tools_running(&self) -> bool {
        self.state
            .active_tools
            .lock()
            .is_ok_and(|tools| !tools.is_empty())
    }

    /// Jump the view back to the bottom and re-arm stickiness.
    ///
    /// Submits call this so freshly appended output is visible and
    /// followed; the End key shares the path.
    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.stick = Stick::Follow;
    }

    /// Jump the view to the oldest line and drop stickiness.
    ///
    /// The Home key calls this. The offset is saturated because the
    /// scrollable height is layout-time knowledge: the next render's
    /// settle clamps it to the document's top, and the dropped
    /// stickiness keeps growth below the viewport from dragging the
    /// view back down.
    fn scroll_to_top(&mut self) {
        self.scroll_offset = usize::MAX;
        self.stick = Stick::Hold;
    }

    /// Re-arm stickiness when a scroll lands near the bottom.
    ///
    /// Scroll-down actions within [`STICK_TOLERANCE`] lines of the
    /// newest content count as "back at the bottom"; the next layout
    /// snaps fully.
    fn rearm_if_near_bottom(&mut self) {
        if self.scroll_offset <= STICK_TOLERANCE {
            self.stick = Stick::Follow;
        }
    }

    /// Decide whether a background-requested redraw may run now.
    ///
    /// A queued graduation always forces the frame — the graduated
    /// message must not wait out the cap. Otherwise a
    /// frame renders at most once per frame interval; a request
    /// arriving sooner parks itself in the pending flag for the
    /// periodic tick to claim. The caller supplies the frame clock.
    pub fn redraw_due(&mut self, now: Instant) -> bool {
        if self.graduation_waiting() {
            self.render_pending = false;
            self.last_frame = Some(now);
            return true;
        }
        let due = self
            .last_frame
            .is_none_or(|last| now.duration_since(last) >= FRAME_INTERVAL);
        if due {
            self.last_frame = Some(now);
            self.render_pending = false;
        } else {
            self.render_pending = true;
        }
        due
    }

    /// Claim a parked redraw for the periodic tick.
    ///
    /// Returns whether a capped background request was waiting;
    /// claiming stamps the frame time so the interval restarts. The
    /// claim does not itself re-check the interval — a parked
    /// request is owed its frame whenever the tick lands, which can
    /// sit inside the interval after a notify-initiated frame; the
    /// tick stays several frame intervals wide, so the reading is
    /// deliberate. Ticks with nothing pending and no tools in
    /// flight still draw nothing.
    pub fn take_pending_redraw(&mut self, now: Instant) -> bool {
        if !self.render_pending {
            return false;
        }
        self.render_pending = false;
        self.last_frame = Some(now);
        true
    }

    /// Handle a wake from the shared notify.
    ///
    /// Re-registers the listener before gating the redraw, so a
    /// notification raised while this wake is being handled finds a
    /// listener waiting and is delivered on the next iteration
    /// rather than lost. Returns whether the redraw may run.
    pub fn notify_wake(
        &mut self,
        notify: &event_listener::Event,
        listener: &mut event_listener::EventListener,
    ) -> bool {
        *listener = notify.listen();
        self.redraw_due(Instant::now())
    }

    /// Handle a periodic tick.
    ///
    /// Claims any parked background redraw first, advances the
    /// spinner one frame while tools run, steps the view while a
    /// drag is parked at the conversation pane's top or bottom row —
    /// the beat that keeps an edge push scrolling between movement
    /// reports — parked outright while a permission ask is pending —
    /// and flips the composer caret's blink phase on its
    /// own beat. An idle tick between flips and away from all of the
    /// above draws nothing.
    pub fn tick_wake(&mut self, now: Instant) -> bool {
        let claimed = self.take_pending_redraw(now);
        let dragged = if self.pending_permissions.front().is_some() {
            self.drag_position = None;
            false
        } else {
            self.drag_autoscroll()
        };
        let blinked = self.caret_blink.tick();
        let notice_lapsed = self
            .transient_notice
            .as_ref()
            .is_some_and(|(_, until)| *until <= now);
        if notice_lapsed {
            self.transient_notice = None;
        }
        let health_lapsed = {
            let mut health = self
                .state
                .health
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match *health {
                HealthNotice::Reconnected { at } => {
                    let lapsed = at
                        .checked_add(NOTICE_HOLD)
                        .is_some_and(|until| until <= now);
                    if lapsed {
                        *health = HealthNotice::Idle;
                    }
                    lapsed
                }
                _ => false,
            }
        };
        let prompts_retired = !self.pending_permissions.is_empty()
            && !self
                .state
                .agent_running
                .load(std::sync::atomic::Ordering::SeqCst);
        if prompts_retired {
            self.pending_permissions.clear();
        }
        let questions_retired = !self.pending_questions.is_empty()
            && !self
                .state
                .agent_running
                .load(std::sync::atomic::Ordering::SeqCst);
        if questions_retired {
            self.pending_questions.clear();
            self.clamp_question_view();
        }
        self.any_tools_running()
            || claimed
            || dragged
            || blinked
            || notice_lapsed
            || health_lapsed
            || prompts_retired
            || questions_retired
    }

    /// Whether a graduation is waiting to land.
    ///
    /// A poisoned lock recovers — the same policy the drain applies
    /// — so a waiting event still forces its frame after another
    /// thread's panic.
    fn graduation_waiting(&self) -> bool {
        let graduations = self
            .state
            .graduations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        !graduations.is_empty()
    }

    /// Split the frame's rows: the constraints top to bottom, and the
    /// reservation a pending ask holds out of the conversation.
    ///
    /// Every optional section — the plan, the waiting prompts, the
    /// ask's reserved rows, the place row — takes only the rows it
    /// paints, and each stands above the furniture below it in the
    /// order the frame reads: the plan and the waiting prompts ride
    /// above the ask's sheet, so a tall ask meets their bottom edge
    /// rather than painting through them.
    fn frame_constraints(&self, layout: FrameLayout) -> (Vec<Constraint>, u16) {
        let FrameLayout {
            area,
            composer_height,
            strip_position,
            question_page,
            sections,
        } = layout;
        let strip_at_top = matches!(strip_position, StripPosition::Top);
        let queue_shown = sections.stands(SECTION_QUEUE);
        let todo_shown = sections.stands(SECTION_TODO);
        let place_shown = sections.stands(SECTION_PLACE);
        let ruled = self.theme.ui.composer_border.is_some();
        let question_reserved = if let Some(page) = question_page {
            let sheet =
                self.question_sheet_height(area.width, self.question_sheet_area(area).height, page);
            sheet
                .saturating_sub(composer_height)
                .saturating_sub(2)
                .saturating_sub(STATUS_BAR_AIR)
                .saturating_sub(u16::from(place_shown))
        } else {
            0
        };
        let mut constraints = Vec::with_capacity(8);
        if strip_at_top {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(0));
        if todo_shown {
            constraints.push(Constraint::Length(self.todo_section_height(area.width)));
        }
        if queue_shown {
            constraints.push(Constraint::Length(self.queue_section_height(area.width)));
        }
        if question_reserved > 0 {
            constraints.push(Constraint::Length(question_reserved));
        }
        constraints.push(Constraint::Length(1));
        // A ruled theme stands its caption above the top rule rather
        // than between the rules: the caption rides the spacer's own
        // slot and the spacer takes the next row down, so the box the
        // rules draw never encloses the directory's name. Unruled
        // themes keep the caption tight against the composer's paint.
        if ruled && place_shown {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(1));
        if !ruled && place_shown {
            constraints.push(Constraint::Length(1));
        }
        constraints.extend([
            Constraint::Length(composer_height),
            Constraint::Length(STATUS_BAR_ROWS),
        ]);
        (constraints, question_reserved)
    }

    /// Render one frame of the layout.
    ///
    /// Conversation fills the space above a blank spacer row that
    /// separates it from the input field; the place row — the
    /// working directory, once the host names one — sits between
    /// that spacer and the composer, and the three-row footer closes
    /// the frame at the bottom — each pane renders through its own
    /// method. The input field grows with its buffer up to its
    /// text-row cap and then holds: the conversation pane reflows
    /// while the composer grows and holds still once it has.
    pub fn render(&mut self, frame: &mut Frame) {
        self.settle_handed();
        self.settle_todo_expansion();
        self.drain_shared_state();
        self.sync_queue_view();
        let area = frame.area();
        let wrap_width = area
            .width
            .max(1)
            .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2));
        let (rows, caret) = self.composer_wrap(wrap_width);
        let text_rows = u16::try_from(rows.len())
            .unwrap_or(1)
            .clamp(1, INPUT_TEXT_ROWS);
        let composer_height = text_rows
            .saturating_add(INPUT_VERTICAL_PADDING.saturating_mul(2))
            .saturating_add(1);
        let strip_at_top = matches!(self.strip_position, StripPosition::Top);
        let question_page = self
            .pending_questions
            .len()
            .checked_sub(1)
            .filter(|_| self.pending_permissions.is_empty());
        let queue_shown = self.queued_display_len() > 0;
        // The waiting section stands whenever anything waits, not only
        // under a run: prompts a resume owes wait with no run up, and
        // hiding them behind the run gate would put them back in the
        // invisible backlog the seeding exists to clear. The plan
        // section below makes the same argument for its own rows.
        // The plan stands between the conversation and everything else
        // while any task does — no run gate: a list with work left is
        // worth keeping in view between runs too, carried across claims
        // exactly as the runner carries its own store, and a finished
        // list leaves at the claim that follows it — the same edge the
        // runner clears the carried store at — so the section never
        // ghosts done work across runs and never drops work the
        // session still owes. Ctrl+T walks its postures — capped, full,
        // hidden — a view preference, not a mode: it owns no keys and
        // the plan keeps updating underneath either way. Hidden keeps
        // one row of itself standing: the folded marker names the
        // count and the chord, so a plan can be out of sight without
        // being out of mind.
        let todo_shown = !self.latest_todos.is_empty();
        let place_shown = self.working_dir.is_some();
        let (constraints, question_reserved) = self.frame_constraints(FrameLayout {
            area,
            composer_height,
            strip_position: self.strip_position,
            question_page,
            sections: Sections::EMPTY
                .add_todo(todo_shown)
                .add_queue(queue_shown)
                .add_place(place_shown),
        });
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let base = usize::from(strip_at_top);
        let todo_at = usize::from(todo_shown);
        let furniture = base
            .saturating_add(todo_at)
            .saturating_add(usize::from(question_reserved > 0))
            .saturating_add(usize::from(queue_shown));
        let place_at = usize::from(place_shown);
        let fallback = area;
        self.render_conversation(frame, pane(&chunks, base, fallback));
        if todo_shown {
            self.render_todo_section(frame, pane(&chunks, base.saturating_add(1), fallback));
        }
        if queue_shown {
            self.render_queue(
                frame,
                pane(
                    &chunks,
                    base.saturating_add(1).saturating_add(todo_at),
                    fallback,
                ),
            );
        }
        self.render_notice(frame, pane(&chunks, furniture.saturating_add(1), fallback));
        if self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.run_anchor.get_or_insert_with(Instant::now);
        } else {
            self.run_anchor = None;
        }
        // A ruled theme seats its caption above the top rule (the
        // spacer's own slot rides one row higher than usual); an
        // unruled one keeps it tight against the composer's paint.
        let caption_above_rule = self.theme.ui.composer_border.is_some() && place_shown;
        let spacer_at = furniture
            .saturating_add(2)
            .saturating_add(usize::from(caption_above_rule));
        let place_row_at = if caption_above_rule {
            furniture.saturating_add(2)
        } else {
            furniture.saturating_add(3)
        };
        let strip_row = if strip_at_top { 0 } else { spacer_at };
        let strip_drew = self.render_pulse_strip(frame, pane(&chunks, strip_row, fallback));
        let strip_owns_spacer = strip_drew && !strip_at_top;
        if place_shown {
            self.render_place_row(frame, pane(&chunks, place_row_at, fallback));
        }
        if let Some(color) = self.theme.ui.composer_border {
            let spacer = pane(&chunks, spacer_at, fallback);
            let composer = pane(
                &chunks,
                furniture.saturating_add(3).saturating_add(place_at),
                fallback,
            );
            let rule = Style::default()
                .fg(color)
                .add_modifier(ratatui::style::Modifier::UNDERLINED);
            if spacer.height > 0 && !strip_owns_spacer {
                frame.buffer_mut().set_style(spacer, rule);
            }
            // The bottom rule rides the composer's padding row, one
            // above the identity row: the model and mode hang below
            // the rule — outside the box — rather than sitting on the
            // underline, which read as the text crossing its own
            // border. A starved composer keeps the rule on its last
            // row rather than underlining a row it does not own.
            if composer.height > 0 {
                let rule_row = if composer.height >= 2 {
                    composer.bottom().saturating_sub(2)
                } else {
                    composer.bottom().saturating_sub(1)
                };
                let bottom = Rect {
                    y: rule_row,
                    height: 1,
                    ..composer
                };
                frame.buffer_mut().set_style(bottom, rule);
            }
        }
        let input_area = pane(
            &chunks,
            furniture.saturating_add(3).saturating_add(place_at),
            fallback,
        );
        self.input_wrap_width = wrap_width;
        self.render_input(frame, input_area, &rows, caret);
        self.render_status_bar(
            frame,
            pane(
                &chunks,
                furniture.saturating_add(4).saturating_add(place_at),
                fallback,
            ),
        );
        if let Some((prompt, more)) = self.pending_permissions.front().map(|request| {
            (
                request.prompt.clone(),
                self.pending_permissions.len().saturating_sub(1),
            )
        }) {
            self.render_permission_overlay(frame, area, &prompt, more);
            return;
        }
        if self.pending_permissions.is_empty() && !self.pending_questions.is_empty() {
            self.render_question_overlay(
                frame,
                area,
                self.pending_questions.len().saturating_sub(1),
            );
        }
    }

    /// The rows the pinned plan section holds at this width.
    ///
    /// Counted off the same [`todo_view::section_lines`] the section
    /// renders, so the rows the layout reserves are the rows the
    /// frame sees — no blank beneath the plan, no row it cannot pay
    /// for. The folded posture is one row: the marker
    /// [`todo_view::folded_hint`] leaves, cheaper than the list and
    /// louder than nothing.
    fn todo_section_height(&self, width: u16) -> u16 {
        if self.plan_view == PlanView::Hidden {
            // The air above the badge row.
            return 2;
        }
        let colors = todo_view::TodoColors {
            dim: self.theme.ui.dim,
            accent: self.theme.ui.primary,
            moved: self.theme.ui.secondary,
            badge_fill: self.theme.ui.secondary,
            badge_letters: self.theme.ui.background,
        };
        let inner = width
            .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
            .max(1);
        let body_cap = match self.plan_view {
            PlanView::Full => usize::MAX,
            _ => TODO_SECTION_BODY_ROWS,
        };
        let chord_hint = (self.plan_view != PlanView::Full).then_some("ctrl+t");
        u16::try_from(
            todo_view::section_lines(
                &self.latest_todos,
                &self.todo_section_changes,
                &colors,
                inner,
                body_cap,
                chord_hint,
            )
            .len(),
        )
        .unwrap_or(0)
    }

    /// Render the pinned plan section: the run's current tasks,
    /// standing over the furniture while any do.
    ///
    /// Lines of text, not a box — the same register the conversation's
    /// todo blocks and the notice row use — inset from the frame's
    /// edges, headed by the done fraction, completed rows uniform and
    /// retired, moved rows still standing in the accent, and an
    /// ellipsis naming whatever the cap cut. Read-only by design: the
    /// tasks are the agent's bookkeeping, the model owns the list, and
    /// the section asks for no keys at all — it is furniture, not a
    /// sheet.
    fn render_todo_section(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let colors = todo_view::TodoColors {
            dim: self.theme.ui.dim,
            accent: self.theme.ui.primary,
            moved: self.theme.ui.secondary,
            badge_fill: self.theme.ui.secondary,
            badge_letters: self.theme.ui.background,
        };
        if self.plan_view == PlanView::Hidden {
            // The folded stand-in: air, then the badge row — the count
            // and the chord that opens the list again. The plan is out
            // of sight, not out of mind.
            frame.render_widget(
                Paragraph::new(todo_view::folded_lines(&self.latest_todos, &colors)),
                Rect {
                    x: area
                        .x
                        .saturating_add(TODO_SECTION_INSET)
                        .min(area.right().saturating_sub(1)),
                    width: area
                        .width
                        .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
                        .max(1),
                    ..area
                },
            );
            return;
        }
        let body_cap = match self.plan_view {
            PlanView::Full => usize::MAX,
            _ => TODO_SECTION_BODY_ROWS,
        };
        let chord_hint = (self.plan_view != PlanView::Full).then_some("ctrl+t");
        let lines = todo_view::section_lines(
            &self.latest_todos,
            &self.todo_section_changes,
            &colors,
            area.width
                .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
                .max(1),
            body_cap,
            chord_hint,
        );
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                x: area
                    .x
                    .saturating_add(TODO_SECTION_INSET)
                    .min(area.right().saturating_sub(1)),
                width: area
                    .width
                    .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
                    .max(1),
                ..area
            },
        );
    }

    /// Render the pending question as a sheet over the prompt box.
    ///
    /// The same anchored sheet the permission ask uses, carrying the
    /// question's optional header chip, its text, its option rows (the
    /// cursor row marked `›`, a marked row also `•`), each option's
    /// dimmed description, the key hints — and, after the model's
    /// options, the compose row: the escape the option list never
    /// offered, where Enter or Space opens a draft the user types freely
    /// and the next Enter sends as the answer. While that draft is open
    /// the terminal cursor rides its caret, so the cell the next
    /// character lands in is visible; the composer's own caret is hidden
    /// under a question anyway, so nothing competes for it. Long question
    /// text wraps at the box's inner width; option labels and
    /// descriptions clamp to one row each, ellipsized — the pickable
    /// surface stays scannable whatever the model asked. The frame
    /// reserves the sheet's height above the conversation (see
    /// [`TuiApp::render`]), so nothing the run said hides behind the
    /// box. A terminal shorter than the sheet does not clip it: the same
    /// walk chooses the rows that fit, keeping the draft row and the hints
    /// and windowing the options around the cursor.
    fn render_question_overlay(&self, frame: &mut Frame, area: Rect, more: usize) {
        let Some(entry) = self.shown_question() else {
            return;
        };
        let wrap_at = usize::from(area.width.saturating_sub(4));
        let sheet_area = self.question_sheet_area(area);
        let budget = usize::from(sheet_area.height.saturating_sub(2));
        let sheet = self.question_sheet(entry, wrap_at, more, budget);
        let lines = sheet.lines;
        let height = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(sheet_area.height);
        let overlay = bottom_sheet_rect(height, sheet_area);
        if overlay.width == 0 || overlay.height == 0 {
            return;
        }
        let accent = Style::default().fg(self.theme.ui.primary);
        let title = if more == 0 {
            " Question ".to_string()
        } else {
            format!(
                " Question {} of {} ",
                self.question_view.saturating_add(1),
                more.saturating_add(1)
            )
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(accent)
            .title(Span::styled(title, accent));
        frame.render_widget(Clear, overlay);
        frame.render_widget(Paragraph::new(lines).block(block), overlay);
        if let Some((row, column)) = sheet.caret
            && row.saturating_add(1) < usize::from(overlay.height)
        {
            let caret_x = usize::from(overlay.x)
                .saturating_add(1)
                .saturating_add(COMPOSE_PREFIX.width())
                .saturating_add(column)
                .min(usize::from(overlay.right()).saturating_sub(1));
            let caret_y = usize::from(overlay.y)
                .saturating_add(1)
                .saturating_add(row)
                .min(usize::from(overlay.bottom()).saturating_sub(1));
            frame.set_cursor_position((
                u16::try_from(caret_x).unwrap_or(0),
                u16::try_from(caret_y).unwrap_or(0),
            ));
        }
    }

    /// The question sheet's rows and the caret riding them, laid out for
    /// the width the box has and the height the terminal leaves it.
    ///
    /// Every consumer of the sheet's shape — the render and the height
    /// the frame reserves for it — builds through this one walk, so the
    /// box the user sees and the rows the layout holds aside can never
    /// drift apart.
    ///
    /// `budget` is the rows the sheet may occupy inside its borders, and
    /// both consumers hand it the same number. When the terminal cannot
    /// hold everything, the rows go where the answer is made: the draft
    /// row and the key hints keep theirs whatever happens, because a
    /// sheet that cut the row being typed would leave the user writing
    /// blind, and it is the option list that gives way — windowed around
    /// the row the cursor stands on, so the walk stays on screen and the
    /// arrows walk the window alongside it. See [`fit_question_sheet`].
    fn question_sheet(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
        more: usize,
        budget: usize,
    ) -> QuestionSheet {
        let head = self.question_sheet_head(entry, wrap_at);
        let options = self.question_sheet_options(entry, wrap_at);
        let (compose, caret_in_compose) = self.question_sheet_compose(entry, wrap_at);
        let tail = self.question_sheet_tail(entry, wrap_at, more);
        let cursor_option = entry.ui.cursor.min(entry.prompt.options.len());
        let (lines, compose_start) =
            fit_question_sheet(head, &options, compose, tail, cursor_option, budget);
        let caret = caret_in_compose
            .map(|(row, column)| (compose_start.saturating_add(row), column))
            .filter(|(row, _)| *row < lines.len());
        QuestionSheet { lines, caret }
    }

    /// The sheet's opening block: the header chip, the question's own
    /// text, and the row of air over the options.
    ///
    /// The question wraps at the box's inner width and clamps to
    /// [`MAX_OVERLAY_ROWS`] rows, the last ellipsized, so a paragraph of a
    /// question cannot push the options off a short terminal on its own.
    /// It is the one text block the sheet never re-wraps: what was asked
    /// is what the user has to read to answer.
    fn question_sheet_head(&self, entry: &PendingQuestion, wrap_at: usize) -> Vec<Line<'static>> {
        let question = &entry.prompt;
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(header) = &question.header {
            lines.push(Line::styled(
                format!("[{header}]"),
                Style::default().fg(self.theme.ui.secondary),
            ));
        }
        lines.extend(
            wrap_prompt_rows(&question.question, wrap_at, MAX_OVERLAY_ROWS)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.foreground))),
        );
        lines.push(Line::default());
        lines
    }

    /// The sheet's option block: one entry per option, holding its label
    /// row and, where it has one, its description row beneath it.
    ///
    /// Grouped rather than flattened because a short terminal drops
    /// options whole: a description left standing apart from the label it
    /// describes reads as another option's. The grouping is also what lets
    /// the sheet keep the cursor's own option visible — the walk that
    /// chooses which options survive is anchored on it.
    ///
    /// Labels and descriptions clamp to one row each, ellipsized, so the
    /// pickable surface stays scannable whatever the model asked. The
    /// indent is applied *outside* `wrap_prompt_rows`: that wrapper splits
    /// on spaces, so a leading run of them is dropped as empty words and
    /// the row lands flush-left instead.
    fn question_sheet_options(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
    ) -> Vec<(usize, Vec<Line<'static>>)> {
        let question = &entry.prompt;
        let ui = &entry.ui;
        let draft_supersedes =
            ui.composing && !question.multi_select && !ui.draft.trim().is_empty();
        question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let marker = if index == ui.cursor && !ui.composing {
                    "\u{203a} "
                } else {
                    "  "
                };
                let checked = if ui.toggled.contains(&index) && !draft_supersedes {
                    "\u{2022} "
                } else {
                    "  "
                };
                let label =
                    wrap_prompt_rows(&format!("{marker}{checked}{}", option.label), wrap_at, 1)
                        .first()
                        .cloned()
                        .unwrap_or_default();
                let style = if index == ui.cursor {
                    Style::default().fg(self.theme.ui.primary)
                } else {
                    Style::default().fg(self.theme.ui.foreground)
                };
                let mut rows = vec![Line::styled(label, style)];
                if let Some(description) = &option.description {
                    let detail = wrap_prompt_rows(
                        description,
                        wrap_at.saturating_sub(DETAIL_INDENT.width()),
                        1,
                    )
                    .first()
                    .cloned()
                    .unwrap_or_default();
                    rows.push(Line::styled(
                        format!("{DETAIL_INDENT}{detail}"),
                        Style::default().fg(self.theme.ui.secondary),
                    ));
                }
                (index, rows)
            })
            .collect()
    }

    /// The sheet's compose block: the row the answer is written on, with
    /// the draft laid across as many rows as the box's width asks for, and
    /// the caret's place in that layout while the draft owns the keys.
    ///
    /// The draft wraps with the composer's own wrap rather than a second
    /// one of the sheet's devising, so the two editable surfaces in the
    /// app break text at the same points and an answer longer than the box
    /// — a path, a UUID, a sentence — continues on a row the user can see
    /// instead of running past the border into nothing. The caret's row
    /// comes out of that wrap too, which is the only place that knows
    /// which row the caret's character landed on.
    ///
    /// Outside the draft the block is the row at rest plus the subtext
    /// saying what it is for: the invitation, or the words already typed,
    /// which Esc leaves in place precisely because stepping out of the
    /// mode is not a request to lose them.
    fn question_sheet_compose(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let ui = &entry.ui;
        let mut lines: Vec<Line<'static>> = vec![Line::default()];
        if ui.composing {
            let draft_width = u16::try_from(wrap_at.saturating_sub(COMPOSE_PREFIX.width()).max(1))
                .unwrap_or(u16::MAX);
            let caret_byte = byte_index(&ui.draft, ui.caret);
            let (rows, caret) =
                crate::input::wrap_rows_and_caret(&ui.draft, caret_byte, draft_width);
            let first = lines.len();
            let continuation = " ".repeat(COMPOSE_PREFIX.width());
            for (index, row) in rows.iter().enumerate() {
                let prefix = if index == 0 {
                    COMPOSE_PREFIX
                } else {
                    continuation.as_str()
                };
                lines.push(Line::styled(
                    format!("{prefix}{row}"),
                    Style::default().fg(self.theme.ui.primary),
                ));
            }
            let (caret_row, caret_column) = caret.unwrap_or((0, 0));
            return (
                lines,
                Some((
                    first.saturating_add(usize::from(caret_row)),
                    usize::from(caret_column),
                )),
            );
        }
        let free_row = entry.prompt.options.len();
        let marker = if ui.cursor == free_row {
            "\u{203a} "
        } else {
            "  "
        };
        let checked = if ui.toggled.contains(&free_row) {
            "\u{2022} "
        } else {
            "  "
        };
        let has_draft = !ui.draft.is_empty();
        let body = if has_draft {
            ui.draft.as_str()
        } else {
            "Type your answer"
        };
        lines.push(Line::styled(
            format!("{marker}{checked}\u{270e} {body}"),
            if ui.cursor == free_row {
                Style::default().fg(self.theme.ui.primary)
            } else {
                Style::default().fg(self.theme.ui.foreground)
            },
        ));
        let subtext = if has_draft {
            "enter opens it again \u{b7} enter there sends"
        } else {
            "a reply the options did not offer"
        };
        lines.push(Line::styled(
            format!("{DETAIL_INDENT}{subtext}"),
            Style::default().fg(self.theme.ui.secondary),
        ));
        (lines, None)
    }

    /// The sheet's closing block: the row of air under the compose row and
    /// the keys explained.
    ///
    /// The hints never give up their rows to the terminal, which is the
    /// other half of why they lead with the keys that end the exchange:
    /// what a short terminal cuts is the tail of the list, not the list.
    fn question_sheet_tail(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
        more: usize,
    ) -> Vec<Line<'static>> {
        let ui = &entry.ui;
        let mut lines: Vec<Line<'static>> = vec![Line::default()];
        let toggle = if entry.prompt.multi_select {
            "[space] toggle"
        } else {
            "[space] pick"
        };
        let parts: Vec<&str> = if ui.composing {
            let enter = if ui.draft.trim().is_empty() {
                "[enter] needs a written answer"
            } else {
                "[enter] send"
            };
            vec![
                "[esc] back",
                enter,
                "[home/end] ends",
                "[\u{2190}\u{2192}] caret",
            ]
        } else if more == 0 {
            vec![
                "[esc] dismiss",
                "[enter] confirm",
                "[\u{2191}\u{2193}] move",
                toggle,
            ]
        } else {
            vec![
                "[esc] dismiss one",
                "[enter] confirm",
                "[\u{2191}\u{2193}] move",
                "[\u{2190}\u{2192}] question",
                toggle,
            ]
        };
        let hints = parts.join(" \u{b7} ");
        lines.extend(
            wrap_prompt_rows(&hints, wrap_at, MAX_OVERLAY_ROWS)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.secondary))),
        );
        lines
    }

    /// The rows the pending question's sheet occupies, borders included.
    ///
    /// The frame reserves this many rows above its bottom furniture —
    /// whatever of the sheet the furniture does not already cover —
    /// so the conversation's rows end above the box instead of
    /// rendering behind it. A terminal shorter than the sheet fits the
    /// sheet to it, rows chosen as [`fit_question_sheet`] describes,
    /// rather than hanging rows the frame has nowhere to draw.
    ///
    /// `more` is the count the sheet itself renders beside the hints.
    /// Passing anything else desynchronizes the two by exactly the rows
    /// the queue segment adds once the hints wrap — the box grows past
    /// the rows held aside and covers the conversation's tail it was
    /// reserved to uncover.
    #[must_use]
    fn question_sheet_height(&self, width: u16, sheet_area_height: u16, more: usize) -> u16 {
        let Some(entry) = self.shown_question() else {
            return 0;
        };
        let wrap_at = usize::from(width.max(4).saturating_sub(4));
        let budget = usize::from(sheet_area_height.saturating_sub(2));
        let sheet = self.question_sheet(entry, wrap_at, more, budget);
        u16::try_from(sheet.lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(sheet_area_height)
    }

    /// The rows a question sheet may take over the frame.
    ///
    /// The status bar's two content rows stay its own — the session
    /// line reads while an ask waits — while the footer's air is
    /// margin no pane paints, which the sheet may stand on: a deeper
    /// composer margin costs the sheet no rows it did not already
    /// spend. So does the strip's row when the strip is configured to
    /// the top of the terminal: the sheet cannot occupy a row the
    /// layout has already promised elsewhere, and a reservation that
    /// counted it too asked the solver for one row more than the frame
    /// had, leaving it to squeeze an unspecified pane to pay. Both the
    /// reservation and the box derive their height from here, so they
    /// cannot disagree about how much room there is.
    fn question_sheet_area(&self, area: Rect) -> Rect {
        let strip_rows = u16::from(matches!(self.strip_position, StripPosition::Top));
        Rect {
            height: area
                .height
                .saturating_sub(STATUS_BAR_ROWS - STATUS_BAR_AIR)
                .saturating_sub(strip_rows),
            ..area
        }
    }

    /// Render the pending permission ask as a sheet over the prompt
    /// box.
    ///
    /// Draws where the user's attention already is: anchored at the
    /// bottom, directly above the status bar, covering the notice row,
    /// the spacer, and the composer — the box the user would type in
    /// is replaced by the question. The conversation above keeps
    /// rendering behind it, so what the run is doing stays visible
    /// while the ask waits. The prompt and the hints are each
    /// pre-wrapped at the box's inner width (a block at most
    /// `MAX_OVERLAY_ROWS` rows, the last ellipsized), so no rendered
    /// line can re-flow inside the border and the hints always fit,
    /// whatever the tool name's length or the terminal's width; only
    /// a terminal shorter than the box clips, at the box's top. The
    /// strings arrive cloned so this borrows nothing from the queue.
    fn render_permission_overlay(&self, frame: &mut Frame, area: Rect, prompt: &str, more: usize) {
        let hints = if more == 0 {
            "[y] allow · [n] deny · ctrl+c cancels (a draft clears first)".to_string()
        } else {
            format!("[y] allow · [n] deny · ctrl+c cancels · {more} more waiting")
        };
        let wrap_at = usize::from(area.width.saturating_sub(2));
        let mut lines: Vec<Line<'_>> = wrap_prompt_rows(prompt, wrap_at, MAX_OVERLAY_ROWS)
            .into_iter()
            .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.foreground)))
            .collect();
        lines.push(Line::default());
        lines.extend(
            wrap_prompt_rows(&hints, wrap_at, MAX_OVERLAY_ROWS)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.secondary))),
        );
        let sheet_area = Rect {
            height: area.height.saturating_sub(STATUS_BAR_ROWS - STATUS_BAR_AIR),
            ..area
        };
        let height = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(sheet_area.height);
        let overlay = bottom_sheet_rect(height, sheet_area);
        if overlay.width == 0 || overlay.height == 0 {
            return;
        }
        let accent = Style::default().fg(self.theme.ui.primary);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(accent)
            .title(Span::styled(" Permission required ", accent));
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(self.theme.ui.surface)),
            overlay,
        );
    }

    /// Render the conversation pane: the settled conversation, the
    /// streaming region below it, any running tools, and the
    /// scrollbar in a reserved right gutter.
    ///
    /// The gutter is one column off the pane's right edge, reserved
    /// whether or not the document scrolls, so wrapped text never
    /// collides with the scrollbar and the wrap width stays stable
    /// as the document crosses the scrollability threshold. The
    /// text also sits inside the pane's top and right edges — one
    /// breathing row below the top, blank columns short of the
    /// gutter — so the transcript never touches its container's
    /// edges; the scrollbar track shares the inset, belonging to
    /// the padded content window it measures. A pinned view
    /// re-anchors to the newest line; a detached view holds its
    /// place while content streams in below it or collapses away.
    /// Invalidate the bake whole, keeping the width.
    ///
    /// For the shapes that change how already-baked messages render —
    /// a verbosity switch re-shapes every tool body, an expansion
    /// click re-renders one block — the bake cannot stand: it froze
    /// the old shape. Rare by nature (a key press, a click), so the
    /// full re-render they cost is the price they always paid.
    fn rebake_whole(&mut self) {
        self.conversation_bake.clear();
        self.conversation_bake_meta = BakeMeta::default();
        self.conversation_baked_through = 0;
        self.conversation_baked_prev_user = None;
    }

    /// Bake the conversation cache incrementally.
    ///
    /// Messages are immutable once a newer one follows — only the
    /// last is ever patched, a run's duration landing on it — so the
    /// cache bakes each message once and a graduation re-renders only
    /// the tail. A full-transcript rebuild cost seconds on
    /// markdown-heavy sessions, freezing the frame — spinner included
    /// — on every tool result that landed. Width changes and
    /// truncations below the bake are the two shapes that invalidate
    /// it. The tail renders alone, so its row indexes are
    /// tail-relative; the frame's merged view products are rebuilt
    /// with the tail rebased by the bake's line count, and the bake's
    /// own metadata lives in [`Self::conversation_bake_meta`].
    fn bake_conversation_cache(&mut self, text_area: Rect, width: u16) {
        if self.conversation_cache_width != width
            || self.conversation.len() < self.conversation_baked_through
        {
            self.conversation_cache.clear();
            self.conversation_bake.clear();
            self.conversation_bake_meta = BakeMeta::default();
            self.conversation_baked_through = 0;
            self.conversation_baked_prev_user = None;
            self.conversation_cache_width = width;
        }
        // Every rebuild forfeits the selection, as it always did:
        // line indexes shift under it.
        self.selection = None;
        self.drag_position = None;
        let bake_end = self.conversation.len().saturating_sub(1);
        if bake_end > self.conversation_baked_through {
            let mut prev_user = self.conversation_baked_prev_user;
            let baked_start = self.conversation_baked_through;
            let mut baked = BakeOutputs {
                lines: std::mem::take(&mut self.conversation_bake),
                ..BakeOutputs::default()
            };
            self.conversation_lines(text_area, baked_start, bake_end, &mut prev_user, &mut baked);
            self.conversation_baked_prev_user = prev_user;
            self.conversation_baked_through = bake_end;
            self.conversation_bake = baked.lines;
            self.conversation_bake_meta
                .tool_summary_lines
                .extend(baked.tool_summary_lines);
            self.conversation_bake_meta
                .thinking_summary_lines
                .extend(baked.thinking_summary_lines);
            self.conversation_bake_meta
                .compaction_summary_lines
                .extend(baked.compaction_summary_lines);
            self.conversation_bake_meta
                .user_turn_ranges
                .extend(baked.user_turn_ranges);
            self.conversation_bake_meta
                .todo_block_ranges
                .extend(baked.todo_block_ranges);
        }
        let mut tail = BakeOutputs::default();
        let mut tail_prev = self.conversation_baked_prev_user;
        let tail_start = self.conversation.len().saturating_sub(1);
        self.conversation_lines(
            text_area,
            tail_start,
            self.conversation.len(),
            &mut tail_prev,
            &mut tail,
        );
        let mut cache = self.conversation_bake.clone();
        cache.extend(tail.lines);
        self.conversation_cache = cache;
        let tail_base = self.conversation_bake.len();
        self.tool_summary_lines = self.conversation_bake_meta.tool_summary_lines.clone();
        for (index, id) in tail.tool_summary_lines {
            self.tool_summary_lines
                .insert(index.saturating_add(tail_base), id);
        }
        self.thinking_summary_lines = self.conversation_bake_meta.thinking_summary_lines.clone();
        for (index, id) in tail.thinking_summary_lines {
            self.thinking_summary_lines
                .insert(index.saturating_add(tail_base), id);
        }
        self.compaction_summary_lines =
            self.conversation_bake_meta.compaction_summary_lines.clone();
        for (index, id) in tail.compaction_summary_lines {
            self.compaction_summary_lines
                .insert(index.saturating_add(tail_base), id);
        }
        self.user_turn_ranges = self.conversation_bake_meta.user_turn_ranges.clone();
        self.user_turn_ranges
            .extend(tail.user_turn_ranges.iter().map(|(start, end, mark)| {
                (
                    start.saturating_add(tail_base),
                    end.saturating_add(tail_base),
                    mark.saturating_add(tail_base),
                )
            }));
        self.todo_block_ranges = self.conversation_bake_meta.todo_block_ranges.clone();
        self.todo_block_ranges
            .extend(tail.todo_block_ranges.iter().map(|range| {
                range.start.saturating_add(tail_base)..range.end.saturating_add(tail_base)
            }));
        self.conversation_cache_generation = self.conversation_generation;
    }

    /// Render the conversation pane: the transcript's windowed view
    /// and everything live that rides it.
    ///
    /// The pane is the layered stack the reader scrolls — the baked
    /// conversation cache, the speaker-change seam, the live lanes in
    /// conversation order (thinking rows, the streaming reply's
    /// frozen and live regions, compaction notices), the running tool
    /// rows, the turn's ticking timer footer, and — just above that
    /// timer, in the spot they will land in — a steer waiting for the
    /// model to read it, pinned as a user row — windowed to the pane's
    /// height around
    /// the scroll position, so a keystroke's cost tracks the window,
    /// never the session. The settled turns' chrome (their raised
    /// boxes and chevrons) paints from the cached turn ranges over
    /// the same window; the selection reverses in place; the
    /// scrollbar rides the gutter when the stack outgrows the pane.
    /// The view state this records is the click-mapping every later
    /// pointer event trusts.
    fn render_conversation(&mut self, frame: &mut Frame, area: Rect) {
        let padded = Rect {
            y: area.y.saturating_add(CONVERSATION_TOP_INSET),
            height: area.height.saturating_sub(CONVERSATION_TOP_INSET),
            ..area
        };
        let (pane, scrollbar_area) = split_scrollbar_gutter(padded);
        let mark_gutter = (pane.width
            >= CONVERSATION_MARK_MIN_TEXT
                .saturating_add(CONVERSATION_MARK_GUTTER)
                .saturating_add(CONVERSATION_RIGHT_INSET)
                .saturating_add(CONVERSATION_BOX_MARGIN))
        .then_some(Rect {
            width: CONVERSATION_MARK_GUTTER,
            ..pane
        });
        let text_area = mark_gutter.as_ref().map_or(pane, |gutter| Rect {
            x: gutter.right(),
            width: pane.width.saturating_sub(CONVERSATION_MARK_GUTTER),
            ..pane
        });
        let text_area = Rect {
            width: text_area
                .width
                .saturating_sub(CONVERSATION_RIGHT_INSET)
                .saturating_sub(CONVERSATION_BOX_MARGIN),
            ..text_area
        };
        let conversation_height = text_area.height as usize;
        let width = text_area.width.max(1);
        if self.conversation_cache_generation != self.conversation_generation
            || self.conversation_cache_width != width
        {
            self.bake_conversation_cache(text_area, width);
        }

        let conversation_len = self.conversation_cache.len();
        if self.refresh_streaming_region(width)
            && self
                .selection
                .as_ref()
                .is_some_and(|selection| ordered(selection).1.line >= conversation_len)
        {
            self.selection = None;
            self.drag_position = None;
        }
        let (tools, running_rows) = self.active_tool_lines(width);
        let thinking_rows = self.live_thinking_lines(width);
        let compaction_rows = self.live_compaction_lines();
        let live_open = !thinking_rows.0.is_empty()
            || !self.frozen_lines.is_empty()
            || !self.live_lines.is_empty()
            || !compaction_rows.is_empty()
            || !tools.is_empty();
        let seam: Vec<Line<'static>> = if !self.conversation_cache.is_empty() && live_open {
            vec![Line::from("")]
        } else {
            Vec::new()
        };
        let selectable_lines = self
            .conversation_cache
            .len()
            .saturating_add(self.frozen_lines.len())
            .saturating_add(self.live_lines.len());
        self.thinking_live_rows = thinking_rows.1.map(|row| {
            let start = row
                .saturating_add(self.conversation_cache.len())
                .saturating_add(seam.len());
            start..start.saturating_add(thinking_rows.0.len())
        });
        let pre_tool_offset = self
            .conversation_cache
            .len()
            .saturating_add(seam.len())
            .saturating_add(thinking_rows.0.len())
            .saturating_add(self.frozen_lines.len())
            .saturating_add(self.live_lines.len())
            .saturating_add(compaction_rows.len());
        self.tool_running_lines = running_rows
            .into_iter()
            .map(|(index, call_id)| (index.saturating_add(pre_tool_offset), call_id))
            .collect();
        let run_timer = self.live_run_timer();
        let pending_steer_rows = self.pending_steer_lines(width);
        let lane_end = selectable_lines
            .saturating_add(seam.len())
            .saturating_add(thinking_rows.0.len())
            .saturating_add(compaction_rows.len())
            .saturating_add(tools.len());
        let pinned_steer = (!pending_steer_rows.is_empty())
            .then(|| (lane_end, lane_end.saturating_add(pending_steer_rows.len())));
        let total_lines = lane_end
            .saturating_add(pending_steer_rows.len())
            .saturating_add(run_timer.len());
        self.settle_scroll_offset(total_lines, conversation_height);
        let skip = self.snap_skip_to_whole_todo_blocks(
            total_lines
                .saturating_sub(conversation_height)
                .saturating_sub(self.scroll_offset),
            conversation_height,
        );
        let segments: Vec<&[Line<'_>]> = vec![
            &self.conversation_cache,
            &seam,
            &thinking_rows.0,
            &self.frozen_lines,
            &self.live_lines,
            &compaction_rows,
            &tools,
            &pending_steer_rows,
            &run_timer,
        ];
        let mut visible = visible_window(&segments, skip, conversation_height);
        self.last_view = Some(ViewState {
            area: text_area,
            skip,
            total: total_lines,
            selectable: selectable_lines,
        });
        if let Some(selection) = &self.selection {
            reverse_selection(&mut visible, skip, selection);
        }
        self.paint_user_turn_chrome(
            frame,
            pane,
            mark_gutter,
            skip,
            conversation_height,
            pinned_steer,
        );
        frame.render_widget(Paragraph::new(visible), text_area);
        if let Some(gutter) = scrollbar_area
            && let Some((thumb_pos, thumb_len)) =
                scrollbar_geometry(total_lines, conversation_height, skip)
        {
            render_scrollbar(
                frame,
                gutter,
                thumb_pos,
                thumb_len,
                self.theme.ui.scrollbar_thumb,
                self.theme.ui.scrollbar_track,
            );
        }
    }

    /// Paint the settled user turns' chrome: each turn's raised box
    /// and the chevron at its opening text row.
    ///
    /// The box keeps [`CONVERSATION_BOX_MARGIN`] of the pane's own
    /// background outside itself on both sides and spans the turn's
    /// padding rows and text; the chevron sits a column inside the
    /// box's left edge. Both are painted from the cached turn ranges
    /// against this frame's window, so turns off-screen paint
    /// nothing.
    fn paint_user_turn_chrome(
        &self,
        frame: &mut Frame,
        pane: Rect,
        mark_gutter: Option<Rect>,
        skip: usize,
        conversation_height: usize,
        pinned_steer: Option<(usize, usize)>,
    ) {
        let mut turns = self.user_turn_ranges.clone();
        if let Some((start, end)) = pinned_steer {
            turns.push((start, end, start.saturating_add(1)));
        }
        for &(start, end, _) in &turns {
            let first_visible = start.max(skip);
            let last_visible = end.min(skip.saturating_add(conversation_height));
            if first_visible >= last_visible {
                continue;
            }
            let Some(y) = first_visible
                .checked_sub(skip)
                .and_then(|offset| u16::try_from(offset).ok())
            else {
                continue;
            };
            let Some(h) = last_visible
                .checked_sub(first_visible)
                .and_then(|rows| u16::try_from(rows).ok())
            else {
                continue;
            };
            let block = Rect {
                x: pane.x.saturating_add(CONVERSATION_BOX_MARGIN),
                width: pane
                    .width
                    .saturating_sub(CONVERSATION_BOX_MARGIN.saturating_mul(2)),
                y: pane.y.saturating_add(y),
                height: h,
            };
            frame
                .buffer_mut()
                .set_style(block, Style::default().bg(self.theme.ui.surface));
        }
        let Some(gutter) = mark_gutter else {
            return;
        };
        for &(_, _, mark) in &turns {
            let Some(on_screen) = mark.checked_sub(skip) else {
                continue;
            };
            let Ok(row) = u16::try_from(on_screen) else {
                continue;
            };
            if row < gutter.height {
                frame.buffer_mut()[(
                    gutter
                        .x
                        .saturating_add(CONVERSATION_BOX_MARGIN)
                        .saturating_add(1),
                    gutter.y.saturating_add(row),
                )]
                    .set_symbol("❯")
                    .set_fg(self.theme.ui.primary);
            }
        }
    }

    /// The steers a run accepted but no turn boundary has read, as
    /// user rows pinned just above the live lane's timer.
    ///
    /// Each held steer renders exactly as a settled user turn renders
    /// — a breathing row, wrapped text in the user's own color,
    /// another breathing row — but just above the running turn's
    /// timer, in the spot the boundary that reads it will land it in:
    /// the model has not read the words, so they hold no place in the
    /// conversation yet, and nothing jumps when they graduate. A run
    /// that ends unread hands them to the waiting section instead,
    /// and the pin empties with that return.
    fn pending_steer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let steers = self
            .state
            .pending_steers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut lines = Vec::new();
        for text in steers {
            lines.push(Line::default());
            for segment in text.split('\n') {
                lines.extend(plain_wrapped_lines(
                    segment,
                    usize::from(width.max(1)),
                    self.theme.ui.user_message_fg,
                ));
            }
            lines.push(Line::default());
        }
        lines
    }

    /// The running turn's live elapsed line, when one runs.
    ///
    /// The completed reply prints its total duration as a dim `duration`
    /// line at its foot; the running turn owes the same line while it
    /// runs — the total so far, the same figure format, the same dim
    /// register — named by a quiet `elapsed` word while it still
    /// counts, the `duration` word arriving only when the total is
    /// final. A breathing row of margin stands above the line, so the
    /// ticking figure never touches the streaming text it closes. A
    /// spinner dot leads it in the accent — the same braille orbit,
    /// the same animation beat, as the running tools' own indicators,
    /// so everything in flight pulses together. Empty while nothing
    /// runs: between turns nothing ticks.
    fn live_run_timer(&self) -> Vec<Line<'static>> {
        let started = self
            .state
            .run_started_ms
            .load(std::sync::atomic::Ordering::SeqCst);
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::SeqCst)
            || started == 0
        {
            return Vec::new();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let elapsed = now.saturating_sub(started);
        let frame_index = usize::try_from(
            elapsed
                .checked_div(crate::tool_render::SPINNER_FRAME_MS)
                .unwrap_or(0),
        )
        .unwrap_or(0)
        .checked_rem(SPINNER_FRAMES.len())
        .unwrap_or(0);
        let frame = SPINNER_FRAMES
            .get(frame_index)
            .or_else(|| SPINNER_FRAMES.first())
            .copied()
            .unwrap_or("");
        vec![
            Line::from(Vec::new()),
            Line::from(vec![
                Span::styled(
                    frame.to_string(),
                    Style::default().fg(self.theme.ui.primary),
                ),
                Span::styled(
                    format!(" elapsed {}", format_duration(elapsed)),
                    self.theme.ui.dim,
                ),
            ]),
        ]
    }

    /// Move a scrolled window's cut out of the interior of a todo block.
    ///
    /// The plan badge keeps the list's fraction on screen at every
    /// scroll, so a window that severs an expanded todo block mid-list
    /// shows a count and a set of rows that disagree — the badge says
    /// twelve, the frame carries eleven, and nothing marks the cut.
    /// While the view is scrolled (the bottom pin keeps its
    /// newest-line guarantee untouched), a cut landing inside a block
    /// that fits the viewport moves up to the block's own first row:
    /// the block renders whole or not at all, and the fraction and the
    /// rows can never disagree. A block taller than the viewport stays
    /// scrollable — there is no whole to keep.
    fn snap_skip_to_whole_todo_blocks(&self, skip: usize, viewport: usize) -> usize {
        if self.scroll_offset == 0 {
            return skip;
        }
        self.todo_block_ranges
            .iter()
            .find(|block| skip > block.start && skip < block.end && block.len() <= viewport)
            .map_or(skip, |block| block.start)
    }

    /// Settle the scroll offset for this frame's layout.
    ///
    /// A pinned view sits at the newest line: the offset resets to
    /// zero and growth below the viewport carries it along. A
    /// detached view holds its anchor: the offset is compensated for
    /// the document growing or shrinking since the last frame and
    /// clamped to the scrollable height, so the same lines stay in
    /// view while content streams in below or collapses away.
    fn settle_scroll_offset(&mut self, total_lines: usize, viewport: usize) {
        if self.stick.follows() {
            self.scroll_offset = 0;
        } else {
            let delta = total_lines.abs_diff(self.last_layout_lines);
            if total_lines >= self.last_layout_lines {
                self.scroll_offset = self.scroll_offset.saturating_add(delta);
            } else {
                self.scroll_offset = self.scroll_offset.saturating_sub(delta);
            }
            self.scroll_offset = self.scroll_offset.min(total_lines.saturating_sub(viewport));
        }
        self.last_layout_lines = total_lines;
    }

    /// Retain a graduated call's full detail, evicting at the cap.
    ///
    /// The one insertion path for the detail store: a graduation
    /// with captured content, and the resume seeding of blocks the
    /// file carried. A retried call graduates once per attempt
    /// under the same id, so the queue is requeued rather than
    /// duplicated and the detail ages from its newest attempt; the
    /// oldest entry retires past the cap with its toggle state — and
    /// a retirement that cleared an expanded flag rebakes at once,
    /// so baked rows cannot keep showing a shape no toggle backs.
    fn retain_tool_detail(&mut self, call_id: &str, input_json: &str, output: &str) {
        if call_id.is_empty() || input_json.is_empty() {
            return;
        }
        self.tool_detail_order.retain(|id| id != call_id);
        self.tool_detail_order.push_back(call_id.to_string());
        if self.tool_detail_order.len() > TOOL_DETAIL_CAP
            && let Some(retired) = self.tool_detail_order.pop_front()
        {
            self.tool_details.remove(&retired);
            if self.expanded_tools.remove(&retired) {
                self.conversation_generation = self.conversation_generation.saturating_add(1);
                self.rebake_whole();
            }
        }
        self.tool_details.insert(
            call_id.to_string(),
            ToolDetail {
                input_json: input_json.to_string(),
                output: output.to_string(),
            },
        );
    }

    /// Take what the observer finished since the last frame.
    ///
    /// Finalized replies and completed tool calls alike move into
    /// the conversation as assistant messages; run failures
    /// recorded by the mode driver surface as error messages — so
    /// none of the shared buffers accumulates across frames.
    /// Whenever a reply, a tool completion, a failure, or a turn-end
    /// history lands, the turn-end hook — when installed — receives
    /// the conversation snapshot beside the newest engine history,
    /// the host's save point. A poisoned lock is
    /// recovered — the same policy the observer writes with — so
    /// finalized data still graduates.
    fn drain_shared_state(&mut self) {
        let graduations = take_locked(&self.state.graduations);
        let retry_lines = take_locked(&self.state.retry_lines);
        let now = chrono::Utc::now();
        for line in retry_lines {
            if let Some(index) = self.retry_row_index.get(&line.episode).copied() {
                if let Some(TuiMessage::System { text, .. }) = self.conversation.get_mut(index) {
                    *text = line.text;
                }
            } else {
                self.push_message(TuiMessage::System {
                    text: line.text.clone(),
                    timestamp: now,
                });
                let index = self.conversation.len().saturating_sub(1);
                self.retry_row_index.insert(line.episode, index);
            }
        }
        if !self.state.agent_running.load(Ordering::SeqCst) {
            self.return_guidance();
        }
        let mut turn_ended = graduations
            .iter()
            .any(|graduation| !matches!(graduation, Graduation::Sent { .. }));
        let turn_histories = take_locked(&self.state.turn_histories);
        let machine_history = turn_histories.last().cloned();
        turn_ended |= machine_history.is_some();
        for graduation in graduations {
            match graduation {
                Graduation::Sent { text, steered } => {
                    self.push_message(TuiMessage::User {
                        text,
                        timestamp: now,
                        steered,
                    });
                    if !steered && !self.plan_interrupted {
                        self.retire_finished_plan();
                    }
                    self.plan_interrupted = false;
                }
                Graduation::Reply { text, thinking } => {
                    let mut blocks = Vec::new();
                    blocks.extend(thinking);
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text { text });
                    }
                    self.push_message(TuiMessage::Assistant {
                        blocks,
                        timestamp: now,
                        duration_ms: None,
                    });
                }
                Graduation::RunEnded {
                    duration_ms,
                    success,
                } => {
                    self.plan_interrupted = !success;
                    if let Some(TuiMessage::Assistant {
                        duration_ms: reply_duration,
                        ..
                    }) = self.conversation.last_mut()
                    {
                        *reply_duration = Some(duration_ms);
                    }
                }
                Graduation::Tool(result) => {
                    let mut todo_armed: Option<String> = None;
                    self.retain_tool_detail(
                        &result.call_id,
                        &result.full_input,
                        &result.full_output,
                    );
                    if matches!(result.name.as_str(), "Edit" | "MultiEdit" | "Write")
                        && crate::diff::parse_receipt(&result.full_output).is_some()
                    {
                        self.expanded_tools.insert(result.call_id.clone());
                    }
                    if result.name == "TodoWrite"
                        && !result.is_error
                        && let Some(rows) = crate::todo_view::parse_rendered(&result.full_output)
                            .or_else(|| crate::todo_view::parse_submission(&result.full_input))
                    {
                        let changes = crate::todo_view::status_changes(&rows, &self.latest_todos);
                        self.todo_changes
                            .insert(result.call_id.clone(), changes.clone());
                        self.todo_section_changes = changes;
                        self.latest_todos = rows;
                        self.expanded_tools.insert(result.call_id.clone());
                        todo_armed = Some(result.call_id.clone());
                    }
                    self.push_message(TuiMessage::Assistant {
                        blocks: vec![ContentBlock::Tool {
                            name: result.name,
                            call_id: result.call_id,
                            input_preview: result.input_summary,
                            success: !result.is_error,
                            elapsed_secs: result.duration.as_secs_f64(),
                            output_preview: result.output_preview,
                            retained_input: result.full_input,
                        }],
                        timestamp: now,
                        duration_ms: None,
                    });
                    if let Some(id) = todo_armed {
                        self.auto_expanded_todo = Some(id);
                    }
                }
            }
        }
        let compactions = take_locked(&self.state.compactions);
        let errors = take_locked(&self.state.errors);
        turn_ended |= !errors.is_empty();
        for text in errors {
            self.push_message(TuiMessage::Error {
                text,
                timestamp: now,
            });
        }
        for record in compactions {
            self.push_message(record);
        }
        let verdicts = take_locked(&self.copy_outcomes);
        if let Some(latest) = verdicts.into_iter().max_by_key(|verdict| verdict.attempt)
            && latest.attempt >= self.posted_copy_attempt
        {
            self.posted_copy_attempt = latest.attempt;
            let wording = copy_verdict_notice(latest);
            if !wording.is_empty() {
                self.post_notice(wording.to_string());
            }
        }
        if turn_ended && let Some(hook) = &self.turn_end_hook {
            hook(&self.conversation, machine_history.as_ref());
        }
    }

    /// Flatten the conversation into styled lines for the given width.
    ///
    /// Assistant text goes through the markdown pipeline with the
    /// assistant base color; user and system messages render as
    /// single styled lines; error messages wrap as styled plaintext
    /// at the pane width, so a long failure body stays readable
    /// instead of clipping at the right edge; a completed tool
    /// block renders its verbosity-shaped summary line(s) between
    /// the text blocks around it. A blank line separates a user
    /// message from the agent material beside it, and each user
    /// turn opens and closes with a padding row the caller tints as
    /// part of its box — pad and blank together are the boundary's
    /// breathing room. The live region
    /// (streaming text, in-flight tools) is assembled by the caller.
    fn conversation_lines(
        &self,
        area: Rect,
        start: usize,
        end: usize,
        previous_was_user: &mut Option<bool>,
        outputs: &mut BakeOutputs,
    ) {
        let BakeOutputs {
            lines,
            tool_summary_lines: summary_lines,
            thinking_summary_lines: thinking_lines,
            compaction_summary_lines: compaction_lines,
            user_turn_ranges: user_turns,
            todo_block_ranges: todo_blocks,
        } = outputs;
        let width = area.width.max(1);
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        for (message_index, message) in self.conversation.iter().enumerate().take(end).skip(start) {
            let is_user = matches!(message, TuiMessage::User { .. });
            if previous_was_user.is_some_and(|was_user| was_user != is_user) {
                lines.push(Line::default());
            }
            *previous_was_user = Some(is_user);
            match message {
                TuiMessage::User { text, .. } => {
                    let tint_start = lines.len();
                    lines.push(Line::default());
                    let mark_row = lines.len();
                    for segment in text.split('\n') {
                        lines.extend(plain_wrapped_lines(
                            segment,
                            usize::from(width),
                            self.theme.ui.user_message_fg,
                        ));
                    }
                    lines.push(Line::default());
                    user_turns.push((tint_start, lines.len(), mark_row));
                }
                TuiMessage::Assistant {
                    blocks,
                    duration_ms,
                    ..
                } => {
                    for (block_index, block) in blocks.iter().enumerate() {
                        match block {
                            ContentBlock::Thinking {
                                text,
                                redacted,
                                duration_ms,
                            } => {
                                let key = format!("m{message_index}b{block_index}");
                                let expanded = self.expanded_thinking.contains(&key);
                                let first = lines.len();
                                lines.extend(thinking_section_lines(
                                    text,
                                    *redacted,
                                    *duration_ms,
                                    expanded,
                                    usize::from(width),
                                    self.theme.ui.dim,
                                ));
                                index_summary_rows(thinking_lines, first, lines.len(), &key);
                            }
                            ContentBlock::Text { text } => {
                                lines.extend(markdown::render_markdown(
                                    text,
                                    width,
                                    &markdown_theme,
                                    &syntax_theme,
                                    self.theme.ui.assistant_message_fg,
                                    None,
                                ));
                            }
                            ContentBlock::Tool { .. } => {
                                self.push_tool_block_lines(
                                    block,
                                    width,
                                    lines,
                                    summary_lines,
                                    todo_blocks,
                                );
                            }
                        }
                    }
                    if let Some(duration_ms) = duration_ms {
                        lines.push(Line::styled(
                            format!("duration {}", format_duration(*duration_ms)),
                            self.theme.ui.dim,
                        ));
                    }
                }
                TuiMessage::System { text, .. } => {
                    for segment in text.split('\n') {
                        lines.extend(plain_wrapped_lines(
                            segment,
                            usize::from(width),
                            self.theme.ui.dim,
                        ));
                    }
                }
                TuiMessage::Error { text, .. } => {
                    lines.extend(plain_wrapped_lines(
                        text,
                        usize::from(width),
                        self.theme.ui.status_error,
                    ));
                }
                message @ TuiMessage::Compacted { .. } => {
                    let key = format!("m{message_index}");
                    let first = lines.len();
                    lines.extend(compaction_notice_lines(
                        message,
                        self.expanded_compactions.contains(&key),
                        self.theme.ui.dim,
                    ));
                    index_summary_rows(compaction_lines, first, lines.len(), &key);
                }
            }
        }
    }

    /// Render one completed tool block into the conversation rows.
    ///
    /// Builds the block's verbosity-shaped summary line(s) and, when
    /// the call carries retained expansion data, prepends the fold
    /// marker and appends the expanded detail. Every row the block
    /// produced is registered under the call's id, so a press on any
    /// of them resolves back to the call it belongs to.
    fn push_tool_block_lines(
        &self,
        block: &ContentBlock,
        width: u16,
        lines: &mut Vec<Line<'static>>,
        summary_lines: &mut HashMap<usize, String>,
        todo_blocks: &mut Vec<std::ops::Range<usize>>,
    ) {
        let ContentBlock::Tool {
            name,
            call_id,
            input_preview,
            success,
            elapsed_secs,
            ..
        } = block
        else {
            return;
        };
        let elapsed = if elapsed_secs.is_finite()
            && *elapsed_secs >= 0.0
            && *elapsed_secs <= MAX_ELAPSED_SECS
        {
            std::time::Duration::from_secs_f64(*elapsed_secs)
        } else {
            std::time::Duration::ZERO
        };
        let record = ToolResultDisplay {
            name: name.clone(),
            call_id: String::new(),
            is_error: !*success,
            duration: elapsed,
            input_summary: input_preview.clone(),
            output_preview: String::new(),
            full_input: String::new(),
            full_output: String::new(),
        };
        let first = lines.len();
        lines.extend(crate::tool_render::completed_tool_lines(
            &record,
            &self.theme,
            self.verbosity,
        ));
        if let Some(detail) = self.tool_details.get(call_id) {
            let expanded = self.expanded_tools.contains(call_id);
            prepend_marker(
                lines,
                first,
                if expanded { "▾ " } else { "▸ " },
                self.theme.ui.dim,
            );
            if expanded {
                if matches!(name.as_str(), "Edit" | "MultiEdit" | "Write")
                    && let Some(diff) = crate::diff::parse_receipt(&detail.output)
                {
                    lines.extend(crate::diff::render_diff(
                        &diff,
                        self.config.display.diff_layout,
                        width,
                        crate::diff::diff_colors(&self.theme),
                    ));
                } else if name == "TodoWrite"
                    && let Some(rows) = crate::todo_view::parse_rendered(&detail.output)
                        .or_else(|| crate::todo_view::parse_submission(&detail.input_json))
                {
                    let colors = crate::todo_view::TodoColors {
                        dim: self.theme.ui.dim,
                        accent: self.theme.ui.primary,
                        moved: self.theme.ui.secondary,
                        badge_fill: self.theme.ui.secondary,
                        badge_letters: self.theme.ui.background,
                    };
                    let changes = self
                        .todo_changes
                        .get(call_id.as_str())
                        .cloned()
                        .unwrap_or_default();
                    lines.push(todo_view::summary_line(&rows, &colors));
                    lines.extend(todo_view::render_rows(&rows, &changes, &colors, width));
                    todo_blocks.push(first..lines.len());
                } else {
                    lines.extend(expansion_lines(
                        &detail.input_json,
                        Some(&detail.output),
                        width,
                        self.theme.ui.assistant_message_fg,
                        self.theme.ui.dim,
                    ));
                }
            }
            for index in first..lines.len() {
                summary_lines.insert(index, call_id.clone());
            }
        }
    }

    /// The streaming region's live rows: everything after the
    /// frozen prefix.
    ///
    /// Three parts, assembled in order: the cached lines of frozen
    /// blocks (settled content, rendered once when its terminating
    /// boundary arrived — after a blank run, or after a top-level
    /// fence closes); the live segment — every complete line since
    /// the freeze, re-parsed as markdown so each block renders as
    /// soon as its lines complete (an open code fence is closed
    /// synthetically, so in-flight code renders as the framed block
    /// instead of a wrapped paragraph; a block longer than
    /// [`LIVE_PARSE_LINE_CAP`] lines or [`LIVE_PARSE_BYTE_CAP`]
    /// bytes keeps only its trailing cap as markdown and its head
    /// renders as plaintext until it freezes, bounding the
    /// per-frame parse); and the unterminated tail — the last
    /// line of what the freeze has not yet consumed, as
    /// width-wrapped plaintext, so a closer the frozen block
    /// already rendered never draws twice while growth after it
    /// appears as it arrives. An empty buffer renders nothing; a
    /// poisoned lock is recovered —
    /// the same policy the drain applies — so the live view keeps
    /// rendering after another thread's panic. Returns whether the
    /// region's rendered lines changed this call — the signal that
    /// the line numbering below the settled transcript moved.
    fn refresh_streaming_region(&mut self, width: u16) -> bool {
        let buffer = self
            .state
            .streaming_text
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if buffer.is_empty() {
            return self.reset_stream_cache();
        }
        let mut changed = false;
        if self.stream_cache_width != width {
            changed = self.reset_stream_cache();
            self.stream_cache_width = width;
        } else if self.frozen_upto > 0
            && buffer
                .get(..self.frozen_upto)
                .is_none_or(|prefix| fingerprint(prefix) != self.frozen_fingerprint)
        {
            changed = self.reset_stream_cache();
        }
        let frozen_before = self.frozen_lines.len();
        self.advance_freeze(&buffer, width);
        if self.frozen_lines.len() != frozen_before {
            changed = true;
        }

        let unfrozen = buffer.get(self.frozen_upto..).unwrap_or("");
        let live = unfrozen
            .rsplit_once('\n')
            .map_or(unfrozen, |(_, tail)| tail);
        let live_md = unfrozen
            .get(..unfrozen.len().saturating_sub(live.len()))
            .filter(|segment| !segment.is_empty());
        let stamp = live_md.map_or(0, fingerprint) ^ {
            let mut seed = live.len() as u64;
            for ch in live.chars().rev().take(64) {
                seed = seed.wrapping_mul(31).wrapping_add(u64::from(u32::from(ch)));
            }
            seed
        };
        if self.live_stamp == stamp
            && self.live_frozen_upto == self.frozen_upto
            && self.live_width == width
        {
            return changed;
        }

        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let base = self.theme.ui.assistant_message_fg;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let leading = live_md.map_or(0, |segment| split_blank_prefix(segment).0);
        if !self.frozen_lines.is_empty() && (live_md.is_some() || !live.is_empty()) {
            let gap = self.frozen_separators.max(leading);
            for _ in 0..gap {
                lines.push(Line::from(""));
            }
        }
        if let Some(segment) = live_md {
            let (_, core) = split_blank_prefix(segment);
            let (line_head, recent) = split_live_segment(core, LIVE_PARSE_LINE_CAP);
            let (older, recent) = bound_recent_bytes(line_head, recent, LIVE_PARSE_BYTE_CAP);
            let md_at_buffer_start = self.frozen_upto == 0 && leading == 0 && older.is_empty();
            if !older.is_empty() {
                lines.extend(plain_wrapped_lines(&older, usize::from(width), base));
            }
            let mut source = String::new();
            if !md_at_buffer_start {
                source.push('\n');
            }
            if let Some(marker) = open_fence_closer(&older) {
                source.push_str(marker);
                source.push('\n');
                source.push_str(recent);
                source.push('\n');
                source.push_str(marker);
            } else {
                source.push_str(recent);
                if let Some(marker) = open_fence_closer(recent) {
                    source.push('\n');
                    source.push_str(marker);
                }
            }
            lines.extend(markdown::render_markdown(
                &source,
                width,
                &markdown_theme,
                &syntax_theme,
                base,
                None,
            ));
        }
        if !live.is_empty() {
            lines.extend(plain_wrapped_lines(live, usize::from(width), base));
        }
        self.live_stamp = stamp;
        self.live_frozen_upto = self.frozen_upto;
        self.live_width = width;
        self.live_lines = lines;
        true
    }

    /// Reset the streaming cache.
    ///
    /// Drops the frozen lines, offset, and separator count; the next
    /// layout re-freezes from the buffer's start. Called when the
    /// buffer empties (the reply graduated or the turn failed), its
    /// frozen prefix stops matching, or the pane width changes.
    /// Returns whether any rendered lines were dropped.
    fn reset_stream_cache(&mut self) -> bool {
        let dropped = !(self.frozen_lines.is_empty() && self.live_lines.is_empty());
        self.frozen_lines.clear();
        self.frozen_upto = 0;
        self.frozen_separators = 0;
        self.frozen_fingerprint = 0;
        self.live_stamp = 0;
        self.live_frozen_upto = 0;
        self.live_width = 0;
        self.live_lines.clear();
        dropped
    }

    /// Freeze every newly settled block into the cache.
    ///
    /// Extends [`frozen_upto`](Self::frozen_upto) to the last safe
    /// boundary in the buffer and appends the newly frozen slice's
    /// rendered lines. The slice's leading blank run belongs to the
    /// join gap, not the slice's own render: the gap rows pushed
    /// before the lines are the larger of the pending separator
    /// count and that run, so a boundary taken at a closed fence
    /// and a following blank run never double-count. When no
    /// visible line is cached yet the run is the document-leading
    /// one and the batch's absorb-one rule applies instead. A safe
    /// boundary sits at the start of the line after a blank line or
    /// after a top-level fence closes, outside any code fence, where
    /// the blocks on either side cannot continue each other — so the
    /// slice parses standalone exactly as it parses in place.
    fn advance_freeze(&mut self, buffer: &str, width: u16) {
        let boundary = last_safe_boundary(buffer, self.frozen_upto);
        if boundary <= self.frozen_upto {
            return;
        }
        let Some(slice) = buffer.get(self.frozen_upto..boundary) else {
            return;
        };
        let (leading, core) = split_blank_prefix(slice);
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let rendered = markdown::render_markdown(
            core,
            width,
            &markdown_theme,
            &syntax_theme,
            self.theme.ui.assistant_message_fg,
            None,
        );
        let gap = if self.frozen_lines.is_empty() {
            self.frozen_separators.saturating_sub(1)
        } else {
            self.frozen_separators.max(leading)
        };
        for _ in 0..gap {
            self.frozen_lines.push(Line::from(""));
        }
        self.frozen_lines.extend(rendered);
        self.frozen_upto = boundary;
        self.frozen_separators = if core.is_empty() && !self.frozen_lines.is_empty() {
            0
        } else {
            trailing_blank_lines(slice).max(1)
        };
        if let Some(prefix) = buffer.get(..boundary) {
            self.frozen_fingerprint = fingerprint(prefix);
        }
    }

    /// The in-flight tool indicator lines, with their click rows.
    ///
    /// One dim line per dispatched call — expanded, when opened,
    /// to the call's captured input with the output pending; a
    /// poisoned lock renders none, the same policy the status bar
    /// applies. Returns the lines and, for each call the capture
    /// store knows, every row of its block — summary and
    /// expansion alike, the rows a click toggles.
    fn active_tool_lines(&self, width: u16) -> (Vec<Line<'static>>, HashMap<usize, String>) {
        let tools = self
            .state
            .active_tools
            .lock()
            .map_or_else(|_| Vec::new(), |tools| tools.clone());
        let captures = self
            .state
            .tool_captures
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut lines = Vec::new();
        let mut rows = HashMap::new();
        for tool in &tools {
            let first = lines.len();
            lines.extend(crate::tool_render::running_tool_lines(
                tool,
                &self.theme,
                self.verbosity,
            ));
            let Some(capture) = captures.get(&tool.call_id) else {
                continue;
            };
            let expanded = self.expanded_tools.contains(&tool.call_id);
            prepend_marker(
                &mut lines,
                first,
                if expanded { "▾ " } else { "▸ " },
                self.theme.ui.dim,
            );
            if expanded {
                let output = if capture.done {
                    Some(capture.output.as_str())
                } else {
                    None
                };
                lines.extend(expansion_lines(
                    &capture.input_json,
                    output,
                    width,
                    self.theme.ui.assistant_message_fg,
                    self.theme.ui.dim,
                ));
            }
            for index in first..lines.len() {
                rows.insert(index, tool.call_id.clone());
            }
        }
        (lines, rows)
    }

    /// The frame's single wrap of the composer, reused across
    /// frames that do not touch it.
    ///
    /// A full-buffer wrap costs tens of milliseconds at the paste
    /// cap, and idle frames — the caret's blink, the tool spinner,
    /// a streaming delta — would otherwise pay it every time. The
    /// cache keys on the editor's mutation stamp and the wrap
    /// width, so only edits and resizes re-wrap; everything else
    /// clones the cached grid.
    fn composer_wrap(&mut self, width: u16) -> (Arc<Vec<String>>, (u16, u16)) {
        if let Some(cache) = &self.input_wrap_cache
            && cache.stamp == self.input.stamp()
            && cache.width == width
        {
            return (Arc::clone(&cache.rows), cache.caret);
        }
        let (rows, caret) = self.input.display_rows_and_caret(width);
        let caret = caret.unwrap_or((0, 0));
        let rows = Arc::new(rows);
        self.input_wrap_cache = Some(InputWrapCache {
            stamp: self.input.stamp(),
            width,
            rows: Arc::clone(&rows),
            caret,
        });
        (rows, caret)
    }

    /// The connection-health line for the notice row, if one shows.
    ///
    /// A retry in flight is invisible here (the retries themselves
    /// live in the conversation as reconnection rows; the notice row
    /// would only duplicate them), and the reconnected note holds only
    /// for the notice hold before yielding to whatever transient
    /// notice waits beneath it. A running compaction pass does not
    /// render here either — it lives in the conversation's live lane
    /// while it holds, beside the streaming rows it sits among.
    fn health_notice_text(&self) -> Option<String> {
        match &*self
            .state
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            HealthNotice::Reconnected { at } => {
                let until = at.checked_add(NOTICE_HOLD)?;
                (Instant::now() < until).then(|| "reconnected".to_string())
            }
            HealthNotice::Idle
            | HealthNotice::Retrying { .. }
            | HealthNotice::Compacting { .. } => None,
        }
    }

    /// The rows the waiting section needs for the prompt it shows.
    ///
    /// The badge, the air above it and below it, and the rows the
    /// shown prompt wraps into at this width — the section reserves
    /// what it paints and nothing beneath it: a one-line wording ends
    /// the section at its own last row, so the air under the queue is
    /// the frame's furniture (the notice row, the spacer), never the
    /// section's own blank margin. A wrapping wording grows the
    /// section only as far as the two prompt rows the cap allows, so
    /// the conversation pays for words, not for air.
    fn queue_section_height(&self, width: u16) -> u16 {
        let total = self.queued_display_len();
        if total == 0 {
            return 0;
        }
        let shown = self.queue_view.min(total.saturating_sub(1));
        let Some((_, text)) = self.queued_at(shown) else {
            return 0;
        };
        let prefix = queue_prompt_prefix(total, shown);
        let limit = usize::from(width).saturating_sub(prefix.width()).max(1);
        let cap = usize::from(QUEUE_ROWS).saturating_sub(3);
        let body = wrap_prompt_rows(&text, limit, cap).len();
        u16::try_from(body.saturating_add(3)).unwrap_or(QUEUE_ROWS)
    }

    /// Render the prompts waiting to run.
    ///
    /// The section opens with a `QUEUED` title row naming what the rows
    /// under it are, then shows one prompt at a time, its text across the
    /// remaining rows rather than a digest: the reason to look at this
    /// section is to check wording before it reaches the model, so a
    /// summary of a prompt is precisely what the user came here to avoid.
    /// The marker and the `[n/total]` count ride the prompt's first row
    /// rather than spending columns of their own, and the count stands
    /// down for a lone prompt — being one of one is not a fact worth two
    /// columns.
    ///
    /// The keys ride the section's last row when the width can pay for
    /// them, and name only what the current mode answers: the walk's
    /// chords while the section has the keyboard, the saving ones while a
    /// prompt is out in the composer, and otherwise the chord that gets the
    /// user here. A hint for keys that do nothing from where the keys
    /// actually are would be a lie printed in the theme's dim. Where the
    /// text and the hint cannot both fit, the text keeps the row and the
    /// hint is dropped whole — never re-wrapped around, never allowed to
    /// push a word off the cut.
    ///
    /// Paints no background across its rows, exactly as the notice row
    /// does: these are lines of text, not a box, and a filled panel
    /// would put a block of the theme's surface into a terminal
    /// configured to paint none. The title is the one exception, and it
    /// is a badge rather than a panel — a pill a few cells wide is a
    /// label, not a filled box — stood in from the section's left edge,
    /// held a blank row clear of the prompt below it and another clear
    /// of the transcript above, and filled with the primary, the same
    /// accent the composer's edge column wears, letters in the canvas
    /// color, at rest and under the keys alike. In a theme that defers
    /// to the terminal the fill resets and the badge degrades to plain
    /// text — the letters keep at most a foreground voice, which
    /// paints nothing.
    fn render_queue(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let total = self.queued_display_len();
        let shown = self.queue_view.min(total.saturating_sub(1));
        let Some((_, text)) = self.queued_at(shown) else {
            return;
        };
        let rows = usize::from(area.height.min(QUEUE_ROWS));
        let top_standoff = usize::from(rows > 3);
        let bottom_standoff = usize::from(rows > 2);
        let body_rows = rows
            .saturating_sub(1)
            .saturating_sub(top_standoff)
            .saturating_sub(bottom_standoff);
        let focused = self.queue_mode == QueueMode::Focused;
        let prefix = queue_prompt_prefix(total, shown);
        let width = usize::from(area.width);
        let limit = width.saturating_sub(prefix.width()).max(1);
        let body = wrap_prompt_rows(&text, limit, body_rows);
        let marker_style = Style::default().fg(if focused {
            self.theme.ui.primary
        } else {
            self.theme.ui.dim
        });
        let (fill, letters) = (self.theme.ui.primary, self.theme.ui.background);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
        if top_standoff > 0 {
            lines.push(Line::from(Vec::new()));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {QUEUE_TITLE} "),
                Style::default().fg(letters).bg(fill),
            ),
            Span::raw(" "),
            Span::styled(
                self.queue_hint().to_string(),
                Style::default().fg(self.theme.ui.dim),
            ),
        ]));
        if bottom_standoff > 0 {
            lines.push(Line::from(Vec::new()));
        }
        for (index, row) in body.into_iter().take(body_rows).enumerate() {
            let mut spans = Vec::with_capacity(2);
            if index == 0 {
                spans.push(Span::styled(prefix.clone(), marker_style));
            }
            spans.push(Span::styled(
                row,
                Style::default().fg(self.theme.ui.input_text),
            ));
            lines.push(Line::from(spans));
        }
        lines.resize(rows, Line::from(Vec::new()));
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// The chords the queue section names beside its badge.
    ///
    /// They are the mode's, not the section's: while the composer holds the
    /// keys the section cannot act on a letter typed into it, so the badge
    /// offers the way in rather than keys that would be swallowed a moment
    /// later. Once the section has them, it says what it does with them,
    /// including the way back out — the mode that swallows everything else
    /// owes the user the key that ends it.
    fn queue_hint(&self) -> &'static str {
        if matches!(self.queue_mode, QueueMode::Editing { .. }) {
            "enter save  esc discard"
        } else if let QueueMode::Confirming { act } = self.queue_mode {
            match act {
                QueueAct::Steer => "guide the running turn? y/n",
                QueueAct::Drop => "drop this prompt? y/n",
            }
        } else if self.queue_mode == QueueMode::Focused {
            "\u{2190}/\u{2192} walk  e edit  s guide  d drop  esc back"
        } else {
            "ctrl+q walk the queue"
        }
    }

    /// Render the notice row: one line of transient or state-driven
    /// message above the composer.
    ///
    /// The armed-quit hint owns the row while it holds — safety
    /// wording outranks anything expiring — and a transient
    /// notice (a cancel, an event worth a glance, not a transcript
    /// row) shows until its hold elapses. Stream and tool retries
    /// live in the conversation as their own rows and keep this row
    /// free. Blank otherwise; the row is
    /// always reserved so nothing on screen shifts when a message
    /// arrives or leaves.
    fn render_notice(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let text = if self.quit.armed {
            Some(self.transient_notice.as_ref().map_or_else(
                || "press ctrl+c again to quit".to_string(),
                |(notice, _)| notice.clone(),
            ))
        } else {
            self.health_notice_text()
                .or_else(|| {
                    self.transient_notice
                        .as_ref()
                        .filter(|(_, until)| *until > Instant::now())
                        .map(|(text, _)| text.clone())
                })
                .or_else(|| self.steering_hint())
        };
        if let Some(text) = text {
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(self.theme.ui.input_border))
                    .alignment(ratatui::layout::Alignment::Right),
                area,
            );
        }
    }

    /// The one thing the row offers on its own initiative: a draft standing
    /// there while a run holds the agent.
    ///
    /// The impatient chord is the one a user has to think of at the moment it
    /// matters, and that moment looks exactly like typing an ordinary prompt.
    /// Empty composer or idle agent, the row stays blank — advertising a key
    /// that would do nothing is noise, and the row belongs to real notices
    /// whenever they have anything to say.
    fn steering_hint(&self) -> Option<String> {
        if self.guidance.is_none()
            || !self.state.agent_running.load(Ordering::SeqCst)
            || self.input.text().trim().is_empty()
        {
            return None;
        }
        Some("ctrl+s sends this to the running turn".to_string())
    }

    /// Render the input field: a fixed multi-row window onto the
    /// buffer, as a borderless composer drawn on the theme's surface
    /// color and padded on all sides. On painted chrome the pane's
    /// left edge is a solid one-cell accent column in the theme's
    /// primary color — the composer is the screen's always-focused
    /// surface, so its marker takes the focus accent — sitting inside
    /// the side padding, clear of the text; the paintless theme keeps
    /// its terminal-drawn hairlines and paints nothing, edge included.
    ///
    /// The field's shape is a pure background fill with square
    /// corners — solid by construction, like the scrollbar. Corners
    /// stay square deliberately: a cell grid cannot draw a smooth
    /// curve, and both approximations it can draw (corner glyphs,
    /// clipped corner cells) render as pixel-like steps. The text sits
    /// inside horizontal and vertical padding; the window shows the
    /// caret's line and the lines above it, so typing at the end
    /// sees the newest lines and browsing up scrolls with the
    /// caret. The box closes with its identity row — the model, its
    /// effort, and the permission mode in the dim register, the
    /// composer's state tag at the row's very right — sitting on the
    /// box's bottom edge, the padding row above it the breathing.
    /// It is not a composer row: nothing typed lands there and the
    /// caret never visits it. The field carries no other text of its
    /// own — queue state lives in its own section. `rows` and
    /// `caret` are the frame's single wrap of the buffer, computed by
    /// [`render`](Self::render) and shared with the identity row's
    /// tag. The text window takes its height from the pane's own rows
    /// net of the vertical padding, so no paint of the field's lands
    /// outside the pane it was given; the cursor position is clamped
    /// the same way, so a terminal too short for the field's full
    /// height parks the cursor on a row the field owns, and a field
    /// allotted no rows at all parks no cursor.
    fn render_input(&mut self, frame: &mut Frame, area: Rect, rows: &[String], caret: (u16, u16)) {
        let (caret_row, column) = caret;
        let muted = self.section_holds_keys();
        let voice = if muted {
            self.theme.ui.dim
        } else {
            self.theme.ui.input_text
        };

        let surface = self.theme.ui.surface;
        let text_style = if self.theme.ui.composer_border.is_some() {
            Style::default().fg(voice)
        } else {
            frame
                .buffer_mut()
                .set_style(area, Style::default().bg(surface));
            let edge = Rect {
                width: area.width.min(1),
                ..area
            };
            frame.buffer_mut().set_style(
                edge,
                Style::default().bg(if muted {
                    self.theme.ui.dim
                } else {
                    self.theme.ui.primary
                }),
            );
            Style::default().fg(voice).bg(surface)
        };
        let text_height = INPUT_TEXT_ROWS.min(
            area.height
                .saturating_sub(INPUT_VERTICAL_PADDING.saturating_mul(2))
                .saturating_sub(1),
        );
        let info_style = if self.theme.ui.composer_border.is_some() {
            Style::default().fg(self.theme.ui.dim)
        } else {
            Style::default().fg(self.theme.ui.dim).bg(surface)
        };
        let info_x = area
            .x
            .saturating_add(INPUT_SIDE_INSET)
            .min(area.right().saturating_sub(1));
        let info_width = area
            .width
            .max(1)
            .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2));
        if area.height >= 1 {
            let identity_row = Rect {
                y: area.bottom().saturating_sub(1),
                height: 1,
                width: info_width,
                x: info_x,
            };
            let tag = Self::input_state_tag(rows, caret);
            let tag_width = if tag.is_empty() {
                0
            } else {
                u16::try_from(tag.width().saturating_add(1))
                    .unwrap_or(info_width)
                    .min(info_width)
            };
            let identity = self.composer_identity_text();
            if !identity.is_empty() {
                frame.render_widget(
                    Paragraph::new(identity).style(info_style),
                    Rect {
                        width: info_width.saturating_sub(tag_width),
                        ..identity_row
                    },
                );
            }
            if tag_width > 0 {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        tag,
                        Style::default().fg(self.theme.ui.input_border),
                    ))
                    .style(info_style)
                    .alignment(ratatui::layout::Alignment::Right),
                    identity_row,
                );
            }
        }
        let interior = Rect {
            x: area
                .x
                .saturating_add(INPUT_SIDE_INSET)
                .min(area.right().saturating_sub(1)),
            y: area.y.saturating_add(INPUT_VERTICAL_PADDING),
            width: area
                .width
                .max(1)
                .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2)),
            height: text_height,
        };
        let visible_rows = usize::from(text_height);
        let start = usize::from(caret_row).saturating_sub(visible_rows.saturating_sub(1));
        self.input_view = Some(InputView {
            pane: area,
            origin: (interior.x, interior.y),
            start,
            visible: visible_rows,
        });
        let lines: Vec<Line<'_>> = rows
            .iter()
            .skip(start)
            .take(visible_rows)
            .map(|row| Line::styled(row.as_str(), text_style))
            .collect();
        frame.render_widget(Paragraph::new(lines), interior);

        let caret_x = area
            .x
            .saturating_add(INPUT_SIDE_INSET)
            .saturating_add(column)
            .min(
                area.x
                    .saturating_add(INPUT_SIDE_INSET)
                    .saturating_add(interior.width)
                    .saturating_sub(1),
            )
            .min(area.right().saturating_sub(2));
        let caret_y = area
            .y
            .saturating_add(INPUT_VERTICAL_PADDING)
            .saturating_add(
                u16::try_from(usize::from(caret_row).saturating_sub(start)).unwrap_or(0),
            )
            .min(area.bottom().saturating_sub(1));
        if area.height > 0 && self.composer_caret_visible() {
            frame.set_cursor_position((caret_x, caret_y));
        }
    }

    /// Whether the waiting section holds the keyboard, the composer
    /// reduced to reading matter.
    ///
    /// True while the section is focused or holding an ask — the two
    /// states where a keypress lands on the queue, not the draft — and
    /// false the moment the keys come back: an edit lifts a prompt into
    /// the composer, and there the composer is the active editor again.
    fn section_holds_keys(&self) -> bool {
        matches!(
            self.queue_mode,
            QueueMode::Focused | QueueMode::Confirming { .. }
        )
    }

    /// Whether the composer's terminal cursor shows this frame.
    ///
    /// False while the blink phase has it off — and for the whole
    /// time a permission ask is pending: the sheet owns the composer's
    /// rows and its keys, and a cursor there would point at nothing
    /// the user can edit. The waiting section's focus and its ask
    /// hide it for the same reason: the keys are landing on the
    /// queue, and a blinking caret in a composer that cannot be typed
    /// into would point at a lie. A frame that sets no cursor
    /// position hides the terminal cursor, so this is the whole
    /// show-or-hide decision.
    #[must_use]
    pub fn composer_caret_visible(&self) -> bool {
        self.caret_blink.on
            && !self.section_holds_keys()
            && self.pending_permissions.front().is_none()
            && self.pending_questions.is_empty()
    }

    /// Render the footer's three rows: a row of air under the
    /// composer, the bar's row, and the bottom row. The air is the
    /// composer's bottom margin — the identity row closes the box and
    /// the footer's facts start one clear row under it. The context
    /// readout is the footer's head — the bar a cell in from the
    /// left edge of its row, standing exactly over the numbers that
    /// lead the bottom row from that same column — and the session
    /// id is its tail, right-aligned a cell in from the right edge
    /// on the bar's row: the footer's middle, vertically centered
    /// between the air above and the numbers below. The composer's
    /// state tag no longer rides here —
    /// it moved to the identity row inside the composer — so the
    /// footer is all session, no draft state. Both rows sit on the
    /// themed bar colors, and an id without the room to clear the bar
    /// is forgone rather than run into it. A chunk allotted no rows
    /// renders nothing — the bar never paints a row another pane
    /// owns.
    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let bar_style = Style::default()
            .fg(self.theme.ui.status_bar_fg)
            .bg(self.theme.ui.status_bar_bg);
        let (context_tokens, window) = self.context_snapshot();
        let context_fill = context_bar_fill(context_tokens, window, CONTEXT_BAR_CELLS);
        let readout = context_bar_readout(context_tokens, window);
        let air = area.height.saturating_sub(2).min(STATUS_BAR_AIR);
        frame.buffer_mut().set_style(
            Rect {
                y: area.y.saturating_add(air),
                height: area.height.saturating_sub(air),
                ..area
            },
            Style::default().bg(self.theme.ui.status_bar_bg),
        );
        let content = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        };
        let bar_y = content.y.saturating_sub(1);
        let id_span = if area.height > 1 {
            self.session_id.as_ref().and_then(|id| {
                let id_width = u16::try_from(id.width()).ok()?;
                let start = area
                    .width
                    .checked_sub(id_width)?
                    .checked_sub(u16::try_from(STATUS_BAR_MARGIN).unwrap_or(0))?;
                (usize::from(start) > STATUS_BAR_MARGIN + CONTEXT_BAR_CELLS + 1).then(|| {
                    let x = area.x.saturating_add(start);
                    (bar_y, x, x.saturating_add(id_width))
                })
            })
        } else {
            None
        };
        self.session_id_span.set(id_span);
        if area.height > 1 {
            let mut bar_line = self.context_bar_line(STATUS_BAR_MARGIN, context_fill);
            if let Some((_, start, _)) = id_span {
                let pad = usize::from(start)
                    .saturating_sub(
                        usize::from(area.x)
                            .saturating_add(STATUS_BAR_MARGIN)
                            .saturating_add(CONTEXT_BAR_CELLS),
                    )
                    .max(1);
                bar_line.spans.push(Span::raw(" ".repeat(pad)));
                bar_line.spans.push(Span::raw(
                    self.session_id.as_deref().unwrap_or_default().to_string(),
                ));
            }
            frame.render_widget(
                Paragraph::new(bar_line).style(bar_style),
                Rect {
                    y: bar_y,
                    height: 1,
                    ..area
                },
            );
        }
        let mut spans = vec![Span::raw(" ".repeat(STATUS_BAR_MARGIN))];
        if let Some((percent, totals)) = &readout {
            spans.push(Span::styled(
                totals.clone(),
                Style::default().fg(self.theme.ui.dim),
            ));
            spans.push(Span::styled(
                format!(" ({percent})"),
                Style::default().fg(self.theme.ui.scrollbar_thumb),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style), content);
    }

    /// Render the place row: the working directory and its branch on
    /// their own row directly above the composer, outside the box —
    /// a caption, dim on the plain background, inset to line up with
    /// the composer's text. The caption closes tight against the box:
    /// the box's top padding row keeps the blank row between the
    /// caption and the text, so no air row of the layout's own rides
    /// between the two. The caption costs the frame a single row until
    /// the host names a directory.
    fn render_place_row(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let text = self.place_text();
        if text.is_empty() {
            return;
        }
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(self.theme.ui.dim)),
            Rect {
                x: area
                    .x
                    .saturating_add(INPUT_SIDE_INSET)
                    .min(area.right().saturating_sub(1)),
                y: area.y,
                height: 1,
                width: area
                    .width
                    .max(1)
                    .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2)),
            },
        );
    }

    /// The place row's text: the working directory spelled per the
    /// config's [`PathStyle`] — the full path, or the `~/`-relative
    /// shape — with the git branch in parentheses when the host named
    /// one. Where the session stands, named above the box the user
    /// types into; the footer keeps what the session stands in
    /// briefly, its context pair and its id.
    fn place_text(&self) -> String {
        match (&self.working_dir, &self.git_branch) {
            (Some(dir), Some(branch)) => {
                format!(
                    "{} ({branch})",
                    display_path(dir, self.config.display.path_style)
                )
            }
            (Some(dir), None) => display_path(dir, self.config.display.path_style),
            (None, _) => String::new(),
        }
    }

    /// The composer's identity row: the model with its thinking
    /// effort and the permission mode, one dim line closing the box
    /// over its final row of air, the composer's state tag at the
    /// row's very right.
    ///
    /// These are the facts about the *next* thing the user types —
    /// which model answers and what it may do without asking — so they
    /// ride inside the composer's box, one row of context under its
    /// text, rather than on the footer below where they read as
    /// facts about the session's tail.
    fn composer_identity_text(&self) -> String {
        let effort = match self.config.api.reasoning_effort.as_deref() {
            Some(level) => capitalize_first(level),
            None => "Auto".to_string(),
        };
        format!(
            " {} ({effort})  ·  mode: {}",
            self.config.api.model,
            self.config.runner.permission_mode.as_str()
        )
    }

    /// The context readout's inputs: the tokens used against the window
    /// the engine actually runs — the live window once the engine has
    /// reported one, the config's (or the fallback) before that.
    fn context_snapshot(&self) -> (u64, u64) {
        let live_window = self.state.context_window.load(Ordering::SeqCst);
        let window = if live_window > 0 {
            live_window
        } else {
            self.config
                .api
                .context_window
                .unwrap_or(dch_config::FALLBACK_CONTEXT_WINDOW)
        };
        let used = self
            .state
            .tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .context_tokens;
        (used, window)
    }

    /// The context bar's own row: `padding` cells of air, then the
    /// fill over the track — one line the bottom row's numbers and the
    /// row above it share a column count through.
    fn context_bar_line(&self, padding: usize, fill: usize) -> Line<'static> {
        Line::from(vec![
            Span::raw(" ".repeat(padding)),
            Span::styled(
                " ".repeat(fill),
                Style::default().bg(self.theme.ui.scrollbar_thumb),
            ),
            Span::styled(
                " ".repeat(CONTEXT_BAR_CELLS.saturating_sub(fill)),
                Style::default().bg(self.theme.ui.scrollbar_track),
            ),
        ])
    }

    /// The composer's state as its identity-row tag, empty when the
    /// buffer fits the window.
    ///
    /// Reports which line the caret is on over the buffer's total
    /// while the draft outgrows the composer's window — the one fact
    /// about the draft the box itself cannot show. Submissions queued
    /// behind the driver count themselves in the queue's own section,
    /// and the footer's right edge carries no text in any state.
    /// `rows` and `caret` arrive from the frame's single wrap of the
    /// buffer, so asking never re-wraps the editor.
    fn input_state_tag(rows: &[String], caret: (u16, u16)) -> String {
        if rows.len() > usize::from(INPUT_TEXT_ROWS) {
            let (caret_row, _) = caret;
            format!(
                "{}/{}",
                usize::from(caret_row).saturating_add(1).min(rows.len()),
                rows.len()
            )
        } else {
            String::new()
        }
    }
}

/// Prefix a disclosure glyph onto one rendered line.
///
/// The summary row's existing spans shift right by the glyph, so
/// the marker rides whatever styling the row already carries.
fn prepend_marker(lines: &mut [Line<'static>], index: usize, glyph: &str, color: Color) {
    let Some(line) = lines.get_mut(index) else {
        return;
    };
    let mut spans = Vec::with_capacity(line.spans.len().saturating_add(1));
    spans.push(Span::styled(glyph.to_string(), Style::default().fg(color)));
    spans.extend(std::mem::take(&mut line.spans));
    line.spans = spans;
}

/// The lines of a tool block's expansion.
///
/// The full command — the call's pretty-printed input — over the
/// call's output, each indented under its label and wrapped at the
/// pane width. A call still running shows its command with the
/// output pending, so an expansion opened early is not a dead end.
fn expansion_lines(
    input_json: &str,
    output: Option<&str>,
    width: u16,
    base: Color,
    dim: Color,
) -> Vec<Line<'static>> {
    let wrap = usize::from(width.max(1));
    let mut lines = Vec::new();
    lines.push(Line::styled(
        "    input:".to_string(),
        Style::default().fg(dim),
    ));
    for raw in input_json.split('\n') {
        lines.extend(plain_wrapped_lines(&format!("    {raw}"), wrap, dim));
    }
    match output {
        Some(text) => {
            lines.push(Line::styled(
                "    output:".to_string(),
                Style::default().fg(dim),
            ));
            for raw in text.split('\n') {
                lines.extend(plain_wrapped_lines(&format!("    {raw}"), wrap, base));
            }
        }
        None => lines.push(Line::styled(
            "    output: … running".to_string(),
            Style::default().fg(dim),
        )),
    }
    lines.push(Line::from(""));
    lines
}

/// Cells the status bar's context-usage bar fills.
///
/// Sixteen — wide enough that the numbers spelling the bar's fraction
/// never run past its right edge (`200k/200k (100%)`, the widest the
/// compact units spell, is sixteen cells) — while staying a glanceable
/// fraction rather than a second scrollbar.
const CONTEXT_BAR_CELLS: usize = 16;

/// The rows the status bar occupies at any width: three — a row of
/// air under the composer, the bar's row, and the bottom row. The
/// air is the composer's bottom margin and stays unpainted, so the
/// identity row that closes the box — the model, its effort, and the
/// mode — reads as the last thing in the composer and the footer's
/// facts start one clear row under it. The footer carries no prose
/// to wrap: its facts are the numbers leading the bottom row from
/// the bar's left edge and the id closing it at the right, and a
/// frame too narrow for both forgoes the id rather than growing
/// rows.
const STATUS_BAR_ROWS: u16 = 3;

/// The footer's air: the rows of its margin, painted by no pane.
/// They belong to no content — an overlay sheet may stand on them,
/// which is how the margin costs the sheets nothing: their bottom
/// edge keeps the same clear distance above the bar's own rows.
const STATUS_BAR_AIR: u16 = 1;

/// The footer's side margins: one cell of air at each edge. The
/// bar and the numbers beneath it start one cell in — the bar
/// standing exactly over what it measures — and the session id
/// closes its row one cell short of the right edge.
///
/// One cell each side — a breath, not an inset: the readout is
/// still the first thing at the left and the id the last at the
/// right; neither touches the frame's edge.
const STATUS_BAR_MARGIN: usize = 1;

/// The filled-cell count of the status bar's context-usage bar.
///
/// `used` tokens against a `window` budget, rounded to the nearest of
/// `cells` cells and clamped to the bar — a full bar when usage
/// somehow exceeds the window, an empty one when the window is
/// unknown (zero), which is the fresh-session state before the first
/// turn reports.
fn context_bar_fill(used: u64, window: u64, cells: usize) -> usize {
    if window == 0 {
        return 0;
    }
    let cells = u64::try_from(cells).unwrap_or(u64::MAX);
    let filled = used
        .saturating_mul(cells)
        .saturating_add(window.saturating_div(2))
        .checked_div(window)
        .unwrap_or(cells);
    usize::try_from(filled.min(cells)).unwrap_or(usize::MAX)
}

/// The numbers that spell the context bar's fraction.
///
/// The percentage rounds from the true ratio — not from the bar's
/// cells, whose steps would make every reading end in zero — and the
/// totals ride in compact units, thousands up to a million and
/// millions beyond (`419k/1M`, never `1000k`), so a bar kept
/// deliberately glanceable still answers "how full, exactly". A
/// session beyond its window shows its honest over-100 percentage:
/// the bar clamps full, the number tells the truth. An unknown window
/// (zero) returns `None`: there is no denominator, and the bar above
/// the readout is already empty.
fn context_bar_readout(used: u64, window: u64) -> Option<(String, String)> {
    if window == 0 {
        return None;
    }
    let percent = used
        .saturating_mul(100)
        .saturating_add(window.saturating_div(2))
        .checked_div(window)
        .unwrap_or(0);
    let compact = |tokens: u64| {
        let thousands = tokens.saturating_add(500).checked_div(1_000).unwrap_or(0);
        if thousands < 1_000 {
            return format!("{thousands}k");
        }
        let tenths = thousands.saturating_add(50).checked_div(100).unwrap_or(0);
        let whole = tenths.checked_div(10).unwrap_or(0);
        let frac = tenths.saturating_sub(whole.saturating_mul(10));
        if frac == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{frac}M")
        }
    };
    Some((
        format!("{percent}%"),
        format!("{}/{}", compact(used), compact(window)),
    ))
}

/// Spell the working directory per the configured style: the full
/// path as the host named it, or the `~/`-relative shape when the
/// directory sits under the user's home — falling back to the full
/// path outside home or when the environment names no home, so the
/// row never shows a bare unanchored fragment.
fn display_path(dir: &str, style: PathStyle) -> String {
    match style {
        PathStyle::Full => dir.to_string(),
        PathStyle::Relative => std::env::var("HOME")
            .ok()
            .filter(|home| !home.is_empty())
            .and_then(|home| relativize(Path::new(dir), Path::new(&home)))
            .unwrap_or_else(|| dir.to_string()),
    }
}

/// The `~/`-spelling of `dir` under `home`, or `None` when the
/// directory sits outside home.
///
/// The home directory itself is the bare `~`.
fn relativize(dir: &Path, home: &Path) -> Option<String> {
    if dir == home {
        Some("~".to_string())
    } else {
        dir.strip_prefix(home)
            .ok()
            .map(|rest| format!("~/{}", rest.display()))
    }
}

/// Copy `word` with its first character uppercased.
///
/// The status bar's effort label shows the configured level's own
/// spelling with a leading capital — `low` renders `(Low)` — whatever
/// case the config file carried; the rest of the word is untouched.
fn capitalize_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}

/// One display cell of the rendered transcript.
///
/// A conversation line index paired with the column, in cells, of
/// the character within that rendered line — the unit a mouse
/// selection drags over.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CellPos {
    /// The conversation line the cell sits on.
    ///
    /// Index into the app's rendered-transcript row space, not the
    /// terminal's — scrolling moves the terminal row but never this
    /// index.
    line: usize,

    /// The cell column within the rendered line.
    ///
    /// Terminal cells, so a wide glyph counts two; the selection's
    /// character granularity comes from clipping to the line's width.
    col: usize,
}

/// A tool call's retained full data, for the expanded block.
///
/// What the dispatch-side capture recorded, held on the app past
/// graduation so the expansion outlives the shared store's
/// turnover.
struct ToolDetail {
    /// The call's input, pretty-printed JSON.
    ///
    /// What the dispatch-side capture recorded at call time; the
    /// expansion renders it verbatim below the summary row.
    input_json: String,

    /// The call's output, within the retention cap.
    ///
    /// Truncated to the capture store's bound before it lands here,
    /// so a host rendering the expansion never reparses megabytes.
    output: String,
}

/// A mouse selection between two display cells.
///
/// Order-independent: whichever end the press anchored stays put
/// while the head follows the drag — and the edge-scroll steps that
/// carry it past the viewport — in content coordinates, so scrolling
/// between drags does not move the selection.
struct CellSelection {
    /// The cell the press landed on.
    ///
    /// Stays fixed for the selection's lifetime — the anchor is where
    /// the press began, whichever end of the range it reads as.
    anchor: CellPos,

    /// The cell the drag currently reaches.
    ///
    /// Moves with the drag and the edge-scroll steps that carry it
    /// past the viewport; the copy takes the range it spans.
    head: CellPos,
}

/// The quit lifecycle's two halves.
///
/// Exit is Ctrl+C pressed twice on an empty buffer: the first
/// arms, the second confirms and requests the exit. The press
/// that clears a non-empty buffer arms nothing — clearing is not
/// consenting — and any other key or mouse press disarms, so the
/// chord never fires from stale intent.
#[derive(Debug, Clone, Copy, Default)]
struct QuitState {
    /// Whether one press has armed the exit.
    ///
    /// True only between the empty-buffer chord press and the key
    /// that confirms or disarms it.
    armed: bool,

    /// Whether the second press confirmed it.
    ///
    /// The run loop leaves at the top of its next iteration.
    requested: bool,
}

impl QuitState {
    /// Record a first press.
    ///
    /// Called only for the chord's own spelling on an empty buffer;
    /// the press that clears a draft never reaches here.
    fn arm(&mut self) {
        self.armed = true;
    }

    /// Take back a first press.
    ///
    /// Any key that is not the chord's own spelling disarms it.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

/// The composer caret's blink state.
///
/// The terminal's blinking-cursor request is often ignored or
/// preference-gated, so the app owns the blink: half a second
/// visible, half a second dark. Keyboard input resolidifies the
/// caret and restarts the phase, so typing holds it solid and the
/// blink resumes after half a second of stillness; mouse traffic
/// reads, it does not type, and leaves the phase alone.
struct CaretBlink {
    /// Whether the caret is in its visible phase.
    ///
    /// The frame shows the terminal cursor only while this is true;
    /// the pending-permission and pending-question sheets suppress it.
    on: bool,

    /// Ticks since the phase last flipped.
    ///
    /// Counted against the run loop's tick so the half-second phases
    /// survive frame-rate changes.
    ticks: u32,
}

impl Default for CaretBlink {
    fn default() -> Self {
        Self { on: true, ticks: 0 }
    }
}

impl CaretBlink {
    /// Restart the visible phase — the reaction to any input.
    ///
    /// Keyboard input resolidifies the caret immediately; mouse
    /// traffic never calls this, so reading alone does not hold the
    /// caret solid.
    fn resolidify(&mut self) {
        self.on = true;
        self.ticks = 0;
    }

    /// Advance one tick, flipping the phase when it is due.
    ///
    /// Returns whether the phase flipped, so the caller can redraw —
    /// the only idle activity a blink generates.
    fn tick(&mut self) -> bool {
        self.ticks = self.ticks.saturating_add(1);
        if self.ticks >= CARET_BLINK_TICKS {
            self.ticks = 0;
            self.on = !self.on;
            true
        } else {
            false
        }
    }
}

/// The composer's cached wrap.
///
/// One buffer mutation at one width: the rows the buffer wraps to
/// and the caret's resolved cell among them, kept until the
/// editor's stamp or the pane width moves. The answers a frame's
/// sizing, painting, and status tag all need, computed once per
/// change instead of once per frame.
struct InputWrapCache {
    /// The editor mutation stamp the wrap was computed at.
    ///
    /// The cache holds exactly while this matches the editor's
    /// current stamp; every accepted edit and caret move advances
    /// it, which is what tells an untouched frame to reuse the
    /// grid.
    stamp: u64,

    /// The wrap width the grid was built for.
    ///
    /// A resize re-wraps rather than reuses — wrapped rows are
    /// width-shaped, and a pane of another width would clip rows
    /// baked for the old one.
    width: u16,

    /// The wrapped rows, shared by `Arc`.
    ///
    /// The same grid `display_rows` would rebuild; a cache hit
    /// clones the reference, not the grid — an idle frame pays
    /// neither the wrap nor a copy of its tens of thousands of
    /// rows.
    rows: Arc<Vec<String>>,

    /// The caret's cell on that grid.
    ///
    /// Resolved, the empty buffer's `(0, 0)` parking spot included,
    /// so a hit answers both questions the frame asks without
    /// touching the editor again.
    caret: (u16, u16),
}

/// What the last frame's composer pane looked like.
///
/// The bridge between a press (screen cells, between frames) and the
/// composer's wrap grid: which pane rect belongs to the composer,
/// which interior cell holds the grid's first row and column, and
/// which slice of the grid the pane's text window was showing.
#[derive(Clone, Copy)]
struct InputView {
    /// The composer pane's rect — presses inside belong to it.
    ///
    /// The press-routing claim: a press inside this rect places the
    /// caret, everything outside it falls to the conversation's
    /// selection logic.
    pane: Rect,

    /// The interior cell where the wrap grid's row 0, column 0 sits.
    ///
    /// Press coordinates become grid coordinates by subtracting
    /// this, net of the pane's own side inset.
    origin: (u16, u16),

    /// The wrap-grid row the text window's top row shows.
    ///
    /// The window follows the caret's line; this records which
    /// slice was on screen when the press landed.
    start: usize,

    /// How many wrap rows the text window shows.
    ///
    /// A press below the window's last row clamps into it — the
    /// way editors clamp a click below a document's end.
    visible: usize,
}

/// The facts [`TuiApp::render`] resolves before the frame's rows are
/// split, gathered so the constraint build reads as one act: which
/// optional sections stand this frame, and the heights the fixed
/// furniture carries.
#[derive(Clone, Copy)]
struct FrameLayout {
    area: Rect,
    composer_height: u16,
    /// Where the pulse strip takes its row — above the conversation
    /// or on the spacer beside the composer.
    strip_position: StripPosition,
    /// The question sheet's page the overlay answers, when the sheet
    /// stands: the count ahead, as the overlay's own footer reads it.
    question_page: Option<usize>,
    sections: Sections,
}

/// What the last frame's conversation pane looked like.
///
/// The bridge between mouse events (screen cells, between frames)
/// and the conversation's line indices: the pane's rect and the skip
/// the viewport rendered with. Stale by at most one event while
/// frames are in flight — imperceptible for a drag.
#[derive(Clone, Copy)]
struct ViewState {
    /// The conversation pane's rect, gutter excluded.
    ///
    /// The screen-cell bounds a mouse event is tested against; the
    /// scrollbar's gutter column stays outside so a press there
    /// belongs to nothing.
    area: Rect,

    /// The first conversation line the pane showed.
    ///
    /// The additive base that turns a screen row into a
    /// conversation line index — clicks, drags, and the
    /// edge-scroll all map through it.
    skip: usize,

    /// The conversation's total line count that frame.
    ///
    /// The scroll space's size: running-tool rows are included,
    /// because the viewport scrolls over them even though a
    /// selection cannot cover them.
    total: usize,

    /// The line count a selection can cover — the total without the
    /// running-tool rows, which render below the selectable document
    /// and never join the selection's line space.
    selectable: usize,
}

/// How long all of one copy's process transports may run in total.
///
/// One clock covers the configured command, the platform helper, and
/// the environment-named transports alike, so a hanging helper costs
/// at most this much before the chain kills it and reports.
const COPY_DEADLINE: Duration = Duration::from_millis(1500);

/// The poll step the bounded transport wait sleeps between checks.
///
/// Fine enough that a fast helper's exit is noticed within one step
/// of finishing, coarse enough that the poll thread never competes
/// with the render loop for the core.
const COPY_POLL_STEP: Duration = Duration::from_millis(25);

/// How long a finished transport's writer gets to land its write.
///
/// A well-behaved helper's write completes milliseconds after its
/// exit; the grace covers only that drain, so a writer parked on a
/// pipe some escaped descendant holds is abandoned instead of gating
/// the verdict — the thread leaves whenever the pipe finally closes.
const WRITER_GRACE: Duration = Duration::from_millis(250);

/// What a copy path knows synchronously about a delivery.
///
/// The funnel's immediate phase answers with one of these, drawn from
/// the same vocabulary the drained verdict uses, so the two phases can
/// only agree or upgrade — never contradict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAnswer {
    /// A transport confirmed delivery synchronously.
    ///
    /// Earns the plain `selection copied` claim on the spot; the
    /// injected test recorders report this.
    Verified,

    /// The escape's bytes left, and confirmation is still pending.
    ///
    /// Transports are in flight, so the honest immediate wording is
    /// the unconfirmed one until a verdict lands.
    Unconfirmed,

    /// Nothing reported anything.
    ///
    /// No bytes left and nothing schedulable — the wording names the
    /// escape hatch.
    Failed,
}

/// The notice wording a synchronous copy answer earns.
///
/// Word-for-word the vocabulary [`copy_verdict_notice`] settles on, so
/// the immediate phase never claims more than the phase after it.
#[must_use]
fn copy_answer_notice(answer: CopyAnswer) -> &'static str {
    match answer {
        CopyAnswer::Verified => "selection copied",
        // An in-flight transport chain owes a verdict; saying anything
        // now would flash a claim the next frame replaces — the
        // verdict is the only wording that holds the row.
        CopyAnswer::Unconfirmed => "",
        CopyAnswer::Failed => "copy failed — set display.copy_command",
    }
}

/// The synchronous answer once the escape write is known.
///
/// With nothing schedulable the escape is the whole story — its write
/// settles the answer outright, keeping the no-transports host's
/// immediate wording. With transports scheduled the answer is
/// unconfirmed whatever the escape did: confirmation is theirs to
/// deliver as a verdict.
#[must_use]
fn transport_answer(escape_written: bool, transports_scheduled: bool) -> CopyAnswer {
    match (escape_written, transports_scheduled) {
        (_, true) => CopyAnswer::Unconfirmed,
        (true, false) => CopyAnswer::Verified,
        (false, false) => CopyAnswer::Failed,
    }
}

/// The definitive result of one off-loaded copy attempt.
///
/// The escape write carries no acceptance signal, so only a process
/// transport that exited successfully counts as confirmation; the
/// frame drain maps these facts onto the notice row's wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CopyVerdict {
    /// Which copy attempt this verdict belongs to.
    ///
    /// One monotonic counter per app stamps every copy at the funnel,
    /// so overlapping attempts drain in attempt order — a late verdict
    /// from an older copy cannot overwrite a newer one.
    attempt: u64,

    /// Whether the OSC 52 escape's bytes left stdout.
    ///
    /// True means the handoff happened, never that the receiving
    /// terminal accepted it — the honest ceiling of an escape.
    escape_written: bool,

    /// Whether any process transport exited successfully.
    ///
    /// A transport that launched, consumed the text, and exited zero
    /// is the only delivery the chain can actually confirm.
    verified: bool,
}

/// The shared slot off-loaded copies report their verdicts into.
///
/// One slot per app: the default copier pushes from its background
/// task and the frame drain empties it every frame, so nothing
/// accumulates. Pushes land in whatever order the transports finish;
/// the drain posts the newest attempt's verdict, and only when no
/// newer copy already holds the notice row.
type CopyOutcomes = Arc<std::sync::Mutex<Vec<CopyVerdict>>>;

/// The seam a completed selection's text goes to.
///
/// Receives the copy's attempt id beside the text — the same id any
/// later verdict lands under — and answers synchronously with what
/// the copy path knows; the default answers from the transport chain
/// and a test recorder from its own script.
type SelectionCopier = Box<dyn Fn(u64, &str) -> CopyAnswer>;

/// The notice wording a copy's verdict settles on.
///
/// A verified transport earned the plain claim; an escape that only
/// left the process says so; nothing at all names the escape hatch.
#[must_use]
fn copy_verdict_notice(verdict: CopyVerdict) -> &'static str {
    if verdict.verified {
        "selection copied"
    } else if verdict.escape_written {
        // The escape's bytes left but nothing confirmed delivery —
        // claiming either way would lie, and an empty wording posts
        // no row at all, which is the honest silence.
        ""
    } else {
        "copy failed — set display.copy_command"
    }
}

/// Take the next copy-attempt id from the shared counter.
///
/// One counter per app, shared with the copier: the read-modify-write
/// is a single atomic step, so copies stamping concurrently always
/// take distinct ids, and the first stamp is 1.
fn next_attempt(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1)
}

/// Hand `text` to every clipboard transport the environment offers.
///
/// The terminal-side OSC 52 escape runs inline — one buffered write —
/// and every process transport moves to a background task bounded by
/// one shared kill deadline, so a hanging helper or a child that
/// never reads its stdin cannot freeze input and rendering. The
/// caller-stamped `attempt` rides onto the verdict unchanged. The
/// transports' verdict lands as a follow-up notice through the render
/// wake this pings; until then the returned escape result is all the
/// caller can honestly claim. Failures stay silent per attempt: a
/// copy that cannot be delivered is a nuisance, not an error worth
/// interrupting a session for.
fn copy_via_transports(
    custom: Option<&str>,
    attempt: u64,
    text: &str,
    outcomes: &CopyOutcomes,
    wake: &Arc<event_listener::Event>,
) -> CopyAnswer {
    let escape_written = osc52_write(text);
    let command_line = configured_copy_command(custom).map(str::to_string);
    let transports_scheduled = command_line.is_some()
        || cfg!(target_os = "macos")
        || !host_transports(clipboard_env()).is_empty();
    let answer = transport_answer(escape_written, transports_scheduled);
    if !transports_scheduled {
        return answer;
    }
    let outcomes = Arc::clone(outcomes);
    let wake = Arc::clone(wake);
    let text = text.to_string();
    let job = move || {
        let verified = run_process_transports(command_line.as_deref(), &text, COPY_DEADLINE);
        let mut slot = outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        slot.push(CopyVerdict {
            attempt,
            escape_written,
            verified,
        });
        wake.notify(1);
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        drop(tokio::task::spawn_blocking(job));
    } else {
        drop(std::thread::spawn(job));
    }
    answer
}

/// The configured copy command, blank values dropped.
///
/// A blank or whitespace-only `display.copy_command` is the one-input
/// case of the chain's unset discipline: `sh -c ""` would exit
/// successfully and count as a delivery that delivered nothing.
fn configured_copy_command(custom: Option<&str>) -> Option<&str> {
    custom.filter(|command| !command.trim().is_empty())
}

/// Write the OSC 52 clipboard escape for `text`.
///
/// Both terminator spellings are written — BEL and ST — because a few
/// terminals accept only one of them. The write is the only signal
/// available from this side: acceptance by the receiving terminal is
/// unknowable, so success here means the bytes left and nothing more.
fn osc52_write(text: &str) -> bool {
    use std::io::Write as _;

    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = std::io::stdout();
    stdout
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .is_ok()
        && stdout
            .write_all(format!("\x1b]52;c;{encoded}\x1b\\").as_bytes())
            .is_ok()
        && stdout.flush().is_ok()
}

/// Which host clipboard transports the environment makes possible.
///
/// Snapshotted from the process environment once per copy; a helper
/// that is installed but has no server to talk to (a Wayland tool
/// with no Wayland session) is ruled out by its missing variable, so
/// it never runs just to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClipboardEnv {
    /// Whether `$TMUX` is set: inside tmux, `load-buffer -w` reaches
    /// tmux's own paste buffer and asks the outer terminal for its
    /// clipboard.
    in_tmux: bool,

    /// Whether `$WAYLAND_DISPLAY` is set: a Wayland session can take
    /// the copy through `wl-copy`.
    wayland: bool,

    /// Whether `$DISPLAY` is set: an X session can take the copy
    /// through `xclip` or `xsel`.
    x11: bool,
}

/// A host-side clipboard transport, in attempt order.
///
/// One spawned helper that takes the text on stdin; the variants name
/// both the program and the order the chain tries them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostTransport {
    /// tmux's buffer, pushed onward to the system clipboard.
    ///
    /// `load-buffer -w -` reads the text on stdin — `set-buffer`
    /// would take `-` as its literal data argument and drop the piped
    /// text — fills tmux's paste buffer, and forwards to the attached
    /// terminal's clipboard, covering both paste paths.
    Tmux,

    /// The Wayland clipboard helper.
    ///
    /// `wl-copy` owns the Wayland clipboard selection directly.
    WlCopy,

    /// The X11 clipboard helper, clipboard selection.
    ///
    /// `xclip -selection clipboard` targets the selection a paste
    /// reads by default.
    Xclip,

    /// The X11 clipboard helper, alternate spelling.
    ///
    /// `xsel --clipboard --input` covers hosts that ship xsel rather
    /// than xclip.
    Xsel,
}

impl HostTransport {
    /// The spawned command that carries this transport.
    ///
    /// Built with its fixed arguments; the caller pipes the text into
    /// its stdin and waits out the exit.
    fn command(self) -> std::process::Command {
        match self {
            Self::Tmux => {
                let mut command = std::process::Command::new("tmux");
                command.args(["load-buffer", "-w", "-"]);
                command
            }
            Self::WlCopy => std::process::Command::new("wl-copy"),
            Self::Xclip => {
                let mut command = std::process::Command::new("xclip");
                command.args(["-selection", "clipboard"]);
                command
            }
            Self::Xsel => {
                let mut command = std::process::Command::new("xsel");
                command.args(["--clipboard", "--input"]);
                command
            }
        }
    }
}

/// Read the process environment into a [`ClipboardEnv`].
///
/// A variable that is unset or empty disables its transport: the
/// helper would otherwise run against a display or server that does
/// not exist.
fn clipboard_env() -> ClipboardEnv {
    ClipboardEnv {
        in_tmux: non_empty("TMUX"),
        wayland: non_empty("WAYLAND_DISPLAY") || wayland_socket_present(),
        x11: non_empty("DISPLAY"),
    }
}

/// Whether a Wayland compositor's socket is discoverable without
/// `$WAYLAND_DISPLAY`.
///
/// A tmux server launched from a text console carries no graphical
/// variables into its panes even while a Wayland session runs
/// elsewhere, and a terminal that ignores OSC 52 then turns the
/// tmux-buffer transport into a dead end. libwayland falls back to
/// the conventional `wayland-0` socket under `$XDG_RUNTIME_DIR`, so a
/// socket sitting there is evidence enough that `wl-copy` can reach a
/// compositor; the lock file beside it counts too, since it appears
/// and disappears with the socket.
fn wayland_socket_present() -> bool {
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    wayland_socket_in(std::path::Path::new(&runtime_dir))
}

/// The directory scan behind [`wayland_socket_present`].
///
/// Pure over the directory so the pin can drive it with a fixture;
/// every entry whose name begins `wayland-` — socket or lock — is
/// evidence a compositor is reachable through it.
fn wayland_socket_in(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("wayland-"))
    })
}

/// Whether `name` is set to a non-empty value.
///
/// Empty-valued variables count as unset: an empty `$DISPLAY`, for
/// instance, names no X server to reach.
fn non_empty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// The host transports worth attempting for `env`, in order.
///
/// Selection is pure over the snapshot so the pins can drive it
/// without touching the process environment; the order only matters
/// for spawn cost, since every attempt carries the same text.
fn host_transports(env: ClipboardEnv) -> Vec<HostTransport> {
    let mut transports = Vec::new();
    if env.in_tmux {
        transports.push(HostTransport::Tmux);
    }
    if env.wayland {
        transports.push(HostTransport::WlCopy);
    }
    if env.x11 {
        transports.push(HostTransport::Xclip);
        transports.push(HostTransport::Xsel);
    }
    transports
}

/// Run every process transport under one shared deadline.
///
/// The configured command, the platform helper, and the transports
/// the environment names all draw on the same clock, so the whole
/// chain costs at most the deadline whatever it contains. Returns
/// whether any transport exited successfully; an empty chain returns
/// false without spawning anything.
fn run_process_transports(custom: Option<&str>, text: &str, deadline: Duration) -> bool {
    let start = Instant::now();
    let mut verified = false;
    if let Some(command_line) = custom {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(command_line);
        verified |= run_bounded(command, text, start, deadline);
    }
    if cfg!(target_os = "macos") {
        verified |= run_bounded(std::process::Command::new("pbcopy"), text, start, deadline);
    }
    for transport in host_transports(clipboard_env()) {
        verified |= run_bounded(transport.command(), text, start, deadline);
    }
    verified
}

/// Spawn one transport and bound its whole lifetime.
///
/// The text reaches the child through a writer thread, so a helper
/// that ignores stdin cannot wedge the chain on a full pipe, and the
/// poll loop kills the child's whole process tree once the deadline
/// passes — the worst case is one bounded wait, never an open-ended
/// one. Returns whether the child exited successfully inside the
/// deadline.
fn run_bounded(
    mut command: std::process::Command,
    text: &str,
    start: Instant,
    deadline: Duration,
) -> bool {
    // The group is what makes the deadline's kill reach descendants
    // that inherited the pipe — killing the child alone strands them.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let mut stdin = child.stdin.take();
    let owned = text.to_string();
    let (writer_done, writer_landed) = std::sync::mpsc::channel::<std::io::Result<()>>();
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;

        if let Some(pipe) = stdin.as_mut() {
            drop(writer_done.send(pipe.write_all(owned.as_bytes())));
        }
    });
    let Some(expiry) = start.checked_add(deadline) else {
        kill_transport_tree(&mut child);
        wait_writer_bounded(writer, &writer_landed);
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let succeeded = status.success();
                wait_writer_bounded(writer, &writer_landed);
                return succeeded;
            }
            Ok(None) => {}
            Err(_) => {
                kill_transport_tree(&mut child);
                wait_writer_bounded(writer, &writer_landed);
                return false;
            }
        }
        if Instant::now() >= expiry {
            kill_transport_tree(&mut child);
            wait_writer_bounded(writer, &writer_landed);
            return false;
        }
        std::thread::sleep(COPY_POLL_STEP);
    }
}

/// Kill a transport's whole process tree.
///
/// The child runs as its own process-group leader, so a group signal
/// reaches descendants that inherited the pipe — a backgrounded
/// helper holding stdin dies with its parent and the writer
/// unblocks. Away from Unix the direct child is all that can be
/// reached.
fn kill_transport_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pgid) = i32::try_from(child.id()) {
        // SAFETY: the pgid is the freshly spawned child's own id,
        // not yet reaped, so no pid reuse can retarget the signal;
        // killpg only delivers it.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    drop(child.kill());
}

/// Wait out a transport's writer within its grace, then abandon it.
///
/// Every well-behaved transport's write lands inside the grace —
/// normally in milliseconds — so an exit status is not reported
/// before the write finished. A writer still parked on a pipe some
/// escaped descendant holds cannot be waited out at all: the grace
/// lapses, the join is dropped, and the thread leaves by itself
/// whenever the pipe finally closes. The verdict is never held
/// hostage to it.
fn wait_writer_bounded(
    writer: std::thread::JoinHandle<()>,
    landed: &std::sync::mpsc::Receiver<std::io::Result<()>>,
) {
    if landed.recv_timeout(WRITER_GRACE).is_ok() {
        drop(writer.join());
    } else {
        drop(writer);
    }
}

/// The empty stand-in for the tool segment a selection never covers.
///
/// Running tools render below the selectable document; the window
/// helpers want four segments, and this one is always empty — a
/// `static` so a returned window can borrow it past the call.
static NO_LINES: [Line<'static>; 0] = [];

/// Spell a paste's line breaks the editor's way.
///
/// Terminals differ in how they relay newlines inside a bracketed
/// paste — some send line feeds, some carriage returns, some both.
/// The editor's line model is the line feed, and a carriage return
/// that survives as content welds the whole paste into one logical
/// line (the composer's window then shows only its tail) and later
/// reaches the terminal as a cell the renderer never meant to
/// draw. Both spellings fold here; every other character passes
/// through untouched.
/// A paste's text as the question draft can hold it: one line, nothing
/// discarded.
///
/// The composer keeps pasted newlines because its own buffer is
/// multi-line; a draft answers one question in one row, so the breaks a
/// pasted block carries become the spaces between its parts. Any other
/// control character joins them for the same reason the terminal has no
/// cell for it — a tab or a stray escape has nowhere to draw — and the
/// spaces leave the text readable and re-editable.
fn paste_for_one_line(text: &str) -> String {
    normalize_pasted_newlines(text)
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Whether a key belongs to the app while an overlay owns the screen.
///
/// Paging the transcript and cycling the display verbosity read the
/// conversation; neither answers the question or the permission waiting on
/// top of it, and that conversation is exactly what a user needs in order
/// to decide. Both overlays therefore hold these keys off the sheet and
/// hand them to the machinery below, the way they already hold off the `c`
/// chords: swallowing them would leave a user scrolling for context with a
/// keyboard that answers nothing. The steering chord rides the same rule
/// — the hint row offers it the moment a draft meets a running agent, and
/// an overlay swallowing a chord the interface is advertising would be
/// advertising noise.
fn key_survives_the_sheet(key: KeyEvent) -> bool {
    let chord = key.code == KeyCode::Char('c')
        && (key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    let steering = key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL;
    chord
        || steering
        || matches!(
            key.code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::F(2)
        )
}

fn normalize_pasted_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Order a selection's ends by line then column.
///
/// The anchor stays wherever the press landed and the head follows
/// the drag, so either end can sit above the other; walking the
/// covered span and highlighting it both need the ends sorted.
/// Returns the lower end first, compared as `(line, column)` —
/// the order the rendered line space reads.
fn ordered(selection: &CellSelection) -> (&CellPos, &CellPos) {
    let (a, b) = (&selection.anchor, &selection.head);
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Whether a character's display cells meet the selection's span.
///
/// A character counts as covered when any of its cells does: a
/// selection landing on the second cell of a wide character still
/// picks it up, the way a terminal's native selection treats
/// partially covered ink. The character occupies `cell..cell +
/// width`; `start..=end` are the span's cell bounds.
fn cells_overlap(cell: usize, width: usize, start: usize, end: usize) -> bool {
    cell <= end && cell.saturating_add(width).saturating_sub(1) >= start
}

/// The graphemes of `flat` whose cells meet `start..=end`.
///
/// Walks by display cells and takes every grapheme cluster that
/// overlaps the span — so the copied text matches the highlighted
/// cells exactly. Clusters, not characters: a combining mark adds
/// no cell of its own, so the selection's grid stays the one the
/// renderer painted, and a selected base carries its marks.
fn covered_chars(flat: &str, start: usize, end_inclusive: usize) -> String {
    let mut out = String::new();
    let mut cell = 0;
    for cluster in flat.graphemes(true) {
        let width = cluster.width();
        if cells_overlap(cell, width, start, end_inclusive) {
            out.push_str(cluster);
        }
        cell = cell.saturating_add(width);
        if cell > end_inclusive {
            break;
        }
    }
    out
}

/// Reverse-video the selected cells across the visible lines.
///
/// Splits the affected lines' spans at the selection's cell
/// boundaries and flips exactly the covered characters, so the
/// highlight is character-granular whatever the underlying styles.
fn reverse_selection(lines: &mut [Line<'_>], skip: usize, selection: &CellSelection) {
    let (from, to) = ordered(selection);
    for (offset, line) in lines.iter_mut().enumerate() {
        let line_index = skip.saturating_add(offset);
        if line_index < from.line || line_index > to.line {
            continue;
        }
        let start = if line_index == from.line { from.col } else { 0 };
        let end = if line_index == to.line {
            to.col
        } else {
            usize::MAX
        };
        let mut cell = 0;
        let mut spans = Vec::with_capacity(line.spans.len().saturating_add(2));
        for span in line.spans.drain(..) {
            let content = span.content;
            let mut chunk = String::new();
            for cluster in content.graphemes(true) {
                let width = cluster.width();
                let selected = cells_overlap(cell, width, start, end);
                if selected {
                    if !chunk.is_empty() {
                        spans.push(ratatui::text::Span::styled(
                            std::mem::take(&mut chunk),
                            span.style,
                        ));
                    }
                    spans.push(ratatui::text::Span::styled(
                        cluster.to_string(),
                        span.style.add_modifier(ratatui::style::Modifier::REVERSED),
                    ));
                } else {
                    chunk.push_str(cluster);
                }
                cell = cell.saturating_add(width);
            }
            if !chunk.is_empty() {
                spans.push(ratatui::text::Span::styled(chunk, span.style));
            }
        }
        line.spans = spans;
    }
}

/// Take a shared buffer's contents, leaving it empty.
///
/// Recovers from poisoning — the same policy the observer writes
/// with — so finalized data still graduates after another thread's
/// panic left the lock poisoned.
fn take_locked<T: Default>(mutex: &Mutex<T>) -> T {
    std::mem::take(&mut *mutex.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Collect the viewport window across concatenated line segments.
///
/// The settled conversation renders from a cache that must survive
/// the frame, so instead of concatenating and cloning every segment
/// this walks them with the skip offset and clones only the lines
/// the viewport actually shows — per-frame cost tracks the pane
/// height, not the session length. The segments' borrow and their
/// content are decoupled lifetimes: the caller may assemble the
/// window from transient borrows of lines it keeps beyond them.
fn visible_window<'a>(segments: &[&'a [Line<'a>]], skip: usize, height: usize) -> Vec<Line<'a>> {
    let mut visible = Vec::with_capacity(height.min(64));
    let mut skip = skip;
    for segment in segments {
        if skip >= segment.len() {
            skip = skip.saturating_sub(segment.len());
            continue;
        }
        let take = segment
            .len()
            .saturating_sub(skip)
            .min(height.saturating_sub(visible.len()));
        if let Some(window) = segment.get(skip..skip.saturating_add(take)) {
            visible.extend(window.iter().cloned());
        }
        skip = 0;
        if visible.len() >= height {
            break;
        }
    }
    visible
}

/// Fetch a layout pane by index with a whole-area fallback.
///
/// The three-pane layout always splits into three rects; the
/// fallback keeps rendering total if a shorter split ever appears.
fn pane(chunks: &[Rect], index: usize, fallback: Rect) -> Rect {
    chunks.get(index).copied().unwrap_or(fallback)
}

/// The most body rows any one block of the permission overlay shows
/// before ellipsizing.
///
/// The cap keeps the box short enough to stay inside the border on
/// every terminal tall enough to matter; the prompt is a question and
/// the hints are one line, not a document.
const MAX_OVERLAY_ROWS: usize = 3;

/// Word-wrap `text` to `limit` display columns, hard-splitting any
/// single word wider than `limit`, keeping at most `max_rows` rows
/// (the last carries an ellipsis when truncation happens).
///
/// Rows never exceed `limit`, so a paragraph rendering them inside a
/// border `limit + 2` wide cannot re-flow them.
fn wrap_prompt_rows(text: &str, limit: usize, max_rows: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = word.width();
        if word_width > limit {
            if !current.is_empty() {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            for grapheme in word.graphemes(true) {
                let grapheme_width = grapheme.width();
                if current_width.saturating_add(grapheme_width) > limit {
                    rows.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                current.push_str(grapheme);
                current_width = current_width.saturating_add(grapheme_width);
            }
            continue;
        }
        let joined = if current.is_empty() {
            word_width
        } else {
            current_width.saturating_add(1).saturating_add(word_width)
        };
        if joined <= limit && !current.is_empty() {
            current.push(' ');
            current.push_str(word);
            current_width = joined;
        } else {
            if !current.is_empty() {
                rows.push(std::mem::take(&mut current));
            }
            current.push_str(word);
            current_width = word_width;
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        if let Some(last) = rows.last_mut() {
            let keep = last.width().saturating_sub(1);
            let mut trimmed = String::new();
            let mut kept = 0usize;
            for grapheme in last.graphemes(true) {
                let grapheme_width = grapheme.width();
                if kept.saturating_add(grapheme_width) > keep {
                    break;
                }
                trimmed.push_str(grapheme);
                kept = kept.saturating_add(grapheme_width);
            }
            trimmed.push('…');
            *last = trimmed;
        }
    }
    rows
}

/// A full-width rectangle anchored at the bottom of `area`.
///
/// The permission sheet's geometry: it grows upward from just above
/// the status bar over the rows the composer occupies, so the
/// conversation above and the session line below both stay in view.
fn bottom_sheet_rect(height: u16, area: Rect) -> Rect {
    let height = height.min(area.height);
    Rect {
        x: area.x,
        y: area.y.saturating_add(area.height).saturating_sub(height),
        width: area.width,
        height,
    }
}

/// Split the conversation pane into its text area and scrollbar
/// gutter.
///
/// The gutter is one column off the right edge, reserved whether or
/// not the document scrolls — wrapped text can never collide with
/// the scrollbar, and the wrap width never churns when the document
/// crosses the scrollability threshold. Returns the gutter as `None`
/// only in the degenerate one-column pane, where the text keeps the
/// full width and no scrollbar renders.
fn split_scrollbar_gutter(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < 2 {
        return (area, None);
    }
    let gutter = Rect {
        x: area.right().saturating_sub(1),
        width: 1,
        ..area
    };
    let text = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    (text, Some(gutter))
}

/// Columns of tinted breathing room at each end of the input field's
/// text, inside the fill's edge.
const INPUT_SIDE_INSET: u16 = 2;

/// Blank columns a user turn's box keeps outside itself, between its
/// edges and the pane's.
///
/// The prompt box is a raised surface floating on the terminal's own
/// background, not a band pinned to the pane's edges: this much
/// regular background shows on the box's left and right alike, with
/// the scrollbar's gutter beyond. The chevron sits a column inside
/// the box's left edge, this margin in from the pane's.
const CONVERSATION_BOX_MARGIN: u16 = 1;

/// Columns the conversation reserves at its left edge before its
/// text starts.
///
/// The marker is chrome drawn into the gutter, never span content, so
/// selections and copies walk only the text. The width covers the
/// box's left margin, a pad column, the chevron, and the gap before
/// the text.
const CONVERSATION_MARK_GUTTER: u16 = 5;

/// The narrowest conversation text that still carries the marker
/// gutter beside it.
///
/// Below this the pane keeps its full width and draws no chevrons — a
/// marker that starves the text is worse than no marker.
const CONVERSATION_MARK_MIN_TEXT: u16 = 8;

/// Blank rows the conversation keeps between the pane's top edge and
/// its first line of text.
///
/// The transcript opens this far below the pane's edge — the same
/// breathing-room convention the composer and the status bar use — so
/// the topmost line never touches whatever sits above the pane. The
/// scrollbar track starts below the inset too: the track measures the
/// padded content window, not the raw pane.
const CONVERSATION_TOP_INSET: u16 = 1;

/// Blank columns a user turn's box keeps between its wrapped text and
/// its right edge, inside the box.
///
/// Wrapped user lines stop this far short of the box's edge, the
/// padding the prompt surface carries around its text; agent lines
/// share the stop so every message column aligns, leaving the box's
/// margin and the scrollbar's gutter blank beyond.
const CONVERSATION_RIGHT_INSET: u16 = 3;

/// Rows of text the input field shows at its largest.
///
/// The field starts as a single line and grows one row per line of
/// buffer — typed or wrapped — up to this cap; past it the text
/// window scrolls with the caret and the conversation pane above
/// holds still again.
const INPUT_TEXT_ROWS: u16 = 3;

/// Blank tinted rows of breathing room above and below the field's
/// text rows — the vertical half of the composer's inner padding.
const INPUT_VERTICAL_PADDING: u16 = 1;

/// The scrollbar thumb's track position and length for a document of
/// `content` lines shown through a `viewport`-line window whose top
/// line is `skip` lines into the document.
///
/// Exact integer proportion: the thumb covers the viewport's share
/// of the track — never less than one cell, never more than the
/// track — and sits where the viewport sits in the document, flush
/// with the track's top at the document's start and its bottom at
/// the end. Returns `None` when the document fits the viewport or
/// the viewport has no rows: no thumb, and the caller leaves the
/// gutter blank.
fn scrollbar_geometry(content: usize, viewport: usize, skip: usize) -> Option<(usize, usize)> {
    let scrollable = content.checked_sub(viewport)?;
    let track = viewport;
    if scrollable == 0 || track == 0 {
        return None;
    }
    let thumb = track
        .saturating_mul(viewport)
        .checked_div(content)?
        .clamp(1, track);
    let reach = track.saturating_sub(thumb);
    let position = skip
        .min(scrollable)
        .saturating_mul(reach)
        .checked_div(scrollable)?;
    Some((position, thumb))
}

/// Paint the scrollbar into its gutter as background fills: a
/// bright thumb segment over a dim rail, both in the theme's
/// scrollbar colors.
///
/// Background color is the only solid-fill primitive a terminal
/// guarantees: it covers the whole cell rectangle regardless of
/// font metrics, so consecutive rows join into one unbroken bar.
/// Glyphs cannot do this — on terminals whose cell height exceeds
/// the font's em, every glyph (blocks included) leaves a hairline
/// gap between rows and a glyph column reads as stacked bars.
fn render_scrollbar(
    frame: &mut Frame,
    gutter: Rect,
    thumb_pos: usize,
    thumb_len: usize,
    thumb_color: ratatui::style::Color,
    track_color: ratatui::style::Color,
) {
    for (row, y) in (gutter.y..gutter.bottom()).enumerate() {
        let in_thumb = row >= thumb_pos && row < thumb_pos.saturating_add(thumb_len);
        let color = if in_thumb { thumb_color } else { track_color };
        frame.buffer_mut()[(gutter.x, y)]
            .set_char(' ')
            .set_bg(color);
    }
}

/// How a non-blank line can continue a block across a blank line.
///
/// Only lists and quotes do in the grammar: a blank inside a loose
/// list or a multi-paragraph quote belongs to the block, so the
/// freeze scanner must not settle content there.
#[derive(Clone, Copy, PartialEq)]
enum ContLine {
    /// A `-`/`*`/numbered item line.
    ///
    /// Continues a loose list: the blank line above it separates
    /// items, not blocks, so the list stays open across it.
    List,

    /// A `>`-prefixed quote line.
    ///
    /// Continues a multi-paragraph quote: consecutive `>`-prefixed
    /// lines around a blank belong to one blockquote.
    Quote,

    /// Anything else.
    ///
    /// Starts a fresh block — a blank line above it closes whatever
    /// came before, and the scanner may settle content there.
    Other,
}

/// Classify a non-blank line's cross-blank continuation kind.
///
/// A lexical check, not a parse: leading spaces are skipped and the
/// line's first token is matched against the list and quote markers
/// the grammar accepts. The caller has already established the line
/// is non-blank.
fn cont_line_kind(line: &str) -> ContLine {
    let trimmed = line.trim_start_matches(' ');
    if trimmed.starts_with('>') {
        return ContLine::Quote;
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    let list = trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || (digits > 0
            && trimmed
                .get(digits..digits.saturating_add(2))
                .is_some_and(|marker| marker == ". "));
    if list {
        ContLine::List
    } else {
        ContLine::Other
    }
}

/// The fence marker a line opens or closes, if it is a fence line.
///
/// Leading spaces and trailing line ends are ignored, matching the
/// grammar's `" "*` around fence markers.
fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start_matches(' ');
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// The closing fence a segment still needs, if it ends inside one.
///
/// Tracks fence opens and closes line by line; a fence left open at
/// the segment's end reports its marker so the caller can close it
/// synthetically before parsing, letting in-flight code render as
/// the framed block.
fn open_fence_closer(segment: &str) -> Option<&'static str> {
    let mut open: Option<&'static str> = None;
    for line in segment.split_inclusive('\n') {
        let Some(marker) = fence_marker(line.trim_end_matches(['\n', '\r'])) else {
            continue;
        };
        if open == Some(marker) {
            open = None;
        } else if open.is_none() {
            open = Some(marker);
        }
    }
    open
}

/// The last offset at or after `from` where the buffer's earlier
/// content is settled.
///
/// A safe boundary is the start of a non-blank line that follows a
/// blank line — found outside any code fence, where the non-blank
/// lines flanking the blank cannot continue the same block — or the
/// line after a top-level fence closes: a closed fence cannot be
/// continued, so everything through its closer is settled even
/// without a following blank. A fence opened while a list or quote
/// was in progress is list-nested and gets no close-boundary; the
/// surrounding block may continue past it. Returns `from` when no
/// later boundary exists.
fn last_safe_boundary(buffer: &str, from: usize) -> usize {
    let tail = buffer.get(from..).unwrap_or("");
    let mut boundary = from;
    let mut offset = from;
    let mut fence: Option<&'static str> = None;
    let mut kind_before_fence = ContLine::Other;
    let mut prev_kind = ContLine::Other;
    let mut pending_blank = false;
    for line in tail.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if let Some(marker) = fence_marker(text) {
            if fence == Some(marker) {
                fence = None;
                if kind_before_fence == ContLine::Other {
                    boundary = offset.saturating_add(line.len());
                }
            } else if fence.is_none() {
                fence = Some(marker);
                kind_before_fence = prev_kind;
            }
            prev_kind = ContLine::Other;
            pending_blank = false;
        } else if fence.is_some() || text.trim().is_empty() {
            if fence.is_none() {
                pending_blank = true;
            }
        } else {
            let kind = cont_line_kind(text);
            if pending_blank
                && !matches!(
                    (prev_kind, kind),
                    (ContLine::List, ContLine::List) | (ContLine::Quote, ContLine::Quote)
                )
            {
                boundary = offset;
            }
            prev_kind = kind;
            pending_blank = false;
        }
        offset = offset.saturating_add(line.len());
    }
    boundary
}

/// Bound the markdown part by bytes as well as lines.
///
/// A single line can carry an entire minified block, so the line cap
/// alone leaves the parse unbounded; past the byte cap the excess
/// moves into the plaintext head, cut on a character boundary so the
/// markdown part always starts on one.
fn bound_recent_bytes<'a>(older: &str, recent: &'a str, cap: usize) -> (String, &'a str) {
    if recent.len() <= cap {
        return (older.to_string(), recent);
    }
    let mut cut = recent.len().saturating_sub(cap);
    while !recent.is_char_boundary(cut) {
        cut = cut.saturating_add(1);
    }
    let mut head = String::from(older);
    head.push_str(recent.get(..cut).unwrap_or(""));
    (head, recent.get(cut..).unwrap_or(""))
}

/// Split a slice's leading blank run from its content.
///
/// Returns the run's line count and the remainder. A boundary taken
/// at a closed fence leaves the following blank run at the head of
/// the next slice; that run belongs to the join gap rather than the
/// slice's own render, so the caller folds it into the gap rows.
fn split_blank_prefix(slice: &str) -> (usize, &str) {
    let mut count: usize = 0;
    let mut offset: usize = 0;
    for line in slice.split_inclusive('\n') {
        if !line.trim().is_empty() {
            break;
        }
        count = count.saturating_add(1);
        offset = offset.saturating_add(line.len());
    }
    (count, slice.get(offset..).unwrap_or(""))
}

/// Split the live segment at the parse cap.
///
/// Returns the over-cap head and the trailing cap of lines; the head
/// renders as plaintext and the tail parses as markdown, bounding
/// the per-frame parse while a single long block streams. A segment
/// at or under the cap returns an empty head.
fn split_live_segment(segment: &str, cap: usize) -> (&str, &str) {
    let cap = cap.max(1);
    let split = segment
        .rmatch_indices('\n')
        .nth(cap.saturating_sub(1))
        .map(|(index, _)| index);
    match split {
        Some(index) => {
            let head = segment.get(..index).unwrap_or("");
            let tail = segment.get(index.saturating_add(1)..).unwrap_or("");
            (head, tail)
        }
        None => ("", segment),
    }
}

/// Count the blank lines a frozen slice ends with.
///
/// The blank run before a boundary is the separator the batch render
/// shows between the blocks on either side; the cache re-inserts
/// exactly this many empty rows at the join, however many blank
/// lines the source carried.
fn trailing_blank_lines(slice: &str) -> usize {
    slice
        .lines()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count()
}

/// A cheap identity stamp for a text prefix.
///
/// Within a turn the streaming buffer only grows, but a turn that
/// cleared and refilled the buffer can reach the old frozen length
/// again; the stamp tells that replacement apart from growth where a
/// length comparison cannot.
fn fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The thinking-display preference for the current session.
///
/// Follow is the default: live reasoning renders expanded and
/// auto-collapses when the stream completes. Suppressed renders every
/// stream collapsed from its first delta — the remembered answer to a
/// live-stream collapse, held only for the session.
enum ThinkingMode {
    /// Live reasoning renders expanded and auto-collapses at graduation.
    ///
    /// The default every session starts in; clicking the live row
    /// while expanded switches to
    /// [`Suppressed`](ThinkingMode::Suppressed).
    Follow,

    /// Every reasoning stream renders collapsed from its first delta.
    ///
    /// The remembered answer to a live-stream collapse; clicking the
    /// live row while collapsed switches back to
    /// [`Follow`](ThinkingMode::Follow).
    Suppressed,
}

/// The pulse bar's breathing period, in milliseconds.
///
/// One full animation cycle per period — the centered breath's
/// grow-and-shrink or the sweep's fill-and-clear; the phase is a pure
/// function of elapsed time, so the bar animates smoothly at any
/// frame rate and renders identically in tests.
const PULSE_PERIOD_MS: u64 = 3600;

/// How many live thinking body lines render at most.
///
/// The expanded live body keeps its trailing lines, bounding the
/// per-frame wrap cost of an unbounded reasoning dump; the graduated
/// block keeps everything.
const LIVE_THINKING_BODY_CAP: usize = 500;

/// The placeholder shown for withheld reasoning.
///
/// A redaction marker carries no text; a blank body would read as
/// nothing, so the section says what happened instead.
const THINKING_REDACTED_PLACEHOLDER: &str = "reasoning withheld by the provider";

/// The rows a compaction notice renders as.
///
/// Collapsed, one dim row — the trigger and the before-to-after
/// token counts a scrolling reader needs; expanded, the pass's
/// remaining facts follow beneath. The marker mirrors the thinking
/// sections' shape so one gesture vocabulary covers every
/// expandable row.
fn compaction_notice_lines(
    message: &TuiMessage,
    expanded: bool,
    dim: ratatui::style::Color,
) -> Vec<Line<'static>> {
    let TuiMessage::Compacted {
        tokens_before,
        tokens_after,
        reason,
        evicted,
        duration_ms,
        compactor,
        compression_ratio,
        ..
    } = message
    else {
        return Vec::new();
    };
    let (tokens_before, tokens_after, evicted, duration_ms) =
        (*tokens_before, *tokens_after, *evicted, *duration_ms);
    let compactor = compactor.as_deref();
    let compression_ratio = *compression_ratio;
    let reason = reason.as_str();
    let marker = if expanded { "▾" } else { "▸" };
    let mut lines = vec![Line::styled(
        format!("{marker} Compacted · {}", format_duration(duration_ms)),
        dim,
    )];
    if expanded {
        let saved = tokens_before.saturating_sub(tokens_after);
        lines.push(Line::styled(
            format!("  {tokens_before} → {tokens_after} tokens (saved {saved})"),
            dim,
        ));
        lines.push(Line::styled(format!("  {evicted} messages evicted"), dim));
        lines.push(Line::styled(format!("  reason: {reason}"), dim));
        if let Some(name) = compactor {
            lines.push(Line::styled(format!("  compactor: {name}"), dim));
        }
        if let Some(ratio) = compression_ratio {
            lines.push(Line::styled(format!("  compression {ratio:.1}x"), dim));
        }
        lines.push(Line::styled(
            "  earlier exchanges now live in the summary — durable state is \
             the findings file and todo list"
                .to_string(),
            dim,
        ));
    }
    lines
}

/// Map every row a keyed block produced onto its key.
///
/// The press handlers resolve a screen row to the expandable block
/// it belongs to through these maps; one inserter keeps every
/// keyed block's registration identical.
fn index_summary_rows(
    map: &mut HashMap<usize, String>,
    first: usize,
    after_last: usize,
    key: &str,
) {
    for index in first..after_last {
        map.insert(index, key.to_string());
    }
}

/// Render a millisecond duration in the largest units that fit.
///
/// Under a second renders as `<1s`; seconds render bare; a minute or
/// more renders minutes beside seconds; an hour or more renders
/// hours, minutes, and seconds together — so a reader sees the
/// magnitude at a glance whether the span is a twitch or a marathon.
fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs == 0 {
        "<1s".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m {}s", secs / 3600, secs % 3600 / 60, secs % 60)
    }
}

/// The breathing pulse bar as a fixed-width string with unstyled padding.
///
/// A thin centered run of `─` that grows linearly from the row's
/// center toward both edges and shrinks back over
/// [`PULSE_PERIOD_MS`] — a triangle breath, computed in pure integer
/// arithmetic and centered to the cell: when the padding splits odd,
/// the run sits half a cell right of center. The run never fully
/// disappears: its floor is one cell, so the strip is visible from
/// the run's first frame. Only the run carries the style; the
/// padding around it stays unstyled, so no terminal ever shows
/// styled spaces as a tinted band. The string is exactly `width`
/// characters, filling the span its caller reserves without
/// shifting anything around it.
fn pulse_centered(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let progress = if mark < half {
        mark
    } else {
        period.saturating_sub(mark)
    };
    let len = u64::try_from(width)
        .unwrap_or(0)
        .saturating_mul(progress)
        .checked_div(half)
        .unwrap_or(0);
    let len = usize::try_from(len).unwrap_or(0).min(width);
    let len = len.max(width.min(1));
    let start = width.saturating_sub(len).div_ceil(2);
    let tail = width.saturating_sub(start).saturating_sub(len);
    let mut pulse = String::with_capacity(width);
    for _ in 0..start {
        pulse.push(' ');
    }
    for _ in 0..len {
        pulse.push('─');
    }
    for _ in 0..tail {
        pulse.push(' ');
    }
    pulse
}

/// The sweeping pulse bar as a fixed-width string with unstyled padding.
///
/// The first half of [`PULSE_PERIOD_MS`] fills from the left edge —
/// a one-cell seed growing right until the row is full — and the
/// second half clears left-to-right with the right edge pinned at
/// the row's end, the cleared count rounded up so the row blanks for
/// an instant before the wrap restarts the seed. Like the breath,
/// the phase is a pure function of elapsed time and the string is
/// exactly `width` characters, its run carrying the only style.
fn pulse_sweep(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let mut pulse = String::with_capacity(width);
    if mark < half {
        let len = u64::try_from(width)
            .unwrap_or(0)
            .saturating_mul(mark)
            .checked_div(half)
            .unwrap_or(0);
        let len = usize::try_from(len)
            .unwrap_or(0)
            .min(width)
            .max(width.min(1));
        for _ in 0..len {
            pulse.push('─');
        }
        for _ in 0..width.saturating_sub(len) {
            pulse.push(' ');
        }
    } else {
        let cleared = u64::try_from(width)
            .unwrap_or(0)
            .saturating_mul(mark.saturating_sub(half))
            .div_ceil(half)
            .min(u64::try_from(width).unwrap_or(0));
        let cleared = usize::try_from(cleared).unwrap_or(0).min(width);
        for _ in 0..cleared {
            pulse.push(' ');
        }
        for _ in 0..width.saturating_sub(cleared) {
            pulse.push('─');
        }
    }
    pulse
}

/// The bouncing pulse bar as a fixed-width string with unstyled padding.
///
/// One [`PULSE_PERIOD_MS`] pass sweeps left-to-right — the sweep's
/// fill and clear — and the next sweeps back right-to-left with the
/// geometry mirrored: a right-anchored fill growing from a one-cell
/// seed and a left-pinned clear, the row blanking for an instant at
/// every turn before the next pass begins. Like the other styles,
/// the phase is a pure function of elapsed time and the string is
/// exactly `width` characters, its run carrying the only style.
fn pulse_bounce(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period.saturating_mul(2));
    let forward = mark < period;
    let pass = if forward {
        mark
    } else {
        mark.saturating_sub(period)
    };
    let span = u64::try_from(width).unwrap_or(0);
    let (len, left_pad) = if pass < half {
        let len = span
            .saturating_mul(pass)
            .checked_div(half)
            .unwrap_or(0)
            .min(span);
        let len = usize::try_from(len)
            .unwrap_or(0)
            .min(width)
            .max(width.min(1));
        let pad = if forward {
            0
        } else {
            width.saturating_sub(len)
        };
        (len, pad)
    } else {
        let cleared = span
            .saturating_mul(pass.saturating_sub(half))
            .div_ceil(half)
            .min(span);
        let cleared = usize::try_from(cleared).unwrap_or(0).min(width);
        let len = width.saturating_sub(cleared);
        let pad = if forward { cleared } else { 0 };
        (len, pad)
    };
    let mut pulse = String::with_capacity(width);
    for _ in 0..left_pad {
        pulse.push(' ');
    }
    for _ in 0..len {
        pulse.push('─');
    }
    for _ in 0..width.saturating_sub(left_pad).saturating_sub(len) {
        pulse.push(' ');
    }
    pulse
}

/// The traveling pulse bar as a fixed-width string with unstyled padding.
///
/// A segment a quarter of the row's width (floored at three cells,
/// capped at the row) glides from the left edge to the right over
/// one [`PULSE_PERIOD_MS`] pass and back over the next, the mirror
/// continuing from exactly where the forward pass ended — one
/// continuous scanner motion with the segment always on the row.
/// Like the other styles, the phase is a pure function of elapsed
/// time and the string is exactly `width` characters, its run
/// carrying the only style.
fn pulse_travel(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period.saturating_mul(2));
    let forward = mark < period;
    let pass = if forward {
        mark
    } else {
        mark.saturating_sub(period)
    };
    let len = (width / 4).max(3).min(width);
    let reach = width.saturating_sub(len);
    let pos = u64::try_from(reach)
        .unwrap_or(0)
        .saturating_mul(pass)
        .checked_div(period)
        .unwrap_or(0);
    let pos = usize::try_from(pos).unwrap_or(0).min(reach);
    let left_pad = if forward {
        pos
    } else {
        reach.saturating_sub(pos)
    };
    let mut pulse = String::with_capacity(width);
    for _ in 0..left_pad {
        pulse.push(' ');
    }
    for _ in 0..len {
        pulse.push('─');
    }
    for _ in 0..width.saturating_sub(left_pad).saturating_sub(len) {
        pulse.push(' ');
    }
    pulse
}

/// The circulating pulse bar as a fixed-width string with unstyled padding.
///
/// A segment in travel's geometry — a quarter of the row's width,
/// floored at three cells, capped at the row — advances one full lap
/// around the row per [`PULSE_PERIOD_MS`], covering
/// `[pos, pos + len)` modulo the width: while the tail is still
/// exiting the right edge the head is already entering the left, two
/// runs visible during the crossing and the row never blank. Like
/// the other styles, the phase is a pure function of elapsed time
/// and the string is exactly `width` characters.
fn pulse_snake(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let len = (width / 4).max(3).min(width);
    let pos = u64::try_from(width)
        .unwrap_or(0)
        .saturating_mul(mark)
        .checked_div(period)
        .unwrap_or(0);
    let pos = usize::try_from(pos)
        .unwrap_or(0)
        .min(width.saturating_sub(1));
    let head = pos.saturating_add(len).saturating_sub(width);
    let tail = len.saturating_sub(head);
    let mut pulse = String::with_capacity(width);
    for _ in 0..head {
        pulse.push('─');
    }
    for _ in 0..pos.saturating_sub(head) {
        pulse.push(' ');
    }
    for _ in 0..tail {
        pulse.push('─');
    }
    for _ in 0..width.saturating_sub(pos).saturating_sub(tail) {
        pulse.push(' ');
    }
    pulse
}

/// The worm path's position numerator and its shared denominator.
///
/// The path is the integral of a velocity `2 + 12(2u−1)⁴` — position
/// `(20u + 12(2u−1)⁵ + 12)/44` — racing the outer sections at up to
/// seven times its center crawl and dipping between smoothly. Every
/// progress shares one denominator, so a caller holding two ends
/// rounds their difference once instead of rounding the ends against
/// each other. The quintic terms outgrow `u64` at wide rows, hence
/// `u128`.
fn worm_numer(progress: u64, span: u64) -> (u128, u128) {
    let n = progress.min(span);
    let wide_span = u128::from(span);
    let second = wide_span.saturating_mul(wide_span);
    let fourth = second.saturating_mul(second);
    let fifth = fourth.saturating_mul(wide_span);
    let base = u128::from(n)
        .saturating_mul(20)
        .saturating_mul(fourth)
        .saturating_add(fifth.saturating_mul(12));
    let numer = if n.saturating_mul(2) <= span {
        base.saturating_sub(
            u128::from(span.saturating_sub(n.saturating_mul(2)))
                .saturating_pow(5)
                .saturating_mul(12),
        )
    } else {
        base.saturating_add(
            u128::from(n.saturating_mul(2).saturating_sub(span))
                .saturating_pow(5)
                .saturating_mul(12),
        )
    };
    (numer, fifth.saturating_mul(44).max(1))
}

/// The worm-large path: position numerator and denominator.
///
/// The path is the integral of a velocity `(3 + 6u + 6u²)/8` —
/// position `(3n·span² + 3n²·span + 2n³) / (8·span³)` — moving at
/// three-eighths of the mean speed from the first frame with real
/// acceleration from the start, the pull strengthening through and
/// past the row's center, the far edge arriving at five times the
/// seed's speed. No deceleration anywhere and no branch split; the
/// cubes keep the numerator far inside `u128`.
fn worm_large_numer(progress: u64, span: u64) -> (u128, u128) {
    let n = progress.min(span);
    let wide_span = u128::from(span);
    let wide_n = u128::from(n);
    let squared = wide_n.saturating_mul(wide_n);
    let cubed = squared.saturating_mul(wide_n);
    let span_squared = wide_span.saturating_mul(wide_span);
    (
        wide_n
            .saturating_mul(span_squared)
            .saturating_mul(3)
            .saturating_add(squared.saturating_mul(wide_span).saturating_mul(3))
            .saturating_add(cubed.saturating_mul(2)),
        span_squared
            .saturating_mul(wide_span)
            .saturating_mul(8)
            .max(1),
    )
}

/// Render a worm run between two half-cell edges.
///
/// A cell the run covers fully renders `─`, and one it covers halfway
/// renders that half as a solid tick — `╴` for the left half, `╶` for
/// the right — so the run reads as one line with trimmed ends, its
/// length breathing at most half a cell. An empty run still seeds the
/// run's own leading half-cell tick at the left while seeding — the
/// shape the next frames grow from — so a traversal's first frame
/// never snaps from a full cell to a half.
fn worm_run(head_h: u128, tail_h: u128, width: usize, seeding: bool) -> String {
    let mut pulse = String::with_capacity(width.saturating_mul(3));
    for cell in 0..width {
        let lo = (cell as u128).saturating_mul(2);
        let hi = lo.saturating_add(1);
        let lo_covered = lo >= tail_h && lo < head_h;
        let hi_covered = hi >= tail_h && hi < head_h;
        match (lo_covered, hi_covered) {
            (true, true) => pulse.push('─'),
            (true, false) => pulse.push('╴'),
            (false, true) => pulse.push('╶'),
            (false, false) => pulse.push(' '),
        }
    }
    if seeding && width > 0 && !pulse.contains('─') && !pulse.contains('╴') && !pulse.contains('╶')
    {
        pulse.replace_range(..1, "╴");
    }
    pulse
}

/// The worm pulse bar as a fixed-width string with unstyled padding.
///
/// The bunched inchworm crawl on a shared path: the head crosses the
/// row racing the outer sections and flowing through a center that
/// crawls at a seventh of that speed, and the tail follows the same
/// path a quarter-period behind — the run stretching as one end
/// races away, bunching where the other catches it flowing, and
/// absorbed at the head's settle on the right edge, the row blanking
/// for an instant before the seed reappears at the left; one
/// traversal per [`PULSE_PERIOD_MS`]. Like the other styles, the
/// phase is a pure function of elapsed time and the string is
/// exactly `width` characters, its run carrying the only style.
fn pulse_worm(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let quarter = period / 4;
    let move_span = quarter.saturating_mul(3);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    // A widening cast: `usize` has no `From` into `u128`, and this
    // one can never truncate.
    let wide_width = width as u128;
    let (head_n, den) = worm_numer(mark, move_span);
    let (tail_n, _) = worm_numer(mark.saturating_sub(quarter), move_span);
    let scale = wide_width.saturating_mul(2);
    let head_h = head_n
        .saturating_mul(scale)
        .checked_div(den)
        .unwrap_or(0)
        .min(scale);
    let tail_h = if mark < move_span {
        tail_n.saturating_mul(scale).checked_div(den).unwrap_or(0)
    } else {
        tail_n.saturating_mul(scale).div_ceil(den)
    }
    .min(head_h);
    worm_run(head_h, tail_h, width, mark < move_span)
}

/// The large worm pulse bar as a fixed-width string with unstyled
/// padding.
///
/// The inchworm stretched to the row's full span on its own clock —
/// a little quicker than the shared pulse period, so the long
/// traversal reads as its own tempo — with one traversal spanning two
/// periods: the head rides the accelerating path over the whole first
/// period, setting off gently from the seed and pulling away ever
/// harder through and past the row's center, the run spanning the
/// full row at the period, while the tail rides the same path over
/// the whole second, swallowing the run into the right the same way;
/// each end moves one period and rests one, the tail anchored first,
/// the head settled last. Like
/// the other styles, the phase is a pure function of elapsed time and
/// the string is exactly `width` characters, its run carrying the
/// only style.
fn pulse_worm_large(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2).saturating_mul(3).saturating_div(4);
    let traversal = period.saturating_mul(2);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(traversal);
    // A widening cast: `usize` has no `From` into `u128`, and this
    // one can never truncate.
    let wide_width = width as u128;
    let (head_n, den) = worm_large_numer(mark, period);
    let (tail_n, _) = worm_large_numer(mark.saturating_sub(period), period);
    let scale = wide_width.saturating_mul(2);
    let head_h = head_n
        .saturating_mul(scale)
        .checked_div(den)
        .unwrap_or(0)
        .min(scale);
    let tail_h = if mark < period {
        tail_n.saturating_mul(scale).checked_div(den).unwrap_or(0)
    } else {
        tail_n.saturating_mul(scale).div_ceil(den)
    }
    .min(head_h);
    worm_run(head_h, tail_h, width, mark < period)
}

/// The glow's brightness at an instant, in eighths of full.
///
/// One eased breath per [`PULSE_PERIOD_MS`]: full brightness at the
/// breath's start, easing down to half at the half-breath and back —
/// a smoothstep on each leg, so the turns at the extremes settle
/// gently instead of snapping. A pure function of elapsed time, so
/// the glow renders identically at any frame rate.
fn glow_eighths(began: Instant, now: Instant) -> u64 {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let leg = if mark < half {
        mark
    } else {
        mark.saturating_sub(half)
    };
    // The smoothstep `3x² − 2x³` on the leg's own clock, exact in
    // integers over `half³`.
    let leg_wide = u128::from(leg);
    let half_wide = u128::from(half).max(1);
    let leg_squared = leg_wide.saturating_mul(leg_wide);
    let half_cubed = half_wide
        .saturating_mul(half_wide)
        .saturating_mul(half_wide);
    let step = leg_squared
        .saturating_mul(half_wide)
        .saturating_mul(3)
        .saturating_sub(leg_squared.saturating_mul(leg_wide).saturating_mul(2));
    let eighths = if mark < half {
        8_u128
            .saturating_mul(half_cubed)
            .saturating_sub(step.saturating_mul(4))
    } else {
        4_u128
            .saturating_mul(half_cubed)
            .saturating_add(step.saturating_mul(4))
    }
    .checked_div(half_cubed)
    .unwrap_or(8);
    u64::try_from(eighths).unwrap_or(8)
}

/// A color scaled to eighths of its brightness.
///
/// RGB colors scale channel-wise; every other color returns
/// unchanged, so a theme with a named strip color renders the glow
/// as a plain static bar rather than guessing at named-color
/// distances.
fn scaled_color(color: Color, eighths: u64) -> Color {
    let scale = |channel: u8| -> u8 {
        u8::try_from(
            u64::from(channel)
                .saturating_mul(eighths)
                .checked_div(8)
                .unwrap_or(0),
        )
        .unwrap_or(0)
    };
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(scale(r), scale(g), scale(b)),
        other => other,
    }
}

/// The styled span of a pulse bar: only its runs, unstyled padding around.
///
/// Walking the bar's string — whichever style shaped it — run by run
/// keeps every padding cell free of any style, so the strip paints
/// exactly the bar and never a tinted band of colored spaces, however
/// many runs the style renders. The run class covers the bar's full
/// cell and its half-cell solid ticks alike: all three carry the
/// color.
fn pulse_span(
    style: StripStyle,
    began: Instant,
    now: Instant,
    width: usize,
    color: Color,
) -> Line<'static> {
    let pulse = match style {
        StripStyle::Breathe => pulse_centered(began, now, width),
        StripStyle::Sweep => pulse_sweep(began, now, width),
        StripStyle::Bounce => pulse_bounce(began, now, width),
        StripStyle::Travel => pulse_travel(began, now, width),
        StripStyle::Snake => pulse_snake(began, now, width),
        StripStyle::Worm => pulse_worm(began, now, width),
        StripStyle::WormLong => pulse_worm_large(began, now, width),
        StripStyle::Off => String::new(),
        StripStyle::Glow => std::iter::repeat_n('─', width).collect(),
    };
    let mut spans = Vec::new();
    let mut pad = String::new();
    let mut run = String::new();
    let mut in_run = false;
    for ch in pulse.chars() {
        if ch == '─' || ch == '╴' || ch == '╶' {
            if !in_run && !pad.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut pad)));
            }
            in_run = true;
            run.push(ch);
        } else {
            if in_run && !run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    Style::default().fg(color),
                ));
            }
            in_run = false;
            pad.push(ch);
        }
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, Style::default().fg(color)));
    } else if !pad.is_empty() {
        spans.push(Span::raw(pad));
    }
    Line::from(spans)
}

/// Render one thinking section's lines.
///
/// The summary row — marker, label, duration — always renders in the
/// dim tone; expanded sections add the body as italic dim-wrapped
/// lines, never through the markdown pipeline — the slant and the
/// tone together mark the reasoning as the model's inner voice, not
/// the reply. Withheld reasoning renders the placeholder in place of
/// an empty body, in the same italic register.
fn thinking_section_lines(
    text: &str,
    redacted: bool,
    duration_ms: u64,
    expanded: bool,
    width: usize,
    dim: Color,
) -> Vec<Line<'static>> {
    let marker = if expanded { "▾" } else { "▸" };
    let style = Style::default().fg(dim);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(
            "Thinking",
            style.add_modifier(ratatui::style::Modifier::ITALIC),
        ),
        Span::styled(format!(" · {}", format_duration(duration_ms)), style),
    ])];
    if expanded {
        let body = if redacted && text.is_empty() {
            THINKING_REDACTED_PLACEHOLDER
        } else {
            text
        };
        let body_style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        for segment in body.split('\n') {
            lines.extend(plain_wrapped_lines_styled(segment, width, body_style));
        }
    }
    lines
}

/// Wrap text into styled plaintext lines at a display width.
///
/// The input renders raw: split on newlines and greedily wrapped at
/// the width on character boundaries, styled with the caller's
/// foreground. No parsing, so the cost stays linear in the input and
/// partial markup shows exactly as typed — the properties the
/// streaming tail and the error rows both rely on.
fn plain_wrapped_lines(text: &str, width: usize, fg: ratatui::style::Color) -> Vec<Line<'static>> {
    plain_wrapped_lines_styled(text, width, Style::default().fg(fg))
}

/// Wrap text into styled plaintext lines under a full [`Style`].
///
/// The same greedy wrapping as [`plain_wrapped_lines`]; the style
/// carries modifiers as well as the foreground, so registers like the
/// italic thinking body — color plus slant — pass through intact.
pub(crate) fn plain_wrapped_lines_styled(
    text: &str,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let cap = width.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for ch in raw.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if cap.saturating_sub(current_width) < ch_width && !current.is_empty() {
                lines.push(Line::styled(std::mem::take(&mut current), style));
                current_width = 0;
            }
            current.push(ch);
            current_width = current_width.saturating_add(ch_width);
        }
        lines.push(Line::styled(current, style));
    }
    lines
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        clippy::missing_panics_doc,
        clippy::missing_errors_doc
    )]

    use super::*;

    #[test]
    fn the_auto_expanded_todo_block_folds_when_the_conversation_moves_on() {
        // The plan's inline expansion is momentary: the update
        // expands so the reader sees where the list stands, and the
        // conversation's next step — a reply, another tool's result —
        // folds it back to its summary row. The standing panel carries
        // the list from there.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject.expanded_tools.insert("call_todo".to_string());
        subject.auto_expanded_todo = Some("call_todo".to_string());
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "working on it".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        assert!(
            !subject.expanded_tools.contains("call_todo"),
            "the next graduation folds the auto-expanded block"
        );
        assert!(
            subject.auto_expanded_todo.is_none(),
            "and retires the fold's tracking"
        );
    }

    #[test]
    fn the_auto_expanded_todo_block_folds_when_the_next_tool_starts() {
        // A tool in flight is the conversation moving on even before
        // its result lands: the frame-start settle folds the block the
        // moment something is running.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject.expanded_tools.insert("call_todo".to_string());
        subject.auto_expanded_todo = Some("call_todo".to_string());
        subject.settle_todo_expansion();
        assert!(
            subject.expanded_tools.contains("call_todo"),
            "nothing running — the block still shows the plan"
        );
        subject
            .active_tools()
            .lock()
            .expect("the tools lock")
            .push(crate::message::ActiveTool {
                call_id: "call_next".to_string(),
                name: "Bash".to_string(),
                input_summary: String::new(),
                start: std::time::Instant::now(),
            });
        subject.settle_todo_expansion();
        assert!(
            !subject.expanded_tools.contains("call_todo"),
            "the next tool starting folds it"
        );
    }

    #[test]
    fn an_automatic_fold_on_push_rebakes_off_the_baked_rows() {
        // The sibling of the eviction pin, from the graduation side: a
        // plan's expansion is armed for folding, and the next message
        // folds it. By then the block sits mid-conversation, past the
        // tail the incremental bake re-renders, so the fold has to
        // retire the rows it baked while the flag stood — a collapsed
        // toggle must not keep painting an expanded block.
        let mut subject = app();
        let receipt = [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -1,2 +1,2 @@",
            "  fn main() {",
            "-     old();",
            "+     new();",
            "  }",
        ]
        .join("\n");
        subject.retain_tool_detail("call_todo", "{\"k\":1}", &receipt);
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Tool {
                name: "TodoWrite".to_string(),
                call_id: "call_todo".to_string(),
                input_preview: "src/lib.rs".to_string(),
                output_preview: String::new(),
                retained_input: String::new(),
                success: true,
                elapsed_secs: 0.1,
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        subject.push_message(TuiMessage::User {
            text: "after the block".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
        });
        subject.expanded_tools.insert("call_todo".to_string());
        subject.auto_expanded_todo = Some("call_todo".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(
            expanded.contains("old();"),
            "the armed block draws its expanded detail: {expanded}"
        );
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "working on it".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        assert!(
            !subject.expanded_tools.contains("call_todo"),
            "the next message folds the auto-expanded block"
        );
        terminal
            .draw(|frame| subject.render(frame))
            .expect("redraw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            !collapsed.contains("old();"),
            "the fold rebakes — the baked detail rows do not outlive the \
             flag they rode: {collapsed}"
        );
        assert!(
            collapsed.contains("src/lib.rs"),
            "the block's own summary row stands: {collapsed}"
        );
    }

    #[test]
    fn a_settled_expansion_folds_rebake_off_the_baked_rows() {
        // The same retirement from the frame's head: a tool starting to
        // run folds the plan block, and the rows baked in its expanded
        // shape go with the flag rather than outliving it.
        let mut subject = app();
        let receipt = [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -1,2 +1,2 @@",
            "  fn main() {",
            "-     old();",
            "+     new();",
            "  }",
        ]
        .join("\n");
        subject.retain_tool_detail("call_todo", "{\"k\":1}", &receipt);
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Tool {
                name: "TodoWrite".to_string(),
                call_id: "call_todo".to_string(),
                input_preview: "src/lib.rs".to_string(),
                output_preview: String::new(),
                retained_input: String::new(),
                success: true,
                elapsed_secs: 0.1,
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        subject.push_message(TuiMessage::User {
            text: "after the block".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
        });
        subject.expanded_tools.insert("call_todo".to_string());
        subject.auto_expanded_todo = Some("call_todo".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(
            expanded.contains("old();"),
            "the armed block draws its expanded detail: {expanded}"
        );
        subject
            .active_tools()
            .lock()
            .expect("the tools lock")
            .push(ActiveTool {
                call_id: "call_next".to_string(),
                name: "Bash".to_string(),
                input_summary: String::new(),
                start: Instant::now(),
            });
        subject.settle_todo_expansion();
        assert!(
            !subject.expanded_tools.contains("call_todo"),
            "the tool starting to run folds it"
        );
        terminal
            .draw(|frame| subject.render(frame))
            .expect("redraw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            !collapsed.contains("old();"),
            "the fold rebakes — the baked detail rows do not outlive the \
             flag they rode: {collapsed}"
        );
        assert!(
            collapsed.contains("src/lib.rs"),
            "the block's own summary row stands: {collapsed}"
        );
    }

    #[test]
    fn the_frame_window_opens_on_the_first_draw() {
        // The per-second lane measures from the first draw it counts,
        // not from the draw standing at its close — the difference
        // between a second that can pass and one that never does. So
        // the opening draw seeds the window and says nothing, and the
        // counters go out whole when the window falls.
        let epoch = Instant::now();
        let mut stats = FrameStats {
            anim_wakes: 4,
            ..FrameStats::default()
        };
        assert!(
            stats.note_draw(Duration::from_millis(5), epoch).is_none(),
            "the opening draw seeds the window and reports nothing"
        );
        assert_eq!(stats.draws, 1, "and is counted itself");
        assert!(
            stats
                .note_draw(Duration::from_millis(5), epoch + Duration::from_millis(900))
                .is_none(),
            "a draw inside the second leaves it standing"
        );
        let lane = stats
            .note_draw(
                Duration::from_millis(5),
                epoch + Duration::from_millis(1_100),
            )
            .expect("a draw past the second closes the window");
        assert_eq!(
            (lane.anim_wakes, lane.draws),
            (4, 3),
            "the closed window reports everything it counted"
        );
        assert_eq!(stats.draws, 0, "the reopened window starts empty");
        assert_eq!(stats.anim_wakes, 0, "in every counter");
        assert!(stats.worst_draw.is_none(), "including the worst draw");
        assert_eq!(
            stats.second_start,
            Some(epoch + Duration::from_millis(1_100)),
            "the next window opens at the draw that closed the last"
        );
    }

    #[test]
    fn the_worst_draw_survives_the_second() {
        // The lane carries the slowest draw the window saw, not the
        // draw that happened to be standing when it closed.
        let epoch = Instant::now();
        let mut stats = FrameStats::default();
        stats.note_draw(Duration::from_millis(3), epoch);
        stats.note_draw(
            Duration::from_millis(120),
            epoch + Duration::from_millis(200),
        );
        let lane = stats
            .note_draw(
                Duration::from_millis(4),
                epoch + Duration::from_millis(1_000),
            )
            .expect("the second closes");
        assert_eq!(
            lane.worst_draw_ms, 120,
            "the window's maximum, not its last draw"
        );
    }

    #[test]
    fn a_manual_expansion_outlives_the_auto_fold() {
        // The reader's own expansions are theirs: only the one block
        // the auto-expand armed ever folds.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject.expanded_tools.insert("call_mine".to_string());
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "next step".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        assert!(
            subject.expanded_tools.contains("call_mine"),
            "a manual expansion is never folded by the auto rule"
        );
    }

    #[test]
    fn a_graduation_re_renders_only_the_tail() {
        // The bake is the spinner's autonomy: messages are immutable
        // once a newer one follows, so a graduation re-renders only
        // the messages after the bake — a full-transcript re-render
        // cost seconds on markdown-heavy sessions and froze the frame,
        // spinner included, on every tool result that landed.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        for turn in 0..5 {
            subject.push_message(TuiMessage::User {
                text: format!("prompt {turn}"),
                timestamp: now,
                steered: false,
            });
            subject.push_message(TuiMessage::Assistant {
                blocks: vec![ContentBlock::Text {
                    text: format!("## reply {turn}\n\n```rust\nfn f{turn}() {{}}\n```"),
                }],
                timestamp: now,
                duration_ms: Some(900),
            });
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let baked = subject.conversation_baked_through;
        let prefix: Vec<String> = subject
            .conversation_cache
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(baked, 9, "all but the last message bakes");
        subject.push_message(TuiMessage::User {
            text: "next".to_string(),
            timestamp: now,
            steered: false,
        });
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert_eq!(
            subject.conversation_baked_through, 10,
            "the bake advances by appending — one message re-rendered"
        );
        let after: Vec<String> = subject
            .conversation_cache
            .iter()
            .take(prefix.len())
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(
            prefix, after,
            "the baked prefix is byte-identical across the graduation"
        );
    }

    #[test]
    fn the_incremental_bake_matches_a_full_rebuild() {
        // The optimization may not change what the reader sees: the
        // incrementally baked cache and a from-scratch rebuild of the
        // same conversation render identical lines.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        subject.push_message(TuiMessage::User {
            text: "do it".to_string(),
            timestamp: now,
            steered: false,
        });
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![
                ContentBlock::Text {
                    text: "## done\n\n**bold** and `code`".to_string(),
                },
                ContentBlock::Tool {
                    name: "Read".to_string(),
                    call_id: "c1".to_string(),
                    input_preview: "a.rs".to_string(),
                    success: true,
                    elapsed_secs: 0.5,
                    output_preview: "ok".to_string(),
                    retained_input: String::new(),
                },
            ],
            timestamp: now,
            duration_ms: Some(1200),
        });
        subject.push_message(TuiMessage::User {
            text: "again".to_string(),
            timestamp: now,
            steered: false,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let incremental = subject.conversation_cache.clone();
        // Force a whole-transcript rebake: clear the bake and lie
        // about the generation.
        subject.conversation_cache.clear();
        subject.conversation_bake.clear();
        subject.conversation_baked_through = 0;
        subject.conversation_baked_prev_user = None;
        subject.conversation_cache_generation =
            subject.conversation_cache_generation.wrapping_sub(1);
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let full = subject.conversation_cache.clone();
        assert_eq!(
            texts_of(&incremental),
            texts_of(&full),
            "the bake and the rebuild render identical lines"
        );
    }

    #[test]
    fn a_truncation_below_the_bake_rebakes_whole() {
        // A correction cut removes messages the bake already rendered:
        // the bake cannot keep lines for messages that no longer
        // exist, so a truncation below it resets and rebakes.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        for turn in 0..4 {
            subject.push_message(TuiMessage::User {
                text: format!("p{turn}"),
                timestamp: now,
                steered: false,
            });
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert!(subject.conversation_baked_through >= 3);
        subject.conversation.truncate(2);
        subject.conversation_generation = subject.conversation_generation.wrapping_add(1);
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert_eq!(
            subject.conversation.len(),
            2,
            "the conversation holds the truncated tail"
        );
        let texts: Vec<String> = texts_of(&subject.conversation_cache);
        assert!(
            texts.iter().any(|text| text.contains("p0")),
            "the rebake renders the surviving messages from scratch"
        );
    }

    /// The joined text of rendered lines, for line-by-line compares.
    fn texts_of(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_fresh_session_first_prompt_never_shows_as_queued() {
        // The handoff window: a submit into an idle session is the
        // driver's very next claim, one channel hop away, and the
        // queue section must not draw the prompt as waiting in
        // between — the flash of QUEUED a fresh session's first
        // prompt made. The shared queue still holds the prompt for
        // the driver; only the display pays one fewer row.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        subject.submit_tx = Some(tx);
        subject.submit_text("first prompt".to_string());
        assert_eq!(
            subject.queued_len(),
            1,
            "the driver finds the prompt in the shared queue"
        );
        assert_eq!(
            subject.queued_display_len(),
            0,
            "the handoff window hides the row from the section"
        );
        assert_eq!(
            subject.queue_section_height(60),
            0,
            "no rows to show — no section, no flash"
        );
        // The claim lands: the prompt leaves the queue and the next
        // frame retires the hide.
        let claimed = subject
            .remove_queued(0)
            .expect("the first pushed id is the driver's claim");
        assert_eq!(claimed, "first prompt");
        subject.settle_handed();
        assert!(subject.handed.is_none(), "the window died with the claim");
        // A busy session queues visibly: a run in flight means the
        // new row genuinely waits, and waiting rows show.
        subject
            .state
            .live_claim
            .store(7, std::sync::atomic::Ordering::SeqCst);
        subject.submit_text("second prompt".to_string());
        assert_eq!(
            subject.queued_display_len(),
            1,
            "genuinely waiting rows show under the title"
        );
    }

    fn app() -> TuiApp {
        TuiApp::new(dch_config::DchConfig::default())
    }

    /// The rendered text of a whole buffer, row by row.
    ///
    /// Joins each row's cell symbols so a test can assert on words
    /// the way a reader sees them, not on individual cells.
    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut rows = Vec::new();
        for y in buffer.area.y..buffer.area.bottom() {
            let mut row = String::new();
            for x in buffer.area.x..buffer.area.right() {
                if let Some(cell) = buffer.cell((x, y)) {
                    row.push_str(cell.symbol());
                }
            }
            rows.push(row.trim_end().to_string());
        }
        rows.join("\n")
    }

    #[test]
    fn an_expanded_edit_with_an_undiffable_output_keeps_the_plain_expansion() {
        // The edit family's rows own the diff render only when the
        // parser says diff; an error body or a large-file preview
        // falls back to the plain input/output expansion, and that
        // branch needs its own witness — a claim without a test is
        // how the branch rots.
        let mut subject = app();
        subject.tool_details.insert(
            "call_err".to_string(),
            ToolDetail {
                input_json: "{\"file_path\": \"src/gone.rs\"}".to_string(),
                output: "Edit failed: old_text not found in src/gone.rs".to_string(),
            },
        );
        subject.expanded_tools.insert("call_err".to_string());
        let block = ContentBlock::Tool {
            name: "Edit".to_string(),
            call_id: "call_err".to_string(),
            input_preview: "src/gone.rs".to_string(),
            output_preview: String::new(),
            retained_input: String::new(),
            success: false,
            elapsed_secs: 0.1,
        };
        let mut lines = Vec::new();
        let mut summaries = HashMap::new();
        subject.push_tool_block_lines(&block, 80, &mut lines, &mut summaries, &mut Vec::new());
        let flat: String = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            flat.contains("input:"),
            "the plain expansion renders for an undiffable edit output: {flat}"
        );
        assert!(
            flat.contains("output:"),
            "the raw output is shown verbatim: {flat}"
        );
        assert!(
            !flat.contains("lines changed"),
            "no stats bar renders for an undiffable output: {flat}"
        );
    }

    #[test]
    fn an_edit_s_diff_expands_only_on_live_arrival() {
        // The diff is the point of a live edit row: it renders as it
        // happens, without a click. Restored history keeps its
        // collapsed rows — a resumed marathon of past edits would
        // otherwise bury the prose under walls of bands — and neither
        // a read nor a non-diff output expands anywhere.
        let mut subject = app();
        let receipt = [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -1,2 +1,2 @@",
            "  fn main() {",
            "-     old();",
            "+     new();",
            "  }",
        ]
        .join("\n");
        // The resume-seeding path: details recorded, nothing expanded.
        subject.retain_tool_detail("call_restored", "{\"k\":1}", &receipt);
        assert!(
            !subject.expanded_tools.contains("call_restored"),
            "restored history stays collapsed however diff-shaped"
        );
        // The live graduation path: the same receipt expands itself
        // once the app drains the observer's graduation queue.
        subject
            .state
            .graduations
            .lock()
            .expect("the graduation lock")
            .push(crate::observer::Graduation::Tool(
                crate::observer::ToolResultDisplay {
                    name: "Edit".to_string(),
                    call_id: "call_live".to_string(),
                    is_error: false,
                    duration: std::time::Duration::ZERO,
                    input_summary: "src/lib.rs".to_string(),
                    output_preview: receipt.clone(),
                    full_input: "{\"k\":1}".to_string(),
                    full_output: receipt,
                },
            ));
        subject.drain_shared_state();
        assert!(
            subject.expanded_tools.contains("call_live"),
            "a live parsed edit diff expands its own row"
        );
    }

    #[test]
    fn an_evicted_expansion_rebakes_off_the_baked_rows() {
        // Past the cap the oldest call's expansion retires with its
        // detail, and the rows it baked in the expanded shape must
        // not outlive the flag that backed them: the retirement
        // rebakes, so the next draw renders the block collapsed —
        // mid-conversation, where the tail re-render cannot reach it.
        let mut subject = app();
        let receipt = [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -1,2 +1,2 @@",
            "  fn main() {",
            "-     old();",
            "+     new();",
            "  }",
        ]
        .join("\n");
        subject.retain_tool_detail("call_old", "{\"k\":1}", &receipt);
        subject.expanded_tools.insert("call_old".to_string());
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Tool {
                name: "Edit".to_string(),
                call_id: "call_old".to_string(),
                input_preview: "src/lib.rs".to_string(),
                output_preview: String::new(),
                retained_input: String::new(),
                success: true,
                elapsed_secs: 0.1,
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        subject.push_message(TuiMessage::User {
            text: "after the block".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal.draw(|frame| subject.render(frame)).expect("draw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(
            expanded.contains("old();"),
            "the expanded block draws its diff while the flag stands: {expanded}"
        );
        // TOOL_DETAIL_CAP later graduations retire the oldest call.
        for index in 0..TOOL_DETAIL_CAP {
            subject.retain_tool_detail(&format!("call_{index}"), "{\"k\":1}", "plain output");
        }
        assert!(
            !subject.expanded_tools.contains("call_old"),
            "the cap retired the oldest call's toggle state"
        );
        terminal
            .draw(|frame| subject.render(frame))
            .expect("redraw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            !collapsed.contains("old();"),
            "the retirement rebakes — the baked diff rows do not outlive \
             the flag they rode: {collapsed}"
        );
        assert!(
            collapsed.contains("src/lib.rs"),
            "the block's own summary row stands: {collapsed}"
        );
    }

    #[test]
    fn an_expanded_edit_renders_its_diff_with_real_line_numbers() {
        // The expanded detail of an edit-family call is the parsed
        // diff — banded rows, both gutters — not the raw receipt; the
        // layout follows the config, and an unparseable output keeps
        // the plain input/output expansion.
        let mut config = dch_config::DchConfig::default();
        config.display.diff_layout = dch_config::DiffLayout::Inline;
        let mut subject = TuiApp::new(config);
        let receipt = [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -38,3 +38,3 @@",
            "  fn main() {",
            "-     let old = previous();",
            "+     let new = followup();",
            "  }",
        ]
        .join("\n");
        subject.tool_details.insert(
            "call_diff".to_string(),
            ToolDetail {
                input_json: "{\"file_path\": \"src/lib.rs\"}".to_string(),
                output: receipt,
            },
        );
        subject.expanded_tools.insert("call_diff".to_string());
        let block = ContentBlock::Tool {
            name: "Edit".to_string(),
            call_id: "call_diff".to_string(),
            input_preview: "src/lib.rs".to_string(),
            output_preview: String::new(),
            retained_input: String::new(),
            success: true,
            elapsed_secs: 0.1,
        };
        let mut lines = Vec::new();
        let mut summaries = HashMap::new();
        subject.push_tool_block_lines(&block, 100, &mut lines, &mut summaries, &mut Vec::new());
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        let joined = rendered.join("\n");
        assert!(
            joined.contains("+1") && joined.contains("-1") && joined.contains("2 lines changed"),
            "the stats bar leads the block: {joined}"
        );
        assert!(
            !joined.contains("@@"),
            "no raw hunk header — the rendered rows carry the addresses"
        );
        assert!(
            joined.contains("39") && joined.contains("previous();"),
            "the removal carries its real old-side number: {joined}"
        );
        assert!(
            !joined.contains("input:"),
            "the diff replaces the raw input/output expansion: {joined}"
        );
        assert!(
            subject
                .tool_details
                .get("call_diff")
                .is_some_and(|detail| detail.output.contains("let new")),
            "the detail record itself is untouched by rendering"
        );
    }

    #[test]
    fn a_running_compaction_owns_the_live_lane_like_thinking() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
        *state.health.lock().unwrap() = HealthNotice::Compacting {
            began: std::time::Instant::now(),
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let showing = buffer_text(terminal.backend().buffer());
        assert!(
            showing.contains("▸ Compacting ·"),
            "the running pass renders in the thinking row's register, \
             marker and label and running duration: {showing}"
        );
        assert!(
            !showing.contains("compacting context ("),
            "the old notice-row wording is gone: {showing}"
        );

        *state.health.lock().unwrap() = HealthNotice::Idle;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let cleared = buffer_text(terminal.backend().buffer());
        assert!(
            !cleared.contains("Compacting"),
            "the lane clears with the pass: {cleared}"
        );
    }

    #[test]
    fn a_compaction_notice_renders_collapsed_and_expands_to_its_details() {
        let mut app = app();
        app.push_message(TuiMessage::Compacted {
            tokens_before: 16_686,
            tokens_after: 4_231,
            reason: "threshold".to_string(),
            evicted: 7,
            duration_ms: 312,
            compactor: Some("qa".to_string()),
            compression_ratio: Some(3.9),
            timestamp: chrono::Utc::now(),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            collapsed.contains("Compacted · <1s"),
            "the collapsed row carries the pass duration like every other block: {collapsed}"
        );
        assert!(
            !collapsed.contains("16686"),
            "the token counts wait for the expansion: {collapsed}"
        );
        assert!(!collapsed.contains("messages evicted"));

        app.expanded_compactions.insert("m0".to_string());
        app.conversation_generation = app.conversation_generation.saturating_add(1);
        terminal.draw(|frame| app.render(frame)).expect("redraw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(expanded.contains("16686 → 4231 tokens"), "{expanded}");
        assert!(expanded.contains("saved 12455"), "{expanded}");
        assert!(expanded.contains("7 messages evicted"), "{expanded}");
        assert!(expanded.contains("reason: threshold"), "{expanded}");
        assert!(expanded.contains("compactor: qa"), "{expanded}");
        assert!(expanded.contains("compression 3.9x"), "{expanded}");
        assert!(
            expanded.contains("durable state is the findings file and todo list"),
            "the expanded details remind the reader what survived: {expanded}"
        );
    }

    #[test]
    fn a_baked_compaction_notice_expands_on_its_click() {
        // The notice sits mid-conversation — a later message follows
        // it — so its rows were baked once with the collapsed flag;
        // only the tail re-renders each frame. The click must rebake,
        // else the expansion never shows: the shape the lone-tail pin
        // above cannot distinguish.
        let mut app = app();
        app.push_message(TuiMessage::Compacted {
            tokens_before: 16_686,
            tokens_after: 4_231,
            reason: "threshold".to_string(),
            evicted: 7,
            duration_ms: 312,
            compactor: Some("qa".to_string()),
            compression_ratio: Some(3.9),
            timestamp: chrono::Utc::now(),
        });
        app.push_message(TuiMessage::User {
            text: "after the notice".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            collapsed.contains("Compacted · <1s"),
            "the baked notice draws collapsed: {collapsed}"
        );
        assert!(
            !collapsed.contains("16686"),
            "the token counts wait for the expansion: {collapsed}"
        );
        // The click lands on the notice's own summary row, resolved
        // through the same map and view the handler reads.
        let (&line, _) = app
            .compaction_summary_lines
            .iter()
            .next()
            .expect("the notice registered its summary row");
        let view = app.last_view.expect("the render left its view");
        let row = view
            .area
            .y
            .saturating_add(u16::try_from(line.saturating_sub(view.skip)).expect("row on screen"));
        assert!(
            app.toggle_compaction_at(view.area.x, row),
            "the press belongs to the notice"
        );
        terminal.draw(|frame| app.render(frame)).expect("redraw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(
            expanded.contains("16686 → 4231 tokens"),
            "the click rebakes the notice's rows — a mid-conversation \
             expansion cannot ride the tail re-render: {expanded}"
        );
    }

    #[test]
    fn scrollbar_geometry_is_hidden_when_the_document_fits() {
        assert_eq!(
            scrollbar_geometry(10, 10, 0),
            None,
            "exactly full: no thumb"
        );
        assert_eq!(
            scrollbar_geometry(9, 10, 0),
            None,
            "shorter than the viewport: none"
        );
    }

    #[test]
    fn scrollbar_geometry_pins_flush_at_both_ends() {
        assert_eq!(
            scrollbar_geometry(100, 10, 0),
            Some((0, 1)),
            "at the document's start the thumb is flush with the track's top"
        );
        assert_eq!(
            scrollbar_geometry(100, 10, 90),
            Some((9, 1)),
            "at its end the thumb is flush with the track's bottom"
        );
    }

    #[test]
    fn scrollbar_geometry_sizes_the_thumb_proportionally() {
        assert_eq!(
            scrollbar_geometry(20, 10, 0),
            Some((0, 5)),
            "a document twice the viewport gives the thumb half the track"
        );
        assert_eq!(
            scrollbar_geometry(11, 10, 1),
            Some((1, 9)),
            "one scrollable line gives a near-full thumb at the far end"
        );
        let (mid_pos, thumb) = scrollbar_geometry(100, 10, 45).expect("scrollable");
        assert_eq!(
            (mid_pos, thumb),
            (4, 1),
            "the midpoint sits at the track's middle"
        );
    }

    #[test]
    fn scrollbar_geometry_clamps_an_out_of_range_skip() {
        assert_eq!(
            scrollbar_geometry(100, 10, 999),
            scrollbar_geometry(100, 10, 90),
            "a skip past the end reports the end, never past the track"
        );
    }

    #[test]
    fn the_scrollbar_stays_one_column_wide_at_any_document_length() {
        fn seed_turns(app: &mut TuiApp, count: usize) {
            for _ in 0..count {
                app.push_message(TuiMessage::User {
                    text: "a turn of a few wrapped lines".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                });
            }
        }

        fn thumb_columns(app: &mut TuiApp) -> Vec<u16> {
            let thumb = app.theme.ui.scrollbar_thumb;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24))
                .expect("terminal");
            terminal.draw(|frame| app.render(frame)).expect("draw");
            let buffer = terminal.backend().buffer();
            let mut columns: Vec<u16> = Vec::new();
            for y in 0..24u16 {
                for x in 0..80u16 {
                    if buffer[(x, y)].style().bg == Some(thumb) && !columns.contains(&x) {
                        columns.push(x);
                    }
                }
            }
            columns
        }

        let mut short = app();
        seed_turns(&mut short, 1);
        assert!(
            thumb_columns(&mut short).is_empty(),
            "a conversation that fits draws no scrollbar at all"
        );

        let mut long = app();
        seed_turns(&mut long, 60);
        assert_eq!(
            thumb_columns(&mut long),
            vec![79],
            "a long conversation's scrollbar is exactly the one reserved gutter column"
        );
    }

    #[test]
    fn the_tick_redraws_only_while_tools_run() {
        let app = app();
        assert!(!app.any_tools_running(), "no tools in flight yet");

        app.active_tools()
            .lock()
            .expect("the tools lock")
            .push(ActiveTool {
                call_id: String::new(),
                name: "Grep".to_string(),
                input_summary: "\"needle\"".to_string(),
                start: std::time::Instant::now(),
            });
        assert!(app.any_tools_running(), "a running tool asks for redraws");

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = app.active_tools().lock().expect("the tools lock");
            panic!("poison the tools lock");
        }));
        assert!(panicked.is_err(), "the poisoning panic must unwind");
        assert!(
            !app.any_tools_running(),
            "a poisoned lock reads as no tools in flight"
        );
    }

    #[test]
    fn the_live_lane_opens_with_the_speaker_change_row() {
        // The running turn owes the same blank row under the prompt
        // that the batch render puts between a user message and the
        // assistant content answering it: without it the first running
        // row rides one row tighter than the same content sits after
        // graduating, and the gap opens as a jump the moment it does.
        let mut subject = app();
        subject.push_message(TuiMessage::User {
            text: "go check the files".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
        });
        subject
            .active_tools()
            .lock()
            .expect("the tools lock")
            .push(ActiveTool {
                call_id: "call_seam".to_string(),
                name: "Grep".to_string(),
                input_summary: "\"needle\"".to_string(),
                start: std::time::Instant::now(),
            });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| subject.render(frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24u16)
            .map(|y| {
                (0..80u16)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let prompt = rows
            .iter()
            .position(|row| row.contains("go check the files"))
            .expect("the submitted prompt renders");
        let tool = rows
            .iter()
            .position(|row| row.contains("Grep"))
            .expect("the running tool renders");
        assert_eq!(
            tool.saturating_sub(prompt),
            3,
            "the prompt's closing blank, the speaker-change row, then the \
             running tool — the same air the graduated turn sits in: {rows:?}"
        );
    }

    #[test]
    fn a_runtime_dir_socket_names_wayland_without_the_variable() {
        // The console-launched tmux shape: no $WAYLAND_DISPLAY, but the
        // compositor's socket sits in the runtime dir libwayland
        // already falls back to — the wl-copy transport engages.
        let dir = std::env::temp_dir().join(format!("dch-wayland-pin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("pin dir");
        assert!(
            !wayland_socket_in(&dir),
            "an empty runtime dir offers no wayland"
        );
        std::fs::write(dir.join("wayland-0"), b"").expect("socket marker");
        assert!(
            wayland_socket_in(&dir),
            "the conventional socket names a reachable compositor"
        );
        std::fs::write(dir.join("wayland-1.lock"), b"").expect("lock marker");
        assert!(
            wayland_socket_in(&dir),
            "the lock file beside the socket counts"
        );
        std::fs::remove_dir_all(&dir).expect("pin dir cleaned");
    }

    #[test]
    fn host_transports_follow_the_environment_snapshot() {
        let none = host_transports(ClipboardEnv {
            in_tmux: false,
            wayland: false,
            x11: false,
        });
        assert!(none.is_empty(), "a headless host offers no transports");

        let tmux_only = host_transports(ClipboardEnv {
            in_tmux: true,
            wayland: false,
            x11: false,
        });
        assert_eq!(tmux_only, [HostTransport::Tmux], "tmux is named first");

        let tmux_command = HostTransport::Tmux.command();
        assert_eq!(tmux_command.get_program(), "tmux");
        assert_eq!(
            tmux_command.get_args().collect::<Vec<_>>(),
            ["load-buffer", "-w", "-"],
            "set-buffer would take `-` as literal data and drop the piped text"
        );

        let wayland_only = host_transports(ClipboardEnv {
            in_tmux: false,
            wayland: true,
            x11: false,
        });
        assert_eq!(
            wayland_only,
            [HostTransport::WlCopy],
            "a wayland session names wl-copy"
        );

        let x11_only = host_transports(ClipboardEnv {
            in_tmux: false,
            wayland: false,
            x11: true,
        });
        assert_eq!(
            x11_only,
            [HostTransport::Xclip, HostTransport::Xsel],
            "an x session tries both spellings in order"
        );

        let everything = host_transports(ClipboardEnv {
            in_tmux: true,
            wayland: true,
            x11: true,
        });
        assert_eq!(
            everything,
            [
                HostTransport::Tmux,
                HostTransport::WlCopy,
                HostTransport::Xclip,
                HostTransport::Xsel
            ],
            "the full environment keeps the attempt order"
        );
    }

    #[test]
    fn copy_verdict_wording_covers_all_three_states() {
        assert_eq!(
            copy_verdict_notice(CopyVerdict {
                attempt: 1,
                escape_written: false,
                verified: true,
            }),
            "selection copied",
            "a verified transport earns the plain claim"
        );
        assert_eq!(
            copy_verdict_notice(CopyVerdict {
                attempt: 1,
                escape_written: true,
                verified: false,
            }),
            "",
            "an escape nothing confirmed posts no row at all"
        );
        assert_eq!(
            copy_verdict_notice(CopyVerdict {
                attempt: 1,
                escape_written: false,
                verified: false,
            }),
            "copy failed — set display.copy_command",
            "nothing at all names the escape hatch"
        );
    }

    #[test]
    fn a_blank_copy_command_counts_as_unset() {
        assert_eq!(configured_copy_command(None), None, "unset stays unset");
        assert_eq!(configured_copy_command(Some("")), None, "empty is dropped");
        assert_eq!(
            configured_copy_command(Some("  \t ")),
            None,
            "whitespace-only is dropped"
        );
        assert_eq!(
            configured_copy_command(Some("cat > /dev/null")),
            Some("cat > /dev/null"),
            "a real command passes through as authored"
        );
    }

    #[test]
    fn a_fast_copy_command_verifies_within_the_deadline() {
        let began = std::time::Instant::now();
        assert!(
            run_process_transports(
                Some("cat > /dev/null"),
                "payload",
                Duration::from_millis(500),
            ),
            "a command that consumes stdin and exits zero verifies"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the wait stays bounded"
        );
    }

    #[test]
    fn a_hanging_copy_command_is_killed_at_the_deadline() {
        let began = std::time::Instant::now();
        assert!(
            !run_process_transports(Some("sleep 30"), "payload", Duration::from_millis(150)),
            "a command that outlives the deadline reports no delivery"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the chain returns around the deadline, not the child's lifetime"
        );
    }

    #[test]
    fn a_descendant_holding_stdin_cannot_outlive_the_deadline() {
        let began = std::time::Instant::now();
        assert!(
            !run_process_transports(
                Some("cat > /dev/null & sleep 60"),
                "payload",
                Duration::from_millis(200),
            ),
            "a transport whose descendant holds the pipe reports no delivery"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the group kill plus the writer grace bound the whole chain"
        );
    }

    #[test]
    fn a_drained_verdict_replaces_the_notice_wording() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.post_notice("selection copied".to_string());
        let mut slot = app.copy_outcomes.lock().expect("the outcomes slot");
        slot.push(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: false,
        });
        drop(slot);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert_eq!(
            app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
            Some("selection copied"),
            "an unconfirmed verdict clears no posted wording — the row a \
             verified claim posted stays"
        );
    }

    #[test]
    fn a_late_verdict_from_an_older_copy_cannot_overwrite_a_newer_one() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let mut slot = app.copy_outcomes.lock().expect("the outcomes slot");
        slot.push(CopyVerdict {
            attempt: 2,
            escape_written: true,
            verified: true,
        });
        slot.push(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: false,
        });
        drop(slot);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert_eq!(
            app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
            Some("selection copied"),
            "the newest attempt's wording is the one that shows"
        );
    }

    #[test]
    fn an_older_verdict_cannot_overwrite_a_newer_synchronous_copy() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_selection_copier(Box::new(|_attempt, _text| CopyAnswer::Verified));
        app.copy_selection_and_notice("first");
        app.copy_selection_and_notice("second");
        let mut slot = app.copy_outcomes.lock().expect("the outcomes slot");
        slot.push(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: false,
        });
        drop(slot);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert_eq!(
            app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
            Some("selection copied"),
            "the newest synchronous copy keeps its verified wording"
        );
    }

    #[test]
    fn a_verdict_still_upgrades_its_own_immediate_wording() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_selection_copier(Box::new(|_attempt, _text| CopyAnswer::Unconfirmed));
        app.copy_selection_and_notice("pending");
        let mut slot = app.copy_outcomes.lock().expect("the outcomes slot");
        slot.push(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: true,
        });
        drop(slot);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert_eq!(
            app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
            Some("selection copied"),
            "the same attempt's settled verdict replaces its immediate wording"
        );
    }

    #[test]
    fn concurrent_copies_stamp_one_distinct_attempt_each() {
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let gate = Arc::new(std::sync::Barrier::new(8));
        let stampers: Vec<_> = (0..8)
            .map(|_| {
                let counter = Arc::clone(&counter);
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    (0..1000)
                        .map(|_| next_attempt(&counter))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut stamped = Vec::new();
        for stamper in stampers {
            stamped.extend(stamper.join().expect("the stamper survives"));
        }
        stamped.sort_unstable();
        assert_eq!(
            stamped,
            (1..=8000).collect::<Vec<_>>(),
            "overlapping copies never share an attempt id"
        );
    }

    #[test]
    fn the_copy_answer_wording_matches_the_verdict_vocabulary() {
        assert_eq!(
            copy_answer_notice(CopyAnswer::Verified),
            "selection copied",
            "verified delivery earns the plain claim"
        );
        assert_eq!(
            copy_answer_notice(CopyAnswer::Unconfirmed),
            "",
            "a pending delivery posts nothing — the verdict is the only wording"
        );
        assert_eq!(
            copy_answer_notice(CopyAnswer::Failed),
            "copy failed — set display.copy_command",
            "nothing at all names the escape hatch"
        );
    }

    #[test]
    fn the_synchronous_answer_follows_what_is_schedulable() {
        assert_eq!(
            transport_answer(true, false),
            CopyAnswer::Verified,
            "an escape-only host keeps its immediate wording"
        );
        assert_eq!(
            transport_answer(false, false),
            CopyAnswer::Failed,
            "nothing leaving and nothing schedulable fails outright"
        );
        assert_eq!(
            transport_answer(true, true),
            CopyAnswer::Unconfirmed,
            "transports in flight hold the plain claim back"
        );
        assert_eq!(
            transport_answer(false, true),
            CopyAnswer::Unconfirmed,
            "a failed escape still waits on the transports' verdict"
        );
    }

    #[test]
    fn the_pulse_bar_breathes_symmetrically_from_the_center() {
        let began = Instant::now();
        let width = 20;
        let quarter = pulse_centered(began, began + Duration::from_millis(900), width);
        let three_quarter = pulse_centered(began, began + Duration::from_millis(2700), width);
        // 450 ms in: len 5 against 15 cells of padding — the odd
        // remainder that floors or ceils; the run must take the ceil
        // half on its left.
        let odd = pulse_centered(began, began + Duration::from_millis(450), width);
        for bar in [&quarter, &three_quarter, &odd] {
            assert_eq!(bar.chars().count(), width, "the bar fills its span exactly");
            let left = bar.chars().take_while(|ch| *ch == ' ').count();
            let right = bar.chars().rev().take_while(|ch| *ch == ' ').count();
            assert!(
                left == right || left == right.saturating_add(1),
                "centered to the cell — at most half a cell right: \
                 left {left}, right {right}: {bar:?}"
            );
        }
        assert!(
            odd.chars().take_while(|ch| *ch == ' ').count() == 8,
            "the odd remainder ceils onto the left pad: {odd:?}"
        );
        assert!(quarter.contains('─'), "the growing half carries a run");
        assert!(
            three_quarter.contains('─'),
            "the shrinking half still carries a run"
        );
        let full = pulse_centered(began, began + Duration::from_millis(1800), width);
        assert_eq!(
            full.matches('─').count(),
            width,
            "the cycle's peak fills the whole span"
        );
        let empty = pulse_centered(began, began, width);
        assert_eq!(
            empty.matches('─').count(),
            1,
            "the cycle's start and end hold the one-cell seed, never a blank strip"
        );
    }

    #[test]
    fn an_off_strip_paints_nothing_but_the_wake_stays_up() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut config = dch_config::DchConfig::default();
        config.display.strip_style = dch_config::StripStyle::Off;
        let app = TuiApp::from_observer_state(config, state);
        assert!(
            app.pulse_animation_living(),
            "an off strip still owes the elapsed row its cadence, mid-run"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        let drew = terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let _ = drew;
        let buffer = terminal.backend().buffer();
        for column in 0..80u16 {
            assert!(
                buffer
                    .cell((column, 0))
                    .is_some_and(|cell| cell.symbol() == " "),
                "the off strip leaves the row empty at {column}"
            );
        }
    }

    #[test]
    fn a_glow_breathes_its_color_while_staying_put() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut config = dch_config::DchConfig::default();
        config.display.theme = "dracula".to_string();
        config.display.strip_style = dch_config::StripStyle::Glow;
        let mut app = TuiApp::from_observer_state(config, state);
        let base = app.theme.ui.composer_border.unwrap_or(app.theme.ui.primary);
        assert!(
            matches!(base, ratatui::prelude::Color::Rgb(..)),
            "the dracula strip color is RGB, so the breath is visible"
        );
        let render = |app: &TuiApp| -> (usize, ratatui::prelude::Color) {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
            terminal
                .draw(|frame| {
                    app.render_pulse_strip(
                        frame,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 80,
                            height: 1,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let bar = (0..80u16)
                .filter(|&column| {
                    buffer
                        .cell((column, 0))
                        .is_some_and(|cell| cell.symbol() == "─")
                })
                .count();
            let color = buffer
                .cell((0, 0))
                .map_or(ratatui::prelude::Color::Reset, |cell| cell.fg);
            (bar, color)
        };
        app.run_anchor = Some(Instant::now());
        let (bar, full) = render(&app);
        assert_eq!(bar, 80, "the glow bar holds the full row");
        assert_eq!(full, base, "the breath starts at full brightness");
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(1800))
                .expect("now is past the epoch"),
        );
        let (bar, dim) = render(&app);
        assert_eq!(bar, 80, "the bar never moves");
        assert_eq!(
            dim,
            scaled_color(base, 4),
            "the half-breath sits at half brightness"
        );
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(3600))
                .expect("now is past the epoch"),
        );
        let (bar, wrapped) = render(&app);
        assert_eq!(bar, 80, "the bar stays full through the wrap");
        assert_eq!(wrapped, base, "the wrap returns to full brightness");
    }

    #[test]
    fn a_named_strip_color_glows_without_breathing() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut config = dch_config::DchConfig::default();
        config.display.strip_style = dch_config::StripStyle::Glow;
        let mut app = TuiApp::from_observer_state(config, state);
        let named = ratatui::prelude::Color::Blue;
        app.theme.ui.composer_border = Some(named);
        let render = |app: &TuiApp| -> ratatui::prelude::Color {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
            terminal
                .draw(|frame| {
                    app.render_pulse_strip(
                        frame,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 80,
                            height: 1,
                        },
                    );
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .cell((0, 0))
                .map_or(ratatui::prelude::Color::Reset, |cell| cell.fg)
        };
        app.run_anchor = Some(Instant::now());
        assert_eq!(
            render(&app),
            named,
            "the named color draws as itself at full breath"
        );
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(1800))
                .expect("now is past the epoch"),
        );
        assert_eq!(
            render(&app),
            named,
            "a named color cannot scale, so the glow stays static through the half breath"
        );
    }

    #[test]
    fn the_pulse_bar_runs_on_the_runs_clock_not_the_streams() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let mut app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 900 ms into the run, a stream that began this instant: the
        // bar must follow the run's clock, not the stream's.
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(900))
                .expect("the monotonic clock is well past its epoch"),
        );
        *state.thinking.lock().expect("the thinking lock") = crate::observer::ThinkingStream {
            text: "a stream that just began".to_string(),
            began: Some(Instant::now()),
            ..Default::default()
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let bar = (0..80u16)
            .filter(|&column| {
                buffer
                    .cell((column, 0))
                    .is_some_and(|cell| cell.symbol() == "─")
            })
            .count();
        assert!(
            bar > 8,
            "a run 900 ms old grows the bar wide ({bar} cells), not the one-cell seed \
             a fresh stream's clock would draw"
        );
    }

    #[test]
    fn a_sweep_fills_from_the_left_then_clears_left_to_right() {
        let began = Instant::now();
        let width = 20;
        let seed = pulse_sweep(began, began, width);
        assert_eq!(
            seed.matches('─').count(),
            1,
            "the fill starts from a one-cell seed: {seed:?}"
        );
        assert!(seed.starts_with('─'), "the seed sits at the left edge");
        let quarter = pulse_sweep(began, began + Duration::from_millis(900), width);
        assert_eq!(
            quarter.matches('─').count(),
            10,
            "a quarter cycle fills half the row: {quarter:?}"
        );
        assert!(
            quarter.starts_with('─'),
            "the fill is left-anchored: {quarter:?}"
        );
        let full = pulse_sweep(began, began + Duration::from_millis(1800), width);
        assert_eq!(
            full.matches('─').count(),
            20,
            "the half point fills the whole row"
        );
        let three_quarter = pulse_sweep(began, began + Duration::from_millis(2700), width);
        assert_eq!(
            three_quarter.matches('─').count(),
            10,
            "the clear removes half the row from the left: {three_quarter:?}"
        );
        assert!(
            three_quarter.ends_with('─'),
            "the clear keeps the right edge pinned: {three_quarter:?}"
        );
        assert_eq!(
            three_quarter
                .chars()
                .take(10)
                .filter(|ch| *ch == ' ')
                .count(),
            10,
            "the cleared cells lead: {three_quarter:?}"
        );
        let near_wrap = pulse_sweep(began, began + Duration::from_millis(3599), width);
        assert_eq!(
            near_wrap.matches('─').count(),
            0,
            "the row blanks just before the wrap: {near_wrap:?}"
        );
        let wrapped = pulse_sweep(began, began + Duration::from_millis(3600), width);
        assert_eq!(
            wrapped.matches('─').count(),
            1,
            "the wrap restarts the seed: {wrapped:?}"
        );
        for bar in [&seed, &quarter, &full, &three_quarter, &near_wrap, &wrapped] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    #[test]
    fn the_sweep_renders_left_anchored_when_configured() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let mut config = dch_config::DchConfig::default();
        config.display.strip_style = dch_config::StripStyle::Sweep;
        let mut app = TuiApp::from_observer_state(config, state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(900))
                .expect("the monotonic clock is well past its epoch"),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.cell((0, 0)).expect("the first cell").symbol(),
            "─",
            "the sweep run starts at column 0, where the breath would pad first"
        );
        assert!(
            buffer
                .cell((39, 0))
                .is_some_and(|cell| cell.symbol() == "─"),
            "the fill reached its 900 ms extent"
        );
        assert!(
            buffer
                .cell((45, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "beyond the fill the row stays clear"
        );
    }

    #[test]
    fn a_bounce_sweeps_each_way_in_turn() {
        let began = Instant::now();
        let width = 20;
        let forward_seed = pulse_bounce(began, began, width);
        assert!(
            forward_seed.starts_with('─') && forward_seed.matches('─').count() == 1,
            "the forward pass seeds at the left edge: {forward_seed:?}"
        );
        let forward_half = pulse_bounce(began, began + Duration::from_millis(900), width);
        assert!(
            forward_half.starts_with('─') && forward_half.matches('─').count() == 10,
            "the forward fill grows left-anchored: {forward_half:?}"
        );
        let forward_clear = pulse_bounce(began, began + Duration::from_millis(2700), width);
        assert!(
            forward_clear.ends_with('─') && forward_clear.matches('─').count() == 10,
            "the forward clear keeps the right edge pinned: {forward_clear:?}"
        );
        let turn = pulse_bounce(began, began + Duration::from_millis(3599), width);
        assert_eq!(
            turn.matches('─').count(),
            0,
            "the row blanks for the turn: {turn:?}"
        );
        let reverse_seed = pulse_bounce(began, began + Duration::from_millis(3600), width);
        assert!(
            reverse_seed.ends_with('─') && reverse_seed.matches('─').count() == 1,
            "the reverse pass seeds at the right edge: {reverse_seed:?}"
        );
        let reverse_half = pulse_bounce(began, began + Duration::from_millis(4500), width);
        assert!(
            reverse_half.ends_with('─') && reverse_half.matches('─').count() == 10,
            "the reverse fill grows right-anchored: {reverse_half:?}"
        );
        let reverse_clear = pulse_bounce(began, began + Duration::from_millis(6300), width);
        assert!(
            reverse_clear.starts_with('─') && reverse_clear.matches('─').count() == 10,
            "the reverse clear keeps the left edge pinned: {reverse_clear:?}"
        );
        let back = pulse_bounce(began, began + Duration::from_millis(7199), width);
        assert_eq!(
            back.matches('─').count(),
            0,
            "the row blanks before the round trip wraps: {back:?}"
        );
        let wrapped = pulse_bounce(began, began + Duration::from_millis(7200), width);
        assert!(
            wrapped.starts_with('─') && wrapped.matches('─').count() == 1,
            "the wrap restarts the forward seed: {wrapped:?}"
        );
        for bar in [
            &forward_seed,
            &forward_half,
            &forward_clear,
            &turn,
            &reverse_seed,
            &reverse_half,
            &reverse_clear,
            &back,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    #[test]
    fn the_bounce_renders_right_anchored_on_its_reverse_pass() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let mut config = dch_config::DchConfig::default();
        config.display.strip_style = dch_config::StripStyle::Bounce;
        let mut app = TuiApp::from_observer_state(config, state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(4500))
                .expect("the monotonic clock is well past its epoch"),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.cell((79, 0)).expect("the last cell").symbol(),
            "─",
            "the reverse pass anchors its run at the right edge"
        );
        assert!(
            buffer
                .cell((40, 0))
                .is_some_and(|cell| cell.symbol() == "─"),
            "the reverse fill reached its halfway extent"
        );
        assert!(
            buffer
                .cell((35, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "before the fill the row stays clear"
        );
    }

    #[test]
    fn a_travel_glides_each_way_across_the_row() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let forward_start = pulse_travel(began, began, width);
        assert!(
            leading(&forward_start) == 0 && forward_start.matches('─').count() == 5,
            "the forward pass starts the segment at the left edge: {forward_start:?}"
        );
        let forward_mid = pulse_travel(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&forward_mid) == 7 && forward_mid.matches('─').count() == 5,
            "mid-pass the segment sits mid-row: {forward_mid:?}"
        );
        let reverse_start = pulse_travel(began, began + Duration::from_millis(3600), width);
        assert!(
            leading(&reverse_start) == 15 && reverse_start.ends_with('─'),
            "the reverse pass continues from the right edge: {reverse_start:?}"
        );
        let reverse_mid = pulse_travel(began, began + Duration::from_millis(4500), width);
        assert!(
            leading(&reverse_mid) == 12 && reverse_mid.matches('─').count() == 5,
            "the reverse glide walks the span backwards: {reverse_mid:?}"
        );
        let wrapped = pulse_travel(began, began + Duration::from_millis(7200), width);
        assert!(
            leading(&wrapped) == 0 && wrapped.matches('─').count() == 5,
            "the wrap restarts the forward pass: {wrapped:?}"
        );
        for bar in [
            &forward_start,
            &forward_mid,
            &reverse_start,
            &reverse_mid,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
            assert_eq!(
                bar.matches('─').count(),
                5,
                "the segment keeps its length at every sample"
            );
        }
    }

    #[test]
    fn the_travel_renders_right_anchored_on_its_reverse_pass() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let mut config = dch_config::DchConfig::default();
        config.display.strip_style = dch_config::StripStyle::Travel;
        let mut app = TuiApp::from_observer_state(config, state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(4500))
                .expect("the monotonic clock is well past its epoch"),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .cell((45, 0))
                .is_some_and(|cell| cell.symbol() == "─"),
            "the reverse pass carries the segment past mid-row"
        );
        assert!(
            buffer
                .cell((64, 0))
                .is_some_and(|cell| cell.symbol() == "─"),
            "the segment's right end trails it"
        );
        assert!(
            buffer
                .cell((70, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "clear cells sit beyond the segment"
        );
        assert!(
            buffer
                .cell((20, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "and before it, where the forward pass would have begun at column 0"
        );
    }

    #[test]
    fn a_snake_circulates_through_the_wrap() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let lap_start = pulse_snake(began, began, width);
        assert!(
            leading(&lap_start) == 0 && lap_start.matches('─').count() == 5,
            "the lap starts the segment at the left edge: {lap_start:?}"
        );
        let mid_lap = pulse_snake(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&mid_lap) == 10 && mid_lap.matches('─').count() == 5,
            "mid-lap the segment sits mid-row: {mid_lap:?}"
        );
        let wrapping = pulse_snake(began, began + Duration::from_millis(3150), width);
        assert!(
            wrapping.starts_with('─')
                && wrapping.ends_with('─')
                && wrapping.matches('─').count() == 5,
            "the crossing splits the segment across both edges: {wrapping:?}"
        );
        let near_lap_end = pulse_snake(began, began + Duration::from_millis(3599), width);
        assert!(
            near_lap_end.starts_with('─')
                && near_lap_end.ends_with('─')
                && near_lap_end.matches('─').count() == 5,
            "the segment stays whole in circulation, never blank: {near_lap_end:?}"
        );
        let wrapped = pulse_snake(began, began + Duration::from_millis(3600), width);
        assert!(
            leading(&wrapped) == 0 && wrapped.matches('─').count() == 5,
            "the lap wraps cleanly: {wrapped:?}"
        );
        for bar in [&lap_start, &mid_lap, &wrapping, &near_lap_end, &wrapped] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
            assert_eq!(
                bar.matches('─').count(),
                5,
                "the segment keeps its length at every sample"
            );
        }
    }

    #[test]
    fn the_snake_renders_split_at_the_wrap() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let mut config = dch_config::DchConfig::default();
        config.display.strip_style = dch_config::StripStyle::Snake;
        let mut app = TuiApp::from_observer_state(config, state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(3150))
                .expect("the monotonic clock is well past its epoch"),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer.cell((0, 0)).is_some_and(|cell| cell.symbol() == "─"),
            "the entering head run claims the left edge"
        );
        assert!(
            buffer
                .cell((10, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "clear cells follow the head run"
        );
        assert!(
            buffer
                .cell((69, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "and precede the exiting tail run"
        );
        assert!(
            buffer
                .cell((79, 0))
                .is_some_and(|cell| cell.symbol() == "─"),
            "the exiting tail run holds the right edge"
        );
    }

    #[test]
    fn a_large_worm_stretches_the_full_row_and_folds_back() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let fulls = |bar: &str| bar.matches('─').count();
        let halves = |bar: &str| bar.matches('╴').count() + bar.matches('╶').count();
        let left_seed = pulse_worm_large(began, began, width);
        assert!(
            leading(&left_seed) == 0 && fulls(&left_seed) == 0 && halves(&left_seed) == 1,
            "the run's own half-cell tick seeds the left edge: {left_seed:?}"
        );
        let setting_off = pulse_worm_large(began, began + Duration::from_millis(900), width);
        assert!(
            leading(&setting_off) == 0 && fulls(&setting_off) == 3 && halves(&setting_off) == 1,
            "the head sets off with the acceleration already felt: {setting_off:?}"
        );
        let building = pulse_worm_large(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&building) == 0 && fulls(&building) == 9 && halves(&building) == 1,
            "the head pulls harder as it approaches the row's center: {building:?}"
        );
        let center_crossing = pulse_worm_large(began, began + Duration::from_millis(2400), width);
        assert!(
            leading(&center_crossing) == 0
                && fulls(&center_crossing) == 16
                && halves(&center_crossing) == 0,
            "past the row's center the acceleration is at its steepest so far: {center_crossing:?}"
        );
        let arrived = pulse_worm_large(began, began + Duration::from_millis(2700), width);
        assert!(
            leading(&arrived) == 0
                && fulls(&arrived) == 20
                && halves(&arrived) == 0
                && arrived.starts_with('─')
                && arrived.ends_with('─'),
            "the head reaches the right edge at full sprint: {arrived:?}"
        );
        let chasing = pulse_worm_large(began, began + Duration::from_millis(3599), width);
        assert!(
            leading(&chasing) == 4
                && fulls(&chasing) == 16
                && halves(&chasing) == 0
                && chasing.ends_with('─'),
            "the tail chases the settled head, the run pinned to the right edge: {chasing:?}"
        );
        let building_tail = pulse_worm_large(began, began + Duration::from_millis(4500), width);
        assert!(
            leading(&building_tail) == 10
                && fulls(&building_tail) == 10
                && halves(&building_tail) == 0,
            "the tail pulls harder as it crosses the row's center: {building_tail:?}"
        );
        let folding = pulse_worm_large(began, began + Duration::from_millis(5300), width);
        assert!(
            leading(&folding) == 19
                && fulls(&folding) == 1
                && halves(&folding) == 0
                && folding.ends_with('─'),
            "the tail folds the run home: {folding:?}"
        );
        let gone = pulse_worm_large(began, began + Duration::from_millis(5399), width);
        assert_eq!(
            (fulls(&gone), halves(&gone)),
            (0, 0),
            "the row blanks just before the wrap: {gone:?}"
        );
        let wrapped = pulse_worm_large(began, began + Duration::from_millis(5400), width);
        assert!(
            leading(&wrapped) == 0 && fulls(&wrapped) == 0 && halves(&wrapped) == 1,
            "the wrap restarts the left seed at half-cell width: {wrapped:?}"
        );
        for bar in [
            &left_seed,
            &setting_off,
            &building,
            &center_crossing,
            &arrived,
            &chasing,
            &building_tail,
            &folding,
            &gone,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    #[test]
    fn a_worms_edges_step_in_half_cells_with_their_ends() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let fulls = |bar: &str| bar.matches('─').count();
        let halves = |bar: &str| bar.matches('╴').count() + bar.matches('╶').count();
        let left_seed = pulse_worm(began, began, width);
        assert!(
            leading(&left_seed) == 0 && fulls(&left_seed) == 0 && halves(&left_seed) == 1,
            "the run's own half-cell tick seeds the left edge: {left_seed:?}"
        );
        let stretching = pulse_worm(began, began + Duration::from_millis(900), width);
        assert!(
            leading(&stretching) == 0 && fulls(&stretching) == 8 && halves(&stretching) == 0,
            "the head races toward the center while the tail waits: {stretching:?}"
        );
        let bunched = pulse_worm(began, began + Duration::from_millis(1350), width);
        assert!(
            leading(&bunched) == 6 && fulls(&bunched) == 4 && halves(&bunched) == 0,
            "the tail closes on the head flowing through the center: {bunched:?}"
        );
        let head_half = pulse_worm(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&head_half) == 8
                && fulls(&head_half) == 3
                && halves(&head_half) == 1
                && head_half.contains("───╴"),
            "the head's edge crosses its half — one solid tick trimming the run's end: {head_half:?}"
        );
        let before = pulse_worm(began, began + Duration::from_millis(1799), width);
        assert_eq!(
            (fulls(&before), halves(&before)),
            (fulls(&head_half), halves(&head_half)),
            "adjacent frames hold the bunch steady — each edge rounds alone: {before:?} {head_half:?}"
        );
        let tail_half = pulse_worm(began, began + Duration::from_millis(1950), width);
        assert!(
            leading(&tail_half) == 8
                && fulls(&tail_half) == 3
                && halves(&tail_half) == 1
                && tail_half.contains(" ╶──"),
            "the tail's edge crosses its half beside the center — one solid tick leading the run, no twitch: {tail_half:?}"
        );
        let stretched = pulse_worm(began, began + Duration::from_millis(2250), width);
        assert!(
            leading(&stretched) == 10 && fulls(&stretched) == 3 && halves(&stretched) == 1,
            "the head races right while the tail flows through the center: {stretched:?}"
        );
        let right_full = pulse_worm(began, began + Duration::from_millis(2700), width);
        assert!(
            leading(&right_full) == 12
                && right_full.ends_with('─')
                && fulls(&right_full) == 8
                && halves(&right_full) == 0,
            "the head settles at the right edge, the run pinned to it: {right_full:?}"
        );
        let absorbing = pulse_worm(began, began + Duration::from_millis(3150), width);
        assert!(
            leading(&absorbing) == 14 && fulls(&absorbing) == 6 && halves(&absorbing) == 0,
            "the racing tail clears the run from the left: {absorbing:?}"
        );
        let gone = pulse_worm(began, began + Duration::from_millis(3599), width);
        assert_eq!(
            (fulls(&gone), halves(&gone)),
            (0, 0),
            "the row blanks just before the wrap: {gone:?}"
        );
        let wrapped = pulse_worm(began, began + Duration::from_millis(3600), width);
        assert!(
            leading(&wrapped) == 0 && fulls(&wrapped) == 0 && halves(&wrapped) == 1,
            "the wrap restarts the left seed at half-cell width: {wrapped:?}"
        );
        for bar in [
            &left_seed,
            &stretching,
            &bunched,
            &head_half,
            &before,
            &tail_half,
            &stretched,
            &right_full,
            &absorbing,
            &gone,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    #[test]
    fn the_half_cell_edges_render_beside_the_center() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let mut config = dch_config::DchConfig::default();
        config.display.strip_style = dch_config::StripStyle::Worm;
        let mut app = TuiApp::from_observer_state(config, state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        app.run_anchor = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(1850))
                .expect("the monotonic clock is well past its epoch"),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .cell((33, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "clear cells sit before the run"
        );
        assert!(
            buffer
                .cell((34, 0))
                .is_some_and(|cell| cell.symbol() == "╶"),
            "the tail's solid half-tick leads the run"
        );
        assert!(
            buffer
                .cell((35, 0))
                .is_some_and(|cell| cell.symbol() == "─"),
            "full cells carry the run between the dashes"
        );
        assert!(
            buffer
                .cell((45, 0))
                .is_some_and(|cell| cell.symbol() == "─"),
            "full cells reach the head's half"
        );
        assert!(
            buffer
                .cell((46, 0))
                .is_some_and(|cell| cell.symbol() == "╴"),
            "the head's solid half-tick ends the run"
        );
        assert!(
            buffer
                .cell((47, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "clear cells sit beyond the head"
        );
    }

    #[test]
    fn context_bar_fill_rounds_usage_into_cells() {
        assert_eq!(context_bar_fill(0, 200_000, 10), 0);
        assert_eq!(context_bar_fill(100_000, 200_000, 10), 5);
        assert_eq!(context_bar_fill(20_000, 200_000, 10), 1);
        assert_eq!(context_bar_fill(200_000, 200_000, 10), 10);
        assert_eq!(
            context_bar_fill(300_000, 200_000, 10),
            10,
            "usage past the window clamps at full"
        );
        assert_eq!(
            context_bar_fill(100_000, 0, 10),
            0,
            "an unknown window draws the bar empty"
        );
    }

    #[test]
    fn the_status_bar_reads_the_live_window() {
        let mut app = app();
        app.config.api.context_window = Some(200_000);
        app.state
            .context_window
            .store(100_000, std::sync::atomic::Ordering::SeqCst);
        app.state
            .tokens
            .lock()
            .expect("the tokens lock")
            .context_tokens = 50_000;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let thumb = app.theme.ui.scrollbar_thumb;
        let buffer = terminal.backend().buffer();
        let mut thumb_cells = 0usize;
        for y in 0..24u16 {
            for x in 0..80u16 {
                if buffer[(x, y)].style().bg == Some(thumb) {
                    thumb_cells += 1;
                }
            }
        }
        let expected = context_bar_fill(50_000, 100_000, CONTEXT_BAR_CELLS);
        assert_eq!(
            thumb_cells, expected,
            "the bar divides by the live window, not the config field"
        );
    }

    #[test]
    fn context_bar_readout_rounds_from_the_true_ratio() {
        assert_eq!(
            context_bar_readout(55_000, 100_000),
            Some(("55%".to_string(), "55k/100k".to_string())),
            "the readout spells the same fraction the bar paints"
        );
        assert_eq!(
            context_bar_readout(419_000, 1_000_000),
            Some(("42%".to_string(), "419k/1M".to_string())),
            "a million-strong window spells its unit: 1M, not 1000k"
        );
        assert_eq!(
            context_bar_readout(1_234_000, 2_000_000),
            Some(("62%".to_string(), "1.2M/2M".to_string())),
            "millions in between render in tenths"
        );
        assert_eq!(
            context_bar_readout(999_999, 1_000_000),
            Some(("100%".to_string(), "1M/1M".to_string())),
            "rounding into a million promotes the unit rather than printing 1000k"
        );
        assert_eq!(
            context_bar_readout(2_499, 200_000),
            Some(("1%".to_string(), "2k/200k".to_string())),
            "small usage rounds to its own numbers, not to zero"
        );
        assert_eq!(
            context_bar_readout(250_000, 200_000),
            Some(("125%".to_string(), "250k/200k".to_string())),
            "an over-window session reads its honest over-100 percent"
        );
        assert_eq!(
            context_bar_readout(30_000, 0),
            None,
            "an unknown window spells nothing — no denominator"
        );
    }

    #[test]
    fn the_status_bar_shows_the_context_bar_at_the_configured_fill() {
        let mut app = app();
        app.state
            .tokens
            .lock()
            .expect("the tokens lock")
            .context_tokens = 100_000;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let thumb = app.theme.ui.scrollbar_thumb;
        let buffer = terminal.backend().buffer();
        let mut thumb_cells = 0usize;
        for y in 0..24u16 {
            for x in 0..80u16 {
                if buffer[(x, y)].style().bg == Some(thumb) {
                    thumb_cells += 1;
                }
            }
        }
        assert_eq!(thumb_cells, 8, "half the window fills half of the bar");
        let rows: Vec<String> = (0..24u16)
            .map(|y| {
                (0..80u16)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let numbers_row = rows
            .iter()
            .position(|row| row.contains("100k/200k (50%)"))
            .expect("the totals and their percent spell below the bar");
        let bar_row = u16::try_from(numbers_row.saturating_sub(1)).unwrap_or(0);
        let bar_cells = (0..80u16)
            .filter(|&x| buffer[(x, bar_row)].style().bg == Some(thumb))
            .count();
        assert_eq!(
            bar_cells, 8,
            "the bar rides the row directly above the numbers that spell it"
        );
    }

    #[test]
    fn drain_shared_state_moves_retry_lines_into_the_conversation() {
        let mut app = app();
        let queue = std::sync::Arc::clone(&app.state.retry_lines);
        queue
            .lock()
            .expect("the retry lock")
            .push(crate::observer::RetryLine {
                episode: 0,
                text: "⟳ connection lost — reconnection attempt 1/3".to_string(),
            });
        app.drain_shared_state();
        queue
            .lock()
            .expect("the retry lock")
            .push(crate::observer::RetryLine {
                episode: 0,
                text: "⟳ connection lost — reconnection attempt 2/3".to_string(),
            });
        app.drain_shared_state();
        let retry_rows: Vec<&TuiMessage> = app
            .conversation()
            .iter()
            .filter(|message| matches!(message, TuiMessage::System { .. }))
            .collect();
        assert_eq!(
            retry_rows.len(),
            1,
            "one episode, one row — the second attempt rewrote it: {retry_rows:?}"
        );
        match retry_rows.first().expect("the retry row") {
            TuiMessage::System { text, .. } => {
                assert!(
                    text.contains("reconnection attempt 2/3"),
                    "the row holds the newest attempt: {text}"
                );
            }
            other => panic!("the retry row is a system row: {other:?}"),
        }
        assert!(
            queue.lock().expect("the retry lock").is_empty(),
            "the queue drains"
        );
    }

    #[test]
    fn a_finished_run_stamps_its_total_onto_the_final_reply_only() {
        let mut app = app();
        let mut queue = app.state.graduations.lock().expect("the graduation lock");
        queue.push(crate::observer::Graduation::Reply {
            text: "checking".to_string(),
            thinking: None,
        });
        queue.push(crate::observer::Graduation::Reply {
            text: "done".to_string(),
            thinking: None,
        });
        queue.push(crate::observer::Graduation::RunEnded {
            duration_ms: 84_000,
            success: true,
        });
        drop(queue);
        app.drain_shared_state();
        let durations: Vec<Option<u64>> = app
            .conversation()
            .iter()
            .filter_map(|message| match message {
                TuiMessage::Assistant { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .collect();
        assert_eq!(
            durations,
            vec![None, Some(84_000)],
            "only the run's final reply carries the run's total"
        );
    }

    #[test]
    fn a_run_that_graduated_no_reply_drops_its_total() {
        let mut app = app();
        app.push_message(TuiMessage::User {
            text: "do a thing".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
        });
        app.state
            .graduations
            .lock()
            .expect("the graduation lock")
            .push(crate::observer::Graduation::RunEnded {
                duration_ms: 84_000,
                success: true,
            });
        app.drain_shared_state();
        assert!(
            app.conversation()
                .last()
                .is_some_and(|message| matches!(message, TuiMessage::User { .. })),
            "a run with no reply leaves the conversation alone — no stranger is stamped"
        );
    }

    #[test]
    fn an_assistant_reply_renders_its_duration_as_a_trailing_line() {
        let mut app = app();
        app.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "the measured answer".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: Some(5_424_500),
        });
        app.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "the unmeasured answer".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let showing = buffer_text(terminal.backend().buffer());
        assert!(
            showing.contains("1h 30m 24s"),
            "a measured reply carries its duration on a trailing line: {showing}"
        );
        assert_eq!(
            showing.matches("1h 30m 24s").count(),
            1,
            "an unmeasured reply renders no duration line: {showing}"
        );
    }

    #[test]
    fn durations_render_in_their_largest_fitting_units() {
        assert_eq!(format_duration(0), "<1s");
        assert_eq!(format_duration(999), "<1s");
        assert_eq!(format_duration(1_000), "1s");
        assert_eq!(format_duration(42_500), "42s");
        assert_eq!(format_duration(60_000), "1m 0s");
        assert_eq!(format_duration(84_000), "1m 24s");
        assert_eq!(format_duration(3_600_000), "1h 0m 0s");
        assert_eq!(format_duration(5_424_500), "1h 30m 24s");
    }

    #[test]
    fn a_thinking_section_collapses_to_one_row_and_expands_to_dim_body() {
        let dim = Color::Rgb(1, 2, 3);
        let collapsed = thinking_section_lines("line one\nline two", false, 84_000, false, 40, dim);
        assert_eq!(collapsed.len(), 1, "collapsed renders only the summary row");
        let row = collapsed.first().expect("the summary row");
        let joined: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            joined.contains("▸ Thinking · 1m 24s"),
            "the summary row reads as one label: {joined}"
        );
        assert!(
            row.spans.iter().any(|span| span.content == "Thinking"
                && span
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::ITALIC)),
            "the Thinking word renders italic: {row:?}"
        );

        let expanded = thinking_section_lines("line one\nline two", false, 84_000, true, 40, dim);
        assert_eq!(expanded.len(), 3, "the row plus two body lines");
        for line in &expanded {
            let fg = line
                .style
                .fg
                .or_else(|| line.spans.first().and_then(|span| span.style.fg));
            assert_eq!(fg, Some(dim), "every thinking line carries the dim tone");
        }
        for line in expanded.iter().skip(1) {
            let italic = line
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC)
                || line.spans.iter().any(|span| {
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::ITALIC)
                });
            assert!(italic, "the thinking body renders italic: {line:?}");
        }

        let redacted = thinking_section_lines("", true, 500, true, 40, dim);
        assert!(
            redacted.iter().any(|line| line
                .spans
                .iter()
                .any(|span| span.content.contains(THINKING_REDACTED_PLACEHOLDER))),
            "withheld reasoning renders the placeholder, not a blank body"
        );
    }

    #[test]
    fn the_live_thinking_title_reads_the_running_register() {
        // While the stream runs, the title answers to the same
        // register a running tool's title carries — the default
        // foreground, italic as thinking's identity — while its
        // elapsed span reads the light reply register; the completed
        // block greys both out with everything else.
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.thinking.lock().expect("the thinking lock") = crate::observer::ThinkingStream {
            text: "reasoning in flight".to_string(),
            began: Some(Instant::now()),
            ..Default::default()
        };
        let (lines, _) = app.live_thinking_lines(80);
        let row = lines.first().expect("the marker row");
        let title = row
            .spans
            .iter()
            .find(|span| span.content.contains("Thinking"))
            .expect("the title span");
        assert_eq!(
            title.style.fg, None,
            "the title carries the default foreground while running — \
             the running tool's register, not the dim"
        );
        assert!(
            title
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC),
            "italic stays thinking's identity"
        );
        let elapsed = row
            .spans
            .iter()
            .find(|span| span.content.contains("·"))
            .expect("the elapsed span");
        assert_eq!(
            elapsed.style.fg,
            Some(app.theme.ui.assistant_message_fg),
            "the elapsed reads light while the stream runs — the \
             theme's reply color"
        );
        let marker = row
            .spans
            .iter()
            .find(|span| span.content.contains("▾") || span.content.contains("▸"))
            .expect("the marker span");
        assert_eq!(
            marker.style.fg,
            Some(app.theme.ui.dim),
            "the marker keeps the dim tone"
        );
        // Completed: the transcript's block greys the title out.
        let settled =
            thinking_section_lines("done reasoning", false, 1200, false, 80, app.theme.ui.dim);
        let settled_title = settled
            .first()
            .expect("the settled marker row")
            .spans
            .iter()
            .find(|span| span.content.contains("Thinking"))
            .expect("the settled title");
        assert_eq!(
            settled_title.style.fg,
            Some(app.theme.ui.dim),
            "the completed block's title is greyed out"
        );
    }

    #[test]
    fn the_live_thinking_body_renders_italic_in_the_dim_tone() {
        // The live lane must read the same register the completed
        // sections do: dim plus italic. The default theme here is the
        // transparent one, so the tone asserted is that theme's fixed
        // neutral gray — the deterministic grey-out that keeps
        // reasoning from matching the reply's foreground.
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.thinking.lock().expect("the thinking lock") = crate::observer::ThinkingStream {
            text: "reasoning in flight".to_string(),
            began: Some(Instant::now()),
            ..Default::default()
        };
        let (lines, first_row) = app.live_thinking_lines(80);
        assert_eq!(first_row, Some(0), "the marker row registers for clicks");
        assert!(
            lines.len() >= 2,
            "the marker row plus the expanded body: {lines:?}"
        );
        let joined: String = lines
            .first()
            .expect("the marker row")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            joined.contains("Thinking"),
            "the marker row labels the stream: {joined}"
        );
        for line in lines.iter().skip(1) {
            let fg = line
                .style
                .fg
                .or_else(|| line.spans.first().and_then(|span| span.style.fg));
            assert_eq!(
                fg,
                Some(app.theme.ui.dim),
                "the live body carries the theme's dim tone: {line:?}"
            );
            let italic = line
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC)
                || line.spans.iter().any(|span| {
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::ITALIC)
                });
            assert!(italic, "the live body renders italic: {line:?}");
        }
    }

    #[test]
    fn the_draft_wraps_a_written_answer_with_the_caret_in_it() {
        // The rows a written answer is drawn across, and the caret's place
        // among them, come from the composer's own wrap — the sheet asks the
        // same question the composer asks and gets the same answer.
        let (rows, caret) = crate::input::wrap_rows_and_caret("keep the reply short", 4, 10);
        assert_eq!(
            rows,
            vec!["keep the", "reply", "short"],
            "the draft breaks where the composer breaks the same text"
        );
        assert_eq!(
            caret,
            Some((0, 4)),
            "a caret after a word stands in that word's row, past it"
        );

        let (_rows, caret) = crate::input::wrap_rows_and_caret("keep the reply short", 12, 10);
        assert_eq!(
            caret,
            Some((1, 3)),
            "a caret inside a wrapped row counts cells from that row's own start"
        );
    }

    #[test]
    fn a_caret_after_the_last_character_opens_the_row_it_writes_on() {
        // A caret past the last character takes the next free cell, which is
        // a row of its own when the row it follows has no room left — the
        // cell a user is about to type into is always on screen.
        let (rows, caret) = crate::input::wrap_rows_and_caret("short", 5, 5);
        assert_eq!(rows, vec!["short", ""], "a full row hands the caret a row");
        assert_eq!(caret, Some((1, 0)));

        let (rows, caret) = crate::input::wrap_rows_and_caret("hi", 2, 10);
        assert_eq!(rows, vec!["hi"], "room left on the row means no extra row");
        assert_eq!(caret, Some((0, 2)));
    }

    #[test]
    fn the_steering_chord_survives_the_sheets_and_the_section() {
        // The hint row offers Ctrl+S the moment a draft meets a running
        // agent, and the queue's focus swallows everything the section
        // does not answer — the survival set is what keeps an advertised
        // key from becoming advertised noise.
        assert!(key_survives_the_sheet(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn a_pasted_block_becomes_one_line() {
        // An answer is one line, so the breaks and control characters a
        // pasted block carries arrive as the spaces between its parts.
        assert_eq!(paste_for_one_line("two\r\nlines"), "two lines");
        assert_eq!(paste_for_one_line("two\nlines"), "two lines");
        assert_eq!(paste_for_one_line("tab\there"), "tab here");
        assert_eq!(paste_for_one_line("plain"), "plain");
    }

    #[test]
    fn the_keys_that_belong_to_the_app_survive_the_sheet() {
        // The keys that read the conversation rather than answer the sheet
        // stay the app's, so a user can page for context with a question up.
        for code in [KeyCode::PageUp, KeyCode::PageDown, KeyCode::F(2)] {
            assert!(
                key_survives_the_sheet(KeyEvent::new(code, KeyModifiers::NONE)),
                "{code:?} reads the conversation, not the sheet"
            );
        }
        assert!(key_survives_the_sheet(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        for key in [
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        ] {
            assert!(
                !key_survives_the_sheet(key),
                "{key:?} belongs to the sheet in front of the user"
            );
        }
    }

    /// Rows to spend on the sheet, marked by the block each came from, so
    /// a test can say which rows survived.
    fn block_rows(mark: &str, count: usize) -> Vec<ratatui::text::Line<'static>> {
        (0..count)
            .map(|index| ratatui::text::Line::from(format!("{mark}{index}")))
            .collect()
    }

    #[test]
    fn the_sheet_never_gives_up_the_row_being_written() {
        // The compose block and the key hints are what a user cannot answer
        // without, so they are paid for first and the question's text and the
        // option list are what a short terminal takes from.
        let groups: Vec<(usize, Vec<ratatui::text::Line<'static>>)> =
            (0..2).map(|index| (index, block_rows("o", 1))).collect();
        let (lines, compose_start) = fit_question_sheet(
            block_rows("h", 3),
            &groups,
            block_rows("c", 2),
            block_rows("t", 1),
            0,
            3,
        );
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(compose_start, 0, "the draft starts at the top of the sheet");
        assert_eq!(
            rows,
            vec!["c0", "c1", "t0"],
            "a terminal holding only the untouchable blocks keeps them whole"
        );

        // One more row buys the question's text back before it buys a
        // single option: reading the question outranks reading the list.
        let groups = vec![(0, block_rows("o", 1)), (1, block_rows("o", 1))];
        let (lines, compose_start) = fit_question_sheet(
            block_rows("h", 3),
            &groups,
            block_rows("c", 2),
            block_rows("t", 1),
            0,
            6,
        );
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(compose_start, 3);
        assert_eq!(rows, vec!["h0", "h1", "h2", "c0", "c1", "t0"]);
    }

    #[test]
    fn the_option_window_holds_the_row_the_cursor_stands_on() {
        // The options give way as a window around the row the cursor stands
        // on, never as a truncation that leaves the cursor outside.
        let options: Vec<(usize, Vec<ratatui::text::Line<'static>>)> =
            (0..5).map(|index| (index, block_rows("o", 1))).collect();
        assert_eq!(
            option_window(&options, 4, 1),
            (4, 5),
            "the cursor's own row is the row that survives"
        );
        assert_eq!(
            option_window(&options, 1, 2),
            (1, 3),
            "the window grows below first, so the list reads in the order it was asked"
        );
        assert_eq!(option_window(&options, 2, 0), (0, 0), "nothing to show");
        let tall = vec![(0, block_rows("o", 3)), (1, block_rows("o", 1))];
        assert_eq!(
            option_window(&tall, 0, 2),
            (0, 0),
            "a cursor's group that will not fit leaves the list out entirely"
        );
    }

    #[test]
    fn the_draft_walks_and_erases_by_words() {
        // Editing a written reply follows the composer's line-editor
        // conventions: words are units, and `Delete` erases to the right.
        let mut ui = QuestionUi::default();
        ui.open_composing();
        for ch in "run the lints".chars() {
            ui.type_char(ch);
        }
        assert_eq!(ui.caret, 13);

        ui.caret_word_left();
        assert_eq!(ui.caret, 8, "the caret lands at the word's start");
        ui.caret_word_right();
        assert_eq!(ui.caret, 13, "and walks back the same distance");

        ui.caret_word_left();
        ui.delete_word_left();
        assert_eq!(
            ui.draft, "run lints",
            "one word goes with the space that held it off, and not the line"
        );
        assert_eq!(ui.caret, 4);

        ui.caret_word_left();
        assert_eq!(ui.caret, 0, "the caret stops at the line's start");
        ui.delete_word_left();
        assert_eq!(ui.draft, "run lints", "nothing lies before the start");

        ui.caret_end();
        ui.delete_to_start();
        assert_eq!(
            ui.draft, "",
            "from the end of the line, one press takes everything written"
        );

        for ch in "draft".chars() {
            ui.type_char(ch);
        }
        ui.caret = 0;
        ui.delete_forward();
        assert_eq!(ui.draft, "raft", "Delete erases right of the caret");
        ui.caret_word_right();
        assert_eq!(ui.caret, 4);
        ui.insert_str("s were ");
        assert_eq!(ui.draft, "rafts were ", "a paste lands at the caret");
    }

    #[test]
    fn the_sheet_is_measured_against_the_rows_left_after_the_strip() {
        // The rows the question sheet may occupy: the frame's height less
        // the status bar, less the pulse strip's row when it is configured
        // to the top. The sheet's reservation and its box are both measured
        // from here, which is what keeps the layout's fixed rows from ever
        // summing past the frame the solver was handed.
        use ratatui::layout::Rect;

        let mut app = app();
        let area = Rect::new(0, 0, 40, 12);
        assert_eq!(
            app.question_sheet_area(area).height,
            12u16
                .saturating_sub(STATUS_BAR_ROWS)
                .saturating_add(STATUS_BAR_AIR),
            "only the bar's content rows are spoken for before the sheet sees the frame — its air is margin the sheet stands on"
        );

        app.strip_position = dch_config::StripPosition::Top;
        assert_eq!(
            app.question_sheet_area(area).height,
            12u16
                .saturating_sub(STATUS_BAR_ROWS)
                .saturating_add(STATUS_BAR_AIR)
                .saturating_sub(1),
            "a strip at the top spends a row the sheet is then not offered"
        );
    }

    #[test]
    fn relativize_spells_home_as_tilde_and_deeper_paths_under_it() {
        // The `~/` shape: home itself is the bare tilde, a directory
        // under it keeps its tail, and a directory outside it has no
        // relative spelling at all — the caller shows the full path.
        let home = Path::new("/home/tester");
        assert_eq!(
            relativize(Path::new("/home/tester"), home).as_deref(),
            Some("~"),
            "the home directory itself is the bare tilde"
        );
        assert_eq!(
            relativize(Path::new("/home/tester/repos/dch"), home).as_deref(),
            Some("~/repos/dch"),
            "a directory under home keeps its tail under the tilde"
        );
        assert_eq!(
            relativize(Path::new("/srv/work"), home),
            None,
            "a directory outside home has no relative spelling"
        );
    }
}
