//! The Write tool — writes content to a file, after syntax validation and a
//! staleness check against the path's last-recorded read.

use std::path::Path;

use loopctl::Tool;
use loopctl::tool::DisplayHint;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::context::RunnerContext;
use crate::context::require_cwd;
use crate::context::runner_ctx;
use crate::diff::format_file_change;
use crate::linter::LinterResult;
use crate::linter::lint_content;
use crate::util::ResolvePolicy;
use crate::util::canonicalize_existing;
use crate::util::reject_url;
use crate::util::resolve_path;

/// Input for the Write tool.
///
/// Writes content to a file, creating parent directories as needed and
/// running the linter gate on supported file types before the atomic write.
#[derive(Default, Deserialize, Serialize, Tool)]
#[tool(
    name = "Write",
    system_prompt = "Use Write for new files or full rewrites; prefer Edit for \
             targeted changes. The linter runs automatically on supported \
             types — fix reported errors.",
    description = "Write content to a file. Syntax validation is automatically performed \
         for supported file types (.rs, .json, .py, .js, .ts, etc.)"
)]
pub struct WriteInput {
    /// The path to the file to write.
    ///
    /// May be absolute or relative; relative paths are resolved against the
    /// runner's working directory. Parent directories are created as needed,
    /// and an existing file is overwritten in full — prefer Edit for targeted
    /// changes to an existing file. URLs are rejected.
    file_path: String,

    /// The content to write.
    ///
    /// The complete new contents of the file, not a patch or fragment. The
    /// linter gate validates it for supported file types before the write
    /// happens, so malformed syntax is blocked rather than written to disk.
    content: String,

    /// Skip syntax validation (not recommended).
    ///
    /// When `true`, the linter gate is bypassed and the file is written even
    /// if the content has syntax errors. Defaults to `false`; the gate exists
    /// to prevent file corruption from malformed output.
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_linter: Option<bool>,
}

impl WriteInput {
    /// Serializes the typed input and delegates to `write_inner`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the input cannot be serialized back to JSON
    /// or when `write_inner` fails.
    async fn run(&self, input: Self, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let rc = runner_ctx(ctx).cloned();
        let value = serde_json::to_value(&input)
            .map_err(|e| ToolError::Execution(format!("serialize tool input: {e}")))?;
        self.write_inner(value, rc).await
    }

    /// Body of [`Tool::call`].
    ///
    /// Orchestrates validate → lint → staleness check → write. When the path
    /// has a recorded baseline (a prior Read this session), content that
    /// differs from the recorded hash refuses the write as a soft conflict;
    /// the compared file's identity is pinned and re-checked at the rename,
    /// so a target swapped in between aborts instead of replacing an
    /// uncompared file. Under the contained policy, missing parent
    /// directories are created through a walk that never follows symbolic
    /// links. A successful write refreshes the recorded baseline, so the
    /// model's own write never registers as a later external change.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] for a missing `RunnerContext`, a missing
    /// `file_path`, a missing `content`, a URL `file_path` or a path escaping
    /// the working directory, a target that changed while the write was
    /// being prepared, or a file-system error during parent creation or the
    /// atomic write.
    async fn write_inner(
        &self,
        input: Value,
        rc: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let policy = match rc.as_ref() {
            Some(context) => context.resolve_policy,
            None => ResolvePolicy::Contained,
        };
        let cwd = require_cwd(rc.clone())?;
        let file_path = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing file_path".to_string()))?;
        reject_url("Write", file_path)?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing content".to_string()))?;
        let skip_linter = input
            .get("skip_linter")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut full_path = resolve_path(file_path, &cwd, policy)?;
        if policy == ResolvePolicy::Unrestricted {
            full_path = canonicalize_existing(&full_path)?;
        }

        if !skip_linter {
            let result = lint_content(&full_path, content);
            if !result.is_valid {
                return Ok(ToolOutput::error_text(format_lint_failure(
                    &full_path, &result,
                )));
            }
        }

        let old_content = match tokio::fs::File::open(&full_path).await.ok() {
            Some(mut file) => {
                if policy == ResolvePolicy::Contained {
                    crate::util::verify_handle_inside(&file, &cwd)?;
                }
                let mut buffer = String::new();
                match file.read_to_string(&mut buffer).await {
                    Ok(_) => Some(buffer),
                    Err(_) => None,
                }
            }
            None => None,
        };

        let mut expected = None;
        if let Some(baseline_hash) = rc.as_ref().and_then(|rc| rc.baseline_for(&full_path)) {
            match crate::conflict::check_content_hash_unchanged(baseline_hash, &full_path).await {
                Ok(identity) => expected = Some(identity),
                Err(failure) => {
                    return match failure {
                        crate::conflict::CheckFailure::Changed => Ok(ToolOutput::error_text(
                            crate::conflict::changed_message(&full_path),
                        )),
                        crate::conflict::CheckFailure::Fault(e) => Err(e),
                    };
                }
            }
        }

        // The contained write creates its parents through the pinned,
        // no-follow walk inside `atomic_write`; the unrestricted policy
        // keeps the plain path-based creation.
        if policy == ResolvePolicy::Unrestricted
            && let Some(parent) = full_path.parent()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        crate::fs::atomic_write(&full_path, content, &cwd, policy, expected.as_ref())?;

        if let Some(rc) = &rc {
            rc.record_baseline(&full_path, crate::state::observe_bytes(content.as_bytes()));
        }

        let display_path = file_path;
        let message = format_file_change(display_path, old_content.as_deref(), content);

        Ok(ToolOutput::text(message).with_hint(DisplayHint::Diff))
    }
}

/// Format a [`LinterResult`] failure as a human-readable message for the tool
/// output.
///
/// Shared by Write and Edit. The message is structured so the model can read
/// the error list and correct its output:
///
/// ```text
/// Syntax validation failed for src/main.rs:
///   line 12: expected expression, found `;`
/// Blocked to prevent file corruption.
/// To bypass this check, use skip_linter: true (not recommended).
/// ```
///
/// Each error is indented on its own line, prefixed with `line N:` when the
/// line number is known. The trailing two lines explain why the write did not
/// happen and how to bypass the check if the user explicitly accepts the risk.
pub(crate) fn format_lint_failure(path: &Path, result: &LinterResult) -> String {
    use std::fmt::Write;
    let mut msg = format!("Syntax validation failed for {}:\n", path.display());
    for err in &result.errors {
        match err.line {
            Some(line) => writeln!(msg, "  line {line}: {}", err.message).ok(),
            None => writeln!(msg, "  {}", err.message).ok(),
        };
    }
    msg.push_str("Blocked to prevent file corruption.\n");
    msg.push_str("To bypass this check, use skip_linter: true (not recommended).");
    msg
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
    use loopctl::tool::ToolContext;
    use serde_json::json;
    use std::path::PathBuf;

    fn ctx_in(cwd: &str) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = cwd.to_string();
        ctx.set_extension(RunnerContext::new(PathBuf::from(cwd)));
        ctx
    }

    #[tokio::test]
    async fn write_new_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "new.rs",
            "content": "fn main() { println!(\"hello\"); }\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        let written = std::fs::read_to_string(tmp.path().join("new.rs")).unwrap();
        assert!(written.contains("hello"));
        assert!(out.text_content().contains("Created: new.rs"));
        assert_eq!(out.display_hint, Some(DisplayHint::Diff));
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "sub/dir/new.rs",
            "content": "fn main() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(tmp.path().join("sub/dir/new.rs").exists());
    }

    #[tokio::test]
    async fn write_overwrites_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("existing.rs");
        std::fs::write(&target, "old content\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "existing.rs",
            "content": "fn main() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        let written = std::fs::read_to_string(&target).unwrap();
        assert_eq!(written, "fn main() {}\n");
        assert!(out.text_content().contains("Changed: existing.rs"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("script.sh");
        std::fs::write(&target, "#!/bin/bash\necho old\n").unwrap();
        // Set executable permissions (0o755).
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "script.sh",
            "content": "#!/bin/bash\necho new\n"
        });
        tool.call(input, &ctx).await.unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        // Sticky/setuid bits may vary; check the permission octal we set.
        assert_eq!(
            mode & 0o777,
            0o755,
            "permissions should be preserved as 0o755, got 0o{:o}",
            mode & 0o777
        );
    }

    #[tokio::test]
    async fn lint_failure_blocks_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "bad.rs",
            "content": "fn main() { let x = ; }"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("Syntax validation failed"));
        assert!(!tmp.path().join("bad.rs").exists());
    }

    #[tokio::test]
    async fn skip_linter_bypasses_gate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "bad.rs",
            "content": "fn main() { let x = ; }",
            "skip_linter": true
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(tmp.path().join("bad.rs").exists());
    }

    #[tokio::test]
    async fn unsupported_extension_writes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "notes.txt",
            "content": "just some text\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(tmp.path().join("notes.txt").exists());
    }

    #[tokio::test]
    async fn no_temp_file_left_on_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "clean.rs",
            "content": "fn main() {}\n"
        });
        tool.call(input, &ctx).await.unwrap();
        // No .tmp files should remain in the directory.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["clean.rs"]);
    }

    #[tokio::test]
    async fn missing_file_path_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let err = tool
            .call(json!({ "content": "x" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn missing_content_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let err = tool
            .call(json!({ "file_path": "x.rs" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn url_file_path_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let err = tool
            .call(
                json!({ "file_path": "https://example.com/page", "content": "x" }),
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
    async fn absolute_path_honored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("abs.rs");
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": target.to_str().unwrap(),
            "content": "fn main() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(target.exists());
    }

    #[tokio::test]
    async fn writetool_registered_in_builtin_registry() {
        let reg = crate::registry::builtin_registry();
        let tool = reg.get("Write").expect("Write registered");
        assert!(!tool.is_read_only());
        assert!(!tool.is_concurrency_safe());
    }

    #[test]
    fn schema_matches_spec() {
        let schema = WriteInput::default().schema();
        let props = schema
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(props.contains_key("file_path"));
        assert!(props.contains_key("content"));
        assert!(props.contains_key("skip_linter"));
        let required = schema
            .input_schema
            .get("required")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(required.len(), 2);
    }

    #[tokio::test]
    async fn write_accepts_a_dotdot_workspace_spelling() {
        // The runner anchors the workspace lexically: a `..` in the
        // spelling is collapsed at construction so the pinned write's
        // prefix matching compares against the resolved form — the write
        // lands in the real workspace instead of failing with a misleading
        // "not inside" error.
        let tmp = tempfile::TempDir::new().unwrap();
        let spelling = format!("{}/sub/..", tmp.path().to_str().unwrap());
        let mut ctx = ToolContext::default();
        ctx.cwd = spelling.clone();
        ctx.set_extension(RunnerContext::new(PathBuf::from(spelling)));

        let tool = WriteInput::default();
        let input = json!({
            "file_path": "landed.rs",
            "content": "fn main() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("landed.rs")).unwrap(),
            "fn main() {}\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_in_workspace_symlink_to_outside_is_rejected() {
        // Writing through an in-workspace symlink that targets a file outside
        // cwd must be rejected by resolve_path before any write happens. The
        // external target must remain untouched.
        use std::os::unix::fs::symlink;
        let work = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let real = outside.path().join("real.txt");
        std::fs::write(&real, "original").unwrap();
        let link = work.path().join("link.rs");
        symlink(&real, &link).unwrap();

        let cwd = work.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({
            "file_path": "link.rs",
            "content": "fn main() {}\n"
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("symlink")),
            "expected symlink-escape rejection, got {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "original",
            "external target must be untouched"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrestricted_write_to_a_symlink_updates_the_referent_and_keeps_the_link() {
        // Unrestricted writes honor symbolic links: the content lands on the
        // real file and the link entry survives as a link.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.rs");
        let link = tmp.path().join("link.rs");
        std::fs::write(&real, "fn old() {}\n").unwrap();
        symlink(&real, &link).unwrap();

        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.set_extension(
            RunnerContext::new(tmp.path().to_path_buf())
                .with_resolve_policy(ResolvePolicy::Unrestricted),
        );
        let tool = WriteInput::default();
        let input = json!({
            "file_path": "link.rs",
            "content": "fn new() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link must survive the write"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "fn new() {}\n");
    }

    #[tokio::test]
    async fn unrestricted_write_creates_a_new_file() {
        // A path that does not resolve is not an error: new-file writes
        // must keep working under the unrestricted policy.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.set_extension(
            RunnerContext::new(tmp.path().to_path_buf())
                .with_resolve_policy(ResolvePolicy::Unrestricted),
        );
        let tool = WriteInput::default();
        let input = json!({
            "file_path": "fresh.rs",
            "content": "fn main() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("fresh.rs")).unwrap(),
            "fn main() {}\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrestricted_write_to_an_unresolvable_symlink_fails_without_severing() {
        // A link whose target cannot be resolved (a loop here) cannot be
        // honored; the write must fail rather than silently replace the
        // link entry with a regular file.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let link = tmp.path().join("loop.rs");
        symlink(&link, &link).unwrap();

        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.set_extension(
            RunnerContext::new(tmp.path().to_path_buf())
                .with_resolve_policy(ResolvePolicy::Unrestricted),
        );
        let tool = WriteInput::default();
        let input = json!({
            "file_path": "loop.rs",
            "content": "fn main() {}\n"
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("cannot resolve"), "{err}");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the unresolvable link must survive the failed write"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrestricted_write_to_a_dangling_symlink_fails_without_severing() {
        // A link to a nonexistent file is the reachable sibling of the loop
        // case: a rename onto the link path would replace the entry instead
        // of creating the referent, so the write must refuse and leave the
        // link — and the absent referent — exactly as they were.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let link = tmp.path().join("dangling.rs");
        symlink(tmp.path().join("absent.rs"), &link).unwrap();

        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.set_extension(
            RunnerContext::new(tmp.path().to_path_buf())
                .with_resolve_policy(ResolvePolicy::Unrestricted),
        );
        let tool = WriteInput::default();
        let input = json!({
            "file_path": "dangling.rs",
            "content": "fn main() {}\n"
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("cannot resolve"), "{err}");
        assert!(
            err.to_string().contains("absent.rs"),
            "the refusal must name the broken target: {err}"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the dangling link must survive the failed write"
        );
        assert!(
            !tmp.path().join("absent.rs").exists(),
            "the refused write must not create the referent"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrestricted_write_through_a_symlinked_dir_keys_the_baseline_on_the_real_path() {
        // A new file written through a symlinked directory cannot be
        // canonicalized before the write (it does not exist yet); once it
        // exists the post-write baseline must be re-keyed to the physical
        // path, so a later write through the real spelling still runs the
        // staleness guard instead of the no-baseline fallback.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let realdir = tmp.path().join("realdir");
        std::fs::create_dir(&realdir).unwrap();
        let linkdir = tmp.path().join("linkdir");
        symlink(&realdir, &linkdir).unwrap();

        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.set_extension(
            RunnerContext::new(tmp.path().to_path_buf())
                .with_resolve_policy(ResolvePolicy::Unrestricted),
        );
        let tool = WriteInput::default();
        let first = json!({
            "file_path": "linkdir/new.rs",
            "content": "fn v1() {}\n"
        });
        let out = tool.call(first, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());

        std::fs::write(realdir.join("new.rs"), "EXTERNAL\n").unwrap();

        let retry = json!({
            "file_path": "realdir/new.rs",
            "content": "fn v2() {}\n"
        });
        let out = tool.call(retry, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("changed on disk"),
            "{}",
            out.text_content()
        );
        assert_eq!(
            std::fs::read_to_string(realdir.join("new.rs")).unwrap(),
            "EXTERNAL\n",
            "the refused retry must not clobber the external content"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrestricted_conflict_guard_tracks_the_referent_through_a_symlink() {
        // Reading through a link arms the baseline on the physical file, so
        // an external change to the referent trips the write's staleness
        // check even though both operations used the link spelling.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.rs");
        let link = tmp.path().join("link.rs");
        std::fs::write(&real, "fn one() {}\n").unwrap();
        symlink(&real, &link).unwrap();

        let mut ctx = ToolContext::default();
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.set_extension(
            RunnerContext::new(tmp.path().to_path_buf())
                .with_resolve_policy(ResolvePolicy::Unrestricted),
        );

        let reader = crate::read::ReadInput::default();
        let read_out = reader
            .call(json!({ "file_path": "link.rs" }), &ctx)
            .await
            .unwrap();
        assert!(!read_out.is_error);

        std::fs::write(&real, "externally replaced\n").unwrap();

        let tool = WriteInput::default();
        let input = json!({
            "file_path": "link.rs",
            "content": "fn two() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "the changed referent must abort the write");
        assert!(
            out.text_content().contains("changed on disk"),
            "{}",
            out.text_content()
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "externally replaced\n",
            "the referent must be untouched by the refused write"
        );
    }

    #[tokio::test]
    async fn unrestricted_write_reaches_outside_the_workspace() {
        // The headline promise of the opt-out: an unrestricted write lands
        // on any path the process may touch, outside the workspace included.
        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let mut ctx = ToolContext::default();
        ctx.cwd = workspace.path().to_string_lossy().into_owned();
        ctx.set_extension(
            RunnerContext::new(workspace.path().to_path_buf())
                .with_resolve_policy(ResolvePolicy::Unrestricted),
        );
        let tool = WriteInput::default();
        let input = json!({
            "file_path": outside.path().join("out.txt").to_str().unwrap(),
            "content": "fn main() {}\n"
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("out.txt")).unwrap(),
            "fn main() {}\n"
        );
    }

    /// Drives a real Read (which records the content baseline on the shared
    /// context) before the Write. The external mutation's mtime is forced
    /// with `set_modified` so the test never depends on filesystem timestamp
    /// granularity.
    #[tokio::test]
    async fn write_refuses_to_clobber_a_file_changed_since_it_was_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("note.txt");
        std::fs::write(&target, "original\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let ctx = ctx_in(cwd);

        let read = crate::read::ReadInput::default();
        read.call(json!({ "file_path": "note.txt" }), &ctx)
            .await
            .unwrap();

        std::fs::write(&target, "EXTERNAL\n").unwrap();
        let baseline = std::fs::metadata(&target).unwrap().modified().unwrap();
        let forced = baseline + std::time::Duration::from_secs(30);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        file.set_modified(forced).unwrap();

        let tool = WriteInput::default();
        let input = json!({ "file_path": "note.txt", "content": "clobber\n" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("changed on disk"), "{text}");
        assert!(text.contains("Read"), "{text}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "EXTERNAL\n",
            "the clobbering write must not happen"
        );
    }

    #[tokio::test]
    async fn write_allows_an_existing_file_that_was_never_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("existing.txt");
        std::fs::write(&target, "old\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({ "file_path": "existing.txt", "content": "new\n" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn write_allows_an_unchanged_file_that_was_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("note.txt");
        std::fs::write(&target, "original\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let ctx = ctx_in(cwd);

        let read = crate::read::ReadInput::default();
        read.call(json!({ "file_path": "note.txt" }), &ctx)
            .await
            .unwrap();

        let tool = WriteInput::default();
        let input = json!({ "file_path": "note.txt", "content": "rewritten\n" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "rewritten\n");
    }

    #[tokio::test]
    async fn write_allows_a_brand_new_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteInput::default();
        let ctx = ctx_in(cwd);
        let input = json!({ "file_path": "fresh.txt", "content": "hello\n" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("fresh.txt")).unwrap(),
            "hello\n"
        );
    }
}
