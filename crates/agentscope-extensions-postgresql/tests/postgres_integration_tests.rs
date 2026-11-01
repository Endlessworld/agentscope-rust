//! Live PostgreSQL integration tests — **environment gated**.
//!
//! The rest of this crate's tests run fully offline against [`MemoryClient`] (an in-memory
//! SQL simulator). These tests exercise the real `tokio-postgres` adapter
//! ([`PgSimpleClient`]) and are skipped unless `AGENTSCOPE_TEST_POSTGRES_URL` is set:
//!
//! ```text
//! # PowerShell
//! $env:AGENTSCOPE_TEST_POSTGRES_URL = "host=localhost user=postgres password=secret dbname=agentscope"
//! cargo test -p agentscope-extensions-postgresql --features postgres-driver \
//!     --test postgres_integration_tests
//! ```
//!
//! Without the variable (or when the server is unreachable) every test prints a `skipping`
//! line and returns successfully, so `cargo test` stays green in CI without a database.
//!
//! Note on runtimes (PG-01): [`PgSimpleClient`] drives `tokio-postgres` on its own runtime,
//! and its `block_on` bridge is runtime-safe: a **synchronous** call (`BaseStore`, advisory
//! `try_enter`, DDL) made from inside a multi-threaded Tokio runtime uses `block_in_place`
//! instead of panicking, which is exactly what the harness `RemoteFilesystem` needs (it
//! awaits `BaseStore` from inside `async fn`s). The async store traits additionally bridge to
//! `spawn_blocking`.
#![cfg(feature = "postgres-driver")]

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_postgresql::{
    driver::PgSimpleClient, PostgresAgentStateStore, PostgresClient, PostgresError,
    PostgresRemoteSnapshotClient, PostgresSandboxExecutionGuard, PostgresStore, QueryOutcome,
    SqlStatement, SqlValue, UNVERSIONED,
};
use agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient;
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use agentscope_harness::filesystem::{BaseStore, StoreValue};
use serde_json::json;

const ENV: &str = "AGENTSCOPE_TEST_POSTGRES_URL";

/// Opens a client, or returns `None` (after printing why) when the test must be skipped.
fn connect() -> Option<Arc<PgSimpleClient>> {
    let url = match std::env::var(ENV) {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            eprintln!("skipping: set {ENV} to run live PostgreSQL tests");
            return None;
        }
    };
    match PgSimpleClient::connect(url.trim()) {
        Ok(client) => Some(client),
        Err(e) => {
            eprintln!("skipping: cannot connect to {ENV}: {e}");
            None
        }
    }
}

/// Builds a small multi-thread runtime for the async trait calls.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime")
}

/// Process-unique identifier, keeping every test isolated in its own schema/table
/// (the tests run concurrently and may share one database).
fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{}_{nanos}", std::process::id())
}

fn drop_schema(client: &dyn PostgresClient, schema: &str) {
    let _ = client.query(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[]);
}

fn drop_table(client: &dyn PostgresClient, table: &str) {
    let _ = client.query(&format!("DROP TABLE IF EXISTS \"{table}\""), &[]);
}

fn value(v: serde_json::Value) -> StoreValue {
    v.as_object().expect("object value").clone()
}

#[test]
fn agent_state_roundtrip_and_versioned_cas_on_a_live_server() {
    let Some(client) = connect() else { return };
    let schema = unique("agentscope_it_state");
    drop_schema(client.as_ref(), &schema);

    // `create_if_not_exist` defaults to `true` (Java builder semantics), so `build()` runs
    // the DDL synchronously — outside any runtime, as `PgSimpleClient` requires.
    let store = PostgresAgentStateStore::builder(client.clone())
        .schema(schema.clone())
        .unwrap()
        .table("sessions_it")
        .unwrap()
        .build()
        .unwrap();

    let rt = runtime();
    rt.block_on(async {
        // single value round-trip
        store.save(None, "s1", "k", json!({"n": 1})).await;
        assert_eq!(store.get(None, "s1", "k").await, Some(json!({"n": 1})));
        assert!(store.exists(None, "s1").await);
        assert_eq!(store.list_session_ids(None).await, vec!["s1".to_string()]);

        // list + incremental append through the core `list_hash` path
        store.save_list(None, "s1", "log", &[json!(1)]).await;
        store
            .save_list(None, "s1", "log", &[json!(1), json!(2)])
            .await;
        assert_eq!(
            store.get_list(None, "s1", "log").await,
            vec![json!(1), json!(2)]
        );
        // full rewrite (shrinking list) inside one transaction
        store.save_list(None, "s1", "log", &[json!(9)]).await;
        assert_eq!(store.get_list(None, "s1", "log").await, vec![json!(9)]);

        // versioned CAS: the trait surface must be backed by the real implementation
        assert!(AgentStateStore::supports_versioning(&store));
        assert_eq!(
            AgentStateStore::get_versioned(&store, None, "s1", "k")
                .await
                .version,
            1
        );
        assert_eq!(
            AgentStateStore::save_if_version(&store, None, "s1", "k", json!({"n": 2}), 1).await,
            2
        );
        assert_eq!(
            AgentStateStore::save_if_version(&store, None, "s1", "k", json!({"n": 3}), 1).await,
            UNVERSIONED
        );
        assert_eq!(store.get(None, "s1", "k").await, Some(json!({"n": 2})));

        // cleanup: session-level delete removes every row of the session
        store.delete(None, "s1").await;
        assert!(!store.exists(None, "s1").await);
    });

    drop_schema(client.as_ref(), &schema);
}

#[test]
fn base_store_cas_and_prefix_search_on_a_live_server() {
    let Some(client) = connect() else { return };
    let schema = unique("agentscope_it_store");
    drop_schema(client.as_ref(), &schema);

    let store = PostgresStore::builder(client.clone())
        .schema(schema.clone())
        .unwrap()
        .table("kv_it")
        .unwrap()
        .initialize_schema(true)
        .build()
        .unwrap();

    let ns = vec!["filesystem".to_string()];
    assert!(store.supports_atomic_session_storage());
    assert!(store.get(&ns, "k").is_none());

    // create-if-absent, then a conflicting create must fail
    assert!(store.put_if_version(&ns, "k", value(json!({"a": 1})), 0));
    assert!(!store.put_if_version(&ns, "k", value(json!({"a": 2})), 0));
    assert_eq!(store.get(&ns, "k").unwrap().version, 1);

    // matched / mismatched CAS
    assert!(store.put_if_version(&ns, "k", value(json!({"a": 2})), 1));
    assert!(!store.put_if_version(&ns, "k", value(json!({"a": 3})), 1));
    assert_eq!(store.get(&ns, "k").unwrap().version, 2);

    // unconditional put + sorted prefix search + delete
    store.put(&ns, "b", value(json!({"b": 2})));
    let keys: Vec<String> = store
        .search(&ns, 10, 0)
        .into_iter()
        .map(|item| item.key)
        .collect();
    assert_eq!(keys, vec!["b".to_string(), "k".to_string()]);
    store.delete(&ns, "k");
    assert!(store.get(&ns, "k").is_none());

    drop_schema(client.as_ref(), &schema);
}

#[test]
fn snapshot_roundtrip_on_a_live_server() {
    let Some(client) = connect() else { return };
    let table = unique("agentscope_it_snap");
    drop_table(client.as_ref(), &table);

    let client_snapshot =
        PostgresRemoteSnapshotClient::with_table(client.clone(), &table, true).unwrap();

    let rt = runtime();
    rt.block_on(async {
        assert!(!client_snapshot.exists("s1").await.unwrap());
        client_snapshot.upload("s1", b"archive").await.unwrap();
        assert!(client_snapshot.exists("s1").await.unwrap());
        assert_eq!(client_snapshot.download("s1").await.unwrap(), b"archive");
        // upsert overwrites
        client_snapshot.upload("s1", b"archive-v2").await.unwrap();
        assert_eq!(client_snapshot.download("s1").await.unwrap(), b"archive-v2");
        assert!(client_snapshot.download("absent").await.is_err());
    });

    drop_table(client.as_ref(), &table);
}

/// PG-01 acceptance: the harness awaits `BaseStore` from inside `async fn`s, i.e. on a Tokio
/// worker. Before the fix `PgSimpleClient` called `Runtime::block_on` unconditionally there and
/// panicked with "cannot block the current thread from within a runtime" on the first
/// `read`/`write`; the driver now blocks in place instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_store_operations_inside_a_multi_thread_runtime_do_not_panic() {
    let Some(client) = connect() else { return };
    let table = unique("agentscope_it_rt");
    drop_table(client.as_ref(), &table);

    // DDL runs synchronously — on this runtime's worker thread.
    let store = PostgresStore::builder(client.clone())
        .table(table.clone())
        .unwrap()
        .initialize_schema(true)
        .build()
        .unwrap();

    let ns = vec!["filesystem".to_string()];
    store.put(&ns, "k", value(json!({"v": 1})));
    assert_eq!(
        store.get(&ns, "k").map(|item| item.value),
        Some(value(json!({"v": 1})))
    );

    // Advisory-lock / transaction paths drive extra connections from the same worker.
    assert!(store.put_if_version(&ns, "k", value(json!({"v": 2})), 1));
    assert_eq!(store.get(&ns, "k").unwrap().version, 2);

    // The advisory-lock guard opens a dedicated session and drives it through the same
    // `block_on` bridge — this used to panic as well.
    let guard = PostgresSandboxExecutionGuard::builder(client.clone()).build();
    let key = SandboxIsolationKey::new("session", unique("lock"));
    let lease = guard
        .try_enter(&key)
        .expect("advisory lock must be acquired from a worker thread");
    assert!(lease.is_open());
    drop(lease);

    store.delete(&ns, "k");
    assert!(store.get(&ns, "k").is_none());

    drop_table(client.as_ref(), &table);
}

#[test]
fn advisory_lock_is_exclusive_across_sessions_on_a_live_server() {
    let Some(client) = connect() else { return };
    let key = SandboxIsolationKey::new("session", unique("lock"));

    let holder = PostgresSandboxExecutionGuard::builder(client.clone())
        .lock_timeout(Duration::from_secs(5))
        .unwrap()
        .build();
    let contender = PostgresSandboxExecutionGuard::builder(client.clone())
        .lock_timeout(Duration::from_millis(300))
        .unwrap()
        .build();

    let lease = holder.try_enter(&key).expect("first lease must succeed");
    assert!(lease.is_open());

    // PG advisory locks are session scoped: a *second dedicated session* must lose.
    assert!(matches!(
        contender.try_enter(&key),
        Err(PostgresError::LockTimeout(_))
    ));

    drop(lease);
    let lease = contender
        .try_enter(&key)
        .expect("lock must be free once the lease is dropped");
    assert!(lease.is_open());
}

#[test]
fn transaction_seam_commits_and_rolls_back_on_a_live_server() {
    let Some(client) = connect() else { return };
    let table = unique("agentscope_it_tx");
    drop_table(client.as_ref(), &table);
    client
        .execute_batch(&format!(
            "CREATE TABLE \"{table}\" (k TEXT PRIMARY KEY, v TEXT NOT NULL)"
        ))
        .unwrap();

    let insert = |k: &str, v: &str| {
        SqlStatement::new(
            format!("INSERT INTO \"{table}\" (k, v) VALUES ($1, $2)"),
            vec![SqlValue::text(k), SqlValue::text(v)],
        )
    };

    let outcomes = client
        .execute_in_transaction(&[insert("a", "1"), insert("b", "2")])
        .expect("multi-statement write must commit");
    assert_eq!(outcomes.len(), 2);

    // The second statement violates the primary key: everything must roll back.
    let failing = client.execute_in_transaction(&[insert("c", "3"), insert("a", "4")]);
    assert!(failing.is_err());

    let rows = client
        .query(&format!("SELECT k FROM \"{table}\" ORDER BY k"), &[])
        .unwrap();
    match rows {
        QueryOutcome::Rows(rows) => {
            let keys: Vec<&str> = rows
                .iter()
                .filter_map(|row| row.first().and_then(|cell| cell.as_str()))
                .collect();
            assert_eq!(keys, vec!["a", "b"]);
        }
        other => panic!("expected rows, got {other:?}"),
    }

    drop_table(client.as_ref(), &table);
}

/// PG-09: the statement kind comes from the server's prepared-statement columns, not from a
/// `SELECT` prefix. A CTE ending in `SELECT`, an `EXPLAIN` or a leading comment used to be
/// routed to `execute`, so callers saw `Affected` and read an empty result set.
#[test]
fn statement_kind_follows_the_server_result_set_on_a_live_server() {
    let Some(client) = connect() else { return };

    match client
        .query("WITH x AS (SELECT 1 AS n) SELECT n FROM x", &[])
        .expect("CTE select")
    {
        QueryOutcome::Rows(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].first().and_then(|cell| cell.as_i32()), Some(1));
        }
        other => panic!("a CTE SELECT must be reported as rows, got {other:?}"),
    }

    // A leading comment must not change the classification.
    assert!(matches!(
        client.query("/* keep me */ SELECT 2", &[]),
        Ok(QueryOutcome::Rows(_))
    ));
    assert!(matches!(
        client.query("EXPLAIN SELECT 1", &[]),
        Ok(QueryOutcome::Rows(_))
    ));

    // A statement without a result set still reports an affected-row count.
    let table = unique("agentscope_it_kind");
    match client
        .query(&format!("CREATE TABLE \"{table}\" (k TEXT)"), &[])
        .expect("DDL executes")
    {
        QueryOutcome::Affected(_) => {}
        other => panic!("DDL must be reported as affected rows, got {other:?}"),
    }
    drop_table(client.as_ref(), &table);
}
