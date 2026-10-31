//! OSS-02 / OSS-03 over the real S3 backend: the stores must translate compare-and-set
//! into backend-evaluated preconditions.
//!
//! `httpmock` plays the bucket, so the assertions can look at the wire: a CAS must send
//! `If-Match: <etag>` (or `If-None-Match: *` for create-if-absent) and must treat `412` as
//! a lost race rather than as a backend failure — and it must not write the payload when
//! it loses.

#![cfg(feature = "s3")]

use std::sync::Arc;

use agentscope_core::state::store::AgentStateStore;
use agentscope_core::state::UNVERSIONED;
use agentscope_extensions_oss::{OssAgentStateStore, OssClient, OssStore, S3OssClient};
use agentscope_harness::filesystem::remote::store::BaseStore;
use httpmock::prelude::*;

const BUCKET: &str = "test-bucket";
const STATE_PREFIX: &str = "test/state/";
const STORE_PREFIX: &str = "test/store/";

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

fn value(text: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("foo".to_string(), serde_json::json!(text));
    map
}

fn state_path() -> String {
    format!("/{BUCKET}/{STATE_PREFIX}alice/s1/k.json")
}

#[tokio::test]
async fn state_store_conditional_write_sends_if_match_and_advances_the_version() {
    let server = MockServer::start();
    let path = state_path();

    let read = server.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(200)
            .header("etag", "\"e1\"")
            .header("content-type", "application/json")
            .body("{\"turn\":1}");
    });
    let write = server.mock(|when, then| {
        when.method(PUT)
            .path(path.clone())
            .header("if-match", "\"e1\"");
        then.status(200).header("etag", "\"e2\"");
    });

    let store = state_store(&server);
    assert!(AgentStateStore::supports_versioning(&store));

    let loaded = AgentStateStore::get_versioned(&store, Some("alice"), "s1", "k").await;
    assert_eq!(loaded.value, Some(serde_json::json!({"turn": 1})));
    assert_ne!(loaded.version, 0);

    let written = AgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "k",
        serde_json::json!({"turn": 2}),
        loaded.version,
    )
    .await;

    assert_ne!(written, UNVERSIONED, "CAS must commit on the matching ETag");
    assert_ne!(
        written, loaded.version,
        "the token must follow the new ETag"
    );
    read.assert_hits(2);
    write.assert_hits(1);
}

#[tokio::test]
async fn state_store_reports_a_lost_race_as_unversioned() {
    let server = MockServer::start();
    let path = state_path();

    let read = server.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(200)
            .header("etag", "\"e1\"")
            .body("{\"turn\":1}");
    });
    let rejected = server.mock(|when, then| {
        when.method(PUT).path(path.clone());
        then.status(412).body(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>PreconditionFailed</Code></Error>",
        );
    });

    let store = state_store(&server);
    let loaded = AgentStateStore::get_versioned(&store, Some("alice"), "s1", "k").await;
    let written = AgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "k",
        serde_json::json!({"turn": 2}),
        loaded.version,
    )
    .await;

    assert_eq!(written, UNVERSIONED);
    read.assert_hits(2);
    rejected.assert_hits(1);
}

#[tokio::test]
async fn state_store_create_if_absent_uses_if_none_match() {
    let server = MockServer::start();
    let path = state_path();

    let created = server.mock(|when, then| {
        when.method(PUT)
            .path(path.clone())
            .header("if-none-match", "*");
        then.status(200).header("etag", "\"new\"");
    });

    let store = state_store(&server);
    let written = AgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "k",
        serde_json::json!({"turn": 1}),
        0,
    )
    .await;

    assert_ne!(written, UNVERSIONED);
    created.assert_hits(1);
}

#[tokio::test]
async fn base_store_cas_anchors_on_the_version_sidecar() {
    let server = MockServer::start();
    let version_path = format!("/{BUCKET}/{STORE_PREFIX}ns1/k.version");
    let data_path = format!("/{BUCKET}/{STORE_PREFIX}ns1/k.json");

    let read_version = server.mock(|when, then| {
        when.method(GET).path(version_path.clone());
        then.status(200).header("etag", "\"v1\"").body("3");
    });
    let bump_version = server.mock(|when, then| {
        when.method(PUT)
            .path(version_path.clone())
            .header("if-match", "\"v1\"");
        then.status(200).header("etag", "\"v2\"");
    });
    let write_payload = server.mock(|when, then| {
        when.method(PUT).path(data_path.clone());
        then.status(200);
    });

    let store = base_store(&server);
    assert!(store.supports_atomic_session_storage());
    assert!(store.put_if_version(&ns(), "k", value("next"), 3));

    read_version.assert_hits(1);
    bump_version.assert_hits(1);
    write_payload.assert_hits(1);
}

#[tokio::test]
async fn base_store_cas_loser_does_not_write_the_payload() {
    let server = MockServer::start();
    let version_path = format!("/{BUCKET}/{STORE_PREFIX}ns1/k.version");
    let data_path = format!("/{BUCKET}/{STORE_PREFIX}ns1/k.json");

    let read_version = server.mock(|when, then| {
        when.method(GET).path(version_path.clone());
        then.status(200).header("etag", "\"v1\"").body("3");
    });
    let bump_version = server.mock(|when, then| {
        when.method(PUT).path(version_path.clone());
        then.status(412).body(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>PreconditionFailed</Code></Error>",
        );
    });
    let write_payload = server.mock(|when, then| {
        when.method(PUT).path(data_path.clone());
        then.status(200);
    });

    let store = base_store(&server);
    assert!(!store.put_if_version(&ns(), "k", value("next"), 3));

    read_version.assert_hits(1);
    bump_version.assert_hits(1);
    write_payload.assert_hits(0);
}

#[tokio::test]
async fn base_store_cas_rejects_a_stale_expected_version_without_a_write() {
    let server = MockServer::start();
    let version_path = format!("/{BUCKET}/{STORE_PREFIX}ns1/k.version");

    let read_version = server.mock(|when, then| {
        when.method(GET).path(version_path.clone());
        then.status(200).header("etag", "\"v1\"").body("3");
    });
    let any_write = server.mock(|when, then| {
        when.method(PUT);
        then.status(200);
    });

    let store = base_store(&server);
    assert!(!store.put_if_version(&ns(), "k", value("next"), 7));

    read_version.assert_hits(1);
    any_write.assert_hits(0);
}
