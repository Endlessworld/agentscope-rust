//! Low-level Redis command seam shared by the components that do **not** go through
//! [`crate::state::adapter::RedisClientAdapter`].
//!
//! ## Why this exists
//!
//! `RedisState`'s sibling components — [`crate::store::RedisStore`],
//! [`crate::snapshot::RedisRemoteSnapshotClient`] and
//! [`crate::sandbox::RedisSandboxExecutionGuard`] — use primitives that the state-store
//! adapter trait does not model (`HGETALL`, `ZRANGEBYLEX`, `SET NX PX`, `SET ... EX`, raw
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

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use redis::aio::ConnectionManager;
use redis::Value;

use crate::state::connection::ClusterRouter;

/// Async command surface shared by store / snapshot / sandbox-guard.
///
/// Semantics match the corresponding Redis commands (`HSET`/`HGETALL`/`DEL`/`SET`/
/// `GET`/`EXISTS`/`SET ... EX`/`EVAL`).
#[async_trait::async_trait]
#[cfg_attr(test, allow(dead_code))]
pub trait RedisCommands: Send + Sync + std::fmt::Debug {
    /// `SET key value` — string/binary full replacement.
    async fn set_bytes(&self, key: &str, value: &[u8]) -> Result<()>;

    /// `SET key value EX <seconds>` — one atomic write that carries its TTL.
    ///
    /// Used by the snapshot upload so a value can never land without the expiry it was
    /// configured with (REDIS-08: `SET` followed by `EXPIRE` was two round-trips, and the
    /// key lived forever when only the second one failed).
    async fn set_bytes_with_ttl(&self, key: &str, value: &[u8], seconds: i64) -> Result<()>;

    /// `GET key` — `None` when the key is missing.
    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// `EXISTS key`.
    async fn exists(&self, key: &str) -> Result<bool>;

    /// `SET key value NX PX <ttlMs>` — `true` when the lease was acquired.
    async fn set_nx_px(&self, key: &str, value: &str, ttl_ms: u64) -> Result<bool>;

    /// `EVAL script 1 key argv1` — used for the token-checked lease release.
    async fn eval_one_key(&self, script: &str, key: &str, arg: &str) -> Result<Value>;

    /// `DEL key` — returns the number of keys removed.
    async fn del(&self, key: &str) -> Result<i64>;
}

/// Production [`RedisCommands`] backed by a live async [`ConnectionManager`].
///
/// When the underlying connection was built by
/// [`RedisConnection::connect_cluster`](crate::state::connection::RedisConnection::connect_cluster)
/// the wrapper also carries its [`ClusterRouter`], so every key-scoped command below
/// (snapshot upload / download / EXISTS, the sandbox `SET NX PX`, the lease-release `EVAL`)
/// starts on the primary that owns the key and follows its `MOVED` / `ASK` redirects
/// instead of failing with `MOVED` on a non-seed slot (REDIS-01).
#[derive(Clone)]
pub struct ManagerRedisCommands {
    manager: ConnectionManager,
    router: Option<Arc<ClusterRouter>>,
}

impl std::fmt::Debug for ManagerRedisCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerRedisCommands")
            .finish_non_exhaustive()
    }
}

impl ManagerRedisCommands {
    /// Wraps a connection manager (single node: no slot routing).
    pub fn new(manager: ConnectionManager) -> Self {
        Self {
            manager,
            router: None,
        }
    }

    /// Wraps a connection manager together with the cluster router of its connection.
    pub(crate) fn with_router(
        manager: ConnectionManager,
        router: Option<Arc<ClusterRouter>>,
    ) -> Self {
        Self { manager, router }
    }

    /// Runs `cmd` for `key`: through the cluster router when there is one, else straight on
    /// the single connection.
    async fn run_keyed(&self, context: &str, key: &str, cmd: &redis::Cmd) -> Result<Value> {
        match &self.router {
            Some(router) => router.run(key, cmd).await,
            None => {
                let mut conn = self.manager.clone();
                cmd.query_async(&mut conn)
                    .await
                    .map_err(|e| map_err(context, e))
            }
        }
    }
}

fn map_err(context: &str, e: redis::RedisError) -> Error {
    Error::Other(format!("Redis {context}: {e}"))
}

/// Decodes a raw reply into `T`, reporting a decode failure as a Redis error.
fn from_value<T: redis::FromRedisValue>(value: Value) -> Result<T> {
    T::from_redis_value(value).map_err(|e| Error::Other(format!("Redis reply decode: {e}")))
}

#[async_trait::async_trait]
impl RedisCommands for ManagerRedisCommands {
    async fn set_bytes(&self, key: &str, value: &[u8]) -> Result<()> {
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value);
        self.run_keyed("SET", key, &cmd).await?;
        Ok(())
    }

    async fn set_bytes_with_ttl(&self, key: &str, value: &[u8], seconds: i64) -> Result<()> {
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value).arg("EX").arg(seconds);
        self.run_keyed("SET EX", key, &cmd).await?;
        Ok(())
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut cmd = redis::cmd("GET");
        cmd.arg(key);
        from_value(self.run_keyed("GET", key, &cmd).await?)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let mut cmd = redis::cmd("EXISTS");
        cmd.arg(key);
        from_value(self.run_keyed("EXISTS", key, &cmd).await?)
    }

    async fn set_nx_px(&self, key: &str, value: &str, ttl_ms: u64) -> Result<bool> {
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value).arg("NX").arg("PX").arg(ttl_ms);
        let reply: Option<String> = from_value(self.run_keyed("SET NX PX", key, &cmd).await?)?;
        Ok(reply.as_deref() == Some("OK"))
    }

    async fn eval_one_key(&self, script: &str, key: &str, arg: &str) -> Result<Value> {
        let mut cmd = redis::cmd("EVAL");
        cmd.arg(script).arg(1).arg(key).arg(arg);
        self.run_keyed("EVAL", key, &cmd).await
    }

    async fn del(&self, key: &str) -> Result<i64> {
        let mut cmd = redis::cmd("DEL");
        cmd.arg(key);
        from_value(self.run_keyed("DEL", key, &cmd).await?)
    }
}
