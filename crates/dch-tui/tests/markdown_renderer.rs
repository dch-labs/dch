//! Renderer regression suite — every case asserts on the returned
//! `Vec<Line>`: joined text, structure, and styles; never on exact
//! tree-sitter token colors.

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]

use dch_tui::markdown::{MarkdownTheme, SyntaxTheme, render_markdown};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

const BASE: Color = Color::White;

/// A theme with a distinct style per element, so style assertions
/// identify their source.
fn md_theme() -> MarkdownTheme {
    MarkdownTheme {
        header: [
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ],
        bold: Style::default().add_modifier(Modifier::BOLD),
        italic: Style::default().add_modifier(Modifier::ITALIC),
        code_inline: Style::default().fg(Color::Green).bg(Color::Black),
        code_block: Color::DarkGray,
        link: Style::default().fg(Color::Blue),
        quote: Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC),
        list_item: Style::default().fg(Color::Yellow),
        horizontal_rule: Style::default().fg(Color::DarkGray),
        border: Color::White,
        dim: Color::DarkGray,
    }
}

/// A palette with a distinct color per capture.
fn syntax_theme() -> SyntaxTheme {
    SyntaxTheme {
        plain: Color::White,
        attribute: Color::Indexed(1),
        comment: Color::Indexed(2),
        constant: Color::Indexed(3),
        constructor: Color::Indexed(4),
        embedded: Color::Indexed(5),
        function: Color::Indexed(6),
        keyword: Color::Yellow,
        number: Color::Indexed(15),
        operator: Color::Indexed(7),
        property: Color::Indexed(8),
        punctuation: Color::Indexed(9),
        string: Color::Green,
        r#type: Color::Cyan,
        variable: Color::Indexed(10),
        variable_builtin: Color::Indexed(11),
        tag: Color::Indexed(12),
        delimiter: Color::Indexed(13),
        escape: Color::Indexed(14),
    }
}

fn render(text: &str, width: u16) -> Vec<Line<'static>> {
    render_markdown(text, width, &md_theme(), &syntax_theme(), BASE, None)
}

fn joined(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn line_width(line: &Line<'_>) -> usize {
    UnicodeWidthStr::width(joined(line).as_str())
}

#[test]
fn a_plain_paragraph_renders_as_one_line_per_row() {
    let lines = render("hello world", 80);
    assert_eq!(lines.len(), 1);
    assert_eq!(joined(&lines[0]), "hello world");
    assert_eq!(lines[0].spans[0].style.fg, Some(BASE));
}

#[test]
fn the_parser_rows_bound_the_lines() {
    let paragraph = "word ".repeat(50);
    let lines = render(paragraph.trim(), 20);
    assert!(lines.len() > 1, "expected wrapping, got one line");
    for line in &lines {
        assert!(
            line_width(line) <= 20,
            "line exceeds the wrap width: {:?}",
            joined(line)
        );
    }
}

#[test]
fn bold_and_italic_words_carry_their_modifiers() {
    let lines = render("**bold** and *italic*", 80);
    let spans = &lines[0].spans;
    let bold = spans.iter().find(|s| s.content.as_ref() == "bold").unwrap();
    assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(bold.style.fg, Some(BASE));
    let plain = spans.iter().find(|s| s.content.as_ref() == "and").unwrap();
    assert!(!plain.style.add_modifier.contains(Modifier::BOLD));
    let italic = spans
        .iter()
        .find(|s| s.content.as_ref() == "italic")
        .unwrap();
    assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn inline_code_uses_the_code_inline_style() {
    let lines = render("wrap `code` end", 80);
    let span = lines[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "code")
        .unwrap();
    let expected = Style::default().fg(BASE).patch(md_theme().code_inline);
    assert_eq!(span.style, expected);
}

#[test]
fn heading_levels_pick_their_header_style() {
    let inputs = [
        "# one",
        "## two",
        "### three",
        "#### four",
        "##### five",
        "###### six",
    ];
    for (idx, input) in inputs.iter().enumerate() {
        let lines = render(input, 80);
        let expected = Style::default().fg(BASE).patch(md_theme().header[idx]);
        for span in &lines[0].spans {
            assert_eq!(
                span.style,
                expected,
                "level {} heading spans must carry header{}",
                idx + 1,
                idx + 1
            );
        }
    }
}

#[test]
fn a_code_block_renders_framed_lines() {
    let lines = render("```rust\nlet x = 1;\nlet y = 2;\n```", 80);
    let code_lines: Vec<&Line<'static>> = lines
        .iter()
        .filter(|l| !joined(l).trim().is_empty())
        .collect();
    assert_eq!(code_lines.len(), 2, "one line per source line");
    for line in &code_lines {
        let first = line.spans.first().unwrap();
        let last = line.spans.last().unwrap();
        assert_eq!(first.content.as_ref(), " ");
        assert_eq!(first.style.bg, Some(Color::DarkGray));
        assert_eq!(last.content.as_ref(), " ");
        assert_eq!(last.style.bg, Some(Color::DarkGray));
    }
}

#[test]
fn a_language_without_a_grammar_renders_plain() {
    let lines = render("```text\nhello\n```", 80);
    let code = lines
        .iter()
        .find(|l| joined(l).contains("hello"))
        .expect("the code line renders");
    let body = code
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "hello")
        .unwrap();
    assert_eq!(body.style.fg, Some(syntax_theme().plain));
    assert_eq!(body.style.bg, Some(Color::DarkGray));
}

#[test]
fn an_unknown_language_does_not_panic() {
    let lines = render("```totallymadeup\nx\n```", 80);
    assert!(lines.iter().any(|l| joined(l).contains('x')));
}

#[test]
fn a_quote_carries_the_dim_prefix() {
    let lines = render("> quoted", 80);
    let prefix = &lines[0].spans[0];
    assert_eq!(prefix.content.as_ref(), "│ ");
    assert_eq!(prefix.style.fg, Some(Color::DarkGray));
    let body = &lines[0].spans[1];
    let expected = Style::default().fg(BASE).patch(md_theme().quote);
    assert_eq!(body.style, expected);
}

#[test]
fn unordered_list_rows_start_with_their_marker() {
    let lines = render("- a\n- b\n- c", 80);
    let rows: Vec<&Line<'static>> = lines
        .iter()
        .filter(|l| !joined(l).trim().is_empty())
        .collect();
    assert_eq!(rows.len(), 3);
    for row in rows {
        let marker = row
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "• ")
            .unwrap_or_else(|| panic!("no marker span in {:?}", joined(row)));
        assert_eq!(marker.style.fg, Some(Color::Yellow));
    }
}

#[test]
fn ordered_list_markers_render_renumbered() {
    let lines = render("1. a\n1. b\n1. c", 80);
    let markers: Vec<String> = lines
        .iter()
        .filter(|l| !joined(l).trim().is_empty())
        .map(|l| joined(l))
        .collect();
    assert!(markers.iter().any(|m| m.starts_with("1. ")), "{markers:?}");
    assert!(markers.iter().any(|m| m.starts_with("2. ")), "{markers:?}");
    assert!(markers.iter().any(|m| m.starts_with("3. ")), "{markers:?}");
}

#[test]
fn a_table_renders_box_borders() {
    let lines = render("| A | B |\n|---|---|\n| 1 | 2 |", 40);
    let first = joined(lines.first().unwrap());
    assert!(
        first.starts_with('┌') && first.contains('┬') && first.ends_with('┐'),
        "{first:?}"
    );
    let header_row = joined(&lines[1]);
    assert!(
        header_row.starts_with('│') && header_row.contains('A'),
        "{header_row:?}"
    );
    let header_sep = joined(&lines[2]);
    assert!(
        header_sep.starts_with('├') && header_sep.contains('┼') && header_sep.ends_with('┤'),
        "{header_sep:?}"
    );
    let last = joined(lines.last().unwrap());
    assert!(
        last.starts_with('└') && last.contains('┴') && last.ends_with('┘'),
        "{last:?}"
    );

    let header_line = &lines[1];
    assert!(
        header_line
            .spans
            .iter()
            .any(|s| s.content.as_ref() == "A" && s.style.add_modifier.contains(Modifier::BOLD))
    );
}

#[test]
fn table_cells_pad_to_their_column_width() {
    let lines = render("| A | B |\n|---|---|\n| 1 | 2 |", 40);
    let data_row = lines
        .iter()
        .find(|l| joined(l).starts_with("│ 1"))
        .expect("the data row renders");
    let between_borders: String = joined(data_row).trim_matches(|c| c == '│').to_string();
    assert!(
        between_borders.starts_with(" 1") && between_borders.len() > 2,
        "cell text with padding expected, got {between_borders:?}"
    );
}

#[test]
fn one_blank_line_between_blocks_and_none_at_the_edges() {
    let lines = render("para one\n\npara two", 80);
    assert!(!joined(lines.first().unwrap()).trim().is_empty());
    assert!(!joined(lines.last().unwrap()).trim().is_empty());
    let blanks = lines.iter().filter(|l| joined(l).trim().is_empty()).count();
    assert_eq!(blanks, 1, "exactly one blank between, got {blanks}");
}

#[test]
fn a_horizontal_rule_spans_min_of_width_and_forty() {
    let lines = render("---", 80);
    assert_eq!(lines.len(), 1);
    let span = &lines[0].spans[0];
    assert_eq!(UnicodeWidthStr::width(span.content.as_ref()), 40);
    assert_eq!(span.style, md_theme().horizontal_rule);
}

#[test]
fn clip_width_bounds_every_line() {
    let paragraph = "abcdefghij ".repeat(10);
    let lines = render_markdown(
        paragraph.trim(),
        200,
        &md_theme(),
        &syntax_theme(),
        BASE,
        Some(20),
    );
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(
            line_width(line) <= 20,
            "clipped line exceeds 20: {:?}",
            joined(line)
        );
    }
}

#[test]
fn clip_width_respects_wide_glyphs() {
    let lines = render_markdown(
        "中文中文中文中文中文",
        80,
        &md_theme(),
        &syntax_theme(),
        BASE,
        Some(7),
    );
    for line in &lines {
        let width = line_width(line);
        assert!(
            width <= 7,
            "wide-glyph clip crossed the budget: width {width}"
        );
    }
}

#[test]
fn empty_input_renders_no_lines() {
    let lines = render("", 80);
    assert!(lines.is_empty());
}

#[test]
fn malformed_input_does_not_panic() {
    for input in ["```unterminated code block", "- [x", "| a | b |"] {
        let _ = render(input, 80);
    }
}

#[test]
fn task_markers_keep_their_following_space() {
    let lines = render("- [ ] todo item\n- [x] done item", 80);
    let text: Vec<String> = lines.iter().map(joined).collect();
    assert!(
        text.iter().any(|l| l.contains("- [ ] todo item")),
        "the open marker must not glue to its text: {text:?}"
    );
    assert!(
        text.iter().any(|l| l.contains("- [x] done item")),
        "the closed marker must not glue to its text: {text:?}"
    );
}

/// A known snippet highlights through the palette: representative
/// tokens carry the color of their capture, pinning the highlighter's
/// emit order to `emit_order_colors`.
#[test]
#[cfg(feature = "syntax-highlight")]
fn highlighted_tokens_carry_their_capture_colors() {
    let theme = syntax_theme();
    let lines = render_markdown(
        "```rust\nfn main() { let s = \"x\"; }\n```",
        80,
        &md_theme(),
        &theme,
        BASE,
        None,
    );
    let spans: Vec<&ratatui::text::Span> = lines.iter().flat_map(|l| l.spans.iter()).collect();

    let keyword = spans.iter().find(|s| s.content.as_ref() == "fn").unwrap();
    assert_eq!(keyword.style.fg, Some(theme.keyword));
    let function = spans.iter().find(|s| s.content.as_ref() == "main").unwrap();
    assert_eq!(function.style.fg, Some(theme.function));
    let string = spans
        .iter()
        .find(|s| s.content.as_ref() == "\"x\"")
        .unwrap();
    assert_eq!(string.style.fg, Some(theme.string));
}

#[test]
fn truncated_wide_cells_keep_their_column_alignment() {
    let lines = render("| 中 | 文 |\n|---|---|", 10);
    let widths: Vec<usize> = lines.iter().map(line_width).collect();
    assert!(
        widths.len() >= 3,
        "borders plus a header row, got {widths:?}"
    );
    assert!(
        widths.iter().all(|width| *width == widths[0]),
        "every table line must share one width, got {widths:?}"
    );
}

#[test]
fn a_list_item_hard_splits_a_word_wider_than_the_width() {
    let path = "/very/long/path/to/some/deeply/nested/directory/structure/file_name.rs";
    let lines = render(&format!("- see {path} for details"), 30);
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(
            line_width(line) <= 30,
            "list line exceeds 30: {:?}",
            joined(line)
        );
    }
    let reconstructed: String = lines
        .iter()
        .map(|l| joined(l).trim().to_string())
        .collect::<String>();
    assert!(
        reconstructed.starts_with("• see"),
        "the marker row leads the item: {reconstructed:?}"
    );
    assert!(
        reconstructed.contains(path),
        "the split token survives contiguously across its chunk rows: {reconstructed:?}"
    );
    assert!(
        lines.iter().skip(1).any(|l| joined(l).starts_with("  ")),
        "continuation rows carry the hanging indent: {:?}",
        lines.iter().map(joined).collect::<Vec<_>>()
    );
}

#[test]
fn an_ordered_list_item_hard_splits_a_wide_token() {
    let path = "/very/long/path/to/some/deeply/nested/directory/structure/file_name.rs";
    let lines = render(&format!("1. see {path} for details"), 30);
    for line in &lines {
        assert!(
            line_width(line) <= 30,
            "ordered list line exceeds 30: {:?}",
            joined(line)
        );
    }
    let reconstructed: String = lines
        .iter()
        .map(|l| joined(l).trim().to_string())
        .collect::<String>();
    assert!(
        reconstructed.starts_with("1. see"),
        "renumbering survives the split: {reconstructed:?}"
    );
    assert!(
        reconstructed.contains(path),
        "the split token survives contiguously: {reconstructed:?}"
    );
}

#[test]
fn a_nested_list_item_hard_splits_within_its_indent() {
    let path = "/very/long/path/to/some/deeply/nested/directory/structure/file_name.rs";
    let lines = render(&format!("- outer\n  - inner {path}"), 40);
    for line in &lines {
        assert!(
            line_width(line) <= 40,
            "nested list line exceeds 40: {:?}",
            joined(line)
        );
    }
    let reconstructed: String = lines
        .iter()
        .map(|l| joined(l).trim().to_string())
        .collect::<String>();
    assert!(
        reconstructed.contains(path),
        "the nested token survives contiguously: {reconstructed:?}"
    );
}

#[test]
fn a_fitting_list_item_stays_on_one_line() {
    let lines = render("- one two three", 40);
    assert_eq!(lines.len(), 1, "no split when the item fits");
    assert_eq!(joined(&lines[0]), "• one two three");
}

fn code_source(line: &Line<'_>) -> String {
    let text = joined(line);
    let text = text.as_str();
    let text = text.strip_prefix(' ').unwrap_or(text);
    let text = text.strip_suffix(' ').unwrap_or(text);
    text.to_string()
}

#[test]
fn a_wide_code_line_soft_wraps_onto_continuation_rows() {
    let source = "a".repeat(60);
    let lines = render(&format!("```text\n{source}\n```"), 30);
    assert!(lines.len() >= 3, "60 columns wrap at a 30-column pane");
    for line in &lines {
        assert!(
            line_width(line) <= 30,
            "code row exceeds 30: {:?}",
            joined(line)
        );
    }
    assert!(
        joined(&lines[0]).starts_with(' ') && joined(&lines[0]).ends_with(' '),
        "the first row keeps its panel padding: {:?}",
        joined(&lines[0])
    );
    for line in &lines {
        assert!(
            joined(line).starts_with(' ') && joined(line).ends_with(' '),
            "every row of the panel keeps its padding: {:?}",
            joined(line)
        );
        for span in &line.spans {
            assert!(
                !span.content.contains('↪'),
                "a wrapped code row draws only what the source carried — \
                 a selection copies span text verbatim: {:?}",
                span.content
            );
        }
    }
    let reconstructed: String = lines.iter().map(code_source).collect();
    assert_eq!(reconstructed, source, "wrapping drops no characters");
}

#[test]
fn code_soft_wrapping_preserves_every_source_line() {
    // Wrapping must not reorder or invent content: 45 columns of `b`
    // at a 30-column pane split 28/17 across two rows, the empty
    // source line keeps its own padding-only row, and `short` keeps
    // its row — the rows read in order reassemble the source exactly.
    let long = "b".repeat(45);
    let source = format!("{long}\n\nshort");
    let lines = render(&format!("```text\n{source}\n```"), 30);
    for line in &lines {
        assert!(
            line_width(line) <= 30,
            "code row exceeds 30: {:?}",
            joined(line)
        );
    }
    let rows: Vec<String> = lines.iter().map(code_source).collect();
    assert_eq!(
        rows,
        vec![
            "b".repeat(28),
            "b".repeat(17),
            String::new(),
            "short".to_string(),
        ],
        "the rows reassemble the source in order: {rows:?}"
    );
    assert_eq!(rows.join(""), source.replace('\n', ""), "no column lost");
}

#[test]
fn an_empty_code_line_renders_one_blank_panel_row() {
    let lines = render("```text\na\n\nb\n```", 80);
    let blanks: Vec<&Line<'_>> = lines
        .iter()
        .filter(|l| joined(l).trim().is_empty())
        .collect();
    assert_eq!(blanks.len(), 1, "one blank panel row per empty source line");
    assert_eq!(line_width(blanks[0]), 2, "the blank row is the two pads");
}

#[test]
#[cfg(feature = "syntax-highlight")]
fn a_split_highlighted_span_keeps_its_style_across_the_break() {
    let literal = format!("\"{}\"", "x".repeat(50));
    let source = format!("let s = {literal};");
    let lines = render(&format!("```rust\n{source}\n```"), 30);
    for line in &lines {
        assert!(
            line_width(line) <= 30,
            "highlighted code row exceeds 30: {:?}",
            joined(line)
        );
    }
    let spans: Vec<&ratatui::text::Span> = lines.iter().flat_map(|l| l.spans.iter()).collect();
    let head = spans
        .iter()
        .find(|s| s.content.starts_with('"') && s.content.len() > 1)
        .expect("the literal's head renders");
    let tail = spans
        .iter()
        .find(|s| s.content.contains('"') && s.content.ends_with('"'))
        .expect("the literal's tail renders");
    assert!(
        head.content != tail.content,
        "the literal actually split across rows"
    );
    assert_eq!(
        head.style.fg, tail.style.fg,
        "both pieces of the split span keep one style"
    );
}
