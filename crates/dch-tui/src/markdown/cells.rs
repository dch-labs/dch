//! Cell-grid measurement and the display-width policy every renderer
//! in this crate shares.

use std::borrow::Cow;
use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthChar as _;
use unicode_width::UnicodeWidthStr;

/// The variation selector that asks for emoji presentation (VS16).
///
/// A cell-grid terminal cannot honor the request uniformly: some
/// terminals and fonts paint the selector-bearing cluster across two
/// cells, others across one, and no query exists that would tell this
/// process which. The display policy below strips it, so a cluster
/// the screen cannot size never reaches a surface that applies the
/// policy — the composer and the question sheet's compose draft are
/// the two documented raw surfaces, kept whole for their own stated
/// reasons.
const EMOJI_PRESENTATION_SELECTOR: char = '\u{fe0f}';

/// The variation selector that asks for text presentation (VS15).
///
/// The same disagreement in the other direction: a terminal that
/// honors the selector paints the cluster narrow while one that
/// ignores it may still paint its emoji face wide. The selector is
/// the only part of the cluster whose removal cannot change the
/// glyph a font chooses on its own, so the policy strips it with its
/// sibling.
const TEXT_PRESENTATION_SELECTOR: char = '\u{fe0e}';

/// The zero-width joiner that chains emoji into one composed glyph.
///
/// The shared table charges each member its own cells and the joiner
/// nothing, while a composing terminal paints the whole chain as one
/// glyph — the same unsizable disagreement the selector carries, so
/// the policy strips it too and the members render side by side.
const ZERO_WIDTH_JOINER: char = '\u{200d}';

/// The combining mark that turns a digit, `#`, or `*` into a keycap
/// glyph.
///
/// The table gives the stripped-selector remainder (`1` plus the
/// mark) one cell while a terminal that composes off the mark alone
/// paints a two-cell keycap — so the policy drops the mark after a
/// keycap base and the digit renders as itself.
const KEYCAP_MARK: char = '\u{20e3}';

/// The characters a keycap mark may legally follow.
///
/// The grammar's own base set; anything else carrying the mark keeps
/// it, measured as the combining character it is.
fn is_keycap_base(ch: char) -> bool {
    ch.is_ascii_digit() || ch == '#' || ch == '*'
}

/// The emoji modifiers — the skin-tone selectors.
///
/// The table's whole-string measure and a composing terminal agree
/// with each other on a base-plus-modifier cluster, but a
/// non-composing terminal advances the two wide members separately:
/// model and buffer say two cells, that terminal says four, and the
/// two cells past the glyph go stale. The modifier is presentation,
/// not content — the policy drops it after an emoji base and the
/// base renders at one width everywhere.
fn is_emoji_modifier(ch: char) -> bool {
    matches!(ch, '\u{1f3fb}'..='\u{1f3ff}')
}

/// Whether `ch` is a pictograph a modifier may legally follow.
///
/// The emoji planes and the symbol blocks the modifier bases live
/// in; precise to the grammar for every real sequence, and a
/// modifier after anything else keeps its own two cells — a swatch
/// every measure agrees on.
fn is_modifier_base(ch: char) -> bool {
    ch >= '\u{1f300}' || matches!(ch, '\u{2600}'..='\u{27bf}')
}

/// The tag characters subdivision flags carry their region in.
///
/// Format characters with no cell of their own, legible only to a
/// terminal that composes the whole sequence into one flag — a
/// composition the policy refuses to rely on. The tags strip with
/// the rest and the flag's poles render as themselves.
fn is_tag_character(ch: char) -> bool {
    matches!(ch, '\u{e0020}'..='\u{e007f}')
}

/// The display width of `text` in terminal cells.
///
/// The one measure this crate's layout math is allowed to use. It
/// rides the same `unicode-width` table ratatui renders and diffs
/// with, so a width computed here is the width the buffer believes —
/// the property whose absence let stale cells leak beside conversation
/// rows when the app measured with one table and the buffer with
/// another.
#[must_use]
pub fn str_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// The display width a single character occupies.
///
/// Zero for the characters that occupy no cell of their own —
/// combining marks, joiners, format characters — so callers sum
/// without an `unwrap_or` at every site. A selector-bearing cluster
/// still measures through [`str_width`], which sees the whole
/// sequence; per-character summation is for iteration that walks
/// `chars()` and cannot stop to cluster.
#[must_use]
pub fn char_cells(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// The text as the screen should carry it: every composition input
/// stripped, every surviving grapheme self-consistent.
///
/// The display half of the width policy. The selectors (`FE0F`/
/// `FE0E`), the zero-width joiner, the keycap mark after its base,
/// the emoji modifiers after their base, and the flag tag characters
/// are all presentation — they change what glyph a composing terminal
/// paints without changing the text's content — and every one of them
/// makes the model's width, the buffer's width, and some terminal
/// class's width disagree. Stripping them leaves text whose every
/// grapheme measures the same to the whole-string table the buffer
/// diffs with, to the per-character sums a non-composing terminal
/// advances by, and to the composing terminals that no longer
/// receive anything to compose. Two classes need no strip and are
/// pinned by test rather than transformed: a regional-indicator pair
/// measures two cells as a cluster, two as separate indicators, and
/// two as a composed flag; and the table (pinned by ratatui to
/// `=0.2.0`, so frozen for buffer and app alike) scores the
/// default-emoji-presentation symbols — `❤` the famous one — one
/// cell against the two an emoji-font terminal paints, a
/// font-dependent window no cell-grid program can close from its
/// side. The strip is for the screen only: a copy must reproduce the
/// text the user selected, so callers sanitize a display clone and
/// never the cached lines a selection copies from. Clean text
/// borrows through untouched.
#[must_use]
pub fn for_display(text: &str) -> Cow<'_, str> {
    if !text.chars().any(needs_policy_walk) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(policy_projection(text).0)
}

/// Whether the policy removes `ch` given the character kept before
/// it.
///
/// The one decision list every projection consults — the strip,
/// the painted mapping, and the review walk — so no two projections
/// can ever disagree about what the policy removes.
fn policy_removes(ch: char, previous: Option<char>) -> bool {
    ch == EMOJI_PRESENTATION_SELECTOR
        || ch == TEXT_PRESENTATION_SELECTOR
        || ch == ZERO_WIDTH_JOINER
        || is_tag_character(ch)
        || (ch == KEYCAP_MARK && previous.is_some_and(is_keycap_base))
        || (is_emoji_modifier(ch) && previous.is_some_and(is_modifier_base))
}

/// The strip walk itself, returning the projection and each kept
/// character's raw byte offset.
///
/// The one implementation of the policy's decisions: [`for_display`]
/// takes the string half and [`painted_steps`] takes the mapping
/// half, so a projection can never disagree with the text the screen
/// receives. The offsets index `text` at each kept character's first
/// byte, in order — exactly the anchors a painted-to-raw mapping
/// needs.
fn policy_projection(text: &str) -> (String, Vec<usize>) {
    let mut cleaned = String::with_capacity(text.len());
    let mut starts = Vec::new();
    let mut previous: Option<char> = None;
    for (index, ch) in text.char_indices() {
        if policy_removes(ch, previous) {
            continue;
        }
        cleaned.push(ch);
        starts.push(index);
        previous = Some(ch);
    }
    (cleaned, starts)
}

/// One painted grapheme's place in its raw source: the raw byte
/// range it (with any composition inputs bound to it) came from, and
/// the cells it paints.
///
/// The raw range tiles the source text: a grapheme's range runs from
/// its own first kept character to the next grapheme's, so the
/// composition inputs a cluster carried — the selector after a
/// glyph, the joiner between members — stay bound to the grapheme
/// before them, and a copy that covers a cluster reproduces the
/// author's text whole while a copy that cuts through a joined chain
/// still lands on character boundaries. The last grapheme's range
/// runs to the end of the source.
pub type PaintedStep = (std::ops::Range<usize>, usize);

/// The graphemes of `text` as the pane paints them, each with its
/// raw source range and painted width.
///
/// The walk the selection's cell math runs on: the widths are the
/// post-policy cells the strip actually renders (a family's members
/// at two cells each, a selector's glyph at one), so a screen column
/// the reader swept and a column this walk counts are the same
/// column. Graphemes are grouped in the projection and measured
/// whole, which the no-leak invariant already guarantees agrees with
/// their per-character sums.
#[must_use]
pub fn painted_steps(text: &str) -> Vec<PaintedStep> {
    let (cleaned, starts) = policy_projection(text);
    let mut firsts = Vec::new();
    let mut char_index = 0usize;
    let mut widths = Vec::new();
    for cluster in cleaned.graphemes(true) {
        let Some(first) = starts.get(char_index) else {
            break;
        };
        firsts.push(*first);
        widths.push(str_width(cluster));
        char_index = char_index.saturating_add(cluster.chars().count());
    }
    // Composition inputs the text opens with belong to the first
    // grapheme's range, so the ranges tile the whole source.
    if let Some(open) = firsts.first_mut() {
        *open = 0;
    }
    let mut ends = firsts.iter().copied().skip(1).collect::<VecDeque<_>>();
    ends.push_back(text.len());
    firsts
        .into_iter()
        .zip(ends)
        .zip(widths)
        .map(|((start, end), width)| (start..end, width))
        .collect()
}

/// Whether the policy walk must consider `ch`.
///
/// The gate every stripped class routes through: a text carrying
/// none of these code points cannot change under the policy and
/// borrows through untouched. The regional indicators are
/// deliberately absent — a pair is already self-consistent and
/// passes as it stands.
fn needs_policy_walk(ch: char) -> bool {
    ch == EMOJI_PRESENTATION_SELECTOR
        || ch == TEXT_PRESENTATION_SELECTOR
        || ch == ZERO_WIDTH_JOINER
        || ch == KEYCAP_MARK
        || is_emoji_modifier(ch)
        || is_tag_character(ch)
}

/// The cells the screen will spend on `text` — the post-policy
/// measure every layout gate must budget by.
///
/// The width a row actually paints is the width of its stripped
/// projection, not the width the whole-string table scores for the
/// raw text: a joined emoji family measures two as a raw cluster and
/// six once the policy renders its members apart, so a gate that
/// budgets by the raw score reserves two cells for what paints as
/// six and clips the row's tail. This is the one number both the
/// wrap math and the screen agree on; `str_width` alone stays for
/// text already known to be post-policy or for asserting the table's
/// own verdicts.
#[must_use]
pub fn display_width(text: &str) -> usize {
    let cleaned = for_display(text);
    str_width(&cleaned)
}

/// Append `ch` to `out` as its `\u{…}` escape, uppercase hex.
///
/// Hand-rolled rather than `fmt`: the crate's denied-lint set admits
/// neither a discarded `write!` result nor the `format!`-push, and
/// the hex table plus a fixed shift list is the whole story for the
/// policy's removed code points — every one of them four or five
/// significant digits.
fn push_escape(ch: char, out: &mut String) {
    /// The digit a nibble spells, uppercase.
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    /// Nibble shifts, most significant first.
    const SHIFTS: [u32; 8] = [28, 24, 20, 16, 12, 8, 4, 0];
    let value = u32::from(ch);
    out.push_str("\\u{");
    let mut leading = true;
    for &shift in &SHIFTS {
        let nibble = ((value >> shift) & 0xf) as usize;
        if nibble == 0 && leading && shift != 0 {
            continue;
        }
        leading = false;
        if let Some(digit) = HEX.get(nibble) {
            out.push(*digit as char);
        }
    }
    out.push('}');
}

/// The text as the permission sheet previews it: composition inputs
/// rendered as visible `\u{…}` escapes.
///
/// The strip that serves prose serves a decision surface badly: two
/// strings differing only by a composition input — a scope with and
/// without its selector, a command with and without its joiner —
/// strip to one preview while the arms still record and run the
/// distinct originals, so the sheet could ask a reader to approve
/// one of two commands it cannot tell apart. The review walk removes
/// exactly what the display policy removes — one decision list,
/// shared with [`for_display`] — but renders each removed input as
/// its Rust escape: the preview stays measurable plain text, and two
/// distinct inputs can never share one preview. A literal backslash
/// doubles — the one way a typed escape spelling could otherwise
/// collide with a removed code point's escape — so plain text passes
/// through unchanged except its backslashes, and the caller's
/// originals are untouched; only the preview changes.
#[must_use]
pub fn for_review(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous: Option<char> = None;
    for ch in text.chars() {
        if policy_removes(ch, previous) {
            push_escape(ch, &mut out);
            continue;
        }
        if ch == '\\' {
            out.push_str("\\\\");
        } else {
            out.push(ch);
        }
        previous = Some(ch);
    }
    out
}

/// The widest head of `text` that fits `budget` display cells.
///
/// Walks graphemes and measures each with [`str_width`], so a
/// combining sequence the budget crosses is kept or dropped whole and
/// the returned slice's width — by the same table the caller gates
/// with — never exceeds `budget`. A first grapheme wider than the
/// whole budget yields the empty head.
#[must_use]
pub fn prefix_within(text: &str, budget: usize) -> &str {
    let mut used = 0usize;
    let mut end = 0usize;
    for cluster in text.graphemes(true) {
        let cells = str_width(cluster);
        if used.saturating_add(cells) > budget {
            break;
        }
        used = used.saturating_add(cells);
        end = end.saturating_add(cluster.len());
    }
    text.get(..end).unwrap_or("")
}

/// The widest tail of `text` that fits `budget` display cells.
///
/// The head's mirror for caps that keep the end — the part that tells
/// two long values apart. The same grapheme walk under the same
/// [`str_width`] table, so the returned slice measures no more than
/// `budget` cells and never splits a combining sequence.
#[must_use]
pub fn suffix_within(text: &str, budget: usize) -> &str {
    let mut used = 0usize;
    let mut start = text.len();
    for cluster in text.graphemes(true).rev() {
        let cells = str_width(cluster);
        if used.saturating_add(cells) > budget {
            break;
        }
        used = used.saturating_add(cells);
        start = start.saturating_sub(cluster.len());
    }
    text.get(start..).unwrap_or("")
}

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::expect_used,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_bearing_cluster_measures_two_cells() {
        // The measure that must match the buffer's: ratatui's own
        // table (unicode-width 0.2) counts a pictograph plus VS16 as
        // two cells, and this oracle rides the same version.
        assert_eq!(str_width("\u{2714}\u{fe0f}"), 2);
        assert_eq!(str_width("\u{2714}"), 1);
    }

    #[test]
    fn the_display_policy_strips_the_presentation_set_and_nothing_else() {
        assert_eq!(
            for_display("done \u{2714}\u{fe0f} shipped"),
            "done \u{2714} shipped"
        );
        assert_eq!(for_display("a\u{301} e\u{301}"), "a\u{301} e\u{301}");
        assert!(matches!(
            for_display("plain text"),
            Cow::Borrowed("plain text")
        ));
    }

    #[test]
    fn the_display_policy_splits_a_joined_chain_into_its_members() {
        // The joined chain measures two to the whole-string table and
        // six to a non-composing terminal's per-character advance;
        // stripped, the members measure six to both.
        assert_eq!(
            for_display("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
            "\u{1f468}\u{1f469}\u{1f467}"
        );
        assert!(matches!(for_display("\u{200d}"), Cow::Owned(_)));
    }

    #[test]
    fn the_display_policy_drops_a_modifier_after_its_base() {
        // The whole-string table and a composing terminal say two
        // cells for a base-plus-modifier cluster; a non-composing
        // terminal advances four. The modifier is presentation — the
        // stripped base renders two cells to every measure.
        assert_eq!(str_width("\u{1f44d}\u{1f3fd}"), 2);
        assert_eq!(
            "\u{1f44d}\u{1f3fd}".chars().map(char_cells).sum::<usize>(),
            4
        );
        assert_eq!(for_display("\u{1f44d}\u{1f3fd}"), "\u{1f44d}");
        assert_eq!(for_display("\u{270c}\u{fe0f}\u{1f3fd} ok"), "\u{270c} ok");
        // A modifier after nothing its grammar names keeps its own
        // two cells — a swatch every measure agrees on.
        assert_eq!(for_display("x\u{1f3fd}"), "x\u{1f3fd}");
    }

    #[test]
    fn the_display_policy_drops_a_keycap_mark_after_its_base() {
        // Stripped of its selector the keycap is a digit plus a
        // combining mark — one cell to the table, two to a terminal
        // that composes off the mark alone. The digit alone is one
        // cell everywhere.
        assert_eq!(for_display("1\u{fe0f}\u{20e3}"), "1");
        assert_eq!(for_display("#\u{fe0f}\u{20e3}"), "#");
        assert_eq!(for_display("a\u{20e3}"), "a\u{20e3}");
    }

    #[test]
    fn the_display_policy_drops_flag_tags_and_the_text_selector() {
        // A subdivision flag carries its region in tag characters a
        // only composing terminal reads; the tags strip with the
        // joiner and the poles render as themselves.
        assert_eq!(
            for_display("\u{1f3f3}\u{fe0f}\u{200d}\u{26a7}\u{fe0f}\u{e0067}\u{e0062}"),
            "\u{1f3f3}\u{26a7}"
        );
        assert_eq!(for_display("\u{2764}\u{fe0e}"), "\u{2764}");
    }

    #[test]
    fn a_regional_indicator_pair_passes_self_consistent() {
        // The famous pair needs no strip: the cluster, the two
        // indicators apart, and the composed flag all measure two
        // cells, so every terminal class agrees without help.
        let pair = "\u{1f1f3}\u{1f1f4}";
        assert_eq!(str_width(pair), 2);
        assert_eq!(pair.chars().map(char_cells).sum::<usize>(), 2);
        assert!(matches!(for_display(pair), Cow::Borrowed(_)));
    }

    #[test]
    fn every_surviving_grapheme_measures_the_same_whole_and_summed() {
        // The no-leak invariant: after the policy, no grapheme on the
        // screen can make a non-composing terminal advance by a
        // different width than the buffer's whole-string measure —
        // the two numbers this test holds equal are the two numbers
        // that used to disagree.
        let corpus = [
            "family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} next",
            "tone \u{1f44d}\u{1f3fd} done \u{270c}\u{fe0f}\u{1f3fd}",
            "key 1\u{fe0f}\u{20e3} and #\u{20e3}",
            "flag \u{1f1f3}\u{1f1f4} sub \u{1f3f3}\u{fe0f}\u{200d}\u{26a7}\u{fe0f}\u{e0074}\u{e0067}",
            "marks \u{2714}\u{fe0f}\u{fe0e} and \u{2764}\u{fe0e}",
            "cjk \u{4e2d}\u{6587} combining e\u{301} plain",
        ];
        for case in corpus {
            let cleaned = for_display(case);
            for cluster in cleaned.graphemes(true) {
                let whole = str_width(cluster);
                let summed = cluster.chars().map(char_cells).sum::<usize>();
                assert_eq!(
                    whole, summed,
                    "grapheme {cluster:?} in {cleaned:?} splits the measures"
                );
            }
        }
    }

    #[test]
    fn the_frozen_table_scores_default_emoji_presentation_symbols_one_cell() {
        // The documented irreducible window: ratatui pins this
        // crate's table to =0.2.0, which scores the
        // default-emoji-presentation symbols one cell while an
        // emoji-font terminal paints two. No cell-grid program can
        // close a font-dependent window from its side; this pin
        // moves loudly when a ratatui upgrade un-pins the table and
        // the window can be re-evaluated.
        assert_eq!(char_cells('\u{2764}'), 1);
        assert_eq!(str_width("\u{2764}"), 1);
    }

    #[test]
    fn display_width_scores_what_the_screen_spends_not_what_the_table_scores() {
        // Every class whose raw cluster scores narrower than its
        // stripped render. A layout gate budgeting
        // by the raw score reserves too few cells and clips the row
        // it sized; this measure is the one the screen agrees with.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(str_width(family), 2);
        assert_eq!(display_width(family), 6);
        assert_eq!(str_width("\u{2714}\u{fe0f}"), 2);
        assert_eq!(display_width("\u{2714}\u{fe0f}"), 1);
        assert_eq!(str_width("\u{1f44d}\u{1f3fd}"), 2);
        assert_eq!(display_width("\u{1f44d}\u{1f3fd}"), 2);
        assert_eq!(display_width("1\u{fe0f}\u{20e3}"), 1);
        assert_eq!(
            display_width("plain words"),
            str_width("plain words"),
            "text the policy leaves alone measures identically"
        );
    }

    #[test]
    fn zero_width_characters_measure_zero_cells() {
        assert_eq!(char_cells('\u{200d}'), 0);
        assert_eq!(char_cells('\u{fe0f}'), 0);
        assert_eq!(char_cells('a'), 1);
        assert_eq!(char_cells('\u{4e2d}'), 2);
    }

    #[test]
    fn painted_steps_map_painted_cells_back_to_raw_spans() {
        // The mapping: every grapheme walks at the width the pane
        // paints it, its raw range carries the composition inputs
        // bound to it, and the ranges tile the source so a copy
        // reproduces the author's bytes.
        let check = "a\u{2714}\u{fe0f}b";
        let steps = painted_steps(check);
        let widths: Vec<usize> = steps.iter().map(|(_, w)| *w).collect();
        assert_eq!(widths, vec![1, 1, 1]);
        let slices: Vec<&str> = steps
            .iter()
            .map(|(range, _)| check.get(range.clone()).unwrap_or(""))
            .collect();
        assert_eq!(slices, vec!["a", "\u{2714}\u{fe0f}", "b"]);

        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let steps = painted_steps(family);
        let widths: Vec<usize> = steps.iter().map(|(_, w)| *w).collect();
        assert_eq!(widths, vec![2, 2, 2]);
        let joined: String = steps
            .iter()
            .map(|(range, _)| family.get(range.clone()).unwrap_or(""))
            .collect();
        assert_eq!(joined, family, "the raw ranges tile the source");
        assert_eq!(
            steps.iter().map(|(_, w)| w).sum::<usize>(),
            display_width(family)
        );
    }

    #[test]
    fn leading_composition_inputs_bind_to_the_first_grapheme() {
        // A text opening with a composition input still tiles whole:
        // the input binds to the first grapheme's raw range, so a
        // copy covering the first painted character reproduces the
        // author's leading bytes with it.
        let text = "\u{fe0f}abc";
        let steps = painted_steps(text);
        let slices: Vec<&str> = steps
            .iter()
            .map(|(range, _)| text.get(range.clone()).unwrap_or(""))
            .collect();
        assert_eq!(slices, vec!["\u{fe0f}a", "b", "c"]);
        let joined: String = slices.concat();
        assert_eq!(joined, text, "the ranges tile the source from byte zero");
    }

    #[test]
    fn the_review_projection_keeps_distinct_inputs_distinct() {
        // Findings B and C's class: two strings differing only by a
        // composition input strip to one preview, so a permission
        // sheet could not tell the scopes or commands apart. The
        // review walk renders each removed input as its escape, and
        // the previews stay distinct while plain text passes through
        // untouched.
        let bare = "echo '\u{2714}'";
        let selected = "echo '\u{2714}\u{fe0f}'";
        assert_eq!(for_display(bare), for_display(selected));
        assert_ne!(for_review(bare), for_review(selected));
        assert!(for_review(selected).contains("\\u{FE0F}"));
        assert_eq!(for_review(bare), "echo '\u{2714}'");
        let joined = "\u{1f468}\u{200d}\u{1f469}";
        assert!(for_review(joined).contains("\\u{200D}"));
        // The escaped preview is plain measurable text: no
        // composition input survives it.
        for cluster in for_review(joined).graphemes(true) {
            assert_eq!(
                str_width(cluster),
                cluster.chars().map(char_cells).sum::<usize>()
            );
        }
    }

    #[test]
    fn the_review_projection_escapes_literal_backslashes_too() {
        // A removed code point renders as its escape, so a command
        // carrying the literal escape spelling must not share that
        // preview with the command carrying the real code point —
        // the backslash doubles and the two previews read apart.
        let real = "echo '\u{2714}\u{fe0f}'";
        let spelled = "echo '\u{2714}\\u{FE0F}'";
        assert_eq!(
            for_review(real),
            "echo '\u{2714}\\u{FE0F}'",
            "the removed selector renders as its escape"
        );
        assert_ne!(
            for_review(real),
            for_review(spelled),
            "the real code point and its literal spelling must not share a preview"
        );
        assert_eq!(
            for_review(spelled),
            "echo '\u{2714}\\\\u{FE0F}'",
            "the literal backslash doubles in the preview"
        );
    }

    #[test]
    fn a_budgeted_head_keeps_whole_graphemes() {
        assert_eq!(
            prefix_within("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}", 4),
            "\u{4e2d}\u{6587}"
        );
        assert_eq!(prefix_within("hello", 3), "hel");
        assert_eq!(prefix_within("hello", 9), "hello");
        assert_eq!(prefix_within("\u{4e2d}\u{6587}", 1), "");
        assert_eq!(prefix_within("e\u{301}", 1), "e\u{301}");
    }

    #[test]
    fn a_budgeted_tail_keeps_whole_graphemes() {
        assert_eq!(
            suffix_within("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}", 4),
            "\u{6d4b}\u{8bd5}"
        );
        assert_eq!(suffix_within("hello", 3), "llo");
        assert_eq!(suffix_within("hello", 9), "hello");
        assert_eq!(suffix_within("e\u{301}", 1), "e\u{301}");
    }
}
