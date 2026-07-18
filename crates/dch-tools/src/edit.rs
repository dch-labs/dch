//! The Edit tool — replace a unique occurrence of text in a file.
//!
//! Edit reads a file, locates `old_text`, and requires it to appear exactly
//! once (non-overlapping). The replacement is run through the linter gate
//! before writing, and the result is returned as a line diff preview.

use std::future::Future;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;

use crate::context::RunnerContext;
use crate::context::runner_ctx;
use crate::diff::format_file_change;
use crate::linter::LinterResult;
use crate::linter::lint_content;
use crate::util::is_url;
use crate::write::format_lint_failure;

/// Edit a file by replacing a **unique** occurrence of text. Runs the linter
/// gate on the result before writing; returns a line diff preview.
///
/// Not concurrency-safe and not read-only: editing mutates a file, and two
/// concurrent edits to the same path would race.
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn description(&self) -> &'static str {
        "Edit a file by replacing text. Syntax validation is automatically \
         performed for supported file types."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The path to the file to edit"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "The text to replace"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "The replacement text"
                    },
                    "skip_linter": {
                        "type": "boolean",
                        "description": "Skip syntax validation (not recommended)",
                        "default": false
                    }
                },
                "required": ["file_path", "old_text", "new_text"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let rc = runner_ctx(ctx).cloned();
        Box::pin(self.edit_inner(input, rc))
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn system_prompt(&self) -> Option<String> {
        Some(
            "old_text must be unique in the file. For multiple changes, use \
             MultiEdit. Both run the linter after applying."
                .to_string(),
        )
    }
}

impl EditTool {
    /// Body of [`Tool::call`].
    ///
    /// Orchestrates parse → read → apply → lint → write. Recoverable conditions
    /// (text not found, ambiguous match, linter failure) are surfaced as soft
    /// [`ToolOutput`] errors; hard failures (bad args, missing file, I/O fault)
    /// become [`ToolError`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for a missing `file_path`/`old_text`/
    /// `new_text`, an empty `old_text`, or a URL `file_path`. Returns
    /// [`ToolError::FileNotFound`] when the target does not exist. Returns
    /// [`ToolError::Execution`] on a genuine I/O fault.
    async fn edit_inner(
        &self,
        input: Value,
        rc: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let cwd = rc
            .as_ref()
            .ok_or_else(|| {
                ToolError::Execution(
                    "RunnerContext extension is not installed on the ToolContext".to_string(),
                )
            })?
            .cwd
            .clone();
        let parsed = parse_input(&input)?;
        let full_path = resolve_path(parsed.file_path, &cwd);
        let old_content = read_existing(&full_path, parsed.file_path).await?;
        let new_content = match apply_edit(&old_content, parsed.old_text, parsed.new_text) {
            Ok(c) => c,
            Err(reason) => return Ok(reason.into_output()),
        };

        if !parsed.skip_linter {
            if let Err(result) = check_linter(&full_path, &new_content) {
                return Ok(ToolOutput::error_text(format_lint_failure(
                    &full_path, &result,
                )));
            }
        }

        crate::fs::atomic_write(&full_path, &new_content)?;
        let message = format_file_change(parsed.file_path, Some(&old_content), &new_content);
        Ok(ToolOutput::text(message))
    }
}

/// Parsed and validated Edit input.
#[derive(Debug)]
struct EditInput<'a> {
    /// The file path exactly as supplied by the caller (before cwd resolution),
    /// borrowed from the input. Used in messages so the model sees the path it
    /// named, not the canonicalized form.
    file_path: &'a str,
    /// The text to find in the file.
    old_text: &'a str,
    /// The text to replace `old_text` with.
    new_text: &'a str,
    /// Whether to skip the linter gate on the result.
    skip_linter: bool,
}

/// A recoverable reason an edit was not applied.
///
/// These are *soft* failures modes: the caller surfaces them to the loop as a
/// `ToolOutput::error_text` so the model can correct and retry, rather than as
/// a hard [`ToolError`]. [`EditError::into_output`] is the single place the
/// structured reason is formatted for the loop.
#[derive(Debug, PartialEq, Eq)]
enum EditError {
    /// `old_text` does not appear in the file. Produced by [`apply_edit`] when
    /// [`locate_unique`] returns [`FindResult::NotFound`].
    NotFound,
    /// `old_text` appears more than once; it must be unique. Produced by
    /// [`apply_edit`] when [`locate_unique`] returns [`FindResult::Ambiguous`].
    Ambiguous {
        /// The non-overlapping occurrence count (always greater than 1).
        count: usize,
    },
}

impl EditError {
    /// Format this reason as the soft [`ToolOutput`] returned to the loop.
    fn into_output(self) -> ToolOutput {
        match self {
            EditError::NotFound => ToolOutput::error_text(
                "Old text not found in file.\n\n\
                 Hints:\n  \
                 - The file may have changed since you last read it — try re-reading with `Read`\n  \
                 - Check for whitespace or Unicode differences\n  \
                 - Use `Grep` to search for the text you want to replace",
            ),
            EditError::Ambiguous { count } => ToolOutput::error_text(format!(
                "old_text appears {count} times in the file; it must be unique. \
                 Add surrounding context to disambiguate, or use MultiEdit."
            )),
        }
    }
}

/// Extract the Edit arguments from the JSON `input` and validate them.
///
/// `file_path`, `old_text`, and `new_text` must be present strings; `old_text`
/// must be non-empty; `file_path` must not be a URL.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] for a missing field, an empty
/// `old_text`, or a URL `file_path`.
fn parse_input(input: &Value) -> Result<EditInput<'_>, ToolError> {
    let file_path = input
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("Missing file_path".to_string()))?;
    let old_text = input
        .get("old_text")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("Missing old_text".to_string()))?;
    let new_text = input
        .get("new_text")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("Missing new_text".to_string()))?;
    let skip_linter = input
        .get("skip_linter")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if old_text.is_empty() {
        return Err(ToolError::InvalidInput(
            "old_text must not be empty".to_string(),
        ));
    }

    if is_url(file_path) {
        return Err(ToolError::InvalidInput(
            "URLs are not supported by the Edit tool. Use WebFetch for URLs.".to_string(),
        ));
    }
    Ok(EditInput {
        file_path,
        old_text,
        new_text,
        skip_linter,
    })
}

/// Resolve a possibly-relative `file_path` against `cwd`.
///
/// Absolute paths are used as-is; relative paths are joined to `cwd`.
fn resolve_path(file_path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_relative() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

/// Read an existing file's full contents as UTF-8.
///
/// Distinguishes a missing file ([`ToolError::FileNotFound`]) from a genuine
/// I/O fault ([`ToolError::Execution`]). The `display_path` is used verbatim in
/// the not-found error so the caller sees the path it supplied.
///
/// # Errors
///
/// Returns [`ToolError::FileNotFound`] when the file does not exist, and
/// [`ToolError::Execution`] on any other I/O error (including non-UTF-8 reads).
async fn read_existing(full_path: &Path, display_path: &str) -> Result<String, ToolError> {
    if !tokio::fs::try_exists(full_path)
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?
    {
        return Err(ToolError::FileNotFound(display_path.to_string()));
    }
    tokio::fs::read_to_string(full_path)
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))
}

/// Locate `old_text` in `content` and splice `new_text` into its place.
///
/// On success returns the new content. The result is structured; the caller
/// formats the error for the loop via [`EditError::into_output`].
///
/// # Errors
///
/// Returns [`EditError::NotFound`] when `old_text` is absent, or
/// [`EditError::Ambiguous`] when it appears more than once.
fn apply_edit(content: &str, old_text: &str, new_text: &str) -> Result<String, EditError> {
    match locate_unique(content, old_text) {
        FindResult::NotFound => Err(EditError::NotFound),
        FindResult::Ambiguous { count } => Err(EditError::Ambiguous { count }),
        FindResult::Unique(range) => Ok(splice(content, range, new_text)),
    }
}

/// Run the linter gate on the candidate `new_content`.
///
/// Returns `Ok(())` when the content passes (or the extension is unsupported,
/// which `lint_content` no-ops). The `skip_linter` decision lives in the
/// orchestrator, not here — this function always lints.
///
/// # Errors
///
/// Returns `Err(LinterResult)` — the *failing* result, with `is_valid == false`
/// and a non-empty `errors` list — when the content fails validation. The
/// caller formats the diagnostics for the loop.
fn check_linter(full_path: &Path, new_content: &str) -> Result<(), LinterResult> {
    let result = lint_content(full_path, new_content);
    if result.is_valid { Ok(()) } else { Err(result) }
}

/// Outcome of locating `old_text` within the file content.
///
/// Produced by [`locate_unique`]. Edit treats each variant differently: a
/// [`FindResult::Unique`] result is spliced and written; the other two become
/// soft errors returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FindResult {
    /// Exactly one non-overlapping occurrence, with its byte range in `content`.
    Unique(Range<usize>),
    /// `old_text` does not appear in `content`.
    NotFound,
    /// `old_text` appears more than once.
    Ambiguous {
        /// The non-overlapping occurrence count (always greater than 1).
        count: usize,
    },
}

/// Classify how many non-overlapping times `old_text` occurs in `content`.
///
/// Uses `str::matches` (non-overlapping count) and `str::find` (first
/// position). Returns [`FindResult::Unique`] only for exactly one occurrence,
/// carrying the byte range to splice into.
pub(crate) fn locate_unique(content: &str, old_text: &str) -> FindResult {
    let Some(start) = content.find(old_text) else {
        return FindResult::NotFound;
    };
    let after_first = start.saturating_add(old_text.len());
    if content
        .get(after_first..)
        .is_some_and(|rest| rest.contains(old_text))
    {
        let count = content.matches(old_text).count();
        return FindResult::Ambiguous { count };
    }
    let end = start.saturating_add(old_text.len());
    FindResult::Unique(start..end)
}

/// Splice `replacement` into `content`, replacing the byte `range`.
///
/// The `range` must be a valid UTF-8-boundary slice of `content` as produced by
/// [`locate_unique`]; this holds by construction because `str::find` returns
/// char-boundary offsets. The result is the prefix before `range.start`, the
/// `replacement`, then the suffix from `range.end`.
pub(crate) fn splice(content: &str, range: Range<usize>, replacement: &str) -> String {
    let prefix = content.get(..range.start).unwrap_or("");
    let suffix = content.get(range.end..).unwrap_or("");
    let cap = prefix
        .len()
        .saturating_add(replacement.len())
        .saturating_add(suffix.len());
    let mut result = String::with_capacity(cap);
    result.push_str(prefix);
    result.push_str(replacement);
    result.push_str(suffix);
    result
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
    use crate::runtime::RuntimeConfig;
    use crate::state::SessionState;
    use loopctl::tool::ToolContext;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Builds a `ToolContext` with a `RunnerContext` pointing at `cwd`.
    fn ctx_in(cwd: &str) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = cwd.to_string();
        let rc = RunnerContext {
            cwd: PathBuf::from(cwd),
            session_state: Arc::new(Mutex::new(SessionState::default())),
            question_tx: None,
            runtime: RuntimeConfig::default(),
        };
        ctx.set_extension(rc);
        ctx
    }

    #[tokio::test]
    async fn happy_path_unique_replace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("src.rs");
        std::fs::write(&target, "fn main() { println!(\"hi\"); }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "src.rs",
            "old_text": "println!(\"hi\")",
            "new_text": "println!(\"bye\")"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(written.contains("bye"));
        assert!(!written.contains("hi"));
        assert!(out.text_content().contains("Changed: src.rs"));
        assert!(out.text_content().contains("+ "));
    }

    #[tokio::test]
    async fn not_found_is_soft_error_file_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("f.txt");
        let original = "line one\nline two\n";
        std::fs::write(&target, original).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "f.txt",
            "old_text": "absent text",
            "new_text": "whatever"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        let text = out.text_content();
        assert!(text.contains("not found") || text.contains("Not found"));
        assert!(text.contains("Read") || text.contains("Grep") || text.contains("Unicode"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), original);
    }

    #[tokio::test]
    async fn ambiguous_is_soft_error_mentions_multiedit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("f.txt");
        let original = "dup\ndup\ndup\n";
        std::fs::write(&target, original).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "f.txt",
            "old_text": "dup",
            "new_text": "x"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        let text = out.text_content();
        assert!(text.contains('3'), "{text}");
        assert!(text.contains("MultiEdit"), "{text}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), original);
    }

    #[tokio::test]
    async fn empty_old_text_is_hard_invalid_input() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("f.txt");
        std::fs::write(&target, "content\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "f.txt",
            "old_text": "",
            "new_text": "x"
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("empty")),
            "{err:?}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "content\n");
    }

    #[tokio::test]
    async fn linter_blocks_bad_rust_edit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("bad.rs");
        std::fs::write(&target, "fn main() { let x = 1; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "bad.rs",
            "old_text": "let x = 1;",
            "new_text": "let x = ;"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("Syntax validation failed"), "{text}");
        assert!(text.contains("skip_linter"), "{text}");
        // File NOT written.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "fn main() { let x = 1; }\n"
        );
    }

    #[tokio::test]
    async fn skip_linter_writes_anyway() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("bad.rs");
        std::fs::write(&target, "fn main() { let x = 1; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "bad.rs",
            "old_text": "let x = 1;",
            "new_text": "let x = ;",
            "skip_linter": true
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "fn main() { let x = ; }\n"
        );
    }

    #[tokio::test]
    async fn unsupported_extension_passes_through() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("notes.md");
        std::fs::write(&target, "# Title\nbody\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "notes.md",
            "old_text": "body",
            "new_text": "new body"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("new body")
        );
    }

    #[tokio::test]
    async fn file_not_found_is_file_not_found_variant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "nope.rs",
            "old_text": "x",
            "new_text": "y"
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::FileNotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn relative_path_resolved_against_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("sub");
        std::fs::create_dir(&nested).unwrap();
        let target = nested.join("rel.rs");
        std::fs::write(&target, "fn old() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "sub/rel.rs",
            "old_text": "fn old() {}",
            "new_text": "fn new() {}"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(std::fs::read_to_string(&target).unwrap().contains("new()"));
    }

    #[tokio::test]
    async fn missing_new_text_is_invalid_input() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let err = tool
            .call(json!({ "file_path": "x.rs", "old_text": "a" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn url_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "https://example.com/page",
            "old_text": "a",
            "new_text": "b"
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")),
            "{err:?}"
        );
    }

    #[test]
    fn not_read_only_and_not_concurrency_safe() {
        let tool = EditTool;
        assert!(!tool.is_read_only());
        assert!(!tool.is_concurrency_safe());
    }

    #[test]
    fn edittool_registered_in_builtin_registry() {
        let reg = crate::registry::builtin_registry();
        let tool = reg.get("Edit").expect("EditTool registered");
        assert!(!tool.is_read_only());
        assert!(!tool.is_concurrency_safe());
    }

    #[tokio::test]
    async fn atomic_write_leaves_no_temp_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("clean.rs");
        std::fs::write(&target, "fn main() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = EditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "clean.rs",
            "old_text": "fn main() {}",
            "new_text": "fn main() { println!(); }"
        });
        tool.call(input, &ctx).await.unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["clean.rs"]);
    }

    #[test]
    fn locate_unique_byte_range_splice_correct() {
        let content = "hello world";
        let res = locate_unique(content, "world");
        assert_eq!(res, FindResult::Unique(6..11));
        assert_eq!(splice(content, 6..11, "rust"), "hello rust");
    }

    #[test]
    fn locate_unique_not_found_and_ambiguous() {
        assert_eq!(locate_unique("abc", "xyz"), FindResult::NotFound);
        assert_eq!(
            locate_unique("dup dup dup", "dup"),
            FindResult::Ambiguous { count: 3 }
        );
    }

    #[test]
    fn locate_unique_non_overlapping_edge_case() {
        // "aa" in "aaaa" — non-overlapping count is 2, not 3.
        assert_eq!(
            locate_unique("aaaa", "aa"),
            FindResult::Ambiguous { count: 2 }
        );
        // A single occurrence at the start is unique.
        assert_eq!(locate_unique("aab", "aa"), FindResult::Unique(0..2));
    }

    #[test]
    fn parse_input_valid_full() {
        let input = json!({
            "file_path": "a.rs",
            "old_text": "x",
            "new_text": "y",
            "skip_linter": true
        });
        let parsed = parse_input(&input).unwrap();
        assert_eq!(parsed.file_path, "a.rs");
        assert_eq!(parsed.old_text, "x");
        assert_eq!(parsed.new_text, "y");
        assert!(parsed.skip_linter);
    }

    #[test]
    fn parse_input_defaults_skip_linter_false() {
        let input = json!({
            "file_path": "a.rs",
            "old_text": "x",
            "new_text": "y"
        });
        let parsed = parse_input(&input).unwrap();
        assert!(!parsed.skip_linter);
    }

    #[test]
    fn parse_input_missing_fields_are_invalid_input() {
        assert!(matches!(
            parse_input(&json!({ "old_text": "x", "new_text": "y" })).unwrap_err(),
            ToolError::InvalidInput(ref s) if s.contains("file_path")
        ));
        assert!(matches!(
            parse_input(&json!({ "file_path": "a", "new_text": "y" })).unwrap_err(),
            ToolError::InvalidInput(ref s) if s.contains("old_text")
        ));
        assert!(matches!(
            parse_input(&json!({ "file_path": "a", "old_text": "x" })).unwrap_err(),
            ToolError::InvalidInput(ref s) if s.contains("new_text")
        ));
    }

    #[test]
    fn parse_input_empty_old_text_is_invalid_input() {
        let input = json!({ "file_path": "a.rs", "old_text": "", "new_text": "y" });
        assert!(matches!(
            parse_input(&input).unwrap_err(),
            ToolError::InvalidInput(ref s) if s.contains("empty")
        ));
    }

    #[test]
    fn parse_input_url_file_path_is_invalid_input() {
        let input = json!({
            "file_path": "https://example.com/x",
            "old_text": "a",
            "new_text": "b"
        });
        assert!(matches!(
            parse_input(&input).unwrap_err(),
            ToolError::InvalidInput(ref s) if s.contains("WebFetch")
        ));
    }

    #[test]
    fn resolve_path_relative_joins_cwd() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve_path("sub/a.rs", cwd),
            PathBuf::from("/work/sub/a.rs")
        );
    }

    #[test]
    fn resolve_path_absolute_used_as_is() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path("/abs/a.rs", cwd), PathBuf::from("/abs/a.rs"));
    }

    #[tokio::test]
    async fn read_existing_missing_file_is_file_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("nope.txt");
        let err = read_existing(&missing, "nope.txt").await.unwrap_err();
        assert!(
            matches!(err, ToolError::FileNotFound(ref s) if s == "nope.txt"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn read_existing_non_utf8_is_execution_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("bin.dat");
        // Invalid UTF-8 (lone continuation byte) — read_to_string rejects it.
        std::fs::write(&target, b"\xFF\xFE\x00").unwrap();
        let err = read_existing(&target, "bin.dat").await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "{err:?}");
    }

    #[tokio::test]
    async fn read_existing_returns_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("ok.txt");
        std::fs::write(&target, "hello\n").unwrap();
        let content = read_existing(&target, "ok.txt").await.unwrap();
        assert_eq!(content, "hello\n");
    }

    #[test]
    fn apply_edit_unique_returns_spliced_content() {
        assert_eq!(
            apply_edit("hello world", "world", "rust").unwrap(),
            "hello rust"
        );
    }

    #[test]
    fn apply_edit_not_found_returns_structured_error() {
        // Structured: the caller formats it, not the helper.
        assert_eq!(apply_edit("abc", "zzz", "y"), Err(EditError::NotFound));
    }

    #[test]
    fn apply_edit_ambiguous_returns_structured_error_with_count() {
        // The count is carried as data, not pre-stringified.
        assert_eq!(
            apply_edit("dup dup", "dup", "x"),
            Err(EditError::Ambiguous { count: 2 })
        );
    }

    #[test]
    fn edit_error_not_found_formats_to_soft_output() {
        let out = EditError::NotFound.into_output();
        assert!(out.is_error);
        let text = out.text_content();
        assert!(text.contains("not found"), "{text}");
        assert!(text.contains("Grep") || text.contains("Read"), "{text}");
    }

    #[test]
    fn edit_error_ambiguous_formats_to_soft_output_with_count() {
        let out = EditError::Ambiguous { count: 3 }.into_output();
        assert!(out.is_error);
        let text = out.text_content();
        assert!(text.contains('3'), "{text}");
        assert!(text.contains("MultiEdit"), "{text}");
    }

    #[test]
    fn check_linter_valid_content_passes() {
        let path = Path::new("a.rs");
        assert!(check_linter(path, "fn main() {}\n").is_ok());
    }

    #[test]
    fn check_linter_invalid_content_returns_failure_result() {
        let path = Path::new("a.rs");
        // The structured LinterResult is carried on the Err side.
        let result = check_linter(path, "fn main() { let x = ; }").unwrap_err();
        assert!(!result.is_valid);
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn check_linter_unsupported_extension_passes() {
        // .txt is not a linted extension — passes regardless of content.
        let path = Path::new("a.txt");
        assert!(check_linter(path, "garbage {{{ not code").is_ok());
    }
}
