//! agentscope-extensions-mysql - MySQL/JDBC-backed distributed-store extensions.
//!
//! Ports `agentscope-extensions-mysql` (Java) onto the existing core/harness traits, per
//! `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1 (reuse harness abstractions, do not
//! rebuild them).
//!
//! # Components
//!
//! | Rust type | Java counterpart | Harness/core trait re-used |
//! |---|---|---|
//! | [`MysqlDistributedStore`] | `MysqlDistributedStore` | assembles the four below |
//! | [`MysqlAgentStateStore`] | `MysqlAgentStateStore` | `agentscope_core::state::AgentStateStore` |
//! | [`MysqlStore`] | `JdbcStore` | `agentscope_harness::filesystem::remote::store::BaseStore` |
//! | [`MysqlSnapshotSpec`] | `JdbcSnapshotSpec` | `agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec` |
//! | [`MysqlSandboxExecutionGuard`] | `JdbcSandboxExecutionGuard` | (harness guard is a concrete struct — see module docs) |
//!
//! # Backends
//!
//! Each component is written against a narrow backend trait ([`SqlBackend`],
//! [`StateBackend`], [`SnapshotBackend`], [`LockBackend`]) so the crate compiles and is
//! fully testable without a live MySQL server. Driver adapters implement these traits;
//! in-crate in-memory backends back the unit tests.
//!
//! # CAS / version semantics
//!
//! `StoreItem.version` and `BaseStore::put_if_version` follow the harness definition
//! exactly: `version` starts at `1` and increments on each successful write; a CAS with
//! `expected_version == 0` means create-if-absent. See [`StoreDialect`] for the SQL shapes.

pub mod distributed_store;
pub mod error;
pub mod guard;
pub mod identifier;
pub mod snapshot;
pub mod state;
pub mod store;

pub use distributed_store::{MysqlDistributedStore, MysqlDistributedStoreBuilder};
pub use error::{MysqlError, MysqlResult};
pub use guard::{
    LockBackend, MysqlLockLease, MysqlSandboxExecutionGuard, MysqlSandboxExecutionGuardBuilder,
    DEFAULT_KEY_PREFIX,
};
pub use identifier::{validate_identifier, MAX_IDENTIFIER_LENGTH};
pub use snapshot::{
    MysqlRemoteSnapshotClient, MysqlSnapshotSpec, SnapshotBackend, DEFAULT_SNAPSHOT_TABLE,
};
pub use state::{
    CasOutcome, MysqlAgentStateStore, StateBackend, DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME,
    HASH_KEY_SUFFIX, SINGLE_STATE_INDEX,
};
pub use store::{
    value_from_json, H2StoreDialect, MysqlStore, MysqlStoreBuilder, MysqlStoreDialect,
    PostgresStoreDialect, SqlBackend, SqliteStoreDialect, StoreDialect, DEFAULT_STORE_TABLE,
    NS_SEPARATOR,
};
