//! `TuiObserver` tests — pure state mutation driven with reshaped
//! contexts; no terminal, no network. The engine-built context types
//! arrive from one shared throwaway run each test binary harvests;
//! the rest are still hand-built (the post context is not
//! constructible outside its crate).

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use dch_tui::ContentBlock;
use dch_tui::Graduation;
use dch_tui::TokenCounts;
use dch_tui::observer::{TuiObserver, TuiObserverState};
use loopctl::observer::{
    AttemptResetContext, LoopObserver, ResponseContext, StreamContext, TextDeltaContext,
    ToolCallReceivedContext, ToolPreContext, TurnEndContext,
};
use serde_json::json;

fn state_and_observer() -> (TuiObserver, TuiObserverState) {
    TuiObserverState::new().into_observer()
}

fn delta(text: &str) -> TextDeltaContext {
    TextDeltaContext {
        turn: 0,
        delta: text.to_string(),
    }
}

fn received(call_id: &str, tool: &str, input: serde_json::Value) -> ToolCallReceivedContext {
    ToolCallReceivedContext {
        turn: 0,
        tool: tool.to_string(),
        call_id: call_id.to_string(),
        input,
    }
}

fn pre(call_id: &str, tool: &str) -> ToolPreContext {
    let mut ctx = common::tool_pre();
    ctx.turn = 0;
    ctx.tool = tool.to_string();
    ctx.tool_call_id = call_id.to_string();
    ctx
}

fn thinking(turn: usize, delta: &str) -> loopctl::observer::ThinkingDeltaContext {
    loopctl::observer::ThinkingDeltaContext {
        turn,
        delta: delta.to_string(),
    }
}

fn failed_stream() -> loopctl::observer::StreamFailureContext {
    loopctl::observer::StreamFailureContext {
        turn: 0,
        model: "test-model".to_string(),
        error: loopctl::error::LoopError::InvalidInput("the stream died".to_string()),
    }
}

#[test]
fn an_armed_observer_scrubs_secrets_from_the_input_summary() {
    let (observer, kept) = TuiObserverState::new()
        .with_secret_redaction(true)
        .into_observer();
    observer.on_tool_call_received(&received(
        "call-redact",
        "Bash",
        json!({"command": "curl -H 'Authorization: Bearer sk-super-secret-value' https://api.test"}),
    ));
    observer.on_tool_pre(&pre("call-redact", "Bash"));
    let active = kept.active_tools.lock().expect("the active lock");
    let summary = &active.first().expect("the active tool").input_summary;
    assert!(
        summary.contains("[REDACTED:bearer]"),
        "the credential is masked in the summary: {summary}"
    );
    assert!(
        !summary.contains("sk-super-secret-value"),
        "no raw credential survives: {summary}"
    );
}

#[test]
fn the_default_observer_records_the_input_summary_verbatim() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received(
        "call-raw",
        "Bash",
        json!({"command": "curl -H 'Authorization: Bearer sk-super-secret-value' https://api.test"}),
    ));
    observer.on_tool_pre(&pre("call-raw", "Bash"));
    let active = kept.active_tools.lock().expect("the active lock");
    let summary = &active.first().expect("the active tool").input_summary;
    assert!(
        summary.contains("sk-super-secret-value"),
        "redaction off keeps the summary raw: {summary}"
    );
}

#[test]
fn the_turn_end_reports_the_context_size() {
    let (observer, kept) = state_and_observer();
    let mut ctx = turn_end(0, 4, 1);
    ctx.context_tokens = 45_678;
    observer.on_turn_end(&ctx);
    assert_eq!(
        tokens_of(&kept).context_tokens,
        45_678,
        "the loop's exposed context size lands in the shared state"
    );
}

#[test]
fn an_attempt_reset_pushes_a_reconnection_row() {
    let (observer, kept) = state_and_observer();
    observer.on_stream_failure(&failed_stream());
    observer.on_attempt_reset(&AttemptResetContext {
        turn: 0,
        attempt: 2,
    });
    observer.on_attempt_reset(&AttemptResetContext {
        turn: 0,
        attempt: 3,
    });
    let lines = kept.retry_lines.lock().expect("the retry lock");
    let health = kept.health.lock().expect("the health lock");
    assert_eq!(
        *health,
        dch_tui::observer::HealthNotice::Retrying { attempt: Some(2) },
        "the notice carries the same retry index the row prints — \
         ctx.attempt is 1-indexed over attempts, the retry index is one less"
    );
    assert_eq!(lines.len(), 2, "both retries queue: {lines:?}");
    let max = usize::try_from(loopctl::stream::handler::StreamRetryConfig::default().max_retries)
        .unwrap_or(usize::MAX);
    let first = lines.first().expect("the first");
    assert_eq!(first.episode, 0);
    assert!(
        first
            .text
            .contains(&format!("⟳ connection lost — reconnection attempt 1/{max}")),
        "the first retry reads 1 of the ladder the handler runs: {:?}",
        first.text
    );
    let second = lines.last().expect("the second");
    assert_eq!(second.episode, 0);
    assert!(
        second
            .text
            .contains(&format!("⟳ connection lost — reconnection attempt 2/{max}")),
        "the second retry reads 2 of the ladder the handler runs: {:?}",
        second.text
    );
}

#[test]
fn a_repeated_tool_failure_keeps_the_conversation_free_of_retry_rows() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received("call-1", "Bash", json!({"command": "x"})));
    observer.finish_tool("call-1", "Bash", true, Duration::from_millis(5));
    observer.on_tool_call_received(&received("call-2", "Bash", json!({"command": "x"})));
    let lines = kept.retry_lines.lock().expect("the retry lock");
    assert!(
        lines.is_empty(),
        "tool retries stay out of the conversation — the failure lives in the graduated tool block: {lines:?}"
    );
}

#[test]
fn thinking_deltas_append_in_arrival_order_and_wake_the_render() {
    let (observer, kept) = state_and_observer();
    let listener = kept.render_notify.listen();
    observer.on_thinking_delta(&thinking(0, "first "));
    observer.on_thinking_delta(&thinking(0, "second"));
    let stream = kept.thinking.lock().expect("the thinking lock");
    assert_eq!(
        stream.text, "first second",
        "deltas concatenate in arrival order"
    );
    assert_eq!(stream.turn, 0);
    assert!(stream.began.is_some(), "the first delta stamps the clock");
    assert!(!stream.redacted);
    drop(stream);
    futures::executor::block_on(listener);
}

#[test]
fn a_response_graduates_the_thinking_with_its_reply_and_clears_the_buffer() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "because "));
    observer.on_thinking_delta(&thinking(0, "reasons"));
    observer.on_response(&ResponseContext {
        turn: 0,
        text: "the answer".to_string(),
        usage: None,
    });
    let graduations = kept.graduations.lock().expect("the queue lock").clone();
    let [
        Graduation::Reply {
            text,
            thinking: block,
        },
    ] = graduations.as_slice()
    else {
        panic!("one graduation for one response: {graduations:?}");
    };
    assert_eq!(text, "the answer");
    let Some(dch_tui::ContentBlock::Thinking { text, redacted, .. }) = block else {
        panic!("the reasoning graduates as a Thinking block: {block:?}");
    };
    assert_eq!(text, "because reasons");
    assert!(!redacted);
    assert!(
        kept.thinking
            .lock()
            .expect("the thinking lock")
            .text
            .is_empty(),
        "the buffer clears once its reasoning graduates"
    );

    observer.on_thinking_delta(&thinking(1, "fresh "));
    assert_eq!(
        kept.thinking.lock().expect("the thinking lock").text,
        "fresh ",
        "a follow-up stream starts from an empty buffer"
    );
}

#[test]
fn a_failed_stream_keeps_its_partial_thinking_for_the_turn_end() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "half "));
    observer.on_thinking_delta(&thinking(0, "a thought"));
    observer.on_stream_failure(&failed_stream());
    let stream = kept.thinking.lock().expect("the thinking lock");
    assert!(
        stream.text == "half a thought",
        "a failed attempt's reasoning survives to the turn end: {:?}",
        stream.text
    );
    assert!(!stream.redacted);
    assert!(stream.began.is_some());
}

#[test]
fn a_retried_attempt_resets_the_dead_prefix_before_its_retry_streams() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "dead prefix "));
    observer.on_text_delta(&delta("dead text "));
    observer.on_attempt_reset(&AttemptResetContext {
        turn: 0,
        attempt: 2,
    });
    assert!(
        kept.thinking
            .lock()
            .expect("the thinking lock")
            .text
            .is_empty(),
        "the reset cue discards the dead attempt's partial reasoning"
    );
    assert!(
        kept.streaming_text
            .lock()
            .expect("the text lock")
            .is_empty(),
        "the reset cue discards the dead attempt's partial text"
    );
    observer.on_thinking_delta(&thinking(0, "live reasoning"));
    observer.on_text_delta(&delta("live text"));
    observer.on_response(&ResponseContext {
        turn: 0,
        text: "the retry's answer".to_string(),
        usage: None,
    });
    let graduations = kept.graduations.lock().expect("the queue lock").clone();
    let [
        Graduation::Reply {
            thinking: block, ..
        },
    ] = graduations.as_slice()
    else {
        panic!("one graduation for one response: {graduations:?}");
    };
    let Some(dch_tui::ContentBlock::Thinking { text, .. }) = block else {
        panic!("the retry's live reasoning graduates: {block:?}");
    };
    assert_eq!(
        text, "live reasoning",
        "the dead attempt's prefix never concatenates onto the retry's reasoning"
    );
}

#[test]
fn a_fallback_served_turn_discards_its_dead_streaming_prefix() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "partially streamed"));
    observer.on_text_delta(&delta("partially "));
    let mut cue = common::transport_fallback();
    cue.turn = 0;
    observer.on_transport_fallback(&cue);
    assert!(
        kept.thinking
            .lock()
            .expect("the thinking lock")
            .text
            .is_empty(),
        "the fallback cue discards the dead streaming prefix's reasoning"
    );
    assert!(
        kept.streaming_text
            .lock()
            .expect("the text lock")
            .is_empty(),
        "the fallback cue discards the dead streaming prefix's text"
    );
    observer.on_response(&ResponseContext {
        turn: 0,
        text: "the fallback's answer".to_string(),
        usage: None,
    });
    let graduations = kept.graduations.lock().expect("the queue lock").clone();
    let [
        Graduation::Reply {
            thinking: block, ..
        },
    ] = graduations.as_slice()
    else {
        panic!("one graduation for one response: {graduations:?}");
    };
    assert!(
        block.is_none(),
        "the dead prefix's reasoning never rides the fallback reply: {block:?}"
    );
}

#[test]
fn an_empty_thinking_delta_marks_the_stream_redacted() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, ""));
    let stream = kept.thinking.lock().expect("the thinking lock");
    assert!(stream.redacted, "an empty delta signals withheld reasoning");
    assert!(stream.text.is_empty());
}

#[test]
fn a_failed_turn_graduates_its_partial_thinking() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "half "));
    observer.on_thinking_delta(&thinking(0, "a thought"));
    observer.on_turn_end(&failed_turn_end(0, 0, 0));
    let stream = kept.thinking.lock().expect("the thinking lock");
    assert!(
        stream.text.is_empty() && stream.began.is_none(),
        "the live reasoning row folds once the partial block graduated"
    );
    drop(stream);
    let graduations = kept.graduations.lock().expect("the graduations lock");
    assert_eq!(
        graduations.len(),
        1,
        "the cancelled run's own reasoning survives as one block: {graduations:?}"
    );
    match graduations.first().expect("one graduation") {
        Graduation::Reply {
            text,
            thinking:
                Some(ContentBlock::Thinking {
                    text: body,
                    redacted,
                    duration_ms,
                }),
        } => {
            assert!(
                text.is_empty(),
                "a cancelled turn carries no reply text: {text:?}"
            );
            assert_eq!(body, "half a thought", "{body}");
            assert!(!redacted, "{body}");
            assert!(*duration_ms < 10_000, "the duration is the stream's own");
        }
        other => panic!("unexpected graduation: {other:?}"),
    }
}

#[test]
fn a_retried_turns_failed_end_graduates_only_the_retrys_own_reasoning() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "dead attempt "));
    observer.on_stream_failure(&failed_stream());
    observer.on_attempt_reset(&AttemptResetContext {
        turn: 0,
        attempt: 2,
    });
    observer.on_thinking_delta(&thinking(0, "the retry's reasoning"));
    observer.on_turn_end(&failed_turn_end(0, 0, 0));
    let graduations = kept
        .graduations
        .lock()
        .expect("the graduations lock")
        .clone();
    let [
        Graduation::Reply {
            thinking: block, ..
        },
    ] = graduations.as_slice()
    else {
        panic!("one graduation for the retried turn: {graduations:?}");
    };
    let Some(dch_tui::ContentBlock::Thinking { text, .. }) = block else {
        panic!("the retry's live reasoning graduates: {block:?}");
    };
    assert_eq!(
        text, "the retry's reasoning",
        "the reset cue discarded the dead attempt's reasoning, so the failed turn end graduates only what the retry itself streamed"
    );
}

#[test]
fn a_stale_turns_thinking_never_graduates_onto_another_turns_failed_end() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "turn zero's reasoning"));
    observer.on_turn_end(&failed_turn_end(1, 0, 0));
    assert!(
        kept.graduations
            .lock()
            .expect("the graduations lock")
            .is_empty(),
        "turn one's failed end must not carry turn zero's orphaned reasoning"
    );
}

#[test]
fn a_stale_stream_never_graduates_onto_a_new_reply() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "cancelled mid-thought"));
    observer.on_response(&ResponseContext {
        turn: 1,
        text: "a later reply".to_string(),
        usage: None,
    });
    let stream = kept.thinking.lock().expect("the thinking lock");
    assert!(
        stream.text.is_empty() && stream.began.is_none(),
        "a response for another turn discards the stale stream rather than graduating it"
    );
    drop(stream);
    let graduations = kept.graduations.lock().expect("the graduations lock");
    let Some(reply) = graduations.iter().rev().find_map(|g| match g {
        Graduation::Reply { thinking, .. } => Some(thinking),
        Graduation::Tool(_) | Graduation::RunEnded { .. } => None,
    }) else {
        panic!("the reply still graduates");
    };
    assert!(
        reply.is_none(),
        "the stale reasoning must not ride the new reply's graduation"
    );
}

#[test]
fn a_turn_change_discards_rather_than_splices_living_reasoning() {
    let (observer, kept) = state_and_observer();
    observer.on_thinking_delta(&thinking(0, "abandoned mid-flight"));
    observer.on_thinking_delta(&thinking(1, "fresh turn"));
    let stream = kept.thinking.lock().expect("the thinking lock");
    assert_eq!(
        stream.text, "fresh turn",
        "a new turn's stream starts empty even when the previous turn's \
         terminal event never arrived"
    );
    assert_eq!(stream.turn, 1);
}

#[test]
fn a_turn_end_queues_its_history_for_the_host() {
    let (observer, kept) = state_and_observer();
    let mut success = turn_end(0, 10, 20);
    success.success = true;
    success.history = Arc::new(vec![
        loopctl::message::Message::user("read the note"),
        loopctl::message::Message::assistant("done"),
    ]);
    observer.on_turn_end(&success);
    let mut failed = failed_turn_end(1, 10, 20);
    failed.history = Arc::new(vec![loopctl::message::Message::user("read the note")]);
    observer.on_turn_end(&failed);
    let queued: Vec<(usize, String)> = kept
        .turn_histories
        .lock()
        .expect("the histories lock")
        .iter()
        .map(|history| {
            (
                history.len(),
                history
                    .first()
                    .map(loopctl::message::Message::text_content)
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        queued,
        vec![(2, "read the note".to_string())],
        "a successful turn end queues the engine's own snapshot; a \
         failed one publishes nothing — the resolution read owns that \
         state"
    );
}

fn stream(turn: usize, input_tokens: u64, output_tokens: u64) -> StreamContext {
    StreamContext {
        turn,
        model: "test-model".to_string(),
        input_tokens,
        output_tokens,
    }
}

fn turn_end(turn: usize, input_tokens: u64, output_tokens: u64) -> TurnEndContext {
    let mut ctx = common::turn_end();
    ctx.turn = turn;
    ctx.input_tokens = input_tokens;
    ctx.output_tokens = output_tokens;
    ctx
}

fn failed_turn_end(turn: usize, input_tokens: u64, output_tokens: u64) -> TurnEndContext {
    let mut ctx = turn_end(turn, input_tokens, output_tokens);
    ctx.success = false;
    ctx.error = Some("the stream died mid-reply".to_string());
    ctx
}

fn tokens_of(state: &TuiObserverState) -> TokenCounts {
    *state.tokens.lock().expect("the tokens lock")
}

#[test]
fn the_split_pattern_shares_the_buffers() {
    let (observer, kept) = state_and_observer();
    observer.on_text_delta(&delta("hi"));
    assert_eq!(
        *kept.streaming_text.lock().expect("the streaming lock"),
        "hi",
        "a write through the observer reaches the kept state"
    );
}

#[test]
fn the_notify_arc_is_shared() {
    let (observer, kept) = state_and_observer();
    let listener = kept.render_notify.listen();
    observer.on_text_delta(&delta("wake"));
    futures::executor::block_on(listener);
}

#[test]
fn text_deltas_append_in_arrival_order() {
    let (observer, kept) = state_and_observer();
    for part in ["a", "b", "c"] {
        observer.on_text_delta(&delta(part));
    }
    assert_eq!(*kept.streaming_text.lock().unwrap(), "abc");
}

#[test]
fn a_response_clears_the_streaming_buffer() {
    let (observer, kept) = state_and_observer();
    observer.on_text_delta(&delta("partial"));
    observer.on_response(&ResponseContext {
        turn: 0,
        text: "partial".to_string(),
        usage: None,
    });
    assert!(kept.streaming_text.lock().unwrap().is_empty());
}

#[test]
fn a_response_hands_the_finalized_reply_to_the_display_buffer() {
    let (observer, kept) = state_and_observer();
    observer.on_text_delta(&delta("partial "));
    observer.on_response(&ResponseContext {
        turn: 0,
        text: "partial reply".to_string(),
        usage: None,
    });
    let replies: Vec<String> = kept
        .graduations
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            Graduation::Reply { text, .. } => Some(text.clone()),
            Graduation::Tool(_) | Graduation::RunEnded { .. } => None,
        })
        .collect();
    assert_eq!(
        replies,
        vec!["partial reply".to_string()],
        "the response text survives finalization for the display to graduate"
    );
    assert!(
        kept.streaming_text.lock().unwrap().is_empty(),
        "the live buffer clears once its reply is handed over"
    );
}

#[test]
fn a_response_without_deltas_keeps_the_only_reply_copy() {
    let (observer, kept) = state_and_observer();
    observer.on_response(&ResponseContext {
        turn: 0,
        text: "the whole reply".to_string(),
        usage: None,
    });
    assert_eq!(
        kept.graduations.lock().unwrap().clone(),
        vec![Graduation::Reply {
            text: "the whole reply".to_string(),
            thinking: None,
        }],
        "a non-streaming turn's response text is the reply's only copy — it must not be dropped"
    );
}

#[test]
fn a_tool_pre_adds_an_active_tool() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received("call-1", "Edit", json!({"path": "a.rs"})));
    observer.on_tool_pre(&pre("call-1", "Edit"));
    let tools = kept.active_tools.lock().unwrap().clone();
    let tool = tools.first().expect("one active tool");
    assert_eq!(tool.name, "Edit");
    assert_eq!(tool.call_id, "call-1");
    assert!(
        tool.input_summary.contains("a.rs"),
        "the stashed input summary travels with the tool: {}",
        tool.input_summary
    );
}

#[test]
fn a_tool_post_moves_the_active_tool_to_results() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received("call-1", "Edit", json!("x")));
    observer.on_tool_pre(&pre("call-1", "Edit"));
    observer.finish_tool("call-1", "Edit", false, Duration::from_millis(5));

    assert!(kept.active_tools.lock().unwrap().is_empty());
    let results = kept.graduations.lock().unwrap().clone();
    let Graduation::Tool(result) = results.first().expect("one completed result") else {
        panic!("a completed tool graduates as a Tool event");
    };
    assert_eq!(result.name, "Edit");
    assert!(!result.is_error);
    assert_eq!(result.duration, Duration::from_millis(5));
}

#[test]
fn stream_success_sets_per_turn_and_accumulates_cumulative() {
    let (observer, kept) = state_and_observer();
    observer.on_stream_success(&stream(0, 10, 20));
    observer.on_stream_success(&stream(1, 5, 7));
    let tokens = tokens_of(&kept);
    assert_eq!(tokens.input, 5, "per-turn counts are last-wins");
    assert_eq!(tokens.output, 7);
    assert_eq!(tokens.cumulative_input, 15, "cumulative counts sum");
    assert_eq!(tokens.cumulative_output, 27);
}

#[test]
fn a_turn_end_without_a_stream_still_accumulates() {
    let (observer, kept) = state_and_observer();
    observer.on_turn_end(&turn_end(0, 3, 4));
    let tokens = tokens_of(&kept);
    assert_eq!(
        tokens.input, 3,
        "a turn-end-only turn sets the per-turn counts"
    );
    assert_eq!(tokens.output, 4);
    assert_eq!(tokens.cumulative_input, 3);
    assert_eq!(tokens.cumulative_output, 4);
}

#[test]
fn a_turn_end_after_its_stream_does_not_double_count() {
    let (observer, kept) = state_and_observer();
    observer.on_stream_success(&stream(0, 10, 20));
    observer.on_turn_end(&turn_end(0, 10, 20));
    let tokens = tokens_of(&kept);
    assert_eq!(
        tokens.cumulative_input, 10,
        "the stream already counted this turn"
    );
    assert_eq!(tokens.cumulative_output, 20);
}

#[test]
fn interleaved_posts_match_by_call_id() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received("call-1", "Grep", json!("first")));
    observer.on_tool_call_received(&received("call-2", "Grep", json!("second")));
    observer.on_tool_pre(&pre("call-1", "Grep"));
    observer.on_tool_pre(&pre("call-2", "Grep"));
    observer.finish_tool("call-1", "Grep", false, Duration::from_millis(1));

    let tools = kept.active_tools.lock().unwrap().clone();
    assert_eq!(
        tools.len(),
        1,
        "the first completion removes the first call"
    );
    assert!(
        tools.first().unwrap().input_summary.contains("second"),
        "the surviving entry is the still-running call"
    );
}

#[test]
fn reset_clears_every_buffer_and_the_private_maps() {
    let (observer, kept) = state_and_observer();
    observer.on_text_delta(&delta("partial"));
    observer.on_response(&ResponseContext {
        turn: 0,
        text: "partial".to_string(),
        usage: None,
    });
    observer.on_tool_call_received(&received("call-1", "Edit", json!("x")));
    observer.on_tool_pre(&pre("call-1", "Edit"));
    observer.on_stream_success(&stream(0, 10, 20));
    kept.errors.lock().unwrap().push("run failed".to_string());
    observer.reset();

    assert!(kept.streaming_text.lock().unwrap().is_empty());
    assert!(kept.graduations.lock().unwrap().is_empty());
    assert!(kept.active_tools.lock().unwrap().is_empty());

    assert!(
        kept.errors.lock().unwrap().is_empty(),
        "the driver's error buffer clears with the rest"
    );
    assert_eq!(tokens_of(&kept), TokenCounts::default());
    assert!(
        kept.current_turn.lock().unwrap().is_none(),
        "the mirrored turn counter resets with the rest"
    );
    observer.on_tool_pre(&pre("call-1", "Edit"));
    assert!(
        kept.active_tools
            .lock()
            .unwrap()
            .first()
            .unwrap()
            .input_summary
            .is_empty(),
        "the summary stash is cleared with the rest"
    );
    observer.on_turn_end(&turn_end(0, 5, 7));
    assert_eq!(
        tokens_of(&kept).cumulative_input,
        5,
        "the double-count note is cleared with the rest — a repeated turn id accumulates again"
    );
}

#[test]
fn a_failed_turn_discards_its_partial_streaming_text() {
    let (observer, kept) = state_and_observer();
    observer.on_text_delta(&delta("Hel"));
    observer.on_turn_end(&failed_turn_end(0, 4, 1));
    assert!(
        kept.streaming_text.lock().unwrap().is_empty(),
        "a failed turn's uncommitted text is discarded, not left to merge into the next turn's reply"
    );
}

#[test]
fn every_retry_attempt_renders_the_stashed_summary() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received("call-1", "Edit", json!({"path": "a.rs"})));
    observer.on_tool_pre(&pre("call-1", "Edit"));
    observer.finish_tool("call-1", "Edit", true, Duration::from_millis(1));
    observer.on_tool_pre(&pre("call-1", "Edit"));
    let tools = kept.active_tools.lock().unwrap().clone();
    let retried = tools
        .iter()
        .find(|tool| tool.call_id == "call-1")
        .expect("the retry attempt is in flight");
    assert!(
        retried.input_summary.contains("a.rs"),
        "the stash survives the first attempt so the retry renders the same summary: {}",
        retried.input_summary
    );
}

#[test]
fn undispatched_summaries_are_bounded_by_a_wholesale_drop() {
    let (observer, kept) = state_and_observer();
    for index in 0..64 {
        observer.on_tool_call_received(&received(&format!("call-{index}"), "Bash", json!(index)));
    }
    observer.on_tool_call_received(&received("call-64", "Bash", json!("newest")));
    observer.on_tool_pre(&pre("call-0", "Bash"));
    observer.on_tool_pre(&pre("call-64", "Bash"));
    let tools = kept.active_tools.lock().unwrap().clone();
    let stale = tools
        .iter()
        .find(|tool| tool.call_id == "call-0")
        .expect("the pre-cap call dispatched");
    assert!(
        stale.input_summary.is_empty(),
        "the stash dropped wholesale once past the cap, so pre-cap entries render no summary"
    );
    let newest = tools
        .iter()
        .find(|tool| tool.call_id == "call-64")
        .expect("the post-cap call dispatched");
    assert!(
        newest.input_summary.contains("newest"),
        "entries stashed after the drop render their summary"
    );
}

#[test]
fn every_state_mutation_notifies() {
    let (observer, kept) = state_and_observer();

    let listener = kept.render_notify.listen();
    observer.on_text_delta(&delta("a"));
    futures::executor::block_on(listener);

    let listener = kept.render_notify.listen();
    observer.on_response(&ResponseContext {
        turn: 0,
        text: String::new(),
        usage: None,
    });
    futures::executor::block_on(listener);

    let listener = kept.render_notify.listen();
    observer.on_tool_pre(&pre("call-1", "Edit"));
    futures::executor::block_on(listener);

    let listener = kept.render_notify.listen();
    observer.finish_tool("call-1", "Edit", false, Duration::from_millis(1));
    futures::executor::block_on(listener);

    let listener = kept.render_notify.listen();
    observer.on_stream_success(&stream(0, 1, 2));
    futures::executor::block_on(listener);

    let listener = kept.render_notify.listen();
    observer.on_turn_end(&turn_end(0, 1, 2));
    futures::executor::block_on(listener);

    let listener = kept.render_notify.listen();
    observer.reset();
    futures::executor::block_on(listener);
}

#[test]
fn a_thousand_deltas_without_a_listener_complete_promptly() {
    let (observer, kept) = state_and_observer();
    let start = std::time::Instant::now();
    for i in 0..1000 {
        observer.on_text_delta(&delta(&i.to_string()));
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "1000 deltas took {elapsed:?} — the observer must never wait on the display"
    );
    let text = kept.streaming_text.lock().unwrap().clone();
    assert_eq!(text.chars().count(), 2890, "every delta concatenated");
}

#[test]
fn send_sync_and_the_observer_names_itself() {
    fn as_trait(object: &TuiObserver) -> &(dyn LoopObserver + Send + Sync) {
        object
    }
    let (observer, _kept) = state_and_observer();
    assert_eq!(as_trait(&observer).name(), "tui");
}

#[test]
fn a_poisoned_buffer_is_recovered_not_propagated() {
    let (observer, kept) = state_and_observer();
    let shared = std::sync::Arc::new(kept);
    let poisoning = std::sync::Arc::clone(&shared);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = poisoning.streaming_text.lock().expect("the streaming lock");
        panic!("poison the streaming buffer");
    }));
    assert!(panicked.is_err(), "the poisoning panic must unwind");
    observer.on_text_delta(&delta("after"));
    let written = shared
        .streaming_text
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        written.contains("after"),
        "the observer keeps writing through a poisoned lock"
    );
}

#[test]
fn a_long_input_stashes_its_extracted_value_not_truncated_json() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received(
        "call-1",
        "Edit",
        json!({
            "file_path": "src/lib.rs",
            "old_string": "fn main() { let x = 1; }",
            "new_string": "fn main() { let x = 2; }"
        }),
    ));
    observer.on_tool_pre(&pre("call-1", "Edit"));
    let tools = kept.active_tools.lock().unwrap().clone();
    let summary = &tools.first().expect("one active tool").input_summary;
    assert_eq!(
        summary, "src/lib.rs",
        "the stash holds the extracted value — capped JSON would no longer humanize: {summary}"
    );
}

#[test]
fn batch_tools_stash_their_counts_with_the_value() {
    let (observer, kept) = state_and_observer();
    observer.on_tool_call_received(&received(
        "call-1",
        "MultiEdit",
        json!({
            "edits": [
                {"file_path": "a.rs"},
                {"file_path": "b.rs"},
                {"file_path": "c.rs"}
            ]
        }),
    ));
    observer.on_tool_call_received(&received(
        "call-2",
        "TodoWrite",
        json!({"todos": [{"a": 1}, {"b": 2}]}),
    ));
    observer.on_tool_pre(&pre("call-1", "MultiEdit"));
    observer.on_tool_pre(&pre("call-2", "TodoWrite"));
    let tools = kept.active_tools.lock().unwrap().clone();
    let multiedit = &tools.first().expect("the MultiEdit").input_summary;
    assert_eq!(
        multiedit, "a.rs (3 edits)",
        "the count lives in the stash — it cannot be recovered at render: {multiedit}"
    );
    let todo = &tools.get(1).expect("the TodoWrite").input_summary;
    assert_eq!(
        todo, "2 items",
        "a list-only input carries its size as the value: {todo}"
    );
}
