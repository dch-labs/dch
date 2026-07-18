//! Types for the interactive question protocol between tools and the UI.
//!
//! A tool that needs input from the user sends a [`QuestionRequest`] over the
//! runner context's channel; the UI answers via the per-question one-shot
//! channel.

/// A request from a tool to ask the user one or more questions.
pub struct QuestionRequest {
    /// The questions to present to the user. Sent as a batch so the UI can
    /// render them together; each carries its own response channel.
    pub questions: Vec<Question>,
}

/// A single question to put to the user.
pub struct Question {
    /// The question text.
    pub question: String,
    /// Optional short label shown above the question in the UI. Should be at
    /// most ~12 characters so it fits a chip/tag header.
    pub header: Option<String>,
    /// The selectable answers. Should be non-empty; the UI renders one option
    /// per row and returns the chosen [`QuestionOption::label`] values.
    pub options: Vec<QuestionOption>,
    /// Whether multiple options may be selected. When `false` the UI should
    /// return exactly one answer; when `true` it may return several.
    pub multi_select: bool,
    /// One-shot channel carrying the user's answer back to the asking tool.
    /// The tool awaits this; the UI sends exactly one [`QuestionResponse`]
    /// and then the channel closes.
    pub response_tx: tokio::sync::oneshot::Sender<QuestionResponse>,
}

/// One selectable answer for a [`Question`].
#[derive(Debug, Clone)]
pub struct QuestionOption {
    /// The answer text, shown to the user and echoed back in
    /// [`QuestionResponse::answers`] when selected. This is the value the
    /// asking tool matches against.
    pub label: String,
    /// Optional longer explanation of this answer, shown beneath the label.
    pub description: Option<String>,
}

/// The user's answer(s) to a single [`Question`].
#[derive(Debug, Clone)]
pub struct QuestionResponse {
    /// The question text that was answered. Echoed back so the asking tool can
    /// correlate the response to the question it posed.
    pub question: String,
    /// The selected answer labels. Empty if the user dismissed the question,
    /// one entry for single-select, or several when `multi_select` was `true`.
    pub answers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_request_can_be_built() {
        let (tx, _rx) = tokio::sync::oneshot::channel::<QuestionResponse>();
        let _req = QuestionRequest {
            questions: vec![Question {
                question: "Continue?".to_string(),
                header: None,
                options: vec![QuestionOption {
                    label: "Yes".to_string(),
                    description: None,
                }],
                multi_select: false,
                response_tx: tx,
            }],
        };
    }
}
