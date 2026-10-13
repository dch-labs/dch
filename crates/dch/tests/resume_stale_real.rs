//! The staleness guard against the real incident directory.
//!
//! Session `41cee350` froze mid-run — its engine half stopped at a
//! handful of messages while the transcript grew past a hundred rows —
//! and a resume adopting that half silently amnesed the session. This
//! drives the actual session directory through the real binary's
//! resume path and asserts the guard's two observable effects: the
//! resumed run answers (the derive feeds the model), and the engine
//! half is rewritten from the derived history rather than the frozen
//! one, so the next resume starts from the full conversation.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use std::io::Write as _;

const INCIDENT: &str = "41cee350-6a61-4e6f-b1df-62caa60280eb";

#[test]
fn the_frozen_incident_session_resumes_on_the_derive() {
    // The fixture ships a synthesized copy of the incident pair: a
    // long compaction-free transcript beside a fresh-stamped engine
    // half holding a sliver of it — the freeze shape.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/frozen-41cee350")
        .join(INCIDENT);

    let sb = common::Sandbox::with_server(common::CannedServer::sse(vec![common::sse_text_turn(
        "resumed fine",
    )]));
    let sessions = sb.sessions_dir().join(INCIDENT);
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    for name in ["session.json", "machine.json"] {
        let bytes = std::fs::read(fixture.join(name)).expect("fixture file");
        let mut file = std::fs::File::create(sessions.join(name)).unwrap();
        file.write_all(&bytes).unwrap();
    }

    let out = sb.dch(&["--resume", INCIDENT, "say resumed fine"]);
    assert!(
        out.status.success(),
        "the resumed run completes: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("resumed fine"),
        "the run answered from the derived history"
    );

    let rewritten =
        std::fs::read_to_string(sessions.join("machine.json")).expect("machine half after run");
    let value: serde_json::Value = serde_json::from_str(&rewritten).expect("parses");
    let machine_rows = value
        .get("messages")
        .and_then(|messages| messages.as_array())
        .map_or(0, Vec::len);
    assert!(
        machine_rows > 40,
        "the engine half now carries the derived conversation ({machine_rows} messages) — \
         the ten-message frozen sliver was not the seed"
    );
}
