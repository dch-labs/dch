//! TOML configuration loading for `dch`.
//!
//! [`DchConfig`] is the root table; every section type is re-exported
//! here, so consumers import from the crate root alone.

#![warn(missing_docs)]
// Tests assert on internals and fixtures: panicking helpers,
// `unwrap` and `expect` on states the fixture itself built, and
// docs-free test fns are the test canon, so the denied-lint set
// stands down for the whole crate under `cfg(test)` — one
// justification, in one place, instead of one per module.
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::let_underscore_must_use,
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        clippy::panic,
        clippy::unwrap_used,
    )
)]

mod config;

pub use self::config::api::{
    ApiConfig, ApiType, FALLBACK_CONTEXT_WINDOW, REASONING_EFFORT_WIRE_NAMES, is_reasoning_effort,
};
pub use self::config::display::{
    CaretDisplay, DiffLayout, DiffPalette, DisplayConfig, PathStyle, SplashColors, StripPosition,
    StripStyle, Verbosity,
};
pub use self::config::mcp::{McpConfig, McpServerConfig};
pub use self::config::models::ModelProfile;
pub use self::config::permissions::{
    MatcherParts, PermissionMode, PermissionRuleConfig, PermissionRuleOutcome, PermissionsConfig,
    SharedPermissionMode,
};
pub use self::config::project::{ProjectConfig, TechProfile};
pub use self::config::runner::{
    CompactorSelection, Role, RoleOverride, RunnerConfig, TURN_TIMEOUT_FLOOR_SECS,
};
pub use self::config::security::SecurityConfig;
pub use self::config::telemetry::TelemetryConfig;
pub use self::config::{DchConfig, DchConfigError, config_dir};

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the key names a serialized value carries, descending
    /// tables and arrays of tables.
    ///
    /// Sections, their fields, and the fields inside each array entry
    /// all count as knobs, so a nested field the surface walk never
    /// saw is demanded like any other.
    fn collect_knobs(value: &serde_json::Value, names: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(table) => {
                for (key, inner) in table {
                    names.push(key.clone());
                    collect_knobs(inner, names);
                }
            }
            serde_json::Value::Array(entries) => {
                for entry in entries {
                    collect_knobs(entry, names);
                }
            }
            _ => {}
        }
    }

    /// Whether the sample spells `name` as a configuration key.
    ///
    /// A line counts only where the knob appears in one of TOML's key
    /// spellings — `name =`, a `[section]` header, or an
    /// `[[array.entry]]` header — commented or not. A prose mention
    /// satisfies none of them, which is the teeth the sweep exists
    /// for.
    fn sample_documents(sample: &str, name: &str) -> bool {
        sample.lines().any(|line| {
            let trimmed = line.trim();
            let spelled = trimmed.strip_prefix("# ").unwrap_or(trimmed);
            if let Some(header) = spelled.strip_prefix('[') {
                return header
                    .trim_end_matches(']')
                    .trim_matches(['[', ']'])
                    .split('.')
                    .any(|segment| segment == name);
            }
            spelled.starts_with(&format!("{name} =")) || spelled.starts_with(&format!("{name}="))
        })
    }

    /// Knobs of `config` the sample never spells as a key.
    ///
    /// The report drives the example-config sweep: an empty list is
    /// the passing verdict, and each entry names a knob whose
    /// documentation is missing or merely prose.
    fn undocumented_knobs(sample: &str, config: &DchConfig) -> Vec<String> {
        let mut names = Vec::new();
        collect_knobs(
            &serde_json::to_value(config).expect("the config serializes"),
            &mut names,
        );
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter(|name| !sample_documents(sample, name))
            .collect()
    }

    /// The default config plus one role override and one technology.
    ///
    /// The default's nested tables are empty, so a walk of it sees
    /// `role_overrides` but never the fields inside an entry; the
    /// fixture contributes those names mechanically.
    fn config_with_nested_entries() -> DchConfig {
        let mut config = DchConfig::default();
        config.runner.role_overrides.push(RoleOverride {
            role: Role::Coding,
            prompt: "plan the work".to_string(),
        });
        config.project.techs.push(TechProfile {
            language: "rust".to_string(),
            build: Some("cargo build".to_string()),
            test: None,
            lint: None,
            conventions: None,
        });
        config.models.push(ModelProfile {
            model: "a-hosted-model".to_string(),
            ..ModelProfile::default()
        });
        config
    }

    #[test]
    fn the_knob_walk_descends_into_nested_tables() {
        let fixture = config_with_nested_entries();
        let without_nested = concat!(
            "# [[runner.role_overrides]]\n",
            "# role = \"coding\"\n",
            "# [[project.techs]]\n",
            "# language = \"rust\"\n",
        );
        let report = undocumented_knobs(without_nested, &fixture);
        for nested in ["prompt", "build", "test", "lint", "conventions"] {
            assert!(
                report.iter().any(|name| name == nested),
                "the walk demands the nested `{nested}` a flat sweep never saw: {report:?}"
            );
        }
        let with_nested = concat!(
            "# [[runner.role_overrides]]\n",
            "# role = \"coding\"\n",
            "# prompt = \"plan the work\"\n",
            "# [[project.techs]]\n",
            "# language = \"rust\"\n",
            "# build = \"cargo build\"\n",
            "# test = \"cargo test\"\n",
            "# lint = \"cargo clippy\"\n",
            "# conventions = \"none\"\n",
        );
        let report = undocumented_knobs(with_nested, &fixture);
        for nested in ["prompt", "build", "test", "lint", "conventions"] {
            assert!(
                !report.iter().any(|name| name == nested),
                "a key-shaped `{nested} =` line documents the nested knob: {report:?}"
            );
        }
    }

    #[test]
    fn a_prose_mention_does_not_count_as_documenting_a_knob() {
        let prose = "# raise compact_threshold when sessions run long\n";
        let report = undocumented_knobs(prose, &DchConfig::default());
        assert!(
            report.iter().any(|name| name == "compact_threshold"),
            "a prose mention is not a key-shaped spelling: {report:?}"
        );
        let keyed = "# compact_threshold = 80\n";
        let report = undocumented_knobs(keyed, &DchConfig::default());
        assert!(
            !report.iter().any(|name| name == "compact_threshold"),
            "the key-shaped spelling documents the knob: {report:?}"
        );
    }

    #[test]
    fn the_example_config_documents_every_configured_knob() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example.config");
        let sample = std::fs::read_to_string(path).expect("example.config is readable");
        toml::from_str::<DchConfig>(&sample).expect("example.config parses as a config");
        let report = undocumented_knobs(&sample, &config_with_nested_entries());
        assert!(
            report.is_empty(),
            "every knob the walk can see must appear in a key-shaped spelling: {report:?}"
        );
        // reasoning_effort skip-serializes when unset, so the walk
        // cannot see it; its presence is asserted by name.
        assert!(
            sample_documents(&sample, "reasoning_effort"),
            "example.config must document the `reasoning_effort` knob"
        );
    }

    #[test]
    fn test_default_config() {
        let c = DchConfig::default();
        assert_eq!(c.api.model, "");
        assert_eq!(c.api.api_type, ApiType::Ollama);
        assert_eq!(c.api.max_tokens, 32_000);
        assert_eq!(c.api.request_timeout_secs, 120);
        assert_eq!(c.display.verbosity, Verbosity::Normal);
        assert_eq!(c.display.theme, "dch");
        assert_eq!(c.runner.max_turns, 200);
        assert_eq!(c.runner.turn_timeout_secs, 300);
        assert_eq!(c.runner.compact_threshold, 80);
        assert_eq!(c.runner.compactor, CompactorSelection::Qa);
        assert_eq!(c.runner.permission_mode, PermissionMode::Auto);
        assert_eq!(c.telemetry.level, "info");
        assert!(c.security.redact_secrets);
    }

    #[test]
    fn an_unknown_reasoning_effort_fails_the_load_naming_the_ladder() {
        // The ladder is a construction contract: an unknown rung kills
        // client construction at startup, so the loader refuses it
        // here — for the `[api]` value and for a `[[models]]` entry's
        // own override, either spelling of the same brick — naming the
        // rungs that exist. The write-through's gate reads the same
        // list, so what /set may write, the loader accepts.
        let err = DchConfig::from_toml_str("[api]\nmodel = \"m\"\nreasoning_effort = \"hihg\"\n")
            .expect_err("a typo is not a rung");
        let message = err.to_string();
        assert!(
            message.contains("hihg") && message.contains("xhigh"),
            "the refusal names the value and the ladder: {message}"
        );
        let err = DchConfig::from_toml_str(
            "[api]\nmodel = \"base\"\n\n[[models]]\nmodel = \"m\"\nreasoning_effort = \"turbo\"\n",
        )
        .expect_err("an entry's override rides the same ladder");
        assert!(
            err.to_string().contains("turbo"),
            "the entry refusal names the value: {err}"
        );
        DchConfig::from_toml_str("[api]\nmodel = \"m\"\nreasoning_effort = \"xhigh\"\n")
            .expect("every rung of the ladder parses");
    }

    #[test]
    fn the_example_config_parses_to_the_recorded_document() {
        // The parse is pinned to a recorded rendering: the file is read
        // through the real loader and written back out, so a change to
        // the schema, to a default, or to the file itself surfaces as a
        // diff against the record instead of a silent shift. To
        // re-record, write what the loaded config's `to_toml_string`
        // returns into the fixture.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example.config");
        let sample = std::fs::read_to_string(path).expect("example.config is readable");
        let config = DchConfig::from_toml_str(&sample).expect("example.config loads as a config");
        assert_eq!(
            config
                .to_toml_string()
                .expect("the loaded config serializes"),
            include_str!("example.config.parsed.toml"),
            "example.config must parse to the recorded document"
        );
    }
}
