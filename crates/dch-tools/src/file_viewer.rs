//! The `FileViewer` tool — paginated, token-efficient file viewing.
//!
//! The complement to [`ReadTool`](crate::ReadTool): where Read caps at ~200
//! lines for quick lookups, `FileViewer` navigates large files in chunks via
//! `page`/`page_size` (sequential) or `offset`/`limit` (direct seek), with a
//! header naming the current window and a `[Navigate: …]` hint.

use std::future::Future;
use std::path::Path;
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
use crate::util::is_url;
use crate::util::resolve_path;

/// Default number of lines returned per page.
const DEFAULT_PAGE_SIZE: usize = 100;

/// Maximum page size to prevent excessive token usage.
const MAX_PAGE_SIZE: usize = 500;

/// Which rendering mode the caller asked for.
///
/// Selected by the `output_format` parameter in the tool's input schema. The
/// enum is the seam that future syntax-highlighting work (post-v1, in the TUI
/// layer) would plug into — for v1, `Plain` and `Markdown` are fully wired and
/// `Colored` degrades to plain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OutputFormat {
    /// Plain text with line numbers.
    ///
    /// The default when `output_format` is omitted or set to `"plain"`. Each
    /// line is rendered as `{line_num:>6} │ {content}` with no decoration,
    /// decoration, or ANSI escapes. This is what the headless runner and the
    /// model itself consume; it must be solid and token-efficient.
    #[default]
    Plain,

    /// ANSI-colored output.
    ///
    /// Accepted when the caller passes `"colored"`, `"color"`, or `"ansi"`,
    /// but for v1 **degrades to plain** — no ANSI escape bytes are emitted.
    /// Real syntax highlighting lives in the TUI layer (via the theme system
    /// and tree-sitter captures), not in tool text output, where ANSI escapes
    /// waste model tokens. This variant exists so prompts that request it
    /// don't break; it will be wired post-v1 if a headless `--color` mode is
    /// wanted.
    Colored,

    /// Markdown-fenced output.
    ///
    /// Wraps the *entire* view window in a single fenced code block with a
    /// language tag from [`detect_language`] (e.g. ` ```rust\n…\n``` `). This
    /// is cheap (no parser dependency), useful for non-terminal consumers, and
    /// corrects the salvage's per-line fencing bug. Selected by `"markdown"`
    /// or `"md"`.
    Markdown,
}

impl OutputFormat {
    /// Parse the `output_format` parameter from the tool input.
    ///
    /// Matching is case-insensitive. Aliases are accepted so existing prompt
    /// templates and model behavior carry over without breakage:
    /// `"colored"`, `"color"`, and `"ansi"` all map to [`Colored`](Self::Colored);
    /// `"markdown"` and `"md"` map to [`Markdown`](Self::Markdown).
    ///
    /// Any unrecognized value (including the empty string) falls back to
    /// [`Plain`](Self::Plain) — the default and the only fully-realized format
    /// for v1.
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "colored" | "color" | "ansi" => Self::Colored,
            "markdown" | "md" => Self::Markdown,
            _ => Self::Plain,
        }
    }
}

/// Paginated, token-efficient file viewer.
///
/// Returns a window of lines (default 100) from a local file, prefixed with a
/// header naming the file + current window and a `[Navigate: …]` hint. Supports
/// two navigation modes: `page`/`page_size` (sequential) and `offset`/`limit`
/// (direct seek); `offset`+`limit` wins when both are given.
pub struct FileViewerTool;

impl Tool for FileViewerTool {
    fn name(&self) -> &'static str {
        "FileViewer"
    }

    fn description(&self) -> &'static str {
        "View a file with pagination. Shows 100 lines per page by default. \
         Use page parameter for sequential access or offset/limit for direct \
         line access."
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
                        "description": "The path to the file to view"
                    },
                    "page": {
                        "type": "integer",
                        "description": "Page number (1-indexed)",
                        "default": 1,
                        "minimum": 1
                    },
                    "page_size": {
                        "type": "integer",
                        "description": "Number of lines per page",
                        "default": 100,
                        "minimum": 1,
                        "maximum": MAX_PAGE_SIZE
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Starting line number (1-indexed, alternative to page)",
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (alternative to page_size)",
                        "minimum": 1,
                        "maximum": MAX_PAGE_SIZE
                    },
                    "output_format": {
                        "type": "string",
                        "description": "Output format: 'plain' (default), 'colored', or 'markdown'",
                        "enum": ["plain", "colored", "color", "ansi", "markdown", "md"]
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
        Box::pin(self.view_inner(input, rc))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

impl FileViewerTool {
    /// Body of [`Tool::call`].
    ///
    /// Orchestrates parse → resolve → read → bounds → render. Recoverable
    /// conditions (missing file, offset/page beyond EOF) are soft
    /// [`ToolOutput`] errors; bad args and OS faults become [`ToolError`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for a missing `file_path`, a URL
    /// `file_path`, `page=0`, or `offset=0`. Returns [`ToolError::Execution`]
    /// on a genuine I/O fault or a missing [`RunnerContext`].
    async fn view_inner(
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

        let Some(content) = read_content(&full_path).await? else {
            return Ok(ToolOutput::error_text(format!(
                "File not found: {}",
                parsed.file_path
            )));
        };
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let bounds = calculate_bounds(&input, total_lines)?;

        if total_lines == 0 {
            return Ok(ToolOutput::text(bounds.format_header(parsed.file_path)));
        }

        if bounds.start > total_lines {
            return Ok(ToolOutput::error_text(format!(
                "File: {}\nOffset {} is beyond file length ({})",
                parsed.file_path, bounds.start, total_lines
            )));
        }

        let view_lines = lines
            .get(bounds.start.saturating_sub(1)..bounds.end)
            .unwrap_or(&[]);

        let output = render_output(parsed.file_path, &bounds, view_lines, parsed.output_format);
        Ok(ToolOutput::text(output))
    }
}

/// Parsed and validated `FileViewer` input.
///
/// Produced by [`parse_input`] from the raw JSON the model sends. The
/// `file_path` is validated (present, not a URL) before this struct is
/// constructed; resolution against `cwd` happens later in
/// [`view_inner`](FileViewerTool::view_inner) via the shared
/// [`resolve_path`](crate::util::resolve_path).
struct ParsedInput<'a> {
    /// The file path exactly as supplied by the caller, before cwd resolution.
    ///
    /// Borrowed from the input JSON (`'a`) — no allocation. Kept in its raw
    /// form so headers, error messages, and diff output show the path the
    /// model named, not the resolved absolute path.
    file_path: &'a str,

    /// The requested rendering mode, parsed from the `output_format` parameter.
    ///
    /// Defaults to [`OutputFormat::Plain`] when the parameter is absent or
    /// unrecognized. Determines whether the body is rendered as plain numbered
    /// lines, a single markdown fenced block, or (degraded) plain for v1.
    output_format: OutputFormat,
}

/// Extract the file path and output format from the input, validating the
/// path is present and not a URL.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] for a missing `file_path` or a URL.
fn parse_input(input: &Value) -> Result<ParsedInput<'_>, ToolError> {
    let file_path = input
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("Missing file_path".to_string()))?;

    if is_url(file_path) {
        return Err(ToolError::InvalidInput(
            "URLs are not supported by the FileViewer tool. Use WebFetch for URLs.".to_string(),
        ));
    }

    let output_format = input
        .get("output_format")
        .and_then(Value::as_str)
        .map(OutputFormat::from_str)
        .unwrap_or_default();

    Ok(ParsedInput {
        file_path,
        output_format,
    })
}

/// Read a file's contents.
///
/// Returns `None` when the file doesn't exist (the caller surfaces a soft
/// error); `Some(content)` on success. OS read faults become
/// [`ToolError::Execution`].
///
/// # Errors
///
/// Returns [`ToolError::Execution`] on any read fault other than "not found".
async fn read_content(full_path: &Path) -> Result<Option<String>, ToolError> {
    if !tokio::fs::try_exists(full_path)
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?
    {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(full_path)
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    Ok(Some(content))
}

/// Render the header + numbered body lines + navigation hint into a single
/// output string.
///
/// Builds the full tool output in three parts: the [`ViewBounds::format_header`]
/// two-line header, the numbered body lines (`{:>6} │ {content}`), and the
/// trailing [`ViewBounds::format_hint`] navigation hint (omitted when empty).
/// A blank line separates the header from the body for readability.
///
/// The `output_format` controls body wrapping: [`OutputFormat::Markdown`]
/// wraps the body in a single fenced block with a language tag from
/// [`detect_language`]; [`OutputFormat::Plain`] and [`OutputFormat::Colored`]
/// (which degrades to plain) emit the lines as-is.
fn render_output(
    file_path: &str,
    bounds: &ViewBounds,
    view_lines: &[&str],
    output_format: OutputFormat,
) -> String {
    let header = bounds.format_header(file_path);
    let hint = bounds.format_hint();

    let body: Vec<String> = view_lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let line_num = bounds.start.saturating_add(i);
            format!("{line_num:>6} │ {line}")
        })
        .collect();

    let mut output = Vec::new();
    output.push(header);
    output.push(String::new());

    match output_format {
        OutputFormat::Markdown => {
            let lang = detect_language(file_path);
            output.push(format!("```{lang}"));
            output.extend(body);
            output.push("```".to_string());
        }
        OutputFormat::Colored | OutputFormat::Plain => {
            output.extend(body);
        }
    }

    if !hint.is_empty() {
        output.push(hint);
    }

    output.join("\n")
}

/// A computed read window into a file.
///
/// Produced by [`calculate_bounds`] from the caller's `page`/`page_size` or
/// `offset`/`limit` input. All line numbers are 1-indexed and inclusive. The
/// `page`/`total_pages` fields are `Some` only in page-based mode; in
/// offset-based mode they are `None` and the header omits the `(Page n/N)`
/// annotation.
struct ViewBounds {
    /// First line number in the window (1-indexed, inclusive).
    ///
    /// Computed as `(page - 1) * page_size + 1` in page mode, or the raw
    /// `offset` value in offset mode. When `start > total`, the view is
    /// beyond EOF and the caller returns a soft over-seek error.
    start: usize,

    /// Last line number in the window (1-indexed, inclusive).
    ///
    /// Computed as `start + window_size - 1`. The actual rendered end may be
    /// clamped to `total` if the window extends past the file's last line.
    end: usize,

    /// Total number of lines in the file.
    ///
    /// Computed once from `content.lines().count()` and passed into
    /// [`calculate_bounds`]. Used for the header's `of T` annotation, the
    /// over-seek check, and the navigation hint's "is this the last window?"
    /// decision.
    total: usize,

    /// Page number when in page-based mode.
    ///
    /// `Some(page)` when the caller navigated via `page`/`page_size`; `None`
    /// when in offset-based mode. Drives the `(Page n/N)` header annotation
    /// and the `page=N+1` / `page=N-1` navigation hints.
    page: Option<usize>,

    /// Total page count when in page-based mode.
    ///
    /// `Some(total_pages)` alongside [`page`](Self::page); computed as
    /// `total_lines.div_ceil(page_size)`. Always paired with `page` — both
    /// are `Some` or both are `None`.
    total_pages: Option<usize>,
}

impl ViewBounds {
    /// Format the two-line header for the view window.
    ///
    /// In page-based mode: `File: <path> (Page n/N)\nLines a-b of T`.
    /// In offset-based mode: `File: <path>\nLines a-b of T` (no page info).
    ///
    /// The exact strings are ported verbatim from the salvage source — the
    /// model has learned to read this format, and T-SP/T-28 may parse it.
    /// Do not change the wording without a coordinated task.
    fn format_header(&self, file_path: &str) -> String {
        let page_info = if let (Some(page), Some(total_pages)) = (self.page, self.total_pages) {
            format!(" (Page {page}/{total_pages})")
        } else {
            String::new()
        };
        format!(
            "File: {file_path}{page_info}\nLines {}-{} of {}",
            self.start, self.end, self.total
        )
    }

    /// Format the trailing `[Navigate: …]` hint telling the model which
    /// parameter values retrieve the next or previous window.
    ///
    /// In page-based mode, the hint offers `page=N+1` (if not the last page)
    /// and `page=N-1` (if not the first). In both modes, `offset=1` is
    /// offered when the window doesn't start at line 1, and `offset=end+1`
    /// when it doesn't reach the last line — so the model can always jump to
    /// either end of the file regardless of which navigation mode it's in.
    ///
    /// Returns an empty string when the window spans the entire file (no
    /// navigation possible), so the caller can omit the hint entirely. The
    /// hint string format (`[Navigate: k=v | k=v]`) is ported verbatim from
    /// the salvage source.
    fn format_hint(&self) -> String {
        let mut hints = Vec::new();

        if let (Some(page), Some(total_pages)) = (self.page, self.total_pages) {
            if page < total_pages {
                hints.push(format!("page={}", page.saturating_add(1)));
            }
            if page > 1 {
                hints.push(format!("page={}", page.saturating_sub(1)));
            }
        }

        if self.start > 1 {
            hints.push("offset=1".to_string());
        }

        if self.end < self.total {
            let next_offset = self.end.saturating_add(1);
            hints.push(format!("offset={next_offset}"));
        }

        if hints.is_empty() {
            String::new()
        } else {
            format!("\n[Navigate: {}]", hints.join(" | "))
        }
    }
}

/// Compute the view window from the input parameters.
///
/// When `offset` is present, offset-mode is used (ignoring `page`/`page_size`).
/// Otherwise page-mode is used with `page` defaulting to 1 and `page_size` to
/// [`DEFAULT_PAGE_SIZE`]. Both `page_size` and `limit` are clamped to
/// [`MAX_PAGE_SIZE`].
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `page == 0`, `offset == 0`, or any
/// explicitly supplied numeric field is non-integer, negative, or out of range.
fn calculate_bounds(input: &Value, total_lines: usize) -> Result<ViewBounds, ToolError> {
    let page_size = json_usize_strict(input, "page_size")?
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .min(MAX_PAGE_SIZE);

    if input.get("offset").is_some() {
        bounds_from_offset(input, total_lines, page_size)
    } else {
        bounds_from_page(input, total_lines, page_size)
    }
}

/// Extract an optional `usize` integer field from JSON, rejecting malformed values.
///
/// Returns `Ok(None)` when the key is absent (caller applies a default).
/// Returns `Ok(Some(n))` when the key is present and is a valid non-negative
/// integer that fits in `usize`. Returns `Err(InvalidInput)` when the key is
/// present but is not an integer, is negative, or exceeds the platform's
/// `usize` range — so malformed input is caught rather than silently defaulted.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the key is present but the value
/// is not a valid non-negative integer.
fn json_usize_strict(input: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                ToolError::InvalidInput(format!("'{key}' must be a non-negative integer"))
            }),
        Some(_) => Err(ToolError::InvalidInput(format!(
            "'{key}' must be a non-negative integer"
        ))),
    }
}

/// Compute bounds in offset-based mode (`offset`+`limit`).
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `offset == 0` or any supplied
/// field is malformed.
fn bounds_from_offset(
    input: &Value,
    total_lines: usize,
    page_size: usize,
) -> Result<ViewBounds, ToolError> {
    let offset = json_usize_strict(input, "offset")?
        .ok_or_else(|| ToolError::InvalidInput("Missing offset value".to_string()))?;
    if offset == 0 {
        return Err(ToolError::InvalidInput(
            "Offset must be at least 1".to_string(),
        ));
    }

    let limit = json_usize_strict(input, "limit")?
        .unwrap_or(page_size)
        .min(MAX_PAGE_SIZE);

    Ok(ViewBounds {
        start: offset,
        end: offset
            .saturating_add(limit)
            .saturating_sub(1)
            .min(total_lines.max(1)),
        total: total_lines,
        page: None,
        total_pages: None,
    })
}

/// Compute bounds in page-based mode (`page`+`page_size`).
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `page == 0` or any supplied field
/// is malformed.
fn bounds_from_page(
    input: &Value,
    total_lines: usize,
    page_size: usize,
) -> Result<ViewBounds, ToolError> {
    let page = json_usize_strict(input, "page")?.unwrap_or(1);

    if page == 0 {
        return Err(ToolError::InvalidInput(
            "Page must be at least 1".to_string(),
        ));
    }

    let start = page
        .saturating_sub(1)
        .saturating_mul(page_size)
        .saturating_add(1);
    let end = start
        .saturating_add(page_size)
        .saturating_sub(1)
        .min(total_lines.max(1));
    let total_pages = if total_lines == 0 {
        1
    } else {
        total_lines.div_ceil(page_size.max(1))
    };

    Ok(ViewBounds {
        start,
        end,
        total: total_lines,
        page: Some(page),
        total_pages: Some(total_pages),
    })
}

/// Detect a language tag from the file's extension.
///
/// Used by [`OutputFormat::Markdown`] to tag the fenced code block (e.g. a
/// `.rs` file produces ` ```rust `). The mapping is a small inline extension
/// match — not a full language-detection heuristic. Extensions with no known
/// mapping return an empty string, which renders as a bare fence (` ``` `)
/// with no language hint.
///
/// This is separate from the TUI's `SyntaxTheme` tree-sitter capture system
/// (T-22); it exists only to make markdown-fenced tool output useful for
/// non-terminal consumers.
fn detect_language(file_path: &str) -> &'static str {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext.to_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "md" => "markdown",
        _ => "",
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default,
    clippy::indexing_slicing,
    clippy::format_collect
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

    #[test]
    fn bounds_page_one() {
        let input = json!({"page": 1, "page_size": 100});
        let b = calculate_bounds(&input, 1000).unwrap();
        assert_eq!(b.start, 1);
        assert_eq!(b.end, 100);
        assert_eq!(b.page, Some(1));
        assert_eq!(b.total_pages, Some(10));
    }

    #[test]
    fn bounds_page_two() {
        let input = json!({"page": 2, "page_size": 100});
        let b = calculate_bounds(&input, 1000).unwrap();
        assert_eq!(b.start, 101);
        assert_eq!(b.end, 200);
        assert_eq!(b.page, Some(2));
    }

    #[test]
    fn bounds_offset_mode() {
        let input = json!({"offset": 500, "limit": 50});
        let b = calculate_bounds(&input, 1000).unwrap();
        assert_eq!(b.start, 500);
        assert_eq!(b.end, 549);
        assert!(b.page.is_none());
    }

    #[test]
    fn bounds_default_empty_input() {
        let input = json!({});
        let b = calculate_bounds(&input, 1000).unwrap();
        assert_eq!(b.start, 1);
        assert_eq!(b.end, 100);
        assert_eq!(b.page, Some(1));
    }

    #[test]
    fn bounds_invalid_page_zero() {
        assert!(calculate_bounds(&json!({"page": 0}), 1000).is_err());
    }

    #[test]
    fn bounds_invalid_offset_zero() {
        assert!(calculate_bounds(&json!({"offset": 0}), 1000).is_err());
    }

    #[test]
    fn bounds_page_size_clamped() {
        let input = json!({"page": 1, "page_size": 1000});
        let b = calculate_bounds(&input, 10000).unwrap();
        assert_eq!(b.end - b.start + 1, MAX_PAGE_SIZE);
    }

    #[test]
    fn bounds_end_clamped_to_total_in_page_mode() {
        // 5-line file, page_size 100 → end should clamp to 5, not 100.
        let input = json!({"page": 1, "page_size": 100});
        let b = calculate_bounds(&input, 5).unwrap();
        assert_eq!(b.start, 1);
        assert_eq!(b.end, 5, "end must clamp to total_lines");
    }

    #[test]
    fn bounds_end_clamped_to_total_in_offset_mode() {
        // 10-line file, offset 8, limit 100 → end should clamp to 10, not 107.
        let input = json!({"offset": 8, "limit": 100});
        let b = calculate_bounds(&input, 10).unwrap();
        assert_eq!(b.start, 8);
        assert_eq!(b.end, 10, "end must clamp to total_lines");
    }

    #[test]
    fn bounds_end_clamped_on_partial_final_page() {
        // 150-line file, page 2 of 100 → lines 101-150, not 101-200.
        let input = json!({"page": 2, "page_size": 100});
        let b = calculate_bounds(&input, 150).unwrap();
        assert_eq!(b.start, 101);
        assert_eq!(b.end, 150, "partial final page must clamp to total");
    }

    #[test]
    fn bounds_end_clamped_on_empty_file() {
        // 0-line file → end should be 1 (max(1)), not 100.
        let input = json!({});
        let b = calculate_bounds(&input, 0).unwrap();
        assert_eq!(b.end, 1, "empty file end must be 1 (max(1))");
        assert_eq!(b.total_pages, Some(1), "empty file must show 1 page");
    }

    #[test]
    fn bounds_offset_precedence_over_page() {
        let input = json!({"page": 2, "page_size": 100, "offset": 500, "limit": 50});
        let b = calculate_bounds(&input, 1000).unwrap();
        assert_eq!(b.start, 500);
        assert_eq!(b.end, 549);
        assert!(b.page.is_none());
    }

    #[test]
    fn header_contains_page_and_lines() {
        let b = ViewBounds {
            start: 1,
            end: 100,
            total: 1000,
            page: Some(1),
            total_pages: Some(10),
        };
        let h = b.format_header("test.rs");
        assert!(h.contains("test.rs"), "{h}");
        assert!(h.contains("Page 1/10"), "{h}");
        assert!(h.contains("Lines 1-100 of 1000"), "{h}");
    }

    #[test]
    fn hint_first_page_offers_next() {
        let b = ViewBounds {
            start: 1,
            end: 100,
            total: 1000,
            page: Some(1),
            total_pages: Some(10),
        };
        let h = b.format_hint();
        assert!(h.contains("page=2"), "{h}");
        assert!(!h.contains("page=0"), "{h}");
    }

    #[test]
    fn hint_last_page_offers_prev() {
        let b = ViewBounds {
            start: 901,
            end: 1000,
            total: 1000,
            page: Some(10),
            total_pages: Some(10),
        };
        let h = b.format_hint();
        assert!(h.contains("page=9"), "{h}");
        assert!(!h.contains("page=11"), "{h}");
    }

    #[test]
    fn hint_offset_mode_offers_start_and_next() {
        let b = ViewBounds {
            start: 500,
            end: 600,
            total: 1000,
            page: None,
            total_pages: None,
        };
        let h = b.format_hint();
        assert!(h.contains("offset=1"), "{h}");
        assert!(h.contains("offset=601"), "{h}");
    }

    #[tokio::test]
    async fn happy_path_page_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f.txt");
        let content: String = (1..=250).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "f.txt"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("Page 1/3"), "{text}");
        assert!(text.contains("page=2"), "{text}");
        assert!(text.contains("line 1"), "{text}");
        assert!(text.contains("line 100"), "{text}");
        assert!(!text.contains("line 101"), "{text}");
    }

    #[tokio::test]
    async fn next_page() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f.txt");
        let content: String = (1..=250).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "f.txt", "page": 2});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("line 101"), "{text}");
        assert!(text.contains("line 200"), "{text}");
        assert!(!text.contains("line 201"), "{text}");
    }

    #[tokio::test]
    async fn offset_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f.txt");
        let content: String = (1..=250).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "f.txt", "offset": 150, "limit": 25});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("line 150"), "{text}");
        assert!(text.contains("line 174"), "{text}");
        assert!(!text.contains("line 175"), "{text}");
        assert!(text.contains("offset=175"), "{text}");
    }

    #[tokio::test]
    async fn offset_beyond_eof() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, "short\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "f.txt", "offset": 9999});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("beyond file length"),
            "{}",
            out.text_content()
        );
        assert!(out.text_content().contains("(1)"), "{}", out.text_content());
    }

    #[tokio::test]
    async fn missing_file_is_soft_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "nope.txt"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(
            out.text_content().contains("File not found"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn url_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "https://example.com/x"});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn relative_path_resolved_against_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("src");
        std::fs::create_dir(&nested).unwrap();
        let f = nested.join("lib.rs");
        std::fs::write(&f, "fn main() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "src/lib.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("fn main()"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn page_size_clamped_at_runtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("big.txt");
        let content: String = (1..=600).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "big.txt", "page_size": 10000});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("line 500"), "{text}");
        assert!(!text.contains("line 501"), "{text}");
    }

    #[tokio::test]
    async fn output_format_markdown_single_fence() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("code.rs");
        std::fs::write(&f, "fn main() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "code.rs", "output_format": "markdown"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("```rust"), "{text}");
        let fence_count = text.matches("```").count();
        assert_eq!(fence_count, 2, "exactly one fence pair, got {fence_count}");
    }

    #[tokio::test]
    async fn output_format_colored_no_ansi() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("code.rs");
        std::fs::write(&f, "fn main() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "code.rs", "output_format": "colored"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(!text.contains('\x1b'), "no ANSI escape bytes: {text:?}");
    }

    #[tokio::test]
    async fn empty_file_no_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("empty.txt");
        std::fs::write(&f, "").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "empty.txt"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Lines"), "{text}");
        assert!(text.contains("of 0"), "{text}");
    }

    #[test]
    fn trait_contract_and_registry() {
        let tool = FileViewerTool;
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("FileViewer").is_some(), "FileViewer registered");
    }

    #[test]
    fn detect_language_known_extensions() {
        assert_eq!(detect_language("a.rs"), "rust");
        assert_eq!(detect_language("a.py"), "python");
        assert_eq!(detect_language("a.ts"), "typescript");
        assert_eq!(detect_language("a.json"), "json");
    }

    #[test]
    fn detect_language_unknown_returns_empty() {
        assert_eq!(detect_language("a.unknownext"), "");
        assert_eq!(detect_language("Makefile"), "");
    }

    // ---- regression: malformed values rejected (not silently defaulted) ----

    #[test]
    fn bounds_reject_negative_page_size() {
        let input = json!({"page_size": -5});
        assert!(calculate_bounds(&input, 1000).is_err());
    }

    #[test]
    fn bounds_reject_non_integer_page() {
        let input = json!({"page": "abc"});
        assert!(calculate_bounds(&input, 1000).is_err());
    }

    #[test]
    fn bounds_reject_non_integer_offset() {
        let input = json!({"offset": true});
        assert!(calculate_bounds(&input, 1000).is_err());
    }

    #[test]
    fn bounds_reject_negative_limit() {
        let input = json!({"offset": 10, "limit": -1});
        assert!(calculate_bounds(&input, 1000).is_err());
    }

    // ---- regression: header reports clamped range + empty-file Page 1/1 ----

    #[tokio::test]
    async fn short_file_header_shows_clamped_end() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("short.txt");
        std::fs::write(&f, "a\nb\nc\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "short.txt"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Lines 1-3 of 3"), "{text}");
        assert!(!text.contains("Lines 1-100"), "{text}");
    }

    #[tokio::test]
    async fn empty_file_header_shows_page_one_of_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("empty.txt");
        std::fs::write(&f, "").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "empty.txt"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Page 1/1"), "{text}");
        assert!(!text.contains("Page 1/0"), "{text}");
    }

    #[tokio::test]
    async fn partial_final_page_header_correct() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f.txt");
        // 150 lines → 2 pages of 100: page 2 shows lines 101-150, not 101-200.
        let content: String = (1..=150).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, &content).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = FileViewerTool;
        let ctx = ctx_in(cwd);
        let input = json!({"file_path": "f.txt", "page": 2});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Lines 101-150 of 150"), "{text}");
        assert!(!text.contains("101-200"), "{text}");
    }
}
