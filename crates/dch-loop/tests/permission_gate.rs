//! The permission gate's dispatch behavior: the mode × category matrix
//! enforced through real pipeline dispatches.
//!
//! The matrix cells themselves are pinned by `dch-tools`' own tests;
//! these prove the layer maps them onto live dispatches — allowances
//! execute the tool, blocks deny with mode-and-tool text, asks resolve
//! through the resolver (or deny without one), and a cancelled run
//! denies a pending ask rather than executing it.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use dch_loop::permission_layer;
use dch_tools::permission::PermissionMode;
use dch_tools::permission::PermissionMode::{AcceptEdits, Auto, Plan};
use loopctl::cancel::CancelSignal;
use loopctl::middleware::AskResolverFn;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolPipeline;
use loopctl::tool::PermissionCheck;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolRegistry;
use loopctl::tool::ToolSchema;
use serde_json::Value;

/// A tool whose only behavior is its name: it runs and echoes it.
///
/// The gate classifies by dispatch name, so one probe shape covers
/// every category by wearing different names.
struct Probe {
    name: &'static str,
}

impl Tool for Probe {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "test probe"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name.to_string(),
            description: "test probe".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call<'a>(
        &'a self,
        _input: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>> {
        let report = format!("{} ran", self.name);
        Box::pin(async move { Ok(ToolOutput::text(report)) })
    }
}

/// A pipeline with the gate over a registry containing one probe.
///
/// The gate is the only middleware, so a dispatch through this
/// pipeline exercises exactly the layer's verdict: an allowance runs
/// the probe, a denial short-circuits before it, and an ask resolves
/// through `resolver` when one is attached.
fn gated_pipeline(
    mode: PermissionMode,
    resolver: Option<AskResolverFn>,
    probe: Probe,
) -> ToolPipeline {
    let mut registry = ToolRegistry::new();
    registry.register(probe);
    ToolPipeline::builder()
        .with_middleware(permission_layer(mode, resolver))
        .with_core(Arc::new(registry))
        .build()
        .expect("static composition builds")
}

/// A dispatch context for `tool`, carrying `cancel`.
///
/// Shaped like the contexts the engine builds — the `Allow`
/// permission the engine seeds, and a cancel signal the cancel-race
/// pin can trip before dispatching.
fn dispatch_for(tool: &str, cancel: Arc<CancelSignal>) -> ToolDispatchContext {
    ToolDispatchContext {
        tool_name: tool.to_string(),
        input: Value::Null,
        call_id: "call_probe".to_string(),
        turn_number: 0,
        cancel,
        permission: PermissionCheck::Allow,
        tool_context: ToolContext::default(),
    }
}

/// A resolver that always answers `answer`.
///
/// The consultation settles immediately, so a dispatch through it
/// resolves synchronously on the verdict the pin wants.
fn always(answer: bool) -> AskResolverFn {
    Arc::new(move |_prompt: &str, _tool: &str| Box::pin(async move { answer }))
}

/// A resolver whose answer never arrives.
///
/// Paired with a pre-tripped cancel signal, the gate's cancel race is
/// the only thing that can settle the dispatch — which is exactly
/// what the pin asserts.
fn never_resolving() -> AskResolverFn {
    Arc::new(|_prompt: &str, _tool: &str| Box::pin(std::future::pending::<bool>()))
}

/// Where a recording resolver stashes its consultations.
type Consultations = Arc<Mutex<Vec<(String, String)>>>;

/// A resolver that records what it was asked.
///
/// Returns the resolver beside the log it appends `(prompt, tool)`
/// pairs to in arrival order, so pins can assert both that a
/// consultation happened and what it carried.
fn recording() -> (AskResolverFn, Consultations) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let resolver = {
        let log = Arc::clone(&log);
        let resolver: AskResolverFn = Arc::new(move |prompt: &str, tool: &str| {
            log.lock()
                .expect("log lock")
                .push((prompt.to_string(), tool.to_string()));
            Box::pin(async { true })
        });
        resolver
    };
    (resolver, log)
}

/// A fresh, untripped cancel signal.
///
/// The plain building block `dispatch_for` takes; the cancel-race pin
/// trips its own before dispatching.
fn fresh_cancel() -> Arc<CancelSignal> {
    Arc::new(CancelSignal::new())
}

#[tokio::test]
async fn plan_denies_a_write_with_mode_and_tool_in_the_reason() {
    let pipeline = gated_pipeline(Plan, None, Probe { name: "Write" });
    let result = pipeline.invoke(dispatch_for("Write", fresh_cancel())).await;
    assert!(result.is_error, "Plan must deny a Write dispatch");
    let text = result.output.to_string();
    assert!(
        text.contains("Write") && text.contains("Plan"),
        "the denial must name the tool and the mode: {text}"
    );
}

#[tokio::test]
async fn auto_mode_passes_every_category_through() {
    for name in ["Write", "Bash", "WebFetch", "TodoWrite"] {
        let pipeline = gated_pipeline(Auto, None, Probe { name });
        let result = pipeline.invoke(dispatch_for(name, fresh_cancel())).await;
        assert!(!result.is_error, "Auto must allow {name}");
        assert!(
            result.output.to_string().contains("ran"),
            "the probe under {name} must have executed"
        );
    }
}

#[tokio::test]
async fn plan_allows_the_read_family() {
    for name in ["Read", "Glob", "LSP"] {
        let pipeline = gated_pipeline(Plan, None, Probe { name });
        let result = pipeline.invoke(dispatch_for(name, fresh_cancel())).await;
        assert!(!result.is_error, "Plan must allow the read-only {name}");
    }
}

#[tokio::test]
async fn accept_edits_allows_writes_without_consulting_the_resolver() {
    let (resolver, log) = recording();
    let pipeline = gated_pipeline(AcceptEdits, Some(resolver), Probe { name: "Write" });
    let result = pipeline.invoke(dispatch_for("Write", fresh_cancel())).await;
    assert!(!result.is_error, "AcceptEdits auto-allows file writes");
    assert!(
        log.lock().expect("log lock").is_empty(),
        "an auto-allowed cell must not consult the resolver"
    );
}

#[tokio::test]
async fn ask_cells_deny_without_a_resolver() {
    let pipeline = gated_pipeline(AcceptEdits, None, Probe { name: "Bash" });
    let result = pipeline.invoke(dispatch_for("Bash", fresh_cancel())).await;
    assert!(result.is_error, "a headless Ask must degrade to a denial");
    let text = result.output.to_string();
    assert!(
        text.contains("permission required"),
        "the denial must say permission was required: {text}"
    );
}

#[tokio::test]
async fn ask_cells_resolve_through_the_resolver() {
    let allowed = gated_pipeline(AcceptEdits, Some(always(true)), Probe { name: "Bash" });
    let result = allowed.invoke(dispatch_for("Bash", fresh_cancel())).await;
    assert!(!result.is_error, "an approving resolver lets the call run");
    assert!(result.output.to_string().contains("ran"));

    let denied = gated_pipeline(AcceptEdits, Some(always(false)), Probe { name: "Bash" });
    let result = denied.invoke(dispatch_for("Bash", fresh_cancel())).await;
    assert!(result.is_error, "a refusing resolver denies the call");
    assert!(
        result.output.to_string().contains("denied by user"),
        "the refusal must say who denied it: {}",
        result.output
    );
}

#[tokio::test]
async fn the_resolver_receives_prompt_and_tool_name() {
    let (resolver, log) = recording();
    let pipeline = gated_pipeline(AcceptEdits, Some(resolver), Probe { name: "Bash" });
    let _ = pipeline.invoke(dispatch_for("Bash", fresh_cancel())).await;
    let recorded = log.lock().expect("log lock").clone();
    assert_eq!(recorded.len(), 1, "exactly one consultation");
    let (prompt, tool) = recorded.into_iter().next().expect("one consultation");
    assert_eq!(tool, "Bash");
    assert!(
        prompt.contains("Bash") && prompt.contains("AcceptEdits"),
        "the prompt must label the tool and the mode: {prompt}"
    );
}

#[tokio::test]
async fn a_cancelled_signal_denies_a_pending_ask() {
    let pipeline = gated_pipeline(AcceptEdits, Some(never_resolving()), Probe { name: "Bash" });
    let cancel = fresh_cancel();
    cancel.cancel();
    let result = pipeline.invoke(dispatch_for("Bash", cancel)).await;
    assert!(result.is_error, "a cancelled run must not execute the tool");
    let text = result.output.to_string();
    assert!(
        text.contains("cancelled while awaiting approval"),
        "the denial must name the cancellation: {text}"
    );
}

#[tokio::test]
async fn unknown_tools_fail_closed_through_the_layer() {
    let blocked = gated_pipeline(
        Plan,
        None,
        Probe {
            name: "DefinitelyNotATool",
        },
    );
    let result = blocked
        .invoke(dispatch_for("DefinitelyNotATool", fresh_cancel()))
        .await;
    assert!(result.is_error, "Plan must deny an unclassified tool");

    let allowed = gated_pipeline(
        Auto,
        None,
        Probe {
            name: "DefinitelyNotATool",
        },
    );
    let result = allowed
        .invoke(dispatch_for("DefinitelyNotATool", fresh_cancel()))
        .await;
    assert!(!result.is_error, "Auto remains the only mode that runs one");
}
