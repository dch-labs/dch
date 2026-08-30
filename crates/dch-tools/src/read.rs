//! The Read file tool — reads a file from disk with line/byte truncation.

use std::fmt::Write;

use tokio::io::AsyncReadExt;

use loopctl::Tool;
use loopctl::message::ImageSource;
use loopctl::message::ToolContent;
use loopctl::message::ToolContentPart;
use loopctl::tool::DisplayHint;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::context::RunnerContext;
use crate::context::require_cwd;
use crate::context::runner_ctx;
use crate::input::get_usize;
use crate::util::mime_type_from_path;
use crate::util::reject_url;
use crate::util::resolve_path;
use crate::walk;

/// Maximum number of lines returned by the Read tool.
pub const MAX_FILE_READ_LINES: usize = 200;

/// Maximum file size before we refuse to read entirely.
pub const MAX_FILE_SIZE_BYTES: usize = 10 * 1024 * 1024;

/// Maximum bytes of content returned (~100K tokens); guards long-line files.
const MAX_FILE_READ_BYTES: usize = 400_000;

/// Default limit when `offset` is provided but `limit` is not.
const DEFAULT_OFFSET_LIMIT: usize = 200;

/// Input for the Read tool.
///
/// Reads a file from disk with line/byte truncation and returns its contents;
/// images come back as multipart blocks and binary or oversized files are
/// rejected with pointers to `FileViewer` and the search tools.
#[derive(Default, Deserialize, Serialize, Tool)]
#[tool(
    name = "Read",
    read_only,
    concurrency_safe,
    description = "Read the contents of a file (up to 200 lines). For larger files or to \
         continue reading past truncation, use FileViewer with offset/limit parameters."
)]
pub struct ReadInput {
    /// The path to the file to read.
    ///
    /// May be absolute or relative; relative paths are resolved against the
    /// runner's working directory. The path must name a regular file — a
    /// directory yields a soft error pointing at `Glob`/`Grep` — and URLs are
    /// rejected.
    file_path: String,

    /// Starting line number (1-indexed).
    ///
    /// Lines before this offset are skipped, and the output is prefixed with a
    /// marker telling the model earlier lines were omitted. Defaults to 1
    /// (read from the start); must be at least 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,

    /// Maximum number of lines to return (default 200).
    ///
    /// Counted from the offset; when the view extends past the end of the
    /// file, the output ends with a truncation marker naming the next offset
    /// to use. Values above the 200-line ceiling are clamped to it; like
    /// `offset`, zero is rejected as invalid input.
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,

    /// Line range to read, as an alternative to `offset`/`limit`.
    ///
    /// Supported formats: `'1-100'` (lines 1 to 100), `'50:'` (line 50 to the
    /// end), `':100'` (first 100 lines), or a single line number. Ignored when
    /// `offset` or `limit` is also specified — the explicit fields win.
    #[serde(skip_serializing_if = "Option::is_none")]
    line_range: Option<String>,
}

impl ReadInput {
    /// Serializes the typed input and delegates to `read_inner`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the input cannot be serialized back to JSON
    /// or when `read_inner` fails.
    async fn run(&self, input: Self, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let rc = runner_ctx(ctx).cloned();
        let value = serde_json::to_value(&input)
            .map_err(|e| ToolError::Execution(format!("serialize tool input: {e}")))?;
        self.read_inner(value, rc).await
    }

    /// Body of [`Tool::call`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] for a missing `file_path`, a URL, a missing
    /// `RunnerContext`, a file-system error, or invalid `offset`/`limit`/`line_range`.
    async fn read_inner(
        &self,
        input: Value,
        runner_context: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let file_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing file_path".to_string()))?;
        reject_url("Read", file_path)?;

        let cwd = require_cwd(runner_context)?;
        let full_path = resolve_path(file_path, &cwd)?;

        let metadata = metadata_or_not_found(&full_path, file_path).await?;
        if let Some(too_large) = too_large_if_over(metadata.len()) {
            return Ok(too_large);
        }

        if !metadata.is_file() {
            return Ok(ToolOutput::error_text(format!(
                "{file_path} is a directory, not a file. \
                 Use Glob or Grep to explore its contents."
            )));
        }

        let bytes = read_capped(&full_path).await?;
        if let Some(too_large) = too_large_if_over(bytes.len() as u64) {
            return Ok(too_large);
        }
        if let Some(mime) = mime_type_from_path(&full_path) {
            return Ok(image_output(mime, &bytes));
        }
        if walk::bytes_look_binary(&bytes) {
            return Ok(ToolOutput::error_text(format!(
                "File {file_path} appears to be binary. \
                 Use Grep or FileViewer to inspect specific content."
            )));
        }

        let content = decode_utf8(bytes)?;
        let (offset, limit) = resolve_range(&input)?;
        Ok(format_text(&content, file_path, offset, limit))
    }
}

/// Fetch the target's metadata, mapping absence to the Read tool's not-found
/// error.
///
/// The error names the file as the model wrote it and appends suggestions —
/// a Glob pattern built from the file's name, and a typo reminder — so a
/// misspelled path is recoverable on the next turn.
///
/// # Errors
///
/// Returns [`ToolError::FileNotFound`] when the path does not exist. Any other
/// metadata fault also maps to [`ToolError::FileNotFound`], preserving this
/// check's single-error shape.
async fn metadata_or_not_found(
    full_path: &std::path::Path,
    file_path: &str,
) -> Result<std::fs::Metadata, ToolError> {
    tokio::fs::metadata(full_path).await.map_err(|_| {
        let filename = full_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("*");
        ToolError::FileNotFound(format!(
            "{file_path}\n\nSuggestions:\n\
             - Use Glob with pattern '**/*{filename}*' to search for similar files\n\
             - Check the path for typos or incorrect casing"
        ))
    })
}

/// Encode `bytes` as a base64 multipart image block.
///
/// The image is returned as a single [`ToolContentPart::Image`] carrying the
/// resolved `mime` type, so the model consumes it as an image rather than as
/// raw bytes.
fn image_output(mime: &'static str, bytes: &[u8]) -> ToolOutput {
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    let source = ImageSource::new_base64(mime, b64);
    ToolOutput::success(ToolContent::from_multipart(vec![ToolContentPart::Image {
        source,
    }]))
}

/// Decode the file's bytes as UTF-8 text.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] carrying the underlying decode error when
/// the bytes are not valid UTF-8.
fn decode_utf8(bytes: Vec<u8>) -> Result<String, ToolError> {
    String::from_utf8(bytes)
        .map_err(|e| ToolError::Execution(format!("Failed to decode file as UTF-8: {e}")))
}

/// Build the "file too large" rejection when `byte_count` exceeds the cap.
///
/// Returns `Some(ToolOutput)` (a soft error pointing the model at `FileViewer`
/// or the search tools) when `byte_count > MAX_FILE_SIZE_BYTES`, else `None`.
/// The single source of the rejection message — used by the metadata-size
/// check and by the post-read check after [`read_capped`] (a file can grow
/// between the metadata check and the bounded read).
fn too_large_if_over(byte_count: u64) -> Option<ToolOutput> {
    if byte_count > MAX_FILE_SIZE_BYTES as u64 {
        Some(ToolOutput::error_text(format!(
            "File is too large to read ({byte_count} bytes). Use FileViewer for paginated reading, \
             or Grep/CodeSearch to find specific content."
        )))
    } else {
        None
    }
}

/// Read at most `MAX_FILE_SIZE_BYTES + 1` bytes from `path`.
///
/// Bounds the memory allocation so a file that grows past the cap between the
/// metadata check and the read cannot OOM. If the returned buffer has more than
/// `MAX_FILE_SIZE_BYTES` bytes, the caller rejects it via [`too_large_if_over`].
///
/// # Errors
///
/// Returns [`ToolError::Execution`] if the file cannot be opened or read.
async fn read_capped(path: &std::path::Path) -> Result<Vec<u8>, ToolError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to open file: {e}")))?;
    let cap = MAX_FILE_SIZE_BYTES.saturating_add(1);
    let mut buf = Vec::with_capacity(cap.min(8192));
    file.take(cap as u64)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to read file: {e}")))?;
    Ok(buf)
}

/// Resolve `(offset, limit)` from the input, honoring the documented
/// precedence.
///
/// Explicit `offset`/`limit` win; otherwise `line_range`; otherwise the full
/// file. Integer parsing goes through [`get_usize`], so a negative or
/// non-integer `offset`/`limit` is rejected loudly with the field named rather
/// than silently coerced to a default.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `offset` or `limit` is zero, when
/// either is not a valid non-negative integer (per [`get_usize`]), or when
/// `line_range` fails to parse.
fn resolve_range(input: &Value) -> Result<(usize, usize), ToolError> {
    let offset = get_usize(input, "offset")?;
    let limit = get_usize(input, "limit")?;

    if offset.is_some() || limit.is_some() {
        let offset = match offset {
            Some(0) => {
                return Err(ToolError::InvalidInput(
                    "offset must be at least 1, got 0".to_string(),
                ));
            }
            Some(n) => n,
            None => 1,
        };
        let limit = match limit {
            Some(0) => {
                return Err(ToolError::InvalidInput(
                    "limit must be at least 1, got 0".to_string(),
                ));
            }
            Some(n) => n,
            None => DEFAULT_OFFSET_LIMIT,
        };
        let limit = limit.min(MAX_FILE_READ_LINES);
        return Ok((offset, limit));
    }

    if let Some(range) = input.get("line_range").and_then(Value::as_str) {
        let (line_offset, line_limit) = parse_line_range(range).map_err(ToolError::InvalidInput)?;
        return Ok((line_offset, line_limit.min(MAX_FILE_READ_LINES)));
    }

    Ok((1, MAX_FILE_READ_LINES))
}

/// Apply line/byte truncation and produce the text [`ToolOutput`].
///
/// The file content is sliced to the `[offset, offset+limit)` line range, then
/// checked against two independent ceilings:
///
/// - **Line count** — at most [`MAX_FILE_READ_LINES`] lines. If the view extends
///   past the end of the file, a `[FILE TRUNCATED]` marker tells the caller how
///   many lines remain and what `offset` to use next.
/// - **Byte count** — at most [`MAX_FILE_READ_BYTES`] bytes of *joined output*.
///   If a single line is long enough to exceed the byte cap, the output is
///   truncated at a char boundary and a byte-oriented `[FILE TRUNCATED]`
///   marker is appended instead.
///
/// Three fast paths bypass the marker logic:
///
/// - `offset` beyond the file length → a one-line "beyond file length" message.
/// - The whole file fits (`offset == 1`, the view reaches the last line, and
///   the joined view is under the byte cap) → the raw content is returned
///   with no markers.
/// - A partial view that starts past line 1 → a `[Lines before offset N
///   omitted]` header precedes the content.
fn format_text(content: &str, file_path: &str, offset: usize, limit: usize) -> ToolOutput {
    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();

    if offset > total_lines {
        return ToolOutput::text(format!(
            "File: {file_path}\nOffset {offset} is beyond file length ({total_lines})"
        ));
    }

    let start_idx = offset.saturating_sub(1);
    let effective_end = offset
        .saturating_add(limit)
        .saturating_sub(1)
        .min(total_lines);
    let view_lines = all_lines.get(start_idx..effective_end).unwrap_or_default();
    let shown_content = view_lines.join("\n");

    if shown_content.len() > MAX_FILE_READ_BYTES {
        let original_size = shown_content.len();
        let mut cut = MAX_FILE_READ_BYTES;
        while !shown_content.is_char_boundary(cut) && cut > 0 {
            cut = cut.saturating_sub(1);
        }
        let truncated = shown_content.get(..cut).unwrap_or(&shown_content);
        return ToolOutput::text(format!(
            "{truncated}\n\n[FILE TRUNCATED: Showing first {} of {} bytes (~{} tokens). \
             File has long lines — use FileViewer for paginated reading.]",
            cut,
            original_size,
            original_size / 4
        ));
    }

    if offset == 1 && effective_end >= total_lines && content.len() <= MAX_FILE_READ_BYTES {
        return ToolOutput::text(content.to_string()).with_hint(DisplayHint::Suppress);
    }

    let mut output = String::new();
    if offset > 1 {
        write!(
            output,
            "[Lines before offset {offset} omitted — use offset=1 to read from the start]\n\n"
        )
        .ok();
    }

    output.push_str(&shown_content);
    if effective_end < total_lines {
        let remaining = total_lines.saturating_sub(effective_end);
        let next_offset = effective_end.saturating_add(1);
        write!(
            output,
            "\n\n[FILE TRUNCATED: Showing lines {offset}-{effective_end} of {total_lines}. \
             Use FileViewer with offset={next_offset} to see the remaining {remaining} lines.]"
        )
        .ok();
    } else if offset > 1 {
        write!(
            output,
            "\n\n[Showing lines {offset}-{effective_end} of {total_lines}]"
        )
        .ok();
    }
    ToolOutput::text(output).with_hint(DisplayHint::Suppress)
}

/// Parse a line range string into `(offset, limit)`.
///
/// Supported formats:
/// - `"1-100"` → lines 1 to 100 → offset=1, limit=100
/// - `"50:"`   → from line 50 to end → offset=50, limit=MAX
/// - `":100"`  → first 100 lines → offset=1, limit=100
/// - `"100"`   → line 100 only → offset=100, limit=1
///
/// # Errors
///
/// Returns a descriptive `String` for empty input, zero values, inverted
/// ranges, or unparseable tokens.
pub(crate) fn parse_line_range(range: &str) -> Result<(usize, usize), String> {
    let range = range.trim();
    if range.is_empty() {
        return Err("line_range cannot be empty".to_string());
    }
    if let Some(parsed) = range.split_once('-').map(parse_dash_range).transpose()? {
        return Ok(parsed);
    }
    if let Some(parsed) = range.split_once(':').map(parse_colon_range).transpose()? {
        return Ok(parsed);
    }
    parse_single_line(range)
}

/// Parse a dash-separated range like `"1-100"` into `(offset, limit)`.
///
/// # Errors
///
/// Returns a descriptive `String` if either side is unparseable, zero, or
/// the end precedes the start.
fn parse_dash_range((left, right): (&str, &str)) -> Result<(usize, usize), String> {
    let start: usize = if left.is_empty() {
        1
    } else {
        left.parse().map_err(|_| {
            format!("Invalid line_range start: '{left}'. Expected a positive integer.")
        })?
    };
    let end: usize = right
        .parse()
        .map_err(|_| format!("Invalid line_range end: '{right}'. Expected a positive integer."))?;
    if start == 0 {
        return Err("line_range start must be >= 1".to_string());
    }
    if end < start {
        return Err(format!("line_range end ({end}) must be >= start ({start})"));
    }
    let count = end.saturating_sub(start).saturating_add(1);
    Ok((start, count))
}

/// Parse a colon-separated range like `"50:"` or `":100"` into `(offset, limit)`.
///
/// An empty right side is open-ended (`"50:"` reads from line 50 to the end,
/// capped at [`MAX_FILE_READ_LINES`]); a present right side makes the range
/// inclusive (`"50:100"` covers lines 50 through 100). An empty left side
/// starts at line 1.
///
/// # Errors
///
/// Returns a descriptive `String` if both sides are empty, a side is
/// unparseable, or a value is zero.
fn parse_colon_range((left, right): (&str, &str)) -> Result<(usize, usize), String> {
    if left.is_empty() && right.is_empty() {
        return Err("line_range ':' requires at least one side. Use '1:' or ':100'.".to_string());
    }
    if left.is_empty() {
        let count: usize = right.parse().map_err(|_| {
            format!("Invalid line_range count: '{right}'. Expected a positive integer.")
        })?;
        if count == 0 {
            return Err("line_range count must be >= 1".to_string());
        }
        return Ok((1, count));
    }
    let start: usize = left
        .parse()
        .map_err(|_| format!("Invalid line_range start: '{left}'. Expected a positive integer."))?;
    if start == 0 {
        return Err("line_range start must be >= 1".to_string());
    }
    if right.is_empty() {
        return Ok((start, MAX_FILE_READ_LINES));
    }
    let end: usize = right
        .parse()
        .map_err(|_| format!("Invalid line_range end: '{right}'. Expected a positive integer."))?;
    if end == 0 {
        return Err("line_range end must be >= 1".to_string());
    }
    if end < start {
        return Err(format!("line_range end ({end}) must be >= start ({start})"));
    }
    Ok((start, end.saturating_sub(start).saturating_add(1)))
}

/// Parse a single line number like `"100"` into `(offset, limit)`.
///
/// # Errors
///
/// Returns a descriptive `String` if the token is unparseable or zero.
fn parse_single_line(range: &str) -> Result<(usize, usize), String> {
    let line: usize = range.parse().map_err(|_| {
        format!(
            "Invalid line_range: '{range}'. \
             Supported formats: '1-100', '50:', ':100', or a single line number."
        )
    })?;
    if line == 0 {
        return Err("line_range must be >= 1".to_string());
    }
    Ok((line, 1))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::format_collect,
    clippy::format_push_string,
    clippy::redundant_closure_for_method_calls,
    clippy::field_reassign_with_default
)]
mod tests {
    use super::*;
    use crate::context::RunnerContext;
    use loopctl::tool::ToolContext;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Builds a `ToolContext` with a `RunnerContext` pointing at `cwd`.
    fn ctx_in(cwd: &str) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = cwd.to_string();
        let rc = RunnerContext {
            cwd: PathBuf::from(cwd),
            todos: Arc::new(Mutex::new(Vec::new())),
            question_tx: Arc::new(Mutex::new(None)),
        };
        ctx.set_extension(rc);
        ctx
    }

    async fn read(input: Value, cwd: &str) -> Result<ToolOutput, ToolError> {
        let tool = ReadInput::default();
        let ctx = ctx_in(cwd);
        tool.call(input, &ctx).await
    }

    fn input(path: &str) -> Value {
        json!({ "file_path": path })
    }

    #[tokio::test]
    async fn test_read_small_file_returns_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("small.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(path.to_str().unwrap()), cwd).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(out.text_content(), "hello world\n");
    }

    #[tokio::test]
    async fn test_read_truncates_at_max_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("big.txt");
        let content: String = (0..250).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(path.to_str().unwrap()), cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("FILE TRUNCATED"), "missing truncation marker");
        assert!(text.contains("lines 1-200 of 250"));
        assert!(text.contains("offset=201"));
        assert!(text.contains("line 199"));
        assert!(!text.contains("line 200"));
    }

    #[tokio::test]
    async fn test_read_exactly_max_lines_not_truncated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("exact.txt");
        let content: String = (0..200).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(path.to_str().unwrap()), cwd).await.unwrap();
        let text = out.text_content();
        assert!(!text.contains("FILE TRUNCATED"));
    }

    #[tokio::test]
    async fn test_read_rejects_oversized_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("huge.txt");
        let content = vec![b'x'; MAX_FILE_SIZE_BYTES + 1];
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(path.to_str().unwrap()), cwd).await.unwrap();
        assert!(out.is_error);
        let text = out.text_content();
        assert!(text.contains("too large"));
        assert!(text.contains("FileViewer"));
    }

    #[tokio::test]
    async fn test_read_missing_file_path_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let err = read(json!({}), cwd).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(ref s) if s.contains("file_path")));
    }

    #[tokio::test]
    async fn test_read_offset_returns_tail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("ten.txt");
        let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "offset": 5 });
        let out = read(input, cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("line 5"));
        assert!(text.contains("line 10"));
        assert!(!text.contains("line 4"));
        assert!(text.contains("omitted"));
    }

    #[tokio::test]
    async fn test_read_offset_and_limit_slice() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("ten.txt");
        let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "offset": 5, "limit": 3 });
        let out = read(input, cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("line 5"));
        assert!(text.contains("line 7"));
        assert!(!text.contains("line 4"));
        assert!(!text.contains("line 8"));
    }

    #[tokio::test]
    async fn test_read_offset_beyond_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("two.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "offset": 100 });
        let out = read(input, cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("beyond file length"));
    }

    #[tokio::test]
    async fn test_read_offset_zero_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("two.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "offset": 0 });
        let err = read(input, cwd).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(ref s) if s.contains("at least 1")));
    }

    #[tokio::test]
    async fn test_read_negative_offset_rejected_not_silently_defaulted() {
        // Regression: a negative offset must error loudly, not silently become 1.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("two.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "offset": -5 });
        let err = read(input, cwd).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("integer `-5`")),
            "negative offset should be rejected: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_read_negative_limit_rejected_not_silently_defaulted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("two.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "limit": -1 });
        let err = read(input, cwd).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("integer `-1`")),
            "negative limit should be rejected: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_read_limit_only_from_start() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("ten.txt");
        let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "limit": 5 });
        let out = read(input, cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("line 5"));
        assert!(!text.contains("line 6"));
    }

    #[tokio::test]
    async fn test_read_missing_file_returns_file_not_found_variant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let err = read(input("nonexistent.txt"), cwd).await.unwrap_err();
        assert!(matches!(err, ToolError::FileNotFound(_)));
    }

    #[tokio::test]
    async fn test_read_url_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let err = read(input("https://example.com/page"), cwd)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")));
    }

    #[tokio::test]
    async fn test_read_image_returns_multipart() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pic.png");
        let png = b"\x89PNG\r\n\x1a\n";
        std::fs::write(&path, png).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(path.to_str().unwrap()), cwd).await.unwrap();
        assert!(!out.is_error);
        assert!(matches!(out.payload, ToolContent::Multipart(_)));
    }

    #[tokio::test]
    async fn test_read_binary_returns_error_text() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("blob.dat");
        // NUL bytes in the leading region → binary sniff fires.
        std::fs::write(&path, b"\x00\x01\x02\x00\x04").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(path.to_str().unwrap()), cwd).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("binary"));
    }

    #[tokio::test]
    async fn test_read_directory_returns_error_text() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("subdir");
        std::fs::create_dir(&dir).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(dir.to_str().unwrap()), cwd).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("directory"));
    }

    #[tokio::test]
    async fn test_read_multibyte_truncation_with_offset() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("multibyte.txt");
        // Each line is 3 bytes (€) × 1000 = 3000 bytes per line.
        // A single line exceeds MAX_FILE_READ_BYTES only at >133 lines, so
        // build one very long line with multibyte chars and offset past line 1.
        let euro = "€".repeat(200_000); // 600_000 bytes, one line
        let content = format!("header\n{euro}\n");
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let input = json!({ "file_path": path.to_str().unwrap(), "offset": 2 });
        let out = read(input, cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("FILE TRUNCATED"), "should truncate: {text}");
        // Truncation must land on a char boundary of the shown content, not the
        // full file — the sliced output must be valid UTF-8 (it already is via
        // get(..cut), but the char-boundary check must use shown_content).
        assert!(text.len() < content.len());
    }

    #[tokio::test]
    async fn test_read_whole_file_fast_path_respects_byte_cap() {
        // A single-line file over MAX_FILE_READ_BYTES, read with no offset/limit,
        // takes the whole-file fast path. It must still hit the byte cap and
        // emit a truncation marker rather than returning the full wall of text.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("oneline.txt");
        let over_cap = "x".repeat(MAX_FILE_READ_BYTES + 1000);
        std::fs::write(&path, &over_cap).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let out = read(input(path.to_str().unwrap()), cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("FILE TRUNCATED"), "should truncate: {text}");
        assert!(text.len() < over_cap.len());
    }

    #[tokio::test]
    async fn test_read_with_line_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("twenty.txt");
        let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();

        let input = json!({ "file_path": path.to_str().unwrap(), "line_range": "5-7" });
        let out = read(input, cwd).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("line 7"));
        assert!(!text.contains("line 4"));
        assert!(!text.contains("line 8"));
    }

    #[test]
    fn test_readtool_schema_matches_spec() {
        let schema = ReadInput::default().schema();
        let input = schema.input_schema;
        let required = input.get("required").and_then(|v| v.as_array()).unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "file_path");
        let limit = input
            .pointer("/properties/limit/type")
            .and_then(|v| v.as_str());
        assert_eq!(limit, Some("integer"));
    }

    #[test]
    fn test_readtool_registered_in_builtin_registry() {
        let reg = crate::registry::builtin_registry();
        let tool = reg.get("Read").expect("Read registered");
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
    }

    #[test]
    fn test_parse_line_range_dash() {
        assert_eq!(parse_line_range("1-100").unwrap(), (1, 100));
        assert_eq!(parse_line_range("5-10").unwrap(), (5, 6));
        assert_eq!(parse_line_range("100-100").unwrap(), (100, 1));
    }

    #[test]
    fn test_parse_line_range_colon_open_end() {
        assert_eq!(parse_line_range("50:").unwrap(), (50, MAX_FILE_READ_LINES));
    }

    #[test]
    fn test_parse_line_range_colon_open_start() {
        assert_eq!(parse_line_range(":100").unwrap(), (1, 100));
        assert_eq!(parse_line_range(":1").unwrap(), (1, 1));
    }

    #[test]
    fn test_parse_line_range_colon_both_sides() {
        assert_eq!(parse_line_range("50:100").unwrap(), (50, 51));
        assert_eq!(parse_line_range("1:1").unwrap(), (1, 1));
        assert!(parse_line_range("10:5").is_err());
        assert!(parse_line_range("1:0").is_err());
    }

    #[test]
    fn test_parse_line_range_single_line() {
        assert_eq!(parse_line_range("42").unwrap(), (42, 1));
        assert_eq!(parse_line_range("1").unwrap(), (1, 1));
    }

    #[test]
    fn test_parse_line_range_whitespace() {
        assert_eq!(parse_line_range("  1-100  ").unwrap(), (1, 100));
        assert_eq!(
            parse_line_range(" 50: ").unwrap(),
            (50, MAX_FILE_READ_LINES)
        );
    }

    #[test]
    fn test_parse_line_range_errors() {
        assert!(parse_line_range("").is_err());
        assert!(parse_line_range("0").is_err());
        assert!(parse_line_range("0-5").is_err());
        assert!(parse_line_range("10-5").is_err());
        assert!(parse_line_range("abc").is_err());
        assert!(parse_line_range(":").is_err());
        assert!(parse_line_range(":0").is_err());
    }

    #[tokio::test]
    async fn read_capped_small_file_returns_all_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("small.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let bytes = read_capped(&path).await.unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[tokio::test]
    async fn read_capped_exactly_at_cap_returns_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("exact.bin");
        std::fs::write(&path, vec![b'x'; MAX_FILE_SIZE_BYTES]).unwrap();
        let bytes = read_capped(&path).await.unwrap();
        assert_eq!(bytes.len(), MAX_FILE_SIZE_BYTES);
    }

    #[tokio::test]
    async fn read_capped_over_cap_returns_cap_plus_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("over.bin");
        std::fs::write(&path, vec![b'x'; MAX_FILE_SIZE_BYTES + 100]).unwrap();
        let bytes = read_capped(&path).await.unwrap();
        assert_eq!(bytes.len(), MAX_FILE_SIZE_BYTES + 1);
    }

    #[tokio::test]
    async fn read_capped_empty_file_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("empty.txt");
        std::fs::write(&path, b"").unwrap();
        let bytes = read_capped(&path).await.unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn read_capped_missing_file_is_execution_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nope.txt");
        let err = read_capped(&path).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_through_in_workspace_symlink_to_outside_is_rejected() {
        // An in-workspace symlink whose target is outside cwd must be caught
        // by resolve_path's filesystem check, even though the link's own path
        // is lexically inside the workspace. Reading the link would otherwise
        // reach the external file.
        use std::os::unix::fs::symlink;
        let work = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "SHOULD NOT BE READ").unwrap();
        let link = work.path().join("link.txt");
        symlink(&secret, &link).unwrap();

        let cwd = work.path().to_str().unwrap();
        let input = json!({ "file_path": "link.txt" });
        let err = read(input, cwd).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("symlink")),
            "expected symlink-escape rejection, got {err:?}"
        );
    }
}
