//! The ingredients a runner hands the loop's managers: the resilient
//! stream handler, the context manager around the selected compactor,
//! and the model-fallback breaker.

use std::sync::Arc;

use loopctl::compact::ContextCompactor;
use loopctl::compact::ContextManager;
use loopctl::compact::TruncatingCompactor;
use loopctl::compact::qa_summarizer::QaSummarizer;
use loopctl::compact::qa_summarizer::QaSummarizerConfig;
use loopctl::compact::rolling::RollingCompactor;
use loopctl::compact::rolling::RollingConfig;
use loopctl::compact::structured_summarizer::StructuredSummarizer;
use loopctl::compact::structured_summarizer::StructuredSummaryConfig;
use loopctl::fallback::FallbackManager;
use loopctl::stream::handler::RateLimitConfig;
use loopctl::stream::handler::StreamHandler;
use loopctl::stream::handler::StreamTimeoutConfig;
use loopctl::stream::repetition::RepetitionGuardConfig;

use crate::DchClient;
use crate::RunnerError;

/// Arm the model-fallback breaker for a primary/fallback pair.
///
/// The breaker starts closed: the primary serves until it accumulates enough
/// failures to trip, after which the fallback serves until a recovery probe
/// on the primary succeeds.
///
/// # Errors
///
/// Returns [`RunnerError::Config`] only when the breaker's internal state
/// lock is poisoned.
pub(super) fn arm_fallback(primary: &str, fallback: &str) -> Result<FallbackManager, RunnerError> {
    let manager = FallbackManager::for_model(primary)
        .and_then(|m| {
            m.set_fallback_models(vec![fallback.to_string()])
                .map(|()| m)
        })
        .map_err(|e| RunnerError::Config(format!("fallback breaker: {e}")))?;
    Ok(manager)
}

/// Build the stream handler one runner session runs its model calls on.
///
/// The stream policy the host tunes, and nothing else: the whole-call
/// budget — `[runner] turn_timeout_secs`, the wall clock one streaming
/// call may consume — the rate-limit ladder's floor — `[runner]
/// rate_limit_delay_secs`, the first hintless backoff and the base the
/// ladder doubles from — and the repetition guard's floor, which
/// [`repetition_guard_for`] maps from `[runner]
/// repetition_min_period_bytes`. The rate-limit floor lifts the
/// ladder's cap with it when it rises past the library's own
/// 60-second ceiling: a floor the cap cannot carry is a config
/// the handler would refuse and silently fall back to its defaults,
/// trading the configured 120 seconds for the stock five, so the cap
/// meets the floor instead — the host asked for a long window, and
/// the whole ladder must honour it. Everything else the handler's own
/// defaults own (the hint-honouring policy, the escalation threshold,
/// the first-event window, the guard's repeat count and period cap)
/// stays stock: the host's levers are the numbers the provider's quota
/// window and the model's own habits — not the transport — should
/// decide.
pub(super) fn stream_handler(runner: &dch_config::RunnerConfig) -> StreamHandler {
    let timeout_config = StreamTimeoutConfig {
        total_stream_timeout: std::time::Duration::from_secs(runner.turn_timeout_secs),
        ..StreamTimeoutConfig::default()
    };
    let library = RateLimitConfig::default();
    let floor = std::time::Duration::from_secs(runner.rate_limit_delay_secs);
    let rate_limit_config = RateLimitConfig {
        default_delay: floor,
        max_delay: library.max_delay.max(floor),
        ..library
    };
    StreamHandler::new()
        .with_timeout_config(timeout_config)
        .with_rate_limit_config(rate_limit_config)
        .with_repetition_guard(repetition_guard_for(runner))
}

/// The repetition guard's configuration for one runner session.
///
/// The guard aborts a model call whose text ends in one byte window
/// repeated verbatim four times running, and `[runner]
/// repetition_min_period_bytes` is the smallest window it treats as a
/// period. A byte-level window cannot tell the horizontal border of an
/// ASCII-art box from a loop: the border is one character repeated, a
/// box-drawing horizontal is three bytes, and sixty-four of them are four
/// of the library's 48-byte windows. That is why the floor rides the
/// config rather than the library's default: the default of 384 keeps
/// any such border under four windows, since it would have to be 512
/// characters wide, while a genuine stanza loop still trips. Everything
/// else stays at the library's defaults (the repeat count, the period
/// cap), and `0` disarms the guard whole, its floor falling back to the
/// library's so the configuration still validates.
pub(super) fn repetition_guard_for(runner: &dch_config::RunnerConfig) -> RepetitionGuardConfig {
    let library = RepetitionGuardConfig::default();
    RepetitionGuardConfig {
        enabled: runner.repetition_min_period_bytes != 0,
        min_period_bytes: runner
            .repetition_min_period_bytes
            .max(library.min_period_bytes),
        ..library
    }
}

/// Build the context manager around exactly the compactor the
/// configuration names.
///
/// The selection is the whole contract: `qa` runs the question/answer
/// summarizer, `structured` the sectioned summarizer, `rolling` the
/// bounded aging pass, and `truncating` the oldest-drop — each bound to
/// the session's own client, the same conversation model, no second
/// endpoint to configure. No other compactor ever runs in the selected
/// one's place: a stage whose model call fails fails its pass, the
/// engine's threshold machinery defers while the payload fits the
/// window and retries on the next turn, and the emergency line ends the
/// run honestly rather than silently swapping strategy. The manager
/// carries the session's window and threshold exactly as the engine's
/// implicit default would have, so the selection changes only the
/// compactor, never the policy.
pub(super) fn context_manager_for(
    selection: dch_config::CompactorSelection,
    client: &Arc<DchClient>,
    session_config: &loopctl::config::SessionConfig,
) -> ContextManager {
    let shared = Arc::clone(client) as loopctl::api::SharedApiClient;
    let compactor: Arc<dyn ContextCompactor> = match selection {
        dch_config::CompactorSelection::Truncating => Arc::new(TruncatingCompactor::default()),
        dch_config::CompactorSelection::Qa => {
            Arc::new(QaSummarizer::new(shared, QaSummarizerConfig::default()))
        }
        dch_config::CompactorSelection::Structured => Arc::new(StructuredSummarizer::new(
            shared,
            StructuredSummaryConfig::default(),
        )),
        dch_config::CompactorSelection::Rolling => {
            Arc::new(RollingCompactor::new(shared, RollingConfig::default()))
        }
    };
    ContextManager::new(compactor)
        .with_context_window(session_config.context_window)
        .with_threshold(session_config.compact_threshold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Runner;
    use crate::runner::test_support::{
        CannedResponse, RecordingObserver, SseServer, recorded_requests, sse_text_turn,
        sse_tool_call_turn, wire_config,
    };
    use dch_config::DchConfig;
    use loopctl::managers::LoopManagers;
    use loopctl::message::Message;
    use loopctl::observer::LoopObserver;
    use loopctl::stream::repetition::detect_repetition;
    use tempfile::TempDir;

    #[test]
    fn the_stream_handler_carries_the_configured_rate_limit_floor() {
        // The host's one lever on the retry ladder, read back from the
        // handler a session would run: the hintless floor is exactly the
        // configured seconds — the tuned floor the quota window asked
        // for, never the stock five — while the whole-call budget keeps
        // its own knob and the ladder's policy (cap, escalation
        // threshold, hint honouring) stays at the handler's defaults.
        let mut config = DchConfig::default();
        config.runner.rate_limit_delay_secs = 45;
        config.runner.turn_timeout_secs = 480;
        let handler = stream_handler(&config.runner);
        assert_eq!(
            handler.rate_limit_config().default_delay,
            std::time::Duration::from_secs(45),
            "the hintless floor is the configured seconds"
        );
        assert_eq!(
            handler.rate_limit_config().max_delay,
            RateLimitConfig::default().max_delay,
            "the ladder's cap stays at the handler's own default"
        );
        assert_eq!(
            handler.rate_limit_config().fallback_after_retries,
            RateLimitConfig::default().fallback_after_retries,
            "the escalation threshold stays at the handler's own default"
        );
        assert_eq!(
            handler.timeout_config().total_stream_timeout,
            std::time::Duration::from_secs(480),
            "the whole-call budget keeps its own knob"
        );
    }

    #[test]
    fn the_default_handler_floors_the_ladder_at_fifteen_seconds() {
        // The untuned session is tuned anyway: the config's default
        // floor — fifteen — reaches the handler, so the ladder a stock
        // session runs ramps 15 + 30 + 60, not the library's 5 + 10 +
        // 20. Under the library's own ceiling, the cap stays stock.
        let handler = stream_handler(&DchConfig::default().runner);
        assert_eq!(
            handler.rate_limit_config().default_delay,
            std::time::Duration::from_secs(15),
            "the default floor is fifteen seconds at the handler"
        );
        assert_eq!(
            handler.rate_limit_config().max_delay,
            RateLimitConfig::default().max_delay,
            "a floor under the library's ceiling leaves the cap stock"
        );
        assert!(
            handler.rate_limit_config().validate().is_ok(),
            "the default floor is a config the handler accepts whole"
        );
    }

    #[test]
    fn a_floor_past_the_library_ceiling_lifts_the_cap_with_it() {
        // A 120-second floor against the library's
        // 60-second cap. Left to the stock cap, the handler would
        // refuse the config and quietly run its defaults — a 5-second
        // first retry where the host asked for 120. The cap meets the
        // floor instead: the handler validates the ladder whole and
        // carries the floor it was given.
        let mut config = DchConfig::default();
        config.runner.rate_limit_delay_secs = 120;
        let handler = stream_handler(&config.runner);
        assert_eq!(
            handler.rate_limit_config().default_delay,
            std::time::Duration::from_secs(120),
            "the long floor reaches the handler undiscarded"
        );
        assert!(
            handler.rate_limit_config().max_delay >= std::time::Duration::from_secs(120),
            "the cap carries the floor"
        );
        assert!(
            handler.rate_limit_config().validate().is_ok(),
            "the ladder validates whole — nothing for the handler to refuse"
        );
    }

    #[test]
    fn the_stream_handler_carries_the_configured_repetition_floor() {
        // The host's lever on the repetition guard, read back from the
        // handler a session would run: the floor is exactly the
        // configured bytes, armed, while the repeat count and the period
        // cap stay at the library's defaults and the whole configuration
        // is one the guard accepts.
        let mut config = DchConfig::default();
        config.runner.repetition_min_period_bytes = 256;
        let handler = stream_handler(&config.runner);
        assert_eq!(
            handler.repetition_config().min_period_bytes,
            256,
            "the floor is the configured bytes"
        );
        assert!(
            handler.repetition_config().enabled,
            "a nonzero floor arms the guard"
        );
        let library = RepetitionGuardConfig::default();
        assert_eq!(
            handler.repetition_config().min_repeats,
            library.min_repeats,
            "the repeat count stays at the library's default"
        );
        assert_eq!(
            handler.repetition_config().max_period_bytes,
            library.max_period_bytes,
            "the period cap stays at the library's default"
        );
        assert!(
            handler.repetition_config().validate().is_ok(),
            "the floor is a config the guard accepts whole"
        );
    }

    #[test]
    fn the_default_repetition_floor_clears_an_ascii_art_border() {
        // Two hundred box-drawing horizontals are 600 bytes — three times
        // the width at which the library's 48-byte floor reads a border as
        // a loop (sixty-four glyphs, 192 bytes, four windows). Under the
        // default floor no such border reaches four windows — it would
        // have to be 512 characters wide — while a stanza repeated
        // verbatim still does: forty 50-byte copies hold the eight-stanza
        // period four times over.
        let guard = repetition_guard_for(&DchConfig::default().runner);
        assert_eq!(
            guard.min_period_bytes, 384,
            "the default floor is 384 bytes"
        );
        let border = "\u{2500}".repeat(200);
        assert_eq!(
            border.len(),
            600,
            "the fixture spans three times the incident's width"
        );
        assert!(
            detect_repetition(border.as_bytes(), &guard).is_none(),
            "a border is a line, not a loop"
        );
        let stanza = "<tool_call><function=Bash></function></tool_call>\n";
        assert_eq!(stanza.len(), 50, "the stanza of the incident's shape");
        let hit = detect_repetition(stanza.repeat(40).as_bytes(), &guard)
            .expect("a stanza repeated verbatim is still a loop");
        assert_eq!(
            hit.period_bytes, 400,
            "the eight-stanza period, the first multiple of the floor"
        );
        assert_eq!(hit.repeats, 4, "the verdict threshold, never a census");
    }

    #[test]
    fn a_zero_repetition_floor_disarms_the_guard() {
        // Zero is the documented disarm value: the guard arrives
        // disarmed with a floor the library still accepts, and the
        // stanza that trips the default passes untouched.
        let mut config = DchConfig::default();
        config.runner.repetition_min_period_bytes = 0;
        let handler = stream_handler(&config.runner);
        assert!(
            !handler.repetition_config().enabled,
            "zero disarms the guard"
        );
        assert!(
            handler.repetition_config().validate().is_ok(),
            "a disarmed guard is still a valid configuration"
        );
        let stanza = "<tool_call><function=Bash></function></tool_call>\n";
        assert!(
            detect_repetition(stanza.repeat(40).as_bytes(), handler.repetition_config()).is_none(),
            "nothing fires under a disarmed guard"
        );
    }

    #[test]
    fn installing_the_handler_hands_the_managers_the_guard() {
        // The runner installs the handler on the managers bundle, which
        // adopts the handler's guard at install time — the one
        // configuration both turn paths enforce, streaming and
        // non-streaming alike.
        let mut config = DchConfig::default();
        config.runner.repetition_min_period_bytes = 256;
        let managers = LoopManagers::new().with_stream_handler(stream_handler(&config.runner));
        assert_eq!(
            managers.repetition_guard().min_period_bytes,
            256,
            "the managers carry the handler's floor"
        );
        assert!(
            managers.repetition_guard().enabled,
            "armed, as the handler armed it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ascii_art_border_reaches_the_turn_output() {
        // The runner installs the handler with the default repetition
        // floor, so a turn whose whole text is a box-drawing border three
        // times the width at which the library's floor reads a border as
        // a loop completes and delivers the border as its output.
        let border = "\u{2500}".repeat(200);
        let server = SseServer::start(vec![sse_text_turn(&border)]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 5), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run settles within timeout")
            .expect("a border is not a repetition loop");
        assert_eq!(run.output.as_deref(), Some(border.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_degenerate_stanza_stream_still_aborts_as_a_repetition_loop() {
        // The raised floor moves the line, it does not erase it: forty
        // verbatim copies of the incident's stanza hold the eight-stanza
        // period four times over, so the run ends with the typed
        // repetition error carrying that period and the verdict threshold.
        let stanza = "<tool_call><function=Bash></function></tool_call>\n".repeat(40);
        let server = SseServer::start(vec![sse_text_turn(&stanza)]).await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 5), dir.path())
            .build()
            .await
            .expect("constructs");
        let err = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run settles within timeout")
            .expect_err("a stanza repeated verbatim is a repetition loop");
        match err {
            loopctl::error::LoopError::RepetitionDetected {
                period_bytes,
                repeats,
                ..
            } => {
                assert_eq!(
                    period_bytes, 400,
                    "the eight-stanza period the floor first admits"
                );
                assert_eq!(repeats, 4, "the verdict threshold");
            }
            other => panic!("expected the typed repetition error, got: {other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retryable_stream_failure_is_retried_and_the_turn_completes() {
        // The runner installs loopctl's resilient stream handler, so a
        // 500 on the first stream attempt is one retryable failure,
        // not the end of the run: the ladder re-requests and the
        // canned second attempt serves the turn.
        let server = SseServer::start_canned(vec![
            CannedResponse {
                status: 500,
                body: "{\"error\": \"transient\"}".to_string(),
            },
            CannedResponse {
                status: 200,
                body: sse_text_turn("done"),
            },
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut runner = Runner::builder(&wire_config(server.port, 5), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("hi"))
            .await
            .expect("run settles within timeout")
            .expect("the retried stream succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            2,
            "exactly two requests: the failed attempt and its retry"
        );
    }

    /// One canned non-streaming completion body.
    ///
    /// Any assistant text serves the summarizers' calls in tests — the
    /// passes are pinned by their requests arriving and the run continuing,
    /// not by the summary's wording.
    fn canned_completion_body() -> String {
        "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\
         \"message\":{\"role\":\"assistant\",\"content\":\"compact summary\"},\
         \"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\
         \"completion_tokens\":1,\"total_tokens\":2}}"
            .to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_settled_compaction_counts_for_the_headless_save_path() {
        // The headless machine snapshot persists its compaction count
        // from the console observer's counter; a real settled pass —
        // the truncating selection spends no model call — must land
        // in both the observer and the handle construction hands the
        // save path.
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        std::fs::write(dir.path().join("chunk-a.txt"), &chunk).expect("write fixture");
        std::fs::write(dir.path().join("chunk-b.txt"), &chunk).expect("write fixture");
        let mut script = Vec::new();
        for index in 0..18 {
            let name = if index % 2 == 0 {
                "chunk-a.txt"
            } else {
                "chunk-b.txt"
            };
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start(script).await;
        let mut config = wire_config(server.port, 22);
        config.api.context_window = Some(90_000);
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let observer = Arc::new(crate::console_observer::ConsoleObserver::with_sinks(
            dch_config::Verbosity::Quiet,
            false,
            Box::new(std::io::sink()),
            Box::new(std::io::sink()),
        ));
        let counter = observer.compaction_counter();
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the run completes")
            .expect("the run compacts and finishes");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert_eq!(
            observer.compaction_count(),
            1,
            "the settled pass lands in the count the snapshot saves"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the save path's handle reads the same total"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_truncating_selection_compacts_by_dropping_messages() {
        // Several tool turns of medium results accumulate past the compact
        // threshold; the next request must be preceded by a compaction pass
        // that drops the older exchanges instead of serving over-window.
        // The growth rides on the read tool — Bash's own viewing path
        // answers with a refusal, not file contents. The window leaves the
        // deep default landing (30% of the threshold) enough room for the
        // terminal's preserved-tail floor — a window whose floor cannot
        // fit the landing is the truncator's documented refusal shape,
        // not this pin's claim.
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        std::fs::write(dir.path().join("chunk-a.txt"), &chunk).expect("write fixture");
        std::fs::write(dir.path().join("chunk-b.txt"), &chunk).expect("write fixture");
        let read_a = || sse_tool_call_turn("Read", &serde_json::json!({"path": "chunk-a.txt"}));
        let read_b = || sse_tool_call_turn("Read", &serde_json::json!({"path": "chunk-b.txt"}));
        let mut script = Vec::new();
        for index in 0..18 {
            if index % 2 == 0 {
                script.push(read_a());
            } else {
                script.push(read_b());
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start(script).await;
        let mut config = wire_config(server.port, 22);
        config.api.context_window = Some(90_000);
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            19,
            "eighteen tool turns plus the final text turn, no extras"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("\"stream\":false")),
            "truncation spends no model calls"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compactor_calls_leave_the_session_reasoning_effort_behind() {
        // The compaction chain speaks to the same endpoint through its
        // own client: a summarizer reasoning at the session's `high`
        // effort burns minutes and its output budget on thinking, so
        // the chain's requests carry no effort while the conversation
        // turns keep it.
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        let names = [
            "chunk-a.txt",
            "chunk-b.txt",
            "chunk-c.txt",
            "chunk-d.txt",
            "chunk-e.txt",
            "chunk-f.txt",
            "chunk-g.txt",
            "chunk-h.txt",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
        }
        let mut script = Vec::new();
        for name in names {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for round in 0..7u32 {
            for name in names {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name, "offset": round}),
                ));
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.api.reasoning_effort = Some("high".to_string());
        config.runner.compactor = dch_config::CompactorSelection::Qa;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the pass ran: {:?}",
            observer.snapshot()
        );
        let requests = recorded_requests(&server);
        let summarizer_requests: Vec<&String> = requests
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .collect();
        assert!(
            !summarizer_requests.is_empty(),
            "the summarizer called through the chain's own client"
        );
        assert!(
            summarizer_requests
                .iter()
                .all(|request| !request.contains("reasoning_effort")),
            "no compactor request carries the session's effort"
        );
        assert!(
            requests.iter().any(|request| {
                request.contains("\"stream\":true") && request.contains("reasoning_effort")
            }),
            "the conversation turns keep the configured effort"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_qa_selection_compacts_through_the_summarizer_and_the_run_continues() {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        let names = [
            "chunk-a.txt",
            "chunk-b.txt",
            "chunk-c.txt",
            "chunk-d.txt",
            "chunk-e.txt",
            "chunk-f.txt",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
        }
        let mut script = Vec::new();
        for name in names {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for _ in 0..8 {
            for name in names {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.runner.compactor = dch_config::CompactorSelection::Qa;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let summarizer_calls = recorded_requests(&server)
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            summarizer_calls >= 1,
            "the qa pass runs through the session's own client"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_structured_selection_compacts_through_the_structured_summarizer() {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(400);
        let names = [
            "chunk-a.txt",
            "chunk-b.txt",
            "chunk-c.txt",
            "chunk-d.txt",
            "chunk-e.txt",
            "chunk-f.txt",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
        }
        let mut script = Vec::new();
        for name in names {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for _ in 0..8 {
            for name in names {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.runner.compactor = dch_config::CompactorSelection::Structured;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let summarizer_calls = recorded_requests(&server)
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            summarizer_calls >= 1,
            "the structured pass runs through the session's own client"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rolling_selection_compacts_through_the_aging_pass() {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(120);
        // Ten grow runs, each closing in its own text seam: the rolling
        // pass ages only past its tenth boundary, and mid-run text
        // cannot serve (a text-only turn completes the run), so the
        // boundaries come from run seams. Unique paths per run keep the
        // repeated-call loop guard out of the picture.
        let names_for = |round: usize| {
            (0..6)
                .map(|file| format!("chunk-r{round}-{file}.txt"))
                .collect::<Vec<String>>()
        };
        for round in 0..10 {
            for name in names_for(round) {
                std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
            }
        }
        let mut script = Vec::new();
        for name in names_for(0) {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for round in 1..10 {
            for name in names_for(round) {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
            script.push(sse_text_turn(&format!("round {round} done")));
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_completion(script, canned_completion_body()).await;
        let mut config = wire_config(server.port, 70);
        config.api.context_window = Some(130_000);
        config.runner.compact_threshold = 40;
        config.runner.compactor = dch_config::CompactorSelection::Rolling;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        for round in 1..10 {
            let grown = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                runner.run(&format!("grow {round}")),
            )
            .await
            .expect("the grow run completes")
            .expect("the grow run succeeds");
            assert_eq!(
                grown.output.as_deref(),
                Some(format!("round {round} done").as_str())
            );
        }
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("run succeeds across the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold request must compact first: {:?}",
            observer.snapshot()
        );
        let rolling_calls = recorded_requests(&server)
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            rolling_calls >= 1,
            "the aging pass summarizes through the session's own client"
        );
    }

    /// The strict-selection pin body, shared by the qa, structured, and
    /// rolling variants: the configured compactor is the only one that
    /// ever runs, so when its model call fails the pass fails — nothing
    /// truncates in its place, the history rides the deferral intact,
    /// and the run proceeds while the payload fits the window.
    async fn a_failing_stage_never_substitutes(selection: dch_config::CompactorSelection) {
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(120);
        // Each run reads its own six files — the same path eleven times
        // would trip the engine's repeated-call loop guard — and every
        // completed run leaves the assistant-text seam the rolling pass
        // counts as an aging boundary; mid-run text cannot serve (a
        // text-only turn completes the run), so the boundaries come
        // from run seams and rolling's ten-boundary gate opens only
        // once the fixture genuinely has ten of them.
        let names_for = |round: usize| {
            (0..6)
                .map(|file| format!("chunk-r{round}-{file}.txt"))
                .collect::<Vec<String>>()
        };
        for round in 0..10 {
            for name in names_for(round) {
                std::fs::write(dir.path().join(name), &chunk).expect("write fixture");
            }
        }
        let mut script = Vec::new();
        for name in names_for(0) {
            script.push(sse_tool_call_turn(
                "Read",
                &serde_json::json!({"path": name}),
            ));
        }
        script.push(sse_text_turn("checkpoint"));
        for round in 1..10 {
            for name in names_for(round) {
                script.push(sse_tool_call_turn(
                    "Read",
                    &serde_json::json!({"path": name}),
                ));
            }
            script.push(sse_text_turn(&format!("round {round} done")));
        }
        script.push(sse_text_turn("recovered"));
        let server = SseServer::start_with_failing_completion(script).await;
        let mut config = wire_config(server.port, 60);
        config.api.context_window = Some(130_000);
        config.runner.compact_threshold = 40;
        config.runner.compactor = selection;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .build()
            .await
            .expect("constructs");
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the growth run completes")
            .expect("the growth run succeeds");
        assert_eq!(first.output.as_deref(), Some("checkpoint"));
        for round in 1..10 {
            let grown = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                runner.run(&format!("grow {round}")),
            )
            .await
            .expect("the grow run completes")
            .expect("the failed passes defer and the run proceeds");
            assert_eq!(
                grown.output.as_deref(),
                Some(format!("round {round} done").as_str())
            );
        }
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("run completes")
            .expect("the failed passes defer and the run proceeds");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        let requests = recorded_requests(&server);
        let summarizer_attempts = requests
            .iter()
            .filter(|request| request.contains("\"stream\":false"))
            .count();
        assert!(
            summarizer_attempts >= 1,
            "the configured stage genuinely tries its model call"
        );
        let snapshot = observer.snapshot();
        assert!(
            snapshot.contains(&"compaction_failed"),
            "the failed pass surfaces as a compaction failure: {snapshot:?}"
        );
        assert!(
            !snapshot.contains(&"compaction"),
            "no other compactor landed in the configured stage's place: {snapshot:?}"
        );
        let streaming: Vec<&String> = requests
            .iter()
            .filter(|request| !request.contains("\"stream\":false"))
            .collect();
        let occurrences = |request: &str| {
            request
                .matches("payload line for the context budget")
                .count()
        };
        let peak = streaming.iter().map(|request| occurrences(request)).max();
        let final_count = streaming.last().map(|request| occurrences(request));
        assert!(
            matches!((peak, final_count), (Some(peak), Some(end)) if end == peak),
            "the history rides the deferral intact — no silent truncation: \
             peak {peak:?} vs final {final_count:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_qa_stage_never_substitutes_another_compactor() {
        a_failing_stage_never_substitutes(dch_config::CompactorSelection::Qa).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_structured_stage_never_substitutes_another_compactor() {
        a_failing_stage_never_substitutes(dch_config::CompactorSelection::Structured).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_rolling_stage_never_substitutes_another_compactor() {
        a_failing_stage_never_substitutes(dch_config::CompactorSelection::Rolling).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_loop_history_compacts_through_the_qa_chain() {
        // The shape that wedged before the engine's compaction fixes:
        // every assistant message is a tool call and every result opens
        // the next user message, so no pair-safe split boundary exists
        // anywhere in the restored history. The qa selection must still
        // compact — through its own slid split — and the resumed run
        // must proceed.
        let server = SseServer::start_with_completion(
            vec![sse_text_turn("recovered")],
            canned_completion_body(),
        )
        .await;
        let dir = TempDir::new().expect("tempdir");
        let chunk = "payload line for the context budget\n".repeat(170);
        let mut history = vec![Message::new(
            loopctl::message::Role::User,
            vec![loopctl::message::MessagePart::text("grow the context")],
        )];
        for index in 0..150u32 {
            let call_id = format!("seed-{index}");
            history.push(Message::new(
                loopctl::message::Role::Assistant,
                vec![loopctl::message::MessagePart::tool_call(
                    call_id.clone(),
                    "Read",
                    serde_json::json!({ "path": "chunk.txt" }),
                )],
            ));
            history.push(Message::new(
                loopctl::message::Role::User,
                vec![loopctl::message::MessagePart::tool_result(
                    call_id,
                    "Read",
                    chunk.clone(),
                    false,
                )],
            ));
        }
        let mut config = wire_config(server.port, 60);
        config.api.context_window = Some(350_000);
        config.runner.compact_threshold = 60;
        config.runner.compactor = dch_config::CompactorSelection::Qa;
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .with_history(history)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("more"))
            .await
            .expect("the resumed run completes")
            .expect("the run succeeds through the compaction boundary");
        assert_eq!(run.output.as_deref(), Some("recovered"));
        assert!(
            observer.snapshot().contains(&"compaction"),
            "the over-threshold tool-loop history must compact: {:?}",
            observer.snapshot()
        );
        assert!(
            observer.snapshot().contains(&"pre_compaction"),
            "the pass announces its start before it runs: {:?}",
            observer.snapshot()
        );
        let seeded_lines = 12_usize.saturating_mul(170);
        let carried = recorded_requests(&server).first().map_or(0, |request| {
            request
                .matches("payload line for the context budget")
                .count()
        });
        assert!(
            carried < seeded_lines,
            "the follow-up request carries the compacted history: \
             {carried} of {seeded_lines} payload lines survived"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_seeded_history_resumes_without_recompaction() {
        let server = SseServer::start(vec![sse_text_turn("continued")]).await;
        let dir = TempDir::new().expect("tempdir");
        let config = wire_config(server.port, 7);
        let observer = Arc::new(RecordingObserver::new());
        let mut runner = Runner::builder(&config, dir.path())
            .with_observer(Arc::clone(&observer) as Arc<dyn LoopObserver>)
            .with_history(vec![
                Message::new(
                    loopctl::message::Role::User,
                    vec![loopctl::message::MessagePart::text("seed marker fa1")],
                ),
                Message::new(
                    loopctl::message::Role::Assistant,
                    vec![loopctl::message::MessagePart::text("prior answer")],
                ),
            ])
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("next"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.output.as_deref(), Some("continued"));
        assert!(
            !observer.snapshot().contains(&"compaction"),
            "a resumed working set under the threshold must not recompact: {:?}",
            observer.snapshot()
        );
        assert!(
            recorded_requests(&server)
                .first()
                .is_some_and(|request| request.contains("seed marker fa1")),
            "the seeded working set reaches the wire verbatim"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_configured_fallback_model_serves_once_the_primary_trips() {
        // A permanent auth failure costs exactly one attempt, so three
        // failing runs trip the breaker deterministically; the next run's
        // request must name the fallback model and succeed.
        let denied = r#"{"error":{"message":"invalid api key"}}"#.to_string();
        let server = SseServer::start_canned(vec![
            CannedResponse {
                status: 401,
                body: denied.clone(),
            },
            CannedResponse {
                status: 401,
                body: denied.clone(),
            },
            CannedResponse {
                status: 401,
                body: denied,
            },
            CannedResponse {
                status: 200,
                body: sse_text_turn("saved by the fallback"),
            },
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let mut config = wire_config(server.port, 10);
        config.api.fallback_model = Some("dch-fallback-model".to_string());
        let mut runner = Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("constructs");
        for attempt in 1..=3 {
            let result = runner.run("fail").await;
            assert!(result.is_err(), "denial {attempt} must fail the run");
        }
        let recovered = runner.run("recover").await.expect("fallback serves");
        assert_eq!(recovered.output.as_deref(), Some("saved by the fallback"));
        let requests = recorded_requests(&server);
        assert_eq!(
            requests.len(),
            4,
            "one request per run, no retry ladder on 401"
        );
        for (index, request) in requests.iter().enumerate().take(3) {
            assert!(
                request.contains("\"model\":\"test-model\""),
                "request {index} must name the primary model: {request}"
            );
        }
        assert!(
            requests[3].contains("\"model\":\"dch-fallback-model\""),
            "the post-trip request must carry the fallback model: {}",
            requests[3]
        );
    }
}
