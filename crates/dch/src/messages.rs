//! Transcript assembly for headless runs.
//!
//! Headless mode prints as it goes and holds no display state, so
//! its saved transcript is assembled once, after the run returns,
//! from the turn outputs the runner recorded. The mapping records
//! text turns and failures — the display-grade minimum; tool-call
//! detail is not part of the headless transcript. Because the
//! outputs come from the session's own records, a run that completed
//! turns before failing still saves those turns.

use dch_tui::{ContentBlock, TuiMessage};
use loopctl::error::LoopError;
use loopctl::message::{Message, MessagePart, Role};

/// Build the transcript of one headless run.
///
/// The submitted prompt becomes the user message; every recorded
/// turn output becomes an assistant message holding that text —
/// including turns a failed run completed before its error — and a
/// failure, when there is one, closes the transcript as an error
/// message. Timestamps are mapping-time; the envelope's save stamp
/// carries the real clock.
pub(crate) fn transcript_from(
    prompt: &str,
    turn_outputs: &[String],
    failure: Option<&LoopError>,
) -> Vec<TuiMessage> {
    let now = chrono::Utc::now();
    let mut messages = vec![TuiMessage::User {
        text: prompt.to_string(),
        timestamp: now,
        steered: false,
        unsent: false,
    }];
    for output in turn_outputs {
        if output.trim().is_empty() {
            continue;
        }
        messages.push(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: output.clone(),
            }],
            timestamp: now,
            duration_ms: None,
        });
    }
    if let Some(err) = failure {
        messages.push(TuiMessage::Error {
            text: err.to_string(),
            timestamp: now,
        });
    }
    messages
}

/// The prompt a session's naming call coins from.
///
/// The conversation's first user prompt: the first user message in the
/// agent's history whose text parts carry words — for a resumed
/// session, the prompt that opened its conversation — falling back to
/// the prompt that armed the call when the history holds none, the
/// fresh session's shape. A session whose naming call never answered
/// thereby retries against its conversation's own opening ask rather
/// than against whatever follow-up happened to arm the retry. The
/// chosen message's text parts join with newlines, in order.
pub(crate) fn naming_subject(messages: &[Message], claimed: &str) -> String {
    messages
        .iter()
        .filter(|message| matches!(message.role, Role::User))
        .find_map(|message| {
            let text = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    MessagePart::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        })
        .unwrap_or_else(|| claimed.to_string())
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

    fn outputs(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_successful_run_maps_to_user_then_assistants() {
        let transcript = transcript_from("do it", &outputs(&["first", "", "second"]), None);
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
    fn a_failed_run_still_saves_its_completed_turns() {
        let err = loopctl::error::LoopError::Api("provider refused".to_string());
        let transcript = transcript_from("do it", &outputs(&["first turn complete"]), Some(&err));
        assert_eq!(
            transcript.len(),
            3,
            "the user message, the completed turn, and the failure"
        );
        assert!(matches!(
            &transcript[1],
            TuiMessage::Assistant { blocks, .. }
                if matches!(&blocks[0], ContentBlock::Text { text } if text == "first turn complete")
        ));
        assert!(matches!(
            &transcript[2],
            TuiMessage::Error { text, .. } if text.contains("provider refused")
        ));
    }

    #[test]
    fn a_failure_with_no_completed_turns_saves_prompt_and_error() {
        let err = loopctl::error::LoopError::Api("provider refused".to_string());
        let transcript = transcript_from("do it", &[], Some(&err));
        assert_eq!(
            transcript.len(),
            2,
            "nothing was produced — the prompt and the failure alone"
        );
        assert!(matches!(
            &transcript[1],
            TuiMessage::Error { text, .. } if text.contains("provider refused")
        ));
    }

    #[test]
    fn a_fresh_history_names_from_the_claimed_prompt() {
        let subject = naming_subject(&[], "do it now");
        assert_eq!(
            subject, "do it now",
            "no history — the arming prompt is the subject"
        );
    }

    #[test]
    fn a_restored_history_names_from_its_first_user_prompt() {
        let history = vec![
            Message::user("map the coastline roads"),
            Message::assistant("the coast is mapped"),
            Message::user("now chart the tides"),
        ];
        let subject = naming_subject(&history, "now chart the tides");
        assert_eq!(
            subject, "map the coastline roads",
            "the opening ask is the subject, not the arming follow-up"
        );
    }

    #[test]
    fn a_user_message_without_words_is_skipped_for_one_that_has_them() {
        let history = vec![
            Message::user("   "),
            Message::assistant("quiet"),
            Message::user("the real opening ask"),
        ];
        let subject = naming_subject(&history, "the arming prompt");
        assert_eq!(subject, "the real opening ask");
    }

    #[test]
    fn a_multi_part_user_prompt_joins_its_text_parts() {
        let opening = Message::new(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "map the coastline roads".to_string(),
                },
                MessagePart::Text {
                    text: "(start at the harbour)".to_string(),
                },
            ],
        );
        let history = vec![opening, Message::assistant("done")];
        let subject = naming_subject(&history, "the arming prompt");
        assert_eq!(subject, "map the coastline roads\n(start at the harbour)");
    }
}
