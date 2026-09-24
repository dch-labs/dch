//! The automated acceptance matrix: one scenario per automatable AT.
//!
//! Every test scripts the model's turns as canned SSE bodies, runs the
//! real `dch` binary against them in an isolated HOME and workdir, and
//! asserts on what a real invocation produces — stdout, the exit code,
//! the done-file record, the session store, the recorded provider
//! requests (what the model was shown), and the tools' filesystem
//! effects. No network beyond the loopback fixture; no engine internals
//! — the binary is driven exactly as a user or CI drives it.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use common::Sandbox;
use common::git_fixture;
use common::sse_text_turn;
use common::sse_tool_call_turn;
use serde_json::json;

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

/// A minimal cargo crate the LSP scenario navigates.
fn fixture_crate(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("src")).expect("src dir");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"gate_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("Cargo.toml written");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn uses_add() -> i32 {\n    add(1, 2)\n}\n",
    )
    .expect("lib.rs written");
}

#[test]
fn at1_boot_hello_answers() {
    let sb = Sandbox::sse(vec![sse_text_turn("hello there")]);
    let out = sb.dch(&["hello"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    assert!(stdout(&out).contains("hello there"), "{}", stdout(&out));
}

#[test]
fn at2_read_tool_serves_the_file() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Read", &json!({"file_path": "note.txt"})),
        sse_text_turn("I read it."),
    ]);
    std::fs::write(sb.workdir().join("note.txt"), "garden wall\n").expect("note written");
    let out = sb.dch(&["read the note"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let requests = sb.requests();
    let follow_up = requests
        .get(1)
        .expect("the tool result came back to the model");
    assert!(follow_up.contains("garden wall"), "{follow_up}");
}

#[test]
fn at3_write_tool_creates_the_file() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "Write",
            &json!({"file_path": "test.txt", "content": "hello\n"}),
        ),
        sse_text_turn("Created test.txt."),
    ]);
    let out = sb.dch(&["create the file"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let written = std::fs::read_to_string(sb.workdir().join("test.txt")).expect("file written");
    assert_eq!(written, "hello\n");
    assert!(
        stdout(&out).contains("Created test.txt."),
        "{}",
        stdout(&out)
    );
}

#[test]
fn at4_edit_tool_replaces_the_line() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "Edit",
            &json!({
                "file_path": "test.txt",
                "old_text": "first version",
                "new_text": "second version"
            }),
        ),
        sse_text_turn("Edited."),
    ]);
    std::fs::write(sb.workdir().join("test.txt"), "first version\n").expect("base written");
    let out = sb.dch(&["edit line 1"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let edited = std::fs::read_to_string(sb.workdir().join("test.txt")).expect("file edited");
    assert_eq!(edited, "second version\n");
}

#[test]
fn at5_bash_tool_runs_in_the_workdir() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Bash", &json!({"command": "ls"})),
        sse_text_turn("Listed."),
    ]);
    std::fs::write(sb.workdir().join("present.txt"), "x").expect("marker written");
    let out = sb.dch(&["list files"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let requests = sb.requests();
    let follow_up = requests.get(1).expect("the listing came back to the model");
    assert!(follow_up.contains("present.txt"), "{follow_up}");
}

#[test]
fn at7_a_completed_turn_auto_saves_the_session() {
    let sb = Sandbox::sse(vec![sse_text_turn("saved")]);
    let out = sb.dch(&["hello"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let sessions = std::fs::read_dir(sb.sessions_dir()).expect("sessions dir exists");
    let saved: Vec<_> = sessions
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("session.json").is_file())
        .collect();
    assert_eq!(saved.len(), 1, "exactly one session with a transcript");
}

#[test]
fn at8_resume_continues_the_history() {
    let sb = Sandbox::sse(vec![
        sse_text_turn("first answer"),
        sse_text_turn("second answer"),
    ]);
    let first = sb.dch(&["first question"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stderr: {}",
        lossy(&first.stderr)
    );

    let id = std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name != "README.md")
        .expect("one saved session id");

    let before = sb.requests().len();
    let second = sb.dch(&["--resume", &id, "second question"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "stderr: {}",
        lossy(&second.stderr)
    );
    let requests = sb.requests();
    let resumed = requests
        .get(before)
        .expect("the resumed run reached the model");
    assert!(
        resumed.contains("first answer"),
        "the prior history rides the resumed request: {resumed}"
    );
    assert!(
        stdout(&second).contains("second answer"),
        "{}",
        stdout(&second)
    );
}

#[test]
fn at9_list_sessions_prints_both_ids() {
    let sb = Sandbox::sse(vec![sse_text_turn("one"), sse_text_turn("two")]);
    sb.dch(&["first"]);
    sb.dch(&["second"]);
    let out = sb.dch(&["--list-sessions"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let ids: Vec<String> = std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "README.md")
        .collect();
    assert_eq!(ids.len(), 2, "two sessions saved");
    let table = stdout(&out);
    for id in &ids {
        assert!(table.contains(id), "row for {id}: {table}");
    }
}

#[test]
fn at11_headless_prints_stdout_and_writes_the_done_file() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Bash", &json!({"command": "echo done"})),
        sse_text_turn("listed"),
    ]);
    let done = sb.workdir().join("done.json");
    let out = sb.dch(&["list files", "--done-file", done.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    assert!(stdout(&out).contains("listed"), "{}", stdout(&out));
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&done).expect("done-file written"))
            .expect("done-file is JSON");
    assert_eq!(record.get("success"), Some(&json!(true)));
    assert!(
        record
            .get("turns")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            >= 2,
        "turns recorded: {record}"
    );
    assert!(
        record
            .get("tools_used")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            >= 1,
        "tool count recorded: {record}"
    );
}

#[test]
fn at12_plan_mode_blocks_the_write() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "Write",
            &json!({"file_path": "test.txt", "content": "hello\n"}),
        ),
        sse_text_turn("understood"),
    ]);
    let out = sb.dch(&["create the file", "--permission-mode", "plan"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    assert!(
        !sb.workdir().join("test.txt").exists(),
        "Plan mode never lets the write land"
    );
    let requests = sb.requests();
    let follow_up = requests.get(1).expect("the block came back to the model");
    assert!(
        follow_up.contains("denied by Plan mode"),
        "the block reason reaches the model: {follow_up}"
    );
}

#[test]
fn at17_bash_then_multiedit_apply_together() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Bash", &json!({"command": "echo base > doc.txt"})),
        sse_tool_call_turn(
            "MultiEdit",
            &json!({"edits": [{
                "file_path": "doc.txt",
                "old_text": "base",
                "new_text": "revised"
            }]}),
        ),
        sse_text_turn("done"),
    ]);
    let out = sb.dch(&["fix it"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let doc = std::fs::read_to_string(sb.workdir().join("doc.txt")).expect("doc written");
    assert_eq!(doc.trim_end(), "revised", "bash created, multiedit revised");
}

#[test]
fn at19_submit_renders_a_patch() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Submit", &json!({})),
        sse_text_turn("patch ready"),
    ]);
    git_fixture(sb.workdir());
    let out = sb.dch(&["submit the change"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let requests = sb.requests();
    let follow_up = requests.get(1).expect("the patch came back to the model");
    assert!(
        follow_up.contains("## Pull Request Summary"),
        "the rendered patch reaches the model: {follow_up}"
    );
}

#[test]
fn at20_webfetch_fetches_the_local_page() {
    let page = common::CannedServer::plain(
        "text/html",
        "<html><body><h1>gate marker page</h1></body></html>",
    );
    let sb = Sandbox::with_server(common::CannedServer::sse(vec![
        sse_tool_call_turn(
            "WebFetch",
            &json!({"url": format!("http://127.0.0.1:{}/page", page.port())}),
        ),
        sse_text_turn("fetched"),
    ]));
    let out = sb.dch(&["fetch the page"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let requests = sb.requests();
    let follow_up = requests.get(1).expect("the page came back to the model");
    assert!(
        follow_up.contains("gate marker page"),
        "the fetched content reaches the model: {follow_up}"
    );
}

#[test]
fn at21_lsp_resolves_a_definition() {
    if !common::rust_analyzer_available() {
        return;
    }
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "LSP",
            &json!({
                "operation": "goToDefinition",
                "file_path": "src/lib.rs",
                "line": 5,
                "character": 5
            }),
        ),
        sse_text_turn("resolved"),
    ]);
    fixture_crate(sb.workdir());
    let out = sb.dch(&["go to the definition"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    let requests = sb.requests();
    let follow_up = requests
        .get(1)
        .expect("the location came back to the model");
    assert!(follow_up.contains("lib.rs"), "{follow_up}");
}

#[test]
fn at22_accept_edits_headless_denies_shell() {
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Bash", &json!({"command": "touch pwned.txt"})),
        sse_text_turn("denied noted"),
    ]);
    let out = sb.dch(&["run it", "--permission-mode", "accept-edits"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", lossy(&out.stderr));
    assert!(
        !sb.workdir().join("pwned.txt").exists(),
        "headless Ask cells deny rather than prompt"
    );
    let requests = sb.requests();
    let follow_up = requests.get(1).expect("the denial came back to the model");
    assert!(
        follow_up.contains("AcceptEdits") && follow_up.contains("Bash"),
        "the unresolved ask reaches the model naming mode and tool: {follow_up}"
    );
}

#[test]
fn the_scenario_binary_resolves_the_cargo_build_by_default() {
    let env = loopctl::testing::EnvGuard::acquire(&["DCH_BIN"]);
    env.remove("DCH_BIN");
    assert_eq!(
        common::dch_binary(),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_dch")),
        "without an override every scenario drives the cargo-built binary"
    );
}

#[test]
fn the_scenario_binary_honors_the_release_override() {
    let env = loopctl::testing::EnvGuard::acquire(&["DCH_BIN"]);
    // The override points at the same debug binary so concurrently
    // running scenarios stay unaffected; only the resolution is pinned.
    env.set("DCH_BIN", env!("CARGO_BIN_EXE_dch"));
    assert_eq!(
        common::dch_binary(),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_dch")),
        "a set DCH_BIN redirects every spawner to that binary"
    );
}

#[test]
fn a_lingering_pty_child_is_killed_at_its_deadline() {
    let sb = Sandbox::sse(vec![sse_text_turn("late answer")]);
    let mut session = sb.dch_pty(&[]);
    session.expect("test-model", std::time::Duration::from_secs(20));
    let reaped = session.reap_within(std::time::Duration::from_millis(300));
    assert!(
        reaped.is_none(),
        "an idling TUI outlives a fraction-of-a-second deadline"
    );
    let status = session
        .reap_within(std::time::Duration::from_secs(5))
        .expect("the killed child reaps promptly");
    assert!(
        !status.success(),
        "a killed child exits with a non-success status"
    );
}
