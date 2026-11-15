//! The write guard's state-machine sequences, driven end-to-end through the
//! real tools.
//!
//! The staleness guard's gates were each pinned where they are implemented;
//! these pins walk the *orderings* an adversarial caller can reach — the
//! walkable paths no other suite exercised:
//!
//! - `skip_linter` bypasses the linter gate only — the guard's resumed
//!   hold and the conflict check run whatever the flag says, because the
//!   flag selects a validator, not a write path.
//! - An external change refuses the write; an external *revert to the
//!   observed bytes* releases it — the guard compares content, not
//!   timestamps, so returning the file to the bytes the model saw is
//!   exactly as honest as never touching it.
//! - A baseline recorded through one path spelling covers every other
//!   spelling of the same file — the map keys on the resolved path, so
//!   the guard cannot be dodged by re-spelling. Pinned on the refusal
//!   side, where a key miss would silently allow the clobber.
//! - A batch whose second file conflicts externally keeps the first
//!   file's write on disk and names it in a durable receipt — the batch
//!   is per-file atomic, and the partial result must survive compaction.
//!
//! The property test pins the contract both ways at once: the write lands
//! exactly when the file's current bytes are the bytes the model last
//! observed.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolOutput;
use loopctl::tool::builtin::ReadTool;
use loopctl::tool::builtin::fs::FileSession;
use loopctl::tool::builtin::fs::FileSource;
use loopctl::tool::builtin::fs::MultiEditTool;
use loopctl::tool::builtin::fs::WriteTool;
use serde_json::json;

/// A context whose cwd is `dir`, carrying the session the family shares.
///
/// The session is cloned into the context and into the read source, so a
/// read through one arms the guard every write consults — the wiring a
/// real dispatch installs.
fn ctx_in(cwd: &Path) -> (FileSession, ToolContext) {
    let session = FileSession::new(cwd.to_path_buf());
    let mut ctx = ToolContext {
        cwd: cwd.to_string_lossy().into_owned(),
        ..ToolContext::default()
    };
    session.attach(&mut ctx);
    (session, ctx)
}

/// Call `tool` at `input` and return the output, failing the test on a
/// hard dispatch error — everything under test here is a soft verdict.
async fn call(tool: &dyn Tool, input: serde_json::Value, ctx: &ToolContext) -> ToolOutput {
    tool.call(input, ctx)
        .await
        .expect("the dispatch itself must not fault")
}

/// A Read dispatch whose clean output proves the baseline armed.
async fn read_file(session: &FileSession, ctx: &ToolContext, path: &str) {
    let read = ReadTool::new(FileSource::new(session.clone()));
    let out = call(&read, json!({ "path": path }), ctx).await;
    assert!(
        !out.is_error,
        "the read must serve the file to arm the guard: {}",
        out.text_content()
    );
}

/// A Write dispatch, error-marked when the guard refused it.
async fn write_file(ctx: &ToolContext, file_path: &str, content: &str) -> ToolOutput {
    call(
        &WriteTool::new(),
        json!({ "file_path": file_path, "content": content }),
        ctx,
    )
    .await
}

/// The refusal the guard returns for a target whose bytes moved.
fn changed_mark() -> &'static str {
    "changed on disk"
}

/// The refusal the guard returns for a baseline only a resume armed.
fn resumed_mark() -> &'static str {
    "last read in a previous session"
}

#[tokio::test]
async fn a_pre_call_external_change_aborts_at_the_merge_with_the_anchor_wording() {
    // The companion to the mid-batch pin: a *pre-call* external
    // change never reaches the write phase — the anchor requirement
    // runs first, and the merge refuses with the anchor wording. No
    // staleness receipt, nothing on disk. The mid-batch twin names the
    // conflicting file in a durable receipt; this pins the other shape
    // so text-keying consumers can tell the two aborts apart.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("merged.rs");
    std::fs::write(&target, "fn a() {}\n").expect("seed the target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "merged.rs").await;
    // The external edit lands before the call: the model's anchor no
    // longer exists in the file the merge reads.
    std::fs::write(&target, "fn external() {}\n").expect("external edit");
    let multi = MultiEditTool::new();
    let out = call(
        &multi,
        json!({
            "file_path": "merged.rs",
            "edits": [{ "old_text": "fn a() {}", "new_text": "fn b() {}" }],
        }),
        &ctx,
    )
    .await;
    assert!(
        out.is_error,
        "the merge refuses the vanished anchor: {}",
        out.text_content()
    );
    assert!(
        out.text_content().contains("Old text not found"),
        "the abort carries the merge wording, not a staleness receipt: {}",
        out.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("the target survives"),
        "fn external() {}\n",
        "the abort writes nothing"
    );
}

#[tokio::test]
async fn skip_linter_never_bypasses_the_resumed_baseline_hold() {
    // The linter-off cell of the configuration matrix: a `skip_linter`
    // write against a resume-armed baseline must still hold. The flag
    // removes the validator from the write path; it must not remove the
    // staleness guard, or a resumed session could rewrite a file whose
    // bytes it never saw by declaring them invalid syntax.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("guarded.rs");
    std::fs::write(&target, "fn main() {}\n").expect("seed the target");
    let (session, ctx) = ctx_in(dir.path());
    assert!(
        session.record_resumed_read("guarded.rs").await,
        "the resume re-arm records the baseline"
    );

    let write = WriteTool::new();
    let held = call(
        &write,
        json!({
            "file_path": "guarded.rs",
            "content": "fn rewritten() {}",
            "skip_linter": true
        }),
        &ctx,
    )
    .await;
    assert!(held.is_error, "skip_linter must not lift the resumed hold");
    assert!(
        held.text_content().contains(resumed_mark()),
        "the refusal names the resume arm: {}",
        held.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "fn main() {}\n",
        "nothing may land while the hold stands"
    );

    // The live read releases the guard, and the same skip_linter write
    // then lands — the flag only ever governed validation.
    read_file(&session, &ctx, "guarded.rs").await;
    let released = call(
        &write,
        json!({
            "file_path": "guarded.rs",
            "content": "fn rewritten() {}",
            "skip_linter": true
        }),
        &ctx,
    )
    .await;
    assert!(
        !released.is_error,
        "a live read clears the hold whatever the flag says: {}",
        released.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "fn rewritten() {}",
        "the write lands after the release"
    );
}

#[tokio::test]
async fn skip_linter_never_bypasses_the_changed_content_hold() {
    // The stale-changed cell of the same matrix the resumed pin holds:
    // a `skip_linter` write against a file whose bytes moved since the
    // live read must refuse exactly as the validated write would. The
    // flag removes the validator only; the conflict check is not a
    // validator, and the refusal is what keeps a bypassed lint pass
    // from becoming a bypassed clobber guard.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("moved.rs");
    std::fs::write(&target, "fn main() {}\n").expect("seed the target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "moved.rs").await;

    std::fs::write(&target, "fn externally_moved() {}\n").expect("external change");
    let write = WriteTool::new();
    let held = call(
        &write,
        json!({
            "file_path": "moved.rs",
            "content": "fn rewritten() {}",
            "skip_linter": true
        }),
        &ctx,
    )
    .await;
    assert!(
        held.is_error,
        "skip_linter must not lift the conflict check"
    );
    assert!(
        held.text_content().contains(changed_mark()),
        "the refusal names the conflict: {}",
        held.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "fn externally_moved() {}\n",
        "nothing may land while the bytes are stale"
    );

    // The live read of the moved bytes releases the guard, and the same
    // bypassed-lint write then lands.
    read_file(&session, &ctx, "moved.rs").await;
    let released = call(
        &write,
        json!({
            "file_path": "moved.rs",
            "content": "fn rewritten() {}",
            "skip_linter": true
        }),
        &ctx,
    )
    .await;
    assert!(
        !released.is_error,
        "a fresh read clears the hold whatever the flag says: {}",
        released.text_content()
    );
}

/// A validator that always reports one diagnostic on `trigger` only.
///
/// The ordering pin needs a validator whose verdict is deterministic
/// and distinguishable from the guard's own wording; `LintGate` itself
/// is crate-private in dch-tools, and the property under test — which
/// gate refuses first — belongs to the validator seam, not to any one
/// validator's syntax rules.
struct RefusingValidator {
    trigger: &'static str,
}

impl loopctl::tool::builtin::fs::ContentValidator for RefusingValidator {
    fn validate<'a>(
        &'a self,
        path: &'a Path,
        _content: &'a str,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Vec<loopctl::tool::builtin::fs::ValidationDiagnostic>> + Send + 'a>,
    > {
        let fires = path.file_name().is_some_and(|name| name == self.trigger);
        Box::pin(async move {
            if fires {
                vec![loopctl::tool::builtin::fs::ValidationDiagnostic {
                    line: None,
                    message: "validator verdict".to_string(),
                }]
            } else {
                Vec::new()
            }
        })
    }
}

#[tokio::test]
async fn a_write_that_is_both_invalid_and_stale_reports_the_lint_first() {
    // The gate order made observable: the validator runs before the
    // staleness check, so a write that is both validator-refused and
    // stale surfaces the validator's refusal — the model fixes the
    // content first and meets the staleness refusal on its retry, one
    // verdict per round trip rather than two interleaved. The retry
    // with the flag that skips the validator then meets the staleness
    // refusal alone, proving the first refusal was the validator's and
    // not the guard's.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("both.rs");
    std::fs::write(&target, "fn a() {}\n").expect("seed the target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "both.rs").await;
    std::fs::write(&target, "fn external() {}\n").expect("external change");

    let validated =
        WriteTool::new().with_validator(Arc::new(RefusingValidator { trigger: "both.rs" }));
    let first = call(
        &validated,
        json!({ "file_path": "both.rs", "content": "replacement" }),
        &ctx,
    )
    .await;
    assert!(first.is_error, "a validator-refused write refuses");
    assert!(
        first.text_content().contains("validator verdict"),
        "the validator's verdict leads when both gates would refuse: {}",
        first.text_content()
    );
    assert!(
        !first.text_content().contains(changed_mark()),
        "the staleness verdict stays behind the validator's: {}",
        first.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "fn external() {}\n",
        "a refused write moves no bytes"
    );

    let skipped = call(
        &validated,
        json!({
            "file_path": "both.rs",
            "content": "replacement",
            "skip_linter": true
        }),
        &ctx,
    )
    .await;
    assert!(
        skipped.is_error && skipped.text_content().contains(changed_mark()),
        "with the validator skipped, the staleness refusal is the one left: {}",
        skipped.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "fn external() {}\n",
        "the stale write still moves no bytes"
    );
}

#[tokio::test]
async fn an_external_revert_to_the_observed_bytes_releases_the_guard() {
    // read → external change → refused write → external revert → the
    // write lands. The guard compares content hashes, so a file restored
    // to the bytes the model observed is exactly the file the guard
    // armed against — and the model's context matches disk again.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("watched.txt");
    std::fs::write(&target, "observed\n").expect("seed the target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "watched.txt").await;

    std::fs::write(&target, "external\n").expect("external writer");
    let refused = write_file(&ctx, "watched.txt", "ours\n").await;
    assert!(refused.is_error, "the moved bytes must refuse the write");
    assert!(
        refused.text_content().contains(changed_mark()),
        "the refusal names the conflict: {}",
        refused.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "external\n",
        "the external content survives the refusal"
    );

    // The revert: same path, same bytes the read observed.
    std::fs::write(&target, "observed\n").expect("external revert");
    let released = write_file(&ctx, "watched.txt", "ours\n").await;
    assert!(
        !released.is_error,
        "a revert to the observed bytes must release the write: {}",
        released.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "ours\n",
        "the write lands on the reverted file"
    );
}

#[tokio::test]
async fn a_baseline_through_one_spelling_covers_every_other_spelling() {
    // The relative and absolute spellings of one file meet at one
    // baseline. The refusal side is the load-bearing pin: a read through
    // one spelling followed by an external change must refuse a write
    // through the other — a key miss there would silently allow the
    // clobber, which is exactly the dodge the resolved-key rule exists
    // to close.
    let dir = tempfile::tempdir().expect("tempdir");
    let absolute = dir.path().join("spelled.txt");
    let absolute_spelling = absolute.to_string_lossy().into_owned();

    // Relative read, absolute write: the external change must refuse.
    std::fs::write(&absolute, "seen\n").expect("seed the target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "spelled.txt").await;
    std::fs::write(&absolute, "external\n").expect("external writer");
    let refused = write_file(&ctx, &absolute_spelling, "ours\n").await;
    assert!(
        refused.is_error && refused.text_content().contains(changed_mark()),
        "an absolute write must consult the relative read's baseline: {}",
        refused.text_content()
    );

    // Absolute read, relative write: same refusal, mirrored.
    std::fs::write(&absolute, "seen again\n").expect("re-seed the target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, &absolute_spelling).await;
    std::fs::write(&absolute, "external\n").expect("external writer");
    let refused = write_file(&ctx, "spelled.txt", "ours\n").await;
    assert!(
        refused.is_error && refused.text_content().contains(changed_mark()),
        "a relative write must consult the absolute read's baseline: {}",
        refused.text_content()
    );

    // The release side: with the bytes untouched, a write through the
    // other spelling lands — the shared key serves the honest path too.
    std::fs::write(&absolute, "seen again\n").expect("re-seed the target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "spelled.txt").await;
    let released = write_file(&ctx, &absolute_spelling, "ours\n").await;
    assert!(
        !released.is_error,
        "an unchanged file writes through either spelling: {}",
        released.text_content()
    );
}

/// An external writer hiding inside the batch: every time the gate
/// validates `trigger`'s content, it rewrites `victim` on disk. This is
/// the only public seam that lands a write strictly inside a batch —
/// after its read phase has captured the originals and before its
/// write phase re-checks them — which is the window the per-file
/// conflict check exists to close.
struct MidBatchWriter {
    trigger: &'static str,
    victim: PathBuf,
    bytes: &'static str,
}

impl loopctl::tool::builtin::fs::ContentValidator for MidBatchWriter {
    fn validate<'a>(
        &'a self,
        path: &'a Path,
        _content: &'a str,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Vec<loopctl::tool::builtin::fs::ValidationDiagnostic>> + Send + 'a>,
    > {
        let victim = self.victim.clone();
        let bytes = self.bytes;
        let fires = path.file_name().is_some_and(|name| name == self.trigger);
        Box::pin(async move {
            if fires {
                std::fs::write(&victim, bytes).expect("the mid-batch writer lands");
            }
            Vec::new()
        })
    }
}

#[tokio::test]
async fn a_mid_batch_conflict_keeps_earlier_writes_and_names_them() {
    // Multi-file batch, external conflict on the second file — the
    // external write lands mid-batch, between the read phase and the
    // write phase: the first file's write stays on disk, the receipt is
    // an error naming both the conflicted file and the files already
    // written, and the receipt is stamped durable — a partial batch
    // changed the world, so its record must survive compaction.
    let dir = tempfile::tempdir().expect("tempdir");
    let first = dir.path().join("first.rs");
    let second = dir.path().join("second.rs");
    std::fs::write(&first, "fn one() {}\n").expect("seed the first target");
    std::fs::write(&second, "fn two() {}\n").expect("seed the second target");
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "first.rs").await;
    read_file(&session, &ctx, "second.rs").await;

    let batch = MultiEditTool::new().with_validator(Arc::new(MidBatchWriter {
        trigger: "first.rs",
        victim: second.clone(),
        bytes: "externally changed\n",
    }));
    let out = call(
        &batch,
        json!({
            "edits": [
                {
                    "file_path": "first.rs",
                    "old_text": "fn one() {}",
                    "new_text": "fn one_rewritten() {}"
                },
                {
                    "file_path": "second.rs",
                    "old_text": "fn two() {}",
                    "new_text": "fn two_rewritten() {}"
                }
            ]
        }),
        &ctx,
    )
    .await;
    assert!(out.is_error, "the batch must abort at the conflict");
    let receipt = out.text_content();
    assert!(
        receipt.contains("second.rs") && receipt.contains(changed_mark()),
        "the receipt names the conflicted file: {receipt}"
    );
    assert!(
        receipt.contains("Already written by this batch: first.rs"),
        "the receipt names what reached the disk: {receipt}"
    );
    assert_eq!(
        out.retention,
        Some(loopctl::tool::Retention::Durable),
        "a partial apply changed state — its receipt must survive compaction"
    );
    assert_eq!(
        std::fs::read_to_string(&first).expect("first target readable"),
        "fn one_rewritten() {}\n",
        "the first file's write stays written"
    );
    assert_eq!(
        std::fs::read_to_string(&second).expect("second target readable"),
        "externally changed\n",
        "the conflicted file is untouched"
    );

    // A conflict before any write keeps the receipt evictable: the batch
    // that changed nothing is ordinary tool output, not a side-effect
    // record.
    let (session, ctx) = ctx_in(dir.path());
    read_file(&session, &ctx, "first.rs").await;
    let batch = MultiEditTool::new().with_validator(Arc::new(MidBatchWriter {
        trigger: "first.rs",
        victim: first.clone(),
        bytes: "externally changed\n",
    }));
    let out = call(
        &batch,
        json!({
            "edits": [
                {
                    "file_path": "first.rs",
                    "old_text": "fn one_rewritten() {}",
                    "new_text": "fn one_again() {}"
                }
            ]
        }),
        &ctx,
    )
    .await;
    assert!(out.is_error, "the conflict must abort the batch");
    assert!(
        !out.text_content().contains("Already written"),
        "nothing reached the disk, so the receipt names nothing written: {}",
        out.text_content()
    );
    assert_eq!(
        out.retention, None,
        "a conflict that wrote nothing is not a side-effect record"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn without_redaction_the_write_family_keeps_the_raw_route() {
    // The redaction on×off cell of the configuration matrix: the
    // write-target gate and the overwrite guard install only through
    // `with_redaction`, so a session started with redaction off has
    // neither. The module docs claim exactly this coupling — the raw
    // route replaces a FIFO where the guarded route refuses — and both
    // halves are pinned here, since the off-half is the one a future
    // refactor could silently change while the on-half pins keep
    // passing.
    let dir = tempfile::tempdir().expect("tempdir");
    let fifo = dir.path().join("pipe.conf");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "the fifo fixture exists");

    let (session, _ctx) = ctx_in(dir.path());

    let registry = loopctl::tool::ToolRegistry::new();
    registry.register(WriteTool::new());
    let raw = loopctl::middleware::ToolPipeline::builder()
        .with_core(Arc::new(registry))
        .build()
        .expect("the bare pipeline builds");

    // The guarded route refuses the FIFO destination outright.
    let guarded_registry = loopctl::tool::ToolRegistry::new();
    guarded_registry.register(WriteTool::new());
    let guarded =
        dch_loop::redaction_guard::with_redaction(loopctl::middleware::ToolPipeline::builder())
            .with_core(Arc::new(guarded_registry))
            .build()
            .expect("the guarded pipeline builds");
    let mut guarded_ctx = ToolContext {
        cwd: dir.path().to_string_lossy().into_owned(),
        ..ToolContext::default()
    };
    session.attach(&mut guarded_ctx);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        guarded.dispatch(&mut loopctl::middleware::ToolDispatchContext {
            tool_name: "Write".to_string(),
            input: json!({ "file_path": "pipe.conf", "content": "replacement" }),
            call_id: "guarded".to_string(),
            turn_number: 0,
            cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::Allow,
            tool_context: guarded_ctx,
        }),
    )
    .await
    .expect("the guarded route answers instead of parking on the fifo");
    assert!(
        result.is_error && result.output.to_string().contains("not a regular file"),
        "the guarded route refuses the fifo: {}",
        result.output
    );

    // The raw route answers too — and lands: the write replaces the
    // FIFO with an ordinary file, exactly what the gate exists to stop.
    let mut raw_ctx = ToolContext {
        cwd: dir.path().to_string_lossy().into_owned(),
        ..ToolContext::default()
    };
    session.attach(&mut raw_ctx);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        raw.dispatch(&mut loopctl::middleware::ToolDispatchContext {
            tool_name: "Write".to_string(),
            input: json!({ "file_path": "pipe.conf", "content": "replacement" }),
            call_id: "raw".to_string(),
            turn_number: 0,
            cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::Allow,
            tool_context: raw_ctx,
        }),
    )
    .await
    .expect("the raw route answers instead of parking on the fifo");
    assert!(
        !result.is_error,
        "the raw route writes through: {}",
        result.output
    );
    assert!(
        fifo.is_file(),
        "the raw route replaced the fifo with a regular file"
    );
    assert_eq!(
        std::fs::read_to_string(&fifo).expect("the replacement readable"),
        "replacement",
        "the raw route's write landed"
    );
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(64))]

    /// The guard's contract, both directions, over arbitrary content:
    /// the write lands exactly when the file's current bytes are the
    /// bytes the model last observed. An external writer that changes
    /// the bytes refuses; an external writer that restores them (or
    /// never disturbs them) releases. The verdict tracks the bytes —
    /// no timestamp, no stat smoothing.
    #[test]
    fn the_write_lands_exactly_when_disk_holds_the_observed_bytes(
        observed in "[a-zA-Z0-9]{1,24}\\n[a-z]{0,8}",
        external in "[a-zA-Z0-9]{1,24}\\n[a-z]{0,8}",
        ours in "[a-zA-Z0-9]{1,24}\\n",
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime builds");
        runtime.block_on(async move {
            let dir = tempfile::tempdir().expect("tempdir");
            let target = dir.path().join("prop.txt");
            std::fs::write(&target, &observed).expect("seed the target");
            let (session, ctx) = ctx_in(dir.path());
            read_file(&session, &ctx, "prop.txt").await;

            std::fs::write(&target, &external).expect("the external writer lands");
            let out = write_file(&ctx, "prop.txt", &ours).await;
            if external == observed {
                assert!(
                    !out.is_error,
                    "restored bytes release the write: {}",
                    out.text_content()
                );
                assert_eq!(
                    std::fs::read_to_string(&target).expect("target readable"),
                    ours,
                    "the write landed"
                );
            } else {
                assert!(
                    out.is_error,
                    "changed bytes refuse the write: {}",
                    out.text_content()
                );
                assert_eq!(
                    std::fs::read_to_string(&target).expect("target readable"),
                    external,
                    "the external content survived the refusal"
                );
            }
        });
    }
}
