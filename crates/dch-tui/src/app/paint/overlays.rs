//! The floating overlays: question and permission sheets, the
//! session and model pickers, and command completion.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Wrap;

use super::super::TuiApp;
use super::super::composer::COMPOSE_PREFIX;
use super::super::context::{STATUS_BAR_AIR, STATUS_BAR_ROWS};
use super::super::notices::{wrap_prompt_rows, wrap_prompt_rows_last, wrap_rows};
use super::super::permissions::{
    MAX_OVERLAY_ROWS, PermissionSheet, bottom_sheet_rect, expanded_command_budget,
    expanded_fixed_rows, popup_bound, popup_rect, popup_window,
};
use super::super::session_commands::fit_picker_title;
use super::super::{
    Block, BorderType, Borders, Clear, Color, Line, Paragraph, Span, Style, str_width,
};
use crate::cells::{display_width, for_display};
use crate::commands;

impl TuiApp {
    /// Render the pending question as a sheet over the prompt box.
    ///
    /// The same anchored sheet the permission ask uses, carrying the
    /// question's optional header chip, its text, its option rows (the
    /// cursor row marked `›`, a marked row also `•`), each option's
    /// dimmed description, the key hints — and, after the model's
    /// options, the compose row: the escape the option list never
    /// offered, where Enter or Space opens a draft the user types freely
    /// and the next Enter sends as the answer. While that draft is open
    /// the terminal cursor rides its caret, so the cell the next
    /// character lands in is visible; the composer's own caret is hidden
    /// under a question anyway, so nothing competes for it. Long question
    /// text wraps at the box's inner width; option labels and
    /// descriptions clamp to one row each, ellipsized — the pickable
    /// surface stays scannable whatever the model asked. The frame
    /// reserves the sheet's height above the conversation (see
    /// [`TuiApp::render`]), so nothing the run said hides behind the
    /// box. A terminal shorter than the sheet does not clip it: the same
    /// walk chooses the rows that fit, keeping the draft row and the hints
    /// and windowing the options around the cursor.
    pub(in crate::app) fn render_question_overlay(
        &self,
        frame: &mut Frame,
        area: Rect,
        more: usize,
    ) {
        let Some(entry) = self.questions.shown_question() else {
            return;
        };
        let wrap_at = usize::from(area.width.saturating_sub(4));
        let sheet_area = self.question_sheet_area(area);
        let budget = usize::from(sheet_area.height.saturating_sub(2));
        let sheet = self.question_sheet(entry, wrap_at, more, budget);
        let lines = sheet.lines;
        let height = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(sheet_area.height);
        let overlay = bottom_sheet_rect(height, sheet_area);
        if overlay.width == 0 || overlay.height == 0 {
            return;
        }
        let accent = Style::default().fg(self.theme.ui.primary);
        let title = if more == 0 {
            " Question ".to_string()
        } else {
            format!(
                " Question {} of {} ",
                self.questions.question_view.saturating_add(1),
                more.saturating_add(1)
            )
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(accent)
            .title(Span::styled(title, accent));
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().bg(self.theme.ui.surface)),
            overlay,
        );
        if let Some((row, column)) = sheet.caret
            && row.saturating_add(1) < usize::from(overlay.height)
        {
            let caret_x = usize::from(overlay.x)
                .saturating_add(1)
                .saturating_add(str_width(COMPOSE_PREFIX))
                .saturating_add(column)
                .min(usize::from(overlay.right()).saturating_sub(1));
            let caret_y = usize::from(overlay.y)
                .saturating_add(1)
                .saturating_add(row)
                .min(usize::from(overlay.bottom()).saturating_sub(1));
            frame.set_cursor_position((
                u16::try_from(caret_x).unwrap_or(0),
                u16::try_from(caret_y).unwrap_or(0),
            ));
        }
    }

    /// Render the pending permission ask as a sheet over the prompt
    /// box.
    ///
    /// Draws where the user's attention already is: anchored at the
    /// bottom, directly above the status bar, covering the bottom
    /// furniture — the place row (or, with no place, the notice row),
    /// the spacer, and the composer — the box the user would type in
    /// is replaced by the question. The conversation above keeps
    /// rendering behind it, so what the run is doing stays visible
    /// while the ask waits. The prompt and the hints are each
    /// pre-wrapped at the box's inner width (a block at most
    /// `MAX_OVERLAY_ROWS` rows, the last ellipsized), so no rendered
    /// line can re-flow inside the border and the hints always fit,
    /// whatever the tool name's length or the terminal's width; only
    /// a terminal shorter than the box clips, at the box's top. The
    /// sheet is gathered already — this borrows nothing from the
    /// queue.
    ///
    /// `always` carries what an "allow always" answer would record, in
    /// the rules' own phrases; the line stands between the command and
    /// the hints so the scope of the `a` press is read before the key
    /// is pressed, not after. `command` is the call's raw command from
    /// the input — exactly what will run — drawn as its own block, and
    /// `reason` is the live text when the sheet's `r` press opened the
    /// reason line, whose hints swap for the reason line's own.
    pub(in crate::app) fn render_permission_overlay(
        &self,
        frame: &mut Frame,
        area: Rect,
        sheet: &PermissionSheet,
    ) {
        let wrap_at = usize::from(area.width.saturating_sub(2));
        let command_truncated = sheet.command.as_ref().is_some_and(|command| {
            wrap_rows(&format!("runs: {command}"), wrap_at).len() > MAX_OVERLAY_ROWS
        });
        let mut hints = if sheet.reason.is_some() {
            "[enter] deny with this reason · [esc] back to the ask".to_string()
        } else if sheet.more == 0 {
            "[y] allow · [a] allow always · [d] deny always · [n] deny · [r] deny with a reason · esc or ctrl+c cancels".to_string()
        } else {
            format!(
                "[y] allow · [a] allow always · [d] deny always · [n] deny · [r] deny with a reason · esc or ctrl+c cancels · {more} more waiting",
                more = sheet.more,
            )
        };
        if command_truncated {
            hints.push_str(if self.permission.permission_command_expanded {
                " · [e] collapse"
            } else {
                " · [e] expand"
            });
        }
        let mut lines: Vec<Line<'_>> = wrap_prompt_rows(&sheet.prompt, wrap_at, MAX_OVERLAY_ROWS)
            .into_iter()
            .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.foreground)))
            .collect();
        let sheet_area = Rect {
            height: area.height.saturating_sub(STATUS_BAR_ROWS - STATUS_BAR_AIR),
            ..area
        };
        let scope_line = (!sheet.always.is_empty())
            .then(|| format!("a remembers: {}", sheet.always.join(" · ")));
        let deny_line =
            (!sheet.never.is_empty()).then(|| format!("d remembers: {}", sheet.never.join(" · ")));
        let reason_line = sheet
            .reason
            .as_ref()
            .map(|reason| format!("reason: {reason}▏"));
        if let Some(command) = &sheet.command {
            let runs = format!("runs: {command}");
            let rows = if self.permission.permission_command_expanded {
                let blocks = [
                    scope_line.as_deref(),
                    deny_line.as_deref(),
                    reason_line.as_deref(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<&str>>();
                let budget = expanded_command_budget(
                    sheet_area.height,
                    lines.len(),
                    expanded_fixed_rows(&blocks, &hints, wrap_at),
                );
                wrap_prompt_rows_last(&runs, wrap_at, budget)
            } else {
                wrap_prompt_rows(&runs, wrap_at, MAX_OVERLAY_ROWS)
            };
            lines.extend(
                rows.into_iter()
                    .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.primary))),
            );
        }
        if let Some(scope) = scope_line {
            lines.extend(
                wrap_prompt_rows(&scope, wrap_at, MAX_OVERLAY_ROWS)
                    .into_iter()
                    .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.secondary))),
            );
        }
        if let Some(scope) = deny_line {
            lines.extend(
                wrap_prompt_rows(&scope, wrap_at, MAX_OVERLAY_ROWS)
                    .into_iter()
                    .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.secondary))),
            );
        }
        if let Some(typed) = reason_line {
            lines.extend(
                wrap_prompt_rows(&typed, wrap_at, MAX_OVERLAY_ROWS)
                    .into_iter()
                    .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.foreground))),
            );
        }
        lines.push(Line::default());
        lines.extend(
            wrap_prompt_rows(&hints, wrap_at, MAX_OVERLAY_ROWS)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.secondary))),
        );
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

    /// Render the saved-session picker on the composer's chosen side.
    ///
    /// The same floating shape the completion popup takes: rows
    /// beside the composer, the cursor row highlighted — inverting
    /// under a palette that defers its colors, exactly the popup's
    /// rule — a dim header naming the keys, and a window that follows
    /// the cursor so a long catalog always keeps the chosen row on
    /// screen, its capacity sized to the rows the side actually
    /// offers so a short terminal sheds list rows, never the cursor
    /// itself. Each row spells the short id, the session's name, the
    /// model, when the session last saved, and its prompt count.
    pub(in crate::app) fn render_session_picker(
        &mut self,
        frame: &mut Frame,
        input_area: Rect,
        above: bool,
    ) {
        const SHOWN: usize = 8;
        let Some(picker) = self.pickers.session_picker.as_ref() else {
            return;
        };
        if picker.rows.is_empty() {
            return;
        }
        let surface = self.theme.ui.surface;
        let bound = popup_bound(frame, input_area, above);
        if bound < 2 {
            return;
        }
        let shown = SHOWN.min(usize::from(bound).saturating_sub(1));
        let (start, cursor_row) = popup_window(picker.cursor, picker.rows.len(), shown);
        let lines: Vec<Line> = std::iter::once(Line::styled(
            " sessions — ↑↓ walk · ⏎ resume · esc close ",
            Style::default().fg(self.theme.ui.dim),
        ))
        .chain(
            picker
                .rows
                .iter()
                .skip(start)
                .take(shown)
                .enumerate()
                .map(|(index, pick)| {
                    let highlighted = index == cursor_row;
                    let style = if highlighted {
                        if self.theme.ui.primary == Color::Reset || surface == Color::Reset {
                            Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
                        } else {
                            Style::default().fg(surface).bg(self.theme.ui.primary)
                        }
                    } else {
                        Style::default().fg(self.theme.ui.input_text).bg(surface)
                    };
                    let short = &pick.id[..pick.id.len().min(8)];
                    let dir = pick
                        .workdir
                        .as_deref()
                        .and_then(|dir| {
                            std::path::Path::new(dir)
                                .file_name()
                                .and_then(|name| name.to_str())
                        })
                        .map_or_else(|| "—".to_string(), |name| for_display(name).into_owned());
                    let model = for_display(&pick.model);
                    let dir_col = format!(
                        "{dir}{}",
                        " ".repeat(20usize.saturating_sub(display_width(&dir)))
                    );
                    let model_col = format!(
                        "{model}{}",
                        " ".repeat(12usize.saturating_sub(display_width(&model)))
                    );
                    Line::from(Span::styled(
                        format!(
                            " {short}  {}  {dir_col} {model_col} {}  {}p ",
                            fit_picker_title(pick.title.as_deref()),
                            pick.saved_at.format("%b %d %H:%M"),
                            pick.prompts
                        ),
                        style,
                    ))
                }),
        )
        .collect();
        let height = u16::try_from(lines.len()).unwrap_or(1).min(bound);
        if height == 0 {
            return;
        }
        let popup = popup_rect(input_area, height, above);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(surface)),
            popup,
        );
    }

    /// Render the configured-models picker on the composer's chosen side.
    ///
    /// The same floating shape the session picker takes: a dim header
    /// naming the keys, each row the model name with the running one
    /// marked `•`, the cursor row highlighted (inverting under a
    /// palette that defers its colors). The window follows the cursor
    /// at its edges, same scroll rule as the session picker.
    pub(in crate::app) fn render_model_picker(
        &mut self,
        frame: &mut Frame,
        input_area: Rect,
        above: bool,
    ) {
        const SHOWN: usize = 8;
        let Some(picker) = self.pickers.model_picker.as_ref() else {
            return;
        };
        if picker.rows.is_empty() {
            return;
        }
        let surface = self.theme.ui.surface;
        let bound = popup_bound(frame, input_area, above);
        if bound < 2 {
            return;
        }
        let shown = SHOWN.min(usize::from(bound).saturating_sub(1));
        let total = picker.rows.len();
        let max_start = total.saturating_sub(shown);
        let start = picker
            .cursor
            .saturating_sub(shown.saturating_sub(1))
            .min(max_start);
        let running = &self.config.api.model;
        let lines: Vec<Line> = std::iter::once(Line::styled(
            " models — ↑↓ walk · ⏎ switch · esc close ",
            Style::default().fg(self.theme.ui.dim),
        ))
        .chain(
            picker
                .rows
                .iter()
                .skip(start)
                .take(shown)
                .enumerate()
                .map(|(index, name)| {
                    let highlighted = index == picker.cursor.saturating_sub(start);
                    let style = if highlighted {
                        if self.theme.ui.primary == Color::Reset || surface == Color::Reset {
                            Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
                        } else {
                            Style::default().fg(surface).bg(self.theme.ui.primary)
                        }
                    } else {
                        Style::default().fg(self.theme.ui.input_text).bg(surface)
                    };
                    let shown = for_display(name);
                    let marker = if name == running { "•" } else { " " };
                    Line::from(Span::styled(format!(" {marker} {shown} "), style))
                }),
        )
        .collect();
        let height = u16::try_from(lines.len()).unwrap_or(1).min(bound);
        if height == 0 {
            return;
        }
        let popup = popup_rect(input_area, height, above);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(surface)),
            popup,
        );
    }

    /// Draw a live completion popup above the composer.
    ///
    /// While the composer holds a leading-slash line, this lists what the same
    /// [`commands::complete`] that Tab accepts would offer right now — command
    /// names for `/t…`, and the command's values (models, theme names, config
    /// keys) once the command word is complete and an argument has begun, as in
    /// `/theme def`. A command row carries the same one-line summary `/help`
    /// prints for it — dimmed, and aligned in one column past the widest name
    /// the window shows, `/help`'s own shape column sized to the panel, so the
    /// descriptions read as a column rather than tailing each name; a value
    /// row stands alone — the value is the whole offer. It is a read-only
    /// preview recomputed each frame from the
    /// current text; Tab is what actually accepts or cycles a choice, so this
    /// never changes what is typed. It hides when there is nothing to offer, or
    /// when the sole candidate is already fully spelled out in the composer.
    pub(in crate::app) fn render_completion(
        &mut self,
        frame: &mut Frame,
        input_area: Rect,
        above: bool,
    ) {
        const SHOWN: usize = 6;
        let bound = popup_bound(frame, input_area, above);
        let shown = SHOWN.min(usize::from(bound));
        let Some((prefix, candidates)) = self.popup_candidates() else {
            self.completion.completion = None;
            return;
        };

        let total = candidates.len();
        let active_cycle = self
            .completion
            .completion
            .as_ref()
            .filter(|cycle| cycle.prefix == prefix);
        let start = active_cycle.map_or(0, |cycle| {
            let max_start = total.saturating_sub(shown);
            let start = cycle.window_start.min(max_start);
            let selected = cycle.index.min(total.saturating_sub(1));
            if selected < start {
                selected
            } else if selected >= start.saturating_add(shown) {
                selected
                    .saturating_sub(shown.saturating_sub(1))
                    .min(max_start)
            } else {
                start
            }
        });
        let highlighted_row = active_cycle.map(|cycle| {
            cycle
                .index
                .min(total.saturating_sub(1))
                .saturating_sub(start)
        });
        let window: Vec<&String> = candidates.iter().skip(start).take(shown).collect();

        let surface = self.theme.ui.surface;
        let completing_command = prefix == "/";
        let width = usize::from(input_area.width.max(1));
        let summary_column = if completing_command {
            window
                .iter()
                .map(|candidate| str_width(&format!("   /{candidate}")))
                .max()
                .unwrap_or(0)
                .saturating_add(2)
        } else {
            0
        };
        let lines: Vec<Line> = window
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let highlighted = highlighted_row == Some(index);
                let style = if highlighted {
                    if self.theme.ui.primary == Color::Reset
                        || self.theme.ui.surface == Color::Reset
                    {
                        Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
                    } else {
                        Style::default()
                            .fg(self.theme.ui.surface)
                            .bg(self.theme.ui.primary)
                    }
                } else {
                    Style::default().fg(self.theme.ui.input_text).bg(surface)
                };
                let mut spans = if completing_command {
                    let name = format!("/{candidate}");
                    vec![Span::styled(format!("{name:<summary_column$}"), style)]
                } else {
                    vec![Span::styled(format!("{} ", for_display(candidate)), style)]
                };
                if completing_command && let Some(summary) = commands::summary_for(candidate) {
                    let budget = width.saturating_sub(summary_column);
                    let summary_style = if highlighted {
                        style
                    } else {
                        Style::default().fg(self.theme.ui.dim).bg(surface)
                    };
                    let clamped = wrap_prompt_rows(summary, budget, 1)
                        .first()
                        .cloned()
                        .unwrap_or_default();
                    spans.push(Span::styled(clamped, summary_style));
                }
                Line::from(spans)
            })
            .collect();

        let height = u16::try_from(lines.len()).unwrap_or(1).min(bound);
        if height == 0 {
            return;
        }
        let popup = popup_rect(input_area, height, above);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(surface)),
            popup,
        );
    }
}
