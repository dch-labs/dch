//! Theme types owned by the markdown module.
//!
//! These are deliberately parallel to the application's theme types and
//! are the extraction seam: the module defines its own vocabulary and
//! the host maps into it, never the reverse. When the module is promoted
//! to a standalone crate, these types travel with it unchanged.

use ratatui::style::{Color, Style};

/// Which syntax-highlight capture a code span belongs to.
///
/// The capture names a tree-sitter highlight category (for example
/// `keyword`, `string`, `function`) rather than a color: the parser
/// classifies source text without knowing how it will be painted, and
/// the renderer resolves each capture through a [`SyntaxTheme`].
/// `None` marks text no highlighter ran on — unknown languages,
/// highlighting disabled, or a highlighting failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyntaxCapture {
    /// Attribute names, such as `href` inside an HTML tag.
    ///
    /// The name half of a name–value pair in markup — HTML attribute
    /// keys, decorator and annotation arguments — never the assigned
    /// value.
    Attribute,

    /// Comment text of any language.
    ///
    /// Line and block comments land here in full, delimiters included
    /// (`//`, `/* */`, `#`), so a theme can dim them uniformly.
    Comment,

    /// Constant and enumeration values.
    ///
    /// Named constants and enum variants in expression position;
    /// distinct from plain bindings, which land in `Variable`.
    Constant,

    /// Constructor and builder names.
    ///
    /// Calls that build a value — `Some`, `Vec::new`, struct-literal
    /// type names — rather than the type's definition site.
    Constructor,

    /// Host-language text embedded inside another construct, such as
    /// template literals.
    ///
    /// The interior of a region where one language lives inside
    /// another; the outer language's captures resume once the region
    /// closes.
    Embedded,

    /// Function and method names.
    ///
    /// Free functions and methods alike, builtins included, in both
    /// call and definition positions.
    Function,

    /// Language keywords, such as `fn`, `let`, or `return`.
    ///
    /// Reserved words carrying grammar structure rather than a value —
    /// declarations, control flow, and the modifiers a grammar groups
    /// with its keywords.
    Keyword,

    /// Numeric literals of any base, including floats and units.
    ///
    /// Integers, floats, hex and binary spellings, and suffix-bearing
    /// literals like `42u8`, with any in-literal separators attached.
    Number,

    /// Operators, such as `+`, `==`, or `->`.
    ///
    /// Symbolic operators and arrows; word-shaped operators such as
    /// `not` land here too when the grammar marks them as operators.
    Operator,

    /// Property and field names on a value.
    ///
    /// The field half of a member access — the `bar` of `foo.bar` —
    /// in structs, objects, and configuration blocks.
    Property,

    /// Punctuation that is not an operator, such as braces and commas.
    ///
    /// Brackets, braces, and separating commas — structural glue
    /// around other tokens. Delimiter roles with their own visual
    /// weight split off into `Delimiter`.
    Punctuation,

    /// String literals of any flavor, including interpolated ones.
    ///
    /// Quoted text in all its spellings — single, double, raw,
    /// heredoc — with the quote marks attached; escape sequences
    /// inside split off into `Escape`.
    String,

    /// Type names, including structs, enums, and traits.
    ///
    /// Both user-defined and builtin types, wherever a name is used as
    /// a type rather than a value, generic parameters included.
    Type,

    /// Plain variable bindings and parameters.
    ///
    /// Ordinary identifiers — bindings, locals, and call parameters —
    /// in both use and declaration positions.
    Variable,

    /// Builtin and special variables, such as `self` or `$0`.
    ///
    /// Reserved identifiers that behave like variables; kept apart
    /// from plain bindings so a theme can flag them without
    /// pattern-matching names.
    VariableBuiltin,

    /// Markup tags, such as HTML element names.
    ///
    /// Element names in markup and templating languages, opening and
    /// closing alike; attribute names inside the tag land in
    /// `Attribute`.
    Tag,

    /// Delimiters of composite constructs, such as markdown fences.
    ///
    /// Region markers — fences, heredoc boundaries, backtick pairs —
    /// that frame a construct rather than join an expression.
    Delimiter,

    /// Escape sequences, such as `\n` inside a string.
    ///
    /// Backslash sequences and similar escapes, captured while still
    /// inside their literal so a theme can set them apart from the
    /// surrounding text.
    Escape,

    /// No highlighter ran on this text.
    ///
    /// The language was unknown, highlighting was disabled, or the
    /// highlighter failed; the renderer falls back to plain
    /// code-block styling. Every code word carries this variant until
    /// the renderer tags captures.
    None,
}

/// The capture-to-color resolution, owned by this module.
///
/// A plain function table rather than a struct of one field per capture,
/// so it stays small and maps cleanly onto the standalone crate. The
/// host constructs one from its own syntax palette; the renderer calls
/// it per [`SyntaxCapture`] when painting highlighted code.
#[derive(Debug, Clone)]
pub struct SyntaxTheme(pub fn(SyntaxCapture) -> Color);

/// Per-element markdown styles, owned by this module.
///
/// Mirrors the *shape* of the host application's markdown palette but is
/// a distinct type: the host builds one by mapping its own theme in, and
/// the module never imports the host's types. Keeping the two separate
/// is what lets the module leave the host crate without a rewrite.
#[derive(Debug, Clone, Default)]
pub struct MarkdownTheme {
    /// The six heading levels, strongest first.
    ///
    /// Indexed by heading level minus one; renderers clamp levels beyond
    /// six onto the last entry.
    pub header: [Style; 6],
    /// Style for `**bold**` spans.
    ///
    /// Applied to words the parser classified as bold.
    pub bold: Style,
    /// Style for `*italic*` spans.
    ///
    /// Applied to words the parser classified as italic.
    pub italic: Style,
    /// Style for inline `` `code` `` spans.
    ///
    /// Distinguishes inline code from fenced code-block text, which the
    /// renderer paints through [`SyntaxTheme`] instead.
    pub code_inline: Style,
    /// The code-block background or frame color.
    ///
    /// A single color rather than a full style: the block's background
    /// and border share it, while the text inside comes from the
    /// syntax captures.
    pub code_block: Color,
    /// Style for link text.
    ///
    /// Applied to the visible label of a link; the URL travels as a
    /// non-renderable word the renderer may surface on demand.
    pub link: Style,
    /// Style for blockquote text.
    ///
    /// Applied across the whole quote block.
    pub quote: Style,
    /// Style for list-item text.
    ///
    /// Applied to item content; the marker glyphs carry their own
    /// classification.
    pub list_item: Style,
    /// Style for horizontal rules.
    ///
    /// Applied to the rule glyph row the renderer draws.
    pub horizontal_rule: Style,
}
