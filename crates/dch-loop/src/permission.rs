//! The dch permission gate: the mode × category matrix expressed as
//! loopctl middleware.
//!
//! The policy itself lives in
//! [`dch_tools::permission`] — the categories,
//! the modes, and the [`decide`] matrix. This module turns that policy
//! into a [`PermissionMiddleware`] installed in the runner's dispatch
//! pipeline: `Allow` cells pass through, `Block` cells deny with a
//! reason naming the mode and category, and `Ask` cells prompt through
//! an async resolver when one is attached (the TUI) or deny outright
//! when none is (headless). A prompt races the dispatch's cancel
//! signal, so a cancelled run never executes a tool however the
//! approval would have resolved.
//!
//! [`decide`]: dch_tools::permission::decide

use dch_tools::permission::PermissionMode;
use dch_tools::permission::PermissionOutcome;
use dch_tools::permission::decide;
use dch_tools::permission::tool_category;
use loopctl::middleware::AskResolverFn;
use loopctl::middleware::PermissionMiddleware;
use loopctl::tool::PermissionCheck;

/// Build the permission gate for `mode`, optionally resolving `Ask`
/// cells through `resolver`.
///
/// The returned middleware maps every tool dispatch through the mode ×
/// category matrix: a deny carries the mode and category in its reason
/// (the pipeline formats the model-visible error as `Permission {reason}
/// for tool '{tool}'`), and an ask carries a prompt naming the tool,
/// its category, and the mode. Without a resolver an `Ask` degrades to
/// a denial inside the middleware — correct for headless runs, which
/// have no one to ask.
#[must_use]
pub fn permission_layer(
    mode: PermissionMode,
    resolver: Option<AskResolverFn>,
) -> PermissionMiddleware {
    let middleware = PermissionMiddleware::from_context().with_check(move |ctx| {
        let category = tool_category(&ctx.tool_name);
        match decide(mode, category) {
            PermissionOutcome::Allow => PermissionCheck::Allow,
            PermissionOutcome::Block => PermissionCheck::Deny {
                reason: format!("denied by {mode:?} mode ({category:?} tool)"),
            },
            PermissionOutcome::Ask => PermissionCheck::Ask {
                prompt: format!("Allow '{}' ({category:?}) in {mode:?} mode?", ctx.tool_name),
            },
        }
    });
    match resolver {
        Some(resolver) => middleware.with_ask_resolver(resolver),
        None => middleware,
    }
}

/// The one conversion point from the config-side mode to the
/// dch-tools mirror.
///
/// The config type owns serde; the tools-side copy exists so
/// mode-aware code need not depend on dch-config. The relabeling is
/// pinned variant-for-variant by test.
pub(crate) fn tools_mode(mode: dch_config::PermissionMode) -> PermissionMode {
    match mode {
        dch_config::PermissionMode::Auto => PermissionMode::Auto,
        dch_config::PermissionMode::Plan => PermissionMode::Plan,
        dch_config::PermissionMode::AcceptEdits => PermissionMode::AcceptEdits,
        dch_config::PermissionMode::Interactive => PermissionMode::Interactive,
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn config_mode_maps_variant_for_variant() {
        assert!(matches!(
            tools_mode(dch_config::PermissionMode::Auto),
            PermissionMode::Auto
        ));
        assert!(matches!(
            tools_mode(dch_config::PermissionMode::Plan),
            PermissionMode::Plan
        ));
        assert!(matches!(
            tools_mode(dch_config::PermissionMode::AcceptEdits),
            PermissionMode::AcceptEdits
        ));
        assert!(matches!(
            tools_mode(dch_config::PermissionMode::Interactive),
            PermissionMode::Interactive
        ));
    }
}
