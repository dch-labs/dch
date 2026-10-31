//! The todo nudge through the headless entrypoint, end to end.
//!
//! The reminder turn is engine-injected and builder-registered, so
//! every host that arms the nudge gets the same follow-up — these
//! drive the real binary against a canned provider, proving the
//! headless host rides the mechanism the TUI does: the run that
//! files and abandons a plan is extended by exactly one reminder
//! request, a run that closes its plan ends where the model left
//! it, and the mid-run belts reach the model as steered user
//! guidance — committed once, replayed on every later request of
//! the run — while the save filters keep their wording out of both
//! saved files.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::indexing_slicing
)]

mod common;

use common::Sandbox;
use common::sse_text_turn;
use common::sse_tool_call_turn;
use serde_json::json;

#[test]
fn a_headless_run_with_an_undone_plan_gets_one_reminder_turn() {
    // The parity pin: the prompt's turn files a one-item plan and
    // speaks, the run lands clean with the item pending, and the
    // engine's hook extends the same run by one turn whose prompt is
    // the reminder. The third request the canned provider sees is
    // that reminder — and there is no fourth, because the follow-up's
    // own clean end is never offered another.
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]}),
        ),
        sse_text_turn("working on it"),
        sse_text_turn("noted, but not closing it"),
    ]);
    let out = sb.dch(&["do the thing"]);
    assert!(
        out.status.success(),
        "the run completes, reminder included: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = sb.requests();
    assert_eq!(
        requests.len(),
        3,
        "the prompt's two turns and exactly one reminder turn: {requests:?}"
    );
    assert!(
        requests[2].contains("reminder: 1 todo item"),
        "the injected turn's prompt is the reminder wording: {requests:?}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("1 todo item left unfinished"),
        "the exit warning states the work the run left undone: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The wire carries the prompt — the engine's follow-up works that
    // way — but no saved file persists it as the user's words: the
    // machine history drops it at save, keeping the turn's answer.
    let session_dir = std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("session.json").is_file())
        .expect("the one saved session");
    let machine = std::fs::read_to_string(session_dir.join("machine.json"))
        .expect("the machine history file");
    // The law is the wording, not one run's spelling of it: no reminder
    // the belts contribute — at any count, mid-run or at the end — is
    // allowed to persist as a user message. The specific phrase alone
    // would pass a save that kept a different reminder turn.
    assert!(
        !machine.contains("reminder:"),
        "no contributed reminder persists in the machine history: {machine}"
    );
    assert!(
        !machine.contains("still not completed"),
        "nor does any spelling of the end-of-run offer: {machine}"
    );
    assert!(
        machine.contains("noted, but not closing it"),
        "the follow-up's answer keeps its place in the history it \
         answered: {machine}"
    );
}

#[test]
fn a_session_saved_after_its_followup_resumes_alternating() {
    // The resume-round-trip half of the hide: the follow-up turn
    // commits as [user: reminder, assistant: answer], the save drops
    // the reminder, and a raw drop would leave the answer standing
    // directly after the run's own last assistant message — the
    // consecutive-assistant shape strict role-alternation providers
    // reject on a resumed session's first request. The saved file
    // must alternate, and a resume of it must reach the model with
    // both of the run's halves aboard and no reminder between them.
    // The resumed run earns its own follow-up — the seeded plan still
    // stands — so the script carries its answer too.
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]}),
        ),
        sse_text_turn("working on it"),
        sse_text_turn("noted, but not closing it"),
        sse_text_turn("still here"),
        sse_text_turn("closing now"),
    ]);
    let first = sb.dch(&["do the thing"]);
    assert!(
        first.status.success(),
        "the run completes, reminder included: stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let session_dir = std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("session.json").is_file())
        .expect("the one saved session");
    let machine: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(session_dir.join("machine.json")).expect("the machine file"),
    )
    .expect("the machine file parses");
    let roles: Vec<&str> = machine["messages"]
        .as_array()
        .expect("the messages array")
        .iter()
        .map(|message| message["role"].as_str().expect("a role string"))
        .collect();
    assert!(
        roles.windows(2).all(|pair| pair[0] != pair[1]),
        "the saved machine history alternates — the shape a resume \
         replays and a strict provider accepts: {roles:?}"
    );
    assert_eq!(
        roles.first(),
        Some(&"user"),
        "the history opens on the session's own prompt: {roles:?}"
    );

    let id = session_dir
        .file_name()
        .expect("the session directory names its id")
        .to_string_lossy()
        .into_owned();
    let before = sb.requests().len();
    let second = sb.dch(&["--resume", &id, "next question"]);
    assert!(
        second.status.success(),
        "the resumed run completes: stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let requests = sb.requests();
    let resumed = requests
        .get(before)
        .expect("the resumed run reached the model");
    assert!(
        resumed.contains("working on it") && resumed.contains("noted, but not closing it"),
        "both halves of the run ride the resumed request — the \
         orphaned answer merged, not dropped: {resumed}"
    );
    assert!(
        !resumed.contains("reminder: 1 todo item"),
        "the dropped reminder replays as no one's words: {resumed}"
    );
}

#[test]
fn a_headless_run_that_closes_its_plan_gets_no_reminder() {
    // The nudge exists for the filing-and-forgetting shape, not as a
    // tax on every run: a plan filed complete earns no follow-up, and
    // the provider sees only the turns the model itself took.
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "only", "subject": "the work",
                               "status": "completed"}]}),
        ),
        sse_text_turn("all done"),
    ]);
    let out = sb.dch(&["do the thing"]);
    assert!(
        out.status.success(),
        "the run completes on its own: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = sb.requests();
    assert_eq!(
        requests.len(),
        2,
        "a closed plan nudges nothing: {requests:?}"
    );
}

#[test]
fn a_headless_mid_run_reminder_rides_as_steered_user_guidance() {
    // The start-discipline belt through the real binary: a run that
    // files a pending plan, then fires a work tool without claiming an
    // entry, gets the reminder on the next request — steered into the
    // machine's pending set as a user message, committed to history
    // once and replayed on every later request of the run.
    // Persistence is the save filters' to own: the wording never
    // reaches either saved file, so nothing a user reads or a resume
    // replays carries it as anyone's words.
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]}),
        ),
        sse_tool_call_turn("Grep", &json!({"pattern": "needle"})),
        sse_text_turn("working on it"),
        sse_text_turn("noted, but not closing it"),
    ]);
    let out = sb.dch(&["do the thing"]);
    assert!(
        out.status.success(),
        "the run completes, reminder included: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = sb.requests();
    assert_eq!(
        requests.len(),
        4,
        "three model turns plus the end-of-run follow-up: {requests:?}"
    );
    let midrun = |request: &str| request.matches("no plan entry in progress").count();
    assert_eq!(
        midrun(&requests[0]) + midrun(&requests[1]),
        0,
        "the filing turn and its successor stay quiet — the work tool has \
         not fired yet: {requests:?}"
    );
    assert_eq!(
        midrun(&requests[2]),
        1,
        "the Grep fired against the unclaimed plan — the next request \
         carries the start reminder: {requests:?}"
    );
    assert_eq!(
        midrun(&requests[3]),
        1,
        "the steered reminder is committed once — the follow-up replays \
         it exactly once: {requests:?}"
    );
    assert_eq!(
        message_roles(&requests[2], "no plan entry in progress"),
        vec!["user".to_string()],
        "the reminder travels as steered user guidance at the tail — \
         never a system insertion: {requests:?}"
    );
    let session_dir = std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("session.json").is_file())
        .expect("the one saved session");
    let display =
        std::fs::read_to_string(session_dir.join("session.json")).expect("the transcript file");
    assert!(
        !display.contains("no plan entry in progress"),
        "the display transcript a user reads holds no trace of the \
         reminder: {display}"
    );
    let machine = std::fs::read_to_string(session_dir.join("machine.json"))
        .expect("the machine history file");
    assert!(
        !machine.contains("no plan entry in progress"),
        "the machine history a resume replays drops the steered wording \
         at save"
    );
}

#[test]
fn a_headless_planless_run_gets_one_creation_note_that_never_persists() {
    // The third belt through the real binary: a run that goes four
    // turns without ever filing a plan is told once — the note rides
    // the fourth request as steered user guidance, committed once and
    // replayed by the final request, and the save filters keep its
    // wording out of both saved files.
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn("Grep", &json!({"pattern": "one"})),
        sse_tool_call_turn("Grep", &json!({"pattern": "two"})),
        sse_tool_call_turn("Grep", &json!({"pattern": "three"})),
        sse_tool_call_turn("Grep", &json!({"pattern": "four"})),
        sse_text_turn("done"),
    ]);
    let out = sb.dch(&["do the thing"]);
    assert!(
        out.status.success(),
        "the run completes on its own: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = sb.requests();
    assert_eq!(
        requests.len(),
        5,
        "four work turns and the final answer: {requests:?}"
    );
    let note = |request: &str| request.matches("file one with TodoWrite now").count();
    assert_eq!(
        note(&requests[3]),
        1,
        "the fourth planless turn is told, once: {requests:?}"
    );
    assert_eq!(
        note(&requests[0]) + note(&requests[1]) + note(&requests[2]),
        0,
        "quiet before the fourth turn: {requests:?}"
    );
    assert_eq!(
        note(&requests[4]),
        1,
        "the steered note is committed once — the final request replays \
         it exactly once: {requests:?}"
    );
    assert_eq!(
        message_roles(&requests[3], "file one with TodoWrite now"),
        vec!["user".to_string()],
        "the note rides the tail as steered user guidance — never a \
         system prompt: {requests:?}"
    );
    let session_dir = std::fs::read_dir(sb.sessions_dir())
        .expect("sessions dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("session.json").is_file())
        .expect("the one saved session");
    for file in ["session.json", "machine.json"] {
        // The read cannot fall back to an empty string: a file the
        // save never wrote would satisfy "no trace of the note"
        // without being read at all, and the pin would pass on the
        // absence it is meant to catch.
        let body = std::fs::read_to_string(session_dir.join(file))
            .expect("the save writes both files before the run ends");
        assert!(
            !body.contains("file one with TodoWrite now"),
            "the {file} file holds no trace of the note"
        );
    }
    // The same entry's save carries the loop's own context figure —
    // the number the listing's CONTEXT column and a resumed status bar
    // read — where a hand-written headless envelope used to record a
    // flat zero.
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(session_dir.join("session.json")).expect("session.json"),
    )
    .expect("session.json is JSON");
    let context = saved
        .get("tokens")
        .and_then(|tokens| tokens.get("context_tokens"))
        .and_then(serde_json::Value::as_u64);
    assert!(
        context.is_some_and(|tokens| tokens > 0),
        "the headless save carries the run's context figure: {saved:?}"
    );
}

#[test]
fn the_machinery_speaks_to_the_log_when_rust_log_names_it() {
    // The belts answer the "is it nudging?" question from the log
    // instead of a wire proxy: with RUST_LOG naming the nudge target,
    // the real binary's stderr carries every contribution and offer,
    // each named by its own event and counted — and no plan text in
    // sight. The wording itself stays where the transcript pins read
    // it: on the wire.
    let sb = Sandbox::sse(vec![
        sse_tool_call_turn(
            "TodoWrite",
            &json!({"todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]}),
        ),
        // Work without a runner: the successful call after the plan is
        // what arms the start-discipline belt.
        sse_tool_call_turn("Bash", &json!({"command": "echo working"})),
        sse_text_turn("working on it"),
        sse_text_turn("noted, but not closing it"),
    ]);
    let out = sb.dch_env(
        &["do the thing"],
        &[("RUST_LOG", "dch_loop::todo_nudge=debug")],
    );
    assert!(
        out.status.success(),
        "the run completes: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("start-discipline reminder contributed"),
        "the mid-run belt's own event stands in the log: {stderr}"
    );
    assert!(
        stderr.contains("end-of-run follow-up offered"),
        "the end-of-run offer reports under its own event, not the \
         mid-run one's: {stderr}"
    );
    assert!(
        stderr.contains("undone=1"),
        "the offer line carries the count in place of the plan's words: {stderr}"
    );
    assert!(
        stderr.contains("1 todo item left unfinished"),
        "the same run's exit tells the terminal the count the log \
         holds: {stderr}"
    );
    assert!(
        !stderr.contains("the work"),
        "the log names events, never the subjects the plan carries: {stderr}"
    );
    let wire = sb.requests().join("\n");
    assert!(
        wire.contains("work is underway with no plan entry in progress"),
        "the mid-run reminder still reaches the model in its wording: {wire}"
    );
    assert!(
        wire.contains("1 todo item still not completed"),
        "the end-of-run offer still reaches the model in its wording: {wire}"
    );
}

/// The roles of every message in a recorded request whose text
/// contains `needle`.
///
/// Reads the request's JSON body, so a pin can assert not only that a
/// wording reached the provider but in whose name it traveled.
fn message_roles(request: &str, needle: &str) -> Vec<String> {
    // The sandbox records bare bodies; the runner-level twin records
    // whole HTTP requests. Accept both.
    let body = if request.starts_with('{') {
        request
    } else {
        request.split("\r\n\r\n").nth(1).unwrap_or_default()
    };
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    parsed
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|message| {
                    message
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|content| content.contains(needle))
                })
                .map(|message| {
                    message
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}
