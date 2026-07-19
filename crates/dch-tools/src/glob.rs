//! The `Glob` tool — gitignore-aware file globbing.
//!
//! Walks a directory with the shared [`walk_files`](crate::walk_files) walker
//! and returns the relative paths of files matching a glob pattern. Pattern
//! matching uses ripgrep's glob engine (`ignore::overrides::Override`), which
//! supports `*`, `?`, `**`, character classes `[abc]`, and brace expansion
//! `{a,b}`. A pattern with no `/` matches the basename anywhere in the tree.

use std::future::Future;
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
use crate::util::is_url;
use crate::util::resolve_path;
use crate::walk;

/// Gitignore-aware file globbing tool.
///
/// `Glob` resolves its `path` against the runner's cwd (or walks `.` when no
/// path is given), filters the file tree with [`walk_files`](crate::walk_files)
/// (which honors `.gitignore`, `.git/info/exclude`, global gitignore,
/// `.dchignore`, and an always-exclude list), and returns the relative paths
/// of files matching the user's `pattern`. Pattern matching uses ripgrep's
/// glob engine, so a pattern with no `/` matches the basename anywhere in the
/// tree (so `*.rs` matches both `top.rs` and `src/a.rs`). Results are sorted
/// alphabetically and returned as a pretty-printed JSON string array.
///
/// An empty result is a successful `ToolOutput::text("No files found…")`, not
/// an error: callers distinguish "matched nothing" from "failed" via `is_error`.
pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern in the specified directory. \
         Supports *, **, ? patterns. Returns matching file paths."
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
                        "description": "Glob pattern to match files (e.g., '**/*.rs', 'src/**/*.json')"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (defaults to current working directory)"
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
        Box::pin(self.glob_inner(input, rc))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

impl GlobTool {
    /// Body of [`Tool::call`].
    ///
    /// Orchestrates: extract cwd → parse args → build matcher → walk → sort
    /// → format. URL `path`, missing `pattern`, and an unparseable pattern
    /// become [`ToolError::InvalidInput`]; serialization failure becomes
    /// [`ToolError::Execution`]. An empty match set is a success message.
    ///
    /// The synchronous directory walk runs on a blocking thread via
    /// [`tokio::task::spawn_blocking`] so a large tree cannot stall the async
    /// runtime — `ignore`'s walker is itself synchronous.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when the [`RunnerContext`] extension is
    /// absent, the blocking task joins unsuccessfully, or `serde_json` cannot
    /// encode the result. Returns [`ToolError::InvalidInput`] for a missing
    /// `pattern`, a URL `path`, or a pattern the matcher cannot parse.
    async fn glob_inner(
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
        if is_url(&parsed.base_path) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the Glob tool. Use WebFetch for URLs.".to_string(),
            ));
        }

        let base = resolve_path(&parsed.base_path, &cwd);
        let glob_override = build_glob_override(&base, &parsed.pattern)?;
        let pattern = parsed.pattern.clone();
        let matches = tokio::task::spawn_blocking(move || collect_matches(&base, &glob_override))
            .await
            .map_err(|e| ToolError::Execution(format!("Glob walk task failed: {e}")))?;

        if matches.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No files found matching pattern: {pattern}"
            )));
        }

        let json = serde_json::to_string_pretty(&matches)
            .map_err(|e| ToolError::Execution(format!("Failed to serialize results: {e}")))?;
        Ok(ToolOutput::text(json))
    }
}

/// Walk `base` and collect the relative paths of files matching `glob_override`.
///
/// Runs on a blocking thread (called via `spawn_blocking`). Sorts the result
/// alphabetically before returning, as required by the tool spec.
fn collect_matches(base: &Path, glob_override: &ignore::overrides::Override) -> Vec<String> {
    let mut matches = Vec::new();
    for entry in walk::walk_files(base, &[], &[]) {
        let path = entry.path();
        let rel = rel_for_match(path, base);
        if glob_override.matched(rel.as_path(), false).is_whitelist() {
            push_match_string(&mut matches, path, base);
        }
    }
    matches.sort();
    matches
}

/// Parsed and validated `Glob` input.
///
/// `pattern` is the user's glob. `base_path` is the directory to search in,
/// either as supplied or defaulted to `"."` (the search root, resolved against
/// the runner cwd by the caller). It may be relative.
struct ParsedInput {
    /// The glob pattern supplied by the caller, copied verbatim from the input
    /// JSON.
    ///
    /// No normalization happens here — backslashes, brace expansion, character
    /// classes all pass through unchanged. The pattern is fed directly to the
    /// `ignore::overrides::Override` matcher in [`build_glob_override`].
    pattern: String,

    /// The directory to search in, defaulted to `"."` when absent.
    ///
    /// May be relative; the caller resolves it against the runner cwd via
    /// [`resolve_path`](crate::util::resolve_path) before walking, so relative
    /// paths reach the agent's working directory rather than the process's
    /// current directory. An absolute path is used as-is.
    base_path: String,
}

/// Extract the `pattern` (required) and `path` (optional, defaults to `"."`).
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `pattern` is missing or not a
/// string.
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
    Ok(ParsedInput { pattern, base_path })
}

/// Build a ripgrep-grade glob matcher for `pattern`, rooted at `base`.
///
/// The matcher uses gitignore-flavored semantics: a pattern with no `/`
/// matches the basename anywhere in the tree; a leading `/` anchors to the
/// search root; `!` is negation. Without a leading `!`, patterns are
/// whitelist matches and every non-matching file is implicitly excluded.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `pattern` is not a valid glob
/// (unclosed character class, malformed brace expansion, etc.).
fn build_glob_override(
    base: &Path,
    pattern: &str,
) -> Result<ignore::overrides::Override, ToolError> {
    let mut builder = ignore::overrides::OverrideBuilder::new(base);
    builder
        .add(pattern)
        .map_err(|e| ToolError::InvalidInput(format!("Invalid glob pattern '{pattern}': {e}")))?;
    builder
        .build()
        .map_err(|e| ToolError::Execution(format!("Failed to build glob matcher: {e}")))
}

/// The relative path to match against, falling back to the file name.
///
/// `ignore::overrides::Override::matched` matches paths relative to its root
/// (`base`). For a file `base/src/a.rs`, the relative path `src/a.rs` is what
/// the matcher expects. When the entry is not under `base` (e.g. an absolute
/// path from outside), the bare file name is used so the matcher still gets a
/// basename to test against `/`-free patterns.
fn rel_for_match(path: &Path, base: &Path) -> PathBuf {
    path.strip_prefix(base)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
}

/// Push a match string into the result set, relative to `base` when possible.
///
/// Prefers the path relative to `base` (so `src/a.rs`, not
/// `/abs/repo/src/a.rs`); falls back to the absolute form when the entry is
/// not under `base`.
fn push_match_string(matches: &mut Vec<String>, path: &Path, base: &Path) {
    let s = path.strip_prefix(base).map_or_else(
        |_| path.to_string_lossy().into_owned(),
        |p| p.to_string_lossy().into_owned(),
    );
    matches.push(s);
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
    use std::path::PathBuf;

    /// A matcher built against a fake root, for the no-I/O unit tests.
    fn matcher(pattern: &str) -> ignore::overrides::Override {
        let mut b = ignore::overrides::OverrideBuilder::new("/repo");
        b.add(pattern).expect("valid pattern");
        b.build().expect("builds")
    }

    #[test]
    fn matcher_recursive_star_matches_at_any_depth() {
        let ov = matcher("**/*.rs");
        assert!(ov.matched("src/main.rs", false).is_whitelist());
        assert!(ov.matched("a/b/c.rs", false).is_whitelist());
        assert!(ov.matched("top.rs", false).is_whitelist());
    }

    #[test]
    fn matcher_extension_only_matches_basename_anywhere() {
        // A pattern with no `/` matches the basename at any depth.
        let ov = matcher("*.rs");
        assert!(ov.matched("top.rs", false).is_whitelist());
        assert!(ov.matched("src/main.rs", false).is_whitelist());
        assert!(ov.matched("a/b/c.rs", false).is_whitelist());
        assert!(ov.matched("a/b/c.txt", false).is_ignore());
    }

    #[test]
    fn matcher_prefix_recursive_star_matches_zero_dirs() {
        let ov = matcher("src/**/*.rs");
        assert!(ov.matched("src/a.rs", false).is_whitelist());
        assert!(ov.matched("src/a/b.rs", false).is_whitelist());
        assert!(ov.matched("other/a.rs", false).is_ignore());
    }

    #[test]
    fn matcher_character_class() {
        let ov = matcher("*.[rs]s");
        assert!(ov.matched("a.rs", false).is_whitelist());
        assert!(ov.matched("a.ss", false).is_whitelist());
        assert!(ov.matched("a.cs", false).is_ignore());
    }

    #[test]
    fn matcher_brace_expansion() {
        let ov = matcher("*.{rs,toml}");
        assert!(ov.matched("a.rs", false).is_whitelist());
        assert!(ov.matched("b.toml", false).is_whitelist());
        assert!(ov.matched("c.json", false).is_ignore());
    }

    #[test]
    fn matcher_invalid_pattern_rejected() {
        let result = build_glob_override(&PathBuf::from("/repo"), "[unclosed");
        assert!(result.is_err(), "unclosed class must error");
        match result {
            Err(ToolError::InvalidInput(msg)) => assert!(
                msg.contains("Invalid glob pattern"),
                "message should explain: {msg}"
            ),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn parse_input_defaults_path_to_dot() {
        let input = json!({"pattern": "*.rs"});
        let parsed = parse_input(&input).unwrap();
        assert_eq!(parsed.pattern, "*.rs");
        assert_eq!(parsed.base_path, ".");
    }

    #[test]
    fn parse_input_uses_explicit_path() {
        let input = json!({"pattern": "*.rs", "path": "/other"});
        let parsed = parse_input(&input).unwrap();
        assert_eq!(parsed.base_path, "/other");
    }

    #[test]
    fn parse_input_missing_pattern_errors() {
        let input = json!({});
        assert!(parse_input(&input).is_err());
    }

    #[test]
    fn rel_for_match_strips_base_prefix() {
        let path = Path::new("/repo/src/a.rs");
        let base = Path::new("/repo");
        assert_eq!(rel_for_match(path, base), PathBuf::from("src/a.rs"));
    }

    #[test]
    fn rel_for_match_falls_back_to_absolute() {
        let path = Path::new("/other/a.rs");
        let base = Path::new("/repo");
        assert_eq!(rel_for_match(path, base), PathBuf::from("/other/a.rs"));
    }

    #[test]
    fn push_match_string_strips_base_prefix() {
        let mut out = Vec::new();
        push_match_string(&mut out, Path::new("/repo/src/a.rs"), Path::new("/repo"));
        assert_eq!(out, vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn push_match_string_falls_back_to_absolute() {
        let mut out = Vec::new();
        push_match_string(&mut out, Path::new("/other/a.rs"), Path::new("/repo"));
        assert_eq!(out, vec!["/other/a.rs".to_string()]);
    }

    #[test]
    fn push_match_string_appends_across_calls() {
        // Confirm each call pushes one entry rather than replacing the set.
        let mut out = Vec::new();
        push_match_string(&mut out, Path::new("/repo/a.rs"), Path::new("/repo"));
        push_match_string(&mut out, Path::new("/repo/b.rs"), Path::new("/repo"));
        assert_eq!(out, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn collect_matches_sorts_alphabetically() {
        // Lay out files in non-lexical creation order; assert sorted output.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("zebra.rs"), "").expect("write");
        std::fs::write(tmp.path().join("alpha.rs"), "").expect("write");
        std::fs::write(tmp.path().join("mike.rs"), "").expect("write");
        let ov = matcher("*.rs");
        let got = collect_matches(tmp.path(), &ov);
        assert_eq!(
            got,
            vec![
                "alpha.rs".to_string(),
                "mike.rs".to_string(),
                "zebra.rs".to_string()
            ],
            "must be sorted"
        );
    }

    #[test]
    fn collect_matches_returns_relative_paths() {
        // Entries outside `base` (a symlinked-in file or a path from outside)
        // would fall back to absolute; here we assert the common-case relative
        // form for files directly under base.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/main.rs"), "").expect("write");
        std::fs::write(tmp.path().join("top.rs"), "").expect("write");
        let ov = matcher("**/*.rs");
        let mut got = collect_matches(tmp.path(), &ov);
        got.sort();
        assert_eq!(
            got,
            vec!["src/main.rs".to_string(), "top.rs".to_string()],
            "paths must be relative to base"
        );
    }

    #[test]
    fn collect_matches_empty_dir_returns_empty() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let ov = matcher("**/*.rs");
        assert!(collect_matches(tmp.path(), &ov).is_empty());
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
    clippy::indexing_slicing
)]
mod integration_tests {
    use super::*;
    use crate::context::RunnerContext;
    use crate::runtime::RuntimeConfig;
    use crate::state::SessionState;
    use loopctl::tool::ToolContext;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Build a `ToolContext` carrying a `RunnerContext` whose cwd is `dir`.
    ///
    /// Mirrors the harness the other tools' tests use so the cwd-extension
    /// plumbing is exercised identically across the crate.
    fn ctx_in(dir: &std::path::Path) -> ToolContext {
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

    /// Write `contents` to `dir/rel` (creating parent dirs as needed).
    fn write_file(dir: &std::path::Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, contents).expect("write fixture file");
    }

    #[tokio::test]
    async fn happy_path_recursive_under_src() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "src/main.rs", "");
        write_file(tmp.path(), "src/util.rs", "");
        write_file(tmp.path(), "README.md", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "src/**/*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let parsed: Vec<String> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(
            parsed,
            vec!["src/main.rs".to_string(), "src/util.rs".to_string()]
        );
    }

    #[tokio::test]
    async fn extension_only_matches_basename_anywhere() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "top.rs", "");
        write_file(tmp.path(), "src/nested.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let parsed: Vec<String> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(
            parsed,
            vec!["src/nested.rs".to_string(), "top.rs".to_string()],
            "must be sorted, both depths match"
        );
    }

    #[tokio::test]
    async fn recursive_star_matches_all_depths() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "");
        write_file(tmp.path(), "b/c.rs", "");
        write_file(tmp.path(), "d/e/f.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "**/*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<String> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed.len(), 3, "all three depths: {parsed:?}");
        assert!(parsed.contains(&"a.rs".to_string()));
        assert!(parsed.contains(&"b/c.rs".to_string()));
        assert!(parsed.contains(&"d/e/f.rs".to_string()));
    }

    #[tokio::test]
    async fn no_matches_is_success_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "**/*.nonexistent"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "empty match is not an error");
        assert!(
            out.text_content()
                .contains("No files found matching pattern: **/*.nonexistent"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn gitignore_is_respected() {
        let tmp = tempfile::TempDir::new().unwrap();
        // ignore honors .gitignore only inside a git work tree; create a
        // .git/ marker so the walker treats this dir as a repo root.
        std::fs::create_dir_all(tmp.path().join(".git")).expect("create .git marker");
        write_file(tmp.path(), ".gitignore", "ignored.rs\n");
        write_file(tmp.path(), "ignored.rs", "");
        write_file(tmp.path(), "kept.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "**/*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            !text.contains("ignored.rs"),
            "gitignored file absent: {text}"
        );
        assert!(text.contains("kept.rs"), "non-ignored file present: {text}");
    }

    #[tokio::test]
    async fn always_exclude_respected_without_git() {
        // No .git, no .gitignore — but target/ still must be excluded by the
        // always-exclude Override list.
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "target/debug/foo.rs", "");
        write_file(tmp.path(), "main.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "**/*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(!text.contains("target"), "always-exclude hit: {text}");
        assert!(text.contains("main.rs"), "{text}");
    }

    #[tokio::test]
    async fn dchignore_is_respected() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), ".dchignore", "secret.rs\n");
        write_file(tmp.path(), "secret.rs", "");
        write_file(tmp.path(), "public.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "**/*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            !text.contains("secret.rs"),
            ".dchignore'd file absent: {text}"
        );
        assert!(text.contains("public.rs"), "{text}");
    }

    #[tokio::test]
    async fn path_defaults_to_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "");
        write_file(tmp.path(), "b.txt", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "*.txt", "path": "."});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<String> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed.len(), 2, "{parsed:?}");
    }

    #[tokio::test]
    async fn path_is_a_subdirectory() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "src/a.rs", "");
        write_file(tmp.path(), "other/b.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "*.rs", "path": "src"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("other"), "{text}");
    }

    #[tokio::test]
    async fn url_path_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "*.rs", "path": "https://example.com/y"});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn missing_pattern_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "missing pattern is InvalidInput: {err:?}"
        );
    }

    #[tokio::test]
    async fn invalid_glob_pattern_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "[unclosed"});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("Invalid glob pattern")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn results_are_sorted() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Create out of lexical order; assert the result is sorted.
        write_file(tmp.path(), "zebra.rs", "");
        write_file(tmp.path(), "alpha.rs", "");
        write_file(tmp.path(), "mike.rs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<String> = serde_json::from_str(&out.text_content()).unwrap();
        let mut expected = parsed.clone();
        expected.sort();
        assert_eq!(parsed, expected, "result must be sorted alphabetically");
    }

    #[tokio::test]
    async fn empty_directory_no_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "**/*"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "empty dir is a success message");
        assert!(
            out.text_content().contains("No files found"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn character_class_integration() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "");
        write_file(tmp.path(), "a.ss", "");
        write_file(tmp.path(), "a.cs", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "*.[rs]s"});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<String> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed, vec!["a.rs".to_string(), "a.ss".to_string()]);
    }

    #[tokio::test]
    async fn brace_expansion_integration() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "");
        write_file(tmp.path(), "b.toml", "");
        write_file(tmp.path(), "c.json", "");
        let tool = GlobTool;
        let ctx = ctx_in(tmp.path());
        let input = json!({"pattern": "*.{rs,toml}"});
        let out = tool.call(input, &ctx).await.unwrap();
        let parsed: Vec<String> = serde_json::from_str(&out.text_content()).unwrap();
        assert_eq!(parsed, vec!["a.rs".to_string(), "b.toml".to_string()]);
    }

    #[test]
    fn trait_contract_and_registry() {
        let tool = GlobTool;
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.name(), "Glob");
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("Glob").is_some(), "Glob registered");
    }
}
