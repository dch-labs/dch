//! Adversarial sequence stress for the detect-on-write conflict machinery.
//!
//! Drives the real tools (no test doubles) through the multi-call sequences
//! the per-tool unit tests cannot express: repeated writes without an
//! intervening read, external changes and reverts, recovery loops, and
//! baseline lookups across path spellings and multiple files. One shared
//! [`RunnerContext`] per scenario mirrors the runner's single-context wiring.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use loopctl::tool::{Tool, ToolContext};
use serde_json::json;

use dch_tools::RunnerContext;
use dch_tools::{EditInput, ReadInput, WriteInput};

fn ctx_in(cwd: &str) -> ToolContext {
    let mut ctx = ToolContext::default();
    ctx.cwd = cwd.to_string();
    ctx.set_extension(RunnerContext::new(PathBuf::from(cwd)));
    ctx
}

async fn read(ctx: &ToolContext, path: &str) {
    let tool = ReadInput::default();
    let out = tool.call(json!({ "file_path": path }), ctx).await.unwrap();
    assert!(!out.is_error, "read {path}: {}", out.text_content());
}

async fn write(ctx: &ToolContext, path: &str, content: &str) -> bool {
    let tool = WriteInput::default();
    let out = tool
        .call(json!({ "file_path": path, "content": content }), ctx)
        .await
        .unwrap();
    !out.is_error
}

async fn edit(ctx: &ToolContext, path: &str, old_text: &str, new_text: &str) -> bool {
    let tool = EditInput::default();
    let out = tool
        .call(
            json!({
                "file_path": path,
                "old_text": old_text,
                "new_text": new_text
            }),
            ctx,
        )
        .await
        .unwrap();
    !out.is_error
}

fn disk(tmp: &Path, name: &str) -> String {
    std::fs::read_to_string(tmp.join(name)).unwrap()
}

fn force_mtime_change(path: &Path) {
    let baseline = std::fs::metadata(path).unwrap().modified().unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(baseline + Duration::from_secs(30))
        .unwrap();
}

#[tokio::test]
async fn read_then_two_writes_both_succeed_without_an_external_change() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("note.txt"), "v1\n").unwrap();
    let ctx = ctx_in(tmp.path().to_str().unwrap());

    read(&ctx, "note.txt").await;
    let first = write(&ctx, "note.txt", "v2\n").await;
    let second = write(&ctx, "note.txt", "v3\n").await;

    assert!(first, "the first write must succeed");
    assert!(
        second,
        "the model's own write is not an external change: {}",
        disk(tmp.path(), "note.txt")
    );
    assert_eq!(disk(tmp.path(), "note.txt"), "v3\n");

    // The baseline after the second write is the model's own post-write
    // state — a genuine external change must still be caught.
    std::fs::write(tmp.path().join("note.txt"), "EXTERNAL\n").unwrap();
    force_mtime_change(&tmp.path().join("note.txt"));
    assert!(
        !write(&ctx, "note.txt", "v4\n").await,
        "an external change after the model's writes must conflict"
    );
    assert_eq!(disk(tmp.path(), "note.txt"), "EXTERNAL\n");
}

#[tokio::test]
async fn external_change_conflicts_the_write_and_a_reread_recovers_the_loop() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("note.txt"), "v1\n").unwrap();
    let ctx = ctx_in(tmp.path().to_str().unwrap());

    read(&ctx, "note.txt").await;
    std::fs::write(tmp.path().join("note.txt"), "EXTERNAL\n").unwrap();
    force_mtime_change(&tmp.path().join("note.txt"));
    assert!(
        !write(&ctx, "note.txt", "clobber\n").await,
        "the stale write must be refused"
    );
    assert_eq!(disk(tmp.path(), "note.txt"), "EXTERNAL\n");

    read(&ctx, "note.txt").await;
    assert!(
        write(&ctx, "note.txt", "v2\n").await,
        "after re-reading the external content the write must go through"
    );
    assert_eq!(disk(tmp.path(), "note.txt"), "v2\n");
}

#[tokio::test]
async fn external_revert_with_restored_bytes_still_conflicts_on_mtime() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("note.txt"), "v1\n").unwrap();
    let ctx = ctx_in(tmp.path().to_str().unwrap());

    read(&ctx, "note.txt").await;
    std::fs::write(tmp.path().join("note.txt"), "EXTERNAL\n").unwrap();
    force_mtime_change(&tmp.path().join("note.txt"));
    std::fs::write(tmp.path().join("note.txt"), "v1\n").unwrap();
    force_mtime_change(&tmp.path().join("note.txt"));

    // The bytes match the baseline again, but the mtime moved twice while the
    // model was not looking. Write's mtime method refuses — the cheap,
    // documented false-positive of the extrinsic baseline. A fresh Read
    // re-arms the write.
    assert!(
        !write(&ctx, "note.txt", "v2\n").await,
        "a moved mtime conflicts even when the bytes match"
    );
    read(&ctx, "note.txt").await;
    assert!(write(&ctx, "note.txt", "v2\n").await);
}

#[tokio::test]
async fn an_edit_between_read_and_write_keeps_the_write_unblocked() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("code.rs"), "fn a() {}\n").unwrap();
    let ctx = ctx_in(tmp.path().to_str().unwrap());

    read(&ctx, "code.rs").await;
    assert!(
        edit(&ctx, "code.rs", "fn a() {}", "fn b() {}").await,
        "the model's own edit applies"
    );
    // The edit refreshed the recorded baseline, so the model's own edit must
    // not register as an external change for the following write.
    assert!(
        write(&ctx, "code.rs", "fn c() {}\n").await,
        "a write after the model's own edit must succeed"
    );
    assert_eq!(disk(tmp.path(), "code.rs"), "fn c() {}\n");
}

#[tokio::test]
async fn alias_spelling_finds_no_baseline_and_allows_the_write() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("note.txt"), "v1\n").unwrap();
    let ctx = ctx_in(tmp.path().to_str().unwrap());

    read(&ctx, "note.txt").await;
    // A differently-spelled name for the same file carries no recorded
    // baseline (documented lookup contract) — Write proceeds unchecked.
    assert!(write(&ctx, "./note.txt", "v2\n").await);
    assert_eq!(disk(tmp.path(), "note.txt"), "v2\n");
}

#[tokio::test]
async fn reads_of_many_files_keep_distinct_baselines() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(tmp.path().join(name), format!("{name}\n")).unwrap();
    }
    let ctx = ctx_in(tmp.path().to_str().unwrap());

    read(&ctx, "a.txt").await;
    read(&ctx, "b.txt").await;
    read(&ctx, "a.txt").await;
    std::fs::write(tmp.path().join("b.txt"), "EXTERNAL\n").unwrap();
    force_mtime_change(&tmp.path().join("b.txt"));

    // a.txt's baseline is intact; b.txt's is stale. The checks must not
    // cross wires.
    assert!(write(&ctx, "a.txt", "a2\n").await);
    assert!(!write(&ctx, "b.txt", "b2\n").await);
    assert_eq!(disk(tmp.path(), "a.txt"), "a2\n");
    assert_eq!(disk(tmp.path(), "b.txt"), "EXTERNAL\n");
}
