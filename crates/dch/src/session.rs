//! Session persistence — crash-atomic transcripts under `~/.dch/sessions`.
//!
//! [`SessionSaver`] writes a pair of JSON files per session
//! (`~/.dch/sessions/<uuid>/`): `session.json`, the display
//! conversation, and `machine.json`, the engine's committed history —
//! the working set the model carried. The display file is rewritten
//! atomically after every completed turn through a same-directory
//! temp file and rename; the machine file uses the same discipline
//! under its own stamp, the moment its content was read from the
//! runner. Resume seeds a new engine from the machine file whenever
//! it is at least as new as the display file beside it, so a resumed
//! session continues process-identically — the same messages the
//! model would have seen had the process never exited — and degrades
//! to deriving over the display transcript otherwise. The session id
//! always comes from the agent loop — the saver never mints one.

use std::path::{Path, PathBuf};

use dch_tui::TuiMessage;
use uuid::Uuid;

/// The on-disk format tag this version writes and the only one it reads.
///
/// Everything this saver persists is namespaced by the tag, so a
/// future format change is a new tag rather than a silent
/// reinterpretation: a reader that meets a tag it does not know
/// refuses the file as corrupt instead of guessing at its shape.
const FORMAT: &str = "dch.v1";

/// The format tag of the machine-history sibling file.
///
/// Namespaces the engine-side snapshot the same way [`FORMAT`]
/// namespaces the display transcript, so a machine history written by
/// a future shape is refused rather than misread.
const MACHINE_FORMAT: &str = "dch.machine.v1";

/// Persists and restores conversation sessions under
/// `~/.dch/sessions/<uuid>/session.json`.
///
/// One directory per session holds `session.json` plus any sibling
/// artifacts the runtime writes alongside it; the saver owns only its
/// own file. Saves are whole-file rewrites through a same-directory
/// temp file and rename, so a crash mid-write leaves the previous
/// good transcript intact. The session id is consumed, never created —
/// callers pass the identity the agent loop already carries.
pub struct SessionSaver {
    /// The session whose file this saver reads and writes.
    ///
    /// Names the on-disk directory; sourced from the runner, not
    /// minted here, so a saved file always matches the id the loop
    /// is using.
    session_id: Uuid,

    /// The model the session runs, recorded in the envelope.
    ///
    /// Read back by listing and resume so a session can be
    /// re-instantiated with the model it was started with.
    model: String,

    /// The root sessions directory, injectable for tests.
    ///
    /// Every path the saver touches derives from it, so pointing it
    /// at a throwaway directory moves the whole on-disk footprint —
    /// the seam that keeps tests off the real `~/.dch/sessions`.
    base_dir: PathBuf,
}

/// Summary of one saved session, as returned by listing.
///
/// Read from the file's envelope rather than filesystem metadata, so
/// a transcript copied between machines keeps its real model and
/// activity time instead of the copy's.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    /// The session identity, also the on-disk directory name.
    ///
    /// Reported from the directory rather than the envelope inside,
    /// so it is always the address a `--resume` can reach — the two
    /// agree for every file this saver writes and stay coherent for
    /// one copied or moved by hand.
    pub id: Uuid,

    /// The model the session ran.
    ///
    /// Read from the envelope so a listing shows what the session
    /// actually used, letting a user pick a session to resume
    /// without remembering which model it started with.
    pub model: String,

    /// When the session was last saved.
    ///
    /// Each save restamps this, so it tracks the last completed
    /// turn rather than the session's start.
    pub last_activity: chrono::DateTime<chrono::Utc>,

    /// How many messages the transcript holds.
    ///
    /// The user's submissions, one per prompt — not the raw entry
    /// count: tool calls, replies, and error rows stay out, and so do
    /// steered echo rows, which carry the session's own words (a
    /// steer, a hook-injected reminder) rather than a prompt the user
    /// submitted.
    pub user_message_count: usize,

    /// The engine's context-size estimate at the last save.
    ///
    /// The same figure a running TUI's usage bar fills against and a
    /// headless run's save records from the loop itself, so the
    /// listing's CONTEXT column and the session agree on what
    /// "context" means — approximately the context a resume
    /// re-loads, since a resume replays capped previews and
    /// re-estimates — not the session's lifetime token spend, which
    /// grows monotonically and can dwarf any window.
    pub context_tokens: u64,
}

/// The machine-history sibling's on-disk shape; private like
/// [`StoredSession`].
///
/// Carries the engine's committed conversation under its own format
/// tag and stamp. The stamp is the snapshot's read time, so the pair
/// rule — a machine history older than the display file beside it is
/// stale — judges content age, not write order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredMachine {
    /// The format tag; [`MACHINE_FORMAT`] for everything this version
    /// writes.
    ///
    /// Refused on read when unknown, exactly as the display file's
    /// tag is.
    format: String,

    /// When the runner's committed history was read for this
    /// snapshot.
    ///
    /// Written by the caller at read time and compared against the
    /// display file's save stamp on resume.
    saved_at: chrono::DateTime<chrono::Utc>,

    /// The engine's committed history, in conversation order.
    ///
    /// Verbatim loopctl messages — the working set the model carried
    /// — so seeding a new runner from it continues the session exactly
    /// where the process left off.
    messages: Vec<loopctl::message::Message>,

    /// How many compaction passes the session had run when this
    /// snapshot was taken.
    ///
    /// The display transcript is the usual witness — its Compacted
    /// rows explain why the engine half is small — but a headless
    /// session's transcript builder never records one, so the count
    /// rides the snapshot itself and the resume guard reads it as the
    /// same exemption. Absent in older files, reading as zero: a
    /// pre-count snapshot never claims a compaction it cannot show.
    #[serde(default)]
    compactions: u32,
}

/// Errors arising while saving, loading, or listing sessions.
///
/// Typed so resume can tell "no such session" (start fresh) from
/// "the file is corrupt" (warn, offer salvage) instead of matching
/// on message text.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// No session exists under the requested session id.
    ///
    /// A normal state — a typo'd id, a deleted file, or a stray
    /// regular file squatting on the directory's name — and distinct
    /// from corruption: the caller can degrade to a fresh session
    /// without warning about damaged history.
    #[error("session {0} not found")]
    NotFound(Uuid),

    /// The file exists but is not a readable session transcript.
    ///
    /// Covers unparseable JSON, a wrong envelope shape, and an
    /// unknown format tag — anything a future version or a foreign
    /// writer could leave behind.
    #[error("session file is corrupt: {0}")]
    Corrupt(String),

    /// The filesystem refused a read or write.
    ///
    /// Permissions, a full disk, a vanished directory — failures of
    /// the medium rather than the file's contents. Unlike the other
    /// variants this one tends to block every subsequent session
    /// operation too, so callers treat it as a hard stop.
    #[error("session I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The transcript could not be serialized.
    ///
    /// A write-side failure only — the in-memory conversation could
    /// not be turned into JSON. Loads never produce it: an
    /// unparseable file is reported as corruption instead.
    #[error("session serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// The on-disk JSON shape; private — callers go through
/// [`SessionSaver`].
///
/// The envelope wraps the messages with the fields listing and resume
/// need, so a summary never requires parsing the message array. Field
/// order is fixed for diff-stability across saves; the `format` tag
/// versions the shape so a reader can refuse what it does not know.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredSession {
    /// The format tag; [`FORMAT`] for everything this version writes.
    ///
    /// Checked on every read — an unknown tag is refused as
    /// corruption, which is what keeps a future format readable
    /// only by the version that understands it.
    format: String,

    /// The session identity the agent loop minted.
    ///
    /// Recorded for provenance and inspection; the directory name —
    /// not this field — is the addressable identity a resume or
    /// listing resolves by.
    session_id: Uuid,

    /// The model the session ran.
    ///
    /// Written by the host at saver construction so the envelope
    /// always carries it; the listing surfaces it, and a resume reads
    /// it as provenance — the engine runs on the current
    /// configuration's model and provider.
    model: String,

    /// When this save happened; restamped per save.
    ///
    /// The listing's last-activity column and its newest-first sort
    /// both read this stamp, never filesystem metadata — a
    /// transcript copied between machines keeps its real history.
    saved_at: chrono::DateTime<chrono::Utc>,

    /// The conversation in order, interleaving preserved.
    ///
    /// The display model verbatim: what renders in the TUI is what
    /// was saved, and resume seeds both the display and the agent
    /// reconstruction from this one array.
    messages: Vec<TuiMessage>,

    /// The session's cumulative token totals at save time.
    ///
    /// Restamped per save like the timestamp, so the newest file
    /// carries the whole session's accounting. Absent on files saved
    /// before totals were persisted; those read as zero and keep
    /// accumulating from the resume.
    #[serde(default)]
    tokens: SessionTokens,
}

/// A session's cumulative token accounting.
///
/// What the status bar's running total shows. The TUI persists and
/// restores it, so a resumed session continues the count instead of
/// restarting it at zero. A headless single-run has no live counter to
/// carry, so it saves the spend halves as zeros — the context figure is
/// the exception, read off the run that just finished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionTokens {
    /// Input tokens since the session began.
    ///
    /// Summed across every turn the API reported — a monotonically
    /// growing spend figure, restamped per save and re-seeded on
    /// resume so the count continues rather than restarts.
    pub(crate) cumulative_input: u64,

    /// Output tokens since the session began.
    ///
    /// The output half of the same accounting; added to
    /// [`cumulative_input`](Self::cumulative_input) it is exactly the
    /// figure the status bar's total shows.
    pub(crate) cumulative_output: u64,

    /// The input tokens of the most recent turn.
    ///
    /// What the API was billed for reading last — the system
    /// prompt, the replayed history, the tool results — and so the
    /// approximate size of the context a resume re-loads. Zero on
    /// files saved before it was tracked.
    #[serde(default)]
    pub(crate) last_input_tokens: u64,

    /// The engine's context-size estimate at the moment of the save.
    ///
    /// The same figure the loop's `context_tokens` accessor reports —
    /// what the status bar's usage bar shows across a resume, before
    /// the first turn re-reports it. Absent on files saved before the
    /// field existed; those read as zero and the first turn refills
    /// the bar.
    #[serde(default)]
    pub(crate) context_tokens: u64,
}

impl SessionSaver {
    /// Construct a saver that records the given model in the envelope.
    ///
    /// The constructor hosts use — they know the model from the
    /// config they just built the runner with, and listing and resume
    /// read it back.
    #[must_use]
    pub fn with_model(session_id: Uuid, model: String) -> Self {
        Self {
            session_id,
            model,
            base_dir: sessions_dir(),
        }
    }

    /// Construct a saver rooted at `base_dir` instead of the default.
    ///
    /// The injection point every test path shares — the session
    /// suite and the headless test seam both point it at a throwaway
    /// directory so no test ever touches the real `~/.dch/sessions`.
    pub(crate) fn with_base_dir(session_id: Uuid, model: String, base_dir: PathBuf) -> Self {
        Self {
            session_id,
            model,
            base_dir,
        }
    }

    /// The file this saver writes: `<base>/<uuid>/session.json`.
    ///
    /// The layout is the addressing contract — a session is its
    /// directory, and everything that loads, lists, or resumes one
    /// builds this same path from the identity.
    fn session_path(&self) -> PathBuf {
        self.base_dir
            .join(self.session_id.to_string())
            .join("session.json")
    }

    /// The machine-history sibling: `<base>/<uuid>/machine.json`.
    ///
    /// Sits beside [`session.json`](Self::session_path) in the same
    /// directory so the pair travels with a copied session directory;
    /// the display file never references it, so its absence degrades
    /// resume to the derive walk rather than breaking the load.
    fn machine_path(&self) -> PathBuf {
        self.base_dir
            .join(self.session_id.to_string())
            .join("machine.json")
    }

    /// Serialize the engine's working set into the machine-history
    /// file, atomically.
    ///
    /// The snapshot is the loop's committed history — exactly what the
    /// model would have carried had the process never exited — and
    /// `saved_at` is the moment the caller read it from the runner, not
    /// the moment of this write, so a resume comparing it against the
    /// display file's stamp judges the pair by when each half was
    /// true, never by how long the write queue was backed up.
    ///
    /// # Errors
    ///
    /// Same conditions as [`SessionSaver::save_at`], against the sibling
    /// path.
    pub fn save_machine(
        &self,
        machine: &[loopctl::message::Message],
        saved_at: chrono::DateTime<chrono::Utc>,
        compactions: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.machine_path();
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let stored = StoredMachine {
            format: MACHINE_FORMAT.to_string(),
            saved_at,
            messages: machine.to_vec(),
            compactions,
        };
        let json = serde_json::to_string(&stored)?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut temp, json.as_bytes())?;
        drop(
            temp.persist(&path)
                .map_err(|err| SessionError::Io(err.error))?,
        );
        Ok(())
    }

    /// Serialize `messages` into the session file, atomically, stamped
    /// at this call.
    ///
    /// The test-facing convenience: production paths stamp by content
    /// time through [`SessionSaver::save_at`] — the turn-end hook's
    /// fire time on the TUI path, the shared pair stamp on the
    /// headless path — so the machine-history sibling's read-time
    /// stamp beside it compares content against content.
    ///
    /// # Errors
    ///
    /// Fails when the directory or file cannot be created, written,
    /// or renamed — always through a boxed [`SessionError`].
    #[cfg(test)]
    pub fn save(
        &self,
        messages: &[TuiMessage],
        tokens: SessionTokens,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.save_at(messages, tokens, chrono::Utc::now())?;
        Ok(())
    }

    /// The stamped core of [`SessionSaver::save_at`].
    ///
    /// `saved_at` is the moment this snapshot's content was true —
    /// the hook's fire time on the turn-end path, not the writer's
    /// write time — so the machine-history sibling's read-time stamp
    /// beside it compares content against content: a pair is fresh
    /// exactly when the engine half saw everything the display half
    /// recorded.
    ///
    /// # Errors
    ///
    /// Same conditions as [`SessionSaver::save_at`], typed through a
    /// boxed [`SessionError`].
    pub fn save_at(
        &self,
        messages: &[TuiMessage],
        tokens: SessionTokens,
        saved_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.save_inner(messages, tokens, saved_at)?;
        Ok(())
    }

    /// The typed core of [`SessionSaver::save_at`].
    ///
    /// # Errors
    ///
    /// Same conditions as [`SessionSaver::save_at`], typed.
    fn save_inner(
        &self,
        messages: &[TuiMessage],
        tokens: SessionTokens,
        saved_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), SessionError> {
        let path = self.session_path();
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let stored = StoredSession {
            format: FORMAT.to_string(),
            session_id: self.session_id,
            model: self.model.clone(),
            saved_at,
            messages: messages.to_vec(),
            tokens,
        };
        let json = serde_json::to_string_pretty(&stored)?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut temp, json.as_bytes())?;
        drop(
            temp.persist(&path)
                .map_err(|err| SessionError::Io(err.error))?,
        );
        Ok(())
    }

    /// Enumerate all saved sessions with summary metadata,
    /// newest-first.
    ///
    /// Scans UUID-named directories under the sessions root. A
    /// session whose file is missing or unreadable is skipped with a
    /// warning rather than failing the listing — one corrupt file
    /// must not blank the table.
    ///
    /// # Errors
    ///
    /// Fails only when the sessions root itself cannot be read;
    /// per-session problems are skipped, not propagated.
    pub fn list_sessions() -> Result<Vec<SessionSummary>, Box<dyn std::error::Error>> {
        Ok(list_sessions_in(&sessions_dir())?)
    }
}

/// The default sessions root: `~/.dch/sessions`.
///
/// Derived from the same `~/.dch` the config loader resolves, so
/// configuration and transcripts can never disagree about where the
/// user's data lives.
pub(crate) fn sessions_dir() -> PathBuf {
    dch_config::config_dir().join("sessions")
}

/// Load a session's messages together with the model its envelope
/// records, rooted at `base`.
///
/// The resume path's read: one parse yields both the display model
/// and the model the session ran, so a resumed session can keep its
/// original model without a second read. Rooted at `base` so test
/// paths never touch the real sessions directory.
///
/// # Errors
///
/// Same conditions as [`load_envelope`], with the envelope's model
/// carried out alongside the messages.
pub(crate) fn load_with_meta_in(
    id: Uuid,
    base: &Path,
) -> Result<
    (
        Vec<TuiMessage>,
        String,
        SessionTokens,
        chrono::DateTime<chrono::Utc>,
    ),
    SessionError,
> {
    let stored = load_envelope(&base.join(id.to_string()).join("session.json"), id, base)?;
    Ok((
        stored.messages,
        stored.model,
        stored.tokens,
        stored.saved_at,
    ))
}

/// Load a session's machine-history snapshot when the pair is fresh.
///
/// The pair rule: a snapshot older than the display file beside it is
/// stale — the display half recorded turns the engine half never saw,
/// so seeding from it would silently drop them; resume derives from
/// the display transcript instead, which is lossy but complete. An
/// absent, unreadable, or unknown-format snapshot degrades the same
/// way with one warn, so the worst case of any sibling-file problem is
/// exactly the pre-snapshot resume behavior.
///
/// Returns the committed history to seed a new runner with, or `None`
/// when the caller should derive.
pub(crate) fn load_machine_in(
    id: Uuid,
    base: &Path,
    session_saved_at: chrono::DateTime<chrono::Utc>,
) -> Option<(Vec<loopctl::message::Message>, u32)> {
    let path = base.join(id.to_string()).join("machine.json");
    let json = match std::fs::read_to_string(&path) {
        Ok(json) => json,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "machine history unreadable; deriving resume from the display transcript"
            );
            return None;
        }
    };
    let stored: StoredMachine = match serde_json::from_str(&json) {
        Ok(stored) => stored,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "machine history unparseable; deriving resume from the display transcript"
            );
            return None;
        }
    };
    if stored.format != MACHINE_FORMAT {
        tracing::warn!(
            format = %stored.format,
            "unknown machine-history format; deriving resume from the display transcript"
        );
        return None;
    }
    if stored.saved_at < session_saved_at {
        tracing::warn!(
            session = %id,
            "machine history predates the display file; deriving resume from the display transcript"
        );
        return None;
    }
    Some((stored.messages, stored.compactions))
}

/// Read and parse a transcript file into its full envelope.
///
/// The single parse every load path shares — messages-only loads and
/// metadata-carrying loads alike — so no caller re-reads the file.
///
/// # Errors
///
/// [`SessionError::NotFound`] when the file is absent or no path to
/// it can exist — a stray regular file occupying the session
/// directory's name reads as `ENOTDIR`, which is the same "nothing
/// resumable under this id" fact as an absent directory;
/// [`SessionError::Corrupt`] when it does not parse or its format
/// tag is unknown; [`SessionError::Io`] when the read fails —
/// including an `ENOTDIR` whose broken component is the sessions
/// root itself, which is a broken layout worth stopping for, not a
/// missing session: degrading to a fresh id there would silently
/// lose every later save to the same broken root.
fn load_envelope(
    path: &Path,
    session_id: Uuid,
    base: &Path,
) -> Result<StoredSession, SessionError> {
    let json = match std::fs::read_to_string(path) {
        Ok(json) => json,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionError::NotFound(session_id));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => {
            if base.is_dir() {
                return Err(SessionError::NotFound(session_id));
            }
            return Err(SessionError::Io(err));
        }
        Err(err) => return Err(SessionError::Io(err)),
    };
    let stored: StoredSession =
        serde_json::from_str(&json).map_err(|err| SessionError::Corrupt(err.to_string()))?;
    if stored.format != FORMAT {
        return Err(SessionError::Corrupt(format!(
            "unknown format tag {:?}",
            stored.format
        )));
    }
    Ok(stored)
}

/// The listing core, rooted at `base`.
///
/// Returns summaries sorted newest-first by the envelope's save
/// stamp. Only canonically UUID-named directories count — anything
/// else (non-UUID names, non-canonical spellings, corrupt or
/// missing files) is warned about where meaningful and skipped, so
/// one bad entry never blanks the table.
///
/// # Errors
///
/// Fails only when the root directory cannot be read — per-session
/// problems skip the entry instead.
pub(crate) fn list_sessions_in(base: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => return Err(SessionError::Io(err)),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(name) else {
            continue;
        };
        // The address resume builds is the id's canonical spelling,
        // so a directory named in any other form — braced, urn, or
        // case-variant — cannot be reached through the id this
        // listing would report. Skipping it keeps the table and the
        // load path agreeing on what an id addresses.
        if id.to_string() != name {
            tracing::warn!(session = %id, directory = %name, "skipping a non-canonical session directory name");
            continue;
        }
        let file = path.join("session.json");
        let json = match std::fs::read_to_string(&file) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(session = %id, error = %err, "skipping unreadable session file");
                continue;
            }
        };
        let stored: StoredSession = match serde_json::from_str(&json) {
            Ok(stored) => stored,
            Err(err) => {
                tracing::warn!(session = %id, error = %err, "skipping corrupt session file");
                continue;
            }
        };
        if stored.format != FORMAT {
            tracing::warn!(session = %id, format = %stored.format, "skipping unknown session format");
            continue;
        }
        // The directory is the addressable identity — resume reaches a
        // transcript by it — so the summary reports it even when the
        // envelope inside was written under a different id (a copied
        // or hand-moved file).
        sessions.push(SessionSummary {
            id,
            model: stored.model,
            last_activity: stored.saved_at,
            user_message_count: stored
                .messages
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        dch_tui::TuiMessage::User {
                            steered: false,
                            unsent: false,
                            ..
                        }
                    )
                })
                .count(),
            context_tokens: stored.tokens.context_tokens,
        });
    }
    sessions.sort_by_key(|summary| std::cmp::Reverse(summary.last_activity));
    Ok(sessions)
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
    use dch_tui::ContentBlock;

    fn saver(dir: &std::path::Path, id: Uuid) -> SessionSaver {
        SessionSaver::with_base_dir(id, "test-model".to_string(), dir.to_path_buf())
    }

    /// Load through the path-injectable core so tests never read the
    /// real sessions root.
    fn load(dir: &std::path::Path, id: Uuid) -> Result<Vec<TuiMessage>, SessionError> {
        Ok(load_envelope(&dir.join(id.to_string()).join("session.json"), id, dir)?.messages)
    }

    fn sample_messages() -> Vec<TuiMessage> {
        let now = chrono::Utc::now();
        vec![
            TuiMessage::User {
                text: "fix the leak".to_string(),
                timestamp: now,
                steered: false,
                unsent: false,
            },
            TuiMessage::Assistant {
                blocks: vec![
                    ContentBlock::Text {
                        text: "reading first".to_string(),
                    },
                    ContentBlock::Tool {
                        call_id: String::new(),
                        name: "Read".to_string(),
                        input_preview: "a.rs".to_string(),
                        success: true,
                        elapsed_secs: 0.25,
                        output_preview: "…".to_string(),
                        retained_input: String::new(),
                    },
                    ContentBlock::Text {
                        text: "done".to_string(),
                    },
                ],
                timestamp: now,
                duration_ms: Some(900),
            },
            TuiMessage::System {
                text: "resumed".to_string(),
                timestamp: now,
            },
            TuiMessage::Error {
                text: "boom".to_string(),
                timestamp: now,
            },
        ]
    }

    /// Write an envelope with a controlled save stamp, bypassing
    /// `save`'s `Utc::now()`.
    fn write_envelope(
        dir: &std::path::Path,
        id: Uuid,
        saved_at: chrono::DateTime<chrono::Utc>,
        messages: &[TuiMessage],
    ) {
        let stored = StoredSession {
            format: FORMAT.to_string(),
            session_id: id,
            model: "test-model".to_string(),
            saved_at,
            messages: messages.to_vec(),
            tokens: SessionTokens::default(),
        };
        let path = dir.join(id.to_string()).join("session.json");
        std::fs::create_dir_all(path.parent().expect("the session dir")).expect("mkdir");
        let json = serde_json::to_string_pretty(&stored).expect("serialize");
        std::fs::write(path, json).expect("write the envelope");
    }

    fn stamp(seconds: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(&format!("2026-09-16T12:{seconds:02}:00Z"))
            .expect("a fixed stamp")
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn save_load_round_trips_interleaved_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let messages = sample_messages();
        saver(dir.path(), id)
            .save(&messages, SessionTokens::default())
            .expect("save");
        let loaded = load(dir.path(), id).expect("load");
        assert_eq!(
            loaded, messages,
            "variants, blocks, and ordering survive the round trip"
        );
    }

    #[test]
    fn the_message_count_skips_steered_rows() {
        // The listing's MSGS column names the user's submissions —
        // one per prompt. Steered echo rows are the session's own
        // words riding the transcript as user rows (a steer the run
        // read, a hook-injected reminder), and counting them would
        // tell the user they submitted prompts they never typed.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let messages = vec![
            dch_tui::TuiMessage::User {
                text: "the first prompt".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            },
            dch_tui::TuiMessage::User {
                text: "a steer the run read".to_string(),
                timestamp: chrono::Utc::now(),
                steered: true,
                unsent: false,
            },
            dch_tui::TuiMessage::User {
                text: "the second prompt".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            },
        ];
        crate::session::SessionSaver::with_base_dir(id, "m".to_string(), dir.path().to_path_buf())
            .save(&messages, crate::session::SessionTokens::default())
            .expect("save");
        let listed = list_sessions_in(dir.path()).expect("list");
        assert_eq!(
            listed.first().expect("the one session").user_message_count,
            2,
            "only the submitted prompts count — the steered row is \
             the session's own word, not the user's"
        );
    }

    #[test]
    fn token_totals_survive_a_save_and_default_for_legacy_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let totals = SessionTokens {
            cumulative_input: 12_345,
            cumulative_output: 678,
            last_input_tokens: 54_321,
            context_tokens: 45_678,
        };
        crate::session::SessionSaver::with_base_dir(
            id,
            "test-model".to_string(),
            dir.path().to_path_buf(),
        )
        .save(&sample_messages(), totals)
        .expect("save");
        let (_, _, loaded, _) = load_with_meta_in(id, dir.path()).expect("load");
        assert_eq!(loaded, totals, "the accounting rides the envelope");
        let listed = list_sessions_in(dir.path()).expect("list");
        assert_eq!(
            listed.first().expect("the one session").context_tokens,
            45_678,
            "the listing carries the context estimate the status bar \
             fills against — not the lifetime spend"
        );

        // A file saved before totals existed parses to zeros: the
        // envelope omits the key entirely, as the old writer did.
        let legacy = r#"{"format":"dch.v1","session_id":"ID","model":"m","saved_at":"2026-01-01T00:00:00Z","messages":[]}"#;
        let legacy_id = Uuid::new_v4();
        let legacy_dir = dir.path().join(legacy_id.to_string());
        std::fs::create_dir_all(&legacy_dir).expect("legacy dir");
        std::fs::write(
            legacy_dir.join("session.json"),
            legacy.replace("ID", &legacy_id.to_string()),
        )
        .expect("legacy file");
        let (_, _, legacy_tokens, _) =
            load_with_meta_in(legacy_id, dir.path()).expect("load legacy");
        assert_eq!(
            legacy_tokens,
            SessionTokens::default(),
            "a pre-totals file reads as zero and keeps accumulating"
        );
    }

    #[test]
    fn envelope_fields_survive_a_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        saver(dir.path(), id)
            .save(&sample_messages(), SessionTokens::default())
            .expect("save");
        let raw = std::fs::read_to_string(dir.path().join(id.to_string()).join("session.json"))
            .expect("read raw");
        assert!(raw.contains(r#""format": "dch.v1""#), "tag written: {raw}");
        assert!(raw.contains(&format!("\"{id}\"")), "id written: {raw}");
        assert!(raw.contains("test-model"), "model written: {raw}");
        assert!(
            raw.contains("saved_at"),
            "the save stamp is part of the envelope: {raw}"
        );
    }

    #[test]
    fn the_file_lands_at_sessions_uuid_session_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let path = dir.path().join(id.to_string()).join("session.json");
        assert!(!path.exists(), "nothing saved yet");
        saver(dir.path(), id)
            .save(&sample_messages(), SessionTokens::default())
            .expect("save into a missing directory");
        assert!(path.is_file(), "the layout is sessions/<uuid>/session.json");
    }

    #[test]
    fn an_atomic_save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        saver(dir.path(), id)
            .save(&sample_messages(), SessionTokens::default())
            .expect("save");
        let entries: Vec<String> = std::fs::read_dir(dir.path().join(id.to_string()))
            .expect("the session dir")
            .filter_map(|entry| {
                entry
                    .ok()
                    .and_then(|e| e.file_name().to_str().map(str::to_string))
            })
            .collect();
        assert_eq!(
            entries,
            vec!["session.json".to_string()],
            "the rename consumed the temp file"
        );
    }

    #[test]
    fn a_rapid_second_save_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let saver = saver(dir.path(), id);
        saver
            .save(&sample_messages(), SessionTokens::default())
            .expect("first save");
        let second = vec![TuiMessage::User {
            text: "second".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        }];
        saver
            .save(&second, SessionTokens::default())
            .expect("second save");
        assert_eq!(
            load(dir.path(), id).expect("load"),
            second,
            "the newest write is what loads"
        );
    }

    #[test]
    fn load_reports_a_missing_session_as_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let err = load_envelope(
            &dir.path().join(id.to_string()).join("session.json"),
            id,
            dir.path(),
        )
        .map(|stored| stored.messages)
        .expect_err("nothing was saved");
        assert!(
            matches!(&err, SessionError::NotFound(missing) if *missing == id),
            "a missing file is typed NotFound, got {err:?}"
        );
    }

    #[test]
    fn a_stray_file_where_the_session_directory_would_be_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        // A regular file squatting on the directory name: reading
        // <id>/session.json fails with ENOTDIR, not ENOENT. The root is
        // sound, so this is the same "nothing resumable" fact as a
        // missing id — not a layout worth stopping for.
        std::fs::write(dir.path().join(id.to_string()), "not a directory")
            .expect("write the stray file");
        let err = load_envelope(
            &dir.path().join(id.to_string()).join("session.json"),
            id,
            dir.path(),
        )
        .map(|stored| stored.messages)
        .expect_err("no session can exist under the id");
        assert!(
            matches!(&err, SessionError::NotFound(missing) if *missing == id),
            "an unreachable path is typed NotFound like an absent one, got {err:?}"
        );
    }

    #[test]
    fn a_regular_file_where_the_sessions_root_belongs_is_an_io_failure() {
        // ENOTDIR with the root itself as the broken component is a
        // broken sessions layout, not a missing session: degrading to
        // fresh would hand the run a session id whose every later save
        // fails against the same file root, losing the transcript
        // silently. The load must surface Io — the arm resume exits on.
        let root = tempfile::NamedTempFile::new().expect("the root as a regular file");
        let id = Uuid::new_v4();
        let err = load_envelope(
            &root.path().join(id.to_string()).join("session.json"),
            id,
            root.path(),
        )
        .map(|stored| stored.messages)
        .expect_err("no session can load under a file root");
        assert!(
            matches!(err, SessionError::Io(ref io_err) if io_err.kind() == std::io::ErrorKind::NotADirectory),
            "the broken root is typed Io, got {err:?}"
        );
    }

    #[test]
    fn load_reports_garbage_as_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let path = dir.path().join(id.to_string()).join("session.json");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, "{not valid json").expect("write garbage");
        let err = load_envelope(&path, id, dir.path())
            .map(|stored| stored.messages)
            .expect_err("garbage must fail");
        assert!(
            matches!(err, SessionError::Corrupt(_)),
            "garbage is typed Corrupt, got {err:?}"
        );
    }

    #[test]
    fn load_reports_a_wrong_shape_as_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let path = dir.path().join(id.to_string()).join("session.json");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, r#"{"wrong":"shape"}"#).expect("write a wrong shape");
        let err = load_envelope(&path, id, dir.path())
            .map(|stored| stored.messages)
            .expect_err("a wrong shape must fail");
        assert!(
            matches!(err, SessionError::Corrupt(_)),
            "a wrong shape is typed Corrupt, got {err:?}"
        );
    }

    #[test]
    fn load_refuses_an_unknown_format_tag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let path = dir.path().join(id.to_string()).join("session.json");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        let future = format!(
            "{{\"format\":\"dch.v9\",\"session_id\":\"{id}\",\"model\":\"m\",\"saved_at\":\"2026-09-16T12:00:00Z\",\"messages\":[]}}"
        );
        std::fs::write(&path, future).expect("write a future format");
        let err = load_envelope(&path, id, dir.path())
            .map(|stored| stored.messages)
            .expect_err("an unknown tag must be refused");
        assert!(
            matches!(&err, SessionError::Corrupt(msg) if msg.contains("dch.v9")),
            "an unknown format tag is Corrupt, got {err:?}"
        );
    }

    #[test]
    fn list_sessions_on_an_empty_base_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            list_sessions_in(dir.path()).expect("list"),
            Vec::new(),
            "an empty root lists nothing"
        );
        let missing = dir.path().join("never-created");
        assert_eq!(
            list_sessions_in(&missing).expect("list"),
            Vec::new(),
            "a missing root lists nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_inaccessible_sessions_root_reports_the_io_error() {
        use std::os::unix::fs::PermissionsExt as _;
        if unsafe { libc::getuid() } == 0 {
            // Root ignores directory permissions, so the lock this
            // test forces would not block the stat it probes.
            return;
        }
        let parent = tempfile::tempdir().expect("tempdir");
        let root = parent.path().join("sessions");
        std::fs::create_dir_all(&root).expect("mkdir");
        let mut locked = std::fs::metadata(parent.path())
            .expect("the parent")
            .permissions();
        locked.set_mode(0o000);
        std::fs::set_permissions(parent.path(), locked).expect("lock the parent");
        let listed = list_sessions_in(&root);
        let mut open = std::fs::metadata(parent.path())
            .expect("the parent")
            .permissions();
        open.set_mode(0o755);
        std::fs::set_permissions(parent.path(), open).expect("unlock the parent");
        assert!(
            matches!(listed, Err(SessionError::Io(_))),
            "a root the process cannot reach is an error, not an empty listing: {listed:?}"
        );
    }

    #[test]
    fn list_sessions_sorts_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let oldest = Uuid::new_v4();
        let middle = Uuid::new_v4();
        let newest = Uuid::new_v4();
        write_envelope(dir.path(), oldest, stamp(0), &sample_messages());
        write_envelope(dir.path(), newest, stamp(40), &sample_messages());
        write_envelope(dir.path(), middle, stamp(20), &sample_messages());
        let listed = list_sessions_in(dir.path()).expect("list");
        let ids: Vec<Uuid> = listed.iter().map(|summary| summary.id).collect();
        assert_eq!(
            ids,
            vec![newest, middle, oldest],
            "summaries arrive newest-first"
        );
    }

    #[test]
    fn list_sessions_skips_a_corrupt_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = Uuid::new_v4();
        let bad = Uuid::new_v4();
        write_envelope(dir.path(), good, stamp(0), &sample_messages());
        let bad_path = dir.path().join(bad.to_string()).join("session.json");
        std::fs::create_dir_all(bad_path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&bad_path, "{broken").expect("write a corrupt sibling");
        let listed = list_sessions_in(dir.path()).expect("list");
        let ids: Vec<Uuid> = listed.iter().map(|summary| summary.id).collect();
        assert_eq!(ids, vec![good], "the corrupt sibling is skipped, not fatal");
    }

    #[test]
    fn list_sessions_ignores_non_uuid_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stray = dir.path().join("not-a-uuid");
        std::fs::create_dir_all(&stray).expect("mkdir");
        std::fs::write(stray.join("session.json"), "{}").expect("filler");
        let good = Uuid::new_v4();
        write_envelope(dir.path(), good, stamp(0), &sample_messages());
        let listed = list_sessions_in(dir.path()).expect("list");
        let ids: Vec<Uuid> = listed.iter().map(|summary| summary.id).collect();
        assert_eq!(ids, vec![good], "only UUID-named directories count");
    }

    #[test]
    fn list_sessions_skips_a_non_canonical_uuid_directory_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reachable = Uuid::new_v4();
        write_envelope(dir.path(), reachable, stamp(0), &sample_messages());
        // The same id in a spelling resume would never build: the
        // canonical form is the only address the loader has.
        let mangled = Uuid::new_v4();
        write_envelope(dir.path(), mangled, stamp(10), &sample_messages());
        let upper = dir.path().join(mangled.to_string().to_uppercase());
        std::fs::rename(dir.path().join(mangled.to_string()), &upper).expect("rename");
        let listed = list_sessions_in(dir.path()).expect("list");
        let ids: Vec<Uuid> = listed.iter().map(|summary| summary.id).collect();
        assert_eq!(
            ids,
            vec![reachable],
            "a non-canonical name lists nothing — its id would not reach it on resume"
        );
    }

    #[test]
    fn list_sessions_counts_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let messages = sample_messages();
        let expected = messages
            .iter()
            .filter(|message| matches!(message, dch_tui::TuiMessage::User { .. }))
            .count();
        write_envelope(dir.path(), id, stamp(0), &messages);
        let listed = list_sessions_in(dir.path()).expect("list");
        assert_eq!(
            listed.first().expect("the one session").user_message_count,
            expected,
            "the summary counts the user's turns, not every entry"
        );
    }

    #[test]
    fn an_empty_transcript_saves_and_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        saver(dir.path(), id)
            .save(&[], SessionTokens::default())
            .expect("save empty");
        assert_eq!(
            load(dir.path(), id).expect("load"),
            Vec::new(),
            "an empty transcript round-trips"
        );
    }

    #[test]
    fn the_on_disk_tags_are_the_frozen_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        saver(dir.path(), id)
            .save(&sample_messages(), SessionTokens::default())
            .expect("save");
        let raw = std::fs::read_to_string(dir.path().join(id.to_string()).join("session.json"))
            .expect("read raw");
        assert!(
            raw.contains(r#""role": "user""#),
            "message tags are the frozen role tags: {raw}"
        );
        assert!(
            raw.contains(r#""type": "tool""#),
            "block tags are the frozen type tags: {raw}"
        );
    }

    #[test]
    fn a_copied_transcript_lists_under_its_directory_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = Uuid::new_v4();
        write_envelope(dir.path(), original, stamp(0), &sample_messages());
        let copy = Uuid::new_v4();
        let copy_dir = dir.path().join(copy.to_string());
        std::fs::create_dir_all(&copy_dir).expect("mkdir");
        std::fs::copy(
            dir.path().join(original.to_string()).join("session.json"),
            copy_dir.join("session.json"),
        )
        .expect("copy the transcript");

        let listed = list_sessions_in(dir.path()).expect("list");
        let ids: Vec<Uuid> = listed.iter().map(|summary| summary.id).collect();
        assert!(
            ids.contains(&original) && ids.contains(&copy),
            "each directory lists under its own identity: {ids:?}"
        );
        assert_eq!(
            ids.len(),
            2,
            "a copied transcript does not duplicate the envelope's identity"
        );
    }

    /// A working set carrying a real tool-call id, as a tool-using
    /// turn leaves in the engine's committed history.
    fn machine_history() -> Vec<loopctl::message::Message> {
        use loopctl::message::Message;
        use loopctl::message::MessagePart;
        use loopctl::message::Role;
        vec![
            Message::new(Role::User, vec![MessagePart::text("read the note")]),
            Message::new(
                Role::Assistant,
                vec![MessagePart::tool_call(
                    "call_7f3a",
                    "Read",
                    serde_json::json!({"path": "note.txt"}),
                )],
            ),
        ]
    }

    /// The JSON rendering of a message list, for value equality.
    ///
    /// Messages are `#[non_exhaustive]` without `PartialEq`, but they
    /// are serializable — comparing renderings is the same equality
    /// a round trip must preserve, byte for byte.
    fn rendered(messages: &[loopctl::message::Message]) -> serde_json::Value {
        serde_json::to_value(messages).expect("messages serialize")
    }

    #[test]
    fn a_machine_snapshot_round_trips_its_compaction_count() {
        // The count is the headless transcript's only compaction
        // witness: what the saver writes is what the resume guard's
        // exemption reads, and a pre-count file loads as zero.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        write_envelope(dir.path(), id, stamp(10), &sample_messages());
        saver(dir.path(), id)
            .save_machine(&machine_history(), stamp(10), 3)
            .expect("machine saved");
        let restored = load_machine_in(id, dir.path(), stamp(10)).expect("a fresh pair loads");
        assert_eq!(restored.1, 3, "the count survives the round trip");
        assert_eq!(
            restored.0.len(),
            machine_history().len(),
            "the count rides beside the history it describes"
        );
    }

    #[test]
    fn a_machine_snapshot_restores_the_working_set_exactly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        write_envelope(dir.path(), id, stamp(10), &sample_messages());
        saver(dir.path(), id)
            .save_machine(&machine_history(), stamp(10), 0)
            .expect("machine saved");
        let restored = load_machine_in(id, dir.path(), stamp(10)).expect("a fresh pair loads");
        assert_eq!(rendered(&restored.0), rendered(&machine_history()));
        assert!(
            restored
                .0
                .iter()
                .flat_map(|message| &message.parts)
                .any(|part| matches!(part, loopctl::message::MessagePart::ToolCall { id, .. } if id == "call_7f3a")),
            "the model-issued call id survives the round trip"
        );
    }

    #[test]
    fn a_stale_pair_degrades_to_derive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        write_envelope(dir.path(), id, stamp(20), &sample_messages());
        saver(dir.path(), id)
            .save_machine(&machine_history(), stamp(10), 0)
            .expect("machine saved");
        assert!(
            load_machine_in(id, dir.path(), stamp(20)).is_none(),
            "a snapshot older than the display file must not seed the resume"
        );
    }

    #[test]
    fn call_ids_survive_a_second_save_resume_cycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        let keeper = saver(dir.path(), id);
        keeper
            .save_machine(&machine_history(), stamp(10), 0)
            .expect("first machine save");
        write_envelope(dir.path(), id, stamp(10), &sample_messages());
        let first = load_machine_in(id, dir.path(), stamp(10)).expect("first cycle loads");
        keeper
            .save_machine(&first.0, stamp(30), first.1)
            .expect("second machine save");
        write_envelope(dir.path(), id, stamp(30), &sample_messages());
        let second = load_machine_in(id, dir.path(), stamp(30)).expect("second cycle loads");
        assert_eq!(
            rendered(&first.0),
            rendered(&second.0),
            "a save-resume-save cycle keeps the working set byte-stable"
        );
    }

    #[test]
    fn an_unknown_machine_format_degrades_to_derive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = Uuid::new_v4();
        write_envelope(dir.path(), id, stamp(10), &sample_messages());
        // A complete envelope with an alien tag: the parse succeeds, so
        // the refusal below is genuinely the format check's verdict.
        std::fs::write(
            dir.path().join(id.to_string()).join("machine.json"),
            "{\"format\": \"dch.machine.v99\", \"saved_at\": \"2026-09-16T12:10:00Z\", \
             \"messages\": [], \"compactions\": 0}",
        )
        .expect("alien machine file written");
        assert!(
            load_machine_in(id, dir.path(), stamp(10)).is_none(),
            "an unknown tag degrades to the derive walk, never a guess"
        );
    }
}
