//! OSS-08: the S3 backend driven **through the stores** (`OssAgentStateStore`,
//! `OssStore`, `OssRemoteSnapshotClient`) instead of through the client alone.
//!
//! `tests/s3.rs` exercises `S3OssClient` method by method, which is exactly why the
//! nested-runtime bug (OSS-01) went unnoticed for a while: the stores call the synchronous
//! client from `async fn` bodies. Every case below goes through a store, and the
//! `#[tokio::test]` ones do it from inside a Tokio runtime.
//!
//! A real S3-compatible backend (MinIO / Ceph / AWS S3 / Aliyun OSS-S3) can be exercised
//! too: set `AGENTSCOPE_OSS_S3_ENDPOINT`, `AGENTSCOPE_OSS_S3_BUCKET`,
//! `AGENTSCOPE_OSS_S3_ACCESS_KEY`, `AGENTSCOPE_OSS_S3_SECRET_KEY` (and optionally
//! `AGENTSCOPE_OSS_S3_REGION`) and run with `--features s3`. Without those variables the
//! real-backend case reports that it was skipped and returns early.

#![cfg(feature = "s3")]

use std::sync::Arc;

use agentscope_core::state::store::AgentStateStore;
use agentscope_extensions_oss::{
    OssAgentStateStore, OssClient, OssRemoteSnapshotClient, OssStore, S3OssClient,
};
use agentscope_harness::agent::sandbox::RemoteSnapshotClient;
use agentscope_harness::filesystem::remote::store::{BaseStore, StoreValue};
use httpmock::prelude::*;

const BUCKET: &str = "test-bucket";
const STATE_PREFIX: &str = "test/state/";
const STORE_PREFIX: &str = "test/store/";
const SNAPSHOT_PREFIX: &str = "test/snapshots/";

fn client(server: &MockServer) -> Arc<dyn OssClient> {
    Arc::new(
        S3OssClient::builder()
            .bucket(BUCKET)
            .region("us-east-1")
            .endpoint(server.base_url())
            .access_key("test-access-key")
            .secret_key("test-secret-key")
            .env_credentials(false)
            .build()
            .expect("S3 client against the mock endpoint"),
    )
}

fn state_store(server: &MockServer) -> OssAgentStateStore {
    OssAgentStateStore::builder()
        .client(client(server))
        .bucket_name(BUCKET)
        .key_prefix(STATE_PREFIX)
        .build()
        .unwrap()
}

fn base_store(server: &MockServer) -> OssStore {
    OssStore::builder()
        .client(client(server))
        .bucket_name(BUCKET)
        .key_prefix(STORE_PREFIX)
        .build()
        .unwrap()
}

fn ns() -> Vec<String> {
    vec!["ns1".to_string()]
}

fn value(text: &str) -> StoreValue {
    let mut map = serde_json::Map::new();
    map.insert("foo".to_string(), serde_json::json!(text));
    map
}

fn state_path(key: &str) -> String {
    format!("/{BUCKET}/{STATE_PREFIX}alice/s1/{key}")
}

fn store_path(key: &str) -> String {
    format!("/{BUCKET}/{STORE_PREFIX}ns1/{key}")
}

/// A `ListObjectsV2` page body.
fn list_xml(keys: &[&str], truncated: bool, next_token: Option<&str>) -> String {
    let contents = keys
        .iter()
        .map(|key| {
            format!(
                "<Contents>\
                   <Key>{key}</Key>\
                   <LastModified>2026-11-01T00:00:00.000Z</LastModified>\
                   <ETag>\"etag\"</ETag>\
                   <Size>1</Size>\
                   <StorageClass>STANDARD</StorageClass>\
                 </Contents>"
            )
        })
        .collect::<String>();
    let next = next_token
        .map(|token| format!("<NextContinuationToken>{token}</NextContinuationToken>"))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
           <Name>{BUCKET}</Name>\
           <KeyCount>{}</KeyCount>\
           <IsTruncated>{truncated}</IsTruncated>\
           {next}\
           {contents}\
         </ListBucketResult>",
        keys.len()
    )
}

#[tokio::test]
async fn state_store_single_value_round_trip_over_s3() {
    let server = MockServer::start();
    let path = state_path("k.json");

    let put = server.mock(|when, then| {
        when.method(PUT).path(path.clone());
        then.status(200).header("etag", "\"e1\"");
    });
    let get = server.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(200)
            .header("etag", "\"e1\"")
            .header("content-type", "application/json")
            .body("{\"turn\":1}");
    });

    let store = state_store(&server);
    store
        .save(Some("alice"), "s1", "k", serde_json::json!({"turn": 1}))
        .await;
    assert_eq!(
        store.get(Some("alice"), "s1", "k").await,
        Some(serde_json::json!({"turn": 1}))
    );

    put.assert_hits(1);
    get.assert_hits(1);
}

#[tokio::test]
async fn state_store_save_list_writes_payload_and_hash_over_s3() {
    let server = MockServer::start();
    let list_path = state_path("m.list.json");
    let hash_path = state_path("m.list.hash");

    // The two reads that precede the rewrite are unmatched, so the backend answers 404 and
    // the store sees "nothing stored yet" (same as a fresh key).
    let list_put = server.mock(|when, then| {
        when.method(PUT).path(list_path.clone());
        then.status(200);
    });
    let hash_put = server.mock(|when, then| {
        when.method(PUT).path(hash_path.clone());
        then.status(200);
    });

    let store = state_store(&server);
    store
        .save_list(
            Some("alice"),
            "s1",
            "m",
            &[serde_json::json!(1), serde_json::json!(2)],
        )
        .await;

    list_put.assert_hits(1);
    hash_put.assert_hits(1);
}

#[tokio::test]
async fn state_store_get_list_reads_the_payload_over_s3() {
    let server = MockServer::start();
    let get = server.mock(|when, then| {
        when.method(GET).path(state_path("m.list.json"));
        then.status(200).body("[1,2,3]");
    });

    let store = state_store(&server);
    assert_eq!(
        store.get_list(Some("alice"), "s1", "m").await,
        vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3)
        ]
    );
    get.assert_hits(1);
}

#[tokio::test]
async fn state_store_session_delete_batches_keys_over_s3() {
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(GET)
            .path(format!("/{BUCKET}/"))
            .query_param("list-type", "2")
            .query_param("prefix", format!("{STATE_PREFIX}alice/s1/"));
        then.status(200).body(list_xml(
            &[
                "test/state/alice/s1/a.json",
                "test/state/alice/s1/b.json",
                "test/state/alice/s1/c.list.json",
                "test/state/alice/s1/c.list.hash",
            ],
            false,
            None,
        ));
    });
    let bulk = server.mock(|when, then| {
        when.method(POST)
            .query_param_exists("delete")
            .body_contains("test/state/alice/s1/a.json")
            .body_contains("test/state/alice/s1/c.list.hash");
        then.status(200).body("<DeleteResult></DeleteResult>");
    });
    let per_key = server.mock(|when, then| {
        when.method(DELETE);
        then.status(204);
    });

    let store = state_store(&server);
    store.delete(Some("alice"), "s1").await;

    list.assert_hits(1);
    // One bulk request for all four objects instead of four DELETEs.
    bulk.assert_hits(1);
    per_key.assert_hits(0);
}

#[test]
fn base_store_put_writes_payload_and_version_over_s3() {
    let server = MockServer::start();
    let data_put = server.mock(|when, then| {
        when.method(PUT)
            .path(store_path("k.json"))
            .body_contains("\"foo\":\"bar\"");
        then.status(200);
    });
    let version_put = server.mock(|when, then| {
        when.method(PUT).path(store_path("k.version"));
        then.status(200);
    });

    let store = base_store(&server);
    store.put(&ns(), "k", value("bar"));

    data_put.assert_hits(1);
    version_put.assert_hits(1);
}

#[test]
fn base_store_get_reads_payload_and_version_over_s3() {
    let server = MockServer::start();
    let data_get = server.mock(|when, then| {
        when.method(GET).path(store_path("k.json"));
        then.status(200).body("{\"foo\":\"bar\"}");
    });
    let version_get = server.mock(|when, then| {
        when.method(GET).path(store_path("k.version"));
        then.status(200).body("7");
    });

    let store = base_store(&server);
    let item = store.get(&ns(), "k").expect("item");
    assert_eq!(item.key, "k");
    assert_eq!(item.value.get("foo"), Some(&serde_json::json!("bar")));
    assert_eq!(item.version, 7);

    data_get.assert_hits(1);
    version_get.assert_hits(1);
}

#[tokio::test]
async fn base_store_create_if_absent_over_s3() {
    let server = MockServer::start();
    let create = server.mock(|when, then| {
        when.method(PUT)
            .path(store_path("k.json"))
            .header("if-none-match", "*");
        then.status(200).header("etag", "\"e1\"");
    });
    let version_put = server.mock(|when, then| {
        when.method(PUT).path(store_path("k.version"));
        then.status(200);
    });

    let store = base_store(&server);
    assert!(store.put_if_version(&ns(), "k", value("first"), 0));

    create.assert_hits(1);
    version_put.assert_hits(1);
}

#[tokio::test]
async fn base_store_create_if_absent_conflict_over_s3() {
    let server = MockServer::start();
    let create = server.mock(|when, then| {
        when.method(PUT)
            .path(store_path("k.json"))
            .header("if-none-match", "*");
        then.status(412).body(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>PreconditionFailed</Code></Error>",
        );
    });
    let version_put = server.mock(|when, then| {
        when.method(PUT).path(store_path("k.version"));
        then.status(200);
    });

    let store = base_store(&server);
    assert!(!store.put_if_version(&ns(), "k", value("first"), 0));

    create.assert_hits(1);
    // A lost create must not bump the version.
    version_put.assert_hits(0);
}

#[tokio::test]
async fn base_store_search_bounds_requests_and_follows_tokens_over_s3() {
    let server = MockServer::start();
    let list_path = format!("/{BUCKET}/");

    // `search(limit = 2, offset = 2)` needs four items = eight keys; each item is a
    // payload plus its `.version` sidecar.
    let second_page = server.mock(|when, then| {
        when.method(GET)
            .path(list_path.clone())
            .query_param("list-type", "2")
            .query_param("prefix", format!("{STORE_PREFIX}ns1/"))
            .query_param("max-keys", "6")
            .query_param("continuation-token", "t1");
        then.status(200).body(list_xml(
            &[
                "test/store/ns1/k01.json",
                "test/store/ns1/k01.version",
                "test/store/ns1/k02.json",
                "test/store/ns1/k02.version",
                "test/store/ns1/k03.json",
                "test/store/ns1/k03.version",
            ],
            false,
            None,
        ));
    });
    let first_page = server.mock(|when, then| {
        when.method(GET)
            .path(list_path.clone())
            .query_param("list-type", "2")
            .query_param("prefix", format!("{STORE_PREFIX}ns1/"))
            .query_param("max-keys", "8");
        then.status(200).body(list_xml(
            &["test/store/ns1/k00.json", "test/store/ns1/k00.version"],
            true,
            Some("t1"),
        ));
    });

    for key in ["k02", "k03"] {
        server.mock(|when, then| {
            when.method(GET).path(store_path(&format!("{key}.json")));
            then.status(200).body("{\"foo\":\"bar\"}");
        });
        server.mock(|when, then| {
            when.method(GET).path(store_path(&format!("{key}.version")));
            then.status(200).body("1");
        });
    }

    let store = base_store(&server);
    let keys: Vec<String> = store
        .search(&ns(), 2, 2)
        .into_iter()
        .map(|item| item.key)
        .collect();

    assert_eq!(keys, vec!["k02", "k03"]);
    first_page.assert_hits(1);
    second_page.assert_hits(1);
}

#[tokio::test]
async fn snapshot_upload_and_download_over_s3() {
    let server = MockServer::start();
    let path = format!("/{BUCKET}/{SNAPSHOT_PREFIX}snap-1.tar");

    let upload = server.mock(|when, then| {
        when.method(PUT).path(path.clone()).body("tar-bytes");
        then.status(200);
    });
    let download = server.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(200)
            .header("content-type", "application/x-tar")
            .body("tar-bytes");
    });

    let snapshots = OssRemoteSnapshotClient::new(
        client(&server),
        BUCKET,
        Some(SNAPSHOT_PREFIX),
    )
    .unwrap();

    snapshots.upload("snap-1", b"tar-bytes").await.unwrap();
    assert_eq!(snapshots.download("snap-1").await.unwrap(), b"tar-bytes");
    assert!(snapshots.exists("snap-1").await.is_ok());

    upload.assert_hits(1);
    download.assert_hits(1);
}

/// A real backend built from the `AGENTSCOPE_OSS_S3_*` environment, if configured.
fn real_backend() -> Option<S3OssClient> {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.trim().is_empty());
    let endpoint = env("AGENTSCOPE_OSS_S3_ENDPOINT")?;
    let bucket = env("AGENTSCOPE_OSS_S3_BUCKET")?;
    let access_key = env("AGENTSCOPE_OSS_S3_ACCESS_KEY")?;
    let secret_key = env("AGENTSCOPE_OSS_S3_SECRET_KEY")?;
    let region = env("AGENTSCOPE_OSS_S3_REGION").unwrap_or_else(|| "us-east-1".to_string());

    S3OssClient::builder()
        .bucket(bucket)
        .region(region)
        .endpoint(endpoint)
        .access_key(access_key)
        .secret_key(secret_key)
        .env_credentials(false)
        .build()
        .ok()
}

/// Real-backend check for the OSS-02/03 atomicity claim: a stale precondition must come
/// back as `412` (a lost race) instead of an error or a successful overwrite.
#[tokio::test]
async fn real_backend_reports_a_lost_precondition_as_a_conflict() {
    let Some(client) = real_backend() else {
        eprintln!(
            "skipped real-backend precondition test: set AGENTSCOPE_OSS_S3_ENDPOINT, \
             AGENTSCOPE_OSS_S3_BUCKET, AGENTSCOPE_OSS_S3_ACCESS_KEY and \
             AGENTSCOPE_OSS_S3_SECRET_KEY to run it"
        );
        return;
    };

    let bucket = client.bucket_name().to_string();
    let store = OssStore::builder()
        .client(Arc::new(client))
        .bucket_name(bucket)
        .key_prefix("agentscope/oss-precondition-probe/")
        .build()
        .unwrap();
    let ns = vec!["cas".to_string()];

    assert!(store.put_if_version(&ns, "probe", value("first"), 0));
    // The key now exists: a second create-if-absent must lose.
    assert!(!store.put_if_version(&ns, "probe", value("second"), 0));
    // A stale version must lose as well.
    assert!(!store.put_if_version(&ns, "probe", value("third"), 99));
    // The current version still wins.
    let current = store.get(&ns, "probe").expect("probe").version;
    assert!(store.put_if_version(&ns, "probe", value("fourth"), current));

    store.delete(&ns, "probe");
}
