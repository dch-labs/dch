//! Transcript persistence: the background writer that folds
//! display and machine snapshots into session files, the handle
//! turns use to feed it, and the naming slot session identity
//! arms and settles through it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dch_tui::TuiMessage;

/// The session-file token totals for the live counters.
///
/// One mapping for both save sites — the turn-end hook and the
/// pre-first-frame snapshot — so the file always carries the totals
/// the status bar showed at the moment of the save, including the
/// ones a resume restored; a poisoned lock records zeros rather than
/// blocking a save.
pub(crate) fn snapshot_tokens(
    live: &Arc<std::sync::Mutex<dch_tui::TokenCounts>>,
) -> crate::session::SessionTokens {
    live.lock().map_or_else(
        |_| crate::session::SessionTokens::default(),
        |counts| crate::session::SessionTokens {
            cumulative_input: counts.cumulative_input,
            cumulative_output: counts.cumulative_output,
            last_input_tokens: counts.input,
            context_tokens: counts.context_tokens,
        },
    )
}

/// The handle a turn-end hook uses to hand transcripts to the writer.
///
/// Holds the shared one-slot mailbox: publishing a snapshot
/// overwrites whatever is still waiting, so at most one pending
/// transcript is ever retained — a stalled write cannot pile
/// snapshots up behind it. Publishing is all the hook does — cheap
/// enough to run inline on the render task.
pub(crate) struct TranscriptHandle {
    /// The one-slot mailbox this handle publishes into, shared with
    /// the writer.
    ///
    /// Holds the newest waiting snapshot of each half — display and
    /// machine — under one lock; a publish overwrites the waiting
    /// snapshot of its own half rather than queueing behind it,
    /// which is what bounds retention at one transcript per half.
    pending: Arc<std::sync::Mutex<PendingSaves>>,

    /// The condvar that wakes the writer for a publish.
    ///
    /// The writer sleeps only while the slot sits empty and
    /// re-checks on every wake, so a notify racing a drain costs
    /// one spurious look and a published snapshot never waits
    /// unseen.
    signal: Arc<std::sync::Condvar>,

    /// The writer's current save target.
    ///
    /// Held in a swappable cell so a `/clear` can move future saves
    /// to a fresh session file while the ended one stands, and a
    /// relabel can swap the model without moving the session id. A
    /// publish pins the saver it finds into the snapshot it takes,
    /// so a save already in flight keeps the destination it was
    /// aimed at — a swap mid-write cannot rename it onto the new
    /// file.
    pub(crate) saver: Arc<std::sync::Mutex<Arc<crate::session::SessionSaver>>>,

    /// The session id a naming call is currently in flight for.
    ///
    /// Armed for exactly the session whose claim spawned the
    /// one-shot title call and cleared when that call settles, so a
    /// prompt claimed while its own session's name is still being
    /// coined buys no second call. The slot follows identity: a
    /// session the host swapped in meanwhile — a `/clear` rotation, a
    /// live resume — owes its own call and fires it, and the retired
    /// session's settling task can neither arm nor disarm another
    /// session's naming. Shared by every clone — the spawned task
    /// settles it through its own handle.
    naming_in_flight: Arc<std::sync::Mutex<Option<uuid::Uuid>>>,
}

impl Clone for TranscriptHandle {
    /// Clone the mailbox handle.
    ///
    /// Both halves are shared arcs, so a clone publishes through the
    /// same one-slot mailbox the original does — the host keeps one to
    /// seed the session file before the first frame.
    fn clone(&self) -> Self {
        Self {
            pending: Arc::clone(&self.pending),
            signal: Arc::clone(&self.signal),
            saver: Arc::clone(&self.saver),
            naming_in_flight: Arc::clone(&self.naming_in_flight),
        }
    }
}

impl TranscriptHandle {
    /// Publish one conversation snapshot for the writer.
    ///
    /// Replaces the session's own waiting snapshot — within a session
    /// only the newest transcript is worth writing — while a snapshot
    /// the identity swap stranded for the outgoing session stays
    /// queued beside it, so a stalled writer cannot drop the ended
    /// session's last turn because the fresh session published behind
    /// it. Never blocks and never fails visibly.
    pub(crate) fn send(&self, messages: Vec<TuiMessage>, tokens: crate::session::SessionTokens) {
        let saver = self
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.push_display(TranscriptSnapshot {
            messages,
            tokens,
            saved_at: chrono::Utc::now(),
            saver,
        });
        drop(pending);
        self.signal.notify_one();
    }

    /// Publish one machine-history snapshot for the writer.
    ///
    /// Same per-session replacement as [`send`](Self::send), for the
    /// engine's committed history; the writer persists it under its
    /// own sibling file, so the display half in flight beside it is
    /// never held back by this half or vice versa.
    pub(crate) fn send_machine(&self, mut machine: MachineSnapshot) {
        machine.saver = Some(
            self.saver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        );
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.push_machine(machine);
        drop(pending);
        self.signal.notify_one();
    }

    /// The newest snapshots queued for `id`, display and machine.
    ///
    /// A live resume reads the target's latest published state through
    /// here before falling back to disk: a snapshot still waiting on
    /// the writer is newer than the file it will become, and resuming
    /// from the stale file would replace it. The read clones and
    /// leaves the queue untouched — the writer still persists what it
    /// holds while the resumed runner builds on the newer content. A
    /// machine half older than the display half it would pair with is
    /// left out, the same staleness judgment the disk path applies.
    /// Snapshots the writer has taken and is writing stay visible
    /// here until their write lands.
    pub(crate) fn latest_queued_for(&self, id: uuid::Uuid) -> Option<QueuedForResume> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let display = pending
            .display
            .iter()
            .rev()
            .find(|waiting| waiting.saver.session_id() == id)
            .or_else(|| {
                pending
                    .in_flight_display
                    .iter()
                    .rev()
                    .find(|waiting| waiting.saver.session_id() == id)
            })?;
        let machine = pending
            .machine
            .iter()
            .rev()
            .find(|waiting| waiting.saver.as_ref().map(|pinned| pinned.session_id()) == Some(id))
            .or_else(|| {
                pending.in_flight_machine.iter().rev().find(|waiting| {
                    waiting.saver.as_ref().map(|pinned| pinned.session_id()) == Some(id)
                })
            })
            .filter(|waiting| waiting.saved_at >= display.saved_at)
            .map(|waiting| {
                (
                    std::sync::Arc::clone(&waiting.messages),
                    waiting.compactions,
                )
            });
        Some(QueuedForResume {
            messages: display.messages.clone(),
            tokens: display.tokens,
            machine,
            model: display.saver.model().to_string(),
            permission_mode: display.saver.permission_mode(),
            saved_at: display.saved_at,
            title: display.saver.title().map(str::to_string),
        })
    }

    /// A handle with no writer behind it, for resume-flow tests.
    ///
    /// Published snapshots wait in the queue untouched — there is no
    /// thread to drain them — so a test drives the exact state a
    /// stalled writer leaves behind without racing one.
    #[cfg(test)]
    pub(crate) fn handle_without_writer(saver: Arc<crate::session::SessionSaver>) -> Self {
        Self {
            pending: Arc::new(std::sync::Mutex::new(PendingSaves::default())),
            signal: Arc::new(std::sync::Condvar::new()),
            saver: Arc::new(std::sync::Mutex::new(saver)),
            naming_in_flight: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Relabel the saver's model without moving its session id.
    ///
    /// A `/model` switch changes the model the session runs, and the
    /// envelope's model field must follow: the file a later
    /// `--resume` reads names the model this session ended on, which
    /// is the model the next run of that session defaults to. The
    /// id stays exactly where it is — the switch moves the model,
    /// never the identity.
    pub(crate) fn relabel_model(&self, model: &str) {
        let mut saver = self
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = saver.session_id();
        *saver = Arc::new(saver.with_identity(id, model.to_string()));
    }

    /// Move the writer's save target to a brand-new session, returning its id.
    ///
    /// A `/clear` calls this at rest once the loop's rewind emptied the
    /// history: the fresh identity sends every *later* save to a new file
    /// while the ended one stands whole, and a save already published keeps
    /// the destination it was published under — the pin lives in the
    /// snapshot, not the cell. The swap lives in the saver cell shared with
    /// the writer thread, so the turn-end hook's own handle — a clone of
    /// this one — begins writing under the new id from its next publish.
    /// The recorded model is the loop's current one, so the fresh file's
    /// provenance names the loop that writes it, and the fresh identity
    /// drops any permission-mode override — a fresh session runs the
    /// configuration's default. Returns the new session id
    /// as the status bar renders it, so the driver can publish it to the
    /// display.
    pub(crate) fn rotate(&self, model: &str) -> String {
        let mut saver = self
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = saver
            .with_identity(uuid::Uuid::new_v4(), model.to_string())
            .with_title(None)
            .with_permission_mode(None);
        let id = fresh.session_id().to_string();
        *saver = Arc::new(fresh);
        id
    }

    /// Move the writer's save target onto an existing session's file.
    ///
    /// A live resume calls this at rest: the session being adopted
    /// already has a file, and the writes that follow belong to it —
    /// the continued conversation appends to the identity it resumed
    /// rather than orphaning it under a fresh one. Like a rotation, a
    /// save already published keeps its own destination. The model
    /// recorded is the loop's current one, the same provenance a
    /// launch resume files under, and the adopted session's own
    /// permission-mode override rides along — `None` when it never
    /// carried one, dropping whatever the ended session had. Returns
    /// the id as the status bar renders it, so the driver can publish
    /// it to the display.
    pub(crate) fn retarget(
        &self,
        id: uuid::Uuid,
        model: &str,
        title: Option<String>,
        permission_mode: Option<dch_config::PermissionMode>,
    ) -> String {
        let mut saver = self
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *saver = Arc::new(
            saver
                .with_identity(id, model.to_string())
                .with_title(title)
                .with_permission_mode(permission_mode),
        );
        id.to_string()
    }

    /// Whether the session's name has landed on the save target.
    ///
    /// The claim path reads this through
    /// [`name_session_if_owed`](crate::tui::driver::name_session_if_owed):
    /// a session with a title is never renamed, and one without
    /// tries again on its next claimed prompt.
    pub(crate) fn has_title(&self) -> bool {
        self.saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .title()
            .is_some()
    }

    /// The session identity the save target currently writes under.
    ///
    /// Read under the saver lock alone — never nested with the
    /// naming slot — so the claim path can arm the slot for exactly
    /// the session it is about to name.
    pub(crate) fn current_session_id(&self) -> uuid::Uuid {
        self.saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_id()
    }

    /// Arm the naming slot for `id` unless its call is in flight.
    ///
    /// Returns whether the caller owes the call: `false` only when
    /// the slot already holds this session, the shape that keeps a
    /// quick second prompt in one session from buying a second call.
    /// A slot held for a different session — the one a rotation or
    /// retarget just retired — gives way, so the fresh session's
    /// first prompt always fires its own naming.
    pub(crate) fn arm_naming(&self, id: uuid::Uuid) -> bool {
        let mut slot = self
            .naming_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *slot == Some(id) {
            return false;
        }
        *slot = Some(id);
        true
    }

    /// Settle the naming slot for `id`.
    ///
    /// The spawned task calls this when its call answers or fails,
    /// clearing the slot only when it still names that session. A
    /// session swapped in meanwhile owns the slot now, and disarming
    /// its in-flight call is not the stale task's to do — its own
    /// settle must leave that session's naming armed.
    pub(crate) fn settle_naming(&self, id: uuid::Uuid) {
        let mut slot = self
            .naming_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *slot == Some(id) {
            *slot = None;
        }
    }

    /// Record the session's name on the save target, if it still is
    /// that session.
    ///
    /// The naming task calls this when its one-shot call answers —
    /// usually mid-run, between two publishes — with the identity it
    /// captured at spawn. The cell may have moved meanwhile: a
    /// `/clear` rotated it onto a fresh session, a live resume onto
    /// an adopted one, and words coined for the retired conversation
    /// belong to neither. The compare-and-set applies the title only
    /// when the cell still holds the named session, so a stale
    /// answer strands itself while the ended session's file stands as
    /// it saved. The identity and model ride untouched: a name
    /// changes nothing about where or as whom the session saves.
    pub(crate) fn set_title_for(&self, id: uuid::Uuid, title: String) {
        let mut saver = self
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if saver.session_id() != id {
            return;
        }
        let model = saver.model.clone();
        *saver = Arc::new(saver.with_identity(id, model).with_title(Some(title)));
    }

    /// Record the session's permission-mode override on the save target.
    ///
    /// A `/mode` move's remembering half: the mode the loop enforces is
    /// the session's own from the next save on. The identity, model,
    /// workdir, and title ride untouched — like a title, the override
    /// belongs to whatever session the cell currently targets — and
    /// passing `None` drops the override, which a `/clear`'s fresh
    /// identity does on purpose.
    pub(crate) fn set_permission_mode(&self, mode: Option<dch_config::PermissionMode>) {
        let mut saver = self
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = saver.session_id();
        let model = saver.model.clone();
        *saver = Arc::new(saver.with_identity(id, model).with_permission_mode(mode));
    }
}

/// One turn-end snapshot: the conversation and its accounting.
///
/// What the hook publishes and the writer persists together, so the
/// newest file always pairs the transcript with the totals the
/// status bar showed when it was taken — the two halves of a
/// session's state that a resume restores as one.
#[derive(Clone)]
struct TranscriptSnapshot {
    /// The conversation at turn end.
    ///
    /// The display model verbatim, ordered as rendered; the writer
    /// serializes it as the file's `messages` array unchanged.
    messages: Vec<TuiMessage>,

    /// The cumulative token totals at turn end.
    ///
    /// The session's accounting — lifetime input and output plus
    /// the last turn's input — read from the shared counters at
    /// publish time, so the file never carries a total the bar did
    /// not show.
    tokens: crate::session::SessionTokens,

    /// When this snapshot's content became true.
    ///
    /// The publish moment — the hook's fire time — so the machine
    /// sibling's read-time stamp beside it compares content against
    /// content, never write order.
    saved_at: chrono::DateTime<chrono::Utc>,

    /// The destination this snapshot was published under.
    ///
    /// Pinned at publish time: a rotation that lands after the publish
    /// cannot redirect a save already in flight — the ended session's
    /// last turn lands in the ended session's file, never in the fresh
    /// one that replaced it.
    saver: Arc<crate::session::SessionSaver>,
}

/// One machine-history snapshot: the engine's committed conversation.
///
/// What the turn-end hook publishes at every turn boundary and the
/// submission driver re-publishes when a run resolves; the writer
/// persists it under the sibling `machine.json`. The stamp is the
/// moment the snapshot was taken for persistence, so the pair rule on
/// resume judges content age rather than write order.
pub(crate) struct MachineSnapshot {
    /// The engine's history at the snapshot's turn boundary.
    ///
    /// The event's own coherent set — full on a successful turn,
    /// committed-only on a failed one, engine truth either way —
    /// shared behind an Arc so publishing never clones the history.
    pub(crate) messages: Arc<Vec<loopctl::message::Message>>,

    /// When this snapshot was taken for persistence.
    ///
    /// Carried through to the file's `saved_at` unchanged.
    pub(crate) saved_at: chrono::DateTime<chrono::Utc>,

    /// How many compaction passes the conversation had recorded when
    /// this snapshot was published.
    ///
    /// Counted from the display transcript's Compacted rows — the
    /// TUI transcript carries them, so the count and the rows agree;
    /// the driver's post-run republish passes zero, which loses
    /// nothing because the transcript's rows carry the exemption on
    /// this path.
    pub(crate) compactions: u32,

    /// The destination this snapshot was published under.
    ///
    /// Pinned by the publish itself: the handle's send path stamps the
    /// saver it read under its lock into the snapshot, so a later
    /// rotation or retarget cannot redirect a save already waiting.
    /// `None` only between construction and that stamp.
    pub(crate) saver: Option<Arc<crate::session::SessionSaver>>,
}

/// Count the compaction rows a conversation has recorded.
///
/// The TUI's machine snapshots source their compaction count from the
/// display transcript's own Compacted rows, so the snapshot file and
/// the transcript beside it always tell the guard the same story.
pub(crate) fn recorded_compactions(conversation: &[dch_tui::TuiMessage]) -> u32 {
    u32::try_from(
        conversation
            .iter()
            .filter(|message| matches!(message, dch_tui::TuiMessage::Compacted { .. }))
            .count(),
    )
    .unwrap_or(u32::MAX)
}

/// The writer's pending halves, newest-wins per session identity.
///
/// Display and machine snapshots arrive from different tasks on
/// different schedules — the hook fires from observer events while
/// the run future is still resolving, the driver's machine read
/// follows the resolution — so each half queues only its own kind.
/// Within a session the newest snapshot replaces the waiting one; a
/// rotation or retarget that swaps the identity in preserves the
/// outgoing session's snapshot beside the fresh one until both
/// persist — a stalled writer cannot lose the ended session's last
/// turn because the fresh session published behind it. A snapshot the
/// writer has taken stays visible in the in-flight half until its
/// write lands, so a resume read never falls back to the file the
/// write has not reached yet.
#[derive(Default)]
struct PendingSaves {
    /// Display snapshots waiting to persist, newest per session.
    ///
    /// One entry per waiting session identity: the session's own next
    /// publish replaces it, a swap in identity appends. Bounded by the
    /// sessions a swap strands while the writer stalls.
    display: Vec<TranscriptSnapshot>,

    /// Machine-history snapshots waiting to persist, newest per session.
    ///
    /// The same per-session rule on the engine half; the two halves
    /// never block each other.
    machine: Vec<MachineSnapshot>,

    /// Display snapshots the writer has taken and is writing.
    ///
    /// Held here — not in a local — so a resume read between the take
    /// and the write still sees the newest queued state instead of
    /// falling back to the file the write has not reached yet.
    in_flight_display: Vec<TranscriptSnapshot>,

    /// Machine snapshots the writer has taken and is writing.
    ///
    /// The same visibility rule on the engine half.
    in_flight_machine: Vec<MachineSnapshot>,
}

impl PendingSaves {
    /// Whether neither half is waiting.
    ///
    /// The writer's condvar predicate: sleep only when there is
    /// nothing of either kind to persist.
    fn is_empty(&self) -> bool {
        self.display.is_empty()
            && self.machine.is_empty()
            && self.in_flight_display.is_empty()
            && self.in_flight_machine.is_empty()
    }

    /// Queue one display snapshot, newest-wins within its session.
    ///
    /// The session the snapshot is pinned to decides which waiting
    /// entry it replaces — its own next publish, so a stalled writer
    /// persists only the newest transcript of a session — while an
    /// entry pinned to another identity stays queued beside it, so a
    /// rotation or retarget cannot silently drop the outgoing
    /// session's last turn.
    fn push_display(&mut self, snapshot: TranscriptSnapshot) {
        let id = snapshot.saver.session_id();
        self.display
            .retain(|waiting| waiting.saver.session_id() != id);
        self.display.push(snapshot);
    }

    /// Queue one machine snapshot, newest-wins within its session.
    ///
    /// The same per-identity rule as the display half, judged by the
    /// pin the publish stamped. A snapshot that carries no pin is
    /// dropped here: the writer could not aim it at any file, and no
    /// send path produces one — the stamp happens before the queue is
    /// reached.
    fn push_machine(&mut self, snapshot: MachineSnapshot) {
        let Some(saver) = snapshot.saver.as_ref() else {
            return;
        };
        let id = saver.session_id();
        self.machine
            .retain(|waiting| waiting.saver.as_ref().map(|pinned| pinned.session_id()) != Some(id));
        self.machine.push(snapshot);
    }
}

/// The newest queued halves of one session, read for a live resume.
///
/// A resume that is about to adopt an identity reads these before it
/// trusts the file on disk: a snapshot still waiting on the writer is
/// newer than the file it will become.
pub(crate) struct QueuedForResume {
    /// The display transcript at the newest waiting publish.
    ///
    /// Verbatim as the hook published it, ordered as rendered — the
    /// state the resumed display re-seeds from instead of the older
    /// contents a stalled writer has not written to the file yet.
    pub(crate) messages: Vec<TuiMessage>,

    /// The token totals that publish paired with the transcript.
    ///
    /// The session's accounting at publish time — lifetime input and
    /// output plus the last turn's input — so the resumed display
    /// continues the figures the conversation actually carried rather
    /// than restarting from the older file's.
    pub(crate) tokens: crate::session::SessionTokens,

    /// The engine's committed history at the newest waiting publish,
    /// when one waits.
    ///
    /// The history rides with the compaction count its snapshot
    /// recorded, so the resumed runner continues process-identically
    /// and the staleness guard reads the same story the snapshot
    /// tells.
    pub(crate) machine: Option<(std::sync::Arc<Vec<loopctl::message::Message>>, u32)>,

    /// The model the queued state was published under.
    ///
    /// Read from the pinned saver: a relabel that landed between the
    /// disk write and this publish is newer than the file's own
    /// record, and the adoption trusts the newer one.
    pub(crate) model: String,

    /// The permission-mode override the queued state carried.
    ///
    /// `None` leaves the file's own record standing; a recorded mode
    /// is what the conversation actually ran under at publish time.
    pub(crate) permission_mode: Option<dch_config::PermissionMode>,

    /// The publish moment of the newest waiting display.
    ///
    /// The freshness boundary the disk machine snapshot is re-judged
    /// against once the queued display becomes the adopted truth.
    pub(crate) saved_at: chrono::DateTime<chrono::Utc>,

    /// The session's recorded name at the newest waiting publish.
    ///
    /// Read from the pinned saver: a naming answer that landed between
    /// the disk write and this publish is newer than the file's own
    /// record, and the adoption carries it instead of leaving the
    /// resumed session untitled.
    pub(crate) title: Option<String>,
}

/// The single writer thread that persists turn-end snapshots.
///
/// One thread takes the queued snapshots — one per waiting session —
/// writes each under the destination it was published under, and
/// waits for the next — so the newest transcript of a session always
/// lands last and an older, slower save can never rename over a
/// newer one. Retention is bounded at one snapshot per waiting
/// session identity even while a write is stalled. Joining waits out
/// the in-flight write and anything still queued, so a quitting
/// session persists its last turn.
pub(crate) struct TranscriptWorker {
    /// The mailbox the writer drains, shared with the handles.
    ///
    /// The same lock every publish goes through: newest-wins per
    /// session identity, so the writer persists the newest snapshot
    /// of each waiting session and never a stack of stale ones.
    pending: Arc<std::sync::Mutex<PendingSaves>>,

    /// The condvar the writer waits on between snapshots.
    ///
    /// Its predicate is the slot sitting empty and the stop flag
    /// down, re-checked on every wake, so a shutdown notification
    /// cannot land unseen between the check and the wait.
    signal: Arc<std::sync::Condvar>,

    /// The writer's stop flag, set by a join or a drop.
    ///
    /// The loop leaves after draining whatever the slot still
    /// holds — a quitting session persists its final turn — and the
    /// drop path sets the same flag so an early-failure exit parks
    /// the writer off its wait.
    shutdown: Arc<AtomicBool>,

    /// The writer thread's join handle, taken at join.
    ///
    /// `None` once joined, so the idempotent drop finds nothing to
    /// wait on; before a spawn completes it is absent for the same
    /// reason.
    handle: Option<std::thread::JoinHandle<()>>,
}

/// How the writer waits for the next snapshot.
///
/// Takes the locked slot and returns it re-acquired — the condvar
/// wait in production, anything a test needs to interpose in
/// between.
type WaitFn = std::sync::Arc<
    dyn Fn(std::sync::MutexGuard<'_, PendingSaves>) -> std::sync::MutexGuard<'_, PendingSaves>
        + Send
        + Sync,
>;

impl TranscriptWorker {
    /// Spawn the writer for `saver` and return its publish handle.
    ///
    /// Constructs the session's single writer thread together with
    /// the handle the turn-end hook publishes through; the worker
    /// half is joined at teardown so the last transcript is on disk
    /// before the session returns.
    pub(crate) fn spawn(saver: Arc<crate::session::SessionSaver>) -> (TranscriptHandle, Self) {
        let signal = Arc::new(std::sync::Condvar::new());
        let wait_signal = Arc::clone(&signal);
        Self::spawn_inner(
            saver,
            signal,
            Arc::new(move |guard| {
                wait_signal
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }),
        )
    }

    /// Spawn a writer whose wait runs `park` first, holding the slot.
    ///
    /// Test seam: `park` runs while the slot lock is held, between
    /// the predicate check and the condvar wait — the exact state in
    /// which a notification fired without that lock would be lost.
    /// A test holds the writer there for exactly as long as it
    /// needs.
    #[cfg(test)]
    fn spawn_gated(
        saver: Arc<crate::session::SessionSaver>,
        park: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> (TranscriptHandle, Self) {
        let signal = Arc::new(std::sync::Condvar::new());
        let wait_signal = Arc::clone(&signal);
        Self::spawn_inner(
            saver,
            signal,
            Arc::new(move |guard| {
                park();
                wait_signal
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }),
        )
    }

    /// The writer body every spawn variant shares.
    ///
    /// Runs the loop — wait through the injected `wait`, take the
    /// newest snapshot, save it — and returns the handle/worker pair
    /// wired to one slot, signal, and shutdown flag. The variants
    /// differ only in the wait they inject.
    fn spawn_inner(
        saver: Arc<crate::session::SessionSaver>,
        signal: Arc<std::sync::Condvar>,
        wait: WaitFn,
    ) -> (TranscriptHandle, Self) {
        let pending = Arc::new(std::sync::Mutex::new(PendingSaves::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_pending = Arc::clone(&pending);
        let thread_shutdown = Arc::clone(&shutdown);
        let saver_cell = Arc::new(std::sync::Mutex::new(saver));
        let handle = std::thread::spawn(move || {
            loop {
                let mut guard = thread_pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while guard.is_empty() && !thread_shutdown.load(Ordering::SeqCst) {
                    guard = wait(guard);
                }
                let queued_display = std::mem::take(&mut guard.display);
                let queued_machine = std::mem::take(&mut guard.machine);
                guard.in_flight_display.extend(queued_display);
                guard.in_flight_machine.extend(queued_machine);
                drop(guard);
                loop {
                    let (display, machine) = {
                        let guard = thread_pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        (
                            guard.in_flight_display.first().cloned(),
                            guard.in_flight_machine.first().map(|snapshot| {
                                (
                                    std::sync::Arc::clone(&snapshot.messages),
                                    snapshot.saved_at,
                                    snapshot.compactions,
                                    snapshot.saver.clone(),
                                )
                            }),
                        )
                    };
                    if display.is_none() && machine.is_none() {
                        break;
                    }
                    if let Some(snapshot) = display {
                        if let Err(err) = snapshot.saver.save_at(
                            &snapshot.messages,
                            snapshot.tokens,
                            snapshot.saved_at,
                        ) {
                            tracing::warn!(error = %err, "session transcript could not be saved");
                        }
                        let mut guard = thread_pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.in_flight_display.remove(0);
                    }
                    if let Some((messages, saved_at, compactions, saver)) = machine {
                        let written = saver
                            .as_ref()
                            .map(|pinned| pinned.save_machine(&messages, saved_at, compactions));
                        if let Some(Err(err)) = written {
                            tracing::warn!(error = %err, "machine history could not be saved");
                        }
                        let mut guard = thread_pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.in_flight_machine.remove(0);
                    }
                    let guard = thread_pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.display.is_empty()
                        && guard.machine.is_empty()
                        && guard.in_flight_display.is_empty()
                        && guard.in_flight_machine.is_empty()
                        && thread_shutdown.load(Ordering::SeqCst)
                    {
                        break;
                    }
                }
                let guard = thread_pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if thread_shutdown.load(Ordering::SeqCst) && guard.is_empty() {
                    break;
                }
            }
        });
        let worker_pending = Arc::clone(&pending);
        (
            TranscriptHandle {
                pending,
                signal: Arc::clone(&signal),
                saver: saver_cell,
                naming_in_flight: Arc::new(std::sync::Mutex::new(None)),
            },
            TranscriptWorker {
                pending: worker_pending,
                signal,
                shutdown,
                handle: Some(handle),
            },
        )
    }

    /// Store the shutdown flag under the slot lock and wake the
    /// writer.
    ///
    /// The store must hold the lock — a notification fired between
    /// the writer's predicate check and its wait would otherwise be
    /// lost, and the writer would sleep forever. A snapshot still in
    /// the slot when the writer wakes is written before the thread
    /// exits.
    fn signal_shutdown(&self) {
        {
            let _pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.shutdown.store(true, Ordering::SeqCst);
        }
        self.signal.notify_all();
    }

    /// Wait for the writer to drain and exit.
    ///
    /// Runs the shutdown sequence and joins the thread; when this
    /// returns, the last published transcript is on disk.
    pub(crate) fn join(mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

impl Drop for TranscriptWorker {
    /// Park off the writer even when no join happens.
    ///
    /// The early-failure paths return without joining; without this
    /// the writer would wait on its condvar forever. Idempotent with
    /// [`TranscriptWorker::join`], which takes the handle first and
    /// leaves this nothing to do.
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::driver::DriverContext;
    use crate::tui::driver::RunOverrides;
    use crate::tui::driver::run_driver_loop;
    use crate::tui::permissions_bridge::InteractiveLanes;
    use crate::tui::test_support::*;
    use dch_tui::SessionCommand;
    use dch_tui::Submission;
    use dch_tui::TuiMessage;
    use dch_tui::TuiObserverState;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[test]
    fn an_initial_snapshot_persists_a_loadable_session_file() {
        // The exit line's promise holds from the first frame: the
        // initial snapshot writes the session file even when no turn
        // ever ends, so the printed command resumes something real.
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        handle.send(Vec::new(), crate::session::SessionTokens::default());
        worker.join();
        let messages = saved_messages(dir.path(), id);
        assert!(
            messages.is_empty(),
            "the pre-first-frame snapshot loads back as the empty session"
        );
    }

    #[test]
    fn an_initial_snapshot_preserves_restored_token_totals() {
        // A resume seeds all three counters before the first frame; an
        // exit with no new turn must keep the triple in the file.
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        let state = TuiObserverState::new();
        {
            let mut tokens = state.tokens.lock().expect("the tokens lock");
            tokens.cumulative_input = 9_999;
            tokens.cumulative_output = 111;
            tokens.input = 777;
        }
        handle.send(Vec::new(), snapshot_tokens(&state.tokens));
        worker.join();
        let tokens = saved_tokens(dir.path(), id);
        assert_eq!(
            tokens.cumulative_input, 9_999,
            "the restored input total survives a turnless exit"
        );
        assert_eq!(
            tokens.cumulative_output, 111,
            "the restored output total survives a turnless exit"
        );
        assert_eq!(
            tokens.last_input_tokens, 777,
            "the restored last-turn figure survives a turnless exit"
        );
    }

    #[test]
    fn an_older_snapshot_never_overwrites_a_newer_one() {
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        let now = chrono::Utc::now();
        let bulky = vec![
            TuiMessage::User {
                text: "x".repeat(20_000),
                timestamp: now,
                steered: false,
                unsent: false,
            };
            100
        ];
        let final_turn = vec![TuiMessage::User {
            text: "final turn".to_string(),
            timestamp: now,
            steered: false,
            unsent: false,
        }];
        handle.send(bulky, crate::session::SessionTokens::default());
        handle.send(final_turn, crate::session::SessionTokens::default());
        drop(handle);
        worker.join();
        let messages = saved_messages(dir.path(), id);
        assert_eq!(
            messages.len(),
            1,
            "the transcript is the newest snapshot, however the queue interleaved"
        );
        assert!(
            matches!(&messages.first().expect("the one message"), TuiMessage::User { text, .. } if text == "final turn"),
            "a slow older snapshot must not rename over a completed newer one"
        );
    }

    #[test]
    fn joining_the_writer_flushes_the_final_snapshot() {
        let (saver, dir, id) = worker_saver();
        let (handle, worker) = TranscriptWorker::spawn(saver);
        handle.send(
            vec![TuiMessage::User {
                text: "the last turn".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        drop(handle);
        worker.join();
        let messages = saved_messages(dir.path(), id);
        assert_eq!(
            messages.len(),
            1,
            "join returns only after the final write is on disk"
        );
        assert!(
            matches!(&messages.first().expect("the one message"), TuiMessage::User { text, .. } if text == "the last turn"),
            "the persisted transcript is the joined snapshot"
        );
    }

    #[test]
    fn a_shutdown_notification_is_never_lost_between_check_and_wait() {
        let (saver, dir, id) = worker_saver();
        let gate = Arc::new(Gate::closed());
        let hold = Arc::clone(&gate);
        let (_handle, worker) = TranscriptWorker::spawn_gated(saver, Arc::new(move || hold.hold()));

        // Park the writer in the exact state the race lives in:
        // predicate checked false, slot lock still held, not yet
        // waiting on the writer's own signal.
        gate.wait_parked();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let joiner = std::thread::spawn(move || {
            worker.join();
            // A failed send means the test already gave up on this
            // joiner.
            let _sent = done_tx.send(());
        });
        // Let join meet the held lock (or, on the unfixed shape,
        // fire its notification into the vacant window).
        std::thread::sleep(std::time::Duration::from_millis(100));
        gate.release();

        if done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            panic!(
                "join hung: the shutdown notification fired while the \
                 writer sat between its predicate check and its wait, and \
                 was lost — the flag must be stored under the slot lock"
            );
        }
        drop(joiner.join());
        drop((dir, id));
    }

    #[test]
    fn a_dropped_writer_takes_no_late_publish() {
        let (saver, dir, id) = worker_saver();
        let gate = Arc::new(Gate::closed());
        let hold = Arc::clone(&gate);
        let (handle, worker) = TranscriptWorker::spawn_gated(saver, Arc::new(move || hold.hold()));
        gate.wait_parked();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let dropper = std::thread::spawn(move || {
            // Dropping, not joining — the early-failure path's shape.
            // With the shutdown store under the lock, this blocks
            // until the gate opens, then parks the writer off.
            drop(worker);
            let _sent = done_tx.send(());
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        gate.release();
        if done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            panic!("drop hung: an unjoined worker must still be parked off");
        }
        drop(dropper.join());

        // The writer is gone: a publish after the drop has no
        // consumer and must never reach the disk.
        handle.send(
            vec![TuiMessage::User {
                text: "late".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        let path = dir.path().join(id.to_string()).join("session.json");
        assert!(
            !path.exists(),
            "a dropped worker must not leave a writer behind to consume a late publish"
        );
    }

    #[test]
    fn a_published_snapshot_keeps_the_destination_it_was_published_with() {
        // A save published before a rotation keeps the destination it was
        // published under: the pin rides in the snapshot itself, so a
        // mid-flight swap of the saver cell cannot redirect it. Driven
        // without a writer so the pending slot — where the pin lives —
        // is inspectable exactly across the rotation boundary.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let first = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            first,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let handle = TranscriptHandle {
            pending: Arc::new(std::sync::Mutex::new(PendingSaves::default())),
            signal: Arc::new(std::sync::Condvar::new()),
            saver: Arc::new(std::sync::Mutex::new(saver)),
            naming_in_flight: Arc::new(std::sync::Mutex::new(None)),
        };
        handle.send(
            vec![dch_tui::TuiMessage::User {
                text: "the ended session's last turn".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        let rotated = handle.rotate("m");
        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pinned = pending
            .display
            .first()
            .expect("the published snapshot waits in the slot")
            .saver
            .session_id();
        drop(pending);
        assert_eq!(
            pinned, first,
            "the waiting save keeps the destination it was published under"
        );
        let cell = handle
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            cell.session_id().to_string(),
            rotated,
            "the cell itself moved to the fresh identity"
        );
    }

    #[test]
    fn a_rotation_preserves_the_outgoing_sessions_pending_snapshots() {
        // The stall the one-slot mailbox lost: the ended session's
        // last publish is still waiting when the rotation swaps the
        // identity, and the fresh session's first publish arrives
        // behind it. Both snapshots wait — each pinned to its own
        // destination — so the writer drains the ended session's
        // final turn and the fresh session's opening, not only the
        // fresh one.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let first = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            first,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let handle = TranscriptHandle {
            pending: Arc::new(std::sync::Mutex::new(PendingSaves::default())),
            signal: Arc::new(std::sync::Condvar::new()),
            saver: Arc::new(std::sync::Mutex::new(saver)),
            naming_in_flight: Arc::new(std::sync::Mutex::new(None)),
        };
        handle.send(
            vec![dch_tui::TuiMessage::User {
                text: "the ended session's last turn".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        handle.send_machine(MachineSnapshot {
            messages: Arc::new(vec![loopctl::message::Message::user(
                "the engine's history",
            )]),
            saved_at: chrono::Utc::now(),
            compactions: 0,
            saver: None,
        });
        let rotated = handle.rotate("m");
        handle.send_machine(MachineSnapshot {
            messages: Arc::new(vec![loopctl::message::Message::user(
                "the fresh engine's history",
            )]),
            saved_at: chrono::Utc::now(),
            compactions: 0,
            saver: None,
        });
        handle.send(
            vec![dch_tui::TuiMessage::User {
                text: "the fresh session's opening".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        let rotated_id = uuid::Uuid::parse_str(&rotated).expect("the rotation mints an id");
        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let display_ids: Vec<uuid::Uuid> = pending
            .display
            .iter()
            .map(|waiting| waiting.saver.session_id())
            .collect();
        let machine_ids: Vec<uuid::Uuid> = pending
            .machine
            .iter()
            .map(|waiting| {
                waiting
                    .saver
                    .as_ref()
                    .expect("the publish pinned its saver")
                    .session_id()
            })
            .collect();
        drop(pending);
        assert_eq!(
            display_ids,
            vec![first, rotated_id],
            "both sessions' snapshots wait — the outgoing one is not replaced"
        );
        assert_eq!(
            machine_ids,
            vec![first, rotated_id],
            "the machine half queues per identity the same way"
        );
    }

    #[test]
    fn the_resume_reads_the_newest_queued_snapshot_for_its_identity() {
        // The live resume's read path: the newest queued halves of the
        // identity it adopts — display, totals, and the machine
        // history with its compaction count — cloned out while the
        // queue itself stays intact, so the writer still persists what
        // it holds and no other session's snapshot is mistaken for
        // this one.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let first = uuid::Uuid::new_v4();
        let saver = Arc::new(
            crate::session::SessionSaver::with_base_dir(
                first,
                "m".to_string(),
                dir.path().to_path_buf(),
            )
            .with_title(Some("the recorded title".to_string())),
        );
        let handle = TranscriptHandle {
            pending: Arc::new(std::sync::Mutex::new(PendingSaves::default())),
            signal: Arc::new(std::sync::Condvar::new()),
            saver: Arc::new(std::sync::Mutex::new(saver)),
            naming_in_flight: Arc::new(std::sync::Mutex::new(None)),
        };
        handle.send(
            vec![dch_tui::TuiMessage::User {
                text: "the ended session's last turn".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        handle.send_machine(MachineSnapshot {
            messages: Arc::new(vec![loopctl::message::Message::user(
                "the engine's history",
            )]),
            saved_at: chrono::Utc::now(),
            compactions: 3,
            saver: None,
        });
        let rotated = handle.rotate("m");
        let rotated_id = uuid::Uuid::parse_str(&rotated).expect("the rotation mints an id");
        handle.send_machine(MachineSnapshot {
            messages: Arc::new(vec![loopctl::message::Message::user(
                "the fresh engine's history",
            )]),
            saved_at: chrono::Utc::now(),
            compactions: 1,
            saver: None,
        });
        handle.send(
            vec![dch_tui::TuiMessage::User {
                text: "the fresh session's opening".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );

        let ended = handle
            .latest_queued_for(first)
            .expect("the ended session's snapshot waits");
        assert!(
            format!("{:?}", ended.messages).contains("the ended session's last turn"),
            "the read reaches the newest publish of the identity asked for"
        );
        assert_eq!(
            ended.title.as_deref(),
            Some("the recorded title"),
            "the pinned saver's recorded name rides with the queued state"
        );
        assert!(
            matches!(&ended.machine, Some((history, 3)) if format!("{history:?}").contains("the engine's history")),
            "the machine half rides with its own compaction count"
        );
        let fresh = handle
            .latest_queued_for(rotated_id)
            .expect("the fresh session's snapshot waits");
        assert!(
            format!("{:?}", fresh.messages).contains("the fresh session's opening"),
            "the fresh identity reads its own opening, not the ended one"
        );
        assert!(
            fresh.machine.is_none(),
            "a machine half older than the display it would pair with is judged \
             stale — the resume's derive walk covers the difference"
        );
        assert!(
            handle.latest_queued_for(uuid::Uuid::new_v4()).is_none(),
            "a stranger's identity reads nothing"
        );

        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            pending.display.len(),
            2,
            "the read clones — the queue stays intact for the writer"
        );
        drop(pending);

        // The writer's take: queue entries move to the in-flight half
        // and the resume read follows them there — a write still
        // running hides nothing from the identity it belongs to.
        {
            let mut pending = handle
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let drained_display = std::mem::take(&mut pending.display);
            let drained_machine = std::mem::take(&mut pending.machine);
            pending.in_flight_display.extend(drained_display);
            pending.in_flight_machine.extend(drained_machine);
        }
        let in_flight = handle
            .latest_queued_for(first)
            .expect("the taken snapshot stays visible while its write runs");
        assert!(
            format!("{:?}", in_flight.messages).contains("the ended session's last turn"),
            "the in-flight half answers the identity's read"
        );
        assert!(
            handle.latest_queued_for(uuid::Uuid::new_v4()).is_none(),
            "a stranger's identity still reads nothing from the in-flight half"
        );
    }

    #[test]
    fn a_rotated_writer_lands_each_save_in_its_own_file() {
        // The write half, sequenced through the real writer: a save the
        // rotation overtakes lands in the ended session's file, and the
        // first save published after the swap opens the fresh one.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let first = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            first,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let now = chrono::Utc::now();
        handle.send(
            vec![dch_tui::TuiMessage::User {
                text: "the ended session's last turn".to_string(),
                timestamp: now,
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        let first_path = dir.path().join(first.to_string()).join("session.json");
        let written = std::time::Instant::now();
        while !first_path.exists()
            && std::time::Instant::now().duration_since(written) < std::time::Duration::from_secs(2)
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            first_path.exists(),
            "the pre-rotation save reaches the ended session's file"
        );
        let rotated = handle.rotate("m");
        handle.send(
            vec![dch_tui::TuiMessage::User {
                text: "the fresh session's first turn".to_string(),
                timestamp: now,
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        worker.join();
        let first_save = std::fs::read_to_string(&first_path)
            .expect("the ended session's file stands with its last turn");
        assert!(
            first_save.contains("the ended session's last turn"),
            "got {first_save}"
        );
        let rotated_save = std::fs::read_to_string(dir.path().join(rotated).join("session.json"))
            .expect("the post-rotation save lands in the fresh session's file");
        assert!(
            rotated_save.contains("the fresh session's first turn"),
            "got {rotated_save}"
        );
    }

    #[test]
    fn a_naming_slot_armed_for_one_session_does_not_disarm_another() {
        // The naming slot's compare-and-set, driven worker-free:
        // identity is the only thing that may arm, apply, or clear
        // it. A retired session's settle must leave the live
        // session's naming armed, and its late answer must not title
        // the session that replaced it.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let first = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            first,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, _worker) = TranscriptWorker::spawn(saver);
        assert!(
            handle.arm_naming(first),
            "the first session's claim arms the slot"
        );
        assert!(
            !handle.arm_naming(first),
            "a second prompt in the same session buys no second call"
        );
        handle.rotate("m");
        let fresh = handle.current_session_id();
        assert_ne!(first, fresh, "the rotation moved the save target");
        assert!(
            handle.arm_naming(fresh),
            "the fresh session's first prompt arms its own call — a slot \
             held for the retired session does not stand it down"
        );
        handle.settle_naming(first);
        assert_eq!(
            *handle.naming_in_flight.lock().expect("naming slot"),
            Some(fresh),
            "the stale session's settle leaves the live session's naming armed"
        );
        handle.set_title_for(first, "a stale subject".to_string());
        assert!(
            !handle.has_title(),
            "a stale answer cannot title the session that replaced its own"
        );
        handle.set_title_for(fresh, "the fresh subject".to_string());
        assert!(
            handle.has_title(),
            "the live session's own answer titles it"
        );
        handle.settle_naming(fresh);
        assert_eq!(
            *handle.naming_in_flight.lock().expect("naming slot"),
            None,
            "settling the live session's call clears the slot for a retry"
        );
    }

    #[tokio::test]
    async fn a_late_naming_answer_titles_the_session_whose_prompt_coined_it() {
        // The first session's name is held past the turn it rode
        // beside, and the host rotates onto a fresh session before it
        // lands. The stale answer must strand itself: the fresh
        // session files a title its own prompt coined — proving its
        // first prompt bought a naming call at all — while the
        // retired session's file keeps the shape it saved.
        let server = SseServer::start_with_naming_delay(
            vec![
                (sse_text_turn("first answer"), std::time::Duration::ZERO),
                (
                    sse_text_turn("second answer"),
                    std::time::Duration::from_millis(2400),
                ),
            ],
            std::time::Duration::from_millis(1500),
        )
        .await;
        let dir = tempfile::tempdir().expect("workdir tempdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let root = sessions.keep();
        let first = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            first,
            "test-model".to_string(),
            root.clone(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(saver);
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "open the first conversation");
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            rx,
            DriverContext {
                config: config.clone(),
                config_base: std::sync::Arc::new(config),
                workdir: dir.path().to_path_buf(),
                sessions_root: root.clone(),
                state: state.clone(),
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            transcript.clone(),
            session_tx,
        )));
        let mut first_done = false;
        for _ in 0..1800 {
            if state
                .machine_history
                .lock()
                .expect("history lock")
                .is_some()
            {
                first_done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            first_done,
            "the first turn completes while its session's name is still held back"
        );
        transcript.send(
            vec![TuiMessage::User {
                text: "open the first conversation".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        tx.send(Submission::Command {
            command: SessionCommand::Clear,
        })
        .expect("driver channel open");
        let mut fresh = first;
        let mut rotated = false;
        for _ in 0..1800 {
            let current = transcript.current_session_id();
            if current != first {
                fresh = current;
                rotated = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            rotated,
            "the clear moves the save target onto a fresh session"
        );
        queue_prompt(&state, &tx, "open the second conversation");
        drop(tx);
        let machine_path = root.join(fresh.to_string()).join("machine.json");
        let mut second_done = false;
        for _ in 0..1800 {
            if machine_path.exists() && transcript.has_title() {
                second_done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            second_done,
            "the fresh session's turn completed and its own naming answer landed \
             on the cell — the publish that follows files whichever title the \
             cell holds"
        );
        transcript.send(
            vec![TuiMessage::User {
                text: "open the second conversation".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        let _ = driver.await;
        worker.join();
        let retired =
            crate::session::load_with_meta_in(first, &root).expect("the retired session's file");
        assert_eq!(
            retired.title, None,
            "the retired session's file keeps the shape it saved — a stale naming \
             answer never reaches it"
        );
        let fresh_file =
            crate::session::load_with_meta_in(fresh, &root).expect("the fresh session's file");
        assert_eq!(
            fresh_file.title.as_deref(),
            Some("Canned Session Title 2"),
            "the fresh session files the title its own prompt coined — the retired \
             conversation's words never land on it, and its first prompt always \
             buys a naming call"
        );
    }
}
