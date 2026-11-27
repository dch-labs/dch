//! Building a [`Runner`]: the builder a host registers its extensions
//! on, and the `build` that assembles the registered parts into a
//! running loop.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dch_tools::RunnerContext;
use loopctl::engine::BareLoop;
use loopctl::engine::LoopMachine;
use loopctl::managers::LoopManagers;
use loopctl::mcp::McpToolProvider;
use loopctl::message::Message;
use loopctl::middleware::PermissionAskResolver;
use loopctl::middleware::ToolMiddleware;
use loopctl::observer::LoopObserver;
use loopctl::tool::builtin::fs::FileSession;

use crate::DchClient;
use crate::RunnerError;
use crate::permission::permission_layer_live;
use crate::runner::RetirementSink;
use crate::runner::Runner;
use crate::runner::managers::arm_fallback;
use crate::runner::managers::context_manager_for;
use crate::runner::managers::stream_handler;
use crate::runner::pipeline::build_pipeline;
use crate::runner::plan::seed_standing_plan;
use crate::runner::registries::compose_twin_registries;
use crate::runner::registries::connect_mcp_servers;
use crate::runner::registries::fs_resolve_policy_for;
use crate::runner::registries::read_windows_for;
use crate::runner::registries::resolve_policy_for;
use crate::runner::session::build_session;

impl Runner {
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
            permission_mode_cell: None,
            permission_config_rules: std::sync::Arc::new(Vec::new),
            permission_memory: None,
            interactive_tools: true,
            todo_nudge: None,
            retirement_sink: None,
        }
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

    /// Optional typed resolver for the permission gate's `Ask` verdicts.
    ///
    /// Interactive hosts (the TUI) supply one so `Ask` cells prompt the
    /// user; without it the gate denies asks outright, which is the
    /// headless contract. The resolver receives the whole call — the
    /// typed [`loopctl::middleware::PermissionAsk`] — and answers a
    /// [`loopctl::middleware::PermissionResolution`], whose
    /// `DenyWithFeedback` carries the user's words to the model. It runs
    /// on the dispatch task, so it must not block — and a pending
    /// resolution races the run's cancel signal, so a cancelled run
    /// denies rather than waits.
    permission_resolver: Option<std::sync::Arc<dyn PermissionAskResolver>>,

    /// The shared, live permission mode, when the host wired one.
    ///
    /// `None` builds freeze the config's mode — the standing contract
    /// for hosts that never move a mode mid-run.
    permission_mode_cell: Option<dch_config::SharedPermissionMode>,

    /// The config's `[permissions]` rules, compiled to engine rules.
    ///
    /// Set by [`with_permission_config_rules`](Self::with_permission_config_rules);
    /// the gate evaluates them ahead of the remembered layer and the
    /// matrix on every dispatch. Empty by default — a config without the
    /// section adds nothing.
    permission_config_rules: crate::permission::ConfigRulesView,

    /// The live remembered-approvals snapshot, when the host wired one.
    ///
    /// The interactive host's bridge over its "allow always" memory; the
    /// gate consults it per dispatch, so a remember made one call ago
    /// covers the next. `None` evaluates the remembered layer as always
    /// empty — the headless contract, and the pre-remember spelling.
    permission_memory: Option<crate::permission::MemorySnapshot>,

    /// Whether the interaction-only tools register.
    ///
    /// True by default — the interactive host's composition. Turned off
    /// by [`without_interactive_tools`](Self::without_interactive_tools)
    /// for hosts with no one to ask (a headless run), so the model is
    /// never offered a call that could only fail.
    interactive_tools: bool,

    /// How the host echoes an accepted todo-nudge reminder, and
    /// whether it will still take one.
    ///
    /// Set together by [`with_todo_nudge`](Self::with_todo_nudge);
    /// their presence is what arms the nudge at all — a host that
    /// passes none builds a runner whose runs end where the model
    /// leaves them. The predicate is asked at every clean run end
    /// the turn budget leaves room to offer a follow-up at, and at
    /// every turn top the mid-run reminder's contributor consults —
    /// before the plan is read in both — so a host that begins
    /// leaving mid-session declines without unwinding anything.
    todo_nudge: Option<TodoNudgeRegistration>,

    /// The host's claim-edge eye on plan retirements.
    ///
    /// Set by [`with_retirement_sink`](Self::with_retirement_sink);
    /// `None` on a host that polls
    /// [`take_claim_retirements`](Runner::take_claim_retirements)
    /// after its runs instead.
    retirement_sink: Option<RetirementSink>,
}

/// The host-supplied halves of the todo nudge.
///
/// The echo an accepted end-of-run reminder owes the transcript, and
/// the leave-to-extend asked at every clean run end the turn budget
/// leaves room to offer at and at every turn-top consult the mid-run
/// reminder's contributor makes — carried as one registration because
/// they arm together and neither makes sense without the other.
type TodoNudgeRegistration = (
    Arc<dyn Fn(String) + Send + Sync>,
    Arc<dyn Fn() -> bool + Send + Sync>,
);

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

    /// Register the typed resolver the permission gate consults on `Ask`.
    ///
    /// The gate is always installed — it enforces the configured
    /// `[runner] permission_mode` matrix on every dispatch; the
    /// resolver decides what an `Ask` cell does. Without one, `Ask`
    /// denies (headless); with one, the resolver's
    /// [`loopctl::middleware::PermissionResolution`] decides — allow,
    /// plain deny, or deny with the user's words as the reason. The
    /// same resolver serves every run of the built runner.
    #[must_use]
    pub fn with_permission_resolver(
        mut self,
        resolver: std::sync::Arc<dyn PermissionAskResolver>,
    ) -> Self {
        self.permission_resolver = Some(resolver);
        self
    }

    /// Share the permission mode as a live cell the gate re-reads per
    /// dispatch.
    ///
    /// The host's wiring for a mode that moves while a run is in
    /// flight: the cell outlives every runner (it belongs to the
    /// session's lanes), and the gate consults it on each tool call,
    /// so `/mode` mid-run lands on the very next dispatch. Without
    /// one, the build freezes the config's mode at construction, as
    /// it always did.
    #[must_use]
    pub fn with_permission_mode_cell(mut self, mode: dch_config::SharedPermissionMode) -> Self {
        self.permission_mode_cell = Some(mode);
        self
    }

    /// Install the config's `[permissions]` rules into the gate.
    ///
    /// The host's conversion of the config section to engine rules —
    /// [`config_rules`](crate::permission::config_rules) is the one
    /// translation — so a `deny` on `**/.env` or an `ask` on `WebFetch`
    /// holds in every mode the config can reach, ahead of remembered
    /// approvals and the matrix. Nothing installed is the default: the
    /// section's absence changes no verdict.
    #[must_use]
    pub fn with_permission_config_rules(
        mut self,
        rules: crate::permission::ConfigRulesView,
    ) -> Self {
        self.permission_config_rules = rules;
        self
    }

    /// Share the live remembered-approvals snapshot with the gate.
    ///
    /// The interactive host's bridge over its "allow always" memory: the
    /// snapshot is read on **every dispatch**, so a remember made one call
    /// ago covers the very next variant of the same work — an ask that is
    /// covered never reaches the resolver at all, and the verdict log
    /// names `remembered` as the rule that allowed it. Nothing installed
    /// is the default: the remembered layer evaluates as always empty.
    #[must_use]
    pub fn with_permission_memory(mut self, memory: crate::permission::MemorySnapshot) -> Self {
        self.permission_memory = Some(memory);
        self
    }

    /// Omit the interaction-only tools from the built runner's registry.
    ///
    /// The headless host's spelling of "no one to ask": `AskUserQuestion`
    /// registers in the default composition and soft-errors when no UI
    /// answers, but a composition that cannot ask at all is cleaner —
    /// the model never sees the tool, so it never spends a turn on a
    /// call that could only fail. Both registry positions (the engine's
    /// advertised set and the dispatch core) omit it together, keeping
    /// the two identical by construction. MCP tools are untouched: a
    /// server's tools are the host's own choice.
    #[must_use]
    pub fn without_interactive_tools(mut self) -> Self {
        self.interactive_tools = false;
        self
    }

    /// Arm the todo nudge, with the echo an accepted reminder owes
    /// and the host's leave to extend a run.
    ///
    /// The halves and their wordings are the module doc's own; what
    /// this registration alone owns are the two contracts the host
    /// codes against. `echo` fires once, with the reminder text, at
    /// the moment a follow-up turn is injected — observation, not
    /// display; the TUI passes a no-op so the reminder surfaces
    /// nowhere a user reads. `may_extend` is the decline lane: asked
    /// at every consult — clean run ends the turn budget leaves room
    /// to offer at, and every turn-top the contributor makes —
    /// before the plan is read, so it answers even when no offer can
    /// materialize; `false` declines whatever the plan would have
    /// offered, unspent and unechoed. One accepted window remains: a
    /// quit racing between the predicate's acceptance and the
    /// engine's cancel may still echo and pay for a reminder turn
    /// the teardown then stops — the same window a flag check beside
    /// any engine turn leaves. Without the pair the runner builds
    /// without the nudge and runs end where the model leaves them.
    #[must_use]
    pub fn with_todo_nudge(
        mut self,
        echo: impl Fn(String) + Send + Sync + 'static,
        may_extend: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        self.todo_nudge = Some((Arc::new(echo), Arc::new(may_extend)));
        self
    }

    /// Publish each claim's plan retirements the moment the claim
    /// makes them.
    ///
    /// The sink receives the entries a claim past the first retires —
    /// never-started plan entries dropped so the run inherits owed
    /// work and history, not abandoned ideas — at the claim edge
    /// itself: the call happens before the run the claim precedes has
    /// sent the provider anything, so a host whose display renders the
    /// plan can drop the retired rows while that run is still in
    /// flight instead of holding them until the run resolves. Spared
    /// claims — a runner's first, and any that retires nothing — call
    /// nothing. The batch still lands in
    /// [`take_claim_retirements`](Runner::take_claim_retirements);
    /// a host that registers a sink reads the change here and leaves
    /// the polling accessor to the hosts that never registered one.
    #[must_use]
    pub fn with_retirement_sink(
        mut self,
        sink: impl Fn(&[dch_tools::TodoEntry]) + Send + Sync + 'static,
    ) -> Self {
        self.retirement_sink = Some(Arc::new(sink));
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
        // Judged first, before a client is built or a server dialled: a
        // config assembled in code never passed through the loader's
        // validation, and the guard would otherwise be armed outside the
        // library's window without a word.
        dch_config::check_repetition_min_period_bytes(
            self.config.runner.repetition_min_period_bytes,
        )
        .map_err(|err| RunnerError::Config(err.to_string()))?;
        let context = Arc::new(
            RunnerContext::new(self.workdir.clone())
                .with_resolve_policy(resolve_policy_for(&self.config.runner)),
        );
        let fs_session = FileSession::new(self.workdir.clone())
            .with_resolve_policy(fs_resolve_policy_for(&self.config.runner));

        let client = Arc::new(crate::create_client(&self.config.api)?);
        let connections = connect_mcp_servers(self.config, &context.cwd).await?;
        let read_windows = read_windows_for(&self.config.runner);
        let (registry, core_registry) = compose_twin_registries(
            &connections,
            &fs_session,
            read_windows,
            self.interactive_tools,
            &self.mcp_providers,
        );

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

        let mut managers = LoopManagers::new()
            .with_stream_handler(stream_handler(&self.config.runner))
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
        let gate = permission_layer_live(
            self.permission_mode_cell.clone().unwrap_or_else(|| {
                std::sync::Arc::new(std::sync::Mutex::new(self.config.runner.permission_mode))
            }),
            self.permission_resolver,
            std::sync::Arc::clone(&self.permission_config_rules),
            self.permission_memory
                .clone()
                .unwrap_or_else(|| Arc::new(dch_permissions::empty_view)),
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

        let mut reminder_contributor: Option<crate::todo_nudge::TodoReminderContributor> = None;
        if let Some((echo, may_extend)) = self.todo_nudge {
            let gate = Arc::new(crate::todo_nudge::PlanGate::new());
            managers = managers.with_hook_executor(Arc::new(
                loopctl::hooks::HookExecutor::new().with_hook(Arc::new(
                    crate::todo_nudge::TodoNudgeHook::new(
                        Arc::clone(&context.todos),
                        Arc::clone(&gate),
                        echo,
                        Arc::clone(&may_extend),
                    ),
                )),
            ));
            reminder_contributor = Some(crate::todo_nudge::TodoReminderContributor::new(
                Arc::clone(&context.todos),
                gate,
                may_extend,
            ));
        }

        seed_standing_plan(&context, &self.history);

        let mut inner = BareLoop::<DchClient>::from_machine_with_managers(
            LoopMachine::from_history(self.history),
            session_config,
            client,
            registry,
            managers,
        );
        if let Some(contributor) = reminder_contributor {
            let steering = inner.steering();
            inner.add_contributor(Box::new(contributor.with_steering(steering)));
        }

        Ok(Runner {
            context_window: resolved_window,
            inner,
            context,
            fs_session,
            run_config,
            claimed_once: false,
            claim_retirements: Vec::new(),
            retirement_sink: self.retirement_sink,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::{
        RecordingObserver, SseServer, gemini_sse_text_turn, gemini_wire_config, message_roles,
        offline_config, recorded_requests, sse_batched_tool_calls_turn, sse_text_turn,
        sse_tool_call_turn, wire_config,
    };
    use loopctl::message::MessagePart;
    use serde_json::Value;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_working_without_a_runner_gets_the_start_reminder() {
        // The root-flow catch, at the engine: the model files an
        // all-pending plan, then fires a work tool without claiming any
        // entry. The start-discipline reminder rides the next request's
        // tail as steered guidance — a user message after the standing
        // conversation, never a mutation of it — and, once committed,
        // the episode stays quiet inside its window; the end-of-run
        // follow-up carries its own closing wording beside it (the
        // staleness clock is still inside grace there).
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"})),
            sse_text_turn("done for now"),
            sse_text_turn("noted, but not closing it"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x\n").expect("note");
        let echoed: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let echo_sink = Arc::clone(&echoed);
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(
                move |reminder| {
                    echo_sink.lock().expect("echo lock").push(reminder);
                },
                || true,
            )
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        assert_eq!(
            run.output.as_deref(),
            Some("noted, but not closing it"),
            "the run's output is the follow-up's answer — the plan still \
             stood at the end, so the end-of-run nudge fired too"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            4,
            "three model calls plus the end-of-run follow-up: {requests:?}"
        );
        let start = |request: &str| request.matches("no plan entry in progress").count();
        assert_eq!(
            start(&requests[0]),
            0,
            "the plan-filing turn predates any work: {requests:?}"
        );
        assert_eq!(
            start(&requests[1]),
            0,
            "the turn after filing is quiet — the work tool has not \
             fired yet: {requests:?}"
        );
        assert_eq!(
            start(&requests[2]),
            1,
            "the request after the Read carries the steered start reminder \
             at its tail: {requests:?}"
        );
        assert_eq!(
            message_roles(&requests[2], "no plan entry in progress"),
            vec!["user".to_string()],
            "the reminder travels as steered user guidance at the tail — \
             never a system insertion ahead of history: {requests:?}"
        );
        assert_eq!(
            start(&requests[3]),
            1,
            "the steered reminder is committed once — the follow-up replays \
             it exactly once and nothing more: {requests:?}"
        );
        let stacked = requests[3].matches("still not completed").count();
        assert_eq!(
            stacked, 1,
            "the follow-up carries its own closing wording alone — the \
             staleness clock is still inside grace there: {requests:?}"
        );
        let echoed = echoed.lock().expect("echo lock").clone();
        assert_eq!(
            echoed.len(),
            1,
            "the mid-run reminder echoes nothing — only the end-of-run \
             nudge owes the display a row: {echoed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_frozen_plan_gets_the_item_list_reminder() {
        // The staleness belt, at the engine: a plan filed, one work
        // tool fired (earning the start reminder), then the plan frozen
        // through the grace window. The request past the grace edge
        // carries the staleness reminder naming the undone entry —
        // icon, id, subject — as steered user guidance at the tail,
        // and the follow-up carries its own closing wording beside it.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"})),
            sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"})),
            sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"})),
            sse_text_turn("done for now"),
            sse_text_turn("noted, but not closing it"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x\n").expect("note");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(|_| {}, || true)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        assert_eq!(
            run.output.as_deref(),
            Some("noted, but not closing it"),
            "the follow-up's answer is the run's output"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            6,
            "five model calls plus the end-of-run follow-up: {requests:?}"
        );
        let stale = |request: &str| request.matches("the plan unchanged").count();
        for turn in 0..4 {
            assert_eq!(
                stale(&requests[turn]),
                0,
                "turn {turn} is inside the grace window — quiet: {requests:?}"
            );
        }
        assert_eq!(
            stale(&requests[4]),
            1,
            "past the grace window the frozen plan reminds, riding that \
             request's tail: {requests:?}"
        );
        assert!(
            requests[4].contains("○ [only] the work"),
            "the staleness reminder names the undone entry itself: {requests:?}"
        );
        assert_eq!(
            message_roles(&requests[4], "the plan unchanged"),
            vec!["user".to_string()],
            "the item-list reminder rides the tail as steered user \
             guidance: {requests:?}"
        );
        assert_eq!(
            stale(&requests[5]),
            1,
            "the steered reminder is committed once — the follow-up replays \
             it exactly once and nothing more: {requests:?}"
        );
        assert!(
            requests[5].contains("then finish"),
            "the end-of-run nudge closed the run as the final backstop: \
             {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_clean_plan_earns_no_mid_run_reminder() {
        // A plan in good standing never interrupts the work: a run
        // whose list is filed complete, then worked through, carries
        // no reminder text in any request.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "completed"}]
                }),
            ),
            sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"})),
            sse_text_turn("done"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "x\n").expect("note");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(|_| {}, || true)
            .build()
            .await
            .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        let requests = recorded_requests(&server);
        assert_eq!(requests.len(), 3, "three model calls: {requests:?}");
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("reminder:")),
            "a clean plan reminds nothing mid-run: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_planless_multistep_run_earns_one_creation_note() {
        // The third belt, at the engine: a run four turns deep with no
        // plan at all — the shape no stale-reminder can touch — gets
        // one note suggesting it file one, steered onto the next
        // request's tail as user guidance the model sees once.
        let script = vec![
            sse_tool_call_turn("Grep", &serde_json::json!({"pattern": "one"})),
            sse_tool_call_turn("Grep", &serde_json::json!({"pattern": "two"})),
            sse_tool_call_turn("Grep", &serde_json::json!({"pattern": "three"})),
            sse_tool_call_turn("Grep", &serde_json::json!({"pattern": "four"})),
            sse_text_turn("done"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(|_| {}, || true)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        assert_eq!(
            run.output.as_deref(),
            Some("done"),
            "a planless clean run ends where the model left it"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            5,
            "four work turns and the final answer: {requests:?}"
        );
        let note = |request: &str| request.matches("file one with TodoWrite now").count();
        for turn in 0..3 {
            assert_eq!(
                note(&requests[turn]),
                0,
                "the first three turns stay quiet — short work meets no \
                 note: {requests:?}"
            );
        }
        assert_eq!(
            note(&requests[3]),
            1,
            "the fourth planless turn is told, once, at that request's \
             tail: {requests:?}"
        );
        assert_eq!(
            message_roles(&requests[3], "file one with TodoWrite now"),
            vec!["user".to_string()],
            "the note rides the tail as steered user guidance, never a \
             system insertion ahead of history: {requests:?}"
        );
        assert_eq!(
            note(&requests[4]),
            1,
            "the steered note is committed once — the final request replays \
             it exactly once and nothing more: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_followup_turn_carries_its_own_wording_alone() {
        // The engine prepares the injected follow-up through the
        // contributors, and a plan that changed during the run's last
        // turn starts a fresh staleness clock — nothing is due at the
        // follow-up's consult, so its request carries the closing
        // wording alone. The mid-run belts stay clock-keyed across the
        // injection boundary.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "one", "subject": "first",
                               "status": "pending"}]
                }),
            ),
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "one", "subject": "first",
                               "status": "pending"},
                              {"id": "two", "subject": "second",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("done for now"),
            sse_text_turn("noted, but not closing it"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(|_| {}, || true)
            .build()
            .await
            .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            4,
            "three model turns plus the follow-up: {requests:?}"
        );
        let followup = &requests[3];
        assert!(
            followup.contains("then finish"),
            "the follow-up's own closing wording rides its prompt: {requests:?}"
        );
        assert!(
            !requests[2].contains("reminder:"),
            "a plan that just changed owes nothing — the clock restarted \
             with the write: {requests:?}"
        );
        let stacked = followup.matches("still not completed").count();
        assert_eq!(
            stacked, 1,
            "the follow-up carries its own wording alone — nothing was \
             due at its consult: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_nudged_builder_runs_exactly_one_engine_followup() {
        // The builder wiring every host rides: with a nudge callback
        // registered, a run that files a plan and leaves it undone is
        // extended by exactly one engine-injected reminder turn — the
        // callback fires with the reminder at the turn's injection,
        // the third request carries the reminder text as its prompt,
        // and the follow-up's own clean end earns nothing further,
        // whatever its answer left on the list.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("working on it"),
            sse_text_turn("noted, but not closing it"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let echoed: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let echo_sink = Arc::clone(&echoed);
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(
                move |reminder| {
                    echo_sink.lock().expect("echo lock").push(reminder);
                },
                || true,
            )
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds, follow-up included");
        assert_eq!(
            run.output.as_deref(),
            Some("noted, but not closing it"),
            "the follow-up's answer is the run's output — it joined the run"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            3,
            "the prompt's two turns and exactly one reminder: {requests:?}"
        );
        assert!(
            requests[2].contains("reminder: 1 todo item"),
            "the injected turn's prompt is the reminder wording: {requests:?}"
        );
        let echoed = echoed.lock().expect("echo lock").clone();
        assert_eq!(
            echoed,
            vec![
                "reminder: 1 todo item still not completed — finish the \
                  remaining work, or mark the entries done with TodoWrite, \
                  then finish (a completed request from pending lands \
                  in_progress — walk it with a follow-up call)"
                    .to_string()
            ],
            "the callback fired once, with the reminder, at the injection"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_declined_offer_runs_no_followup_and_writes_no_row() {
        // The leave-to-extend lane: a host that answers false at the
        // offer spends nothing — no reminder request leaves, no echo
        // fires, and the run's output is the model's own last word.
        // The gate is asked before the reminder is even built, so a
        // leaving session cannot pay for a turn its teardown would
        // abort.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("working on it"),
            sse_text_turn("never served"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let echoed: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let echo_sink = Arc::clone(&echoed);
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(
                move |reminder| {
                    echo_sink.lock().expect("echo lock").push(reminder);
                },
                || false,
            )
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds on its own turns");
        assert_eq!(
            run.output.as_deref(),
            Some("working on it"),
            "the model's own last word is the output — no reminder ran"
        );
        assert_eq!(
            recorded_requests(&server).len(),
            2,
            "the declined offer spent no request"
        );
        assert!(
            echoed.lock().expect("echo lock").is_empty(),
            "the declined offer echoed nothing"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_builder_without_a_nudge_callback_runs_plain() {
        // The opt-out is the host's decision: no callback, no hook,
        // and a run that files and abandons a plan ends where the
        // model left it — the same requests a nudge-free runner
        // always made.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("working on it"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        assert_eq!(run.output.as_deref(), Some("working on it"));
        assert_eq!(
            recorded_requests(&server).len(),
            2,
            "no callback registered, no reminder turn"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_ending_exactly_at_its_turn_budget_is_offered_no_followup() {
        // The offer's budget room: the engine consults the hook only when
        // the run's own turns left room — `turns_taken() < max_turns` — so
        // a run that files a plan and speaks its last word on the final
        // allowed turn lands clean with the plan undone and still earns no
        // reminder. The pin holds the boundary from the roomless side.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("working on it"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let echoed: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let echo_sink = Arc::clone(&echoed);
        let mut runner = Runner::builder(&wire_config(server.port, 2), dir.path())
            .with_todo_nudge(
                move |reminder| {
                    echo_sink.lock().expect("echo lock").push(reminder);
                },
                || true,
            )
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run completes on its own turns");
        assert_eq!(
            run.output.as_deref(),
            Some("working on it"),
            "the budget-exact end is a clean completion, not a budget error"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            2,
            "the run's two turns and no reminder — the budget had no room for \
             one: {requests:?}"
        );
        assert!(
            echoed.lock().expect("echo lock").is_empty(),
            "no echo is owed for an offer that was never made"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_with_one_turn_of_budget_room_gets_the_followup() {
        // The same clean end with one turn of room: the plan is equally
        // undone, the answer equally landed, and the engine now has budget
        // for the reminder — the offer depends on the room, not on the
        // plan alone. The boundary's other side, one turn apart.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("working on it"),
            sse_text_turn("noted, but not closing it"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 3), dir.path())
            .with_todo_nudge(|_| {}, || true)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds, reminder included");
        assert_eq!(
            run.output.as_deref(),
            Some("noted, but not closing it"),
            "the reminder's answer is the run's output"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            3,
            "one turn of room, one reminder: {requests:?}"
        );
        assert!(
            requests[2].contains("reminder: 1 todo item"),
            "the third request is the reminder: {requests:?}"
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
        config.runner.permission_mode = dch_config::PermissionMode::ReadOnly;
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
            requests[1].contains("denied by ReadOnly mode") && requests[1].contains("Write"),
            "the denial riding the second request must name the mode and the tool: {}",
            requests[1]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interactive_ask_resolves_through_the_builder_resolver_end_to_end() {
        let server = SseServer::start(vec![
            sse_tool_call_turn(
                "Bash",
                &serde_json::json!({"command": "touch bridge-probe"}),
            ),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 10);
        config.runner.permission_mode = dch_config::PermissionMode::Interactive;
        let consultations = Arc::new(Mutex::new(Vec::<(String, String, Value)>::new()));
        let resolver: std::sync::Arc<dyn loopctl::middleware::PermissionAskResolver> = {
            let consultations = Arc::clone(&consultations);
            Arc::new(RecordingResolver {
                seen: consultations,
                park: None,
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
        let (tool, prompt, input) = recorded.into_iter().next().expect("one consultation");
        assert_eq!(tool, "Bash");
        assert!(
            prompt.contains("Bash") && prompt.contains("Interactive"),
            "the prompt must label the tool and the mode: {prompt}"
        );
        assert_eq!(
            input,
            serde_json::json!({"command": "touch bridge-probe"}),
            "the typed ask carries the raw call input, verbatim"
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
    async fn a_rebuild_with_carried_history_keeps_the_read_guard_armed() {
        // The mid-session switch shape: the host rebuilds the runner on
        // a carried machine history and re-arms the guard from that
        // history's own Read call inputs — the fresh context's baseline
        // map starts empty, so without the re-arm an externally changed
        // file would take the stale write. The refusal wording is the
        // resumed-baseline shape: the model never saw these bytes in
        // this session's engine.
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
        let first = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        // The carried history: the conversation's earlier turn read the
        // file and the read came back clean — recorded verbatim in the
        // machine shape a switch carries, call paired to its result the
        // way the host's re-arm pairing reads it.
        let history = vec![
            Message::new(
                loopctl::message::Role::Assistant,
                vec![MessagePart::tool_call(
                    "call_1",
                    "Read",
                    serde_json::json!({"path": "note.txt"}),
                )],
            ),
            Message::new(
                loopctl::message::Role::User,
                vec![MessagePart::tool_result(
                    "call_1", "Read", "original", false,
                )],
            ),
        ];
        let rebuilt = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_history(history)
            .build()
            .await
            .expect("rebuilds with the carried history");
        drop(first);
        // The host's re-arm: every path the carried history shows read
        // and clean — the pairing rule the resume path ships.
        let mut succeeded = std::collections::HashSet::new();
        for message in rebuilt.committed_history() {
            for part in &message.parts {
                if let MessagePart::ToolResult {
                    call_id, is_error, ..
                } = part
                    && !matches!(is_error, Some(true))
                {
                    succeeded.insert(call_id.clone());
                }
            }
        }
        for message in rebuilt.committed_history() {
            for part in &message.parts {
                let MessagePart::ToolCall {
                    id, name, input, ..
                } = part
                else {
                    continue;
                };
                if !succeeded.contains(id) {
                    continue;
                }
                if name == "Read"
                    && let Some(path) = input.get("path").and_then(serde_json::Value::as_str)
                {
                    assert!(
                        rebuilt.record_resumed_read(path).await,
                        "the carried read re-arms on the rebuilt context"
                    );
                }
            }
        }
        std::fs::write(dir.path().join("note.txt"), "externally changed")
            .expect("mutate after the rebuild");
        let mut rebuilt = rebuilt;
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), rebuilt.run("go"))
            .await
            .expect("run settles within timeout")
            .expect("a refused tool is a soft error, not a run failure");
        assert_eq!(run.output.as_deref(), Some("done"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap_or_default(),
            "externally changed",
            "the re-armed guard held the write across the rebuild"
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
        let consultations = Arc::new(Mutex::new(Vec::<(String, String, Value)>::new()));
        let parked: Arc<
            Mutex<Vec<tokio::sync::oneshot::Sender<loopctl::middleware::PermissionResolution>>>,
        > = Arc::new(Mutex::new(Vec::new()));
        let resolver: std::sync::Arc<dyn loopctl::middleware::PermissionAskResolver> = {
            let consultations = Arc::clone(&consultations);
            let parked = Arc::clone(&parked);
            Arc::new(RecordingResolver {
                seen: consultations,
                park: Some(parked),
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
        let consulted: Vec<String> = consultations
            .lock()
            .expect("consultations lock")
            .iter()
            .map(|(tool, _prompt, _input)| tool.clone())
            .collect();
        assert_eq!(
            consulted,
            ["Write"],
            "reads are free in Interactive; only the write consults"
        );
        std::fs::write(dir.path().join("note.txt"), "externally changed")
            .expect("mutate during the ask");
        release
            .send(loopctl::middleware::PermissionResolution::AllowOnce)
            .expect("the resolver still awaits");
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

    /// The typed resolver fixture: records every ask's tool, prompt, and
    /// raw input, then either answers `AllowOnce` outright or parks the
    /// ask on a channel the test resolves.
    struct RecordingResolver {
        seen: Arc<Mutex<Vec<(String, String, Value)>>>,
        park: Option<
            Arc<
                Mutex<Vec<tokio::sync::oneshot::Sender<loopctl::middleware::PermissionResolution>>>,
            >,
        >,
    }

    impl loopctl::middleware::PermissionAskResolver for RecordingResolver {
        fn resolve<'a>(
            &'a self,
            ask: &'a loopctl::middleware::PermissionAsk,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = loopctl::middleware::PermissionResolution>
                    + Send
                    + 'a,
            >,
        > {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push((ask.tool_name.clone(), ask.prompt.clone(), ask.input.clone()));
            }
            match &self.park {
                Some(park) => {
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    if let Ok(mut parked) = park.lock() {
                        parked.push(reply_tx);
                    }
                    Box::pin(async move {
                        reply_rx
                            .await
                            .unwrap_or(loopctl::middleware::PermissionResolution::DenyOnce)
                    })
                }
                None => Box::pin(async { loopctl::middleware::PermissionResolution::AllowOnce }),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_explicit_window_beats_the_probe() {
        // An explicit window skips discovery altogether: the builder asks
        // nothing, and the run's completion is the server's first and only
        // request, answered with the turn the script holds for it.
        let server = SseServer::start(vec![sse_text_turn("answer")]).await;
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
        assert!(
            recorded_requests(&server).is_empty(),
            "an explicit window skips discovery: the builder asked nothing: {:?}",
            recorded_requests(&server)
        );
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(
            run.output.as_deref(),
            Some("answer"),
            "the run's answer arrives"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            1,
            "the run's completion is the one request: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repetition_floor_outside_the_library_range_refuses_the_build() {
        // A config assembled in code never passed through the loader's
        // validation, so the builder judges the floor itself, first of
        // all: 0 disarms the guard and stands; any other value must sit
        // within the library's window, and a refusal dials nothing.
        let server = SseServer::start(Vec::new()).await;
        let dir = TempDir::new().expect("tempdir");
        for (period, bound) in [(47_usize, "48-byte floor"), (4_097, "4096-byte period cap")] {
            let mut config = wire_config(server.port, 7);
            config.api.context_window = Some(90_000);
            config.runner.repetition_min_period_bytes = period;
            let err = Runner::builder(&config, dir.path())
                .build()
                .await
                .err()
                .expect("a floor outside the library's range refuses the build");
            assert!(
                matches!(
                    &err,
                    RunnerError::Config(message)
                        if message.contains(&format!("repetition_min_period_bytes = {period}"))
                            && message.contains(bound)
                ),
                "the refusal names the value and the bound: {err:?}"
            );
        }
        let mut config = wire_config(server.port, 7);
        config.api.context_window = Some(90_000);
        config.runner.repetition_min_period_bytes = 0;
        Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("0 disarms the guard and builds");
        assert!(
            recorded_requests(&server).is_empty(),
            "no request was made for any of these builds: {:?}",
            recorded_requests(&server)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unset_window_adopts_the_model_listing_answer() {
        // The servers without a /props document — vLLM and its kin —
        // publish each model's window on the listing route instead;
        // discovery reads it there when the capability document has
        // no answer.
        let server = SseServer::start(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"unrelated": true}"#.to_string(),
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
        // ask; after every live question goes unanswered, the
        // documented-model table sizes the session.
        let server = SseServer::start(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"unrelated": true}"#.to_string(),
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
    async fn a_runner_without_interactive_tools_never_advertises_the_ask() {
        // The knob the headless host turns: with it, the tools the
        // provider is offered carry no AskUserQuestion — the model
        // never sees a tool whose call could only fail — while the
        // default composition still offers it. The request body is
        // where the advertised set is visible, so that is what is
        // pinned.
        let ask_server = SseServer::start(vec![sse_text_turn("fine")]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut ask_runner = Runner::builder(&wire_config(ask_server.port, 5), dir.path())
            .build()
            .await
            .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(20), ask_runner.run("go"))
            .await
            .expect("the default run completes")
            .expect("the default run finishes");
        assert!(
            recorded_requests(&ask_server)
                .iter()
                .any(|body| body.contains("AskUserQuestion")),
            "the default composition advertises the ask tool"
        );

        let bare_server = SseServer::start(vec![sse_text_turn("fine")]).await;
        let bare_dir = TempDir::new().expect("tempdir");
        let mut bare_runner = Runner::builder(&wire_config(bare_server.port, 5), bare_dir.path())
            .without_interactive_tools()
            .build()
            .await
            .expect("constructs");
        tokio::time::timeout(std::time::Duration::from_secs(20), bare_runner.run("go"))
            .await
            .expect("the non-interactive run completes")
            .expect("the non-interactive run finishes");
        assert!(
            recorded_requests(&bare_server)
                .iter()
                .all(|body| !body.contains("AskUserQuestion")),
            "the non-interactive composition never mentions the ask tool"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_mid_run_nudge_rides_the_tail_and_keeps_the_prefix_stable() {
        // The cache contract: a provider with a prefix cache pays nothing
        // for the reminder. The start-discipline break lands mid-run — a
        // Bash call fires against a seeded plan whose entry stands
        // pending — and the reminder the engine raises must ride the
        // request as a TAIL user message, appended after the
        // conversation that already stands. A head-side reminder (a
        // system message inserted ahead of history) mutates the prefix
        // and re-prefills the whole context on every nudged turn; this
        // pin holds the earlier request's messages as an untouched
        // prefix of the next one.
        let script = vec![
            sse_tool_call_turn("Bash", &serde_json::json!({"command": "true"})),
            sse_text_turn("working"),
            sse_text_turn("closing out"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(|_| {}, || true)
            .build()
            .await
            .expect("constructs");
        runner
            .context()
            .todos
            .lock()
            .expect("todos lock")
            .push(dch_tools::TodoEntry {
                id: "only".to_string(),
                subject: "the work".to_string(),
                description: String::new(),
                status: dch_tools::TodoStatus::Pending,
                active_form: None,
            });
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("go"))
            .await
            .expect("the run settles")
            .expect("the run succeeds");

        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2,
            "the run made at least two requests, got {}",
            requests.len()
        );
        let messages_of = |body: &str| -> Vec<serde_json::Value> {
            let json_body = match body.split_once("\r\n\r\n") {
                Some((_, json_body)) => json_body,
                None => body,
            };
            serde_json::from_str::<serde_json::Value>(json_body)
                .expect("request body parses")
                .get("messages")
                .and_then(|messages| messages.as_array())
                .expect("messages array")
                .clone()
        };
        let first = messages_of(&requests[0]);
        let second = messages_of(&requests[1]);
        assert!(
            second.len() > first.len(),
            "the second request carries the turn's growth"
        );
        assert_eq!(
            &second[..first.len()],
            &first[..],
            "the first request's messages stand as an untouched prefix of the second — \
             a nudge inserted ahead of history breaks every prefix cache"
        );
        let message_text = |message: &serde_json::Value| -> String {
            message
                .get("content")
                .map_or_default(|content| match content.as_str() {
                    Some(text) => text.to_string(),
                    None => content.to_string(),
                })
        };
        let reminder_at_tail = second[first.len()..].iter().any(|message| {
            message.get("role").and_then(|role| role.as_str()) == Some("user")
                && message_text(message).contains("no plan entry in progress")
        });
        assert!(
            reminder_at_tail,
            "the start-discipline reminder rides the tail as a user message"
        );
        assert!(
            !second[..first.len()]
                .iter()
                .any(|message| message_text(message).contains("no plan entry in progress")),
            "no reminder text stands inside the stable prefix"
        );
    }
}
