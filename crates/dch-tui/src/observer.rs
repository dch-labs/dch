//! The agent-to-display bridge.
//!
//! `TuiObserver` is a [`LoopObserver`] that writes every
//! event the display renders into shared state and wakes the app —
//! the sole connection between a running agent loop and the terminal
//! UI. The observer performs no rendering and no I/O; it is a pure
//! state mutator whose every mutation ends with a notify.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use loopctl::middleware::SecretPatternSet;
use loopctl::observer::{
    AttemptResetContext, CompactedContext, CompactionFailedContext, LoopObserver,
    PreCompactionContext, ResponseContext, RunEndContext, StreamContext, StreamFailureContext,
    TextDeltaContext, ThinkingDeltaContext, ToolCallReceivedContext, ToolPreContext,
    TransportFallbackContext, TurnEndContext,
};

use crate::message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};

/// The longest an input summary renders.
///
/// Call inputs can be arbitrarily large JSON; the indicator line
/// needs a glanceable fragment, not the document. Sits above the
/// renderer's own display budget so the stash never clips what the
/// summary row could have shown.
const SUMMARY_LIMIT: usize = 120;

/// Map one compaction pass into its display record.
///
/// Takes the flat facts the event carries rather than the event
/// itself, so the mapping — the trigger's label, the millisecond
/// conversion, the finite-only ratio — is testable without forging
/// engine-side telemetry. An infinite ratio (an empty result) maps to
/// `None` rather than a value no JSON form can carry.
fn compaction_notice(
    tokens_before: u64,
    tokens_after: u64,
    reason: loopctl::compact::CompactReason,
    evicted: usize,
    duration: std::time::Duration,
    compactor: Option<&str>,
    compression_ratio: f64,
) -> TuiMessage {
    TuiMessage::Compacted {
        tokens_before,
        tokens_after,
        reason: reason.to_string(),
        evicted,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        compactor: compactor.map(str::to_string),
        compression_ratio: if compression_ratio.is_finite() {
            Some(compression_ratio)
        } else {
            None
        },
        timestamp: chrono::Utc::now(),
    }
}

/// Map one failed compaction pass into its error-row text.
///
/// Takes the flat facts the event carries rather than the event
/// itself, so the wording is testable without forging the engine's
/// non-exhaustive context. A missing cause is the pass that completed
/// yet still left the history over the window — named as such rather
/// than standing in as a placeholder for real cause text.
fn compaction_failure_text(tokens_before: u64, context_window: u64, cause: Option<&str>) -> String {
    match cause {
        Some(cause) => {
            format!("compaction failed at ~{tokens_before} of {context_window} tokens: {cause}")
        }
        None => format!(
            "compaction failed at ~{tokens_before} of {context_window} tokens: \
             the pass completed but the history still did not fit"
        ),
    }
}

/// How many stashed input summaries accumulate before the stash drops.
///
/// Summaries are useful only until their call dispatches or the turn
/// moves on; a wholesale drop at this depth bounds entries for calls
/// that never dispatch (unknown tools) without ordering machinery.
const PENDING_SUMMARY_CAP: usize = 64;

/// A completed tool call, formatted for display.
///
/// Carries what a glanceable result line needs — name, outcome,
/// duration — beside the full command and output the expansion
/// shows, taken from the dispatch-side capture when one exists.
/// The numeric loop-detection fingerprint never reaches these
/// fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultDisplay {
    /// The invoked tool's name.
    ///
    /// As reported by the dispatcher.
    pub name: String,

    /// The model-issued call id.
    ///
    /// Keys the runtime expansion data; empty when no capture
    /// recorded the call.
    pub call_id: String,

    /// Whether the call returned an error.
    ///
    /// Picks the line's success or failure styling.
    pub is_error: bool,

    /// How long the call took.
    ///
    /// Wall-clock time from dispatch to completion.
    pub duration: Duration,

    /// The call's input, compacted, as stashed at dispatch.
    ///
    /// Carried forward from the retiring active entry so the
    /// completed line can humanize ("Reading src/main.rs") without
    /// re-deriving the input; empty when no summary was stashed.
    pub input_summary: String,

    /// The call's retained output, as the capture recorded it.
    ///
    /// Same text as [`full_output`](Self::full_output) — carried
    /// twice so the transcript block's serialized field and the
    /// runtime expansion feed from one graduation. Empty when the
    /// call ran without a capture.
    pub output_preview: String,

    /// The call's full input, pretty-printed JSON, for expansion.
    ///
    /// Taken from the dispatch-side capture; empty when the call
    /// ran without one.
    pub full_input: String,

    /// The call's full output, within the retention cap, for
    /// expansion.
    ///
    /// Taken from the dispatch-side capture — the redacted text the
    /// pipeline returned; empty when the call ran without one.
    pub full_output: String,
}

/// One conversation-ordered completion event.
///
/// Replies and tool completions graduate through a single queue so
/// the conversation preserves the order the events actually
/// happened — separate per-kind buffers would drain in buffer order,
/// not event order, and a reply could land above the tool that
/// produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum Graduation {
    /// A finalized reply's committed text with its turn's reasoning.
    ///
    /// Pushed by the response event — for a non-streaming turn the
    /// only copy of the text. The thinking block rides the same
    /// graduation so one assistant message carries the turn's
    /// reasoning ahead of its answer; `None` when the turn streamed
    /// no reasoning at all.
    Reply {
        /// The committed reply text.
        ///
        /// The response event's own copy — for a non-streaming turn,
        /// the only one.
        text: String,

        /// The turn's graduated reasoning, if any.
        ///
        /// `None` for turns that streamed no reasoning; the block
        /// carries the concatenated text and its redaction marker.
        thinking: Option<ContentBlock>,
    },

    /// A completed tool call.
    ///
    /// Carries the input summary stashed at dispatch, so the
    /// graduated line humanizes without re-deriving the input.
    Tool(ToolResultDisplay),

    /// A finished run's total wall time.
    ///
    /// Pushed by the run-end event, after the run's own replies and
    /// tool completions — the queue's order is the events' order, so
    /// the display can stamp the total onto the run's final reply
    /// without tracking which reply ended the run.
    RunEnded {
        /// The run's total duration, in milliseconds.
        ///
        /// The engine-measured wall time from run start to run end —
        /// the same figure the engine reports to every observer.
        duration_ms: u64,
    },
}

/// One retry/reconnection line queued for the conversation.
///
/// Tagged with the failure episode it belongs to, so the display
/// updates a single row in place as the attempts of one episode
/// spend, and opens a fresh row only when a later episode begins.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryLine {
    /// The failure episode this line belongs to.
    ///
    /// One episode spans the stream failures and retries of one
    /// rough patch; a recovered stretch closes it, and the next
    /// failure opens the next id.
    pub episode: u64,

    /// The row text, pre-formatted for the conversation.
    ///
    /// Carries the `⟳` marker, the phase wording, and the attempt
    /// counter (`N/max`) — the app moves it into the episode's row
    /// verbatim, so no per-frame formatting ever re-runs. The `max`
    /// mirrors the stream handler's default retry ladder: the runner
    /// installs no custom ladder, and a host that does must keep this
    /// display in step with it.
    pub text: String,
}

/// The in-flight reasoning stream one turn is producing.
///
/// Held by [`TuiObserverState::thinking`]; the display reads it live
/// and the response event graduates it — as does a failed turn's end,
/// which graduates what streamed as a partial block instead of
/// dropping it. `began` is `None` until the
/// first delta arrives, so a turn with no reasoning carries no
/// duration.
#[derive(Debug, Clone, Default)]
pub struct ThinkingStream {
    /// The reasoning text so far, in arrival order.
    ///
    /// Concatenated delta by delta; the live row renders it as it
    /// grows and the graduation takes it whole.
    pub text: String,

    /// Whether the provider signalled redacted reasoning.
    ///
    /// An empty delta means the content was withheld; the section
    /// renders a placeholder rather than blanks.
    pub redacted: bool,

    /// The turn the stream belongs to.
    ///
    /// Carried from the delta context; a stream never spans turns,
    /// so a turn change implies the previous stream graduated or was
    /// discarded first.
    pub turn: usize,

    /// When the first delta arrived.
    ///
    /// Stamped once, then read for the live row's running duration
    /// and the graduated block's `duration_ms`; `None` until the
    /// stream starts.
    pub began: Option<Instant>,

    /// Whether the turn's reply text has begun streaming.
    ///
    /// Set by the first text delta while the stream is living; the
    /// live lane folds to its collapsed row then — the reply takes
    /// the display's attention while the block rides it out until
    /// graduation — and deltas keep arriving into the buffer. A
    /// stream orphaned by a dead run folds the same way when any
    /// later reply streams, but the render drops it instead: the
    /// folded row shows only for the stream whose own turn is the
    /// one replying, which
    /// [`TuiObserverState::reply_turn`](crate::observer::TuiObserverState::reply_turn)
    /// names.
    pub text_began: bool,

    /// When the reply text began, if it has.
    ///
    /// Stamped with the first text delta that set
    /// [`text_began`](Self::text_began); the folded row's duration
    /// reads through it so the timer freezes at the thinking phase's
    /// end instead of running on through the reply it no longer
    /// measures. `None` while the stream is still the live lane.
    pub text_began_at: Option<Instant>,
}

impl ThinkingStream {
    /// Take the stream's graduation, leaving it empty.
    ///
    /// Returns `None` when nothing accumulated — no text and no
    /// redaction marker — so turns without reasoning graduate no
    /// block. The duration comes from `began` when the stream ran.
    fn take_block(&mut self, now: Instant) -> Option<ContentBlock> {
        let began = self.began.take()?;
        // The phase the timer claims ended when the reply began — the
        // same instant the live row's frozen span measures — so the
        // graduated section carries the number the display already
        // showed and the handoff does not visibly rewrite it.
        let phase_end = self.text_began_at.take().unwrap_or(now);
        let block = if self.text.is_empty() && !self.redacted {
            None
        } else {
            Some(ContentBlock::Thinking {
                text: std::mem::take(&mut self.text),
                redacted: self.redacted,
                duration_ms: u64::try_from(phase_end.duration_since(began).as_millis())
                    .unwrap_or(0),
            })
        };
        self.redacted = false;
        self.turn = 0;
        self.text_began = false;
        self.text_began_at = None;
        block
    }

    /// Whether anything is on display — text or the redaction marker.
    ///
    /// The live row renders only while the stream has content.
    #[must_use]
    pub fn is_living(&self) -> bool {
        !self.text.is_empty() || self.redacted
    }
}

/// Shared, thread-safe state exchanged between the observer and the
/// display.
///
/// Construct this first, then split it via
/// [`into_observer`](Self::into_observer): the returned observer goes
/// to the agent as one of its observers; the retained state goes to
/// the app. Both halves hold clones of the same `Arc`s, so a write
/// on the observer side — or by the mode driver, the third holder —
/// is visible to the app side. The event is the
/// wake-up signal: the observer notifies after every mutation; the
/// app awaits a listener to know when to redraw. The mutexes guard
/// plain data and are never held across an await.
#[derive(Clone)]
pub struct TuiObserverState {
    /// Whether the observer scrubs secret-shaped text from tool input
    /// summaries.
    ///
    /// Carried from the config's `redact_secrets` at construction and
    /// consumed when the state splits into an observer — the pattern
    /// set is built there, once. The default is off.
    secret_redaction: bool,

    /// Retry and reconnect rows awaiting the conversation.
    ///
    /// Pushed by the retry cues — a stream attempt entering, tagged
    /// with the failure episode it belongs to — and drained by the
    /// display into transcript rows: the first line of an episode
    /// opens a row, later lines update it in place, so one rough
    /// patch reads as one row. Persisted in the session file and
    /// dropped on replay (the model never sees them), exactly like
    /// every other lifecycle row.
    pub retry_lines: Arc<Mutex<Vec<RetryLine>>>,

    /// The failure episode currently open, if any.
    ///
    /// Opened by the first stream failure after a settled stretch,
    /// closed when the turn settles or the stream recovers. The
    /// attempt-reset cue attaches its row to the open episode.
    retry_episode: Arc<Mutex<Option<u64>>>,

    /// The next failure-episode id to hand out.
    ///
    /// Monotonic for the process lifetime, so a recovered stretch's
    /// row can never be re-attached by a later episode sharing its
    /// id.
    retry_episode_counter: Arc<AtomicU64>,

    /// The in-flight assistant text, accumulated delta by delta.
    ///
    /// The response event records the turn's committed text in
    /// [`graduations`](Self::graduations) and clears this
    /// buffer — the buffer's own accumulation is not what graduates. A
    /// failed turn discards the partial text at its turn-end event, and
    /// a retried or fallback-served stream discards it at the reset
    /// cue, so the buffer never outlives its reply.
    pub streaming_text: Arc<Mutex<String>>,

    /// The turn the latest reply text delta belonged to.
    ///
    /// Stamped by every text delta and read by the live lane's
    /// render: a folded reasoning stream whose turn matches it is
    /// the reply's own — its collapsed row rides beside the
    /// streaming text until graduation — while a stream from any
    /// other turn is an orphan the display drops instead.
    pub reply_turn: Arc<std::sync::atomic::AtomicUsize>,

    /// The in-flight reasoning stream, accumulated delta by delta.
    ///
    /// Mirrors [`streaming_text`](Self::streaming_text): the response
    /// event graduates it onto the reply's message and clears it; a
    /// failed turn clears it so a cancelled run's reasoning never
    /// graduates later. A retried stream announces itself with the
    /// attempt-reset cue, which discards the dead attempt's partial
    /// reasoning here, so the retry starts from an empty buffer and
    /// only live reasoning ever graduates; a fallback-served turn
    /// clears it the same way at the fallback cue. The first delta
    /// stamps `began`, which the live row's running duration and the
    /// graduated block's `duration_ms` measure from. The reply's first
    /// text delta folds the live display; accumulation continues
    /// regardless.
    pub thinking: Arc<Mutex<ThinkingStream>>,

    /// Conversation events waiting to graduate, oldest first.
    ///
    /// Replies and tool completions arrive here in the order they
    /// happen, whatever kind they are; the display drains the queue
    /// on redraw and graduates each event into the conversation, so
    /// completed calls stay visible without the buffer accumulating
    /// across a session.
    pub graduations: Arc<Mutex<Vec<Graduation>>>,

    /// Machine histories from turn ends, oldest first.
    ///
    /// Written by the turn-end event of every successful turn, which
    /// carries the engine's own coherent snapshot — the committed
    /// history plus the run's turns so far — behind an Arc, and
    /// drained by the display, which hands the newest to the
    /// turn-end hook beside the conversation. Failed turns publish
    /// nothing here: their committed-only set is stale against the
    /// resolution read the mode driver takes when the run lands, and
    /// a late stale publish would overwrite it. Entries left behind
    /// by a slow frame are superseded, not replayed: the persistence
    /// layer keeps only the newest machine half anyway. Not cleared
    /// by [`reset`](TuiObserver::reset) — like the queued counter, it
    /// is persistence-bound state that outlives the turn it came
    /// from.
    pub turn_histories: Arc<Mutex<Vec<Arc<Vec<loopctl::message::Message>>>>>,

    /// The newest engine-truth history, however it was last known.
    ///
    /// Written at every successful turn end and by the mode driver
    /// when a run resolves (its committed read), so the display's
    /// turn-end hook can re-stamp it beside every display publish —
    /// without that, a run's final error rows re-stamp the display
    /// half after the engine half's last publish, the pair rule reads
    /// the engine half as stale, and the next resume derives from the
    /// display transcript instead of adopting the snapshot. The
    /// content is always a coherent committed set, so a re-stamp
    /// never claims more truth than the history has.
    pub machine_history: Arc<Mutex<Option<Arc<Vec<loopctl::message::Message>>>>>,

    /// Tools currently executing.
    ///
    /// One entry per dispatched call, removed on completion.
    pub active_tools: Arc<Mutex<Vec<ActiveTool>>>,

    /// Whether a submission is currently executing.
    ///
    /// Set by the mode driver around each run; the display reads it
    /// to route Ctrl+C — a run in flight takes the press as a
    /// cancel, an idle session takes it as the quit chord's first
    /// half.
    pub agent_running: Arc<std::sync::atomic::AtomicBool>,

    /// Full tool calls captured at dispatch, keyed by call id.
    ///
    /// Written by the capture middleware on the dispatch pipeline
    /// and drained by [`finish_tool`](TuiObserver::finish_tool) as
    /// calls complete — the lifecycle events alone carry no output
    /// text. Absent in modes that install no middleware; entries
    /// that never graduate are bounded by the store's own cap.
    pub tool_captures: crate::tool_capture::ToolCaptureSink,

    /// Run-level failures the display has not taken yet.
    ///
    /// Written by the mode driver when a submitted task's run fails;
    /// the display drains the buffer on redraw, surfacing each
    /// failure as an error row in the conversation.
    pub errors: Arc<Mutex<Vec<String>>>,

    /// Compaction notices awaiting the conversation.
    ///
    /// Pushed by the compaction event and drained by the display into
    /// transcript rows: one pass, one row, carrying the token counts
    /// and the pass's facts. Persisted in the session file like every
    /// other lifecycle row and dropped on replay — the engine's
    /// context is the machine snapshot's business, never the notice's.
    pub compactions: Arc<Mutex<Vec<crate::message::TuiMessage>>>,

    /// The context window the engine is actually running against.
    ///
    /// Seeded from the runner's resolved window at startup and
    /// refreshed by every turn-end event, so the status bar divides
    /// by the denominator the thresholds enforce — including one a
    /// provider supplied mid-session. Zero means unknown; the bar
    /// falls back to the config value while it holds.
    pub context_window: Arc<std::sync::atomic::AtomicU64>,

    /// Submitted tasks the agent driver has not taken yet.
    ///
    /// Incremented by the display before the submit is sent and
    /// decremented by the driver after its receive, so
    /// increment-before-send-before-receive-before-decrement is
    /// totally ordered under any scheduler and the count can never
    /// wrap. The input area's title renders it so a
    /// mid-run submit is visibly queued. Not cleared by
    /// [`reset`](TuiObserver::reset) — queued submissions outlive
    /// turn state.
    pub queued: Arc<std::sync::atomic::AtomicUsize>,

    /// Token usage counters, per-turn and cumulative.
    ///
    /// Updated from stream and turn events.
    pub tokens: Arc<Mutex<TokenCounts>>,

    /// The wake-up signal.
    ///
    /// The observer notifies after every mutation; the app's loop
    /// awaits a listener registered on it.
    pub render_notify: Arc<event_listener::Event>,

    /// Turn number of the most recent completed stream, zero-based.
    ///
    /// Mirrored from the observer's dedup ledger so the display layer
    /// can render progress without reaching into the observer itself;
    /// `None` until the first stream reports and after a reset.
    pub current_turn: Arc<Mutex<Option<usize>>>,

    /// Connection health for the notice row.
    ///
    /// Written by the stream lifecycle handlers and read by the
    /// display each frame: the retrying state holds for as long as
    /// retries run, and the reconnected note only until its hold
    /// lapses.
    pub health: Arc<Mutex<HealthNotice>>,
}

/// The connection-health line the notice row renders.
///
/// Written by the stream lifecycle handlers: a transport failure
/// raises the retrying state, the attempt-reset cue refines it with
/// the attempt number, a recovered stream replaces it with a brief
/// reconnected note, and every turn end settles it back to idle —
/// no retry outlives the turn it belonged to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthNotice {
    /// No retry is in flight.
    ///
    /// The resting state between runs and after a turn settles.
    Idle,
    /// A stream failed and a retry is expected or in flight.
    ///
    /// Raised on the transport-failure cue, which fires before the
    /// engine's retry decision; the attempt-reset cue refines the
    /// entry with the attempt number once the retry is certain.
    Retrying {
        /// The retry the stream is entering, 1-indexed against the
        /// ladder's retry budget.
        ///
        /// `None` until the retry starts — the failure cue fires
        /// before the engine decides whether to retry at all. Once
        /// set, the value is the same index the reconnection row
        /// prints: one less than the 1-indexed attempt the handler
        /// reports, so no two surfaces disagree by one.
        attempt: Option<usize>,
    },
    /// A retried stream recovered.
    ///
    /// Holds only for the notice row's usual hold before hiding, so
    /// recovery is confirmed without lingering.
    Reconnected {
        /// When recovery happened, for the hold's expiry.
        ///
        /// Stamped once at recovery so the display never restarts
        /// the hold.
        at: Instant,
    },
    /// A compaction pass is running.
    ///
    /// Raised at the pass-start cue and held until the pass settles —
    /// the success cue retires it into the transcript's compacted row,
    /// the failure cue into its error row — or the run ends, which
    /// retires it alone when a cancelled in-flight pass never fires
    /// either. A summarizer pass can hold for minutes on a slow
    /// backend, so this is a live state that renders in the
    /// conversation's live lane while it holds; `began` stamps the
    /// cue so the lane's running duration reads true.
    Compacting {
        /// When the pass-start cue fired.
        ///
        /// Stamped once at the raise; the live lane measures its
        /// running duration from this and never restarts it.
        began: Instant,
    },
}

impl TuiObserverState {
    /// Create empty shared state.
    ///
    /// The single construction entry point — the observer and the
    /// app are both derived from the value returned here, never from
    /// separately-allocated `Arc`s.
    #[must_use]
    pub fn new() -> Self {
        Self {
            streaming_text: Arc::new(Mutex::new(String::new())),
            reply_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            thinking: Arc::new(Mutex::new(ThinkingStream::default())),
            graduations: Arc::new(Mutex::new(Vec::new())),
            turn_histories: Arc::new(Mutex::new(Vec::new())),
            machine_history: Arc::new(Mutex::new(None)),
            active_tools: Arc::new(Mutex::new(Vec::new())),
            agent_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tool_captures: Arc::new(Mutex::new(HashMap::new())),
            errors: Arc::new(Mutex::new(Vec::new())),
            compactions: Arc::new(Mutex::new(Vec::new())),
            context_window: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tokens: Arc::new(Mutex::new(TokenCounts::default())),
            render_notify: Arc::new(event_listener::Event::new()),
            current_turn: Arc::new(Mutex::new(None)),
            health: Arc::new(Mutex::new(HealthNotice::Idle)),
            secret_redaction: false,
            retry_lines: Arc::new(Mutex::new(Vec::new())),
            retry_episode: Arc::new(Mutex::new(None)),
            retry_episode_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Arm secret scrubbing of tool input summaries.
    ///
    /// With this set, the observer masks secret-shaped text in the
    /// one-line input summaries it stashes — the same pattern set the
    /// dispatch pipeline's redacting pass applies to tool output — so
    /// a credential echoed inside a command or file content is
    /// displayed and persisted masked. The default is off: redaction
    /// is the host's choice, made once in the config.
    #[must_use]
    pub fn with_secret_redaction(mut self, enabled: bool) -> Self {
        self.secret_redaction = enabled;
        self
    }

    /// Split this state into an observer and the retained state.
    ///
    /// The two are produced together and share the same underlying
    /// allocations, so they can never drift. Cheap — cloning the
    /// state bumps one reference count per shared buffer and copies
    /// no data.
    #[must_use]
    pub fn into_observer(self) -> (TuiObserver, Self) {
        let observer = TuiObserver {
            state: self.clone(),
            pending_summaries: Mutex::new(HashMap::new()),
            last_counted_turn: Mutex::new(None),
            secret_scrub: self.secret_redaction.then(SecretPatternSet::default_common),
        };
        (observer, self)
    }
}

impl Default for TuiObserverState {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`LoopObserver`] that pushes updates into shared state.
///
/// Constructed via
/// [`TuiObserverState::into_observer`](TuiObserverState::into_observer)
/// and registered with the agent as one of its observers. Every
/// callback writes into the shared state and notifies, so a display
/// sharing those `Arc`s redraws. The observer performs no rendering
/// and no I/O.
pub struct TuiObserver {
    /// The shared state every callback writes into.
    ///
    /// The same allocations the display half holds, cloned from the
    /// state this observer was split from.
    state: TuiObserverState,

    /// The pattern set scrubbing tool input summaries, when the host
    /// armed input redaction.
    ///
    /// The same default set the dispatch pipeline's redacting pass
    /// applies to tool output; `None` leaves summaries verbatim.
    secret_scrub: Option<SecretPatternSet>,

    /// Input summaries keyed by call id, stashed when calls are
    /// received.
    ///
    /// Peeked by the dispatch event, never consumed — retries re-fire
    /// the dispatch, and every attempt renders its summary. Calls the
    /// model emits that are never dispatched (unknown tools appear
    /// only here) would otherwise linger: the stash is dropped
    /// wholesale once [`PENDING_SUMMARY_CAP`] entries accumulate, and
    /// `reset` clears it with the rest.
    pending_summaries: Mutex<HashMap<String, String>>,

    /// The turn whose stream tokens were already accumulated.
    ///
    /// The double-count guard: a turn-end event carrying totals the
    /// stream event already counted is skipped.
    last_counted_turn: Mutex<Option<usize>>,
}

/// Lock a mutex, recovering from poisoning.
///
/// A poisoned lock means some other thread panicked mid-mutation;
/// the data may be stale but the display must keep rendering, so the
/// guard is recovered rather than the poison propagated.
fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Condense a tool call's input into a glanceable summary.
///
/// Extracts the call's primary value from the full input JSON while
/// it is still in hand, then caps it on a character boundary — a
/// capped bare value stays usable for humanizing, where capped JSON
/// would no longer parse.
fn summarize_input(tool: &str, input: &serde_json::Value) -> String {
    let value = crate::tool_render::display_input(tool, input);
    if value.chars().count() <= SUMMARY_LIMIT {
        return value;
    }
    let truncated: String = value
        .chars()
        .take(SUMMARY_LIMIT.saturating_sub(1))
        .collect();
    format!("{truncated}…")
}

impl TuiObserver {
    /// Wake the display after a mutation.
    ///
    /// A listener registered before the notify observes it; a notify
    /// with no registered listener is dropped — which is why the
    /// app's run loop keeps one listener registered across draws
    /// and event handling. A residual window remains: a mutation
    /// landing in the gap between a fired listener and the loop's
    /// re-registration finds none, and its wake-up is lost — the
    /// shared state already holds the mutation, and the caret's
    /// blink tick forces a frame within half a second, so the cost
    /// is a bounded render delay, never a lost update. Never
    /// blocks either way.
    fn notify(&self) {
        self.state.render_notify.notify(1);
    }

    /// Record a completed tool call and retire its active entry.
    ///
    /// The call id pairs this completion with its dispatch exactly —
    /// same-tool retries and parallel calls included. The retiring
    /// entry's input summary carries forward, so the completed line
    /// can humanize without re-deriving the call's input, and the
    /// dispatch-side capture — when the pipeline installed one —
    /// rides along for the expansion. The loop-detection
    /// fingerprint and any display hint stay out of the recorded
    /// fields.
    pub fn finish_tool(&self, call_id: &str, name: &str, is_error: bool, duration: Duration) {
        let input_summary = {
            let mut tools = recover(&self.state.active_tools);
            tools
                .iter()
                .position(|tool| tool.call_id == call_id)
                .map(|position| tools.remove(position).input_summary)
                .unwrap_or_default()
        };
        let (full_input, full_output) = {
            let mut captures = recover(&self.state.tool_captures);
            captures
                .remove(call_id)
                .map(|capture| (capture.input_json, capture.output))
                .unwrap_or_default()
        };
        recover(&self.state.graduations).push(Graduation::Tool(ToolResultDisplay {
            name: name.to_string(),
            call_id: call_id.to_string(),
            is_error,
            duration,
            input_summary,
            output_preview: full_output.clone(),
            full_input,
            full_output,
        }));
        self.notify();
    }

    /// Discard the live stream lanes for `turn`.
    ///
    /// Shared by the attempt-reset and transport-fallback cues: both
    /// mean the partially streamed prefix the display holds is dead —
    /// a retry replaces it with a fresh attempt, a fallback with a
    /// non-streaming reply — so neither lane may keep showing it and
    /// neither may graduate. The thinking buffer resets only when it
    /// belongs to `turn`; a buffer left over from an older turn is
    /// the delta handlers' discard to make. The text buffer carries
    /// no turn of its own and always clears.
    fn discard_live_streams(&self, turn: usize) {
        {
            let mut stream = recover(&self.state.thinking);
            if stream.is_living() && stream.turn == turn {
                *stream = ThinkingStream::default();
            }
        }
        recover(&self.state.streaming_text).clear();
    }
}

/// The name this observer reports to the loop.
///
/// Constant so the trait's borrowed-string return needs no
/// per-call construction.
const OBSERVER_NAME: &str = "tui";

impl LoopObserver for TuiObserver {
    fn name(&self) -> &str {
        OBSERVER_NAME
    }

    fn on_text_delta(&self, ctx: &TextDeltaContext) {
        {
            let mut stream = recover(&self.state.thinking);
            if stream.is_living() && !stream.text_began {
                // The reply's own text folds the live reasoning lane
                // to its collapsed row — a living stream folds here,
                // stale or not — and the fold freezes the timer at
                // the thinking phase's end.
                stream.text_began = true;
                stream.text_began_at = Some(Instant::now());
            }
        }
        self.state
            .reply_turn
            .store(ctx.turn, std::sync::atomic::Ordering::SeqCst);
        recover(&self.state.streaming_text).push_str(&ctx.delta);
        self.notify();
    }

    fn on_thinking_delta(&self, ctx: &ThinkingDeltaContext) {
        {
            let mut stream = recover(&self.state.thinking);
            if stream.is_living() && stream.turn != ctx.turn {
                // A stream never spans turns; a turn change with
                // living content means the previous turn's terminal
                // event never arrived — discard rather than splice.
                *stream = ThinkingStream::default();
            }
            if ctx.delta.is_empty() {
                stream.redacted = true;
            } else {
                stream.text.push_str(&ctx.delta);
            }
            stream.turn = ctx.turn;
            stream.began.get_or_insert_with(Instant::now);
        }
        self.notify();
    }

    fn on_response(&self, ctx: &ResponseContext) {
        let thinking = {
            let mut stream = recover(&self.state.thinking);
            if stream.is_living() && stream.turn != ctx.turn {
                // A cancelled run's terminal event may never arrive;
                // its reasoning must not graduate onto another turn's
                // reply.
                *stream = ThinkingStream::default();
                None
            } else {
                stream.take_block(Instant::now())
            }
        };
        if !ctx.text.is_empty() || thinking.is_some() {
            recover(&self.state.graduations).push(Graduation::Reply {
                text: ctx.text.clone(),
                thinking,
            });
        }
        recover(&self.state.streaming_text).clear();
        self.notify();
    }

    fn on_stream_failure(&self, _ctx: &StreamFailureContext) {
        // The partial reasoning stays in the buffer: a retried attempt
        // resets it at its own cue (`on_attempt_reset`), and a terminal
        // failure's turn end graduates it — a timeout after minutes of
        // reasoning must not erase what the model produced.
        {
            let mut episode = recover(&self.state.retry_episode);
            if episode.is_none() {
                let next = self
                    .state
                    .retry_episode_counter
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *episode = Some(next);
            }
        }
        let mut health = recover(&self.state.health);
        if !matches!(*health, HealthNotice::Retrying { .. }) {
            *health = HealthNotice::Retrying { attempt: None };
        }
        drop(health);
        self.notify();
    }

    fn on_attempt_reset(&self, ctx: &AttemptResetContext) {
        self.discard_live_streams(ctx.turn);
        let max = loopctl::stream::handler::StreamRetryConfig::default().max_retries;
        let max = usize::try_from(max).unwrap_or(usize::MAX);
        // The retry index the row prints — one less than the 1-indexed
        // attempt the handler reports, clamped to the ladder's budget —
        // is the one number every surface carries; a raw attempt here
        // would read one higher than the row beside it.
        let attempt = ctx.attempt.saturating_sub(1).min(max);
        *recover(&self.state.health) = HealthNotice::Retrying {
            attempt: Some(attempt),
        };
        let episode = recover(&self.state.retry_episode);
        if let Some(episode) = *episode {
            recover(&self.state.retry_lines).push(RetryLine {
                episode,
                text: format!("⟳ connection lost — reconnection attempt {attempt}/{max}"),
            });
        }
        self.notify();
    }

    fn on_pre_compaction(&self, _ctx: &PreCompactionContext) {
        *recover(&self.state.health) = HealthNotice::Compacting {
            began: Instant::now(),
        };
        self.notify();
    }

    fn on_compaction(&self, ctx: &CompactedContext) {
        // The pass settled: the transcript's compacted row is the
        // durable record, and the running notice must yield to it.
        *recover(&self.state.health) = HealthNotice::Idle;
        // The bar must read the compacted figure the moment the pass
        // lands — waiting for the next turn end would leave it pegged
        // at the pre-pass size beside a row that just announced the
        // shrink.
        recover(&self.state.tokens).context_tokens = ctx.tokens_after;
        recover(&self.state.compactions).push(compaction_notice(
            ctx.tokens_before,
            ctx.tokens_after,
            ctx.reason,
            ctx.evicted_messages,
            ctx.telemetry.duration,
            ctx.stage
                .as_deref()
                .or(ctx.telemetry.compactor_name.as_deref()),
            ctx.telemetry.compression_ratio,
        ));
        self.notify();
    }

    fn on_compaction_failed(&self, ctx: &CompactionFailedContext) {
        // The pass died: the error row is the durable record; the
        // running notice yields the same way the success row makes it.
        *recover(&self.state.health) = HealthNotice::Idle;
        recover(&self.state.errors).push(compaction_failure_text(
            ctx.tokens_before,
            ctx.context_window,
            ctx.error.as_deref(),
        ));
        self.notify();
    }

    fn on_transport_fallback(&self, ctx: &TransportFallbackContext) {
        self.discard_live_streams(ctx.turn);
        // The reply arrives unstreamed on another transport; a
        // lingering retrying line would describe a stream that no
        // longer exists.
        *recover(&self.state.health) = HealthNotice::Idle;
        self.notify();
    }

    fn on_tool_call_received(&self, ctx: &ToolCallReceivedContext) {
        let mut summary = summarize_input(&ctx.tool, &ctx.input);
        if let Some(set) = &self.secret_scrub {
            set.scrub(&mut summary);
        }
        let mut stash = recover(&self.pending_summaries);
        if stash.len() >= PENDING_SUMMARY_CAP {
            stash.clear();
        }
        stash.insert(ctx.call_id.clone(), summary);
        drop(stash);
    }

    fn on_tool_pre(&self, ctx: &ToolPreContext) {
        let input_summary = recover(&self.pending_summaries)
            .get(&ctx.tool_call_id)
            .cloned()
            .unwrap_or_default();
        recover(&self.state.active_tools).push(ActiveTool {
            call_id: ctx.tool_call_id.clone(),
            name: ctx.tool.clone(),
            input_summary,
            start: Instant::now(),
        });
        self.notify();
    }

    fn on_tool_post(&self, ctx: &loopctl::observer::ToolPostContext) {
        self.finish_tool(&ctx.tool_call_id, &ctx.tool, ctx.is_error, ctx.duration);
    }

    fn on_stream_success(&self, ctx: &StreamContext) {
        {
            let mut tokens = recover(&self.state.tokens);
            tokens.input = ctx.input_tokens;
            tokens.output = ctx.output_tokens;
            tokens.cumulative_input = tokens.cumulative_input.saturating_add(ctx.input_tokens);
            tokens.cumulative_output = tokens.cumulative_output.saturating_add(ctx.output_tokens);
        }
        *recover(&self.last_counted_turn) = Some(ctx.turn);
        *recover(&self.state.current_turn) = Some(ctx.turn);
        *recover(&self.state.retry_episode) = None;
        let mut health = recover(&self.state.health);
        if matches!(*health, HealthNotice::Retrying { .. }) {
            *health = HealthNotice::Reconnected { at: Instant::now() };
        }
        drop(health);
        self.notify();
    }

    fn on_turn_end(&self, ctx: &TurnEndContext) {
        if !ctx.success {
            // A dead turn's partial reply text never graduates — the
            // engine keeps what it salvages and the next turn restarts
            // clean. The reasoning, though, exists only here: what
            // streamed before the cancel is the turn's own record, so
            // it graduates as a partial thinking block instead of
            // vanishing with the run. A retried attempt discarded its
            // dead reasoning at its own cues — the attempt reset or
            // the transport fallback — so this graduation carries only
            // what the turn's last live attempt streamed, and only
            // when it belongs to this turn.
            let thinking = {
                let mut stream = recover(&self.state.thinking);
                if stream.is_living() && stream.turn == ctx.turn {
                    stream.take_block(Instant::now())
                } else {
                    *stream = ThinkingStream::default();
                    None
                }
            };
            recover(&self.state.streaming_text).clear();
            if let Some(block) = thinking {
                recover(&self.state.graduations).push(Graduation::Reply {
                    text: String::new(),
                    thinking: Some(block),
                });
            }
        }
        // The turn settled either way; a retry line must not outlive
        // the turn it belonged to — a dead turn's failure surfaces as
        // the run's own error rows. The failure episode closes with
        // it: a later failure opens a fresh reconnection row.
        *recover(&self.state.health) = HealthNotice::Idle;
        *recover(&self.state.retry_episode) = None;
        let already_counted = recover(&self.last_counted_turn).is_some_and(|turn| turn == ctx.turn);
        *recover(&self.state.current_turn) = Some(ctx.turn);
        if ctx.success {
            // Only a successful turn's snapshot publishes: a failed
            // turn's committed-only set is stale the moment the run
            // resolves — the engine commits the salvage after the
            // event — and publishing it would overwrite the larger,
            // later resolution read with an older, smaller history.
            let history = Arc::clone(&ctx.history);
            recover(&self.state.turn_histories).push(Arc::clone(&history));
            *recover(&self.state.machine_history) = Some(history);
        }
        if let Some(window) = ctx.context_window {
            self.state
                .context_window
                .store(window, std::sync::atomic::Ordering::SeqCst);
        }
        let mut tokens = recover(&self.state.tokens);
        tokens.input = ctx.input_tokens;
        tokens.output = ctx.output_tokens;
        tokens.context_tokens = ctx.context_tokens;
        if !already_counted {
            tokens.cumulative_input = tokens.cumulative_input.saturating_add(ctx.input_tokens);
            tokens.cumulative_output = tokens.cumulative_output.saturating_add(ctx.output_tokens);
        }
        self.notify();
    }

    fn on_run_end(&self, ctx: &RunEndContext) {
        // A cancelled run can drop an in-flight compaction pass — no
        // pass-end cue ever fires — so the running notice must not
        // outlive the run it belonged to. Every other health state
        // already settles at its own cue; this is the belt for the one
        // that cannot.
        *recover(&self.state.health) = HealthNotice::Idle;
        recover(&self.state.graduations).push(Graduation::RunEnded {
            duration_ms: ctx.duration_ms,
        });
        self.notify();
    }

    fn reset(&self) {
        recover(&self.state.streaming_text).clear();
        *recover(&self.state.thinking) = ThinkingStream::default();
        recover(&self.state.graduations).clear();
        recover(&self.state.active_tools).clear();
        recover(&self.state.errors).clear();
        recover(&self.state.compactions).clear();
        recover(&self.state.retry_lines).clear();
        *recover(&self.state.retry_episode) = None;
        *recover(&self.state.tokens) = TokenCounts::default();
        *recover(&self.state.health) = HealthNotice::Idle;
        recover(&self.pending_summaries).clear();
        *recover(&self.last_counted_turn) = None;
        *recover(&self.state.current_turn) = None;
        self.notify();
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;
    use loopctl::compact::CompactReason;

    #[test]
    fn on_run_end_queues_the_run_total_for_the_final_reply() {
        let (observer, state) = TuiObserverState::new().into_observer();
        observer.on_run_end(&RunEndContext::new(true, None, 2, 84_000));
        assert_eq!(
            state.graduations.lock().unwrap().clone(),
            vec![Graduation::RunEnded {
                duration_ms: 84_000
            }],
            "the run-end event graduates the engine-measured total"
        );
    }

    #[test]
    fn compaction_failure_text_names_the_cause_or_the_unfit_pass() {
        let with_cause = compaction_failure_text(
            207_686,
            180_000,
            Some("qa summarizer summarize step failed: error sending request"),
        );
        assert!(
            with_cause.contains("compaction failed at ~207686 of 180000 tokens:"),
            "{with_cause}"
        );
        assert!(
            with_cause.contains("qa summarizer summarize step failed"),
            "the cause reaches the row verbatim: {with_cause}"
        );

        let unfit = compaction_failure_text(9_000, 8_000, None);
        assert!(
            unfit.contains("the pass completed but the history still did not fit"),
            "a causeless failure names its own shape: {unfit}"
        );
    }

    #[test]
    fn on_compaction_appends_exactly_one_display_record() {
        let (observer, state) = TuiObserverState::new().into_observer();
        assert!(state.compactions.lock().unwrap().is_empty());
        drop(observer);

        let record = compaction_notice(
            16_686,
            4_231,
            CompactReason::ThresholdExceeded,
            7,
            std::time::Duration::from_millis(312),
            Some("qa"),
            3.9,
        );
        match &record {
            TuiMessage::Compacted {
                tokens_before,
                tokens_after,
                reason,
                evicted,
                duration_ms,
                compactor,
                compression_ratio,
                ..
            } => {
                assert_eq!((*tokens_before, *tokens_after), (16_686, 4_231));
                assert_eq!(reason, "threshold exceeded");
                assert_eq!(*evicted, 7);
                assert_eq!(*duration_ms, 312);
                assert_eq!(compactor.as_deref(), Some("qa"));
                assert_eq!(*compression_ratio, Some(3.9));
            }
            other => panic!("the compaction event becomes a compacted record: {other:?}"),
        }

        let unlabelled = compaction_notice(
            10,
            0,
            CompactReason::Emergency,
            1,
            std::time::Duration::from_millis(5),
            None,
            f64::INFINITY,
        );
        match &unlabelled {
            TuiMessage::Compacted {
                reason,
                compactor,
                compression_ratio,
                ..
            } => {
                assert_eq!(reason, "emergency");
                assert_eq!(compactor.as_deref(), None);
                assert_eq!(*compression_ratio, None, "an infinite ratio is omitted");
            }
            other => panic!("still a compacted record: {other:?}"),
        }
    }
}
