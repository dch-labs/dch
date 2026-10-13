//! Gate the repository-hygiene rules over git's own view of the index.
//!
//! Two states defeat a repository's own safeguards without ever breaking a
//! build: a path the ignore rules match that was staged before those rules
//! covered it, which `.gitignore` cannot reach, and a test or record that is
//! present in the working tree but absent from the index, so a commit ships
//! the claim while omitting the proof. Both have recurred in this repository,
//! which is why the check exists rather than a note. [`Checker`] is pure over
//! the two lists [`run_check`] gathers from `git ls-files`, so every rule is
//! pinned by a unit test over a synthetic view without touching a repository.

use std::process::Command;
use std::process::ExitCode;

/// The root-level file naming the work a repository still owes.
///
/// A commit that resolves rows of it is expected to carry it: the file is
/// the standing list the next round reads, so an untracked copy means the
/// list a reader acts on is not the one in the repository.
const OUTSTANDING_LOG: &str = "OUTSTANDING_ISSUES.md";

/// The prefix of a crate's integration-test tree.
///
/// Everything beneath it is a pin: an integration test file, or a fixture
/// one such test reads. Losing either from a commit removes coverage no
/// compile error notices, which is the failure this rule reports.
const TEST_TREE_PREFIX: &str = "crates/";

/// The filename mark identifying a review artifact at the repository root.
///
/// Review rounds write these beside the code they audit; the commit that
/// applies a round's findings carries the artifact the findings came from.
const REVIEW_ARTIFACT_PREFIX: &str = "code-review-";

/// The extension a review artifact carries.
///
/// Compared case-insensitively against the path's extension, so the rule
/// reads the artifact's role rather than the spelling of its suffix.
const MARKDOWN_EXTENSION: &str = "md";

/// The two index views these rules read, exactly as `git` reports them.
///
/// Narrowing the check to these two lists is what lets it be unit-tested:
/// both are plain path lists, so a test supplies the shape `git ls-files`
/// prints without owning a repository.
#[derive(Debug)]
pub struct IndexView<'a> {
    /// Paths held in the index that the ignore rules also match.
    ///
    /// The overlap of `--cached` and `--ignored`: staged despite being
    /// ignored, the state an ignore rule can never undo on its own.
    pub staged_and_ignored: &'a [String],

    /// Paths in neither the index nor the ignore set.
    ///
    /// Untracked files, which a commit omits silently — unlike a missing
    /// source file, nothing fails to build when one of them is left out.
    pub untracked: &'a [String],
}

/// One hygiene-rule violation, attributed to the path that caused it.
///
/// Rules accumulate rather than fail fast, so one run lists every index
/// problem at once rather than one per invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The path whose index state the rule reports.
    ///
    /// Names where the fix belongs; [`Violation::detail`] states what is
    /// wrong about it.
    pub path: String,

    /// The state that fails, and what to do about it.
    ///
    /// Carries the remedy, because these are one-command fixes and a reader
    /// should not have to know the command already.
    pub detail: String,
}

/// The hygiene checker, pure over an [`IndexView`].
///
/// Holding no I/O lets each rule be driven with a synthetic view, so a
/// verdict is pinned without a repository, an index, or a `git` invocation.
pub struct Checker<'a> {
    view: &'a IndexView<'a>,
}

impl<'a> Checker<'a> {
    /// A checker borrowing the index view it will inspect.
    ///
    /// The view is borrowed rather than owned because the rules only read
    /// path lists: the caller keeps its `git` output where it is, and the
    /// same borrow serves both rules in one pass.
    pub fn new(view: &'a IndexView) -> Self {
        Self { view }
    }

    /// Run both rules, collecting every violation.
    ///
    /// The rules are independent, so one pass reports every index problem
    /// rather than stopping at the first; an empty return is the verdict
    /// `check-hygiene` prints as a pass.
    pub fn check(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        out.extend(self.rule_no_ignored_paths_in_index());
        out.extend(self.rule_no_untracked_pins());
        out
    }

    /// Rule 1: no path the ignore rules match sits in the index.
    ///
    /// An ignore rule only speaks for untracked paths, so a path staged
    /// before it was ignored — or staged with `-f` after — keeps traveling
    /// with every commit while the tree says it should not exist there.
    /// Scratch captures land here most often, and each one commits the
    /// residue of a failed run.
    fn rule_no_ignored_paths_in_index(&self) -> Vec<Violation> {
        self.view
            .staged_and_ignored
            .iter()
            .map(|path| Violation {
                path: path.clone(),
                detail:
                    "staged although the ignore rules match it; the index holds it regardless, \
                         so clear it with `git rm --cached <path>`"
                        .to_string(),
            })
            .collect()
    }

    /// Rule 2: no test or record is left out of the index.
    ///
    /// A commit records what the index holds, so an untracked integration
    /// test, its fixture, or a review artifact simply does not travel. The
    /// damage is quiet: the suite stays green because the absent test never
    /// ran, and the coverage the commit message claims was never shipped.
    fn rule_no_untracked_pins(&self) -> Vec<Violation> {
        self.view
            .untracked
            .iter()
            .filter(|path| is_pin_path(path))
            .map(|path| Violation {
                path: path.clone(),
                detail: "untracked, so a commit omits it while the work it proves lands; add it \
                         with the commit that claims its coverage"
                    .to_string(),
            })
            .collect()
    }
}

/// Whether an untracked path carries coverage or the record of a round.
///
/// Two families count. Anything under a crate's `tests/` tree is a pin or a
/// fixture one reads. At the repository root, a review artifact or the
/// standing issue list is the record a round is expected to ship with; the
/// root restriction keeps these well-known names from matching a nested
/// file of the same spelling. Everything else — a scratch note, a build
/// leftover, work in progress — is deliberately outside the rule, since a
/// check that fails on ordinary files gets switched off rather than obeyed.
fn is_pin_path(path: &str) -> bool {
    let in_test_tree = path
        .strip_prefix(TEST_TREE_PREFIX)
        .is_some_and(|rest| rest.contains("/tests/"));
    let at_root = !path.contains('/');
    let is_record = path == OUTSTANDING_LOG
        || (path.starts_with(REVIEW_ARTIFACT_PREFIX)
            && std::path::Path::new(path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(MARKDOWN_EXTENSION)));
    in_test_tree || (at_root && is_record)
}

/// Ask `git` for one list of paths, one per line.
///
/// The returned error is the text the caller prints verbatim, naming the
/// `git` invocation that failed and what it reported.
///
/// # Errors
///
/// Reports the failure when `git` cannot be spawned or exits non-zero — a
/// repository it cannot read. The caller turns any failure into the "could
/// not be consulted" exit rather than a violation, because a toolchain
/// problem must not read as a clean index.
fn git_listed(args: &[&str]) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn `git {}`: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Runs the hygiene check over this repository and reports the verdict.
///
/// Asks `git` which paths are both staged and ignored, and which are
/// untracked, then feeds both to [`Checker`]. Exits `0` on a clean index,
/// `1` when a rule is violated, and `3` when `git` could not be consulted —
/// an unreadable repository must not read as a clean one.
pub fn run_check() -> ExitCode {
    let staged = match git_listed(&["ls-files", "-i", "-c", "--exclude-standard"]) {
        Ok(paths) => paths,
        Err(message) => {
            eprintln!("xtask: {message}");
            return ExitCode::from(3);
        }
    };
    let untracked = match git_listed(&["ls-files", "--others", "--exclude-standard"]) {
        Ok(paths) => paths,
        Err(message) => {
            eprintln!("xtask: {message}");
            return ExitCode::from(3);
        }
    };

    let view = IndexView {
        staged_and_ignored: &staged,
        untracked: &untracked,
    };
    let violations = Checker::new(&view).check();

    if violations.is_empty() {
        println!("xtask: hygiene check passed (2 rules, 0 violations)");
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "xtask: hygiene check FAILED ({} violation(s)):",
            violations.len()
        );
        for violation in &violations {
            eprintln!("  - {}: {}", violation.path, violation.detail);
        }
        ExitCode::FAILURE
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
mod tests {
    use super::*;

    /// Build a path list the way `git ls-files` prints one.
    ///
    /// The rules read owned `String` lists because that is what the front
    /// end hands them; the tests spell their fixtures as `&str` and convert
    /// here rather than littering every case with the conversion.
    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    /// A view over the given staged-and-ignored and untracked lists.
    ///
    /// Returns the owned halves beside the case that needs them, since the
    /// view itself only borrows and a temporary would not outlive the call.
    fn view(staged: &[&str], untracked: &[&str]) -> (Vec<String>, Vec<String>) {
        (paths(staged), paths(untracked))
    }

    #[test]
    fn an_ignored_path_in_the_index_violates_the_index_rule() {
        let (staged, untracked) = view(&["run.out"], &[]);
        let violations = Checker::new(&IndexView {
            staged_and_ignored: &staged,
            untracked: &untracked,
        })
        .check();
        assert_eq!(
            violations
                .iter()
                .map(|v| v.path.as_str())
                .collect::<Vec<_>>(),
            vec!["run.out"],
            "a staged path the ignore rules match is reported by name"
        );
    }

    #[test]
    fn a_staged_path_that_is_not_ignored_satisfies_the_index_rule() {
        let (staged, untracked) = view(&[], &[]);
        let violations = Checker::new(&IndexView {
            staged_and_ignored: &staged,
            untracked: &untracked,
        })
        .check();
        assert!(
            violations.is_empty(),
            "the rule reads the staged-and-ignored overlap, so a source file staged as usual \
             cannot appear here: {violations:?}"
        );
    }

    #[test]
    fn an_untracked_test_file_violates_the_pin_rule() {
        let (staged, untracked) = view(&[], &["crates/dch/tests/resume_stale_real.rs"]);
        let violations = Checker::new(&IndexView {
            staged_and_ignored: &staged,
            untracked: &untracked,
        })
        .check();
        assert_eq!(
            violations.len(),
            1,
            "an integration test outside the index ships no coverage: {violations:?}"
        );
    }

    #[test]
    fn an_untracked_fixture_violates_the_pin_rule() {
        let fixture = "crates/dch/tests/fixtures/frozen-41cee350/session.json";
        let (staged, untracked) = view(&[], &[fixture]);
        let violations = Checker::new(&IndexView {
            staged_and_ignored: &staged,
            untracked: &untracked,
        })
        .check();
        assert_eq!(
            violations
                .iter()
                .map(|v| v.path.as_str())
                .collect::<Vec<_>>(),
            vec![fixture],
            "a fixture its test cannot run without travels with it or not at all"
        );
    }

    #[test]
    fn an_untracked_root_review_artifact_violates_the_pin_rule() {
        let (staged, untracked) =
            view(&[], &["code-review-2026-10-11.md", "OUTSTANDING_ISSUES.md"]);
        let violations = Checker::new(&IndexView {
            staged_and_ignored: &staged,
            untracked: &untracked,
        })
        .check();
        assert_eq!(
            violations.len(),
            2,
            "a commit that applies a round's findings carries the record they came from: \
             {violations:?}"
        );
    }

    #[test]
    fn an_untracked_file_outside_the_pin_sets_satisfies_the_pin_rule() {
        let (staged, untracked) = view(
            &[],
            &[
                "crates/dch/src/notes.md",
                "notes/code-review-2026-10-11.md",
                "scratch.md",
            ],
        );
        let violations = Checker::new(&IndexView {
            staged_and_ignored: &staged,
            untracked: &untracked,
        })
        .check();
        assert!(
            violations.is_empty(),
            "work in progress and a nested file borrowing a record's name are not index \
             problems: {violations:?}"
        );
    }

    #[test]
    fn a_clean_index_view_satisfies_both_rules() {
        let (staged, untracked) = view(&[], &[]);
        let violations = Checker::new(&IndexView {
            staged_and_ignored: &staged,
            untracked: &untracked,
        })
        .check();
        assert!(
            violations.is_empty(),
            "a healthy repository cannot trip either rule: {violations:?}"
        );
    }
}
