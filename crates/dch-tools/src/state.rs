//! Per-session record of the model's latest known content per touched file.
//!
//! The Write tool's detect-on-write conflict check compares the target's
//! current content hash against the hash recorded when the path was last
//! touched; this module supplies that record. Read records what it observes;
//! a successful Write/Edit/MultiEdit records the post-write content, so the
//! model's own writes never register as external changes. The map holds one
//! entry per path — the latest touch wins.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::path::PathBuf;

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
pub type FileBaselines = BTreeMap<PathBuf, u64>;

/// Fingerprint `bytes` for baseline comparison.
///
/// Process-local by design (see [`FileBaselines`]): the same bytes always
/// produce the same hash within a session, which is the only guarantee the
/// staleness check needs.
pub(crate) fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Record `hash` as the model's latest known state of `path`.
///
/// Called after every successful touch: Read records the hash of the bytes
/// it served, the writing tools record the hash of the content they wrote —
/// which is why the model's own writes never register as external changes.
/// Inserting an existing key overwrites, so the map stays one-entry-per-file.
pub(crate) fn record(baselines: &mut FileBaselines, path: &Path, hash: u64) {
    baselines.insert(path.to_path_buf(), hash);
}

/// The recorded baseline for `path`, if the path was touched.
///
/// `None` means either an unknown path or no check to make: with no recorded
/// touch there is nothing to compare the file's current content against, and
/// the caller lets the write proceed.
pub(crate) fn baseline(baselines: &FileBaselines, path: &Path) -> Option<u64> {
    baselines.get(path).copied()
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
        record(&mut baselines, a, content_hash(b"one"));
        record(&mut baselines, b, content_hash(b"two"));
        record(&mut baselines, a, content_hash(b"three"));

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
    fn equal_bytes_hash_equal_and_different_bytes_differ() {
        assert_eq!(content_hash(b"same"), content_hash(b"same"));
        assert_ne!(content_hash(b"one"), content_hash(b"two"));
    }

    #[test]
    fn a_single_byte_flip_changes_the_hash() {
        assert_ne!(content_hash(b"v1"), content_hash(b"v2"));
    }
}
