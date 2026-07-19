//! Shared gitignore-aware directory walker and glob primitives.
//!
//! The single [`walk_files`] entry point is what every search-style tool
//! (`Glob`, `Grep`, `CodeSearch`, `Tree`) uses for directory traversal. It
//! honors `.gitignore`, `.git/info/exclude`, the global gitignore, and an
//! optional `.dchignore` file, and it applies an always-exclude list so that
//! `target/`, `node_modules/`, `.git/`, and friends never leak into results —
//! even in non-git repositories.
//!
//! The [`wildcard_match`] / [`matches_any_glob`] / [`likely_binary`] helpers
//! are filename-level `*`/`?` matching for include/exclude filters and binary
//! detection. They live here so every consumer uses one implementation.

use std::path::Path;

/// Build a gitignore-aware file walker over `base`.
///
/// Honors `.gitignore`, `.git/info/exclude`, the global gitignore, and a
/// `.dchignore` file (when present); skips hidden entries; and always excludes
/// `target/`, `node_modules/`, `.git/`, `__pycache__/`, `.venv/`, and the other
/// directories named by [`build_default_overrides`] — even in non-git
/// repositories. Symlinks are not followed, matching `ignore`'s default, so
/// symlink cycles cannot hang the walker.
///
/// The optional `include_patterns` and `exclude_patterns` are filename-level
/// globs (`*`/`?`); pass empty slices to disable filename filtering. `Glob`
/// always passes empty slices because it filters via its own matcher after the
/// walk; `Grep` and `CodeSearch` use these to restrict the file set.
///
/// Returns each matched file as an `ignore::DirEntry` (not directories, not
/// symlinked-into files).
#[must_use]
pub fn walk_files(
    base: &Path,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> Box<dyn Iterator<Item = ignore::DirEntry> + Send> {
    let mut builder = ignore::WalkBuilder::new(base);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .add_custom_ignore_filename(".dchignore");

    let overrides = build_default_overrides(base);
    builder.overrides(overrides);

    let include = include_patterns.to_vec();
    let exclude = exclude_patterns.to_vec();

    Box::new(
        builder
            .build()
            .filter_map(std::result::Result::ok)
            .filter(move |e| {
                if !e.file_type().is_some_and(|ft| ft.is_file()) {
                    return false;
                }
                let file_name = e.path().file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !include.is_empty() && !matches_any_glob(file_name, &include) {
                    return false;
                }
                if matches_any_glob(file_name, &exclude) {
                    return false;
                }
                true
            }),
    )
}

/// Build the always-exclude override set for a search root.
///
/// These directories are removed from results regardless of `.gitignore`,
/// because they are almost never useful to search (`target/`, `node_modules/`,
/// `.git/`, `__pycache__/`, virtualenvs, build output). The `!`-prefix is
/// gitignore "negate-to-exclude" syntax as interpreted by
/// `ignore::overrides::Override`. On a build error the matcher falls back to
/// an empty `Override` (no exclusions) rather than panicking.
#[must_use]
pub fn build_default_overrides(base: &Path) -> ignore::overrides::Override {
    let mut builder = ignore::overrides::OverrideBuilder::new(base);
    let always_exclude = [
        "!target/**",
        "!node_modules/**",
        "!.git/**",
        "!__pycache__/**",
        "!.next/**",
        "!.venv/**",
        "!venv/**",
        "!dist/**",
        "!build/**",
        "!.cache/**",
    ];
    for pat in always_exclude {
        builder.add(pat).ok();
    }
    builder
        .build()
        .unwrap_or_else(|_| ignore::overrides::Override::empty())
}

/// True if `filename` matches any pattern in `patterns`.
///
/// Each pattern is matched with [`wildcard_match`], which supports `*` (any
/// run of characters) and `?` (a single character). Used by [`walk_files`]'s
/// include/exclude filters and exposed for other tools that need the same
/// filename-glob check.
#[must_use]
pub fn matches_any_glob(filename: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| wildcard_match(filename, p))
}

/// Single-segment wildcard matcher supporting `*` and `?`.
///
/// `*` matches any run of characters (including empty); `?` matches exactly
/// one character. Both are byte-position based with backtracking, so they
/// handle repeated wildcards correctly. This is intentionally limited to a
/// single path segment — it does not understand `/`. Path-aware `**` matching
/// is each tool's job (ripgrep's `Override` for `Glob`), not here.
#[must_use]
pub fn wildcard_match(text: &str, pattern: &str) -> bool {
    let text_chars: Vec<char> = text.chars().collect();
    let pattern_chars: Vec<char> = pattern.chars().collect();
    let text_len = text_chars.len();
    let pat_len = pattern_chars.len();
    let mut t_pos = 0usize;
    let mut p_pos = 0usize;
    let mut star_pos: Option<usize> = None;
    let mut match_pos = 0usize;
    while t_pos < text_len {
        let p_char = pattern_chars.get(p_pos).copied();
        let t_char = text_chars.get(t_pos).copied();
        let is_question = p_char == Some('?');
        let is_exact = p_char.is_some_and(|p| t_char.is_some_and(|t| p == t));
        if is_question || is_exact {
            t_pos = t_pos.saturating_add(1);
            p_pos = p_pos.saturating_add(1);
        } else if p_char == Some('*') {
            star_pos = Some(p_pos);
            match_pos = t_pos;
            p_pos = p_pos.saturating_add(1);
        } else if let Some(sp) = star_pos {
            p_pos = sp.saturating_add(1);
            match_pos = match_pos.saturating_add(1);
            t_pos = match_pos;
        } else {
            return false;
        }
    }
    while p_pos < pat_len && pattern_chars.get(p_pos) == Some(&'*') {
        p_pos = p_pos.saturating_add(1);
    }
    p_pos == pat_len
}

/// Binary file extensions that should always be skipped by search tools.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "pdf", "zip", "tar", "gz", "bz2", "xz", "7z", "rar",
    "exe", "dll", "so", "dylib", "a", "lib", "o", "obj", "class", "pyc", "rlib", "wasm",
];

/// Bytes of the read buffer used by [`likely_binary`] for content sniffing.
const SNIFF_BYTES: usize = 8192;

/// True if `path` is likely a binary file, by extension or content sniff.
///
/// The check has two stages: first, a known-binary extension (`png`, `pdf`,
/// `so`, …); if that is inconclusive, the first 8 KB are read and scanned for
/// NUL bytes or a low ratio of printable/whitespace bytes. Either signal
/// marks the file binary. Read faults are treated as "not binary" so the
/// caller can surface the real error itself.
#[must_use]
pub fn likely_binary(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        if BINARY_EXTENSIONS.contains(&ext_lower.as_str()) {
            return true;
        }
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buffer = [0u8; SNIFF_BYTES];
    let Ok(n) = std::io::Read::read(&mut file, &mut buffer) else {
        return false;
    };
    if n == 0 {
        return false;
    }
    let Some(window) = buffer.get(..n) else {
        return false;
    };
    if window.contains(&0) {
        return true;
    }
    let threshold = n.saturating_mul(3).saturating_div(4);
    let text_bytes = window
        .iter()
        .filter(|&&b| b == b'\t' || b == b'\n' || b == b'\r' || (32..=126).contains(&b))
        .count();
    text_bytes < threshold
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn wildcard_match_star() {
        assert!(wildcard_match("test.rs", "*.rs"));
        assert!(wildcard_match("file.json", "*.json"));
        assert!(!wildcard_match("test.rs", "*.json"));
        assert!(wildcard_match("anything", "*"));
    }

    #[test]
    fn wildcard_match_question() {
        assert!(wildcard_match("foo.rs", "???.rs"));
        assert!(!wildcard_match("test.rs", "???.rs"));
    }

    #[test]
    fn wildcard_match_combined() {
        assert!(wildcard_match("my_file_test.rs", "*_test.rs"));
        assert!(wildcard_match("file123.json", "file???.json"));
    }

    #[test]
    fn matches_any_glob_multiple_patterns() {
        let patterns = vec!["*.rs".to_string(), "*.json".to_string()];
        assert!(matches_any_glob("test.rs", &patterns));
        assert!(matches_any_glob("data.json", &patterns));
        assert!(!matches_any_glob("readme.md", &patterns));
    }

    #[test]
    fn matches_any_glob_empty_patterns() {
        let patterns: Vec<String> = vec![];
        assert!(!matches_any_glob("test.rs", &patterns));
    }

    #[test]
    fn likely_binary_by_extension() {
        assert!(likely_binary(&PathBuf::from("image.png")));
        assert!(likely_binary(&PathBuf::from("lib.rlib")));
        assert!(likely_binary(&PathBuf::from("code.pyc")));
        assert!(!likely_binary(&PathBuf::from("main.rs")));
        assert!(!likely_binary(&PathBuf::from("config.toml")));
    }

    #[test]
    fn build_default_overrides_excludes_target() {
        // target/ and friends are excluded even in non-git repos. Every
        // pattern in the always-exclude list is exercised here.
        let ov = build_default_overrides(Path::new("/repo"));
        for excluded in [
            "target/debug/foo.rs",
            "node_modules/pkg/index.js",
            ".git/HEAD",
            "__pycache__/x.pyc",
            ".next/cache.json",
            ".venv/bin/python",
            "venv/bin/python",
            "dist/bundle.js",
            "build/out.o",
            ".cache/tmp",
        ] {
            assert!(
                ov.matched(excluded, false).is_ignore(),
                "{excluded} should be excluded by the always-exclude list"
            );
        }
    }

    #[test]
    fn build_default_overrides_does_not_exclude_source() {
        // Source paths that share no segment with the always-exclude list must
        // pass through (return None, not Ignore).
        let ov = build_default_overrides(Path::new("/repo"));
        for kept in ["src/main.rs", "README.md", "tests/glob.rs"] {
            assert!(
                !ov.matched(kept, false).is_ignore(),
                "{kept} should not be excluded"
            );
        }
    }

    #[test]
    fn build_default_overrides_does_not_panic_on_relative_base() {
        // `.` is a valid search root for the override builder; the function
        // must produce a usable matcher from it, not panic.
        let ov = build_default_overrides(Path::new("."));
        assert!(ov.matched("target/x.rs", false).is_ignore());
    }

    /// Walk `base` and return the relative paths of yielded entries, sorted.
    ///
    /// Sorting makes the assertion order-independent, which matters because
    /// `ignore`'s traversal order is not guaranteed to be lexical.
    fn walked(base: &Path, include: &[String], exclude: &[String]) -> Vec<String> {
        let mut out: Vec<String> = walk_files(base, include, exclude)
            .map(|e| {
                e.path()
                    .strip_prefix(base)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        out.sort();
        out
    }

    /// Create a `.git` marker dir so `ignore` treats `dir` as a git work tree
    /// and honors `.gitignore`.
    fn make_git_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).expect("create .git marker");
    }

    #[test]
    fn walk_files_returns_files_only() {
        // Directories must not appear in the output, only files.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/b.rs"), "").expect("write");
        let got = walked(tmp.path(), &[], &[]);
        assert!(got.contains(&"a.rs".to_string()), "{got:?}");
        assert!(got.contains(&"src/b.rs".to_string()), "{got:?}");
        assert!(
            !got.iter()
                .any(|p| p == "src" || p.ends_with("/src") || p == ".git"),
            "no directories in output: {got:?}"
        );
    }

    #[test]
    fn walk_files_respects_gitignore_in_git_repo() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        make_git_repo(tmp.path());
        std::fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").expect("write gitignore");
        std::fs::write(tmp.path().join("ignored.rs"), "").expect("write");
        std::fs::write(tmp.path().join("kept.rs"), "").expect("write");
        let got = walked(tmp.path(), &[], &[]);
        assert!(got.contains(&"kept.rs".to_string()), "{got:?}");
        assert!(
            !got.contains(&"ignored.rs".to_string()),
            "gitignored file must be absent: {got:?}"
        );
    }

    #[test]
    fn walk_files_does_not_apply_gitignore_without_git() {
        // Without a .git marker, ignore does not honor .gitignore. This locks
        // the upstream behavior so a future change is caught (and documents
        // why the Glob integration test creates a .git dir).
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").expect("write gitignore");
        std::fs::write(tmp.path().join("ignored.rs"), "").expect("write");
        let got = walked(tmp.path(), &[], &[]);
        assert!(
            got.contains(&"ignored.rs".to_string()),
            "without .git, .gitignore is not applied: {got:?}"
        );
    }

    #[test]
    fn walk_files_respects_dchignore_without_git() {
        // `.dchignore` is a custom-ignore-filename, applied by the walker
        // regardless of whether the dir is a git repo.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join(".dchignore"), "secret.rs\n").expect("write dchignore");
        std::fs::write(tmp.path().join("secret.rs"), "").expect("write");
        std::fs::write(tmp.path().join("public.rs"), "").expect("write");
        let got = walked(tmp.path(), &[], &[]);
        assert!(got.contains(&"public.rs".to_string()), "{got:?}");
        assert!(
            !got.contains(&"secret.rs".to_string()),
            ".dchignore'd file must be absent: {got:?}"
        );
    }

    #[test]
    fn walk_files_applies_always_exclude_without_git() {
        // The always-exclude list (build_default_overrides) is what makes
        // target/ disappear even in non-git repos.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("target/debug")).expect("mkdir");
        std::fs::write(tmp.path().join("target/debug/foo.rs"), "").expect("write");
        std::fs::write(tmp.path().join("main.rs"), "").expect("write");
        let got = walked(tmp.path(), &[], &[]);
        assert!(got.contains(&"main.rs".to_string()), "{got:?}");
        assert!(
            !got.iter().any(|p| p.contains("target")),
            "target/ must be always-excluded: {got:?}"
        );
    }

    #[test]
    fn walk_files_include_filter_restricts_filenames() {
        // Filename include filter: only files matching at least one pattern.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        std::fs::write(tmp.path().join("b.json"), "").expect("write");
        std::fs::write(tmp.path().join("c.rs"), "").expect("write");
        let include = vec!["*.rs".to_string()];
        let got = walked(tmp.path(), &include, &[]);
        assert_eq!(got, vec!["a.rs".to_string(), "c.rs".to_string()]);
    }

    #[test]
    fn walk_files_exclude_filter_removes_filenames() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        std::fs::write(tmp.path().join("b.test.rs"), "").expect("write");
        std::fs::write(tmp.path().join("c.rs"), "").expect("write");
        let exclude = vec!["*.test.rs".to_string()];
        let got = walked(tmp.path(), &[], &exclude);
        assert_eq!(got, vec!["a.rs".to_string(), "c.rs".to_string()]);
    }

    #[test]
    fn walk_files_empty_dir_yields_nothing() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let got = walked(tmp.path(), &[], &[]);
        assert!(got.is_empty(), "{got:?}");
    }
}
