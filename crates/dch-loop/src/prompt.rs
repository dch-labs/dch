//! System-prompt assembly for the agent loop.
//!
//! The system prompt is built from three parts: a [`Role`] (who the agent
//! acts as — its prose comes from [`Role::system_prompt`]), an optional set of
//! [`TechProfile`]s (what stacks it works on — detected from the repo, or
//! overridden in `[project]` config), and one short prose fragment per
//! registered tool whose [`loopctl::tool::Tool::system_prompt`] returns
//! `Some`. The role text lives with the role; the tech profiles live in
//! [`crate::project`]; the per-tool fragments are contributed by the tools
//! themselves at runtime.
//!
//! The builder is pure and side-effect-free: given the same inputs it yields
//! the same string. Working and temporary directory context is appended by
//! the caller at runner construction, not here.

use crate::project::render_techs;
use dch_config::Role;
use dch_config::TechProfile;
use loopctl::tool::ToolRegistry;

/// Agent discipline shared by every role.
///
/// Prepended to each role's body so the cross-cutting conduct rules live in
/// exactly one place. Deliberately stack-agnostic: it says *how* to work
/// (understand before acting, read what you change), not *what* stack the
/// project is — the stack is the tech-profile axis, detected separately, so a
/// C++ or Haskell or Bash project is not mis-described by hard-coded language
/// defaults.
const SHARED_DISCIPLINE: &str = "\
You are a proactive agent operating on the user's machine.

CORE CONDUCT
- Understand before you act. Read the relevant files, logs, or config before
  changing anything; inspect the thing you intend to modify.
- Form a complete enough picture of the task before editing, adding, or
  removing anything. Ask one clarifying question only when a genuine ambiguity
  blocks progress — never ask \"which file?\" or \"what should I do?\"; find
  it and make a reasonable choice.
- Surface a brief plan before sizable writes so the change is reviewable; then
  apply it.
- When you do not know the stack or conventions, explore the project (list
  files, read config and build manifests) and follow what you find rather than
  guessing.

IMAGES
- Read detects image files (png, jpg, jpeg, webp, gif) and returns them as
  structured image content. Do not call external or non-existent tools such as
  \"analyze_image\" — they do not exist.

FILE PATHS
- Write within the working directory unless the user names a specific location.
  Do not write to system directories. Use the temp directory for scratch and
  intermediate output.";

/// Assemble the system prompt from the default role, no tech profiles, and
/// each registered tool's [`loopctl::tool::Tool::system_prompt`] fragment.
///
/// Equivalent to [`with_context`] with [`Role::General`] (the default), no
/// techs, and no project conventions. The runner composes the full prompt —
/// with detected techs and cwd/temp — via [`with_context`].
///
/// # Examples
///
/// ```
/// use dch_loop::build_system_prompt;
/// let registry = loopctl::tool::ToolRegistry::new();
/// let prompt = build_system_prompt(&registry);
/// assert!(!prompt.is_empty());
/// ```
#[must_use]
pub fn build_system_prompt(tools: &ToolRegistry) -> String {
    with_context(Role::General, &[], None, None, tools)
}

/// Assemble the system prompt from a chosen [`Role`] and each registered
/// tool's [`loopctl::tool::Tool::system_prompt`] fragment, with no tech
/// profiles.
///
/// Convenience for callers that have selected a role but have no detected
/// stack; equivalent to [`with_context`] with empty techs and no conventions.
#[must_use]
pub fn with_role(role: Role, tools: &ToolRegistry) -> String {
    with_context(role, &[], None, None, tools)
}

/// Assemble the full system prompt from a [`Role`], tech profiles, and each
/// registered tool's [`loopctl::tool::Tool::system_prompt`] fragment.
///
/// The result is the shared discipline, then the role body, then (when
/// non-empty) a `PROJECT` section rendered from `techs` and
/// `project_conventions`, then one `## <Tool>` section per tool whose
/// `system_prompt()` returns `Some` — appended in alphabetical order by tool
/// name. Tools returning `None` contribute nothing. Order is deterministic:
/// the same inputs always yield the same string.
///
/// `role_prompt_override`, when `Some`, replaces the role body — the shared
/// discipline, tech profile, and per-tool fragments still append. The runner
/// resolves it from `[runner].role_overrides` for the selected role (see
/// `RunnerConfig::role_override`); pass `None` to use the role's built-in
/// [`Role::system_prompt`].
///
/// The caller (the runner) is responsible for appending the working and temp
/// directory paths; this function returns role + project + fragments only.
#[must_use]
pub fn with_context(
    role: Role,
    techs: &[TechProfile],
    project_conventions: Option<&str>,
    role_prompt_override: Option<&str>,
    tools: &ToolRegistry,
) -> String {
    let mut out = String::new();
    out.push_str(SHARED_DISCIPLINE);
    out.push_str("\n\n");
    let role_body = role_prompt_override.unwrap_or_else(|| role.system_prompt());
    out.push_str(role_body);
    let rendered = render_techs(techs, project_conventions);
    if !rendered.is_empty() {
        out.push_str("\n\n");
        out.push_str(&rendered);
    }
    append_fragments(&mut out, tools);
    out
}

/// Append one `## <Name>` section per tool that has a `system_prompt`
/// fragment, in alphabetical order by tool name.
///
/// Tools whose [`Tool::system_prompt`] returns `None` are skipped — they
/// contribute nothing to the prompt. The local re-sort (after `tool_names()`
/// already sorts) keeps the ordering contract owned by this module, so a
/// future loopctl change to `tool_names()` cannot silently reorder fragments.
fn append_fragments(out: &mut String, tools: &ToolRegistry) {
    let mut fragments: Vec<(String, String)> = tools
        .tool_names()
        .into_iter()
        .filter_map(|name| {
            let tool = tools.get(&name)?;
            tool.system_prompt().map(|frag| (name, frag))
        })
        .collect();
    // tool_names() is already sorted; re-sort locally so the ordering
    // contract is owned by this module and survives a future change upstream.
    fragments.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, frag) in fragments {
        out.push_str("\n\n## ");
        out.push_str(&name);
        out.push_str("\n\n");
        out.push_str(&frag);
    }
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
    use dch_config::TechProfile;
    use loopctl::tool::Tool;
    use loopctl::tool::ToolContext;
    use loopctl::tool::ToolError;
    use loopctl::tool::ToolOutput;
    use loopctl::tool::ToolSchema;
    use serde_json::Value;
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;

    /// A throwaway tool that optionally contributes a system-prompt fragment.
    struct FragTool {
        name: &'static str,
        frag: Option<&'static str>,
    }

    impl Tool for FragTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "test tool"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                tool: self.name.to_string(),
                description: self.description().to_string(),
                input_schema: json!({}),
            }
        }
        fn call(
            &self,
            _input: Value,
            _ctx: &ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
            Box::pin(async { Ok(ToolOutput::text("ok")) })
        }
        fn system_prompt(&self) -> Option<String> {
            self.frag.map(str::to_string)
        }
    }

    /// Build a registry from a list of test tools.
    fn reg(tools: Vec<FragTool>) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for t in tools {
            r.register(t);
        }
        r
    }

    #[test]
    fn default_role_is_general_and_present_on_empty_registry() {
        let prompt = build_system_prompt(&ToolRegistry::new());
        assert!(prompt.contains("GENERAL ASSISTANCE"), "{prompt}");
        assert!(
            !prompt.contains("\n\n## "),
            "no fragments on empty registry: {prompt}"
        );
    }

    #[test]
    fn every_role_returns_distinct_prose_via_system_prompt() {
        // The role is the single source of truth: each variant's
        // system_prompt() must return distinct prose with its task header.
        let roles = [
            (Role::General, "GENERAL ASSISTANCE"),
            (Role::Coding, "IMPLEMENT FEATURES AND FIXES"),
            (Role::Refactor, "WITHOUT CHANGING BEHAVIOR"),
            (Role::Debug, "REPRODUCE, ISOLATE"),
            (Role::Review, "REVIEW AND REPORT"),
            (Role::Docs, "WRITE OR REVISE DOCUMENTATION"),
            (Role::Tests, "WRITE AND IMPROVE TESTS"),
        ];
        let mut bodies: Vec<&'static str> = Vec::new();
        for (role, header) in roles {
            let prose = role.system_prompt();
            assert!(prose.contains(header), "{role:?}: missing {header:?}");
            bodies.push(prose);
        }
        let unique: std::collections::HashSet<&&str> = bodies.iter().collect();
        assert_eq!(unique.len(), bodies.len(), "roles must differ");
    }

    #[test]
    fn shared_discipline_present_in_every_role_and_is_stack_agnostic() {
        for role in [
            Role::General,
            Role::Coding,
            Role::Refactor,
            Role::Debug,
            Role::Review,
            Role::Docs,
            Role::Tests,
        ] {
            let prompt = with_role(role, &ToolRegistry::new());
            assert!(
                prompt.contains("Understand before you act"),
                "role {role:?} missing conduct rule"
            );
            assert!(
                prompt.contains("IMAGES"),
                "role {role:?} missing image policy"
            );
            assert!(
                prompt.contains("FILE PATHS"),
                "role {role:?} missing path policy"
            );
            assert!(
                !prompt.contains("DEFAULT FILE SEARCH PATTERNS"),
                "role {role:?} leaked language globs into shared discipline"
            );
        }
    }

    #[test]
    fn banned_terms_absent_from_every_role() {
        for role in [
            Role::General,
            Role::Coding,
            Role::Refactor,
            Role::Debug,
            Role::Review,
            Role::Docs,
            Role::Tests,
        ] {
            let prompt = with_role(role, &ToolRegistry::new());
            for banned in [
                "TaskStatus",
                "NARROW",
                "subagent",
                "clamped to 600",
                "WebSearch",
            ] {
                assert!(
                    !prompt.contains(banned),
                    "role {role:?} contains banned term {banned:?}"
                );
            }
        }
    }

    #[test]
    fn with_context_injects_polyglot_techs_and_conventions() {
        let empty = ToolRegistry::new();
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
        let prompt = with_context(
            Role::Coding,
            &techs,
            Some("conventional commits"),
            None,
            &empty,
        );
        assert!(prompt.contains("PROJECT"), "{prompt}");
        assert!(prompt.contains("Language: rust"), "{prompt}");
        assert!(prompt.contains("Language: bash"), "{prompt}");
        assert!(
            prompt.contains("Project conventions: conventional commits"),
            "{prompt}"
        );
    }

    #[test]
    fn with_context_sections_in_documented_order() {
        let r = reg(vec![FragTool {
            name: "Zeta",
            frag: Some("z"),
        }]);
        let techs = vec![TechProfile {
            language: "rust".to_string(),
            build: Some("cargo build".to_string()),
            test: None,
            lint: None,
            conventions: None,
        }];
        let prompt = with_context(Role::Coding, &techs, Some("fmt"), None, &r);
        let discipline = prompt.find("CORE CONDUCT").unwrap();
        let role_body = prompt.find("IMPLEMENT FEATURES").unwrap();
        let project = prompt.find("PROJECT").unwrap();
        let fragment = prompt.find("## Zeta").unwrap();
        assert!(
            discipline < role_body && role_body < project && project < fragment,
            "sections out of order: discipline={discipline}, role={role_body}, project={project}, fragment={fragment}\n{prompt}"
        );
    }

    #[test]
    fn with_context_omits_project_section_when_empty() {
        let empty = ToolRegistry::new();
        let prompt = with_context(Role::Coding, &[], None, None, &empty);
        assert!(
            !prompt.contains("PROJECT"),
            "empty techs + no conventions should add no section: {prompt}"
        );
    }

    #[test]
    fn single_fragment_appended_under_header() {
        let r = reg(vec![FragTool {
            name: "Alpha",
            frag: Some("do alpha"),
        }]);
        let prompt = with_role(Role::Coding, &r);
        assert!(prompt.contains("\n\n## Alpha\n\ndo alpha"), "{prompt}");
    }

    #[test]
    fn none_fragment_omitted() {
        let r = reg(vec![FragTool {
            name: "Silent",
            frag: None,
        }]);
        let prompt = with_role(Role::Coding, &r);
        assert!(
            !prompt.contains("## Silent"),
            "None-fragment tool should be silent: {prompt}"
        );
    }

    #[test]
    fn multiple_fragments_sorted_and_stable() {
        let r = reg(vec![
            FragTool {
                name: "Zeta",
                frag: Some("z"),
            },
            FragTool {
                name: "Alpha",
                frag: Some("a"),
            },
            FragTool {
                name: "Mu",
                frag: Some("m"),
            },
        ]);
        let prompt = with_role(Role::Coding, &r);
        let alpha = prompt.find("## Alpha").unwrap();
        let mu = prompt.find("## Mu").unwrap();
        let zeta = prompt.find("## Zeta").unwrap();
        assert!(
            alpha < mu && mu < zeta,
            "fragments must be alphabetical: {prompt}"
        );
        let prompt2 = with_role(Role::Coding, &r);
        assert_eq!(prompt, prompt2);
    }

    #[test]
    fn role_prompt_override_replaces_role_body_keeps_rest() {
        // When set, the override replaces the role's built-in prose; the
        // shared discipline, tech profile, and fragments still append.
        let empty = ToolRegistry::new();
        let override_body = "CUSTOM ROLE PROSE: do the thing your way.";
        let prompt = with_context(Role::Debug, &[], None, Some(override_body), &empty);
        // Override is used in place of the role's built-in body.
        assert!(
            prompt.contains("CUSTOM ROLE PROSE"),
            "override missing: {prompt}"
        );
        assert!(
            !prompt.contains("REPRODUCE, ISOLATE"),
            "built-in debug prose should be replaced, not appended: {prompt}"
        );
        // Shared discipline still present.
        assert!(
            prompt.contains("Understand before you act"),
            "discipline dropped: {prompt}"
        );
    }

    #[test]
    fn role_prompt_override_still_appends_fragments_and_techs() {
        let r = reg(vec![FragTool {
            name: "Tool",
            frag: Some("frag"),
        }]);
        let techs = vec![TechProfile {
            language: "rust".to_string(),
            build: Some("cargo build".to_string()),
            test: None,
            lint: None,
            conventions: None,
        }];
        let prompt = with_context(Role::Coding, &techs, None, Some("OVERRIDE BODY"), &r);
        assert!(prompt.contains("OVERRIDE BODY"), "override: {prompt}");
        assert!(prompt.contains("Language: rust"), "techs: {prompt}");
        assert!(
            prompt.contains("\n\n## Tool\n\nfrag"),
            "fragments: {prompt}"
        );
    }
}
