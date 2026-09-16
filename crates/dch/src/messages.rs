//! Transcript assembly for headless runs.
//!
//! Headless mode prints as it goes and holds no display state, so
//! its saved transcript is assembled once, after the run returns,
//! from the record the agent loop kept. The mapping records text
//! turns and failures — the display-grade minimum; tool-call detail
//! is not part of the headless transcript.

use dch_tui::{ContentBlock, TuiMessage};
use loopctl::engine::Run;
use loopctl::error::LoopError;

/// Build the transcript of one headless run.
///
/// The submitted prompt becomes the user message; each turn with
/// output becomes an assistant message holding that text; a failed
/// run appends the failure as an error message, so a cancelled or
/// errored session still saves what happened. Timestamps are
/// mapping-time — the envelope's save stamp carries the real clock.
pub(crate) fn transcript_from_run(
    prompt: &str,
    result: &Result<Run, LoopError>,
) -> Vec<TuiMessage> {
    let now = chrono::Utc::now();
    let mut messages = vec![TuiMessage::User {
        text: prompt.to_string(),
        timestamp: now,
    }];
    if let Ok(run) = result {
        for turn in &run.turns {
            if turn.output.trim().is_empty() {
                continue;
            }
            messages.push(TuiMessage::Assistant {
                blocks: vec![ContentBlock::Text {
                    text: turn.output.clone(),
                }],
                timestamp: now,
                duration_ms: None,
            });
        }
    }
    if let Err(err) = result {
        messages.push(TuiMessage::Error {
            text: err.to_string(),
            timestamp: now,
        });
    }
    messages
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn run_with_outputs(outputs: &[&str]) -> Run {
        let mut run = Run::new("prompt", &loopctl::engine::RunConfig::default());
        for (index, output) in outputs.iter().enumerate() {
            run.turns.push(loopctl::engine::Turn {
                turn: index,
                input: String::new(),
                output: (*output).to_string(),
                tool_calls: Vec::new(),
                input_tokens: 0,
                output_tokens: 0,
            });
        }
        run
    }

    #[test]
    fn a_successful_run_maps_to_user_then_assistants() {
        let transcript =
            transcript_from_run("do it", &Ok(run_with_outputs(&["first", "", "second"])));
        assert_eq!(
            transcript.len(),
            3,
            "the user message plus two non-empty turns"
        );
        assert!(matches!(&transcript[0], TuiMessage::User { text, .. } if text == "do it"));
        assert!(matches!(
            &transcript[1],
            TuiMessage::Assistant { blocks, .. }
                if matches!(&blocks[0], ContentBlock::Text { text } if text == "first")
        ));
        assert!(matches!(
            &transcript[2],
            TuiMessage::Assistant { blocks, .. }
                if matches!(&blocks[0], ContentBlock::Text { text } if text == "second")
        ));
    }

    #[test]
    fn a_failed_run_maps_to_an_error_message() {
        let err = loopctl::error::LoopError::Api("provider refused".to_string());
        let transcript = transcript_from_run("do it", &Err(err));
        assert_eq!(transcript.len(), 2, "the user message plus the failure");
        assert!(matches!(
            &transcript[1],
            TuiMessage::Error { text, .. } if text.contains("provider refused")
        ));
    }
}
