//! The TUI binary under fatal signals leaves the terminal restored.
//!
//! Two PTY scenarios drive the real binary to its idle full-screen
//! session and then repeat a signal until the child dies: SIGTERM and
//! SIGINT. Both must exit 130 — the forced-exit contract — and both
//! must have written the alternate-screen teardown to the wire, which
//! is the observable proof that terminal restoration ran on the signal
//! path instead of the process dying at the default disposition.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use std::time::Duration;
use std::time::Instant;

use common::PtySession;
use common::Sandbox;

/// The alternate screen's enter sequence — the session's "I am up".
const ALT_SCREEN_ENTER: &str = "\x1b[?1049h";

/// The alternate screen's leave sequence — the restore's observable half.
const ALT_SCREEN_LEAVE: &str = "\x1b[?1049l";

/// Wait until the raw output stream carries `needle`.
///
/// The raw stream, not the stripped screen: the alt-screen sequences
/// this scenario pins are escape bytes, invisible to the stripped
/// matcher [`PtySession::expect`] uses.
fn wait_for_wire(session: &PtySession, needle: &str, timeout: Duration) {
    let deadline = Instant::now()
        .checked_add(timeout)
        .expect("deadline computable");
    loop {
        if session.snapshot().contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the wire never carried {needle:?}; output so far:\n{}",
            session.snapshot()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Repeat `signal` until the child dies, then return its exit status.
///
/// The repeat is what makes the forced double-press reachable, and the
/// between-press reaps keep a reaped pid from ever being signaled
/// again — the kernel may have handed it to another process.
fn force_until_dead(session: &mut PtySession, signal: libc::c_int) -> portable_pty::ExitStatus {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(15))
        .expect("deadline computable");
    loop {
        assert!(
            Instant::now() < deadline,
            "the tui never exited under repeated signals"
        );
        // SAFETY: the pid names this session's child, proven still live
        // by the reap check before the first kill of each pass.
        let delivered = unsafe { libc::kill(session.pid().cast_signed(), signal) } == 0;
        if delivered {
            for _ in 0..10 {
                if session.try_reap().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if let Some(status) = session.try_reap() {
            return status;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn interrupt_scenario(signal: libc::c_int) {
    let sandbox = Sandbox::sse(Vec::new());
    let mut session = sandbox.dch_pty(&[]);
    wait_for_wire(&session, ALT_SCREEN_ENTER, Duration::from_secs(30));
    let status = force_until_dead(&mut session, signal);
    assert_eq!(status.exit_code(), 130, "the forced-exit contract");
    assert!(
        session.snapshot().contains(ALT_SCREEN_LEAVE),
        "the terminal teardown must ride the signal path; output:\n{}",
        session.snapshot()
    );
}

#[test]
fn repeated_sigterm_exits_130_and_restores_the_alt_screen() {
    interrupt_scenario(libc::SIGTERM);
}

#[test]
fn repeated_sigint_exits_130_and_restores_the_alt_screen() {
    interrupt_scenario(libc::SIGINT);
}
