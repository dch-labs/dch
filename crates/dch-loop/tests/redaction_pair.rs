//! The write-protection layers install as one unit or not at all.
//!
//! [`with_redaction`](dch_loop::redaction_guard::with_redaction) is
//! the only seam that installs the target-type gate, the overwrite
//! guard, and the scrubbing pass, so a pipeline either carries all
//! three — the type gate outermost, the guard directly above the
//! redaction, in that order — or none; neither guard's stated
//! precondition can be broken by composition.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::sync::Arc;

use dch_loop::redaction_guard::with_redaction;
use loopctl::middleware::ToolPipeline;
use loopctl::tool::ToolRegistry;

/// A pipeline with no middleware over an empty core.
///
/// The empty registry satisfies the builder's core requirement
/// without contributing behavior, so the layer list a pin reads is
/// exactly what the caller installed.
fn bare_pipeline() -> ToolPipeline {
    ToolPipeline::builder()
        .with_core(Arc::new(ToolRegistry::new()))
        .build()
        .expect("static composition builds")
}

#[test]
fn the_redaction_pair_installs_together_or_not_at_all() {
    let redacting = with_redaction(ToolPipeline::builder())
        .with_core(Arc::new(ToolRegistry::new()))
        .build()
        .expect("static composition builds");
    let names = redacting.middleware_names();
    assert_eq!(
        names,
        vec![
            "write-target-gate",
            "redaction-guard",
            "redaction",
            "tool_call"
        ],
        "the seam installs the write-protection layers directly above the core, in that order, \
         and nothing else"
    );

    let bare = bare_pipeline();
    let bare_names = bare.middleware_names();
    assert!(
        !bare_names.iter().any(|name| {
            *name == "write-target-gate" || *name == "redaction-guard" || *name == "redaction"
        }),
        "without the seam no layer is present: {bare_names:?}"
    );
}
