//! Types for the interactive question protocol between tools and the UI.
//!
//! A tool that needs input from the user sends a [`QuestionRequest`] over the
//! runner context's channel; the UI answers via the per-question one-shot
//! channel.

/// A request from a tool to ask the user one or more questions.
pub struct QuestionRequest {
    /// The questions to present to the user.
    pub questions: Vec<Question>,
}

/// A single question to put to the user.
pub struct Question {
    /// The question text.
    pub question: String,
    /// Optional short label shown above the question (at most ~12 characters).
    pub header: Option<String>,
    /// The selectable answers.
    pub options: Vec<QuestionOption>,
    /// Whether multiple options may be selected.
    pub multi_select: bool,
    /// One-shot channel carrying the user's answer back to the asking tool.
    pub response_tx: tokio::sync::oneshot::Sender<QuestionResponse>,
}

/// One selectable answer for a [`Question`].
#[derive(Debug, Clone)]
pub struct QuestionOption {
    /// The answer text shown to and returned for the user.
    pub label: String,
    /// Optional longer explanation of this answer.
    pub description: Option<String>,
}

/// The user's answer(s) to a single [`Question`].
#[derive(Debug, Clone)]
pub struct QuestionResponse {
    /// The question that was answered.
    pub question: String,
    /// The selected answer labels.
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
