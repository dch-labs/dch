//! The rule types: what a verdict matches on, and the shapes rules take
//! in configuration, on the wire, and in the session's memory.
//!
//! One matcher vocabulary — a tool name, a shell verb prefix, a path
//! glob — serves all three spellings. The engine's [`Rule`] is the
//! evaluated form; [`PermissionsConfig`] is the configuration's; and
//! [`PermissionRule`] and [`RememberRules`] are what the session records
//! and what an ask hands back.

use std::collections::BTreeMap;
use std::fmt;

use globset::Glob;
use globset::GlobBuilder;

use crate::vocabulary::PermissionOutcome;

/// A compiled path glob — the matcher behind [`Matcher::PathGlob`].
///
/// Patterns follow globset semantics with path components kept intact: `*`
/// stays inside one path component and `**` crosses them, so `crates/**`
/// covers the subtree while `crates/*` covers only its direct children. A
/// glob is matched against a path relative to the session's working
/// directory, which is the spelling both config rules and remembered
/// approvals are written in.
#[derive(Clone)]
pub struct PathGlob {
    /// The pattern as written, carried verbatim for display and round-trip.
    ///
    /// Equality, hashing, and the debug rendering all read this field — two
    /// globs compiled from the same pattern are the same glob.
    pattern: String,

    /// The compiled glob, rebuilt per match from the immutable pattern.
    ///
    /// Storing the uncompiled form keeps the type cheap to clone, hash, and
    /// compare, and a match compiles it in microseconds — the frequency a
    /// permission verdict runs at cannot feel the difference.
    glob: Glob,
}

impl PathGlob {
    /// Compile `pattern` into a matcher, refusing strings globset rejects.
    ///
    /// Component separators are made literal, so `*` never crosses a `/` —
    /// the spelling a user writes (`crates/**`, `**/.env`) is the matching
    /// they get. The error carries the pattern so a bad line in a config or
    /// a permissions file can be named.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidGlob`] when the pattern is not a well-formed glob —
    /// an unterminated character class, a malformed `**`, or any other
    /// string globset's compiler rejects.
    pub fn new(pattern: impl Into<String>) -> Result<Self, InvalidGlob> {
        let pattern = pattern.into();
        let glob = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|source| InvalidGlob {
                pattern: pattern.clone(),
                source,
            })?;
        Ok(Self { pattern, glob })
    }

    /// The pattern as written, for display and round-trip.
    ///
    /// The permissions file and the `/permissions` surface show this
    /// spelling, and an edited config round-trips it byte-for-byte —
    /// nothing re-canonicalizes what the author wrote.
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Whether `path` — spelled relative to the working directory — matches.
    ///
    /// The matcher is rebuilt from the immutable pattern per call, so the
    /// check is stateless and shareable; the cost is a regex wrap, well
    /// under the dispatch a permission verdict already pays.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.glob.compile_matcher().is_match(path)
    }
}

impl fmt::Debug for PathGlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PathGlob")
            .field("pattern", &self.pattern)
            .finish_non_exhaustive()
    }
}

impl PartialEq for PathGlob {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for PathGlob {}

/// A path glob globset refused to compile.
///
/// Raised by [`PathGlob::new`] and surfaced wherever a pattern enters — a
/// config load names the offending rule, a permissions file names the
/// offending entry — so a typo is a loud error rather than a rule that
/// silently matches nothing.
#[derive(Debug, thiserror::Error)]
#[error("invalid path glob {pattern:?}: {source}")]
pub struct InvalidGlob {
    /// The rejected pattern, carried so the offending line can be named.
    ///
    /// The `Display` rendering embeds it, so a config error reading
    /// `invalid path glob "crates/**["` points straight at the line to fix.
    pattern: String,

    /// The compiler's own diagnosis of the pattern.
    ///
    /// Chained as the error source, so a caller that wants the underlying
    /// reason gets globset's wording without this type paraphrasing it.
    #[source]
    source: globset::Error,
}

/// What one rule matches on.
///
/// A matcher is a predicate over a call's [`CallShape`](crate::engine::CallShape),
/// never over the world: the exact registered tool name, one shell verb
/// prefix, or a path glob over the call's write targets. There is
/// deliberately no category-level matcher for user-written rules — a
/// category is the built-in matrix's vocabulary, and config rules name
/// what they mean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Matcher {
    /// The call's registered tool name, matched exactly.
    ///
    /// `Write` matches only the write tool; nothing about the tool's input
    /// is consulted, so a whole-tool rule is as broad as the tool itself.
    Tool(String),

    /// One arity-trimmed shell verb prefix, `cargo test` for
    /// `cargo test -p x`.
    ///
    /// The prefix matches when any clean command segment carries it —
    /// complex segments never prefix-match.
    ShellPrefix(String),

    /// A path glob over the call's write targets.
    ///
    /// Every target the call would overwrite is matched against the glob,
    /// relative to the working directory; a call with no write targets
    /// matches nothing.
    PathGlob(PathGlob),
}

/// One permission rule: what it matches and the verdict it lands.
///
/// A rule is inert data — the engine reads it, the gate records which rule
/// fired, and a config or a permissions file stores it. The `id` is what
/// makes the rule auditable rather than magical: every decision names the
/// rule that decided it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The rule's stable identity, carried into every decision it makes.
    ///
    /// A config rule's id comes from the config (a generated position when
    /// the author wrote none); a name like `no-env-writes` turns `deny` in
    /// a log line into something a human can trace back to its line.
    pub id: String,

    /// The verdict a match lands: allow, ask, or block.
    ///
    /// The engine reads the two directions differently — an allow rule
    /// loosens through full-coverage, a deny or ask rule tightens on any
    /// match — so this field carries the whole behavioral contract of the
    /// rule.
    pub outcome: PermissionOutcome,

    /// What the rule matches on.
    ///
    /// One of the three predicates the engine understands; a config file
    /// or a permissions file spells it with one key per rule.
    pub matcher: Matcher,
}

/// Which verdict one configured permission rule lands.
///
/// The wire spelling is lowercase — `allow`, `ask`, `deny` — matching the
/// words the `[permissions]` documentation uses; serde rejects anything
/// else at load with the field and the offending value named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRuleOutcome {
    /// Run matching calls without confirmation, however the matrix would
    /// have ruled — the explicit loosening a config rule can grant.
    Allow,

    /// Prompt for matching calls even in modes that would have allowed
    /// them — the tightening that keeps `auto` honest about one tool or
    /// path.
    Ask,

    /// Refuse matching calls outright, in every mode including `read_only`
    /// (where the matrix already blocks, so the rule only names itself).
    Deny,
}

/// One configured permission rule, as the `[permissions]` file writes it.
///
/// A rule names a verdict (`outcome`) and exactly one matcher — `tool`,
/// `shell_prefix`, or `path_glob`. Two or no matcher keys are a load-time
/// error, not a rule that silently means both or neither; the same for an
/// unknown key, so a typo (`glob` for `path_glob`) cannot become a rule
/// that never fires.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRuleConfig {
    /// The rule's identity, for logs and the audit trail.
    ///
    /// Optional; when absent the engine refers to the rule by its
    /// declaration position (`rule-3`), stable for the run the config
    /// loaded in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// What the rule does to a matching call. See [`PermissionRuleOutcome`].
    ///
    /// Required — a rule without a verdict is a load-time parse error, not
    /// a rule with a default behavior.
    pub outcome: PermissionRuleOutcome,

    /// Match by exact registered tool name — `"Write"`, `"WebFetch"`.
    ///
    /// A whole-tool rule is as broad as the tool itself; reach for it when
    /// the tool's whole surface is meant, and prefer `path_glob` when only
    /// part of a write tool's reach is meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,

    /// Match one shell verb prefix — `"cargo test"` for
    /// `cargo test -p x`.
    ///
    /// The prefix is matched against each clean segment of the
    /// command, the same segmentation the "allow always" memory uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_prefix: Option<String>,

    /// Match write targets under a path glob — `"crates/**"`, `"**/.env"`.
    ///
    /// The glob is compiled at load with the same settings the policy
    /// engine uses, so a config that loads is a config the engine can
    /// match: `*` stays inside one path component, `**` crosses them,
    /// paths are relative to the session's working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_glob: Option<String>,
}

impl PermissionRuleConfig {
    /// One-line spelling of the rule, as a listing shows it — the
    /// verdict, then the matcher it names.
    ///
    /// The same words the `[permissions]` documentation uses for the
    /// keys, so a row here reads back to its TOML without a
    /// translation table.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let outcome = match self.outcome {
            PermissionRuleOutcome::Allow => "allow",
            PermissionRuleOutcome::Ask => "ask",
            PermissionRuleOutcome::Deny => "deny",
        };
        let matcher = match (&self.tool, &self.shell_prefix, &self.path_glob) {
            (Some(tool), _, _) => format!("tool {tool}"),
            (_, Some(prefix), _) => format!("shell_prefix {prefix}"),
            (_, _, Some(glob)) => format!("path_glob {glob}"),
            (None, None, None) => String::new(),
        };
        if matcher.is_empty() {
            outcome.to_string()
        } else {
            format!("{outcome} {matcher}")
        }
    }

    /// The rule's id as the engine names it: the author's, or the
    /// declaration position when none was written.
    ///
    /// `position` is the rule's zero-based index in the config's rule
    /// list; a generated id is only ever used within the run that loaded
    /// that list, so a shifted file never silently re-binds an old name.
    #[must_use]
    pub fn rule_id(&self, position: usize) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => format!("rule-{position}"),
        }
    }

    /// The matcher key the rule names, when it names exactly one.
    ///
    /// The configuration's validation turns a missing or doubled key into
    /// a load-time error; the projection in
    /// [`PermissionsConfig::compiled`] trusts its verdict.
    fn matcher_key(&self) -> Option<&'static str> {
        match (
            self.tool.is_some(),
            self.shell_prefix.is_some(),
            self.path_glob.is_some(),
        ) {
            (true, false, false) => Some("tool"),
            (false, true, false) => Some("shell_prefix"),
            (false, false, true) => Some("path_glob"),
            _ => None,
        }
    }
}

/// Permission rules the user configures ahead of any session.
///
/// The `[permissions]` section. These rules sit above the session's
/// remembered approvals and the built-in mode × category matrix in the
/// policy engine's precedence: a `deny` or `ask` rule fires on any call it
/// matches, an `allow` rule set loosens only when it covers every axis of
/// a call. A `deny` applies in every mode including `read_only`, whose
/// "touches nothing" line stays absolute; `ask` and `allow` are skipped
/// under it. With the section absent the field is
/// empty and the engine is pure matrix plus memory — today's behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    /// The rules, in declaration order — the order the engine resolves in.
    ///
    /// See [`PermissionRuleConfig`] for the shape of one entry; an empty
    /// list configures nothing, which is the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PermissionRuleConfig>,

    /// The preset selected for the sessions that load this config.
    ///
    /// Names an entry in [`Self::presets`]; its rules join the config
    /// layer after the top-level ones. `None` — the default — selects
    /// nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,

    /// The named rule bundles a session can select.
    ///
    /// Each preset is one `[permissions.presets.<name>]` table carrying
    /// its own `rules` array, shaped exactly like the top-level list.
    /// Empty by default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub presets: BTreeMap<String, Preset>,
}

impl PermissionsConfig {
    /// Project the configured rules into engine-ready parts.
    ///
    /// Returns `(id, outcome, matcher)` triples in declaration order; the
    /// caller (dch-loop) turns them into the policy engine's rule type.
    /// The matcher payload is plain strings because the config crate sits
    /// below the engine's crate and cannot name its types; dch-loop does
    /// the unpacking. Compilation cannot fail here — validation refuses
    /// an uncompilable pattern at load — so the projection is total.
    #[must_use]
    pub fn compiled(&self) -> Vec<(String, PermissionRuleOutcome, MatcherParts)> {
        self.rules
            .iter()
            .enumerate()
            .map(|(position, rule)| {
                let matcher = match rule.matcher_key() {
                    Some("tool") => MatcherParts::Tool(rule.tool.clone().unwrap_or_default()),
                    Some("shell_prefix") => {
                        MatcherParts::ShellPrefix(rule.shell_prefix.clone().unwrap_or_default())
                    }
                    _ => MatcherParts::PathGlob(rule.path_glob.clone().unwrap_or_default()),
                };
                (rule.rule_id(position), rule.outcome, matcher)
            })
            .collect()
    }

    /// Compile the configured rules into engine rules.
    ///
    /// The projection the gate consumes: ids authored or positional,
    /// outcomes mapped onto the verdict vocabulary (`deny` lands as a
    /// block), and matchers compiled with the engine's own glob settings.
    /// When a preset is selected, its rules join after the top-level
    /// ones — the preset's ids are prefixed with its name, so the audit
    /// trail names where each rule came from. Validation compiled every
    /// pattern at load with the engine's settings, so a failure here
    /// means the two settings drifted — the offending rule is skipped
    /// with a warning rather than silently protecting nothing.
    #[must_use]
    pub fn engine_rules(&self) -> Vec<Rule> {
        self.engine_rules_at(self.preset.as_deref())
    }

    /// Compile the configured rules under a live preset selection that
    /// overrides the file's own.
    ///
    /// The same projection [`engine_rules`](Self::engine_rules) makes,
    /// with the selection passed in rather than read: `/preset` moves
    /// a process-live selection the configuration file never carries,
    /// and the gate's re-seeds — the swap itself, a `/clear` rotation,
    /// a healthy resume — all join from the selection in effect, so
    /// the rules cell and the listing it mirrors cannot drift apart.
    /// `None` selects nothing, whatever the file says. A name the
    /// presets do not carry is refused where it can be — the
    /// configuration rejects a dangling selection at load, and the
    /// CLI flag is validated the same way — so the join-nothing arm
    /// here belongs to the one holder that cannot be refused on the
    /// spot: the process-live selection cell, which a configuration
    /// reload can leave naming a preset that no longer exists, and
    /// which then joins nothing until the next selection says
    /// otherwise.
    #[must_use]
    pub fn engine_rules_at(&self, preset: Option<&str>) -> Vec<Rule> {
        let mut rules = self.engine_rules_with_prefix("");
        if let Some(name) = preset
            && let Some(preset) = self.presets.get(name)
        {
            rules.extend(preset.engine_rules(name));
        }
        rules
    }

    /// The conversion core, prefixing every rule id with `prefix`.
    ///
    /// Top-level rules pass the empty prefix; a preset passes
    /// `preset.<name>.`.
    #[must_use]
    pub fn engine_rules_with_prefix(&self, prefix: &str) -> Vec<Rule> {
        self.compiled()
            .into_iter()
            .filter_map(|(id, outcome, matcher)| {
                let matcher = match matcher {
                    MatcherParts::Tool(tool) => Some(Matcher::Tool(tool)),
                    MatcherParts::ShellPrefix(prefix) => Some(Matcher::ShellPrefix(prefix)),
                    MatcherParts::PathGlob(pattern) => match PathGlob::new(pattern) {
                        Ok(glob) => Some(Matcher::PathGlob(glob)),
                        Err(why) => {
                            tracing::warn!(
                                error = %why,
                                "a configured permission glob no longer compiles — the rule \
                                 is skipped"
                            );
                            None
                        }
                    },
                }?;
                Some(Rule {
                    id: format!("{prefix}{id}"),
                    outcome: match outcome {
                        PermissionRuleOutcome::Allow => PermissionOutcome::Allow,
                        PermissionRuleOutcome::Ask => PermissionOutcome::Ask,
                        PermissionRuleOutcome::Deny => PermissionOutcome::Block,
                    },
                    matcher,
                })
            })
            .collect()
    }

    /// Reject rules that could never do what they say.
    ///
    /// A rule naming no matcher key — or naming two — cannot be evaluated
    /// against a call's shape, and a `path_glob` that fails to compile
    /// would silently protect nothing; both refuse, with the rule named.
    /// The outcome needs no check here: serde already refused a value
    /// outside `allow`/`ask`/`deny` at parse, with the field and value
    /// named.
    ///
    /// # Errors
    ///
    /// Returns the problem's description for a rule with zero or two
    /// matcher keys, or a `path_glob` the glob compiler rejects — the
    /// configuration's loader turns it into its own load-time error.
    pub fn validate(&self) -> Result<(), String> {
        Self::validate_rules("[permissions]", &self.rules)?;
        if let Some(selected) = &self.preset
            && !self.presets.contains_key(selected)
        {
            let available: Vec<&str> = self.presets.keys().map(String::as_str).collect();
            return Err(format!(
                "[permissions] names preset {selected:?} but none is configured — \
                 available: {}",
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            ));
        }
        for (name, preset) in &self.presets {
            Self::validate_rules(&format!("[permissions] preset {name:?}"), &preset.rules)?;
        }
        Ok(())
    }

    /// The rule-shape half of validation, shared by the top-level list
    /// and every preset.
    ///
    /// # Errors
    ///
    /// Returns the problem's description for a rule with zero or two
    /// matcher keys, an empty matcher payload (a matcher that names
    /// nothing), or a `path_glob` the glob compiler rejects.
    fn validate_rules(scope: &str, rules: &[PermissionRuleConfig]) -> Result<(), String> {
        for (position, rule) in rules.iter().enumerate() {
            let named = rule.rule_id(position);
            let Some(key) = rule.matcher_key() else {
                return Err(format!(
                    "{scope} rule {named:?} names no matcher — or names two: give it \
                     exactly one of tool, shell_prefix, or path_glob"
                ));
            };
            let empty_payload = match (&rule.tool, &rule.shell_prefix, &rule.path_glob) {
                (Some(tool), _, _) if tool.trim().is_empty() => Some("tool"),
                (_, Some(prefix), _) if prefix.trim().is_empty() => Some("shell_prefix"),
                (_, _, Some(glob)) if glob.trim().is_empty() => Some("path_glob"),
                _ => None,
            };
            if let Some(field) = empty_payload {
                return Err(format!(
                    "{scope} rule {named:?} carries an empty {field} matcher — it \
                     names nothing, so it is refused"
                ));
            }
            if key == "path_glob"
                && let Some(pattern) = &rule.path_glob
                && let Err(why) = PathGlob::new(pattern)
            {
                return Err(format!(
                    "{scope} rule {named:?} carries path_glob = {pattern:?} that is \
                     not a well-formed glob: {why}"
                ));
            }
        }
        Ok(())
    }
}

/// The matcher payload of one configured rule, projected for the engine.
///
/// The configuration spells matchers as plain strings and this crate owns
/// both sides of that projection — the schema here, the compiled
/// [`Matcher`] there — so `MatcherParts` is the halfway point the
/// conversion walks. Exactly one variant is ever constructed — validation
/// refuses anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatcherParts {
    /// An exact tool-name matcher.
    ///
    /// Carries the registered name verbatim; the engine compares it
    /// case-sensitively against the call's tool.
    Tool(String),

    /// A shell verb-prefix matcher.
    ///
    /// Carries the prefix verbatim, segmentation and trimming left to the
    /// engine's own derivation.
    ShellPrefix(String),

    /// A path-glob matcher, pattern verbatim as written.
    ///
    /// Load-time validation compiled it with the engine's settings, so
    /// this string is known-good; the engine compiles it again when the
    /// conversion runs, where the compiled form lives.
    PathGlob(String),
}

/// One named rule bundle: a `[permissions.presets.<name>]` table.
///
/// A preset is a reusable set of rules a session selects by name —
/// the same shapes as the top-level list, joined into the config layer
/// after it when selected.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preset {
    /// The preset's rules, in declaration order — the order the
    /// engine resolves them in when the preset is selected.
    ///
    /// Same shapes as the top-level list; validation refuses the same
    /// malformations, scoped by the preset's name.
    pub rules: Vec<PermissionRuleConfig>,
}

impl Preset {
    /// Compile the preset's rules into engine rules, prefixing every id
    /// with the preset name so the audit trail names where each rule
    /// came from.
    #[must_use]
    pub fn engine_rules(&self, name: &str) -> Vec<Rule> {
        PermissionsConfig {
            rules: self.rules.clone(),
            preset: None,
            presets: BTreeMap::new(),
        }
        .engine_rules_with_prefix(&format!("preset.{name}."))
    }
}

/// What one "allow always" answer remembers — typed data derived from
/// the call, never a display string.
///
/// For a Bash ask, [`RememberRules::Segments`] carries one arity-trimmed
/// verb prefix per *writer* command segment — reader segments mint
/// nothing, on either side of the sheet: the carve-out runs them free
/// on their own, so a rule for them records nothing the approval was
/// about — plus the exact text of any segment too complex to prefix
/// safely (substitutions, redirections, control flow). Approving
/// `cargo test -p dch-tui | tail -5` remembers `cargo test` — `tail`
/// rides the reader carve-out, free without a blessing — so every
/// later variant of the writer — different flags, different packages —
/// runs without asking again; an
/// `echo "EXIT: $?"` trailer is remembered as that exact text alone
/// and can never ride an `echo` verb. A multi-line script is remembered
/// per line. A write-family ask remembers [`RememberRules::PathGlobs`] —
/// the scopes the targets live under. Every other tool remembers
/// [`RememberRules::Tool`] — the tool itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RememberRules {
    /// The whole tool, remembered and matched by name.
    ///
    /// The ask's tool as the gate saw it — `Write`, `WebFetch`,
    /// `Jobs` — carried verbatim and matched verbatim: one approval
    /// covers every later ask of that tool, whatever its arguments,
    /// and nothing beyond it. Every tool that is not Bash lands
    /// here; the prefix arithmetic is a shell-only refinement.
    Tool(String),

    /// A Bash ask's remember set: one arity-trimmed verb prefix per
    /// *writer* command segment (reader segments mint nothing, on
    /// either side of the sheet — the carve-out runs them free on
    /// their own), plus the exact text of any segment too
    /// complex to prefix safely (substitutions, redirections, control
    /// flow, a quoted first word). Coverage is all-or-nothing across
    /// the whole set: every segment of a later ask must be remembered,
    /// and a complex segment is remembered as its exact text, so
    /// identical reruns run free while variants re-ask.
    Segments {
        /// Verb prefixes, one per clean command segment.
        ///
        /// One arity-trimmed verb per clean segment — `cargo test`
        /// from `cargo test --workspace` — so the same work with
        /// different flags re-reads the same rule, while a segment
        /// outside the set re-asks.
        prefixes: Vec<String>,

        /// Exact normalized texts of complex segments — substitutions,
        /// redirections, control flow, a quoted first word — remembered
        /// as themselves.
        texts: Vec<String>,
    },

    /// One path glob per distinct scope a write ask's targets live under.
    ///
    /// A target below the working directory's root remembers its
    /// directory's subtree — `crates/dch-loop/**` — so later work in the
    /// same directory runs without asking; a target at the root, or one
    /// outside the working directory, remembers its exact path, because a
    /// subtree glob at the root would be the whole workspace. Coverage is
    /// all-or-nothing across the call's targets: a batch spanning two
    /// directories remembers both scopes, and a later call reaching past
    /// either re-asks.
    PathGlobs(Vec<String>),
}

/// One remembered rule, as the session's permissions file stores it.
///
/// The persisted half of an "allow always" or "always deny"
/// answer: typed data, not a display string, so what the
/// user was told at the press is what later sessions evaluate. The serde
/// spelling is internally tagged and stable — a file written by an older
/// build reads fine, and a newer variant is simply one the older build
/// has never seen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum PermissionRule {
    /// A whole tool, remembered and matched by its registered name.
    ///
    /// The coarsest rule a remember can produce, and the shape every
    /// non-Bash remember takes: one approval covers every later ask
    /// of the tool, whatever the input.
    Tool {
        /// The tool's registered name, as the gate classified it —
        /// `Write`, `WebFetch` — carried verbatim through the
        /// session file and matched exactly on the next ask.
        tool: String,
    },

    /// A shell verb prefix, covering every later command whose
    /// segment begins with these words.
    ///
    /// The fine-grained shape: remembered from the approved command's
    /// *writer* segments — reader segments mint nothing, on either
    /// side of the sheet — so approval of one spelling
    /// of the work covers its later variants — flags and arguments
    /// differ, the verb holds.
    ShellPrefix {
        /// The arity-trimmed verb prefix — `cargo test`, not
        /// `cargo test -p dch-tui` — matched against each segment's
        /// leading words.
        prefix: String,
    },

    /// A path-scoped write remember, covering later writes whose
    /// targets the glob matches.
    ///
    /// The fine-grained shape for the write family, remembered from
    /// where the approved call's targets lived: a directory's subtree
    /// (`crates/dch-loop/**`) for a file below the working directory's
    /// root, the exact file for a root-level or outside-the-workdir
    /// target. Coverage is all-or-nothing across a call's targets, so a
    /// batch reaching past every remembered scope still asks.
    PathGlob {
        /// The glob pattern as written, relative to the working
        /// directory — `crates/**`, `README.md` — matched against each
        /// target's leading path.
        pattern: String,
    },

    /// A command-text rule, covering the identical command text only —
    /// the remember shape for segments too complex to prefix safely
    /// (substitutions, redirections, control flow, multi-line lines).
    /// Exact matching is what makes it safe to remember at all: the
    /// identical rerun runs free, and any variant re-asks.
    CommandText {
        /// The exact normalized command text.
        ///
        /// Normalized the way the segment parser scrubs, so a stored
        /// remember compares equal to a later identical segment byte
        /// for byte, and every variant of the wording re-asks.
        text: String,
    },
}

impl PermissionRule {
    /// The phrase the sheet and the `/permissions` list show for this
    /// rule — what an approval of it covers, in the session's own
    /// vocabulary.
    ///
    /// The same words everywhere the rule surfaces, so what the user was
    /// told at the `a`-press is what the list shows later; there is no
    /// second spelling to drift.
    #[must_use]
    pub fn remember_phrase(&self) -> String {
        match self {
            Self::Tool { tool } => format!("every future {tool} call"),
            Self::ShellPrefix { prefix } => format!("every future `{prefix}` run"),
            Self::PathGlob { pattern } => format!("writes under `{pattern}`"),
            Self::CommandText { .. } => "the exact command".to_string(),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::vocabulary::PermissionOutcome;

    #[test]
    fn a_permissions_section_refuses_unknown_fields() {
        // A typo'd key inside `[permissions]` must fail the parse, not
        // silently disarm the section: `ruls` for `rules` would load
        // an empty section — pure matrix — and every configured rule
        // would quietly never exist.
        let err =
            serde_json::from_str::<PermissionsConfig>(r#"{"rules": [], "ruls": []}"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown"),
            "the parse names the unknown field: {err}"
        );
        let honest = serde_json::from_str::<PermissionsConfig>(r#"{"rules": []}"#).unwrap();
        assert_eq!(
            PermissionsConfig::default(),
            honest,
            "the empty honest section stays parseable"
        );
    }

    #[test]
    fn a_preset_table_refuses_unknown_fields() {
        // A typo'd key inside a preset table must fail the parse, not
        // silently produce an empty preset: an empty rule list is a
        // valid preset, so the session would otherwise run with none
        // of the rules the user wrote and no message naming the miss.
        let err = serde_json::from_str::<Preset>(r#"{"rules": [], "rulz": []}"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown"),
            "the parse names the unknown field: {err}"
        );
        let honest = serde_json::from_str::<Preset>(r#"{"rules": []}"#).unwrap();
        assert_eq!(Preset::default(), honest, "an empty rules list stays valid");
    }

    #[test]
    fn a_rule_with_an_empty_matcher_payload_is_refused() {
        // A matcher that names nothing — `shell_prefix = ""`, a blank
        // tool or glob — is a written rule that can never fire: the
        // prefix arm demands a non-empty verb on a clean segment, so
        // the degenerate value loads as dead weight at best and, on
        // the allow side, waved complex payloads through. Load-time
        // validation refuses it, the same refusal a bad glob earns.
        let config = |rule: PermissionRuleConfig| PermissionsConfig {
            rules: vec![rule],
            preset: None,
            presets: BTreeMap::new(),
        };
        let refused = [
            PermissionRuleConfig {
                id: None,
                outcome: PermissionRuleOutcome::Allow,
                tool: Some(String::new()),
                shell_prefix: None,
                path_glob: None,
            },
            PermissionRuleConfig {
                id: None,
                outcome: PermissionRuleOutcome::Allow,
                tool: None,
                shell_prefix: Some(String::new()),
                path_glob: None,
            },
            PermissionRuleConfig {
                id: None,
                outcome: PermissionRuleOutcome::Allow,
                tool: None,
                shell_prefix: None,
                path_glob: Some(String::new()),
            },
        ];
        for rule in refused {
            let err = config(rule).validate().unwrap_err();
            assert!(err.contains("empty"), "the empty payload is named: {err}");
        }
        let honest = config(PermissionRuleConfig {
            id: None,
            outcome: PermissionRuleOutcome::Allow,
            tool: None,
            shell_prefix: Some("cargo test".to_string()),
            path_glob: None,
        });
        honest.validate().unwrap();
    }

    fn preset_config() -> PermissionsConfig {
        let mut presets = BTreeMap::new();
        presets.insert(
            "rust-dev".to_string(),
            Preset {
                rules: vec![PermissionRuleConfig {
                    id: Some("preset-test".to_string()),
                    outcome: PermissionRuleOutcome::Deny,
                    tool: None,
                    shell_prefix: Some("git push".to_string()),
                    path_glob: None,
                }],
            },
        );
        PermissionsConfig {
            rules: Vec::new(),
            preset: Some("rust-dev".to_string()),
            presets,
        }
    }

    #[test]
    fn a_selected_preset_joins_the_config_layer_with_prefixed_ids() {
        let rules = preset_config().engine_rules();
        assert_eq!(rules.len(), 1, "the selected preset's rules compile");
        assert_eq!(rules[0].id, "preset.rust-dev.preset-test");
        assert_eq!(rules[0].outcome, PermissionOutcome::Block);
        assert!(matches!(&rules[0].matcher, Matcher::ShellPrefix(p) if p == "git push"));
    }

    #[test]
    fn no_preset_selected_compiles_nothing() {
        let mut config = preset_config();
        config.preset = None;
        assert!(
            config.engine_rules().is_empty(),
            "unselected presets never reach the engine"
        );
    }

    #[test]
    fn validation_refuses_a_missing_selected_preset_by_name() {
        let mut config = preset_config();
        config.preset = Some("nope".to_string());
        let why = config.validate().unwrap_err();
        assert!(
            why.contains("rust-dev"),
            "the available names are listed: {why}"
        );
    }
}
