//! The session's remembered decisions: the rules an "allow always"
//! answer records, held live and written through to the host's storage.
//!
//! The memory is the session's own permission state — consulted per
//! dispatch by the policy, recorded by the UI's answers, persisted by a
//! hook the host installs — so a choice outlives the turn it was made
//! in, and a resume restores exactly what its file records.

use std::sync::Arc;
use std::sync::Mutex;

use crate::engine::MemoryView;
use crate::engine::RememberedRules;
use crate::rule::PermissionRule;

/// The session's remembered rules, projected for the policy engine.
///
/// The snapshot closure reads the live memory at evaluation time, so a
/// remember made one call ago covers the very next one. Cheap by
/// contract: the memory is a handful of rules, and cloning the
/// projection costs what the mode-cell read beside it costs.
pub type MemorySnapshot = Arc<dyn Fn() -> MemoryView + Send + Sync>;

/// An empty memory view — nothing allowed, nothing denied.
///
/// The builders' fallback when a host wires no memory: every
/// coverage check answers no, every deny check answers no, and
/// every askable call asks.
#[must_use]
pub fn empty_view() -> crate::engine::MemoryView {
    crate::engine::MemoryView::empty()
}

/// The hook's shape: handed the whole rule list on every change.
///
/// The whole list, never a delta — the host's storage is rewritten
/// in kind at each fire, so the hook stays dumb: replace, not
/// merge. The call runs synchronously inside the remember, so a
/// slow write-through is a slow answer, the cost paid where the
/// choice was made.
pub type PersistHook = Arc<dyn Fn(&[PermissionRule], &[PermissionRule]) + Send + Sync>;

/// The session's remembered permission rules.
///
/// The "allow always" choices as typed rules: a shell approval
/// remembers one verb prefix per clean segment plus the exact text of
/// each complex one, so every later variant of the same work runs
/// without asking again, while every other tool remembers itself. Shared
/// between the resolver — which
/// answers a remembered ask before it ever reaches the display — and
/// the panel — whose allow-always answer records — with an optional
/// persistence hook the host installs to write the list through to
/// the session's own files.
#[derive(Clone, Default)]
pub struct PermissionMemory {
    /// The always-allow rules, in remember order.
    ///
    /// One entry per distinct "allow always" coverage — a whole tool,
    /// one shell verb prefix, one write-path scope — appended as the
    /// answers arrive and never reordered or pruned mid-session: the
    /// order is what persistence round-trips, and membership is all the
    /// coverage checks read. A resume replaces the lists wholesale
    /// ([`PermissionMemory::adopt`]).
    allow_rules: Arc<Mutex<Vec<PermissionRule>>>,

    /// The always-deny rules, same shapes, the other answer.
    ///
    /// A matched deny blocks the call silently, in every mode — the
    /// user's "never" asked once and does not ask again.
    deny_rules: Arc<Mutex<Vec<PermissionRule>>>,

    /// The host's write-through, when one is installed.
    ///
    /// Fired with the whole pair after every remember or forget that
    /// changed it — in the interactive host, the session's own
    /// permissions file — so a choice outlives the process that made
    /// it. `None` in hosts with nowhere to write: the memory still
    /// answers asks, the choices just stay in-process.
    persist: Arc<Mutex<Option<PersistHook>>>,
}

impl PermissionMemory {
    /// Whether a whole-tool ask was answered "allow always".
    ///
    /// The tool rule's exact match: remembered by name, matched by
    /// name — a remembered `Write` covers every later `Write` ask,
    /// and nothing else.
    #[must_use]
    pub fn covers_tool(&self, tool: &str) -> bool {
        self.allow_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|rule| matches!(rule, PermissionRule::Tool { tool: name } if name == tool))
    }

    /// Whether every one of a shell ask's verb prefixes is remembered.
    ///
    /// An empty prefix set — every segment complex, none carrying a
    /// verb — is never covered here: it asks every time.
    #[must_use]
    pub fn covers_prefixes(&self, prefixes: &[String]) -> bool {
        if prefixes.is_empty() {
            return false;
        }
        let rules = self
            .allow_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prefixes.iter().all(|prefix| {
            rules.iter().any(|rule| match rule {
                PermissionRule::ShellPrefix { prefix: remembered } => remembered == prefix,
                PermissionRule::Tool { .. }
                | PermissionRule::PathGlob { .. }
                | PermissionRule::CommandText { .. } => false,
            })
        })
    }

    /// Whether any deny rule names the whole-tool ask.
    ///
    /// Any-match: the user's "never" needs one rule naming the tool.
    #[must_use]
    pub fn denies_tool(&self, tool: &str) -> bool {
        self.deny_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|rule| matches!(rule, PermissionRule::Tool { tool: name } if name == tool))
    }

    /// Record an "allow always" answer's rules and write them through.
    ///
    /// Same-shape rules the deny list already carries are skipped — a
    /// blessing cannot mint over an explicit refusal, and the refusal
    /// is sticky until [`forget_at`](Self::forget_at) — and an
    /// already-remembered rule is a no-op. The write-through fires
    /// only when the list actually grew.
    pub fn remember_rules(&self, rules: &[PermissionRule]) {
        let mintable = Self::uncontradicted(&self.deny_rules, rules);
        self.remember_into(&self.allow_rules, &mintable);
    }

    /// Record an "always deny" answer's rules and write them through.
    ///
    /// Same contract as [`remember_rules`](Self::remember_rules), the
    /// other list — a matched deny blocks silently in every mode, so
    /// the work the rules name never runs nor asks again — with the
    /// skip pointing the other way: same-shape rules the allow list
    /// already carries are skipped, so a chain refusal can never
    /// retract a blessing by association.
    pub fn remember_deny(&self, rules: &[PermissionRule]) {
        let mintable = Self::uncontradicted(&self.allow_rules, rules);
        self.remember_into(&self.deny_rules, &mintable);
    }

    /// What a press on `rules` would actually record — on the deny
    /// side when `deny` is set, the allow side otherwise: the rules
    /// not already present, same shape, in the other list.
    ///
    /// The sheet's remember lines render this, so the scope a press
    /// records is read before the key is pressed, not after; the
    /// record arms apply the same filter through
    /// [`remember_rules`](Self::remember_rules) and
    /// [`remember_deny`](Self::remember_deny), so what the sheet
    /// shows is what the press writes.
    #[must_use]
    pub fn would_record(&self, rules: &[PermissionRule], deny: bool) -> Vec<PermissionRule> {
        let other = if deny {
            &self.allow_rules
        } else {
            &self.deny_rules
        };
        Self::uncontradicted(other, rules)
    }

    /// The rules from `rules` the other list does not already carry —
    /// the cross-list half of the dedupe, shared by both record arms
    /// and the sheet's preview.
    fn uncontradicted(
        other: &Mutex<Vec<PermissionRule>>,
        rules: &[PermissionRule],
    ) -> Vec<PermissionRule> {
        let other = other
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rules
            .iter()
            .filter(|rule| !other.contains(*rule))
            .cloned()
            .collect()
    }

    /// Append new rules to one list, deduped within the list (the
    /// cross-list skip is the caller's), firing the write-through only
    /// on growth.
    fn remember_into(&self, list: &Mutex<Vec<PermissionRule>>, rules: &[PermissionRule]) {
        let mut list = list
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = list.len();
        for rule in rules {
            if !list.contains(rule) {
                list.push(rule.clone());
            }
        }
        if list.len() == before {
            return;
        }
        drop(list);
        self.fire_persist();
    }

    /// Take the rule at `position` out of the memory, write-through and
    /// all.
    ///
    /// The position is an index into the concatenation of the allow
    /// list then the deny list — the positional primitive for a host
    /// that keys removals by index; the shipped `/permissions` sheet
    /// keys its forgets by rendered identity instead (see
    /// [`forget_matching`](Self::forget_matching)). `None` for an
    /// index past both lists. The write-through fires on the removal
    /// like it fires on a growth — the session's permissions file is
    /// rewritten to the post-forget pair — so what the memory forgets
    /// the file forgets with it, and the next resume never adopts the
    /// rule back.
    pub fn forget_at(&self, position: usize) -> Option<PermissionRule> {
        let mut allow = self
            .allow_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut deny = self
            .deny_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = if position < allow.len() {
            allow.remove(position)
        } else {
            let deny_position = position.checked_sub(allow.len())?;
            if deny_position >= deny.len() {
                return None;
            }
            deny.remove(deny_position)
        };
        drop(allow);
        drop(deny);
        self.fire_persist();
        Some(removed)
    }

    /// Remove the first rule equal to `rule` from the named list,
    /// write-through and all.
    ///
    /// The `/permissions` sheet's identity-keyed removal: the press
    /// names a position in a listing rendered earlier, and the memory
    /// can move under it — an adopt replaces the pair wholesale — so
    /// the removal matches the rule the user read, not the position
    /// it rendered at. Nothing matches, nothing is removed; the
    /// write-through fires only when a rule actually left.
    pub fn forget_matching(&self, allow: bool, rule: &PermissionRule) -> Option<PermissionRule> {
        let mut list = if allow {
            self.allow_rules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        } else {
            self.deny_rules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let position = list.iter().position(|candidate| candidate == rule)?;
        let removed = list.remove(position);
        drop(list);
        self.fire_persist();
        Some(removed)
    }

    /// Adopt a rule pair wholesale — a session's own choices at
    /// open, a resumed session's at adoption, a cleared session's
    /// empty none.
    ///
    /// Replacement, not merge: the pair handed over is the memory's
    /// whole truth from the call on, so a resume restores exactly
    /// what its file records and can never accumulate a phantom.
    /// No write-through fires — adoption is not a new choice, and
    /// the file the pair came from is already its record.
    pub fn adopt(&self, allow: Vec<PermissionRule>, deny: Vec<PermissionRule>) {
        *self
            .allow_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = allow;
        *self
            .deny_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = deny;
    }

    /// The always-allow rules, in remember order.
    ///
    /// A snapshot, not a handle: mutations after the call land in
    /// the memory, never in the returned list — the display reads
    /// this, the verdicts read the live cell.
    #[must_use]
    pub fn allow_rules(&self) -> Vec<PermissionRule> {
        self.allow_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The always-deny rules, in remember order.
    ///
    /// The deny list's mirror of [`allow_rules`](Self::allow_rules):
    /// same snapshot semantics, the other decision — what the session
    /// has been told to never run nor ask about.
    #[must_use]
    pub fn deny_rules(&self) -> Vec<PermissionRule> {
        self.deny_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Install the host's write-through, replacing any before it.
    ///
    /// The hook receives the whole pair after every change that
    /// altered it — never on a duplicate, where nothing changed — so
    /// the host's storage mirrors the memory without the memory
    /// knowing where the storage lives.
    pub fn persist_through(&self, persist: PersistHook) {
        *self
            .persist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(persist);
    }

    /// Fire the write-through with both lists, if one is installed.
    ///
    /// Every mutation funnels here after unlocking, so the hook's
    /// contract — whole pair, replace-not-merge — is stated once and
    /// the remember and the forget both inherit it.
    fn fire_persist(&self) {
        if let Some(persist) = self
            .persist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            persist(&self.allow_rules(), &self.deny_rules());
        }
    }

    /// The memory projected for the policy engine.
    ///
    /// Both lists compile into the engine's coverage vocabulary; a rule
    /// whose pattern no longer compiles is skipped with a warning —
    /// one unreadable rule must not silence the rest of the memory.
    #[must_use]
    pub fn snapshot(&self) -> crate::engine::MemoryView {
        crate::engine::MemoryView {
            allowed: compile_list(self.allow_rules()),
            denied: compile_list(self.deny_rules()),
        }
    }
}

/// Compile one rule list into the engine's coverage vocabulary.
///
/// The one place a [`PermissionRule`] becomes a coverage entry — used
/// by the snapshot for both lists, so allow and deny compile under
/// identical settings.
fn compile_list(rules: Vec<PermissionRule>) -> RememberedRules {
    let mut compiled = RememberedRules::empty();
    for rule in rules {
        match rule {
            PermissionRule::Tool { tool } => compiled.tools.push(tool),
            PermissionRule::ShellPrefix { prefix } => {
                compiled.shell_prefixes.push(prefix);
            }
            PermissionRule::CommandText { text } => {
                compiled.command_texts.push(text);
            }
            PermissionRule::PathGlob { pattern } => {
                match crate::rule::PathGlob::new(pattern.clone()) {
                    Ok(glob) => compiled.path_globs.push(glob),
                    Err(why) => tracing::warn!(
                        error = %why,
                        pattern = %pattern,
                        "a remembered permission glob no longer compiles — the \
                         rule is skipped"
                    ),
                }
            }
        }
    }
    compiled
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc
)]
mod tests {
    use super::*;

    fn prefix(p: &str) -> PermissionRule {
        PermissionRule::ShellPrefix {
            prefix: p.to_string(),
        }
    }

    #[test]
    fn a_deny_press_skips_an_already_blessed_verb() {
        // The skip itself, from a live case: the session blessed
        // `cargo build` once; a later chain refusal carrying the same
        // verb records the others and leaves the blessing untouched —
        // deny precedence would make a contradicting rule dead anyway,
        // and the both-lists state is the confusion this avoids.
        let memory = PermissionMemory::default();
        memory.remember_rules(&[prefix("cargo build")]);
        memory.remember_deny(&[prefix("cargo build"), prefix("cargo check")]);
        assert_eq!(
            memory.deny_rules(),
            vec![prefix("cargo check")],
            "the blessed verb never entered the deny list"
        );
        assert_eq!(
            memory.allow_rules(),
            vec![prefix("cargo build")],
            "the blessing is sticky"
        );
    }

    #[test]
    fn an_allow_press_skips_an_already_denied_verb() {
        // The skip points both ways, as structural insurance: today a
        // deny-matched chain never asks again (the gate answers
        // before any press), so the case is unreachable end to end —
        // the guard lives here so an explicit press can never mint a
        // contradiction, whatever the gate's ordering does tomorrow.
        let memory = PermissionMemory::default();
        memory.remember_deny(&[prefix("make ci")]);
        memory.remember_rules(&[prefix("make ci"), prefix("make test")]);
        assert_eq!(
            memory.allow_rules(),
            vec![prefix("make test")],
            "the denied verb never entered the allow list"
        );
        assert_eq!(memory.deny_rules(), vec![prefix("make ci")]);
    }

    #[test]
    fn identical_presses_record_nothing_new() {
        // Idempotence by shape: a repeated press dedupes against its
        // own list, and a press the other list already carries records
        // nothing at all — one decision, one entry.
        let memory = PermissionMemory::default();
        memory.remember_rules(&[prefix("cargo test"), prefix("tail")]);
        memory.remember_rules(&[prefix("cargo test")]);
        assert_eq!(
            memory.allow_rules(),
            vec![prefix("cargo test"), prefix("tail")],
            "the repeated rule is a no-op"
        );
        memory.remember_deny(&[prefix("cargo test")]);
        assert!(
            memory.deny_rules().is_empty(),
            "the blessed verb is skipped, the deny list stays empty"
        );
    }

    #[test]
    fn a_fully_skipped_press_fires_no_write_through() {
        // The write-through is the session file mirroring the memory:
        // a press that records nothing (every rule skipped) changed
        // nothing, so the hook stays silent — the file never grows a
        // phantom rewrite of the pair it already holds.
        let memory = PermissionMemory::default();
        memory.remember_rules(&[prefix("cargo build")]);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&writes);
        memory.persist_through(Arc::new(
            move |allow: &[PermissionRule], deny: &[PermissionRule]| {
                sink.lock().unwrap().push((allow.to_vec(), deny.to_vec()));
            },
        ));
        memory.remember_deny(&[prefix("cargo build")]);
        assert!(
            writes.lock().unwrap().is_empty(),
            "a fully skipped press is not a change — no write-through"
        );
        memory.remember_deny(&[prefix("rm")]);
        assert_eq!(writes.lock().unwrap().len(), 1, "a real growth fires once");
    }

    #[test]
    fn would_record_previews_the_post_skip_set() {
        // The sheet's remember lines render this: what the press will
        // write, not what it would have written before the skip
        // existed — both sides preview the same filter the record
        // arms apply.
        let memory = PermissionMemory::default();
        memory.remember_rules(&[prefix("tail"), prefix("cargo build")]);
        let never = vec![prefix("tail"), prefix("cargo build"), prefix("rm")];
        assert_eq!(
            memory.would_record(&never, true),
            vec![prefix("rm")],
            "the blessed verbs are skipped, the fresh one previews"
        );
        memory.remember_deny(&never);
        let always = vec![prefix("rm"), prefix("cargo build")];
        assert_eq!(
            memory.would_record(&always, false),
            vec![prefix("cargo build")],
            "the denied verb is skipped, the unblessed one previews"
        );
    }
}
