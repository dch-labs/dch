//! The agent-to-display bridge.
//!
//! `TuiObserver` is a [`LoopObserver`] that writes every
//! event the display renders into shared state and wakes the app —
//! the sole connection between a running agent loop and the terminal
//! UI. The observer performs no rendering and no I/O; it is a pure
//! state mutator whose every mutation ends with a notify.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use loopctl::observer::{
    LoopObserver, ResponseContext, StreamContext, TextDeltaContext, ToolCallReceivedContext,
    ToolPreContext, TurnEndContext,
};

use crate::message::{ActiveTool, TokenCounts};

/// The longest an input summary renders.
///
/// Call inputs can be arbitrarily large JSON; the indicator line
/// needs a glanceable fragment, not the document.
const SUMMARY_LIMIT: usize = 60;

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

    /// A short preview of the call's output.
    ///
    /// Empty from the lifecycle events alone — they carry no output
    /// text; the conversation is the record of what a tool printed.
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
    /// A finalized reply's committed text.
    ///
    /// Pushed by the response event — for a non-streaming turn the
    /// only copy of the text.
    Reply(String),

    /// A completed tool call.
    ///
    /// Carries the input summary stashed at dispatch, so the
    /// graduated line humanizes without re-deriving the input.
    Tool(ToolResultDisplay),
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
    /// The in-flight assistant text, accumulated delta by delta.
    ///
    /// The response event records the turn's committed text in
    /// [`graduations`](Self::graduations) and clears this
    /// buffer — the buffer's own accumulation, which a retried stream
    /// can leave duplicated or truncated, is not what graduates. A
    /// failed turn discards the partial text at its turn-end event,
    /// so the buffer never outlives its reply.
    pub streaming_text: Arc<Mutex<String>>,

    /// Conversation events waiting to graduate, oldest first.
    ///
    /// Replies and tool completions arrive here in the order they
    /// happen, whatever kind they are; the display drains the queue
    /// on redraw and graduates each event into the conversation, so
    /// completed calls stay visible without the buffer accumulating
    /// across a session.
    pub graduations: Arc<Mutex<Vec<Graduation>>>,

    /// Tools currently executing.
    ///
    /// One entry per dispatched call, removed on completion.
    pub active_tools: Arc<Mutex<Vec<ActiveTool>>>,

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
            graduations: Arc::new(Mutex::new(Vec::new())),
            active_tools: Arc::new(Mutex::new(Vec::new())),
            tool_captures: Arc::new(Mutex::new(HashMap::new())),
            errors: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tokens: Arc::new(Mutex::new(TokenCounts::default())),
            render_notify: Arc::new(event_listener::Event::new()),
        }
    }

    /// Split this state into an observer and the retained state.
    ///
    /// The two are produced together and share the same underlying
    /// allocations, so they can never drift. Cheap — cloning the
    /// state bumps seven reference counts and copies no data.
    #[must_use]
    pub fn into_observer(self) -> (TuiObserver, Self) {
        let observer = TuiObserver {
            state: self.clone(),
            pending_summaries: Mutex::new(HashMap::new()),
            last_counted_turn: Mutex::new(None),
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
            output_preview: String::new(),
            full_input,
            full_output,
        }));
        self.notify();
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
        recover(&self.state.streaming_text).push_str(&ctx.delta);
        self.notify();
    }

    fn on_response(&self, ctx: &ResponseContext) {
        if !ctx.text.is_empty() {
            recover(&self.state.graduations).push(Graduation::Reply(ctx.text.clone()));
        }
        recover(&self.state.streaming_text).clear();
        self.notify();
    }

    fn on_tool_call_received(&self, ctx: &ToolCallReceivedContext) {
        let summary = summarize_input(&ctx.tool, &ctx.input);
        let mut stash = recover(&self.pending_summaries);
        if stash.len() >= PENDING_SUMMARY_CAP {
            stash.clear();
        }
        stash.insert(ctx.call_id.clone(), summary);
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
        self.notify();
    }

    fn on_turn_end(&self, ctx: &TurnEndContext) {
        if !ctx.success {
            recover(&self.state.streaming_text).clear();
        }
        let already_counted = recover(&self.last_counted_turn).is_some_and(|turn| turn == ctx.turn);
        let mut tokens = recover(&self.state.tokens);
        tokens.input = ctx.input_tokens;
        tokens.output = ctx.output_tokens;
        if !already_counted {
            tokens.cumulative_input = tokens.cumulative_input.saturating_add(ctx.input_tokens);
            tokens.cumulative_output = tokens.cumulative_output.saturating_add(ctx.output_tokens);
        }
        self.notify();
    }

    fn reset(&self) {
        recover(&self.state.streaming_text).clear();
        recover(&self.state.graduations).clear();
        recover(&self.state.active_tools).clear();
        recover(&self.state.errors).clear();
        *recover(&self.state.tokens) = TokenCounts::default();
        recover(&self.pending_summaries).clear();
        *recover(&self.last_counted_turn) = None;
        self.notify();
    }
}
