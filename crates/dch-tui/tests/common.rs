//! Shared fixtures for the integration suites.
//!
//! The engine-built observer contexts (`ToolPreContext`,
//! `TurnEndContext`, `TransportFallbackContext`) are constructable
//! only inside loopctl: throwaway `BareLoop` runs over the testing
//! mocks produce genuine values, harvested once per test binary and
//! reshaped field-by-field by each suite's helpers.

#![allow(dead_code)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use loopctl::api::ApiClient;
use loopctl::api::NonStreamingResponse;
use loopctl::api::StreamRequest;
use loopctl::api::error::ApiError;
use loopctl::engine::BareLoop;
use loopctl::engine::Loop;
use loopctl::engine::RunConfig;
use loopctl::managers::LoopManagers;
use loopctl::observer::LoopObserver;
use loopctl::observer::ToolPreContext;
use loopctl::observer::TransportFallbackContext;
use loopctl::observer::TurnEndContext;
use loopctl::stream::StreamEvent;
use loopctl::stream::StreamStopReason;
use loopctl::stream::Usage;
use loopctl::stream::handler::StreamHandler;
use loopctl::stream::handler::StreamRetryConfig;
use loopctl::stream::handler::StreamTimeoutConfig;
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
        let tools = ToolRegistry::new();
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

/// The transport-fallback context one engine run fired, kept for
/// reshaping.
struct FallbackHarvest {
    /// The fallback context the run produced.
    fallback: Mutex<Option<TransportFallbackContext>>,
}

impl LoopObserver for FallbackHarvest {
    fn name(&self) -> &'static str {
        "fallback-harvest"
    }

    fn on_transport_fallback(&self, ctx: &TransportFallbackContext) {
        *self.fallback.lock().expect("the fallback slot") = Some(ctx.clone());
    }
}

/// A client whose every stream attempt dies with a retryable transport
/// error while its non-streaming path serves a healthy answer — the
/// shape that drives the engine's last-chance transport fallback.
struct FailingStreamClient;

impl ApiClient for FailingStreamClient {
    fn model(&self) -> String {
        "fallback-harvest-model".to_string()
    }

    fn set_model(&self, _model: &str) -> bool {
        false
    }

    fn stream_messages(
        &self,
        _request: &StreamRequest,
    ) -> Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, ApiError>> + Send + 'static>> {
        Box::pin(futures::stream::once(async {
            Err(ApiError::api("stream transport broke"))
        }))
    }

    fn create_message(
        &self,
        _request: &StreamRequest,
    ) -> Pin<Box<dyn Future<Output = Result<NonStreamingResponse, ApiError>> + Send + '_>> {
        Box::pin(async {
            Ok(NonStreamingResponse {
                message: loopctl::testing::test_assistant_message("fallback answer"),
                stop_reason: StreamStopReason::EndTurn,
                usage: Some(Usage::new(4, 6)),
            })
        })
    }
}

/// A genuine `TransportFallbackContext`, harvested once per test
/// binary.
pub fn transport_fallback() -> TransportFallbackContext {
    static HARVEST: OnceLock<TransportFallbackContext> = OnceLock::new();
    HARVEST
        .get_or_init(|| {
            let recorder = Arc::new(FallbackHarvest {
                fallback: Mutex::new(None),
            });
            let handler = StreamHandler::new()
                .with_retry_config(StreamRetryConfig {
                    max_retries: 1,
                    base_delay_ms: 1,
                    max_delay_ms: 1,
                    jitter_factor: 0.0,
                })
                .with_timeout_config(StreamTimeoutConfig {
                    fallback_to_non_streaming: true,
                    ..Default::default()
                });
            let managers = LoopManagers::new()
                .with_stream_handler(handler)
                .with_observer(Arc::clone(&recorder) as Arc<dyn LoopObserver>);
            let mut engine = BareLoop::new_with_managers(
                Arc::new(FailingStreamClient),
                ToolRegistry::new(),
                loopctl::testing::test_config(),
                managers,
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("the fallback harvest runtime");
            let _result = runtime.block_on(engine.run("harvest", &RunConfig::default()));
            recorder
                .fallback
                .lock()
                .expect("the fallback slot")
                .clone()
                .expect("the exhausted ladder serves the turn by fallback")
        })
        .clone()
}
