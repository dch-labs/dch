//! The terminal application shell.
//!
//! `TuiApp` owns the input buffer, the conversation, and the shared
//! state background producers write through; `run` drives the
//! render ↔ input ↔ scroll loop until the user quits.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use event_listener::Event as WakeEvent;
use futures::StreamExt;
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::UnicodeWidthStr;

use dch_config::DchConfig;

use crate::markdown;
use crate::message::{ActiveTool, TokenCounts, TuiMessage};
use crate::theme::Theme;

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

    /// The streaming-text buffer for the reply in progress.
    ///
    /// One allocation shared by whatever produces reply text and the
    /// renderer; it reads empty while no reply is in progress.
    streaming_text: Arc<Mutex<String>>,

    /// The list of tools in flight.
    ///
    /// One entry per dispatched call, removed on completion; empty
    /// while nothing is running.
    active_tools: Arc<Mutex<Vec<ActiveTool>>>,

    /// The token-usage counters.
    ///
    /// The status bar reads the cumulative totals from this shared
    /// copy.
    tokens: Arc<Mutex<TokenCounts>>,

    /// The shared redraw request.
    ///
    /// Waking it makes the run loop redraw on its next iteration; a
    /// listener stays registered across draws and event handling.
    render_notify: Arc<WakeEvent>,

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
    pub fn new(config: DchConfig) -> Self {
        let theme = if let Some(theme) = Theme::by_name(&config.display.theme) {
            theme
        } else {
            tracing::warn!("unknown theme '{}', using default", config.display.theme);
            Theme::default()
        };
        Self {
            theme,
            conversation: Vec::new(),
            streaming_text: Arc::new(Mutex::new(String::new())),
            active_tools: Arc::new(Mutex::new(Vec::new())),
            tokens: Arc::new(Mutex::new(TokenCounts::default())),
            render_notify: Arc::new(WakeEvent::new()),
            input: String::new(),
            cursor: 0,
            scroll_offset: 0,
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
        &self.streaming_text
    }

    /// The shared in-flight tool list.
    ///
    /// Producers record entries per dispatched call and remove them
    /// on completion.
    #[must_use]
    pub fn active_tools(&self) -> &Arc<Mutex<Vec<ActiveTool>>> {
        &self.active_tools
    }

    /// The shared token counters.
    ///
    /// The status bar's totals read through this handle.
    #[must_use]
    pub fn tokens(&self) -> &Arc<Mutex<TokenCounts>> {
        &self.tokens
    }

    /// The shared redraw request.
    ///
    /// Waking it triggers a redraw on the loop's next iteration.
    #[must_use]
    pub fn render_notify(&self) -> &Arc<WakeEvent> {
        &self.render_notify
    }

    /// Run the UI event loop until the user exits.
    ///
    /// Selects over terminal events, the shared notify (the path
    /// background state uses to request a redraw), and a periodic
    /// tick held in reserve for animated chrome. A notify listener
    /// stays registered across draws and event handling, so a
    /// notification sent while the loop is busy is delivered on the
    /// next iteration rather than lost. Redraws happen only when an
    /// arm asks for one. The caller owns terminal setup and teardown.
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
        let notify = Arc::clone(&self.render_notify);
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
                    needs_redraw = true;
                    listener = notify.listen();
                }
                _instant = tick.tick() => {}
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
                true
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                true
            }
            (KeyCode::PageUp, _) => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                true
            }
            (KeyCode::PageDown, _) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
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

    /// Jump the view back to the bottom of the conversation.
    ///
    /// Submits call this so freshly appended output is visible.
    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Render one frame of the three-pane layout.
    ///
    /// Conversation fills the space above the input box; the status
    /// bar closes the frame at the bottom row.
    pub fn render(&mut self, frame: &mut Frame) {
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
        let lines = self.conversation_lines(conversation_area);
        let total_lines = lines.len();
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
        if total_lines > conversation_height {
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

    /// Flatten the conversation into styled lines for the given width.
    ///
    /// Assistant text goes through the markdown pipeline with the
    /// assistant base color; user, system, and error messages render
    /// as single styled lines.
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
                        if let crate::message::ContentBlock::Text { text } = block {
                            lines.extend(markdown::render_markdown(
                                text,
                                width,
                                &markdown_theme,
                                &syntax_theme,
                                self.theme.ui.assistant_message_fg,
                                None,
                            ));
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
        lines
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
        let tokens = self.tokens.lock().map_or_else(
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

/// Fetch a layout pane by index with a whole-area fallback.
///
/// The three-pane layout always splits into three rects; the
/// fallback keeps rendering total if a shorter split ever appears.
fn pane(chunks: &[Rect], index: usize, fallback: Rect) -> Rect {
    chunks.get(index).copied().unwrap_or(fallback)
}
