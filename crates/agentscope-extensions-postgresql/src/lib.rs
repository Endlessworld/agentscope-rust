//! agentscope-extensions-postgresql — PostgreSQL-backed distributed persistence for
//! AgentScope Rust, mirroring the Java `agentscope-extensions-postgresql` module.
//!
//! See `docs/extensions/POSTGRESQL__GAP_TRACKER.md` for the migration status.
//!
//! # Provided components (the shared storage-module contract)
//! * [`PostgresDistributedStore`] — composer/factory (mirrors Java `PostgresDistributedStore`)
//! * [`PostgresAgentStateStore`] — async [`AgentStateStore`](agentscope_core::state::AgentStateStore)
//! * [`PostgresStore`] — sync [`BaseStore`](agentscope_harness::filesystem::BaseStore)
//! * [`PostgresSnapshotSpec`] / [`PostgresRemoteSnapshotClient`] — sandbox snapshots
//! * [`PostgresSandboxExecutionGuard`] — advisory-lock distributed guard
//!
//! # Architecture
//! All SQL flows through the synchronous [`PostgresClient`] seam ([`client`]). The async
//! traits are bridged onto it via `tokio::task::spawn_blocking` ([`blocking`]).
//! [`MemoryClient`] is an in-memory SQL simulator used by this crate's tests, so the
//! crate compiles and tests with **no live database**. The real driver adapter
//! (`PgSimpleClient`, feature `postgres-driver`) wraps `tokio-postgres`.
//!
//! Two capabilities are **session-scoped** and therefore explicit on the seam:
//! * [`PostgresClient::open_lock_session`] yields a dedicated connection per advisory-lock
//!   lease (PG advisory locks belong to a session; sharing one would silently break mutual
//!   exclusion — see [`guard`]).
//! * [`PostgresClient::execute_in_transaction`] runs a multi-statement write atomically on a
//!   dedicated connection (Java `executeInWriteTransaction`).
//!
//! Both default to an explicit [`PostgresError::DriverUnavailable`] instead of degrading.

pub mod blocking;
pub mod client;
pub mod distributed;
pub mod error;
pub mod guard;
pub mod memory;
pub mod snapshot;
pub mod sql;
pub mod state;
pub mod store;

mod murmur3;

#[cfg(feature = "postgres-driver")]
pub mod driver;

pub use client::{
    Cell, PostgresClient, PostgresLockSession, QueryOutcome, Row, SqlStatement, SqlValue,
};
pub use distributed::PostgresDistributedStore;
pub use error::{PgResult, PostgresError};
pub use guard::{
    PostgresSandboxExecutionGuard, PostgresSandboxExecutionGuardBuilder, PostgresSandboxLease,
    DEFAULT_KEY_PREFIX, DEFAULT_LOCK_TIMEOUT_SECONDS,
};
pub use memory::MemoryClient;
pub use snapshot::{
    initialize_snapshot_table, postgres_snapshot_spec, postgres_snapshot_spec_with_table,
    PostgresRemoteSnapshotClient, PostgresSnapshotSpec, DEFAULT_TABLE as DEFAULT_SNAPSHOT_TABLE,
};
pub use state::{
    PostgresAgentStateStore, PostgresAgentStateStoreBuilder, ANON_USER, DEFAULT_SCHEMA_NAME,
    DEFAULT_TABLE_NAME as DEFAULT_SESSIONS_TABLE, HASH_KEY_SUFFIX, SINGLE_STATE_INDEX, UNVERSIONED,
};
pub use store::{PostgresStore, PostgresStoreBuilder, DEFAULT_TABLE_NAME as DEFAULT_STORE_TABLE};
