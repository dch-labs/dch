//! The missing-server-binary path, isolated in its own test binary.
//!
//! Producing the "server not found" error requires clearing `PATH`, which
//! in the shared library-test binary would break every concurrent spawn —
//! bash fixtures, live language servers' own cargo children. A separate
//! test binary owns its process environment, so the mutation is contained.

use std::path::PathBuf;

use dch_tools::LspTool;
use dch_tools::context::RunnerContext;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use serde_json::json;

#[tokio::test]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
async fn missing_server_binary_is_a_soft_error() {
    let guard = loopctl::testing::EnvGuard::acquire(&["PATH"]);
    guard.set("PATH", "");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "pub fn add() {}\n").unwrap();
    let mut ctx = ToolContext::default();
    ctx.set_extension(RunnerContext::new(PathBuf::from(tmp.path())));

    let out = LspTool
        .call(
            json!({
                "operation": "hover",
                "file_path": "lib.rs",
                "line": 1,
                "character": 1
            }),
            &ctx,
        )
        .await
        .unwrap();
    drop(guard);
    assert!(out.is_error, "soft error: {}", out.text_content());
    assert!(
        out.text_content().contains("not found"),
        "{}",
        out.text_content()
    );
}
