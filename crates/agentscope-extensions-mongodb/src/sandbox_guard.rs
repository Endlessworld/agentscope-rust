//! MongoDB-based sandbox execution guard, mirroring
//! `io.agentscope.extensions.mongodb.sandbox.MongoSandboxExecutionGuard`.
//!
//! Uses a dedicated collection as a distributed lock. Each lock is a document with a
//! unique `_id` derived from the isolation key and a TTL index on `expiresAt` to
//! auto-release stale locks.
//!
//! Acquisition is a two-step process for correct mutual exclusion:
//! 1. Attempt `insert_one` — atomic under the unique `_id` index.
//! 2. On duplicate key, attempt a non-upsert `find_one_and_update` filtered on
//!    `expiresAt <= now` — reclaiming an expired lock.
//!
//! Each acquisition generates a random token; renewal and release filter by token, so
//! two guard instances cannot release each other's locks (mirroring the Redis guard's
//! `SET NX` + CAS token pattern). While a [`MongodbLease`] is open, a background task
//! renews the lease every `lease_ttl / 3`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey, SandboxLease};
use serde_json::{json, Map};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Instant};

use crate::client::{FindFilter, FindOneAndUpdateOptions, MongoClient, MongoError, Update};
use crate::constants::{DEFAULT_DATABASE, SANDBOX_LOCKS_COLLECTION};
use crate::index_utils::{create_index_with_migration, IndexSpec};

const FIELD_LOCK_ID: &str = "_id";
const FIELD_OWNER: &str = "owner";
const FIELD_TOKEN: &str = "token";
const FIELD_EXPIRES_AT: &str = "expiresAt";

/// A sandbox execution guard backed by MongoDB locks.
///
/// Unlike the Java class (which implements `SandboxExecutionGuard` directly), this
/// Rust type holds a MongoDB client and is used via [`MongodbGuardClient`], which is
/// what actually implements the harness
/// [`SandboxGuard`](agentscope_harness::agent::sandbox::SandboxGuard) trait for a given
/// isolation key.
#[derive(Clone)]
pub struct MongodbSandboxExecutionGuard {
    client: Arc<dyn MongoClient>,
    database_name: String,
    /// Collection key the guard actually talks to: `scoped_collection(database,
    /// collection)`. The in-process backend keeps a flat map, so the database is
    /// encoded into the key (the same convention `MongodbRemoteSnapshotClient`
    /// uses); a real server scopes collections per database natively.
    collection_key: String,
    lock_timeout: Duration,
    lease_ttl: Duration,
    retry_interval: Duration,
    owner: String,
}

impl std::fmt::Debug for MongodbSandboxExecutionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbSandboxExecutionGuard")
            .field("database_name", &self.database_name)
            .field("collection_key", &self.collection_key)
            .field("lock_timeout", &self.lock_timeout)
            .field("lease_ttl", &self.lease_ttl)
            .field("retry_interval", &self.retry_interval)
            .field("owner", &self.owner)
            .finish()
    }
}

impl MongodbSandboxExecutionGuard {
    /// Returns a builder.
    pub fn builder(client: Arc<dyn MongoClient>) -> MongodbSandboxExecutionGuardBuilder {
        MongodbSandboxExecutionGuardBuilder::new(client)
    }

    /// The configured database name.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// The database-scoped collection key the guard reads and writes.
    pub fn collection_key(&self) -> &str {
        &self.collection_key
    }

    /// Ensures the TTL index on `expiresAt` exists.
    pub async fn ensure_indexes(&self) -> Result<()> {
        create_index_with_migration(
            &self.client,
            &self.collection_key,
            &IndexSpec::ascending(&[FIELD_EXPIRES_AT]).expire_after_seconds(0),
        )
        .await
        .map_err(|e| Error::Tool(format!("failed to create sandbox lock TTL index: {e}")))
    }

    /// Acquires the lock for `key`, returning a token-scoped lease.
    ///
    /// Polls until acquired or `lock_timeout` elapses. Mirrors Java
    /// `MongoSandboxExecutionGuard.tryEnter`.
    pub async fn try_enter(&self, key: &SandboxIsolationKey) -> Result<MongodbLeaseHandle> {
        let lock_id = compose_lock_id(key);
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            let now = chrono::Utc::now().timestamp_millis();
            let expires_at = now + self.lease_ttl.as_millis() as i64;
            let token = uuid::Uuid::new_v4().to_string();

            // Step 1: atomic insert.
            let mut doc = Map::new();
            doc.insert(FIELD_LOCK_ID.to_string(), json!(lock_id));
            doc.insert(FIELD_OWNER.to_string(), json!(self.owner));
            doc.insert(FIELD_TOKEN.to_string(), json!(token));
            doc.insert(FIELD_EXPIRES_AT.to_string(), json!(expires_at));
            match self.client.insert_one(&self.collection_key, doc).await {
                Ok(()) => {
                    return Ok(self.spawn_lease(lock_id, token, expires_at));
                }
                Err(MongoError::DuplicateKey) => {}
                Err(e) => {
                    return Err(Error::Tool(format!(
                        "Failed to acquire MongoDB lock '{lock_id}': {e}"
                    )))
                }
            }

            // Step 2: reclaim only if expired.
            let filter =
                FindFilter::eq(FIELD_LOCK_ID, json!(lock_id)).lte(FIELD_EXPIRES_AT, json!(now));
            let update = Update::new()
                .set(FIELD_OWNER, json!(self.owner))
                .set(FIELD_TOKEN, json!(token))
                .set(FIELD_EXPIRES_AT, json!(expires_at));
            match self
                .client
                .find_one_and_update(
                    &self.collection_key,
                    &filter,
                    &update,
                    &FindOneAndUpdateOptions {
                        upsert: false,
                        return_after: true,
                    },
                )
                .await
            {
                Ok(Some(_)) => return Ok(self.spawn_lease(lock_id, token, expires_at)),
                Ok(None) => {}
                Err(e) => {
                    return Err(Error::Tool(format!(
                        "Failed to reclaim MongoDB lock '{lock_id}': {e}"
                    )))
                }
            }

            if Instant::now() >= deadline {
                return Err(Error::Tool(format!(
                    "Timed out waiting for MongoDB lock: {lock_id}"
                )));
            }
            sleep(self.retry_interval).await;
        }
    }

    fn spawn_lease(&self, lock_id: String, token: String, expires_at: i64) -> MongodbLeaseHandle {
        let interval = Duration::from_millis((self.lease_ttl.as_millis() as u64 / 3).max(1));
        let client = self.client.clone();
        let collection_key = self.collection_key.clone();
        let lease_ttl_ms = self.lease_ttl.as_millis() as i64;
        let renew_id = lock_id.clone();
        let renew_token = token.clone();
        let renew_interval = interval;
        let lock_lost = Arc::new(AtomicBool::new(false));
        let renew_lost = lock_lost.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(renew_interval);
            // Skip the immediate first tick; renew after one interval.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let new_expires = chrono::Utc::now().timestamp_millis() + lease_ttl_ms;
                let filter = FindFilter::eq(FIELD_LOCK_ID, json!(renew_id.clone()))
                    .and(FindFilter::eq(FIELD_TOKEN, json!(renew_token.clone())));
                let update = Update::new().set(FIELD_EXPIRES_AT, json!(new_expires));
                // `find_one_and_update` (instead of `update_one`) reports whether a
                // document still matched: `None` means the lock document is gone or its
                // token was overwritten (expired and reclaimed by another owner), i.e. the
                // lease lost the lock while the call kept running (MONGO-06).
                match client
                    .find_one_and_update(
                        &collection_key,
                        &filter,
                        &update,
                        &FindOneAndUpdateOptions {
                            upsert: false,
                            return_after: true,
                        },
                    )
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        // Nothing left to renew: flip the flag so the host can react and
                        // stop the ticker instead of pushing writes that match nothing.
                        renew_lost.store(true, Ordering::SeqCst);
                        tracing::warn!(
                            "[sandbox-guard] MongoDB lock {renew_id} lost during execution \
                             (expired or reclaimed); continuing without the lock"
                        );
                        break;
                    }
                    Err(e) => {
                        // Never let an error escape; TTL expiry remains a safety valve.
                        tracing::warn!("[sandbox-guard] failed to renew MongoDB lock {renew_id}: {e}");
                    }
                }
            }
        });
        MongodbLeaseHandle {
            client: self.client.clone(),
            collection_key: self.collection_key.clone(),
            lock_id,
            token,
            expires_at,
            lock_lost,
            renewal: Some(handle),
        }
    }

    fn lock_held(&self, lock_id: &str, now: i64) -> bool {
        // `is_held` is synchronous in the harness trait; consult the backend with a single
        // (non-polling) read. This probe is diagnostics-only — acquire/release are async.
        let filter = FindFilter::eq(FIELD_LOCK_ID, json!(lock_id));
        match futures::executor::block_on(self.client.find_one(&self.collection_key, &filter)) {
            Ok(Some(doc)) => doc
                .get(FIELD_EXPIRES_AT)
                .and_then(|v| v.as_i64())
                .map(|e| e > now)
                .unwrap_or(false),
            _ => false,
        }
    }
}

/// A lease handle returned by [`MongodbSandboxExecutionGuard::try_enter`].
///
/// Dropping it cancels the renewal task and releases the lock (token-scoped).
pub struct MongodbLeaseHandle {
    client: Arc<dyn MongoClient>,
    /// Database-scoped collection key of the lock document.
    collection_key: String,
    lock_id: String,
    token: String,
    expires_at: i64,
    /// Set by the renewal task when the lock document disappeared (expired and
    /// reclaimed, or its token was overwritten).
    lock_lost: Arc<AtomicBool>,
    renewal: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for MongodbLeaseHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbLeaseHandle")
            .field("lock_id", &self.lock_id)
            .field("token", &self.token)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl MongodbLeaseHandle {
    /// The lock document id.
    pub fn lock_id(&self) -> &str {
        &self.lock_id
    }

    /// The random token scoping this lease.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The initial expiry timestamp (millis since epoch).
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }

    /// True once renewal found the lock document gone (or owned by another token), i.e.
    /// the lease is no longer guarding the isolation key.
    ///
    /// Mirrors Java `MongoLease.renew` warning on `matchedCount == 0`; the flag lets the
    /// host observe the loss (Java only logs). The renewal ticker stops in that case —
    /// there is nothing left to renew — and the caller must treat the sandbox as unlocked.
    pub fn is_lock_lost(&self) -> bool {
        self.lock_lost.load(Ordering::SeqCst)
    }

    /// Releases the lock, deleting the document only when the token still matches.
    ///
    /// Async on purpose: the delete goes through the MongoDB client seam, so it must be
    /// awaited instead of bridged with `futures::executor::block_on` (which stalls a
    /// runtime worker and deadlocks a current-thread runtime).
    pub async fn release_async(&mut self) {
        self.stop_renewal();
        let Some((client, collection_key, lock_id, token)) = self.release_parts() else {
            return;
        };
        let filter = FindFilter::eq(FIELD_LOCK_ID, json!(lock_id))
            .and(FindFilter::eq(FIELD_TOKEN, json!(token)));
        if let Err(e) = client.delete_one(&collection_key, &filter).await {
            tracing::warn!("[sandbox-guard] failed to release MongoDB lock {lock_id}: {e}");
        }
    }

    fn stop_renewal(&mut self) {
        if let Some(handle) = self.renewal.take() {
            handle.abort();
        }
    }

    /// Takes the connection details out of the lease so the release runs exactly once
    /// (the fields are emptied, so a later `Drop` finds nothing to do).
    fn release_parts(&mut self) -> Option<(Arc<dyn MongoClient>, String, String, String)> {
        let lock_id = std::mem::take(&mut self.lock_id);
        if lock_id.is_empty() {
            return None;
        }
        let token = std::mem::take(&mut self.token);
        let collection_key = std::mem::take(&mut self.collection_key);
        Some((self.client.clone(), collection_key, lock_id, token))
    }
}

impl Drop for MongodbLeaseHandle {
    fn drop(&mut self) {
        self.stop_renewal();
        let Some((client, collection_key, lock_id, token)) = self.release_parts() else {
            return;
        };
        let filter = FindFilter::eq(FIELD_LOCK_ID, json!(lock_id.clone()))
            .and(FindFilter::eq(FIELD_TOKEN, json!(token)));
        // `Drop` cannot await: schedule the token-scoped delete on the ambient runtime. If
        // there is none, the lease TTL (and its TTL index) is the safety valve — never block
        // the current thread on a driver future.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = client.delete_one(&collection_key, &filter).await;
                });
            }
            Err(_) => tracing::warn!(
                "[sandbox-guard] dropping MongoDB lease {lock_id} outside a Tokio runtime; \
                 the lock will expire with its TTL"
            ),
        }
    }
}

/// Hash of an isolation key → lock document id.
pub fn compose_lock_id(key: &SandboxIsolationKey) -> String {
    let raw = format!("{}:{}", key.scope.to_lowercase(), key.value);
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("lock:{hex}")
}

/// Builder for [`MongodbSandboxExecutionGuard`].
pub struct MongodbSandboxExecutionGuardBuilder {
    client: Arc<dyn MongoClient>,
    database_name: String,
    collection: String,
    lock_timeout: Duration,
    lease_ttl: Duration,
    retry_interval: Duration,
    owner: String,
}

impl MongodbSandboxExecutionGuardBuilder {
    fn new(client: Arc<dyn MongoClient>) -> Self {
        Self {
            client,
            database_name: DEFAULT_DATABASE.to_string(),
            collection: SANDBOX_LOCKS_COLLECTION.to_string(),
            lock_timeout: Duration::from_secs(30 * 60),
            lease_ttl: Duration::from_secs(30 * 60),
            retry_interval: Duration::from_millis(500),
            owner: default_owner(),
        }
    }

    pub fn database_name(mut self, name: impl Into<String>) -> Self {
        self.database_name = name.into();
        self
    }

    pub fn collection(mut self, name: impl Into<String>) -> Self {
        self.collection = name.into();
        self
    }

    /// Sets the maximum time to wait for acquisition before timing out.
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    /// Sets the lease TTL written on acquisition (renewed every `ttl / 3`).
    pub fn lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl = ttl;
        self
    }

    /// Sets the polling interval between acquisition attempts.
    pub fn retry_interval(mut self, interval: Duration) -> Self {
        self.retry_interval = interval;
        self
    }

    /// Sets a human-readable owner label for diagnostics.
    pub fn owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    pub fn build(self) -> MongodbSandboxExecutionGuard {
        // Scope the lock collection to the configured database, mirroring Java
        // `mongoClient.getDatabase(databaseName).getCollection(collectionName)`.
        // The in-process backend is flat, so the database is encoded into the key
        // via `scoped_collection` (the convention `MongodbRemoteSnapshotClient`
        // already uses); guards on different databases therefore never share a lock.
        let collection_key =
            crate::client::scoped_collection(&self.database_name, &self.collection);
        MongodbSandboxExecutionGuard {
            client: self.client,
            database_name: self.database_name,
            collection_key,
            lock_timeout: self.lock_timeout,
            lease_ttl: self.lease_ttl,
            retry_interval: self.retry_interval,
            owner: self.owner,
        }
    }
}

fn default_owner() -> String {
    format!("agentscope:{}", std::process::id())
}

/// A per-isolation-key view of a [`MongodbSandboxExecutionGuard`].
///
/// The harness `SandboxExecutionGuard` is a concrete in-process struct, but the harness also
/// exposes the pluggable `SandboxGuard` trait. This adapter mirrors the harness guard's
/// method surface (`try_enter` / `release` / `is_held` / `active_count`) over the
/// distributed MongoDB lock **and** implements `SandboxGuard`, so it can be handed to
/// `DistributedStore::sandbox_execution_guard(...)` or used wherever the local guard would.
#[derive(Clone)]
pub struct MongodbGuardClient {
    inner: Arc<MongodbSandboxExecutionGuard>,
    leases: Arc<
        std::sync::Mutex<std::collections::HashMap<SandboxIsolationKey, Vec<MongodbLeaseHandle>>>,
    >,
}

impl std::fmt::Debug for MongodbGuardClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbGuardClient").finish()
    }
}

impl MongodbGuardClient {
    pub fn new(inner: Arc<MongodbSandboxExecutionGuard>) -> Self {
        Self {
            inner,
            leases: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Tries to acquire a lease for the given isolation key.
    ///
    /// Awaits the distributed lock acquisition (which polls with `tokio::time::sleep`).
    /// **Fail-closed** (MONGO-04): a failed acquisition returns `Err` so the caller aborts;
    /// it used to degrade to [`SandboxLease::noop`], which dropped cross-process exclusion
    /// without telling anyone.
    pub async fn try_enter(
        &self,
        key: SandboxIsolationKey,
    ) -> agentscope_core::error::Result<SandboxLease> {
        match self.inner.try_enter(&key).await {
            Ok(lease) => {
                if let Ok(mut leases) = self.leases.lock() {
                    leases.entry(key.clone()).or_default().push(lease);
                }
                Ok(SandboxLease::new(key))
            }
            Err(e) => {
                tracing::error!(error = %e, "[sandbox-guard] failed to acquire MongoDB lock");
                Err(agentscope_core::error::Error::Other(format!(
                    "MongoDB sandbox guard lock not acquired: {e}"
                )))
            }
        }
    }

    /// Releases a lease for the given isolation key.
    pub async fn release(&self, key: &SandboxIsolationKey) {
        let dropped = self
            .leases
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
        if let Some(mut lease) = dropped {
            // Token-scoped delete, awaited on this task (no blocking bridge).
            lease.release_async().await;
        }
    }

    /// Returns true if the given key's lock is currently held (and unexpired).
    pub fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        let lock_id = compose_lock_id(key);
        let now = chrono::Utc::now().timestamp_millis();
        self.inner.lock_held(&lock_id, now)
    }

    /// Returns the number of active leases (test helper).
    pub fn active_count(&self) -> usize {
        self.leases
            .lock()
            .map(|g| g.values().map(|v| v.len()).sum())
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl SandboxGuard for MongodbGuardClient {
    async fn try_enter(
        &self,
        key: SandboxIsolationKey,
    ) -> agentscope_core::error::Result<SandboxLease> {
        MongodbGuardClient::try_enter(self, key).await
    }

    async fn release(&self, key: &SandboxIsolationKey) {
        MongodbGuardClient::release(self, key).await
    }

    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        MongodbGuardClient::is_held(self, key)
    }

    fn active_count(&self) -> usize {
        MongodbGuardClient::active_count(self)
    }

    /// MongoDB lock documents serialise across processes.
    fn is_distributed(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::InMemoryMongoClient;

    fn guard() -> MongodbSandboxExecutionGuard {
        MongodbSandboxExecutionGuard::builder(Arc::new(InMemoryMongoClient::new()))
            .lease_ttl(Duration::from_secs(60))
            .retry_interval(Duration::from_millis(10))
            .lock_timeout(Duration::from_millis(200))
            .build()
    }

    #[test]
    fn lock_id_is_stable_and_scope_sensitive() {
        let a = compose_lock_id(&SandboxIsolationKey::new("user", "u1"));
        let b = compose_lock_id(&SandboxIsolationKey::new("user", "u1"));
        let c = compose_lock_id(&SandboxIsolationKey::new("session", "u1"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("lock:"));
    }

    #[tokio::test]
    async fn acquire_and_release_roundtrip() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let mut lease = g.try_enter(&key).await.unwrap();
        assert!(g.lock_held(
            &lease.lock_id().to_string(),
            chrono::Utc::now().timestamp_millis()
        ));
        lease.release_async().await;
        assert!(!g.lock_held(
            &lease.lock_id().to_string(),
            chrono::Utc::now().timestamp_millis()
        ));
    }

    #[tokio::test]
    async fn second_acquire_times_out_while_held() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let _held = g.try_enter(&key).await.unwrap();
        let err = g.try_enter(&key).await.unwrap_err();
        assert!(err.to_string().contains("Timed out"));
    }

    /// Regression test for the builder `database_name` being dropped on `build()`:
    /// two guards configured with different databases must not contend for the same
    /// isolation key (Java scopes the lock collection to
    /// `mongoClient.getDatabase(databaseName)`).
    #[tokio::test]
    async fn database_name_partitions_locks() {
        let client: Arc<dyn MongoClient> = Arc::new(InMemoryMongoClient::new());
        let build = |database: &str| {
            MongodbSandboxExecutionGuard::builder(client.clone())
                .database_name(database)
                .lease_ttl(Duration::from_secs(60))
                .retry_interval(Duration::from_millis(10))
                .lock_timeout(Duration::from_millis(100))
                .build()
        };
        let guard_a = build("db_a");
        let guard_b = build("db_b");

        assert_eq!(guard_a.database_name(), "db_a");
        assert_eq!(guard_b.database_name(), "db_b");
        assert_eq!(
            guard_a.collection_key(),
            crate::client::scoped_collection("db_a", SANDBOX_LOCKS_COLLECTION)
        );
        assert_ne!(guard_a.collection_key(), guard_b.collection_key());

        let key = SandboxIsolationKey::new("user", "u1");
        let lease_a = guard_a.try_enter(&key).await.unwrap();
        let lease_b = guard_b.try_enter(&key).await.unwrap();

        let now = chrono::Utc::now().timestamp_millis();
        assert!(guard_a.lock_held(lease_a.lock_id(), now));
        assert!(guard_b.lock_held(lease_b.lock_id(), now));
        assert_ne!(lease_a.token(), lease_b.token());

        // Same database still contends: the second acquisition times out.
        let guard_a2 = build("db_a");
        assert!(guard_a2.try_enter(&key).await.is_err());
    }

    #[test]
    fn default_database_is_scoped_into_collection_key() {
        let g = guard();
        assert_eq!(g.database_name(), DEFAULT_DATABASE);
        assert_eq!(
            g.collection_key(),
            crate::client::scoped_collection(DEFAULT_DATABASE, SANDBOX_LOCKS_COLLECTION)
        );
    }

    #[tokio::test]
    async fn expired_lock_can_be_reclaimed() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let lock_id = compose_lock_id(&key);
        // Seed an already-expired lock document.
        let mut doc = serde_json::Map::new();
        doc.insert("_id".to_string(), json!(lock_id));
        doc.insert("owner".to_string(), json!("dead"));
        doc.insert("token".to_string(), json!("dead-token"));
        doc.insert("expiresAt".to_string(), json!(0));
        g.client.insert_one(g.collection_key(), doc).await.unwrap();
        // Reclaim succeeds despite the existing document.
        let lease = g.try_enter(&key).await.unwrap();
        assert_ne!(lease.token(), "dead-token");
    }

    #[tokio::test]
    async fn release_is_token_scoped() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let lock_id = compose_lock_id(&key);
        let mut lease = g.try_enter(&key).await.unwrap();
        // Tamper: another owner overwrote the token.
        let filter = FindFilter::eq("_id", json!(lock_id.clone()));
        let update = Update::new().set("token", json!("someone-else"));
        g.client
            .update_one(g.collection_key(), &filter, &update, &Default::default())
            .await
            .unwrap();
        lease.release_async().await;
        // The lock document must survive because the token no longer matches.
        assert!(g
            .client
            .find_one(g.collection_key(), &filter)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn adapter_mirrors_harness_guard_surface() {
        let client = MongodbGuardClient::new(Arc::new(guard()));
        let key = SandboxIsolationKey::new("user", "u1");
        let lease = client.try_enter(key.clone()).await.expect("lease");
        assert!(!lease.is_noop());
        assert_eq!(client.active_count(), 1);
        assert!(client.is_held(&key));
        client.release(&key).await;
        assert_eq!(client.active_count(), 0);
        assert!(!client.is_held(&key));

        // A MongoDB lock document serialises across processes.
        assert!(SandboxGuard::is_distributed(&client));
    }

    /// The harness trait methods are genuinely async: awaiting a trait acquisition and
    /// release inside a timeout must complete (no `futures::executor::block_on` bridge).
    #[tokio::test]
    async fn trait_acquisition_and_release_are_awaitable() {
        let client = MongodbGuardClient::new(Arc::new(guard()));
        let key = SandboxIsolationKey::new("session", "awaitable");

        let lease = tokio::time::timeout(
            Duration::from_millis(500),
            SandboxGuard::try_enter(&client, key.clone()),
        )
        .await
        .expect("trait acquisition must not deadlock the runtime")
        .expect("trait acquisition succeeds");
        assert!(!lease.is_noop());

        tokio::time::timeout(
            Duration::from_millis(500),
            SandboxGuard::release(&client, &key),
        )
        .await
        .expect("trait release must not deadlock the runtime");
        assert!(!client.is_held(&key));
        assert_eq!(client.active_count(), 0);
    }

    /// Java `lockDocumentExpiryUsesLeaseTtlNotAcquisitionTimeout` + `builderWithCustomOwner`:
    /// the lock document records the owner label, the token, and an `expiresAt` derived from
    /// `lease_ttl` (not from `lock_timeout`).
    #[tokio::test]
    async fn lock_document_records_owner_and_lease_ttl() {
        let g = MongodbSandboxExecutionGuard::builder(Arc::new(InMemoryMongoClient::new()))
            .lease_ttl(Duration::from_secs(120))
            .lock_timeout(Duration::from_secs(5))
            .owner("owner-1")
            .build();
        let key = SandboxIsolationKey::new("user", "u1");
        let before = chrono::Utc::now().timestamp_millis();
        let lease = g.try_enter(&key).await.unwrap();

        let filter = FindFilter::eq("_id", json!(lease.lock_id()));
        let doc = g
            .client
            .find_one(g.collection_key(), &filter)
            .await
            .unwrap()
            .expect("lock document exists");
        assert_eq!(doc.get("owner").and_then(|v| v.as_str()), Some("owner-1"));
        assert_eq!(
            doc.get("token").and_then(|v| v.as_str()),
            Some(lease.token())
        );
        let expires_at = doc
            .get("expiresAt")
            .and_then(|v| v.as_i64())
            .expect("expiresAt written on acquisition");
        let expected = before + 120_000;
        assert!(
            (expires_at - expected).abs() <= 5_000,
            "expiresAt must derive from lease_ttl (got {expires_at}, expected ~{expected})"
        );
    }

    /// The renewal task (every `lease_ttl / 3`) keeps pushing `expiresAt` forward, so a
    /// long-running sandbox never loses its lock to the TTL index.
    #[tokio::test]
    async fn lease_renewal_extends_the_lock_expiry() {
        let g = MongodbSandboxExecutionGuard::builder(Arc::new(InMemoryMongoClient::new()))
            .lease_ttl(Duration::from_millis(300))
            .retry_interval(Duration::from_millis(10))
            .lock_timeout(Duration::from_millis(100))
            .build();
        let key = SandboxIsolationKey::new("session", "renewed");
        let lease = g.try_enter(&key).await.unwrap();
        let initial_expiry = lease.expires_at();
        let filter = FindFilter::eq("_id", json!(lease.lock_id()));

        tokio::time::sleep(Duration::from_millis(350)).await;

        let doc = g
            .client
            .find_one(g.collection_key(), &filter)
            .await
            .unwrap()
            .expect("lock document still present");
        let renewed_expiry = doc
            .get("expiresAt")
            .and_then(|v| v.as_i64())
            .expect("expiresAt is written on acquisition");
        assert!(
            renewed_expiry > initial_expiry,
            "renewal must extend expiresAt ({renewed_expiry} > {initial_expiry})"
        );
        // The lease still owns the lock after several renewal cycles.
        assert!(g.lock_held(lease.lock_id(), chrono::Utc::now().timestamp_millis()));
    }

    /// End-to-end lifecycle mirroring `SandboxManager::release_lease`: the manager claims
    /// the lease, then awaits `SandboxGuard::release`, which deletes the lock document.
    /// A second release is a no-op (the harness `mark_released` gate makes it exactly once).
    #[tokio::test]
    async fn harness_lifecycle_releases_the_lock_exactly_once() {
        let client = MongodbGuardClient::new(Arc::new(guard()));
        let key = SandboxIsolationKey::new("session", "lifecycle");

        let lease = client.try_enter(key.clone()).await.expect("lease");
        assert!(!lease.is_noop());
        assert!(client.is_held(&key));

        assert!(lease.mark_released(), "the manager claims the lease first");
        client.release(&key).await;
        assert!(!client.is_held(&key), "the lock document is deleted");
        assert_eq!(client.active_count(), 0);

        assert!(!lease.mark_released(), "release is exactly once");
        client.release(&key).await;
        assert!(!client.is_held(&key));
        assert_eq!(client.active_count(), 0);
    }

    /// MONGO-04: the adapter is **fail-closed**. A lock that cannot be taken surfaces as
    /// `Err`; it must never degrade to a no-op lease (which would drop cross-process
    /// mutual exclusion silently, the exact bug the contract exists to prevent).
    #[tokio::test]
    async fn guard_client_fails_closed_when_the_lock_is_held() {
        let inner = Arc::new(guard());
        let holder = MongodbGuardClient::new(inner.clone());
        let loser = MongodbGuardClient::new(inner.clone());
        let key = SandboxIsolationKey::new("user", "u1");

        let held = holder.try_enter(key.clone()).await.expect("first lease");
        assert!(!held.is_noop());

        let error = SandboxGuard::try_enter(&loser, key.clone())
            .await
            .expect_err("a held lock must not yield a lease");
        assert!(
            format!("{error}").contains("MongoDB sandbox guard lock not acquired"),
            "{error}"
        );
        assert_eq!(loser.active_count(), 0, "no lease is recorded on failure");
        assert!(holder.is_held(&key), "the holder keeps the lock");

        holder.release(&key).await;
        assert!(!holder.is_held(&key));
    }

    /// MONGO-04 through the real assembly path: `SandboxManager::acquire` propagates the
    /// guard error with `?`, so no sandbox is created while the distributed lock is held
    /// elsewhere. Before the fix the guard returned a no-op lease, the manager continued
    /// and the sandbox client was reached.
    #[tokio::test]
    async fn harness_manager_aborts_when_the_mongo_lock_is_unavailable() {
        use agentscope_harness::agent::isolation_scope::IsolationScope;
        use agentscope_harness::agent::sandbox::{
            InMemorySandboxStateStore, SandboxContext, SandboxManager,
        };

        let inner = Arc::new(guard());
        let holder = MongodbGuardClient::new(inner.clone());
        let key = SandboxIsolationKey::new("agent", "mongo-agent");
        let held = holder.try_enter(key.clone()).await.expect("holder lease");

        let manager = SandboxManager::with_shared_guard(
            Arc::new(UnreachableSandboxClient),
            Arc::new(InMemorySandboxStateStore::new()),
            "mongo-agent",
            Arc::new(MongodbGuardClient::new(inner.clone())),
        );
        let context = SandboxContext::builder()
            .isolation_scope(IsolationScope::Agent)
            .build();

        let error = manager
            .acquire(&context, None)
            .await
            .expect_err("acquire must abort while the lock is held");
        assert!(
            format!("{error}").contains("MongoDB sandbox guard lock not acquired"),
            "the guard error must be the one that aborts the acquisition: {error}"
        );

        drop(held);
        holder.release(&key).await;
    }

    /// A sandbox backend that must never be reached: any call reports it explicitly, so a
    /// test can tell "the guard aborted the acquisition" from "the backend was used".
    struct UnreachableSandboxClient;

    #[async_trait::async_trait]
    impl agentscope_harness::agent::sandbox::SandboxClient for UnreachableSandboxClient {
        async fn create(
            &self,
            _workspace_spec: &agentscope_harness::agent::sandbox::WorkspaceSpec,
            _snapshot_spec: Option<&dyn agentscope_harness::agent::sandbox::SandboxSnapshotSpec>,
            _options: Option<&agentscope_harness::agent::sandbox::SandboxClientOptions>,
        ) -> Result<Box<dyn agentscope_harness::agent::sandbox::Sandbox>> {
            Err(Error::Other(
                "sandbox client must not be reached without the lock".into(),
            ))
        }

        async fn resume(
            &self,
            _state: &agentscope_harness::agent::sandbox::SandboxState,
        ) -> Result<Box<dyn agentscope_harness::agent::sandbox::Sandbox>> {
            Err(Error::Other(
                "sandbox client must not be reached without the lock".into(),
            ))
        }

        async fn delete(
            &self,
            _sandbox: &dyn agentscope_harness::agent::sandbox::Sandbox,
        ) -> Result<()> {
            Err(Error::Other(
                "sandbox client must not be reached without the lock".into(),
            ))
        }

        fn serialize_state(
            &self,
            state: &agentscope_harness::agent::sandbox::SandboxState,
        ) -> Result<String> {
            serde_json::to_string(state).map_err(|e| Error::Other(e.to_string()))
        }

        fn deserialize_state(
            &self,
            json: &str,
        ) -> Result<agentscope_harness::agent::sandbox::SandboxState> {
            serde_json::from_str(json).map_err(|e| Error::Other(e.to_string()))
        }
    }

    /// MONGO-06: renewal notices when the lock document is gone / stolen and stops pushing
    /// `expiresAt`, flagging the lease instead of renewing forever in silence.
    #[tokio::test]
    async fn renewal_detects_a_lost_lock() {
        let g = MongodbSandboxExecutionGuard::builder(Arc::new(InMemoryMongoClient::new()))
            .lease_ttl(Duration::from_millis(300))
            .retry_interval(Duration::from_millis(10))
            .lock_timeout(Duration::from_millis(100))
            .build();
        let key = SandboxIsolationKey::new("session", "stolen");
        let lease = g.try_enter(&key).await.unwrap();
        assert!(!lease.is_lock_lost());

        // Another owner reclaims the lock: same `_id`, different token.
        let filter = FindFilter::eq("_id", json!(lease.lock_id()));
        let update = Update::new().set("token", json!("other-owner"));
        g.client
            .update_one(g.collection_key(), &filter, &update, &Default::default())
            .await
            .unwrap();

        // One renewal cycle (lease_ttl / 3) is enough to observe the loss.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            lease.is_lock_lost(),
            "renewal must flag the lease once its token no longer matches"
        );
        // The ticker stopped, so the stolen document keeps the other owner's token.
        let doc = g
            .client
            .find_one(g.collection_key(), &filter)
            .await
            .unwrap()
            .expect("lock document still exists");
        assert_eq!(
            doc.get("token").and_then(|v| v.as_str()),
            Some("other-owner")
        );
        // The ticker stopped, so `expiresAt` was not pushed forward by our renewal.
        assert_eq!(
            doc.get("expiresAt").and_then(|v| v.as_i64()),
            Some(lease.expires_at()),
            "a lost lease must stop renewing"
        );
    }
}
