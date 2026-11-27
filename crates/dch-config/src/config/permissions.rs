//! The `[permissions]` section: its rule schema and its load-time
//! validation.
//!
//! The schema types are re-exported from `dch-permissions`, and
//! [`DchConfig::validate_permissions`] refuses a rule that could never
//! do what it says.

use crate::DchConfig;
use crate::DchConfigError;

pub use dch_permissions::MatcherParts;
pub use dch_permissions::PermissionMode;
pub use dch_permissions::PermissionRuleConfig;
pub use dch_permissions::PermissionRuleOutcome;
pub use dch_permissions::PermissionsConfig;
pub use dch_permissions::SharedPermissionMode;

impl DchConfig {
    /// Reject `[permissions]` rules that could never do what they say.
    ///
    /// A rule naming no matcher key — or naming two — cannot be evaluated
    /// against a call's shape, and a `path_glob` that fails to compile
    /// would silently protect nothing; both refuse with the rule named,
    /// the way the rest of validation turns a silently-rewritten value
    /// into a message. The check itself lives with the rule schema in
    /// the permission domain — one home for what a rule may say and
    /// what it may match — so the message a user reads is the same one
    /// every other consumer of the schema produces.
    ///
    /// # Errors
    ///
    /// Returns [`DchConfigError::Invalid`] for a rule with zero or two
    /// matcher keys, or a `path_glob` the glob compiler rejects.
    pub(super) fn validate_permissions(&self) -> Result<(), DchConfigError> {
        self.permissions.validate().map_err(DchConfigError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DchConfig;

    #[test]
    fn permissions_rules_parse_with_their_matchers_and_verdicts() {
        let config = DchConfig::from_toml_str(
            r#"
[[permissions.rules]]
id = "no-env-writes"
outcome = "deny"
path_glob = "**/.env"

[[permissions.rules]]
outcome = "allow"
shell_prefix = "cargo test"

[[permissions.rules]]
id = "webfetch-asks"
outcome = "ask"
tool = "WebFetch"
"#,
        )
        .expect("a well-formed [permissions] section parses");
        let rules = &config.permissions.rules;
        assert_eq!(rules.len(), 3, "every rule lands");
        assert_eq!(rules[0].outcome, PermissionRuleOutcome::Deny);
        assert_eq!(
            rules[0].path_glob.as_deref(),
            Some("**/.env"),
            "the deny names its glob"
        );
        assert_eq!(rules[1].outcome, PermissionRuleOutcome::Allow);
        assert!(rules[1].id.is_none(), "the id is optional");
        assert_eq!(
            rules[2].outcome,
            PermissionRuleOutcome::Ask,
            "an ask rule tightens the modes that would allow"
        );
        // The projection carries the authored and generated ids in order.
        let compiled = config.permissions.compiled();
        assert_eq!(compiled.len(), 3);
        assert_eq!(compiled[0].0, "no-env-writes", "an authored id is kept");
        assert_eq!(compiled[1].0, "rule-1", "a missing id takes the position");
        assert_eq!(compiled[1].1, PermissionRuleOutcome::Allow);
        assert_eq!(
            compiled[1].2,
            MatcherParts::ShellPrefix("cargo test".to_string()),
        );
        assert_eq!(compiled[2].2, MatcherParts::Tool("WebFetch".to_string()));
    }

    #[test]
    fn a_config_without_a_permissions_section_has_no_rules() {
        let config = DchConfig::from_toml_str("[api]\nmodel = \"m\"\nbase_url = \"http://h/v1\"\n")
            .expect("a pre-permissions config parses");
        assert!(
            config.permissions.rules.is_empty(),
            "the section is optional; its absence configures nothing"
        );
        assert_eq!(
            config.permissions.compiled(),
            Vec::new(),
            "the section is optional; its absence configures nothing"
        );
    }

    #[test]
    fn a_rule_without_exactly_one_matcher_is_a_load_error() {
        for toml in [
            r#"
[[permissions.rules]]
outcome = "deny"
"#,
            r#"
[[permissions.rules]]
outcome = "deny"
tool = "Write"
shell_prefix = "cargo"
"#,
            r#"
[[permissions.rules]]
outcome = "deny"
tool = "Write"
path_glob = "**/.env"
"#,
        ] {
            let why = DchConfig::from_toml_str(toml)
                .expect_err("a rule without exactly one matcher cannot load");
            assert!(
                why.to_string().contains("exactly one of tool"),
                "the error names the one-of contract: {why}"
            );
        }
    }

    #[test]
    fn an_unknown_rule_key_is_a_load_error() {
        let why = DchConfig::from_toml_str(
            r#"
[[permissions.rules]]
outcome = "deny"
glob = "**/.env"
"#,
        )
        .expect_err("a mistyped matcher key cannot load");
        assert!(
            why.to_string().contains("glob"),
            "the error names the unknown key so the typo is one read away: {why}"
        );
    }

    #[test]
    fn a_selected_preset_must_exist_and_its_rules_validate() {
        // [permissions] preset names one of [permissions.presets.<name>];
        // naming nothing configured is a startup error listing the
        // available names, and a preset's own rules go through the same
        // shape checks as the top-level list.
        let why = DchConfig::from_toml_str("[permissions]\npreset = \"nope\"\n")
            .expect_err("a missing selected preset cannot load");
        assert!(
            why.to_string().contains("none is configured"),
            "the empty list is named: {why}"
        );

        let why = DchConfig::from_toml_str(concat!(
            "[permissions]\n",
            "preset = \"nope\"\n",
            "\n",
            "[permissions.presets.rust-dev]\n",
            "rules = [{ outcome = \"allow\", shell_prefix = \"cargo test\" }]\n",
        ))
        .expect_err("a missing named preset cannot load");
        assert!(
            why.to_string().contains("rust-dev"),
            "the error lists the available names: {why}"
        );

        let why = DchConfig::from_toml_str(concat!(
            "[permissions.presets.bad]\n",
            "[[permissions.presets.bad.rules]]\n",
            "outcome = \"deny\"\n",
            "path_glob = \"x[\"\n",
        ))
        .expect_err("a preset with a bad glob cannot load");
        assert!(
            why.to_string().contains("[permissions] preset \"bad\""),
            "the error scopes the preset by name: {why}"
        );

        let ok = DchConfig::from_toml_str(concat!(
            "[permissions]\n",
            "preset = \"rust-dev\"\n",
            "[permissions.presets.rust-dev]\n",
            "rules = [{ outcome = \"allow\", shell_prefix = \"cargo test\" }]\n",
        ))
        .expect("an inline-rules preset parses");
        assert_eq!(ok.permissions.preset.as_deref(), Some("rust-dev"));
        assert_eq!(ok.permissions.presets["rust-dev"].rules.len(), 1);
    }

    #[test]
    fn a_malformed_path_glob_is_a_load_error() {
        let why = DchConfig::from_toml_str(
            r#"
[[permissions.rules]]
id = "broken"
outcome = "deny"
path_glob = "crates/**["
"#,
        )
        .expect_err("an uncompilable glob cannot load");
        assert!(
            why.to_string().contains("crates/**["),
            "the error names the offending pattern: {why}"
        );
    }
}
