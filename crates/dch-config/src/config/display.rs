//! The `[display]` section: the rendering preferences and the
//! vocabularies they choose among.
//!
//! [`DisplayConfig`] is the table; each knob's spelling — strip
//! placement and motion, path style, splash colours, caret beat, diff
//! layout and palette — is a serde-named enum in this module.

/// Console output verbosity.
///
/// Controls how chatty the display layer is at runtime. It feeds into
/// [`DisplayConfig::verbosity`] and is (de)serialized as its lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    /// Errors only.
    ///
    /// Suppresses informational output so the console shows failures and nothing
    /// else.
    Quiet,

    /// Default informational output.
    ///
    /// The standard level a user sees without opting into extra detail.
    #[default]
    Normal,

    /// Debug-level detail.
    ///
    /// Adds lower-level diagnostic output on top of the normal level.
    Verbose,
}

/// Where the TUI's activity strip renders while the agent runs.
///
/// Selects the breathing bar's row: the spacer directly above the
/// composer, or a dedicated first row of the terminal. Consumed from
/// [`DisplayConfig::strip_position`] and (de)serialized as its
/// lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StripPosition {
    /// The spacer row directly above the composer.
    ///
    /// The default placement: the bar claims the row the composer's
    /// hairline would otherwise paint, and stands the rule down while
    /// it breathes.
    #[default]
    Composer,

    /// A dedicated first row of the terminal.
    ///
    /// The bar claims screen row 0, full width, above the conversation
    /// pane — which shrinks by one row — and the composer's spacer row
    /// keeps its hairline.
    Top,
}

/// How the TUI spells the session's working directory in the place
/// row above the composer.
///
/// Selects the path's shape. Consumed from
/// [`DisplayConfig::path_style`] and (de)serialized as its lowercase
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathStyle {
    /// The absolute path, exactly as the host named it.
    ///
    /// The default: no information lost, and the row reads the same
    /// wherever the session roams.
    #[default]
    Full,

    /// The path relative to the user's home directory, `~/`-spelled.
    ///
    /// `~/repos/dch` for a session under home — and the full path
    /// when the directory sits outside home or the environment names
    /// no home, so the row never shows a bare unanchored fragment.
    Relative,
}

/// Which color vocabulary the welcome splash paints in.
///
/// Selects the splash's rendition. Consumed from
/// [`DisplayConfig::splash_colors`] and (de)serialized as its
/// lowercase name — `"auto"`, `"truecolor"`, or `"palette"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SplashColors {
    /// The original art everywhere; the palette rendition only on
    /// request.
    ///
    /// The default: a multiplexer forwards direct colors verbatim to
    /// every client with the RGB feature, and a client without it keeps
    /// the hues in its own approximation — so the photograph is painted
    /// as authored wherever the session sits. The palette rendition is
    /// the explicit `palette` choice, for a tmux whose clients it knows
    /// as 256-color and re-encodes per color onto the grey ramp.
    #[default]
    Auto,

    /// The original truecolor art, everywhere and unconditionally.
    ///
    /// For the session that knows its own terminal best: no detection,
    /// no fallback.
    Truecolor,

    /// The fixed 256-color palette rendition, everywhere.
    ///
    /// The same photograph authored for the palette every 256-color
    /// terminal paints identically — the setting for non-truecolor
    /// environments.
    Palette,
}

/// How the composer's caret presents itself.
///
/// Selects the caret's beat. Consumed from
/// [`DisplayConfig::caret_display`] and (de)serialized as its
/// lowercase name — `"static"` or `"blink"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaretDisplay {
    /// A solid caret, on in every frame.
    ///
    /// The blink's dark phase never darkens it: some eyes and some
    /// terminals prefer the still marker.
    Static,

    /// The blinking caret, on a clock-steady beat.
    ///
    /// The default: the 530 ms on/off cadence, resolidified by
    /// keyboard input.
    #[default]
    Blink,
}

/// The animation the TUI's activity strip plays while the agent runs.
///
/// Selects the breathing bar's motion. Consumed from
/// [`DisplayConfig::strip_style`] and (de)serialized as its lowercase
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StripStyle {
    /// Grows from the row's center out to both edges, then shrinks
    /// back.
    ///
    /// The default motion: a triangle breath that never fully
    /// disappears — its run keeps a one-cell floor.
    #[default]
    Breathe,

    /// Fills from the left edge to the right end, then clears
    /// left-to-right.
    ///
    /// The right edge stays pinned while the left edge chases it off,
    /// the row blanking for an instant before the fill restarts from
    /// a one-cell seed.
    Sweep,

    /// Sweeps each way in turn — fill and clear left-to-right, then
    /// fill and clear right-to-left, repeating.
    ///
    /// The sweep's motion ping-ponging between the edges: each
    /// direction pass fills from its edge and clears toward the
    /// other, the row blanking for an instant at every turn.
    Bounce,

    /// Glides a short segment from edge to edge and back, repeating.
    ///
    /// The scanner's segment spans a quarter of the row (floored at
    /// three cells) and travels continuously — one pass each way per
    /// cycle, the motion never pausing or blanking.
    Travel,

    /// Circulates a short segment out the right edge and in the left,
    /// one continuous lap after another.
    ///
    /// Travel's segment without the bounce: a quarter-row run
    /// (floored at three cells) advances around the row, its head
    /// entering the left edge while its tail still exits the right —
    /// the row never blanking through the wrap.
    Snake,

    /// Crawls across the row in stretch-and-shrink beats, absorbed at
    /// the far edge before reappearing at the left.
    ///
    /// The inchworm: a seed stretches left-anchored to the center,
    /// shrinks as the tail catches up, stretches center-anchored to
    /// the right edge, and shrinks once more until gone — one
    /// traversal per period.
    Worm,

    /// Stretches from the seed across the whole row to the right
    /// edge, then folds back into it.
    ///
    /// The long worm: the run grows from the left seed along the
    /// worm's eased path until it spans the full row at the period's
    /// half, then shrinks along the same path as the tail races from
    /// the left edge and swallows it into the right; one traversal
    /// per period.
    #[serde(rename = "worm_long")]
    WormLong,

    /// Draws no strip at all.
    ///
    /// The configured row stays empty for the whole run and the
    /// composer's hairline returns in the spacer. The animation wake
    /// is not part of the bargain: a run in flight keeps waking it,
    /// because the tool rows' spinners and their elapsed counts beat
    /// on that clock whether or not there is a strip to draw.
    Off,

    /// Holds a full-width bar that never moves, its color slowly
    /// breathing.
    ///
    /// One full breath per period between the strip color and about
    /// half its brightness — a static bar that slightly glows. A
    /// theme whose strip color is a named color renders the bar
    /// without the breath.
    Glow,
}

/// Display / rendering preferences.
///
/// Controls how agent output is presented to the user. All fields default via
/// the manual [`Default`] impl below.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// Disable ANSI color in a single-run invocation's console output.
    ///
    /// When true, the lines a headless run streams — tool lines, streamed
    /// reply text — carry no ANSI escapes. Color is equally off when
    /// `NO_COLOR` is set to a non-empty value or stderr is not a
    /// terminal; this flag turns it off even on an interactive terminal.
    /// The interactive TUI is unaffected: its colors come from `theme`.
    /// Defaults to `false` (color follows the terminal's own verdict).
    pub no_color: bool,

    /// How much to print.
    ///
    /// Selects the runtime verbosity level. Defaults to [`Verbosity::Normal`].
    /// See [`Verbosity`].
    pub verbosity: Verbosity,

    /// Theme name (resolved by the TUI).
    ///
    /// A free-form name the TUI maps to a concrete color scheme. Defaults to
    /// `"dch"`.
    pub theme: String,

    /// Where the TUI's activity strip renders while the agent runs.
    ///
    /// `composer` (the default) places the breathing bar in the spacer row
    /// directly above the prompt box; `top` gives it a dedicated first row
    /// of the terminal, above the conversation pane. See
    /// [`StripPosition`].
    pub strip_position: StripPosition,

    /// The animation the TUI's activity strip plays.
    ///
    /// `breathe` (the default) grows from the row's center out to
    /// both edges and shrinks back; `sweep` fills from the left edge
    /// to the right end and then clears left-to-right. See
    /// [`StripStyle`].
    pub strip_style: StripStyle,

    /// How the TUI spells the working directory in the place row
    /// above the composer.
    ///
    /// `full` (the default) shows the absolute path the host named;
    /// `relative` spells it `~/`-relative to the user's home
    /// directory, falling back to the full path outside home. See
    /// [`PathStyle`].
    pub path_style: PathStyle,

    /// Which color vocabulary the welcome splash paints in.
    ///
    /// `auto` (the default) keeps the original truecolors everywhere
    /// they can reach the screen; `truecolor` and `palette` force one
    /// rendition unconditionally — the palette is an explicit opt-in,
    /// never an automatic downgrade. See [`SplashColors`].
    pub splash_colors: SplashColors,

    /// Let the TUI capture the mouse.
    ///
    /// Capture is what lets the wheel scroll the conversation, and it is
    /// also what takes click-drag text selection away from the terminal —
    /// the two are mutually exclusive, and many terminals offer no
    /// modifier bypass. Defaults to `true`: the wheel scrolls the
    /// conversation. Set `false` to hand selection back to the terminal
    /// — the wheel then reaches the app only where the terminal
    /// supports alternate-scroll translation, and keyboard scrolling
    /// (PageUp/PageDown, arrows on an empty editor) covers the rest.
    pub mouse_capture: bool,

    /// How the composer's caret presents itself.
    ///
    /// `blink` (the default) keeps the clock-steady 530 ms on/off
    /// cadence; `static` stands the caret solid in every frame. The
    /// caret's shape and placement never change — only its beat. See
    /// [`CaretDisplay`].
    pub caret_display: CaretDisplay,

    /// How the TUI lays out a file edit's diff in the conversation
    /// pane.
    ///
    /// `inline` (the default) stacks removals above insertions as
    /// full-width solid bands; `side-by-side` pairs each removed line
    /// with its replacement across a center gutter. Narrow terminals
    /// fall back to the inline shape whatever the setting. See
    /// [`DiffLayout`].
    pub diff_layout: DiffLayout,

    /// The color pair a file edit's diff bands paint with.
    ///
    /// `classic` (the default) keeps the review-taught green under
    /// insertions and red under removals; `soft` lightens the green
    /// and dims the red for palettes the saturated hues fight;
    /// `blue-orange` swaps in the color-blind-safe pairing. See
    /// [`DiffPalette`].
    pub diff_palette: DiffPalette,

    /// A custom clipboard command for the TUI's selection copy.
    ///
    /// When set, a completed selection's text is piped to this command's
    /// stdin — run through `sh -c`, so arguments and pipes work — in
    /// addition to the built-in transports: the terminal-side OSC 52
    /// escape and whatever host clipboard tools the environment names
    /// (`tmux set-buffer`, `wl-copy`, `xclip`, `xsel`, `pbcopy`). Set it
    /// where none of the built-ins reach, e.g. a remote session whose
    /// local terminal ignores OSC 52. Defaults to unset.
    pub copy_command: Option<String>,
}

/// How the TUI renders a file edit's diff in the conversation pane.
///
/// Both layouts show the same hunks with the same real line numbers;
/// the choice is spatial. A side-by-side view pairs each removed line
/// with the line that replaced it across a center gutter, which reads
/// fastest on wide terminals; an inline view stacks removals above
/// insertions, which fits narrow terminals and unified-diff habits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffLayout {
    /// Two panes across one row: the old line beside its replacement.
    ///
    /// A replacement reads as one glance. Terminals narrower than the
    /// two-pane floor fall back to the inline shape rather than
    /// squeeze the panes unreadably, so the choice degrades
    /// gracefully on split screens.
    SideBySide,

    /// One pane: removals stack above the insertions that replaced
    /// them, in unified-diff order.
    ///
    /// The default: full-width solid bands under bright text —
    /// the shape an edit reads fastest in.
    #[default]
    Inline,
}

/// Which hues a file edit's diff bands paint with.
///
/// The bands are solid, bright-text carriers; the palette names the
/// pair. `classic` keeps the green and red a code review taught the
/// eye for every theme; `soft` and `blue-orange` are the recolors a
/// palette whose own hues fight saturated green and red reaches for
/// — the softer register, and the pairing that separates under every
/// common color-vision deficiency the way green and red never do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffPalette {
    /// Lighter green insertions, dimmed red removals.
    ///
    /// The softer register for palettes whose own greens and reds
    /// sit pale or warm: bands that read as changes without
    /// shouting over the theme.
    Soft,

    /// Blue insertions, orange removals.
    ///
    /// The color-blind-safe pairing — the hues stay distinguishable
    /// under deuteranopia and protanopia, where green and red
    /// collapse into one.
    BlueOrange,

    /// GitHub-style green insertions, red removals.
    ///
    /// The default: the hues a code review taught the eye.
    #[default]
    Classic,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            no_color: false,
            verbosity: Verbosity::default(),
            theme: "dch".to_string(),
            strip_position: StripPosition::default(),
            strip_style: StripStyle::default(),
            path_style: PathStyle::default(),
            splash_colors: SplashColors::default(),
            diff_layout: DiffLayout::default(),
            diff_palette: DiffPalette::default(),
            mouse_capture: true,
            caret_display: CaretDisplay::default(),
            copy_command: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DchConfig;

    #[test]
    fn display_defaults_to_capturing_the_wheel() {
        let display = DisplayConfig::default();
        assert!(
            display.mouse_capture,
            "the wheel scrolls the conversation by default"
        );
    }

    #[test]
    fn splash_colors_parse_and_default() {
        // The knob speaks its three lowercase names and defaults to
        // auto, which keeps the original truecolors everywhere — the
        // palette is an explicit opt-in, never a downgrade the session
        // decides on its own. The knob rides the [display] table of
        // the whole document, so the honest parse is the one the
        // config loader performs.
        let mode = |document: &str| -> SplashColors {
            toml::from_str::<DchConfig>(document)
                .expect("the document parses")
                .display
                .splash_colors
        };
        assert_eq!(
            mode("[display]\n"),
            SplashColors::Auto,
            "an absent knob is the default that keeps the original art"
        );
        assert_eq!(
            mode("[display]\nsplash_colors = \"truecolor\"\n"),
            SplashColors::Truecolor,
            "truecolor forces the original art everywhere"
        );
        assert_eq!(
            mode("[display]\nsplash_colors = \"palette\"\n"),
            SplashColors::Palette,
            "palette forces the fixed-palette rendition everywhere"
        );
    }

    #[test]
    fn strip_style_parses_with_a_breathe_default() {
        let display: DisplayConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Breathe,
            "a config without the field keeps the breathing animation"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "sweep""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Sweep,
            "the left-anchored fill parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "bounce""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Bounce,
            "the two-way sweep parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "travel""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Travel,
            "the gliding scanner parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "snake""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Snake,
            "the circulating segment parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "worm""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Worm,
            "the inchworm crawl parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "worm_long""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::WormLong,
            "the full-row worm parses from its hyphenated name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "off""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Off,
            "the absent strip parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "glow""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Glow,
            "the breathing static bar parses from its lowercase name"
        );
        assert!(
            toml::from_str::<DisplayConfig>(r#"strip_style = "spin""#).is_err(),
            "an unknown style fails the load instead of falling back"
        );
    }

    #[test]
    fn strip_position_parses_with_a_composer_default() {
        let display: DisplayConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert_eq!(
            display.strip_position,
            StripPosition::Composer,
            "a config without the field keeps the composer placement"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_position = "top""#).unwrap();
        assert_eq!(
            display.strip_position,
            StripPosition::Top,
            "the dedicated top row parses from its lowercase name"
        );
        assert!(
            toml::from_str::<DisplayConfig>(r#"strip_position = "middle""#).is_err(),
            "an unknown placement fails the load instead of falling back"
        );
    }

    #[test]
    fn path_style_parses_with_a_full_default() {
        let display: DisplayConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert_eq!(
            display.path_style,
            PathStyle::Full,
            "a config without the field keeps the full path"
        );
        let display: DisplayConfig = toml::from_str(r#"path_style = "relative""#).unwrap();
        assert_eq!(
            display.path_style,
            PathStyle::Relative,
            "the home-relative shape parses from its lowercase name"
        );
        assert!(
            toml::from_str::<DisplayConfig>(r#"path_style = "short""#).is_err(),
            "an unknown style fails the load instead of falling back"
        );
    }

    #[test]
    fn caret_display_defaults_to_blink_and_parses_both_names() {
        let mut document = toml::from_str::<DisplayConfig>("").expect("the empty document parses");
        assert_eq!(
            document.caret_display,
            CaretDisplay::Blink,
            "the caret blinks by default"
        );
        document = toml::from_str::<DisplayConfig>("caret_display = \"static\"\n")
            .expect("the static spelling parses");
        assert_eq!(document.caret_display, CaretDisplay::Static);
        document = toml::from_str::<DisplayConfig>("caret_display = \"blink\"\n")
            .expect("the blink spelling parses");
        assert_eq!(document.caret_display, CaretDisplay::Blink);
        assert!(
            toml::from_str::<DisplayConfig>("caret_display = \"solid\"\n").is_err(),
            "an unknown spelling fails the load instead of falling back"
        );
    }

    #[test]
    fn a_config_without_the_mouse_field_parses_to_capture() {
        let display: DisplayConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert!(display.mouse_capture);
        assert_eq!(display.theme, "dracula");
    }

    #[test]
    fn the_default_diff_layout_is_inline_and_every_spelling_loads() {
        assert_eq!(
            DisplayConfig::default().diff_layout,
            DiffLayout::Inline,
            "a config that never names a layout runs the inline bands"
        );
        for (spelling, expected) in [
            ("side-by-side", DiffLayout::SideBySide),
            ("inline", DiffLayout::Inline),
        ] {
            let parsed: DisplayConfig =
                toml::from_str(&format!("diff_layout = \"{spelling}\"")).unwrap();
            assert_eq!(
                parsed.diff_layout, expected,
                "the {spelling} spelling loads"
            );
        }
        assert!(
            toml::from_str::<DisplayConfig>("diff_layout = \"diagonal\"").is_err(),
            "an unknown layout is a config error, not a silent default"
        );
    }

    #[test]
    fn the_default_diff_palette_is_classic_and_every_spelling_loads() {
        assert_eq!(
            DisplayConfig::default().diff_palette,
            DiffPalette::Classic,
            "a config that never names a palette runs the classic bands"
        );
        for (spelling, expected) in [
            ("soft", DiffPalette::Soft),
            ("blue-orange", DiffPalette::BlueOrange),
            ("classic", DiffPalette::Classic),
        ] {
            let parsed: DisplayConfig =
                toml::from_str(&format!("diff_palette = \"{spelling}\"")).unwrap();
            assert_eq!(
                parsed.diff_palette, expected,
                "the {spelling} spelling loads"
            );
        }
        assert!(
            toml::from_str::<DisplayConfig>("diff_palette = \"neon\"").is_err(),
            "an unknown palette is a config error, not a silent default"
        );
    }
}
