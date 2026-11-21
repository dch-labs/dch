#![cfg(test)]

//! Fixtures shared by the app module's tests: the subject
//! builders, the frame-reading helpers, and the small shims the
//! pins reuse.

use super::TuiApp;
use crate::commands;
use crate::message::TuiMessage;

/// A summary-less compacted row for the attribution pins.
///
/// The stats are fixtures; the reason rides through so a pin can
/// name the pass's trigger where the wording matters.
pub(crate) fn compacted_row(reason: &str) -> TuiMessage {
    TuiMessage::Compacted {
        tokens_before: 10,
        tokens_after: 5,
        reason: reason.to_string(),
        evicted: 3,
        duration_ms: 100,
        compactor: Some("structured".to_string()),
        compression_ratio: Some(2.0),
        summary: None,
        timestamp: chrono::Utc::now(),
    }
}

/// The joined text of rendered lines, for line-by-line compares.
///
/// Each ratatui line's spans concatenate into one string, in order,
/// so a pin can assert on a rendered row's words — including the
/// trailing spaces a layout leaves — without touching cells.
pub(crate) fn texts_of(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        })
        .collect()
}

/// A fresh app on the default config: the standard test subject.
///
/// Every fixture that renders or measures a splash, a pane, or a
/// listing starts from this app, so the pins share one starting
/// shape and a config-field default change surfaces in all of them
/// at once.
pub(crate) fn app() -> TuiApp {
    TuiApp::new(dch_config::DchConfig::default())
}

/// The rendered text of a whole buffer, row by row.
///
/// Joins each row's cell symbols so a test can assert on words
/// the way a reader sees them, not on individual cells.
pub(crate) fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let mut rows = Vec::new();
    for y in buffer.area.y..buffer.area.bottom() {
        let mut row = String::new();
        for x in buffer.area.x..buffer.area.right() {
            if let Some(cell) = buffer.cell((x, y)) {
                row.push_str(cell.symbol());
            }
        }
        rows.push(row.trim_end().to_string());
    }
    rows.join("\n")
}

/// Whether any cell in the row holds a symbol beyond a space.
///
/// A row the leak would stain reads as text; a row the pane erase
/// cleaned reads as blank.
pub(crate) fn row_has_text(buffer: &ratatui::buffer::Buffer, y: u16) -> bool {
    (buffer.area.x..buffer.area.right()).any(|x| {
        buffer
            .cell((x, y))
            .is_some_and(|cell| cell.symbol() != " " && cell.symbol() != "")
    })
}

/// Whether a glyph belongs to the splash's art alphabet.
///
/// The splash's figure is drawn from the half-block and shade glyphs
/// ratatui renders one cell each; membership in that set separates
/// the art from the spaces and text a fresh render also paints.
pub(crate) fn is_art_glyph(glyph: &str) -> bool {
    "▀▄▌▐░▒▓█▘▝▖▗▚▞▙▛▜▟▏▎▍▋▊▉▁▂▃▅▆▇".contains(glyph)
}

/// The art-glyph cells one fresh-session render paints.
///
/// The count a default-config app produces at the given size — the
/// figure the splash-shape pins compare against, so a layout change
/// that moves or redraws the art shifts every pin built on it.
pub(crate) fn rendered_art(width: u16, height: u16) -> usize {
    rendered_art_with(app(), width, height)
}

/// The art-glyph cells one given fresh app paints at a size.
///
/// Renders the app on a test backend and counts the cells whose
/// glyph belongs to the art alphabet — the measurement every
/// splash-shape pin reduces to.
pub(crate) fn rendered_art_with(mut subject: TuiApp, width: u16, height: u16) -> usize {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .filter(|cell| is_art_glyph(cell.symbol()))
        .count()
}

/// The distinct columns one fresh render's art occupies.
///
/// Zero when the render paints no art; otherwise the count of
/// columns from the leftmost to the rightmost art glyph inclusive —
/// the width the splash-shape pins assert the art keeps.
pub(crate) fn rendered_art_columns(width: u16, height: u16) -> usize {
    art_bounds(width, height).map_or(0, |(left, right)| {
        usize::from(right.saturating_sub(left).saturating_add(1))
    })
}

/// The rows one fresh render's art occupies.
///
/// Zero when the render paints no art; otherwise the count of rows
/// from the topmost to the bottommost art glyph inclusive — the
/// height the splash-shape pins assert the art keeps.
pub(crate) fn rendered_art_rows(width: u16, height: u16) -> usize {
    art_rows(width, height).map_or(0, |(top, bottom)| {
        usize::from(bottom.saturating_sub(top).saturating_add(1))
    })
}

/// The leftmost and rightmost art-glyph columns in one render.
///
/// Scans every cell of the rendered buffer and reports the horizontal
/// extent the art covers; `None` when the render paints no art at
/// all, which is itself a pin-worthy shape.
pub(crate) fn art_bounds(width: u16, height: u16) -> Option<(u16, u16)> {
    let mut subject = app();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut left = None;
    let mut right = None;
    for row in 0..buffer.area.height {
        for column in 0..buffer.area.width {
            if is_art_glyph(buffer[(column, row)].symbol()) {
                left = Some(left.map_or(column, |known: u16| known.min(column)));
                right = Some(right.map_or(column, |known: u16| known.max(column)));
            }
        }
    }
    Some((left?, right?))
}

/// The topmost and bottommost art-glyph rows in one render.
///
/// Scans every cell of the rendered buffer and reports the vertical
/// extent the art covers; `None` when the render paints no art at
/// all, which is itself a pin-worthy shape.
pub(crate) fn art_rows(width: u16, height: u16) -> Option<(u16, u16)> {
    let mut subject = app();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut top = None;
    let mut bottom = None;
    for row in 0..buffer.area.height {
        for column in 0..buffer.area.width {
            if is_art_glyph(buffer[(column, row)].symbol()) {
                top.get_or_insert(row);
                bottom = Some(row);
            }
        }
    }
    Some((top?, bottom?))
}

/// Rows to spend on the sheet, marked by the block each came from, so
/// a test can say which rows survived.
///
/// Each line is `{mark}{index}` — `██0`, `██1`, … — so a rendered
/// sheet can be matched line by line: an index still present after an
/// operation names the row that survived it, and a missing one names
/// the row the operation dropped.
pub(crate) fn block_rows(mark: &str, count: usize) -> Vec<ratatui::text::Line<'static>> {
    (0..count)
        .map(|index| ratatui::text::Line::from(format!("{mark}{index}")))
        .collect()
}

/// Run a slash command and return everything it surfaced, as one
/// newline-joined string.
///
/// Both lanes a command may write to are read: the transcript rows it
/// appended (`/cost`, `/themes`, `/version`) and the transient status
/// line it raised (`/exit`, `/stop`, `/copy`). The line must parse to
/// a command — anything else fails the test that passed it.
pub(crate) fn command_output(app: &mut TuiApp, line: &str) -> String {
    let commands::Parsed::Command(command) = commands::parse(line) else {
        panic!("expected {line:?} to parse to a command");
    };
    let before = app.conversation.len();
    app.run_command(command);
    let mut out: Vec<String> = app
        .conversation
        .iter()
        .skip(before)
        .filter_map(|message| match message {
            TuiMessage::System { text, .. } | TuiMessage::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    if let Some((notice, _)) = &app.transient_notice {
        out.push(notice.clone());
    }
    out.join("\n")
}

/// The screen rows a draw paints, one `String` per terminal row.
///
/// Lets a test read the popup the way the user sees it: whole rows of
/// text, not styled cells. The draw always runs on the fixture's
/// 80×24 terminal, so the row indices stay stable across assertions.
pub(crate) fn screen_rows(subject: &mut TuiApp) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rows: Vec<String> = vec![String::new(); 24];
    for (index, cell) in buffer.content.iter().enumerate() {
        let (_, y) = buffer.pos_of(index);
        if let Some(row) = rows.get_mut(usize::from(y)) {
            row.push_str(cell.symbol());
        }
    }
    rows
}
