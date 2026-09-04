//! The named theme constructors: plain data, one function per theme.
//!
//! Each constructor is a complete palette — every syntax capture, markdown
//! element, and chrome surface gets an explicit color, so a theme renders
//! identically regardless of defaults. Palette entries without a distinct
//! identity follow the sibling the palette assigns them (attributes follow
//! functions, punctuation follows comments, and so on). The lookup table at
//! the bottom is the single registry [`Theme::by_name`] resolves through; a
//! theme missing from it is unreachable from configuration.

use ratatui::style::{Color, Modifier, Style};

use super::{MarkdownTheme, SyntaxTheme, Theme, UIStyle};

/// A bold style for a heading: `color` at bold weight.
fn heading(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// An underlined style for a link.
fn link(color: Color) -> Style {
    Style::default()
        .fg(color)
        .add_modifier(Modifier::UNDERLINED)
}

/// An italic style for quoted material.
fn quote(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::ITALIC)
}

/// Inline code: an accent foreground on a tinted background.
fn inline_code(fg: Color, bg: Color) -> Style {
    Style::default().fg(fg).bg(bg)
}

/// A plain foreground for an element without modifiers.
fn plain(color: Color) -> Style {
    Style::default().fg(color)
}

impl Theme {
    /// The default dch theme.
    ///
    /// Dark violet surfaces with pink, purple, and cyan accents at high
    /// contrast — readable at small sizes, and the fallback for unknown
    /// theme names.
    #[must_use]
    pub(crate) fn dracula() -> Self {
        Self {
            name: "Dracula",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(86, 182, 194)),
                header2: heading(Color::Rgb(189, 147, 249)),
                header3: heading(Color::Rgb(255, 184, 108)),
                header4: heading(Color::Rgb(241, 250, 140)),
                header5: heading(Color::Rgb(139, 233, 253)),
                header6: heading(Color::Rgb(110, 210, 220)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(255, 184, 108), Color::Rgb(68, 71, 90)),
                code_block: plain(Color::Rgb(248, 248, 242)),
                link: link(Color::Rgb(98, 114, 164)),
                quote: quote(Color::Rgb(98, 114, 164)),
                list_item: plain(Color::Rgb(248, 248, 242)),
                horizontal_rule: plain(Color::Rgb(98, 114, 164)),
            },
            ui: UIStyle {
                background: Color::Rgb(40, 42, 54),
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
                status_success: Color::Rgb(110, 210, 220),
                status_warning: Color::Rgb(241, 250, 140),
                status_error: Color::Rgb(255, 85, 85),
                scrollbar_thumb: Color::Rgb(98, 114, 164),
                scrollbar_track: Color::Rgb(68, 71, 90),
            },
        }
    }

    /// The Nord theme.
    ///
    /// Muted blue-gray arctic palette with frost-blue accents; deliberately
    /// low-saturation, built for long sessions rather than punch.
    #[must_use]
    pub(crate) fn nord() -> Self {
        Self {
            name: "Nord",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(136, 192, 208)),
                header2: heading(Color::Rgb(143, 188, 187)),
                header3: heading(Color::Rgb(129, 161, 193)),
                header4: heading(Color::Rgb(94, 129, 172)),
                header5: heading(Color::Rgb(163, 190, 140)),
                header6: heading(Color::Rgb(208, 135, 112)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(235, 203, 139), Color::Rgb(59, 66, 82)),
                code_block: plain(Color::Rgb(236, 239, 244)),
                link: link(Color::Rgb(163, 190, 140)),
                quote: quote(Color::Rgb(94, 129, 172)),
                list_item: plain(Color::Rgb(236, 239, 244)),
                horizontal_rule: plain(Color::Rgb(94, 129, 172)),
            },
            ui: UIStyle {
                background: Color::Rgb(46, 52, 64),
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
                status_success: Color::Rgb(163, 190, 140),
                status_warning: Color::Rgb(235, 203, 139),
                status_error: Color::Rgb(191, 97, 106),
                scrollbar_thumb: Color::Rgb(94, 129, 172),
                scrollbar_track: Color::Rgb(59, 66, 82),
            },
        }
    }

    /// The Tokyo Night theme.
    ///
    /// Dark indigo surfaces with neon pink, blue, and cyan accents; a cool,
    /// high-contrast cast for night work.
    #[must_use]
    pub(crate) fn tokyo_night() -> Self {
        Self {
            name: "Tokyo Night",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(224, 108, 117)),
                header2: heading(Color::Rgb(122, 162, 247)),
                header3: heading(Color::Rgb(187, 154, 247)),
                header4: heading(Color::Rgb(235, 137, 88)),
                header5: heading(Color::Rgb(151, 206, 238)),
                header6: heading(Color::Rgb(169, 177, 214)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(235, 137, 88), Color::Rgb(42, 44, 60)),
                code_block: plain(Color::Rgb(200, 208, 255)),
                link: link(Color::Rgb(151, 206, 238)),
                quote: quote(Color::Rgb(92, 106, 152)),
                list_item: plain(Color::Rgb(200, 208, 255)),
                horizontal_rule: plain(Color::Rgb(92, 106, 152)),
            },
            ui: UIStyle {
                background: Color::Rgb(26, 27, 38),
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
                status_success: Color::Rgb(169, 177, 214),
                status_warning: Color::Rgb(235, 137, 88),
                status_error: Color::Rgb(224, 108, 117),
                scrollbar_thumb: Color::Rgb(92, 106, 152),
                scrollbar_track: Color::Rgb(42, 44, 60),
            },
        }
    }

    /// The Gruvbox Dark theme.
    ///
    /// Warm retro earth tones on a dark brown-gray base; orange and yellow
    /// accents keep long code sessions gentle on the eyes.
    #[must_use]
    pub(crate) fn gruvbox_dark() -> Self {
        Self {
            name: "Gruvbox Dark",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(69, 133, 136)),
                header2: heading(Color::Rgb(177, 98, 134)),
                header3: heading(Color::Rgb(215, 153, 33)),
                header4: heading(Color::Rgb(152, 151, 26)),
                header5: heading(Color::Rgb(104, 157, 106)),
                header6: heading(Color::Rgb(235, 219, 178)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(215, 153, 33), Color::Rgb(60, 56, 54)),
                code_block: plain(Color::Rgb(235, 219, 178)),
                link: link(Color::Rgb(104, 157, 106)),
                quote: quote(Color::Rgb(146, 131, 116)),
                list_item: plain(Color::Rgb(235, 219, 178)),
                horizontal_rule: plain(Color::Rgb(146, 131, 116)),
            },
            ui: UIStyle {
                background: Color::Rgb(40, 40, 40),
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
                status_success: Color::Rgb(152, 151, 26),
                status_warning: Color::Rgb(215, 153, 33),
                status_error: Color::Rgb(204, 36, 29),
                scrollbar_thumb: Color::Rgb(146, 131, 116),
                scrollbar_track: Color::Rgb(60, 56, 54),
            },
        }
    }

    /// The Gruvbox Light theme.
    ///
    /// The same warm hues on paper-white; suited to bright rooms and
    /// print-like reading.
    #[must_use]
    pub(crate) fn gruvbox_light() -> Self {
        Self {
            name: "Gruvbox Light",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(7, 102, 120)),
                header2: heading(Color::Rgb(143, 63, 113)),
                header3: heading(Color::Rgb(181, 118, 0)),
                header4: heading(Color::Rgb(121, 116, 14)),
                header5: heading(Color::Rgb(66, 123, 88)),
                header6: heading(Color::Rgb(60, 56, 54)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(175, 58, 3), Color::Rgb(235, 219, 178)),
                code_block: plain(Color::Rgb(60, 56, 54)),
                link: link(Color::Rgb(66, 123, 88)),
                quote: quote(Color::Rgb(146, 131, 116)),
                list_item: plain(Color::Rgb(60, 56, 54)),
                horizontal_rule: plain(Color::Rgb(146, 131, 116)),
            },
            ui: UIStyle {
                background: Color::Rgb(251, 241, 199),
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
                status_success: Color::Rgb(152, 151, 26),
                status_warning: Color::Rgb(215, 153, 33),
                status_error: Color::Rgb(204, 36, 29),
                scrollbar_thumb: Color::Rgb(146, 131, 116),
                scrollbar_track: Color::Rgb(235, 219, 178),
            },
        }
    }

    /// The Solarized Dark theme.
    ///
    /// The classic precision palette on a deep blue-green base; a
    /// deliberately limited hue range with carefully tuned contrast.
    #[must_use]
    pub(crate) fn solarized_dark() -> Self {
        Self {
            name: "Solarized Dark",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(38, 139, 210)),
                header2: heading(Color::Rgb(211, 54, 130)),
                header3: heading(Color::Rgb(181, 137, 0)),
                header4: heading(Color::Rgb(133, 153, 0)),
                header5: heading(Color::Rgb(42, 161, 152)),
                header6: heading(Color::Rgb(253, 246, 227)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(181, 137, 0), Color::Rgb(0, 43, 54)),
                code_block: plain(Color::Rgb(131, 148, 150)),
                link: link(Color::Rgb(42, 161, 152)),
                quote: quote(Color::Rgb(45, 79, 87)),
                list_item: plain(Color::Rgb(131, 148, 150)),
                horizontal_rule: plain(Color::Rgb(45, 79, 87)),
            },
            ui: UIStyle {
                background: Color::Rgb(0, 43, 54),
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
                status_success: Color::Rgb(133, 153, 0),
                status_warning: Color::Rgb(181, 137, 0),
                status_error: Color::Rgb(220, 50, 47),
                scrollbar_thumb: Color::Rgb(45, 79, 87),
                scrollbar_track: Color::Rgb(7, 54, 66),
            },
        }
    }

    /// The Solarized Light theme.
    ///
    /// The Solarized precision palette on a warm cream base; the same hue
    /// discipline as its dark sibling, in daylight.
    #[must_use]
    pub(crate) fn solarized_light() -> Self {
        Self {
            name: "Solarized Light",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(38, 139, 210)),
                header2: heading(Color::Rgb(211, 54, 130)),
                header3: heading(Color::Rgb(181, 137, 0)),
                header4: heading(Color::Rgb(133, 153, 0)),
                header5: heading(Color::Rgb(42, 161, 152)),
                header6: heading(Color::Rgb(88, 110, 117)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(181, 137, 0), Color::Rgb(0, 43, 54)),
                code_block: plain(Color::Rgb(88, 110, 117)),
                link: link(Color::Rgb(42, 161, 152)),
                quote: quote(Color::Rgb(0, 43, 54)),
                list_item: plain(Color::Rgb(88, 110, 117)),
                horizontal_rule: plain(Color::Rgb(0, 43, 54)),
            },
            ui: UIStyle {
                background: Color::Rgb(253, 246, 227),
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
                status_success: Color::Rgb(133, 153, 0),
                status_warning: Color::Rgb(181, 137, 0),
                status_error: Color::Rgb(220, 50, 47),
                scrollbar_thumb: Color::Rgb(0, 43, 54),
                scrollbar_track: Color::Rgb(238, 232, 213),
            },
        }
    }

    /// The Catppuccin Latte theme.
    ///
    /// The light flavor of the Catppuccin family: pastel accents on warm
    /// white, soothing rather than stark.
    #[must_use]
    pub(crate) fn catppuccin_latte() -> Self {
        Self {
            name: "Catppuccin Latte",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(30, 102, 245)),
                header2: heading(Color::Rgb(234, 118, 203)),
                header3: heading(Color::Rgb(223, 142, 29)),
                header4: heading(Color::Rgb(64, 160, 43)),
                header5: heading(Color::Rgb(23, 146, 153)),
                header6: heading(Color::Rgb(188, 192, 204)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(76, 79, 105), Color::Rgb(220, 224, 232)),
                code_block: plain(Color::Rgb(76, 79, 105)),
                link: link(Color::Rgb(23, 146, 153)),
                quote: quote(Color::Rgb(108, 111, 133)),
                list_item: plain(Color::Rgb(76, 79, 105)),
                horizontal_rule: plain(Color::Rgb(108, 111, 133)),
            },
            ui: UIStyle {
                background: Color::Rgb(239, 241, 245),
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
                status_success: Color::Rgb(64, 160, 43),
                status_warning: Color::Rgb(223, 142, 29),
                status_error: Color::Rgb(210, 15, 57),
                scrollbar_thumb: Color::Rgb(108, 111, 133),
                scrollbar_track: Color::Rgb(220, 224, 228),
            },
        }
    }

    /// The Catppuccin Frappe theme.
    ///
    /// Pastel accents on a medium-dark base — the lightest of the family's
    /// dark flavors.
    #[must_use]
    pub(crate) fn catppuccin_frappe() -> Self {
        Self {
            name: "Catppuccin Frappe",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(140, 170, 238)),
                header2: heading(Color::Rgb(244, 184, 228)),
                header3: heading(Color::Rgb(229, 200, 144)),
                header4: heading(Color::Rgb(166, 209, 137)),
                header5: heading(Color::Rgb(129, 200, 190)),
                header6: heading(Color::Rgb(165, 173, 206)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(229, 200, 144), Color::Rgb(98, 104, 128)),
                code_block: plain(Color::Rgb(198, 208, 245)),
                link: link(Color::Rgb(129, 200, 190)),
                quote: quote(Color::Rgb(98, 104, 128)),
                list_item: plain(Color::Rgb(198, 208, 245)),
                horizontal_rule: plain(Color::Rgb(98, 104, 128)),
            },
            ui: UIStyle {
                background: Color::Rgb(48, 52, 70),
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
                status_success: Color::Rgb(166, 209, 137),
                status_warning: Color::Rgb(229, 200, 144),
                status_error: Color::Rgb(231, 130, 132),
                scrollbar_thumb: Color::Rgb(98, 104, 128),
                scrollbar_track: Color::Rgb(64, 69, 89),
            },
        }
    }

    /// The Catppuccin Macchiato theme.
    ///
    /// Pastel accents a step darker than Frappe; a middle stop for those
    /// who find the darker flavors too deep.
    #[must_use]
    pub(crate) fn catppuccin_macchiato() -> Self {
        Self {
            name: "Catppuccin Macchiato",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(138, 173, 244)),
                header2: heading(Color::Rgb(245, 189, 230)),
                header3: heading(Color::Rgb(238, 212, 159)),
                header4: heading(Color::Rgb(166, 218, 149)),
                header5: heading(Color::Rgb(139, 213, 202)),
                header6: heading(Color::Rgb(165, 173, 203)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(238, 212, 159), Color::Rgb(91, 96, 120)),
                code_block: plain(Color::Rgb(202, 211, 245)),
                link: link(Color::Rgb(139, 213, 202)),
                quote: quote(Color::Rgb(91, 96, 120)),
                list_item: plain(Color::Rgb(202, 211, 245)),
                horizontal_rule: plain(Color::Rgb(91, 96, 120)),
            },
            ui: UIStyle {
                background: Color::Rgb(36, 39, 58),
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
                status_success: Color::Rgb(166, 218, 149),
                status_warning: Color::Rgb(238, 212, 159),
                status_error: Color::Rgb(237, 135, 150),
                scrollbar_thumb: Color::Rgb(91, 96, 120),
                scrollbar_track: Color::Rgb(54, 58, 79),
            },
        }
    }

    /// The Catppuccin Mocha theme.
    ///
    /// The darkest Catppuccin flavor: pastel accents on deep cocoa
    /// surfaces, gentle on the eyes without going pure black.
    #[must_use]
    pub(crate) fn catppuccin_mocha() -> Self {
        Self {
            name: "Catppuccin Mocha",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(137, 180, 250)),
                header2: heading(Color::Rgb(245, 194, 231)),
                header3: heading(Color::Rgb(249, 226, 175)),
                header4: heading(Color::Rgb(166, 227, 161)),
                header5: heading(Color::Rgb(148, 226, 213)),
                header6: heading(Color::Rgb(166, 173, 200)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(249, 226, 175), Color::Rgb(88, 91, 112)),
                code_block: plain(Color::Rgb(205, 214, 244)),
                link: link(Color::Rgb(148, 226, 213)),
                quote: quote(Color::Rgb(88, 91, 112)),
                list_item: plain(Color::Rgb(205, 214, 244)),
                horizontal_rule: plain(Color::Rgb(88, 91, 112)),
            },
            ui: UIStyle {
                background: Color::Rgb(30, 30, 46),
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
                status_success: Color::Rgb(166, 227, 161),
                status_warning: Color::Rgb(249, 226, 175),
                status_error: Color::Rgb(243, 139, 168),
                scrollbar_thumb: Color::Rgb(88, 91, 112),
                scrollbar_track: Color::Rgb(49, 50, 68),
            },
        }
    }

    /// The One Dark theme.
    ///
    /// Atom's classic dark palette: cool grays with blue, green, and coral
    /// accents, tuned for editor readability.
    #[must_use]
    pub(crate) fn one_dark() -> Self {
        Self {
            name: "One Dark",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(97, 175, 239)),
                header2: heading(Color::Rgb(198, 120, 221)),
                header3: heading(Color::Rgb(209, 154, 102)),
                header4: heading(Color::Rgb(152, 195, 121)),
                header5: heading(Color::Rgb(171, 178, 191)),
                header6: heading(Color::Rgb(255, 255, 255)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(209, 154, 102), Color::Rgb(52, 57, 66)),
                code_block: plain(Color::Rgb(171, 178, 191)),
                link: link(Color::Rgb(97, 175, 239)),
                quote: quote(Color::Rgb(92, 99, 112)),
                list_item: plain(Color::Rgb(171, 178, 191)),
                horizontal_rule: plain(Color::Rgb(92, 99, 112)),
            },
            ui: UIStyle {
                background: Color::Rgb(40, 44, 52),
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
                status_success: Color::Rgb(152, 195, 121),
                status_warning: Color::Rgb(209, 154, 102),
                status_error: Color::Rgb(224, 108, 117),
                scrollbar_thumb: Color::Rgb(92, 99, 112),
                scrollbar_track: Color::Rgb(52, 57, 66),
            },
        }
    }

    /// The Monokai theme.
    ///
    /// The vintage high-contrast dark palette: yellow, green, and magenta on
    /// near-black, for those who like their syntax loud.
    #[must_use]
    pub(crate) fn monokai() -> Self {
        Self {
            name: "Monokai",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(102, 217, 239)),
                header2: heading(Color::Rgb(174, 129, 255)),
                header3: heading(Color::Rgb(244, 191, 117)),
                header4: heading(Color::Rgb(166, 226, 46)),
                header5: heading(Color::Rgb(161, 239, 228)),
                header6: heading(Color::Rgb(249, 248, 245)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(244, 191, 117), Color::Rgb(62, 61, 50)),
                code_block: plain(Color::Rgb(248, 248, 242)),
                link: link(Color::Rgb(161, 239, 228)),
                quote: quote(Color::Rgb(117, 113, 94)),
                list_item: plain(Color::Rgb(248, 248, 242)),
                horizontal_rule: plain(Color::Rgb(117, 113, 94)),
            },
            ui: UIStyle {
                background: Color::Rgb(39, 40, 34),
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
                status_success: Color::Rgb(166, 226, 46),
                status_warning: Color::Rgb(244, 191, 117),
                status_error: Color::Rgb(249, 38, 114),
                scrollbar_thumb: Color::Rgb(117, 113, 94),
                scrollbar_track: Color::Rgb(62, 61, 50),
            },
        }
    }

    /// The `GitHub` Dark theme.
    ///
    /// `GitHub`'s own dark-mode code palette: restrained blues and greens on
    /// near-black, optimized for reading diffs and prose alike.
    #[must_use]
    pub(crate) fn github_dark() -> Self {
        Self {
            name: "GitHub Dark",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(33, 136, 255)),
                header2: heading(Color::Rgb(179, 146, 240)),
                header3: heading(Color::Rgb(255, 234, 127)),
                header4: heading(Color::Rgb(52, 208, 88)),
                header5: heading(Color::Rgb(57, 197, 207)),
                header6: heading(Color::Rgb(250, 251, 252)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(255, 234, 127), Color::Rgb(45, 51, 59)),
                code_block: plain(Color::Rgb(209, 213, 218)),
                link: link(Color::Rgb(57, 197, 207)),
                quote: quote(Color::Rgb(149, 157, 165)),
                list_item: plain(Color::Rgb(209, 213, 218)),
                horizontal_rule: plain(Color::Rgb(149, 157, 165)),
            },
            ui: UIStyle {
                background: Color::Rgb(36, 41, 46),
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
                status_success: Color::Rgb(63, 185, 80),
                status_warning: Color::Rgb(187, 128, 9),
                status_error: Color::Rgb(248, 81, 73),
                scrollbar_thumb: Color::Rgb(149, 157, 165),
                scrollbar_track: Color::Rgb(45, 51, 59),
            },
        }
    }

    /// The `GitHub` Light theme.
    ///
    /// `GitHub`'s light-mode palette on white; the most conservative light
    /// theme in the set.
    #[must_use]
    pub(crate) fn github_light() -> Self {
        Self {
            name: "GitHub Light",
            syntax: SyntaxTheme {
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
            markdown: MarkdownTheme {
                header1: heading(Color::Rgb(3, 102, 214)),
                header2: heading(Color::Rgb(90, 50, 163)),
                header3: heading(Color::Rgb(219, 171, 9)),
                header4: heading(Color::Rgb(40, 167, 69)),
                header5: heading(Color::Rgb(5, 152, 188)),
                header6: heading(Color::Rgb(36, 41, 47)),
                bold: Style::default().add_modifier(Modifier::BOLD),
                italic: Style::default().add_modifier(Modifier::ITALIC),
                code_inline: inline_code(Color::Rgb(36, 41, 47), Color::Rgb(225, 228, 232)),
                code_block: plain(Color::Rgb(36, 41, 47)),
                link: link(Color::Rgb(5, 152, 188)),
                quote: quote(Color::Rgb(149, 157, 165)),
                list_item: plain(Color::Rgb(36, 41, 47)),
                horizontal_rule: plain(Color::Rgb(149, 157, 165)),
            },
            ui: UIStyle {
                background: Color::Rgb(255, 255, 255),
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
                status_success: Color::Rgb(40, 167, 69),
                status_warning: Color::Rgb(219, 171, 9),
                status_error: Color::Rgb(215, 58, 73),
                scrollbar_thumb: Color::Rgb(149, 157, 165),
                scrollbar_track: Color::Rgb(225, 228, 232),
            },
        }
    }
}

/// A theme constructor as stored in the lookup table.
///
/// Plain function pointers keep the registry const-constructible; each entry
/// pairs a lookup key with the function that builds its [`Theme`].
type ThemeConstructor = fn() -> Theme;

/// The named themes: lookup key → constructor, in registry order.
///
/// `"default"` aliases [`Theme::default`] so config authors can name the
/// fallback explicitly. Adding a theme means adding the constructor and
/// one row here: `by_name` resolves only through this table, so a
/// constructor without a row is unreachable from configuration. The tests
/// pin that every row resolves and every key is distinct.
pub(crate) const THEME_CONSTRUCTORS: &[(&str, ThemeConstructor)] = &[
    ("default", Theme::default),
    ("dracula", Theme::dracula),
    ("nord", Theme::nord),
    ("tokyo_night", Theme::tokyo_night),
    ("gruvbox_dark", Theme::gruvbox_dark),
    ("gruvbox_light", Theme::gruvbox_light),
    ("solarized_dark", Theme::solarized_dark),
    ("solarized_light", Theme::solarized_light),
    ("catppuccin_latte", Theme::catppuccin_latte),
    ("catppuccin_frappe", Theme::catppuccin_frappe),
    ("catppuccin_macchiato", Theme::catppuccin_macchiato),
    ("catppuccin_mocha", Theme::catppuccin_mocha),
    ("one_dark", Theme::one_dark),
    ("monokai", Theme::monokai),
    ("github_dark", Theme::github_dark),
    ("github_light", Theme::github_light),
];
