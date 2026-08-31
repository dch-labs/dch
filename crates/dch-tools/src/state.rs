//! Per-session record of the model's latest known state per touched file.
//!
//! The Write tool's detect-on-write conflict check compares a target's
//! current `mtime` against the `mtime` recorded when the path was last
//! touched; this module supplies that record. Read records what it observes;
//! a successful Write/Edit/MultiEdit records the post-write `mtime`, so the
//! model's own writes never register as external changes. The map holds one
//! entry per path — the latest touch wins.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

/// The model's latest known `mtime` per touched file.
///
/// Keyed by the resolved absolute path (each tool's [`resolve_path`](crate::util::resolve_path)
/// output), so equivalent spellings of the same file share one baseline and a
/// staleness check can't be dodged by re-spelling a path. A value of `None`
/// means the path was touched but its `mtime` could not be measured (the
/// `stat` failed at read time); it clears any older baseline rather than
/// leaving a stale one to be trusted.
pub type FileBaselines = BTreeMap<PathBuf, Option<SystemTime>>;

/// Record `mtime` as the model's latest known state of `path`.
///
/// `path` is the resolved identity both the recording tool and the checking
/// tool compute for the file. A `None` `mtime` (the `stat` failed) overwrites
/// any older baseline — the newest observation wins, and an unmeasurable one
/// is no basis for a staleness verdict.
pub(crate) fn record(baselines: &mut FileBaselines, path: &Path, mtime: Option<SystemTime>) {
    baselines.insert(path.to_path_buf(), mtime);
}

/// The recorded baseline for `path`, when the latest touch measured an
/// `mtime`.
pub(crate) fn baseline(baselines: &FileBaselines, path: &Path) -> Option<SystemTime> {
    baselines.get(path).copied().flatten()
}

/// The file's `mtime`, or `None` when the `stat` fails for any reason.
///
/// Best-effort by contract: callers record what they could observe and treat
/// `None` as "no baseline", never as an error.
pub(crate) async fn current_mtime(path: &Path) -> Option<SystemTime> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|metadata| metadata.modified().ok())
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
        let epoch = SystemTime::UNIX_EPOCH;
        record(&mut baselines, Path::new("a.rs"), Some(epoch));
        record(
            &mut baselines,
            Path::new("b.rs"),
            Some(epoch + std::time::Duration::from_secs(5)),
        );
        record(
            &mut baselines,
            Path::new("a.rs"),
            Some(epoch + std::time::Duration::from_secs(10)),
        );

        assert_eq!(
            baseline(&baselines, Path::new("a.rs")),
            Some(epoch + std::time::Duration::from_secs(10)),
            "the latest touch wins"
        );
        assert_eq!(
            baseline(&baselines, Path::new("b.rs")),
            Some(epoch + std::time::Duration::from_secs(5)),
            "other paths keep their own baselines"
        );
        assert_eq!(baseline(&baselines, Path::new("missing.rs")), None);
    }

    #[test]
    fn an_unmeasured_touch_clears_the_baseline() {
        let mut baselines = FileBaselines::default();
        let epoch = SystemTime::UNIX_EPOCH;
        record(&mut baselines, Path::new("a.rs"), Some(epoch));
        record(&mut baselines, Path::new("a.rs"), None);

        assert_eq!(
            baseline(&baselines, Path::new("a.rs")),
            None,
            "an unmeasurable latest touch is no basis for a staleness verdict"
        );
    }

    #[tokio::test]
    async fn current_mtime_is_none_for_a_missing_path() {
        assert_eq!(
            current_mtime(Path::new("/nonexistent/dch-mtime-probe")).await,
            None
        );
    }
}
