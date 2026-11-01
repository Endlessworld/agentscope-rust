//! Optional real MySQL driver adapter (`feature = "mysql_async"`).
//!
//! The crate's four backend seams ([`crate::StateBackend`], [`crate::SqlBackend`],
//! [`crate::SnapshotBackend`], [`crate::LockBackend`]) are deliberately synchronous and
//! driver-agnostic; every component is written against them so the crate builds and its
//! unit tests run with zero network access. This module supplies the missing piece: one
//! concrete adapter backed by [`mysql_async`], implementing **all four** seams over a single
//! connection pool.
//!
//! ```no_run
//! # #[cfg(feature = "mysql_async")]
//! # fn main() -> Result<(), agentscope_extensions_mysql::MysqlError> {
//! use std::sync::Arc;
//! use agentscope_extensions_mysql::{MysqlAgentStateStore, MysqlAsyncBackend};
//!
//! let backend = MysqlAsyncBackend::connect("mysql://root:secret@127.0.0.1:3306/mysql")?;
//! let state = MysqlAgentStateStore::new(backend)?;
//! # let _ = state;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "mysql_async"))]
//! # fn main() {}
//! ```
//!
//! # Failure semantics
//!
//! [`MysqlAsyncBackend::connect`] pings the server eagerly. Without the feature — or when
//! the URL is invalid or the server unreachable — construction fails with
//! [`crate::MysqlError::DriverUnavailable`]. It never hands back a backend that would
//! silently swallow writes.

use crate::snapshot::DEFAULT_SNAPSHOT_TABLE;
use crate::state::{DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME};
use crate::store::DEFAULT_STORE_TABLE;

#[cfg(feature = "mysql_async")]
mod real;
#[cfg(feature = "mysql_async")]
pub use real::MysqlAsyncBackend;

#[cfg(not(feature = "mysql_async"))]
mod stub;
#[cfg(not(feature = "mysql_async"))]
pub use stub::MysqlAsyncBackend;

/// Table names the driver-backed backend reads and writes.
///
/// The components ([`crate::MysqlAgentStateStore`], [`crate::MysqlStore`],
/// [`crate::MysqlSnapshotSpec`]) only learn their table name at construction, so both sides
/// must agree. [`crate::MysqlDistributedStore::connect_with_config`] is the supported way to
/// keep them in sync: it builds every component from this config, so the names below are the
/// ones the statements actually use. The defaults match each component's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlAsyncConfig {
    /// Database holding the session-state table (also created by schema bootstrap).
    pub database: String,
    /// Session-state table ([`crate::DEFAULT_TABLE_NAME`]).
    pub state_table: String,
    /// Namespace KV table ([`crate::DEFAULT_STORE_TABLE`]).
    pub store_table: String,
    /// Snapshot BLOB table ([`crate::DEFAULT_SNAPSHOT_TABLE`]).
    pub snapshot_table: String,
}

impl Default for MysqlAsyncConfig {
    fn default() -> Self {
        Self {
            database: DEFAULT_DATABASE_NAME.to_string(),
            state_table: DEFAULT_TABLE_NAME.to_string(),
            store_table: DEFAULT_STORE_TABLE.to_string(),
            snapshot_table: DEFAULT_SNAPSHOT_TABLE.to_string(),
        }
    }
}

impl MysqlAsyncConfig {
    /// Builds an explicit configuration from four table names.
    pub fn new(
        database: impl Into<String>,
        state_table: impl Into<String>,
        store_table: impl Into<String>,
        snapshot_table: impl Into<String>,
    ) -> Self {
        Self {
            database: database.into(),
            state_table: state_table.into(),
            store_table: store_table.into(),
            snapshot_table: snapshot_table.into(),
        }
    }
}

/// Whether the crate was compiled with the real driver adapter.
pub fn driver_available() -> bool {
    cfg!(feature = "mysql_async")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_the_component_defaults() {
        let config = MysqlAsyncConfig::default();
        assert_eq!(config.database, "agentscope");
        assert_eq!(config.state_table, "agentscope_sessions");
        assert_eq!(config.store_table, "agentscope_store");
        assert_eq!(config.snapshot_table, "agentscope_snapshots");
    }

    #[test]
    fn feature_flag_matches_the_compile_target() {
        assert_eq!(driver_available(), cfg!(feature = "mysql_async"));
    }

    #[test]
    #[cfg(not(feature = "mysql_async"))]
    fn connect_without_the_feature_reports_driver_unavailable() {
        let err = MysqlAsyncBackend::connect("mysql://root@127.0.0.1:3306/mysql")
            .expect_err("the adapter must not pretend to connect without its driver");
        assert!(
            matches!(err, crate::MysqlError::DriverUnavailable(_)),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains("mysql_async"));
    }
}
