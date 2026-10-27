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

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::{SandboxIsolationKey, SandboxLease};
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
/// what actually implements the harness [`SandboxExecutionGuard`] trait for a given
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
                if client
                    .update_one(
                        &collection_key,
                        &filter,
                        &update,
                        &crate::client::UpdateOptions::default(),
                    )
                    .await
                    .is_err()
                {
                    // Never let an error escape; TTL expiry remains a safety valve.
                    continue;
                }
            }
        });
        MongodbLeaseHandle {
            client: self.client.clone(),
            collection_key: self.collection_key.clone(),
            lock_id,
            token,
            expires_at,
            renewal: Some(handle),
        }
    }

    fn lock_held(&self, lock_id: &str, now: i64) -> bool {
        // `is_held` is synchronous in the harness trait; consult the in-process
        // backend directly when available, otherwise assume observable state via a
        // blocking read.
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

    /// Releases the lock, deleting the document only when the token still matches.
    pub fn release(&mut self) {
        if let Some(handle) = self.renewal.take() {
            handle.abort();
        }
        let filter = FindFilter::eq(FIELD_LOCK_ID, json!(self.lock_id.clone()))
            .and(FindFilter::eq(FIELD_TOKEN, json!(self.token.clone())));
        let _ = futures::executor::block_on(self.client.delete_one(&self.collection_key, &filter));
    }
}

impl Drop for MongodbLeaseHandle {
    fn drop(&mut self) {
        self.release();
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
/// The harness [`agentscope_harness::agent::sandbox::SandboxExecutionGuard`] is a
/// concrete in-process struct (not a trait), so the MongoDB guard cannot `impl` it.
/// This adapter instead mirrors the harness guard's method surface
/// (`try_enter` / `release` / `is_held` / `active_count`) over the distributed
/// MongoDB lock, so callers can substitute it wherever the local guard would be used.
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
    /// Mirrors the harness guard: on acquisition failure it degrades to a no-op
    /// lease rather than deadlocking the caller.
    pub fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        let guard = self.inner.clone();
        let acquire_key = key.clone();
        let handle =
            futures::executor::block_on(async move { guard.try_enter(&acquire_key).await });
        match handle {
            Ok(lease) => {
                if let Ok(mut leases) = self.leases.lock() {
                    leases.entry(key.clone()).or_default().push(lease);
                }
                SandboxLease::new(key)
            }
            Err(e) => {
                tracing::warn!(error = %e, "[sandbox-guard] failed to acquire MongoDB lock");
                SandboxLease::noop()
            }
        }
    }

    /// Releases a lease for the given isolation key.
    pub fn release(&self, key: &SandboxIsolationKey) {
        if let Ok(mut leases) = self.leases.lock() {
            if let Some(stack) = leases.get_mut(key) {
                if let Some(mut lease) = stack.pop() {
                    lease.release();
                }
                if stack.is_empty() {
                    leases.remove(key);
                }
            }
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
        lease.release();
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
        lease.release();
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
        let lease = client.try_enter(key.clone());
        assert!(!lease.is_noop());
        assert_eq!(client.active_count(), 1);
        assert!(client.is_held(&key));
        client.release(&key);
        assert_eq!(client.active_count(), 0);
        assert!(!client.is_held(&key));
    }
}
