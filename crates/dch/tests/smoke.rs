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

/// Whether a TCP connection can reach any address the host resolves
/// to.
///
/// A name may answer on several addresses — `localhost` alone can
/// resolve to the `IPv6` loopback ahead of the `IPv4` one, and a provider
/// listening only on `127.0.0.1` is reachable through the second
/// however fast the first refuses. Every resolved address gets its
/// own connection attempt before the host reads as unreachable, and
/// a name that resolves to nothing is unreachable outright.
fn any_address_reachable(host: &str, port: u16) -> bool {
    let addresses: Vec<std::net::SocketAddr> = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
        Err(_) => match std::net::ToSocketAddrs::to_socket_addrs(&(host, port)) {
            Ok(resolved) => resolved.collect(),
            Err(_) => return false,
        },
    };
    addresses.iter().any(|address| {
        std::net::TcpStream::connect_timeout(address, Duration::from_secs(1)).is_ok()
    })
}

/// Whether the live-provider leg has nothing to talk to.
///
/// The smoke suite's one real-provider test needs either a reachable
/// endpoint or a configured key; on a box with neither — no local
/// Ollama listening, no environment credentials — the leg cannot smoke
/// anything, and failing the suite would report an absence, not a
/// defect. Reachability is a one-second TCP probe of every address the
/// base URL's host resolves to; a key in either env var keeps the leg
/// on regardless, so a configured key against an unreachable endpoint
/// still fails honestly.
fn provider_absent(base_url: &str) -> bool {
    let keyed =
        std::env::var("DCH_SMOKE_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok();
    if keyed {
        return false;
    }
    let host_port = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .rsplit_once('/')
        .map_or(
            base_url
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .trim_end_matches('/'),
            |(head, _)| head,
        );
    let Some((host, port)) = host_port.rsplit_once(':') else {
        return true;
    };
    let Ok(port) = port.parse::<u16>() else {
        return true;
    };
    !any_address_reachable(host, port)
}

#[test]
fn a_provider_listening_on_one_address_is_reachable_through_its_name() {
    // `localhost` can resolve to the IPv6 loopback ahead of the IPv4
    // one, and a provider bound to `127.0.0.1` alone is behind the
    // second address. A probe that stops at the first address the
    // resolver returns reports such a provider absent and silently
    // skips the live leg; every address owes an attempt before the
    // host reads as unreachable.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
    let port = listener
        .local_addr()
        .expect("a bound socket has an address")
        .port();
    assert!(
        any_address_reachable("localhost", port),
        "the name reaches the listener through at least one of its addresses"
    );
}

#[test]
fn a_silent_port_on_every_address_reads_as_absent() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
    let port = listener
        .local_addr()
        .expect("a bound socket has an address")
        .port();
    drop(listener);
    assert!(
        !any_address_reachable("127.0.0.1", port),
        "nothing answers on the address, so the host reads as unreachable"
    );
}

/// The tool-result answer an `AskUserQuestion` call carried back.
///
/// The follow-up request repeats the scripted call in its history —
/// every option label among the arguments — so the request body as a
/// whole names options the run never chose. The answer worth asserting
/// on is the tool-result message paired with the `AskUserQuestion`
/// call by its call id: its content is what the ask actually returned
/// to the model.
fn ask_tool_result(request_body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(request_body).ok()?;
    let messages = parsed.get("messages")?.as_array()?;
    let ask_id = messages.iter().find_map(|message| {
        message
            .get("tool_calls")
            .and_then(|calls| calls.as_array())
            .and_then(|calls| {
                calls.iter().find(|call| {
                    call.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(|name| name.as_str())
                        == Some("AskUserQuestion")
                })
            })
            .and_then(|call| call.get("id"))
            .and_then(|id| id.as_str())
    })?;
    messages.iter().find_map(|message| {
        let answers_the_ask = message.get("role").and_then(|role| role.as_str()) == Some("tool")
            && message.get("tool_call_id").and_then(|id| id.as_str()) == Some(ask_id);
        answers_the_ask.then(|| {
            message
                .get("content")
                .and_then(|content| content.as_str())
                .unwrap_or_default()
                .to_string()
        })
    })
}

#[test]
fn the_ask_assertion_reads_the_tool_result_not_the_scripted_arguments() {
    // The discrimination the live leg depends on: the request body
    // carries `Vanilla` in the scripted call's arguments whether or
    // not Enter confirmed it, so a whole-body search passes on a run
    // that answered differently. The paired tool result is the only
    // place the confirmed option lives.
    let follow_up = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "ask me something"},
            {"role": "assistant", "tool_calls": [{
                "id": "call-1",
                "function": {
                    "name": "AskUserQuestion",
                    "arguments": "{\"questions\":[{\"options\":[{\"label\":\"Vanilla\"}]}]}"
                }
            }]},
            {"role": "tool", "tool_call_id": "call-1", "content": "declined the question"}
        ]
    })
    .to_string();
    let declined = ask_tool_result(&follow_up).expect("the paired result is found");
    assert_eq!(
        declined, "declined the question",
        "the paired tool result is the answer, not the arguments beside it"
    );
    assert!(
        !declined.contains("Vanilla"),
        "an unconfirmed option must not read as the answer: {declined}"
    );
    let answered = follow_up.replace("declined the question", "chose Vanilla");
    let confirmed = ask_tool_result(&answered).expect("the paired result is found");
    assert!(
        confirmed.contains("Vanilla"),
        "the confirmed option rides the tool result: {confirmed}"
    );
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
    if provider_absent(&base_url) {
        eprintln!(
            "skipping: no live provider at {base_url} and no key set \
            (DCH_SMOKE_API_KEY / OPENAI_API_KEY)"
        );
        return;
    }
    let model = std::env::var("DCH_SMOKE_MODEL").ok().unwrap_or_else(|| {
        common::discover_model(&base_url)
            .unwrap_or_else(|| panic!("no model discoverable at {base_url}; set DCH_SMOKE_MODEL"))
    });
    let mut config = dch_config::DchConfig::default();
    let keyed = std::env::var("DCH_SMOKE_API_KEY").or_else(|_| std::env::var("OPENAI_API_KEY"));
    config.api.api_type = match &keyed {
        Ok(key) => {
            config.api.api_key = Some(key.clone());
            dch_config::ApiType::OpenAi
        }
        Err(_) => dch_config::ApiType::Ollama,
    };
    config.api.base_url = base_url;
    config.api.model = model;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut runner = dch_loop::Runner::builder(&config, dir.path())
        .build()
        .await
        .expect("runner builds against the live provider");
    let run = tokio::time::timeout(
        Duration::from_secs(300),
        runner.run("Reply with exactly the word: ready"),
    )
    .await
    .expect("the live provider answers inside the 300-second boot deadline")
    .expect("the run completes against the live provider");
    let answer = runner.session_turn_outputs().join("\n");
    assert!(
        answer.to_lowercase().contains("ready"),
        "the live answer arrived: {answer}"
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
    session.cancel_run();
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
        sse_text_turn("the flavor was noted"),
    ]);
    let session = sb.dch_pty(&[]);
    session.expect("test-model", Duration::from_secs(20));
    let mut session = session;
    submit(&mut session, "ask me something");
    // The supervised ask waits for the user: the sheet rises, Enter
    // confirms the row it stands on, and the choice rides the reply
    // channel back into the run.
    session.expect("Which flavor?", Duration::from_secs(20));
    session.send(b"\r");
    session.expect("the flavor was noted", Duration::from_secs(20));
    let requests = sb.requests();
    let follow_up = requests
        .get(1)
        .expect("the tool result came back to the model");
    let answer =
        ask_tool_result(follow_up).expect("the ask's own tool result rides the follow-up request");
    assert!(
        answer.contains("Vanilla"),
        "the confirmed option rode the reply channel through the TUI: {answer}"
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
