//! The dispatch-time boundary between the overwrite guard and the
//! write tool's syntax gate.
//!
//! The guard sits outside the tool in the pipeline, so a Write that
//! both carries unfit content and targets redaction-hidden bytes meets
//! the guard's refusal first; with a target the redactor serves
//! verbatim, the same call proceeds to the tool and meets the linter
//! refusal instead. Both directions are pinned because the ordering
//! claim ("the guard must sit directly above the scrubbing pass, under
//! every host layer") is only testable through the refusal a call sees.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default
)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dch_loop::redaction_guard::with_redaction;
use loopctl::middleware::ToolDispatchContext;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolRegistry;
use loopctl::tool::builtin::fs::ContentValidator;
use loopctl::tool::builtin::fs::FileSession;
use loopctl::tool::builtin::fs::ValidationDiagnostic;
use loopctl::tool::builtin::fs::WriteTool;
use serde_json::json;

struct UnfitGate;

impl ContentValidator for UnfitGate {
    fn validate<'a>(
        &'a self,
        _path: &'a Path,
        content: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ValidationDiagnostic>> + Send + 'a>>
    {
        Box::pin(async move {
            if content.contains("BAD") {
                vec![ValidationDiagnostic {
                    line: None,
                    message: "syntax probe refusal".to_string(),
                }]
            } else {
                Vec::new()
            }
        })
    }
}

/// A pipeline of the redaction pair over a Write tool gated by [`UnfitGate`].
fn guarded_pipeline() -> loopctl::middleware::ToolPipeline {
    let mut registry = ToolRegistry::new();
    registry.register(
        WriteTool::new().with_validator(Arc::new(UnfitGate) as Arc<dyn ContentValidator>),
    );
    with_redaction(loopctl::middleware::ToolPipeline::builder())
        .with_core(Arc::new(registry))
        .build()
        .expect("static composition builds")
}

/// A dispatch context wired with the session and cwd the tool resolves under.
fn write_dispatch(cwd: &Path, file_path: &str, content: &str) -> ToolDispatchContext {
    let mut tool_context = ToolContext::default();
    tool_context.cwd = cwd.to_string_lossy().into_owned();
    tool_context.set_extension(FileSession::new(cwd.to_path_buf()));
    ToolDispatchContext {
        tool_name: "Write".to_string(),
        input: json!({"file_path": file_path, "content": content}),
        call_id: "call_boundary".to_string(),
        turn_number: 0,
        cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
        permission: loopctl::tool::PermissionCheck::Allow,
        tool_context,
    }
}

/// Seeds a target the curated patterns scrub; assembled at runtime so no
/// secret-shaped literal appears in source or test output.
fn seed_scrubbed(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let key: String = ["api_", "key"].concat();
    let value: String = "abcdefghijkl".chars().chain("mnop".chars()).collect();
    std::fs::write(&path, format!("{key}={value}\nport = 8080\n")).expect("seed target");
    path
}

#[tokio::test]
async fn unfit_content_over_a_clean_target_meets_the_syntax_refusal() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("bad.rs"), "fn main() { OK }\n").expect("seed target");
    let pipeline = guarded_pipeline();
    let mut ctx = write_dispatch(dir.path(), "bad.rs", "fn main() { BAD }");
    let result = pipeline.dispatch(&mut ctx).await;
    let text = result.output.to_string();
    assert!(result.is_error, "the unfit content must not land: {text}");
    assert!(
        text.contains("validation failed"),
        "the linter's refusal answers a clean-target call: {text}"
    );
}

#[tokio::test]
async fn unfit_content_over_a_scrubbed_target_meets_the_guard_refusal_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = seed_scrubbed(dir.path(), "bad.rs");
    let pipeline = guarded_pipeline();
    let mut ctx = write_dispatch(dir.path(), "bad.rs", "fn main() { BAD }");
    let result = pipeline.dispatch(&mut ctx).await;
    let text = result.output.to_string();
    assert!(
        result.is_error,
        "the guarded target must not be written: {text}"
    );
    assert!(
        text.contains("redactor"),
        "the guard, outside the tool, answers before the syntax gate: {text}"
    );
    assert!(
        std::fs::read_to_string(&target)
            .expect("target readable")
            .contains("port = 8080"),
        "the guard's refusal moves no bytes"
    );
}
