//! The terminal-state contract under a multiplexer, driven end to end.
//!
//! One PTY scenario stands the real binary inside tmux's own
//! environment spellings — `TMUX` set, the terminal typed
//! `tmux-256color` — and pins the contract on the wire: a pane owns
//! its canvas and writes no terminal state at all. It asks nothing
//! (`OSC 11` query), sets nothing, restores nothing, tints no cursor
//! (`OSC 12`/`OSC 112`) — from launch through a committed theme switch
//! to the exit drain. The frame paints its canvas under its own cells
//! instead, so the pane keeps the client's default background and no
//! erase the multiplexer performs on its own — a resize reflow, a
//! display-panes overlay's clear, the alternate-screen handoff — can
//! show the theme's color where the frame did not put it.
//!
//! The twin scenario holds the same contract against a harness that
//! WOULD answer a query the moment it left the pane: the watcher stays
//! armed and no byte of the exchange ever runs, proving the pane's
//! silence is its own decision rather than a silent terminal's.
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
/// a pin over the wire's own bytes — an SGR color, an OSC shape —
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

#[test]
fn a_tmux_pane_asks_nothing_and_writes_no_terminal_state() {
    // The contract, proven through the whole stack — binary, driver,
    // terminal writer: launched under tmux's spellings, the pane
    // writes no byte of the terminal-state families, whatever a theme
    // switch does, and the exit drain is just as empty.
    let sb = common::Sandbox::sse(Vec::new());
    let mut session = sb.dch_pty_in_env(
        &[],
        &[
            ("TMUX", "/tmp/tmux-0/default,12345,0"),
            ("TERM", "tmux-256color"),
        ],
    );
    session.expect("test-model", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(600));
    let startup = session.snapshot();
    assert!(
        !startup.contains("\x1b]11;?"),
        "a pane asks nothing — the exchange is not its to run: {startup:?}"
    );
    assert!(
        !startup.contains("\x1b]11;rgb:"),
        "a pane sets no default — the multiplexer's erases keep the client's own: {startup:?}"
    );
    assert!(
        !startup.contains("\x1b]12;"),
        "no cursor tint from a pane: {startup:?}"
    );
    submit(&mut session, "/theme github_light");
    std::thread::sleep(Duration::from_millis(900));
    let switched = session.snapshot();
    assert!(
        !switched.contains("\x1b]11;rgb:"),
        "the switch writes no default either: {switched:?}"
    );
    assert!(
        !switched.contains("\x1b]12;"),
        "no cursor tint on the switch: {switched:?}"
    );
    let (_, drained) = session.quit_and_read(Duration::from_secs(20));
    assert!(
        !drained.contains("\x1b]11;"),
        "the exit restores nothing it never set: {drained:?}"
    );
    assert!(
        !drained.contains("\x1b]12;") && !drained.contains("\x1b]112"),
        "no cursor tint or reset leaves the pane: {drained:?}"
    );
}

/// A multiplexed pane owns its canvas and writes no terminal state at
/// all — not even against a terminal that would answer.
///
/// A pane's `OSC 11` set retargets the pane's default inside the
/// multiplexer's grid, and every erase the multiplexer performs on its
/// own — the reflow of a resized window, the clear of a display-panes
/// overlay, the alternate-screen handoff at exit — then paints with
/// the theme's canvas where the frame never drew: other panes
/// mid-redraw, newly exposed rows, the tab the client switches to. So
/// a multiplexed session runs none of the exchange: it asks nothing,
/// sets nothing, restores nothing, and tints no cursor (tmux re-emits
/// a pane's `OSC 12` to the client on every redraw the cursor's pane
/// takes). The frame paints its canvas under its own cells instead,
/// so the pane keeps the client's default and shows one canvas
/// whatever the client keeps as its own — launch to exit.
#[test]
fn a_multiplexed_pane_owns_its_canvas_and_writes_no_terminal_state() {
    // The whole stack against a harness that WOULD answer a query: no
    // query ever leaves, so nothing is ever written, and the grounding
    // paints the canvas on the blank rows the widgets left default.
    let sb = common::Sandbox::sse(Vec::new());
    let mut session = sb.dch_pty_in_env(
        &[],
        &[
            ("TMUX", "/tmp/tmux-0/default,12345,0"),
            ("TERM", "tmux-256color"),
        ],
    );
    // The watcher stays armed: had a query left the pane, the exchange
    // would have run — and the pins below would catch its bytes.
    session.answer_background_query("ffff/ffff/ffff", Duration::from_secs(30));
    session.expect("test-model", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(600));
    let startup = session.snapshot();
    assert!(
        !startup.contains("\x1b]11;?"),
        "a pane asks nothing — the exchange is not its to run: {startup:?}"
    );
    assert!(
        !startup.contains("\x1b]11;rgb:"),
        "a pane sets no default — the multiplexer's erases keep the client's own: {startup:?}"
    );
    assert!(
        !startup.contains("\x1b]12;"),
        "a multiplexed pane never tints the client-shared cursor: {startup:?}"
    );
    // The pane grounds its own canvas instead: blank rows carry the
    // canvas as an explicit background rather than the terminal
    // default, because a multiplexer hands default cells to the client
    // as the client's own default — whatever background that is would
    // otherwise show through the frame's blanks.
    assert!(
        startup.contains("48;2;12;12;12m                "),
        "a grounded pane paints its canvas on the blank rows: {startup:?}"
    );
    submit(&mut session, "/theme dracula");
    let switched = await_raw(&session, "48;2;40;42;54m", Duration::from_secs(5));
    let tail = &switched[startup.len().min(switched.len())..];
    assert!(
        !tail.contains("\x1b]11;") && !tail.contains("\x1b]12;"),
        "the committed switch repaints the canvas on the cells and writes no terminal state: {tail:?}"
    );
    let (_, drained) = session.quit_and_read(Duration::from_secs(20));
    assert!(
        !drained.contains("\x1b]11;"),
        "the exit restores nothing — nothing was ever set: {drained:?}"
    );
    assert!(
        !drained.contains("\x1b]12;") && !drained.contains("\x1b]112"),
        "no cursor tint or reset leaves the pane: {drained:?}"
    );
}
