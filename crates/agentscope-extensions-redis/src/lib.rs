//! # agentscope-extensions-redis
//!
//! Redis-backed distributed storage implementations for AgentScope Rust, mirroring the
//! Java module `io.agentscope.extensions.redis`.
//!
//! This crate **implements existing harness/core traits** rather than inventing parallel
//! abstractions (see `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1):
//!
//! | Component | Implemented trait / harness type |
//! |---|---|
//! | [`RedisAgentStateStore`] | `agentscope_core::state::AgentStateStore` |
//! | [`RedisStore`] | `agentscope_harness::filesystem::remote::store::BaseStore` |
//! | [`RedisSnapshotSpec`] / [`RedisRemoteSnapshotClient`] | `SandboxSnapshotSpec` / `RemoteSnapshotClient` |
//! | [`RedisSandboxExecutionGuard`] | distributed equivalent of the harness guard |
//! | [`RedisDistributedStore`] | one-stop combiner (projects onto harness `DistributedStore`) |
//!
//! CAS / version semantics follow the harness definitions: `BaseStore::put_if_version`
//! treats `expected_version == 0` as "create only if absent", and state-store versions are
//! monotonic counters bumped atomically via Lua.
//!
//! ## Quick start
//!
//! ```no_run
//! # async fn demo() -> agentscope_core::error::Result<()> {
//! use agentscope_extensions_redis::RedisDistributedStore;
//!
//! // One connection wires state store + filesystem KV + snapshots + guard.
//! let store = RedisDistributedStore::connect("redis://localhost:6379").await?;
//! let agent_state = store.agent_state_store()?;
//! let _ = agent_state;
//! # Ok(()) }
//! ```

pub mod sandbox;
pub mod snapshot;
pub mod state;
pub mod store;

pub(crate) mod raw_client;
#[cfg(test)]
pub(crate) mod test_support;

mod distributed_store;

pub use distributed_store::RedisDistributedStore;
pub use raw_client::ManagerRedisCommands;
pub use sandbox::{
    RedisSandboxExecutionGuard, RedisSandboxExecutionGuardBuilder, RedisSandboxLease,
};
pub use snapshot::{RedisRemoteSnapshotClient, RedisSnapshotSpec};
pub use state::adapter::RedisClientAdapter;
pub use state::connection::RedisConnection;
pub use state::{KeyLayoutVersion, RedisAgentStateStore, RedisAgentStateStoreBuilder, VersionedState};
pub use store::RedisStore;
