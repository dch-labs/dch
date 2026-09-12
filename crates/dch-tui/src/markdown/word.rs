//! The word model: styled words and their classifications.

use super::parser::MdParseEnum;
use super::theme::SyntaxCapture;

/// Metadata attached to words for internal processing.
///
/// These words never render. They carry structural facts the builders
/// and layout transforms need — list kinds, the code-block language,
/// table column counts, and heading levels — and travel in a
/// component's `meta_info` where the renderer can consult them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaData {
    /// The word belongs to an unordered list item.
    ///
    /// Marks the item's marker row so the layout can pick the bullet
    /// shape and indentation.
    UList,

    /// The word belongs to an ordered list item.
    ///
    /// Marks the item's marker row so the layout can renumber it.
    OList,

    /// The fenced code block's programming language.
    ///
    /// The raw string from the opening fence (for example `rust`);
    /// alias normalization is a render-time concern.
    PLanguage,

    /// A table separator row's captured cell count.
    ///
    /// One such word per separator column; counting them yields the
    /// table's column count.
    ColumnsCount,

    /// The heading's level, one to six.
    ///
    /// Carried as the count of leading `#` characters.
    HeadingLevel(u8),
}

/// The style classification of a word in markdown.
///
/// Inline emphasis, links, and code map onto distinct variants so the
/// renderer can theme each kind; code-block text carries a
/// [`SyntaxCapture`] tag rather than a color, keeping the parser
/// color-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordType {
    /// Plain text with no emphasis.
    ///
    /// The default classification everything unmarked lands in.
    Normal,

    /// Text inside `**bold**` markers.
    ///
    /// Markers are stripped before the word reaches content.
    Bold,

    /// Text inside `*italic*` or `_italic_` markers.
    ///
    /// Both asterisk and underscore spellings arrive here.
    Italic,

    /// Text inside `***bold italic***` markers.
    ///
    /// Triple-marked spans the renderer styles with both modifiers.
    BoldItalic,

    /// Inline text inside backticks.
    ///
    /// Distinct from code-block text, which carries a capture instead.
    Code,

    /// Code-block text, tagged by its highlight capture.
    ///
    /// The capture names the syntax category (or
    /// [`SyntaxCapture::None`] when no highlighter ran); the renderer
    /// resolves it to a color through the syntax theme.
    CodeBlock(SyntaxCapture),

    /// Text inside `~~strikethrough~~` markers.
    ///
    /// Tilde-marked spans the renderer may strike through.
    Strikethrough,

    /// The visible label of a link.
    ///
    /// The URL travels separately as a `LinkData` word.
    Link,

    /// The URL behind a link.
    ///
    /// Non-renderable metadata the renderer may surface on demand.
    LinkData,

    /// A list bullet or numbering glyph.
    ///
    /// Renumbering and bullet logic rewrite this word's content during layout.
    ListMarker,

    /// A structural metadata word.
    ///
    /// See [`MetaData`] for what it can carry; never rendered.
    MetaInfo(MetaData),

    /// A footnote reference inline in text.
    ///
    /// Marks the reference site; the definition lives in a `Footnote` block.
    FootnoteInline,

    /// The body text of a footnote definition.
    ///
    /// The prose a footnote reference points at.
    Footnote,

    /// The label of a footnote definition.
    ///
    /// Non-renderable metadata pairing the definition with its
    /// reference.
    FootnoteData,
}

/// A single styled word within a markdown document.
///
/// The smallest unit the parser emits: its content plus the
/// classification the renderer themes. Words are the atoms of every
/// block's content rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// The word's text, markers stripped.
    ///
    /// Emphasis and code markers never survive into content; only the
    /// payload does.
    content: String,
    /// How the renderer should style this word.
    ///
    /// Set once at parse time; layout transforms rewrite content but never the classification.
    word_type: WordType,
}

impl Word {
    /// Create a word from content and a classification.
    ///
    /// The parser's builders call this for every emitted token.
    #[must_use]
    pub fn new(content: String, word_type: WordType) -> Self {
        Self { content, word_type }
    }

    /// The word's text content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// The word's style classification.
    #[must_use]
    pub fn kind(&self) -> WordType {
        self.word_type
    }

    /// Replace the word's text, keeping its classification.
    ///
    /// Layout transforms use this when trimming or renumbering content
    /// in place.
    pub fn set_content(&mut self, content: impl Into<String>) {
        self.content = content.into();
    }

    /// Whether this word should be rendered.
    ///
    /// Metadata words (link URLs, footnote labels, structural markers)
    /// are stored alongside content but skipped when painting.
    #[must_use]
    pub fn is_renderable(&self) -> bool {
        !matches!(
            self.word_type,
            WordType::MetaInfo(_) | WordType::LinkData | WordType::FootnoteData
        )
    }
}

/// Convert a parser node kind into a word classification.
///
/// The bridge between the grammar's rule names and the renderer-facing
/// [`WordType`]; code-block text arrives untagged
/// ([`SyntaxCapture::None`]) because highlighting is a render-time
/// concern.
impl From<MdParseEnum> for WordType {
    fn from(value: MdParseEnum) -> Self {
        use MdParseEnum as E;
        match value {
            E::Code => WordType::Code,
            E::Bold => WordType::Bold,
            E::Italic => WordType::Italic,
            E::BoldItalic => WordType::BoldItalic,
            E::Strikethrough => WordType::Strikethrough,
            E::Link | E::WikiLink | E::InlineLink => WordType::Link,
            E::Digit => WordType::ListMarker,
            E::FootnoteRef => WordType::FootnoteInline,
            E::PLanguage => WordType::MetaInfo(MetaData::PLanguage),
            E::LinkData => WordType::LinkData,
            E::CodeBlockStr | E::CodeBlockStrSpaceIndented => {
                WordType::CodeBlock(SyntaxCapture::None)
            }
            _ => WordType::Normal,
        }
    }
}
