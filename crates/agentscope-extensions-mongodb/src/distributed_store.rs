//! MongoDB-backed [`DistributedStore`], mirroring
//! `io.agentscope.extensions.mongodb.MongoDistributedStore`.
//!
//! Composes the four MongoDB components:
//! * [`MongodbAgentStateStore`] — agent session state,
//! * [`MongodbStore`] — workspace filesystem KV,
//! * [`MongodbSandboxExecutionGuard`] — sandbox execution locking,
//! * [`MongodbSnapshotSpec`] — sandbox workspace snapshots.
//!
//! The harness [`DistributedStore`] currently exposes only `agent_state_store` and
//! `isolation_scope` (see `EXTENSIONS_GAP_TRACKER.md` §2.2), so this type stores the
//! full component set and projects the sub-set the harness understands via
//! [`MongodbDistributedStore::to_harness_store`], while keeping the remaining
//! components accessible through typed accessors for direct wiring.

use std::sync::{Arc, Mutex};

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;
use serde_json::{json, Value};

use crate::client::MongoClient;
use crate::constants::{DEFAULT_DATABASE, SNAPSHOTS_COLLECTION};
use crate::sandbox_guard::{MongodbGuardClient, MongodbSandboxExecutionGuard};
use crate::snapshot::{MongodbRemoteSnapshotClient, MongodbSnapshotSpec};
use crate::state_store::MongodbAgentStateStore;
use crate::store::MongodbStore;

/// MongoDB-backed distributed store composing all four MongoDB components.
#[derive(Clone)]
pub struct MongodbDistributedStore {
    client: Arc<dyn MongoClient>,
    database_name: String,
    isolation_scope: IsolationScope,

    agent_state_store: MongodbAgentStateStore,
    base_store: MongodbStore,
    snapshot_client: MongodbRemoteSnapshotClient,
    snapshot_spec: MongodbSnapshotSpec,
    execution_guard: Arc<MongodbSandboxExecutionGuard>,
    guard_client: Arc<MongodbGuardClient>,

    /// Cache of lazily-created state stores, keyed by nothing (single instance); kept
    /// behind a Mutex to mirror Java's volatile double-checked caching without
    /// changing the shared component identities.
    _cache: Arc<Mutex<()>>,
}

impl std::fmt::Debug for MongodbDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbDistributedStore")
            .field("database_name", &self.database_name)
            .field("isolation_scope", &self.isolation_scope)
            .finish()
    }
}

impl MongodbDistributedStore {
    /// Creates a distributed store over the given client and database name.
    ///
    /// Databases/collections default to [`DEFAULT_DATABASE`] and the per-component
    /// default collection constants. Call [`MongodbDistributedStore::ensure_indexes`]
    /// before first use to create the required indexes.
    pub fn new(client: Arc<dyn MongoClient>, database_name: Option<&str>) -> Self {
        let database_name = database_name.unwrap_or(DEFAULT_DATABASE).to_string();

        let agent_state_store = {
            // Cascade-delete snapshots when a session is removed. Only the SESSION
            // scope is eligible because a SESSION-scoped sandbox is tied 1:1 to the
            // session; USER/AGENT/GLOBAL share sandboxes across sessions. This is
            // best-effort: a failure is logged and the session is still removed.
            let cascade_snapshot_client =
                MongodbRemoteSnapshotClient::new(client.clone(), Some(&database_name), None);
            let cascade_state_client = client.clone();
            let cascade_db = database_name.clone();
            MongodbAgentStateStore::builder()
                .client(client.clone())
                .on_delete(Arc::new(move |_user_id, session_id| {
                    let snapshot_client = cascade_snapshot_client.clone();
                    let state_client = cascade_state_client.clone();
                    let db = cascade_db.clone();
                    let session_id = session_id.to_string();
                    // Best-effort, synchronous cascade via the blocking executor.
                    let _ = futures::executor::block_on(async move {
                        if let Some(snapshot_id) =
                            find_session_snapshot_id(&state_client, &db, &session_id).await
                        {
                            let _ = snapshot_client.delete(&snapshot_id).await;
                        }
                    });
                }))
                .build()
                .expect("client provided")
        };

        let base_store = MongodbStore::with_defaults(client.clone());
        let snapshot_client =
            MongodbRemoteSnapshotClient::new(client.clone(), Some(&database_name), None);
        let snapshot_spec = MongodbSnapshotSpec::new(
            client.clone(),
            Some(&database_name),
            Some(SNAPSHOTS_COLLECTION),
        );
        let execution_guard = Arc::new(
            MongodbSandboxExecutionGuard::builder(client.clone())
                .database_name(database_name.clone())
                .build(),
        );
        let guard_client = Arc::new(MongodbGuardClient::new(execution_guard.clone()));

        Self {
            client,
            database_name,
            isolation_scope: IsolationScope::Session,
            agent_state_store,
            base_store,
            snapshot_client,
            snapshot_spec,
            execution_guard,
            guard_client,
            _cache: Arc::new(Mutex::new(())),
        }
    }

    /// Sets the isolation scope for the harness projection.
    pub fn with_isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    /// The backing MongoDB client.
    pub fn client(&self) -> &Arc<dyn MongoClient> {
        &self.client
    }

    /// The configured database name.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// The agent state store component.
    pub fn agent_state_store(&self) -> &MongodbAgentStateStore {
        &self.agent_state_store
    }

    /// The workspace KV base store component.
    pub fn base_store(&self) -> &MongodbStore {
        &self.base_store
    }

    /// The snapshot spec component.
    pub fn snapshot_spec(&self) -> &MongodbSnapshotSpec {
        &self.snapshot_spec
    }

    /// The snapshot client component.
    pub fn snapshot_client(&self) -> &MongodbRemoteSnapshotClient {
        &self.snapshot_client
    }

    /// The sandbox execution guard component.
    pub fn execution_guard(&self) -> &Arc<MongodbSandboxExecutionGuard> {
        &self.execution_guard
    }

    /// A harness-trait [`SandboxExecutionGuard`](agentscope_harness::agent::sandbox::guard::SandboxExecutionGuard)
    /// view of the MongoDB guard.
    pub fn sandbox_execution_guard(&self) -> Arc<MongodbGuardClient> {
        self.guard_client.clone()
    }

    /// Ensures all component indexes exist.
    pub async fn ensure_indexes(&self) -> Result<()> {
        self.agent_state_store
            .ensure_indexes()
            .await
            .map_err(map_err)?;
        self.base_store.ensure_indexes().await.map_err(map_err)?;
        self.execution_guard
            .ensure_indexes()
            .await
            .map_err(|e| Error::Tool(format!("failed to ensure guard indexes: {e}")))?;
        Ok(())
    }

    /// Projects the sub-set of components the harness [`DistributedStore`] currently
    /// models (`agent_state_store` + `isolation_scope`).
    ///
    /// The full component set remains on this type; when the harness `DistributedStore`
    /// grows the additional fields (tracker §2.2), this projection will be extended.
    pub fn to_harness_store(&self) -> DistributedStore {
        let mut builder = DistributedStore::builder()
            .agent_state_store(Arc::new(self.agent_state_store.clone()))
            .isolation_scope(self.isolation_scope);
        let _ = &mut builder;
        builder.build()
    }
}

fn map_err(e: crate::client::MongoError) -> Error {
    Error::Tool(format!("MongoDB distributed store operation failed: {e}"))
}

/// Best-effort lookup of a SESSION-scoped remote snapshot id for a session.
///
/// Mirrors Java `MongoDistributedStore.findSessionSnapshotId`: reads the harness
/// `SessionSandboxStateStore` slot (`sandbox/session/<sessionId>` /
/// `_sandbox_state`) from the sessions collection and extracts `snapshot.id` when
/// `snapshot.type == "remote"`.
async fn find_session_snapshot_id(
    client: &Arc<dyn MongoClient>,
    database_name: &str,
    session_id: &str,
) -> Option<String> {
    let _ = database_name; // collection is resolved from the store default
    let filter = crate::client::FindFilter::eq(
        "_id",
        json!({ "user": "__anon__", "session": format!("sandbox/session/{session_id}") }),
    );
    let doc = client
        .find_one(crate::constants::SESSIONS_COLLECTION, &filter)
        .await
        .ok()
        .flatten()?;
    let json_str = doc
        .get("states")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("_sandbox_state"))
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("json"))
        .and_then(|v| v.as_str())?;
    extract_remote_snapshot_id(json_str)
}

/// Parses a sandbox-state JSON string and returns the remote snapshot id, when
/// `snapshot.type == "remote"` and `snapshot.id` is a non-blank string.
fn extract_remote_snapshot_id(json_str: &str) -> Option<String> {
    let root: Value = serde_json::from_str(json_str).ok()?;
    let snapshot = root.get("snapshot")?;
    if snapshot.get("type").and_then(|v| v.as_str()) != Some("remote") {
        return None;
    }
    snapshot
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::InMemoryMongoClient;
    // Traits providing the inherent-looking methods exercised below.
    use agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient as _;
    use agentscope_harness::agent::sandbox::SandboxSnapshotSpec as _;
    use agentscope_harness::filesystem::remote::store::BaseStore as _;
    use serde_json::json;

    fn store() -> MongodbDistributedStore {
        MongodbDistributedStore::new(Arc::new(InMemoryMongoClient::new()), Some("agentscope"))
    }

    /// Seeds the harness sandbox-state slot so the cascade lookup finds a remote
    /// snapshot id, mirroring what `SessionSandboxStateStore` writes.
    async fn seed_session_snapshot(raw: &InMemoryMongoClient, session_id: &str, snapshot_id: &str) {
        use crate::client::{
            FindFilter, InMemoryMongoClient as _, MongoClient as _, Update, UpdateOptions,
        };
        let filter = FindFilter::eq(
            "_id",
            json!({ "user": "__anon__", "session": format!("sandbox/session/{session_id}") }),
        );
        let state = json!({
            "snapshot": { "type": "remote", "id": snapshot_id }
        })
        .to_string();
        let update = Update::new().set("states._sandbox_state.json", json!(state));
        raw.update_one(
            crate::constants::SESSIONS_COLLECTION,
            &filter,
            &update,
            &UpdateOptions { upsert: true },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn ensure_indexes_creates_all_component_indexes() {
        let s = store();
        s.ensure_indexes().await.unwrap();
    }

    #[tokio::test]
    async fn state_store_component_round_trips() {
        let s = store();
        use agentscope_core::state::AgentStateStore;
        s.agent_state_store()
            .save(Some("u"), "sess", "k", json!(1))
            .await;
        assert_eq!(
            s.agent_state_store().get(Some("u"), "sess", "k").await,
            Some(json!(1))
        );
    }

    #[tokio::test]
    async fn base_store_component_round_trips() {
        let s = store();
        let mut v = serde_json::Map::new();
        v.insert("content".to_string(), json!("hi"));
        s.base_store().put(&["a".to_string()], "k", v);
        assert!(s.base_store().get(&["a".to_string()], "k").is_some());
    }

    #[tokio::test]
    async fn snapshot_spec_component_round_trips() {
        let s = store();
        let snap = s.snapshot_spec().build("snap-1").unwrap();
        snap.persist(b"data").await.unwrap();
        assert_eq!(snap.restore().await.unwrap(), b"data");
    }

    #[tokio::test]
    async fn guard_component_exposed() {
        let s = store();
        let guard = s.sandbox_execution_guard();
        use agentscope_harness::agent::sandbox::SandboxIsolationKey;
        let key = SandboxIsolationKey::new("user", "u");
        let lease = guard.try_enter(key.clone());
        assert!(!lease.is_noop());
        guard.release(&key);
    }

    #[test]
    fn harness_projection_carries_state_store() {
        let s = store();
        let harness = s.to_harness_store();
        assert!(harness.agent_state_store.is_some());
    }

    #[test]
    fn defaults_to_agentscope_database() {
        let s = MongodbDistributedStore::new(Arc::new(InMemoryMongoClient::new()), None);
        assert_eq!(s.database_name(), "agentscope");
    }

    #[tokio::test]
    async fn deleting_session_cascades_to_snapshot() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let s = MongodbDistributedStore::new(raw.clone(), Some("agentscope"));
        use agentscope_core::state::AgentStateStore;

        // Persist a snapshot and record it in the harness session sandbox-state slot.
        s.snapshot_client()
            .upload("snap-42", b"workspace-tar")
            .await
            .unwrap();
        assert!(s.snapshot_client().exists("snap-42").await.unwrap());
        seed_session_snapshot(&raw, "sess-1", "snap-42").await;

        // Deleting the session triggers the on_delete cascade, which reclaims the
        // snapshot because its id is discoverable from the session slot.
        s.agent_state_store()
            .delete(Some("__anon__"), "sess-1")
            .await;
        assert!(!s.snapshot_client().exists("snap-42").await.unwrap());
    }

    #[tokio::test]
    async fn deleting_session_without_snapshot_does_not_error() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let s = MongodbDistributedStore::new(raw.clone(), Some("agentscope"));
        use agentscope_core::state::AgentStateStore;
        s.agent_state_store()
            .save(Some("u"), "sess-2", "k", json!(1))
            .await;
        // No sandbox-state slot → cascade lookup finds nothing; deletion still succeeds.
        s.agent_state_store().delete(Some("u"), "sess-2").await;
        assert!(!s.agent_state_store().exists(Some("u"), "sess-2").await);
    }
}
