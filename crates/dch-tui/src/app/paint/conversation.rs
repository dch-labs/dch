//! The conversation pane's painters.
//!
//! The layered stack the reader scrolls — its windowed view, the
//! user-turn chrome, the scrollbar in its reserved gutter — and the
//! helpers that shape it: the pane split, the furniture constants,
//! and the display-width policy the viewport passes on its way to
//! the screen.
use ratatui::style::Style;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::cells::{carries_policy_input, first_retained_offset, for_display, painted_steps};

use super::super::selection::{ViewState, reverse_selection};
use super::super::{SelectableShape, TuiApp};

impl TuiApp {
    /// Render the conversation pane: the transcript's windowed view
    /// and everything live that rides it.
    ///
    /// The pane is the layered stack the reader scrolls — the baked
    /// conversation cache, the speaker-change seam, the live lanes in
    /// conversation order (thinking rows, the streaming reply's
    /// frozen and live regions, compaction notices), the running tool
    /// rows, the turn's ticking timer footer, and — just above that
    /// timer, in the spot they will land in — a steer waiting for the
    /// model to read it, pinned as a user row — windowed to the pane's
    /// height around
    /// the scroll position, so a keystroke's cost tracks the window,
    /// never the session. The settled turns' chrome (their raised
    /// boxes and chevrons) paints from the cached turn ranges over
    /// the same window; the selection reverses in place; the
    /// scrollbar rides the gutter when the stack outgrows the pane.
    /// The view state this records is the click-mapping every later
    /// pointer event trusts.
    pub(in crate::app) fn render_conversation(&mut self, frame: &mut Frame, area: Rect) {
        let padded = Rect {
            y: area.y.saturating_add(CONVERSATION_TOP_INSET),
            height: area.height.saturating_sub(CONVERSATION_TOP_INSET),
            ..area
        };
        let (pane, scrollbar_area) = split_scrollbar_gutter(padded);
        let mark_gutter = (pane.width
            >= CONVERSATION_MARK_MIN_TEXT
                .saturating_add(CONVERSATION_MARK_GUTTER)
                .saturating_add(CONVERSATION_RIGHT_INSET)
                .saturating_add(CONVERSATION_BOX_MARGIN))
        .then_some(Rect {
            width: CONVERSATION_MARK_GUTTER,
            ..pane
        });
        let text_area = mark_gutter.as_ref().map_or(pane, |gutter| Rect {
            x: gutter.right(),
            width: pane.width.saturating_sub(CONVERSATION_MARK_GUTTER),
            ..pane
        });
        let text_area = Rect {
            width: text_area
                .width
                .saturating_sub(CONVERSATION_RIGHT_INSET)
                .saturating_sub(CONVERSATION_BOX_MARGIN),
            ..text_area
        };
        let conversation_height = text_area.height as usize;
        let width = text_area.width.max(1);
        if self.bake.conversation_cache_generation != self.bake.conversation_generation
            || self.bake.conversation_cache_width != width
        {
            self.bake_conversation_cache(text_area, width);
        }

        let streaming_changed = self.refresh_streaming_region(width);
        let (tools, running_rows) = self.active_tool_lines(width);
        let thinking_rows = self.live_thinking_lines(width);
        let compaction_rows = self.live_compaction_lines();
        let live_open = !thinking_rows.0.is_empty()
            || !self.freeze.frozen_lines.is_empty()
            || !self.freeze.live_lines.is_empty()
            || !compaction_rows.is_empty()
            || !tools.is_empty();
        let seam: Vec<Line<'static>> = if !self.bake.conversation_cache.is_empty() && live_open {
            vec![Line::from("")]
        } else {
            Vec::new()
        };
        let shape: SelectableShape = (
            self.bake.conversation_cache.len(),
            seam.len(),
            thinking_rows.0.len(),
            self.freeze.frozen_lines.len(),
            self.freeze.live_lines.len(),
            compaction_rows.len(),
            tools.len(),
        );
        self.settle_selectable_shape(streaming_changed, shape);
        self.thinking_live_rows = thinking_rows.1.map(|row| {
            let start = row
                .saturating_add(self.bake.conversation_cache.len())
                .saturating_add(seam.len());
            start..start.saturating_add(thinking_rows.0.len())
        });
        let pre_tool_offset = self
            .bake
            .conversation_cache
            .len()
            .saturating_add(seam.len())
            .saturating_add(thinking_rows.0.len())
            .saturating_add(self.freeze.frozen_lines.len())
            .saturating_add(self.freeze.live_lines.len())
            .saturating_add(compaction_rows.len());
        self.tools.tool_running_lines = running_rows
            .into_iter()
            .map(|(index, call_id)| (index.saturating_add(pre_tool_offset), call_id))
            .collect();
        let run_timer = self.live_run_timer();
        let pending_steer_rows = self.pending_steer_lines(width);
        let lane_end = self
            .bake
            .conversation_cache
            .len()
            .saturating_add(seam.len())
            .saturating_add(thinking_rows.0.len())
            .saturating_add(self.freeze.frozen_lines.len())
            .saturating_add(self.freeze.live_lines.len())
            .saturating_add(compaction_rows.len())
            .saturating_add(tools.len());
        let pinned_steer = (!pending_steer_rows.is_empty())
            .then(|| (lane_end, lane_end.saturating_add(pending_steer_rows.len())));
        let total_lines = lane_end
            .saturating_add(pending_steer_rows.len())
            .saturating_add(run_timer.len());
        self.settle_scroll_offset(total_lines, conversation_height);
        let skip = self.snap_skip_to_whole_todo_blocks(
            total_lines
                .saturating_sub(conversation_height)
                .saturating_sub(self.scroll.scroll_offset),
            conversation_height,
        );
        frame.render_widget(ratatui::widgets::Clear, pane);
        self.paint_user_turn_chrome(
            frame,
            pane,
            mark_gutter,
            skip,
            conversation_height,
            pinned_steer,
        );
        self.lane.live_seam_lines = seam;
        self.lane.live_thinking_cache = thinking_rows.0;
        self.lane.live_compaction_cache = compaction_rows;
        self.lane.live_tools_cache = tools;
        let segments: Vec<&[Line<'_>]> = vec![
            &self.bake.conversation_cache,
            &self.lane.live_seam_lines,
            &self.lane.live_thinking_cache,
            &self.freeze.frozen_lines,
            &self.freeze.live_lines,
            &self.lane.live_compaction_cache,
            &self.lane.live_tools_cache,
            &pending_steer_rows,
            &run_timer,
        ];
        let mut visible = visible_window(&segments, skip, conversation_height);
        self.pointer.last_view = Some(ViewState {
            area: text_area,
            skip,
            total: total_lines,
            selectable: lane_end,
        });
        if let Some(selection) = &self.pointer.selection {
            reverse_selection(&mut visible, skip, selection);
        }
        for_display_lines(&mut visible);
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

    /// Paint the settled user turns' chrome: each turn's raised box
    /// and the chevron at its opening text row.
    ///
    /// The box keeps [`CONVERSATION_BOX_MARGIN`] of the pane's own
    /// background outside itself on both sides and spans the turn's
    /// padding rows and text; the chevron sits a column inside the
    /// box's left edge. Both are painted from the cached turn ranges
    /// against this frame's window, so turns off-screen paint
    /// nothing.
    fn paint_user_turn_chrome(
        &self,
        frame: &mut Frame,
        pane: Rect,
        mark_gutter: Option<Rect>,
        skip: usize,
        conversation_height: usize,
        pinned_steer: Option<(usize, usize)>,
    ) {
        let mut turns = self.bake.user_turn_ranges.clone();
        if let Some((start, end)) = pinned_steer {
            turns.push((start, end, start.saturating_add(1)));
        }
        for &(start, end, _) in &turns {
            let first_visible = start.max(skip);
            let last_visible = end.min(skip.saturating_add(conversation_height));
            if first_visible >= last_visible {
                continue;
            }
            let Some(y) = first_visible
                .checked_sub(skip)
                .and_then(|offset| u16::try_from(offset).ok())
            else {
                continue;
            };
            let Some(h) = last_visible
                .checked_sub(first_visible)
                .and_then(|rows| u16::try_from(rows).ok())
            else {
                continue;
            };
            let block = Rect {
                x: pane.x.saturating_add(CONVERSATION_BOX_MARGIN),
                width: pane
                    .width
                    .saturating_sub(CONVERSATION_BOX_MARGIN.saturating_mul(2)),
                y: pane.y.saturating_add(y),
                height: h,
            };
            frame
                .buffer_mut()
                .set_style(block, Style::default().bg(self.theme.ui.surface));
        }
        let Some(gutter) = mark_gutter else {
            return;
        };
        for &(_, _, mark) in &turns {
            let Some(on_screen) = mark.checked_sub(skip) else {
                continue;
            };
            let Ok(row) = u16::try_from(on_screen) else {
                continue;
            };
            if row < gutter.height {
                frame.buffer_mut()[(
                    gutter
                        .x
                        .saturating_add(CONVERSATION_BOX_MARGIN)
                        .saturating_add(1),
                    gutter.y.saturating_add(row),
                )]
                    .set_symbol("❯")
                    .set_fg(self.theme.ui.primary);
            }
        }
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

/// Blank columns a user turn's box keeps outside itself, between its
/// edges and the pane's.
///
/// The prompt box is a raised surface floating on the terminal's own
/// background, not a band pinned to the pane's edges: this much
/// regular background shows on the box's left and right alike, with
/// the scrollbar's gutter beyond. The chevron sits a column inside
/// the box's left edge, this margin in from the pane's.
const CONVERSATION_BOX_MARGIN: u16 = 1;

/// Columns the conversation reserves at its left edge before its
/// text starts.
///
/// The marker is chrome drawn into the gutter, never span content, so
/// selections and copies walk only the text. The width covers the
/// box's left margin, a pad column, the chevron, and the gap before
/// the text.
const CONVERSATION_MARK_GUTTER: u16 = 5;

/// The narrowest conversation text that still carries the marker
/// gutter beside it.
///
/// Below this the pane keeps its full width and draws no chevrons — a
/// marker that starves the text is worse than no marker.
const CONVERSATION_MARK_MIN_TEXT: u16 = 8;

/// Blank rows the conversation keeps between the pane's top edge and
/// its first line of text.
///
/// The transcript opens this far below the pane's edge — the same
/// breathing-room convention the composer and the status bar use — so
/// the topmost line never touches whatever sits above the pane. The
/// scrollbar track starts below the inset too: the track measures the
/// padded content window, not the raw pane.
const CONVERSATION_TOP_INSET: u16 = 1;

/// Blank columns a user turn's box keeps between its wrapped text and
/// its right edge, inside the box.
///
/// Wrapped user lines stop this far short of the box's edge, the
/// padding the prompt surface carries around its text; agent lines
/// share the stop so every message column aligns, leaving the box's
/// margin and the scrollbar's gutter blank beyond.
const CONVERSATION_RIGHT_INSET: u16 = 3;

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
pub(in crate::app) fn scrollbar_geometry(
    content: usize,
    viewport: usize,
    skip: usize,
) -> Option<(usize, usize)> {
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

/// Strip every composition input from the viewport about to reach
/// the screen.
///
/// The display half of the cell policy ([`for_display`]): selectors,
/// joiners, keycap marks, emoji modifiers, and flag tags all make
/// the model's width, the buffer's width, and some terminal class's
/// width disagree, and a diff-based renderer cannot survive the
/// disagreement — the cells the model believes a cluster consumed
/// keep whatever an earlier frame wrote there. The walk is flattened:
/// one policy pass over the line's concatenated spans, each surviving
/// grapheme re-attached to the span that owns its first retained
/// character — the
/// same single projection the selection's flip and copy walk — so a
/// cluster split across a span boundary strips exactly as it paints
/// and the row on the screen spends the cells the selection math
/// budgets. A per-span strip would keep a span-opening modifier as
/// its own ink, two cells the flattened walk never counted. The
/// viewport [`visible_window`] returns is already a per-frame clone,
/// so this mutates nothing a selection copies from; a line whose
/// spans carry no policy input — the common case — costs one scan
/// and no allocation.
pub(in crate::app) fn for_display_lines(lines: &mut [Line<'_>]) {
    for line in lines {
        if !line
            .spans
            .iter()
            .any(|span| carries_policy_input(span.content.as_ref()))
        {
            continue;
        }
        let mut flat = String::new();
        let mut owners: Vec<(usize, Style)> = Vec::new();
        for span in line.spans.drain(..) {
            owners.push((flat.len(), span.style));
            flat.push_str(span.content.as_ref());
        }
        let mut spans: Vec<Span<'_>> = Vec::with_capacity(owners.len().saturating_add(2));
        let mut chunk = String::new();
        let mut chunk_style = Style::default();
        let head_kept = first_retained_offset(&flat);
        for (range, _width) in painted_steps(&flat) {
            let Some(raw) = flat.get(range.clone()) else {
                continue;
            };
            let style_at = if range.start == 0 {
                head_kept
            } else {
                range.start
            };
            let style = owners
                .iter()
                .rev()
                .find(|(at, _)| *at <= style_at)
                .map_or(chunk_style, |(_, style)| *style);
            if !chunk.is_empty() && style != chunk_style {
                spans.push(Span::styled(std::mem::take(&mut chunk), chunk_style));
            }
            chunk.push_str(for_display(raw).as_ref());
            chunk_style = style;
        }
        if !chunk.is_empty() {
            spans.push(Span::styled(chunk, chunk_style));
        }
        line.spans = spans;
    }
}

/// Collect the viewport window across concatenated line segments.
///
/// The settled conversation renders from a cache that must survive the
/// frame, so instead of concatenating and cloning every segment this
/// walks them with the skip offset and clones only the lines the
/// viewport actually shows — per-frame cost tracks the pane height,
/// not the session length. The segments' borrow and their content are
/// decoupled lifetimes: the caller may assemble the window from
/// transient borrows of lines it keeps beyond them.
pub(in crate::app) fn visible_window<'a>(
    segments: &[&'a [Line<'a>]],
    skip: usize,
    height: usize,
) -> Vec<Line<'a>> {
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

#[cfg(test)]
mod tests {

    use super::scrollbar_geometry;
    use ratatui::text::{Line, Span};
    #[test]
    fn a_retained_opener_carries_its_own_spans_style() {
        // painted_steps opens the first step's range at byte zero so
        // the ranges tile the source — leading composition inputs
        // belong to the first grapheme for copying — but the style
        // lookup must key on the first retained character, the one
        // that paints. A selector opening the line inside its own
        // span mis-attributed the character after it to the
        // selector's style.
        use ratatui::style::{Color, Style};
        let selector_style = Style::default().fg(Color::Red);
        let text_style = Style::default().fg(Color::Green);
        let mut lines = vec![Line::from(vec![
            Span::styled("\u{fe0f}", selector_style),
            Span::styled("a b", text_style),
        ])];
        super::for_display_lines(&mut lines);
        let line = lines.first().expect("the fixture line survives the strip");
        let painted = line
            .spans
            .iter()
            .find(|span| span.content.contains('a'))
            .expect("the retained opener paints");
        assert_eq!(
            painted.style.fg,
            Some(Color::Green),
            "the retained opener carries its own span's style, not the removed selector's"
        );
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
}
