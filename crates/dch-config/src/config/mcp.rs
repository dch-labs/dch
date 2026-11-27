//! The `[[mcp.servers]]` entries: external MCP tool servers spawned
//! over stdio at startup.
//!
//! [`McpConfig`] lists the servers in configuration order and
//! [`McpServerConfig`] is one server's command, arguments,
//! environment, and tool allow-list.

/// MCP tool-server settings, in the `[mcp]` config section.
///
/// Each entry names one external MCP server whose tools are adapted into the
/// agent's tool registry at startup, exposed as `{name}__{tool}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct McpConfig {
    /// Servers to connect at startup, in configuration order.
    ///
    /// Defaults to empty — no external tools.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One `[[mcp.servers]]` entry: an MCP server to spawn over stdio.
///
/// The command is launched as a child process and speaks the MCP stdio
/// transport; a server that fails to start or complete its handshake is a
/// startup error (fail-closed) rather than a silently missing tool set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct McpServerConfig {
    /// Short name used to prefix the server's tools (`{name}__{tool}`).
    ///
    /// Also names the server in startup errors.
    pub name: String,

    /// Executable to spawn.
    ///
    /// Resolved on `PATH` or taken as an absolute path.
    pub command: String,

    /// Arguments passed to the executable.
    ///
    /// Defaults to empty.
    #[serde(default)]
    pub args: Vec<String>,

    /// Extra environment variables for the child process.
    ///
    /// Defaults to empty.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,

    /// Restrict the server to these tool names.
    ///
    /// Defaults to `None` — every tool the server exposes is adapted. A
    /// listed name the server does not offer is a startup error rather than
    /// a silently missing tool.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use crate::DchConfig;

    #[test]
    fn present_but_empty_mcp_section_parses_as_no_servers() {
        let c: DchConfig = toml::from_str("[mcp]\n").unwrap();
        assert_eq!(
            c.mcp.servers,
            Vec::new(),
            "the section's presence alone configures no servers"
        );
    }
}
