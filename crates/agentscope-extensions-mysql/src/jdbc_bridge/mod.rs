//! JDBC façade over MySQL (`feature = "jdbc-bridge"`).
//!
//! The crate already ships two MySQL backends:
//!
//! * [`crate::MysqlDistributedStore`] — the MySQL-native bundle over the four synchronous
//!   seams ([`crate::StateBackend`], [`crate::SqlBackend`], [`crate::SnapshotBackend`],
//!   [`crate::LockBackend`]);
//! * `agentscope_extensions_jdbc::JdbcDistributedStore` — the **crate-neutral** façade the rest
//!   of the workspace (and Java's `MysqlIntegrationTest`) uses, assembled from a
//!   `JdbcExecutor` + `AbstractJdbcDialect` pair.
//!
//! This module closes the second path: [`MysqlJdbcStore`] is a one-call wrapper around
//! `JdbcDistributedStore::create_with_dialect_and_schema` with the MySQL executor
//! (`agentscope_extensions_jdbc::connect_mysql`) and `MysqlDialect`.
//!
//! # Why the dependency points this way
//!
//! `agentscope-extensions-jdbc` owns the dialect/component layer and must stay driver-agnostic
//! — it cannot depend on this crate. The bridge therefore lives **here**, behind the optional
//! `jdbc-bridge` feature (which also turns on jdbc's `mysql-executor`); the default build of
//! both crates is unchanged and pulls in no driver.
//!
//! # Failure semantics
//!
//! [`MysqlJdbcStoreBuilder::build`] connects eagerly and never guesses: without the feature it
//! returns [`crate::MysqlError::DriverUnavailable`] naming the missing feature, and with the
//! feature an empty/invalid URL or an unreachable server also fails there (wrapped from
//! `JdbcError::Configuration` / `JdbcError::Database`). No half-built store can be handed out.
//!
//! ```no_run
//! # #[cfg(feature = "jdbc-bridge")]
//! # fn main() -> Result<(), agentscope_extensions_mysql::MysqlError> {
//! use agentscope_extensions_mysql::jdbc_bridge::MysqlJdbcStore;
//!
//! let store = MysqlJdbcStore::connect("mysql://root:secret@127.0.0.1:3306/agentscope")?;
//! let harness = store.to_harness_distributed_store();
//! # let _ = harness;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "jdbc-bridge"))]
//! # fn main() {}
//! ```

#[cfg(feature = "jdbc-bridge")]
mod real;
#[cfg(feature = "jdbc-bridge")]
pub use real::{MysqlJdbcStore, MysqlJdbcStoreBuilder};

#[cfg(not(feature = "jdbc-bridge"))]
mod stub;
#[cfg(not(feature = "jdbc-bridge"))]
pub use stub::{MysqlJdbcStore, MysqlJdbcStoreBuilder};

/// Whether the crate was compiled with the JDBC façade bridge.
pub fn jdbc_bridge_available() -> bool {
    cfg!(feature = "jdbc-bridge")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_flag_matches_the_compile_target() {
        assert_eq!(jdbc_bridge_available(), cfg!(feature = "jdbc-bridge"));
    }

    #[cfg(not(feature = "jdbc-bridge"))]
    #[test]
    fn building_without_the_feature_reports_driver_unavailable() {
        let error = MysqlJdbcStoreBuilder::new("mysql://root@127.0.0.1:3306/agentscope")
            .build()
            .expect_err("the bridge must not pretend to build without its feature");
        assert!(
            matches!(error, crate::MysqlError::DriverUnavailable(_)),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("jdbc-bridge"));

        let error = MysqlJdbcStore::connect("mysql://root@127.0.0.1:3306/agentscope")
            .expect_err("the bridge must not pretend to connect without its feature");
        assert!(matches!(
            error,
            crate::MysqlError::DriverUnavailable(_)
        ));
    }
}
