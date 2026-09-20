//! Session resume and listing — the flow layer over session
//! persistence.
//!
//! [`resolve_resume`] turns the `--resume` flag into one
//! [`ResumeControl`] both run modes consume: a loaded session seeds
//! the display and the agent's conversation history, reuses its id so
//! further auto-saves land in the same file, and keeps its model
//! unless the CLI overrides it; a load that fails degrade-able ways
//! starts a fresh session carrying a warning for the mode to render.
//! `--list-sessions` prints the saved-session table and exits. The
//! data layer lives in [`crate::session`]; this module owns dispatch,
//! presentation, and the display-to-history conversion.

use std::io::IsTerminal as _;
use std::io::Write;
use std::path::Path;

use dch_tui::{ContentBlock, TuiMessage};
use loopctl::message::{Message, MessagePart, Role};
use uuid::Uuid;

use crate::args::Args;
use crate::session::SessionError;
use crate::session::SessionSummary;

/// What the run modes start from, once the resume intent is resolved.
///
/// One resolution serves both modes: the control is built before the
/// mode dispatch and threaded into whichever branch runs, so the
/// load, the degradation policy, and the model precedence are decided
/// exactly once. `Fresh` covers both a plain invocation (no flag, no
/// warning) and a resume that degraded — the warning, when present,
/// is the mode's to render (stderr in headless; in the TUI, a stderr
/// line at startup — which survives a construction failure — plus a
/// system note in the conversation).
#[derive(Debug)]
pub(crate) enum ResumeControl {
    /// `--resume <id>` was given and the session loaded.
    ///
    /// Carries everything the run modes continue from: the restored
    /// transcript, the model the session ran, and the id further
    /// auto-saves must keep writing under.
    Resumed(ResumeOutcome),

    /// No resume happened — nothing was asked, or the ask failed
    /// degrade-able ways.
    Fresh {
        /// The session id further auto-saves write under.
        ///
        /// `None` keeps today's behavior: the runner's own minted id
        /// names the file, and that is what every current resolution
        /// path produces — a degrade never carries an id, because
        /// the fresh session is a new identity and the unloaded file
        /// is left alone. The consumers' fallbacks are what runs.
        session_id: Option<Uuid>,

        /// Why a fresh session is running despite a resume request.
        ///
        /// `None` when no resume was requested at all.
        warn: Option<String>,
    },
}

/// A loaded session: everything both run modes need to continue it.
///
/// Built by [`load_for_resume_in`] from one file read. The display
/// model is verbatim — what was saved is what renders; the agent
/// history is derived from it through [`tui_messages_to_loopctl`],
/// which is lossy by design.
#[derive(Debug)]
pub(crate) struct ResumeOutcome {
    /// The resumed session's UUID, echoed from the id asked for.
    ///
    /// The mode constructs its saver with this id, so auto-saves
    /// overwrite the file that was resumed rather than orphaning it
    /// under a fresh identity.
    pub(crate) session_id: Uuid,

    /// The saved conversation in display form, verbatim.
    ///
    /// Seeds the TUI's conversation or the headless transcript
    /// prefix; no conversion happens at the consumers.
    pub(crate) messages: Vec<TuiMessage>,

    /// The model the saved session ran, from the envelope.
    ///
    /// Applied by the modes only when the CLI does not override the
    /// model, so a resumed session keeps running what it started
    /// with unless the user says otherwise.
    pub(crate) model: String,
}

/// Why a resume could not load a session.
///
/// The flow-layer mirror of [`SessionError`]: the variants are what
/// the degradation policy branches on, not what the file read
/// produced. `Serde` folds into [`ResumeError::Corrupt`] — an
/// unparseable body is corruption however the reader tripped over it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ResumeError {
    /// No session exists under the requested id.
    ///
    /// Benign — a typo or a deleted file. Degrades to fresh.
    #[error("session {0} not found")]
    NotFound(Uuid),

    /// The file exists but is not a readable session transcript.
    ///
    /// The user's history is damaged; the degrade warns loudly and
    /// never touches the file.
    #[error("session file is corrupt: {0}")]
    Corrupt(String),

    /// The session file or its directory cannot be read.
    ///
    /// A failure of the medium — permissions, a vanished parent — on
    /// the one path this resolution reads. The process exits rather
    /// than degrade: the user named this session by id, and silently
    /// swapping to a fresh one would hide a problem worth fixing.
    #[error("session I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<SessionError> for ResumeError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::NotFound(id) => ResumeError::NotFound(id),
            SessionError::Corrupt(detail) => ResumeError::Corrupt(detail),
            SessionError::Io(err) => ResumeError::Io(err),
            SessionError::Serde(err) => ResumeError::Corrupt(err.to_string()),
        }
    }
}

/// Resolve the resume intent against the user's sessions directory.
///
/// The entry `main` calls before dispatching modes: a plain
/// invocation resolves to a quiet fresh control, a loadable id to a
/// resumed one, and a degrade-able failure to fresh with the warning
/// attached. A session file that cannot be read exits the process
/// rather than resolving — see [`resolve_resume_in`].
pub(crate) fn resolve_resume(args: &Args) -> ResumeControl {
    resolve_resume_in(args, &crate::session::sessions_dir())
}

/// Resolve the resume intent against `base` instead of the default
/// sessions directory.
///
/// The injection point the resume tests share, so none of them reads
/// the real `~/.dch/sessions`.
pub(crate) fn resolve_resume_in(args: &Args, base: &Path) -> ResumeControl {
    let Some(id) = args.resume else {
        return ResumeControl::Fresh {
            session_id: None,
            warn: None,
        };
    };
    match load_for_resume_in(id, base) {
        Ok(outcome) => ResumeControl::Resumed(outcome),
        Err(ResumeError::NotFound(missing)) => ResumeControl::Fresh {
            session_id: None,
            warn: Some(format!(
                "session {missing} not found; starting a fresh session"
            )),
        },
        Err(ResumeError::Corrupt(detail)) => ResumeControl::Fresh {
            session_id: None,
            warn: Some(format!(
                "session {id} is corrupt and cannot be resumed ({detail}); \
                 starting a fresh session — the file was not modified"
            )),
        },
        Err(ResumeError::Io(err)) => {
            // The session's own file is unreadable — worth a hard
            // stop, not a silent degrade. A headless caller polling a
            // done-file must hear about this exit, so the bootstrap
            // marker rules apply here exactly as they do for a
            // runtime that fails to construct.
            let (message, marker) = io_exit_report(args, std::io::stdin().is_terminal(), &err);
            if let Some(path) = marker
                && let Err(write_err) = crate::done::write_done_file(
                    &path,
                    &crate::done::DoneStatus::failure(message.clone()),
                    None,
                )
            {
                crate::signals::report(&format!(
                    "dch: cannot write the done-file at {}: {write_err}",
                    path.display()
                ));
            }
            crate::signals::report(&format!("dch: {message}"));
            std::process::exit(1);
        }
    }
}

/// What the unreadable-session-file exit reports, and where its
/// done-file marker lands.
///
/// The decision half of the resume `Io` arm, split from the exit it
/// ends in so the marker rules are pinnable without killing a test
/// process: the message names the failing read, and the marker goes
/// exactly where a headless run's runtime-failure marker would —
/// single-run shapes with `--done-file`, nothing else.
fn io_exit_report(
    args: &Args,
    stdin_is_terminal: bool,
    err: &std::io::Error,
) -> (String, Option<std::path::PathBuf>) {
    let message = format!("cannot read the session file: {err}");
    let marker = if crate::bootstrap_marker_applies(args, stdin_is_terminal) {
        args.done_file.clone()
    } else {
        None
    };
    (message, marker)
}

/// Point `config` at the model the resumed session ran, unless the
/// CLI overrides the model.
///
/// The precedence both run modes share: `--model` (already applied
/// to the config by the CLI overrides before this runs) beats the
/// envelope's model, which beats the config file's. A resumed
/// session keeps running what it started with unless the user says
/// otherwise.
pub(crate) fn apply_resumed_model(
    config: &mut dch_config::DchConfig,
    args: &Args,
    outcome: &ResumeOutcome,
) {
    if args.model.is_none() {
        config.api.model.clone_from(&outcome.model);
    }
}

/// Load a session by id under `base` into everything resume needs.
///
/// One read yields the messages and the envelope's model; the
/// session id is echoed from the request, making the reuse a property
/// of the outcome rather than a second decision at the consumer.
///
/// # Errors
///
/// [`ResumeError::NotFound`] when nothing exists under the id;
/// [`ResumeError::Corrupt`] when the file does not parse or carries
/// an unknown format; [`ResumeError::Io`] when the read itself fails.
pub(crate) fn load_for_resume_in(id: Uuid, base: &Path) -> Result<ResumeOutcome, ResumeError> {
    let (messages, model) = crate::session::load_with_meta_in(id, base)?;
    Ok(ResumeOutcome {
        session_id: id,
        messages,
        model,
    })
}

/// Reconstruct the agent-facing conversation from the display model.
///
/// The mapping walk [`tui_messages_to_loopctl`] performs:
///
/// - `User` text joins the user message in progress — preceded, when
///   the previous reply left tool results outstanding, by those
///   result parts — and coalesces, across a paragraph break, with
///   the previous user message when no assistant reply separates
///   them (the display layer's `System` and `Error` notices sat
///   between two submissions, or the transcript repeats them
///   directly). Coalescing is what keeps user and assistant
///   messages alternating, the shape providers expect of a
///   continued conversation.
/// - `Assistant` content joins the assistant message in progress —
///   consecutive assistant records coalesce into one message whose
///   parts mirror the blocks in order. The display layer graduates
///   each tool call and each reply text as separate one-block
///   records, where the live engine recorded one message per model
///   response; coalescing restores that shape and keeps each tool
///   call adjacent to the user message that carries its result.
///   Each tool call's input is wrapped as `{"preview": …}` — the
///   object shape tool-call inputs carry on the wire.
/// - `System` and `Error` are dropped: the model never saw them the
///   first time (they are display-layer notices), so it must not see
///   them on resume. The system prompt is likewise not part of this
///   reconstruction — it is rebuilt from current config on every
///   request, never stored in the conversation.
///
/// Two shapes can still end the conversation on a user message: a
/// transcript that ends mid-turn leaves its final tool results as
/// the last user message on their own, and one that ends on an
/// unanswered prompt (a failed or cancelled turn) leaves that
/// prompt as the last user message. Both are unavoidable here —
/// the following run's own prompt is appended by the engine, past
/// this walk — so a provider that enforces strict alternation may
/// refuse the very first request after such a resume.
///
/// When `redact_secrets` is set, the same secret patterns live
/// dispatch scrubs with run over every tool preview before it joins
/// the context — a session saved before redaction was enabled must
/// not re-admit what it captured.
///
/// The reconstruction is **lossy by design**: tool blocks carry
/// previews, not the full input and output the original call exchanged,
/// and no original tool-call id. Ids are synthesized positionally
/// (`resume-0`, `resume-1`, … across the whole transcript) and shared
/// between each call and its result so the correlation holds. A
/// resumed session with large tool outputs may drift from what the
/// model originally saw; the display transcript is the faithful
/// record, this is the best-effort context.
pub(crate) fn tui_messages_to_loopctl(
    messages: &[TuiMessage],
    redact_secrets: bool,
) -> Vec<Message> {
    let patterns = redact_secrets.then(loopctl::middleware::SecretPatternSet::default_common);
    let mut converted = Vec::new();
    let mut pending_results: Vec<MessagePart> = Vec::new();
    let mut tool_index = 0usize;
    for message in messages {
        match message {
            TuiMessage::User { text, .. } => {
                let mut parts = std::mem::take(&mut pending_results);
                parts.push(MessagePart::text(text.clone()));
                push_user_message(&mut converted, parts);
            }
            TuiMessage::Assistant { blocks, .. } => {
                let mut parts = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(MessagePart::text(text.clone()));
                        }
                        ContentBlock::Tool {
                            name,
                            input_preview,
                            success,
                            output_preview,
                            ..
                        } => {
                            let id = format!("resume-{tool_index}");
                            tool_index = tool_index.saturating_add(1);
                            let mut input = input_preview.clone();
                            let mut output = output_preview.clone();
                            if let Some(patterns) = &patterns {
                                patterns.scrub(&mut input);
                                patterns.scrub(&mut output);
                            }
                            parts.push(MessagePart::tool_call(
                                id.clone(),
                                name.clone(),
                                serde_json::json!({ "preview": input }),
                            ));
                            pending_results.push(MessagePart::tool_result(
                                id,
                                name.clone(),
                                output,
                                !*success,
                            ));
                        }
                    }
                }
                push_assistant_message(&mut converted, parts);
            }
            TuiMessage::System { .. } | TuiMessage::Error { .. } => {}
        }
    }
    if !pending_results.is_empty() {
        push_user_message(&mut converted, pending_results);
    }
    converted
}

/// Add `parts` to the conversation as user content, coalescing with
/// the previous message when it is also user-role.
///
/// The alternation keeper of the reconstruction: user submissions
/// not separated by an assistant reply — the tool-result rider plus
/// its following prompt, or two prompts around dropped display
/// notices — become parts of one user message, so the walk never
/// emits adjacent user-role messages. A paragraph break is inserted
/// ahead of coalesced content: providers serialize a message's text
/// parts with no separator of their own, so without it two prompts
/// would reach the wire glued into one run of words.
fn push_user_message(converted: &mut Vec<Message>, parts: Vec<MessagePart>) {
    match converted.last_mut() {
        Some(Message {
            role: Role::User,
            parts: existing,
        }) => {
            existing.push(MessagePart::text("\n\n"));
            existing.extend(parts);
        }
        _ => converted.push(Message {
            role: Role::User,
            parts,
        }),
    }
}

/// Add `parts` to the conversation as assistant content, coalescing
/// with the previous message when it is also assistant-role.
///
/// The display layer graduates each tool call and each reply text as
/// its own one-block record, so a tool-using turn saves consecutive
/// assistant records where the live engine recorded one message per
/// model response. Coalescing restores that shape, which is also
/// what keeps a tool call and its deferred result adjacent on the
/// wire — providers require the result message to follow the
/// assistant message carrying its call with nothing in between.
///
/// When both the accumulated content and the incoming parts carry
/// text, a paragraph break is inserted between them: providers
/// serialize a message's text parts with no separator of their own,
/// so a narrate-then-call turn's surrounding texts would otherwise
/// reach the wire glued into one run of words. Tool blocks are
/// structural — a text/tool boundary needs no break, which is why
/// the common call-then-reply merge gains none.
fn push_assistant_message(converted: &mut Vec<Message>, parts: Vec<MessagePart>) {
    let holds_text = |parts: &[MessagePart]| {
        parts
            .iter()
            .any(|part| matches!(part, MessagePart::Text { .. }))
    };
    match converted.last_mut() {
        Some(Message {
            role: Role::Assistant,
            parts: existing,
        }) => {
            if holds_text(existing) && holds_text(&parts) {
                existing.push(MessagePart::text("\n\n"));
            }
            existing.extend(parts);
        }
        _ => converted.push(Message {
            role: Role::Assistant,
            parts,
        }),
    }
}

/// The file paths a transcript shows the prior session reading.
///
/// A `Read` block's input preview carries that call's `file_path`
/// value — the display layer extracts that field — so blocks named
/// `Read` hold addressable paths; other tools' previews are
/// humanized summaries (commands, patterns, edit counts) and yield
/// nothing. Coverage is bounded by what the transcript recorded:
/// the display layer caps previews, so a path longer than the cap
/// survives only truncated (and then resolves to nothing, safely
/// skipping the re-arm), and transcripts saved by headless runs
/// carry text-only turns with no tool blocks at all. A relative
/// preview path re-resolves against the *resume-time* cwd when the
/// baseline is re-armed — resuming from a different directory misses
/// the file (the guard stays disarmed, the safe direction) or arms
/// a different same-named file under the new cwd (a resume-armed
/// baseline holds the first write for a live Read here either way).
/// Order is preserved and duplicates kept: re-recording a baseline
/// is idempotent for the guard's purposes.
pub(crate) fn resumed_read_paths(messages: &[TuiMessage]) -> Vec<String> {
    let mut paths = Vec::new();
    for message in messages {
        if let TuiMessage::Assistant { blocks, .. } = message {
            for block in blocks {
                if let ContentBlock::Tool {
                    name,
                    input_preview,
                    ..
                } = block
                    && name == "Read"
                    && !input_preview.trim().is_empty()
                {
                    paths.push(input_preview.trim().to_string());
                }
            }
        }
    }
    paths
}

/// Print the saved-session table and exit successfully.
///
/// The `--list-sessions` dispatch target; short-circuits every other
/// mode. An empty listing is the answer "nothing", not a failure, so
/// it prints its notice and exits 0. A listing that cannot happen —
/// the sessions root is unreadable — reports to stderr and exits 1.
pub(crate) fn run_list_sessions() -> ! {
    match crate::session::SessionSaver::list_sessions() {
        Ok(sessions) => {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            if let Err(err) = render_sessions(&sessions, &mut out) {
                crate::signals::report(&format!("dch: cannot print the session table: {err}"));
                std::process::exit(1);
            }
        }
        Err(err) => {
            crate::signals::report(&format!("dch: cannot list sessions: {err}"));
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

/// The table's column widths: id, model, last activity, messages.
///
/// One set for the header, the rule, and every row, so the columns
/// line up without measuring content. The id column is two wider
/// than a UUID to give the copy-paste target breathing room; the
/// message count is right-aligned against the rest.
const ID_WIDTH: usize = 38;
const MODEL_WIDTH: usize = 24;
const ACTIVITY_WIDTH: usize = 17;

/// Render the session table, newest-first as given, to `out`.
///
/// One row per session — full UUID (users copy-paste it into
/// `--resume`), model, last activity, message count — preceded by a
/// header and a rule. An empty listing prints only the no-sessions
/// notice. Rows render in the order received; sorting is the data
/// layer's, not the renderer's.
///
/// # Errors
///
/// Fails when writing to `out` fails.
pub(crate) fn render_sessions(
    sessions: &[SessionSummary],
    out: &mut impl Write,
) -> std::io::Result<()> {
    if sessions.is_empty() {
        writeln!(out, "No saved sessions.")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<ID_WIDTH$} {:<MODEL_WIDTH$} {:<ACTIVITY_WIDTH$} {:>4}",
        "SESSION ID", "MODEL", "LAST ACTIVITY", "MSGS",
    )?;
    writeln!(
        out,
        "{:<ID_WIDTH$} {:<MODEL_WIDTH$} {:<ACTIVITY_WIDTH$} {:>4}",
        "─".repeat(ID_WIDTH),
        "─".repeat(MODEL_WIDTH),
        "─".repeat(ACTIVITY_WIDTH),
        "─".repeat(4),
    )?;
    for summary in sessions {
        writeln!(
            out,
            "{:<ID_WIDTH$} {} {:<ACTIVITY_WIDTH$} {:>4}",
            summary.id,
            fit_column(&summary.model, MODEL_WIDTH),
            summary.last_activity.format("%Y-%m-%d %H:%M"),
            summary.message_count,
        )?;
    }
    Ok(())
}

/// Pad `text` to `width`, truncating with an ellipsis when longer.
///
/// Keeps one row's over-long model name from shifting the columns
/// that follow it. Capped by characters — model names are plain
/// identifiers, not prose.
fn fit_column(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return format!("{text:<width$}");
    }
    let kept = text
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    format!("{kept}…")
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
    use chrono::TimeZone as _;
    use clap::Parser as _;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.timestamp_opt(1_800_000_000, 0).unwrap()
    }

    fn user(text: &str) -> TuiMessage {
        TuiMessage::User {
            text: text.to_string(),
            timestamp: now(),
        }
    }

    fn assistant(blocks: Vec<ContentBlock>) -> TuiMessage {
        TuiMessage::Assistant {
            blocks,
            timestamp: now(),
            duration_ms: None,
        }
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_string(),
        }
    }

    fn tool_block(name: &str, success: bool) -> ContentBlock {
        ContentBlock::Tool {
            call_id: String::new(),
            name: name.to_string(),
            input_preview: "a.rs".to_string(),
            success,
            elapsed_secs: 0.25,
            output_preview: "first lines…".to_string(),
        }
    }

    fn roles(converted: &[Message]) -> Vec<Role> {
        converted.iter().map(|message| message.role).collect()
    }

    /// The tool-call and tool-result parts of a converted transcript,
    /// paired positionally.
    fn tool_pairs(converted: &[Message]) -> Vec<(String, String, bool)> {
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for message in converted {
            for part in &message.parts {
                match part {
                    MessagePart::ToolCall { id, name, .. } => {
                        calls.push((id.clone(), name.clone()));
                    }
                    MessagePart::ToolResult {
                        call_id, is_error, ..
                    } => {
                        results.push((call_id.clone(), is_error.unwrap_or(false)));
                    }
                    _ => {}
                }
            }
        }
        calls
            .into_iter()
            .zip(results)
            .map(|((id, name), (result_id, is_error))| {
                (id, format!("{name}|{result_id}"), is_error)
            })
            .collect()
    }

    #[test]
    fn a_user_message_becomes_one_user_message() {
        let converted = tui_messages_to_loopctl(&[user("hi")], false);
        assert_eq!(converted.len(), 1, "one in, one out");
        assert_eq!(roles(&converted), vec![Role::User]);
        match &converted[0].parts[..] {
            [MessagePart::Text { text }] => assert_eq!(text, "hi"),
            parts => panic!("a single text part was expected: {parts:?}"),
        }
    }

    #[test]
    fn an_assistant_text_reply_maps_to_one_assistant_message() {
        let converted = tui_messages_to_loopctl(&[assistant(vec![text_block("hello")])], false);
        assert_eq!(
            converted.len(),
            1,
            "no tool results follow a text-only reply"
        );
        assert_eq!(roles(&converted), vec![Role::Assistant]);
        match &converted[0].parts[..] {
            [MessagePart::Text { text }] => assert_eq!(text, "hello"),
            parts => panic!("a single text part was expected: {parts:?}"),
        }
    }

    #[test]
    fn an_assistant_tool_reply_pairs_a_call_with_its_result() {
        let converted =
            tui_messages_to_loopctl(&[assistant(vec![tool_block("Read", true)])], false);
        assert_eq!(
            roles(&converted),
            vec![Role::Assistant, Role::User],
            "the call is assistant-role, the result user-role"
        );
        match (&converted[0].parts[..], &converted[1].parts[..]) {
            (
                [MessagePart::ToolCall { id, name, .. }],
                [MessagePart::ToolResult { call_id, .. }],
            ) => {
                assert_eq!(id, call_id, "the synthesized id correlates call and result");
                assert_eq!(name, "Read");
                assert_eq!(id, "resume-0", "ids count from zero");
            }
            parts => panic!("a call part then a result part were expected: {parts:?}"),
        }
    }

    #[test]
    fn a_failed_tool_maps_to_an_error_result() {
        let converted =
            tui_messages_to_loopctl(&[assistant(vec![tool_block("Bash", false)])], false);
        assert_eq!(
            tool_pairs(&converted),
            vec![("resume-0".to_string(), "Bash|resume-0".to_string(), true)],
            "success inverts into the result's error flag"
        );
    }

    #[test]
    fn interleaved_blocks_keep_their_order() {
        let converted = tui_messages_to_loopctl(
            &[assistant(vec![
                text_block("reading first"),
                tool_block("Read", true),
                tool_block("Grep", true),
                text_block("done"),
            ])],
            false,
        );
        assert_eq!(roles(&converted), vec![Role::Assistant, Role::User]);
        assert_eq!(
            converted[0].parts.len(),
            4,
            "every block lands in the assistant message, in order"
        );
        match &converted[1].parts[..] {
            [
                MessagePart::ToolResult { call_id: first, .. },
                MessagePart::ToolResult {
                    call_id: second, ..
                },
            ] => {
                assert_eq!(
                    (first.as_str(), second.as_str()),
                    ("resume-0", "resume-1"),
                    "the two calls get distinct, positional ids"
                );
            }
            parts => panic!("two result parts were expected: {parts:?}"),
        }
    }

    #[test]
    fn two_turns_alternate_roles_in_order() {
        let transcript = vec![
            user("first"),
            assistant(vec![tool_block("Read", true)]),
            user("second"),
            assistant(vec![text_block("done")]),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            roles(&converted),
            vec![Role::User, Role::Assistant, Role::User, Role::Assistant],
            "a turn's tool results ride the next user message, so roles alternate"
        );
        match &converted[2].parts[..] {
            [
                MessagePart::ToolResult { call_id, .. },
                MessagePart::Text { text },
            ] => {
                assert_eq!(call_id, "resume-0", "the result keeps its call's id");
                assert_eq!(text, "second", "the prompt text follows its results");
            }
            parts => panic!("the merged user message carries results then text: {parts:?}"),
        }
    }

    #[test]
    fn a_trailing_tool_rider_closes_the_conversation() {
        // A transcript ending mid-turn — its last reply made tool
        // calls and never finished — leaves the results as the final
        // user-role message on their own.
        let converted =
            tui_messages_to_loopctl(&[assistant(vec![tool_block("Read", true)])], false);
        assert_eq!(
            roles(&converted),
            vec![Role::Assistant, Role::User],
            "an unmergeable rider stands as the last message"
        );
    }

    #[test]
    fn prompts_around_a_dropped_notice_coalesce_into_one_user_message() {
        // The shape every failed turn leaves behind: two submissions
        // with only a display notice between them. The notice is
        // dropped, so the walk must coalesce the prompts — emitting
        // adjacent user-role messages here is exactly the provider
        // rejection the merge exists to prevent.
        let transcript = vec![
            user("first"),
            TuiMessage::Error {
                text: "boom".to_string(),
                timestamp: now(),
            },
            user("second"),
            assistant(vec![text_block("done")]),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            roles(&converted),
            vec![Role::User, Role::Assistant],
            "the dropped error leaves no gap the alternation falls into"
        );
        match &converted[0].parts[..] {
            [
                MessagePart::Text { text: first },
                MessagePart::Text { text: separator },
                MessagePart::Text { text: second },
            ] => {
                assert_eq!(
                    (first.as_str(), second.as_str()),
                    ("first", "second"),
                    "both prompts ride one user message, oldest first"
                );
                assert_eq!(
                    separator, "\n\n",
                    "providers join text parts with no separator — a paragraph break must sit between"
                );
            }
            parts => panic!("the coalesced message carries both prompts, separated: {parts:?}"),
        }
    }

    #[test]
    fn the_shape_the_tui_saves_alternates_roles_and_keeps_results_adjacent() {
        // The transcript a tool-using TUI turn actually saves: the
        // display layer graduates the tool call and the reply text
        // as two consecutive assistant records. The reconstruction
        // must coalesce them — anything else puts the reply between
        // a tool call and its result and breaks role alternation.
        let transcript = vec![
            user("fix the bug"),
            assistant(vec![tool_block("Read", true)]),
            assistant(vec![text_block("I fixed it")]),
            user("next prompt"),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            roles(&converted),
            vec![Role::User, Role::Assistant, Role::User],
            "one assistant message per model response, as the live engine records"
        );
        match (&converted[1].parts[..], &converted[2].parts[..]) {
            (
                [
                    MessagePart::ToolCall { id, .. },
                    MessagePart::Text { text: reply },
                ],
                [
                    MessagePart::ToolResult { call_id, .. },
                    MessagePart::Text { text: prompt },
                ],
            ) => {
                assert_eq!(
                    id, call_id,
                    "the result message immediately follows the assistant message carrying its call"
                );
                assert_eq!(reply, "I fixed it");
                assert_eq!(prompt, "next prompt");
            }
            parts => panic!(
                "call and text in one assistant message, result then prompt in one user message: {parts:?}"
            ),
        }
    }

    #[test]
    fn a_narrate_then_call_turn_keeps_its_text_boundary() {
        // A turn whose responses narrate around the call — text,
        // then the tool, then the closing text — saves as three
        // assistant records. Coalescing them without a break would
        // glue the two texts into one run of words on every
        // provider (text parts serialize with no separator), while
        // the call-then-reply merge must not gain a stray leading
        // break — the shape pin above matches its parts exactly.
        let transcript = vec![
            user("fix it"),
            assistant(vec![text_block("Let me check the file first")]),
            assistant(vec![tool_block("Read", true)]),
            assistant(vec![text_block("Done")]),
            user("next"),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            roles(&converted),
            vec![Role::User, Role::Assistant, Role::User]
        );
        match &converted[1].parts[..] {
            [
                MessagePart::Text { text: before },
                MessagePart::ToolCall { .. },
                MessagePart::Text { text: separator },
                MessagePart::Text { text: after },
            ] => {
                assert_eq!(
                    (before.as_str(), after.as_str()),
                    ("Let me check the file first", "Done")
                );
                assert_eq!(
                    separator, "\n\n",
                    "the two narrations serialize joined with no separator — a break must sit between"
                );
            }
            parts => {
                panic!("narration, call, break, closing text in one assistant message: {parts:?}")
            }
        }
    }

    #[test]
    fn redaction_scrubs_secret_shaped_previews() {
        let token = format!("ghp_{}", "x".repeat(36));
        let leaked = assistant(vec![ContentBlock::Tool {
            call_id: String::new(),
            name: "Bash".to_string(),
            input_preview: format!("echo {token}"),
            success: true,
            elapsed_secs: 0.1,
            output_preview: format!("leaked {token} again"),
        }]);
        let debug = format!(
            "{:?}",
            tui_messages_to_loopctl(std::slice::from_ref(&leaked), true)
        );
        assert!(
            !debug.contains(&token),
            "redaction must scrub the token from both previews: {debug}"
        );
        let verbatim = format!(
            "{:?}",
            tui_messages_to_loopctl(std::slice::from_ref(&leaked), false)
        );
        assert!(
            verbatim.contains(&token),
            "without the flag the previews pass through verbatim: {verbatim}"
        );
    }

    #[test]
    fn read_previews_are_extracted_as_resumable_paths() {
        let transcript = vec![
            user("look"),
            assistant(vec![
                tool_block("Read", true),
                tool_block("Bash", true),
                text_block("done"),
            ]),
            assistant(vec![text_block("and again")]),
        ];
        assert_eq!(
            resumed_read_paths(&transcript),
            vec!["a.rs".to_string()],
            "only Read previews are paths — other tools' previews are summaries"
        );
    }

    #[test]
    fn system_and_error_messages_are_dropped() {
        let transcript = vec![
            TuiMessage::System {
                text: "welcome".to_string(),
                timestamp: now(),
            },
            user("hi"),
            TuiMessage::Error {
                text: "boom".to_string(),
                timestamp: now(),
            },
            assistant(vec![text_block("hello")]),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            converted.len(),
            2,
            "display-only notices never reach the model's history"
        );
    }

    #[test]
    fn the_input_preview_rides_as_a_json_object() {
        let converted =
            tui_messages_to_loopctl(&[assistant(vec![tool_block("Read", true)])], false);
        match &converted[0].parts[..] {
            [MessagePart::ToolCall { input, .. }] => assert_eq!(
                input,
                &serde_json::json!({ "preview": "a.rs" }),
                "the preview rides wrapped in an object, the shape tool-call inputs carry on the wire"
            ),
            parts => panic!("a tool-call part was expected: {parts:?}"),
        }
    }

    #[test]
    fn repeated_conversions_are_identical() {
        let transcript = vec![
            user("hi"),
            assistant(vec![tool_block("Read", true)]),
            assistant(vec![text_block("done")]),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            roles(&converted),
            vec![Role::User, Role::Assistant, Role::User],
            "adjacent assistant records coalesce into one message and the unmergeable \
             result rider closes the conversation — determinism must not paper over the shape"
        );
        let first = format!("{:?}", tui_messages_to_loopctl(&transcript, false));
        let second = format!("{:?}", tui_messages_to_loopctl(&transcript, false));
        assert_eq!(first, second, "ids are positional, never random");
    }

    #[test]
    fn an_empty_transcript_converts_to_an_empty_history() {
        assert!(
            tui_messages_to_loopctl(&[], false).is_empty(),
            "nothing in, nothing out"
        );
    }

    fn save_session(dir: &std::path::Path, id: Uuid, model: &str, messages: &[TuiMessage]) {
        crate::session::SessionSaver::with_base_dir(id, model.to_string(), dir.to_path_buf())
            .save(messages)
            .expect("save the fixture session");
    }

    #[test]
    fn a_saved_session_loads_for_resume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        save_session(
            dir.path(),
            id,
            "resume-model",
            &[
                user("one"),
                assistant(vec![text_block("two")]),
                user("three"),
            ],
        );
        let outcome = load_for_resume_in(id, dir.path()).expect("the saved session loads");
        assert_eq!(
            outcome.messages.len(),
            3,
            "the transcript round-trips into the outcome"
        );
        assert!(!tui_messages_to_loopctl(&outcome.messages, false).is_empty());
    }

    #[test]
    fn a_missing_session_reports_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load_for_resume_in(Uuid::new_v4(), dir.path()).expect_err("nothing was saved");
        assert!(
            matches!(err, ResumeError::NotFound(_)),
            "a missing id is typed NotFound, got {err:?}"
        );
    }

    #[test]
    fn a_corrupt_session_reports_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let path = dir.path().join(id.to_string()).join("session.json");
        std::fs::create_dir_all(path.parent().expect("the session dir")).expect("mkdir");
        std::fs::write(&path, "{broken").expect("write garbage");
        let err = load_for_resume_in(id, dir.path()).expect_err("garbage must fail");
        assert!(
            matches!(err, ResumeError::Corrupt(_)),
            "garbage is typed Corrupt, got {err:?}"
        );
    }

    #[test]
    fn the_model_comes_from_the_envelope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        save_session(dir.path(), id, "glm-4.7", &[user("hi")]);
        let outcome = load_for_resume_in(id, dir.path()).expect("load");
        assert_eq!(
            outcome.model, "glm-4.7",
            "the envelope's model travels with the outcome"
        );
    }

    #[test]
    fn the_session_id_echoes_the_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        save_session(dir.path(), id, "m", &[user("hi")]);
        let outcome = load_for_resume_in(id, dir.path()).expect("load");
        assert_eq!(outcome.session_id, id, "the id is reused, not reminted");
    }

    fn rendered(sessions: &[SessionSummary]) -> String {
        let mut out = Vec::new();
        render_sessions(sessions, &mut out).expect("render");
        String::from_utf8(out).expect("utf-8 output")
    }

    fn summary(id: Uuid, model: &str, minutes: u32, count: usize) -> SessionSummary {
        SessionSummary {
            id,
            model: model.to_string(),
            last_activity: chrono::Utc
                .timestamp_opt(
                    1_800_000_000i64.saturating_add(i64::from(minutes).saturating_mul(60)),
                    0,
                )
                .unwrap(),
            message_count: count,
        }
    }

    #[test]
    fn an_empty_listing_prints_the_notice() {
        assert_eq!(rendered(&[]), "No saved sessions.\n");
    }

    #[test]
    fn a_listing_prints_the_header_and_one_row_per_session() {
        let sessions = [
            summary(Uuid::new_v4(), "m-a", 0, 12),
            summary(Uuid::new_v4(), "m-b", 5, 4),
        ];
        let table = rendered(&sessions);
        let mut lines = table.lines();
        let header = lines.next().expect("the header line");
        let rule = lines.next().expect("the separator rule");
        assert!(
            ["SESSION ID", "MODEL", "LAST ACTIVITY", "MSGS"]
                .iter()
                .all(|column| header.contains(column)),
            "the header names every column: {header}"
        );
        assert!(
            rule.chars().all(|c| c == '─' || c == ' '),
            "the rule is dashes under the columns: {rule}"
        );
        let rows: Vec<&str> = lines.collect();
        assert_eq!(rows.len(), 2, "one row per session: {table}");
    }

    #[test]
    fn rows_preserve_the_given_order() {
        let oldest = Uuid::new_v4();
        let newest = Uuid::new_v4();
        let table = rendered(&[summary(oldest, "m", 0, 1), summary(newest, "m", 9, 2)]);
        let older_at = table.find(&oldest.to_string()).expect("the older row");
        let newer_at = table.find(&newest.to_string()).expect("the newer row");
        assert!(
            older_at < newer_at,
            "the renderer keeps the order it was given, newest-first from the data layer"
        );
    }

    #[test]
    fn rows_carry_the_full_uuid() {
        let id = Uuid::new_v4();
        let table = rendered(&[summary(id, "m", 0, 1)]);
        assert!(
            table.contains(&id.to_string()),
            "the row carries the copy-pasteable full UUID: {table}"
        );
    }

    #[test]
    fn an_over_long_model_is_truncated_to_its_column() {
        let long = "x".repeat(40);
        let table = rendered(&[summary(Uuid::new_v4(), &long, 0, 1)]);
        let row = table.lines().last().expect("the data row");
        assert!(
            row.chars().count() <= ID_WIDTH + MODEL_WIDTH + ACTIVITY_WIDTH + 4 + 6,
            "a long model must not shift the columns that follow: {row}"
        );
        assert!(row.contains('…'), "the cap is visible: {row}");
    }

    fn parse(args: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("dch").chain(args.iter().copied()))
            .expect("the fixture args parse")
    }

    #[test]
    fn without_the_flag_the_control_is_fresh_and_quiet() {
        let control = resolve_resume_in(&parse(&[]), Path::new("/nowhere"));
        match control {
            ResumeControl::Fresh {
                session_id: None,
                warn: None,
            } => {}
            control => panic!("a plain invocation must reduce to today's flow: {control:?}"),
        }
    }

    #[test]
    fn a_missing_session_degrades_to_fresh_with_a_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let control = resolve_resume_in(&parse(&["--resume", &id.to_string()]), dir.path());
        match control {
            ResumeControl::Fresh {
                session_id: None,
                warn: Some(warn),
            } => assert!(
                warn.contains("not found"),
                "the warning names the failure: {warn}"
            ),
            control => panic!("a missing session degrades to Fresh: {control:?}"),
        }
    }

    #[test]
    fn a_stray_file_instead_of_a_session_directory_degrades_to_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        // A regular file where the session directory would be: the
        // load must read it as "nothing under this id", not as an
        // unreadable root worth exiting over. Misclassification to
        // the Io arm kills this test process at its exit(1).
        std::fs::write(dir.path().join(id.to_string()), "not a directory")
            .expect("write the stray file");
        let control = resolve_resume_in(&parse(&["--resume", &id.to_string()]), dir.path());
        match control {
            ResumeControl::Fresh {
                session_id: None,
                warn: Some(warn),
            } => assert!(
                warn.contains("not found"),
                "the stray file degrades like a typo'd id: {warn}"
            ),
            control => panic!("an unreachable id degrades to Fresh: {control:?}"),
        }
    }

    #[test]
    fn the_resumed_model_applies_only_without_a_cli_override() {
        let outcome = ResumeOutcome {
            session_id: Uuid::new_v4(),
            messages: Vec::new(),
            model: "resume-model".to_string(),
        };
        let mut config = dch_config::DchConfig::default();
        config.api.model = "config-model".to_string();
        crate::headless::apply_cli_overrides(&mut config, &parse(&[]));
        apply_resumed_model(&mut config, &parse(&[]), &outcome);
        assert_eq!(
            config.api.model, "resume-model",
            "with no --model, the envelope's model beats the config file's"
        );

        let mut config = dch_config::DchConfig::default();
        config.api.model = "config-model".to_string();
        let cli = parse(&["--model", "cli-model"]);
        crate::headless::apply_cli_overrides(&mut config, &cli);
        apply_resumed_model(&mut config, &cli, &outcome);
        assert_eq!(
            config.api.model, "cli-model",
            "a --model flag beats both the envelope and the config file"
        );
    }

    #[test]
    fn an_unreadable_root_exit_reports_the_marker_only_for_headless_shapes() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let task = parse(&["probe", "--done-file", "done.json"]);
        let (message, marker) = io_exit_report(&task, true, &err);
        assert!(
            message.contains("session file") && message.contains("denied"),
            "the exit message names the failing read: {message}"
        );
        assert_eq!(
            marker,
            Some(std::path::PathBuf::from("done.json")),
            "a headless shape with --done-file takes the runtime-failure marker"
        );

        let tui = parse(&["--done-file", "done.json"]);
        let (_message, marker) = io_exit_report(&tui, true, &err);
        assert!(
            marker.is_none(),
            "an interactive shape never takes the bootstrap marker"
        );

        let headless_no_marker = parse(&["probe"]);
        let (_message, marker) = io_exit_report(&headless_no_marker, false, &err);
        assert!(
            marker.is_none(),
            "without --done-file there is no marker to write"
        );
    }

    #[test]
    fn a_corrupt_session_degrades_and_leaves_the_file_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let path = dir.path().join(id.to_string()).join("session.json");
        std::fs::create_dir_all(path.parent().expect("the session dir")).expect("mkdir");
        std::fs::write(&path, "{broken").expect("write garbage");
        let control = resolve_resume_in(&parse(&["--resume", &id.to_string()]), dir.path());
        match control {
            ResumeControl::Fresh {
                session_id: None,
                warn: Some(warn),
            } => assert!(
                warn.contains("corrupt") && warn.contains("not modified"),
                "the degrade warns loudly and promises the file untouched: {warn}"
            ),
            control => panic!("a corrupt session degrades to Fresh: {control:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("the corrupt file survives"),
            "{broken",
            "the degrade never touches the damaged file"
        );
    }

    #[test]
    fn a_loaded_session_passes_through_as_resumed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        save_session(dir.path(), id, "m", &[user("hi")]);
        let control = resolve_resume_in(&parse(&["--resume", &id.to_string()]), dir.path());
        match control {
            ResumeControl::Resumed(outcome) => {
                assert_eq!(outcome.session_id, id);
                assert_eq!(outcome.messages.len(), 1);
            }
            control @ ResumeControl::Fresh { .. } => {
                panic!("the happy path passes through unchanged: {control:?}")
            }
        }
    }

    #[test]
    fn a_resumed_control_carries_the_id_the_modes_reuse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        save_session(dir.path(), id, "m", &[user("hi")]);
        match resolve_resume_in(&parse(&["--resume", &id.to_string()]), dir.path()) {
            ResumeControl::Resumed(outcome) => assert_eq!(
                outcome.session_id, id,
                "the saver reuses the loaded identity, so saves overwrite the resumed file"
            ),
            control @ ResumeControl::Fresh { .. } => panic!("expected Resumed: {control:?}"),
        }
    }
}
