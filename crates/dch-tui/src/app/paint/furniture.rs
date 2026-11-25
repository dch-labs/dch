//! The frame's fixed furniture: the pulse strip, the todo and queue
//! sections, the notice row, the composer, the status bar, and the
//! working-directory seat.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::super::TuiApp;
use super::super::composer::{
    INPUT_SIDE_INSET, INPUT_TEXT_ROWS, INPUT_VERTICAL_PADDING, InputView,
};
use super::super::context::{
    CONTEXT_BAR_CELLS, STATUS_BAR_AIR, STATUS_BAR_MARGIN, context_bar_fill, context_bar_readout,
};
use super::super::notices::wrap_prompt_rows;
use super::super::pulse::{glow_eighths, pulse_span, scaled_color};
use super::super::queue::{QUEUE_ROWS, QUEUE_TITLE, QueueMode, QueuePosture, queue_prompt_prefix};
use super::super::todos::{PlanView, TODO_SECTION_BODY_ROWS, TODO_SECTION_INSET};
use super::super::{Color, Line, Paragraph, Span, StripStyle, Style, str_width};
use super::conversation::for_display_lines;
use crate::cells::{display_width, for_display};
use crate::todo_view;
use std::time::Instant;

impl TuiApp {
    /// Render the pinned pulse strip into its row.
    ///
    /// While the agent runs, the strip's configured row becomes the
    /// activity strip: the bar, full width, in the theme's
    /// composer-border color (the primary edge on themes without one
    /// — the same color the prompt box's left border paints). The row
    /// is the spacer directly above the composer by default, or the
    /// terminal's dedicated first row when the config says so; the
    /// glow style holds a static full-width bar whose color breathes,
    /// and the off style yields the row back, nothing drawn. Every
    /// frame the strip does not paint — the off style, a run at rest —
    /// clears the row first, so its last painted bar never lingers
    /// into the frames that follow. The bar
    /// anchors on the run's first frame and never re-anchors: streams
    /// beginning and graduating inside the run leave its phase
    /// untouched, so one continuous animation runs from the frame the
    /// run starts to the frame it ends. Returns whether the strip
    /// claimed the row, so the composer hairline can stand down for
    /// the frame when the row was its spacer.
    pub(in crate::app) fn render_pulse_strip(&self, frame: &mut Frame, area: Rect) -> bool {
        if matches!(self.display.strip_style, StripStyle::Off) {
            Self::strip_stood_down(frame, area);
            return false;
        }
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::Relaxed)
            || area.height == 0
        {
            Self::strip_stood_down(frame, area);
            return false;
        }
        let now = self.pulse_now();
        let base = self
            .theme
            .ui
            .composer_border
            .unwrap_or(self.theme.ui.primary);
        let anchor = self.run_anchor.unwrap_or(now);
        let color = if matches!(self.display.strip_style, StripStyle::Glow) {
            scaled_color(base, glow_eighths(anchor, now))
        } else {
            base
        };
        let pulse = pulse_span(
            self.display.strip_style,
            anchor,
            now,
            usize::from(area.width),
            color,
        );
        frame.render_widget(pulse, area);
        true
    }

    /// Erase the strip's row for a frame the strip does not paint.
    ///
    /// The frame buffer is cumulative: a cell no widget paints keeps
    /// whatever the previous frame left there, so a strip that simply
    /// stopped drawing would leave its last bar — or, after a theme
    /// switch, the previous palette's colors — standing in the row the
    /// composer sits beside. On a direct terminal the clear is the
    /// terminal default the idle row returns to, and a ruled theme's
    /// hairline paints over it after; inside a multiplexer the
    /// grounding walk that closes the layout refills it with the
    /// palette's canvas.
    fn strip_stood_down(frame: &mut Frame, area: Rect) {
        frame.render_widget(ratatui::widgets::Clear, area);
    }

    /// The instant the strip animates against: a pin's clock when one
    /// stands, the real clock otherwise.
    ///
    /// The glow's phase is a pure function of the time since the
    /// run's anchor, so a pin that fixes both ends asserts an exact
    /// brightness instead of racing the milliseconds between setting
    /// the anchor and drawing the frame.
    fn pulse_now(&self) -> Instant {
        #[cfg(test)]
        if let Some(pinned) = self.pacing.pulse_clock.get() {
            return pinned;
        }
        #[cfg(not(test))]
        let _ = self;
        Instant::now()
    }

    /// Render the pinned plan section: the run's current tasks,
    /// standing over the furniture while any do.
    ///
    /// Lines of text, not a box — the same register the conversation's
    /// todo blocks and the notice row use — inset from the frame's
    /// edges, headed by the done fraction, completed rows uniform and
    /// retired, moved rows still standing in the accent, and an
    /// ellipsis naming whatever the cap cut. Read-only by design: the
    /// tasks are the agent's bookkeeping, the model owns the list, and
    /// the section asks for no keys at all — it is furniture, not a
    /// sheet, and its one interaction is the posture cycle a click on
    /// it walks.
    pub(in crate::app) fn render_todo_section(&mut self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        self.plan.todo_section_rect = area;
        let colors = todo_view::TodoColors {
            plain: self.theme.ui.foreground,
            dim: self.theme.ui.dim,
            accent: self.theme.ui.primary,
            moved: self.theme.ui.secondary,
            badge_fill: self.theme.ui.primary,
            badge_letters: self.theme.ui.background,
        };
        if self.plan.plan_view == PlanView::Hidden {
            let mut folded = todo_view::folded_lines(&self.plan.latest_todos, &colors);
            for_display_lines(&mut folded);
            frame.render_widget(
                Paragraph::new(folded),
                Rect {
                    x: area
                        .x
                        .saturating_add(TODO_SECTION_INSET)
                        .min(area.right().saturating_sub(1)),
                    width: area
                        .width
                        .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
                        .max(1),
                    ..area
                },
            );
            return;
        }
        let body_cap = match self.plan.plan_view {
            PlanView::Full => self.full_body_cap.get(),
            _ => TODO_SECTION_BODY_ROWS,
        };
        let chord_hint = self.plan_chord_hint();
        let mut lines = todo_view::section_lines(
            &self.plan.latest_todos,
            &self.plan.todo_section_changes,
            &colors,
            area.width
                .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
                .max(1),
            body_cap,
            self.plan.clamped_todo_skip(),
            (chord_hint, self.plan.plan_cut_way()),
        );
        for_display_lines(&mut lines);
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                x: area
                    .x
                    .saturating_add(TODO_SECTION_INSET)
                    .min(area.right().saturating_sub(1)),
                width: area
                    .width
                    .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
                    .max(1),
                ..area
            },
        );
    }

    /// Render the prompts waiting to run.
    ///
    /// The section opens with a `QUEUED` title row naming what the rows
    /// under it are, then shows one prompt at a time, its text across the
    /// remaining rows rather than a digest: the reason to look at this
    /// section is to check wording before it reaches the model, so a
    /// summary of a prompt is precisely what the user came here to avoid.
    /// The marker and the `[n/total]` count ride the prompt's first row
    /// rather than spending columns of their own, and the count stands
    /// down for a lone prompt — being one of one is not a fact worth two
    /// columns. The marker itself carries the focus cue: dim while the
    /// composer holds the keys, the primary accent once the section
    /// does — a voice the dim register never renders, so the cue
    /// survives a theme (Dracula among them) whose secondary accent
    /// and dim are the same color.
    ///
    /// The keys ride the section's last row when the width can pay for
    /// them, and name only what the current mode answers: the walk's
    /// chords while the section has the keyboard, the saving ones while a
    /// prompt is out in the composer, and otherwise the chord that gets the
    /// user here. A hint for keys that do nothing from where the keys
    /// actually are would be a lie printed in the theme's dim. Where the
    /// text and the hint cannot both fit, the text keeps the row and the
    /// hint is dropped whole — never re-wrapped around, never allowed to
    /// push a word off the cut.
    ///
    /// Paints no background across its rows, exactly as the notice row
    /// does: these are lines of text, not a box, and a filled panel
    /// would put a block of the theme's surface into a terminal
    /// configured to paint none. The title is the one exception, and it
    /// is a badge rather than a panel — a pill a few cells wide is a
    /// label, not a filled box — stood in from the section's left edge,
    /// held a blank row clear of the prompt below it and another clear
    /// of the transcript above, and filled with the secondary — the
    /// voice the plan's badge traded it, so the two pills keep one
    /// family and two names — letters in the canvas
    /// color, at rest and under the keys alike. In a theme that defers
    /// to the terminal the fill resets and the badge degrades to plain
    /// text — the letters keep at most a foreground voice, which
    /// paints nothing. The render records its full seat — the inset
    /// pane it paints, margins included — so a later click anywhere
    /// in the section's rows answers, the same seat the plan section
    /// records.
    pub(in crate::app) fn render_queue(&mut self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let total = self.queued_display_len();
        if total == 0 {
            return;
        }
        self.queue.queue_section_rect = area;
        let pane = Rect {
            x: area
                .x
                .saturating_add(TODO_SECTION_INSET)
                .min(area.right().saturating_sub(1)),
            width: area
                .width
                .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
                .max(1),
            ..area
        };
        if self.queue.queue_posture == QueuePosture::Folded
            && self.queue.queue_mode == QueueMode::Plain
        {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Vec::new()),
                    Line::from(vec![
                        Span::styled(
                            format!(" {QUEUE_TITLE} "),
                            Style::default()
                                .fg(self.theme.ui.background)
                                .bg(self.theme.ui.secondary),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            format!("{total} waiting · ctrl+q to show"),
                            Style::default().fg(self.theme.ui.dim),
                        ),
                    ]),
                ]),
                pane,
            );
            return;
        }
        let shown = self.queue.queue_view.min(total.saturating_sub(1));
        let Some((_, text)) = self.queued_at(shown) else {
            return;
        };
        let rows = usize::from(area.height.min(QUEUE_ROWS));
        let top_standoff = usize::from(rows > 3);
        let bottom_standoff = usize::from(rows > 2);
        let body_rows = rows
            .saturating_sub(1)
            .saturating_sub(top_standoff)
            .saturating_sub(bottom_standoff);
        let focused = self.queue.queue_mode == QueueMode::Focused;
        let prefix = queue_prompt_prefix(total, shown);
        let width = usize::from(pane.width);
        let limit = width.saturating_sub(str_width(&prefix)).max(1);
        let body = wrap_prompt_rows(&for_display(&text), limit, body_rows);
        let marker_style = Style::default().fg(if focused {
            self.theme.ui.primary
        } else {
            self.theme.ui.dim
        });
        let (fill, letters) = (self.theme.ui.secondary, self.theme.ui.background);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
        if top_standoff > 0 {
            lines.push(Line::from(Vec::new()));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {QUEUE_TITLE} "),
                Style::default().fg(letters).bg(fill),
            ),
            Span::raw(" "),
            Span::styled(
                self.queue_hint().to_string(),
                Style::default().fg(self.theme.ui.dim),
            ),
        ]));
        if bottom_standoff > 0 {
            lines.push(Line::from(Vec::new()));
        }
        for (index, row) in body.into_iter().take(body_rows).enumerate() {
            let mut spans = Vec::with_capacity(2);
            if index == 0 {
                spans.push(Span::styled(prefix.clone(), marker_style));
            }
            spans.push(Span::styled(
                row,
                Style::default().fg(self.theme.ui.input_text),
            ));
            lines.push(Line::from(spans));
        }
        lines.resize(rows, Line::from(Vec::new()));
        frame.render_widget(Paragraph::new(lines), pane);
    }

    /// Render the notice row: one line of transient or state-driven
    /// message, right-aligned.
    ///
    /// The row the place-less frame reserves so nothing on screen
    /// shifts when a message arrives or leaves — with a place row
    /// standing, the notice rides that row's right edge instead and
    /// this renders nowhere. Blank otherwise; the text is the same
    /// one [`render_place_row`](Self::render_place_row) hosts.
    pub(in crate::app) fn render_notice(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        if let Some(text) = self.notice_text() {
            frame.render_widget(
                Paragraph::new(for_display(&text))
                    .style(Style::default().fg(self.theme.ui.input_border))
                    .alignment(ratatui::layout::Alignment::Right),
                area,
            );
        }
    }

    /// Render the input field: a fixed multi-row window onto the
    /// buffer, as a borderless composer drawn on the theme's surface
    /// color and padded on all sides. On painted chrome the pane's
    /// left edge is a solid one-cell accent column in the theme's
    /// primary color — the composer is the screen's always-focused
    /// surface, so its marker takes the focus accent — sitting inside
    /// the side padding, clear of the text; the paintless theme keeps
    /// its terminal-drawn hairlines and paints nothing, edge included.
    ///
    /// The field's shape is a pure background fill with square
    /// corners — solid by construction, like the scrollbar. Corners
    /// stay square deliberately: a cell grid cannot draw a smooth
    /// curve, and both approximations it can draw (corner glyphs,
    /// clipped corner cells) render as pixel-like steps. The text sits
    /// inside horizontal and vertical padding; the window shows the
    /// caret's line and the lines above it, so typing at the end
    /// sees the newest lines and browsing up scrolls with the
    /// caret. The box closes with its identity row — the model, its
    /// effort, and the permission mode in the dim register, the
    /// composer's state tag at the row's very right — sitting on the
    /// box's bottom edge, the padding row above it the breathing.
    /// It is not a composer row: nothing typed lands there and the
    /// caret never visits it. The field carries no other text of its
    /// own — queue state lives in its own section. `rows` and
    /// `caret` are the frame's single wrap of the buffer, computed by
    /// [`render`](Self::render) and shared with the identity row's
    /// tag. The text window takes its height from the pane's own rows
    /// net of the vertical padding, so no paint of the field's lands
    /// outside the pane it was given; the caret paints inside the
    /// pane the same way, so a terminal too short for the field's full
    /// height shows the caret on a row the field owns, and a field
    /// allotted no rows at all paints no caret. The field's rows
    /// render as typed, the display policy aside: the caret's column
    /// rides the same wrap the rows come from, and projecting the
    /// draft would move painted cells off that math. Every keystroke
    /// rewraps and repaints the field, so a cluster the terminal
    /// sizes differently cannot outlive the next press.
    pub(in crate::app) fn render_input(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        rows: &[String],
        caret: (u16, u16),
    ) {
        let muted = self.section_holds_keys();
        let text_style = self.paint_composer_surface(frame, area, muted);
        self.paint_composer_identity(frame, area, rows, caret);
        let (interior, start) = self.paint_composer_text(frame, area, rows, text_style, caret.0);
        self.paint_composer_caret(frame, area, interior, start, caret);
    }

    /// Paint the composer pane's own surface and say how its text sits
    /// on it.
    ///
    /// A themed composer border keeps the pane's surface to whatever
    /// stands behind it; without one the pane claims its surface here,
    /// and the leftmost column takes the accent the muted field reads
    /// dim and the live field reads primary. The style handed back is
    /// the text's own: the muted field dims its voice, the live field
    /// speaks in the input's colour.
    fn paint_composer_surface(&self, frame: &mut Frame, area: Rect, muted: bool) -> Style {
        let voice = if muted {
            self.theme.ui.dim
        } else {
            self.theme.ui.input_text
        };
        let surface = self.theme.ui.surface;
        if self.theme.ui.composer_border.is_some() {
            Style::default().fg(voice)
        } else {
            frame
                .buffer_mut()
                .set_style(area, Style::default().bg(surface));
            let edge = Rect {
                width: area.width.min(1),
                ..area
            };
            frame.buffer_mut().set_style(
                edge,
                Style::default().bg(if muted {
                    self.theme.ui.dim
                } else {
                    self.theme.ui.primary
                }),
            );
            Style::default().fg(voice).bg(surface)
        }
    }

    /// Paint the composer's identity row along the pane's bottom edge.
    ///
    /// The session's name and the field's state tag share the row the
    /// way the box closes itself: the identity takes what the tag
    /// leaves, the tag holds its own width right-aligned, and an empty
    /// tag leaves the identity the whole row. A pane with no rows
    /// paints nothing at all.
    fn paint_composer_identity(
        &self,
        frame: &mut Frame,
        area: Rect,
        rows: &[String],
        caret: (u16, u16),
    ) {
        let surface = self.theme.ui.surface;
        let info_style = if self.theme.ui.composer_border.is_some() {
            Style::default().fg(self.theme.ui.dim)
        } else {
            Style::default().fg(self.theme.ui.dim).bg(surface)
        };
        let info_x = area
            .x
            .saturating_add(INPUT_SIDE_INSET)
            .min(area.right().saturating_sub(1));
        let info_width = area
            .width
            .max(1)
            .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2));
        if area.height >= 1 {
            let identity_row = Rect {
                y: area.bottom().saturating_sub(1),
                height: 1,
                width: info_width,
                x: info_x,
            };
            let tag = Self::input_state_tag(rows, caret);
            let tag_width = if tag.is_empty() {
                0
            } else {
                u16::try_from(str_width(&tag).saturating_add(1))
                    .unwrap_or(info_width)
                    .min(info_width)
            };
            let identity = self.composer_identity_text();
            if !identity.is_empty() {
                frame.render_widget(
                    Paragraph::new(for_display(&identity)).style(info_style),
                    Rect {
                        width: info_width.saturating_sub(tag_width),
                        ..identity_row
                    },
                );
            }
            if tag_width > 0 {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        tag,
                        Style::default().fg(self.theme.ui.input_border),
                    ))
                    .style(info_style)
                    .alignment(ratatui::layout::Alignment::Right),
                    identity_row,
                );
            }
        }
    }

    /// Paint the field's visible rows and say where they sit.
    ///
    /// The window slides with the caret's row: the field's own height
    /// net of its padding decides how many rows stand, the window
    /// starts at the caret's row less all but the last of them, and
    /// the geometry is published to the pointer's hand beside it — the
    /// same `InputView` a click converts through, so what was painted
    /// and what is aimed at never disagree. Returns the window's own
    /// rectangle and its first row, which is where the caret's math
    /// continues.
    fn paint_composer_text(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        rows: &[String],
        text_style: Style,
        caret_row: u16,
    ) -> (Rect, usize) {
        let text_height = INPUT_TEXT_ROWS.min(
            area.height
                .saturating_sub(INPUT_VERTICAL_PADDING.saturating_mul(2))
                .saturating_sub(1),
        );
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
        let visible_rows = usize::from(text_height);
        let start = usize::from(caret_row).saturating_sub(visible_rows.saturating_sub(1));
        self.composer.input_view = Some(InputView {
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
        (interior, start)
    }

    /// Paint the caret cell itself, wherever the window left it.
    ///
    /// The caret rides the window: its column inside the interior, its
    /// row counted from the window's first, both clamped to the rows
    /// the pane owns — a terminal too short for the field still shows
    /// the caret on a row the field owns. A theme without real colours
    /// reverses the cell; one with them paints the input's own
    /// foreground onto its accent.
    fn paint_composer_caret(
        &self,
        frame: &mut Frame,
        area: Rect,
        interior: Rect,
        start: usize,
        caret: (u16, u16),
    ) {
        let (caret_row, column) = caret;
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
            let caret_style = if self.theme.ui.primary == Color::Reset
                || self.theme.ui.background == Color::Reset
            {
                Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
            } else {
                Style::default()
                    .fg(self.theme.ui.background)
                    .bg(self.theme.ui.primary)
            };
            if let Some(cell) = frame.buffer_mut().cell_mut((caret_x, caret_y)) {
                cell.set_style(caret_style);
            }
        }
    }

    /// Render the footer's three rows: a row of air under the
    /// composer, the bar's row, and the bottom row. The air is the
    /// composer's bottom margin — the identity row closes the box and
    /// the footer's facts start one clear row under it. The context
    /// readout is the footer's head — the bar a cell in from the
    /// left edge of its row, standing exactly over the numbers that
    /// lead the bottom row from that same column — and the session
    /// id is its tail, right-aligned a cell in from the right edge
    /// on the bar's row: the footer's middle, vertically centered
    /// between the air above and the numbers below. The composer's
    /// state tag rides the identity row inside the composer, not
    /// here — the footer is all session, no draft state. Both rows sit
    /// on the themed bar colors, and an id without the room to clear
    /// the bar is forgone rather than run into it. A chunk allotted no
    /// rows renders nothing — the bar never paints a row another pane
    /// owns.
    pub(in crate::app) fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let bar_style = Style::default()
            .fg(self.theme.ui.status_bar_fg)
            .bg(self.theme.ui.status_bar_bg);
        let (context_tokens, window) = self.context_snapshot();
        let context_fill = context_bar_fill(context_tokens, window, CONTEXT_BAR_CELLS);
        let readout = context_bar_readout(context_tokens, window);
        let air = area.height.saturating_sub(2).min(STATUS_BAR_AIR);
        frame.buffer_mut().set_style(
            Rect {
                y: area.y.saturating_add(air),
                height: area.height.saturating_sub(air),
                ..area
            },
            Style::default().bg(self.theme.ui.status_bar_bg),
        );
        let content = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        };
        let bar_y = content.y.saturating_sub(1);
        let id_span = if area.height > 1 {
            self.host.session_id.as_ref().and_then(|id| {
                let id_width = u16::try_from(str_width(id)).ok()?;
                let start = area
                    .width
                    .checked_sub(id_width)?
                    .checked_sub(u16::try_from(STATUS_BAR_MARGIN).unwrap_or(0))?;
                (usize::from(start) > STATUS_BAR_MARGIN + CONTEXT_BAR_CELLS + 1).then(|| {
                    let x = area.x.saturating_add(start);
                    (bar_y, x, x.saturating_add(id_width))
                })
            })
        } else {
            None
        };
        self.host.session_id_span.set(id_span);
        if area.height > 1 {
            let mut bar_line = self.context_bar_line(STATUS_BAR_MARGIN, context_fill);
            if let Some((_, start, _)) = id_span {
                let pad = usize::from(start)
                    .saturating_sub(
                        usize::from(area.x)
                            .saturating_add(STATUS_BAR_MARGIN)
                            .saturating_add(CONTEXT_BAR_CELLS),
                    )
                    .max(1);
                bar_line.spans.push(Span::raw(" ".repeat(pad)));
                bar_line.spans.push(Span::raw(
                    self.host
                        .session_id
                        .as_deref()
                        .unwrap_or_default()
                        .to_string(),
                ));
            }
            frame.render_widget(
                Paragraph::new(bar_line).style(bar_style),
                Rect {
                    y: bar_y,
                    height: 1,
                    ..area
                },
            );
        }
        let mut spans = vec![Span::raw(" ".repeat(STATUS_BAR_MARGIN))];
        if let Some((percent, totals)) = &readout {
            spans.push(Span::styled(
                totals.clone(),
                Style::default().fg(self.theme.ui.dim),
            ));
            spans.push(Span::styled(
                format!(" ({percent})"),
                Style::default().fg(self.theme.ui.scrollbar_thumb),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style), content);
    }

    /// Render the place row: the working directory and its branch on
    /// their own row directly above the composer, outside the box —
    /// a caption, dim on the plain background, inset to line up with
    /// the composer's text. The caption closes tight against the box:
    /// the box's top padding row keeps the blank row between the
    /// caption and the text, so no air row of the layout's own rides
    /// between the two. The caption costs the frame a single row until
    /// the host names a directory.
    pub(in crate::app) fn render_place_row(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let text = self.place_text();
        if text.is_empty() {
            return;
        }
        let notice = self.notice_text();
        let notice_len = notice
            .as_ref()
            .map_or(0u16, |text| {
                u16::try_from(display_width(text)).unwrap_or(u16::MAX)
            })
            .saturating_add(1);
        let row_width = area
            .width
            .max(1)
            .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2));
        frame.render_widget(
            Paragraph::new(for_display(&text)).style(Style::default().fg(self.theme.ui.dim)),
            Rect {
                x: area
                    .x
                    .saturating_add(INPUT_SIDE_INSET)
                    .min(area.right().saturating_sub(1)),
                y: area.y,
                height: 1,
                width: row_width.saturating_sub(notice_len),
            },
        );
        if let Some(notice) = notice {
            let x = area
                .x
                .saturating_add(INPUT_SIDE_INSET)
                .min(area.right().saturating_sub(1));
            frame.render_widget(
                Paragraph::new(for_display(&notice))
                    .style(Style::default().fg(self.theme.ui.input_border))
                    .alignment(ratatui::layout::Alignment::Right),
                Rect {
                    x: x.saturating_add(row_width.saturating_sub(notice_len)),
                    y: area.y,
                    height: 1,
                    width: notice_len.min(row_width),
                },
            );
        }
    }
}
