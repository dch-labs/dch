//! The theme system: named color palettes for syntax, markdown, and chrome.
//!
//! A [`Theme`] bundles the three palettes the TUI consumes: tree-sitter
//! syntax colors, markdown element styles, and application-chrome colors.
//! Themes are compiled-in data selected by name — from `[display] theme` or
//! the `--theme` flag — through [`Theme::by_name`]. A theme is pure data:
//! it carries color only, and components decide how to render it.

use ratatui::style::{Color, Style};

pub(crate) mod theme_data;

/// The tree-sitter capture names, in [`SyntaxTheme::syntax_colors`] slot
/// order.
///
/// This list is the contract between the palette and any syntax
/// highlighter: the highlighter's capture-name list must match it slot for
/// slot, and `syntax_colors` indexes the palette by exactly this order —
/// a name list that drifts from it miscolors every theme silently, which
/// is why the two live together here.
pub const HIGHLIGHT_NAMES: [&str; 18] = [
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

/// A complete color theme: syntax highlighting, markdown rendering, and UI
/// chrome.
///
/// Construct via [`Theme::by_name`] or one of the named constructors
/// (`Theme::dracula`, `Theme::nord`, …). Themes are value types with no
/// behavior beyond [`SyntaxTheme::syntax_colors`]; rendering logic lives with
/// the components that draw.
#[derive(Debug, Clone)]
pub struct Theme {
    /// The theme's display name, as rendered in UI and diagnostics.
    ///
    /// Matches the constructor's conventional spelling ("Dracula", "Gruvbox
    /// Dark") and is independent of the lowercase lookup key used by
    /// [`Theme::by_name`]. Static because every theme is compiled-in data —
    /// a theme cannot be built with a name its constructor does not carry.
    pub name: &'static str,

    /// Colors for the tree-sitter captures highlighted inside code blocks.
    ///
    /// Projected into the highlighter's emit order by
    /// [`SyntaxTheme::syntax_colors`]; each field tunes one capture class.
    pub syntax: SyntaxTheme,

    /// Styles for each markdown element the renderer distinguishes.
    ///
    /// Full [`Style`] values, so modifiers travel with the palette instead
    /// of living in the renderer; see [`MarkdownTheme`].
    pub markdown: MarkdownTheme,

    /// Colors for the application chrome: borders, input, status bar.
    ///
    /// Color-only by design — components add their own modifiers when they
    /// draw; see [`UIStyle`].
    pub ui: UIStyle,
}

/// Colors for the tree-sitter captures the highlighter emits.
///
/// One field per core capture the theme author can tune. The field set is
/// richer than what the highlighter currently indexes — see
/// [`syntax_colors`](Self::syntax_colors) for the projection into the
/// highlighter's emit order.
#[derive(Debug, Clone)]
pub struct SyntaxTheme {
    /// Color for attribute captures: annotations such as Rust's `#[derive]` or
    /// Python's `@decorator`.
    ///
    /// Rendered wherever the highlighter emits `attribute`, typically on
    /// declaration lines; shipped palettes usually echo the function color,
    /// since attributes decorate items.
    pub attribute: Color,

    /// Color for comment captures across the tree.
    ///
    /// Comments render at every nesting depth, so this color sets the muted
    /// register of annotated code and should fall back toward the dim end of
    /// the palette.
    pub comment: Color,

    /// Color for constant captures: `const` items, statics, and units the
    /// highlighter classifies as constants.
    ///
    /// Often paired with the number color so literal-like values read as one
    /// family across a code block.
    pub constant: Color,

    /// Color for constructor captures: tuple-struct and unit-variant
    /// constructors invoked without a path.
    ///
    /// Usually tuned near the function color, since constructors read as call
    /// sites to the eye of the reader.
    pub constructor: Color,

    /// Color for embedded-language regions: HTML injections, markdown code
    /// fences, template blocks.
    ///
    /// A renderer can tint whole injected spans with it rather than
    /// re-highlighting the embedded language token by token.
    pub embedded: Color,

    /// Color for function names, covering the `function` and
    /// `function.builtin` captures.
    ///
    /// This is the dominant accent in most code blocks, so themes typically
    /// give it a mid-saturation tone that stays calm at high frequency.
    pub function: Color,

    /// Color for keyword captures: control flow, declaration modifiers, and
    /// primitive markers.
    ///
    /// Together with the string color it dominates the code-block palette,
    /// being the highest-frequency token class the highlighter emits.
    pub keyword: Color,

    /// Color for numeric literals (the `number` capture).
    ///
    /// Renderers may also apply it to literal values directly; the highlighter
    /// itself folds numeric constants into the constant capture.
    pub number: Color,

    /// Color for operator captures: arithmetic, logical, and assignment
    /// operators.
    ///
    /// Sits between keywords and punctuation in visual weight, so themes pick
    /// a tone that bridges the two.
    pub operator: Color,

    /// Color for property and field accesses (the `property` capture).
    ///
    /// Usually near the variable color so `obj.field` chains read as one unit
    /// rather than alternating tones.
    pub property: Color,

    /// Color for punctuation and bracket captures, both `punctuation` and
    /// `punctuation.bracket`.
    ///
    /// It carries the structural rhythm of code, so themes keep it quiet —
    /// noticeably dimmer than the token colors it frames.
    pub punctuation: Color,

    /// Color for string literals (the `string` capture).
    ///
    /// Together with the keyword color it dominates the code-block palette,
    /// which is why the two are chosen as a pair.
    pub string: Color,

    /// Color for type captures, both `type` and `type.builtin`.
    ///
    /// Applied to declarations and annotations alike, from struct names to
    /// generic parameters.
    pub r#type: Color,

    /// Color for variable captures, including `variable.parameter`.
    ///
    /// The most frequently rendered non-keyword token class, so it stays
    /// close to the default foreground in most palettes.
    pub variable: Color,

    /// Color for builtin variables: `self`, `super`, and language-level
    /// globals (the `variable.builtin` capture).
    ///
    /// Usually a step apart from plain variables so the special ones read
    /// as language-provided rather than user-defined.
    pub variable_builtin: Color,

    /// Color for markup tags: HTML and JSX element names (the `tag`
    /// capture).
    ///
    /// Themes often echo the keyword color here, since tags play the same
    /// structural role in markup that keywords play in code.
    pub tag: Color,

    /// Color for delimiter captures: commas, semicolons, and other
    /// separators (the `punctuation.delimiter` capture).
    ///
    /// Kept dimmer than brackets so nesting structure reads before the
    /// separators that mark it.
    pub delimiter: Color,

    /// Color for escape sequences (the `string.special` capture).
    ///
    /// Rendered inside strings where `\n`, `\t`, and friends appear;
    /// typically an alerting accent so escapes stand out of the literal.
    pub escape: Color,
}

impl SyntaxTheme {
    /// Project the palette into the `[Color; 18]` array the highlighter
    /// indexes.
    ///
    /// The index order is the highlighter's `HIGHLIGHT_NAMES` emit order:
    /// attribute, constant, function.builtin, function, keyword, operator,
    /// property, punctuation, punctuation.bracket, punctuation.delimiter,
    /// string, string.special, tag, type, type.builtin, variable,
    /// variable.builtin, variable.parameter. Captures the highlighter folds
    /// together share a field (`function.builtin` renders as `function`);
    /// four palette entries (`comment`, `constructor`, `embedded`, `number`)
    /// are not indexed today and remain as palette entries for richer custom
    /// rendering. The order is [`HIGHLIGHT_NAMES`]: the list a syntax
    /// highlighter must adopt verbatim as its capture-name order, so the
    /// palette and the highlighter cannot drift apart silently.
    #[must_use]
    pub fn syntax_colors(&self) -> [Color; 18] {
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

/// Styles for each markdown element the renderer distinguishes.
///
/// Unlike [`UIStyle`], these carry full [`Style`] values — modifiers such as
/// bold and italic are semantic to the element, not chrome decisions.
#[derive(Debug, Clone)]
pub struct MarkdownTheme {
    /// Style for level-1 headings, the strongest emphasis a document carries.
    ///
    /// Shipped palettes give it BOLD plus the leading accent of the heading
    /// hue ramp, so section starts are visible at a glance.
    pub header1: Style,

    /// Style for level-2 headings.
    ///
    /// BOLD with the second accent of the heading ramp, a visible step below
    /// level 1 without dropping to body weight.
    pub header2: Style,

    /// Style for level-3 headings.
    ///
    /// BOLD with the third ramp color; the deepest level that still reads as
    /// a section title at conversational font sizes.
    pub header3: Style,

    /// Style for level-4 headings.
    ///
    /// BOLD with the fourth ramp color; below this depth headings are mostly
    /// distinguished by color alone.
    pub header4: Style,

    /// Style for level-5 headings.
    ///
    /// BOLD with the fifth ramp color, aimed at outline-level structure that
    /// rarely appears in chat-length documents.
    pub header5: Style,

    /// Style for level-6 headings, the quietest of the ramp.
    ///
    /// BOLD with the final ramp color; present for completeness of the
    /// markdown contract rather than frequent use.
    pub header6: Style,

    /// Style for bold spans.
    ///
    /// Carries the BOLD modifier with no color change: emphasis comes from
    /// weight, so the surrounding text color applies unchanged.
    pub bold: Style,

    /// Style for italic spans.
    ///
    /// Carries the ITALIC modifier with no color change, keeping quoted or
    /// stressed words in the author's voice.
    pub italic: Style,

    /// Style for inline code rendered inside running text.
    ///
    /// Shipped palettes pair a tinted background with an accent foreground
    /// so code reads as a distinct register without breaking the line.
    pub code_inline: Style,

    /// Style for fenced code blocks.
    ///
    /// The block's base style; token colors layer on top of it from the
    /// syntax palette via [`SyntaxTheme::syntax_colors`].
    pub code_block: Style,

    /// Style for links.
    ///
    /// Shipped palettes add UNDERLINED and a distinct accent so links read
    /// as interactive even in a static transcript.
    pub link: Style,

    /// Style for block quotes.
    ///
    /// Typically the dim color, so quoted material recedes behind the
    /// author's own voice.
    pub quote: Style,

    /// Style for list items and their markers.
    ///
    /// The renderer applies it to the bullet or number; the item's content
    /// inherits from the surrounding context.
    pub list_item: Style,

    /// Style for horizontal rules.
    ///
    /// Applied to the drawn line only; themes keep it at the dim level so
    /// section breaks whisper rather than shout.
    pub horizontal_rule: Style,
}

/// Colors for the application chrome: borders, input area, status bar.
///
/// Color-only by design — a theme defines the palette, and components add
/// their own modifiers (bold status text, dim timestamps). The field set is
/// foreground-centric; where a concept also has a background in richer
/// renderers, the surface decides whether to use it.
#[derive(Debug, Clone)]
pub struct UIStyle {
    /// The application background.
    ///
    /// Every surface without its own palette entry draws on this; the
    /// foreground must hold contrast against it everywhere.
    pub background: Color,

    /// The default foreground.
    ///
    /// Applied wherever no more specific color matches; the body-text color
    /// against [`background`](Self::background).
    pub foreground: Color,

    /// The accent for highlights, selection, and focus emphasis.
    ///
    /// The loudest color in the palette, used sparingly by design.
    pub primary: Color,

    /// The muted accent paired with [`primary`](Self::primary).
    ///
    /// Used for secondary controls and hover states where the primary
    /// accent would shout.
    pub secondary: Color,

    /// The de-emphasized foreground.
    ///
    /// Timestamps, hints, and separators render here rather than in the full
    /// foreground.
    pub dim: Color,

    /// Border color for unfocused surfaces.
    ///
    /// The default frame for panels that are not receiving input.
    pub border_color: Color,

    /// Border color for the focused surface.
    ///
    /// Must read clearly against [`border_color`](Self::border_color) so
    /// focus travels visibly across the screen.
    pub focused_border_color: Color,

    /// Foreground of user messages in the conversation.
    ///
    /// The transcript distinguishes speakers by this color rather than by
    /// full-width panels.
    pub user_message_fg: Color,

    /// Foreground of assistant messages in the conversation.
    ///
    /// Paired with [`user_message_fg`](Self::user_message_fg) so the
    /// transcript's two voices can be told apart. Palettes that keep one
    /// voice color set the two equal; themes that separate them step this
    /// a tone away from the user's color.
    pub assistant_message_fg: Color,

    /// Border color of the input box.
    ///
    /// Frames the editing surface; themes often echo the secondary accent
    /// here.
    pub input_border: Color,

    /// Text color inside the input box.
    ///
    /// Kept at full contrast — the input is always the user's active focus.
    pub input_text: Color,

    /// The status bar's background.
    ///
    /// Paired with [`status_bar_fg`](Self::status_bar_fg); themes pick a
    /// tone that frames the screen against the content background.
    pub status_bar_bg: Color,

    /// The status bar's foreground.
    ///
    /// Must read on [`status_bar_bg`](Self::status_bar_bg) at small sizes,
    /// so the pair stays high-contrast.
    pub status_bar_fg: Color,

    /// Color for success indicators.
    ///
    /// Applied to check-style markers and completion notices in tool
    /// output and the status line.
    pub status_success: Color,

    /// Color for warning indicators.
    ///
    /// Applied to recoverable problems the user should notice without
    /// alarm.
    pub status_warning: Color,

    /// Color for error indicators.
    ///
    /// Applied to failures in tool output and status reporting; the
    /// strongest of the three status colors.
    pub status_error: Color,

    /// The scrollbar thumb color.
    ///
    /// The draggable part; its contrast against the track is the whole
    /// point.
    pub scrollbar_thumb: Color,

    /// The scrollbar track color.
    ///
    /// The rail behind the thumb, usually at or near the background tone.
    pub scrollbar_track: Color,
}

impl Theme {
    /// Look up a theme by name, returning `None` when unknown.
    ///
    /// Lookup keys are the constructor identifiers (`dracula`,
    /// `gruvbox_dark`), matched case-insensitively with whitespace runs
    /// folding to underscores — so the display spelling (`"Gruvbox Dark"`)
    /// resolves the same as the identifier, however it is spaced or cased.
    /// The `"default"` key aliases [`Theme::default`]. Every constructor is
    /// reachable through this table.
    ///
    /// An unknown name is the caller's problem to report: the TUI boot path
    /// is expected to log a warning naming the unknown theme and fall back
    /// to [`Theme::default`], so a config typo degrades visibly but never
    /// fails the launch.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        let key = name
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");
        theme_data::THEME_CONSTRUCTORS
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, constructor)| constructor())
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dracula()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn registry_keys_are_unique() {
        // `by_name` resolves with `find`, so a duplicated key would
        // silently shadow: only the first row would ever resolve. The keys
        // must be pairwise distinct for every row to be reachable.
        let keys: Vec<&str> = theme_data::THEME_CONSTRUCTORS
            .iter()
            .map(|(key, _)| *key)
            .collect();
        let unique: std::collections::HashSet<&str> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len(), "duplicate registry key: {keys:?}");
    }

    #[test]
    fn every_known_name_resolves() {
        // The lookup table is the single registry: every entry must resolve,
        // or `by_name` silently degraded for that key.
        for (key, _) in theme_data::THEME_CONSTRUCTORS {
            let theme = Theme::by_name(key).unwrap_or_else(|| panic!("{key} unresolved"));
            assert!(!theme.name.is_empty(), "{key}: empty display name");
        }
    }

    #[test]
    fn the_v1_theme_set_is_registered() {
        // The v1 first cut: exactly this registry scope; the remaining
        // themes land in a follow-up expansion. The length assert makes the
        // census exact, so a row added or removed without updating this
        // test fails here first.
        assert_eq!(
            theme_data::THEME_CONSTRUCTORS.len(),
            16,
            "the registry scope moved; update this census with it"
        );
        for key in [
            "default",
            "dracula",
            "nord",
            "tokyo_night",
            "gruvbox_dark",
            "gruvbox_light",
            "solarized_dark",
            "solarized_light",
            "catppuccin_latte",
            "catppuccin_frappe",
            "catppuccin_macchiato",
            "catppuccin_mocha",
            "one_dark",
            "monokai",
            "github_dark",
            "github_light",
        ] {
            assert!(
                theme_data::THEME_CONSTRUCTORS
                    .iter()
                    .any(|(k, _)| *k == key),
                "{key} must be in the registry"
            );
        }
    }

    #[test]
    fn by_name_resolves_the_documented_names() {
        for key in [
            "dracula",
            "nord",
            "tokyo_night",
            "gruvbox_dark",
            "catppuccin_mocha",
            "github_dark",
        ] {
            assert!(Theme::by_name(key).is_some(), "{key} must resolve");
        }
    }

    #[test]
    fn by_name_is_trimmed_and_case_insensitive() {
        for spelling in ["Dracula", "DRACULA", "  dracula  "] {
            assert_eq!(
                Theme::by_name(spelling).map(|t| t.name),
                Some("Dracula"),
                "{spelling:?} must resolve to Dracula"
            );
        }
        assert_eq!(
            Theme::by_name("  Gruvbox Dark  ").map(|t| t.name),
            Some("Gruvbox Dark"),
            "multi-word keys normalize too"
        );
        assert_eq!(
            Theme::by_name("GRUVBOX  DARK").map(|t| t.name),
            Some("Gruvbox Dark"),
            "whitespace runs fold like single spaces"
        );
    }

    #[test]
    fn by_name_unknown_returns_none() {
        // Unknown is `None` here; the warn-and-default fallback is the
        // caller's policy (documented on `by_name`).
        assert!(Theme::by_name("nonexistent").is_none());
        assert!(Theme::by_name("").is_none());
        assert!(Theme::by_name("   ").is_none());
    }

    #[test]
    fn default_is_dracula() {
        assert_eq!(Theme::default().name, "Dracula");
        assert_eq!(
            Theme::default().syntax.keyword,
            Theme::dracula().syntax.keyword
        );
    }

    #[test]
    fn the_default_key_aliases_the_default_theme() {
        assert_eq!(Theme::by_name("default").map(|t| t.name), Some("Dracula"));
    }

    #[test]
    fn syntax_colors_match_the_highlighter_names() {
        // The names-versus-array pin: walking HIGHLIGHT_NAMES beside the
        // projected array, every slot must hold the color of the field its
        // capture name maps to — the documented folds included — so a
        // reordered array or a drifted name list fails here instead of
        // miscoloring every theme.
        for (key, _) in theme_data::THEME_CONSTRUCTORS {
            let syntax = Theme::by_name(key).unwrap().syntax;
            let colors = syntax.syntax_colors();
            for (name, color) in HIGHLIGHT_NAMES.iter().zip(colors.iter()) {
                let expected = match *name {
                    "attribute" => syntax.attribute,
                    "constant" => syntax.constant,
                    "function.builtin" | "function" => syntax.function,
                    "keyword" => syntax.keyword,
                    "operator" => syntax.operator,
                    "property" => syntax.property,
                    "punctuation" | "punctuation.bracket" => syntax.punctuation,
                    "punctuation.delimiter" => syntax.delimiter,
                    "string" => syntax.string,
                    "string.special" => syntax.escape,
                    "tag" => syntax.tag,
                    "type" | "type.builtin" => syntax.r#type,
                    "variable.builtin" => syntax.variable_builtin,
                    "variable.parameter" | "variable" => syntax.variable,
                    other => panic!("capture name without a mapped field: {other}"),
                };
                assert_eq!(*color, expected, "{key}: {name} slot drifted");
            }
        }
    }

    #[test]
    fn no_theme_palette_contains_reset_colors() {
        // `Reset` in a palette is a gap, not a color choice: every theme
        // must fully specify its syntax palette.
        for (key, _) in theme_data::THEME_CONSTRUCTORS {
            let theme = Theme::by_name(key).unwrap();
            assert!(
                !theme.syntax.syntax_colors().contains(&Color::Reset),
                "{key}: Reset leaked into the syntax palette"
            );
        }
    }

    #[test]
    fn markdown_element_styles_carry_explicit_colors() {
        // Every element style except the pure-modifier ones (bold, italic)
        // must set a foreground: a bare style would render the element in
        // ambient text color, hiding a palette gap instead of surfacing it.
        for (key, _) in theme_data::THEME_CONSTRUCTORS {
            let markdown = Theme::by_name(key).unwrap().markdown;
            let styled = [
                markdown.header1,
                markdown.header2,
                markdown.header3,
                markdown.header4,
                markdown.header5,
                markdown.header6,
                markdown.code_inline,
                markdown.code_block,
                markdown.link,
                markdown.quote,
                markdown.list_item,
                markdown.horizontal_rule,
            ];
            for style in styled {
                assert!(
                    style.fg.is_some(),
                    "{key}: markdown style without a foreground"
                );
            }
        }
    }

    #[test]
    fn markdown_styles_keep_the_semantic_modifiers() {
        // The markdown contract the renderer relies on: headings are
        // bold, emphasis is italic, links are underlined.

        let markdown = Theme::dracula().markdown;
        assert!(
            markdown.header1.add_modifier.contains(Modifier::BOLD),
            "header1 must stay bold"
        );
        assert!(
            markdown.italic.add_modifier.contains(Modifier::ITALIC),
            "italic must stay italic"
        );
        assert!(
            markdown.link.add_modifier.contains(Modifier::UNDERLINED),
            "link must stay underlined"
        );
    }
}
