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
/// Keyed by the resolved absolute path (each tool's [`resolve_path`](crate::util::resolve_path)
/// output), so equivalent spellings of the same file share one baseline and a
/// staleness check can't be dodged by re-spelling a path.
///
/// The hash is a process-local [`DefaultHasher`] fingerprint of the file's
/// bytes — deliberately not a stable or cryptographic digest: baselines live
/// and die with the session, and the hash only ever answers "did the bytes
/// change since the model last touched this file".
pub type FileBaselines = BTreeMap<PathBuf, FileBaseline>;

/// One observation of a file's content, at a known point in the session's
/// observation order.
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

/// The content hash recorded for `path`, if the path was touched.
///
/// `None` means the path has never been touched this session — Write then
/// proceeds unchecked, per the no-baseline rule.
pub(crate) fn baseline(baselines: &FileBaselines, path: &Path) -> Option<u64> {
    baselines.get(path).map(|baseline| baseline.hash)
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
            baseline(&baselines, a),
            Some(content_hash(b"three")),
            "the latest touch wins"
        );
        assert_eq!(
            baseline(&baselines, b),
            Some(content_hash(b"two")),
            "other paths keep their own baselines"
        );
        assert_eq!(baseline(&baselines, Path::new("missing.rs")), None);
    }

    #[test]
    fn an_older_observation_never_supersedes_a_newer_one() {
        let mut baselines = FileBaselines::default();
        let newer = observe_bytes(b"new");
        let mut older = observe_bytes(b"old");
        older.observed = newer.observed - 1;

        // The older observation arrives last (the concurrent-insert case).
        record(&mut baselines, Path::new("a.rs"), older);
        record(&mut baselines, Path::new("a.rs"), newer);
        assert_eq!(
            baseline(&baselines, Path::new("a.rs")),
            Some(newer.hash),
            "newer first, older second: newest must stand"
        );

        let mut baselines = FileBaselines::default();
        record(&mut baselines, Path::new("a.rs"), newer);
        record(&mut baselines, Path::new("a.rs"), older);
        assert_eq!(
            baseline(&baselines, Path::new("a.rs")),
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
