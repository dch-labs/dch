//! Per-project-root language-server pool.
//!
//! Keeps at most one live server per project root for the process
//! lifetime, so the first query in a workspace pays the server's startup
//! and indexing and subsequent queries reuse it. A process-wide static:
//! the registry is constructed more than once per runner, and only a
//! shared pool guarantees one server per root across all of them.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;

use super::client::LspClient;
use super::client::SpawnError;
use super::servers::LspServerConfig;

/// The process-wide server pool.
///
/// Shared by every `LspTool` instance so one rust-analyzer serves all
/// queries for a given project root.
static SERVER_POOL: LazyLock<ServerPool> = LazyLock::new(ServerPool::new);

/// Serializes every test that can spawn or evict a pooled server.
///
/// Spawn-counter assertions read global deltas, so a concurrent spawn or
/// eviction from another test would make the deltas lie. Every
/// spawn-capable test holds this gate for its whole body.
#[cfg(test)]
pub(crate) static SPAWN_GATE: Mutex<()> = Mutex::const_new(());

/// A pool of live language servers keyed by project root.
///
/// Guarantees at most one server per root for the whole process: the
/// tool registry is rebuilt per runner, so only this shared holder can
/// deduplicate spawns across registries. Entries are evicted when an
/// exchange on them fails (see [`ServerPool::evict`]) so the next call
/// cold-starts a fresh server; otherwise they live for the process
/// lifetime, a live server serving every later query for its root.
pub(crate) struct ServerPool {
    /// Live clients by project root.
    ///
    /// Guarded by an async mutex because a cold entry spawns and
    /// initializes a server — an await — while holding the guard, which
    /// also serializes first-spawn per root.
    servers: Mutex<HashMap<PathBuf, Arc<Mutex<LspClient>>>>,

    /// How many servers this pool has started, ever.
    ///
    /// Monotonic and never reset; the reuse test reads it to prove the
    /// second query in a root did not spawn again.
    spawns: AtomicU32,
}

impl ServerPool {
    /// An empty pool; no server is started until first use.
    ///
    /// Spawning is deliberately lazy — a process that never queries a
    /// Rust file never pays the server's startup and indexing cost.
    fn new() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
            spawns: AtomicU32::new(0),
        }
    }

    /// The client for `root`, starting one under `config` if none is live.
    ///
    /// A present entry is returned as-is — until an exchange on it fails
    /// and the caller evicts it, the entry is the live server for the
    /// root. A missing entry spawns and initializes a server bound to
    /// `root_uri`, inserts it, and returns the shared handle. Callers
    /// serialize on the returned client's own mutex; one operation runs
    /// per server at a time.
    ///
    /// # Errors
    ///
    /// Propagates [`SpawnError`] from the spawn or the initialization
    /// handshake; nothing is inserted on failure.
    pub(crate) async fn get_or_spawn(
        &self,
        root: &Path,
        root_uri: &url::Url,
        config: &LspServerConfig,
    ) -> Result<Arc<Mutex<LspClient>>, SpawnError> {
        let mut servers = self.servers.lock().await;
        if let Some(client) = servers.get(root) {
            return Ok(Arc::clone(client));
        }
        let client = LspClient::start(config, root_uri).await?;
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let client = Arc::new(Mutex::new(client));
        servers.insert(root.to_path_buf(), Arc::clone(&client));
        Ok(client)
    }

    /// Remove `root`'s pooled entry when it is still `client`.
    ///
    /// Called after an exchange on `client` failed — a crashed,
    /// desynced, or wedged server is worse than useless: every later
    /// query would fail identically. The identity check keeps a fresh
    /// server another caller already spawned from being evicted for the
    /// failure of its predecessor. In-flight holders keep their `Arc`;
    /// the server dies only when the last handle drops.
    pub(crate) async fn evict(&self, root: &Path, client: &Arc<Mutex<LspClient>>) {
        let mut servers = self.servers.lock().await;
        if let Some(pooled) = servers.get(root)
            && Arc::ptr_eq(pooled, client)
        {
            servers.remove(root);
        }
    }

    /// How many servers this pool has started, ever.
    ///
    /// Test-only observation point: comparing the counter across two
    /// calls is how the reuse test tells a warm reuse from a silent
    /// respawn.
    #[cfg(test)]
    pub(crate) fn spawn_count(&self) -> u32 {
        self.spawns.load(Ordering::SeqCst)
    }
}

/// Acquire the pooled client for `root`, spawning under `config` on first
/// use.
///
/// The process-wide static's [`ServerPool::get_or_spawn`]; kept behind a
/// free function so callers never name the static.
///
/// # Errors
///
/// Propagates [`SpawnError`] from the spawn or initialization.
pub(crate) async fn pooled_client(
    root: &Path,
    root_uri: &url::Url,
    config: &LspServerConfig,
) -> Result<Arc<Mutex<LspClient>>, SpawnError> {
    SERVER_POOL.get_or_spawn(root, root_uri, config).await
}

/// Evict `root`'s pooled entry when it is still `client`.
///
/// The failure-facing twin of [`pooled_client`]: call it after any
/// exchange error on a pooled client so the next call cold-starts a
/// fresh server instead of inheriting the dead one.
pub(crate) async fn evict_root(root: &Path, client: &Arc<Mutex<LspClient>>) {
    SERVER_POOL.evict(root, client).await;
}

/// The number of servers the process-wide pool has started, ever.
///
/// A test-only accessor on the same footing as
/// [`pooled_client`]: tests observe spawn behavior without naming the
/// static themselves.
#[cfg(test)]
pub(crate) fn pool_spawn_count() -> u32 {
    SERVER_POOL.spawn_count()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use std::sync::Arc;

    use lsp_types::Position;

    use super::SPAWN_GATE;
    use super::evict_root;
    use super::pooled_client;
    use crate::lsp::client::fakes::fake_server;
    use crate::lsp::client::fakes::frame;
    use serde_json::json;

    /// The initialize response every fake server starts with.
    fn init_frame() -> String {
        frame(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"capabilities": {"positionEncoding": "utf-8"}}
        }))
    }

    #[tokio::test]
    async fn a_server_reply_error_keeps_the_pooled_client() {
        let gate = SPAWN_GATE.lock().await;
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_path_buf();
        let root_uri = url::Url::from_file_path(&root).unwrap();

        let error_frame = frame(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {"code": -32602, "message": "invalid params"}
        }));
        let (live_home, config) = fake_server(&[init_frame(), error_frame], "sleep 2");
        let first = pooled_client(&root, &root_uri, &config).await.unwrap();
        {
            let mut client = first.lock().await;
            let err = client
                .hover(&root_uri, Position::new(0, 0))
                .await
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    crate::lsp::client::RequestError::Server(ref e)
                        if e.to_string().contains("-32602")
                ),
                "a JSON-RPC error reply classifies as Server: {err:?}"
            );
        }
        let second = pooled_client(&root, &root_uri, &config).await.unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "a server reply error must not cost the pooled server"
        );
        drop(live_home);
        drop(gate);
    }

    #[tokio::test]
    async fn an_evicted_root_cold_starts_a_fresh_server() {
        let gate = SPAWN_GATE.lock().await;
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_path_buf();
        let root_uri = url::Url::from_file_path(&root).unwrap();

        let (die_home, die_config) = fake_server(&[init_frame()], "sleep 0.2");
        let dead = pooled_client(&root, &root_uri, &die_config).await.unwrap();
        {
            let mut client = dead.lock().await;
            client
                .hover(&root_uri, Position::new(0, 0))
                .await
                .expect_err("the dying server cannot answer the hover");
        }
        drop(die_home);

        evict_root(&root, &dead).await;

        let hover_frame = frame(&json!({"jsonrpc": "2.0", "id": 2, "result": null}));
        let (live_home, live_config) = fake_server(&[init_frame(), hover_frame], "sleep 2");
        let fresh = pooled_client(&root, &root_uri, &live_config).await.unwrap();
        assert!(
            !Arc::ptr_eq(&dead, &fresh),
            "the failed entry must not be handed out again"
        );
        {
            let mut client = fresh.lock().await;
            let hover = client.hover(&root_uri, Position::new(0, 0)).await.unwrap();
            assert!(hover.is_none());
        }
        drop(live_home);
        drop(gate);
    }
}
