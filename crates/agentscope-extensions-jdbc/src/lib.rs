//! agentscope-extensions-jdbc — multi-database JDBC dialect abstraction for
//! AgentScope Rust, mirroring `io.agentscope.extensions.jdbc`.
//!
//! Replaces the deprecated `agentscope-extensions-mysql` and
//! `agentscope-extensions-postgresql` modules with a single module that supports MySQL,
//! PostgreSQL, H2, and SQLite through one aggregated [`dialect::AbstractJdbcDialect`].
//!
//! # Architecture
//!
//! Four layers, top-down:
//!
//! 1. **Facade** — [`JdbcDistributedStore`] auto-detects the dialect and assembles all
//!    four components.
//! 2. **Component layer** (database-agnostic) — [`JdbcStore`], [`JdbcAgentStateStore`],
//!    [`JdbcSnapshotSpec`], [`JdbcSandboxExecutionGuard`],
//!    [`JdbcAgentSkillRepository`]. Zero inline SQL — all SQL sourced from
//!    [`dialect::BoundSql`] returned by dialect methods.
//! 3. **Dialect layer** — [`dialect::AbstractJdbcDialect`] implements all table-domain
//!    traits (`StoreDialect`, `SessionStateDialect`, `SnapshotDialect`, `SkillDialect`,
//!    `SkillResourcesDialect`) plus the lock strategy. Holds a unified table prefix +
//!    per-table overrides.
//! 4. **Vendor layer** — one type per database ([`dialect::vendor`]), overriding only DDL
//!    and divergent SQL syntax.
//!
//! # Reused harness/core traits
//!
//! Per `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1, this crate **implements** existing
//! traits rather than defining parallel ones:
//! * [`agentscope_core::state::AgentStateStore`]
//! * [`agentscope_harness::filesystem::remote::BaseStore`]
//! * [`agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec`] / `RemoteSnapshotClient`
//! * [`agentscope_harness::agent::sandbox::SandboxExecutionGuard`]
//!
//! `put_if_version` CAS semantics, `StoreItem.version` meaning, and snapshot-id validation
//! are all harness-defined.
//!
//! # Execution backend
//!
//! Java components take a `javax.sql.DataSource`. Rust has no universal JDBC surface, so
//! components depend on the [`backend::JdbcExecutor`] trait. The default concrete backend
//! is [`backend::RusqliteExecutor`] (SQLite, including in-memory), which mirrors the Java
//! H2/SQLite test backends.

pub mod backend;
pub mod dialect;
mod distributed_store;
pub mod sandbox;
pub mod skill;
pub mod snapshot;
pub mod state;
pub mod store;

pub use backend::{
    ColumnValue, JdbcError, JdbcExecutor, JdbcResult, JdbcSession, RusqliteExecutor, SharedExecutor,
};
pub use distributed_store::JdbcDistributedStore;
pub use sandbox::{
    JdbcSandboxExecutionGuard, JdbcSandboxLease, NativeLockFlavor, NativeLockGuard,
    NativeLockGuardLease, ProviderBackedGuard, TableLockStrategy,
};
pub use skill::{JdbcAgentSkillRepository, SkillRepositoryError, DEFAULT_NAMESPACE};
pub use snapshot::{JdbcRemoteSnapshotClient, JdbcSnapshotSpec};
pub use state::{JdbcAgentStateStore, VersionedState, UNVERSIONED};
pub use store::JdbcStore;
// Re-export the reused harness/core traits for ergonomic use by callers.
pub use agentscope_core::state::AgentStateStore;
pub use agentscope_harness::filesystem::{BaseStore, StoreItem, StoreValue};

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
///
/// Used as the `updated_at` / `acquired_at` column value across all components.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A monotonic counter used to disambiguate same-millisecond lock acquisitions in tests.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a monotonically increasing sequence number. Useful for ordering-agnostic
/// assertions in tests.
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_millis_is_positive() {
        assert!(now_millis() > 0);
    }

    #[test]
    fn next_seq_is_monotonic() {
        assert!(next_seq() < next_seq());
    }
}
