//! Shared fixtures for the integration suites.
//!
//! The engine-built observer contexts (`ToolPreContext`,
//! `TurnEndContext`) are constructable only inside loopctl: one
//! throwaway `BareLoop` run over the testing mocks produces genuine
//! values, harvested once per test binary and reshaped field-by-field
//! by each suite's helpers.

#![allow(dead_code)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use loopctl::engine::BareLoop;
use loopctl::engine::Loop;
use loopctl::engine::RunConfig;
use loopctl::managers::LoopManagers;
use loopctl::observer::LoopObserver;
use loopctl::observer::ToolPreContext;
use loopctl::observer::TurnEndContext;
use loopctl::testing::MockApiClient;
use loopctl::testing::MockTool;
use loopctl::tool::ToolRegistry;

/// The contexts one engine run fired, kept for reshaping.
struct Harvest {
    /// The tool-dispatch context the run produced.
    pre: Mutex<Option<ToolPreContext>>,
    /// The last turn-end context the run produced.
    turn_end: Mutex<Option<TurnEndContext>>,
}

impl LoopObserver for Harvest {
    fn name(&self) -> &'static str {
        "harvest"
    }

    fn on_tool_pre(&self, ctx: &ToolPreContext) {
        *self.pre.lock().expect("the pre slot") = Some(ctx.clone());
    }

    fn on_turn_end(&self, ctx: &TurnEndContext) {
        *self.turn_end.lock().expect("the turn-end slot") = Some(ctx.clone());
    }
}

/// A genuine `ToolPreContext`, harvested once per test binary.
pub fn tool_pre() -> ToolPreContext {
    harvested().0.clone()
}

/// A genuine successful `TurnEndContext`, harvested once per test binary.
pub fn turn_end() -> TurnEndContext {
    harvested().1.clone()
}

fn harvested() -> &'static (ToolPreContext, TurnEndContext) {
    static HARVEST: OnceLock<(ToolPreContext, TurnEndContext)> = OnceLock::new();
    HARVEST.get_or_init(|| {
        let recorder = Arc::new(Harvest {
            pre: Mutex::new(None),
            turn_end: Mutex::new(None),
        });
        let client = MockApiClient::new("harvest-model")
            .with_tool_call("harvest-call", "Mock", serde_json::json!({}))
            .with_text_response("harvest complete");
        let mut tools = ToolRegistry::new();
        tools.register(MockTool::new("Mock", "harvest transport"));
        let managers =
            LoopManagers::new().with_observer(Arc::clone(&recorder) as Arc<dyn LoopObserver>);
        let mut engine = BareLoop::new_with_managers(
            Arc::new(client),
            tools,
            loopctl::testing::test_config(),
            managers,
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the harvest runtime");
        let _result = runtime.block_on(engine.run("harvest", &RunConfig::default()));
        let pre = recorder
            .pre
            .lock()
            .expect("the pre slot")
            .clone()
            .expect("the run dispatches the mock tool");
        let turn_end = recorder
            .turn_end
            .lock()
            .expect("the turn-end slot")
            .clone()
            .expect("the run ends a turn");
        (pre, turn_end)
    })
}
