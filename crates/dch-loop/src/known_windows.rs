//! Context windows for the models whose provider publishes none.
//!
//! The live probe asks the session's own server; the cloud providers
//! answer neither question — their model listings carry ids and dates
//! and nothing else — so a session on one of them lands on the
//! fallback window unless someone states a number. This table is the
//! someone: the documented windows of the models providers describe in
//! prose and refuse to describe on the wire. It is consulted only
//! after the live probe has no answer, so a server that does publish
//! the session model's window always wins over the table.
//!
//! Entries carry the model id exactly as its provider's listing spells
//! it, compared case-insensitively — a typed `GLM-5.3` is the same
//! model — and the window is the full budget one request may carry. A
//! model absent from the table is deliberately absent: silence keeps
//! the fallback rather than a guessed number.

/// The known models and their documented per-request windows.
///
/// Each entry's comment cites the provider page stating the number.
/// Keep the table to windows a provider states outright — an
/// approximate window mis-sizes compaction more than the fallback
/// does, and a wrong entry outlives its source.
const KNOWN_WINDOWS: &[(&str, u64)] = &[
    // Z.AI's model pages: "Context Length: 1M" for each of these.
    // https://docs.z.ai/guides/llm/glm-5.2
    ("glm-5.2", 1_000_000),
    // https://docs.z.ai/guides/llm/glm-5.3
    ("glm-5.3", 1_000_000),
    // https://docs.z.ai/guides/vlm/glm-5.3-flash
    ("glm-5.3-flash", 1_000_000),
    ("glm-5.3-flashx", 1_000_000),
];

/// The documented window for `model`, if the table knows it.
///
/// Exact id match, case-insensitive — no prefixes, so a sibling with a
/// longer name never inherits its family's entry. `None` is the honest
/// answer for an unknown model: the caller keeps its fallback window.
pub(crate) fn known_model_window(model: &str) -> Option<u64> {
    KNOWN_WINDOWS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(model))
        .map(|(_, window)| *window)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]
mod tests {
    use super::*;

    #[test]
    fn the_table_answers_for_each_entry_it_carries() {
        // A stale entry that no longer matches its own id would be
        // dead weight the moment it lands; every row answers.
        for (model, window) in KNOWN_WINDOWS {
            assert_eq!(
                known_model_window(model),
                Some(*window),
                "the table's own entries are the first thing it must answer: {model}"
            );
        }
    }

    #[test]
    fn matching_ignores_case_but_not_suffixes() {
        assert_eq!(
            known_model_window("GLM-5.3"),
            Some(1_000_000),
            "a typed uppercase spelling names the same model"
        );
        assert_eq!(
            known_model_window("glm-5.3-flash"),
            Some(1_000_000),
            "the flash sibling carries its own entry"
        );
        assert_eq!(
            known_model_window("glm-5.3-preview"),
            None,
            "an unlisted sibling never inherits its family's entry"
        );
        assert_eq!(
            known_model_window("glm-4.6"),
            None,
            "a model no source states is no answer, not a guess"
        );
    }
}
