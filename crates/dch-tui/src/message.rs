//! The conversation message model — plain display data.
//!
//! These types carry what the TUI renders. They hold no behavior: the
//! conversation view decides how to paint each message, the session
//! layer decides how to serialize it.

use chrono::{DateTime, Utc};

/// One rendered conversation message.
///
/// The block model splits assistant output into ordered
/// [`ContentBlock`]s so text and tool activity can interleave
/// faithfully; user, system, and error messages stay plain text.
#[derive(Debug, Clone)]
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
        /// `None` while the duration is unknown or unmeasured.
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
/// Blocks carry the reply's content in order; the renderer walks
/// them so tool summaries sit between the text around them.
#[derive(Debug, Clone)]
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

        /// A short preview of the call's output.
        ///
        /// Truncated to what a scrolling reader needs.
        output_preview: String,
    },
}

/// Token usage counters shared with the observer.
///
/// Populated by the observer as the session runs; the status bar
/// reads the cumulative totals. Stays at zero until an agent feeds
/// it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    /// Input tokens of the current turn.
    ///
    /// Resets when a new turn starts.
    pub input: u64,

    /// Output tokens of the current turn.
    ///
    /// Resets when a new turn starts.
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
