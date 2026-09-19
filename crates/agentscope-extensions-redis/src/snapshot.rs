//! Redis-backed sandbox snapshot storage, mirroring
//! `io.agentscope.extensions.redis.snapshot.*`.
//!
//! * [`RedisRemoteSnapshotClient`] implements the harness [`RemoteSnapshotClient`] trait
//!   using Redis binary values (archives stored as `{prefix}{snapshotId}.tar`).
//! * [`RedisSnapshotSpec`] is a convenience [`SandboxSnapshotSpec`] wrapping the client.
//!
//! Both talk to the crate-internal [`RedisCommands`] seam, so the real command sequence
//! can be exercised offline with an in-memory backend.

use std::sync::Arc;

use async_trait::async_trait;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshotSpec,
};

use crate::raw_client::RedisCommands;
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
    commands: Arc<dyn RedisCommands>,
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
            commands: connection.raw_client(),
            key_prefix: normalize_prefix(key_prefix),
            ttl_seconds: ttl_seconds.filter(|s| *s > 0),
        }
    }

    /// Builds a client over an arbitrary command backend (production connection, or in
    /// tests an in-memory double).
    #[cfg(test)]
    pub(crate) fn from_commands(
        commands: Arc<dyn RedisCommands>,
        key_prefix: &str,
        ttl_seconds: Option<i64>,
    ) -> Self {
        Self {
            commands,
            key_prefix: normalize_prefix(key_prefix),
            ttl_seconds: ttl_seconds.filter(|s| *s > 0),
        }
    }

    /// The configured, normalized key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// The configured TTL in seconds (`None` when none applies).
    pub fn ttl_seconds(&self) -> Option<i64> {
        self.ttl_seconds
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
        self.commands
            .set_bytes(&key, data)
            .await
            .map_err(|e| Error::Other(format!("Redis snapshot upload failed: {e}")))?;
        if let Some(ttl) = self.ttl_seconds {
            self.commands
                .expire(&key, ttl)
                .await
                .map_err(|e| Error::Other(format!("Redis snapshot EXPIRE failed: {e}")))?;
        }
        Ok(())
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        let key = self.compose_key(snapshot_id)?;
        let data = self
            .commands
            .get_bytes(&key)
            .await
            .map_err(|e| Error::Other(format!("Redis snapshot download failed: {e}")))?;
        data.ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Snapshot not found in Redis: {key}"),
            ))
        })
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool> {
        let key = self.compose_key(snapshot_id)?;
        self.commands
            .exists(&key)
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
    pub fn new(connection: &RedisConnection, key_prefix: &str, ttl_seconds: Option<i64>) -> Self {
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
    use crate::test_support::MemoryRedisCommands;

    fn client(
        ttl_seconds: Option<i64>,
        prefix: &str,
    ) -> (RedisRemoteSnapshotClient, Arc<MemoryRedisCommands>) {
        let commands = Arc::new(MemoryRedisCommands::new());
        let client =
            RedisRemoteSnapshotClient::from_commands(commands.clone(), prefix, ttl_seconds);
        (client, commands)
    }

    #[test]
    fn normalize_prefix_rules() {
        assert_eq!(normalize_prefix(""), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_prefix("snap:"), "snap:");
        assert_eq!(normalize_prefix("snap"), "snap:");
    }

    #[test]
    fn non_positive_ttl_is_dropped() {
        let (c, _) = client(Some(0), "p:");
        assert_eq!(c.ttl_seconds(), None);
        let (c, _) = client(Some(-5), "p:");
        assert_eq!(c.ttl_seconds(), None);
        let (c, _) = client(Some(30), "p:");
        assert_eq!(c.ttl_seconds(), Some(30));
    }

    #[tokio::test]
    async fn upload_then_download_round_trip() {
        let (client, commands) = client(None, "snap:");
        let payload = b"archive-bytes\x00\xff".to_vec();
        client.upload("s1", &payload).await.unwrap();

        assert_eq!(client.download("s1").await.unwrap(), payload);
        assert!(client.exists("s1").await.unwrap());
        assert_eq!(commands.stored_bytes("snap:s1.tar").as_deref(), Some(payload.as_slice()));
        assert_eq!(commands.expiration_of("snap:s1.tar"), None, "no TTL configured");
    }

    #[tokio::test]
    async fn upload_applies_ttl_when_configured() {
        let (client, commands) = client(Some(120), "snap:");
        client.upload("s2", b"x").await.unwrap();
        assert_eq!(commands.expiration_of("snap:s2.tar"), Some(120));
        assert_eq!(commands.inspect(|d| d.expire_calls), 1);
    }

    #[tokio::test]
    async fn download_missing_snapshot_reports_not_found() {
        let (client, _commands) = client(None, "snap:");
        let err = client.download("absent").await.unwrap_err();
        match err {
            Error::Io(io) => {
                assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
                assert!(io.to_string().contains("snap:absent.tar"));
            }
            other => panic!("expected Error::Io(NotFound), got {other:?}"),
        }
        assert!(!client.exists("absent").await.unwrap());
    }

    #[tokio::test]
    async fn blank_snapshot_id_is_rejected_before_io() {
        let (client, commands) = client(None, "snap:");
        assert!(client.upload("  ", b"x").await.is_err());
        assert!(client.download("").await.is_err());
        assert!(client.exists("  ").await.is_err());
        assert_eq!(commands.inspect(|d| d.set_bytes_calls), 0);
    }

    #[tokio::test]
    async fn backend_failures_are_wrapped_with_context() {
        let (c1, commands) = client(Some(5), "snap:");
        commands.configure(|d| d.fail_set_bytes = true);
        let err = c1.upload("s", b"x").await.unwrap_err();
        assert!(format!("{err}").contains("Redis snapshot upload failed"), "{err}");

        let (c2, commands) = client(Some(5), "snap:");
        commands.configure(|d| {
            d.strings.insert("snap:s.tar".into(), b"x".to_vec());
            d.fail_expire = true;
        });
        let err = c2.upload("s", b"x").await.unwrap_err();
        assert!(format!("{err}").contains("Redis snapshot EXPIRE failed"), "{err}");
    }

    #[tokio::test]
    async fn spec_builds_snapshots_and_rejects_unsafe_ids() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let client = Arc::new(RedisRemoteSnapshotClient::from_commands(
            commands,
            "snap:",
            None,
        ));
        let spec = RedisSnapshotSpec::from_client(client);

        let snapshot = spec.build("session-1").expect("valid id");
        assert_eq!(snapshot.id(), "session-1");
        assert_eq!(snapshot.snapshot_type(), "remote");
        assert!(snapshot.is_persistence_enabled());

        // id validation is inherited from the harness `RemoteSnapshotSpec`
        assert!(spec.build("  ").is_err(), "blank id rejected");
        assert!(spec.build("a/b").is_err(), "path separator rejected");
        assert!(spec.build("..").is_err(), "traversal rejected");
    }

    #[tokio::test]
    async fn spec_snapshot_round_trips_through_the_client() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let client = Arc::new(RedisRemoteSnapshotClient::from_commands(
            commands.clone(),
            "snap:",
            None,
        ));
        let spec = RedisSnapshotSpec::from_client(client);
        let snapshot = spec.build("s9").unwrap();
        snapshot.persist(b"payload").await.unwrap();
        assert_eq!(commands.stored_bytes("snap:s9.tar").as_deref(), Some(b"payload".as_ref()));
        assert!(snapshot.is_restorable().await.unwrap());
        assert_eq!(snapshot.restore().await.unwrap(), b"payload");
    }
}
