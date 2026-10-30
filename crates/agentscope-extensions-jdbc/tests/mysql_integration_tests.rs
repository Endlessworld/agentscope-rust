//! MySQL integration tests — **environment gated**, mirroring the Java
//! `MysqlIntegrationTest` (8 `@Test`, Testcontainers).
//!
//! No Docker is required and no dependency was added: the tests connect to whatever server
//! `AGENTSCOPE_TEST_MYSQL_URL` points at and **skip** (print + return) when it is unset.
//! They are never `#[ignore]`d, so `cargo test -p agentscope-extensions-jdbc` stays green on a
//! machine without a database.
//!
//! ```powershell
//! # start a disposable server first, e.g.
//! #   docker run --rm -e MYSQL_ROOT_PASSWORD=secret -e MYSQL_DATABASE=agentscope -p 33306:3306 mysql:8.0
//! $env:AGENTSCOPE_TEST_MYSQL_URL = "mysql://root:secret@127.0.0.1:33306/agentscope"
//! cargo test -p agentscope-extensions-jdbc --features mysql-executor --test mysql_integration_tests
//! ```
//!
//! The URL **must** name a database (path segment): the MySQL dialect resolves the
//! session/snapshot/skill tables through `DATABASE()`, and the snapshot table embeds a
//! `LONGBLOB` — the case the offline SQLite tests cannot cover (BLOB vs TEXT share one wire
//! type in MySQL and are told apart by the column's character set).

#![cfg(feature = "mysql-executor")]

use std::sync::Arc;

use agentscope_extensions_jdbc::backend::{ColumnValue, JdbcError, JdbcExecutor};
use agentscope_extensions_jdbc::dialect::vendor::MysqlDialect;
use agentscope_extensions_jdbc::dialect::{
    detect_dialect, AbstractJdbcDialect, BoundSql, DialectKind,
};
use agentscope_extensions_jdbc::sandbox::NativeLockFlavor;
use agentscope_extensions_jdbc::skill::JdbcAgentSkillRepository;
use agentscope_extensions_jdbc::{
    mysql_executor_from_env, next_seq, now_millis, AgentStateStore, JdbcDistributedStore,
    JdbcRemoteSnapshotClient, MysqlExecutor, MYSQL_URL_ENV,
};
use agentscope_extensions_skills::AgentSkillRepository;
use agentscope_harness::agent::skill::AgentSkill;
use agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient;
use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey};
use agentscope_harness::agent::IsolationScope;
use agentscope_harness::filesystem::BaseStore;
use serde_json::json;

/// Connects, or returns `None` (after printing a skip notice) when no server is configured.
fn executor() -> Option<Arc<MysqlExecutor>> {
    match mysql_executor_from_env() {
        Ok(executor) => Some(Arc::new(executor)),
        Err(JdbcError::Configuration(message)) => {
            eprintln!("skipped: {message} (set {MYSQL_URL_ENV} to run this test)");
            None
        }
        Err(error) => panic!("unexpected driver error: {error}"),
    }
}

/// A name that does not collide with previous runs (the same database is reused).
fn unique(prefix: &str) -> String {
    format!("{prefix}-{}-{}", now_millis(), next_seq())
}

fn mysql_dialect() -> Arc<dyn AbstractJdbcDialect> {
    Arc::new(MysqlDialect::new())
}

// ---------------------------------------------------------------------------
//  Java: MysqlIntegrationTest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dialect_detection() {
    // Java: MysqlIntegrationTest.dialectDetection (`AbstractJdbcDialect.from(ds)` sees MySQL)
    assert_eq!(detect_dialect("MySQL").kind(), DialectKind::Mysql);
    // MariaDB reports a different product name but is served by the same dialect.
    assert_eq!(detect_dialect("MariaDB").kind(), DialectKind::Mysql);

    // The live server must really speak MySQL: the probe is decoded through the driver.
    let Some(executor) = executor() else {
        return;
    };
    let rows = executor
        .query(&BoundSql::no_params("SELECT 1"))
        .expect("live MySQL probe");
    assert_eq!(rows[0][0], ColumnValue::Int(1));
}

#[tokio::test]
async fn state_store_crud() {
    // Java: MysqlIntegrationTest.stateStoreCrud
    let Some(executor) = executor() else {
        return;
    };
    let store = JdbcDistributedStore::create(executor, "MySQL").expect("schema initialisation");
    let state = store.agent_state_store();
    let session = unique("it-mysql-session");

    state
        .save(Some("user1"), &session, "key", json!({"value": "value"}))
        .await;
    let loaded = state
        .get(Some("user1"), &session, "key")
        .await
        .expect("the state must round-trip");
    assert_eq!(loaded.get("value"), Some(&json!("value")));
    assert!(
        state.get(Some("user2"), &session, "key").await.is_none(),
        "another user's session must not be visible"
    );
}

#[tokio::test]
async fn kv_store_cas() {
    // Java: MysqlIntegrationTest.kvStoreCas
    let Some(executor) = executor() else {
        return;
    };
    let store = JdbcDistributedStore::create(executor, "MySQL").expect("schema initialisation");
    let base = store.base_store();
    let namespace = vec![unique("it-mysql-ns")];

    let mut value = serde_json::Map::new();
    value.insert("v".to_string(), json!(1));
    assert!(
        base.put_if_version(&namespace, "k", value, 0),
        "create-if-absent (expected version 0) must succeed"
    );
    let item = base.get(&namespace, "k").expect("item must exist");
    assert_eq!(item.version, 1);
    assert_eq!(item.value.get("v"), Some(&json!(1)));
}

#[tokio::test]
async fn snapshot_blob_round_trip() {
    // Java: MysqlIntegrationTest.snapshotBlob (LONGBLOB upload/download)
    let Some(executor) = executor() else {
        return;
    };
    let client = JdbcRemoteSnapshotClient::with_schema(executor, mysql_dialect())
        .expect("snapshot schema");
    let id = unique("it-mysql-snapshot");
    // Deliberately binary (non-UTF-8): MySQL reports TEXT and BLOB with the same wire type,
    // so the payload must be decoded by the column's character set, not by UTF-8 validity.
    let payload = vec![0x1f_u8, 0x8b, 0x08, 0x00, 0xff, 0xfe, 0x00, 0x80];

    client.upload(&id, &payload).await.expect("upload");
    assert!(client.exists(&id).await.expect("exists"));
    assert_eq!(client.download(&id).await.expect("download"), payload);
}

#[tokio::test]
async fn long_lock_name_is_sha256_normalised() {
    // Java: MysqlIntegrationTest.longLockName (composed name > 64 chars ⇒ truncated + SHA-256)
    let Some(executor) = executor() else {
        return;
    };
    let store = JdbcDistributedStore::create(executor, "MySQL")
        .expect("schema initialisation")
        .with_native_lock_guard(NativeLockFlavor::Mysql);
    let harness = store.to_harness_store();
    let guard = harness
        .sandbox_execution_guard
        .expect("the store projects its guard");

    let long_value = format!("agent-{}", "x".repeat(120));
    let isolation =
        SandboxIsolationKey::resolve(Some(&IsolationScope::Global), None, &long_value)
            .expect("isolation key");

    let lease = guard.try_enter(isolation.clone()).await;
    assert!(
        !lease.is_noop(),
        "GET_LOCK must succeed with the normalised (>64 char) name on a live server"
    );
    guard.release(&isolation).await;
}

#[tokio::test]
async fn build_blocks_when_a_legacy_table_lacks_the_version_column() {
    // Java: MysqlIntegrationTest.buildBlocksOnLegacyTableMissingColumn
    let Some(executor) = executor() else {
        return;
    };
    // Best effort reset: a previously interrupted run may have left the column dropped.
    // Both failures (table missing / column already present) are expected and ignored.
    let _ = executor.execute(&BoundSql::no_params(
        "ALTER TABLE agentscope_sessions ADD COLUMN version BIGINT NOT NULL DEFAULT 1",
    ));
    JdbcDistributedStore::create(executor.clone(), "MySQL").expect("initial schema");

    executor
        .execute(&BoundSql::no_params(
            "ALTER TABLE agentscope_sessions DROP COLUMN version",
        ))
        .expect("regress the sessions table to the legacy shape");

    let error = JdbcDistributedStore::create(executor.clone(), "MySQL")
        .expect_err("a table without the version column must block construction");
    let message = error.to_string();
    assert!(message.contains("version"), "unexpected message: {message}");
    assert!(
        message.contains("agentscope_sessions"),
        "unexpected message: {message}"
    );

    // Keep the database reusable for the next run.
    executor
        .execute(&BoundSql::no_params(
            "ALTER TABLE agentscope_sessions ADD COLUMN version BIGINT NOT NULL DEFAULT 1",
        ))
        .expect("restore the version column");
}

#[tokio::test]
async fn skill_repository_round_trip() {
    // Java: MysqlIntegrationTest.skillRepositoryRoundTrip
    let Some(executor) = executor() else {
        return;
    };
    let repo = JdbcAgentSkillRepository::with_schema(executor, mysql_dialect())
        .expect("skill schema");
    let name = unique("mysql-skill");
    let skill = AgentSkill::builder()
        .name(name.as_str())
        .description("integration skill")
        .source("integration")
        .add_metadata("homepage", json!("https://example.com"))
        .add_resource("readme.md", "hello")
        .build();

    assert!(repo.save(&[skill], false).expect("save"));
    let loaded = repo.get_skill(&name).expect("load");
    assert_eq!(
        Some(&json!("https://example.com")),
        loaded.metadata().get("homepage")
    );
    assert_eq!(
        Some("hello"),
        loaded.resources().get("readme.md").map(String::as_str)
    );
    assert!(repo.delete(&name).expect("delete"));
    assert!(!repo.skill_exists(&name).expect("exists"));
}

#[tokio::test]
async fn namespaced_skill_isolation() {
    // Java: MysqlIntegrationTest.namespacedSkillIsolation (UNIQUE(namespace, name))
    let Some(executor) = executor() else {
        return;
    };
    let repo = JdbcAgentSkillRepository::with_schema(executor, mysql_dialect())
        .expect("skill schema");
    let name = unique("mysql-ns-skill");
    let team_a = AgentSkill::builder()
        .name(name.as_str())
        .description("team-a")
        .source("integration")
        .add_resource("docs/a.md", "a")
        .build();
    let team_b = AgentSkill::builder()
        .name(name.as_str())
        .description("team-b")
        .source("integration")
        .add_resource("docs/b.md", "b")
        .build();

    assert!(repo.save_in("team-a", &[team_a], false).expect("team-a"));
    assert!(
        repo.save_in("team-b", &[team_b], false)
            .expect("UNIQUE(namespace, name) must admit the same name in another namespace"),
    );
    let from_a = repo.get_skill_in("team-a", &name).expect("team-a read");
    let from_b = repo.get_skill_in("team-b", &name).expect("team-b read");
    assert_eq!("team-a", from_a.description());
    assert_eq!("team-b", from_b.description());

    // Deletes stay inside their namespace: team-b's row survives team-a's delete.
    assert!(repo.delete_in("team-a", &name).expect("delete team-a"));
    assert!(repo.get_skill_in("team-b", &name).is_ok());
    assert!(repo.delete_in("team-b", &name).expect("delete team-b"));
}
