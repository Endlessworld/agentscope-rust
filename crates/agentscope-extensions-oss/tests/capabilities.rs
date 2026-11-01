//! Default behaviour of the *optional* [`OssClient`] capabilities.
//!
//! `delete_many` and `put_multipart` were added after the trait shipped, so their
//! defaults must keep every existing backend working: a correct (if slower) result
//! instead of a panic, and an explicit `Unsupported` instead of a silent success.

use agentscope_extensions_oss::{InMemoryOssClient, OssClient, OssError};

#[test]
fn delete_many_defaults_to_one_delete_per_key() {
    let client = InMemoryOssClient::new();
    client.put("a.json", b"1").unwrap();
    client.put("b.json", b"2").unwrap();

    client
        .delete_many(&["a.json".to_string(), "b.json".to_string()])
        .unwrap();

    assert!(client.is_empty());
}

#[test]
fn delete_many_of_nothing_is_a_no_op() {
    let client = InMemoryOssClient::new();
    client.put("a.json", b"1").unwrap();

    client.delete_many(&[]).unwrap();

    assert_eq!(client.len(), 1);
}

#[test]
fn put_multipart_is_opt_in() {
    let client = InMemoryOssClient::new();

    match client.put_multipart("big.tar", b"payload", 5 * 1024 * 1024) {
        Err(OssError::Unsupported(capability)) => assert_eq!(capability, "multipart upload"),
        other => panic!("expected OssError::Unsupported, got {other:?}"),
    }
    assert!(client.is_empty());
}
