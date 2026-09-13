//! Block-level components and their width-driven layout.
//!
//! A [`TextComponent`] is one markdown block — paragraph, heading, code
//! block, list, quote, table — parsed into rows of styled words and
//! laid out to a terminal width by the transforms in this module.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::word::{MetaData, Word, WordType};

/// Block-level node kinds in markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextNode {
    /// A paragraph of flowing text.
    ///
    /// Flowing text the layout wraps at the width; the most common block.
    Paragraph,

    /// A heading line at any level.
    ///
    /// The level itself travels in the component's metadata.
    Heading,

    /// A fenced or indented code block.
    ///
    /// Source lines are kept verbatim; the fence's language rides in metadata.
    CodeBlock,

    /// A list, ordered or unordered, at any nesting.
    ///
    /// Items carry their own markers and indentation metadata; the layout renumbers and indents.
    List,

    /// A blockquote.
    ///
    /// Rendered with a bar and wrapped against a narrower inner width.
    Quote,

    /// A GFM task-list item.
    ///
    /// A checkbox item; the marker word records its open or completed state.
    Task,

    /// A blank line between blocks.
    ///
    /// Contributes one blank visual row between the blocks around it.
    LineBreak,

    /// A horizontal rule.
    ///
    /// Contributes one rule row the renderer paints with the rule style.
    HorizontalSeparator,

    /// A table carrying its computed column widths and row heights.
    ///
    /// Filled by the layout transform: `widths_by_column` after
    /// column fitting, `heights_by_row` after cell wrapping. An empty
    /// pair marks a degenerate table the transform could not fit.
    Table(Vec<u16>, Vec<u16>),

    /// A footnote definition.
    ///
    /// Pairs a non-renderable label word with the body text that follows it.
    Footnote,
}

/// One block-level component with its content words and layout info.
///
/// Owns the parsed words in visual rows, the non-renderable metadata
/// words (language, list kinds, column counts), and the height the
/// layout transform computed for the current width.
#[derive(Debug, Clone)]
pub struct TextComponent {
    /// The block's kind.
    ///
    /// Selects the layout transform and how the renderer paints the block.
    kind: TextNode,
    /// Rows of words; each row renders as one visual line.
    ///
    /// Built by the parse, reshaped by the layout transform; empty rows are meaningful in tables.
    content: Vec<Vec<Word>>,
    /// Non-renderable metadata words, such as language and list kinds.
    ///
    /// Languages, list kinds, indents, and column counts live here rather than in renderable rows.
    meta_info: Vec<Word>,
    /// Visual height after the layout transform.
    ///
    /// What the renderer allocates vertically; recomputed by every `transform` call.
    height: u16,
}

impl TextComponent {
    /// Create a component from a flat list of words.
    ///
    /// The words become a single row; non-renderable words move to the
    /// component's metadata.
    #[must_use]
    pub fn new(kind: TextNode, content: Vec<Word>) -> Self {
        let meta_info: Vec<Word> = content
            .iter()
            .filter(|w| !w.is_renderable() || w.kind() == WordType::FootnoteInline)
            .cloned()
            .collect();
        let content: Vec<Word> = content.into_iter().filter(Word::is_renderable).collect();
        Self {
            kind,
            content: vec![content],
            meta_info,
            height: 0,
        }
    }

    /// Create a component from pre-formatted multi-row content.
    ///
    /// For blocks whose rows are structural from the parse — code
    /// lines, list items, table cells.
    #[must_use]
    pub fn new_formatted(kind: TextNode, content: Vec<Vec<Word>>) -> Self {
        Self::new_formatted_with_meta(kind, content, Vec::new())
    }

    /// Create a component from multi-row content plus explicit metadata.
    ///
    /// Non-renderable words remaining in the rows are folded into the
    /// metadata alongside the explicit ones.
    #[must_use]
    pub fn new_formatted_with_meta(
        kind: TextNode,
        content: Vec<Vec<Word>>,
        mut meta_info: Vec<Word>,
    ) -> Self {
        meta_info.extend(
            content
                .iter()
                .flatten()
                .filter(|w| !w.is_renderable())
                .cloned(),
        );
        let content: Vec<Vec<Word>> = content
            .into_iter()
            .map(|row| row.into_iter().filter(Word::is_renderable).collect())
            .collect();
        Self {
            kind,
            height: u16::try_from(content.len()).unwrap_or(u16::MAX),
            meta_info,
            content,
        }
    }

    /// The block's kind.
    #[must_use]
    pub fn kind(&self) -> &TextNode {
        &self.kind
    }

    /// The component's rows of words, each row one visual line.
    #[must_use]
    pub fn content(&self) -> &Vec<Vec<Word>> {
        &self.content
    }

    /// The component's non-renderable metadata words.
    #[must_use]
    pub fn meta_info(&self) -> &[Word] {
        &self.meta_info
    }

    /// The component's visual height at the last transform.
    #[must_use]
    pub fn height(&self) -> u16 {
        self.height
    }

    /// All content joined into plain-text lines.
    ///
    /// Code-block rows carry a leading newline from the grammar, which
    /// is stripped here; interior blank lines are code and stay, so the
    /// syntax highlighter receives the true source. Table cells are
    /// reassembled per data row.
    #[must_use]
    pub fn content_as_lines(&self) -> Vec<String> {
        if let TextNode::Table(widths, row_heights) = &self.kind {
            let column_count = widths.len();
            if column_count == 0 {
                // Degenerate table (malformed/empty): each content row
                // stands as its own plain line.
                return self
                    .content
                    .iter()
                    .map(|row| row.iter().map(Word::content).collect::<String>())
                    .collect();
            }
            // Content is laid out per data row as `heights[r]` sub-rows
            // per cell; take each cell's first sub-row as its primary
            // text and join a row's cells with spaces.
            let mut result = Vec::new();
            let mut offset = 0;
            for max_h in row_heights {
                let mut row_parts: Vec<String> = Vec::new();
                for _col in 0..column_count {
                    if let Some(row) = self.content.get(offset) {
                        row_parts.push(row.iter().map(Word::content).collect::<String>());
                    }
                    offset = offset.saturating_add(*max_h as usize);
                }
                result.push(row_parts.join(" "));
            }
            result
        } else if let TextNode::CodeBlock = &self.kind {
            // Interior blank lines are code: the highlighter's source
            // must keep them, so only leading and trailing empties are
            // trimmed — each is the grammar's leading-newline artifact.
            let stripped: Vec<String> = self
                .content
                .iter()
                .map(|row| {
                    let text: String = row.iter().map(Word::content).collect();
                    text.strip_prefix('\n').unwrap_or(&text).to_string()
                })
                .collect();
            let Some(first) = stripped.iter().position(|line| !line.is_empty()) else {
                return Vec::new();
            };
            let Some(last) = stripped.iter().rposition(|line| !line.is_empty()) else {
                return Vec::new();
            };
            stripped
                .get(first..=last)
                .map_or_else(Vec::new, ToOwned::to_owned)
        } else {
            self.content
                .iter()
                .map(|row| row.iter().map(Word::content).collect::<String>())
                .collect()
        }
    }

    /// Re-run this block's layout for `width`.
    ///
    /// Dispatches by kind: wrapping for text blocks, indentation and
    /// renumbering for lists, column fitting for tables; code blocks
    /// and rules only fix their height.
    pub fn transform(&mut self, width: u16) {
        match &self.kind {
            TextNode::Paragraph | TextNode::Task | TextNode::Quote | TextNode::Heading => {
                transform_paragraph(self, width);
            }
            TextNode::List => {
                transform_list(self, width);
            }
            TextNode::CodeBlock => {
                transform_codeblock(self);
            }
            TextNode::LineBreak | TextNode::HorizontalSeparator => {
                self.height = 1;
            }
            TextNode::Table(_, _) => {
                transform_table(self, width);
            }
            TextNode::Footnote => {
                self.height = 0;
            }
        }
    }
}

/// Wrap a sequence of words to fit within `width` columns.
///
/// Words that fit accumulate on the current line; a word wider than the
/// whole width is hard-split at a character boundary and its remainder
/// continues on following lines.
#[must_use]
pub fn word_wrapping(words: &[Word], width: usize) -> Vec<Vec<Word>> {
    if width == 0 {
        return vec![words.to_vec()];
    }
    let mut lines: Vec<Vec<Word>> = Vec::new();
    let mut line: Vec<Word> = Vec::new();
    let mut line_len: usize = 0;

    for word in words {
        let word_len = display_width(word.content());
        if line_len.saturating_add(word_len) <= width {
            line_len = line_len.saturating_add(word_len);
            line.push(word.clone());
        } else if word_len <= width {
            lines.push(std::mem::take(&mut line));
            let mut word = word.clone();
            let trimmed = word.content().trim_start().to_owned();
            word.set_content(trimmed);
            line_len = display_width(word.content());
            line.push(word);
        } else {
            if width.saturating_sub(line_len) < 4 {
                lines.push(std::mem::take(&mut line));
                line_len = 0;
            }
            let split_width = width.saturating_sub(line_len);
            let (head, tail) = split_by_width(word.content(), split_width);
            if !head.is_empty() {
                line.push(Word::new(head, word.kind()));
                lines.push(std::mem::take(&mut line));
            }
            let mut remaining = tail;
            while display_width(&remaining) > width {
                let (head, tail) = split_by_width(&remaining, width);
                lines.push(vec![Word::new(head, word.kind())]);
                remaining = tail;
            }
            if remaining.is_empty() {
                line_len = 0;
            } else {
                line_len = display_width(&remaining);
                line = vec![Word::new(remaining, word.kind())];
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The terminal display width of `text`, counting wide characters.
///
/// Wide glyphs count as their terminal columns, not bytes or characters.
fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Split `text` at the widest prefix that fits `max_width` columns.
///
/// The split lands on a character boundary and the tail carries the
/// remainder. A single character wider than the budget is returned
/// alone — an unsplittable wide glyph owns its row — so the head can
/// exceed `max_width` by at most one character.
#[must_use]
fn split_by_width(text: &str, max_width: usize) -> (String, String) {
    if max_width == 0 {
        return (String::new(), text.to_string());
    }
    let mut width: usize = 0;
    let mut split_idx = 0;
    for (i, c) in text.char_indices() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if width.saturating_add(char_width) > max_width {
            if split_idx == 0 {
                split_idx = i.saturating_add(c.len_utf8());
            }
            break;
        }
        width = width.saturating_add(char_width);
        split_idx = i.saturating_add(c.len_utf8());
        if width == max_width {
            break;
        }
    }
    // `split_idx` only ever holds a `char_indices` boundary or 0.
    #[allow(clippy::string_slice)]
    let (head, tail) = text.split_at(split_idx);
    (head.to_string(), tail.to_string())
}

/// Lay out a paragraph-like component: wrap its words to the width.
///
/// Headings share the paragraph wrap; tasks and quotes wrap against a
/// narrower inner width to leave room for their markers and bars.
fn transform_paragraph(component: &mut TextComponent, width: u16) {
    let inner_width = match &component.kind {
        TextNode::Paragraph => width.saturating_sub(1) as usize,
        TextNode::Task => width.saturating_sub(4) as usize,
        TextNode::Quote => width.saturating_sub(2) as usize,
        _ => width as usize,
    };
    let lines = word_wrapping(
        &component
            .content
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>(),
        inner_width,
    );
    component.height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    component.content = lines;
}

/// Lay out a code-block component.
///
/// Code never re-wraps: rows already mirror source lines from the
/// parse, so layout only records the height. Syntax highlighting is a
/// render-time concern — the renderer tags code words with captures
/// and colors them through the syntax theme.
fn transform_codeblock(component: &mut TextComponent) {
    component.height = u16::try_from(component.content.len()).unwrap_or(u16::MAX);
}

/// Lay out a list: wrap items, indent them by nesting, renumber ordered
/// items sequentially.
///
/// Ordered items are renumbered from one per nesting level, which is
/// what repairs the all-ones numbering models emit; indentation comes
/// from the indent words recorded in the component's metadata.
fn transform_list(component: &mut TextComponent, width: u16) {
    let width = width as usize;
    let mut lines: Vec<Vec<Word>> = Vec::new();

    let indent_iter = component
        .meta_info
        .iter()
        .filter(|w| w.content().trim().is_empty() && !matches!(w.kind(), WordType::MetaInfo(_)));
    let list_type_iter = component.meta_info.iter().filter(|w| {
        matches!(
            w.kind(),
            WordType::MetaInfo(MetaData::UList | MetaData::OList)
        )
    });
    let mut zip_iter = indent_iter.zip(list_type_iter);

    let mut o_list_counter_stack = vec![0u32];
    let mut indent = 0;
    let mut tmp: usize = 0;
    let mut extra_indent = 0;

    let mut line: Vec<Word> = Vec::new();
    let mut len = 0;

    for word in component.content.iter_mut().flatten() {
        let word_len = display_width(word.content());
        if word_len.saturating_add(len) < width && word.kind() != WordType::ListMarker {
            len = len.saturating_add(word_len);
            line.push(word.clone());
        } else {
            let filler_content = if word.kind() == WordType::ListMarker {
                indent = if let Some((meta, list_type)) = zip_iter.next() {
                    match tmp.cmp(&display_width(meta.content())) {
                        std::cmp::Ordering::Less => {
                            o_list_counter_stack.push(0);
                        }
                        std::cmp::Ordering::Greater => {
                            o_list_counter_stack.pop();
                        }
                        std::cmp::Ordering::Equal => {}
                    }
                    if matches!(list_type.kind(), WordType::MetaInfo(MetaData::OList)) {
                        if let Some(counter) = o_list_counter_stack.last_mut() {
                            *counter = counter.saturating_add(1);
                            word.set_content(format!("{counter}. "));
                            extra_indent = 1;
                        }
                    } else {
                        extra_indent = 0;
                    }
                    tmp = display_width(meta.content());
                    tmp
                } else {
                    0
                };
                " ".repeat(indent)
            } else {
                " ".repeat(indent.saturating_add(2).saturating_add(extra_indent))
            };

            let filler = Word::new(filler_content, WordType::Normal);
            lines.push(std::mem::take(&mut line));
            let trimmed = word.content().trim_start().to_owned();
            word.set_content(trimmed);
            len = display_width(word.content()).saturating_add(display_width(filler.content()));
            line = vec![filler, word.clone()];
        }
    }
    lines.push(line);
    lines.retain(|l| l.iter().any(|w| !w.content().is_empty()));

    component.height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    component.content = lines;
}

/// Cell padding on each side of a table column's content.
///
/// Part of every column's budget beyond its content, alongside the borders.
const TABLE_CELL_PADDING: u16 = 1;

/// Lay out a table: fit columns to the width and wrap every cell.
///
/// Columns keep their natural widths when they fit; otherwise width is
/// distributed proportionally with a minimum of one column per cell.
/// Each cell wraps into sub-rows padded to its data row's height, and
/// the component records the final column widths and row heights for
/// the renderer. A table whose cells cannot be divided into columns
/// degenerates to a single-line block.
fn transform_table(component: &mut TextComponent, width: u16) {
    let width = width.saturating_sub(1);
    let column_count = component
        .meta_info
        .iter()
        .filter(|w| matches!(w.kind(), WordType::MetaInfo(MetaData::ColumnsCount)))
        .count();

    if column_count == 0 || !component.content.len().is_multiple_of(column_count) {
        component.height = 1;
        component.kind = TextNode::Table(vec![], vec![]);
        return;
    }
    let row_count = component
        .content
        .len()
        .checked_div(column_count)
        .unwrap_or(0);

    let content = &mut component.content;

    // Natural column widths: the widest cell content in each column.
    let mut natural_widths = vec![0u16; column_count];
    for row in content.chunks(column_count) {
        for (col_i, cell) in row.iter().enumerate() {
            let cell_len: usize = cell.iter().map(|w| display_width(w.content())).sum();
            if let Some(w) = natural_widths.get_mut(col_i) {
                *w = (*w).max(u16::try_from(cell_len).unwrap_or(u16::MAX));
            }
        }
    }

    // Border and padding every column adds beyond its content.
    let styling_overhead: u16 = 1u16.saturating_add(
        u16::try_from(column_count)
            .unwrap_or(u16::MAX)
            .saturating_mul(TABLE_CELL_PADDING.saturating_mul(2).saturating_add(1)),
    );
    let available_for_content: u16 = width.saturating_sub(styling_overhead);

    // A budget that cannot give every column a single column degenerates:
    // honoring the one-column minimum would push the table past the width.
    if usize::from(available_for_content) < column_count {
        component.height = 1;
        component.kind = TextNode::Table(vec![], vec![]);
        return;
    }

    let total_natural: u64 = natural_widths.iter().copied().map(u64::from).sum();
    let final_widths = if total_natural <= u64::from(available_for_content) {
        natural_widths
    } else {
        let mut capped = vec![0u16; column_count];

        // Pass one: distribute the available width proportionally to
        // natural widths, one column minimum.
        for (cap, &natural) in capped.iter_mut().zip(natural_widths.iter()) {
            if total_natural > 0 {
                let share = u16::try_from(
                    u64::from(available_for_content)
                        .saturating_mul(u64::from(natural))
                        .checked_div(total_natural)
                        .unwrap_or(0),
                )
                .unwrap_or(u16::MAX);
                *cap = share.max(1);
            } else {
                *cap = 1;
            }
        }

        // Pass two: hand the leftover budget to columns still short of
        // their natural width.
        let capped_sum: u64 = capped.iter().copied().map(u64::from).sum();
        let mut budget = i64::from(available_for_content)
            .saturating_sub(i64::try_from(capped_sum).unwrap_or(i64::MAX));
        if budget > 0 {
            for (cap, &natural) in capped.iter_mut().zip(natural_widths.iter()) {
                let want = natural.saturating_sub(*cap);
                let give = u16::try_from(budget).unwrap_or(u16::MAX).min(want);
                *cap = cap.saturating_add(give);
                budget = budget.saturating_sub(i64::from(give));
                if budget <= 0 {
                    break;
                }
            }
        }

        capped
    };

    // Wrap each cell to its column's width; sub-rows pad to the data
    // row's height so every cell in a row has the same sub-row count.
    let mut row_heights: Vec<u16> = Vec::with_capacity(row_count);
    let mut wrapped_cells: Vec<Vec<Vec<Vec<Word>>>> = Vec::with_capacity(row_count);

    for row in content.chunks(column_count) {
        let mut row_wrapped: Vec<Vec<Vec<Word>>> = Vec::with_capacity(column_count);
        let mut max_height: u16 = 1;

        for (col_idx, cell) in row.iter().enumerate() {
            let col_w = final_widths.get(col_idx).copied().unwrap_or(0) as usize;
            if col_w == 0 {
                row_wrapped.push(vec![vec![]]);
                continue;
            }
            let sub_rows = word_wrapping(cell, col_w);
            let h = u16::try_from(sub_rows.len()).unwrap_or(u16::MAX);
            if h > max_height {
                max_height = h;
            }
            row_wrapped.push(sub_rows);
        }

        row_heights.push(max_height);
        wrapped_cells.push(row_wrapped);
    }

    // Flatten: per data row, each cell's padded sub-rows in column
    // order — the renderer reconstructs the grid from the recorded
    // column count and row heights.
    let mut flat_content: Vec<Vec<Word>> = Vec::new();
    for (cells, &height) in wrapped_cells.iter().zip(row_heights.iter()) {
        let max_h = height as usize;
        for cell in cells {
            let mut padded = cell.clone();
            while padded.len() < max_h {
                padded.push(vec![]);
            }
            flat_content.extend(padded);
        }
    }

    component.content = flat_content;
    component.height = u16::try_from(
        row_heights
            .iter()
            .copied()
            .map(usize::from)
            .sum::<usize>()
            .saturating_add(3),
    )
    .unwrap_or(u16::MAX);
    component.kind = TextNode::Table(final_widths, row_heights);
}
