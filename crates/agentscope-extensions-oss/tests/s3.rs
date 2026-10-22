//! End-to-end tests for the generic S3 backend over real HTTP.
//!
//! `httpmock` starts a local HTTP server; `S3OssClient` is pointed at it via a
//! custom endpoint (path-style). No cloud account or network access is needed.
//!
//! Before the `s3` feature / `S3OssClient` existed these tests could not even
//! compile (`agentscope_extensions_oss::S3OssClient` was unresolved), so every
//! case below is a genuine regression guard rather than a tautology.

#![cfg(feature = "s3")]

use agentscope_extensions_oss::{OssClient, OssError, S3OssClient};
use httpmock::prelude::*;
use httpmock::Method::HEAD;

const BUCKET: &str = "test-bucket";

fn test_client(server: &MockServer) -> S3OssClient {
    S3OssClient::builder()
        .bucket(BUCKET)
        .region("us-east-1")
        .endpoint(server.base_url())
        .access_key("test-access-key")
        .secret_key("test-secret-key")
        .env_credentials(false)
        .build()
        .expect("S3OssClient should build against the mock endpoint")
}

fn list_xml(keys: &[&str], truncated: bool, next_token: Option<&str>) -> String {
    let contents = keys
        .iter()
        .map(|key| {
            format!(
                "<Contents>\
                   <Key>{key}</Key>\
                   <LastModified>2026-10-22T00:00:00.000Z</LastModified>\
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
           <Prefix>logs/</Prefix>\
           <KeyCount>{}</KeyCount>\
           <MaxKeys>2</MaxKeys>\
           <IsTruncated>{truncated}</IsTruncated>\
           {next}\
           {contents}\
         </ListBucketResult>",
        keys.len()
    )
}

#[test]
fn put_then_get_round_trip() {
    let server = MockServer::start();
    let object_path = format!("/{BUCKET}/folder/object.bin");

    let put_mock = server.mock(|when, then| {
        when.method(PUT).path(object_path.clone());
        then.status(200);
    });
    let get_mock = server.mock(|when, then| {
        when.method(GET).path(object_path.clone());
        then.status(200)
            .header("content-type", "application/octet-stream")
            .body("hello s3");
    });

    let client = test_client(&server);
    client.put("folder/object.bin", b"hello s3").expect("put");

    let fetched = client
        .get("folder/object.bin")
        .expect("get")
        .expect("object");
    assert_eq!(fetched, b"hello s3".to_vec());

    put_mock.assert();
    get_mock.assert();
}

#[test]
fn get_missing_object_returns_none() {
    let server = MockServer::start();
    let missing_path = format!("/{BUCKET}/missing.json");

    let mock = server.mock(|when, then| {
        when.method(GET).path(missing_path.clone());
        then.status(404).body(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>NoSuchKey</Code><Message>missing</Message></Error>",
        );
    });

    let client = test_client(&server);
    assert_eq!(client.get("missing.json").expect("get"), None);
    mock.assert();
}

#[test]
fn exists_uses_head_and_reports_presence() {
    let server = MockServer::start();
    let present_path = format!("/{BUCKET}/present.txt");
    let absent_path = format!("/{BUCKET}/absent.txt");

    let present = server.mock(|when, then| {
        when.method(HEAD).path(present_path.clone());
        then.status(200);
    });
    let absent = server.mock(|when, then| {
        when.method(HEAD).path(absent_path.clone());
        then.status(404);
    });

    let client = test_client(&server);
    assert!(client.exists("present.txt").expect("head present"));
    assert!(!client.exists("absent.txt").expect("head absent"));

    present.assert();
    absent.assert();
}

#[test]
fn delete_is_idempotent() {
    let server = MockServer::start();
    let present_path = format!("/{BUCKET}/present.txt");
    let absent_path = format!("/{BUCKET}/absent.txt");

    let present = server.mock(|when, then| {
        when.method(DELETE).path(present_path.clone());
        then.status(204);
    });
    let absent = server.mock(|when, then| {
        when.method(DELETE).path(absent_path.clone());
        then.status(404).body(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>NoSuchKey</Code><Message>missing</Message></Error>",
        );
    });

    let client = test_client(&server);
    client.delete("present.txt").expect("first delete");
    client.delete("present.txt").expect("second delete");
    client.delete("absent.txt").expect("delete of missing key");

    present.assert_hits(2);
    absent.assert();
}

#[test]
fn list_page_uses_max_keys_and_continuation_token() {
    let server = MockServer::start();
    // `rust-s3` appends the (here empty) list path to the bucket URL, leaving a
    // trailing slash: `/bucket/`.
    let list_path = format!("/{BUCKET}/");

    // Create the second-page mock first: httpmock evaluates mocks by ascending
    // id, and both mocks share path/prefix. A page-1 request has no
    // `continuation-token`, so it cannot match this one.
    let second_page = server.mock(|when, then| {
        when.method(GET)
            .path(list_path.clone())
            .query_param("list-type", "2")
            .query_param("prefix", "logs/")
            .query_param("max-keys", "2")
            .query_param("continuation-token", "token-page-2");
        then.status(200)
            .header("content-type", "application/xml")
            .body(list_xml(&["logs/c"], false, None));
    });
    let first_page = server.mock(|when, then| {
        when.method(GET)
            .path(list_path.clone())
            .query_param("list-type", "2")
            .query_param("prefix", "logs/")
            .query_param("max-keys", "2");
        then.status(200)
            .header("content-type", "application/xml")
            .body(list_xml(&["logs/a", "logs/b"], true, Some("token-page-2")));
    });

    let client = test_client(&server);

    let page1 = client.list_page("logs/", 2, None).expect("page 1");
    let keys1: Vec<&str> = page1.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys1, vec!["logs/a", "logs/b"]);
    assert!(page1.truncated);
    assert_eq!(page1.next_token.as_deref(), Some("token-page-2"));

    let page2 = client
        .list_page("logs/", 2, page1.next_token.as_deref())
        .expect("page 2");
    let keys2: Vec<&str> = page2.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys2, vec!["logs/c"]);
    assert!(!page2.truncated);
    assert_eq!(page2.next_token, None);

    first_page.assert();
    second_page.assert();
}

#[test]
fn forbidden_response_maps_to_typed_backend_error() {
    let server = MockServer::start();
    let path = format!("/{BUCKET}/protected");

    let mock = server.mock(|when, then| {
        when.method(PUT).path(path.clone());
        then.status(403).body(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>AccessDenied</Code><Message>denied</Message></Error>",
        );
    });

    let client = test_client(&server);
    let error = client.put("protected", b"nope").expect_err("403 must fail");
    let rendered = error.to_string();
    assert!(
        matches!(
            error,
            OssError::S3(_) | OssError::S3Http { status: 403, .. }
        ),
        "expected a typed S3 error, got: {rendered}"
    );
    assert!(
        rendered.contains("403") || rendered.contains("AccessDenied"),
        "error should retain status/body information: {rendered}"
    );
    mock.assert();
}

#[test]
fn configuration_validation_rejects_missing_bucket_and_credentials() {
    let missing_bucket = S3OssClient::builder()
        .access_key("ak")
        .secret_key("sk")
        .env_credentials(false)
        .build()
        .expect_err("blank/missing bucket must fail");
    assert!(matches!(missing_bucket, OssError::BlankBucket));

    let missing_credentials = S3OssClient::builder()
        .bucket("bucket")
        .env_credentials(false)
        .build()
        .expect_err("missing credentials must fail");
    assert!(matches!(missing_credentials, OssError::MissingCredentials));

    let partial_credentials = S3OssClient::builder()
        .bucket("bucket")
        .access_key("ak")
        .env_credentials(false)
        .build()
        .expect_err("secret key is required too");
    assert!(matches!(partial_credentials, OssError::MissingCredentials));
}
