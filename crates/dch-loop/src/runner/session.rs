//! The session config a runner starts its loop with: the mapped
//! settings and the composed system prompt.

use std::path::Path;

use loopctl::tool::ToolRegistry;

use crate::detect_tech_stack;
use crate::merge_by_language;
use crate::with_context;

/// Compose the session config — including the system prompt — for `config`.
///
/// Maps the runner settings onto a [`loopctl::config::SessionConfig`], detects
/// the tech stack under `workdir`, merges any `[project]` overrides, and
/// composes the full system prompt via [`with_context`] from the active role,
/// the resolved techs, any project-wide conventions, and a per-role prose
/// override when one is configured. The prompt is written into
/// `session_config.system_prompt` before the [`BareLoop`] reads it.
///
/// A free function, apart from [`RunnerBuilder::build`]: the composition
/// is unit-tested on its own, without constructing a provider client.
///
/// [`BareLoop`]: loopctl::engine::BareLoop
/// [`RunnerBuilder::build`]: super::build::RunnerBuilder::build
pub(super) fn build_session(
    config: &dch_config::DchConfig,
    registry: &ToolRegistry,
    workdir: &Path,
) -> loopctl::config::SessionConfig {
    let mut session_config = config.to_session_config();
    let role_override = config.runner.role_override(config.runner.role);
    let detected = detect_tech_stack(workdir);
    let techs = merge_by_language(detected, &config.project);
    let prompt = with_context(
        config.runner.role,
        &techs,
        config.project.conventions.as_deref(),
        role_override,
        registry,
    );
    session_config.system_prompt = Some(prompt);
    session_config
}

#[cfg(test)]
mod tests {
    use super::*;
    use dch_config::DchConfig;
    use loopctl::tool::builtin::fs::FileSession;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn build_session_composes_the_system_prompt_for_general_role() {
        let config = DchConfig::default();
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
        let session = build_session(&config, &registry, Path::new("/tmp"));
        let prompt = session
            .system_prompt
            .as_deref()
            .expect("system_prompt is composed");
        assert_ne!(prompt, "", "the composed system prompt carries a body");
        // Default role is General; its body must be present.
        assert!(
            prompt.contains("YOUR ROLE: GENERAL ASSISTANCE"),
            "General role body missing: {prompt}"
        );
    }

    #[test]
    fn build_session_honors_role_override() {
        use dch_config::Role;
        use dch_config::RoleOverride;
        let mut config = DchConfig::default();
        config.runner.role = Role::Coding;
        config.runner.role_overrides = vec![RoleOverride {
            role: Role::Coding,
            prompt: "OVERRIDE MARKER: do the thing.".to_string(),
        }];
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
        let session = build_session(&config, &registry, Path::new("/tmp"));
        let prompt = session.system_prompt.expect("composed");
        assert!(
            prompt.contains("OVERRIDE MARKER"),
            "override prose missing: {prompt}"
        );
        assert!(
            !prompt.contains("YOUR ROLE: IMPLEMENT FEATURES AND FIXES"),
            "an override must replace the built-in role body, not append to it: {prompt}"
        );
    }

    #[test]
    fn build_session_includes_project_section_when_a_tech_marker_is_present() {
        // End-to-end through build_session: a Cargo.toml marker under workdir
        // must produce a PROJECT section carrying the rust toolchain. Proves
        // detect_tech_stack → merge_by_language → with_context flows through.
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"probe\"\n",
        )
        .expect("write marker");
        let config = DchConfig::default();
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
        let session = build_session(&config, &registry, dir.path());
        let prompt = session.system_prompt.expect("composed");
        assert!(
            prompt.contains("PROJECT"),
            "PROJECT section missing with a Cargo.toml marker: {prompt}"
        );
        assert!(
            prompt.contains("cargo build"),
            "rust build command missing: {prompt}"
        );
    }

    #[test]
    fn build_session_omits_project_section_when_no_tech_marker() {
        // An empty tempdir yields no detected techs, so no PROJECT section.
        let dir = TempDir::new().expect("tempdir");
        let config = DchConfig::default();
        let registry = dch_tools::builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        );
        let session = build_session(&config, &registry, dir.path());
        let prompt = session.system_prompt.expect("composed");
        assert!(
            !prompt.contains("PROJECT"),
            "PROJECT section should be absent with no markers: {prompt}"
        );
    }
}
