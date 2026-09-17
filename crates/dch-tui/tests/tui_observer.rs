//! `TuiObserver` tests — pure state mutation driven with hand-built
//! contexts; no real loop, no terminal, no network. The completion
//! path goes through the plain-args helper (the post context is not
//! constructible outside its crate).

#![allow(
    clippy::uninlined_format_args,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::time::Duration;

use dch_tui::TokenCounts;
use dch_tui::observer::{TuiObserver, TuiObserverState};
use loopctl::observer::{
    LoopObserver, ResponseContext, StreamContext, TextDeltaContext, ToolCallReceivedContext,
    ToolPreContext, TurnEndContext,
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
    ToolPreContext {
        turn: 0,
        tool: tool.to_string(),
        tool_call_id: call_id.to_string(),
    }
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
    TurnEndContext {
        turn,
        success: true,
        error: None,
        duration_ms: 10,
        input_tokens,
        output_tokens,
    }
}

fn failed_turn_end(turn: usize, input_tokens: u64, output_tokens: u64) -> TurnEndContext {
    TurnEndContext {
        turn,
        success: false,
        error: Some("the stream died mid-reply".to_string()),
        duration_ms: 10,
        input_tokens,
        output_tokens,
    }
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
    let replies = kept.completed_replies.lock().unwrap().clone();
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
        kept.completed_replies.lock().unwrap().clone(),
        vec!["the whole reply".to_string()],
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
    let results = kept.tool_results.lock().unwrap().clone();
    let result = results.first().expect("one completed result");
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
    assert!(kept.completed_replies.lock().unwrap().is_empty());
    assert!(kept.active_tools.lock().unwrap().is_empty());
    assert!(kept.tool_results.lock().unwrap().is_empty());
    assert!(
        kept.errors.lock().unwrap().is_empty(),
        "the driver's error buffer clears with the rest"
    );
    assert_eq!(tokens_of(&kept), TokenCounts::default());
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
