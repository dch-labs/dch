//! The terminal application shell.
//!
//! `TuiApp` owns the input buffer, the conversation, and the shared
//! state background producers write through; `run` drives the
//! render ↔ input ↔ scroll loop until the user quits.

use std::collections::HashMap;
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
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthChar as _;
use unicode_width::UnicodeWidthStr as _;

use dch_config::DchConfig;

use crate::Graduation;
use crate::events::TerminalEvents;
use crate::input::{InputAction, InputEditor};
use crate::markdown;
use crate::message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
use crate::observer::{ToolResultDisplay, TuiObserverState};
use crate::permission::PermissionRequest;
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

/// The periodic beat the run loop wakes on.
///
/// The animation cadence while tools run — the spinner advances and
/// the elapsed stamps move one display step (a tenth of a second)
/// per tick, matching the stamps' finest shown digit so they march
/// uniformly whether or not input is redrawing — and the scroll beat
/// a drag parked at the conversation pane's edge and the composer
/// caret's blink phase step with. An idle tick between events draws
/// nothing: it claims a parked background redraw, and neither that,
/// a drag, nor a blink flip being due, the loop sleeps on.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Ticks per caret blink phase.
///
/// Half a second on, half a second off — the cadence editors settle
/// on when they own the blink themselves. The terminal's own
/// blinking-cursor request may be ignored or preference-gated, so
/// the app toggles visibility on its tick instead: what the user
/// sees does not depend on the terminal's cooperation.
const CARET_BLINK_TICKS: u32 = 5;

/// How many graduated calls keep their expansion data.
///
/// The most recent calls stay expandable; older ones fold back to
/// plain summary rows once this many newer calls have graduated.
/// The retained set is bounded the way the capture store is, in
/// FIFO order rather than a wholesale drop — the newest tool
/// blocks, the ones a reader is still working through, are the
/// ones that stay openable.
const TOOL_DETAIL_CAP: usize = 256;

/// How long a transient notice holds the row above the composer.
///
/// Long enough to be read at a glance, short enough that the row
/// reads as empty by default.
const NOTICE_HOLD: Duration = Duration::from_secs(4);

/// How close to the newest line a scroll action must land to count
/// as "back at the bottom".
///
/// The streaming region grows between a scroll and the next layout,
/// so an exact offset-zero check would detach a view that only fell
/// behind by growth.
const STICK_TOLERANCE: usize = 2;

/// Lines one wheel or touchpad scroll event moves.
///
/// One line per wheel event: a burst of events still accumulates
/// into fast travel, while a single notch lands the smallest useful
/// step.
const WHEEL_SCROLL_LINES: usize = 1;

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

    /// The column count the composer last rendered at.
    ///
    /// The editor's vertical keys navigate the wrap grid this width
    /// defines; the event loop runs between frames, so the last
    /// rendered width is the geometry the user is looking at when a
    /// key lands.
    input_wrap_width: u16,

    /// What the last frame's composer pane looked like.
    ///
    /// A press inside the pane places the composer's caret where it
    /// landed — the pane rect to claim the press, the interior cell
    /// where the wrap grid begins, and the window of wrap rows the
    /// pane was showing when the user was looking at it.
    input_view: Option<InputView>,

    /// The composer caret's blink state.
    ///
    /// The app owns the caret's blink — the terminal's own
    /// blinking-cursor request is too often ignored or
    /// preference-gated to rely on — and this is the phase the frame
    /// renders from: the caret parks on its cell while visible and
    /// nowhere while dark. [`CaretBlink`] carries the cadence and the
    /// input-reset contract.
    caret_blink: CaretBlink,

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

    /// The mouse selection over the transcript, if any.
    ///
    /// Both ends are display cells — conversation line index plus
    /// column — in the coordinate space of the rendered lines, so
    /// the selection is character-granular and holds still while the
    /// view scrolls. It survives the release — the copy happens then,
    /// the highlight stays up — until the next press replaces it; a
    /// press-release without travel clears it.
    selection: Option<CellSelection>,

    /// The previous frame's conversation viewport.
    ///
    /// Mouse events arrive between frames carrying screen cells;
    /// this maps them onto conversation lines using the geometry of
    /// the frame the user is looking at.
    last_view: Option<ViewState>,

    /// The screen cell the drag last reported, while the button is
    /// held.
    ///
    /// Set between a press inside the conversation pane and its
    /// release. A terminal reports a drag only while the pointer
    /// moves, so a pointer pushed against the pane's edge goes quiet
    /// — the periodic tick reads this to keep scrolling there, one
    /// line per beat, until the release arrives or the document runs
    /// out. A release delivered outside the terminal's view never
    /// arrives as an event; buttonless motion (only sent with no
    /// button held) ends the drag instead, so a lost release cannot
    /// run the scroll for long.
    drag_position: Option<(u16, u16)>,

    /// Where a completed selection's text goes.
    ///
    /// The default copies through OSC 52 and, where present,
    /// `pbcopy`; tests install a recorder to observe the copy
    /// without touching any clipboard.
    copier: Box<dyn Fn(&str)>,

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

    /// The quit lifecycle.
    ///
    /// Exit is a deliberate chord, not a slip: the state carries
    /// both halves of it.
    quit: QuitState,

    /// How a Ctrl+C cancels the run in flight, when one is.
    ///
    /// Installed by the mode driver alongside the shared running
    /// flag; absent in tests and headless hosts, where a cancel
    /// press simply reports handled.
    run_canceller: Option<Box<dyn Fn()>>,

    /// Tool calls whose blocks render expanded.
    ///
    /// Keyed by call id; a click on a summary row toggles membership.
    /// Running calls share the key with their completed block, so an
    /// expansion opened while the tool runs survives its graduation.
    expanded_tools: std::collections::HashSet<String>,

    /// Full command and output per tool call, keyed by call id.
    ///
    /// Filled from the dispatch-side capture as calls graduate; a
    /// block renders its expansion from here. Runtime-only — the
    /// serialized session keeps previews, not payloads — and
    /// bounded to the most recent `TOOL_DETAIL_CAP` calls in FIFO
    /// order, so a long session's oldest blocks fold back to plain
    /// summary rows instead of retaining payloads forever.
    tool_details: HashMap<String, ToolDetail>,

    /// Graduation order of the retained tool details, oldest first.
    ///
    /// The eviction queue behind the detail cap's FIFO: the front
    /// names the entry the next graduation retires.
    tool_detail_order: std::collections::VecDeque<String>,

    /// Conversation line index to call id, for tool block rows.
    ///
    /// The click targets: every line of each completed block that
    /// has retained detail — summary and expansion alike, so a
    /// click anywhere on an open block folds it. Rebuilt with the
    /// conversation cache.
    tool_summary_lines: HashMap<usize, String>,

    /// Line index to call id, for running tool block rows.
    ///
    /// The same click targets for in-flight calls — the row and its
    /// expansion — in the line space the tools segment occupies;
    /// recorded per frame.
    tool_running_lines: HashMap<usize, String>,

    /// The application configuration this shell was built from.
    ///
    /// The status bar reads the model name from it.
    config: DchConfig,

    /// The session's id, as the status bar shows it.
    ///
    /// Set by the host once the session's identity is known — the
    /// resumed file's own id, or the runner's fresh one — so the
    /// user can read (and copy out) which session they are in.
    session_id: Option<String>,

    /// The transient notice on the row above the composer, with
    /// the instant its hold elapses.
    ///
    /// A glance-worthy event — a cancelled run — that is not a
    /// transcript row: it shows for
    /// [`NOTICE_HOLD`](self::NOTICE_HOLD) and leaves the reserved
    /// row blank again.
    transient_notice: Option<(String, Instant)>,

    /// The composer's cached wrap, if it is still fresh.
    ///
    /// Valid for one buffer mutation at one width — see
    /// [`composer_wrap`](Self::composer_wrap).
    input_wrap_cache: Option<InputWrapCache>,

    /// Permission asks waiting for the user's answer, oldest first.
    ///
    /// The gate resolves `Ask` cells by handing the UI one request per
    /// pending tool call; the front of the queue is what the overlay
    /// shows and what a keypress answers. A finished or cancelled run
    /// empties it — the gate has already denied anything unanswered
    /// against the cancel signal.
    pending_permissions: std::collections::VecDeque<PermissionRequest>,

    /// The channel permission requests arrive on, until `run` claims
    /// it.
    ///
    /// Installed by the host before the run loop starts; the loop's
    /// select arm moves requests into the queue, so between runs the
    /// slot simply holds the receiver.
    permission_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>>,

    /// Whether the next frame must repaint in full.
    ///
    /// Set when the terminal regains focus: some terminals skip
    /// painting while their window is hidden or occluded, so frames
    /// drawn during that time leave stale cells that the incremental
    /// diff cannot know about. The run loop claims the flag, clears
    /// the screen, and lets the next draw repaint every cell — the
    /// same full repaint a resize forces.
    full_repaint_pending: bool,
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
            input_wrap_width: 80,
            input_view: None,
            caret_blink: CaretBlink::default(),
            scroll_offset: 0,
            auto_scroll: true,
            render_pending: false,
            last_frame: None,
            last_layout_lines: 0,
            selection: None,
            last_view: None,
            drag_position: None,
            copier: Box::new(copy_to_clipboard),
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
            quit: QuitState::default(),
            run_canceller: None,
            expanded_tools: std::collections::HashSet::new(),
            tool_details: HashMap::new(),
            tool_detail_order: std::collections::VecDeque::new(),
            tool_summary_lines: HashMap::new(),
            tool_running_lines: HashMap::new(),
            input_wrap_cache: None,
            session_id: None,
            transient_notice: None,
            pending_permissions: std::collections::VecDeque::new(),
            permission_rx: None,
            full_repaint_pending: false,
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

    /// Post a transient notice above the composer.
    ///
    /// Replaces any notice still holding — one row, one message,
    /// the newest wins.
    pub fn post_notice(&mut self, text: String) {
        let until = Instant::now()
            .checked_add(NOTICE_HOLD)
            .unwrap_or_else(Instant::now);
        self.transient_notice = Some((text, until));
    }

    /// Name the session the status bar shows.
    ///
    /// The host calls this once the session's identity settles —
    /// before the run loop starts, so the first frame already
    /// carries it.
    pub fn set_session_id(&mut self, id: String) {
        self.session_id = Some(id);
    }

    /// Install the run canceller.
    ///
    /// The mode driver's half of the Ctrl+C contract: while
    /// [`agent_running`](TuiObserverState::agent_running) is set, a
    /// Ctrl+C press on an empty composer calls this to stop the
    /// submission in flight.
    pub fn set_run_canceller(&mut self, canceller: Box<dyn Fn()>) {
        self.run_canceller = Some(canceller);
    }

    /// Install a whole prior conversation as the session's starting
    /// state.
    ///
    /// The resume path's display seeding: the restored transcript
    /// becomes the conversation in one mutation, the view stays
    /// pinned to the newest line, and the first frame renders the
    /// full history as though it had always been there. Call before
    /// the run loop starts; a mid-session call would splice history
    /// into a live conversation.
    pub fn seed_messages(&mut self, messages: Vec<TuiMessage>) {
        for message in &messages {
            let TuiMessage::Assistant { blocks, .. } = message else {
                continue;
            };
            for block in blocks {
                if let ContentBlock::Tool {
                    call_id,
                    retained_input,
                    output_preview,
                    ..
                } = block
                {
                    self.retain_tool_detail(call_id, retained_input, output_preview);
                }
            }
        }
        self.conversation.extend(messages);
        self.conversation_generation = self.conversation_generation.saturating_add(1);
        self.scroll_offset = 0;
        self.auto_scroll = true;
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

    /// Whether the composer caret is in its visible blink phase.
    ///
    /// The terminal's own cursor stands in for the caret, so this is
    /// the app-side half of the blink contract: keyboard input
    /// resolidifies it, mouse traffic does not.
    #[must_use]
    pub fn caret_visible(&self) -> bool {
        self.caret_blink.on
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
        self.quit.requested
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

    /// Install the channel permission requests arrive on.
    ///
    /// The receiver half of the bridge the host builds around the
    /// runner's permission resolver; once set, pending asks render as
    /// an overlay and the run loop claims the channel into its select.
    pub fn set_permission_requests(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        self.permission_rx = Some(rx);
    }

    /// Land one delivered request and any others already waiting
    /// behind it.
    ///
    /// The run loop's select arm hands the request its wakeup
    /// delivered plus the receiver it owns: the request enters the
    /// queue and the receiver drains, so a burst of ready asks lands
    /// together and the first frame's overlay reports the true
    /// waiting count instead of admitting them one loop iteration at
    /// a time.
    pub fn land_permission_requests(
        &mut self,
        first: PermissionRequest,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        self.pending_permissions.push_back(first);
        while let Ok(request) = rx.try_recv() {
            self.pending_permissions.push_back(request);
        }
    }

    /// Move any waiting permission requests into the queue.
    ///
    /// The test seam for staging asks without the run loop:
    /// production keeps its receiver in a local and lands bursts
    /// through [`land_permission_requests`](Self::land_permission_requests)
    /// instead — once `run` claims the channel, this slot is empty and
    /// the method is a no-op.
    pub fn poll_permission_requests(&mut self) {
        while let Some(request) = self
            .permission_rx
            .as_mut()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.pending_permissions.push_back(request);
        }
    }

    /// Answer the front permission request, if one is pending.
    ///
    /// Sends `allow` on its reply channel and drops it from the queue;
    /// a send that finds no receiver (a resolver already cancelled
    /// away) is silently fine — the gate denied that call on its own.
    fn resolve_pending_permission(&mut self, allow: bool) {
        if let Some(front) = self.pending_permissions.pop_front()
            && front.reply.send(allow).is_err()
        {
            // The resolver stopped waiting (a cancelled dispatch); the
            // gate denied that call on its own.
            tracing::trace!("permission reply channel already closed");
        }
    }

    /// Apply a key press to a pending permission ask.
    ///
    /// `Some(redraw)` when a request is pending — the overlay owns the
    /// keyboard: plain `y`/Enter allow, plain `n`/Esc deny, and every
    /// other key is swallowed so nothing reaches the composer — whose
    /// control chords would otherwise submit or edit the hidden draft
    /// (Ctrl-Enter and Ctrl-M submit, Ctrl-W deletes a word).
    /// `None` only for the two `c` chords the app machinery owns —
    /// cancel/quit and copy — so those survive under a prompt.
    /// Modifier-decorated answers (Alt+y, Alt+Enter) do not answer:
    /// the documented keys are the plain ones.
    fn permission_key(&mut self, key: KeyEvent) -> Option<bool> {
        self.pending_permissions.front()?;
        let chord = key.code == KeyCode::Char('c')
            && (key.modifiers == KeyModifiers::CONTROL
                || key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        if chord {
            return None;
        }
        let bare = key.modifiers == KeyModifiers::NONE;
        let shift = key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Char('y' | 'Y') if bare || shift => {
                self.resolve_pending_permission(true);
                Some(true)
            }
            KeyCode::Enter if bare => {
                self.resolve_pending_permission(true);
                Some(true)
            }
            KeyCode::Char('n' | 'N') if bare || shift => {
                self.resolve_pending_permission(false);
                Some(true)
            }
            KeyCode::Esc if bare => {
                self.resolve_pending_permission(false);
                Some(true)
            }
            _ => Some(false),
        }
    }

    /// Claim the pending full repaint, if one is due.
    ///
    /// One-shot: the caller (the run loop) owns acting on the claim —
    /// clearing the terminal so the next frame repaints every cell
    /// instead of diffing against a buffer the visible screen never
    /// matched.
    #[must_use]
    pub fn take_full_repaint(&mut self) -> bool {
        std::mem::take(&mut self.full_repaint_pending)
    }

    /// Replace the selection copier.
    ///
    /// The instrumentation seam for the selection pins: a recorder
    /// observes what a release copies without touching any real
    /// clipboard. Production never calls this.
    pub fn set_selection_copier(&mut self, copier: Box<dyn Fn(&str)>) {
        self.copier = copier;
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
        self.quit = QuitState::default();
        let mut events = TerminalEvents::spawn();
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        let notify = Arc::clone(&self.state.render_notify);
        let mut listener = notify.listen();
        let mut permission_rx = self.permission_rx.take();
        let mut permission_closed = false;

        terminal.draw(|frame| self.render(frame))?;

        while !self.quit.requested {
            let mut needs_redraw = tokio::select! {
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
                maybe_request = async {
                    match permission_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(request) = maybe_request {
                        match permission_rx.as_mut() {
                            Some(rx) => self.land_permission_requests(request, rx),
                            None => self.pending_permissions.push_back(request),
                        }
                        true
                    } else {
                        // The resolver side is gone (the runner was
                        // dropped); latching the arm inert keeps the
                        // closed channel from spinning the loop.
                        permission_closed = true;
                        false
                    }
                }
            };
            if permission_closed {
                permission_rx = None;
                permission_closed = false;
            }
            if self.take_full_repaint() {
                if let Err(err) = terminal.clear() {
                    tracing::warn!("full repaint clear failed: {err}");
                }
                needs_redraw = true;
            }
            if needs_redraw {
                terminal.draw(|frame| self.render(frame))?;
            }
        }
        Ok(())
    }

    /// Copy the live selection, if any.
    ///
    /// The terminal-standard copy chord. A span that covered no
    /// characters leaves the clipboard alone — the same discipline
    /// the release and the keyboard walk apply.
    fn copy_selection_now(&mut self) {
        if let Some(selection) = self.selection.as_ref() {
            let (text, covered_any) = self.selection_text(selection);
            if covered_any {
                (self.copier)(&text);
            }
        }
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
    /// confirming Ctrl+C — an Enter, a send, an echo — is applied after the
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
        while !self.quit.requested
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
    /// repeats are ignored so a held key fires once per press. While a
    /// permission ask is pending, the overlay owns the keyboard —
    /// keystrokes and pastes alike.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) => {
                self.caret_blink.resolidify();
                if key.kind != KeyEventKind::Press {
                    return false;
                }
                self.handle_key(*key)
            }
            Event::Mouse(mouse) => self.handle_mouse(*mouse),
            Event::Paste(text) => {
                if self.pending_permissions.front().is_some() {
                    // The overlay owns the keyboard; a paste behind it
                    // must not seed the composer invisibly.
                    return true;
                }
                self.caret_blink.resolidify();
                self.input.insert_str(&normalize_pasted_newlines(text));
                true
            }
            Event::Resize(_, _) => true,
            Event::FocusGained => {
                self.full_repaint_pending = true;
                true
            }
            Event::FocusLost => false,
        }
    }

    /// Apply one key press: the quit chord, copy chord, and
    /// page-scroll keys stay app-level, everything else belongs to the
    /// input editor — unless a permission ask is pending, in which
    /// case the overlay owns the keyboard first (`y`/Enter allow,
    /// `n`/Esc deny, the rest swallowed) and only the two `c` chords
    /// (cancel/quit, copy) pass through to the machinery below.
    ///
    /// Ctrl+C clears a non-empty buffer — arming nothing, so a
    /// cleared draft still takes two further presses to exit — and
    /// on an empty one the second press quits; any other key
    /// disarms, so the chord never fires from stale intent. While a
    /// run is in flight the press serves the run instead: a draft
    /// clears first, then an empty-composer press cancels the run —
    /// arming nothing, so exiting after the cancel takes its own
    /// two presses.
    /// Ctrl+Shift+C copies the live selection where the terminal
    /// reports the shift; on terminals that collapse it to a plain
    /// Ctrl+C it simply joins the chord's clearing behavior. Up,
    /// Down, and End fall back to the transcript while the input
    /// sits empty with nothing to recall — a fresh session's arrows
    /// still scroll the conversation — and join the editor as soon as
    /// anything is typed or recallable. Shift and an arrow, with a
    /// transcript selection up, moves the selection's head instead:
    /// the editor's select-by-keyboard.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        ) {
            self.quit.disarm();
        }
        if let Some(resolved) = self.permission_key(key) {
            return resolved;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), mods) if mods == KeyModifiers::CONTROL | KeyModifiers::SHIFT => {
                self.copy_selection_now();
                true
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self
                    .state
                    .agent_running
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    if !self.input.is_empty() {
                        self.input.clear();
                    } else if let Some(cancel) = &self.run_canceller {
                        cancel();
                        self.post_notice("Agent cancelled".to_string());
                    }
                } else if !self.input.is_empty() {
                    self.input.clear();
                } else if self.quit.armed {
                    self.quit.requested = true;
                } else {
                    self.quit.arm();
                }
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
            (KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right, KeyModifiers::SHIFT)
                if self.selection.is_some() =>
            {
                self.nudge_selection(key.code);
                true
            }
            _ => {
                let action = self.input.handle_key(key, self.input_wrap_width);
                self.apply_input_action(action)
            }
        }
    }

    /// Apply one mouse event.
    ///
    /// The wheel scrolls by [`WHEEL_SCROLL_LINES`]; a press-drag-
    /// release over the conversation pane selects rendered
    /// characters — inside the app — and the release quietly hands
    /// the covered text to the clipboard while the highlight stays
    /// up until the next press. A drag pushed against the pane's
    /// top or bottom row scrolls the view in the drag's direction,
    /// extending the selection — one line per movement report, and
    /// one per tick while the pointer stays parked there; a press
    /// over the composer pane places its caret where it landed.
    /// Anything else is ignored.
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
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.quit.disarm();
                if let Some(cell) = self.cell_at(mouse.column, mouse.row) {
                    self.selection = Some(CellSelection {
                        anchor: cell,
                        head: cell,
                    });
                    self.drag_position = Some((mouse.column, mouse.row));
                    true
                } else {
                    let retired = self.selection.take().is_some();
                    self.drag_position = None;
                    self.place_input_caret(mouse.column, mouse.row) || retired
                }
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if self.selection.is_none() {
                    return false;
                }
                self.drag_position = Some((mouse.column, mouse.row));
                let mut handled = false;
                if let Some(cell) = self.cell_at(mouse.column, mouse.row)
                    && let Some(selection) = self.selection.as_mut()
                {
                    selection.head = cell;
                    handled = true;
                }
                self.drag_autoscroll() || handled
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.drag_position = None;
                match self.selection.take() {
                    Some(selection) => {
                        if selection.anchor == selection.head {
                            self.toggle_tool_at(mouse.column, mouse.row);
                        } else {
                            let (text, covered_any) = self.selection_text(&selection);
                            if covered_any {
                                (self.copier)(&text);
                            }
                            self.selection = Some(selection);
                        }
                        true
                    }
                    None => self.toggle_tool_at(mouse.column, mouse.row),
                }
            }
            MouseEventKind::Moved => {
                self.drag_position = None;
                false
            }
            _ => false,
        }
    }

    /// Scroll one step while a drag sits at the pane's top or bottom
    /// row, extending the selection head to the edge's line after
    /// the step.
    ///
    /// Editors scroll the document under a selection dragged past
    /// the viewport, in the drag's direction. A terminal reports a
    /// drag only while the pointer moves, so a pointer parked at the
    /// edge goes quiet — the drag arm runs this for an immediate
    /// step per movement and the periodic tick keeps running it once
    /// per beat while the push continues. The scroll stops at the
    /// document's first or last line whatever the drag state says,
    /// and the head clamps into the selectable document, so an edge
    /// push never extends the highlight onto the tool rows. The
    /// state itself ends on the release or, when that
    /// arrived outside the terminal's view, on the first buttonless
    /// motion — so a lost release cannot carry the scroll far.
    /// Returns whether the view or the head moved.
    fn drag_autoscroll(&mut self) -> bool {
        let Some((column, row)) = self.drag_position else {
            return false;
        };
        let Some(view) = self.last_view.as_ref().copied() else {
            return false;
        };
        let height = usize::from(view.area.height);
        if height == 0 || view.total == 0 {
            return false;
        }
        let selectable_last = view.selectable.saturating_sub(1);
        let col = usize::from(column.saturating_sub(view.area.x))
            .min(usize::from(view.area.width.saturating_sub(1)));
        if row <= view.area.y {
            let head = CellPos {
                line: view.skip.saturating_sub(1).min(selectable_last),
                col,
            };
            let scrolled = view.skip > 0;
            if scrolled {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.auto_scroll = false;
            }
            let extended = self.extend_selection_head(head);
            scrolled || extended
        } else if usize::from(row).saturating_add(1) >= usize::from(view.area.bottom()) {
            let last = view
                .skip
                .saturating_add(1)
                .min(view.total.saturating_sub(height))
                .saturating_add(height)
                .saturating_sub(1)
                .min(view.total.saturating_sub(1))
                .min(selectable_last);
            let head = CellPos { line: last, col };
            let scrolled = self.scroll_offset > 0;
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
            self.rearm_if_near_bottom();
            let extended = self.extend_selection_head(head);
            scrolled || extended
        } else {
            false
        }
    }

    /// Point the selection head at `head`, reporting whether it moved.
    ///
    /// Exactly one selection is in play during a drag; with none —
    /// the button already released, say — there is nothing to extend.
    fn extend_selection_head(&mut self, head: CellPos) -> bool {
        match self.selection.as_mut() {
            Some(selection) => {
                let moved = selection.head != head;
                selection.head = head;
                moved
            }
            None => false,
        }
    }

    /// Move the selection head one step for Shift and an arrow key.
    ///
    /// The head moves and the anchor stays, so stepping away from the
    /// anchor extends the selection and stepping back over it shrinks
    /// it — the same select-by-keyboard every editor ships. Left and
    /// Right step one cell and wrap at line ends; Up and Down step
    /// one rendered line and keep the column. The viewport follows
    /// the head, and the adjusted text replaces the clipboard copy
    /// silently, exactly as a release does.
    fn nudge_selection(&mut self, direction: KeyCode) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let head = selection.head;
        let up_one = head.line.saturating_sub(1);
        let down_one = head.line.saturating_add(1);
        let nudged = match direction {
            KeyCode::Left => {
                if head.col > 0 {
                    CellPos {
                        line: head.line,
                        col: head.col.saturating_sub(1),
                    }
                } else if head.line > 0 {
                    let width = self.line_width(up_one).unwrap_or(0);
                    CellPos {
                        line: up_one,
                        col: width.saturating_sub(1),
                    }
                } else {
                    head
                }
            }
            KeyCode::Right => {
                let width = self.line_width(head.line).unwrap_or(0);
                if width > 0 && head.col.saturating_add(1) < width {
                    CellPos {
                        line: head.line,
                        col: head.col.saturating_add(1),
                    }
                } else if self.line_width(down_one).is_some() {
                    CellPos {
                        line: down_one,
                        col: 0,
                    }
                } else {
                    head
                }
            }
            KeyCode::Up => {
                if head.line > 0 {
                    CellPos {
                        line: up_one,
                        col: head.col,
                    }
                } else {
                    head
                }
            }
            KeyCode::Down => {
                if self.line_width(down_one).is_some() {
                    CellPos {
                        line: down_one,
                        col: head.col,
                    }
                } else {
                    head
                }
            }
            _ => head,
        };
        if nudged == head {
            return;
        }
        if let Some(selection) = self.selection.as_mut() {
            selection.head = nudged;
        }
        self.follow_head(nudged.line);
        if let Some(selection) = self.selection.as_ref()
            && selection.anchor != selection.head
        {
            let (text, covered_any) = self.selection_text(selection);
            if covered_any {
                (self.copier)(&text);
            }
        }
    }

    /// The rendered width, in cells, of one selectable conversation
    /// line.
    ///
    /// `None` past the end of the document the selection walks — the
    /// same line segments [`Self::selectable_window`] windows over,
    /// with running tools excluded. Left and Right use the width as
    /// their wrap boundary and Down uses existence as its floor.
    fn line_width(&self, index: usize) -> Option<usize> {
        self.selectable_window(index, 1)
            .into_iter()
            .next()
            .map(|line| line.spans.iter().map(|span| span.content.width()).sum())
    }

    /// A window over the lines a selection can cover.
    ///
    /// The settled conversation, the frozen streaming prefix, and
    /// the live region — running tools render after these but are
    /// not selectable, so they stay out of the selection's line
    /// space: what a copy walks and what the highlight spans are the
    /// same lines the pane shows above any tool block.
    fn selectable_window(&self, skip: usize, height: usize) -> Vec<Line<'_>> {
        visible_window(
            [
                &self.conversation_cache,
                &self.frozen_lines,
                &self.live_lines,
                &NO_LINES,
            ],
            skip,
            height,
        )
    }

    /// Scroll the view just enough to keep `line` on screen.
    ///
    /// A keyboard-walked selection head moves through the document
    /// under its own power; the viewport follows so the moving end
    /// stays visible — stepping upward detaches the view, and
    /// stepping back within the stick tolerance re-arms it.
    fn follow_head(&mut self, line: usize) {
        let Some(view) = self.last_view.as_ref().copied() else {
            return;
        };
        let height = usize::from(view.area.height);
        if height == 0 {
            return;
        }
        if line < view.skip {
            self.scroll_offset = self
                .scroll_offset
                .saturating_add(view.skip.saturating_sub(line));
            self.auto_scroll = false;
        } else if line >= view.skip.saturating_add(height) {
            let excess = line
                .saturating_sub(view.skip.saturating_add(height))
                .saturating_add(1);
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
            self.rearm_if_near_bottom();
        }
    }

    /// The transcript cell under screen column/row, if the position
    /// lands inside the last frame's conversation pane.
    ///
    /// The line clamps into the selectable document: running tools
    /// render below it but never join the selection's line space,
    /// so a press on a tool row — or on the empty pane under short
    /// content — anchors on the last line a copy can walk, the way
    /// an editor clamps a click below a document's end. A pane with
    /// nothing selectable offers no cell at all.
    fn cell_at(&self, column: u16, row: u16) -> Option<CellPos> {
        let view = self.last_view.as_ref()?;
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return None;
        }
        let last = view.selectable.checked_sub(1)?;
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)))
            .min(last);
        let col = usize::from(column.saturating_sub(view.area.x));
        Some(CellPos { line, col })
    }

    /// Toggle the tool expansion at screen column/row, if any.
    ///
    /// A click — press and release without travel — on a tool
    /// summary row or a running tool row opens or closes that
    /// call's expansion. Toggling a completed block changes the
    /// rendered line count below it, so it bumps the conversation
    /// generation and forfeits the selection, the same way any
    /// renumbering mutation does; a running call's rows are
    /// rebuilt every frame and need no bump. Returns whether a
    /// tool row was hit at all.
    fn toggle_tool_at(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.last_view else {
            return false;
        };
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return false;
        }
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)));
        let call_id = self
            .tool_summary_lines
            .get(&line)
            .or_else(|| self.tool_running_lines.get(&line))
            .cloned()
            .unwrap_or_default();
        if call_id.is_empty() {
            return false;
        }
        let expanding = !self.expanded_tools.remove(&call_id);
        if expanding {
            self.expanded_tools.insert(call_id.clone());
        }
        if self.tool_details.contains_key(&call_id) {
            if expanding {
                self.auto_scroll = false;
            }
            self.conversation_generation = self.conversation_generation.saturating_add(1);
            self.selection = None;
            self.drag_position = None;
        }
        true
    }

    /// Place the composer's caret where a press landed.
    ///
    /// Presses inside the composer pane claim the press — padding
    /// rows included, click-clamped the way editors clamp: the row
    /// clamps into the text window, and the column travels raw so
    /// the caret search clamps it to that row's own width. Returns
    /// whether the press belonged to the composer at all.
    fn place_input_caret(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.input_view else {
            return false;
        };
        if column < view.pane.x
            || column >= view.pane.right()
            || row < view.pane.y
            || row >= view.pane.bottom()
        {
            return false;
        }
        let grid_row = view.start.saturating_add(
            usize::from(row.saturating_sub(view.origin.1)).min(view.visible.saturating_sub(1)),
        );
        let grid_column = usize::from(column.saturating_sub(view.origin.0));
        self.input
            .move_caret_to_cell(self.input_wrap_width, grid_row, grid_column);
        true
    }

    /// The characters the selection covers, as plain text, with
    /// whether any character was covered at all.
    ///
    /// Walks the same rendered lines the highlight walks, so what is
    /// copied is exactly what is shown selected. The flag is false
    /// only when no row covered a single character — a span over
    /// blank cells — so a caller can leave the clipboard alone even
    /// though the separators make the text itself non-empty; an
    /// interior blank line inside a genuine selection is content
    /// and does not clear the flag.
    fn selection_text(&self, selection: &CellSelection) -> (String, bool) {
        let (from, to) = ordered(selection);
        let lines = self.selectable_window(
            from.line,
            to.line.saturating_sub(from.line).saturating_add(1),
        );
        let mut text = String::new();
        let mut covered_any = false;
        for (offset, line) in lines.iter().enumerate() {
            let line_index = from.line.saturating_add(offset);
            let start = if line_index == from.line { from.col } else { 0 };
            let end = if line_index == to.line {
                to.col
            } else {
                usize::MAX
            };
            let flat: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            let covered = covered_chars(&flat, start, end);
            if !covered.is_empty() {
                covered_any = true;
            }
            text.push_str(&covered);
            if line_index < to.line {
                text.push('\n');
            }
        }
        (text, covered_any)
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
    /// tick stays several frame intervals wide, so the reading is
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
    /// spinner one frame while tools run, steps the view while a
    /// drag is parked at the conversation pane's top or bottom row —
    /// the beat that keeps an edge push scrolling between movement
    /// reports — and flips the composer caret's blink phase on its
    /// own beat. An idle tick between flips and away from all of the
    /// above draws nothing.
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
        let dragged = self.drag_autoscroll();
        let blinked = self.caret_blink.tick();
        let notice_lapsed = self
            .transient_notice
            .as_ref()
            .is_some_and(|(_, until)| *until <= now);
        if notice_lapsed {
            self.transient_notice = None;
        }
        // No run, no prompts: a finished or cancelled run's dispatches
        // are gone, so the gate already denied whatever went unanswered
        // — the queue must not hold an overlay the run can no longer
        // act on. Dropping the requests closes their reply channels,
        // which reads as a denial on any resolver still parked on one.
        let prompts_retired = !self.pending_permissions.is_empty()
            && !self
                .state
                .agent_running
                .load(std::sync::atomic::Ordering::SeqCst);
        if prompts_retired {
            self.pending_permissions.clear();
        }
        self.any_tools_running()
            || claimed
            || dragged
            || blinked
            || notice_lapsed
            || prompts_retired
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

    /// Render one frame of the layout.
    ///
    /// Conversation fills the space above a blank spacer row that
    /// separates it from the input field; the two-row status bar
    /// closes the frame at the bottom — each pane renders through its
    /// own method. The input field grows with its buffer up to its
    /// text-row cap and then holds: the conversation pane reflows
    /// while the composer grows and holds still once it has.
    pub fn render(&mut self, frame: &mut Frame) {
        self.drain_shared_state();
        let area = frame.area();
        // The composer's height follows its buffer: one text row when
        // empty, one more per line — typed or wrapped — up to the cap.
        // The wrap width is the pane's regardless of how tall the pane
        // ends up — the vertical split gives every chunk the full
        // width — so it comes from the terminal itself, and the one
        // wrap computed here serves the height, the field, and the
        // status tag alike.
        let wrap_width = area
            .width
            .max(1)
            .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2));
        let (rows, caret) = self.composer_wrap(wrap_width);
        let text_rows = u16::try_from(rows.len())
            .unwrap_or(1)
            .clamp(1, INPUT_TEXT_ROWS);
        let composer_height = text_rows.saturating_add(INPUT_VERTICAL_PADDING.saturating_mul(2));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(composer_height),
                Constraint::Length(STATUS_BAR_ROWS),
            ])
            .split(area);

        let fallback = area;
        self.render_conversation(frame, pane(&chunks, 0, fallback));
        self.render_notice(frame, pane(&chunks, 1, fallback));
        // The paintless theme's composer marking: hairline rules the
        // terminal draws itself — an underlined spacer puts a 1px
        // line at the box's top edge, an underlined last row at its
        // bottom. Solid and thin on every terminal: the rule is the
        // terminal's own decoration, not glyphs (which gap between
        // rows on some) or paint (which is a full cell thick).
        if let Some(color) = self.theme.ui.composer_border {
            let spacer = pane(&chunks, 2, fallback);
            let composer = pane(&chunks, 3, fallback);
            let rule = Style::default()
                .fg(color)
                .add_modifier(ratatui::style::Modifier::UNDERLINED);
            if spacer.height > 0 {
                frame.buffer_mut().set_style(spacer, rule);
            }
            if composer.height > 0 {
                let bottom = Rect {
                    y: composer.bottom().saturating_sub(1),
                    height: 1,
                    ..composer
                };
                frame.buffer_mut().set_style(bottom, rule);
            }
        }
        let input_area = pane(&chunks, 3, fallback);
        self.input_wrap_width = wrap_width;
        self.render_input(frame, input_area, &rows, caret);
        self.render_status_bar(frame, pane(&chunks, 4, fallback), &rows, caret);
        if let Some((prompt, more)) = self.pending_permissions.front().map(|request| {
            (
                request.prompt.clone(),
                self.pending_permissions.len().saturating_sub(1),
            )
        }) {
            self.render_permission_overlay(frame, area, &prompt, more);
        }
    }

    /// Render the pending permission ask as a sheet over the prompt
    /// box.
    ///
    /// Draws where the user's attention already is: anchored at the
    /// bottom, directly above the status bar, covering the notice row,
    /// the spacer, and the composer — the box the user would type in
    /// is replaced by the question. The conversation above keeps
    /// rendering behind it, so what the run is doing stays visible
    /// while the ask waits. The prompt and the hints are each
    /// pre-wrapped at the box's inner width (a block at most
    /// `MAX_OVERLAY_ROWS` rows, the last ellipsized), so no rendered
    /// line can re-flow inside the border and the hints always fit,
    /// whatever the tool name's length or the terminal's width; only
    /// a terminal shorter than the box clips, at the box's top. The
    /// strings arrive cloned so this borrows nothing from the queue.
    fn render_permission_overlay(&self, frame: &mut Frame, area: Rect, prompt: &str, more: usize) {
        let hints = if more == 0 {
            "[y] allow · [n] deny · ctrl+c cancels (a draft clears first)".to_string()
        } else {
            format!("[y] allow · [n] deny · ctrl+c cancels · {more} more waiting")
        };
        // The sheet spans the terminal, so its inner width is the full
        // width minus the border pair — wrap every block there, and
        // every rendered line fits the box whatever the terminal's
        // width. The hints wrap too, so even they cannot re-flow past
        // the height the box reserves for them.
        let wrap_at = usize::from(area.width.saturating_sub(2));
        let mut lines: Vec<Line<'_>> = wrap_prompt_rows(prompt, wrap_at, MAX_OVERLAY_ROWS)
            .into_iter()
            .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.foreground)))
            .collect();
        lines.push(Line::default());
        lines.extend(
            wrap_prompt_rows(&hints, wrap_at, MAX_OVERLAY_ROWS)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.secondary))),
        );
        // The sheet owns every row above the status bar when it needs
        // them, but never the status bar itself — the session line
        // stays readable while an ask waits.
        let sheet_area = Rect {
            height: area.height.saturating_sub(STATUS_BAR_ROWS),
            ..area
        };
        let height = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(sheet_area.height);
        let overlay = bottom_sheet_rect(height, sheet_area);
        if overlay.width == 0 || overlay.height == 0 {
            return;
        }
        let accent = Style::default().fg(self.theme.ui.primary);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(accent)
            .title(Span::styled(" Permission required ", accent));
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(self.theme.ui.surface)),
            overlay,
        );
    }

    /// Render the conversation pane: the settled conversation, the
    /// streaming region below it, any running tools, and the
    /// scrollbar in a reserved right gutter.
    ///
    /// The gutter is one column off the pane's right edge, reserved
    /// whether or not the document scrolls, so wrapped text never
    /// collides with the scrollbar and the wrap width stays stable
    /// as the document crosses the scrollability threshold. A
    /// pinned view re-anchors to the newest line; a detached view
    /// holds its place while content streams in below it or
    /// collapses away.
    fn render_conversation(&mut self, frame: &mut Frame, area: Rect) {
        let (text_area, scrollbar_area) = split_scrollbar_gutter(area);
        let conversation_height = text_area.height as usize;
        let width = text_area.width.max(1);
        if self.conversation_cache_generation != self.conversation_generation
            || self.conversation_cache_width != width
        {
            let mut summary_lines = HashMap::new();
            self.conversation_cache = self.conversation_lines(text_area, &mut summary_lines);
            self.tool_summary_lines = summary_lines;
            self.conversation_cache_width = width;
            self.conversation_cache_generation = self.conversation_generation;
            // A rebuilt line space — a new message graduating in, a
            // verbosity switch, a resize — re-numbers every line under
            // the selection's stored indexes, so the highlight would
            // silently re-target other text; a re-flowed document
            // forfeits the selection instead, the way a terminal's
            // native one is lost on redraw.
            self.selection = None;
            self.drag_position = None;
        }
        // The streaming region re-renders as its reply grows — a
        // delta re-wraps the live lines, a freeze advances the frozen
        // ones — re-numbering every line from the frozen prefix down.
        // A selection reaching into that region is stored against the
        // old numbering and forfeits, the same way a rebuilt
        // conversation forfeits one; a selection entirely within the
        // settled transcript keeps its indexes and survives.
        let conversation_len = self.conversation_cache.len();
        if self.refresh_streaming_region(width)
            && self
                .selection
                .as_ref()
                .is_some_and(|selection| ordered(selection).1.line >= conversation_len)
        {
            self.selection = None;
            self.drag_position = None;
        }
        let (tools, running_rows) = self.active_tool_lines(width);
        let selectable_lines = self
            .conversation_cache
            .len()
            .saturating_add(self.frozen_lines.len())
            .saturating_add(self.live_lines.len());
        self.tool_running_lines = running_rows
            .into_iter()
            .map(|(index, call_id)| (index.saturating_add(selectable_lines), call_id))
            .collect();
        let total_lines = selectable_lines.saturating_add(tools.len());
        self.settle_scroll_offset(total_lines, conversation_height);
        let skip = total_lines
            .saturating_sub(conversation_height)
            .saturating_sub(self.scroll_offset);
        let mut visible = visible_window(
            [
                &self.conversation_cache,
                &self.frozen_lines,
                &self.live_lines,
                &tools,
            ],
            skip,
            conversation_height,
        );
        self.last_view = Some(ViewState {
            area: text_area,
            skip,
            total: total_lines,
            selectable: selectable_lines,
        });
        if let Some(selection) = &self.selection {
            reverse_selection(&mut visible, skip, selection);
        }
        frame.render_widget(Paragraph::new(visible), text_area);
        if let Some(gutter) = scrollbar_area
            && let Some((thumb_pos, thumb_len)) =
                scrollbar_geometry(total_lines, conversation_height, skip)
        {
            render_scrollbar(
                frame,
                gutter,
                thumb_pos,
                thumb_len,
                self.theme.ui.scrollbar_thumb,
                self.theme.ui.scrollbar_track,
            );
        }
    }

    /// Settle the scroll offset for this frame's layout.
    ///
    /// A pinned view sits at the newest line: the offset resets to
    /// zero and growth below the viewport carries it along. A
    /// detached view holds its anchor: the offset is compensated for
    /// the document growing or shrinking since the last frame and
    /// clamped to the scrollable height, so the same lines stay in
    /// view while content streams in below or collapses away.
    fn settle_scroll_offset(&mut self, total_lines: usize, viewport: usize) {
        if self.auto_scroll {
            self.scroll_offset = 0;
        } else {
            let delta = total_lines.abs_diff(self.last_layout_lines);
            if total_lines >= self.last_layout_lines {
                self.scroll_offset = self.scroll_offset.saturating_add(delta);
            } else {
                self.scroll_offset = self.scroll_offset.saturating_sub(delta);
            }
            self.scroll_offset = self.scroll_offset.min(total_lines.saturating_sub(viewport));
        }
        self.last_layout_lines = total_lines;
    }

    /// Retain a graduated call's full detail, evicting at the cap.
    ///
    /// The one insertion path for the detail store: a graduation
    /// with captured content, and the resume seeding of blocks the
    /// file carried. A retried call graduates once per attempt
    /// under the same id, so the queue is requeued rather than
    /// duplicated and the detail ages from its newest attempt; the
    /// oldest entry retires past the cap with its toggle state.
    fn retain_tool_detail(&mut self, call_id: &str, input_json: &str, output: &str) {
        if call_id.is_empty() || input_json.is_empty() {
            return;
        }
        self.tool_detail_order.retain(|id| id != call_id);
        self.tool_detail_order.push_back(call_id.to_string());
        if self.tool_detail_order.len() > TOOL_DETAIL_CAP
            && let Some(retired) = self.tool_detail_order.pop_front()
        {
            self.tool_details.remove(&retired);
            self.expanded_tools.remove(&retired);
        }
        self.tool_details.insert(
            call_id.to_string(),
            ToolDetail {
                input_json: input_json.to_string(),
                output: output.to_string(),
            },
        );
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
                    self.retain_tool_detail(
                        &result.call_id,
                        &result.full_input,
                        &result.full_output,
                    );
                    self.push_message(TuiMessage::Assistant {
                        blocks: vec![ContentBlock::Tool {
                            name: result.name,
                            call_id: result.call_id,
                            input_preview: result.input_summary,
                            success: !result.is_error,
                            elapsed_secs: result.duration.as_secs_f64(),
                            output_preview: result.output_preview,
                            retained_input: result.full_input,
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
    fn conversation_lines(
        &self,
        area: Rect,
        summary_lines: &mut HashMap<usize, String>,
    ) -> Vec<Line<'static>> {
        let width = area.width.max(1);
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for message in &self.conversation {
            match message {
                TuiMessage::User { text, .. } => {
                    // Wrapped, not clipped: pasted text routinely
                    // runs lines past the pane's width, and a row
                    // cut at the edge reads as text that never
                    // arrived.
                    for segment in text.split('\n') {
                        lines.extend(plain_wrapped_lines(
                            segment,
                            usize::from(width),
                            self.theme.ui.user_message_fg,
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
                                call_id,
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
                                    call_id: String::new(),
                                    is_error: !*success,
                                    duration: elapsed,
                                    input_summary: input_preview.clone(),
                                    output_preview: String::new(),
                                    full_input: String::new(),
                                    full_output: String::new(),
                                };
                                let first = lines.len();
                                lines.extend(crate::tool_render::completed_tool_lines(
                                    &record,
                                    &self.theme,
                                    self.verbosity,
                                ));
                                if let Some(detail) = self.tool_details.get(call_id) {
                                    let expanded = self.expanded_tools.contains(call_id);
                                    prepend_marker(
                                        &mut lines,
                                        first,
                                        if expanded { "▾ " } else { "▸ " },
                                        self.theme.ui.dim,
                                    );
                                    if expanded {
                                        lines.extend(expansion_lines(
                                            &detail.input_json,
                                            Some(&detail.output),
                                            width,
                                            self.theme.ui.assistant_message_fg,
                                            self.theme.ui.dim,
                                        ));
                                    }
                                    for index in first..lines.len() {
                                        summary_lines.insert(index, call_id.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                TuiMessage::System { text, .. } => {
                    for segment in text.split('\n') {
                        lines.extend(plain_wrapped_lines(
                            segment,
                            usize::from(width),
                            self.theme.ui.dim,
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
    /// rendering after another thread's panic. Returns whether the
    /// region's rendered lines changed this call — the signal that
    /// the line numbering below the settled transcript moved.
    fn refresh_streaming_region(&mut self, width: u16) -> bool {
        let buffer = self
            .state
            .streaming_text
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if buffer.is_empty() {
            return self.reset_stream_cache();
        }
        let mut changed = false;
        if self.stream_cache_width != width {
            changed = self.reset_stream_cache();
            self.stream_cache_width = width;
        } else if self.frozen_upto > 0
            && buffer
                .get(..self.frozen_upto)
                .is_none_or(|prefix| fingerprint(prefix) != self.frozen_fingerprint)
        {
            changed = self.reset_stream_cache();
        }
        let frozen_before = self.frozen_lines.len();
        self.advance_freeze(&buffer, width);
        if self.frozen_lines.len() != frozen_before {
            changed = true;
        }

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
            return changed;
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
        true
    }

    /// Reset the streaming cache.
    ///
    /// Drops the frozen lines, offset, and separator count; the next
    /// layout re-freezes from the buffer's start. Called when the
    /// buffer empties (the reply graduated or the turn failed), its
    /// frozen prefix stops matching, or the pane width changes.
    /// Returns whether any rendered lines were dropped.
    fn reset_stream_cache(&mut self) -> bool {
        let dropped = !(self.frozen_lines.is_empty() && self.live_lines.is_empty());
        self.frozen_lines.clear();
        self.frozen_upto = 0;
        self.frozen_separators = 0;
        self.frozen_fingerprint = 0;
        self.live_stamp = 0;
        self.live_frozen_upto = 0;
        self.live_width = 0;
        self.live_lines.clear();
        dropped
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

    /// The in-flight tool indicator lines, with their click rows.
    ///
    /// One dim line per dispatched call — expanded, when opened,
    /// to the call's captured input with the output pending; a
    /// poisoned lock renders none, the same policy the status bar
    /// applies. Returns the lines and, for each call the capture
    /// store knows, every row of its block — summary and
    /// expansion alike, the rows a click toggles.
    fn active_tool_lines(&self, width: u16) -> (Vec<Line<'static>>, HashMap<usize, String>) {
        let tools = self
            .state
            .active_tools
            .lock()
            .map_or_else(|_| Vec::new(), |tools| tools.clone());
        let captures = self
            .state
            .tool_captures
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut lines = Vec::new();
        let mut rows = HashMap::new();
        for tool in &tools {
            let first = lines.len();
            lines.extend(crate::tool_render::running_tool_lines(
                tool,
                self.spinner_idx,
                &self.theme,
                self.verbosity,
            ));
            let Some(capture) = captures.get(&tool.call_id) else {
                continue;
            };
            let expanded = self.expanded_tools.contains(&tool.call_id);
            prepend_marker(
                &mut lines,
                first,
                if expanded { "▾ " } else { "▸ " },
                self.theme.ui.dim,
            );
            if expanded {
                let output = if capture.done {
                    Some(capture.output.as_str())
                } else {
                    None
                };
                lines.extend(expansion_lines(
                    &capture.input_json,
                    output,
                    width,
                    self.theme.ui.assistant_message_fg,
                    self.theme.ui.dim,
                ));
            }
            for index in first..lines.len() {
                rows.insert(index, tool.call_id.clone());
            }
        }
        (lines, rows)
    }

    /// The frame's single wrap of the composer, reused across
    /// frames that do not touch it.
    ///
    /// A full-buffer wrap costs tens of milliseconds at the paste
    /// cap, and idle frames — the caret's blink, the tool spinner,
    /// a streaming delta — would otherwise pay it every time. The
    /// cache keys on the editor's mutation stamp and the wrap
    /// width, so only edits and resizes re-wrap; everything else
    /// clones the cached grid.
    fn composer_wrap(&mut self, width: u16) -> (Arc<Vec<String>>, (u16, u16)) {
        if let Some(cache) = &self.input_wrap_cache
            && cache.stamp == self.input.stamp()
            && cache.width == width
        {
            return (Arc::clone(&cache.rows), cache.caret);
        }
        let (rows, caret) = self.input.display_rows_and_caret(width);
        let caret = caret.unwrap_or((0, 0));
        let rows = Arc::new(rows);
        self.input_wrap_cache = Some(InputWrapCache {
            stamp: self.input.stamp(),
            width,
            rows: Arc::clone(&rows),
            caret,
        });
        (rows, caret)
    }

    /// Render the notice row: one line of transient or state-driven
    /// message above the composer.
    ///
    /// The armed-quit hint owns the row while it holds — safety
    /// wording outranks anything expiring — and a transient notice
    /// (a cancel, an event worth a glance, not a transcript row)
    /// shows until its hold elapses. Blank otherwise; the row is
    /// always reserved so nothing on screen shifts when a message
    /// arrives or leaves.
    fn render_notice(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let text = if self.quit.armed {
            Some("press ctrl+c again to quit")
        } else {
            self.transient_notice
                .as_ref()
                .filter(|(_, until)| *until > Instant::now())
                .map(|(text, _)| text.as_str())
        };
        if let Some(text) = text {
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(self.theme.ui.input_border)),
                area,
            );
        }
    }

    /// Render the input field: a fixed multi-row window onto the
    /// buffer, as a borderless composer drawn on the theme's surface
    /// color and padded on all sides.
    ///
    /// The field's shape is a pure background fill with square
    /// corners — solid by construction, like the scrollbar. Corners
    /// stay square deliberately: a cell grid cannot draw a smooth
    /// curve, and both approximations it can draw (corner glyphs,
    /// clipped corner cells) render as pixel-like steps. The text sits
    /// inside horizontal and vertical padding; the window shows the
    /// caret's line and the lines above it, so typing at the end
    /// sees the newest lines and browsing up scrolls with the
    /// caret. The field carries no text of its own — queue and
    /// line-position state lives on the status bar. `rows` and
    /// `caret` are the frame's single wrap of the buffer, computed by
    /// [`render`](Self::render) and shared with the status tag. The
    /// text window takes its height from the pane's own rows net of
    /// the vertical padding, so no paint of the field's lands
    /// outside the pane it was given; the cursor position is clamped
    /// the same way, so a terminal too short for the field's full
    /// height parks the cursor on a row the field owns, and a field
    /// allotted no rows at all parks no cursor.
    fn render_input(&mut self, frame: &mut Frame, area: Rect, rows: &[String], caret: (u16, u16)) {
        let (caret_row, column) = caret;

        let surface = self.theme.ui.surface;
        let text_style = if self.theme.ui.composer_border.is_some() {
            // A bordered composer paints nothing — the hairline rules
            // live in `render`, on the rows above and below this pane.
            Style::default().fg(self.theme.ui.input_text)
        } else {
            frame
                .buffer_mut()
                .set_style(area, Style::default().bg(surface));
            Style::default().fg(self.theme.ui.input_text).bg(surface)
        };
        let text_height = INPUT_TEXT_ROWS.min(area.height.saturating_sub(INPUT_VERTICAL_PADDING));
        let interior = Rect {
            x: area
                .x
                .saturating_add(INPUT_SIDE_INSET)
                .min(area.right().saturating_sub(1)),
            y: area.y.saturating_add(INPUT_VERTICAL_PADDING),
            width: area
                .width
                .max(1)
                .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2)),
            height: text_height,
        };
        // The window shows the caret's line and the lines above it —
        // typing at the end sees the newest lines, browsing up
        // scrolls with the caret.
        let visible_rows = usize::from(text_height);
        let start = usize::from(caret_row).saturating_sub(visible_rows.saturating_sub(1));
        self.input_view = Some(InputView {
            pane: area,
            origin: (interior.x, interior.y),
            start,
            visible: visible_rows,
        });
        let lines: Vec<Line<'_>> = rows
            .iter()
            .skip(start)
            .take(visible_rows)
            .map(|row| Line::styled(row.as_str(), text_style))
            .collect();
        frame.render_widget(Paragraph::new(lines), interior);

        let caret_x = area
            .x
            .saturating_add(INPUT_SIDE_INSET)
            .saturating_add(column)
            .min(
                area.x
                    .saturating_add(INPUT_SIDE_INSET)
                    .saturating_add(interior.width)
                    .saturating_sub(1),
            )
            .min(area.right().saturating_sub(2));
        let caret_y = area
            .y
            .saturating_add(INPUT_VERTICAL_PADDING)
            .saturating_add(
                u16::try_from(usize::from(caret_row).saturating_sub(start)).unwrap_or(0),
            )
            .min(area.bottom().saturating_sub(1));
        if area.height > 0 && self.composer_caret_visible() {
            frame.set_cursor_position((caret_x, caret_y));
        }
    }

    /// Whether the composer's terminal cursor shows this frame.
    ///
    /// False while the blink phase has it off — and for the whole
    /// time a permission ask is pending: the sheet owns the composer's
    /// rows and its keys, and a cursor there would point at nothing
    /// the user can edit. A frame that sets no cursor position hides
    /// the terminal cursor, so this is the whole show-or-hide
    /// decision.
    #[must_use]
    pub fn composer_caret_visible(&self) -> bool {
        self.caret_blink.on && self.pending_permissions.front().is_none()
    }

    /// Render the one-line status bar.
    ///
    /// Names the configured model and the cumulative token totals on
    /// the left; while the input holds state — submissions queued
    /// behind the driver, or a buffer longer than the composer's
    /// window — a position tag sits right-aligned in the theme's
    /// input accent color. Both sit on the themed bar colors, and
    /// the status text is clipped short of the tag's columns, so
    /// the two never paint over each other on a narrow frame. A
    /// chunk allotted no rows renders nothing — the bar never paints
    /// a row another pane owns. `rows` and `caret` arrive from the
    /// frame's single wrap of the buffer.
    fn render_status_bar(&self, frame: &mut Frame, area: Rect, rows: &[String], caret: (u16, u16)) {
        if area.height == 0 {
            return;
        }
        let tokens = self.state.tokens.lock().map_or_else(
            |_| 0,
            |counts| {
                counts
                    .cumulative_input
                    .saturating_add(counts.cumulative_output)
            },
        );
        let mut status_text = format!(" {}  │  CTX: {tokens}", self.config.api.model);
        if let Some(id) = &self.session_id {
            status_text.push_str("  │  ");
            status_text.push_str(id);
        }
        let bar_style = Style::default()
            .fg(self.theme.ui.status_bar_fg)
            .bg(self.theme.ui.status_bar_bg);
        let tag = self.input_state_tag(rows, caret);
        let tag_width = if !tag.is_empty() && area.width > 4 {
            u16::try_from(tag.width().saturating_add(2))
                .unwrap_or(area.width)
                .min(area.width)
        } else {
            0
        };
        let left_width = match tag_width {
            0 => area.width,
            reserved => area.width.saturating_sub(reserved.saturating_add(1)),
        };
        // The bar's own background spans both of its rows first, so
        // the gutter column a clipped left edge leaves between the two
        // paragraphs reads as bar, not as a hole in it; the content
        // sits on the bottom row, the row above it breathing room.
        frame
            .buffer_mut()
            .set_style(area, Style::default().bg(self.theme.ui.status_bar_bg));
        let content = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(status_text).style(bar_style),
            Rect {
                width: left_width,
                ..content
            },
        );
        if tag_width > 0 {
            let tag_area = Rect {
                x: content.right().saturating_sub(tag_width),
                width: tag_width,
                ..content
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    tag,
                    Style::default().fg(self.theme.ui.input_border),
                ))
                .style(bar_style)
                .alignment(ratatui::layout::Alignment::Right),
                tag_area,
            );
        }
    }

    /// The composer's state as a status-bar tag, empty when idle.
    ///
    /// Reports submissions queued behind the driver and, while the
    /// buffer holds more lines than the composer's window, which
    /// line the caret is on — the two facts a user can act on.
    /// `rows` and `caret` arrive from the frame's single wrap of the
    /// buffer, so asking never re-wraps the editor.
    fn input_state_tag(&self, rows: &[String], caret: (u16, u16)) -> String {
        let queued = self.state.queued.load(Ordering::SeqCst);
        let mut parts = Vec::new();
        if queued > 0 {
            parts.push(format!("{queued} queued"));
        }
        if rows.len() > usize::from(INPUT_TEXT_ROWS) {
            let (caret_row, _) = caret;
            parts.push(format!(
                "{}/{}",
                usize::from(caret_row).saturating_add(1).min(rows.len()),
                rows.len()
            ));
        }
        parts.join(" · ")
    }
}

/// Prefix a disclosure glyph onto one rendered line.
///
/// The summary row's existing spans shift right by the glyph, so
/// the marker rides whatever styling the row already carries.
fn prepend_marker(lines: &mut [Line<'static>], index: usize, glyph: &str, color: Color) {
    let Some(line) = lines.get_mut(index) else {
        return;
    };
    let mut spans = Vec::with_capacity(line.spans.len().saturating_add(1));
    spans.push(Span::styled(glyph.to_string(), Style::default().fg(color)));
    spans.extend(std::mem::take(&mut line.spans));
    line.spans = spans;
}

/// The lines of a tool block's expansion.
///
/// The full command — the call's pretty-printed input — over the
/// call's output, each indented under its label and wrapped at the
/// pane width. A call still running shows its command with the
/// output pending, so an expansion opened early is not a dead end.
fn expansion_lines(
    input_json: &str,
    output: Option<&str>,
    width: u16,
    base: Color,
    dim: Color,
) -> Vec<Line<'static>> {
    let wrap = usize::from(width.max(1));
    let mut lines = Vec::new();
    lines.push(Line::styled(
        "    input:".to_string(),
        Style::default().fg(dim),
    ));
    for raw in input_json.split('\n') {
        lines.extend(plain_wrapped_lines(&format!("    {raw}"), wrap, dim));
    }
    match output {
        Some(text) => {
            lines.push(Line::styled(
                "    output:".to_string(),
                Style::default().fg(dim),
            ));
            for raw in text.split('\n') {
                lines.extend(plain_wrapped_lines(&format!("    {raw}"), wrap, base));
            }
        }
        None => lines.push(Line::styled(
            "    output: … running".to_string(),
            Style::default().fg(dim),
        )),
    }
    lines.push(Line::from(""));
    lines
}

/// One display cell of the rendered transcript.
///
/// A conversation line index paired with the column, in cells, of
/// the character within that rendered line — the unit a mouse
/// selection drags over.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CellPos {
    /// The conversation line the cell sits on.
    line: usize,
    /// The cell column within the rendered line.
    col: usize,
}

/// A tool call's retained full data, for the expanded block.
///
/// What the dispatch-side capture recorded, held on the app past
/// graduation so the expansion outlives the shared store's
/// turnover.
struct ToolDetail {
    /// The call's input, pretty-printed JSON.
    input_json: String,
    /// The call's output, within the retention cap.
    output: String,
}

/// A mouse selection between two display cells.
///
/// Order-independent: whichever end the press anchored stays put
/// while the head follows the drag — and the edge-scroll steps that
/// carry it past the viewport — in content coordinates, so scrolling
/// between drags does not move the selection.
struct CellSelection {
    /// The cell the press landed on.
    anchor: CellPos,
    /// The cell the drag currently reaches.
    head: CellPos,
}

/// The quit lifecycle's two halves.
///
/// Exit is Ctrl+C pressed twice on an empty buffer: the first
/// arms, the second confirms and requests the exit. The press
/// that clears a non-empty buffer arms nothing — clearing is not
/// consenting — and any other key or mouse press disarms, so the
/// chord never fires from stale intent.
#[derive(Debug, Clone, Copy, Default)]
struct QuitState {
    /// Whether one press has armed the exit.
    armed: bool,
    /// Whether the second press confirmed it.
    ///
    /// The run loop leaves at the top of its next iteration.
    requested: bool,
}

impl QuitState {
    /// Record a first press.
    fn arm(&mut self) {
        self.armed = true;
    }

    /// Take back a first press.
    ///
    /// Any key that is not the chord's own spelling disarms it.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

/// The composer caret's blink state.
///
/// The terminal's blinking-cursor request is often ignored or
/// preference-gated, so the app owns the blink: half a second
/// visible, half a second dark. Keyboard input resolidifies the
/// caret and restarts the phase, so typing holds it solid and the
/// blink resumes after half a second of stillness; mouse traffic
/// reads, it does not type, and leaves the phase alone.
struct CaretBlink {
    /// Whether the caret is in its visible phase.
    on: bool,
    /// Ticks since the phase last flipped.
    ticks: u32,
}

impl Default for CaretBlink {
    fn default() -> Self {
        Self { on: true, ticks: 0 }
    }
}

impl CaretBlink {
    /// Restart the visible phase — the reaction to any input.
    fn resolidify(&mut self) {
        self.on = true;
        self.ticks = 0;
    }

    /// Advance one tick, flipping the phase when it is due.
    ///
    /// Returns whether the phase flipped, so the caller can redraw —
    /// the only idle activity a blink generates.
    fn tick(&mut self) -> bool {
        self.ticks = self.ticks.saturating_add(1);
        if self.ticks >= CARET_BLINK_TICKS {
            self.ticks = 0;
            self.on = !self.on;
            true
        } else {
            false
        }
    }
}

/// The composer's cached wrap.
///
/// One buffer mutation at one width: the rows the buffer wraps to
/// and the caret's resolved cell among them, kept until the
/// editor's stamp or the pane width moves. The answers a frame's
/// sizing, painting, and status tag all need, computed once per
/// change instead of once per frame.
struct InputWrapCache {
    /// The editor mutation stamp the wrap was computed at.
    ///
    /// The cache holds exactly while this matches the editor's
    /// current stamp; every accepted edit and caret move advances
    /// it, which is what tells an untouched frame to reuse the
    /// grid.
    stamp: u64,

    /// The wrap width the grid was built for.
    ///
    /// A resize re-wraps rather than reuses — wrapped rows are
    /// width-shaped, and a pane of another width would clip rows
    /// baked for the old one.
    width: u16,

    /// The wrapped rows, shared by `Arc`.
    ///
    /// The same grid `display_rows` would rebuild; a cache hit
    /// clones the reference, not the grid — an idle frame pays
    /// neither the wrap nor a copy of its tens of thousands of
    /// rows.
    rows: Arc<Vec<String>>,

    /// The caret's cell on that grid.
    ///
    /// Resolved, the empty buffer's `(0, 0)` parking spot included,
    /// so a hit answers both questions the frame asks without
    /// touching the editor again.
    caret: (u16, u16),
}

/// What the last frame's composer pane looked like.
///
/// The bridge between a press (screen cells, between frames) and the
/// composer's wrap grid: which pane rect belongs to the composer,
/// which interior cell holds the grid's first row and column, and
/// which slice of the grid the pane's text window was showing.
#[derive(Clone, Copy)]
struct InputView {
    /// The composer pane's rect — presses inside belong to it.
    ///
    /// The press-routing claim: a press inside this rect places the
    /// caret, everything outside it falls to the conversation's
    /// selection logic.
    pane: Rect,

    /// The interior cell where the wrap grid's row 0, column 0 sits.
    ///
    /// Press coordinates become grid coordinates by subtracting
    /// this, net of the pane's own side inset.
    origin: (u16, u16),

    /// The wrap-grid row the text window's top row shows.
    ///
    /// The window follows the caret's line; this records which
    /// slice was on screen when the press landed.
    start: usize,

    /// How many wrap rows the text window shows.
    ///
    /// A press below the window's last row clamps into it — the
    /// way editors clamp a click below a document's end.
    visible: usize,
}

/// What the last frame's conversation pane looked like.
///
/// The bridge between mouse events (screen cells, between frames)
/// and the conversation's line indices: the pane's rect and the skip
/// the viewport rendered with. Stale by at most one event while
/// frames are in flight — imperceptible for a drag.
#[derive(Clone, Copy)]
struct ViewState {
    /// The conversation pane's rect, gutter excluded.
    ///
    /// The screen-cell bounds a mouse event is tested against; the
    /// scrollbar's gutter column stays outside so a press there
    /// belongs to nothing.
    area: Rect,

    /// The first conversation line the pane showed.
    ///
    /// The additive base that turns a screen row into a
    /// conversation line index — clicks, drags, and the
    /// edge-scroll all map through it.
    skip: usize,

    /// The conversation's total line count that frame.
    ///
    /// The scroll space's size: running-tool rows are included,
    /// because the viewport scrolls over them even though a
    /// selection cannot cover them.
    total: usize,

    /// The line count a selection can cover — the total without the
    /// running-tool rows, which render below the selectable document
    /// and never join the selection's line space.
    selectable: usize,
}

/// Hand `text` to the clipboard.
///
/// Two transports, both attempted: OSC 52 — the terminal-side
/// clipboard escape, honored by most modern terminals — and macOS's
/// `pbcopy`, which is authoritative where it exists. Failures are
/// silent: a copy that cannot be delivered is a nuisance, not an
/// error worth interrupting a session for.
fn copy_to_clipboard(text: &str) {
    use std::io::Write as _;

    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = std::io::stdout();
    drop(write!(stdout, "\x1b]52;c;{encoded}\x07"));
    drop(stdout.flush());
    if cfg!(target_os = "macos")
        && let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            drop(stdin.write_all(text.as_bytes()));
        }
        drop(child.wait());
    }
}

/// The empty stand-in for the tool segment a selection never covers.
///
/// Running tools render below the selectable document; the window
/// helpers want four segments, and this one is always empty — a
/// `static` so a returned window can borrow it past the call.
static NO_LINES: [Line<'static>; 0] = [];

/// Spell a paste's line breaks the editor's way.
///
/// Terminals differ in how they relay newlines inside a bracketed
/// paste — some send line feeds, some carriage returns, some both.
/// The editor's line model is the line feed, and a carriage return
/// that survives as content welds the whole paste into one logical
/// line (the composer's window then shows only its tail) and later
/// reaches the terminal as a cell the renderer never meant to
/// draw. Both spellings fold here; every other character passes
/// through untouched.
fn normalize_pasted_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Order a selection's ends by line then column.
///
/// The anchor stays wherever the press landed and the head follows
/// the drag, so either end can sit above the other; walking the
/// covered span and highlighting it both need the ends sorted.
/// Returns the lower end first, compared as `(line, column)` —
/// the order the rendered line space reads.
fn ordered(selection: &CellSelection) -> (&CellPos, &CellPos) {
    let (a, b) = (&selection.anchor, &selection.head);
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Whether a character's display cells meet the selection's span.
///
/// A character counts as covered when any of its cells does: a
/// selection landing on the second cell of a wide character still
/// picks it up, the way a terminal's native selection treats
/// partially covered ink. The character occupies `cell..cell +
/// width`; `start..=end` are the span's cell bounds.
fn cells_overlap(cell: usize, width: usize, start: usize, end: usize) -> bool {
    cell <= end && cell.saturating_add(width).saturating_sub(1) >= start
}

/// The graphemes of `flat` whose cells meet `start..=end`.
///
/// Walks by display cells and takes every grapheme cluster that
/// overlaps the span — so the copied text matches the highlighted
/// cells exactly. Clusters, not characters: a combining mark adds
/// no cell of its own, so the selection's grid stays the one the
/// renderer painted, and a selected base carries its marks.
fn covered_chars(flat: &str, start: usize, end_inclusive: usize) -> String {
    let mut out = String::new();
    let mut cell = 0;
    for cluster in flat.graphemes(true) {
        let width = cluster.width();
        if cells_overlap(cell, width, start, end_inclusive) {
            out.push_str(cluster);
        }
        cell = cell.saturating_add(width);
        if cell > end_inclusive {
            break;
        }
    }
    out
}

/// Reverse-video the selected cells across the visible lines.
///
/// Splits the affected lines' spans at the selection's cell
/// boundaries and flips exactly the covered characters, so the
/// highlight is character-granular whatever the underlying styles.
fn reverse_selection(lines: &mut [Line<'_>], skip: usize, selection: &CellSelection) {
    let (from, to) = ordered(selection);
    for (offset, line) in lines.iter_mut().enumerate() {
        let line_index = skip.saturating_add(offset);
        if line_index < from.line || line_index > to.line {
            continue;
        }
        let start = if line_index == from.line { from.col } else { 0 };
        let end = if line_index == to.line {
            to.col
        } else {
            usize::MAX
        };
        let mut cell = 0;
        let mut spans = Vec::with_capacity(line.spans.len().saturating_add(2));
        for span in line.spans.drain(..) {
            let content = span.content;
            let mut chunk = String::new();
            for cluster in content.graphemes(true) {
                let width = cluster.width();
                let selected = cells_overlap(cell, width, start, end);
                if selected {
                    if !chunk.is_empty() {
                        spans.push(ratatui::text::Span::styled(
                            std::mem::take(&mut chunk),
                            span.style,
                        ));
                    }
                    spans.push(ratatui::text::Span::styled(
                        cluster.to_string(),
                        span.style.add_modifier(ratatui::style::Modifier::REVERSED),
                    ));
                } else {
                    chunk.push_str(cluster);
                }
                cell = cell.saturating_add(width);
            }
            if !chunk.is_empty() {
                spans.push(ratatui::text::Span::styled(chunk, span.style));
            }
        }
        line.spans = spans;
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

/// The most body rows any one block of the permission overlay shows
/// before ellipsizing.
///
/// The cap keeps the box short enough to stay inside the border on
/// every terminal tall enough to matter; the prompt is a question and
/// the hints are one line, not a document.
const MAX_OVERLAY_ROWS: usize = 3;

/// Word-wrap `text` to `limit` display columns, hard-splitting any
/// single word wider than `limit`, keeping at most `max_rows` rows
/// (the last carries an ellipsis when truncation happens).
///
/// Rows never exceed `limit`, so a paragraph rendering them inside a
/// border `limit + 2` wide cannot re-flow them.
fn wrap_prompt_rows(text: &str, limit: usize, max_rows: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = word.width();
        if word_width > limit {
            if !current.is_empty() {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            for grapheme in word.graphemes(true) {
                let grapheme_width = grapheme.width();
                if current_width.saturating_add(grapheme_width) > limit {
                    rows.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                current.push_str(grapheme);
                current_width = current_width.saturating_add(grapheme_width);
            }
            continue;
        }
        let joined = if current.is_empty() {
            word_width
        } else {
            current_width.saturating_add(1).saturating_add(word_width)
        };
        if joined <= limit && !current.is_empty() {
            current.push(' ');
            current.push_str(word);
            current_width = joined;
        } else {
            if !current.is_empty() {
                rows.push(std::mem::take(&mut current));
            }
            current.push_str(word);
            current_width = word_width;
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        if let Some(last) = rows.last_mut() {
            let keep = last.width().saturating_sub(1);
            let mut trimmed = String::new();
            let mut kept = 0usize;
            for grapheme in last.graphemes(true) {
                let grapheme_width = grapheme.width();
                if kept.saturating_add(grapheme_width) > keep {
                    break;
                }
                trimmed.push_str(grapheme);
                kept = kept.saturating_add(grapheme_width);
            }
            trimmed.push('…');
            *last = trimmed;
        }
    }
    rows
}

/// A full-width rectangle anchored at the bottom of `area`.
///
/// The permission sheet's geometry: it grows upward from just above
/// the status bar over the rows the composer occupies, so the
/// conversation above and the session line below both stay in view.
fn bottom_sheet_rect(height: u16, area: Rect) -> Rect {
    let height = height.min(area.height);
    Rect {
        x: area.x,
        y: area.y.saturating_add(area.height).saturating_sub(height),
        width: area.width,
        height,
    }
}

/// Split the conversation pane into its text area and scrollbar
/// gutter.
///
/// The gutter is one column off the right edge, reserved whether or
/// not the document scrolls — wrapped text can never collide with
/// the scrollbar, and the wrap width never churns when the document
/// crosses the scrollability threshold. Returns the gutter as `None`
/// only in the degenerate one-column pane, where the text keeps the
/// full width and no scrollbar renders.
fn split_scrollbar_gutter(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < 2 {
        return (area, None);
    }
    let gutter = Rect {
        x: area.right().saturating_sub(1),
        width: 1,
        ..area
    };
    let text = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    (text, Some(gutter))
}

/// Columns of tinted breathing room at each end of the input field's
/// text, inside the fill's edge.
const INPUT_SIDE_INSET: u16 = 2;

/// Rows of text the input field shows at its largest.
///
/// The field starts as a single line and grows one row per line of
/// buffer — typed or wrapped — up to this cap; past it the text
/// window scrolls with the caret and the conversation pane above
/// holds still again.
const INPUT_TEXT_ROWS: u16 = 3;

/// Blank tinted rows of breathing room above and below the field's
/// text rows — the vertical half of the composer's inner padding.
const INPUT_VERTICAL_PADDING: u16 = 1;

/// Rows the status bar spans: its content on the bottom row, one row
/// of breathing room above it.
const STATUS_BAR_ROWS: u16 = 2;

/// The scrollbar thumb's track position and length for a document of
/// `content` lines shown through a `viewport`-line window whose top
/// line is `skip` lines into the document.
///
/// Exact integer proportion: the thumb covers the viewport's share
/// of the track — never less than one cell, never more than the
/// track — and sits where the viewport sits in the document, flush
/// with the track's top at the document's start and its bottom at
/// the end. Returns `None` when the document fits the viewport or
/// the viewport has no rows: no thumb, and the caller leaves the
/// gutter blank.
fn scrollbar_geometry(content: usize, viewport: usize, skip: usize) -> Option<(usize, usize)> {
    let scrollable = content.checked_sub(viewport)?;
    let track = viewport;
    if scrollable == 0 || track == 0 {
        return None;
    }
    let thumb = track
        .saturating_mul(viewport)
        .checked_div(content)?
        .clamp(1, track);
    let reach = track.saturating_sub(thumb);
    let position = skip
        .min(scrollable)
        .saturating_mul(reach)
        .checked_div(scrollable)?;
    Some((position, thumb))
}

/// Paint the scrollbar into its gutter as background fills: a
/// bright thumb segment over a dim rail, both in the theme's
/// scrollbar colors.
///
/// Background color is the only solid-fill primitive a terminal
/// guarantees: it covers the whole cell rectangle regardless of
/// font metrics, so consecutive rows join into one unbroken bar.
/// Glyphs cannot do this — on terminals whose cell height exceeds
/// the font's em, every glyph (blocks included) leaves a hairline
/// gap between rows and a glyph column reads as stacked bars.
fn render_scrollbar(
    frame: &mut Frame,
    gutter: Rect,
    thumb_pos: usize,
    thumb_len: usize,
    thumb_color: ratatui::style::Color,
    track_color: ratatui::style::Color,
) {
    for (row, y) in (gutter.y..gutter.bottom()).enumerate() {
        let in_thumb = row >= thumb_pos && row < thumb_pos.saturating_add(thumb_len);
        let color = if in_thumb { thumb_color } else { track_color };
        frame.buffer_mut()[(gutter.x, y)]
            .set_char(' ')
            .set_bg(color);
    }
}

/// How a non-blank line can continue a block across a blank line.
///
/// Only lists and quotes do in the grammar: a blank inside a loose
/// list or a multi-paragraph quote belongs to the block, so the
/// freeze scanner must not settle content there.
#[derive(Clone, Copy, PartialEq)]
enum ContLine {
    /// A `-`/`*`/numbered item line.
    ///
    /// Continues a loose list: the blank line above it separates
    /// items, not blocks, so the list stays open across it.
    List,
    /// A `>`-prefixed quote line.
    ///
    /// Continues a multi-paragraph quote: consecutive `>`-prefixed
    /// lines around a blank belong to one blockquote.
    Quote,
    /// Anything else.
    ///
    /// Starts a fresh block — a blank line above it closes whatever
    /// came before, and the scanner may settle content there.
    Other,
}

/// Classify a non-blank line's cross-blank continuation kind.
///
/// A lexical check, not a parse: leading spaces are skipped and the
/// line's first token is matched against the list and quote markers
/// the grammar accepts. The caller has already established the line
/// is non-blank.
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
    fn scrollbar_geometry_is_hidden_when_the_document_fits() {
        assert_eq!(
            scrollbar_geometry(10, 10, 0),
            None,
            "exactly full: no thumb"
        );
        assert_eq!(
            scrollbar_geometry(9, 10, 0),
            None,
            "shorter than the viewport: none"
        );
    }

    #[test]
    fn scrollbar_geometry_pins_flush_at_both_ends() {
        assert_eq!(
            scrollbar_geometry(100, 10, 0),
            Some((0, 1)),
            "at the document's start the thumb is flush with the track's top"
        );
        assert_eq!(
            scrollbar_geometry(100, 10, 90),
            Some((9, 1)),
            "at its end the thumb is flush with the track's bottom"
        );
    }

    #[test]
    fn scrollbar_geometry_sizes_the_thumb_proportionally() {
        assert_eq!(
            scrollbar_geometry(20, 10, 0),
            Some((0, 5)),
            "a document twice the viewport gives the thumb half the track"
        );
        assert_eq!(
            scrollbar_geometry(11, 10, 1),
            Some((1, 9)),
            "one scrollable line gives a near-full thumb at the far end"
        );
        let (mid_pos, thumb) = scrollbar_geometry(100, 10, 45).expect("scrollable");
        assert_eq!(
            (mid_pos, thumb),
            (4, 1),
            "the midpoint sits at the track's middle"
        );
    }

    #[test]
    fn scrollbar_geometry_clamps_an_out_of_range_skip() {
        assert_eq!(
            scrollbar_geometry(100, 10, 999),
            scrollbar_geometry(100, 10, 90),
            "a skip past the end reports the end, never past the track"
        );
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
