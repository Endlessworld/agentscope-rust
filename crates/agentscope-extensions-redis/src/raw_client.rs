//! Low-level Redis command seam shared by the components that do **not** go through
//! [`crate::state::adapter::RedisClientAdapter`].
//!
//! ## Why this exists
//!
//! `RedisState`'s sibling components — [`crate::store::RedisStore`],
//! [`crate::snapshot::RedisRemoteSnapshotClient`] and
//! [`crate::sandbox::RedisSandboxExecutionGuard`] — use primitives that the state-store
//! adapter trait does not model (`HGETALL`, `ZRANGEBYLEX`, `SET NX PX`, `EXPIRE`, raw
//! binary payloads, Lua scripts returning non-integer replies).
//!
//! Historically each of those types held a `redis::ConnectionManager` directly. That made
//! them impossible to exercise offline, because the `redis` crate's async command futures
//! only dispose over a real socket / runtime IO driver.
//!
//! [`RedisCommands`] is the minimal async command surface those components need. It is
//! implemented by the production [`ManagerRedisCommands`] (a thin wrapper over a live
//! [`redis::aio::ConnectionManager`]) and, in tests, by an in-memory double, so every
//! component can be covered offline without a server.
//!
//! The trait is crate-internal (`pub(crate)`) — it is an implementation seam, not part of
//! the public API. The production wrapper is exposed so that
//! [`crate::RedisConnection::raw_client`] can hand out an `Arc<dyn RedisCommands>`.

use agentscope_core::error::{Error, Result};
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Value};

/// Async command surface shared by store / snapshot / sandbox-guard.
///
/// Semantics match the corresponding Redis commands (`HSET`/`HGETALL`/`DEL`/`SET`/
/// `GET`/`EXISTS`/`EXPIRE`/`EVAL`).
#[async_trait::async_trait]
#[cfg_attr(test, allow(dead_code))]
pub trait RedisCommands: Send + Sync + std::fmt::Debug {
    /// `SET key value` — string/binary full replacement.
    async fn set_bytes(&self, key: &str, value: &[u8]) -> Result<()>;

    /// `GET key` — `None` when the key is missing.
    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// `EXISTS key`.
    async fn exists(&self, key: &str) -> Result<bool>;

    /// `EXPIRE key seconds`.
    async fn expire(&self, key: &str, seconds: i64) -> Result<()>;

    /// `SET key value NX PX <ttlMs>` — `true` when the lease was acquired.
    async fn set_nx_px(&self, key: &str, value: &str, ttl_ms: u64) -> Result<bool>;

    /// `EVAL script 1 key argv1` — used for the token-checked lease release.
    async fn eval_one_key(&self, script: &str, key: &str, arg: &str) -> Result<Value>;

    /// `DEL key` — returns the number of keys removed.
    async fn del(&self, key: &str) -> Result<i64>;
}

/// Production [`RedisCommands`] backed by a live async [`ConnectionManager`].
#[derive(Clone)]
pub struct ManagerRedisCommands {
    manager: ConnectionManager,
}

impl std::fmt::Debug for ManagerRedisCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerRedisCommands")
            .finish_non_exhaustive()
    }
}

impl ManagerRedisCommands {
    /// Wraps a connection manager.
    pub fn new(manager: ConnectionManager) -> Self {
        Self { manager }
    }
}

fn map_err(context: &str, e: redis::RedisError) -> Error {
    Error::Other(format!("Redis {context}: {e}"))
}

#[async_trait::async_trait]
impl RedisCommands for ManagerRedisCommands {
    async fn set_bytes(&self, key: &str, value: &[u8]) -> Result<()> {
        let mut conn = self.manager.clone();
        conn.set::<_, _, ()>(key, value)
            .await
            .map_err(|e| map_err("SET", e))
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut conn = self.manager.clone();
        conn.get::<_, Option<Vec<u8>>>(key)
            .await
            .map_err(|e| map_err("GET", e))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let mut conn = self.manager.clone();
        conn.exists::<_, bool>(key)
            .await
            .map_err(|e| map_err("EXISTS", e))
    }

    async fn expire(&self, key: &str, seconds: i64) -> Result<()> {
        let mut conn = self.manager.clone();
        conn.expire::<_, ()>(key, seconds)
            .await
            .map_err(|e| map_err("EXPIRE", e))
    }

    async fn set_nx_px(&self, key: &str, value: &str, ttl_ms: u64) -> Result<bool> {
        let reply: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("NX")
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut self.manager.clone())
            .await
            .map_err(|e| map_err("SET NX PX", e))?;
        Ok(reply.as_deref() == Some("OK"))
    }

    async fn eval_one_key(&self, script: &str, key: &str, arg: &str) -> Result<Value> {
        let mut conn = self.manager.clone();
        redis::cmd("EVAL")
            .arg(script)
            .arg(1)
            .arg(key)
            .arg(arg)
            .query_async(&mut conn)
            .await
            .map_err(|e| map_err("EVAL", e))
    }

    async fn del(&self, key: &str) -> Result<i64> {
        let mut conn = self.manager.clone();
        conn.del::<_, i64>(key).await.map_err(|e| map_err("DEL", e))
    }
}
