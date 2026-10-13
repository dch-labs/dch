//! The context figure the status bar reads refreshes the moment a
//! compaction pass lands.
//!
//! A probe observer registered behind the TUI observer samples the
//! live token figure inside its own `on_compaction` — the instant the
//! bar's data must already describe the compacted size, not the
//! pre-pass figure the previous turn end stamped. The scenario rides
//! the proven truncating-selection recipe: chunk reads cross the
//! threshold, the pass lands mid-run, and the probe's sample plus the
//! settled record tell the whole story.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use std::sync::{Arc, Mutex};

use common::sse_tool_call_turn;
use serde_json::json;

/// Samples the bar's live figure inside each pass-settle event.
struct Probe {
    /// The shared display state, Arc-joined to the TUI observer's.
    state: dch_tui::TuiObserverState,

    /// One sample per compaction pass, in fire order.
    samples: Mutex<Vec<u64>>,

    /// The serving stage each pass reported, in fire order.
    stages: Mutex<Vec<Option<String>>>,
}

impl loopctl::observer::LoopObserver for Probe {
    fn name(&self) -> &'static str {
        "context-bar-probe"
    }

    fn on_compaction(&self, ctx: &loopctl::observer::CompactedContext) {
        // Registered behind the TUI observer, so its settle handling —
        // including the token refresh under test — has already run
        // when this fires.
        let figure = self
            .state
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .context_tokens;
        self.samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(figure);
        self.stages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.stage.clone());
    }
}

#[tokio::test]
async fn the_context_figure_refreshes_the_moment_a_pass_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let chunk = "payload line for the context budget\n".repeat(400);
    for name in ["chunk-a.txt", "chunk-b.txt", "chunk-c.txt", "chunk-d.txt"] {
        std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
    }
    let names = ["chunk-a.txt", "chunk-b.txt", "chunk-c.txt", "chunk-d.txt"];
    let mut script = Vec::new();
    for name in names.iter().cycle().take(24) {
        script.push(sse_tool_call_turn("Read", &json!({"path": name})));
    }
    script.push(common::sse_text_turn("recovered"));
    let server = common::CannedServer::sse(script);

    let mut config = dch_config::DchConfig::default();
    config.api.api_type = dch_config::ApiType::OpenAi;
    config.api.base_url = format!("http://127.0.0.1:{}/v1", server.port());
    config.api.model = "test-model".to_string();
    config.api.api_key = Some("test-key".to_string());
    config.api.context_window = Some(120_000);
    config.runner.compactor = dch_config::CompactorSelection::Truncating;

    let (observer, state) = dch_tui::TuiObserverState::new().into_observer();
    let probe: Arc<Probe> = Arc::new(Probe {
        state: state.clone(),
        samples: Mutex::new(Vec::new()),
        stages: Mutex::new(Vec::new()),
    });
    let probe_observer: Arc<dyn loopctl::observer::LoopObserver> = Arc::clone(&probe) as _;
    let mut runner = dch_loop::Runner::builder(&config, dir.path())
        .with_observer(Arc::new(observer))
        .with_observer(probe_observer)
        .build()
        .await
        .expect("runner builds");
    let run = tokio::time::timeout(std::time::Duration::from_secs(60), runner.run("grow"))
        .await
        .expect("the run completes inside the timeout");
    assert!(run.is_ok(), "the scripted run completes: {run:?}");

    let passes = state
        .compactions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(last) = passes.last() else {
        panic!("the fixture must compact: no pass recorded");
    };
    let (tokens_after, recorded_compactor) = match last {
        dch_tui::message::TuiMessage::Compacted {
            tokens_after,
            compactor,
            ..
        } => (*tokens_after, compactor.clone()),
        other => panic!("the pass record is a compacted row: {other:?}"),
    };
    let serving_stage = probe
        .stages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last()
        .cloned()
        .flatten();
    assert_eq!(
        recorded_compactor, serving_stage,
        "the record names the stage that served the pass — the transcript's \
         compactor field is populated from the serving-stage report"
    );
    assert!(
        tokens_after < 40_000,
        "the truncating terminal landed well under the threshold: {tokens_after}"
    );
    let samples = probe
        .samples
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(first_sample) = samples.first() else {
        panic!("the probe observed no pass");
    };
    assert_eq!(
        *first_sample, tokens_after,
        "the bar's figure already read the landing size at the settle event — \
         not the pre-pass figure the previous turn end stamped"
    );
}
