//! The `Tree` tool — visual directory-tree rendering with depth limits.
//!
//! Renders a Unicode box-drawing tree (directories before files within each
//! level) up to `max_depth`, optionally including files and/or filtering them
//! by a glob `pattern`. Appends an `N directories, M files` summary. Skips
//! `.gitignore`'d paths via the shared gitignore-aware walker.

use std::collections::HashMap;
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
use crate::walk::WalkEntry;
use crate::walk::matches_any_glob;
use crate::walk::walk_entries;

/// Default maximum depth when the caller omits `max_depth`.
const DEFAULT_MAX_DEPTH: usize = 3;

/// Floor for a caller-supplied `max_depth`; values below this are raised.
const MIN_MAX_DEPTH: usize = 1;

/// Ceiling for a caller-supplied `max_depth`; values above this are lowered.
const MAX_MAX_DEPTH: usize = 50;

/// Display directory tree structure with depth limits and filtering.
///
/// Respects `.gitignore`. Read-only and concurrency-safe — Tree only reads
/// directory metadata and never mutates the filesystem.
pub struct TreeTool;

impl Tool for TreeTool {
    fn name(&self) -> &'static str {
        "Tree"
    }

    fn description(&self) -> &'static str {
        "Display directory tree structure with depth limits and filtering. \
         Respects .gitignore."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to display (defaults to current working directory)",
                        "default": "."
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Maximum depth to display (clamped to 1-50)",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 3
                    },
                    "include_files": {
                        "type": "boolean",
                        "description": "Whether to include files (true) or only directories (false)",
                        "default": true
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to filter files (e.g., '*.rs')"
                    }
                }
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let rc = runner_ctx(ctx).cloned();
        Box::pin(self.tree_inner(input, rc))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

impl TreeTool {
    /// Body of [`Tool::call`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when no [`RunnerContext`] is installed
    /// on the [`ToolContext`] or on a filesystem I/O error during metadata
    /// checks. Returns [`ToolError::InvalidInput`] for a URL `path` or a
    /// non-directory `path`.
    async fn tree_inner(
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

        let base_path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        if is_url(base_path) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the Tree tool. Use WebFetch for URLs.".to_string(),
            ));
        }

        let max_depth = input
            .get("max_depth")
            .and_then(Value::as_u64)
            .and_then(|d| usize::try_from(d).ok())
            .unwrap_or(DEFAULT_MAX_DEPTH)
            .clamp(MIN_MAX_DEPTH, MAX_MAX_DEPTH);
        let include_files = input
            .get("include_files")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let pattern = input.get("pattern").and_then(Value::as_str);
        let full_path = resolve_path(base_path, &cwd)?;

        if !tokio::fs::try_exists(&full_path)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?
        {
            return Ok(ToolOutput::error_text(format!(
                "Path does not exist: {base_path}"
            )));
        }

        let metadata = tokio::fs::metadata(&full_path)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        if !metadata.is_dir() {
            return Err(ToolError::InvalidInput(format!(
                "Path is not a directory: {base_path}"
            )));
        }

        let entries = walk_entries(&full_path, Some(max_depth));
        let entries_were_empty = entries.is_empty();
        let filtered = filter_entries(entries, &full_path, include_files, pattern);

        if filtered.is_empty() {
            let message = if entries_were_empty {
                format!("Empty directory: {base_path}")
            } else {
                format!("No matching entries in: {base_path}")
            };
            return Ok(ToolOutput::error_text(message));
        }

        let tree = format_tree(&full_path, base_path, &filtered);
        let summary = format_summary(&filtered);

        Ok(ToolOutput::text(format!("{tree}\n\n{summary}")))
    }
}

/// Filter the walker's entries by `include_files` and the optional `pattern`.
///
/// Directories are always kept (the `pattern` filter is files-only). When
/// `pattern` contains a `/`, it matches against the path relative to `root`;
/// otherwise it matches the filename only.
fn filter_entries(
    entries: Vec<WalkEntry>,
    root: &Path,
    include_files: bool,
    pattern: Option<&str>,
) -> Vec<WalkEntry> {
    let pat_vec: Vec<String> = pattern.map(|p| vec![p.to_string()]).unwrap_or_default();
    entries
        .into_iter()
        .filter(|e| {
            if e.is_dir {
                return true;
            }
            if !include_files {
                return false;
            }
            if pat_vec.is_empty() {
                return true;
            }
            if pattern.is_some_and(|p| p.contains('/')) {
                let rel = e
                    .path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|r| r.to_str())
                    .unwrap_or("");
                matches_any_glob(rel, &pat_vec)
            } else {
                let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                matches_any_glob(name, &pat_vec)
            }
        })
        .collect()
}

/// Render `entries` as a Unicode box-drawing tree rooted at `root`.
///
/// Directories are listed before files within each level, both groups sorted
/// alphabetically. Uses `├── `, `└── `, `│   `, and `    ` connectors — the
/// standard `tree(1)`-style output the model expects.
fn format_tree(root: &Path, display_name: &str, entries: &[WalkEntry]) -> String {
    let mut children_by_parent: HashMap<PathBuf, Vec<&WalkEntry>> = HashMap::new();
    for e in entries {
        if let Some(parent) = e.path.parent() {
            children_by_parent
                .entry(parent.to_path_buf())
                .or_default()
                .push(e);
        }
    }
    for children in children_by_parent.values_mut() {
        children.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
        });
    }

    let mut output = String::new();
    output.push_str(display_name);
    output.push('/');

    let root_children: &[&WalkEntry] = children_by_parent.get(root).map_or(&[], Vec::as_slice);
    render_children(root_children, &children_by_parent, "", &mut output);
    output
}

/// Recursively render one level of the tree.
///
/// Appends each child with the correct connector (`├── ` or `└── `) and
/// indentation prefix (`│   ` or `    `), recursing into directories.
fn render_children(
    children: &[&WalkEntry],
    children_by_parent: &HashMap<PathBuf, Vec<&WalkEntry>>,
    prefix: &str,
    output: &mut String,
) {
    let count = children.len();
    for (i, child) in children.iter().enumerate() {
        let is_last = i == count.saturating_sub(1);
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = if is_last { "    " } else { "│   " };
        let name = child
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?");

        output.push('\n');
        output.push_str(prefix);
        output.push_str(connector);
        output.push_str(name);
        if child.is_dir {
            output.push('/');
            let sub_children: &[&WalkEntry] = children_by_parent
                .get(&child.path)
                .map_or(&[], Vec::as_slice);
            if !sub_children.is_empty() {
                let new_prefix = format!("{prefix}{child_prefix}");
                render_children(sub_children, children_by_parent, &new_prefix, output);
            }
        }
    }
}

/// Build the `N director(y|ies), M file(s)` summary line.
///
/// Singularizes both nouns when the count is exactly one; otherwise uses
/// the plural forms. The format and pluralization rules are part of the
/// tool's output contract, ported verbatim from the source.
fn format_summary(filtered: &[WalkEntry]) -> String {
    let dir_count = filtered.iter().filter(|e| e.is_dir).count();
    let file_count = filtered.iter().filter(|e| !e.is_dir).count();
    let dir_word = if dir_count == 1 {
        "directory"
    } else {
        "directories"
    };
    let file_word = if file_count == 1 { "file" } else { "files" };
    format!("{dir_count} {dir_word}, {file_count} {file_word}")
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

    #[test]
    fn format_summary_singular() {
        let one_of_each = vec![
            WalkEntry {
                path: PathBuf::from("/d"),
                is_dir: true,
            },
            WalkEntry {
                path: PathBuf::from("/f"),
                is_dir: false,
            },
        ];
        assert_eq!(format_summary(&one_of_each), "1 directory, 1 file");
    }

    #[test]
    fn format_summary_plural() {
        let two_dirs_three_files = vec![
            WalkEntry {
                path: PathBuf::from("/d1"),
                is_dir: true,
            },
            WalkEntry {
                path: PathBuf::from("/d2"),
                is_dir: true,
            },
            WalkEntry {
                path: PathBuf::from("/f1"),
                is_dir: false,
            },
            WalkEntry {
                path: PathBuf::from("/f2"),
                is_dir: false,
            },
            WalkEntry {
                path: PathBuf::from("/f3"),
                is_dir: false,
            },
        ];
        assert_eq!(
            format_summary(&two_dirs_three_files),
            "2 directories, 3 files"
        );
    }

    #[test]
    fn format_summary_zero() {
        assert_eq!(format_summary(&[]), "0 directories, 0 files");
    }

    #[test]
    fn format_tree_empty_entries() {
        let root = Path::new("/repo");
        let out = format_tree(root, ".", &[]);
        assert_eq!(out, "./");
    }

    #[test]
    fn format_tree_dirs_before_files() {
        let entries = vec![
            WalkEntry {
                path: PathBuf::from("/repo/file_a"),
                is_dir: false,
            },
            WalkEntry {
                path: PathBuf::from("/repo/dir_b"),
                is_dir: true,
            },
            WalkEntry {
                path: PathBuf::from("/repo/file_c"),
                is_dir: false,
            },
            WalkEntry {
                path: PathBuf::from("/repo/dir_a"),
                is_dir: true,
            },
        ];
        let out = format_tree(Path::new("/repo"), ".", &entries);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "./");
        assert!(lines[1].contains("dir_a/"), "{}", lines[1]);
        assert!(lines[2].contains("dir_b/"), "{}", lines[2]);
        assert!(lines[3].contains("file_a"), "{}", lines[3]);
        assert!(lines[4].contains("file_c"), "{}", lines[4]);
    }

    #[test]
    fn format_tree_last_child_connector() {
        let entries = vec![
            WalkEntry {
                path: PathBuf::from("/repo/a"),
                is_dir: true,
            },
            WalkEntry {
                path: PathBuf::from("/repo/b"),
                is_dir: false,
            },
        ];
        let out = format_tree(Path::new("/repo"), ".", &entries);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].starts_with("├──"), "first child: {}", lines[1]);
        assert!(lines[2].starts_with("└──"), "last child: {}", lines[2]);
    }

    #[test]
    fn format_tree_trailing_slash_on_dirs() {
        let entries = vec![
            WalkEntry {
                path: PathBuf::from("/repo/dir"),
                is_dir: true,
            },
            WalkEntry {
                path: PathBuf::from("/repo/file.txt"),
                is_dir: false,
            },
        ];
        let out = format_tree(Path::new("/repo"), ".", &entries);
        assert!(out.contains("dir/"), "dir should have trailing slash");
        assert!(
            !out.contains("file.txt/"),
            "file should NOT have trailing slash"
        );
    }

    #[tokio::test]
    async fn happy_path_default_depth() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b/c/d")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();
        std::fs::write(root.join("a/file.rs"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "."});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("a/"));
        assert!(text.contains("file.txt"));
        assert!(!text.contains("c/d"));
        assert!(!text.contains("d/"), "depth-4 dir must be absent: {text}");
    }

    #[tokio::test]
    async fn max_depth_1() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/deep.rs"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": ".", "max_depth": 1});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("a/"));
        assert!(!text.contains("b/"));
    }

    #[tokio::test]
    async fn include_files_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": ".", "include_files": false});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("dir/"));
        assert!(!text.contains("file.txt"));
    }

    #[tokio::test]
    async fn pattern_filter_filename() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "x").unwrap();
        std::fs::write(root.join("b.md"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": ".", "pattern": "*.rs"});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.md"), "{text}");
    }

    #[tokio::test]
    async fn files_only_dir_with_include_files_false_is_filtered_empty() {
        // A non-empty directory whose entries are all removed by filtering
        // must report "No matching entries", not the misleading "Empty
        // directory". Here the dir holds only files and include_files=false
        // strips them all while the walk genuinely found entries.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "x").unwrap();
        std::fs::write(root.join("b.md"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": ".", "include_files": false});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(
            text.contains("No matching entries"),
            "filtered-empty must be truthful: {text}"
        );
        assert!(
            !text.contains("Empty directory"),
            "must not mislabel a populated dir as empty: {text}"
        );
    }

    #[tokio::test]
    async fn pattern_matching_nothing_is_filtered_empty() {
        // Same truthfulness check via the pattern filter: a populated dir
        // whose files all fail the glob must report "No matching entries".
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "x").unwrap();
        std::fs::write(root.join("b.md"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": ".", "pattern": "*.nomatch"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(
            text.contains("No matching entries"),
            "filtered-empty must be truthful: {text}"
        );
        assert!(
            !text.contains("Empty directory"),
            "must not mislabel a populated dir as empty: {text}"
        );
    }

    #[tokio::test]
    async fn max_depth_zero_clamped_to_one() {
        // max_depth=0 is below the floor (MIN_MAX_DEPTH=1); it must clamp to
        // 1, yielding direct children of root rather than an empty "0 dirs,
        // 0 files" result.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/deep.rs"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": ".", "max_depth": 0});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(
            text.contains("a/"),
            "depth-1 child visible after clamp: {text}"
        );
        assert!(
            !text.contains("b/"),
            "depth-2 child still hidden after clamp to 1: {text}"
        );
    }

    #[tokio::test]
    async fn max_depth_above_ceiling_clamps_without_panic() {
        // max_depth far above the ceiling (MAX_MAX_DEPTH=50) must not panic
        // and must behave as a deep traversal over a shallow tree.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/deep.rs"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": ".", "max_depth": 9999});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("deep.rs"), "deep file visible: {text}");
    }

    #[tokio::test]
    async fn missing_path_is_soft_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "nope"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("does not exist"));
    }

    #[tokio::test]
    async fn file_not_directory_is_invalid_input() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "f.txt"});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("not a directory")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn url_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "https://example.com/x"});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn empty_directory_is_soft_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = tmp.path().join("empty");
        std::fs::create_dir(&sub).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "empty"});
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("Empty directory"));
    }

    #[tokio::test]
    async fn summary_counts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("d1")).unwrap();
        std::fs::create_dir_all(root.join("d2")).unwrap();
        std::fs::write(root.join("f1.txt"), "x").unwrap();
        std::fs::write(root.join("f2.txt"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "."});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("2 directories, 2 files"), "{text}");
    }

    #[tokio::test]
    async fn gitignore_respected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::write(root.join("ignored.log"), "x").unwrap();
        std::fs::write(root.join("kept.txt"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "."});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            !text.contains("ignored.log"),
            "gitignored file should be absent: {text}"
        );
        assert!(text.contains("kept.txt"), "{text}");
    }

    #[tokio::test]
    async fn always_exclude_respected_without_gitignore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/foo"), "x").unwrap();
        std::fs::write(root.join("main.rs"), "x").unwrap();

        let cwd = root.to_str().unwrap();
        let tool = TreeTool;
        let ctx = ctx_in(cwd);
        let input = json!({"path": "."});
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            !text.contains("target"),
            "target/ should be excluded: {text}"
        );
        assert!(text.contains("main.rs"), "{text}");
    }

    #[test]
    fn trait_contract_and_registry() {
        let tool = TreeTool;
        assert_eq!(tool.name(), "Tree");
        assert!(tool.is_read_only());
        assert!(tool.is_concurrency_safe());
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("Tree").is_some(), "TreeTool registered");
    }
}
