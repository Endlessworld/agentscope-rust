//! PostgreSQL integration tests — **environment gated**, mirroring the Java
//! `PostgresIntegrationTest` (8 `@Test`, Testcontainers).
//!
//! No Docker is required and no dependency was added: the tests connect to whatever server
//! `AGENTSCOPE_TEST_POSTGRES_URL` points at and **skip** (print + return) when it is unset.
//! They are never `#[ignore]`d, so `cargo test -p agentscope-extensions-jdbc` stays green on a
//! machine without a database.
//!
//! ```powershell
//! # start a disposable server first, e.g.
//! #   docker run --rm -e POSTGRES_PASSWORD=secret -p 55432:5432 postgres:16
//! $env:AGENTSCOPE_TEST_POSTGRES_URL = "host=localhost port=55432 user=postgres password=secret dbname=postgres"
//! cargo test -p agentscope-extensions-jdbc --features postgres-executor --test postgres_integration_tests
//! ```

#![cfg(feature = "postgres-executor")]

use std::sync::Arc;

use agentscope_extensions_jdbc::backend::{ColumnValue, JdbcError, JdbcExecutor, JdbcSession};
use agentscope_extensions_jdbc::dialect::vendor::PostgresDialect;
use agentscope_extensions_jdbc::dialect::{AbstractJdbcDialect, BoundSql, SqlParam};
use agentscope_extensions_jdbc::sandbox::NativeLockFlavor;
use agentscope_extensions_jdbc::{
    next_seq, now_millis, postgres_executor_from_env, AgentStateStore, JdbcDistributedStore,
    JdbcRemoteSnapshotClient, PostgresExecutor, POSTGRES_URL_ENV,
};
use agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient;
use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey};
use agentscope_harness::agent::IsolationScope;
use agentscope_harness::filesystem::BaseStore;

/// Connects, or returns `None` (after printing a skip notice) when no server is configured.
fn executor() -> Option<Arc<PostgresExecutor>> {
    match postgres_executor_from_env() {
        Ok(executor) => Some(Arc::new(executor)),
        Err(JdbcError::Configuration(message)) => {
            eprintln!("skipped: {message} (set {POSTGRES_URL_ENV} to run this test)");
            None
        }
        Err(error) => panic!("unexpected driver error: {error}"),
    }
}

/// A name that does not collide with previous runs.
fn unique(prefix: &str) -> String {
    format!("{prefix}-{}-{}", now_millis(), next_seq())
}

// ---------------------------------------------------------------------------
//  Java: PostgresIntegrationTest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn base_store_round_trip() {
    // Java: PostgresIntegrationTest.storeWorks
    let Some(executor) = executor() else {
        return;
    };
    let store = JdbcDistributedStore::create(executor, "PostgreSQL").expect("schema initialisation");
    let base = store.base_store();
    let namespace = vec![unique("it-store")];

    let mut value = serde_json::Map::new();
    value.insert("v".to_string(), serde_json::json!(7));
    base.put(&namespace, "k", value);

    let item = base.get(&namespace, "k").expect("the item must round-trip");
    assert_eq!(item.value.get("v"), Some(&serde_json::json!(7)));
    assert!(
        base.get(&namespace, "missing").is_none(),
        "an unknown key must read as absent"
    );
}

#[tokio::test]
async fn agent_state_round_trip() {
    // Java: PostgresIntegrationTest.agentStateWorks
    let Some(executor) = executor() else {
        return;
    };
    let store = JdbcDistributedStore::create(executor, "PostgreSQL").expect("schema initialisation");
    let state = store.agent_state_store();
    let session = unique("it-session");

    state
        .save(Some("user1"), &session, "k", serde_json::json!({"v": 11}))
        .await;
    let value = state
        .get(Some("user1"), &session, "k")
        .await
        .expect("the state must round-trip");
    assert_eq!(value.get("v"), Some(&serde_json::json!(11)));
}

#[tokio::test]
async fn snapshot_upload_download_round_trip() {
    // Java: PostgresIntegrationTest.snapshotWorks
    let Some(executor) = executor() else {
        return;
    };
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(PostgresDialect::new());
    let client = JdbcRemoteSnapshotClient::with_schema(executor, dialect).expect("snapshot schema");
    let id = unique("it-snapshot");
    let payload = b"agentscope workspace archive".to_vec();

    client.upload(&id, &payload).await.expect("upload");
    assert!(client.exists(&id).await.expect("exists"));
    assert_eq!(client.download(&id).await.expect("download"), payload);
}

#[tokio::test]
async fn native_advisory_lock_round_trip() {
    // Java: PostgresIntegrationTest.advisoryLockWorks (`pg_try_advisory_lock`)
    let Some(executor) = executor() else {
        return;
    };
    let store = JdbcDistributedStore::create(executor, "PostgreSQL")
        .expect("schema initialisation")
        .with_native_lock_guard(NativeLockFlavor::Postgres);
    let harness = store.to_harness_store();
    let guard = harness
        .sandbox_execution_guard
        .expect("the store projects its guard");

    let isolation = SandboxIsolationKey::resolve(
        Some(&IsolationScope::Global),
        None,
        &unique("it-lock-agent"),
    )
    .expect("isolation key");

    let lease = guard.try_enter(isolation.clone()).await.expect("lease");
    assert!(
        !lease.is_noop(),
        "pg_try_advisory_lock must succeed on a live server"
    );
    guard.release(&isolation).await;
}

#[tokio::test]
async fn a_failed_transaction_rolls_back() {
    // Java: the atomicity `JdbcAgentSkillRepository` relies on (`in_transaction`).
    let Some(executor) = executor() else {
        return;
    };
    executor
        .execute(&BoundSql::no_params(
            "CREATE TABLE IF NOT EXISTS agentscope_jdbc_tx_probe (id BIGINT PRIMARY KEY)",
        ))
        .expect("create probe table");
    executor
        .execute(&BoundSql::no_params("DELETE FROM agentscope_jdbc_tx_probe"))
        .expect("clean probe table");

    let result = executor.in_transaction(&mut |session: &mut dyn JdbcSession| {
        session.execute(&BoundSql::new(
            "INSERT INTO agentscope_jdbc_tx_probe (id) VALUES (?)",
            vec![SqlParam::Int(1)],
        ))?;
        Err(JdbcError::Database("rollback please".to_string()))
    });
    assert!(result.is_err(), "the closure error must be propagated");

    let rows = executor
        .query(&BoundSql::no_params(
            "SELECT count(*) FROM agentscope_jdbc_tx_probe",
        ))
        .expect("count rows");
    assert_eq!(
        rows[0][0],
        ColumnValue::Int(0),
        "the insert must have been rolled back"
    );
}
