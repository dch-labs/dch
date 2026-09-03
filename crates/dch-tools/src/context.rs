//! The runner context extension stored on each `loopctl::tool::ToolContext`.
//!
//! Tools retrieve it with [`runner_ctx`] to reach per-call, tool-facing state:
//! the working directory, the agent's per-run todo list, the channel slot
//! for asking the user interactive questions, the session's file-baseline
//! map backing the Write tool's staleness check, and the path-containment
//! policy the file tools resolve under.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use loopctl::tool::ToolError;

use crate::fs::WorkspaceAnchor;
use crate::question::QuestionRequest;
use crate::state::FileBaselines;
use crate::todo::TodoEntry;
use crate::util::ResolvePolicy;

/// Per-call, tool-facing context attached to every `ToolContext`.
///
/// Carries what a tool invocation needs that is specific to this run and isn't
/// already on loopctl's `ToolContext`: the working directory (as a `PathBuf`,
/// the form the file tools prefer), the agent's per-run todo list, the
/// optional channel a tool uses during its call to ask the user a question,
/// the model's file-baseline map backing the Write tool's staleness check,
/// and the path-containment policy the file tools resolve under. Stored as
/// a typed extension via
/// [`ToolContext::set_extension`](loopctl::tool::ToolContext::set_extension)
/// and retrieved with [`runner_ctx`].
///
/// Cloning is cheap — `todos`, `question_tx`, and `file_baselines` are
/// behind `Arc`s, so clones share the same mutable list, the same channel
/// slot, and the same map rather than copying them. This is how multiple
/// tool invocations within one run observe each other's todo-list mutations
/// (and how a Write sees what a prior Read recorded).
#[derive(Clone)]
pub struct RunnerContext {
    /// The working directory the agent operates within.
    ///
    /// Every tool that touches the filesystem resolves relative paths against
    /// this directory. It is the absolute root for file operations (`Read`,
    /// `Write`, `Edit`, `MultiEdit`, `Glob`, `Grep`, etc.). Set once at runner
    /// construction from the configured or CLI-supplied working directory.
    pub cwd: PathBuf,

    /// The agent's current todo list.
    ///
    /// Replaced wholesale by the `TodoWrite` tool on each call — the model
    /// sends the complete desired list, not a delta. The `Arc<Mutex<>>`
    /// wrapper lets concurrent tool calls read and mutate it safely; cloning
    /// [`RunnerContext`] shares the same list (no copy). Per-run: the runner
    /// clears it at the top of each `run()` so a new prompt starts fresh.
    pub todos: Arc<Mutex<Vec<TodoEntry>>>,

    /// Channel for asking the user interactive questions, behind a shared slot.
    ///
    /// Used by the asking tool to send a [`QuestionRequest`] to the UI (a TUI
    /// overlay or a headless reader). The slot starts empty; the asking tool
    /// returns an error instead of blocking when no channel is installed. A
    /// host that can prompt installs its sender before the first run (or
    /// between runs) — every tool dispatch reads the slot live, so a later
    /// installation affects subsequent dispatches immediately. Cloning
    /// [`RunnerContext`] shares the same slot.
    pub question_tx: Arc<Mutex<Option<mpsc::Sender<QuestionRequest>>>>,

    /// The model's latest known content hash per touched file.
    ///
    /// Read records what it observes; a successful write records the
    /// post-write content (see [`FileBaselines`]). The Write tool's
    /// detect-on-write conflict check compares the target's current content
    /// hash against this record before overwriting. Concurrent touches of
    /// one path resolve to the newest observation. Cloning
    /// [`RunnerContext`] shares the same map. Keys are normalized by this
    /// context's record and lookup methods — never by the tool call sites —
    /// so every spelling of a file meets at one key.
    pub file_baselines: Arc<Mutex<FileBaselines>>,

    /// The model's latest known content per live file identity (unix
    /// device and inode).
    ///
    /// The unrestricted policy's second baseline index (see
    /// [`FileIdentities`](crate::state::FileIdentities)): path keys cannot
    /// unify aliases that canonicalization cannot see — two hard links to
    /// one file are two equally canonical spellings — so records and
    /// lookups carry the file's stat identity alongside the path, and the
    /// staleness guard holds whichever spelling of a file arrives. Only
    /// populated and consulted under [`ResolvePolicy::Unrestricted`];
    /// contained keys are the lexical resolution output by design. Cloning
    /// [`RunnerContext`] shares the same map. Lookups hold whichever of
    /// the two indexes' entries is the newer observation, so a hard-link
    /// alias's newer record is never shadowed by the stale path entry of
    /// the spelling that happens to be asked. Residual, mirroring the
    /// `TargetIdentity` field docs: a delete-and-recreate that reclaims
    /// the recorded device-inode pair arms a guard for a file the model
    /// never touched — a false refusal, never a false pass, since the
    /// guard passes only on a hash match with content the model actually
    /// observed.
    pub file_identities: Arc<Mutex<crate::state::FileIdentities>>,

    /// A retained descriptor for the workspace's resolved root, opened
    /// when this context was constructed.
    ///
    /// Contained walks start from a duplicate of this descriptor rather
    /// than reopening the anchor path, so a symlink swapped onto the
    /// workspace spelling after construction cannot redirect them: the
    /// starting directory is the one the operator's spelling resolved to
    /// at construction time. Contained reads verify opened handles
    /// against this descriptor's true location the same way, so a swap
    /// cannot turn a read into a byte source outside the pinned
    /// workspace. When the workspace could not be opened at construction
    /// time the anchor retains no descriptor and contained operations
    /// fail closed. A swap can still skew *validation*: path resolution
    /// judges the workspace spelling's current target while walks and
    /// handle checks judge the pinned root — the divergence is fail-safe,
    /// costing a refusal, never an escape. Cloning [`RunnerContext`]
    /// shares the same descriptor.
    pub(crate) workspace_anchor: Arc<WorkspaceAnchor>,

    /// Whether file tools confine paths to [`cwd`](Self::cwd).
    ///
    /// [`ResolvePolicy::Contained`] (the default) rejects paths that escape
    /// the working directory; [`ResolvePolicy::Unrestricted`] lets tools
    /// reach any path the OS permits. Set from the config/CLI `unsafe_paths`
    /// switch at runner construction.
    pub resolve_policy: ResolvePolicy,
}

impl RunnerContext {
    /// Create a context for `cwd` with an empty todo list, no question
    /// channel, no recorded file baselines, and contained path resolution.
    ///
    /// A relative `cwd` is anchored to the process's current directory.
    /// [`resolve_path`](crate::util::resolve_path) decides containment by
    /// comparing lexical prefixes, and a bare `.` normalizes to nothing —
    /// left un-anchored, it would reject every relative target. `.` and `..`
    /// are collapsed lexically — a leftover `..` would break the pinned
    /// write's prefix matching against this path — while symlinks are not
    /// resolved, matching the lexical philosophy applied to targets. On the
    /// rare failure of the current-directory probe, `cwd` is stored as
    /// given.
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
        let cwd = crate::util::normalize_lexical(&cwd);
        let workspace_anchor = Arc::new(WorkspaceAnchor::pin(&cwd));
        Self {
            cwd,
            todos: Arc::new(Mutex::new(Vec::new())),
            question_tx: Arc::new(Mutex::new(None)),
            file_baselines: Arc::new(Mutex::new(FileBaselines::default())),
            file_identities: Arc::new(Mutex::new(crate::state::FileIdentities::default())),
            workspace_anchor,
            resolve_policy: ResolvePolicy::default(),
        }
    }

    /// Set the path-containment policy the file tools resolve under.
    ///
    /// Builder-style companion to [`new`](Self::new), used by the runner
    /// wiring to lift the configured `unsafe_paths` switch onto the context.
    /// Contained resolution is the default; only an explicit opt-out
    /// produces [`ResolvePolicy::Unrestricted`].
    #[must_use]
    pub fn with_resolve_policy(mut self, resolve_policy: ResolvePolicy) -> Self {
        self.resolve_policy = resolve_policy;
        self
    }

    /// Record an observation as the model's latest known state of `path`.
    ///
    /// `path` is normalized to the map's key form before storing (see
    /// [`RunnerContext::file_baselines`]), so a record made through a
    /// symlinked-directory spelling of a just-created file lands on the same
    /// key a later lookup through the physical spelling produces. Thin
    /// locking wrapper over [`record`](crate::state::record), which owns the
    /// ordering semantics (newest observation wins; an older one arriving
    /// out of order is discarded).
    pub(crate) fn record_baseline(&self, path: &Path, baseline: crate::state::FileBaseline) {
        let key = self.baseline_map_key(path);
        if self.resolve_policy == ResolvePolicy::Unrestricted
            && let Some(identity) = identity_of(path)
        {
            let mut identities = self
                .file_identities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::state::record_identity(&mut identities, identity, baseline);
        }
        let mut baselines = self
            .file_baselines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::state::record(&mut baselines, &key, baseline);
    }

    /// The content hash recorded for `path`, if the path was touched.
    ///
    /// `path` is normalized with the same rule [`record_baseline`](Self::record_baseline)
    /// applies, so a lookup through any spelling of a file finds the
    /// baseline regardless of which spelling recorded it. Under
    /// [`ResolvePolicy::Unrestricted`] the lookup consults both indexes —
    /// the path key and the file's stat identity, since hard-link aliases
    /// are distinct path keys over one physical file — and holds whichever
    /// entry carries the newer observation, mirroring the record rule.
    /// Thin locking wrapper over the accessors in [`crate::state`].
    pub(crate) fn baseline_for(&self, path: &Path) -> Option<u64> {
        let key = self.baseline_map_key(path);
        let by_path = {
            let baselines = self
                .file_baselines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::state::entry(&baselines, &key)
        };
        if self.resolve_policy != ResolvePolicy::Unrestricted {
            return by_path.map(|baseline| baseline.hash);
        }
        let by_identity = identity_of(path).and_then(|identity| {
            let identities = self
                .file_identities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::state::entry_identity(&identities, identity)
        });
        let newest = match (by_path, by_identity) {
            (Some(path_entry), Some(identity_entry)) => Some({
                if identity_entry.observed > path_entry.observed {
                    identity_entry
                } else {
                    path_entry
                }
            }),
            (only, None) | (None, only) => only,
        };
        newest.map(|baseline| baseline.hash)
    }

    /// The baseline map key for `path`, per the run's resolve policy.
    ///
    /// Under [`ResolvePolicy::Contained`] the key is the path as given —
    /// the tools' contained resolution output — so records and lookups stay
    /// in sync without filesystem probes. Under
    /// [`ResolvePolicy::Unrestricted`] the key is the canonicalized
    /// physical file, falling back to the path as given when the probe
    /// fails (a file removed again between its write and this call); this
    /// is what lets a file first created through a symlinked directory be
    /// re-keyed to its now-resolvable referent.
    fn baseline_map_key(&self, path: &Path) -> PathBuf {
        match self.resolve_policy {
            ResolvePolicy::Contained => path.to_path_buf(),
            ResolvePolicy::Unrestricted => {
                std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
            }
        }
    }
}

/// The stat identity (device, inode) of the file `path` reaches, if the
/// platform exposes one and the file exists.
///
/// The stat follows symbolic links, so the identity is the referent's —
/// hard-link aliases, which canonicalization cannot unify, stat to the
/// same pair and share one baseline. `None` covers the two cases where no
/// identity can be recorded and callers fall back to path keys alone: the
/// path does not resolve (nothing exists to guard yet), and the record
/// side simply skips the identity index while the lookup side reports a
/// miss.
///
/// Used by [`RunnerContext::record_baseline`] and
/// [`RunnerContext::baseline_for`](Self::baseline_for) under the
/// unrestricted policy only.
#[cfg(unix)]
fn identity_of(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// The identity of the file `path` reaches, on a platform without a stable
/// stat identity.
///
/// Always `None`: there is nothing to key the identity index with, so
/// records and lookups degrade to path keys alone and the hard-link alias
/// guard is absent — consistent with this platform's other degraded
/// checks, which fail closed where containment is at stake and merely
/// narrow protection where it is not.
#[cfg(not(unix))]
fn identity_of(path: &Path) -> Option<(u64, u64)> {
    let _ = path;
    None
}

impl fmt::Debug for RunnerContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let has_channel = self
            .question_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        let baselines = self
            .file_baselines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let identities = self
            .file_identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        f.debug_struct("RunnerContext")
            .field("cwd", &self.cwd)
            .field("todos", &self.todos)
            .field("question_tx", &has_channel)
            .field("file_baselines", &baselines)
            .field("file_identities", &identities)
            .field("workspace_anchor", &self.workspace_anchor)
            .field("resolve_policy", &self.resolve_policy)
            .finish()
    }
}

/// Downcast the `ToolContext` extension to a [`RunnerContext`] reference.
///
/// Returns `None` when no `RunnerContext` extension is installed on `ctx`;
/// callers should handle that case rather than unwrapping.
///
/// # Examples
///
/// ```
/// use dch_tools::RunnerContext;
/// use dch_tools::runner_ctx;
///
/// let mut ctx = loopctl::tool::ToolContext::default();
/// assert!(runner_ctx(&ctx).is_none());
///
/// ctx.set_extension(RunnerContext::new(".".into()));
/// assert!(runner_ctx(&ctx).is_some());
/// ```
#[must_use]
pub fn runner_ctx(ctx: &loopctl::tool::ToolContext) -> Option<&RunnerContext> {
    ctx.get_extension::<RunnerContext>()
}

/// Extract the working directory from an optional [`RunnerContext`].
///
/// Every tool's dispatch starts with this resolution: the extension is
/// installed by the runner before each tool call, so its absence means the
/// tool ran outside a dch-composed pipeline.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] naming the missing extension when
/// `runner_context` is `None`.
pub fn require_cwd(runner_context: Option<RunnerContext>) -> Result<PathBuf, ToolError> {
    runner_context.map(|rc| rc.cwd).ok_or_else(|| {
        ToolError::Execution(
            "RunnerContext extension is not installed on the ToolContext".to_string(),
        )
    })
}

/// Statically asserts `RunnerContext: Send + Sync`, the bound required to store it as a `ToolContext` extension.
///
/// `Arc<Mutex<Vec<TodoEntry>>>` is `Send + Sync`,
/// `Arc<Mutex<Option<mpsc::Sender<_>>>>` is `Send + Sync`, and `PathBuf` is
/// trivially so.
const _: fn() = || {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<RunnerContext>();
};

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::expect_used,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use crate::question::Question;
    use crate::question::QuestionOption;
    use crate::question::QuestionRequest;
    use crate::todo::TodoEntry;
    use crate::todo::TodoStatus;
    use loopctl::tool::ToolContext;

    fn sample() -> RunnerContext {
        RunnerContext::new(PathBuf::from("/tmp/workspace"))
    }

    #[test]
    fn new_makes_a_relative_cwd_absolute() {
        let rc = RunnerContext::new(PathBuf::from("."));
        assert!(
            rc.cwd.is_absolute(),
            "a bare `.` must anchor to the process cwd: {:?}",
            rc.cwd
        );
        assert_eq!(rc.cwd, std::env::current_dir().unwrap());
    }

    #[test]
    fn a_relative_cwd_resolves_relative_targets() {
        let rc = RunnerContext::new(PathBuf::from("."));
        let resolved =
            crate::util::resolve_path("src/main.rs", &rc.cwd, ResolvePolicy::Contained).unwrap();
        assert!(
            resolved.is_absolute(),
            "the target must resolve against the anchored cwd: {resolved:?}"
        );
        assert!(resolved.ends_with(Path::new("src/main.rs")));
    }

    #[test]
    fn the_resolve_policy_round_trips_through_the_extension() {
        // The runner wiring lifts the configured unsafe_paths switch onto
        // the context; tools must read the same policy back out.
        let mut ctx = ToolContext::default();
        ctx.set_extension(sample().with_resolve_policy(ResolvePolicy::Unrestricted));
        let rc = runner_ctx(&ctx).expect("extension was set");
        assert_eq!(
            rc.resolve_policy,
            ResolvePolicy::Unrestricted,
            "the policy must survive the extension round trip"
        );
    }

    #[test]
    fn debug_reports_the_resolve_policy() {
        let unrestricted = sample().with_resolve_policy(ResolvePolicy::Unrestricted);
        let rendered = format!("{unrestricted:?}");
        assert!(
            rendered.contains("resolve_policy: Unrestricted"),
            "Debug should carry the policy: {rendered:?}"
        );
    }

    #[test]
    fn a_new_context_defaults_to_contained_resolution() {
        assert_eq!(sample().resolve_policy, ResolvePolicy::Contained);
    }

    #[test]
    fn extension_roundtrip() {
        let mut ctx = ToolContext::default();
        ctx.set_extension(sample());
        let got = ctx.get_extension::<RunnerContext>();
        assert!(got.is_some());
        assert_eq!(
            got.map(|r| r.cwd.clone()).unwrap_or_default(),
            PathBuf::from("/tmp/workspace")
        );
    }

    #[test]
    fn runner_ctx_present() {
        let mut ctx = ToolContext::default();
        ctx.set_extension(sample());
        let rc = runner_ctx(&ctx).expect("extension was set");
        assert_eq!(rc.cwd, PathBuf::from("/tmp/workspace"));
    }

    #[test]
    fn runner_ctx_absent() {
        let ctx = ToolContext::default();
        assert!(runner_ctx(&ctx).is_none());
    }

    #[test]
    fn shared_todos_visible_across_clones() {
        let rc = sample();
        let twin = rc.clone();
        rc.todos
            .lock()
            .expect("todos lock not poisoned")
            .push(TodoEntry {
                id: "1".to_string(),
                subject: "Ship it".to_string(),
                description: String::new(),
                status: TodoStatus::Pending,
                active_form: None,
            });
        let observed = twin.todos.lock().expect("todos lock not poisoned").len();
        assert_eq!(observed, 1);
    }

    #[test]
    fn todos_default_empty_and_can_be_cleared_to_reset_run() {
        // The per-run reset is a clear of the vec. Verify the default is empty
        // and that clearing works (the runner does this at the top of each run).
        let rc = sample();
        assert!(rc.todos.lock().expect("todos lock not poisoned").is_empty());
        rc.todos
            .lock()
            .expect("todos lock not poisoned")
            .push(TodoEntry {
                id: "x".to_string(),
                subject: "task".to_string(),
                description: String::new(),
                status: TodoStatus::Pending,
                active_form: None,
            });
        rc.todos.lock().expect("todos lock not poisoned").clear();
        assert!(rc.todos.lock().expect("todos lock not poisoned").is_empty());
    }

    #[test]
    fn question_tx_round_trips_a_request_and_empty_is_default() {
        // Default is an empty slot (headless); once a sender is installed, a
        // sent QuestionRequest reaches the receiver. This is the plumbing the
        // asking tool uses.
        assert!(sample().question_tx.lock().expect("slot").is_none());

        let (tx, rx) = mpsc::channel::<QuestionRequest>();
        let rc = RunnerContext {
            question_tx: Arc::new(Mutex::new(Some(tx))),
            ..sample()
        };
        let twin = rc.clone();
        // The cloned context shares the slot — senders cloned from it reach
        // the same receiver.
        let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
        let first = rc
            .question_tx
            .lock()
            .expect("slot")
            .clone()
            .expect("sender set");
        first
            .send(QuestionRequest {
                questions: vec![Question {
                    question: "ok?".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "Yes".to_string(),
                        description: None,
                    }],
                    multi_select: false,
                    response_tx: resp_tx,
                }],
            })
            .expect("receiver alive");
        let second = twin
            .question_tx
            .lock()
            .expect("slot on clone")
            .clone()
            .expect("sender set on clone");
        second
            .send(QuestionRequest {
                questions: Vec::new(),
            })
            .expect("receiver still alive");
        // try_recv (non-blocking) twice: confirms both sends arrived without
        // hanging on iter(), which would block forever waiting for senders to
        // drop.
        assert!(rx.try_recv().is_ok(), "first send missing");
        assert!(rx.try_recv().is_ok(), "second send missing");
        assert!(rx.try_recv().is_err(), "more than two sends arrived");
    }

    #[test]
    fn debug_reports_channel_presence_not_the_channel() {
        // The slot holds an mpsc::Sender, which is not Debug; the Debug impl
        // must report presence as a bool rather than the channel itself.
        let with_tx = {
            let (tx, _rx) = mpsc::channel::<QuestionRequest>();
            RunnerContext {
                question_tx: Arc::new(Mutex::new(Some(tx))),
                ..sample()
            }
        };
        let without_tx = sample();
        assert!(
            format!("{with_tx:?}").contains("question_tx: true"),
            "Debug should report question_tx present"
        );
        assert!(
            format!("{without_tx:?}").contains("question_tx: false"),
            "Debug should report question_tx absent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn baseline_keys_meet_through_symlink_spellings_under_unrestricted() {
        // The key rule lives here, not at the tool call sites: a record
        // made through a symlinked-directory spelling and a lookup through
        // the physical spelling must land on one key, so the staleness
        // guard survives a spelling change — including for a file first
        // created through the link, whose path could not be canonicalized
        // before it existed.
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let realdir = tmp.path().join("realdir");
        std::fs::create_dir(&realdir).unwrap();
        let linkdir = tmp.path().join("linkdir");
        symlink(&realdir, &linkdir).unwrap();

        let rc = RunnerContext::new(tmp.path().to_path_buf())
            .with_resolve_policy(ResolvePolicy::Unrestricted);
        // Records happen after a successful write, so the referent exists
        // by the time the key is computed — create it to reproduce that
        // state; this is what makes the linkdir spelling resolvable.
        std::fs::write(realdir.join("new.rs"), b"v1").unwrap();
        rc.record_baseline(&linkdir.join("new.rs"), crate::state::observe_bytes(b"v1"));
        assert_eq!(
            rc.baseline_for(&realdir.join("new.rs")),
            Some(crate::state::content_hash(b"v1")),
            "the lookup must normalize to the recorded physical key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn contained_baseline_keys_stay_as_given() {
        // Contained keys are the tools' contained-resolution output; the
        // map must not canonicalize them, so a symlinked spelling and the
        // physical spelling are distinct keys exactly as they are distinct
        // resolution results.
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let realdir = tmp.path().join("realdir");
        std::fs::create_dir(&realdir).unwrap();
        let linkdir = tmp.path().join("linkdir");
        symlink(&realdir, &linkdir).unwrap();

        let rc = RunnerContext::new(tmp.path().to_path_buf());
        rc.record_baseline(&linkdir.join("a.rs"), crate::state::observe_bytes(b"A"));
        assert_eq!(
            rc.baseline_for(&linkdir.join("a.rs")),
            Some(crate::state::content_hash(b"A")),
            "the recorded spelling must find its own baseline"
        );
        assert_eq!(
            rc.baseline_for(&realdir.join("a.rs")),
            None,
            "the physical spelling is a distinct key under Contained"
        );
    }

    #[cfg(unix)]
    #[test]
    fn baseline_lookup_holds_the_newest_observation_across_hard_link_aliases() {
        // Hard-link aliases are two distinct path keys over one physical
        // file: canonicalization unifies symlinks, not links. The lookup
        // must hold the newest observation of the file, whichever spelling
        // recorded it — not the entry of the spelling that was asked.
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.txt");
        let hard = tmp.path().join("hard.txt");
        std::fs::write(&real, b"v1").unwrap();
        std::fs::hard_link(&real, &hard).unwrap();

        let rc = RunnerContext::new(tmp.path().to_path_buf())
            .with_resolve_policy(ResolvePolicy::Unrestricted);
        rc.record_baseline(&real, crate::state::observe_bytes(b"v1"));
        rc.record_baseline(&hard, crate::state::observe_bytes(b"EXT"));

        assert_eq!(
            rc.baseline_for(&real),
            Some(crate::state::content_hash(b"EXT")),
            "the alias's newer observation must supersede the stale path entry"
        );
        assert_eq!(
            rc.baseline_for(&hard),
            Some(crate::state::content_hash(b"EXT"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn baseline_lookup_takes_the_newest_across_aliases_in_both_insert_orders() {
        // Mirror of the map's out-of-order pins: whichever spelling records
        // the older observation first, the newer one wins for every
        // spelling of the file.
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.txt");
        let hard = tmp.path().join("hard.txt");
        std::fs::write(&real, b"v1").unwrap();
        std::fs::hard_link(&real, &hard).unwrap();

        let older = crate::state::observe_bytes(b"v1");
        let newer = crate::state::observe_bytes(b"EXT");

        let rc = RunnerContext::new(tmp.path().to_path_buf())
            .with_resolve_policy(ResolvePolicy::Unrestricted);
        rc.record_baseline(&hard, older);
        rc.record_baseline(&real, newer);
        assert_eq!(
            rc.baseline_for(&hard),
            Some(newer.hash),
            "newer through the physical spelling must win on the alias"
        );

        let rc = RunnerContext::new(tmp.path().to_path_buf())
            .with_resolve_policy(ResolvePolicy::Unrestricted);
        rc.record_baseline(&real, older);
        rc.record_baseline(&hard, newer);
        assert_eq!(
            rc.baseline_for(&real),
            Some(newer.hash),
            "newer through the alias must win on the physical spelling"
        );
    }
}
