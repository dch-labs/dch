//! Session naming — the few words a session file carries as its name.
//!
//! A session id says where a conversation lives; a timestamp says when
//! it last moved. Neither says what it was *about*, which is the only
//! question a user picking a resume actually asks. So the first prompt
//! a session claims fires one small side call to the session's own
//! model — a utility request, the compaction chain's discipline: the
//! session's thinking budget stripped, no tools, a one-shot
//! non-streaming reply — and the sanitized few words that come back
//! ride the envelope from the next save onward. A call that fails
//! leaves the session untitled and the next claim tries again; a
//! title, once landed, is never regenerated.

use loopctl::api::ApiClient;
use loopctl::api::StreamRequest;
use loopctl::message::Message;
use loopctl::message::MessagePart;

use crate::provider::create_compactor_client;

/// The system prompt that frames the naming call.
///
/// The prompt asks for the few words a session is filed under and
/// nothing else. Its opening sentence doubles as the request's
/// identity on the wire: a one-shot naming request is recognizable by
/// this prompt alone, so the sentence stays plain ASCII and
/// byte-for-byte stable — anything that matches this prompt travels
/// with every change to it.
pub const NAME_SYSTEM_PROMPT: &str = "You name terminal agent sessions. Read the user's \
     first prompt and reply with only a title for the session it opens: two to five \
     words, plain text, no quotes, no surrounding punctuation, no emoji, nothing else.";

/// How much of the first prompt rides the naming call.
///
/// A title needs the ask, not the appendix: the head of the prompt
/// carries the subject, and the cap keeps a pasted log from paying its
/// full token price for three words of answer.
const PROMPT_CHAR_CAP: usize = 2_000;

/// The longest title kept.
///
/// A model that answers in a sentence still lands a title that fits a
/// listing row; the cut is by characters on the reply's first line.
const TITLE_CHAR_CAP: usize = 60;

/// Name a session from its first prompt, over the session's provider.
///
/// Builds the utility client (the session's endpoint and model with
/// the thinking budget stripped — the same shape the compaction chain
/// speaks to) and asks it for the title. The call is one-shot and
/// best-effort: any failure to build the client or complete the call
/// returns `None`, leaving the session untitled and the naming debt
/// to a later claim.
pub async fn session_title(config: &dch_config::ApiConfig, first_prompt: &str) -> Option<String> {
    let client = match create_compactor_client(config) {
        Ok(client) => client,
        Err(err) => {
            tracing::debug!(error = %err, "session-title client could not be built");
            return None;
        }
    };
    title_from_client(&client, first_prompt).await
}

/// The naming call itself, over any client.
///
/// Split from [`session_title`] so tests drive the request and its
/// sanitation with a mock client instead of a server. The prompt is
/// capped to its head, framed by [`NAME_SYSTEM_PROMPT`], and sent with
/// no tools; the reply is reduced to its first line, stripped of
/// markdown and quoting decoration, and capped to a listing row's
/// width.
pub async fn title_from_client(client: &dyn ApiClient, first_prompt: &str) -> Option<String> {
    let capped: String = first_prompt.chars().take(PROMPT_CHAR_CAP).collect();
    let request = StreamRequest::new(vec![Message::user(capped)])
        .with_system(Some(NAME_SYSTEM_PROMPT.to_string()));
    match client.create_message(&request).await {
        Ok(response) => cleaned_title(&text_of(&response.message)),
        Err(err) => {
            tracing::debug!(error = %err, "session-title call failed; the session stays unnamed");
            None
        }
    }
}

/// Every text part of a message, concatenated in order.
///
/// A reply that opened with a thought block still names the session:
/// its text parts carry the words, whatever else rode the message.
fn text_of(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Reduce a naming reply to a title, or `None` when it holds none.
///
/// The words the model underlined — heading hashes, wrapping quotes
/// and backticks, trailing sentence punctuation — are decoration the
/// reply can wear without changing the title underneath; whitespace
/// collapses so a wrapped line still reads as one phrase. An answer
/// that is blank after all of that names nothing.
fn cleaned_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().next().unwrap_or_default();
    // Decoration nests — a backtick behind a period, a quote outside a
    // comma — so the strips alternate until nothing more comes off.
    let mut title = first_line;
    loop {
        let before = title;
        title = title.trim();
        while let Some(rest) = title.strip_prefix('#') {
            title = rest.trim_start();
        }
        title = title.trim_start_matches(['"', '\'', '`']);
        title = title.trim_end_matches(['.', ',', ';', ':', '!', '?', '"', '\'', '`']);
        if title == before {
            break;
        }
    }
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let capped: String = collapsed.chars().take(TITLE_CHAR_CAP).collect();
    Some(capped)
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

    /// A naming request answers with this canned title through the mock.
    const CANNED: &str = "Fix the login loop";

    fn mock() -> loopctl::testing::MockApiClient {
        loopctl::testing::MockApiClient::new("test-model").with_text_response(CANNED)
    }

    #[tokio::test]
    async fn a_clean_reply_becomes_the_title() {
        assert_eq!(
            title_from_client(&mock(), "please fix the login").await,
            Some(CANNED.to_string())
        );
    }

    #[tokio::test]
    async fn a_decorated_reply_is_reduced_to_its_words() {
        let decorated = loopctl::testing::MockApiClient::new("m")
            .with_text_response("  # `Fix the Login Loop`.  \nSure — happy to help!");
        assert_eq!(
            title_from_client(&decorated, "fix the login").await,
            Some("Fix the Login Loop".to_string())
        );
    }

    #[tokio::test]
    async fn a_blank_reply_names_nothing() {
        for blank in ["", "   ", "\n\n"] {
            let empty = loopctl::testing::MockApiClient::new("m").with_text_response(blank);
            assert_eq!(
                title_from_client(&empty, "anything").await,
                None,
                "a blank reply must not become a title"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_call_leaves_the_session_unnamed() {
        let failing = loopctl::testing::MockApiClient::new("m").with_error("endpoint down");
        assert_eq!(title_from_client(&failing, "name me").await, None);
    }

    #[tokio::test]
    async fn a_long_reply_is_capped_to_a_listing_row() {
        let wordy = loopctl::testing::MockApiClient::new("m")
            .with_text_response("word ".repeat(40).trim_end());
        let title = title_from_client(&wordy, "prompt").await;
        let title = title.expect("a wordy reply still names the session");
        assert!(
            title.chars().count() <= TITLE_CHAR_CAP,
            "the title fits a row: {title:?}"
        );
    }

    #[tokio::test]
    async fn the_request_carries_the_marker_and_a_capped_prompt() {
        let client = mock();
        let long_prompt: String = "x".repeat(PROMPT_CHAR_CAP + 500);
        let _ = title_from_client(&client, &long_prompt).await;
        let requests = client.captured_requests();
        let request = requests
            .first()
            .expect("the naming call reached the client");
        assert_eq!(
            request.system.as_deref(),
            Some(NAME_SYSTEM_PROMPT),
            "the naming system prompt — marker included — rides the request"
        );
        let sent: String = request
            .messages
            .first()
            .expect("one user message")
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(
            sent.chars().count(),
            PROMPT_CHAR_CAP,
            "only the head of a pasted novella rides the call"
        );
        assert!(
            request.tools.is_none(),
            "the naming call advertises no tools"
        );
    }
}
