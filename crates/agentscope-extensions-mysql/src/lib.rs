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
//! # Real driver
//!
//! [`MysqlAsyncBackend`] is the real `mysql_async`-backed adapter implementing **all four**
//! seams over one pool. It lives behind the optional `mysql_async` feature so the default
//! build pulls no database driver; without the feature (or when the server is unreachable)
//! construction fails with [`MysqlError::DriverUnavailable`] instead of returning a store
//! that silently drops writes. Set `AGENTSCOPE_TEST_MYSQL_URL` and run
//! `cargo test -p agentscope-extensions-mysql --features mysql_async --test mysql_integration`
//! for the live-server tests (they skip themselves when the variable is unset).
//!
//! # JDBC façade (`feature = "jdbc-bridge"`)
//!
//! [`jdbc_bridge::MysqlJdbcStore`] is the one-call bridge to the crate-neutral
//! `agentscope_extensions_jdbc::JdbcDistributedStore`: it connects the jdbc crate's MySQL
//! executor, assembles the four JDBC components and (by default) uses the native `GET_LOCK`
//! guard — the same default the Java `MysqlDialect` has. Without the feature every constructor
//! returns [`MysqlError::DriverUnavailable`] instead of a store that cannot write.
//!
//! Both conveniences fail closed: [`MysqlDistributedStore::connect`] needs the `mysql_async`
//! feature, [`jdbc_bridge::MysqlJdbcStore::connect`] needs `jdbc-bridge`.
//!
//! # CAS / version semantics
//!
//! `StoreItem.version` and `BaseStore::put_if_version` follow the harness definition
//! exactly: `version` starts at `1` and increments on each successful write; a CAS with
//! `expected_version == 0` means create-if-absent. See [`StoreDialect`] for the SQL shapes.

pub mod distributed_store;
pub mod driver;
pub mod error;
pub mod guard;
pub mod identifier;
pub mod jdbc_bridge;
pub mod snapshot;
pub mod state;
pub mod store;

pub use distributed_store::{MysqlDistributedStore, MysqlDistributedStoreBuilder};
pub use driver::{driver_available, MysqlAsyncBackend, MysqlAsyncConfig};
pub use error::{MysqlError, MysqlResult};
pub use guard::{
    java_string_hash_code, truncate_lock_name, LockBackend, MysqlLockLease,
    MysqlSandboxExecutionGuard, MysqlSandboxExecutionGuardBuilder, DEFAULT_KEY_PREFIX,
    DEFAULT_LOCK_TIMEOUT, MAX_LOCK_NAME_LENGTH,
};
pub use identifier::{validate_identifier, MAX_IDENTIFIER_LENGTH};
pub use jdbc_bridge::{jdbc_bridge_available, MysqlJdbcStore, MysqlJdbcStoreBuilder};
pub use snapshot::{
    snapshot_table_ddl, MysqlRemoteSnapshotClient, MysqlSnapshotSpec, SnapshotBackend,
    DEFAULT_SNAPSHOT_TABLE,
};
pub use state::{
    add_version_column_sql, create_database_sql, create_sessions_table_sql, validate_state_key,
    CasOutcome, MysqlAgentStateStore, StateBackend, DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME,
    HASH_KEY_SUFFIX, MAX_STATE_IDENTIFIER_LENGTH, SINGLE_STATE_INDEX,
};
pub use store::{
    value_from_json, H2StoreDialect, MysqlStore, MysqlStoreBuilder, MysqlStoreDialect,
    PostgresStoreDialect, SqlBackend, SqliteStoreDialect, StoreDialect, DEFAULT_STORE_TABLE,
    NS_SEPARATOR,
};
