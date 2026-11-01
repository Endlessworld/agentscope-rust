//! Redis-backed sandbox execution guard, mirroring
//! `io.agentscope.extensions.redis.sandbox.RedisSandboxExecutionGuard`.
//!
//! ## Contract alignment
//!
//! The harness ships `SandboxExecutionGuard` as a **concrete in-process struct** plus the
//! pluggable `SandboxGuard` trait (whose `try_enter` / `release` are `async`, so a
//! distributed backend can `await` its own polling loop). This module therefore provides a
//! *distributed* guard with the **same method surface** (`try_enter` / `release` /
//! `is_held`) so it can be used interchangeably by callers that need cross-process
//! serialisation, backed by a Redis `SET NX PX` lease. The trait impl awaits the
//! acquisition directly — no `futures::executor::block_on` bridge.
//!
//! ## Diagnostics: local view vs distributed probe
//!
//! `SandboxGuard::is_held` is synchronous, so it cannot round-trip to Redis. On this guard
//! it answers from the **local** lease registry (the keys this process acquired through the
//! trait) and never touches the network — blocked Tokio workers and deadlocked
//! `current_thread` runtimes are exactly why (REDIS-07). Two surfaces give the distributed
//! answer instead:
//!
//! * [`SandboxGuard::try_is_held`] — the async trait seam, which awaits a real `EXISTS`
//!   probe and returns the error instead of reporting "free" (`Ok(false)`) when Redis is
//!   unreachable. Callers that can `await` should prefer it.
//! * [`RedisSandboxExecutionGuard::probe_is_held`] — the same probe as an inherent method.
//!
//! The local view also cannot see a lease held by *another* process, and it ignores leases
//! taken through the inherent [`RedisSandboxExecutionGuard::try_enter`] (which are not
//! tracked here); only the distributed probe answers those.
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
//! Release failures are logged and swallowed, never propagated, so the harness
//! post-call path always completes (matching the Java contract).
//!
//! ## Release explicitly; `Drop` is only a backstop
//!
//! [`RedisSandboxLease::release`] runs the token-CAS delete on the caller's task. `Drop`
//! runs the same script as a spawned task (or, with no runtime at all, through a blocking
//! bridge), so a runtime shutdown can discard the task before it is polled and the slot
//! then stays taken until the lease TTL expires ([`DEFAULT_LEASE_TTL`], 30 min). Hosts that
//! need the slot back deterministically, or that are about to shut the runtime down, must
//! await `release()` instead of relying on `Drop` (REDIS-14).
//!
//! All Redis access goes through the crate-internal [`RedisCommands`] seam, so the whole
//! acquire / compete / release lifecycle is covered by offline tests.
//!
//! ## Not re-entrant
//!
//! Unlike the harness in-process `SandboxExecutionGuard`, a Redis lease is **not**
//! re-entrant: `SET NX` fails while the key exists, so a second `try_enter` for a key held by
//! the same or another caller keeps spinning until the lease is released or its TTL expires
//! (Java behaves the same way). Nested callers must reuse the one [`RedisSandboxLease`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use redis::Value;

use agentscope_core::error::Result;
use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey, SandboxLease};

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
    /// Concrete leases acquired through the harness `SandboxGuard` trait, keyed by
    /// isolation key.
    ///
    /// The harness `SandboxLease` carries no release hook, so the [`RedisSandboxLease`]
    /// (which deletes the Redis key on drop) must be retained until
    /// `SandboxGuard::release` is called.
    trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<RedisSandboxLease>>>>,
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

    /// Whether the given key is currently held in **Redis**, by this process or any other.
    ///
    /// Awaits a single `EXISTS` round-trip; the error is propagated, so a Redis failure can
    /// never read as "the slot is free". This is the distributed counterpart of the
    /// synchronous [`SandboxGuard::is_held`] local view, and what
    /// [`SandboxGuard::try_is_held`] delegates to.
    pub async fn probe_is_held(&self, key: &SandboxIsolationKey) -> Result<bool> {
        let redis_key = self.compose_key(key);
        self.commands.exists(&redis_key).await
    }
}

#[async_trait::async_trait]
impl SandboxGuard for RedisSandboxExecutionGuard {
    /// Acquires the Redis lease by awaiting the `SET NX PX` retry loop.
    ///
    /// The loop sleeps with `tokio::time::sleep` between attempts, so it must be driven by
    /// an async runtime (it used to be wrapped in `futures::executor::block_on`, which spun
    /// or deadlocked when the acquisition ran inside a Tokio task).
    ///
    /// **Fail-closed** (REDIS-06): when Redis rejects the `SET NX PX` (connection lost,
    /// AUTH failure, wrong type) the error is returned and the caller aborts. It used to be
    /// logged and downgraded to [`SandboxLease::noop`], which let the sandbox run without
    /// the cross-process exclusion the guard exists to provide.
    async fn try_enter(&self, key: SandboxIsolationKey) -> Result<SandboxLease> {
        match RedisSandboxExecutionGuard::try_enter(self, &key).await {
            Ok(lease) => {
                if let Ok(mut leases) = self.trait_leases.lock() {
                    leases.entry(key.clone()).or_default().push(lease);
                }
                Ok(SandboxLease::new(key))
            }
            Err(e) => {
                tracing::error!(error = %e, "[sandbox-guard] failed to acquire Redis lease");
                Err(e)
            }
        }
    }

    async fn release(&self, key: &SandboxIsolationKey) {
        let dropped =
            self.trait_leases
                .lock()
                .ok()
                .and_then(|mut leases| match leases.get_mut(key) {
                    Some(stack) => {
                        let lease = stack.pop();
                        if stack.is_empty() {
                            leases.remove(key);
                        }
                        lease
                    }
                    None => None,
                });
        match dropped {
            // The token CAS delete runs on this task, never through a blocking bridge.
            Some(lease) => {
                lease.release().await;
            }
            None => {
                RedisSandboxExecutionGuard::release(self, key).await;
            }
        }
    }

    /// Local view: is this process currently holding a lease for `key`?
    ///
    /// Answers from the in-process registry of trait-acquired leases and performs **no**
    /// Redis round-trip, so it is safe to call from a synchronous context inside an async
    /// task. It cannot see a lease held by another process (and does not model leases taken
    /// through the inherent [`RedisSandboxExecutionGuard::try_enter`]). Callers that can
    /// `await` and need the distributed answer must use
    /// [`SandboxGuard::try_is_held`]; this method used to bridge to `EXISTS` with
    /// `futures::executor::block_on`, which blocked a Tokio worker and could hang a
    /// `current_thread` runtime (REDIS-07).
    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        self.trait_leases
            .lock()
            .map(|leases| leases.get(key).is_some_and(|stack| !stack.is_empty()))
            .unwrap_or(false)
    }

    /// Async diagnostics probe: awaits the Redis `EXISTS` round-trip.
    ///
    /// Unlike the synchronous local view above this sees every holder, and a failing probe
    /// is an `Err` rather than a silent `false`.
    async fn try_is_held(&self, key: &SandboxIsolationKey) -> Result<bool> {
        self.probe_is_held(key).await
    }

    fn active_count(&self) -> usize {
        self.trait_leases
            .lock()
            .map(|leases| leases.values().map(|stack| stack.len()).sum())
            .unwrap_or(0)
    }

    /// Redis `SET NX PX` serialises across processes.
    fn is_distributed(&self) -> bool {
        true
    }
}

/// Lease returned by [`RedisSandboxExecutionGuard::try_enter`].
///
/// Releasing is idempotent and attempted on drop; failures are logged, not thrown. `Drop` is
/// a best-effort backstop (see the module docs), so a caller that needs the slot back before
/// the runtime shuts down must await [`RedisSandboxLease::release`] explicitly.
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
    ///
    /// Diagnostic getter: the release paths read the flag directly, so the only callers are
    /// idempotence tests and operators (REDIS-10).
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
        // Backstop only: a shutting-down runtime can discard the task, which is why a caller
        // that needs a deterministic release must call `release().await` (REDIS-14).
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
    ///
    /// # Panics
    /// A blank prefix is rejected, mirroring Java's `IllegalArgumentException`: falling back
    /// to the default would let two differently-configured guards share one lock keyspace,
    /// i.e. silently serialise unrelated tenants (REDIS-12).
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        assert!(!prefix.trim().is_empty(), "keyPrefix must not be blank");
        self.key_prefix = normalize_prefix(&prefix);
        self
    }

    /// Sets the lease TTL.
    ///
    /// # Panics
    /// A zero TTL is rejected rather than rounded up to 1ms (Java throws
    /// `IllegalArgumentException`), so a mis-configured lease can never look acquired but
    /// already be expired (REDIS-12).
    pub fn lease_ttl(mut self, ttl: Duration) -> Self {
        assert!(!ttl.is_zero(), "leaseTtl must be positive");
        self.lease_ttl_ms = ttl.as_millis().max(1) as u64;
        self
    }

    /// Sets the retry interval between acquisition attempts.
    ///
    /// # Panics
    /// A zero interval is rejected, matching the TTL check above (REDIS-12).
    pub fn retry_interval(mut self, interval: Duration) -> Self {
        assert!(!interval.is_zero(), "retryInterval must be positive");
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
            trait_leases: Arc::new(Mutex::new(HashMap::new())),
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

    #[test]
    #[should_panic(expected = "keyPrefix must not be blank")]
    fn a_blank_key_prefix_is_rejected() {
        // Java's `Builder.keyPrefix` throws IllegalArgumentException. Falling back to the
        // default would silently share one lock keyspace across differently-configured
        // guards, i.e. serialise unrelated tenants (REDIS-12).
        let commands = Arc::new(MemoryRedisCommands::new());
        let _ = RedisSandboxExecutionGuard::builder_from_commands(commands).key_prefix("   ");
    }

    #[test]
    #[should_panic(expected = "leaseTtl must be positive")]
    fn a_zero_lease_ttl_is_rejected() {
        // It used to be rounded up to 1ms, which hands out a lease that is expired before
        // the caller can use it (REDIS-12).
        let commands = Arc::new(MemoryRedisCommands::new());
        let _ = RedisSandboxExecutionGuard::builder_from_commands(commands)
            .lease_ttl(Duration::ZERO);
    }

    #[test]
    #[should_panic(expected = "retryInterval must be positive")]
    fn a_zero_retry_interval_is_rejected() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let _ = RedisSandboxExecutionGuard::builder_from_commands(commands)
            .retry_interval(Duration::ZERO);
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
        assert!(guard.probe_is_held(&key).await.expect("probe answers"));

        lease.release().await;
    }

    #[tokio::test]
    async fn explicit_release_is_idempotent_and_frees_the_slot() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("Agent", "a");
        let lease = guard.try_enter(&key).await.unwrap();

        assert!(lease.release().await, "first release owns the token");
        assert!(lease.is_released());
        assert!(!guard.probe_is_held(&key).await.expect("probe answers"));
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
            assert!(guard.probe_is_held(&key).await.expect("probe answers"));
        }
        // Drop scheduled the CAS release on the ambient runtime
        tokio::task::yield_now().await;
        assert_eq!(
            commands.release_eval_calls(),
            1,
            "Drop ran the release script once"
        );
        assert!(
            !guard.probe_is_held(&key).await.expect("probe answers"),
            "slot freed by Drop"
        );
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
        assert!(guard.probe_is_held(&key).await.expect("probe answers"));
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
        assert!(!guard.probe_is_held(&key).await.expect("probe answers"));
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

    /// The harness `SandboxGuard` impl delegates to the async guard: acquire, report held,
    /// keep the lease alive until `release`, then clear the Redis key.
    #[tokio::test]
    async fn harness_sandbox_guard_trait_wraps_the_redis_lease() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock:")
            .build();
        let key = SandboxIsolationKey::new("session", "trait");

        let lease = SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect("trait acquisition succeeds");
        assert!(!lease.is_noop(), "lease acquired through the trait");
        assert!(SandboxGuard::is_held(&guard, &key));
        assert_eq!(SandboxGuard::active_count(&guard), 1);
        assert!(commands.stored_bytes("lock:session:trait").is_some());

        SandboxGuard::release(&guard, &key).await;
        assert!(!SandboxGuard::is_held(&guard, &key));
        assert_eq!(SandboxGuard::active_count(&guard), 0);
        assert!(commands.stored_bytes("lock:session:trait").is_none());

        // The trait object reports the Redis guard as cross-process.
        assert!(SandboxGuard::is_distributed(&guard));
        let object: Arc<dyn SandboxGuard> = Arc::new(guard);
        assert!(object.is_distributed());
    }

    /// The trait methods are genuinely async: awaiting them inside a `tokio::time::timeout`
    /// must complete instead of being bridged through `futures::executor::block_on` (which
    /// used to stall a runtime worker while the retry loop slept).
    #[tokio::test]
    async fn trait_acquisition_and_release_are_awaitable() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock:")
            .retry_interval(Duration::from_millis(10))
            .build();
        let key = SandboxIsolationKey::new("session", "awaitable");

        let lease = tokio::time::timeout(
            Duration::from_millis(500),
            SandboxGuard::try_enter(&guard, key.clone()),
        )
        .await
        .expect("trait acquisition must not deadlock the runtime")
        .expect("trait acquisition succeeds");
        assert!(!lease.is_noop());
        assert_eq!(commands.set_nx_calls(), 1, "uncontended: one SET NX");

        tokio::time::timeout(
            Duration::from_millis(500),
            SandboxGuard::release(&guard, &key),
        )
        .await
        .expect("trait release must not deadlock the runtime");
        assert!(!guard.probe_is_held(&key).await.expect("probe answers"));
        assert!(commands.stored_bytes("lock:session:awaitable").is_none());
    }

    /// REDIS-06 (fail-closed): when the lock store rejects the acquisition, the **trait**
    /// surface must return the error. It used to log and downgrade to
    /// [`SandboxLease::noop`], so the caller ran the sandbox with no cross-process
    /// exclusion at all while reporting success.
    #[tokio::test]
    async fn trait_acquisition_failure_is_an_error_not_a_noop_lease() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock:")
            .retry_interval(Duration::from_millis(1))
            .build();
        commands.configure(|d| d.fail_set_nx = true);
        let key = SandboxIsolationKey::new("session", "fail-closed");

        let error = SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect_err("a failed acquisition must not yield a lease");
        assert!(
            format!("{error}").contains("Redis sandbox guard SET NX failed"),
            "{error}"
        );
        assert_eq!(
            SandboxGuard::active_count(&guard),
            0,
            "no lease may be retained after a failed acquisition"
        );
        assert!(!guard.probe_is_held(&key).await.expect("probe answers"));
        assert!(
            commands.stored_bytes("lock:session:fail-closed").is_none(),
            "a rejected SET NX must not leave a lock key behind"
        );
    }

    /// REDIS-06: the failure is not sticky — once the lock store answers again the same
    /// guard acquires normally, so fail-closed never turns into fail-forever.
    #[tokio::test]
    async fn trait_acquisition_recovers_after_a_failed_attempt() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock:")
            .retry_interval(Duration::from_millis(1))
            .build();
        let key = SandboxIsolationKey::new("session", "recover");

        commands.configure(|d| d.fail_set_nx = true);
        SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect_err("lock store is down");
        assert_eq!(SandboxGuard::active_count(&guard), 0);

        commands.configure(|d| d.fail_set_nx = false);
        let lease = SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect("the guard must recover once the lock store answers");
        assert!(!lease.is_noop());
        assert_eq!(SandboxGuard::active_count(&guard), 1);
        assert!(guard.probe_is_held(&key).await.expect("probe answers"));

        SandboxGuard::release(&guard, &key).await;
        assert_eq!(SandboxGuard::active_count(&guard), 0);
        assert!(commands.stored_bytes("lock:session:recover").is_none());
    }

    /// REDIS-06: each held slot is tracked independently, so releasing one never frees the
    /// other's Redis key (the harness reports `active_count` per acquired lease).
    #[tokio::test]
    async fn trait_leases_are_tracked_per_key() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock:")
            .retry_interval(Duration::from_millis(1))
            .build();
        let first = SandboxIsolationKey::new("session", "one");
        let second = SandboxIsolationKey::new("session", "two");

        SandboxGuard::try_enter(&guard, first.clone())
            .await
            .expect("first slot");
        SandboxGuard::try_enter(&guard, second.clone())
            .await
            .expect("second slot");
        assert_eq!(SandboxGuard::active_count(&guard), 2);

        SandboxGuard::release(&guard, &first).await;
        assert_eq!(SandboxGuard::active_count(&guard), 1);
        assert!(
            !guard.probe_is_held(&first).await.expect("probe answers"),
            "the released slot is free again"
        );
        assert!(
            guard.probe_is_held(&second).await.expect("probe answers"),
            "the other slot is untouched"
        );

        SandboxGuard::release(&guard, &second).await;
        assert_eq!(SandboxGuard::active_count(&guard), 0);
        assert!(commands.stored_bytes("lock:session:two").is_none());
    }

    /// REDIS-06: a held slot is not handed out twice — the second acquisition spins on
    /// `SET NX` until the lease is released (same as Java `tryEnter`).
    #[tokio::test]
    async fn a_held_lease_is_not_reentrant() {
        let (guard, _commands) = guard("lock:", Duration::from_millis(5));
        let key = SandboxIsolationKey::new("session", "no-reentry");
        let lease = guard.try_enter(&key).await.unwrap();

        let second = tokio::time::timeout(Duration::from_millis(50), guard.try_enter(&key)).await;
        assert!(second.is_err(), "a held key must not be handed out twice");

        lease.release().await;
        let again = guard
            .try_enter(&key)
            .await
            .expect("released slot is acquirable again");
        again.release().await;
    }

    /// REDIS-07: a failing probe must not read as "free". The async surface returns the
    /// error, while the synchronous local view keeps answering from the in-process lease
    /// registry without touching Redis.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_is_held_surfaces_a_failed_probe_instead_of_reporting_free() {
        let commands = Arc::new(MemoryRedisCommands::new());
        let guard = RedisSandboxExecutionGuard::builder_from_commands(commands.clone())
            .key_prefix("lock:")
            .build();
        let key = SandboxIsolationKey::new("session", "probe");
        SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect("trait acquisition succeeds");

        commands.configure(|d| d.fail_exists = true);
        let error = SandboxGuard::try_is_held(&guard, &key)
            .await
            .expect_err("a failed probe must not be reported as a free slot");
        assert!(
            format!("{error}").contains("injected EXISTS failure"),
            "{error}"
        );
        assert_eq!(commands.exists_calls(), 1, "one probe, no retry storm");

        assert!(
            SandboxGuard::is_held(&guard, &key),
            "the local view still knows this process holds the lease"
        );
        assert_eq!(
            commands.exists_calls(),
            1,
            "the synchronous local view must not probe Redis"
        );

        commands.configure(|d| d.fail_exists = false);
        assert!(SandboxGuard::try_is_held(&guard, &key)
            .await
            .expect("the probe recovers"));
        assert_eq!(commands.exists_calls(), 2);

        SandboxGuard::release(&guard, &key).await;
        assert!(!SandboxGuard::is_held(&guard, &key));
        assert!(!SandboxGuard::try_is_held(&guard, &key)
            .await
            .expect("a released slot is free"));
    }

    /// REDIS-07: `try_is_held` awaits the `EXISTS` round-trip instead of bridging it with
    /// `futures::executor::block_on`, so it completes on a single-threaded runtime, and
    /// the synchronous `is_held` reads only the local lease registry (no network).
    #[tokio::test(flavor = "current_thread")]
    async fn try_is_held_awaits_the_probe_without_blocking_a_current_thread_runtime() {
        let (guard, commands) = guard("lock:", Duration::from_millis(1));
        let key = SandboxIsolationKey::new("session", "current-thread");

        // Nothing is held yet: both surfaces agree, and only `try_is_held` asked Redis.
        assert!(!SandboxGuard::is_held(&guard, &key));
        assert!(!SandboxGuard::try_is_held(&guard, &key)
            .await
            .expect("probe answers"));
        assert_eq!(
            commands.exists_calls(),
            1,
            "only the async probe round-tripped"
        );

        SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect("a lease is acquirable on a current_thread runtime");
        let probes = commands.exists_calls();
        assert!(
            SandboxGuard::is_held(&guard, &key),
            "the local view sees the lease"
        );
        assert_eq!(
            commands.exists_calls(),
            probes,
            "the synchronous view never touches Redis"
        );

        let held = tokio::time::timeout(
            Duration::from_millis(500),
            SandboxGuard::try_is_held(&guard, &key),
        )
        .await
        .expect("the awaitable probe must not block the runtime")
        .expect("probe answers");
        assert!(held);

        SandboxGuard::release(&guard, &key).await;
        assert!(!SandboxGuard::is_held(&guard, &key));
        assert!(!SandboxGuard::try_is_held(&guard, &key)
            .await
            .expect("a released slot is free"));
    }
}
