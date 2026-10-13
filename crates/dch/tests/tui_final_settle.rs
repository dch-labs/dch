//! The finished run's final answer renders without any input.
//!
//! The incident shape: the run ends after a thinking-then-answer turn
//! and the pane stops at the thinking row; the answer only appears
//! when the user types. A draw is the only thing that lands a
//! graduation, so this pin lets the run finish, waits out several
//! caret-blink frames with no input at all, and then asserts the
//! answer is on the screen — if a wake can be lost, the blink tick
//! must still carry the frame.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use common::sse_thinking_text_turn;
use common::sse_tool_call_turn;
use serde_json::json;
use std::time::Duration;

#[test]
fn the_final_answer_renders_with_no_input_after_the_run_ends() {
    let answer = "## Settled report\n\nThe settle-probe answer must appear unaided: \
        the run closed, the blink frames owe the repaint, and no keystroke \
        arrives to rescue it.\n\n- finding one\n- finding two";
    let sb = common::Sandbox::with_server(common::CannedServer::sse(vec![
        sse_tool_call_turn("Read", &json!({"path": "notes.txt"})),
        sse_thinking_text_turn("closing out the turn now", answer),
    ]));
    std::fs::write(sb.workdir().join("notes.txt"), "some notes\n").expect("fixture");

    let session = sb.dch_pty(&[]);
    let mut session = session;
    session.expect("test-model", Duration::from_secs(20));
    session.send(b"run it");
    session.send(b"\r");
    // Wait for the run to close: the run-duration line graduates with
    // the reply, so its appearance means the engine finished.
    session.expect("Settled report", Duration::from_secs(40));
    // Now idle long enough for several blink frames (500 ms each) with
    // no input, then snapshot: the answer must already be visible.
    std::thread::sleep(Duration::from_millis(2500));
    let screen = session.snapshot();
    let plain: String = {
        let mut out = String::new();
        let mut chars = screen.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    let words: Vec<&str> = plain.split_whitespace().collect();
    let joined = words.join(" ");
    assert!(
        joined.contains("must appear unaided"),
        "the answer renders with zero input after the run ends: {joined:?}"
    );
    session.quit_and_finish(Duration::from_secs(20));
}
