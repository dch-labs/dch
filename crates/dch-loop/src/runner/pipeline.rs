//! The dispatch pipeline a runner installs, outermost first: the context
//! injector every dispatch passes through, the output cap, the permission
//! gate, the host middleware, the redaction guard, and the tool core.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use dch_tools::RunnerContext;
use loopctl::middleware::OutputLimitMiddleware;
use loopctl::middleware::PermissionMiddleware;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolMiddleware;
use loopctl::middleware::ToolPipeline;
use loopctl::tool::ToolRegistry;
use loopctl::tool::builtin::fs::FileSession;

use crate::RunnerError;

/// Build the dispatch pipeline with the [`ContextInjector`], the whole-result
/// output cap, and the permission gate installed.
///
/// Layering, outermost first: the context injector (every layer below — and
/// the tool — sees the enriched context), the whole-result output cap (a
/// middleware's post-processing runs on the way back out, so sitting directly
/// under the injector makes the cap the last word on any result a permission
/// verdict allowed through, while a denied call never pays for it), the
/// permission gate (denials short-circuit before any host work), the host
/// middleware in registration order, then the write-protection layers installed by
/// [`with_redaction`](crate::redaction_guard::with_redaction) when `redact`
/// is set (the guard refuses whole-file writes over content the redactor
/// hides; everything above them observes scrubbed output), and the
/// builtin-tool core.
///
/// A `max_output_chars` of `0` skips the cap layer entirely rather than
/// installing a no-op, so `middleware_names()` reports what actually runs.
///
/// A host's own capture middleware registers above the cap and therefore
/// records the tool's pre-cap bytes, not the cut the model receives — the
/// capture's own retention bound is the only ceiling the transcript
/// honors, and a host that wants the delivered shape recorded must cap
/// inside its capture rather than rely on this layer's position.
///
/// The pipeline's core holds its own registry instance for execution while the
/// [`BareLoop`] holds the one it advertises to the model; the registry stores
/// boxed tools that cannot be shared, so the engine's by-value registry and
/// the pipeline's core registry cannot be one instance. Both are composed from
/// the same providers by `compose_registry`, keeping the advertised and
/// executable tool sets identical by construction.
///
/// # Errors
///
/// - [`RunnerError::Config`] if the builder rejects the composition — cannot
///   happen with this fixed stack (the core is always set), but mapped to a
///   typed error rather than panicked on.
///
/// [`BareLoop`]: loopctl::engine::BareLoop
pub(super) fn build_pipeline(
    context: &Arc<RunnerContext>,
    fs_session: &FileSession,
    middleware: &[Arc<dyn ToolMiddleware>],
    permission: PermissionMiddleware,
    redact: bool,
    max_output_chars: usize,
    core_registry: ToolRegistry,
) -> Result<ToolPipeline, RunnerError> {
    let mut builder = ToolPipeline::builder().with_middleware(ContextInjector {
        context: Arc::clone(context),
        fs_session: fs_session.clone(),
    });
    if max_output_chars != 0 {
        builder = builder.with_middleware(OutputLimitMiddleware::new(max_output_chars));
    }
    builder = builder.with_middleware(permission);
    for host_middleware in middleware {
        builder = builder.with_middleware_arc(Arc::clone(host_middleware));
    }
    if redact {
        builder = crate::redaction_guard::with_redaction(builder);
    }
    builder
        .with_core(Arc::new(core_registry))
        .build()
        .map_err(|e| RunnerError::Config(format!("dispatch pipeline: {e}")))
}

/// A dispatch middleware that populates the per-call tool context with
/// the shared [`RunnerContext`] before the tool runs.
///
/// The engine builds each dispatch's [`ToolContext`](loopctl::tool::ToolContext)
/// with only the session id — no working directory, no interactivity flag, no
/// host extensions — but its pipeline contract hands middlewares the context to
/// augment first. This injector installs the runner context as the typed
/// extension (what `dch_tools::runner_ctx` reads) and sets the native `cwd` /
/// `is_non_interactive` fields, then delegates down the chain unchanged.
struct ContextInjector {
    /// The shared runner context, cloned into each dispatch's extension slot.
    ///
    /// Cloned on every dispatch (cheaply — `RunnerContext` is `Clone` with an
    /// `Arc`-shared todo list and question channel slot) so tools always see
    /// the current runner state.
    context: Arc<RunnerContext>,

    /// The filesystem session the builtin file family dispatches through.
    ///
    /// Attached to every dispatch's context beside the runner extension, so
    /// read and the writing tools share one baseline map, containment
    /// policy, and working directory by construction.
    fs_session: FileSession,
}

impl ToolMiddleware for ContextInjector {
    /// A stable middleware name for pipeline diagnostics and logging.
    ///
    /// Appears in `middleware_names()` traces; keep it stable and distinctive.
    fn name(&self) -> &'static str {
        "dch-context"
    }

    /// Augment the dispatch context, then continue down the pipeline.
    ///
    /// Installs the [`RunnerContext`] extension and the native `cwd` /
    /// `is_non_interactive` fields, then delegates via `next` — the injector
    /// adds no control flow of its own and never short-circuits.
    /// Interactivity is read live off the question channel slot on every
    /// dispatch, and it follows the sender's liveness rather than its
    /// presence: a slot still holding a sender whose reader is gone answers
    /// non-interactive, so a retired generation cannot advertise an asking
    /// path the dispatches behind it cannot use.
    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ToolDispatchContext,
        next: &'a ToolPipeline,
    ) -> Pin<Box<dyn Future<Output = loopctl::middleware::ToolDispatchResult> + Send + 'a>> {
        let asks_live = self
            .context
            .question_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|sender| !sender.is_closed());
        ctx.tool_context.cwd = self.context.cwd.to_string_lossy().into_owned();
        ctx.tool_context.is_non_interactive = !asks_live;
        self.fs_session.attach(&mut ctx.tool_context);
        ctx.tool_context.set_extension((*self.context).clone());
        Box::pin(async move { next.dispatch(ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DchClient;
    use crate::permission::permission_layer;
    use crate::runner::Runner;
    use crate::runner::test_support::{
        SseServer, recorded_requests, sample_context, sse_read_tool_call_turn, sse_text_turn,
        sse_tool_call_turn, wire_config,
    };
    use dch_tools::builtin_registry;
    use loopctl::engine::BareLoop;
    use loopctl::engine::Loop;
    use loopctl::engine::RunConfig;
    use loopctl::managers::LoopManagers;
    use loopctl::tool::Tool;
    use loopctl::tool::ToolContext;
    use loopctl::tool::ToolError;
    use loopctl::tool::ToolOutput;
    use loopctl::tool::ToolSchema;
    use serde_json::Value;
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A tool that reports the `cwd` it sees via the `RunnerContext` extension
    /// (sentinel `"absent"` when no extension is installed) plus the native
    /// `is_non_interactive` flag. Used to prove the `ContextInjector`
    /// middleware actually enriches the dispatch context.
    struct ExtensionProbe;

    impl Tool for ExtensionProbe {
        fn name(&self) -> &'static str {
            "Probe"
        }
        fn description(&self) -> &'static str {
            "reports the RunnerContext cwd"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                "Probe",
                "reports the RunnerContext cwd",
                serde_json::json!({"type": "object"}),
            )
        }
        fn call<'a>(
            &'a self,
            _input: Value,
            ctx: &ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>> {
            let report = format!(
                "{},non_interactive={}",
                runner_ctx_cwd(ctx),
                ctx.is_non_interactive
            );
            Box::pin(async move { Ok(ToolOutput::text(report)) })
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn system_prompt(&self) -> Option<String> {
            Some("probe".to_string())
        }
    }

    fn runner_ctx_cwd(ctx: &ToolContext) -> String {
        ctx.get_extension::<RunnerContext>().map_or_else(
            || "absent".to_string(),
            |rc| rc.cwd.to_string_lossy().into_owned(),
        )
    }

    /// A pass-through middleware whose name proves pipeline placement.
    ///
    /// Alters nothing about a dispatch; its only behavior is its
    /// name, which the pipeline exposes in `middleware_names()` —
    /// letting the placement pins observe layering through the real
    /// public surface instead of reaching into the pipeline's
    /// internals.
    struct HostProbe;

    impl ToolMiddleware for HostProbe {
        fn name(&self) -> &'static str {
            "host-probe"
        }
        fn dispatch<'a>(
            &'a self,
            ctx: &'a mut ToolDispatchContext,
            next: &'a ToolPipeline,
        ) -> Pin<Box<dyn Future<Output = loopctl::middleware::ToolDispatchResult> + Send + 'a>>
        {
            Box::pin(async move { next.dispatch(ctx).await })
        }
    }

    fn probe_dispatch_context() -> loopctl::middleware::ToolDispatchContext {
        loopctl::middleware::ToolDispatchContext {
            tool_name: "Probe".to_string(),
            input: Value::Null,
            call_id: "call_probe".to_string(),
            turn_number: 0,
            cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::Allow,
            tool_context: loopctl::tool::ToolContext::default(),
        }
    }

    fn probe_pipeline(with_injector: bool) -> ToolPipeline {
        if with_injector {
            probe_pipeline_with(&sample_context("/tmp/probe-cwd"))
        } else {
            let registry = ToolRegistry::new();
            registry.register(ExtensionProbe);
            ToolPipeline::builder()
                .with_core(Arc::new(registry))
                .build()
                .expect("static composition builds")
        }
    }

    fn probe_pipeline_with(context: &Arc<RunnerContext>) -> ToolPipeline {
        let registry = ToolRegistry::new();
        registry.register(ExtensionProbe);
        ToolPipeline::builder()
            .with_core(Arc::new(registry))
            .with_middleware(ContextInjector {
                context: Arc::clone(context),
                fs_session: FileSession::new(PathBuf::from(".")),
            })
            .build()
            .expect("static composition builds")
    }

    #[tokio::test]
    async fn pipeline_injects_the_extension_into_the_dispatch_context() {
        let pipeline = probe_pipeline(true);
        let mut ctx = probe_dispatch_context();
        let result = pipeline.dispatch(&mut ctx).await;
        assert!(
            !result.is_error,
            "probe dispatch through the injector must succeed"
        );
        assert!(
            result.output.to_string().contains("/tmp/probe-cwd"),
            "the probe must see the injected cwd: {result:?}"
        );
    }

    #[tokio::test]
    async fn pipeline_without_injector_leaves_the_extension_absent() {
        let pipeline = probe_pipeline(false);
        let mut ctx = probe_dispatch_context();
        let result = pipeline.dispatch(&mut ctx).await;
        assert!(!result.is_error, "probe dispatch must succeed");
        assert!(
            result.output.to_string().contains("absent"),
            "without the injector the probe must report the extension absent: {result:?}"
        );
    }

    struct PanicProbe;

    impl Tool for PanicProbe {
        fn name(&self) -> &'static str {
            "PanicProbe"
        }
        fn description(&self) -> &'static str {
            "panics on every call"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                "PanicProbe",
                "panics on every call",
                serde_json::json!({"type": "object"}),
            )
        }
        fn call<'a>(
            &'a self,
            _input: Value,
            _ctx: &ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>> {
            Box::pin(async move { panic!("probe panic") })
        }
    }

    #[tokio::test]
    async fn a_panicking_tool_surfaces_as_an_error_result_through_the_composed_pipeline() {
        let registry = builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
        registry.register(PanicProbe);
        let pipeline = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            false,
            0,
            registry,
        )
        .expect("static composition builds");
        let mut ctx = loopctl::middleware::ToolDispatchContext {
            tool_name: "PanicProbe".to_string(),
            input: Value::Null,
            call_id: "call_panic".to_string(),
            turn_number: 0,
            cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::Allow,
            tool_context: loopctl::tool::ToolContext::default(),
        };
        let result = pipeline.dispatch(&mut ctx).await;
        assert!(
            result.is_error,
            "a contained panic must surface as an error result, not a process death: {result:?}"
        );
        assert!(
            result.output.to_string().contains("probe panic"),
            "the error carries the panic payload for the model to react to: {result:?}"
        );
    }

    #[test]
    fn build_pipeline_places_the_injector_outermost_over_the_core() {
        let pipeline = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            false,
            dch_config::RunnerConfig::default().max_tool_output_chars,
            builtin_registry(
                &FileSession::new(PathBuf::from(".")),
                dch_tools::ReadWindows::default(),
            ),
        )
        .expect("static composition builds");
        assert_eq!(
            pipeline.middleware_names(),
            vec!["dch-context", "output_limit", "permission", "tool_call"],
            "the injector must be the outermost layer, with the cap and the\n\
             permission gate directly under it, over the tool-call core"
        );
    }

    #[test]
    fn build_pipeline_layers_host_middleware_between_gate_and_redaction() {
        let cap = dch_config::RunnerConfig::default().max_tool_output_chars;
        let redacting = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            true,
            cap,
            builtin_registry(
                &FileSession::new(PathBuf::from(".")),
                dch_tools::ReadWindows::default(),
            ),
        )
        .expect("static composition builds");
        assert_eq!(
            redacting.middleware_names(),
            vec![
                "dch-context",
                "output_limit",
                "permission",
                "write-target-gate",
                "redaction-guard",
                "redaction",
                "tool_call"
            ],
            "the gate wraps the core under the injector"
        );
        let layered = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[Arc::new(HostProbe)],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            true,
            cap,
            builtin_registry(
                &FileSession::new(PathBuf::from(".")),
                dch_tools::ReadWindows::default(),
            ),
        )
        .expect("static composition builds");
        assert_eq!(
            layered.middleware_names(),
            vec![
                "dch-context",
                "output_limit",
                "permission",
                "host-probe",
                "write-target-gate",
                "redaction-guard",
                "redaction",
                "tool_call"
            ],
            "host middleware sit under the gate and above the write-protection layers"
        );
    }

    /// A tool whose output echoes what a diff echo carries: a long
    /// high-entropy token beside ordinary text.
    struct HighEntropyEchoTool;

    impl loopctl::tool::Tool for HighEntropyEchoTool {
        fn name(&self) -> &'static str {
            "HighEntropyEcho"
        }

        fn description(&self) -> &'static str {
            "Test probe: echoes a fixed high-entropy payload."
        }

        fn schema(&self) -> loopctl::tool::ToolSchema {
            loopctl::tool::ToolSchema::new(
                self.name(),
                self.description(),
                serde_json::json!({"type": "object", "properties": {}}),
            )
        }

        fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &loopctl::tool::ToolContext,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<loopctl::tool::ToolOutput, loopctl::tool::ToolError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async {
                Ok(loopctl::tool::ToolOutput::text(
                    "wrote fixture with token qX7vK2mP9wL4nR8tY3jH6bF1cD5gS0aZ7eU2iO and padding aaaaaaaa",
                ))
            })
        }
    }

    #[tokio::test]
    async fn redaction_scrubs_high_entropy_tokens_in_tool_output() {
        let registry = loopctl::tool::ToolRegistry::new();
        registry.register(HighEntropyEchoTool);
        let pipeline = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            true,
            0,
            registry,
        )
        .expect("static composition builds");
        let mut ctx = probe_dispatch_context();
        ctx.tool_name = "HighEntropyEcho".to_string();

        let result = pipeline.dispatch(&mut ctx).await;
        let text = result.output.to_string();
        assert!(
            text.contains("[REDACTED:high_entropy]"),
            "the default-on redaction reaches the model's own echoed tokens: {text}"
        );
        assert!(
            text.contains("aaaaaaaa"),
            "low-entropy padding survives untouched: {text}"
        );
    }

    /// A tool whose one result is far larger than any sensible cap, for
    /// driving the pipeline's whole-result output ceiling.
    struct FloodingEchoTool;

    impl Tool for FloodingEchoTool {
        fn name(&self) -> &'static str {
            "FloodingEcho"
        }

        fn description(&self) -> &'static str {
            "Test probe: returns a payload far larger than the output cap."
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                self.name(),
                self.description(),
                serde_json::json!({"type": "object", "properties": {}}),
            )
        }

        fn call(
            &self,
            _input: Value,
            _ctx: &ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
            Box::pin(async { Ok(ToolOutput::text("z".repeat(5_000))) })
        }
    }

    /// A pipeline over `output_limit` dispatching only [`FloodingEchoTool`].
    fn capped_pipeline(output_limit: usize) -> ToolPipeline {
        let registry = ToolRegistry::new();
        registry.register(FloodingEchoTool);
        build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            false,
            output_limit,
            registry,
        )
        .expect("static composition builds")
    }

    #[test]
    fn build_pipeline_places_the_output_cap_directly_under_the_injector() {
        let pipeline = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            false,
            dch_config::RunnerConfig::default().max_tool_output_chars,
            builtin_registry(
                &FileSession::new(PathBuf::from(".")),
                dch_tools::ReadWindows::default(),
            ),
        )
        .expect("static composition builds");
        assert_eq!(
            pipeline.middleware_names(),
            vec!["dch-context", "output_limit", "permission", "tool_call"],
            "the injector stays outermost, the cap sits directly under it, and\n\
             the permission gate under the cap so a denied call never pays for it"
        );
    }

    #[test]
    fn a_zero_output_cap_installs_no_cap_layer() {
        let pipeline = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            false,
            0,
            builtin_registry(
                &FileSession::new(PathBuf::from(".")),
                dch_tools::ReadWindows::default(),
            ),
        )
        .expect("static composition builds");
        assert!(
            !pipeline.middleware_names().contains(&"output_limit"),
            "a disabled cap is absent from the report, not present and inert: {:?}",
            pipeline.middleware_names()
        );
    }

    /// A capture middleware recording what crossed its layer.
    struct RecordingCapture {
        /// The output text the layer observed, whole.
        recorded: Arc<std::sync::Mutex<String>>,
    }

    impl loopctl::middleware::ToolMiddleware for RecordingCapture {
        fn name(&self) -> &'static str {
            "recording-capture"
        }

        fn dispatch<'a>(
            &'a self,
            ctx: &'a mut loopctl::middleware::ToolDispatchContext,
            next: &'a ToolPipeline,
        ) -> Pin<Box<dyn Future<Output = loopctl::middleware::ToolDispatchResult> + Send + 'a>>
        {
            Box::pin(async move {
                let result = next.dispatch(ctx).await;
                *self.recorded.lock().expect("the capture slot") = result.output.to_string();
                result
            })
        }
    }

    #[tokio::test]
    async fn a_host_capture_above_the_cap_records_pre_cut_bytes() {
        // The composition places host middleware above the output
        // cap, so a capture there observes the tool's whole output
        // while the model receives the cut — the retention bound the
        // capture itself sets is the transcript's only ceiling.
        let recorded = Arc::new(std::sync::Mutex::new(String::new()));
        let registry = ToolRegistry::new();
        registry.register(FloodingEchoTool);
        let pipeline = build_pipeline(
            &sample_context("/tmp/probe-cwd"),
            &FileSession::new(PathBuf::from(".")),
            &[Arc::new(RecordingCapture {
                recorded: Arc::clone(&recorded),
            })],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            false,
            200,
            registry,
        )
        .expect("static composition builds");
        let mut ctx = probe_dispatch_context();
        ctx.tool_name = "FloodingEcho".to_string();

        let result = pipeline.dispatch(&mut ctx).await;
        assert_eq!(
            result.output.to_string().chars().count(),
            200,
            "the model receives the cut result"
        );
        assert_eq!(
            recorded.lock().expect("the capture slot").chars().count(),
            5_000,
            "the capture above the cap recorded the tool's whole output"
        );
    }

    #[tokio::test]
    async fn a_flooded_tool_result_reaches_the_model_cut_to_the_cap() {
        let pipeline = capped_pipeline(200);
        let mut ctx = probe_dispatch_context();
        ctx.tool_name = "FloodingEcho".to_string();

        let result = pipeline.dispatch(&mut ctx).await;
        let text = result.output.to_string();
        assert_eq!(
            text.chars().count(),
            200,
            "the delivered result lands on the configured ceiling"
        );
        assert!(
            text.ends_with("[truncated]"),
            "a cut result says so rather than passing as the tool's whole output"
        );
    }

    #[tokio::test]
    async fn a_zero_output_cap_leaves_an_oversized_result_whole() {
        let pipeline = capped_pipeline(0);
        let mut ctx = probe_dispatch_context();
        ctx.tool_name = "FloodingEcho".to_string();

        let result = pipeline.dispatch(&mut ctx).await;
        assert_eq!(
            result.output.to_string().chars().count(),
            5_000,
            "the documented disable keeps the tool's output verbatim"
        );
    }

    #[tokio::test]
    async fn injector_also_populates_the_native_context_fields() {
        let pipeline = probe_pipeline(true);
        let mut ctx = probe_dispatch_context();
        let _ = pipeline.dispatch(&mut ctx).await;
        assert_eq!(ctx.tool_context.cwd, "/tmp/probe-cwd");
        assert!(
            ctx.tool_context.is_non_interactive,
            "no question channel means non-interactive"
        );
    }

    #[tokio::test]
    async fn installing_a_channel_flips_subsequent_dispatches_to_interactive() {
        // The injector reads the question slot live per dispatch: an empty
        // slot dispatches non-interactive, installing a sender afterwards
        // flips the next dispatch — no rebuild needed.
        let context = sample_context("/tmp/probe-cwd");
        let pipeline = probe_pipeline_with(&context);
        let mut first = probe_dispatch_context();
        let _ = pipeline.dispatch(&mut first).await;
        assert!(
            first.tool_context.is_non_interactive,
            "empty slot must dispatch non-interactive"
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        *context.question_tx.lock().expect("slot lock") = Some(tx);
        let mut second = probe_dispatch_context();
        let _ = pipeline.dispatch(&mut second).await;
        assert_eq!(second.tool_context.cwd, "/tmp/probe-cwd");
        assert!(
            !second.tool_context.is_non_interactive,
            "installed channel must dispatch interactive"
        );
    }

    #[tokio::test]
    async fn a_dead_sender_in_the_slot_dispatches_non_interactive() {
        // A generation handoff can leave the slot holding a sender after its
        // reader is gone; reading presence alone would keep advertising an
        // asking path every later dispatch cannot actually use.
        let context = sample_context("/tmp/probe-cwd");
        let pipeline = probe_pipeline_with(&context);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        *context.question_tx.lock().expect("slot lock") = Some(tx);
        let mut dispatch = probe_dispatch_context();
        let _ = pipeline.dispatch(&mut dispatch).await;
        assert!(
            dispatch.tool_context.is_non_interactive,
            "a sender whose reader is gone is not an asking path"
        );
    }

    /// A dispatch context for one named tool call, for pipeline-seam drives.
    ///
    /// The stress scenarios below exercise the real dispatch pipeline —
    /// injector, gate, core registry — without an engine or a provider:
    /// each drives tool calls through the same middleware stack a live
    /// run dispatches through.
    fn tool_dispatch(name: &str, input: Value) -> loopctl::middleware::ToolDispatchContext {
        loopctl::middleware::ToolDispatchContext {
            tool_name: name.to_string(),
            input,
            call_id: format!("call_{name}"),
            turn_number: 0,
            cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::Allow,
            tool_context: loopctl::tool::ToolContext::default(),
        }
    }

    /// A real dispatch pipeline over `dir`: injector → Auto gate → core.
    ///
    /// The session the injector attaches per dispatch is a clone of the
    /// one the core registry's file family was built over, so reads and
    /// writes driven through this pipeline share one baseline map — the
    /// production wiring's exact shape, minus the engine.
    fn fs_pipeline(dir: &Path, redact: bool) -> ToolPipeline {
        let context = sample_context(&dir.to_string_lossy());
        let session = FileSession::new(dir.to_path_buf());
        build_pipeline(
            &context,
            &session,
            &[],
            permission_layer(dch_permissions::PermissionMode::Auto, None),
            redact,
            dch_config::RunnerConfig::default().max_tool_output_chars,
            builtin_registry(&session, dch_tools::ReadWindows::default()),
        )
        .expect("static composition builds")
    }

    /// Drive one tool call through `pipeline` and return its result.
    async fn drive(
        pipeline: &ToolPipeline,
        name: &str,
        input: Value,
    ) -> loopctl::middleware::ToolDispatchResult {
        let mut ctx = tool_dispatch(name, input);
        pipeline.dispatch(&mut ctx).await
    }

    #[tokio::test]
    async fn a_read_then_own_write_then_follow_up_write_needs_no_reread() {
        // read → write → write again with no intervening read: the model's
        // own write refreshes the recorded baseline, so the follow-up clears
        // the staleness guard — the session the injector attaches is the one
        // the dispatched tools share.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "v1").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "note.txt"})).await;
        assert!(!read.is_error, "{}", read.output);

        let first = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "note.txt", "content": "v2"}),
        )
        .await;
        assert!(!first.is_error, "{}", first.output);

        let second = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "note.txt", "content": "v3"}),
        )
        .await;
        assert!(
            !second.is_error,
            "the model's own write must refresh the baseline: {}",
            second.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap_or_default(),
            "v3",
            "the follow-up write lands its content"
        );
    }

    #[tokio::test]
    async fn a_byte_identical_external_revert_releases_the_guard() {
        // read → external change → byte-identical revert → write: the guard
        // is keyed on content, not on modification events, so a file whose
        // bytes are back to what the model last saw accepts the write —
        // the model's knowledge is accurate again.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("watched.txt");
        std::fs::write(&target, "v1").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(
            &pipeline,
            "Read",
            serde_json::json!({"path": "watched.txt"}),
        )
        .await;
        assert!(!read.is_error, "{}", read.output);

        std::fs::write(&target, "external").expect("external change");
        std::fs::write(&target, "v1").expect("byte-identical revert");

        let reverted = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "watched.txt", "content": "ours"}),
        )
        .await;
        assert!(
            !reverted.is_error,
            "a byte-identical revert must read as unchanged: {}",
            reverted.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "ours",
            "the write lands over the reverted content"
        );
    }

    #[tokio::test]
    async fn a_refused_write_leaves_the_baseline_armed_for_the_revert() {
        // read → external change → write (refused) → byte-identical
        // revert → write: only a successful write refreshes the
        // recorded baseline, so the refused write in the middle arms
        // nothing — the revert restores exactly the bytes the model
        // last saw, and the write over them lands.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("watched.txt");
        std::fs::write(&target, "v1").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(
            &pipeline,
            "Read",
            serde_json::json!({"path": "watched.txt"}),
        )
        .await;
        assert!(!read.is_error, "{}", read.output);

        std::fs::write(&target, "external").expect("external change");
        let refused = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "watched.txt", "content": "ours"}),
        )
        .await;
        assert!(refused.is_error, "a stale write is refused: {refused:?}");
        assert!(
            refused.output.to_string().contains("changed on disk"),
            "the refusal names the staleness it refused: {refused:?}"
        );

        std::fs::write(&target, "v1").expect("byte-identical revert");
        let reverted = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "watched.txt", "content": "ours"}),
        )
        .await;
        assert!(
            !reverted.is_error,
            "the refusal refreshed nothing, so the revert reads as unchanged: {}",
            reverted.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "ours",
            "the write lands over the reverted content"
        );
    }

    #[tokio::test]
    async fn a_conflict_on_the_second_write_leaves_the_first_write_landed() {
        // Two writes in one model turn: the fresh file lands, the stale one
        // is held. Dispatches are independent — one call's refusal never
        // rolls back a sibling that already executed.
        let dir = TempDir::new().expect("tempdir");
        let watched = dir.path().join("watched.rs");
        std::fs::write(&watched, "fn main() {}\n").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "watched.rs"})).await;
        assert!(!read.is_error, "{}", read.output);
        std::fs::write(&watched, "fn main() { /* external */ }\n").expect("external change");

        let fresh = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "fresh.txt", "content": "new"}),
        )
        .await;
        assert!(!fresh.is_error, "{}", fresh.output.to_string());
        assert!(dir.path().join("fresh.txt").exists());

        let stale = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "watched.rs", "content": "fn main() { ours }\n"}),
        )
        .await;
        assert!(
            stale.is_error && stale.output.to_string().contains("changed on disk"),
            "the second write must refuse against its stale baseline: {}",
            stale.output
        );
        assert_eq!(
            std::fs::read_to_string(&watched).unwrap_or_default(),
            "fn main() { /* external */ }\n",
            "the held write moves no bytes"
        );
    }

    #[tokio::test]
    async fn a_first_write_to_a_never_read_existing_file_overwrites_it() {
        // No recorded baseline → no staleness guard: the first write to an
        // existing file the session never read overwrites it. Intended
        // semantics — a guard here would demand a read before every fresh
        // session's first write — pinned so a silent tightening or loosening
        // is caught.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("existing.txt");
        std::fs::write(&target, "before").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let overwrite = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "existing.txt", "content": "after"}),
        )
        .await;
        assert!(
            !overwrite.is_error,
            "a no-baseline write is allowed by design: {}",
            overwrite.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "after",
            "the overwrite lands"
        );

        // The overwrite itself arms the guard: a follow-up against a file
        // changed externally since is held.
        std::fs::write(&target, "external").expect("external change");
        let held = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "existing.txt", "content": "again"}),
        )
        .await;
        assert!(
            held.is_error,
            "the model's own write armed the baseline it is now held against: {}",
            held.output
        );
    }

    #[tokio::test]
    async fn a_multiedit_batch_refreshes_the_baselines_a_later_write_consults() {
        // The batch tool shares the session the injector attaches, so a
        // landed MultiEdit must refresh every touched file's baseline the
        // same way a landed Write does — otherwise the model's own batch
        // registers as an external change against its follow-up write.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
        std::fs::write(dir.path().join("b.txt"), "beta\n").expect("seed b");
        let pipeline = fs_pipeline(dir.path(), false);

        let batch = drive(
            &pipeline,
            "MultiEdit",
            serde_json::json!({
                "edits": [
                    {"file_path": "a.txt", "old_text": "alpha", "new_text": "ALPHA"},
                    {"file_path": "b.txt", "old_text": "beta", "new_text": "BETA"}
                ]
            }),
        )
        .await;
        assert!(!batch.is_error, "{}", batch.output);

        let follow_up = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "b.txt", "content": "rewritten\n"}),
        )
        .await;
        assert!(
            !follow_up.is_error,
            "the model's own batch must not read as an external change: {}",
            follow_up.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap_or_default(),
            "rewritten\n",
            "the follow-up write lands its content"
        );
    }

    #[tokio::test]
    async fn a_dry_run_batch_arms_no_baseline_a_later_write_judges() {
        // The preview reads its targets to build the diff but records no
        // observation: only a landed write refreshes a baseline. So a
        // dry-run over a file the session never read leaves that file in
        // the never-read class — a later Write lands even after an
        // external change, judged by no baseline the preview armed. The
        // pin guards the neutrality: a preview that silently armed the
        // guard would hold writes the model never verified a view of.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("previewed.txt");
        std::fs::write(&target, "alpha\n").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let preview = drive(
            &pipeline,
            "MultiEdit",
            serde_json::json!({
                "edits": [
                    {"file_path": "previewed.txt", "old_text": "alpha", "new_text": "ALPHA"}
                ],
                "dry_run": true
            }),
        )
        .await;
        assert!(!preview.is_error, "{}", preview.output);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "alpha\n",
            "the preview writes nothing"
        );

        std::fs::write(&target, "external\n").expect("external change after the preview");
        let write = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "previewed.txt", "content": "ours\n"}),
        )
        .await;
        assert!(
            !write.is_error,
            "the preview armed no baseline, so the never-read write is judged \
             by none: {}",
            write.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "ours\n",
            "the write lands over the externally changed bytes"
        );
    }

    #[tokio::test]
    async fn a_backgrounded_redirection_over_a_redacted_target_is_refused_too() {
        // The guard sits above the shell tool, so it judges the command
        // before the background branch ever spawns the detached job: the
        // backgrounded spelling of a refused redirection is refused the
        // same way, and no job is started for it.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            format!("api_key = {}\nport = 8080\n", "K".repeat(20)),
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), true);

        let bash = drive(
            &pipeline,
            "Bash",
            serde_json::json!({
                "command": "printf overwritten > env.conf",
                "background": true
            }),
        )
        .await;
        assert!(
            bash.is_error,
            "the backgrounded spelling earns the same refusal: {bash:?}"
        );
        assert!(
            !bash.output.to_string().contains("Started background job"),
            "no job may be spawned for a refused redirection: {}",
            bash.output
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let after = std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default();
        assert!(
            after.contains("api_key = ") && !after.contains("overwritten"),
            "the true bytes survive the refused background redirection: {after:?}"
        );
    }

    #[tokio::test]
    async fn a_secret_target_meets_the_overwrite_guard_before_the_linter() {
        // The guard is dispatch-layer and the linter is tool-layer,
        // so broken content against a secret-bearing target refuses
        // at the guard with its own wording — and against a clean
        // target the same content flows on to the tool's syntax gate.
        // Pinning the pair keeps a future reorder of the two layers
        // from silently swapping which refusal the model reads.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("env.conf"), "api_key=abcdefghijklmnop\n")
            .expect("seed the secret-bearing target");
        std::fs::write(dir.path().join("plain.rs"), "fn main() {}\n").expect("seed clean target");
        let pipeline = fs_pipeline(dir.path(), true);

        let guarded = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "env.conf", "content": "fn broken {"}),
        )
        .await;
        assert!(
            guarded.is_error && guarded.output.to_string().contains("redactor hides"),
            "the guard refuses before the linter ever runs: {}",
            guarded.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default(),
            "api_key=abcdefghijklmnop\n",
            "the guarded write moves no bytes"
        );

        let linted = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "plain.rs", "content": "fn broken {"}),
        )
        .await;
        assert!(
            linted.is_error && !linted.output.to_string().contains("redactor hides"),
            "a clean target passes the guard and meets the tool's syntax gate: {}",
            linted.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plain.rs")).unwrap_or_default(),
            "fn main() {}\n",
            "the linted write moves no bytes either"
        );
    }

    #[tokio::test]
    async fn an_unprotected_first_write_at_a_pipe_keeps_the_raw_route() {
        // The write-protection family installs only beside redaction, and
        // the tool layer's own refusal is baseline-conditional: a pipe the
        // session never read holds no baseline, so with redaction off the
        // staleness check classifies nothing and the atomic rename lands
        // an ordinary file over the endpoint — the target gate's
        // documented "raw route". Pinned so the acceptance of that cell
        // is a fact the suite owns: a future unconditioned WriteTargetGate
        // flips this pin, and a stall here would mean the tool opened the
        // pipe instead of renaming over it.
        let dir = TempDir::new().expect("tempdir");
        let fifo = dir.path().join("notify");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "the fifo fixture exists");
        let pipeline = fs_pipeline(dir.path(), false);

        let write = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                &pipeline,
                "Write",
                serde_json::json!({"file_path": "notify", "content": "payload"}),
            ),
        )
        .await
        .expect("the raw route settles instead of parking on the pipe");
        assert!(
            !write.is_error,
            "the never-read pipe holds no baseline to hold the write with: {}",
            write.output
        );
        assert!(
            std::fs::metadata(&fifo).is_ok_and(|meta| meta.is_file()),
            "an ordinary file stands at the endpoint after the rename"
        );
        assert_eq!(
            std::fs::read_to_string(&fifo).unwrap_or_default(),
            "payload",
            "the renamed file carries the written bytes"
        );
    }

    #[tokio::test]
    async fn a_batch_whose_second_entry_lost_its_anchor_writes_nothing() {
        // One MultiEdit call, two files: both read through the pipeline,
        // the second externally changed before the batch runs. The
        // anchor the second entry names is gone from the file's current
        // bytes, so the batch's preparation stage — which reads every
        // target and substitutes every anchor before any write — refuses
        // the whole call: nothing lands, ahead-entry included. The
        // mid-batch partial-landing shape (writes that already reached
        // the disk when a *later* entry's re-check conflicts) belongs to
        // the write stage loopctl pins; the deterministic dch-seam fact
        // is this all-or-nothing refusal at preparation.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
        std::fs::write(dir.path().join("b.txt"), "beta\n").expect("seed b");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(
            &pipeline,
            "ReadFiles",
            serde_json::json!({"files": ["a.txt", "b.txt"]}),
        )
        .await;
        assert!(!read.is_error, "{}", read.output);
        std::fs::write(dir.path().join("b.txt"), "external\n")
            .expect("external change to the second file");

        let batch = drive(
            &pipeline,
            "MultiEdit",
            serde_json::json!({
                "edits": [
                    {"file_path": "a.txt", "old_text": "alpha", "new_text": "ALPHA"},
                    {"file_path": "b.txt", "old_text": "beta", "new_text": "BETA"}
                ]
            }),
        )
        .await;
        assert!(
            batch.is_error,
            "the batch refuses against the changed second file: {batch:?}"
        );
        let refusal = batch.output.to_string();
        assert!(
            refusal.contains("b.txt"),
            "the refusal names the file whose anchor is gone: {refusal}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap_or_default(),
            "alpha\n",
            "preparation-stage refusal lands nothing — the ahead entry stays as it was"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap_or_default(),
            "external\n",
            "the externally changed file keeps its bytes"
        );
    }

    #[tokio::test]
    async fn skip_linter_true_does_not_bypass_the_staleness_guard() {
        // The bypass lifts only the syntax gate: `skip_linter: true` against
        // a stale baseline must still refuse. The two gates are independent,
        // and the conflict check is the later one — a bypass that also
        // skipped it would clobber external changes with broken syntax.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("watched.rs");
        std::fs::write(&target, "fn main() {}\n").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "watched.rs"})).await;
        assert!(!read.is_error, "{}", read.output);
        std::fs::write(&target, "fn main() { /* external */ }\n").expect("external change");

        let bypassed = drive(
            &pipeline,
            "Write",
            serde_json::json!({
                "file_path": "watched.rs",
                "content": "fn broken {",
                "skip_linter": true
            }),
        )
        .await;
        assert!(
            bypassed.is_error && bypassed.output.to_string().contains("changed on disk"),
            "the linter bypass must not widen past the conflict check: {}",
            bypassed.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "fn main() { /* external */ }\n",
            "the bypassed-but-conflicted write moves no bytes"
        );
    }

    #[tokio::test]
    async fn a_baseline_recorded_through_a_relative_spelling_guards_an_absolute_write() {
        // Relative and absolute spellings of one file must meet at the same
        // baseline key: a read of "note.txt" arms the guard for a write
        // spelled with the workspace's absolute path — the spelling the
        // tools' own error messages print, so a model echoing it back must
        // not slip past the armed guard.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("note.txt");
        std::fs::write(&target, "v1").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "note.txt"})).await;
        assert!(!read.is_error, "{}", read.output);
        std::fs::write(&target, "external").expect("external change");

        let absolute = drive(
            &pipeline,
            "Write",
            serde_json::json!({
                "file_path": target.to_string_lossy(),
                "content": "ours"
            }),
        )
        .await;
        assert!(
            absolute.is_error && absolute.output.to_string().contains("changed on disk"),
            "the absolute spelling must resolve to the read's baseline key: {}",
            absolute.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "external",
            "the cross-spelling write moves no bytes"
        );
    }

    #[tokio::test]
    async fn a_file_only_viewed_never_arms_the_staleness_guard() {
        // FileViewer serves bounded windows over the same session but
        // records no baseline — arming is the file source's job, and a
        // preview is not the model's whole-file knowledge. So a file the
        // session only viewed stays in the never-read class: the first
        // write lands even after an external change. Pinned as the
        // intended reading of the shared-session claim — the registry
        // doc's "one baseline map" is about which map the family shares,
        // not about every reader recording into it.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("viewed.txt");
        std::fs::write(&target, "v1\n").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), false);

        let view = drive(
            &pipeline,
            "FileViewer",
            serde_json::json!({"file_path": "viewed.txt"}),
        )
        .await;
        assert!(!view.is_error, "{}", view.output);

        std::fs::write(&target, "external\n").expect("external change");
        let write = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "viewed.txt", "content": "ours\n"}),
        )
        .await;
        assert!(
            !write.is_error,
            "a viewer-only session holds no baseline to hold the write with: {}",
            write.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "ours\n",
            "the write lands over the externally changed bytes"
        );
    }

    #[tokio::test]
    async fn concurrent_readers_share_one_baseline_map_whichever_armed_a_file() {
        // ReadFiles and Read may fly together (both are concurrency-safe) and
        // they write the same session's baseline map through the one injector
        // the pipeline attaches. Whichever call armed a file, the guard judges
        // that file by its own bytes at write time: the externally changed one
        // holds, the ones nobody touched land. The newest-observation-wins
        // stamping is what makes an overlapping Read/ReadFiles pair on one
        // file resolve to one baseline instead of last-insert-wins.
        let dir = TempDir::new().expect("tempdir");
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), format!("{name} v1\n")).expect("seed target");
        }
        let pipeline = fs_pipeline(dir.path(), false);

        let batch = drive(
            &pipeline,
            "ReadFiles",
            serde_json::json!({"files": ["a.txt", "b.txt", "c.txt"]}),
        );
        let single_b = drive(&pipeline, "Read", serde_json::json!({"path": "b.txt"}));
        let (r_batch, r_single) = tokio::join!(batch, single_b);
        assert!(!r_batch.is_error, "{}", r_batch.output);
        assert!(!r_single.is_error, "{}", r_single.output);

        std::fs::write(dir.path().join("b.txt"), "external\n").expect("external change");

        let write_a = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "a.txt", "content": "ours a\n"}),
        )
        .await;
        let write_b = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "b.txt", "content": "ours b\n"}),
        )
        .await;
        assert!(
            !write_a.is_error,
            "a file only the batch read must land when untouched: {}",
            write_a.output
        );
        assert!(
            write_b.is_error && write_b.output.to_string().contains("changed on disk"),
            "the file both readers armed must hold against its change: {}",
            write_b.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap_or_default(),
            "external\n",
            "the held write moves no bytes"
        );
    }

    #[tokio::test]
    async fn a_write_over_a_redacted_read_is_refused_and_the_true_bytes_survive() {
        // The confirmed destruction path: redaction rewrites the read's
        // output while the staleness baseline keys on the true bytes, so
        // an unguarded Write would land the placeholder text over the
        // real secret. The overwrite guard must refuse it and leave the
        // file untouched.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            "api_key=abcdefghijklmnop\nport = 8080\n",
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), true);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "env.conf"})).await;
        assert!(!read.is_error, "{}", read.output);
        assert!(
            read.output.to_string().contains("[REDACTED:"),
            "the redaction layer must be scrubbing the read: {read:?}"
        );

        let write = drive(
            &pipeline,
            "Write",
            serde_json::json!({
                "file_path": "env.conf",
                "content": "api_key=[REDACTED:api_key_kv]\nport = 8080\n"
            }),
        )
        .await;
        assert!(
            write.is_error,
            "a whole-file write over redacted content is refused: {write:?}"
        );
        let refusal = write.output.to_string();
        assert!(
            refusal.contains("env.conf") && refusal.contains("Edit"),
            "the refusal names the target and points at the surgical path: {refusal}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default(),
            "api_key=abcdefghijklmnop\nport = 8080\n",
            "the true bytes survive the refused write"
        );
    }

    #[tokio::test]
    async fn a_write_over_a_redacted_read_sent_through_the_path_alias_is_refused() {
        // The alias the fs family documents — `path` where the schemas
        // advertise `file_path` — serves the write tool as surely as the
        // canonical spelling, so the overwrite guard reads the field the
        // same way: a whole-file write spelled through the drift's key
        // is refused over redacted content, and the true bytes survive
        // exactly as the canonical spelling's refusal leaves them.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            "api_key=abcdefghijklmnop\nport = 8080\n",
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), true);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "env.conf"})).await;
        assert!(!read.is_error, "{}", read.output);
        assert!(
            read.output.to_string().contains("[REDACTED:"),
            "the redaction layer must be scrubbing the read: {read:?}"
        );

        let write = drive(
            &pipeline,
            "Write",
            serde_json::json!({
                "path": "env.conf",
                "content": "api_key=[REDACTED:api_key_kv]\nport = 8080\n"
            }),
        )
        .await;
        assert!(
            write.is_error,
            "the alias spelling names the same target the canonical does: {write:?}"
        );
        let refusal = write.output.to_string();
        assert!(
            refusal.contains("env.conf") && refusal.contains("Edit"),
            "the refusal names the target and the surgical path whatever key spelled it: {refusal}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default(),
            "api_key=abcdefghijklmnop\nport = 8080\n",
            "the true bytes survive the alias-spelled refusal too"
        );
    }

    #[tokio::test]
    async fn an_alias_spelled_write_at_a_pipe_is_refused_by_the_target_gate() {
        // The gate's contract in the drift's own spelling: a write whose
        // destination is a FIFO would rename an ordinary file over the
        // endpoint another process waits on, and a target read through
        // the `path` alias meets the same refusal — the gate settles
        // the destination the way the tool resolves it.
        let dir = TempDir::new().expect("tempdir");
        let fifo = dir.path().join("pipe.conf");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "the fifo fixture exists");
        let pipeline = fs_pipeline(dir.path(), true);

        let write = drive(
            &pipeline,
            "Write",
            serde_json::json!({"path": "pipe.conf", "content": "payload"}),
        )
        .await;
        assert!(
            write.is_error,
            "a pipe destination refuses the write through the alias: {write:?}"
        );
        let refusal = write.output.to_string();
        assert!(
            refusal.contains("pipe.conf") && refusal.contains("FIFO"),
            "the gate names the target and its kind whatever key spelled it: {refusal}"
        );
    }

    #[tokio::test]
    async fn an_alias_spelled_multi_edit_entry_meets_the_same_target_gate() {
        // One batch names several files, and each entry may spell its
        // target through the alias: the gate walks the array resolving
        // the field the way the tool does, so a per-edit `path` at an
        // unreplaceable destination is refused like the canonical —
        // and a passable entry ahead of it does not hide the bad one.
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir(dir.path().join("nested")).expect("a directory target");
        let pipeline = fs_pipeline(dir.path(), true);

        let batch = drive(
            &pipeline,
            "MultiEdit",
            serde_json::json!({
                "edits": [
                    {"path": "plain.txt", "old_text": "a", "new_text": "b"},
                    {"path": "nested", "old_text": "a", "new_text": "b"},
                ]
            }),
        )
        .await;
        assert!(
            batch.is_error,
            "a directory destination refuses the batch through the alias: {batch:?}"
        );
        let refusal = batch.output.to_string();
        assert!(
            refusal.contains("nested") && refusal.contains("directory"),
            "the gate names the walked entry's target and kind: {refusal}"
        );
    }

    #[tokio::test]
    async fn a_multi_edit_entry_falling_onto_the_batch_default_is_gated_too() {
        // The batch default is a destination like any other: an entry
        // that carries no path of its own resolves onto the top-level
        // `file_path`, so a batch defaulting onto a FIFO must be
        // refused even though no entry names it — the schema
        // advertises that default, a model sends it, and the walk
        // that skipped it would let the tool read the FIFO or rename
        // an ordinary file over the endpoint. A passable entry ahead
        // of the pathless one must not hide it, exactly as the
        // walked-alias pin holds for spelled targets.
        let dir = TempDir::new().expect("tempdir");
        let fifo = dir.path().join("notify");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "the fifo fixture exists");
        std::fs::write(dir.path().join("plain.txt"), "a").expect("seed a plain file");
        let pipeline = fs_pipeline(dir.path(), true);

        let batch = drive(
            &pipeline,
            "MultiEdit",
            serde_json::json!({
                "file_path": "notify",
                "edits": [
                    {"file_path": "plain.txt", "old_text": "a", "new_text": "b"},
                    {"old_text": "a", "new_text": "c"},
                ]
            }),
        )
        .await;
        assert!(
            batch.is_error,
            "an entry defaulting onto the pipe is refused before anything runs: {batch:?}"
        );
        let refusal = batch.output.to_string();
        assert!(
            refusal.contains("notify") && refusal.contains("FIFO"),
            "the refusal names the default-leg target and its kind: {refusal}"
        );
    }

    #[tokio::test]
    async fn a_mistyped_entry_path_does_not_borrow_the_gated_default() {
        // The default is only for entries that name nothing. An entry
        // mistyping its own `file_path` is refused by the tool on the
        // entry itself, before the default is consulted — so a gate
        // that mirrored that fallthrough would refuse on the default
        // target where the batch never reaches it. The batch is still
        // an error either way; this pin is about which hand answers:
        // the tool's teaching error on the mistyped entry, not the
        // gate's verdict on a destination this call cannot land on.
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir(dir.path().join("nested")).expect("a directory target");
        let pipeline = fs_pipeline(dir.path(), true);

        let batch = drive(
            &pipeline,
            "MultiEdit",
            serde_json::json!({
                "file_path": "nested",
                "edits": [{"file_path": 42, "old_text": "a", "new_text": "b"}]
            }),
        )
        .await;
        assert!(
            batch.is_error,
            "a mistyped entry path is refused: {batch:?}"
        );
        let refusal = batch.output.to_string();
        assert!(
            refusal.contains("must be a string"),
            "the tool answers on the mistyped entry, the hand the tool itself raises: {refusal}"
        );
        assert!(
            !refusal.contains("not a regular file"),
            "the gate does not judge the default for an entry that never reaches it: {refusal}"
        );
    }

    #[tokio::test]
    async fn a_write_to_a_file_the_redactor_does_not_match_still_lands_under_redaction() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("plain.conf"), "port = 8080\n").expect("seed plain file");
        let pipeline = fs_pipeline(dir.path(), true);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "plain.conf"})).await;
        assert!(!read.is_error, "{}", read.output);

        let write = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "plain.conf", "content": "port = 9090\n"}),
        )
        .await;
        assert!(
            !write.is_error,
            "a target redaction serves verbatim stays writable: {}",
            write.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plain.conf")).unwrap_or_default(),
            "port = 9090\n",
            "the write lands"
        );
    }

    #[tokio::test]
    async fn an_edit_avoiding_the_secret_region_still_lands_on_a_redacted_read() {
        // The refusal's release path: an edit's anchor runs against the
        // file's true bytes, so a clean-region old_text the model did
        // see verbatim matches and the edit lands — the secret line
        // untouched beside it.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            "api_key=abcdefghijklmnop\nport = 8080\n",
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), true);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "env.conf"})).await;
        assert!(!read.is_error, "{}", read.output);

        let edit = drive(
            &pipeline,
            "Edit",
            serde_json::json!({
                "file_path": "env.conf",
                "old_text": "port = 8080",
                "new_text": "port = 9090"
            }),
        )
        .await;
        assert!(
            !edit.is_error,
            "a surgical edit over a clean region is the documented path forward: {}",
            edit.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default(),
            "api_key=abcdefghijklmnop\nport = 9090\n",
            "the edit lands with the secret region intact"
        );
    }

    #[tokio::test]
    async fn a_bash_redirect_over_a_redacted_target_is_refused_and_the_true_bytes_survive() {
        // The shell is the Write tool's escape hatch: a redirection is a
        // whole-file write in operator spelling, and the probe that
        // established the gap clobbered the secret exactly this way. The
        // guard names every redirection target the command carries, so
        // the refusal arrives before the shell runs anything.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            "api_key=abcdefghijklmnop\nport = 8080\n",
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), true);

        let bash = drive(
            &pipeline,
            "Bash",
            serde_json::json!({
                "command": "printf 'api_key=[REDACTED:api_key_kv]\nport = 8080\n' > env.conf"
            }),
        )
        .await;
        assert!(
            bash.is_error,
            "a shell redirection over redacted content is refused: {bash:?}"
        );
        let refusal = bash.output.to_string();
        assert!(
            refusal.contains("env.conf") && refusal.contains("redaction off"),
            "the refusal names the target and the release path: {refusal}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default(),
            "api_key=abcdefghijklmnop\nport = 8080\n",
            "the true bytes survive the refused redirection"
        );
    }

    #[tokio::test]
    async fn a_bash_redirect_to_a_clean_target_still_lands_under_redaction() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("plain.conf"), "port = 8080\n").expect("seed plain file");
        let pipeline = fs_pipeline(dir.path(), true);

        let bash = drive(
            &pipeline,
            "Bash",
            serde_json::json!({"command": "printf 'port = 9090\n' > plain.conf"}),
        )
        .await;
        assert!(
            !bash.is_error,
            "a redirection the patterns do not match stays runnable: {}",
            bash.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plain.conf")).unwrap_or_default(),
            "port = 9090\n",
            "the shell's write lands"
        );
    }

    #[tokio::test]
    async fn a_stdout_redirect_over_a_redacted_target_is_refused_and_the_true_bytes_survive() {
        // `>&file` is the whole-file write wearing bash's duplicate
        // spelling: bash opens the file for it exactly as it does for
        // `>file`, so the guard's refusal has to reach this spelling too.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            "api_key=quartzgmpbankfyle6\nport = 8080\n",
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), true);

        let bash = drive(
            &pipeline,
            "Bash",
            serde_json::json!({"command": "printf x >& env.conf"}),
        )
        .await;
        assert!(
            bash.is_error,
            "the duplicate spelling of a whole-file redirection is refused: {bash:?}"
        );
        assert!(
            bash.output.to_string().contains("env.conf"),
            "the refusal names the target: {}",
            bash.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default(),
            "api_key=quartzgmpbankfyle6\nport = 8080\n",
            "the true bytes survive the refused redirection"
        );
    }

    #[tokio::test]
    async fn a_stdout_redirect_to_a_clean_target_still_lands_under_redaction() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("plain.conf"), "port = 8080\n").expect("seed plain file");
        let pipeline = fs_pipeline(dir.path(), true);

        let bash = drive(
            &pipeline,
            "Bash",
            serde_json::json!({"command": "printf 'port = 9090\n' 1>& plain.conf"}),
        )
        .await;
        assert!(
            !bash.is_error,
            "the newly armed spelling stays runnable on a target the patterns miss: {}",
            bash.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plain.conf")).unwrap_or_default(),
            "port = 9090\n",
            "the shell's descriptor-spelled write lands"
        );
    }

    #[tokio::test]
    async fn a_bash_command_without_redirection_still_runs_under_redaction() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            "api_key=abcdefghijklmnop\nport = 8080\n",
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), true);

        let bash = drive(
            &pipeline,
            "Bash",
            serde_json::json!({"command": "grep -c port env.conf"}),
        )
        .await;
        assert!(
            !bash.is_error,
            "a command that only reads the guarded file stays runnable: {}",
            bash.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("env.conf")).unwrap_or_default(),
            "api_key=abcdefghijklmnop\nport = 8080\n",
            "a read-only shell command never touches the file"
        );
    }

    #[tokio::test]
    async fn redaction_off_leaves_the_write_path_unguarded() {
        // The guard exists only under redaction: with the scrubber
        // absent the model receives faithful file content, so the
        // ordinary staleness rules alone govern writes.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("env.conf"),
            "api_key=abcdefghijklmnop\nport = 8080\n",
        )
        .expect("seed the secret-bearing file");
        let pipeline = fs_pipeline(dir.path(), false);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "env.conf"})).await;
        assert!(!read.is_error, "{}", read.output);
        assert!(
            !read.output.to_string().contains("[REDACTED:"),
            "redaction is off: the read serves the true bytes"
        );

        let write = drive(
            &pipeline,
            "Write",
            serde_json::json!({"file_path": "env.conf", "content": "port = 9090\n"}),
        )
        .await;
        assert!(
            !write.is_error,
            "without redaction the whole-file write follows the ordinary rules: {}",
            write.output
        );
    }

    #[tokio::test]
    async fn a_redacted_read_then_write_back_is_refused_and_the_secret_survives() {
        // The pipeline-seam pin of the redaction overwrite guard: the read's
        // output is scrubbed while the file's true bytes hold a matched
        // secret, so a whole-file Write of the placeholder text the model
        // saw must be refused at dispatch — the staleness guard alone
        // would clear it, and the write would destroy the secret.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("env.conf");
        std::fs::write(&target, "api_key=abcdefghijklmnopqrstuvwxyz12\nport=8080\n")
            .expect("seed target");
        let pipeline = fs_pipeline(dir.path(), true);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "env.conf"})).await;
        assert!(
            read.output.to_string().contains("[REDACTED"),
            "the redaction pass scrubs the read the model sees: {}",
            read.output
        );

        let write_back = drive(
            &pipeline,
            "Write",
            serde_json::json!({
                "file_path": "env.conf",
                "content": "api_key=[REDACTED:key_value]\nport=8080\n"
            }),
        )
        .await;
        assert!(
            write_back.is_error
                && write_back
                    .output
                    .to_string()
                    .contains("values the secrets redactor hides"),
            "the whole-file write over hidden content must refuse: {}",
            write_back.output
        );
        assert!(
            std::fs::read_to_string(&target)
                .unwrap_or_default()
                .contains("abcdefghijklmnopqrstuvwxyz12"),
            "the refused write leaves the true secret on disk"
        );
    }

    #[tokio::test]
    async fn a_plain_file_stays_whole_file_writable_under_redaction() {
        // The guard's inverse: a file no pattern matches crosses the
        // redactor verbatim, so the model's context is faithful and the
        // whole-file write proceeds — the guard refuses only what
        // redaction actually hides.
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("plain.conf");
        std::fs::write(&target, "port = 8080\nhost = localhost\n").expect("seed target");
        let pipeline = fs_pipeline(dir.path(), true);

        let read = drive(&pipeline, "Read", serde_json::json!({"path": "plain.conf"})).await;
        assert!(
            read.output.to_string().contains("port = 8080"),
            "a plain file crosses the redactor verbatim: {}",
            read.output
        );

        let written = drive(
            &pipeline,
            "Write",
            serde_json::json!({
                "file_path": "plain.conf",
                "content": "port = 9090\nhost = localhost\n"
            }),
        )
        .await;
        assert!(
            !written.is_error,
            "a faithful-context write must proceed: {}",
            written.output
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default(),
            "port = 9090\nhost = localhost\n",
            "the write lands"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_output_secrets_are_scrubbed_before_reaching_the_model() {
        let leak = "Authorization: Bearer sk-ant-api03-0123456789abcdef0123456789abcdef";
        let server = SseServer::start(vec![
            sse_tool_call_turn(
                "Bash",
                &serde_json::json!({"command": format!("echo '{leak}'")}),
            ),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let config = wire_config(server.port, 7);
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("leak"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && requests[1].contains("[REDACTED:"),
            "the tool result fed back to the model must be scrubbed: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabling_redaction_passes_tool_output_through_verbatim() {
        let leak = "Authorization: Bearer sk-ant-api03-0123456789abcdef0123456789abcdef";
        let server = SseServer::start(vec![
            sse_tool_call_turn(
                "Bash",
                &serde_json::json!({"command": format!("echo '{leak}'")}),
            ),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 7);
        config.security.redact_secrets = false;
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("leak"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && !requests[1].contains("[REDACTED:"),
            "with redaction off the result must pass through unscrubbed: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn builtin_tool_dispatch_sees_the_runner_context_end_to_end() {
        // Turn 1 asks the real ReadTool for note.txt; turn 2 finishes. ReadTool
        // errors when the RunnerContext extension is absent, so a successful
        // run proves the injector middleware enriches the real dispatch path.
        let server = SseServer::start(vec![sse_read_tool_call_turn(), sse_text_turn("done")]).await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "probe payload\n").expect("write fixture");
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("read it"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.tool_call_count(), 1, "the Read tool was dispatched");
        assert_eq!(run.turn_count(), 2, "tool turn plus final text turn");
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && requests[1].contains("probe payload"),
            "the tool result must flow into the follow-up request: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interactivity_flag_flows_through_full_engine_runs() {
        // Drive the real engine (not just the pipeline in isolation) with the
        // probe tool: run 1 with an empty channel slot dispatches
        // non-interactive; installing a channel into the shared slot flips
        // run 2 to interactive — the injector reads the slot live on every
        // real dispatch.
        let server = SseServer::start(vec![
            sse_tool_call_turn("Probe", &serde_json::json!({})),
            sse_text_turn("done"),
            sse_tool_call_turn("Probe", &serde_json::json!({})),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let cwd = dir.path().to_string_lossy().into_owned();
        let context = sample_context(&cwd);
        let mut managers = LoopManagers::new();
        // The engine executes through the pipeline's core registry while its
        // own registry only advertises schemas, so the probe must sit in both.
        managers.set_pipeline(probe_pipeline_with(&context));
        let registry = ToolRegistry::new();
        registry.register(ExtensionProbe);
        let config = wire_config(server.port, 7);
        let client = crate::create_client(&config.api).expect("client builds");
        let mut engine = BareLoop::<DchClient>::new_with_managers(
            Arc::new(client),
            registry,
            loopctl::config::SessionConfig::default(),
            managers,
        );
        let policy = RunConfig::default();
        let first = engine
            .run("probe", &policy)
            .await
            .expect("first run succeeds");
        assert_eq!(first.tool_call_count(), 1);
        let requests = recorded_requests(&server);
        assert!(
            requests[1].contains(&cwd) && requests[1].contains("non_interactive=true"),
            "run 1 must see the extension and an empty (non-interactive) slot: {requests:?}"
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        *context.question_tx.lock().expect("slot lock") = Some(tx);
        let second = engine
            .run("again", &policy)
            .await
            .expect("second run succeeds");
        assert_eq!(second.tool_call_count(), 1);
        let requests = recorded_requests(&server);
        assert!(
            requests[3].contains("non_interactive=false"),
            "run 2 (channel installed) must dispatch interactive: {requests:?}"
        );
    }
}
