//! Shared JSON-input parsing helpers for the tools.
//!
//! Tools that accept JSON input from the model repeatedly need to extract
//! optional integer and string-array fields with strict validation — a
//! malformed value should fail loudly as [`ToolError::InvalidInput`], not be
//! silently coerced to a default. These helpers centralize that pattern so
//! every search-style tool (`Grep`, `CodeSearch`, …) validates identically.

use loopctl::tool::ToolError;
use serde_json::Value;

/// Extract an optional `usize` field, rejecting malformed values loudly.
///
/// Returns `Ok(None)` when the key is absent (caller applies a default).
/// Returns `Ok(Some(n))` for a valid non-negative integer that fits in
/// `usize`. Returns `Err(InvalidInput)` when the key is present but is not a
/// non-negative integer — so `{"max_matches": -5}` or
/// `{"max_matches": "abc"}` fail loudly rather than silently defaulting.
///
/// Prefer [`get_u64`] when the field is naturally a `u64` (durations, sizes
/// destined for `Duration::from_secs` or similar) — it avoids the
/// `usize`-width dance on 32-bit targets.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the key is present but the value
/// is not a non-negative integer that fits in the platform's `usize` range.
pub fn get_usize(input: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| usize_field_error(key)),
        Some(_) => Err(usize_field_error(key)),
    }
}

/// Build the shared "not a valid usize" error for [`get_usize`].
///
/// One message for both failure arms (non-integer JSON, and integer that
/// overflows the platform's `usize` range) — they're indistinguishable to the
/// caller's intent and naming them separately would leak Rust's type-width
/// distinction into a model-facing message. Mentions the platform's
/// `usize::MAX` so an out-of-range value is actionable rather than confusing.
fn usize_field_error(key: &str) -> ToolError {
    ToolError::InvalidInput(format!(
        "'{key}' must be a non-negative integer (0 to {max})",
        max = usize::MAX
    ))
}

/// Extract an optional `u64` field, rejecting malformed values loudly.
///
/// Counterpart to [`get_usize`] for fields whose natural type is `u64` rather
/// than `usize` — e.g. durations feeding `Duration::from_secs`. Returns
/// `Ok(None)` when the key is absent (caller applies a default), `Ok(Some(n))`
/// for a valid non-negative integer, and `Err(InvalidInput)` when the key is
/// present but is not a non-negative integer — so `{"timeout": -5}` or
/// `{"timeout": "abc"}` fail loudly rather than silently defaulting.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the key is present but the value
/// is not a non-negative integer that fits in a `u64`.
pub fn get_u64(input: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| u64_field_error(key)),
        Some(_) => Err(u64_field_error(key)),
    }
}

/// Build the shared "not a valid u64" error for [`get_u64`].
///
/// Mirrors [`usize_field_error`], naming the `u64` range. JSON numbers that
/// fail `as_u64` are negative, fractional, non-numeric, or larger than
/// `u64::MAX` — all map to the one message since the caller's intent is the
/// same.
fn u64_field_error(key: &str) -> ToolError {
    ToolError::InvalidInput(format!(
        "'{key}' must be a non-negative integer (0 to {max})",
        max = u64::MAX
    ))
}

/// Extract a string-array field, dropping non-string elements.
///
/// Returns an empty `Vec` when the key is absent. A present-but-non-array
/// value is rejected; an array with non-string elements drops those elements.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the key is present but the value
/// is not a JSON array.
pub fn get_string_list(input: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match input.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(arr)) => Ok(arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()),
        Some(_) => Err(ToolError::InvalidInput(format!(
            "'{key}' must be an array of strings"
        ))),
    }
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
    use serde_json::json;

    #[test]
    fn get_usize_absent_returns_none() {
        let input = json!({});
        let got = get_usize(&input, "n").unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn get_usize_valid_integer() {
        let input = json!({"n": 42});
        let got = get_usize(&input, "n").unwrap();
        assert_eq!(got, Some(42));
    }

    #[test]
    fn get_usize_zero_is_valid() {
        let input = json!({"n": 0});
        let got = get_usize(&input, "n").unwrap();
        assert_eq!(got, Some(0));
    }

    #[test]
    fn get_usize_rejects_negative() {
        let input = json!({"n": -5});
        assert!(get_usize(&input, "n").is_err());
    }

    #[test]
    fn get_usize_rejects_float() {
        let input = json!({"n": 1.5});
        assert!(get_usize(&input, "n").is_err());
    }

    #[test]
    fn get_usize_rejects_non_number() {
        let input = json!({"n": "abc"});
        assert!(get_usize(&input, "n").is_err());
        let input = json!({"n": true});
        assert!(get_usize(&input, "n").is_err());
        let input = json!({"n": null});
        assert!(get_usize(&input, "n").is_err());
    }

    #[test]
    fn get_usize_error_names_the_range() {
        // The message must state the platform's usize range so an
        // out-of-range value (e.g. > u32::MAX on a 32-bit target) is
        // actionable, not confusing.
        let input = json!({"n": "not a number"});
        let err = get_usize(&input, "n").unwrap_err();
        let msg = match err {
            ToolError::InvalidInput(s) => s,
            other => panic!("expected InvalidInput, got {other:?}"),
        };
        assert!(msg.contains("0 to"), "message should name the range: {msg}");
        assert!(
            msg.contains(&usize::MAX.to_string()),
            "message should include usize::MAX for this platform: {msg}"
        );
        assert!(msg.contains("'n'"), "message should name the key: {msg}");
    }

    #[test]
    fn get_u64_absent_returns_none() {
        assert_eq!(get_u64(&json!({}), "n").unwrap(), None);
    }

    #[test]
    fn get_u64_valid_integer() {
        assert_eq!(get_u64(&json!({"n": 42}), "n").unwrap(), Some(42));
    }

    #[test]
    fn get_u64_rejects_negative() {
        // A negative JSON number is not a u64; it must error, not default.
        assert!(get_u64(&json!({"n": -1}), "n").is_err());
    }

    #[test]
    fn get_u64_rejects_float_and_non_number() {
        assert!(get_u64(&json!({"n": 1.5}), "n").is_err());
        assert!(get_u64(&json!({"n": "abc"}), "n").is_err());
        assert!(get_u64(&json!({"n": true}), "n").is_err());
    }

    #[test]
    fn get_u64_error_names_the_range() {
        // Mirrors get_usize_error_names_the_range: the message states the u64
        // range so an out-of-range value is actionable, not confusing.
        let err = get_u64(&json!({"n": "not a number"}), "n").unwrap_err();
        let msg = match err {
            ToolError::InvalidInput(s) => s,
            other => panic!("expected InvalidInput, got {other:?}"),
        };
        assert!(msg.contains("0 to"), "message should name the range: {msg}");
        assert!(
            msg.contains(&u64::MAX.to_string()),
            "message should include u64::MAX: {msg}"
        );
        assert!(msg.contains("'n'"), "message should name the key: {msg}");
    }

    #[test]
    fn get_string_list_absent_returns_empty() {
        let input = json!({});
        let got = get_string_list(&input, "pats").unwrap();
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn get_string_list_valid_array() {
        let input = json!({"pats": ["*.rs", "*.toml"]});
        let got = get_string_list(&input, "pats").unwrap();
        assert_eq!(got, vec!["*.rs".to_string(), "*.toml".to_string()]);
    }

    #[test]
    fn get_string_list_empty_array() {
        let input = json!({"pats": []});
        let got = get_string_list(&input, "pats").unwrap();
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn get_string_list_drops_non_string_elements() {
        let input = json!({"pats": ["*.rs", 42, true, "*.toml"]});
        let got = get_string_list(&input, "pats").unwrap();
        assert_eq!(got, vec!["*.rs".to_string(), "*.toml".to_string()]);
    }

    #[test]
    fn get_string_list_rejects_non_array() {
        let input = json!({"pats": "*.rs"});
        assert!(get_string_list(&input, "pats").is_err());
        let input = json!({"pats": 42});
        assert!(get_string_list(&input, "pats").is_err());
        let input = json!({"pats": null});
        assert!(get_string_list(&input, "pats").is_err());
    }
}
