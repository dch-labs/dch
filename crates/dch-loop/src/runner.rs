//! The top-level agent: a thin owner of one configured `BareLoop` plus the
//! shared runner context.
//!
//! [`Runner`] is pure composition glue — it holds no agent-loop logic of its
//! own. [`Runner::builder`] constructs the loopctl ingredients (a concrete
//! provider client, a tool registry, a session config carrying the composed
//! system prompt, and the observer-bearing managers bundle), wires them into
//! a [`BareLoop`], and exposes [`Runner::run`] / [`Runner::cancel`] as
//! one-line delegations beside [`Runner::compact_now`], the between-turns
//! compaction seam.
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
//!
//! [`LoopObserver`]: loopctl::observer::LoopObserver
//! [`ToolMiddleware`]: loopctl::middleware::ToolMiddleware

mod build;
mod managers;
mod pipeline;
mod plan;
mod registries;
mod session;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use dch_tools::RunnerContext;
use loopctl::engine::BareLoop;
use loopctl::engine::Loop;
use loopctl::engine::Run;
use loopctl::engine::RunConfig;
use loopctl::error::LoopError;
use loopctl::message::Message;
use loopctl::tool::builtin::fs::FileSession;

use crate::DchClient;
use crate::question_bridge::QuestionPath;

pub use self::build::RunnerBuilder;

/// The top-level agent: the single type the rest of the application holds.
///
/// `Runner` is pure composition glue — it owns no agent-loop logic of its own.
/// [`Runner::builder`] assembles the loopctl ingredients (a concrete provider
/// client, a tool registry whose every dispatch sees the shared
/// [`RunnerContext`], a session config carrying the composed system prompt,
/// and the observer-bearing managers bundle) into one [`BareLoop`], and the
/// public surface stays thin: runs carry or retire the session's todo plan
/// and delegate to the loop, the between-turns seams (cancel, one manual
/// compaction pass) delegate the same way, and the rest are accessors.
///
/// Construct once per agent identity with [`Runner::builder`]…`.build()`,
/// then drive it with [`Runner::run`]: each call is one prompt → loop →
/// final answer against the configured provider, sharing one conversation
/// across calls. Each run starts by carrying the standing plan forward when
/// it holds unfinished entries — the plan belongs to the session — and
/// clearing it when the prior run finished everything. Cancellation is
/// cooperative via [`Runner::cancel`] or [`Runner::cancel_signal`].
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

    /// Whether any prompt has claimed this runner yet.
    ///
    /// The claim-edge retirement of never-started plan entries spares
    /// the first claim of a runner's life: a resumed runner's seeded
    /// plan is the prior session's open work arriving whole, and the
    /// first continued run owes it unchanged — every claim past that
    /// one is a new prompt, and a new prompt inherits owed work, not
    /// abandoned ideas.
    claimed_once: bool,

    /// The plan entries the last claim retired, awaiting the host.
    ///
    /// Cleared and refilled at every claim past the first that finds
    /// unfinished entries, and handed to the host through
    /// [`take_claim_retirements`](Self::take_claim_retirements)
    /// so the display and every plan reader can publish the change. A
    /// claim that finds every entry completed — the empty store
    /// included — clears the store and this batch both: the batch is
    /// strictly one claim's retirements, so a claim that retires
    /// nothing hands the host nothing, however the claim before it
    /// ended.
    claim_retirements: Vec<dch_tools::TodoEntry>,

    /// The host's synchronous eye on the claim's retirements.
    ///
    /// Set by
    /// [`with_retirement_sink`](RunnerBuilder::with_retirement_sink);
    /// when present it receives the batch the moment a claim retires
    /// entries — before the run the claim precedes has asked the
    /// provider anything — so a host whose display shows the plan can
    /// publish the retirement while that run is still in flight rather
    /// than after it resolves. The batch still accumulates for
    /// [`take_claim_retirements`](Self::take_claim_retirements); a
    /// host that registered a sink reads the change here and leaves
    /// the polling accessor to the hosts that never registered one.
    retirement_sink: Option<RetirementSink>,
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

    /// Run one prompt → loop → final answer, with the session-default policy.
    ///
    /// Uses the run policy mapped from `DchConfig` at construction (turn
    /// budget, dispatch policy). The session's todo plan carries into this
    /// run when it holds unfinished entries and retires when the prior run
    /// finished everything; a claim past the runner's first also retires
    /// entries still pending — never started — while in-progress entries
    /// carry as owed work. Returns when the model signals end-of-turn, the
    /// budget is exhausted, the loop detector aborts, or
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
    /// The session's todo plan carries into this run when it holds unfinished
    /// entries and retires when the prior run finished everything; a claim
    /// past the runner's first also retires entries still pending — never
    /// started, so a new prompt inherits the owed work (in progress) and
    /// the history (completed) but not the ideas an earlier prompt filed
    /// and left — while the first claim of a resumed runner carries the
    /// seeded plan whole. The
    /// returned [`Run`] records `run_config` as its own snapshot, so per-call
    /// overrides are visible in the run's accounting. This mirrors loopctl's
    /// own default/override pairing (`stream_messages` vs
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
        {
            let mut store = self
                .context
                .todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if store
                .iter()
                .all(|entry| matches!(entry.status, dch_tools::TodoStatus::Completed))
            {
                store.clear();
                self.claim_retirements.clear();
            } else if self.claimed_once {
                self.claim_retirements.clear();
                store.retain(|entry| {
                    let standing = !matches!(entry.status, dch_tools::TodoStatus::Pending);
                    if standing {
                        true
                    } else {
                        self.claim_retirements.push(entry.clone());
                        false
                    }
                });
            }
        }
        if let Some(sink) = &self.retirement_sink
            && !self.claim_retirements.is_empty()
        {
            sink(&self.claim_retirements);
        }
        self.claimed_once = true;
        self.inner.run(input, run_config).await
    }

    /// Take the plan entries the last claim retired.
    ///
    /// The claim-edge retirement drops never-started entries from the
    /// store where the run cannot see them; this hands the dropped
    /// entries to the host so the change is published — the display's
    /// plan section drops the retired rows instead of holding them
    /// until the next `TodoWrite` lands. Each claim's batch is
    /// replaced by the next: a host that polls after every run never
    /// misses one, and a host that never polls loses only the record,
    /// never the retirement itself. A host that needs the batch the
    /// moment the claim makes it — while the run is still in flight —
    /// registers
    /// [`with_retirement_sink`](RunnerBuilder::with_retirement_sink)
    /// instead and reads it there.
    #[must_use]
    pub fn take_claim_retirements(&mut self) -> Vec<dch_tools::TodoEntry> {
        std::mem::take(&mut self.claim_retirements)
    }

    /// Run one compaction pass over the committed history, now.
    ///
    /// The manual `/compact` seam: between runs, the engine's own
    /// context manager — the same compactor and thresholds the
    /// automatic passes use — folds the history, and the pruned result
    /// is installed as the machine's history so the next run continues
    /// from the compacted context. The returned pair carries the
    /// notice a host shows — the tokens the pass saved and the size it
    /// left the context, figures the pass itself measured, never
    /// derived — beside whether the pass actually changed the history:
    /// `false` for a fitting context the pass declined to fold, so a
    /// host's bookkeeping (a snapshot republish, a compaction count)
    /// can skip its work when nothing changed. A
    /// machine caught mid-turn refuses: compaction runs
    /// between runs, not under a live turn.
    ///
    /// # Errors
    ///
    /// Propagates the engine's own shapes — [`LoopError::InvalidInput`]
    /// when a turn is in flight, [`LoopError::CompactionFailed`] when
    /// the compactor itself fails, and
    /// [`LoopError::ContextExceeded`] when the compacted result still
    /// exceeds the window.
    pub async fn compact_now(&mut self) -> Result<(bool, String), LoopError> {
        let outcome = self.inner.compact_manual().await?;
        let notice = compaction_notice(&outcome);
        let compacted = matches!(outcome, loopctl::compact::EnsureContextResult::Compacted(_));
        Ok((compacted, notice))
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

    /// How many plan entries stand not completed, as the run left
    /// them.
    ///
    /// Read from the todo store the run's own `TodoWrite` calls filed —
    /// the same list every reminder counted. A host printing a session
    /// exit summary reports it so unfinished work is a fact the owner
    /// reads, not a nag the model receives.
    #[must_use]
    pub fn todo_undone(&self) -> usize {
        self.context
            .todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|entry| !matches!(entry.status, dch_tools::TodoStatus::Completed))
            .count()
    }

    /// The engine's context-size estimate, as the run left it.
    ///
    /// The same figure the engine reports per turn to observers — the
    /// estimated payload the provider would receive for the
    /// conversation as it stands. Readable between runs; a host
    /// saving session state after a run records it so a listing or a
    /// resumed session shows the context the run ended on.
    #[must_use]
    pub fn context_tokens(&self) -> u64 {
        self.inner.context_tokens()
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

/// The host's claim-edge publication for plan retirements.
///
/// Receives the entries a claim retired, the whole batch at once, at
/// the claim edge itself — the shape
/// [`with_retirement_sink`](RunnerBuilder::with_retirement_sink)
/// registers and the runner keeps beside its own polling accessor.
type RetirementSink = Arc<dyn Fn(&[dch_tools::TodoEntry]) + Send + Sync>;

/// The notice a manual compaction pass earned, from what the pass measured.
///
/// Pure over the outcome so the wording is pinnable without an engine:
/// the compacted arm carries only the figures the pass itself reported —
/// tokens saved and the size the context now measures, never a figure
/// derived from them — and the no-action arm says the context fits. The
/// wildcard arm exists because loopctl marks the outcome non-exhaustive;
/// its honest answer is a finished-without-figures notice, never a
/// fabricated number.
fn compaction_notice(outcome: &loopctl::compact::EnsureContextResult) -> String {
    match outcome {
        loopctl::compact::EnsureContextResult::Compacted(result) => format!(
            "compacted: {} tokens saved — the context now measures {} tokens",
            result.tokens_saved, result.tokens_after
        ),
        loopctl::compact::EnsureContextResult::NoAction(_) => {
            "nothing to compact — the context fits".to_string()
        }
        _ => "compaction pass finished".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::{
        CannedResponse, SseServer, offline_config, recorded_requests, sse_read_tool_call_turn,
        sse_text_turn, sse_tool_call_turn, wire_config,
    };
    use loopctl::observer::LoopObserver;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[test]
    fn the_compaction_notice_carries_only_measured_figures() {
        // Both arms pinned at the wording level: a compacted pass names
        // the tokens saved and the size the context now measures —
        // figures the pass itself reported, never derived from them —
        // and a no-action pass says the context fits without inventing
        // numbers.
        let compacted = loopctl::compact::EnsureContextResult::Compacted(
            loopctl::compact::CompactionOutcome::compacted(Vec::new(), 1_000, 650),
        );
        assert_eq!(
            compaction_notice(&compacted),
            "compacted: 350 tokens saved — the context now measures 650 tokens",
            "the compacted arm prints the pass's own figures"
        );
        let fitting = loopctl::compact::EnsureContextResult::NoAction(Vec::new());
        assert_eq!(
            compaction_notice(&fitting),
            "nothing to compact — the context fits",
            "the no-action arm claims nothing it did not measure"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_runner_reports_the_plan_entries_left_undone() {
        // The exit summary's source of truth: whatever the last
        // settled run left on the plan, readable after the run for a
        // host to state as a fact — the follow-up's own refusal to
        // close the list still standing.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
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
        assert_eq!(
            runner.todo_undone(),
            1,
            "the plan stands with one entry undone for the exit summary"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_runner_reports_the_loops_context_estimate_after_a_run() {
        // The figure a host saves with the transcript: nonzero once a
        // turn has run, so a listing that repeats it does not read a
        // zero the conversation's own size contradicts.
        let script = vec![sse_text_turn("done")];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        let before = runner.context_tokens();
        tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("do it"))
            .await
            .expect("run settles within timeout")
            .expect("the run succeeds");
        assert_eq!(
            before, 0,
            "a fresh session's empty conversation estimates nothing"
        );
        assert!(
            runner.context_tokens() > 0,
            "the run's conversation stands — the estimate with it"
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

    /// A store entry with the given id and status, subject and id alike.
    fn standing_entry(id: &str, status: dch_tools::TodoStatus) -> dch_tools::TodoEntry {
        dch_tools::TodoEntry {
            id: id.to_string(),
            subject: id.to_string(),
            description: String::new(),
            status,
            active_form: None,
        }
    }

    #[tokio::test]
    async fn a_second_claim_retires_never_started_entries() {
        // The plan a new prompt inherits: owed work, not abandoned
        // ideas. Entries still pending were never started — the rung
        // law has no way back into pending — so the claim past the
        // first drops them while the in-progress entry carries, and
        // the exit's unfinished count reads only what is owed. The
        // first claim is exempt: a runner fresh off a build owes its
        // standing plan whole.
        let config = offline_config();
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        {
            let mut store = runner.context.todos.lock().expect("todos lock");
            store.push(standing_entry("idea", dch_tools::TodoStatus::Pending));
            store.push(standing_entry("owed", dch_tools::TodoStatus::InProgress));
        }
        drop(runner.run("first prompt").await);
        let after_first: Vec<String> = runner
            .context
            .todos
            .lock()
            .expect("todos lock")
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        assert_eq!(
            after_first,
            ["idea", "owed"],
            "the first claim carries the standing plan whole"
        );
        drop(runner.run("second prompt").await);
        let after_second: Vec<String> = runner
            .context
            .todos
            .lock()
            .expect("todos lock")
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        assert_eq!(
            after_second,
            ["owed"],
            "the second claim retires the never-started entry"
        );
        assert_eq!(
            runner.todo_undone(),
            1,
            "the unfinished count reads only the owed work"
        );
        let retired = runner.take_claim_retirements();
        assert_eq!(
            retired
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["idea"],
            "the dropped entry is handed to the host for publishing"
        );
        assert!(
            runner.take_claim_retirements().is_empty(),
            "the take drains the batch — a polling host never re-reads a claim"
        );
    }

    #[tokio::test]
    async fn a_seeded_plan_carries_the_first_claim_and_retires_at_the_second() {
        // The resume composition the arms only half describe: the plan
        // arrives through the seed writer (a restored history's last
        // TodoWrite render), the first claim carries it whole, and the
        // second claim — a new prompt the model answered without
        // touching the plan — retires the never-started entry while
        // the in-progress one carries as owed work. The seed writer is
        // pinned in isolation (seed_standing_plan_adopted_iff_undone)
        // and the claim edges on a hand-pushed store
        // (a_second_claim_retires_never_started_entries); this walks
        // the writer combination a resumed session actually drives.
        fn ids_of(runner: &Runner) -> Vec<String> {
            runner
                .context
                .todos
                .lock()
                .expect("todos lock")
                .iter()
                .map(|entry| entry.id.clone())
                .collect()
        }
        let config = offline_config();
        let dir = TempDir::new().expect("tempdir");
        let seeded_render = "# Task List\n\nSummary: 1 pending, 1 in progress, 0 \
             completed\n\n○ [idea] the idea\n◐ [owed] the owed\n\n";
        let mut runner = Runner::builder(&config, dir.path())
            .with_history(vec![loopctl::message::Message::new(
                loopctl::message::Role::User,
                vec![loopctl::message::MessagePart::tool_result(
                    "c1",
                    "TodoWrite",
                    seeded_render,
                    false,
                )],
            )])
            .build()
            .await
            .expect("constructs");
        assert_eq!(
            ids_of(&runner),
            ["idea".to_string(), "owed".to_string()],
            "the seed writer installs the restored plan"
        );
        drop(runner.run("first prompt").await);
        assert_eq!(
            ids_of(&runner),
            ["idea".to_string(), "owed".to_string()],
            "the first claim carries the seeded plan whole"
        );
        drop(runner.run("second prompt").await);
        assert_eq!(
            ids_of(&runner),
            ["owed".to_string()],
            "the second claim retires the seeded never-started entry"
        );
        let retired = runner.take_claim_retirements();
        assert_eq!(
            retired
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["idea"],
            "the retired entry is the seed's own, handed to the host"
        );
        assert_eq!(
            runner.todo_undone(),
            1,
            "the owed work is all that reads undone"
        );
    }

    #[tokio::test]
    async fn a_retirement_sink_hears_the_batch_at_the_claim_itself() {
        // The claim edge, not the run's landing: the sink owes the host
        // the batch while the run it precedes is still in flight, and
        // the strictest offline proof of that is a run whose provider
        // refuses the connection outright — the run never answers
        // anything, yet the retirement the claim made has already been
        // published. The spared first claim calls nothing, and the
        // polling accessor still holds the batch a sink-registered
        // host leaves for the ones that never registered.
        let mut config = offline_config();
        config.api.base_url = "http://127.0.0.1:1".to_string();
        let dir = TempDir::new().expect("tempdir");
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let mut runner = Runner::builder(&config, dir.path())
            .with_retirement_sink(move |entries| {
                let mut seen = sink_seen.lock().expect("sink record lock");
                for entry in entries {
                    seen.push(entry.id.clone());
                }
            })
            .build()
            .await
            .expect("constructs");
        {
            let mut store = runner.context.todos.lock().expect("todos lock");
            store.push(standing_entry("idea", dch_tools::TodoStatus::Pending));
        }
        drop(runner.run("first prompt").await);
        assert!(
            seen.lock().expect("sink record lock").is_empty(),
            "the spared first claim retires nothing, so the sink hears nothing"
        );
        drop(runner.run("second prompt").await);
        assert_eq!(
            *seen.lock().expect("sink record lock"),
            ["idea".to_string()],
            "the claim published its retirement before the refused run resolved anything"
        );
        assert_eq!(
            runner
                .take_claim_retirements()
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["idea"],
            "the batch still waits in the polling accessor — the sink is a second leg, not a replacement"
        );
    }

    #[tokio::test]
    async fn a_claim_over_a_finished_plan_refires_nothing() {
        // The batch is strictly one claim's retirements. The sink host
        // never drains the polling accessor, so the retiring claim's
        // batch is still standing when the next claim arrives over an
        // empty store — every entry completed, which the empty store
        // trivially is — and that claim must hand the host nothing:
        // the store-clearing arm empties the batch beside the store,
        // so an already-published retirement cannot fire twice and a
        // polling host cannot re-read it.
        let mut config = offline_config();
        config.api.base_url = "http://127.0.0.1:1".to_string();
        let dir = TempDir::new().expect("tempdir");
        let fired: Arc<std::sync::atomic::AtomicUsize> =
            Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink_fired = Arc::clone(&fired);
        let mut runner = Runner::builder(&config, dir.path())
            .with_retirement_sink(move |_entries| {
                sink_fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .build()
            .await
            .expect("constructs");
        {
            let mut store = runner.context.todos.lock().expect("todos lock");
            store.push(standing_entry("idea", dch_tools::TodoStatus::Pending));
        }
        drop(runner.run("first prompt").await);
        drop(runner.run("second prompt").await);
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second claim published its retirement exactly once"
        );
        drop(runner.run("third prompt").await);
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the claim over the emptied store fires nothing — the retired \
             batch died with the plan that held it"
        );
        assert!(
            runner.take_claim_retirements().is_empty(),
            "a polling host reads the finished plan's claim as empty"
        );
    }

    #[tokio::test]
    async fn the_first_claim_carries_the_seeded_plan_whole() {
        // The resume half: a restored history whose last render holds
        // undone entries seeds the store, and the session's first
        // continued run owes that plan unchanged — the retirement is
        // for claims past the first, so resuming to finish open work
        // never finds its pendings gone.
        let config = offline_config();
        let dir = TempDir::new().expect("tempdir");
        let seeded_render = "# Task List\n\nSummary: 1 pending, 0 in progress, 0 \
             completed\n\n○ [only] the owed work\n\n";
        let history = vec![loopctl::message::Message::new(
            loopctl::message::Role::User,
            vec![loopctl::message::MessagePart::tool_result(
                "c1",
                "TodoWrite",
                seeded_render,
                false,
            )],
        )];
        let mut runner = Runner::builder(&config, dir.path())
            .with_history(history)
            .build()
            .await
            .expect("constructs");
        drop(runner.run("continue the work").await);
        let store = runner.context.todos.lock().expect("todos lock").clone();
        assert_eq!(
            store
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["only"],
            "the resumed session's first run carries its seeded plan whole"
        );
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

    #[tokio::test]
    async fn compact_now_at_rest_reports_a_fitting_context() {
        // The no-action arm through the whole method, not just the
        // formatter: a fresh runner's empty history fits, the pass
        // declines to fold anything, and the notice says so without
        // inventing figures. (The mid-turn refusal the doc names is the
        // engine's own guard, pinned on the engine's side: run() takes
        // the runner exclusively, so no host of this crate can hold a
        // live turn and issue compact_now at once — the TUI refuses
        // /compact while a run holds the agent before the engine's own
        // check is ever reachable.)
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&offline_config(), dir.path())
            .build()
            .await
            .expect("Runner::new constructs offline");
        let (compacted, notice) = runner
            .compact_now()
            .await
            .expect("an empty context needs no pass");
        assert_eq!(
            notice, "nothing to compact — the context fits",
            "the no-action notice is the pass's own verdict"
        );
        assert!(
            !compacted,
            "a fitting context changed no history — the host counts no pass"
        );
    }

    #[tokio::test]
    async fn compact_now_folds_the_committed_history_in_place() {
        // A history already past the threshold, folded at rest: the
        // runner's own committed history — the one the next run and the
        // host's snapshot read — is the folded result, and the notice
        // carries the figures the pass measured. This is the pass whose
        // result the TUI driver publishes after; pinned here at the
        // source, with the truncating selection so the pass is the
        // machine's own work. One scripted run first: the truncator
        // folds material older than the last run, so the seeded bulk
        // must sit behind a completed exchange to be droppable.
        let dir = TempDir::new().expect("tempdir");
        let seeds: Vec<loopctl::message::Message> = (0..20)
            .map(|turn| {
                let filler = "a line of context for the compactor ".repeat(120);
                if turn % 2 == 0 {
                    loopctl::message::Message::user(format!("earlier exchange {turn}: {filler}"))
                } else {
                    loopctl::message::Message::assistant(format!("earlier answer {turn}: {filler}"))
                }
            })
            .collect();
        let server = SseServer::start(vec![sse_text_turn("grown")]).await;
        let mut config = wire_config(server.port, 5);
        config.api.context_window = Some(40_000);
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let mut runner = Runner::builder(&config, dir.path())
            .with_history(seeds)
            .build()
            .await
            .expect("constructs over a fat history");
        tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the scripted run completes")
            .expect("the run finishes");
        let before = runner.committed_history().len();
        let (compacted, notice) = runner
            .compact_now()
            .await
            .expect("a manual pass at rest runs");
        assert!(
            notice.starts_with("compacted: "),
            "the notice carries the pass's own figures: {notice}"
        );
        assert!(compacted, "a real fold changed the history");
        let after = runner.committed_history().len();
        assert!(
            after < before,
            "the committed history the next run continues from is the folded one \
             ({before} -> {after})"
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

    /// Offers one piece of guidance from inside the run, at a turn's start.
    ///
    /// The loop emits `on_turn_start` after it has drained the steering slot
    /// for that turn and before it sends that turn's request, so an offer
    /// made here stands provably behind the boundary that could have read it.
    /// A wall-clock poll cannot establish that: an offer accepted while the
    /// run opens is still ahead of the first drain and rides the first
    /// request, which is the loop behaving correctly rather than the guidance
    /// going unread.
    struct TurnStartGuidance {
        /// The loop's slot, attached once the built runner hands it out.
        slot: Mutex<Option<Arc<loopctl::engine::Steering>>>,
        /// The guidance the observer offers.
        text: String,
        /// Whether the run in flight accepted the offer.
        accepted: std::sync::atomic::AtomicBool,
    }

    impl TurnStartGuidance {
        /// A guide holding `text`, with no slot yet and no offer made.
        fn new(text: &str) -> Arc<Self> {
            Arc::new(Self {
                slot: Mutex::new(None),
                text: text.to_string(),
                accepted: std::sync::atomic::AtomicBool::new(false),
            })
        }

        /// Hand the observer the slot the built runner owns.
        fn attach(&self, steering: Arc<loopctl::engine::Steering>) {
            *self.slot.lock().expect("the slot lock") = Some(steering);
        }

        /// Whether the run in flight took the offer.
        fn accepted(&self) -> bool {
            self.accepted.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl LoopObserver for TurnStartGuidance {
        fn name(&self) -> &'static str {
            "turn-start-guidance"
        }

        fn on_turn_start(&self, ctx: &loopctl::observer::TurnStartContext) {
            if ctx.turn > 0 || self.accepted() {
                return;
            }
            let offered = self
                .slot
                .lock()
                .expect("the slot lock")
                .as_ref()
                .is_some_and(|slot| slot.push(Message::user(self.text.clone())));
            if offered {
                self.accepted
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guidance_the_run_never_reads_is_left_for_the_host() {
        // One turn, so the run has exactly one boundary and nothing behind
        // it: the offer is placed after that boundary by the loop's own turn
        // event rather than raced for, which is what makes the run's failure
        // to reach it a fact of the shape rather than of the machine's speed.
        // A sentinel rather than a phrase — the request body carries the
        // system prompt and the tool schemas, where ordinary words appear by
        // coincidence and a negative assertion would prove nothing.
        let server = SseServer::start(vec![sse_text_turn("answered")]).await;
        let dir = TempDir::new().expect("tempdir");
        let guide = TurnStartGuidance::new("the answer is stale, late-4w7s");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_observer(Arc::clone(&guide) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let seam = runner.steering();
        guide.attach(Arc::clone(&seam));

        let run = tokio::time::timeout(std::time::Duration::from_secs(30), runner.run("answer"))
            .await
            .expect("the run settles within its budget")
            .expect("the run succeeds");
        assert_eq!(run.output.as_deref(), Some("answered"));
        assert!(
            guide.accepted(),
            "the run in flight took the guidance at its turn's start"
        );

        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            1,
            "one turn and no boundary after it: {requests:?}"
        );
        assert!(
            !requests[0].contains("late-4w7s"),
            "an offer behind the drain cannot retroactively enter the request \
             its turn sent: {:?}",
            requests[0]
        );

        let left = seam.take_all();
        let texts: Vec<String> = left.iter().map(Message::text_content).collect();
        assert_eq!(
            texts,
            ["the answer is stale, late-4w7s"],
            "what the run never reached comes back to the host, in the order it \
             arrived"
        );
        assert!(seam.is_empty(), "taking it clears the slot");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guidance_is_delivered_or_handed_back_but_never_lost() {
        // The law a host relies on, with no timing assumption in it: an offer
        // landing while a run is open — whenever in the run it lands, and the
        // moment it lands is what a loaded machine moves — either reaches the
        // model or comes back. So the earliest accepted moment is raced for
        // here rather than placed, and both outcomes pass while neither, and
        // especially both, cannot.
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
            if seam.push(Message::user("lost-2k9d")) {
                offered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::ZERO).await;
        }
        assert!(offered, "the run in flight took the offer");

        let run = task
            .await
            .expect("the task joins")
            .expect("the run succeeds");
        assert_eq!(run.output.as_deref(), Some("answered"));
        let delivered = recorded_requests(&server)
            .iter()
            .any(|request| request.contains("lost-2k9d"));
        let returned = seam.take_all();
        assert_ne!(
            delivered,
            !returned.is_empty(),
            "the guidance either rode a request or came back, never both and \
             never neither: delivered {delivered}, returned {} message(s)",
            returned.len()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unfinished_plan_carries_into_the_next_run() {
        // A dead endpoint makes the run fail, but the carry decision
        // happens before the request — proving the rule is tied to run
        // start, not to success. The prior run's open work stands into
        // the next run of the same session, ids and rungs intact.
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
                subject: "open work".to_string(),
                description: String::new(),
                status: dch_tools::TodoStatus::Pending,
                active_form: None,
            });
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi")).await;
        let store = runner.context().todos.lock().expect("todos lock").clone();
        assert_eq!(
            store.len(),
            1,
            "an unfinished plan carries into the session's next run"
        );
        assert_eq!(
            store.first().map(|entry| entry.status),
            Some(dch_tools::TodoStatus::Pending),
            "the carried entry stands in its rung"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_run_inherits_the_undone_plan() {
        // The carry rule on the wire: the first run files a pending plan
        // and refuses to close it; the second run, on the same runner,
        // adds one new entry — and its conversation shows the carried
        // entry standing beside the newborn, the rung and id intact.
        // Nothing re-files; the plan simply survived the run boundary.
        let script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("done for now"),
            sse_text_turn("noted, but not closing it"),
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "fresh", "subject": "the new work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("all handled"),
            sse_text_turn("closed out"),
        ];
        let server = SseServer::start(script).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .with_todo_nudge(|_| {}, || true)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("one"))
            .await
            .expect("the first run settles")
            .expect("the first run succeeds");
        assert_eq!(
            first.output.as_deref(),
            Some("noted, but not closing it"),
            "the first run ends on the follow-up's answer, plan still open"
        );
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("two"))
            .await
            .expect("the second run settles")
            .expect("the second run succeeds");
        assert_eq!(
            second.output.as_deref(),
            Some("closed out"),
            "the second run ends on its follow-up's answer — the carried \
             plan still earns its nudge"
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            6,
            "each run's turns plus its follow-up: {requests:?}"
        );
        let last = requests.last().expect("the second run's final request");
        assert!(
            last.contains("\u{25cb} [only] the work"),
            "the carried entry stands in the second run's conversation, id \
             and rung intact: {requests:?}"
        );
        assert!(
            last.contains("\u{25cb} [fresh] the new work"),
            "the newborn joined it without replacing anything: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_completed_plan_clears_for_the_next_run() {
        // The other half of the carry rule: a plan whose every entry the
        // prior run finished retires with it — the next run starts
        // fresh, not inheriting a wall of completed rows.
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
                subject: "finished work".to_string(),
                description: String::new(),
                status: dch_tools::TodoStatus::Completed,
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
            "a completed plan retires — the next run starts fresh"
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
