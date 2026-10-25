//! The top-level agent: a thin owner of one configured `BareLoop` plus the
//! shared runner context.
//!
//! [`Runner`] is pure composition glue — it holds no agent-loop logic of its
//! own. [`Runner::builder`] constructs the loopctl ingredients (a concrete
//! provider client, a tool registry, a session config carrying the composed
//! system prompt, and the observer-bearing managers bundle), wires them into
//! a [`BareLoop`], and exposes [`Runner::run`] / [`Runner::cancel`] as
//! one-line delegations.
//!
//! The loop offers two extension axes with distinct contracts, and the
//! builder exposes both symmetrically:
//!
//! - **Observers** ([`LoopObserver`]) — passive, read-only lifecycle events
//!   (streaming deltas, tool pre/post, compaction, model switches). They
//!   never affect execution; a display, a logger, and a metrics recorder
//!   can all observe the same run.
//! - **Middleware** ([`ToolMiddleware`]) — intercepts every tool dispatch
//!   and may rewrite the context, the output, or the control flow. This is
//!   where the runner installs its context injector, the whole-result
//!   output cap, the permission gate, and (unless disabled) the
//!   secrets-redaction pass.
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
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use dch_tools::ResolvePolicy;
use dch_tools::RunnerContext;
use dch_tools::builtin_registry;
use loopctl::compact::ContextCompactor;
use loopctl::compact::ContextManager;
use loopctl::compact::TruncatingCompactor;
use loopctl::compact::qa_summarizer::QaSummarizer;
use loopctl::compact::qa_summarizer::QaSummarizerConfig;
use loopctl::compact::rolling::RollingCompactor;
use loopctl::compact::rolling::RollingConfig;
use loopctl::compact::structured_summarizer::StructuredSummarizer;
use loopctl::compact::structured_summarizer::StructuredSummaryConfig;
use loopctl::engine::BareLoop;
use loopctl::engine::Loop;
use loopctl::engine::LoopMachine;
use loopctl::engine::Run;
use loopctl::engine::RunConfig;
use loopctl::error::LoopError;
use loopctl::fallback::FallbackManager;
use loopctl::managers::LoopManagers;
use loopctl::mcp::CommandSpec;
use loopctl::mcp::McpClient;
use loopctl::mcp::McpToolProvider;
use loopctl::message::Message;
use loopctl::middleware::AskResolverFn;
use loopctl::middleware::OutputLimitMiddleware;
use loopctl::middleware::PermissionMiddleware;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolMiddleware;
use loopctl::middleware::ToolPipeline;
use loopctl::observer::LoopObserver;
use loopctl::stream::handler::StreamHandler;
use loopctl::stream::handler::StreamTimeoutConfig;
use loopctl::tool::Tool;
use loopctl::tool::ToolRegistry;
use loopctl::tool::builtin::fs::FileSession;
use loopctl::tool::builtin::fs::ResolvePolicy as FsResolvePolicy;

use crate::DchClient;
use crate::RunnerError;
use crate::detect_tech_stack;
use crate::merge_by_language;
use crate::permission::permission_layer;
use crate::permission::tools_mode;
use crate::question_bridge::QuestionPath;
use crate::with_context;

/// The top-level agent: the single type the rest of the application holds.
///
/// `Runner` is pure composition glue — it owns no agent-loop logic of its own.
/// [`Runner::builder`] assembles the loopctl ingredients (a concrete provider
/// client, a tool registry whose every dispatch sees the shared
/// [`RunnerContext`], a session config carrying the composed system prompt,
/// and the observer-bearing managers bundle) into one [`BareLoop`], and the
/// public surface stays thin: runs reset the per-run todo list and delegate
/// to the loop, the rest are accessors.
///
/// Construct once per agent identity with [`Runner::builder`]…`.build()`,
/// then drive it with [`Runner::run`]: each call is one prompt → loop →
/// final answer against the configured provider, sharing one conversation
/// across calls. Each run starts by resetting the per-run todo list (a new
/// prompt plans fresh). Cancellation is cooperative via [`Runner::cancel`]
/// or [`Runner::cancel_signal`].
pub struct Runner {
    /// The agent loop, monomorphized over the concrete [`DchClient`].
    ///
    /// Constructed once in [`RunnerBuilder::build`] via
    /// `from_machine_with_managers` with the builtin tool registry, the
    /// composed session config, and the observer-carrying managers (which
    /// carry the dispatch pipeline with the context injector installed)
    /// over a state machine seeded with the builder's prior history —
    /// empty for a fresh session. Every public method delegates to it;
    /// the concrete (non-`dyn`) client keeps the per-turn LLM call
    /// statically dispatched.
    inner: BareLoop<DchClient>,

    /// The window one request may carry, as this runner resolved it.
    ///
    /// The explicit config value, the provider's answer, or the
    /// fallback — in that precedence — captured once at construction
    /// so the display's context bar and any consumer can read the
    /// same denominator the thresholds enforce.
    context_window: u64,

    /// The shared per-runner context, cloned into every tool dispatch.
    ///
    /// Built from `workdir` in [`RunnerBuilder::build`] and carried by the
    /// private context-injector middleware in the dispatch pipeline, so every
    /// builtin tool reaches `cwd`, the todo list, and the question channel
    /// slot through the `ToolContext` extension. Exposed via
    /// [`Runner::context`] for host wiring, and mutated by the asking path
    /// [`Runner::question_path`] hands out.
    context: Arc<RunnerContext>,

    /// The filesystem session the builtin file family dispatches through.
    ///
    /// Built beside the context in [`RunnerBuilder::build`] and carried by
    /// the context injector into every dispatch, so the advertised tools
    /// and the executing tools share one baseline map and containment
    /// policy. Exposed for the resume re-arm through
    /// [`Runner::record_resumed_read`].
    fs_session: FileSession,

    /// The session-default run policy (turn budget, dispatch policy).
    ///
    /// Mapped once from `DchConfig` in [`RunnerBuilder::build`] and used by
    /// every [`Runner::run`] call — the underlying `Loop::run` contract takes
    /// the run config per call, so this field supplies dch's default. A
    /// caller that needs a different budget for one run passes an override to
    /// [`Runner::run_with`] rather than rebuilding the `Runner` (rebuilding
    /// creates a fresh `BareLoop` and discards the conversation).
    run_config: RunConfig,
}

impl Runner {
    /// Re-arm the write guard for a file a restored transcript shows read.
    ///
    /// Delegates to the filesystem session's own re-arm: the file's
    /// current bytes become the recorded baseline, marked as resumed —
    /// the model never saw these bytes in this session, so the first
    /// write still demands a fresh live read. Returns whether the path
    /// resolved and could be read and recorded.
    pub async fn record_resumed_read(&self, file_path: &str) -> bool {
        self.fs_session.record_resumed_read(file_path).await
    }

    /// Start building a runner for `config` operating within `workdir`.
    ///
    /// Returns a [`RunnerBuilder`]; register observers and dispatch
    /// middleware on it, then [`build`](RunnerBuilder::build) the runner.
    /// `workdir` is taken explicitly rather than read from the process's
    /// current directory so a resumed session or a test fixture can point
    /// the agent at the correct tree.
    #[must_use]
    pub fn builder<'a>(config: &'a dch_config::DchConfig, workdir: &Path) -> RunnerBuilder<'a> {
        RunnerBuilder {
            config,
            workdir: workdir.to_path_buf(),
            observers: Vec::new(),
            middleware: Vec::new(),
            mcp_providers: Vec::new(),
            history: Vec::new(),
            permission_resolver: None,
        }
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

    /// The session identity the inner engine owns.
    ///
    /// A UUID minted once when the engine is constructed and stable
    /// across every `run()` call on this runner — hosts key
    /// per-session artifacts (saved transcripts) by it. A resumed
    /// host keys by the restored session's id instead, so its
    /// artifacts keep appending to the resumed file; this accessor
    /// reports the engine's own identity either way. The runner
    /// never rotates it.
    #[must_use]
    pub fn session_id(&self) -> uuid::Uuid {
        self.inner.session().id
    }

    /// The assistant text of every turn the session completed, in
    /// order.
    ///
    /// Reads the engine's own run records, so turns a failed run
    /// completed before its error are included — a host saving a
    /// transcript after a failure still sees the assistant text that
    /// was produced. Pure tool-call turns carry no text and are
    /// skipped.
    #[must_use]
    pub fn session_turn_outputs(&self) -> Vec<String> {
        self.inner
            .session()
            .runs
            .iter()
            .flat_map(|run| run.turns.iter())
            .map(|turn| turn.output.clone())
            .filter(|output| !output.trim().is_empty())
            .collect()
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

    /// The shared steering slot, cloned from the inner loop.
    ///
    /// For the host that lets a user speak to a run already in flight:
    /// the returned handle reaches the slot the loop itself drains at
    /// each turn boundary, so guidance joins the conversation the model
    /// continues instead of opening another one. It is a clone rather
    /// than a borrow for the same reason as
    /// [`cancel_signal`](Runner::cancel_signal) — the task driving a
    /// session lives inside [`run`](Runner::run)'s mutable borrow, so a
    /// seam into the run has to reach it from outside the loop.
    #[must_use]
    pub fn steering(&self) -> Arc<loopctl::engine::Steering> {
        self.inner.steering()
    }

    /// A reference to the shared runner context.
    ///
    /// The returned `RunnerContext` exposes the shared todo list (behind an
    /// `Arc<Mutex<…>>`, so callers can mutate it) and the question channel
    /// slot. Intended for host wiring and inspection — a host that asks the
    /// user installs its channel through [`Runner::question_path`], or
    /// through [`install_question_tx`](RunnerContext::install_question_tx) on
    /// a clone when the installing task outlives this borrow.
    #[must_use]
    pub fn context(&self) -> &RunnerContext {
        &self.context
    }

    /// The engine's committed history, for snapshotting.
    ///
    /// The messages the loop has fully committed — never an in-flight
    /// turn's partials — which is exactly the working set a resume
    /// must seed from to continue this session process-identically.
    /// Read after a run resolves, so the snapshot carries every turn
    /// the completed submission produced.
    #[must_use]
    pub fn committed_history(&self) -> Vec<Message> {
        self.inner.machine().history().to_vec()
    }

    /// Cut the committed history back to a length seen before a turn.
    ///
    /// The host's way to un-ask a prompt: `rewind_to` on the loop's own
    /// machine, so the turn appended after `len` leaves the conversation
    /// the model reads, and the session goes on reading it as though that
    /// turn never ran. dch uses it when the user commits an edit to a
    /// prompt the run had already claimed — the corrected wording
    /// replaces the turn that went out rather than arguing with it in the
    /// same context.
    ///
    /// Valid between runs only: the machine refuses while a turn is in
    /// flight, and a cancelled run has to resolve first. The cut snaps to
    /// a turn boundary, so no tool result is orphaned from its call, and
    /// the loop keeps its machine, its observers, and every
    /// [`cancel_signal`](Self::cancel_signal) clone already handed out —
    /// nothing is rebuilt, which is the whole reason this exists instead
    /// of a fresh loop seeded with shorter history.
    ///
    /// Returns the number of messages removed; a `len` at or beyond the
    /// current length removes nothing and reports so.
    ///
    /// # Errors
    ///
    /// [`LoopError::InvalidInput`] when a turn is in flight — the run has
    /// to be cancelled and allowed to resolve first. Everything else is an
    /// `Ok`: a `len` past the end of the history is a no-op, not a failure.
    ///
    /// [`rewind_to`]: loopctl::engine::core::LoopMachine::rewind_to
    pub fn rewind_history_to(&mut self, len: usize) -> Result<usize, LoopError> {
        self.inner.rewind_history_to(len)
    }

    /// The context window this runner's thresholds divide by.
    ///
    /// The resolved value — explicit config over provider discovery
    /// over the fallback — as fed into the session config at
    /// construction.
    #[must_use]
    pub fn context_window(&self) -> u64 {
        self.context_window
    }

    /// This runner's handle on the path its tools ask questions through.
    ///
    /// An interactive host calls [`supervise`](QuestionPath::supervise) on
    /// the returned handle to put its prompt stream in place and keep it
    /// there for the session's whole life; a headless host asks for nothing,
    /// leaving the slot empty so the asking tool refuses rather than
    /// blocking. The handle shares this runner's context, and every tool
    /// dispatch reads the asking slot live, so an installation takes effect
    /// on the next call.
    #[must_use]
    pub fn question_path(&self) -> QuestionPath {
        QuestionPath::new(Arc::clone(&self.context))
    }
}

/// Incremental construction of a [`Runner`].
///
/// Created by [`Runner::builder`]; register extension points with
/// [`with_observer`](Self::with_observer) and
/// [`with_middleware`](Self::with_middleware), then
/// [`build`](Self::build). The two axes are independent and may each be
/// registered any number of times:
///
/// - Observers receive read-only lifecycle events and never affect
///   execution — displays, loggers, and metrics recorders coexist freely.
/// - Middleware intercepts every tool dispatch in registration order and may
///   rewrite the dispatch context, the tool output, or the control flow.
///
/// # Examples
///
/// ```no_run
/// # async fn example() {
/// use std::sync::Arc;
/// use dch_loop::Runner;
///
/// let config = dch_config::DchConfig::default();
/// let runner = Runner::builder(&config, std::path::Path::new("."))
///     .build()
///     .await;
/// # }
/// ```
pub struct RunnerBuilder<'a> {
    /// The application configuration the runner is built from.
    ///
    /// Read during [`build`](Self::build) to construct the provider client,
    /// connect any `[[mcp.servers]]` stdio servers, compose the session
    /// config, and arm the model fallback breaker when one is configured.
    config: &'a dch_config::DchConfig,

    /// The directory the agent operates within.
    ///
    /// Used as the working directory tools see in the dispatch context and as
    /// the root for tech-stack detection when composing the system prompt.
    workdir: PathBuf,

    /// Lifecycle observers, registered with the loop in the order added.
    ///
    /// Each observer receives every read-only lifecycle event; registration
    /// order determines only the notification order among them and never
    /// affects execution.
    observers: Vec<Arc<dyn LoopObserver>>,

    /// Host dispatch middleware installed by the host application.
    ///
    /// Installed under the runner's context injector and above the
    /// secrets-redaction pass, in the order added, so they see the enriched
    /// context and scrubbed output.
    middleware: Vec<Arc<dyn ToolMiddleware>>,

    /// Pre-connected MCP tool providers supplied by the host.
    ///
    /// Registered alongside any config-declared stdio servers — their tools
    /// join the registry beside the builtin tools in the order added. Use
    /// these for transports the config cannot express.
    mcp_providers: Vec<McpToolProvider>,

    /// Prior conversation the built loop starts from, empty for a fresh
    /// session.
    ///
    /// Seeded into the loop's state machine at construction (see
    /// [`RunnerBuilder::with_history`]); a `run` on the built runner
    /// continues from the restored point instead of an empty history.
    history: Vec<Message>,

    /// Optional async resolver for the permission gate's `Ask` verdicts.
    ///
    /// Interactive hosts (the TUI) supply one so `Ask` cells prompt the
    /// user; without it the gate denies asks outright, which is the
    /// headless contract. The resolver runs on the dispatch task, so it
    /// must not block — and a pending resolution races the run's cancel
    /// signal, so a cancelled run denies rather than waits.
    permission_resolver: Option<AskResolverFn>,
}

impl RunnerBuilder<'_> {
    /// Register a lifecycle observer.
    ///
    /// Observers may be added freely; each one receives every event. The
    /// registration order only determines notification order among them.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn LoopObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    /// Register a dispatch middleware.
    ///
    /// Middleware runs in registration order, inside the runner's context
    /// injector (so it sees the enriched
    /// [`ToolContext`](loopctl::tool::ToolContext)) and outside the
    /// secrets-redaction pass (so it observes scrubbed output). The same
    /// instance may be shared across runners.
    #[must_use]
    pub fn with_middleware(mut self, middleware: Arc<dyn ToolMiddleware>) -> Self {
        self.middleware.push(middleware);
        self
    }

    /// Register an already-connected MCP tool provider.
    ///
    /// The provider's tools join the registry beside the builtin tools (and
    /// beside any `[[mcp.servers]]` stdio connections from the config). Use
    /// this for transports the config cannot express — an in-process server or
    /// an HTTP endpoint with a custom client.
    #[must_use]
    pub fn with_mcp_provider(mut self, provider: McpToolProvider) -> Self {
        self.mcp_providers.push(provider);
        self
    }

    /// Start the built loop from a prior conversation instead of an empty
    /// one.
    ///
    /// The history is seeded into the loop's state machine, so the first
    /// [`run`](Runner::run) on the built runner continues the restored
    /// conversation rather than beginning a new one — the resume path's
    /// reconstruction step. Provide a well-formed conversation: user
    /// and assistant messages alternating — a turn's tool results
    /// ride the user message that follows their calls — and every
    /// tool call paired with a matching tool result. A transcript
    /// ending mid-turn leaves its final tool results as the
    /// conversation's last user message on their own. The loop does
    /// not validate the shape; providers that reject it surface the
    /// refusal as an API error on the first run. Observers and
    /// middleware registered on the builder keep receiving every
    /// event of the continued session — the seeding touches only the
    /// conversation.
    #[must_use]
    pub fn with_history(mut self, history: Vec<Message>) -> Self {
        self.history = history;
        self
    }

    /// Register the resolver the permission gate consults on `Ask`.
    ///
    /// The gate is always installed — it enforces the configured
    /// `[runner] permission_mode` matrix on every dispatch; the
    /// resolver decides what an `Ask` cell does. Without one, `Ask`
    /// denies (headless); with one, the resolver's `bool` answer
    /// allows or denies the call. The same resolver serves every run
    /// of the built runner.
    #[must_use]
    pub fn with_permission_resolver(mut self, resolver: AskResolverFn) -> Self {
        self.permission_resolver = Some(resolver);
        self
    }

    /// Assemble the [`Runner`].
    ///
    /// Constructs the provider client, composes the system prompt (role,
    /// tech stack detected under the workdir merged with `[project]`
    /// overrides, per-tool fragments), builds the dispatch pipeline
    /// (context injector → whole-result output cap → permission gate →
    /// host middleware → secrets redaction when enabled → builtin tools),
    /// installs the compactor
    /// `[runner] compactor` selects (the qa summarizer by default, bound
    /// to the same client the conversation itself uses), installs the
    /// resilient stream handler (retry ladder with jittered backoff,
    /// rate-limit backoff, non-streaming transport fallback — a transient
    /// stream failure retries instead of failing the run), and — when
    /// `api.fallback_model` is configured — arms the model fallback
    /// breaker so a failing primary is routed around automatically.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::Client`] if the provider client cannot be constructed
    ///   (missing API key, HTTP client failure).
    /// - [`RunnerError::Config`] if the fallback breaker cannot be armed, an
    ///   `[[mcp.servers]]` entry fails to spawn or handshake, or the dispatch
    ///   pipeline cannot be built.
    pub async fn build(self) -> Result<Runner, RunnerError> {
        let context = Arc::new(
            RunnerContext::new(self.workdir.clone())
                .with_resolve_policy(resolve_policy_for(&self.config.runner)),
        );
        let fs_session = FileSession::new(self.workdir.clone())
            .with_resolve_policy(fs_resolve_policy_for(&self.config.runner));

        let client = Arc::new(crate::create_client(&self.config.api)?);
        let connections = connect_mcp_servers(self.config, &context.cwd).await?;
        let read_windows = read_windows_for(&self.config.runner);
        let registry = compose_registry(&connections, &fs_session, read_windows);
        let core_registry = compose_registry(&connections, &fs_session, read_windows);
        for provider in &self.mcp_providers {
            provider.register_into(&registry);
            provider.register_into(&core_registry);
        }

        let mut session_config = build_session(self.config, &registry, &self.workdir);
        let mut resolved_window = session_config.context_window;
        if self.config.api.context_window.is_none()
            && let Some(discovered) =
                crate::context_probe::discover_window(&self.config.api, client.as_ref()).await
        {
            tracing::debug!(
                window = discovered,
                "context window discovered from the provider"
            );
            session_config.context_window = discovered;
            resolved_window = discovered;
        }
        let run_config = self.config.to_run_config();

        let timeout_config = StreamTimeoutConfig {
            total_stream_timeout: std::time::Duration::from_secs(
                self.config.runner.turn_timeout_secs,
            ),
            ..StreamTimeoutConfig::default()
        };
        let mut managers = LoopManagers::new()
            .with_stream_handler(StreamHandler::new().with_timeout_config(timeout_config))
            .with_context_manager(Arc::new(context_manager_for(
                self.config.runner.compactor,
                &Arc::new(crate::provider::create_compactor_client(&self.config.api)?),
                &session_config,
            )));
        for observer in self.observers {
            managers.register_observer(observer);
        }
        if let Some(fallback_model) = &self.config.api.fallback_model {
            managers =
                managers.with_fallback(arm_fallback(&self.config.api.model, fallback_model)?);
        }
        let gate = permission_layer(
            tools_mode(self.config.runner.permission_mode),
            self.permission_resolver,
        );
        managers.set_pipeline(build_pipeline(
            &context,
            &fs_session,
            &self.middleware,
            gate,
            self.config.security.redact_secrets,
            self.config.runner.max_tool_output_chars,
            core_registry,
        )?);

        let inner = BareLoop::<DchClient>::from_machine_with_managers(
            LoopMachine::from_history(self.history),
            session_config,
            client,
            registry,
            managers,
        );

        Ok(Runner {
            context_window: resolved_window,
            inner,
            context,
            fs_session,
            run_config,
        })
    }
}

/// Spawn and adapt every config-declared MCP server.
///
/// Each `[[mcp.servers]]` entry is launched over the stdio transport and its
/// tools discovered under the server's name prefix. A server that fails to
/// spawn or complete the MCP handshake fails construction naming the server —
/// a silently missing tool set is worse than a clear startup error. The child
/// runs in `workdir`, the same directory builtin tools resolve paths against.
///
/// # Errors
///
/// Returns [`RunnerError::Config`] naming the failing server on any transport
/// or handshake failure.
async fn connect_mcp_servers(
    config: &dch_config::DchConfig,
    workdir: &Path,
) -> Result<Vec<McpConnection>, RunnerError> {
    let mut connections = Vec::new();
    for server in &config.mcp.servers {
        let command = CommandSpec {
            program: server.command.clone(),
            args: server.args.clone(),
            env: server
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            cwd: Some(workdir.to_string_lossy().into_owned()),
        };
        let client = McpClient::stdio(command)
            .await
            .map_err(|e| RunnerError::Config(format!("mcp server '{}': {e}", server.name)))?;
        connections.push(adopt_mcp_server(server, client).await?);
    }
    Ok(connections)
}

/// Connect an already-running MCP `client` as `server` and snapshot
/// its tools under the server's allowlist.
///
/// The provider is connected first; each allowlisted name is then
/// checked against what the server actually exposed, so a listed
/// name the server does not offer fails construction rather than
/// silently dropping a tool.
///
/// # Errors
///
/// Returns [`RunnerError::Config`] when the handshake fails or an
/// allowlisted tool name is not offered by the server.
async fn adopt_mcp_server(
    server: &dch_config::McpServerConfig,
    client: McpClient,
) -> Result<McpConnection, RunnerError> {
    let provider = McpToolProvider::connect(client, Some(server.name.clone()))
        .await
        .map_err(|e| RunnerError::Config(format!("mcp server '{}': {e}", server.name)))?;
    let allowed = match &server.tools {
        None => None,
        Some(names) => {
            let exposed: std::collections::HashSet<String> = provider
                .tools()
                .iter()
                .map(|tool| tool.name().to_string())
                .collect();
            let mut allowlist = std::collections::HashSet::new();
            for name in names {
                let exposed_name = format!("{}__{name}", server.name);
                if !exposed.contains(&exposed_name) {
                    return Err(RunnerError::Config(format!(
                        "mcp server '{}': tool '{name}' is not offered by the server",
                        server.name
                    )));
                }
                allowlist.insert(exposed_name);
            }
            Some(allowlist)
        }
    };
    Ok(McpConnection { provider, allowed })
}

/// One connected MCP server, adapted into the registry alongside its
/// containment policy.
///
/// Built by [`adopt_mcp_server`] and consumed by [`compose_registry`]:
/// the provider carries the discovered tools, the allowlist decides
/// which of them a composition registers.
struct McpConnection {
    /// The connected provider.
    ///
    /// `tools` lists what the server exposed at discovery; `register_into`
    /// adapts them into a [`ToolRegistry`].
    provider: McpToolProvider,

    /// The exposed (prefixed) tool names to adapt.
    ///
    /// `None` adapts every discovered tool; a populated set adapts only the
    /// listed ones and leaves the rest unregistered.
    allowed: Option<std::collections::HashSet<String>>,
}

/// The resolve policy a runner context carries for the given runner config.
///
/// `unsafe_paths` is the explicit opt-out: off (the default) keeps every
/// file tool confined to the working directory, on lets them reach any path
/// the OS permits.
fn resolve_policy_for(runner: &dch_config::RunnerConfig) -> ResolvePolicy {
    if runner.unsafe_paths {
        ResolvePolicy::Unrestricted
    } else {
        ResolvePolicy::Contained
    }
}

/// The resolve policy the filesystem session carries for the given config.
///
/// The same `unsafe_paths` switch mapped onto the filesystem family's own
/// policy type, so the builtin read and writing tools judge containment by
/// the identical setting the search tools see.
fn fs_resolve_policy_for(runner: &dch_config::RunnerConfig) -> FsResolvePolicy {
    if runner.unsafe_paths {
        FsResolvePolicy::Unrestricted
    } else {
        FsResolvePolicy::Contained
    }
}

/// Compose the full tool registry: the builtin tools plus every MCP server's.
///
/// Each server contributes per its containment policy: a connection without
/// an allowlist registers every discovered tool, an allowlisted connection
/// only the listed ones. Called once per consumer — the engine's by-value
/// registry and the dispatch pipeline's core cannot share boxed tools, so
/// each gets its own composition from the same connections (an MCP tool
/// clones into both, keeping the two positions identical by construction).
fn compose_registry(
    connections: &[McpConnection],
    fs_session: &FileSession,
    windows: dch_tools::ReadWindows,
) -> ToolRegistry {
    let registry = builtin_registry(fs_session, windows);
    for connection in connections {
        match &connection.allowed {
            None => connection.provider.register_into(&registry),
            Some(allowed) => {
                for tool in connection.provider.tools() {
                    if allowed.contains(tool.name()) {
                        registry.register(tool.clone());
                    }
                }
            }
        }
    }
    registry
}

/// The read-family window ceilings the config's `[runner]` section selects.
///
/// Maps the config fields straight onto dch-tools' [`ReadWindows`], keeping
/// the runner the only place config becomes tool construction. The
/// conversation's ceiling on one tool result doubles as the `ReadFiles`
/// batch budget, so the per-file ceiling, the batch budget, and the
/// dispatched-result cap come from one place and cannot drift apart.
fn read_windows_for(runner: &dch_config::RunnerConfig) -> dch_tools::ReadWindows {
    dch_tools::ReadWindows {
        max_lines: runner.read_max_lines,
        max_bytes: runner.read_max_bytes,
        max_batch_bytes: runner.max_tool_output_chars,
    }
}

/// Arm the model-fallback breaker for a primary/fallback pair.
///
/// The breaker starts closed: the primary serves until it accumulates enough
/// failures to trip, after which the fallback serves until a recovery probe
/// on the primary succeeds.
///
/// # Errors
///
/// Returns [`RunnerError::Config`] only when the breaker's internal state
/// lock is poisoned.
fn arm_fallback(primary: &str, fallback: &str) -> Result<FallbackManager, RunnerError> {
    let manager = FallbackManager::for_model(primary)
        .and_then(|m| {
            m.set_fallback_models(vec![fallback.to_string()])
                .map(|()| m)
        })
        .map_err(|e| RunnerError::Config(format!("fallback breaker: {e}")))?;
    Ok(manager)
}

/// Build the context manager around exactly the compactor the
/// configuration names.
///
/// The selection is the whole contract: `qa` runs the question/answer
/// summarizer, `structured` the sectioned summarizer, `rolling` the
/// bounded aging pass, and `truncating` the oldest-drop — each bound to
/// the session's own client, the same conversation model, no second
/// endpoint to configure. No other compactor ever runs in the selected
/// one's place: a stage whose model call fails fails its pass, the
/// engine's threshold machinery defers while the payload fits the
/// window and retries on the next turn, and the emergency line ends the
/// run honestly rather than silently swapping strategy. The manager
/// carries the session's window and threshold exactly as the engine's
/// implicit default would have, so the selection changes only the
/// compactor, never the policy.
fn context_manager_for(
    selection: dch_config::CompactorSelection,
    client: &Arc<DchClient>,
    session_config: &loopctl::config::SessionConfig,
) -> ContextManager {
    let shared = Arc::clone(client) as loopctl::api::SharedApiClient;
    let compactor: Arc<dyn ContextCompactor> = match selection {
        dch_config::CompactorSelection::Truncating => Arc::new(TruncatingCompactor::default()),
        dch_config::CompactorSelection::Qa => {
            Arc::new(QaSummarizer::new(shared, QaSummarizerConfig::default()))
        }
        dch_config::CompactorSelection::Structured => Arc::new(StructuredSummarizer::new(
            shared,
            StructuredSummaryConfig::default(),
        )),
        dch_config::CompactorSelection::Rolling => {
            Arc::new(RollingCompactor::new(shared, RollingConfig::default()))
        }
    };
    ContextManager::new(compactor)
        .with_context_window(session_config.context_window)
        .with_threshold(session_config.compact_threshold)
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
/// Extracted as a free function (rather than inlined in
/// [`RunnerBuilder::build`]) so the composition can be unit-tested without
/// constructing a provider client.
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
/// the same providers by [`compose_registry`], keeping the advertised and
/// executable tool sets identical by construction.
///
/// # Errors
///
/// - [`RunnerError::Config`] if the builder rejects the composition — cannot
///   happen with this fixed stack (the core is always set), but mapped to a
///   typed error rather than panicked on.
fn build_pipeline(
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

    fn sample_context(cwd: &str) -> Arc<RunnerContext> {
        Arc::new(RunnerContext::new(PathBuf::from(cwd)))
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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

    #[test]
    fn read_windows_for_carries_the_output_cap_into_the_batch_budget() {
        let runner = dch_config::RunnerConfig {
            max_tool_output_chars: 4096,
            read_max_bytes: 2048,
            ..dch_config::RunnerConfig::default()
        };
        let windows = read_windows_for(&runner);
        assert_eq!(
            (windows.max_bytes, windows.max_batch_bytes),
            (2048, 4096),
            "the per-file ceiling and the batch budget come from the same config\n\
             pair as the dispatched-result cap, so the three cannot drift"
        );
        let disabled = dch_config::RunnerConfig {
            max_tool_output_chars: 0,
            ..dch_config::RunnerConfig::default()
        };
        assert_eq!(
            read_windows_for(&disabled).max_batch_bytes,
            0,
            "the disable sentinel maps through rather than collapsing to a default"
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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

    #[test]
    fn unsafe_paths_config_wires_unrestricted_resolution() {
        // The config switch maps onto both policy tracks the runner carries:
        // the search tools' policy and the file family's own. Anything the
        // config leaves unset stays contained on both.
        let config = dch_config::DchConfig::default();
        assert_eq!(
            resolve_policy_for(&config.runner),
            ResolvePolicy::Contained,
            "the default config keeps tools contained"
        );
        assert_eq!(
            fs_resolve_policy_for(&config.runner),
            FsResolvePolicy::Contained,
            "the default config keeps the file family contained"
        );
        let mut opted_out = dch_config::DchConfig::default();
        opted_out.runner.unsafe_paths = true;
        assert_eq!(
            resolve_policy_for(&opted_out.runner),
            ResolvePolicy::Unrestricted,
            "the opt-out must lift containment"
        );
        assert_eq!(
            fs_resolve_policy_for(&opted_out.runner),
            FsResolvePolicy::Unrestricted,
            "the opt-out must lift the file family's containment too"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reasoning_content_deltas_reach_the_observer_through_the_engine() {
        // The canned stream's `reasoning_content` deltas parse as
        // thinking parts, the engine forwards them as
        // `on_thinking_delta`, and the reply still graduates — all
        // offline against the local SSE server.
        struct ThinkingRecorder {
            deltas: Arc<Mutex<Vec<String>>>,
        }
        impl LoopObserver for ThinkingRecorder {
            fn name(&self) -> &'static str {
                "thinking-recorder"
            }
            fn on_thinking_delta(&self, ctx: &loopctl::observer::ThinkingDeltaContext) {
                self.deltas
                    .lock()
                    .expect("the deltas lock")
                    .push(ctx.delta.clone());
            }
        }
        let turn = [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"reasoning_content": "step one, "}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"reasoning_content": "step two"}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"content": "the answer"}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": null, "finish_reason": "stop"}]
            }),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();
        let server = SseServer::start(vec![turn]).await;
        let dir = TempDir::new().expect("tempdir");
        let recorder = Arc::new(ThinkingRecorder {
            deltas: Arc::new(Mutex::new(Vec::new())),
        });
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_observer(Arc::clone(&recorder) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("think"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        assert_eq!(run.output.as_deref(), Some("the answer"));
        assert_eq!(
            recorder.deltas.lock().expect("the deltas lock").as_slice(),
            ["step one, ", "step two"],
            "the reasoning deltas arrive in order through the engine"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_contained_runner_refuses_a_read_that_escapes_the_workdir() {
        // The file family judges containment by the session the runner
        // attaches, so the contained default must refuse an escaping read
        // through the real dispatch path — not merely by construction.
        let server = SseServer::start(vec![
            sse_tool_call_turn("Read", &serde_json::json!({"path": "../secret.txt"})),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        std::fs::write(outside.path().join("secret.txt"), "SHOULD NOT BE READ")
            .expect("seed outside target");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("read it"))
            .await
            .expect("run settles within timeout")
            .expect("a refused tool is a soft error, not a run failure");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && requests[1].contains("escapes the working directory"),
            "the containment refusal must ride the follow-up request: {requests:?}"
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

    /// Send one question through the slot's channel, as a dispatch would.
    ///
    /// Performs the same live slot read a tool call performs and drops the
    /// answer channel, since these pins assert on the prompt side.
    fn ask_through_the_slot(runner: &Runner, question: &str) {
        let sender = runner
            .context()
            .question_tx
            .lock()
            .expect("slot lock")
            .clone()
            .expect("an installed channel is what every dispatch reads");
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        sender
            .send(dch_tools::QuestionRequest {
                questions: vec![dch_tools::Question {
                    question: question.to_string(),
                    header: None,
                    options: Vec::new(),
                    multi_select: false,
                    response_tx,
                }],
            })
            .expect("the asking channel's receiver is alive");
    }

    #[tokio::test]
    async fn question_path_installs_the_channel_into_the_shared_context() {
        let dir = TempDir::new().expect("tempdir");
        let runner = Runner::builder(&offline_config(), dir.path())
            .build()
            .await
            .expect("Runner::new constructs");
        let mut prompts = runner.question_path().install();
        ask_through_the_slot(&runner, "wired?");
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), prompts.recv())
            .await
            .expect("the installed stream carries the request")
            .expect("the bridge stays alive");
        assert_eq!(prompt.question, "wired?");
    }

    #[tokio::test]
    async fn session_id_is_stable_and_non_nil() {
        let mut config = offline_config();
        config.api.base_url = "http://127.0.0.1:1".to_string();
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("Runner::new constructs");
        let first = runner.session_id();
        assert!(
            !first.is_nil(),
            "the engine mints a real UUID at construction"
        );
        drop(runner.run("first").await);
        drop(runner.run("second").await);
        assert_eq!(
            runner.session_id(),
            first,
            "the identity survives run boundaries — two refused runs did not rotate it"
        );
    }

    #[tokio::test]
    async fn a_built_runner_carries_the_seeded_history() {
        let config = offline_config();
        let dir = TempDir::new().expect("tempdir");
        let history = vec![
            loopctl::message::Message::user("earlier turn"),
            loopctl::message::Message::assistant("earlier answer"),
        ];
        let runner = Runner::builder(&config, dir.path())
            .with_history(history.clone())
            .build()
            .await
            .expect("Runner::new constructs");
        let conversation = runner.inner.conversation();
        assert_eq!(
            conversation.len(),
            history.len(),
            "the built loop must start from the restored conversation, not an empty one"
        );
        for (seeded, restored) in history.iter().zip(&conversation) {
            assert_eq!(restored.role, seeded.role, "roles survive the seeding");
            match (seeded.parts.first(), restored.parts.first()) {
                (
                    Some(loopctl::message::MessagePart::Text { text: seeded_text }),
                    Some(loopctl::message::MessagePart::Text {
                        text: restored_text,
                    }),
                ) => assert_eq!(
                    restored_text, seeded_text,
                    "the text parts survive the seeding verbatim"
                ),
                parts => panic!("both fixtures are single-text-part messages: {parts:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_runner_without_seeded_history_starts_empty() {
        let config = offline_config();
        let dir = TempDir::new().expect("tempdir");
        let runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("Runner::new constructs");
        assert!(
            runner.inner.conversation().is_empty(),
            "a fresh session must not inherit any conversation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_seeded_runner_sends_the_restored_history_on_the_first_request() {
        let server = SseServer::start(vec![sse_text_turn("continued")]).await;
        let dir = TempDir::new().expect("tempdir");
        let history = vec![
            loopctl::message::Message::user("earlier turn"),
            loopctl::message::Message::assistant("earlier answer"),
        ];
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .with_history(history)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("fresh"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.output.as_deref(), Some("continued"));
        let requests = recorded_requests(&server);
        assert_eq!(requests.len(), 1, "one text turn, one request");
        assert!(
            requests[0].contains("earlier turn")
                && requests[0].contains("earlier answer")
                && requests[0].contains("fresh"),
            "the restored conversation rides the first provider request ahead of the new prompt"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observers_fire_in_lifecycle_order_on_a_seeded_runner() {
        let server = SseServer::start(vec![sse_text_turn("hello")]).await;
        let dir = TempDir::new().expect("tempdir");
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .with_history(vec![
                loopctl::message::Message::user("earlier turn"),
                loopctl::message::Message::assistant("earlier answer"),
            ])
            .build()
            .await
            .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(
            observer.snapshot(),
            vec!["run_start", "turn_start", "response", "turn_end", "run_end"],
            "the continued session is observed exactly like a fresh one"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_turns_survive_a_later_failure_in_the_same_run() {
        // Turn 1 answers with text and a Read call, so the loop runs
        // a second model turn; that call is denied permanently. The
        // run fails, but turn 1's text was recorded before the
        // failure and must remain readable.
        let denied = r#"{"error":{"message":"invalid api key"}}"#.to_string();
        let first_turn = [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"content": "first turn complete"}, "finish_reason": null}]
            }),
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
                    "function": {"arguments": "{\"path\":\"note.txt\"}"}}
                ]}, "finish_reason": null}]
            }),
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": null, "finish_reason": "tool_calls"}]
            }),
        ]
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();
        let server = SseServer::start_canned(vec![
            CannedResponse {
                status: 200,
                body: first_turn,
            },
            CannedResponse {
                status: 401,
                body: denied,
            },
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let config = wire_config(server.port, 10);
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        let result = runner.run("do it").await;
        assert!(result.is_err(), "the second model call fails the run");
        assert_eq!(
            runner.session_turn_outputs(),
            vec!["first turn complete".to_string()],
            "the text of a turn completed before the failure is retained"
        );
    }

    #[tokio::test]
    async fn question_path_replaces_an_existing_channel() {
        let dir = TempDir::new().expect("tempdir");
        let runner = Runner::builder(&offline_config(), dir.path())
            .build()
            .await
            .expect("Runner::new constructs");
        let mut retired = runner.question_path().install();
        let mut current = runner.question_path().install();
        ask_through_the_slot(&runner, "second?");
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), current.recv())
            .await
            .expect("the replacement stream receives")
            .expect("the replacement bridge stays alive");
        assert_eq!(prompt.question, "second?");
        let retired_closed =
            tokio::time::timeout(std::time::Duration::from_secs(5), retired.recv())
                .await
                .expect("the replaced stream closes")
                .is_none();
        assert!(retired_closed, "the replaced generation no longer receives");
    }

    #[test]
    fn build_session_composes_the_system_prompt_for_general_role() {
        let config = DchConfig::default();
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
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
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
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
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
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
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
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

    #[tokio::test]
    async fn runner_builder_constructs_offline_and_cancel_signal_trips() {
        // Runner::new must construct without a network connection (the provider
        // builder is lazy), and cancel()/cancel_signal() must delegate to the
        // BareLoop. Cancelling trips the shared signal a subsequent run would
        // observe.
        let dir = TempDir::new().expect("tempdir");
        let config = offline_config();
        let runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("Runner::new constructs offline");
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
        sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"}))
    }

    /// A canned turn carrying several complete tool calls at once.
    ///
    /// Emits every call fully formed in a single delta (distinct indices
    /// and ids, arguments complete), then the `tool_calls` stop — the
    /// wire shape of a model that batches independent calls into one
    /// response instead of spending a round-trip per call.
    fn sse_batched_tool_calls_turn(calls: &[(&str, &serde_json::Value)]) -> String {
        let entries: Vec<serde_json::Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (tool, args))| {
                serde_json::json!({
                    "index": i,
                    "id": format!("call_{}", i + 1),
                    "function": {"name": tool, "arguments": args.to_string()}
                })
            })
            .collect();
        [
            serde_json::json!({
                "id": "c1", "model": "test-model",
                "choices": [{"delta": {"tool_calls": entries}, "finish_reason": null}]
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

    /// A one-text-part Gemini streaming turn that finishes with STOP.
    ///
    /// Gemini's SSE shape: each `data:` line is a complete JSON object
    /// carrying `candidates[0].content.parts[]` fragments, and the
    /// terminal chunk carries `finishReason` plus `usageMetadata`. No
    /// `[DONE]` sentinel exists on this wire — the stream simply ends.
    fn gemini_sse_text_turn(text: &str) -> String {
        let chunks = [
            serde_json::json!({
                "candidates": [{
                    "content": {"parts": [{"text": text}], "role": "model"}
                }]
            }),
            serde_json::json!({
                "candidates": [{"finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 6}
            }),
        ];
        let mut body = String::new();
        for chunk in chunks {
            body.push_str("data: ");
            body.push_str(&chunk.to_string());
            body.push_str("\n\n");
        }
        body
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

    /// One canned HTTP response: status code plus body.
    ///
    /// The server consumes these one per connection, in order, so a
    /// test scripts a whole multi-request conversation — including
    /// its failures — by stacking them.
    #[derive(Clone)]
    struct CannedResponse {
        /// HTTP status the server answers with.
        ///
        /// Any code the provider client can receive; non-2xx bodies
        /// become provider errors the run surfaces.
        status: u16,
        /// Response body bytes.
        ///
        /// Verbatim on the wire — for SSE responses, the full
        /// `data:`-event stream the client parses.
        body: String,
    }

    impl SseServer {
        async fn start(responses: Vec<String>) -> Self {
            Self::start_with_delay(responses, std::time::Duration::ZERO).await
        }

        async fn start_canned(responses: Vec<CannedResponse>) -> Self {
            Self::start_canned_with_delay(responses, std::time::Duration::ZERO).await
        }

        async fn start_with_delay(responses: Vec<String>, delay: std::time::Duration) -> Self {
            let canned = responses
                .into_iter()
                .map(|body| CannedResponse { status: 200, body })
                .collect();
            Self::start_canned_with_delay(canned, delay).await
        }

        async fn start_canned_with_delay(
            responses: Vec<CannedResponse>,
            delay: std::time::Duration,
        ) -> Self {
            Self::start_canned_with_completion(responses, delay, None).await
        }

        /// Serve `streamed` to streaming requests and `completion` to the rest.
        ///
        /// Compaction summarizers issue plain JSON completion calls alongside
        /// the conversation's SSE turns; serving one fixed body to every
        /// non-streaming request lets a test drive a compacting session
        /// without knowing how many summarizer calls a pass makes — the
        /// recorded requests still show each one.
        async fn start_with_completion(streamed: Vec<String>, completion: String) -> Self {
            let canned = streamed
                .into_iter()
                .map(|body| CannedResponse { status: 200, body })
                .collect();
            Self::start_canned_with_completion(
                canned,
                std::time::Duration::ZERO,
                Some(CannedResponse {
                    status: 200,
                    body: completion,
                }),
            )
            .await
        }

        /// Serve `streamed` to streaming requests; refuse every other one.
        ///
        /// The mirror of [`Self::start_with_completion`]: each
        /// non-streaming request — a compaction summarizer's call — draws a
        /// 500, so a test can pin the fallback floor carrying the pass when
        /// the summarizer endpoint is down.
        async fn start_with_failing_completion(streamed: Vec<String>) -> Self {
            let canned = streamed
                .into_iter()
                .map(|body| CannedResponse { status: 200, body })
                .collect();
            Self::start_canned_with_completion(
                canned,
                std::time::Duration::ZERO,
                Some(CannedResponse {
                    status: 500,
                    body: "{\"error\":{\"message\":\"summarizer endpoint unavailable\"}}"
                        .to_string(),
                }),
            )
            .await
        }

        async fn start_canned_with_completion(
            responses: Vec<CannedResponse>,
            delay: std::time::Duration,
            completion: Option<CannedResponse>,
        ) -> Self {
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
                let mut queue: std::collections::VecDeque<CannedResponse> = responses.into();
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let mut request_text = None;
                    if let (Some(req), Ok(mut recorded)) =
                        (read_http_request(&mut sock).await, task_requests.lock())
                    {
                        recorded.push(req.clone());
                        request_text = Some(req);
                    }
                    if delay > std::time::Duration::ZERO {
                        tokio::time::sleep(delay).await;
                    }
                    let canned = match (&completion, request_text.as_deref()) {
                        (Some(completion), Some(req)) if req.contains("\"stream\":false") => {
                            Some(completion.clone())
                        }
                        _ => queue.pop_front(),
                    };
                    let bytes = canned.map_or_else(
                        || {
                            "HTTP/1.1 500 No canned response\r\nContent-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                                .to_string()
                        },
                        |r| {
                            let content_type = if r.status == 200 {
                                "text/event-stream"
                            } else {
                                "application/json"
                            };
                            format!(
                                "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                                r.status,
                                reason_phrase(r.status),
                                r.body.len(),
                                r.body
                            )
                        },
                    );
                    let _ = sock.write_all(bytes.as_bytes()).await;
                }
            });
            Self { port, requests }
        }
    }

    /// Reason phrase for the statuses the canned server emits.
    ///
    /// The hand-written HTTP response needs one per status line;
    /// keeping the table local avoids an http-crate dependency just
    /// to spell "Unauthorized" in a fixture.
    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
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
    ///
    /// Gives the ordering pins something deterministic to read: each
    /// callback appends its name, and the finished run's sequence is
    /// asserted against the expected lifecycle order.
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
        fn on_compaction(&self, _: &loopctl::observer::CompactedContext) {
            self.record("compaction");
        }
        fn on_compaction_failed(&self, _: &loopctl::observer::CompactionFailedContext) {
            self.record("compaction_failed");
        }
        fn on_pre_compaction(&self, _: &loopctl::observer::PreCompactionContext) {
            self.record("pre_compaction");
        }
    }

    fn wire_config(port: u16, max_turns: usize) -> DchConfig {
        wire_config_for(dch_config::ApiType::OpenAi, port, max_turns)
    }

    /// The Gemini twin of [`wire_config`], for goldens over the Gemini wire.
    fn gemini_wire_config(port: u16, max_turns: usize) -> DchConfig {
        wire_config_for(dch_config::ApiType::Gemini, port, max_turns)
    }

    fn wire_config_for(api_type: dch_config::ApiType, port: u16, max_turns: usize) -> DchConfig {
        use dch_config::ApiConfig;
        let mut config = DchConfig::default();
        config.api = ApiConfig {
            api_type,
            base_url: format!("http://127.0.0.1:{port}"),
            api_key: Some("dummy".to_string()),
            model: "test-model".to_string(),
            request_timeout_secs: 10,
            ..ApiConfig::default()
        };
        config.api.context_window = Some(dch_config::FALLBACK_CONTEXT_WINDOW);
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
    async fn plan_mode_blocks_a_write_end_to_end() {
        let server = SseServer::start(vec![
            sse_tool_call_turn(
                "Write",
                &serde_json::json!({"file_path": "note.txt", "content": "x"}),
            ),
            sse_text_turn("stopped"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 10);
        config.runner.permission_mode = dch_config::PermissionMode::Plan;
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("write it"))
            .await
            .expect("run completes within timeout")
            .expect("a denied tool is a soft error, not a run failure");
        assert_eq!(run.output.as_deref(), Some("stopped"));
        assert!(
            !dir.path().join("note.txt").exists(),
            "the blocked write must not touch the filesystem"
        );
        let requests = recorded_requests(&server);
        assert_eq!(requests.len(), 2, "denied call, then the closing turn");
        assert!(
            requests[1].contains("denied by Plan mode") && requests[1].contains("Write"),
            "the denial riding the second request must name the mode and the tool: {}",
            requests[1]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accept_edits_ask_resolves_through_the_builder_resolver_end_to_end() {
        let server = SseServer::start(vec![
            sse_tool_call_turn("Bash", &serde_json::json!({"command": "echo bridge-probe"})),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 10);
        config.runner.permission_mode = dch_config::PermissionMode::AcceptEdits;
        let consultations = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let resolver: loopctl::middleware::AskResolverFn = {
            let consultations = Arc::clone(&consultations);
            Arc::new(move |prompt: &str, tool: &str| {
                consultations
                    .lock()
                    .expect("consultations lock")
                    .push((prompt.to_string(), tool.to_string()));
                Box::pin(async { true })
            })
        };
        let mut runner = Runner::builder(&config, dir.path())
            .with_permission_resolver(resolver)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("run it"))
            .await
            .expect("run completes within timeout")
            .expect("the approved dispatch succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let recorded = consultations.lock().expect("consultations lock").clone();
        assert_eq!(recorded.len(), 1, "exactly one Ask consults the resolver");
        let (prompt, tool) = recorded.into_iter().next().expect("one consultation");
        assert_eq!(tool, "Bash");
        assert!(
            prompt.contains("Bash") && prompt.contains("AcceptEdits"),
            "the prompt must label the tool and the mode: {prompt}"
        );
        let requests = recorded_requests(&server);
        assert_eq!(requests.len(), 2, "approved call, then the closing turn");
        assert!(
            requests[1].contains("bridge-probe") && !requests[1].contains("Permission denied"),
            "the approved Bash call must have executed: {}",
            requests[1]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_set_reasoning_effort_rides_the_request_wire() {
        let server = SseServer::start(vec![sse_text_turn("done")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 5);
        config.api.reasoning_effort = Some("low".to_string());
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run settles within timeout")
            .expect("the effort-level request succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        let body = requests.first().expect("the request was recorded");
        assert!(
            body.contains("\"reasoning_effort\":\"low\""),
            "the effort level must ride the wire: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unset_reasoning_effort_leaves_the_wire_untouched() {
        let server = SseServer::start(vec![sse_text_turn("done")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 5), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run settles within timeout")
            .expect("the plain request succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        let body = requests.first().expect("the request was recorded");
        assert!(
            !body.contains("reasoning_effort"),
            "the inert default must not add a field: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_gemini_effort_rides_the_wire_as_a_thinking_budget() {
        // The Gemini family has no reasoning_effort field: its effort
        // seam is the thinkingBudget the client must inject under
        // generationConfig.thinkingConfig — low maps to 1024 on the
        // provider's fixed ladder.
        let server = SseServer::start(vec![gemini_sse_text_turn("done")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = gemini_wire_config(server.port, 5);
        config.api.reasoning_effort = Some("low".to_string());
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run settles within timeout")
            .expect("the gemini effort-level request succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        let body = requests.first().expect("the request was recorded");
        assert!(
            body.contains("\"generationConfig\":{\"thinkingConfig\":{\"thinkingBudget\":1024}}"),
            "the effort level must ride the wire as a thinking budget: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retryable_stream_failure_is_retried_and_the_turn_completes() {
        // The runner installs loopctl's resilient stream handler, so a
        // 500 on the first stream attempt is one retryable failure,
        // not the end of the run: the ladder re-requests and the
        // canned second attempt serves the turn.
        let server = SseServer::start_canned(vec![
            CannedResponse {
                status: 500,
                body: "{\"error\": \"transient\"}".to_string(),
            },
            CannedResponse {
                status: 200,
                body: sse_text_turn("done"),
            },
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 5), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run settles within timeout")
            .expect("the retried stream succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            2,
            "exactly two requests: the failed attempt and its retry"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prior_turn_reasoning_rides_the_next_request() {
        // The amnesia fix, end to end: turn one streams reasoning and
        // text; turn two's request must carry that reasoning back on
        // the wire (the OpenAI family's native `reasoning_content`
        // lane), so the model's working memory survives the replay
        // boundary.
        let reasoning_then_call = || {
            [
                serde_json::json!({
                    "id": "c1", "model": "test-model",
                    "choices": [{"delta": {"reasoning_content": "the candidate list is settled: "}, "finish_reason": null}]
                }),
                serde_json::json!({
                    "id": "c1", "model": "test-model",
                    "choices": [{"delta": {"reasoning_content": "findings one and two verified"}, "finish_reason": null}]
                }),
                serde_json::json!({
                    "id": "c1", "model": "test-model",
                    "choices": [{"delta": {"tool_calls": [{
                        "index": 0, "id": "call_1",
                        "function": {"name": "Bash", "arguments": "{\"command\":\"echo checkpoint\"}"}
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
        };
        let server = SseServer::start(vec![reasoning_then_call(), sse_text_turn("done")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 5), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("go"))
            .await
            .expect("run settles within timeout")
            .expect("both turns succeed");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert_eq!(requests.len(), 2, "reasoning turn, then the closing turn");
        let second = requests.last().expect("closing request recorded");
        assert!(
            second.contains("findings one and two verified"),
            "the prior turn's reasoning must ride the next request: {second}"
        );
        assert!(
            second.contains("reasoning_content"),
            "the replay rides the native lane: {second}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_two_call_batch_dispatches_both_in_one_round_trip() {
        // Both calls are read-only-safe echoes, so parallel dispatch runs
        // them as one wave; the pin is the round-trip economics either
        // way — one provider request carries both calls, and both results
        // ride the next request in input order.
        let server = SseServer::start(vec![
            sse_batched_tool_calls_turn(&[
                ("Bash", &serde_json::json!({"command": "echo first-probe"})),
                ("Bash", &serde_json::json!({"command": "echo second-probe"})),
            ]),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("go"))
            .await
            .expect("run settles within timeout")
            .expect("both dispatched calls succeed");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            2,
            "the whole batch costs one round-trip, then the closing turn"
        );
        let follow_up = requests.last().expect("closing request recorded");
        let first = follow_up.find("first-probe").expect("first result rides");
        let second = follow_up.find("second-probe").expect("second result rides");
        assert!(
            first < second,
            "results must ride in input order: {follow_up}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resumed_read_rearm_guards_the_dispatching_write_tools() {
        // The re-arm and the executing Write must share one session: the
        // re-armed baseline is exactly what the dispatched Write consults,
        // so a file changed after the re-arm refuses the clobber.
        let server = SseServer::start(vec![
            sse_tool_call_turn(
                "Write",
                &serde_json::json!({"file_path": "note.txt", "content": "clobber"}),
            ),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "original").expect("seed target");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        assert!(
            runner.record_resumed_read("note.txt").await,
            "the readable in-reach file re-arms"
        );
        std::fs::write(dir.path().join("note.txt"), "externally changed")
            .expect("mutate after the re-arm");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("go"))
            .await
            .expect("run settles within timeout")
            .expect("a refused tool is a soft error, not a run failure");
        assert_eq!(run.output.as_deref(), Some("done"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap_or_default(),
            "externally changed",
            "the re-armed guard held the write"
        );
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && requests[1].contains("last read in a previous session"),
            "the re-arm's resumed-baseline refusal must ride the follow-up request: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_approved_write_still_refuses_a_target_changed_during_the_ask() {
        // The approval window must not widen the conflict window: the
        // gate parks the dispatch until the resolver answers, and the
        // staleness check runs only after the release, so a change
        // landing while the ask is pending still refuses the write.
        let server = SseServer::start(vec![
            sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"})),
            sse_tool_call_turn(
                "Write",
                &serde_json::json!({"file_path": "note.txt", "content": "clobber"}),
            ),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "original").expect("seed target");
        let mut config = wire_config(server.port, 10);
        config.runner.permission_mode = dch_config::PermissionMode::Interactive;
        let consultations: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let parked: Arc<Mutex<Vec<tokio::sync::oneshot::Sender<bool>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let resolver: loopctl::middleware::AskResolverFn = {
            let consultations = Arc::clone(&consultations);
            let parked = Arc::clone(&parked);
            Arc::new(move |_prompt: &str, tool: &str| {
                consultations
                    .lock()
                    .expect("consultations lock")
                    .push(tool.to_string());
                if tool == "Read" {
                    Box::pin(async { true })
                } else {
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    parked.lock().expect("parked lock").push(reply_tx);
                    Box::pin(async move { reply_rx.await.unwrap_or(false) })
                }
            })
        };
        let mut runner = Runner::builder(&config, dir.path())
            .with_permission_resolver(resolver)
            .build()
            .await
            .expect("constructs");
        let driving = tokio::spawn(async move { runner.run("go").await });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while parked.lock().expect("parked lock").is_empty() && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let release = parked
            .lock()
            .expect("parked lock")
            .pop()
            .expect("the write ask parked a reply");
        assert_eq!(
            consultations.lock().expect("consultations lock").as_slice(),
            ["Read", "Write"],
            "the read ask resolves immediately; only the write parks"
        );
        std::fs::write(dir.path().join("note.txt"), "externally changed")
            .expect("mutate during the ask");
        release.send(true).expect("the resolver still awaits");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), driving)
            .await
            .expect("run settles within timeout")
            .expect("the driving task joins")
            .expect("a refused tool is a soft error, not a run failure");
        assert_eq!(run.output.as_deref(), Some("done"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap_or_default(),
            "externally changed",
            "the approved write must not clobber the change that landed during the ask"
        );
        let requests = recorded_requests(&server);
        assert_eq!(requests.len(), 3, "read turn, refused write turn, close");
        assert!(
            requests[2].contains("changed on disk"),
            "the conflict refusal must ride the closing request: {}",
            requests[2]
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
            permission_layer(dch_tools::permission::PermissionMode::Auto, None),
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
    async fn the_engine_and_core_registries_agree_on_every_concurrency_verdict() {
        // The engine plans parallel waves from its own registry while the
        // pipeline executes through the core's. The two constructions must
        // agree on every tool's identity and concurrency verdict — a drift
        // would let a plan serialize or parallelize calls the executor
        // disagrees with, while the schemas alone still matched. Both
        // compositions carry an in-process MCP server's tool beside the
        // builtin set, so the walk covers the external registration path
        // too: a provider that joined only one side fails the twin lookup
        // outright.
        let client = loopctl::mcp::McpClient::in_process(GreetServer::new())
            .await
            .expect("mcp handshake");
        let connection = adopt_mcp_server(&demo_server_config(None), client)
            .await
            .expect("adopt the demo server");
        let session = FileSession::new(PathBuf::from("."));
        let engine = compose_registry(
            std::slice::from_ref(&connection),
            &session,
            dch_tools::ReadWindows::default(),
        );
        let core = compose_registry(
            std::slice::from_ref(&connection),
            &session,
            dch_tools::ReadWindows::default(),
        );
        for registry in [&engine, &core] {
            assert!(
                registry.contains("demo__greet"),
                "both composed registries carry the external tool"
            );
        }

        let inputs = [
            Value::Null,
            serde_json::json!({"command": "echo hi"}),
            serde_json::json!({"files": ["a.rs"]}),
            serde_json::json!({"todos": []}),
            serde_json::json!({"file_path": "a.rs", "content": "x"}),
        ];
        for tool in engine.all_tools() {
            let name = tool.name();
            let twin = core
                .get(name)
                .unwrap_or_else(|| panic!("the core registry must carry {name}"));
            assert_eq!(
                tool.is_concurrency_safe(),
                twin.is_concurrency_safe(),
                "{name} must share its static concurrency verdict"
            );
            for input in &inputs {
                assert_eq!(
                    tool.is_safe_for_concurrent_execution(input),
                    twin.is_safe_for_concurrent_execution(input),
                    "{name} must share its verdict for {input}"
                );
            }
        }
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
    async fn run_completes_with_the_session_default_policy() {
        let server = SseServer::start(vec![sse_text_turn("probe says hello")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes within timeout")
            .expect("run succeeds against the canned server");
        assert_eq!(run.output.as_deref(), Some("probe says hello"));
        assert_eq!(run.turn_count(), 1);
        assert!(run.stop_reason.is_none(), "clean stop, no error reason");
        assert_eq!(run.config.max_turns, 7, "session default policy is used");
    }

    /// One canned non-streaming completion body.
    ///
    /// Any assistant text serves the summarizers' calls in tests — the
    /// passes are pinned by their requests arriving and the run continuing,
    /// not by the summary's wording.
    fn canned_completion_body() -> String {
        "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\
         \"message\":{\"role\":\"assistant\",\"content\":\"compact summary\"},\
         \"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\
         \"completion_tokens\":1,\"total_tokens\":2}}"
            .to_string()
    }

    /// Records every turn-end snapshot fact the publishing rides on.
    struct TurnEndProbe {
        /// One (turn, success, history length) per turn end, in order.
        facts: Mutex<Vec<(usize, bool, usize)>>,
    }

    impl TurnEndProbe {
        /// An observer with an empty fact log.
        fn new() -> Self {
            Self {
                facts: Mutex::new(Vec::new()),
            }
        }
    }

    impl LoopObserver for TurnEndProbe {
        fn name(&self) -> &'static str {
            "turn-end-probe"
        }

        fn on_turn_end(&self, ctx: &loopctl::observer::TurnEndContext) {
            if let Ok(mut facts) = self.facts.lock() {
                facts.push((ctx.turn, ctx.success, ctx.history.len()));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_successful_turn_end_carries_a_growing_history() {
        // The machine-half publishing rides turn-end snapshots: each
        // successful turn must carry a history at least as long as the
        // last, or the snapshot freezes and a resume silently amneses
        // everything after the freeze point.
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(20);
        std::fs::write(dir.path().join("chunk-a.txt"), &chunk).expect("write fixture");
        std::fs::write(dir.path().join("chunk-b.txt"), &chunk).expect("write fixture");
        let mut script = Vec::new();
        for turn in 0..130u32 {
            let offset = turn;
            if turn % 2 == 0 {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": "chunk-a.txt", "offset": offset}),
                ));
            } else {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": "chunk-b.txt", "offset": offset}),
                ));
            }
        }
        script.push(sse_text_turn("all done"));
        let server = SseServer::start(script).await;
        let config = wire_config(server.port, 200);
        let probe = Arc::new(TurnEndProbe::new());
        // A seeded history, the resume path's shape: the engine starts
        // from a restored set and must snapshot seed-plus-growth, never
        // the seed alone.
        let mut seed = vec![Message::user("restored context")];
        for index in 0..9u32 {
            let call_id = format!("seed-{index}");
            seed.push(Message::new(
                loopctl::message::Role::Assistant,
                vec![loopctl::message::MessagePart::tool_call(
                    call_id.clone(),
                    "Read",
                    serde_json::json!({ "path": "chunk-a.txt" }),
                )],
            ));
            seed.push(Message::new(
                loopctl::message::Role::User,
                vec![loopctl::message::MessagePart::tool_result(
                    call_id,
                    "Read",
                    chunk.clone(),
                    false,
                )],
            ));
        }
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&probe) as Arc<dyn LoopObserver>)
            .with_history(seed)
            .build()
            .await
            .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(30), runner.run("grow"))
            .await
            .expect("run completes")
            .expect("run succeeds");

        let facts = probe.facts.lock().expect("the probe lock").clone();
        assert!(
            facts.len() >= 130,
            "the fixture drove every turn end: {} facts",
            facts.len()
        );
        let mut longest = 0usize;
        for (turn, success, len) in facts {
            if success {
                assert!(
                    len >= longest,
                    "turn {turn} carried a shrunken history ({len} after {longest})"
                );
                longest = longest.max(len);
            }
        }
        assert!(
            longest >= 260,
            "the final snapshot holds the whole exchange set ({longest})"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_settled_compaction_counts_for_the_headless_save_path() {
        // The headless machine snapshot persists its compaction count
        // from the console observer's counter; a real settled pass —
        // the truncating selection spends no model call — must land
        // in both the observer and the handle construction hands the
        // save path.
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        std::fs::write(dir.path().join("chunk-a.txt"), &chunk).expect("write fixture");
        std::fs::write(dir.path().join("chunk-b.txt"), &chunk).expect("write fixture");
        let mut script = Vec::new();
        for index in 0..18 {
            let name = if index % 2 == 0 {
                "chunk-a.txt"
            } else {
                "chunk-b.txt"
            };
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start(script).await;
        let mut config = wire_config(server.port, 22);
        config.api.context_window = Some(90_000);
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let observer = Arc::new(crate::console_observer::ConsoleObserver::with_sinks(
            dch_config::Verbosity::Quiet,
            false,
            Box::new(std::io::sink()),
            Box::new(std::io::sink()),
        ));
        let counter = observer.compaction_counter();
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the run completes")
            .expect("the run compacts and finishes");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert_eq!(
            observer.compaction_count(),
            1,
            "the settled pass lands in the count the snapshot saves"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the save path's handle reads the same total"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_truncating_selection_compacts_by_dropping_messages() {
        // Several tool turns of medium results accumulate past the compact
        // threshold; the next request must be preceded by a compaction pass
        // that drops the older exchanges instead of serving over-window.
        // The growth rides on the read tool — Bash's own viewing path
        // answers with a refusal, not file contents. The window leaves the
        // deep default landing (30% of the threshold) enough room for the
        // terminal's preserved-tail floor — a window whose floor cannot
        // fit the landing is the truncator's documented refusal shape,
        // not this pin's claim.
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        std::fs::write(dir.path().join("chunk-a.txt"), &chunk).expect("write fixture");
        std::fs::write(dir.path().join("chunk-b.txt"), &chunk).expect("write fixture");
        let read_a = || sse_tool_call_turn("Read", &serde_json::json!({"path": "chunk-a.txt"}));
        let read_b = || sse_tool_call_turn("Read", &serde_json::json!({"path": "chunk-b.txt"}));
        let mut script = Vec::new();
        for index in 0..18 {
            if index % 2 == 0 {
                script.push(read_a());
            } else {
                script.push(read_b());
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start(script).await;
        let mut config = wire_config(server.port, 22);
        config.api.context_window = Some(90_000);
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            19,
            "eighteen tool turns plus the final text turn, no extras"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("\"stream\":false")),
            "truncation spends no model calls"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compactor_calls_leave_the_session_reasoning_effort_behind() {
        // The compaction chain speaks to the same endpoint through its
        // own client: a summarizer reasoning at the session's `high`
        // effort burns minutes and its output budget on thinking, so
        // the chain's requests carry no effort while the conversation
        // turns keep it.
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        let names = [
            "chunk-a.txt",
            "chunk-b.txt",
            "chunk-c.txt",
            "chunk-d.txt",
            "chunk-e.txt",
            "chunk-f.txt",
            "chunk-g.txt",
            "chunk-h.txt",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
        }
        let mut script = Vec::new();
        for name in names {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for round in 0..7u32 {
            for name in names {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name, "offset": round}),
                ));
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.api.reasoning_effort = Some("high".to_string());
        config.runner.compactor = dch_config::CompactorSelection::Qa;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the pass ran: {:?}",
            observer.snapshot()
        );
        let requests = recorded_requests(&server);
        let summarizer_requests: Vec<&String> = requests
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .collect();
        assert!(
            !summarizer_requests.is_empty(),
            "the summarizer called through the chain's own client"
        );
        assert!(
            summarizer_requests
                .iter()
                .all(|request| !request.contains("reasoning_effort")),
            "no compactor request carries the session's effort"
        );
        assert!(
            requests.iter().any(|request| {
                request.contains("\"stream\":true") && request.contains("reasoning_effort")
            }),
            "the conversation turns keep the configured effort"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_qa_selection_compacts_through_the_summarizer_and_the_run_continues() {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        let names = [
            "chunk-a.txt",
            "chunk-b.txt",
            "chunk-c.txt",
            "chunk-d.txt",
            "chunk-e.txt",
            "chunk-f.txt",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
        }
        let mut script = Vec::new();
        for name in names {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for _ in 0..8 {
            for name in names {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.runner.compactor = dch_config::CompactorSelection::Qa;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let summarizer_calls = recorded_requests(&server)
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            summarizer_calls >= 1,
            "the qa pass runs through the session's own client"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_structured_selection_compacts_through_the_structured_summarizer() {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        let names = [
            "chunk-a.txt",
            "chunk-b.txt",
            "chunk-c.txt",
            "chunk-d.txt",
            "chunk-e.txt",
            "chunk-f.txt",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
        }
        let mut script = Vec::new();
        for name in names {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for _ in 0..8 {
            for name in names {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.runner.compactor = dch_config::CompactorSelection::Structured;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let summarizer_calls = recorded_requests(&server)
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            summarizer_calls >= 1,
            "the structured pass runs through the session's own client"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rolling_selection_compacts_through_the_aging_pass() {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(120);
        // Ten grow runs, each closing in its own text seam: the rolling
        // pass ages only past its tenth boundary, and mid-run text
        // cannot serve (a text-only turn completes the run), so the
        // boundaries come from run seams. Unique paths per run keep the
        // repeated-call loop guard out of the picture.
        let names_for = |round: usize| {
            (0..6)
                .map(|file| format!("chunk-r{round}-{file}.txt"))
                .collect::<Vec<String>>()
        };
        for round in 0..10 {
            for name in names_for(round) {
                std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
            }
        }
        let mut script = Vec::new();
        for name in names_for(0) {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for round in 1..10 {
            for name in names_for(round) {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
            script.push(sse_text_turn(&format!("round {round} done")));
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.runner.compact_threshold = 40;
        config.runner.compactor = dch_config::CompactorSelection::Rolling;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        for round in 1..10 {
            let grown = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                runner.run(&format!("grow {round}")),
            )
            .await
            .expect("the grow run completes")
            .expect("the grow run succeeds");
            assert_eq!(
                grown.output.as_deref(),
                Some(format!("round {round} done").as_str())
            );
        }
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let rolling_calls = recorded_requests(&server)
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            rolling_calls >= 1,
            "the aging pass summarizes through the session's own client"
        );
    }

    /// The strict-selection pin body, shared by the qa, structured, and
    /// rolling variants: the configured compactor is the only one that
    /// ever runs, so when its model call fails the pass fails — nothing
    /// truncates in its place, the history rides the deferral intact,
    /// and the run proceeds while the payload fits the window.
    async fn a_failing_stage_never_substitutes(selection: dch_config::CompactorSelection) {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(120);
        // Each run reads its own six files — the same path eleven times
        // would trip the engine's repeated-call loop guard — and every
        // completed run leaves the assistant-text seam the rolling pass
        // counts as an aging boundary; mid-run text cannot serve (a
        // text-only turn completes the run), so the boundaries come
        // from run seams and rolling's ten-boundary gate opens only
        // once the fixture genuinely has ten of them.
        let names_for = |round: usize| {
            (0..6)
                .map(|file| format!("chunk-r{round}-{file}.txt"))
                .collect::<Vec<String>>()
        };
        for round in 0..10 {
            for name in names_for(round) {
                std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
            }
        }
        let mut script = Vec::new();
        for name in names_for(0) {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for round in 1..10 {
            for name in names_for(round) {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
            script.push(sse_text_turn(&format!("round {round} done")));
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_failing_completion(script).await;
        let mut config = wire_config(server.port, 60);
        config.api.context_window = Some(130_000);
        config.runner.compact_threshold = 40;
        config.runner.compactor = selection;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        for round in 1..10 {
            let grown = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                runner.run(&format!("grow {round}")),
            )
            .await
            .expect("the grow run completes")
            .expect("the failed passes defer and the run proceeds");
            assert_eq!(
                grown.output.as_deref(),
                Some(format!("round {round} done").as_str())
            );
        }
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("the failed passes defer and the run proceeds");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        let requests = recorded_requests(&server);
        let summarizer_attempts = requests
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            summarizer_attempts >= 1,
            "the configured stage genuinely tries its model call"
        );
        let snapshot = observer.snapshot();
        assert!(
            snapshot.contains(&"compaction_failed"),
            "the failed pass surfaces as a compaction failure: {snapshot:?}"
        );
        assert!(
            !snapshot.contains(&"compaction"),
            "no other compactor landed in the configured stage's place: {snapshot:?}"
        );
        let streaming: Vec<&String> = requests
            .iter()
            .filter(|request| !request.contains("\"stream\":false"))
            .collect();
        let occurrences = |request: &str| {
            request
                .matches("payload line for the context budget")
                .count()
        };
        let peak = streaming.iter().map(|request| occurrences(request)).max();
        let final_count = streaming.last().map(|request| occurrences(request));
        assert!(
            matches!((peak, final_count), (Some(peak), Some(end)) if end == peak),
            "the history rides the deferral intact — no silent truncation: \
             peak {peak:?} vs final {final_count:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_qa_stage_never_substitutes_another_compactor() {
        a_failing_stage_never_substitutes(dch_config::CompactorSelection::Qa).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_structured_stage_never_substitutes_another_compactor() {
        a_failing_stage_never_substitutes(dch_config::CompactorSelection::Structured).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_rolling_stage_never_substitutes_another_compactor() {
        a_failing_stage_never_substitutes(dch_config::CompactorSelection::Rolling).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_loop_history_compacts_through_the_qa_chain() {
        // The shape that wedged before the engine's compaction fixes:
        // every assistant message is a tool call and every result opens
        // the next user message, so no pair-safe split boundary exists
        // anywhere in the restored history. The qa selection must still
        // compact — through its own slid split — and the resumed run
        // must proceed.
        let server = SseServer::start_with_completion(
            vec![sse_text_turn("recovered")],
            canned_completion_body(),
        )
        .await;
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(170);
        let mut history = vec![Message::new(
            loopctl::message::Role::User,
            vec![loopctl::message::MessagePart::text("grow the context")],
        )];
        for index in 0..150u32 {
            let call_id = format!("seed-{index}");
            history.push(Message::new(
                loopctl::message::Role::Assistant,
                vec![loopctl::message::MessagePart::tool_call(
                    call_id.clone(),
                    "Read",
                    serde_json::json!({ "path": "chunk.txt" }),
                )],
            ));
            history.push(Message::new(
                loopctl::message::Role::User,
                vec![loopctl::message::MessagePart::tool_result(
                    call_id,
                    "Read",
                    chunk.clone(),
                    false,
                )],
            ));
        }
        let mut config = wire_config(server.port, 60);
        config.api.context_window = Some(350_000);
        config.runner.compact_threshold = 60;
        config.runner.compactor = dch_config::CompactorSelection::Qa;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .with_history(history)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("the resumed run completes")
            .expect("the run succeeds through the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold tool-loop history must compact: {:?}",
            observer.snapshot()
        );
        assert!(
            observer.snapshot().contains(&"pre_compaction"),
            "the pass announces its start before it runs: {:?}",
            observer.snapshot()
        );
        let seeded_lines = 12_usize.saturating_mul(170);
        let carried = recorded_requests(&server).first().map_or(0, |request| {
            request
                .matches("payload line for the context budget")
                .count()
        });
        assert!(
            carried < seeded_lines,
            "the follow-up request carries the compacted history: \
             {carried} of {seeded_lines} payload lines survived"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_seeded_history_resumes_without_recompaction() {
        let server = SseServer::start(vec![sse_text_turn("continued")]).await;
        let dir = TempDir::new().expect("tempdir");
        let config = wire_config(server.port, 7);
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .with_history(vec![
                Message::new(
                    loopctl::message::Role::User,
                    vec![loopctl::message::MessagePart::text("seed marker fa1")],
                ),
                Message::new(
                    loopctl::message::Role::Assistant,
                    vec![loopctl::message::MessagePart::text("prior answer")],
                ),
            ])
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("next"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.output.as_deref(), Some("continued"));
        assert!(
            !observer.snapshot().contains(&"compaction"),
            "a resumed working set under the threshold must not recompact: {:?}",
            observer.snapshot()
        );
        assert!(
            recorded_requests(&server)
                .first()
                .is_some_and(|request| request.contains("seed marker fa1")),
            "the seeded working set reaches the wire verbatim"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn committed_history_pairs_every_tool_call_with_its_result() {
        let server = SseServer::start(vec![sse_read_tool_call_turn(), sse_text_turn("done")]).await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("note.txt"),
            "probe payload
",
        )
        .expect("write fixture");
        let config = wire_config(server.port, 7);
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("read"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        let history = runner.committed_history();
        let calls: std::collections::HashSet<String> = history
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                loopctl::message::MessagePart::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let results: std::collections::HashSet<String> = history
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                loopctl::message::MessagePart::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        assert!(!calls.is_empty(), "the tool turn committed its call");
        assert_eq!(
            calls, results,
            "a completed run leaves every call paired with its result"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_explicit_window_beats_the_probe() {
        let server = SseServer::start(vec![
            r#"{"n_ctx": 262144, "total_slots": 4}"#.to_string(),
            sse_text_turn("answer"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 7);
        config.api.context_window = Some(90_000);
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        assert_eq!(
            runner.context_window(),
            90_000,
            "a deliberate config override wins over discovery"
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes")
            .expect("run succeeds");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unset_window_adopts_the_model_listing_answer() {
        // The servers without a /props document — vLLM and its kin —
        // publish each model's window on the listing route instead;
        // discovery reads it there when the capability document has
        // no answer.
        let server = SseServer::start(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"test-model","max_model_len":262144,"meta":{"n_ctx":262144}}]}"#
                .to_string(),
            sse_text_turn("answer"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 7);
        config.api.context_window = None;
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        assert_eq!(
            runner.context_window(),
            262_144,
            "the listing's answer sizes the session when /props has none - unscaled, \
             this family reports usage and calibrates"
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes")
            .expect("run succeeds");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unset_window_adopts_the_known_model_table() {
        // A cloud provider publishes no window anywhere its client can
        // ask; after both live questions go unanswered, the
        // documented-model table sizes the session.
        let server = SseServer::start(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"other-model"}]}"#.to_string(),
            sse_text_turn("answer"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 7);
        config.api.model = "glm-5.3".to_string();
        config.api.context_window = None;
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        assert_eq!(
            runner.context_window(),
            1_000_000,
            "the documented table sizes a model the server never names - unscaled, \
             this family reports usage and calibrates"
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes")
            .expect("run succeeds");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unset_window_adopts_the_discovered_one() {
        let props = r#"{"n_ctx": 520000, "total_slots": 4}"#.to_string();
        let chunk = "payload line for the context budget\n".repeat(400);
        let dir = TempDir::new().expect("tempdir");
        let names = [
            "chunk-a.txt",
            "chunk-b.txt",
            "chunk-c.txt",
            "chunk-d.txt",
            "chunk-e.txt",
            "chunk-f.txt",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
        }
        let mut script = vec![props];
        for _ in 0..8 {
            for name in names {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start(script).await;
        let mut config = wire_config(server.port, 60);
        config.api.context_window = None;
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        assert_eq!(
            runner.context_window(),
            130_000,
            "the provider's per-request share becomes the window - unscaled, \
             this family reports usage and calibrates"
        );
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("run completes")
            .expect("run succeeds across the discovered boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the discovered window's threshold is enforced — the fallback default would never cross: {:?}",
            observer.snapshot()
        );
    }

    /// An rmcp server exposing one `greet` tool, for the MCP registration test.
    ///
    /// Serves over an in-memory transport, so the registration pin
    /// exercises the real MCP handshake and tool discovery without
    /// spawning a process or opening a socket.
    #[derive(Clone)]
    struct GreetServer {
        // rmcp's tool_handler macro reaches the router through its generated
        // trait impl, so rustc cannot see the read.
        #[allow(dead_code)]
        router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    }

    #[rmcp::tool_router]
    impl GreetServer {
        fn new() -> Self {
            Self {
                router: Self::tool_router(),
            }
        }

        #[rmcp::tool(description = "Return a friendly greeting")]
        async fn greet(&self) -> String {
            "hello, world!".to_string()
        }

        #[rmcp::tool(description = "Say goodbye")]
        async fn farewell(&self) -> String {
            "goodbye!".to_string()
        }
    }

    #[allow(clippy::unused_async_trait_impl)] // rmcp's tool_handler emits un-awaited trait impls
    #[rmcp::tool_handler]
    impl rmcp::handler::server::ServerHandler for GreetServer {}

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_provider_tools_join_the_registry_and_dispatch() {
        let client = loopctl::mcp::McpClient::in_process(GreetServer::new())
            .await
            .expect("mcp handshake");
        let provider = loopctl::mcp::McpToolProvider::connect(client, Some("demo".to_string()))
            .await
            .expect("tool discovery");
        let server = SseServer::start(vec![
            sse_tool_call_turn("demo__greet", &serde_json::json!({})),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let config = wire_config(server.port, 7);
        let mut runner = Runner::builder(&config, dir.path())
            .with_mcp_provider(provider)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("greet"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        assert_eq!(run.tool_call_count(), 1, "the MCP tool was dispatched");
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && requests[1].contains("hello, world!"),
            "the MCP tool result must flow back to the model: {requests:?}"
        );
    }

    fn demo_server_config(tools: Option<Vec<String>>) -> dch_config::McpServerConfig {
        dch_config::McpServerConfig {
            name: "demo".to_string(),
            command: "unused".to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            tools,
        }
    }

    #[tokio::test]
    async fn an_mcp_allowlist_registers_only_listed_tools() {
        let connection = adopt_mcp_server(
            &demo_server_config(Some(vec!["greet".to_string()])),
            loopctl::mcp::McpClient::in_process(GreetServer::new())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let registry = compose_registry(
            &[connection],
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );

        assert!(
            registry.contains("demo__greet"),
            "the allowlisted tool must be registered"
        );
        assert!(
            !registry.contains("demo__farewell"),
            "an unlisted tool must stay unregistered"
        );
        let expected = builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        )
        .len()
            + 1;
        assert_eq!(registry.len(), expected, "exactly one external tool joins");
    }

    #[tokio::test]
    async fn an_unknown_allowlisted_tool_fails_startup_naming_both() {
        let err = adopt_mcp_server(
            &demo_server_config(Some(vec!["nope".to_string()])),
            loopctl::mcp::McpClient::in_process(GreetServer::new())
                .await
                .unwrap(),
        )
        .await
        .map(|_| ())
        .unwrap_err();
        let RunnerError::Config(msg) = &err else {
            panic!("expected Config error, got {err:?}");
        };
        assert!(
            msg.contains("demo") && msg.contains("nope"),
            "the error must name the server and the missing tool: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_configured_fallback_model_serves_once_the_primary_trips() {
        // A permanent auth failure costs exactly one attempt, so three
        // failing runs trip the breaker deterministically; the next run's
        // request must name the fallback model and succeed.
        let denied = r#"{"error":{"message":"invalid api key"}}"#.to_string();
        let server = SseServer::start_canned(vec![
            CannedResponse {
                status: 401,
                body: denied.clone(),
            },
            CannedResponse {
                status: 401,
                body: denied.clone(),
            },
            CannedResponse {
                status: 401,
                body: denied,
            },
            CannedResponse {
                status: 200,
                body: sse_text_turn("saved by the fallback"),
            },
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 10);
        config.api.fallback_model = Some("dch-fallback-model".to_string());
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        for attempt in 1..=3 {
            let result = runner.run("fail").await;
            assert!(result.is_err(), "denial {attempt} must fail the run");
        }
        let recovered = runner.run("recover").await.expect("fallback serves");
        assert_eq!(recovered.output.as_deref(), Some("saved by the fallback"));
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            4,
            "one request per run, no retry ladder on 401"
        );
        for (index, request) in requests.iter().enumerate().take(3) {
            assert!(
                request.contains("\"model\":\"test-model\""),
                "request {index} must name the primary model: {request}"
            );
        }
        assert!(
            requests[3].contains("\"model\":\"dch-fallback-model\""),
            "the post-trip request must carry the fallback model: {}",
            requests[3]
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
    async fn run_with_applies_the_override_for_that_call_only() {
        let server = SseServer::start(vec![sse_text_turn("first"), sse_text_turn("second")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
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
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
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
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conversation_persists_across_runs() {
        let server = SseServer::start(vec![
            sse_text_turn("first answer"),
            sse_text_turn("second answer"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
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
        let mut runner = Runner::builder(&wire_config(server.port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
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
        let mut runner = Runner::builder(&wire_config(port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi")).await;
        assert!(
            matches!(result, Ok(Err(_))),
            "expected an Err from run against a refused endpoint, got {result:?}"
        );
    }

    #[tokio::test]
    async fn the_steering_handle_is_the_loops_own_shared_slot() {
        // The handle is the whole seam, so it has to be one slot: a fresh
        // slot per call would let a host steer a loop that never reads it.
        let dir = TempDir::new().expect("tempdir");
        let runner = Runner::builder(&offline_config(), dir.path())
            .build()
            .await
            .expect("Runner::new constructs");
        let handle = runner.steering();
        assert!(
            Arc::ptr_eq(&handle, &runner.steering()),
            "every call hands out the loop's own slot, not another one"
        );
        assert!(
            handle.is_empty(),
            "a session that never ran has nothing waiting"
        );
        assert!(
            !handle.push(loopctl::message::Message::user("too early")),
            "with no run in flight the loop refuses the offer rather than storing it"
        );
        assert!(
            handle.take_all().is_empty(),
            "a refused offer is not stored anywhere: the caller still holds the \
             text, which is what lets a host deliver it another way"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guidance_reaches_the_model_at_the_next_turn_boundary() {
        // The first response is held back, so the offer provably lands while
        // the run is open and before any boundary — and the law says where it
        // goes next: not into the request already in flight, but into the one
        // after the tool result.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("seed.txt"), "seeded").expect("seed the workdir");
        let server = SseServer::start_with_delay(
            vec![
                sse_tool_call_turn("Read", &serde_json::json!({"path": "seed.txt"})),
                sse_text_turn("guided"),
            ],
            std::time::Duration::from_millis(700),
        )
        .await;
        let runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        let seam = runner.steering();
        let task = tokio::spawn(async move {
            let mut runner = runner;
            tokio::time::timeout(std::time::Duration::from_secs(30), runner.run("read it"))
                .await
                .expect("the run settles within its budget")
        });

        // Wait until the first request has been sent and is sitting on the
        // held-back response: from here the guidance cannot reach that
        // request by any legitimate route, which is the half of the timing
        // law worth pinning.
        for _ in 0..400 {
            if !recorded_requests(&server).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            recorded_requests(&server).len(),
            1,
            "the first request is in flight when the guidance is offered"
        );
        // A sentinel rather than a phrase: the request body carries the
        // system prompt and the tool schemas, where ordinary words appear
        // by coincidence and a negative assertion would prove nothing.
        assert!(
            seam.push(loopctl::message::Message::user(
                "the file it reads next is generated, steer-9v4q"
            )),
            "the run in flight took the guidance"
        );

        let run = task
            .await
            .expect("the task joins")
            .expect("the run succeeds");
        assert_eq!(run.output.as_deref(), Some("guided"));
        assert!(
            seam.is_empty(),
            "the loop drained its slot at the boundary, so nothing is owed back"
        );
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2,
            "a tool turn then the answer: {requests:?}"
        );
        assert!(
            !requests[0].contains("steer-9v4q"),
            "the guidance cannot retroactively enter the request already sent"
        );
        assert!(
            requests[1].contains("steer-9v4q"),
            "the guidance rides the follow-up request, in the conversation the \
             model continues: {:?}",
            requests[1]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guidance_the_run_never_reads_is_left_for_the_host() {
        // One turn, no boundary after it: the guidance was accepted, and the
        // run ended anyway. The seam's other law — the host gets it back, so
        // nothing the user typed dies in a slot nobody will drain again.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("answered")],
            std::time::Duration::from_millis(700),
        )
        .await;
        let dir = TempDir::new().expect("tempdir");
        let runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        let seam = runner.steering();
        let task = tokio::spawn(async move {
            let mut runner = runner;
            tokio::time::timeout(std::time::Duration::from_secs(30), runner.run("answer"))
                .await
                .expect("the run settles within its budget")
        });

        let mut offered = false;
        for _ in 0..400 {
            if seam.push(loopctl::message::Message::user("advice that arrived late")) {
                offered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(offered, "the run in flight took the guidance");

        let run = task
            .await
            .expect("the task joins")
            .expect("the run succeeds");
        assert_eq!(run.output.as_deref(), Some("answered"));
        let left = seam.take_all();
        let texts: Vec<String> = left
            .iter()
            .map(loopctl::message::Message::text_content)
            .collect();
        assert_eq!(
            texts,
            ["advice that arrived late"],
            "what the run never reached comes back to the host, in the order it \
             arrived"
        );
        assert!(seam.is_empty(), "taking it clears the slot");
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
        let mut runner = Runner::builder(&wire_config(port, 7), dir.path())
            .build()
            .await
            .expect("constructs");
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
        let mut runner = Runner::builder(&wire_config(server.port, 2), dir.path())
            .build()
            .await
            .expect("constructs");
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
