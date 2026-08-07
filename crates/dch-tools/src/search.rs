//! Shared machinery for the regex content-search tools (`Grep`, `CodeSearch`).
//!
//! Both tools walk a directory with the shared gitignore-aware walker, read
//! each non-binary file, and collect regex matches as [`Match`] records. What
//! differs is how each file is scanned (`Grep` emits one record per matched
//! line; `CodeSearch` can attach a context snippet) and how the collected
//! matches are rendered (`Grep` → JSON array; `CodeSearch` → grouped ranges
//! or `file:line`). This module owns everything *else*: the walk loop, the
//! per-file and global caps, the binary/size guards, the regex cache lookup,
//! and the no-match success message.

use std::path::Path;
use std::path::PathBuf;

use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use regex::Regex;

use crate::input::get_string_list;
use crate::input::get_usize;
use crate::regex_cache::get_or_compile;
use crate::walk;

/// One matched line, as collected by the search runner.
///
/// The common shape both content-search tools agree on: a relative file path,
/// a 1-indexed line number, and the line's text. This is the unit the shared
/// walk loop collects; each tool then renders its `Vec<Match>` its own way.
///
/// `Grep` serializes a slice of these verbatim as JSON objects
/// `{"file", "line", "content"}`. `CodeSearch` groups them by file and
/// collapses the line numbers into ranges for its succinct mode; in its
/// context mode it repurposes the `content` field to hold a rendered
/// `±context_lines` snippet at scan time, then emits `file:line` plus that
/// snippet.
///
/// Cloning is cheap (three owned `String`s/`usize`), and the shared runner
/// hands each tool an owned `Vec<Match>`, so tools can move or clone records
/// freely while rendering.
#[derive(Debug, Clone)]
pub struct Match {
    /// The file path relative to the search base.
    ///
    /// Computed at scan time by stripping the walked file's absolute path
    /// against the resolved search base. Falls back to the file's display
    /// form (or `"unknown"` for non-UTF8 paths) when the strip fails — e.g.
    /// for entries the walker yields from outside the base. Both renderers
    /// echo this verbatim into the output.
    pub file: String,

    /// 1-indexed line number of the match within the file.
    ///
    /// Source files are 1-indexed in every editor and in the model's
    /// expectation, so this is stored 1-indexed even though `lines()`
    /// enumeration is 0-indexed; the scan functions apply the `+1`. The
    /// grouped renderer collapses consecutive values of this field into
    /// ranges (`5-7`); the JSON renderer emits it as-is.
    pub line: usize,

    /// The matched line's text, or a rendered context snippet.
    ///
    /// `Grep` always stores the raw matched line here and serializes it as
    /// the JSON `content`. `CodeSearch` stores the raw line in succinct mode
    /// (its renderer ignores it — it only emits the line number) and a
    /// rendered `±context_lines` snippet in context mode (which its renderer
    /// emits indented under the `file:line` header).
    pub content: String,
}

/// Bundles the inputs the shared walk loop needs.
///
/// Constructed by each tool's async body from its parsed input, moved into the
/// blocking thread, and read by [`run`]. Grouping these fields here keeps the
/// [`run`] signature to one struct plus a scan closure, instead of a long
/// parameter list.
///
/// Tool-specific knobs (`Grep`'s per-file `max_matches`, `CodeSearch`'s
/// `context_lines`) are deliberately **not** here — they are captured by the
/// per-file scan closure each tool passes to [`run`]. Keeping them out means
/// this struct models only the walk/cap state that is genuinely shared, and a
/// future search tool with different knobs can reuse it without inheriting
/// fields it does not use.
pub struct SearchJob {
    /// The compiled regex to match against each line.
    ///
    /// Produced by [`compile_pattern`] on the async side before the job is
    /// built, so the blocking thread does no regex compilation. Cheap to move
    /// across the thread boundary (`Regex` is `Arc`-backed internally).
    pub regex: Regex,

    /// Filename-level glob filters forwarded to the walker.
    ///
    /// Empty means "no include filter" — every file the walker yields is read.
    /// Drawn from the tool's `include_patterns` input array; each entry is
    /// matched with the walker's single-segment `*`/`?` matcher against file
    /// basenames.
    pub include: Vec<String>,

    /// Filename-level glob exclusions forwarded to the walker.
    ///
    /// A file whose basename matches any entry here is skipped before it is
    /// read. Drawn from the tool's `exclude_patterns` input array; matched
    /// with the same single-segment matcher as [`include`](Self::include).
    pub exclude: Vec<String>,

    /// Total match cap across all files.
    ///
    /// Enforced in [`run`] as a running-total ceiling: once this many matches
    /// are collected overall, the walk stops. The per-file scan closure
    /// receives the headroom under this cap (further tightened by
    /// [`per_file_cap`](Self::per_file_cap) when set) as its `limit`
    /// argument, so a single huge file cannot allocate unbounded matches
    /// before the cap discards them.
    pub max_results: usize,

    /// Optional per-file match cap.
    ///
    /// `None` (the `CodeSearch` shape) means "no per-file cap — the only
    /// ceiling is [`max_results`](Self::max_results)." `Some(n)` (the `Grep`
    /// shape, where `n` is `max_matches`, default 100) tightens each file's
    /// scan to `min(n, remaining_under_max_results)`, stopping one huge file
    /// from saturating the result before the walker moves on.
    ///
    /// Enforced in [`run`]: it computes the effective per-file limit and
    /// passes that to the closure, so the cap is visible at the loop level
    /// rather than hidden inside a tool-specific closure.
    pub per_file_cap: Option<usize>,

    /// The directory to search, already resolved against the runner cwd.
    ///
    /// Absolute by the time it reaches the job — [`resolve_path`](crate::util::resolve_path)
    /// was applied on the async side. Every walked file's path is stripped
    /// against this base to produce the relative `file` field of each
    /// [`Match`].
    pub base: PathBuf,

    /// The pattern string, exactly as the caller supplied it.
    ///
    /// Carried alongside the compiled [`regex`](Self::regex) only so the
    /// no-match success message ("No matches found for pattern: …") can echo
    /// the original text back to the model. Not used for matching — the
    /// compiled regex is.
    pub pattern: String,
}

/// The fields shared by every content-search tool's input.
///
/// Both `Grep` and `CodeSearch` parse `pattern`, `path`, `case_insensitive`,
/// `max_results`, `include_patterns`, and `exclude_patterns` identically.
/// This struct carries those six fields so the parsing logic lives once in
/// [`parse_input`]; each tool then layers its own tool-specific
/// fields (`max_matches`, `context_lines`, etc.) on top.
pub struct CommonInput {
    /// The regex pattern, exactly as supplied by the caller.
    ///
    /// Fed to [`compile_pattern`] for caching; echoed back in the no-match
    /// message and the result header.
    pub pattern: String,

    /// The directory to search in, defaulted to `"."` when absent.
    ///
    /// May be relative; the caller resolves it against the runner cwd before
    /// walking. Rejected by the caller if it is a URL.
    pub base_path: String,

    /// Whether to compile the pattern case-insensitively.
    ///
    /// When `true`, [`compile_pattern`] wraps the pattern with `(?i)`
    /// (idempotently) before caching. Defaults to `false`.
    pub case_insensitive: bool,

    /// Total match cap across all files, before any tool-specific clamping.
    ///
    /// Defaults to the `default_max_results` argument passed to
    /// [`parse_input`]. The caller may further clamp this (e.g.
    /// `CodeSearch` applies `.min(RESULTS_CAP)`). Enforced globally in
    /// [`run`] — once this many matches are collected, the walk stops.
    pub max_results: usize,

    /// Filename-level glob filters forwarded to the walker.
    ///
    /// Empty means "no include filter." Non-string elements are dropped by
    /// [`get_string_list`] before this struct is built.
    pub include_patterns: Vec<String>,

    /// Filename-level glob exclusions forwarded to the walker.
    ///
    /// A file matching any entry here is skipped before it is read.
    pub exclude_patterns: Vec<String>,
}

/// Parse the six input fields shared by all content-search tools.
///
/// Extracts `pattern` (required), `path` (default `"."`), `case_insensitive`
/// (default `false`), `max_results` (default `default_max_results`),
/// `include_patterns` (default empty), and `exclude_patterns` (default empty)
/// from the tool's JSON input. Malformed values are rejected loudly via
/// [`get_usize`] / [`get_string_list`] and explicit `as_str` / `as_bool`
/// checks.
///
/// Each tool calls this with its own default for `max_results`, then parses
/// its own additional fields and assembles its tool-specific struct. The
/// caller may further clamp `max_results` after this call (e.g. `.min(cap)`).
/// An explicit zero is clamped to `1` so the caller always gets at least one
/// result.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `pattern` is missing, when
/// `max_results` is present but not a non-negative integer, or when an
/// array field is present but not a JSON array.
pub fn parse_input(
    input: &serde_json::Value,
    default_max_results: usize,
) -> Result<CommonInput, ToolError> {
    let pattern = input
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("Missing 'pattern' field".to_string()))?
        .to_string();
    let base_path = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| ".".to_string(), str::to_string);
    let case_insensitive = input
        .get("case_insensitive")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let max_results = get_usize(input, "max_results")?
        .unwrap_or(default_max_results)
        .max(1);
    let include_patterns = get_string_list(input, "include_patterns")?;
    let exclude_patterns = get_string_list(input, "exclude_patterns")?;
    Ok(CommonInput {
        pattern,
        base_path,
        case_insensitive,
        max_results,
        include_patterns,
        exclude_patterns,
    })
}

/// Compute the display path of `file_path` relative to `base_path`.
///
/// Returns the stripped relative path when possible, the full path string when
/// `file_path` is not under `base_path`, or `"unknown"` when the path cannot be
/// rendered as UTF-8. Shared by Grep and `CodeSearch` so the relative-path
/// computation is identical across both tools.
#[must_use]
pub fn relative_file(file_path: &Path, base_path: &Path) -> String {
    file_path
        .strip_prefix(base_path)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or_else(|| file_path.to_str().unwrap_or("unknown"))
        .to_string()
}

/// Compile `pattern` through the shared regex cache, case-sensitive or not.
///
/// Routes the pattern through [`get_or_compile`] so a pattern compiled by one
/// tool is a cache hit when the other tool (or a later call to the same tool)
/// compiles it. When `case_insensitive` is `true`, the pattern is wrapped
/// with the `(?i)` flag (idempotently) before caching.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] wrapping the regex engine's error if
/// the pattern fails to compile. The caller surfaces this directly to the
/// model — an invalid regex is a caller bug worth fixing, not a tool failure.
pub fn compile_pattern(pattern: &str, case_insensitive: bool) -> Result<Regex, ToolError> {
    get_or_compile(pattern, case_insensitive)
        .map_err(|e| ToolError::InvalidInput(format!("Invalid regex pattern: {e}")))
}

/// Walk `job.base`, scan each non-binary file via `scan_file`, collect up to
/// `job.max_results` [`Match`]es total.
///
/// This is the shared heart of both content-search tools. It owns:
/// - the gitignore-aware directory walk (via [`walk::walk_files`]),
/// - the binary-file skip (via [`walk::likely_binary`]),
/// - the file-size guard against OOM on huge files (via [`walk::file_too_large`]),
/// - the whole-file read (`std::fs::read_to_string`),
/// - and **both caps** — the global `max_results` ceiling (the walk stops once
///   it's reached) and the per-file `per_file_cap` (each file's scan is
///   limited to `min(per_file_cap, remaining_under_max_results)` when set).
///
/// `scan_file` is the per-tool specialization: it receives the file's content,
/// the resolved paths, and `limit` — the already-tightened per-file ceiling
/// (the `min` of the two caps). Its contract is simply to return at most
/// `limit` matches; it does no cap math itself. This keeps the cap policy
/// visible at the loop level rather than hidden inside a tool-specific
/// closure.
///
/// Unreadable files are silently skipped — a search over a tree with
/// permission-denied files still returns the matches it could collect, rather
/// than failing the whole search. This matches the behavior of `grep` and
/// `ripgrep`: a permission error on one file is not a search failure.
pub fn run<F>(job: &SearchJob, scan_file: F) -> Vec<Match>
where
    F: Fn(&Regex, &str, &Path, &Path, usize) -> Vec<Match>,
{
    let mut matches = Vec::new();
    for entry in walk::walk_files(&job.base, &job.include, &job.exclude) {
        if matches.len() >= job.max_results {
            break;
        }
        let path = entry.path();
        if walk::file_too_large(path) || walk::likely_binary(path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let remaining = job.max_results.saturating_sub(matches.len());
        let limit = job.per_file_cap.map_or(remaining, |cap| cap.min(remaining));
        let file_matches = scan_file(&job.regex, &content, path, &job.base, limit);
        matches.extend(file_matches);
    }
    matches
}

/// The "no matches" success message both tools emit for an empty result set.
///
/// An empty search is **not** an error — the model uses "matched nothing" as a
/// signal to broaden or refine its pattern, distinct from a tool failure.
/// Returning it as a successful [`ToolOutput::text`] (rather than a soft
/// error) keeps `is_error == false`, so the model's retry logic does not fire
/// on a legitimately-empty result.
///
/// Both tools reach for this via the shared runner so the wording stays
/// consistent: the body names the pattern verbatim, which is the shape the
/// model expects and can act on (broaden or refine the pattern).
#[must_use]
pub fn no_matches_message(pattern: &str) -> ToolOutput {
    ToolOutput::text(format!("No matches found for pattern: {pattern}"))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::regex_cache::clear_cache;
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-global regex cache so they
    /// cannot race against each other or against other cache-mutating tests.
    static CACHE_LOCK: Mutex<()> = Mutex::new(());

    fn job(base: &Path, max_results: usize, per_file_cap: Option<usize>) -> SearchJob {
        SearchJob {
            regex: Regex::new("match").unwrap(),
            include: vec![],
            exclude: vec![],
            max_results,
            per_file_cap,
            base: base.to_path_buf(),
            pattern: "match".to_string(),
        }
    }

    /// A scan closure that emits one `Match` per line containing the regex's
    /// pattern, up to `limit`. Mirrors what a real tool's `scan_file` does.
    fn scan(regex: &Regex, content: &str, file: &Path, base: &Path, limit: usize) -> Vec<Match> {
        let rel = file.strip_prefix(base).unwrap_or(file);
        let mut out = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if out.len() >= limit {
                break;
            }
            if regex.is_match(line) {
                out.push(Match {
                    file: rel.to_string_lossy().into_owned(),
                    line: i + 1,
                    content: line.to_string(),
                });
            }
        }
        out
    }

    #[test]
    fn compile_pattern_valid() {
        let _guard = CACHE_LOCK.lock().unwrap();
        clear_cache();
        let re = compile_pattern("foo", false).unwrap();
        assert!(re.is_match("foobar"));
    }

    #[test]
    fn compile_pattern_case_insensitive() {
        let _guard = CACHE_LOCK.lock().unwrap();
        clear_cache();
        let re = compile_pattern("foo", true).unwrap();
        assert!(re.is_match("FOO"));
        assert!(re.is_match("foo"));
    }

    #[test]
    fn compile_pattern_invalid_returns_invalid_input() {
        let _guard = CACHE_LOCK.lock().unwrap();
        clear_cache();
        let err = compile_pattern("(unclosed", false).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("Invalid regex pattern")),
            "{err:?}"
        );
    }

    #[test]
    fn no_matches_message_format_and_not_error() {
        let out = no_matches_message("my_pattern");
        assert!(!out.is_error, "no-match is a success, not an error");
        assert_eq!(
            out.text_content(),
            "No matches found for pattern: my_pattern"
        );
    }

    #[test]
    fn no_matches_message_preserves_special_chars() {
        let out = no_matches_message("(?P<name>foo)");
        assert!(out.text_content().contains("(?P<name>foo)"));
    }

    #[test]
    fn run_global_cap_stops_walk() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\nmatch\nmatch\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "match\nmatch\nmatch\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "match\nmatch\nmatch\n").unwrap();

        let j = job(tmp.path(), 4, None);
        let matches = run(&j, scan);
        assert_eq!(matches.len(), 4, "global cap stops at 4");
    }

    #[test]
    fn run_per_file_caps_each_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\nmatch\nmatch\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "match\nmatch\nmatch\n").unwrap();

        let j = job(tmp.path(), 100, Some(2));
        let matches = run(&j, scan);
        let a_count = matches.iter().filter(|m| m.file == "a.txt").count();
        let b_count = matches.iter().filter(|m| m.file == "b.txt").count();
        assert_eq!(a_count, 2, "per-file cap on a.txt");
        assert_eq!(b_count, 2, "per-file cap on b.txt");
        assert_eq!(matches.len(), 4);
    }

    #[test]
    fn run_per_file_none_means_only_global() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\nmatch\nmatch\n").unwrap();

        let j = job(tmp.path(), 100, None);
        let matches = run(&j, scan);
        assert_eq!(matches.len(), 3, "no per-file cap → all 3 from one file");
    }

    #[test]
    fn run_caps_compose_per_file_min_remaining() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\nmatch\nmatch\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "match\nmatch\nmatch\n").unwrap();

        // per_file 2, global 3: a.txt yields 2, b.txt yields 1, then stop.
        let j = job(tmp.path(), 3, Some(2));
        let matches = run(&j, scan);
        assert_eq!(matches.len(), 3, "total cap stops at 3");
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for m in &matches {
            *counts.entry(m.file.as_str()).or_insert(0) += 1;
        }
        let max = counts.values().copied().max().unwrap_or(0);
        assert!(max <= 2, "no file exceeded per-file cap: max was {max}");
    }

    #[test]
    fn run_passes_tightened_limit_to_closure() {
        use std::cell::Cell;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\nmatch\nmatch\n").unwrap();

        let j = job(tmp.path(), 100, Some(2));
        // Closure records the largest `limit` it saw via a Cell (run takes
        // Fn, not FnMut, so the closure must be stateless from the borrow
        // checker's view). With per_file=2 and remaining=100, the closure
        // should receive 2.
        let observed = Cell::new(usize::MAX);
        let matches = run(&j, |_re, _content, _file, _base, limit| {
            observed.set(limit.min(observed.get()));
            let mut v = Vec::new();
            for _ in 0..limit {
                v.push(Match {
                    file: "a.txt".to_string(),
                    line: 1,
                    content: String::new(),
                });
            }
            v
        });
        assert_eq!(observed.get(), 2, "closure received the per-file cap");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn run_skips_binary_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A file with NUL bytes → likely_binary returns true.
        let mut bytes = vec![0u8; 32];
        bytes.extend_from_slice(b"match\n");
        std::fs::write(tmp.path().join("data.png"), &bytes).unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\n").unwrap();

        let j = job(tmp.path(), 100, None);
        let matches = run(&j, scan);
        assert!(
            matches.iter().all(|m| m.file != "data.png"),
            "binary skipped"
        );
        assert!(matches.iter().any(|m| m.file == "a.txt"));
    }

    #[test]
    fn run_skips_oversized_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let big = "match ".repeat(usize::try_from(crate::walk::MAX_FILE_BYTES + 1).unwrap());
        std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
        std::fs::write(tmp.path().join("small.txt"), "match\n").unwrap();

        let j = job(tmp.path(), 100, None);
        let matches = run(&j, scan);
        assert!(
            matches.iter().all(|m| m.file != "big.txt"),
            "oversized file skipped"
        );
        assert!(matches.iter().any(|m| m.file == "small.txt"));
    }

    #[test]
    fn run_silently_skips_unreadable_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A path that exists as a directory (not a file) — read_to_string fails.
        std::fs::create_dir_all(tmp.path().join("not_a_file")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\n").unwrap();

        let j = job(tmp.path(), 100, None);
        // Should not panic; should return the one readable match.
        let matches = run(&j, scan);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "a.txt");
    }

    #[test]
    fn run_empty_dir_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let j = job(tmp.path(), 100, None);
        assert!(run(&j, scan).is_empty());
    }

    #[test]
    fn run_match_file_and_line_are_relative_and_1_indexed() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nBRAVO\nalpha\n").unwrap();

        // job() builds a regex for "match"; override to an exact, distinct token.
        let mut j = job(tmp.path(), 100, None);
        j.regex = Regex::new("BRAVO").unwrap();
        let matches = run(&j, scan);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "a.txt", "relative to base");
        assert_eq!(matches[0].line, 2, "1-indexed line number");
        assert_eq!(matches[0].content, "BRAVO");
    }

    #[test]
    fn relative_file_strips_base_prefix() {
        assert_eq!(
            relative_file(Path::new("/repo/src/main.rs"), Path::new("/repo")),
            "src/main.rs"
        );
    }

    #[test]
    fn relative_file_nested_subdir() {
        assert_eq!(
            relative_file(Path::new("/repo/a/b/c.rs"), Path::new("/repo")),
            "a/b/c.rs"
        );
    }

    #[test]
    fn relative_file_base_equals_file() {
        assert_eq!(relative_file(Path::new("/repo"), Path::new("/repo")), "");
    }

    #[test]
    fn relative_file_not_under_base_falls_back_to_full() {
        assert_eq!(
            relative_file(Path::new("/other/x.rs"), Path::new("/repo")),
            "/other/x.rs"
        );
    }
}
