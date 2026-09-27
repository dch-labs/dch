//! End-to-end check that the real builtin registry's `system_prompt()`
//! fragments agree with the documented fragment set.
//!
//! Ignored until loopctl's `read` tool lands its `system_prompt()` fragment
//! (the only builtin still without one). When un-ignored it asserts the
//! documented headers are present and that tools without a fragment
//! contribute none.

#![allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]

use dch_loop::build_system_prompt;
use dch_tools::builtin_registry;

#[test]
#[ignore = "until loopctl's read tool lands its system_prompt() fragment"]
fn builtin_registry_emits_documented_fragment_headers() {
    let prompt = build_system_prompt(&builtin_registry(
        &loopctl::tool::builtin::fs::FileSession::new(std::path::PathBuf::from(".")),
    ));
    for header in ["## Bash", "## read", "## Write", "## Edit", "## TodoWrite"] {
        assert!(
            prompt.contains(header),
            "expected fragment header {header:?} in prompt"
        );
    }
    for silent in ["## Glob", "## Grep", "## Tree", "## FileViewer"] {
        assert!(
            !prompt.contains(silent),
            "tool {silent:?} has no system_prompt fragment; none expected"
        );
    }
}
