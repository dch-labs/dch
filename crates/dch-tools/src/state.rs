//! Per-session record of the model's latest known content per touched file.
//!
//! The Write tool's detect-on-write conflict check compares the target's
//! current content hash against the hash recorded when the path was last
//! touched; this module supplies that record. Read records what it observes;
//! a successful Write/Edit/MultiEdit records the post-write content, so the
//! model's own writes never register as external changes. The map holds one
//! entry per path, and the entry is always the *newest* observation: each
//! touch carries a sequence number stamped when its bytes were in hand, and
//! an older observation never supersedes a newer one, whichever insert lands
//! last.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Monotonic observation counter, ordering concurrent touches of one path.
///
/// Stamped by [`observe_bytes`] at the moment the bytes are in hand; the
/// process-local counter never repeats a value within a session, so the
/// stamp order matches the observation order exactly.
static OBSERVATION_SEQ: AtomicU64 = AtomicU64::new(0);

/// The model's latest known content hash per touched file.
///
/// Keyed by each tool's resolution of the submitted path: the
/// [`resolve_path`](crate::util::resolve_path) output under the contained
/// policy, and the canonicalized physical file on top of it under the
/// unrestricted policy. Under Unrestricted, then, equivalent spellings of
/// the same file — including spellings that pass through a symbolic link —
/// share one baseline and a staleness check can't be dodged by re-spelling
/// a path. Under Contained the keys are the lexical resolution output, so
/// alias spellings are deliberately distinct keys; there the protection
/// comes from the write layer refusing symbolic-link spellings rather than
/// from key merging. The rule is applied by the owning context —
/// `RunnerContext`'s `record_baseline` and `baseline_for` normalize keys
/// on the way in and out, so no call site can record or query under a
/// divergent spelling, and a file first created through a symlinked
/// directory is re-keyed to its now-resolvable physical path.
///
/// The hash is a process-local [`DefaultHasher`] fingerprint of the file's
/// bytes — deliberately not a stable or cryptographic digest: baselines live
/// and die with the session, and the hash only ever answers "did the bytes
/// change since the model last touched this file".
pub type FileBaselines = BTreeMap<PathBuf, FileBaseline>;

/// One observation of a file's content, at a known point in the session's
/// observation order.
///
/// Produced by `observe_bytes` at the moment the bytes were in hand; a
/// map entry always holds the newest one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileBaseline {
    /// Where this observation sits in the session's touch order.
    ///
    /// Stamped when the bytes were in hand; `record` uses it so concurrent
    /// touches of one path resolve to the newest observation regardless of
    /// the order their inserts land.
    pub observed: u64,

    /// Content hash of the observed bytes.
    ///
    /// Process-local fingerprint (see `content_hash`); compared against
    /// the target's current content at the next write.
    pub hash: u64,
}

/// The process-local content hash of `bytes`.
///
/// Deliberately not a stable or cryptographic digest (see [`FileBaselines`]):
/// the hash only ever answers, within one session, "did the bytes change
/// since the model last touched this file".
pub(crate) fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Observe `bytes`: stamp the session's touch order and hash the content.
///
/// Call this at the moment the bytes are in hand — the stamp is what makes
/// the baseline the *newest* observation rather than merely the last insert.
pub(crate) fn observe_bytes(bytes: &[u8]) -> FileBaseline {
    FileBaseline {
        observed: OBSERVATION_SEQ.fetch_add(1, Ordering::Relaxed),
        hash: content_hash(bytes),
    }
}

/// Record an observation as the model's latest known state of `path`.
///
/// An observation with a lower sequence than the recorded one arrived out of
/// order (concurrent touches of one path) and is discarded, so the entry
/// always reflects the newest observation.
pub(crate) fn record(baselines: &mut FileBaselines, path: &Path, baseline: FileBaseline) {
    match baselines.get(path) {
        Some(existing) if existing.observed > baseline.observed => return,
        _ => {}
    }
    baselines.insert(path.to_path_buf(), baseline);
}

/// The observation recorded for `path`, if the path was touched.
///
/// The stamp and hash together, for callers that consult both indexes and
/// must hold whichever observation is newer.
pub(crate) fn entry(baselines: &FileBaselines, path: &Path) -> Option<FileBaseline> {
    baselines.get(path).copied()
}

/// The model's latest known content per live file identity (unix device and
/// inode).
///
/// The unrestricted policy's second index over the same observations: keys
/// by path cannot unify aliases that canonicalization cannot see — two hard
/// links to one file are two equally canonical spellings — so the recorded
/// file's stat identity is kept alongside the path key, and a lookup stats
/// the target to find the baseline whichever spelling arrives. Kept in step
/// with [`FileBaselines`] by the same record calls. A lookup consults both
/// indexes and holds whichever of the two entries carries the newer
/// observation stamp: the path entry of one alias spelling can be older
/// than the identity entry, and letting it win would judge the file
/// against content the model has since superseded.
///
/// Residual, mirroring the `TargetIdentity` field docs on the rename gate:
/// an externally deleted-and-recreated file that reclaims the recorded
/// device-inode pair reads as an identity match, arming a staleness guard
/// for a file the model never touched. The direction is safe — a false
/// *refusal*, recovered by re-reading, never a false pass, because the
/// guard passes only when the current bytes hash-match the newest
/// observation the model actually made of this file.
pub type FileIdentities = BTreeMap<(u64, u64), FileBaseline>;

/// Record an observation as the model's latest known state of the file
/// `identity` names.
///
/// Mirrors [`record`]'s newest-observation-wins semantics with one entry
/// per live identity: a later observation of the same file through any
/// spelling supersedes the earlier one, so the entry always reflects the
/// newest content the model is known to have produced for that file,
/// however it is spelled. Called alongside [`record`] by the unrestricted
/// policy's record path.
pub(crate) fn record_identity(
    identities: &mut FileIdentities,
    identity: (u64, u64),
    baseline: FileBaseline,
) {
    match identities.get(&identity) {
        Some(existing) if existing.observed > baseline.observed => return,
        _ => {}
    }
    identities.insert(identity, baseline);
}

/// The observation recorded for the file `identity` names, if any.
///
/// The identity-side counterpart of [`entry`]: carries the stamp, so a
/// caller consulting both indexes can hold the newer of the two
/// observations instead of letting a stale path entry shadow it.
pub(crate) fn entry_identity(
    identities: &FileIdentities,
    identity: (u64, u64),
) -> Option<FileBaseline> {
    identities.get(&identity).copied()
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
    fn baseline_follows_the_latest_recorded_touch() {
        let mut baselines = FileBaselines::default();
        let a = Path::new("a.rs");
        let b = Path::new("b.rs");
        record(&mut baselines, a, observe_bytes(b"one"));
        record(&mut baselines, b, observe_bytes(b"two"));
        record(&mut baselines, a, observe_bytes(b"three"));

        assert_eq!(
            entry(&baselines, a).map(|obs| obs.hash),
            Some(content_hash(b"three")),
            "the latest touch wins"
        );
        assert_eq!(
            entry(&baselines, b).map(|obs| obs.hash),
            Some(content_hash(b"two")),
            "other paths keep their own baselines"
        );
        assert_eq!(
            entry(&baselines, Path::new("missing.rs")).map(|obs| obs.hash),
            None
        );
    }

    #[test]
    fn an_older_observation_never_supersedes_a_newer_one() {
        // Older is observed first, newer second — both stamps are generated,
        // and the inserts below land in both orders to model the
        // concurrent-insert race.
        let mut baselines = FileBaselines::default();
        let older = observe_bytes(b"old");
        let newer = observe_bytes(b"new");

        // Newer lands first; the older observation arrives last and must be
        // discarded.
        record(&mut baselines, Path::new("a.rs"), newer);
        record(&mut baselines, Path::new("a.rs"), older);
        assert_eq!(
            entry(&baselines, Path::new("a.rs")).map(|obs| obs.hash),
            Some(newer.hash),
            "newer first, older second: newest must stand"
        );

        let mut baselines = FileBaselines::default();
        record(&mut baselines, Path::new("a.rs"), older);
        record(&mut baselines, Path::new("a.rs"), newer);
        assert_eq!(
            entry(&baselines, Path::new("a.rs")).map(|obs| obs.hash),
            Some(newer.hash),
            "newer last, older first: newest must still stand"
        );
    }

    #[test]
    fn equal_bytes_hash_equal_and_different_bytes_differ() {
        assert_eq!(content_hash(b"same"), content_hash(b"same"));
        assert_ne!(content_hash(b"one"), content_hash(b"two"));
    }

    #[test]
    fn a_single_byte_flip_changes_the_hash() {
        assert_ne!(content_hash(b"v1"), content_hash(b"v2"));
    }
}
