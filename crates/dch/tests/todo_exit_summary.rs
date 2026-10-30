//! The session's exit summary, driven through a real run.
//!
//! The exit lines state one fact the driver captured — how many plan
//! entries its runs left undone — and the whole path from the settled
//! run to that line had no test: the formatter's own pins call it with
//! figures hand-picked from outside, so nothing proved the driver ever
//! recorded a real one, nor that an exit reading nothing refuses to
//! claim a clean plan. These scenarios run the binary on a pseudoterminal
//! against the scripted provider, file a plan, quit, and read what the
//! terminal printed after the screen came back.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use common::sse_text_turn;
use common::sse_tool_call_turn;
use serde_json::json;
use std::time::Duration;

/// Open the TUI, submit `prompt`, and wait until the run is underway.
fn open_and_submit(sb: &common::Sandbox, prompt: &str) -> common::PtySession {
    let mut session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    session.send(prompt.as_bytes());
    session.send(b"\r");
    session
}

#[test]
fn a_session_that_ends_with_an_undone_entry_says_so_when_it_exits() {
    // The shape the exit line exists for: the plan is filed, the work
    // answers, and nothing closes the entry — the engine offers its one
    // follow-up per run and the follow-up's own end offers no other, so
    // a run can settle with the entry open. The exit owes the user that
    // fact beside the resume command, and it can only owe it truthfully
    // if the driver recorded the count when its run settled.
    let sb = common::Sandbox::sse(vec![
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "one", "subject": "File the report",
                               "status": "in_progress"}]}),
        ),
        sse_text_turn("filed, but not closed"),
        // The follow-up's answer: acknowledged, entry still open.
        sse_text_turn("acknowledged, still open"),
    ]);
    let session = open_and_submit(&sb, "file the report and leave it open");
    session.expect("File the report", Duration::from_secs(30));
    let (status, raw) = session.quit_and_read(Duration::from_secs(20));
    let screen = common::strip_ansi(&raw);
    assert!(status.success(), "the session exits cleanly: {screen}");
    assert!(
        screen.contains("1 todo item left unfinished"),
        "the settled run's undone count rides the exit lines: {screen}"
    );
    assert!(
        screen.contains("Resume with:"),
        "the resume command keeps its place beside the count: {screen}"
    );
    assert!(
        !screen.contains("before its plan was read"),
        "a run that settled has an answer to give: {screen}"
    );
}

#[test]
fn a_session_that_closes_its_plan_exits_without_an_unfinished_word() {
    // The other end of the same capture: a plan walked to completed
    // records a real zero, and the exit stays the plain two lines. Both
    // ends matter — a capture path that always said "open" and one that
    // always said "clean" fail this pair in opposite directions.
    let sb = common::Sandbox::sse(vec![
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "one", "subject": "Close the loop",
                               "status": "in_progress"}]}),
        ),
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "one", "status": "completed"}]}),
        ),
        sse_text_turn("closed out"),
    ]);
    let session = open_and_submit(&sb, "close the loop");
    session.expect("Close the loop", Duration::from_secs(30));
    let (status, raw) = session.quit_and_read(Duration::from_secs(20));
    let screen = common::strip_ansi(&raw);
    assert!(status.success(), "the session exits cleanly: {screen}");
    assert!(
        screen.contains("Resume with:"),
        "the resume command prints whatever the plan did: {screen}"
    );
    assert!(
        !screen.contains("left unfinished"),
        "a plan walked to completed leaves no unfinished word: {screen}"
    );
}
