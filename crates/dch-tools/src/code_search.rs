//! The `CodeSearch` tool — token-efficient regex code search.
//!
//! Same regex + walker engine as [`Grep`](crate::GrepInput), but returns
//! grouped `file` headers with collapsed consecutive-line ranges by default
//! (no matched content), and only includes matched content when the caller
//! opts in via `context_lines > 0`. A global `max_results` cap (default 50,
//! clamp 200) bounds the total output regardless of how many matches one
//! file contains.

use std::path::Path;
use std::path::PathBuf;

use loopctl::Tool;
use loopctl::tool::DisplayHint;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::context::RunnerContext;
use crate::context::require_cwd;
use crate::context::runner_ctx;
use crate::input::get_usize;
use crate::output::MAX_INLINE_OUTPUT_BYTES;
use crate::output::truncate_or_write_to_temp;
use crate::search::Match;
use crate::search::SearchJob;
use crate::search::compile_pattern;
use crate::search::no_matches_message;
use crate::util::ResolvePolicy;
use crate::util::reject_url;
use crate::util::resolve_path;

/// Default total match cap across all files when the caller omits `max_results`.
const DEFAULT_MAX_RESULTS: usize = 50;

/// Hard ceiling `max_results` is clamped to, regardless of what the caller asks.
const RESULTS_CAP: usize = 200;

/// Hard ceiling `context_lines` is clamped to, regardless of what the caller asks.
const MAX_CONTEXT_LINES: usize = 5;

/// Input for the `CodeSearch` tool.
///
/// Token-efficient regex code search: same engine as `Grep`, but returns the
/// smallest useful answer — `file` headers with collapsed consecutive-line
/// ranges by default, and matched-content snippets only when
/// `context_lines > 0`.
#[derive(Default, Deserialize, Serialize, Tool)]
#[tool(
    name = "CodeSearch",
    read_only,
    concurrency_safe,
    description = "Search code with succinct, token-efficient results. Returns file:line \
         format by default. Use context_lines > 0 to include matched content."
)]
pub struct CodeSearchInput {
    /// The regular expression to search file contents for.
    ///
    /// Compiled with the shared [`compile_pattern`] helper, so an unparseable
    /// pattern is rejected as invalid input before any walking starts. Matched
    /// against whole lines.
    pattern: String,

    /// The directory to search in, defaulting to the current working directory.
    ///
    /// May be relative, in which case it is resolved against the runner's cwd,
    /// not the process's. URLs are rejected; the walk honors `.gitignore`.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,

    /// File patterns restricting the search to matching files (e.g., ['*.rs']).
    ///
    /// Applied in addition to the walker's `.gitignore` handling. Files that
    /// match no pattern are skipped entirely; binary files are always skipped.
    #[allow(clippy::doc_link_with_quotes)]
    #[serde(skip_serializing_if = "Option::is_none")]
    include_patterns: Option<Vec<String>>,

    /// File patterns removing files from the search (e.g., ['*.lock']).
    ///
    /// Takes precedence over `include_patterns`: a file matching both is
    /// excluded. Useful for pruning large generated directories such as
    /// `target/*`.
    #[allow(clippy::doc_link_with_quotes)]
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_patterns: Option<Vec<String>>,

    /// Whether to match the pattern case-insensitively.
    ///
    /// Defaults to `false` (case-sensitive). When `true`, the compiled regex
    /// has the case-insensitive flag set.
    #[serde(skip_serializing_if = "Option::is_none")]
    case_insensitive: Option<bool>,

    /// Lines of context shown around each match (default: 0, cap: 5).
    ///
    /// At 0 the output is the succinct form: file headers with collapsed line
    /// ranges and no content. Above 0, each match also renders a
    /// `±context_lines` snippet with `>` marking the matched line.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_lines: Option<usize>,

    /// Maximum total matches across all files (default: 50, cap: 200).
    ///
    /// Bounds the output regardless of how many matches a single file
    /// contains; the walk stops once the cap is reached. Requests above the
    /// hard cap are lowered to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_results: Option<usize>,
}

impl CodeSearchInput {
    /// Serializes the typed input and delegates to `code_search_inner`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the input cannot be serialized back to JSON
    /// or when `code_search_inner` fails.
    async fn run(&self, input: Self, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let rc = runner_ctx(ctx).cloned();
        let temp_dir = PathBuf::from(&ctx.temp_dir);
        let value = serde_json::to_value(&input)
            .map_err(|e| ToolError::Execution(format!("serialize tool input: {e}")))?;
        self.code_search_inner(value, rc, temp_dir).await
    }

    /// Body of [`Tool::call`].
    ///
    /// Orchestrates parse → compile → walk → render. An empty match set is a
    /// success message; bad args and invalid patterns become [`ToolError`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for a missing `pattern`, a URL
    /// `path`, a malformed `max_results`/`context_lines`, or a pattern the
    /// regex engine cannot parse. Returns [`ToolError::Execution`] when the
    /// [`RunnerContext`] extension is absent or the blocking task joins
    /// unsuccessfully.
    async fn code_search_inner(
        &self,
        input: Value,
        rc: Option<RunnerContext>,
        temp_dir: PathBuf,
    ) -> Result<ToolOutput, ToolError> {
        let policy = match rc.as_ref() {
            Some(context) => context.resolve_policy,
            None => ResolvePolicy::Contained,
        };
        let cwd = require_cwd(rc)?;

        let parsed_input = crate::search::parse_input(&input, DEFAULT_MAX_RESULTS)?;
        let context_lines = get_usize(&input, "context_lines")?
            .unwrap_or(0)
            .min(MAX_CONTEXT_LINES);

        reject_url("CodeSearch", &parsed_input.base_path)?;

        let regex = compile_pattern(&parsed_input.pattern, parsed_input.case_insensitive)?;
        let base = resolve_path(&parsed_input.base_path, &cwd, policy)?;
        let job = SearchJob {
            regex,
            include: parsed_input.include_patterns,
            exclude: parsed_input.exclude_patterns,
            max_results: parsed_input.max_results.min(RESULTS_CAP),
            per_file_cap: None,
            base,
            pattern: parsed_input.pattern.clone(),
        };
        let matches = tokio::task::spawn_blocking(move || {
            crate::search::run(&job, |regex, content, file_path, base, limit| {
                scan_file(regex, content, file_path, base, context_lines, limit)
            })
        })
        .await
        .map_err(|e| ToolError::Execution(format!("CodeSearch walk task failed: {e}")))?;

        if matches.is_empty() {
            return Ok(no_matches_message(&parsed_input.pattern));
        }

        Ok(
            render(&matches, &parsed_input.pattern, context_lines, &temp_dir)
                .with_hint(DisplayHint::Json),
        )
    }
}

/// Scan one file's lines for regex matches; return up to `limit` [`Match`]es.
///
/// Computes a path relative to `base_path`, enumerates `content.lines()` with
/// 1-indexed line numbers, and pushes one [`Match`] per matching line,
/// stopping once `limit` are collected. When `context_lines > 0`, each
/// match's `content` is a rendered snippet of the `±context_lines` window
/// with `>` marking the matched line; otherwise `content` is the matched
/// line's text (unused by the succinct renderer).
fn scan_file(
    regex: &Regex,
    content: &str,
    file_path: &Path,
    base_path: &Path,
    context_lines: usize,
    limit: usize,
) -> Vec<Match> {
    let lines: Vec<&str> = content.lines().collect();
    let rel_path = crate::search::relative_file(file_path, base_path);
    let mut results = Vec::new();
    for (line_num, line) in lines.iter().enumerate() {
        if results.len() >= limit {
            break;
        }
        if !regex.is_match(line) {
            continue;
        }
        let content = if context_lines > 0 {
            render_snippet(&lines, line_num, context_lines)
        } else {
            line.to_string()
        };
        results.push(Match {
            file: rel_path.clone(),
            line: line_num.saturating_add(1),
            content,
        });
    }
    results
}

/// Render the `±context_lines` window around `line_num` as a snippet.
///
/// The matched line (`line_num + 1`, 1-indexed) is marked `>`; its neighbors
/// are marked ` `. Each line is formatted `{marker}{lineno}: {text}` and the
/// window is joined with newlines.
fn render_snippet(lines: &[&str], line_num: usize, context_lines: usize) -> String {
    let start = line_num.saturating_sub(context_lines);
    let end = line_num
        .saturating_add(context_lines)
        .saturating_add(1)
        .min(lines.len());
    let matched_1indexed = line_num.saturating_add(1);
    let mut buf = Vec::new();
    for (i, l) in lines.get(start..end).unwrap_or(&[]).iter().enumerate() {
        let actual = start.saturating_add(i).saturating_add(1);
        let marker = if actual == matched_1indexed { ">" } else { " " };
        buf.push(format!("{marker}{actual}: {l}"));
    }
    buf.join("\n")
}

/// Render the collected matches in one of two modes keyed on `context_lines`.
///
/// `context_lines == 0` → grouped: `Found N match(es) for "{pattern}":` header,
/// a blank line, then per file the path and collapsed line ranges
/// (`  {N}` or `  {A}-{B}`). `context_lines > 0` → the same header followed by
/// `file:line` per match and its snippet (the first snippet line indented).
/// Output spills to a temp
/// file via the shared helper if it exceeds the inline limit.
fn render(matches: &[Match], pattern: &str, context_lines: usize, temp_dir: &Path) -> ToolOutput {
    let mut out = Vec::with_capacity(matches.len().saturating_add(2));
    let match_word = if matches.len() == 1 {
        "match"
    } else {
        "matches"
    };
    out.push(format!(
        "Found {} {match_word} for \"{pattern}\":",
        matches.len()
    ));
    out.push(String::new());

    if context_lines == 0 {
        let mut by_file: std::collections::BTreeMap<&str, Vec<usize>> =
            std::collections::BTreeMap::new();

        for m in matches {
            by_file.entry(m.file.as_str()).or_default().push(m.line);
        }

        for (file, lines) in by_file {
            out.push(file.to_string());
            for range in group_consecutive(lines) {
                match range {
                    LineRange::Single(n) => out.push(format!("  {n}")),
                    LineRange::Range(s, e) => out.push(format!("  {s}-{e}")),
                }
            }
        }
    } else {
        let mut sorted: Vec<&Match> = matches.iter().collect();
        sorted.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
        for m in sorted {
            out.push(format!("{}:{}", m.file, m.line));
            if !m.content.is_empty() {
                out.push(format!("  {}", m.content));
            }
        }
    }
    let text = out.join("\n");
    truncate_or_write_to_temp(text, "code_search", temp_dir, MAX_INLINE_OUTPUT_BYTES)
}

/// A display form for a (possibly collapsed) run of consecutive line numbers.
///
/// Produced by [`group_consecutive`] from a `Vec<usize>` of raw line numbers.
/// The succinct renderer formats each variant as either `  {N}` (single) or
/// `  {A}-{B}` (range) — the two-space indent and the hyphen syntax are the
/// shape the model has been trained to read and act on.
#[derive(Debug, PartialEq, Eq)]
enum LineRange {
    /// A single, isolated line number with no immediate neighbors in the set.
    ///
    /// Rendered as `  {N}`. Produced when a line has no adjacent matches —
    /// e.g. line 5 alone in a set of `[5, 7, 8]`.
    Single(usize),

    /// An inclusive run of two or more consecutive line numbers, `start..=end`.
    ///
    /// Rendered as `  {start}-{end}`. Produced when [`group_consecutive`]
    /// walks a run of adjacent lines — e.g. `[7, 8]` becomes
    /// `Range(7, 8)` rendered as `  7-8`. A run of length 1 is always a
    /// [`Single`](Self::Single), never a degenerate `Range(n, n)`.
    Range(usize, usize),
}

/// Sort, dedup, and collapse consecutive line numbers into [`LineRange`]s.
///
/// Sorts ascending, drops duplicates, then walks the run emitting
/// [`LineRange::Single`] for isolated lines and [`LineRange::Range`] for
/// two-or-more consecutive. A single line never becomes a degenerate
/// `Range(n, n)` — it stays a `Single`.
///
/// Used only by the succinct renderer to turn a file's match line numbers
/// into the compact `5` / `7-8` display form.
fn group_consecutive(mut lines: Vec<usize>) -> Vec<LineRange> {
    lines.sort_unstable();
    lines.dedup();
    let mut ranges = Vec::new();
    let mut iter = lines.into_iter();
    let Some(mut start) = iter.next() else {
        return ranges;
    };
    let mut prev = start;
    for line in iter {
        if line == prev.saturating_add(1) {
            prev = line;
        } else {
            push_range(&mut ranges, start, prev);
            start = line;
            prev = line;
        }
    }
    push_range(&mut ranges, start, prev);
    ranges
}

/// Append the appropriate [`LineRange`] variant for a finalized run.
///
/// Helper for [`group_consecutive`]: once a run of consecutive lines ends (or
/// the input ends), this decides whether the run was a single line (`start ==
/// end` → `Single`) or a real range (`start < end` → `Range`) and pushes it.
fn push_range(out: &mut Vec<LineRange>, start: usize, end: usize) {
    if start == end {
        out.push(LineRange::Single(start));
    } else {
        out.push(LineRange::Range(start, end));
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
    clippy::too_many_lines
)]
mod tests {
    use super::*;
    use crate::context::RunnerContext;
    use loopctl::tool::ToolContext;
    use serde_json::json;
    use std::path::PathBuf;

    fn ctx_in(dir: &Path) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = dir.to_string_lossy().into_owned();
        ctx.set_extension(RunnerContext::new(PathBuf::from(dir)));
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
    fn group_consecutive_empty() {
        assert!(group_consecutive(vec![]).is_empty());
    }

    #[test]
    fn group_consecutive_single() {
        assert_eq!(group_consecutive(vec![5]), vec![LineRange::Single(5)]);
    }

    #[test]
    fn group_consecutive_one_run() {
        assert_eq!(
            group_consecutive(vec![5, 6, 7, 8]),
            vec![LineRange::Range(5, 8)]
        );
    }

    #[test]
    fn group_consecutive_multiple_runs() {
        assert_eq!(
            group_consecutive(vec![1, 2, 3, 5, 7, 8, 10]),
            vec![
                LineRange::Range(1, 3),
                LineRange::Single(5),
                LineRange::Range(7, 8),
                LineRange::Single(10),
            ]
        );
    }

    #[test]
    fn group_consecutive_dedup() {
        assert_eq!(
            group_consecutive(vec![1, 1, 2, 2, 3]),
            vec![LineRange::Range(1, 3)]
        );
    }

    #[test]
    fn parse_defaults() {
        let common =
            crate::search::parse_input(&json!({"pattern": "foo"}), DEFAULT_MAX_RESULTS).unwrap();
        assert_eq!(common.pattern, "foo");
        assert_eq!(common.base_path, ".");
        assert!(!common.case_insensitive);
        assert_eq!(common.max_results, DEFAULT_MAX_RESULTS);
        let context_lines = get_usize(&json!({}), "context_lines")
            .unwrap()
            .unwrap_or(0)
            .min(MAX_CONTEXT_LINES);
        assert_eq!(context_lines, 0);
    }

    #[test]
    fn parse_clamps_max_results() {
        let common = crate::search::parse_input(
            &json!({"pattern": "x", "max_results": 99_999}),
            DEFAULT_MAX_RESULTS,
        )
        .unwrap();
        assert_eq!(common.max_results.min(RESULTS_CAP), RESULTS_CAP);
    }

    #[test]
    fn parse_clamps_context_lines() {
        let cl = get_usize(
            &json!({"pattern": "x", "context_lines": 99}),
            "context_lines",
        )
        .unwrap()
        .unwrap_or(0)
        .min(MAX_CONTEXT_LINES);
        assert_eq!(cl, MAX_CONTEXT_LINES);
    }

    #[test]
    fn parse_rejects_negative_max_results() {
        assert!(
            crate::search::parse_input(
                &json!({"pattern": "x", "max_results": -1}),
                DEFAULT_MAX_RESULTS
            )
            .is_err()
        );
    }

    #[test]
    fn parse_missing_pattern_errors() {
        assert!(crate::search::parse_input(&json!({}), DEFAULT_MAX_RESULTS).is_err());
    }

    #[test]
    fn scan_file_succinct_keeps_matched_text() {
        let regex = regex::Regex::new("foo").unwrap();
        let content = "foo\nbar\nfoo";
        let results = scan_file(
            &regex,
            content,
            Path::new("/repo/a.rs"),
            Path::new("/repo"),
            0,
            100,
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].line, 1);
        assert_eq!(results[1].line, 3);
        // Succinct: content is the raw line text (the renderer ignores it).
        assert_eq!(results[0].content, "foo");
    }

    #[test]
    fn scan_file_context_renders_snippet() {
        let regex = regex::Regex::new("mid").unwrap();
        let content = "line1\nmid\nline3";
        let results = scan_file(
            &regex,
            content,
            Path::new("/repo/a.rs"),
            Path::new("/repo"),
            1,
            100,
        );
        assert_eq!(results.len(), 1);
        let snippet = &results[0].content;
        assert!(snippet.contains(">2: mid"), "{snippet}");
        assert!(snippet.contains(" 1: line1"), "{snippet}");
        assert!(snippet.contains(" 3: line3"), "{snippet}");
    }

    #[test]
    fn scan_file_context_at_file_start() {
        let regex = regex::Regex::new("first").unwrap();
        let content = "first\nsecond\nthird";
        let results = scan_file(
            &regex,
            content,
            Path::new("/repo/a.rs"),
            Path::new("/repo"),
            3,
            100,
        );
        assert_eq!(results.len(), 1);
        let snippet = &results[0].content;
        assert!(snippet.contains(">1: first"), "{snippet}");
        assert!(snippet.contains(" 2: second"), "{snippet}");
    }

    #[test]
    fn scan_file_respects_limit() {
        let regex = regex::Regex::new("x").unwrap();
        let content = "x\nx\nx\nx\nx";
        let results = scan_file(
            &regex,
            content,
            Path::new("/repo/a.rs"),
            Path::new("/repo"),
            0,
            2,
        );
        assert_eq!(results.len(), 2, "per-file limit stops the scan early");
    }

    #[test]
    fn render_snippet_marks_matched_line_with_gt() {
        let lines = vec!["alpha", "mid", "gamma"];
        // line_num is 0-indexed; match is on index 1 ("mid").
        let snippet = render_snippet(&lines, 1, 1);
        assert!(snippet.contains(">2: mid"), "{snippet}");
        assert!(snippet.contains(" 1: alpha"), "{snippet}");
        assert!(snippet.contains(" 3: gamma"), "{snippet}");
    }

    #[test]
    fn render_snippet_at_file_start_clamps_window() {
        // Match on line 1 (index 0), context 3 → start saturates to 0.
        let lines = vec!["first", "second", "third"];
        let snippet = render_snippet(&lines, 0, 3);
        assert!(snippet.contains(">1: first"), "{snippet}");
        assert!(snippet.contains(" 2: second"), "{snippet}");
        assert!(snippet.contains(" 3: third"), "{snippet}");
        // No line 0 or negative.
        assert!(!snippet.contains(" 0:"), "{snippet}");
    }

    #[test]
    fn render_snippet_at_file_end_clamps_window() {
        // Match on last line; trailing context clamps to lines.len().
        let lines = vec!["a", "b", "last"];
        let snippet = render_snippet(&lines, 2, 5);
        assert!(snippet.contains(">3: last"), "{snippet}");
        assert!(snippet.contains(" 2: b"), "{snippet}");
        assert!(!snippet.contains(" 4:"), "no line beyond EOF: {snippet}");
    }

    #[test]
    fn render_succinct_groups_and_collapses() {
        let matches = vec![
            Match {
                file: "a.rs".into(),
                line: 5,
                content: "x".into(),
            },
            Match {
                file: "a.rs".into(),
                line: 6,
                content: "x".into(),
            },
            Match {
                file: "a.rs".into(),
                line: 7,
                content: "x".into(),
            },
            Match {
                file: "b.rs".into(),
                line: 10,
                content: "x".into(),
            },
        ];
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, "x", 0, tmp.path());
        let text = out.text_content();
        assert!(text.contains("Found 4 matches for \"x\":"), "{text}");
        assert!(text.contains("a.rs\n  5-7"), "{text}");
        assert!(text.contains("b.rs\n  10"), "{text}");
        // Succinct mode: no matched content.
        assert!(!text.contains("\nx\n"), "no content: {text}");
    }

    #[test]
    fn render_context_emits_file_colon_line_and_snippet() {
        let matches = vec![Match {
            file: "a.rs".into(),
            line: 2,
            content: ">2: mid\n 3: line3".into(),
        }];
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, "mid", 1, tmp.path());
        let text = out.text_content();
        assert!(text.contains("a.rs:2"), "{text}");
        assert!(text.contains("  >2: mid"), "{text}");
    }

    #[test]
    fn render_files_sorted_alphabetically() {
        let matches = vec![
            Match {
                file: "z.rs".into(),
                line: 1,
                content: "x".into(),
            },
            Match {
                file: "a.rs".into(),
                line: 1,
                content: "x".into(),
            },
            Match {
                file: "m.rs".into(),
                line: 1,
                content: "x".into(),
            },
        ];
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, "x", 0, tmp.path());
        let text = out.text_content();
        let a = text.find("a.rs").unwrap();
        let m = text.find("m.rs").unwrap();
        let z = text.find("z.rs").unwrap();
        assert!(a < m && m < z, "files sorted alphabetically: {text}");
    }

    #[test]
    fn render_context_mode_sorted_by_file_then_line() {
        // Matches in non-sorted order; context mode must sort by file then line.
        let matches = vec![
            Match {
                file: "b.rs".into(),
                line: 10,
                content: "x".into(),
            },
            Match {
                file: "a.rs".into(),
                line: 5,
                content: "x".into(),
            },
            Match {
                file: "a.rs".into(),
                line: 3,
                content: "x".into(),
            },
            Match {
                file: "b.rs".into(),
                line: 2,
                content: "x".into(),
            },
        ];
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, "x", 1, tmp.path());
        let text = out.text_content();
        let file_lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains(':') && !l.starts_with("Found") && !l.starts_with("  "))
            .collect();
        // a.rs:3 should come before a.rs:5, which comes before b.rs:2, then b.rs:10
        let joined = file_lines.join("\n");
        assert!(
            joined.find("a.rs:3").unwrap() < joined.find("a.rs:5").unwrap(),
            "same file sorted by line: {joined}"
        );
        assert!(
            joined.find("a.rs:5").unwrap() < joined.find("b.rs:2").unwrap(),
            "files sorted alphabetically: {joined}"
        );
        assert!(
            joined.find("b.rs:2").unwrap() < joined.find("b.rs:10").unwrap(),
            "same file sorted by line: {joined}"
        );
    }

    #[test]
    fn render_large_output_spills_to_temp() {
        let padding = "x".repeat(400);
        let matches: Vec<Match> = (0..300)
            .map(|i| Match {
                file: format!("f{i}.rs"),
                line: i + 1,
                content: padding.clone(),
            })
            .collect();
        let tmp = tempfile::TempDir::new().unwrap();
        let out = render(&matches, "x", 1, tmp.path());
        let text = out.text_content();
        assert!(text.contains("result too large"), "should spill: {text}");
        assert!(text.contains("Full output written to:"), "{text}");
    }

    #[tokio::test]
    async fn succinct_happy_path_multifile() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "fn foo() {}\nconst X = 1;\n");
        write_file(tmp.path(), "b.txt", "foo bar\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool.call(json!({"pattern": "foo"}), &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Found 2 matches for \"foo\":"), "{text}");
        assert!(text.contains("a.rs"), "{text}");
        assert!(text.contains("b.txt"), "{text}");
        assert!(
            !text.contains("fn foo()"),
            "no content in succinct mode: {text}"
        );
        assert_eq!(out.display_hint, Some(DisplayHint::Json));
    }

    #[tokio::test]
    async fn consecutive_lines_collapse_to_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "foo\nfoo\nfoo\n";
        write_file(tmp.path(), "a.rs", body);
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool.call(json!({"pattern": "foo"}), &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("1-3"), "collapsed to a range: {text}");
    }

    #[tokio::test]
    async fn context_lines_renders_snippet() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "one\ntwo\nthree\nfour\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({"pattern": "two", "context_lines": 1}), &ctx)
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs:2"), "{text}");
        assert!(text.contains(">2: two"), "{text}");
        assert!(text.contains(" 1: one"), "{text}");
        assert!(text.contains(" 3: three"), "{text}");
    }

    #[tokio::test]
    async fn max_results_global_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", &"x\n".repeat(100));
        write_file(tmp.path(), "b.rs", &"x\n".repeat(100));
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({"pattern": "x", "max_results": 5}), &ctx)
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("Found 5 matches"), "global cap at 5: {text}");
    }

    #[tokio::test]
    async fn max_results_clamped_to_max() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "x\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({"pattern": "x", "max_results": 99999}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn case_insensitive_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({"pattern": "FOO", "case_insensitive": true}), &ctx)
            .await
            .unwrap();
        assert!(
            out.text_content().contains("Found 1 match"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn include_patterns_restricts_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        write_file(tmp.path(), "b.txt", "foo\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(
                json!({"pattern": "foo", "include_patterns": ["*.rs"]}),
                &ctx,
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.txt"), "{text}");
    }

    #[tokio::test]
    async fn exclude_patterns_removes_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        write_file(tmp.path(), "b.lock", "foo\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(
                json!({"pattern": "foo", "exclude_patterns": ["*.lock"]}),
                &ctx,
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.lock"), "{text}");
    }

    #[tokio::test]
    async fn binary_file_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut bytes = vec![0u8; 32];
        bytes.extend_from_slice(b"foo\n");
        std::fs::write(tmp.path().join("data.png"), &bytes).unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(
                json!({"pattern": "foo", "include_patterns": ["*.png", "*.rs"]}),
                &ctx,
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(!text.contains("data.png"), "binary skipped: {text}");
    }

    #[tokio::test]
    async fn no_matches_is_success_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool.call(json!({"pattern": "zzz"}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(
            out.text_content()
                .contains("No matches found for pattern: zzz")
        );
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let err = tool
            .call(json!({"pattern": "(unclosed"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("Invalid regex pattern")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn url_path_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let err = tool
            .call(
                json!({"pattern": "x", "path": "https://example.com/y"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn relative_path_resolved_against_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "foo\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(json!({"pattern": "foo", "path": "."}), &ctx)
            .await
            .unwrap();
        assert!(out.text_content().contains("a.rs"));
    }

    #[tokio::test]
    async fn gitignore_is_respected() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        write_file(tmp.path(), "ignored.rs", "foo\n");
        write_file(tmp.path(), "kept.rs", "foo\n");
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool.call(json!({"pattern": "foo"}), &ctx).await.unwrap();
        let text = out.text_content();
        assert!(!text.contains("ignored.rs"), "{text}");
        assert!(text.contains("kept.rs"), "{text}");
    }

    #[tokio::test]
    async fn empty_file_no_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("empty.rs"), "").unwrap();
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool.call(json!({"pattern": "foo"}), &ctx).await.unwrap();
        assert!(out.text_content().contains("No matches found"));
    }

    #[tokio::test]
    async fn malformed_max_results_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let err = tool
            .call(json!({"pattern": "x", "max_results": -5}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn large_result_spills_to_temp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let padding = "x".repeat(400);
        let line = format!("match {padding}");
        let body: String = std::iter::repeat_n(format!("{line}\n"), 600).collect();
        write_file(tmp.path(), "big.rs", &body);
        let tool = CodeSearchInput::default();
        let ctx = ctx_in(tmp.path());
        let out = tool
            .call(
                json!({"pattern": "match", "context_lines": 2, "max_results": 200}),
                &ctx,
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("result too large"), "should spill: {text}");
        assert!(text.contains("Full output written to:"), "{text}");
        assert!(text.contains("Use FileViewer"), "{text}");
    }

    #[test]
    fn trait_contract_and_registry() {
        let tool = CodeSearchInput::default();
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.name(), "CodeSearch");
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("CodeSearch").is_some(), "CodeSearch registered");
    }
}
