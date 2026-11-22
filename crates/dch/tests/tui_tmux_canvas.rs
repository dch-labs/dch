//! The multiplexed canvas contract, driven end to end.
//!
//! One PTY scenario stands the real binary inside tmux's own
//! environment spellings — `TMUX` set, the terminal typed
//! `tmux-256color` — and pins the launch-time decision on the wire:
//! the pane's canvas is painted on the app's own cells (the theme's
//! RGB ground reaches the terminal as explicit cell backgrounds), and
//! not one OSC 11 or OSC 12 byte — the writes that retarget a
//! multiplexer's client-shared default background and cursor — ever
//! leaves the pane, whatever a theme switch does.

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
fn a_tmux_pane_paints_its_canvas_and_never_touches_the_client_default() {
    // The scene the unit pins model, proven through the whole stack —
    // binary, driver, render, terminal writer: launched under tmux's
    // spellings, the session paints the default dark canvas as
    // explicit cell backgrounds, and the light switch repaints the
    // light canvas the same way. The client-shared default is never
    // written at either step — an OSC 11 set would repaint every
    // other pane in the user's window, and a query would park the
    // reader for nothing — so neither byte shape may appear in the
    // stream at all.
    let sb = common::Sandbox::sse(Vec::new());
    let mut session = sb.dch_pty_in_env(
        &[],
        &[
            ("TMUX", "/tmp/tmux-0/default,12345,0"),
            ("TERM", "tmux-256color"),
        ],
    );
    session.expect("test-model", Duration::from_secs(20));
    let startup = await_raw(&session, "48;2;12;12;12", Duration::from_secs(5));
    assert!(
        !startup.contains("\x1b]11;"),
        "no OSC 11 byte — set or query — may leave a multiplexed pane: {startup:?}"
    );
    assert!(
        !startup.contains("\x1b]12;"),
        "no OSC 12 cursor tint may leave a multiplexed pane: {startup:?}"
    );
    submit(&mut session, "/theme github_light");
    let switched = await_raw(&session, "48;2;255;255;255", Duration::from_secs(5));
    assert!(
        !switched.contains("\x1b]11;"),
        "the light switch repaints cells, never the client default: {switched:?}"
    );
    session.quit_and_finish(Duration::from_secs(20));
}
