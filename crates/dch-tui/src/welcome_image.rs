//! The welcome's real-image layer: the photograph itself, placed by
//! graphics-capable terminals.
//!
//! Where the terminal speaks the kitty graphics protocol — kitty,
//! `Ghostty` and `WezTerm` among them — the welcome shows a rendition
//! of the actual photograph: cropped to the band's own aspect and
//! resampled for its size, so the terminal's final scale is a gentle
//! one and no heavy downscale ever reaches its fast path. Every other
//! terminal sees the cell art alone,
//! unchanged: this layer is strictly additive, transmitted only after
//! the terminal answers the graphics query, and the cell art stays
//! painted underneath it as the base layer, so a terminal that
//! answers and then fails to draw degrades to exactly the approved
//! image rather than to a blank band.
//!
//! The commands are pure byte-string builders, testable without a
//! terminal; the run loop owns the single write point and the
//! lifecycle — transmit once, place when the band's seat changes (the
//! origin rides the seat, and a moved band deletes its stale
//! placement before the new one lands), delete when the welcome ends.

use std::sync::OnceLock;

/// The photograph, as shipped.
///
/// The original PNG bytes, embedded whole: the source every
/// [`rendition`] is resampled from, and the standing fallback should
/// a rendition ever fail to build — the terminal then scales the
/// original itself, exactly as it always did.
pub(crate) const PHOTO: &[u8] = include_bytes!("fjord_photo.png");

/// The assumed cell — twice as tall as wide — for terminals that
/// never name their own, the rendition's oversampling factor, and the
/// canonical crop's share of the photograph's height.
///
/// A terminal that answers the cell-size asks sizes the rendition to
/// its true cell instead; this is the silent-terminal footing. The
/// geometry is tripled — exact on a hi-dpi display, and even a cell
/// answer missed between zoom steps leaves the terminal only a mild
/// downscale. The crop is fixed so every size renders the same
/// picture: full width, a centered strip of the height, framed once.
const CELL_PIXELS_WIDE: u16 = 9;
const CELL_PIXELS_TALL: u16 = 18;
const RENDITION_OVERSAMPLE: u16 = 3;
const CANONICAL_CROP_PER_HUNDRED: u32 = 78;

/// The decoded photograph, decoded once for the process.
///
/// The decode runs on the first rendition that needs it and its
/// result is shared from then on; a file that fails to decode parks
/// a 1×1 image every rendition refuses on sight, so the caller's
/// fallback — the shipped bytes — governs, and a broken file costs
/// one failed decode, not one per frame.
static PHOTO_IMAGE: OnceLock<image::RgbImage> = OnceLock::new();

/// The photograph rendered for a band of `columns × rows` cells.
///
/// The terminal that places an image scales whatever it was given
/// into the cells it was told — its own way, quality be damned. This
/// builds what the band actually wants: the photograph framed once —
/// one fixed crop, the same picture at every size — Lanczos-resampled
/// in linear light to thrice the band's pixel size, so the detail the
/// shrink gives up is chosen by a proper filter, not a fast one, and
/// the terminal's remaining scale barely touches it. `None` refuses
/// the impossible — no cells, or a photograph that failed to decode —
/// and the caller falls back to
/// the shipped bytes.
pub(crate) fn rendition(columns: u16, rows: u16) -> Option<Vec<u8>> {
    let photo = PHOTO_IMAGE.get_or_init(|| {
        image::load_from_memory(PHOTO)
            .map_or_else(|_| image::RgbImage::new(1, 1), |dynamic| dynamic.to_rgb8())
    });
    let source_w = photo.width();
    let source_h = photo.height();
    if columns == 0 || rows == 0 || source_w < 2 || source_h < 2 {
        return None;
    }
    let (cell_wide, cell_tall) =
        crate::terminal::cell_pixels().unwrap_or((CELL_PIXELS_WIDE, CELL_PIXELS_TALL));
    let target_w = u32::from(columns)
        .saturating_mul(u32::from(cell_wide))
        .saturating_mul(u32::from(RENDITION_OVERSAMPLE));
    let target_h = u32::from(rows)
        .saturating_mul(u32::from(cell_tall))
        .saturating_mul(u32::from(RENDITION_OVERSAMPLE));
    let crop_w = source_w;
    let crop_h = source_h
        .saturating_mul(CANONICAL_CROP_PER_HUNDRED)
        .checked_div(100)
        .unwrap_or(source_h)
        .clamp(2, source_h);
    let target_w = target_w.min(crop_w);
    let target_h = target_h.min(crop_h);
    let x = source_w.saturating_sub(crop_w) / 2;
    let y = source_h.saturating_sub(crop_h) / 2;
    let cropped = image::imageops::crop_imm(photo, x, y, crop_w, crop_h).to_image();
    let mut resized = resize_in_linear(&cropped, target_w, target_h);
    sharpen(&mut resized);
    deepen_sky(&mut resized);
    lift_stars(&mut resized);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(resized)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// Resample the crop in linear light, not in the file's gamma.
///
/// The file's bytes carry the display curve: averaging them averages
/// the curve, not the light, and every resampled edge lands darker
/// and duller than the scene — most visibly in a night sky, whose
/// smooth gradient an sRGB average plugs into bands. Converting to
/// linear light, resampling there — the same Lanczos — and encoding
/// back with the curve's inverse keeps the light's own averages. The
/// cost is one float pass per rendition, paid once per size.
fn resize_in_linear(cropped: &image::RgbImage, target_w: u32, target_h: u32) -> image::RgbImage {
    let mut linear = image::Rgb32FImage::new(cropped.width(), cropped.height());
    for (to, from) in linear.pixels_mut().zip(cropped.pixels()) {
        for (slot, channel) in to.0.iter_mut().zip(from.0.iter()) {
            *slot = srgb_to_linear(f32::from(*channel) / 255.0);
        }
    }
    let sampled = image::imageops::resize(
        &linear,
        target_w,
        target_h,
        image::imageops::FilterType::Lanczos3,
    );
    let mut out = image::RgbImage::new(target_w, target_h);
    for (to, from) in out.pixels_mut().zip(sampled.pixels()) {
        for (slot, channel) in to.0.iter_mut().zip(from.0.iter()) {
            let display = linear_to_srgb(channel.clamp(0.0, 1.0)) * 255.0;
            *slot = quantize_byte(display);
        }
    }
    out
}

/// The byte ladder, as floats — the cast-free quantize's search space.
///
/// Built once from `u8` values widened losslessly; every entry equals
/// its index, so a search over the ladder names the byte a rounded
/// display value means.
static BYTE_LADDER: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();

/// Quantize a display value into its byte — the pipeline's one float
/// to byte step.
///
/// Stable Rust has no checked float-to-integer conversion (`TryFrom`
/// covers only integer pairs), and a raw `as` cast is the one
/// conversion class this codebase does not write. The value is
/// clamped to the byte range and rounded; the ladder of byte values —
/// widened losslessly with `From<u8>` — is then searched for the rung
/// the rounded value names, and the index is narrowed back with
/// `TryInto`, the same checked narrowing the rest of the workspace
/// uses. Out-of-range input saturates rather than truncates.
fn quantize_byte(display: f32) -> u8 {
    let ladder = BYTE_LADDER.get_or_init(|| (0..=u8::MAX).map(f32::from).collect());
    ladder
        .partition_point(|level| *level <= display.clamp(0.0, 255.0) - 0.5)
        .try_into()
        .unwrap_or(u8::MAX)
}

/// The sRGB display curve's own decode: an encoded byte into linear
/// light.
///
/// The piecewise form the standard defines — a straight toe below
/// `0.04045`, where a pure power curve would ask near-black for a
/// slope it cannot keep, and the 2.4 power above it. The resample's
/// averages are taken in this space ([`resize_in_linear`]), never in
/// the encoded one the file stores.
fn srgb_to_linear(display: f32) -> f32 {
    if display <= 0.04045 {
        display / 12.92
    } else {
        ((display + 0.055) / 1.055).powf(2.4)
    }
}

/// The curve's inverse: linear light back to an encoded value.
///
/// The exact inverse of [`srgb_to_linear`] — the same break at the
/// same point, the 1/2.4 power above it — so a pixel that travels
/// there and back returns to where it started, and the resample's
/// round trip costs no color. The byte the encoded value means is
/// settled by [`quantize_byte`].
fn linear_to_srgb(light: f32) -> f32 {
    if light <= 0.003_130_8 {
        light * 12.92
    } else {
        1.055 * light.powf(1.0 / 2.4) - 0.055
    }
}

/// The unsharp pass's reach and strength.
///
/// The sigma spans about one rendition pixel — half a pixel on the
/// display, since the rendition carries hi-dpi headroom — so the pass
/// sharpens only what the resample softened. The strength is a
/// fraction in integers, three tenths: a touch, not a halo.
const SHARPEN_SIGMA: f32 = 1.0;
const SHARPEN_NUMERATOR: i32 = 3;
const SHARPEN_DENOMINATOR: i32 = 10;

/// The floor under the pass: detail quieter than this is noise's
/// whisper, not an edge, and amplifying it would grain the sky.
///
/// A flat cut, not a ramp — detail under the floor boosts nothing
/// and the pixel stands exactly — which is what keeps a flat field
/// a fixed point of the pass instead of slowly graining upward.
const SHARPEN_FLOOR: i32 = 2;

/// Add back a touch of the detail the resample softened.
///
/// The classic unsharp mask in integer arithmetic: the difference
/// between each pixel and its gaussian-blurred self is its local
/// detail, and a fraction of that detail rides back onto the pixel —
/// edges steepen, the busy middle recovers its bite, and a flat field
/// is a fixed point, changing nothing at all.
fn sharpen(image: &mut image::RgbImage) {
    let soft = image::imageops::blur(image, SHARPEN_SIGMA);
    for (pixel, blurred) in image.pixels_mut().zip(soft.pixels()) {
        for channel in 0..3 {
            let (Some(original), Some(softened)) = (pixel.0.get(channel), blurred.0.get(channel))
            else {
                continue;
            };
            let original = i32::from(*original);
            let detail = original.saturating_sub(i32::from(*softened));
            let boost = if detail.abs() < SHARPEN_FLOOR {
                0
            } else {
                detail
                    .saturating_mul(SHARPEN_NUMERATOR)
                    .checked_div(SHARPEN_DENOMINATOR)
                    .unwrap_or(0)
            };
            let boosted = original.saturating_add(boost);
            if let Some(slot) = pixel.0.get_mut(channel)
                && let Some(value) = u8::try_from(boosted.clamp(0, 255)).ok()
            {
                *slot = value;
            }
        }
    }
}

/// The graduated night the sky wears, as a reach and three falls.
///
/// The top of the frame deepens — a touch darker, a touch bluer —
/// fading linearly to nothing under halfway: the night reads as deep
/// as the cell art beneath it. Red falls furthest and blue least, so
/// the deepening leans into the sky's own blue rather than washing
/// it gray.
const SKY_REACH_PER_HUNDRED: u32 = 45;
const SKY_FALL_RED: i32 = 12;
const SKY_FALL_GREEN: i32 = 8;
const SKY_FALL_BLUE: i32 = 4;

/// Deepen the sky toward the top of the frame.
///
/// A graduated filter in integer arithmetic: each row inside the
/// reach gives up a share of its light, the share largest at the very
/// top and fading to nothing at the reach's end, so the horizon and
/// everything below it stand untouched and no seam shows where the
/// graduation ends.
fn deepen_sky(image: &mut image::RgbImage) {
    let reach = image
        .height()
        .saturating_mul(SKY_REACH_PER_HUNDRED)
        .checked_div(100)
        .unwrap_or(0);
    if reach == 0 {
        return;
    }
    for (y, row) in image.rows_mut().enumerate() {
        let depth = reach.saturating_sub(u32::try_from(y).unwrap_or(0));
        if depth == 0 {
            break;
        }
        let fall = |top: i32| -> i32 {
            top.saturating_mul(i32::try_from(depth).unwrap_or(0))
                .checked_div(i32::try_from(reach).unwrap_or(1))
                .unwrap_or(0)
        };
        let (red_fall, green_fall, blue_fall) = (
            fall(SKY_FALL_RED),
            fall(SKY_FALL_GREEN),
            fall(SKY_FALL_BLUE),
        );
        for pixel in row {
            for (slot, fall) in pixel.0.iter_mut().zip([red_fall, green_fall, blue_fall]) {
                *slot = slot.saturating_sub(u8::try_from(fall).unwrap_or(0));
            }
        }
    }
}

/// The star pass's reach and grip.
///
/// A star is a small bright core against its own local sky: the
/// pass compares each sky pixel with its gaussian-blurred
/// surroundings and lifts only the pixels standing far above them —
/// real star cores, brightened by a fraction of their own height.
/// The threshold sits well above the sky's grain, and the wide
/// highlights of mountains and shore lights blur to themselves and
/// stand exactly.
const STAR_BACKGROUND_SIGMA: f32 = 1.6;
const STAR_THRESHOLD: i32 = 24;
const STAR_GAIN_NUMERATOR: i32 = 4;
const STAR_GAIN_DENOMINATOR: i32 = 10;

/// Lift the sky's stars out of the night.
///
/// Runs over the sky's rows only — the same reach the graduation
/// darkens — after the darkening, so a core's height is measured
/// against the night it actually stands in and the lift is not spent
/// on pixels the graduation is about to dim. Everything except a
/// core stands exactly where it was.
fn lift_stars(image: &mut image::RgbImage) {
    let reach = image
        .height()
        .saturating_mul(SKY_REACH_PER_HUNDRED)
        .checked_div(100)
        .unwrap_or(0);
    if reach == 0 {
        return;
    }
    let soft = image::imageops::blur(image, STAR_BACKGROUND_SIGMA);
    let reach = usize::try_from(reach).unwrap_or(0);
    for (row, soft_row) in image.rows_mut().zip(soft.rows()).take(reach) {
        for (pixel, blurred) in row.zip(soft_row) {
            for (slot, background) in pixel.0.iter_mut().zip(blurred.0.iter()) {
                let core = i32::from(*slot);
                let height = core.saturating_sub(i32::from(*background));
                if height >= STAR_THRESHOLD {
                    let lift = height
                        .saturating_mul(STAR_GAIN_NUMERATOR)
                        .checked_div(STAR_GAIN_DENOMINATOR)
                        .unwrap_or(0);
                    let lifted = core.saturating_add(lift);
                    *slot = u8::try_from(lifted.clamp(0, 255)).unwrap_or(*slot);
                }
            }
        }
    }
}

/// The id the welcome's image is transmitted under.
///
/// A fixed, small number: one image lives at a time, and a fixed id
/// lets the delete command name it without bookkeeping.
const IMAGE_ID: u32 = 7;

/// A kitty-graphics command as raw bytes.
///
/// The caller owns the write; nothing here touches a terminal.
fn apc(payload: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len().saturating_add(8));
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// The transmission commands: the PNG, base64, in protocol-sized
/// chunks.
///
/// The first chunk carries `f=100` (PNG data), the image id, and
/// `m=1` (more follows); every continuation carries only `m` — and
/// `q=2` on the closing one — because the protocol permits nothing
/// else once the id is established, so the terminal composes the
/// image when the `m=0` chunk lands. Control data and payload are
/// divided by the protocol's `;`, never a comma, so the base64 text
/// is unambiguously the payload. Chunking is protocol-mandated — a
/// command is limited to 4096 bytes — so the encoder splits the
/// base64 text, never the image bytes.
pub(crate) fn transmit_commands(png: &[u8], chunk: usize) -> Vec<Vec<u8>> {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let step = chunk.saturating_sub(64).max(64);
    let mut commands = Vec::new();
    let mut first = true;
    let bytes = encoded.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let end = start.saturating_add(step).min(bytes.len());
        let last = end == bytes.len();
        let head = if first {
            format!("f=100,i={IMAGE_ID},m={};", u8::from(!last))
        } else if last {
            "m=0,q=2;".to_string()
        } else {
            "m=1;".to_string()
        };
        let payload: String = encoded
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect();
        commands.push(apc(&format!("{head}{payload}")));
        first = false;
        start = end;
    }
    commands
}

/// The placement command: the image at the cursor, sized to cells.
///
/// `C=1` places the image's top-left at the cursor and leaves the
/// cursor where it was — the placement moves no cursor, so the
/// terminal's real position stays where ratatui believes it is. The
/// caller positions the cursor first; the `c`/`r` pair sizes it to
/// whole cells — the same geometry the cell art's band occupies, so
/// the two layers register exactly. `q=2` silences the response; the
/// placement is fire-and-forget.
pub(crate) fn place_command(columns: u16, rows: u16) -> Vec<u8> {
    apc(&format!("a=p,i={IMAGE_ID},c={columns},r={rows},C=1,q=2"))
}

/// The placement-delete command: the anchors gone, the data kept.
///
/// Kitty's delete action with `d=i` removes every placement of the
/// named image while leaving its transmitted data in place — the
/// one by-id deletion the protocol offers; the emit path sends this
/// before re-placing a band whose seat moved, so the old anchor
/// never outlives the geometry it was built for.
pub(crate) fn delete_placements_command() -> Vec<u8> {
    apc(&format!("a=d,d=i,i={IMAGE_ID},q=2"))
}

/// The deletion command: the image gone, by its id.
///
/// Emitted when the welcome ends, so the floating photograph never
/// outlives the session-opening it belongs to; text drawn after it
/// would otherwise sit beneath a stale picture. The bytes are the
/// placement-delete's — the terminal's by-id deletion clears what is
/// visible, which is everything the welcome owed the image.
pub(crate) fn delete_command() -> Vec<u8> {
    delete_placements_command()
}

/// Cursor addressing, 1-based, as the placement's anchor.
///
/// The protocol's cell geometry is one-based rows and columns; the
/// band's zero-based rect converts here, keeping the arithmetic in
/// one tested place.
pub(crate) fn cursor_to(column: u16, row: u16) -> Vec<u8> {
    let column = column.saturating_add(1);
    let row = row.saturating_add(1);
    format!("\x1b[{row};{column}H").into_bytes()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
mod tests {
    use super::*;

    #[test]
    fn the_transmitted_payload_is_standard_base64() {
        // The wire speaks the standard alphabet: three known
        // bytes encode to their standard quad and ride the one
        // chunk a payload this small produces, head to tail.
        let commands = transmit_commands(&[0xAB, 0xCD, 0xEF], 4096);
        assert_eq!(commands.len(), 1, "a three-byte payload is one chunk");
        let command = String::from_utf8(commands.first().cloned().unwrap()).unwrap();
        assert!(
            command.starts_with("\x1b_Gf=100,i=7,m=0;q83v\x1b\\"),
            "the payload is the standard encoding of AB CD EF, divided from the control data by the protocol's semicolon: {command:?}"
        );
    }

    #[test]
    fn the_byte_ladder_rounds_and_saturates_like_the_cast_it_replaces() {
        // The quantize is the pipeline's old `round().clamp() as` step
        // recast without the cast: halves round away from zero the way
        // `f32::round` does, the endpoints land on themselves, and an
        // out-of-range value saturates instead of truncating.
        assert_eq!(quantize_byte(0.0), 0, "zero is zero");
        assert_eq!(quantize_byte(255.0), 255, "white is white");
        assert_eq!(quantize_byte(186.4), 186, "below the half rounds down");
        assert_eq!(quantize_byte(186.6), 187, "above the half rounds up");
        assert_eq!(
            quantize_byte(186.5),
            187,
            "the half itself rounds away from zero"
        );
        assert_eq!(quantize_byte(-3.0), 0, "below the range saturates");
        assert_eq!(quantize_byte(300.0), 255, "above the range saturates");
    }
    #[test]
    fn transmission_chunks_the_payload_and_ends_with_m0() {
        let png = vec![0xABu8; 512];
        let commands = transmit_commands(&png, 256);
        assert!(
            commands.len() > 1,
            "512 bytes of PNG base64-encode past one 256-byte chunk"
        );
        let first = String::from_utf8(commands.first().cloned().unwrap()).unwrap();
        assert!(
            first.contains("f=100,"),
            "the first chunk names the PNG format: {first:?}"
        );
        assert!(
            first.contains("m=1;"),
            "a non-final chunk announces more, semicolon-divided: {first:?}"
        );
        let last = String::from_utf8(commands.last().cloned().unwrap()).unwrap();
        assert!(
            last.starts_with("\x1b_Gm=0,q=2;"),
            "the closing chunk carries only m and q — the protocol permits nothing else on a continuation: {last:?}"
        );
        assert!(
            !last.contains("i="),
            "a continuation never repeats the image id: {last:?}"
        );
        for command in &commands {
            let text = String::from_utf8(command.clone()).unwrap();
            assert!(text.starts_with("\x1b_G") && text.ends_with("\x1b\\"));
        }
    }

    #[test]
    fn placement_sizes_to_cells_and_deletes_by_id() {
        let place = String::from_utf8(place_command(48, 11)).unwrap();
        assert!(place.contains("a=p,"), "the placement displays: {place:?}");
        assert!(place.contains("c=48"), "columns size the image: {place:?}");
        assert!(place.contains("r=11"), "rows size the image: {place:?}");
        let delete = String::from_utf8(delete_command()).unwrap();
        assert!(delete.contains("a=d,"), "the deletion removes: {delete:?}");
        assert!(
            delete.contains("i=7"),
            "the deletion names the image: {delete:?}"
        );
        let placements = String::from_utf8(delete_placements_command()).unwrap();
        assert!(
            placements.contains("a=d,d=i,"),
            "the placement delete is the protocol's delete action by id, not the invalid `a=D,d=p`: {placements:?}"
        );
        assert!(
            placements.contains("i=7"),
            "the placement delete names the image: {placements:?}"
        );
        assert!(
            placements.contains("q=2"),
            "the placement delete is fire-and-forget like its siblings: {placements:?}"
        );
        assert_eq!(
            placements, delete,
            "the terminal offers one by-id deletion — the two names spell the same bytes"
        );
    }

    #[test]
    fn cursor_addressing_is_one_based() {
        assert_eq!(cursor_to(0, 0), b"\x1b[1;1H".to_vec());
        assert_eq!(cursor_to(9, 20), b"\x1b[21;10H".to_vec());
    }

    #[test]
    fn every_size_renders_the_same_picture() {
        // Zoom may resize the picture; it may not reframe it. Two
        // bands, one photograph: the corners of both renditions read
        // the same source pixels, because the crop is fixed and only
        // the scale changes.
        let small = rendition(34, 9).expect("the small rendition");
        let large = rendition(61, 16).expect("the large rendition");
        let small = image::load_from_memory(&small)
            .expect("the small decodes")
            .to_rgb8();
        let large = image::load_from_memory(&large)
            .expect("the large decodes")
            .to_rgb8();
        let (corner_a, corner_b) = (small.get_pixel(0, 0).0, large.get_pixel(0, 0).0);
        for channel in 0..3 {
            let (x, y) = (i32::from(corner_a[channel]), i32::from(corner_b[channel]));
            assert!(
                (x - y).abs() <= 12,
                "channel {channel}: one picture, two scales — the corner drifted {x} vs {y}"
            );
        }
    }

    #[test]
    fn a_rendition_lands_on_the_band_s_oversampled_pixels() {
        // What the band wants, the band gets: the rendition arrives at
        // exactly the band's pixel geometry, oversampled for hi-dpi —
        // the same fixed crop every size renders, never a reframe.
        let png = rendition(34, 9).expect("the rendition encodes");
        let decoded = image::load_from_memory(&png).expect("the rendition decodes");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (
                u32::from(34 * CELL_PIXELS_WIDE * RENDITION_OVERSAMPLE),
                u32::from(9 * CELL_PIXELS_TALL * RENDITION_OVERSAMPLE)
            ),
            "the rendition lands on the band's oversampled pixel geometry"
        );
        assert!(rendition(0, 9).is_none(), "no band, no rendition");
    }

    #[test]
    fn linear_light_averages_keep_the_scenes_own_brightness() {
        // The signature of resampling in light instead of the display
        // curve: a half-black, half-white field averages to the
        // brighter linear mean — near 188, not sRGB-space's 127 — and
        // the curve's endpoints and midpoint decode exactly.
        assert_eq!(srgb_to_linear(0.0), 0.0, "black is black in both");
        assert_eq!(srgb_to_linear(1.0), 1.0, "white is white in both");
        assert!(
            (srgb_to_linear(0.5) - 0.2139).abs() < 0.001,
            "the curve's own midpoint decodes to linear 0.214"
        );
        assert!(
            (linear_to_srgb(srgb_to_linear(0.75)) - 0.75).abs() < 0.001,
            "the round trip lands where it left"
        );
        let mut field = image::RgbImage::new(64, 8);
        for y in 0..8u32 {
            for x in 0..64u32 {
                // The split lands mid-pixel on the 4-wide output —
                // pixel (1, 0) averages eight black and eight white.
                let shade = if x < 24 { 0 } else { 255 };
                field.put_pixel(x, y, image::Rgb([shade, shade, shade]));
            }
        }
        let resized = resize_in_linear(&field, 4, 1);
        let mixed = resized.get_pixel(1, 0).0[0];
        assert!(
            (150..225).contains(&mixed),
            "the linear mean reads brighter than sRGB's 127: {mixed}"
        );
    }

    #[test]
    fn a_whisper_of_noise_sharpens_to_nothing() {
        // Sub-floor detail is noise, not edge: a checkerboard one
        // level apart is the sky's grain, and the pass leaves it
        // alone rather than amplifying it.
        let mut image = image::RgbImage::from_fn(8, 8, |x, y| {
            let shade = if (x + y) % 2 == 0 { 100 } else { 101 };
            image::Rgb([shade, shade, shade])
        });
        sharpen(&mut image);
        assert!(
            image
                .pixels()
                .all(|pixel| pixel.0.iter().all(|&channel| (99..=102).contains(&channel))),
            "sub-floor detail is the noise's whisper — the pass leaves it be"
        );
    }

    #[test]
    fn a_flat_field_sharpens_to_itself() {
        // The pass touches detail only: where the blurred self equals
        // the pixel, the pixel stands — a flat picture sharpens to
        // itself, and the sky keeps its calm.
        let mut image = image::RgbImage::from_pixel(8, 8, image::Rgb([70, 90, 110]));
        sharpen(&mut image);
        assert!(
            image.pixels().all(|pixel| pixel.0 == [70, 90, 110]),
            "a flat field is a fixed point"
        );
    }

    #[test]
    fn the_stars_lift_clear_of_their_sky() {
        // A star is a small bright core over its own local sky: the
        // pass lifts only cores standing far above their blurred
        // surroundings — the flat sky and the interior of wide bright
        // areas stand exactly, and the core comes away brighter than
        // it went in.
        let mut image = image::RgbImage::from_pixel(24, 40, image::Rgb([10, 12, 20]));
        for y in 0..12u32 {
            for x in 0..24u32 {
                image.put_pixel(x, y, image::Rgb([200, 200, 200]));
            }
        }
        for y in 15..17u32 {
            for x in 10..12u32 {
                image.put_pixel(x, y, image::Rgb([160, 170, 190]));
            }
        }
        lift_stars(&mut image);
        assert_eq!(
            image.get_pixel(1, 6).0,
            [200, 200, 200],
            "a wide bright area's interior stands exactly"
        );
        assert_eq!(
            image.get_pixel(20, 30).0,
            [10, 12, 20],
            "the flat sky stands exactly"
        );
        let core = image.get_pixel(10, 15).0[1];
        assert!(core > 190, "the star core lifts past its input: {core}");
    }

    #[test]
    fn the_sky_deepens_toward_the_top_and_no_further() {
        // A graduated night: the very top gives up the most light and
        // leans blue doing it; below the reach the picture stands
        // exactly as it was.
        let mut image = image::RgbImage::from_pixel(4, 40, image::Rgb([120, 110, 100]));
        deepen_sky(&mut image);
        let top = image.get_pixel(0, 0).0;
        let bottom = image.get_pixel(0, 39).0;
        assert_eq!(bottom, [120, 110, 100], "below the reach, nothing changes");
        assert!(
            top[0] < 120 && top[1] < 110 && top[2] < 100,
            "the top darkens"
        );
        assert!(
            120 - u32::from(top[0]) > 100 - u32::from(top[2]),
            "red falls further than blue — the deepening leans blue"
        );
    }
}
