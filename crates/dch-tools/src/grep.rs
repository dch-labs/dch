//! The `Grep` tool — regex content search across a directory tree.
//!
//! Walks a directory with the shared [`walk_files`](crate::walk_files) walker,
//! skipping binary files, reads each remaining file, and returns every line
//! that matches the user-supplied regex as a JSON object
//! `{file, line, content}`. Compiled patterns are cached process-globally
//! via the [`regex_cache`](crate::regex_cache) module so repeated calls with
//! the same pattern skip recompilation.

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use regex::Regex;
use serde_json::Value;
use serde_json::json;

use crate::context::RunnerContext;
use crate::context::runner_ctx;
use crate::regex_cache::get_or_compile;
use crate::regex_cache::get_or_compile_case_insensitive;
use crate::util::is_url;
use crate::util::resolve_path;

const DEFAULT_MAX_MATCHES: usize = 100;
const DEFAULT_MAX_RESULTS: usize = 1000;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Regex content-search tool.
///
/// Walks a directory (gitignore-aware), reads each non-binary file, and
/// returns matching lines as JSON `[{file, line, content}]`. Up to
/// `max_matches` lines are kept per file (default 100). An empty match set is
/// a success message, not an error.
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn description(&self) -> &'static str {
        "Search for a regex pattern in file contents within a directory. \
         Returns matching lines with file paths and line numbers."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (defaults to current working directory)"
                    },
                    "include_patterns": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "File patterns to include (e.g., ['*.rs', '*.json'])"
                    },
                    "exclude_patterns": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "File patterns to exclude (e.g., ['*.lock', 'target/*'])"
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Enable case-insensitive matching"
                    },
                    "max_matches": {
                        "type": "integer",
                        "description": "Maximum number of matches per file (default: 100)"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum total matches across all files (default: 1000)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let rc = runner_ctx(ctx).cloned();
        let temp_dir = PathBuf::from(ctx.temp_dir.clone());
        Box::pin(self.grep_inner(input, rc, temp_dir))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

impl GrepTool {
    /// Body of [`Tool::call`].
    ///
    /// Orchestrates parse → compile → walk → search → format. An empty match
    /// set is a success message; bad args and invalid patterns become
    /// [`ToolError`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for a missing `pattern`, a URL
    /// `path`, or a pattern the regex engine cannot parse. Returns
    /// [`ToolError::Execution`] when the [`RunnerContext`] extension is
    /// absent, the blocking task joins unsuccessfully, or `serde_json` cannot
    /// encode the result.
    async fn grep_inner(
        &self,
        input: Value,
        rc: Option<RunnerContext>,
        temp_dir: PathBuf,
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

        if is_url(&parsed.base_path) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the Grep tool. Use WebFetch for URLs.".to_string(),
            ));
        }

        let regex = compile_pattern(&parsed.pattern, parsed.case_insensitive)?;

        let base = resolve_path(&parsed.base_path, &cwd);
        let job = SearchJob {
            regex,
            include: parsed.include_patterns,
            exclude: parsed.exclude_patterns,
            max_matches: parsed.max_matches,
            max_results: parsed.max_results,
            base,
            pattern: parsed.pattern,
            temp_dir,
        };
        let out = tokio::task::spawn_blocking(move || job::run(&job)).await;
        let output =
            out.map_err(|e| ToolError::Execution(format!("Grep walk task failed: {e}")))?;
        Ok(output)
    }
}

/// Bundles everything the blocking task needs.
///
/// Constructed on the async side, moved into `spawn_blocking`, and consumed by
/// the `job::run` runner. Grouping the fields here keeps the call site readable
/// and avoids a long parameter list on the runner function.
struct SearchJob {
    /// The compiled regex to match against each line.
    ///
    /// Produced by [`compile_pattern`] before the job is built, so the blocking
    /// thread does no regex compilation. Cheap to move (the `Regex` is
    /// `Arc`-backed internally).
    regex: Regex,

    /// Filename-level glob filters forwarded to the walker.
    ///
    /// Empty means "no include filter" — every file the walker yields is read.
    /// Drawn from the tool's `include_patterns` input; each entry is matched
    /// with the walker's single-segment `*`/`?` matcher.
    include: Vec<String>,

    /// Filename-level glob exclusions forwarded to the walker.
    ///
    /// A file matching any entry here is skipped before it is read. Drawn from
    /// the tool's `exclude_patterns` input; matched with the same single-
    /// segment matcher as [`include`](Self::include).
    exclude: Vec<String>,

    /// Per-file match cap.
    ///
    /// Defaults to [`DEFAULT_MAX_MATCHES`] when the caller omits the field.
    /// Enforced inside [`search_file`]: once this many matches are collected
    /// from one file, scanning that file stops.
    max_matches: usize,

    /// Total match cap across all files.
    ///
    /// Defaults to [`DEFAULT_MAX_RESULTS`] when the caller omits the field.
    /// Enforced in `job::run`: once this many matches are collected overall,
    /// the walk stops. Composes with [`max_matches`](Self::max_matches).
    max_results: usize,

    /// The directory to search, resolved against the runner cwd.
    ///
    /// Absolute by the time it reaches the job — [`resolve_path`] was applied
    /// on the async side. Every walked file's path is stripped against this to
    /// produce the relative `file` field in the JSON output.
    base: PathBuf,

    /// The pattern string, exactly as the caller supplied it.
    ///
    /// Carried alongside the compiled [`regex`](Self::regex) only so the
    /// "No matches found for pattern: {pattern}" success message can echo it
    /// back. Not used for matching.
    pattern: String,

    /// Where to spill oversized results to a temp file.
    ///
    /// Read from the native `ToolContext::temp_dir` field. The directory is
    /// created on demand by the output helper if the result spills.
    temp_dir: PathBuf,
}

/// Runner body for the blocking thread.
mod job {
    use super::SearchJob;
    use super::search_file;
    use crate::output::MAX_INLINE_OUTPUT_BYTES;
    use crate::output::truncate_or_write_to_temp;
    use crate::walk;
    use loopctl::tool::ToolOutput;

    /// Walk `base`, scan each non-binary file, collect matches, format output.
    ///
    /// Returns a [`ToolOutput`] directly: success text on no matches, JSON on
    /// matches (spilled to a temp file if oversized). Never returns `Err` —
    /// every failure mode (binary file unreadable, etc.) is silently skipped
    /// by the walker and reader, matching the salvage contract.
    pub(super) fn run(job: &SearchJob) -> ToolOutput {
        let mut matches = Vec::new();
        for entry in walk::walk_files(&job.base, &job.include, &job.exclude) {
            if matches.len() >= job.max_results {
                break;
            }
            let path = entry.path();
            if walk::likely_binary(path) {
                continue;
            }
            if file_too_large(path) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let remaining = job.max_results.saturating_sub(matches.len());
            let per_file = job.max_matches.min(remaining);
            let file_matches = search_file(&job.regex, &content, path, &job.base, per_file);
            matches.extend(file_matches);
        }

        if matches.is_empty() {
            return ToolOutput::text(format!("No matches found for pattern: {}", job.pattern));
        }
        let json = match serde_json::to_string_pretty(&matches) {
            Ok(s) => s,
            Err(e) => {
                // Effectively unreachable for a Vec of JSON values, but keep
                // the failure path honest rather than unwrap-ing.
                return ToolOutput::text(format!("Failed to serialize results: {e}"));
            }
        };
        truncate_or_write_to_temp(json, "grep", &job.temp_dir, MAX_INLINE_OUTPUT_BYTES)
    }

    /// True when `path`'s metadata size exceeds `MAX_FILE_BYTES`.
    ///
    /// Guards `read_to_string` against a path-matched file large enough to
    /// OOM the tool. Metadata failures read as "not too large" so the caller
    /// still attempts the read and surfaces the real I/O error.
    fn file_too_large(path: &std::path::Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| m.len() > super::MAX_FILE_BYTES)
    }
}

/// Parsed and validated `Grep` input.
///
/// Produced by [`parse_input`] from the raw JSON the model sends. All fields
/// are owned (the values outlive the borrowed input JSON because they feed a
/// `spawn_blocking` task). Defaults are applied here, not at the call site:
/// `path` defaults to `"."`, `case_insensitive` to `false`, `max_matches` to
/// [`DEFAULT_MAX_MATCHES`]. Validation is up front — a missing `pattern`, a
/// malformed `max_matches`, or a non-array `include_patterns`/`exclude_patterns`
/// fails before this struct is built.
struct ParsedInput {
    /// The regex pattern, exactly as supplied by the caller.
    ///
    /// Copied verbatim from the input JSON — no normalization. Fed to
    /// [`compile_pattern`], which routes it through the shared regex cache.
    /// Also echoed back in the "No matches found for pattern: …" success
    /// message.
    pattern: String,

    /// The directory to search in, defaulted to `"."` when absent.
    ///
    /// May be relative; the caller resolves it against the runner cwd via
    /// [`resolve_path`] before walking. An absolute path is used as-is.
    /// Rejected earlier by [`parse_input`] if it is a URL.
    base_path: String,

    /// Whether to compile the pattern case-insensitively.
    ///
    /// When `true`, [`compile_pattern`] wraps the pattern with the `(?i)` flag
    /// (idempotently) before caching. Defaults to `false`.
    case_insensitive: bool,

    /// Per-file match cap.
    ///
    /// Defaults to [`DEFAULT_MAX_MATCHES`] when the caller omits the field or
    /// when it is absent. Enforced inside [`search_file`]: once this many
    /// matches are collected from one file, scanning that file stops.
    max_matches: usize,

    /// Total match cap across all files.
    ///
    /// Defaults to [`DEFAULT_MAX_RESULTS`] when the caller omits the field.
    /// Enforced in `job::run`: once this many matches are collected overall,
    /// the walk stops. Composes with [`max_matches`](Self::max_matches) — a
    /// single file is capped first, then the running total.
    max_results: usize,

    /// Filename-level glob filters forwarded to the walker.
    ///
    /// Empty means "no include filter." Drawn from the tool's
    /// `include_patterns` input array; non-string elements are dropped by
    /// [`parse_string_array`] before this struct is built.
    include_patterns: Vec<String>,

    /// Filename-level glob exclusions forwarded to the walker.
    ///
    /// A file matching any entry here is skipped before it is read. Drawn from
    /// the tool's `exclude_patterns` input array; non-string elements are
    /// dropped by [`parse_string_array`].
    exclude_patterns: Vec<String>,
}

/// Extract and validate the tool input.
///
/// `path` defaults to `"."`; the caller resolves it against the runner cwd.
/// `case_insensitive` defaults to `false`; `max_matches` defaults to
/// `DEFAULT_MAX_MATCHES`; `max_results` defaults to `DEFAULT_MAX_RESULTS`.
/// Non-integer or negative `max_matches`/`max_results` values are rejected
/// rather than silently coerced.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `pattern` is missing, when
/// `max_matches` or `max_results` is present but not a non-negative integer,
/// or when an array field contains non-string elements.
fn parse_input(input: &Value) -> Result<ParsedInput, ToolError> {
    let pattern = input
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("Missing 'pattern' field".to_string()))?
        .to_string();
    let base_path = input
        .get("path")
        .and_then(Value::as_str)
        .map_or_else(|| ".".to_string(), str::to_string);
    let case_insensitive = input
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_matches = json_usize_strict(input, "max_matches")?.unwrap_or(DEFAULT_MAX_MATCHES);
    let max_results = json_usize_strict(input, "max_results")?.unwrap_or(DEFAULT_MAX_RESULTS);
    let include_patterns = parse_string_array(input, "include_patterns")?;
    let exclude_patterns = parse_string_array(input, "exclude_patterns")?;
    Ok(ParsedInput {
        pattern,
        base_path,
        case_insensitive,
        max_matches,
        max_results,
        include_patterns,
        exclude_patterns,
    })
}

/// Compile `pattern` through the shared cache, case-sensitive or not.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] wrapping the regex engine's error if
/// the pattern fails to compile.
fn compile_pattern(pattern: &str, case_insensitive: bool) -> Result<Regex, ToolError> {
    let compiled = if case_insensitive {
        get_or_compile_case_insensitive(pattern)
    } else {
        get_or_compile(pattern)
    };
    compiled.map_err(|e| ToolError::InvalidInput(format!("Invalid regex pattern: {e}")))
}

/// Extract an optional `usize` field, rejecting malformed values loudly.
///
/// Returns `Ok(None)` when the key is absent (caller applies a default). Returns
/// `Ok(Some(n))` for a valid non-negative integer that fits in `usize`. Returns
/// `Err(InvalidInput)` when the key is present but is not a non-negative
/// integer — so `{"max_matches": -5}` or `{"max_matches": "abc"}` fail loudly
/// rather than silently defaulting.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the key is present but the value
/// is not a non-negative integer.
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

/// Extract a string-array field, ignoring non-string elements.
///
/// A present-but-non-array value is rejected; an array with non-string
/// elements drops those elements (matching the salvage's `filter_map` shape).
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the key is present but the value
/// is not a JSON array.
fn parse_string_array(input: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match input.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(arr)) => Ok(arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()),
        Some(_) => Err(ToolError::InvalidInput(format!(
            "'{key}' must be an array of strings"
        ))),
    }
}

/// Scan one file's content for regex matches; return up to `max_matches`.
///
/// Computes a path relative to `base_path`, enumerates `content.lines()` with
/// 1-indexed line numbers, and pushes a JSON object per match in the order
/// they appear. The per-file cap is enforced here — the caller collects across
/// files unbounded.
fn search_file(
    regex: &Regex,
    content: &str,
    file_path: &Path,
    base_path: &Path,
    max_matches: usize,
) -> Vec<Value> {
    let rel_path = file_path
        .strip_prefix(base_path)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or_else(|| file_path.to_str().unwrap_or("unknown"));
    let mut results = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        if results.len() >= max_matches {
            break;
        }
        if regex.is_match(line) {
            results.push(json!({
                "file": rel_path,
                "line": line_num.saturating_add(1),
                "content": line
            }));
        }
    }
    results
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default,
    clippy::indexing_slicing
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

    /// Build a `ToolContext` with a `RunnerContext` whose cwd points at `dir`,
    /// mirroring the harness used by the other tools' tests.
    fn ctx_in(dir: &Path) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = dir.to_string_lossy().into_owned();
        let rc = RunnerContext {
            cwd: PathBuf::from(dir),
            session_state: Arc::new(Mutex::new(SessionState::default())),
            question_tx: None,
            runtime: RuntimeConfig::default(),
        };
        ctx.set_extension(rc);
        ctx
    }

    fn write_file(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn parse_input_defaults() {
        let input = json!({"pattern": "foo"});
        let parsed = parse_input(&input).unwrap();
        assert_eq!(parsed.pattern, "foo");
        assert_eq!(parsed.base_path, ".");
        assert!(!parsed.case_insensitive);
        assert_eq!(parsed.max_matches, DEFAULT_MAX_MATCHES);
        assert!(parsed.include_patterns.is_empty());
        assert!(parsed.exclude_patterns.is_empty());
    }

    #[test]
    fn parse_input_reads_explicit_values() {
        let input = json!({
            "pattern": "foo",
            "path": "src",
            "case_insensitive": true,
            "max_matches": 5,
            "include_patterns": ["*.rs"],
            "exclude_patterns": ["*.test.rs"]
        });
        let parsed = parse_input(&input).unwrap();
        assert_eq!(parsed.base_path, "src");
        assert!(parsed.case_insensitive);
        assert_eq!(parsed.max_matches, 5);
        assert_eq!(parsed.include_patterns, vec!["*.rs".to_string()]);
        assert_eq!(parsed.exclude_patterns, vec!["*.test.rs".to_string()]);
    }

    #[test]
    fn parse_input_missing_pattern_errors() {
        assert!(parse_input(&json!({})).is_err());
    }

    #[test]
    fn parse_input_rejects_negative_max_matches() {
        assert!(parse_input(&json!({"pattern": "x", "max_matches": -5})).is_err());
    }

    #[test]
    fn parse_input_rejects_non_integer_max_matches() {
        assert!(parse_input(&json!({"pattern": "x", "max_matches": "abc"})).is_err());
    }

    #[test]
    fn parse_input_rejects_non_array_include_patterns() {
        assert!(parse_input(&json!({"pattern": "x", "include_patterns": "foo"})).is_err());
    }

    #[test]
    fn parse_input_drops_non_string_array_elements() {
        let input = json!({"pattern": "x", "include_patterns": ["*.rs", 42, true, "*.toml"]});
        let parsed = parse_input(&input).unwrap();
        assert_eq!(
            parsed.include_patterns,
            vec!["*.rs".to_string(), "*.toml".to_string()]
        );
    }

    #[test]
    fn compile_pattern_invalid_errors() {
        assert!(compile_pattern("(unclosed", false).is_err());
        assert!(compile_pattern("(unclosed", true).is_err());
    }

    #[test]
    fn search_file_finds_matches_in_line_order() {
        let regex = regex::Regex::new("foo").unwrap();
        let content = "foo bar\nnope\nfoo again\nbaz";
        let base = Path::new("/repo");
        let file = Path::new("/repo/a.rs");
        let results = search_file(&regex, content, file, base, 100);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["file"], "a.rs");
        assert_eq!(results[0]["line"], 1);
        assert_eq!(results[0]["content"], "foo bar");
        assert_eq!(results[1]["line"], 3);
        assert_eq!(results[1]["content"], "foo again");
    }

    #[test]
    fn search_file_caps_at_max_matches() {
        let regex = regex::Regex::new("x").unwrap();
        let content = "x\nx\nx\nx\nx";
        let base = Path::new("/repo");
        let file = Path::new("/repo/a.rs");
        let results = search_file(&regex, content, file, base, 2);
        assert_eq!(results.len(), 2, "per-file cap enforced");
    }

    #[test]
    fn search_file_zero_max_matches_returns_empty() {
        let regex = regex::Regex::new("x").unwrap();
        let content = "x\nx\nx";
        let base = Path::new("/repo");
        let file = Path::new("/repo/a.rs");
        let results = search_file(&regex, content, file, base, 0);
        assert!(results.is_empty(), "max_matches=0 must yield no results");
    }

    #[tokio::test]
    async fn happy_path_multifile() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "fn foo() {}\nconst X = 1;\n");
        write_file(tmp.path(), "b.txt", "foo bar\nbaz\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "foo"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed.len(), 2);
        let files: Vec<&str> = parsed.iter().map(|v| v["file"].as_str().unwrap()).collect();
        assert!(files.contains(&"a.rs"));
        assert!(files.contains(&"b.txt"));
    }

    #[tokio::test]
    async fn case_insensitive_off_by_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "fn foo() {}\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "FN"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.text_content().contains("No matches found"));
    }

    #[tokio::test]
    async fn case_insensitive_on_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "fn foo() {}\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "FN", "case_insensitive": true});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(out.text_content().contains("\"line\": 1"));
    }

    #[tokio::test]
    async fn include_patterns_restricts_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        write_file(tmp.path(), "b.txt", "foo\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "foo", "include_patterns": ["*.rs"]});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.txt"), "{text}");
    }

    #[tokio::test]
    async fn exclude_patterns_removes_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        write_file(tmp.path(), "b.txt", "foo\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "foo", "exclude_patterns": ["*.txt"]});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.txt"), "{text}");
    }

    #[tokio::test]
    async fn max_matches_caps_per_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body: String = (0..300).map(|_| "x\n".to_string()).collect();
        write_file(tmp.path(), "a.txt", &body);
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "x", "max_matches": 5});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed.len(), 5, "per-file cap at 5");
    }

    #[tokio::test]
    async fn no_matches_is_success_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "fn foo() {}\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "zzz_nomatch"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(
            out.text_content()
                .contains("No matches found for pattern: zzz_nomatch")
        );
    }

    #[tokio::test]
    async fn missing_pattern_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("pattern")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "(unclosed"});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("Invalid regex pattern")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn url_path_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "x", "path": "https://example.com/y"});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn binary_file_is_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Write a file with NUL bytes so likely_binary() returns true.
        let mut bytes = vec![b'\0'; 32];
        bytes.extend_from_slice(b"foo\n");
        std::fs::write(tmp.path().join("data.png"), &bytes).unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "foo", "include_patterns": ["*.png", "*.rs"]});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("data.png"), "binary file skipped: {text}");
    }

    #[tokio::test]
    async fn relative_path_resolved_against_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "foo", "path": "."});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.text_content().contains("a.rs"));
    }

    #[tokio::test]
    async fn gitignore_is_respected() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        write_file(tmp.path(), "ignored.rs", "foo\n");
        write_file(tmp.path(), "kept.rs", "foo\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "foo"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            !text.contains("ignored.rs"),
            "gitignored file absent: {text}"
        );
        assert!(text.contains("kept.rs"), "{text}");
    }

    #[tokio::test]
    async fn large_result_spills_to_temp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut body = String::new();
        for i in 0..5000 {
            use std::fmt::Write as _;
            writeln!(body, "match_long_line_{i}").ok();
        }
        write_file(tmp.path(), "big.txt", &body);
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "match_long_line", "max_matches": 5000});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("result too large"), "should spill: {text}");
        assert!(text.contains("Full output written to:"), "{text}");
        assert!(text.contains("Use FileViewer"), "{text}");
    }

    #[test]
    fn trait_contract_and_registry() {
        let tool = GrepTool;
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.name(), "Grep");
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("Grep").is_some(), "Grep registered");
    }

    #[tokio::test]
    async fn max_results_caps_total_across_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "x\nx\nx\n");
        write_file(tmp.path(), "b.rs", "x\nx\nx\n");
        write_file(tmp.path(), "c.rs", "x\nx\nx\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "x", "max_matches": 100, "max_results": 4});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed.len(), 4, "total cap stops the walk at 4");
    }

    #[tokio::test]
    async fn max_results_composes_with_per_file_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "x\nx\nx\n");
        write_file(tmp.path(), "b.rs", "x\nx\nx\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "x", "max_matches": 2, "max_results": 3});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed.len(), 3, "total cap stops at 3");
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for v in &parsed {
            let file = v["file"].as_str().unwrap_or("").to_string();
            *counts.entry(file).or_insert(0) += 1;
        }
        let per_file_max = counts.values().copied().max().unwrap_or(0);
        assert!(
            per_file_max <= 2,
            "no file exceeded the per-file cap: max was {per_file_max}"
        );
    }

    #[tokio::test]
    async fn oversized_file_is_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let big: String = "x".repeat(usize::try_from(MAX_FILE_BYTES + 1).unwrap());
        std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
        write_file(tmp.path(), "small.txt", "x\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "x"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(!text.contains("big.txt"), "oversized file skipped: {text}");
        assert!(text.contains("small.txt"), "{text}");
    }
}
