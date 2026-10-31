//! Live MongoDB integration tests.
//!
//! These tests talk to a **real** MongoDB deployment and are gated on the
//! `AGENTSCOPE_TEST_MONGODB_URL` environment variable: when it is unset the tests
//! return immediately (they are deliberately *not* `#[ignore]`d, so a configured CI
//! runner exercises them by default).
//!
//! Run them with (PowerShell):
//!
//! ```powershell
//! $env:AGENTSCOPE_TEST_MONGODB_URL = "mongodb://127.0.0.1:27017"
//! cargo test -p agentscope-extensions-mongodb --test mongodb_live -j 2
//! ```
//!
//! Every test creates its own throw-away database (`agentscope_it_<uuid>`) and drops
//! it at the end, so runs never interfere with each other or with a developer's data.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_mongodb::client::{scoped_collection, FindFilter, IndexInfo};
use agentscope_extensions_mongodb::{
    create_index_with_migration, IndexSpec, MongodbClient, MongodbDistributedStore,
    MongodbSandboxExecutionGuard, UNVERSIONED,
};
use agentscope_harness::agent::sandbox::{RemoteSnapshotClient, SandboxIsolationKey};
use agentscope_harness::filesystem::remote::store::{BaseStore, StoreValue};
use futures::FutureExt as _;
use serde_json::{json, Value};

/// Reads the live-MongoDB URL, or `None` when the suite is not configured.
fn mongo_url() -> Option<String> {
    match std::env::var("AGENTSCOPE_TEST_MONGODB_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => None,
    }
}

fn store_value(value: Value) -> StoreValue {
    value.as_object().cloned().unwrap_or_default()
}

/// Runs `body` against a freshly created, database-scoped distributed store and drops
/// the database afterwards — including when an assertion unwinds, so a failed run never
/// leaves throw-away databases behind.
async fn with_store<F, Fut>(body: F)
where
    F: FnOnce(MongodbDistributedStore, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(url) = mongo_url() else {
        eprintln!("AGENTSCOPE_TEST_MONGODB_URL is not set; skipping the live MongoDB test");
        return;
    };
    let database = format!("agentscope_it_{}", uuid::Uuid::new_v4().simple());
    let client = Arc::new(
        MongodbClient::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("failed to connect to {url}: {e}")),
    );
    let store = MongodbDistributedStore::new(client.clone(), Some(&database));
    store.ensure_indexes().await.expect("ensure_indexes failed");

    let outcome = std::panic::AssertUnwindSafe(body(store.clone(), database.clone()))
        .catch_unwind()
        .await;

    if let Err(e) = client.drop_database(&database).await {
        eprintln!("failed to drop the throw-away database {database}: {e}");
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn state_store_roundtrip_versioning_and_lists() {
    with_store(|store, _database| async move {
        let state = store.agent_state_store();

        // Plain values survive the JSON -> BSON -> JSON round-trip.
        state
            .save(Some("u1"), "s1", "scratchpad", json!({"note": "hi", "n": 3}))
            .await;
        assert_eq!(
            state.get(Some("u1"), "s1", "scratchpad").await,
            Some(json!({"note": "hi", "n": 3}))
        );
        assert!(state.exists(Some("u1"), "s1").await);

        // Lists: full rewrite, then incremental append (list_hash path).
        state
            .save_list(Some("u1"), "s1", "msgs", &[json!(1), json!(2)])
            .await;
        assert_eq!(
            state.get_list(Some("u1"), "s1", "msgs").await,
            vec![json!(1), json!(2)]
        );
        state
            .save_list(Some("u1"), "s1", "msgs", &[json!(1), json!(2), json!(3)])
            .await;
        assert_eq!(
            state.get_list(Some("u1"), "s1", "msgs").await,
            vec![json!(1), json!(2), json!(3)]
        );

        // Versioning / CAS.
        assert_eq!(
            state.get_version(Some("u1"), "s1", "cas").await.unwrap(),
            (None, 0)
        );
        assert_eq!(
            state
                .save_if_version(Some("u1"), "s1", "cas", &json!(1), 0)
                .await
                .unwrap(),
            1
        );
        // Second create-if-absent must lose the race.
        assert_eq!(
            state
                .save_if_version(Some("u1"), "s1", "cas", &json!(2), 0)
                .await
                .unwrap(),
            UNVERSIONED
        );
        assert_eq!(
            state
                .save_if_version(Some("u1"), "s1", "cas", &json!(3), 1)
                .await
                .unwrap(),
            2
        );
        // Stale CAS version is rejected.
        assert_eq!(
            state
                .save_if_version(Some("u1"), "s1", "cas", &json!(4), 1)
                .await
                .unwrap(),
            UNVERSIONED
        );
        assert_eq!(state.get(Some("u1"), "s1", "cas").await, Some(json!(3)));

        // Sessions are discoverable per user.
        assert_eq!(state.list_session_ids(Some("u1")).await, vec!["s1".to_string()]);
        assert!(state.list_session_ids(Some("other")).await.is_empty());

        // Key deletion only removes that key.
        state.delete_key(Some("u1"), "s1", "cas").await.unwrap();
        assert_eq!(state.get(Some("u1"), "s1", "cas").await, None);
        assert_eq!(
            state.get(Some("u1"), "s1", "scratchpad").await,
            Some(json!({"note": "hi", "n": 3}))
        );

        // Session deletion removes the slot.
        state.delete(Some("u1"), "s1").await;
        assert!(!state.exists(Some("u1"), "s1").await);

        // Index lifecycle: `ensure_indexes` created the compound user/session index on
        // this database-scoped collection, and MongoDB's implicit `_id_` index is
        // filtered out of `list_indexes`.
        let indexes = store
            .client()
            .list_indexes(state.collection())
            .await
            .expect("list_indexes");
        assert!(
            indexes.iter().all(|i| i.name != "_id_"),
            "the implicit _id_ index must be hidden: {indexes:?}"
        );
        let compound = indexes
            .iter()
            .find(|i| {
                i.keys
                    == vec![("user_id".to_string(), 1), ("session_id".to_string(), 1)]
            })
            .unwrap_or_else(|| panic!("compound index missing: {indexes:?}"));
        assert!(!compound.unique);
        assert_eq!(compound.expire_after_seconds, None);
        assert!(!indexes.is_empty());
    })
    .await;
}

/// `BaseStore` is synchronous and drives the seams with `futures::executor::block_on`;
/// this test therefore also proves the driver bridge is safe from inside a Tokio test.
#[tokio::test]
async fn base_store_roundtrip_through_blocking_api() {
    with_store(|store, _database| async move {
        let base = store.base_store();
        let namespace = vec!["files".to_string(), "ws".to_string()];

        // Keys containing dots/dollars are escaped by the store layer.
        base.put(
            &namespace,
            "a.txt",
            store_value(json!({"content": "hello", "meta": {"a.b": "$x"}})),
        );
        let item = base.get(&namespace, "a.txt").expect("item stored");
        assert_eq!(item.version, 1);
        assert_eq!(item.value, store_value(json!({"content": "hello", "meta": {"a.b": "$x"}})));

        base.put(&namespace, "a.txt", store_value(json!({"content": "v2"})));
        assert_eq!(base.get(&namespace, "a.txt").expect("item").version, 2);

        // Optimistic concurrency: create-if-absent, then CAS.
        assert!(base.put_if_version(&namespace, "b.txt", store_value(json!({"v": 1})), 0));
        assert!(!base.put_if_version(&namespace, "b.txt", store_value(json!({"v": 2})), 0));
        assert!(base.put_if_version(&namespace, "b.txt", store_value(json!({"v": 2})), 1));
        assert!(!base.put_if_version(&namespace, "b.txt", store_value(json!({"v": 3})), 1));

        // Namespace search is key-ordered and paginated.
        let keys: Vec<String> = base.search(&namespace, 10, 0).into_iter().map(|i| i.key).collect();
        assert_eq!(keys, vec!["a.txt".to_string(), "b.txt".to_string()]);
        let page: Vec<String> = base.search(&namespace, 1, 1).into_iter().map(|i| i.key).collect();
        assert_eq!(page, vec!["b.txt".to_string()]);
        assert!(base.search(&namespace, 0, 0).is_empty());

        // A sibling namespace (not a prefix of this one) sees nothing.
        assert!(base.search(&["other".to_string()], 10, 0).is_empty());
        // Parent namespaces see nested items: `search` is prefix-scoped (directory-like).
        assert_eq!(base.search(&["files".to_string()], 10, 0).len(), 2);

        base.delete(&namespace, "a.txt");
        assert!(base.get(&namespace, "a.txt").is_none());
        assert!(base.get(&namespace, "b.txt").is_some());
    })
    .await;
}

#[tokio::test]
async fn index_options_conflict_is_migrated_on_live_server() {
    with_store(|store, database| async move {
        let client = store.client();
        let collection = scoped_collection(&database, "agentscope_index_migration");
        let spec = IndexSpec::ascending(&["_updated_at"])
            .sparse()
            .expire_after_seconds(3600);

        // Simulate a stale index from an older release: the *same* default name the
        // driver derives for `spec`, but with different options (non-sparse, 2h TTL).
        client
            .insert_raw_index(
                &collection,
                IndexInfo {
                    name: "_updated_at_1".to_string(),
                    keys: vec![("_updated_at".to_string(), 1)],
                    unique: false,
                    sparse: false,
                    expire_after_seconds: Some(7200),
                },
            )
            .await
            .expect("seed a conflicting index");

        // error 85 must surface with the crate's variant, not as a generic backend error.
        let error = client
            .create_index(&collection, &spec)
            .await
            .expect_err("conflicting options must be reported");
        assert_eq!(error.code(), Some(85), "unexpected error: {error}");

        // The migration helper drops by the *actual* name and re-creates the index.
        create_index_with_migration(client, &collection, &spec)
            .await
            .expect("migration succeeds");

        let indexes = client.list_indexes(&collection).await.unwrap();
        assert_eq!(indexes.len(), 1, "conflicting index was replaced: {indexes:?}");
        assert_eq!(indexes[0].expire_after_seconds, Some(3600));
        assert!(indexes[0].sparse);
        assert_eq!(indexes[0].name, "_updated_at_1");

        // `count` (part of the seam, unused by the stores) round-trips.
        assert_eq!(client.count(&collection, &FindFilter::default()).await.unwrap(), 0);
        let mut doc = serde_json::Map::new();
        doc.insert("_id".to_string(), json!("only"));
        client.insert_one(&collection, doc).await.unwrap();
        assert_eq!(client.count(&collection, &FindFilter::default()).await.unwrap(), 1);
        assert_eq!(
            client
                .count(&collection, &FindFilter::eq("_id", json!("only")))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            client
                .count(&collection, &FindFilter::eq("_id", json!("missing")))
                .await
                .unwrap(),
            0
        );

        // `drop_index` by the server-side name; a second drop reports "not found".
        client
            .drop_index(&collection, &indexes[0].name)
            .await
            .expect("drop_index");
        assert!(client.list_indexes(&collection).await.unwrap().is_empty());
        let error = client
            .drop_index(&collection, &indexes[0].name)
            .await
            .expect_err("dropping a missing index must fail");
        assert_eq!(error.code(), Some(27), "unexpected error: {error}");
    })
    .await;
}

#[tokio::test]
async fn snapshot_gridfs_roundtrip_and_chunking() {
    with_store(|store, _database| async move {
        let snapshots = store.snapshot_client();

        snapshots.upload("snap-small", b"hello snapshot").await.unwrap();
        assert!(snapshots.exists("snap-small").await.unwrap());
        assert_eq!(snapshots.download("snap-small").await.unwrap(), b"hello snapshot");

        // A payload spanning several 255 KB GridFS chunks.
        let big: Vec<u8> = (0..600 * 1024).map(|i| (i % 251) as u8).collect();
        snapshots.upload("snap-big", &big).await.unwrap();
        assert_eq!(snapshots.download("snap-big").await.unwrap(), big);

        // Re-upload replaces the previous revision (upsert keyed by filename).
        snapshots.upload("snap-small", b"second revision").await.unwrap();
        assert_eq!(
            snapshots.download("snap-small").await.unwrap(),
            b"second revision"
        );

        assert!(snapshots.delete("snap-small").await.unwrap());
        assert!(!snapshots.exists("snap-small").await.unwrap());
        assert!(!snapshots.delete("snap-small").await.unwrap());
        assert!(snapshots.download("snap-small").await.is_err());
    })
    .await;
}

#[tokio::test]
async fn sandbox_guard_lease_is_mutually_exclusive() {
    with_store(|store, database| async move {
        let guard = store.execution_guard();
        // The harness view (`Arc<dyn SandboxGuard>` projection) is what the sandbox
        // manager drives; it probes the lock document through the blocking bridge.
        let view = store.sandbox_execution_guard();
        let key = SandboxIsolationKey::new("user", "u-live");

        let mut lease = guard.try_enter(&key).await.expect("first acquisition succeeds");
        assert!(view.is_held(&key));

        // A second guard on the same database/lock collection must contend.
        let contender = MongodbSandboxExecutionGuard::builder(store.client().clone())
            .database_name(database)
            .lock_timeout(Duration::from_millis(300))
            .lease_ttl(Duration::from_secs(30))
            .retry_interval(Duration::from_millis(50))
            .build();
        let error = contender.try_enter(&key).await.expect_err("lock is held");
        assert!(error.to_string().contains("Timed out"), "unexpected error: {error}");

        lease.release_async().await;
        assert!(!view.is_held(&key));

        // The lock is free again; the harness view acquires and releases it too.
        let sandbox_lease = view.try_enter(key.clone()).await;
        assert!(!sandbox_lease.is_noop());
        assert!(view.is_held(&key));
        view.release(&key).await;
        assert!(!view.is_held(&key));
    })
    .await;
}
