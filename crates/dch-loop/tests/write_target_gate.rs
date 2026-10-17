//! A write-family call at a destination that is not a regular file is
//! refused before the tool reaches it.
//!
//! The write family replaces its target, and a destination that holds no
//! file content is not something replacement means anything by. A whole-file
//! write to a pipe path renames an ordinary file over it, silently unlinking
//! the endpoint another process is waiting on; an edit reads its target
//! first, and a device reads without end. The gate under test settles the
//! target's kind from metadata and refuses the call before either happens, so
//! a session cannot destroy a destination it was told to write. Each pin is a
//! refusal-behaviour pin over the real tools: removing the middleware changes
//! what the call does, not merely how fast it answers. The deadline in
//! `dispatched` is what keeps a regression that reintroduces the open from
//! hanging the suite instead of failing it.

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
use std::sync::mpsc;
use std::time::Duration;

use dch_loop::redaction_guard::with_redaction;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolPipeline;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolRegistry;
use loopctl::tool::builtin::fs::EditTool;
use loopctl::tool::builtin::fs::FileSession;
use loopctl::tool::builtin::fs::MultiEditTool;
use loopctl::tool::builtin::fs::WriteTool;
use serde_json::json;

/// How long a dispatch may take before the test calls it a stall.
///
/// Generous against a loaded machine and far short of what an open on a pipe
/// does, which is wait for a writer that never arrives. Anything the gate
/// answers lands inside it in microseconds; the budget is here so a call that
/// stops answering fails the suite rather than hanging it.
const DISPATCH_BUDGET: Duration = Duration::from_secs(10);

/// The refusal phrase the gate shares for every unreplaceable destination.
///
/// Asserted alongside each specific kind, so a pin proves the call met the
/// gate rather than some unrelated tool error that happens to name a pipe.
const REFUSAL_MARK: &str = "not a regular file";

/// The phrase naming where a genuine pipe or device destination belongs.
///
/// Asserted beside the kind because a tool that declines to read a target for
/// its own reasons says why in its own words, and pointing the model at the
/// shell is this gate's own contribution — the pair is what identifies the
/// refusal as the gate's rather than a coincidence of phrasing.
const REDIRECTION_MARK: &str = "through Bash instead";

/// A pipeline of the write-protection layers over the real write family.
///
/// The genuine tools sit in the core on purpose: the claim is that a call
/// meets the gate before the tool touches its destination, and only the real
/// tool shows what the gate kept from happening.
fn write_family_pipeline() -> ToolPipeline {
    let registry = ToolRegistry::new();
    registry.register(WriteTool::new());
    registry.register(EditTool::new());
    registry.register(MultiEditTool::new());
    with_redaction(ToolPipeline::builder())
        .with_core(Arc::new(registry))
        .build()
        .expect("static composition builds")
}

/// Dispatch `tool_name` at `input`, reporting whether it was refused and how.
///
/// The dispatch runs on its own thread under its own runtime, and the answer
/// comes back over a channel the caller waits on only until the budget
/// spends. The thread is the load-bearing part of this design: an open on a
/// FIFO is not cancellable, and dropping the future that owns it blocks until
/// the open finishes, so a deadline kept inside the test's own task would hang
/// alongside the very call it was meant to escape. A worker the test stops
/// waiting for can be left behind, so a call that never returns fails the
/// suite instead of stalling it.
fn dispatched(tool_name: &str, cwd: &Path, input: serde_json::Value) -> (bool, String) {
    let spawned = tool_name.to_string();
    let cwd = cwd.to_path_buf();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for one dispatch");
        let outcome = runtime.block_on(async {
            let pipeline = write_family_pipeline();
            let mut ctx = dispatch_context(&spawned, &cwd, input);
            let result = pipeline.dispatch(&mut ctx).await;
            (result.is_error, result.output.to_string())
        });
        sender.send(outcome).ok();
    });

    receiver.recv_timeout(DISPATCH_BUDGET).unwrap_or_else(|_| {
        panic!(
            "`{tool_name}` stalled: nothing answered within {DISPATCH_BUDGET:?}, which is \
             exactly what an open on this destination does"
        )
    })
}

/// A dispatch context for `tool_name`, resolving paths under `cwd`.
///
/// Carries the filesystem session the file family dispatches through, so
/// the call reaches the tool the way a real session's would — the gate's
/// verdict is what is under test, but the route to it is the production one.
fn dispatch_context(tool_name: &str, cwd: &Path, input: serde_json::Value) -> ToolDispatchContext {
    let mut tool_context = ToolContext::default();
    tool_context.cwd = cwd.to_string_lossy().into_owned();
    tool_context.set_extension(FileSession::new(cwd.to_path_buf()));
    ToolDispatchContext {
        tool_name: tool_name.to_string(),
        input,
        call_id: "call_target_gate".to_string(),
        turn_number: 0,
        cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
        permission: loopctl::tool::PermissionCheck::Allow,
        tool_context,
    }
}

/// Create a FIFO at `name` under `dir`, the destination a write would replace.
///
/// Built with `mkfifo` rather than approximated by a directory or an absent
/// path, because what matters is that the destination is an endpoint other
/// code holds: rewriting an ordinary file by mistake loses content, while a
/// pipe unlinked by a rename takes its reader down with it.
fn fifo(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "the fifo fixture exists");
    path
}

/// Whether the entry at `path` is still a FIFO rather than something else.
///
/// Metadata rather than an open, matching the gate's own reasoning: opening
/// the entry is the act under test, so a check must not perform it. A rename
/// over the path leaves an entry of another kind, which this reports.
#[cfg(unix)]
fn still_a_fifo(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;

    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_fifo())
}

#[test]
fn a_write_at_a_fifo_is_refused_before_the_tool_replaces_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = fifo(dir.path(), "pipe.txt");

    let (is_error, text) = dispatched(
        "Write",
        dir.path(),
        json!({"file_path": "pipe.txt", "content": "hello\n"}),
    );

    assert!(
        is_error,
        "a write at a pipe must not proceed, since the destination holds no content to replace: {text}"
    );
    assert!(
        text.contains("FIFO") && text.contains(REFUSAL_MARK) && text.contains(REDIRECTION_MARK),
        "the refusal names the kind and where that destination belongs: {text}"
    );
    assert!(
        still_a_fifo(&target),
        "the refusal moves no bytes and unlinks nothing, so a reader keeps its pipe"
    );
}

#[test]
fn an_edit_at_a_fifo_is_refused_before_the_tool_opens_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    fifo(dir.path(), "pipe.txt");

    let (is_error, text) = dispatched(
        "Edit",
        dir.path(),
        json!({"file_path": "pipe.txt", "old_text": "a", "new_text": "b"}),
    );

    assert!(
        is_error,
        "an edit reads its target whole before substituting, so a pipe has no such content: {text}"
    );
    assert!(
        text.contains("FIFO") && text.contains(REDIRECTION_MARK),
        "the refusal says which kind it found, not only that the call failed: {text}"
    );
}

#[test]
fn a_multi_edit_at_a_fifo_refuses_the_batch_and_names_the_offending_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    fifo(dir.path(), "pipe.txt");
    let plain = dir.path().join("plain.txt");
    std::fs::write(&plain, "first\n").expect("seed the plain target");

    let (is_error, text) = dispatched(
        "MultiEdit",
        dir.path(),
        json!({"edits": [
            {"file_path": "plain.txt", "old_text": "first", "new_text": "second"},
            {"file_path": "pipe.txt", "old_text": "a", "new_text": "b"}
        ]}),
    );

    assert!(
        is_error,
        "one unreplaceable destination refuses the whole batch: {text}"
    );
    assert!(
        text.contains("pipe.txt") && text.contains(REDIRECTION_MARK),
        "the refusal names which entry of the batch it settled on: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&plain).expect("target readable"),
        "first\n",
        "the refusal moves no bytes, not even those of the batch's sound entry"
    );
}

#[test]
fn a_write_at_a_device_target_is_refused_without_reading_it() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (is_error, text) = dispatched(
        "Write",
        dir.path(),
        json!({"file_path": "/dev/zero", "content": "hello\n"}),
    );

    assert!(
        is_error,
        "a device target reads without end, so it cannot be a replacement destination: {text}"
    );
    assert!(
        text.contains("character device") && text.contains(REDIRECTION_MARK),
        "the refusal names the kind, distinguishing it from a pipe: {text}"
    );
}

#[test]
fn a_write_at_a_regular_file_still_reaches_the_tool() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("config.txt");
    std::fs::write(&target, "old\n").expect("seed the target");

    let (is_error, text) = dispatched(
        "Write",
        dir.path(),
        json!({"file_path": "config.txt", "content": "port = 8080\n"}),
    );

    assert!(!is_error, "an ordinary write must land: {text}");
    assert!(
        !text.contains(REFUSAL_MARK),
        "the gate refuses nothing it should not: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "port = 8080\n",
        "the write itself reached the file"
    );
}

#[test]
fn a_write_at_a_new_file_is_not_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("fresh.txt");

    let (is_error, text) = dispatched(
        "Write",
        dir.path(),
        json!({"file_path": "fresh.txt", "content": "created\n"}),
    );

    assert!(
        !is_error,
        "creating a file is the ordinary route and the gate stays out of it: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "created\n",
        "the new file landed"
    );
}
