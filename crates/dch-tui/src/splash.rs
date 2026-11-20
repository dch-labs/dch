//! The empty-session splash: a dithered, two-color fjord.
//!
//! A new session — a fresh launch, or a conversation just cleared —
//! paints a photograph of a fjord above the welcome composer until its
//! first line of traffic arrives. The art is a precomputed
//! terminal-symbol rendering: each cell carries two colors and one
//! glyph, chosen offline by a two-color quantizer over the cell's four
//! sub-pixels with Floyd–Steinberg dithering — the quadrant-block
//! alphabet covers every sub-pixel pattern, and the fraction blocks
//! shade edges at eighths of a cell, so a cell holds twice the
//! vertical detail a plain half-block could. Twelve rungs are embedded
//! as text assets, so no image decoding, dependency, or graphics
//! protocol rides along.
//!
//! Two renditions of the same photograph ride along, and the session
//! picks one at first decode. The truecolor set is the original art —
//! every terminal that takes direct-color escapes shows it exactly,
//! which is every launch outside tmux and every tmux client carrying
//! the RGB feature. The palette set is the same photograph re-derived
//! for the fixed 256-color vocabulary — black, blues, teals, and
//! blue-grays with the neutral ramp held out, so the night stays a
//! blue night — for the one environment where truecolor cannot pass:
//! behind tmux whose client lacks the RGB feature, where tmux would
//! re-encode every direct color into that same palette per color and
//! blind, mangling the art. Palette indexes tmux passes through
//! verbatim, so that rendition arrives exactly as authored.
//! [`crate::terminal::truecolor_warranted`] draws the line; the
//! choice is made once per process.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;

/// Hex digits per encoded cell: one ink color, one ground color.
///
/// Twelve characters — two per channel — covering the glyph's ink
/// first and the cell's ground second, in that order.
const CELL_HEX: usize = 12;

/// Characters per encoded cell: the hex pair, then the glyph itself.
///
/// Every glyph in the alphabet is a single character, so the cell's
/// stride is the hex width plus one.
const CELL_CHARS: usize = CELL_HEX + 1;

/// One decoded splash scale.
///
/// The grid dimensions ride beside the painted rows so the fit check
/// and the centering math read one source of truth per scale; the
/// scales list orders its entries largest first, and the picker walks
/// it in that order.
pub(crate) struct SplashArt {
    /// The art's width in terminal cells.
    ///
    /// The largest scale spans one hundred nine cells; the welcome
    /// chooses the largest rung whose columns and rows both fit the
    /// pane, and paints it whole.
    pub(crate) width: u16,

    /// The art's height in terminal rows.
    ///
    /// One terminal row per encoded row; a pane must be at least this
    /// tall before the scale is offered.
    pub(crate) height: u16,

    /// The painted rows, one [`Line`] of two-color spans per row.
    ///
    /// Empty when the embedded asset failed its shape checks — a
    /// malformed scale is skipped rather than rendered, and every
    /// other scale still offers itself.
    pub(crate) lines: Vec<Line<'static>>,
}

/// The decoded scales of the rendition this session shows, largest
/// first.
///
/// Decoded once on first use and shared for the process's life. Both
/// renditions share one grid per rung — the same widths and heights —
/// so the welcome's fit walk is indifferent to which set was chosen.
fn scales() -> &'static [SplashArt] {
    static SCALES: OnceLock<Vec<SplashArt>> = OnceLock::new();
    SCALES.get_or_init(|| {
        if crate::terminal::truecolor_warranted() {
            truecolor_scales()
        } else {
            palette_scales()
        }
    })
}

/// The dissolve's reach, in columns from each vertical edge.
///
/// The photograph's own margins — the flat night its subject floats
/// in — are blue-black, and the canvas they sit on is neutral: side by
/// side the two read as a mismatched background flanking the image.
/// Four columns is the width of those margins in every rung, so the
/// dissolve never reaches the subject itself.
const EDGE_DISSOLVE_COLUMNS: usize = 4;

/// Dissolve the art's outer columns into the canvas it floats on.
///
/// The welcome's render path runs this over the cropped rows before
/// painting them, keyed to the live theme's canvas: the outermost
/// column goes **unstyled** — no ink, no ground, a cell the terminal
/// paints as its own default, which *is* the canvas, exactly, in
/// every vocabulary — and the strength falls off with the square of
/// the remaining reach, so the second column keeps most of its own
/// color and the photograph's margins fade as a gradient, the way a
/// printed photograph vignettes into its mat. Truecolor cells step
/// toward the canvas itself; palette cells step toward the palette's
/// floor beneath the canvas ([`palette_floor`]), because the fixed
/// ramp cannot name the canvas — its gray steps straddle it — and
/// snapping a blend onto the nearer step above would paint a rim
/// *lighter* than the page it dissolves into. Both the ink and the
/// ground blend, so a fraction block's glyphs vanish with the ground
/// instead of speckling the fade. A canvas the theme cannot name —
/// [`Color::Reset`], the transparent and deferred sessions —
/// dissolves nothing: the terminal's own background is the user's
/// choice, and the photograph keeps the margins it was authored
/// with.
pub(crate) fn dissolve_edges(lines: &mut [Line<'static>], canvas: Color) {
    let Color::Rgb(base_red, base_green, base_blue) = canvas else {
        return;
    };
    let reach = EDGE_DISSOLVE_COLUMNS;
    let span_square = u32::try_from(reach.saturating_mul(reach)).unwrap_or(1);
    let floor = palette_floor((base_red, base_green, base_blue));
    for line in lines {
        let width = line.spans.len();
        for (column, span) in line.spans.iter_mut().enumerate() {
            let from_left = column;
            let from_right = width.saturating_sub(column.saturating_add(1));
            let edge_distance = from_left.min(from_right);
            if edge_distance >= reach {
                continue;
            }
            let inside = reach.saturating_sub(edge_distance);
            if inside == reach {
                *span = Span::raw(" ");
                continue;
            }
            let numerator = u32::try_from(inside.saturating_mul(inside)).unwrap_or(0);
            let step = |color: Option<Color>| -> Option<Color> {
                toward_canvas(
                    color,
                    (base_red, base_green, base_blue),
                    floor,
                    numerator,
                    span_square,
                )
            };
            let style = span.style;
            if let Some(color) = step(style.fg) {
                span.style = style.fg(color);
            }
            if let Some(color) = step(style.bg) {
                span.style = span.style.bg(color);
            }
        }
    }
}

/// One color stepped toward the canvas by `numerator / denominator` of
/// the distance.
///
/// Truecolor steps in place; a palette index decodes, steps toward the
/// palette's floor beneath the canvas, and snaps back to the nearest
/// fixed entry; anything else — a color the dissolve cannot speak —
/// stands exactly as it was.
fn toward_canvas(
    color: Option<Color>,
    canvas: (u8, u8, u8),
    floor: (u8, u8, u8),
    numerator: u32,
    denominator: u32,
) -> Option<Color> {
    let color = color?;
    Some(match color {
        Color::Rgb(red, green, blue) => Color::Rgb(
            blend_channel(red, canvas.0, numerator, denominator),
            blend_channel(green, canvas.1, numerator, denominator),
            blend_channel(blue, canvas.2, numerator, denominator),
        ),
        Color::Indexed(entry) => {
            let (red, green, blue) = palette_triple(entry);
            let stepped = (
                blend_channel(red, floor.0, numerator, denominator),
                blend_channel(green, floor.1, numerator, denominator),
                blend_channel(blue, floor.2, numerator, denominator),
            );
            Color::Indexed(nearest_palette_entry(stepped))
        }
        other => other,
    })
}

/// One channel stepped toward the canvas, in saturation-safe integers.
///
/// The dissolve's whole arithmetic in miniature, written the way the
/// workspace's lint policy demands: the byte widens losslessly
/// ([`u32::from`]), the distance is an absolute difference (either
/// direction of the blend is one code path), the fraction of it that
/// applies multiplies and divides through the checked and saturating
/// operations — a zero denominator steps nothing, an oversized
/// numerator saturates — and the result narrows back through
/// [`u8::try_from`] with the original byte as its own fallback, never
/// an `as` cast that could truncate. A `numerator` of zero leaves the
/// channel exactly where it stood; the full `denominator` lands it on
/// the canvas.
fn blend_channel(value: u8, base: u8, numerator: u32, denominator: u32) -> u8 {
    let widened = u32::from(value);
    let base = u32::from(base);
    let gap = base.abs_diff(widened);
    let step = gap
        .saturating_mul(numerator)
        .checked_div(denominator)
        .unwrap_or(0);
    let blended = if base > widened {
        widened.saturating_add(step)
    } else {
        widened.saturating_sub(step)
    };
    u8::try_from(blended).unwrap_or(value)
}

/// The fixed palette entry whose triple sits nearest `target`.
///
/// The dissolve's palette arm must speak an index the terminal paints
/// verbatim, so a blended color snaps to the nearest entry rather than
/// falling back to truecolor — which tmux would re-encode and mangle,
/// the exact fate the palette rendition exists to avoid. Distance is
/// squared Euclidean over the decoded triples; a tie names the lower
/// entry, so the choice is deterministic.
fn nearest_palette_entry(target: (u8, u8, u8)) -> u8 {
    let channel = |a: u8, b: u8| -> u32 {
        let gap = u32::from(a).abs_diff(u32::from(b));
        gap.saturating_mul(gap)
    };
    let distance = |entry: u8| -> u32 {
        let (red, green, blue) = palette_triple(entry);
        channel(red, target.0)
            .saturating_add(channel(green, target.1))
            .saturating_add(channel(blue, target.2))
    };
    let mut best = PALETTE_FIRST;
    let mut best_distance = distance(best);
    for entry in PALETTE_FIRST.saturating_add(1)..=u8::MAX {
        let candidate = distance(entry);
        if candidate < best_distance {
            best = entry;
            best_distance = candidate;
        }
    }
    best
}

/// The fixed palette's floor beneath the canvas.
///
/// The target the dissolve's palette arm walks toward: the nearest
/// entry that does not sit lighter than the canvas, or the darkest
/// entry when every one does. The ramp's gray steps straddle any
/// canvas between them — the default theme's `(14, 14, 14)` between
/// the ramp's 8 and 18 — so the nearest entry outright can be a step
/// *above* the canvas, and a fade snapped onto it paints a rim
/// lighter than the page it dissolves into. The floor keeps the walk
/// darkward; the unstyled boundary column — not any entry — is what
/// meets the canvas itself.
fn palette_floor(canvas: (u8, u8, u8)) -> (u8, u8, u8) {
    let weight = |triple: (u8, u8, u8)| -> u32 {
        u32::from(triple.0)
            .saturating_add(u32::from(triple.1))
            .saturating_add(u32::from(triple.2))
    };
    let canvas_weight = weight(canvas);
    let distance = |triple: (u8, u8, u8)| -> u32 {
        let channel = |a: u8, b: u8| -> u32 {
            let gap = u32::from(a).abs_diff(u32::from(b));
            gap.saturating_mul(gap)
        };
        channel(triple.0, canvas.0)
            .saturating_add(channel(triple.1, canvas.1))
            .saturating_add(channel(triple.2, canvas.2))
    };
    let mut best: Option<(u8, u8, u8)> = None;
    for entry in PALETTE_FIRST..=u8::MAX {
        let triple = palette_triple(entry);
        if weight(triple) > canvas_weight {
            continue;
        }
        if best.is_none_or(|current| distance(triple) < distance(current)) {
            best = Some(triple);
        }
    }
    best.unwrap_or_else(|| palette_triple(PALETTE_FIRST))
}

/// The sRGB triple one fixed palette entry paints.
///
/// The cube's and the ramp's own decode — the inverse of
/// [`palette_entry`] — in one place, shared by the dissolve and the
/// round-trip pins.
fn palette_triple(entry: u8) -> (u8, u8, u8) {
    let index = usize::from(entry);
    if index < 232 {
        let cube = index.saturating_sub(usize::from(PALETTE_FIRST));
        let pick = |slot: usize| -> u8 { CUBE_STEPS.get(slot % 6).copied().unwrap_or(u8::MAX) };
        (pick(cube / 36), pick(cube / 6), pick(cube))
    } else {
        let step = u32::try_from(index.saturating_sub(232))
            .unwrap_or(u32::MAX)
            .saturating_mul(10);
        let level = u8::try_from(step.saturating_add(8)).unwrap_or(u8::MAX);
        (level, level, level)
    }
}

/// The truecolor rendition's rungs, decoded from their embedded
/// assets.
///
/// The original art — every terminal that takes direct-color escapes
/// paints it exactly — ordered largest first as the picker walks.
/// Built as its own step, not inline in [`scales`], so a test can
/// decode every rung of the set independent of the process's cached
/// truecolor verdict, which picks one rendition per process and
/// would leave the other arm unreachable in-process.
fn truecolor_scales() -> Vec<SplashArt> {
    vec![
        art(109, 28, include_str!("fjord_splash_w109.txt"), false),
        art(102, 27, include_str!("fjord_splash_w102.txt"), false),
        art(95, 25, include_str!("fjord_splash_w95.txt"), false),
        art(89, 23, include_str!("fjord_splash_w89.txt"), false),
        art(82, 21, include_str!("fjord_splash_w82.txt"), false),
        art(75, 20, include_str!("fjord_splash_w75.txt"), false),
        art(68, 18, include_str!("fjord_splash_w68.txt"), false),
        art(61, 16, include_str!("fjord_splash_w61.txt"), false),
        art(54, 14, include_str!("fjord_splash_w54.txt"), false),
        art(48, 12, include_str!("fjord_splash_w48.txt"), false),
        art(41, 11, include_str!("fjord_splash_w41.txt"), false),
        art(34, 9, include_str!("fjord_splash_w34.txt"), false),
    ]
}

/// The palette rendition's rungs, decoded from their embedded
/// assets.
///
/// The same photograph re-derived for the fixed 256-color vocabulary,
/// one rung per truecolor rung and on the same grid, ordered largest
/// first. Standing beside [`truecolor_scales`] as its own step keeps
/// the whole set decodable by tests regardless of which rendition
/// the process's one-time verdict chose.
fn palette_scales() -> Vec<SplashArt> {
    vec![
        art(109, 28, include_str!("fjord_palette_w109.txt"), true),
        art(102, 27, include_str!("fjord_palette_w102.txt"), true),
        art(95, 25, include_str!("fjord_palette_w95.txt"), true),
        art(89, 23, include_str!("fjord_palette_w89.txt"), true),
        art(82, 21, include_str!("fjord_palette_w82.txt"), true),
        art(75, 20, include_str!("fjord_palette_w75.txt"), true),
        art(68, 18, include_str!("fjord_palette_w68.txt"), true),
        art(61, 16, include_str!("fjord_palette_w61.txt"), true),
        art(54, 14, include_str!("fjord_palette_w54.txt"), true),
        art(48, 12, include_str!("fjord_palette_w48.txt"), true),
        art(41, 11, include_str!("fjord_palette_w41.txt"), true),
        art(34, 9, include_str!("fjord_palette_w34.txt"), true),
    ]
}

/// Decode one asset into its scale entry.
///
/// Every row carries one [`Span`] per cell — the cell's glyph painted
/// in its two colors — and a row whose character count strays from the
/// declared grid, a malformed hex pair, or a missing row empties that
/// scale's lines: the asset is fixed at build time, a shape error is a
/// development artifact, and the honest rendering of a broken scale is
/// none at all, leaving the smaller scales to serve.
///
/// `palette` picks the color vocabulary: `true` speaks in the fixed
/// palette's indexes — a value the palette does not carry still falls
/// back to truecolor, keeping a hand-edited asset honest — and `false`
/// keeps the embedded truecolor pairs exactly as authored.
fn art(width: u16, height: u16, asset: &str, palette: bool) -> SplashArt {
    let row_chars = usize::from(width).saturating_mul(CELL_CHARS);
    let mut rows = Vec::new();
    for raw in asset.lines() {
        let chars: Vec<char> = raw.chars().collect();
        if chars.len() != row_chars {
            return blank(width, height);
        }
        let mut spans = Vec::new();
        for cell in chars.chunks(CELL_CHARS) {
            let (hex, glyph) = cell.split_at(CELL_HEX);
            let Some((foreground, background)) = cell_colors(hex, palette) else {
                return blank(width, height);
            };
            let Some(glyph) = glyph.first() else {
                return blank(width, height);
            };
            spans.push(Span::styled(
                glyph.to_string(),
                Style::default().fg(foreground).bg(background),
            ));
        }
        rows.push(Line::from(spans));
    }
    if rows.len() == usize::from(height) {
        SplashArt {
            width,
            height,
            lines: rows,
        }
    } else {
        blank(width, height)
    }
}

/// An entry whose scale renders nothing.
///
/// The degraded shape of a malformed asset: the grid stays declared —
/// the entry keeps its place in the list — while its empty lines make
/// every picker pass over it for the next smaller scale.
fn blank(width: u16, height: u16) -> SplashArt {
    SplashArt {
        width,
        height,
        lines: Vec::new(),
    }
}

/// The embedded scales of this session's rendition, largest first.
///
/// Exposed for the welcome's own walk: the fit depends on the
/// terminal's reported cell aspect, which decides how many columns
/// each rung must yield, so the choosing belongs to the caller — this
/// list only orders the candidates.
pub(crate) fn rungs() -> &'static [SplashArt] {
    scales()
}

/// The two style colors one encoded cell carries.
///
/// The first six hex digits are the glyph's ink and the last six its
/// ground; any pair that is not two hex digits yields `None`, which
/// the decode treats as a malformed asset. In the palette vocabulary
/// a color some entry carries becomes that entry's index; anything
/// else — and everything, when the vocabulary is truecolor — stays
/// [`Color::Rgb`].
fn cell_colors(hex: &[char], palette: bool) -> Option<(Color, Color)> {
    let text: String = hex.iter().collect();
    let channel = |pair: &[u8]| -> Option<u8> {
        let digits = std::str::from_utf8(pair).ok()?;
        u8::from_str_radix(digits, 16).ok()
    };
    let mut channels = [0u8; 6];
    for (slot, pair) in channels.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        *slot = channel(pair)?;
    }
    let [red, green, blue, ground_red, ground_green, ground_blue] = channels;
    let speak = |channels: [u8; 3]| -> Color {
        if palette {
            palette_or_truecolor(channels)
        } else {
            let [red, green, blue] = channels;
            Color::Rgb(red, green, blue)
        }
    };
    Some((
        speak([red, green, blue]),
        speak([ground_red, ground_green, ground_blue]),
    ))
}

/// The 256-color palette's fixed range, as this module speaks it.
///
/// Entries below sixteen are the terminal's own ANSI colors — user
/// config, not a fixed vocabulary — so the palette rendition never
/// uses them and speaks only in the cube and grayscale runs every
/// 256-color terminal paints the same.
const PALETTE_FIRST: u8 = 16;

/// The palette's channel ladder: six cube levels per channel.
///
/// Every 256-color terminal builds its cube from these same six
/// levels per channel, so a color whose channels all sit exactly on
/// the ladder names exactly one cube entry — and the quantizer's work
/// is the colors that do not.
const CUBE_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// A color as the palette knows it, or as truecolor when it does not.
///
/// A value some entry carries becomes that entry's index, and a color
/// no entry carries stays [`Color::Rgb`] — the honest spelling for a
/// value the fixed palette cannot name.
fn palette_or_truecolor(channels: [u8; 3]) -> Color {
    let Some(entry) = palette_entry(channels) else {
        let [red, green, blue] = channels;
        return Color::Rgb(red, green, blue);
    };
    Color::Indexed(entry)
}

/// The palette entry carrying `channels`, if one does.
///
/// The cube and the ramp are each injective, so an exact value names
/// exactly one entry; `None` for anything the palette does not carry.
/// The grayscale check runs first — the ramp and the cube's gray
/// diagonal share only black, which both spellings agree on naming
/// sixteen.
fn palette_entry(channels: [u8; 3]) -> Option<u8> {
    let [red, green, blue] = channels;
    if red == green && green == blue {
        // The grayscale ramp: 8, 18, …, 238 — eight plus ten per step.
        let level = u32::from(red);
        if level == 0 {
            return Some(PALETTE_FIRST);
        }
        if (8..=238).contains(&level) && level.saturating_sub(8) % 10 == 0 {
            let entry = 232u32.saturating_add(level.saturating_sub(8) / 10);
            return u8::try_from(entry).ok();
        }
    }
    let step =
        |channel: u8| -> Option<usize> { CUBE_STEPS.iter().position(|level| *level == channel) };
    let red_step = step(red)?;
    let green_step = step(green)?;
    let blue_step = step(blue)?;
    let entry = usize::from(PALETTE_FIRST)
        .checked_add(red_step.checked_mul(36)?)?
        .checked_add(green_step.checked_mul(6)?)?
        .checked_add(blue_step)?;
    if entry < 232 {
        u8::try_from(entry).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One palette entry's sRGB triple, as the pins below spell it.
    fn palette_rgb(entry: u8) -> [u8; 3] {
        let (red, green, blue) = palette_triple(entry);
        [red, green, blue]
    }

    #[test]
    fn every_palette_entry_names_itself_back() {
        for entry in PALETTE_FIRST..=255u8 {
            assert_eq!(
                palette_entry(palette_rgb(entry)),
                Some(entry),
                "entry {entry} round-trips"
            );
        }
    }

    #[test]
    fn a_color_the_palette_lacks_stays_truecolor() {
        assert_eq!(palette_entry([1, 2, 3]), None);
        assert_eq!(palette_entry([10, 10, 10]), None, "off the ramp");
        assert_eq!(palette_entry([96, 0, 0]), None, "between cube steps");
        assert_eq!(palette_or_truecolor([1, 2, 3]), Color::Rgb(1, 2, 3));
        assert_eq!(palette_or_truecolor([0, 0, 95]), Color::Indexed(17));
    }

    #[test]
    fn both_renditions_decode_in_their_own_vocabularies() {
        let truecolor = art(34, 9, include_str!("fjord_splash_w34.txt"), false);
        let palette = art(34, 9, include_str!("fjord_palette_w34.txt"), true);
        assert!(!truecolor.lines.is_empty(), "the truecolor rung decodes");
        assert!(!palette.lines.is_empty(), "the palette rung decodes");
        assert!(
            truecolor
                .lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| matches!(span.style.fg, Some(Color::Rgb(..)))),
            "the truecolor rendition paints truecolor pairs"
        );
        for line in &palette.lines {
            for span in &line.spans {
                assert!(
                    matches!(span.style.fg, Some(Color::Indexed(n)) if (16..=255).contains(&n)),
                    "palette fg is a fixed palette index: {:?}",
                    span.style.fg
                );
                assert!(
                    matches!(span.style.bg, Some(Color::Indexed(n)) if (16..=255).contains(&n)),
                    "palette bg is a fixed palette index: {:?}",
                    span.style.bg
                );
            }
        }
    }

    #[test]
    fn the_palette_set_decodes_whole_in_the_smallest_rung() {
        let palette = art(34, 9, include_str!("fjord_palette_w34.txt"), true);
        assert_eq!(palette.lines.len(), 9, "nine rows in the smallest rung");
        assert_eq!(palette.width, 34, "the declared width stands");
    }

    #[test]
    fn every_rung_of_both_renditions_decodes_at_its_declared_grid() {
        // The decode blanks a malformed asset by design — a graceful
        // degradation for a development artifact — so the only honest
        // pin walks the whole embedded set: all twelve rungs of each
        // rendition, both grids, every row count against the declared
        // height. A truncated or malformed asset anywhere in the
        // ladder fails here at build-test time instead of blanking a
        // rung silently in production.
        for rendition in [truecolor_scales(), palette_scales()] {
            assert_eq!(rendition.len(), 12, "twelve rungs per rendition");
            for rung in &rendition {
                assert_eq!(
                    rung.lines.len(),
                    usize::from(rung.height),
                    "the rung {} cells wide decodes every declared row",
                    rung.width
                );
                assert!(
                    rung.lines
                        .iter()
                        .all(|line| line.spans.len() == usize::from(rung.width)),
                    "every decoded row paints one span per declared cell"
                );
            }
        }
    }

    #[test]
    fn the_dissolve_lands_the_outermost_column_on_the_canvas() {
        // The seam the dissolve exists to kill: the art's outermost
        // column becomes exactly the canvas (ink and ground both, so
        // no glyph specks it), the columns past the reach keep their
        // authored colors untouched, and the fade between them is a
        // real gradient — the second column steps toward the canvas
        // without landing on it.
        let art = art(34, 9, include_str!("fjord_splash_w34.txt"), false);
        let canvas = Color::Rgb(14, 14, 14);
        let before = art.lines.first().map_or_default(|line| line.spans.clone());
        let mut faded = art.lines.clone();
        dissolve_edges(&mut faded, canvas);
        let row = faded.first().map_or_default(|line| line.spans.clone());
        let outer = row
            .first()
            .map(|span| (span.content.as_ref(), span.style.fg, span.style.bg));
        assert_eq!(
            outer,
            Some((" ", None, None)),
            "the outermost column is an unstyled space — the terminal's own default, the canvas, with no glyph dots on it"
        );
        let second = row.get(1).map(|span| span.style.bg);
        assert_ne!(second, Some(None), "the second column keeps its own color");
        assert_ne!(
            second,
            before.first().map(|span| span.style.bg),
            "and it moved"
        );
        for column in [8usize, 17, 25] {
            assert_eq!(
                row.get(column).map(|span| span.style.bg),
                before.get(column).map(|span| span.style.bg),
                "the picture past the reach stands exactly as authored"
            );
        }
    }

    #[test]
    fn a_canvas_the_theme_cannot_name_dissolves_nothing() {
        // Reset is the transparent and deferred sessions: the
        // terminal's own background is the user's choice, and the
        // photograph keeps the margins it was authored with.
        let art = art(34, 9, include_str!("fjord_splash_w34.txt"), false);
        let mut faded = art.lines.clone();
        dissolve_edges(&mut faded, Color::Reset);
        assert_eq!(
            faded, art.lines,
            "a Reset canvas leaves every span exactly as decoded"
        );
    }

    #[test]
    fn the_palette_rendition_dissolves_within_the_palette() {
        // The palette arm must speak indexes tmux passes verbatim: a
        // blended color snaps to the nearest fixed entry, never to a
        // truecolor escape that would come back re-encoded and
        // mangled — and the boundary column goes unstyled instead, the
        // one spelling whose pixels are the canvas itself, which no
        // entry can name.
        let art = art(34, 9, include_str!("fjord_palette_w34.txt"), true);
        let mut faded = art.lines.clone();
        dissolve_edges(&mut faded, Color::Rgb(14, 14, 14));
        let last = faded
            .first()
            .map_or(0, |line| line.spans.len())
            .saturating_sub(1);
        for (row, line) in faded.iter().enumerate() {
            for (column, span) in line.spans.iter().enumerate() {
                if column == 0 || column == last {
                    assert_eq!(
                        (span.content.as_ref(), span.style.fg, span.style.bg),
                        (" ", None, None),
                        "row {row}: the boundary column is an unstyled space — the canvas default, no glyph dots"
                    );
                    continue;
                }
                for color in [span.style.fg, span.style.bg] {
                    assert!(
                        matches!(color, Some(Color::Indexed(n)) if (16..=255).contains(&n)),
                        "row {row} column {column}: the dissolve stays in the fixed palette: {color:?}"
                    );
                }
            }
        }
        assert_ne!(
            faded, art.lines,
            "and the dissolve actually moved the colors"
        );
    }

    #[test]
    fn the_palette_fade_never_rims_lighter_than_the_canvas() {
        // The tmux symptom this pins away: the gray ramp's steps
        // straddle the canvas (the default theme's 14 sits between the
        // ramp's 8 and 18), so a fade that snaps onto the nearer step
        // above paints a rim lighter than the page it dissolves into.
        // The boundary is unstyled now, and every faded palette column
        // walks darkward — its decoded weight never above the authored
        // color's — so no column of the fade is lighter than the art
        // it came from.
        let art = art(34, 9, include_str!("fjord_palette_w34.txt"), true);
        let canvas = (14_u8, 14_u8, 14_u8);
        let mut faded = art.lines.clone();
        dissolve_edges(&mut faded, Color::Rgb(14, 14, 14));
        let weight = |triple: (u8, u8, u8)| -> u32 {
            u32::from(triple.0)
                .saturating_add(u32::from(triple.1))
                .saturating_add(u32::from(triple.2))
        };
        let ground = |span: &Span| -> (u8, u8, u8) {
            match span.style.bg {
                Some(Color::Indexed(entry)) => palette_triple(entry),
                Some(Color::Rgb(red, green, blue)) => (red, green, blue),
                _ => canvas,
            }
        };
        for (row, (authored, dissolved)) in art.lines.iter().zip(&faded).enumerate() {
            let width = authored.spans.len();
            for column in 1..width.saturating_sub(1) {
                let edge_distance = column.min(width.saturating_sub(column.saturating_add(1)));
                if edge_distance == 0 || edge_distance >= EDGE_DISSOLVE_COLUMNS {
                    continue;
                }
                let Some(before) = authored.spans.get(column) else {
                    continue;
                };
                let Some(after) = dissolved.spans.get(column) else {
                    continue;
                };
                assert!(
                    weight(ground(after)) <= weight(ground(before)),
                    "row {row} column {column}: the fade walks darkward, never lighter"
                );
            }
        }
    }

    #[test]
    fn the_palette_floor_sits_at_or_below_the_canvas() {
        // The floor the palette arm walks toward: the nearest entry
        // the canvas is not darker than. The default theme's neutral
        // 14 lands on the ramp's 8 — never the 18 above it — and a
        // canvas the whole palette sits under falls to black.
        assert_eq!(
            palette_floor((14, 14, 14)),
            (8, 8, 8),
            "the ramp's 8 is the nearest entry the canvas outweighs"
        );
        assert_eq!(
            palette_floor((240, 240, 240)),
            (238, 238, 238),
            "a light canvas still finds its floor beneath"
        );
        assert_eq!(
            palette_floor((0, 0, 0)),
            (0, 0, 0),
            "a black canvas floors on black itself"
        );
    }

    #[test]
    fn the_nearest_palette_entry_names_an_exact_triple_itself() {
        // The decode is the search's own yardstick: every entry's
        // triple snaps back to the entry, and an off-ladder gray
        // snaps to the ramp rung beside it.
        for entry in [16u8, 17, 59, 231, 232, 245, 255] {
            let (red, green, blue) = palette_triple(entry);
            assert_eq!(
                nearest_palette_entry((red, green, blue)),
                entry,
                "entry {entry}'s own triple is its nearest"
            );
        }
        assert_eq!(
            nearest_palette_entry((14, 14, 14)),
            233,
            "a neutral gray between the ramp's 8 and 18 snaps to the nearer 18"
        );
    }
}
