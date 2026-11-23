//! The theme system: named color palettes for syntax, markdown, and chrome.
//!
//! A [`Theme`] bundles the three palettes the TUI consumes: tree-sitter
//! syntax colors, markdown element styles, and application-chrome colors.
//! Themes are compiled-in data selected by name through [`Theme::by_name`],
//! which resolves however the name is spelled or cased. A theme is pure
//! data: it carries color only, and components decide how to render it.

use ratatui::style::{Color, Style};

pub(crate) mod theme_data;

/// The tree-sitter capture names, in [`crate::markdown::SyntaxTheme::emit_order_colors`] slot
/// order.
///
/// This list is the contract between the palette and any syntax
/// highlighter: the highlighter's capture-name list must match it slot for
/// slot, and
/// [`emit_order_colors`](crate::markdown::SyntaxTheme::emit_order_colors)
/// projects the palette by exactly this order — a name list that drifts
/// from it miscolors every theme silently, which
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
/// Construct via [`Theme::by_name`] or [`Default::default`]; every field is
/// public, so a custom palette can also be assembled as a plain struct
/// literal. Themes are value types with no behavior beyond
/// [`crate::markdown::SyntaxTheme::emit_order_colors`]; rendering logic lives with the
/// components that draw.
#[derive(Debug, Clone)]
pub struct Theme {
    /// The theme's display name, as rendered in UI and diagnostics.
    ///
    /// Matches the spec's conventional spelling ("Dracula", "Gruvbox
    /// Dark") and is independent of the lowercase lookup key used by
    /// [`Theme::by_name`]. Static because every theme is compiled-in data —
    /// a theme cannot be built with a name its spec does not carry.
    pub name: &'static str,

    /// Colors for the tree-sitter captures highlighted inside code blocks.
    ///
    /// Projected into the highlighter's emit order by
    /// [`crate::markdown::SyntaxTheme::emit_order_colors`]; each field tunes one capture class.
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
/// [`emit_order_colors`](crate::markdown::SyntaxTheme::emit_order_colors) for the projection into the
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

/// Styles for each markdown element the renderer distinguishes.
///
/// Unlike [`UIStyle`], these carry full [`Style`] values — modifiers such as
/// bold and italic are semantic to the element, not chrome decisions.
#[derive(Debug, Clone)]
pub struct MarkdownTheme {
    /// Style for level-1 headings, the strongest emphasis a document carries.
    ///
    /// Shipped palettes give it BOLD plus their heading color — the
    /// first step of a hue ramp where the palette carries one, or
    /// the palette's single heading hue where it repeats at every
    /// depth — so section starts are visible at a glance.
    pub header1: Style,

    /// Style for level-2 headings.
    ///
    /// BOLD with the heading color a step below level 1 without
    /// dropping to body weight; the second step of the ramp where
    /// the palette carries one.
    pub header2: Style,

    /// Style for level-3 headings.
    ///
    /// BOLD with the heading color for this depth; the deepest
    /// level that still reads as a section title at conversational
    /// font sizes.
    pub header3: Style,

    /// Style for level-4 headings.
    ///
    /// BOLD with the heading color for this depth; below this
    /// depth headings are mostly distinguished by color alone.
    pub header4: Style,

    /// Style for level-5 headings.
    ///
    /// BOLD with the heading color for this depth, aimed at
    /// outline-level structure that rarely appears in chat-length
    /// documents.
    pub header5: Style,

    /// Style for level-6 headings, the quietest of the ramp.
    ///
    /// BOLD with the heading color for this depth; present for
    /// completeness of the markdown contract rather than frequent
    /// use.
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
    /// Shipped palettes carry an accent foreground on the regular
    /// background so code reads as a distinct register without
    /// breaking the line.
    pub code_inline: Style,

    /// Style for fenced code blocks.
    ///
    /// The block's base style; token colors layer on top of it from the
    /// syntax palette via [`crate::markdown::SyntaxTheme::emit_order_colors`].
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
    /// The application background — the canvas.
    ///
    /// The session points the terminal's default background at this
    /// (OSC 11, where the terminal supports it), and the conversation
    /// pane renders on that default rather than on painted cells —
    /// window margin and cell grid share one color on one layer, so
    /// the theme reads edge to edge with no seam between them. On a
    /// terminal without that support the canvas is the terminal's own
    /// configured background. The foreground must hold contrast
    /// against it everywhere.
    pub background: Color,

    /// The raised-surface background — the palette's own elevated
    /// step off the canvas.
    ///
    /// The composer and the user-turn prompt boxes draw on this, so a
    /// surface reads as elevated over
    /// [`background`](Self::background) rather than as a hole in it.
    /// Each theme carries its palette's own elevated background step
    /// (a current-line, mantle, or bg1 tone) rather than a derived
    /// tint, which is what gives the chrome per-theme color at rest.
    pub surface: Color,

    /// The default foreground.
    ///
    /// Applied wherever no more specific color matches; the body-text color
    /// against [`background`](Self::background).
    pub foreground: Color,

    /// The accent for highlights, selection, and focus emphasis.
    ///
    /// The loudest color in the palette, used sparingly by design. On
    /// painted chrome the composer's left edge column and the
    /// conversation's user-turn chevron draw here — the input and the
    /// user's own words are the screen's focal surfaces; the paintless
    /// theme's accents are Reset and it draws neither.
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

    /// The input field's accent color.
    ///
    /// The field carries no border glyphs — its shape is a
    /// background tint — so this color draws the composer's state
    /// tag on the status bar (queued submissions, line position);
    /// themes often echo the secondary accent here.
    pub input_border: Color,

    /// Text color inside the input field.
    ///
    /// Kept at full contrast — the input is always the user's active
    /// focus, and it sits on the field's tint rather than the plain
    /// background.
    pub input_text: Color,

    /// The status bar's background.
    ///
    /// Paired with [`status_bar_fg`](Self::status_bar_fg); set to the
    /// theme's background so the bar sits on the terminal-level canvas
    /// and only the raised composer paints at the foot of the screen —
    /// the prompt box and the status row read as distinct panes.
    pub status_bar_bg: Color,

    /// The status bar's foreground.
    ///
    /// Must read on [`status_bar_bg`](Self::status_bar_bg) at small sizes,
    /// so the pair stays high-contrast.
    pub status_bar_fg: Color,

    /// The thin solid border marking the composer.
    ///
    /// `None` wherever the chrome paints — an elevated surface step
    /// already separates the panes. A theme that paints nothing (the
    /// transparent one) underlines the rows above and below the
    /// composer in this color instead: hairline rules the terminal
    /// draws itself, thin and solid everywhere, while the pane stays
    /// on the terminal's own background.
    pub composer_border: Option<Color>,

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
    /// Lookup keys are the registry keys (`dracula`,
    /// `gruvbox_dark`), matched case-insensitively with whitespace runs
    /// folding to underscores — so the display spelling (`"Gruvbox Dark"`)
    /// resolves the same as the identifier, however it is spaced or cased.
    /// The `"dch"` key names the fallback palette. Every spec is
    /// reachable through this table, and retired keys resolve through
    /// [`Theme::key_for_name`]'s aliases.
    ///
    /// An unknown name is the caller's problem to report: callers should
    /// log a warning naming the unknown theme and fall back to
    /// [`Default::default`], so a bad name degrades visibly without failing
    /// the session.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        let key = Self::key_for_name(name)?;
        theme_data::THEME_SPECS
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, spec)| spec.build())
    }

    /// The registry key a name resolves to, when it resolves at all.
    ///
    /// The inverse half of [`Theme::by_name`]'s folding: whatever spelling a
    /// user types — the key, the display name, any casing or spacing —
    /// answers with the canonical registry key, the one spelling the
    /// config file documents and `/themes` lists. Callers that persist
    /// a theme name store this, so one value has one stored spelling
    /// everywhere; `None` is the same miss [`Theme::by_name`] reports.
    #[must_use]
    pub fn key_for_name(name: &str) -> Option<&'static str> {
        let key = name
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");
        if let Some((canonical, _)) = theme_data::THEME_SPECS
            .iter()
            .find(|(known, _)| *known == key)
        {
            return Some(*canonical);
        }
        theme_data::THEME_ALIASES
            .iter()
            .find(|(alias, _)| *alias == key)
            .map(|(_, canonical)| *canonical)
    }

    /// The display names every compiled-in theme answers to, in registry order.
    ///
    /// Reads the same spec table [`Theme::by_name`] resolves through,
    /// so the list is exactly the set of names that resolve — never a separate
    /// copy that could fall out of step as themes are added or removed. The
    /// names are the display spelling (`"Gruvbox Dark"`) rather than the
    /// lowercase lookup key, which is what a user of `/theme` reads and types,
    /// and [`Theme::by_name`] folds case and whitespace so this spelling still
    /// resolves. Intended as a completion source; it reads only the table's
    /// display labels — it builds no palettes — so a caller may reach it on
    /// every keypress.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        theme_data::THEME_SPECS
            .iter()
            .map(|(_, spec)| spec.name)
            .collect()
    }

    /// The canonical command spelling of every compiled-in theme.
    ///
    /// These are the lookup identifiers — `dch`, `opencode`, `nord`,
    /// `gruvbox_dark`, `tokyo_night` — the spellings a well-formed config
    /// file carries under
    /// `theme =` (the loader itself takes any string; these are the words it
    /// resolves) and the exact strings [`Theme::by_name`] matches, so what a
    /// user sees listed by `/themes` or offered by `/theme <Tab>` is
    /// precisely what they then type. Retired spellings (`default`)
    /// resolve through the alias table [`Theme::key_for_name`] walks
    /// but never appear here. Distinct from [`Theme::names`], which
    /// returns the pretty display spelling (`"Gruvbox Dark"`) the palette is
    /// *labelled* by. Because
    /// the keys sit in the compile-time registry beside their palette
    /// specs, this reads only the table's identifiers — it builds no
    /// palettes — so it is cheap enough to
    /// call on every keypress, unlike [`Theme::names`].
    #[must_use]
    pub fn command_names() -> Vec<&'static str> {
        theme_data::THEME_SPECS
            .iter()
            .map(|(key, _)| *key)
            .collect()
    }
}

impl Theme {
    /// Defer the terminal-canvas colors to the terminal's own scheme.
    ///
    /// The adaptation for a session whose background override did not
    /// land — the terminal never answered the OSC 11 query, or the
    /// platform has no query to send. The palette's near-canvas
    /// foreground (and every color equal to it) was chosen against
    /// the theme's own canvas color; on a terminal that kept a light
    /// background, that text is invisible. Every such color becomes
    /// [`Color::Reset`], the terminal's own foreground, which its own
    /// background guarantees readable. Painted pairs survive
    /// untouched — the composer paints its surface and the status bar
    /// paints its canvas, so their text keeps its palette, and the
    /// furniture stays the theme's. Headings get the same test
    /// generalized: a heading anchored to the canvas defers like the
    /// text registers, and a heading whose color falls under the
    /// 4.5:1 AA floor against white — the light canvas the deferral
    /// exists for — defers too, because on that terminal it would be
    /// unreadable. A heading that clears the floor on both canvases
    /// keeps its palette.
    #[must_use]
    pub fn deferred_to_terminal(&self) -> Self {
        fn deferring(color: Color, anchor: Color) -> Color {
            if color == anchor { Color::Reset } else { color }
        }
        fn channel(value: u8) -> f64 {
            let unit = f64::from(value) / 255.0;
            if unit <= 0.04045 {
                unit / 12.92
            } else {
                ((unit + 0.055) / 1.055).powf(2.4)
            }
        }
        fn contrast(a: Color, b: Color) -> f64 {
            fn luminance(color: Color) -> f64 {
                let Color::Rgb(red, green, blue) = color else {
                    return 0.0;
                };
                0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
            }
            let (la, lb) = (luminance(a), luminance(b));
            (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
        }
        fn defer_style(style: Style, anchor: Color) -> Style {
            match style.fg {
                None => style,
                Some(fg) => style.fg(if fg == anchor || !passes_light_floor(fg) {
                    Color::Reset
                } else {
                    fg
                }),
            }
        }
        /// Whether a heading foreground clears the AA floor against
        /// white. Non-`Rgb` colors carry no scorable channels — they
        /// defer rather than pass on an unscored technicality.
        fn passes_light_floor(fg: Color) -> bool {
            match fg {
                Color::Rgb(..) => contrast(fg, Color::Rgb(255, 255, 255)) >= 4.5,
                _ => false,
            }
        }
        let anchor = self.ui.foreground;
        let mut deferred = self.clone();
        deferred.ui.foreground = deferring(deferred.ui.foreground, anchor);
        deferred.ui.user_message_fg = deferring(deferred.ui.user_message_fg, anchor);
        deferred.ui.assistant_message_fg = deferring(deferred.ui.assistant_message_fg, anchor);
        deferred.syntax.punctuation = deferring(deferred.syntax.punctuation, anchor);
        deferred.syntax.delimiter = deferring(deferred.syntax.delimiter, anchor);
        deferred.markdown.header1 = defer_style(deferred.markdown.header1, anchor);
        deferred.markdown.header2 = defer_style(deferred.markdown.header2, anchor);
        deferred.markdown.header3 = defer_style(deferred.markdown.header3, anchor);
        deferred.markdown.header4 = defer_style(deferred.markdown.header4, anchor);
        deferred.markdown.header5 = defer_style(deferred.markdown.header5, anchor);
        deferred.markdown.header6 = defer_style(deferred.markdown.header6, anchor);
        deferred.markdown.code_block = defer_style(deferred.markdown.code_block, anchor);
        deferred
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default()
    }
}

impl From<&Theme> for crate::markdown::MarkdownTheme {
    fn from(value: &Theme) -> Self {
        Self {
            header: [
                value.markdown.header1,
                value.markdown.header2,
                value.markdown.header3,
                value.markdown.header4,
                value.markdown.header5,
                value.markdown.header6,
            ],
            bold: value.markdown.bold,
            italic: value.markdown.italic,
            code_inline: value.markdown.code_inline,
            code_block: value.markdown.code_block.bg.unwrap_or(Color::Reset),
            link: value.markdown.link,
            quote: value.markdown.quote,
            list_item: value.markdown.list_item,
            horizontal_rule: value.markdown.horizontal_rule,
            border: value.ui.border_color,
            dim: value.ui.dim,
        }
    }
}

impl From<&Theme> for crate::markdown::SyntaxTheme {
    fn from(value: &Theme) -> Self {
        Self {
            plain: value.ui.foreground,
            attribute: value.syntax.attribute,
            comment: value.syntax.comment,
            constant: value.syntax.constant,
            constructor: value.syntax.constructor,
            embedded: value.syntax.embedded,
            function: value.syntax.function,
            keyword: value.syntax.keyword,
            number: value.syntax.number,
            operator: value.syntax.operator,
            property: value.syntax.property,
            punctuation: value.syntax.punctuation,
            string: value.syntax.string,
            r#type: value.syntax.r#type,
            variable: value.syntax.variable,
            variable_builtin: value.syntax.variable_builtin,
            tag: value.syntax.tag,
            delimiter: value.syntax.delimiter,
            escape: value.syntax.escape,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn registry_keys_are_unique() {
        // `by_name` resolves with `find`, so a duplicated key would
        // silently shadow: only the first row would ever resolve. The keys
        // must be pairwise distinct for every row to be reachable.
        let keys: Vec<&str> = theme_data::THEME_SPECS
            .iter()
            .map(|(key, _)| *key)
            .collect();
        let unique: std::collections::HashSet<&str> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len(), "duplicate registry key: {keys:?}");
    }

    #[test]
    fn command_names_are_the_settable_lookup_keys() {
        // The command spelling is the registry key (`gruvbox_dark`), which is
        // what `/themes` lists, `/theme <Tab>` offers, and `by_name` accepts —
        // distinct from the pretty display name (`"Gruvbox Dark"`).
        let commands = Theme::command_names();
        assert!(commands.contains(&"opencode"));
        assert!(commands.contains(&"gruvbox_dark"));
        // Not display spellings: no spaces, no capitals.
        assert!(
            commands
                .iter()
                .all(|key| !key.contains(' ') && *key == key.to_lowercase()),
            "command names are lower-case identifiers, got {commands:?}"
        );
        // Every command name resolves back to itself; every display name too.
        for key in &commands {
            assert!(Theme::by_name(key).is_some(), "{key} must resolve");
        }
        assert_eq!(
            commands.len(),
            Theme::names().len(),
            "same set, two spellings"
        );
    }

    #[test]
    fn names_lists_every_registered_theme_in_registry_order() {
        // The completion source for `/theme` must be exactly the set that
        // `by_name` resolves, in the same order, and spelled so it resolves.
        let names = Theme::names();
        assert_eq!(names.len(), theme_data::THEME_SPECS.len());
        assert!(names.iter().all(|name| !name.is_empty()));
        for name in names {
            assert!(
                Theme::by_name(name).is_some(),
                "name {name} must resolve through by_name"
            );
        }
    }

    #[test]
    fn autumn_night_s_text_roles_clear_the_contrast_floor() {
        // The upstream muted gray-green (#626C66) reads 3.66:1 on this
        // theme's canvas — under the 4.5:1 AA floor for small text.
        // The three roles that carry words lift to a lighter tone of
        // the same hue (5.73:1 on the canvas, 5.01:1 on the surface);
        // the decorative cells that share the color upstream stay
        // faithful.
        /// WCAG relative luminance: the sRGB channels through the
        /// spec's transfer function, then the fixed coefficient mix.
        fn luminance(color: Color) -> f64 {
            let channel = |value: u8| {
                let c = f64::from(value) / 255.0;
                if c <= 0.040_45 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            let Color::Rgb(r, g, b) = color else {
                return 0.0;
            };
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        }
        fn ratio(foreground: Color, background: Color) -> f64 {
            let a = luminance(foreground);
            let b = luminance(background);
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        }
        let theme = Theme::by_name("autumn_night").expect("the autumn night theme is registered");
        let canvas = theme.ui.background;
        let surface = theme.ui.surface;
        for (role, color) in [
            ("syntax comments", theme.syntax.comment),
            (
                "level-6 headings",
                theme.markdown.header6.fg.unwrap_or(canvas),
            ),
            ("the dim register", theme.ui.dim),
        ] {
            assert!(
                ratio(color, canvas) >= 4.5,
                "{role} reads {} on the canvas — under the floor",
                ratio(color, canvas)
            );
            assert!(
                ratio(color, surface) >= 4.5,
                "{role} reads {} on the surface — under the floor",
                ratio(color, surface)
            );
        }
        for (role, color) in [
            ("the scrollbar thumb", theme.ui.scrollbar_thumb),
            ("the input border", theme.ui.input_border),
            (
                "the horizontal rule",
                theme.markdown.horizontal_rule.fg.unwrap_or(canvas),
            ),
        ] {
            assert_eq!(
                color,
                Color::Rgb(98, 108, 102),
                "{role} stays the upstream muted gray-green"
            );
        }
    }

    #[test]
    fn the_heading_orange_diverges_between_canvases() {
        // The deferral's heading rule exists because this palette's
        // heading orange clears the AA floor on the house canvas and
        // fails it on white — the light canvas the deferral serves.
        // Pinning both sides keeps the rule from being deleted as
        // unnecessary.
        fn channel(value: u8) -> f64 {
            let unit = f64::from(value) / 255.0;
            if unit <= 0.04045 {
                unit / 12.92
            } else {
                ((unit + 0.055) / 1.055).powf(2.4)
            }
        }
        fn ratio(a: Color, b: Color) -> f64 {
            fn luminance(color: Color) -> f64 {
                let Color::Rgb(red, green, blue) = color else {
                    return 0.0;
                };
                0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
            }
            let (la, lb) = (luminance(a), luminance(b));
            (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
        }
        let heading = Color::Rgb(255, 193, 51);
        let canvas = Theme::default().ui.background;
        assert!(
            ratio(heading, canvas) >= 4.5,
            "the heading orange must read on the installed canvas"
        );
        assert!(
            ratio(heading, Color::Rgb(255, 255, 255)) < 4.5,
            "the heading orange must fail on white — or the deferral rule is dead weight"
        );
    }

    #[test]
    fn an_unsynced_terminal_defers_the_canvas_anchored_colors() {
        // The default palette paints conversation text in its own
        // near-canvas foreground on an unpainted terminal background;
        // when the background override never landed — the terminal
        // kept a light scheme of its own — that text is invisible.
        // The deferral hands every anchored color to the terminal's
        // own foreground while the painted pairs and accents stand.
        let theme = Theme::default();
        let anchor = theme.ui.foreground;
        assert_eq!(anchor, Color::Rgb(244, 242, 209));
        let deferred = theme.deferred_to_terminal();
        assert_eq!(
            deferred.ui.foreground,
            Color::Reset,
            "conversation foreground defers to the terminal's own"
        );
        assert_eq!(deferred.ui.user_message_fg, Color::Reset);
        assert_eq!(deferred.ui.assistant_message_fg, Color::Reset);
        assert_eq!(
            deferred.syntax.punctuation,
            Color::Reset,
            "code punctuation sits on the terminal canvas and defers"
        );
        assert_eq!(
            deferred.syntax.delimiter,
            Color::Reset,
            "code delimiters sit on the terminal canvas and defer"
        );
        // Painted pairs survive untouched — the composer paints its
        // surface and the status bar paints its canvas, so their text
        // keeps its palette and the furniture stays the theme's.
        assert_eq!(
            deferred.ui.input_text, theme.ui.input_text,
            "the composer paints its surface: its pair stands"
        );
        assert_eq!(
            deferred.ui.surface, theme.ui.surface,
            "the composer's painted surface stands"
        );
        assert_eq!(
            deferred.ui.status_bar_fg, theme.ui.status_bar_fg,
            "the status bar paints its canvas: its pair stands"
        );
        assert_eq!(
            deferred.ui.status_bar_bg, theme.ui.status_bar_bg,
            "the status bar's painted canvas stands"
        );
        assert_eq!(
            deferred.ui.scrollbar_track, theme.ui.scrollbar_track,
            "the scrollbar's track stands"
        );
        assert_eq!(
            deferred.syntax.keyword, theme.syntax.keyword,
            "accent colors read on both schemes and stand"
        );
        assert_eq!(
            deferred.ui.background, theme.ui.background,
            "the canvas field is inert without the override; a later landing needs it"
        );
        // The heading orange fails the light-canvas floor (1.62:1
        // on white) while clearing it on the house canvas (12:1) —
        // the divergence that makes the heading rule necessary. On
        // deferral every heading falls to the terminal's own
        // foreground, readable by construction on either canvas, and
        // the bold weight that marks a heading survives the swap.
        let heading_orange = Color::Rgb(255, 193, 51);
        assert_eq!(
            theme.markdown.header1.fg,
            Some(heading_orange),
            "the installed palette keeps the heading orange"
        );
        for level in 1..=6 {
            let deferred_heading = match level {
                1 => deferred.markdown.header1,
                2 => deferred.markdown.header2,
                3 => deferred.markdown.header3,
                4 => deferred.markdown.header4,
                5 => deferred.markdown.header5,
                _ => deferred.markdown.header6,
            };
            assert_eq!(
                deferred_heading.fg,
                Some(Color::Reset),
                "deferred heading level {level} falls to the terminal's own foreground"
            );
            assert!(
                deferred_heading
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD),
                "deferred heading level {level} keeps its bold weight"
            );
            // The code-block base text is off-anchor by construction
            // (the parents' channel midpoint, not the cream) and fails
            // the white floor (1.52:1), so it defers like the headings —
            // restoring master's behavior, where the default pinned it
            // to the foreground and the equality arm answered.
            assert_eq!(
                theme.markdown.code_block.fg,
                Some(Color::Rgb(198, 216, 177)),
                "the installed palette keeps the code-block color"
            );
            assert_eq!(
                deferred.markdown.code_block.fg,
                Some(Color::Reset),
                "the deferred code block falls to the terminal's own foreground"
            );
            // A non-Rgb heading fg carries no scorable channels; the
            // floor test defers it rather than pass it unscored. A
            // custom palette assembling Indexed headings gets the
            // conservative read, not a silent 21:1.
            let mut indexed_heading = theme.clone();
            indexed_heading.markdown.header3 = Style::default()
                .fg(Color::Indexed(4))
                .add_modifier(ratatui::style::Modifier::BOLD);
            let deferred_indexed = indexed_heading.deferred_to_terminal();
            assert_eq!(
                deferred_indexed.markdown.header3.fg,
                Some(Color::Reset),
                "a non-Rgb heading fg defers conservatively"
            );
            // Boundary: a mid gray clears the 4.5:1 floor against white
            // under the WCAG exponent (5.1:1) and keeps its palette —
            // under a squared shortcut it reads 4.0:1 and would wrongly
            // defer. Not the anchor, so the equality rule never fires:
            // this exercises the contrast arm alone.
            let mut gray_heading = theme.clone();
            let gray = Style::default()
                .fg(Color::Rgb(110, 110, 110))
                .add_modifier(ratatui::style::Modifier::BOLD);
            for level in [
                &mut gray_heading.markdown.header1,
                &mut gray_heading.markdown.header2,
                &mut gray_heading.markdown.header3,
                &mut gray_heading.markdown.header4,
                &mut gray_heading.markdown.header5,
                &mut gray_heading.markdown.header6,
            ] {
                *level = gray;
            }
            let deferred_gray = gray_heading.deferred_to_terminal();
            assert_eq!(
                deferred_gray.markdown.header1.fg,
                Some(Color::Rgb(110, 110, 110)),
                "a heading that clears the white-canvas floor keeps its color"
            );
        }
    }

    #[test]
    fn the_deferral_is_idempotent_on_the_transparent_theme() {
        // The transparent theme is the deferred register by
        // construction — every ground and ink already Reset — so a
        // deferral applied to it must be the identity: a double
        // deferral that moved anything would mean the deferred path
        // itself paints a patch somewhere.
        let theme = Theme::by_name("transparent").expect("transparent resolves");
        let deferred = theme.deferred_to_terminal();
        // Idempotence: the deferred palette equals the original for
        // every painted pair, whatever the original carried.
        assert_eq!(deferred.ui.surface, theme.ui.surface);
        assert_eq!(deferred.ui.status_bar_bg, theme.ui.status_bar_bg);
        assert_eq!(deferred.ui.scrollbar_track, theme.ui.scrollbar_track);
        assert_eq!(deferred.ui.input_text, theme.ui.input_text);
        assert_eq!(deferred.ui.status_bar_fg, theme.ui.status_bar_fg);
        assert_eq!(deferred.ui.background, Color::Reset);
        assert_eq!(deferred.ui.foreground, Color::Reset);
    }

    #[test]
    fn every_known_name_resolves() {
        // The lookup table is the single registry: every entry must resolve,
        // or `by_name` silently degraded for that key.
        for (key, _) in theme_data::THEME_SPECS {
            let theme = Theme::by_name(key).unwrap_or_else(|| panic!("{key} unresolved"));
            assert!(!theme.name.is_empty(), "{key}: empty display name");
        }
    }

    #[test]
    fn the_theme_registry_is_exact() {
        // The length assert makes the census exact, so a row added or
        // removed without updating this test fails here first.
        assert_eq!(
            theme_data::THEME_SPECS.len(),
            23,
            "the registry scope moved; update this census with it"
        );
        for key in [
            "dch",
            "opencode",
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
            "ayu_dark",
            "rose_pine",
            "kanagawa_wave",
            "dark_plus",
            "autumn_night",
            "transparent",
        ] {
            assert!(
                theme_data::THEME_SPECS.iter().any(|(k, _)| *k == key),
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
    fn key_for_name_folds_every_spelling_to_the_registry_key() {
        // The persistence half of the lookup: whatever spelling a user
        // types — key, display name, any casing or spacing — the stored
        // value is the registry key, the one spelling `/themes` lists
        // and the config file documents, so one theme has one stored
        // spelling on every surface.
        for typed in [
            "nord",
            "Nord",
            "NORD",
            " nord ",
            "Gruvbox Dark",
            "gruvbox_dark",
        ] {
            let key = Theme::key_for_name(typed).unwrap_or_else(|| panic!("{typed:?} resolves"));
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "the registry key is the lowercase identifier spelling: {key}"
            );
            assert!(
                Theme::by_name(key).is_some_and(|theme| !theme.name.is_empty()),
                "the key resolves back through by_name"
            );
        }
        assert_eq!(
            Theme::key_for_name("not-a-theme"),
            None,
            "a miss is by_name's miss"
        );
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
    fn default_is_the_near_black_palette() {
        // The shipped default paints: an unconfigured session lands on the
        // near-black canvas, not the paintless one, so the trait
        // resolution and the registry's `"dch"` row must both be
        // the palette spec itself.
        assert_eq!(Theme::default().name, "Dch");
        assert_eq!(Theme::default().syntax.keyword, Color::Rgb(255, 178, 0));
        assert_eq!(Theme::default().ui.background, Color::Rgb(12, 12, 12));
        assert_eq!(
            Theme::by_name("dch").map(|theme| theme.syntax.keyword),
            Some(Color::Rgb(255, 178, 0)),
            "the registry row resolves through the same palette"
        );
    }

    #[test]
    fn the_dch_key_resolves_the_fallback_theme() {
        assert_eq!(
            Theme::by_name("dch").map(|t| t.name),
            Some(Theme::default().name)
        );
    }

    #[test]
    fn the_retired_default_key_resolves_the_renamed_palette() {
        // The rename stranded every config naming the old spelling;
        // the alias keeps those resolving to the same palette — the
        // one the near-black pin holds to the old default's exact
        // colors — so an upgrade changes nothing on screen.
        assert_eq!(Theme::key_for_name("default"), Some("opencode"));
        assert_eq!(Theme::by_name("Default").map(|t| t.name), Some("Opencode"));
        // The alias table stays honest: every target is a live key,
        // and no alias shadows one.
        for (alias, target) in theme_data::THEME_ALIASES {
            assert!(
                theme_data::THEME_SPECS.iter().any(|(k, _)| k == target),
                "alias {alias} points at the missing key {target}"
            );
            assert!(
                !theme_data::THEME_SPECS.iter().any(|(k, _)| k == alias),
                "alias {alias} shadows a live key"
            );
        }
    }

    #[test]
    fn emit_order_matches_the_highlighter_names() {
        // The names-versus-array pin: walking HIGHLIGHT_NAMES beside the
        // module projection the renderer indexes, every slot must hold
        // the color of the field its capture name maps to — the
        // documented folds included — so a reordered array or a drifted
        // name list fails here instead of miscoloring every theme. The
        // walk goes through the From bridge, so the bridge is pinned in
        // the same pass.
        for (key, _) in theme_data::THEME_SPECS {
            let syntax: crate::markdown::SyntaxTheme = (&Theme::by_name(key).unwrap()).into();
            let colors = syntax.emit_order_colors();
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

    /// WCAG relative-luminance channel curve, shared by the contrast pins.
    fn channel(v: u8) -> f32 {
        let c = f32::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG relative luminance of an RGB color; panics on non-RGB, which
    /// every canvas-repainting theme's tested colors are.
    fn luminance(color: Color) -> f32 {
        let (r, g, b) = match color {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("theme contrast colors must be RGB, got {other:?}"),
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    /// Whether a theme defers its canvas to the terminal.
    ///
    /// The transparent theme paints no canvas of its own — `Reset`
    /// backgrounds everywhere — so the pins that reason about paint
    /// (elevation off the canvas, contrast against it, palette
    /// polarity) have nothing to reason about and skip it.
    fn defers_to_terminal(ui: &UIStyle) -> bool {
        ui.background == Color::Reset
    }

    #[test]
    fn scrollbar_thumb_contrasts_with_track() {
        // A thumb painted the track's own color is invisible; the pair must
        // differ in every theme or scrolling stops reading as motion.
        for (key, _) in theme_data::THEME_SPECS {
            let ui = Theme::by_name(key).unwrap().ui;
            assert_ne!(
                ui.scrollbar_thumb, ui.scrollbar_track,
                "{key}: scrollbar thumb matches its track"
            );
        }
    }

    #[test]
    fn every_theme_s_status_bar_sits_on_the_canvas_background() {
        // The bar is canvas, not chrome: it carries the theme's
        // background and steps off the elevated surface, so the
        // composer stays the single raised block at the foot of the
        // screen and the status row reads as a pane of its own below
        // the prompt box.
        for (key, _) in theme_data::THEME_SPECS {
            let ui = Theme::by_name(key).unwrap().ui;
            if defers_to_terminal(&ui) {
                continue;
            }
            assert_eq!(
                ui.status_bar_bg, ui.background,
                "{key}: the bar sits on the theme's canvas background"
            );
            assert_ne!(
                ui.status_bar_bg, ui.surface,
                "{key}: the bar must step off the raised surface"
            );
        }
    }

    #[test]
    fn every_theme_s_status_text_reads_on_its_bar() {
        // WCAG relative luminance: model and token counts must stay
        // legible on the canvas-colored status bar.
        for (key, _) in theme_data::THEME_SPECS {
            let ui = Theme::by_name(key).unwrap().ui;
            if defers_to_terminal(&ui) {
                continue;
            }
            let (lf, lb) = (luminance(ui.status_bar_fg), luminance(ui.status_bar_bg));
            let (lighter, darker) = if lf > lb { (lf, lb) } else { (lb, lf) };
            let ratio = (lighter + 0.05) / (darker + 0.05);
            assert!(
                ratio >= 3.0,
                "{key}: status-text contrast {ratio:.2} is below 3:1"
            );
        }
    }

    #[test]
    fn every_theme_keeps_its_surfaces_and_scrollbar_visible_on_the_canvas() {
        // The session points the terminal's default background at
        // `background`, so the colors that must read against it — the
        // composer's `surface`, the scrollbar's thumb and track —
        // cannot equal it: a match would render the element invisible
        // on the canvas rather than elevated or visible.
        for (key, _) in theme_data::THEME_SPECS {
            let ui = Theme::by_name(key).unwrap().ui;
            if !defers_to_terminal(&ui) {
                assert_ne!(
                    ui.surface, ui.background,
                    "{key}: the composer surface must step off the canvas"
                );
            }
            assert_ne!(
                ui.scrollbar_thumb, ui.background,
                "{key}: the scrollbar thumb must be visible on the canvas"
            );
            assert_ne!(
                ui.scrollbar_track, ui.background,
                "{key}: the scrollbar track must be visible on the canvas"
            );
        }
    }

    #[test]
    fn every_repainting_theme_keeps_its_standing_text_visible_on_the_canvas() {
        // The standing foreground is the register the live lane's
        // running titles and the todo plan's standing rows paint with —
        // a theme that repaints the canvas owns both ends of the pair,
        // so the two must read against each other. github_light on a
        // dark-scheme terminal shipped invisible titles exactly because
        // a register leaned on the terminal's default foreground
        // instead of the theme's own; this pins the theme-side half of
        // that contract for every canvas-repainting theme.
        for (key, _) in theme_data::THEME_SPECS {
            let ui = Theme::by_name(key).unwrap().ui;
            if defers_to_terminal(&ui) {
                continue;
            }
            let (lf, lb) = (luminance(ui.foreground), luminance(ui.background));
            let (lighter, darker) = if lf > lb { (lf, lb) } else { (lb, lf) };
            let ratio = (lighter + 0.05) / (darker + 0.05);
            assert!(
                ratio >= 3.0,
                "{key}: standing-text contrast {ratio:.2} is below 3:1"
            );
        }
    }

    #[test]
    fn the_transparent_theme_defers_every_paint_to_the_terminal() {
        // The contract the user chose: no colors of the app's own —
        // every full-strength foreground is the terminal's, the
        // canvas, the composer, and the status bar paint nothing, a
        // thin border in the terminal's dim tone marks the composer,
        // the dim register is a fixed neutral gray — the scrollbar's
        // own mid tone, mid enough to read on either pole, because
        // the indexed bright-black is scheme-defined and can render
        // indistinguishable from the foreground — so de-emphasized
        // text (thinking bodies, timestamps, system rows) greys out
        // on every terminal, the semantic trio defers through the
        // terminal's indexed colors, and the scrollbar's neutral
        // grays are the only other paint of the theme's own.
        let theme = Theme::by_name("transparent").unwrap();
        let ui = theme.ui;
        assert_eq!(ui.background, Color::Reset);
        assert_eq!(ui.surface, Color::Reset);
        assert_eq!(
            ui.composer_border,
            Some(Color::Indexed(8)),
            "a thin border in the terminal's dim tone marks the composer"
        );
        assert_eq!(
            ui.dim,
            Color::Rgb(142, 142, 142),
            "de-emphasized text carries the theme's fixed neutral gray, the \
             scrollbar's own mid tone — deterministic where the indexed \
             bright-black can render as the foreground"
        );
        assert_eq!(ui.status_bar_bg, Color::Reset);
        assert_eq!(ui.foreground, Color::Reset);
        assert_eq!(ui.user_message_fg, Color::Reset);
        assert_eq!(ui.assistant_message_fg, Color::Reset);
        assert_eq!(
            theme.markdown.code_inline.bg, None,
            "inline code paints no background of its own"
        );
        assert_eq!(ui.status_success, Color::Indexed(2));
        assert_eq!(ui.status_warning, Color::Indexed(3));
        assert_eq!(ui.status_error, Color::Indexed(1));
        assert_ne!(ui.scrollbar_thumb, ui.scrollbar_track);
    }

    #[test]
    fn no_theme_palette_contains_reset_colors() {
        // `Reset` in a palette is a gap, not a color choice — except
        // for the transparent theme, whose entire contract is deferral:
        // every theme must fully specify every field of its syntax and
        // chrome palettes.
        for (key, _) in theme_data::THEME_SPECS {
            let theme = Theme::by_name(key).unwrap();
            if defers_to_terminal(&theme.ui) {
                continue;
            }
            let syntax = theme.syntax;
            let syntax_fields = [
                syntax.attribute,
                syntax.comment,
                syntax.constant,
                syntax.constructor,
                syntax.embedded,
                syntax.function,
                syntax.keyword,
                syntax.number,
                syntax.operator,
                syntax.property,
                syntax.punctuation,
                syntax.string,
                syntax.r#type,
                syntax.variable,
                syntax.variable_builtin,
                syntax.tag,
                syntax.delimiter,
                syntax.escape,
            ];
            assert!(
                !syntax_fields.contains(&Color::Reset),
                "{key}: Reset leaked into the syntax palette"
            );
            let ui = theme.ui;
            let ui_fields = [
                ui.background,
                ui.surface,
                ui.foreground,
                ui.primary,
                ui.secondary,
                ui.dim,
                ui.border_color,
                ui.focused_border_color,
                ui.user_message_fg,
                ui.assistant_message_fg,
                ui.input_border,
                ui.input_text,
                ui.status_bar_bg,
                ui.status_bar_fg,
                ui.status_success,
                ui.status_warning,
                ui.status_error,
                ui.scrollbar_thumb,
                ui.scrollbar_track,
            ];
            assert!(
                !ui_fields.contains(&Color::Reset),
                "{key}: Reset leaked into the chrome palette"
            );
        }
    }

    #[test]
    fn markdown_element_styles_carry_explicit_colors() {
        // Every element style except the pure-modifier ones (bold, italic)
        // must set a foreground: a bare style would render the element in
        // ambient text color, hiding a palette gap instead of surfacing it.
        for (key, _) in theme_data::THEME_SPECS {
            let theme = Theme::by_name(key).unwrap();
            let markdown = theme.markdown;
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
                if defers_to_terminal(&theme.ui) {
                    continue;
                }
                assert!(
                    style.fg.is_some_and(|fg| fg != Color::Reset),
                    "{key}: markdown style without a real foreground"
                );
            }
        }
    }

    #[test]
    fn markdown_styles_keep_the_semantic_modifiers() {
        // The markdown contract the renderer relies on, in every shipped
        // palette: headings are bold, emphasis is italic, links are
        // underlined.
        for (key, _) in theme_data::THEME_SPECS {
            let markdown = Theme::by_name(key).unwrap().markdown;
            let headings = [
                markdown.header1,
                markdown.header2,
                markdown.header3,
                markdown.header4,
                markdown.header5,
                markdown.header6,
            ];
            for heading in headings {
                assert!(
                    heading.add_modifier.contains(Modifier::BOLD),
                    "{key}: a heading lost its bold"
                );
            }
            assert!(
                markdown.italic.add_modifier.contains(Modifier::ITALIC),
                "{key}: italic lost its italics"
            );
            assert!(
                markdown.link.add_modifier.contains(Modifier::UNDERLINED),
                "{key}: link lost its underline"
            );
        }
    }

    #[test]
    fn palette_fingerprints_are_pinned() {
        // The table rewrite's safety net: one fingerprint over each
        // theme's complete palette — every field of all three
        // sections; the pure-modifier styles carry no palette color
        // and are excluded — so a single
        // transposed or mistyped literal anywhere in theme_data
        // fails here instead of silently recoloring a theme.
        fn fingerprint(theme: &Theme) -> u64 {
            /// FNV-1a over the palette's canonical bytes: one tag per
            /// field in a fixed order, then the color's variant and
            /// raw channels.
            ///
            /// Deliberately not `Debug` and not `DefaultHasher`: both
            /// move under dependency and toolchain bumps — ratatui's
            /// formats and std's hasher internals alike — which would
            /// break every pin without a palette change. This encoding
            /// depends only on the channel values the palette carries.
            fn mix(bytes: &[u8]) -> u64 {
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                for byte in bytes {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
                hash
            }
            fn push(tag: u8, color: Option<Color>, out: &mut Vec<u8>) {
                out.push(tag);
                match color {
                    None => out.push(0x00),
                    Some(Color::Reset) => out.push(0x01),
                    Some(Color::Indexed(index)) => {
                        out.push(0x02);
                        out.push(index);
                    }
                    Some(Color::Rgb(red, green, blue)) => {
                        out.push(0x03);
                        out.extend([red, green, blue]);
                    }
                    Some(other) => panic!("unencodable color in a pinned palette: {other:?}"),
                }
            }
            let mut bytes = Vec::new();
            let syntax = [
                theme.syntax.attribute,
                theme.syntax.comment,
                theme.syntax.constant,
                theme.syntax.constructor,
                theme.syntax.embedded,
                theme.syntax.function,
                theme.syntax.keyword,
                theme.syntax.number,
                theme.syntax.operator,
                theme.syntax.property,
                theme.syntax.punctuation,
                theme.syntax.string,
                theme.syntax.r#type,
                theme.syntax.variable,
                theme.syntax.variable_builtin,
                theme.syntax.tag,
                theme.syntax.delimiter,
                theme.syntax.escape,
            ];
            for (tag, color) in syntax.iter().enumerate() {
                push(u8::try_from(tag).unwrap(), Some(*color), &mut bytes);
            }
            let markdown = [
                theme.markdown.header1.fg,
                theme.markdown.header2.fg,
                theme.markdown.header3.fg,
                theme.markdown.header4.fg,
                theme.markdown.header5.fg,
                theme.markdown.header6.fg,
                theme.markdown.code_inline.fg,
                theme.markdown.code_block.fg,
                theme.markdown.link.fg,
                theme.markdown.quote.fg,
                theme.markdown.list_item.fg,
                theme.markdown.horizontal_rule.fg,
            ];
            for (tag, color) in markdown.iter().enumerate() {
                push(
                    u8::try_from(tag.saturating_add(18)).unwrap(),
                    *color,
                    &mut bytes,
                );
            }
            // Bold and italic carry no palette color — pure modifiers,
            // constant across every theme — so they are not encoded.
            let ui = [
                Some(theme.ui.background),
                Some(theme.ui.surface),
                Some(theme.ui.foreground),
                Some(theme.ui.primary),
                Some(theme.ui.secondary),
                Some(theme.ui.dim),
                Some(theme.ui.border_color),
                Some(theme.ui.focused_border_color),
                Some(theme.ui.user_message_fg),
                Some(theme.ui.assistant_message_fg),
                Some(theme.ui.input_border),
                Some(theme.ui.input_text),
                Some(theme.ui.status_bar_bg),
                Some(theme.ui.status_bar_fg),
                theme.ui.composer_border,
                Some(theme.ui.status_success),
                Some(theme.ui.status_warning),
                Some(theme.ui.status_error),
                Some(theme.ui.scrollbar_thumb),
                Some(theme.ui.scrollbar_track),
            ];
            for (tag, color) in ui.iter().enumerate() {
                push(
                    u8::try_from(tag.saturating_add(31)).unwrap(),
                    *color,
                    &mut bytes,
                );
            }
            mix(&bytes)
        }
        for (key, expected) in [
            ("autumn_night", 0x6b5c_0331_9ad8_be5e_u64),
            ("ayu_dark", 0x1911_6ab3_5ba5_085d_u64),
            ("catppuccin_frappe", 0xed1a_fff3_2be2_b14a_u64),
            ("catppuccin_latte", 0x4ad6_21a7_3a75_599a_u64),
            ("catppuccin_macchiato", 0xd108_9399_a770_34fe_u64),
            ("catppuccin_mocha", 0x55ed_1a71_705b_6589_u64),
            ("dark_plus", 0x52c9_4215_dc2f_0181_u64),
            ("opencode", 0x0a2c_02f0_fc36_358f_u64),
            ("dch", 0xe72d_168c_1009_5b4d_u64),
            ("dracula", 0x638a_e1fc_5827_cdfe_u64),
            ("github_dark", 0x7cd3_3b90_395c_1a0c_u64),
            ("github_light", 0xdb12_c770_9e0f_1969_u64),
            ("gruvbox_dark", 0x09dd_c1df_fb96_9cd6_u64),
            ("gruvbox_light", 0x1794_b68c_612b_6c3e_u64),
            ("kanagawa_wave", 0xbc3b_0601_1fd3_77a5_u64),
            ("monokai", 0x9175_0d3e_5b9f_c07d_u64),
            ("nord", 0xa36f_4a42_fea5_ca2c_u64),
            ("one_dark", 0xbbcf_dd19_f967_a58b_u64),
            ("rose_pine", 0x3f18_447f_69cf_792a_u64),
            ("solarized_dark", 0xbd4b_1daa_eb04_faa0_u64),
            ("solarized_light", 0x4241_de1e_6433_19bc_u64),
            ("tokyo_night", 0xddc4_d2c4_83cf_ce68_u64),
            ("transparent", 0xcf86_d191_b459_f056_u64),
        ] {
            let theme = Theme::by_name(key).unwrap();
            assert_eq!(
                fingerprint(&theme),
                expected,
                "{key}: the palette drifted — a literal in theme_data changed"
            );
        }
    }

    #[test]
    fn host_theme_maps_into_the_module_themes() {
        let theme = Theme::default();
        let markdown: crate::markdown::MarkdownTheme = (&theme).into();
        let syntax: crate::markdown::SyntaxTheme = (&theme).into();

        assert_eq!(markdown.header[0], theme.markdown.header1);
        assert_eq!(markdown.border, theme.ui.border_color);
        assert_eq!(markdown.dim, theme.ui.dim);
        assert_eq!(
            markdown.code_block,
            theme.markdown.code_block.bg.unwrap_or(Color::Reset)
        );

        assert_eq!(
            [
                syntax.plain,
                syntax.attribute,
                syntax.comment,
                syntax.constant,
                syntax.constructor,
                syntax.embedded,
                syntax.function,
                syntax.keyword,
                syntax.number,
                syntax.operator,
                syntax.property,
                syntax.punctuation,
                syntax.string,
                syntax.r#type,
                syntax.variable,
                syntax.variable_builtin,
                syntax.tag,
                syntax.delimiter,
                syntax.escape,
            ],
            [
                theme.ui.foreground,
                theme.syntax.attribute,
                theme.syntax.comment,
                theme.syntax.constant,
                theme.syntax.constructor,
                theme.syntax.embedded,
                theme.syntax.function,
                theme.syntax.keyword,
                theme.syntax.number,
                theme.syntax.operator,
                theme.syntax.property,
                theme.syntax.punctuation,
                theme.syntax.string,
                theme.syntax.r#type,
                theme.syntax.variable,
                theme.syntax.variable_builtin,
                theme.syntax.tag,
                theme.syntax.delimiter,
                theme.syntax.escape,
            ],
            "the From bridge carries every palette field, plain included"
        );
    }
}
