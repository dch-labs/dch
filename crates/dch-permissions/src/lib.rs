//! The permission domain for dch: modes, rules, memory, and the policy
//! seam the agent loop evaluates every tool call against.
//!
//! Every dispatch answers three questions — which mode is the session in,
//! which rules cover this call, and what the verdict is — and this crate
//! owns all three answers: the mode vocabulary, the rule types and their
//! file formats, the pure evaluation engine, the shell and write parsers
//! that reduce a call to what the policy can read, the remembered
//! decisions, and the [`PermissionPolicy`] seam a host plugs its policy
//! into. The crate depends on no other workspace crate and not on the
//! agent loop; the mapping between this domain and the loop's dispatch
//! types lives in the adapter that consumes it.

pub mod engine;
pub mod memory;
pub mod mode;
pub mod policy;
pub mod rule;
pub mod shape;
pub mod vocabulary;

pub use engine::CallShape;
pub use engine::Decision;
pub use engine::MemoryView;
pub use engine::RememberedRules;
pub use engine::evaluate;
pub use memory::MemorySnapshot;
pub use memory::PermissionMemory;
pub use memory::empty_view;
pub use mode::PermissionMode;
pub use mode::SharedPermissionMode;
pub use policy::PermissionPolicy;
pub use policy::RulesPolicy;
pub use rule::InvalidGlob;
pub use rule::Matcher;
pub use rule::MatcherParts;
pub use rule::PathGlob;
pub use rule::PermissionRule;
pub use rule::PermissionRuleConfig;
pub use rule::PermissionRuleOutcome;
pub use rule::PermissionsConfig;
pub use rule::Preset;
pub use rule::RememberRules;
pub use rule::Rule;
pub use shape::CommandSegment;
pub use shape::command_segments;
pub use shape::deny_rules;
pub use shape::is_session_furniture;
pub use shape::relative_target;
pub use shape::remember_rules;
pub use shape::segment_reads_only;
pub use shape::shell_reads_only;
pub use shape::write_family_path;
pub use shape::write_family_targets;
pub use vocabulary::PermissionOutcome;
pub use vocabulary::ToolCategory;
pub use vocabulary::decide;
pub use vocabulary::tool_category;
