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
//! optional question channel) through a [`RunnerContext`] extension. Because
//! [`BareLoop`] builds the per-dispatch [`ToolContext`] internally and never
//! injects host extensions, the runner wraps every builtin tool in a private
//! adapter that clones the incoming context, installs the extension, and
//! delegates to the inner tool.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use dch_tools::BashTool;
use dch_tools::CodeSearchTool;
use dch_tools::EditTool;
use dch_tools::FileViewerTool;
use dch_tools::GlobTool;
use dch_tools::GrepTool;
use dch_tools::MultiEditTool;
use dch_tools::ReadTool;
use dch_tools::RunnerContext;
use dch_tools::TreeTool;
use dch_tools::WriteTool;
use loopctl::engine::BareLoop;
use loopctl::engine::Loop;
use loopctl::engine::Run;
use loopctl::engine::RunConfig;
use loopctl::error::LoopError;
use loopctl::managers::LoopManagers;
use loopctl::observer::LoopObserver;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolRegistry;
use loopctl::tool::ToolSchema;
use serde_json::Value;

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
/// and the observer-bearing managers bundle) into one [`BareLoop`], and every
/// public method after that is a one-line delegation to it.
///
/// Construct once per agent identity with [`Runner::new`], then drive it with
/// [`Runner::run`]: each call is one prompt → loop → final answer against the
/// configured provider, sharing one conversation across calls. Cancellation is
/// cooperative via [`Runner::cancel`] or [`Runner::cancel_signal`].
pub struct Runner {
    /// The agent loop, monomorphized over the concrete [`DchClient`].
    ///
    /// Constructed once in [`Runner::new`] via `new_with_managers` with the
    /// wrapped tool registry, the composed session config, and the
    /// observer-carrying managers. Every public method delegates to it; the
    /// concrete (non-`dyn`) client keeps the per-turn LLM call statically
    /// dispatched.
    inner: BareLoop<DchClient>,

    /// The shared per-runner context, cloned into every tool dispatch.
    ///
    /// Built from `workdir` in [`Runner::new`] and carried by the private
    /// context-injecting tool wrappers installed in `inner`'s registry, so
    /// every builtin tool reaches `cwd`, the todo list, and the question
    /// channel through the `ToolContext` extension. Exposed read-only-ish via
    /// [`Runner::context`] for host wiring.
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
            question_tx: None,
        });

        let registry = build_wrapped_registry(&context);

        let session_config = build_session(config, &registry, workdir);
        let run_config = config.to_run_config();
        let client = crate::create_client(&config.api)?;

        let mut managers = LoopManagers::new();
        for observer in observers {
            managers.register_observer(observer);
        }

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
    /// budget, dispatch policy). Returns when the model signals end-of-turn,
    /// the budget is exhausted, the loop detector aborts, or [`Runner::cancel`]
    /// is called. Equivalent to [`Runner::run_with`] with the default config;
    /// the conversation is shared across `run`/`run_with` calls on the same
    /// `Runner`.
    ///
    /// # Errors
    ///
    /// - [`LoopError::MaxTurnsExceeded`] when the per-run turn budget is hit.
    /// - [`LoopError::Cancelled`] when [`Runner::cancel`] fires mid-run.
    /// - [`LoopError::Api`] / [`LoopError::StreamError`] on provider failures.
    /// - Other [`LoopError`] variants as surfaced by the loop (compaction,
    ///   detection, reflection, tool recovery).
    pub async fn run(&mut self, input: &str) -> Result<Run, LoopError> {
        self.inner.run(input, &self.run_config).await
    }

    /// Run one prompt → loop → final answer, with an explicit run policy.
    ///
    /// The variation point for per-run budgets: `run_with` overrides the
    /// session default for this one call without rebuilding the `Runner` (a
    /// rebuild constructs a fresh `BareLoop` and discards the conversation).
    /// The returned [`Run`] records `run_config` as its own snapshot, so
    /// per-call overrides are visible in the run's accounting. This mirrors
    /// loopctl's own default/override pairing (`stream_messages` vs
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
    /// `Arc<Mutex<…>>`, so callers can mutate it) and the optional question
    /// channel. Intended for host wiring — e.g. a TUI installing its question
    /// sender before the first run, or an observer reading the todo list — not
    /// for general inspection.
    #[must_use]
    pub fn context(&self) -> &RunnerContext {
        &self.context
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

/// Build the tool registry with every builtin wrapped so its dispatches see
/// `context` as the [`RunnerContext`] extension.
///
/// The builtin tools require the extension (e.g. [`ReadTool`] errors when it is
/// absent), and [`BareLoop`] builds the per-dispatch [`ToolContext`] internally
/// without injecting host extensions. Wrapping each concrete tool at registry
/// build time — clone the incoming context, install the extension, delegate —
/// makes the extension reachable with no loopctl change. Tool identity (name,
/// schema, prompt, dispatch metadata) is preserved exactly, so the model and
/// the dispatcher see the wrapped tools as indistinguishable from the originals.
///
/// The tool list mirrors [`dch_tools::builtin_registry`]; each concrete tool is
/// constructed, wrapped in a private `Contextual` adapter, and registered.
fn build_wrapped_registry(context: &Arc<RunnerContext>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Contextual::new(ReadTool, Arc::clone(context)));
    registry.register(Contextual::new(BashTool, Arc::clone(context)));
    registry.register(Contextual::new(WriteTool, Arc::clone(context)));
    registry.register(Contextual::new(EditTool, Arc::clone(context)));
    registry.register(Contextual::new(MultiEditTool, Arc::clone(context)));
    registry.register(Contextual::new(FileViewerTool, Arc::clone(context)));
    registry.register(Contextual::new(GlobTool, Arc::clone(context)));
    registry.register(Contextual::new(GrepTool, Arc::clone(context)));
    registry.register(Contextual::new(CodeSearchTool, Arc::clone(context)));
    registry.register(Contextual::new(TreeTool, Arc::clone(context)));
    registry
}

/// A [`Tool`] that injects a [`RunnerContext`] extension into the per-dispatch
/// [`ToolContext`] before delegating to `inner`.
///
/// See [`build_wrapped_registry`]. Every [`Tool`] method forwards to `inner`
/// unchanged; only [`call`](Tool::call) is non-trivial.
struct Contextual<T: Tool> {
    /// The real tool whose dispatches the wrapper injects the extension into.
    ///
    /// Every `Tool` method other than `call` delegates to this field verbatim,
    /// so the wrapped tool is indistinguishable from the original to the model
    /// and to the dispatcher.
    inner: T,
    /// The shared runner context, cloned into each dispatch's extension slot.
    ///
    /// Cloned (cheaply — `RunnerContext` is `Clone` with an `Arc`-shared todo
    /// list) on every `call` so each dispatch sees the current runner state.
    context: Arc<RunnerContext>,
}

impl<T: Tool> Contextual<T> {
    /// Wrap `tool` so every dispatch sees `context` as the extension.
    ///
    /// The wrapper is constructed once per builtin at registry-build time (see
    /// [`build_wrapped_registry`]) and registered in place of the bare tool.
    fn new(tool: T, context: Arc<RunnerContext>) -> Self {
        Self {
            inner: tool,
            context,
        }
    }
}

/// [`Tool`] implementation for [`Contextual`]: a transparent decorator that
/// preserves the inner tool's identity while injecting the shared
/// [`RunnerContext`] extension into the per-dispatch [`ToolContext`].
///
/// Every method except [`Tool::call`] is a plain delegation, so the model, the
/// parallel dispatcher, and the system-prompt composer all observe the wrapper
/// as indistinguishable from the original tool. Only `call` mutates the
/// incoming context — it clones the dispatcher-built `ToolContext`, installs
/// the extension, and forwards to the inner tool.
impl<T: Tool> Tool for Contextual<T> {
    /// Forward the inner tool's stable identifier.
    ///
    /// The model addresses tools by this name, so it must — and does — match
    /// the unwrapped builtin exactly.
    fn name(&self) -> &str {
        self.inner.name()
    }

    /// Forward the inner tool's human-readable description unchanged.
    ///
    /// This prose is embedded in the tool schema shown to the model, so any
    /// alteration here would change what the model believes the tool does.
    fn description(&self) -> &str {
        self.inner.description()
    }

    /// Forward the inner tool's JSON schema (argument shape + identity).
    ///
    /// The returned [`ToolSchema`] is what the provider receives, so forwarding
    /// verbatim keeps argument validation identical to the bare tool.
    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }

    /// Dispatch the tool with the shared [`RunnerContext`] injected.
    ///
    /// This is the one non-delegating method. The dispatcher hands us a
    /// [`ToolContext`] that carries no host extension; we clone it, install our
    /// cached `RunnerContext` as the extension, and forward the call to the
    /// inner tool. The clone is cheap (extensions are `Arc`-backed) and keeps
    /// the borrow lifetime tied to this dispatch only.
    ///
    /// # Errors
    ///
    /// Propagates whatever [`ToolError`] the inner tool yields — the wrapper
    /// adds no failure mode of its own.
    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>> {
        let mut owned = ctx.clone();
        owned.set_extension((*self.context).clone());
        Box::pin(async move { self.inner.call(input, &owned).await })
    }

    /// Forward the inner tool's static concurrency declaration.
    ///
    /// The parallel planner reads this to decide whether two instances of the
    /// tool may co-dispatch; a dropped forward would silently serialize
    /// read-only tools.
    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }

    /// Forward the inner tool's per-call concurrency check.
    ///
    /// Some tools are conditionally parallelizable (e.g. read paths yes, write
    /// paths no); this forwards the inner verdict for the given `input` so the
    /// planner's per-dispatch gating is preserved.
    fn is_safe_for_concurrent_execution(&self, input: &Value) -> bool {
        self.inner.is_safe_for_concurrent_execution(input)
    }

    /// Forward the inner tool's resource key, if any.
    ///
    /// The dispatcher uses this to serialize writes that touch the same
    /// resource (e.g. the same file path). Forwarding is mandatory: a `None`
    /// here would let two conflicting writes race silently.
    fn resource_key(&self, input: &Value) -> Option<String> {
        self.inner.resource_key(input)
    }

    /// Forward the inner tool's read-only flag.
    ///
    /// Read-only tools may be dispatched speculatively and in parallel; this
    /// forwards the inner classification so the wrapper does not accidentally
    /// demote a safe tool into the serialized queue.
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    /// Forward the inner tool's system-prompt fragment, if any.
    ///
    /// Tools contribute per-tool prose (usage hints, examples) to the composed
    /// system prompt; forwarding keeps that composition identical to the bare
    /// registry.
    fn system_prompt(&self) -> Option<String> {
        self.inner.system_prompt()
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
    use loopctl::tool::ToolContext;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// A tool that reports the `cwd` it sees via the `RunnerContext` extension,
    /// or the sentinel `"absent"` when no extension is installed. Used to prove
    /// `Contextual` actually injects the extension.
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
            let cwd = runner_ctx_cwd(ctx);
            Box::pin(async move { Ok(ToolOutput::text(cwd)) })
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn system_prompt(&self) -> Option<String> {
            Some("probe".to_string())
        }
    }

    /// A tool that overrides the dispatch-metadata methods the parallel planner
    /// queries (`is_concurrency_safe`, `is_safe_for_concurrent_execution`,
    /// `resource_key`), so the `Contextual` wrapper's forwarding of each can be
    /// asserted independently. The values are deliberately non-default so a
    /// dropped forward is detectable.
    struct MetadataProbe;

    impl Tool for MetadataProbe {
        fn name(&self) -> &'static str {
            "MetadataProbe"
        }
        fn description(&self) -> &'static str {
            "overrides dispatch metadata for forwarding tests"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                tool: "MetadataProbe".to_string(),
                description: "overrides dispatch metadata".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}}
                }),
            }
        }
        fn call<'a>(
            &'a self,
            _input: Value,
            _ctx: &ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>> {
            Box::pin(async { Ok(ToolOutput::text("ok")) })
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        fn is_safe_for_concurrent_execution(&self, input: &Value) -> bool {
            // Safe only when the input carries `"safe": true`.
            input.get("safe").and_then(Value::as_bool).unwrap_or(false)
        }
        fn resource_key(&self, input: &Value) -> Option<String> {
            input.get("path").and_then(Value::as_str).map(String::from)
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
            question_tx: None,
        })
    }

    #[tokio::test]
    async fn contextual_injects_the_extension_into_the_dispatch_context() {
        let context = sample_context("/tmp/probe-cwd");
        let wrapped = Contextual::new(ExtensionProbe, context);
        let ctx = ToolContext::default();
        let out = wrapped.call(Value::Null, &ctx).await.expect("probe runs");
        assert_eq!(out_text(&out), "/tmp/probe-cwd");
    }

    #[tokio::test]
    async fn without_contextual_the_extension_is_absent() {
        let out = ExtensionProbe
            .call(Value::Null, &ToolContext::default())
            .await
            .expect("probe runs");
        assert_eq!(out_text(&out), "absent");
    }

    #[test]
    fn contextual_preserves_tool_identity() {
        let context = sample_context("/tmp");
        let wrapped = Contextual::new(ExtensionProbe, context);
        assert_eq!(wrapped.name(), "Probe");
        assert_eq!(wrapped.description(), "reports the RunnerContext cwd");
        assert_eq!(wrapped.schema().tool, "Probe");
        assert_eq!(wrapped.system_prompt().as_deref(), Some("probe"));
        assert!(wrapped.is_read_only());
    }

    #[test]
    fn contextual_forwards_concurrency_metadata_to_the_inner_tool() {
        // The parallel planner queries these on the wrapped tool; a dropped
        // forward would silently serialize all parallel dispatch.
        let context = sample_context("/tmp");
        let wrapped = Contextual::new(MetadataProbe, context);
        assert!(
            wrapped.is_concurrency_safe(),
            "is_concurrency_safe must forward"
        );
        let safe = serde_json::json!({"safe": true});
        let unsafe_ = serde_json::json!({"safe": false});
        assert!(
            wrapped.is_safe_for_concurrent_execution(&safe),
            "is_safe_for_concurrent_execution must forward the true case"
        );
        assert!(
            !wrapped.is_safe_for_concurrent_execution(&unsafe_),
            "is_safe_for_concurrent_execution must forward the false case"
        );
    }

    #[test]
    fn contextual_forwards_resource_key_to_the_inner_tool() {
        // resource_key drives same-resource serialization for parallel writes;
        // a dropped forward would co-dispatch conflicting writes silently.
        let context = sample_context("/tmp");
        let wrapped = Contextual::new(MetadataProbe, context);
        let with_path = serde_json::json!({"path": "/tmp/a.txt"});
        assert_eq!(
            wrapped.resource_key(&with_path).as_deref(),
            Some("/tmp/a.txt"),
            "resource_key must forward"
        );
        let without_path = serde_json::json!({});
        assert!(
            wrapped.resource_key(&without_path).is_none(),
            "resource_key None case must forward"
        );
    }

    #[test]
    fn build_wrapped_registry_wraps_every_builtin() {
        // Guard against drift: build_wrapped_registry must register exactly the
        // set builtin_registry does, no more, no less. Comparing against the
        // single source of truth catches a future task (e.g. TodoTool) adding a
        // tool to builtin_registry but not to the wrapper.
        let context = sample_context("/tmp");
        let wrapped = build_wrapped_registry(&context);
        let mut expected = dch_tools::builtin_registry().tool_names();
        let mut actual = wrapped.tool_names();
        expected.sort();
        actual.sort();
        assert_eq!(
            actual, expected,
            "wrapped registry drifted from builtin_registry"
        );
    }

    #[test]
    fn build_session_composes_the_system_prompt_for_general_role() {
        let config = DchConfig::default();
        let context = sample_context("/tmp");
        let registry = build_wrapped_registry(&context);
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
        let context = sample_context("/tmp");
        let registry = build_wrapped_registry(&context);
        let session = build_session(&config, &registry, Path::new("/tmp"));
        let prompt = session.system_prompt.expect("composed");
        assert!(
            prompt.contains("OVERRIDE MARKER"),
            "override prose missing: {prompt}"
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
        let context = sample_context("/tmp");
        let registry = build_wrapped_registry(&context);
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
        let context = sample_context("/tmp");
        let registry = build_wrapped_registry(&context);
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

    fn out_text(out: &ToolOutput) -> String {
        out.text_content()
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
        let args = serde_json::json!({"file_path": "note.txt"}).to_string();
        [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1",
                    "function": {"name": "Read", "arguments": ""}
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
        // run proves Contextual injects through the actual BareLoop dispatch.
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
}
