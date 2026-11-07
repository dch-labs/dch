//! The prompts typed while a turn runs, held until theirs.
//!
//! A submit that lands mid-turn is not a message to that turn: it is the
//! next turn's prompt, waiting. This module owns the waiting set — one
//! store the display and the driver both reach through the same lock, so
//! the text the panel shows and the text the next turn sends have one
//! source. Two copies of a prompt is how a queue ends up lying about
//! itself: the panel would read out what the keystrokes said while the
//! send carried what had been handed over at submit time, and an edit
//! confirmed on screen would change nothing that ever reached the model.
//!
//! Prompts are addressed by id rather than by position. A waiting list
//! empties from the front as prompts are sent and from anywhere as the
//! user drops them, so an index names a different prompt a moment later;
//! an id names the same one until it leaves. That is what lets an edit be
//! confirmed against a prompt the user watched leave the queue without
//! the text landing on its neighbour instead, and it is why the driver's
//! claim and the panel's drop can both arrive late and still be right
//! about which prompt they mean.

use std::collections::VecDeque;

/// One prompt waiting its turn.
///
/// Just the text and what it is called. There is no timestamp and no
/// status flag: a prompt is waiting until it is sent, edited, or dropped,
/// and all three are the queue's own operations rather than a state this
/// type carries. A `sent` bit in particular would mean a prompt that is
/// gone from the waiting set and still in it — [`PromptQueue::remove`]
/// takes the entry with it, which is the only way a prompt can stop
/// waiting.
#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    /// This prompt's address, issued at [`PromptQueue::push`].
    ///
    /// The stable half of the identity: an index into the waiting order
    /// shifts every time a prompt ahead leaves, and this does not. It is
    /// never reused within a queue, so an id either names its own prompt
    /// or names one that has gone — never a different prompt.
    pub id: u64,

    /// The text as it stands, which is what the next turn will send.
    ///
    /// Current, not as-submitted: [`PromptQueue::replace`] rewrites it in
    /// place, so the model reads the words the user settled on rather than
    /// the ones they typed first and meant to fix. Nothing else owns a
    /// copy of it — the panel renders this field and the driver sends it.
    pub text: String,
}

/// The prompts waiting to be sent, in the order they were submitted.
///
/// The whole queue's state is these two fields, and the display's
/// selection over it lives on the app rather than here: what a prompt *is*
/// is shared with the driver, while where the user stands among them is the
/// terminal's own business. That split is why an entry
/// can leave from under the cursor — claimed by the driver between one
/// frame and the next — without either half holding a stale copy of the
/// other.
///
/// [`Default`] is the empty queue, the state every session starts in.
/// The type has no failure mode: every operation is total, so there is no
/// error to carry out of it. A lookup for a prompt that is no longer
/// there answers with nothing, which is the honest reading of a queue a
/// third party drains underneath you rather than a condition to raise.
#[derive(Debug, Clone, Default)]
pub struct PromptQueue {
    /// The waiting prompts, oldest first.
    ///
    /// Front is the next to send, so the driver's claim and the order the
    /// panel lists share one orientation and never disagree about which
    /// prompt comes first. A [`VecDeque`] rather than a `Vec` because
    /// both ends are hot: prompts leave from the front as turns end and
    /// from anywhere as the user drops them.
    entries: VecDeque<QueuedPrompt>,

    /// The id the next pushed prompt takes.
    ///
    /// Increases by one per prompt and stops at the maximum rather than
    /// wrapping, so no two prompts a session can actually hold share an
    /// id and a late lookup can never be pointed at a prompt that arrived
    /// after the lookup was made.
    next_id: u64,
}

impl PromptQueue {
    /// Add `text` to the back of the waiting order and name it.
    ///
    /// Submission order is send order: a prompt joins behind every prompt
    /// already waiting, whatever the terminal has selected or is in the
    /// middle of editing. The returned id is how this prompt is addressed
    /// from here on — the driver receives it rather than the text, so the
    /// text that gets sent is whatever this entry holds at the moment the
    /// turn claims it.
    pub fn push(&mut self, text: String) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push_back(QueuedPrompt { id, text });
        id
    }

    /// Take the prompt `id` out and return the text it would have sent.
    ///
    /// This is the one removal, and both directions of it need the same
    /// thing: the driver taking its next prompt and the user dropping one
    /// both end with the prompt gone and no longer sendable. Nothing
    /// distinguishes the two afterwards, which is correct — a dropped
    /// prompt is simply one that never runs, and a sent one is a prompt
    /// that left for the turn instead.
    ///
    /// `None` when no prompt holds that id. That is the ordinary outcome
    /// for a prompt already claimed while its id sat in the channel, and
    /// for one the user dropped twice; answering `None` is what makes both
    /// safe to act on rather than a reason to hold a lock across a turn.
    pub fn remove(&mut self, id: u64) -> Option<String> {
        let position = self.index_of(id)?;
        Some(self.entries.remove(position)?.text)
    }

    /// Rewrite the prompt `id` holds to `text`, keeping its place.
    ///
    /// The edit path's whole job, and it is worth being clear about what
    /// it changes: the text the next turn sends, and nothing else. The id
    /// stands so an id already handed to the driver still finds this
    /// prompt, and the position stands so walking the queue does not jump
    /// under the keys because a word was fixed. Whether the new text is
    /// acceptable to send is the caller's question — an empty edit is
    /// refused before it gets here, not stored here for the turn to choke
    /// on.
    ///
    /// `false` when no prompt holds that id, which is how a confirm
    /// landing on a prompt the driver claimed a moment ago reports itself
    /// instead of editing a neighbour.
    pub fn replace(&mut self, id: u64, text: String) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.text = text;
        true
    }

    /// The prompt at position `index` in the waiting order.
    ///
    /// Position is a display coordinate — the panel counts rows, and so
    /// does the user's cursor — while every operation addresses by id.
    /// This is where one converts to the other, and nothing caches the
    /// answer: a row is re-read every frame because a prompt ahead of it
    /// can leave at any moment and move everything up.
    ///
    /// [`None`] past the end, including every index into an empty queue,
    /// so a cursor standing over a queue that just drained renders nothing
    /// rather than reaching past its last row.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&QueuedPrompt> {
        self.entries.get(index)
    }

    /// The waiting-order position of the prompt `id`, if it is still here.
    ///
    /// A search rather than a stored position, because a position is not
    /// stable enough to hold: prompts leave from the front as turns claim
    /// them and from the middle as the user drops them, so anything
    /// remembered across a turn would be a stale index waiting to point
    /// at the wrong text. The scan is over what a user queues by hand —
    /// a handful of prompts, not a buffer — and it answers only the
    /// display's question of where to draw a row.
    #[must_use]
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }

    /// How many prompts wait.
    ///
    /// Entries in send order, counting a prompt whose body is out in the
    /// composer: lifting one out to be edited leaves it waiting, so only a
    /// send or a drop changes the count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing waits.
    ///
    /// The panel's row is held for the queue only while this is false, so
    /// an ordinary session's layout does not spend a row on a queue no
    /// prompt was ever typed into.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop every waiting prompt, keeping nothing for a turn to claim.
    ///
    /// `/clear` empties the session between turns: a prompt typed during a
    /// run and still waiting belongs to the conversation that just ended,
    /// so it must not surface as the fresh session's first turn. Ids are not
    /// reused, so a claim for a prompt that had already been named still
    /// resolves to nothing rather than to one that survived.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// What one submit asks the driver to do.
///
/// The panel and the engine's driver are separate tasks, and this is the
/// whole protocol between them: the display never holds the loop, so a
/// press of Enter is a request rather than a call. Three requests exist
/// because the same act can mean three things — send what is waiting,
/// replace what already went out, or carry out a session command between
/// turns.
///
/// The queue's own ids ride this boundary on purpose. A prompt still
/// waiting is *named*, not quoted: the driver reads its text out of the
/// shared queue at the moment the turn starts, so an edit saved or a
/// prompt dropped after this message was sent is what takes effect, and
/// the display never has to retract a message already in flight.
#[derive(Debug, PartialEq)]
pub enum Submission {
    /// Run the waiting prompt `id` when its turn comes.
    ///
    /// `id` may name a prompt that no longer waits by the time the driver
    /// reads this — dropped from the panel, or spent by a run that
    /// already claimed it. A cancel stops only the turn in flight and
    /// never takes the backlog with it. A name the driver cannot
    /// resolve is nothing to do, which is also the honest reading.
    Run {
        /// The waiting entry to send.
        ///
        /// An id rather than the text, resolved against the queue when the
        /// driver takes this message up: whatever the entry holds at that
        /// moment is what runs, so an edit saved in the meantime is honoured
        /// without the display having to withdraw anything.
        id: u64,
    },

    /// Run `text` in place of the prompt `id` that already went out.
    ///
    /// The correction the panel's editor makes possible: the user saved an
    /// edit into an entry the run had claimed between the two keys, so the
    /// wording they settled on has to go out *instead of* the turn already
    /// running or already finished — and the model must not carry both.
    /// The driver therefore cuts the superseded turn out of the committed
    /// history before sending, which it can only do for the turn it most
    /// recently claimed: that is the one no later answer has been built
    /// on. Where the cut is not available the correction still goes out,
    /// as an ordinary turn rather than not at all — a silently lost
    /// correction is worse than a redundant one.
    ///
    /// The turn this names is stopped rather than waited out: the display
    /// sends this first and raises the runner's cancel beside it, so an
    /// in-flight superseded turn resolves cancelled and the cut lands on
    /// whatever it had committed by then. The driver clears the signal
    /// before the corrected turn starts, and a cancel raised this way
    /// reports no error of its own — the correction is the next story.
    Correct {
        /// The entry the edit was made from, matched by the driver against
        /// the turn it last claimed.
        ///
        /// The match is what makes cutting that turn safe: it is the newest
        /// claim, so no later answer has been built on it. An id the driver
        /// cannot match sends the correction as an ordinary turn instead.
        id: u64,

        /// The corrected text, sent in the superseded prompt's place.
        ///
        /// Carried whole rather than named, because the wording it replaces
        /// has left the queue: the entry that held it is the run's now, so
        /// this is the only surviving copy of what the user settled on.
        text: String,
    },

    /// Carry out `command` against the loop the driver owns, at rest.
    ///
    /// The session lane of the composer's command language: the acts that
    /// reach the loop (`/clear`, `/model`) ride this rather than a second
    /// channel, because the driver is the only holder of the runner. A
    /// command is a between-turns act, so the display raises it only when
    /// no run is in flight; the running check there and the take-up here
    /// mean the two halves cannot disagree mid-run.
    Command {
        /// The session command to carry out on the driver's loop.
        ///
        /// Named rather than executed inline because the loop lives on the
        /// driver task; the display settles its own half where it can — a
        /// clear empties the transcript on the spot — and what only the
        /// driver can produce comes back over the session channel.
        command: SessionCommand,
    },
}

/// A composer command that needs the loop the driver owns.
///
/// Only the commands whose effect is on the agent loop appear here. The
/// purely presentational ones (`/help`, `/theme`) and the unknown-command
/// notice are carried out by the display itself and never cross this
/// boundary, so this type names just the acts the driver has to perform on
/// the runner it holds — and takes up only at rest, since a command is not
/// a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    /// Rewind the committed history to empty and rotate the save target.
    ///
    /// The display has already emptied the transcript, the queue, and the
    /// context display on its own half when it sent this; the driver's part
    /// is to rewind the live loop's committed history to nothing — the
    /// same runner, client, and machine state stand — and rotate the
    /// save target to a fresh session id, so the ended conversation's
    /// file stands whole and the next turn starts a new one under a new
    /// identity. A rewind the loop refuses lands as a notice and leaves
    /// everything — history, identity — exactly as it stood.
    Clear,

    /// Switch the live loop's model to `name`, rebuilding on its entry.
    ///
    /// Carries the name the user resolved, so the driver selects its
    /// `[[models]]` entry onto the user-stated base — every field the
    /// entry states, the context window above all, belongs to the
    /// engine a rebuild installs — and rebuilds the runner on it with
    /// the conversation carried; a cross-provider entry rides
    /// [`SessionCommand::SwitchProvider`] instead, which the display
    /// routes it to before this lane is ever touched.
    Model {
        /// The model name to switch the live loop to.
        ///
        /// The configured entry's own spelling; the loop's rebuild is
        /// built around it and the verdict publishes it back.
        name: String,
    },

    /// Rebuild the runner on a different provider, carrying the
    /// conversation.
    ///
    /// A cross-provider model switch cannot rename the live client —
    /// the wire shape differs — so the driver builds a fresh runner on
    /// the profile's connection settings and seeds it with the
    /// committed history the old runner holds. What the rebuild
    /// installed travels back in its own update, and the display's
    /// configuration and the file take it then — the loop's verdict,
    /// never the send, is what they record.
    SwitchProvider {
        /// The model name on the new provider.
        ///
        /// Carried whole because the rebuild names the model it builds
        /// the new client around.
        model: String,

        /// The base URL the new provider listens on.
        ///
        /// The endpoint the rebuilt runner's client sends every request
        /// to.
        base_url: String,

        /// The API type the new provider speaks, spelled as the
        /// config's `snake_case` name.
        ///
        /// The wire shape the rebuild builds the client for; an empty
        /// spelling inherits the protocol already running.
        api_type: String,
    },

    /// Swap the live runner to a saved session's restored history.
    ///
    /// The driver alone can load: it parses the id, reads the saved
    /// pair, and rebuilds the runner on the restored machine history —
    /// the same shape a launch-time resume builds — so the next
    /// submission continues the conversation the saved session left.
    /// What the display cannot do for itself comes back over the
    /// session channel: the loaded transcript, the restored token
    /// accounting, and the id whose file later saves write.
    Resume {
        /// The saved session's id, as the display resolved it.
        ///
        /// The whole address of the load: the driver parses it before it
        /// reads anything.
        id: String,
    },

    /// Run one compaction pass over the committed history, now.
    ///
    /// The manual `/compact` seam: the engine's own context manager
    /// folds the history between runs, the next turn continues from
    /// the compacted context, and the notice carries the token
    /// figures the pass measured. The driver publishes the folded
    /// history to the machine snapshot beside the notice, and the
    /// payload is the display transcript's Compacted-row count at the
    /// moment of the command — the one number the driver cannot see
    /// for itself, so the snapshot it publishes records a truthful
    /// total (the rows the display carries plus this pass) without
    /// holding the display.
    Compact {
        /// How many Compacted rows the display carries, for the
        /// machine snapshot this pass publishes.
        compactions: u32,
    },

    /// List the saved sessions under the session root, for a picker.
    ///
    /// The `/sessions` seam: only the driver holds the sessions root,
    /// so the display asks and the catalog comes back as a
    /// [`SessionUpdate::SessionCatalog`] — newest first, exactly what
    /// the listing reads — for the picker sheet to walk.
    ListSessions,
}

/// One saved session as a picker row sees it.
///
/// The display-facing half of a saved-session summary: the id a
/// `--resume` takes, the model the session ran, when its last save
/// landed, and how many prompts the user submitted. The driver reads
/// the sessions root and maps its summaries into this shape; the
/// panel renders and walks it without touching the filesystem —
/// the crate boundary keeps the root on the host's side of it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPick {
    /// The session identity, as `dch --resume <id>` takes it.
    ///
    /// The canonical spelling of the uuid naming the session's
    /// directory — the same address the picker's Enter hands to the
    /// resume lane.
    pub id: String,

    /// The model the session ran.
    ///
    /// The envelope's own record, so a row tells which model the
    /// conversation belongs to before it is reopened.
    pub model: String,

    /// When the session's last save landed.
    ///
    /// Newer rows sort first; the picker shows the moment so recent
    /// conversations stand out from stale ones.
    pub saved_at: chrono::DateTime<chrono::Utc>,

    /// How many prompts the user submitted over the session.
    ///
    /// The summary's own count — submissions only, not tool calls or
    /// echo rows — one glance at how long the conversation runs.
    pub prompts: usize,

    /// The session's name, when the naming call has answered.
    ///
    /// The few words that say what the conversation was about — the
    /// thing a user actually picks a resume by, which no id or
    /// timestamp can say. `None` on sessions saved before titles
    /// existed and on ones whose naming call never answered; the
    /// picker's row renders the absence as a dash.
    pub title: Option<String>,

    /// The working directory the session ran in, when recorded.
    ///
    /// The picker's row shows it so sessions on the same model in
    /// different repos read differently at a glance; `None` on
    /// sessions saved before the field existed.
    pub workdir: Option<String>,
}

/// A between-turns fact the driver hands back to the display.
///
/// The reverse direction of [`Submission`]: the driver alone holds the
/// runner, so when it acts on a [`SessionCommand`] the loop's identity
/// changes there and only the driver can say so. Rather than a second
/// protocol, this small enum rides a dedicated channel from driver to panel
/// and names what the display cannot compute for itself — which session
/// you are now in, which model the loop will carry next, the transcript a
/// resumed session restored — plus one shape for everything a command's
/// outcome leaves to say: a plain line of text for a refusal, a failure,
/// or a result. Everything else the display reads from the shared
/// observer state; these are the values that live only on the driver task.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    /// The session's id is now `id`; the status bar shows it on its next draw.
    ///
    /// Posted after a `/clear` rotates the save target to a fresh
    /// identity, so the id the user reads is the one the ended
    /// conversation was retired behind and the next save is written
    /// under. The loop itself was rewound, not swapped: the same runner
    /// keeps running on an emptied history.
    SessionId {
        /// The new session id, rendered as the status bar shows it.
        ///
        /// Names the conversation every later save writes.
        id: String,
    },

    /// The live loop's model is now `name`; the status bar shows it.
    ///
    /// Posted after a `/model` switch lands, so the bar and the next
    /// request's body agree on the model without the panel having to guess
    /// whether the driver's apply succeeded. The display carries the name
    /// into the running configuration and saves it, so the file describes
    /// the loop that accepted the switch.
    ModelName {
        /// The model name now carried by the live loop.
        ///
        /// The loop's own report of the switch it accepted — the display
        /// records and saves it, never assumes it.
        name: String,
    },

    /// A cross-provider switch landed: the loop now runs `model` on a new
    /// endpoint.
    ///
    /// Posted after the rebuilt runner takes over, carrying the
    /// connection settings that rebuild installed — and behind them
    /// the whole entry the rebuild selected: the display applies the
    /// wire fields and re-resolves the entry over the user-stated base,
    /// so its configuration mirrors what the engine runs, field for
    /// field, and saves the same write-through a same-provider switch
    /// performs.
    Provider {
        /// The model name now carried by the live loop.
        ///
        /// The rebuild's own report of the model its client was built
        /// around.
        model: String,

        /// The base URL the new provider listens on.
        ///
        /// The endpoint the rebuilt runner's client sends every request
        /// to.
        base_url: String,

        /// The API type the new provider speaks, spelled as the config's
        /// `snake_case` name.
        ///
        /// The wire shape the rebuild builds the client for; an empty
        /// spelling inherits the protocol already running.
        api_type: String,
    },

    /// A saved session was adopted; the display re-seeds from what this
    /// carries.
    ///
    /// The display half of a live resume: the driver rebuilt the loop on
    /// the saved machine history and moved the save target onto the
    /// session's own file, and this brings back what the screen shows —
    /// the loaded transcript, the session's accounting, and the id now
    /// being written.
    Resumed {
        /// The adopted session's id — the file later saves write.
        ///
        /// The identity the save target moved onto; the status bar shows
        /// it as the session in progress.
        id: String,

        /// The loaded display transcript, verbatim as saved.
        ///
        /// Replaces what the screen shows in one move, the same seeding
        /// a launch resume performs.
        transcript: Vec<crate::message::TuiMessage>,

        /// The session's cumulative token accounting at save time.
        ///
        /// Restored into the running counters so the resumed session
        /// continues the saved one's story.
        tokens: ResumedTokens,
    },

    /// A line the driver could not keep to itself: a refusal, a failure, or
    /// a result.
    ///
    /// The notice row's text, already user-facing prose: why a switch was
    /// declined, why a resume or rebuild could not happen, or the figures
    /// a manual compaction measured. The display posts it as-is; the
    /// driver has no other way to be heard.
    Notice {
        /// The text to post on the notice row.
        ///
        /// Already user-facing prose from the driver; the display posts
        /// it as-is.
        text: String,
    },

    /// The saved sessions under the root, newest first, for a picker.
    ///
    /// The answer to a [`SessionCommand::ListSessions`]: the catalog a
    /// `/sessions` picker walks. An empty catalog is itself the
    /// answer — nothing saved yet — and the display says so rather
    /// than opening an empty sheet.
    SessionCatalog {
        /// The saved sessions, newest first.
        ///
        /// The listing's own order: the picker highlights the first
        /// row — the most recent conversation — on open.
        rows: Vec<SessionPick>,
    },
}

/// The token accounting a resumed session restores, as the driver read it
/// from the saved envelope.
///
/// Plain numbers rather than the host's own accounting type: the panel
/// writes them into its running counters so the resumed session continues
/// the saved one's story instead of restarting at zero, and the channel
/// that carries them crosses into this crate, which owns no session-file
/// types of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumedTokens {
    /// Input tokens since the resumed session began.
    ///
    /// The spend figure the status bar's running total shows, continued
    /// from the save rather than restarted.
    pub cumulative_input: u64,

    /// Output tokens since the resumed session began.
    ///
    /// The output half of the same accounting.
    pub cumulative_output: u64,

    /// The input tokens of the resumed session's most recent turn.
    ///
    /// The approximate size of the context the resume re-loads; the first
    /// new turn re-reports it.
    pub last_input: u64,

    /// The engine's context-size estimate at the moment of the save.
    ///
    /// What the usage bar shows across the resume, before the first turn
    /// replaces it with a live figure.
    pub context_tokens: u64,
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
    fn push_hands_out_fresh_ids_and_keeps_send_order() {
        let mut queue = PromptQueue::default();

        let first = queue.push("first".to_string());
        let second = queue.push("second".to_string());
        let third = queue.push("third".to_string());

        assert_eq!((first, second, third), (0, 1, 2));
        assert_eq!(queue.len(), 3);
        let texts: Vec<&str> = (0..queue.len())
            .filter_map(|index| queue.get(index))
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts, ["first", "second", "third"]);
        assert_eq!(
            (
                queue.index_of(first),
                queue.index_of(second),
                queue.index_of(third)
            ),
            (Some(0), Some(1), Some(2))
        );
    }

    #[test]
    fn clearing_drops_every_waiting_prompt_without_reusing_ids() {
        // `/clear` empties the session between turns: nothing that was waiting
        // may surface as the fresh session's first turn, and an id named before
        // the clear must not silently re-point at a prompt issued after it.
        let mut queue = PromptQueue::default();
        let first = queue.push("first".to_string());
        queue.push("second".to_string());

        queue.clear();

        assert!(queue.is_empty(), "nothing survives a clear");
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.index_of(first), None, "the old id names nothing");
        // A prompt issued after the clear takes a fresh id, never a recycled one.
        let later = queue.push("later".to_string());
        assert_ne!(later, first, "ids are not reused across a clear");
        assert_eq!(
            queue.index_of(first),
            None,
            "the old id still names nothing"
        );
    }

    #[test]
    fn removing_a_prompt_yields_its_current_text() {
        let mut queue = PromptQueue::default();
        let id = queue.push("first".to_string());
        queue.push("second".to_string());

        assert_eq!(queue.remove(id).as_deref(), Some("first"));

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.index_of(id), None);
        assert_eq!(
            queue.get(0).map(|entry| entry.text.as_str()),
            Some("second")
        );
        // And the claim cannot be repeated: the prompt is gone, not spent.
        assert_eq!(queue.remove(id), None);
    }

    #[test]
    fn removing_a_dropped_or_absent_prompt_yields_nothing() {
        let mut queue = PromptQueue::default();
        let kept = queue.push("kept".to_string());
        let dropped = queue.push("dropped".to_string());

        assert_eq!(queue.remove(dropped).as_deref(), Some("dropped"));
        // Gone once: nothing left to take, and nothing taken from the row
        // behind where it sat.
        assert_eq!(queue.remove(dropped), None);
        // An id this queue never issued, including one its neighbour will
        // be issued next.
        assert_eq!(queue.remove(99), None);

        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.get(0).map(|entry| (entry.id, entry.text.as_str())),
            Some((kept, "kept"))
        );
    }

    #[test]
    fn replace_edits_in_place_and_leaves_the_order_alone() {
        let mut queue = PromptQueue::default();
        queue.push("first".to_string());
        let second = queue.push("second".to_string());
        queue.push("third".to_string());

        assert!(queue.replace(second, "second, corrected".to_string()));

        assert_eq!(queue.index_of(second), Some(1));
        assert_eq!(
            queue.get(1).map(|entry| (entry.id, entry.text.as_str())),
            Some((second, "second, corrected"))
        );
        assert_eq!(queue.get(0).map(|entry| entry.text.as_str()), Some("first"));
        assert_eq!(queue.get(2).map(|entry| entry.text.as_str()), Some("third"));
        // And the edited text is what leaves: the model reads the fix, not
        // the first draft.
        assert_eq!(queue.remove(second).as_deref(), Some("second, corrected"));
    }

    #[test]
    fn replace_reports_nothing_when_the_prompt_is_gone() {
        let mut queue = PromptQueue::default();
        let sent_on = queue.push("queued".to_string());
        queue.push("waiting".to_string());
        // The driver's claim: this prompt is out of the queue, its id
        // still sitting in the channel.
        queue.remove(sent_on);

        assert!(!queue.replace(sent_on, "edited too late".to_string()));
        assert!(!queue.replace(99, "never queued".to_string()));

        // Nothing moved and nothing was written anywhere near it.
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.get(0).map(|entry| entry.text.as_str()),
            Some("waiting")
        );
    }

    #[test]
    fn a_removed_prompt_leaves_the_rest_whole_and_in_order() {
        let mut queue = PromptQueue::default();
        queue.push("first".to_string());
        let middle = queue.push("middle".to_string());
        let last = queue.push("last".to_string());

        assert_eq!(queue.remove(middle).as_deref(), Some("middle"));

        assert_eq!(queue.len(), 2);
        assert_eq!(queue.get(0).map(|entry| entry.text.as_str()), Some("first"));
        assert_eq!(queue.get(1).map(|entry| entry.text.as_str()), Some("last"));
        // The survivor's address is untouched, so an edit confirmed now
        // still lands on its own prompt rather than on the row it moved to.
        assert_eq!(queue.index_of(last), Some(1));
        assert!(queue.replace(last, "last, corrected".to_string()));
        assert_eq!(queue.get(0).map(|entry| entry.text.as_str()), Some("first"));
    }

    #[test]
    fn ids_never_repeat_after_a_prompt_leaves() {
        let mut queue = PromptQueue::default();
        let first = queue.push("one".to_string());
        let second = queue.push("two".to_string());
        queue.remove(first);
        queue.remove(second);

        let third = queue.push("three".to_string());

        assert_ne!(third, first);
        assert_ne!(third, second);
        assert_eq!(queue.index_of(third), Some(0));
        assert_eq!(queue.index_of(first), None);

        // A prompt that left early is still unfindable after its
        // successor arrives: the id was retired with it, not recycled to
        // whoever happens to sit at that position now.
        let fourth = queue.push("four".to_string());
        queue.remove(third);
        assert_eq!(queue.index_of(third), None);
        assert_eq!(queue.index_of(fourth), Some(0));
    }

    #[test]
    fn an_empty_queue_answers_for_every_lookuper() {
        let mut queue = PromptQueue::default();

        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert!(queue.get(0).is_none());
        assert_eq!(queue.index_of(0), None);
        assert_eq!(queue.remove(0), None);
        assert!(!queue.replace(0, "nothing to edit".to_string()));

        // And the first prompt after an empty queue still starts clean.
        let id = queue.push("first".to_string());
        assert_eq!(queue.index_of(id), Some(0));
        assert_eq!(queue.len(), 1);
    }
}
