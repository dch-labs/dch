//! Real-provider and terminal-driven smoke cases for the release ritual.
//!
//! Every case is `#[ignore]`d out of the ordinary suite and runs only
//! under `DCH_E2E=1` (see `make smoke`): the first family spends tokens
//! against a live model, and the PTY family drives the TUI through a
//! real pseudoterminal — timing a terminal emulator into CI on every
//! push would trade the gate's cleanliness for coverage the automated
//! suite already holds at the engine layer. Byte-level asserts only:
//! these cases pin what crosses the wire, while the checklist's manual
//! rows keep the human's eye on what it looks like.

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
use std::time::Duration;

fn e2e_enabled() -> bool {
    std::env::var("DCH_E2E").is_ok_and(|value| value == "1")
}

/// One turn of the composer: type a prompt and submit it.
fn submit(session: &mut common::PtySession, prompt: &str) {
    session.send(prompt.as_bytes());
    session.send(b"\r");
}

/// Wait until the sandbox's provider has received the first request.
///
/// The request landing is the run-started proof: once the binary has
/// dialed the endpoint the turn is in flight, so an interrupt that
/// follows cancels a live run instead of racing the submit itself and
/// clearing the draft.
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

#[ignore = "spends tokens against a live provider; run via make smoke"]
#[tokio::test]
async fn at1_real_provider_answers() {
    if !e2e_enabled() {
        return;
    }
    let base_url =
        std::env::var("DCH_SMOKE_BASE_URL").unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let model = std::env::var("DCH_SMOKE_MODEL").unwrap_or_else(|_| "qwen2.5-coder".into());
    let mut config = dch_config::DchConfig::default();
    config.api.api_type = dch_config::ApiType::OpenAi;
    config.api.base_url = base_url;
    config.api.model = model;
    if let Ok(key) = std::env::var("DCH_SMOKE_API_KEY") {
        config.api.api_key = Some(key);
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let mut runner = dch_loop::Runner::builder(&config, dir.path())
        .build()
        .await
        .expect("runner builds against the live provider");
    let started = std::time::Instant::now();
    let run = runner
        .run("Reply with exactly the word: ready")
        .await
        .expect("the live provider answers");
    let answer = runner.session_turn_outputs().join("\n");
    assert!(
        answer.to_lowercase().contains("ready"),
        "the live answer arrived: {answer}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "a boot smoke must complete without hanging, took {:?}",
        started.elapsed()
    );
    assert!(!run.turns.is_empty(), "at least one turn ran");
}

#[ignore = "drives a real terminal; run via make smoke"]
#[test]
fn at6_tui_cancel_renders_and_exits_clean() {
    if !e2e_enabled() {
        return;
    }
    let sb = Sandbox::with_server(common::CannedServer::sse_delayed(
        vec![sse_text_turn("slow answer")],
        Duration::from_secs(4),
    ));
    let mut session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    submit(&mut session, "hello");
    await_first_request(&sb, Duration::from_secs(20));
    session.send(b"\x03");
    session.expect("cancel", Duration::from_secs(20));
    let status = session.quit_and_finish(Duration::from_secs(20));
    assert!(
        status.success(),
        "a cancelled TUI run leaves cleanly: {status}"
    );
}

#[ignore = "drives a real terminal; run via make smoke"]
#[test]
fn at10_theme_nord_colors_render() {
    if !e2e_enabled() {
        return;
    }
    let sb = Sandbox::sse(vec![sse_text_turn(
        "# Nord check\n\nfrosty header rendered",
    )]);
    let session = sb.dch_pty(&["--theme", "nord"]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    submit(&mut session, "show a header");
    session.expect("frosty header rendered", Duration::from_secs(20));
    let screen = session.snapshot();
    assert!(
        screen.contains("38;2;136;192;208") || screen.contains("48;2;59;66;82"),
        "nord's own RGB values reach the terminal: {screen}"
    );
    session.quit_and_finish(Duration::from_secs(20));
}

#[ignore = "drives a real terminal; run via make smoke"]
#[test]
fn at13_code_block_highlight_renders() {
    if !e2e_enabled() {
        return;
    }
    let answer = "Here is code:\n```rust\nfn main() {\n    let x = 42;\n}\n```\n";
    let sb = Sandbox::sse(vec![sse_text_turn(answer)]);
    let session = sb.dch_pty(&["--theme", "nord"]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    submit(&mut session, "show me rust code");
    session.expect("fn main", Duration::from_secs(20));
    let screen = session.snapshot();
    assert!(
        !screen.contains("```"),
        "the renderer consumes the fence instead of printing it: {screen}"
    );
    assert!(
        screen.contains("38;2;"),
        "the code block renders with truecolor styling: {screen}"
    );
    session.quit_and_finish(Duration::from_secs(20));
}

#[ignore = "drives a real terminal; run via make smoke"]
#[test]
fn at18_ask_tool_reports_through_the_tui() {
    if !e2e_enabled() {
        return;
    }
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "AskUserQuestion",
            &json!({"questions": [{
                "question": "Which flavor?",
                "options": [{"label": "Vanilla"}, {"label": "Chocolate"}]
            }]}),
        ),
        sse_text_turn("proceeding without an answer"),
    ]);
    let session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    submit(&mut session, "ask me something");
    session.expect("proceeding without an answer", Duration::from_secs(20));
    let requests = sb.requests();
    let follow_up = requests
        .get(1)
        .expect("the tool result came back to the model");
    assert!(
        follow_up.contains("non-interactive"),
        "until the ask overlay lands the tool degrades honestly: {follow_up}"
    );
    session.quit_and_finish(Duration::from_secs(20));
}

#[ignore = "drives a real terminal; run via make smoke"]
#[test]
fn at22_tui_permission_prompt_approves() {
    if !e2e_enabled() {
        return;
    }
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Bash", &json!({"command": "touch approved.txt"})),
        sse_text_turn("command approved and run"),
    ]);
    let session = sb.dch_pty(&["--permission-mode", "accept-edits"]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    submit(&mut session, "touch the file");
    session.expect("allow 'bash'", Duration::from_secs(20));
    session.send(b"y");
    session.expect("command approved and run", Duration::from_secs(20));
    assert!(
        sb.workdir().join("approved.txt").exists(),
        "the approved command actually ran"
    );
    session.quit_and_finish(Duration::from_secs(20));
}
