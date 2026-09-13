//! Syntax highlighting via tree-sitter.
//!
//! Maps a fenced code block's language tag to a grammar, runs the
//! tree-sitter highlighter over the source, and returns the highlight
//! event stream the renderer walks. Unknown languages and highlighting
//! failures fall back to unhighlighted output — never an error.

use tree_sitter_highlight::HighlightEvent;

/// The capture names the highlighter is configured with, in emit
/// order.
///
/// The renderer's palette projection indexes into this order; the
/// two lists travel together and must not drift apart.
const HIGHLIGHT_NAMES: [&str; 18] = [
    "attribute",
    "constant",
    "function.builtin",
    "function",
    "keyword",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// The outcome of a highlighting run.
#[derive(Debug)]
pub(crate) enum HighlightInfo {
    /// The highlighter produced an event stream for the source.
    ///
    /// Each event either delimits a source byte range or toggles a
    /// highlight on the stack; the renderer consumes them in order.
    Highlighted(Vec<HighlightEvent>),

    /// No highlighting is available for the input.
    ///
    /// Unknown language, missing grammar, or a highlighting failure;
    /// the renderer falls back to plain code styling.
    Unhighlighted,
}

/// Highlight `code` according to its `language` tag.
///
/// Aliases fold onto their grammar ("rs" is Rust, "yml" is YAML); any
/// language without a matching grammar yields
/// [`HighlightInfo::Unhighlighted`].
pub(crate) fn highlight_code(language: &str, code: &[u8]) -> HighlightInfo {
    match language {
        "bash" | "sh" => hl(
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            tree_sitter_bash::HIGHLIGHT_QUERY,
            code,
        ),
        "c" => hl(
            tree_sitter_c::LANGUAGE.into(),
            "c",
            tree_sitter_c::HIGHLIGHT_QUERY,
            code,
        ),
        "cpp" | "c++" => hl(
            tree_sitter_cpp::LANGUAGE.into(),
            "cpp",
            tree_sitter_cpp::HIGHLIGHT_QUERY,
            code,
        ),
        "css" => hl(
            tree_sitter_css::LANGUAGE.into(),
            "css",
            tree_sitter_css::HIGHLIGHTS_QUERY,
            code,
        ),
        "diff" | "patch" => hl(
            tree_sitter_diff::LANGUAGE.into(),
            "diff",
            tree_sitter_diff::HIGHLIGHTS_QUERY,
            code,
        ),
        "elixir" => hl(
            tree_sitter_elixir::LANGUAGE.into(),
            "elixir",
            tree_sitter_elixir::HIGHLIGHTS_QUERY,
            code,
        ),
        "go" => hl(
            tree_sitter_go::LANGUAGE.into(),
            "go",
            tree_sitter_go::HIGHLIGHTS_QUERY,
            code,
        ),
        "html" => hl(
            tree_sitter_html::LANGUAGE.into(),
            "html",
            tree_sitter_html::HIGHLIGHTS_QUERY,
            code,
        ),
        "java" => hl(
            tree_sitter_java::LANGUAGE.into(),
            "java",
            tree_sitter_java::HIGHLIGHTS_QUERY,
            code,
        ),
        "javascript" | "js" => hl(
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            code,
        ),
        "json" => hl(
            tree_sitter_json::LANGUAGE.into(),
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY,
            code,
        ),
        "lua" => hl(
            tree_sitter_lua::LANGUAGE.into(),
            "lua",
            tree_sitter_lua::HIGHLIGHTS_QUERY,
            code,
        ),
        "ocaml" => hl(
            tree_sitter_ocaml::LANGUAGE_OCAML.into(),
            "ocaml",
            tree_sitter_ocaml::HIGHLIGHTS_QUERY,
            code,
        ),
        "php" => hl(
            tree_sitter_php::LANGUAGE_PHP.into(),
            "php",
            tree_sitter_php::HIGHLIGHTS_QUERY,
            code,
        ),
        "python" | "py" => hl(
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
            code,
        ),
        "rust" | "rs" => hl(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            code,
        ),
        "scala" => hl(
            tree_sitter_scala::LANGUAGE.into(),
            "scala",
            tree_sitter_scala::HIGHLIGHTS_QUERY,
            code,
        ),
        "tsx" => hl(
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            code,
        ),
        "typescript" | "ts" => hl(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            code,
        ),
        "yaml" | "yml" => hl(
            tree_sitter_yaml::LANGUAGE.into(),
            "yaml",
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
            code,
        ),
        _ => HighlightInfo::Unhighlighted,
    }
}

/// Run the highlighter for one grammar.
///
/// A query-configuration error or a highlighting error degrades to
/// [`HighlightInfo::Unhighlighted`] rather than propagating — code
/// always renders, with or without colors.
fn hl(
    language: tree_sitter::Language,
    name: &str,
    highlights_query: &str,
    code: &[u8],
) -> HighlightInfo {
    use tree_sitter_highlight::{HighlightConfiguration, Highlighter};

    let Ok(mut config) = HighlightConfiguration::new(language, name, highlights_query, "", "")
    else {
        return HighlightInfo::Unhighlighted;
    };
    config.configure(&HIGHLIGHT_NAMES);

    let mut highlighter = Highlighter::new();
    match highlighter.highlight(&config, code, None, |_| None) {
        Ok(iter) => match iter.collect::<Result<Vec<_>, _>>() {
            Ok(events) => HighlightInfo::Highlighted(events),
            Err(_) => HighlightInfo::Unhighlighted,
        },
        Err(_) => HighlightInfo::Unhighlighted,
    }
}
