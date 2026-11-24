//! The notice and line builders: transient notices, compaction
//! summaries and their ledger rows, the live thinking section, and
//! the tool blocks' line rendering.

use super::TuiApp;
use super::todos::ToolDetail;
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation as _;

use super::todos::ThinkingMode;
use crate::cells::str_width;
use crate::cells::{char_cells, display_width};
use crate::message::{ContentBlock, TuiMessage};
use crate::observer::{HealthNotice, ToolResultDisplay, TurnPublication};
use crate::todo_view;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

impl TuiApp {
    /// The live thinking section's rows, and where its row sits.
    ///
    /// Renders while a reasoning stream is living and the turn's
    /// reply text has not begun: one row carrying the marker, label,
    /// and running duration, with the capped italic body below it under
    /// Follow and nothing under Suppressed — and every returned row a
    /// click target for the Follow/Suppressed mode, as tool rows are.
    /// The reply's first text delta folds the lane for the rest of
    /// the turn; reasoning deltas keep accumulating out of sight. The
    /// breathing bar itself lives in the pinned strip above the
    /// composer, not here. The second slot is the block's first row
    /// within the returned lines, for click registration.
    pub(crate) fn live_thinking_lines(&self, width: u16) -> (Vec<Line<'static>>, Option<usize>) {
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return (Vec::new(), None);
        }
        let stream = self
            .state
            .thinking
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !stream.is_living() {
            return (Vec::new(), None);
        }
        let folded = stream.text_began;
        if folded
            && stream.turn
                != self
                    .state
                    .reply_turn
                    .load(std::sync::atomic::Ordering::SeqCst)
        {
            return (Vec::new(), None);
        }
        let now = Instant::now();
        let phase_end = stream.text_began_at.unwrap_or(now);
        let elapsed = stream.began.map_or(0, |began| {
            u64::try_from(phase_end.duration_since(began).as_millis()).unwrap_or(0)
        });
        let dim = self.theme.ui.dim;
        let expanded = !folded && matches!(self.expansion.thinking_mode, ThinkingMode::Follow);
        let marker = if expanded { "▾" } else { "▸" };
        let row = Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(dim)),
            Span::styled(
                "Thinking",
                Style::default()
                    .fg(self.theme.ui.foreground)
                    .add_modifier(ratatui::style::Modifier::ITALIC),
            ),
            Span::styled(
                format!(" · {}", format_duration(elapsed)),
                Style::default().fg(self.theme.ui.assistant_message_fg),
            ),
        ]);
        let mut lines = vec![row];
        if expanded {
            let body_source: &str = if stream.redacted && stream.text.is_empty() {
                THINKING_REDACTED_PLACEHOLDER
            } else {
                stream.text.as_str()
            };
            let body_style = Style::default()
                .fg(dim)
                .add_modifier(ratatui::style::Modifier::ITALIC);
            let mut wrapped = Vec::new();
            for segment in body_source.lines() {
                wrapped.extend(plain_wrapped_lines_styled(
                    segment,
                    usize::from(width),
                    body_style,
                ));
            }
            let start = wrapped.len().saturating_sub(LIVE_THINKING_BODY_CAP);
            lines.extend(wrapped.into_iter().skip(start));
        }
        let toggleable = if folded { None } else { Some(0) };
        (lines, toggleable)
    }

    /// The running compaction pass's rows for the live lane.
    ///
    /// Renders while the pass-start cue holds — one row in the same
    /// register as the live thinking row: marker, italic label, and
    /// the running duration measured from the cue. The pass settles
    /// into the transcript's compacted row on its own; nothing
    /// renders here once it does.
    pub(crate) fn live_compaction_lines(&self) -> Vec<Line<'static>> {
        let began = {
            let health = self
                .state
                .health
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let HealthNotice::Compacting { began } = *health else {
                return Vec::new();
            };
            began
        };
        let dim = self.theme.ui.dim;
        let elapsed = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        vec![Line::from(vec![
            Span::styled("▸ ".to_string(), Style::default().fg(dim)),
            Span::styled(
                "Compacting",
                Style::default()
                    .fg(dim)
                    .add_modifier(ratatui::style::Modifier::ITALIC),
            ),
            Span::styled(
                format!(" · {}", format_duration(elapsed)),
                Style::default().fg(dim),
            ),
        ])]
    }

    /// Render one completed tool block into the conversation rows.
    ///
    /// Builds the block's verbosity-shaped summary line(s) and, when
    /// the call carries retained expansion data, prepends the fold
    /// marker and appends the expanded detail. Every row a block
    /// with retained detail produced is registered under the call's
    /// id, so a press on any of them resolves back to the call it
    /// belongs to; an evicted block renders plain summary rows no
    /// press resolves.
    pub(crate) fn push_tool_block_lines(
        &self,
        block: &ContentBlock,
        width: u16,
        lines: &mut Vec<Line<'static>>,
        summary_lines: &mut HashMap<usize, String>,
        todo_blocks: &mut Vec<std::ops::Range<usize>>,
    ) {
        let ContentBlock::Tool {
            name,
            call_id,
            input_preview,
            success,
            elapsed_secs,
            ..
        } = block
        else {
            return;
        };
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
            self.display.verbosity,
        ));
        if let Some(detail) = self.tools.tool_details.get(call_id) {
            let expanded = self.expansion.expanded_tools.contains(call_id);
            prepend_marker(
                lines,
                first,
                if expanded { "▾ " } else { "▸ " },
                self.theme.ui.dim,
            );
            if expanded {
                if matches!(name.as_str(), "Edit" | "MultiEdit" | "Write")
                    && let Some(diff) = crate::diff::parse_receipt(&detail.output)
                {
                    lines.extend(crate::diff::render_diff(
                        &diff,
                        self.config.display.diff_layout,
                        width,
                        crate::diff::diff_colors(&self.theme, self.config.display.diff_palette),
                    ));
                } else if name == "TodoWrite"
                    && let Some(rows) = crate::todo_view::parse_rendered(&detail.output)
                        .map(|rows| {
                            crate::todo_view::rows_with_submitted_subjects(rows, &detail.input_json)
                        })
                        .or_else(|| crate::todo_view::parse_submission(&detail.input_json))
                {
                    let colors = crate::todo_view::TodoColors {
                        plain: self.theme.ui.foreground,
                        dim: self.theme.ui.dim,
                        accent: self.theme.ui.primary,
                        moved: self.theme.ui.secondary,
                        badge_fill: self.theme.ui.primary,
                        badge_letters: self.theme.ui.background,
                    };
                    let changes = self
                        .plan
                        .todo_changes
                        .get(call_id.as_str())
                        .cloned()
                        .unwrap_or_default();
                    lines.push(todo_view::summary_line(&rows, &colors));
                    lines.extend(todo_view::render_rows(&rows, &changes, &colors, width));
                    todo_blocks.push(first..lines.len());
                } else {
                    lines.extend(expansion_lines(
                        &detail.input_json,
                        Some(&detail.output),
                        width,
                        self.theme.ui.assistant_message_fg,
                        self.theme.ui.dim,
                    ));
                }
            }
            for index in first..lines.len() {
                summary_lines.insert(index, call_id.clone());
            }
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
    pub(crate) fn active_tool_lines(
        &self,
        width: u16,
    ) -> (Vec<Line<'static>>, HashMap<usize, String>) {
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
                &self.theme,
                self.display.verbosity,
            ));
            let Some(capture) = captures.get(&tool.call_id) else {
                continue;
            };
            let expanded = self.expansion.expanded_tools.contains(&tool.call_id);
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

    /// The connection-health line for the notice row, if one shows.
    ///
    /// A retry in flight is invisible here (the retries themselves
    /// live in the conversation as reconnection rows; the notice row
    /// would only duplicate them), and the reconnected note holds only
    /// for the notice hold before yielding to whatever transient
    /// notice waits beneath it. A running compaction pass does not
    /// render here either — it lives in the conversation's live lane
    /// while it holds, beside the streaming rows it sits among.
    fn health_notice_text(&self) -> Option<String> {
        match &*self
            .state
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            HealthNotice::Reconnected { at } => {
                let until = at.checked_add(NOTICE_HOLD)?;
                (Instant::now() < until).then(|| "reconnected".to_string())
            }
            HealthNotice::Idle
            | HealthNotice::Retrying { .. }
            | HealthNotice::Compacting { .. } => None,
        }
    }

    /// The notice's current text, if any stands.
    ///
    /// One line of state-driven or transient message, chosen by
    /// precedence: while the quit ladder is armed, a standing transient
    /// notice speaks in its place and the ladder's own wording only
    /// when none does; past that, the health notice, a transient
    /// notice still inside its hold, and the steering hint, in that
    /// order. Stream and tool retries never appear here — they live in
    /// the conversation as their own rows — so whatever this returns
    /// is the one line the frame has to say, hosted by
    /// [`render_notice`](Self::render_notice) or the place row's right
    /// edge.
    pub(crate) fn notice_text(&self) -> Option<String> {
        if self.quit.armed {
            Some(self.transient_notice.as_ref().map_or_else(
                || "press ctrl+c again to quit".to_string(),
                |(notice, _)| notice.clone(),
            ))
        } else {
            self.health_notice_text()
                .or_else(|| {
                    self.transient_notice
                        .as_ref()
                        .filter(|(_, until)| *until > Instant::now())
                        .map(|(text, _)| text.clone())
                })
                .or_else(|| self.steering_hint())
        }
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

    /// Give every summary-less compaction row the pass's own words,
    /// when a publication stamped with its pass settles the debt.
    ///
    /// Each owed row carries the pass number the observer stamped its
    /// event with, and a publication settles it only when it carries
    /// that number exactly — a filing the manual pass made, or a
    /// turn-end publication whose count equals the pass, a count the
    /// turn end cannot hold until the pass has happened and cannot
    /// still hold once a later pass has folded. Anything else — an
    /// older turn's marked head, a filing minted for another pass —
    /// leaves the debt standing, and a filing whose row settled
    /// through the turn-end channel is dropped so it cannot linger.
    pub(crate) fn settle_compaction_summary(&mut self, publications: &[TurnPublication]) {
        if self.compaction_summary_owed.is_empty() {
            return;
        }
        let owed = self.compaction_summary_owed.clone();
        let mut settled: Vec<u64> = Vec::new();
        for (pass, index) in owed {
            let summary = self.take_filed_summary(pass).or_else(|| {
                publications
                    .iter()
                    .rev()
                    .find(|publication| publication.passes == pass)
                    .and_then(|publication| marked_compaction_summary(&publication.history))
            });
            if let Some(summary) = summary {
                self.attach_compaction_summary(index, summary);
                self.drop_filed_summary(pass);
                settled.push(pass);
            }
        }
        self.compaction_summary_owed
            .retain(|(pass, _)| !settled.contains(pass));
    }

    /// Take the filing stamped for `pass`, when one is filed.
    ///
    /// The manual pass's own folded feed, removed from the channel as
    /// it is taken — a filing serves exactly the row carrying its
    /// stamp, never a later one. The reduction to a marked summary
    /// happens after the take: a filing whose head carries no ledger
    /// mark is consumed and settles nothing, keeping the channel in
    /// step with the passes it files for.
    pub(crate) fn take_filed_summary(&mut self, pass: u64) -> Option<String> {
        let mut filed = self
            .state
            .compaction_publications
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let stamped = filed
            .iter()
            .position(|filing| filing.pass == pass)
            .map(|at| filed.remove(at));
        drop(filed);
        stamped.and_then(|filing| marked_compaction_summary(&filing.folded))
    }

    /// Drop any filing stamped for `pass`.
    ///
    /// The pair cleanup when a row settles through the turn-end
    /// channel: the pass's own filing, if the driver filed one, has
    /// nothing left to serve and must not outlive its row.
    pub(crate) fn drop_filed_summary(&mut self, pass: u64) {
        self.state
            .compaction_publications
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|filing| filing.pass != pass);
    }

    /// Attach a compacted context's summary to the row awaiting it.
    ///
    /// The row the drain named when the summary-less compaction
    /// notice landed takes the published history's head text. The
    /// attach mutates a row the bake may already hold rendered, so it
    /// bumps the conversation's generation and rebakes whole — the
    /// same discipline any mutation of a baked row keeps. A row that
    /// no longer sits where the drain named it takes nothing.
    pub(crate) fn attach_compaction_summary(&mut self, index: usize, summary: String) {
        let attached = match self.conversation.get_mut(index) {
            Some(TuiMessage::Compacted {
                summary: slot @ None,
                ..
            }) => {
                *slot = Some(summary);
                true
            }
            _ => false,
        };
        if attached {
            self.bake.conversation_generation = self.bake.conversation_generation.saturating_add(1);
            self.bake.rebake_whole();
        }
    }
}

/// The tool lane's retained state: the graduated calls' captured
/// detail, its FIFO eviction order, and the running calls' rows.
///
/// Everything the tool blocks keep between frames — the full
/// input/output pairs an expansion renders (bounded by the cap), the
/// queue that retires them, the row map a running block's press
/// resolves through, and the retry episodes' rewritten rows.
#[derive(Default)]
pub(crate) struct ToolLane {
    /// Full command and output per tool call, keyed by call id.
    ///
    /// Filled from the dispatch-side capture as calls graduate; a
    /// block renders its expansion from here. Runtime-only — the
    /// serialized session keeps previews, not payloads — and
    /// bounded to the most recent `TOOL_DETAIL_CAP` calls in FIFO
    /// order, so a long session's oldest blocks fold back to plain
    /// summary rows instead of retaining payloads forever.
    pub(crate) tool_details: HashMap<String, ToolDetail>,

    /// Graduation order of the retained tool details, oldest first.
    ///
    /// The eviction queue behind the detail cap's FIFO: the front
    /// names the entry the next graduation retires.
    pub(crate) tool_detail_order: std::collections::VecDeque<String>,

    /// Line index to call id, for running tool block rows.
    ///
    /// The same click targets for in-flight calls — the row and its
    /// expansion — in the line space the tools segment occupies;
    /// recorded per frame.
    pub(crate) tool_running_lines: HashMap<usize, String>,

    /// The conversation row each retry episode's bar lives in, keyed
    /// by episode id.
    ///
    /// The first attempt of an episode pushes a `System` row and
    /// records its index; later attempts of the same episode rewrite
    /// that row in place, so one rough patch reads as one line that
    /// counts up. Stale entries are inert rather than absent:
    /// episode ids come from a monotonic counter, so a retry that
    /// lands after a clear always carries a fresh id and never
    /// re-reads an index the clear orphaned.
    pub(crate) retry_row_index: HashMap<u64, usize>,
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

/// How many live thinking body lines render at most.
///
/// The expanded live body keeps its trailing lines, bounding the
/// rendered rows an unbounded reasoning dump paints — the wrap
/// itself still walks the whole body under the thinking lock; the
/// graduated block keeps everything.
const LIVE_THINKING_BODY_CAP: usize = 500;

/// The placeholder shown for withheld reasoning.
///
/// A redaction marker carries no text; a blank body would read as
/// nothing, so the section says what happened instead.
const THINKING_REDACTED_PLACEHOLDER: &str = "reasoning withheld by the provider";

/// The header a structured or rolling compaction pass marks its
/// summary message with — the one ledger spelling every marked
/// compactor shares.
///
/// The mark is how a folded feed's head is told from the conversation
/// it replaced; see [`marked_compaction_summary`].
const COMPACTED_LEDGER_HEADER: &str = "## Conversation summary";

/// The pass's own words a folded history leads with, when it leads
/// with any.
///
/// The structured and rolling compactors mark their summary message
/// with the ledger header, so a folded feed's head can be recognized
/// for what it is. The gate is the whole safety of the backfill: a
/// pre-fold history's head is an ordinary prompt — often the
/// session's first — and attaching it would dress conversation up as
/// the pass's output. The QA compactor's summary carries no mark, so
/// its rows keep no summary rather than a wrong one.
pub(crate) fn marked_compaction_summary(history: &[loopctl::message::Message]) -> Option<String> {
    let head = history.first()?.text_content();
    if head.starts_with(COMPACTED_LEDGER_HEADER) {
        Some(head.trim().to_string())
    } else {
        None
    }
}

/// The collapsed row's one-line peek at the pass's summary.
///
/// The ledger header is boilerplate every pass carries; the reader's
/// question is what the pass kept, so the preview is the first line
/// with content past any headers, capped to the pane in display
/// cells — a wide glyph spends two — with the ellipsis reserved
/// inside the budget, the same measure every column fit keeps.
/// `None` when the summary holds no content line at all.
fn summary_preview_line(summary: &str, width: usize) -> Option<String> {
    let clean = crate::tool_capture::sanitize_for_display(summary);
    let line = clean
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("## "))?;
    let budget = width.saturating_sub(2).max(1);
    if display_width(line) <= budget {
        return Some(line.to_string());
    }
    let cell_budget = budget.saturating_sub(1);
    let mut used = 0usize;
    let mut kept = String::new();
    for glyph in line.chars() {
        let span = char_cells(glyph);
        if used.saturating_add(span) > cell_budget {
            break;
        }
        kept.push(glyph);
        used = used.saturating_add(span);
    }
    Some(format!("{kept}…"))
}

/// The rows a compaction notice renders as.
///
/// Collapsed, one dim row — the marker and the pass duration a
/// scrolling reader needs, with a one-line peek at the summary when
/// the row carries one; expanded, the pass's remaining facts follow
/// beneath. The marker mirrors the thinking sections' shape so one
/// gesture vocabulary covers every expandable row.
pub(crate) fn compaction_notice_lines(
    message: &TuiMessage,
    expanded: bool,
    dim: ratatui::style::Color,
    width: u16,
) -> Vec<Line<'static>> {
    let TuiMessage::Compacted {
        tokens_before,
        tokens_after,
        reason,
        evicted,
        duration_ms,
        compactor,
        compression_ratio,
        summary,
        ..
    } = message
    else {
        return Vec::new();
    };
    let (tokens_before, tokens_after, evicted, duration_ms) =
        (*tokens_before, *tokens_after, *evicted, *duration_ms);
    let compactor = compactor.as_deref();
    let compression_ratio = *compression_ratio;
    let reason = reason.as_str();
    let marker = if expanded { "▾" } else { "▸" };
    let mut lines = vec![Line::styled(
        format!("{marker} Compacted · {}", format_duration(duration_ms)),
        dim,
    )];
    if !expanded
        && let Some(preview) = summary
            .as_deref()
            .and_then(|text| summary_preview_line(text, usize::from(width)))
    {
        lines.push(Line::styled(format!("  {preview}"), dim));
    }
    if expanded {
        let saved = tokens_before.saturating_sub(tokens_after);
        lines.push(Line::styled(
            format!("  {tokens_before} → {tokens_after} tokens (saved {saved})"),
            dim,
        ));
        lines.push(Line::styled(format!("  {evicted} messages evicted"), dim));
        lines.push(Line::styled(format!("  reason: {reason}"), dim));
        if let Some(name) = compactor {
            lines.push(Line::styled(format!("  compactor: {name}"), dim));
        }
        if let Some(ratio) = compression_ratio {
            lines.push(Line::styled(format!("  compression {ratio:.1}x"), dim));
        }
        if let Some(summary) = summary.as_deref().filter(|text| !text.is_empty()) {
            lines.push(Line::styled(String::new(), dim));
            let limit = usize::from(width).saturating_sub(2).max(1);
            let clean = crate::tool_capture::sanitize_for_display(summary);
            for source_line in clean.lines() {
                let trimmed = source_line.trim();
                if trimmed.is_empty() {
                    lines.push(Line::styled(String::new(), dim));
                    continue;
                }
                for row in wrap_prompt_rows(trimmed, limit, usize::MAX) {
                    lines.push(Line::styled(format!("  {row}"), dim));
                }
            }
        }
        lines.push(Line::styled(
            "  earlier exchanges now live in the summary — durable state is \
             the findings file and todo list"
                .to_string(),
            dim,
        ));
    }
    lines
}

/// Map every row a keyed block produced onto its key.
///
/// The press handlers resolve a screen row to the expandable block
/// it belongs to through these maps; one inserter keeps every
/// keyed block's registration identical.
pub(crate) fn index_summary_rows(
    map: &mut HashMap<usize, String>,
    first: usize,
    after_last: usize,
    key: &str,
) {
    for index in first..after_last {
        map.insert(index, key.to_string());
    }
}

/// Render one thinking section's lines.
///
/// The summary row — marker, label, duration — always renders in the
/// dim tone; expanded sections add the body as italic dim-wrapped
/// lines, never through the markdown pipeline — the slant and the
/// tone together mark the reasoning as the model's inner voice, not
/// the reply. Withheld reasoning renders the placeholder in place of
/// an empty body, in the same italic register.
pub(crate) fn thinking_section_lines(
    text: &str,
    redacted: bool,
    duration_ms: u64,
    expanded: bool,
    width: usize,
    dim: Color,
) -> Vec<Line<'static>> {
    let marker = if expanded { "▾" } else { "▸" };
    let style = Style::default().fg(dim);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(
            "Thinking",
            style.add_modifier(ratatui::style::Modifier::ITALIC),
        ),
        Span::styled(format!(" · {}", format_duration(duration_ms)), style),
    ])];
    if expanded {
        let body = if redacted && text.is_empty() {
            THINKING_REDACTED_PLACEHOLDER
        } else {
            text
        };
        let body_style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        for segment in body.split('\n') {
            lines.extend(plain_wrapped_lines_styled(segment, width, body_style));
        }
    }
    lines
}
/// The largest elapsed span a persisted tool block may carry.
///
/// `Duration::from_secs_f64` panics past its own representable
/// range; a hostile or corrupted value beyond this bound renders as
/// zero instead of killing the first frame after resume.
pub(crate) const MAX_ELAPSED_SECS: f64 = 9.0e18;

/// How long a transient notice holds the row above the composer.
///
/// Long enough to be read at a glance, short enough that the row
/// reads as empty by default.
pub(crate) const NOTICE_HOLD: Duration = Duration::from_secs(4);

/// Render a millisecond duration in the largest units that fit.
///
/// Under a second renders as `<1s`; seconds render bare; a minute or
/// more renders minutes beside seconds; an hour or more renders
/// hours, minutes, and seconds together — so a reader sees the
/// magnitude at a glance whether the span is a twitch or a marathon.
pub(crate) fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs == 0 {
        "<1s".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m {}s", secs / 3600, secs % 3600 / 60, secs % 60)
    }
}

/// Word-wrap `text` to `limit` display columns, hard-splitting any
/// single word wider than `limit`.
///
/// The uncapped core both capped twins build on: rows never exceed
/// `limit`, so a paragraph rendering them inside a border `limit + 2`
/// wide cannot re-flow them.
pub(crate) fn wrap_rows(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = str_width(word);
        if word_width > limit {
            if !current.is_empty() {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            for grapheme in word.graphemes(true) {
                let grapheme_width = str_width(grapheme);
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
    rows
}

/// [`wrap_rows`] keeping at most `max_rows` rows from the head.
///
/// The last kept row carries an ellipsis when truncation happens —
/// the collapsed previews' shape: a short head of the text with the
/// cut named where the reader's eye lands. A `max_rows` of zero buys
/// no rows at all — the caller's budget is honored exactly, never
/// exceeded by an ellipsis row.
pub(crate) fn wrap_prompt_rows(text: &str, limit: usize, max_rows: usize) -> Vec<String> {
    if max_rows == 0 {
        return Vec::new();
    }
    let mut rows = wrap_rows(text, limit);
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        if let Some(last) = rows.last_mut() {
            let keep = str_width(last).saturating_sub(1);
            let mut trimmed = String::new();
            let mut kept = 0usize;
            for grapheme in last.graphemes(true) {
                let grapheme_width = str_width(grapheme);
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

/// [`wrap_rows`] keeping at most `max_rows` rows from the tail.
///
/// The expanded command preview's shape: the *end* of a command is
/// the half a collapsed head hides — the destructive suffix — so the
/// kept rows are the wrap's last, with one leading ellipsis row
/// naming the cut. The ellipsis counts inside `max_rows` — the
/// ceiling is the caller's row budget, and the return never exceeds
/// it — so an overflowing text yields the ellipsis plus `max_rows - 1`
/// tail rows, while a text that fits returns its own rows unchanged.
/// Together the collapsed head and the expanded tail cover the whole
/// of any command that fits the cap.
pub(crate) fn wrap_prompt_rows_last(text: &str, limit: usize, max_rows: usize) -> Vec<String> {
    let rows = wrap_rows(text, limit);
    if rows.len() <= max_rows {
        return rows;
    }
    let keep = max_rows.saturating_sub(1);
    if keep == 0 {
        return vec!["…".to_string()];
    }
    let skip = rows.len().saturating_sub(keep);
    let mut kept: Vec<String> = rows.into_iter().skip(skip).collect();
    kept.insert(0, "…".to_string());
    kept
}

/// Wrap text into styled plaintext lines at a display width.
///
/// The input renders raw: split on newlines and greedily wrapped at
/// the width on character boundaries, styled with the caller's
/// foreground. No parsing, so the cost stays linear in the input and
/// partial markup shows exactly as typed — the properties the
/// streaming tail and the error rows both rely on.
pub(crate) fn plain_wrapped_lines(
    text: &str,
    width: usize,
    fg: ratatui::style::Color,
) -> Vec<Line<'static>> {
    plain_wrapped_lines_styled(text, width, Style::default().fg(fg))
}

/// Wrap text into styled plaintext lines under a full [`Style`].
///
/// The same greedy wrapping as [`plain_wrapped_lines`]; the style
/// carries modifiers as well as the foreground, so registers like the
/// italic thinking body — color plus slant — pass through intact. The
/// text passes through [`crate::tool_capture::sanitize_for_display`]
/// first: carriage returns break the line, escape sequences and the
/// remaining control characters drop, so no control byte can reach a
/// cell and re-execute against the frame. Everything wrapped here is
/// producer-controlled prose — a provider error body, a thinking
/// dump, a system notice — and any of it can carry the bytes that
/// once leaked a pane mid-scroll.
pub(crate) fn plain_wrapped_lines_styled(
    text: &str,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let cap = width.max(1);
    let mut lines = Vec::new();
    let cleaned = crate::tool_capture::sanitize_for_display(text);
    for raw in cleaned.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for ch in raw.chars() {
            let ch_width = char_cells(ch);
            if cap.saturating_sub(current_width) < ch_width && !current.is_empty() {
                lines.push(Line::styled(std::mem::take(&mut current), style));
                current_width = 0;
            }
            current.push(ch);
            current_width = current_width.saturating_add(ch_width);
        }
        lines.push(Line::styled(current, style));
    }
    lines
}

/// The rows a conversation error message renders as.
///
/// A blank spacer row opens the block when rows stand above it, so a
/// failure never reads as the tail of whatever it landed beneath — a
/// completed tool's duration footer being exactly the row it must not
/// glue onto. When the text carries the well-known loopctl API
/// failure shape, a header row names the failure's own facts — the
/// attempt count, the HTTP status, and the gateway tag an edge body
/// carried — wrapping to the pane like the detail beneath it, so a
/// tag longer than the width folds rather than clips — before the
/// wrapped detail, indented under the header, in the error tone.
/// Prose failures — a cancellation, a compaction notice — render
/// without a header, as the wrapped text alone. The text itself
/// passes through the same sanitizer the wrap funnel applies, so a
/// persisted HTML body bakes clean on resume too.
pub(crate) fn error_block_lines(
    text: &str,
    width: u16,
    error_color: Color,
    rows_above: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if rows_above {
        lines.push(Line::from(String::new()));
    }
    let facts = api_failure_facts(text);
    if let Some(header) = facts.as_ref().map(ApiFailureFacts::header) {
        for row in wrap_rows(&header, usize::from(width.max(1))) {
            lines.push(Line::styled(row, Style::default().fg(error_color)));
        }
    }
    let indent = if facts.is_some() { "  " } else { "" };
    let budget = usize::from(width.max(1))
        .saturating_sub(indent.len())
        .max(1);
    let cleaned = crate::tool_capture::sanitize_for_display(text);
    for raw in cleaned.split('\n') {
        if raw.is_empty() {
            lines.push(Line::styled(
                String::new(),
                Style::default().fg(error_color),
            ));
            continue;
        }
        for row in wrap_rows(raw, budget) {
            lines.push(Line::styled(
                format!("{indent}{row}"),
                Style::default().fg(error_color),
            ));
        }
    }
    lines
}

/// The failure facts an API error's well-known text shape carries.
///
/// The engine surfaces a provider failure as a `Display` string — the
/// only place the attempt count and HTTP status exist — so the
/// presentation parses those facts back out of the shape the engine
/// formats: `API error: … after {n} attempts … HTTP {code}`. Any
/// other text parses to `None`, and the row renders without a header
/// rather than a wrong one.
#[derive(Debug, PartialEq, Eq)]
struct ApiFailureFacts {
    /// The attempts the stream burned before the failure.
    ///
    /// Parsed from the `after {n} attempts` phrase; `None` when the
    /// text names no count.
    attempts: Option<u32>,

    /// The HTTP status the provider answered with.
    ///
    /// The first `HTTP {code}` the body names; `None` when the failure
    /// carries no status.
    status: Option<u16>,

    /// The gateway tag an edge error page named, if one appears.
    ///
    /// The text of the last `<center>…</center>` span — the mark an
    /// HTML body signs itself with; `None` for non-HTML bodies. The
    /// tag is provider-controlled text, so it is sanitized where it
    /// is parsed: the header interpolates it verbatim, and a crafted
    /// edge page must not carry its control bytes there. The sanitizer
    /// spells a carriage return as a line break, so the sanitized tag
    /// also collapses to one row — the header is a single line, and a
    /// break inside it would ride a cell mid-frame.
    gateway: Option<String>,
}

impl ApiFailureFacts {
    /// The header row the facts spell, in the review's standing form.
    ///
    /// The glyph marks the block as a failure of its own — never the
    /// tool row above it — and the facts follow in the order a reader
    /// asks for them: what died, how hard it tried, what the provider
    /// answered, who answered.
    fn header(&self) -> String {
        let mut facts = Vec::new();
        if let Some(attempts) = self.attempts {
            facts.push(if attempts == 1 {
                "1 attempt".to_string()
            } else {
                format!("{attempts} attempts")
            });
        }
        if let Some(status) = self.status {
            facts.push(format!("HTTP {status}"));
        }
        let core = if facts.is_empty() {
            format!("{} model request failed", crate::tool_render::ERR_GLYPH)
        } else {
            format!(
                "{} model request failed — {}",
                crate::tool_render::ERR_GLYPH,
                facts.join(" · ")
            )
        };
        match self.gateway.as_deref() {
            Some(gateway) => format!("{core} ({gateway})"),
            None => core,
        }
    }
}

/// Parse the failure facts out of an API error's text.
///
/// The gate is the `API error: ` prefix — the shape the engine's API
/// variant formats — so no other error row's prose can mint a
/// model-request header. Inside the body the attempt phrase and the
/// status code are scanned independently: either alone is enough for
/// a header, and a body carrying neither yields `None`.
fn api_failure_facts(text: &str) -> Option<ApiFailureFacts> {
    let body = text.strip_prefix("API error: ")?;
    let attempts = attempt_count(body);
    let status = http_status(body);
    if attempts.is_none() && status.is_none() {
        return None;
    }
    Some(ApiFailureFacts {
        attempts,
        status,
        gateway: gateway_tag(text).and_then(|tag| {
            let cleaned = crate::tool_capture::sanitize_for_display(&tag);
            let one_row = cleaned.replace('\n', " ");
            let trimmed = one_row.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }),
    })
}

/// The `{n}` of an `after {n} attempts` phrase in the body.
///
/// Every `after ` occurrence is tried, so a body that quotes the
/// phrase in prose cannot mask the real count elsewhere in it.
fn attempt_count(body: &str) -> Option<u32> {
    for (at, _) in body.match_indices(" after ") {
        let Some(rest) = body.get(at.saturating_add(" after ".len())..) else {
            continue;
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let Some(count) = rest.get(..digits).and_then(|lead| lead.parse::<u32>().ok()) else {
            continue;
        };
        if rest
            .get(digits..)
            .is_some_and(|tail| tail.starts_with(" attempts"))
        {
            return Some(count);
        }
    }
    None
}

/// The first `HTTP {code}` the body names.
///
/// The word `HTTP` appears twice in the engine's own shape —
/// `HTTP error: HTTP 502` — and only a three-digit code following it
/// counts, so the error-category mention is skipped by scan order.
fn http_status(body: &str) -> Option<u16> {
    for (at, _) in body.match_indices("HTTP ") {
        let Some(rest) = body.get(at.saturating_add("HTTP ".len())..) else {
            continue;
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 3
            && let Some(code) = rest.get(..digits).and_then(|lead| lead.parse::<u16>().ok())
        {
            return Some(code);
        }
    }
    None
}

/// The text of the last `<center>…</center>` span in the text.
///
/// An edge's HTML error page signs itself in one — `alibaba-ga` in
/// the session this parsing exists for — and the tag survives as the
/// only human-readable mark of who answered. The caller sanitizes
/// the tag before it reaches a row: the span's text is the
/// provider's own bytes, and a header is exactly where the display
/// policy must apply.
fn gateway_tag(text: &str) -> Option<String> {
    let (open, _) = text.rmatch_indices("<center>").next()?;
    let after = text.get(open.saturating_add("<center>".len())..)?;
    let close = after.find("</center>")?;
    let tag = after.get(..close)?.trim();
    (!tag.is_empty()).then(|| tag.to_string())
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn a_compaction_preview_budgets_by_the_painted_width() {
        // The preview's gate once measured the raw cluster (two
        // cells) and passed a line that paints at eight; it now
        // truncates under the same budget, so the collapsed notice
        // never renders wider than the pane it sits in.
        let summary =
            "## Kept\n\nkept \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} decisions follow"
                .to_string();
        let preview = summary_preview_line(&summary, 28).expect("a content line exists");
        assert!(
            preview.ends_with('…'),
            "the over-budget line truncates: {preview:?}"
        );
        assert!(
            display_width(&preview) <= 26,
            "the preview fits the budget it paints against: {}",
            display_width(&preview)
        );
    }

    #[test]
    fn a_wide_script_summary_preview_caps_by_display_cells() {
        // Forty wide glyphs read as forty characters but spend eighty
        // cells; the preview must fit the pane's cells, ellipsis
        // reserved inside the budget — the same measure every column
        // fit keeps.
        let summary = format!("## Conversation summary (compacted)\n{}", "題".repeat(40));
        let preview = summary_preview_line(&summary, 78).expect("a content line");
        assert!(
            str_width(&preview) <= 78,
            "the preview fits the pane's cells, a wide glyph spending \
                 two: {} cells",
            str_width(&preview)
        );
        assert!(
            preview.ends_with('…'),
            "an over-long line is ellipsized inside the budget, never \
                 clipped past it"
        );
    }

    #[test]
    fn a_thinking_section_collapses_to_one_row_and_expands_to_dim_body() {
        let dim = Color::Rgb(1, 2, 3);
        let collapsed = thinking_section_lines("line one\nline two", false, 84_000, false, 40, dim);
        assert_eq!(collapsed.len(), 1, "collapsed renders only the summary row");
        let row = collapsed.first().expect("the summary row");
        let joined: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            joined.contains("▸ Thinking · 1m 24s"),
            "the summary row reads as one label: {joined}"
        );
        assert!(
            row.spans.iter().any(|span| span.content == "Thinking"
                && span
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::ITALIC)),
            "the Thinking word renders italic: {row:?}"
        );

        let expanded = thinking_section_lines("line one\nline two", false, 84_000, true, 40, dim);
        assert_eq!(expanded.len(), 3, "the row plus two body lines");
        for line in &expanded {
            let fg = line
                .style
                .fg
                .or_else(|| line.spans.first().and_then(|span| span.style.fg));
            assert_eq!(fg, Some(dim), "every thinking line carries the dim tone");
        }
        for line in expanded.iter().skip(1) {
            let italic = line
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC)
                || line.spans.iter().any(|span| {
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::ITALIC)
                });
            assert!(italic, "the thinking body renders italic: {line:?}");
        }

        let redacted = thinking_section_lines("", true, 500, true, 40, dim);
        assert!(
            redacted.iter().any(|line| line
                .spans
                .iter()
                .any(|span| span.content.contains(THINKING_REDACTED_PLACEHOLDER))),
            "withheld reasoning renders the placeholder, not a blank body"
        );
    }

    #[test]
    fn the_live_thinking_title_reads_the_running_register() {
        // While the stream runs, the title answers to the same
        // register a running tool's title carries — the theme's own
        // standing foreground, italic as thinking's identity — while
        // its elapsed span reads the light reply register; the
        // completed block greys both out with everything else. The
        // register is explicit, never the terminal's default: a
        // canvas-repainting theme cannot lean on a default foreground
        // belonging to the scheme the user launched under — the exact
        // white-on-white github_light shipped on a dark-scheme
        // terminal.
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.thinking.lock().expect("the thinking lock") = crate::observer::ThinkingStream {
            text: "reasoning in flight".to_string(),
            began: Some(Instant::now()),
            ..Default::default()
        };
        let (lines, _) = app.live_thinking_lines(80);
        let row = lines.first().expect("the marker row");
        let title = row
            .spans
            .iter()
            .find(|span| span.content.contains("Thinking"))
            .expect("the title span");
        assert_eq!(
            title.style.fg,
            Some(app.theme.ui.foreground),
            "the title carries the theme's standing foreground while running — \
                 the running tool's register, never the terminal default or the dim"
        );
        assert!(
            title
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC),
            "italic stays thinking's identity"
        );
        let elapsed = row
            .spans
            .iter()
            .find(|span| span.content.contains("·"))
            .expect("the elapsed span");
        assert_eq!(
            elapsed.style.fg,
            Some(app.theme.ui.assistant_message_fg),
            "the elapsed reads light while the stream runs — the \
                 theme's reply color"
        );
        let marker = row
            .spans
            .iter()
            .find(|span| span.content.contains("▾") || span.content.contains("▸"))
            .expect("the marker span");
        assert_eq!(
            marker.style.fg,
            Some(app.theme.ui.dim),
            "the marker keeps the dim tone"
        );
        // Completed: the transcript's block greys the title out.
        let settled =
            thinking_section_lines("done reasoning", false, 1200, false, 80, app.theme.ui.dim);
        let settled_title = settled
            .first()
            .expect("the settled marker row")
            .spans
            .iter()
            .find(|span| span.content.contains("Thinking"))
            .expect("the settled title");
        assert_eq!(
            settled_title.style.fg,
            Some(app.theme.ui.dim),
            "the completed block's title is greyed out"
        );
    }

    use crate::app::TuiApp;
    use crate::app::test_support::*;
    use crate::cells::{display_width, str_width};
    use crate::commands::{self};
    use crate::message::TuiMessage;
    use crate::observer::{CompactionEvent, CompactionFiling, HealthNotice, TurnPublication};

    #[test]
    fn a_running_compaction_owns_the_live_lane_like_thinking() {
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
        *state.health.lock().unwrap() = HealthNotice::Compacting {
            began: std::time::Instant::now(),
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let showing = buffer_text(terminal.backend().buffer());
        assert!(
            showing.contains("▸ Compacting ·"),
            "the running pass renders in the thinking row's register, \
             marker and label and running duration: {showing}"
        );
        assert!(
            !showing.contains("compacting context ("),
            "the old notice-row wording is gone: {showing}"
        );

        *state.health.lock().unwrap() = HealthNotice::Idle;
        terminal.draw(|frame| app.render(frame)).unwrap();
        let cleared = buffer_text(terminal.backend().buffer());
        assert!(
            !cleared.contains("Compacting"),
            "the lane clears with the pass: {cleared}"
        );
    }

    #[test]
    fn a_crafted_compaction_summary_bakes_clean_into_its_rows() {
        // The pass's own words are producer-controlled prose. A
        // summary carrying escape sequences must reach the rows as
        // the words alone — the collapsed preview and the expanded
        // body both. The pin reads the debris the old code left: the
        // terminal drops the escape byte itself, so what painted was
        // the sequence's parameter bytes beside the words, and the
        // rows' width was measured on them.
        let mut app = app();
        app.push_message(TuiMessage::Compacted {
            tokens_before: 16_686,
            tokens_after: 4_231,
            reason: "threshold".to_string(),
            evicted: 7,
            duration_ms: 312,
            compactor: Some("qa".to_string()),
            compression_ratio: Some(3.9),
            // The OSC carries its BEL terminator: an unterminated
            // string sequence rightly runs to the end of the text.
            summary: Some("kept \u{1b}[2J the ledger\nand \u{1b}]11;?\u{07} this too".to_string()),
            timestamp: chrono::Utc::now(),
        });
        for expanded in [false, true] {
            app.expansion.expanded_compactions.clear();
            if expanded {
                app.expansion.expanded_compactions.insert("m0".to_string());
            }
            // The click's own half: the expansion repaints through a
            // generation bump, or the bake keeps serving collapsed rows.
            app.bake.conversation_generation = app.bake.conversation_generation.saturating_add(1);
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24))
                .expect("terminal");
            terminal.draw(|frame| app.render(frame)).expect("draw");
            let painted = buffer_text(terminal.backend().buffer());
            assert!(
                !painted.contains("[2J") && !painted.contains("]11;?"),
                "no escape parameter bytes paint as debris in the {} rows: {painted:?}",
                if expanded { "expanded" } else { "collapsed" }
            );
            assert!(
                painted.contains("kept") && painted.contains("the ledger"),
                "the words on either side of the sequence stand: {painted:?}"
            );
            if expanded {
                assert!(
                    painted.contains("this too"),
                    "the second line's words stand too: {painted:?}"
                );
            }
        }
    }

    #[test]
    fn a_lone_carriage_return_in_a_summary_splits_before_the_preview_measures() {
        // A lone carriage return inside a source line: the sanitizer
        // folds it into a line break, so sanitizing after the split
        // handed one "line" an embedded newline — the terminal then
        // dropped the byte and glued the two halves into one word.
        // Sanitizing the whole summary first makes the fold a real
        // split: the preview reads the first half alone, and the
        // expanded body shows the halves on their own rows.
        let mut app = app();
        app.push_message(TuiMessage::Compacted {
            tokens_before: 16_686,
            tokens_after: 4_231,
            reason: "threshold".to_string(),
            evicted: 7,
            duration_ms: 312,
            compactor: None,
            compression_ratio: None,
            summary: Some("head\rfoot".to_string()),
            timestamp: chrono::Utc::now(),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            collapsed.contains("head") && !collapsed.contains("headfoot"),
            "the collapsed preview reads the first half alone, never the glued pair: {collapsed:?}"
        );
        app.expansion.expanded_compactions.insert("m0".to_string());
        app.bake.conversation_generation = app.bake.conversation_generation.saturating_add(1);
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let expanded = buffer_text(terminal.backend().buffer());
        let head_row = expanded
            .lines()
            .any(|row| row.contains("head") && !row.contains("foot"));
        let foot_row = expanded
            .lines()
            .any(|row| row.contains("foot") && !row.contains("head"));
        assert!(
            head_row && foot_row,
            "the expanded body shows the halves on their own rows: {expanded:?}"
        );
    }

    #[test]
    fn a_compaction_notice_renders_collapsed_and_expands_to_its_details() {
        let mut app = app();
        app.push_message(TuiMessage::Compacted {
            tokens_before: 16_686,
            tokens_after: 4_231,
            reason: "threshold".to_string(),
            evicted: 7,
            duration_ms: 312,
            compactor: Some("qa".to_string()),
            compression_ratio: Some(3.9),
            summary: Some("The session mapped the login flow and fixed the retry loop".to_string()),
            timestamp: chrono::Utc::now(),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            collapsed.contains("Compacted · <1s"),
            "the collapsed row carries the pass duration like every other block: {collapsed}"
        );
        assert!(
            collapsed.contains("mapped the login flow"),
            "the collapsed row peeks at the pass's own words — the text \
             the model continues from, without the expansion: {collapsed}"
        );
        assert!(
            !collapsed.contains("16686"),
            "the token counts wait for the expansion: {collapsed}"
        );
        assert!(!collapsed.contains("messages evicted"));

        app.expansion.expanded_compactions.insert("m0".to_string());
        app.bake.conversation_generation = app.bake.conversation_generation.saturating_add(1);
        terminal.draw(|frame| app.render(frame)).expect("redraw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(expanded.contains("16686 → 4231 tokens"), "{expanded}");
        assert!(expanded.contains("saved 12455"), "{expanded}");
        assert!(expanded.contains("7 messages evicted"), "{expanded}");
        assert!(expanded.contains("reason: threshold"), "{expanded}");
        assert!(expanded.contains("compactor: qa"), "{expanded}");
        assert!(expanded.contains("compression 3.9x"), "{expanded}");
        assert!(
            expanded.contains("The session mapped the login flow and fixed the retry loop"),
            "the summary the pass left the context under shows beside the stats: {expanded}"
        );
        assert!(
            expanded.contains("durable state is the findings file and todo list"),
            "the expanded details remind the reader what survived: {expanded}"
        );
    }

    #[test]
    fn a_baked_compaction_notice_expands_on_its_click() {
        // The notice sits mid-conversation — a later message follows
        // it — so its rows were baked once with the collapsed flag;
        // only the tail re-renders each frame. The click must rebake,
        // else the expansion never shows: the shape the lone-tail pin
        // above cannot distinguish.
        let mut app = app();
        app.push_message(TuiMessage::Compacted {
            tokens_before: 16_686,
            tokens_after: 4_231,
            reason: "threshold".to_string(),
            evicted: 7,
            duration_ms: 312,
            compactor: Some("qa".to_string()),
            compression_ratio: Some(3.9),
            summary: None,
            timestamp: chrono::Utc::now(),
        });
        app.push_message(TuiMessage::User {
            text: "after the notice".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            collapsed.contains("Compacted · <1s"),
            "the baked notice draws collapsed: {collapsed}"
        );
        assert!(
            !collapsed.contains("16686"),
            "the token counts wait for the expansion: {collapsed}"
        );
        // The click lands on the notice's own summary row, resolved
        // through the same map and view the handler reads.
        let (&line, _) = app
            .bake
            .compaction_summary_lines
            .iter()
            .next()
            .expect("the notice registered its summary row");
        let view = app.pointer.last_view.expect("the render left its view");
        let row = view
            .area
            .y
            .saturating_add(u16::try_from(line.saturating_sub(view.skip)).expect("row on screen"));
        assert!(
            app.toggle_compaction_at(view.area.x, row),
            "the press belongs to the notice"
        );
        terminal.draw(|frame| app.render(frame)).expect("redraw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(
            expanded.contains("16686 → 4231 tokens"),
            "the click rebakes the notice's rows — a mid-conversation \
             expansion cannot ride the tail re-render: {expanded}"
        );
    }

    #[test]
    fn a_compaction_row_takes_its_summary_from_the_folded_history_head() {
        let mut app = app();
        let folded = std::sync::Arc::new(vec![loopctl::message::Message::user(
            "## Conversation summary (compacted)\n- the work stood here",
        )]);
        app.state
            .turn_histories
            .lock()
            .expect("the turn-history lock")
            .push(TurnPublication {
                passes: 1,
                history: std::sync::Arc::clone(&folded),
            });
        app.state
            .compactions
            .lock()
            .expect("the compaction lock")
            .push(CompactionEvent {
                pass: 1,
                record: TuiMessage::Compacted {
                    tokens_before: 10,
                    tokens_after: 5,
                    reason: "threshold exceeded".to_string(),
                    evicted: 3,
                    duration_ms: 100,
                    compactor: Some("structured".to_string()),
                    compression_ratio: Some(2.0),
                    summary: None,
                    timestamp: chrono::Utc::now(),
                },
            });
        app.drain_shared_state();
        match app.conversation().last() {
            Some(TuiMessage::Compacted { summary, .. }) => assert!(
                summary
                    .as_deref()
                    .is_some_and(|text| text.starts_with("## Conversation summary")),
                "the row carries the pass's own words: {summary:?}"
            ),
            other => panic!("the compacted row landed: {other:?}"),
        }
    }

    #[test]
    fn a_compaction_row_never_takes_a_pre_fold_historys_head() {
        let mut app = app();
        app.state
            .turn_histories
            .lock()
            .expect("the turn-history lock")
            .push(TurnPublication {
                passes: 1,
                history: std::sync::Arc::new(vec![loopctl::message::Message::user(
                    "the original first prompt",
                )]),
            });
        app.state
            .compactions
            .lock()
            .expect("the compaction lock")
            .push(CompactionEvent {
                pass: 1,
                record: TuiMessage::Compacted {
                    tokens_before: 10,
                    tokens_after: 5,
                    reason: "threshold exceeded".to_string(),
                    evicted: 3,
                    duration_ms: 100,
                    compactor: None,
                    compression_ratio: None,
                    summary: None,
                    timestamp: chrono::Utc::now(),
                },
            });
        app.drain_shared_state();
        match app.conversation().last() {
            Some(TuiMessage::Compacted { summary, .. }) => assert!(
                summary.is_none(),
                "a pre-fold history's head is conversation, not the pass's \
                 output — the row keeps waiting: {summary:?}"
            ),
            other => panic!("the compacted row landed: {other:?}"),
        }
    }

    #[test]
    fn a_manual_compactions_row_takes_its_summary_from_its_own_filing() {
        // The manual pass owns no turn end to carry its publication, so
        // the driver files the folded feed in the pass's own channel —
        // taken once per summary-less row, never shared.
        let mut app = app();
        let folded = std::sync::Arc::new(vec![loopctl::message::Message::user(
            "## Conversation summary (compacted)\n- the folded ledger",
        )]);
        app.state
            .compaction_publications
            .lock()
            .expect("the publications lock")
            .push(CompactionFiling {
                pass: 1,
                folded: std::sync::Arc::clone(&folded),
            });
        app.state
            .compactions
            .lock()
            .expect("the compaction lock")
            .push(CompactionEvent {
                pass: 1,
                record: TuiMessage::Compacted {
                    tokens_before: 10,
                    tokens_after: 5,
                    reason: "manual".to_string(),
                    evicted: 3,
                    duration_ms: 100,
                    compactor: Some("structured".to_string()),
                    compression_ratio: Some(2.0),
                    summary: None,
                    timestamp: chrono::Utc::now(),
                },
            });
        app.drain_shared_state();
        match app.conversation().last() {
            Some(TuiMessage::Compacted { summary, .. }) => assert!(
                summary
                    .as_deref()
                    .is_some_and(|text| text.starts_with("## Conversation summary")),
                "the manual row carries the pass's own words: {summary:?}"
            ),
            other => panic!("the compacted row landed: {other:?}"),
        }
        assert!(
            app.state
                .compaction_publications
                .lock()
                .expect("the publications lock")
                .is_empty(),
            "a filing serves exactly the row that drained it"
        );
    }

    #[test]
    fn a_compaction_row_ignores_the_standing_history() {
        // Past the first fold the standing machine history's head is a
        // marked summary that outlives the pass that coined it — a
        // resumed session seeds the slot with the previous
        // conversation's words. A new summary-less row must not read
        // it: only a publication attributable to the pass settles the
        // debt, so the row keeps waiting here.
        let mut app = app();
        let stale = std::sync::Arc::new(vec![loopctl::message::Message::user(
            "## Conversation summary (compacted)\n- a previous pass's ledger",
        )]);
        *app.state
            .machine_history
            .lock()
            .expect("the machine-history lock") = Some(std::sync::Arc::clone(&stale));
        app.state
            .compactions
            .lock()
            .expect("the compaction lock")
            .push(CompactionEvent {
                pass: 1,
                record: TuiMessage::Compacted {
                    tokens_before: 10,
                    tokens_after: 5,
                    reason: "threshold exceeded".to_string(),
                    evicted: 3,
                    duration_ms: 100,
                    compactor: Some("structured".to_string()),
                    compression_ratio: Some(2.0),
                    summary: None,
                    timestamp: chrono::Utc::now(),
                },
            });
        app.drain_shared_state();
        match app.conversation().last() {
            Some(TuiMessage::Compacted { summary, .. }) => assert!(
                summary.is_none(),
                "the standing cache's marked head is a previous pass's \
                 words — the row keeps its debt: {summary:?}"
            ),
            other => panic!("the compacted row landed: {other:?}"),
        }
    }

    #[test]
    fn the_live_thinking_body_renders_italic_in_the_dim_tone() {
        // The live lane must read the same register the completed
        // sections do: dim plus italic. The default theme here is the
        // transparent one, so the tone asserted is that theme's fixed
        // neutral gray — the deterministic grey-out that keeps
        // reasoning from matching the reply's foreground.
        let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
        let app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
        state
            .agent_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.thinking.lock().expect("the thinking lock") = crate::observer::ThinkingStream {
            text: "reasoning in flight".to_string(),
            began: Some(Instant::now()),
            ..Default::default()
        };
        let (lines, first_row) = app.live_thinking_lines(80);
        assert_eq!(first_row, Some(0), "the marker row registers for clicks");
        assert!(
            lines.len() >= 2,
            "the marker row plus the expanded body: {lines:?}"
        );
        let joined: String = lines
            .first()
            .expect("the marker row")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            joined.contains("Thinking"),
            "the marker row labels the stream: {joined}"
        );
        for line in lines.iter().skip(1) {
            let fg = line
                .style
                .fg
                .or_else(|| line.spans.first().and_then(|span| span.style.fg));
            assert_eq!(
                fg,
                Some(app.theme.ui.dim),
                "the live body carries the theme's dim tone: {line:?}"
            );
            let italic = line
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC)
                || line.spans.iter().any(|span| {
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::ITALIC)
                });
            assert!(italic, "the live body renders italic: {line:?}");
        }
    }

    #[test]
    fn the_popup_command_rows_carry_the_help_summary() {
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        // The popup's full summary belongs to the docked composer's
        // width — the welcome's four-fifths field truncates it — so
        // the rows ride a landed prompt.
        subject.push_message(TuiMessage::User {
            text: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        subject.composer.input.set_text("/".to_string());
        let rows = screen_rows(&mut subject);
        let (_, candidates, _) = subject.fresh_completion("/");
        let mut columns: Vec<usize> = Vec::new();
        for name in candidates.iter().take(6) {
            let summary = commands::summary_for(name).expect("every listed command has a summary");
            let row = rows
                .iter()
                .find(|row| row.contains(&format!("/{name} ")))
                .unwrap_or_else(|| panic!("the popup lists /{name}, rows: {rows:?}"));
            let column = row.find(summary).unwrap_or_else(|| {
                panic!("the /{name} row carries its /help summary {summary:?}, got {row:?}")
            });
            columns.push(column);
        }
        let first = columns.first().copied().unwrap_or(0);
        assert!(
            columns.iter().all(|&column| column == first),
            "every summary starts in the same column, got {columns:?} (rows: {rows:?})"
        );
    }

    #[test]
    fn the_tab_notice_names_the_choice_and_nothing_else() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        // Themes are silent — the palette change is the feedback — so
        // the notice's shape is verified through a key completion.
        app.composer.input.set_text("/verbosity ".to_string());
        // The first Tab from a bare space browses without applying;
        // the second applies the first candidate and carries the notice.
        assert!(app.complete_command_line(false));
        assert!(app.complete_command_line(false));
        let notice = app
            .transient_notice
            .as_ref()
            .map_or_default(|(text, _)| text.clone());
        assert!(
            !notice.is_empty() && !notice.contains("more") && !notice.contains(','),
            "the notice is the chosen value alone, no list, no tally: {notice}"
        );

        // A theme completion is silent: the palette previews live and
        // no notice rides on top of what the user is already seeing.
        let mut theme_app = TuiApp::new(dch_config::DchConfig::default());
        theme_app.composer.input.set_text("/theme ".to_string());
        assert!(theme_app.complete_command_line(false));
        let silent = theme_app
            .transient_notice
            .as_ref()
            .map_or_default(|(text, _)| text.clone());
        assert!(
            silent.is_empty(),
            "a theme completion posts no notice — the preview is the feedback: {silent}"
        );
    }

    #[test]
    fn a_set_of_the_api_key_masks_the_value_in_the_notice() {
        // The set-side twin of the `/config` and `/unset` masks: a
        // secret's value must never ride the notice lane — the notice is
        // a persisted message, so the raw key would outlive the moment
        // the user typed it. The set itself still lands; only its echo
        // is masked, and the clearing spelling `unset` stays legible.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/set api_key sk-live-supersecret");
        assert!(out.contains("••••"), "the notice masks the secret: {out:?}");
        assert!(
            !out.contains("sk-live-supersecret"),
            "the raw secret never rides the notice: {out:?}"
        );
        assert_eq!(
            app.config.api.api_key.as_deref(),
            Some("sk-live-supersecret"),
            "the masked echo still lands the real value"
        );
    }

    #[test]
    fn compact_sends_the_manual_pass_to_the_driver() {
        // The manual seam: `/compact` sends the driver the compact
        // command, the engine's own context manager folds the history
        // between runs, and the next turn continues from the compacted
        // context. The display's notice stays free of token figures —
        // the driver's own logging carries those.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_submit_tx(tx);
        let out = command_output(&mut app, "/compact");
        let sent = rx
            .try_recv()
            .expect("the compact command reaches the driver");
        assert!(
            matches!(
                sent,
                crate::queue::Submission::Command {
                    command: crate::queue::SessionCommand::Compact { .. }
                }
            ),
            "the driver gets the manual pass: {sent:?}"
        );
        assert!(
            out.is_empty() || !out.contains("automatic"),
            "the command sends the pass rather than narrating the automatic one: {out:?}"
        );
    }

    #[test]
    fn a_notice_from_the_driver_lands_on_the_notice_row() {
        // The driver has no other way to be heard: a refusal, a failure,
        // or a result arrives as plain text and posts where the user
        // reads.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.apply_session_update(crate::queue::SessionUpdate::Notice {
            text: "the loop kept its model".to_string(),
        });
        let (notice, _) = app.transient_notice.as_ref().expect("a notice stands");
        assert!(notice.contains("kept its model"), "got {notice:?}");
    }

    #[test]
    fn a_stolen_printable_key_lands_in_the_composer_and_other_bytes_notice() {
        // The pause handshake costs latency, never input: a printable
        // keystroke the query consumed returns to the composer as if
        // typed, and a control byte is said aloud rather than dropped.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.return_stolen_byte(b'x');
        assert!(
            app.composer.input.text().contains('x'),
            "the printable keystroke reaches the composer"
        );
        app.return_stolen_byte(0x03);
        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("a consumed control byte is acknowledged");
        assert!(notice.contains("consumed"), "got {notice:?}");
    }

    #[test]
    fn the_wrap_funnel_breaks_on_carriage_returns_and_strips_controls() {
        // The funnel is the last seam before a cell: a lone CR breaks
        // the row (the contract the capture-side sanitizer documents),
        // tabs expand, and no other control byte survives — the class
        // of bytes that once walked a pane's writes off their rows
        // mid-scroll.
        let lines = plain_wrapped_lines_styled("a\r\nb\rc\u{7}d\te", 20, Style::default());
        let rows: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            rows,
            ["a", "b", "cd    e"],
            "CR is a break, controls drop, tabs expand: {rows:?}"
        );
    }

    #[test]
    fn the_session_502_bakes_into_a_spacer_a_header_and_clean_rows() {
        // A provider's HTML error page with CRLF pairs — the edge's
        // body included. The block must open on a row of its own, name
        // the failure's facts in a header, and leave none of the page's
        // escape sequences behind: every row free of them and bounded
        // by the wrap width.
        let raw = "API error: stream (stream failed before completing after 4 attempts: \
HTTP error: HTTP 502: <html>\r\n<head><title>502 Bad Gateway</title></head>\r\n\
<body bgcolor=\"white\">\r\n<center><h1>502 Bad Gateway</h1></center>\r\n\
<hr><center>alibaba-ga</center>\r\n</body>\r\n</html>\r\n) \
and fallback failed: HTTP error: HTTP 502: upstream reset";
        let color = Color::Rgb(200, 30, 30);
        let lines = error_block_lines(raw, 80, color, true);
        let joined = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let spacer = lines.first().expect("the spacer row");
        assert_eq!(joined(spacer), "", "the block opens on a blank spacer");
        let header = joined(lines.get(1).expect("the header row"));
        assert!(
            header.contains("✗ model request failed"),
            "the header names the failure: {header}"
        );
        assert!(
            header.contains("4 attempts"),
            "the attempt count survives the parse: {header}"
        );
        assert!(
            header.contains("HTTP 502"),
            "the status survives the parse: {header}"
        );
        assert!(
            header.contains("(alibaba-ga)"),
            "the gateway tag survives the parse: {header}"
        );
        for (index, line) in lines.iter().enumerate() {
            let row = joined(line);
            assert!(
                !row.chars().any(char::is_control),
                "row {index} carries no control characters: {row:?}"
            );
            assert!(
                str_width(&row) <= 80,
                "row {index} stays inside the wrap width: {} cells",
                str_width(&row)
            );
        }
        assert!(
            lines
                .iter()
                .any(|line| joined(line).contains("502 Bad Gateway")),
            "the persisted detail still renders under the header"
        );
    }

    #[test]
    fn a_gateway_tag_longer_than_the_pane_wraps_into_the_width() {
        // The header is the block's one provider-sized span: an edge
        // whose tag outgrows the pane wraps like the detail does —
        // every row inside the width, the whole tag still present —
        // instead of silently losing its tail to the clip.
        let tag = "a".repeat(120);
        let raw = format!("API error: HTTP error: HTTP 502: <center>{tag}</center>");
        let lines = error_block_lines(&raw, 40, Color::Rgb(1, 2, 3), false);
        let joined = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let rows: Vec<String> = lines.iter().map(joined).collect();
        assert!(
            rows.len() > 1,
            "an over-wide header folds into several rows, not one clipped row"
        );
        for (index, row) in rows.iter().enumerate() {
            assert!(
                str_width(row) <= 40,
                "row {index} stays inside the width: {} cells",
                str_width(row)
            );
        }
        assert!(
            rows.concat().contains(&tag),
            "the whole tag survives the fold: {rows:?}"
        );
    }

    #[test]
    fn a_crafted_gateway_tag_bakes_clean_into_the_header() {
        // The header's one provider-controlled span: an edge page that
        // signs itself with terminal control bytes inside the
        // `<center>` mark. The detail rows were always sanitized; the
        // header must be too — the tag is parsed out of the raw body
        // and interpolated verbatim, so the display policy applies at
        // the parse, or the bytes the block exists to keep off cells
        // ride the header in.
        let raw = "API error: stream failed before completing after 2 attempts: \
HTTP error: HTTP 500: <center>\u{1b}[2J\u{1b}[Hoops</center>";
        let lines = error_block_lines(raw, 80, Color::Rgb(1, 2, 3), false);
        let joined = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let header = joined(lines.first().expect("the header row"));
        assert!(
            header.contains("(oops)"),
            "the tag's readable text survives the sanitize — the H belongs \
             to the \\x1b[H escape the policy strips: {header}"
        );
        assert!(
            !header.chars().any(|ch| ch.is_control() || ch == '\u{1b}'),
            "the header carries no control bytes: {header:?}"
        );
        assert!(
            !header.contains("[2J") && !header.contains("[H"),
            "the escape sequences the crafted page carried are gone: {header:?}"
        );
    }

    #[test]
    fn a_gateway_tag_broken_across_rows_stays_one_header_row() {
        // The sanitizer spells a carriage return as a break, and the
        // header interpolates the tag on one line — a line nothing
        // splits on, unlike the detail rows beneath it. A tag the edge
        // page broke across its rows must collapse to spaces there, or
        // the break byte rides a cell mid-frame.
        let raw = "API error: HTTP error: HTTP 503: <center>ali\r\nbaba-ga</center>";
        let lines = error_block_lines(raw, 80, Color::Rgb(1, 2, 3), false);
        let header = lines
            .first()
            .expect("the header row")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            header.contains("(ali baba-ga)"),
            "the tag's broken rows joined inside the parens: {header:?}"
        );
        assert!(
            !header.chars().any(char::is_control),
            "the header carries no control bytes: {header:?}"
        );
    }

    #[test]
    fn a_prose_failure_renders_as_its_text_without_a_header() {
        // A cancellation or a compaction notice is prose: no API
        // shape, no header, no indent — only the spacer separating the
        // block from the rows above it.
        let lines = error_block_lines("Agent cancelled", 40, Color::Rgb(1, 2, 3), true);
        let joined = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(
            lines.len(),
            2,
            "the spacer and the text, nothing else: {:?}",
            lines.iter().map(&joined).collect::<Vec<_>>()
        );
        assert_eq!(joined(lines.first().expect("the spacer")), "");
        assert_eq!(
            joined(lines.get(1).expect("the text row")),
            "Agent cancelled",
            "prose renders verbatim, unindented"
        );
    }

    #[test]
    fn the_first_rows_of_a_bake_take_no_spacer() {
        // An error that opens the pane — a resume whose transcript
        // ends in a failure — starts at the top row, not one below it.
        let lines = error_block_lines("provider unreachable", 40, Color::Rgb(1, 2, 3), false);
        let joined = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(
            joined(lines.first().expect("the first row")),
            "provider unreachable",
            "no spacer precedes the pane's first row"
        );
    }

    #[test]
    fn a_status_without_attempts_still_names_the_status() {
        // A single-shot failure never reaches the attempt phrase, but
        // its status is exactly the fact the header exists to carry —
        // and no count is invented around it.
        let lines = error_block_lines(
            "API error: HTTP error: HTTP 503: upstream unavailable",
            40,
            Color::Rgb(1, 2, 3),
            true,
        );
        let header = lines
            .get(1)
            .expect("the header row")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            header.contains("HTTP 503"),
            "the status alone heads the block: {header}"
        );
        assert!(
            !header.contains("attempts"),
            "no count is invented: {header}"
        );
    }

    #[test]
    fn a_tool_recovery_shape_mints_no_model_request_header() {
        // The attempt phrase belongs to the API shape alone: a
        // tool-recovery failure carries `after {n} attempts` too, and
        // without the API-prefix gate it would be misattributed to a
        // model request that never happened.
        let lines = error_block_lines(
            "Tool recovery exhausted after 3 attempts: bash",
            40,
            Color::Rgb(1, 2, 3),
            true,
        );
        assert!(
            lines.iter().all(|line| !line
                .spans
                .iter()
                .any(|span| span.content.contains("model request"))),
            "no header is minted outside the API shape"
        );
    }
}
