//! The activity strip's animation engine: the integer-clock pulse
//! styles that draw the running bar, one per strip posture.

use dch_config::StripStyle;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::time::Instant;

/// The pulse bar's breathing period, in milliseconds.
///
/// One full animation cycle per period — the centered breath's
/// grow-and-shrink or the sweep's fill-and-clear; the phase is a pure
/// function of elapsed time, so the bar animates smoothly at any
/// frame rate and renders identically in tests.
pub(crate) const PULSE_PERIOD_MS: u64 = 3600;

/// The breathing pulse bar as a fixed-width string with unstyled padding.
///
/// A thin centered run of `─` that grows linearly from the row's
/// center toward both edges and shrinks back over
/// [`PULSE_PERIOD_MS`] — a triangle breath, computed in pure integer
/// arithmetic and centered to the cell: when the padding splits odd,
/// the run sits half a cell right of center. The run never fully
/// disappears: its floor is one cell, so the strip is visible from
/// the run's first frame. Only the run carries the style; the
/// padding around it stays unstyled, so no terminal ever shows
/// styled spaces as a tinted band. The string is exactly `width`
/// characters, filling the span its caller reserves without
/// shifting anything around it.
pub(crate) fn pulse_centered(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let progress = if mark < half {
        mark
    } else {
        period.saturating_sub(mark)
    };
    let len = u64::try_from(width)
        .unwrap_or(0)
        .saturating_mul(progress)
        .checked_div(half)
        .unwrap_or(0);
    let len = usize::try_from(len).unwrap_or(0).min(width);
    let len = len.max(width.min(1));
    let start = width.saturating_sub(len).div_ceil(2);
    let tail = width.saturating_sub(start).saturating_sub(len);
    let mut pulse = String::with_capacity(width);
    for _ in 0..start {
        pulse.push(' ');
    }
    for _ in 0..len {
        pulse.push('─');
    }
    for _ in 0..tail {
        pulse.push(' ');
    }
    pulse
}

/// The sweeping pulse bar as a fixed-width string with unstyled padding.
///
/// The first half of [`PULSE_PERIOD_MS`] fills from the left edge —
/// a one-cell seed growing right until the row is full — and the
/// second half clears left-to-right with the right edge pinned at
/// the row's end, the cleared count rounded up so the row blanks for
/// an instant before the wrap restarts the seed. Like the breath,
/// the phase is a pure function of elapsed time and the string is
/// exactly `width` characters, its run carrying the only style.
pub(crate) fn pulse_sweep(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let mut pulse = String::with_capacity(width);
    if mark < half {
        let len = u64::try_from(width)
            .unwrap_or(0)
            .saturating_mul(mark)
            .checked_div(half)
            .unwrap_or(0);
        let len = usize::try_from(len)
            .unwrap_or(0)
            .min(width)
            .max(width.min(1));
        for _ in 0..len {
            pulse.push('─');
        }
        for _ in 0..width.saturating_sub(len) {
            pulse.push(' ');
        }
    } else {
        let cleared = u64::try_from(width)
            .unwrap_or(0)
            .saturating_mul(mark.saturating_sub(half))
            .div_ceil(half)
            .min(u64::try_from(width).unwrap_or(0));
        let cleared = usize::try_from(cleared).unwrap_or(0).min(width);
        for _ in 0..cleared {
            pulse.push(' ');
        }
        for _ in 0..width.saturating_sub(cleared) {
            pulse.push('─');
        }
    }
    pulse
}

/// The bouncing pulse bar as a fixed-width string with unstyled padding.
///
/// One [`PULSE_PERIOD_MS`] pass sweeps left-to-right — the sweep's
/// fill and clear — and the next sweeps back right-to-left with the
/// geometry mirrored: a right-anchored fill growing from a one-cell
/// seed and a left-pinned clear, the row blanking for an instant at
/// every turn before the next pass begins. Like the other styles,
/// the phase is a pure function of elapsed time and the string is
/// exactly `width` characters, its run carrying the only style.
pub(crate) fn pulse_bounce(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period.saturating_mul(2));
    let forward = mark < period;
    let pass = if forward {
        mark
    } else {
        mark.saturating_sub(period)
    };
    let span = u64::try_from(width).unwrap_or(0);
    let (len, left_pad) = if pass < half {
        let len = span
            .saturating_mul(pass)
            .checked_div(half)
            .unwrap_or(0)
            .min(span);
        let len = usize::try_from(len)
            .unwrap_or(0)
            .min(width)
            .max(width.min(1));
        let pad = if forward {
            0
        } else {
            width.saturating_sub(len)
        };
        (len, pad)
    } else {
        let cleared = span
            .saturating_mul(pass.saturating_sub(half))
            .div_ceil(half)
            .min(span);
        let cleared = usize::try_from(cleared).unwrap_or(0).min(width);
        let len = width.saturating_sub(cleared);
        let pad = if forward { cleared } else { 0 };
        (len, pad)
    };
    let mut pulse = String::with_capacity(width);
    for _ in 0..left_pad {
        pulse.push(' ');
    }
    for _ in 0..len {
        pulse.push('─');
    }
    for _ in 0..width.saturating_sub(left_pad).saturating_sub(len) {
        pulse.push(' ');
    }
    pulse
}

/// The traveling pulse bar as a fixed-width string with unstyled padding.
///
/// A segment a quarter of the row's width (floored at three cells,
/// capped at the row) glides from the left edge to the right over
/// one [`PULSE_PERIOD_MS`] pass and back over the next, the mirror
/// continuing from exactly where the forward pass ended — one
/// continuous scanner motion with the segment always on the row.
/// Like the other styles, the phase is a pure function of elapsed
/// time and the string is exactly `width` characters, its run
/// carrying the only style.
pub(crate) fn pulse_travel(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period.saturating_mul(2));
    let forward = mark < period;
    let pass = if forward {
        mark
    } else {
        mark.saturating_sub(period)
    };
    let len = (width / 4).max(3).min(width);
    let reach = width.saturating_sub(len);
    let pos = u64::try_from(reach)
        .unwrap_or(0)
        .saturating_mul(pass)
        .checked_div(period)
        .unwrap_or(0);
    let pos = usize::try_from(pos).unwrap_or(0).min(reach);
    let left_pad = if forward {
        pos
    } else {
        reach.saturating_sub(pos)
    };
    let mut pulse = String::with_capacity(width);
    for _ in 0..left_pad {
        pulse.push(' ');
    }
    for _ in 0..len {
        pulse.push('─');
    }
    for _ in 0..width.saturating_sub(left_pad).saturating_sub(len) {
        pulse.push(' ');
    }
    pulse
}

/// The circulating pulse bar as a fixed-width string with unstyled padding.
///
/// A segment in travel's geometry — a quarter of the row's width,
/// floored at three cells, capped at the row — advances one full lap
/// around the row per [`PULSE_PERIOD_MS`], covering
/// `[pos, pos + len)` modulo the width: while the tail is still
/// exiting the right edge the head is already entering the left, two
/// runs visible during the crossing and the row never blank. Like
/// the other styles, the phase is a pure function of elapsed time
/// and the string is exactly `width` characters.
pub(crate) fn pulse_snake(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let len = (width / 4).max(3).min(width);
    let pos = u64::try_from(width)
        .unwrap_or(0)
        .saturating_mul(mark)
        .checked_div(period)
        .unwrap_or(0);
    let pos = usize::try_from(pos)
        .unwrap_or(0)
        .min(width.saturating_sub(1));
    let head = pos.saturating_add(len).saturating_sub(width);
    let tail = len.saturating_sub(head);
    let mut pulse = String::with_capacity(width);
    for _ in 0..head {
        pulse.push('─');
    }
    for _ in 0..pos.saturating_sub(head) {
        pulse.push(' ');
    }
    for _ in 0..tail {
        pulse.push('─');
    }
    for _ in 0..width.saturating_sub(pos).saturating_sub(tail) {
        pulse.push(' ');
    }
    pulse
}

/// The worm path's position numerator and its shared denominator.
///
/// The path is the integral of a velocity `2 + 12(2u−1)⁴` — position
/// `(20u + 12(2u−1)⁵ + 12)/44` — racing the outer sections at up to
/// seven times its center crawl and dipping between smoothly. Every
/// progress shares one denominator, so a caller holding two ends
/// rounds their difference once instead of rounding the ends against
/// each other. The quintic terms outgrow `u64` at wide rows, hence
/// `u128`.
fn worm_numer(progress: u64, span: u64) -> (u128, u128) {
    let n = progress.min(span);
    let wide_span = u128::from(span);
    let second = wide_span.saturating_mul(wide_span);
    let fourth = second.saturating_mul(second);
    let fifth = fourth.saturating_mul(wide_span);
    let base = u128::from(n)
        .saturating_mul(20)
        .saturating_mul(fourth)
        .saturating_add(fifth.saturating_mul(12));
    let numer = if n.saturating_mul(2) <= span {
        base.saturating_sub(
            u128::from(span.saturating_sub(n.saturating_mul(2)))
                .saturating_pow(5)
                .saturating_mul(12),
        )
    } else {
        base.saturating_add(
            u128::from(n.saturating_mul(2).saturating_sub(span))
                .saturating_pow(5)
                .saturating_mul(12),
        )
    };
    (numer, fifth.saturating_mul(44).max(1))
}

/// The worm-large path: position numerator and denominator.
///
/// The path is the integral of a velocity `(3 + 6u + 6u²)/8` —
/// position `(3n·span² + 3n²·span + 2n³) / (8·span³)` — moving at
/// three-eighths of the mean speed from the first frame with real
/// acceleration from the start, the pull strengthening through and
/// past the row's center, the far edge arriving at five times the
/// seed's speed. No deceleration anywhere and no branch split; the
/// cubes keep the numerator far inside `u128`.
fn worm_large_numer(progress: u64, span: u64) -> (u128, u128) {
    let n = progress.min(span);
    let wide_span = u128::from(span);
    let wide_n = u128::from(n);
    let squared = wide_n.saturating_mul(wide_n);
    let cubed = squared.saturating_mul(wide_n);
    let span_squared = wide_span.saturating_mul(wide_span);
    (
        wide_n
            .saturating_mul(span_squared)
            .saturating_mul(3)
            .saturating_add(squared.saturating_mul(wide_span).saturating_mul(3))
            .saturating_add(cubed.saturating_mul(2)),
        span_squared
            .saturating_mul(wide_span)
            .saturating_mul(8)
            .max(1),
    )
}

/// Render a worm run between two half-cell edges.
///
/// A cell the run covers fully renders `─`, and one it covers halfway
/// renders that half as a solid tick — `╴` for the left half, `╶` for
/// the right — so the run reads as one line with trimmed ends, its
/// length breathing at most half a cell. An empty run still seeds the
/// run's own leading half-cell tick at the left while seeding — the
/// shape the next frames grow from — so a traversal's first frame
/// never snaps from a full cell to a half.
fn worm_run(head_h: u128, tail_h: u128, width: usize, seeding: bool) -> String {
    let mut pulse = String::with_capacity(width.saturating_mul(3));
    for cell in 0..width {
        let lo = (cell as u128).saturating_mul(2);
        let hi = lo.saturating_add(1);
        let lo_covered = lo >= tail_h && lo < head_h;
        let hi_covered = hi >= tail_h && hi < head_h;
        match (lo_covered, hi_covered) {
            (true, true) => pulse.push('─'),
            (true, false) => pulse.push('╴'),
            (false, true) => pulse.push('╶'),
            (false, false) => pulse.push(' '),
        }
    }
    if seeding && width > 0 && !pulse.contains('─') && !pulse.contains('╴') && !pulse.contains('╶')
    {
        pulse.replace_range(..1, "╴");
    }
    pulse
}

/// The worm pulse bar as a fixed-width string with unstyled padding.
///
/// The bunched inchworm crawl on a shared path: the head crosses the
/// row racing the outer sections and flowing through a center that
/// crawls at a seventh of that speed, and the tail follows the same
/// path a quarter-period behind — the run stretching as one end
/// races away, bunching where the other catches it flowing, and
/// absorbed at the head's settle on the right edge, the row blanking
/// for an instant before the seed reappears at the left; one
/// traversal per [`PULSE_PERIOD_MS`]. Like the other styles, the
/// phase is a pure function of elapsed time and the string is
/// exactly `width` characters, its run carrying the only style.
pub(crate) fn pulse_worm(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2);
    let quarter = period / 4;
    let move_span = quarter.saturating_mul(3);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let wide_width = width as u128;
    let (head_n, den) = worm_numer(mark, move_span);
    let (tail_n, _) = worm_numer(mark.saturating_sub(quarter), move_span);
    let scale = wide_width.saturating_mul(2);
    let head_h = head_n
        .saturating_mul(scale)
        .checked_div(den)
        .unwrap_or(0)
        .min(scale);
    let tail_h = if mark < move_span {
        tail_n.saturating_mul(scale).checked_div(den).unwrap_or(0)
    } else {
        tail_n.saturating_mul(scale).div_ceil(den)
    }
    .min(head_h);
    worm_run(head_h, tail_h, width, mark < move_span)
}

/// The large worm pulse bar as a fixed-width string with unstyled
/// padding.
///
/// The inchworm stretched to the row's full span on its own clock —
/// a little quicker than the shared pulse period, so the long
/// traversal reads as its own tempo — with one traversal spanning two
/// periods: the head rides the accelerating path over the whole first
/// period, setting off gently from the seed and pulling away ever
/// harder through and past the row's center, the run spanning the
/// full row at the period, while the tail rides the same path over
/// the whole second, swallowing the run into the right the same way;
/// each end moves one period and rests one, the tail anchored first,
/// the head settled last. Like
/// the other styles, the phase is a pure function of elapsed time and
/// the string is exactly `width` characters, its run carrying the
/// only style.
pub(crate) fn pulse_worm_large(began: Instant, now: Instant, width: usize) -> String {
    let period = PULSE_PERIOD_MS.max(2).saturating_mul(3).saturating_div(4);
    let traversal = period.saturating_mul(2);
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(traversal);
    let wide_width = width as u128;
    let (head_n, den) = worm_large_numer(mark, period);
    let (tail_n, _) = worm_large_numer(mark.saturating_sub(period), period);
    let scale = wide_width.saturating_mul(2);
    let head_h = head_n
        .saturating_mul(scale)
        .checked_div(den)
        .unwrap_or(0)
        .min(scale);
    let tail_h = if mark < period {
        tail_n.saturating_mul(scale).checked_div(den).unwrap_or(0)
    } else {
        tail_n.saturating_mul(scale).div_ceil(den)
    }
    .min(head_h);
    worm_run(head_h, tail_h, width, mark < period)
}

/// The glow's brightness at an instant, in eighths of full.
///
/// One eased breath per [`PULSE_PERIOD_MS`]: full brightness at the
/// breath's start, easing down to half at the half-breath and back —
/// a smoothstep on each leg, so the turns at the extremes settle
/// gently instead of snapping. A pure function of elapsed time, so
/// the glow renders identically at any frame rate.
pub(crate) fn glow_eighths(began: Instant, now: Instant) -> u64 {
    let period = PULSE_PERIOD_MS.max(2);
    let half = period / 2;
    let elapsed = u64::try_from(now.duration_since(began).as_millis()).unwrap_or(0);
    let mark = elapsed.rem_euclid(period);
    let leg = if mark < half {
        mark
    } else {
        mark.saturating_sub(half)
    };
    let leg_wide = u128::from(leg);
    let half_wide = u128::from(half).max(1);
    let leg_squared = leg_wide.saturating_mul(leg_wide);
    let half_cubed = half_wide
        .saturating_mul(half_wide)
        .saturating_mul(half_wide);
    let step = leg_squared
        .saturating_mul(half_wide)
        .saturating_mul(3)
        .saturating_sub(leg_squared.saturating_mul(leg_wide).saturating_mul(2));
    let eighths = if mark < half {
        8_u128
            .saturating_mul(half_cubed)
            .saturating_sub(step.saturating_mul(4))
    } else {
        4_u128
            .saturating_mul(half_cubed)
            .saturating_add(step.saturating_mul(4))
    }
    .checked_div(half_cubed)
    .unwrap_or(8);
    u64::try_from(eighths).unwrap_or(8)
}

/// A color scaled to eighths of its brightness.
///
/// RGB colors scale channel-wise; every other color returns
/// unchanged, so a theme with a named strip color renders the glow
/// as a plain static bar rather than guessing at named-color
/// distances.
pub(crate) fn scaled_color(color: Color, eighths: u64) -> Color {
    let scale = |channel: u8| -> u8 {
        u8::try_from(
            u64::from(channel)
                .saturating_mul(eighths)
                .checked_div(8)
                .unwrap_or(0),
        )
        .unwrap_or(0)
    };
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(scale(r), scale(g), scale(b)),
        other => other,
    }
}

/// The styled span of a pulse bar: only its runs, unstyled padding around.
///
/// Walking the bar's string — whichever style shaped it — run by run
/// keeps every padding cell free of any style, so the strip paints
/// exactly the bar and never a tinted band of colored spaces, however
/// many runs the style renders. The run class covers the bar's full
/// cell and its half-cell solid ticks alike: all three carry the
/// color.
pub(crate) fn pulse_span(
    style: StripStyle,
    began: Instant,
    now: Instant,
    width: usize,
    color: Color,
) -> Line<'static> {
    let pulse = match style {
        StripStyle::Breathe => pulse_centered(began, now, width),
        StripStyle::Sweep => pulse_sweep(began, now, width),
        StripStyle::Bounce => pulse_bounce(began, now, width),
        StripStyle::Travel => pulse_travel(began, now, width),
        StripStyle::Snake => pulse_snake(began, now, width),
        StripStyle::Worm => pulse_worm(began, now, width),
        StripStyle::WormLong => pulse_worm_large(began, now, width),
        StripStyle::Off => String::new(),
        StripStyle::Glow => std::iter::repeat_n('─', width).collect(),
    };
    let mut spans = Vec::new();
    let mut pad = String::new();
    let mut run = String::new();
    let mut in_run = false;
    for ch in pulse.chars() {
        if ch == '─' || ch == '╴' || ch == '╶' {
            if !in_run && !pad.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut pad)));
            }
            in_run = true;
            run.push(ch);
        } else {
            if in_run && !run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    Style::default().fg(color),
                ));
            }
            in_run = false;
            pad.push(ch);
        }
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, Style::default().fg(color)));
    } else if !pad.is_empty() {
        spans.push(Span::raw(pad));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {

    use std::time::Duration;

    use super::*;

    #[test]
    fn the_pulse_bar_breathes_symmetrically_from_the_center() {
        let began = Instant::now();
        let width = 20;
        let quarter = pulse_centered(began, began + Duration::from_millis(900), width);
        let three_quarter = pulse_centered(began, began + Duration::from_millis(2700), width);
        // 450 ms in: len 5 against 15 cells of padding — the odd
        // remainder that floors or ceils; the run must take the ceil
        // half on its left.
        let odd = pulse_centered(began, began + Duration::from_millis(450), width);
        for bar in [&quarter, &three_quarter, &odd] {
            assert_eq!(bar.chars().count(), width, "the bar fills its span exactly");
            let left = bar.chars().take_while(|ch| *ch == ' ').count();
            let right = bar.chars().rev().take_while(|ch| *ch == ' ').count();
            assert!(
                left == right || left == right.saturating_add(1),
                "centered to the cell — at most half a cell right: \
                 left {left}, right {right}: {bar:?}"
            );
        }
        assert!(
            odd.chars().take_while(|ch| *ch == ' ').count() == 8,
            "the odd remainder ceils onto the left pad: {odd:?}"
        );
        assert!(quarter.contains('─'), "the growing half carries a run");
        assert!(
            three_quarter.contains('─'),
            "the shrinking half still carries a run"
        );
        let full = pulse_centered(began, began + Duration::from_millis(1800), width);
        assert_eq!(
            full.matches('─').count(),
            width,
            "the cycle's peak fills the whole span"
        );
        let empty = pulse_centered(began, began, width);
        assert_eq!(
            empty.matches('─').count(),
            1,
            "the cycle's start and end hold the one-cell seed, never a blank strip"
        );
    }

    #[test]
    fn a_sweep_fills_from_the_left_then_clears_left_to_right() {
        let began = Instant::now();
        let width = 20;
        let seed = pulse_sweep(began, began, width);
        assert_eq!(
            seed.matches('─').count(),
            1,
            "the fill starts from a one-cell seed: {seed:?}"
        );
        assert!(seed.starts_with('─'), "the seed sits at the left edge");
        let quarter = pulse_sweep(began, began + Duration::from_millis(900), width);
        assert_eq!(
            quarter.matches('─').count(),
            10,
            "a quarter cycle fills half the row: {quarter:?}"
        );
        assert!(
            quarter.starts_with('─'),
            "the fill is left-anchored: {quarter:?}"
        );
        let full = pulse_sweep(began, began + Duration::from_millis(1800), width);
        assert_eq!(
            full.matches('─').count(),
            20,
            "the half point fills the whole row"
        );
        let three_quarter = pulse_sweep(began, began + Duration::from_millis(2700), width);
        assert_eq!(
            three_quarter.matches('─').count(),
            10,
            "the clear removes half the row from the left: {three_quarter:?}"
        );
        assert!(
            three_quarter.ends_with('─'),
            "the clear keeps the right edge pinned: {three_quarter:?}"
        );
        assert_eq!(
            three_quarter
                .chars()
                .take(10)
                .filter(|ch| *ch == ' ')
                .count(),
            10,
            "the cleared cells lead: {three_quarter:?}"
        );
        let near_wrap = pulse_sweep(began, began + Duration::from_millis(3599), width);
        assert_eq!(
            near_wrap.matches('─').count(),
            0,
            "the row blanks just before the wrap: {near_wrap:?}"
        );
        let wrapped = pulse_sweep(began, began + Duration::from_millis(3600), width);
        assert_eq!(
            wrapped.matches('─').count(),
            1,
            "the wrap restarts the seed: {wrapped:?}"
        );
        for bar in [&seed, &quarter, &full, &three_quarter, &near_wrap, &wrapped] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    #[test]
    fn a_bounce_sweeps_each_way_in_turn() {
        let began = Instant::now();
        let width = 20;
        let forward_seed = pulse_bounce(began, began, width);
        assert!(
            forward_seed.starts_with('─') && forward_seed.matches('─').count() == 1,
            "the forward pass seeds at the left edge: {forward_seed:?}"
        );
        let forward_half = pulse_bounce(began, began + Duration::from_millis(900), width);
        assert!(
            forward_half.starts_with('─') && forward_half.matches('─').count() == 10,
            "the forward fill grows left-anchored: {forward_half:?}"
        );
        let forward_clear = pulse_bounce(began, began + Duration::from_millis(2700), width);
        assert!(
            forward_clear.ends_with('─') && forward_clear.matches('─').count() == 10,
            "the forward clear keeps the right edge pinned: {forward_clear:?}"
        );
        let turn = pulse_bounce(began, began + Duration::from_millis(3599), width);
        assert_eq!(
            turn.matches('─').count(),
            0,
            "the row blanks for the turn: {turn:?}"
        );
        let reverse_seed = pulse_bounce(began, began + Duration::from_millis(3600), width);
        assert!(
            reverse_seed.ends_with('─') && reverse_seed.matches('─').count() == 1,
            "the reverse pass seeds at the right edge: {reverse_seed:?}"
        );
        let reverse_half = pulse_bounce(began, began + Duration::from_millis(4500), width);
        assert!(
            reverse_half.ends_with('─') && reverse_half.matches('─').count() == 10,
            "the reverse fill grows right-anchored: {reverse_half:?}"
        );
        let reverse_clear = pulse_bounce(began, began + Duration::from_millis(6300), width);
        assert!(
            reverse_clear.starts_with('─') && reverse_clear.matches('─').count() == 10,
            "the reverse clear keeps the left edge pinned: {reverse_clear:?}"
        );
        let back = pulse_bounce(began, began + Duration::from_millis(7199), width);
        assert_eq!(
            back.matches('─').count(),
            0,
            "the row blanks before the round trip wraps: {back:?}"
        );
        let wrapped = pulse_bounce(began, began + Duration::from_millis(7200), width);
        assert!(
            wrapped.starts_with('─') && wrapped.matches('─').count() == 1,
            "the wrap restarts the forward seed: {wrapped:?}"
        );
        for bar in [
            &forward_seed,
            &forward_half,
            &forward_clear,
            &turn,
            &reverse_seed,
            &reverse_half,
            &reverse_clear,
            &back,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    #[test]
    fn a_travel_glides_each_way_across_the_row() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let forward_start = pulse_travel(began, began, width);
        assert!(
            leading(&forward_start) == 0 && forward_start.matches('─').count() == 5,
            "the forward pass starts the segment at the left edge: {forward_start:?}"
        );
        let forward_mid = pulse_travel(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&forward_mid) == 7 && forward_mid.matches('─').count() == 5,
            "mid-pass the segment sits mid-row: {forward_mid:?}"
        );
        let reverse_start = pulse_travel(began, began + Duration::from_millis(3600), width);
        assert!(
            leading(&reverse_start) == 15 && reverse_start.ends_with('─'),
            "the reverse pass continues from the right edge: {reverse_start:?}"
        );
        let reverse_mid = pulse_travel(began, began + Duration::from_millis(4500), width);
        assert!(
            leading(&reverse_mid) == 12 && reverse_mid.matches('─').count() == 5,
            "the reverse glide walks the span backwards: {reverse_mid:?}"
        );
        let wrapped = pulse_travel(began, began + Duration::from_millis(7200), width);
        assert!(
            leading(&wrapped) == 0 && wrapped.matches('─').count() == 5,
            "the wrap restarts the forward pass: {wrapped:?}"
        );
        for bar in [
            &forward_start,
            &forward_mid,
            &reverse_start,
            &reverse_mid,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
            assert_eq!(
                bar.matches('─').count(),
                5,
                "the segment keeps its length at every sample"
            );
        }
    }

    #[test]
    fn a_snake_circulates_through_the_wrap() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let lap_start = pulse_snake(began, began, width);
        assert!(
            leading(&lap_start) == 0 && lap_start.matches('─').count() == 5,
            "the lap starts the segment at the left edge: {lap_start:?}"
        );
        let mid_lap = pulse_snake(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&mid_lap) == 10 && mid_lap.matches('─').count() == 5,
            "mid-lap the segment sits mid-row: {mid_lap:?}"
        );
        let wrapping = pulse_snake(began, began + Duration::from_millis(3150), width);
        assert!(
            wrapping.starts_with('─')
                && wrapping.ends_with('─')
                && wrapping.matches('─').count() == 5,
            "the crossing splits the segment across both edges: {wrapping:?}"
        );
        let near_lap_end = pulse_snake(began, began + Duration::from_millis(3599), width);
        assert!(
            near_lap_end.starts_with('─')
                && near_lap_end.ends_with('─')
                && near_lap_end.matches('─').count() == 5,
            "the segment stays whole in circulation, never blank: {near_lap_end:?}"
        );
        let wrapped = pulse_snake(began, began + Duration::from_millis(3600), width);
        assert!(
            leading(&wrapped) == 0 && wrapped.matches('─').count() == 5,
            "the lap wraps cleanly: {wrapped:?}"
        );
        for bar in [&lap_start, &mid_lap, &wrapping, &near_lap_end, &wrapped] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
            assert_eq!(
                bar.matches('─').count(),
                5,
                "the segment keeps its length at every sample"
            );
        }
    }

    #[test]
    fn a_large_worm_stretches_the_full_row_and_folds_back() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let fulls = |bar: &str| bar.matches('─').count();
        let halves = |bar: &str| bar.matches('╴').count() + bar.matches('╶').count();
        let left_seed = pulse_worm_large(began, began, width);
        assert!(
            leading(&left_seed) == 0 && fulls(&left_seed) == 0 && halves(&left_seed) == 1,
            "the run's own half-cell tick seeds the left edge: {left_seed:?}"
        );
        let setting_off = pulse_worm_large(began, began + Duration::from_millis(900), width);
        assert!(
            leading(&setting_off) == 0 && fulls(&setting_off) == 3 && halves(&setting_off) == 1,
            "the head sets off with the acceleration already felt: {setting_off:?}"
        );
        let building = pulse_worm_large(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&building) == 0 && fulls(&building) == 9 && halves(&building) == 1,
            "the head pulls harder as it approaches the row's center: {building:?}"
        );
        let center_crossing = pulse_worm_large(began, began + Duration::from_millis(2400), width);
        assert!(
            leading(&center_crossing) == 0
                && fulls(&center_crossing) == 16
                && halves(&center_crossing) == 0,
            "past the row's center the acceleration is at its steepest so far: {center_crossing:?}"
        );
        let arrived = pulse_worm_large(began, began + Duration::from_millis(2700), width);
        assert!(
            leading(&arrived) == 0
                && fulls(&arrived) == 20
                && halves(&arrived) == 0
                && arrived.starts_with('─')
                && arrived.ends_with('─'),
            "the head reaches the right edge at full sprint: {arrived:?}"
        );
        let chasing = pulse_worm_large(began, began + Duration::from_millis(3599), width);
        assert!(
            leading(&chasing) == 4
                && fulls(&chasing) == 16
                && halves(&chasing) == 0
                && chasing.ends_with('─'),
            "the tail chases the settled head, the run pinned to the right edge: {chasing:?}"
        );
        let building_tail = pulse_worm_large(began, began + Duration::from_millis(4500), width);
        assert!(
            leading(&building_tail) == 10
                && fulls(&building_tail) == 10
                && halves(&building_tail) == 0,
            "the tail pulls harder as it crosses the row's center: {building_tail:?}"
        );
        let folding = pulse_worm_large(began, began + Duration::from_millis(5300), width);
        assert!(
            leading(&folding) == 19
                && fulls(&folding) == 1
                && halves(&folding) == 0
                && folding.ends_with('─'),
            "the tail folds the run home: {folding:?}"
        );
        let gone = pulse_worm_large(began, began + Duration::from_millis(5399), width);
        assert_eq!(
            (fulls(&gone), halves(&gone)),
            (0, 0),
            "the row blanks just before the wrap: {gone:?}"
        );
        let wrapped = pulse_worm_large(began, began + Duration::from_millis(5400), width);
        assert!(
            leading(&wrapped) == 0 && fulls(&wrapped) == 0 && halves(&wrapped) == 1,
            "the wrap restarts the left seed at half-cell width: {wrapped:?}"
        );
        for bar in [
            &left_seed,
            &setting_off,
            &building,
            &center_crossing,
            &arrived,
            &chasing,
            &building_tail,
            &folding,
            &gone,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    #[test]
    fn a_worms_edges_step_in_half_cells_with_their_ends() {
        let began = Instant::now();
        let width = 20;
        let leading = |bar: &str| bar.chars().take_while(|ch| *ch == ' ').count();
        let fulls = |bar: &str| bar.matches('─').count();
        let halves = |bar: &str| bar.matches('╴').count() + bar.matches('╶').count();
        let left_seed = pulse_worm(began, began, width);
        assert!(
            leading(&left_seed) == 0 && fulls(&left_seed) == 0 && halves(&left_seed) == 1,
            "the run's own half-cell tick seeds the left edge: {left_seed:?}"
        );
        let stretching = pulse_worm(began, began + Duration::from_millis(900), width);
        assert!(
            leading(&stretching) == 0 && fulls(&stretching) == 8 && halves(&stretching) == 0,
            "the head races toward the center while the tail waits: {stretching:?}"
        );
        let bunched = pulse_worm(began, began + Duration::from_millis(1350), width);
        assert!(
            leading(&bunched) == 6 && fulls(&bunched) == 4 && halves(&bunched) == 0,
            "the tail closes on the head flowing through the center: {bunched:?}"
        );
        let head_half = pulse_worm(began, began + Duration::from_millis(1800), width);
        assert!(
            leading(&head_half) == 8
                && fulls(&head_half) == 3
                && halves(&head_half) == 1
                && head_half.contains("───╴"),
            "the head's edge crosses its half — one solid tick trimming the run's end: {head_half:?}"
        );
        let before = pulse_worm(began, began + Duration::from_millis(1799), width);
        assert_eq!(
            (fulls(&before), halves(&before)),
            (fulls(&head_half), halves(&head_half)),
            "adjacent frames hold the bunch steady — each edge rounds alone: {before:?} {head_half:?}"
        );
        let tail_half = pulse_worm(began, began + Duration::from_millis(1950), width);
        assert!(
            leading(&tail_half) == 8
                && fulls(&tail_half) == 3
                && halves(&tail_half) == 1
                && tail_half.contains(" ╶──"),
            "the tail's edge crosses its half beside the center — one solid tick leading the run, no twitch: {tail_half:?}"
        );
        let stretched = pulse_worm(began, began + Duration::from_millis(2250), width);
        assert!(
            leading(&stretched) == 10 && fulls(&stretched) == 3 && halves(&stretched) == 1,
            "the head races right while the tail flows through the center: {stretched:?}"
        );
        let right_full = pulse_worm(began, began + Duration::from_millis(2700), width);
        assert!(
            leading(&right_full) == 12
                && right_full.ends_with('─')
                && fulls(&right_full) == 8
                && halves(&right_full) == 0,
            "the head settles at the right edge, the run pinned to it: {right_full:?}"
        );
        let absorbing = pulse_worm(began, began + Duration::from_millis(3150), width);
        assert!(
            leading(&absorbing) == 14 && fulls(&absorbing) == 6 && halves(&absorbing) == 0,
            "the racing tail clears the run from the left: {absorbing:?}"
        );
        let gone = pulse_worm(began, began + Duration::from_millis(3599), width);
        assert_eq!(
            (fulls(&gone), halves(&gone)),
            (0, 0),
            "the row blanks just before the wrap: {gone:?}"
        );
        let wrapped = pulse_worm(began, began + Duration::from_millis(3600), width);
        assert!(
            leading(&wrapped) == 0 && fulls(&wrapped) == 0 && halves(&wrapped) == 1,
            "the wrap restarts the left seed at half-cell width: {wrapped:?}"
        );
        for bar in [
            &left_seed,
            &stretching,
            &bunched,
            &head_half,
            &before,
            &tail_half,
            &stretched,
            &right_full,
            &absorbing,
            &gone,
            &wrapped,
        ] {
            assert_eq!(
                bar.chars().count(),
                width,
                "every sample fills its span exactly"
            );
        }
    }

    use crate::app::test_support::*;

    use crate::commands::Command;

    #[test]
    fn strip_settings_apply_live_without_a_restart() {
        // The strip pair is display state copied at construction; the
        // live arm re-copies it from the config the /set just edited,
        // so the next frame animates in the new shape and row — no
        // restart owed.
        let mut app = app();
        assert_eq!(app.display.strip_style, dch_config::StripStyle::Breathe);
        app.run_command(Command::Set {
            key: "strip_style".to_string(),
            value: "sweep".to_string(),
        });
        assert_eq!(
            app.display.strip_style,
            dch_config::StripStyle::Sweep,
            "the very next frame animates the configured style"
        );
        assert_eq!(
            app.display.strip_position,
            dch_config::StripPosition::Composer
        );
        app.run_command(Command::Set {
            key: "strip_position".to_string(),
            value: "top".to_string(),
        });
        assert_eq!(
            app.display.strip_position,
            dch_config::StripPosition::Top,
            "the strip answers its new row without a restart"
        );
    }
}
