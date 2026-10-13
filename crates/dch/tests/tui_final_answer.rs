//! The final answer renders: a run whose closing turn reasons first
//! must still show its text once the run ends.
//!
//! The incident shape: the transcript and machine half carry the
//! final thinking-plus-text turn, but the conversation pane stops at
//! the collapsed thinking row and the run-duration line — the answer
//! exists everywhere except on screen. This pin drives the real
//! binary through a thinking-heavy tool run that closes on a
//! reasoning-then-answer turn and asserts the answer's words are on
//! the terminal when the run ends.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use common::sse_text_turn;
use common::sse_thinking_text_turn;
use common::sse_thinking_tool_turn;
use serde_json::json;
use std::time::Duration;

#[test]
fn the_final_answer_after_thinking_stays_on_screen() {
    // The incident shape: a marathon of reasoning-then-tool turns —
    // each one folding the live thinking lane — closed by a final
    // reasoning turn whose answer is long, markdown-structured text,
    // the kind whose streaming freeze engages before graduation.
    let answer = "## Final report\n\nThe wrap-up answer the display must keep: \
        inspection finished cleanly.\n\n- one finding held\n- another held too\n\n \
        Done for now, with enough lines that the streaming freeze settles \
        blocks before the run ends and the graduation moves the text into \
        the conversation.";
    let sb = common::Sandbox::with_server(common::CannedServer::sse(vec![
        sse_thinking_tool_turn("planning pass one", "Read", &json!({"path": "notes.txt"})),
        sse_thinking_tool_turn("planning pass two", "Read", &json!({"path": "notes.txt"})),
        sse_thinking_text_turn("wrapping up: state what was checked and conclude", answer),
        sse_text_turn("held open"),
    ]));
    std::fs::write(sb.workdir().join("notes.txt"), "some notes\n").expect("fixture written");
    let mut session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    session.send(b"inspect the notes");
    session.send(b"\r");
    session.expect(
        "The wrap-up answer the display must keep",
        Duration::from_secs(30),
    );
    session.quit_and_finish(Duration::from_secs(20));
}
