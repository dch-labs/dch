//! The machine-history snapshot outlives a cancelled run.
//!
//! One PTY scenario holds the provider stream open with a deliberately
//! slow scripted endpoint, cancels the in-flight run with a single
//! interrupt, and tears down with the quit chord. A cancelled run has
//! still resolved — and every resolved exit path persists the
//! machine-history sibling beside the transcript, so the assertion is
//! simply that the sibling exists on disk once the session has exited.
//!
//! Two more scenarios pin the turn-end publish: a process killed with
//! `SIGKILL` mid-run still leaves the last completed turn's exchange
//! on disk (the publish that happens while the run future is still
//! owned), and a compaction that completed before a cancelled turn
//! survives to a resume that never recompacts.

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

/// The one saved session id under the sandbox's isolated home.
fn sole_session_id(sb: &common::Sandbox) -> String {
    std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name != "README.md")
        .expect("one saved session id")
}

#[test]
fn a_cancelled_run_still_leaves_the_machine_history_sibling() {
    let sb = common::Sandbox::with_server(common::CannedServer::sse_delayed(
        vec![sse_text_turn("never lands")],
        Duration::from_secs(30),
    ));
    let session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    session.send(b"hold my stream");
    session.send(b"\r");
    // Let the request reach the held-open endpoint before cancelling,
    // so the interrupt resolves a genuinely in-flight run.
    std::thread::sleep(Duration::from_secs(2));
    session.send(b"\x03");
    session.expect("Agent cancelled", Duration::from_secs(15));
    session.quit_and_finish(Duration::from_secs(20));

    let id = sole_session_id(&sb);
    let machine = sb.sessions_dir().join(&id).join("machine.json");
    let raw = std::fs::read_to_string(&machine)
        .expect("the machine sibling exists after the cancelled run");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("the sibling parses");
    assert_eq!(
        value.get("format").and_then(serde_json::Value::as_str),
        Some("dch.machine.v1"),
        "{value:?}"
    );
    assert!(
        value
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|messages| !messages.is_empty()),
        "the sibling carries the run's committed history: {value:?}"
    );
}

#[test]
fn a_kill_mid_run_leaves_the_last_completed_turn_on_disk() {
    let sb = common::Sandbox::with_server(common::CannedServer::sse_scheduled(vec![
        (
            sse_tool_call_turn("Read", &json!({ "path": "note.txt" })),
            Duration::ZERO,
        ),
        (sse_text_turn("held open"), Duration::from_secs(60)),
    ]));
    std::fs::write(sb.workdir().join("note.txt"), "probe payload\n").expect("note written");

    let session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    session.send(b"read the note");
    session.send(b"\r");
    session.expect("note.txt", Duration::from_secs(30));
    // The frame that rendered the tool line also fired the turn-end
    // publish; give the writer thread its moment before the kill.
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: the pid names this session's child, proven still live
    // by the expect that just matched its rendered output.
    let delivered = unsafe { libc::kill(session.pid().cast_signed(), libc::SIGKILL) == 0 };
    assert!(delivered, "the kill signal was sent");
    drop(session.finish(Duration::from_secs(10)));

    let id = sole_session_id(&sb);
    let machine = sb.sessions_dir().join(&id).join("machine.json");
    let raw = std::fs::read_to_string(&machine)
        .expect("the machine sibling exists despite the mid-run kill");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("the sibling parses");
    let messages = value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .expect("the sibling carries messages");
    assert!(
        messages.len() >= 3,
        "turn 1's whole exchange committed before the kill: {value:?}"
    );
    assert!(
        raw.contains("probe payload"),
        "the tool result rode the committed history: {value:?}"
    );
}

#[test]
fn a_compacted_machine_half_survives_a_cancelled_run_and_resumes_clean() {
    // Growth mirrors the proven compaction recipes: chunks read in
    // turns until the second submission crosses the threshold, the
    // declining summarizer calls eat scripted bodies on their way to
    // the truncating terminal, and the run closes on its text turn —
    // the turn whose end publishes the compacted machine half. The
    // chunk supply ahead of the closing text body is deliberately
    // generous: every summarizer decline consumes a scripted body,
    // and the closing body must survive to be served to the engine.
    // The reads alternate between two files so no identical-call
    // streak forms — the loop detector ends a run at the tenth
    // repeat of one call. The window is sized so the compacted half
    // plus the resumed engine's request overhead sits well under the
    // trigger again — a machine half restored at the threshold's
    // edge would legitimately compact at the next run start and the
    // resume assertion would test nothing. The next submission is
    // held by the server, cancelled mid-turn, and the quit tears
    // down; what must survive is the compacted set, and the resume
    // from it must submit without ever recompacting.
    let chunk = "payload line for the pty compaction budget\n".repeat(400);
    let names = ["chunk-a.txt", "chunk-b.txt", "chunk-c.txt", "chunk-d.txt"];
    let mut script = Vec::new();
    for _ in 0..3 {
        for name in names {
            script.push((
                sse_tool_call_turn("Read", &json!({"path": name})),
                Duration::ZERO,
            ));
        }
    }
    script.push((sse_text_turn("checkpoint"), Duration::ZERO));
    for _ in 0..3 {
        for name in names {
            script.push((
                sse_tool_call_turn("Read", &json!({"path": name})),
                Duration::ZERO,
            ));
        }
    }
    script.push((sse_text_turn("all done"), Duration::ZERO));
    script.push((sse_text_turn("held open"), Duration::from_secs(60)));
    // The strict default selection (qa) lands its own pass: the
    // non-stream summarizer call answers with a parseable completion
    // so the compacted machine half is genuinely compacted.
    let nonstream = "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":\
     [{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"compact summary\"},\
     \"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}"
        .to_string();
    let sb = common::Sandbox::sse_scheduled_nonstream_configured(
        script,
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
    session.send(b"grow the context");
    session.send(b"\r");
    session.expect("checkpoint", Duration::from_secs(30));
    session.send(b"keep reading");
    session.send(b"\r");
    session.expect("all done", Duration::from_secs(60));
    session.send(b"one more");
    session.send(b"\r");
    std::thread::sleep(Duration::from_secs(2));
    session.send(b"\x03");
    session.expect("Agent cancelled", Duration::from_secs(15));
    session.quit_and_finish(Duration::from_secs(20));

    let id = sole_session_id(&sb);
    let machine = sb.sessions_dir().join(&id).join("machine.json");
    let raw = std::fs::read_to_string(&machine)
        .expect("the machine sibling exists after the cancelled run");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("the sibling parses");
    let count = value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    assert!(
        count <= 20,
        "the persisted machine half is the compacted set, not the \
         uncompacted marathon ({count} messages): {value:?}"
    );

    let second = sb.dch_pty(&["--resume", &id]);
    second.expect("test-model", Duration::from_secs(20));
    let mut second = second;
    second.send(b"after the resume");
    second.send(b"\r");
    // A derive-then-compact resume shows its notice within a second of
    // the run starting; a machine-restored one goes straight to the
    // (spent) endpoint. Hold long enough to see either, assert the
    // absence of the compaction path.
    std::thread::sleep(Duration::from_secs(8));
    let screen = second.snapshot();
    assert!(
        !screen.contains("Compacting"),
        "a resume from the compacted machine half must not re-compact: {screen}"
    );
    second.quit_and_finish(Duration::from_secs(20));
}

#[test]
fn an_error_ended_session_resumes_from_the_machine_snapshot_without_recompacting() {
    // A session whose last run failed still resolved, so its machine
    // half is on disk (pinned above). Resuming must restore it rather
    // than re-derive the transcript and emergency-compact: the resumed
    // session's first run goes straight to the provider — the script
    // is spent, so it fails fast — and through all of that, no
    // compaction notice may ever render.
    let sb = common::Sandbox::with_server(common::CannedServer::sse(vec![sse_tool_call_turn(
        "Read",
        &json!({ "path": "note.txt" }),
    )]));
    std::fs::write(sb.workdir().join("note.txt"), "probe payload\n").expect("note written");

    let first = sb.dch_pty(&[]);
    first.expect("test-model", Duration::from_secs(20));
    let mut first = first;
    first.send(b"read the note");
    first.send(b"\r");
    first.expect("API error", Duration::from_secs(30));
    first.quit_and_finish(Duration::from_secs(20));

    let id = sole_session_id(&sb);
    assert!(
        sb.sessions_dir().join(&id).join("machine.json").exists(),
        "the failed run left the machine sibling"
    );

    let second = sb.dch_pty(&["--resume", &id]);
    second.expect("test-model", Duration::from_secs(20));
    let mut second = second;
    second.send(b"continue");
    second.send(b"\r");
    // A derive-then-compact resume shows its live-lane row within a
    // second of the run starting; a machine-restored one goes straight
    // to the (spent) endpoint. Hold long enough to see either, assert
    // the absence of the compaction path.
    std::thread::sleep(Duration::from_secs(8));
    let screen = second.snapshot();
    assert!(
        !screen.contains("Compacting"),
        "a machine-restored resume must not re-derive and re-compact: {screen}"
    );
    second.quit_and_finish(Duration::from_secs(20));
}
