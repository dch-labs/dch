//! The terminal-state exchange on a direct terminal, driven end to end.
//!
//! One PTY scenario stands the real binary in a direct terminal whose
//! harness answers the background query the moment it leaves: the
//! startup exchange installs the theme's canvas, tints the cursor, and
//! the exit restores both — the answered half of the contract, pinned
//! on the wire.
//!
//! The twin scenario walks the deferred half: input keeps arriving
//! through the startup query's window (focus events, which the session
//! consumes without touching the composer), so the session opens
//! never-asked — and the committed theme switch then asks under the
//! quiesce handshake. The switch's canvas write must land inside the
//! synchronized-output burst that carries the re-styled cells, exactly
//! once, never duplicated at the question's moment: a canvas flipped
//! at keypress time would beat the text it must land beside.
//!
//! A browse preview rides the same contract: an arrow step parks the
//! previewed canvas and the flush writes it inside its own window —
//! the write-at-flush half of the preview, on the wire. A harness pin
//! rides beside them: the input flood owes the keystroke lane
//! nothing, so it runs to its deadline while the lane is held — the
//! discipline that keeps a scenario's submit from racing a blocked
//! flood under load.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use std::time::Duration;

/// Type a command and submit it.
///
/// The keystroke form of the composer's submit: the text, then the
/// return that sends it.
fn submit(session: &mut common::PtySession, prompt: &str) {
    session.send(prompt.as_bytes());
    session.send(b"\r");
}

/// Wait until the raw terminal stream contains `needle`.
///
/// The counterpart [`common::PtySession::expect`] deserves for words:
/// that match runs over the screen with escape sequences stripped, so
/// a pin over the wire's own bytes — an OSC shape, a mode-2026 marker —
/// waits here instead, over the untouched stream.
fn await_raw(session: &common::PtySession, needle: &str, timeout: Duration) -> String {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .expect("deadline computable");
    loop {
        let snapshot = session.snapshot();
        if snapshot.contains(needle) {
            return snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the pty never wrote {needle:?}; stream so far:\n{snapshot}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until the raw stream carries `needle` at least `times` times.
///
/// The counterpart of [`await_raw`] for a write the stream already
/// carries — a canvas the startup installed, written again by a
/// restore — so a pin waits for the next occurrence rather than the
/// first.
fn await_raw_times(
    session: &common::PtySession,
    needle: &str,
    times: usize,
    timeout: Duration,
) -> String {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .expect("deadline computable");
    loop {
        let snapshot = session.snapshot();
        if snapshot.matches(needle).count() >= times {
            return snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the pty never wrote {needle:?} {times} times; stream so far:\n{snapshot}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The byte offset of `needle`'s zero-based `nth` occurrence.
///
/// A pin's helper for asserting on one write among several of the
/// same shape — the restore beside the install.
fn nth_offset(haystack: &str, needle: &str, nth: usize) -> Option<usize> {
    haystack.match_indices(needle).nth(nth).map(|(at, _)| at)
}

/// Wait until a spawned harness thread has finished.
///
/// The deadline-bounded stand-in for joining: a background thread may
/// block in its own writes — that is their design — and a hang must
/// surface as a bounded failure, not a stuck binary.
fn await_thread(thread: &std::thread::JoinHandle<()>, timeout: Duration) {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .expect("deadline computable");
    while !thread.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "the spawned thread never finished — the pty stopped draining"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_answered_startup_installs_the_canvas_and_tints_the_cursor() {
    // The answered half of the contract, through the whole stack: the
    // query leaves, the harness answers, and the startup installs the
    // theme's canvas as the terminal default beside the cursor tint —
    // both of which the exit gives back, the captured original and the
    // terminal's own cursor.
    let sb = common::Sandbox::sse(Vec::new());
    let session = sb.dch_pty_in_env(&[], &[]);
    session.answer_background_query("ffff/ffff/ffff", Duration::from_secs(30));
    let startup = await_raw(&session, "\x1b]11;rgb:0c/0c/0c", Duration::from_secs(20));
    assert!(
        startup.contains("\x1b]11;?"),
        "the question the answer rode in on left the pane: {startup:?}"
    );
    assert!(
        startup.contains("\x1b]12;rgb:ff/b2/00"),
        "the cursor takes the palette's accent tint: {startup:?}"
    );
    let (_, drained) = session.quit_and_read(Duration::from_secs(20));
    assert!(
        drained.contains("\x1b]11;rgb:ff/ff/ff"),
        "the exit restores the captured original background: {drained:?}"
    );
    assert!(
        drained.contains("\x1b]112"),
        "the exit resets the tinted cursor: {drained:?}"
    );
}

#[test]
fn a_committed_switch_writes_its_canvas_once_inside_the_burst() {
    // The deferred half, through its one mid-session question. Input
    // floods in across the startup query's window and the post-reader
    // retry's — focus-lost reports, which the session consumes without
    // a composer edit or a frame — so both stand down on waiting input
    // and the session stays never-asked. The committed switch then
    // asks under the quiesce handshake, and its palette lands through
    // one synchronized-output burst: the canvas written exactly once,
    // inside the open window, the cursor tint riding the same buffer.
    // A canvas written at the question's moment instead would show up
    // before the window opened or as a second write — either way this
    // pin catches it.
    let sb = common::Sandbox::sse(Vec::new());
    let mut session = sb.dch_pty_in_env(&[], &[]);
    session.answer_background_query("ffff/ffff/ffff", Duration::from_secs(60));
    let flood = session.flood_input(b"\x1b[O", Duration::from_millis(4000));
    await_thread(&flood, Duration::from_secs(20));
    let settled = session.snapshot();
    assert!(
        !settled.contains("\x1b]11;?"),
        "fixture premise: input kept every launch-time question from leaving — the \
         switch's ask is the session's first: {settled:?}"
    );
    submit(&mut session, "/theme nord");
    let switched = await_raw(&session, "\x1b]11;rgb:2e/34/40", Duration::from_secs(20));
    let canvas = "\x1b]11;rgb:2e/34/40";
    assert_eq!(
        switched.matches(canvas).count(),
        1,
        "the switched canvas is written exactly once — the burst's write, never a \
         second one at the question's moment: {switched:?}"
    );
    let at = switched.find(canvas).expect("just counted the one");
    let window = switched[..at]
        .rfind("\x1b[?2026h")
        .expect("the canvas rides inside a synchronized window");
    assert!(
        !switched[window..at].contains("\x1b[?2026l"),
        "the window stands open through the canvas write: {switched:?}"
    );
    assert!(
        switched[at..].contains("\x1b[?2026l"),
        "the window closes after the canvas lands: {switched:?}"
    );
    assert!(
        switched[window..].contains("\x1b]12;rgb:88/c0/d0"),
        "the nord cursor tint rides the same burst, after the canvas: {switched:?}"
    );
    let (_, drained) = session.quit_and_read(Duration::from_secs(20));
    assert!(
        drained.contains("\x1b]11;rgb:ff/ff/ff"),
        "the exit restores the captured original background: {drained:?}"
    );
    assert!(
        drained.contains("\x1b]112"),
        "the exit resets the tinted cursor: {drained:?}"
    );
}

#[test]
fn a_browse_preview_writes_its_canvas_inside_the_burst() {
    // The preview's terminal half on the wire: the first arrow step
    // lands on the second candidate — opencode, after the session's
    // own dch — and the next flush writes its canvas exactly once,
    // inside an open synchronized-output window, the cursor tint
    // riding the same burst: the write-at-flush contract, before any
    // commit. Esc then writes the session's own canvas back through a
    // second window of the same shape, so the terminal comes back
    // with the browse.
    let sb = common::Sandbox::sse(Vec::new());
    let mut session = sb.dch_pty_in_env(&[], &[]);
    session.answer_background_query("ffff/ffff/ffff", Duration::from_secs(60));
    let launched = await_raw(&session, "\x1b]11;rgb:0c/0c/0c", Duration::from_secs(20));
    let own_canvas = "\x1b]11;rgb:0c/0c/0c";
    let own_writes = launched.matches(own_canvas).count();
    session.send(b"/theme ");
    session.send(b"\x1b[B");
    let previewed_canvas = "\x1b]11;rgb:0e/0e/0e";
    let previewed = await_raw(&session, previewed_canvas, Duration::from_secs(20));
    assert_eq!(
        previewed.matches(previewed_canvas).count(),
        1,
        "the previewed canvas is written exactly once: {previewed:?}"
    );
    let at = previewed
        .find(previewed_canvas)
        .expect("just counted the one");
    let window = previewed[..at]
        .rfind("\x1b[?2026h")
        .expect("the preview's canvas rides inside a synchronized window");
    assert!(
        !previewed[window..at].contains("\x1b[?2026l"),
        "the window stands open through the preview's canvas write: {previewed:?}"
    );
    assert!(
        previewed[at..].contains("\x1b[?2026l"),
        "the window closes after the canvas lands: {previewed:?}"
    );
    assert!(
        previewed[window..].contains("\x1b]12;rgb:f8/b9/7f"),
        "the previewed cursor tint rides the same burst, after the canvas: {previewed:?}"
    );
    assert_eq!(
        previewed.matches(own_canvas).count(),
        own_writes,
        "the preview writes the previewed canvas, never the session's own: {previewed:?}"
    );
    session.send(b"\x1b");
    let restored = await_raw_times(
        &session,
        own_canvas,
        own_writes.saturating_add(1),
        Duration::from_secs(20),
    );
    assert_eq!(
        restored.matches(own_canvas).count(),
        own_writes.saturating_add(1),
        "the dismissed browse writes the session's canvas back exactly once: {restored:?}"
    );
    let again = nth_offset(&restored, own_canvas, own_writes).expect("counted the write");
    let back_window = restored[..again]
        .rfind("\x1b[?2026h")
        .expect("the restore rides inside its own synchronized window");
    assert!(
        !restored[back_window..again].contains("\x1b[?2026l"),
        "the restore's window stands open through its canvas write: {restored:?}"
    );
    assert!(
        restored[again..].contains("\x1b[?2026l"),
        "the restore's window closes after its canvas lands: {restored:?}"
    );
    let (_, drained) = session.quit_and_read(Duration::from_secs(20));
    assert!(
        drained.contains("\x1b]112"),
        "the exit resets the tinted cursor: {drained:?}"
    );
}

#[test]
fn a_flood_runs_to_its_deadline_while_the_keystroke_lane_is_held() {
    // The lock discipline the committed-switch scenario depends on:
    // the flood writes through the master's side lane, so holding the
    // keystroke lane — the lock a submit needs — cannot stall it.
    // Under the defect the flood blocked on the held lock and this
    // wait starved past its deadline; passing it is the pin.
    let sb = common::Sandbox::sse(Vec::new());
    let session = sb.dch_pty_in_env(&[], &[]);
    let held = session.keystroke_lane();
    let flood = session.flood_input(b"\x1b[O", Duration::from_millis(300));
    await_thread(&flood, Duration::from_secs(15));
    drop(held);
    let _ = session.quit_and_read(Duration::from_secs(20));
}
