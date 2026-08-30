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
    ///
    /// When `false`, `errors` is guaranteed non-empty.
    pub is_valid: bool,

    /// Validation errors found.
    ///
    /// Empty when `is_valid` is `true`; currently holds exactly one entry
    /// (validators return early), but the `Vec` leaves room for multi-error
    /// reporting without changing the public type.
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
    /// 1-indexed line number of the error, when the validator can determine it.
    ///
    /// `None` for validators without position info (e.g. `syn` on stable
    /// toolchains); always `Some` for the Python indentation heuristic.
    pub line: Option<usize>,

    /// Human-readable description of the validation error.
    ///
    /// Produced by the validator that found the problem (e.g. `syn`'s error
    /// string for Rust, the JS delimiter-counter's "unmatched closing brace"
    /// for JS/TS). Surfaced verbatim to the model in the lint-failure message
    /// so it can correct the input and retry.
    pub message: String,
}

impl LinterResult {
    /// Construct a passing result with no errors.
    ///
    /// Returns a [`LinterResult`] with `is_valid == true` and an empty error
    /// list. Used by every validator's success path — the content passed all
    /// checks, so there is nothing to report.
    fn pass() -> Self {
        Self {
            is_valid: true,
            errors: vec![],
        }
    }

    /// Construct a failing result carrying a single error.
    ///
    /// Returns a [`LinterResult`] with `is_valid == false` and the given
    /// [`LinterError`] as the sole entry. Validators return early on the first
    /// error, so the list holds exactly one entry today; the `Vec` leaves room
    /// for multi-error reporting without changing the public type.
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
    /// The model still gets the message — it just can't jump to the position.
    fn msg(message: impl Into<String>) -> Self {
        Self {
            line: None,
            message: message.into(),
        }
    }

    /// Construct an error at a specific 1-indexed line.
    ///
    /// Used by validators that track position as they scan (e.g. the Python
    /// indentation heuristic). The line number lets the lint-failure message
    /// direct the model to the exact spot to fix.
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

/// Validate Python source via a fast in-process structural check.
///
/// Runs without spawning a Python interpreter (which would require one at
/// runtime and break the synchronous contract). Two structural error families
/// the model commonly makes are caught without attempting full syntax
/// validation:
///
/// - **Indentation** — lines that mix tabs and spaces in leading whitespace,
///   or whose dedent returns to an indentation level that was never
///   established by an outer block (`unindent does not match any outer
///   indentation level`). Consistent-but-unconventional depths (hanging
///   indents, aligned continuation lines) are not penalized.
/// - **Delimiters** — unmatched `()`, `[]`, or `{}`, scanning past `#` line
///   comments and single-quoted, double-quoted, and triple-quoted strings.
///
/// False negatives (passing content Python would reject) are acceptable;
/// false positives (failing valid content) are not.
fn lint_python(content: &str) -> LinterResult {
    if let Some(err) = python_indent_check(content) {
        return LinterResult::fail(err);
    }
    if let Some(err) = python_delimiter_check(content) {
        return LinterResult::fail(err);
    }
    LinterResult::pass()
}

/// One stage of [`lint_python`]: indentation consistency.
///
/// Flags two cases: leading whitespace that mixes tabs and spaces on the same
/// line, and a dedent that lands on a width no outer block established. Blank
/// lines and lines whose first non-whitespace character is `#` are skipped so
/// they don't perturb the indent stack. Continuation lines inside open `()`,
/// `[]`, or `{}` groups are also skipped (Python ignores their indentation),
/// tracked via a running delimiter depth.
fn python_indent_check(content: &str) -> Option<LinterError> {
    let mut indent_stack: Vec<usize> = vec![0];
    let mut delim_depth = 0i32;
    let mut in_triple = None::<char>;
    for (i, raw) in content.lines().enumerate() {
        let line_no = i.saturating_add(1);

        if in_triple.is_some() {
            update_triple_state(raw, &mut in_triple);
            continue;
        }

        let trimmed = raw.trim_start();

        update_triple_from_line(trimmed, &mut in_triple);
        if in_triple.is_some() {
            continue;
        }

        if delim_depth > 0 || trimmed.is_empty() || trimmed.starts_with('#') {
            delim_depth = delim_depth.saturating_add(python_net_delimiters(trimmed));
            continue;
        }

        let leading = raw.len().saturating_sub(trimmed.len());
        if leading > 0 {
            let prefix = &raw[..leading];
            if prefix.contains('\t') && prefix.contains(' ') {
                return Some(LinterError::at(
                    line_no,
                    "inconsistent indentation: mixes tabs and spaces",
                ));
            }
        }
        match indent_stack.last() {
            Some(&top) if leading == top => {}
            Some(&top) if leading > top => {
                indent_stack.push(leading);
            }
            _ => {
                while indent_stack.last().is_some_and(|&w| w > leading) {
                    indent_stack.pop();
                }
                if indent_stack.last().is_none_or(|&w| w != leading) {
                    return Some(LinterError::at(
                        line_no,
                        "inconsistent indentation: dedent does not match any outer level",
                    ));
                }
            }
        }
        delim_depth = delim_depth.saturating_add(python_net_delimiters(trimmed));
    }
    None
}

/// Check whether `line` opens a triple-quoted string that spans beyond it.
///
/// Scans the line for a `"""` or `'''` opener that is not closed on the same
/// line (accounting for single-line strings and `#` comments that may precede
/// it). If found, sets `state` to the quote character.
fn update_triple_from_line(line: &str, state: &mut Option<char>) {
    if state.is_some() {
        return;
    }
    let bytes = line.as_bytes();
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match in_str {
            Some(q) => {
                if b == b'\\' {
                    i = i.saturating_add(2);
                    continue;
                }
                if b == q {
                    in_str = None;
                }
            }
            None => match b {
                b'#' => break,
                b'"' | b'\'' => {
                    if bytes.get(i.saturating_add(1)) == Some(&b)
                        && bytes.get(i.saturating_add(2)) == Some(&b)
                    {
                        let rest = &line[i.saturating_add(3)..];
                        let close: String = [b as char, b as char, b as char].iter().collect();
                        if !rest.contains(&close) {
                            *state = Some(b as char);
                        }
                        return;
                    }
                    in_str = Some(b);
                }
                _ => {}
            },
        }
        i = i.saturating_add(1);
    }
}

/// If inside a triple string, check whether `line` closes it.
///
/// Scans for an unescaped triple-quote delimiter — `\"""` inside a string does
/// not close it, only a bare `"""` or `'''` does.
fn update_triple_state(line: &str, state: &mut Option<char>) {
    let Some(q) = *state else { return };
    let bytes = line.as_bytes();
    let qb = q as u8;
    let mut i: usize = 0;
    let mut escaped = false;
    while i.saturating_add(2) <= bytes.len() {
        let Some(&b) = bytes.get(i) else { break };
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == qb
            && bytes.get(i.saturating_add(1)) == Some(&qb)
            && bytes.get(i.saturating_add(2)) == Some(&qb)
        {
            *state = None;
            return;
        }
        i = i.saturating_add(1);
    }
}

/// Net delimiter delta for one line of Python source.
///
/// Drives a [`PythonScanner`] over the line, summing `+1` per opener and `-1`
/// per closer, so the string/comment-skipping logic is shared with
/// [`python_delimiter_check`]. Returns 0 for a line that is entirely inside a
/// string or comment. A limitation: a triple-quoted string opened on a prior
/// line makes this line's content "inside a string," which a per-line scanner
/// cannot detect — such a line is scanned as if top-level. This rarely
/// matters, because continuation lines inside triple strings are unusual.
fn python_net_delimiters(line: &str) -> i32 {
    let mut depth = 0i32;
    let mut scanner = PythonScanner::new(line);
    while let Some(ch) = scanner.next_structural() {
        match ch {
            '(' | '[' | '{' => depth = depth.saturating_add(1),
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

/// A character-stream scanner that skips Python string literals and comments.
///
/// Wraps a `Peekable<Chars>` and yields only the *structural* characters the
/// Python checks care about: the delimiters `()[]{}` and `\n` (as a line
/// boundary for the per-line continuation check). String bodies —
/// single-quoted, double-quoted (with `\` escapes), and triple-quoted (which
/// may span lines) — and `#` comment bodies are consumed silently and never
/// yield a character of their own. Both [`python_delimiter_check`]
/// (whole-content balancing) and the per-line continuation depth used by
/// [`python_indent_check`] drive this scanner so the string/comment-skipping
/// logic lives in exactly one place.
struct PythonScanner<'a> {
    /// The underlying character stream, peekable for quote lookahead.
    ///
    /// Peekable so a quote can be checked for a triple-quote run before the
    /// scanner commits to consuming it.
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl<'a> PythonScanner<'a> {
    /// Build a scanner over `source`.
    ///
    /// The scanner borrows `source` for its lifetime and starts positioned at
    /// the first character.
    fn new(source: &'a str) -> Self {
        Self {
            chars: source.chars().peekable(),
        }
    }

    /// Advance to and return the next structural character.
    ///
    /// Returns the next `(`, `)`, `[`, `]`, `{`, `}`, or `\n`, skipping any
    /// string literal or `#` comment body encountered along the way. `None` at
    /// EOF. Comments are consumed up to (but not including) the next `\n`, so a
    /// comment never yields a structural character of its own.
    fn next_structural(&mut self) -> Option<char> {
        while let Some(ch) = self.chars.next() {
            match ch {
                '#' => self.skip_comment(),
                '(' | ')' | '[' | ']' | '{' | '}' | '\n' => return Some(ch),
                quote @ ('\'' | '"') => {
                    let triple = self.chars.peek().is_some_and(|&c| c == quote)
                        && self.chars.clone().nth(1).is_some_and(|c| c == quote);
                    if triple {
                        self.chars.next();
                        self.chars.next();
                        self.skip_triple_string(quote);
                    } else {
                        self.skip_single_line_string(quote);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Consume a `#` comment body (the `#` already consumed).
    ///
    /// Reads until the next `\n` (left unconsumed so it is yielded separately
    /// as a line boundary) or EOF.
    fn skip_comment(&mut self) {
        while self.chars.peek().is_some_and(|&c| c != '\n') {
            self.chars.next();
        }
    }

    /// Consume a single-line string body (the opener already consumed).
    ///
    /// Reads until the matching `quote`, honoring `\` escapes, or until `\n`
    /// (an unterminated single-line string ends at the newline, matching
    /// Python's own behavior).
    fn skip_single_line_string(&mut self, quote: char) {
        let mut escaped = false;
        for c in self.chars.by_ref() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote || c == '\n' {
                break;
            }
        }
    }

    /// Consume a triple-quoted string body (the opening `qqq` already consumed).
    ///
    /// Reads until the closing `qqq`, which may be many lines away. An
    /// unterminated triple string runs to EOF.
    fn skip_triple_string(&mut self, quote: char) {
        while let Some(c) = self.chars.next() {
            if c == quote
                && self.chars.peek().is_some_and(|&n| n == quote)
                && self.chars.clone().nth(1).is_some_and(|n| n == quote)
            {
                self.chars.next();
                self.chars.next();
                return;
            }
        }
    }
}

/// One stage of [`lint_python`]: delimiter balancing.
///
/// Drives a [`PythonScanner`] over the whole content, counting `()[]{}`. Any
/// counter going negative (an unmatched closer) or nonzero at EOF (an unmatched
/// opener) is rejected. String and comment bodies are skipped by the scanner so
/// delimiters inside them don't affect the count.
fn python_delimiter_check(content: &str) -> Option<LinterError> {
    let mut paren = 0u32;
    let mut bracket = 0u32;
    let mut brace = 0u32;
    let mut scanner = PythonScanner::new(content);
    while let Some(ch) = scanner.next_structural() {
        match ch {
            '(' => paren = paren.saturating_add(1),
            ')' => {
                if paren == 0 {
                    return Some(LinterError::msg("unmatched closing parenthesis `)`"));
                }
                paren = paren.saturating_sub(1);
            }
            '[' => bracket = bracket.saturating_add(1),
            ']' => {
                if bracket == 0 {
                    return Some(LinterError::msg("unmatched closing bracket `]`"));
                }
                bracket = bracket.saturating_sub(1);
            }
            '{' => brace = brace.saturating_add(1),
            '}' => {
                if brace == 0 {
                    return Some(LinterError::msg("unmatched closing brace `}`"));
                }
                brace = brace.saturating_sub(1);
            }
            _ => {}
        }
    }
    if paren != 0 {
        return Some(LinterError::msg(format!(
            "unbalanced parentheses: depth {paren} at end of file"
        )));
    }
    if bracket != 0 {
        return Some(LinterError::msg(format!(
            "unbalanced brackets: depth {bracket} at end of file"
        )));
    }
    if brace != 0 {
        return Some(LinterError::msg(format!(
            "unbalanced braces: depth {brace} at end of file"
        )));
    }
    None
}

/// Heuristic brace/bracket matching for JS and TS source.
///
/// Tracks the nesting depth of `()`, `[]`, and `{}` as a single-pass scan.
/// String literals (`"`, `'`, `` ` ``), line comments (`//`), and block
/// comments (`/* */`) are skipped via the `skip_js_*` helpers so that braces
/// inside strings or comments don't affect the count.
///
/// If any counter goes negative (an unmatched closer) or any counter is
/// nonzero at EOF (an unmatched opener), the content is rejected. This
/// catches the most common structural errors without a full parser, and
/// runs in-process with no external runtime dependency.
fn lint_js(content: &str) -> LinterResult {
    let mut paren = 0u32;
    let mut bracket = 0u32;
    let mut brace = 0u32;
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '/' if matches!(chars.peek(), Some('/')) => skip_js_line_comment(&mut chars),
            '/' if matches!(chars.peek(), Some('*')) => skip_js_block_comment(&mut chars),
            quote @ ('"' | '\'' | '`') => skip_js_string(quote, &mut chars),
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
                    return LinterResult::fail(LinterError::msg("unmatched closing bracket `]`"));
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

/// Consume a `//` line comment, with only the first `/` consumed.
///
/// Consumes the second `/` and everything through the terminating newline
/// (or to EOF), so nothing inside the comment body can affect the delimiter
/// counters.
fn skip_js_line_comment(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    chars.next();
    while let Some(&c) = chars.peek() {
        chars.next();
        if c == '\n' {
            break;
        }
    }
}

/// Consume a `/* */` block comment, with only the leading `/` consumed.
///
/// Consumes the `*` and reads through the closing `*/` (or to EOF for an
/// unterminated comment), so delimiters inside the comment body never reach
/// the counters.
fn skip_js_block_comment(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    chars.next();
    let mut prev = '\0';
    for c in chars.by_ref() {
        if prev == '*' && c == '/' {
            break;
        }
        prev = c;
    }
}

/// Consume a string literal, with the opening `quote` consumed.
///
/// Reads until the matching `quote`, honoring `\` escapes so an escaped
/// quote does not terminate the literal; an unterminated literal runs to
/// EOF.
fn skip_js_string(quote: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
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
    fn python_unbalanced_paren_fails() {
        let result = lint_content(Path::new("a.py"), "def foo(:\n    pass\n");
        assert!(!result.is_valid, "{:?}", result.errors);
    }

    #[test]
    fn python_balanced_ignores_braces_in_string_and_comment() {
        let src = "x = \"{not a brace}\"\n# this (has [delimiters]\ny = [1, 2, 3]\n";
        let result = lint_content(Path::new("a.py"), src);
        assert!(result.is_valid, "{:?}", result.errors);
    }

    #[test]
    fn python_triple_quoted_string_braces_ignored() {
        let src = "x = \"\"\"\nthis has { and [ and ( inside\n\"\"\"\ny = 1\n";
        let result = lint_content(Path::new("a.py"), src);
        assert!(result.is_valid, "{:?}", result.errors);
    }

    #[test]
    fn python_multiline_triple_string_indent_not_checked() {
        // Lines inside an open triple-quoted string should not be checked for
        // indentation — the scanner must preserve triple-string state across
        // line boundaries. Without the fix, line 2's 2-space indent would be
        // flagged as a bad dedent.
        let src = "x = \"\"\"\n  arbitrary indent inside\n    more indent\n\"\"\"\ny = 1\n";
        let result = lint_content(Path::new("a.py"), src);
        assert!(
            result.is_valid,
            "indent inside triple string should not be checked: {result:?}"
        );
    }

    #[test]
    fn python_escaped_triple_quote_does_not_close_string() {
        // An escaped triple quote (\""") inside a triple-quoted string must NOT
        // close it — the scanner should keep tracking the open string across
        // the following lines with arbitrary indentation.
        let src = "x = \"\"\"\n  \\\"\"\" more text\n  still inside\n\"\"\"\ny = 1\n";
        let result = lint_content(Path::new("a.py"), src);
        assert!(
            result.is_valid,
            "escaped triple quote should not close the string: {result:?}"
        );
    }

    #[test]
    fn python_bad_dedent_fails() {
        // Dedent to column 3, which no outer block established (cols 0 and 4
        // were the seen levels).
        let src = "def f():\n    x = 1\n   y = 2\n";
        let result = lint_content(Path::new("a.py"), src);
        assert!(!result.is_valid, "{:?}", result.errors);
    }

    #[test]
    fn python_valid_nested_blocks_pass() {
        let src = "def f():\n    if True:\n        return 1\n    return 2\n";
        let result = lint_content(Path::new("a.py"), src);
        assert!(result.is_valid, "{:?}", result.errors);
    }

    #[test]
    fn python_hanging_indent_in_parens_passes() {
        // Continuation lines inside open parens may be indented arbitrarily.
        let src = "x = (\n    1,\n      2,\n)\n";
        let result = lint_content(Path::new("a.py"), src);
        assert!(result.is_valid, "{:?}", result.errors);
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
