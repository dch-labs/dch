//! The overwrite guard's exemption of the surgical tools, end to end.
//!
//! [`with_redaction`](dch_loop::redaction_guard::with_redaction) name-gates
//! `Write` alone: an `Edit` or `MultiEdit` over a scrubbed target never
//! meets the guard, so a stale anchor is answered by the tool's own
//! anchor-miss refusal. That is the sound shape — a model that only ever
//! saw the scrubbed view cannot name the redacted bytes, so an edit that
//! tries to touch them matches nothing and moves nothing. Driven through
//! the production registry, so the refusal is the one a run composes, and
//! the same sequence is driven with redaction off, where the model sees
//! the true bytes and the edit lands.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default
)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dch_loop::redaction_guard::with_redaction;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolDispatchResult;
use loopctl::middleware::ToolPipeline;
use loopctl::tool::PermissionCheck;
use loopctl::tool::ToolContext;
use loopctl::tool::builtin::fs::FileSession;
use serde_json::Value;
use serde_json::json;

/// The words only a `Write` ever hears: the guard's refusal.
const GUARD_REFUSAL: &str = "contains values the secrets redactor hides from you";

/// The production registry under the redaction pair.
fn redacting_pipeline(session: &FileSession) -> ToolPipeline {
    let registry = dch_tools::builtin_registry(session, dch_tools::ReadWindows::default());
    with_redaction(ToolPipeline::builder())
        .with_core(Arc::new(registry))
        .build()
        .expect("static composition builds")
}

/// The production registry with no middleware — redaction off.
fn plain_pipeline(session: &FileSession) -> ToolPipeline {
    let registry = dch_tools::builtin_registry(session, dch_tools::ReadWindows::default());
    ToolPipeline::builder()
        .with_core(Arc::new(registry))
        .build()
        .expect("static composition builds")
}

/// One call through `pipeline`, wired the way the runner's injector wires
/// it: the cwd and the session the tools resolve under.
async fn dispatch(
    pipeline: &ToolPipeline,
    session: &FileSession,
    cwd: &Path,
    name: &str,
    input: Value,
) -> ToolDispatchResult {
    let mut tool_context = ToolContext::default();
    tool_context.cwd = cwd.to_string_lossy().into_owned();
    tool_context.set_extension(session.clone());
    let mut ctx = ToolDispatchContext {
        tool_name: name.to_string(),
        input,
        call_id: "call_edit_exemption".to_string(),
        turn_number: 0,
        cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
        permission: PermissionCheck::Allow,
        tool_context,
    };
    pipeline.dispatch(&mut ctx).await
}

/// Seeds a target the curated patterns scrub; assembled at runtime so no
/// secret-shaped literal appears in source or test output.
fn seed_scrubbed(dir: &Path, name: &str) -> (PathBuf, String) {
    let key: String = ["api_", "key"].concat();
    let value: String = "abcdefghijkl".chars().chain("mnop".chars()).collect();
    let secret_line = format!("{key}={value}");
    let path = dir.join(name);
    std::fs::write(&path, format!("{secret_line}\nport = 8080\n")).expect("seed target");
    (path, secret_line)
}

/// The secret line exactly as a read showed it to the model, from the
/// first `needle` it carries to the end of that line — the scrubbed
/// marker under redaction, the real key with it off.
fn secret_line_as_shown<'a>(view: &'a str, needle: &str) -> &'a str {
    let start = view
        .find(needle)
        .unwrap_or_else(|| panic!("the secret line is in the view: {view:?}"));
    let rest = &view[start..];
    rest.split('\n').next().unwrap_or(rest)
}

#[tokio::test]
async fn an_edit_over_a_scrubbed_target_refuses_by_anchor_not_by_the_guard() {
    // Read under redaction, then edit the secret line as the model saw
    // it — the scrubbed rendering. The guard is live in this pipeline
    // (a whole-file write over the same target is its refusal), but the
    // surgical tools are not on its path: the anchor names bytes the
    // file does not carry, so the tool's own anchor-miss refusal is what
    // the model reads, and the true bytes survive untouched.
    let dir = tempfile::tempdir().expect("tempdir");
    let (target, true_line) = seed_scrubbed(dir.path(), "env.conf");
    let session = FileSession::new(dir.path().to_path_buf());
    let pipeline = redacting_pipeline(&session);

    let read = dispatch(
        &pipeline,
        &session,
        dir.path(),
        "Read",
        json!({"path": "env.conf"}),
    )
    .await;
    let view = read.output.to_string();
    assert!(!read.is_error, "the read answers: {view}");
    let shown = secret_line_as_shown(&view, "[REDACTED:").to_string();
    assert!(
        shown.contains("[REDACTED:") && !shown.contains(&true_line["api_key=".len()..]),
        "the model saw the scrubbed rendering, not the secret: {shown}"
    );

    let write = dispatch(
        &pipeline,
        &session,
        dir.path(),
        "Write",
        json!({"file_path": "env.conf", "content": format!("{shown}\nport = 8080\n")}),
    )
    .await;
    let write_text = write.output.to_string();
    assert!(
        write.is_error && write_text.contains(GUARD_REFUSAL),
        "the guard stands over this target: {write_text}"
    );

    for tool in ["Edit", "MultiEdit"] {
        let input = if tool == "Edit" {
            json!({"file_path": "env.conf", "old_text": shown, "new_text": "api_key=replaced"})
        } else {
            json!({"edits": [{"file_path": "env.conf", "old_text": shown, "new_text": "api_key=replaced"}]})
        };
        let edit = dispatch(&pipeline, &session, dir.path(), tool, input).await;
        let text = edit.output.to_string();
        assert!(
            edit.is_error,
            "{tool} must refuse an anchor the file does not carry: {text}"
        );
        assert!(
            text.contains("Old text not found in file"),
            "{tool}'s anchor-miss refusal is the one the model reads: {text}"
        );
        assert!(
            !text.contains(GUARD_REFUSAL),
            "the guard is not on the {tool} path: {text}"
        );
        assert!(
            !text.contains("validation failed"),
            "nor is the linter — the anchor check refuses first: {text}"
        );
    }

    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        format!("{true_line}\nport = 8080\n"),
        "the refused edits moved no bytes"
    );
}

#[tokio::test]
async fn the_same_edit_lands_with_redaction_off() {
    // With redaction off the model reads the true bytes, so the line it
    // edits is the line the file carries, and the edit lands — the
    // refusal above is the scrubbed view's doing, not the edit's.
    let dir = tempfile::tempdir().expect("tempdir");
    let (target, true_line) = seed_scrubbed(dir.path(), "env.conf");
    let session = FileSession::new(dir.path().to_path_buf());
    let pipeline = plain_pipeline(&session);

    let read = dispatch(
        &pipeline,
        &session,
        dir.path(),
        "Read",
        json!({"path": "env.conf"}),
    )
    .await;
    let view = read.output.to_string();
    assert!(!read.is_error, "the read answers: {view}");
    assert!(!view.contains("[REDACTED:"), "nothing is scrubbed: {view}");
    assert_eq!(
        secret_line_as_shown(&view, "api_key="),
        true_line,
        "the model saw the true bytes"
    );

    let edit = dispatch(
        &pipeline,
        &session,
        dir.path(),
        "Edit",
        json!({"file_path": "env.conf", "old_text": true_line, "new_text": "api_key=replaced"}),
    )
    .await;
    assert!(!edit.is_error, "the edit lands: {}", edit.output);
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "api_key=replaced\nport = 8080\n",
        "the surgical edit replaced the line it named"
    );
}
