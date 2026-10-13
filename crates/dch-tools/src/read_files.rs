//! The `ReadFiles` tool — several whole files in one call.
//!
//! A thin batch wrapper around a single-file `ReadTool`: every path
//! is served by a reader over the session's shared file state — the
//! same source the `Read` tool serves from — so the containment
//! policy and the staleness baseline are one and the same, and the
//! batched form differs only in how many files one round-trip
//! covers.

use std::future::Future;
use std::pin::Pin;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use loopctl::tool::builtin::fs::FileSource;
use loopctl::tool::builtin::read::ReadTool;
use serde_json::Value;
use serde_json::json;

/// Upper bound on paths one call may carry.
///
/// Bounds the joined payload and the per-call work; a caller with more
/// files splits across calls, each still far cheaper than one
/// round-trip per file.
const MAX_FILES_PER_CALL: usize = 10;

/// The line a file contributes when its own content did not fit the
/// call's remaining byte budget.
///
/// Names the remedy rather than leaving the header to be read as an
/// empty file: the bytes exist, the caller has to page for them.
const OVER_BUDGET: &str = "[not returned \u{2014} this call's byte budget is spent; \
                            read it with the Read tool, which pages by offset]";

/// The line a file contributes when the call's byte budget was already
/// spent before the file was reached.
///
/// Distinct from [`OVER_BUDGET`] because nothing was read: no staleness
/// baseline is armed for a path carrying this marker, and the same
/// remedy applies.
const BUDGET_SPENT: &str = "[not read \u{2014} this call's byte budget is spent; \
                            read it with the Read tool, which pages by offset]";

/// The blank line that closes one section of the joined batch output.
///
/// Counted into a section's charge, so the byte budget covers every byte
/// the batch writes — header, body, and separator alike — rather than the
/// body alone.
const SECTION_TAIL: &str = "\n\n";

/// Batched whole-file reader dispatching through a single-file
/// reader.
///
/// Constructed in [`builtin_registry`](crate::builtin_registry) with
/// a reader over the same
/// [`FileSession`](loopctl::tool::builtin::fs::FileSession) the
/// single-file tool serves from, so both forms enforce the session's
/// one containment policy and staleness baseline. Output is each
/// file's reader body under a `=== path ===` header, in request
/// order, while the whole call shares one byte budget: the leading
/// files arrive whole, the file whose content cannot fit, and every
/// file after it, contributes its header plus a marker naming the
/// paging remedy instead of its bytes. A failed file
/// contributes its error text under a marked header without failing the
/// call, and only an all-files failure returns an error result.
pub struct ReadFilesTool {
    /// The single-file reader every path is dispatched through.
    ///
    /// A [`ReadTool<FileSource>`] over the session's shared file
    /// state, the same source the `Read` tool serves from, so the
    /// containment policy and the staleness baseline match the
    /// single-file form's exactly. The default window matches for a
    /// different reason: it is the reader's construction constant,
    /// and both tools' readers are built the same way. A batched
    /// read through it behaves exactly like the per-file form it
    /// wraps.
    inner: ReadTool<FileSource>,

    /// The byte ceiling on one call's joined output, `0` for unbudgeted.
    ///
    /// One budget serves the whole batch, not one per file, so a
    /// multi-file read can never hand back more than a single tool
    /// result is meant to carry. Headers and separators count against
    /// it.
    max_batch_bytes: usize,
}

impl ReadFilesTool {
    /// Wrap the reader instance the batch dispatches through, bounded by
    /// `max_batch_bytes`.
    ///
    /// The reader serves the session's containment policy and
    /// staleness baseline, so a batched read through it is
    /// indistinguishable from calling `Read` per file. `max_batch_bytes`
    /// bounds one call's joined output; `0` leaves the batch unbudgeted.
    #[must_use]
    pub fn new(inner: ReadTool<FileSource>, max_batch_bytes: usize) -> Self {
        Self {
            inner,
            max_batch_bytes,
        }
    }
}

impl Tool for ReadFilesTool {
    fn name(&self) -> &'static str {
        "ReadFiles"
    }

    fn description(&self) -> &'static str {
        "Read several files in one call. When a task, protocol, or reading \
         list names more than one file, pass every path at once — each \
         returns the same numbered body the Read tool produces, under a \
         path header. The files share one byte budget: a file past it \
         comes back as a marker, not content, so page a long file's \
         remainder with the Read tool's offset deliberately instead of \
         discovering the cut."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File paths to read, in result order. \
                                        Relative paths resolve against the \
                                        working directory under the same \
                                        containment as the Read tool."
                    }
                },
                "required": ["files"]
            }),
        )
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let ctx = ctx.clone();
        Box::pin(self.read_files_inner(input, ctx))
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn system_prompt(&self) -> Option<String> {
        Some(
            "Read several files in one call: when a task, protocol, or \
              orientation list names several files, pass all the paths to \
              ReadFiles together instead of spending a round-trip per file. \
              Each file returns the Read tool's default window; the batch \
              shares one byte budget, so a file past the budget returns a \
              marker instead of content. A file that outlives its window or \
              the budget still needs the Read tool with offset."
                .to_string(),
        )
    }
}

impl ReadFilesTool {
    /// Body of [`Tool::call`].
    ///
    /// Dispatches each path through the wrapped reader sequentially,
    /// keeping result order equal to request order. A file that fails
    /// contributes its error body under a marked header and the call
    /// still succeeds; only when every file fails does the call return
    /// an error result.
    ///
    /// The batch spends one byte budget in request order: the leading
    /// files arrive whole, the first file whose content cannot fit
    /// arrives under a header marked as not returned, and every later
    /// file arrives under a header marked as not read at all. The
    /// overflow file is read — knowing a file cannot fit requires
    /// reading it — which arms its staleness baseline; the files after
    /// it are never touched. A budget of `0` is unbudgeted: every file
    /// is served. Each section is charged exactly the bytes it contributes,
    /// a failed read's wider header included, so the content a batch serves
    /// never exceeds the budget; the marker rows that follow exhaustion are
    /// fixed-size and sit outside it. A failed read is the one section
    /// exempt from the fit check: its body is the error text, short by
    /// construction and the fact the caller needs, so it is served even
    /// past the line and the charge saturates the budget to zero.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] when `files` is missing or
    /// empty, when any entry is not a string, or when it carries more
    /// than [`MAX_FILES_PER_CALL`] paths; otherwise propagates nothing —
    /// per-file failures are rendered into the output.
    async fn read_files_inner(
        &self,
        input: Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let items = input
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ToolError::InvalidInput("Missing files: pass at least one path".to_string())
            })?;
        let mut files: Vec<String> = Vec::new();
        for item in items {
            let Some(path) = item.as_str() else {
                return Err(ToolError::InvalidInput(format!(
                    "Every files entry must be a string path; found {item}"
                )));
            };
            files.push(path.to_string());
        }
        if files.is_empty() {
            return Err(ToolError::InvalidInput(
                "Missing files: pass at least one path".to_string(),
            ));
        }
        if files.len() > MAX_FILES_PER_CALL {
            return Err(ToolError::InvalidInput(format!(
                "At most {MAX_FILES_PER_CALL} files per call; split the batch"
            )));
        }
        let mut body = String::new();
        let mut failures: usize = 0;
        let mut remaining = if self.max_batch_bytes == 0 {
            usize::MAX
        } else {
            self.max_batch_bytes
        };
        for path in &files {
            if remaining == 0 {
                remaining = remaining.saturating_sub(Self::push_section(
                    &mut body,
                    &Self::section_header(path, false),
                    BUDGET_SPENT,
                ));
                continue;
            }
            let (failed, content) = match self.inner.call(json!({ "path": path }), &ctx).await {
                Ok(output) => (output.is_error, output.text_content()),
                Err(err) => (true, err.to_string()),
            };
            failures = failures.saturating_add(usize::from(failed));
            let header = Self::section_header(path, failed);
            let section_len = content
                .len()
                .saturating_add(header.len())
                .saturating_add(SECTION_TAIL.len());
            if !failed && section_len > remaining {
                remaining = 0;
                Self::push_section(&mut body, &Self::section_header(path, false), OVER_BUDGET);
                continue;
            }
            remaining = remaining.saturating_sub(Self::push_section(&mut body, &header, &content));
        }
        if failures == files.len() {
            return Ok(ToolOutput::error_text(body));
        }
        Ok(ToolOutput::text(body))
    }

    /// Append one file's header and body, returning the bytes appended.
    ///
    /// The single place the section shape (header, body, trailing blank
    /// line) is written, so a budgeted section and a served one are
    /// indistinguishable apart from their body. The returned count is what
    /// the batch's budget charges for that section, so a charge can never
    /// drift from the bytes the caller ends up reading.
    fn push_section(body: &mut String, header: &str, content: &str) -> usize {
        let before = body.len();
        body.push_str(header);
        body.push_str(content);
        body.push_str(SECTION_TAIL);
        body.len().saturating_sub(before)
    }

    /// The header line one file contributes, widened when its read failed.
    ///
    /// Both shapes live here because the budget charges the header that is
    /// actually written: a failed read names itself in the header, and that
    /// wider spelling is part of the section's cost.
    fn section_header(path: &str, failed: bool) -> String {
        if failed {
            format!("=== {path} (failed) ===\n")
        } else {
            format!("=== {path} ===\n")
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default
)]
mod tests {
    use super::*;
    use crate::context::RunnerContext;
    use loopctl::tool::builtin::fs::FileSession;
    use loopctl::tool::builtin::fs::FileSource;
    use std::path::PathBuf;

    /// A reader over a temp directory, plus that directory's path.
    ///
    /// The batch is unbudgeted: these tests pin the batch's ordering and
    /// failure shape, which the budget must not disturb.
    fn tool_in_tmp() -> (ReadFilesTool, tempfile::TempDir) {
        tool_in_tmp_budgeted(0)
    }

    /// A reader over a temp directory whose batch carries
    /// `max_batch_bytes`, plus that directory's path.
    fn tool_in_tmp_budgeted(max_batch_bytes: usize) -> (ReadFilesTool, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let reader = ReadTool::new(FileSource::new(FileSession::new(tmp.path().to_path_buf())));
        (ReadFilesTool::new(reader, max_batch_bytes), tmp)
    }

    /// A tool context whose cwd and [`RunnerContext`] both point at `cwd`.
    fn ctx_in(cwd: &std::path::Path) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = cwd.to_string_lossy().to_string();
        ctx.set_extension(RunnerContext::new(cwd.to_path_buf()));
        ctx
    }

    #[tokio::test]
    async fn several_files_return_in_order_under_path_headers() {
        let (tool, tmp) = tool_in_tmp();
        std::fs::write(tmp.path().join("a.rs"), "alpha\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "beta\n").unwrap();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({ "files": ["b.rs", "a.rs"] }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        let text = out.text_content();
        let b = text.find("=== b.rs ===").unwrap();
        let a = text.find("=== a.rs ===").unwrap();
        assert!(b < a, "result order matches request order: {text}");
        assert!(text.contains("beta") && text.contains("alpha"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_file_among_good_ones_still_returns_the_good_ones() {
        let (tool, tmp) = tool_in_tmp();
        std::fs::write(tmp.path().join("good.rs"), "kept\n").unwrap();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({ "files": ["good.rs", "absent.rs"] }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "one failure must not fail the batch");
        let text = out.text_content();
        assert!(text.contains("kept"), "{text}");
        assert!(text.contains("=== absent.rs (failed) ==="), "{text}");
    }

    #[tokio::test]
    async fn every_file_failing_is_an_error_result() {
        let (tool, tmp) = tool_in_tmp();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({ "files": ["absent1.rs", "absent2.rs"] }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error, "an all-failure batch is an error result");
    }

    #[tokio::test]
    async fn an_empty_or_missing_files_list_is_invalid_input() {
        let (tool, tmp) = tool_in_tmp();
        let ctx = ctx_in(tmp.path());
        for input in [json!({ "files": [] }), json!({})] {
            let err = tool.call(input, &ctx).await.unwrap_err();
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        }
    }

    #[tokio::test]
    async fn a_non_string_entry_is_invalid_input() {
        let (tool, tmp) = tool_in_tmp();
        std::fs::write(tmp.path().join("a.rs"), "alpha\n").unwrap();
        let ctx = ctx_in(tmp.path());
        let err = tool
            .call(json!({ "files": ["a.rs", 5] }), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref msg) if msg.contains("string")),
            "the malformed entry fails the whole batch loudly: {err:?}"
        );
    }

    #[tokio::test]
    async fn more_than_ten_files_is_invalid_input() {
        let (tool, tmp) = tool_in_tmp();
        let ctx = ctx_in(tmp.path());
        let many: Vec<String> = (0..11).map(|i| format!("f{i}.rs")).collect();
        let err = tool.call(json!({ "files": many }), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_batched_read_arms_the_staleness_guard_for_every_file_in_the_batch() {
        // The batch dispatches through the session's shared file source, so
        // each path in the call records the read baseline the single-file
        // Read records. Wired the production way — one session attached to
        // the context and threaded into the reader — a later Write against
        // a batch-read file must be held when the file changed externally
        // since the batch, and must land for the batch-read file nobody
        // touched. The guard must not see a batched read as "never read".
        use loopctl::tool::builtin::fs::WriteTool;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("held.rs"), "v1\n").expect("seed held");
        std::fs::write(tmp.path().join("clean.rs"), "v1\n").expect("seed clean");
        let session = FileSession::new(tmp.path().to_path_buf());
        let tool = ReadFilesTool::new(ReadTool::new(FileSource::new(session.clone())), 0);
        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().to_string();
        ctx.set_extension(RunnerContext::new(tmp.path().to_path_buf()));
        session.attach(&mut ctx);
        let out = tool
            .call(json!({ "files": ["held.rs", "clean.rs"] }), &ctx)
            .await
            .expect("the batch reads");
        assert!(!out.is_error, "{}", out.text_content());

        std::fs::write(tmp.path().join("held.rs"), "externally changed\n")
            .expect("mutate one batch target");
        let writer = WriteTool::new();
        let held = writer
            .call(json!({ "file_path": "held.rs", "content": "ours\n" }), &ctx)
            .await
            .expect("a refused write is a soft error, not a failure");
        assert!(
            held.is_error,
            "the changed batch-read file must hold the write: {}",
            held.text_content()
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("held.rs")).unwrap_or_default(),
            "externally changed\n",
            "the held write moves no bytes"
        );

        let clean = writer
            .call(
                json!({ "file_path": "clean.rs", "content": "ours\n" }),
                &ctx,
            )
            .await
            .expect("the clean write succeeds");
        assert!(
            !clean.is_error,
            "the untouched batch-read file accepts its write: {}",
            clean.text_content()
        );
    }

    /// A reader over an existing directory whose batch carries
    /// `max_batch_bytes`.
    ///
    /// The budget tests need two tools over one directory — a probe that
    /// serves everything, then a run under a budget derived from what the
    /// probe wrote — because a budget is fixed when the tool is built.
    fn tool_over(path: &std::path::Path, max_batch_bytes: usize) -> ReadFilesTool {
        let reader = ReadTool::new(FileSource::new(FileSession::new(path.to_path_buf())));
        ReadFilesTool::new(reader, max_batch_bytes)
    }

    /// A filler payload of exactly `bytes` `x` characters.
    fn payload(bytes: usize) -> String {
        "x".repeat(bytes)
    }

    #[tokio::test]
    async fn a_batch_over_its_budget_returns_the_files_that_fit_in_order() {
        let (tool, tmp) = tool_in_tmp_budgeted(6_050);
        std::fs::write(tmp.path().join("a.rs"), payload(3_000)).unwrap();
        std::fs::write(tmp.path().join("b.rs"), payload(3_000)).unwrap();
        std::fs::write(tmp.path().join("c.rs"), payload(3_000)).unwrap();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({ "files": ["a.rs", "b.rs", "c.rs"] }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        let text = out.text_content();
        let a = text.find("=== a.rs ===").unwrap();
        let b = text.find("=== b.rs ===").unwrap();
        let c = text.find("=== c.rs ===").unwrap();
        assert!(a < b && b < c, "sections keep request order: {text}");
        let head = &text[..c];
        let served = head.matches('x').count();
        assert_eq!(
            served, 6_000,
            "the two files that fit arrive whole, nothing else's bytes appear"
        );
        assert!(
            text[c..].contains("not returned"),
            "the third is marked: {text}"
        );
    }

    #[tokio::test]
    async fn the_file_that_cannot_fit_is_marked_and_names_its_remedy() {
        let (tool, tmp) = tool_in_tmp_budgeted(1_500);
        std::fs::write(tmp.path().join("a.rs"), payload(3_000)).unwrap();
        std::fs::write(tmp.path().join("b.rs"), payload(3_000)).unwrap();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({ "files": ["a.rs", "b.rs"] }), &ctx)
            .await
            .unwrap();
        let text = out.text_content();
        assert!(
            !text.contains('x'),
            "no over-budget bytes are served: {text}"
        );
        assert!(text.contains("=== a.rs ==="), "the header survives: {text}");
        assert!(
            text.contains("not returned"),
            "the marker is honest: {text}"
        );
        assert!(
            text.contains("Read tool") && text.contains("pages by offset"),
            "the marker names the paging remedy: {text}"
        );
    }

    #[tokio::test]
    async fn files_after_the_overflow_are_never_read() {
        use loopctl::tool::builtin::fs::WriteTool;
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            std::fs::write(tmp.path().join(name), payload(3_000)).unwrap();
        }
        let session = FileSession::new(tmp.path().to_path_buf());
        let tool = ReadFilesTool::new(ReadTool::new(FileSource::new(session.clone())), 6_050);
        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().to_string();
        ctx.set_extension(RunnerContext::new(tmp.path().to_path_buf()));
        session.attach(&mut ctx);
        let out = tool
            .call(json!({ "files": ["a.rs", "b.rs", "c.rs", "d.rs"] }), &ctx)
            .await
            .unwrap();
        let text = out.text_content();
        let d = text.find("=== d.rs ===").unwrap();
        assert!(
            text[d..].contains("not read"),
            "the file after the overflow is marked as never read: {text}"
        );

        let writer = WriteTool::new();
        let written = writer
            .call(json!({ "file_path": "d.rs", "content": "written\n" }), &ctx)
            .await
            .expect("a write is attempted");
        assert!(
            !written.is_error,
            "no staleness baseline was armed for the unread file: {}",
            written.text_content()
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("d.rs")).unwrap(),
            "written\n",
            "the write landed, proving the scanner never read the file"
        );
    }

    #[tokio::test]
    async fn an_unbudgeted_batch_serves_every_file_whole() {
        let (tool, tmp) = tool_in_tmp_budgeted(0);
        std::fs::write(tmp.path().join("a.rs"), payload(3_000)).unwrap();
        std::fs::write(tmp.path().join("b.rs"), payload(3_000)).unwrap();
        std::fs::write(tmp.path().join("c.rs"), payload(3_000)).unwrap();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({ "files": ["a.rs", "b.rs", "c.rs"] }), &ctx)
            .await
            .unwrap();
        let text = out.text_content();
        assert_eq!(
            text.matches('x').count(),
            9_000,
            "0 is unbudgeted, not a ceiling that serves nothing"
        );
        assert!(
            !text.contains("not returned") && !text.contains("not read"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_failed_section_is_charged_the_header_it_actually_writes() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("good.rs"), payload(40)).unwrap();
        let ctx = ctx_in(tmp.path());
        let whole = tool_over(tmp.path(), 0)
            .call(json!({ "files": ["nope.rs", "good.rs"] }), &ctx)
            .await
            .unwrap()
            .text_content();
        assert!(
            whole.contains("=== good.rs ==="),
            "the unbudgeted batch serves the readable file: {whole}"
        );

        let marked = tool_over(tmp.path(), whole.len().saturating_sub(1))
            .call(json!({ "files": ["nope.rs", "good.rs"] }), &ctx)
            .await
            .unwrap()
            .text_content();
        assert!(
            marked.contains("not returned"),
            "one byte short of the batch as written, the last section is \
             marked instead of served: {marked}"
        );
        assert!(
            !marked.contains("xxxx"),
            "a section the budget cannot carry contributes no content: {marked}"
        );
    }

    #[tokio::test]
    async fn a_failed_read_too_big_for_the_budget_is_served_with_its_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = ctx_in(tmp.path());
        let whole = tool_over(tmp.path(), 0)
            .call(json!({ "files": ["nope.rs"] }), &ctx)
            .await
            .unwrap()
            .text_content();
        assert!(
            whole.contains("(failed)"),
            "the unbudgeted form serves a failure under its marked header: {whole}"
        );

        // A failure's body is its error text — short, and the one
        // thing the model needs to act on — so the budget never
        // replaces a failed section with the not-returned marker:
        // that marker's remedy (read it with the Read tool) is a lie
        // for a file that does not exist.
        let served = tool_over(tmp.path(), 16)
            .call(json!({ "files": ["nope.rs"] }), &ctx)
            .await
            .unwrap()
            .text_content();
        assert!(
            served.contains("(failed)"),
            "a failure wider than the budget is still served under its \
             marked header: {served}"
        );
        assert!(
            served.contains("nope.rs"),
            "the failed section names the file: {served}"
        );
        assert!(
            !served.contains("not returned"),
            "the marker never replaces a failure's own error text: {served}"
        );
    }

    #[tokio::test]
    async fn a_fitting_batch_is_byte_identical_to_the_unbudgeted_form() {
        let (roomy, roomy_dir) = tool_in_tmp_budgeted(50_000);
        let (plain, plain_dir) = tool_in_tmp_budgeted(0);
        let files = json!({ "files": ["a.rs", "absent.rs", "b.rs"] });
        for dir in [roomy_dir.path(), plain_dir.path()] {
            std::fs::write(dir.join("a.rs"), payload(3_000)).unwrap();
            std::fs::write(dir.join("b.rs"), payload(3_000)).unwrap();
        }
        let roomy_out = roomy
            .call(files.clone(), &ctx_in(roomy_dir.path()))
            .await
            .unwrap();
        let plain_out = plain.call(files, &ctx_in(plain_dir.path())).await.unwrap();
        assert_eq!(
            roomy_out.text_content(),
            plain_out.text_content(),
            "a generous budget changes nothing, partial failures included"
        );
    }

    #[test]
    fn trait_contract_and_registry() {
        let session = FileSession::new(PathBuf::from("."));
        let reader = ReadTool::new(FileSource::new(session.clone()));
        let tool = ReadFilesTool::new(reader, 0);
        assert_eq!(tool.name(), "ReadFiles");
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        assert_eq!(
            crate::permission::tool_category("ReadFiles"),
            crate::permission::ToolCategory::FileRead
        );
        let reg =
            crate::registry::builtin_registry(&session, crate::registry::ReadWindows::default());
        assert!(reg.get("ReadFiles").is_some(), "ReadFiles registered");
        assert!(
            reg.get("Read").is_some(),
            "single-file Read still registered"
        );
    }
}
