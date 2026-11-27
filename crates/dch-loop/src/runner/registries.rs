//! The tool registries a runner serves: the path-resolution policies,
//! the MCP server connections, and the composition that keeps the
//! advertised and the dispatchable tool sets identical.

use std::path::Path;

use dch_tools::ResolvePolicy;
use dch_tools::builtin_registry;
use loopctl::mcp::CommandSpec;
use loopctl::mcp::McpClient;
use loopctl::mcp::McpToolProvider;
use loopctl::tool::Tool;
use loopctl::tool::ToolRegistry;
use loopctl::tool::builtin::fs::FileSession;
use loopctl::tool::builtin::fs::ResolvePolicy as FsResolvePolicy;

use crate::RunnerError;

/// Spawn and adapt every config-declared MCP server.
///
/// Each `[[mcp.servers]]` entry is launched over the stdio transport and its
/// tools discovered under the server's name prefix. A server that fails to
/// spawn or complete the MCP handshake fails construction naming the server —
/// a silently missing tool set is worse than a clear startup error. The child
/// runs in `workdir`, the same directory builtin tools resolve paths against.
///
/// # Errors
///
/// Returns [`RunnerError::Config`] naming the failing server on any transport
/// or handshake failure.
pub(super) async fn connect_mcp_servers(
    config: &dch_config::DchConfig,
    workdir: &Path,
) -> Result<Vec<McpConnection>, RunnerError> {
    let mut connections = Vec::new();
    for server in &config.mcp.servers {
        let command = CommandSpec {
            program: server.command.clone(),
            args: server.args.clone(),
            env: server
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            cwd: Some(workdir.to_string_lossy().into_owned()),
        };
        let client = McpClient::stdio(command)
            .await
            .map_err(|e| RunnerError::Config(format!("mcp server '{}': {e}", server.name)))?;
        connections.push(adopt_mcp_server(server, client).await?);
    }
    Ok(connections)
}

/// Connect an already-running MCP `client` as `server` and snapshot
/// its tools under the server's allowlist.
///
/// The provider is connected first; each allowlisted name is then
/// checked against what the server actually exposed, so a listed
/// name the server does not offer fails construction rather than
/// silently dropping a tool.
///
/// # Errors
///
/// Returns [`RunnerError::Config`] when the handshake fails or an
/// allowlisted tool name is not offered by the server.
async fn adopt_mcp_server(
    server: &dch_config::McpServerConfig,
    client: McpClient,
) -> Result<McpConnection, RunnerError> {
    let provider = McpToolProvider::connect(client, Some(server.name.clone()))
        .await
        .map_err(|e| RunnerError::Config(format!("mcp server '{}': {e}", server.name)))?;
    let allowed = match &server.tools {
        None => None,
        Some(names) => {
            let exposed: std::collections::HashSet<String> = provider
                .tools()
                .iter()
                .map(|tool| tool.name().to_string())
                .collect();
            let mut allowlist = std::collections::HashSet::new();
            for name in names {
                let exposed_name = format!("{}__{name}", server.name);
                if !exposed.contains(&exposed_name) {
                    return Err(RunnerError::Config(format!(
                        "mcp server '{}': tool '{name}' is not offered by the server",
                        server.name
                    )));
                }
                allowlist.insert(exposed_name);
            }
            Some(allowlist)
        }
    };
    Ok(McpConnection { provider, allowed })
}

/// One connected MCP server, adapted into the registry alongside its
/// containment policy.
///
/// Built by [`adopt_mcp_server`] and consumed by [`compose_registry`]:
/// the provider carries the discovered tools, the allowlist decides
/// which of them a composition registers.
pub(super) struct McpConnection {
    /// The connected provider.
    ///
    /// `tools` lists what the server exposed at discovery; `register_into`
    /// adapts them into a [`ToolRegistry`].
    provider: McpToolProvider,

    /// The exposed (prefixed) tool names to adapt.
    ///
    /// `None` adapts every discovered tool; a populated set adapts only the
    /// listed ones and leaves the rest unregistered.
    allowed: Option<std::collections::HashSet<String>>,
}

/// The resolve policy a runner context carries for the given runner config.
///
/// `unsafe_paths` is the explicit opt-out: off (the default) keeps every
/// file tool confined to the working directory, on lets them reach any path
/// the OS permits.
pub(super) fn resolve_policy_for(runner: &dch_config::RunnerConfig) -> ResolvePolicy {
    if runner.unsafe_paths {
        ResolvePolicy::Unrestricted
    } else {
        ResolvePolicy::Contained
    }
}

/// The resolve policy the filesystem session carries for the given config.
///
/// The same `unsafe_paths` switch mapped onto the filesystem family's own
/// policy type, so the builtin read and writing tools judge containment by
/// the identical setting the search tools see.
pub(super) fn fs_resolve_policy_for(runner: &dch_config::RunnerConfig) -> FsResolvePolicy {
    if runner.unsafe_paths {
        FsResolvePolicy::Unrestricted
    } else {
        FsResolvePolicy::Contained
    }
}

/// Compose the full tool registry: the builtin tools plus every MCP server's.
///
/// Each server contributes per its containment policy: a connection without
/// an allowlist registers every discovered tool, an allowlisted connection
/// only the listed ones. Called once per consumer — the engine's by-value
/// registry and the dispatch pipeline's core cannot share boxed tools, so
/// each gets its own composition from the same connections (an MCP tool
/// clones into both, keeping the two positions identical by construction).
/// `ask_tools` selects the builtin half: the full composition or the
/// non-interactive one that omits `AskUserQuestion`.
fn compose_registry(
    connections: &[McpConnection],
    fs_session: &FileSession,
    windows: dch_tools::ReadWindows,
    ask_tools: bool,
) -> ToolRegistry {
    let registry = if ask_tools {
        builtin_registry(fs_session, windows)
    } else {
        dch_tools::non_interactive_registry(fs_session, windows)
    };
    for connection in connections {
        match &connection.allowed {
            None => connection.provider.register_into(&registry),
            Some(allowed) => {
                for tool in connection.provider.tools() {
                    if allowed.contains(tool.name()) {
                        registry.register(tool.clone());
                    }
                }
            }
        }
    }
    registry
}

/// The two registries one runner serves: advertised and dispatchable.
///
/// What the engine's registry advertises to the model and what the
/// pipeline's core registry can run are the same composition of the
/// same inputs — builtins, interactive-tool gating, every MCP
/// connection, and the extra MCP providers landing in both — so the
/// twin composition lives in this one seam. The legs share every
/// argument by construction and the provider loop names both
/// registries, and the registry-twin pin drives this seam itself: a
/// change that wires one leg differently fails there rather than
/// shipping a runner whose advertised tools and dispatchable tools
/// disagree.
pub(super) fn compose_twin_registries(
    connections: &[McpConnection],
    fs_session: &FileSession,
    windows: dch_tools::ReadWindows,
    ask_tools: bool,
    mcp_providers: &[McpToolProvider],
) -> (ToolRegistry, ToolRegistry) {
    let registry = compose_registry(connections, fs_session, windows, ask_tools);
    let core_registry = compose_registry(connections, fs_session, windows, ask_tools);
    for provider in mcp_providers {
        provider.register_into(&registry);
        provider.register_into(&core_registry);
    }
    (registry, core_registry)
}

/// The read-family window ceilings the config's `[runner]` section selects.
///
/// Maps the config fields straight onto dch-tools' [`ReadWindows`], keeping
/// the runner the only place config becomes tool construction. The
/// conversation's ceiling on one tool result doubles as the `ReadFiles`
/// batch budget, so the per-file ceiling, the batch budget, and the
/// dispatched-result cap come from one place and cannot drift apart.
pub(super) fn read_windows_for(runner: &dch_config::RunnerConfig) -> dch_tools::ReadWindows {
    dch_tools::ReadWindows {
        max_lines: runner.read_max_lines,
        max_bytes: runner.read_max_bytes,
        max_batch_bytes: runner.max_tool_output_chars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Runner;
    use crate::runner::test_support::{
        SseServer, recorded_requests, sse_text_turn, sse_tool_call_turn, wire_config,
    };
    use serde_json::Value;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn read_windows_for_carries_the_output_cap_into_the_batch_budget() {
        let runner = dch_config::RunnerConfig {
            max_tool_output_chars: 4096,
            read_max_bytes: 2048,
            ..dch_config::RunnerConfig::default()
        };
        let windows = read_windows_for(&runner);
        assert_eq!(
            (windows.max_bytes, windows.max_batch_bytes),
            (2048, 4096),
            "the per-file ceiling and the batch budget come from the same config\n\
             pair as the dispatched-result cap, so the three cannot drift"
        );
        let disabled = dch_config::RunnerConfig {
            max_tool_output_chars: 0,
            ..dch_config::RunnerConfig::default()
        };
        assert_eq!(
            read_windows_for(&disabled).max_batch_bytes,
            0,
            "the disable sentinel maps through rather than collapsing to a default"
        );
    }

    #[test]
    fn unsafe_paths_config_wires_unrestricted_resolution() {
        // The config switch maps onto both policy tracks the runner carries:
        // the search tools' policy and the file family's own. Anything the
        // config leaves unset stays contained on both.
        let config = dch_config::DchConfig::default();
        assert_eq!(
            resolve_policy_for(&config.runner),
            ResolvePolicy::Contained,
            "the default config keeps tools contained"
        );
        assert_eq!(
            fs_resolve_policy_for(&config.runner),
            FsResolvePolicy::Contained,
            "the default config keeps the file family contained"
        );
        let mut opted_out = dch_config::DchConfig::default();
        opted_out.runner.unsafe_paths = true;
        assert_eq!(
            resolve_policy_for(&opted_out.runner),
            ResolvePolicy::Unrestricted,
            "the opt-out must lift containment"
        );
        assert_eq!(
            fs_resolve_policy_for(&opted_out.runner),
            FsResolvePolicy::Unrestricted,
            "the opt-out must lift the file family's containment too"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_contained_runner_refuses_a_read_that_escapes_the_workdir() {
        // The file family judges containment by the session the runner
        // attaches, so the contained default must refuse an escaping read
        // through the real dispatch path — not merely by construction.
        let server = SseServer::start(vec![
            sse_tool_call_turn("Read", &serde_json::json!({"path": "../secret.txt"})),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        std::fs::write(outside.path().join("secret.txt"), "SHOULD NOT BE READ")
            .expect("seed outside target");
        let mut runner = Runner::builder(&wire_config(server.port, 10), dir.path())
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("read it"))
            .await
            .expect("run settles within timeout")
            .expect("a refused tool is a soft error, not a run failure");
        assert_eq!(run.output.as_deref(), Some("done"));
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && requests[1].contains("escapes the working directory"),
            "the containment refusal must ride the follow-up request: {requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_advertised_and_executable_registries_carry_the_same_tools() {
        // The engine's registry and the pipeline's core registry are
        // one seam's two hands — [`compose_twin_registries`], the
        // helper `build()` serves both legs from — so what the engine
        // advertises is exactly what a dispatch can run. The pin
        // drives the seam itself with a live provider aboard, not a
        // composition of its own: a change that composes one leg
        // differently, gates one leg only, or registers the provider
        // into one registry fails here rather than shipping a runner
        // whose halves disagree.
        let fs_session = FileSession::new(PathBuf::from("."));
        let windows = read_windows_for(&dch_config::RunnerConfig::default());
        let names = |registry: &loopctl::tool::ToolRegistry| {
            let mut names: Vec<String> = registry
                .all_tools()
                .into_iter()
                .map(|tool| tool.name().to_string())
                .collect();
            names.sort();
            names
        };
        let client = loopctl::mcp::McpClient::in_process(GreetServer::new())
            .await
            .expect("mcp handshake");
        let provider = loopctl::mcp::McpToolProvider::connect(client, Some("demo".to_string()))
            .await
            .expect("tool discovery");
        let providers = [provider];
        let (advertised, core) =
            compose_twin_registries(&[], &fs_session, windows, true, &providers);
        assert_eq!(
            names(&advertised),
            names(&core),
            "the seam's two hands agree"
        );
        assert!(
            names(&advertised).iter().any(|name| name == "Read"),
            "the builtin family is present"
        );
        assert!(
            names(&advertised).iter().any(|name| name == "demo__greet"),
            "the provider's tool is aboard the advertised half"
        );
        assert!(
            names(&core).iter().any(|name| name == "demo__greet"),
            "the provider's tool is aboard the dispatchable half"
        );
        // The interactive gating holds identically on both legs: the
        // one interaction-only tool drops out together or not at all.
        let (quiet_advertised, quiet_core) =
            compose_twin_registries(&[], &fs_session, windows, false, &[]);
        assert!(
            !names(&quiet_advertised)
                .iter()
                .any(|name| name == "AskUserQuestion"),
            "the quiet composition drops the interaction-only tool"
        );
        assert_eq!(names(&quiet_advertised), names(&quiet_core));
    }

    #[tokio::test]
    async fn the_engine_and_core_registries_agree_on_every_concurrency_verdict() {
        // The engine plans parallel waves from its own registry while the
        // pipeline executes through the core's. The two constructions must
        // agree on every tool's identity and concurrency verdict — a drift
        // would let a plan serialize or parallelize calls the executor
        // disagrees with, while the schemas alone still matched. Both
        // compositions carry an in-process MCP server's tool beside the
        // builtin set, so the walk covers the external registration path
        // too: a provider that joined only one side fails the twin lookup
        // outright.
        let client = loopctl::mcp::McpClient::in_process(GreetServer::new())
            .await
            .expect("mcp handshake");
        let connection = adopt_mcp_server(&demo_server_config(None), client)
            .await
            .expect("adopt the demo server");
        let session = FileSession::new(PathBuf::from("."));
        let engine = compose_registry(
            std::slice::from_ref(&connection),
            &session,
            dch_tools::ReadWindows::default(),
            true,
        );
        let core = compose_registry(
            std::slice::from_ref(&connection),
            &session,
            dch_tools::ReadWindows::default(),
            true,
        );
        for registry in [&engine, &core] {
            assert!(
                registry.contains("demo__greet"),
                "both composed registries carry the external tool"
            );
        }

        let inputs = [
            Value::Null,
            serde_json::json!({"command": "echo hi"}),
            serde_json::json!({"files": ["a.rs"]}),
            serde_json::json!({"todos": []}),
            serde_json::json!({"file_path": "a.rs", "content": "x"}),
        ];
        for tool in engine.all_tools() {
            let name = tool.name();
            let twin = core
                .get(name)
                .unwrap_or_else(|| panic!("the core registry must carry {name}"));
            assert_eq!(
                tool.is_concurrency_safe(),
                twin.is_concurrency_safe(),
                "{name} must share its static concurrency verdict"
            );
            for input in &inputs {
                assert_eq!(
                    tool.is_safe_for_concurrent_execution(input),
                    twin.is_safe_for_concurrent_execution(input),
                    "{name} must share its verdict for {input}"
                );
            }
        }
    }

    /// An rmcp server exposing one `greet` tool, for the MCP registration test.
    ///
    /// Serves over an in-memory transport, so the registration pin
    /// exercises the real MCP handshake and tool discovery without
    /// spawning a process or opening a socket.
    #[derive(Clone)]
    struct GreetServer {
        // rmcp's tool_handler macro reaches the router through its generated
        // trait impl, so rustc cannot see the read.
        #[allow(dead_code)]
        router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    }

    #[rmcp::tool_router]
    impl GreetServer {
        fn new() -> Self {
            Self {
                router: Self::tool_router(),
            }
        }

        #[rmcp::tool(description = "Return a friendly greeting")]
        async fn greet(&self) -> String {
            "hello, world!".to_string()
        }

        #[rmcp::tool(description = "Say goodbye")]
        async fn farewell(&self) -> String {
            "goodbye!".to_string()
        }
    }

    #[allow(clippy::unused_async_trait_impl)] // rmcp's tool_handler emits un-awaited trait impls
    #[rmcp::tool_handler]
    impl rmcp::handler::server::ServerHandler for GreetServer {}

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_provider_tools_join_the_registry_and_dispatch() {
        let client = loopctl::mcp::McpClient::in_process(GreetServer::new())
            .await
            .expect("mcp handshake");
        let provider = loopctl::mcp::McpToolProvider::connect(client, Some("demo".to_string()))
            .await
            .expect("tool discovery");
        let server = SseServer::start(vec![
            sse_tool_call_turn("demo__greet", &serde_json::json!({})),
            sse_text_turn("done"),
        ])
        .await;
        let dir = TempDir::new().expect("tempdir");
        let config = wire_config(server.port, 7);
        let mut runner = Runner::builder(&config, dir.path())
            .with_mcp_provider(provider)
            .build()
            .await
            .expect("constructs");
        let run = tokio::time::timeout(std::time::Duration::from_secs(10), runner.run("greet"))
            .await
            .expect("run completes")
            .expect("run succeeds");
        assert_eq!(run.output.as_deref(), Some("done"));
        assert_eq!(run.tool_call_count(), 1, "the MCP tool was dispatched");
        let requests = recorded_requests(&server);
        assert!(
            requests.len() >= 2 && requests[1].contains("hello, world!"),
            "the MCP tool result must flow back to the model: {requests:?}"
        );
    }

    fn demo_server_config(tools: Option<Vec<String>>) -> dch_config::McpServerConfig {
        dch_config::McpServerConfig {
            name: "demo".to_string(),
            command: "unused".to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            tools,
        }
    }

    #[tokio::test]
    async fn an_mcp_allowlist_registers_only_listed_tools() {
        let connection = adopt_mcp_server(
            &demo_server_config(Some(vec!["greet".to_string()])),
            loopctl::mcp::McpClient::in_process(GreetServer::new())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let registry = compose_registry(
            &[connection],
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
            true,
        );

        assert!(
            registry.contains("demo__greet"),
            "the allowlisted tool must be registered"
        );
        assert!(
            !registry.contains("demo__farewell"),
            "an unlisted tool must stay unregistered"
        );
        let expected = builtin_registry(
            &FileSession::new(PathBuf::from(".")),
            dch_tools::ReadWindows::default(),
        )
        .len()
            + 1;
        assert_eq!(registry.len(), expected, "exactly one external tool joins");
    }

    #[tokio::test]
    async fn an_unknown_allowlisted_tool_fails_startup_naming_both() {
        let err = adopt_mcp_server(
            &demo_server_config(Some(vec!["nope".to_string()])),
            loopctl::mcp::McpClient::in_process(GreetServer::new())
                .await
                .unwrap(),
        )
        .await
        .map(|_| ())
        .unwrap_err();
        let RunnerError::Config(msg) = &err else {
            panic!("expected Config error, got {err:?}");
        };
        assert!(
            msg.contains("demo") && msg.contains("nope"),
            "the error must name the server and the missing tool: {msg}"
        );
    }
}
