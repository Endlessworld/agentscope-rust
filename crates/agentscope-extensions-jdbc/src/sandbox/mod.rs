//! JDBC-backed sandbox execution guard component, mirroring
//! `io.agentscope.extensions.jdbc.sandbox`.

mod jdbc_guard;
mod native_lock_guard;

pub use jdbc_guard::{
    compose_lock_name, normalize_key_prefix, JdbcSandboxExecutionGuard, JdbcSandboxLease,
    TableLockStrategy, DEFAULT_KEY_PREFIX, DEFAULT_LOCK_TIMEOUT_SECONDS,
};
pub use native_lock_guard::{
    NativeLockFlavor, NativeLockGuard, NativeLockGuardLease, ProviderBackedGuard,
};
