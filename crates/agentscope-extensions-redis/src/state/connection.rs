//! Canonical [`RedisClientAdapter`] backed by the async `redis` crate.
//!
//! Mirrors the three Java adapters (Jedis / Lettuce / Redisson) collapsed into a single
//! connection-managed async client. Standalone / Sentinel / Cluster all work through
//! [`RedisConnection::connect`], which builds a [`redis::aio::ConnectionManager`] from a
//! URL (and tolerates a missing `redis://` scheme for parity with the Java examples that
//! pass `"localhost:6379"`).
//!
//! `find_keys_by_pattern` uses the **`SCAN` cursor protocol** (never `KEYS`), matching all
//! three Java adapters; the cursor loop lives in the trait impl below.

use std::sync::Arc;

use async_trait::async_trait;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use agentscope_core::error::{Error, Result};

use super::adapter::RedisClientAdapter;
use crate::raw_client::{ManagerRedisCommands, RedisCommands};

fn map_err(context: &str, e: redis::RedisError) -> Error {
    Error::Other(format!("Redis {context}: {e}"))
}

/// `COUNT` hint passed to `SCAN` — bounded work per round-trip, matching the intent of the
/// Java adapters (which rely on the driver's default batching).
const SCAN_COUNT: usize = 100;

/// A connection-managed async Redis client.
///
/// Cloning is cheap: the underlying [`ConnectionManager`] multiplexes a single
/// reconnecting connection, making this safe to share via `Arc` across the state store,
/// store, snapshot client and execution guard.
#[derive(Clone)]
pub struct RedisConnection {
    manager: ConnectionManager,
}

impl std::fmt::Debug for RedisConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConnection").finish_non_exhaustive()
    }
}

impl RedisConnection {
    /// Connects to Redis using a URL such as `redis://localhost:6379` or `localhost:6379`.
    ///
    /// A bare `host:port` (no scheme) is normalised to `redis://host:port`.
    pub async fn connect(url: &str) -> Result<Self> {
        let normalized = if url.contains("://") {
            url.to_string()
        } else {
            format!("redis://{url}")
        };
        let client = redis::Client::open(normalized)
            .map_err(|e| Error::Other(format!("Invalid Redis URL: {e}")))?;
        let manager = ConnectionManager::new(client)
            .await
            .map_err(|e| map_err("connect", e))?;
        Ok(Self { manager })
    }

    /// Wraps an already-built [`ConnectionManager`].
    pub fn from_manager(manager: ConnectionManager) -> Self {
        Self { manager }
    }

    /// Exposes the underlying connection manager for advanced/raw operations.
    pub fn manager(&self) -> &ConnectionManager {
        &self.manager
    }

    /// Creates a cheap clone sharing the same connection.
    pub fn clone_shared(&self) -> Self {
        self.clone()
    }

    /// Hands out this connection as a shared [`RedisClientAdapter`].
    ///
    /// Used by [`crate::RedisDistributedStore`] to wire the agent state store over the
    /// same connection.
    pub(crate) fn raw_adapter(&self) -> Arc<dyn RedisClientAdapter> {
        Arc::new(self.clone())
    }

    /// Hands out the crate-internal low-level command seam backed by the same
    /// connection, for the components that do not use [`RedisClientAdapter`]
    /// (`RedisRemoteSnapshotClient` / `RedisSandboxExecutionGuard`).
    pub(crate) fn raw_client(&self) -> Arc<dyn RedisCommands> {
        Arc::new(ManagerRedisCommands::new(self.manager.clone()))
    }
}

#[async_trait]
impl RedisClientAdapter for RedisConnection {
    async fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut conn = self.manager.clone();
        conn.set::<_, _, ()>(key, value)
            .await
            .map_err(|e| map_err("SET", e))
    }

    async fn get(&self, key: &str) -> Result<Option<String>> {
        let mut conn = self.manager.clone();
        conn.get::<_, Option<String>>(key)
            .await
            .map_err(|e| map_err("GET", e))
    }

    async fn right_push_list(&self, key: &str, value: &str) -> Result<()> {
        let mut conn = self.manager.clone();
        conn.rpush::<_, _, ()>(key, value)
            .await
            .map_err(|e| map_err("RPUSH", e))
    }

    async fn range_list(&self, key: &str, start: i64, end: i64) -> Result<Vec<String>> {
        let mut conn = self.manager.clone();
        conn.lrange::<_, Vec<String>>(key, start as isize, end as isize)
            .await
            .map_err(|e| map_err("LRANGE", e))
    }

    async fn get_list_length(&self, key: &str) -> Result<i64> {
        let mut conn = self.manager.clone();
        conn.llen::<_, i64>(key).await.map_err(|e| map_err("LLEN", e))
    }

    async fn delete_keys(&self, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut conn = self.manager.clone();
        conn.del::<_, ()>(keys)
            .await
            .map_err(|e| map_err("DEL", e))
    }

    async fn add_to_set(&self, key: &str, member: &str) -> Result<()> {
        let mut conn = self.manager.clone();
        conn.sadd::<_, _, ()>(key, member)
            .await
            .map_err(|e| map_err("SADD", e))
    }

    async fn get_set_members(&self, key: &str) -> Result<Vec<String>> {
        let mut conn = self.manager.clone();
        conn.smembers::<_, Vec<String>>(key)
            .await
            .map_err(|e| map_err("SMEMBERS", e))
    }

    async fn get_set_size(&self, key: &str) -> Result<i64> {
        let mut conn = self.manager.clone();
        conn.scard::<_, i64>(key)
            .await
            .map_err(|e| map_err("SCARD", e))
    }

    async fn key_exists(&self, key: &str) -> Result<bool> {
        let mut conn = self.manager.clone();
        conn.exists::<_, bool>(key)
            .await
            .map_err(|e| map_err("EXISTS", e))
    }

    /// Finds all keys matching a glob pattern using the `SCAN` cursor protocol.
    ///
    /// Implements the same contract as the Java adapters: loop `SCAN <cursor> MATCH
    /// <pattern> COUNT <n>` until the returned cursor is `0`, accumulating keys into a set
    /// (deduplicated) — never `KEYS`.
    async fn find_keys_by_pattern(&self, pattern: &str) -> Result<Vec<String>> {
        let mut conn = self.manager.clone();
        let mut cursor: u64 = 0;
        let mut found: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(SCAN_COUNT)
                .query_async(&mut conn)
                .await
                .map_err(|e| map_err("SCAN", e))?;
            found.extend(batch);
            if next == 0 {
                break;
            }
            cursor = next;
        }
        Ok(found.into_iter().collect())
    }

    async fn eval_script(
        &self,
        script: &str,
        keys: &[String],
        args: &[String],
    ) -> Result<Option<i64>> {
        let mut conn = self.manager.clone();
        let mut cmd = redis::cmd("EVAL");
        cmd.arg(script).arg(keys.len());
        for k in keys {
            cmd.arg(k);
        }
        for a in args {
            cmd.arg(a);
        }
        let reply: redis::Value = cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| map_err("EVAL", e))?;
        Ok(value_to_i64(&reply))
    }
}

/// Coerces a Redis integer reply into `i64`.
///
/// Redis returns bulk strings for numbers produced via `tostring(...)` (used by the
/// `PUT_SCRIPT`), so both `Int` and string `BulkString` replies are handled.
fn value_to_i64(value: &redis::Value) -> Option<i64> {
    use redis::Value;
    match value {
        Value::Int(i) => Some(*i),
        Value::BulkString(bytes) => std::str::from_utf8(bytes).ok()?.trim().parse().ok(),
        Value::SimpleString(s) => s.trim().parse().ok(),
        Value::Nil => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::Value;

    #[test]
    fn value_to_i64_handles_int_bulk_and_simple() {
        assert_eq!(value_to_i64(&Value::Int(7)), Some(7));
        assert_eq!(value_to_i64(&Value::BulkString(b"12".to_vec())), Some(12));
        assert_eq!(value_to_i64(&Value::SimpleString("3".to_string())), Some(3));
        assert_eq!(value_to_i64(&Value::Nil), None);
    }
}
