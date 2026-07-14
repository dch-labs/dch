//! Syntax-checking gate shared by Write, Edit, and `MultiEdit`.
//!
//! The entry point is [`lint_content`], which infers the language from the file
//! extension and runs a synchronous in-process validator. Unsupported extensions
//! always pass.

use std::path::Path;

/// Result of syntax-checking a file's contents before writing.
///
/// Carries a pass/fail flag and a list of errors. When `is_valid` is `true`,
/// `errors` is always empty. When `false`, `errors` contains at least one
/// entry describing the first (and currently only) problem found.
///
/// The validators in this module return early on the first error, so the list
/// typically holds a single `LinterError`. The `Vec` leaves room for
/// multi-error reporting in future without changing the public type.
#[derive(Debug, Clone)]
pub struct LinterResult {
    /// Whether the content passed all validation checks.
    pub is_valid: bool,
    /// Validation errors found, if any. Empty when `is_valid` is `true`.
    pub errors: Vec<LinterError>,
}

/// One validation error found during linting.
///
/// Carries a human-readable message and, when the parser can determine it,
/// the 1-indexed line number of the offending content. Not all validators
/// produce line numbers (e.g. the Python indentation heuristic always does;
/// the Rust `syn` validator does not on stable toolchains).
#[derive(Debug, Clone)]
pub struct LinterError {
    /// 1-indexed line number of the error, when known.
    pub line: Option<usize>,
    /// Human-readable description of the validation error.
    pub message: String,
}

impl LinterResult {
    /// Construct a passing result with no errors.
    fn pass() -> Self {
        Self {
            is_valid: true,
            errors: vec![],
        }
    }

    /// Construct a failing result carrying a single error.
    fn fail(error: LinterError) -> Self {
        Self {
            is_valid: false,
            errors: vec![error],
        }
    }
}

impl LinterError {
    /// Construct an error with a message but no line number.
    ///
    /// Used by validators that cannot determine the line (e.g. `syn` on stable
    /// toolchains, or parsers that report a structural error without a span).
    fn msg(message: impl Into<String>) -> Self {
        Self {
            line: None,
            message: message.into(),
        }
    }

    /// Construct an error at a specific 1-indexed line.
    ///
    /// Used by validators that track position as they scan (e.g. the Python
    /// indentation heuristic).
    fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
        }
    }
}

/// Syntax-check `content` as if it lived at `path`.
///
/// The language is inferred from the file extension. Unsupported extensions
/// always return a passing result (no validation possible).
///
/// This function is synchronous and never spawns a subprocess. Safe to call
/// from an async tool body without `spawn_blocking`.
#[must_use]
pub fn lint_content(path: &Path, content: &str) -> LinterResult {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "rs" => lint_rust(content),
        "json" => lint_json(content),
        "toml" => lint_toml(content),
        "yaml" | "yml" => lint_yaml(content),
        "py" => lint_python(content),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => lint_js(content),
        _ => LinterResult::pass(),
    }
}

/// Validate Rust source using `syn::parse_file`.
///
/// Catches syntax errors (missing semicolons, unbalanced braces, etc.) but not
/// type or borrow errors — the linter's job is "is this a syntactically valid
/// Rust file?", not "does this compile?". Runs in microseconds with no project
/// context or subprocess.
///
/// Line numbers are not available on stable toolchains because `syn`'s span
/// location API requires the `span-locations` feature, which the workspace
/// does not enable. The error message from `syn` still carries useful detail.
fn lint_rust(content: &str) -> LinterResult {
    match syn::parse_file(content) {
        Ok(_) => LinterResult::pass(),
        Err(e) => LinterResult::fail(LinterError::msg(e.to_string())),
    }
}

/// Validate JSON by parsing into `serde_json::Value`.
///
/// Empty or whitespace-only content is treated as invalid (an empty JSON file
/// is not valid JSON). On parse failure, `serde_json`'s error message (which
/// includes line and column on most inputs) is forwarded as-is.
fn lint_json(content: &str) -> LinterResult {
    if content.trim().is_empty() {
        return LinterResult::fail(LinterError::msg("empty JSON content"));
    }
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(_) => LinterResult::pass(),
        Err(e) => LinterResult::fail(LinterError::msg(e.to_string())),
    }
}

/// Validate TOML by parsing into `toml::Value`.
///
/// Empty or whitespace-only content is treated as invalid. On parse failure,
/// the `toml` crate's error message is forwarded as-is.
fn lint_toml(content: &str) -> LinterResult {
    if content.trim().is_empty() {
        return LinterResult::fail(LinterError::msg("empty TOML content"));
    }
    match content.parse::<toml::Value>() {
        Ok(_) => LinterResult::pass(),
        Err(e) => LinterResult::fail(LinterError::msg(e.to_string())),
    }
}

/// Validate YAML by parsing into `serde_yaml::Value`.
///
/// Empty or whitespace-only content is treated as invalid. On parse failure,
/// the `serde_yaml` crate's error message is forwarded as-is.
fn lint_yaml(content: &str) -> LinterResult {
    if content.trim().is_empty() {
        return LinterResult::fail(LinterError::msg("empty YAML content"));
    }
    match serde_yaml::from_str::<serde_yaml::Value>(content) {
        Ok(_) => LinterResult::pass(),
        Err(e) => LinterResult::fail(LinterError::msg(e.to_string())),
    }
}

/// Heuristic indentation check for Python source.
///
/// Flags lines whose leading whitespace mixes tabs and spaces — a common
/// `IndentationError` cause that the Python interpreter rejects at runtime.
/// The check is intentionally conservative: it does not verify consistent
/// indentation depth across blocks, validate syntax, or detect mixed
/// indentation on non-leading whitespace. False negatives (passing content
/// that Python would reject) are acceptable; false positives (failing valid
/// content) are not.
///
/// This replaces the salvage's `python3 -m py_compile` subprocess call, which
/// required a Python interpreter at runtime and broke the synchronous contract.
fn lint_python(content: &str) -> LinterResult {
    for (i, line) in content.lines().enumerate() {
        let leading = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect::<String>();
        if leading.contains('\t') && leading.contains(' ') {
            return LinterResult::fail(LinterError::at(
                i.saturating_add(1),
                "inconsistent indentation: mixes tabs and spaces",
            ));
        }
    }
    LinterResult::pass()
}

/// Heuristic brace/bracket matching for JS and TS source.
///
/// Tracks the nesting depth of `()`, `[]`, and `{}` as a single-pass scan.
/// String literals (`"`, `'`, `` ` ``), line comments (`//`), and block
/// comments (`/* */`) are skipped so that braces inside strings or comments
/// don't affect the count.
///
/// If any counter goes negative (an unmatched closer) or any counter is
/// nonzero at EOF (an unmatched opener), the content is rejected. This
/// catches the most common structural errors without a full parser.
///
/// This replaces the salvage's `node --check` subprocess call, which required
/// Node.js at runtime and broke the synchronous contract.
fn lint_js(content: &str) -> LinterResult {
    let mut paren = 0u32;
    let mut bracket = 0u32;
    let mut brace = 0u32;
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '/' if matches!(chars.peek(), Some('/')) => {
                // Line comment — skip to end of line.
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if matches!(chars.peek(), Some('*')) => {
                chars.next();
                // Block comment — skip to closing */.
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            '"' | '\'' | '`' => {
                // String literal — scan until the matching quote, skipping
                // escaped chars via a flag (no inner chars.next() needed,
                // so a for-loop works without borrow conflicts).
                let quote = ch;
                let mut in_escape = false;
                for c in chars.by_ref() {
                    if in_escape {
                        in_escape = false;
                    } else if c == '\\' {
                        in_escape = true;
                    } else if c == quote {
                        break;
                    }
                }
            }
            '(' => paren = paren.saturating_add(1),
            ')' => {
                if paren == 0 {
                    return LinterResult::fail(LinterError::msg(
                        "unmatched closing parenthesis `)`",
                    ));
                }
                paren = paren.saturating_sub(1);
            }
            '[' => bracket = bracket.saturating_add(1),
            ']' => {
                if bracket == 0 {
                    return LinterResult::fail(LinterError::msg("unmatched closing bracket `]"));
                }
                bracket = bracket.saturating_sub(1);
            }
            '{' => brace = brace.saturating_add(1),
            '}' => {
                if brace == 0 {
                    return LinterResult::fail(LinterError::msg("unmatched closing brace `}`"));
                }
                brace = brace.saturating_sub(1);
            }
            _ => {}
        }
    }
    if paren != 0 {
        return LinterResult::fail(LinterError::msg(format!(
            "unbalanced parentheses: depth {paren} at end of file"
        )));
    }
    if bracket != 0 {
        return LinterResult::fail(LinterError::msg(format!(
            "unbalanced brackets: depth {bracket} at end of file"
        )));
    }
    if brace != 0 {
        return LinterResult::fail(LinterError::msg(format!(
            "unbalanced braces: depth {brace} at end of file"
        )));
    }
    LinterResult::pass()
}

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::format_collect,
    clippy::indexing_slicing,
    clippy::let_underscore_must_use
)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rust_valid() {
        let result = lint_content(Path::new("a.rs"), "fn main() { println!(\"hi\"); }");
        assert!(result.is_valid, "{:?}", result.errors);
    }

    #[test]
    fn rust_invalid_reports_error() {
        let result = lint_content(Path::new("a.rs"), "fn main() { let x = ; }");
        assert!(!result.is_valid);
        assert_eq!(result.errors.len(), 1);
        let err = &result.errors[0];
        assert!(err.message.contains("expected"), "{}", err.message);
    }

    #[test]
    fn json_valid() {
        let result = lint_content(Path::new("a.json"), r#"{"key": "value"}"#);
        assert!(result.is_valid);
    }

    #[test]
    fn json_invalid() {
        let result = lint_content(Path::new("a.json"), r#"{"a": }"#);
        assert!(!result.is_valid);
    }

    #[test]
    fn json_empty_fails() {
        let result = lint_content(Path::new("a.json"), "   ");
        assert!(!result.is_valid);
    }

    #[test]
    fn toml_valid() {
        let result = lint_content(Path::new("a.toml"), "[package]\nname = \"x\"\n");
        assert!(result.is_valid);
    }

    #[test]
    fn toml_invalid() {
        let result = lint_content(Path::new("a.toml"), "[package\nname = ");
        assert!(!result.is_valid);
    }

    #[test]
    fn yaml_valid() {
        let result = lint_content(Path::new("a.yaml"), "key: value\n");
        assert!(result.is_valid);
    }

    #[test]
    fn yaml_invalid() {
        let result = lint_content(Path::new("a.yaml"), "key: [unterminated");
        assert!(!result.is_valid);
    }

    #[test]
    fn python_clean_passes() {
        let result = lint_content(Path::new("a.py"), "def foo():\n    return 42\n");
        assert!(result.is_valid);
    }

    #[test]
    fn python_mixed_indent_fails() {
        let result = lint_content(Path::new("a.py"), "def foo():\n\t return 42\n");
        assert!(!result.is_valid);
        assert_eq!(result.errors[0].line, Some(2));
    }

    #[test]
    fn js_balanced_passes() {
        let result = lint_content(Path::new("a.js"), "function foo() { return [1, 2]; }");
        assert!(result.is_valid);
    }

    #[test]
    fn js_unbalanced_brace_fails() {
        let result = lint_content(Path::new("a.js"), "function foo() {");
        assert!(!result.is_valid);
    }

    #[test]
    fn js_brace_in_string_not_counted() {
        let result = lint_content(Path::new("a.js"), r#"var x = "{";"#);
        assert!(result.is_valid);
    }

    #[test]
    fn unknown_extension_passes() {
        let result = lint_content(Path::new("a.txt"), "garbage{{{");
        assert!(result.is_valid);
    }

    #[test]
    fn no_extension_passes() {
        let result = lint_content(Path::new("Makefile"), "anything");
        assert!(result.is_valid);
    }

    #[test]
    fn extension_case_insensitive() {
        let result = lint_content(Path::new("A.RS"), "fn main() { let x = ; }");
        assert!(!result.is_valid);
    }

    #[test]
    fn no_panic_on_large_input() {
        let big = "x".repeat(10_000_000);
        let _ = lint_content(Path::new("a.rs"), &big);
        let braces = "{".repeat(100_000);
        let _ = lint_content(Path::new("a.json"), &braces);
    }
}
