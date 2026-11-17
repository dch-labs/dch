//! The piped-stdin + task-argument combination, against the real binary.
//!
//! The unit matrix in `headless.rs` pins the resolution table at the
//! function seam; these cases pin the behavior the shell actually
//! drives — `ip a | dch "what is my ip"` — end to end: the real binary,
//! a real pipe on stdin, a scripted provider that records the request,
//! and the provider's own received prompt as the verdict. No E2E gate:
//! the server is local and scripted, the run is one turn, the cost is
//! milliseconds.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use common::CannedServer;
use common::Sandbox;
use common::sse_text_turn;
use std::time::Duration;

/// Wait until the provider has recorded its first request body.
fn await_first_request(sb: &Sandbox, timeout: Duration) {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .expect("deadline computable");
    while sb.requests().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the run never reached the provider"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The one request's prompt text — the user message the provider saw.
fn prompt_of(request: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(request).expect("the recorded request is JSON");
    parsed
        .get("messages")
        .and_then(|messages| messages.as_array())
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(|role| role.as_str()) == Some("user"))
        })
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn a_piped_stdin_and_a_task_reach_the_agent_as_one_labeled_prompt() {
    // The user's exact shape — `ip a | dch "what is my ip"` — must
    // deliver the pipe's output to the agent, not drop it in the
    // kernel buffer: the provider receives one user message carrying
    // the piped body under its label and the task under its own, in
    // that order.
    let sb = Sandbox::with_server(CannedServer::sse(vec![sse_text_turn("192.168.1.7")]));
    let output = sb.dch_stdin(
        &["what is my ip"],
        "1: lo: 127.0.0.1\n2: eth0: 192.168.1.7\n",
    );
    assert!(output.status.success(), "the run completes: {output:?}");
    await_first_request(&sb, Duration::from_secs(30));
    let prompt = prompt_of(&sb.requests().first().cloned().unwrap_or_default());
    let stdin_at = prompt.find("--- piped stdin ---");
    let body_at = prompt.find("2: eth0: 192.168.1.7");
    let task_at = prompt.find("--- task ---");
    let task_text_at = prompt.find("what is my ip");
    assert!(stdin_at.is_some(), "the scaffold labels the pipe: {prompt}");
    assert!(
        body_at.is_some(),
        "the pipe's body rides the prompt: {prompt}"
    );
    assert!(task_at.is_some(), "the scaffold labels the task: {prompt}");
    assert!(
        task_text_at.is_some(),
        "the task text rides the prompt: {prompt}"
    );
    let offsets: Vec<usize> = [stdin_at, body_at, task_at, task_text_at]
        .iter()
        .map(|found| found.unwrap_or(usize::MAX))
        .collect();
    let mut in_order = offsets.clone();
    in_order.sort_unstable();
    assert_eq!(
        offsets, in_order,
        "stdin first, task last — the instruction holds the recency position: {prompt}"
    );
}

#[test]
fn an_empty_pipe_with_a_task_sends_the_task_alone() {
    // `dch "task" < /dev/null`: the pipe is a pipe and is drained, but
    // it delivers nothing — the provider sees the task verbatim, with
    // no scaffold, the same prompt the old semantics sent.
    let sb = Sandbox::with_server(CannedServer::sse(vec![sse_text_turn("ok")]));
    let output = sb.dch_stdin(&["the task"], "");
    assert!(output.status.success(), "the run completes: {output:?}");
    await_first_request(&sb, Duration::from_secs(30));
    let prompt = prompt_of(&sb.requests().first().cloned().unwrap_or_default());
    assert_eq!(prompt, "the task", "no labels, no error — the task alone");
}

#[test]
fn piped_stdin_without_a_task_sends_the_body_alone() {
    // The unchanged half of the table: no task argument, a piped body
    // — the prompt is the body verbatim, trailing line breaks trimmed,
    // no scaffold labels.
    let sb = Sandbox::with_server(CannedServer::sse(vec![sse_text_turn("ok")]));
    let output = sb.dch_stdin(&[], "summarize this\n\nbody text\n");
    assert!(output.status.success(), "the run completes: {output:?}");
    await_first_request(&sb, Duration::from_secs(30));
    let prompt = prompt_of(&sb.requests().first().cloned().unwrap_or_default());
    assert_eq!(
        prompt, "summarize this\n\nbody text",
        "the piped body alone, trimmed — no labels"
    );
}
