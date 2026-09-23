//! Language Server Protocol support for the LSP tool.
//!
//! A stdio JSON-RPC client, the server registry that maps file extensions
//! to server commands, and the per-root process pool that keeps one live
//! server per workspace.

pub(crate) mod client;
pub(crate) mod pool;
pub mod servers;

pub use servers::LspServerConfig;
pub use servers::get_server_for_file;
pub use servers::supported_extensions;
