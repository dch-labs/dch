//! The permission gate's dispatch behavior: the mode × category matrix
//! enforced through real pipeline dispatches.
//!
//! The matrix cells themselves are pinned in `dch-permissions`' tests;
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
use dch_permissions::PermissionMode;
use dch_permissions::PermissionMode::{Auto, Interactive, ReadOnly};
use loopctl::cancel::CancelSignal;
use loopctl::middleware::PermissionAsk;
use loopctl::middleware::PermissionAskResolver;
use loopctl::middleware::PermissionResolution;
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
        ToolSchema::new(
            self.name,
            "test probe",
            serde_json::json!({"type": "object"}),
        )
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
    resolver: Option<Arc<dyn PermissionAskResolver>>,
    probe: Probe,
) -> ToolPipeline {
    let registry = ToolRegistry::new();
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

/// A dispatch context for `tool` carrying `input`, so a probe can
/// arrive shaped like a real call.
///
/// The shell carve-out reads the `command` field, so a shell verdict
/// needs a dispatch that carries one; everything else mirrors
/// [`dispatch_for`].
fn dispatch_for_input(tool: &str, input: Value, cancel: Arc<CancelSignal>) -> ToolDispatchContext {
    ToolDispatchContext {
        tool_name: tool.to_string(),
        input,
        call_id: "call_probe".to_string(),
        turn_number: 0,
        cancel,
        permission: PermissionCheck::Allow,
        tool_context: ToolContext::default(),
    }
}

/// A resolver that answers every ask with a fixed resolution.
///
/// The consultation settles immediately, so a dispatch through it
/// resolves synchronously on the verdict the pin wants.
struct Fixed {
    resolution: PermissionResolution,
}

impl PermissionAskResolver for Fixed {
    fn resolve<'a>(
        &'a self,
        _ask: &'a PermissionAsk,
    ) -> Pin<Box<dyn Future<Output = PermissionResolution> + Send + 'a>> {
        let answer = self.resolution.clone();
        Box::pin(async move { answer })
    }
}

/// A resolver whose answer never arrives.
///
/// Paired with a pre-tripped cancel signal, the gate's cancel race is
/// the only thing that can settle the dispatch — which is exactly
/// what the pin asserts.
struct Hanging;

impl PermissionAskResolver for Hanging {
    fn resolve<'a>(
        &'a self,
        _ask: &'a PermissionAsk,
    ) -> Pin<Box<dyn Future<Output = PermissionResolution> + Send + 'a>> {
        Box::pin(std::future::pending())
    }
}

/// The resolver that answers `AllowOnce` on every ask.
fn allowing() -> Arc<dyn PermissionAskResolver> {
    Arc::new(Fixed {
        resolution: PermissionResolution::AllowOnce,
    })
}

/// The resolver that answers `DenyOnce` on every ask.
fn refusing() -> Arc<dyn PermissionAskResolver> {
    Arc::new(Fixed {
        resolution: PermissionResolution::DenyOnce,
    })
}

/// One recorded ask: every field the typed seam guarantees.
#[derive(Clone)]
struct SeenAsk {
    tool_name: String,
    prompt: String,
    input: Value,
    call_id: String,
    turn_number: usize,
}

/// Where a recording resolver keeps the asks it has seen.
type Consultations = Arc<Mutex<Vec<SeenAsk>>>;

/// A resolver that records every ask it sees and answers `AllowOnce`.
///
/// Returns the resolver beside the log it appends to in arrival order,
/// so pins can assert both that a consultation happened and what the
/// typed seam carried.
fn recording() -> (Arc<dyn PermissionAskResolver>, Consultations) {
    let log: Consultations = Arc::new(Mutex::new(Vec::new()));
    let wrapped: Arc<dyn PermissionAskResolver> = Arc::new(Recording {
        inner: Arc::new(Fixed {
            resolution: PermissionResolution::AllowOnce,
        }),
        log: Arc::clone(&log),
    });
    (wrapped, log)
}

/// A resolver that logs each ask, then delegates the answer.
struct Recording {
    inner: Arc<dyn PermissionAskResolver>,
    log: Consultations,
}

impl PermissionAskResolver for Recording {
    fn resolve<'a>(
        &'a self,
        ask: &'a PermissionAsk,
    ) -> Pin<Box<dyn Future<Output = PermissionResolution> + Send + 'a>> {
        if let Ok(mut log) = self.log.lock() {
            log.push(SeenAsk {
                tool_name: ask.tool_name.clone(),
                prompt: ask.prompt.clone(),
                input: ask.input.clone(),
                call_id: ask.call_id.clone(),
                turn_number: ask.turn_number,
            });
        }
        self.inner.resolve(ask)
    }
}

/// A fresh, untripped cancel signal.
///
/// The plain building block `dispatch_for` takes; the cancel-race pin
/// trips its own before dispatching.
fn fresh_cancel() -> Arc<CancelSignal> {
    Arc::new(CancelSignal::new())
}

#[tokio::test]
async fn read_only_denies_a_write_with_mode_and_tool_in_the_reason() {
    let pipeline = gated_pipeline(ReadOnly, None, Probe { name: "Write" });
    let result = pipeline.invoke(dispatch_for("Write", fresh_cancel())).await;
    assert!(result.is_error, "Read-only must deny a Write dispatch");
    let text = result.output.to_string();
    assert!(
        text.contains("Write") && text.contains("ReadOnly"),
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
async fn read_only_allows_the_read_family() {
    for name in ["Read", "ReadFiles", "Glob", "LSP"] {
        let pipeline = gated_pipeline(ReadOnly, None, Probe { name });
        let result = pipeline.invoke(dispatch_for(name, fresh_cancel())).await;
        assert!(
            !result.is_error,
            "Read-only must allow the read-only {name}"
        );
    }
}

#[tokio::test]
async fn interactive_asks_writes_through_the_resolver() {
    // Under the three-mode matrix, file writes prompt in interactive —
    // the old accept_edits auto-apply becomes an explicit allow rule.
    let (resolver, log) = recording();
    let pipeline = gated_pipeline(Interactive, Some(resolver), Probe { name: "Write" });
    let result = pipeline.invoke(dispatch_for("Write", fresh_cancel())).await;
    assert!(
        !result.is_error,
        "an approving resolver lets the interactive write run"
    );
    assert_eq!(
        log.lock().expect("log lock").len(),
        1,
        "the ask consulted the resolver exactly once"
    );
}

#[tokio::test]
async fn ask_cells_deny_without_a_resolver() {
    let pipeline = gated_pipeline(Interactive, None, Probe { name: "Bash" });
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
    let allowed = gated_pipeline(Interactive, Some(allowing()), Probe { name: "Bash" });
    let result = allowed.invoke(dispatch_for("Bash", fresh_cancel())).await;
    assert!(!result.is_error, "an approving resolver lets the call run");
    assert!(result.output.to_string().contains("ran"));

    let denied = gated_pipeline(Interactive, Some(refusing()), Probe { name: "Bash" });
    let result = denied.invoke(dispatch_for("Bash", fresh_cancel())).await;
    assert!(result.is_error, "a refusing resolver denies the call");
    assert!(
        result.output.to_string().contains("denied by user"),
        "the refusal must say who denied it: {}",
        result.output
    );
}

#[tokio::test]
async fn a_typed_refusal_reason_reaches_the_model() {
    // The typed refusal, end to end: a `DenyWithFeedback` refusal hands the
    // model the user's words as the denial reason — not a bare no it
    // can only guess at.
    let pipeline = gated_pipeline(
        Interactive,
        Some(Arc::new(Fixed {
            resolution: PermissionResolution::DenyWithFeedback("no network today".to_string()),
        })),
        Probe { name: "WebFetch" },
    );
    let result = pipeline
        .invoke(dispatch_for("WebFetch", fresh_cancel()))
        .await;
    assert!(result.is_error, "the feedback denial refuses the call");
    let text = result.output.to_string();
    assert!(
        text.contains("no network today"),
        "the model reads the user's words as the reason: {text}"
    );
    assert!(
        !text.contains("denied by user"),
        "the words replace the bare refusal, they do not accompany it: {text}"
    );
}

#[tokio::test]
async fn the_typed_resolver_receives_the_full_call() {
    let (resolver, log) = recording();
    let pipeline = gated_pipeline(Interactive, Some(resolver), Probe { name: "Bash" });
    let command = serde_json::json!({ "command": "cargo test -p dch-loop" });
    let _ = pipeline
        .invoke(dispatch_for_input("Bash", command.clone(), fresh_cancel()))
        .await;
    let recorded = log.lock().expect("log lock").clone();
    assert_eq!(recorded.len(), 1, "exactly one consultation");
    let ask = recorded.into_iter().next().expect("one consultation");
    assert_eq!(ask.tool_name, "Bash");
    assert!(
        ask.prompt.contains("Bash") && ask.prompt.contains("Interactive"),
        "the prompt must label the tool and the mode: {}",
        ask.prompt
    );
    assert_eq!(
        ask.input, command,
        "the raw call input arrives verbatim — the record's identity"
    );
    assert_eq!(
        ask.call_id, "call_probe",
        "the call id arrives for correlation"
    );
    assert_eq!(ask.turn_number, 0, "the turn arrives for correlation");
}

#[tokio::test]
async fn a_cancelled_signal_denies_a_pending_ask() {
    let pipeline = gated_pipeline(Interactive, Some(Arc::new(Hanging)), Probe { name: "Bash" });
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
async fn a_batched_read_runs_free_in_interactive_and_read_only() {
    let (resolver, log) = recording();
    let asking = gated_pipeline(Interactive, Some(resolver), Probe { name: "ReadFiles" });
    let result = asking
        .invoke(dispatch_for("ReadFiles", fresh_cancel()))
        .await;
    assert!(!result.is_error, "the batched read still runs");
    assert_eq!(
        log.lock().expect("log lock").len(),
        0,
        "reads are free in Interactive — no consultation, no prompt"
    );

    let planned = gated_pipeline(ReadOnly, None, Probe { name: "ReadFiles" });
    let result = planned
        .invoke(dispatch_for("ReadFiles", fresh_cancel()))
        .await;
    assert!(!result.is_error, "Read-only runs the read-only batch");
}

#[tokio::test]
async fn unknown_tools_fail_closed_through_the_layer() {
    let blocked = gated_pipeline(
        ReadOnly,
        None,
        Probe {
            name: "DefinitelyNotATool",
        },
    );
    let result = blocked
        .invoke(dispatch_for("DefinitelyNotATool", fresh_cancel()))
        .await;
    assert!(result.is_error, "Read-only must deny an unclassified tool");

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

#[tokio::test]
async fn read_only_runs_a_command_that_only_looks() {
    let pipeline = gated_pipeline(ReadOnly, None, Probe { name: "Bash" });
    let command = serde_json::json!({ "command": "git log --oneline" });
    let result = pipeline
        .invoke(dispatch_for_input("Bash", command, fresh_cancel()))
        .await;
    assert!(!result.is_error, "Read-only runs a command that only looks");
    assert!(
        result.output.to_string().contains("ran"),
        "the probe under the command must have executed"
    );
}

#[tokio::test]
async fn read_only_blocks_a_command_that_writes_without_asking() {
    let (resolver, log) = recording();
    let pipeline = gated_pipeline(ReadOnly, Some(resolver), Probe { name: "Bash" });
    let command = serde_json::json!({ "command": "find . -delete" });
    let result = pipeline
        .invoke(dispatch_for_input("Bash", command, fresh_cancel()))
        .await;
    assert!(result.is_error, "Read-only refuses a mutating command");
    assert!(
        log.lock().expect("log lock").is_empty(),
        "what read-only blocks, it blocks without asking"
    );
}

#[tokio::test]
async fn read_only_keeps_its_todo_list() {
    let pipeline = gated_pipeline(ReadOnly, None, Probe { name: "TodoWrite" });
    let result = pipeline
        .invoke(dispatch_for("TodoWrite", fresh_cancel()))
        .await;
    assert!(!result.is_error, "the todo tool runs under read-only");
    assert!(
        result.output.to_string().contains("ran"),
        "the probe under TodoWrite must have executed"
    );
}

/// A live gate over `rules` and `remembered`, with a `mode` cell.
///
/// The production spelling — the same builder the runner wires — so a
/// dispatch through this pipeline exercises the full layering: config
/// rules first, then the remembered approvals, then the matrix.
fn live_pipeline(
    mode: dch_config::SharedPermissionMode,
    resolver: Option<Arc<dyn PermissionAskResolver>>,
    rules: Vec<dch_permissions::Rule>,
    remembered: dch_permissions::MemoryView,
) -> ToolPipeline {
    let registry = ToolRegistry::new();
    registry.register(Probe { name: "Write" });
    registry.register(Probe { name: "Bash" });
    registry.register(Probe { name: "Mystery" });
    ToolPipeline::builder()
        .with_middleware(dch_loop::permission_layer_live(
            mode,
            resolver,
            std::sync::Arc::new(move || rules.clone()),
            std::sync::Arc::new(move || remembered.clone()),
        ))
        .with_core(Arc::new(registry))
        .build()
        .expect("static composition builds")
}

#[tokio::test]
async fn a_configured_deny_holds_even_in_auto() {
    // The config layer's tighten-first contract through a live dispatch:
    // auto's matrix allows everything, and a deny on a path glob still
    // refuses the call — one bad target is enough.
    let rules = vec![dch_permissions::Rule {
        id: "no-env-writes".to_string(),
        outcome: dch_permissions::PermissionOutcome::Block,
        matcher: dch_permissions::Matcher::PathGlob(
            dch_permissions::PathGlob::new("**/.env").expect("the glob compiles"),
        ),
    }];
    let pipeline = live_pipeline(
        Arc::new(std::sync::Mutex::new(dch_config::PermissionMode::Auto)),
        None,
        rules,
        dch_permissions::MemoryView::empty(),
    );
    let result = pipeline
        .invoke(dispatch_for_input(
            "Write",
            serde_json::json!({ "file_path": ".env" }),
            fresh_cancel(),
        ))
        .await;
    assert!(result.is_error, "the configured deny refuses the write");
    let text = result.output.to_string();
    assert!(
        text.contains("denied by configured rule no-env-writes"),
        "the denial names the rule that refused, not the mode: {text}"
    );
}

#[tokio::test]
async fn an_unclassified_tools_command_field_takes_no_reader_gate() {
    // The reader gate keys on the shell, not on any tool whose input
    // happens to carry a `command`: an Unclassified tool is fail-closed
    // (the vocabulary's own promise), so under read-only it blocks even
    // when its input spells a pure reader command.
    let pipeline = live_pipeline(
        Arc::new(std::sync::Mutex::new(dch_config::PermissionMode::ReadOnly)),
        None,
        Vec::new(),
        dch_permissions::MemoryView::empty(),
    );
    let result = pipeline
        .invoke(dispatch_for_input(
            "Mystery",
            serde_json::json!({ "command": "ls" }),
            fresh_cancel(),
        ))
        .await;
    assert!(
        result.is_error,
        "read_only blocks an Unclassified tool whatever its input spells"
    );
    let text = result.output.to_string();
    assert!(
        !text.contains("reader"),
        "the refusal comes from the matrix's fail-closed classification, \
         not a reader allowance: {text}"
    );
}

#[tokio::test]
async fn a_covered_ask_answers_on_sight_through_the_typed_resolver() {
    // The remembered-coverage pin: an Interactive gate asks for `cargo test`
    // — unless the remembered snapshot carries that verb, and then the
    // gate answers itself and the typed resolver is never consulted: no
    // sheet, no consultation, the silent allow the memory always meant.
    // A verb the memory does not carry asks as ever, and the typed ask
    // arrives carrying the raw call.
    let log: Consultations = Arc::new(Mutex::new(Vec::new()));
    let refusing_fixed = Arc::new(Fixed {
        resolution: PermissionResolution::DenyOnce,
    });
    let wrapped: Arc<dyn PermissionAskResolver> = Arc::new(Recording {
        inner: refusing_fixed,
        log: Arc::clone(&log),
    });
    let mut remembered = dch_permissions::MemoryView::empty();
    remembered
        .allowed
        .shell_prefixes
        .push("cargo test".to_string());
    let pipeline = live_pipeline(
        Arc::new(std::sync::Mutex::new(
            dch_config::PermissionMode::Interactive,
        )),
        Some(wrapped),
        Vec::new(),
        remembered,
    );
    let covered = pipeline
        .invoke(dispatch_for_input(
            "Bash",
            serde_json::json!({ "command": "cargo test --all-features" }),
            fresh_cancel(),
        ))
        .await;
    assert!(!covered.is_error, "the remembered verb runs without asking");
    assert!(
        log.lock().expect("log lock").is_empty(),
        "a covered ask never reaches the resolver — no sheet"
    );
    let fresh = pipeline
        .invoke(dispatch_for_input(
            "Bash",
            serde_json::json!({ "command": "cargo build" }),
            fresh_cancel(),
        ))
        .await;
    assert!(fresh.is_error, "an uncovered verb asks, and was refused");
    let seen = log.lock().expect("log lock").clone();
    assert_eq!(seen.len(), 1, "the uncovered verb consulted the resolver");
    assert_eq!(
        seen.into_iter().next().expect("one consultation").input,
        serde_json::json!({ "command": "cargo build" }),
        "the uncovered ask carried the raw call"
    );
}

#[tokio::test]
async fn a_deny_always_blocks_a_later_match_silently() {
    // The `d` press's standing half, end to end: the deny rules the
    // sheet recorded project into the gate's denied view, and a later
    // call they match is refused before any mode matrix or resolver
    // consult — an allowing resolver still sees nothing, so the block
    // is the remembered deny acting on its own.
    let mut remembered = dch_permissions::MemoryView::empty();
    remembered
        .denied
        .shell_prefixes
        .push("cargo publish".to_string());
    let (resolver, log) = recording();
    let pipeline = live_pipeline(
        Arc::new(std::sync::Mutex::new(
            dch_config::PermissionMode::Interactive,
        )),
        Some(resolver),
        Vec::new(),
        remembered,
    );
    let blocked = pipeline
        .invoke(dispatch_for_input(
            "Bash",
            serde_json::json!({ "command": "cargo publish --dry-run" }),
            fresh_cancel(),
        ))
        .await;
    assert!(
        blocked.is_error,
        "a remembered deny refuses the later match"
    );
    assert!(
        blocked.output.to_string().contains("remembered refusal"),
        "the refusal names the remembered layer, not the mode: {}",
        blocked.output
    );
    assert!(
        log.lock().expect("log lock").is_empty(),
        "a remembered deny blocks silently — no sheet, no consult"
    );
    // An unrelated verb is untouched: it asks, the fixture allows, the
    // call runs.
    let fresh = pipeline
        .invoke(dispatch_for_input(
            "Bash",
            serde_json::json!({ "command": "cargo build" }),
            fresh_cancel(),
        ))
        .await;
    assert!(!fresh.is_error, "the unrelated verb still runs");
    assert_eq!(
        log.lock().expect("log lock").len(),
        1,
        "only the fresh verb consulted the resolver"
    );
}
