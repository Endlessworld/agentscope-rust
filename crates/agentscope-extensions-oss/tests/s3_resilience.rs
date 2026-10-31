//! OSS-04 regression: request timeouts and retry/backoff for the S3 backend.
//!
//! Before the executor seam gained a deadline and a retry loop, a hung backend parked the
//! calling thread forever and the first transient `5xx` failed the operation. The cases
//! below drive real HTTP: `httpmock` for the timeout, and a tiny hand-written endpoint that
//! answers `500` twice before succeeding (`httpmock` cannot express a request *sequence*).

#![cfg(feature = "s3")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_extensions_oss::{OssClient, OssError, S3OssClient};
use httpmock::prelude::*;

const BUCKET: &str = "test-bucket";

fn builder(server: &MockServer) -> agentscope_extensions_oss::S3OssClientBuilder {
    S3OssClient::builder()
        .bucket(BUCKET)
        .region("us-east-1")
        .endpoint(server.base_url())
        .access_key("test-access-key")
        .secret_key("test-secret-key")
        .env_credentials(false)
}

/// Minimal S3-ish endpoint: the first `failures` requests answer `500`, then `200`.
fn flaky_server(failures: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n".as_slice()) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            let (status, body) = if attempt < failures {
                (
                    "500 Internal Server Error",
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                     <Error><Code>InternalError</Code></Error>",
                )
            } else {
                ("200 OK", "recovered")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: \"e{attempt}\"\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (format!("http://{address}"), hits)
}

#[test]
fn retries_transient_server_errors_until_the_request_succeeds() {
    let (endpoint, hits) = flaky_server(2);
    let client = S3OssClient::builder()
        .bucket(BUCKET)
        .endpoint(endpoint)
        .access_key("ak")
        .secret_key("sk")
        .env_credentials(false)
        .max_retries(3)
        .build()
        .expect("client");

    let body = client
        .get("flaky.txt")
        .expect("retries must recover from two 500s")
        .expect("object");

    assert_eq!(body, b"recovered".to_vec());
    assert!(
        hits.load(Ordering::SeqCst) >= 3,
        "expected at least three attempts, saw {}",
        hits.load(Ordering::SeqCst)
    );
}

#[test]
fn a_client_without_retries_gives_up_on_a_server_error() {
    let (endpoint, _hits) = flaky_server(usize::MAX);
    let client = S3OssClient::builder()
        .bucket(BUCKET)
        .endpoint(endpoint)
        .access_key("ak")
        .secret_key("sk")
        .env_credentials(false)
        .max_retries(0)
        .build()
        .expect("client");

    let error = client.get("flaky.txt").expect_err("500 must surface");
    assert!(
        error.is_retryable(),
        "5xx must be classified retryable: {error}"
    );
}

#[test]
fn a_rejected_request_is_not_retried() {
    let server = MockServer::start();
    let path = format!("/{BUCKET}/protected.txt");
    let mock = server.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(403).body(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>AccessDenied</Code></Error>",
        );
    });

    let error = builder(&server)
        .max_retries(3)
        .build()
        .expect("client")
        .get("protected.txt")
        .expect_err("403 must fail");

    assert!(!error.is_retryable(), "4xx must not be retried: {error}");
    mock.assert_hits(1);
}

#[test]
fn a_slow_backend_fails_with_a_typed_timeout_instead_of_hanging() {
    let server = MockServer::start();
    let path = format!("/{BUCKET}/slow.txt");
    let mock = server.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(200)
            .delay(Duration::from_millis(2_000))
            .body("too late");
    });

    let client = builder(&server)
        .timeout(Duration::from_millis(50))
        .max_retries(0)
        .build()
        .expect("client");

    let started = Instant::now();
    let error = client.get("slow.txt").expect_err("must time out");
    let elapsed = started.elapsed();

    assert!(matches!(error, OssError::Timeout(_)), "got {error:?}");
    assert!(error.is_retryable(), "a timeout is retryable");
    assert!(
        elapsed < Duration::from_secs(1),
        "timeout must fire promptly, took {elapsed:?}"
    );
    assert_eq!(mock.hits(), 1);
}

#[test]
fn builder_defaults_and_overrides_are_visible() {
    let server = MockServer::start();
    let defaults = builder(&server).build().expect("client");
    assert_eq!(
        defaults.timeout(),
        agentscope_extensions_oss::s3::DEFAULT_TIMEOUT
    );
    assert_eq!(
        defaults.max_retries(),
        agentscope_extensions_oss::s3::DEFAULT_MAX_RETRIES
    );

    let tuned = builder(&server)
        .timeout(Duration::from_millis(250))
        .max_retries(0)
        .build()
        .expect("client");
    assert_eq!(tuned.timeout(), Duration::from_millis(250));
    assert_eq!(tuned.max_retries(), 0);
}
