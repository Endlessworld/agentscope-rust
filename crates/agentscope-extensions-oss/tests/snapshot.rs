//! Tests for [`OssSnapshotSpec`] / [`OssRemoteSnapshotClient`].

mod common;

use std::sync::Arc;

use agentscope_extensions_oss::{OssClient, OssError, OssRemoteSnapshotClient, OssSnapshotSpec};
use agentscope_harness::agent::sandbox::{
    RemoteSnapshotClient, SandboxSnapshot, SandboxSnapshotSpec,
};

#[tokio::test]
async fn upload_download_exists_roundtrip() {
    let client = common::mem_client();
    let snap = OssRemoteSnapshotClient::new(client.clone(), "bucket", Some("snap/")).unwrap();

    assert!(!snap.exists("s1").await.unwrap());
    snap.upload("s1", b"archive-bytes").await.unwrap();
    assert!(snap.exists("s1").await.unwrap());
    assert!(client.exists("snap/s1.tar").unwrap());
    assert_eq!(snap.download("s1").await.unwrap(), b"archive-bytes");
}

#[tokio::test]
async fn download_missing_fails() {
    let client = common::mem_client();
    let snap = OssRemoteSnapshotClient::new(client, "bucket", Some("snap/")).unwrap();
    assert!(snap.download("missing").await.is_err());
}

#[test]
fn client_rejects_blank_bucket() {
    let client = common::mem_client();
    assert!(matches!(
        OssRemoteSnapshotClient::new(client, "   ", None).unwrap_err(),
        OssError::BlankBucket
    ));
}

#[tokio::test]
async fn rejects_unsafe_snapshot_id() {
    let client = common::mem_client();
    let snap = OssRemoteSnapshotClient::new(client, "bucket", None).unwrap();
    assert!(snap.upload("../evil", b"x").await.is_err());
    assert!(snap.exists("a/b").await.is_err());
}

#[test]
fn spec_builds_remote_snapshot() {
    let client = common::mem_client();
    let spec = OssSnapshotSpec::new(client, "bucket", Some("snap/")).unwrap();
    let snapshot: Arc<dyn SandboxSnapshot> = spec.build("session-1").unwrap();
    assert_eq!(snapshot.id(), "session-1");
    assert_eq!(snapshot.snapshot_type(), "remote");
}

#[test]
fn spec_rejects_unsafe_id() {
    let client = common::mem_client();
    let spec = OssSnapshotSpec::new(client, "bucket", None).unwrap();
    assert!(spec.build("../bad").is_err());
}

#[tokio::test]
async fn spec_snapshot_persists_and_restores() {
    let client = common::mem_client();
    let spec = OssSnapshotSpec::new(client, "bucket", Some("snap/")).unwrap();
    let snapshot = spec.build("session-1").unwrap();
    snapshot.persist(b"data").await.unwrap();
    assert!(snapshot.is_restorable().await.unwrap());
    assert_eq!(snapshot.restore().await.unwrap(), b"data");
}

#[allow(unused_imports)]
use common::as_dyn as _as_dyn;
