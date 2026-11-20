//! The named themes: one spec per theme, one builder, one
//! registry.
//!
//! Each spec is a complete palette — every syntax capture,
//! markdown element, and chrome surface gets an explicit color,
//! so a theme renders identically regardless of defaults. The
//! field set lives once, in the spec structs: every theme lists
//! it whole, so a missing entry is a compile error rather than a
//! silently defaulted color. Palette entries without a distinct
//! identity follow the sibling the palette assigns them
//! (attributes follow functions, punctuation follows comments, and
//! so on). [`ThemeSpec::build`] applies the shared style shapes —
//! bold headings, underlined links, italic quotes — once for every
//! theme. The lookup table at the bottom is the single registry
//! [`Theme::by_name`] resolves through; a theme missing from it is
//! unreachable from configuration.

use ratatui::style::{Color, Modifier, Style};

use super::{MarkdownTheme, SyntaxTheme, Theme, UIStyle};

/// A bold style for a heading: `color` at bold weight.
///
/// Headings own their whole block line, so weight alone carries the
/// level — no underline or background, which would compete with the
/// prose the heading introduces.
fn heading(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// An underlined style for a link.
///
/// The underline keeps a link recognizable on a monochrome terminal,
/// where the color shift alone would vanish into the surrounding
/// prose.
fn link(color: Color) -> Style {
    Style::default()
        .fg(color)
        .add_modifier(Modifier::UNDERLINED)
}

/// An italic style for quoted material.
///
/// The slant marks the span as someone else's words without dimming
/// it, so a quotation stays fully readable against the agent's own
/// prose.
fn quote(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::ITALIC)
}

/// Inline code: an accent foreground on the regular background.
///
/// The span carries no tint of its own — the prompt box owns the
/// surface color, and agent prose holds its emphasis in color alone.
fn inline_code(fg: Color) -> Style {
    Style::default().fg(fg)
}

/// A plain foreground for an element without modifiers.
///
/// Color only: any modifier here would leak into every element built
/// on the style, so decoration stays each caller's own choice.
fn plain(color: Color) -> Style {
    Style::default().fg(color)
}

/// One theme's syntax palette: a color per capture field.
///
/// Field-for-field the authored face of [`SyntaxTheme`]; the builder
/// copies them straight across. Each field names the tree-sitter
/// capture class it colors inside fenced code blocks, in terminal
/// cells.
struct SyntaxColors {
    /// The color for attribute captures.
    ///
    /// Annotations such as Rust's `#[derive]` or Python's `@decorator`;
    /// palettes usually echo the function color, since attributes
    /// decorate items.
    attribute: Color,

    /// The color for comment captures.
    ///
    /// Comments render at every nesting depth, so this sets the muted
    /// register of annotated code; most palettes push it toward the
    /// dim end.
    comment: Color,

    /// The color for constant captures.
    ///
    /// `const` items, statics, and units the highlighter classifies as
    /// constants; often paired with the number color so literal-like
    /// values read as one family.
    constant: Color,

    /// The color for constructor captures.
    ///
    /// Tuple-struct and unit-variant constructors invoked without a
    /// path; usually tuned near the function color, since constructors
    /// read as call sites.
    constructor: Color,

    /// The color for embedded-language regions.
    ///
    /// HTML injections, markdown code fences, template blocks; a
    /// renderer can tint whole injected spans with it rather than
    /// re-highlighting token by token.
    embedded: Color,

    /// The color for function names.
    ///
    /// Covers both `function` and `function.builtin`; the dominant
    /// accent in most code blocks, so themes keep it mid-saturation to
    /// stay calm at high frequency.
    function: Color,

    /// The color for keyword captures.
    ///
    /// Control flow, declaration modifiers, primitive markers; with the
    /// string color it dominates the code-block palette as the
    /// highest-frequency token class.
    keyword: Color,

    /// The color for numeric literals.
    ///
    /// The `number` capture; the highlighter itself folds numeric
    /// constants into the constant capture, so this field colors
    /// literals the parser tags directly.
    number: Color,

    /// The color for operator captures.
    ///
    /// Arithmetic, logical, and assignment operators; sits between
    /// keywords and punctuation in visual weight, so themes pick a
    /// bridging tone.
    operator: Color,

    /// The color for property and field accesses.
    ///
    /// The `property` capture; usually near the variable color so
    /// `obj.field` chains read as one unit.
    property: Color,

    /// The color for punctuation and bracket captures.
    ///
    /// Both `punctuation` and `punctuation.bracket`; carries the
    /// structural rhythm of code, so themes keep it noticeably dimmer
    /// than the tokens it frames.
    punctuation: Color,

    /// The color for string literals.
    ///
    /// The `string` capture; chosen as a pair with the keyword color,
    /// which together dominate the code-block palette.
    string: Color,

    /// The color for type captures.
    ///
    /// Both `type` and `type.builtin`, applied to declarations and
    /// annotations alike, from struct names to generic parameters.
    r#type: Color,

    /// The color for variable captures.
    ///
    /// Including `variable.parameter`; the most frequent non-keyword
    /// token class, kept close to the default foreground in most
    /// palettes.
    variable: Color,

    /// The color for builtin variables.
    ///
    /// `self`, `super`, and language-level globals — the
    /// `variable.builtin` capture; usually a step apart from plain
    /// variables so the special ones read as language-provided.
    variable_builtin: Color,

    /// The color for markup tags.
    ///
    /// HTML and JSX element names — the `tag` capture; often echoes
    /// the keyword color, tags playing the structural role in markup
    /// that keywords play in code.
    tag: Color,

    /// The color for delimiter captures.
    ///
    /// Commas, semicolons, and other separators — the
    /// `punctuation.delimiter` capture; dimmer than brackets so
    /// nesting structure reads before the separators marking it.
    delimiter: Color,

    /// The color for escape sequences.
    ///
    /// The `string.special` capture — `\n`, `\t`, and friends inside
    /// string literals; typically an alerting accent so escapes stand
    /// out of the literal.
    escape: Color,
}

/// One theme's markdown palette: colors only.
///
/// The style shapes — which elements carry weight, underline, or
/// slant — are shared by every theme, so [`ThemeSpec::build`] applies
/// them and a spec carries nothing but color.
struct MarkdownColors {
    /// The heading colors, levels one through six.
    ///
    /// Index zero is level one; the builder applies bold weight to
    /// whichever color each level carries, so the array holds the
    /// hue ramp alone.
    header: [Color; 6],

    /// The inline-code color.
    ///
    /// Code spans inside running prose; the builder applies it as a
    /// plain foreground, letting the surrounding surface provide the
    /// contrast.
    code_inline: Color,

    /// The fenced-code-block text color.
    ///
    /// The block's base text color; token colors from the syntax
    /// palette layer on top of it when highlighting runs.
    code_block: Color,

    /// The link color.
    ///
    /// The builder adds the underline so links stay recognizable even
    /// where the color shift is small.
    link: Color,

    /// The block-quote color.
    ///
    /// The builder applies italics, marking quoted material as
    /// someone else's words without dimming it.
    quote: Color,

    /// The list-marker color.
    ///
    /// Bullets and numbers; the item's own content inherits the
    /// surrounding context.
    list_item: Color,

    /// The horizontal-rule color.
    ///
    /// The divider rows between blocks; usually the theme's quietest
    /// register.
    horizontal_rule: Color,
}

/// One theme's chrome palette: the application surfaces.
///
/// Field-for-field the authored face of [`UIStyle`]; the builder
/// copies them straight across. Every entry is a terminal cell color
/// for one surface of the interface — canvases, borders, message
/// text, status indicators, and the scrollbar.
struct UiColors {
    /// The canvas behind the whole interface.
    ///
    /// The terminal is cleared to this color on launch; every other
    /// surface paints over it.
    background: Color,

    /// The raised surface color.
    ///
    /// Panels and blocks that sit one step above the canvas — the
    /// composer's box and floating sheets.
    surface: Color,

    /// The default text color.
    ///
    /// Conversation prose, unstyled chrome, and everything that does
    /// not name a more specific register.
    foreground: Color,

    /// The primary accent.
    ///
    /// Caret, focused edges, in-progress markers: the one color that
    /// says "here".
    primary: Color,

    /// The secondary accent.
    ///
    /// Quiet emphasis — borders and ornaments that support the
    /// primary rather than competing with it.
    secondary: Color,

    /// The de-emphasis register.
    ///
    /// Thinking bodies, timestamps, system rows: text that must be
    /// readable but recede.
    dim: Color,

    /// The resting border color.
    ///
    /// Boxes and rules that frame content without claiming attention.
    border_color: Color,

    /// The focused border color.
    ///
    /// The edge of whatever currently owns the keyboard or the eye;
    /// usually the primary accent.
    focused_border_color: Color,

    /// The user's own message text.
    ///
    /// The words the operator typed, set apart from the agent's
    /// voice.
    user_message_fg: Color,

    /// The agent's message text.
    ///
    /// Replies, reasoning, and narration from the model.
    assistant_message_fg: Color,

    /// The composer border at rest.
    ///
    /// The frame around the input while nothing owns it specially.
    input_border: Color,

    /// The composer's text.
    ///
    /// The draft being edited, caret included.
    input_text: Color,

    /// The status bar's background.
    ///
    /// The bottom row's canvas; often the theme's base background
    /// repeated.
    status_bar_bg: Color,

    /// The status bar's text.
    ///
    /// Model name, working directory, and the row's other facts.
    status_bar_fg: Color,

    /// The composer border override, if the terminal names one.
    ///
    /// `None` paints the theme's own input border; `Some` defers to a
    /// terminal-scheme index — the one field the transparent theme
    /// carries a value for.
    composer_border: Option<Color>,

    /// The success indicator.
    ///
    /// Completed runs, passed checks, healthy states.
    status_success: Color,

    /// The warning indicator.
    ///
    /// Unfinished work, retries, degraded-but-alive states.
    status_warning: Color,

    /// The error indicator.
    ///
    /// Failed runs, refused calls, damage notices.
    status_error: Color,

    /// The scrollbar thumb.
    ///
    /// The moving handle that names the viewport's place in the
    /// document.
    scrollbar_thumb: Color,

    /// The scrollbar track.
    ///
    /// The channel the thumb travels in.
    scrollbar_track: Color,
}

/// One complete theme as authored data.
///
/// A theme is exactly this table: its display name and the three
/// color groups the builder assembles into a [`Theme`]. Nothing else
/// varies between themes — the style shapes live in the builder, so
/// adding a theme means adding one spec and one registry row.
pub(crate) struct ThemeSpec {
    /// The theme's display name.
    ///
    /// The pretty spelling (`"Gruvbox Dark"`) a user reads in
    /// listings; the registry key is the spelling they type.
    pub(crate) name: &'static str,

    /// The code-block capture palette.
    ///
    /// One color per tree-sitter capture class; see
    /// [`SyntaxColors`] for what each class covers.
    syntax: SyntaxColors,

    /// The markdown element palette.
    ///
    /// Colors only — the builder applies the shared style shapes; see
    /// [`MarkdownColors`].
    markdown: MarkdownColors,

    /// The chrome palette.
    ///
    /// One color per interface surface; see [`UiColors`].
    ui: UiColors,
}

impl ThemeSpec {
    /// Build the theme this spec describes.
    ///
    /// The one place the shared style shapes are applied: headings at
    /// bold weight, links underlined, quotes italic, code inline and
    /// block as plain foregrounds, and bold/italic as pure modifiers
    /// — decisions shared by every theme, applied once here.
    pub(crate) fn build(&self) -> Theme {
        let SyntaxColors {
            attribute,
            comment,
            constant,
            constructor,
            embedded,
            function,
            keyword,
            number,
            operator,
            property,
            punctuation,
            string,
            r#type,
            variable,
            variable_builtin,
            tag,
            delimiter,
            escape,
        } = self.syntax;
        let MarkdownColors {
            header,
            code_inline,
            code_block,
            link: link_color,
            quote: quote_color,
            list_item,
            horizontal_rule,
        } = self.markdown;
        let [header1, header2, header3, header4, header5, header6] = header;
        let UiColors {
            background,
            surface,
            foreground,
            primary,
            secondary,
            dim,
            border_color,
            focused_border_color,
            user_message_fg,
            assistant_message_fg,
            input_border,
            input_text,
            status_bar_bg,
            status_bar_fg,
            composer_border,
            status_success,
            status_warning,
            status_error,
            scrollbar_thumb,
            scrollbar_track,
        } = self.ui;
        Theme {
            name: self.name,
            syntax: SyntaxTheme {
                attribute,
                comment,
                constant,
                constructor,
                embedded,
                function,
                keyword,
                number,
                operator,
                property,
                punctuation,
                string,
                r#type,
                variable,
                variable_builtin,
                tag,
                delimiter,
                escape,
            },
            markdown: MarkdownTheme {
                header1: heading(header1),
                header2: heading(header2),
                header3: heading(header3),
                header4: heading(header4),
                header5: heading(header5),
                header6: heading(header6),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(code_inline),
                code_block: plain(code_block),
                link: link(link_color),
                quote: quote(quote_color),
                list_item: plain(list_item),
                horizontal_rule: plain(horizontal_rule),
            },
            ui: UIStyle {
                background,
                surface,
                foreground,
                primary,
                secondary,
                dim,
                border_color,
                focused_border_color,
                user_message_fg,
                assistant_message_fg,
                input_border,
                input_text,
                status_bar_bg,
                status_bar_fg,
                composer_border,
                status_success,
                status_warning,
                status_error,
                scrollbar_thumb,
                scrollbar_track,
            },
        }
    }
}

impl Theme {
    /// The fallback palette.
    ///
    /// The inherent method the `Default` trait impl and the registry's
    /// first row resolve through; the palette itself is the [`DCH`]
    /// spec like any other theme.
    pub(crate) fn default() -> Self {
        DCH.build()
    }
}

/// Opencode's dark palette — the app's original default, retired
/// by the house palette `dch` but still reachable under its own
/// name.
///
/// A neutral near-black canvas a shade above true black, stepped
/// through an achromatic gray ramp, with a warm peach accent and
/// cyan and green for the registers between — a quiet canvas that
/// stays close to achromatic at rest.
///
/// Derived from opencode's default dark palette
/// (`packages/tui/src/theme/assets/opencode.json`), MIT-licensed,
/// Copyright (c) 2025 opencode. The display name is `"Opencode"`:
/// the palette carries its provenance as its name.
const OPENCODE: ThemeSpec = ThemeSpec {
    name: "Opencode",
    syntax: SyntaxColors {
        attribute: Color::Rgb(236, 188, 132),
        comment: Color::Rgb(123, 123, 123),
        constant: Color::Rgb(248, 159, 71),
        constructor: Color::Rgb(248, 185, 127),
        embedded: Color::Rgb(136, 210, 146),
        function: Color::Rgb(248, 185, 127),
        keyword: Color::Rgb(149, 129, 214),
        number: Color::Rgb(248, 159, 71),
        operator: Color::Rgb(93, 178, 203),
        property: Color::Rgb(93, 178, 203),
        punctuation: Color::Rgb(242, 242, 242),
        string: Color::Rgb(136, 210, 146),
        r#type: Color::Rgb(236, 188, 132),
        variable: Color::Rgb(220, 117, 111),
        variable_builtin: Color::Rgb(220, 117, 111),
        tag: Color::Rgb(149, 129, 214),
        delimiter: Color::Rgb(242, 242, 242),
        escape: Color::Rgb(252, 199, 164),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(248, 185, 127),
            Color::Rgb(149, 129, 214),
            Color::Rgb(136, 210, 146),
            Color::Rgb(93, 178, 203),
            Color::Rgb(236, 188, 132),
            Color::Rgb(123, 123, 123),
        ],
        code_inline: Color::Rgb(136, 210, 146),
        code_block: Color::Rgb(242, 242, 242),
        link: Color::Rgb(248, 185, 127),
        quote: Color::Rgb(236, 188, 132),
        list_item: Color::Rgb(248, 185, 127),
        horizontal_rule: Color::Rgb(123, 123, 123),
    },
    ui: UiColors {
        background: Color::Rgb(14, 14, 14),
        surface: Color::Rgb(32, 32, 32),
        foreground: Color::Rgb(242, 242, 242),
        primary: Color::Rgb(248, 185, 127),
        secondary: Color::Rgb(123, 123, 123),
        dim: Color::Rgb(123, 123, 123),
        border_color: Color::Rgb(77, 77, 77),
        focused_border_color: Color::Rgb(248, 185, 127),
        user_message_fg: Color::Rgb(242, 242, 242),
        assistant_message_fg: Color::Rgb(242, 242, 242),
        input_border: Color::Rgb(123, 123, 123),
        input_text: Color::Rgb(242, 242, 242),
        status_bar_bg: Color::Rgb(14, 14, 14),
        status_bar_fg: Color::Rgb(242, 242, 242),
        composer_border: None,
        status_success: Color::Rgb(136, 210, 146),
        status_warning: Color::Rgb(248, 159, 71),
        status_error: Color::Rgb(220, 117, 111),
        scrollbar_thumb: Color::Rgb(123, 123, 123),
        scrollbar_track: Color::Rgb(57, 57, 57),
    },
};

/// The Dracula theme.
///
/// Dark violet surfaces with pink, purple, and cyan accents at high
/// contrast — readable at small sizes.
const DRACULA: ThemeSpec = ThemeSpec {
    name: "Dracula",
    syntax: SyntaxColors {
        attribute: Color::Rgb(86, 182, 194),
        comment: Color::Rgb(98, 114, 164),
        constant: Color::Rgb(189, 147, 249),
        constructor: Color::Rgb(86, 182, 194),
        embedded: Color::Rgb(241, 250, 140),
        function: Color::Rgb(86, 182, 194),
        keyword: Color::Rgb(255, 121, 198),
        number: Color::Rgb(189, 147, 249),
        operator: Color::Rgb(255, 121, 198),
        property: Color::Rgb(139, 233, 253),
        punctuation: Color::Rgb(98, 114, 164),
        string: Color::Rgb(241, 250, 140),
        r#type: Color::Rgb(255, 184, 108),
        variable: Color::Rgb(139, 233, 253),
        variable_builtin: Color::Rgb(139, 233, 253),
        tag: Color::Rgb(255, 121, 198),
        delimiter: Color::Rgb(255, 121, 198),
        escape: Color::Rgb(255, 85, 85),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(86, 182, 194),
            Color::Rgb(189, 147, 249),
            Color::Rgb(255, 184, 108),
            Color::Rgb(241, 250, 140),
            Color::Rgb(139, 233, 253),
            Color::Rgb(110, 210, 220),
        ],
        code_inline: Color::Rgb(255, 184, 108),
        code_block: Color::Rgb(248, 248, 242),
        link: Color::Rgb(98, 114, 164),
        quote: Color::Rgb(98, 114, 164),
        list_item: Color::Rgb(248, 248, 242),
        horizontal_rule: Color::Rgb(98, 114, 164),
    },
    ui: UiColors {
        background: Color::Rgb(40, 42, 54),
        surface: Color::Rgb(68, 71, 90),
        foreground: Color::Rgb(248, 248, 242),
        primary: Color::Rgb(189, 147, 249),
        secondary: Color::Rgb(98, 114, 164),
        dim: Color::Rgb(98, 114, 164),
        border_color: Color::Rgb(68, 71, 90),
        focused_border_color: Color::Rgb(189, 147, 249),
        user_message_fg: Color::Rgb(248, 248, 242),
        assistant_message_fg: Color::Rgb(248, 248, 242),
        input_border: Color::Rgb(98, 114, 164),
        input_text: Color::Rgb(248, 248, 242),
        status_bar_bg: Color::Rgb(40, 42, 54),
        status_bar_fg: Color::Rgb(248, 248, 242),
        composer_border: None,
        status_success: Color::Rgb(110, 210, 220),
        status_warning: Color::Rgb(241, 250, 140),
        status_error: Color::Rgb(255, 85, 85),
        scrollbar_thumb: Color::Rgb(98, 114, 164),
        scrollbar_track: Color::Rgb(68, 71, 90),
    },
};

/// The Nord theme.
///
/// Muted blue-gray arctic palette with frost-blue accents; deliberately
/// low-saturation, built for long sessions rather than punch.
const NORD: ThemeSpec = ThemeSpec {
    name: "Nord",
    syntax: SyntaxColors {
        attribute: Color::Rgb(163, 190, 140),
        comment: Color::Rgb(94, 129, 172),
        constant: Color::Rgb(129, 161, 193),
        constructor: Color::Rgb(163, 190, 140),
        embedded: Color::Rgb(235, 203, 139),
        function: Color::Rgb(163, 190, 140),
        keyword: Color::Rgb(136, 192, 208),
        number: Color::Rgb(129, 161, 193),
        operator: Color::Rgb(143, 188, 187),
        property: Color::Rgb(216, 222, 233),
        punctuation: Color::Rgb(94, 129, 172),
        string: Color::Rgb(235, 203, 139),
        r#type: Color::Rgb(208, 135, 112),
        variable: Color::Rgb(216, 222, 233),
        variable_builtin: Color::Rgb(216, 222, 233),
        tag: Color::Rgb(136, 192, 208),
        delimiter: Color::Rgb(143, 188, 187),
        escape: Color::Rgb(191, 97, 106),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(136, 192, 208),
            Color::Rgb(143, 188, 187),
            Color::Rgb(129, 161, 193),
            Color::Rgb(94, 129, 172),
            Color::Rgb(163, 190, 140),
            Color::Rgb(208, 135, 112),
        ],
        code_inline: Color::Rgb(235, 203, 139),
        code_block: Color::Rgb(236, 239, 244),
        link: Color::Rgb(163, 190, 140),
        quote: Color::Rgb(94, 129, 172),
        list_item: Color::Rgb(236, 239, 244),
        horizontal_rule: Color::Rgb(94, 129, 172),
    },
    ui: UiColors {
        background: Color::Rgb(46, 52, 64),
        surface: Color::Rgb(59, 66, 82),
        foreground: Color::Rgb(236, 239, 244),
        primary: Color::Rgb(136, 192, 208),
        secondary: Color::Rgb(94, 129, 172),
        dim: Color::Rgb(94, 129, 172),
        border_color: Color::Rgb(59, 66, 82),
        focused_border_color: Color::Rgb(136, 192, 208),
        user_message_fg: Color::Rgb(216, 222, 233),
        assistant_message_fg: Color::Rgb(236, 239, 244),
        input_border: Color::Rgb(94, 129, 172),
        input_text: Color::Rgb(236, 239, 244),
        status_bar_bg: Color::Rgb(46, 52, 64),
        status_bar_fg: Color::Rgb(236, 239, 244),
        composer_border: None,
        status_success: Color::Rgb(163, 190, 140),
        status_warning: Color::Rgb(235, 203, 139),
        status_error: Color::Rgb(191, 97, 106),
        scrollbar_thumb: Color::Rgb(94, 129, 172),
        scrollbar_track: Color::Rgb(59, 66, 82),
    },
};

/// The Tokyo Night theme.
///
/// Dark indigo surfaces with neon pink, blue, and cyan accents; a cool,
/// high-contrast cast for night work.
const TOKYO_NIGHT: ThemeSpec = ThemeSpec {
    name: "Tokyo Night",
    syntax: SyntaxColors {
        attribute: Color::Rgb(122, 162, 247),
        comment: Color::Rgb(92, 106, 152),
        constant: Color::Rgb(187, 154, 247),
        constructor: Color::Rgb(122, 162, 247),
        embedded: Color::Rgb(169, 177, 214),
        function: Color::Rgb(122, 162, 247),
        keyword: Color::Rgb(224, 108, 117),
        number: Color::Rgb(187, 154, 247),
        operator: Color::Rgb(224, 108, 117),
        property: Color::Rgb(151, 206, 238),
        punctuation: Color::Rgb(92, 106, 152),
        string: Color::Rgb(169, 177, 214),
        r#type: Color::Rgb(235, 137, 88),
        variable: Color::Rgb(151, 206, 238),
        variable_builtin: Color::Rgb(151, 206, 238),
        tag: Color::Rgb(224, 108, 117),
        delimiter: Color::Rgb(224, 108, 117),
        escape: Color::Rgb(187, 154, 247),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(224, 108, 117),
            Color::Rgb(122, 162, 247),
            Color::Rgb(187, 154, 247),
            Color::Rgb(235, 137, 88),
            Color::Rgb(151, 206, 238),
            Color::Rgb(169, 177, 214),
        ],
        code_inline: Color::Rgb(235, 137, 88),
        code_block: Color::Rgb(200, 208, 255),
        link: Color::Rgb(151, 206, 238),
        quote: Color::Rgb(92, 106, 152),
        list_item: Color::Rgb(200, 208, 255),
        horizontal_rule: Color::Rgb(92, 106, 152),
    },
    ui: UiColors {
        background: Color::Rgb(26, 27, 38),
        surface: Color::Rgb(41, 46, 66),
        foreground: Color::Rgb(200, 208, 255),
        primary: Color::Rgb(122, 162, 247),
        secondary: Color::Rgb(92, 106, 152),
        dim: Color::Rgb(92, 106, 152),
        border_color: Color::Rgb(42, 44, 60),
        focused_border_color: Color::Rgb(122, 162, 247),
        user_message_fg: Color::Rgb(200, 208, 255),
        assistant_message_fg: Color::Rgb(200, 208, 255),
        input_border: Color::Rgb(92, 106, 152),
        input_text: Color::Rgb(200, 208, 255),
        status_bar_bg: Color::Rgb(26, 27, 38),
        status_bar_fg: Color::Rgb(200, 208, 255),
        composer_border: None,
        status_success: Color::Rgb(169, 177, 214),
        status_warning: Color::Rgb(235, 137, 88),
        status_error: Color::Rgb(224, 108, 117),
        scrollbar_thumb: Color::Rgb(92, 106, 152),
        scrollbar_track: Color::Rgb(42, 44, 60),
    },
};

/// The Gruvbox Dark theme.
///
/// Warm retro earth tones on a dark brown-gray base; orange and yellow
/// accents keep long code sessions gentle on the eyes.
const GRUVBOX_DARK: ThemeSpec = ThemeSpec {
    name: "Gruvbox Dark",
    syntax: SyntaxColors {
        attribute: Color::Rgb(69, 133, 136),
        comment: Color::Rgb(146, 131, 116),
        constant: Color::Rgb(204, 36, 29),
        constructor: Color::Rgb(69, 133, 136),
        embedded: Color::Rgb(152, 151, 26),
        function: Color::Rgb(69, 133, 136),
        keyword: Color::Rgb(177, 98, 134),
        number: Color::Rgb(204, 36, 29),
        operator: Color::Rgb(177, 98, 134),
        property: Color::Rgb(235, 219, 178),
        punctuation: Color::Rgb(146, 131, 116),
        string: Color::Rgb(152, 151, 26),
        r#type: Color::Rgb(215, 153, 33),
        variable: Color::Rgb(235, 219, 178),
        variable_builtin: Color::Rgb(235, 219, 178),
        tag: Color::Rgb(177, 98, 134),
        delimiter: Color::Rgb(177, 98, 134),
        escape: Color::Rgb(251, 73, 52),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(69, 133, 136),
            Color::Rgb(177, 98, 134),
            Color::Rgb(215, 153, 33),
            Color::Rgb(152, 151, 26),
            Color::Rgb(104, 157, 106),
            Color::Rgb(235, 219, 178),
        ],
        code_inline: Color::Rgb(215, 153, 33),
        code_block: Color::Rgb(235, 219, 178),
        link: Color::Rgb(104, 157, 106),
        quote: Color::Rgb(146, 131, 116),
        list_item: Color::Rgb(235, 219, 178),
        horizontal_rule: Color::Rgb(146, 131, 116),
    },
    ui: UiColors {
        background: Color::Rgb(40, 40, 40),
        surface: Color::Rgb(60, 56, 54),
        foreground: Color::Rgb(235, 219, 178),
        primary: Color::Rgb(177, 98, 134),
        secondary: Color::Rgb(146, 131, 116),
        dim: Color::Rgb(146, 131, 116),
        border_color: Color::Rgb(146, 131, 116),
        focused_border_color: Color::Rgb(177, 98, 134),
        user_message_fg: Color::Rgb(235, 219, 178),
        assistant_message_fg: Color::Rgb(235, 219, 178),
        input_border: Color::Rgb(146, 131, 116),
        input_text: Color::Rgb(235, 219, 178),
        status_bar_bg: Color::Rgb(40, 40, 40),
        status_bar_fg: Color::Rgb(235, 219, 178),
        composer_border: None,
        status_success: Color::Rgb(152, 151, 26),
        status_warning: Color::Rgb(215, 153, 33),
        status_error: Color::Rgb(204, 36, 29),
        scrollbar_thumb: Color::Rgb(146, 131, 116),
        scrollbar_track: Color::Rgb(60, 56, 54),
    },
};

/// The Gruvbox Light theme.
///
/// The same warm hues on paper-white; suited to bright rooms and
/// print-like reading.
const GRUVBOX_LIGHT: ThemeSpec = ThemeSpec {
    name: "Gruvbox Light",
    syntax: SyntaxColors {
        attribute: Color::Rgb(7, 102, 120),
        comment: Color::Rgb(146, 131, 116),
        constant: Color::Rgb(157, 0, 6),
        constructor: Color::Rgb(7, 102, 120),
        embedded: Color::Rgb(121, 116, 14),
        function: Color::Rgb(7, 102, 120),
        keyword: Color::Rgb(143, 63, 113),
        number: Color::Rgb(157, 0, 6),
        operator: Color::Rgb(143, 63, 113),
        property: Color::Rgb(60, 56, 54),
        punctuation: Color::Rgb(146, 131, 116),
        string: Color::Rgb(121, 116, 14),
        r#type: Color::Rgb(181, 118, 0),
        variable: Color::Rgb(60, 56, 54),
        variable_builtin: Color::Rgb(60, 56, 54),
        tag: Color::Rgb(143, 63, 113),
        delimiter: Color::Rgb(143, 63, 113),
        escape: Color::Rgb(204, 36, 29),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(7, 102, 120),
            Color::Rgb(143, 63, 113),
            Color::Rgb(181, 118, 0),
            Color::Rgb(121, 116, 14),
            Color::Rgb(66, 123, 88),
            Color::Rgb(60, 56, 54),
        ],
        code_inline: Color::Rgb(175, 58, 3),
        code_block: Color::Rgb(60, 56, 54),
        link: Color::Rgb(66, 123, 88),
        quote: Color::Rgb(146, 131, 116),
        list_item: Color::Rgb(60, 56, 54),
        horizontal_rule: Color::Rgb(146, 131, 116),
    },
    ui: UiColors {
        background: Color::Rgb(251, 241, 199),
        surface: Color::Rgb(235, 219, 178),
        foreground: Color::Rgb(60, 56, 54),
        primary: Color::Rgb(177, 98, 134),
        secondary: Color::Rgb(146, 131, 116),
        dim: Color::Rgb(146, 131, 116),
        border_color: Color::Rgb(146, 131, 116),
        focused_border_color: Color::Rgb(177, 98, 134),
        user_message_fg: Color::Rgb(60, 56, 54),
        assistant_message_fg: Color::Rgb(60, 56, 54),
        input_border: Color::Rgb(146, 131, 116),
        input_text: Color::Rgb(60, 56, 54),
        status_bar_bg: Color::Rgb(251, 241, 199),
        status_bar_fg: Color::Rgb(60, 56, 54),
        composer_border: None,
        status_success: Color::Rgb(152, 151, 26),
        status_warning: Color::Rgb(215, 153, 33),
        status_error: Color::Rgb(204, 36, 29),
        scrollbar_thumb: Color::Rgb(146, 131, 116),
        scrollbar_track: Color::Rgb(235, 219, 178),
    },
};

/// The Solarized Dark theme.
///
/// The classic precision palette on a deep blue-green base; a
/// deliberately limited hue range with carefully tuned contrast.
const SOLARIZED_DARK: ThemeSpec = ThemeSpec {
    name: "Solarized Dark",
    syntax: SyntaxColors {
        attribute: Color::Rgb(38, 139, 210),
        comment: Color::Rgb(45, 79, 87),
        constant: Color::Rgb(220, 50, 47),
        constructor: Color::Rgb(38, 139, 210),
        embedded: Color::Rgb(133, 153, 0),
        function: Color::Rgb(38, 139, 210),
        keyword: Color::Rgb(211, 54, 130),
        number: Color::Rgb(220, 50, 47),
        operator: Color::Rgb(211, 54, 130),
        property: Color::Rgb(131, 148, 150),
        punctuation: Color::Rgb(45, 79, 87),
        string: Color::Rgb(133, 153, 0),
        r#type: Color::Rgb(181, 137, 0),
        variable: Color::Rgb(131, 148, 150),
        variable_builtin: Color::Rgb(131, 148, 150),
        tag: Color::Rgb(211, 54, 130),
        delimiter: Color::Rgb(211, 54, 130),
        escape: Color::Rgb(203, 75, 22),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(38, 139, 210),
            Color::Rgb(211, 54, 130),
            Color::Rgb(181, 137, 0),
            Color::Rgb(133, 153, 0),
            Color::Rgb(42, 161, 152),
            Color::Rgb(253, 246, 227),
        ],
        code_inline: Color::Rgb(181, 137, 0),
        code_block: Color::Rgb(131, 148, 150),
        link: Color::Rgb(42, 161, 152),
        quote: Color::Rgb(45, 79, 87),
        list_item: Color::Rgb(131, 148, 150),
        horizontal_rule: Color::Rgb(45, 79, 87),
    },
    ui: UiColors {
        background: Color::Rgb(0, 43, 54),
        surface: Color::Rgb(7, 54, 66),
        foreground: Color::Rgb(131, 148, 150),
        primary: Color::Rgb(211, 54, 130),
        secondary: Color::Rgb(45, 79, 87),
        dim: Color::Rgb(45, 79, 87),
        border_color: Color::Rgb(45, 79, 87),
        focused_border_color: Color::Rgb(211, 54, 130),
        user_message_fg: Color::Rgb(131, 148, 150),
        assistant_message_fg: Color::Rgb(131, 148, 150),
        input_border: Color::Rgb(45, 79, 87),
        input_text: Color::Rgb(131, 148, 150),
        status_bar_bg: Color::Rgb(0, 43, 54),
        status_bar_fg: Color::Rgb(131, 148, 150),
        composer_border: None,
        status_success: Color::Rgb(133, 153, 0),
        status_warning: Color::Rgb(181, 137, 0),
        status_error: Color::Rgb(220, 50, 47),
        scrollbar_thumb: Color::Rgb(45, 79, 87),
        scrollbar_track: Color::Rgb(7, 54, 66),
    },
};

/// The Solarized Light theme.
///
/// The Solarized precision palette on a warm cream base; the same hue
/// discipline as its dark sibling, in daylight.
const SOLARIZED_LIGHT: ThemeSpec = ThemeSpec {
    name: "Solarized Light",
    syntax: SyntaxColors {
        attribute: Color::Rgb(38, 139, 210),
        comment: Color::Rgb(0, 43, 54),
        constant: Color::Rgb(220, 50, 47),
        constructor: Color::Rgb(38, 139, 210),
        embedded: Color::Rgb(133, 153, 0),
        function: Color::Rgb(38, 139, 210),
        keyword: Color::Rgb(211, 54, 130),
        number: Color::Rgb(220, 50, 47),
        operator: Color::Rgb(211, 54, 130),
        property: Color::Rgb(88, 110, 117),
        punctuation: Color::Rgb(0, 43, 54),
        string: Color::Rgb(133, 153, 0),
        r#type: Color::Rgb(181, 137, 0),
        variable: Color::Rgb(88, 110, 117),
        variable_builtin: Color::Rgb(88, 110, 117),
        tag: Color::Rgb(211, 54, 130),
        delimiter: Color::Rgb(211, 54, 130),
        escape: Color::Rgb(203, 75, 22),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(38, 139, 210),
            Color::Rgb(211, 54, 130),
            Color::Rgb(181, 137, 0),
            Color::Rgb(133, 153, 0),
            Color::Rgb(42, 161, 152),
            Color::Rgb(88, 110, 117),
        ],
        code_inline: Color::Rgb(88, 110, 117),
        code_block: Color::Rgb(88, 110, 117),
        link: Color::Rgb(42, 161, 152),
        quote: Color::Rgb(0, 43, 54),
        list_item: Color::Rgb(88, 110, 117),
        horizontal_rule: Color::Rgb(0, 43, 54),
    },
    ui: UiColors {
        background: Color::Rgb(253, 246, 227),
        surface: Color::Rgb(238, 232, 213),
        foreground: Color::Rgb(88, 110, 117),
        primary: Color::Rgb(211, 54, 130),
        secondary: Color::Rgb(0, 43, 54),
        dim: Color::Rgb(0, 43, 54),
        border_color: Color::Rgb(0, 43, 54),
        focused_border_color: Color::Rgb(211, 54, 130),
        user_message_fg: Color::Rgb(88, 110, 117),
        assistant_message_fg: Color::Rgb(88, 110, 117),
        input_border: Color::Rgb(0, 43, 54),
        input_text: Color::Rgb(88, 110, 117),
        status_bar_bg: Color::Rgb(253, 246, 227),
        status_bar_fg: Color::Rgb(88, 110, 117),
        composer_border: None,
        status_success: Color::Rgb(133, 153, 0),
        status_warning: Color::Rgb(181, 137, 0),
        status_error: Color::Rgb(220, 50, 47),
        scrollbar_thumb: Color::Rgb(0, 43, 54),
        scrollbar_track: Color::Rgb(238, 232, 213),
    },
};

/// The Catppuccin Latte theme.
///
/// The light flavor of the Catppuccin family: pastel accents on warm
/// white, soothing rather than stark.
const CATPPUCCIN_LATTE: ThemeSpec = ThemeSpec {
    name: "Catppuccin Latte",
    syntax: SyntaxColors {
        attribute: Color::Rgb(30, 102, 245),
        comment: Color::Rgb(108, 111, 133),
        constant: Color::Rgb(210, 15, 57),
        constructor: Color::Rgb(30, 102, 245),
        embedded: Color::Rgb(64, 160, 43),
        function: Color::Rgb(30, 102, 245),
        keyword: Color::Rgb(234, 118, 203),
        number: Color::Rgb(210, 15, 57),
        operator: Color::Rgb(234, 118, 203),
        property: Color::Rgb(76, 79, 105),
        punctuation: Color::Rgb(108, 111, 133),
        string: Color::Rgb(64, 160, 43),
        r#type: Color::Rgb(223, 142, 29),
        variable: Color::Rgb(76, 79, 105),
        variable_builtin: Color::Rgb(76, 79, 105),
        tag: Color::Rgb(234, 118, 203),
        delimiter: Color::Rgb(234, 118, 203),
        escape: Color::Rgb(210, 15, 57),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(30, 102, 245),
            Color::Rgb(234, 118, 203),
            Color::Rgb(223, 142, 29),
            Color::Rgb(64, 160, 43),
            Color::Rgb(23, 146, 153),
            Color::Rgb(188, 192, 204),
        ],
        code_inline: Color::Rgb(76, 79, 105),
        code_block: Color::Rgb(76, 79, 105),
        link: Color::Rgb(23, 146, 153),
        quote: Color::Rgb(108, 111, 133),
        list_item: Color::Rgb(76, 79, 105),
        horizontal_rule: Color::Rgb(108, 111, 133),
    },
    ui: UiColors {
        background: Color::Rgb(239, 241, 245),
        surface: Color::Rgb(230, 233, 239),
        foreground: Color::Rgb(76, 79, 105),
        primary: Color::Rgb(234, 118, 203),
        secondary: Color::Rgb(108, 111, 133),
        dim: Color::Rgb(108, 111, 133),
        border_color: Color::Rgb(108, 111, 133),
        focused_border_color: Color::Rgb(234, 118, 203),
        user_message_fg: Color::Rgb(76, 79, 105),
        assistant_message_fg: Color::Rgb(76, 79, 105),
        input_border: Color::Rgb(108, 111, 133),
        input_text: Color::Rgb(76, 79, 105),
        status_bar_bg: Color::Rgb(239, 241, 245),
        status_bar_fg: Color::Rgb(76, 79, 105),
        composer_border: None,
        status_success: Color::Rgb(64, 160, 43),
        status_warning: Color::Rgb(223, 142, 29),
        status_error: Color::Rgb(210, 15, 57),
        scrollbar_thumb: Color::Rgb(108, 111, 133),
        scrollbar_track: Color::Rgb(220, 224, 228),
    },
};

/// The Catppuccin Frappe theme.
///
/// Pastel accents on a medium-dark base — the lightest of the family's
/// dark flavors.
const CATPPUCCIN_FRAPPE: ThemeSpec = ThemeSpec {
    name: "Catppuccin Frappe",
    syntax: SyntaxColors {
        attribute: Color::Rgb(140, 170, 238),
        comment: Color::Rgb(98, 104, 128),
        constant: Color::Rgb(231, 130, 132),
        constructor: Color::Rgb(140, 170, 238),
        embedded: Color::Rgb(166, 209, 137),
        function: Color::Rgb(140, 170, 238),
        keyword: Color::Rgb(244, 184, 228),
        number: Color::Rgb(231, 130, 132),
        operator: Color::Rgb(244, 184, 228),
        property: Color::Rgb(198, 208, 245),
        punctuation: Color::Rgb(98, 104, 128),
        string: Color::Rgb(166, 209, 137),
        r#type: Color::Rgb(229, 200, 144),
        variable: Color::Rgb(198, 208, 245),
        variable_builtin: Color::Rgb(198, 208, 245),
        tag: Color::Rgb(244, 184, 228),
        delimiter: Color::Rgb(244, 184, 228),
        escape: Color::Rgb(231, 130, 132),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(140, 170, 238),
            Color::Rgb(244, 184, 228),
            Color::Rgb(229, 200, 144),
            Color::Rgb(166, 209, 137),
            Color::Rgb(129, 200, 190),
            Color::Rgb(165, 173, 206),
        ],
        code_inline: Color::Rgb(229, 200, 144),
        code_block: Color::Rgb(198, 208, 245),
        link: Color::Rgb(129, 200, 190),
        quote: Color::Rgb(98, 104, 128),
        list_item: Color::Rgb(198, 208, 245),
        horizontal_rule: Color::Rgb(98, 104, 128),
    },
    ui: UiColors {
        background: Color::Rgb(48, 52, 70),
        surface: Color::Rgb(41, 44, 60),
        foreground: Color::Rgb(198, 208, 245),
        primary: Color::Rgb(244, 184, 228),
        secondary: Color::Rgb(98, 104, 128),
        dim: Color::Rgb(98, 104, 128),
        border_color: Color::Rgb(98, 104, 128),
        focused_border_color: Color::Rgb(244, 184, 228),
        user_message_fg: Color::Rgb(198, 208, 245),
        assistant_message_fg: Color::Rgb(198, 208, 245),
        input_border: Color::Rgb(98, 104, 128),
        input_text: Color::Rgb(198, 208, 245),
        status_bar_bg: Color::Rgb(48, 52, 70),
        status_bar_fg: Color::Rgb(198, 208, 245),
        composer_border: None,
        status_success: Color::Rgb(166, 209, 137),
        status_warning: Color::Rgb(229, 200, 144),
        status_error: Color::Rgb(231, 130, 132),
        scrollbar_thumb: Color::Rgb(98, 104, 128),
        scrollbar_track: Color::Rgb(64, 69, 89),
    },
};

/// The Catppuccin Macchiato theme.
///
/// Pastel accents a step darker than Frappe; a middle stop for those
/// who find the darker flavors too deep.
const CATPPUCCIN_MACCHIATO: ThemeSpec = ThemeSpec {
    name: "Catppuccin Macchiato",
    syntax: SyntaxColors {
        attribute: Color::Rgb(138, 173, 244),
        comment: Color::Rgb(91, 96, 120),
        constant: Color::Rgb(237, 135, 150),
        constructor: Color::Rgb(138, 173, 244),
        embedded: Color::Rgb(166, 218, 149),
        function: Color::Rgb(138, 173, 244),
        keyword: Color::Rgb(245, 189, 230),
        number: Color::Rgb(237, 135, 150),
        operator: Color::Rgb(245, 189, 230),
        property: Color::Rgb(202, 211, 245),
        punctuation: Color::Rgb(91, 96, 120),
        string: Color::Rgb(166, 218, 149),
        r#type: Color::Rgb(238, 212, 159),
        variable: Color::Rgb(202, 211, 245),
        variable_builtin: Color::Rgb(202, 211, 245),
        tag: Color::Rgb(245, 189, 230),
        delimiter: Color::Rgb(245, 189, 230),
        escape: Color::Rgb(237, 135, 150),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(138, 173, 244),
            Color::Rgb(245, 189, 230),
            Color::Rgb(238, 212, 159),
            Color::Rgb(166, 218, 149),
            Color::Rgb(139, 213, 202),
            Color::Rgb(165, 173, 203),
        ],
        code_inline: Color::Rgb(238, 212, 159),
        code_block: Color::Rgb(202, 211, 245),
        link: Color::Rgb(139, 213, 202),
        quote: Color::Rgb(91, 96, 120),
        list_item: Color::Rgb(202, 211, 245),
        horizontal_rule: Color::Rgb(91, 96, 120),
    },
    ui: UiColors {
        background: Color::Rgb(36, 39, 58),
        surface: Color::Rgb(30, 32, 48),
        foreground: Color::Rgb(202, 211, 245),
        primary: Color::Rgb(245, 189, 230),
        secondary: Color::Rgb(91, 96, 120),
        dim: Color::Rgb(91, 96, 120),
        border_color: Color::Rgb(91, 96, 120),
        focused_border_color: Color::Rgb(245, 189, 230),
        user_message_fg: Color::Rgb(202, 211, 245),
        assistant_message_fg: Color::Rgb(202, 211, 245),
        input_border: Color::Rgb(91, 96, 120),
        input_text: Color::Rgb(202, 211, 245),
        status_bar_bg: Color::Rgb(36, 39, 58),
        status_bar_fg: Color::Rgb(202, 211, 245),
        composer_border: None,
        status_success: Color::Rgb(166, 218, 149),
        status_warning: Color::Rgb(238, 212, 159),
        status_error: Color::Rgb(237, 135, 150),
        scrollbar_thumb: Color::Rgb(91, 96, 120),
        scrollbar_track: Color::Rgb(54, 58, 79),
    },
};

/// The Catppuccin Mocha theme.
///
/// The darkest Catppuccin flavor: pastel accents on deep cocoa
/// surfaces, gentle on the eyes without going pure black.
const CATPPUCCIN_MOCHA: ThemeSpec = ThemeSpec {
    name: "Catppuccin Mocha",
    syntax: SyntaxColors {
        attribute: Color::Rgb(137, 180, 250),
        comment: Color::Rgb(88, 91, 112),
        constant: Color::Rgb(243, 139, 168),
        constructor: Color::Rgb(137, 180, 250),
        embedded: Color::Rgb(166, 227, 161),
        function: Color::Rgb(137, 180, 250),
        keyword: Color::Rgb(245, 194, 231),
        number: Color::Rgb(243, 139, 168),
        operator: Color::Rgb(245, 194, 231),
        property: Color::Rgb(205, 214, 244),
        punctuation: Color::Rgb(88, 91, 112),
        string: Color::Rgb(166, 227, 161),
        r#type: Color::Rgb(249, 226, 175),
        variable: Color::Rgb(205, 214, 244),
        variable_builtin: Color::Rgb(205, 214, 244),
        tag: Color::Rgb(245, 194, 231),
        delimiter: Color::Rgb(245, 194, 231),
        escape: Color::Rgb(243, 139, 168),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(137, 180, 250),
            Color::Rgb(245, 194, 231),
            Color::Rgb(249, 226, 175),
            Color::Rgb(166, 227, 161),
            Color::Rgb(148, 226, 213),
            Color::Rgb(166, 173, 200),
        ],
        code_inline: Color::Rgb(249, 226, 175),
        code_block: Color::Rgb(205, 214, 244),
        link: Color::Rgb(148, 226, 213),
        quote: Color::Rgb(88, 91, 112),
        list_item: Color::Rgb(205, 214, 244),
        horizontal_rule: Color::Rgb(88, 91, 112),
    },
    ui: UiColors {
        background: Color::Rgb(30, 30, 46),
        surface: Color::Rgb(24, 24, 37),
        foreground: Color::Rgb(205, 214, 244),
        primary: Color::Rgb(245, 194, 231),
        secondary: Color::Rgb(88, 91, 112),
        dim: Color::Rgb(88, 91, 112),
        border_color: Color::Rgb(88, 91, 112),
        focused_border_color: Color::Rgb(245, 194, 231),
        user_message_fg: Color::Rgb(205, 214, 244),
        assistant_message_fg: Color::Rgb(205, 214, 244),
        input_border: Color::Rgb(88, 91, 112),
        input_text: Color::Rgb(205, 214, 244),
        status_bar_bg: Color::Rgb(30, 30, 46),
        status_bar_fg: Color::Rgb(205, 214, 244),
        composer_border: None,
        status_success: Color::Rgb(166, 227, 161),
        status_warning: Color::Rgb(249, 226, 175),
        status_error: Color::Rgb(243, 139, 168),
        scrollbar_thumb: Color::Rgb(88, 91, 112),
        scrollbar_track: Color::Rgb(49, 50, 68),
    },
};

/// The One Dark theme.
///
/// Atom's classic dark palette: cool grays with blue, green, and coral
/// accents, tuned for editor readability.
const ONE_DARK: ThemeSpec = ThemeSpec {
    name: "One Dark",
    syntax: SyntaxColors {
        attribute: Color::Rgb(97, 175, 239),
        comment: Color::Rgb(92, 99, 112),
        constant: Color::Rgb(224, 108, 117),
        constructor: Color::Rgb(97, 175, 239),
        embedded: Color::Rgb(152, 195, 121),
        function: Color::Rgb(97, 175, 239),
        keyword: Color::Rgb(198, 120, 221),
        number: Color::Rgb(224, 108, 117),
        operator: Color::Rgb(198, 120, 221),
        property: Color::Rgb(171, 178, 191),
        punctuation: Color::Rgb(92, 99, 112),
        string: Color::Rgb(152, 195, 121),
        r#type: Color::Rgb(209, 154, 102),
        variable: Color::Rgb(171, 178, 191),
        variable_builtin: Color::Rgb(171, 178, 191),
        tag: Color::Rgb(198, 120, 221),
        delimiter: Color::Rgb(198, 120, 221),
        escape: Color::Rgb(224, 108, 117),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(97, 175, 239),
            Color::Rgb(198, 120, 221),
            Color::Rgb(209, 154, 102),
            Color::Rgb(152, 195, 121),
            Color::Rgb(171, 178, 191),
            Color::Rgb(255, 255, 255),
        ],
        code_inline: Color::Rgb(209, 154, 102),
        code_block: Color::Rgb(171, 178, 191),
        link: Color::Rgb(97, 175, 239),
        quote: Color::Rgb(92, 99, 112),
        list_item: Color::Rgb(171, 178, 191),
        horizontal_rule: Color::Rgb(92, 99, 112),
    },
    ui: UiColors {
        background: Color::Rgb(40, 44, 52),
        surface: Color::Rgb(44, 49, 58),
        foreground: Color::Rgb(171, 178, 191),
        primary: Color::Rgb(198, 120, 221),
        secondary: Color::Rgb(92, 99, 112),
        dim: Color::Rgb(92, 99, 112),
        border_color: Color::Rgb(92, 99, 112),
        focused_border_color: Color::Rgb(198, 120, 221),
        user_message_fg: Color::Rgb(171, 178, 191),
        assistant_message_fg: Color::Rgb(171, 178, 191),
        input_border: Color::Rgb(92, 99, 112),
        input_text: Color::Rgb(171, 178, 191),
        status_bar_bg: Color::Rgb(40, 44, 52),
        status_bar_fg: Color::Rgb(171, 178, 191),
        composer_border: None,
        status_success: Color::Rgb(152, 195, 121),
        status_warning: Color::Rgb(209, 154, 102),
        status_error: Color::Rgb(224, 108, 117),
        scrollbar_thumb: Color::Rgb(92, 99, 112),
        scrollbar_track: Color::Rgb(52, 57, 66),
    },
};

/// The Monokai theme.
///
/// The vintage high-contrast dark palette: yellow, green, and magenta on
/// near-black, for those who like their syntax loud.
const MONOKAI: ThemeSpec = ThemeSpec {
    name: "Monokai",
    syntax: SyntaxColors {
        attribute: Color::Rgb(102, 217, 239),
        comment: Color::Rgb(117, 113, 94),
        constant: Color::Rgb(249, 38, 114),
        constructor: Color::Rgb(102, 217, 239),
        embedded: Color::Rgb(166, 226, 46),
        function: Color::Rgb(102, 217, 239),
        keyword: Color::Rgb(174, 129, 255),
        number: Color::Rgb(249, 38, 114),
        operator: Color::Rgb(174, 129, 255),
        property: Color::Rgb(248, 248, 242),
        punctuation: Color::Rgb(117, 113, 94),
        string: Color::Rgb(166, 226, 46),
        r#type: Color::Rgb(244, 191, 117),
        variable: Color::Rgb(248, 248, 242),
        variable_builtin: Color::Rgb(248, 248, 242),
        tag: Color::Rgb(174, 129, 255),
        delimiter: Color::Rgb(174, 129, 255),
        escape: Color::Rgb(249, 38, 114),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(102, 217, 239),
            Color::Rgb(174, 129, 255),
            Color::Rgb(244, 191, 117),
            Color::Rgb(166, 226, 46),
            Color::Rgb(161, 239, 228),
            Color::Rgb(249, 248, 245),
        ],
        code_inline: Color::Rgb(244, 191, 117),
        code_block: Color::Rgb(248, 248, 242),
        link: Color::Rgb(161, 239, 228),
        quote: Color::Rgb(117, 113, 94),
        list_item: Color::Rgb(248, 248, 242),
        horizontal_rule: Color::Rgb(117, 113, 94),
    },
    ui: UiColors {
        background: Color::Rgb(39, 40, 34),
        surface: Color::Rgb(62, 61, 50),
        foreground: Color::Rgb(248, 248, 242),
        primary: Color::Rgb(174, 129, 255),
        secondary: Color::Rgb(117, 113, 94),
        dim: Color::Rgb(117, 113, 94),
        border_color: Color::Rgb(117, 113, 94),
        focused_border_color: Color::Rgb(174, 129, 255),
        user_message_fg: Color::Rgb(248, 248, 242),
        assistant_message_fg: Color::Rgb(248, 248, 242),
        input_border: Color::Rgb(117, 113, 94),
        input_text: Color::Rgb(248, 248, 242),
        status_bar_bg: Color::Rgb(39, 40, 34),
        status_bar_fg: Color::Rgb(248, 248, 242),
        composer_border: None,
        status_success: Color::Rgb(166, 226, 46),
        status_warning: Color::Rgb(244, 191, 117),
        status_error: Color::Rgb(249, 38, 114),
        scrollbar_thumb: Color::Rgb(117, 113, 94),
        scrollbar_track: Color::Rgb(62, 61, 50),
    },
};

/// The `GitHub` Dark theme.
///
/// `GitHub`'s own dark-mode code palette: restrained blues and greens on
/// near-black, optimized for reading diffs and prose alike.
const GITHUB_DARK: ThemeSpec = ThemeSpec {
    name: "GitHub Dark",
    syntax: SyntaxColors {
        attribute: Color::Rgb(33, 136, 255),
        comment: Color::Rgb(149, 157, 165),
        constant: Color::Rgb(234, 74, 90),
        constructor: Color::Rgb(33, 136, 255),
        embedded: Color::Rgb(52, 208, 88),
        function: Color::Rgb(33, 136, 255),
        keyword: Color::Rgb(179, 146, 240),
        number: Color::Rgb(234, 74, 90),
        operator: Color::Rgb(179, 146, 240),
        property: Color::Rgb(209, 213, 218),
        punctuation: Color::Rgb(149, 157, 165),
        string: Color::Rgb(52, 208, 88),
        r#type: Color::Rgb(255, 234, 127),
        variable: Color::Rgb(209, 213, 218),
        variable_builtin: Color::Rgb(209, 213, 218),
        tag: Color::Rgb(179, 146, 240),
        delimiter: Color::Rgb(179, 146, 240),
        escape: Color::Rgb(249, 117, 131),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(33, 136, 255),
            Color::Rgb(179, 146, 240),
            Color::Rgb(255, 234, 127),
            Color::Rgb(52, 208, 88),
            Color::Rgb(57, 197, 207),
            Color::Rgb(250, 251, 252),
        ],
        code_inline: Color::Rgb(255, 234, 127),
        code_block: Color::Rgb(209, 213, 218),
        link: Color::Rgb(57, 197, 207),
        quote: Color::Rgb(149, 157, 165),
        list_item: Color::Rgb(209, 213, 218),
        horizontal_rule: Color::Rgb(149, 157, 165),
    },
    ui: UiColors {
        background: Color::Rgb(36, 41, 46),
        surface: Color::Rgb(48, 54, 61),
        foreground: Color::Rgb(209, 213, 218),
        primary: Color::Rgb(179, 146, 240),
        secondary: Color::Rgb(149, 157, 165),
        dim: Color::Rgb(149, 157, 165),
        border_color: Color::Rgb(149, 157, 165),
        focused_border_color: Color::Rgb(179, 146, 240),
        user_message_fg: Color::Rgb(209, 213, 218),
        assistant_message_fg: Color::Rgb(209, 213, 218),
        input_border: Color::Rgb(149, 157, 165),
        input_text: Color::Rgb(209, 213, 218),
        status_bar_bg: Color::Rgb(36, 41, 46),
        status_bar_fg: Color::Rgb(201, 209, 217),
        composer_border: None,
        status_success: Color::Rgb(63, 185, 80),
        status_warning: Color::Rgb(187, 128, 9),
        status_error: Color::Rgb(248, 81, 73),
        scrollbar_thumb: Color::Rgb(149, 157, 165),
        scrollbar_track: Color::Rgb(45, 51, 59),
    },
};

/// The `GitHub` Light theme.
///
/// `GitHub`'s light-mode palette on white; the most conservative light
/// theme in the set.
const GITHUB_LIGHT: ThemeSpec = ThemeSpec {
    name: "GitHub Light",
    syntax: SyntaxColors {
        attribute: Color::Rgb(3, 102, 214),
        comment: Color::Rgb(149, 157, 165),
        constant: Color::Rgb(215, 58, 73),
        constructor: Color::Rgb(3, 102, 214),
        embedded: Color::Rgb(40, 167, 69),
        function: Color::Rgb(3, 102, 214),
        keyword: Color::Rgb(90, 50, 163),
        number: Color::Rgb(215, 58, 73),
        operator: Color::Rgb(90, 50, 163),
        property: Color::Rgb(36, 41, 47),
        punctuation: Color::Rgb(149, 157, 165),
        string: Color::Rgb(40, 167, 69),
        r#type: Color::Rgb(219, 171, 9),
        variable: Color::Rgb(36, 41, 47),
        variable_builtin: Color::Rgb(36, 41, 47),
        tag: Color::Rgb(90, 50, 163),
        delimiter: Color::Rgb(90, 50, 163),
        escape: Color::Rgb(203, 36, 49),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(3, 102, 214),
            Color::Rgb(90, 50, 163),
            Color::Rgb(219, 171, 9),
            Color::Rgb(40, 167, 69),
            Color::Rgb(5, 152, 188),
            Color::Rgb(36, 41, 47),
        ],
        code_inline: Color::Rgb(36, 41, 47),
        code_block: Color::Rgb(36, 41, 47),
        link: Color::Rgb(5, 152, 188),
        quote: Color::Rgb(149, 157, 165),
        list_item: Color::Rgb(36, 41, 47),
        horizontal_rule: Color::Rgb(149, 157, 165),
    },
    ui: UiColors {
        background: Color::Rgb(255, 255, 255),
        surface: Color::Rgb(246, 248, 250),
        foreground: Color::Rgb(36, 41, 47),
        primary: Color::Rgb(90, 50, 163),
        secondary: Color::Rgb(149, 157, 165),
        dim: Color::Rgb(149, 157, 165),
        border_color: Color::Rgb(149, 157, 165),
        focused_border_color: Color::Rgb(90, 50, 163),
        user_message_fg: Color::Rgb(36, 41, 47),
        assistant_message_fg: Color::Rgb(36, 41, 47),
        input_border: Color::Rgb(149, 157, 165),
        input_text: Color::Rgb(36, 41, 47),
        status_bar_bg: Color::Rgb(255, 255, 255),
        status_bar_fg: Color::Rgb(36, 41, 47),
        composer_border: None,
        status_success: Color::Rgb(40, 167, 69),
        status_warning: Color::Rgb(219, 171, 9),
        status_error: Color::Rgb(215, 58, 73),
        scrollbar_thumb: Color::Rgb(149, 157, 165),
        scrollbar_track: Color::Rgb(225, 228, 232),
    },
};

/// Ayu's dark carrier.
///
/// A near-black blue canvas far below the usual slate band, with
/// orange, yellow, and cyan accents — the theme that reads
/// closest to plain black without being it.
const AYU_DARK: ThemeSpec = ThemeSpec {
    name: "Ayu Dark",
    syntax: SyntaxColors {
        attribute: Color::Rgb(83, 189, 250),
        comment: Color::Rgb(104, 104, 104),
        constant: Color::Rgb(234, 108, 115),
        constructor: Color::Rgb(83, 189, 250),
        embedded: Color::Rgb(145, 179, 98),
        function: Color::Rgb(83, 189, 250),
        keyword: Color::Rgb(250, 233, 148),
        number: Color::Rgb(234, 108, 115),
        operator: Color::Rgb(250, 233, 148),
        property: Color::Rgb(179, 177, 173),
        punctuation: Color::Rgb(104, 104, 104),
        string: Color::Rgb(145, 179, 98),
        r#type: Color::Rgb(249, 175, 79),
        variable: Color::Rgb(179, 177, 173),
        variable_builtin: Color::Rgb(179, 177, 173),
        tag: Color::Rgb(250, 233, 148),
        delimiter: Color::Rgb(250, 233, 148),
        escape: Color::Rgb(240, 113, 120),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(83, 189, 250),
            Color::Rgb(250, 233, 148),
            Color::Rgb(249, 175, 79),
            Color::Rgb(145, 179, 98),
            Color::Rgb(144, 225, 198),
            Color::Rgb(255, 255, 255),
        ],
        code_inline: Color::Rgb(249, 175, 79),
        code_block: Color::Rgb(179, 177, 173),
        link: Color::Rgb(144, 225, 198),
        quote: Color::Rgb(104, 104, 104),
        list_item: Color::Rgb(179, 177, 173),
        horizontal_rule: Color::Rgb(104, 104, 104),
    },
    ui: UiColors {
        background: Color::Rgb(10, 14, 20),
        surface: Color::Rgb(16, 20, 28),
        foreground: Color::Rgb(179, 177, 173),
        primary: Color::Rgb(250, 233, 148),
        secondary: Color::Rgb(104, 104, 104),
        dim: Color::Rgb(104, 104, 104),
        border_color: Color::Rgb(104, 104, 104),
        focused_border_color: Color::Rgb(250, 233, 148),
        user_message_fg: Color::Rgb(179, 177, 173),
        assistant_message_fg: Color::Rgb(179, 177, 173),
        input_border: Color::Rgb(104, 104, 104),
        input_text: Color::Rgb(179, 177, 173),
        status_bar_bg: Color::Rgb(10, 14, 20),
        status_bar_fg: Color::Rgb(179, 177, 173),
        composer_border: None,
        status_success: Color::Rgb(145, 179, 98),
        status_warning: Color::Rgb(249, 175, 79),
        status_error: Color::Rgb(234, 108, 115),
        scrollbar_thumb: Color::Rgb(104, 104, 104),
        scrollbar_track: Color::Rgb(60, 60, 66),
    },
};

/// Rose Pine's base palette.
///
/// A deep violet canvas with muted iris, foam, and gold accents —
/// natural, muted pine tones at their darkest step.
const ROSE_PINE: ThemeSpec = ThemeSpec {
    name: "Rose Pine",
    syntax: SyntaxColors {
        attribute: Color::Rgb(156, 207, 216),
        comment: Color::Rgb(110, 106, 134),
        constant: Color::Rgb(235, 111, 146),
        constructor: Color::Rgb(156, 207, 216),
        embedded: Color::Rgb(49, 116, 143),
        function: Color::Rgb(156, 207, 216),
        keyword: Color::Rgb(196, 167, 231),
        number: Color::Rgb(235, 111, 146),
        operator: Color::Rgb(196, 167, 231),
        property: Color::Rgb(224, 222, 244),
        punctuation: Color::Rgb(110, 106, 134),
        string: Color::Rgb(49, 116, 143),
        r#type: Color::Rgb(246, 193, 119),
        variable: Color::Rgb(224, 222, 244),
        variable_builtin: Color::Rgb(224, 222, 244),
        tag: Color::Rgb(196, 167, 231),
        delimiter: Color::Rgb(196, 167, 231),
        escape: Color::Rgb(235, 111, 146),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(156, 207, 216),
            Color::Rgb(196, 167, 231),
            Color::Rgb(246, 193, 119),
            Color::Rgb(49, 116, 143),
            Color::Rgb(235, 188, 186),
            Color::Rgb(224, 222, 244),
        ],
        code_inline: Color::Rgb(246, 193, 119),
        code_block: Color::Rgb(224, 222, 244),
        link: Color::Rgb(235, 188, 186),
        quote: Color::Rgb(110, 106, 134),
        list_item: Color::Rgb(224, 222, 244),
        horizontal_rule: Color::Rgb(110, 106, 134),
    },
    ui: UiColors {
        background: Color::Rgb(25, 23, 36),
        surface: Color::Rgb(31, 29, 46),
        foreground: Color::Rgb(224, 222, 244),
        primary: Color::Rgb(196, 167, 231),
        secondary: Color::Rgb(110, 106, 134),
        dim: Color::Rgb(110, 106, 134),
        border_color: Color::Rgb(110, 106, 134),
        focused_border_color: Color::Rgb(196, 167, 231),
        user_message_fg: Color::Rgb(224, 222, 244),
        assistant_message_fg: Color::Rgb(224, 222, 244),
        input_border: Color::Rgb(110, 106, 134),
        input_text: Color::Rgb(224, 222, 244),
        status_bar_bg: Color::Rgb(25, 23, 36),
        status_bar_fg: Color::Rgb(224, 222, 244),
        composer_border: None,
        status_success: Color::Rgb(49, 116, 143),
        status_warning: Color::Rgb(246, 193, 119),
        status_error: Color::Rgb(235, 111, 146),
        scrollbar_thumb: Color::Rgb(110, 106, 134),
        scrollbar_track: Color::Rgb(38, 35, 58),
    },
};

/// Kanagawa's wave palette.
///
/// An ink-warm canvas drawn from sumi and the blue of The Great
/// Wave, with gold and old-world red accents — the palette's
/// signature step outside the slate band.
const KANAGAWA_WAVE: ThemeSpec = ThemeSpec {
    name: "Kanagawa Wave",
    syntax: SyntaxColors {
        attribute: Color::Rgb(126, 156, 216),
        comment: Color::Rgb(114, 113, 105),
        constant: Color::Rgb(195, 64, 67),
        constructor: Color::Rgb(126, 156, 216),
        embedded: Color::Rgb(118, 148, 106),
        function: Color::Rgb(126, 156, 216),
        keyword: Color::Rgb(149, 127, 184),
        number: Color::Rgb(195, 64, 67),
        operator: Color::Rgb(149, 127, 184),
        property: Color::Rgb(220, 215, 186),
        punctuation: Color::Rgb(114, 113, 105),
        string: Color::Rgb(118, 148, 106),
        r#type: Color::Rgb(192, 163, 110),
        variable: Color::Rgb(220, 215, 186),
        variable_builtin: Color::Rgb(220, 215, 186),
        tag: Color::Rgb(149, 127, 184),
        delimiter: Color::Rgb(149, 127, 184),
        escape: Color::Rgb(232, 36, 36),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(126, 156, 216),
            Color::Rgb(149, 127, 184),
            Color::Rgb(192, 163, 110),
            Color::Rgb(118, 148, 106),
            Color::Rgb(106, 149, 137),
            Color::Rgb(220, 215, 186),
        ],
        code_inline: Color::Rgb(192, 163, 110),
        code_block: Color::Rgb(220, 215, 186),
        link: Color::Rgb(106, 149, 137),
        quote: Color::Rgb(114, 113, 105),
        list_item: Color::Rgb(220, 215, 186),
        horizontal_rule: Color::Rgb(114, 113, 105),
    },
    ui: UiColors {
        background: Color::Rgb(31, 31, 40),
        surface: Color::Rgb(42, 42, 55),
        foreground: Color::Rgb(220, 215, 186),
        primary: Color::Rgb(149, 127, 184),
        secondary: Color::Rgb(114, 113, 105),
        dim: Color::Rgb(114, 113, 105),
        border_color: Color::Rgb(114, 113, 105),
        focused_border_color: Color::Rgb(149, 127, 184),
        user_message_fg: Color::Rgb(220, 215, 186),
        assistant_message_fg: Color::Rgb(220, 215, 186),
        input_border: Color::Rgb(114, 113, 105),
        input_text: Color::Rgb(220, 215, 186),
        status_bar_bg: Color::Rgb(31, 31, 40),
        status_bar_fg: Color::Rgb(220, 215, 186),
        composer_border: None,
        status_success: Color::Rgb(118, 148, 106),
        status_warning: Color::Rgb(192, 163, 110),
        status_error: Color::Rgb(195, 64, 67),
        scrollbar_thumb: Color::Rgb(114, 113, 105),
        scrollbar_track: Color::Rgb(54, 54, 70),
    },
};

/// VS Code's default Dark+ palette.
///
/// The editor everyone knows: a neutral dark canvas with blue
/// functions, purple keywords, red numbers, and green strings —
/// and a canvas that sits below the slate band most dark themes
/// share.
const DARK_PLUS: ThemeSpec = ThemeSpec {
    name: "Dark Plus",
    syntax: SyntaxColors {
        attribute: Color::Rgb(86, 156, 214),
        comment: Color::Rgb(102, 102, 102),
        constant: Color::Rgb(214, 24, 27),
        constructor: Color::Rgb(86, 156, 214),
        embedded: Color::Rgb(106, 153, 85),
        function: Color::Rgb(86, 156, 214),
        keyword: Color::Rgb(188, 63, 188),
        number: Color::Rgb(214, 24, 27),
        operator: Color::Rgb(188, 63, 188),
        property: Color::Rgb(204, 204, 204),
        punctuation: Color::Rgb(102, 102, 102),
        string: Color::Rgb(106, 153, 85),
        r#type: Color::Rgb(228, 210, 1),
        variable: Color::Rgb(204, 204, 204),
        variable_builtin: Color::Rgb(204, 204, 204),
        tag: Color::Rgb(188, 63, 188),
        delimiter: Color::Rgb(188, 63, 188),
        escape: Color::Rgb(206, 145, 120),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(86, 156, 214),
            Color::Rgb(188, 63, 188),
            Color::Rgb(228, 210, 1),
            Color::Rgb(106, 153, 85),
            Color::Rgb(78, 201, 176),
            Color::Rgb(229, 229, 229),
        ],
        code_inline: Color::Rgb(228, 210, 1),
        code_block: Color::Rgb(204, 204, 204),
        link: Color::Rgb(78, 201, 176),
        quote: Color::Rgb(102, 102, 102),
        list_item: Color::Rgb(204, 204, 204),
        horizontal_rule: Color::Rgb(102, 102, 102),
    },
    ui: UiColors {
        background: Color::Rgb(31, 31, 31),
        surface: Color::Rgb(37, 37, 38),
        foreground: Color::Rgb(204, 204, 204),
        primary: Color::Rgb(188, 63, 188),
        secondary: Color::Rgb(102, 102, 102),
        dim: Color::Rgb(102, 102, 102),
        border_color: Color::Rgb(102, 102, 102),
        focused_border_color: Color::Rgb(188, 63, 188),
        user_message_fg: Color::Rgb(204, 204, 204),
        assistant_message_fg: Color::Rgb(204, 204, 204),
        input_border: Color::Rgb(102, 102, 102),
        input_text: Color::Rgb(204, 204, 204),
        status_bar_bg: Color::Rgb(31, 31, 31),
        status_bar_fg: Color::Rgb(204, 204, 204),
        composer_border: None,
        status_success: Color::Rgb(106, 153, 85),
        status_warning: Color::Rgb(228, 210, 1),
        status_error: Color::Rgb(214, 24, 27),
        scrollbar_thumb: Color::Rgb(102, 102, 102),
        scrollbar_track: Color::Rgb(60, 60, 60),
    },
};

/// The Autumn Night theme.
///
/// A faithful port of the Helix editor's `autumn_night` palette:
/// a near-black neutral background under cream foreground text, with
/// autumn accents — red keywords, green strings, yellow functions,
/// and a brown-turquoise supporting family; the cream foreground
/// is where the warmth lives, the background itself is neutral.
/// Syntax and UI cells are faithful, with one accessibility
/// lift: the muted gray-green upstream paints comments and dim
/// text (`#626C66`) sits under the 4.5:1 small-text contrast
/// floor on this canvas, so the three roles that carry words —
/// comments, level-6 headings, and the dim register — use a
/// lighter tone of the same hue, while the decorative cells that
/// share it upstream (the scrollbar thumb, the rule, the input
/// border) stay faithful. Markdown headers grade across the
/// palette for the TUI's six-level structure, where upstream's
/// flat single color would render every level alike. The scheme
/// descends from the Autumn color scheme built for the Komodo IDE,
/// ported to Vim by Kenneth Love and Chris Jones, improved by
/// Yorick Peterse, and ported to Helix by Jens Getreu; this port
/// keeps its night variant's darker background set. The display
/// name is `"Autumn Night"`.
const AUTUMN_NIGHT: ThemeSpec = ThemeSpec {
    name: "Autumn Night",
    syntax: SyntaxColors {
        attribute: Color::Rgb(134, 193, 185),
        comment: Color::Rgb(130, 140, 133),
        constant: Color::Rgb(243, 242, 204),
        constructor: Color::Rgb(250, 213, 102),
        embedded: Color::Rgb(153, 190, 112),
        function: Color::Rgb(250, 213, 102),
        keyword: Color::Rgb(240, 94, 72),
        number: Color::Rgb(134, 193, 185),
        operator: Color::Rgb(243, 242, 204),
        property: Color::Rgb(207, 186, 139),
        punctuation: Color::Rgb(243, 242, 204),
        string: Color::Rgb(153, 190, 112),
        r#type: Color::Rgb(243, 242, 204),
        variable: Color::Rgb(243, 242, 204),
        variable_builtin: Color::Rgb(250, 213, 102),
        tag: Color::Rgb(243, 242, 204),
        delimiter: Color::Rgb(243, 242, 204),
        escape: Color::Rgb(134, 193, 185),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(250, 213, 102),
            Color::Rgb(255, 255, 159),
            Color::Rgb(207, 186, 139),
            Color::Rgb(134, 193, 185),
            Color::Rgb(196, 196, 196),
            Color::Rgb(130, 140, 133),
        ],
        code_inline: Color::Rgb(153, 190, 112),
        code_block: Color::Rgb(153, 190, 112),
        link: Color::Rgb(114, 165, 158),
        quote: Color::Rgb(207, 186, 139),
        list_item: Color::Rgb(243, 242, 204),
        horizontal_rule: Color::Rgb(98, 108, 102),
    },
    ui: UiColors {
        background: Color::Rgb(9, 9, 9),
        surface: Color::Rgb(26, 26, 26),
        foreground: Color::Rgb(243, 242, 204),
        primary: Color::Rgb(250, 213, 102),
        secondary: Color::Rgb(170, 170, 170),
        dim: Color::Rgb(130, 140, 133),
        border_color: Color::Rgb(64, 64, 64),
        focused_border_color: Color::Rgb(250, 213, 102),
        user_message_fg: Color::Rgb(243, 242, 204),
        assistant_message_fg: Color::Rgb(243, 242, 204),
        input_border: Color::Rgb(98, 108, 102),
        input_text: Color::Rgb(243, 242, 204),
        status_bar_bg: Color::Rgb(9, 9, 9),
        status_bar_fg: Color::Rgb(196, 196, 196),
        composer_border: None,
        status_success: Color::Rgb(153, 190, 112),
        status_warning: Color::Rgb(255, 255, 159),
        status_error: Color::Rgb(240, 94, 72),
        scrollbar_thumb: Color::Rgb(98, 108, 102),
        scrollbar_track: Color::Rgb(33, 35, 34),
    },
};

/// The dch house palette: an `opencode`-leaning blend of
/// `opencode` and `autumn_night` — and the default.
///
/// Opencode's neutral near-black keeps the canvas quiet;
/// Autumn Night's warm dusk lifts it. The code palette takes its
/// syntax registers at seven parts `opencode`, one part
/// `autumn_night` — three quarters of the way from the parents'
/// midpoint toward `opencode` — so they read as opencode with a
/// breath of autumn's warmth, except keywords, which carry the
/// house orange (the primary accent's own `#FFB200`); function,
/// constructor, and type names, which read in `autumn_night`'s
/// warm gold blended a tenth toward white (`#FBD975`); and
/// function arguments, which read in `autumn_night`'s own
/// register rather than a blend with opencode's brick red; and
/// punctuation and delimiters, which stay on the foreground
/// family's cream so they defer with it on unsynced terminals. The
/// primary accent and the heading ramp carry the house orange
/// (`#FFB200`) and its lighter title variant (`#FFC133`, the
/// primary blended a fifth toward white). The foreground family —
/// body text, both message registers, and the composer's draft —
/// is a warm cream of its own (`#F4F2D1`), a shade off
/// `autumn_night`'s foreground rather than either parent's. The
/// house theme reads as neither parent while inheriting both
/// canvases' contrast pairs.
/// [`Theme::default`] is this palette, so the `"dch"` key and
/// an unconfigured session both resolve to it.
const DCH: ThemeSpec = ThemeSpec {
    name: "Dch",
    syntax: SyntaxColors {
        attribute: Color::Rgb(223, 189, 139),
        comment: Color::Rgb(124, 125, 124),
        constant: Color::Rgb(247, 169, 88),
        constructor: Color::Rgb(251, 217, 117),
        embedded: Color::Rgb(138, 208, 142),
        function: Color::Rgb(251, 217, 117),
        keyword: Color::Rgb(255, 178, 0),
        number: Color::Rgb(234, 163, 85),
        operator: Color::Rgb(112, 186, 203),
        property: Color::Rgb(107, 179, 195),
        punctuation: Color::Rgb(244, 242, 209),
        string: Color::Rgb(138, 208, 142),
        r#type: Color::Rgb(251, 217, 117),
        variable: Color::Rgb(243, 242, 204),
        variable_builtin: Color::Rgb(224, 129, 110),
        tag: Color::Rgb(161, 143, 213),
        delimiter: Color::Rgb(244, 242, 209),
        escape: Color::Rgb(237, 198, 167),
    },
    markdown: MarkdownColors {
        header: [
            Color::Rgb(255, 193, 51),
            Color::Rgb(255, 193, 51),
            Color::Rgb(255, 193, 51),
            Color::Rgb(255, 193, 51),
            Color::Rgb(255, 193, 51),
            Color::Rgb(255, 193, 51),
        ],
        code_inline: Color::Rgb(145, 200, 129),
        code_block: Color::Rgb(198, 216, 177),
        link: Color::Rgb(181, 175, 143),
        quote: Color::Rgb(222, 187, 136),
        list_item: Color::Rgb(246, 214, 166),
        horizontal_rule: Color::Rgb(111, 116, 113),
    },
    ui: UiColors {
        background: Color::Rgb(12, 12, 12),
        surface: Color::Rgb(29, 29, 29),
        foreground: Color::Rgb(244, 242, 209),
        primary: Color::Rgb(255, 178, 0),
        secondary: Color::Rgb(147, 147, 147),
        dim: Color::Rgb(127, 132, 128),
        border_color: Color::Rgb(71, 71, 71),
        focused_border_color: Color::Rgb(249, 199, 115),
        user_message_fg: Color::Rgb(244, 242, 209),
        assistant_message_fg: Color::Rgb(244, 242, 209),
        input_border: Color::Rgb(111, 116, 113),
        input_text: Color::Rgb(244, 242, 209),
        status_bar_bg: Color::Rgb(12, 12, 12),
        status_bar_fg: Color::Rgb(219, 219, 219),
        composer_border: None,
        status_success: Color::Rgb(145, 200, 129),
        status_warning: Color::Rgb(252, 207, 115),
        status_error: Color::Rgb(230, 106, 92),
        scrollbar_thumb: Color::Rgb(111, 116, 113),
        scrollbar_track: Color::Rgb(45, 46, 46),
    },
};

/// The Transparent theme.
///
/// No colors of its own: every background and foreground defers
/// to the terminal's scheme through `Reset`, so the interface
/// follows the terminal — light or dark, transparent or opaque —
/// instead of painting over it. The session leaves the
/// terminal's default background untouched (no background
/// override), and the palette carries only what must not defer
/// to plain defaults: hairline rules in the terminal's own dim
/// tone underline the rows above and below the composer — thin
/// and solid on every terminal, where glyphs gap between rows
/// and paint is a full cell thick — the dim register as a fixed
/// neutral gray, the scrollbar's own mid tone, so de-emphasized
/// text (thinking bodies, timestamps, system rows) greys out on
/// every terminal: the indexed bright-black is scheme-defined
/// and some schemes — or a contrast-adjusting terminal — render
/// it indistinguishable from the foreground, while a truecolor
/// gray renders exactly and degrades to a visible silver on
/// 256- and 16-color fallbacks; the terminal-indexed semantic
/// trio (green,
/// yellow, red — the scheme's own, not ours) and the
/// scrollbar's neutral grays, mid enough to read on either
/// pole, carry the rest.
/// Syntax highlighting is deliberately absent: code renders in
/// the terminal's own foreground.
const TRANSPARENT: ThemeSpec = ThemeSpec {
    name: "Transparent",
    syntax: SyntaxColors {
        attribute: Color::Reset,
        comment: Color::Reset,
        constant: Color::Reset,
        constructor: Color::Reset,
        embedded: Color::Reset,
        function: Color::Reset,
        keyword: Color::Reset,
        number: Color::Reset,
        operator: Color::Reset,
        property: Color::Reset,
        punctuation: Color::Reset,
        string: Color::Reset,
        r#type: Color::Reset,
        variable: Color::Reset,
        variable_builtin: Color::Reset,
        tag: Color::Reset,
        delimiter: Color::Reset,
        escape: Color::Reset,
    },
    markdown: MarkdownColors {
        header: [
            Color::Reset,
            Color::Reset,
            Color::Reset,
            Color::Reset,
            Color::Reset,
            Color::Reset,
        ],
        code_inline: Color::Reset,
        code_block: Color::Reset,
        link: Color::Reset,
        quote: Color::Reset,
        list_item: Color::Reset,
        horizontal_rule: Color::Reset,
    },
    ui: UiColors {
        background: Color::Reset,
        surface: Color::Reset,
        foreground: Color::Reset,
        primary: Color::Reset,
        secondary: Color::Reset,
        dim: Color::Rgb(142, 142, 142),
        border_color: Color::Reset,
        focused_border_color: Color::Reset,
        user_message_fg: Color::Reset,
        assistant_message_fg: Color::Reset,
        input_border: Color::Reset,
        input_text: Color::Reset,
        status_bar_bg: Color::Reset,
        status_bar_fg: Color::Reset,
        composer_border: Some(Color::Indexed(8)),
        status_success: Color::Indexed(2),
        status_warning: Color::Indexed(3),
        status_error: Color::Indexed(1),
        scrollbar_thumb: Color::Rgb(142, 142, 142),
        scrollbar_track: Color::Rgb(74, 74, 74),
    },
};

/// Retired registry keys that still resolve, and the live key each
/// points at now.
///
/// A rename would otherwise strand every existing config that names
/// the old spelling: the startup path answers an unknown theme with a
/// log line and the fallback — invisible to a TUI user whose colors
/// silently change. Each alias keeps the old spelling resolving to
/// the renamed palette, so an upgrade reads identically. The registry
/// is consulted before the aliases, so a live key can never be
/// shadowed, and the alias test pins that every alias target exists
/// and no alias collides with a registry key.
pub(crate) const THEME_ALIASES: &[(&str, &str)] = &[("default", "opencode")];

/// A theme spec as stored in the lookup table.
///
/// Const references keep the registry const-constructible; each entry
/// pairs a lookup key with the spec that builds its [`Theme`].
pub(crate) const THEME_SPECS: &[(&str, &ThemeSpec)] = &[
    ("dch", &DCH),
    ("opencode", &OPENCODE),
    ("dracula", &DRACULA),
    ("nord", &NORD),
    ("tokyo_night", &TOKYO_NIGHT),
    ("gruvbox_dark", &GRUVBOX_DARK),
    ("gruvbox_light", &GRUVBOX_LIGHT),
    ("solarized_dark", &SOLARIZED_DARK),
    ("solarized_light", &SOLARIZED_LIGHT),
    ("catppuccin_latte", &CATPPUCCIN_LATTE),
    ("catppuccin_frappe", &CATPPUCCIN_FRAPPE),
    ("catppuccin_macchiato", &CATPPUCCIN_MACCHIATO),
    ("catppuccin_mocha", &CATPPUCCIN_MOCHA),
    ("one_dark", &ONE_DARK),
    ("monokai", &MONOKAI),
    ("github_dark", &GITHUB_DARK),
    ("github_light", &GITHUB_LIGHT),
    ("ayu_dark", &AYU_DARK),
    ("rose_pine", &ROSE_PINE),
    ("kanagawa_wave", &KANAGAWA_WAVE),
    ("dark_plus", &DARK_PLUS),
    ("autumn_night", &AUTUMN_NIGHT),
    ("transparent", &TRANSPARENT),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The round-half-up channel average of two colors.
    ///
    /// The exact rounding the DCH construction promises; any other
    /// rounding diverges on odd channel sums.
    fn midpoint(a: Color, b: Color) -> Color {
        fn avg(x: u8, y: u8) -> u8 {
            u8::try_from((u16::from(x) + u16::from(y)).div_ceil(2))
                .expect("two u8 channels average inside u8")
        }
        match (a, b) {
            (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
                Color::Rgb(avg(ar, br), avg(ag, bg), avg(ab, bb))
            }
            _ => panic!("the dch parents carry a non-Rgb color; the midpoint is undefined"),
        }
    }

    /// The syntax construction: three quarters of the way from the
    /// parents' midpoint toward opencode — seven parts opencode to
    /// one part autumn — round-half-up per channel.
    ///
    /// The code palette reads as opencode with a breath of
    /// autumn's warmth; the blend is `(7·opencode + autumn + 4) /
    /// 8` on each channel, which is exactly the three-quarters
    /// interpolation: `m + 3/4·(o − m)` with `m = (o+a)/2`.
    fn toward_opencode(opencode: Color, autumn: Color) -> Color {
        fn blend(o: u8, a: u8) -> u8 {
            u8::try_from((7 * u16::from(o) + u16::from(a) + 4) / 8)
                .expect("the blend stays inside u8")
        }
        match (opencode, autumn) {
            (Color::Rgb(or_, og, ob), Color::Rgb(ar, ag, ab)) => {
                Color::Rgb(blend(or_, ar), blend(og, ag), blend(ob, ab))
            }
            _ => panic!("the dch parents carry a non-Rgb color; the blend is undefined"),
        }
    }

    #[test]
    fn the_opencode_lean_is_exactly_three_quarters() {
        // The pin: the helper agrees with the construction written
        // from the geometry — midpoint m = (o+a)/2, three quarters of
        // the way toward opencode, one round-half-up — not with its
        // own formula copied back. In eighths,
        // 8·v = 2·m + 6·(o − m) = (o + a) + 6·o, so any other
        // weighting (seven eighths of the way, say) lands a full
        // channel step off on most of the 65,536 pairs swept here.
        for o in 0..=u8::MAX {
            for a in 0..=u8::MAX {
                let eighths = u16::from(o) + u16::from(a) + 6 * u16::from(o);
                // The single round-half-up: +4 before the divide by 8.
                let expected = u8::try_from((eighths + 4) / 8)
                    .expect("the three-quarters blend stays inside u8");
                let Color::Rgb(r, g, b) = toward_opencode(Color::Rgb(o, o, o), Color::Rgb(a, a, a))
                else {
                    panic!("toward_opencode must return Rgb for Rgb input");
                };
                assert_eq!(
                    (r, g, b),
                    (expected, expected, expected),
                    "channel pair ({o}, {a}): the lean is not three quarters of the way from the parents' midpoint toward opencode"
                );
            }
        }
    }

    #[test]
    fn the_opencode_canvas_is_not_the_registry_darkest() {
        // The corrected provenance doc drops opencode's "darkest
        // canvas" claim; this pins the fact behind that correction —
        // autumn_night's canvas is darker, so any future doc that
        // reinstates the superlative contradicts a pin.
        let Color::Rgb(or_, og, ob) = OPENCODE.ui.background else {
            panic!("the opencode canvas is Rgb");
        };
        let Color::Rgb(ar, ag, ab) = AUTUMN_NIGHT.ui.background else {
            panic!("the autumn_night canvas is Rgb");
        };
        assert!(
            (ar, ag, ab) < (or_, og, ob),
            "autumn_night ({ar},{ag},{ab}) must sit darker than opencode ({or_},{og},{ob})"
        );
    }

    #[test]
    fn the_opencode_secondary_is_achromatic() {
        // The corrected provenance doc drops "violet secondary";
        // the secondary is an achromatic gray. This pins that fact —
        // equal channels — so the dropped claim cannot quietly
        // become true unremarked.
        let Color::Rgb(r, g, b) = OPENCODE.ui.secondary else {
            panic!("the opencode secondary is Rgb");
        };
        assert_eq!(
            (r, g, b),
            (r, r, r),
            "the opencode secondary is achromatic gray, not violet ({r},{g},{b})"
        );
    }

    /// One spec's syntax palette, in capture order.
    ///
    /// The same order [`SYNTAX_FIELD_NAMES`] names, so the midpoint
    /// pin's failure messages line up.
    fn syntax_of(spec: &ThemeSpec) -> [Color; 11] {
        [
            spec.syntax.attribute,
            spec.syntax.comment,
            spec.syntax.constant,
            spec.syntax.embedded,
            spec.syntax.number,
            spec.syntax.operator,
            spec.syntax.property,
            spec.syntax.string,
            spec.syntax.variable_builtin,
            spec.syntax.tag,
            spec.syntax.escape,
        ]
    }

    /// One spec's markdown color fields, element order.
    ///
    /// The same order [`MARKDOWN_FIELD_NAMES`] names, for the
    /// midpoint pin's failure messages.
    fn markdown_of(spec: &ThemeSpec) -> [Color; 6] {
        [
            spec.markdown.code_inline,
            spec.markdown.code_block,
            spec.markdown.link,
            spec.markdown.quote,
            spec.markdown.list_item,
            spec.markdown.horizontal_rule,
        ]
    }

    /// One spec's chrome color fields, surface order.
    ///
    /// The same order [`UI_FIELD_NAMES`] names, for the midpoint
    /// pin's failure messages; the primary accent and the foreground
    /// family are not midpoint fields — the chrome pin asserts them
    /// separately.
    fn ui_of(spec: &ThemeSpec) -> [Color; 14] {
        [
            spec.ui.background,
            spec.ui.surface,
            spec.ui.secondary,
            spec.ui.dim,
            spec.ui.border_color,
            spec.ui.focused_border_color,
            spec.ui.input_border,
            spec.ui.status_bar_bg,
            spec.ui.status_bar_fg,
            spec.ui.status_success,
            spec.ui.status_warning,
            spec.ui.status_error,
            spec.ui.scrollbar_thumb,
            spec.ui.scrollbar_track,
        ]
    }

    #[test]
    fn the_dch_syntax_leans_toward_opencode() {
        // The DCH doc promises the code palette at seven parts
        // opencode, one part autumn (three quarters of the way
        // from the parents' midpoint) — this pin
        // walks the construction itself, not just the resulting
        // fingerprint: any field that drifts off the blend, in
        // either parent or in the house theme, fails with the field
        // named. Punctuation and delimiter are the exceptions: both
        // parents pin them to their foregrounds, so the house theme
        // keeps them on the foreground family — the autumn-leaning
        // cream — and they keep deferring with it on unsynced
        // terminals.
        let warm_cream = Color::Rgb(244, 242, 209);
        assert_eq!(
            DCH.syntax.punctuation, warm_cream,
            "code punctuation follows the foreground family"
        );
        assert_eq!(DCH.syntax.delimiter, warm_cream);
        // Function arguments read in autumn_night's own register:
        // the parents' midpoint blends autumn's cream with
        // opencode's brick red, which reads as blood in code — the
        // house takes autumn's spelling instead.
        assert_eq!(
            DCH.syntax.variable, AUTUMN_NIGHT.syntax.variable,
            "arguments (the variable register) use autumn_night's color"
        );
        // Keywords carry the house orange — the same accent as the
        // caret and focused edges — so the highest-frequency token
        // class speaks the house color in code as everywhere else.
        assert_eq!(
            DCH.syntax.keyword, DCH.ui.primary,
            "keywords carry the house orange, the primary accent's own color"
        );
        // Names and types read in autumn_night's gold, blended a
        // tenth of the way toward white — the same warm gold it
        // gives its own call sites, lifted just enough to read
        // lighter on the house canvas. The gold is yellow enough
        // to stand apart from the orange keywords while staying in
        // the house's warm family.
        let gold = Color::Rgb(251, 217, 117);
        assert_eq!(
            DCH.syntax.function, gold,
            "function names read in the lightened autumn gold"
        );
        assert_eq!(DCH.syntax.constructor, gold);
        assert_eq!(
            DCH.syntax.r#type, gold,
            "type names (argument types included) read in the gold"
        );
        for (((d, o), a), field) in syntax_of(&DCH)
            .iter()
            .zip(syntax_of(&OPENCODE))
            .zip(syntax_of(&AUTUMN_NIGHT))
            .zip(SYNTAX_FIELD_NAMES)
        {
            assert_eq!(
                *d,
                toward_opencode(o, a),
                "syntax field {field} is not the opencode-leaning blend"
            );
        }
    }

    #[test]
    fn the_dch_markdown_is_the_parents_channel_midpoint() {
        // The heading ramp is the one deliberate off-midpoint
        // markdown surface: every level carries the lighter house
        // orange — the primary accent blended a fifth toward white,
        // so titles read in the house color at every depth without
        // the accent's full heat. The parents' grey-midpoint ramp
        // is replaced wholesale.
        for (level, header) in DCH.markdown.header.iter().enumerate() {
            assert_eq!(
                *header,
                Color::Rgb(255, 193, 51),
                "header level {} is the lighter house orange (#FFC133)",
                level.saturating_add(1)
            );
        }
        for (((d, o), a), field) in markdown_of(&DCH)
            .iter()
            .zip(markdown_of(&OPENCODE))
            .zip(markdown_of(&AUTUMN_NIGHT))
            .zip(MARKDOWN_FIELD_NAMES)
        {
            assert_eq!(
                *d,
                midpoint(o, a),
                "markdown field {field} is not the parents' midpoint"
            );
        }
    }

    #[test]
    fn the_dch_chrome_is_the_parents_channel_midpoint() {
        // The primary accent and the foreground family are the
        // deliberate off-midpoint chrome colors. The accent is the
        // house orange, chosen over the parents' peach so the house
        // accent is the house's own; the foreground family — body
        // text, both message registers, and the composer's draft —
        // is a warm cream of its own (`#F4F2D1`), a shade off
        // autumn_night's foreground — similar to autumn, not
        // autumn. All three deferring registers carry it equally so
        // the deferral's anchor equality holds and unsynced-terminal
        // text defers as one.
        assert_eq!(
            DCH.ui.primary,
            Color::Rgb(255, 178, 0),
            "the primary accent is the house orange (#FFB200)"
        );
        let warm_cream = Color::Rgb(244, 242, 209);
        assert_eq!(
            DCH.ui.foreground, warm_cream,
            "body text leans toward autumn_night's cream without matching it"
        );
        // And the prose's claim is exact: the cream is a shade off
        // autumn_night's own foreground — every channel within six,
        // and the triple as a whole not autumn's (it is its own
        // color; individual channels may coincide).
        if let (Color::Rgb(dr, dg, db), Color::Rgb(ar, ag, ab)) =
            (warm_cream, AUTUMN_NIGHT.ui.foreground)
        {
            assert!(
                (dr, dg, db) != (ar, ag, ab),
                "the cream is its own color, not autumn's foreground"
            );
            for (house, autumn, channel) in [(dr, ar, "red"), (dg, ag, "green"), (db, ab, "blue")] {
                let distance = house.abs_diff(autumn);
                assert!(
                    distance <= 6,
                    "channel {channel} sits {distance} off autumn's foreground — not the promised shade"
                );
            }
        }
        assert_eq!(DCH.ui.user_message_fg, warm_cream);
        assert_eq!(DCH.ui.assistant_message_fg, warm_cream);
        assert_eq!(DCH.ui.input_text, warm_cream);
        for (((d, o), a), field) in ui_of(&DCH)
            .iter()
            .zip(ui_of(&OPENCODE))
            .zip(ui_of(&AUTUMN_NIGHT))
            .zip(UI_FIELD_NAMES)
        {
            assert_eq!(
                *d,
                midpoint(o, a),
                "ui field {field} is not the parents' midpoint"
            );
        }
        // The one non-color chrome field: both parents leave it
        // unset, so the midpoint must too.
        assert_eq!(
            DCH.ui.composer_border, AUTUMN_NIGHT.ui.composer_border,
            "the parents agree on composer_border, so the midpoint must too"
        );
        assert_eq!(
            OPENCODE.ui.composer_border, DCH.ui.composer_border,
            "the parents agree on composer_border, so the midpoint must too"
        );
    }

    /// The syntax capture names, for the midpoint pin's failure
    /// messages.
    const SYNTAX_FIELD_NAMES: [&str; 11] = [
        "attribute",
        "comment",
        "constant",
        "embedded",
        "number",
        "operator",
        "property",
        "string",
        "variable_builtin",
        "tag",
        "escape",
    ];

    /// The markdown color-field names, for the midpoint pin's failure
    /// messages.
    const MARKDOWN_FIELD_NAMES: [&str; 6] = [
        "code_inline",
        "code_block",
        "link",
        "quote",
        "list_item",
        "horizontal_rule",
    ];

    /// The chrome color-field names, for the midpoint pin's failure
    /// messages — the primary accent and the foreground family
    /// excepted, which the chrome pin holds to the house orange and
    /// the autumn-leaning cream instead of the midpoint.
    const UI_FIELD_NAMES: [&str; 14] = [
        "background",
        "surface",
        "secondary",
        "dim",
        "border_color",
        "focused_border_color",
        "input_border",
        "status_bar_bg",
        "status_bar_fg",
        "status_success",
        "status_warning",
        "status_error",
        "scrollbar_thumb",
        "scrollbar_track",
    ];
}
