//! Markdown rendering into styled ratatui lines.
//!
//! Walks the parser's laid-out component tree and projects every word
//! into spans through the module's themes; block structure — blank
//! separation between blocks, quote prefixes, table borders, code
//! framing — is assembled here. The renderer is stateless: callers own
//! any caching.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::parser::parse_markdown;
use super::text_component::{TextComponent, TextNode};
use super::theme::{MarkdownTheme, SyntaxTheme};
use super::word::{MetaData, Word, WordType};

/// Render a markdown string into styled terminal lines.
///
/// Parses `text` via the module's parser, projects every word through
/// the themes, and returns lines ready for
/// `ratatui::text::Text::from(lines)` or a `Paragraph`. Word-wrapping
/// is the parser's job at `wrap_width`; this function only styles and
/// assembles. Blocks are separated by a single blank line — except
/// task items, which continue the preceding block without a
/// separator, reading as inline checklist entries. `base_color` is
/// the foreground for unstyled words — the caller picks it per
/// message source, keeping this module free of agent concepts. When
/// `clip_width` is `Some`, every emitted line is clipped to that many
/// display columns.
#[must_use]
pub fn render_markdown(
    text: &str,
    wrap_width: u16,
    markdown_theme: &MarkdownTheme,
    syntax_theme: &SyntaxTheme,
    base_color: Color,
    clip_width: Option<usize>,
) -> Vec<Line<'static>> {
    let root = parse_markdown(text, wrap_width);
    let base_style = Style::default().fg(base_color);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut prev_emitted = false;

    for component in root.components() {
        match component.kind() {
            TextNode::Heading => {
                let level = heading_level(component);
                let header_style = markdown_theme
                    .header
                    .get(level.saturating_sub(1))
                    .map_or(base_style, |header| base_style.patch(*header));
                push_blank_between(&mut lines, prev_emitted);
                push_word_rows(component, header_style, markdown_theme, &mut lines);
                prev_emitted = true;
            }
            TextNode::Paragraph | TextNode::List => {
                push_blank_between(&mut lines, prev_emitted);
                push_word_rows(component, base_style, markdown_theme, &mut lines);
                prev_emitted = true;
            }
            TextNode::CodeBlock => {
                push_blank_between(&mut lines, prev_emitted);
                let language = component
                    .meta_info()
                    .iter()
                    .find_map(|w| match w.kind() {
                        WordType::MetaInfo(MetaData::PLanguage) => Some(w.content().to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let code = component.content_as_lines().join("\n");
                render_code_block(&code, &language, markdown_theme, syntax_theme, &mut lines);
                prev_emitted = true;
            }
            TextNode::Quote => {
                push_blank_between(&mut lines, prev_emitted);
                let prefix_style = Style::default().fg(markdown_theme.dim);
                let quote_style = base_style.patch(markdown_theme.quote);
                for row in component.content() {
                    let mut spans = vec![Span::styled("│ ", prefix_style)];
                    for word in row {
                        spans.extend(word_to_spans(word, quote_style, markdown_theme));
                    }
                    lines.push(Line::from(spans));
                }
                prev_emitted = true;
            }
            TextNode::Task => {
                push_word_rows(component, base_style, markdown_theme, &mut lines);
                prev_emitted = true;
            }
            TextNode::HorizontalSeparator => {
                push_blank_between(&mut lines, prev_emitted);
                let rule = "─".repeat(usize::from(wrap_width).min(40));
                lines.push(Line::from(Span::styled(
                    rule,
                    markdown_theme.horizontal_rule,
                )));
                prev_emitted = true;
            }
            TextNode::LineBreak => {
                lines.push(Line::from(Span::raw("")));
                prev_emitted = false;
            }
            TextNode::Table(_, _) => {
                push_blank_between(&mut lines, prev_emitted);
                if let TextNode::Table(widths, _) = component.kind()
                    && !widths.is_empty()
                {
                    render_table(component, markdown_theme, base_style, &mut lines);
                } else {
                    for plain in component.content_as_lines() {
                        lines.push(Line::from(Span::styled(plain, base_style)));
                    }
                }
                prev_emitted = true;
            }
            TextNode::Footnote => {}
        }
    }

    while lines.last().is_some_and(is_blank) {
        lines.pop();
    }

    match clip_width {
        Some(max_width) => lines
            .into_iter()
            .map(|line| clip_line(line, max_width))
            .collect(),
        None => lines,
    }
}

/// The heading's level, clamped into the one-to-six style range.
///
/// Stored in metadata by the parser as [`MetaData::HeadingLevel`]; a
/// missing marker renders as level one.
fn heading_level(component: &TextComponent) -> usize {
    let level = component
        .meta_info()
        .iter()
        .find_map(|w| match w.kind() {
            WordType::MetaInfo(MetaData::HeadingLevel(level)) => Some(usize::from(level)),
            _ => None,
        })
        .unwrap_or(1);
    level.clamp(1, 6)
}

/// Push the single blank separator between blocks, if one is due.
///
/// Only interior boundaries get a blank line — never a leading one —
/// so consecutive blocks read separated without doubling explicit
/// breaks.
fn push_blank_between(lines: &mut Vec<Line<'static>>, prev_emitted: bool) {
    if prev_emitted {
        lines.push(Line::from(Span::raw("")));
    }
}

/// Project a component's rows of words into styled lines.
///
/// Empty rows are skipped, matching the parser's padding-only rows in
/// table-adjacent components.
fn push_word_rows(
    component: &TextComponent,
    base_style: Style,
    theme: &MarkdownTheme,
    lines: &mut Vec<Line<'static>>,
) {
    for row in component.content() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for word in row {
            spans.extend(word_to_spans(word, base_style, theme));
        }
        if !spans.is_empty() {
            lines.push(Line::from(spans));
        }
    }
}

/// Map a single parsed word to its spans.
///
/// The word's theme style patches the block-level `base_style`, so a
/// theme style that omits a property inherits the base (the message
/// color survives) while one that sets it overrides.
fn word_to_spans(word: &Word, base_style: Style, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    let content = word.content().to_string();
    match word.kind() {
        WordType::Bold | WordType::BoldItalic => vec![Span::styled(
            content,
            base_style.patch(theme.bold).add_modifier(Modifier::BOLD),
        )],
        WordType::Italic => vec![Span::styled(
            content,
            base_style
                .patch(theme.italic)
                .add_modifier(Modifier::ITALIC),
        )],
        WordType::Code => vec![Span::styled(content, base_style.patch(theme.code_inline))],
        WordType::Strikethrough => vec![Span::styled(
            content,
            base_style.add_modifier(Modifier::CROSSED_OUT),
        )],
        WordType::Link => vec![Span::styled(
            content,
            base_style
                .patch(theme.link)
                .add_modifier(Modifier::UNDERLINED),
        )],
        WordType::LinkData => vec![Span::styled(content, base_style.fg(theme.dim))],
        WordType::ListMarker => vec![Span::styled(content, base_style.patch(theme.list_item))],
        _ => vec![Span::styled(content, base_style)],
    }
}

/// Render a code block, one framed line per source line.
///
/// Every line is wrapped in a leading and trailing space column
/// painted in the code-block background so the block reads as a solid
/// panel. With the `syntax-highlight` feature the tree-sitter
/// highlighter runs here over the block's source; without it every
/// line renders plain.
fn render_code_block(
    code: &str,
    language: &str,
    markdown_theme: &MarkdownTheme,
    syntax_theme: &SyntaxTheme,
    lines: &mut Vec<Line<'static>>,
) {
    let panel_bg = markdown_theme.code_block;
    let plain_fg = syntax_theme.plain;

    #[cfg(feature = "syntax-highlight")]
    {
        use super::highlight::{HighlightInfo, highlight_code};

        match highlight_code(language, code.as_bytes()) {
            HighlightInfo::Highlighted(events) => {
                let emit_colors = syntax_theme.emit_order_colors();
                let source = code.as_bytes();
                let leading = || Span::styled(" ", Style::default().bg(panel_bg));
                let mut current_line: Vec<Span<'static>> = vec![leading()];
                let mut highlight_stack: Vec<Color> = vec![plain_fg];

                for event in &events {
                    match event {
                        tree_sitter_highlight::HighlightEvent::Source { start, end } => {
                            let chunk = source
                                .get(*start..*end)
                                .and_then(|bytes| std::str::from_utf8(bytes).ok());
                            if let Some(chunk) = chunk {
                                let color = highlight_stack.last().copied().unwrap_or(plain_fg);
                                let style = Style::default().fg(color).bg(panel_bg);
                                for (i, line_str) in chunk.split('\n').enumerate() {
                                    if i > 0 {
                                        current_line.push(leading());
                                        lines.push(Line::from(std::mem::take(&mut current_line)));
                                        current_line = vec![leading()];
                                    }
                                    if !line_str.is_empty() {
                                        current_line
                                            .push(Span::styled(line_str.to_string(), style));
                                    }
                                }
                            }
                        }
                        tree_sitter_highlight::HighlightEvent::HighlightStart(idx) => {
                            let color = emit_colors.get(idx.0).copied().unwrap_or(plain_fg);
                            highlight_stack.push(if color == Color::Reset {
                                plain_fg
                            } else {
                                color
                            });
                        }
                        tree_sitter_highlight::HighlightEvent::HighlightEnd => {
                            highlight_stack.pop();
                        }
                    }
                }

                if current_line.len() > 1 {
                    current_line.push(leading());
                    lines.push(Line::from(current_line));
                }
            }
            HighlightInfo::Unhighlighted => {
                render_plain_code_block(code, panel_bg, plain_fg, lines);
            }
        }
    }
    #[cfg(not(feature = "syntax-highlight"))]
    {
        let _ = language;
        render_plain_code_block(code, panel_bg, plain_fg, lines);
    }
}

/// Render code lines with one plain color on the block background.
///
/// The fallback every unhighlighted path converges on: unknown
/// language, missing grammar, highlighting failure, or the feature
/// turned off.
fn render_plain_code_block(
    code: &str,
    panel_bg: Color,
    plain_fg: Color,
    lines: &mut Vec<Line<'static>>,
) {
    for code_line in code.lines() {
        let mut spans = vec![Span::styled(" ", Style::default().bg(panel_bg))];
        if !code_line.is_empty() {
            spans.push(Span::styled(
                code_line.to_string(),
                Style::default().fg(plain_fg).bg(panel_bg),
            ));
        }
        spans.push(Span::styled(" ", Style::default().bg(panel_bg)));
        lines.push(Line::from(spans));
    }
}

/// Render a table with box-drawing borders, cell padding, and
/// per-word cell styling.
///
/// The parser lays content out per data row: for each row `r`, each
/// column `c` owns `row_heights[r]` sub-rows. The flat index of
/// `(r, c, sub)` is `cumulative_height[r] * column_count + c *
/// row_heights[r] + sub`, which this walk rebuilds from the recorded
/// heights.
fn render_table(
    component: &TextComponent,
    theme: &MarkdownTheme,
    base_style: Style,
    lines: &mut Vec<Line<'static>>,
) {
    let TextNode::Table(column_widths, row_heights) = component.kind() else {
        return;
    };
    let column_count = column_widths.len();
    if column_count == 0 || row_heights.is_empty() {
        return;
    }

    let border = Style::default().fg(theme.border);
    let header_style = base_style.add_modifier(Modifier::BOLD);
    let content = component.content();

    let mut cumulative_height: Vec<usize> = Vec::with_capacity(row_heights.len().saturating_add(1));
    cumulative_height.push(0);
    for &height in row_heights {
        let prev = cumulative_height.last().copied().unwrap_or(0);
        cumulative_height.push(prev.saturating_add(usize::from(height)));
    }

    lines.push(Line::from(border_spans(
        "┌",
        "┬",
        "┐",
        column_widths,
        border,
    )));

    for (row_idx, &max_h) in row_heights.iter().enumerate() {
        let is_header = row_idx == 0;
        let style = if is_header { header_style } else { base_style };

        for sub in 0..usize::from(max_h) {
            let mut row_spans = vec![Span::styled("│", border)];
            let empty: Vec<Word> = Vec::new();

            for col_idx in 0..column_count {
                let base = cumulative_height
                    .get(row_idx)
                    .copied()
                    .unwrap_or(0)
                    .saturating_mul(column_count);
                let idx = base.saturating_add(
                    col_idx
                        .saturating_mul(usize::from(max_h))
                        .saturating_add(sub),
                );
                let cell_words = content.get(idx).unwrap_or(&empty);
                let cell_text: String = cell_words.iter().map(Word::content).collect();
                let cell_width = UnicodeWidthStr::width(cell_text.as_str());
                let target_width = usize::from(column_widths.get(col_idx).copied().unwrap_or(0));

                row_spans.push(Span::styled(" ", style));
                if cell_width > target_width {
                    row_spans.push(Span::styled(
                        unicode_truncate(&cell_text, target_width),
                        style,
                    ));
                } else {
                    for word in cell_words {
                        row_spans.extend(word_to_spans(word, style, theme));
                    }
                    let pad = target_width.saturating_sub(cell_width);
                    if pad > 0 {
                        row_spans.push(Span::styled(" ".repeat(pad), style));
                    }
                }
                row_spans.push(Span::styled(" ", style));

                if col_idx.saturating_add(1) < column_count {
                    row_spans.push(Span::styled("│", border));
                }
            }

            row_spans.push(Span::styled("│", border));
            lines.push(Line::from(row_spans));
        }

        if is_header {
            lines.push(Line::from(border_spans(
                "├",
                "┼",
                "┤",
                column_widths,
                border,
            )));
        }
    }

    lines.push(Line::from(border_spans(
        "└",
        "┴",
        "┘",
        column_widths,
        border,
    )));
}

/// Assemble one horizontal table border line.
///
/// Each column contributes its width plus two padding columns of `─`,
/// joined by `join` glyphs and closed by `left`/`right` corners.
fn border_spans(
    left: &'static str,
    join: &'static str,
    right: &'static str,
    column_widths: &[u16],
    style: Style,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(left, style)];
    for (col_idx, &width) in column_widths.iter().enumerate() {
        spans.push(Span::styled(
            "─".repeat(usize::from(width).saturating_add(2)),
            style,
        ));
        if col_idx.saturating_add(1) < column_widths.len() {
            spans.push(Span::styled(join, style));
        }
    }
    spans.push(Span::styled(right, style));
    spans
}

/// Truncate a string to at most `max_width` display columns.
///
/// Stops before the first character that would cross the budget, so
/// wide glyphs and multi-byte characters are never split.
#[must_use]
fn unicode_truncate(text: &str, max_width: usize) -> String {
    let mut width: usize = 0;
    let mut chars = String::new();
    for c in text.chars() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if width.saturating_add(char_width) > max_width {
            break;
        }
        width = width.saturating_add(char_width);
        chars.push(c);
    }
    chars
}

/// Clip a line to at most `max_width` display columns.
///
/// Spans that fit pass through untouched; the first span that crosses
/// the budget is truncated (styles preserved) and the rest are
/// dropped. This is the last-resort guard against wide spans
/// overflowing the pane the caller lays out.
#[must_use]
fn clip_line(line: Line<'static>, max_width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;

    for span in line.spans {
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        let remaining = max_width.saturating_sub(used);

        if remaining == 0 {
            break;
        }

        if span_width <= remaining {
            used = used.saturating_add(span_width);
            spans.push(span);
        } else {
            let truncated: String = span
                .content
                .chars()
                .scan(0usize, |width, c| {
                    let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
                    *width = width.saturating_add(char_width);
                    if *width <= remaining { Some(c) } else { None }
                })
                .collect();
            spans.push(Span::styled(truncated, span.style));
            break;
        }
    }

    Line::from(spans)
}

/// Whether a line carries no visible content.
fn is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::missing_panics_doc,
        clippy::missing_errors_doc
    )]

    use super::*;

    #[test]
    fn unicode_truncate_stops_at_the_width() {
        assert_eq!(unicode_truncate("hello", 3), "hel");
        assert_eq!(unicode_truncate("hello", 10), "hello");
        assert_eq!(unicode_truncate("", 5), "");
        assert_eq!(unicode_truncate("中文测试", 4), "中文");
    }

    #[test]
    fn clip_line_keeps_styles_and_width() {
        let line = Line::from(vec![
            Span::styled("abcd".to_string(), Style::default().fg(Color::Red)),
            Span::styled("efgh".to_string(), Style::default().fg(Color::Blue)),
        ]);
        let clipped = clip_line(line, 5);
        assert_eq!(clipped.spans.len(), 2);
        assert_eq!(clipped.spans[0].content.as_ref(), "abcd");
        assert_eq!(clipped.spans[1].content.as_ref(), "e");
        assert_eq!(clipped.spans[1].style.fg, Some(Color::Blue));
    }

    #[test]
    fn clip_line_respects_wide_glyphs() {
        let line = Line::from(Span::raw("中文中文"));
        let clipped = clip_line(line, 3);
        let width = UnicodeWidthStr::width(clipped.spans[0].content.as_ref());
        assert!(width <= 3, "clipped width {width} exceeds 3");
    }
}
