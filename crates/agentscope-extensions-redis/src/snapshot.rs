//! Redis-backed sandbox snapshot storage, mirroring
//! `io.agentscope.extensions.redis.snapshot.*`.
//!
//! * [`RedisRemoteSnapshotClient`] implements the harness [`RemoteSnapshotClient`] trait
//!   using Redis binary values (archives stored as `{prefix}{snapshotId}.tar`).
//! * [`RedisSnapshotSpec`] is a convenience [`SandboxSnapshotSpec`] wrapping the client.

use std::sync::Arc;

use async_trait::async_trait;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshotSpec,
};

use crate::state::connection::RedisConnection;

/// Default key prefix for snapshot archives.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:sandbox:snapshots:";

fn normalize_prefix(prefix: &str) -> String {
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

/// [`RemoteSnapshotClient`] backed by Redis binary values.
#[derive(Clone)]
pub struct RedisRemoteSnapshotClient {
    manager: ConnectionManager,
    key_prefix: String,
    ttl_seconds: Option<i64>,
}

impl std::fmt::Debug for RedisRemoteSnapshotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisRemoteSnapshotClient")
            .field("key_prefix", &self.key_prefix)
            .field("ttl_seconds", &self.ttl_seconds)
            .finish_non_exhaustive()
    }
}

impl RedisRemoteSnapshotClient {
    /// Creates a client from a shared [`RedisConnection`].
    ///
    /// * `key_prefix` — optional prefix; defaults to [`DEFAULT_KEY_PREFIX`].
    /// * `ttl_seconds` — optional TTL; non-positive values mean "no TTL".
    pub fn new(connection: &RedisConnection, key_prefix: &str, ttl_seconds: Option<i64>) -> Self {
        Self {
            manager: connection.manager().clone(),
            key_prefix: normalize_prefix(key_prefix),
            ttl_seconds: ttl_seconds.filter(|s| *s > 0),
        }
    }

    /// Composes the Redis key for a snapshot id.
    fn compose_key(&self, snapshot_id: &str) -> Result<String> {
        if snapshot_id.trim().is_empty() {
            return Err(Error::Other("snapshotId must not be blank".into()));
        }
        Ok(format!("{}{snapshot_id}.tar", self.key_prefix))
    }
}

#[async_trait]
impl RemoteSnapshotClient for RedisRemoteSnapshotClient {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        let key = self.compose_key(snapshot_id)?;
        let mut conn = self.manager.clone();
        conn.set::<_, _, ()>(&key, data)
            .await
            .map_err(|e| Error::Other(format!("Redis snapshot upload failed: {e}")))?;
        if let Some(ttl) = self.ttl_seconds {
            conn.expire::<_, ()>(&key, ttl)
                .await
                .map_err(|e| Error::Other(format!("Redis snapshot EXPIRE failed: {e}")))?;
        }
        Ok(())
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        let key = self.compose_key(snapshot_id)?;
        let mut conn = self.manager.clone();
        let data: Option<Vec<u8>> = conn
            .get(&key)
            .await
            .map_err(|e| Error::Other(format!("Redis snapshot download failed: {e}")))?;
        data.ok_or_else(|| Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Snapshot not found in Redis: {key}"),
        )))
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool> {
        let key = self.compose_key(snapshot_id)?;
        let mut conn = self.manager.clone();
        conn.exists::<_, bool>(&key)
            .await
            .map_err(|e| Error::Other(format!("Redis snapshot EXISTS failed: {e}")))
    }
}

/// Convenience [`SandboxSnapshotSpec`] for Redis-backed snapshot storage.
///
/// Wraps a [`RemoteSnapshotSpec`] around a [`RedisRemoteSnapshotClient`], so it composes
/// directly with the harness sandbox manager.
pub struct RedisSnapshotSpec {
    inner: RemoteSnapshotSpec,
}

impl std::fmt::Debug for RedisSnapshotSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisSnapshotSpec").finish_non_exhaustive()
    }
}

impl RedisSnapshotSpec {
    /// Creates a Redis snapshot spec.
    pub fn new(
        connection: &RedisConnection,
        key_prefix: &str,
        ttl_seconds: Option<i64>,
    ) -> Self {
        let client = Arc::new(RedisRemoteSnapshotClient::new(connection, key_prefix, ttl_seconds));
        Self { inner: RemoteSnapshotSpec::new(client) }
    }

    /// Creates a spec from an already-built [`RedisRemoteSnapshotClient`].
    pub fn from_client(client: Arc<RedisRemoteSnapshotClient>) -> Self {
        Self { inner: RemoteSnapshotSpec::new(client) }
    }
}

impl SandboxSnapshotSpec for RedisSnapshotSpec {
    fn build(
        &self,
        snapshot_id: &str,
    ) -> Result<Arc<dyn agentscope_harness::agent::sandbox::snapshot::SandboxSnapshot>> {
        self.inner.build(snapshot_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_prefix_rules() {
        assert_eq!(normalize_prefix(""), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_prefix("snap:"), "snap:");
        assert_eq!(normalize_prefix("snap"), "snap:");
    }
}
