//! Process-global LRU cache of compiled regexes.
//!
//! Tools that accept user-supplied regex patterns (`Grep`, `CodeSearch`)
//! would otherwise recompile the same pattern on every call. This module
//! caches up to 256 compiled regexes process-globally, with
//! least-recently-used eviction.
//!
//! `regex::Regex` is `Send + Sync` and clones cheaply (it is `Arc`-backed
//! internally), so the cache hands out clones and never holds its lock across
//! the actual search. That is what makes the calling tools safe to mark
//! concurrency-safe despite sharing global state: the shared state is a
//! lock-protected lookup table, not mutable file or session state.

use std::sync::Mutex;

use regex::Regex;

/// Maximum number of compiled regexes kept in the cache.
///
/// Each compiled regex uses roughly 1–10 `KiB`, so 256 entries caps the cache
/// at ~256 `KiB`–2.5 `MiB` — ample for the small set of patterns a single
/// agent run tends to repeat, small enough to be forgettable.
const MAX_CACHE_SIZE: usize = 256;

/// The process-global cache.
///
/// Newest entries live at the back; eviction removes the front. `Mutex::new`
/// is `const` since Rust 1.63, so no lazy initialization is needed.
static REGEX_CACHE: Mutex<Vec<(String, Regex)>> = Mutex::new(Vec::new());

/// Return a compiled regex for `pattern`, fetching from the cache when able.
///
/// On a cache hit the entry is moved to the back (most-recently-used). On a
/// miss the regex is compiled, stored at the back (evicting the front if the
/// cache is full), and returned. A poisoned lock falls back to compiling
/// without caching — callers still get a working regex.
///
/// # Errors
///
/// Returns the underlying [`regex::Error`] unchanged if `pattern` fails to
/// compile. Invalid patterns are never stored in the cache.
pub fn get_or_compile(pattern: &str) -> Result<Regex, regex::Error> {
    if let Some(hit) = lookup_cached(pattern) {
        return Ok(hit);
    }

    let regex = Regex::new(pattern)?;
    insert_cached(pattern.to_string(), regex.clone());
    Ok(regex)
}

/// Return a case-insensitive compiled regex for `pattern`.
///
/// Wraps `pattern` with the `(?i)` flag (idempotent — if the pattern already
/// begins with `(?i)`, no second prefix is added) and then delegates to
/// [`get_or_compile`]. The wrapped form is what gets cached, so a subsequent
/// case-sensitive call with the same body does not collide with this entry.
///
/// # Errors
///
/// Returns the underlying [`regex::Error`] if `pattern` fails to compile.
pub fn get_or_compile_case_insensitive(pattern: &str) -> Result<Regex, regex::Error> {
    let cached_pattern = if pattern.starts_with("(?i)") {
        pattern.to_string()
    } else {
        format!("(?i){pattern}")
    };

    get_or_compile(&cached_pattern)
}

/// Drop every cached entry.
///
/// Intended for tests that need a known cache state, and for runtime memory
/// pressure (the cache is bounded, so calling this is rarely necessary in
/// production).
pub fn clear_cache() {
    if let Ok(mut cache) = REGEX_CACHE.lock() {
        cache.clear();
    }
}

/// Cache-hit path: find `pattern`, move it to the back, return a clone.
///
/// Returns `None` on a miss, a poisoned lock, or an empty cache. Holding the
/// regex across the lock release is safe because the clone is Arc-backed.
fn lookup_cached(pattern: &str) -> Option<Regex> {
    let mut cache = REGEX_CACHE.lock().ok()?;
    let idx = cache.iter().rposition(|(p, _)| p == pattern)?;
    let (_, regex) = cache.remove(idx);
    let cloned = regex.clone();
    cache.push((pattern.to_string(), regex));
    Some(cloned)
}

/// Cache-miss path: store `regex` under `pattern`, evicting the front entry
/// if the cache is at capacity.
fn insert_cached(pattern: String, regex: Regex) {
    let Ok(mut cache) = REGEX_CACHE.lock() else {
        return;
    };
    if cache.len() >= MAX_CACHE_SIZE {
        // Vec::remove(0) is O(n); fine at n=256, and only on the miss path.
        cache.remove(0);
    }
    cache.push((pattern, regex));
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
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    pub fn cache_size() -> usize {
        REGEX_CACHE.lock().map_or(0, |c| c.len())
    }

    /// Build a marker string unique across parallel test threads.
    ///
    /// `std::process::id()` is constant across threads in one process; the
    /// counter makes each call distinct even when tests run in parallel.
    fn marker(label: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}_{}_{}",
            label,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        )
    }

    #[test]
    fn cache_hit_returns_matching_regex() {
        clear_cache();
        let m = marker("hit");
        let re1 = get_or_compile(&m).unwrap();
        let re2 = get_or_compile(&m).unwrap();
        assert!(re1.is_match(&m));
        assert!(re2.is_match(&m));
    }

    #[test]
    fn cache_miss_compiles_distinct_patterns() {
        let m1 = marker("miss_a");
        let m2 = marker("miss_b");
        let re1 = get_or_compile(&m1).unwrap();
        let re2 = get_or_compile(&m2).unwrap();
        assert!(re1.is_match(&m1));
        assert!(re2.is_match(&m2));
        assert!(!re1.is_match(&m2));
    }

    #[test]
    fn cache_evicts_at_capacity() {
        for i in 0..(MAX_CACHE_SIZE + 50) {
            let p = format!("eviction_{}_{}", std::process::id(), i);
            let _ = get_or_compile(&p).unwrap();
        }
        let newest = format!("eviction_{}_{}", std::process::id(), MAX_CACHE_SIZE + 49);
        let re = get_or_compile(&newest).unwrap();
        assert!(re.is_match(&newest));
    }

    #[test]
    fn cache_lru_reorders_on_access() {
        clear_cache();
        let first = marker("lru_first");
        let _second = marker("lru_second");
        let _third = marker("lru_third");
        // Re-access "first" — it should still resolve (LRU kept it).
        let re = get_or_compile(&first).unwrap();
        assert!(re.is_match(&first));
    }

    #[test]
    fn case_insensitive_matches_uppercase() {
        let m = marker("ci");
        let re = get_or_compile_case_insensitive(&m).unwrap();
        assert!(re.is_match(&m.to_uppercase()));
        assert!(!re.is_match("no_match_xyz"));
    }

    #[test]
    fn case_insensitive_is_idempotent() {
        let m = marker("ci_idem");
        let wrapped = format!("(?i){m}");
        // Both forms should compile and match case-insensitively.
        let re_plain = get_or_compile_case_insensitive(&m).unwrap();
        let re_prefixed = get_or_compile_case_insensitive(&wrapped).unwrap();
        assert!(re_plain.is_match(&m.to_uppercase()));
        assert!(re_prefixed.is_match(&m.to_uppercase()));
    }

    #[test]
    fn invalid_regex_errors() {
        let result = get_or_compile("(unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn clear_cache_empties_state() {
        let m = marker("clear");
        let _ = get_or_compile(&m).unwrap();
        assert!(cache_size() > 0);
        clear_cache();
        assert_eq!(cache_size(), 0);
    }
}
