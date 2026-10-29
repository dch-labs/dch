//! The todo funnel as engine policy: creation, start discipline,
//! staleness, and the end-of-run follow-up.
//!
//! The plan's law is that it moves at the moments of work — an entry
//! goes [`InProgress`](dch_tools::TodoStatus::InProgress) when its work
//! begins and [`Completed`](dch_tools::TodoStatus::Completed) the moment
//! it lands — and this module is the set of belts that keep a model on
//! that law. A run several turns in with no plan at all earns one
//! note to file one. Work fired against a plan where nothing
//! is in progress earns the start-discipline reminder on the very next
//! request — the moment the flow broke, not turns later. A plan left
//! untouched while undone work stands earns the staleness reminder,
//! naming the undone entries so the model can act without rebuilding
//! the list from memory; a plan being worked stays silent. And a run
//! that lands with undone items still earns one follow-up turn. All
//! mid-run wordings travel through the loop's steering slot — a
//! user message appended at the tail of the next request — so a
//! prefix-cache provider never re-prefills the standing conversation
//! for a reminder. Steering commits the message into history, where
//! every host filter (transcript render, resume replay) drops the
//! wording [`is_nudge_wording`] recognizes. The wording is the
//! crate's own, fixed for every host; the echo the end-of-run half
//! owes the display arrives through the callback the host registers
//! with the builder.

use std::fmt::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dch_tools::TodoEntry;
use dch_tools::TodoStatus;
use loopctl::contributor::ContextContributor;
use loopctl::contributor::ContributorContext;
use loopctl::engine::Steering;
use loopctl::hooks::Hook;
use loopctl::hooks::PostRunAction;
use loopctl::hooks::context::PostToolUseContext;
use loopctl::hooks::context::RunEndContext;
use loopctl::message::Message;
use loopctl::message::MessagePart;
use loopctl::message::Role;

/// One plan state as the staleness clock compares it.
///
/// Every entry's id, subject, and status, in store order — the empty
/// vector is the empty plan, the state every run starts from.
type PlanState = Vec<(String, String, TodoStatus)>;

/// The work-started signal: tool activity against an unclaimed plan.
///
/// Set by [`TodoNudgeHook`]'s post-tool consult when a successful
/// non-`TodoWrite` tool call lands while the plan holds undone work and
/// nothing stands in progress — the exact moment the start discipline
/// broke — and cleared by any successful `TodoWrite`, at every run's
/// first turn, and by [`Self::record_tool_call`] itself whenever the
/// plan no longer matches the violation. The contributor reads it at
/// the next turn top, so the reminder lands on the first request after
/// the break.
pub(crate) struct PlanGate {
    /// Whether unclaimed work has fired since the last plan write.
    ///
    /// The whole state of the gate: `true` means a tool succeeded
    /// against a plan with open work and no runner, and the contributor
    /// owes the start reminder for that episode.
    work_unmarked: AtomicBool,
}

impl Default for PlanGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PlanGate {
    /// A gate that has seen no work.
    ///
    /// Every run starts here — the contributor's first-turn consult
    /// clears whatever a prior run left, so a carried plan's episodes
    /// begin unreminded.
    pub(crate) fn new() -> Self {
        Self {
            work_unmarked: AtomicBool::new(false),
        }
    }

    /// Record one completed tool call against the plan.
    ///
    /// A successful `TodoWrite` clears the gate — the plan was just
    /// touched, so no start violation stands. Any other successful call
    /// sets it when the plan holds undone work and nothing is in
    /// progress, and leaves it untouched otherwise: failed calls never
    /// signal work, and a plan with a runner has nothing to claim.
    pub(crate) fn record_tool_call(&self, tool: &str, ok: bool, todos: &Mutex<Vec<TodoEntry>>) {
        if tool == "TodoWrite" {
            if ok {
                self.clear();
            }
            return;
        }
        if !ok {
            return;
        }
        let store = todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unclaimed = !store.is_empty()
            && store
                .iter()
                .any(|entry| !matches!(entry.status, TodoStatus::Completed))
            && store
                .iter()
                .all(|entry| !matches!(entry.status, TodoStatus::InProgress));
        if unclaimed {
            self.work_unmarked.store(true, Ordering::SeqCst);
        }
    }

    /// Whether unclaimed work stands recorded.
    ///
    /// The contributor's read of the episode signal; the gate is not
    /// consumed — damping is the contributor's, so a re-consult of the
    /// same turn reproduces its message.
    fn work_unmarked(&self) -> bool {
        self.work_unmarked.load(Ordering::SeqCst)
    }

    /// Clear the episode signal.
    ///
    /// Run-start bookkeeping: a new run's first tool call, not the prior
    /// run's, decides the next episode.
    fn clear(&self) {
        self.work_unmarked.store(false, Ordering::SeqCst);
    }
}

/// The one follow-up a run with unfinished plan work earns.
///
/// Reads the todo store the run's own tool calls filed, and offers the
/// engine a reminder turn when anything on the list still stands
/// undone. The echo callback fires with the same reminder text the
/// moment the follow-up is accepted, so a host that observes without
/// displaying — a headless logger, a test counting offers — learns the
/// turn began. One nudge per run by the engine's bound: the follow-up
/// turn itself is never offered another, whatever its answer leaves on
/// the list.
pub(crate) struct TodoNudgeHook {
    /// The plan store the run's `TodoWrite` calls filed into.
    ///
    /// The same `Arc` the runner's context hands its tools, read here
    /// at the run's end and at every post-tool consult — the freshest
    /// list there is.
    todos: Arc<Mutex<Vec<TodoEntry>>>,

    /// The work-started signal shared with the contributor.
    ///
    /// The post-tool consult is the only place tool activity is
    /// observable; the gate carries the one bit the contributor needs
    /// across the seam.
    gate: Arc<PlanGate>,

    /// How the host observes the accepted reminder.
    ///
    /// Called once, with the reminder text, at the moment the
    /// follow-up is accepted — before the turn runs. The TUI passes a
    /// no-op: the reminder must not surface as the user's words
    /// anywhere a user reads, so the callback exists for hosts that
    /// observe without displaying.
    echo: Arc<dyn Fn(String) + Send + Sync>,

    /// Whether the host will still take a follow-up turn.
    ///
    /// Asked at every clean run end the turn budget leaves room to
    /// offer a follow-up at, before the plan is read or any reminder
    /// built. A host whose session is leaving answers false and the
    /// offer dies there — no echo, no request, no row for a turn the
    /// teardown would only abort.
    may_extend: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl TodoNudgeHook {
    /// Build the policy over a plan store, the shared work gate, an
    /// echo, and the host's leave to extend.
    ///
    /// All four halves outlive the build: the store is the same `Arc`
    /// the runner's tools write through, the gate is the one the
    /// contributor reads, and the echo and predicate are held for the
    /// runner's whole lifetime.
    pub(crate) fn new(
        todos: Arc<Mutex<Vec<TodoEntry>>>,
        gate: Arc<PlanGate>,
        echo: Arc<dyn Fn(String) + Send + Sync>,
        may_extend: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            todos,
            gate,
            echo,
            may_extend,
        }
    }
}

impl Hook for TodoNudgeHook {
    fn name(&self) -> &'static str {
        "todo-nudge"
    }

    fn on_post_tool_use(&self, ctx: &PostToolUseContext) {
        self.gate
            .record_tool_call(&ctx.tool_name, !ctx.is_error, &self.todos);
    }

    fn on_run_followup(&self, _ctx: &RunEndContext) -> Option<PostRunAction> {
        if !(self.may_extend)() {
            tracing::debug!(
                target: "dch_loop::todo_nudge",
                "end-of-run offer declined by the host"
            );
            return None;
        }
        let reminder = reminder_text(&self.todos)?;
        tracing::debug!(
            target: "dch_loop::todo_nudge",
            "{reminder}"
        );
        (self.echo)(reminder.clone());
        Some(PostRunAction::FollowUp {
            prompt: reminder,
            origin: "hook:todo-nudge".to_string(),
        })
    }
}

/// The mid-run belts as tail-side guidance the user never sees.
///
/// Consulted by the engine at the top of every turn. The planless run
/// earns one creation note; work recorded against an unclaimed plan
/// earns the start-discipline reminder for that episode; a plan frozen
/// past the grace window with undone work standing earns the staleness
/// reminder naming the undone entries. The reminder text is delivered
/// through the loop's steering slot, so it appends at the tail of the
/// next request's conversation — a head-side insertion would mutate
/// the standing prefix and force a prefix-cache provider to re-prefill
/// the whole context on every nudged turn. Steering commits the
/// message into history, so the wording is one [`is_nudge_wording`]
/// recognizes and every host filter — transcript render, resume
/// replay — already drops. A plan being worked stays silent — the
/// staleness clock resets on any write, so the belts never drumbeat
/// into active list work.
pub(crate) struct TodoReminderContributor {
    /// The plan store the run's `TodoWrite` calls filed into.
    ///
    /// The same `Arc` the runner's context hands its tools, read here
    /// at every turn top — the freshest list there is.
    todos: Arc<Mutex<Vec<TodoEntry>>>,

    /// The work-started signal the hook records.
    ///
    /// Read at every consult after the predicate; the episode it names
    /// is the start rule's trigger.
    gate: Arc<PlanGate>,

    /// Whether the host will still take a turn's worth of steering.
    ///
    /// The reminder never reaches the user's screen and rides a turn
    /// that is already happening, but it still aims the model, and a
    /// host whose session is leaving does not aim it. Asked first at
    /// every consult, before the plan is read at all — even when
    /// nothing is due, so a host whose predicate carries a side
    /// effect sees one ask per turn top.
    may_extend: Arc<dyn Fn() -> bool + Send + Sync>,

    /// The loop's steering slot the reminder text rides.
    ///
    /// Attached by the runner once the loop exists: a reminder is a
    /// user message appended at the next turn boundary, which is what
    /// keeps the standing conversation a stable prefix. `None` until
    /// then, and in tests that pin the decision law alone —
    /// [`Self::pending_reminder`] answers without a slot.
    steering: Option<Arc<Steering>>,

    /// The staleness clock and the per-rule damping turns.
    ///
    /// The state fingerprint and the turn it first stood feed the
    /// staleness rule; the two damping turns hold the turn each
    /// reminder last fired, so a rule stays quiet inside its re-arm
    /// window and through a re-consult of the answered turn itself
    /// — delivery rides the steering slot, so a push the engine's
    /// discarded preparation already made is still waiting in the
    /// slot and a same-turn refire would deliver it twice. Reset
    /// whole at every run's first
    /// turn: the runner treats plans as carried, but episode and clock
    /// state belong to one run.
    dampers: Mutex<Dampers>,

    /// This run's shape, for the planless creation note.
    ///
    /// Two latches, both reset at every run's first turn: whether the
    /// run has ever shown a plan, and whether the note itself has
    /// already been contributed.
    run_shape: Mutex<RunShape>,
}

/// The turn-keyed damping state one run's belts carry.
///
/// Private state of [`TodoReminderContributor`]; reset together at each
/// run's first turn.
#[derive(Default)]
struct Dampers {
    /// The plan state this run last showed, and the turn it first stood.
    ///
    /// The staleness clock: a state's age is the current turn minus its
    /// birth turn, and any change to the plan — status, subject, id,
    /// order — starts a new clock. `None` before the run's first
    /// consult.
    state: Option<(PlanState, usize)>,

    /// The turn the start reminder last fired, this episode.
    ///
    /// `None` when no episode stands or the last one closed — an entry
    /// going in progress ends an episode, so the next one earns its
    /// reminder immediately.
    start: Option<usize>,

    /// The turn the staleness reminder last fired for the standing state.
    ///
    /// Cleared whenever the state changes, so a fresh clock always
    /// starts unreminded.
    stale: Option<usize>,
}

/// The per-run latches behind the planless creation note.
///
/// Private state of [`TodoReminderContributor`]; reset together at each
/// run's first turn.
struct RunShape {
    /// Whether any consult of this run has seen a non-empty plan.
    ///
    /// Latches on the first non-empty sight and stays set for the run's
    /// rest — the creation note's off switch, since an emptied list is
    /// a filed-then-closed plan, not a never-filed one.
    saw_plan: bool,

    /// Whether the creation note has been contributed this run.
    ///
    /// Set with the note's one contribution, so however long the run
    /// drifts planless afterward it is never told twice.
    noted: bool,
}

impl TodoReminderContributor {
    /// Build the contributor over a plan store, the shared work gate,
    /// and the host's leave to extend.
    ///
    /// The store is the same `Arc` the runner's tools write through and
    /// the gate the one its hook records into; every run's first turn
    /// re-sees whatever plan that run holds.
    pub(crate) fn new(
        todos: Arc<Mutex<Vec<TodoEntry>>>,
        gate: Arc<PlanGate>,
        may_extend: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            todos,
            gate,
            may_extend,
            steering: None,
            dampers: Mutex::new(Dampers::default()),
            run_shape: Mutex::new(RunShape {
                saw_plan: false,
                noted: false,
            }),
        }
    }

    /// Attach the loop's steering slot the reminders ride.
    ///
    /// The slot exists only once the loop is built, so the runner
    /// constructs the contributor first and hands the slot over before
    /// registering it. Without a slot the decision law still runs —
    /// [`Self::pending_reminder`] answers — but nothing is delivered.
    pub(crate) fn with_steering(mut self, steering: Arc<Steering>) -> Self {
        self.steering = Some(steering);
        self
    }

    /// The reminder this turn owes, if any, as the text to deliver.
    ///
    /// The whole decision law — the turn reset, the planless note,
    /// the start episode, the staleness clock, and every damper —
    /// lives here, so the law stays pinnable without a loop or a slot.
    /// The delivery half is [`Self::contribute`], which rides the text
    /// through the steering slot.
    fn pending_reminder(&self, turn: usize) -> Option<String> {
        if turn == 0 {
            *self
                .dampers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Dampers::default();
            *self
                .run_shape
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = RunShape {
                saw_plan: false,
                noted: false,
            };
            self.gate.clear();
        }
        if !(self.may_extend)() {
            return None;
        }
        let state = plan_state(&self.todos);
        let mut shape = self
            .run_shape
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.is_empty() {
            shape.saw_plan = true;
        }
        if state.is_empty() && !shape.saw_plan && !shape.noted && turn >= PLANLESS_TURNS {
            shape.noted = true;
            tracing::debug!(
                target: "dch_loop::todo_nudge",
                turn,
                "creation note contributed"
            );
            return Some(creation_note_text());
        }
        drop(shape);
        if !state
            .iter()
            .any(|entry| !matches!(entry.2, TodoStatus::Completed))
        {
            // A clean or empty plan owes nothing, and any open start
            // episode is over: its entry completed or its plan emptied.
            self.dampers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .start = None;
            return None;
        }
        let mut dampers = self
            .dampers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let birth = match dampers.state.as_ref() {
            Some((seen, birth)) if *seen == state => *birth,
            _ => {
                dampers.state = Some((state.clone(), turn));
                dampers.stale = None;
                turn
            }
        };
        let none_running = state
            .iter()
            .all(|entry| !matches!(entry.2, TodoStatus::InProgress));
        if none_running && self.gate.work_unmarked() {
            let damped = dampers.start.is_some_and(|answered| {
                turn >= answered && turn.saturating_sub(answered) <= STALE_REARM_TURNS
            });
            if !damped {
                dampers.start = Some(turn);
                let reminder = start_wording();
                tracing::debug!(
                    target: "dch_loop::todo_nudge",
                    turn,
                    "{reminder}"
                );
                return Some(reminder);
            }
        } else if !none_running {
            dampers.start = None;
        }
        if turn.saturating_sub(birth) >= STALE_GRACE_TURNS {
            if let Some(answered) = dampers.stale
                && turn >= answered
                && turn.saturating_sub(answered) <= STALE_REARM_TURNS
            {
                return None;
            }
            dampers.stale = Some(turn);
            let reminder = stale_wording(&state, turn.saturating_sub(birth));
            tracing::debug!(
                target: "dch_loop::todo_nudge",
                turn,
                "{reminder}"
            );
            return Some(reminder);
        }
        None
    }
}

/// How many turns a run may run planless before the creation note.
///
/// Three: short tasks — a question, a quick edit, a two-step setup —
/// finish inside it and never see the note, while anything still going
/// by the fourth request has enough moving parts that a plan would have
/// earned its keep.
const PLANLESS_TURNS: usize = 3;

/// How many turns a plan may stand untouched before the staleness
/// reminder.
///
/// The grace window: a plan being worked changes within it and never
/// meets the reminder, while a plan frozen past it has open work
/// standing that nothing is advancing — the state the reminder exists
/// for. Three keeps the catch close behind the freeze without firing on
/// an entry legitimately mid-flight on a busy turn.
const STALE_GRACE_TURNS: usize = 3;

/// How many turns a reminded rule stays quiet before it re-arms.
///
/// The belt against a model that ignores a reminder outright: the same
/// frozen state — or the same unclaimed-work episode — re-earns its
/// reminder every so many turns, so a whole run cannot drift on one
/// unanswered word. Eight is rare enough never to drumbeat into active
/// work and frequent enough that a 90-turn drift meets it a dozen
/// times.
const STALE_REARM_TURNS: usize = 8;

/// How many undone entries the staleness reminder lists before eliding.
///
/// The list exists so the model can act without rebuilding the plan
/// from transcript memory; past a dozen entries the tail is context the
/// reminder cannot buy, and the count names what the ellipsis cut.
const ITEM_CAP: usize = 12;

impl ContextContributor for TodoReminderContributor {
    fn contribute(&self, ctx: &ContributorContext<'_>) -> Option<Message> {
        // Delivery only. The decision law lives in
        // [`Self::pending_reminder`], and its text rides the loop's
        // steering slot — a user message appended at the next turn
        // boundary — because this contract's placement is head-side: a
        // message returned here is prepended ahead of the standing
        // conversation, and a prefix-cache provider would re-prefill
        // the whole context on every nudged turn. Riding the tail
        // keeps the earlier request an untouched prefix of the next.
        // The steering path commits the message into history, where
        // every host filter drops the wording [`is_nudge_wording`]
        // recognizes. Returning `None` keeps the per-turn consult —
        // the law's clock — without a head mutation.
        if let Some(reminder) = self.pending_reminder(ctx.turn)
            && let Some(slot) = &self.steering
            && !slot.push(Message::user(reminder))
        {
            tracing::debug!(
                target: "dch_loop::todo_nudge",
                "reminder undeliverable: no run in flight to steer"
            );
        }
        None
    }
}

/// The planless run's creation note wording.
///
/// The funnel's first belt, under the tool contract and the system
/// prompt: the words said "keep the todo list current" and the run
/// answered with turns of work and no plan. An imperative with a single
/// out for work already finished; fixed wording like every belt, and
/// deliberately distinct from the two reminders — it directs filing,
/// not updating.
fn creation_note_text() -> String {
    "note: this run is several turns in with no plan — file one with \
     TodoWrite now if any work remains, and keep it current as you go"
        .to_string()
}

/// The start-discipline reminder wording.
///
/// The root-flow catch: tool work has fired against a plan whose
/// entries all stand unstarted, so the model began implementing
/// without signaling what it began. The wording names the one move
/// that closes the gap — no count, because the plan's entries are all
/// equally unclaimed and the tool's own render names them.
fn start_wording() -> String {
    "reminder: work is underway with no plan entry in progress — mark \
     the entry you are working on in_progress with TodoWrite now"
        .to_string()
}

/// The staleness reminder wording, naming the undone entries.
///
/// The plan has stood untouched past the grace window while undone work
/// remains — finished-but-unmarked, started-but-abandoned, or never
/// begun. The list is the point: each undone entry's icon, id, and
/// subject ride the reminder so the model can act on exactly them
/// without rebuilding the plan from transcript memory, and the
/// instruction covers both directions of the moment rule. Past
/// [`ITEM_CAP`] entries the tail elides with a count.
fn stale_wording(state: &PlanState, age: usize) -> String {
    let undone: Vec<&(String, String, TodoStatus)> = state
        .iter()
        .filter(|entry| !matches!(entry.2, TodoStatus::Completed))
        .collect();
    let mut text = format!(
        "reminder: {} todo item{} still not completed, the plan unchanged \
         for {} turns — mark finished entries completed and the entry you \
         are starting in_progress with TodoWrite:",
        undone.len(),
        if undone.len() == 1 { "" } else { "s" },
        age
    );
    for entry in undone.iter().take(ITEM_CAP) {
        let (id, subject, status) = (entry.0.as_str(), entry.1.as_str(), entry.2);
        let icon = match status {
            TodoStatus::InProgress => '◐',
            TodoStatus::Completed => '●',
            TodoStatus::Pending => '○',
        };
        write!(text, "\n  {icon} [{id}] {subject}").ok();
    }
    let hidden = undone.len().saturating_sub(ITEM_CAP);
    if hidden > 0 {
        write!(text, "\n  … +{hidden} more").ok();
    }
    text
}

/// The plan's identity as the staleness clock compares it: every
/// entry's id, subject, and status, in store order.
///
/// A subject reword is a plan change — the model described the work
/// differently — so it starts a new clock like any other edit.
fn plan_state(todos: &Mutex<Vec<TodoEntry>>) -> PlanState {
    let todos = todos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    todos
        .iter()
        .map(|entry| (entry.id.clone(), entry.subject.clone(), entry.status))
        .collect()
}

/// The reminder wording for a list with undone work, if there is any.
///
/// The end-of-run half's gate: the count names what stands, `None`
/// stays `None` for a finished or empty list so a clean run ends clean.
pub(crate) fn reminder_text(todos: &Mutex<Vec<TodoEntry>>) -> Option<String> {
    let undone = count_undone(todos);
    (undone > 0).then(|| closing_wording(undone))
}

/// Whether a message's text is one of this module's reminder wordings.
///
/// Every reminder — the creation note, the start-discipline and
/// staleness belts, and the end-of-run follow-up — reaches committed
/// history as a user message through the steering slot, and a host
/// that refuses to persist or replay a reminder as the user's words
/// uses this to drop exactly those messages at its save, render, and
/// resume sites. The recognition is the wording set's own: the two
/// fixed mid-run openings match by prefix, and the count-led shape —
/// a count, then the todo-item phrase — carries the staleness and
/// closing wordings that name their undone count.
#[must_use]
pub fn is_nudge_wording(text: &str) -> bool {
    if text.starts_with("note: this run is several turns in with no plan") {
        return true;
    }
    if text.starts_with("reminder: work is underway with no plan entry in progress") {
        return true;
    }
    let Some(rest) = text.strip_prefix("reminder: ") else {
        return false;
    };
    let Some((count, tail)) = rest.split_once(' ') else {
        return false;
    };
    count.parse::<u64>().is_ok() && tail.starts_with("todo item")
}

/// The committed history minus the end-of-run reminder's prompt, in
/// the shape a provider replays.
///
/// The follow-up turn commits as `[user: reminder, assistant:
/// answer]`, and a host that persists the prompt would replay the
/// reminder as the user's own words on every resume — so the filter
/// drops exactly the user messages [`is_nudge_wording`] recognizes.
/// The drop leaves a seam the raw filter cannot: with the prompt gone
/// the turn's answer stands directly after the run's own last
/// assistant message, two consecutive assistant records — the
/// malformed shape strict role-alternation providers reject, failing
/// a resumed session's first request. The orphaned answer therefore
/// merges into the preceding assistant message, with a paragraph
/// break inserted when both halves carry text so the words never glue
/// into one run — the same rule the display-derive path merges its
/// graduated records by. Anything that is not the message immediately
/// following a dropped reminder passes through untouched.
///
/// The machine wire carries no `steered` mark, so a user who types
/// the reminder wording verbatim as their prompt is dropped here
/// while the display file's steered-gated filter keeps the row — the
/// accepted cost of matching wording on a wire that cannot say who
/// spoke.
#[must_use]
pub fn history_without_nudge(history: &[Message]) -> Vec<Message> {
    let mut filtered: Vec<Message> = Vec::with_capacity(history.len());
    let mut after_dropped_nudge = false;
    for message in history {
        if message.role == Role::User && is_nudge_wording(&Message::text_content(message)) {
            after_dropped_nudge = true;
            continue;
        }
        let merges_into_last = after_dropped_nudge
            && message.role == Role::Assistant
            && matches!(
                filtered.last(),
                Some(Message {
                    role: Role::Assistant,
                    ..
                })
            );
        if merges_into_last
            && let Some(Message {
                parts: existing, ..
            }) = filtered.last_mut()
        {
            let holds_text = |parts: &[MessagePart]| {
                parts
                    .iter()
                    .any(|part| matches!(part, MessagePart::Text { .. }))
            };
            if holds_text(existing) && holds_text(&message.parts) {
                existing.push(MessagePart::text("\n\n"));
            }
            existing.extend(message.parts.clone());
        } else {
            filtered.push(message.clone());
        }
        after_dropped_nudge = false;
    }
    filtered
}

/// How many entries on the plan are not completed.
///
/// Pending and in-progress entries both stand as work a reminder can
/// name; only a completed entry is closed. The lock recovers from
/// poison the codebase's usual way, so a panicking counter cannot take
/// the reminder lane down with it.
fn count_undone(todos: &Mutex<Vec<TodoEntry>>) -> usize {
    let todos = todos
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    todos
        .iter()
        .filter(|entry| !matches!(entry.status, TodoStatus::Completed))
        .count()
}

/// The end-of-run nudge's wording for `undone` standing items.
///
/// The final backstop, after the mid-run belts had their say: the run
/// is over, so the instruction is to finish or close the list, with the
/// rung law named so the closing sweep knows a completed request from
/// pending lands one rung short and walks with a follow-up call. The
/// count is the caller's, the plural follows it, and the opening shape
/// is the one [`is_nudge_wording`] recognizes.
fn closing_wording(undone: usize) -> String {
    format!(
        "reminder: {undone} todo item{} still not completed — finish the \
         remaining work, or mark the entries done with TodoWrite, then \
         finish (a completed request from pending lands in_progress — \
         walk it with a follow-up call)",
        if undone == 1 { "" } else { "s" }
    )
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
    use loopctl::hooks::context::RunEndReason;
    use proptest::prelude::*;

    fn entry(id: &str, status: TodoStatus) -> TodoEntry {
        TodoEntry {
            id: id.to_string(),
            subject: format!("subject {id}"),
            description: String::new(),
            status,
            active_form: None,
        }
    }

    fn todos(entries: &[TodoEntry]) -> Arc<Mutex<Vec<TodoEntry>>> {
        Arc::new(Mutex::new(entries.to_vec()))
    }

    fn run_end() -> RunEndContext {
        RunEndContext {
            session_id: uuid::Uuid::new_v4(),
            reason: RunEndReason::Complete,
            total_turns: 1,
            total_tokens: 0,
            duration_secs: 0,
        }
    }

    fn contributor(store: &Arc<Mutex<Vec<TodoEntry>>>) -> TodoReminderContributor {
        TodoReminderContributor::new(
            Arc::clone(store),
            Arc::new(PlanGate::new()),
            Arc::new(|| true),
        )
    }

    fn contributor_with_gate(
        store: &Arc<Mutex<Vec<TodoEntry>>>,
        gate: &Arc<PlanGate>,
    ) -> TodoReminderContributor {
        TodoReminderContributor::new(Arc::clone(store), Arc::clone(gate), Arc::new(|| true))
    }

    #[test]
    fn the_nudge_wording_is_recognized_for_the_save_filter() {
        // The hosts' save, render, and resume sites drop a reminder by
        // this shape — every wording this module emits answers true,
        // anything a user could type answers false. The mid-run
        // wordings ride the steering slot as user messages now, so
        // their fixed openings answer true alongside the count-led
        // closing and staleness shapes.
        assert!(is_nudge_wording(&closing_wording(1)));
        assert!(is_nudge_wording(&closing_wording(2)));
        let stale = stale_wording(
            &vec![(
                "4".to_string(),
                "A subject".to_string(),
                TodoStatus::Pending,
            )],
            6,
        );
        assert!(is_nudge_wording(&stale));
        assert!(is_nudge_wording(&start_wording()));
        assert!(is_nudge_wording(&creation_note_text()));
        assert!(!is_nudge_wording("steer: take the other branch"));
        assert!(!is_nudge_wording("reminder: soon"));
        assert!(!is_nudge_wording(
            "reminder: 4 items of some other kind remain"
        ));
        assert!(!is_nudge_wording(
            "note: this run is several turns in with a different opening"
        ));
    }

    #[test]
    fn the_saved_history_alternates_once_the_nudge_prompt_is_dropped() {
        // The follow-up turn commits as [user: reminder, assistant:
        // answer]; dropping the prompt alone would leave the answer
        // standing directly after the run's own last assistant
        // message — the consecutive-assistant shape strict
        // role-alternation providers reject on a resumed session's
        // first request. The filter merges the orphan into that
        // message, so the saved file replays alternating.
        let history = vec![
            Message::user("do the thing"),
            Message::assistant("working on it"),
            Message::user(closing_wording(1)),
            Message::assistant("noted, but not closing it"),
        ];
        let saved = history_without_nudge(&history);
        let roles: Vec<Role> = saved.iter().map(|message| message.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant],
            "the reminder row is gone and its orphaned answer merged: {saved:?}"
        );
        assert_eq!(
            saved.last().expect("merged tail").text_content(),
            "working on it\n\nnoted, but not closing it",
            "text halves keep a paragraph break when they merge"
        );
    }

    #[test]
    fn a_structural_followup_answer_merges_without_a_break() {
        // The break exists for text halves that would otherwise glue
        // into one run of words; a tool-call-only answer is
        // structural, so it extends the preceding assistant message
        // as-is — the derive path's own rule.
        let call = MessagePart::tool_call(
            "call_1",
            "Bash",
            serde_json::json!({"command": "make test"}),
        );
        let history = vec![
            Message::user("do the thing"),
            Message::new(Role::Assistant, vec![MessagePart::text("on it")]),
            Message::user(closing_wording(2)),
            Message::new(Role::Assistant, vec![call]),
        ];
        let saved = history_without_nudge(&history);
        assert_eq!(saved.len(), 2, "the orphan merged: {saved:?}");
        let merged = saved.last().expect("merged tail");
        assert_eq!(
            merged.text_content(),
            "on it",
            "the text half stands alone — no break was inserted"
        );
        assert!(
            merged
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ToolCall { .. })),
            "the structural half rides the merged message"
        );
    }

    #[test]
    fn history_without_a_nudge_passes_through_unchanged() {
        // The repair is surgical: only the message immediately behind
        // a dropped reminder merges, and a history with no reminder
        // in it — including a trailing reminder whose answer never
        // came — comes back exactly as it went in.
        let history = vec![
            Message::user("do the thing"),
            Message::assistant("all done"),
        ];
        let saved = history_without_nudge(&history);
        assert_eq!(
            format!("{saved:?}"),
            format!("{history:?}"),
            "an untouched history comes back verbatim"
        );
        let trailing = vec![
            Message::user("do the thing"),
            Message::assistant("working on it"),
            Message::user(closing_wording(1)),
        ];
        let saved = history_without_nudge(&trailing);
        assert_eq!(
            saved.len(),
            2,
            "an unanswered reminder drops and merges nothing: {saved:?}"
        );
        assert_eq!(
            saved.last().expect("standing tail").text_content(),
            "working on it",
            "the run's own last message stands unchanged"
        );
    }

    #[test]
    fn a_finished_or_empty_list_offers_nothing() {
        let empty: [TodoEntry; 0] = [];
        assert!(
            reminder_text(&Mutex::new(empty.to_vec())).is_none(),
            "an empty plan stands clean — no reminder is due"
        );
        let finished = [entry("a", TodoStatus::Completed)];
        assert!(
            reminder_text(&Mutex::new(finished.to_vec())).is_none(),
            "a plan whose every entry is completed stands clean"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn the_reminder_count_tracks_the_undone_over_any_list(
            entries in proptest::collection::vec(
                (
                    "[a-z]{1,8}",
                    "[a-z ]{0,24}",
                    "[a-z ]{0,40}",
                    0u8..3,
                ),
                0..24
            ),
        ) {
            let list: Vec<TodoEntry> = entries
                .into_iter()
                .map(|(id, subject, description, status)| TodoEntry {
                    id,
                    subject,
                    description,
                    status: match status {
                        0 => TodoStatus::Pending,
                        1 => TodoStatus::InProgress,
                        _ => TodoStatus::Completed,
                    },
                    active_form: None,
                })
                .collect();
            let undone = list
                .iter()
                .filter(|entry| !matches!(entry.status, TodoStatus::Completed))
                .count();
            let reminder = reminder_text(&std::sync::Mutex::new(list));
            if undone == 0 {
                prop_assert!(reminder.is_none());
            } else {
                let reminder = reminder.expect("undone work offers a reminder");
                let named: usize = reminder
                    .strip_prefix("reminder: ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|count| count.parse().ok())
                    .expect("the wording opens with its count");
                prop_assert_eq!(named, undone);
            }
        }
    }

    #[test]
    fn the_reminder_names_the_undone_count_in_both_numbers() {
        let mixed = [
            entry("a", TodoStatus::Completed),
            entry("b", TodoStatus::Pending),
            entry("c", TodoStatus::InProgress),
        ];
        let reminder =
            reminder_text(&Mutex::new(mixed.to_vec())).expect("undone work earns a reminder");
        assert!(
            reminder.starts_with("reminder: 2 todo items still not completed"),
            "the count names every non-completed entry and takes the plural: {reminder}"
        );
    }

    #[test]
    fn the_leave_to_extend_is_consulted_even_when_the_plan_is_clean() {
        let consulted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&consulted);
        let hook = TodoNudgeHook::new(
            todos(&[entry("a", TodoStatus::Completed)]),
            Arc::new(PlanGate::new()),
            Arc::new(|_| {}),
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            }),
        );
        let action = hook.on_run_followup(&run_end());
        assert!(
            action.is_none(),
            "a clean plan offers nothing, whatever the predicate answers"
        );
        assert_eq!(
            consulted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the leave-to-extend is asked once per clean run end, before the \
             list decides whether a reminder is due"
        );
    }

    #[test]
    fn a_declined_leave_to_extend_echoes_nothing_even_with_undone_work() {
        let echoed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let echo_sink = Arc::clone(&echoed);
        let hook = TodoNudgeHook::new(
            todos(&[entry("a", TodoStatus::Pending)]),
            Arc::new(PlanGate::new()),
            Arc::new(move |text| {
                echo_sink.lock().expect("echo lock").push(text);
            }),
            Arc::new(|| false),
        );
        let action = hook.on_run_followup(&run_end());
        assert!(
            action.is_none(),
            "a declining host is owed no turn, undone work or not"
        );
        assert!(
            echoed.lock().expect("echo lock").is_empty(),
            "the decline happens before the echo"
        );
    }

    #[test]
    fn work_without_a_runner_earns_the_start_reminder_once() {
        // The root-flow catch: a tool fired against an all-pending plan,
        // the gate records it, and the very next consult contributes the
        // start reminder — then stays quiet inside the re-arm window
        // while the violation persists.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let gate = Arc::new(PlanGate::new());
        gate.record_tool_call("Read", true, &store);
        let contributor = contributor_with_gate(&store, &gate);
        let first = contributor
            .pending_reminder(1)
            .expect("unclaimed work earns the start reminder immediately");
        assert!(
            first.contains("mark the entry you are working on in_progress"),
            "the wording names the one move that closes the gap: {first}"
        );
        assert!(
            contributor.pending_reminder(2).is_none(),
            "the same episode stays quiet inside the re-arm window"
        );
    }

    #[test]
    fn a_start_reminder_episode_ends_when_an_entry_goes_in_progress() {
        // An entry running closes the episode: the latch clears, and a
        // later episode — the entry completing, work continuing
        // unclaimed — earns its reminder immediately.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let gate = Arc::new(PlanGate::new());
        gate.record_tool_call("Read", true, &store);
        let contributor = contributor_with_gate(&store, &gate);
        assert!(
            contributor.pending_reminder(1).is_some(),
            "the first episode reminds"
        );
        store
            .lock()
            .expect("store lock")
            .first_mut()
            .expect("the entry stands")
            .status = TodoStatus::InProgress;
        assert!(
            contributor.pending_reminder(2).is_none(),
            "a running entry is no violation — nothing is due"
        );
        store
            .lock()
            .expect("store lock")
            .first_mut()
            .expect("the entry stands")
            .status = TodoStatus::Pending;
        gate.record_tool_call("Read", true, &store);
        assert!(
            contributor.pending_reminder(3).is_some(),
            "the next episode earns its reminder without waiting out a window"
        );
    }

    #[test]
    fn a_frozen_start_violation_re_arms_past_the_window() {
        // The model that works on without ever claiming an entry is
        // re-reminded each window past the first, so a whole run cannot
        // drift on one unanswered word. The staleness belt may fire its
        // own wording inside the span — this pin counts start wordings
        // alone.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let gate = Arc::new(PlanGate::new());
        gate.record_tool_call("Read", true, &store);
        let contributor = contributor_with_gate(&store, &gate);
        let start = |turn: usize| {
            contributor
                .pending_reminder(turn)
                .is_some_and(|message| message.contains("no plan entry in progress"))
        };
        assert!(start(1), "the first reminder");
        for turn in 2..=9 {
            assert!(
                !start(turn),
                "turn {turn} stays quiet inside the start window"
            );
        }
        assert!(
            start(10),
            "the frozen violation re-earns the start reminder past the window"
        );
    }

    #[test]
    fn a_stale_plan_earns_the_item_list_reminder_at_three_turns() {
        // The staleness belt: a plan untouched past the grace window
        // with undone work standing earns a reminder that names each
        // undone entry — icon, id, subject — so the model can act
        // without rebuilding the list from memory.
        let store = todos(&[
            entry("done", TodoStatus::Completed),
            entry("wait", TodoStatus::Pending),
            entry("held", TodoStatus::InProgress),
        ]);
        let contributor = contributor(&store);
        assert!(
            contributor.pending_reminder(0).is_none(),
            "the plan's first sight starts the clock, quietly"
        );
        assert!(
            contributor.pending_reminder(2).is_none(),
            "inside the grace window, quiet"
        );
        let reminder = contributor
            .pending_reminder(STALE_GRACE_TURNS)
            .expect("past the grace window the frozen plan reminds");
        assert!(
            reminder.starts_with("reminder: 2 todo items still not completed"),
            "the count leads, as the save filter's shape: {reminder}"
        );
        assert!(
            reminder.contains("○ [wait] subject wait"),
            "each undone entry is named: {reminder}"
        );
        assert!(
            reminder.contains("◐ [held] subject held"),
            "in-progress entries ride their icon: {reminder}"
        );
        assert!(
            !reminder.contains("[done]"),
            "completed entries do not ride the reminder: {reminder}"
        );
    }

    #[test]
    fn a_plan_write_resets_the_staleness_clock() {
        // Any write to the plan starts a new clock, so a plan being
        // worked never meets the reminder — the drumbeat the old
        // per-state law beat into active list work is structurally
        // gone.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let contributor = contributor(&store);
        assert!(
            contributor.pending_reminder(0).is_none(),
            "the clock starts at the plan's first sight"
        );
        store
            .lock()
            .expect("store lock")
            .first_mut()
            .expect("the entry stands")
            .status = TodoStatus::InProgress;
        assert!(
            contributor.pending_reminder(3).is_none(),
            "the write moved the clock — quiet at the old trigger turn"
        );
        assert!(
            contributor.pending_reminder(5).is_none(),
            "and quiet past it, the new state still inside grace"
        );
    }

    #[test]
    fn a_touched_plan_stays_quiet_inside_the_grace_window() {
        // The engaged model — updating every turn or two — never meets
        // a reminder: each write restarts the clock, and three quiet
        // turns never accumulate.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let contributor = contributor(&store);
        let mut turn = 0;
        while turn < 12 {
            assert!(
                contributor.pending_reminder(turn).is_none(),
                "turn {turn}: an engaged plan stays quiet"
            );
            store
                .lock()
                .expect("store lock")
                .first_mut()
                .expect("the entry stands")
                .subject = format!("subject {turn}");
            turn += 2;
        }
    }

    #[test]
    fn a_stale_plan_re_arms_every_window_while_frozen() {
        // The frozen plan is re-reminded at each window edge until it
        // moves or the work closes. The first reminder rides the grace
        // edge (turn 3); the quiet window spans the next eight turns;
        // the edge after that re-earns it.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let contributor = contributor(&store);
        assert!(
            contributor.pending_reminder(0).is_none(),
            "the clock starts at the plan's first sight"
        );
        assert!(
            contributor.pending_reminder(STALE_GRACE_TURNS).is_some(),
            "the first reminder at the grace edge"
        );
        let next_edge = STALE_GRACE_TURNS + STALE_REARM_TURNS + 1;
        for turn in STALE_GRACE_TURNS + 1..next_edge {
            assert!(
                contributor.pending_reminder(turn).is_none(),
                "turn {turn} stays quiet inside the window"
            );
        }
        assert!(
            contributor.pending_reminder(next_edge).is_some(),
            "the next window edge re-earns the reminder"
        );
    }

    #[test]
    fn a_same_turn_reconsult_stays_quiet_because_the_slot_holds_the_push() {
        // The engine may prepare a turn, discard it — a compaction
        // deferral, a steer drain, an overflow salvage — and prepare
        // the same turn number again. The reminder the discarded
        // preparation pushed is still waiting in the steering slot,
        // so the re-consult stays quiet: a refire would deliver the
        // wording twice, once from each push. Only a later turn past
        // the re-arm window re-earns it.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let contributor = contributor(&store);
        assert!(
            contributor.pending_reminder(0).is_none(),
            "the clock starts at the plan's first sight"
        );
        assert!(
            contributor.pending_reminder(STALE_GRACE_TURNS).is_some(),
            "the stale state contributes on the first preparation"
        );
        assert!(
            contributor.pending_reminder(STALE_GRACE_TURNS).is_none(),
            "the same turn's re-preparation stays quiet — the slot \
             already holds the pushed reminder"
        );
        assert!(
            contributor
                .pending_reminder(STALE_GRACE_TURNS + 1)
                .is_none(),
            "a later turn with the state unchanged stays damped"
        );
    }

    #[test]
    fn the_leave_to_extend_is_asked_at_every_turn_top_even_when_quiet() {
        // The predicate is the host's one lane and the first thing
        // every consult asks — before the plan is read at all — so a
        // host whose predicate carries a side effect sees one consult
        // per turn top, whether anything is due or not.
        let consulted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&consulted);
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let contributor = TodoReminderContributor::new(
            Arc::clone(&store),
            Arc::new(PlanGate::new()),
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            }),
        );
        assert!(
            contributor.pending_reminder(1).is_none(),
            "inside grace, quiet"
        );
        assert!(
            contributor.pending_reminder(2).is_none(),
            "still inside grace"
        );
        store.lock().expect("store lock").clear();
        assert!(
            contributor.pending_reminder(3).is_none(),
            "a clean plan contributes nothing"
        );
        assert_eq!(
            consulted.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "one ask per turn-top consult — quiet, quiet, and clean alike"
        );
    }

    #[test]
    fn a_clean_or_empty_plan_contributes_nothing() {
        let contributor = contributor(&todos(&[entry("a", TodoStatus::Completed)]));
        assert!(
            contributor.pending_reminder(1).is_none(),
            "a plan in good standing never interrupts the work"
        );
    }

    #[test]
    fn a_subject_reword_resets_the_staleness_clock() {
        // The state is the list's content, not its write count: the
        // same shape described differently is a new state with a new
        // clock.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let contributor = contributor(&store);
        assert!(
            contributor.pending_reminder(0).is_none(),
            "the clock starts at the plan's first sight"
        );
        assert!(
            contributor.pending_reminder(STALE_GRACE_TURNS).is_some(),
            "the frozen state reminds at the grace edge"
        );
        if let Some(first) = store.lock().expect("store lock").first_mut() {
            first.subject = "a different wording".to_string();
        }
        assert!(
            contributor
                .pending_reminder(STALE_GRACE_TURNS + 1)
                .is_none(),
            "the reworded plan is fresh news — a new clock starts"
        );
        assert!(
            contributor
                .pending_reminder(STALE_GRACE_TURNS + 1 + STALE_GRACE_TURNS)
                .is_some(),
            "and its own grace edge reminds in turn"
        );
    }

    #[test]
    fn a_new_run_resets_the_clocks_and_the_gate() {
        // The runner carries an unfinished plan into the next run, but
        // the clocks and episodes belong to one run: the next run's
        // first turn starts them over.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let gate = Arc::new(PlanGate::new());
        gate.record_tool_call("Read", true, &store);
        let contributor = contributor_with_gate(&store, &gate);
        assert!(
            contributor.pending_reminder(1).is_some(),
            "the first run's episode reminds"
        );
        assert!(
            contributor.pending_reminder(0).is_none(),
            "the next run's first turn resets clock, latches, and gate"
        );
        assert!(
            contributor.pending_reminder(1).is_none(),
            "the gate's episode did not survive the boundary"
        );
    }

    #[test]
    fn a_declining_host_earns_no_reminder() {
        let contributor = contributor(&todos(&[entry("a", TodoStatus::Pending)]));
        assert!(
            contributor.pending_reminder(STALE_GRACE_TURNS).is_none(),
            "the decline lane covers the steered reminders too — a host \
             whose session is leaving does not aim the model"
        );
    }

    #[test]
    fn the_gate_records_only_unclaimed_successful_work() {
        // The signal is exact: a successful non-TodoWrite call against
        // open, unrunnered work sets it; failures never do, a running
        // entry means nothing is unclaimed, and a successful TodoWrite
        // clears whatever stood.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let gate = PlanGate::new();
        gate.record_tool_call("Read", false, &store);
        assert!(!gate.work_unmarked(), "a failed call signals no work");
        gate.record_tool_call("Read", true, &store);
        assert!(gate.work_unmarked(), "unclaimed work sets the gate");
        gate.record_tool_call("TodoWrite", true, &store);
        assert!(!gate.work_unmarked(), "a plan write clears the episode");
        store
            .lock()
            .expect("store lock")
            .first_mut()
            .expect("the entry stands")
            .status = TodoStatus::InProgress;
        gate.record_tool_call("Read", true, &store);
        assert!(
            !gate.work_unmarked(),
            "a plan with a runner has nothing unclaimed"
        );
    }

    #[test]
    fn a_planless_run_earns_one_creation_note_at_the_fourth_turn() {
        let store = todos(&[]);
        let contributor = contributor(&store);
        for turn in 0..PLANLESS_TURNS {
            assert!(
                contributor.pending_reminder(turn).is_none(),
                "turn {turn} stays quiet — short tasks never meet the note"
            );
        }
        let note = contributor
            .pending_reminder(PLANLESS_TURNS)
            .expect("the fourth planless turn is told once");
        assert!(
            note.contains("file one with TodoWrite now"),
            "the note directs filing, distinct from the update reminders: {note}"
        );
        for turn in PLANLESS_TURNS + 1..PLANLESS_TURNS + 4 {
            assert!(
                contributor.pending_reminder(turn).is_none(),
                "however long the run drifts planless, it is told once — \
                 turn {turn} stays quiet"
            );
        }
    }

    #[test]
    fn a_run_that_ever_filed_a_plan_earns_no_creation_note() {
        let store = todos(&[]);
        let contributor = contributor(&store);
        assert!(
            contributor.pending_reminder(1).is_none(),
            "quiet early turns"
        );
        store
            .lock()
            .expect("store lock")
            .push(entry("a", TodoStatus::Completed));
        assert!(
            contributor.pending_reminder(2).is_none(),
            "a filed plan contributes no reminder — every entry completed"
        );
        store.lock().expect("store lock").clear();
        assert!(
            contributor.pending_reminder(5).is_none(),
            "the emptied store is a closed plan, not a planless run — no \
             creation note"
        );
    }

    #[test]
    fn a_new_run_forgets_the_planless_note() {
        let store = todos(&[]);
        let contributor = contributor(&store);
        assert!(
            contributor.pending_reminder(PLANLESS_TURNS).is_some(),
            "the first run's planless drift is noted"
        );
        assert!(
            contributor.pending_reminder(0).is_none(),
            "the next run's first turn resets the latches"
        );
        for turn in 1..PLANLESS_TURNS {
            assert!(
                contributor.pending_reminder(turn).is_none(),
                "quiet early turns again"
            );
        }
        assert!(
            contributor.pending_reminder(PLANLESS_TURNS).is_some(),
            "and its own planless drift is noted on its own fourth turn"
        );
    }

    #[test]
    fn the_hooks_post_tool_consult_feeds_the_gate() {
        // The hook is the only place tool activity is observable; its
        // post-tool consult records the call into the shared gate the
        // contributor reads.
        let store = todos(&[entry("a", TodoStatus::Pending)]);
        let gate = Arc::new(PlanGate::new());
        let hook = TodoNudgeHook::new(
            Arc::clone(&store),
            Arc::clone(&gate),
            Arc::new(|_| {}),
            Arc::new(|| true),
        );
        let ctx = PostToolUseContext {
            tool_name: "Read".to_string(),
            input: serde_json::json!({}),
            output: String::new(),
            is_error: false,
            duration_ms: 0,
            session_id: uuid::Uuid::new_v4(),
            turn_number: 1,
        };
        hook.on_post_tool_use(&ctx);
        assert!(
            gate.work_unmarked(),
            "the consult recorded the unclaimed work"
        );
    }
}
