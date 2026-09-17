//! The terminal application shell.
//!
//! `TuiApp` owns the input buffer, the conversation, and the shared
//! state background producers write through; `run` drives the
//! render ↔ input ↔ scroll loop until the user quits.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::UnicodeWidthChar;

use dch_config::DchConfig;

use crate::Graduation;
use crate::events::TerminalEvents;
use crate::input::{InputAction, InputEditor};
use crate::markdown;
use crate::message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
use crate::observer::{ToolResultDisplay, TuiObserverState};
use crate::theme::Theme;
use crate::tool_render::SPINNER_FRAMES;
use dch_config::Verbosity;

/// The callback fired when a turn's outcome joins the conversation.
///
/// Installed by the mode driver to persist the session transcript;
/// receives the conversation snapshot, so the host never reaches
/// into display state.
type TurnEndHook = Box<dyn Fn(&[TuiMessage]) + Send + Sync>;

/// The largest elapsed span a persisted tool block may carry.
///
/// `Duration::from_secs_f64` panics past its own representable
/// range; a hostile or corrupted value beyond this bound renders as
/// zero instead of killing the first frame after resume.
const MAX_ELAPSED_SECS: f64 = 9.0e18;

/// The minimum spacing between background-initiated frames.
///
/// Redraws requested through the shared notify are coalesced to at
/// most one per interval, so a burst of stream deltas costs one
/// frame whatever the token rate.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How close to the newest line a scroll action must land to count
/// as "back at the bottom".
///
/// The streaming region grows between a scroll and the next layout,
/// so an exact offset-zero check would detach a view that only fell
/// behind by growth.
const STICK_TOLERANCE: usize = 2;

/// Lines one wheel or touchpad scroll event moves.
///
/// The terminal convention per notch: enough that touchpad momentum
/// accumulates into fast travel without overshooting a single
/// gesture.
const WHEEL_SCROLL_LINES: usize = 3;

/// The most lines of the live segment parsed as markdown per frame.
///
/// A single block that has not yet met a boundary is re-parsed whole
/// on every frame, so the parse cost grows with the entire unsettled
/// block; past this cap the block's head renders as plaintext until
/// it freezes, keeping the per-frame parse inside the frame budget.
const LIVE_PARSE_LINE_CAP: usize = 400;

/// The most bytes of the live segment parsed as markdown per frame.
///
/// Lines can be arbitrarily long — a minified code or data line
/// carries a whole block in one line — so the line cap alone does
/// not bound the parse. Past this byte cap the excess joins the
/// plaintext head, on a character boundary.
const LIVE_PARSE_BYTE_CAP: usize = 16_384;

/// The main TUI application.
///
/// Owns the event loop, the input buffer, and the shared state
/// background producers write through. Construct with
/// [`TuiApp::new`], which allocates the shared state fresh.
pub struct TuiApp {
    /// The active palette and element styles.
    ///
    /// Resolved from `config.display.theme` at construction; an
    /// unknown name warns and falls back to the default theme.
    pub theme: Theme,

    /// The conversation so far, oldest first.
    ///
    /// Submits append here; the conversation pane renders it and the
    /// scroll offset walks it from the bottom.
    conversation: Vec<TuiMessage>,

    /// The shared state the observer half of the display writes.
    ///
    /// Bundled as one value so the split pattern stays in a single
    /// place: an app built from externally-created state and its
    /// observer hold clones of these same allocations.
    state: TuiObserverState,

    /// Where submitted input lines travel to the agent.
    ///
    /// Set by the mode driver before the run loop; Enter sends each
    /// non-empty submit through it alongside the local echo. `None`
    /// on a bare app — submits render locally and reach no agent.
    submit_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,

    /// Notified when a turn's outcome lands in the conversation.
    ///
    /// Installed by the mode driver; receives the full conversation
    /// snapshot each time a reply graduates or a failure surfaces, so
    /// the host can persist it without reaching into display state.
    /// `None` on a bare app — turns come and go unpersisted.
    turn_end_hook: Option<TurnEndHook>,

    /// The display verbosity shaping tool lines.
    ///
    /// Initialized from the config and cycled at runtime; switching
    /// invalidates the conversation and live caches so completed
    /// bodies re-render in the new mode on the next frame.
    verbosity: Verbosity,

    /// The braille spinner's current frame.
    ///
    /// Advanced one frame per render tick while any tool runs;
    /// running lines rebuild every frame anyway, so the pulse costs
    /// nothing extra.
    spinner_idx: usize,

    /// The input editor: multi-line buffer, cursor, and history.
    ///
    /// Every non-app key routes here. Submits surface as
    /// [`InputAction::Submit`], echo into the conversation, and
    /// travel the submit channel to the agent driver.
    input: InputEditor,

    /// Lines scrolled up from the bottom of the view.
    ///
    /// Zero means pinned to the newest line; submitting resets it.
    scroll_offset: usize,

    /// Whether the view follows the newest content.
    ///
    /// Pinned views re-anchor to the bottom on every layout; any
    /// scroll-up detaches, and scrolling back within
    /// [`STICK_TOLERANCE`] lines of the bottom — or pressing End or
    /// submitting — re-arms.
    auto_scroll: bool,

    /// Whether a capped background redraw is still owed.
    ///
    /// Set when a notify arrives inside the frame interval; the
    /// periodic tick claims it so a stream's tail renders even if no
    /// further delta arrives.
    render_pending: bool,

    /// When the last background-initiated frame rendered.
    ///
    /// Gates redraws to one per [`FRAME_INTERVAL`]; `None` until the
    /// first background frame.
    last_frame: Option<Instant>,

    /// Total lines the previous layout produced.
    ///
    /// While detached, each layout compensates the scroll offset for
    /// growth or shrinkage since this value, holding the viewport
    /// steady as the streaming region extends below it or collapses.
    last_layout_lines: usize,

    /// Rendered lines of the streaming buffer's frozen prefix.
    ///
    /// Blocks before the frozen offset parsed and rendered once, when
    /// their terminating boundary arrived; frames re-use these lines
    /// instead of re-parsing settled content.
    frozen_lines: Vec<Line<'static>>,

    /// Where the streaming buffer's frozen prefix ends, in bytes.
    ///
    /// A safe boundary: the start of the line after a blank line,
    /// outside any fence, where the surrounding blocks cannot
    /// continue each other. Reset alongside the cache whenever the
    /// frozen prefix stops matching the buffer or the pane width
    /// changes.
    frozen_upto: usize,

    /// Separator rows owed after the frozen prefix.
    ///
    /// The blank run an interior boundary consumed becomes empty
    /// rows at the next join, matching the batch render's spacing
    /// for blank-separated blocks; a boundary taken at a closed
    /// fence owes the renderer's one inter-component row even
    /// though the source carried no blank, so the count floors at
    /// one. A run that precedes any frozen content is the
    /// document-leading run and follows the batch's absorb-one rule
    /// instead. Footnote definitions, rule-adjacent boundaries,
    /// indented fences inside lists, and a fence closer followed
    /// directly by a heading or the next list item render
    /// one-to-two rows off their batch spacing at the join — an
    /// accepted mid-stream approximation that self-corrects at
    /// graduation.
    frozen_separators: usize,

    /// Identity stamp of the frozen prefix.
    ///
    /// Recomputed whenever the freeze advances and re-checked on
    /// every layout: a buffer cleared and refilled to a comparable
    /// length fails the stamp and resets the cache, which a length
    /// comparison alone cannot distinguish from growth.
    frozen_fingerprint: u64,

    /// The pane width the streaming cache was built for.
    ///
    /// A resize discards the cache; the next layout re-freezes the
    /// same content at the new width.
    stream_cache_width: u16,

    /// Rendered lines of the streaming buffer's live segment.
    ///
    /// The live segment — the unfrozen complete lines plus the
    /// unterminated tail — only changes when a delta arrives, the
    /// freeze advances, or the pane resizes; input keystrokes and
    /// idle ticks re-render it identically. Frames re-use these
    /// lines until the segment's stamp moves, so a keystroke never
    /// pays the markdown re-parse (a live open code fence
    /// re-highlighted per frame costs frame-budget-breaking time).
    live_lines: Vec<Line<'static>>,

    /// Stamp of the live segment the cached lines were built from.
    ///
    /// A fingerprint of the unfrozen buffer suffix — content, not
    /// just length, so a cleared-and-refilled turn invalidates.
    live_stamp: u64,

    /// The freeze offset the live cache was built at.
    ///
    /// A live rebuild is valid only for the frozen prefix it
    /// started from; when the freeze advances, the cached lines
    /// describe a different suffix and this key forces the rebuild.
    live_frozen_upto: usize,

    /// The pane width the live cache was built at.
    ///
    /// Wrapped live lines are width-shaped; a resize re-wraps
    /// rather than reusing, so a narrowed pane never clips rows
    /// baked for the old width.
    live_width: u16,

    /// Rendered lines of the settled conversation.
    ///
    /// The conversation changes only when a message graduates, a
    /// submit echoes, a tool result drains, or an error lands —
    /// everything else a frame does to it is re-rendering identical
    /// markdown. The cache rebuilds on those mutations and on a width
    /// change; frames clone it, the same per-frame linear trait the
    /// streaming freeze cache has.
    conversation_cache: Vec<Line<'static>>,

    /// The pane width the conversation cache was built for.
    ///
    /// Settled markdown re-flows with the pane; a width change
    /// invalidates the cache wholesale so the next frame rebuilds
    /// every block at the new wrap.
    conversation_cache_width: u16,

    /// Monotonic count of settled-conversation mutations.
    ///
    /// Every message push and verbosity switch bumps it; the render
    /// cache detects staleness by falling behind, which keeps the
    /// comparison against the width key uniform.
    conversation_generation: u64,

    /// The generation the conversation cache was built at.
    ///
    /// Paired with
    /// [`conversation_generation`](Self::conversation_generation):
    /// the cache is stale exactly when the live count has moved
    /// past this stamp, whatever caused the bump.
    conversation_cache_generation: u64,

    /// Whether the run loop should exit.
    ///
    /// Set by the quit keys; the loop leaves at the top of its next
    /// iteration.
    quitting: bool,

    /// The application configuration this shell was built from.
    ///
    /// The status bar reads the model name from it.
    config: DchConfig,
}

impl TuiApp {
    /// Construct the shell with freshly-allocated, empty shared state.
    ///
    /// The TUI renders and responds to input; the shared buffers stay
    /// empty until a producer writes them.
    #[must_use]
    pub fn new(config: DchConfig) -> Self {
        Self::from_observer_state(config, TuiObserverState::new())
    }

    /// Construct the shell around externally-created shared state.
    ///
    /// The retained half of a state split through
    /// [`TuiObserverState::into_observer`] — the observer half goes
    /// to the agent — so writes from the running agent reach this
    /// app's display through the same allocations.
    pub fn from_observer_state(config: DchConfig, state: TuiObserverState) -> Self {
        let theme = if let Some(theme) = Theme::by_name(&config.display.theme) {
            theme
        } else {
            tracing::warn!("unknown theme '{}', using default", config.display.theme);
            Theme::default()
        };
        Self {
            theme,
            conversation: Vec::new(),
            state,
            submit_tx: None,
            turn_end_hook: None,
            verbosity: config.display.verbosity,
            spinner_idx: 0,
            input: InputEditor::new(),
            scroll_offset: 0,
            auto_scroll: true,
            render_pending: false,
            last_frame: None,
            last_layout_lines: 0,
            frozen_lines: Vec::new(),
            frozen_upto: 0,
            frozen_separators: 0,
            frozen_fingerprint: 0,
            stream_cache_width: 0,
            live_lines: Vec::new(),
            live_stamp: 0,
            live_frozen_upto: 0,
            live_width: 0,
            conversation_cache: Vec::new(),
            conversation_cache_width: 0,
            conversation_generation: 1,
            conversation_cache_generation: 0,
            quitting: false,
            config,
        }
    }

    /// The conversation so far, oldest first.
    ///
    /// A borrowed view; append through [`push_message`](Self::push_message).
    #[must_use]
    pub fn conversation(&self) -> &[TuiMessage] {
        &self.conversation
    }

    /// Append a message to the conversation.
    ///
    /// The observer and the session-resume path use this to populate
    /// the view; submitting from the input line goes through the same
    /// path internally.
    pub fn push_message(&mut self, message: TuiMessage) {
        self.conversation.push(message);
        self.conversation_generation = self.conversation_generation.saturating_add(1);
    }

    /// The input buffer's current text.
    ///
    /// Empty right after a submit; keystrokes and pastes edit it
    /// through the editor.
    #[must_use]
    pub fn input(&self) -> &str {
        self.input.text()
    }

    /// The scroll offset in lines above the bottom of the view.
    ///
    /// Zero means the newest line is visible.
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Whether the view follows the newest content.
    ///
    /// True while pinned; any scroll-up detaches until the view
    /// returns near the bottom.
    #[must_use]
    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    /// Whether the run loop should exit.
    ///
    /// True after a quit key; the loop observes it between events.
    #[must_use]
    pub fn is_quitting(&self) -> bool {
        self.quitting
    }

    /// The shared streaming-text buffer.
    ///
    /// One allocation writers append to and the display reads; it
    /// reads empty until a writer starts.
    #[must_use]
    pub fn streaming_text(&self) -> &Arc<Mutex<String>> {
        &self.state.streaming_text
    }

    /// The shared in-flight tool list.
    ///
    /// Producers record entries per dispatched call and remove them
    /// on completion.
    #[must_use]
    pub fn active_tools(&self) -> &Arc<Mutex<Vec<ActiveTool>>> {
        &self.state.active_tools
    }

    /// The shared token counters.
    ///
    /// The status bar's totals read through this handle.
    #[must_use]
    pub fn tokens(&self) -> &Arc<Mutex<TokenCounts>> {
        &self.state.tokens
    }

    /// The shared redraw request.
    ///
    /// Waking it triggers a redraw on the loop's next iteration.
    #[must_use]
    pub fn render_notify(&self) -> &Arc<event_listener::Event> {
        &self.state.render_notify
    }

    /// Route submitted input lines to the agent driver.
    ///
    /// The sender half of the channel the mode driver receives on;
    /// once set, Enter forwards every non-empty submit through it
    /// while the local echo stays in the conversation. A send no
    /// receiver awaits is dropped — the session is ending.
    pub fn set_submit_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        self.submit_tx = Some(tx);
    }

    /// Switch the display verbosity.
    ///
    /// Invalidates the conversation and live caches so every
    /// completed tool body re-renders in the new mode on the next
    /// frame — the shapes differ across modes, so stale lines must
    /// not linger until the next real event.
    pub fn set_verbosity(&mut self, verbosity: Verbosity) {
        self.verbosity = verbosity;
        self.conversation_generation = self.conversation_generation.saturating_add(1);
        self.live_stamp = 0;
    }

    /// Advance to the next verbosity mode, wrapping around.
    ///
    /// F2's handler: one press moves one step along Quiet → Normal
    /// → Verbose → Quiet, each press taking effect on the next
    /// frame through [`set_verbosity`](Self::set_verbosity)'s cache
    /// invalidation.
    fn cycle_verbosity(&mut self) {
        let next = match self.verbosity {
            Verbosity::Quiet => Verbosity::Normal,
            Verbosity::Normal => Verbosity::Verbose,
            Verbosity::Verbose => Verbosity::Quiet,
        };
        self.set_verbosity(next);
    }

    /// Install the callback fired when a turn ends.
    ///
    /// The hook receives the conversation snapshot at the moment a
    /// completed reply or a surfaced failure joins it — the natural
    /// save point for a session transcript. It runs inline on the
    /// render task, so heavy work must move itself off-thread.
    pub fn set_turn_end_hook(&mut self, hook: TurnEndHook) {
        self.turn_end_hook = Some(hook);
    }

    /// Run the UI event loop until the user exits.
    ///
    /// Selects over terminal events, the shared notify (the path
    /// background state uses to request a redraw), and a periodic
    /// tick. A notify listener stays registered across draws and
    /// event handling, so a notification sent while the loop is busy
    /// is delivered on the next iteration rather than lost.
    /// Background redraws are frame-capped: a notify renders at most
    /// once per frame interval, a request arriving sooner parks
    /// itself for the tick to claim, and any queued graduation
    /// forces the frame. Input-driven redraws are not capped. The
    /// tick redraws while tool calls are in flight (keeping their
    /// elapsed stamps live), claims parked background requests, and
    /// otherwise draws nothing. The caller owns terminal setup and
    /// teardown.
    ///
    /// # Errors
    /// Propagates terminal draw failures; the caller's guard still
    /// restores the terminal.
    pub async fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.quitting = false;
        let mut events = TerminalEvents::spawn();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let notify = Arc::clone(&self.state.render_notify);
        let mut listener = notify.listen();

        terminal.draw(|frame| self.render(frame))?;

        while !self.quitting {
            let needs_redraw = tokio::select! {
                maybe_event = events.recv() => {
                    match maybe_event {
                        Some(result) => match result {
                            Ok(event) => {
                                let mut handled = self.handle_event(&event);
                                handled |= self.drain_ready(|| events.poll())?;
                                handled
                            }
                            Err(err) => return Err(err.into()),
                        },
                        None => return Ok(()),
                    }
                }
                () = &mut listener => self.notify_wake(&notify, &mut listener),
                _instant = tick.tick() => self.tick_wake(Instant::now()),
            };
            if needs_redraw {
                terminal.draw(|frame| self.render(frame))?;
            }
        }
        Ok(())
    }

    /// Handle the events already waiting behind the first one.
    ///
    /// A touchpad's wheel momentum and a fast typist both deliver
    /// bursts far faster than a frame; handling one event per loop
    /// iteration makes each wait its own full render, so a
    /// direction reversal queues behind the backlog and the display
    /// keeps scrolling the old way. Draining the ready events first
    /// applies the whole burst to the state — the offsets simply
    /// accumulate — and the caller draws once afterwards. The drain
    /// stops at a quit event, so nothing queued behind the user's
    /// Esc — an Enter, a send, an echo — is applied after the
    /// decision to leave. A read failure from the source propagates
    /// out instead of masquerading as a clean exit.
    ///
    /// Returns whether anything in the burst requires a redraw.
    ///
    /// # Errors
    ///
    /// Propagates a terminal read failure delivered by the source;
    /// the caller surfaces it as a session error rather than
    /// exiting silently.
    pub fn drain_ready<F>(&mut self, mut next: F) -> Result<bool, std::io::Error>
    where
        F: FnMut() -> Option<Result<Event, std::io::Error>>,
    {
        let mut needs_redraw = false;
        while !self.quitting
            && let Some(result) = next()
        {
            let event = result?;
            needs_redraw |= self.handle_event(&event);
        }
        Ok(needs_redraw)
    }

    /// Apply one terminal event to the app state.
    ///
    /// Returns whether the event requires a redraw. Key releases and
    /// repeats are ignored so a held key fires once per press. Mouse
    /// events other than the wheel are ignored.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    return false;
                }
                self.handle_key(*key)
            }
            Event::Mouse(mouse) => self.handle_mouse(*mouse),
            Event::Paste(text) => {
                self.input.insert_str(text);
                true
            }
            Event::Resize(_, _) => true,
            _ => false,
        }
    }

    /// Apply one key press: quit and page-scroll keys stay app-level,
    /// everything else belongs to the input editor.
    ///
    /// Up, Down, and End fall back to the transcript while the input
    /// sits empty with no history to recall — a fresh session's
    /// arrows still scroll the conversation — and join the editor as
    /// soon as anything is typed or recallable.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
                self.quitting = true;
                true
            }
            (KeyCode::F(2), _) => {
                self.cycle_verbosity();
                true
            }
            (KeyCode::PageUp, _) => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                self.auto_scroll = false;
                true
            }
            (KeyCode::PageDown, _) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                self.rearm_if_near_bottom();
                true
            }
            (KeyCode::Up, KeyModifiers::NONE)
                if self.input.is_empty() && !self.input.has_history() =>
            {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.auto_scroll = false;
                true
            }
            (KeyCode::Down, KeyModifiers::NONE)
                if self.input.is_empty() && !self.input.has_history() =>
            {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                self.rearm_if_near_bottom();
                true
            }
            (KeyCode::End, _) if self.input.is_empty() => {
                self.scroll_to_bottom();
                true
            }
            _ => {
                let action = self.input.handle_key(key);
                self.apply_input_action(action)
            }
        }
    }

    /// Apply one mouse event.
    ///
    /// The wheel scrolls by [`WHEEL_SCROLL_LINES`]; anything else is
    /// ignored.
    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(WHEEL_SCROLL_LINES);
                self.auto_scroll = false;
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(WHEEL_SCROLL_LINES);
                self.rearm_if_near_bottom();
                true
            }
            _ => false,
        }
    }

    /// Route an editor action.
    ///
    /// A submit echoes, enqueues, and re-anchors; the rest need
    /// nothing beyond the redraw the caller grants.
    fn apply_input_action(&mut self, action: InputAction) -> bool {
        match action {
            InputAction::Submit(text) => {
                self.submit_text(text);
                true
            }
            InputAction::Redraw | InputAction::None => true,
        }
    }

    /// Send one submitted text to the agent driver and echo it.
    ///
    /// The channel send (when a driver is attached) counts toward
    /// the queued indicator the input title renders; the local echo
    /// lands immediately and the view re-anchors to the newest line.
    fn submit_text(&mut self, text: String) {
        if let Some(tx) = &self.submit_tx {
            self.state.queued.fetch_add(1, Ordering::SeqCst);
            if tx.send(text.clone()).is_err() {
                self.state.queued.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.conversation.push(TuiMessage::User {
            text,
            timestamp: chrono::Utc::now(),
        });
        self.conversation_generation = self.conversation_generation.saturating_add(1);
        self.scroll_to_bottom();
    }

    /// Whether any tool call is in flight.
    ///
    /// The tick consults this to keep in-flight elapsed stamps live;
    /// a poisoned lock reads as none, the same policy the tool
    /// indicator rows apply.
    fn any_tools_running(&self) -> bool {
        self.state
            .active_tools
            .lock()
            .is_ok_and(|tools| !tools.is_empty())
    }

    /// Jump the view back to the bottom and re-arm stickiness.
    ///
    /// Submits call this so freshly appended output is visible and
    /// followed; the End key shares the path.
    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = true;
    }

    /// Re-arm stickiness when a scroll lands near the bottom.
    ///
    /// Scroll-down actions within [`STICK_TOLERANCE`] lines of the
    /// newest content count as "back at the bottom"; the next layout
    /// snaps fully.
    fn rearm_if_near_bottom(&mut self) {
        if self.scroll_offset <= STICK_TOLERANCE {
            self.auto_scroll = true;
        }
    }

    /// Decide whether a background-requested redraw may run now.
    ///
    /// A queued graduation always forces the frame — the graduated
    /// message must not wait out the cap. Otherwise a
    /// frame renders at most once per frame interval; a request
    /// arriving sooner parks itself in the pending flag for the
    /// periodic tick to claim. The caller supplies the frame clock.
    pub fn redraw_due(&mut self, now: Instant) -> bool {
        if self.graduation_waiting() {
            self.render_pending = false;
            self.last_frame = Some(now);
            return true;
        }
        let due = self
            .last_frame
            .is_none_or(|last| now.duration_since(last) >= FRAME_INTERVAL);
        if due {
            self.last_frame = Some(now);
            self.render_pending = false;
        } else {
            self.render_pending = true;
        }
        due
    }

    /// Claim a parked redraw for the periodic tick.
    ///
    /// Returns whether a capped background request was waiting;
    /// claiming stamps the frame time so the interval restarts. The
    /// claim does not itself re-check the interval — a parked
    /// request is owed its frame whenever the tick lands, which can
    /// sit inside the interval after a notify-initiated frame; the
    /// tick is spaced far wider than the interval, so the reading is
    /// deliberate. Ticks with nothing pending and no tools in
    /// flight still draw nothing.
    pub fn take_pending_redraw(&mut self, now: Instant) -> bool {
        if !self.render_pending {
            return false;
        }
        self.render_pending = false;
        self.last_frame = Some(now);
        true
    }

    /// Handle a wake from the shared notify.
    ///
    /// Re-registers the listener before gating the redraw, so a
    /// notification raised while this wake is being handled finds a
    /// listener waiting and is delivered on the next iteration
    /// rather than lost. Returns whether the redraw may run.
    pub fn notify_wake(
        &mut self,
        notify: &event_listener::Event,
        listener: &mut event_listener::EventListener,
    ) -> bool {
        *listener = notify.listen();
        self.redraw_due(Instant::now())
    }

    /// Handle a periodic tick.
    ///
    /// Claims any parked background redraw first, advances the
    /// spinner one frame while tools run, then redraws — the tick
    /// itself is the animation beat. An idle tick with nothing
    /// parked and no tools in flight draws nothing.
    pub fn tick_wake(&mut self, now: Instant) -> bool {
        let claimed = self.take_pending_redraw(now);
        if self.any_tools_running() {
            let next = self.spinner_idx.saturating_add(1);
            self.spinner_idx = if next >= SPINNER_FRAMES.len() {
                0
            } else {
                next
            };
        }
        self.any_tools_running() || claimed
    }

    /// Whether a graduation is waiting to land.
    ///
    /// A poisoned lock recovers — the same policy the drain applies
    /// — so a waiting event still forces its frame after another
    /// thread's panic.
    fn graduation_waiting(&self) -> bool {
        let graduations = self
            .state
            .graduations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        !graduations.is_empty()
    }

    /// Render one frame of the three-pane layout.
    ///
    /// Conversation fills the space above the input box; the status
    /// bar closes the frame at the bottom row. Each frame first takes
    /// what the observer finished — finalized replies and completed
    /// tool calls alike graduate into the conversation. The
    /// streaming region follows the conversation:
    /// frozen blocks render from the cache, the live complete lines
    /// re-parse as markdown, and the unterminated tail renders as
    /// plaintext. A pinned view re-anchors to the newest line; a
    /// detached view's offset is compensated for layout growth and
    /// shrinkage and clamped to the document's scrollable height, so
    /// the viewport holds while content streams in below it or
    /// collapses away.
    pub fn render(&mut self, frame: &mut Frame) {
        self.drain_shared_state();
        let area = frame.area();
        let input_width = area.width.max(1).saturating_sub(2);
        let input_rows = self.input.display_rows(input_width).len();
        let input_height = u16::try_from(input_rows.saturating_add(2))
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_div(2))
            .max(3);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(area);

        let fallback = area;
        let conversation_area = pane(&chunks, 0, fallback);
        let input_area = pane(&chunks, 1, fallback);
        let status_area = pane(&chunks, 2, fallback);

        let conversation_height = conversation_area.height as usize;
        let width = conversation_area.width.max(1);
        if self.conversation_cache_generation != self.conversation_generation
            || self.conversation_cache_width != width
        {
            self.conversation_cache = self.conversation_lines(conversation_area);
            self.conversation_cache_width = width;
            self.conversation_cache_generation = self.conversation_generation;
        }
        self.refresh_streaming_region(width);
        let tools = self.active_tool_lines();
        let total_lines = self
            .conversation_cache
            .len()
            .saturating_add(self.frozen_lines.len())
            .saturating_add(self.live_lines.len())
            .saturating_add(tools.len());
        if self.auto_scroll {
            self.scroll_offset = 0;
        } else {
            let delta = total_lines.abs_diff(self.last_layout_lines);
            if total_lines >= self.last_layout_lines {
                self.scroll_offset = self.scroll_offset.saturating_add(delta);
            } else {
                self.scroll_offset = self.scroll_offset.saturating_sub(delta);
            }
            self.scroll_offset = self
                .scroll_offset
                .min(total_lines.saturating_sub(conversation_height));
        }
        self.last_layout_lines = total_lines;
        let skip = total_lines
            .saturating_sub(conversation_height)
            .saturating_sub(self.scroll_offset);
        let visible = visible_window(
            [
                &self.conversation_cache,
                &self.frozen_lines,
                &self.live_lines,
                &tools,
            ],
            skip,
            conversation_height,
        );
        let visible_len = visible.len();

        frame.render_widget(Paragraph::new(visible), conversation_area);
        if total_lines > conversation_height && !conversation_area.is_empty() {
            let mut scrollbar_state = ScrollbarState::new(total_lines)
                .position(skip)
                .viewport_content_length(visible_len);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                conversation_area,
                &mut scrollbar_state,
            );
        }

        self.render_input(frame, input_area);
        self.render_status_bar(frame, status_area);
    }

    /// Take what the observer finished since the last frame.
    ///
    /// Finalized replies and completed tool calls alike move into
    /// the conversation as assistant messages; run failures
    /// recorded by the mode driver surface as error messages — so
    /// none of the shared buffers accumulates across frames.
    /// Whenever a reply, a tool completion, or a failure lands, the
    /// turn-end hook — when installed — receives the conversation
    /// snapshot, the host's save point. A poisoned lock is
    /// recovered — the same policy the observer writes with — so
    /// finalized data still graduates.
    fn drain_shared_state(&mut self) {
        let graduations = take_locked(&self.state.graduations);
        let now = chrono::Utc::now();
        let mut turn_ended = !graduations.is_empty();
        for graduation in graduations {
            match graduation {
                Graduation::Reply(text) => {
                    self.push_message(TuiMessage::Assistant {
                        blocks: vec![ContentBlock::Text { text }],
                        timestamp: now,
                        duration_ms: None,
                    });
                }
                Graduation::Tool(result) => {
                    self.push_message(TuiMessage::Assistant {
                        blocks: vec![ContentBlock::Tool {
                            name: result.name,
                            input_preview: result.input_summary,
                            success: !result.is_error,
                            elapsed_secs: result.duration.as_secs_f64(),
                            output_preview: result.output_preview,
                        }],
                        timestamp: now,
                        duration_ms: None,
                    });
                }
            }
        }
        let errors = take_locked(&self.state.errors);
        turn_ended |= !errors.is_empty();
        for text in errors {
            self.push_message(TuiMessage::Error {
                text,
                timestamp: now,
            });
        }
        if turn_ended && let Some(hook) = &self.turn_end_hook {
            hook(&self.conversation);
        }
    }

    /// Flatten the conversation into styled lines for the given width.
    ///
    /// Assistant text goes through the markdown pipeline with the
    /// assistant base color; user and system messages render as
    /// single styled lines; error messages wrap as styled plaintext
    /// at the pane width, so a long failure body stays readable
    /// instead of clipping at the right edge; a completed tool
    /// block renders its verbosity-shaped summary line(s) between
    /// the text blocks around it. The live region (streaming text,
    /// in-flight tools) is assembled by the caller.
    fn conversation_lines(&self, area: Rect) -> Vec<Line<'static>> {
        let width = area.width.max(1);
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for message in &self.conversation {
            match message {
                TuiMessage::User { text, .. } => {
                    for segment in text.split('\n') {
                        lines.push(Line::styled(
                            segment.to_string(),
                            Style::default().fg(self.theme.ui.user_message_fg),
                        ));
                    }
                }
                TuiMessage::Assistant { blocks, .. } => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                lines.extend(markdown::render_markdown(
                                    text,
                                    width,
                                    &markdown_theme,
                                    &syntax_theme,
                                    self.theme.ui.assistant_message_fg,
                                    None,
                                ));
                            }
                            ContentBlock::Tool {
                                name,
                                input_preview,
                                success,
                                elapsed_secs,
                                ..
                            } => {
                                let elapsed = if elapsed_secs.is_finite()
                                    && *elapsed_secs >= 0.0
                                    && *elapsed_secs <= MAX_ELAPSED_SECS
                                {
                                    std::time::Duration::from_secs_f64(*elapsed_secs)
                                } else {
                                    std::time::Duration::ZERO
                                };
                                let record = ToolResultDisplay {
                                    name: name.clone(),
                                    is_error: !*success,
                                    duration: elapsed,
                                    input_summary: input_preview.clone(),
                                    output_preview: String::new(),
                                };
                                lines.extend(crate::tool_render::completed_tool_lines(
                                    &record,
                                    &self.theme,
                                    self.verbosity,
                                ));
                            }
                        }
                    }
                }
                TuiMessage::System { text, .. } => {
                    for segment in text.split('\n') {
                        lines.push(Line::styled(
                            segment.to_string(),
                            Style::default().fg(self.theme.ui.dim),
                        ));
                    }
                }
                TuiMessage::Error { text, .. } => {
                    lines.extend(plain_wrapped_lines(
                        text,
                        usize::from(width),
                        self.theme.ui.status_error,
                    ));
                }
            }
        }
        lines
    }

    /// The streaming region's live rows: everything after the
    /// frozen prefix.
    ///
    /// Three parts, assembled in order: the cached lines of frozen
    /// blocks (settled content, rendered once when its terminating
    /// boundary arrived — after a blank run, or after a top-level
    /// fence closes); the live segment — every complete line since
    /// the freeze, re-parsed as markdown so each block renders as
    /// soon as its lines complete (an open code fence is closed
    /// synthetically, so in-flight code renders as the framed block
    /// instead of a wrapped paragraph; a block longer than
    /// [`LIVE_PARSE_LINE_CAP`] lines or [`LIVE_PARSE_BYTE_CAP`]
    /// bytes keeps only its trailing cap as markdown and its head
    /// renders as plaintext until it freezes, bounding the
    /// per-frame parse); and the unterminated tail — the last
    /// line of what the freeze has not yet consumed, as
    /// width-wrapped plaintext, so a closer the frozen block
    /// already rendered never draws twice while growth after it
    /// appears as it arrives. An empty buffer renders nothing; a
    /// poisoned lock is recovered —
    /// the same policy the drain applies — so the live view keeps
    /// rendering after another thread's panic.
    fn refresh_streaming_region(&mut self, width: u16) {
        let buffer = self
            .state
            .streaming_text
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if buffer.is_empty() {
            self.reset_stream_cache();
            return;
        }
        if self.stream_cache_width != width {
            self.reset_stream_cache();
            self.stream_cache_width = width;
        }
        if self.frozen_upto > 0
            && buffer
                .get(..self.frozen_upto)
                .is_none_or(|prefix| fingerprint(prefix) != self.frozen_fingerprint)
        {
            self.reset_stream_cache();
        }
        self.advance_freeze(&buffer, width);

        let unfrozen = buffer.get(self.frozen_upto..).unwrap_or("");
        let live = unfrozen
            .rsplit_once('\n')
            .map_or(unfrozen, |(_, tail)| tail);
        let live_md = unfrozen
            .get(..unfrozen.len().saturating_sub(live.len()))
            .filter(|segment| !segment.is_empty());
        let stamp = live_md.map_or(0, fingerprint) ^ {
            let mut seed = live.len() as u64;
            for ch in live.chars().rev().take(64) {
                seed = seed.wrapping_mul(31).wrapping_add(u64::from(u32::from(ch)));
            }
            seed
        };
        if self.live_stamp == stamp
            && self.live_frozen_upto == self.frozen_upto
            && self.live_width == width
        {
            return;
        }

        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let base = self.theme.ui.assistant_message_fg;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let leading = live_md.map_or(0, |segment| split_blank_prefix(segment).0);
        if !self.frozen_lines.is_empty() && (live_md.is_some() || !live.is_empty()) {
            let gap = self.frozen_separators.max(leading);
            for _ in 0..gap {
                lines.push(Line::from(""));
            }
        }
        if let Some(segment) = live_md {
            let (_, core) = split_blank_prefix(segment);
            let (line_head, recent) = split_live_segment(core, LIVE_PARSE_LINE_CAP);
            let (older, recent) = bound_recent_bytes(line_head, recent, LIVE_PARSE_BYTE_CAP);
            let md_at_buffer_start = self.frozen_upto == 0 && leading == 0 && older.is_empty();
            if !older.is_empty() {
                lines.extend(plain_wrapped_lines(&older, usize::from(width), base));
            }
            let mut source = String::new();
            if !md_at_buffer_start {
                source.push('\n');
            }
            if let Some(marker) = open_fence_closer(&older) {
                source.push_str(marker);
                source.push('\n');
                source.push_str(recent);
                source.push('\n');
                source.push_str(marker);
            } else {
                source.push_str(recent);
                if let Some(marker) = open_fence_closer(recent) {
                    source.push('\n');
                    source.push_str(marker);
                }
            }
            lines.extend(markdown::render_markdown(
                &source,
                width,
                &markdown_theme,
                &syntax_theme,
                base,
                None,
            ));
        }
        if !live.is_empty() {
            lines.extend(plain_wrapped_lines(live, usize::from(width), base));
        }
        self.live_stamp = stamp;
        self.live_frozen_upto = self.frozen_upto;
        self.live_width = width;
        self.live_lines = lines;
    }

    /// Reset the streaming cache.
    ///
    /// Drops the frozen lines, offset, and separator count; the next
    /// layout re-freezes from the buffer's start. Called when the
    /// buffer empties (the reply graduated or the turn failed), its
    /// frozen prefix stops matching, or the pane width changes.
    fn reset_stream_cache(&mut self) {
        self.frozen_lines.clear();
        self.frozen_upto = 0;
        self.frozen_separators = 0;
        self.frozen_fingerprint = 0;
        self.live_stamp = 0;
        self.live_frozen_upto = 0;
        self.live_width = 0;
        self.live_lines.clear();
    }

    /// Freeze every newly settled block into the cache.
    ///
    /// Extends [`frozen_upto`](Self::frozen_upto) to the last safe
    /// boundary in the buffer and appends the newly frozen slice's
    /// rendered lines. The slice's leading blank run belongs to the
    /// join gap, not the slice's own render: the gap rows pushed
    /// before the lines are the larger of the pending separator
    /// count and that run, so a boundary taken at a closed fence
    /// and a following blank run never double-count. When no
    /// visible line is cached yet the run is the document-leading
    /// one and the batch's absorb-one rule applies instead. A safe
    /// boundary sits at the start of the line after a blank line or
    /// after a top-level fence closes, outside any code fence, where
    /// the blocks on either side cannot continue each other — so the
    /// slice parses standalone exactly as it parses in place.
    fn advance_freeze(&mut self, buffer: &str, width: u16) {
        let boundary = last_safe_boundary(buffer, self.frozen_upto);
        if boundary <= self.frozen_upto {
            return;
        }
        let Some(slice) = buffer.get(self.frozen_upto..boundary) else {
            return;
        };
        let (leading, core) = split_blank_prefix(slice);
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let rendered = markdown::render_markdown(
            core,
            width,
            &markdown_theme,
            &syntax_theme,
            self.theme.ui.assistant_message_fg,
            None,
        );
        let gap = if self.frozen_lines.is_empty() {
            self.frozen_separators.saturating_sub(1)
        } else {
            self.frozen_separators.max(leading)
        };
        for _ in 0..gap {
            self.frozen_lines.push(Line::from(""));
        }
        self.frozen_lines.extend(rendered);
        self.frozen_upto = boundary;
        self.frozen_separators = if core.is_empty() && !self.frozen_lines.is_empty() {
            0
        } else {
            trailing_blank_lines(slice).max(1)
        };
        if let Some(prefix) = buffer.get(..boundary) {
            self.frozen_fingerprint = fingerprint(prefix);
        }
    }

    /// The in-flight tool indicator lines.
    ///
    /// One dim line per dispatched call; a poisoned lock renders
    /// none, the same policy the status bar applies.
    fn active_tool_lines(&self) -> Vec<Line<'static>> {
        let tools = self
            .state
            .active_tools
            .lock()
            .map_or_else(|_| Vec::new(), |tools| tools.clone());
        tools
            .iter()
            .flat_map(|tool| {
                crate::tool_render::running_tool_lines(
                    tool,
                    self.spinner_idx,
                    &self.theme,
                    self.verbosity,
                )
            })
            .collect()
    }

    /// Render the input box: wrapped editor rows, the newline hint,
    /// the queued indicator, and the terminal cursor's cell.
    ///
    /// The block title always teaches the two newline gestures — the
    /// hint is the discovery path on terminals where Shift+Enter
    /// arrives as a plain Enter — and prefixes the queued-submission
    /// count while the driver has unclaimed sends. The caret sits
    /// one cell inside the border, offset by the cursor's display
    /// column and wrapped row.
    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.max(1).saturating_sub(2);
        let rows = self.input.display_rows(width);
        let queued = self.state.queued.load(Ordering::SeqCst);
        let title = if queued > 0 {
            format!(" ⏳ {queued} queued · ⏎ enter · shift+enter or \\+enter for newline ")
        } else {
            " ⏎ enter · shift+enter or \\+enter for newline ".to_string()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.ui.input_border))
            .title(title);
        let inner = block.inner(area);
        let lines: Vec<Line<'_>> = rows
            .iter()
            .map(|row| Line::styled(row.as_str(), Style::default().fg(self.theme.ui.input_text)))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        frame.render_widget(block, area);

        let (row, column) = self.input.cursor_cell(width).unwrap_or((0, 0));
        let caret_x = inner
            .x
            .saturating_add(column)
            .min(inner.right().saturating_sub(1));
        let caret_y = inner
            .y
            .saturating_add(row)
            .min(inner.bottom().saturating_sub(1));
        frame.set_cursor_position((caret_x, caret_y));
    }

    /// Render the one-line status bar.
    ///
    /// Names the configured model on the left and the cumulative
    /// token totals on the right; both sit on the themed bar colors.
    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let tokens = self.state.tokens.lock().map_or_else(
            |_| 0,
            |counts| {
                counts
                    .cumulative_input
                    .saturating_add(counts.cumulative_output)
            },
        );
        let status_text = format!(" {}  │  {tokens} tok", self.config.api.model);
        frame.render_widget(
            Paragraph::new(status_text).style(
                Style::default()
                    .fg(self.theme.ui.status_bar_fg)
                    .bg(self.theme.ui.status_bar_bg),
            ),
            area,
        );
    }
}

/// Take a shared buffer's contents, leaving it empty.
///
/// Recovers from poisoning — the same policy the observer writes
/// with — so finalized data still graduates after another thread's
/// panic left the lock poisoned.
fn take_locked<T: Default>(mutex: &Mutex<T>) -> T {
    std::mem::take(&mut *mutex.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Collect the viewport window across concatenated line segments.
///
/// The settled conversation renders from a cache that must survive
/// the frame, so instead of concatenating and cloning every segment
/// this walks them with the skip offset and clones only the lines
/// the viewport actually shows — per-frame cost tracks the pane
/// height, not the session length.
fn visible_window<'a>(segments: [&'a [Line<'a>]; 4], skip: usize, height: usize) -> Vec<Line<'a>> {
    let mut visible = Vec::with_capacity(height.min(64));
    let mut skip = skip;
    for segment in segments {
        if skip >= segment.len() {
            skip = skip.saturating_sub(segment.len());
            continue;
        }
        let take = segment
            .len()
            .saturating_sub(skip)
            .min(height.saturating_sub(visible.len()));
        if let Some(window) = segment.get(skip..skip.saturating_add(take)) {
            visible.extend(window.iter().cloned());
        }
        skip = 0;
        if visible.len() >= height {
            break;
        }
    }
    visible
}

/// Fetch a layout pane by index with a whole-area fallback.
///
/// The three-pane layout always splits into three rects; the
/// fallback keeps rendering total if a shorter split ever appears.
fn pane(chunks: &[Rect], index: usize, fallback: Rect) -> Rect {
    chunks.get(index).copied().unwrap_or(fallback)
}

/// How a non-blank line can continue a block across a blank line.
///
/// Only lists and quotes do in the grammar: a blank inside a loose
/// list or a multi-paragraph quote belongs to the block, so the
/// freeze scanner must not settle content there.
#[derive(Clone, Copy, PartialEq)]
enum ContLine {
    /// A `-`/`*`/numbered item line.
    List,
    /// A `>`-prefixed quote line.
    Quote,
    /// Anything else.
    Other,
}

/// Classify a non-blank line's cross-blank continuation kind.
fn cont_line_kind(line: &str) -> ContLine {
    let trimmed = line.trim_start_matches(' ');
    if trimmed.starts_with('>') {
        return ContLine::Quote;
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    let list = trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || (digits > 0
            && trimmed
                .get(digits..digits.saturating_add(2))
                .is_some_and(|marker| marker == ". "));
    if list {
        ContLine::List
    } else {
        ContLine::Other
    }
}

/// The fence marker a line opens or closes, if it is a fence line.
///
/// Leading spaces and trailing line ends are ignored, matching the
/// grammar's `" "*` around fence markers.
fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start_matches(' ');
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// The closing fence a segment still needs, if it ends inside one.
///
/// Tracks fence opens and closes line by line; a fence left open at
/// the segment's end reports its marker so the caller can close it
/// synthetically before parsing, letting in-flight code render as
/// the framed block.
fn open_fence_closer(segment: &str) -> Option<&'static str> {
    let mut open: Option<&'static str> = None;
    for line in segment.split_inclusive('\n') {
        let Some(marker) = fence_marker(line.trim_end_matches(['\n', '\r'])) else {
            continue;
        };
        if open == Some(marker) {
            open = None;
        } else if open.is_none() {
            open = Some(marker);
        }
    }
    open
}

/// The last offset at or after `from` where the buffer's earlier
/// content is settled.
///
/// A safe boundary is the start of a non-blank line that follows a
/// blank line — found outside any code fence, where the non-blank
/// lines flanking the blank cannot continue the same block — or the
/// line after a top-level fence closes: a closed fence cannot be
/// continued, so everything through its closer is settled even
/// without a following blank. A fence opened while a list or quote
/// was in progress is list-nested and gets no close-boundary; the
/// surrounding block may continue past it. Returns `from` when no
/// later boundary exists.
fn last_safe_boundary(buffer: &str, from: usize) -> usize {
    let tail = buffer.get(from..).unwrap_or("");
    let mut boundary = from;
    let mut offset = from;
    let mut fence: Option<&'static str> = None;
    let mut kind_before_fence = ContLine::Other;
    let mut prev_kind = ContLine::Other;
    let mut pending_blank = false;
    for line in tail.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if let Some(marker) = fence_marker(text) {
            if fence == Some(marker) {
                fence = None;
                if kind_before_fence == ContLine::Other {
                    boundary = offset.saturating_add(line.len());
                }
            } else if fence.is_none() {
                fence = Some(marker);
                kind_before_fence = prev_kind;
            }
            prev_kind = ContLine::Other;
            pending_blank = false;
        } else if fence.is_some() || text.trim().is_empty() {
            if fence.is_none() {
                pending_blank = true;
            }
        } else {
            let kind = cont_line_kind(text);
            if pending_blank
                && !matches!(
                    (prev_kind, kind),
                    (ContLine::List, ContLine::List) | (ContLine::Quote, ContLine::Quote)
                )
            {
                boundary = offset;
            }
            prev_kind = kind;
            pending_blank = false;
        }
        offset = offset.saturating_add(line.len());
    }
    boundary
}

/// Bound the markdown part by bytes as well as lines.
///
/// A single line can carry an entire minified block, so the line cap
/// alone leaves the parse unbounded; past the byte cap the excess
/// moves into the plaintext head, cut on a character boundary so the
/// markdown part always starts on one.
fn bound_recent_bytes<'a>(older: &str, recent: &'a str, cap: usize) -> (String, &'a str) {
    if recent.len() <= cap {
        return (older.to_string(), recent);
    }
    let mut cut = recent.len().saturating_sub(cap);
    while !recent.is_char_boundary(cut) {
        cut = cut.saturating_add(1);
    }
    let mut head = String::from(older);
    head.push_str(recent.get(..cut).unwrap_or(""));
    (head, recent.get(cut..).unwrap_or(""))
}

/// Split a slice's leading blank run from its content.
///
/// Returns the run's line count and the remainder. A boundary taken
/// at a closed fence leaves the following blank run at the head of
/// the next slice; that run belongs to the join gap rather than the
/// slice's own render, so the caller folds it into the gap rows.
fn split_blank_prefix(slice: &str) -> (usize, &str) {
    let mut count: usize = 0;
    let mut offset: usize = 0;
    for line in slice.split_inclusive('\n') {
        if !line.trim().is_empty() {
            break;
        }
        count = count.saturating_add(1);
        offset = offset.saturating_add(line.len());
    }
    (count, slice.get(offset..).unwrap_or(""))
}

/// Split the live segment at the parse cap.
///
/// Returns the over-cap head and the trailing cap of lines; the head
/// renders as plaintext and the tail parses as markdown, bounding
/// the per-frame parse while a single long block streams. A segment
/// at or under the cap returns an empty head.
fn split_live_segment(segment: &str, cap: usize) -> (&str, &str) {
    let cap = cap.max(1);
    let split = segment
        .rmatch_indices('\n')
        .nth(cap.saturating_sub(1))
        .map(|(index, _)| index);
    match split {
        Some(index) => {
            let head = segment.get(..index).unwrap_or("");
            let tail = segment.get(index.saturating_add(1)..).unwrap_or("");
            (head, tail)
        }
        None => ("", segment),
    }
}

/// Count the blank lines a frozen slice ends with.
///
/// The blank run before a boundary is the separator the batch render
/// shows between the blocks on either side; the cache re-inserts
/// exactly this many empty rows at the join, however many blank
/// lines the source carried.
fn trailing_blank_lines(slice: &str) -> usize {
    slice
        .lines()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count()
}

/// A cheap identity stamp for a text prefix.
///
/// Within a turn the streaming buffer only grows, but a turn that
/// cleared and refilled the buffer can reach the old frozen length
/// again; the stamp tells that replacement apart from growth where a
/// length comparison cannot.
fn fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Wrap text into styled plaintext lines at a display width.
///
/// The input renders raw: split on newlines and greedily wrapped at
/// the width on character boundaries, styled with the caller's
/// foreground. No parsing, so the cost stays linear in the input and
/// partial markup shows exactly as typed — the properties the
/// streaming tail and the error rows both rely on.
fn plain_wrapped_lines(text: &str, width: usize, fg: ratatui::style::Color) -> Vec<Line<'static>> {
    let cap = width.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for ch in raw.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if cap.saturating_sub(current_width) < ch_width && !current.is_empty() {
                lines.push(Line::styled(
                    std::mem::take(&mut current),
                    Style::default().fg(fg),
                ));
                current_width = 0;
            }
            current.push(ch);
            current_width = current_width.saturating_add(ch_width);
        }
        lines.push(Line::styled(current, Style::default().fg(fg)));
    }
    lines
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        clippy::missing_panics_doc,
        clippy::missing_errors_doc
    )]

    use super::*;

    fn app() -> TuiApp {
        TuiApp::new(dch_config::DchConfig::default())
    }

    #[test]
    fn the_tick_redraws_only_while_tools_run() {
        let app = app();
        assert!(!app.any_tools_running(), "no tools in flight yet");

        app.active_tools()
            .lock()
            .expect("the tools lock")
            .push(ActiveTool {
                call_id: String::new(),
                name: "Grep".to_string(),
                input_summary: "\"needle\"".to_string(),
                start: std::time::Instant::now(),
            });
        assert!(app.any_tools_running(), "a running tool asks for redraws");

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = app.active_tools().lock().expect("the tools lock");
            panic!("poison the tools lock");
        }));
        assert!(panicked.is_err(), "the poisoning panic must unwind");
        assert!(
            !app.any_tools_running(),
            "a poisoned lock reads as no tools in flight"
        );
    }
}
