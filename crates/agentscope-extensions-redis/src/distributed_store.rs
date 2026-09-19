//! Redis-backed distributed store combiner, mirroring
//! `io.agentscope.extensions.redis.RedisDistributedStore`.
//!
//! ## Composition (vs. the harness `DistributedStore`)
//!
//! The harness [`DistributedStore`] is presently a **concrete struct** exposing only
//! `agent_state_store` + `isolation_scope` (see rollup tracker §2.2 — field set is
//! insufficient vs. Java, which also carries `base_store` / `sandbox_snapshot_spec` /
//! `sandbox_execution_guard` / `message_bus` / `async_tool_registry` / `task_repository`).
//!
//! Because subagents must not expand the harness, this crate provides its own
//! [`RedisDistributedStore`] combiner carrying every Redis-backed component, plus
//! [`RedisDistributedStore::to_distributed_store`] to hand the harness exactly the
//! slice it currently understands. When the harness `DistributedStore` grows the extra
//! fields, this mapping is the single place to extend.
//!
//! ## Wiring
//!
//! The combiner retains the connection information it was built with, so every component
//! — including the synchronous [`BaseStore`] — can be auto-wired from a single
//! configuration, as Java's `RedisDistributedStore` does:
//!
//! * [`agent_state_store`](RedisDistributedStore::agent_state_store) — async, over the
//!   shared [`RedisConnection`];
//! * [`base_store`](RedisDistributedStore::base_store) — synchronous, over a **blocking**
//!   connection opened from the retained URL (the harness [`BaseStore`] trait is sync and
//!   cannot consume an async `ConnectionManager` without a runtime bridge);
//! * snapshot spec / client and sandbox guard — over the shared connection.
//!
//! `base_store()` returns `Err` only for combiners built **without** a URL
//! ([`RedisDistributedStore::from_connection`] / [`from_components`]) — the documented
//! degradation path, where callers use [`base_store_with_url`].
//!
//! [`from_components`]: RedisDistributedStore::from_components
//! [`base_store_with_url`]: RedisDistributedStore::base_store_with_url

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_core::state::AgentStateStore;
use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;
use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec;
use agentscope_harness::filesystem::remote::store::BaseStore;

use crate::sandbox::RedisSandboxExecutionGuard;
use crate::snapshot::{RedisRemoteSnapshotClient, RedisSnapshotSpec};
use crate::state::adapter::RedisClientAdapter;
use crate::state::connection::RedisConnection;
use crate::state::RedisAgentStateStore;
use crate::store::RedisStore;

/// Default root key prefix; component sub-prefixes are appended to it.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:";

fn normalize_root(prefix: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return DEFAULT_KEY_PREFIX.to_string();
    }
    if trimmed.ends_with(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:")
    }
}

/// Redis-backed configuration bundle that wires all distributed storage components with a
/// single Redis configuration.
///
/// ```no_run
/// # async fn demo() -> agentscope_core::error::Result<()> {
/// use agentscope_extensions_redis::RedisDistributedStore;
/// let store = RedisDistributedStore::connect("redis://localhost:6379").await?;
/// let agent_state = store.agent_state_store()?;
/// let base = store.base_store()?;
/// # let _ = (agent_state, base);
/// # Ok(()) }
/// ```
pub struct RedisDistributedStore {
    connection: RedisConnection,
    /// Adapter used by the agent state store (overridable for in-memory backends).
    adapter: Arc<dyn RedisClientAdapter>,
    /// Redis URL retained at construction so the synchronous `BaseStore` can be opened
    /// later without re-configuration.
    url: Option<String>,
    key_prefix: String,
    isolation_scope: IsolationScope,
}

impl std::fmt::Debug for RedisDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisDistributedStore")
            .field("key_prefix", &self.key_prefix)
            .field("isolation_scope", &self.isolation_scope)
            .field("has_url", &self.url.is_some())
            .finish_non_exhaustive()
    }
}

impl RedisDistributedStore {
    /// Connects to Redis and builds the bundle with the default key prefix.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_prefix(url, DEFAULT_KEY_PREFIX).await
    }

    /// Connects to Redis and builds the bundle with a custom root key prefix.
    pub async fn connect_with_prefix(url: &str, key_prefix: &str) -> Result<Self> {
        let connection = RedisConnection::connect(url).await?;
        Ok(Self {
            adapter: connection.raw_adapter(),
            connection,
            url: Some(url.to_string()),
            key_prefix: normalize_root(key_prefix),
            isolation_scope: IsolationScope::default(),
        })
    }

    /// Builds the bundle from an existing [`RedisConnection`].
    ///
    /// No URL is retained, so [`base_store`](Self::base_store) degrades to an error —
    /// use [`with_url`](Self::with_url) to restore auto-wiring, or
    /// [`base_store_with_url`](Self::base_store_with_url) at the call site.
    pub fn from_connection(connection: RedisConnection, key_prefix: &str) -> Self {
        Self {
            adapter: connection.raw_adapter(),
            connection,
            url: None,
            key_prefix: normalize_root(key_prefix),
            isolation_scope: IsolationScope::default(),
        }
    }

    /// Builds the bundle from explicit components (the pluggable-backend path, e.g. an
    /// in-memory adapter in tests or a custom client wrapper).
    pub fn from_components(
        connection: RedisConnection,
        adapter: Arc<dyn RedisClientAdapter>,
        key_prefix: &str,
    ) -> Self {
        Self {
            connection,
            adapter,
            url: None,
            key_prefix: normalize_root(key_prefix),
            isolation_scope: IsolationScope::default(),
        }
    }

    /// Records the Redis URL used for the synchronous `BaseStore` auto-wiring.
    ///
    /// Lets a bundle built via [`from_connection`](Self::from_connection) /
    /// [`from_components`](Self::from_components) regain a working
    /// [`base_store`](Self::base_store).
    pub fn with_url(mut self, url: &str) -> Self {
        self.url = Some(url.to_string());
        self
    }

    /// Sets the default isolation scope carried into [`to_distributed_store`].
    ///
    /// [`to_distributed_store`]: RedisDistributedStore::to_distributed_store
    pub fn with_isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    /// The configured root key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// The shared Redis connection.
    pub fn connection(&self) -> &RedisConnection {
        &self.connection
    }

    /// The retained Redis URL, if the bundle was built from one.
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// The adapter backing the agent state store.
    pub fn adapter(&self) -> &Arc<dyn RedisClientAdapter> {
        &self.adapter
    }

    /// Agent session state store (auto-wired by `distributed_store`).
    ///
    /// Prefix: `{root}session:`.
    pub fn agent_state_store(&self) -> Result<Arc<dyn AgentStateStore>> {
        let store = RedisAgentStateStore::builder()
            .client(self.adapter.clone())
            .key_prefix(format!("{}session:", self.key_prefix))
            .build()?;
        Ok(Arc::new(store))
    }

    /// Workspace filesystem KV store, auto-wired from the retained URL.
    ///
    /// Prefix: `{root}store:`.
    ///
    /// The harness [`BaseStore`] is synchronous, so this opens a dedicated **blocking**
    /// connection derived from the bundle's URL metadata (the async [`RedisConnection`]
    /// cannot be reused directly without a runtime bridge). The connection is lazy: no
    /// I/O happens here, so wiring a `RemoteFilesystem` is cheap and does not require a
    /// reachable server at construction time.
    ///
    /// # Errors
    /// Returns an error — the documented degradation path — when no URL is available
    /// (i.e. the bundle was built by [`from_connection`](Self::from_connection) or
    /// [`from_components`](Self::from_components) without
    /// [`with_url`](Self::with_url)). Use
    /// [`base_store_with_url`](Self::base_store_with_url) in that case.
    pub fn base_store(&self) -> Result<Arc<dyn BaseStore>> {
        let url = self.url.as_deref().ok_or_else(|| {
            Error::Other(
                "RedisDistributedStore::base_store: no Redis URL retained by this bundle; \
                 use with_url(url) / connect(url) or base_store_with_url(url)"
                    .to_string(),
            )
        })?;
        let prefix = format!("{}store:", self.key_prefix);
        Ok(Arc::new(RedisStore::connect_lazy(url, &prefix)?))
    }

    /// Workspace filesystem KV store built from an explicit Redis URL, validated with
    /// `PING`.
    ///
    /// Prefix: `{root}store:`.
    pub fn base_store_with_url(&self, url: &str) -> Result<Arc<dyn BaseStore>> {
        let prefix = format!("{}store:", self.key_prefix);
        Ok(Arc::new(RedisStore::connect(url, &prefix)?))
    }

    /// Sandbox snapshot spec (auto-wired for sandbox mode).
    ///
    /// Prefix: `{root}snapshot:`.
    pub fn sandbox_snapshot_spec(&self) -> RedisSnapshotSpec {
        RedisSnapshotSpec::new(&self.connection, &format!("{}snapshot:", self.key_prefix), None)
    }

    /// Snapshot spec as a trait object for direct harness wiring.
    pub fn sandbox_snapshot_spec_dyn(&self) -> Arc<dyn SandboxSnapshotSpec> {
        Arc::new(self.sandbox_snapshot_spec())
    }

    /// Remote snapshot client (exposed for callers that build their own spec).
    pub fn snapshot_client(&self) -> RedisRemoteSnapshotClient {
        RedisRemoteSnapshotClient::new(
            &self.connection,
            &format!("{}snapshot:", self.key_prefix),
            None,
        )
    }

    /// Sandbox concurrency guard (auto-wired for sandbox mode).
    ///
    /// Prefix: `{root}guard:`.
    pub fn sandbox_execution_guard(&self) -> RedisSandboxExecutionGuard {
        RedisSandboxExecutionGuard::builder(&self.connection)
            .key_prefix(format!("{}guard:", self.key_prefix))
            .build()
    }

    /// Projects onto the harness [`DistributedStore`].
    ///
    /// # Errors
    /// Returns an error if the agent state store cannot be built.
    pub fn to_distributed_store(&self) -> Result<DistributedStore> {
        Ok(DistributedStore::builder()
            .agent_state_store(self.agent_state_store()?)
            .isolation_scope(self.isolation_scope)
            .build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryRedisAdapter;

    /// Builds a `ConnectionManager` **without** connecting.
    ///
    /// `ConnectionManager::new_lazy_with_config` defers the actual connect until the
    /// first command, so this performs no I/O and no network access.
    fn fake_manager() -> redis::aio::ConnectionManager {
        let client = redis::Client::open("redis://127.0.0.1:6379").expect("valid URL");
        redis::aio::ConnectionManager::new_lazy_with_config(
            client,
            redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy manager")
    }

    fn offline_bundle() -> (RedisDistributedStore, Arc<MemoryRedisAdapter>) {
        let adapter = Arc::new(MemoryRedisAdapter::new());
        let connection = RedisConnection::from_manager(fake_manager());
        let store = RedisDistributedStore::from_components(connection, adapter.clone(), "app");
        (store, adapter)
    }

    #[test]
    fn normalize_root_rules() {
        assert_eq!(normalize_root(""), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_root("  "), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_root("app"), "app:");
        assert_eq!(normalize_root("app:"), "app:");
    }

    #[tokio::test]
    async fn from_connection_normalizes_prefix_and_has_no_url() {
        let store = RedisDistributedStore::from_connection(
            RedisConnection::from_manager(fake_manager()),
            "app",
        );
        assert_eq!(store.key_prefix(), "app:");
        assert_eq!(store.url(), None);
    }

    #[tokio::test]
    async fn retained_url_enables_lazy_base_store_auto_wiring() {
        // No server is contacted at wiring time: `base_store` opens a *lazy* blocking
        // connection, so auto-wiring succeeds purely from the retained URL.
        let store = RedisDistributedStore::from_connection(
            RedisConnection::from_manager(fake_manager()),
            "app",
        )
        .with_url("redis://localhost:6379");

        assert_eq!(store.url(), Some("redis://localhost:6379"));
        assert!(store.base_store().is_ok(), "auto-wires without contacting the server");
    }

    #[tokio::test]
    async fn base_store_degrades_without_a_retained_url() {
        let (store, _adapter) = offline_bundle();
        assert_eq!(store.url(), None);
        let msg = match store.base_store() {
            Ok(_) => panic!("base_store must fail without a retained URL"),
            Err(e) => format!("{e}"),
        };
        assert!(msg.contains("no Redis URL retained"), "{msg}");
        assert!(msg.contains("base_store_with_url"), "degradation path is documented: {msg}");
    }

    #[tokio::test]
    async fn agent_state_store_uses_session_prefix_and_memory_adapter() {
        let (store, adapter) = offline_bundle();
        let state = store.agent_state_store().expect("state store builds");
        state.save(Some("u1"), "s1", "k", serde_json::json!(1)).await;
        assert_eq!(state.get(Some("u1"), "s1", "k").await, Some(serde_json::json!(1)));
        assert!(adapter.string_of("app:session:u1/s1:k").is_some(), "prefix honoured");
    }

    #[tokio::test]
    async fn to_distributed_store_projects_state_store_and_scope() {
        let (store, _adapter) = offline_bundle();
        let store = store.with_isolation_scope(IsolationScope::Session);
        let projected = store.to_distributed_store().expect("projection succeeds");
        assert!(projected.agent_state_store.is_some());
        assert_eq!(projected.isolation_scope, IsolationScope::Session);
    }

    #[tokio::test]
    async fn component_factories_are_constructible_offline() {
        let (store, _adapter) = offline_bundle();
        // Snapshot spec/client and guard only need the connection handle; no I/O here.
        let _spec: Arc<dyn SandboxSnapshotSpec> = store.sandbox_snapshot_spec_dyn();
        let _client = store.snapshot_client();
        let guard = store.sandbox_execution_guard();
        assert_eq!(guard.key_prefix(), "app:guard:");
        assert!(Arc::ptr_eq(store.adapter(), store.adapter()), "adapter is stable");
    }
}
