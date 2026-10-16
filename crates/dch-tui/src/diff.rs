//! Diff rendering for file-edit tool results.
//!
//! The edit family's receipts carry unified-diff hunks with real line
//! numbers (the `@@` headers the tool emits). This module parses that
//! shape into structured hunks and renders it as a review-grade
//! diff: a GitHub-style stats bar, then full-width solid bands —
//! green under insertions, red under removals, bright text on both —
//! each row led by its real line number and the seam. The inline
//! layout is the default; the side-by-side layout pairs removals with
//! their replacements across a center gutter. Receipts that carry no
//! hunks (errors, the large-file preview, non-UTF-8 notes) parse as
//! `None`, and the caller falls back to the plain expansion.

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::theme::Theme;

/// The width the line-number gutter takes.
///
/// Five digit columns; line numbers beyond five digits render
/// truncated from the left by the formatter, which a real source file
/// practically never reaches.
const NUMBER_GUTTER: usize = 5;

/// The column count the side-by-side center gutter takes.
///
/// A space, the divider, and a space: enough to read as a seam, never
/// enough to cost a code column.
const CENTER_GUTTER: &str = " │ ";

/// The side-by-side two-pane floor, in terminal columns.
///
/// Below this width the panes could not hold a meaningful code column
/// each, so [`render_diff`] degrades the side-by-side layout to
/// inline — the layout setting is a preference, not a demand that the
/// terminal cannot refuse.
const SIDE_BY_SIDE_FLOOR: usize = 80;

/// The insertions' band: the `GitHub`-style green.
///
/// The hue a code review taught the eye to read as "added"; fixed
/// for every theme because a solid band carries its own contrast and
/// needs no palette's permission.
const ADD_BAND: Color = Color::Rgb(35, 134, 54);

/// The removals' band: the `GitHub`-style red.
///
/// The hue a code review taught the eye to read as "removed"; fixed
/// for every theme, the twin of [`ADD_BAND`].
const DEL_BAND: Color = Color::Rgb(218, 54, 51);

/// The banded rows' text: bright white on either band.
///
/// The one foreground the two hues share comfortably; carrying it as
/// a constant keeps every banded span reading identically.
const BAND_TEXT: Color = Color::Rgb(255, 255, 255);

/// One parsed diff line: its kind, its text, and its 1-based line
/// number on whichever side it occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffLine {
    /// Whether the line was removed from the old file.
    ///
    /// Drives the red band and the old-side gutter claim; a removal
    /// carries no new-side number.
    deleted: bool,

    /// Whether the line was added to the new file.
    ///
    /// Drives the green band and the new-side gutter claim; an
    /// insertion carries no old-side number.
    inserted: bool,

    /// The line's text, without its diff prefix.
    ///
    /// Exactly the source bytes the tool diffed; rendering owns every
    /// prefix, gutter, and tint around them.
    text: String,

    /// The line's 1-based number in the old file, when it has one.
    ///
    /// `None` on insertions — they exist only in the new file — and
    /// on a blank pane half of an unpaired side-by-side row.
    old_no: Option<u32>,

    /// The line's 1-based number in the new file, when it has one.
    ///
    /// `None` on removals — they exist only in the old file — and on
    /// blank pane halves, mirroring [`DiffLine::old_no`].
    new_no: Option<u32>,
}

/// One `@@` hunk: its header line and the diff lines it covers.
///
/// The unit the tool emits and the renderer walks — one hunk renders
/// as its lines alone (the addresses carry the header's facts), and
/// the side-by-side pairing never crosses a hunk boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffHunk {
    /// The hunk's `@@ -a,b +c,d @@` header, parsed for its starts.
    ///
    /// The numbers the tool computed when it emitted the receipt; the
    /// parser anchors its per-line numbering on them, so the display
    /// shows the file's real addresses without re-reading the file.
    header: String,

    /// The hunk's lines in diff order.
    ///
    /// The order the tool emitted: context, removals, insertions, in
    /// source sequence — the pairing into side-by-side rows happens
    /// at render time, never in the parsed shape.
    lines: Vec<DiffLine>,
}

/// One file section of a receipt.
///
/// A receipt holds one per file it touched; the display opens each
/// with its stats bar so a `MultiEdit`'s rows always follow their own
/// file's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffFile {
    /// The section header — `Changed: path (modified)` or
    /// `Created: path (new file)`.
    ///
    /// Parsed for section kind; the stats bar replaces it in the
    /// rendered block, so the raw line itself is kept only for the
    /// summary the tests assert on.
    header: String,

    /// The summary block: the change summary, any end-of-file note,
    /// or a hunkless section's whole prose, in receipt order.
    ///
    /// What renders of it depends on the section: a hunked `Changed:`
    /// section folds it away (the chips carry its counts), a
    /// `Created:` section renders its elision tail after the hunks
    /// (the chips count only what the capped preview carried), and a
    /// hunkless section renders it whole — the section's only
    /// content.
    summary: Vec<String>,

    /// The file's hunks, in order.
    ///
    /// Empty for a section whose receipt carried none — the parse as
    /// a whole then yields `None`, keeping unrenderable shapes out of
    /// the diff path entirely.
    hunks: Vec<DiffHunk>,
}

/// A receipt parsed into renderable diff sections.
///
/// Built only when at least one section carries hunks; every other
/// receipt shape stays on the plain expansion path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReceiptDiff {
    /// The file sections, in receipt order.
    ///
    /// Multi-file receipts keep their order; the renderer walks them
    /// in sequence so the display mirrors the tool's own report.
    files: Vec<DiffFile>,
}

/// Parse an edit-family receipt into diff sections.
///
/// Recognizes the tool's own shapes — `Changed: … (modified)` sections
/// with `@@` hunks and `Created: … (new file)` sections of sequential
/// additions — and returns `None` for everything else: error bodies,
/// the large-file before/after preview, and the non-UTF-8 notes. The
/// caller treats `None` as "render the plain expansion", so an
/// unrecognized receipt can never render worse than it did before
/// diffs existed.
pub(crate) fn parse_receipt(text: &str) -> Option<ReceiptDiff> {
    let mut files: Vec<DiffFile> = Vec::new();
    for (header, body) in split_sections(text) {
        if header.starts_with("Created: ") {
            files.push(parse_created_section(&header, &body));
        } else if header.starts_with("Changed: ") {
            files.push(parse_changed_section(&header, &body));
        }
    }
    let with_hunks = files.iter().any(|file| !file.hunks.is_empty());
    (!files.is_empty() && with_hunks).then_some(ReceiptDiff { files })
}

/// Split a receipt into `(header, body)` sections.
///
/// A section opens where the tool writes its `Changed:` / `Created:`
/// header and runs to the next one; text before the first header is
/// preamble the diff renderer does not own.
fn split_sections(text: &str) -> Vec<(String, Vec<&str>)> {
    /// How many leading space characters a line carries.
    ///
    /// Counts characters, not bytes: the count is only ever used as a
    /// column offset, and the later `get` slice on it must land on a
    /// character boundary to exist at all.
    fn leading_spaces(line: &str) -> usize {
        line.chars()
            .count()
            .saturating_sub(line.trim_start_matches(' ').chars().count())
    }
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    let mut indent = 0_usize;
    // A hunk is open from its `@@` header until the first line that
    // is neither blank nor diff-prefixed — the same closure rule the
    // body parser applies. While a hunk is open, a line whose trimmed
    // form begins `Changed: `/`Created: ` is a context row for a
    // source line that happens to read that way, not a section
    // header: the emitter writes context as two spaces plus the
    // source text, so the shapes are indistinguishable after space
    // trimming, and only the hunk state can tell them apart.
    let mut hunk_open = false;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim_start_matches(' ');
        let header_shaped = trimmed.starts_with("Changed: ") || trimmed.starts_with("Created: ");
        if header_shaped && !hunk_open {
            indent = leading_spaces(line);
            sections.push((trimmed.to_string(), Vec::new()));
            continue;
        }
        if let Some((_, body)) = sections.last_mut() {
            // A body line inside an indented section carries the same
            // indent — strip exactly that many columns so the diff
            // prefixes inside are the tools' own shapes. Lines that
            // start shallower (a section-divider rule, the trailing
            // applied-files block) keep their columns untouched.
            let stripped = if leading_spaces(line) >= indent {
                line.get(indent..).unwrap_or(line)
            } else {
                line
            };
            if stripped.starts_with("@@ ") {
                hunk_open = true;
            } else if hunk_open && !stripped.is_empty() && !stripped.starts_with([' ', '-', '+']) {
                hunk_open = false;
            }
            body.push(stripped);
        }
    }
    sections
}

/// Parse a `Created:` section: sequential additions, numbered from 1.
///
/// The tool's created-file preview caps at its fixed line count and
/// closes with an elision tail; the additions become one
/// all-insertions hunk whose numbers are the file's own, from its
/// first line.
fn parse_created_section(header: &str, body: &[&str]) -> DiffFile {
    let mut hunk = DiffHunk {
        header: String::new(),
        lines: Vec::new(),
    };
    let mut summary: Vec<String> = Vec::new();
    let mut next_no = 1_u32;
    for line in body {
        if let Some(text) = line.strip_prefix("+ ") {
            hunk.lines.push(DiffLine {
                deleted: false,
                inserted: true,
                text: text.to_string(),
                old_no: None,
                new_no: Some(next_no),
            });
            next_no = next_no.saturating_add(1);
        } else if !line.is_empty() {
            summary.push((*line).to_string());
        }
    }
    DiffFile {
        header: header.to_string(),
        summary,
        hunks: if hunk.lines.is_empty() {
            Vec::new()
        } else {
            vec![hunk]
        },
    }
}

/// Parse a `Changed:` section: summary block, then `@@` hunks.
///
/// The lines before the first `@@` header — the change summary and
/// the end-of-file note, when present — form the summary block; every
/// later line belongs to the hunk its `@@` opened. The hunk headers'
/// own start numbers anchor the per-line numbering, the same numbers
/// the tool computed when it emitted the receipt.
fn parse_changed_section(header: &str, body: &[&str]) -> DiffFile {
    let mut summary: Vec<String> = Vec::new();
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut old_no = 0_u32;
    let mut new_no = 0_u32;
    // A multi-file receipt carries prose after the section's diff —
    // the next `File n:` label, a divider rule, the applied-files
    // block. None of it is diff content, and its `+` rows would
    // otherwise count as insertions; the first such line closes the
    // section's diff for good.
    let mut closed = false;
    for line in body {
        if closed {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let hunk = DiffHunk {
                header: format!("@@ {rest}"),
                lines: Vec::new(),
            };
            old_no = hunk_start(&hunk, false).saturating_sub(1);
            new_no = hunk_start(&hunk, true).saturating_sub(1);
            hunks.push(hunk);
        } else if hunks.is_empty() {
            // Before the first hunk header every non-empty line is
            // summary prose — the tool's own summary opens with a
            // `│` character no diff row carries, but the block can
            // hold more than that line, so position, not spelling,
            // is what makes it summary.
            if !line.is_empty() {
                summary.push((*line).to_string());
            }
        } else if line.is_empty() || !line.starts_with([' ', '-', '+']) {
            // Post-hunk prose closes the section's diff; empty lines
            // within a diff are blank context rows, not prose, so the
            // closure keys on content, not on blankness.
            if !line.is_empty() {
                closed = true;
            }
        } else if let Some(text) = line.strip_prefix("- ") {
            old_no = old_no.saturating_add(1);
            if let Some(hunk) = hunks.last_mut() {
                hunk.lines.push(DiffLine {
                    deleted: true,
                    inserted: false,
                    text: text.to_string(),
                    old_no: Some(old_no),
                    new_no: None,
                });
            }
        } else if let Some(text) = line.strip_prefix("+ ") {
            new_no = new_no.saturating_add(1);
            if let Some(hunk) = hunks.last_mut() {
                hunk.lines.push(DiffLine {
                    deleted: false,
                    inserted: true,
                    text: text.to_string(),
                    old_no: None,
                    new_no: Some(new_no),
                });
            }
        } else if let Some(text) = line.strip_prefix("  ") {
            old_no = old_no.saturating_add(1);
            new_no = new_no.saturating_add(1);
            if let Some(hunk) = hunks.last_mut() {
                hunk.lines.push(DiffLine {
                    deleted: false,
                    inserted: false,
                    text: text.to_string(),
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                });
            }
        }
    }
    DiffFile {
        header: header.to_string(),
        summary,
        hunks,
    }
}

/// Read a hunk header's start line for one side.
///
/// The header shape the tool emits is `@@ -old,count +new,count @@`;
/// the parser reads the old start for the old side and the new start
/// for the new one. A malformed header reads as 1 — the numbering
/// then walks from the top of the hunk, wrong in address but never in
/// order, and only a hand-edited receipt can get here.
fn hunk_start(hunk: &DiffHunk, new_side: bool) -> u32 {
    let marker = if new_side { "+" } else { "-" };
    hunk.header
        .strip_prefix("@@ ")
        .and_then(|rest| rest.split(' ').find(|part| part.starts_with(marker)))
        .and_then(|part| part.strip_prefix(marker))
        .and_then(|side| side.split(',').next())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(1)
}

/// The diff palette: the look the bands define.
///
/// Solid GitHub-style bands with bright text — the one place the
/// display deliberately ignores the active theme's hues: a solid
/// band carries its own contrast, so the same green and red read on
/// every palette, light or dark.
/// The neutral registers (context, numbers, the seam) do follow the
/// theme, staying dim wherever they appear.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiffColors {
    /// The insertions' band: `GitHub` green.
    ///
    /// Fixed by [`ADD_BAND`] and never re-tinted by a theme.
    add_bg: Color,

    /// The removals' band: `GitHub` red.
    ///
    /// Fixed by [`DEL_BAND`] and never re-tinted by a theme.
    del_bg: Color,

    /// The banded rows' text: bright white on either band.
    ///
    /// Bright-on-solid reads at every palette; that is the choice.
    band_fg: Color,

    /// The context lines' tone.
    ///
    /// The theme's dim register: context is wayfinding, not content.
    ctx_fg: Color,

    /// The line-number gutters' tone on plain rows.
    ///
    /// Dim like the context — the numbers must be readable without
    /// competing with the code they address.
    num_fg: Color,

    /// The gutter seam's tone.
    ///
    /// A dark gray seam between address and content.
    seam_fg: Color,
}

/// Derive the diff palette from `theme`.
///
/// Only the neutral registers read the theme; the bands are fixed by
/// [`ADD_BAND`] / [`DEL_BAND`] and never vary with the palette.
pub(crate) fn diff_colors(theme: &Theme) -> DiffColors {
    DiffColors {
        add_bg: ADD_BAND,
        del_bg: DEL_BAND,
        band_fg: BAND_TEXT,
        ctx_fg: theme.ui.dim,
        num_fg: theme.ui.dim,
        seam_fg: Color::DarkGray,
    }
}

/// Render a parsed receipt in the configured layout.
///
/// Each file section opens with the stats bar — additions and
/// removals as solid chips beside a dim total — followed by its
/// hunks: full-width banded rows in the inline layout (the default),
/// or paired panes for side-by-side, degrading to inline below the
/// two-pane floor.
pub(crate) fn render_diff(
    diff: &ReceiptDiff,
    layout: dch_config::DiffLayout,
    width: u16,
    colors: DiffColors,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    let inline = match layout {
        dch_config::DiffLayout::Inline => true,
        dch_config::DiffLayout::SideBySide => width < SIDE_BY_SIDE_FLOOR,
    };
    for file in &diff.files {
        // A section the tool could not diff — the large-file
        // before/after preview, a not-UTF-8 prior — carries its facts
        // as prose, and no stats bar: `+0 -0` would claim no change
        // exactly where a massive one happened. Its own header and
        // prose render in the dim register instead, the same
        // information the plain expansion would have shown.
        if file.hunks.is_empty() {
            out.push(Line::styled(
                file.header.clone(),
                Style::default().fg(colors.num_fg),
            ));
            for prose in &file.summary {
                out.push(Line::styled(
                    prose.clone(),
                    Style::default().fg(colors.ctx_fg),
                ));
            }
            continue;
        }
        let added = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.inserted)
            .count();
        let removed = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.deleted)
            .count();
        out.push(Line::from(vec![
            Span::styled(
                format!(" +{added} "),
                Style::default().fg(colors.band_fg).bg(colors.add_bg),
            ),
            Span::styled(
                format!(" -{removed} "),
                Style::default().fg(colors.band_fg).bg(colors.del_bg),
            ),
            Span::styled(
                format!("  {} lines changed", added.saturating_add(removed)),
                Style::default().fg(colors.num_fg),
            ),
        ]));
        for hunk in &file.hunks {
            if inline {
                out.extend(render_hunk_inline(hunk, width, colors));
            } else {
                out.extend(render_hunk_side_by_side(hunk, width, colors));
            }
        }
        // A created section's prose is its elision tail — the preview
        // caps at a fixed line count, and the chips count only what
        // the receipt carried. The tail names what lies beyond the
        // cap, so the bar's count stays honest.
        if file.header.starts_with("Created: ") {
            for prose in &file.summary {
                out.push(Line::styled(
                    prose.clone(),
                    Style::default().fg(colors.ctx_fg),
                ));
            }
        }
    }
    out
}

/// Render one hunk in the inline shape.
///
/// One row per source line: the line's address (its old number for
/// removals and context, its new number for insertions), the seam
/// (`│`), the sign glyph, and the text — removals and insertions as
/// solid full-width bands in the fixed `GitHub` hues with bright text,
/// context unpainted in the theme's dim register. Long content wraps
/// onto continuation rows that keep the band with a blank address and
/// sign, and the padding fills each row to the full width so the
/// bands read as bars, not fragments.
fn render_hunk_inline(hunk: &DiffHunk, width: usize, colors: DiffColors) -> Vec<Line<'static>> {
    /// The address column plus the seam and sign columns the content
    /// starts after.
    const ROW_PREFIX: usize = NUMBER_GUTTER.saturating_add(2);
    let content_room = width.saturating_sub(ROW_PREFIX).max(1);
    let mut out = Vec::new();
    for line in &hunk.lines {
        let (address, sign, band, plain_fg) = if line.deleted {
            (line.old_no, '-', Some(colors.del_bg), colors.band_fg)
        } else if line.inserted {
            (line.new_no, '+', Some(colors.add_bg), colors.band_fg)
        } else {
            (line.old_no, ' ', None, colors.ctx_fg)
        };
        let mut chunks = wrap_chunks(&line.text, content_room);
        if chunks.is_empty() {
            // An empty source line still owes its banded row.
            chunks.push(String::new());
        }
        for (row, chunk) in chunks.into_iter().enumerate() {
            let first_row = row == 0;
            let address_cell = if first_row {
                format!("{:>NUMBER_GUTTER$}", address.unwrap_or_default())
            } else {
                " ".repeat(NUMBER_GUTTER)
            };
            let sign_cell = if first_row { sign } else { ' ' };
            let address_style = match band {
                Some(band) => Style::default().fg(colors.band_fg).bg(band),
                None => Style::default().fg(colors.num_fg),
            };
            let seam_style = match band {
                Some(band) => Style::default().fg(colors.band_fg).bg(band),
                None => Style::default().fg(colors.seam_fg),
            };
            out.push(Line::from(vec![
                Span::styled(address_cell, address_style),
                Span::styled("│", seam_style),
                Span::styled(
                    sign_cell.to_string(),
                    Style::default()
                        .fg(plain_fg)
                        .bg(band.unwrap_or(Color::Reset)),
                ),
                Span::styled(
                    pad_to_width(&chunk, width.saturating_sub(ROW_PREFIX)),
                    Style::default()
                        .fg(plain_fg)
                        .bg(band.unwrap_or(Color::Reset)),
                ),
            ]));
        }
    }
    out
}

/// Render one hunk in the side-by-side shape.
///
/// The inline look, twice: each side owns its pane — address, seam,
/// sign, text — with removals as solid full-width red bands on the
/// left pane and insertions as green on the right, context unpainted
/// in the dim register on both. Removal runs pair index-wise with the
/// insertion runs that follow them, so a replacement reads as one
/// row; content wraps per pane, the shorter side carrying blank
/// continuation rows that keep its band, so the panes never lose
/// alignment and nothing is truncated.
fn render_hunk_side_by_side(
    hunk: &DiffHunk,
    width: usize,
    colors: DiffColors,
) -> Vec<Line<'static>> {
    const PANE_SEAM: usize = 1;
    const MID_SEAM: usize = 3;
    const SIGN_COLUMN: usize = 1;
    let pane_total = width.saturating_sub(MID_SEAM) / 2;
    let text_room = pane_total
        .saturating_sub(NUMBER_GUTTER)
        .saturating_sub(PANE_SEAM)
        .saturating_sub(SIGN_COLUMN)
        .max(1);
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(line) = hunk.lines.get(index) {
        if line.deleted {
            let del_run = hunk
                .lines
                .get(index..)
                .into_iter()
                .flat_map(|rest| rest.iter())
                .take_while(|entry| entry.deleted)
                .count();
            let add_start = index.saturating_add(del_run);
            let add_run = hunk
                .lines
                .get(add_start..)
                .into_iter()
                .flat_map(|rest| rest.iter())
                .take_while(|entry| entry.inserted)
                .count();
            for pair in 0..del_run.max(add_run) {
                let left = hunk
                    .lines
                    .get(index.saturating_add(pair))
                    .filter(|e| e.deleted);
                let right = hunk
                    .lines
                    .get(add_start.saturating_add(pair))
                    .filter(|e| e.inserted);
                out.extend(side_by_side_rows(left, right, text_room, colors));
            }
            index = add_start.saturating_add(add_run);
        } else {
            let context = !line.deleted && !line.inserted;
            out.extend(side_by_side_rows(
                Some(line).filter(|e| !e.inserted),
                if context {
                    Some(line)
                } else {
                    Some(line).filter(|e| e.inserted)
                },
                text_room,
                colors,
            ));
            index = index.saturating_add(1);
        }
    }
    out
}

/// Compose the side-by-side rows one pair renders.
///
/// Each side wraps independently; the row count is the longer side's,
/// and the shorter side pads with blank continuation rows that keep
/// its band — alignment over truncation, the same trade the inline
/// layout makes with its own continuations.
fn side_by_side_rows(
    left: Option<&DiffLine>,
    right: Option<&DiffLine>,
    text_room: usize,
    colors: DiffColors,
) -> Vec<Line<'static>> {
    let wrapped = |line: Option<&DiffLine>| -> Vec<String> {
        match line {
            Some(line) => {
                let mut chunks = wrap_chunks(&line.text, text_room);
                if chunks.is_empty() {
                    chunks.push(String::new());
                }
                chunks
            }
            None => Vec::new(),
        }
    };
    let left_chunks = wrapped(left);
    let right_chunks = wrapped(right);
    let row_count = left_chunks.len().max(right_chunks.len()).max(1);
    let mut out = Vec::new();
    for row in 0..row_count {
        let left_chunk = left_chunks.get(row).map(String::as_str);
        let right_chunk = right_chunks.get(row).map(String::as_str);
        let left_band = left.filter(|line| line.deleted).map(|_| colors.del_bg);
        let right_band = right.filter(|line| line.inserted).map(|_| colors.add_bg);
        let first_row = row == 0;
        let mut spans = pane_spans(
            left.and_then(|line| line.old_no),
            left_chunk,
            first_row,
            left.map_or(' ', |line| if line.deleted { '-' } else { ' ' }),
            text_room,
            colors,
            left_band,
        );
        spans.push(Span::styled(
            CENTER_GUTTER.to_string(),
            Style::default().fg(colors.seam_fg),
        ));
        spans.extend(pane_spans(
            right.and_then(|line| line.new_no),
            right_chunk,
            first_row,
            right.map_or(' ', |line| if line.inserted { '+' } else { ' ' }),
            text_room,
            colors,
            right_band,
        ));
        out.push(Line::from(spans));
    }
    out
}

/// One pane's spans for one side-by-side row.
///
/// The first wrapped row of a side carries its address and sign;
/// continuation rows blank them and keep whatever band the side
/// carries, and the text pads to the pane so a band is a solid bar.
/// A chunk of `None` is the blank continuation of a shorter side —
/// all blanks, no band of its own.
fn pane_spans(
    number: Option<u32>,
    chunk: Option<&str>,
    first_row: bool,
    sign: char,
    room: usize,
    colors: DiffColors,
    band: Option<Color>,
) -> Vec<Span<'static>> {
    let address = if first_row && chunk.is_some() {
        format!("{:>NUMBER_GUTTER$}", number.unwrap_or_default())
    } else {
        " ".repeat(NUMBER_GUTTER)
    };
    let sign_cell = if first_row && chunk.is_some() {
        sign
    } else {
        ' '
    };
    let plain = Style::default().fg(colors.ctx_fg);
    let banded = |bg: Color| Style::default().fg(colors.band_fg).bg(bg);
    let address_style = band.map_or_else(|| Style::default().fg(colors.num_fg), banded);
    let seam_style = band.map_or_else(|| Style::default().fg(colors.seam_fg), banded);
    let text_style = band.map_or(plain, banded);
    let sign_style = band.map_or_else(|| Style::default().fg(colors.ctx_fg), banded);
    vec![
        Span::styled(address, address_style),
        Span::styled("│", seam_style),
        Span::styled(sign_cell.to_string(), sign_style),
        Span::styled(pad_to_width(chunk.unwrap_or_default(), room), text_style),
    ]
}

/// Split `text` into display-width-bounded chunks.
///
/// Greedy on characters, wide-glyph aware: a glyph never splits
/// across chunks, and a chunk never exceeds `room` columns. An empty
/// text yields no chunks — callers that owe a row for an empty source
/// line add it themselves.
fn wrap_chunks(text: &str, room: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar as _;
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut used = 0_usize;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used.saturating_add(ch_width) > room && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            used = 0;
        }
        used = used.saturating_add(ch_width);
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Pad `text` with trailing spaces to exactly `target` display
/// columns, or leave it when it already meets or exceeds them.
///
/// Solid bands need full-width rows; the padding rides the band's
/// own style, so it costs no span of its own.
fn pad_to_width(text: &str, target: usize) -> String {
    use unicode_width::UnicodeWidthStr as _;
    let current = text.width();
    if current >= target {
        text.to_string()
    } else {
        let mut out = text.to_string();
        out.extend(std::iter::repeat_n(' ', target.saturating_sub(current)));
        out
    }
}

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::panic
)]
mod tests {
    use super::*;

    /// The receipt shape the edit tool emits for a mid-file
    /// replacement — summary prose, hunk header, context, removal,
    /// insertion, closing context, each line exactly as
    /// `format_file_change` writes it.
    fn changed_receipt() -> String {
        [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -38,3 +38,3 @@",
            "  fn main() {",
            "-     let old = previous();",
            "+     let new = followup();",
            "  }",
        ]
        .join("\n")
    }

    #[test]
    fn a_changed_receipt_parses_with_real_line_numbers() {
        let diff = parse_receipt(&changed_receipt()).expect("the receipt is diff-shaped");
        assert_eq!(diff.files.len(), 1, "one file section");
        let file = &diff.files[0];
        assert_eq!(file.header, "Changed: src/lib.rs (modified)");
        assert_eq!(file.summary, vec!["│ 1 line removed, 1 added".to_string()]);
        let hunk = file.hunks.first().expect("one hunk");
        assert_eq!(hunk.header, "@@ -38,3 +38,3 @@");
        let context = &hunk.lines[0];
        assert_eq!((context.old_no, context.new_no), (Some(38), Some(38)));
        let removed = &hunk.lines[1];
        assert!(removed.deleted && !removed.inserted);
        assert_eq!(removed.old_no, Some(39));
        assert_eq!(removed.new_no, None);
        let added = &hunk.lines[2];
        assert!(added.inserted && !added.deleted);
        assert_eq!(added.old_no, None);
        assert_eq!(added.new_no, Some(39));
        let closing = &hunk.lines[3];
        assert_eq!((closing.old_no, closing.new_no), (Some(40), Some(40)));
    }

    #[test]
    fn a_created_receipt_parses_as_sequential_additions() {
        let receipt = [
            "Created: notes.md (new file)",
            "+ first",
            "+ second",
            "... +3 more lines",
        ]
        .join("\n");
        let diff = parse_receipt(&receipt).expect("the created receipt is diff-shaped");
        let hunk = diff.files[0].hunks.first().expect("one synthetic hunk");
        assert_eq!(hunk.lines[0].new_no, Some(1));
        assert_eq!(hunk.lines[1].new_no, Some(2));
        assert!(hunk.lines.iter().all(|line| line.inserted));
        // The render keeps the receipt honest about its cap: the chips
        // count what the preview carried, and the elision tail rides
        // after the rows as its own dim line.
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::Inline, 40, colors);
        let flat: String = rows
            .iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            flat.contains("... +3 more lines"),
            "the elision tail renders after the capped rows: {flat}"
        );
    }

    #[test]
    fn a_source_line_shaped_like_a_header_never_hijacks_an_open_hunk() {
        // A context row for a source line that itself begins
        // "Changed: " renders as two spaces of diff context plus that
        // text — space-trimmed, it looks exactly like a section
        // header. Mid-hunk it is body: the section keeps its lines
        // and numbering, and no phantom section opens.
        let receipt = [
            "Changed: log.rs (modified)",
            "│ 2 lines removed, 2 added",
            "@@ -3,4 +3,4 @@",
            "  fn run() {",
            "  Changed: alpha",
            "- let old = previous();",
            "+ let new = followup();",
            "  }",
        ]
        .join("\n");
        let diff = parse_receipt(&receipt).expect("the receipt parses");
        assert_eq!(
            diff.files.len(),
            1,
            "the context line opens no phantom section"
        );
        let hunk = diff.files[0].hunks.first().expect("the real hunk");
        assert_eq!(
            hunk.lines.len(),
            5,
            "every diff row stays in the hunk: {:?}",
            hunk.lines
        );
        let hijacked = &hunk.lines[1];
        assert_eq!(
            hijacked.text, "Changed: alpha",
            "the lookalike line is context, verbatim"
        );
        assert_eq!(
            (hijacked.old_no, hijacked.new_no),
            (Some(4), Some(4)),
            "and the numbering walks straight past it"
        );
    }

    #[test]
    fn a_mixed_receipt_renders_a_hunkless_section_s_preview_not_an_empty_bar() {
        // The multi-file shape that mixes a diffable file with a
        // large-file preview: the preview section carries no hunks,
        // and an empty `+0 -0` bar would claim no change where a
        // massive one happened. Its own prose renders dim instead —
        // the plain expansion's information, in the diff's frame.
        let receipt = [
            "Multi-File Edit Summary",
            "",
            "File 1: small.rs",
            "  Changed: small.rs (modified)",
            "  │ 1 line removed, 1 added",
            "  @@ -5,1 +5,1 @@",
            "  - old();",
            "  + new();",
            "",
            "File 2: giant.generated",
            "  Changed: giant.generated (modified, large file)",
            "  Before:",
            "  - machine output line",
            "  After:",
            "  + regenerated machine output",
        ]
        .join("\n");
        let diff = parse_receipt(&receipt).expect("the receipt parses");
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::Inline, 80, colors);
        let flat: String = rows
            .iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !flat.contains("+0") || !flat.contains("0 lines changed"),
            "no empty stats bar renders: {flat}"
        );
        assert!(
            flat.contains("Before:"),
            "the large file's preview prose renders: {flat}"
        );
        assert!(
            flat.contains("regenerated machine output"),
            "the preview's content is visible: {flat}"
        );
    }

    #[test]
    fn a_multi_edit_receipt_parses_across_its_indented_sections() {
        // The multi-file summary indents every section two spaces
        // under its `File n:` label — the tool's own shape, verbatim
        // from the golden corpus. Both files parse with their hunks
        // and real numbers, and render with a stats bar each.
        let receipt = [
            "Multi-File Edit Summary",
            "",
            "File 1: a.txt",
            "  Changed: a.txt (modified)",
            "  │ 1 line removed, 1 line added",
            "  @@ -12,1 +12,1 @@",
            "  - alpha",
            "  + ALPHA",
            "",
            "File 2: b.txt",
            "  Changed: b.txt (modified)",
            "  │ 1 line removed, 1 line added",
            "  @@ -7,1 +7,1 @@",
            "  - beta",
            "  + BETA",
            "",
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
            "Applied: 2 file(s)",
            "  + a.txt",
            "  + b.txt",
        ]
        .join("\n");
        let diff = parse_receipt(&receipt).expect("the indented multi-edit receipt parses");
        assert_eq!(diff.files.len(), 2, "both file sections parse");
        let first = &diff.files[0];
        assert_eq!(first.header, "Changed: a.txt (modified)");
        let hunk = first.hunks.first().expect("a.txt's hunk");
        let removed = hunk.lines.first().expect("the removal");
        assert_eq!(
            removed.old_no,
            Some(12),
            "the indent did not shift the address"
        );
        let second = &diff.files[1];
        let hunk = second.hunks.first().expect("b.txt's hunk");
        let inserted = hunk.lines.last().expect("the insertion");
        assert_eq!(inserted.new_no, Some(7));
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::Inline, 60, colors);
        let stats_bars = rows
            .iter()
            .filter(|row| {
                let text: String = row.spans.iter().map(|s| s.content.to_string()).collect();
                text.contains("lines changed")
            })
            .count();
        assert_eq!(stats_bars, 2, "each file section opens its own stats bar");
    }

    #[test]
    fn a_verbatim_tool_receipt_parses_and_renders() {
        // The tool's bytes, pasted from the loopctl golden corpus
        // (`tests/golden/fs/edit_found.golden`): the `│` summary
        // prefix, the hunk header, and the diff rows exactly as
        // emitted — the parser's contract is the tool's output, not
        // a shape this file invented.
        let receipt = "Changed: a.rs (modified)\n│ 1 line removed, 1 line added\n@@ -1,2 +1,2 @@\n  fn one() {}\n- fn two() {}\n+ fn two() { todo!() }\n";
        let diff = parse_receipt(receipt).expect("the verbatim receipt parses");
        let file = &diff.files[0];
        assert_eq!(
            file.summary,
            vec!["│ 1 line removed, 1 line added".to_string()]
        );
        let hunk = file.hunks.first().expect("the hunk");
        // Context, removal, insertion — the edit replaced the file's
        // last line, so no closing context follows.
        assert_eq!(hunk.lines.len(), 3);
        assert_eq!(hunk.lines[1].text, "fn two() {}");
        assert_eq!(hunk.lines[2].text, "fn two() { todo!() }");
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::Inline, 60, colors);
        let flat: String = rows
            .iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            flat.contains("todo!()"),
            "the inserted text renders: {flat}"
        );
    }

    #[test]
    fn non_diff_receipts_parse_as_none() {
        // Error bodies, the large-file preview, and labeled notes carry
        // no hunks: the caller keeps its plain expansion for these.
        assert!(parse_receipt("Edited 3 files").is_none());
        assert!(
            parse_receipt("Changed: big.rs (modified, large file)\nBefore:\n- one\nAfter:\n+ two")
                .is_none(),
            "the large-file before/after preview owns no hunk headers"
        );
        assert!(parse_receipt("Changed: f.bin (modified, previous content not UTF-8)\n").is_none());
    }

    #[test]
    fn inline_rows_match_the_poc_look() {
        let diff = parse_receipt(&changed_receipt()).expect("parses");
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::Inline, 80, colors);
        // The stats bar opens the block: two chips and a dim total.
        let stats: String = rows[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            stats.contains("+1") && stats.contains("-1") && stats.contains("2 lines changed"),
            "the stats bar leads the block: {stats}"
        );
        assert_eq!(
            rows[0].spans[0].style.bg,
            Some(ADD_BAND),
            "the additions chip is the fixed green band"
        );
        assert_eq!(
            rows[0].spans[1].style.bg,
            Some(DEL_BAND),
            "the removals chip is the fixed red band"
        );
        // Rows: context, removal, insertion, closing context — one
        // address column, the seam, the sign, bright text on the band,
        // no hunk header row.
        let opener = &rows[1];
        let opener_text: String = opener.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            opener_text.starts_with("   38│"),
            "context opens the hunk with its address: {opener_text:?}"
        );
        let removal = &rows[2];
        let removal_text: String = removal
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            removal_text.starts_with("   39│-"),
            "single address column, seam, sign — the inline row: {removal_text:?}"
        );
        assert_eq!(removal.spans[3].style.bg, Some(DEL_BAND));
        assert_eq!(removal.spans[3].style.fg, Some(BAND_TEXT));
        assert!(
            removal_text.contains("previous();"),
            "the removed text rides the band: {removal_text}"
        );
        let insertion = &rows[3];
        let insertion_text: String = insertion
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            insertion_text.starts_with("   39│+"),
            "the insertion leads with its new address: {insertion_text:?}"
        );
        assert_eq!(insertion.spans[3].style.bg, Some(ADD_BAND));
        let closing = &rows[4];
        let closing_text: String = closing
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            closing_text.starts_with("   40│"),
            "closing context carries its address unpainted: {closing_text:?}"
        );
        assert_eq!(closing.spans[3].style.bg, Some(Color::Reset));
        assert!(
            !rows
                .iter()
                .any(|row| row.spans.iter().any(|span| span.content.contains("@@"))),
            "the hunk header never renders — the addresses carry it"
        );
        // Solid bars: banded rows pad to the full width.
        let band_width: usize = removal
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        assert_eq!(band_width, 80, "the band fills the row");
    }

    #[test]
    fn the_bands_are_the_same_on_every_theme() {
        // Solid bands carry their own contrast — the fixed greens
        // and reds read on light and dark palettes alike, and
        // no theme re-tints them.
        let dark = diff_colors(&crate::theme::Theme::default());
        let light = diff_colors(&crate::theme::Theme::gruvbox_light());
        let bare = diff_colors(&crate::theme::Theme::transparent());
        assert_eq!(dark.add_bg, ADD_BAND);
        assert_eq!(light.add_bg, ADD_BAND);
        assert_eq!(bare.add_bg, ADD_BAND);
        assert_eq!(dark.del_bg, DEL_BAND);
        assert_eq!(light.del_bg, DEL_BAND);
        assert_eq!(bare.del_bg, DEL_BAND);
    }

    #[test]
    fn wrapped_content_keeps_the_band_on_continuation_rows() {
        // A long line wraps: the first row carries address and sign,
        // the continuation keeps the band with blanks — wrapped
        // bands, never a truncated line.
        let receipt = [
            "Changed: src/long.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -5,2 +5,2 @@",
            &format!("- {}", "code ".repeat(60)),
            &format!("+ {}", "new code ".repeat(60)),
        ]
        .join("\n");
        let diff = parse_receipt(&receipt).expect("parses");
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::Inline, 40, colors);
        let first = &rows[1];
        assert!(
            first
                .spans
                .iter()
                .any(|span| span.style.bg == Some(DEL_BAND)),
            "the first wrapped row is banded"
        );
        let continuation = &rows[2];
        let cont_text: String = continuation
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            cont_text.starts_with("     │"),
            "the continuation blank address and sign: {cont_text:?}"
        );
        assert!(
            continuation
                .spans
                .iter()
                .any(|span| span.style.bg == Some(DEL_BAND)),
            "the band continues across the wrap"
        );
    }

    #[test]
    fn side_by_side_pairs_a_replacement_across_the_gutter() {
        let diff = parse_receipt(&changed_receipt()).expect("parses");
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::SideBySide, 120, colors);
        // The hunk renders: context, then removal and insertion PAIRED
        // on one row, then closing context — each pane carrying the
        // inline look's solid band, address, and sign.
        let paired = &rows[2];
        let text: String = paired.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("previous();") && text.contains("followup();"),
            "the row shows the old line and its replacement: {text}"
        );
        assert!(
            text.contains(CENTER_GUTTER.trim()),
            "the center gutter divides the panes: {text}"
        );
        assert!(
            text.starts_with("   39│-") && text.contains("   39│+"),
            "each pane leads with its own address and sign: {text:?}"
        );
        let left_band = paired.spans.first().map(|span| span.style.bg);
        assert_eq!(left_band, Some(Some(DEL_BAND)), "the old pane is red");
        let right_band = paired
            .spans
            .iter()
            .find(|span| span.style.bg == Some(ADD_BAND))
            .map(|span| span.style.bg);
        assert_eq!(right_band, Some(Some(ADD_BAND)), "the new pane is green");
        // Solid bars: the paired row fills the terminal width.
        let row_width: usize = paired
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        // Two even pane splits around the 3-column center gutter:
        // (120 - 3) / 2 = 58 per pane, 58 + 3 + 58 = 119 — the odd
        // column the gutter leaves stays unspent at the row's end.
        assert_eq!(row_width, 119, "both panes pad to solid bars: {text:?}");
        // Context rows render on both panes, unpainted.
        let context = &rows[1];
        let context_text: String = context
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            context_text.contains("fn main() {") && context_text.starts_with("   38│"),
            "context opens both panes with the old address: {context_text:?}"
        );
        assert!(
            !context
                .spans
                .iter()
                .any(|span| span.style.bg == Some(DEL_BAND) || span.style.bg == Some(ADD_BAND)),
            "context rows stay unpainted in side-by-side too"
        );
    }

    #[test]
    fn side_by_side_wraps_keep_alignment_with_blank_bands() {
        // A removal longer than its paired insertion wraps on the
        // left pane alone; the right pane carries blank continuation
        // rows that keep its green band, so the panes never skew and
        // nothing is truncated.
        let receipt = [
            "Changed: src/wide.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -7,2 +7,2 @@",
            &format!("- {}", "long removal ".repeat(30)),
            "+ short",
        ]
        .join("\n");
        let diff = parse_receipt(&receipt).expect("parses");
        let colors = diff_colors(&crate::theme::Theme::default());
        let rows = render_diff(&diff, dch_config::DiffLayout::SideBySide, 100, colors);
        let paired_rows: Vec<&Line> = rows
            .iter()
            .filter(|row| row.spans.iter().any(|span| span.style.bg == Some(DEL_BAND)))
            .collect();
        assert!(
            paired_rows.len() > 1,
            "the long removal wraps across several paired rows"
        );
        let last = paired_rows.last().expect("wrapped rows exist");
        let text: String = last.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains(CENTER_GUTTER.trim()),
            "the final wrapped row still spans both panes: {text:?}"
        );
        assert!(
            last.spans
                .iter()
                .any(|span| span.style.bg == Some(ADD_BAND)),
            "the shorter side keeps its band on the blank continuation: {text:?}"
        );
    }

    #[test]
    fn narrow_terminals_fall_back_to_inline_whatever_the_setting() {
        let diff = parse_receipt(&changed_receipt()).expect("parses");
        let colors = diff_colors(&crate::theme::Theme::default());
        let wide = render_diff(&diff, dch_config::DiffLayout::Inline, 60, colors);
        let requested = render_diff(&diff, dch_config::DiffLayout::SideBySide, 60, colors);
        let shape = |rows: &[Line]| {
            rows.iter()
                .map(|row| {
                    row.spans
                        .iter()
                        .map(|span| span.content.to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shape(&wide),
            shape(&requested),
            "below the two-pane floor the side-by-side setting renders inline"
        );
    }
}
