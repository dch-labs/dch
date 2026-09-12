//! Extraction-readiness of the markdown module: the seam is the
//! deliverable, and these tests prove a future lift-out stays a move,
//! not a rewrite.

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use dch_tui::markdown::{MarkdownTheme, SyntaxCapture, TextNode, WordType, parse_markdown, theme};

/// The module source contains no host references — the extraction grep.
///
/// Scans every source file of the module (and its root) for any `dch_*`
/// reference — a `use` line or a fully qualified path in expression
/// position — plus `loopctl`, any `crate::` path at all, and
/// `super::super` escapes toward the crate root. The module is
/// self-contained, so any hit would break the day the directory moves
/// to its own crate.
#[test]
fn markdown_sources_contain_no_host_imports() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let module_root = manifest_dir.join("src/markdown.rs");
    let module_dir = manifest_dir.join("src/markdown");

    let mut sources = vec![module_root];
    let entries = std::fs::read_dir(&module_dir).expect("markdown module directory exists");
    for entry in entries.flatten() {
        sources.push(entry.path());
    }
    assert!(
        sources.len() > 1,
        "the module directory should hold its sources"
    );

    let forbidden = ["dch_", "loopctl", "crate::", "super::super"];
    for path in sources {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display()));
        for pattern in forbidden {
            assert!(
                !text.contains(pattern),
                "{} imports or references `{pattern}` — the module must stay host-free",
                path.display()
            );
        }
    }
}

/// Code-block words carry a syntax capture, never a color.
///
/// The parser is color-agnostic by construction: pre-highlight,
/// code-block text is tagged [`SyntaxCapture::None`], and the renderer
/// resolves captures through the syntax theme.
#[test]
fn code_block_words_carry_captures_not_colors() {
    let root = parse_markdown("```rust\nlet x = 42;\n```", 80);
    let code_block = root
        .components()
        .into_iter()
        .find(|c| matches!(c.kind(), TextNode::CodeBlock))
        .expect("should have a CodeBlock component");

    let code_words: Vec<WordType> = code_block
        .content()
        .iter()
        .flatten()
        .map(dch_tui::markdown::Word::kind)
        .collect();
    assert!(
        !code_words.is_empty(),
        "the code block should hold classified words"
    );
    for kind in code_words {
        assert_eq!(
            kind,
            WordType::CodeBlock(SyntaxCapture::None),
            "pre-highlight code words carry the untagged capture"
        );
    }
}

/// Pest and unicode-width are plain dependencies; tree-sitter rides
/// behind the syntax-highlight feature.
///
/// Reads the manifest and enforces it: the parser's own dependencies
/// are plain entries, and every tree-sitter entry is optional — the
/// highlighting backend only builds when the feature is enabled, and
/// nothing in the module references it until the renderer wires it
/// up. Ratatui is the module's other legitimate dependency by design —
/// a manifest cannot scope dependencies to one module, so the test
/// asserts the surface it can.
#[test]
fn parser_hard_dependencies_are_pest_and_unicode_width() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("manifest is readable");

    let mut in_dependencies = false;
    let mut saw_pest = false;
    let mut saw_unicode_width = false;
    for line in text.lines() {
        if line.starts_with('[') {
            in_dependencies = line.trim() == "[dependencies]";
            continue;
        }
        if in_dependencies {
            if line.starts_with("pest ") || line.starts_with("pest=") {
                saw_pest = true;
            }
            if line.starts_with("unicode-width") {
                saw_unicode_width = true;
            }
            if line.contains("tree-sitter") {
                assert!(
                    line.contains("optional = true"),
                    "tree-sitter entries must stay optional, gated behind \
                     the syntax-highlight feature: {line}"
                );
            }
        }
    }
    assert!(saw_pest, "pest is a plain dependency");
    assert!(saw_unicode_width, "unicode-width is a plain dependency");
}

/// The module's theme types are its own, distinct from the host's.
///
/// Same names, different types: if they ever collapse, extraction
/// drags a host import along and the move stops being mechanical.
#[test]
fn module_theme_types_are_distinct_from_the_host_theme_types() {
    let module_markdown = std::any::type_name::<MarkdownTheme>();
    let module_syntax = std::any::type_name::<theme::SyntaxTheme>();
    let host_markdown = std::any::type_name::<dch_tui::theme::MarkdownTheme>();
    let host_syntax = std::any::type_name::<dch_tui::theme::SyntaxTheme>();

    assert_ne!(
        module_markdown, host_markdown,
        "MarkdownTheme must be the module's own type"
    );
    assert_ne!(
        module_syntax, host_syntax,
        "SyntaxTheme must be the module's own type"
    );
    let _default = MarkdownTheme::default();
}

/// `parse_markdown` returns a root for garbage, empty, and huge input
/// — never a panic.
#[test]
fn parse_markdown_never_panics_on_garbage() {
    let garbage = [
        "",
        "`",
        "```",
        "| a | b |\n|---|",
        "**bold *nested",
        "x",
        "\n\n\n",
        "|",
        "](",
    ];
    for input in garbage {
        let _root = parse_markdown(input, 80);
    }
    // Sized to stay a seconds-scale test: pest's per-word pair
    // allocation makes a 10 MB token stream effectively quadratic, so
    // the huge-input proof runs at the largest size (100k) that keeps CI
    // honest rather than slow.
    let huge = "a".repeat(100_000);
    let _root = parse_markdown(&huge, 80);
}

/// Re-transforming at a narrower width re-flows the content.
///
/// Parse at a wide width, shrink, and the wrapped rows multiply while
/// none exceeds the new width — the resize path without re-parsing.
#[test]
fn retransform_at_a_narrower_width_reflows() {
    let paragraph = "one two three four five six seven eight nine ten eleven twelve";
    let mut root = parse_markdown(paragraph, 80);
    let wide_rows = root.components().first().map_or(0, |c| c.content().len());

    root.transform(30);

    let components = root.components();
    let component = components
        .first()
        .expect("the paragraph survives the transform");
    for row in component.content() {
        let text: String = row.iter().map(dch_tui::markdown::Word::content).collect();
        let width = unicode_width::UnicodeWidthStr::width(text.as_str());
        assert!(
            width <= 30,
            "row exceeds the new width ({}): {text:?}",
            width
        );
    }
    assert!(
        component.content().len() > wide_rows,
        "narrowing the width must add rows: {} -> {}",
        wide_rows,
        component.content().len()
    );
}
