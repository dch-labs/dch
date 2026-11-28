//! The terminal application shell.
//!
//! `TuiApp` owns the input buffer, the conversation, and the shared
//! state background producers write through; `run` drives the
//! render ↔ input ↔ scroll loop until the user quits.

mod caches;
pub(crate) mod clipboard;
mod completion;
mod composer;
mod context;
mod copies;
mod exit;
mod keys;
pub(crate) mod notices;
mod paint;
mod permissions;
mod pulse;
mod questions;
mod queue;
mod run;
mod selection;
mod session_commands;
mod stream;
mod test_support;
mod todos;

pub use self::clipboard::CopyAnswer;

use self::caches::{BakeMeta, BakeOutputs, LaneCaches, TranscriptBake};
use self::clipboard::{CopyState, SelectionCopier};
#[cfg(test)]
use self::completion::CompletionCycle;
use self::completion::CompletionState;
use self::composer::ComposerState;
use self::context::{STATUS_BAR_AIR, STATUS_BAR_ROWS};
#[cfg(test)]
use self::copies::copy_answer_notice;
use self::copies::copy_verdict_notice;
use self::exit::ExitState;
use self::notices::{
    ToolLane, compaction_notice_lines, index_summary_rows, thinking_section_lines,
};
use self::permissions::PermissionState;
use self::questions::QuestionState;
use self::queue::QueueState;
use self::run::FramePacing;
use self::selection::{
    BakeLayout, PinIdentity, ReshapeNote, ScrollPosture, SelectionState, Stick, take_locked,
};
use self::session_commands::{ConfigWrites, Pickers, ThemeBrowse};
use self::stream::FreezeCache;
use self::todos::{ExpansionState, PlanState};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::Graduation;
use crate::cells::str_width;
use crate::events::InputQuiesce;
use crate::guidance::Guidance;
use crate::message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
use crate::observer::{CompactionEvent, HealthNotice, TuiObserverState};
use crate::queue::{ResumedTokens, SessionUpdate, Submission};
use crate::splash;
use crate::theme::Theme;
use crate::tool_render::SPINNER_FRAMES;
use crossterm::event::{Event, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use dch_config::{DchConfig, StripPosition, StripStyle, Verbosity};

use self::composer::INPUT_SIDE_INSET;
use self::exit::QuitState;
use self::notices::{NOTICE_HOLD, error_block_lines, format_duration, plain_wrapped_lines};
use self::paint::welcome::{WelcomeShape, WelcomeState};
use self::run::FRAME_INTERVAL;
use crate::markdown;
use crate::todo_view;

#[cfg(test)]
mod tests;
/// The callback fired when a turn's outcome joins the conversation.
///
/// Installed by the mode driver to persist the session transcript;
/// receives the conversation snapshot and, when a turn end arrived
/// since the last fire, the newest engine history a publication
/// landed — which only a successful turn publishes; a failed turn's
/// committed-only half is stale by design and reaches no
/// publication, so the hook sees `None` there and the host re-reads
/// the engine's state at its own resolution — persisting both
/// halves without reaching into display state.
type TurnEndHook =
    Box<dyn Fn(&[TuiMessage], Option<&Arc<Vec<loopctl::message::Message>>>) + Send + Sync>;

/// The host's wiring and the session's identity: the lanes the
/// driver installed, the session's id the status bar shows, and the
/// place the row above the composer spells.
///
/// Everything the embedding host hands the shell before the run loop
/// starts, in one place — the submit, guidance, session-update, and
/// turn-end channels, the session id with its clickable span, and
/// the working directory and branch the place row spells. A bare
/// app (tests) carries the defaults: no lanes, no identity, submits
/// that render locally.
#[derive(Default)]
pub(crate) struct HostLanes {
    /// Where submits travel to the agent.
    ///
    /// Set by the mode driver before the run loop; Enter sends each
    /// non-empty submit's id through it, the prompt itself staying in
    /// the shared queue for the driver to read at its turn's start.
    /// The channel carries the identity rather than the text because
    /// the text is what the user may still be editing when the id is
    /// handed over — which is also why a correction of a prompt that
    /// already went out carries its text: that wording has no other
    /// holder left. `None` on a bare app — submits render locally and
    /// reach no agent.
    pub(crate) submit_tx: Option<tokio::sync::mpsc::UnboundedSender<Submission>>,

    /// The running loop's steering slot, where this session has one.
    ///
    /// Installed by the host beside the submit channel. With it, `Ctrl+S`
    /// offers the composer's text to the turn already in flight instead of
    /// opening another one; without it the chord degrades to an ordinary
    /// submit, which is what a session that cannot interrupt a run should
    /// do with the key.
    pub(crate) guidance: Option<Arc<dyn Guidance>>,

    /// Notified when a turn's outcome lands in the conversation.
    ///
    /// Installed by the mode driver; receives the full conversation
    /// snapshot each time a reply graduates or a failure surfaces, so
    /// the host can persist it without reaching into display state.
    /// `None` on a bare app — turns come and go unpersisted.
    pub(crate) turn_end_hook: Option<TurnEndHook>,

    /// The channel between-turns session updates arrive on, until `run`
    /// claims it into its select.
    ///
    /// The driver owns the loop, so a `/model` switch or `/clear` that
    /// changes which model or session the loop carries is decided there and
    /// reported here; this is that one-way path. Installed by the host
    /// beside the submit channel; a standalone app (no driver) leaves it
    /// empty and never sees such an update.
    pub(crate) session_rx: Option<tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>>,

    /// The session's id, as the status bar shows it.
    ///
    /// Set by the host once the session's identity is known — the
    /// resumed file's own id, or the runner's fresh one — so the
    /// user can read (and copy out) which session they are in.
    pub(crate) session_id: Option<String>,

    /// Screen span of the session-id segment as last rendered.
    ///
    /// `(row, start column, end column)` in terminal coordinates, set
    /// during status-bar rendering — which borrows the app immutably,
    /// hence the `Cell` — and read by the click handler: a plain click
    /// inside the span copies the id through the selection transport.
    /// `None` when no id is displayed or the segment is fully clipped.
    pub(crate) session_id_span: std::cell::Cell<Option<(u16, u16, u16)>>,

    /// The working directory, as the host handed it over.
    ///
    /// Stored whole — the path as given — and spelled at paint time
    /// by the place row above the composer per the config's
    /// `path_style`: the full path or its `~`-relative shape. The
    /// string is the only holder, so a frame paints it without
    /// touching the filesystem.
    pub(crate) working_dir: Option<String>,

    /// The git branch beside the working directory.
    ///
    /// Set by the host at launch from the workdir's repository; the
    /// place row renders it in parentheses after the directory, and
    /// nothing when the workdir sits in no working tree.
    pub(crate) git_branch: Option<String>,
}

impl HostLanes {
    /// Hand `id` to the driver, reporting whether the channel took it.
    ///
    /// The send is a [`Submission::Run`]: the driver resolves the id
    /// against the shared queue when the turn comes, so this never has to
    /// take anything back.
    fn send_queued_id(&self, id: u64) -> bool {
        self.submit_tx
            .as_ref()
            .is_some_and(|tx| tx.send(Submission::Run { id }).is_ok())
    }
}

/// The display settings the session carries live: the verbosity, the
/// strip's placement, and its animation style.
///
/// The three `[display]` values a runtime command can move without a
/// restart, read every frame. The rest of the display section lives
/// on the config; these ride the app because they change mid-session
/// and every painter reads them per frame. The seat the host states
/// rides beside them — not a setting, but read like one by the canvas
/// decision.
pub(crate) struct DisplayState {
    /// The display verbosity shaping tool lines.
    ///
    /// Initialized from the config and cycled at runtime; switching
    /// invalidates the conversation and live caches so completed
    /// bodies re-render in the new mode on the next frame.
    pub(crate) verbosity: Verbosity,

    /// Where the activity strip renders while the agent runs.
    ///
    /// Carried from the display config: the spacer row directly above
    /// the composer, or a dedicated first row of the terminal. The
    /// strip's phase and gating are placement-independent.
    pub(crate) strip_position: StripPosition,

    /// The animation the activity strip plays.
    ///
    /// Carried from the display config: the centered breath, or the
    /// left-anchored fill that clears left-to-right. Style and
    /// position are orthogonal; the phase and gating are the same
    /// either way.
    pub(crate) strip_style: StripStyle,

    /// The seat the session was told it sits in, when told.
    ///
    /// `None` until a host states it; the environment's own answer —
    /// a multiplexer's spellings, or none — settles the question
    /// otherwise. A test states it to pin either seat regardless of
    /// the shell the suite runs in.
    pub(crate) terminal_seat: Option<bool>,
}

impl DisplayState {
    /// The rows a question sheet may take over the frame.
    ///
    /// The status bar's two content rows stay its own — the session
    /// line reads while an ask waits — while the footer's air is
    /// margin no pane paints, which the sheet may stand on: a deeper
    /// composer margin costs the sheet no rows it did not already
    /// spend. So does the strip's row when the strip is configured to
    /// the top of the terminal: the sheet cannot occupy a row the
    /// layout has already promised elsewhere, and a reservation that
    /// counted it too asked the solver for one row more than the frame
    /// had, leaving it to squeeze an unspecified pane to pay. The
    /// origin moves down with the strip row, so the sheet's bottom
    /// edge — anchored above the status bar — stays where it is in
    /// every strip position. Both the
    /// reservation and the box derive their height from here, so they
    /// cannot disagree about how much room there is.
    fn question_sheet_area(&self, area: Rect) -> Rect {
        let strip_rows = u16::from(matches!(self.strip_position, StripPosition::Top));
        Rect {
            y: area.y.saturating_add(strip_rows),
            height: area
                .height
                .saturating_sub(STATUS_BAR_ROWS - STATUS_BAR_AIR)
                .saturating_sub(strip_rows),
            ..area
        }
    }
}

impl Stick {
    /// Whether the posture re-anchors the view to the newest line.
    ///
    /// The one question every layout asks of the posture; callers
    /// read a boolean instead of matching the pair, so the anchor
    /// decision stays a single expression wherever the frame builds.
    #[must_use]
    fn follows(self) -> bool {
        matches!(self, Self::Follow)
    }
}

/// Which optional frame sections stand this frame.
///
/// A set, not a flag bundle: every combination occurs (the waiting
/// section and the plan row can stand together or alone), so the
/// frame asks each question it has separately — the same posture the
/// flags spelled one bit per section, kept as one value the frame
/// carries.
#[derive(Clone, Copy, Debug, Default)]
struct Sections(u8);

/// The plan section's bit.
///
/// The set's one value for the plan section: it stands while the
/// run's todo list holds any entry, and the frame asks after it
/// where the todo rows would render.
const SECTION_TODO: u8 = 0b001;

/// The waiting section's bit.
///
/// The set's one value for the waiting section: it stands whenever
/// anything waits in the queue — run or no run — exactly when the
/// panel itself renders.
const SECTION_QUEUE: u8 = 0b010;

/// The place row's bit.
///
/// The set's one value for the place row: it stands once the host
/// names a working directory, and nothing else gates it.
const SECTION_PLACE: u8 = 0b100;

impl Sections {
    /// No section stands.
    ///
    /// The frame's plain furniture only — conversation, notice,
    /// composer, status — with none of the optional sections between
    /// them; the frame starts here and adds what its questions earn.
    const EMPTY: Self = Sections(0);

    /// The set with the plan section added when it stands — the
    /// section that stands while any task does.
    #[must_use]
    const fn add_todo(self, stands: bool) -> Self {
        if stands && self.0 & SECTION_TODO == 0 {
            Sections(self.0 | SECTION_TODO)
        } else {
            self
        }
    }

    /// The set with the waiting section added when it stands — it
    /// stands whenever anything waits.
    #[must_use]
    const fn add_queue(self, stands: bool) -> Self {
        if stands && self.0 & SECTION_QUEUE == 0 {
            Sections(self.0 | SECTION_QUEUE)
        } else {
            self
        }
    }

    /// The set with the place row added when it stands — it stands
    /// once the host names a directory.
    #[must_use]
    const fn add_place(self, stands: bool) -> Self {
        if stands && self.0 & SECTION_PLACE == 0 {
            Sections(self.0 | SECTION_PLACE)
        } else {
            self
        }
    }

    /// Whether the section the bit names stands this frame.
    ///
    /// The read half of the set: the constraint walk asks after each
    /// section in its own order, and the answer is the one bit that
    /// section contributed.
    #[must_use]
    const fn stands(self, section: u8) -> bool {
        self.0 & section != 0
    }
}

/// The main TUI application.
///
/// Owns the event loop, the input buffer, and the shared state
/// background producers write through. Construct with
/// [`TuiApp::new`], which allocates the shared state fresh.
pub struct TuiApp {
    /// The active palette and element styles.
    ///
    /// Resolved from `config.display.theme` at construction; an
    /// unknown name warns and falls back to the default theme. Swapped
    /// only through [`swap_theme`](Self::swap_theme), which drops every
    /// rendering cached under the palette it replaces.
    theme: Theme,

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

    /// The copy pipeline: the seam, the verdict slot, and the ids.
    ///
    /// One state for every copy path — the selection release, the
    /// copy chords, the `/copy` command — so attempts stamp, verdicts
    /// land, and notices watermark through a single owner; see
    /// [`CopyState`] for the fields and their contracts.
    copy: CopyState,

    /// The streaming region's cache: the frozen prefix's rendered
    /// lines beside the live segment's.
    ///
    /// Everything a frame can reuse between stream deltas, owned in
    /// one place; see [`FreezeCache`] for the fields, their stamps,
    /// and the refresh that keeps them honest.
    freeze: FreezeCache,

    /// The live lane's parked rows, as the frame last rendered them.
    ///
    /// The seam, thinking, compaction, and running-tool segments the
    /// frame parks for the between-frame paths; see [`LaneCaches`]
    /// for each segment's contract.
    lane: LaneCaches,

    /// The settled conversation's render state: the cache, its
    /// incremental bake, and the index maps both feed.
    ///
    /// One owner for everything the conversation pane reuses between
    /// mutations; see [`TranscriptBake`] for the fields, the bake's
    /// append-only discipline, and the maps' rebuild contract.
    bake: TranscriptBake,

    /// The permission sheet's state: pending asks, the remembered
    /// rules, and the live cells a swap moves.
    ///
    /// One owner for the interactive permission flow's between-frame
    /// state; see [`PermissionState`] for the fields and the sheet's
    /// key handling.
    permission: PermissionState,

    /// The pinned plan section's state: the list, its deltas, and
    /// the section's posture.
    ///
    /// See [`PlanState`] for the fields and the section's scroll
    /// and posture helpers.
    plan: PlanState,

    /// Which transcript blocks render expanded, and the
    /// thinking-display preference.
    ///
    /// See [`ExpansionState`] for the toggles' keying and the
    /// auto-expansion a graduation arms.
    expansion: ExpansionState,

    /// The waiting-prompt section's state: the walk, the posture,
    /// and the keyboard mode.
    ///
    /// See [`QueueState`] for the fields and the posture flip.
    queue: QueueState,

    /// The question sheet's state: the waiting batch and the walk.
    ///
    /// See [`QuestionState`] for the fields, the sheet's view
    /// helpers, and the withdrawal retire.
    questions: QuestionState,

    /// The composer's Tab-completion state.
    ///
    /// See [`CompletionState`] for the cycle and the dismissal
    /// stamp.
    completion: CompletionState,

    /// The exit path's state: the shutdown flag, the canceller,
    /// and the cancel arm.
    ///
    /// See [`ExitState`].
    exit: ExitState,

    /// The frame lane's pacing state.
    ///
    /// See [`FramePacing`].
    pacing: FramePacing,

    /// The conversation view's scrolling posture.
    ///
    /// See [`ScrollPosture`] for the offset and the stick, and the
    /// jump helpers that move them.
    scroll: ScrollPosture,

    /// The pointer's state over the transcript.
    ///
    /// See [`SelectionState`] for the selection, the viewport
    /// geometry, and the parked drag.
    pointer: SelectionState,

    /// The theme browse's state.
    ///
    /// See [`ThemeBrowse`].
    browse: ThemeBrowse,

    /// The configuration's write-through state.
    ///
    /// See [`ConfigWrites`].
    config_writes: ConfigWrites,

    /// The composer's state: the editor, wrap geometry, and cache.
    ///
    /// See [`ComposerState`].
    composer: ComposerState,

    /// The tool lane's retained state.
    ///
    /// See [`ToolLane`].
    tools: ToolLane,

    /// The host's wiring and the session's identity.
    ///
    /// See [`HostLanes`].
    host: HostLanes,

    /// The display settings the session carries live.
    ///
    /// See [`DisplayState`].
    display: DisplayState,

    /// The floating pickers.
    ///
    /// See [`Pickers`].
    pickers: Pickers,

    /// The quit lifecycle.
    ///
    /// Exit is a deliberate chord, not a slip: the state carries
    /// both halves of it.
    quit: QuitState,

    /// The compaction rows still awaiting their summary text, oldest
    /// first.
    ///
    /// The pass's stats ride the event, but the summary itself is the
    /// compacted history's leading message, which a publication
    /// carries only after the pass settles. Each owed row keeps the
    /// pass number the observer stamped its event with beside its
    /// conversation index, and a row settles only from a publication
    /// carrying its pass number exactly — so a drain that lands
    /// several owed rows keeps them all, each waiting for its own
    /// pass, and a publication of one pass never answers another.
    compaction_summary_owed: Vec<(u64, usize)>,

    /// The live thinking block's absolute rows, when one renders.
    ///
    /// Every rendered row of the live section — the marker row and
    /// the expanded body alike — drives the Follow/Suppressed mode,
    /// matching tool blocks where any row toggles; `None` when no
    /// stream is living.
    thinking_live_rows: Option<std::ops::Range<usize>>,

    /// When the current run started, for the strip's anchor.
    ///
    /// Stamped the first frame the agent-running flag is observed
    /// set and cleared when it clears, so the bar breathes one
    /// continuous phase per run; no stream property ever moves it.
    run_anchor: Option<Instant>,

    /// The application configuration this shell was built from.
    ///
    /// The status bar reads the model name from it.
    config: DchConfig,

    /// The full view's body-row budget the layout settled on.
    ///
    /// The height pass and the render must window the plan
    /// identically or the section's rect and its content drift; the
    /// height pass stores the bound it used and the render reads it
    /// back within the same frame.
    full_body_cap: std::cell::Cell<usize>,

    /// The transient notice on the row above the composer, with
    /// the instant its hold elapses.
    ///
    /// A glance-worthy event — a cancelled run — that is not a
    /// transcript row: it shows for
    /// [`NOTICE_HOLD`](self::NOTICE_HOLD) and leaves the reserved
    /// row blank again.
    transient_notice: Option<(String, Instant)>,

    /// The welcome screen's lifecycle state — the retiring latch.
    ///
    /// Grouped as [`WelcomeState`] so the welcome layer's bookkeeping
    /// reads as one thing; see the group's own docs for what the
    /// latch does.
    welcome: WelcomeState,

    /// The band rect the last welcome frame painted, if it did.
    ///
    /// The art band's own geometry, recorded after each welcome frame:
    /// the dissolve pins and the welcome's own tests read exactly
    /// where the art stood, straight from the frame that painted it.
    welcome_band: Option<ratatui::layout::Rect>,

    /// The reader's pause handshake, wired when the run loop spawns
    /// the event source.
    ///
    /// `None` until the run loop starts — and forever in tests — so a
    /// theme switch with no reader to park defers its palette rather
    /// than query a terminal stdin is contested for.
    input_quiesce: Option<Arc<InputQuiesce>>,
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
        crate::terminal::set_splash_colors(config.display.splash_colors);
        let copy = CopyState::new(
            config.display.copy_command.clone(),
            Arc::clone(&state.render_notify),
        );
        Self {
            theme,
            conversation: Vec::new(),
            state,
            copy,
            bake: TranscriptBake::default(),
            composer: ComposerState::new(),
            tools: ToolLane::default(),
            host: HostLanes::default(),
            display: DisplayState {
                verbosity: config.display.verbosity,
                strip_position: config.display.strip_position,
                strip_style: config.display.strip_style,
                terminal_seat: None,
            },
            pickers: Pickers::default(),
            exit: ExitState::default(),
            pacing: FramePacing::default(),
            scroll: ScrollPosture::default(),
            pointer: SelectionState::default(),
            browse: ThemeBrowse::default(),
            config_writes: ConfigWrites::default(),
            queue: QueueState::default(),
            questions: QuestionState::default(),
            completion: CompletionState::default(),
            plan: PlanState::default(),
            expansion: ExpansionState::default(),
            permission: PermissionState {
                permission_session_start: boot_permission_pair(&config),
                ..PermissionState::default()
            },
            freeze: FreezeCache::default(),
            lane: LaneCaches::default(),
            quit: QuitState::default(),
            compaction_summary_owed: Vec::new(),
            thinking_live_rows: None,
            run_anchor: None,
            full_body_cap: std::cell::Cell::new(usize::MAX),
            transient_notice: None,
            welcome: WelcomeState::default(),
            welcome_band: None,
            input_quiesce: None,
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

    /// The active palette and element styles.
    ///
    /// A borrowed view for the embedding host: every painter reads the
    /// same value each frame, so a caller measuring against it sees
    /// what the next frame draws.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Swap the live theme and drop every rendering cached under the
    /// one it replaces.
    ///
    /// The host's entry point — the startup deferral that hands a
    /// terminal's own scheme back to its palette — and the public face
    /// of the replacement every swap in the crate funnels through. The
    /// bake, the merged cache, and the live lane's styled rows all
    /// re-render under the new palette on the next frame. Nothing is
    /// written to the terminal: a palette that keeps a canvas of its
    /// own owes the cursor tint and the canvas write, which stay the
    /// caller's to land, while the deferral owes neither.
    pub fn set_theme(&mut self, theme: Theme) {
        self.swap_theme(theme);
    }

    /// Replace the live theme, invalidating every cached rendering, and
    /// hand back the palette it displaced.
    ///
    /// The one place the field is assigned. The displaced palette comes
    /// back for the caller that means to restore it — a browse remembers
    /// what the session wore before the preview began. The cursor tint
    /// and the terminal's canvas are the caller's to land: they follow
    /// the palette by different routes and at different moments.
    pub(crate) fn swap_theme(&mut self, theme: Theme) -> Theme {
        self.invalidate_conversation_renders();
        std::mem::replace(&mut self.theme, theme)
    }

    /// Append a message to the conversation.
    ///
    /// The observer and the session-resume path use this to populate
    /// the view; submitting from the input line goes through the same
    /// path internally. A fold of the auto-expanded plan retires the
    /// rows it baked while the flag stood: a mid-conversation block is
    /// past the tail the incremental bake re-renders, so the fold
    /// invalidates the whole bake rather than leave an expanded shape
    /// standing on a flag it just cleared. Either way the generation
    /// moves once for the one message: the fold's rebake advances it,
    /// and a push that folded nothing advances it alone.
    pub fn push_message(&mut self, message: TuiMessage) {
        if let Some(id) = self.expansion.auto_expanded_todo.take()
            && self.expansion.expanded_tools.remove(&id)
        {
            self.note_call_reshape(&id);
            self.bake.rebake_whole();
        } else {
            self.bake.bump_generation();
        }
        self.conversation.push(message);
    }

    /// Name the session the status bar shows.
    ///
    /// The host calls this once the session's identity settles —
    /// before the run loop starts, so the first frame already
    /// carries it.
    pub fn set_session_id(&mut self, id: String) {
        self.host.session_id = Some(id);
    }

    /// Install the driver→panel channel for between-turns session updates.
    ///
    /// The driver alone holds the loop, so a `/model` switch or `/clear`
    /// changes which model and session it carries there; with this channel
    /// installed, those facts come back here to be shown. A standalone app
    /// with no driver never installs it and never sees an update.
    pub fn set_session_requests(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) {
        self.host.session_rx = Some(rx);
    }

    /// Apply one driver→panel update to the panel's own state.
    ///
    /// The status bar's two facts land here, and so does everything else
    /// the channel carries: a provider switch's settings, a resumed
    /// session's transcript, and the driver's notices. A model or provider
    /// fact is saved to the config file as it lands — the write-through's
    /// moment is the loop's verdict, never the send.
    pub fn apply_session_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::SessionId { id } => self.host.session_id = Some(id),
            SessionUpdate::ModelName { name } => {
                if let Some(file) = self.config_writes.config_file.as_mut() {
                    file.api.model.clone_from(&name);
                }
                self.reselect_model(Some(&name));
                self.publish_transcript_now();
                self.persist_config("model");
            }
            SessionUpdate::Provider {
                model,
                base_url,
                api_type,
            } => {
                if let Some(file) = self.config_writes.config_file.as_mut() {
                    file.api.model.clone_from(&model);
                }
                self.reselect_provider(model, base_url, &api_type);
                self.publish_transcript_now();
                self.persist_config("model");
            }
            SessionUpdate::Resumed {
                id,
                transcript,
                tokens,
                permission_mode,
            } => {
                self.config.runner.permission_mode = permission_mode;
                self.adopt_resumed_session(id, transcript, tokens);
            }
            SessionUpdate::PermissionMode { mode } => {
                self.config.runner.permission_mode = mode;
            }
            SessionUpdate::Notice { text } => self.post_notice(text),
            SessionUpdate::SessionCatalog { rows } => self.open_session_picker(rows),
        }
    }

    /// Re-select `requested` over the user-stated base, replacing the
    /// live `[api]`.
    ///
    /// The panel's half of the no-accretion rule: a selection layers
    /// onto the base, never onto the live config's already-selected
    /// bundle, so the second switch of a session equals a restart naming
    /// it rather than a stack of every selection so far. Only `[api]` is
    /// replaced — the live config's display-side state (a `/theme`
    /// change, the settled caches) is not the selection's to touch. A
    /// name no entry declares keeps the base bundle under the bare name,
    /// exactly as `select_model` treats the base's own model.
    fn reselect_model(&mut self, requested: Option<&str>) {
        let mut fresh = self.base_for_selection();
        if fresh.select_model(requested).is_err()
            && let Some(name) = requested
        {
            fresh.api.model = name.to_string();
        }
        self.config.api = fresh.api;
    }

    /// Mirror a cross-provider verdict over the base the same way the
    /// driver resolved it.
    ///
    /// The wire fields the update carries land first — they are the
    /// rebuild's own report — and then the entry's layer resolves over
    /// the base, so an entry's unstated fields inherit the user-stated
    /// `[api]` rather than whichever provider ran before. Only `[api]`
    /// is replaced, for the same reason [`reselect_model`](Self::reselect_model)
    /// replaces only `[api]`.
    fn reselect_provider(&mut self, model: String, base_url: String, api_type: &str) {
        let mut fresh = self.base_for_selection();
        fresh.api.model = model;
        fresh.api.base_url = base_url;
        if let Ok(kind) = serde_json::from_str::<dch_config::ApiType>(&format!("\"{api_type}\"")) {
            fresh.api.api_type = kind;
        }
        let selected = fresh.api.model.clone();
        let _selection = fresh.select_model(Some(&selected));
        self.config.api = fresh.api;
    }

    /// Adopt a session the driver loaded: transcript, queue, and counters.
    ///
    /// The display half of a live resume: the driver rebuilt the loop on
    /// the saved machine history and moved the save target onto the
    /// session's own file; this replaces what is shown with the loaded
    /// conversation — the same seeding a launch resume performs — empties
    /// the waiting queue (those prompts belonged to the conversation that
    /// just ended), releases the summary debts the replaced conversation
    /// still owed (their indexes name rows the replacement removes), and
    /// restores the session's token accounting so the counters continue
    /// the saved session's story instead of restarting at zero. The
    /// session-scoped mode override dies with the conversation it was
    /// recorded against: the adopted session's own mode stands, and
    /// `/unset permission_mode` in it must not resurrect the previous
    /// session's original. The conversation is replaced through
    /// [`Self::replace_conversation_with`], which also drops the bake
    /// and the index-keyed expansions the old rows keyed.
    fn adopt_resumed_session(
        &mut self,
        id: String,
        transcript: Vec<TuiMessage>,
        tokens: ResumedTokens,
    ) {
        self.config_writes
            .config_overrides
            .remove("permission_mode");
        self.compaction_summary_owed.clear();
        self.plan.latest_todos.clear();
        self.plan.todo_section_changes.clear();
        self.replace_conversation_with(transcript);
        self.queue.handed = None;
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        {
            let mut counts = self
                .state
                .tokens
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            counts.cumulative_input = tokens.cumulative_input;
            counts.cumulative_output = tokens.cumulative_output;
            counts.input = tokens.last_input;
            counts.context_tokens = tokens.context_tokens;
        }
        self.host.session_id = Some(id);
        self.scroll.scroll_to_bottom();
    }

    /// Replace the conversation wholesale: seed `transcript` over an
    /// empty one and drop every render that keyed on the old rows.
    ///
    /// The one seam both wholesale replacements share — a live resume
    /// adopting a saved transcript, a `/clear` starting a fresh one.
    /// The bake's reset guard keys on width and length alone, so a
    /// replacement that grows would splice the abandoned rows under
    /// the new head; the bake is dropped whole here instead. The
    /// index-keyed expansions die with the conversation that named
    /// them — a thinking section or compaction expanded by position in
    /// the old conversation is nothing the new one ever opened, and a
    /// driverless host has no adoption reply to drop them later — and
    /// the selection goes with the rows it spanned, and so does the
    /// scroll's layout-bound state — the standing reshape notes, the
    /// pin's identity, and the kept layout, all of which name rows and
    /// messages of the transcript that stood. The todo deltas keyed by
    /// the old transcript's call ids die with it too: the seeding walk
    /// records the resumed transcript's own deltas afresh over the
    /// emptied map, and a cleared session carries none.
    fn replace_conversation_with(&mut self, transcript: Vec<TuiMessage>) {
        self.conversation.clear();
        self.plan.todo_changes.clear();
        self.plan.todo_section_changes.clear();
        self.seed_messages(transcript);
        self.bake.rebake_whole();
        self.expansion.expanded_thinking.clear();
        self.expansion.expanded_compactions.clear();
        self.pointer.selection = None;
        self.scroll.reshaped_blocks.clear();
        self.scroll.pin_identity = None;
        self.scroll.settled_layout = None;
    }

    /// How long a theme switch waits for the event reader to
    /// acknowledge it is parked before giving up on the one background
    /// ask — it must exceed the reader's own poll wait by enough
    /// margin that a reader just leaving an idle poll is still seen,
    /// and enough more that a starved scheduler still lands the
    /// reader inside the window: the acknowledgement costs nothing
    /// when the reader is prompt, so the budget's whole job is the
    /// worst case.
    const QUIESCE_ACK: Duration = Duration::from_secs(2);

    /// The one mid-session ask the session is allowed: park the event
    /// reader, query the terminal while its reply cannot be stolen,
    /// and let the reader go.
    ///
    /// Only a never-asked terminal earns the ask — a proven one never
    /// reaches here (the write-only tier already landed), and a refused
    /// one has answered for the session's life. Without a reader
    /// handle (tests, headless constructors) there is nothing to park
    /// and nothing to ask: the palette defers, the honest answer for a
    /// session that cannot prove its terminal. `install` names the
    /// write's moment: the committed switch passes `false` so the
    /// canvas write lands inside the render burst that carries the
    /// re-styled cells, while the startup retry passes `true` because
    /// its landing is followed by a whole-screen repaint no early
    /// write can race.
    fn ask_background_once(&mut self, background: ratatui::style::Color, install: bool) -> bool {
        if !crate::terminal::next_ask_warranted(crate::terminal::background_support()) {
            return false;
        }
        let Some(quiesce) = self.input_quiesce.clone() else {
            return false;
        };
        quiesce.request();
        let outcome = if quiesce.wait_paused(Self::QUIESCE_ACK) {
            if install {
                crate::terminal::ask_and_install_default_background(background)
            } else {
                crate::terminal::ask_default_background(background)
            }
        } else {
            crate::terminal::QueryOutcome::default()
        };
        quiesce.release();
        if let Some(byte) = outcome.stolen {
            self.return_stolen_byte(byte);
        }
        outcome.installed
    }

    /// Ask the terminal for its default background once the event
    /// reader is up, when the startup never could.
    ///
    /// The startup query stands down whenever input is already
    /// waiting — typed-ahead keys are the user's, not ours to eat — so
    /// a session launched under a fast typist opens deferred, whatever
    /// its terminal would have answered. The palette the deferral
    /// masked is one question away: with the reader running, this asks
    /// once, under the same quiesce handshake a theme switch uses, and
    /// a terminal that answers lands the canvas the startup meant to
    /// install through [`Self::land_startup_canvas`]. A terminal that
    /// stays silent settles the refusal the startup would have recorded,
    /// and nothing moves. The answer's override is written at the
    /// question's moment rather than parked for a flush: the landing
    /// here is followed by the whole-screen repaint the caller draws,
    /// so an immediate write has no intermediate frame to race. Returns
    /// whether the canvas landed.
    fn retry_the_startup_background_ask(&mut self) -> bool {
        let Some(canvas) = self.startup_background_ask_warranted() else {
            return false;
        };
        self.ask_background_once(canvas, true) && self.land_startup_canvas()
    }

    /// The canvas a startup retry would ask for, when one is owed.
    ///
    /// `None` for a palette without an `Rgb` canvas — the paintless
    /// vocabulary defers by construction and has nothing to install —
    /// for a session the terminal has already settled: an answered
    /// terminal installed its canvas, and a refused one is never asked
    /// again this session — and for a pane inside a multiplexer, whose
    /// canvas the frame paints itself with no exchange to retry. Only
    /// the record the typed-ahead startup leaves behind — never asked
    /// — owes the question.
    fn startup_background_ask_warranted(&self) -> Option<ratatui::style::Color> {
        if crate::terminal::multiplexed_pane() {
            return None;
        }
        let canvas = self.theme.ui.background;
        if !matches!(canvas, ratatui::style::Color::Rgb(..)) {
            return None;
        }
        crate::terminal::next_ask_warranted(crate::terminal::background_support()).then_some(canvas)
    }

    /// Land the canvas a startup retry installed: the palette the
    /// deferral masked, re-resolved from the registry under its own
    /// name, the cursor tinted, and every cached line rebuilt — the
    /// caller repaints the frame whole, since every cell's ground may
    /// change. Returns whether the palette was un-masked.
    ///
    /// No registry palette carries the name when the theme is a custom
    /// literal the deferral cannot un-mask: the canvas is installed and
    /// must not stand under a palette whose inks defer to a terminal
    /// that no longer shows it, so the terminal gets its own background
    /// back and the deferred palette stays — through the plain
    /// captured restore the exit path shares, never a theme burst: the
    /// fallback opens no synchronized-output window, so it closes none.
    fn land_startup_canvas(&mut self) -> bool {
        let Some(theme) = Theme::by_name(self.theme.name) else {
            crate::terminal::restore_captured_canvas();
            return false;
        };
        self.swap_theme(theme);
        crate::terminal::set_cursor_color(self.theme.ui.primary);
        true
    }

    /// Hand back a keystroke the terminal query consumed.
    ///
    /// The pause handshake's guarantee is that asking costs latency,
    /// never input — within what one byte can say. An ASCII byte that
    /// is not a control lands in the composer as if typed; a control
    /// byte the composer has no letter for is said aloud on the notice
    /// row. A byte above ASCII is the lead of a multi-byte keystroke
    /// whose tail the query's reader never saw — no character can be
    /// built from it without inventing one the user never typed, so it
    /// is named on the notice row rather than mangled into a Latin-1
    /// look-alike. A bare Esc is the one silent case: Esc is the key
    /// users press to dismiss or abort out of habit, it names no
    /// character the composer could take, and announcing its loss
    /// teaches nothing the half-second query window did not already
    /// cost — the press is swallowed and the screen says nothing.
    fn return_stolen_byte(&mut self, byte: u8) {
        if byte == 0x1b {
            return;
        }
        match char::from_u32(u32::from(byte)).filter(|ch| ch.is_ascii() && !ch.is_control()) {
            Some(ch) => {
                self.composer.input.insert_str(&ch.to_string());
            }
            None if byte.is_ascii() => {
                self.post_notice(
                    "a key pressed during the terminal query was consumed".to_string(),
                );
            }
            None => {
                self.post_notice(
                    "a multi-byte key pressed during the terminal query was consumed".to_string(),
                );
            }
        }
    }

    /// Name the working directory the place row shows.
    ///
    /// The host calls this once at launch with the directory the
    /// runner operates in, and the row spells it per the config's
    /// [`dch_config::PathStyle`] — the full path, or the `~/`-relative shape —
    /// with the branch in parentheses when one was named.
    pub fn set_working_dir(&mut self, dir: &Path) {
        self.host.working_dir = Some(dir.display().to_string());
    }

    /// Name the git branch shown beside the working directory.
    ///
    /// The host calls this once at launch with the workdir's
    /// checked-out branch, when it sits in a working tree; the place
    /// row renders it in parentheses after the path, and no
    /// parenthesis at all when no branch was named.
    pub fn set_git_branch(&mut self, branch: Option<String>) {
        self.host.git_branch = branch;
    }

    /// Persist the conversation now, whatever the run's state.
    ///
    /// The turn-end hook usually fires on the drain that observes a
    /// turn's end — but a run wedged on a silent stream never ends,
    /// and the exit must not wait for it: this fires the same hook
    /// with the conversation as it stands (the claim rows the run
    /// already wrote) and whatever machine history is newest, so the
    /// session file carries everything the user saw at the moment of
    /// leaving. Idempotent with the drain's own firing — the worker
    /// keeps the newest save.
    pub fn flush_turn_end(&mut self) {
        if let Some(hook) = &self.host.turn_end_hook {
            let machine = self
                .state
                .machine_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            hook(&self.conversation, machine.as_ref());
        }
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
        let mut last_row_was_error = false;
        for message in &messages {
            let TuiMessage::Assistant { blocks, .. } = message else {
                if let TuiMessage::User {
                    steered: false,
                    unsent: false,
                    ..
                } = message
                    && !last_row_was_error
                {
                    self.plan.retire_finished_plan();
                }
                last_row_was_error = matches!(message, TuiMessage::Error { .. });
                continue;
            };
            last_row_was_error = false;
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
                if let ContentBlock::Tool {
                    name,
                    call_id,
                    success,
                    retained_input,
                    output_preview,
                    ..
                } = block
                    && name == "TodoWrite"
                    && *success
                    && let Some(rows) = crate::todo_view::parse_rendered(output_preview)
                        .map(|rows| {
                            crate::todo_view::rows_with_submitted_subjects(rows, retained_input)
                        })
                        .or_else(|| crate::todo_view::parse_submission(retained_input))
                {
                    let changes = crate::todo_view::status_changes(&rows, &self.plan.latest_todos);
                    self.plan
                        .todo_changes
                        .insert(call_id.clone(), changes.clone());
                    self.plan.todo_section_changes = changes;
                    self.plan.latest_todos = rows;
                    self.plan.todo_generation = self
                        .state
                        .todo_graduation_generation
                        .load(std::sync::atomic::Ordering::SeqCst);
                    self.expansion.expanded_tools.insert(call_id.clone());
                    self.expansion.auto_expanded_todo = Some(call_id.clone());
                }
            }
        }
        self.conversation.extend(messages);
        self.scroll.scroll_offset = 0;
        self.scroll.stick = Stick::Follow;
    }

    /// The input buffer's current text.
    ///
    /// Empty right after a submit; keystrokes and pastes edit it
    /// through the editor.
    #[must_use]
    pub fn input(&self) -> &str {
        self.composer.input.text()
    }

    /// The scroll offset in lines above the bottom of the view.
    ///
    /// Zero means the newest line is visible.
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll.scroll_offset
    }

    /// Whether the composer caret is in its visible blink phase.
    ///
    /// The app paints the caret itself — the terminal's own cursor
    /// stays hidden for the composer — so this is the app-side half
    /// of the blink contract: keyboard input resolidifies it, mouse
    /// traffic does not. A `static` [`dch_config::CaretDisplay`] reads as
    /// always-on here: the phase stands solid and the beat never
    /// darkens it.
    #[must_use]
    pub fn caret_visible(&self) -> bool {
        matches!(
            self.config.display.caret_display,
            dch_config::CaretDisplay::Static
        ) || self.composer.caret_blink.on
    }

    /// Whether the view follows the newest content.
    ///
    /// True while pinned; any scroll-up detaches until the view
    /// returns near the bottom.
    #[must_use]
    pub fn auto_scroll(&self) -> bool {
        self.scroll.stick.follows()
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

    /// The shared graduation queue, as the observer writes it.
    ///
    /// Exposed for tests that drive conversation arrivals the way the
    /// observer does — push a [`Graduation`] and render, exactly the
    /// path a live run takes.
    pub fn graduations(&self) -> &Arc<Mutex<Vec<crate::observer::Graduation>>> {
        &self.state.graduations
    }

    /// The shared token counters.
    ///
    /// Per-turn and cumulative usage as the observer records it; a
    /// host reads session accounting through this handle. The status
    /// bar renders no token total — the provider's usage figures do
    /// not state a conversation's size, so no honest number exists
    /// to show.
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

    /// Route submits to the agent driver.
    ///
    /// The sender half of the channel the mode driver receives on;
    /// once set, Enter queues every non-empty submit and forwards its
    /// id, and the prompt reaches the transcript when the driver
    /// claims it rather than when the key was pressed — so the row
    /// says what was sent, not what was typed. A send no receiver
    /// awaits puts the prompt back where it came from; the session is
    /// ending and nothing will claim it.
    pub fn set_submit_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<Submission>) {
        self.host.submit_tx = Some(tx);
    }

    /// Install the steering seam, or take it away.
    ///
    /// `None` is the bare-app state and the honest answer for a mode that
    /// cannot reach into a run: the chord stays bound and simply submits,
    /// rather than binding a key to nothing.
    pub fn set_guidance(&mut self, guidance: Option<Arc<dyn Guidance>>) {
        self.host.guidance = guidance;
    }

    /// Claim the pending full repaint, if one is due.
    ///
    /// One-shot: the caller (the run loop) owns acting on the claim —
    /// clearing the terminal so the next frame repaints every cell
    /// instead of diffing against a buffer the visible screen never
    /// matched.
    #[must_use]
    pub fn take_full_repaint(&mut self) -> bool {
        self.pacing.full_repaint_due.take().is_some()
    }

    /// Replace the selection copier.
    ///
    /// The instrumentation seam for the selection pins: a recorder
    /// observes what a release copies without touching any real
    /// clipboard, answering `CopyAnswer::Verified` so the notice
    /// reads as delivered. Production never calls this.
    pub fn set_selection_copier(&mut self, copier: SelectionCopier) {
        self.copy.copier = copier;
    }

    /// Switch the display verbosity.
    ///
    /// Invalidates the conversation and live caches so every
    /// completed tool body re-renders in the new mode on the next
    /// frame — the shapes differ across modes, so stale lines must
    /// not linger until the next real event.
    pub fn set_verbosity(&mut self, verbosity: Verbosity) {
        self.display.verbosity = verbosity;
        self.invalidate_conversation_renders();
    }

    /// Advance to the next verbosity mode, wrapping around.
    ///
    /// F2's handler: one press moves one step along Quiet → Normal
    /// → Verbose → Quiet, each press taking effect on the next
    /// frame through [`set_verbosity`](Self::set_verbosity)'s cache
    /// invalidation.
    fn cycle_verbosity(&mut self) {
        let next = match self.display.verbosity {
            Verbosity::Quiet => Verbosity::Normal,
            Verbosity::Normal => Verbosity::Verbose,
            Verbosity::Verbose => Verbosity::Quiet,
        };
        self.set_verbosity(next);
    }

    /// Persist the standing transcript right now, out of band.
    ///
    /// The turn-end hook is the save path of record, but a model
    /// switch changes the envelope's model field the moment it lands
    /// — the saver's relabel is already in — and the file should say
    /// so immediately, not at whatever turn ends next. This fires the
    /// same hook a turn end does, with the conversation as it stands
    /// and the engine's last committed history, so session.json is
    /// rewritten under the new label at once.
    fn publish_transcript_now(&mut self) {
        let machine = self
            .state
            .machine_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(hook) = &self.host.turn_end_hook {
            hook(&self.conversation, machine.as_ref());
        }
    }

    /// Install the callback fired when a turn ends.
    ///
    /// The hook receives the conversation snapshot at the moment a
    /// completed reply or a surfaced failure joins it — the natural
    /// save point for a session transcript. It runs inline on the
    /// render task, so heavy work must move itself off-thread.
    pub fn set_turn_end_hook(&mut self, hook: TurnEndHook) {
        self.host.turn_end_hook = Some(hook);
    }

    /// Whether the run loop's animation wake should draw this beat.
    ///
    /// The lane draws while anything on screen animates. A running
    /// turn's elapsed row — the spinner and its counting figure —
    /// ticks from the claim that starts the run to the event that
    /// lands it, thinking phases included. A tool call's stopwatch
    /// and spinner tick while the call is in flight. The activity
    /// strip rides the same beats without ever gating them: its off
    /// style paints nothing, and the elapsed row still needs its
    /// frames. A fully idle session pays for none of it.
    #[must_use]
    pub fn pulse_animation_living(&self) -> bool {
        self.any_tools_running()
            || self
                .state
                .agent_running
                .load(std::sync::atomic::Ordering::Relaxed)
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

    /// Ask every overlay that can own the keyboard, innermost first.
    ///
    /// The chain of sheets in priority order: a pending permission
    /// ask answers before a question sheet, which answers before the
    /// session picker, which answers before the waiting queue — only
    /// one of them can stand at a time in practice, and the first
    /// `Some` is the key's verdict. `None` means no sheet claims the
    /// press and the composer's own handling takes over.
    fn sheet_key(&mut self, key: KeyEvent) -> Option<bool> {
        self.permission
            .permission_key(key)
            .or_else(|| self.question_key(key))
            .or_else(|| self.sessions_key(key))
            .or_else(|| self.models_key(key))
            .or_else(|| self.queue_key(key))
    }

    /// One bare Esc: innermost first — popup, draft, then the cancel.
    ///
    /// A standing suggestion dismisses, a draft clears, and only a
    /// bare composer lets the cancel ladder engage — so Esc,Esc
    /// cancels the run only when there is nothing else to consume the
    /// press. The cancel fires through both the live runner's shared
    /// signal (the one the driver keeps current across every rebuild)
    /// and the host's installed closure. At rest the same innermost
    /// order applies with no cancel to reach.
    fn esc_under_run_or_idle(&mut self) -> bool {
        if self.esc_clears_or_dismisses() {
            return true;
        }
        if !self.state.agent_running.load(Ordering::SeqCst) {
            return false;
        }
        if self.exit.cancel_arm {
            self.exit.cancel_arm = false;
            self.fire_cancel();
            self.post_notice("Agent cancelled".to_string());
            return true;
        }
        self.exit.cancel_arm = true;
        self.post_notice("press Esc again to cancel the run".to_string());
        true
    }

    /// Retire the handoff hide once its prompt left the queue.
    ///
    /// A claimed or dropped prompt is gone from the shared queue, and
    /// the window its hide justified is over. A prompt still waiting
    /// keeps its hide — that shape means a driver that stopped
    /// claiming, and the next submit clears the field before its own
    /// push, so at most one row hides at a time.
    fn settle_handed(&mut self) {
        if let Some(id) = self.queue.handed
            && self.queued_index_of(id).is_none()
        {
            self.queue.handed = None;
        }
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

    /// Jump the viewport to the previous or next submitted prompt.
    ///
    /// Alt+Up and Alt+Down walk the user's own prompts — steered
    /// echoes and unsent drafts are not stops — seating the target
    /// prompt's first text row at the top of the viewport, Up the
    /// older direction as it is everywhere else in the scroll model.
    /// A jump holds the stick; one that lands within the follow
    /// tolerance rearms it, so walking back to the newest prompt
    /// returns the view to live tailing. Past the ends the walk
    /// refuses with a notice naming the boundary it stands on.
    fn jump_user_message(&mut self, older: bool) {
        let top = self
            .pacing
            .last_layout_lines
            .saturating_sub(self.pacing.last_layout_viewport)
            .saturating_sub(self.scroll.scroll_offset);
        let target = if older {
            self.bake
                .user_prompt_marks
                .iter()
                .rev()
                .find(|&&mark| mark < top)
        } else {
            self.bake.user_prompt_marks.iter().find(|&&mark| mark > top)
        };
        let Some(&mark) = target else {
            self.post_notice(
                if older {
                    "no earlier prompt — already at the first one"
                } else {
                    "no later prompt — already at the newest one"
                }
                .to_string(),
            );
            return;
        };
        self.keep_the_seen_layout();
        self.scroll.scroll_offset = self
            .pacing
            .last_layout_lines
            .saturating_sub(self.pacing.last_layout_viewport)
            .saturating_sub(mark);
        self.scroll.stick = Stick::Hold;
        self.scroll.rearm_if_near_bottom();
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
            self.pacing.render_pending = false;
            self.pacing.last_frame = Some(now);
            return true;
        }
        let due = self
            .pacing
            .last_frame
            .is_none_or(|last| now.duration_since(last) >= FRAME_INTERVAL);
        if due {
            self.pacing.last_frame = Some(now);
            self.pacing.render_pending = false;
        } else {
            self.pacing.render_pending = true;
        }
        due
    }

    /// Claim a parked redraw for the periodic tick.
    ///
    /// Returns whether a capped background request was waiting;
    /// claiming stamps the frame time so the interval restarts. The
    /// claim does not itself re-check the interval — a parked request
    /// is owed its frame whenever the tick lands, which can sit inside
    /// the interval after a notify-initiated frame; the tick stays
    /// several frame intervals wide, so the reading is deliberate.
    /// Ticks with nothing pending and no tools in flight still draw
    /// nothing. The run loop asks the pacing state directly; the
    /// integration suites ask through here.
    pub fn take_pending_redraw(&mut self, now: Instant) -> bool {
        self.pacing.take_pending_redraw(now)
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
    /// reports — parked outright while a permission ask is pending —
    /// and flips the composer caret's blink phase on its
    /// own beat. An idle tick between flips and away from all of the
    /// above draws nothing.
    pub fn tick_wake(&mut self, now: Instant) -> bool {
        let claimed = self.pacing.take_pending_redraw(now);
        let dragged = if self.permission.pending_permissions.front().is_some() {
            self.pointer.drag_position = None;
            false
        } else {
            self.drag_autoscroll()
        };
        let blinked = self.composer.caret_blink.refresh(now);
        let notice_lapsed = self
            .transient_notice
            .as_ref()
            .is_some_and(|(_, until)| *until <= now);
        if notice_lapsed {
            self.transient_notice = None;
        }
        let health_lapsed = {
            let mut health = self
                .state
                .health
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match *health {
                HealthNotice::Reconnected { at } => {
                    let lapsed = at
                        .checked_add(NOTICE_HOLD)
                        .is_some_and(|until| until <= now);
                    if lapsed {
                        *health = HealthNotice::Idle;
                    }
                    lapsed
                }
                _ => false,
            }
        };
        let prompts_retired = !self.permission.pending_permissions.is_empty()
            && !self
                .state
                .agent_running
                .load(std::sync::atomic::Ordering::SeqCst);
        if prompts_retired {
            self.permission.pending_permissions.clear();
            // The sheet's own entry state retires with it: a reason
            // draft half-typed or a command preview left expanded
            // would greet the next ask as if the retired sheet's keys
            // had carried over.
            self.permission.permission_reason = None;
            self.permission.permission_command_expanded = false;
        }
        let questions_retired = !self.questions.pending_questions.is_empty()
            && !self
                .state
                .agent_running
                .load(std::sync::atomic::Ordering::SeqCst);
        if questions_retired {
            self.questions.pending_questions.clear();
            self.questions.clamp_question_view();
        }
        self.any_tools_running()
            || claimed
            || dragged
            || blinked
            || notice_lapsed
            || health_lapsed
            || prompts_retired
            || questions_retired
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

    /// Split the frame's rows: the constraints top to bottom, and the
    /// reservation a pending ask holds out of the conversation.
    ///
    /// Every optional section — the plan, the waiting prompts, the
    /// ask's reserved rows, the place row — takes only the rows it
    /// paints, and each stands above the furniture below it in the
    /// order the frame reads: the plan and the waiting prompts ride
    /// above the ask's sheet, so a tall ask meets their bottom edge
    /// rather than painting through them.
    fn frame_constraints(&self, layout: FrameLayout) -> (Vec<Constraint>, u16) {
        let FrameLayout {
            area,
            composer_height,
            strip_position,
            question_page,
            sections,
        } = layout;
        let strip_at_top = matches!(strip_position, StripPosition::Top);
        let queue_shown = sections.stands(SECTION_QUEUE);
        let todo_shown = sections.stands(SECTION_TODO);
        let place_shown = sections.stands(SECTION_PLACE);
        let ruled = self.theme.ui.composer_border.is_some();
        let question_reserved = if let Some(page) = question_page {
            let sheet = self.question_sheet_height(
                area.width,
                self.display.question_sheet_area(area).height,
                page,
            );
            sheet
                .saturating_sub(composer_height)
                .saturating_sub(2)
                .saturating_sub(STATUS_BAR_AIR)
        } else {
            0
        };
        let mut constraints = Vec::with_capacity(8);
        if strip_at_top {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(0));
        if todo_shown {
            constraints.push(Constraint::Length(
                self.todo_section_height(area.width, area.height),
            ));
        }
        if queue_shown {
            constraints.push(Constraint::Length(self.queue_section_height(area.width)));
        }
        if question_reserved > 0 {
            constraints.push(Constraint::Length(question_reserved));
        }

        if !place_shown {
            constraints.push(Constraint::Length(1));
        }
        if ruled && place_shown {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(1));
        if !ruled && place_shown {
            constraints.push(Constraint::Length(1));
        }
        constraints.extend([
            Constraint::Length(composer_height),
            Constraint::Length(STATUS_BAR_ROWS),
        ]);
        (constraints, question_reserved)
    }

    /// The welcome composer's widest block, in terminal cells.
    ///
    /// The artless fallback's cap — the block the box takes when no
    /// scale fits the pane at all; wherever art stands, the box takes
    /// the art's own width instead.
    const WELCOME_BLOCK_MAX_WIDTH: u16 = 220;

    /// The words an empty welcome composer murmurs.
    ///
    /// The name itself, in the night colors the picture wears,
    /// riding the field's first text row until the first keystroke
    /// stands it down.
    const WELCOME_PLACEHOLDER: [&str; 3] = ["dream,", "create,", "harness"];

    /// The night-sky color each word of the name wears.
    ///
    /// Starlight gold for the first word, moonlit blue for the
    /// second, twilight lavender for the third — three registers
    /// that read as light in the sky rather than ink on it, whatever
    /// the theme.
    const WELCOME_WORD_COLORS: [ratatui::style::Color; 3] = [
        ratatui::style::Color::Rgb(232, 203, 122),
        ratatui::style::Color::Rgb(150, 180, 222),
        ratatui::style::Color::Rgb(180, 154, 216),
    ];

    /// The most rows the welcome's art band spends.
    ///
    /// The band is the welcome's backdrop, not its tenant: the pane's
    /// own row floor keeps the frame the session's, and the cap only
    /// names the tallest embedded rung so a future taller asset cannot
    /// stretch the band past it. The picture is shown whole or not at
    /// all — never windowed to its richest rows, which cut the water
    /// from the fjord's foot.
    const WELCOME_BAND_MAX_ROWS: u16 = 29;

    /// The give-way lane's ceiling: the w54 rung's band top.
    ///
    /// The largest rung a shrunken margin may seat: above it, the
    /// fraction rules and the margins are a luxury of width again —
    /// so a narrow pane spends its columns on the picture while a
    /// roomy one keeps its proportions, and the seat never shrinks as
    /// the pane widens.
    const NARROW_LANE_MAX_WIDTH: u16 = 55;

    /// The fresh session's welcome geometry, when this frame takes it.
    ///
    /// The welcome shape belongs to a conversation with nothing said
    /// and nothing in flight — no queued prompts, no plan rows, no
    /// pending ask, and no streamed or running bytes, for a run whose
    /// first output has not landed yet still belongs to the docked
    /// layout — and only before the conversation has ever held rows,
    /// so `/clear` never brings the fjord back after a session
    /// happened. When the frame qualifies, the geometry names one
    /// compact group — the splash scale the block holds, the block's
    /// width, and the rows the band above the art spends — with the
    /// art shown whole, bounded by the pane's row floor so the welcome
    /// never fills the screen; `None` leaves the frame to the docked
    /// layout.
    fn welcome_shape(&self, area: Rect) -> Option<WelcomeShape> {
        let streaming_empty = self
            .streaming_text()
            .lock()
            .is_ok_and(|text| text.is_empty());
        let welcome = self.conversation.is_empty()
            && !self.welcome.welcome_retired
            && streaming_empty
            && self.queued_display_len() == 0
            && self.plan.latest_todos.is_empty()
            && !self.state.agent_running.load(Ordering::Relaxed)
            && self.permission.pending_permissions.is_empty()
            && self.questions.pending_questions.is_empty()
            && self.freeze.frozen_lines.is_empty()
            && self.freeze.live_lines.is_empty();
        if !welcome {
            return None;
        }
        let row_floor = area.height.saturating_sub(12);
        let row_budget = row_floor.min(Self::WELCOME_BAND_MAX_ROWS);
        let width_budget = area.width.saturating_sub(8).saturating_mul(3) / 4;
        let narrow_budget = area
            .width
            .saturating_sub(4)
            .min(Self::NARROW_LANE_MAX_WIDTH);
        let width_budget = width_budget.max(narrow_budget);
        let art = splash::rungs().iter().find(|entry| {
            !entry.lines.is_empty() && entry.width <= width_budget && entry.height <= row_budget
        });
        let (art, rows_shown, row_skip) = match art {
            Some(scale) => (Some(scale), row_budget.min(scale.height), 0),
            None => (None, 0, 0),
        };
        let fallback_block = area
            .width
            .saturating_sub(8)
            .min(Self::WELCOME_BLOCK_MAX_WIDTH);
        let block_width = art.map_or(fallback_block, |scale| scale.width);
        let band_height = art.map_or(0, |_| rows_shown);
        Some(WelcomeShape {
            art,
            block_width,
            band_height,
            rows_shown,
            row_skip,
        })
    }

    /// Settle the run timer's anchor against the running flag.
    ///
    /// A running turn arms the anchor once — the first frame of the run
    /// fixes its start — and a run at rest clears it, so the elapsed
    /// the status bar shows never carries a stale anchor across runs.
    fn settle_run_anchor(&mut self) {
        if self.state.agent_running.load(Ordering::Relaxed) {
            self.run_anchor.get_or_insert_with(Instant::now);
        } else {
            self.run_anchor = None;
        }
    }

    /// Render one frame of the layout.
    ///
    /// Conversation fills the space above a blank spacer row that
    /// separates it from the input field; the place row — the
    /// working directory, once the host names one — sits between
    /// that spacer and the composer, and the three-row footer closes
    /// the frame at the bottom — each pane renders through its own
    /// method. The input field grows with its buffer up to its
    /// text-row cap and then holds: the conversation pane reflows
    /// while the composer grows and holds still once it has.
    ///
    /// The layout painted, the frame is grounded when its canvas must
    /// be its own: the grounding walk after the layout paints the
    /// theme canvas under every cell the widgets left on the terminal
    /// default, which a pane inside a multiplexer cannot lean on.
    pub fn render(&mut self, frame: &mut Frame) {
        self.render_layout(frame);
        self.ground_frame_canvas(frame);
    }

    /// Whether a frame must paint its canvas under every cell itself.
    ///
    /// A frame whose canvas is in force but not otherwise guaranteed:
    /// a pane inside a multiplexer (the multiplexer hands default
    /// cells to the client as the client's own default), or a direct
    /// terminal the canvas landed on through the `OSC 11` exchange
    /// and still carries it — the set reprograms the default, but
    /// the terminal's own profile, padding, and any moment the set
    /// has not reached (a refused reply, a slow answer, a switch
    /// mid-flight) all still show through cells left on the default,
    /// so the frame paints what it means: every blank row, tail, and
    /// margin carries the canvas as an explicit background, and the
    /// set remains only as the chrome outside the frame — padding,
    /// the fraction of a cell — joining the same color. An answered
    /// terminal whose canvas was taken back — the startup fallback
    /// restored its own background under a palette the registry
    /// cannot un-mask — keeps the restored scheme: the installed
    /// flag carries that half. A terminal the exchange never landed
    /// on keeps its own scheme: the deferred palette paints no
    /// canvas, and nothing of ours may cover the terminal the user
    /// configured. Only an `Rgb` canvas is grounded — the transparent
    /// palette's whole contract is the terminal's own default.
    pub(crate) fn canvas_ground_warranted(
        multiplexed: bool,
        support: crate::terminal::BackgroundSupport,
        canvas: ratatui::style::Color,
        installed: bool,
    ) -> bool {
        (multiplexed || (support == crate::terminal::BackgroundSupport::Answered && installed))
            && matches!(canvas, ratatui::style::Color::Rgb(..))
    }

    /// State the seat this session sits in: a pane inside a terminal
    /// multiplexer, or a direct terminal.
    ///
    /// Unstated, the environment settles it — the multiplexers sign
    /// their panes (`TMUX`, `STY`, `ZELLIJ`, a `tmux`- or `screen`-typed
    /// `TERM`), and a host that already knows better may say so here.
    /// A test states it to pin either seat's rendering regardless of
    /// the shell the suite runs in. Stated once, it stands for the
    /// session's life. The statement governs the frame's grounding
    /// walk alone, and the one combination it cannot cover is a
    /// deferred palette behind a stated pane seat: the environment
    /// never produces it — a pane runs no exchange, so nothing of its
    /// ever defers — and the walk reads the canvas alone, so the `Rgb`
    /// canvas a deferred palette still carries is grounded exactly as
    /// any other. A host seating a session whose palette deferred
    /// accepts that the canvas paints.
    pub fn set_terminal_seat(&mut self, multiplexed: bool) {
        self.display.terminal_seat = Some(multiplexed);
    }

    /// Whether the frame's canvas is in force on the seat this
    /// session sits in.
    ///
    /// The grounding walk and the welcome's edge dissolve consult the
    /// same answer: a pane paints its canvas under its own cells, and
    /// so does a direct terminal the exchange answered — every other
    /// seat keeps the terminal's own scheme as the ground, and
    /// nothing of ours may paint or blend toward a canvas the
    /// terminal is not showing. The `Rgb` leg rides along because the
    /// transparent palette's whole contract is the terminal's own
    /// default.
    fn canvas_in_force(&self) -> bool {
        let multiplexed = self
            .display
            .terminal_seat
            .unwrap_or_else(crate::terminal::multiplexed_pane);
        Self::canvas_ground_warranted(
            multiplexed,
            crate::terminal::background_support(),
            self.theme.ui.background,
            crate::terminal::canvas_installed(),
        )
    }

    /// Paint the canvas under every cell the widgets left on the
    /// terminal default — when the canvas-ground warrant says so.
    ///
    /// Runs after the whole layout, so every default cell is covered
    /// whatever painted or cleared it: a `Clear` that stood a sheet
    /// down, a pulse strip that yielded its row, the empty rows of a
    /// short conversation. Cells that carry their own background — the
    /// composer's surface, a badge's fill, the art of the splash — keep
    /// it; the walk touches the default alone, and the diff emits only
    /// the cells whose ground actually changed.
    fn ground_frame_canvas(&self, frame: &mut Frame) {
        let canvas = self.theme.ui.background;
        if !self.canvas_in_force() {
            return;
        }
        ground_default_cells(frame.buffer_mut(), canvas);
    }

    /// The layout itself: the panes, furniture, and overlays of one frame.
    ///
    /// Settles the between-frame state the layout reads, resolves the
    /// composer's height, and splits the area into the conversation
    /// pane, the optional sections (plan, waiting queue, place row),
    /// the notice row, the composer, and the status bar — each painted
    /// through its own method, with the overlays standing over the
    /// result. The welcome screen, while it stands, replaces the whole
    /// layout. Grounding the canvas under the cells the widgets leave
    /// on the terminal default is [`render`](Self::render)'s closing
    /// act, kept out of this pass so it runs after every overlay too.
    fn render_layout(&mut self, frame: &mut Frame) {
        self.settle_handed();
        self.settle_todo_expansion();
        self.drain_shared_state();
        self.sync_queue_view();
        let area = frame.area();
        if !self.welcome.welcome_retired && !self.conversation.is_empty() {
            self.welcome.welcome_retired = true;
            self.pacing.full_repaint_due = Some(Instant::now());
        }
        if let Some(shape) = self.welcome_shape(area) {
            let wrap_width = shape.inner_width();
            let (rows, caret) = self.composer.composer_wrap(wrap_width);
            let composer_height = Self::composer_height_for(&rows);
            self.composer.input_wrap_width = wrap_width;
            self.render_welcome(frame, area, &shape, &rows, caret, composer_height);
            return;
        }
        self.welcome_band = None;
        let wrap_width = area
            .width
            .max(1)
            .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2));
        let (rows, caret) = self.composer.composer_wrap(wrap_width);
        let composer_height = Self::composer_height_for(&rows);
        let strip_at_top = matches!(self.display.strip_position, StripPosition::Top);
        let question_page = self
            .questions
            .pending_questions
            .len()
            .checked_sub(1)
            .filter(|_| self.permission.pending_permissions.is_empty());
        let queue_shown = self.queued_display_len() > 0;
        let todo_shown = !self.plan.latest_todos.is_empty();
        let place_shown = self.host.working_dir.is_some();
        let (constraints, question_reserved) = self.frame_constraints(FrameLayout {
            area,
            composer_height,
            strip_position: self.display.strip_position,
            question_page,
            sections: Sections::EMPTY
                .add_todo(todo_shown)
                .add_queue(queue_shown)
                .add_place(place_shown),
        });
        let base = usize::from(strip_at_top);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let todo_at = usize::from(todo_shown);
        let furniture = base
            .saturating_add(todo_at)
            .saturating_add(usize::from(question_reserved > 0))
            .saturating_add(usize::from(queue_shown));
        let fallback = area;
        self.render_conversation(frame, pane(&chunks, base, fallback));
        if todo_shown {
            self.render_todo_section(frame, pane(&chunks, base.saturating_add(1), fallback));
        } else {
            self.plan.todo_section_rect = Rect::default();
        }
        if queue_shown {
            self.render_queue(
                frame,
                pane(
                    &chunks,
                    base.saturating_add(1).saturating_add(todo_at),
                    fallback,
                ),
            );
        } else {
            self.queue.queue_section_rect = Rect::default();
        }
        let notice_rows = usize::from(!place_shown);
        if notice_rows > 0 {
            self.render_notice(frame, pane(&chunks, furniture.saturating_add(1), fallback));
        }
        self.settle_run_anchor();
        let caption_above_rule = self.theme.ui.composer_border.is_some() && place_shown;
        let below_notice = furniture.saturating_add(1).saturating_add(notice_rows);
        let seat_rows = usize::from(place_shown);
        let (spacer_at, place_row_at) = if caption_above_rule {
            (below_notice.saturating_add(seat_rows), below_notice)
        } else {
            (below_notice, below_notice.saturating_add(seat_rows))
        };
        let composer_at = below_notice.saturating_add(seat_rows).saturating_add(1);
        let strip_row = if strip_at_top { 0 } else { spacer_at };
        let strip_drew = self.render_pulse_strip(frame, pane(&chunks, strip_row, fallback));
        let strip_owns_spacer = strip_drew && !strip_at_top;
        if place_shown {
            self.render_place_row(frame, pane(&chunks, place_row_at, fallback));
        }
        if let Some(color) = self.theme.ui.composer_border {
            let spacer = pane(&chunks, spacer_at, fallback);
            let composer = pane(&chunks, composer_at, fallback);
            let rule = Style::default()
                .fg(color)
                .add_modifier(ratatui::style::Modifier::UNDERLINED);
            if spacer.height > 0 && !strip_owns_spacer {
                frame.buffer_mut().set_style(spacer, rule);
            }
            if composer.height > 0 {
                let rule_row = if composer.height >= 2 {
                    composer.bottom().saturating_sub(2)
                } else {
                    composer.bottom().saturating_sub(1)
                };
                let bottom = Rect {
                    y: rule_row,
                    height: 1,
                    ..composer
                };
                frame.buffer_mut().set_style(bottom, rule);
            }
        }
        let input_area = pane(&chunks, composer_at, fallback);
        self.composer.input_wrap_width = wrap_width;
        self.render_input(frame, input_area, &rows, caret);
        self.render_completion(frame, input_area, true);
        self.render_session_picker(frame, input_area, true);
        self.render_model_picker(frame, input_area, true);
        self.render_status_bar(
            frame,
            pane(&chunks, composer_at.saturating_add(1), fallback),
        );
        if let Some(sheet) = self.permission.shown_permission() {
            self.render_permission_overlay(frame, area, &sheet);
            return;
        }
        if self.permission.pending_permissions.is_empty()
            && !self.questions.pending_questions.is_empty()
        {
            self.render_question_overlay(
                frame,
                area,
                self.questions.pending_questions.len().saturating_sub(1),
            );
        }
    }

    /// Bake the conversation cache incrementally.
    ///
    /// Messages are immutable once a newer one follows — only the
    /// last is ever patched, a run's duration landing on it — so the
    /// cache bakes each message once and a graduation re-renders only
    /// the tail. A full-transcript rebuild cost seconds on
    /// markdown-heavy sessions, freezing the frame — spinner included
    /// — on every tool result that landed. Width changes and
    /// truncations below the bake are the two shapes that invalidate
    /// it. The tail renders alone, so its row indexes are
    /// tail-relative; the frame's merged view products are rebuilt
    /// with the tail rebased by the bake's line count, and the bake's
    /// own metadata lives on the bake's `conversation_bake_meta`
    /// field.
    fn bake_conversation_cache(&mut self, text_area: Rect, width: u16) {
        if self.bake.conversation_cache_width != width
            || self.conversation.len() < self.bake.conversation_baked_through
        {
            self.bake.conversation_cache.clear();
            self.bake.conversation_bake.clear();
            self.bake.conversation_bake_meta = BakeMeta::default();
            self.bake.conversation_baked_through = 0;
            self.bake.conversation_baked_prev_user = None;
            self.bake.conversation_cache_width = width;
        }
        // Every rebuild forfeits the selection, as it always did:
        // line indexes shift under it.
        self.pointer.selection = None;
        self.pointer.drag_position = None;
        let bake_end = self.conversation.len().saturating_sub(1);
        if bake_end > self.bake.conversation_baked_through {
            let mut prev_user = self.bake.conversation_baked_prev_user;
            let baked_start = self.bake.conversation_baked_through;
            let mut baked = BakeOutputs {
                lines: std::mem::take(&mut self.bake.conversation_bake),
                ..BakeOutputs::default()
            };
            self.conversation_lines(text_area, baked_start, bake_end, &mut prev_user, &mut baked);
            self.bake.conversation_baked_prev_user = prev_user;
            self.bake.conversation_baked_through = bake_end;
            self.bake.conversation_bake = baked.lines;
            self.bake.conversation_bake_meta.adopt(baked.meta);
        }
        let mut tail = BakeOutputs::default();
        let mut tail_prev = self.bake.conversation_baked_prev_user;
        let tail_start = self.conversation.len().saturating_sub(1);
        self.conversation_lines(
            text_area,
            tail_start,
            self.conversation.len(),
            &mut tail_prev,
            &mut tail,
        );
        let mut cache = self.bake.conversation_bake.clone();
        cache.extend(tail.lines);
        self.bake.conversation_cache = cache;
        let tail_base = self.bake.conversation_bake.len();
        let merged = self
            .bake
            .conversation_bake_meta
            .clone()
            .merged_over(tail.meta, tail_base);
        self.bake.tool_summary_lines = merged.tool_summary_lines;
        self.bake.thinking_summary_lines = merged.thinking_summary_lines;
        self.bake.compaction_summary_lines = merged.compaction_summary_lines;
        self.bake.user_turn_ranges = merged.user_turn_ranges;
        self.bake.user_prompt_marks = merged.user_prompt_marks;
        self.bake.todo_block_ranges = merged.todo_block_ranges;
        self.bake.message_ranges = merged.message_ranges;
        self.bake.message_units = merged.message_units;
        self.bake.conversation_cache_generation = self.bake.conversation_generation;
    }

    /// The running turn's live elapsed line, when one runs.
    ///
    /// The completed reply prints its total duration as a dim `duration`
    /// line at its foot; the running turn owes the same line while it
    /// runs — the total so far, the same figure format, the same dim
    /// register — named by a quiet `elapsed` word while it still
    /// counts, the `duration` word arriving only when the total is
    /// final. A breathing row of margin stands above the line, so the
    /// ticking figure never touches the streaming text it closes. A
    /// spinner dot leads it in the accent — the same braille orbit,
    /// the same animation beat, as the running tools' own indicators,
    /// so everything in flight pulses together. Empty while nothing
    /// runs: between turns nothing ticks.
    fn live_run_timer(&self) -> Vec<Line<'static>> {
        let started = self
            .state
            .run_started_ms
            .load(std::sync::atomic::Ordering::SeqCst);
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::SeqCst)
            || started == 0
        {
            return Vec::new();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let elapsed = now.saturating_sub(started);
        let frame_index = usize::try_from(
            elapsed
                .checked_div(crate::tool_render::SPINNER_FRAME_MS)
                .unwrap_or(0),
        )
        .unwrap_or(0)
        .checked_rem(SPINNER_FRAMES.len())
        .unwrap_or(0);
        let frame = SPINNER_FRAMES
            .get(frame_index)
            .or_else(|| SPINNER_FRAMES.first())
            .copied()
            .unwrap_or("");
        vec![
            Line::from(Vec::new()),
            Line::from(vec![
                Span::styled(
                    frame.to_string(),
                    Style::default().fg(self.theme.ui.primary),
                ),
                Span::styled(
                    format!(" elapsed {}", format_duration(elapsed)),
                    self.theme.ui.dim,
                ),
            ]),
        ]
    }

    /// Keep the layout the reader's first move off the tail is queued
    /// against.
    ///
    /// While the view follows the tail the settle keeps no layout — nothing
    /// is pinned to map across a rewrap. A move that detaches asks for a
    /// row of the frame just shown, so that frame's layout is kept here,
    /// once, for the settle to read the requested row's place in should
    /// the detaching frame also change width; a frame still following
    /// clears it again, and the detached settle replaces it.
    fn keep_the_seen_layout(&mut self) {
        if !self.scroll.stick.follows() {
            return;
        }
        self.scroll.settled_layout = Some(Box::new(BakeLayout {
            ranges: self.bake.message_ranges.clone(),
            units: self.bake.message_units.clone(),
        }));
    }

    /// Settle the scroll offset for this frame's layout.
    ///
    /// A pinned view sits at the newest line: the offset resets to
    /// zero. A detached view holds a *pin* — the row at the top of the
    /// view — and the offset is re-derived from it every frame:
    /// `total − viewport − pin`, saturating at zero. So the view survives
    /// whatever the frame does to its geometry: content streaming in
    /// below the pin or collapsing away above or below it (rows a fold
    /// moves *above* the pin move the pin itself,
    /// [`rows_moved_above_anchor`](Self::rows_moved_above_anchor)), and a
    /// viewport that changes under the view — a pane tmux resized while
    /// it was hidden. Rows reshaped where no keyed block stands to note
    /// them — a retry line rewritten in place, a verbosity or theme
    /// switch re-rendering every block, a transcript replaced wholesale —
    /// are the accepted windows: the first two move the pinned view with
    /// the document, and the last resets the offset outright. The
    /// saturation can only deny the pin for a frame,
    /// never lose it: the pin is remembered while the offset cannot reach
    /// it, and the offset restores it when the geometry allows again. A
    /// pane that changes *width* rewraps every message, so the pin is also
    /// kept as a place in the transcript — message, unit, offset
    /// ([`PinIdentity`]) — and mapped onto the rebaked rows, where its row
    /// index alone would name a different line.
    fn settle_scroll_offset(&mut self, total_lines: usize, viewport: usize) {
        let width = usize::from(self.bake.conversation_cache_width);
        if self.scroll.stick.follows() {
            self.scroll.scroll_offset = 0;
            self.scroll.anchored_row = None;
            self.scroll.settled_offset = 0;
            self.scroll.pin_identity = None;
            self.scroll.reshaped_blocks.clear();
            self.scroll.settled_layout = None;
            self.scroll.settled_width = width;
        } else {
            let reader_moved = self.scroll.scroll_offset != self.scroll.settled_offset;
            let base = self.pin_base();
            let requested = self.scroll.pin_after_reader_move(base);
            let (landed_above, left_above) = self.rows_moved_above_anchor();
            let pin = requested
                .saturating_add(landed_above)
                .saturating_sub(left_above);
            let mut retained = None;
            let pin = if width == self.scroll.settled_width {
                pin
            } else if reader_moved {
                let layout = self.scroll.settled_layout.as_deref();
                match layout.and_then(|layout| {
                    Self::pin_identity_in(requested, &layout.ranges, &layout.units)
                }) {
                    Some(identity) => match self.bake.pin_row_after_rewrap(identity) {
                        Some(row) => {
                            retained = Some(identity);
                            row
                        }
                        None => pin,
                    },
                    None => pin,
                }
            } else {
                match self.scroll.pin_identity {
                    Some(identity) => self.bake.pin_row_after_rewrap(identity).unwrap_or(pin),
                    None => {
                        match self
                            .scroll
                            .settled_layout
                            .as_deref()
                            .and_then(|layout| {
                                Self::pin_identity_in(requested, &layout.ranges, &layout.units)
                            })
                            .and_then(|identity| {
                                self.bake
                                    .pin_row_after_rewrap(identity)
                                    .map(|row| (identity, row))
                            }) {
                            Some((identity, row)) => {
                                retained = Some(identity);
                                row
                            }
                            None => pin,
                        }
                    }
                }
            };
            self.scroll.scroll_offset = total_lines.saturating_sub(viewport).saturating_sub(pin);
            self.scroll.anchored_row = Some(pin);
            self.scroll.settled_offset = self.scroll.scroll_offset;
            let pinned_unit_changed = match (&self.scroll.pin_identity, self.pin_identity_at(pin)) {
                (Some(kept), Some(now)) => {
                    kept.message != now.message || kept.block != now.block || kept.unit != now.unit
                }
                (Some(_), None) => true,
                (None, _) => false,
            };
            if reader_moved || pinned_unit_changed || self.scroll.pin_identity.is_none() {
                self.scroll.pin_identity = retained.or_else(|| self.pin_identity_at(pin));
            }
            self.scroll.settled_width = width;
            self.scroll.settled_layout = Some(Box::new(BakeLayout {
                ranges: self.bake.message_ranges.clone(),
                units: self.bake.message_units.clone(),
            }));
        }
        self.pacing.last_layout_lines = total_lines;
        self.pacing.last_layout_viewport = viewport;
    }

    /// The row the settle and [`pinned_row`](Self::pinned_row) move the pin
    /// from, before the reader's queued notches.
    ///
    /// The remembered pin — or, once the reader has moved and the last
    /// frame could not reach it (the offset saturated), the row that frame
    /// showed: a notch means a row of the view the reader sees, not of a
    /// pin hidden below its range. The base is never clamped to the frame
    /// in hand: the new range limits only the offset, so a requested row
    /// beyond it is denied and remembered — as any pin the offset cannot
    /// reach is — until the geometry reaches it. Both callers take this
    /// base, so a reshape noted ahead of a settle counts its rows against
    /// the pin the settle will move — a note taken against a hidden
    /// remembered pin reads a summary the frame shows below the view's top
    /// as wholly above the pin, and the settle then moves the view by the
    /// block's growth.
    fn pin_base(&self) -> usize {
        let reader_moved = self.scroll.scroll_offset != self.scroll.settled_offset;
        if reader_moved && self.remembered_pin() > self.last_frame_top_row() {
            self.last_frame_top_row()
        } else {
            self.remembered_pin()
        }
    }

    /// The row the detached view's top is on, in the last settle's
    /// coordinates: [`pin_base`](Self::pin_base), moved by whatever has
    /// happened to the offset since — a wheel notch up takes it a row
    /// higher, a notch down a row lower, a prompt jump seats it on the
    /// prompt — or, before any pin is remembered, the paint's own
    /// `total − viewport − offset` against the last totals. The fold sites
    /// measure their blocks against this figure and carry it with the note,
    /// so the settle re-measures against the same pin whatever the reader
    /// has done since.
    fn pinned_row(&self) -> usize {
        self.scroll.pin_after_reader_move(self.pin_base())
    }

    /// The pin as the last settle left it, before whatever the reader has
    /// done to the offset since: the remembered row, or — before any pin
    /// is remembered — the paint's own `total − viewport − offset`
    /// against the last totals.
    fn remembered_pin(&self) -> usize {
        match self.scroll.anchored_row {
            Some(row) => row,
            None => self.last_frame_top_row(),
        }
    }

    /// The row that actually sat at the top of the last frame.
    ///
    /// The last totals less the viewport less the offset that frame
    /// displayed with — the saturated offset, where the frame could not
    /// reach the remembered pin — so this is the row the reader saw,
    /// whatever the remembered pin says.
    fn last_frame_top_row(&self) -> usize {
        self.pacing
            .last_layout_lines
            .saturating_sub(self.pacing.last_layout_viewport)
            .saturating_sub(self.scroll.settled_offset)
    }

    /// Where the pin sits in the transcript's own structure, for the
    /// frame's bake: the message whose rows hold it, the rendering unit
    /// within that message (a source segment, a block), and the row's
    /// offset inside the unit beside the unit's row count. `None` when the
    /// row sits in no baked message — a separator row, or the live region
    /// past the bake.
    fn pin_identity_at(&self, row: usize) -> Option<PinIdentity> {
        Self::pin_identity_in(row, &self.bake.message_ranges, &self.bake.message_units)
    }

    /// [`pin_identity_at`](Self::pin_identity_at) read against whichever
    /// layout's indexes the caller holds — borrowed, never copied.
    ///
    /// The same reading, against the merged indexes of the current bake
    /// or of a kept layout — the previous one, when a settle has to place
    /// a row the reader chose before the rebake moved everything.
    fn pin_identity_in(
        row: usize,
        ranges: &[std::ops::Range<usize>],
        blocks_of: &[Vec<Vec<usize>>],
    ) -> Option<PinIdentity> {
        let message = ranges.partition_point(|range| range.end <= row);
        let range = ranges.get(message)?;
        if !range.contains(&row) {
            return None;
        }
        let blocks = blocks_of.get(message)?;
        let block = blocks
            .partition_point(|units| units.first().copied().unwrap_or(range.end) <= row)
            .checked_sub(1)?;
        let units = blocks.get(block)?;
        let unit = units
            .partition_point(|start| *start <= row)
            .checked_sub(1)?;
        let start = *units.get(unit)?;
        let end = units
            .get(unit.saturating_add(1))
            .copied()
            .or_else(|| {
                blocks
                    .get(block.saturating_add(1))
                    .and_then(|units| units.first())
                    .copied()
            })
            .unwrap_or(range.end);
        Some(PinIdentity {
            message,
            block,
            unit,
            offset: row.saturating_sub(start),
            unit_rows: end.saturating_sub(start),
        })
    }

    /// The rows the block registered under `key` holds above `pin`.
    ///
    /// Tool, thinking, and compaction blocks alike — a key names one
    /// block in exactly one map. The settle counts a block the pin sat
    /// inside this way, before and after a change, and moves the pin by
    /// the difference.
    fn rows_above_pin(&self, key: &str, message: Option<usize>, pin: usize) -> usize {
        self.bake
            .block_rows(key, message)
            .into_iter()
            .filter(|row| *row < pin)
            .count()
    }

    /// The block's row count and its last row, as the row maps stand.
    ///
    /// The note records both so the settle can tell a block that stood
    /// wholly above the pin — its last row before the pin — from one the
    /// pin sat inside, and apply the right rule to each.
    fn block_shape(&self, key: &str, message: Option<usize>) -> (usize, usize) {
        let rows = self.bake.block_rows(key, message);
        let last_row = rows.iter().copied().max().unwrap_or(0);
        (rows.len(), last_row)
    }

    /// Note, ahead of its `rebake_whole()`, that the block registered
    /// under `key` is about to fold or open.
    ///
    /// A pinned view has no anchor to hold and takes no note. A
    /// detached one records the block's shape as it stands in the map
    /// the rebake is about to redraw — the whole block, summary and
    /// expansion alike, since the map registers both postures and the
    /// settle needs the difference, not the detail. A block already
    /// noted keeps its first note: clicks queued behind a slow frame all
    /// read the same painted map, and the settle must apply the net
    /// change once, not once per click.
    fn note_block_reshape(&mut self, key: &str) {
        self.note_block_reshape_in(key, None);
    }

    /// [`note_block_reshape`](Self::note_block_reshape) for a call id a
    /// retry gave two blocks: every rendered occurrence is noted apart.
    ///
    /// A retried call graduates once per attempt under the same id, so
    /// the transcript can hold two of its blocks, and its toggle is the
    /// one id, so both change shape together. The occurrence above the
    /// pin moves it by its own change and the one below by nothing; rows
    /// pooled under the shared id would span the pin from both sides and
    /// read as one block neither wholly above it nor wholly below.
    ///
    /// Only occurrences the previous bake painted take a note. One that
    /// graduated after the last frame has no rows to measure — a
    /// message-scoped read of it would fall back to the painted
    /// occurrence's rows and count that fold twice — and its fold is no
    /// change in the painted transcript.
    fn note_call_reshape(&mut self, call_id: &str) {
        let painted: Vec<usize> = self
            .conversation
            .iter()
            .enumerate()
            .filter(|(index, message)| {
                self.bake.message_ranges.get(*index).is_some()
                    && matches!(
                        message,
                        TuiMessage::Assistant { blocks, .. }
                            if blocks.iter().any(|block| matches!(
                                block,
                                ContentBlock::Tool { call_id: id, .. } if id == call_id
                            ))
                    )
            })
            .map(|(index, _)| index)
            .collect();
        for message in painted {
            self.note_block_reshape_in(call_id, Some(message));
        }
    }

    /// Take the note for one block, `message` naming the occurrence when
    /// the key is shared.
    fn note_block_reshape_in(&mut self, key: &str, message: Option<usize>) {
        if self.scroll.stick.follows() {
            return;
        }
        if self
            .scroll
            .reshaped_blocks
            .iter()
            .any(|note| note.key == key && note.message == message)
        {
            return;
        }
        let pin = self.pinned_row();
        let (len, last_row) = self.block_shape(key, message);
        let rows_above = self.rows_above_pin(key, message, pin);
        self.scroll.reshaped_blocks.push(ReshapeNote {
            key: key.to_string(),
            message,
            rows_above,
            pin,
            len,
            last_row,
        });
    }

    /// Consume the reshape notes: how many rows the noted blocks added
    /// above the pin since their notes, and how many they removed —
    /// measured against the maps as the rebake left them, each against
    /// the pin its note carried, in the last settle's coordinates.
    ///
    /// Each removed row above the pin lifts the pinned row one row in the
    /// document; each added one lowers it. The settle moves the pin by
    /// exactly that, so the pinned row stays the view's top row. A block
    /// that stood wholly above the noted pin moves it by its whole
    /// change in length — every row it gained or lost lands above the
    /// pin, and counting the rows that sit above the pin's old index
    /// would stop short inside a block that expanded past it. A block
    /// the pin sat inside is counted row by row against the noted pin,
    /// so a fold and a scroll handled in one batch agree wherever the
    /// reader has since moved the view. A block wholly below the noted pin
    /// — its first row at or after it — moved nothing above the pin and
    /// moves the pin by nothing. The length rules come first and the
    /// inside count last: its rows are read from the post-change map,
    /// which the blocks above the pin have already shifted, so the count
    /// runs against the noted pin moved by their net — against the pin's
    /// old index it would take a block an earlier fold lifted past the
    /// pinned content for one that landed above it. A block with no rows
    /// when noted takes the inside count, which places whatever rows it
    /// gained on whichever side of the pin they fall.
    fn rows_moved_above_anchor(&mut self) -> (usize, usize) {
        let notes = std::mem::take(&mut self.scroll.reshaped_blocks);
        let mut landed_above = 0usize;
        let mut left_above = 0usize;
        let mut inside = Vec::new();
        for note in notes {
            let (len, _) = self.block_shape(&note.key, note.message);
            let first_row = note.last_row.saturating_sub(note.len.saturating_sub(1));
            if note.len > 0 && note.last_row < note.pin {
                landed_above = landed_above.saturating_add(len.saturating_sub(note.len));
                left_above = left_above.saturating_add(note.len.saturating_sub(len));
            } else if note.len == 0 || first_row <= note.pin {
                inside.push(note);
            }
        }
        for note in inside {
            let pin = note
                .pin
                .saturating_add(landed_above)
                .saturating_sub(left_above);
            let after = self.rows_above_pin(&note.key, note.message, pin);
            landed_above = landed_above.saturating_add(after.saturating_sub(note.rows_above));
            left_above = left_above.saturating_add(note.rows_above.saturating_sub(after));
        }
        (landed_above, left_above)
    }

    /// Take what the observer finished since the last frame.
    ///
    /// Finalized replies and completed tool calls alike move into
    /// the conversation as assistant messages; run failures
    /// recorded by the mode driver surface as error messages — so
    /// none of the shared buffers accumulates across frames.
    /// Whenever a reply, a tool completion, a failure, or a turn-end
    /// history lands, the turn-end hook — when installed — receives
    /// the conversation snapshot beside the newest engine history,
    /// the host's save point. A poisoned lock is
    /// recovered — the same policy the observer writes with — so
    /// finalized data still graduates.
    fn drain_shared_state(&mut self) {
        let graduations = take_locked(&self.state.graduations);
        let retry_lines = take_locked(&self.state.retry_lines);
        let now = chrono::Utc::now();
        for line in retry_lines {
            if let Some(index) = self.tools.retry_row_index.get(&line.episode).copied() {
                if let Some(TuiMessage::System { text, .. }) = self.conversation.get_mut(index) {
                    *text = line.text;
                    if index < self.bake.conversation_baked_through {
                        self.bake.rebake_whole();
                    } else {
                        self.bake.bump_generation();
                    }
                }
            } else {
                self.push_message(TuiMessage::System {
                    text: line.text.clone(),
                    timestamp: now,
                });
                let index = self.conversation.len().saturating_sub(1);
                self.tools.retry_row_index.insert(line.episode, index);
            }
        }
        if !self.state.agent_running.load(Ordering::SeqCst) {
            self.return_guidance();
        }
        let mut turn_ended = graduations
            .iter()
            .any(|graduation| !matches!(graduation, Graduation::Sent { .. }));
        let turn_histories = take_locked(&self.state.turn_histories);
        let machine_history = turn_histories
            .last()
            .map(|publication| Arc::clone(&publication.history));
        turn_ended |= machine_history.is_some();
        for graduation in graduations {
            match graduation {
                Graduation::Sent { text, steered } => {
                    self.push_message(TuiMessage::User {
                        text,
                        timestamp: now,
                        steered,
                        unsent: false,
                    });
                    if !steered && !self.plan.plan_interrupted {
                        self.plan.retire_finished_plan();
                    }
                    self.plan.plan_interrupted = false;
                }
                Graduation::Reply { text, thinking } => {
                    let mut blocks = Vec::new();
                    blocks.extend(thinking);
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text { text });
                    }
                    self.push_message(TuiMessage::Assistant {
                        blocks,
                        timestamp: now,
                        duration_ms: None,
                    });
                }
                Graduation::RunEnded {
                    duration_ms,
                    success,
                } => {
                    self.plan.plan_interrupted = !success;
                    self.exit.cancel_arm = false;
                    if let Some(TuiMessage::Assistant {
                        duration_ms: reply_duration,
                        ..
                    }) = self.conversation.last_mut()
                    {
                        *reply_duration = Some(duration_ms);
                        self.bake.bump_generation();
                    }
                }
                Graduation::Tool(result) => {
                    let mut todo_armed: Option<String> = None;
                    self.retain_tool_detail(
                        &result.call_id,
                        &result.full_input,
                        &result.full_output,
                    );
                    if matches!(result.name.as_str(), "Edit" | "MultiEdit" | "Write")
                        && crate::diff::parse_receipt(&result.full_output).is_some()
                    {
                        self.expansion.expanded_tools.insert(result.call_id.clone());
                    }
                    if result.name == "TodoWrite" && !result.is_error {
                        todo_armed = self.graduate_todo_plan(&result);
                    }
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
                    if let Some(id) = todo_armed {
                        self.expansion.auto_expanded_todo = Some(id);
                    }
                }
            }
        }
        let compactions = take_locked(&self.state.compactions);
        let errors = take_locked(&self.state.errors);
        turn_ended |= !errors.is_empty();
        for text in errors {
            self.push_message(TuiMessage::Error {
                text,
                timestamp: now,
            });
        }
        for event in compactions {
            let CompactionEvent { pass, record } = event;
            let owed = matches!(&record, TuiMessage::Compacted { summary: None, .. });
            let index = self.conversation.len();
            self.push_message(record);
            if owed {
                self.compaction_summary_owed.push((pass, index));
            }
        }
        self.settle_compaction_summary(&turn_histories);
        if let Some(latest) = self.copy.drain_newest_verdict() {
            let wording = copy_verdict_notice(latest);
            if !wording.is_empty() {
                self.post_notice(wording.to_string());
            }
        }
        let retired = take_locked(&self.state.retired_plan_entries);
        if !retired.is_empty() {
            self.plan.latest_todos.retain(|row| {
                !matches!(row.status, todo_view::TodoStatus::Pending)
                    || !retired.iter().any(|entry| {
                        entry.id == row.id
                            && entry.subject == row.subject
                            && entry.generation > self.plan.todo_generation
                    })
            });
        }
        if turn_ended && let Some(hook) = &self.host.turn_end_hook {
            hook(&self.conversation, machine_history.as_ref());
        }
    }

    /// Flatten the conversation into styled lines for the given width.
    ///
    /// Assistant text goes through the markdown pipeline with the
    /// assistant base color; user and system messages render as
    /// single styled lines; error messages render as their own
    /// block — a spacer row keeps the failure apart from whatever it
    /// landed beneath, a parsed header names the failure's facts when
    /// the well-known API shape carries them, and the detail wraps as
    /// styled plaintext at the pane width, so a long failure body
    /// stays readable instead of clipping at the right edge; a
    /// completed tool block renders its verbosity-shaped summary
    /// line(s) between the text blocks around it. A blank line
    /// separates a user message from the agent material beside it,
    /// and each user turn opens and closes with a padding row the
    /// caller tints as part of its box — pad and blank together are
    /// the boundary's breathing room. The live region
    /// (streaming text, in-flight tools) is assembled by the caller.
    fn conversation_lines(
        &self,
        area: Rect,
        start: usize,
        end: usize,
        previous_was_user: &mut Option<bool>,
        outputs: &mut BakeOutputs,
    ) {
        let BakeOutputs { lines, meta } = outputs;
        let BakeMeta {
            tool_summary_lines: summary_lines,
            thinking_summary_lines: thinking_lines,
            compaction_summary_lines: compaction_lines,
            user_turn_ranges: user_turns,
            user_prompt_marks: prompt_marks,
            todo_block_ranges: todo_blocks,
            message_ranges,
            message_units,
        } = meta;
        let width = area.width.max(1);
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        for (message_index, message) in self.conversation.iter().enumerate().take(end).skip(start) {
            let is_user = matches!(message, TuiMessage::User { .. });
            if previous_was_user.is_some_and(|was_user| was_user != is_user) {
                lines.push(Line::default());
            }
            *previous_was_user = Some(is_user);
            let message_start = lines.len();
            let mut block_units: Vec<Vec<usize>> = Vec::new();
            match message {
                TuiMessage::User {
                    text,
                    steered,
                    unsent,
                    ..
                } => {
                    let tint = if *unsent {
                        self.theme.ui.dim
                    } else {
                        self.theme.ui.user_message_fg
                    };
                    let mut units = Vec::new();
                    let tint_start = lines.len();
                    units.push(lines.len());
                    lines.push(Line::default());
                    let mark_row = lines.len();
                    for segment in text.split('\n') {
                        units.push(lines.len());
                        lines.extend(plain_wrapped_lines(segment, usize::from(width), tint));
                    }
                    units.push(lines.len());
                    lines.push(Line::default());
                    user_turns.push((tint_start, lines.len(), mark_row));
                    if !*unsent && !*steered {
                        prompt_marks.push(mark_row);
                    }
                    block_units.push(units);
                }
                TuiMessage::Assistant {
                    blocks,
                    duration_ms,
                    ..
                } => {
                    for (block_index, block) in blocks.iter().enumerate() {
                        let mut units = Vec::new();
                        units.push(lines.len());
                        let first = lines.len();
                        match block {
                            ContentBlock::Thinking {
                                text,
                                redacted,
                                duration_ms,
                            } => {
                                let key = format!("m{message_index}b{block_index}");
                                let expanded = self.expansion.expanded_thinking.contains(&key);
                                lines.extend(thinking_section_lines(
                                    text,
                                    *redacted,
                                    *duration_ms,
                                    expanded,
                                    usize::from(width),
                                    self.theme.ui.dim,
                                ));
                                index_summary_rows(thinking_lines, first, lines.len(), &key);
                                note_paragraph_units(lines, first, &mut units);
                            }
                            ContentBlock::Text { text } => {
                                lines.extend(markdown::render_markdown(
                                    text,
                                    width,
                                    &markdown_theme,
                                    &syntax_theme,
                                    self.theme.ui.assistant_message_fg,
                                    None,
                                ));
                                note_paragraph_units(lines, first, &mut units);
                            }
                            ContentBlock::Tool { .. } => {
                                self.push_tool_block_lines(
                                    block,
                                    width,
                                    lines,
                                    summary_lines,
                                    todo_blocks,
                                );
                                note_paragraph_units(lines, first, &mut units);
                            }
                        }
                        block_units.push(units);
                    }
                    if let Some(duration_ms) = duration_ms {
                        block_units.push(vec![lines.len()]);
                        lines.push(Line::styled(
                            format!("duration {}", format_duration(*duration_ms)),
                            self.theme.ui.dim,
                        ));
                    }
                }
                TuiMessage::System { text, .. } => {
                    let mut units = Vec::new();
                    for segment in text.split('\n') {
                        units.push(lines.len());
                        lines.extend(plain_wrapped_lines(
                            segment,
                            usize::from(width),
                            self.theme.ui.dim,
                        ));
                    }
                    block_units.push(units);
                }
                TuiMessage::Error { text, .. } => {
                    let rows_above = !lines.is_empty() || start > 0;
                    block_units.push(vec![lines.len()]);
                    lines.extend(error_block_lines(
                        text,
                        width,
                        self.theme.ui.status_error,
                        rows_above,
                    ));
                }
                message @ TuiMessage::Compacted { .. } => {
                    let key = format!("m{message_index}");
                    block_units.push(vec![lines.len()]);
                    let first = lines.len();
                    lines.extend(compaction_notice_lines(
                        message,
                        self.expansion.expanded_compactions.contains(&key),
                        self.theme.ui.dim,
                        width,
                    ));
                    index_summary_rows(compaction_lines, first, lines.len(), &key);
                }
            }
            message_ranges.push(message_start..lines.len());
            message_units.push(block_units);
        }
    }

    /// The one thing the row offers on its own initiative: a draft standing
    /// there while a run holds the agent.
    ///
    /// The impatient chord is the one a user has to think of at the moment it
    /// matters, and that moment looks exactly like typing an ordinary prompt.
    /// Empty composer or idle agent, the row stays blank — advertising a key
    /// that would do nothing is noise, and the row belongs to real notices
    /// whenever they have anything to say.
    fn steering_hint(&self) -> Option<String> {
        if self.host.guidance.is_none()
            || !self.state.agent_running.load(Ordering::SeqCst)
            || self.composer.input.text().trim().is_empty()
        {
            return None;
        }
        Some("ctrl+s to guide".to_string())
    }
}

/// The facts [`TuiApp::render`] resolves before the frame's rows are
/// split.
///
/// Gathered so the constraint build reads as one act: which optional
/// sections stand this frame, and the heights the fixed furniture
/// carries.
#[derive(Clone, Copy)]
struct FrameLayout {
    /// The frame's whole screen area, borders included.
    ///
    /// Every constraint the layout resolves is a share of this rect,
    /// and the split hands the same rect back as the chunks' common
    /// parent — the one geometry the whole frame is cut from.
    area: Rect,

    /// How many rows the composer claims this frame.
    ///
    /// Fixed furniture, resolved before the split: the conversation
    /// pane takes whatever this leaves behind it, so a tall paste in
    /// the input box is paid for out of the transcript's rows and not
    /// out of the sections above.
    composer_height: u16,

    /// Where the activity strip takes its row.
    ///
    /// The config's placement for the breathing bar, carried into the
    /// layout because a strip at the terminal's top row is paid for
    /// out of the conversation pane's height while one on the
    /// composer's spacer costs nothing — the constraint build has to
    /// know which before it splits.
    strip_position: StripPosition,

    /// The question sheet's page the overlay answers, when one stands.
    ///
    /// `None` while no ask owns the screen and the layout reserves
    /// nothing for a sheet; `Some(page)` names the count-ahead page
    /// the overlay's own footer reads, so the sheet's reserved height
    /// matches the page actually shown rather than a generic one.
    question_page: Option<usize>,

    /// Which optional sections stand this frame.
    ///
    /// The queue, plan, and place badges are each a maybe-row: this
    /// bitset records which of them earned one, so the constraint
    /// build reserves their rows in one act instead of each section
    /// measuring itself against the transcript's pane.
    sections: Sections,
}

/// The selectable document's per-segment line counts, in render
/// order.
///
/// Conversation, seam, thinking, frozen, live, compaction, tools —
/// the segments whose concatenation is the selection's line space.
/// Comparing two shapes says whether the lane renumbered; the counts
/// themselves never leave the frame that computed them.
type SelectableShape = (usize, usize, usize, usize, usize, usize, usize);

/// Open a unit at every paragraph that starts in the rows a block just
/// appended: each non-blank row following a blank one.
///
/// A long text, thinking, or detail block rewraps paragraph by paragraph,
/// and a rewrap maps the scroll pin's offset across its unit in proportion
/// to the unit's rows — within one paragraph that lands on the same line,
/// where across a whole block the blank rows between paragraphs would skew
/// the proportion onto a neighbour.
fn note_paragraph_units(lines: &[Line<'static>], first: usize, units: &mut Vec<usize>) {
    let Some(rows) = lines.get(first..) else {
        return;
    };
    for (offset, pair) in rows.windows(2).enumerate() {
        let [before, at] = pair else {
            continue;
        };
        if line_is_blank(before) && !line_is_blank(at) {
            units.push(first.saturating_add(offset).saturating_add(1));
        }
    }
}

/// Whether a row carries nothing but whitespace.
///
/// The paragraph seam [`note_paragraph_units`] opens units at: a row
/// whose every span trims to nothing reads as blank on the terminal,
/// whatever padding the renderer put in it.
fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

/// The three-pane layout always splits into three rects; the
/// fallback keeps rendering total if a shorter split ever appears.
fn pane(chunks: &[Rect], index: usize, fallback: Rect) -> Rect {
    chunks.get(index).copied().unwrap_or(fallback)
}

/// Paint `canvas` under every cell of `buffer` still on the terminal default.
///
/// The grounding walk behind a frame that owns its canvas inside a
/// multiplexer — see [`TuiApp::canvas_ground_warranted`]. Only the
/// default is touched, so a cell's own ground — the composer's
/// surface, a badge's fill, the splash's art — stands exactly as the
/// widget painted it, and the canvas lands on everything else: the
/// blank rows, the row tails, the stood-down strips, the pane's own
/// top. Pure over the buffer, so the walk pins without a terminal.
fn ground_default_cells(buffer: &mut ratatui::buffer::Buffer, canvas: Color) {
    for cell in &mut buffer.content {
        if cell.bg == Color::Reset {
            cell.bg = canvas;
        }
    }
}

/// The boot-time permission pair a stand-in config save restores.
///
/// What the session started under — the mode and preset selection as
/// the constructor's config stated them, flag- or file-named —
/// captured once so no later `/mode` or `/preset` swap rides a
/// stand-in save into a file it promised never to touch, and no
/// initial flag-stated pair rides one out.
fn boot_permission_pair(config: &DchConfig) -> (dch_config::PermissionMode, Option<String>) {
    (
        config.runner.permission_mode,
        config.permissions.preset.clone(),
    )
}
