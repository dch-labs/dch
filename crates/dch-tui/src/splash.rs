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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    /// One palette entry's sRGB triple, as the pins below spell it.
    fn palette_rgb(entry: u8) -> [u8; 3] {
        let index = usize::from(entry);
        if index < 232 {
            let cube = index.saturating_sub(usize::from(PALETTE_FIRST));
            let red = CUBE_STEPS.get(cube / 36).copied().unwrap_or(0);
            let green = CUBE_STEPS.get((cube / 6) % 6).copied().unwrap_or(0);
            let blue = CUBE_STEPS.get(cube % 6).copied().unwrap_or(0);
            [red, green, blue]
        } else {
            let step = u32::try_from(index.saturating_sub(232))
                .unwrap_or(u32::MAX)
                .saturating_mul(10);
            let level = u8::try_from(step.saturating_add(8)).unwrap_or(u8::MAX);
            [level, level, level]
        }
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
}
