//! Redis-backed sandbox execution guard, mirroring
//! `io.agentscope.extensions.redis.sandbox.RedisSandboxExecutionGuard`.
//!
//! ## Contract alignment
//!
//! The harness ships `SandboxExecutionGuard` as a **concrete in-process struct**
//! (`try_enter`/`release`/`is_held`), not a trait. The extensions rollup ruling (§2.1)
//! says storage modules must provide an `XxxSandboxExecutionGuard` while CAS/version
//! semantics follow the harness definition. This module therefore provides a
//! *distributed* guard with the **same method surface** (`try_enter` / `release` /
//! `is_held`) so it can be used interchangeably by callers that need cross-process
//! serialisation, backed by a Redis `SET NX PX` lease.
//!
//! ## Mechanism
//!
//! 1. [`RedisSandboxExecutionGuard::try_enter`] writes a unique token to a Redis key with
//!    `SET NX PX <ttlMs>`. If the slot is already held by another caller it retries at
//!    `retry_interval_ms` until the slot is free.
//! 2. The returned [`RedisSandboxLease`] releases the slot on drop via a Lua CAS script
//!    that deletes the key only when the stored value equals the caller's token, so a
//!    different holder's lock can never be released by mistake.
//!
//! [`RedisSandboxLease::release`] is idempotent: the `released` flag is the single source
//! of truth, so an explicit release followed by a drop cannot run the script twice.
//!
//! Release failures are logged and swallowed — never propagated — so the harness
//! post-call path always completes (matching the Java contract).
//!
//! All Redis access goes through the crate-internal [`RedisCommands`] seam, so the whole
//! acquire / compete / release lifecycle is covered by offline tests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use redis::Value;

use agentscope_core::error::Result;
use agentscope_harness::agent::sandbox::SandboxIsolationKey;

use crate::raw_client::RedisCommands;
use crate::state::connection::RedisConnection;

/// Lua script: atomically delete `KEYS[1]` only when its value equals `ARGV[1]`.
pub(crate) const RELEASE_SCRIPT: &str = "if redis.call('get', KEYS[1]) == ARGV[1] then \
     return redis.call('del', KEYS[1]) \
     else \
     return 0 \
     end";

/// Default lock key prefix.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:sandbox:lock:";
/// Default lease TTL (30 minutes).
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30 * 60);
/// Default retry interval (500 ms).
pub const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_millis(500);

fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return DEFAULT_KEY_PREFIX.to_string();
    }
    if trimmed.ends_with(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:")
    }
}

/// Redis-backed distributed sandbox execution guard (`SET NX PX` lease).
#[derive(Clone)]
pub struct RedisSandboxExecutionGuard {
    commands: Arc<dyn RedisCommands>,
    key_prefix: String,
    lease_ttl_ms: u64,
    retry_interval_ms: u64,
}

impl std::fmt::Debug for RedisSandboxExecutionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisSandboxExecutionGuard")
            .field("key_prefix", &self.key_prefix)
            .field("lease_ttl_ms", &self.lease_ttl_ms)
            .field("retry_interval_ms", &self.retry_interval_ms)
            .finish_non_exhaustive()
    }
}

impl RedisSandboxExecutionGuard {
    /// Creates a builder from a shared [`RedisConnection`].
    pub fn builder(connection: &RedisConnection) -> RedisSandboxExecutionGuardBuilder {
        RedisSandboxExecutionGuardBuilder {
            commands: connection.raw_client(),
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
            lease_ttl_ms: DEFAULT_LEASE_TTL.as_millis() as u64,
            retry_interval_ms: DEFAULT_RETRY_INTERVAL.as_millis() as u64,
        }
    }

    /// Creates a builder over an arbitrary command backend (production connection, or in
    /// tests an in-memory double).
    #[cfg(test)]
    pub(crate) fn builder_from_commands(
        commands: Arc<dyn RedisCommands>,
    ) -> RedisSandboxExecutionGuardBuilder {
        RedisSandboxExecutionGuardBuilder {
            commands,
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
            lease_ttl_ms: DEFAULT_LEASE_TTL.as_millis() as u64,
            retry_interval_ms: DEFAULT_RETRY_INTERVAL.as_millis() as u64,
        }
    }

    /// The configured key prefix (normalized to end with `:`).
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// The configured lease TTL in milliseconds.
    pub fn lease_ttl_ms(&self) -> u64 {
        self.lease_ttl_ms
    }

    /// The configured retry interval in milliseconds.
    pub fn retry_interval_ms(&self) -> u64 {
        self.retry_interval_ms
    }

    /// Composes the Redis lock key: `<keyPrefix><scope_lower>:<value>`.
    pub(crate) fn compose_key(&self, key: &SandboxIsolationKey) -> String {
        format!(
            "{}{}:{}",
            self.key_prefix,
            key.scope.to_lowercase(),
            key.value
        )
    }

    /// Acquires the Redis-backed lease for `key`, spinning at `retry_interval` until the
    /// slot is free.
    pub async fn try_enter(&self, key: &SandboxIsolationKey) -> Result<RedisSandboxLease> {
        let redis_key = self.compose_key(key);
        let token = uuid::Uuid::new_v4().to_string();
        loop {
            let acquired = self
                .commands
                .set_nx_px(&redis_key, &token, self.lease_ttl_ms)
                .await
                .map_err(|e| {
                    agentscope_core::error::Error::Other(format!(
                        "Redis sandbox guard SET NX failed: {e}"
                    ))
                })?;
            if acquired {
                tracing::debug!("[sandbox-guard] acquired Redis lease for key={redis_key}");
                return Ok(RedisSandboxLease {
                    commands: self.commands.clone(),
                    key: redis_key,
                    token,
                    released: AtomicBool::new(false),
                });
            }
            tokio::time::sleep(Duration::from_millis(self.retry_interval_ms)).await;
        }
    }

    /// Releases the lease for `key` unconditionally-ish (best-effort `DEL`, no token CAS).
    ///
    /// Provided for parity with the harness guard's `release(&key)`; normally the
    /// [`RedisSandboxLease`] drop handles this. Returns `true` if a lock was still present
    /// at release time.
    pub async fn release(&self, key: &SandboxIsolationKey) -> bool {
        let redis_key = self.compose_key(key);
        match self.commands.del(&redis_key).await {
            Ok(deleted) => deleted > 0,
            Err(e) => {
                tracing::warn!("[sandbox-guard] failed to release Redis lease {redis_key}: {e}");
                false
            }
        }
    }

    /// Whether the given key is currently held (by anyone).
    pub async fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        let redis_key = self.compose_key(key);
        self.commands.exists(&redis_key).await.unwrap_or(false)
    }
}

/// Lease returned by [`RedisSandboxExecutionGuard::try_enter`].
///
/// Releasing is idempotent and performed on drop; failures are logged, not thrown.
#[derive(Debug)]
pub struct RedisSandboxLease {
    commands: Arc<dyn RedisCommands>,
    key: String,
    token: String,
    released: AtomicBool,
}

impl RedisSandboxLease {
    /// The Redis key backing this lease.
    pub fn redis_key(&self) -> &str {
        &self.key
    }

    /// The unique token written to the lock key (the CAS identity of this lease).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Whether this lease has already been released (explicitly or on drop).
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    /// Releases the lease, returning `true` if the lock was still owned by this token.
    ///
    /// Idempotent: once released (or once a release script reported the lock gone) the
    /// flag is set and the script is never run again — including from `Drop`.
    pub async fn release(&self) -> bool {
        if self.is_released() {
            return false;
        }
        match self
            .commands
            .eval_one_key(RELEASE_SCRIPT, &self.key, &self.token)
            .await
        {
            Ok(Value::Int(1)) => {
                self.released.store(true, Ordering::SeqCst);
                true
            }
            Ok(_) => {
                self.released.store(true, Ordering::SeqCst);
                tracing::warn!(
                    "[sandbox-guard] Redis lease for key={} was already gone or held by a different token",
                    self.key
                );
                false
            }
            Err(e) => {
                // Leave the flag clear so `Drop` can retry once.
                tracing::warn!(
                    "[sandbox-guard] failed to release Redis lease for key={}: {e}",
                    self.key
                );
                false
            }
        }
    }
}

impl Drop for RedisSandboxLease {
    fn drop(&mut self) {
        // `swap` claims the release: if an explicit `release()` already ran (or a previous
        // drop did) this is a no-op.
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let commands = self.commands.clone();
        let key = std::mem::take(&mut self.key);
        let token = std::mem::take(&mut self.token);
        let release = async move {
            match commands.eval_one_key(RELEASE_SCRIPT, &key, &token).await {
                Ok(Value::Int(1)) => {
                    tracing::debug!("[sandbox-guard] released Redis lease for key={key}")
                }
                Ok(_) => tracing::warn!(
                    "[sandbox-guard] Redis lease for key={key} was already gone or held by a different token"
                ),
                Err(e) => {
                    tracing::warn!("[sandbox-guard] failed to release Redis lease for key={key}: {e}")
                }
            }
        };
        // Prefer the ambient runtime so a worker thread is never blocked; fall back to
        // blocking only when there is no runtime (the crate's sync→async bridging shape).
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(release);
            }
            Err(_) => {
                futures::executor::block_on(release);
            }
        }
    }
}

/// Builder for [`RedisSandboxExecutionGuard`].
pub struct RedisSandboxExecutionGuardBuilder {
    commands: Arc<dyn RedisCommands>,
    key_prefix: String,
    lease_ttl_ms: u64,
    retry_interval_ms: u64,
}

impl RedisSandboxExecutionGuardBuilder {
    /// Sets the Redis key prefix (default [`DEFAULT_KEY_PREFIX`]).
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = normalize_prefix(&prefix.into());
        self
    }

    /// Sets the lease TTL (must be positive).
    pub fn lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl_ms = ttl.as_millis().max(1) as u64;
        self
    }

    /// Sets the retry interval between acquisition attempts (must be positive).
    pub fn retry_interval(mut self, interval: Duration) -> Self {
        self.retry_interval_ms = interval.as_millis().max(1) as u64;
        self
    }

    /// Builds the guard.
    pub fn build(self) -> RedisSandboxExecutionGuard {
        RedisSandboxExecutionGuard {
            commands: self.commands,
            key_prefix: self.key_prefix,
            lease_ttl_ms: self.lease_ttl_ms,
            retry_interval_ms: self.retry_interval_ms,
        }
    }
}

/// Shared handle alias mirroring the `Arc<dyn ...>` shape used elsewhere.
pub type SharedRedisSandboxExecutionGuard = Arc<RedisSandboxExecutionGuard>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryRedisCommands;

    fn guard(
        prefix: &str,
        retry: Duration,
    ) -> (RedisSandboxExecutionGuard, Arc<MemoryRedisCommands>) {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix(prefix)
            .retry_interval(retry)
            .build();
        (guard, commands)
    }

    #[test]
    fn normalize_prefix_rules() {
        assert_eq!(normalize_prefix(""), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_prefix("  "), DEFAULT_KEY_PREFIX);
        assert_eq!(normalize_prefix("lock:"), "lock:");
        assert_eq!(normalize_prefix("lock"), "lock:");
    }

    #[test]
    fn defaults_match_java_constants() {
        assert_eq!(DEFAULT_LEASE_TTL, Duration::from_secs(1800), "30 minutes");
        assert_eq!(DEFAULT_RETRY_INTERVAL, Duration::from_millis(500));
        assert_eq!(DEFAULT_KEY_PREFIX, "agentscope:sandbox:lock:");
    }

    #[test]
    fn builder_keeps_defaults_and_overrides() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands).build();
        assert_eq!(guard.key_prefix(), DEFAULT_KEY_PREFIX);
        assert_eq!(guard.lease_ttl_ms(), 1_800_000);
        assert_eq!(guard.retry_interval_ms(), 500);
    }

    #[test]
    fn compose_key_format_lowercases_scope() {
        let (guard, _commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "code-agent");
        assert_eq!(guard.compose_key(&key), "lock:agent:code-agent");
    }

    #[tokio::test]
    async fn try_enter_writes_nx_lease_with_ttl_and_unique_token() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "a");
        let lease = guard.try_enter(&key).await.unwrap();

        assert_eq!(lease.redis_key(), "lock:agent:a");
        assert!(!lease.token().is_empty());
        assert!(!lease.is_released());
        assert_eq!(commands.set_nx_calls(), 1, "acquired on the first attempt");
        assert_eq!(
            commands.stored_bytes("lock:agent:a"),
            Some(lease.token().as_bytes().to_vec())
        );
        // lease_ttl default is 30min -> milliseconds, converted to seconds by the double
        assert_eq!(commands.expiration_of("lock:agent:a"), Some(1800));
        assert!(guard.is_held(&key).await);

        lease.release().await;
    }

    #[tokio::test]
    async fn explicit_release_is_idempotent_and_frees_the_slot() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "a");
        let lease = guard.try_enter(&key).await.unwrap();

        assert!(lease.release().await, "first release owns the token");
        assert!(lease.is_released());
        assert!(!guard.is_held(&key).await);
        let evals_after_first = commands.release_eval_calls();

        assert!(!lease.release().await, "second release is a no-op");
        assert_eq!(
            commands.release_eval_calls(),
            evals_after_first,
            "script not run twice"
        );

        // drop must not run the script a third time either
        drop(lease);
        tokio::task::yield_now().await;
        assert_eq!(commands.release_eval_calls(), evals_after_first);
    }

    #[tokio::test]
    async fn drop_releases_the_lease() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "drop");
        {
            let _lease = guard.try_enter(&key).await.unwrap();
            assert!(guard.is_held(&key).await);
        }
        // Drop scheduled the CAS release on the ambient runtime
        tokio::task::yield_now().await;
        assert_eq!(
            commands.release_eval_calls(),
            1,
            "Drop ran the release script once"
        );
        assert!(!guard.is_held(&key).await, "slot freed by Drop");
    }

    #[tokio::test]
    async fn a_foreign_token_cannot_release_someone_elses_lease() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "a");
        let lease = guard.try_enter(&key).await.unwrap();

        // Simulate a different holder overwriting the slot with its own token.
        commands.configure(|d| {
            d.strings
                .insert("lock:agent:a".into(), b"someone-else".to_vec());
        });
        assert!(!lease.release().await, "CAS must not delete a foreign lock");
        assert_eq!(
            commands.stored_bytes("lock:agent:a").as_deref(),
            Some(b"someone-else".as_ref()),
            "foreign lock untouched"
        );
        // now that it is marked released, Drop stays silent
        drop(lease);
        tokio::task::yield_now().await;
        assert_eq!(commands.release_eval_calls(), 1);
    }

    #[tokio::test]
    async fn competing_acquisition_spins_until_released() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "contended");
        let holder = guard.try_enter(&key).await.unwrap();

        // A second acquisition cannot succeed while the first is held: the loop retries.
        let blocked = tokio::time::timeout(Duration::from_millis(50), guard.try_enter(&key)).await;
        assert!(
            blocked.is_err(),
            "second acquisition must spin while the lease is held"
        );
        assert!(
            commands.set_nx_calls() > 1,
            "retry loop actually re-attempted SET NX"
        );

        // Releasing the holder frees the slot for the waiter.
        drop(holder);
        tokio::task::yield_now().await;
        let second = tokio::time::timeout(Duration::from_millis(300), guard.try_enter(&key))
            .await
            .expect("acquire after release")
            .unwrap();
        assert!(guard.is_held(&key).await);
        assert!(second.release().await);
    }

    #[tokio::test]
    async fn set_nx_failure_is_propagated_as_error() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        commands.configure(|d| d.fail_set_nx = true);
        let key = SandboxIsolationKey::new("Agent", "a");
        let err = guard.try_enter(&key).await.unwrap_err();
        assert!(
            format!("{err}").contains("Redis sandbox guard SET NX failed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn guard_level_release_deletes_by_key_and_reports_presence() {
        let (guard, _commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "a");
        let lease = guard.try_enter(&key).await.unwrap();

        assert!(guard.release(&key).await, "DEL removed the key");
        assert!(!guard.release(&key).await, "nothing left to delete");
        assert!(!guard.is_held(&key).await);
        assert!(!lease.release().await, "token CAS finds no lock");
        assert!(lease.is_released());
    }

    #[tokio::test]
    async fn lease_ttl_override_is_applied() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock")
            .lease_ttl(Duration::from_secs(45))
            .retry_interval(Duration::from_millis(1))
            .build();
        assert_eq!(guard.key_prefix(), "lock:", "prefix normalised");
        let key = SandboxIsolationKey::new("Agent", "ttl");
        let lease = guard.try_enter(&key).await.unwrap();
        assert_eq!(commands.expiration_of("lock:agent:ttl"), Some(45));
        lease.release().await;
    }

    #[test]
    fn drop_outside_a_runtime_still_releases() {
        // Plain `#[test]` (no tokio runtime): Drop must fall back to a blocking bridge
        // rather than panicking or silently skipping the release.
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock:")
            .build();
        let lease = RedisSandboxLease {
            commands: commands.clone(),
            key: "lock:agent:bare".to_string(),
            token: "tok-1".to_string(),
            released: AtomicBool::new(false),
        };
        commands.configure(|d| {
            d.strings
                .insert("lock:agent:bare".into(), b"tok-1".to_vec());
        });
        drop(lease);
        assert_eq!(
            commands.release_eval_calls(),
            1,
            "blocking fallback ran the script"
        );
        assert!(commands.stored_bytes("lock:agent:bare").is_none());
        let _ = guard;
    }
}
