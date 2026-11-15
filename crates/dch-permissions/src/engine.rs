//! The permission policy engine: ordered rules over one pure evaluation.
//!
//! Every permission verdict in dch is the product of this module: a call's
//! normalized shape is matched against three layers — the user's configured
//! rules, the session's remembered approvals, and the built-in mode ×
//! category matrix — and the first layer with something to say decides.
//! The engine is pure and synchronous: it reads no configuration, no
//! filesystem, and no clock, so a verdict is a function of its inputs and
//! nothing else. The gate that feeds it (dch-loop) derives the
//! [`CallShape`] from a dispatch; the host that persists remembers and
//! config rules converts them into the rule types here.

use crate::mode::PermissionMode;
use crate::rule::Matcher;
use crate::rule::PathGlob;
use crate::rule::Rule;
use crate::shape::CommandSegment;
use crate::vocabulary::PermissionOutcome;
use crate::vocabulary::ToolCategory;
use crate::vocabulary::decide;

/// A call reduced to the axes the permission system reads.
///
/// The gate derives this from a dispatch — the parsers that split a shell
/// command into segments or resolve a write target live beside it, not
/// here — and the engine matches rules against it. Nothing else about the
/// call exists from the policy's point of view: two calls with the same
/// shape are the same call.
#[derive(Debug, Clone)]
pub struct CallShape {
    /// The call's registered tool name, exact and case-sensitive.
    ///
    /// Tool matchers and whole-tool coverage compare against it verbatim;
    /// every other axis on the shape hangs beside it.
    pub tool: String,

    /// The tool's category, from the classification table.
    ///
    /// Only the built-in matrix reads it — config and remembered rules
    /// deliberately cannot match on it — so the matrix stays the one
    /// place category vocabulary lives.
    pub category: ToolCategory,

    /// Whether the call is session furniture — the todo list, the question
    /// tool, the jobs table's reads — which runs free in every mode.
    ///
    /// Derived by the gate from the tool's name and operation; the engine
    /// takes the verdict as given.
    pub furniture: bool,

    /// Whether a shell command only looks (the reader-gate pass).
    ///
    /// A reader command auto-allows in every mode, `ReadOnly` included; the flag
    /// is the reader gate's whole output.
    pub shell_reads_only: bool,

    /// One segment per command text — every command across newlines and
    /// control operators, readers included. Each carries its verb prefix
    /// (clean segments) or marks itself complex (substitutions,
    /// redirections, control flow, a quoted first word), which is
    /// remembered as exact text.
    ///
    /// Empty for every non-shell call.
    pub segments: Vec<CommandSegment>,

    /// The resolved write targets, relative to the working directory.
    ///
    /// One entry per file the call would overwrite — a `Write` or `Edit`
    /// names one, a `MultiEdit` names its batch. Empty for every
    /// non-write call.
    pub write_targets: Vec<String>,
}

impl CallShape {
    /// The minimal shape for a tool call with no shell or write detail.
    ///
    /// A read-tool call is exactly this; the gate starts here and fills the
    /// shell and write axes only where the tool carries them.
    #[must_use]
    pub fn of(tool: impl Into<String>, category: ToolCategory) -> Self {
        Self {
            tool: tool.into(),
            category,
            furniture: false,
            shell_reads_only: false,
            segments: Vec::new(),
            write_targets: Vec::new(),
        }
    }
}

/// The session's remembered approvals, as the engine reads them.
///
/// This is the memory the "allow always" answers accumulate, projected onto
/// the engine's vocabulary: whole-tool names, shell verb prefixes, exact
/// command texts, and path globs. Coverage here is the whole question the
/// resolver would otherwise ask again.
#[derive(Debug, Clone, Default)]
pub struct RememberedRules {
    /// Whole-tool remembers, matched by exact registered name.
    ///
    /// The v2 permissions file's only vocabulary; path-scoped approvals
    /// land in `path_globs` instead, and a name here still covers every
    /// write the tool makes.
    pub tools: Vec<String>,

    /// Remembered shell verb prefixes, one per approved clean command
    /// segment.
    ///
    /// Coverage requires every clean segment of the new call to appear
    /// here, so the set grows by one entry per distinct verb an "allow
    /// always" approved. Reader segments are remembered here too — they
    /// are part of the chain the user approved — though the reader
    /// gate would allow them without any rule.
    pub shell_prefixes: Vec<String>,

    /// Remembered exact command texts — the complex segments
    /// (substitutions, redirections, control flow, multi-line lines,
    /// a quoted first word) that no verb prefix can safely summarize.
    /// Coverage matches a later segment by its exact normalized text,
    /// so the identical rerun runs free and any variant re-asks.
    pub command_texts: Vec<String>,

    /// Remembered write-path globs.
    ///
    /// A directory-scoped approval records its subtree here; coverage
    /// requires every target of the new call to match at least one of
    /// them.
    pub path_globs: Vec<PathGlob>,
}

impl RememberedRules {
    /// An empty memory — the fresh-session state, and headless's always.
    ///
    /// A host without a permissions file hands this to the engine; every
    /// coverage check answers no and every askable call asks.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether the memory covers `shape` — every axis the call touches is
    /// remembered.
    ///
    /// Coverage is all-or-nothing per call, exactly as an "allow always"
    /// answer meant it: a shell call runs free only when **every**
    /// segment is remembered — each clean segment by its verb prefix or
    /// its exact text, each complex segment by its exact text alone —
    /// or a whole-tool remember covers the call outright, the same
    /// precedence the config layer gives a tool allow; a write
    /// call runs free only when **every** target is remembered, whether
    /// by glob or by a whole-tool rule that predates path scoping; every
    /// other tool runs free on its own name. A call with nothing
    /// remembered on its active axis is not covered — the caller asks.
    #[must_use]
    pub fn covers(&self, shape: &CallShape) -> bool {
        let tool_covered = self.tools.contains(&shape.tool);
        if !shape.segments.is_empty() {
            return tool_covered
                || shape.segments.iter().all(|segment| {
                    if segment.complex {
                        self.command_texts.iter().any(|text| text == &segment.text)
                    } else {
                        self.shell_prefixes.contains(&segment.prefix)
                            || self.command_texts.iter().any(|text| text == &segment.text)
                    }
                });
        }
        if !shape.write_targets.is_empty() {
            let globs: Vec<&PathGlob> = self.path_globs.iter().collect();
            return tool_covered || coverage_over_globs(&shape.write_targets, &globs);
        }
        tool_covered
    }
}

/// Whether every target is matched by at least one of `globs`.
///
/// Coverage for one axis of a call: an empty glob set covers nothing, and
/// a batch passes only when all of its targets land inside the remembered
/// or configured paths.
fn coverage_over_globs(targets: &[String], globs: &[&PathGlob]) -> bool {
    !globs.is_empty()
        && targets
            .iter()
            .all(|target| globs.iter().any(|glob| glob.matches(target)))
}

/// What one decision is and which rule made it.
///
/// The `rule_id` is the audit trail: a config deny names its rule
/// (`config:no-env-writes`), the remembered layers name themselves
/// (`remembered:deny`, `remembered`), the built-ins name the gate
/// (`builtin:reader-gate`, `builtin:matrix`) — every verdict names the
/// layer that produced it, so a transcript, a log, or a dry run can
/// answer "why did this run" without re-deriving the policy. An allow
/// reached through joint rule coverage names the layer alone
/// (`config:allow`) — no single rule covered the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The verdict the call lands under.
    ///
    /// The same vocabulary the matrix and the loopctl primitive speak, so
    /// a decision maps onto `PermissionCheck` without translation.
    pub outcome: PermissionOutcome,

    /// The identity of the rule that decided, in `layer:rule` spelling.
    ///
    /// `config:no-env-writes`, `remembered:deny`, `remembered`,
    /// `builtin:reader-gate`, `builtin:matrix` — the audit trail a log
    /// line or a transcript row carries so a verdict can be traced to
    /// its layer without re-running the policy. A deny always names the
    /// deciding rule; an allow reached through joint rule coverage
    /// names the layer (`config:allow`), since no single rule covered
    /// the call.
    pub rule_id: String,
}

/// The remembered decisions one evaluation consults: what the session
/// has been told to always allow, and what to never run.
///
/// Deny is checked before allow everywhere it exists — the user's
/// "never" outranks their "always" — and deny is the one remembered
/// layer that pierces `read-only`: it can only tighten, and a user who
/// banned a read means never run it, even in a mode that reads freely.
#[derive(Debug, Clone, Default)]
pub struct MemoryView {
    /// The always-allow list, pure coverage: a call runs free only
    /// when every axis item it touches is covered here.
    pub allowed: RememberedRules,

    /// The always-deny list, any-match tightening: one covered item
    /// anywhere on the call refuses it, in every mode.
    pub denied: RememberedRules,
}

impl MemoryView {
    /// A view over two empty lists — nothing remembered at all.
    ///
    /// The builders' fallback when a host wires no memory: every
    /// coverage check answers no, every deny check answers no, and
    /// every askable call asks.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Whether any of a deny list's rules applies to `shape`.
///
/// Tightening is any-match on purpose — one banned target in a batch,
/// one banned verb chained behind an approved one, is enough for the
/// user's "never" to speak. A tool rule matches the call's tool; a
/// prefix rule matches when the call's prefix set carries it; a glob
/// rule matches when any write target lands under it.
fn denied_applies(denied: &RememberedRules, shape: &CallShape) -> bool {
    if denied.tools.contains(&shape.tool) {
        return true;
    }
    for segment in &shape.segments {
        if denied
            .command_texts
            .iter()
            .any(|text| text == &segment.text)
        {
            return true;
        }
        if !segment.complex && denied.shell_prefixes.contains(&segment.prefix) {
            return true;
        }
    }
    let globs: Vec<&PathGlob> = denied.path_globs.iter().collect();
    shape
        .write_targets
        .iter()
        .any(|target| globs.iter().any(|glob| glob.matches(target)))
}

/// Evaluate a call against the full policy: the remembered deny list,
/// then config denies, then — outside `read-only` — the loosening
/// layers, then the built-in matrix.
///
/// Precedence is the contract, and it starts with the user's "never": a
/// remembered deny is checked first, in every mode, because "never ask
/// again" means the call never runs — it cannot be outranked by the
/// mode's posture or by a config allow. A config deny is next (same
/// any-match, same every-mode reach), and only then do the loosening
/// layers open: under `auto`/`interactive` a config `ask` tightens, a
/// config `allow` set loosens through full coverage, and the remembered
/// allow list covers its own; under [`PermissionMode::ReadOnly`] the
/// loosening layers are skipped wholesale — read-only's "touches
/// nothing" contract outranks everything the session or the user has
/// taught the gate. The built-in matrix — with the furniture and
/// reader-gate carve-outs ahead of it — decides whatever the layers
/// above left open.
#[must_use]
pub fn evaluate(
    mode: PermissionMode,
    config_rules: &[Rule],
    remembered: &MemoryView,
    shape: &CallShape,
) -> Decision {
    // The user's remembered "never" pierces every mode.
    if denied_applies(&remembered.denied, shape) {
        return Decision {
            outcome: PermissionOutcome::Block,
            rule_id: "remembered:deny".to_string(),
        };
    }
    for rule in config_rules {
        if rule.outcome == PermissionOutcome::Block && matcher_applies(rule, shape) {
            return Decision {
                outcome: rule.outcome,
                rule_id: format!("config:{}", rule.id),
            };
        }
    }
    if mode != PermissionMode::ReadOnly {
        for rule in config_rules {
            if rule.outcome == PermissionOutcome::Ask && matcher_applies(rule, shape) {
                return Decision {
                    outcome: rule.outcome,
                    rule_id: format!("config:{}", rule.id),
                };
            }
        }
        if config_allows(config_rules, shape) {
            return Decision {
                outcome: PermissionOutcome::Allow,
                rule_id: "config:allow".to_string(),
            };
        }
        if remembered.allowed.covers(shape) {
            return Decision {
                outcome: PermissionOutcome::Allow,
                rule_id: "remembered".to_string(),
            };
        }
    }
    if shape.furniture {
        return Decision {
            outcome: PermissionOutcome::Allow,
            rule_id: "builtin:furniture".to_string(),
        };
    }
    if shape.shell_reads_only {
        return Decision {
            outcome: PermissionOutcome::Allow,
            rule_id: "builtin:reader-gate".to_string(),
        };
    }
    Decision {
        outcome: decide(mode, shape.category),
        rule_id: "builtin:matrix".to_string(),
    }
}

/// Whether one tightening rule — `deny` or `ask` — applies to `shape`.
///
/// Tightening is any-match on purpose: one bad target in a batch, one
/// writer segment chained behind an approved verb, is enough for the rule
/// to speak. Loosening (allow) never takes this path — it needs the full
/// coverage [`config_allows`] computes.
fn matcher_applies(rule: &Rule, shape: &CallShape) -> bool {
    match &rule.matcher {
        Matcher::Tool(name) => *name == shape.tool,
        Matcher::ShellPrefix(prefix) => shape
            .segments
            .iter()
            .any(|segment| !segment.complex && segment.prefix == *prefix),
        Matcher::PathGlob(glob) => shape.write_targets.iter().any(|t| glob.matches(t)),
    }
}

/// Whether the config's allow rules together cover `shape`.
///
/// The same all-or-nothing coverage a remember means, applied to the
/// config's allow rules: a whole-tool allow covers the call outright
/// (the broad lever, named by the user), and otherwise **every**
/// command segment must be covered by a prefix rule — a complex
/// segment is never covered, because no prefix matcher can name what
/// it will run, so a covered prefix cannot carry a complex payload:
/// the call falls through to the ask layer, the same exact-text
/// discipline the remembered layer enforces. Every write target must
/// likewise be matched by an allow glob. A call with no shell or
/// write detail is covered only by its tool's own allow rule.
fn config_allows(config_rules: &[Rule], shape: &CallShape) -> bool {
    let allows = |rule: &Rule| rule.outcome == PermissionOutcome::Allow;
    let tool_covered = config_rules.iter().any(|rule| {
        allows(rule) && matches!(&rule.matcher, Matcher::Tool(name) if *name == shape.tool)
    });
    if !shape.segments.is_empty() {
        return tool_covered
            || shape.segments.iter().all(|segment| {
                config_rules.iter().any(|rule| {
                    allows(rule)
                        && matches!(
                            &rule.matcher,
                            Matcher::ShellPrefix(q) if !segment.complex && q == &segment.prefix
                        )
                })
            });
    }
    if !shape.write_targets.is_empty() {
        let globs: Vec<&PathGlob> = config_rules
            .iter()
            .filter_map(|rule| match &rule.matcher {
                Matcher::PathGlob(glob) if allows(rule) => Some(glob),
                _ => None,
            })
            .collect();
        return tool_covered || coverage_over_globs(&shape.write_targets, &globs);
    }
    tool_covered
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
    use crate::mode::PermissionMode::{Auto, Interactive, ReadOnly};
    use crate::rule::Matcher;
    use crate::rule::PathGlob;
    use crate::rule::Rule;
    use crate::vocabulary::PermissionOutcome::{Allow, Ask, Block};
    use crate::vocabulary::ToolCategory::{
        FileRead, FileWrite, Meta, Network, ShellExecute, Unclassified,
    };
    use crate::vocabulary::tool_category;

    fn rule(id: &str, outcome: PermissionOutcome, matcher: Matcher) -> Rule {
        Rule {
            id: id.to_string(),
            outcome,
            matcher,
        }
    }

    fn tool_rule(id: &str, outcome: PermissionOutcome, tool: &str) -> Rule {
        rule(id, outcome, Matcher::Tool(tool.to_string()))
    }

    fn prefix_rule(id: &str, outcome: PermissionOutcome, prefix: &str) -> Rule {
        rule(id, outcome, Matcher::ShellPrefix(prefix.to_string()))
    }

    fn glob_rule(id: &str, outcome: PermissionOutcome, pattern: &str) -> Rule {
        rule(
            id,
            outcome,
            Matcher::PathGlob(PathGlob::new(pattern).unwrap()),
        )
    }

    fn bash_shape(command_prefixes: &[&str]) -> CallShape {
        CallShape {
            segments: command_prefixes
                .iter()
                .map(|p| CommandSegment {
                    text: (*p).to_string(),
                    prefix: (*p).to_string(),
                    complex: false,
                })
                .collect(),
            ..CallShape::of("Bash", ShellExecute)
        }
    }

    fn write_shape(targets: &[&str]) -> CallShape {
        CallShape {
            write_targets: targets.iter().map(|t| (*t).to_string()).collect(),
            ..CallShape::of("Write", FileWrite)
        }
    }

    fn empty_remembered() -> MemoryView {
        MemoryView::empty()
    }

    #[test]
    fn one_quoted_verbs_remember_covers_no_other_quoted_verb() {
        // A quoted first word is a complex segment, so no prefix — not
        // even the placeholder the old mint could record — covers it:
        // coverage runs through the exact text alone, a different
        // quoted verb is a different command, and only the identical
        // rerun runs free.
        let shape_for = |command: &str| CallShape {
            segments: crate::shape::command_segments(command),
            ..CallShape::of("Bash", ShellExecute)
        };
        let mut remembered = RememberedRules {
            shell_prefixes: vec!["\u{0}".to_string()],
            ..RememberedRules::empty()
        };
        assert!(
            !remembered.covers(&shape_for("'curl' https://evil.invalid/x")),
            "a different quoted verb is a different command — it re-asks"
        );
        assert!(
            !remembered.covers(&shape_for("'cat' notes.txt")),
            "a quoted-verb segment matches only its exact text, never a prefix"
        );
        remembered.command_texts.push("'cat' notes.txt".to_string());
        assert!(
            remembered.covers(&shape_for("'cat' notes.txt")),
            "the identical rerun runs free through its exact text"
        );
    }

    #[test]
    fn empty_layers_land_on_the_matrix_for_every_cell() {
        // With no config rules and nothing remembered, the engine is the
        // matrix: all 18 cells land exactly where decide() puts them.
        for mode in [Auto, ReadOnly, Interactive] {
            for category in [
                FileRead,
                FileWrite,
                ShellExecute,
                Network,
                Meta,
                Unclassified,
            ] {
                let shape = CallShape::of("SomeTool", category);
                let decision = evaluate(mode, &[], &empty_remembered(), &shape);
                assert_eq!(
                    decision,
                    Decision {
                        outcome: decide(mode, category),
                        rule_id: "builtin:matrix".to_string(),
                    },
                    "{mode:?} × {category:?} with empty layers is the matrix"
                );
            }
        }
    }

    #[test]
    fn config_deny_and_ask_fire_on_any_match_in_declaration_order() {
        // A deny on one bad target denies a whole batch; the first matching
        // tightening rule in declaration order wins.
        let rules = [
            glob_rule("first", Block, "**/.env"),
            tool_rule("second", Ask, "Write"),
        ];
        let shape = write_shape(&["README.md", ".env"]);
        let decision = evaluate(Auto, &rules, &empty_remembered(), &shape);
        assert_eq!(
            decision,
            Decision {
                outcome: Block,
                rule_id: "config:first".to_string(),
            },
            "the first declared tightening rule wins"
        );
        let prefix_rules = [prefix_rule("chain", Block, "curl")];
        let shell = bash_shape(&["cargo build", "curl"]);
        assert_eq!(
            evaluate(Auto, &prefix_rules, &empty_remembered(), &shell).outcome,
            Block,
            "one writer segment chained behind an approved verb is enough to deny"
        );
    }

    #[test]
    fn config_ask_tightens_auto_and_loosens_nothing() {
        // Auto's matrix allows everything; an ask rule is still asked.
        let rules = [tool_rule("net-ask", Ask, "WebFetch")];
        let shape = CallShape::of("WebFetch", Network);
        assert_eq!(
            evaluate(Auto, &rules, &empty_remembered(), &shape).outcome,
            Ask,
            "a config ask tightens auto"
        );
        // And an unmatched rule leaves the matrix untouched.
        let other = CallShape::of("Read", FileRead);
        assert_eq!(
            evaluate(Auto, &rules, &empty_remembered(), &other).outcome,
            Allow,
        );
    }

    #[test]
    fn config_allow_requires_full_coverage_of_the_active_axis() {
        // A shell allow covers the call only when every non-reader segment
        // is itself allowed; a path allow only when every target is.
        let rules = [
            prefix_rule("test", Allow, "cargo test"),
            glob_rule("crates", Allow, "crates/**"),
        ];
        let single = bash_shape(&["cargo test"]);
        assert_eq!(
            evaluate(Interactive, &rules, &empty_remembered(), &single).outcome,
            Allow,
            "a single covered segment allows"
        );
        let chained = bash_shape(&["cargo test", "curl"]);
        assert_eq!(
            evaluate(Interactive, &rules, &empty_remembered(), &chained).outcome,
            Ask,
            "one uncovered segment falls back to the matrix"
        );
        let inside = write_shape(&["crates/a/src.rs", "crates/b/src.rs"]);
        assert_eq!(
            evaluate(Interactive, &rules, &empty_remembered(), &inside).outcome,
            Allow,
            "targets all under the glob allow"
        );
        let escaping = write_shape(&["crates/a/src.rs", "target/out.rs"]);
        assert_eq!(
            evaluate(Interactive, &rules, &empty_remembered(), &escaping).outcome,
            Ask,
            "one target off the glob falls back"
        );
        // A whole-tool allow covers everything the tool does.
        let tool_rules = [tool_rule("all-writes", Allow, "Write")];
        let anywhere = write_shape(&["target/out.rs"]);
        assert_eq!(
            evaluate(Interactive, &tool_rules, &empty_remembered(), &anywhere).outcome,
            Allow,
            "a whole-tool allow rule is as broad as the tool"
        );
    }

    #[test]
    fn read_only_outranks_the_learned_layers() {
        // Read-only's "touches nothing" line holds over config allows and
        // remembered approvals alike; only the builtin layer decides.
        let rules = [glob_rule("wide", Allow, "**")];
        let mut view = MemoryView::empty();
        view.allowed.tools.push("Write".to_string());
        let shape = write_shape(&["src/lib.rs"]);
        let decision = evaluate(ReadOnly, &rules, &view, &shape);
        assert_eq!(
            decision,
            Decision {
                outcome: Block,
                rule_id: "builtin:matrix".to_string(),
            },
            "read-only blocks a write the config and the memory would both allow"
        );
        // Furniture and the reader gate still run free under read_only.
        let mut reader = bash_shape(&["git log"]);
        reader.furniture = false;
        reader.shell_reads_only = true;
        assert_eq!(
            evaluate(ReadOnly, &rules, &view, &reader).outcome,
            Allow,
            "a reader command looks under read-only"
        );
        let note = CallShape {
            furniture: true,
            ..CallShape::of("TodoWrite", Meta)
        };
        assert_eq!(
            evaluate(ReadOnly, &rules, &view, &note).rule_id,
            "builtin:furniture",
            "session furniture stays free under read-only"
        );
    }

    #[test]
    fn a_config_deny_blocks_a_read_under_read_only() {
        // Denies sit ahead of the mode gate: a config deny on `Read`
        // decides under read-only, blocking even the reads the mode's
        // matrix would allow — the opposite of the loosening layers,
        // which read-only skips wholesale.
        let rules = [tool_rule("no-reads", Block, "Read")];
        let shape = CallShape::of("Read", FileRead);
        let decision = evaluate(ReadOnly, &rules, &empty_remembered(), &shape);
        assert_eq!(
            decision,
            Decision {
                outcome: Block,
                rule_id: "config:no-reads".to_string(),
            },
            "the config deny decides under read-only, not the matrix's free read"
        );
    }

    #[test]
    fn remembered_coverage_mirrors_the_ask_semantics() {
        // Shell: every non-reader prefix remembered; an empty prefix set —
        // a multi-line script, metacharacters — is never covered.
        let mut remembered = RememberedRules::empty();
        remembered.shell_prefixes.push("cargo test".to_string());
        let variant = bash_shape(&["cargo test"]);
        assert!(remembered.covers(&variant), "flags and args may differ");
        let chained = bash_shape(&["cargo test", "curl"]);
        assert!(!remembered.covers(&chained), "a new segment re-asks");
        let script = bash_shape(&[]);
        assert!(!remembered.covers(&script), "an empty prefix set asks");

        // Writes: a legacy whole-tool remember covers everything; a path
        // remember covers only its glob.
        let mut legacy = RememberedRules::empty();
        legacy.tools.push("Write".to_string());
        assert!(legacy.covers(&write_shape(&["anywhere/x"])));
        let mut scoped = RememberedRules::empty();
        scoped.path_globs.push(PathGlob::new("crates/**").unwrap());
        assert!(scoped.covers(&write_shape(&["crates/a", "crates/b/c"])));
        assert!(!scoped.covers(&write_shape(&["crates/a", "README.md"])));
        assert!(!scoped.covers(&write_shape(&["README.md"])));

        // Other tools: the tool's own name.
        let mut fetch = RememberedRules::empty();
        fetch.tools.push("WebFetch".to_string());
        assert!(fetch.covers(&CallShape::of("WebFetch", Network)));
        assert!(!fetch.covers(&CallShape::of("Jobs", Meta)));
    }

    #[test]
    fn a_whole_tool_remember_covers_a_segmented_bash_call() {
        // The remembered mirror of the config layer's whole-tool allow:
        // a `Tool{"Bash"}` remember covers every Bash call outright,
        // segmented chains included. The shipped minters never produce
        // one — they mint segments — so an adopted permissions.json is
        // the only route that carries one, and the verdict must honor
        // it exactly as `config_allows` honors the config shape.
        let mut remembered = RememberedRules::empty();
        remembered.tools.push("Bash".to_string());
        let clean = bash_shape(&["cargo test", "cargo build"]);
        assert!(
            remembered.covers(&clean),
            "the whole-tool remember covers the chain by name"
        );
        let complex = CallShape {
            segments: vec![CommandSegment {
                text: "make deploy KEY=$(cat token)".to_string(),
                prefix: String::new(),
                complex: true,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        assert!(
            remembered.covers(&complex),
            "the whole-tool remember covers a complex segment outright"
        );
        let mut other_tool = RememberedRules::empty();
        other_tool.tools.push("Write".to_string());
        assert!(
            !other_tool.covers(&clean),
            "a remember for another tool covers nothing on the Bash axis"
        );
    }

    #[test]
    fn an_empty_shell_prefix_allow_cannot_cover_a_complex_segment() {
        // The degenerate allow — `shell_prefix = ""` — names no verb,
        // so it must never cover a complex segment: complex segments
        // carry an empty prefix, and the exact-text discipline holds
        // that no prefix matcher can name what one will run. With the
        // rule present the substitution-carrying call still asks, and
        // a real prefix still covers its clean chain — the tightening
        // arm matches the remembered layer's and matcher_applies'
        // guard, not fights it.
        let degenerate = vec![rule("empty", Allow, Matcher::ShellPrefix(String::new()))];
        let complex = CallShape {
            segments: vec![CommandSegment {
                text: "echo \"EXIT: $(cat ~/.secrets/token)\"".to_string(),
                prefix: String::new(),
                complex: true,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let decision = evaluate(Interactive, &degenerate, &MemoryView::empty(), &complex);
        assert_eq!(
            decision,
            Decision {
                outcome: Ask,
                rule_id: "builtin:matrix".to_string(),
            },
            "the degenerate allow cannot wave a complex payload through"
        );
        let real = vec![rule(
            "builds",
            Allow,
            Matcher::ShellPrefix("cargo build".to_string()),
        )];
        let clean = CallShape {
            segments: vec![CommandSegment {
                text: "cargo build --workspace".to_string(),
                prefix: "cargo build".to_string(),
                complex: false,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let decision = evaluate(Interactive, &real, &MemoryView::empty(), &clean);
        assert_eq!(
            decision,
            Decision {
                outcome: Allow,
                rule_id: "config:allow".to_string(),
            },
            "a real prefix keeps its clean-chain coverage"
        );
    }

    #[test]
    fn remembered_auto_allow_records_the_remembered_rule_id() {
        let mut view = MemoryView::empty();
        view.allowed.shell_prefixes.push("cargo test".to_string());
        let decision = evaluate(Interactive, &[], &view, &bash_shape(&["cargo test"]));
        assert_eq!(
            decision,
            Decision {
                outcome: Allow,
                rule_id: "remembered".to_string(),
            },
        );
    }

    #[test]
    fn a_config_allow_does_not_cover_a_complex_segment() {
        // The config allow layer's coverage is exact: a prefix rule
        // covers the clean segments it names, and a complex segment —
        // substitution, redirection, control flow — is never covered,
        // because no prefix matcher can name what it will run. A
        // covered prefix cannot carry a complex payload: the call
        // falls through to the ask layer, the same exact-text
        // discipline the remembered layer enforces.
        let config = vec![rule(
            "builds",
            Allow,
            Matcher::ShellPrefix("cargo build".to_string()),
        )];
        let shape = CallShape {
            segments: vec![
                CommandSegment {
                    text: "cargo build --workspace".to_string(),
                    prefix: "cargo build".to_string(),
                    complex: false,
                },
                CommandSegment {
                    text: "make deploy KEY=$(cat token)".to_string(),
                    prefix: String::new(),
                    complex: true,
                },
            ],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let decision = evaluate(Interactive, &config, &MemoryView::empty(), &shape);
        assert_eq!(
            decision,
            Decision {
                outcome: Ask,
                rule_id: "builtin:matrix".to_string(),
            },
            "the complex segment asks — the allow cannot carry it"
        );
    }

    #[test]
    fn a_config_allow_still_covers_the_clean_chain() {
        // The coverage fix narrows only the complex pass-through: a
        // clean chain a rule names still resolves without prompting.
        let config = vec![rule(
            "builds",
            Allow,
            Matcher::ShellPrefix("cargo build".to_string()),
        )];
        let shape = CallShape {
            segments: vec![CommandSegment {
                text: "cargo build --workspace".to_string(),
                prefix: "cargo build".to_string(),
                complex: false,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let decision = evaluate(Interactive, &config, &MemoryView::empty(), &shape);
        assert_eq!(
            decision,
            Decision {
                outcome: Allow,
                rule_id: "config:allow".to_string(),
            },
        );
    }

    #[test]
    fn a_whole_tool_allow_covers_a_complex_segment_by_name() {
        // A whole-tool allow is the broad lever, named as such: the
        // user allowed every future Bash call, complex payloads
        // included — the call is covered outright.
        let config = vec![rule("all-bash", Allow, Matcher::Tool("Bash".to_string()))];
        let shape = CallShape {
            segments: vec![CommandSegment {
                text: "make deploy KEY=$(cat token)".to_string(),
                prefix: String::new(),
                complex: true,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        let decision = evaluate(Interactive, &config, &MemoryView::empty(), &shape);
        assert_eq!(
            decision,
            Decision {
                outcome: Allow,
                rule_id: "config:allow".to_string(),
            },
        );
    }

    #[test]
    fn path_globs_match_relative_paths_with_literal_components() {
        let subtree = PathGlob::new("crates/**").unwrap();
        assert!(subtree.matches("crates/a/src/lib.rs"));
        assert!(!subtree.matches("cratesX/a"), "a prefix is not a component");
        let children = PathGlob::new("crates/*").unwrap();
        assert!(children.matches("crates/dch"));
        assert!(
            !children.matches("crates/dch/src/lib.rs"),
            "* stays inside one component"
        );
        let dotfile = PathGlob::new("**/.env").unwrap();
        assert!(dotfile.matches("services/api/.env"));
        assert!(dotfile.matches(".env"));
        let exact = PathGlob::new("README.md").unwrap();
        assert!(exact.matches("README.md"));
        assert!(!exact.matches("docs/README.md"));
        assert!(
            PathGlob::new("crates/**[").is_err(),
            "a malformed glob is refused, not silently broad"
        );
    }

    #[test]
    fn every_registered_builtin_tool_classifies_as_the_shape_will() {
        // The engine leans on the classification table for its category;
        // this pins that a builtin name never lands Unclassified by
        // accident, which would silently turn matrix cells into the
        // fail-closed default.
        for name in [
            "Read",
            "ReadFiles",
            "Write",
            "Edit",
            "MultiEdit",
            "Bash",
            "Jobs",
            "FileViewer",
            "Glob",
            "Grep",
            "CodeSearch",
            "Tree",
            "TodoWrite",
            "Submit",
            "WebFetch",
            "AskUserQuestion",
            "LSP",
        ] {
            assert_ne!(
                tool_category(name),
                Unclassified,
                "{name} must classify; the table and the registry are pinned elsewhere"
            );
        }
    }

    #[test]
    fn a_remembered_deny_blocks_silently_before_any_allow() {
        // The user's "never" outranks their "always": the same verb on
        // both lists denies, and the block names the deny rule so the
        // transcript records which decision spoke.
        let mut remembered = MemoryView::empty();
        remembered
            .allowed
            .shell_prefixes
            .push("cargo test".to_string());
        remembered
            .denied
            .shell_prefixes
            .push("cargo test".to_string());
        let decision = evaluate(
            PermissionMode::Interactive,
            &[],
            &remembered,
            &bash_shape(&["cargo test"]),
        );
        assert_eq!(
            decision,
            Decision {
                outcome: Block,
                rule_id: "remembered:deny".to_string(),
            },
            "deny beats allow on the same verb"
        );
    }

    #[test]
    fn a_remembered_deny_command_text_matches_the_exact_segment() {
        // A command-text deny covers the identical complex segment and
        // nothing else: a variant with a different payload re-asks.
        let mut remembered = MemoryView::empty();
        remembered
            .denied
            .command_texts
            .push("cargo build > owned.txt".to_string());
        let shape = CallShape {
            segments: vec![CommandSegment {
                text: "cargo build > owned.txt".to_string(),
                prefix: String::new(),
                complex: true,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        assert!(
            denied_applies(&remembered.denied, &shape),
            "the exact command text is denied"
        );
        let variant = CallShape {
            segments: vec![CommandSegment {
                text: "cargo build > other.txt".to_string(),
                prefix: String::new(),
                complex: true,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        assert!(
            !denied_applies(&remembered.denied, &variant),
            "a different redirect target is not covered by the deny"
        );
    }

    #[test]
    fn a_remembered_deny_is_any_match_across_the_axes() {
        // One banned target in a batch denies the batch; one banned verb
        // chained behind approved work denies the command; a banned tool
        // denies every ask of it.
        let mut remembered = MemoryView::empty();
        remembered
            .denied
            .path_globs
            .push(PathGlob::new("**/.env").unwrap());
        let shape = write_shape(&["README.md", ".env"]);
        assert!(
            denied_applies(&remembered.denied, &shape),
            "one banned target denies the batch"
        );
        remembered.denied.shell_prefixes.push("curl".to_string());
        let chained = bash_shape(&["cargo test", "curl"]);
        assert!(
            denied_applies(&remembered.denied, &chained),
            "a banned verb chained behind approved work denies"
        );
        remembered.denied.tools.push("WebFetch".to_string());
        assert!(
            denied_applies(
                &remembered.denied,
                &CallShape::of("WebFetch", ToolCategory::Network)
            ),
            "a banned tool denies every ask of it"
        );
    }

    #[test]
    fn a_remembered_deny_pierces_read_only_but_loosening_does_not() {
        // Read-only skips the loosening layers wholesale, yet a user's
        // "never" still applies to the one thing the mode allows: reads.
        let mut remembered = MemoryView::empty();
        remembered.denied.tools.push("Read".to_string());
        let read = CallShape::of("Read", ToolCategory::FileRead);
        let decision = evaluate(PermissionMode::ReadOnly, &[], &remembered, &read);
        assert_eq!(
            decision,
            Decision {
                outcome: Block,
                rule_id: "remembered:deny".to_string(),
            },
            "the deny pierces the mode's free reads"
        );
        // And the loosening layers stay skipped: a remembered allow does
        // not unlock a write under read-only.
        let mut loosening = MemoryView::empty();
        loosening.allowed.tools.push("Write".to_string());
        assert_eq!(
            evaluate(
                PermissionMode::ReadOnly,
                &[],
                &loosening,
                &write_shape(&["src/lib.rs"])
            )
            .outcome,
            Block,
            "read-only outranks the memory's allows"
        );
    }
}
