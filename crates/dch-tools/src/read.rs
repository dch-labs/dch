//! The Read file tool — reads a file from disk with line/byte truncation.

use std::fmt::Write;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::SystemTime;

use loopctl::message::ImageSource;
use loopctl::message::ToolContent;
use loopctl::message::ToolContentPart;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;

use crate::context::RunnerContext;
use crate::context::runner_ctx;
use crate::state::FileReadEntry;
use crate::util::is_url;
use crate::util::mime_type_from_path;

/// Maximum number of lines returned by the Read tool.
pub const MAX_FILE_READ_LINES: usize = 200;
/// Maximum file size before we refuse to read entirely.
pub const MAX_FILE_SIZE_BYTES: usize = 10 * 1024 * 1024;
/// Maximum bytes of content returned (~100K tokens); guards long-line files.
const MAX_FILE_READ_BYTES: usize = 400_000;
/// Default limit when `offset` is provided but `limit` is not.
const DEFAULT_OFFSET_LIMIT: usize = 200;
/// Number of leading bytes to scan for NUL when detecting binary files.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Read the contents of a file (up to 200 lines).
///
/// For larger files or to continue reading past truncation, use `FileViewer`
/// with offset/limit parameters. Read-only and concurrency-safe.
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file (up to 200 lines). For larger files or to \
         continue reading past truncation, use FileViewer with offset/limit parameters."
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
                        "description": "The path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Starting line number (1-indexed). Lines before this offset are skipped."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200,
                        "description": "Maximum number of lines to return (default 200)."
                    },
                    "line_range": {
                        "type": "string",
                        "description": "Line range to read (alternative to offset/limit). \
                        Examples: '1-100', '50:', ':100'. Ignored if offset or limit are also specified."
                    }
                },
                "required": ["file_path"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let rc = runner_ctx(ctx).cloned();
        Box::pin(self.read_inner(input, rc))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

impl ReadTool {
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
        if is_url(file_path) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the Read tool. Use WebFetch for URLs.".to_string(),
            ));
        }

        let cwd = runner_context
            .clone()
            .ok_or_else(|| {
                ToolError::Execution(
                    "RunnerContext extension is not installed on the ToolContext".to_string(),
                )
            })?
            .cwd;
        let path = Path::new(file_path);
        let full_path = if path.is_relative() {
            cwd.join(path)
        } else {
            path.to_path_buf()
        };

        let metadata = tokio::fs::metadata(&full_path).await.map_err(|_| {
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("*");
            ToolError::FileNotFound(format!(
                "{file_path}\n\nSuggestions:\n\
                 - Use Glob with pattern '**/*{filename}*' to search for similar files\n\
                 - Check the path for typos or incorrect casing"
            ))
        })?;

        if metadata.len() > MAX_FILE_SIZE_BYTES as u64 {
            return Ok(ToolOutput::error_text(format!(
                "File is too large to read ({} bytes). Use FileViewer for paginated reading, \
                 or Grep/CodeSearch to find specific content.",
                metadata.len()
            )));
        }

        if !metadata.is_file() {
            return Ok(ToolOutput::error_text(format!(
                "{file_path} is a directory, not a file. \
                 Use Glob or Grep to explore its contents."
            )));
        }

        // Image branch: return a base64-encoded image block.
        if let Some(mime) = mime_type_from_path(&full_path) {
            let bytes = tokio::fs::read(&full_path).await?;
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
            let source = ImageSource::new_base64(mime, b64);
            return Ok(ToolOutput::success(ToolContent::from_multipart(vec![
                ToolContentPart::Image { source },
            ])));
        }

        // Binary sniff: read bytes, check for NUL in the leading region.
        let bytes = tokio::fs::read(&full_path).await?;
        let sniff_end = bytes.len().min(BINARY_SNIFF_BYTES);
        if bytes
            .get(..sniff_end)
            .is_some_and(|head| head.contains(&0u8))
        {
            return Ok(ToolOutput::error_text(format!(
                "File {file_path} appears to be binary. \
                 Use Grep or FileViewer to inspect specific content."
            )));
        }

        // The file is text; convert bytes to string.
        let content = String::from_utf8(bytes)
            .map_err(|e| ToolError::Execution(format!("Failed to decode file as UTF-8: {e}")))?;
        if let Some(rc) = &runner_context {
            // Record the read in session history.
            if let Ok(mut state) = rc.session_state.lock() {
                state.file_read_history.push(FileReadEntry {
                    path: file_path.to_string(),
                    read_at: SystemTime::now(),
                });
            }
        }

        // Resolve offset/limit/line_range precedence.
        let (offset, limit) = resolve_range(&input)?;
        Ok(format_text(&content, file_path, offset, limit))
    }
}

/// Resolve `(offset, limit)` from the input, honoring the documented precedence:
/// explicit `offset`/`limit` win; otherwise `line_range`; otherwise full file.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `offset` or `limit` is zero, when
/// either exceeds `usize` on the target platform, or when `line_range` fails to
/// parse.
fn resolve_range(input: &Value) -> Result<(usize, usize), ToolError> {
    let offset = input.get("offset").and_then(Value::as_u64);
    let limit = input.get("limit").and_then(Value::as_u64);

    if offset.is_some() || limit.is_some() {
        let offset = match offset {
            Some(0) => {
                return Err(ToolError::InvalidInput(
                    "offset must be at least 1".to_string(),
                ));
            }
            Some(n) => usize::try_from(n)
                .map_err(|_| ToolError::InvalidInput("offset too large".to_string()))?,
            None => 1,
        };
        let limit = match limit {
            Some(0) => {
                return Err(ToolError::InvalidInput(
                    "limit must be at least 1".to_string(),
                ));
            }
            Some(n) => usize::try_from(n)
                .map_err(|_| ToolError::InvalidInput("limit too large".to_string()))?,
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
/// - The whole file fits (`offset == 1` and the view reaches the last line) →
///   the raw content is returned with no markers.
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

    // offset >= 1 is guaranteed by resolve_range; saturating ops keep clippy happy.
    let start_idx = offset.saturating_sub(1);
    let effective_end = offset
        .saturating_add(limit)
        .saturating_sub(1)
        .min(total_lines);
    let view_lines = all_lines.get(start_idx..effective_end).unwrap_or_default();
    let shown_content = view_lines.join("\n");

    // Byte cap on the shown view (guards very long single lines).
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

    // Whole-file fast path.
    if offset == 1 && effective_end >= total_lines {
        return ToolOutput::text(content.to_string());
    }

    // Partial view with before/after markers.
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
    ToolOutput::text(output)
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
    // Open-ended "50:" → to end; "50:100" → inclusive range.
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

    async fn read(input: Value, cwd: &str) -> Result<ToolOutput, ToolError> {
        let tool = ReadTool;
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
    async fn test_read_records_file_read_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("tracked.txt");
        std::fs::write(&path, "content\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();

        let tool = ReadTool;
        let ctx = ctx_in(cwd);
        tool.call(input(path.to_str().unwrap()), &ctx)
            .await
            .unwrap();

        let rc = runner_ctx(&ctx).unwrap();
        let state = rc.session_state.lock().unwrap();
        assert_eq!(state.file_read_history.len(), 1);
        assert_eq!(state.file_read_history[0].path, path.to_str().unwrap());
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
        let schema = ReadTool.schema();
        let input = schema.input_schema;
        let required = input.get("required").and_then(|v| v.as_array()).unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "file_path");
        let limit = input
            .pointer("/properties/limit/maximum")
            .and_then(|v| v.as_u64());
        assert_eq!(limit, Some(200));
    }

    #[test]
    fn test_readtool_registered_in_builtin_registry() {
        let reg = crate::registry::builtin_registry();
        let tool = reg.get("Read").expect("ReadTool registered");
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
}
