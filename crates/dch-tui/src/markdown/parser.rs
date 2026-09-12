//! Pest-based markdown parsing into the component tree.
//!
//! The grammar (`md.pest`) recognizes the markdown dialect models
//! actually emit — including malformed tables and lazy list numbering —
//! and this module turns its parse tree into block-level
//! [`TextComponent`]s the renderer can lay out and paint.

use pest::Parser as _;
use pest::iterators::Pairs;
use pest_derive::Parser;

use super::text_component::{TextComponent, TextNode};
use super::word::{MetaData, Word, WordType};

/// The pest parser instance bound to the markdown grammar.
#[derive(Parser)]
#[grammar = "markdown/md.pest"]
struct MdParser;

/// Parse-enum mapping from grammar `Rule` variants.
///
/// Collapses the grammar's many rule spellings (per-word, per-line, and
/// wrapper variants) into one discriminant per construct, which is all
/// the word classification and component builders distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MdParseEnum {
    /// Image alt text.
    ///
    /// Image alt text the paragraph path folds into plain words.
    AltText,

    /// A blank line between blocks.
    ///
    /// Becomes a one-row line-break component between blocks.
    BlockSeparator,

    /// A bold-emphasized word.
    ///
    /// The per-word inner match of a bold wrapper.
    Bold,

    /// The full bold wrapper with markers.
    ///
    /// The full wrapper including its `**` markers.
    BoldStr,

    /// A bold-italic-emphasized word.
    ///
    /// The per-word inner match of a bold-italic wrapper.
    BoldItalic,

    /// The full bold-italic wrapper with markers.
    ///
    /// The full wrapper including its `***` markers.
    BoldItalicStr,

    /// A `[!CAUTION]` admonition block.
    ///
    /// Admonition blocks render like quotes with their own marker.
    Caution,

    /// An inline-code word.
    ///
    /// The per-word inner match of an inline-code span.
    Code,

    /// A fenced or indented code block.
    ///
    /// Both fenced and indented forms arrive here.
    CodeBlock,

    /// One line inside a fenced code block.
    ///
    /// One line of a fenced block, tabs already expanded.
    CodeBlockStr,

    /// One line inside an indented code block.
    ///
    /// One line of an indented block, leading spaces intact.
    CodeBlockStrSpaceIndented,

    /// The full inline-code wrapper with markers.
    ///
    /// The full inline span including its backticks.
    CodeStr,

    /// A digit inside an ordered-list marker.
    ///
    /// The numbered part of an ordered-list marker.
    Digit,

    /// A footnote reference, inline.
    ///
    /// The `[^label]` reference inline in text.
    FootnoteRef,

    /// A footnote definition block.
    ///
    /// A definition block led by its label.
    Footnote,

    /// A heading line.
    ///
    /// Any of the six levels; the level counts from the leading `#` run.
    Heading,

    /// A horizontal rule.
    ///
    /// Becomes the one-row rule component.
    HorizontalSeparator,

    /// An image.
    ///
    /// Images degrade to their alt text as a paragraph.
    Image,

    /// An `[!IMPORTANT]` admonition block.
    ///
    /// Admonition blocks render like quotes with their own marker.
    Important,

    /// Leading indentation of a list item.
    ///
    /// The leading whitespace that sets a list item's nesting.
    Indent,

    /// An angle-bracket inline link.
    ///
    /// Angle-bracket link forms collapse into link words.
    InlineLink,

    /// An italic-emphasized word.
    ///
    /// The per-word inner match of an italic wrapper.
    Italic,

    /// The full italic wrapper with markers.
    ///
    /// The full wrapper including its markers.
    ItalicStr,

    /// A link's visible text.
    ///
    /// The visible label words of any link spelling.
    Link,

    /// The URL behind a link.
    ///
    /// Non-renderable: the URL behind the label.
    LinkData,

    /// The wrapper around a run of list items.
    ///
    /// Wraps a run of items the list builder walks child by child.
    ListContainer,

    /// A `[!NOTE]` admonition block.
    ///
    /// Admonition blocks render like quotes with their own marker.
    Note,

    /// An ordered list.
    ///
    /// Items the layout renumbers sequentially.
    OrderedList,

    /// A code fence's programming language.
    ///
    /// The raw fence string, stored unmodified for the renderer.
    PLanguage,

    /// A paragraph.
    ///
    /// Also the fallback for constructs with no dedicated builder.
    Paragraph,

    /// A blockquote.
    ///
    /// Blockquote lines the paragraph transform wraps narrower.
    Quote,

    /// A sentence inside any block.
    ///
    /// An intermediate wrapper the leaf collector descends through.
    Sentence,

    /// A strikethrough-emphasized word.
    ///
    /// The per-word inner match of a strikethrough wrapper.
    Strikethrough,

    /// The full strikethrough wrapper with markers.
    ///
    /// The full wrapper including its `~~` markers.
    StrikethroughStr,

    /// A table.
    ///
    /// Cells and separator rows become the table component's rows and metadata.
    Table,

    /// One cell of a table row.
    ///
    /// One cell of a data row, wrapped to its column later.
    TableCell,

    /// A table's separator row.
    ///
    /// The dash row; one metadata word per column comes from it.
    TableSeparator,

    /// A GFM task-list item.
    ///
    /// Checkbox items whose marker records the state.
    Task,

    /// A completed task marker `[x]`.
    ///
    /// A `[x]` marker the renderer paints as done.
    TaskClosed,

    /// An open task marker `[ ]`.
    ///
    /// A `[ ]` marker the renderer paints as pending.
    TaskOpen,

    /// A `[!TIP]` admonition block.
    ///
    /// Admonition blocks render like quotes with their own marker.
    Tip,

    /// An unordered list.
    ///
    /// Items the layout prefixes with bullets.
    UnorderedList,

    /// A `[!WARNING]` admonition block.
    ///
    /// Admonition blocks render like quotes with their own marker.
    Warning,

    /// A wiki-style link.
    ///
    /// Double-bracket wiki spellings collapse into link words.
    WikiLink,

    /// A plain word.
    ///
    /// The plain-word rule several inline grammars share.
    Word,
}

impl From<Rule> for MdParseEnum {
    fn from(value: Rule) -> Self {
        match value {
            Rule::word | Rule::h_word | Rule::h_rest | Rule::latex_word | Rule::t_word => {
                Self::Word
            }
            Rule::indent => Self::Indent,
            Rule::italic_word_var_1 | Rule::italic_word_var_2 => Self::Italic,
            Rule::italic_var_1 | Rule::italic_var_2 => Self::ItalicStr,
            Rule::bold_word => Self::Bold,
            Rule::bold => Self::BoldStr,
            Rule::bold_italic_word => Self::BoldItalic,
            Rule::bold_italic => Self::BoldItalicStr,
            Rule::strikethrough_word => Self::Strikethrough,
            Rule::strikethrough => Self::StrikethroughStr,
            Rule::code_word => Self::Code,
            Rule::code => Self::CodeStr,
            Rule::programming_language => Self::PLanguage,
            Rule::link_word | Rule::link_line | Rule::link | Rule::wiki_link_word => Self::Link,
            Rule::wiki_link_alone => Self::WikiLink,
            Rule::inline_link | Rule::inline_link_wrapper => Self::InlineLink,
            Rule::o_list_counter | Rule::digit => Self::Digit,
            Rule::task_open => Self::TaskOpen,
            Rule::task_complete => Self::TaskClosed,
            Rule::code_line => Self::CodeBlockStr,
            Rule::indented_code_line | Rule::indented_code_newline => {
                Self::CodeBlockStrSpaceIndented
            }
            Rule::sentence | Rule::t_sentence | Rule::footnote_sentence => Self::Sentence,
            Rule::table_cell => Self::TableCell,
            Rule::table_separator => Self::TableSeparator,
            Rule::u_list => Self::UnorderedList,
            Rule::o_list => Self::OrderedList,
            Rule::h1 | Rule::h2 | Rule::h3 | Rule::h4 | Rule::h5 | Rule::h6 | Rule::heading => {
                Self::Heading
            }
            Rule::list_container => Self::ListContainer,
            Rule::code_block | Rule::indented_code_block => Self::CodeBlock,
            Rule::table => Self::Table,
            Rule::quote => Self::Quote,
            Rule::task => Self::Task,
            Rule::block_sep => Self::BlockSeparator,
            Rule::horizontal_sep => Self::HorizontalSeparator,
            Rule::link_data | Rule::wiki_link_data => Self::LinkData,
            Rule::warning => Self::Warning,
            Rule::note => Self::Note,
            Rule::tip => Self::Tip,
            Rule::important => Self::Important,
            Rule::caution => Self::Caution,
            Rule::image => Self::Image,
            Rule::alt_word | Rule::alt_text => Self::AltText,
            Rule::footnote_ref => Self::FootnoteRef,
            Rule::footnote => Self::Footnote,
            _ => Self::Paragraph,
        }
    }
}

/// The root of a parsed markdown document.
///
/// Owns the block-level components in document order and re-runs their
/// layout transforms when the terminal width changes.
#[derive(Debug, Clone)]
pub struct ComponentRoot {
    /// The block-level components, in document order.
    ///
    /// Built once by the builders; only the layout transforms mutate what it holds.
    components: Vec<TextComponent>,
}

impl ComponentRoot {
    /// Wrap finished components into a document root.
    #[must_use]
    pub fn new(components: Vec<TextComponent>) -> Self {
        Self { components }
    }

    /// References to each block-level component, in document order.
    #[must_use]
    pub fn components(&self) -> Vec<&TextComponent> {
        self.components.iter().collect()
    }

    /// Re-run every component's layout for a new `width`.
    ///
    /// Called on terminal resize: wrapping, list indentation, and table
    /// column fitting are recomputed from the parsed words without
    /// re-parsing the source markdown.
    pub fn transform(&mut self, width: u16) {
        for component in &mut self.components {
            component.transform(width);
        }
    }
}

/// Parse markdown text into a laid-out [`ComponentRoot`].
///
/// `width` is the target terminal column count; block transforms
/// (word-wrap, list indentation, table column fit) run immediately so
/// the returned tree is render-ready. Unrecoverable input yields an
/// empty root — never a panic.
#[must_use]
pub fn parse_markdown(content: &str, width: u16) -> ComponentRoot {
    let root: Pairs<'_, Rule> = if let Ok(file) = MdParser::parse(Rule::txt, content) {
        file
    } else {
        return ComponentRoot::new(Vec::new());
    };

    let Some(root_pair) = root.into_iter().next() else {
        return ComponentRoot::new(Vec::new());
    };
    let children: Vec<ParseNode> = root_pair_into_children(root_pair);

    let parse_root = ParseRoot::new(children);
    let mut root = node_to_component(&parse_root);
    root.transform(width);
    root
}

/// The pre-component parse tree's root.
///
/// A thin wrapper holding the top-level block nodes before they become
/// components.
#[derive(Debug, Clone)]
struct ParseRoot {
    /// Top-level block nodes, in document order.
    ///
    /// Leaves are the leaf collector's input; wrappers exist to be descended.
    children: Vec<ParseNode>,
}

impl ParseRoot {
    /// Wrap top-level nodes.
    fn new(children: Vec<ParseNode>) -> Self {
        Self { children }
    }
}

/// One node of the pre-component parse tree.
///
/// Carries the grammar rule it came from (as [`MdParseEnum`]), its
/// matched text, and child nodes; the component builders walk these.
#[derive(Debug, Clone)]
struct ParseNode {
    /// The collapsed grammar rule this node matched.
    ///
    /// The collapsed discriminant the word classification and builders dispatch on.
    kind: MdParseEnum,
    /// The node's matched text, post-processed per rule kind.
    ///
    /// Code lines keep expanded tabs; other rules fold newlines to spaces.
    content: String,
    /// Child nodes, innermost last.
    ///
    /// Leaves are the leaf collector's input; wrappers exist to be descended.
    children: Vec<ParseNode>,
}

impl ParseNode {
    /// Create a leaf node from a kind and its text.
    fn new(kind: MdParseEnum, content: String) -> Self {
        Self {
            kind,
            content,
            children: Vec::new(),
        }
    }

    /// The node's collapsed rule.
    fn kind(&self) -> MdParseEnum {
        self.kind
    }

    /// The node's matched text.
    fn content(&self) -> &str {
        &self.content
    }
}

/// Recursively convert a pest pair into a parse node.
///
/// Code lines keep their tabs expanded to spaces and carriage returns
/// dropped; every other rule's text has newlines folded to spaces so
/// multi-line matches behave as flowing text.
fn parse_text(pair: pest::iterators::Pair<'_, Rule>) -> ParseNode {
    let kind: MdParseEnum = pair.as_rule().into();
    let content = if pair.as_rule() == Rule::code_line {
        pair.as_str().replace('\t', "    ").replace('\r', "")
    } else {
        pair.as_str().replace('\n', " ")
    };
    let children = pair.into_inner().map(parse_text).collect();
    ParseNode {
        kind,
        content,
        children,
    }
}

/// Collect the root pair's child nodes.
fn root_pair_into_children(root_pair: pest::iterators::Pair<'_, Rule>) -> Vec<ParseNode> {
    parse_text(root_pair).children
}

/// Convert a parse root into a component root.
fn node_to_component(root: &ParseRoot) -> ComponentRoot {
    let components: Vec<TextComponent> = root.children.iter().map(parse_component).collect();
    ComponentRoot::new(components)
}

/// Convert one block-level parse node into its component.
fn parse_component(node: &ParseNode) -> TextComponent {
    match node.kind() {
        MdParseEnum::Heading => build_heading(node),
        MdParseEnum::CodeBlock => build_code_block(node),
        MdParseEnum::ListContainer => build_list(node),
        MdParseEnum::Quote => build_quote(node),
        MdParseEnum::Task => build_task(node),
        MdParseEnum::Table => build_table(node),
        MdParseEnum::BlockSeparator => TextComponent::new(TextNode::LineBreak, Vec::new()),
        MdParseEnum::HorizontalSeparator => {
            TextComponent::new(TextNode::HorizontalSeparator, Vec::new())
        }
        MdParseEnum::Footnote => build_footnote(node),
        _ => build_paragraph(node),
    }
}

/// Build a heading component, recording its level in metadata.
fn build_heading(node: &ParseNode) -> TextComponent {
    let indent = node.content().chars().take_while(|c| *c == '#').count();
    let mut words = Vec::new();

    words.push(Word::new(
        String::new(),
        WordType::MetaInfo(MetaData::HeadingLevel(
            u8::try_from(indent).unwrap_or(u8::MAX),
        )),
    ));

    // With atomic heading rules, the entire heading is captured as raw
    // text; extract the content after the `# ` prefix and split it into
    // words with proper space separation.
    let full_text = node.content();
    let content = full_text.find(' ').map_or("", |i| {
        let start = i.saturating_add(1);
        if start < full_text.len() && full_text.is_char_boundary(start) {
            // `start` is verified as a char boundary above.
            #[allow(clippy::string_slice)]
            full_text[start..].trim()
        } else {
            ""
        }
    });

    if !content.is_empty() {
        let mut first = true;
        for part in content.split_whitespace() {
            if !first {
                words.push(Word::new(" ".to_owned(), WordType::Normal));
            }
            words.push(Word::new(part.to_owned(), WordType::Normal));
            first = false;
        }
    }

    TextComponent::new(TextNode::Heading, words)
}

/// Build a paragraph component from a block's leaf words.
fn build_paragraph(node: &ParseNode) -> TextComponent {
    let leaf_nodes = get_leaf_nodes(node);
    let words = collect_words(&leaf_nodes);
    TextComponent::new(TextNode::Paragraph, words)
}

/// Build a code-block component, one word-row per source line.
///
/// The fence's language, when present, is captured into metadata as the
/// renderer's hand-off for syntax highlighting.
fn build_code_block(node: &ParseNode) -> TextComponent {
    let leaf_nodes = get_leaf_nodes(node);
    let mut rows: Vec<Vec<Word>> = Vec::new();
    let mut language: Option<String> = None;

    for ln in &leaf_nodes {
        if ln.kind() == MdParseEnum::PLanguage {
            language = Some(ln.content().to_owned());
        }
        let word_type = WordType::from(ln.kind());
        let content = ln.content().to_owned();
        rows.push(vec![Word::new(content, word_type)]);
    }

    let mut meta = Vec::new();
    if let Some(lang) = language {
        meta.push(Word::new(lang, WordType::MetaInfo(MetaData::PLanguage)));
    }

    TextComponent::new_formatted_with_meta(TextNode::CodeBlock, rows, meta)
}

/// Build a list component from its items, recording kinds and indents.
fn build_list(node: &ParseNode) -> TextComponent {
    let mut words: Vec<Vec<Word>> = Vec::new();
    let mut meta = Vec::new();

    for child in &node.children {
        let kind = child.kind();
        let leaf_nodes = get_leaf_nodes(child);
        let mut inner_words = Vec::new();

        for ln in &leaf_nodes {
            let word_type = WordType::from(ln.kind());

            let mut content = if matches!(ln.kind(), MdParseEnum::Indent) {
                ln.content().to_owned()
            } else {
                collapse_spaces(ln.content())
            };

            if matches!(ln.kind(), MdParseEnum::WikiLink | MdParseEnum::InlineLink) {
                inner_words.push(Word::new(content.clone(), WordType::LinkData));
            }

            if content.starts_with(' ') && !matches!(ln.kind(), MdParseEnum::Indent) {
                content.remove(0);
                inner_words.push(Word::new(" ".to_owned(), word_type));
            }

            inner_words.push(Word::new(content, word_type));
        }

        if kind == MdParseEnum::UnorderedList {
            inner_words.push(Word::new(
                "X".to_owned(),
                WordType::MetaInfo(MetaData::UList),
            ));
            inner_words.insert(1, Word::new("• ".to_owned(), WordType::ListMarker));
            meta.push(Word::new(
                String::new(),
                WordType::MetaInfo(MetaData::UList),
            ));
        } else if kind == MdParseEnum::OrderedList {
            inner_words.push(Word::new(
                "X".to_owned(),
                WordType::MetaInfo(MetaData::OList),
            ));
            meta.push(Word::new(
                String::new(),
                WordType::MetaInfo(MetaData::OList),
            ));
        }

        if let Some(first) = inner_words.first() {
            if matches!(
                first.kind(),
                WordType::MetaInfo(MetaData::UList | MetaData::OList)
            ) {
                // List-kind metadata words are already tracked above.
            } else if first.kind() == WordType::from(MdParseEnum::Indent) {
                meta.push(first.clone());
            }
        }

        words.push(inner_words);
    }

    TextComponent::new_formatted_with_meta(TextNode::List, words, meta)
}

/// Deduplicate consecutive spaces, preserving all other characters.
///
/// Indentation width is what matters downstream, not the exact run
/// length, so collapsed runs keep the layout meaningful.
fn collapse_spaces(content: &str) -> String {
    let mut result = String::new();
    let mut prev_space = false;
    for c in content.chars() {
        if c == ' ' {
            if !prev_space {
                result.push(c);
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result
}

/// Build a blockquote component from its leaf words.
fn build_quote(node: &ParseNode) -> TextComponent {
    let leaf_nodes = get_leaf_nodes(node);
    let words = collect_words(&leaf_nodes);
    TextComponent::new(TextNode::Quote, words)
}

/// Build a task-list-item component from its leaf words.
fn build_task(node: &ParseNode) -> TextComponent {
    let leaf_nodes = get_leaf_nodes(node);
    let words = collect_words(&leaf_nodes);
    TextComponent::new(TextNode::Task, words)
}

/// Build a table component, one word-row per cell.
///
/// The separator row becomes column-count metadata; empty cells are
/// preserved so header and data rows keep their alignment, and trailing
/// phantom cells from a missing final newline are trimmed.
fn build_table(node: &ParseNode) -> TextComponent {
    let mut words: Vec<Vec<Word>> = Vec::new();
    let mut meta = Vec::new();

    for cell in &node.children {
        if cell.kind() == MdParseEnum::TableSeparator {
            meta.push(Word::new(
                cell.content().to_owned(),
                WordType::MetaInfo(MetaData::ColumnsCount),
            ));
            continue;
        }
        if cell.children.is_empty() {
            words.push(Vec::new());
            continue;
        }
        let mut inner_words = Vec::new();
        let leaf_nodes = get_leaf_nodes(cell);
        for ln in &leaf_nodes {
            let word_type = WordType::from(ln.kind());
            let (content, had_leading_space, _had_trailing_space) =
                clean_markdown_content(ln.content(), ln.kind());

            if had_leading_space && !inner_words.is_empty() {
                inner_words.push(Word::new(" ".to_owned(), word_type));
            }

            if matches!(ln.kind(), MdParseEnum::WikiLink | MdParseEnum::InlineLink) {
                inner_words.push(Word::new(content.clone(), WordType::LinkData));
            }

            if !content.is_empty() {
                inner_words.push(Word::new(content, word_type));
            }
        }
        words.push(inner_words);
    }

    // A trailing `|` with no final newline parses as the start of one
    // more empty cell; trim such phantom cells back to the column count.
    let column_count = meta.len();
    if column_count > 0 && !words.len().is_multiple_of(column_count) {
        while !words.len().is_multiple_of(column_count) {
            if matches!(words.last(), Some(cell) if cell.is_empty()) {
                words.pop();
            } else {
                break;
            }
        }
    }

    TextComponent::new_formatted_with_meta(TextNode::Table(vec![], vec![]), words, meta)
}

/// Build a footnote component from its label and body.
fn build_footnote(node: &ParseNode) -> TextComponent {
    let mut words = Vec::new();
    if let Some(foot_ref) = node.children.first() {
        words.push(Word::new(
            foot_ref.content().to_owned(),
            WordType::FootnoteData,
        ));
        let rest: String = node
            .children
            .iter()
            .skip(1)
            .map(|c| c.content.as_str())
            .collect();
        words.push(Word::new(rest, WordType::Footnote));
    }
    TextComponent::new(TextNode::Footnote, words)
}

/// Collect the leaf nodes of a parse subtree, in order.
///
/// Inserts separator words around links and emphasized wrappers whose
/// matched text begins with a space, so word spacing survives the
/// emphasis stripping.
fn get_leaf_nodes(node: &ParseNode) -> Vec<ParseNode> {
    let mut leaf_nodes = Vec::new();

    if node.kind() == MdParseEnum::Link {
        let comp = if node.content().starts_with(' ') {
            ParseNode::new(MdParseEnum::Word, " ".to_owned())
        } else {
            ParseNode::new(MdParseEnum::Word, String::new())
        };
        leaf_nodes.push(comp);
    }

    if matches!(
        node.kind(),
        MdParseEnum::CodeStr
            | MdParseEnum::ItalicStr
            | MdParseEnum::BoldStr
            | MdParseEnum::BoldItalicStr
            | MdParseEnum::StrikethroughStr
    ) && node.content().starts_with(' ')
    {
        leaf_nodes.push(ParseNode::new(MdParseEnum::Word, " ".to_owned()));
    }

    if node.children.is_empty() {
        leaf_nodes.push(node.clone());
    } else {
        for child in &node.children {
            leaf_nodes.append(&mut get_leaf_nodes(child));
        }
    }
    leaf_nodes
}

/// Strip markdown syntax markers from content by parsing context.
///
/// Returns the cleaned content plus whether the original had leading
/// and trailing spaces, so callers can restore inter-word spacing the
/// markers consumed.
fn clean_markdown_content(content: &str, kind: MdParseEnum) -> (String, bool, bool) {
    let had_leading_space = content.starts_with(' ');
    let had_trailing_space = content.ends_with(' ');
    let trimmed = content.trim();
    let cleaned = match kind {
        MdParseEnum::Bold | MdParseEnum::BoldStr => trimmed
            .trim_start_matches('*')
            .trim_start_matches('*')
            .trim_end_matches('*')
            .trim_end_matches('*')
            .to_string(),
        MdParseEnum::BoldItalic => trimmed
            .trim_start_matches('*')
            .trim_start_matches('*')
            .trim_start_matches('*')
            .trim_end_matches('*')
            .trim_end_matches('*')
            .trim_end_matches('*')
            .to_string(),
        MdParseEnum::Italic | MdParseEnum::ItalicStr => trimmed
            .trim_start_matches('*')
            .trim_start_matches('_')
            .trim_end_matches('*')
            .trim_end_matches('_')
            .to_string(),
        MdParseEnum::Strikethrough | MdParseEnum::StrikethroughStr => trimmed
            .trim_start_matches('~')
            .trim_start_matches('~')
            .trim_end_matches('~')
            .trim_end_matches('~')
            .to_string(),
        MdParseEnum::Code | MdParseEnum::CodeStr => trimmed
            .trim_start_matches('`')
            .trim_end_matches('`')
            .to_string(),
        MdParseEnum::Link => {
            // Content is `[text](url)`: keep only the bracketed label.
            if let Some(start) = trimmed.find('[') {
                let from = start.saturating_add(1);
                if from < trimmed.len() && trimmed.is_char_boundary(from) {
                    // `from` is verified as a char boundary above.
                    #[allow(clippy::string_slice)]
                    let inner = &trimmed[from..];
                    if let Some(end) = inner.find(']') {
                        let to = from.saturating_add(end);
                        if trimmed.is_char_boundary(to) {
                            // `to` is verified as a char boundary above.
                            #[allow(clippy::string_slice)]
                            return (
                                trimmed[from..to].to_string(),
                                had_leading_space,
                                had_trailing_space,
                            );
                        }
                    }
                }
            }
            trimmed.to_string()
        }
        _ => trimmed.to_string(),
    };
    (cleaned, had_leading_space, had_trailing_space)
}

/// Convert leaf nodes into a flat word list.
///
/// Restores inter-word spacing from the leafs' leading spaces and
/// records link URLs as non-renderable words alongside their labels.
fn collect_words(leaf_nodes: &[ParseNode]) -> Vec<Word> {
    let mut words = Vec::new();
    for ln in leaf_nodes {
        let word_type = WordType::from(ln.kind());
        let (content, had_leading_space, _had_trailing_space) =
            clean_markdown_content(ln.content(), ln.kind());

        if had_leading_space && !words.is_empty() {
            words.push(Word::new(" ".to_owned(), WordType::Normal));
        }

        if matches!(ln.kind(), MdParseEnum::WikiLink | MdParseEnum::InlineLink) {
            words.push(Word::new(content.clone(), WordType::LinkData));
        }

        if !content.is_empty() {
            words.push(Word::new(content, word_type));
        }
    }
    words
}
