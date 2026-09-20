//! The conversation message model — plain display data.
//!
//! These types carry what the TUI renders. They hold no behavior: the
//! conversation view decides how to paint each message, the session
//! layer decides how to serialize it.
//!
//! The serde representation is the frozen on-disk schema: variants
//! carry an internal `role`/`type` tag with `snake_case` names, so a
//! serialized message reads `{"role":"user",…}` and a block
//! `{"type":"tool",…}`. Changing a tag key or variant name is a
//! format break, not a refactor.

use chrono::{DateTime, Utc};

/// One rendered conversation message.
///
/// The block model splits assistant output into ordered
/// [`ContentBlock`]s so text and tool activity can interleave
/// faithfully; user, system, and error messages stay plain text.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TuiMessage {
    /// Text the user submitted from the input line.
    ///
    /// Carries the raw submitted string; multi-line input keeps its
    /// newlines and renders as-is.
    User {
        /// The submitted text, verbatim.
        ///
        /// Multi-line input keeps its newlines and renders as-is.
        text: String,

        /// When the message was submitted.
        ///
        /// Recorded at submit time.
        timestamp: DateTime<Utc>,
    },

    /// A completed assistant reply.
    ///
    /// Blocks appear in emission order; the renderer walks them
    /// top-to-bottom so tool activity sits between the text around it.
    Assistant {
        /// The reply's content blocks, in emission order.
        ///
        /// Text and tool blocks interleave exactly as they were
        /// produced.
        blocks: Vec<ContentBlock>,

        /// When the reply finished.
        ///
        /// Recorded once all blocks are in.
        timestamp: DateTime<Utc>,

        /// How long the reply took, in milliseconds.
        ///
        /// `None` while the duration is unknown or unmeasured; absent
        /// from the serialized form rather than written as `null`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },

    /// An informational line outside the conversation proper.
    ///
    /// Rendered subdued; used for lifecycle notes like session resume.
    System {
        /// The informational text.
        ///
        /// Rendered subdued, outside the reply flow.
        text: String,

        /// When the note was recorded.
        ///
        /// Recorded when the note is pushed.
        timestamp: DateTime<Utc>,
    },

    /// A failure surfaced to the conversation.
    ///
    /// Rendered with the error styling so failures stand out.
    Error {
        /// The error text.
        ///
        /// Rendered with the error styling so failures stand out.
        text: String,

        /// When the error occurred.
        ///
        /// Recorded when the failure surfaces.
        timestamp: DateTime<Utc>,
    },
}

/// A block within an assistant message.
///
/// Blocks carry the reply's content in order; the renderer walks them
/// so tool summaries sit between the text around it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// A stretch of assistant text, possibly markdown.
    ///
    /// The renderer formats it through the markdown pipeline with the
    /// assistant base color.
    Text {
        /// The text to render.
        ///
        /// Formatted as markdown with the assistant base color.
        text: String,
    },

    /// A tool call's summary line.
    ///
    /// Condenses the call to what a scrolling reader needs: the tool
    /// name, a one-line input preview, the outcome, and the elapsed
    /// time; full output stays out of the conversation flow.
    Tool {
        /// The invoked tool's name.
        ///
        /// As reported by the dispatcher.
        name: String,

        /// The model-issued call id.
        ///
        /// Keys the runtime expansion data — the full input and
        /// output captured for this call — which outlives the block
        /// only in memory, never in the serialized session. Empty on
        /// blocks from sessions recorded before the field existed.
        #[serde(default)]
        call_id: String,

        /// A one-line summary of the call's input.
        ///
        /// Condensed from the full arguments.
        input_preview: String,

        /// Whether the call succeeded.
        ///
        /// Picks the line's success or failure styling.
        success: bool,

        /// How long the call took, in seconds.
        ///
        /// Wall-clock time from dispatch to completion.
        elapsed_secs: f64,

        /// The call's retained output, sanitized and capped.
        ///
        /// What the dispatch-side capture kept — redacted where
        /// redaction is on, head and tail within the retention
        /// cap. Persisted so a resumed session's model remembers
        /// what the call actually returned; empty on blocks saved
        /// before the capture existed.
        output_preview: String,

        /// The call's retained input, pretty-printed JSON, capped.
        ///
        /// The full arguments as the model supplied them, within
        /// the same retention cap. A resumed session rebuilds the
        /// tool call's real arguments from this; empty on blocks
        /// saved before the capture existed.
        #[serde(default)]
        retained_input: String,
    },
}

/// Token usage counters shared with the observer.
///
/// Populated by the observer as the session runs; the status bar
/// reads the cumulative totals. Stays at zero until an agent feeds
/// it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    /// Input tokens of the most recent turn.
    ///
    /// Overwritten at each stream or turn-end event; no event fires
    /// at a turn's start, so the value keeps the previous turn's
    /// counts until the next one reports.
    pub input: u64,

    /// Output tokens of the most recent turn.
    ///
    /// Overwritten at each stream or turn-end event; no event fires
    /// at a turn's start, so the value keeps the previous turn's
    /// counts until the next one reports.
    pub output: u64,

    /// Input tokens since the session began.
    ///
    /// The status bar's total reads this.
    pub cumulative_input: u64,

    /// Output tokens since the session began.
    ///
    /// The status bar's total reads this.
    pub cumulative_output: u64,
}

/// A tool currently in flight.
///
/// The observer records one entry per dispatched call and removes it
/// on completion; the display layer reads the list to show progress.
#[derive(Debug, Clone)]
pub struct ActiveTool {
    /// The model-issued call id.
    ///
    /// Pairs the pre and post lifecycle events exactly — including
    /// same-tool retries and parallel calls, where the name alone is
    /// ambiguous. Display code ignores it.
    pub call_id: String,

    /// The invoked tool's name.
    ///
    /// As reported by the dispatcher.
    pub name: String,

    /// A one-line summary of the call's input.
    ///
    /// Condensed from the full arguments when the call is received;
    /// empty when no summary was stashed.
    pub input_summary: String,

    /// When the call was dispatched.
    ///
    /// Elapsed time derives from this monotonically increasing
    /// clock.
    pub start: std::time::Instant,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_the_frozen_tags() {
        let now = DateTime::parse_from_rfc3339("2026-09-16T12:00:00Z")
            .expect("a fixed timestamp")
            .with_timezone(&Utc);
        let messages = vec![
            TuiMessage::User {
                text: "hi\nthere".to_string(),
                timestamp: now,
            },
            TuiMessage::Assistant {
                blocks: vec![
                    ContentBlock::Text {
                        text: "looking".to_string(),
                    },
                    ContentBlock::Tool {
                        name: "Read".to_string(),
                        call_id: "call_1".to_string(),
                        input_preview: "a.rs".to_string(),
                        success: true,
                        elapsed_secs: 1.5,
                        output_preview: "file contents".to_string(),
                        retained_input: "{\n  \"path\": \"a.rs\"\n}".to_string(),
                    },
                ],
                timestamp: now,
                duration_ms: None,
            },
            TuiMessage::System {
                text: "resumed".to_string(),
                timestamp: now,
            },
            TuiMessage::Error {
                text: "boom".to_string(),
                timestamp: now,
            },
        ];
        let json = serde_json::to_string(&messages).expect("the model serializes");
        assert!(
            json.contains(r#""role":"user""#),
            "variant tags are the frozen role tags: {json}"
        );
        assert!(
            json.contains(r#""type":"tool""#),
            "block tags are the frozen type tags: {json}"
        );
        assert!(
            !json.contains("duration_ms"),
            "an absent duration is omitted from the serialized form, not nulled: {json}"
        );
        let back: Vec<TuiMessage> = serde_json::from_str(&json).expect("the model parses back");
        assert_eq!(back, messages, "a round-trip is lossless");

        let old = r#"[{"role":"assistant","blocks":[{"type":"tool","name":"Read","input_preview":"a.rs","success":true,"elapsed_secs":1.5,"output_preview":"…"}],"timestamp":"2026-09-16T12:00:00Z"}]"#;
        let parsed: Vec<TuiMessage> =
            serde_json::from_str(old).expect("sessions recorded before call ids parse");
        assert!(
            parsed.first().is_some_and(|message| matches!(message,
                TuiMessage::Assistant { blocks, .. }
                if blocks.iter().any(|block| matches!(block,
                    ContentBlock::Tool { call_id, retained_input, .. }
                        if call_id.is_empty() && retained_input.is_empty())))),
            "a tool block without a call id or retained input defaults both empty"
        );
    }
}
