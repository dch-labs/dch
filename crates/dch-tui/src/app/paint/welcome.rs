//! The welcome screen's painter.
use ratatui::Frame;
use ratatui::layout::Rect;

use super::super::TuiApp;
use super::super::composer::{INPUT_SIDE_INSET, INPUT_VERTICAL_PADDING};
use super::super::{Constraint, Direction, Layout, Line, Paragraph, Span, Style, pane};
use crate::cells::{display_width, for_display, suffix_within};
use crate::splash;

impl TuiApp {
    /// Render the fresh session's welcome: splash, notice, and the
    /// open composer.
    ///
    /// One compact group — the splash band, the notice on its own
    /// row, and the composer as an open, borderless field the width
    /// of the art, murmuring the name in night colors until the
    /// first keystroke — rides between two equal fillers, so the
    /// group centers in a frame it owns whole while it stands. The
    /// docked rules do not follow it here — the identity row does,
    /// riding the open composer's bottom edge — and the first
    /// traffic of any kind returns the frame to the docked layout.
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
            if self.canvas_in_force() {
                crate::splash::dissolve_edges(&mut cropped, self.theme.ui.background);
            }
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
        if self.composer.input.is_empty() {
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
        let standing = self.host.working_dir.clone().or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.display().to_string())
        });
        if let Some(dir) = standing {
            let mut shown = dir;
            if let Some(branch) = self.host.git_branch.clone() {
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
/// [`fit_picker_title`](super::super::session_commands::fit_picker_title)
/// keeps for the session listing. The measure is painted columns (a
/// wide glyph counts twice) and the display strip runs first, so the
/// room fits exactly what the row paints.
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

    use crate::app::TuiApp;
    use crate::app::test_support::*;
    use crate::cells::display_width;

    use crate::message::TuiMessage;

    #[test]
    fn an_empty_conversation_paints_the_fjord_splash() {
        // The empty pane's own shape: a fresh session shows the
        // half-block fjord, centered, in the art's own colors.
        let mut subject = app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains('▀'),
            "the empty conversation paints the half-block art: {text}"
        );
    }

    #[test]
    fn the_splash_margins_dissolve_into_the_theme_canvas() {
        // The flat blue-black margins the art carries against the
        // theme's canvas read as a mismatched background flanking the
        // image — the outermost art column goes unstyled, a cell the
        // grounding walk then paints as the canvas, so the art's edge
        // and the strips beside it are literally the same cell state.
        // The picture's middle keeps its own colors. A stated pane
        // seat: the canvas is in force there, and the deferred direct
        // seat — which keeps its authored edges — is pinned by its own
        // test.
        let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::terminal::set_multiplexed_for_tests(Some(false));
        let mut subject = app();
        subject.set_terminal_seat(true);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        let buffer = terminal.backend().buffer();
        let band = subject.welcome_band.expect("band");
        let mid = band.y + band.height / 2;
        let canvas = subject.theme.ui.background;
        for x in [band.x, band.right().saturating_sub(1)] {
            let cell = buffer
                .cell((x, mid))
                .map(|cell| (cell.symbol(), cell.fg, cell.bg));
            assert_eq!(
                cell,
                Some((" ", ratatui::style::Color::Reset, canvas)),
                "the outermost art column is a blank cell the walk grounds — no glyph dots in the foreground color"
            );
        }
        let middle = buffer
            .cell((band.x + band.width / 2, mid))
            .map(|cell| cell.bg);
        assert_ne!(
            middle,
            Some(canvas),
            "the picture's middle keeps its own color"
        );
        let strip = buffer
            .cell((band.x.saturating_sub(1), mid))
            .map(|cell| (cell.fg, cell.bg));
        assert_eq!(
            strip,
            Some((ratatui::style::Color::Reset, canvas)),
            "the strips beyond the band are the same grounded cells as the art's edge"
        );
    }

    #[test]
    fn a_deferred_welcome_keeps_its_authored_edges() {
        // A deferred palette keeps its painted pairs, so the surface
        // probe the dissolve once gated on cannot see the deferral:
        // on a direct terminal that never answered, the frame's
        // ground is the terminal's own scheme and the art's margins
        // keep the exact ink and ground they were authored with — no
        // span steps toward a canvas that is not showing. The stated
        // pane seat renders the same deferred theme beside it — there
        // the canvas is in force and the outermost column dissolves —
        // the difference that proves the gate reads the seat, not
        // the palette.
        let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::terminal::set_multiplexed_for_tests(Some(false));
        let mut deferred = app();
        deferred.set_theme(deferred.theme().deferred_to_terminal());
        let mut deferred_terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).expect("terminal");
        deferred_terminal
            .draw(|frame| deferred.render(frame))
            .expect("deferred draw");
        let mut seated = app();
        seated.set_theme(seated.theme().deferred_to_terminal());
        seated.set_terminal_seat(true);
        let mut seated_terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).expect("terminal");
        seated_terminal
            .draw(|frame| seated.render(frame))
            .expect("seated draw");
        let band = deferred.welcome_band.expect("band");
        let mid = band.y + band.height / 2;
        let shape = deferred
            .welcome_shape(ratatui::layout::Rect::new(0, 0, 80, 30))
            .expect("a shape");
        let art = shape.art.expect("the seated rung");
        let row = shape.row_skip.saturating_add(mid.saturating_sub(band.y));
        let authored = art.lines.get(usize::from(row)).expect("the authored row");
        let width = authored.spans.len();
        let default_color = ratatui::style::Color::Reset;
        for (column, span) in [
            (0usize, authored.spans.first()),
            (width.saturating_sub(1), authored.spans.last()),
        ] {
            let Some(span) = span else {
                continue;
            };
            let cell = deferred_terminal.backend().buffer().cell((
                band.x
                    .saturating_add(u16::try_from(column).unwrap_or(u16::MAX)),
                mid,
            ));
            assert_eq!(
                cell.map(|cell| (cell.fg, cell.bg)),
                Some((
                    span.style.fg.unwrap_or(default_color),
                    span.style.bg.unwrap_or(default_color),
                )),
                "the deferred welcome's margin column {column} keeps its authored ink and ground"
            );
        }
        let dissolved = seated_terminal
            .backend()
            .buffer()
            .cell((band.x, mid))
            .map(|cell| (cell.symbol(), cell.bg));
        assert_eq!(
            dissolved,
            Some((" ", seated.theme.ui.background)),
            "the pane twin dissolves the same column — the gate reads the seat, not the palette"
        );
    }

    #[test]
    fn a_prompt_displaces_the_splash_and_a_clear_keeps_it_displaced() {
        // Displacement retires the splash for the run: the first
        // message bakes the cache and the splash stands down; /clear
        // empties the conversation but the session happened — the
        // fjord belongs to a process's first breath only and never
        // returns after it.
        let mut subject = app();
        subject.push_message(TuiMessage::User {
            text: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        let busy = buffer_text(terminal.backend().buffer());
        assert!(
            !busy.contains('▀'),
            "a conversation with traffic paints no splash: {busy}"
        );
        subject.clear_command();
        terminal
            .draw(|frame| subject.render(frame))
            .expect("redraw after clear");
        let cleared = buffer_text(terminal.backend().buffer());
        assert!(
            !cleared.contains('▀'),
            "the cleared conversation keeps the splash retired: {cleared}"
        );
    }

    #[test]
    fn a_narrow_pane_seats_the_small_rung_on_give_way_margins() {
        // Narrow panes spend their columns on the picture: the margins
        // give way to two a side, the w34 rung stands where the old
        // fractions left no room at all, and a pane too narrow even
        // for that still declines rather than clips.
        let mut subject = app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 30)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains('▀'),
            "the give-way margins seat the small rung: {text}"
        );
        let shape = subject
            .welcome_shape(ratatui::layout::Rect::new(0, 0, 40, 30))
            .expect("a shape");
        let art = shape.art.expect("the narrow lane's rung");
        assert_eq!(art.width, 34, "the narrow lane seats the w34 rung");
        let declined = subject
            .welcome_shape(ratatui::layout::Rect::new(0, 0, 30, 30))
            .expect("a shape");
        assert!(
            declined.art.is_none(),
            "too narrow for the smallest rung, the splash still declines"
        );
    }

    #[test]
    fn a_wide_tall_pane_shows_the_rung_whole_with_its_water() {
        // The whole picture, not its richest rows: a pane that seats a
        // tall rung's columns and rows shows every row of it — the
        // water at the fjord's foot included, never windowed away. The
        // three-quarter budget seats a rung below the widest one the
        // pane could hold.
        let subject = app();
        let area = ratatui::layout::Rect::new(0, 0, 120, 44);
        let shape = subject.welcome_shape(area).expect("a shape");
        let art = shape.art.expect("the pane seats the 82x21 rung");
        assert_eq!(art.width, 82, "the rung three-quarter budgets seat");
        assert_eq!(art.height, 21, "the rung's own height");
        assert_eq!(shape.rows_shown, art.height, "the rung shows whole");
        assert_eq!(shape.row_skip, 0, "nothing is windowed away");
    }

    #[test]
    fn a_wide_pane_seats_the_rung_its_width_deserves() {
        // The width governs: a pane wide enough for the w75 rung seats
        // it even when the pane is shorter than that, as long as the
        // group still has its floor of room — the seat never shrinks
        // the picture on a wide, short pane.
        let subject = app();
        let area = ratatui::layout::Rect::new(0, 0, 120, 32);
        let shape = subject.welcome_shape(area).expect("a shape");
        let art = shape.art.expect("the width seats the 75x20 rung");
        assert_eq!(art.width, 75, "width governs the seat");
        assert_eq!(art.height, 20, "the pane's floor still fits it");
    }

    #[test]
    fn a_ruled_theme_underlines_the_welcome_field_too() {
        // The open field is still a field: a theme that rules its
        // docked composer rules the welcome's the same way — hairline
        // underlines on the field's own top and bottom padding rows,
        // drawn by the terminal, thin and solid — while the painted
        // themes' welcome stays bare, and no rule crosses the murmur
        // itself.
        let mut subject = app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 25)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        {
            let buffer = terminal.backend().buffer();
            assert!(
                (0..25u16).all(|y| {
                    (0..80u16).all(|x| {
                        !buffer[(x, y)]
                            .style()
                            .add_modifier
                            .contains(ratatui::style::Modifier::UNDERLINED)
                    })
                }),
                "a painted theme's welcome draws no rules — the field is open"
            );
        }
        subject.theme.ui.composer_border = Some(ratatui::style::Color::Indexed(8));
        terminal
            .draw(|frame| subject.render(frame))
            .expect("ruled draw");
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..80u16)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        };
        let murmur = (0..25u16)
            .find(|&y| row_text(y).contains("dream"))
            .expect("the murmur row");
        // The field's width at this pane: the three-quarter ladder
        // seats the w48 rung, so each rule runs the field's 48 cells.
        // The top rule rides the spacer two rows above the murmur —
        // rule, a blank breath, the text — and the bottom rule the
        // padding row just below it: the docked field's exact shape.
        for row in [murmur.saturating_sub(2), murmur + 1] {
            let ruled = (0..80u16)
                .filter(|x| {
                    buffer[(*x, row)]
                        .style()
                        .add_modifier
                        .contains(ratatui::style::Modifier::UNDERLINED)
                })
                .count();
            assert_eq!(ruled, 48, "row {row}: the rule runs the field's width");
        }
        assert!(
            !buffer[(40, murmur)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "the murmur rides clear of both rules"
        );
    }

    #[test]
    fn a_pane_too_short_for_the_art_paints_nothing() {
        // The art is centered or absent, never clipped — vertically
        // too: a pane whose row floor cannot seat the smallest rung
        // shows no splash rather than a windowed fjord missing its
        // water.
        let subject = app();
        let area = ratatui::layout::Rect::new(0, 0, 120, 20);
        let shape = subject.welcome_shape(area).expect("a shape");
        assert!(
            shape.art.is_none(),
            "a short pane clips nothing — the splash declines"
        );
    }

    #[test]
    fn the_welcome_name_murmurs_in_the_placeholder() {
        // The empty composer carries the name in the picture's own
        // night colors: dream in starlight gold, create in moonlit
        // blue, harness in twilight lavender — one quiet row inside
        // the open field, stood down at the first keystroke.
        let mut subject = app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 60)).expect("terminal");
        terminal.draw(|frame| subject.render(frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        let gold = ratatui::style::Color::Rgb(232, 203, 122);
        let blue = ratatui::style::Color::Rgb(150, 180, 222);
        let lavender = ratatui::style::Color::Rgb(180, 154, 216);
        let word_row = |color: ratatui::style::Color| {
            (0..buffer.area.height)
                .find(|row| (0..buffer.area.width).any(|column| buffer[(column, *row)].fg == color))
        };
        let placeholder_row = word_row(gold).expect("the placeholder murmurs the name");
        assert_eq!(
            word_row(blue),
            Some(placeholder_row),
            "create shares the placeholder's row"
        );
        assert_eq!(
            word_row(lavender),
            Some(placeholder_row),
            "harness shares the placeholder's row"
        );
    }

    #[test]
    fn a_welcome_too_small_for_art_records_no_band() {
        // The band exists to carry art: a pane too small for the
        // smallest rung still shows the welcome — its composer box
        // alone, as the shape's own doc says — and must record no
        // band.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert!(
            app.welcome_band.is_none(),
            "an artless welcome carries no band — the composer box alone"
        );
    }

    #[test]
    fn the_welcome_to_docked_transition_requests_a_full_repaint() {
        // The docked layout paints fewer rows than the splash band did;
        // trusting the diff across that transition is the stale-splash
        // symptom. The retirement arms the repaint the loop clears
        // with.
        let mut subject = app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("welcome draw");
        assert!(!subject.welcome.welcome_retired);
        assert!(
            subject.pacing.full_repaint_due.is_none(),
            "the welcome itself repaints nothing"
        );
        subject.push_message(TuiMessage::User {
            text: "first".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        terminal
            .draw(|frame| subject.render(frame))
            .expect("docked draw");
        assert!(subject.welcome.welcome_retired);
        assert!(
            subject.pacing.full_repaint_due.is_some(),
            "the transition arms the full repaint the loop clears with"
        );
    }

    #[test]
    fn the_docked_frame_leaves_no_splash_rows_behind() {
        // The reported symptom, pinned at the buffer: the welcome
        // paints a deep band, the first docked frame paints a short
        // window — the rows the window cannot reach must hold nothing,
        // because the pane erases whole before its widgets write.
        let mut subject = app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("welcome draw");
        let welcome_buffer = terminal.backend().buffer().clone();
        let deep_splash_row = (0..12)
            .rev()
            .find(|y| row_has_text(&welcome_buffer, *y))
            .expect("the welcome paints a deep band");
        subject.push_message(TuiMessage::User {
            text: "first".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        terminal
            .draw(|frame| subject.render(frame))
            .expect("docked draw");
        assert!(
            !row_has_text(terminal.backend().buffer(), deep_splash_row),
            "the splash row the docked window cannot reach must be erased"
        );
    }
}
/// The welcome screen's lifecycle state, grouped off the app proper.
///
/// The fresh-session welcome — the fjord art over the composer — keeps
/// the one bit of bookkeeping its lifetime needs: whether the
/// conversation has ever held rows, retiring the screen for good.
#[derive(Default)]
pub(crate) struct WelcomeState {
    /// Whether the conversation has ever held rows, retiring the
    /// welcome.
    ///
    /// The splash belongs to the process's first breath: the latch
    /// arms when the conversation first holds rows and never opens
    /// again. `/clear` empties the pane but does not forget a session
    /// happened — a cleared session meets the plain docked pane
    /// instead of the fjord's return.
    pub(crate) welcome_retired: bool,
}

/// The fresh session's welcome geometry.
///
/// What the frame's welcome path needs to seat the group: the splash
/// scale the block holds, the block's width, and the rows the band
/// above the art spends. Built once per frame by
/// [`TuiApp::welcome_shape`](crate::app::TuiApp::welcome_shape)
/// before the composer wraps, for the wrap width depends on the
/// block, not the frame.
pub(crate) struct WelcomeShape {
    /// The largest splash scale the welcome block holds, if any.
    ///
    /// `None` when even the smallest scale exceeds the block — the
    /// welcome still stands, its composer box alone.
    pub(crate) art: Option<&'static splash::SplashArt>,

    /// The welcome composer box's width, in terminal cells.
    ///
    /// The frame's width less its side margins, capped at the welcome
    /// maximum so the block stays compact on wide terminals.
    pub(crate) block_width: u16,

    /// The rows the splash band spends: a seated rung's whole height,
    /// or zero.
    ///
    /// The picker seats only a rung whose whole height fits the pane's
    /// row budget, so the band is exactly that rung's height or empty
    /// — a rung that does not fit gives way to a smaller scale rather
    /// than being trimmed.
    pub(crate) band_height: u16,

    /// The rung rows the welcome shows: the seated rung entire, or
    /// zero.
    ///
    /// The picker seats a rung whole or not at all, so no pane sees a
    /// shorter slice of the fjord: the shown rows are the rung's full
    /// height, and the art's height never follows the pane.
    pub(crate) rows_shown: u16,

    /// The rung rows cropped away above: always zero.
    ///
    /// The picker seats a rung whole or not at all, so no welcome
    /// shape ever windows the art — the field stays in the shape so
    /// the renderer and the pins read one geometry, and the zero is
    /// the picker's own contract spelled out.
    pub(crate) row_skip: u16,
}

impl WelcomeShape {
    /// The wrap width the boxed composer wraps its text to.
    ///
    /// The block's inner width — the border columns and the input's
    /// own side insets spent — so the wrapped rows and the caret land
    /// where the box will show them.
    pub(crate) fn inner_width(&self) -> u16 {
        self.block_width
            .saturating_sub(2)
            .saturating_sub(INPUT_SIDE_INSET.saturating_mul(2))
            .max(1)
    }
}
