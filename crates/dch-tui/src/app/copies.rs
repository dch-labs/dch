//! The copy servo: selection, answer, and session-id copies, the
//! transport scheduler that orders them, and the notices each
//! transport's verdict posts.

use super::TuiApp;
use super::clipboard::CopyAnswer;
use super::clipboard::{
    CopyOutcomes, CopyVerdict, clipboard_env, configured_copy_command, fenced_process_writes,
    host_transports, next_attempt, transport_answer, transport_commands,
};
use super::{Arc, PoisonError};
use crate::message::ContentBlock;
use crate::message::TuiMessage;

impl TuiApp {
    /// Copy the most recent answer to the system clipboard.
    ///
    /// Walks the conversation back to the last assistant turn and lifts its
    /// text blocks (thinking and tool chrome aside), handing them to the same
    /// clipboard path a selection copy uses. With no answer yet it says so
    /// rather than copying an empty string.
    pub(in crate::app) fn copy_command(&mut self) {
        let answer = self
            .conversation
            .iter()
            .rev()
            .find_map(|message| match message {
                TuiMessage::Assistant { blocks, .. } => {
                    let text: String = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!text.trim().is_empty()).then_some(text)
                }
                _ => None,
            });
        match answer {
            Some(text) => self.copy_selection_and_notice(&text),
            None => self.post_notice("no answer to copy yet".to_string()),
        }
    }

    /// Copy the live selection, if any.
    ///
    /// The terminal-standard copy chord. A span that covered no
    /// characters leaves the clipboard alone — the same discipline
    /// the release and the keyboard walk apply. A delivered copy
    /// lands its confirmation on the notice row, like every path.
    pub(in crate::app) fn copy_selection_now(&mut self) {
        let covered = self
            .pointer
            .selection
            .as_ref()
            .map(|selection| self.selection_text(selection));
        if let Some((text, covered_any)) = covered
            && covered_any
        {
            self.copy_selection_and_notice(&text);
        }
    }

    /// Hand a completed selection's text to the clipboard and say so.
    ///
    /// The funnel every copy path — the mouse release, the copy chord,
    /// and the keyboard walk's re-copy — delivers through, so each one
    /// posts from the same vocabulary on the notice row above the
    /// composer. Every copy takes its attempt id here and records it
    /// as the wording that holds the row; the immediate post answers
    /// with what the copier knows right now, and when the production
    /// chain has transports in flight, the frame drain replaces it
    /// with the same attempt's verdict — never with one older than
    /// what a newer copy already posted.
    pub(in crate::app) fn copy_selection_and_notice(&mut self, text: &str) {
        let attempt = next_attempt(&self.copy.copy_attempts);
        let answer = (self.copy.copier)(attempt, text);
        self.copy.posted_copy_attempt = attempt;
        let wording = copy_answer_notice(answer);
        if !wording.is_empty() {
            self.post_notice(wording.to_string());
        }
    }

    /// Copy the session id when the click landed inside its segment.
    ///
    /// Reads the span recorded by the last status-bar render; a click
    /// inside it routes the id through the same transport and notice
    /// path a text selection uses. Returns whether the click consumed
    /// itself here, so the caller can treat a bar click as handled.
    pub(in crate::app) fn copy_session_id_at(&mut self, column: u16, row: u16) -> bool {
        let Some((span_row, start, end)) = self.host.session_id_span.get() else {
            return false;
        };
        if row != span_row || column < start || column >= end {
            return false;
        }
        if let Some(id) = self.host.session_id.clone() {
            self.copy_selection_and_notice(&id);
        }
        true
    }
}

/// The notice wording a synchronous copy answer earns.
///
/// Word-for-word the vocabulary [`copy_verdict_notice`] settles on, so
/// the immediate phase never claims more than the phase after it.
#[must_use]
pub(in crate::app) fn copy_answer_notice(answer: CopyAnswer) -> &'static str {
    match answer {
        CopyAnswer::Verified => "selection copied",
        CopyAnswer::Unconfirmed => "",
        CopyAnswer::Failed => "copy failed — set display.copy_command",
    }
}

/// The notice wording a copy's verdict settles on.
///
/// A verified transport earned the plain claim; an escape that only
/// left the process says so; nothing at all names the escape hatch.
#[must_use]
pub(in crate::app) fn copy_verdict_notice(verdict: CopyVerdict) -> &'static str {
    if verdict.verified {
        "selection copied"
    } else if verdict.escape_written {
        ""
    } else {
        "copy failed — set display.copy_command"
    }
}

/// Hand `text` to every clipboard transport the environment offers.
///
/// The terminal-side OSC 52 escape runs inline — one buffered write —
/// and every process transport moves to a background task bounded by
/// one shared kill deadline, so a hanging helper or a child that
/// never reads its stdin cannot freeze input and rendering. The
/// transports' jobs run through the ordered lane: each takes its
/// turn under the process-wide write lock, a turn granted after a
/// newer copy has started is declined outright, and a copy already
/// superseded before it queues never runs — a slow older helper
/// cannot land its text over a newer copy's transport delivery; an
/// escape-only copy made after the helper was dropped writes outside
/// the lane and stands on its own. The caller-stamped `attempt` rides
/// onto the verdict unchanged. The transports' verdict lands as a
/// follow-up notice through the render wake this pings; until then
/// the returned escape result is all the caller can honestly claim.
/// Failures stay silent per attempt: a copy that cannot be delivered
/// is a nuisance, not an error worth interrupting a session for.
pub(in crate::app) fn copy_via_transports(
    custom: Option<&str>,
    attempt: u64,
    text: &str,
    attempts: &Arc<std::sync::atomic::AtomicU64>,
    outcomes: &CopyOutcomes,
    wake: &Arc<event_listener::Event>,
) -> CopyAnswer {
    let escape_written = osc52_write(text);
    let command_line = configured_copy_command(custom).map(str::to_string);
    let transports_scheduled = command_line.is_some()
        || cfg!(target_os = "macos")
        || !host_transports(clipboard_env()).is_empty();
    let answer = transport_answer(escape_written, transports_scheduled);
    if !transports_scheduled {
        return answer;
    }
    let outcomes = Arc::clone(outcomes);
    let wake = Arc::clone(wake);
    let attempts = Arc::clone(attempts);
    let text = text.to_string();
    let job = move || {
        let commands = transport_commands(command_line.as_deref());
        let verified = fenced_process_writes(attempt, &attempts, commands, &text);
        let mut slot = outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        slot.push(CopyVerdict {
            attempt,
            escape_written,
            verified,
        });
        wake.notify(1);
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        drop(tokio::task::spawn_blocking(job));
    } else {
        drop(std::thread::spawn(job));
    }
    answer
}

/// Write the OSC 52 clipboard escape for `text`.
///
/// Both terminator spellings are written — BEL and ST — because a few
/// terminals accept only one of them. The write is the only signal
/// available from this side: acceptance by the receiving terminal is
/// unknowable, so success here means the bytes left and nothing more.
fn osc52_write(text: &str) -> bool {
    use std::io::Write as _;

    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = std::io::stdout();
    stdout
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .is_ok()
        && stdout
            .write_all(format!("\x1b]52;c;{encoded}\x1b\\").as_bytes())
            .is_ok()
        && stdout.flush().is_ok()
}
