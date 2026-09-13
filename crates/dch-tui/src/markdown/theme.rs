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

/// The capture palette, owned by this module.
///
/// One named color per [`SyntaxCapture`] variant, plus `plain` for
/// unhighlighted code. The host maps its own palette in
/// field-for-field; the renderer resolves captures and the
/// highlighter's emit-order array from it. One field per variant means
/// a new capture forces its matching color — the palette and the
/// vocabulary cannot drift apart silently.
#[derive(Debug, Clone, Copy)]
pub struct SyntaxTheme {
    /// Color for unhighlighted code and [`SyntaxCapture::None`].
    ///
    /// Every code word carries this color until a highlighter tags it;
    /// it is the fallback whenever highlighting is off, fails, or meets
    /// an unknown language.
    pub plain: Color,

    /// Color for attribute captures.
    ///
    /// Mirrors [`SyntaxCapture::Attribute`]; annotation and decorator
    /// names render in it.
    pub attribute: Color,

    /// Color for comment captures.
    ///
    /// Mirrors [`SyntaxCapture::Comment`]; usually tuned toward the dim
    /// end of the palette.
    pub comment: Color,

    /// Color for constant captures.
    ///
    /// Mirrors [`SyntaxCapture::Constant`]; often paired with the
    /// number color so literal-like values read as one family.
    pub constant: Color,

    /// Color for constructor captures.
    ///
    /// Mirrors [`SyntaxCapture::Constructor`]; usually near the
    /// function color, since constructors read as call sites.
    pub constructor: Color,

    /// Color for embedded-language regions.
    ///
    /// Mirrors [`SyntaxCapture::Embedded`]; template interiors and
    /// injections.
    pub embedded: Color,

    /// Color for function and method names.
    ///
    /// Mirrors [`SyntaxCapture::Function`], covering both `function`
    /// and `function.builtin` captures; the dominant accent in most
    /// code blocks.
    pub function: Color,

    /// Color for keyword captures.
    ///
    /// Mirrors [`SyntaxCapture::Keyword`]; with the string color it
    /// dominates the code-block palette.
    pub keyword: Color,

    /// Color for numeric literals.
    ///
    /// Mirrors [`SyntaxCapture::Number`]; not indexed by the
    /// highlighter's capture list today — a palette entry kept for
    /// richer custom rendering.
    pub number: Color,

    /// Color for operator captures.
    ///
    /// Mirrors [`SyntaxCapture::Operator`]; bridges keywords and
    /// punctuation in visual weight.
    pub operator: Color,

    /// Color for property and field accesses.
    ///
    /// Mirrors [`SyntaxCapture::Property`]; usually near the variable
    /// color so `obj.field` chains read as one unit.
    pub property: Color,

    /// Color for punctuation and bracket captures.
    ///
    /// Mirrors [`SyntaxCapture::Punctuation`], covering `punctuation`
    /// and `punctuation.bracket`; kept quieter than the token colors it
    /// frames.
    pub punctuation: Color,

    /// Color for string literals.
    ///
    /// Mirrors [`SyntaxCapture::String`]; one of the two
    /// highest-frequency token classes.
    pub string: Color,

    /// Color for type captures.
    ///
    /// Mirrors [`SyntaxCapture::Type`], covering `type` and
    /// `type.builtin`; declarations and annotations alike.
    pub r#type: Color,

    /// Color for plain variable bindings and parameters.
    ///
    /// Mirrors [`SyntaxCapture::Variable`], covering `variable` and
    /// `variable.parameter`; stays near the default foreground.
    pub variable: Color,

    /// Color for builtin variables such as `self`.
    ///
    /// Mirrors [`SyntaxCapture::VariableBuiltin`], the
    /// `variable.builtin` capture; a step apart from plain variables.
    pub variable_builtin: Color,

    /// Color for markup tags.
    ///
    /// Mirrors [`SyntaxCapture::Tag`]; often echoes the keyword color,
    /// since tags play keywords' structural role in markup.
    pub tag: Color,

    /// Color for delimiter captures.
    ///
    /// Mirrors [`SyntaxCapture::Delimiter`], the
    /// `punctuation.delimiter` capture; dimmer than brackets so
    /// nesting reads before separators.
    pub delimiter: Color,

    /// Color for escape sequences.
    ///
    /// Mirrors [`SyntaxCapture::Escape`], the `string.special`
    /// capture; typically an alerting accent inside strings.
    pub escape: Color,
}

impl SyntaxTheme {
    /// Resolve a capture to its color.
    ///
    /// [`SyntaxCapture::None`] resolves to `plain` — the same color
    /// unhighlighted code uses, so untagged words and failed
    /// highlighting agree by construction.
    #[must_use]
    pub fn color(&self, capture: SyntaxCapture) -> Color {
        match capture {
            SyntaxCapture::None => self.plain,
            SyntaxCapture::Attribute => self.attribute,
            SyntaxCapture::Comment => self.comment,
            SyntaxCapture::Constant => self.constant,
            SyntaxCapture::Constructor => self.constructor,
            SyntaxCapture::Embedded => self.embedded,
            SyntaxCapture::Function => self.function,
            SyntaxCapture::Keyword => self.keyword,
            SyntaxCapture::Number => self.number,
            SyntaxCapture::Operator => self.operator,
            SyntaxCapture::Property => self.property,
            SyntaxCapture::Punctuation => self.punctuation,
            SyntaxCapture::String => self.string,
            SyntaxCapture::Type => self.r#type,
            SyntaxCapture::Variable => self.variable,
            SyntaxCapture::VariableBuiltin => self.variable_builtin,
            SyntaxCapture::Tag => self.tag,
            SyntaxCapture::Delimiter => self.delimiter,
            SyntaxCapture::Escape => self.escape,
        }
    }

    /// Project the palette into the highlighter's emit order.
    ///
    /// The order is the highlighter's `HIGHLIGHT_NAMES` list:
    /// attribute, constant, function.builtin, function, keyword,
    /// operator, property, punctuation, punctuation.bracket,
    /// punctuation.delimiter, string, string.special, tag, type,
    /// type.builtin, variable, variable.builtin, variable.parameter.
    /// Captures the highlighter folds together share a field, and
    /// `comment`, `number`, `constructor`, and `embedded` are not
    /// indexed — they remain palette entries for richer rendering.
    #[must_use]
    pub fn emit_order_colors(&self) -> [Color; 18] {
        [
            self.attribute,
            self.constant,
            self.function,
            self.function,
            self.keyword,
            self.operator,
            self.property,
            self.punctuation,
            self.punctuation,
            self.delimiter,
            self.string,
            self.escape,
            self.tag,
            self.r#type,
            self.r#type,
            self.variable,
            self.variable_builtin,
            self.variable,
        ]
    }
}

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

    /// Color for the box-drawing borders the renderer draws.
    ///
    /// Table borders and other structural chrome paint in it; the host
    /// maps its UI border color in.
    pub border: Color,

    /// Color for de-emphasized chrome text.
    ///
    /// The quote prefix and link-URL words paint in it; the host maps
    /// its dim UI color in.
    pub dim: Color,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]

    use super::*;

    fn palette() -> SyntaxTheme {
        SyntaxTheme {
            plain: Color::White,
            attribute: Color::Indexed(1),
            comment: Color::Indexed(2),
            constant: Color::Indexed(3),
            constructor: Color::Indexed(4),
            embedded: Color::Indexed(5),
            function: Color::Indexed(6),
            keyword: Color::Indexed(7),
            number: Color::Indexed(8),
            operator: Color::Indexed(9),
            property: Color::Indexed(10),
            punctuation: Color::Indexed(11),
            string: Color::Green,
            r#type: Color::Cyan,
            variable: Color::Indexed(12),
            variable_builtin: Color::Indexed(13),
            tag: Color::Indexed(14),
            delimiter: Color::Indexed(15),
            escape: Color::Indexed(16),
        }
    }

    #[test]
    fn color_resolves_every_capture_to_its_field() {
        let theme = palette();
        let cases = [
            (SyntaxCapture::None, theme.plain),
            (SyntaxCapture::Attribute, theme.attribute),
            (SyntaxCapture::Comment, theme.comment),
            (SyntaxCapture::Constant, theme.constant),
            (SyntaxCapture::Constructor, theme.constructor),
            (SyntaxCapture::Embedded, theme.embedded),
            (SyntaxCapture::Function, theme.function),
            (SyntaxCapture::Keyword, theme.keyword),
            (SyntaxCapture::Number, theme.number),
            (SyntaxCapture::Operator, theme.operator),
            (SyntaxCapture::Property, theme.property),
            (SyntaxCapture::Punctuation, theme.punctuation),
            (SyntaxCapture::String, theme.string),
            (SyntaxCapture::Type, theme.r#type),
            (SyntaxCapture::Variable, theme.variable),
            (SyntaxCapture::VariableBuiltin, theme.variable_builtin),
            (SyntaxCapture::Tag, theme.tag),
            (SyntaxCapture::Delimiter, theme.delimiter),
            (SyntaxCapture::Escape, theme.escape),
        ];
        for (capture, expected) in cases {
            assert_eq!(
                theme.color(capture),
                expected,
                "{capture:?} must resolve to its own field"
            );
        }
    }
}
