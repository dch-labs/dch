//! End-to-end proofs of the session pair: exact resume through the
//! machine-history snapshot, and a compaction crossing shrinking the
//! follow-up request.
//!
//! Every scenario drives the real binary against the canned endpoint
//! with an isolated home, so what is asserted is what a user's next
//! `--resume` would actually see on the wire.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use common::Sandbox;
use common::sse_text_turn;
use common::sse_tool_call_turn;
use serde_json::json;

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_tool_marathon_session_resumes_through_compaction() {
    // The saved shape a long agent run leaves: one user task, then
    // dozens of assistant tool records with no user texts between
    // the replies. Resuming it must cross the compaction threshold
    // and shrink — the reconstruction alternates at exchange
    // granularity — instead of coalescing into one uncompactable
    // message that dies before the first request.
    let sb = Sandbox::sse_configured(
        vec![sse_text_turn("resumed")],
        "context_window = 40_000",
        "compactor = \"truncating\"",
    );
    let id = "11111111-2222-3333-4444-555555555555";
    let dir = sb.sessions_dir().join(id);
    std::fs::create_dir_all(&dir).expect("session dir");
    let payload = "marathon payload line for the resume budget\n".repeat(60);
    let mut messages = vec![json!({
        "role": "user",
        "text": "review everything",
        "timestamp": "2026-10-06T05:00:00Z",
    })];
    for index in 0..80u32 {
        messages.push(json!({
            "role": "assistant",
            "blocks": [{
                "type": "tool",
                "name": "Read",
                "call_id": format!("call-{index}"),
                "input_preview": "a.rs",
                "success": true,
                "elapsed_secs": 0.1,
                "output_preview": payload,
                "retained_input": "{\"path\": \"a.rs\"}",
            }],
            "timestamp": "2026-10-06T05:01:00Z",
        }));
    }
    let session = json!({
        "format": "dch.v1",
        "session_id": id,
        "model": "test-model",
        "saved_at": "2026-10-06T05:04:00Z",
        "messages": messages,
        "tokens": {
            "cumulative_input": 0,
            "cumulative_output": 0,
            "last_input_tokens": 0,
            "context_tokens": 0,
        },
    });
    std::fs::write(dir.join("session.json"), session.to_string()).expect("session written");

    let out = sb.dch(&["--resume", id, "continue"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the marathon session resumes: stderr: {}",
        lossy(&out.stderr)
    );
    let requests = sb.requests();
    let first = first_request(&requests);
    let carried = first.matches("marathon payload line").count();
    assert!(
        carried < 80 * 60,
        "the follow-up request carries the compacted history: {carried} payload lines"
    );
}

/// The first conversation request the resumed run made.
///
/// Compaction precedes the request, so the head of the recorded
/// stream is the compacted follow-up the pin measures.
fn first_request(requests: &[String]) -> &str {
    requests.first().map(String::as_str).unwrap_or_default()
}

/// The one saved session id under the sandbox's isolated home.
fn sole_session_id(sb: &Sandbox) -> String {
    std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name != "README.md")
        .expect("one saved session id")
}

#[test]
fn a_failed_run_still_leaves_the_machine_history_sibling() {
    // A run whose stream dies — the script runs dry, the retry ladder
    // exhausts — resolves in failure, and every resolved exit path
    // persists both halves: the transcript the user returns for and
    // the machine sibling the next resume restores from. The sibling
    // must carry the exchange that did complete before the failure.
    let sb = Sandbox::sse(vec![sse_tool_call_turn(
        "Read",
        &json!({"path": "note.txt"}),
    )]);
    std::fs::write(sb.workdir().join("note.txt"), "probe payload\n").expect("note written");
    let out = sb.dch(&["read the note"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "the run fails once the script runs dry: stderr: {}",
        lossy(&out.stderr)
    );
    let id = sole_session_id(&sb);
    let machine = sb.sessions_dir().join(&id).join("machine.json");
    let raw =
        std::fs::read_to_string(&machine).expect("the machine sibling exists after the failed run");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("the sibling parses");
    assert_eq!(
        value.get("format").and_then(serde_json::Value::as_str),
        Some("dch.machine.v1"),
        "{value:?}"
    );
    let messages = value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .expect("the sibling carries messages");
    assert!(
        messages.len() >= 3,
        "the completed exchange rides the sibling: {messages:?}"
    );
}

#[test]
fn a_resumed_session_carries_the_exact_working_set() {
    // The note is far longer than any display preview caps at, with a
    // sentinel sitting beyond every cap: a resume that derives from
    // the display transcript cannot carry it, so finding it on the
    // wire after resume proves the machine snapshot seeded the runner.
    let sentinel = "SENTINEL-deep-in-the-bytes-7f3a91";
    let note = format!(
        "{}\n{}\n",
        "filler line for the preview budget\n".repeat(80),
        sentinel
    );
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Read", &json!({"path": "note.txt"})),
        sse_text_turn("read it"),
        sse_text_turn("resumed answer"),
    ]);
    std::fs::write(sb.workdir().join("note.txt"), note).expect("note written");

    let first = sb.dch(&["read the note"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stderr: {}",
        lossy(&first.stderr)
    );

    let id = sole_session_id(&sb);
    let machine = sb.sessions_dir().join(&id).join("machine.json");
    assert!(
        machine.exists(),
        "a completed run leaves the machine-history sibling beside the display file"
    );

    let before = sb.requests().len();
    let second = sb.dch(&["--resume", &id, "continue"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "stderr: {}",
        lossy(&second.stderr)
    );
    let requests = sb.requests();
    let resumed = requests
        .get(before)
        .expect("the resumed session made its request");
    assert!(
        resumed.contains(sentinel),
        "the resumed request carries the full tool result, not a preview"
    );
    assert!(
        resumed.contains("call_1"),
        "the resumed request carries the model-issued call id"
    );
    assert!(
        !resumed.contains("resume-"),
        "no synthesized call id appears on a snapshot-seeded resume"
    );
}

#[test]
fn a_compacting_run_shrinks_the_follow_up_request() {
    // Distinctive chunk reads grow the context past the window; the
    // truncating compactor must drop the oldest exchanges before a
    // later request, so the final turn's request carries the tail
    // markers and not the head's. The window leaves the deep default
    // landing room for the terminal's preserved-tail floor — a window
    // whose floor cannot fit the landing is the truncator's refusal
    // shape, not this pin's claim.
    let mut script = Vec::new();
    for index in 0..24 {
        script.push(sse_tool_call_turn(
            "Read",
            &json!({"path": format!("chunk-{index}.txt")}),
        ));
    }
    script.push(sse_text_turn("done growing"));
    let sb = Sandbox::sse_configured(
        script,
        "context_window = 130000",
        "compactor = \"truncating\"",
    );
    for index in 0..24 {
        let body = format!("marker-{index} payload line for the budget\n").repeat(400);
        std::fs::write(sb.workdir().join(format!("chunk-{index}.txt")), body)
            .expect("chunk written");
    }
    let out = sb.dch(&["grow the context"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        lossy(&out.stderr),
        lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("done growing"),
        "the run completes across the compaction boundary"
    );

    let requests = sb.requests();
    let final_request = requests
        .last()
        .expect("the final text turn made its request");
    assert!(
        final_request.contains("marker-23"),
        "the preserved tail rides the compacted request"
    );
    assert!(
        !final_request.contains("marker-0"),
        "the dropped head is absent after compaction"
    );
    let mid_request = requests.get(2).expect("a pre-compaction request exists");
    assert!(
        mid_request.contains("marker-0"),
        "before the boundary the head is still on the wire"
    );
}
