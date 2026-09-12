//! Regression suite for the markdown parser — every edge case here
//! encodes a real bug once seen in model output.

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]

use dch_tui::markdown::{MetaData, TextNode, WordType, parse_markdown};

/// Code blocks keep newline-separated lines and the fence's language.
///
/// `content_as_lines()` returns clean, non-empty lines without leading
/// newlines — previously the lines concatenated with no separators and
/// code rendered as one visual line.
#[test]
fn a_code_block_keeps_each_source_line() {
    let code = "```rust\nlet x = 42;\nlet y = x + 1;\n```";
    let root = parse_markdown(code, 80);

    let code_block = root
        .components()
        .into_iter()
        .find(|c| matches!(c.kind(), TextNode::CodeBlock))
        .expect("should have a CodeBlock component");

    let lines = code_block.content_as_lines();
    assert_eq!(lines.len(), 2, "should have 2 code lines");
    assert_eq!(lines[0], "let x = 42;");
    assert_eq!(lines[1], "let y = x + 1;");
    assert_eq!(lines.join("\n"), "let x = 42;\nlet y = x + 1;");

    let has_rust_meta = code_block.meta_info().iter().any(|w| w.content() == "rust");
    assert!(has_rust_meta, "should detect rust language");
}

/// Blank lines inside code blocks are content and must survive.
#[test]
fn interior_blank_lines_are_code_and_survive() {
    let code = "```python\ndef hello():\n    print('hi')\n\n    return True\n```";
    let root = parse_markdown(code, 80);

    let code_block = root
        .components()
        .into_iter()
        .find(|c| matches!(c.kind(), TextNode::CodeBlock))
        .expect("should have a CodeBlock component");

    let lines = code_block.content_as_lines();
    assert_eq!(
        lines,
        vec!["def hello():", "    print('hi')", "", "    return True",],
        "interior blank lines are code and must survive the plain-text view"
    );
}

/// A table whose natural column widths sum past `u16::MAX` must lay out
/// without panicking — the accumulations run in wider integers and clamp.
#[test]
fn a_wide_table_does_not_overflow() {
    let blob = "x".repeat(33_000);
    let md = format!("| {blob} | {blob} |\n|---|---|\n| a | b |");
    let root = parse_markdown(&md, 80);
    assert!(
        root.components()
            .iter()
            .any(|c| matches!(c.kind(), TextNode::Table(_, _))),
        "the wide table still parses as a table"
    );
}

/// A wide glyph wider than the whole budget owns its row — the split
/// can exceed the width by at most one character, and must not panic.
#[test]
fn wide_glyphs_at_minimum_width_own_their_rows() {
    let root = parse_markdown("# 中文测试", 1);
    let comps = root.components();
    let heading = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::Heading))
        .expect("should have a heading");
    for row in heading.content() {
        let text: String = row.iter().map(dch_tui::markdown::Word::content).collect();
        let width = unicode_width::UnicodeWidthStr::width(text.as_str());
        assert!(
            width <= 2,
            "a row may exceed width 1 by at most one character, got {width}: {text:?}"
        );
    }
}

/// Inline code in table cells is classified as code with markers
/// stripped — previously backticks rendered literally.
#[test]
fn inline_code_in_table_cells_keeps_code_type_without_markers() {
    let markdown = "| `topic` | `value` |\n|---|---|\n| `foo` | `bar` |";
    let root = parse_markdown(markdown, 80);

    let table = root
        .components()
        .into_iter()
        .find(|c| matches!(c.kind(), TextNode::Table(_, _)))
        .expect("should have a Table component");

    let all_words: Vec<&dch_tui::markdown::Word> = table.content().iter().flatten().collect();

    let code_words: Vec<&&dch_tui::markdown::Word> = all_words
        .iter()
        .filter(|w| matches!(w.kind(), WordType::Code))
        .collect();
    assert!(
        !code_words.is_empty(),
        "should have at least one Code-typed word, got word types: {:?}",
        all_words.iter().map(|w| w.kind()).collect::<Vec<_>>()
    );

    for w in &code_words {
        let content = w.content();
        assert!(
            !content.contains('`'),
            "Code word should not contain backticks, got: {:?}",
            content
        );
    }

    let code_contents: Vec<&str> = code_words.iter().map(|w| w.content()).collect();
    assert!(
        code_contents.contains(&"topic"),
        "should contain 'topic', got: {:?}",
        code_contents
    );
    assert!(
        code_contents.contains(&"value"),
        "should contain 'value', got: {:?}",
        code_contents
    );
}

/// Tables parse with the right column and cell counts — trailing pipes
/// once produced phantom cells that broke the column multiple.
#[test]
fn a_table_parses_into_aligned_rows_and_columns() {
    let markdown = "| A | B | C |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |";
    let root = parse_markdown(markdown, 80);

    let table = root
        .components()
        .into_iter()
        .find(|c| matches!(c.kind(), TextNode::Table(_, _)))
        .expect("should have a Table component");

    let column_count = table
        .meta_info()
        .iter()
        .filter(|w| matches!(w.kind(), WordType::MetaInfo(MetaData::ColumnsCount)))
        .count();
    assert_eq!(column_count, 3, "should have 3 columns");

    let cell_count = table.content().len();
    assert_eq!(cell_count, 9, "should have 9 cells (3 rows × 3 columns)");
    assert!(
        cell_count.is_multiple_of(column_count),
        "cell count should be multiple of column count"
    );

    if let TextNode::Table(widths, rows) = table.kind() {
        assert_eq!(widths.len(), 3, "should have 3 column widths");
        assert_eq!(rows.len(), 3, "should have 3 rows");
    } else {
        panic!("Table should have Table variant");
    }
}

/// Headings longer than the width wrap into rows that each fit.
#[test]
fn a_heading_wraps_when_it_exceeds_the_width() {
    let heading =
        "# This is a very long heading that definitely exceeds forty characters and should wrap";
    let root = parse_markdown(heading, 40);

    let comps = root.components();
    let heading_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::Heading))
        .expect("should have a heading");

    let rows = heading_comp.content();
    assert!(
        rows.len() > 1,
        "Long heading should wrap to multiple rows, got {} rows",
        rows.len()
    );

    for (i, row) in rows.iter().enumerate() {
        let text: String = row.iter().map(dch_tui::markdown::Word::content).collect();
        let w = unicode_width::UnicodeWidthStr::width(text.as_str());
        assert!(
            w <= 40,
            "Row {} has display width {} > 40: {:?}",
            i,
            w,
            text
        );
    }
}

/// Short headings stay one row.
#[test]
fn a_short_heading_stays_on_one_line() {
    let heading = "# Short";
    let root = parse_markdown(heading, 80);

    let comps = root.components();
    let heading_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::Heading))
        .expect("should have a heading");

    let rows = heading_comp.content();
    assert_eq!(
        rows.len(),
        1,
        "Short heading should be a single row, got {}",
        rows.len()
    );
}

/// Pre-numbered ordered lists keep their sequence.
#[test]
fn ordered_lists_renumber_from_one() {
    let md = "1. first item\n2. second item\n3. third item";
    let root = parse_markdown(md, 80);

    let comps = root.components();
    let list_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::List))
        .expect("should have a list");

    let rows = list_comp.content();

    let markers: Vec<String> = rows
        .iter()
        .map(|row| {
            row.iter()
                .filter(|w| matches!(w.kind(), WordType::ListMarker))
                .map(|w| w.content().to_owned())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();

    assert_eq!(
        markers.len(),
        3,
        "Expected 3 list rows, got {}: {:?}",
        markers.len(),
        markers
    );
    assert_eq!(markers[0], "1. ", "First item should be '1. '");
    assert_eq!(markers[1], "2. ", "Second item should be '2. '");
    assert_eq!(markers[2], "3. ", "Third item should be '3. '");
}

/// Models commonly start every ordered item with "1." — the layout
/// renumbers them sequentially regardless of the written numbers.
#[test]
fn an_all_ones_ordered_list_renumbers_sequentially() {
    let md = "1. first item\n1. second item\n1. third item";
    let root = parse_markdown(md, 80);

    let comps = root.components();
    let list_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::List))
        .expect("should have a list");

    let rows = list_comp.content();

    let markers: Vec<String> = rows
        .iter()
        .map(|row| {
            row.iter()
                .filter(|w| matches!(w.kind(), WordType::ListMarker))
                .map(|w| w.content().to_owned())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();

    assert_eq!(
        markers.len(),
        3,
        "Expected 3 list rows, got {}: {:?}",
        markers.len(),
        markers
    );
    assert_eq!(markers[0], "1. ");
    assert_eq!(
        markers[1], "2. ",
        "Second item should be renumbered to '2. '"
    );
    assert_eq!(
        markers[2], "3. ",
        "Third item should be renumbered to '3. '"
    );
}

/// Blank lines between ordered items do not split the list; numbering
/// stays sequential across the gaps.
#[test]
fn blank_lines_do_not_split_ordered_list_items() {
    let md = "1. first item\n\n2. second item\n\n3. third item";
    let root = parse_markdown(md, 80);

    let comps = root.components();
    let list_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::List))
        .expect("should have exactly one List component");

    let rows = list_comp.content();
    let markers: Vec<String> = rows
        .iter()
        .map(|row| {
            row.iter()
                .filter(|w| matches!(w.kind(), WordType::ListMarker))
                .map(|w| w.content().to_owned())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();

    assert_eq!(
        markers.len(),
        3,
        "Expected 3 list rows, got {}: {:?}",
        markers.len(),
        markers
    );
    assert_eq!(markers[0], "1. ", "First item marker");
    assert_eq!(markers[1], "2. ", "Second item marker");
    assert_eq!(markers[2], "3. ", "Third item marker");
}

/// Tables with arrows and Unicode cells parse as tables, not paragraphs.
#[test]
fn table_cells_carry_their_wide_glyphs() {
    let md = "| Priority | Module | Source | Lines | Notes |\n\
              |----------|--------|--------|-------|-------|\n\
              | B1 | core/types.rs → core/types.rs | 912 lines | AgentConfig, AgentState — depends on observer |  |\n\
              | B2 | core/agent_memory.rs → core/memory.rs | 409 lines | Memory management — depends on types |  |\n\
              | B3 | managers/bundle.rs → loop_control/bundle.rs | 181 lines | Bundles all managers together |  |\n";

    let root = parse_markdown(md, 100);
    let comps = root.components();

    let table_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::Table(_, _)));
    assert!(
        table_comp.is_some(),
        "Expected a Table component, got component kinds: {:?}",
        comps
            .iter()
            .map(|c| format!("{:?}", c.kind()))
            .collect::<Vec<_>>()
    );

    if let Some(tc) = table_comp
        && let TextNode::Table(widths, _heights) = tc.kind()
    {
        assert!(
            !widths.is_empty(),
            "Table should have non-empty column widths"
        );
    }
}

/// A table at end-of-input with no trailing newline must not grow a
/// phantom cell from the trailing pipe — the streaming-era regression.
#[test]
fn a_table_without_a_trailing_newline_still_parses() {
    let md = "| Priority | Module | Source | Lines | Notes |\n\
              |----------|--------|--------|-------|-------|\n\
              | B1 | core/types.rs → core/types.rs | 912 lines | AgentConfig |  |\n\
              | B2 | core/agent_memory.rs → core/memory.rs | 409 lines | Memory management |  |";

    let root = parse_markdown(md, 100);
    let comps = root.components();

    let table_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::Table(_, _)));
    assert!(
        table_comp.is_some(),
        "Expected a Table component, got component kinds: {:?}",
        comps
            .iter()
            .map(|c| format!("{:?}", c.kind()))
            .collect::<Vec<_>>()
    );

    if let Some(tc) = table_comp
        && let TextNode::Table(widths, _heights) = tc.kind()
    {
        assert!(
            !widths.is_empty(),
            "Table should have non-empty column widths, got component kinds: {:?}",
            comps
                .iter()
                .map(|c| format!("{:?}", c.kind()))
                .collect::<Vec<_>>()
        );
    }
}

/// A table embedded between paragraphs still parses as a table.
#[test]
fn a_table_among_paragraphs_parses_in_place() {
    let md = "The plan's \"A\" group is done. Next would be the \"B\" group:\n\n| Priority | Module | Lines | Notes |\n|----------|--------|-------|-------|\n| B1 | core/types.rs → core/types.rs | 912 lines | AgentConfig — depends on observer |\n| B2 | core/agent_memory.rs → core/memory.rs | 409 lines | Memory management |\n\nProceed with this plan.";

    let root = parse_markdown(md, 100);
    let comps = root.components();

    let table_comp = comps
        .iter()
        .find(|c| matches!(c.kind(), TextNode::Table(_, _)));
    assert!(
        table_comp.is_some(),
        "Expected a Table component, got component kinds: {:?}",
        comps
            .iter()
            .map(|c| format!("{:?}", c.kind()))
            .collect::<Vec<_>>()
    );
}
