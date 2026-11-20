//! The welcome screen's painter.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::super::TuiApp;
use super::super::{
    Constraint, Direction, INPUT_SIDE_INSET, INPUT_VERTICAL_PADDING, Layout, Line, Paragraph, Span,
    Style, WelcomeShape, pane,
};
use crate::cells::{display_width, for_display, suffix_within};

impl TuiApp {
    /// Render the fresh session's welcome: splash, notice, open
    /// composer, and the pinned status bar.
    ///
    /// One compact group — the splash band, the notice on its own
    /// row, and the composer as an open, borderless field the width
    /// of the art, murmuring the name in night colors until the
    /// first keystroke — rides between two equal fillers, so the
    /// group centers in the frame while the status bar keeps the
    /// bottom edge. The docked rules do not follow it here — the
    /// identity row does, riding the open composer's bottom edge —
    /// and the first traffic of any kind returns the frame to the
    /// docked layout.
    pub(in crate::app) fn render_welcome(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        shape: &WelcomeShape,
        rows: &[String],
        caret: (u16, u16),
        composer_height: u16,
    ) {
        let box_height = composer_height;
        let rule_row = u16::from(self.theme.ui.composer_border.is_some());
        let chunks = welcome_rows(area, shape.band_height, rule_row, box_height);
        let origin = area
            .x
            .saturating_add(area.width.saturating_sub(shape.block_width) / 2);
        if let Some(scale) = shape.art {
            let band = pane(&chunks, 1, area);
            let mut cropped: Vec<Line<'static>> = scale
                .lines
                .iter()
                .skip(usize::from(shape.row_skip))
                .take(usize::from(shape.rows_shown))
                .cloned()
                .collect();
            crate::splash::dissolve_edges(&mut cropped, self.theme.ui.background);
            let rect = Rect {
                x: origin,
                y: band.y,
                width: scale.width,
                height: shape.rows_shown,
            };
            frame.render_widget(Paragraph::new(cropped), rect);
        }
        let box_row = pane(&chunks, 4, area);
        let inner = Rect {
            x: origin,
            y: box_row.y,
            width: shape.block_width,
            height: box_height,
        };
        if let Some(color) = self.theme.ui.composer_border {
            let rule = Style::default()
                .fg(color)
                .add_modifier(ratatui::style::Modifier::UNDERLINED);
            let spacer = pane(&chunks, 3, area);
            if spacer.height > 0 {
                let top = Rect {
                    x: inner.x,
                    y: spacer.y,
                    width: inner.width,
                    height: 1,
                };
                frame.buffer_mut().set_style(top, rule);
            }
            let bottom = Rect {
                y: inner.bottom().saturating_sub(2),
                height: 1,
                ..inner
            };
            frame.buffer_mut().set_style(bottom, rule);
        }
        let band_row = pane(&chunks, 1, area);
        self.welcome_band = shape.art.map(|scale| ratatui::layout::Rect {
            x: origin,
            y: band_row.y,
            width: scale.width,
            height: shape.band_height,
        });
        if self.input.is_empty() {
            let spans: Vec<Span<'static>> = Self::WELCOME_PLACEHOLDER
                .iter()
                .zip(Self::WELCOME_WORD_COLORS.iter())
                .enumerate()
                .flat_map(|(index, (word, color))| {
                    let space = (index > 0).then(|| Span::raw(" ")).into_iter();
                    space.chain(std::iter::once(Span::styled(
                        (*word).to_string(),
                        Style::default().fg(*color),
                    )))
                })
                .collect();
            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    x: inner.x.saturating_add(INPUT_SIDE_INSET),
                    y: inner.y.saturating_add(INPUT_VERTICAL_PADDING),
                    width: inner.width.saturating_sub(INPUT_SIDE_INSET),
                    height: 1,
                },
            );
        }
        self.render_input(frame, inner, rows, caret);
        let notice_row = pane(&chunks, 2, area);
        let notice = self.notice_text();
        let notice_len = notice
            .as_ref()
            .map_or(0u16, |text| {
                u16::try_from(display_width(text)).unwrap_or(u16::MAX)
            })
            .saturating_add(1);
        let standing = self.working_dir.clone().or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.display().to_string())
        });
        if let Some(dir) = standing {
            let mut shown = dir;
            if let Some(branch) = self.git_branch.clone() {
                shown.push_str(" (");
                shown.push_str(&branch);
                shown.push(')');
            }
            let room = shape.block_width.saturating_sub(notice_len);
            shown = dir_caption(&shown, room);
            frame.render_widget(
                Paragraph::new(Span::styled(shown, Style::default().fg(self.theme.ui.dim))),
                Rect {
                    x: origin,
                    y: notice_row.y,
                    width: room.max(1),
                    height: 1,
                },
            );
        }
        if let Some(notice) = notice {
            frame.render_widget(
                Paragraph::new(for_display(&notice))
                    .style(Style::default().fg(self.theme.ui.input_border))
                    .alignment(ratatui::layout::Alignment::Right),
                Rect {
                    x: origin.saturating_add(shape.block_width.saturating_sub(notice_len)),
                    y: notice_row.y,
                    width: notice_len.min(shape.block_width),
                    height: 1,
                },
            );
        }
        self.render_completion(frame, inner, false);
        self.render_session_picker(frame, inner, false);
        self.render_model_picker(frame, inner, false);
    }
}

/// The welcome group's six rows: a filler above and below, the band,
/// the notice, the rule, the composer box.
///
/// One split in one place — the arithmetic cannot drift between two
/// copies of it.
fn welcome_rows(area: Rect, band_height: u16, rule_row: u16, box_height: u16) -> Vec<Rect> {
    let constraints = [
        Constraint::Min(0),
        Constraint::Length(band_height),
        Constraint::Length(1),
        Constraint::Length(rule_row),
        Constraint::Length(box_height),
        Constraint::Min(0),
    ];
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

/// The working-directory caption as it fits its `room` columns.
///
/// The tail is the part worth keeping — a repository's name sits at
/// the end of its path, so an over-room caption cuts its head and
/// marks the cut with an ellipsis, the same side
/// [`fit_column_tail`](super::super::fit_column_tail) keeps for the
/// session listing. The measure is painted columns (a wide glyph
/// counts twice) and the display strip runs first, so the room fits
/// exactly what the row paints.
fn dir_caption(shown: &str, room: u16) -> String {
    let room = usize::from(room.max(1));
    let shown = for_display(shown);
    if display_width(&shown) <= room {
        return shown.into_owned();
    }
    let keep = room.saturating_sub(1);
    format!("…{}", suffix_within(&shown, keep))
}

#[cfg(test)]
mod tests {
    use super::dir_caption;
    use crate::cells::display_width;

    #[test]
    fn a_wide_caption_fits_its_room_by_painted_columns() {
        // Thirty CJK characters are sixty painted cells; a
        // twenty-column room keeps a ten-character tail plus the
        // ellipsis, never the nineteen characters the character
        // count once kept.
        let caption = dir_caption(&"长".repeat(30), 20);
        assert!(caption.starts_with('…'), "the cut is marked: {caption:?}");
        // The widest tail a twenty-column room holds: nine two-cell
        // glyphs plus the ellipsis — a wide glyph cannot split, so
        // the caption lands one cell shy rather than past its room.
        assert_eq!(display_width(&caption), 19);
        assert!(caption.ends_with('长'), "the tail keeps the path's end");
    }

    #[test]
    fn a_short_caption_passes_through_and_a_family_strips() {
        assert_eq!(
            dir_caption("/home/dch/repos/dch", 40),
            "/home/dch/repos/dch"
        );
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let caption = dir_caption(&format!("/x/{family}"), 12);
        assert!(
            !caption.contains('\u{200d}'),
            "the caption paints the stripped projection: {caption:?}"
        );
    }
}
