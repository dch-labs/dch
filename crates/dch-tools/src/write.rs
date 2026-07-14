//! The Write tool — writes content to a file with syntax validation.

use std::future::Future;
use std::io::Write;
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
use crate::diff::format_file_change;
use crate::linter::LinterResult;
use crate::linter::lint_content;

/// Write content to a file. Syntax validation is automatically performed for
/// supported file types (.rs, .json, .py, .js, .ts, etc.).
///
/// Not concurrency-safe and not read-only: two concurrent writes to the same
/// path would race, and the tool mutates the filesystem.
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file. Syntax validation is automatically performed \
         for supported file types (.rs, .json, .py, .js, .ts, etc.)"
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
                        "description": "The path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write"
                    },
                    "skip_linter": {
                        "type": "boolean",
                        "description": "Skip syntax validation (not recommended)",
                        "default": false
                    }
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let rc = runner_ctx(ctx).cloned();
        Box::pin(self.write_inner(input, rc))
    }

    fn system_prompt(&self) -> Option<String> {
        Some(
            "Use Write for new files or full rewrites; prefer Edit for \
             targeted changes. The linter runs automatically on supported \
             types — fix reported errors."
                .to_string(),
        )
    }
}

impl WriteTool {
    /// Body of [`Tool::call`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] for a missing `RunnerContext`, a missing
    /// `file_path`, a missing `content`, or a file-system error during the
    /// atomic write.
    async fn write_inner(
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
        let file_path = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing file_path".to_string()))?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing content".to_string()))?;
        let skip_linter = input
            .get("skip_linter")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = Path::new(file_path);
        let full_path = if path.is_relative() {
            cwd.join(path)
        } else {
            path.to_path_buf()
        };

        if !skip_linter {
            let result = lint_content(&full_path, content);
            if !result.is_valid {
                return Ok(ToolOutput::error_text(format_lint_failure(
                    &full_path, &result,
                )));
            }
        }

        let old_content = tokio::fs::read_to_string(&full_path).await.ok();
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        atomic_write(&full_path, content)?;

        if let Some(rc) = &rc
            && let Ok(mut state) = rc.session_state.lock()
        {
            state.file_read_history.push(crate::state::FileReadEntry {
                path: file_path.to_string(),
                read_at: std::time::SystemTime::now(),
            });
        }

        let display_path = file_path;
        let message = format_file_change(display_path, old_content.as_deref(), content);

        Ok(ToolOutput::text(message))
    }
}

/// Write `content` to `target` atomically using a temp file in the same
/// directory, then rename.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] on any failure.
fn atomic_write(target: &Path, content: &str) -> Result<(), ToolError> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| ToolError::Execution(format!("Failed to create temp file: {e}")))?;

    if let Ok(meta) = std::fs::metadata(target) {
        let perms = meta.permissions();
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| ToolError::Execution(format!("Failed to set permissions: {e}")))?;
    }

    tmp.write_all(content.as_bytes())
        .map_err(|e| ToolError::Execution(format!("Failed to write temp file: {e}")))?;
    tmp.flush()
        .map_err(|e| ToolError::Execution(format!("Failed to flush temp file: {e}")))?;
    tmp.persist(target)
        .map_err(|e| ToolError::Execution(format!("Failed to persist file: {e}")))?;

    Ok(())
}

/// Format a [`LinterResult`] failure as a human-readable message for the tool
/// output.
///
/// The message is structured so the model can read the error list and correct
/// its output:
///
/// ```text
/// Syntax validation failed for src/main.rs:
///   line 12: expected expression, found `;`
/// Write blocked to prevent file corruption.
/// To bypass this check, use skip_linter: true (not recommended).
/// ```
///
/// Each error is indented on its own line, prefixed with `line N:` when the
/// line number is known. The trailing two lines explain why the write did not
/// happen and how to bypass the check if the user explicitly accepts the risk.
fn format_lint_failure(path: &Path, result: &LinterResult) -> String {
    use std::fmt::Write;
    let mut msg = format!("Syntax validation failed for {}:\n", path.display());
    for err in &result.errors {
        match err.line {
            Some(line) => writeln!(msg, "  line {line}: {}", err.message).ok(),
            None => writeln!(msg, "  {}", err.message).ok(),
        };
    }
    msg.push_str("Write blocked to prevent file corruption.\n");
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
    use crate::runtime::RuntimeConfig;
    use crate::state::SessionState;
    use loopctl::tool::ToolContext;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

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

    #[tokio::test]
    async fn write_new_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteTool;
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
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteTool;
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
        let tool = WriteTool;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("script.sh");
        std::fs::write(&target, "#!/bin/bash\necho old\n").unwrap();
        // Set executable permissions (0o755).
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteTool;
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
        let tool = WriteTool;
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
        let tool = WriteTool;
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
        let tool = WriteTool;
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
        let tool = WriteTool;
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
        let tool = WriteTool;
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
        let tool = WriteTool;
        let ctx = ctx_in(cwd);
        let err = tool
            .call(json!({ "file_path": "x.rs" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn absolute_path_honored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("abs.rs");
        let cwd = tmp.path().to_str().unwrap();
        let tool = WriteTool;
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
        let tool = reg.get("Write").expect("WriteTool registered");
        assert!(!tool.is_read_only());
        assert!(!tool.is_concurrency_safe());
    }

    #[test]
    fn schema_matches_spec() {
        let schema = WriteTool.schema();
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
}
