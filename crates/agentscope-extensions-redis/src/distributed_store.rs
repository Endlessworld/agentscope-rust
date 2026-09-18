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

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_core::state::AgentStateStore;
use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;
use agentscope_harness::filesystem::remote::store::BaseStore;
use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec;

use crate::sandbox::RedisSandboxExecutionGuard;
use crate::snapshot::{RedisRemoteSnapshotClient, RedisSnapshotSpec};
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
/// single Redis connection.
///
/// ```no_run
/// # async fn demo() -> agentscope_core::error::Result<()> {
/// use agentscope_extensions_redis::RedisDistributedStore;
/// let store = RedisDistributedStore::connect("redis://localhost:6379").await?;
/// let agent_state = store.agent_state_store();
/// let base = store.base_store();
/// # let _ = (agent_state, base);
/// # Ok(()) }
/// ```
pub struct RedisDistributedStore {
    connection: RedisConnection,
    key_prefix: String,
    isolation_scope: IsolationScope,
}

impl std::fmt::Debug for RedisDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisDistributedStore")
            .field("key_prefix", &self.key_prefix)
            .field("isolation_scope", &self.isolation_scope)
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
            connection,
            key_prefix: normalize_root(key_prefix),
            isolation_scope: IsolationScope::default(),
        })
    }

    /// Builds the bundle from an existing [`RedisConnection`].
    pub fn from_connection(connection: RedisConnection, key_prefix: &str) -> Self {
        Self {
            connection,
            key_prefix: normalize_root(key_prefix),
            isolation_scope: IsolationScope::default(),
        }
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

    /// Agent session state store (auto-wired by `distributed_store`).
    ///
    /// Prefix: `{root}session:`.
    pub fn agent_state_store(&self) -> Result<Arc<dyn AgentStateStore>> {
        let store = RedisAgentStateStore::builder()
            .client(Arc::new(self.connection.clone()))
            .key_prefix(format!("{}session:", self.key_prefix))
            .build()?;
        Ok(Arc::new(store))
    }

    /// Workspace filesystem KV store.
    ///
    /// Prefix: `{root}store:`.
    ///
    /// Uses a blocking connection derived from the same URL semantics; callers wiring a
    /// `RemoteFilesystem` need the sync [`BaseStore`] shape.
    pub fn base_store(&self) -> Result<Arc<dyn BaseStore>> {
        // The BaseStore trait is synchronous, so we open a dedicated blocking connection
        // lazily via RedisStore::connect using the shared URL is not possible here (the
        // URL is not retained). Callers that need a base store should use
        // `RedisStore::connect` directly, or `base_store_with_url`.
        Err(Error::Other(
            "RedisDistributedStore::base_store requires a URL; use base_store_with_url(url)"
                .to_string(),
        ))
    }

    /// Workspace filesystem KV store built from an explicit Redis URL.
    ///
    /// Prefix: `{root}store:`.
    pub fn base_store_with_url(&self, url: &str) -> Result<Arc<dyn BaseStore>> {
        let prefix = format!("{}store:", self.key_prefix);
        let store = RedisStore::connect(url, &prefix)?;
        Ok(Arc::new(store))
    }

    /// Sandbox snapshot spec (auto-wired for sandbox mode).
    ///
    /// Prefix: `{root}snapshot:`.
    pub fn sandbox_snapshot_spec(&self) -> RedisSnapshotSpec {
        RedisSnapshotSpec::new(
            &self.connection,
            &format!("{}snapshot:", self.key_prefix),
            None,
        )
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

    #[test]
    fn normalize_root_rules() {
        assert_eq!(normalize_root(""), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_root("app"), "app:");
        assert_eq!(normalize_root("app:"), "app:");
    }
}
