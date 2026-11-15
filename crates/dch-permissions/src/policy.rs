//! The policy seam: the trait a session runs, and the shipped default.
//!
//! Everything a permission verdict needs — the mode, the configured
//! rules, the session's remembered approvals — lives behind this trait,
//! so the agent loop depends on an object it can swap rather than on a
//! policy bolted into its pipeline. The default implementation is the
//! curated layered policy ([`evaluate`](crate::engine::evaluate)); a
//! host with different governance implements the trait and the loop
//! never knows.

use std::sync::Arc;

use crate::engine::CallShape;
use crate::engine::Decision;
use crate::engine::evaluate;
use crate::memory::PermissionMemory;
use crate::mode::PermissionMode;
use crate::mode::SharedPermissionMode;
use crate::rule::RememberRules;
use crate::rule::Rule;

/// The policy a session runs under: every verdict, one call.
///
/// The trait is the whole seam between the agent loop and the permission
/// domain. It is pure over the call's shape — no dispatch context, no
/// I/O, no clock — and total: every call gets a named decision. The two
/// methods split the verdict from the memory: what the gate rules now,
/// and what an "always" answer on this ask would record for later.
pub trait PermissionPolicy: Send + Sync {
    /// The verdict for one call's shape — pure, total, named.
    ///
    /// The returned [`Decision`] carries the outcome *and* the rule that
    /// decided it, so a host's audit trail can answer "why" without
    /// re-deriving the policy.
    fn evaluate(&self, call: &CallShape) -> Decision;

    /// The rules an "allow always" answer on this ask would record.
    ///
    /// Typed data derived from the call — verb prefixes for clean shell
    /// segments, exact text for complex ones, directory scopes for
    /// writes, the tool itself otherwise — never a display string, so a
    /// remembered rule can only cover work the approval truly saw.
    fn remember_rules(&self, call: &CallShape) -> RememberRules;
}

/// The shipped policy: the live mode cell, the configured rules, and
/// the session's remembered approvals, in the documented precedence.
///
/// One evaluation pass — config rules first (tightening on any match,
/// loosening only through full coverage), then the remembered
/// approvals, then the built-in mode × category matrix with its
/// carve-outs. The mode cell is read on **every** evaluation, so a
/// `/mode` move mid-run lands on the very next call; the memory is
/// consulted per evaluation too, so a remember made one call ago
/// covers the next.
pub struct RulesPolicy {
    /// The live mode cell, shared with every policy consumer.
    ///
    /// Read on every evaluation rather than held as a value, so a
    /// `/mode` move mid-run lands on the very next call without any
    /// rebuild or refresh step.
    mode_cell: SharedPermissionMode,

    /// The config's `[permissions]` rules, compiled to engine rules.
    ///
    /// Fixed at construction — the config the session loaded is the
    /// config the session keeps — and evaluated ahead of the memory
    /// on every call.
    config_rules: Vec<Rule>,

    /// The session's remembered approvals, consulted live.
    ///
    /// Held as the memory itself, not a snapshot: the UI's answers
    /// land in the same object, so a remember is visible to the very
    /// next evaluation with no refresh step.
    memory: PermissionMemory,
}

impl RulesPolicy {
    /// A policy over a live mode cell, the config's rules, and the
    /// session's memory.
    ///
    /// The memory is held directly — the policy projects it per
    /// evaluation — so a remember the UI records is visible to the very
    /// next call without any refresh step.
    #[must_use]
    pub fn new(
        mode_cell: SharedPermissionMode,
        config_rules: Vec<Rule>,
        memory: PermissionMemory,
    ) -> Self {
        Self {
            mode_cell,
            config_rules,
            memory,
        }
    }

    /// The mode in force, read live.
    ///
    /// The one mode read in the system: every evaluation passes
    /// through here, so the cell is the only thing `/mode` has to
    /// move.
    fn mode(&self) -> PermissionMode {
        *self
            .mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PermissionPolicy for RulesPolicy {
    fn evaluate(&self, call: &CallShape) -> Decision {
        evaluate(
            self.mode(),
            &self.config_rules,
            &self.memory.snapshot(),
            call,
        )
    }

    fn remember_rules(&self, call: &CallShape) -> RememberRules {
        if call.tool == "Bash" {
            let mut prefixes = Vec::new();
            let mut texts = Vec::new();
            for segment in &call.segments {
                if segment.complex {
                    texts.push(segment.text.clone());
                } else {
                    prefixes.push(segment.prefix.clone());
                }
            }
            return RememberRules::Segments { prefixes, texts };
        }
        if !call.write_targets.is_empty()
            && matches!(call.tool.as_str(), "Write" | "Edit" | "MultiEdit")
        {
            return RememberRules::PathGlobs(crate::shape::scope_patterns_for_relative(
                &call.write_targets,
            ));
        }
        RememberRules::Tool(call.tool.clone())
    }
}

/// The policy handed to the loop as a shared object.
///
/// `Arc<dyn PermissionPolicy>` — the runner holds one per session and
/// never knows which implementation it drives; a host with different
/// governance swaps the object, not the pipeline.
pub type SharedPermissionPolicy = Arc<dyn PermissionPolicy>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::engine::CallShape;
    use crate::mode::PermissionMode;
    use crate::rule::PermissionRule;
    use crate::vocabulary::PermissionOutcome;
    use crate::vocabulary::ToolCategory;

    fn policy(mode: PermissionMode) -> RulesPolicy {
        RulesPolicy::new(
            std::sync::Arc::new(std::sync::Mutex::new(mode)),
            Vec::new(),
            PermissionMemory::default(),
        )
    }

    fn write_call(target: &str) -> CallShape {
        CallShape {
            write_targets: vec![target.to_string()],
            ..CallShape::of("Write", ToolCategory::FileWrite)
        }
    }

    #[test]
    fn the_default_policy_is_the_matrix() {
        // With no config rules and nothing remembered, evaluate is the
        // documented matrix.
        assert_eq!(
            policy(PermissionMode::Auto)
                .evaluate(&CallShape::of("Write", ToolCategory::FileWrite))
                .outcome,
            PermissionOutcome::Allow,
        );
        assert_eq!(
            policy(PermissionMode::ReadOnly)
                .evaluate(&CallShape::of("Write", ToolCategory::FileWrite))
                .outcome,
            PermissionOutcome::Block,
        );
        assert_eq!(
            policy(PermissionMode::Interactive)
                .evaluate(&CallShape::of("Write", ToolCategory::FileWrite))
                .rule_id,
            "builtin:matrix",
        );
    }

    #[test]
    fn the_mode_cell_is_read_live_per_evaluation() {
        // The shared cell is the point: a move between evaluations lands
        // on the very next verdict, no rebuild owed.
        let cell: SharedPermissionMode =
            std::sync::Arc::new(std::sync::Mutex::new(PermissionMode::Auto));
        let subject = RulesPolicy::new(
            std::sync::Arc::clone(&cell),
            Vec::new(),
            PermissionMemory::default(),
        );
        let auto_call = write_call("src/lib.rs");
        assert_eq!(
            subject.evaluate(&auto_call).outcome,
            PermissionOutcome::Allow,
            "auto allows the write"
        );
        *cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PermissionMode::Interactive;
        assert_eq!(
            subject.evaluate(&auto_call).outcome,
            PermissionOutcome::Ask,
            "the same policy object now asks — the cell moved"
        );
    }

    #[test]
    fn a_remembered_prefix_is_derived_from_the_shape_and_honored() {
        // The trait's memory half: a bash ask derives its verb prefixes
        // from the shape itself — no dispatch context needed — and the
        // policy honors what its own memory records.
        let memory = PermissionMemory::default();
        memory.remember_rules(&[PermissionRule::ShellPrefix {
            prefix: "cargo test".to_string(),
        }]);
        let subject = RulesPolicy::new(
            std::sync::Arc::new(std::sync::Mutex::new(PermissionMode::Interactive)),
            Vec::new(),
            memory,
        );
        let call = CallShape {
            segments: vec![crate::shape::CommandSegment {
                text: "cargo test --all-features".to_string(),
                prefix: "cargo test".to_string(),
                complex: false,
            }],
            ..CallShape::of("Bash", ToolCategory::ShellExecute)
        };
        assert_eq!(
            subject.remember_rules(&call),
            RememberRules::Segments {
                prefixes: vec!["cargo test".to_string()],
                texts: Vec::new(),
            },
            "the bash ask derives its verb prefixes from the shape"
        );
        assert_eq!(
            subject.evaluate(&call).outcome,
            PermissionOutcome::Allow,
            "the remembered verb runs free"
        );
    }

    #[test]
    fn a_write_ask_derives_directory_scopes_from_relative_targets() {
        let subject = policy(PermissionMode::Interactive);
        let call = write_call("crates/x/src.rs");
        assert_eq!(
            subject.remember_rules(&call),
            RememberRules::PathGlobs(vec!["crates/x/**".to_string()]),
            "the write ask remembers its directory subtree"
        );
    }
}
