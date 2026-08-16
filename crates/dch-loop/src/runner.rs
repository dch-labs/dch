//! The top-level agent: a thin owner of one configured `BareLoop` plus the
//! shared runner context.
//!
//! [`Runner`] is pure composition glue — it holds no agent-loop logic of its
//! own. [`Runner::new`] constructs the loopctl ingredients (a concrete provider
//! client, a tool registry, a session config carrying the composed system
//! prompt, and the observer-bearing managers bundle), wires them into a
//! [`BareLoop`], and exposes [`Runner::run`] / [`Runner::cancel`] as one-line
//! delegations.
//!
//! Tools reach per-call state (the working directory, the todo list, the
//! optional question channel) through a [`RunnerContext`] extension. The
//! engine builds the per-dispatch
//! [`ToolContext`](loopctl::tool::ToolContext) without host state, but its
//! dispatch pipeline hands middlewares the context before the tool runs — so
//! the runner installs a private context-injector middleware that populates
//! the extension (and the native `cwd` / `is_non_interactive` fields) on
//! every dispatch.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use dch_tools::RunnerContext;
use dch_tools::builtin_registry;
use loopctl::engine::BareLoop;
use loopctl::engine::Loop;
use loopctl::engine::Run;
use loopctl::engine::RunConfig;
use loopctl::error::LoopError;
use loopctl::managers::LoopManagers;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolMiddleware;
use loopctl::middleware::ToolPipeline;
use loopctl::observer::LoopObserver;
use loopctl::tool::ToolRegistry;

use crate::DchClient;
use crate::RunnerError;
use crate::detect_tech_stack;
use crate::merge_by_language;
use crate::with_context;

/// The top-level agent: the single type the rest of the application holds.
///
/// `Runner` is pure composition glue — it owns no agent-loop logic of its own.
/// [`Runner::new`] assembles the loopctl ingredients (a concrete provider
/// client, a tool registry whose every dispatch sees the shared
/// [`RunnerContext`], a session config carrying the composed system prompt,
/// and the observer-bearing managers bundle) into one [`BareLoop`], and the
/// public surface stays thin: runs reset the per-run todo list and delegate
/// to the loop, the rest are accessors.
///
/// Construct once per agent identity with [`Runner::new`], then drive it with
/// [`Runner::run`]: each call is one prompt → loop → final answer against the
/// configured provider, sharing one conversation across calls. Each run starts
/// by resetting the per-run todo list (a new prompt plans fresh). Cancellation
/// is cooperative via [`Runner::cancel`] or [`Runner::cancel_signal`].
pub struct Runner {
    /// The agent loop, monomorphized over the concrete [`DchClient`].
    ///
    /// Constructed once in [`Runner::new`] via `new_with_managers` with the
    /// builtin tool registry, the composed session config, and the
    /// observer-carrying managers (which carry the dispatch pipeline with the
    /// context injector installed). Every public method delegates to it; the
    /// concrete (non-`dyn`) client keeps the per-turn LLM call statically
    /// dispatched.
    inner: BareLoop<DchClient>,

    /// The shared per-runner context, cloned into every tool dispatch.
    ///
    /// Built from `workdir` in [`Runner::new`] and carried by the private
    /// context-injector middleware in the dispatch pipeline, so every
    /// builtin tool reaches `cwd`, the todo list, and the question channel
    /// slot through the `ToolContext` extension. Exposed via
    /// [`Runner::context`] for host wiring, and mutated by
    /// [`Runner::set_question_tx`].
    context: Arc<RunnerContext>,

    /// The session-default run policy (turn budget, dispatch policy).
    ///
    /// Mapped once from `DchConfig` in [`Runner::new`] and used by every
    /// [`Runner::run`] call — the underlying `Loop::run` contract takes the
    /// run config per call, so this field supplies dch's default. A caller
    /// that needs a different budget for one run passes an override to
    /// [`Runner::run_with`] rather than rebuilding the `Runner` (rebuilding
    /// creates a fresh `BareLoop` and discards the conversation).
    run_config: RunConfig,
}

impl Runner {
    /// Construct a runner from configuration, observers, and the working
    /// directory the agent operates within.
    ///
    /// `observers` are registered with the loop in the order given; the caller
    /// owns their construction. The system prompt is composed from the active
    /// role, the tech stack detected under `workdir` (merged with any
    /// `[project]` overrides), and per-tool fragments, then installed on the
    /// session config before the [`BareLoop`] is built.
    ///
    /// `workdir` is taken explicitly rather than read from the process's
    /// current directory so a resumed session or a test fixture can point the
    /// agent at the correct tree.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::Client`] if the provider client cannot be constructed
    ///   (missing API key, HTTP client failure).
    pub fn new(
        config: &dch_config::DchConfig,
        observers: Vec<Arc<dyn LoopObserver>>,
        workdir: &Path,
    ) -> Result<Self, RunnerError> {
        let context = Arc::new(RunnerContext {
            cwd: workdir.to_path_buf(),
            todos: Arc::new(std::sync::Mutex::new(Vec::new())),
            question_tx: Arc::new(std::sync::Mutex::new(None)),
        });

        let registry = builtin_registry();

        let session_config = build_session(config, &registry, workdir);
        let run_config = config.to_run_config();
        let client = crate::create_client(&config.api)?;

        let mut managers = LoopManagers::new();
        for observer in observers {
            managers.register_observer(observer);
        }
        managers.set_pipeline(build_pipeline(&context)?);

        let inner = BareLoop::<DchClient>::new_with_managers(
            Arc::new(client),
            registry,
            session_config,
            managers,
        );

        Ok(Self {
            inner,
            context,
            run_config,
        })
    }

    /// Run one prompt → loop → final answer, with the session-default policy.
    ///
    /// Uses the run policy mapped from `DchConfig` at construction (turn
    /// budget, dispatch policy). The per-run todo list is reset at the start
    /// of the run — a new prompt plans fresh. Returns when the model signals
    /// end-of-turn, the budget is exhausted, the loop detector aborts, or
    /// [`Runner::cancel`] is called. Equivalent to [`Runner::run_with`] with
    /// the default config; the conversation is shared across `run`/`run_with`
    /// calls on the same `Runner`.
    ///
    /// # Errors
    ///
    /// - [`LoopError::MaxTurnsExceeded`] when the per-run turn budget is hit.
    /// - [`LoopError::Cancelled`] when [`Runner::cancel`] fires mid-run.
    /// - [`LoopError::Api`] / [`LoopError::StreamError`] on provider failures.
    /// - Other [`LoopError`] variants as surfaced by the loop (compaction,
    ///   detection, reflection, tool recovery).
    pub async fn run(&mut self, input: &str) -> Result<Run, LoopError> {
        let run_config = self.run_config.clone();
        self.run_with(input, &run_config).await
    }

    /// Run one prompt → loop → final answer, with an explicit run policy.
    ///
    /// The variation point for per-run budgets: `run_with` overrides the
    /// session default for this one call without rebuilding the `Runner` (a
    /// rebuild constructs a fresh `BareLoop` and discards the conversation).
    /// The per-run todo list is reset at the start of the run. The returned
    /// [`Run`] records `run_config` as its own snapshot, so per-call overrides
    /// are visible in the run's accounting. This mirrors loopctl's own
    /// default/override pairing (`stream_messages` vs
    /// `stream_messages_with_options`).
    ///
    /// # Errors
    ///
    /// - [`LoopError::MaxTurnsExceeded`] when `run_config.max_turns` is hit.
    /// - [`LoopError::Cancelled`] when [`Runner::cancel`] fires mid-run.
    /// - [`LoopError::Api`] / [`LoopError::StreamError`] on provider failures.
    /// - Other [`LoopError`] variants as surfaced by the loop (compaction,
    ///   detection, reflection, tool recovery).
    pub async fn run_with(
        &mut self,
        input: &str,
        run_config: &RunConfig,
    ) -> Result<Run, LoopError> {
        self.context
            .todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.inner.run(input, run_config).await
    }

    /// Signal the in-flight run, if any, to cancel.
    ///
    /// Cooperative — the loop checks the shared [`CancelSignal`] at the top of
    /// each turn and between tool dispatches.
    ///
    /// [`CancelSignal`]: loopctl::cancel::CancelSignal
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// The shared cancel signal, cloned from the inner loop.
    ///
    /// For external callers that need to select on cancellation from a
    /// separate task (e.g. a Ctrl-C handler in the binary wiring, which
    /// cannot borrow the `Runner` itself). Tripping the returned signal is
    /// equivalent to calling [`Runner::cancel`]; every clone observes the
    /// same trip.
    #[must_use]
    pub fn cancel_signal(&self) -> Arc<loopctl::cancel::CancelSignal> {
        self.inner.cancel_signal()
    }

    /// A reference to the shared runner context.
    ///
    /// The returned `RunnerContext` exposes the shared todo list (behind an
    /// `Arc<Mutex<…>>`, so callers can mutate it) and the question channel
    /// slot. Intended for host wiring and inspection — install the question
    /// channel with [`Runner::set_question_tx`] rather than through this
    /// accessor.
    #[must_use]
    pub fn context(&self) -> &RunnerContext {
        &self.context
    }

    /// Install the channel tools use to ask the user questions.
    ///
    /// Intended for interactive hosts (a TUI) to plug in their question
    /// receiver; a headless runner leaves the slot empty and the asking tool
    /// errors rather than blocking. Every tool dispatch reads the slot live,
    /// so the channel may also be installed or replaced between runs — a
    /// later installation flips subsequent dispatches to interactive
    /// immediately.
    pub fn set_question_tx(&self, tx: std::sync::mpsc::Sender<dch_tools::QuestionRequest>) {
        *self
            .context
            .question_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    }
}

/// Compose the session config — including the system prompt — for `config`.
///
/// Maps the runner settings onto a [`loopctl::config::SessionConfig`], detects
/// the tech stack under `workdir`, merges any `[project]` overrides, and
/// composes the full system prompt via [`with_context`] from the active role,
/// the resolved techs, any project-wide conventions, and a per-role prose
/// override when one is configured. The prompt is written into
/// `session_config.system_prompt` before the [`BareLoop`] reads it.
///
/// Extracted as a free function (rather than inlined in [`Runner::new`]) so the
/// composition can be unit-tested without constructing a provider client.
fn build_session(
    config: &dch_config::DchConfig,
    registry: &ToolRegistry,
    workdir: &Path,
) -> loopctl::config::SessionConfig {
    let mut session_config = config.to_session_config();
    let role_override = config.runner.role_override(config.runner.role);
    let detected = detect_tech_stack(workdir);
    let techs = merge_by_language(detected, &config.project);
    let prompt = with_context(
        config.runner.role,
        &techs,
        config.project.conventions.as_deref(),
        role_override,
        registry,
    );
    session_config.system_prompt = Some(prompt);
    session_config
}

/// Build the dispatch pipeline with the [`ContextInjector`] installed.
///
/// The engine routes every tool call through this pipeline (when one is set on
/// the managers) instead of calling tools directly, and hands each middleware
/// the per-dispatch [`ToolDispatchContext`] — whose `tool_context` field is
/// documented for exactly this augmentation. Putting the injector first
/// (outermost) means any later middleware also sees the enriched context.
///
/// The pipeline's core holds its own registry instance for execution while the
/// [`BareLoop`] holds the one it advertises to the model; the registry stores
/// boxed tools that cannot be shared, so the engine's by-value registry and
/// the pipeline's core registry cannot be one instance. Both come from
/// [`dch_tools::builtin_registry`], whose construction is deterministic —
/// pinned by a dch-tools test asserting two constructions register the
/// identical tool set and schemas.
///
/// # Errors
///
/// - [`RunnerError::Config`] if the builder rejects the composition — cannot
///   happen with this fixed stack (the core is always set), but mapped to a
///   typed error rather than panicked on.
fn build_pipeline(context: &Arc<RunnerContext>) -> Result<ToolPipeline, RunnerError> {
    ToolPipeline::builder()
        .with_middleware(ContextInjector {
            context: Arc::clone(context),
        })
        .with_core(Arc::new(builtin_registry()))
        .build()
        .map_err(|e| RunnerError::Config(format!("dispatch pipeline: {e}")))
}

/// A dispatch middleware that populates the per-call tool context with the
/// shared [`RunnerContext`] before the tool runs.
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
    /// `is_non_interactive` fields (interactivity read live from the question
    /// channel slot), then delegates via `next` — the injector adds no control
    /// flow of its own and never short-circuits.
    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ToolDispatchContext,
        next: &'a ToolPipeline,
    ) -> Pin<Box<dyn Future<Output = loopctl::middleware::ToolDispatchResult> + Send + 'a>> {
        let has_channel = self
            .context
            .question_tx
            .lock()
            .is_ok_and(|slot| slot.is_some());
        ctx.tool_context.cwd = self.context.cwd.to_string_lossy().into_owned();
        ctx.tool_context.is_non_interactive = !has_channel;
        ctx.tool_context.set_extension((*self.context).clone());
        Box::pin(async move { next.dispatch(ctx).await })
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
    clippy::arithmetic_side_effects,
    clippy::let_underscore_must_use
)]
mod tests {
    use super::*;
    use dch_config::DchConfig;
    use loopctl::tool::Tool;
    use loopctl::tool::ToolContext;
    use loopctl::tool::ToolError;
    use loopctl::tool::ToolOutput;
    use loopctl::tool::ToolSchema;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Mutex;
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
            ToolSchema {
                tool: "Probe".to_string(),
                description: "reports the RunnerContext cwd".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }
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

    fn sample_context(cwd: &str) -> Arc<RunnerContext> {
        Arc::new(RunnerContext {
            cwd: PathBuf::from(cwd),
            todos: Arc::new(Mutex::new(Vec::new())),
            question_tx: Arc::new(Mutex::new(None)),
        })
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
            let mut registry = ToolRegistry::new();
            registry.register(ExtensionProbe);
            ToolPipeline::builder()
                .with_core(Arc::new(registry))
                .build()
                .expect("static composition builds")
        }
    }

    fn probe_pipeline_with(context: &Arc<RunnerContext>) -> ToolPipeline {
        let mut registry = ToolRegistry::new();
        registry.register(ExtensionProbe);
        ToolPipeline::builder()
            .with_core(Arc::new(registry))
            .with_middleware(ContextInjector {
                context: Arc::clone(context),
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

    #[test]
    fn build_pipeline_places_the_injector_outermost_over_the_core() {
        let pipeline =
            build_pipeline(&sample_context("/tmp/probe-cwd")).expect("static composition builds");
        assert_eq!(
            pipeline.middleware_names(),
            vec!["dch-context", "tool_call"],
            "the injector must be the outermost layer over the tool-call core"
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
        let (tx, _rx) = std::sync::mpsc::channel();
        *context.question_tx.lock().expect("slot lock") = Some(tx);
        let mut second = probe_dispatch_context();
        let _ = pipeline.dispatch(&mut second).await;
        assert_eq!(second.tool_context.cwd, "/tmp/probe-cwd");
        assert!(
            !second.tool_context.is_non_interactive,
            "installed channel must dispatch interactive"
        );
    }

    #[test]
    fn set_question_tx_installs_the_channel_into_the_shared_context() {
        let dir = TempDir::new().expect("tempdir");
        let runner =
            Runner::new(&offline_config(), Vec::new(), dir.path()).expect("Runner::new constructs");
        let (tx, _rx) = std::sync::mpsc::channel();
        runner.set_question_tx(tx);
        assert!(
            runner
                .context()
                .question_tx
                .lock()
                .expect("slot lock")
                .is_some(),
            "the sender must land in the shared slot tools read"
        );
    }

    #[test]
    fn set_question_tx_replaces_an_existing_channel() {
        let dir = TempDir::new().expect("tempdir");
        let runner =
            Runner::new(&offline_config(), Vec::new(), dir.path()).expect("Runner::new constructs");
        let (first_tx, first_rx) = std::sync::mpsc::channel();
        runner.set_question_tx(first_tx);
        let (second_tx, second_rx) = std::sync::mpsc::channel();
        runner.set_question_tx(second_tx);
        let sender = runner
            .context()
            .question_tx
            .lock()
            .expect("slot lock")
            .clone()
            .expect("sender set");
        sender
            .send(dch_tools::QuestionRequest {
                questions: Vec::new(),
            })
            .expect("receiver alive");
        assert!(
            second_rx.try_recv().is_ok(),
            "the replacement channel must receive"
        );
        assert!(
            first_rx.try_recv().is_err(),
            "the replaced channel must no longer receive"
        );
    }

    #[test]
    fn build_session_composes_the_system_prompt_for_general_role() {
        let config = DchConfig::default();
        let registry = dch_tools::builtin_registry();
        let session = build_session(&config, &registry, Path::new("/tmp"));
        let prompt = session
            .system_prompt
            .as_deref()
            .expect("system_prompt is composed");
        assert!(!prompt.is_empty());
        // Default role is General; its body must be present.
        assert!(
            prompt.contains("YOUR ROLE: GENERAL ASSISTANCE"),
            "General role body missing: {prompt}"
        );
    }

    #[test]
    fn build_session_honors_role_override() {
        use dch_config::Role;
        use dch_config::RoleOverride;
        let mut config = DchConfig::default();
        config.runner.role = Role::Coding;
        config.runner.role_overrides = vec![RoleOverride {
            role: Role::Coding,
            prompt: "OVERRIDE MARKER: do the thing.".to_string(),
        }];
        let registry = dch_tools::builtin_registry();
        let session = build_session(&config, &registry, Path::new("/tmp"));
        let prompt = session.system_prompt.expect("composed");
        assert!(
            prompt.contains("OVERRIDE MARKER"),
            "override prose missing: {prompt}"
        );
        assert!(
            !prompt.contains("YOUR ROLE: IMPLEMENT FEATURES AND FIXES"),
            "an override must replace the built-in role body, not append to it: {prompt}"
        );
    }

    #[test]
    fn build_session_includes_project_section_when_a_tech_marker_is_present() {
        // End-to-end through build_session: a Cargo.toml marker under workdir
        // must produce a PROJECT section carrying the rust toolchain. Proves
        // detect_tech_stack → merge_by_language → with_context flows through.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"probe\"\n",
        )
        .expect("write marker");
        let config = DchConfig::default();
        let registry = dch_tools::builtin_registry();
        let session = build_session(&config, &registry, dir.path());
        let prompt = session.system_prompt.expect("composed");
        assert!(
            prompt.contains("PROJECT"),
            "PROJECT section missing with a Cargo.toml marker: {prompt}"
        );
        assert!(
            prompt.contains("cargo build"),
            "rust build command missing: {prompt}"
        );
    }

    #[test]
    fn build_session_omits_project_section_when_no_tech_marker() {
        // An empty tempdir yields no detected techs, so no PROJECT section.
        let dir = TempDir::new().expect("tempdir");
        let config = DchConfig::default();
        let registry = dch_tools::builtin_registry();
        let session = build_session(&config, &registry, dir.path());
        let prompt = session.system_prompt.expect("composed");
        assert!(
            !prompt.contains("PROJECT"),
            "PROJECT section should be absent with no markers: {prompt}"
        );
    }

    fn offline_config() -> DchConfig {
        use dch_config::ApiConfig;
        use dch_config::ApiType;
        let mut config = DchConfig::default();
        config.api = ApiConfig {
            api_type: ApiType::OpenAi,
            base_url: "https://example.invalid".to_string(),
            api_key: Some("dummy".to_string()),
            model: "test-model".to_string(),
            ..ApiConfig::default()
        };
        config
    }

    #[test]
    fn runner_new_constructs_offline_and_cancel_signal_trips() {
        // Runner::new must construct without a network connection (the provider
        // builder is lazy), and cancel()/cancel_signal() must delegate to the
        // BareLoop. Cancelling trips the shared signal a subsequent run would
        // observe.
        let dir = TempDir::new().expect("tempdir");
        let config = offline_config();
        let runner =
            Runner::new(&config, Vec::new(), dir.path()).expect("Runner::new constructs offline");
        let signal = runner.cancel_signal();
        assert!(
            !signal.is_cancelled(),
            "a fresh runner's cancel signal must not be tripped"
        );
        runner.cancel();
        assert!(
            signal.is_cancelled(),
            "cancel() must trip the shared signal"
        );
    }

    fn sse_text_turn(text: &str) -> String {
        [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"content": text}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": null, "finish_reason": "stop"}]
            }),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect()
    }

    fn sse_read_tool_call_turn() -> String {
        sse_tool_call_turn("Read", &serde_json::json!({"file_path": "note.txt"}))
    }

    fn sse_tool_call_turn(tool: &str, args: &serde_json::Value) -> String {
        let args = args.to_string();
        [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1",
                    "function": {"name": tool, "arguments": ""}
                }]}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": args}
                }]}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": null, "finish_reason": "tool_calls"}]
            }),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect()
    }

    /// A canned-SSE provider endpoint on an ephemeral local port.
    ///
    /// Serves one response body per accepted connection (in order), records
    /// each request body, and optionally delays before answering so a run can
    /// be cancelled while in flight.
    struct SseServer {
        port: u16,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl SseServer {
        async fn start(responses: Vec<String>) -> Self {
            Self::start_with_delay(responses, std::time::Duration::ZERO).await
        }

        async fn start_with_delay(responses: Vec<String>, delay: std::time::Duration) -> Self {
            // Bind here (not inside the task) so the port is known before the
            // first request is sent; the listener moves into the accept loop.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("ephemeral bind");
            let port = listener
                .local_addr()
                .expect("bound socket has an address")
                .port();
            let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let task_requests = Arc::clone(&requests);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let mut queue: std::collections::VecDeque<String> = responses.into();
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let body = queue.pop_front();
                    if let (Some(req), Ok(mut recorded)) =
                        (read_http_request(&mut sock).await, task_requests.lock())
                    {
                        recorded.push(req);
                    }
                    if delay > std::time::Duration::ZERO {
                        tokio::time::sleep(delay).await;
                    }
                    let bytes = body.map_or_else(
                        || {
                            "HTTP/1.1 500 No canned response\r\nContent-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                                .to_string()
                        },
                        |b| {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                                b.len(),
                                b
                            )
                        },
                    );
                    let _ = sock.write_all(bytes.as_bytes()).await;
                }
            });
            Self { port, requests }
        }
    }

    async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
        use tokio::io::AsyncReadExt as _;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_header_end(&buf) {
                break pos;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let total = header_end + 4 + content_length;
        while buf.len() < total {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    /// An observer that records the lifecycle events it receives, in order.
    struct RecordingObserver {
        events: Mutex<Vec<&'static str>>,
    }

    impl RecordingObserver {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn record(&self, event: &'static str) {
            if let Ok(mut events) = self.events.lock() {
                events.push(event);
            }
        }

        fn snapshot(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .map(|events| events.clone())
                .unwrap_or_default()
        }
    }

    impl loopctl::observer::LoopObserver for RecordingObserver {
        fn name(&self) -> &'static str {
            "recording"
        }
        fn on_run_start(&self, _: &loopctl::observer::RunStartContext) {
            self.record("run_start");
        }
        fn on_turn_start(&self, _: &loopctl::observer::TurnStartContext) {
            self.record("turn_start");
        }
        fn on_response(&self, _: &loopctl::observer::ResponseContext) {
            self.record("response");
        }
        fn on_turn_end(&self, _: &loopctl::observer::TurnEndContext) {
            self.record("turn_end");
        }
        fn on_run_end(&self, _: &loopctl::observer::RunEndContext) {
            self.record("run_end");
        }
    }

    fn wire_config(port: u16, max_turns: usize) -> DchConfig {
        use dch_config::ApiConfig;
        use dch_config::ApiType;
        let mut config = DchConfig::default();
        config.api = ApiConfig {
            api_type: ApiType::OpenAi,
            base_url: format!("http://127.0.0.1:{port}"),
            api_key: Some("dummy".to_string()),
            model: "test-model".to_string(),
            request_timeout_secs: 10,
            ..ApiConfig::default()
        };
        config.runner.max_turns = max_turns;
        config
    }

    fn recorded_requests(server: &SseServer) -> Vec<String> {
        server
            .requests
            .lock()
            .map(|reqs| reqs.clone())
            .unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_completes_with_the_session_default_policy() {
        let server = SseServer::start(vec![sse_text_turn("probe says hello")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner =
            Runner::new(&wire_config(server.port, 7), Vec::new(), dir.path()).expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes within timeout")
            .expect("run succeeds against the canned server");
        assert_eq!(run.output.as_deref(), Some("probe says hello"));
        assert_eq!(run.turn_count(), 1);
        assert!(run.stop_reason.is_none(), "clean stop, no error reason");
        assert_eq!(run.config.max_turns, 7, "session default policy is used");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_with_applies_the_override_for_that_call_only() {
        let server = SseServer::start(vec![sse_text_turn("first"), sse_text_turn("second")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner =
            Runner::new(&wire_config(server.port, 7), Vec::new(), dir.path()).expect("constructs");
        let override_config = loopctl::engine::RunConfig::default().with_max_turns(3);
        let first = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            runner.run_with("hi", &override_config),
        )
        .await
        .expect("first run completes")
        .expect("first run succeeds");
        assert_eq!(
            first.config.max_turns, 3,
            "run_with must run with the explicit policy"
        );
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("again"))
            .await
            .expect("second run completes")
            .expect("second run succeeds");
        assert_eq!(
            second.config.max_turns, 7,
            "plain run afterwards must be back on the session default"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observers_fire_in_lifecycle_order_for_one_run() {
        let server = SseServer::start(vec![sse_text_turn("hello")]).await;
        let dir = TempDir::new().expect("tempdir");
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::new(
            &wire_config(server.port, 7),
            vec![Arc::clone(&observer) as Arc<dyn LoopObserver>],
            dir.path(),
        )
        .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(
            observer.snapshot(),
            vec!["run_start", "turn_start", "response", "turn_end", "run_end"],
            "each lifecycle event fires exactly once, in order"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn composed_system_prompt_and_tool_schemas_reach_the_provider() {
        let server = SseServer::start(vec![sse_text_turn("hello")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner =
            Runner::new(&wire_config(server.port, 7), Vec::new(), dir.path()).expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        let requests = recorded_requests(&server);
        assert_eq!(requests.len(), 1, "one LLM request for one turn");
        let body = &requests[0];
        assert!(
            body.contains("YOUR ROLE: GENERAL ASSISTANCE"),
            "composed system prompt must reach the wire: {body}"
        );
        assert!(
            body.contains("\"Read\""),
            "the registry's tool schemas must be advertised: {body}"
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
        let mut runner =
            Runner::new(&wire_config(server.port, 7), Vec::new(), dir.path()).expect("constructs");
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
        let mut registry = ToolRegistry::new();
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
        let (tx, _rx) = std::sync::mpsc::channel();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conversation_persists_across_runs() {
        let server = SseServer::start(vec![
            sse_text_turn("first answer"),
            sse_text_turn("second answer"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner =
            Runner::new(&wire_config(server.port, 7), Vec::new(), dir.path()).expect("constructs");
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            runner.run("first question"),
        )
        .await
        .expect("first run completes")
        .expect("first run succeeds");
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            runner.run("second question"),
        )
        .await
        .expect("second run completes")
        .expect("second run succeeds");
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2
                && requests[1].contains("first answer")
                && requests[1].contains("second question"),
            "the second run's request must carry the prior exchange: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_the_signal_mid_run_returns_cancelled() {
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("too late")],
            std::time::Duration::from_secs(5),
        )
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner =
            Runner::new(&wire_config(server.port, 7), Vec::new(), dir.path()).expect("constructs");
        let signal = runner.cancel_signal();
        let task = tokio::spawn(async move { runner.run("slow").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        signal.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("cancelled run returns promptly")
            .expect("task joins");
        assert!(
            matches!(result, Err(loopctl::error::LoopError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_against_a_dead_endpoint_returns_an_error() {
        // Bind a listener, note its port, drop it: connections are refused
        // deterministically, so run() must surface an error, not hang.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let dir = TempDir::new().expect("tempdir");
        let mut runner =
            Runner::new(&wire_config(port, 7), Vec::new(), dir.path()).expect("constructs");
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi")).await;
        assert!(
            matches!(result, Ok(Err(_))),
            "expected an Err from run against a refused endpoint, got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_clears_the_todo_list_at_the_start_of_each_run() {
        // A dead endpoint makes the run fail, but the reset happens before the
        // request — proving the clear is tied to run start, not to success.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let dir = TempDir::new().expect("tempdir");
        let mut runner =
            Runner::new(&wire_config(port, 7), Vec::new(), dir.path()).expect("constructs");
        runner
            .context()
            .todos
            .lock()
            .expect("todos lock")
            .push(dch_tools::TodoEntry {
                id: "1".to_string(),
                subject: "stale plan".to_string(),
                description: String::new(),
                status: dch_tools::TodoStatus::Pending,
                active_form: None,
            });
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi")).await;
        assert!(
            runner
                .context()
                .todos
                .lock()
                .expect("todos lock")
                .is_empty(),
            "each run must start with a fresh todo list"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_returns_max_turns_exceeded_when_the_budget_is_hit() {
        // Two tool-call turns against a two-turn budget: the model never
        // produces a final answer, so the loop must abort with the budget
        // error rather than request a third turn.
        let server =
            SseServer::start(vec![sse_read_tool_call_turn(), sse_read_tool_call_turn()]).await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "probe payload\n").expect("write fixture");
        let mut runner =
            Runner::new(&wire_config(server.port, 2), Vec::new(), dir.path()).expect("constructs");
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("loop"))
            .await
            .expect("run terminates at the budget");
        assert!(
            matches!(
                result,
                Err(loopctl::error::LoopError::MaxTurnsExceeded { .. })
            ),
            "expected MaxTurnsExceeded, got {result:?}"
        );
    }
}
