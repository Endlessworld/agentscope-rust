//! Live MySQL integration tests for the `mysql_async` driver adapter.
//!
//! These tests need a real server, so they are **environment gated** instead of
//! `#[ignore]`d: they skip themselves (printing a note) when `AGENTSCOPE_TEST_MYSQL_URL`
//! is unset, and CI/local runs with a server exercise them normally.
//!
//! ```text
//! $env:AGENTSCOPE_TEST_MYSQL_URL = 'mysql://root:secret@127.0.0.1:3306/mysql'
//! $env:AGENTSCOPE_TEST_MYSQL_DATABASE = 'agentscope_it'   # optional, default below
//! cargo test -p agentscope-extensions-mysql --features mysql_async --test mysql_integration
//! ```
//!
//! The URL's default schema must be one the user can connect to (the tests themselves
//! create `AGENTSCOPE_TEST_MYSQL_DATABASE` via the store's schema bootstrap, which needs
//! `CREATE` privilege). Tables are dropped before each test so runs are repeatable.
#![cfg(feature = "mysql_async")]

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::state::AgentStateStore as _;
use agentscope_extensions_mysql::{
    LockBackend, MysqlAgentStateStore, MysqlAsyncBackend, MysqlAsyncConfig, MysqlError,
    MysqlRemoteSnapshotClient, MysqlSandboxExecutionGuard, MysqlStore, SnapshotBackend, SqlBackend,
    StateBackend,
};
use agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient;
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use agentscope_harness::filesystem::remote::store::BaseStore;
use serde_json::json;

/// Returns the configured URL, or `None` when the environment gate is closed.
fn url() -> Option<String> {
    match std::env::var("AGENTSCOPE_TEST_MYSQL_URL") {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// Prints the skip note and returns `true` when the test should stop.
fn skip() -> bool {
    if url().is_some() {
        return false;
    }
    eprintln!(
        "skipping: set AGENTSCOPE_TEST_MYSQL_URL to run the live MySQL integration tests"
    );
    true
}

fn config() -> MysqlAsyncConfig {
    let database = std::env::var("AGENTSCOPE_TEST_MYSQL_DATABASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "agentscope_it".to_string());
    MysqlAsyncConfig::new(
        database,
        "agentscope_it_sessions",
        "agentscope_it_store",
        "agentscope_it_snapshots",
    )
}

fn connect(url: &str) -> Arc<MysqlAsyncBackend> {
    MysqlAsyncBackend::connect_with_config(url, config()).expect("connect to MySQL")
}

/// Drops the three tables so each test starts from an empty schema.
fn reset_schema(backend: &Arc<MysqlAsyncBackend>) {
    let config = backend.config();
    for table in [
        config.state_table.as_str(),
        config.store_table.as_str(),
        config.snapshot_table.as_str(),
    ] {
        let sql = format!("DROP TABLE IF EXISTS `{}`.`{}`", config.database, table);
        <MysqlAsyncBackend as StateBackend>::execute_ddl(&**backend, &sql)
            .expect("drop table");
    }
}

fn drop_table(backend: &Arc<MysqlAsyncBackend>, table: &str) {
    let config = backend.config();
    let sql = format!("DROP TABLE IF EXISTS `{}`.`{}`", config.database, table);
    <MysqlAsyncBackend as StateBackend>::execute_ddl(&**backend, &sql).expect("drop table");
}

/// An unreachable server must fail at construction, never hand back a dead backend.
#[test]
fn connect_to_an_unreachable_server_reports_driver_unavailable() {
    let err = MysqlAsyncBackend::connect_with_config(
        "mysql://root:secret@127.0.0.1:1/mysql",
        MysqlAsyncConfig::default(),
    )
    .expect_err("port 1 must not answer");
    assert!(
        matches!(err, MysqlError::DriverUnavailable(_)),
        "unexpected error: {err}"
    );
}

#[test]
fn state_store_round_trip_cas_and_lists() {
    if skip() {
        return;
    }
    let backend = connect(&url().unwrap());
    reset_schema(&backend);
    let config = config();
    let state_backend: Arc<dyn StateBackend> = backend.clone();
    let store = MysqlAgentStateStore::with_config(
        state_backend,
        Some(&config.database),
        Some(&config.state_table),
        true,
    )
    .expect("schema bootstrap");

    // Single value + version CAS.
    let first = futures::executor::block_on(store.save_if_version(
        Some("alice"),
        "s1",
        "counter",
        json!(1),
        None,
    ))
    .expect("unconditional save");
    assert_eq!(first, 1);
    assert_eq!(store.read_version(Some("alice"), "s1", "counter").unwrap(), 1);

    let created = futures::executor::block_on(store.save_if_version(
        Some("alice"),
        "s1",
        "counter",
        json!(2),
        Some(1),
    ))
    .expect("update with the right version");
    assert_eq!(created, 2);

    // Stale version: the row keeps its data and the CAS reports a conflict.
    let stale = futures::executor::block_on(store.save_if_version(
        Some("alice"),
        "s1",
        "counter",
        json!(99),
        Some(1),
    ));
    assert!(stale.is_err(), "stale CAS must fail: {stale:?}");
    assert_eq!(store.read_version(Some("alice"), "s1", "counter").unwrap(), 2);

    // Lists: append, mutate a prefix, then shrink.
    let store_ref = &store;
    futures::executor::block_on(async move {
        use agentscope_core::state::AgentStateStore as _;
        store_ref.save_list(Some("alice"), "s1", "items", &[json!(1), json!(2)]).await;
        assert_eq!(store_ref.get_list(Some("alice"), "s1", "items").await, vec![json!(1), json!(2)]);

        store_ref.save_list(Some("alice"), "s1", "items", &[json!(9), json!(2), json!(3)]).await;
        assert_eq!(
            store_ref.get_list(Some("alice"), "s1", "items").await,
            vec![json!(9), json!(2), json!(3)],
            "a mutated prefix is rewritten, not just appended"
        );

        store_ref.save_list(Some("alice"), "s1", "items", &[json!(9)]).await;
        assert_eq!(
            store_ref.get_list(Some("alice"), "s1", "items").await,
            vec![json!(9)],
            "shrinking drops stale rows"
        );
    });

    // Sessions are isolated per user and enumerable per user.
    futures::executor::block_on(store.save_if_version(
        Some("bob"),
        "s2",
        "counter",
        json!(1),
        None,
    ))
    .expect("bob save");
    assert_eq!(
        futures::executor::block_on(store.list_session_ids(Some("alice"))),
        vec!["s1".to_string()]
    );
    assert_eq!(
        futures::executor::block_on(store.list_session_ids(Some("bob"))),
        vec!["s2".to_string()]
    );

    // Cleanup helper from the Java store.
    assert_eq!(store.truncate_all_sessions().expect("truncate"), 0);
    assert_eq!(store.read_version(Some("alice"), "s1", "counter").unwrap(), 0);
}

#[test]
fn kv_store_cas_and_prefix_search() {
    if skip() {
        return;
    }
    let backend = connect(&url().unwrap());
    let config = config();
    drop_table(&backend, &config.store_table);
    let sql_backend: Arc<dyn SqlBackend> = backend.clone();
    let store = MysqlStore::builder(sql_backend)
        .table_name(config.store_table.clone())
        .expect("valid table name")
        .initialize_schema(true)
        .build();

    let namespace = vec!["it".to_string(), "kv".to_string()];
    let value = |n: i64| {
        json!({ "n": n })
            .as_object()
            .cloned()
            .expect("object")
    };

    store.put(&namespace, "alpha", value(1));
    assert_eq!(store.get(&namespace, "alpha").expect("get").version, 1);

    assert!(
        store.put_if_version(&namespace, "alpha", value(2), 0),
        "create-if-absent succeeds for a missing key"
    );
    assert_eq!(store.get(&namespace, "alpha").expect("get").version, 1);
    assert!(
        !store.put_if_version(&namespace, "alpha", value(3), 99),
        "a stale version is rejected"
    );
    assert!(
        store.put_if_version(&namespace, "alpha", value(4), 1),
        "the current version is accepted"
    );
    assert_eq!(store.get(&namespace, "alpha").expect("get").version, 2);

    store.put(&namespace, "beta", value(5));
    let found = store.search(&namespace, 10, 0);
    let keys: Vec<String> = found.iter().map(|item| item.key.clone()).collect();
    assert_eq!(keys, vec!["alpha".to_string(), "beta".to_string()]);
    assert!(store.supports_atomic_session_storage());

    store.delete(&namespace, "beta");
    assert!(store.get(&namespace, "beta").is_none());
}

#[test]
fn snapshot_upload_download_and_exists() {
    if skip() {
        return;
    }
    let backend = connect(&url().unwrap());
    drop_table(&backend, &config().snapshot_table);
    let snapshot_backend: Arc<dyn SnapshotBackend> = backend.clone();
    // `new_with_schema` mirrors `JdbcSnapshotSpec`, which always sets
    // `initializeSchema = true`.
    let client = MysqlRemoteSnapshotClient::new_with_schema(snapshot_backend);

    assert!(!futures::executor::block_on(client.exists("snap-1")).expect("exists"));
    futures::executor::block_on(client.upload("snap-1", b"archive-bytes")).expect("upload");
    assert!(futures::executor::block_on(client.exists("snap-1")).expect("exists"));
    assert_eq!(
        futures::executor::block_on(client.download("snap-1")).expect("download"),
        b"archive-bytes".to_vec()
    );
    // A second upload replaces the payload instead of failing on the primary key.
    futures::executor::block_on(client.upload("snap-1", b"v2")).expect("re-upload");
    assert_eq!(
        futures::executor::block_on(client.download("snap-1")).expect("download"),
        b"v2".to_vec()
    );
    assert!(
        futures::executor::block_on(client.download("missing")).is_err(),
        "a missing snapshot must be an error"
    );
}

#[test]
fn advisory_lock_is_session_scoped_across_backends() {
    if skip() {
        return;
    }
    let first = connect(&url().unwrap());
    let second = connect(&url().unwrap());
    let lock_backend: Arc<dyn LockBackend> = first.clone();
    let guard = MysqlSandboxExecutionGuard::builder(lock_backend)
        .key_prefix("agentscope:it:lock:")
        .lock_timeout(Duration::from_secs(5))
        .build();
    let key = SandboxIsolationKey::new("user", "alice");

    let lease = guard.try_enter(&key).expect("acquire");
    assert!(guard.is_held(&key));

    // A second session must *not* get the lock: MySQL `GET_LOCK` is session scoped, so the
    // acquire really happened on a dedicated connection.
    assert!(
        !<MysqlAsyncBackend as LockBackend>::try_acquire(
            &*second,
            "agentscope:it:lock:user:alice",
            Duration::from_secs(0),
        )
        .expect("contended acquire")
    );

    drop(lease);
    assert!(!guard.is_held(&key));
    assert!(
        <MysqlAsyncBackend as LockBackend>::try_acquire(
            &*second,
            "agentscope:it:lock:user:alice",
            Duration::from_secs(5),
        )
        .expect("released acquire")
    );
}
