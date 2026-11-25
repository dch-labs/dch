//! The `[project]` section: tech-stack overrides for the
//! auto-detected profile.
//!
//! [`ProjectConfig`] holds the project-wide conventions and one
//! [`TechProfile`] per language the user declares or overrides.

/// One technology in a project, with its toolchain and conventions.
///
/// Most real projects are polyglot — a Rust core, a `TypeScript` frontend, a
/// Python tooling script — and each language has its own build/test/lint
/// commands and its own conventions. This struct captures one such language;
/// [`ProjectConfig`] holds a `Vec<TechProfile>` for all of them.
///
/// Every field except `language` is optional: set only what detection got
/// wrong or can't infer. Loaded from the `[[project.techs]]` array-of-tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct TechProfile {
    /// The language this entry describes, e.g. `"rust"` or `"cpp"`.
    ///
    /// Matches against the detected language so the runner can merge detected
    /// and configured entries by language. Required — an entry with no
    /// language names nothing.
    pub language: String,

    /// Command that builds this language's code.
    ///
    /// Overrides the detected build command for this language, e.g.
    /// `"cargo build"`. `None` keeps the detected value (or leaves it empty
    /// when detection found none).
    pub build: Option<String>,

    /// Command that runs this language's tests.
    ///
    /// Overrides the detected test command, e.g. `"cargo test"`. `None`
    /// keeps the detected value.
    pub test: Option<String>,

    /// Command that lints this language's code.
    ///
    /// Overrides the detected lint command, e.g. `"cargo clippy"`. `None`
    /// keeps the detected value.
    pub lint: Option<String>,

    /// Free-form conventions for this language.
    ///
    /// Style rules, module layout, anything detection can't capture. Appended
    /// to the prompt verbatim under this language's section. `None` keeps the
    /// detected value.
    pub conventions: Option<String>,
}

/// Project / tech-stack overrides for the auto-detected profile.
///
/// Polyglot projects set `[[project.techs]]` once per language; the runner
/// merges each with its detected counterpart by language (set fields override,
/// configured languages detection missed are appended, detected languages the
/// config doesn't mention are kept). `conventions` holds project-wide
/// conventions that span all languages (commit format, branch policy).
///
/// Loaded from the `[project]` table. Absent entirely when the user relies on
/// auto-detection.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Per-language tech entries.
    ///
    /// One `[[project.techs]]` table per language the user wants to declare or
    /// override. The runner merges each with its detected counterpart by
    /// language; entries for languages detection missed are appended.
    pub techs: Vec<TechProfile>,

    /// Free-form project-wide conventions that apply across all languages.
    ///
    /// Prose for anything detection can't capture: commit-message format,
    /// branch policy, where new modules go. Appended to the prompt verbatim.
    pub conventions: Option<String>,
}
