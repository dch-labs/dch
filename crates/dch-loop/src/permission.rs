//! The dch-loop permission adapter: loopctl dispatches in, domain
//! verdicts out.
//!
//! The policy itself lives in [`dch_permissions`] — the vocabulary, the
//! engine, the parsers, the memory. This module is the adapter that
//! joins that domain to the agent loop: it derives a [`CallShape`] from
//! a loopctl dispatch (the tool name, the shell judgment, the write
//! targets, the furniture verdict), hands it to the policy, and maps
//! the domain's [`Decision`](dch_permissions::Decision) onto a
//! [`PermissionCheck`] — allowing, denying with
//! a reason naming the mode and category, or surfacing the ask, which
//! the host's typed resolver answers with the whole call in hand. A
//! prompt races the dispatch's cancel signal one layer down, so a
//! cancelled run never executes a tool however the approval would have
//! resolved.

use std::path::Path;

use dch_permissions::CallShape;
use dch_permissions::PermissionMode;
use dch_permissions::Rule as EngineRule;
use dch_permissions::evaluate;
use dch_permissions::shape::command_segments;
use dch_permissions::shape::is_session_furniture;
use dch_permissions::shape::relative_target;
use dch_permissions::shape::shell_reads_only;
use dch_permissions::shape::write_family_targets;
use dch_permissions::vocabulary::PermissionOutcome;
use dch_permissions::vocabulary::ToolCategory;
use dch_permissions::vocabulary::tool_category;
use loopctl::middleware::PermissionAskResolver;
use loopctl::middleware::PermissionMiddleware;
use loopctl::middleware::ToolDispatchContext;
use loopctl::tool::PermissionCheck;

pub use dch_permissions::RememberRules;

/// The session's remembered approvals, read fresh at every dispatch.
///
/// The host holds the live memory ("allow always" answers and a resumed
/// file's adopted rules); the gate takes this snapshot closure instead of
/// the memory itself, so every dispatch consults the memory as it stands
/// at that dispatch — a remember the user made one call ago covers this
/// one, exactly as the resolver-on-sight behavior it replaces did. The
/// snapshot is cheap by contract: the memory is a handful of rules, and
/// cloning it per dispatch costs what the mode-cell read beside it costs.
pub type MemorySnapshot = dch_permissions::MemorySnapshot;

/// The config's compiled rules as a live view, not a snapshot — the
/// preset-selection twin of the mode cell.
///
/// The cell-shaped twin of [`MemorySnapshot`]: a host that moves the
/// preset selection live (`/preset`) swaps the cell's contents, and the
/// closure re-reads them on every evaluation, so the very next dispatch
/// runs under the new join — no rebuild, no rules copied at build
/// time. The name is the contract: caching what the closure returns
/// would silently break the next-dispatch promise the `/preset` swap
/// makes. Cheap by contract: compiling a config's rules is a Vec
/// build, and the closure is consulted once per dispatch.
pub type ConfigRulesView = std::sync::Arc<dyn Fn() -> Vec<EngineRule> + Send + Sync>;

/// The gate's check closure, shared by every builder spelling.
///
/// One pass: read the mode, reduce the dispatch to its [`CallShape`],
/// evaluate the policy, and map the decision onto a
/// [`PermissionCheck`] — an allow passes through with its rule id logged,
/// a block denies with the reason the model reads, and an ask surfaces
/// its prompt for the host's typed resolver, which receives the whole
/// call beside it. The resolver and the snapshots ride in the closure's
/// builder, so the returned middleware is self-contained and the
/// pipeline never sees the seam.
fn permission_gate(
    mode_source: impl Fn() -> PermissionMode + Send + Sync + 'static,
    resolver: Option<std::sync::Arc<dyn PermissionAskResolver>>,
    config_rules: ConfigRulesView,
    memory: MemorySnapshot,
) -> PermissionMiddleware {
    let middleware =
        PermissionMiddleware::from_context().with_named_check("dch-permission-gate", move |ctx| {
            let mode = mode_source();
            let shape = call_shape(ctx);
            let decision = evaluate(mode, &(config_rules)(), &(memory)(), &shape);
            tracing::debug!(
                tool = %ctx.tool_name,
                rule = %decision.rule_id,
                verdict = ?decision.outcome,
                "permission verdict"
            );
            match decision.outcome {
                PermissionOutcome::Allow => PermissionCheck::Allow,
                PermissionOutcome::Block => {
                    let reason = if let Some(id) = decision.rule_id.strip_prefix("config:") {
                        format!("denied by configured rule {id}")
                    } else if decision.rule_id == "remembered:deny" {
                        "denied by a remembered refusal (deny always)".to_string()
                    } else {
                        format!("denied by {mode:?} mode ({:?} tool)", shape.category)
                    };
                    PermissionCheck::Deny { reason }
                }
                PermissionOutcome::Ask => ask_check(mode, ctx, shape.category),
            }
        });
    match resolver {
        Some(resolver) => middleware.with_ask_resolver_typed(resolver),
        None => middleware,
    }
}

/// Build the permission gate for `mode`, optionally resolving `Ask`
/// cells through `resolver`.
///
/// The frozen spelling: `mode` is fixed at build time — the builder's
/// convenience path, and hosts that never move the mode mid-run. The
/// returned middleware maps every tool dispatch through the full policy
/// with no config rules and nothing remembered: a deny carries the mode
/// and category in its reason (the pipeline formats the model-visible
/// error as `Permission {reason} for tool '{tool}'`), and an ask carries
/// a prompt naming the tool, its category, and the mode. Without a
/// resolver an `Ask` degrades to a denial inside the middleware —
/// correct for headless runs, which have no one to ask.
#[must_use]
pub fn permission_layer(
    mode: PermissionMode,
    resolver: Option<std::sync::Arc<dyn PermissionAskResolver>>,
) -> PermissionMiddleware {
    permission_gate(
        move || mode,
        resolver,
        std::sync::Arc::new(Vec::new),
        std::sync::Arc::new(dch_permissions::empty_view),
    )
}

/// Build the permission gate over a shared, live mode cell.
///
/// The [`permission_layer`] spelling freezes the boot mode; this one reads
/// the cell on **every dispatch**, so a host that moves
/// the mode while a run is in flight — `/mode` mid-run — has the
/// very next tool call under the new mode, no rebuild owed.
#[must_use]
pub fn permission_layer_shared(
    mode: dch_config::SharedPermissionMode,
    resolver: Option<std::sync::Arc<dyn PermissionAskResolver>>,
) -> PermissionMiddleware {
    permission_gate(
        move || {
            *mode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        },
        resolver,
        std::sync::Arc::new(Vec::new),
        std::sync::Arc::new(dch_permissions::empty_view),
    )
}

/// Build the production gate: live mode cell, config rules, and the
/// session's remembered approvals.
///
/// The precedence is [`evaluate`]'s own — config rules first, then the
/// remembered layer, then the built-in matrix with its carve-outs. The
/// mode is read from the cell on **every dispatch**, so a `/mode` mid-run
/// has the very next call under the new mode; the memory snapshot is
/// consulted per dispatch too, so an "allow always" answer covers the
/// very next variant of the same work with no resolver detour — a covered
/// ask never reaches the resolver at all, and the verdict log names
/// `remembered` as the rule that allowed it. Without a resolver an `Ask`
/// degrades to a denial inside the middleware — correct for headless
/// runs, which have no one to ask.
#[must_use]
pub fn permission_layer_live(
    mode: dch_config::SharedPermissionMode,
    resolver: Option<std::sync::Arc<dyn PermissionAskResolver>>,
    config_rules: ConfigRulesView,
    memory: MemorySnapshot,
) -> PermissionMiddleware {
    permission_gate(
        move || {
            *mode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        },
        resolver,
        config_rules,
        memory,
    )
}

/// The config's `[permissions]` rules, compiled into engine rules.
///
/// A thin pass-through over
/// [`PermissionsConfig::engine_rules`](dch_permissions::PermissionsConfig::engine_rules)
/// — the host hands the loaded config, the domain does the conversion,
/// and the compile cannot fail because the config's validation compiled
/// every glob at load with the same settings.
#[must_use]
pub fn config_rules(config: &dch_config::DchConfig) -> Vec<EngineRule> {
    config.permissions.engine_rules()
}

/// The `Ask` verdict, surfaced with its display prompt.
///
/// The prompt names the tool, its category, and the active mode — pure
/// display, and never an identity: the typed ask resolver receives the
/// whole call beside it (raw `input`, call id, turn), so nothing here is
/// built to be matched on later and no format is load-bearing.
fn ask_check(
    mode: PermissionMode,
    ctx: &ToolDispatchContext,
    category: ToolCategory,
) -> PermissionCheck {
    PermissionCheck::Ask {
        prompt: ask_prompt(mode, &ctx.tool_name, category),
    }
}

/// Reduce a dispatch to the axes the policy engine reads.
///
/// The derivation is the whole adapter's knowledge of a call: the
/// registered name and its category, the furniture verdict (the
/// session's own todo, question, and jobs reads), the reader-gate pass
/// for a shell command, the non-reader verb prefixes, and the write
/// targets spelled relative to the working directory. Everything
/// downstream — config rules, remembered coverage, the matrix — reads
/// only this shape, so the parsers live in the domain and nowhere else.
fn call_shape(ctx: &ToolDispatchContext) -> CallShape {
    let command = if ctx.tool_name == "Bash" {
        ctx.input.get("command").and_then(serde_json::Value::as_str)
    } else {
        None
    };
    CallShape {
        tool: ctx.tool_name.clone(),
        category: tool_category(&ctx.tool_name),
        furniture: is_session_furniture(&ctx.tool_name, &ctx.input),
        shell_reads_only: command.is_some_and(shell_reads_only),
        segments: command.map_or_else(Vec::new, command_segments),
        write_targets: dispatch_write_targets(ctx),
    }
}

/// The write targets a dispatch would overwrite, relative to the cwd.
///
/// The write-family names only: `Write` and `Edit` carry one target, a
/// `MultiEdit` batch carries one per entry. Each is spelled relative to
/// the working directory — the spelling path globs are written in — and
/// every other tool carries no targets at all.
fn dispatch_write_targets(ctx: &ToolDispatchContext) -> Vec<String> {
    let raw = write_family_targets(&ctx.tool_name, &ctx.input);
    let cwd = Path::new(&ctx.tool_context.cwd);
    raw.iter()
        .map(|target| relative_target(target, cwd))
        .collect()
}

/// The ask's prompt: the tool, its category, and the active mode.
///
/// Pure display — the typed ask carries the raw `input`, and the sheet
/// that renders the approval reads the command from it, so the prompt
/// carries no payload that could drift from what actually runs.
fn ask_prompt(mode: PermissionMode, tool_name: &str, category: ToolCategory) -> String {
    format!("Allow '{tool_name}' ({category:?}) in {mode:?} mode?")
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]
mod tests {
    use super::*;
    use dch_permissions::MemoryView;

    fn dispatch(tool: &str, input: serde_json::Value) -> ToolDispatchContext {
        ToolDispatchContext {
            tool_name: tool.to_string(),
            input,
            call_id: format!("call_{tool}"),
            turn_number: 0,
            cancel: std::sync::Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::Allow,
            tool_context: loopctl::tool::ToolContext::default(),
        }
    }

    fn verdict(mode: PermissionMode, ctx: &ToolDispatchContext) -> dch_permissions::Decision {
        evaluate(mode, &[], &MemoryView::empty(), &call_shape(ctx))
    }

    #[test]
    fn an_ask_names_the_tool_category_and_mode_alone() {
        // The prompt is pure display — the typed ask carries the raw
        // input, and the sheet reads the command from it — so the
        // wording stays a question and carries no payload.
        let bash = ask_prompt(
            PermissionMode::Interactive,
            "Bash",
            ToolCategory::ShellExecute,
        );
        assert!(
            bash.contains("Bash") && bash.contains("Interactive"),
            "the ask names the tool and the mode: {bash}"
        );
        assert!(
            !bash.contains("cargo"),
            "the prompt carries no command payload: {bash}"
        );
        let write = ask_prompt(
            PermissionMode::Interactive,
            "Write",
            ToolCategory::FileWrite,
        );
        assert_eq!(
            write, "Allow 'Write' (FileWrite) in Interactive mode?",
            "every ask is the same one-line question"
        );
    }

    #[test]
    fn the_sessions_own_furniture_runs_free_in_every_mode() {
        // The todo list and the question tool are the conversation's
        // own furniture: prompting for permission to take a note or
        // ask a question is a prompt about a prompt. They run free in
        // every mode — ReadOnly included, which keeps its todo list and
        // its clarifying questions through a mode that touches
        // nothing — while the tools that reach the session's outsides
        // follow the matrix.
        for tool in ["TodoWrite", "AskUserQuestion"] {
            for mode in [
                PermissionMode::Auto,
                PermissionMode::ReadOnly,
                PermissionMode::Interactive,
            ] {
                assert_eq!(
                    verdict(mode, &dispatch(tool, serde_json::json!({}))).rule_id,
                    "builtin:furniture",
                    "{mode:?} runs {tool} without a prompt"
                );
            }
        }
        assert_eq!(
            verdict(
                PermissionMode::Interactive,
                &dispatch("Submit", serde_json::json!({}))
            )
            .outcome,
            PermissionOutcome::Ask,
            "Submit reaches the session's outsides — it still asks"
        );
        assert_eq!(
            verdict(
                PermissionMode::ReadOnly,
                &dispatch("Submit", serde_json::json!({}))
            )
            .outcome,
            PermissionOutcome::Block,
            "ReadOnly touches nothing outside the conversation"
        );
        // The jobs table's reads are furniture of the same kind:
        // listing it and polling a job's status touch nothing, so they
        // run free everywhere — while cleanup_jobs, the mutator,
        // still follows the matrix.
        for operation in ["jobs", "job_status"] {
            for mode in [
                PermissionMode::Auto,
                PermissionMode::ReadOnly,
                PermissionMode::Interactive,
            ] {
                assert_eq!(
                    verdict(
                        mode,
                        &dispatch("Jobs", serde_json::json!({ "operation": operation }))
                    )
                    .rule_id,
                    "builtin:furniture",
                    "{mode:?} reads the jobs table ({operation}) without a prompt"
                );
            }
        }
        assert_eq!(
            verdict(
                PermissionMode::Interactive,
                &dispatch("Jobs", serde_json::json!({ "operation": "cleanup_jobs" }))
            )
            .outcome,
            PermissionOutcome::Ask,
            "cleanup_jobs mutates the table — it still asks"
        );
        // Reads are free everywhere: a command that only looks runs
        // under Interactive like a file read does — and under ReadOnly,
        // which looks as freely as it reads.
        for mode in [
            PermissionMode::Auto,
            PermissionMode::ReadOnly,
            PermissionMode::Interactive,
        ] {
            assert_eq!(
                verdict(
                    mode,
                    &dispatch(
                        "Bash",
                        serde_json::json!({ "command": "git log --oneline" })
                    )
                )
                .rule_id,
                "builtin:reader-gate",
                "{mode:?} runs a command that only looks"
            );
        }
        // A command that does more than look is matrix shell like any
        // other: ReadOnly refuses it outright, without asking.
        assert_eq!(
            verdict(
                PermissionMode::ReadOnly,
                &dispatch("Bash", serde_json::json!({ "command": "find . -delete" }))
            )
            .outcome,
            PermissionOutcome::Block,
            "ReadOnly refuses a mutating command without asking"
        );
    }

    #[test]
    fn every_registered_builtin_tool_classifies_as_the_gate_will() {
        // The registry↔category drift pin, relocated: this crate sees
        // both the tool registry (dch-tools) and the classification
        // table (the domain), so a registered tool falling to the
        // fail-closed category by accident fails here.
        let registry = dch_tools::registry::builtin_registry(
            &loopctl::tool::builtin::fs::FileSession::new(std::path::PathBuf::from(".")),
            dch_tools::registry::ReadWindows::default(),
        );
        let registered: Vec<String> = registry
            .all_tools()
            .into_iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert!(!registered.is_empty(), "the registry must not be empty");
        for name in registered {
            assert_ne!(
                tool_category(&name),
                ToolCategory::Unclassified,
                "{name} is registered but classifies as Unclassified — add it to the \
                 category table"
            );
        }
    }

    #[test]
    fn the_config_and_domain_mode_are_one_type() {
        // The old config↔tools mirror and its conversion point are
        // gone: the config re-exports the domain enum, so the spellings
        // line up by construction.
        assert_eq!(dch_config::PermissionMode::Auto.as_str(), "auto");
        // The retired accept_edits spelling loads through the migration
        // mapping onto interactive.
        assert_eq!(
            dch_config::PermissionMode::from_wire("accept_edits"),
            Some(dch_config::PermissionMode::Interactive),
        );
        assert_eq!(
            std::mem::discriminant(&dch_config::PermissionMode::ReadOnly),
            std::mem::discriminant(&PermissionMode::ReadOnly),
        );
    }
}
