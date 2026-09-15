//! The terminal application shell.
//!
//! `TuiApp` owns the input buffer, the conversation, and the shared
//! state background producers write through; `run` drives the
//! render ↔ input ↔ scroll loop until the user quits.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use dch_config::DchConfig;

use crate::markdown;
use crate::message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
use crate::observer::{ToolResultDisplay, TuiObserverState};
use crate::theme::Theme;

/// How many completed tool calls the live region keeps visible.
///
/// Oldest entries drop off the drained history once the count passes
/// this depth, so a long session neither loses recent completions nor
/// accumulates all of them.
const TOOL_HISTORY_DEPTH: usize = 20;

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

    /// Completed tool calls this display has taken from the shared
    /// buffer.
    ///
    /// Drained from the shared state on every redraw and capped at
    /// [`TOOL_HISTORY_DEPTH`], so finished calls stay visible while
    /// nothing accumulates without bound.
    tool_history: Vec<ToolResultDisplay>,

    /// The input line's current text.
    ///
    /// Keystrokes insert at the cursor; Enter submits the text and
    /// clears the buffer.
    input: String,

    /// The input cursor's byte index.
    ///
    /// Always at a UTF-8 character boundary; it stays at the end of
    /// the text, since the cursor cannot move within the line.
    cursor: usize,

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
    /// growth since this value, holding the viewport steady as the
    /// streaming region extends below it.
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
            tool_history: Vec::new(),
            input: String::new(),
            cursor: 0,
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
    }

    /// The current input-line text.
    ///
    /// Empty right after a submit; keystrokes append at the cursor.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The input cursor's byte index.
    ///
    /// Counts bytes, not characters, and always sits on a UTF-8
    /// character boundary.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
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

    /// Run the UI event loop until the user exits.
    ///
    /// Selects over terminal events, the shared notify (the path
    /// background state uses to request a redraw), and a periodic
    /// tick. A notify listener stays registered across draws and
    /// event handling, so a notification sent while the loop is busy
    /// is delivered on the next iteration rather than lost.
    /// Background redraws are frame-capped: a notify renders at most
    /// once per frame interval, a request arriving sooner parks
    /// itself for the tick to claim, and a queued finalized reply
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
        let mut events = Box::pin(crossterm::event::EventStream::new());
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let notify = Arc::clone(&self.state.render_notify);
        let mut listener = notify.listen();

        terminal.draw(|frame| self.render(frame))?;

        while !self.quitting {
            let mut needs_redraw = false;
            tokio::select! {
                maybe_event = events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => needs_redraw = self.handle_event(&event),
                        Some(Err(err)) => {
                            tracing::warn!("terminal event stream error: {err}");
                        }
                        None => break,
                    }
                }
                () = &mut listener => {
                    needs_redraw = self.notify_wake(&notify, &mut listener);
                }
                _instant = tick.tick() => {
                    needs_redraw = self.tick_wake(Instant::now());
                }
            }
            if needs_redraw {
                terminal.draw(|frame| self.render(frame))?;
            }
        }
        Ok(())
    }

    /// Apply one terminal event to the app state.
    ///
    /// Returns whether the event requires a redraw. Key releases and
    /// repeats are ignored so a held key fires once per press.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        let Event::Key(key) = event else {
            return matches!(event, Event::Resize(_, _));
        };
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
                self.quitting = true;
                true
            }
            (KeyCode::Enter, _) => {
                let text = std::mem::take(&mut self.input);
                self.cursor = 0;
                if !text.trim().is_empty() {
                    self.conversation.push(TuiMessage::User {
                        text,
                        timestamp: chrono::Utc::now(),
                    });
                    self.scroll_to_bottom();
                }
                true
            }
            (KeyCode::Backspace, _) => {
                self.delete_char_before_cursor();
                true
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                if self.input.is_char_boundary(self.cursor) {
                    self.input.insert(self.cursor, c);
                    self.cursor = self.cursor.saturating_add(c.len_utf8());
                }
                true
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.auto_scroll = false;
                true
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                self.rearm_if_near_bottom();
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
            (KeyCode::End, _) => {
                self.scroll_to_bottom();
                true
            }
            _ => false,
        }
    }

    /// Delete the character before the cursor.
    ///
    /// Walks back to the previous char boundary, so multi-byte input
    /// never sheds a partial character.
    fn delete_char_before_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let Some(prefix) = self.input.get(..self.cursor) else {
            return;
        };
        let new_cursor = prefix.char_indices().last().map_or(0, |(i, _)| i);
        if self.input.is_char_boundary(new_cursor) {
            self.input.drain(new_cursor..);
            self.cursor = new_cursor;
        }
    }

    /// Whether any tool call is in flight.
    ///
    /// The tick consults this to keep in-flight elapsed stamps live;
    /// a poisoned lock reads as none, the same policy the render
    /// path applies to the live region.
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
    /// A queued finalized reply always forces the frame — the
    /// graduated message must not wait out the cap. Otherwise a
    /// frame renders at most once per frame interval; a request
    /// arriving sooner parks itself in the pending flag for the
    /// periodic tick to claim. The caller supplies the frame clock.
    pub fn redraw_due(&mut self, now: Instant) -> bool {
        if self.finalized_reply_waiting() {
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
    /// Claims any parked background redraw first, then redraws when
    /// tool calls are in flight (keeping their elapsed stamps live)
    /// or a claim was made; an idle tick with nothing parked and no
    /// tools in flight draws nothing.
    pub fn tick_wake(&mut self, now: Instant) -> bool {
        let claimed = self.take_pending_redraw(now);
        self.any_tools_running() || claimed
    }

    /// Whether a finalized reply is waiting to graduate.
    ///
    /// A poisoned lock recovers — the same policy the drain applies
    /// — so a finalized reply still forces its frame after another
    /// thread's panic.
    fn finalized_reply_waiting(&self) -> bool {
        let replies = self
            .state
            .completed_replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        !replies.is_empty()
    }

    /// Render one frame of the three-pane layout.
    ///
    /// Conversation fills the space above the input box; the status
    /// bar closes the frame at the bottom row. Each frame first takes
    /// what the observer finished — finalized replies graduate into
    /// the conversation, completed tool calls into the bounded
    /// history. The streaming region follows the conversation:
    /// frozen blocks render from the cache, the live complete lines
    /// re-parse as markdown, and the unterminated tail renders as
    /// plaintext. A pinned view re-anchors to the newest line; a
    /// detached view's offset is compensated for layout growth, so
    /// the viewport holds while content streams in below it.
    pub fn render(&mut self, frame: &mut Frame) {
        self.drain_shared_state();
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        let fallback = area;
        let conversation_area = pane(&chunks, 0, fallback);
        let input_area = pane(&chunks, 1, fallback);
        let status_area = pane(&chunks, 2, fallback);

        let conversation_height = conversation_area.height as usize;
        let width = conversation_area.width.max(1);
        let mut lines = self.conversation_lines(conversation_area);
        lines.extend(self.streaming_region_lines(width));
        lines.extend(self.active_tool_lines());
        let total_lines = lines.len();
        if self.auto_scroll {
            self.scroll_offset = 0;
        } else {
            let growth = total_lines.saturating_sub(self.last_layout_lines);
            self.scroll_offset = self.scroll_offset.saturating_add(growth);
        }
        self.last_layout_lines = total_lines;
        let skip = total_lines
            .saturating_sub(conversation_height)
            .saturating_sub(self.scroll_offset);
        let visible: Vec<Line<'_>> = lines
            .into_iter()
            .skip(skip)
            .take(conversation_height)
            .collect();
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
    /// Finalized replies move into the conversation as assistant
    /// messages; completed tool calls move into this display's
    /// bounded history — so neither shared buffer accumulates across
    /// frames. A poisoned lock is recovered — the same policy the
    /// observer writes with — so finalized data still graduates.
    fn drain_shared_state(&mut self) {
        let replies = take_locked(&self.state.completed_replies);
        let now = chrono::Utc::now();
        for text in replies {
            self.push_message(TuiMessage::Assistant {
                blocks: vec![ContentBlock::Text { text }],
                timestamp: now,
                duration_ms: None,
            });
        }
        self.tool_history
            .extend(take_locked(&self.state.tool_results));
        let drop_count = self.tool_history.len().saturating_sub(TOOL_HISTORY_DEPTH);
        self.tool_history.drain(..drop_count);
    }

    /// Flatten the conversation into styled lines for the given width.
    ///
    /// Assistant text goes through the markdown pipeline with the
    /// assistant base color; user, system, and error messages render
    /// as single styled lines; a completed tool block renders as one
    /// dim summary line between the text blocks around it. The
    /// bounded history of drained tool results closes the settled
    /// content; the live region (streaming text, in-flight tools)
    /// is assembled by the caller.
    fn conversation_lines(&self, area: Rect) -> Vec<Line<'static>> {
        let width = area.width.max(1);
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for message in &self.conversation {
            match message {
                TuiMessage::User { text, .. } => {
                    lines.push(Line::styled(
                        text.clone(),
                        Style::default().fg(self.theme.ui.user_message_fg),
                    ));
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
                                lines.push(completed_tool_line(
                                    name,
                                    input_preview,
                                    *success,
                                    *elapsed_secs,
                                    &self.theme,
                                ));
                            }
                        }
                    }
                }
                TuiMessage::System { text, .. } => {
                    lines.push(Line::styled(
                        text.clone(),
                        Style::default().fg(self.theme.ui.dim),
                    ));
                }
                TuiMessage::Error { text, .. } => {
                    lines.push(Line::styled(
                        text.clone(),
                        Style::default().fg(self.theme.ui.status_error),
                    ));
                }
            }
        }
        for result in &self.tool_history {
            lines.push(result_line(result, &self.theme));
        }
        lines
    }

    /// The streaming region: what the in-flight reply looks like.
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
    /// per-frame parse); and the unterminated tail —
    /// the line still being typed, as width-wrapped plaintext. An
    /// empty or poisoned buffer renders nothing.
    fn streaming_region_lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let buffer = self
            .state
            .streaming_text
            .lock()
            .map_or_else(|_| String::new(), |text| text.clone());
        if buffer.is_empty() {
            self.reset_stream_cache();
            return Vec::new();
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

        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let base = self.theme.ui.assistant_message_fg;
        let live = buffer
            .rsplit_once('\n')
            .map_or(buffer.as_str(), |(_, tail)| tail);
        let live_md_len = buffer.len().saturating_sub(live.len());
        let live_md = buffer
            .get(self.frozen_upto..live_md_len)
            .filter(|segment| !segment.is_empty());
        let mut lines = self.frozen_lines.clone();
        let leading = live_md.map_or(0, |segment| split_blank_prefix(segment).0);
        if !lines.is_empty() && (live_md.is_some() || !live.is_empty()) {
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
        lines
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
            .map(|tool| running_tool_line(tool, &self.theme))
            .collect()
    }

    /// Render the input box with the caret position.
    ///
    /// The caret sits one column inside the border, offset by the
    /// display width of the text before the cursor.
    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.ui.input_border));
        let inner = block.inner(area);
        frame.render_widget(
            Paragraph::new(self.input.clone()).style(Style::default().fg(self.theme.ui.input_text)),
            inner,
        );
        frame.render_widget(block, area);

        let prefix_width = self
            .input
            .get(..self.cursor)
            .map_or(0, UnicodeWidthStr::width);
        let caret_x = u16::try_from(prefix_width)
            .map_or(inner.x, |width| inner.x.saturating_add(width))
            .min(inner.right().saturating_sub(1));
        let caret_y = inner.y;
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

/// Build the conversation line for a completed tool call.
///
/// The outcome marker carries the theme's success or error color; the
/// name, input preview, and elapsed stamp stay dim so tool activity
/// reads as one glanceable line between the text blocks around it.
fn completed_tool_line(
    name: &str,
    input_preview: &str,
    success: bool,
    elapsed_secs: f64,
    theme: &Theme,
) -> Line<'static> {
    let (marker, marker_color) = if success {
        ("✓", theme.ui.status_success)
    } else {
        ("✗", theme.ui.status_error)
    };
    Line::from(vec![
        Span::styled(marker, Style::default().fg(marker_color)),
        Span::styled(
            format!(" {name} {input_preview}{}", format_elapsed(elapsed_secs)),
            Style::default().fg(theme.ui.dim),
        ),
    ])
}

/// Build the conversation line for a drained tool result.
///
/// The result record carries no input preview — the conversation
/// around it supplies the what — so the line is the outcome marker,
/// the name, and the elapsed stamp.
fn result_line(result: &ToolResultDisplay, theme: &Theme) -> Line<'static> {
    let (marker, marker_color) = if result.is_error {
        ("✗", theme.ui.status_error)
    } else {
        ("✓", theme.ui.status_success)
    };
    Line::from(vec![
        Span::styled(marker, Style::default().fg(marker_color)),
        Span::styled(
            format!(
                " {}{}",
                result.name,
                format_elapsed(result.duration.as_secs_f64())
            ),
            Style::default().fg(theme.ui.dim),
        ),
    ])
}

/// Build the conversation line for an in-flight tool call.
///
/// The marker and summary stay dim — a quiet cue that the call is
/// working, replaced by the colored outcome marker once it completes.
fn running_tool_line(tool: &ActiveTool, theme: &Theme) -> Line<'static> {
    let elapsed = tool.start.elapsed().as_secs_f64();
    Line::from(vec![
        Span::styled("⏳", Style::default().fg(theme.ui.dim)),
        Span::styled(
            format!(
                " {} {}{}",
                tool.name,
                tool.input_summary,
                format_elapsed(elapsed)
            ),
            Style::default().fg(theme.ui.dim),
        ),
    ])
}

/// Render a duration as a parenthesized elapsed stamp.
///
/// Sub-minute durations keep one decimal (`(0.4s)`); at a minute the
/// total is rounded once, then split into minutes and whole seconds
/// (`(1m30s)`), so the components never round past the true value.
fn format_elapsed(secs: f64) -> String {
    let total = secs.round();
    if total >= 60.0 {
        format!(" ({:.0}m{:.0}s)", (total / 60.0).floor(), total % 60.0)
    } else {
        format!(" ({secs:.1}s)")
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

/// Wrap streamed text into styled plaintext lines.
///
/// The streaming tail — the line still being typed — renders raw:
/// split on newlines and greedily wrapped at the pane width on
/// character boundaries, styled with the assistant foreground. No
/// parsing, so the cost stays linear in the tail and partial markup
/// shows exactly as typed.
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
