//! The live streaming region: the per-frame refresh, the freeze
//! that caches settled lines, and the segment parsing the freeze is
//! bounded by.
use std::collections::hash_map::DefaultHasher;

use super::TuiApp;

use super::notices::plain_wrapped_lines;
use crate::markdown;
use ratatui::text::Line;
use std::hash::{Hash, Hasher};
use std::sync::PoisonError;

/// The streaming region's cache: the frozen prefix's rendered lines
/// beside the live segment's, with the stamps that keep each half
/// honest.
///
/// One value owns everything a frame can reuse between stream deltas
/// — the settled blocks rendered once at their boundary, the live
/// segment's rows keyed by content and width, and the freeze offset
/// both are measured from — so a keystroke or an idle tick costs
/// nothing and a delta, a resize, or a cleared buffer is the only
/// thing that rebuilds any of it.
#[derive(Default)]
pub(crate) struct FreezeCache {
    /// Rendered lines of the streaming buffer's frozen prefix.
    ///
    /// Blocks before the frozen offset parsed and rendered once, when
    /// their terminating boundary arrived; frames re-use these lines
    /// instead of re-parsing settled content.
    pub(crate) frozen_lines: Vec<Line<'static>>,

    /// Where the streaming buffer's frozen prefix ends, in bytes.
    ///
    /// A safe boundary: the start of the line after a blank line,
    /// outside any fence, where the surrounding blocks cannot
    /// continue each other. Reset alongside the cache whenever the
    /// frozen prefix stops matching the buffer or the pane width
    /// changes.
    pub(crate) frozen_upto: usize,

    /// Separator rows owed after the frozen prefix.
    ///
    /// The blank run an interior boundary consumed becomes empty
    /// rows at the next join, matching the batch render's spacing
    /// for blank-separated blocks; a boundary taken at a closed
    /// fence owes the renderer's one inter-component row even
    /// though the source carried no blank, so the count floors at
    /// one. A run that precedes any frozen content is the
    /// document-leading run and follows the batch's absorb-one rule
    /// instead. Footnote definitions, rule-adjacent boundaries,
    /// indented fences inside lists, and a fence closer followed
    /// directly by a heading or the next list item render
    /// one-to-two rows off their batch spacing at the join — an
    /// accepted mid-stream approximation that self-corrects at
    /// graduation.
    pub(crate) frozen_separators: usize,

    /// Identity stamp of the frozen prefix.
    ///
    /// Recomputed whenever the freeze advances and re-checked on
    /// every layout: a buffer cleared and refilled to a comparable
    /// length fails the stamp and resets the cache, which a length
    /// comparison alone cannot distinguish from growth.
    pub(crate) frozen_fingerprint: u64,

    /// The pane width the streaming cache was built for.
    ///
    /// A resize discards the cache; the next layout re-freezes the
    /// same content at the new width.
    pub(crate) stream_cache_width: u16,

    /// Rendered lines of the streaming buffer's live segment.
    ///
    /// The live segment — the unfrozen complete lines plus the
    /// unterminated tail — only changes when a delta arrives, the
    /// freeze advances, or the pane resizes; input keystrokes and
    /// idle ticks re-render it identically. Frames re-use these
    /// lines until the segment's stamp moves, so a keystroke never
    /// pays the markdown re-parse (a live open code fence
    /// re-highlighted per frame costs frame-budget-breaking time).
    pub(crate) live_lines: Vec<Line<'static>>,

    /// Stamp of the live segment the cached lines were built from.
    ///
    /// A fingerprint of the unfrozen buffer suffix — content, not
    /// just length, so a cleared-and-refilled turn invalidates.
    pub(crate) live_stamp: u64,

    /// The freeze offset the live cache was built at.
    ///
    /// A live rebuild is valid only for the frozen prefix it
    /// started from; when the freeze advances, the cached lines
    /// describe a different suffix and this key forces the rebuild.
    pub(crate) live_frozen_upto: usize,

    /// The pane width the live cache was built at.
    ///
    /// Wrapped live lines are width-shaped; a resize re-wraps
    /// rather than reusing, so a narrowed pane never clips rows
    /// baked for the old width.
    pub(crate) live_width: u16,
}

impl FreezeCache {
    /// The streaming region's live rows: everything after the
    /// frozen prefix.
    ///
    /// Three parts, assembled in order: the cached lines of frozen
    /// blocks (settled content, rendered once when its terminating
    /// boundary arrived — after a blank run, or after a top-level
    /// fence closes); the live segment — every complete line since
    /// the freeze, re-parsed as markdown so each block renders as
    /// soon as its lines complete (an open code fence is closed
    /// synthetically, so in-flight code renders as the framed block
    /// instead of a wrapped paragraph; a block longer than
    /// [`LIVE_PARSE_LINE_CAP`] lines or [`LIVE_PARSE_BYTE_CAP`]
    /// bytes keeps only its trailing cap as markdown and its head
    /// renders as plaintext until it freezes, bounding the
    /// per-frame parse); and the unterminated tail — the last
    /// line of what the freeze has not yet consumed, as
    /// width-wrapped plaintext, so a closer the frozen block
    /// already rendered never draws twice while growth after it
    /// appears as it arrives. An empty buffer renders nothing.
    /// Returns whether the region's rendered lines changed this
    /// call — the signal that the line numbering below the settled
    /// transcript moved.
    pub(crate) fn refresh(
        &mut self,
        buffer: &str,
        width: u16,
        markdown_theme: &markdown::MarkdownTheme,
        syntax_theme: &markdown::SyntaxTheme,
        base: ratatui::style::Color,
    ) -> bool {
        if buffer.is_empty() {
            return self.reset();
        }
        let mut changed = false;
        if self.stream_cache_width != width {
            changed = self.reset();
            self.stream_cache_width = width;
        } else if self.frozen_upto > 0
            && buffer
                .get(..self.frozen_upto)
                .is_none_or(|prefix| fingerprint(prefix) != self.frozen_fingerprint)
        {
            changed = self.reset();
        }
        let frozen_before = self.frozen_lines.len();
        self.advance_freeze(buffer, width, markdown_theme, syntax_theme, base);
        if self.frozen_lines.len() != frozen_before {
            changed = true;
        }

        let unfrozen = buffer.get(self.frozen_upto..).unwrap_or("");
        let live = unfrozen
            .rsplit_once('\n')
            .map_or(unfrozen, |(_, tail)| tail);
        let live_md = unfrozen
            .get(..unfrozen.len().saturating_sub(live.len()))
            .filter(|segment| !segment.is_empty());
        let stamp = fingerprint(unfrozen);
        if self.live_stamp == stamp
            && self.live_frozen_upto == self.frozen_upto
            && self.live_width == width
        {
            return changed;
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        let leading = live_md.map_or(0, |segment| split_blank_prefix(segment).0);
        if !self.frozen_lines.is_empty() && (live_md.is_some() || !live.is_empty()) {
            let gap = self.frozen_separators.max(leading);
            for _ in 0..gap {
                lines.push(Line::from(""));
            }
        }
        if let Some(segment) = live_md {
            let (_, core) = split_blank_prefix(segment);
            let (line_head, recent) = split_live_segment(core, LIVE_PARSE_LINE_CAP);
            let (older, recent) = bound_recent_bytes(line_head, recent, LIVE_PARSE_BYTE_CAP);
            let md_at_buffer_start = self.frozen_upto == 0 && leading == 0 && older.is_empty();
            if !older.is_empty() {
                lines.extend(plain_wrapped_lines(&older, usize::from(width), base));
            }
            let mut source = String::new();
            if !md_at_buffer_start {
                source.push('\n');
            }
            if let Some(marker) = open_fence_closer(&older) {
                source.push_str(marker);
                source.push('\n');
                source.push_str(recent);
                source.push('\n');
                source.push_str(marker);
            } else {
                source.push_str(recent);
                if let Some(marker) = open_fence_closer(recent) {
                    source.push('\n');
                    source.push_str(marker);
                }
            }
            lines.extend(markdown::render_markdown(
                &source,
                width,
                markdown_theme,
                syntax_theme,
                base,
                None,
            ));
        }
        if !live.is_empty() {
            lines.extend(plain_wrapped_lines(live, usize::from(width), base));
        }
        self.live_stamp = stamp;
        self.live_frozen_upto = self.frozen_upto;
        self.live_width = width;
        self.live_lines = lines;
        true
    }

    /// Reset the cache.
    ///
    /// Drops the frozen lines, offset, and separator count; the next
    /// layout re-freezes from the buffer's start. Called when the
    /// buffer empties (the reply graduated or the turn failed), its
    /// frozen prefix stops matching, the pane width changes, or the
    /// session's renders are invalidated wholesale. Returns whether
    /// any rendered lines were dropped.
    pub(crate) fn reset(&mut self) -> bool {
        let dropped = !(self.frozen_lines.is_empty() && self.live_lines.is_empty());
        self.frozen_lines.clear();
        self.frozen_upto = 0;
        self.frozen_separators = 0;
        self.frozen_fingerprint = 0;
        self.live_stamp = 0;
        self.live_frozen_upto = 0;
        self.live_width = 0;
        self.live_lines.clear();
        dropped
    }

    /// Freeze every newly settled block into the cache.
    ///
    /// Extends [`frozen_upto`](Self::frozen_upto) to the last safe
    /// boundary in the buffer and appends the newly frozen slice's
    /// rendered lines. The slice's leading blank run belongs to the
    /// join gap, not the slice's own render: the gap rows pushed
    /// before the lines are the larger of the pending separator
    /// count and that run, so a boundary taken at a closed fence
    /// and a following blank run never double-count. When no
    /// visible line is cached yet the run is the document-leading
    /// one and the batch's absorb-one rule applies instead. A safe
    /// boundary sits at the start of the line after a blank line or
    /// after a top-level fence closes, outside any code fence, where
    /// the blocks on either side cannot continue each other — so the
    /// slice parses standalone exactly as it parses in place.
    fn advance_freeze(
        &mut self,
        buffer: &str,
        width: u16,
        markdown_theme: &markdown::MarkdownTheme,
        syntax_theme: &markdown::SyntaxTheme,
        base: ratatui::style::Color,
    ) {
        let boundary = last_safe_boundary(buffer, self.frozen_upto);
        if boundary <= self.frozen_upto {
            return;
        }
        let Some(slice) = buffer.get(self.frozen_upto..boundary) else {
            return;
        };
        let (leading, core) = split_blank_prefix(slice);
        let rendered =
            markdown::render_markdown(core, width, markdown_theme, syntax_theme, base, None);
        let gap = if self.frozen_lines.is_empty() {
            self.frozen_separators.saturating_sub(1)
        } else {
            self.frozen_separators.max(leading)
        };
        for _ in 0..gap {
            self.frozen_lines.push(Line::from(""));
        }
        self.frozen_lines.extend(rendered);
        self.frozen_upto = boundary;
        self.frozen_separators = if core.is_empty() && !self.frozen_lines.is_empty() {
            0
        } else {
            trailing_blank_lines(slice).max(1)
        };
        if let Some(prefix) = buffer.get(..boundary) {
            self.frozen_fingerprint = fingerprint(prefix);
        }
    }
}

impl TuiApp {
    /// Refresh the streaming region for this frame, if anything
    /// moved.
    ///
    /// The shell half of the refresh: it snapshots the shared
    /// streaming buffer under its lock and resolves the theme's
    /// markdown palettes once, then hands all three to the freeze
    /// cache — the state, the width, and the palette are the only
    /// things outside the cache the region's render reads. Returns
    /// whether the region's rendered lines changed, the signal the
    /// caller passes on for the selection's shape settle.
    pub(crate) fn refresh_streaming_region(&mut self, width: u16) -> bool {
        let buffer = self
            .state
            .streaming_text
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let markdown_theme = markdown::MarkdownTheme::from(&self.theme);
        let syntax_theme = markdown::SyntaxTheme::from(&self.theme);
        let base = self.theme.ui.assistant_message_fg;
        self.freeze
            .refresh(&buffer, width, &markdown_theme, &syntax_theme, base)
    }
}

/// The most lines of the live segment parsed as markdown per frame.
///
/// A single block that has not yet met a boundary is re-parsed whole
/// on every frame, so the parse cost grows with the entire unsettled
/// block; past this cap the block's head renders as plaintext until
/// it freezes, keeping the per-frame parse inside the frame budget.
const LIVE_PARSE_LINE_CAP: usize = 400;

/// The most bytes of the live segment parsed as markdown per frame.
///
/// Lines can be arbitrarily long — a minified code or data line
/// carries a whole block in one line — so the line cap alone does not
/// bound the parse. Past this byte cap the excess joins the
/// plaintext head, on a character boundary.
const LIVE_PARSE_BYTE_CAP: usize = 16_384;

/// How a non-blank line can continue a block across a blank line.
///
/// Only lists and quotes do in the grammar: a blank inside a loose
/// list or a multi-paragraph quote belongs to the block, so the
/// freeze scanner must not settle content there.
#[derive(Clone, Copy, PartialEq)]
enum ContLine {
    /// A `-`/`*`/numbered item line.
    ///
    /// Continues a loose list: the blank line above it separates
    /// items, not blocks, so the list stays open across it.
    List,

    /// A `>`-prefixed quote line.
    ///
    /// Continues a multi-paragraph quote: consecutive `>`-prefixed
    /// lines around a blank belong to one blockquote.
    Quote,

    /// Anything else.
    ///
    /// Starts a fresh block — a blank line above it closes whatever
    /// came before, and the scanner may settle content there.
    Other,
}

/// Classify a non-blank line's cross-blank continuation kind.
///
/// A lexical check, not a parse: leading spaces are skipped and the
/// line's first token is matched against the list and quote markers
/// the grammar accepts. The caller has already established the line
/// is non-blank.
fn cont_line_kind(line: &str) -> ContLine {
    let trimmed = line.trim_start_matches(' ');
    if trimmed.starts_with('>') {
        return ContLine::Quote;
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    let list = trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || (digits > 0
            && trimmed
                .get(digits..digits.saturating_add(2))
                .is_some_and(|marker| marker == ". "));
    if list {
        ContLine::List
    } else {
        ContLine::Other
    }
}

/// The fence marker a line opens or closes, if it is a fence line.
///
/// Leading spaces and trailing line ends are ignored, matching the
/// grammar's `" "*` around fence markers.
fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start_matches(' ');
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// The closing fence a segment still needs, if it ends inside one.
///
/// Tracks fence opens and closes line by line; a fence left open at
/// the segment's end reports its marker so the caller can close it
/// synthetically before parsing, letting in-flight code render as
/// the framed block.
fn open_fence_closer(segment: &str) -> Option<&'static str> {
    let mut open: Option<&'static str> = None;
    for line in segment.split_inclusive('\n') {
        let Some(marker) = fence_marker(line.trim_end_matches(['\n', '\r'])) else {
            continue;
        };
        if open == Some(marker) {
            open = None;
        } else if open.is_none() {
            open = Some(marker);
        }
    }
    open
}

/// The last offset at or after `from` where the buffer's earlier
/// content is settled.
///
/// A safe boundary is the start of a non-blank line that follows a
/// blank line — found outside any code fence, where the non-blank
/// lines flanking the blank cannot continue the same block — or the
/// line after a top-level fence closes: a closed fence cannot be
/// continued, so everything through its closer is settled even
/// without a following blank. A fence opened while a list or quote
/// was in progress is list-nested and gets no close-boundary; the
/// surrounding block may continue past it. Returns `from` when no
/// later boundary exists.
fn last_safe_boundary(buffer: &str, from: usize) -> usize {
    let tail = buffer.get(from..).unwrap_or("");
    let mut boundary = from;
    let mut offset = from;
    let mut fence: Option<&'static str> = None;
    let mut kind_before_fence = ContLine::Other;
    let mut prev_kind = ContLine::Other;
    let mut pending_blank = false;
    for line in tail.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if let Some(marker) = fence_marker(text) {
            if fence == Some(marker) {
                fence = None;
                if kind_before_fence == ContLine::Other {
                    boundary = offset.saturating_add(line.len());
                }
            } else if fence.is_none() {
                fence = Some(marker);
                kind_before_fence = prev_kind;
            }
            prev_kind = ContLine::Other;
            pending_blank = false;
        } else if fence.is_some() || text.trim().is_empty() {
            if fence.is_none() {
                pending_blank = true;
            }
        } else {
            let kind = cont_line_kind(text);
            if pending_blank
                && !matches!(
                    (prev_kind, kind),
                    (ContLine::List, ContLine::List) | (ContLine::Quote, ContLine::Quote)
                )
            {
                boundary = offset;
            }
            prev_kind = kind;
            pending_blank = false;
        }
        offset = offset.saturating_add(line.len());
    }
    boundary
}

/// Bound the markdown part by bytes as well as lines.
///
/// A single line can carry an entire minified block, so the line cap
/// alone leaves the parse unbounded; past the byte cap the excess
/// moves into the plaintext head, cut on a character boundary so the
/// markdown part always starts on one.
fn bound_recent_bytes<'a>(older: &str, recent: &'a str, cap: usize) -> (String, &'a str) {
    if recent.len() <= cap {
        return (older.to_string(), recent);
    }
    let mut cut = recent.len().saturating_sub(cap);
    while !recent.is_char_boundary(cut) {
        cut = cut.saturating_add(1);
    }
    let mut head = String::from(older);
    head.push_str(recent.get(..cut).unwrap_or(""));
    (head, recent.get(cut..).unwrap_or(""))
}

/// Split a slice's leading blank run from its content.
///
/// Returns the run's line count and the remainder. A boundary taken
/// at a closed fence leaves the following blank run at the head of
/// the next slice; that run belongs to the join gap rather than the
/// slice's own render, so the caller folds it into the gap rows.
fn split_blank_prefix(slice: &str) -> (usize, &str) {
    let mut count: usize = 0;
    let mut offset: usize = 0;
    for line in slice.split_inclusive('\n') {
        if !line.trim().is_empty() {
            break;
        }
        count = count.saturating_add(1);
        offset = offset.saturating_add(line.len());
    }
    (count, slice.get(offset..).unwrap_or(""))
}

/// Split the live segment at the parse cap.
///
/// Returns the over-cap head and the trailing cap of lines; the head
/// renders as plaintext and the tail parses as markdown, bounding
/// the per-frame parse while a single long block streams. A segment
/// at or under the cap returns an empty head.
fn split_live_segment(segment: &str, cap: usize) -> (&str, &str) {
    let cap = cap.max(1);
    let split = segment
        .rmatch_indices('\n')
        .nth(cap.saturating_sub(1))
        .map(|(index, _)| index);
    match split {
        Some(index) => {
            let head = segment.get(..index).unwrap_or("");
            let tail = segment.get(index.saturating_add(1)..).unwrap_or("");
            (head, tail)
        }
        None => ("", segment),
    }
}

/// Count the blank lines a frozen slice ends with.
///
/// The blank run before a boundary is the separator the batch render
/// shows between the blocks on either side; the cache re-inserts
/// exactly this many empty rows at the join, however many blank
/// lines the source carried.
fn trailing_blank_lines(slice: &str) -> usize {
    slice
        .lines()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count()
}
/// A cheap identity stamp for a text prefix.
///
/// Within a turn the streaming buffer only grows, but a turn that
/// cleared and refilled the buffer can reach the old frozen length
/// again; the stamp tells that replacement apart from growth where a
/// length comparison cannot.
pub(crate) fn fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
