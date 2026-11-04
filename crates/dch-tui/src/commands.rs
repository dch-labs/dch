//! The composer's in-session command language, parsed locally.
//!
//! A submitted line whose first character is `/` is a command rather than a
//! prompt: it is parsed here and dispatched by the app without ever reaching
//! the model. This module is the whole parse and completion surface — pure,
//! with no knowledge of the loop, the queue, or the terminal — so the command
//! vocabulary has one definition that `parse`, the `/help` palette, `Tab`
//! completion, and the per-command help all read from a single registration
//! table and cannot drift apart.
//!
//! # The shape of the palette
//!
//! Every command is one row of `REGISTRY`: a name, optional aliases, a
//! category for grouping, how many arguments it takes, and the strings that
//! spell it out in `/help`. Adding a command means adding one row — nothing
//! else learns the vocabulary, which is what keeps the palette, completion,
//! and parsing in lockstep.

/// The category a command belongs to, the heading `/help` groups under.
///
/// A closed set chosen so the palette reads in a deliberate order — help,
/// then the acts that move the conversation's container (sessions, the
/// model), then appearance and configuration, then the context levers —
/// rather than the alphabetical accident of a name sort. The [`str`]
/// spelling is what `/help` prints and what the public
/// [`CommandSpec::category`] carries, so no caller needs this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    /// Starting, clearing, and resuming sessions.
    ///
    /// The commands that move the conversation's container: opening a
    /// fresh session and adopting a saved one both land here.
    Sessions,

    /// The model the loop runs: listing it and switching it.
    ///
    /// Everything about which configured model answers — the listing
    /// that surveys them and the switch that moves the loop onto one.
    Models,

    /// The app's look: themes and their previews.
    ///
    /// The palette commands — stepping through themes live and setting
    /// one by name — read here in the listing.
    Appearance,

    /// The conversation's configuration values.
    ///
    /// The `/config` survey and the `/set` and `/unset` session
    /// overrides over the values the file gives.
    Config,

    /// The context the model is shown: manual compaction.
    ///
    /// The lever over how much of the conversation rides the next
    /// request — a compaction pass the user triggers rather than the
    /// threshold.
    Context,

    /// Discovering the palette itself.
    ///
    /// `/help` and its kin — the commands that explain the rest of the
    /// vocabulary — close the listing.
    Help,
}

impl Category {
    /// The heading this category prints under in `/help`.
    ///
    /// The listing's own spelling, friendlier than the variant's
    /// identifier; the only place the string exists.
    const fn label(self) -> &'static str {
        match self {
            Category::Sessions => "Sessions",
            Category::Models => "Models",
            Category::Appearance => "Appearance",
            Category::Config => "Configuration",
            Category::Context => "Context",
            Category::Help => "Help",
        }
    }

    /// The palette's display order: a smaller sort key groups earlier.
    ///
    /// The deliberate reading order — help first, then the acts that
    /// move the conversation, appearance and configuration after —
    /// which the sort in [`specs`] consumes.
    const fn sort_key(self) -> u8 {
        match self {
            Category::Help => 0,
            Category::Sessions => 1,
            Category::Models => 2,
            Category::Appearance => 3,
            Category::Config => 4,
            Category::Context => 5,
        }
    }
}

/// The argument a command accepts.
///
/// Commands otherwise share a shape — a bare name at the head of the line —
/// and differ only in what may follow it, so the whole variety is one choice
/// rather than per-command parsing code that could each drift out of step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgShape {
    /// Takes no argument. A trailing word is a mistake the command has no
    /// reading for, so it is reported rather than run.
    None,

    /// Takes exactly one argument (or, for `/set`, a key and a value). A
    /// missing remainder is a command missing its point.
    One,

    /// Takes a zero-or-one argument. A command like `/theme` or `/help` does
    /// something useful bare *and* with a value: bare opens an interactive
    /// flow or a listing, the argument acts on a named target.
    Optional,
}

/// One command's registration: everything the palette and the parser share.
///
/// A row of this table is the single place a command exists: the same
/// name, argument shape, and wording serve `Tab`, `/help`, and the
/// parser, so the offered vocabulary and the accepted one cannot drift.
struct Spec {
    /// The name typed after the `/`, without the slash.
    ///
    /// Matched case-sensitively as typed: registered names are lowercase, and
    /// a differently-cased word reads as an unknown command rather than a
    /// relaxed match, so `/Help` reports itself instead of silently running.
    name: &'static str,

    /// Alternative names that reach the same command (e.g. `new` for `clear`).
    ///
    /// Recognised by [`parse`] and printed in `/help`, but `Tab` completes to
    /// the primary name — the palette shows canonical spellings.
    aliases: &'static [&'static str],

    /// The group this command lists under in `/help`.
    ///
    /// Chooses the heading and the listing's placement; the registry's
    /// physical order is irrelevant to where a command appears.
    category: Category,

    /// The argument this command takes.
    ///
    /// Drives the arity check in [`parse`] and the placeholder the
    /// palette's argument column shows.
    arg: ArgShape,

    /// The command spelled with its argument shape, the left column of the
    /// palette: `/model <name>`, `/resume [id]`, `/help [command]`.
    usage: &'static str,

    /// The one line saying what the command does, as the palette prints it.
    ///
    /// The `/help` row and the completion popup's description read this
    /// same string, so the two surfaces teach one wording.
    summary: &'static str,

    /// The longer explanation `/help <command>` shows — a sentence or two of
    /// behavior and any guard the command applies, not just the gist.
    detail: &'static str,
}

/// The registered commands, the palette's order within each category.
///
/// Registration order is what a bare `/` offers and the cycle `Tab`
/// walks; the listing re-sorts by category, but completion keeps this
/// table's own sequence.
const REGISTRY: &[Spec] = &[
    Spec {
        name: "help",
        aliases: &["commands", "?"],
        category: Category::Help,
        arg: ArgShape::Optional,
        usage: "/help [command]",
        summary: "List the commands, or explain one in depth.",
        detail: "Lists every command grouped by category. With a command name \
                 (`/help model`) shows that command's detail and aliases.",
    },
    Spec {
        name: "clear",
        aliases: &["new"],
        category: Category::Sessions,
        arg: ArgShape::None,
        usage: "/clear",
        summary: "End this session and start a fresh, empty one.",
        detail: "Empties the transcript and the waiting queue, rewinds the \
                 loop, and opens a new session id to save under. The ended \
                 conversation stays on disk. Refused while a run is in flight.",
    },
    Spec {
        name: "resume",
        aliases: &[],
        category: Category::Sessions,
        arg: ArgShape::Optional,
        usage: "/resume [<id>]",
        summary: "Resume a saved session by id, taking over its file.",
        detail: "Loads the saved conversation back into the loop and the \
                 transcript; later saves write that session's own file. The \
                 id is the one `dch --resume` takes — the exit line prints \
                 it. Refused while a run is in flight.",
    },
    Spec {
        name: "models",
        aliases: &[],
        category: Category::Models,
        arg: ArgShape::None,
        usage: "/models",
        summary: "List the configured models and mark the live one.",
        detail: "Shows each `[[models]]` entry from configuration, marking the \
                 one the loop is on now. Names it offers can be passed to \
                 `/model`.",
    },
    Spec {
        name: "model",
        aliases: &[],
        category: Category::Models,
        arg: ArgShape::One,
        usage: "/model <name>",
        summary: "Switch the running conversation to a configured model.",
        detail: "Renames the model on the live loop between turns, keeping the \
                 conversation. An entry on another endpoint or protocol rebuilds \
                 the runner there and carries the conversation across; the \
                 switch lands when the loop accepts it. Refused while a run is \
                 in flight.",
    },
    Spec {
        name: "theme",
        aliases: &[],
        category: Category::Appearance,
        arg: ArgShape::Optional,
        usage: "/theme [name]",
        summary: "Preview themes, or set the palette by name.",
        detail: "Bare steps to the next theme, live, each time you run it; \
                 the one showing when you stop is the one this session \
                 keeps — the next launch opens the file's theme again, so \
                 a palette meant to stay is set with `/theme <name>` or \
                 `/set theme`, which persist. `/theme <name>` applies one \
                 palette directly, matched case-insensitively.",
    },
    Spec {
        name: "config",
        aliases: &["settings"],
        category: Category::Config,
        arg: ArgShape::None,
        usage: "/config",
        summary: "Show the configuration values in effect.",
        detail: "Prints the running configuration's editable values — model, \
                 verbosity, thresholds, endpoints and the like — as `/set` \
                 would show them, so a value can be read before it is changed.",
    },
    Spec {
        name: "set",
        aliases: &[],
        category: Category::Config,
        arg: ArgShape::One,
        usage: "/set <key> <value>",
        summary: "Override a configuration value for this session.",
        detail: "Changes a value the running panel applies now — the theme and \
                 verbosity (`/set verbosity verbose`). `/set model X` switches \
                 the live model (it is `/model X` in disguise). A value read at \
                 startup cannot be changed for a session already running; /set \
                 says so rather than pretend. A change that lands is saved to \
                 the config file, so the next session opens with it. \
                 `/config` lists every value and its value in effect.",
    },
    Spec {
        name: "unset",
        aliases: &["reset"],
        category: Category::Config,
        arg: ArgShape::One,
        usage: "/unset <key>",
        summary: "Drop a session override, back to its configured value.",
        detail: "Reverts one value `/set` changed back to what configuration \
                 had it, applies the result immediately, and saves the restore \
                 to the config file. `/config` marks which values are \
                 currently overridden.",
    },
    Spec {
        name: "compact",
        aliases: &[],
        category: Category::Context,
        arg: ArgShape::None,
        usage: "/compact",
        summary: "Compact the conversation context now.",
        detail: "Compaction runs automatically as the context nears the window; \
                 this runs one pass on demand, between turns — the next turn \
                 continues from the folded context, and the notice carries the \
                 token figures the pass measured.",
    },
    Spec {
        name: "exit",
        aliases: &["quit", "q"],
        category: Category::Sessions,
        arg: ArgShape::None,
        usage: "/exit",
        summary: "Quit the session and leave the terminal.",
        detail: "Ends the session and restores the terminal. Cancels a run in flight if one is going, then exits. Typing the command is itself the confirmation, so it does not ask you to press twice.",
    },
    Spec {
        name: "stop",
        aliases: &["cancel", "abort"],
        category: Category::Sessions,
        arg: ArgShape::None,
        usage: "/stop",
        summary: "Stop the run in flight, keep the session.",
        detail: "Cancels the current run — the answer being generated stops and the queue waits — but leaves the session and its history standing, unlike /exit. A no-op notice when nothing is running.",
    },
    Spec {
        name: "session",
        aliases: &["id"],
        category: Category::Sessions,
        arg: ArgShape::None,
        usage: "/session",
        summary: "Show the session's id, model, and turns.",
        detail: "Reports the session id (as `dch --resume` would take it), the model the loop runs, the turn count, and the working directory.",
    },
    Spec {
        name: "sessions",
        aliases: &[],
        category: Category::Sessions,
        arg: ArgShape::None,
        usage: "/sessions",
        summary: "List saved sessions and pick one to resume.",
        detail: "Lists the saved sessions — id, model, last activity, and \
                 prompt count, newest first — as a picker: the arrows walk \
                 the rows, Enter resumes the highlighted session, Esc \
                 closes. Refused while a run is in flight, like every \
                 conversation-moving command; `/resume <id>` takes an id \
                 directly.",
    },
    Spec {
        name: "themes",
        aliases: &[],
        category: Category::Appearance,
        arg: ArgShape::None,
        usage: "/themes",
        summary: "List every theme, marking the live one.",
        detail: "Lists all compiled-in palettes by name, the running one marked. Step through them live with a bare /theme, or set one directly with /theme <name>.",
    },
    Spec {
        name: "verbosity",
        aliases: &[],
        category: Category::Config,
        arg: ArgShape::Optional,
        usage: "/verbosity [quiet|normal|verbose]",
        summary: "Set or report the display verbosity.",
        detail: "With a level, sets how much detail each tool row shows (quiet, normal, verbose), applied to the display at once; without one, reports the current level. Overrides /config's value for the session and saves the change to the config file.",
    },
    Spec {
        name: "cost",
        aliases: &["tokens", "usage"],
        category: Category::Context,
        arg: ArgShape::None,
        usage: "/cost",
        summary: "Show the context fill and token totals.",
        detail: "Reports how full the model's context window is and the tokens this session has sent and received, so you can see where the run stands before it grows further.",
    },
    Spec {
        name: "copy",
        aliases: &["copy-answer"],
        category: Category::Context,
        arg: ArgShape::None,
        usage: "/copy",
        summary: "Copy the last answer to the clipboard.",
        detail: "Places the most recent assistant answer on the system clipboard, verbatim. Says so when there is nothing to copy yet, and never reaches the model.",
    },
    Spec {
        name: "version",
        aliases: &["v"],
        category: Category::Help,
        arg: ArgShape::None,
        usage: "/version",
        summary: "Print the dch version.",
        detail: "Reports the version of the running build.",
    },
];

/// One parsed in-session command.
///
/// The command carries everything needed to carry it out and nothing about
/// how: it states the act and its argument, leaving the app to decide which
/// of its lanes runs it. Construction is only ever through [`parse`], so a
/// value here has already cleared the arity check its variant stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// List the palette, or explain `topic` in depth when named.
    ///
    /// The listing groups by category in the palette's own order; a
    /// named topic resolves primary names and aliases alike.
    Help {
        /// A command name to explain, or `None` for the full listing.
        ///
        /// Kept exactly as typed so an unmatched topic can be said
        /// back to the user verbatim.
        topic: Option<String>,
    },

    /// End the session and start a fresh one (`/clear`, `/new`).
    ///
    /// Refused while a run holds the agent; at rest it rotates the
    /// session identity and empties the transcript and queue.
    Clear,

    /// Resume the saved session `target` names, or ask for an id.
    ///
    /// `None` asks for an id rather than acting; a target that is not a
    /// session id is refused where the command runs.
    Resume {
        /// A session id, as `dch --resume` takes it.
        ///
        /// Unvalidated here beyond the shape — the refusal for a
        /// well-formed but unknown id is the driver's to say, where
        /// the sessions root is known.
        target: Option<String>,
    },

    /// List the configured models, marking the live one.
    ///
    /// Reads the `[[models]]` table from configuration; names it
    /// prints are the ones `/model` accepts.
    Models,

    /// Switch the running conversation to the model `name`.
    ///
    /// The switch is a rebuild on the named entry — every field the
    /// entry states belongs to the engine — with the conversation
    /// carried, whether the entry names this provider or another.
    /// With no `[[models]]` entries it is a plain rename. Refused
    /// while a run is in flight.
    Model {
        /// The model name to switch to, as the user typed it.
        ///
        /// Matched case-insensitively against the configured profiles;
        /// a name matching nothing is refused with the accepted names.
        name: String,
    },

    /// Preview themes (`None`) or set the palette to `Some(name)`.
    ///
    /// The bare form steps to the next palette for this session; a
    /// named form lands and persists the palette it names.
    Theme {
        /// The theme to apply, or `None` to open the preview cycle.
        ///
        /// An unknown name is refused with the current palette kept.
        name: Option<String>,
    },

    /// Show the configuration values in effect.
    ///
    /// The survey of every settable key with its current value and
    /// session-override mark — the same vocabulary `/set` accepts.
    Config,

    /// Override config `key` with `value` for this session.
    ///
    /// The value lands in the session's config and the file; a display
    /// value applies this frame, anything the loop reads at
    /// construction applies the next start.
    Set {
        /// The config key to override.
        ///
        /// One of the keys `/config` lists; anything else is refused
        /// with the accepted spellings.
        key: String,

        /// The new value, kept verbatim to the end of the line.
        ///
        /// Parsed exactly as the config file would parse it, so the
        /// command cannot accept a word the file would reject.
        value: String,
    },

    /// Drop the session override on `key`, back to its configured value.
    ///
    /// Restores what the file gave — including absence, when the key
    /// had no value at all — and writes the restore through.
    Unset {
        /// The config key to restore.
        ///
        /// Only a key with a standing override can be unset; anything
        /// else says so rather than rewriting a value.
        key: String,
    },

    /// Run a compaction pass on the live context.
    ///
    /// The manual seam over the engine's own compaction ceremony; a
    /// machine caught mid-turn refuses, and an honest decline changes
    /// nothing.
    Compact,

    /// Quit the TUI and exit the session (`/exit`, `/quit`, `/q`).
    ///
    /// Sets the shutdown flag before cancelling, so prompts queued
    /// behind the exit are not claimed on their way out.
    Exit,

    /// Stop the run in flight without leaving the session (`/stop`).
    ///
    /// The same cancel Esc-Esc fires, as a named command; the session
    /// and its queue stand.
    Stop,

    /// List every compiled-in theme, marking the live one (`/themes`).
    ///
    /// The full registry in construction order — the survey `/theme
    /// <name>` draws its accepted spellings from.
    Themes,

    /// Show token and context usage (`/cost`, `/tokens`).
    ///
    /// The session's accounting — cumulative and per-turn figures
    /// against the context window in effect.
    Cost,

    /// Copy the last answer to the clipboard (`/copy`).
    ///
    /// The final assistant message of the standing conversation, whole;
    /// a session with no answer yet says so.
    Copy,

    /// Show the session's id, model, and shape (`/session`).
    ///
    /// The identity line the exit echo prints — the id a later
    /// `--resume` takes, the model in effect, and the message count.
    Session,

    /// List the saved sessions and pick one to resume (`/sessions`).
    ///
    /// Asks the driver for the session catalog — the sessions root
    /// lives on its side of the crate boundary — and opens a picker
    /// on the answer: newest first, arrows walking, Enter resuming
    /// through the same lane `/resume` takes. Refused while a run is
    /// in flight.
    Sessions,

    /// Print the dch version (`/version`).
    ///
    /// The running build's own version string, nothing more.
    Version,

    /// Set or report the display verbosity (`/verbosity [level]`).
    ///
    /// The mode the transcript renders tool bodies in; the bare form
    /// reports the level in effect.
    Verbosity {
        /// The level to set (`quiet`/`normal`/`verbose`), or `None` to report.
        ///
        /// An unrecognised word is refused with the accepted levels.
        level: Option<String>,
    },
}

/// What a submitted line parsed as.
///
/// The three outcomes are exhaustive: a line either carries no leading `/` and
/// is an ordinary prompt, carries one and names a registered command, or
/// carries one and names nothing (or the wrong shape). Only
/// [`Parsed::NotACommand`] reaches the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// No leading `/`: an ordinary prompt, to flow exactly as any text does.
    ///
    /// The one variant that reaches the model — everything else is
    /// handled in session.
    NotACommand,

    /// A registered command with its argument, ready to dispatch.
    ///
    /// Construction is only through [`parse`], so the arity check has
    /// already passed for the variant carried.
    Command(Command),

    /// A `/`-prefixed line that names no command, or names one with the
    /// wrong shape.
    Unknown {
        /// The word the user typed after the `/`, for the notice.
        ///
        /// Said back verbatim in the unknown-command notice so the
        /// user sees exactly what was not recognised.
        name: String,
    },
}

/// One registered command spelled out for display.
///
/// The view the palette and per-command help read without reaching past this
/// module into the private `REGISTRY`. Its rows come from the same table
/// that governs parsing, so a listing can never advertise a command the parser
/// would reject or omit one it would run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The primary name typed after the `/`, without the slash.
    ///
    /// The canonical spelling `Tab` completes to, whatever alias
    /// reached the command.
    pub name: &'static str,

    /// Alternative names, in registration order.
    ///
    /// Recognised by the parser and printed in `/help`; each is
    /// printed with its own leading slash.
    pub aliases: &'static [&'static str],

    /// The category heading this command lists under.
    ///
    /// The label as `/help` prints it, not the enum identifier — the
    /// display spelling and nothing else.
    pub category: &'static str,

    /// The command spelled with its argument shape, the left palette column.
    ///
    /// What the user types to invoke it, placeholder included — the
    /// registry's own `usage` string carried through.
    pub usage: &'static str,

    /// The argument spelled alone: empty when none, `<name>` when required,
    /// `[name]` when optional. A placeholder, never a concrete example.
    pub args: &'static str,

    /// The one line saying what the command does.
    ///
    /// The same string the completion popup prints beside the name,
    /// so the palette and the panel teach one wording.
    pub summary: &'static str,

    /// The longer `/help <command>` explanation.
    ///
    /// A sentence or two of behavior and any guard the command
    /// applies — the depth the one-line summary cannot carry.
    pub detail: &'static str,
}

/// Where completion candidates come from.
///
/// A command's argument is not a fixed vocabulary but what the user's
/// configuration and session make available — the models and themes listed,
/// the config keys settable. This bundles those read-only sources so
/// [`complete`] offers real values without reaching into configuration itself,
/// keeping this module free of any dependency on how they are produced.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompletionSources<'a> {
    /// The `[[models]]` names from configuration, for `/model`'s argument.
    ///
    /// Read from the user's own configuration at completion time, so
    /// the offered values are the switchable ones and nothing else.
    pub models: &'a [String],

    /// The theme constructor names, for `/theme`'s argument.
    ///
    /// The compiled-in registry's own spellings — the same list
    /// `/themes` prints.
    pub themes: &'a [String],

    /// The settable configuration keys, for `/set` and `/unset`'s argument.
    ///
    /// One list shared with `/config`'s survey and the popup's
    /// descriptions, so every surface offers one vocabulary.
    pub keys: &'a [String],
}

/// Parse one submitted line into what it asks for.
///
/// Only a `/` at the line's very first character opens a command; a line
/// starting any other way — including one that merely contains a `/` or is
/// preceded by whitespace — is [`Parsed::NotACommand`] and flows to the model
/// unchanged. A name or alias is matched against `REGISTRY` and its argument
/// checked against that command's `ArgShape`; a name matching nothing, or
/// matching but breaking the arity, comes back [`Parsed::Unknown`] carrying the
/// typed word. Total: every input yields one of the three readings.
#[must_use]
pub fn parse(line: &str) -> Parsed {
    let Some(body) = line.strip_prefix('/') else {
        return Parsed::NotACommand;
    };
    let (name, argument) = split_command(body);
    let Some(spec) = find(name) else {
        return Parsed::Unknown {
            name: name.to_string(),
        };
    };
    build_command(spec, argument)
}

/// The registered, visible commands spelled out, in palette order.
///
/// Sorted by category (in its deliberate display order) then by name within
/// it, so the listing groups sensibly rather than by registration accident.
#[must_use]
pub fn specs() -> Vec<CommandSpec> {
    let mut visible: Vec<&Spec> = REGISTRY.iter().collect();
    visible.sort_by(|left, right| {
        left.category
            .sort_key()
            .cmp(&right.category.sort_key())
            .then_with(|| left.name.cmp(right.name))
    });
    visible
        .iter()
        .map(|spec| CommandSpec {
            name: spec.name,
            aliases: spec.aliases,
            category: spec.category.label(),
            usage: spec.usage,
            args: arg_hint(spec.arg),
            summary: spec.summary,
            detail: spec.detail,
        })
        .collect()
}

/// The visible command names, in registration order — what a lone `/` offers.
///
/// The cycle order `Tab` walks and the popup's window slices; hidden
/// commands never appear here.
#[must_use]
pub fn names() -> Vec<&'static str> {
    REGISTRY.iter().map(|spec| spec.name).collect()
}

/// The detail and aliases for the command named `name`, for `/help <name>`.
///
/// Matches a primary name or any alias, so `/help new` explains `/clear`.
/// Returns a [`SpecView`] plus the spelling the user asked about, so the
/// explanation can name what they typed. Every field is a `'static` borrow of
/// the `REGISTRY`, so the view is handed out by value with no allocation.
#[must_use]
pub fn detail_for(name: &str) -> Option<(SpecView, &'static str)> {
    let (spec, matched) = find_pair(name)?;
    Some((
        SpecView {
            name: spec.name,
            aliases: spec.aliases,
            usage: spec.usage,
            summary: spec.summary,
            detail: spec.detail,
        },
        matched,
    ))
}

/// The one-line summary for the command named `name`.
///
/// The same words [`specs`] lists under each command in `/help`, lent to
/// surfaces that label as they list — the completion popup above the
/// composer reads it, so the row it offers says what the command does in
/// the register `/help` already prints. Matches a primary name or any
/// alias, like [`detail_for`].
#[must_use]
pub fn summary_for(name: &str) -> Option<&'static str> {
    find(name).map(|spec| spec.summary)
}

/// The visible configuration keys a value command can complete, in order.
///
/// Derived here rather than restated at the app so the keys `Tab`
/// offers after `/set` and `/unset`, and the `/config` listing shows,
/// are one list. (The actual setting and validation of each key lives
/// with the config it edits, in the app — this is only the vocabulary
/// completion offers.)
#[must_use]
pub fn config_keys() -> Vec<&'static str> {
    crate::config_fields::KEYS.to_vec()
}

/// Candidate values for a `Tab` press at the end of `line`, in cycle order.
///
/// Without a leading `/` there is nothing to complete and the vector is empty,
/// the signal to let `Tab` indent as it otherwise would. With the slash,
/// completion has two positions: while the user is on the first word it offers
/// the command names matching the typed fragment — a leading match first, a
/// containing match after; once a space is typed it offers, for the commands
/// that take a value, the configured names from `sources` matching the
/// argument the same ranked way. A value already typed complete is
/// dropped, so a repeated `Tab` offers the rest rather than repeating itself.
#[must_use]
pub fn complete(line: &str, sources: CompletionSources<'_>) -> Vec<String> {
    let Some(body) = line.strip_prefix('/') else {
        return Vec::new();
    };
    // A space anywhere means the caret is past the command word, even before
    // any argument is typed, so `/model ` completes values, not the command
    // name again. `split_command` cannot tell the two apart on its own.
    if body.contains(char::is_whitespace) {
        let (name, argument) = split_command(body);
        argument_candidates(name, argument, sources)
    } else {
        let mut ranked: Vec<(MatchRank, &str)> = REGISTRY
            .iter()
            .filter_map(|spec| match_rank(spec.name, body).map(|rank| (rank, spec.name)))
            .collect();
        ranked.sort_by_key(|&(rank, _)| rank);
        ranked
            .into_iter()
            .map(|(_, name)| name.to_string())
            .collect()
    }
}

/// Where a candidate stands against a typed fragment.
///
/// `Prefix` leads the list — the shape a completion usually means —
/// and `Contains` follows, so a fragment that no candidate starts
/// with still finds the words carrying it inside: `/theme li` reaches
/// `gruvbox_light` and its `*_light` kin. Both compare
/// case-insensitively; the candidate is always offered in its own
/// spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchRank {
    /// The candidate starts with the fragment.
    ///
    /// The shape a completion usually means; these lead the offered
    /// list ahead of any containing match.
    Prefix,

    /// The fragment sits somewhere inside the candidate.
    ///
    /// The fallback that still finds a word carrying the fragment in
    /// its middle — `/theme li` reaching `gruvbox_light`.
    Contains,
}

/// Rank one candidate against a fragment, or `None` when the
/// candidate carries it nowhere. An empty fragment matches every
/// candidate as a prefix, as it always has.
fn match_rank(candidate: &str, fragment: &str) -> Option<MatchRank> {
    if fragment.is_empty() {
        return Some(MatchRank::Prefix);
    }
    let candidate_lower = candidate.to_lowercase();
    let fragment_lower = fragment.to_lowercase();
    if candidate_lower.starts_with(&fragment_lower) {
        Some(MatchRank::Prefix)
    } else if candidate_lower.contains(&fragment_lower) {
        Some(MatchRank::Contains)
    } else {
        None
    }
}

/// The argument candidates for a value-taking command.
///
/// Routes by name: `/model` draws from [`CompletionSources::models`], `/theme`
/// from [`CompletionSources::themes`], `/set` and `/unset` from
/// [`CompletionSources::keys`]. A value already typed complete is
/// dropped, so a repeated `Tab` offers the rest rather than repeating itself.
/// Case-insensitive, so a differently-cased fragment still finds its
/// candidates, while the value itself is offered in its own spelling.
fn argument_candidates(name: &str, argument: &str, sources: CompletionSources<'_>) -> Vec<String> {
    const VERBOSITY_LEVELS: &[&str] = &["quiet", "normal", "verbose"];
    if name == "verbosity" {
        let exact = argument.to_lowercase();
        let mut ranked: Vec<(MatchRank, &str)> = VERBOSITY_LEVELS
            .iter()
            .filter(|level| level.to_lowercase() != exact)
            .filter_map(|level| match_rank(level, argument).map(|rank| (rank, *level)))
            .collect();
        ranked.sort_by_key(|&(rank, _)| rank);
        return ranked
            .into_iter()
            .map(|(_, level)| level.to_string())
            .collect();
    }
    let pool: &[String] = match name {
        "model" => sources.models,
        "theme" => sources.themes,
        "set" | "unset" => sources.keys,
        _ => return Vec::new(),
    };
    let exact = argument.to_lowercase();
    let mut ranked: Vec<(MatchRank, &String)> = pool
        .iter()
        .filter(|candidate| candidate.to_lowercase() != exact)
        .filter_map(|candidate| match_rank(candidate, argument).map(|rank| (rank, candidate)))
        .collect();
    ranked.sort_by_key(|&(rank, _)| rank);
    ranked
        .into_iter()
        .map(|(_, candidate)| candidate.clone())
        .collect()
}

/// Split the body after the `/` into the command word and its argument.
///
/// The word runs to the first whitespace; the argument is everything after it,
/// trimmed. An empty body yields two empty halves. Splitting on whitespace (not
/// a single space) tolerates a run of spaces or a tab between command and value.
fn split_command(body: &str) -> (&str, &str) {
    match body.split_once(char::is_whitespace) {
        Some((name, argument)) => (name, argument.trim()),
        None => (body.trim_end(), ""),
    }
}

/// Find the [`Spec`] a typed word names, by primary name or alias.
///
/// The parser's whole vocabulary lookup; `None` is the unknown-command
/// reading.
fn find(name: &str) -> Option<&'static Spec> {
    REGISTRY
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
}

/// Find the [`Spec`] and the exact spelling matched (primary or alias).
///
/// [`detail_for`](crate::commands::detail_for)'s companion: the second
/// half is what the user typed, so an explanation reached through an
/// alias can name the alias.
fn find_pair(name: &str) -> Option<(&'static Spec, &'static str)> {
    REGISTRY.iter().find_map(|spec| {
        if spec.name == name {
            Some((spec, spec.name))
        } else {
            spec.aliases
                .iter()
                .find(|alias| **alias == name)
                .map(|alias| (spec, *alias))
        }
    })
}

/// Resolve a matched [`Spec`] and its argument into a [`Parsed`].
///
/// Enforces arity from [`ArgShape`]: `None` rejects a non-empty remainder,
/// `One` rejects an empty one, `Optional` accepts either. A command needing
/// two parts (`/set`) checks for its second inside the builder. The
/// single-word commands (`/help`, `/resume`, `/unset`) refuse a surplus
/// second word the same way — a `/resume <id> new` is as much a mistake as
/// an argument to `/clear`, not a command whose trailing word deserves to
/// vanish. When the shape fits, the matching [`Command`] is built carrying
/// the argument.
fn build_command(spec: &Spec, argument: &str) -> Parsed {
    let arity_ok = match spec.arg {
        ArgShape::None => argument.is_empty(),
        ArgShape::One => !argument.is_empty(),
        ArgShape::Optional => true,
    };
    if !arity_ok {
        return Parsed::Unknown {
            name: spec.name.to_string(),
        };
    }
    let single_word = matches!(spec.name, "help" | "resume" | "unset");
    if single_word && argument.split_whitespace().nth(1).is_some() {
        return Parsed::Unknown {
            name: spec.name.to_string(),
        };
    }
    let command = match spec.name {
        "help" => Command::Help {
            topic: first_word(argument),
        },
        "clear" => Command::Clear,
        "resume" => Command::Resume {
            target: first_word(argument),
        },
        "models" => Command::Models,
        "model" => Command::Model {
            name: argument.to_string(),
        },
        "theme" => Command::Theme {
            name: non_empty(argument),
        },
        "config" => Command::Config,
        "set" => match argument.split_once(char::is_whitespace) {
            Some((key, value)) => Command::Set {
                key: key.to_string(),
                value: value.trim().to_string(),
            },
            None => {
                return Parsed::Unknown {
                    name: "set".to_string(),
                };
            }
        },
        "unset" => Command::Unset {
            key: first_word(argument).unwrap_or_default(),
        },
        "compact" => Command::Compact,
        "exit" => Command::Exit,
        "stop" => Command::Stop,
        "session" => Command::Session,
        "sessions" => Command::Sessions,
        "themes" => Command::Themes,
        "verbosity" => Command::Verbosity {
            level: non_empty(argument),
        },
        "cost" => Command::Cost,
        "copy" => Command::Copy,
        "version" => Command::Version,
        _ => {
            return Parsed::Unknown {
                name: spec.name.to_string(),
            };
        }
    };
    Parsed::Command(command)
}

/// The first whitespace-delimited word, or `None` when there is none.
///
/// Splits the argument the way the shell would hand it to a program,
/// so a key with its value separates at the first space.
fn first_word(argument: &str) -> Option<String> {
    argument
        .split_whitespace()
        .next()
        .map(str::to_string)
        .filter(|word| !word.is_empty())
}

/// The argument as `Some` when it holds anything, else `None`.
///
/// The Optional shape's own gate: a bare command and a command with
/// only whitespace after it both read as acting bare.
fn non_empty(argument: &str) -> Option<String> {
    (!argument.is_empty()).then(|| argument.to_string())
}

/// The display spelling of an [`ArgShape`]: the argument column's placeholder.
///
/// Angle brackets name a required argument, square brackets an
/// optional one, and no hint at all a command that takes nothing.
const fn arg_hint(arg: ArgShape) -> &'static str {
    match arg {
        ArgShape::None => "",
        ArgShape::One => "<name>",
        ArgShape::Optional => "[name]",
    }
}

/// A command spelled out for `/help <command>`, the fields an explanation shows.
///
/// A small public view so the app can render a single command's help (its
/// usage, aliases, and detail) without touching the private `REGISTRY`. Every
/// field is a `'static` borrow of the table, so the view is `Copy` and handed
/// out by value.
#[derive(Debug, Clone, Copy)]
pub struct SpecView {
    /// The primary name.
    ///
    /// The canonical spelling, whatever alias the user asked about.
    pub name: &'static str,

    /// The aliases, in registration order.
    ///
    /// Printed with leading slashes so an explanation can list them
    /// exactly as they are typed.
    pub aliases: &'static [&'static str],

    /// The usage line.
    ///
    /// The invocation with its argument placeholder, as the registry
    /// spells it.
    pub usage: &'static str,

    /// The one-line summary.
    ///
    /// The same wording the palette row carries.
    pub summary: &'static str,

    /// The longer detail paragraph.
    ///
    /// The depth `/help <command>` exists to show — behavior and
    /// guards, not just the gist.
    pub detail: &'static str,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    fn sources() -> CompletionSources<'static> {
        static DATA: std::sync::OnceLock<(Vec<String>, Vec<String>, Vec<String>)> =
            std::sync::OnceLock::new();
        let (models, themes, keys) = DATA.get_or_init(|| {
            (
                ["gpt-4o", "gpt-4o-mini", "claude-sonnet"]
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect(),
                ["Dracula", "Gruvbox Dark", "Nord"]
                    .iter()
                    .map(|t| (*t).to_string())
                    .collect(),
                ["model", "verbosity", "temperature"]
                    .iter()
                    .map(|k| (*k).to_string())
                    .collect(),
            )
        });
        CompletionSources {
            models,
            themes,
            keys,
        }
    }

    #[test]
    fn plain_text_without_a_leading_slash_is_not_a_command() {
        assert_eq!(parse("hello"), Parsed::NotACommand);
        assert_eq!(parse("type /clear in the docs"), Parsed::NotACommand);
        assert_eq!(parse("  /help"), Parsed::NotACommand);
    }

    #[test]
    fn every_registered_name_parses_to_its_command() {
        assert_eq!(
            parse("/help"),
            Parsed::Command(Command::Help { topic: None })
        );
        assert_eq!(parse("/clear"), Parsed::Command(Command::Clear));
        assert_eq!(parse("/models"), Parsed::Command(Command::Models));
        assert_eq!(parse("/config"), Parsed::Command(Command::Config));
        assert_eq!(parse("/compact"), Parsed::Command(Command::Compact));
        assert_eq!(
            parse("/model gpt-4o"),
            Parsed::Command(Command::Model {
                name: "gpt-4o".to_string()
            })
        );
    }

    #[test]
    fn the_conversation_control_commands_parse() {
        assert_eq!(parse("/exit"), Parsed::Command(Command::Exit));
        assert_eq!(parse("/stop"), Parsed::Command(Command::Stop));
        assert_eq!(parse("/session"), Parsed::Command(Command::Session));
        assert_eq!(parse("/themes"), Parsed::Command(Command::Themes));
        assert_eq!(parse("/cost"), Parsed::Command(Command::Cost));
        assert_eq!(parse("/copy"), Parsed::Command(Command::Copy));
        assert_eq!(parse("/version"), Parsed::Command(Command::Version));
        // The `q` shorthand reaches the same command as `/exit`.
        assert_eq!(parse("/q"), Parsed::Command(Command::Exit));
    }

    #[test]
    fn verbosity_carries_the_level_only_when_one_is_given() {
        assert_eq!(
            parse("/verbosity"),
            Parsed::Command(Command::Verbosity { level: None })
        );
        assert_eq!(
            parse("/verbosity quiet"),
            Parsed::Command(Command::Verbosity {
                level: Some("quiet".to_string())
            })
        );
    }

    #[test]
    fn aliases_reach_their_command() {
        assert_eq!(
            parse("/commands"),
            Parsed::Command(Command::Help { topic: None })
        );
        assert_eq!(
            parse("/? gpt"),
            Parsed::Command(Command::Help {
                topic: Some("gpt".to_string())
            })
        );
        assert_eq!(parse("/new"), Parsed::Command(Command::Clear));
        assert_eq!(parse("/settings"), Parsed::Command(Command::Config));
        assert_eq!(
            parse("/reset verbosity"),
            Parsed::Command(Command::Unset {
                key: "verbosity".to_string()
            })
        );
    }

    #[test]
    fn value_argument_is_kept_whole_including_spaces() {
        // A theme name with a space survives as one argument, not two.
        assert_eq!(
            parse("/theme Gruvbox Dark"),
            Parsed::Command(Command::Theme {
                name: Some("Gruvbox Dark".to_string())
            })
        );
    }

    #[test]
    fn bare_optional_commands_do_the_bare_thing() {
        assert_eq!(
            parse("/theme"),
            Parsed::Command(Command::Theme { name: None })
        );
        assert_eq!(
            parse("/resume"),
            Parsed::Command(Command::Resume { target: None })
        );
    }

    #[test]
    fn resume_carries_a_target_and_help_carries_a_topic() {
        assert_eq!(
            parse("/resume 3"),
            Parsed::Command(Command::Resume {
                target: Some("3".to_string())
            })
        );
        assert_eq!(
            parse("/help model"),
            Parsed::Command(Command::Help {
                topic: Some("model".to_string())
            })
        );
    }

    #[test]
    fn set_splits_key_from_value_and_value_keeps_spaces() {
        assert_eq!(
            parse("/set verbosity high"),
            Parsed::Command(Command::Set {
                key: "verbosity".to_string(),
                value: "high".to_string()
            })
        );
        assert_eq!(
            parse("/set base_url http://x/y path"),
            Parsed::Command(Command::Set {
                key: "base_url".to_string(),
                value: "http://x/y path".to_string()
            })
        );
    }

    #[test]
    fn arity_mistakes_are_unknown_naming_the_command() {
        // Over-supplied an argumentless command.
        assert_eq!(
            parse("/clear me"),
            Parsed::Unknown {
                name: "clear".to_string()
            }
        );
        assert_eq!(
            parse("/compact now"),
            Parsed::Unknown {
                name: "compact".to_string()
            }
        );
        // Missing a required argument.
        assert_eq!(
            parse("/model"),
            Parsed::Unknown {
                name: "model".to_string()
            }
        );
        // `/set` needs a key AND a value.
        assert_eq!(
            parse("/set"),
            Parsed::Unknown {
                name: "set".to_string()
            }
        );
        assert_eq!(
            parse("/set model"),
            Parsed::Unknown {
                name: "set".to_string()
            }
        );
    }

    #[test]
    fn a_surplus_second_word_is_refused_for_the_single_word_commands() {
        // `/resume <id> new` and `/unset key extra` used to silently drop
        // the trailing word — a mistyped address or a pasted id with a
        // space in it read as the first word alone. A second word is as
        // much a mistake as an argument to `/clear`, so it refuses the
        // same way; the one-word forms still parse untouched.
        assert_eq!(
            parse("/resume 3c0f new"),
            Parsed::Unknown {
                name: "resume".to_string()
            }
        );
        assert_eq!(
            parse("/unset theme junk"),
            Parsed::Unknown {
                name: "unset".to_string()
            }
        );
        assert_eq!(
            parse("/help model extra"),
            Parsed::Unknown {
                name: "help".to_string()
            }
        );
        assert_eq!(
            parse("/resume 3c0f"),
            Parsed::Command(Command::Resume {
                target: Some("3c0f".to_string())
            })
        );
        assert_eq!(
            parse("/unset theme"),
            Parsed::Command(Command::Unset {
                key: "theme".to_string()
            })
        );
    }

    #[test]
    fn help_takes_an_optional_topic() {
        // `/help` on its own lists the palette; with a word it names a topic.
        assert_eq!(
            parse("/help"),
            Parsed::Command(Command::Help { topic: None })
        );
        assert_eq!(
            parse("/help model"),
            Parsed::Command(Command::Help {
                topic: Some("model".to_string())
            })
        );
    }

    #[test]
    fn an_unknown_name_is_reported_with_the_typed_spelling() {
        assert_eq!(
            parse("/frobnicate"),
            Parsed::Unknown {
                name: "frobnicate".to_string()
            }
        );
    }

    #[test]
    fn completion_offers_command_names_for_a_first_word_prefix() {
        let none = CompletionSources::default();
        assert_eq!(complete("/", none), names());
        assert!(complete("/mo", none).contains(&"models".to_string()));
        // A first word no command begins with completes to nothing.
        assert_eq!(complete("/zz", none), Vec::<String>::new());
    }

    #[test]
    fn completion_offers_values_after_the_command() {
        assert_eq!(
            complete("/model gpt", sources()),
            vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]
        );
        assert_eq!(complete("/theme nord", sources()), Vec::<String>::new());
        assert_eq!(complete("/theme no", sources()), vec!["Nord".to_string()]);
        assert!(complete("/set ve", sources()).contains(&"verbosity".to_string()));
    }

    #[test]
    fn completion_of_a_complete_value_offers_nothing() {
        assert_eq!(
            complete("/model claude-sonnet", sources()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn completion_matches_a_fragment_anywhere_not_just_at_the_start() {
        // `/theme li`: nothing starts with it, but the *_light family
        // carries it inside — contains matching reaches what prefix
        // matching cannot, and a prefix match (lilac) always leads the
        // contains matches that follow, pool order holding within each
        // tier.
        let themes: Vec<String> = ["github_light", "lilac", "gruvbox_light", "Nord"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let sources = CompletionSources {
            models: &[],
            themes: &themes,
            keys: &[],
        };
        assert_eq!(
            complete("/theme li", sources),
            vec![
                "lilac".to_string(),
                "github_light".to_string(),
                "gruvbox_light".to_string()
            ],
            "a prefix match leads; contains matches follow in pool order"
        );
        assert_eq!(
            complete("/theme gh", sources),
            vec!["github_light".to_string(), "gruvbox_light".to_string()],
            "contains matching finds every word carrying the fragment"
        );
    }

    #[test]
    fn command_names_match_anywhere_too() {
        let none = CompletionSources::default();
        assert!(
            complete("/heme", none).contains(&"theme".to_string()),
            "a fragment inside the command word still finds the command"
        );
    }

    #[test]
    fn completion_offers_the_verbosity_levels_as_a_fixed_vocabulary() {
        // The three levels are a built-in set, not a value read from config, so
        // a bare `/verbosity ` lists all three and a start narrows to those it
        // begins with — independent of the sources passed in.
        let none = CompletionSources::default();
        assert_eq!(
            complete("/verbosity ", none),
            vec![
                "quiet".to_string(),
                "normal".to_string(),
                "verbose".to_string()
            ]
        );
        // Once the command word is complete, a start of a level narrows to it.
        assert_eq!(complete("/verbosity q", none), vec!["quiet".to_string()]);
        // A level already spelled out completes to nothing.
        assert_eq!(complete("/verbosity quiet", none), Vec::<String>::new());
    }

    #[test]
    fn specs_group_and_cover_the_whole_visible_palette() {
        let specs = specs();
        assert_eq!(specs.len(), names().len(), "every visible name has a row");
        // Grouped: all Help rows sort before all Sessions rows, etc.
        let keys: Vec<&str> = specs.iter().map(|spec| spec.category).collect();
        assert_eq!(keys.first(), Some(&"Help"));
        let compact = specs
            .iter()
            .find(|spec| spec.name == "compact")
            .expect("compact row");
        assert_eq!(compact.category, "Context");
        assert!(compact.usage.contains("/compact"));
    }

    #[test]
    fn detail_for_matches_a_name_or_an_alias() {
        let (spec, matched) = detail_for("new").expect("new explains clear");
        assert_eq!(spec.name, "clear");
        assert_eq!(matched, "new");
        assert!(spec.detail.contains("session"));
    }

    #[test]
    fn config_keys_is_a_shared_nonempty_list() {
        let keys = config_keys();
        assert!(!keys.is_empty(), "there is something to set");
        assert!(keys.contains(&"model"));
    }
}
