//! PostgreSQL advisory-lock-based sandbox execution guard.
//!
//! **Harness note:** unlike the Java side, the harness `SandboxExecutionGuard` is a
//! concrete in-process struct (`agentscope_harness::agent::sandbox::SandboxExecutionGuard`),
//! not a trait. This crate therefore provides a standalone
//! [`PostgresSandboxExecutionGuard`] mirroring the same API surface
//! (`try_enter` / `release`), backed by PostgreSQL advisory locks, so the
//! distributed guard can be used wherever a process-local one would be. See the module
//! gap tracker §4.2 for the harness abstraction follow-up.
//!
//! Mirrors the Java `PostgresSandboxExecutionGuard`: a 64-bit lock key is derived from
//! the isolation key via a hash, acquired with `pg_try_advisory_lock` (polling until the
//! timeout expires) and released with `pg_advisory_unlock`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_harness::agent::sandbox::SandboxIsolationKey;

use crate::client::{Cell, PostgresClient, QueryOutcome, SqlValue};
use crate::error::{PgResult, PostgresError};

/// Default lock-key prefix, matching the Java `DEFAULT_KEY_PREFIX`.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:sandbox:lock:";
/// Default timeout, matching the Java `DEFAULT_LOCK_TIMEOUT_SECONDS` (30 minutes).
pub const DEFAULT_LOCK_TIMEOUT_SECONDS: u64 = 1800;

/// Builder for [`PostgresSandboxExecutionGuard`].
#[derive(Debug, Clone)]
pub struct PostgresSandboxExecutionGuardBuilder {
    client: Arc<dyn PostgresClient>,
    key_prefix: String,
    lock_timeout: Duration,
}

impl PostgresSandboxExecutionGuardBuilder {
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self {
            client,
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
            lock_timeout: Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECONDS),
        }
    }

    /// Sets the lock-key prefix; a trailing `:` is appended when absent.
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> PgResult<Self> {
        let p = prefix.into();
        if p.trim().is_empty() {
            return Err(PostgresError::InvalidIdentifier(
                "keyPrefix must not be blank".into(),
            ));
        }
        self.key_prefix = if p.ends_with(':') { p } else { format!("{p}:") };
        Ok(self)
    }

    /// Sets the lock acquisition timeout.
    pub fn lock_timeout(mut self, timeout: Duration) -> PgResult<Self> {
        if timeout.is_zero() {
            return Err(PostgresError::Query("timeout must be positive".into()));
        }
        self.lock_timeout = timeout;
        Ok(self)
    }

    pub fn build(self) -> PostgresSandboxExecutionGuard {
        PostgresSandboxExecutionGuard {
            client: self.client,
            key_prefix: self.key_prefix,
            lock_timeout: self.lock_timeout,
        }
    }
}

/// Advisory-lock-backed execution guard.
#[derive(Debug, Clone)]
pub struct PostgresSandboxExecutionGuard {
    client: Arc<dyn PostgresClient>,
    key_prefix: String,
    lock_timeout: Duration,
}

impl PostgresSandboxExecutionGuard {
    /// Creates a builder (mirrors Java `PostgresSandboxExecutionGuard.builder(ds)`).
    pub fn builder(client: Arc<dyn PostgresClient>) -> PostgresSandboxExecutionGuardBuilder {
        PostgresSandboxExecutionGuardBuilder::new(client)
    }

    /// Builds a guard with default settings.
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self::builder(client).build()
    }

    /// Computes the 64-bit advisory-lock key for an isolation key.
    ///
    /// Derivation: `prefix + scope_lowercased + ":" + value`, hashed to `i64`.
    pub fn compose_lock_key(&self, key: &SandboxIsolationKey) -> i64 {
        use std::hash::{Hash, Hasher};
        let raw = format!(
            "{}{}:{}",
            self.key_prefix,
            key.scope.to_lowercase(),
            key.value
        );
        let mut h = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut h);
        h.finish() as i64
    }

    /// Tries to acquire the advisory lock, polling until the timeout expires.
    ///
    /// On success returns a [`PostgresSandboxLease`] whose [`Drop`] releases the lock.
    pub fn try_enter(&self, key: &SandboxIsolationKey) -> PgResult<PostgresSandboxLease> {
        let lock_name = self.compose_lock_key(key);
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            if self.try_lock_once(lock_name)? {
                return Ok(PostgresSandboxLease {
                    client: Arc::clone(&self.client),
                    lock_name,
                    released: false,
                });
            }
            if Instant::now() >= deadline {
                return Err(PostgresError::LockTimeout(format!(
                    "{lock_name} (timeout={}s)",
                    self.lock_timeout.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn try_lock_once(&self, lock_name: i64) -> PgResult<bool> {
        let sql = "SELECT pg_try_advisory_lock($1)";
        let params = [SqlValue::BigInt(lock_name)];
        match self.client.query(sql, &params)? {
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_bool)
                .unwrap_or(false)),
            QueryOutcome::Affected(_) => Ok(false),
        }
    }

    /// Releases a previously-acquired lock by key.
    pub fn release(&self, lock_name: i64) -> PgResult<bool> {
        let sql = "SELECT pg_advisory_unlock($1)";
        let params = [SqlValue::BigInt(lock_name)];
        match self.client.query(sql, &params)? {
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_bool)
                .unwrap_or(false)),
            QueryOutcome::Affected(_) => Ok(false),
        }
    }
}

/// A lease held on a PostgreSQL advisory lock; releases on drop.
pub struct PostgresSandboxLease {
    client: Arc<dyn PostgresClient>,
    lock_name: i64,
    released: bool,
}

impl PostgresSandboxLease {
    /// The underlying advisory-lock key.
    pub fn lock_name(&self) -> i64 {
        self.lock_name
    }

    /// Releases the lock; safe to call multiple times.
    pub fn release(&mut self) -> PgResult<bool> {
        if self.released {
            return Ok(false);
        }
        self.released = true;
        let sql = "SELECT pg_advisory_unlock($1)";
        let params = [SqlValue::BigInt(self.lock_name)];
        match self.client.query(sql, &params)? {
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_bool)
                .unwrap_or(false)),
            QueryOutcome::Affected(_) => Ok(false),
        }
    }
}

impl Drop for PostgresSandboxLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl std::fmt::Debug for PostgresSandboxLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSandboxLease")
            .field("lock_name", &self.lock_name)
            .field("released", &self.released)
            .finish()
    }
}
