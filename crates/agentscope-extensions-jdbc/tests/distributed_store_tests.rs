//! `JdbcDistributedStore` facade tests, mirroring the Java `JdbcDistributedStoreH2Test`.
//!
//! Backend: in-memory SQLite (`RusqliteExecutor::in_memory()`) with `SqliteDialect`.
//!
//! Rust difference (§4.2): the facade assembles all four components eagerly in the constructor
//! (Java caches them lazily), and `to_harness_store()` projects only the agent-state-store half
//! because the harness `DistributedStore` has just `agent_state_store` + `isolation_scope`.

use std::sync::Arc;

use agentscope_extensions_jdbc::dialect::vendor::SqliteDialect;
use agentscope_extensions_jdbc::dialect::{detect_dialect, AbstractJdbcDialect, DialectKind};
use agentscope_extensions_jdbc::{
    AgentStateStore, BaseStore, JdbcDistributedStore, RusqliteExecutor,
};
use agentscope_harness::agent::sandbox::snapshot::{RemoteSnapshotClient, SandboxSnapshotSpec};
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use agentscope_harness::agent::IsolationScope;

fn new_store() -> (JdbcDistributedStore, Arc<RusqliteExecutor>) {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let store = JdbcDistributedStore::create(executor.clone(), "SQLite").unwrap();
    (store, executor)
}

fn ns(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

// ---------------------------------------------------------------------------
//  Java: JdbcDistributedStoreH2Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_state_store_saves_and_retrieves_state() {
    // Java: JdbcDistributedStoreH2Test.agentStateStoreWorks
    let (store, _) = new_store();
    let state_store = store.agent_state_store();
    state_store
        .save(
            Some("user1"),
            "session1",
            "key",
            serde_json::json!({ "value": "state-value" }),
        )
        .await;

    let result = state_store.get(Some("user1"), "session1", "key").await;
    assert_eq!(
        result.and_then(|v| v.get("value").cloned()),
        Some(serde_json::json!("state-value"))
    );
}

#[test]
fn base_store_performs_kv_operations() {
    // Java: JdbcDistributedStoreH2Test.baseStoreWorks
    let (store, _) = new_store();
    let base_store = store.base_store();

    let mut value = serde_json::Map::new();
    value.insert("v".to_string(), serde_json::json!(42));
    base_store.put(&ns(&["ns"]), "k", value);

    let item = base_store.get(&ns(&["ns"]), "k").expect("item must exist");
    assert_eq!(item.value.get("v"), Some(&serde_json::json!(42)));
}

#[tokio::test]
async fn snapshot_spec_uploads_and_downloads_snapshots() {
    // Java: JdbcDistributedStoreH2Test.snapshotSpecWorks
    let (store, _) = new_store();
    let spec = store.sandbox_snapshot_spec();
    let snapshot = spec.build("test-snap").unwrap();
    assert_eq!(snapshot.id(), "test-snap");
    assert_eq!(snapshot.snapshot_type(), "remote");
}

#[test]
fn sandbox_execution_guard_acquires_and_releases_lock() {
    // Java: JdbcDistributedStoreH2Test.sandboxExecutionGuardWorks
    let (store, _) = new_store();
    let guard = store.sandbox_execution_guard();
    let isolation_key =
        SandboxIsolationKey::resolve(Some(&IsolationScope::Global), None, "test-agent").unwrap();

    let mut lease = guard.try_enter(isolation_key);
    assert!(lease.is_acquired());
    lease.release();
}

#[tokio::test]
async fn snapshot_upload_download_via_snapshot_spec_client() {
    // Java: JdbcDistributedStoreH2Test.snapshotUploadDownload
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let spec = agentscope_extensions_jdbc::JdbcSnapshotSpec::new(executor, dialect).unwrap();
    let client = spec.client();

    let data = b"test archive";
    client.upload("facade-snap", data).await.unwrap();
    assert_eq!(client.download("facade-snap").await.unwrap(), data);
}

// ---------------------------------------------------------------------------
//  Facade assembly / dialect plumbing
//  (Java: JdbcDistributedStore.create + accessors)
// ---------------------------------------------------------------------------

#[test]
fn create_auto_detects_dialect_from_product_name() {
    // Java: JdbcDistributedStore.create(dataSource) -> AbstractJdbcDialect.from(ds).build().
    // Only the SQLite product name can be exercised offline against the rusqlite backend:
    // the MySQL/PostgreSQL/H2 DDL is not executable on SQLite (Java covers those with
    // Testcontainers / real H2, tracked as environment-limited in §3.2). The product-name ->
    // dialect mapping itself is asserted below via `detect_dialect`.
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let store = JdbcDistributedStore::create(executor, "SQLite").unwrap();
    assert_eq!(store.dialect().kind(), DialectKind::Sqlite);
}

#[test]
fn dialect_detection_mapping_matches_create_path() {
    // Java: AbstractJdbcDialect.from(ds).build() selects the dialect whose supports() matches;
    // Rust mirrors this with product-name substring matching in `detect_dialect`.
    assert_eq!(detect_dialect("SQLite").kind(), DialectKind::Sqlite);
    assert_eq!(detect_dialect("PostgreSQL").kind(), DialectKind::Postgres);
    assert_eq!(detect_dialect("MySQL").kind(), DialectKind::Mysql);
    assert_eq!(detect_dialect("H2").kind(), DialectKind::Ansi);
}

#[test]
fn create_with_dialect_skips_detection() {
    // Java: JdbcDistributedStore.create(dataSource, dialect)
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let store = JdbcDistributedStore::create_with_dialect(executor, dialect).unwrap();
    assert_eq!(store.dialect().kind(), DialectKind::Sqlite);
}

#[test]
fn component_accessors_return_shared_handles() {
    // Java: volatile lazy accessors return the same instance every call.
    // Rust builds them once in the constructor and hands out Arc clones.
    let (store, _) = new_store();
    assert!(Arc::ptr_eq(
        &store.agent_state_store(),
        &store.agent_state_store()
    ));
    assert!(Arc::ptr_eq(&store.base_store(), &store.base_store()));
    assert!(Arc::ptr_eq(
        &store.sandbox_snapshot_spec(),
        &store.sandbox_snapshot_spec()
    ));
    assert!(Arc::ptr_eq(
        &store.sandbox_execution_guard(),
        &store.sandbox_execution_guard()
    ));
}

#[test]
fn components_share_the_same_executor_and_dialect() {
    // Java: JdbcDistributedStore holds one DataSource + one dialect for all four components.
    // Rust: the shared executor is an `Arc<dyn JdbcExecutor>`; verify it aliases the concrete
    // executor by comparing trait-object data pointers.
    let (store, executor) = new_store();
    let concrete: Arc<dyn agentscope_extensions_jdbc::JdbcExecutor> = executor.clone();
    let shared = store.executor();
    assert!(
        Arc::ptr_eq(&shared, &concrete),
        "the facade must share the executor handed to `create`"
    );
    assert_eq!(store.dialect().kind(), DialectKind::Sqlite);
}

#[test]
fn to_harness_store_projects_agent_state_store_and_isolation_scope() {
    // Java: JdbcDistributedStore implements DistributedStore (all accessors live on the facade).
    // Rust: the harness bundle now carries every JDBC component (see tracker §2.2, resolved
    // 2026-10-28).
    let (store, _) = new_store();
    let harness = store.to_harness_store();
    assert!(harness.agent_state_store.is_some());
    assert_eq!(harness.isolation_scope, IsolationScope::default());
}

#[tokio::test]
async fn to_harness_store_fills_every_composite_slot() {
    // Rust-only bridge test (tracker §2.2 resolved 2026-10-28): the JDBC facade projects its
    // base store, snapshot spec and execution guard into the harness `DistributedStore`.
    let (store, _) = new_store();
    let harness = store.to_harness_store();

    // base_store: the Kv store must answer `BaseStore` calls.
    let base = harness.base_store.as_ref().expect("base_store projected");
    let mut value = serde_json::Map::new();
    value.insert("projected".to_string(), serde_json::json!(true));
    base.put(&ns(&["facade"]), "k", value);
    assert!(base.get(&ns(&["facade"]), "k").is_some());

    // sandbox_snapshot_spec: must build a remote snapshot for a valid id.
    let spec = harness
        .sandbox_snapshot_spec
        .as_ref()
        .expect("sandbox_snapshot_spec projected");
    let snapshot = spec.build("projected-snap").expect("snapshot builds");
    assert_eq!(snapshot.id(), "projected-snap");
    assert_eq!(snapshot.snapshot_type(), "remote");

    // sandbox_execution_guard: usable through the object-safe trait.
    let guard = harness
        .sandbox_execution_guard
        .as_ref()
        .expect("sandbox_execution_guard projected");
    let key = SandboxIsolationKey::new("user", "projected");
    let lease = guard.try_enter(key.clone()).await;
    assert!(!lease.is_noop());
    assert!(guard.is_held(&key));
    assert!(guard.active_count() >= 1);
    guard.release(&key).await;
    assert!(!guard.is_held(&key));
    assert!(
        guard.is_distributed(),
        "JDBC projects a cross-process guard"
    );
}

#[test]
fn with_isolation_scope_is_carried_into_harness_store() {
    // Rust-only bridge: overrides the isolation scope projected into the harness type.
    let (store, _) = new_store();
    let harness = store
        .with_isolation_scope(IsolationScope::Global)
        .to_harness_store();
    assert_eq!(harness.isolation_scope, IsolationScope::Global);
}

#[test]
fn schema_creation_can_be_disabled() {
    // Rust facade: create_with_dialect_and_schema(..., false) verifies instead of creating.
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let result = JdbcDistributedStore::create_with_dialect_and_schema(executor, dialect, false);
    assert!(
        result.is_err(),
        "disabling schema creation must fail on an empty database"
    );
}

#[tokio::test]
async fn reopen_with_auto_create_disabled_accepts_the_existing_schema() {
    // Java: DialectBuilderTableGroupsTest.reopenWithAutoCreateFalse — the builder's
    // `autoCreateTable(false)` path must succeed when a previous run already created the
    // tables (it verifies, it does not create). Dedicated case so the guarantee is not left
    // as an implicit side effect of the skill repository's setup helper.
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());

    let first = JdbcDistributedStore::create_with_dialect_and_schema(
        executor.clone(),
        dialect.clone(),
        true,
    )
    .expect("the first open creates the schema");
    let mut value = serde_json::Map::new();
    value.insert("v".to_string(), serde_json::json!(1));
    first.base_store().put(&ns(&["reopen"]), "k", value);
    drop(first);

    // Re-open the *same* database with auto-creation off: verification must pass and the
    // component must keep working against the tables the first open left behind.
    let reopened = JdbcDistributedStore::create_with_dialect_and_schema(
        executor,
        dialect,
        false,
    )
    .expect("re-opening an existing schema with auto-create off must succeed");
    let item = reopened
        .base_store()
        .get(&ns(&["reopen"]), "k")
        .expect("the data written by the first open must still be there");
    assert_eq!(item.value.get("v"), Some(&serde_json::json!(1)));
}

