//! The `Grep` tool — regex content search across a directory tree.
//!
//! Walks a directory with the shared [`walk_files`](crate::walk::walk_files)
//! walker, skipping binary files, reads each remaining file, and returns every
//! line that matches the user-supplied regex as a JSON object
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
use crate::input::get_usize;
use crate::output::MAX_INLINE_OUTPUT_BYTES;
use crate::output::session_temp_dir;
use crate::output::truncate_or_write_to_temp;
use crate::search::Match;
use crate::search::SearchJob;
use crate::search::compile_pattern;
use crate::search::no_matches_message;
use crate::util::is_url;
use crate::util::resolve_path;

/// Default per-file match cap when the caller omits `max_matches`.
const DEFAULT_MAX_MATCHES: usize = 100;
/// Default total match cap across all files when the caller omits `max_results`.
const DEFAULT_MAX_RESULTS: usize = 1000;
/// Hard ceiling `max_results` is clamped to, regardless of what the caller asks.
const MAX_RESULTS_CAP: usize = 1000;

/// Regex content-search tool — the "show me every matching line" search.
///
/// Walks a directory with the shared gitignore-aware walker, reads each
/// non-binary file, and returns every line matching the user-supplied regex
/// as a JSON object `{file, line, content}`. The full result is a
/// pretty-printed JSON array; when it exceeds the inline-output limit it
/// spills to a temp file with a preview and a pointer to the file-viewer
/// tool, so a large search cannot blow out the model's context window.
///
/// Two caps compose: `max_matches` bounds the matches kept **per file**
/// (default 100), and `max_results` bounds the total **across all files**
/// (default 1000). The per-file cap stops one huge file from saturating the
/// result before the walker moves on; the global cap bounds the overall
/// output regardless of how many files match.
///
/// Compiled patterns are cached process-globally via the shared
/// [`regex_cache`](crate::regex_cache) module, so a pattern compiled by this
/// tool is a cache hit when `CodeSearch` (or a later `Grep` call) compiles
/// the same pattern. An empty match set is a successful "No matches found"
/// message, not an error — the model uses it as a signal to broaden or
/// refine the pattern.
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
        let temp_dir = session_temp_dir(Path::new(&ctx.temp_dir), ctx.session_id);
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
    /// Orchestrates parse → compile → walk → render. An empty match set is a
    /// success message; bad args and invalid patterns become [`ToolError`].
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

        let parsed_input = crate::search::parse_input(&input, DEFAULT_MAX_RESULTS)?;
        let max_results = parsed_input.max_results.min(MAX_RESULTS_CAP);
        let max_matches = get_usize(&input, "max_matches")?
            .unwrap_or(DEFAULT_MAX_MATCHES)
            .max(1);

        if is_url(&parsed_input.base_path) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the Grep tool. Use WebFetch for URLs.".to_string(),
            ));
        }

        let regex = compile_pattern(&parsed_input.pattern, parsed_input.case_insensitive)?;
        let base = resolve_path(&parsed_input.base_path, &cwd);
        let job = SearchJob {
            regex,
            include: parsed_input.include_patterns,
            exclude: parsed_input.exclude_patterns,
            max_results,
            per_file_cap: Some(max_matches),
            base,
            pattern: parsed_input.pattern.clone(),
        };
        let matches = tokio::task::spawn_blocking(move || {
            crate::search::run(&job, |regex, content, file_path, base, limit| {
                scan_file(regex, content, file_path, base, limit)
            })
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Grep walk task failed: {e}")))?;

        if matches.is_empty() {
            return Ok(no_matches_message(&parsed_input.pattern));
        }

        Ok(render(&matches, &temp_dir))
    }
}

/// Scan one file's content for regex matches; return up to `limit`.
///
/// Computes a path relative to `base_path`, enumerates `content.lines()` with
/// 1-indexed line numbers, and pushes one [`Match`] per matching line, in
/// order, stopping once `limit` matches are collected.
fn scan_file(
    regex: &Regex,
    content: &str,
    file_path: &Path,
    base_path: &Path,
    limit: usize,
) -> Vec<Match> {
    let rel_path = crate::search::relative_file(file_path, base_path);
    let mut results = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        if results.len() >= limit {
            break;
        }
        if regex.is_match(line) {
            results.push(Match {
                file: rel_path.clone(),
                line: line_num.saturating_add(1),
                content: line.to_string(),
            });
        }
    }
    results
}

/// Render `matches` as a pretty JSON array, spilling to a temp file if oversized.
///
/// Maps each [`Match`] to a JSON object `{"file", "line", "content"}` in
/// collection order, serializes the lot with `serde_json::to_string_pretty`,
/// and hands the string to [`truncate_or_write_to_temp`]. The key order
/// (`file`, `line`, `content`) is the contract the model expects and what
/// `CodeSearch`'s terse format derives from — do not reorder.
///
/// Serialization failure is effectively unreachable for a `Vec` of JSON
/// values but is handled with a text-result fallback rather than `unwrap`,
/// so the function is total.
fn render(matches: &[Match], temp_dir: &Path) -> ToolOutput {
    let json_array: Vec<Value> = matches
        .iter()
        .map(|m| {
            json!({
                "file": m.file,
                "line": m.line,
                "content": m.content,
            })
        })
        .collect();
    let json = match serde_json::to_string_pretty(&json_array) {
        Ok(s) => s,
        Err(e) => return ToolOutput::text(format!("Failed to serialize results: {e}")),
    };
    truncate_or_write_to_temp(json, "grep", temp_dir, MAX_INLINE_OUTPUT_BYTES)
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
    use loopctl::tool::ToolContext;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

    fn ctx_in(dir: &Path) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = dir.to_string_lossy().into_owned();
        let rc = RunnerContext {
            cwd: PathBuf::from(dir),
            todos: Arc::new(Mutex::new(Vec::new())),
            question_tx: None,
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
    fn parse_defaults() {
        let common =
            crate::search::parse_input(&json!({"pattern": "foo"}), DEFAULT_MAX_RESULTS).unwrap();
        assert_eq!(common.pattern, "foo");
        assert_eq!(common.base_path, ".");
        assert!(!common.case_insensitive);
        assert_eq!(common.max_results, DEFAULT_MAX_RESULTS);
        assert!(common.include_patterns.is_empty());
        assert!(common.exclude_patterns.is_empty());
    }

    #[test]
    fn parse_explicit_values() {
        let input = json!({
            "pattern": "foo",
            "path": "src",
            "case_insensitive": true,
            "max_matches": 5,
            "max_results": 10,
            "include_patterns": ["*.rs"],
            "exclude_patterns": ["*.test.rs"]
        });
        let common = crate::search::parse_input(&input, DEFAULT_MAX_RESULTS).unwrap();
        assert_eq!(common.base_path, "src");
        assert!(common.case_insensitive);
        assert_eq!(common.max_results, 10);
        assert_eq!(common.include_patterns, vec!["*.rs".to_string()]);
        assert_eq!(common.exclude_patterns, vec!["*.test.rs".to_string()]);
        let max_matches = get_usize(&input, "max_matches")
            .unwrap()
            .unwrap_or(DEFAULT_MAX_MATCHES);
        assert_eq!(max_matches, 5);
    }

    #[test]
    fn parse_missing_pattern_errors() {
        assert!(crate::search::parse_input(&json!({}), DEFAULT_MAX_RESULTS).is_err());
    }

    #[test]
    fn parse_rejects_negative_max_matches() {
        assert!(get_usize(&json!({"pattern": "x", "max_matches": -5}), "max_matches").is_err());
    }

    #[test]
    fn parse_rejects_non_integer_max_matches() {
        assert!(
            get_usize(
                &json!({"pattern": "x", "max_matches": "abc"}),
                "max_matches"
            )
            .is_err()
        );
    }

    #[test]
    fn parse_rejects_non_array_include_patterns() {
        assert!(
            crate::search::parse_input(
                &json!({"pattern": "x", "include_patterns": "foo"}),
                DEFAULT_MAX_RESULTS,
            )
            .is_err()
        );
    }

    #[test]
    fn parse_drops_non_string_array_elements() {
        let input = json!({"pattern": "x", "include_patterns": ["*.rs", 42, true, "*.toml"]});
        let common = crate::search::parse_input(&input, DEFAULT_MAX_RESULTS).unwrap();
        assert_eq!(
            common.include_patterns,
            vec!["*.rs".to_string(), "*.toml".to_string()]
        );
    }

    #[test]
    fn compile_pattern_invalid_errors() {
        assert!(compile_pattern("(unclosed", false).is_err());
        assert!(compile_pattern("(unclosed", true).is_err());
    }

    #[test]
    fn scan_file_finds_matches_in_line_order() {
        let regex = regex::Regex::new("foo").unwrap();
        let content = "foo bar\nnope\nfoo again\nbaz";
        let base = Path::new("/repo");
        let file = Path::new("/repo/a.rs");
        let results = scan_file(&regex, content, file, base, 100);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].file, "a.rs");
        assert_eq!(results[0].line, 1);
        assert_eq!(results[0].content, "foo bar");
        assert_eq!(results[1].line, 3);
        assert_eq!(results[1].content, "foo again");
    }

    #[test]
    fn scan_file_caps_at_limit() {
        let regex = regex::Regex::new("x").unwrap();
        let content = "x\nx\nx\nx\nx";
        let base = Path::new("/repo");
        let file = Path::new("/repo/a.rs");
        let results = scan_file(&regex, content, file, base, 2);
        assert_eq!(results.len(), 2, "per-file cap enforced");
    }

    #[test]
    fn scan_file_zero_limit_returns_empty() {
        let regex = regex::Regex::new("x").unwrap();
        let content = "x\nx\nx";
        let base = Path::new("/repo");
        let file = Path::new("/repo/a.rs");
        let results = scan_file(&regex, content, file, base, 0);
        assert!(results.is_empty(), "limit=0 must yield no results");
    }

    #[test]
    fn render_emits_json_array_with_correct_keys() {
        let matches = vec![
            Match {
                file: "a.rs".into(),
                line: 1,
                content: "foo".into(),
            },
            Match {
                file: "b.rs".into(),
                line: 5,
                content: "bar".into(),
            },
        ];
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, tmp.path());
        assert!(!out.is_error);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["file"], "a.rs");
        assert_eq!(parsed[0]["line"], 1);
        assert_eq!(parsed[0]["content"], "foo");
        assert_eq!(parsed[1]["file"], "b.rs");
        assert_eq!(parsed[1]["line"], 5);
    }

    #[test]
    fn render_empty_input_returns_empty_json_array() {
        let matches: Vec<Match> = vec![];
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, tmp.path());
        let text = out.text_content();
        assert_eq!(text, "[]", "empty input → empty JSON array, not an error");
    }

    #[test]
    fn render_large_output_spills_to_temp() {
        let line: String = "x".repeat(400);
        let matches: Vec<Match> = (0..500)
            .map(|i| Match {
                file: format!("f{i}.rs"),
                line: i + 1,
                content: line.clone(),
            })
            .collect();
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, tmp.path());
        let text = out.text_content();
        assert!(text.contains("result too large"), "should spill: {text}");
        assert!(text.contains("Full output written to:"), "{text}");
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
    async fn zero_max_matches_clamped_to_one() {
        // Explicit zero should not zero-out all results; clamp to 1.
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "x\nx\nx\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "x", "max_matches": 0});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(
            parsed.len(),
            1,
            "zero max_matches should clamp to 1, got {}",
            parsed.len()
        );
    }

    #[tokio::test]
    async fn zero_max_results_clamped_to_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "x\n");
        write_file(tmp.path(), "b.rs", "x\n");
        let tool = GrepTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "x", "max_results": 0});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(
            parsed.len(),
            1,
            "zero max_results should clamp to 1, got {}",
            parsed.len()
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

    #[tokio::test]
    async fn oversized_file_is_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let big: String = "x".repeat(usize::try_from(crate::walk::MAX_FILE_BYTES + 1).unwrap());
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

    #[test]
    fn trait_contract_and_registry() {
        let tool = GrepTool;
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.name(), "Grep");
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("Grep").is_some(), "Grep registered");
    }
}
