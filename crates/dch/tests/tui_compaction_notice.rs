//! The compaction-in-progress row, driven end to end.
//!
//! One PTY scenario runs the real binary against a deliberately slow
//! scripted endpoint and asserts on what the terminal actually
//! painted: while a summarizer pass holds — the server sleeps before
//! every answer, so the pass spans seconds — the conversation's live
//! lane carries the running row in the thinking register (marker,
//! label, running duration), and once the chain settles the
//! transcript carries the compacted row. The pass-start and pass-end
//! state transitions are pinned at the observer layer; this file pins
//! the full stack — binary, observer, engine, render — on the wire.

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

/// Type a prompt and submit it.
///
/// The keystroke form of the composer's submit: the text, then the
/// return that sends it.
fn submit(session: &mut common::PtySession, prompt: &str) {
    session.send(prompt.as_bytes());
    session.send(b"\r");
}

fn chunk_turn(name: &str) -> String {
    sse_tool_call_turn("Read", &json!({"path": name}))
}

#[test]
fn a_running_compaction_paints_the_live_lane_over_a_real_pass() {
    // Growth mirrors the proven runner recipes: a chunk of known
    // payload read in turns, a mid-run text checkpoint, then a second
    // submission whose accumulated history crosses the threshold. The
    // script's run-2 half is all tool turns but the last, so wherever
    // the two declining summarizer calls land in the queue — the
    // canned server answers every connection with the next body, and
    // a stream body cannot parse as their completion — the
    // conversation still walks tool turns until the closing text
    // turn, and the pass still spans the server's sleep.
    let chunk = "payload line for the pty compaction budget\n".repeat(400);
    let names = ["chunk-a.txt", "chunk-b.txt", "chunk-c.txt", "chunk-d.txt"];
    let mut script = Vec::new();
    for _ in 0..6 {
        script.push(chunk_turn(names[0]));
        script.push(chunk_turn(names[1]));
        script.push(chunk_turn(names[2]));
        script.push(chunk_turn(names[3]));
    }
    script.push(sse_text_turn("all done"));
    // The strict default selection (qa) must land its own pass: the
    // non-stream summarizer call answers with a parseable completion —
    // held by the same delay, so the pass spans it and the live lane
    // paints while the summarizer works.
    let nonstream = "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":\
     [{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"compact summary\"},\
     \"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}"
        .to_string();
    let sb = common::Sandbox::sse_delayed_nonstream_configured(
        script,
        Duration::from_millis(1200),
        nonstream,
        "context_window = 130_000",
        "",
    );
    for name in names {
        std::fs::write(sb.workdir().join(name), &chunk).expect("chunk written");
    }

    let session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    submit(&mut session, "grow the context");

    session.expect("compacting", Duration::from_secs(40));
    session.expect("Compacted", Duration::from_secs(40));
    session.expect("all done", Duration::from_secs(40));
    session.quit_and_finish(Duration::from_secs(20));
}
