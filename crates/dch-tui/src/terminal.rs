//! Terminal lifecycle for full-screen TUI sessions.
//!
//! Raw mode and the alternate screen are session-wide state a TUI
//! must own for its whole run; the guard here makes teardown happen
//! on every exit path, including panics, so the user's terminal is
//! never left behind in a broken mode.

use std::io::{self, Stdout};

use crossterm::cursor::SetCursorStyle;
use std::io::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::Color;

/// The terminal's default background as this process found it, captured
/// before the session overrode it.
///
/// Set by [`sync_default_background`] only when the terminal answered
/// the background query — and read by [`restore_terminal`] to put the
/// user's own color back on every exit path. Absent when no override
/// was installed, so restore never touches a terminal that was never
/// changed.
static ORIGINAL_BACKGROUND: OnceLock<(u8, u8, u8)> = OnceLock::new();

/// What the session has learned about whether this terminal takes an
/// OSC 11 background override.
///
/// `NotAsked` until a query actually runs: the startup query is
/// skipped whenever input is already waiting (typed-ahead keys are the
/// user's, not ours to eat), so a fast typist can launch a session the
/// query never fired in. The mid-session ask upgrades the record the
/// first time a theme switch needs it — with the event reader
/// quiesced, so the terminal's reply cannot be stolen — and
/// [`BackgroundSupport::Answered`] is what
/// [`finish_theme_burst`]'s canvas write lands against ever after. A
/// terminal that was asked and stayed
/// silent is [`BackgroundSupport::Refused`], and a refused terminal is
/// never asked again: the deferred palette is the honest answer for
/// the session's whole life, exactly as it was at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundSupport {
    /// No query has completed against this terminal yet.
    ///
    /// The session's opening state — the one an ask may upgrade from,
    /// unlike the settled verdicts below.
    NotAsked,

    /// The terminal answered, so the override is proven to land.
    ///
    /// The palette's canvas color is written on theme changes from
    /// here on, and no further proof is ever spent.
    Answered,

    /// The terminal was asked and stayed silent; it will not be asked
    /// again this session.
    ///
    /// The deferred palette is the honest answer from now on — the
    /// terminal keeps whatever canvas it came with.
    Refused,
}

/// The session's [`BackgroundSupport`] verdict — like
/// [`ORIGINAL_BACKGROUND`] in spirit, but upgradable from `NotAsked`,
/// because the answer can arrive after startup.
static BACKGROUND_SUPPORT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The session's current verdict on the terminal's OSC 11 support.
///
/// The display reads this to decide whether a theme change may write
/// the terminal's canvas or must defer to it; the atomic below is the
/// storage, upgraded once per session at most.
#[must_use]
pub fn background_support() -> BackgroundSupport {
    match BACKGROUND_SUPPORT.load(std::sync::atomic::Ordering::SeqCst) {
        1 => BackgroundSupport::Answered,
        2 => BackgroundSupport::Refused,
        _ => BackgroundSupport::NotAsked,
    }
}

/// Whether a live switch should ask the terminal now: only a session
/// that never got an answer — skipped at startup, never queried since
/// — asks. A proven terminal writes; a refused one is never asked
/// again.
pub(crate) fn next_ask_warranted(support: BackgroundSupport) -> bool {
    support == BackgroundSupport::NotAsked
}

/// Record what a query taught, never downgrading an answer once it
/// lands.
fn record_support(support: BackgroundSupport) {
    let code = match support {
        BackgroundSupport::NotAsked => 0,
        BackgroundSupport::Answered => 1,
        BackgroundSupport::Refused => 2,
    };
    if BACKGROUND_SUPPORT.load(std::sync::atomic::Ordering::SeqCst) != 1 {
        BACKGROUND_SUPPORT.store(code, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The terminal's line discipline as this process found it, captured
/// before raw mode was first enabled.
///
/// The fatal-signal handler cannot call crossterm to stand raw mode
/// down — crossterm guards its saved state behind a lock a signal must
/// never block on — so it replays this captured copy directly through
/// `tcsetattr` instead. Absent when the capture failed, in which case
/// the handler still writes the mode teardown sequences and exits;
/// a shell-side `reset` then covers the residue.
#[cfg(unix)]
static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// The exact bytes the fatal-signal handler writes to restore the
/// terminal, built once at arm time from the state the session had
/// then: the original background when one was captured, the mode
/// teardown, and the alternate screen exit.
#[cfg(unix)]
static SIGNAL_RESTORE_BYTES: OnceLock<Vec<u8>> = OnceLock::new();

/// The SIGINT handler action this process replaced when it armed the
/// fatal-signal restore, as a raw `sa_sigaction` value; a sentinel
/// (`SIG_DFL` = 0 or `SIG_IGN` = 1) when the arm found no real
/// handler to chain to.
#[cfg(unix)]
static CHAINED_SIGINT: AtomicUsize = AtomicUsize::new(0);

/// The SIGTERM counterpart of [`CHAINED_SIGINT`].
///
/// Holds the `sa_sigaction` the arm displaced for SIGTERM, so the
/// handler chains to whoever owned the signal before the session.
#[cfg(unix)]
static CHAINED_SIGTERM: AtomicUsize = AtomicUsize::new(0);

/// Whether the fatal-signal handler must restore and exit itself
/// rather than chain to the cooperative owner.
///
/// False while the tokio bridge owns interrupts — the whole live
/// session, including a stalled run teardown the runtime is still
/// polling — and true from the top of [`restore_terminal`] onward and
/// inside blocking windows the runtime cannot serve, where a chained
/// handler would strand the signal and leave the terminal broken.
#[cfg(unix)]
static SIGNAL_RESTORE_DEAF: AtomicBool = AtomicBool::new(false);

/// Whether the fatal-signal handler has been armed for this process.
///
/// Arming is once-per-process: the handler and its byte string are
/// built from the first session's state, and a second session in the
/// same process is not a shape this binary runs.
#[cfg(unix)]
static SIGNAL_RESTORE_ARMED: AtomicBool = AtomicBool::new(false);

/// Initialize the terminal for a full-screen TUI session.
///
/// Enables raw mode and enters the alternate screen with bracketed
/// paste armed — paste lands as one atomic event — asks for focus
/// reporting so a window regaining focus can trigger the full repaint
/// (some terminals skip painting while hidden and show stale cells on
/// reveal) — asks the terminal for
/// a blinking block cursor, which many terminals do not offer by
/// default (terminals that ignore the style request simply keep
/// their own) — and asks for modified-key reporting through the
/// kitty protocol's disambiguate flag, so Shift+Enter arrives with
/// its modifier instead of folding into a bare Enter on terminals
/// that speak it. Terminals implementing neither keep their own
/// reporting — iTerm2 already distinguishes Shift+Enter on its own,
/// and xterm's modifyOtherKeys is deliberately left alone: honoring
/// it switches keys to a spelling the event parser cannot read.
/// Returns a
/// terminal bound to stdout. Pair with [`restore_terminal`] — or
/// hold a [`TerminalGuard`] so the pairing is automatic. A failure
/// after raw mode was enabled undoes the partial setup — raw mode
/// off first, the escape sequences best-effort — before returning
/// the error, so an unwritable stdout cannot strand the shell in
/// raw mode.
///
/// # Errors
/// Fails when the terminal mode or escape-sequence writes are
/// rejected, most commonly because stdout is not a terminal.
pub fn init_terminal(mouse_capture: bool) -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    #[cfg(unix)]
    capture_original_termios();
    enable_raw_mode()?;
    let initialized = (|| {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableFocusChange,
            SetCursorStyle::BlinkingBlock,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        if mouse_capture {
            execute!(stdout, EnableMouseCapture)?;
        } else {
            write!(stdout, "{}", alternate_scroll_sequence(true))?;
            ALTERNATE_SCROLL_ON.store(true, Ordering::Relaxed);
        }
        stdout.flush()?;
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend)
    })();
    if initialized.is_err()
        && let Err(restore_err) = restore_terminal()
    {
        tracing::warn!("terminal restore after a failed init failed: {restore_err}");
    }
    initialized
}

/// Point the terminal's default background at the session theme's
/// canvas color.
///
/// The conversation pane renders on the terminal's default
/// background rather than on painted cells, and the margin around
/// the cell grid — window padding, the leftover of fractional cell
/// sizing — is drawn by the terminal from that same default.
/// Terminals that speak OSC 11 let a program retarget the color, so
/// grid and margin share the theme's canvas on one layer. The
/// original color is captured first and restored by
/// [`restore_terminal`] on every exit path; a terminal that does not
/// answer the background query is left exactly as it was, the query
/// doubling as the capability probe — a set with no captured way
/// home would repaint the user's shell window for good. A non-RGB
/// canvas — the transparent theme's `Reset`, which defers to the
/// terminal's own background — leaves this a no-op: the terminal's
/// configured background, transparent or not, stands exactly as the
/// user set it. The query occupies stdin for at most its short
/// deadline before the event reader starts, and typed-ahead input in
/// that window is gated, not blindly consumed (the query's own doc
/// records the accepted residual).
///
/// Returns whether the override landed: `true` when the terminal
/// answered and its default background now carries the theme's
/// canvas; `false` when it stayed silent or the canvas is not RGB.
/// A caller whose palette assumes the canvas must read `false` as
/// "defer to the terminal" — the palette's terminal-background
/// colors were chosen against the theme's canvas, which this
/// terminal is not showing.
#[must_use]
pub fn sync_default_background(background: Color) -> bool {
    query_and_install(background).installed
}

/// Ask the terminal for its default background and install the new
/// override when it answers — the mid-session half of the exchange.
///
/// # The caller owns stdin
///
/// This function reads the terminal's reply from stdin, so it may run
/// only while the event reader thread is quiesced — the handshake in
/// [`crate::events::InputQuiesce`] is the intended caller-side gate.
/// Polled otherwise, the reply's bytes race the reader: they surface
/// in the composer as key events, and the query's timeout reads as a
/// refusal that was really a lost race.
///
/// Records what the answer taught: an answered terminal is
/// [`BackgroundSupport::Answered`] from here on (and
/// [`finish_theme_burst`]'s canvas write serves later switches
/// without asking),
/// a silent one is [`BackgroundSupport::Refused`] and is never asked
/// again this session. A keystroke whose first byte is not an ESC
/// lead teaches nothing — the record stays
/// [`BackgroundSupport::NotAsked`], so a later switch may ask again —
/// and the byte comes back in the outcome for the caller to return
/// to the composer. An ESC-leading keystroke is the accepted loss
/// instead: the reply parser consumes it, the session settles
/// [`BackgroundSupport::Refused`], and no byte comes back — the same
/// one-shot cost the startup window documents.
pub(crate) fn ask_default_background(background: Color) -> QueryOutcome {
    query_and_install(background)
}

/// What the query-and-override exchange left behind.
///
/// The two halves a caller acts on: whether the override landed, and
/// the keystroke the query consumed instead of a reply — a byte that
/// was on its way to the composer and must be handed back rather than
/// dropped.
#[derive(Default)]
pub(crate) struct QueryOutcome {
    /// Whether the terminal answered and the override was written.
    ///
    /// False on silence, a stolen keystroke, or a paintless palette —
    /// in every case the caller keeps the deferred palette.
    pub(crate) installed: bool,

    /// The one byte a non-reply keystroke lost to the query reader.
    ///
    /// `None` on every other path: an answered, silent, or never-run
    /// exchange touches nothing the composer was owed.
    pub(crate) stolen: Option<u8>,
}

/// The query-and-override exchange shared by the startup and
/// mid-session halves. The caller owns stdin; the halves differ only
/// in who guarantees that.
fn query_and_install(background: Color) -> QueryOutcome {
    let Color::Rgb(red, green, blue) = background else {
        return QueryOutcome::default();
    };
    #[cfg(unix)]
    let outcome = query_unix(red, green, blue);
    #[cfg(not(unix))]
    let outcome = {
        let _ = (red, green, blue);
        QueryOutcome::default()
    };
    outcome
}

/// The unix half of the exchange: query, record the verdict, install.
///
/// Split from [`query_and_install`] so the platform halves each read
/// as one straight line — the caller owns stdin either way, and only
/// the byte-level exchange differs.
#[cfg(unix)]
fn query_unix(red: u8, green: u8, blue: u8) -> QueryOutcome {
    let outcome = query_default_background();
    record_support(reply_verdict(&outcome));
    match outcome {
        QueryReply::Answered(original) => {
            let _stored = ORIGINAL_BACKGROUND.set(original);
            let mut stdout = io::stdout();
            drop(write!(
                stdout,
                "{}",
                background_sequence((red, green, blue))
            ));
            drop(stdout.flush());
            QueryOutcome {
                installed: true,
                stolen: None,
            }
        }
        other => QueryOutcome {
            installed: false,
            stolen: match other {
                QueryReply::Stolen(byte) => Some(byte),
                _ => None,
            },
        },
    }
}

/// What a query's reply teaches the support record.
///
/// The stolen byte is the one that must not teach: a keystroke that
/// reached the reader first is not the terminal refusing anything, so
/// the record stays not-asked and a later switch earns its own
/// answer.
fn reply_verdict(reply: &QueryReply) -> BackgroundSupport {
    match reply {
        QueryReply::Answered(_) => BackgroundSupport::Answered,
        QueryReply::Refused => BackgroundSupport::Refused,
        QueryReply::NotAsked | QueryReply::Stolen(_) => BackgroundSupport::NotAsked,
    }
}

/// What a live background change writes at this session's state —
/// the decision separated from the write so it pins without touching
/// stdout.
///
/// [`sync_default_background`] is the startup half and may query the
/// terminal, because nothing else reads it yet. A live theme switch
/// cannot query offhand: the event reader thread is parked on the same
/// stdin, and a query would race it two ways at once — the reply's
/// escape bytes reaching the composer as key events, or the query's
/// timeout reading as "the terminal did not answer" and deferring a
/// palette over a background the override never reached. So the live
/// half only writes — asking mid-session is
/// [`ask_default_background`]'s job, under a quiesced reader — and the
/// write goes one of two ways: an `Rgb` canvas carries the palette's
/// own color, while a `Color::Reset` canvas (the transparent theme,
/// whose whole vocabulary is the terminal's own) hands the terminal
/// its *original* background back — a session that installed an
/// override and then went paintless must not keep painting the
/// previous theme's canvas under a palette that never paints one.
/// Without a captured original, neither direction has anything
/// honest to write.
enum LiveOverridePlan {
    /// Write the palette's own canvas color.
    ///
    /// The proof and the color both stand: an `Rgb` background the
    /// answered terminal takes as its default.
    Write,

    /// Put the terminal's original background back.
    ///
    /// A `Reset` canvas under a proven terminal — the paintless
    /// palettes, whose whole contract is deferring to the terminal.
    RestoreOriginal,

    /// Nothing to write: no proof the terminal takes the override.
    ///
    /// Writing without proof would strand the previous theme's canvas
    /// under a palette that never painted one, so the honest plan is
    /// no plan at all.
    Decline,
}

fn live_override_plan(original_captured: bool, background: Color) -> LiveOverridePlan {
    match (original_captured, background) {
        (true, Color::Rgb(..)) => LiveOverridePlan::Write,
        (true, Color::Reset) => LiveOverridePlan::RestoreOriginal,
        _ => LiveOverridePlan::Decline,
    }
}

/// Whether the write-only override would land for `background` as things
/// stand — the pure half of the theme burst's canvas write
/// ([`finish_theme_burst`]).
///
/// The same conditions the write consults — a captured original
/// background to lean on, and a palette whose canvas is an RGB color or
/// the terminal's own `Reset` — answered without writing a byte, so a
/// caller can decide to defer the write into a later flush (beside the
/// re-styled cells it belongs with) without paying the write twice or
/// guessing at the gate.
pub(crate) fn write_only_warranted(background: Color) -> bool {
    ORIGINAL_BACKGROUND.get().is_some()
        && matches!(
            live_override_plan(true, background),
            LiveOverridePlan::Write | LiveOverridePlan::RestoreOriginal
        )
}

/// The OSC 11 sequence that points the terminal's default background
/// at `channels`.
///
/// The exact wire shape every terminal in practice accepts: the OSC
/// introducer, the `rgb:` color spec with two hex digits per channel,
/// and the string terminator. Both writes the session makes — the
/// override at startup and the restore at teardown — go through this
/// one builder, so the set and the captured-original formats can
/// never drift apart.
fn background_sequence((red, green, blue): (u8, u8, u8)) -> String {
    format!("\x1b]11;rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\")
}

/// The OSC 12 sequence that tints the terminal's text cursor.
///
/// The hardware cursor's one consumer is the question sheet's inline
/// caret — the composer's caret is the app's own painted cell and
/// never reads the tint, so the sheet's caret is what this styles.
/// The palette carries it the same way it carries the background:
/// tinted to the theme's accent, with the same two hex digits per
/// channel and the same wire shape.
fn cursor_sequence((red, green, blue): (u8, u8, u8)) -> String {
    format!("\x1b]12;rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\")
}

/// The OSC 112 sequence that gives the terminal its own cursor color
/// back — the reset where [`cursor_sequence`] is the set.
///
/// The restore needs no captured original and runs no query: 112
/// resets the cursor color to the terminal's configured default,
/// which on a session that never tinted is the color it already had.
fn cursor_reset_sequence() -> &'static str {
    "\x1b]112\x1b\\"
}

/// Tint the terminal cursor to a palette's accent color.
///
/// One-way like the background override's live half and riding the
/// same proof: only a session whose terminal answered the background
/// query sends it, because a terminal that stayed silent about OSC 11
/// has said nothing that recommends OSC 12 either. Best effort by
/// design — the tint is cosmetic, so a failed write is accepted in
/// silence — and a non-`Rgb` accent (the paintless vocabulary's
/// `Reset`) is a no-op: the cursor keeps the terminal's own color.
pub fn set_cursor_color(color: Color) {
    let Color::Rgb(red, green, blue) = color else {
        return;
    };
    if ORIGINAL_BACKGROUND.get().is_none() {
        return;
    }
    let mut stdout = io::stdout();
    drop(write!(stdout, "{}", cursor_sequence((red, green, blue))));
    drop(stdout.flush());
}

/// The mode-2026 window opener: CSI ? 2026 h.
///
/// A constant beside its writer so the burst's framing is one spelled
/// contract, pinned by shape in the tests below.
const SYNCHRONIZED_OUTPUT_BEGIN: &str = "\x1b[?2026h";

/// The mode-2026 window closer: CSI ? 2026 l.
///
/// See [`SYNCHRONIZED_OUTPUT_BEGIN`]; the pair brackets a burst.
const SYNCHRONIZED_OUTPUT_END: &str = "\x1b[?2026l";

/// Open a synchronized-update window (DEC private mode 2026).
///
/// A terminal that implements the mode buffers everything it receives
/// between this marker and the burst's closing write, and repaints
/// once when the window closes — the standard way to land a burst (a
/// theme's re-styled cells beside its canvas write) as one visible
/// step even when the cell diff is large. A terminal without the mode
/// ignores the marker and renders as it always did, so the window
/// costs nothing.
pub fn begin_synchronized_output() {
    let mut stdout = io::stdout();
    drop(write!(stdout, "{SYNCHRONIZED_OUTPUT_BEGIN}"));
    drop(stdout.flush());
}

/// The theme burst's closing bytes: the canvas write, the cursor
/// write, and the synchronized-update window close, one buffer.
///
/// Pure over the captured original so the byte shape pins without a
/// terminal: a captured terminal taking an RGB palette writes its
/// canvas and the accent cursor tint; a paintless (`Reset`) palette
/// restores the terminal's own canvas and resets its cursor; an
/// uncaptured terminal writes neither — the window close is the only
/// byte every shape shares, so the markers always pair.
pub(crate) fn theme_burst_bytes(
    original: Option<(u8, u8, u8)>,
    background: Color,
    primary: Color,
) -> String {
    let channels = match (original, live_override_plan(original.is_some(), background)) {
        (_, LiveOverridePlan::Write) => match background {
            Color::Rgb(red, green, blue) => Some((red, green, blue)),
            _ => None,
        },
        (Some(original), LiveOverridePlan::RestoreOriginal) => Some(original),
        _ => None,
    };
    let mut burst = String::new();
    if let Some((red, green, blue)) = channels {
        burst.push_str(&background_sequence((red, green, blue)));
        if background == Color::Reset {
            burst.push_str(cursor_reset_sequence());
        } else if let Color::Rgb(red, green, blue) = primary {
            burst.push_str(&cursor_sequence((red, green, blue)));
        }
    }
    burst.push_str(SYNCHRONIZED_OUTPUT_END);
    burst
}

/// Close a theme burst: both OSC writes and the synchronized-update
/// window close, in one write.
///
/// The burst's head ([`begin_synchronized_output`]) opens the window
/// before the cell diff; this closes it after, so the canvas and
/// cursor land as the burst's final beat beside the diff's last cells.
/// On a terminal without the mode — whose paints follow its reads —
/// the coalesced single write means the flip cannot split across the
/// stream the way separate writes did, and the cells that painted
/// earliest in the diff never trail the canvas by a whole region.
pub fn finish_theme_burst(background: Color, primary: Color) {
    let closer = theme_burst_bytes(ORIGINAL_BACKGROUND.get().copied(), background, primary);
    let mut stdout = io::stdout();
    drop(write!(stdout, "{closer}"));
    drop(stdout.flush());
}

/// Give the terminal its own cursor color back.
///
/// Unconditional on the exit paths: on a session that never tinted,
/// the OSC 112 reset returns the cursor to the default it already
/// had, so no bookkeeping decides whether to send it.
pub fn reset_cursor_color() {
    let mut stdout = io::stdout();
    drop(write!(stdout, "{}", cursor_reset_sequence()));
    drop(stdout.flush());
}

/// What asking the terminal for its default background produced.
///
/// The ask's own three outcomes — the answer the display's palette
/// rides on, the silence that settles the session's refusal, and the
/// exchange that never ran because input was already waiting.
enum QueryReply {
    /// The terminal answered with its color.
    ///
    /// The RGB triple the theme's canvas defers to and the restore
    /// remembers.
    Answered((u8, u8, u8)),

    /// The terminal was asked and stayed silent past the deadline.
    ///
    /// Silence settles the session's refusal verdict — no second ask
    /// spends the user's time again.
    Refused,

    /// The exchange never ran — input was already waiting, and
    /// typed-ahead keys are the user's, not ours to eat.
    NotAsked,

    /// A keystroke, not a reply: the first byte to arrive was not an
    /// ESC, so no answer was read and the byte rides back to whoever
    /// holds the user's input.
    Stolen(u8),
}

/// Ask the terminal for its default background and read the reply.
///
/// Writes the OSC 11 query and waits for the answer under a short
/// deadline — a terminal that does not implement the query stays
/// silent and the caller skips the whole override. Runs only where
/// the caller owns stdin — before any event reader exists, or
/// mid-session with the reader quiesced — because the reply is raw
/// input, not a key event, so it is read directly.
///
/// Typed-ahead input shares this window, and is gated rather than
/// blindly consumed. Input already queued when the query would run
/// skips the sync outright — those bytes are the user's, and the
/// margin keeps the terminal's own background. Failing that, an OSC
/// reply always opens with `ESC`, so a first byte that is not one
/// stops the query and carries that single byte back to the caller —
/// the keystroke is the user's, not the terminal's answer — instead
/// of drinking everything the user typed; and every byte is read
/// alone, so a keystroke arriving behind a completed reply stays
/// unconsumed for the event reader.
///
/// A reply that has started owns the reading until it terminates:
/// once its first byte has arrived, a second bounded window holds
/// the query open for the rest, and every byte of it is consumed
/// here whatever the parse will say — the event reader's parser
/// does not know OSC replies, so an abandoned tail would reach the
/// composer as keystrokes ("11;rgb:…" typed into the input). A
/// reply that completes inside the window is used even though it
/// outran the first deadline; one that outruns both leaves only
/// bytes that had not yet arrived, and an ESC-leading keystroke (an
/// arrow key, Esc itself) arriving in the window is consumed with
/// the failed reply attempt — a one-shot window, bounded well under
/// half a second, that is accepted rather than bridged.
#[cfg(unix)]
fn query_default_background() -> QueryReply {
    use std::time::Duration;
    use std::time::Instant;

    const QUERY_DEADLINE: Duration = Duration::from_millis(200);
    const REPLY_FINISH_DEADLINE: Duration = Duration::from_millis(200);

    if stdin_ready(Duration::ZERO) {
        return QueryReply::NotAsked;
    }
    let mut stdout = io::stdout();
    if write!(stdout, "\x1b]11;?\x1b\\").is_err() {
        // The query itself never left: the exchange did not run.
        return QueryReply::NotAsked;
    }
    if stdout.flush().is_err() {
        return QueryReply::NotAsked;
    }

    let mut reply = Vec::new();
    let Some(query_deadline) = Instant::now().checked_add(QUERY_DEADLINE) else {
        return QueryReply::Refused;
    };
    while reply.is_empty() {
        let Some(byte) = read_reply_byte(query_deadline) else {
            return QueryReply::Refused;
        };
        if byte != 0x1b {
            return QueryReply::Stolen(byte);
        }
        reply.push(byte);
    }
    let Some(finish_deadline) = Instant::now().checked_add(REPLY_FINISH_DEADLINE) else {
        return QueryReply::Refused;
    };
    while !reply_terminated(&reply) {
        let Some(byte) = read_reply_byte(finish_deadline) else {
            break;
        };
        reply.push(byte);
    }
    if let Some(color) = parse_background_reply(&reply) {
        QueryReply::Answered(color)
    } else {
        QueryReply::Refused
    }
}

/// Read one byte of a reply, waiting until `deadline`.
///
/// `None` when the wait expires or the read fails — the caller
/// decides, phase by phase, what an unfinished reply means. Reads
/// the descriptor directly through [`read_stdin_raw`], so nothing
/// hides from the poll in a userspace buffer.
#[cfg(unix)]
fn read_reply_byte(deadline: std::time::Instant) -> Option<u8> {
    use std::time::Instant;

    let mut byte = [0u8; 1];
    let wait = deadline.saturating_duration_since(Instant::now());
    if wait.is_zero() || !stdin_ready(wait) {
        return None;
    }
    if read_stdin_raw(&mut byte)? != 1 {
        return None;
    }
    byte.first().copied()
}

/// Read up to `buf.len()` raw bytes from the terminal's input.
///
/// A direct `read(2)` on the standard-input descriptor, deliberately
/// not [`std::io::stdin()`]: that handle layers an internal buffer
/// over the descriptor, and one buffered read can drain bytes the
/// query loop's `poll(2)` can then no longer see — the loop would
/// time out with half a reply stranded in userspace. Reading the
/// descriptor keeps every byte where the poll can find it. A signal
/// landing mid-read sets `EINTR`, which retries rather than returning
/// a failure the caller would read as the terminal's refusal — a
/// raced interrupt must not defer the palette for a session whose
/// terminal would have answered.
#[cfg(unix)]
fn read_stdin_raw(buf: &mut [u8]) -> Option<usize> {
    loop {
        let read = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if read == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return usize::try_from(read).ok();
    }
}

/// Whether stdin has bytes waiting within `wait`.
///
/// A one-descriptor `poll(2)` on the terminal's input, the only
/// std-free way to watch stdin with a timeout. The reply reader uses
/// it to tell "the terminal is still composing its answer" from "the
/// terminal has nothing to say"; a zero or elapsed deadline polls
/// once and returns immediately, so the reader never blocks past its
/// deadline waiting for bytes that are already there. `EINTR` — a
/// signal landing inside the wait — retries the poll, never reads as
/// an expired deadline.
#[cfg(unix)]
fn stdin_ready(wait: std::time::Duration) -> bool {
    use std::os::fd::AsRawFd as _;

    let mut polls = [libc::pollfd {
        fd: io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    let timeout = i32::try_from(wait.as_millis()).unwrap_or(0);
    loop {
        let decided = unsafe { libc::poll(polls.as_mut_ptr(), 1, timeout) };
        if decided == -1
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        }
        return decided > 0;
    }
}

/// Whether an OSC reply buffer already carries its terminator.
///
/// Terminals close an OSC reply either with the legacy BEL (`0x07`)
/// or the string terminator (`ESC \`), and a single `read` can deliver
/// a reply split across chunks — the reader loops until one of the
/// two appears so a fragmented answer is assembled, not truncated.
#[cfg(unix)]
fn reply_terminated(reply: &[u8]) -> bool {
    reply.contains(&0x07) || reply.windows(2).any(|window| window == b"\x1b\\")
}

/// Parse an OSC 11 background reply into its RGB channels.
///
/// Accepts the shapes terminals actually answer with: the `rgb:` color
/// spec, one to four hex digits per channel scaled to eight bits the
/// way the spec defines, closed by either terminator (BEL or ST).
/// Anything else — a partial reply, a different sequence, noise — is
/// `None`, and the caller treats the terminal as not answering.
#[cfg(unix)]
fn parse_background_reply(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let start = reply.windows(5).position(|window| window == b"\x1b]11;")?;
    let body = reply.get(start.saturating_add(5)..)?;
    let end = body
        .iter()
        .position(|&byte| byte == 0x07)
        .or_else(|| body.windows(2).position(|window| window == b"\x1b\\"))?;
    let body = std::str::from_utf8(body.get(..end)?).ok()?;
    let spec = body.strip_prefix("rgb:")?;
    let mut channels = spec.split('/');
    let red = parse_channel(channels.next()?)?;
    let green = parse_channel(channels.next()?)?;
    let blue = parse_channel(channels.next()?)?;
    Some((red, green, blue))
}

/// Scale one OSC color channel — one to four hex digits — to eight
/// bits.
///
/// The spec defines a channel's digits as a fixed-point fraction of
/// the maximum its width can express, so the same color arrives as
/// `28` (two digits), `2828` (four), or `2` (one, meaning roughly a
/// fifteenth of the scale). Scaling by the digit-count maximum maps
/// every width onto one comparable `u8`; anything but one to four
/// hex digits is `None`, which the reply parser reads as noise.
#[cfg(unix)]
fn parse_channel(token: &str) -> Option<u8> {
    if token.is_empty() || token.len() > 4 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let digits = u32::try_from(token.len()).ok()?;
    let scale = 16u32.checked_pow(digits)?.checked_sub(1)?;
    let value = u32::from(u16::from_str_radix(token, 16).ok()?);
    u8::try_from(value.checked_mul(255)?.checked_div(scale)?).ok()
}

/// The alternate-scroll translation switch.
///
/// `CSI ? 1007 h` asks the terminal to turn wheel scrolls into arrow
/// keys while the alternate screen is up — mouse reporting stays off,
/// so click-drag selection remains the terminal's own. Byte-pinned so
/// the enable the session writes and the disable teardown writes stay
/// exact complements.
#[cfg(unix)]
fn alternate_scroll_sequence(enable: bool) -> &'static str {
    if enable { "\x1b[?1007h" } else { "\x1b[?1007l" }
}

/// The non-unix counterpart: alternate-scroll translation is a DEC
/// extension without a Windows analogue, so nothing is asked of the
/// terminal either way.
#[cfg(not(unix))]
fn alternate_scroll_sequence(_enable: bool) -> &'static str {
    ""
}

/// Whether this session turned alternate-scroll translation on.
///
/// Set by [`init_terminal`] when it writes the enable for a session
/// that gave the mouse up; claimed by [`restore_terminal`], which
/// stands down only what the session stood up. Alternate-scroll is a
/// user-level setting some terminals ship on by default — turning
/// off a switch the session never turned on would leave it off for
/// whatever runs in that terminal next.
static ALTERNATE_SCROLL_ON: AtomicBool = AtomicBool::new(false);

/// Capture the line discipline exactly as the session found it.
///
/// Runs before raw mode is enabled once per process; a failed capture
/// is recorded as absent, and the fatal-signal handler then does the
/// escape-sequence half of the restore only.
#[cfg(unix)]
fn capture_original_termios() {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `termios` is a plain-data struct owned by this frame, and
    // tcgetattr only fills it from the descriptor's current discipline.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer is valid for one termios record, per above.
    let captured = unsafe { libc::tcgetattr(io::stdin().as_raw_fd(), &raw mut termios) } == 0;
    if captured {
        let _stored = ORIGINAL_TERMIOS.set(termios);
    }
}

/// Build the exact byte string the fatal-signal handler writes.
///
/// Mirrors [`restore_terminal`]'s sequence — original background first
/// when one was captured, then every mode the session stood up stood
/// back down, then the alternate screen left — but assembled once,
/// here, from crossterm's own serializers, so the handler never
/// formats or locks at signal time.
#[cfg(unix)]
fn signal_restore_bytes(original: Option<(u8, u8, u8)>) -> Vec<u8> {
    use crossterm::Command as _;

    /// Append one command's serialized ANSI form.
    ///
    /// The destination is an in-memory string, which never fails a write;
    /// a failure is unreachable in practice and only logged.
    fn pushed(result: std::fmt::Result) {
        if result.is_err() {
            tracing::warn!("in-memory terminal command serialization failed");
        }
    }

    let mut bytes = String::new();
    if let Some(channels) = original {
        bytes.push_str(&background_sequence(channels));
    }
    bytes.push_str(cursor_reset_sequence());
    pushed(DisableMouseCapture.write_ansi(&mut bytes));
    pushed(DisableBracketedPaste.write_ansi(&mut bytes));
    pushed(DisableFocusChange.write_ansi(&mut bytes));
    pushed(PopKeyboardEnhancementFlags.write_ansi(&mut bytes));
    pushed(SetCursorStyle::DefaultUserShape.write_ansi(&mut bytes));
    pushed(LeaveAlternateScreen.write_ansi(&mut bytes));
    if ALTERNATE_SCROLL_ON.load(Ordering::Relaxed) {
        bytes.push_str(alternate_scroll_sequence(false));
    }
    bytes.into_bytes()
}

/// Replay the captured restore bytes and the captured line discipline.
///
/// The handler-side twin of [`restore_terminal`]'s first and last
/// steps, built from raw `write` and `tcsetattr` so a signal arriving
/// on any thread can run it without touching a lock or an allocator.
///
/// # Safety
///
/// Called from a signal handler: the statics it reads must have been
/// set before the handler was armed, and the caller must tolerate the
/// call running on any thread while other threads are mid-operation.
#[cfg(unix)]
unsafe fn run_signal_restore() {
    if let Some(bytes) = SIGNAL_RESTORE_BYTES.get() {
        let mut offset = 0usize;
        while offset < bytes.len() {
            let remaining = bytes.len().saturating_sub(offset);
            // SAFETY: the offset stays within the byte string by the
            // loop condition, and fd 1 is the terminal the session drew on.
            let written = unsafe { libc::write(1, bytes.as_ptr().add(offset).cast(), remaining) };
            let advanced = usize::try_from(written).unwrap_or(0);
            if advanced == 0 {
                break;
            }
            offset = offset.saturating_add(advanced);
        }
    }
    if let Some(original) = ORIGINAL_TERMIOS.get() {
        // SAFETY: the pointer names the captured termios copy, and
        // TCSANOW applies it in place without waiting on queued output.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, original) };
    }
}

/// The fatal-signal handler: chain while the cooperative layer owns
/// interrupts, restore and exit once it cannot.
///
/// In the live phase the saved previous action runs — the tokio
/// bridge's handler, so cooperative cancellation keeps working and the
/// forced double-press path performs its own restore. In the deaf
/// phase the captured restore bytes go out, the captured line
/// discipline comes back, and the process exits 130; the alternative
/// on this path is a terminal stranded in raw mode and the session
/// theme. Only SIGINT and SIGTERM are armed, so the SIGTERM slot is
/// the chain for every signal that is not SIGINT.
#[cfg(unix)]
extern "C" fn fatal_signal_restore_handler(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let chain = if signal == libc::SIGINT {
        &CHAINED_SIGINT
    } else {
        &CHAINED_SIGTERM
    };
    let previous = chain.load(Ordering::SeqCst);
    let action = disposition_action(previous);
    if SIGNAL_RESTORE_DEAF.load(Ordering::SeqCst) || action == DispositionAction::RestoreAndExit {
        // SAFETY: the byte string and termios statics were captured at
        // arm time; the call only touches raw fds and process state.
        unsafe { run_signal_restore() };
        // SAFETY: _exit is async-signal-safe and, on this path, the
        // process's terminal is already restored.
        unsafe { libc::_exit(130) };
    }
    if action == DispositionAction::Chain {
        type ChainedHandler =
            unsafe extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void);
        // SAFETY: the value was stored from a sigaction handoff at arm
        // time and names the handler that owned this signal before this
        // one; the incoming arguments are passed through unchanged.
        let chained = unsafe { std::mem::transmute::<usize, ChainedHandler>(previous) };
        // SAFETY: calling the displaced handler with the kernel's own
        // arguments is the documented chain handoff.
        unsafe { chained(signal, info, context) };
    }
}

/// What the live-phase handler does with a displaced disposition.
///
/// The kernel encodes the two non-handler dispositions as the small
/// integers `SIG_DFL` (0) and `SIG_IGN` (1); anything above them is a
/// function address. Each encoding has its own duty: a real handler
/// is chained to (the tokio bridge's listener, so cooperative
/// cancellation keeps working); an inherited ignore is honored by
/// returning silently; and a displaced default — which meant
/// "terminate the process" before this handler was armed — is
/// restored-and-exited exactly like the deaf phase, so a session the
/// bridge never registered for can still be stopped by SIGTERM or
/// Ctrl-C instead of surviving the signal that should have ended it.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispositionAction {
    /// Chain to the displaced handler.
    ///
    /// The live phase's ordinary case: the tokio bridge owned the
    /// signal before arming, and handing the kernel's arguments back
    /// keeps cooperative cancellation working.
    Chain,
    /// Honor an inherited `SIG_IGN`: return without acting.
    ///
    /// The process inherited silence for this signal, so the handler
    /// adds none — the case of a launch that ignored the signal and a
    /// bridge whose listener never registered.
    HonorIgnore,
    /// Restore the terminal and exit 130, as the deaf phase does.
    ///
    /// The displaced default meant "terminate the process", so
    /// honoring it means leaving cleanly with the terminal restored —
    /// not swallowing the SIGTERM or Ctrl-C that should have ended
    /// the session.
    RestoreAndExit,
}

/// Map a displaced `sa_sigaction` value to its [`DispositionAction`].
///
/// Pure over the raw disposition word so the pin drives every
/// encoding without arming anything: the kernel's sentinel integers
/// name the two special duties, and any other value is a handler
/// address to chain to.
#[cfg(unix)]
fn disposition_action(sa_sigaction: usize) -> DispositionAction {
    match sa_sigaction {
        0 => DispositionAction::RestoreAndExit,
        1 => DispositionAction::HonorIgnore,
        address => {
            // Real handler addresses sit far above both sentinels; the
            // binding exists only to name that any other value is one.
            let _ = address;
            DispositionAction::Chain
        }
    }
}

/// Replace one signal's disposition with the fatal-restore handler,
/// remembering the action it displaced for the live-phase chain.
///
/// # Safety
///
/// `signal` must be one of the signals this module arms (SIGINT or
/// SIGTERM), and the caller must run before or after any other thread
/// depends on the signal's disposition, since `sigaction` swaps it
/// process-wide.
#[cfg(unix)]
unsafe fn install_fatal_handler(signal: libc::c_int, chain: &AtomicUsize) {
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: both sigaction structs are owned plain-data frames, and
    // the signal numbers are the constants this module arms.
    let replaced = unsafe {
        type RawHandler = extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void);
        let mut action: libc::sigaction = std::mem::zeroed();
        let handler: RawHandler = fatal_signal_restore_handler;
        action.sa_sigaction = handler as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigemptyset(&raw mut action.sa_mask);
        libc::sigaction(signal, &raw const action, &raw mut previous)
    } == 0;
    if replaced {
        chain.store(previous.sa_sigaction, Ordering::SeqCst);
    } else {
        tracing::warn!("fatal-signal terminal restore could not be armed");
    }
}

/// Arm the fatal-signal terminal restore for SIGINT and SIGTERM.
///
/// Builds the restore byte string from the state the session has at
/// arm time — call after [`sync_default_background`] so a captured
/// original background rides in it — and replaces both signals'
/// dispositions, keeping the displaced handlers for the live-phase
/// chain. Idempotent: the first call arms, later calls return.
///
/// The handler is deliberately synchronous rather than a tokio
/// listener: it must still work when the runtime is blocked in a
/// non-polling call or already gone, which is exactly where the
/// terminal would otherwise be stranded.
#[cfg(unix)]
pub fn arm_fatal_signal_restore() {
    if SIGNAL_RESTORE_ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _stored =
        SIGNAL_RESTORE_BYTES.set(signal_restore_bytes(ORIGINAL_BACKGROUND.get().copied()));
    // SAFETY: the two calls only register this module's handler and
    // record the actions they displaced.
    unsafe {
        install_fatal_handler(libc::SIGINT, &CHAINED_SIGINT);
        install_fatal_handler(libc::SIGTERM, &CHAINED_SIGTERM);
    }
}

/// The non-unix counterpart: no POSIX signal handling to arm.
///
/// Windows console handling does not go through `sigaction`, so the
/// safety net has no shape here; the ordered teardown still restores
/// the terminal on every path.
#[cfg(not(unix))]
pub fn arm_fatal_signal_restore() {}

/// Mark the start of a blocking window the async runtime cannot serve.
///
/// While the window is open the fatal-signal handler restores the
/// terminal and exits instead of chaining, because the cooperative
/// owner of interrupts cannot run to accept the handoff. Wrap only
/// stretches of straight-line blocking work — a thread join, not an
/// await.
#[cfg(unix)]
pub fn enter_signal_deaf_window() {
    SIGNAL_RESTORE_DEAF.store(true, Ordering::SeqCst);
}

/// The non-unix counterpart: no POSIX signal handling to gate.
///
/// A no-op so the session wiring reads the same on every platform.
#[cfg(not(unix))]
pub fn enter_signal_deaf_window() {}

/// Close a window opened by [`enter_signal_deaf_window`], returning
/// interrupts to the cooperative owner.
#[cfg(unix)]
pub fn exit_signal_deaf_window() {
    SIGNAL_RESTORE_DEAF.store(false, Ordering::SeqCst);
}

/// The non-unix counterpart: no POSIX signal handling to gate.
///
/// A no-op so the session wiring reads the same on every platform.
#[cfg(not(unix))]
pub fn exit_signal_deaf_window() {}

/// Point the fatal-signal handler at self-restore for everything that
/// follows.
///
/// Once the ordinary restore has run — or failed — there is no
/// cooperative layer left to hand a signal to, so the handler stops
/// chaining and makes any later signal a clean, restored exit.
#[cfg(unix)]
fn mark_signal_restore_deaf() {
    SIGNAL_RESTORE_DEAF.store(true, Ordering::SeqCst);
}

/// The non-unix counterpart: nothing to gate.
///
/// The ordered restore is the only restoration path on these platforms.
#[cfg(not(unix))]
fn mark_signal_restore_deaf() {}

/// Restore the terminal after a TUI session.
///
/// Puts a default background the session overrode back first,
/// best-effort, then stands every mode down while raw mode still
/// mutes the line discipline's echo: mouse capture off, bracketed
/// paste off, keyboard-enhancement pops, cursor shape back, and the
/// alternate screen left. With reporting off, any input the session's
/// last moments queued — a wheel burst scrolling as the display
/// settled, a paste still in flight — is discarded from the input
/// queue so the shell never echoes it as escape-sequence garbage;
/// only then does raw mode come off, last, so no input can arrive
/// between the echo waking up and the modes that generate it being
/// gone. All attempts always run and are combined; the raw-mode
/// error, when present, is the one returned. Safe to call repeatedly
/// and safe when the terminal was never initialized — the escape
/// sequences are ignored by a terminal not in those modes.
///
/// # Errors
/// Fails only when the mode change or the escape-sequence write is
/// rejected by the underlying terminal.
pub fn restore_terminal() -> io::Result<()> {
    mark_signal_restore_deaf();
    if let Some(original) = ORIGINAL_BACKGROUND.get() {
        let mut stdout = io::stdout();
        drop(write!(stdout, "{}", background_sequence(*original)));
        drop(stdout.flush());
    }
    reset_cursor_color();
    let modes_result = execute!(
        io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        PopKeyboardEnhancementFlags,
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    );
    discard_pending_input();
    let raw_result = disable_raw_mode();
    let modify_result = (|| {
        let mut stdout = io::stdout();
        if ALTERNATE_SCROLL_ON.swap(false, Ordering::Relaxed) {
            write!(stdout, "{}", alternate_scroll_sequence(false))?;
        }
        stdout.flush()
    })();
    raw_result.and(modes_result).and(modify_result)
}

/// Discard input bytes still queued against the standard-input
/// descriptor.
///
/// The teardown path's drain: mouse reporting is already off when
/// this runs, so nothing new is generated, and the queued wheel and
/// paste bytes a session's final moments produced are dropped instead
/// of being handed to the shell — which, once raw mode is off, would
/// echo them as visible escape-sequence garbage. A no-op when the
/// descriptor is not a terminal or the flush is unsupported.
#[cfg(unix)]
fn discard_pending_input() {
    use std::os::fd::AsRawFd as _;

    if unsafe { libc::tcflush(io::stdin().as_raw_fd(), libc::TCIFLUSH) } != 0 {
        tracing::warn!("pending-input discard failed");
    }
}

/// The non-unix teardown counterpart of the input drain.
///
/// The POSIX flush has no counterpart on this platform's console
/// paths, and leaving queued bytes for the shell is harmless there,
/// so the drain is deliberately empty.
#[cfg(not(unix))]
fn discard_pending_input() {}

/// Stand the fatal-signal net down for an exit that has begun.
///
/// SIGINT and SIGTERM become ignored, process-wide, from the moment
/// the ordered teardown takes over. The multi-press quit chord
/// invites straggler presses, and once the ordinary restore has run
/// the fatal handler meets them deaf — restore bytes out, then
/// `_exit(130)` with whatever stdout still buffers dropped on the
/// floor. The exit lines print in that exact window, so a straggler
/// there can swallow the resume command the label names. Ignored
/// instead, a straggler changes nothing: the teardown finishes, the
/// exit lines print whole, and the process exits on its own. The
/// ignore outlives nothing — the process exits moments later, and no
/// children survive it to inherit the disposition.
#[cfg(unix)]
pub fn ignore_fatal_signals() {
    // SAFETY: a zeroed sigaction is a valid `sa_flags = 0` record, and
    // sigaction installs it for the whole process; the calls only
    // touch this frame and kernel state.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&raw mut action.sa_mask);
            libc::sigaction(signal, &raw const action, std::ptr::null_mut());
        }
    }
}

/// The non-unix counterpart: no POSIX dispositions to stand down.
///
/// The ordered teardown alone owns these platforms' exit; nothing to
/// ignore, nothing that could `_exit` between the restore and the
/// exit lines.
#[cfg(not(unix))]
pub fn ignore_fatal_signals() {}

/// RAII ownership of an initialized TUI terminal.
///
/// Constructing the guard initializes the terminal (see
/// [`init_terminal`]); dropping it restores the terminal, so every
/// exit path — early return, error propagation, unwind — leaves the
/// user's terminal usable.
pub struct TerminalGuard {
    /// Marker that keeps the guard unconstructible outside
    /// [`new`](Self::new).
    ///
    /// The guard owns no state — its drop behavior is the value.
    _private: (),
}

impl TerminalGuard {
    /// Initialize the terminal and take ownership of its restoration.
    ///
    /// Returns the guard together with the terminal to draw with;
    /// keep the guard alive for the whole session.
    ///
    /// # Errors
    /// Propagates [`init_terminal`]'s failure — nothing was changed, so
    /// there is nothing to restore.
    pub fn new(mouse_capture: bool) -> io::Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        let terminal = init_terminal(mouse_capture)?;
        Ok((Self { _private: () }, terminal))
    }

    /// Install the panic hook that restores the terminal before unwinding.
    ///
    /// A panic unwinds through frames that may sit above any guard
    /// (or inside spawned tasks the guard never sees); the hook
    /// restores first so the backtrace prints to a sane terminal.
    /// Call exactly once at startup. Chained hooks are preserved.
    pub fn install_panic_hook() {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            if let Err(err) = restore_terminal() {
                tracing::warn!("terminal restore during panic failed: {err}");
            }
            previous_hook(panic_info);
        }));
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Err(err) = restore_terminal() {
            tracing::warn!("terminal restore failed: {err}");
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;

    /// Serializes tests that touch process-global signal state: the
    /// deaf flag and the fatal dispositions are one shared process,
    /// so parallel signal tests would race each other's stores.
    #[cfg(unix)]
    static SIGNAL_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Reads one signal's current disposition record.
    #[cfg(unix)]
    fn current_disposition(signal: libc::c_int) -> libc::sigaction {
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: a null act changes no disposition; the call only
        // fills the frame-owned record.
        unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut current) };
        current
    }

    /// Reinstalls a captured disposition record for one signal.
    #[cfg(unix)]
    fn reinstall_disposition(signal: libc::c_int, action: libc::sigaction) {
        // SAFETY: the record is the kernel's own prior answer for
        // this signal, frame-owned plain data.
        unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
    }

    /// Consumes one signal left pending by an ignored delivery.
    ///
    /// An ignored signal a blocked thread holds stays pending instead
    /// of being discarded, and restoring a terminate disposition would
    /// deliver that orphan; the zero-timeout wait drains it.
    #[cfg(unix)]
    fn drain_pending(signal: libc::c_int) {
        let mut awaited: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: the set is frame-owned plain data and the two calls
        // only empty it and add the one signal.
        unsafe {
            libc::sigemptyset(&raw mut awaited);
            libc::sigaddset(&raw mut awaited, signal);
        }
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: the record is frame-owned plain data; a zero
            // timeout makes the call a nonblocking poll that removes
            // one pending instance or fails with EAGAIN.
            let taken = unsafe {
                libc::sigtimedwait(&raw const awaited, &raw mut info, &raw const timeout)
            };
            if taken != signal {
                return;
            }
        }
    }

    #[test]
    fn a_stolen_first_byte_is_not_a_refusal_and_carries_the_byte_back() {
        // A keystroke that reached the query reader is not the terminal
        // answering anything: the support record must learn nothing
        // (the ask stays warranted, unlike a genuine silence), and the
        // byte rides back in the outcome so the caller can return it to
        // the composer. A paintless background never runs the exchange,
        // so it takes nothing.
        let verdict = reply_verdict(&QueryReply::Stolen(b'x'));
        assert_eq!(
            verdict,
            BackgroundSupport::NotAsked,
            "a stolen keystroke teaches nothing about the terminal"
        );
        assert!(
            next_ask_warranted(verdict),
            "a later switch may ask again — the terminal never refused"
        );
        let outcome = query_and_install(ratatui::style::Color::Reset);
        assert!(
            !outcome.installed,
            "a non-Rgb background has nothing to install"
        );
        assert_eq!(
            outcome.stolen, None,
            "no exchange ran, so nothing was taken"
        );
    }

    #[test]
    fn a_live_override_writes_the_palette_or_restores_the_original() {
        // The live switch may only write, never query — the reader thread
        // owns stdin once the session is running. An Rgb canvas writes the
        // palette's own color; Reset — the transparent theme's whole
        // vocabulary — hands the terminal its own background back rather
        // than keep the previous theme's override under a palette that
        // never paints one; without a captured original, neither
        // direction has anything honest to write.
        assert!(
            matches!(
                live_override_plan(true, ratatui::style::Color::Rgb(0x28, 0x2a, 0x36)),
                LiveOverridePlan::Write
            ),
            "a terminal that answered takes the palette's own canvas"
        );
        assert!(
            matches!(
                live_override_plan(true, ratatui::style::Color::Reset),
                LiveOverridePlan::RestoreOriginal
            ),
            "a paintless palette gets the terminal's original background back"
        );
        assert!(
            matches!(
                live_override_plan(false, ratatui::style::Color::Rgb(0x28, 0x2a, 0x36)),
                LiveOverridePlan::Decline
            ),
            "a terminal that never answered keeps the deferred palette"
        );
        assert!(
            matches!(
                live_override_plan(false, ratatui::style::Color::Reset),
                LiveOverridePlan::Decline
            ),
            "with nothing captured there is no original to restore either"
        );
    }

    #[test]
    fn only_a_never_queried_terminal_is_asked_again() {
        // A skipped startup query (typed-ahead keys) is the one session
        // shape that earns a later ask; a proven terminal writes, and a
        // refused one has answered the question for good.
        assert!(
            next_ask_warranted(BackgroundSupport::NotAsked),
            "the session never got an answer, so the first switch asks"
        );
        assert!(
            !next_ask_warranted(BackgroundSupport::Answered),
            "a proven terminal takes the write, not the question"
        );
        assert!(
            !next_ask_warranted(BackgroundSupport::Refused),
            "a silent terminal has answered for good; the palette defers"
        );
    }

    #[cfg(unix)]
    #[test]
    fn each_displaced_disposition_gets_its_own_action() {
        // SIG_DFL (0) and SIG_IGN (1) are encodings, not addresses:
        // transmuting either into a call would fault. But they are not
        // the same duty — a displaced default meant "terminate", so
        // the handler restores and exits rather than swallowing the
        // signal; an inherited ignore is honored silently; real
        // handler addresses sit far above both sentinels and chain.
        assert_eq!(
            disposition_action(0),
            DispositionAction::RestoreAndExit,
            "SIG_DFL displaced is honored as the termination it named"
        );
        assert_eq!(
            disposition_action(1),
            DispositionAction::HonorIgnore,
            "SIG_IGN displaced is honored as the silence it named"
        );
        assert_eq!(
            disposition_action(2),
            DispositionAction::Chain,
            "the lowest real address chains"
        );
        assert_eq!(
            disposition_action(0x5555_5555_5000),
            DispositionAction::Chain,
            "a typical PIE handler address chains"
        );
    }

    #[test]
    fn the_cursor_tint_and_reset_are_the_osc_12_family() {
        // The cursor tint rides the same OSC family as the background
        // override — 12 where the background is 11 — and resets through
        // 112, the no-query restore: resetting needs no captured
        // original, because it returns the cursor to the terminal's own
        // default, which an untinted session already had.
        assert_eq!(
            cursor_sequence((0x28, 0x2a, 0x36)),
            "\x1b]12;rgb:28/2a/36\x1b\\",
            "the tint writes the cursor's rgb in OSC 12's shape"
        );
        assert_eq!(
            cursor_reset_sequence(),
            "\x1b]112\x1b\\",
            "the reset is OSC 112, parameterless"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_signal_restore_bytes_carry_the_full_teardown() {
        let with_background = signal_restore_bytes(Some((0x28, 0x2a, 0x36)));
        assert!(
            with_background.starts_with(b"\x1b]11;rgb:28/2a/36\x1b\\"),
            "the original background leads: {:?}",
            String::from_utf8_lossy(&with_background)
        );
        let without_background = signal_restore_bytes(None);
        assert!(
            !without_background.windows(5).any(|w| w == b"\x1b]11;"),
            "no background override was captured, none goes back: {:?}",
            String::from_utf8_lossy(&without_background)
        );
        for bytes in [&with_background, &without_background] {
            assert!(
                bytes.windows(8).any(|w| w == b"\x1b[?1049l"),
                "the alternate screen exit must be in the byte string"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn arming_is_idempotent_and_deaf_windows_toggle() {
        let _signal_state = SIGNAL_STATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        arm_fatal_signal_restore();
        arm_fatal_signal_restore();
        enter_signal_deaf_window();
        assert!(SIGNAL_RESTORE_DEAF.load(Ordering::SeqCst));
        exit_signal_deaf_window();
        assert!(!SIGNAL_RESTORE_DEAF.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn an_ignored_signal_cannot_take_the_deaf_exit() {
        // The exact interleaving the exit lines live in: the handler
        // armed, the deaf flag the ordinary restore leaves standing,
        // and the stand-down that must own whatever arrives next. A
        // delivered SIGINT there must not run the deaf path — this
        // test's own survival is the assertion, so a regression shows
        // as the test process dying 130 mid-run.
        let _signal_state = SIGNAL_STATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior_sigint = current_disposition(libc::SIGINT);
        let prior_sigterm = current_disposition(libc::SIGTERM);
        arm_fatal_signal_restore();
        SIGNAL_RESTORE_DEAF.store(true, Ordering::SeqCst);
        ignore_fatal_signals();
        // SAFETY: the pid is this process, and SIGINT's disposition is
        // SIG_IGN; if the stand-down failed, the deaf handler's
        // `_exit(130)` ends the test binary here.
        unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
        // The signal was ignored, so the process is alive to continue.
        // A thread blocking SIGINT may hold the delivery pending
        // instead of discarded — drain it before the dispositions it
        // ignored go back to what this test found, or the orphan would
        // land on the restored terminate disposition.
        drain_pending(libc::SIGINT);
        drain_pending(libc::SIGTERM);
        reinstall_disposition(libc::SIGINT, prior_sigint);
        reinstall_disposition(libc::SIGTERM, prior_sigterm);
        assert_eq!(
            current_disposition(libc::SIGINT).sa_sigaction,
            prior_sigint.sa_sigaction,
            "SIGINT's disposition stands where the test found it"
        );
        assert_eq!(
            current_disposition(libc::SIGTERM).sa_sigaction,
            prior_sigterm.sa_sigaction,
            "SIGTERM's disposition stands where the test found it"
        );
        SIGNAL_RESTORE_DEAF.store(false, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[test]
    fn the_alternate_scroll_sequences_are_exact_complements() {
        assert_eq!(alternate_scroll_sequence(true), "\x1b[?1007h");
        assert_eq!(alternate_scroll_sequence(false), "\x1b[?1007l");
    }

    #[test]
    fn the_set_sequence_matches_the_verified_terminal_app_bytes() {
        assert_eq!(
            background_sequence((0x28, 0x2a, 0x36)),
            "\x1b]11;rgb:28/2a/36\x1b\\"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_bel_terminated_reply_parses_its_channels() {
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:28/2a/36\x07"),
            Some((0x28, 0x2a, 0x36))
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_st_terminated_reply_parses_too() {
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:28/2a/36\x1b\\"),
            Some((0x28, 0x2a, 0x36))
        );
    }

    #[cfg(unix)]
    #[test]
    fn four_digit_channels_scale_down_to_eight_bits() {
        // The spec scales each channel to 16 bits; four hex digits are
        // the full-precision form and must land on the same eight-bit
        // color the two-digit form names directly.
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:2828/2a2a/3636\x07"),
            Some((0x28, 0x2a, 0x36))
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_digit_channels_scale_up_to_eight_bits() {
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:2/2/2\x07"),
            Some((34, 34, 34))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_set_sequence_round_trips_through_the_reply_parser() {
        // The set the session writes and the reply it parses are the
        // same wire shape, so what restore puts back is exactly what
        // query took out.
        for channels in [(0x28, 0x2a, 0x36), (0, 0, 0), (255, 255, 255)] {
            let sequence = background_sequence(channels);
            assert_eq!(
                parse_background_reply(sequence.as_bytes()),
                Some(channels),
                "{sequence:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn anything_that_is_not_a_complete_background_reply_is_not_one() {
        for noise in [
            &b""[..],
            b"garbage",
            b"\x1b]11;rgb:28/2a/36",
            b"\x1b]10;rgb:00/00/00\x07",
            b"\x1b]11;rgb:28/2a\x07",
            b"\x1b]11;rgb:zz/2a/36\x07",
        ] {
            assert_eq!(parse_background_reply(noise), None, "{noise:?}");
        }
    }
}

// Test code panics by design — its asserts are the assertions.
#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod write_only_warranted_pins {
    use super::*;

    #[test]
    fn the_write_gate_mirrors_the_plan_without_capturing() {
        // The predicate is the write's own gate answered purely: with a
        // captured original it follows the plan for every canvas shape,
        // and in this test process — where no query ever captured an
        // original — it must decline everything, exactly as the write
        // itself does, so a caller may defer to the flush on its word.
        let captured = ORIGINAL_BACKGROUND.get().is_some();
        let lands = |background| {
            matches!(
                live_override_plan(true, background),
                LiveOverridePlan::Write | LiveOverridePlan::RestoreOriginal
            )
        };
        let rgb = Color::Rgb(1, 2, 3);
        assert_eq!(write_only_warranted(rgb), captured && lands(rgb));
        let indexed = Color::Indexed(8);
        assert_eq!(write_only_warranted(indexed), captured && lands(indexed));
        // No capture, no write — the OnceLock stays unset in tests so
        // every shape declines, the same answer the theme burst's
        // canvas write gives for an uncaptured terminal.
        if ORIGINAL_BACKGROUND.get().is_none() {
            assert!(
                !write_only_warranted(rgb),
                "an uncaptured terminal declines a write"
            );
            assert!(!write_only_warranted(Color::Reset));
        }
    }
}

// Test code panics by design — its asserts are the assertions.
#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod synchronized_output_pins {
    use super::*;

    // The markers' byte shapes are the contract: a terminal that
    // implements mode 2026 recognizes exactly these two sequences, and
    // anything else would silently disable the one-repaint window the
    // theme burst rides. The writers spell nothing of their own — they
    // write these constants, so pinning the constants pins the wire.
    #[test]
    fn the_window_markers_are_the_2026_set_and_reset() {
        assert_eq!(SYNCHRONIZED_OUTPUT_BEGIN, "\x1b[?2026h");
        assert_eq!(SYNCHRONIZED_OUTPUT_END, "\x1b[?2026l");
    }

    #[test]
    fn the_burst_closer_carries_canvas_cursor_and_window_close() {
        // Captured terminal, RGB palette: the canvas write, the accent
        // cursor tint, and the window close — that order, one buffer,
        // so a terminal painting as it reads meets the flip once, at
        // the burst's end, never as a head start the composer's rows
        // would trail.
        let original = Some((16, 16, 16));
        let burst = theme_burst_bytes(original, Color::Rgb(40, 40, 40), Color::Rgb(177, 98, 134));
        assert!(burst.starts_with("\x1b]11;rgb:28/28/28\x1b\\"));
        assert!(burst.contains("\x1b]12;rgb:b1/62/86\x1b\\"));
        assert!(burst.ends_with(SYNCHRONIZED_OUTPUT_END));
        // Two OSC sequences (opener and ST each) plus the one CSI
        // close — nothing else rides the buffer.
        assert_eq!(burst.matches('\x1b').count(), 5);

        // Captured terminal, paintless palette: the terminal's own
        // canvas restored, its cursor reset, and the close.
        let reset = theme_burst_bytes(original, Color::Reset, Color::Rgb(1, 2, 3));
        assert!(reset.starts_with("\x1b]11;rgb:10/10/10\x1b\\"));
        assert!(reset.contains("\x1b]112\x1b\\"));
        assert!(reset.ends_with(SYNCHRONIZED_OUTPUT_END));

        // Uncaptured terminal: neither write is warranted — the close
        // alone, so the window markers always pair.
        let declined = theme_burst_bytes(None, Color::Rgb(40, 40, 40), Color::Rgb(1, 2, 3));
        assert_eq!(declined, SYNCHRONIZED_OUTPUT_END);
    }
}
