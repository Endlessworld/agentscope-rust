//! MongoDB-backed [`DistributedStore`], mirroring
//! `io.agentscope.extensions.mongodb.MongoDistributedStore`.
//!
//! Composes the four MongoDB components:
//! * [`MongodbAgentStateStore`] — agent session state,
//! * [`MongodbStore`] — workspace filesystem KV,
//! * [`MongodbSandboxExecutionGuard`] — sandbox execution locking,
//! * [`MongodbSnapshotSpec`] — sandbox workspace snapshots.
//!
//! The harness [`DistributedStore`] carries `agent_state_store` + `isolation_scope` plus the
//! composite slots `base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`
//! (`EXTENSIONS_GAP_TRACKER.md` §2.2, resolved 2026-10-28). This type stores the full
//! component set and projects all of it via
//! [`MongodbDistributedStore::to_harness_store`].

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;
use serde_json::Value;

use crate::client::{scoped_collection, MongoClient};
use crate::constants::{BASE_STORE_COLLECTION, SESSIONS_COLLECTION, SNAPSHOTS_COLLECTION};
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
    /// `database_name = None` follows the **client's** default database
    /// ([`MongoClient::default_database`] — the URI database, else
    /// [`DEFAULT_DATABASE`](crate::constants::DEFAULT_DATABASE)), mirroring Java
    /// `MongoDistributedStore#fromConnectionString` which reads
    /// `connectionString.getDatabase()`: hardcoding `agentscope` here made a store built over
    /// `mongodb://host/mydb` read/write a different database than the Java deployment did
    /// (MONGO-01).
    ///
    /// Call [`MongodbDistributedStore::ensure_indexes`] before first use to create the
    /// required indexes.
    pub fn new(client: Arc<dyn MongoClient>, database_name: Option<&str>) -> Self {
        let database_name = database_name
            .map(str::to_string)
            .unwrap_or_else(|| client.default_database().to_string());

        // Every component is database-scoped (Java: `mongoClient.getDatabase(databaseName)
        // .getCollection(...)`). The flat `MongoClient` seam encodes the database into the
        // collection key via `scoped_collection`, so two stores on different databases
        // never share documents.
        let sessions_collection = scoped_collection(&database_name, SESSIONS_COLLECTION);
        let base_collection = scoped_collection(&database_name, BASE_STORE_COLLECTION);

        let agent_state_store = {
            // Cascade-delete snapshots when a session is removed. Only the SESSION
            // scope is eligible because a SESSION-scoped sandbox is tied 1:1 to the
            // session; USER/AGENT/GLOBAL share sandboxes across sessions. This is
            // best-effort: a failure is logged and the session is still removed.
            let cascade_snapshot_client =
                MongodbRemoteSnapshotClient::new(client.clone(), Some(&database_name), None);
            let cascade_state_client = client.clone();
            let cascade_collection = sessions_collection.clone();
            MongodbAgentStateStore::builder()
                .client(client.clone())
                .collection(sessions_collection.clone())
                .on_delete(Arc::new(
                    move |_user_id: Option<String>,
                          session_id: String|
                          -> futures::future::BoxFuture<'static, ()> {
                        let snapshot_client = cascade_snapshot_client.clone();
                        let state_client = cascade_state_client.clone();
                        let collection = cascade_collection.clone();
                        Box::pin(async move {
                            let snapshot_id = match find_session_snapshot_id(
                                &state_client,
                                &collection,
                                &session_id,
                            )
                            .await
                            {
                                Some(id) => id,
                                None => return,
                            };
                            match snapshot_client.delete(&snapshot_id).await {
                                Ok(true) => tracing::info!(
                                    "[mongo-cascade] deleted snapshot {snapshot_id} for session {session_id}"
                                ),
                                Ok(false) => {}
                                Err(e) => tracing::warn!(
                                    "[mongo-cascade] failed to cascade-delete snapshot \
                                     {snapshot_id} for session {session_id}: {e}"
                                ),
                            }
                        })
                    },
                ))
                .build()
                .expect("client provided")
        };

        let base_store = MongodbStore::new(client.clone(), base_collection);
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
        }
    }

    /// Creates a distributed store **and** its component indexes (MONGO-03).
    ///
    /// Java's `MongoDistributedStore` eagerly constructs every component, and each
    /// component constructor calls `ensureIndexes()`. A Rust deployment that used plain
    /// [`Self::new`] and forgot [`Self::ensure_indexes`] silently lost the session TTL
    /// index (sessions never expire) and the compound indexes (full collection scans).
    /// This async constructor is the equivalent: [`Self::new`] plus
    /// `ensure_indexes().await`.
    ///
    /// [`Self::new`] stays available for callers that manage indexes themselves (e.g.
    /// migrations that need the collections to exist first).
    pub async fn try_new(
        client: Arc<dyn MongoClient>,
        database_name: Option<&str>,
    ) -> Result<Self> {
        let store = Self::new(client, database_name);
        store.ensure_indexes().await?;
        Ok(store)
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

    /// The harness [`SandboxGuard`](agentscope_harness::agent::sandbox::SandboxGuard) view
    /// of the MongoDB guard.
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

    /// Projects the MongoDB components into the harness [`DistributedStore`].
    ///
    /// The execution-guard slot carries [`MongodbGuardClient`] (the harness
    /// `SandboxGuard` view of [`MongodbSandboxExecutionGuard`]).
    pub fn to_harness_store(&self) -> DistributedStore {
        let state: Arc<dyn agentscope_core::state::AgentStateStore> =
            Arc::new(self.agent_state_store.clone());
        let base: Arc<dyn agentscope_harness::filesystem::remote::store::BaseStore> =
            Arc::new(self.base_store.clone());
        let snapshots: Arc<dyn agentscope_harness::agent::sandbox::SandboxSnapshotSpec> =
            Arc::new(self.snapshot_spec.clone());
        let guard: Arc<dyn agentscope_harness::agent::sandbox::SandboxGuard> =
            self.guard_client.clone();
        DistributedStore::builder()
            .agent_state_store(state)
            .isolation_scope(self.isolation_scope)
            .base_store(base)
            .sandbox_snapshot_spec(snapshots)
            .sandbox_execution_guard(guard)
            .build()
    }
}

fn map_err(e: crate::client::MongoError) -> Error {
    Error::Tool(format!("MongoDB distributed store operation failed: {e}"))
}

/// Best-effort lookup of a SESSION-scoped remote snapshot id for a session.
///
/// Mirrors Java `MongoDistributedStore.findSessionSnapshotId`: reads the harness
/// `AgentStateStoreSandboxStateStore` slot (`sandbox/session/<sessionId>` /
/// `_sandbox_state`) from the sessions collection and extracts `snapshot.id` when
/// `snapshot.type == "remote"`.
///
/// The slot is always written with `user = None` (see
/// `agentscope_harness::agent::sandbox::AgentStateStoreSandboxStateStore`), which is the
/// anonymous column — the same `null` user Java passes to `agentStateStore().get(...)`.
async fn find_session_snapshot_id(
    client: &Arc<dyn MongoClient>,
    collection: &str,
    session_id: &str,
) -> Option<String> {
    let filter = crate::state_store::slot_filter(None, &format!("sandbox/session/{session_id}"));
    let doc = client.find_one(collection, &filter).await.ok().flatten()?;
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
        // The distributed store database-scopes every component, including the session
        // collection the cascade reads.
        raw.update_one(
            &scoped_collection("agentscope", SESSIONS_COLLECTION),
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

    /// MONGO-03: the eager constructor leaves every component index in place, mirroring
    /// the Java components whose constructors call `ensureIndexes()`. Plain `new` keeps
    /// creating none (that is the hazard the eager constructor closes).
    #[tokio::test]
    async fn try_new_creates_every_component_index() {
        use crate::client::MongoClient as _;
        use crate::constants::SANDBOX_LOCKS_COLLECTION;

        let raw = Arc::new(InMemoryMongoClient::new());
        let client: Arc<dyn MongoClient> = raw.clone();

        let _plain = MongodbDistributedStore::new(client.clone(), Some("plain"));
        assert!(
            raw.list_indexes(&scoped_collection("plain", SESSIONS_COLLECTION))
                .await
                .unwrap()
                .is_empty(),
            "`new` creates no index; only `try_new`/`ensure_indexes` do"
        );

        let s = MongodbDistributedStore::try_new(client.clone(), Some("eager"))
            .await
            .expect("try_new creates the component indexes");
        assert_eq!(s.database_name(), "eager");

        let sessions = raw
            .list_indexes(&scoped_collection("eager", SESSIONS_COLLECTION))
            .await
            .unwrap();
        let compound = sessions
            .iter()
            .find(|i| i.keys.len() == 2)
            .expect("session compound index");
        assert_eq!(
            compound.keys,
            vec![("user_id".to_string(), 1), ("session_id".to_string(), 1)]
        );

        let base = raw
            .list_indexes(&scoped_collection("eager", BASE_STORE_COLLECTION))
            .await
            .unwrap();
        let namespace = base
            .iter()
            .find(|i| i.keys.len() == 2)
            .expect("base store namespace+key index");
        assert_eq!(
            namespace.keys,
            vec![("namespace".to_string(), 1), ("key".to_string(), 1)]
        );

        let locks = raw
            .list_indexes(&scoped_collection("eager", SANDBOX_LOCKS_COLLECTION))
            .await
            .unwrap();
        let lock_ttl = locks
            .iter()
            .find(|i| i.expire_after_seconds == Some(0))
            .expect("sandbox lock TTL index");
        assert_eq!(lock_ttl.keys, vec![("expiresAt".to_string(), 1)]);
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
        let lease = guard.try_enter(key.clone()).await.expect("lease");
        assert!(!lease.is_noop());
        guard.release(&key).await;
    }

    #[test]
    fn harness_projection_carries_state_store() {
        let s = store();
        let harness = s.to_harness_store();
        assert!(harness.agent_state_store.is_some());
    }

    /// Tracker §2.2 resolved 2026-10-28: base store, snapshot spec and guard are projected;
    /// the guard is reachable through the object-safe `SandboxGuard` trait.
    #[tokio::test]
    async fn harness_projection_fills_every_composite_slot() {
        use agentscope_harness::agent::sandbox::SandboxGuard as _;
        use agentscope_harness::filesystem::remote::store::BaseStore as _;

        let s = store();
        let harness = s.to_harness_store();

        let base = harness.base_store.as_ref().expect("base_store projected");
        let mut v = serde_json::Map::new();
        v.insert("projected".to_string(), json!(true));
        base.put(&["ns".to_string()], "k", v);
        assert!(base.get(&["ns".to_string()], "k").is_some());

        let spec = harness
            .sandbox_snapshot_spec
            .as_ref()
            .expect("sandbox_snapshot_spec projected");
        assert_eq!(spec.build("projected").expect("build").id(), "projected");

        let guard = harness
            .sandbox_execution_guard
            .as_ref()
            .expect("sandbox_execution_guard projected");
        let key = agentscope_harness::agent::sandbox::SandboxIsolationKey::new("user", "u");
        let lease = guard.try_enter(key.clone()).await.expect("lease");
        assert!(!lease.is_noop());
        assert!(guard.is_held(&key));
        guard.release(&key).await;
    }

    #[test]
    fn defaults_to_agentscope_database() {
        let s = MongodbDistributedStore::new(Arc::new(InMemoryMongoClient::new()), None);
        assert_eq!(s.database_name(), "agentscope");
    }

    /// MONGO-01: `new(client, None)` must follow the **connection's** database, not a hardcoded
    /// `agentscope` (Java `MongoDistributedStore#fromConnectionString` reads
    /// `connectionString.getDatabase()`). Before the fix a store over `mongodb://host/mydb`
    /// wrote sessions/workspace files into `agentscope` and could not see existing data.
    #[tokio::test]
    async fn default_database_follows_the_client_connection() {
        use agentscope_core::state::AgentStateStore;

        let raw = Arc::new(InMemoryMongoClient::new().with_default_database("mydb"));
        let s = MongodbDistributedStore::new(raw.clone(), None);
        assert_eq!(s.database_name(), "mydb");

        s.agent_state_store()
            .save(None, "sess", "k", json!(1))
            .await;
        let scoped = scoped_collection("mydb", SESSIONS_COLLECTION);
        assert_eq!(raw.document_count(&scoped), 1);
        assert_eq!(raw.document_count(SESSIONS_COLLECTION), 0);

        // An explicit database name still wins over the connection default.
        let explicit = MongodbDistributedStore::new(raw.clone(), Some("otherdb"));
        assert_eq!(explicit.database_name(), "otherdb");
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
    async fn deleting_session_cascades_to_snapshot_written_by_the_harness_store() {
        // The slot the cascade lookup reads used to have no production writer: MONGODB could
        // parse `states._sandbox_state.json` but nothing ever wrote it. This test exercises the
        // real harness writer (`AgentStateStoreSandboxStateStore`) end to end, instead of
        // hand-seeding the document.
        use agentscope_core::state::AgentStateStore;
        use agentscope_harness::agent::sandbox::{
            AgentStateStoreSandboxStateStore, RemoteSandboxState, SandboxSnapshotRef, SandboxState,
            SandboxStateStore,
        };

        let raw = Arc::new(InMemoryMongoClient::new());
        let s = MongodbDistributedStore::new(raw.clone(), Some("agentscope"));

        s.snapshot_client()
            .upload("snap-77", b"workspace-tar")
            .await
            .unwrap();

        let sandbox_store = AgentStateStoreSandboxStateStore::new(
            Arc::new(s.agent_state_store().clone()),
            "agent-1",
        );
        sandbox_store
            .save(
                "session:sess-77",
                &SandboxState::Remote(RemoteSandboxState {
                    session_id: "sess-77".to_string(),
                    endpoint: "http://localhost:8080".to_string(),
                    token: None,
                    workspace_root_ready: true,
                    workspace_projection_hash: None,
                    metadata: Default::default(),
                    snapshot: Some(SandboxSnapshotRef::Remote {
                        id: "snap-77".to_string(),
                    }),
                }),
            )
            .await
            .unwrap();

        // Deleting the *session* cascades into its `sandbox/session/<id>` slot.
        s.agent_state_store()
            .delete(Some("__anon__"), "sess-77")
            .await;
        assert!(!s.snapshot_client().exists("snap-77").await.unwrap());
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

    /// Java scopes every component with `getDatabase(databaseName)`; the flat `MongoClient`
    /// seam encodes it into the collection key, so two stores on different databases must
    /// not see each other's session state or workspace files.
    #[tokio::test]
    async fn database_name_partitions_state_and_base_store() {
        use agentscope_core::state::AgentStateStore;

        let client: Arc<dyn MongoClient> = Arc::new(InMemoryMongoClient::new());
        let a = MongodbDistributedStore::new(client.clone(), Some("db_a"));
        let b = MongodbDistributedStore::new(client.clone(), Some("db_b"));

        a.agent_state_store()
            .save(Some("u"), "sess", "k", json!("a"))
            .await;
        b.agent_state_store()
            .save(Some("u"), "sess", "k", json!("b"))
            .await;
        assert_eq!(
            a.agent_state_store().get(Some("u"), "sess", "k").await,
            Some(json!("a"))
        );
        assert_eq!(
            b.agent_state_store().get(Some("u"), "sess", "k").await,
            Some(json!("b"))
        );

        let mut value = serde_json::Map::new();
        value.insert("content".to_string(), json!("a-file"));
        a.base_store().put(&["ns".to_string()], "k", value);
        assert!(a.base_store().get(&["ns".to_string()], "k").is_some());
        assert!(
            b.base_store().get(&["ns".to_string()], "k").is_none(),
            "db_b must not see db_a's workspace files"
        );

        assert_ne!(
            a.agent_state_store().collection(),
            b.agent_state_store().collection()
        );
        assert!(a.agent_state_store().collection().starts_with("db_a::"));
    }
}
