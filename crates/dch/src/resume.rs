//! Session resume and listing — the flow layer over session
//! persistence.
//!
//! [`resolve_resume`] turns the `--resume` flag into one
//! [`ResumeControl`] both run modes consume: a loaded session seeds
//! the display and the agent's conversation history and reuses its id
//! so further auto-saves land in the same file, while the engine runs
//! on the current configuration's model and provider — the saved
//! model is provenance for the listing, never an override; a load
//! that fails degrade-able ways starts a fresh session carrying a
//! warning for the mode to render.
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
    /// What was saved is what renders; the agent history is seeded from
    /// the machine snapshot when one is fresh, and only derived from
    /// this transcript otherwise.
    pub(crate) messages: Vec<TuiMessage>,

    /// The engine's committed history, when the sibling snapshot was
    /// fresh.
    ///
    /// Seeding a runner from this continues the session exactly where
    /// the process left off; `None` — an old session without a
    /// snapshot, or a stale or unreadable pair — falls back to the
    /// lossy derive walk over the display transcript.
    pub(crate) machine: Option<Vec<Message>>,

    /// The session's cumulative token totals, from the envelope.
    ///
    /// Seeds the resumed display's running count so it continues
    /// the session's accounting instead of restarting at zero.
    pub(crate) tokens: crate::session::SessionTokens,

    /// How many compaction passes the adopted machine snapshot
    /// records.
    ///
    /// Zero whenever no snapshot was adopted; the staleness guard
    /// reads it beside the transcript's Compacted rows, because a
    /// headless session's transcript carries none and the snapshot's
    /// count is its only witness of a legitimate shrink.
    pub(crate) machine_compactions: u32,
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
    let id = match (args.resume, args.continue_session.unwrap_or(false)) {
        (Some(id), _) => id,
        (None, true) => match latest_session_in(base) {
            Some(id) => id,
            None => {
                return ResumeControl::Fresh {
                    session_id: None,
                    warn: Some(
                        "no saved session to continue; starting a fresh session".to_string(),
                    ),
                };
            }
        },
        (None, false) => {
            return ResumeControl::Fresh {
                session_id: None,
                warn: None,
            };
        }
    };
    match load_for_resume_in(id, base) {
        Ok(mut outcome) => {
            // A snapshot can be stamped fresh yet hold far less than
            // the transcript beside it — a mid-run publish freeze
            // stamps the stale half with every save. When the engine
            // half is implausibly behind a long transcript that shows
            // no compaction (the one legitimate way a snapshot
            // shrinks), adopting it would silently amneses everything
            // after the freeze; deriving from the transcript instead
            // loses formatting, never content.
            if let Some(machine) = &outcome.machine {
                let exchanges = outcome
                    .messages
                    .iter()
                    .filter(|message| {
                        matches!(
                            message,
                            TuiMessage::User { .. } | TuiMessage::Assistant { .. }
                        )
                    })
                    .count();
                let compacted = outcome.machine_compactions > 0
                    || outcome
                        .messages
                        .iter()
                        .any(|message| matches!(message, TuiMessage::Compacted { .. }));
                let frozen =
                    machine.len().saturating_mul(3) < exchanges && exchanges >= 12 && !compacted;
                if frozen {
                    tracing::warn!(
                        session = %id,
                        machine_messages = machine.len(),
                        transcript_exchanges = exchanges,
                        "machine history implausibly behind the transcript; deriving \
                         the resume from the display transcript"
                    );
                    outcome.machine = None;
                    outcome.messages.push(TuiMessage::System {
                        text: "the session's engine snapshot was stale; the resume \
                               derived its context from the transcript — formatting \
                               was lost, content was not"
                            .to_string(),
                        timestamp: chrono::Utc::now(),
                    });
                }
            }
            ResumeControl::Resumed(outcome)
        }
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

/// The most recently saved session under `base`, if any.
///
/// What `--continue` resolves to: the session whose last completed
/// turn is newest — the one the user most likely just closed.
fn latest_session_in(base: &Path) -> Option<uuid::Uuid> {
    crate::session::list_sessions_in(base)
        .ok()?
        .into_iter()
        .max_by_key(|summary| summary.last_activity)
        .map(|summary| summary.id)
}

/// The history a resumed runner seeds from.
///
/// Prefers the machine snapshot — process-identical continuation, the
/// real tool-call ids and full tool results included — and falls back
/// to the lossy derive walk over the display transcript when no fresh
/// snapshot exists. One selection shared by both run modes, so the
/// TUI and headless resumes can never diverge in fidelity. Under the
/// current redaction policy both halves of the snapshot's tool
/// exchanges are scrubbed with the same curated patterns the
/// dispatch-time pair uses: a session saved before redaction was
/// enabled must not re-admit the raw secret bytes its transcript
/// captured, whatever route the resume takes.
pub(crate) fn history_for_resume(outcome: &ResumeOutcome, redact: bool) -> Vec<Message> {
    match outcome.machine.clone() {
        Some(mut machine) => {
            if redact {
                scrub_machine_history(&mut machine);
            }
            machine
        }
        None => tui_messages_to_loopctl(&outcome.messages, redact),
    }
}

/// Scrub secret-shaped bytes out of a snapshot history's tool exchanges.
///
/// The one place a machine snapshot's fidelity is deliberately
/// narrowed: when the current policy hides these shapes from the
/// model, replaying the saved bytes verbatim would hand the provider
/// what the pipeline would have redacted. Both halves of an exchange
/// go — the result's output (the plain variant's text and every text
/// part of a multipart result) and every string leaf of the call's
/// arguments — because a session resumed onto a different provider
/// otherwise replays argument bytes that provider was never shown. The
/// derive path applies the same policy to the same two halves, so the
/// routes cannot disagree about what a resumed secret is.
///
/// The snapshot this narrows is a copy: the read-before-write re-arm
/// in [`resumed_read_paths_with_machine`] reads the caller's own
/// history, so a scrubbed path argument can never reach a baseline.
fn scrub_machine_history(messages: &mut [Message]) {
    let patterns = loopctl::middleware::SecretPatternSet::default_common();
    for message in messages {
        for part in &mut message.parts {
            match part {
                MessagePart::ToolResult { output, .. } => match output {
                    loopctl::message::ToolContent::Text(text) => {
                        patterns.scrub(text);
                    }
                    loopctl::message::ToolContent::Multipart(parts) => {
                        for part in parts {
                            if let loopctl::message::ToolContentPart::Text { text } = part {
                                patterns.scrub(text);
                            }
                        }
                    }
                },
                MessagePart::ToolCall { input, .. } => scrub_json_strings(&patterns, input),
                _ => {}
            }
        }
    }
}

/// Scrub every string leaf of a tool call's argument tree.
///
/// Arguments are arbitrary JSON, so a secret hides as readily in a
/// nested `edits` entry as in a top-level string; the walk descends
/// arrays and objects to each leaf rather than stopping at the
/// surface. Leaves that are not strings carry no text to hide.
fn scrub_json_strings(
    patterns: &loopctl::middleware::SecretPatternSet,
    value: &mut serde_json::Value,
) {
    match value {
        serde_json::Value::String(text) => {
            patterns.scrub(text);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scrub_json_strings(patterns, item);
            }
        }
        serde_json::Value::Object(entries) => {
            for item in entries.values_mut() {
                scrub_json_strings(patterns, item);
            }
        }
        _ => {}
    }
}

/// Load a session for resume, preferring its machine snapshot when
/// the pair is fresh.
///
/// One resolution over both files: the display transcript always
/// loads, and the machine-history sibling seeds the outcome when it
/// is at least as new as the display file — otherwise the outcome
/// carries `None` and the caller derives.
///
/// # Errors
///
/// Propagates [`ResumeError`] when neither file is readable — the
/// same conditions the session loader reports, mapped once for the
/// resume flow.
pub(crate) fn load_for_resume_in(id: Uuid, base: &Path) -> Result<ResumeOutcome, ResumeError> {
    let (messages, _model, tokens, saved_at) = crate::session::load_with_meta_in(id, base)?;
    let messages: Vec<TuiMessage> = messages
        .into_iter()
        .filter(|message| match message {
            TuiMessage::User { text, steered, .. } => {
                !(*steered && dch_loop::is_nudge_wording(text))
            }
            _ => true,
        })
        .collect();
    let (machine, machine_compactions) = match crate::session::load_machine_in(id, base, saved_at) {
        Some((messages, compactions)) => (Some(messages), compactions),
        None => (None, 0),
    };
    Ok(ResumeOutcome {
        session_id: id,
        messages,
        tokens,
        machine,
        machine_compactions,
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
///   call adjacent to the user message that carries its result. An
///   assistant record that arrives with tool results outstanding
///   first closes the previous reply's exchange — the pending results
///   flush as their own user message — so a transcript with no user
///   texts between its replies (a tool marathon) still reconstructs
///   the alternating shape providers and compaction work on. Each
///   tool call's arguments are its retained input parsed back
///   to the object the model sent — or, for blocks saved before the
///   capture, a scrubbed `{"preview": …}` wrapper carrying the
///   one-line summary.
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
/// The reconstruction replays what the call actually exchanged:
/// tool blocks carry their retained input and output — the capped
/// capture of the real arguments and result — under a synthesized
/// positional id (`resume-0`, `resume-1`, … across the whole
/// transcript) shared between each call and its result so the
/// correlation holds. Blocks saved before the capture keep a
/// scrubbed one-line preview stand-in in place of the arguments,
/// and every retained byte sits under the capture's cap, so the
/// replay can still be a bounded approximation of a very large
/// original — the display transcript remains the faithful record.
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
                if !pending_results.is_empty() {
                    // An assistant record arriving with results outstanding
                    // is a response boundary — the results returned and the
                    // model spoke again. Flushing here keeps a transcript
                    // with no user texts between its replies (a tool
                    // marathon) alternating at message granularity instead
                    // of piling every exchange into one uncompactable
                    // assistant run.
                    push_user_message(&mut converted, std::mem::take(&mut pending_results));
                }
                let mut parts = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Thinking { .. } => {
                            // Display-only: reasoning never re-enters the
                            // API-shape history — an unsigned thinking part
                            // is a wire error on Anthropic and forbidden
                            // echo elsewhere.
                        }
                        ContentBlock::Text { text } => {
                            // An empty text part is a wire error waiting
                            // to happen (Anthropic rejects it); the drain
                            // no longer records one, and an older session
                            // that did must not re-send it.
                            if !text.is_empty() {
                                parts.push(MessagePart::text(text.clone()));
                            }
                        }
                        ContentBlock::Tool {
                            name,
                            input_preview,
                            success,
                            output_preview,
                            retained_input,
                            ..
                        } => {
                            let id = format!("resume-{tool_index}");
                            tool_index = tool_index.saturating_add(1);
                            let mut retained = retained_input.clone();
                            if let Some(patterns) = &patterns {
                                patterns.scrub(&mut retained);
                            }
                            let args = serde_json::from_str::<serde_json::Value>(&retained)
                                .unwrap_or_else(|_| {
                                    let mut preview = input_preview.clone();
                                    if let Some(patterns) = &patterns {
                                        patterns.scrub(&mut preview);
                                    }
                                    serde_json::json!({ "preview": preview })
                                });
                            let mut output = output_preview.clone();
                            if let Some(patterns) = &patterns {
                                patterns.scrub(&mut output);
                            }
                            parts.push(MessagePart::tool_call(id.clone(), name.clone(), args));
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
            TuiMessage::System { .. } | TuiMessage::Error { .. } | TuiMessage::Compacted { .. } => {
            }
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
            ..
        }) => {
            existing.push(MessagePart::text("\n\n"));
            existing.extend(parts);
        }
        _ => converted.push(Message::new(Role::User, parts)),
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
    if parts.is_empty() {
        return;
    }
    let holds_text = |parts: &[MessagePart]| {
        parts
            .iter()
            .any(|part| matches!(part, MessagePart::Text { .. }))
    };
    match converted.last_mut() {
        Some(Message {
            role: Role::Assistant,
            parts: existing,
            ..
        }) => {
            if holds_text(existing) && holds_text(&parts) {
                existing.push(MessagePart::text("\n\n"));
            }
            existing.extend(parts);
        }
        _ => converted.push(Message::new(Role::Assistant, parts)),
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
                    && (name == "read" || name == "Read")
                    && !input_preview.trim().is_empty()
                {
                    paths.push(input_preview.trim().to_string());
                }
            }
        }
    }
    paths
}

/// The paths a resume re-arms as read, from the freshest record.
///
/// Single reads come from the transcript's previews — a `Read` call's
/// preview is its path. Batch reads come from the machine snapshot
/// when one exists, because the transcript's `ReadFiles` preview is a
/// truncated one-line summary that cannot carry the whole files array;
/// the snapshot carries the full call input verbatim. A resume with
/// no snapshot arms only what the previews can name.
pub(crate) fn resumed_read_paths_with_machine(
    messages: &[TuiMessage],
    machine: Option<&[Message]>,
) -> Vec<String> {
    let mut paths = resumed_read_paths(messages);
    if let Some(history) = machine {
        for message in history {
            for part in &message.parts {
                let MessagePart::ToolCall { name, input, .. } = part else {
                    continue;
                };
                if name != "ReadFiles" {
                    continue;
                }
                if let Some(files) = input.get("files").and_then(|value| value.as_array()) {
                    for file in files {
                        if let Some(path) = file.as_str() {
                            paths.push(path.to_string());
                        }
                    }
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
pub(crate) fn run_list_sessions(limit: crate::args::ListCount) -> ! {
    match crate::session::SessionSaver::list_sessions() {
        Ok(sessions) => {
            let (shown, hidden) = cap_for_listing(sessions, limit);
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            if let Err(err) = render_sessions(&shown, &mut out) {
                crate::signals::report(&format!("dch: cannot print the session table: {err}"));
                std::process::exit(1);
            }
            if hidden > 0
                && let Err(err) = writeln!(
                    out,
                    "… and {hidden} more — `dch --list-sessions all` shows them"
                )
            {
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
const CONTEXT_WIDTH: usize = 10;

/// Narrow a listing to what its limit shows.
///
/// The listing's newest-first order is the data layer's; the cap
/// keeps the head of it — the sessions a user is most likely
/// reaching for — and reports how many older ones stay hidden so
/// the footer can say how to see them.
fn cap_for_listing(
    sessions: Vec<crate::session::SessionSummary>,
    limit: crate::args::ListCount,
) -> (Vec<crate::session::SessionSummary>, usize) {
    match limit {
        crate::args::ListCount::All => (sessions, 0),
        crate::args::ListCount::Count(count) => {
            let hidden = sessions.len().saturating_sub(count);
            let shown = sessions.into_iter().take(count).collect();
            (shown, hidden)
        }
    }
}

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
        "{:<ID_WIDTH$} {:<MODEL_WIDTH$} {:<ACTIVITY_WIDTH$} {:>4} {:>CONTEXT_WIDTH$}",
        "SESSION ID", "MODEL", "LAST ACTIVITY", "MSGS", "CONTEXT",
    )?;
    writeln!(
        out,
        "{:<ID_WIDTH$} {:<MODEL_WIDTH$} {:<ACTIVITY_WIDTH$} {:>4} {:>CONTEXT_WIDTH$}",
        "─".repeat(ID_WIDTH),
        "─".repeat(MODEL_WIDTH),
        "─".repeat(ACTIVITY_WIDTH),
        "─".repeat(4),
        "─".repeat(CONTEXT_WIDTH),
    )?;
    for summary in sessions {
        writeln!(
            out,
            "{:<ID_WIDTH$} {} {:<ACTIVITY_WIDTH$} {:>4} {:>CONTEXT_WIDTH$}",
            summary.id.to_string(),
            fit_column(&summary.model, MODEL_WIDTH),
            summary.last_activity.format("%Y-%m-%d %H:%M"),
            summary.user_message_count,
            summary.context_tokens,
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
            steered: false,
        }
    }

    fn user_steered(text: &str) -> TuiMessage {
        TuiMessage::User {
            text: text.to_string(),
            timestamp: now(),
            steered: true,
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
            retained_input: String::new(),
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
    fn a_secret_inside_parseable_retained_input_scrubs_before_replay() {
        // The primary path replays the call's real arguments; a
        // session saved without redaction (or a model that embedded
        // a token in its command) must not re-admit it through
        // those arguments on resume.
        use dch_tui::{ContentBlock, TuiMessage};
        let now = chrono::Utc::now();
        let messages = vec![TuiMessage::Assistant {
            blocks: vec![ContentBlock::Tool {
                name: "Bash".to_string(),
                call_id: "call-s".to_string(),
                input_preview: "echo token".to_string(),
                success: true,
                elapsed_secs: 1.0,
                output_preview: "ok".to_string(),
                retained_input: r#"{"command":"echo ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#
                    .to_string(),
            }],
            timestamp: now,
            duration_ms: None,
        }];
        let converted = super::tui_messages_to_loopctl(&messages, true);
        let text = serde_json::to_string(&converted).expect("serializes");
        assert!(
            !text.contains("ghp_"),
            "the secret never re-enters the replayed arguments: {text}"
        );
        assert!(
            text.contains("[REDACTED:"),
            "the scrub leaves its placeholder inside the arguments: {text}"
        );
    }

    #[test]
    fn a_resumed_tool_call_carries_its_real_arguments_and_result() {
        // Retained input and output ride the transcript now: the
        // resumed history rebuilds the call's actual arguments and the
        // result the tool returned, not a preview stand-in.
        use dch_tui::{ContentBlock, TuiMessage};
        let now = chrono::Utc::now();
        let messages = vec![TuiMessage::Assistant {
            blocks: vec![ContentBlock::Tool {
                name: "Bash".to_string(),
                call_id: "call-9".to_string(),
                input_preview: "make test".to_string(),
                success: true,
                elapsed_secs: 2.0,
                output_preview: "74 passed".to_string(),
                retained_input: r#"{"command":"make test"}"#.to_string(),
            }],
            timestamp: now,
            duration_ms: None,
        }];
        let converted = super::tui_messages_to_loopctl(&messages, false);
        let text = serde_json::to_string(&converted).expect("serializes");
        assert!(
            text.contains(r#""command":"make test""#),
            "the call's real arguments ride the history: {text}"
        );
        assert!(
            text.contains("74 passed"),
            "the tool's returned output rides the history: {text}"
        );
        assert!(
            !text.contains("preview"),
            "no stand-in wrapper remains: {text}"
        );
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
        // as two consecutive assistant records — two model responses,
        // the reply arriving after the result returned. The
        // reconstruction closes the exchange at that boundary: the
        // result rides its own user message, the reply follows as the
        // next assistant message, and role alternation plus
        // call-to-result adjacency both hold.
        let transcript = vec![
            user("fix the bug"),
            assistant(vec![tool_block("Read", true)]),
            assistant(vec![text_block("I fixed it")]),
            user("next prompt"),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            roles(&converted),
            vec![
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant,
                Role::User
            ],
            "the reply after the result is its own response, in alternation"
        );
        match (&converted[1].parts[..], &converted[2].parts[..]) {
            ([MessagePart::ToolCall { id, .. }], [MessagePart::ToolResult { call_id, .. }]) => {
                assert_eq!(
                    id, call_id,
                    "the result message immediately follows the assistant message carrying its call"
                );
            }
            parts => panic!(
                "the call's exchange closes with its result as its own user message: {parts:?}"
            ),
        }
        assert!(
            matches!(&converted[3].parts[..], [MessagePart::Text { text }, ..] if text == "I fixed it"),
            "the reply follows the result as the next assistant message"
        );
    }

    #[test]
    fn a_narrate_then_call_turn_keeps_its_text_boundary() {
        // A turn whose responses narrate around the call — text,
        // then the tool, then the closing text — saves as three
        // assistant records. The leading narration and its call were
        // one response and coalesce; the closing narration arrived
        // after the result returned, so the exchange closes first and
        // the narration follows as its own assistant message — the
        // two texts can never glue into one run of words.
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
            vec![
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant,
                Role::User
            ]
        );
        match &converted[1].parts[..] {
            [
                MessagePart::Text { text: before },
                MessagePart::ToolCall { .. },
            ] => {
                assert_eq!(before, "Let me check the file first");
            }
            parts => panic!("narration and its call share one assistant message: {parts:?}"),
        }
        assert!(
            matches!(&converted[2].parts[..], [MessagePart::ToolResult { .. }]),
            "the exchange's result rides its own user message"
        );
        assert!(
            matches!(&converted[3].parts[..], [MessagePart::Text { text }, ..] if text == "Done"),
            "the closing narration follows the result"
        );
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
            retained_input: String::new(),
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
    fn a_typed_prompt_reading_the_reminder_wording_stays() {
        // The steered flag is the corpse's tell: a prompt the user
        // actually typed — even one quoting the reminder's own
        // wording — is the user's words and keeps its place.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        crate::session::SessionSaver::with_base_dir(id, "m".to_string(), root.clone())
            .save_at(
                &[
                    user(
                        "reminder: 2 todo items still not completed — finish the \
                          remaining work",
                    ),
                    user_steered(
                        "reminder: 1 todo item still not completed — finish the \
                         remaining work",
                    ),
                ],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .unwrap();
        let outcome = load_for_resume_in(id, &root).unwrap();
        let texts: Vec<&str> = outcome
            .messages
            .iter()
            .filter_map(|message| match message {
                TuiMessage::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["reminder: 2 todo items still not completed — finish the remaining work"],
            "the typed prompt stays whole, the steered corpse drops: {texts:?}"
        );
    }

    #[test]
    fn a_persisted_reminder_row_is_dropped_on_load() {
        // Reminder rows written before the hide are corpses in older
        // display files — user-shaped, steered-flagged, replaying into
        // the transcript on every resume as if the user had said them.
        // The load drops them by their wording; everything the user
        // actually said, steered or typed, keeps its place.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();
        crate::session::SessionSaver::with_base_dir(id, "m".to_string(), root.clone())
            .save_at(
                &[
                    user("do the work"),
                    user_steered(
                        "reminder: 1 todo item still not completed — finish the \
                         remaining work, or mark the entries done / remove them \
                         with TodoWrite, then finish",
                    ),
                    assistant(vec![text_block("on it")]),
                    user_steered("take the other branch"),
                ],
                crate::session::SessionTokens::default(),
                now,
            )
            .unwrap();
        let outcome = load_for_resume_in(id, &root).unwrap();
        let texts: Vec<&str> = outcome
            .messages
            .iter()
            .filter_map(|message| match message {
                TuiMessage::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["do the work", "take the other branch"],
            "the corpse is gone; the typed prompt and the real steer \
             keep their places: {texts:?}"
        );
    }

    #[test]
    fn missing_machine_json_falls_back_to_derive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        crate::session::SessionSaver::with_base_dir(id, "m".to_string(), root.clone())
            .save_at(
                &[user("hi"), assistant(vec![text_block("hello")])],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .unwrap();
        let outcome = load_for_resume_in(id, &root).unwrap();
        assert!(outcome.machine.is_none(), "no sibling means no snapshot");
        assert_eq!(
            serde_json::to_value(history_for_resume(&outcome, false)).unwrap(),
            serde_json::to_value(tui_messages_to_loopctl(&outcome.messages, false)).unwrap(),
            "without a snapshot the derive walk is the history"
        );
    }

    #[test]
    fn a_fresh_machine_snapshot_wins_over_the_derive_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let stamp = chrono::Utc::now();
        crate::session::SessionSaver::with_base_dir(id, "m".to_string(), root.clone())
            .save_at(
                &[user("hi"), assistant(vec![text_block("hello")])],
                crate::session::SessionTokens::default(),
                stamp,
            )
            .unwrap();
        crate::session::SessionSaver::with_base_dir(id, "m".to_string(), root.clone())
            .save_machine(
                &[loopctl::message::Message::new(
                    loopctl::message::Role::User,
                    vec![loopctl::message::MessagePart::text("exact marker fa1")],
                )],
                stamp,
                0,
            )
            .unwrap();
        let outcome = load_for_resume_in(id, &root).unwrap();
        let history = history_for_resume(&outcome, false);
        assert_eq!(
            history.len(),
            1,
            "the snapshot is the history verbatim, not a derivation"
        );
        assert!(
            history[0].text_content().contains("exact marker fa1"),
            "the snapshot's content reaches the runner untouched"
        );
    }

    #[test]
    fn the_derive_walk_skips_compaction_notices() {
        let notice = TuiMessage::Compacted {
            tokens_before: 100,
            tokens_after: 40,
            reason: "threshold".to_string(),
            evicted: 3,
            duration_ms: 12,
            compactor: Some("qa".to_string()),
            compression_ratio: Some(2.5),
            timestamp: chrono::Utc::now(),
        };
        let without = vec![user("hi"), assistant(vec![text_block("hello")])];
        let with = vec![user("hi"), notice, assistant(vec![text_block("hello")])];
        assert_eq!(
            serde_json::to_value(tui_messages_to_loopctl(&with, false)).unwrap(),
            serde_json::to_value(tui_messages_to_loopctl(&without, false)).unwrap(),
            "a notice about the engine's context is never itself conversation"
        );
    }

    #[test]
    fn a_tool_marathon_reconstructs_as_alternating_exchanges() {
        // The shape a long agent run saves: one user task, then tool
        // and text records with no user texts between the replies.
        // Each new reply closes the previous exchange — its results
        // flush as their own user message — so the history alternates
        // at exchange granularity instead of coalescing into one
        // uncompactable assistant run.
        let mut transcript = vec![TuiMessage::User {
            text: "review the branch".to_string(),
            timestamp: now(),
            steered: false,
        }];
        for _round in 0..4 {
            transcript.push(assistant(vec![
                tool_block("Read", true),
                tool_block("Bash", true),
            ]));
            transcript.push(assistant(vec![text_block("round done")]));
        }
        let converted = tui_messages_to_loopctl(&transcript, false);
        assert_eq!(
            converted.len(),
            10,
            "one task message, then an assistant/user pair per exchange plus the closing reply"
        );
        for pair in converted.windows(2) {
            assert_ne!(
                pair[0].role, pair[1].role,
                "the reconstructed history strictly alternates: {converted:?}"
            );
        }
        let call_ids: Vec<String> = converted
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let result_ids: Vec<String> = converted
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            call_ids, result_ids,
            "every call keeps its result, in emission order"
        );
    }

    #[test]
    fn a_steered_row_resumes_as_the_user_message_the_run_read() {
        // The steered flag separates the session's own words (a steer, a
        // steer the run read) from the user's submissions for the
        // listing's count — but the row still names text the model read,
        // so the reconstructed history must carry it as a user message.
        // Dropping it here would orphan the follow-up turn that answered
        // it from the conversation a resume continues.
        let transcript = vec![
            user("the first prompt"),
            TuiMessage::User {
                text: "reminder: 1 todo item still not completed".to_string(),
                timestamp: now(),
                steered: true,
            },
            assistant(vec![text_block("the follow-up's answer")]),
        ];
        let converted = tui_messages_to_loopctl(&transcript, false);
        let user_texts: Vec<String> = converted
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                MessagePart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            user_texts.contains(&"reminder: 1 todo item still not completed".to_string()),
            "the steered row rides the history the model continues from, \
             flagged or not: {converted:?}"
        );
        assert_eq!(
            converted[0].role,
            Role::User,
            "the prompt and the reminder that followed mid-run resume as one \
             user turn — the shape the engine itself read: {converted:?}"
        );
        assert_eq!(
            converted.len(),
            2,
            "prompt-plus-reminder as one user turn, the answer as the other: \
             {converted:?}"
        );
    }

    #[test]
    fn tui_messages_to_loopctl_strips_thinking_blocks() {
        let transcript = vec![assistant(vec![
            ContentBlock::Thinking {
                text: "secret reasoning".to_string(),
                redacted: false,
                duration_ms: 1_000,
            },
            text_block("the reply"),
        ])];
        let converted = tui_messages_to_loopctl(&transcript, false);
        let Some(Message {
            role: Role::Assistant,
            parts,
            ..
        }) = converted.first()
        else {
            panic!("the assistant message converts: {converted:?}");
        };
        assert_eq!(parts.len(), 1, "only the text part rides the wire");
        assert!(
            !format!("{parts:?}").contains("secret reasoning"),
            "display-only reasoning never re-enters the API-shape history"
        );
    }

    #[test]
    fn empty_text_parts_never_reach_the_wire() {
        let legacy = assistant(vec![
            ContentBlock::Thinking {
                text: "tool deliberation".to_string(),
                redacted: false,
                duration_ms: 10,
            },
            text_block(""),
            text_block("real reply"),
        ]);
        let converted = tui_messages_to_loopctl(std::slice::from_ref(&legacy), false);
        let Some(Message {
            role: Role::Assistant,
            parts,
            ..
        }) = converted.first()
        else {
            panic!("the assistant message converts: {converted:?}");
        };
        assert!(
            !parts
                .iter()
                .any(|part| matches!(part, MessagePart::Text { text } if text.is_empty())),
            "an empty text part — recorded by an older build — must not ride the wire"
        );
    }

    #[test]
    fn a_thinking_only_record_converts_to_no_message() {
        let thinking_only = assistant(vec![ContentBlock::Thinking {
            text: "all reasoning, no reply".to_string(),
            redacted: false,
            duration_ms: 10,
        }]);
        let converted = tui_messages_to_loopctl(std::slice::from_ref(&thinking_only), false);
        assert!(
            converted.is_empty(),
            "a record whose every part strips has no wire shape at all: {converted:?}"
        );
    }

    #[test]
    fn readfiles_batches_re_arm_every_file_from_the_machine_history() {
        // The transcript's ReadFiles preview is a truncated one-line
        // summary, so the batch's paths come from the machine
        // snapshot's full call input — every file, not the preview's
        // first fragment.
        let transcript = vec![
            user("look"),
            assistant(vec![ContentBlock::Tool {
                call_id: String::new(),
                name: "ReadFiles".to_string(),
                input_preview: "a.rs, b.rs, c.rs and 9 more…".to_string(),
                success: true,
                elapsed_secs: 0.25,
                output_preview: "batch lines…".to_string(),
                retained_input: String::new(),
            }]),
        ];
        let machine = vec![Message::new(
            Role::Assistant,
            vec![MessagePart::tool_call(
                "call_1",
                "ReadFiles",
                serde_json::json!({"files": ["a.rs", "b.rs", "c.rs", "deep/nested/path.rs"]}),
            )],
        )];
        assert_eq!(
            resumed_read_paths_with_machine(&transcript, Some(&machine)),
            vec![
                "a.rs".to_string(),
                "b.rs".to_string(),
                "c.rs".to_string(),
                "deep/nested/path.rs".to_string()
            ],
            "the snapshot's full files array re-arms every file the batch read"
        );
        assert!(
            resumed_read_paths_with_machine(&transcript, None).is_empty(),
            "without a snapshot the truncated preview arms nothing it cannot name"
        );
    }

    #[test]
    fn a_machine_snapshot_replays_scrubbed_under_redaction() {
        // A session saved before redaction was enabled carries raw
        // secret bytes in its tool results; the snapshot route must
        // not re-admit them when the current policy hides them — the
        // same scrubbing the derive route applies.
        let machine = vec![
            Message::user("read the config"),
            Message::new(
                Role::Assistant,
                vec![MessagePart::tool_call(
                    "call_1",
                    "Read",
                    serde_json::json!({"file_path": "env.conf"}),
                )],
            ),
            Message::new(
                Role::User,
                vec![MessagePart::tool_result(
                    "call_1",
                    "Read",
                    loopctl::message::ToolContent::Text(
                        "api_key=abcdefghijklmnop\nport = 8080\n".to_string(),
                    ),
                    false,
                )],
            ),
        ];
        let outcome = ResumeOutcome {
            session_id: Uuid::nil(),
            messages: Vec::new(),
            machine: Some(machine),
            tokens: crate::session::SessionTokens::default(),
            machine_compactions: 0,
        };
        let redacted = history_for_resume(&outcome, true);
        let raw = redacted
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| {
                matches!(part, MessagePart::ToolResult { output, .. }
                if matches!(output, loopctl::message::ToolContent::Text(text)
                    if text.contains("abcdefghijklmnop")))
            });
        assert!(
            !raw,
            "the snapshot's secret bytes are scrubbed before replay"
        );
        let verbatim = history_for_resume(&outcome, false);
        let kept = verbatim
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| {
                matches!(part, MessagePart::ToolResult { output, .. }
                if matches!(output, loopctl::message::ToolContent::Text(text)
                    if text.contains("abcdefghijklmnop")))
            });
        assert!(kept, "redaction off replays the snapshot byte-identically");
    }

    #[test]
    fn a_secret_inside_a_multipart_tool_result_scrubs_before_replay() {
        // A tool that returns mixed content carries its text in the
        // multipart variant's parts; the scrub must reach inside each
        // text part exactly as it reaches the plain variant, or the
        // snapshot path replays the secret the dispatch pipeline would
        // have hidden.
        let machine = vec![Message::new(
            Role::User,
            vec![MessagePart::tool_result(
                "call_9",
                "Bash",
                loopctl::message::ToolContent::Multipart(vec![
                    loopctl::message::ToolContentPart::Text {
                        text: "stdout:\napi_key=abcdefghijklmnop\n".to_string(),
                    },
                ]),
                false,
            )],
        )];
        let outcome = ResumeOutcome {
            session_id: Uuid::nil(),
            messages: Vec::new(),
            machine: Some(machine),
            tokens: crate::session::SessionTokens::default(),
            machine_compactions: 0,
        };
        let replayed = serde_json::to_string(&history_for_resume(&outcome, true)).unwrap();
        assert!(
            !replayed.contains("abcdefghijklmnop"),
            "the multipart part's secret bytes are scrubbed before replay: {replayed}"
        );
        let verbatim = serde_json::to_string(&history_for_resume(&outcome, false)).unwrap();
        assert!(
            verbatim.contains("abcdefghijklmnop"),
            "redaction off replays the multipart bytes byte-identically"
        );
    }

    #[test]
    fn a_secret_nested_in_a_tool_calls_arguments_scrubs_before_replay() {
        // The write family carries whatever bytes the model asked to put
        // in a file, nested below the argument surface; a snapshot
        // resumed onto a different provider must not replay bytes that
        // provider was never shown. The token is composed to the shape
        // the scrubber matches — the prefix plus 36 token characters —
        // rather than stored as a credential-shaped literal.
        let token = format!("ghp_{}", "a".repeat(36));
        let machine = vec![
            Message::user("point the client at my account"),
            Message::new(
                Role::Assistant,
                vec![MessagePart::tool_call(
                    "call_2",
                    "MultiEdit",
                    serde_json::json!({
                        "file_path": "src/client.rs",
                        "edits": [{
                            "old_text": "let token = String::new();",
                            "new_text": format!("let token = \"{token}\";"),
                        }],
                    }),
                )],
            ),
        ];
        let outcome = ResumeOutcome {
            session_id: Uuid::nil(),
            messages: Vec::new(),
            machine: Some(machine.clone()),
            tokens: crate::session::SessionTokens::default(),
            machine_compactions: 0,
        };
        let replayed = serde_json::to_string(&history_for_resume(&outcome, true)).unwrap();
        assert!(
            !replayed.contains(&token),
            "the nested argument bytes are scrubbed before replay: {replayed}"
        );
        assert!(
            replayed.contains("[REDACTED:"),
            "the scrub leaves its placeholder inside the arguments: {replayed}"
        );
        let verbatim = serde_json::to_string(&history_for_resume(&outcome, false)).unwrap();
        assert_eq!(
            verbatim,
            serde_json::to_string(&machine).unwrap(),
            "redaction off replays the snapshot byte-identically"
        );
    }

    #[test]
    fn read_previews_are_extracted_as_resumable_paths() {
        let transcript = vec![
            user("look"),
            assistant(vec![
                tool_block("read", true),
                tool_block("Read", true),
                tool_block("Bash", true),
                text_block("done"),
            ]),
            assistant(vec![text_block("and again")]),
        ];
        assert_eq!(
            resumed_read_paths(&transcript),
            vec!["a.rs".to_string(), "a.rs".to_string()],
            "both the current and the legacy read spelling re-arm; other tools' previews are summaries"
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
            vec![Role::User, Role::Assistant, Role::User, Role::Assistant],
            "the reply after the call closes the exchange — the result rides its own \
             user message — and determinism must not paper over the shape"
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

    /// Save a transcript plus a machine half stamped at the same moment.
    fn save_pair(
        dir: &std::path::Path,
        id: Uuid,
        messages: &[TuiMessage],
        machine: &[loopctl::message::Message],
    ) {
        // The engine half saves last so the pair rule sees it as at
        // least as new — the shape a publishing freeze produces, where
        // both halves carry fresh stamps but one stopped growing.
        let saver =
            crate::session::SessionSaver::with_base_dir(id, "m".to_string(), dir.to_path_buf());
        saver
            .save(messages, crate::session::SessionTokens::default())
            .expect("save the display half");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let stamp = chrono::Utc::now();
        saver
            .save_machine(machine, stamp, 0)
            .expect("save the engine half");
    }

    /// A long transcript with no compaction record, in exchange pairs.
    fn marathon_transcript(turns: usize) -> Vec<TuiMessage> {
        let mut messages = Vec::new();
        for turn in 0..turns {
            messages.push(user(&format!("turn {turn}")));
            messages.push(assistant(vec![tool_block("Read", true)]));
            messages.push(assistant(vec![text_block(&format!("step {turn} done"))]));
        }
        messages
    }

    #[test]
    fn a_stale_machine_half_degrades_to_the_derive_with_a_warning() {
        // The freeze shape: the engine half stopped growing mid-run
        // while the transcript kept landing, and both halves share a
        // stamp because the writer stamps them together. Adopting the
        // stale half silently amneses everything after the freeze; the
        // guard degrades to the derive walk and says so in the
        // transcript itself.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let transcript = marathon_transcript(10);
        let frozen: Vec<loopctl::message::Message> = (0..4)
            .map(|step| loopctl::message::Message::user(format!("frozen {step}")))
            .collect();
        save_pair(dir.path(), id, &transcript, &frozen);

        let mut args = parse(&[]);
        args.continue_session = Some(true);
        match resolve_resume_in(&args, dir.path()) {
            ResumeControl::Resumed(outcome) => {
                assert!(
                    outcome.machine.is_none(),
                    "the frozen half is refused, not adopted"
                );
                assert!(
                    outcome.messages.iter().any(|m| matches!(m,
                        TuiMessage::System { text, .. }
                            if text.contains("engine snapshot was stale"))),
                    "the degradation is visible in the transcript: {:?}",
                    outcome.messages.len()
                );
            }
            ResumeControl::Fresh { .. } => panic!("the session resumes, degraded"),
        }
    }

    #[test]
    fn a_compacted_machine_half_is_adopted_despite_the_gap() {
        // Compaction legitimately shrinks the engine half far below
        // the transcript beside it; the guard keys on the compaction
        // record, not the raw gap, so the snapshot rides.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let mut transcript = marathon_transcript(10);
        transcript.push(TuiMessage::Compacted {
            tokens_before: 210_000,
            tokens_after: 40_000,
            reason: "threshold exceeded".to_string(),
            evicted: 120,
            duration_ms: 300_000,
            compactor: Some("qa".to_string()),
            compression_ratio: Some(5.2),
            timestamp: now(),
        });
        let compacted: Vec<loopctl::message::Message> = (0..5)
            .map(|step| loopctl::message::Message::user(format!("kept {step}")))
            .collect();
        save_pair(dir.path(), id, &transcript, &compacted);

        let mut args = parse(&[]);
        args.continue_session = Some(true);
        match resolve_resume_in(&args, dir.path()) {
            ResumeControl::Resumed(outcome) => {
                assert!(
                    outcome.machine.is_some(),
                    "the compacted half is adopted — the record explains the gap"
                );
                assert!(
                    !outcome.messages.iter().any(|m| matches!(m,
                        TuiMessage::System { text, .. }
                            if text.contains("engine snapshot was stale"))),
                    "no degradation row for a legitimate compaction gap"
                );
            }
            ResumeControl::Fresh { .. } => panic!("the session resumes"),
        }
    }

    #[test]
    fn a_counted_compaction_exempts_a_headless_machine_half() {
        // A headless session's display transcript never carries a
        // Compacted row — its builder sees no compaction events — so
        // the snapshot's own compaction count is the guard's only
        // witness that the engine half's gap is a legitimate
        // compaction rather than a publishing freeze.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let transcript = marathon_transcript(10);
        let compacted: Vec<loopctl::message::Message> = (0..5)
            .map(|step| loopctl::message::Message::user(format!("kept {step}")))
            .collect();
        let saver = crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        );
        saver
            .save(&transcript, crate::session::SessionTokens::default())
            .expect("save the display half");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let envelope = serde_json::json!({
            "format": "dch.machine.v1",
            "saved_at": chrono::Utc::now(),
            "messages": compacted,
            "compactions": 2,
        });
        let machine_path = dir.path().join(id.to_string()).join("machine.json");
        std::fs::create_dir_all(machine_path.parent().expect("the session dir"))
            .expect("session dir");
        std::fs::write(&machine_path, envelope.to_string()).expect("machine half");

        let mut args = parse(&[]);
        args.continue_session = Some(true);
        match resolve_resume_in(&args, dir.path()) {
            ResumeControl::Resumed(outcome) => {
                assert!(
                    outcome.machine.is_some(),
                    "the counted half is adopted — the count explains the gap"
                );
                assert!(
                    !outcome.messages.iter().any(|m| matches!(m,
                        TuiMessage::System { text, .. }
                            if text.contains("engine snapshot was stale"))),
                    "no degradation row for a counted compaction gap"
                );
            }
            ResumeControl::Fresh { .. } => panic!("the session resumes"),
        }
    }

    #[test]
    fn a_short_session_never_trips_the_staleness_guard() {
        // Small transcripts sit under the guard's floor; a fresh
        // session's few rows must adopt their snapshot untouched.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let transcript = marathon_transcript(2);
        let machine: Vec<loopctl::message::Message> = (0..3)
            .map(|step| loopctl::message::Message::user(format!("seen {step}")))
            .collect();
        save_pair(dir.path(), id, &transcript, &machine);

        let mut args = parse(&[]);
        args.continue_session = Some(true);
        match resolve_resume_in(&args, dir.path()) {
            ResumeControl::Resumed(outcome) => {
                assert!(
                    outcome.machine.is_some(),
                    "short sessions adopt their snapshot"
                );
            }
            ResumeControl::Fresh { .. } => panic!("the session resumes"),
        }
    }

    #[test]
    fn continue_resolves_the_most_recently_saved_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let older = Uuid::new_v4();
        let newer = Uuid::new_v4();
        save_session(dir.path(), older, "m", &[user("older turn")]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_session(dir.path(), newer, "m", &[user("newer turn")]);

        let mut args = parse(&[]);
        args.continue_session = Some(true);
        match resolve_resume_in(&args, dir.path()) {
            ResumeControl::Resumed(outcome) => {
                assert_eq!(outcome.session_id, newer, "the latest save wins");
                assert!(
                    outcome.messages.iter().any(
                        |m| matches!(m, TuiMessage::User { text, .. } if text == "newer turn")
                    ),
                    "the newest session's transcript loads"
                );
            }
            ResumeControl::Fresh { .. } => panic!("--continue resumes"),
        }

        // Nothing saved: a fresh start with a note, never a crash.
        let empty = tempfile::tempdir().expect("tempdir");
        match resolve_resume_in(&args, empty.path()) {
            ResumeControl::Fresh { warn, .. } => assert!(
                warn.as_deref()
                    .is_some_and(|w| w.contains("no saved session")),
                "the empty case explains itself: {warn:?}"
            ),
            ResumeControl::Resumed(_) => panic!("no sessions means fresh"),
        }
    }

    fn save_session(dir: &std::path::Path, id: Uuid, model: &str, messages: &[TuiMessage]) {
        crate::session::SessionSaver::with_base_dir(id, model.to_string(), dir.to_path_buf())
            .save(messages, crate::session::SessionTokens::default())
            .expect("save the fixture session");
    }

    #[test]
    fn the_listing_cap_keeps_the_newest_and_counts_the_hidden() {
        fn summary(id: u32) -> crate::session::SessionSummary {
            crate::session::SessionSummary {
                id: Uuid::from_u64_pair(u64::from(id), 0),
                model: "m".to_string(),
                last_activity: now(),
                user_message_count: 1,
                context_tokens: 0,
            }
        }
        let sessions: Vec<_> = (0..13).map(summary).collect();
        let (shown, hidden) = cap_for_listing(sessions.clone(), crate::args::ListCount::Count(10));
        assert_eq!(shown.len(), 10, "the default keeps ten");
        assert_eq!(hidden, 3, "the older three stay counted");
        assert_eq!(
            shown.first().map(|s| s.id),
            sessions.first().map(|s| s.id),
            "the cap keeps the head — the newest sessions"
        );
        let (all, hidden) = cap_for_listing(sessions, crate::args::ListCount::All);
        assert_eq!(all.len(), 13, "all shows everything");
        assert_eq!(hidden, 0);
    }

    #[test]
    fn a_resumed_outcome_carries_the_sessions_token_totals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let totals = crate::session::SessionTokens {
            cumulative_input: 9_999,
            cumulative_output: 111,
            last_input_tokens: 9_999,
            context_tokens: 9_876,
        };
        crate::session::SessionSaver::with_base_dir(id, "m".to_string(), dir.path().to_path_buf())
            .save(&[user("hi")], totals)
            .expect("save");
        let outcome = load_for_resume_in(id, dir.path()).expect("load");
        assert_eq!(
            outcome.tokens, totals,
            "the resumed session continues its own accounting"
        );
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
            user_message_count: count,
            context_tokens: 0,
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
            ["SESSION ID", "MODEL", "LAST ACTIVITY", "MSGS", "CONTEXT"]
                .iter()
                .all(|column| header.contains(column)),
            "the header names every column: {header}"
        );
        assert!(
            rule.chars().all(|c| c == '─' || c == ' '),
            "the rule is dashes under the columns: {rule}"
        );
        let total = sessions
            .first()
            .expect("the fixture row")
            .context_tokens
            .to_string();
        let data_row = table.lines().last().expect("the data row");
        let digits_end = data_row
            .rfind(&total)
            .map(|start| start.saturating_add(total.len()))
            .expect("the total renders");
        assert_eq!(
            digits_end,
            header.trim_end().len(),
            "the tokens figure right-aligns under its header: {data_row}"
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
            row.chars().count() <= ID_WIDTH + MODEL_WIDTH + ACTIVITY_WIDTH + 4 + CONTEXT_WIDTH + 7,
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
