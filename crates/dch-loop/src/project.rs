//! Project / tech-stack detection and merging.
//!
//! Infers one [`TechProfile`] per detected language (a polyglot repo yields
//! several) from the repo's marker files, then merges them with the user's
//! `[project]` overrides. The runner injects the rendered profile into the
//! system prompt so the agent knows which stacks it is working on.

use std::path::Path;

use dch_config::ProjectConfig;
use dch_config::Role;
use dch_config::TechProfile;
use loopctl::api::ApiClient;
use loopctl::message::Message;
use loopctl::structured::StructuredError;
use loopctl::structured::StructuredOutput;
use loopctl::structured::request_structured;
use serde::Deserialize;
use serde::Serialize;

/// One marker file and the profile it implies.
///
/// A row in [`MARKERS`]: the presence of `file` at a repo root implies the
/// language and the default build/test/lint commands. Detection is a best
/// guess — a marker doesn't authoritatively determine the language (a
/// `Makefile` could belong to any language's task runner); the `[project]`
/// override exists to correct wrong guesses.
struct Marker {
    /// Filename (or glob) to look for at the repo root.
    ///
    /// Fixed names like `"Cargo.toml"` are compared literally; a `*` in the
    /// pattern triggers a glob match via [`has_glob_match`].
    file: &'static str,

    /// The language name this marker implies.
    ///
    /// A best-guess label like `"rust"`, `"make"`, `"csharp"`; used as the
    /// merge key and the rendered header.
    language: &'static str,

    /// Default build command for this language.
    ///
    /// Copied into [`TechProfile::build`] on detection; overridable.
    build: &'static str,

    /// Default test command for this language.
    ///
    /// Copied into [`TechProfile::test`] on detection; overridable.
    test: &'static str,

    /// Default lint command for this language.
    ///
    /// Copied into [`TechProfile::lint`] on detection; overridable.
    lint: &'static str,
}

/// The marker files `detect_tech_stack` recognizes, in detection order.
///
/// Order is the tie-breaker only when two markers of the *same* language are
/// both present (e.g. `pyproject.toml` and `setup.py`); across languages, all
/// matches are returned. Marker-to-language is a heuristic, not authoritative
/// — a user corrects a wrong guess via `[project].techs`. Add a language by
/// adding a row here.
const MARKERS: &[Marker] = &[
    Marker {
        file: "Cargo.toml",
        language: "rust",
        build: "cargo build",
        test: "cargo test",
        lint: "cargo clippy",
    },
    Marker {
        file: "go.mod",
        language: "go",
        build: "go build ./...",
        test: "go test ./...",
        lint: "go vet ./...",
    },
    Marker {
        file: "package.json",
        language: "typescript",
        build: "npm run build",
        test: "npm test",
        lint: "npm run lint",
    },
    Marker {
        file: "pyproject.toml",
        language: "python",
        build: "pip install -e .",
        test: "pytest",
        lint: "ruff check",
    },
    Marker {
        file: "setup.py",
        language: "python",
        build: "pip install -e .",
        test: "pytest",
        lint: "ruff check",
    },
    Marker {
        file: "pom.xml",
        language: "java",
        build: "mvn compile",
        test: "mvn test",
        lint: "mvn checkstyle:check",
    },
    Marker {
        file: "build.gradle",
        language: "java",
        build: "gradle build",
        test: "gradle test",
        lint: "gradle check",
    },
    Marker {
        file: "build.gradle.kts",
        language: "kotlin",
        build: "gradle build",
        test: "gradle test",
        lint: "gradle check",
    },
    Marker {
        file: "CMakeLists.txt",
        language: "cpp",
        build: "cmake --build build",
        test: "ctest --test-dir build",
        lint: "cmake --build build --target lint",
    },
    Marker {
        file: "Makefile",
        language: "make",
        build: "make",
        test: "make test",
        lint: "make lint",
    },
    Marker {
        file: "mix.exs",
        language: "elixir",
        build: "mix compile",
        test: "mix test",
        lint: "mix credo",
    },
    Marker {
        file: "dub.json",
        language: "d",
        build: "dub build",
        test: "dub test",
        lint: "dub lint",
    },
    Marker {
        file: "*.csproj",
        language: "csharp",
        build: "dotnet build",
        test: "dotnet test",
        lint: "dotnet format --verify-no-changes",
    },
];

/// Detect every tech stack present at `root`, one [`TechProfile`] per language.
///
/// All matching markers contribute; duplicates by language are collapsed
/// (first marker for a language wins, so `pyproject.toml` beats `setup.py`).
/// A root with no recognized marker yields an empty `Vec`; the caller then
/// relies on the agent exploring the repo, or on `[project].techs` overrides
/// merged in afterward by [`merge_by_language`].
///
/// `*.csproj` (C#) is a glob rather than a fixed filename; it matches any
/// direct child of `root` ending in `.csproj`.
#[must_use]
pub fn detect_tech_stack(root: &Path) -> Vec<TechProfile> {
    let mut profiles: Vec<TechProfile> = Vec::new();
    for marker in MARKERS {
        let matched = if marker.file.contains('*') {
            has_glob_match(root, marker.file)
        } else {
            root.join(marker.file).exists()
        };
        if matched && !profiles.iter().any(|p| p.language == marker.language) {
            profiles.push(marker_profile(marker));
        }
    }
    profiles
}

/// Resolve a matched [`Marker`] to its [`TechProfile`].
///
/// Companion to [`language_profile`]: both return a [`TechProfile`], but from
/// different sources — this one from a detected marker (so every command is
/// known and set to `Some(...)`), [`language_profile`] from a language name
/// (e.g. an inferred name from [`analyze_message`]). Conventions are `None`
/// here; detection never infers them, only a `[project]` override does via
/// [`merge_by_language`].
fn marker_profile(m: &Marker) -> TechProfile {
    TechProfile {
        language: m.language.to_string(),
        build: Some(m.build.to_string()),
        test: Some(m.test.to_string()),
        lint: Some(m.lint.to_string()),
        conventions: None,
    }
}

/// Look up the default [`TechProfile`] for a language name.
///
/// Used by message inference ([`MessageAnalysis::tech_profiles`]); detection
/// reads the same [`MARKERS`] table via [`marker_profile`], so both paths
/// agree on what a language means — "rust" resolves to the same
/// build/test/lint whether it was found by a `Cargo.toml` marker or inferred
/// from a "build a Rust CLI" message.
/// Returns `None` for an unknown language (e.g. an inferred name with no
/// marker row); the caller skips it.
fn language_profile(language: &str) -> Option<TechProfile> {
    let lang = language.to_lowercase();
    MARKERS
        .iter()
        .find(|m| m.language == lang)
        .map(marker_profile)
}

/// The model's analysis of a first user message: its intent and its stack.
///
/// Produced by [`analyze_message`] from the first user message. Both fields
/// degrade gracefully — `None` role / empty languages leave the configured
/// role and filesystem detection in charge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageAnalysis {
    /// The role that best fits the user's intent.
    ///
    /// `Some(Role::Coding)` for "create a CLI", `Some(Role::General)` for "how
    /// does systemd work?", `None` when unclear.
    pub suggested_role: Option<Role>,

    /// Languages the message implies.
    ///
    /// E.g. `["rust"]`, or `["rust", "typescript"]` for polyglot intent. Empty
    /// for non-code messages.
    pub languages: Vec<String>,
}

impl StructuredOutput for MessageAnalysis {
    fn name() -> &'static str {
        "message_analysis"
    }

    fn schema() -> serde_json::Value {
        let role_names: Vec<serde_json::Value> = Role::ALL
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or_default())
            .collect();
        let mut role_enum = role_names;
        role_enum.push(serde_json::Value::Null);
        serde_json::json!({
            "type": "object",
            "properties": {
                "suggested_role": {
                    "type": ["string", "null"],
                    "enum": role_enum,
                    "description": "The role that best fits what the user wants to do. null if unclear."
                },
                "languages": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Programming languages the message implies (e.g. \"rust\", \"go\", \"python\", \"typescript\", \"cpp\", \"csharp\", \"java\", \"kotlin\", \"bash\"). Empty array for non-code questions."
                }
            },
            "required": ["suggested_role", "languages"],
            "additionalProperties": false
        })
    }
}

impl MessageAnalysis {
    /// Build [`TechProfile`]s for the analyzed languages.
    ///
    /// Each language resolves via the shared per-language lookup (the same one
    /// detection uses, so "rust" means the same commands whether found by a
    /// `Cargo.toml` marker or inferred from a message). Unknown languages are
    /// skipped rather than producing empty or panicking profiles.
    #[must_use]
    pub fn tech_profiles(&self) -> Vec<TechProfile> {
        self.languages
            .iter()
            .filter_map(|lang| language_profile(lang))
            .collect()
    }
}

/// Analyze the first user message for intent (→ suggested role) and stack.
///
/// One LLM call via loopctl's [`StructuredOutput`] machinery, using the same
/// model the session will use (the `client` is built pre-session). The result
/// is typed — no free-text parsing. See [`MessageAnalysis`] for the fields.
///
/// # Errors
///
/// Returns the loopctl [`StructuredError`] if the call fails or the response
/// does not match the schema. The caller treats any error as "no analysis" —
/// keep the configured role and rely on filesystem detection only — rather
/// than failing the session.
pub async fn analyze_message(
    client: &dyn ApiClient,
    first_message: &str,
) -> Result<MessageAnalysis, StructuredError> {
    let messages = vec![Message::user(first_message)];
    let system = Some(
        "Classify the user's first message. Identify the role that best fits \
         their intent (general assistance, coding, refactor, debug, review, \
         docs, or tests — null if unclear) and any programming languages it \
         implies (empty array for non-code questions)."
            .to_string(),
    );
    request_structured::<MessageAnalysis>(client, messages, system).await
}

/// True if any direct child of `root` matches the single-segment `glob`.
///
/// Used by [`detect_tech_stack`] for glob-based markers (those whose `file`
/// contains `*`, e.g. `"*.csproj"`). Reads only the immediate children of
/// `root` — does not descend into subdirectories, since markers are
/// root-level files. A nonexistent or unreadable `root` returns `false`
/// rather than panicking.
fn has_glob_match(root: &Path, glob: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str()
            && dch_tools::walk::wildcard_match(name, glob)
        {
            return true;
        }
    }
    false
}

/// Merge detected profiles with the user's `[project]` overrides by language.
///
/// For each detected profile whose language matches a configured [`TechProfile`],
/// that tech's set fields override the detected ones. Configured techs whose
/// language was not detected are appended (the user adds a language detection
/// missed). Detected techs the config doesn't mention are kept as-is. Order is
/// detected-first, then appended config-only techs.
#[must_use]
pub fn merge_by_language(
    mut detected: Vec<TechProfile>,
    config: &ProjectConfig,
) -> Vec<TechProfile> {
    for tech in &config.techs {
        if let Some(profile) = detected
            .iter_mut()
            .find(|p| p.language.eq_ignore_ascii_case(&tech.language))
        {
            if tech.build.is_some() {
                profile.build.clone_from(&tech.build);
            }
            if tech.test.is_some() {
                profile.test.clone_from(&tech.test);
            }
            if tech.lint.is_some() {
                profile.lint.clone_from(&tech.lint);
            }
            if tech.conventions.is_some() {
                profile.conventions.clone_from(&tech.conventions);
            }
        } else {
            detected.push(tech.clone());
        }
    }
    detected
}

/// Render a polyglot tech list plus project-wide conventions as the prose
/// block injected into the system prompt.
///
/// Returns an empty string when there is nothing to render — no project
/// conventions and every tech profile bare (no commands, no conventions) —
/// so the caller can skip the section entirely.
#[must_use]
pub fn render_techs(techs: &[TechProfile], project_conventions: Option<&str>) -> String {
    let no_techs = techs.iter().all(all_fields_empty);
    let no_conventions = project_conventions.is_none_or(str::is_empty);
    if no_techs && no_conventions {
        return String::new();
    }
    let mut out = String::from("PROJECT");
    for tech in techs {
        if all_fields_empty(tech) {
            continue;
        }
        out.push_str("\n\n- Language: ");
        out.push_str(&tech.language);
        if let Some(v) = tech.build.as_deref().filter(|v| !v.is_empty()) {
            out.push_str("\n  Build: ");
            out.push_str(v);
        }
        if let Some(v) = tech.test.as_deref().filter(|v| !v.is_empty()) {
            out.push_str("\n  Test: ");
            out.push_str(v);
        }
        if let Some(v) = tech.lint.as_deref().filter(|v| !v.is_empty()) {
            out.push_str("\n  Lint: ");
            out.push_str(v);
        }
        if let Some(v) = tech.conventions.as_deref().filter(|v| !v.is_empty()) {
            out.push_str("\n  Conventions: ");
            out.push_str(v);
        }
    }
    if let Some(conv) = project_conventions
        && !conv.is_empty()
    {
        out.push_str("\n\nProject conventions: ");
        out.push_str(conv);
    }
    out
}

/// True when every field of `tech` other than `language` is unset or empty.
///
/// A profile that is bare (only `language` set, no commands or conventions)
/// contributes nothing actionable to the prompt, so [`render_techs`] skips it.
/// This means a `[project].techs` entry that names only a language — e.g. to
/// nudge detection toward a language with no marker — does not appear in the
/// rendered PROJECT section on its own; pair it with at least one command or
/// convention to make it render.
fn all_fields_empty(tech: &TechProfile) -> bool {
    tech.build.as_deref().is_none_or(str::is_empty)
        && tech.test.as_deref().is_none_or(str::is_empty)
        && tech.lint.as_deref().is_none_or(str::is_empty)
        && tech.conventions.as_deref().is_none_or(str::is_empty)
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
    use dch_config::ProjectConfig;
    use dch_config::TechProfile;

    #[test]
    fn empty_root_yields_no_profiles() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(detect_tech_stack(tmp.path()).is_empty());
        assert!(render_techs(&[], None).is_empty());
    }

    #[test]
    fn cargo_toml_detects_rust_with_lint() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        let profiles = detect_tech_stack(tmp.path());
        assert_eq!(profiles.len(), 1);
        let p = &profiles[0];
        assert_eq!(p.language, "rust");
        assert_eq!(p.build.as_deref(), Some("cargo build"));
        assert_eq!(p.test.as_deref(), Some("cargo test"));
        assert_eq!(p.lint.as_deref(), Some("cargo clippy"));
    }

    #[test]
    fn polyglot_repo_detects_each_language() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("package.json"), "").unwrap();
        std::fs::write(tmp.path().join("go.mod"), "").unwrap();
        let profiles = detect_tech_stack(tmp.path());
        let langs: Vec<&str> = profiles.iter().map(|p| p.language.as_str()).collect();
        assert!(langs.contains(&"rust"), "{langs:?}");
        assert!(langs.contains(&"typescript"), "{langs:?}");
        assert!(langs.contains(&"go"), "{langs:?}");
        assert_eq!(langs.len(), 3, "one profile per language");
    }

    #[test]
    fn duplicate_marker_for_same_language_dedups() {
        // pyproject.toml and setup.py both imply python; only one profile.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        std::fs::write(tmp.path().join("setup.py"), "").unwrap();
        let profiles = detect_tech_stack(tmp.path());
        let py = profiles.iter().filter(|p| p.language == "python").count();
        assert_eq!(py, 1, "python should appear once: {profiles:?}");
    }

    #[test]
    fn csproj_glob_detects_csharp() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("App.csproj"), "").unwrap();
        let profiles = detect_tech_stack(tmp.path());
        assert_eq!(profiles[0].language, "csharp");
    }

    #[test]
    fn has_glob_match_finds_matching_child() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("App.csproj"), "").unwrap();
        assert!(has_glob_match(tmp.path(), "*.csproj"));
    }

    #[test]
    fn has_glob_match_false_when_no_child_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("README.md"), "").unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        assert!(!has_glob_match(tmp.path(), "*.csproj"));
    }

    #[test]
    fn has_glob_match_finds_needle_among_other_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("README.md"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("Lib.csproj"), "").unwrap();
        assert!(
            has_glob_match(tmp.path(), "*.csproj"),
            "should find the .csproj among the other entries"
        );
    }

    #[test]
    fn has_glob_match_nonexistent_root_returns_false() {
        // read_dir fails on a path that doesn't exist; the function must
        // return false rather than panicking. This path is only reachable
        // via a direct call — detect_tech_stack probes real tempdirs.
        assert!(!has_glob_match(
            Path::new("/no/such/dir/anywhere"),
            "*.csproj"
        ));
    }

    #[test]
    fn mix_exs_lowercase_detects_elixir() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("mix.exs"), "").unwrap();
        let profiles = detect_tech_stack(tmp.path());
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].language, "elixir");
    }

    #[test]
    fn build_gradle_detects_java_with_gradle_build() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("build.gradle"), "").unwrap();
        let profiles = detect_tech_stack(tmp.path());
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].language, "java");
        assert_eq!(profiles[0].build.as_deref(), Some("gradle build"));
    }

    #[test]
    fn case_insensitive_language_matching_in_tech_profiles() {
        // Model may return "Rust" (capitalized) — must resolve to the lowercase
        // profile, not be silently dropped.
        let analysis = MessageAnalysis {
            suggested_role: Some(Role::Coding),
            languages: vec!["Rust".to_string()],
        };
        let profiles = analysis.tech_profiles();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].language, "rust");
    }

    #[test]
    fn merge_overrides_matching_language_and_appends_new() {
        let detected = vec![
            TechProfile {
                language: "rust".to_string(),
                build: Some("cargo build".to_string()),
                test: Some("cargo test".to_string()),
                lint: Some("cargo clippy".to_string()),
                conventions: None,
            },
            TechProfile {
                language: "typescript".to_string(),
                build: Some("npm run build".to_string()),
                test: Some("npm test".to_string()),
                lint: None,
                conventions: None,
            },
        ];
        let config = ProjectConfig {
            techs: vec![
                // Override rust's build only; keep detected test/lint.
                TechProfile {
                    language: "rust".to_string(),
                    build: Some("cargo build --release".to_string()),
                    test: None,
                    lint: None,
                    conventions: Some("no unwrap".to_string()),
                },
                // Add a language detection missed.
                TechProfile {
                    language: "bash".to_string(),
                    build: None,
                    test: Some("bats".to_string()),
                    lint: Some("shellcheck".to_string()),
                    conventions: None,
                },
            ],
            conventions: Some("conventional commits".to_string()),
        };
        let merged = merge_by_language(detected, &config);
        let rust = merged.iter().find(|p| p.language == "rust").unwrap();
        assert_eq!(
            rust.build.as_deref(),
            Some("cargo build --release"),
            "override applied"
        );
        assert_eq!(
            rust.test.as_deref(),
            Some("cargo test"),
            "unset field kept detected"
        );
        assert_eq!(
            rust.lint.as_deref(),
            Some("cargo clippy"),
            "unset field kept detected"
        );
        assert_eq!(rust.conventions.as_deref(), Some("no unwrap"));
        let bash = merged.iter().find(|p| p.language == "bash").unwrap();
        assert_eq!(
            bash.test.as_deref(),
            Some("bats"),
            "appended config-only tech"
        );
        assert_eq!(bash.lint.as_deref(), Some("shellcheck"));
        assert_eq!(merged.len(), 3, "rust + typescript + bash");
    }

    #[test]
    fn render_lists_each_tech_and_project_conventions() {
        let techs = vec![
            TechProfile {
                language: "rust".to_string(),
                build: Some("cargo build".to_string()),
                test: Some("cargo test".to_string()),
                lint: Some("cargo clippy".to_string()),
                conventions: None,
            },
            TechProfile {
                language: "bash".to_string(),
                build: None,
                test: Some("bats".to_string()),
                lint: None,
                conventions: None,
            },
        ];
        let rendered = render_techs(&techs, Some("conventional commits"));
        assert!(rendered.contains("PROJECT"), "{rendered}");
        assert!(rendered.contains("Language: rust"), "{rendered}");
        assert!(rendered.contains("Build: cargo build"), "{rendered}");
        assert!(rendered.contains("Lint: cargo clippy"), "{rendered}");
        assert!(rendered.contains("Language: bash"), "{rendered}");
        assert!(rendered.contains("Test: bats"), "{rendered}");
        assert!(
            !rendered.contains("Build: ") || rendered.matches("Build: ").count() == 1,
            "bash omits empty build: {rendered}"
        );
        assert!(
            rendered.contains("Project conventions: conventional commits"),
            "{rendered}"
        );
    }

    // ---- MessageAnalysis + analyze_message tests ----
    //
    // The pure logic (MessageAnalysis::tech_profiles — does "rust" resolve to
    // the right profile?) is tested directly below. The full analyze_message
    // integration (does the LLM round-trip work end to end?) is #[ignore]'d
    // because loopctl's MockApiClient does not yet honor the response_format
    // option that StructuredOutput requires (the default
    // create_message_with_options rejects it). That's a loopctl gap to file;
    // when MockApiClient supports response_format, un-ignore these.

    #[test]
    fn tech_profiles_from_coding_intent() {
        let analysis = MessageAnalysis {
            suggested_role: Some(Role::Coding),
            languages: vec!["rust".to_string()],
        };
        let profiles = analysis.tech_profiles();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].language, "rust");
        assert_eq!(profiles[0].build.as_deref(), Some("cargo build"));
        assert_eq!(profiles[0].test.as_deref(), Some("cargo test"));
        assert_eq!(profiles[0].lint.as_deref(), Some("cargo clippy"));
    }

    #[test]
    fn tech_profiles_empty_for_pure_question() {
        let analysis = MessageAnalysis {
            suggested_role: Some(Role::General),
            languages: vec![],
        };
        assert!(analysis.tech_profiles().is_empty());
    }

    #[test]
    fn tech_profiles_polyglot() {
        let analysis = MessageAnalysis {
            suggested_role: Some(Role::Coding),
            languages: vec!["rust".to_string(), "typescript".to_string()],
        };
        let profiles = analysis.tech_profiles();
        let langs: Vec<&str> = profiles.iter().map(|p| p.language.as_str()).collect();
        assert!(langs.contains(&"rust"), "{langs:?}");
        assert!(langs.contains(&"typescript"), "{langs:?}");
    }

    #[test]
    fn tech_profiles_unknown_language_skipped() {
        let analysis = MessageAnalysis {
            suggested_role: None,
            languages: vec!["brainfuck".to_string()],
        };
        assert!(
            analysis.tech_profiles().is_empty(),
            "unknown lang should be skipped"
        );
    }

    #[test]
    fn analyzed_commands_match_detected_commands() {
        // A language resolved via analysis (tech_profiles) yields the same
        // build/test/lint as detect_tech_stack — proves they share
        // language_profile / MARKERS.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        let detected = detect_tech_stack(tmp.path());
        assert_eq!(detected.len(), 1);
        let detected_rust = &detected[0];

        let analyzed = MessageAnalysis {
            suggested_role: Some(Role::Coding),
            languages: vec!["rust".to_string()],
        };
        let analyzed_rust = &analyzed.tech_profiles()[0];

        assert_eq!(analyzed_rust.build, detected_rust.build);
        assert_eq!(analyzed_rust.test, detected_rust.test);
        assert_eq!(analyzed_rust.lint, detected_rust.lint);
    }

    #[tokio::test]
    #[ignore = "until MockApiClient supports response_format (loopctl gap)"]
    async fn analyze_message_round_trip_with_mock() {
        let client = loopctl::testing::MockApiClient::new("test-model")
            .with_text_response(r#"{"suggested_role":"coding","languages":["rust"]}"#);
        let analysis = analyze_message(&client, "build a Rust CLI").await.unwrap();
        assert_eq!(analysis.suggested_role, Some(Role::Coding));
        assert_eq!(analysis.tech_profiles().len(), 1);
    }

    #[tokio::test]
    #[ignore = "until MockApiClient supports response_format (loopctl gap)"]
    async fn analyze_message_error_is_graceful() {
        let client = loopctl::testing::MockApiClient::new("test-model").with_error("boom");
        let result = analyze_message(&client, "anything").await;
        assert!(result.is_err(), "expected StructuredError");
    }
}
