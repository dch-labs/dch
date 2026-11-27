//! The Unrestricted resolve policy's first-write cell, at the dch level.
//!
//! Every other write-guard pin builds its `FileSession` with the default
//! `Contained` policy, so the two steps `Unrestricted` takes on the way
//! to a write — creating the parent directory itself, and keying the
//! baseline on the canonicalized physical file — were pinned only inside
//! the dependency. Driven here through the production registry: a nested
//! first write through a symlinked directory lands, a read through the
//! physical spelling arms the same baseline, an external change made
//! behind one spelling is seen from the other, and a re-read through one
//! spelling clears the other's write — the step that tells one shared
//! key apart from a key per spelling. (`Contained` creates a
//! missing parent too, through its own descriptor-pinned walk, so parent
//! creation is not the contrast between the policies; the canonical key
//! is — under `Contained` the two spellings are two baselines.)

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default
)]

use std::path::Path;
use std::sync::Arc;

use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolDispatchResult;
use loopctl::middleware::ToolPipeline;
use loopctl::tool::PermissionCheck;
use loopctl::tool::ToolContext;
use loopctl::tool::builtin::fs::FileSession;
use loopctl::tool::builtin::fs::ResolvePolicy;
use serde_json::Value;
use serde_json::json;

/// The conflict refusal's stable words: the guard saw the disk move.
const CHANGED_ON_DISK: &str = "changed on disk since it was read";

/// The production registry over `session`, no middleware.
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
        call_id: "call_unrestricted".to_string(),
        turn_number: 0,
        cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
        permission: PermissionCheck::Allow,
        tool_context,
    };
    pipeline.dispatch(&mut ctx).await
}

/// A `Write` of `content` to `path` through the pipeline.
async fn write(
    pipeline: &ToolPipeline,
    session: &FileSession,
    cwd: &Path,
    path: &str,
    content: &str,
) -> ToolDispatchResult {
    dispatch(
        pipeline,
        session,
        cwd,
        "Write",
        json!({"file_path": path, "content": content}),
    )
    .await
}

/// A `Read` of `path` through the pipeline.
async fn read(
    pipeline: &ToolPipeline,
    session: &FileSession,
    cwd: &Path,
    path: &str,
) -> ToolDispatchResult {
    dispatch(pipeline, session, cwd, "Read", json!({"path": path})).await
}

#[cfg(unix)]
#[tokio::test]
async fn an_unrestricted_first_write_creates_the_parent_and_keys_its_baseline_canonically() {
    // `link` is a directory symlink inside the working directory. Under
    // the Unrestricted policy a write through it resolves to the physical
    // path first — the parent is created there, and the baseline is keyed
    // on the physical file — so every spelling of the file meets at one
    // baseline and a change made behind either is seen from both.
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).expect("the physical directory stands");
    std::os::unix::fs::symlink(&real, dir.path().join("link")).expect("a directory symlink in cwd");
    let session =
        FileSession::new(dir.path().to_path_buf()).with_resolve_policy(ResolvePolicy::Unrestricted);
    let pipeline = plain_pipeline(&session);
    let physical = real.join("deep").join("nested.txt");

    let first = write(
        &pipeline,
        &session,
        dir.path(),
        "link/deep/nested.txt",
        "v1",
    )
    .await;
    assert!(
        !first.is_error,
        "the nested first write lands: {}",
        first.output
    );
    assert_eq!(
        std::fs::read_to_string(&physical).expect("the physical file stands"),
        "v1",
        "the parent was created on the way, through the link"
    );

    let first_read = read(&pipeline, &session, dir.path(), "real/deep/nested.txt").await;
    assert!(
        !first_read.is_error,
        "the physical spelling reads: {}",
        first_read.output
    );

    let again = write(
        &pipeline,
        &session,
        dir.path(),
        "link/deep/nested.txt",
        "v2",
    )
    .await;
    assert!(
        !again.is_error,
        "the follow-up through the other spelling lands on the same baseline: {}",
        again.output
    );
    assert_eq!(
        std::fs::read_to_string(&physical).expect("the physical file stands"),
        "v2"
    );

    // the last read through the physical spelling, the change made behind
    // it, and the write through the link: only the shared canonical key
    // lets the link's write see the physical read's baseline
    let reread = read(&pipeline, &session, dir.path(), "real/deep/nested.txt").await;
    assert!(!reread.is_error, "{}", reread.output);
    std::fs::write(&physical, "external").expect("an external change behind the physical path");

    let refused = write(
        &pipeline,
        &session,
        dir.path(),
        "link/deep/nested.txt",
        "v3",
    )
    .await;
    let refusal = refused.output.to_string();
    assert!(
        refused.is_error && refusal.contains(CHANGED_ON_DISK),
        "the change made behind one spelling refuses a write through the other: {refusal}"
    );
    assert_eq!(
        std::fs::read_to_string(&physical).expect("the physical file stands"),
        "external",
        "the refused write moved no bytes"
    );

    // The step that tells the canonical key apart from per-spelling
    // keys: a re-read through the physical spelling refreshes the one
    // baseline both spellings share, so the write through the link
    // lands. Keyed by spelling, the link would still hold its stale
    // baseline from the `v2` write and refuse the same write again.
    let reread = read(&pipeline, &session, dir.path(), "real/deep/nested.txt").await;
    assert!(!reread.is_error, "{}", reread.output);
    let lands = write(
        &pipeline,
        &session,
        dir.path(),
        "link/deep/nested.txt",
        "v4",
    )
    .await;
    assert!(
        !lands.is_error,
        "a fresh read through one spelling clears the other's write: {}",
        lands.output
    );
    assert_eq!(
        std::fs::read_to_string(&physical).expect("the physical file stands"),
        "v4",
        "the link's write landed on the physical file"
    );
}
