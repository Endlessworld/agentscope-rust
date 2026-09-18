//! JDBC-backed sandbox execution guard component, mirroring
//! `io.agentscope.extensions.jdbc.sandbox`.

mod jdbc_guard;

pub use jdbc_guard::{
    JdbcSandboxExecutionGuard, JdbcSandboxLease, TableLockStrategy, DEFAULT_KEY_PREFIX,
    DEFAULT_LOCK_TIMEOUT_SECONDS,
};
