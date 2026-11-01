//! Generic S3-compatible backend for [`OssClient`], powered by `rust-s3`.
//!
//! Enabled by the optional **`s3`** feature. Works against AWS S3, MinIO,
//! Ceph RGW, Cloudflare R2, and S3-compatible endpoints (including Alibaba
//! Cloud OSS's S3-compatible endpoint).
//!
//! # Blocking model
//!
//! [`OssClient`] is synchronous while `rust-s3` is async, so [`S3OssClient`]
//! owns a **dedicated worker thread** hosting a private Tokio runtime. Each
//! request is shipped to that thread as a job and its result comes back over a
//! channel; the caller's thread only blocks on the channel and **never** enters
//! a runtime.
//!
//! That matters because the core/harness store methods (`AgentStateStore` is
//! `async fn`, `BaseStore` is plain `fn`) call the blocking [`OssClient`]
//! inline on whatever thread polls them. `Runtime::block_on` would panic there
//! with "Cannot start a runtime from within a runtime"; handing the work to an
//! owned thread keeps the client safe from Tokio worker threads, current-thread
//! runtimes, `spawn_blocking` threads and plain OS threads alike.
//!
//! # Resilience
//!
//! Every request is bounded by the client's `timeout` ([`DEFAULT_TIMEOUT`] by default) and
//! retried up to `max_retries` times ([`DEFAULT_MAX_RETRIES`]) when the failure is
//! retryable ([`OssError::is_retryable`]: 5xx, 408/429, transport errors, timeouts), using
//! exponential backoff with jitter. A rejected request (403/404/412, ...) is never
//! retried. Configure both on the builder.
//!
//! # Example
//!
//! ```no_run
//! use agentscope_extensions_oss::S3OssClient;
//!
//! let client = S3OssClient::builder()
//!     .bucket("my-bucket")
//!     .region("us-east-1")                    // default: us-east-1
//!     .endpoint("http://127.0.0.1:9000")      // MinIO / Ceph / OSS-S3
//!     .access_key("access-key")               // or AWS_ACCESS_KEY_ID
//!     .secret_key("secret-key")               // or AWS_SECRET_ACCESS_KEY
//!     .session_token("temporary-token")       // optional
//!     .build()
//!     .unwrap();
//! ```

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::error::S3Error;
use s3::region::Region;

use crate::client::{
    OssClient, OssListPage, OssObjectSummary, OssObjectVersion, PutCondition, PutOutcome,
};
use crate::error::{OssError, OssResult};

/// Default S3 region used when the builder does not specify one.
pub const DEFAULT_REGION: &str = "us-east-1";

/// Default per-request timeout (Java uses the SDK's `ClientConfiguration` timeouts).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default number of retries after the first attempt (Java `maxErrorRetry = 3`).
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Base delay of the exponential retry backoff.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

/// Upper bound of a single retry backoff.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

/// S3 maximum number of keys per `ListObjectsV2` page.
const MAX_KEYS_PER_PAGE: usize = 1000;

/// S3 minimum size of every multipart part except the last.
const S3_MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// Content type used for multipart uploads (parts carry no other metadata).
const MULTIPART_CONTENT_TYPE: &str = "application/octet-stream";

/// S3-compatible implementation of [`OssClient`].
///
/// Holds a dedicated worker thread (with its own Tokio runtime) plus a shared
/// `rust-s3` [`Bucket`]. See the [module docs](self) for the blocking contract.
pub struct S3OssClient {
    worker: S3Worker,
    bucket: Arc<Bucket>,
    bucket_name: String,
    region: String,
    endpoint: Option<String>,
    path_style: bool,
    timeout: Duration,
    max_retries: u32,
}

impl fmt::Debug for S3OssClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never expose credentials via `Debug`.
        f.debug_struct("S3OssClient")
            .field("bucket", &self.bucket_name)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("path_style", &self.path_style)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

impl S3OssClient {
    /// Creates a builder.
    pub fn builder() -> S3OssClientBuilder {
        S3OssClientBuilder::default()
    }

    /// Returns the configured bucket name.
    pub fn bucket_name(&self) -> &str {
        &self.bucket_name
    }

    /// Returns the configured region (or `us-east-1` by default).
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Returns the custom endpoint, if one was configured.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Returns whether path-style addressing is enabled.
    pub fn is_path_style(&self) -> bool {
        self.path_style
    }

    /// Returns the per-request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns how many times a retryable failure is retried.
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Runs `build(bucket)` on the worker thread, blocking until it completes.
    ///
    /// Every attempt is bounded by [`Self::timeout`], and a failure reported as
    /// retryable by [`OssError::is_retryable`] (5xx, 429/408, transport error, timeout)
    /// is retried up to [`Self::max_retries`] times with exponential backoff plus
    /// jitter. `build` is an `Fn` so each attempt builds a fresh future; the future is
    /// **created on the worker thread**, so the caller's runtime is never entered and
    /// `block_on` cannot nest.
    fn run<Fut, Out>(&self, build: impl Fn(Arc<Bucket>) -> Fut + Send + 'static) -> OssResult<Out>
    where
        Fut: std::future::Future<Output = Result<Out, S3Error>> + 'static,
        Out: Send + 'static,
    {
        let bucket = Arc::clone(&self.bucket);
        let timeout = self.timeout;
        let max_retries = self.max_retries;
        let result = self.worker.execute(move |runtime| {
            let mut attempt: u32 = 0;
            loop {
                let outcome = runtime.block_on(async {
                    match tokio::time::timeout(timeout, build(Arc::clone(&bucket))).await {
                        Ok(Ok(value)) => Ok(value),
                        Ok(Err(error)) => Err(OssError::from(error)),
                        Err(_) => Err(OssError::Timeout(timeout)),
                    }
                });
                match outcome {
                    Ok(value) => return Ok(value),
                    Err(error) if attempt < max_retries && error.is_retryable() => {
                        attempt += 1;
                        // The timer handle is only available inside the worker's runtime,
                        // so the sleep future must be created there (not at the call site).
                        let delay = retry_delay(attempt);
                        runtime.block_on(async move { tokio::time::sleep(delay).await });
                    }
                    Err(error) => return Err(error),
                }
            }
        })?;
        result
    }
}

/// Delay before retry number `attempt` (1-based): exponential with up to 25% jitter so a
/// fleet of clients does not stampede the same object after a backend hiccup.
fn retry_delay(attempt: u32) -> Duration {
    static JITTER: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

    let exponent = attempt.saturating_sub(1).min(6);
    let base = RETRY_BASE_DELAY.as_millis() as u64;
    let capped = base
        .saturating_mul(1u64 << exponent)
        .min(RETRY_MAX_DELAY.as_millis() as u64);

    let mut x = JITTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed) ^ u64::from(attempt);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;

    Duration::from_millis(capped * (100 + x % 26) / 100)
}

/// A unit of work for the [`S3Worker`] thread: it receives the worker's runtime
/// and drives one request to completion there.
type Job = Box<dyn FnOnce(&tokio::runtime::Runtime) + Send + 'static>;

/// Owns the dedicated thread that hosts the private Tokio runtime.
///
/// `rust-s3` is async but [`OssClient`] is synchronous, and the stores call the
/// client inline from `async fn` bodies (core `AgentStateStore`) — i.e. possibly
/// on a Tokio worker thread, where `Runtime::block_on` panics. Shipping every
/// job to an owned thread removes the nested-runtime hazard entirely and does
/// not depend on the caller's runtime flavour (`block_in_place` requires a
/// multi-thread runtime, so it would still panic on `#[tokio::test]`).
struct S3Worker {
    // `std::sync::mpsc::Sender` is `Send` but **not** `Sync`, while `OssClient`
    // requires `Send + Sync`; the mutex only exists to restore `Sync` (the lock is
    // held for the duration of a single non-blocking `send`).
    sender: Option<std::sync::Mutex<std::sync::mpsc::Sender<Job>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl S3Worker {
    fn new() -> OssResult<Self> {
        let (sender, receiver) = std::sync::mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<OssResult<()>>();

        let thread = std::thread::Builder::new()
            .name("agentscope-oss-s3".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        let _ = ready_tx.send(Ok(()));
                        runtime
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(OssError::S3Config(format!(
                            "failed to create Tokio runtime: {error}"
                        ))));
                        return;
                    }
                };
                while let Ok(job) = receiver.recv() {
                    job(&runtime);
                }
                // The runtime is dropped here, on its own thread: dropping it
                // from inside the caller's runtime would panic.
            })
            .map_err(|error| {
                OssError::S3Config(format!("failed to spawn S3 worker thread: {error}"))
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                sender: Some(std::sync::Mutex::new(sender)),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                drop(sender);
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(OssError::S3Config(
                "S3 worker thread exited during startup".to_string(),
            )),
        }
    }

    /// Queues `work` and blocks the calling thread until it produced a result.
    fn execute<Out, F>(&self, work: F) -> OssResult<Out>
    where
        F: FnOnce(&tokio::runtime::Runtime) -> Out + Send + 'static,
        Out: Send + 'static,
    {
        let (result_tx, result_rx) = std::sync::mpsc::channel::<Out>();
        let job: Job = Box::new(move |runtime: &tokio::runtime::Runtime| {
            let _ = result_tx.send(work(runtime));
        });
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| OssError::Backend("S3 client is shut down".to_string()))?;
        {
            let sender = sender
                .lock()
                .map_err(|_| OssError::Backend("S3 worker channel is poisoned".to_string()))?;
            sender
                .send(job)
                .map_err(|_| OssError::Backend("S3 worker thread is gone".to_string()))?;
        }
        result_rx
            .recv()
            .map_err(|_| OssError::Backend("S3 worker thread dropped the response".to_string()))
    }
}

impl Drop for S3Worker {
    fn drop(&mut self) {
        // Dropping the sender ends the worker's `recv()` loop; joining makes
        // shutdown deterministic (an in-flight request still completes).
        self.sender = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Builder for [`S3OssClient`].
#[derive(Clone)]
pub struct S3OssClientBuilder {
    bucket: Option<String>,
    region: Option<String>,
    endpoint: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    session_token: Option<String>,
    path_style: Option<bool>,
    env_credentials: bool,
    timeout: Option<Duration>,
    max_retries: Option<u32>,
}

impl Default for S3OssClientBuilder {
    fn default() -> Self {
        Self {
            bucket: None,
            region: None,
            endpoint: None,
            access_key: None,
            secret_key: None,
            session_token: None,
            path_style: None,
            env_credentials: true,
            timeout: None,
            max_retries: None,
        }
    }
}

impl fmt::Debug for S3OssClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never expose credentials via `Debug`.
        f.debug_struct("S3OssClientBuilder")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field(
                "access_key",
                &self.access_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "secret_key",
                &self.secret_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("path_style", &self.path_style)
            .field("env_credentials", &self.env_credentials)
            .finish()
    }
}

impl S3OssClientBuilder {
    /// Sets the target bucket (required, must not be blank).
    pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
        self.bucket = Some(bucket.into());
        self
    }

    /// Sets the S3 region (defaults to [`DEFAULT_REGION`]).
    ///
    /// Ignored for signing purposes when a custom `endpoint` is used, but still
    /// reported as the region name (OSS/MinIO commonly use `us-east-1`).
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Sets a custom endpoint, e.g. `http://127.0.0.1:9000` for MinIO.
    ///
    /// When set, path-style addressing is enabled by default (required by most
    /// non-AWS implementations). Override with [`Self::path_style`].
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Sets the access key id.
    ///
    /// When omitted, `AWS_ACCESS_KEY_ID` is used (unless env fallback is
    /// disabled via [`Self::env_credentials`]).
    pub fn access_key(mut self, access_key: impl Into<String>) -> Self {
        self.access_key = Some(access_key.into());
        self
    }

    /// Sets the secret access key.
    ///
    /// When omitted, `AWS_SECRET_ACCESS_KEY` is used (unless env fallback is
    /// disabled via [`Self::env_credentials`]).
    pub fn secret_key(mut self, secret_key: impl Into<String>) -> Self {
        self.secret_key = Some(secret_key.into());
        self
    }

    /// Sets an optional temporary session token (also read from
    /// `AWS_SESSION_TOKEN` when env fallback is enabled).
    pub fn session_token(mut self, session_token: impl Into<String>) -> Self {
        self.session_token = Some(session_token.into());
        self
    }

    /// Forces path-style (`endpoint/bucket/key`) or virtual-hosted-style
    /// (`bucket.endpoint/key`) addressing.
    ///
    /// Defaults to `true` when a custom endpoint is configured, `false`
    /// otherwise.
    pub fn path_style(mut self, path_style: bool) -> Self {
        self.path_style = Some(path_style);
        self
    }

    /// Enables or disables falling back to `AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY`, and `AWS_SESSION_TOKEN`.
    ///
    /// Enabled by default. Disabling it makes missing credentials a
    /// deterministic error (useful for configuration validation/tests).
    pub fn env_credentials(mut self, enabled: bool) -> Self {
        self.env_credentials = enabled;
        self
    }

    /// Sets the per-request timeout (defaults to [`DEFAULT_TIMEOUT`]).
    ///
    /// The timeout is enforced on the client's worker thread, so a hung backend fails
    /// with [`OssError::Timeout`] instead of parking the calling thread forever.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets how often a retryable failure is retried (defaults to [`DEFAULT_MAX_RETRIES`]).
    ///
    /// `0` disables retries. Only failures reported as retryable by
    /// [`OssError::is_retryable`] are retried.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Validates the configuration and builds the client.
    pub fn build(self) -> OssResult<S3OssClient> {
        let bucket_name = match self.bucket {
            Some(bucket) if !bucket.trim().is_empty() => bucket,
            _ => return Err(OssError::BlankBucket),
        };

        let region_name = self
            .region
            .filter(|region| !region.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REGION.to_string());

        let endpoint = self
            .endpoint
            .map(|endpoint| endpoint.trim().trim_end_matches('/').to_string())
            .filter(|endpoint| !endpoint.is_empty());

        let access_key =
            resolve_credential(self.access_key, "AWS_ACCESS_KEY_ID", self.env_credentials);
        let secret_key = resolve_credential(
            self.secret_key,
            "AWS_SECRET_ACCESS_KEY",
            self.env_credentials,
        );
        let session_token = resolve_credential(
            self.session_token,
            "AWS_SESSION_TOKEN",
            self.env_credentials,
        );

        let (Some(access_key), Some(secret_key)) = (access_key, secret_key) else {
            return Err(OssError::MissingCredentials);
        };

        let credentials = Credentials::new(
            Some(&access_key),
            Some(&secret_key),
            None,
            session_token.as_deref(),
            None,
        )
        .map_err(|e| OssError::S3Config(format!("invalid S3 credentials: {e}")))?;

        let region = match &endpoint {
            Some(endpoint) => Region::Custom {
                region: region_name.clone(),
                endpoint: endpoint.clone(),
            },
            None => region_name.parse::<Region>().map_err(|e| {
                OssError::S3Config(format!("invalid S3 region {region_name:?}: {e}"))
            })?,
        };

        let path_style = self.path_style.unwrap_or(endpoint.is_some());

        let bucket = Bucket::new(&bucket_name, region, credentials)?;
        let bucket = if path_style {
            bucket.with_path_style()
        } else {
            bucket
        };

        // The runtime lives on the worker thread; only a channel handle is kept
        // here, so no `block_on` ever runs on the caller's (possibly async) thread.
        let worker = S3Worker::new()?;

        Ok(S3OssClient {
            worker,
            bucket: Arc::from(bucket),
            bucket_name,
            region: region_name,
            endpoint,
            path_style,
            timeout: self.timeout.unwrap_or(DEFAULT_TIMEOUT),
            max_retries: self.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
        })
    }
}

fn resolve_credential(explicit: Option<String>, env_var: &str, use_env: bool) -> Option<String> {
    if let Some(value) = explicit {
        if !value.trim().is_empty() {
            return Some(value);
        }
    }
    if use_env {
        if let Ok(value) = std::env::var(env_var) {
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// `404` or an explicit `NoSuchKey` response means "object absent".
fn is_not_found(error: &S3Error) -> bool {
    match error {
        S3Error::HttpFailWithBody(404, _) => true,
        S3Error::HttpFailWithBody(_, body) => body.contains("NoSuchKey"),
        _ => false,
    }
}

/// [`is_not_found`] for the [`OssError`] wrapper produced by `S3OssClient::run`.
fn is_missing_object(error: &OssError) -> bool {
    matches!(error, OssError::S3(inner) if is_not_found(inner))
}

fn unexpected_status(status: u16, body: &[u8]) -> OssError {
    OssError::S3Http {
        status,
        body: String::from_utf8_lossy(body).into_owned(),
    }
}

fn ensure_success(status: u16, body: &[u8]) -> OssResult<()> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(unexpected_status(status, body))
    }
}

impl OssClient for S3OssClient {
    fn put(&self, object_key: &str, content: &[u8]) -> OssResult<()> {
        let key = object_key.to_string();
        let body = Arc::new(content.to_vec());
        let response = self.run(move |bucket| {
            let key = key.clone();
            let body = Arc::clone(&body);
            async move { bucket.put_object(key.as_str(), body.as_slice()).await }
        })?;
        ensure_success(response.status_code(), response.as_slice())
    }

    fn get(&self, object_key: &str) -> OssResult<Option<Vec<u8>>> {
        let key = object_key.to_string();
        match self.run(move |bucket| {
            let key = key.clone();
            async move { bucket.get_object(key.as_str()).await }
        }) {
            Ok(response) => {
                let status = response.status_code();
                if status == 404 {
                    Ok(None)
                } else if (200..300).contains(&status) {
                    Ok(Some(response.as_slice().to_vec()))
                } else {
                    Err(unexpected_status(status, response.as_slice()))
                }
            }
            Err(error) if is_missing_object(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn exists(&self, object_key: &str) -> OssResult<bool> {
        let key = object_key.to_string();
        match self.run(move |bucket| {
            let key = key.clone();
            async move { bucket.head_object(key.as_str()).await }
        }) {
            Ok((_, status)) if (200..300).contains(&status) => Ok(true),
            Ok((_, 404)) => Ok(false),
            Ok((_, status)) => Err(unexpected_status(status, b"")),
            Err(error) if is_missing_object(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn delete(&self, object_key: &str) -> OssResult<()> {
        // S3 DeleteObject is idempotent, but some gateways return 404 for a
        // missing key; both are treated as success.
        let key = object_key.to_string();
        match self.run(move |bucket| {
            let key = key.clone();
            async move { bucket.delete_object(key.as_str()).await }
        }) {
            Ok(response) if (200..300).contains(&response.status_code()) => Ok(()),
            Ok(response) if response.status_code() == 404 => Ok(()),
            Ok(response) => Err(unexpected_status(
                response.status_code(),
                response.as_slice(),
            )),
            Err(error) if is_missing_object(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn list_page(
        &self,
        prefix: &str,
        max_keys: usize,
        continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        if max_keys == 0 {
            return Ok(OssListPage::default());
        }

        // `max_keys` and `continuation_token` are forwarded to S3 verbatim;
        // this is a real ListObjectsV2 request, not a slice of a full listing.
        let key_prefix = prefix.to_string();
        let token = continuation_token.map(str::to_string);
        let page_size = max_keys.min(MAX_KEYS_PER_PAGE);
        let (page, status) = self.run(move |bucket| {
            let key_prefix = key_prefix.clone();
            let token = token.clone();
            async move {
                bucket
                    .list_page(key_prefix, None, token, None, Some(page_size))
                    .await
            }
        })?;
        ensure_success(status, b"")?;

        let objects = page
            .contents
            .into_iter()
            .map(|object| OssObjectSummary { key: object.key })
            .collect();

        Ok(OssListPage {
            objects,
            truncated: page.is_truncated,
            next_token: page.next_continuation_token,
        })
    }

    fn supports_conditional_writes(&self) -> bool {
        true
    }

    fn get_with_etag(&self, object_key: &str) -> OssResult<Option<OssObjectVersion>> {
        let key = object_key.to_string();
        match self.run(move |bucket| {
            let key = key.clone();
            async move { bucket.get_object(key.as_str()).await }
        }) {
            Ok(response) => {
                let status = response.status_code();
                if status == 404 {
                    return Ok(None);
                }
                if !(200..300).contains(&status) {
                    return Err(unexpected_status(status, response.as_slice()));
                }
                let etag = etag_of(&response).ok_or_else(|| {
                    OssError::Backend(format!(
                        "S3 GET for '{object_key}' returned no ETag, so it cannot anchor a CAS"
                    ))
                })?;
                Ok(Some(OssObjectVersion {
                    content: response.as_slice().to_vec(),
                    etag,
                }))
            }
            Err(error) if is_missing_object(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn put_conditional(
        &self,
        object_key: &str,
        content: &[u8],
        condition: &PutCondition,
    ) -> OssResult<PutOutcome> {
        let key = object_key.to_string();
        let body = Arc::new(content.to_vec());
        let condition = condition.clone();
        let outcome = self.run(move |bucket| {
            let key = key.clone();
            let body = Arc::clone(&body);
            let condition = condition.clone();
            async move {
                let mut request = bucket.put_object_builder(key.as_str(), body.as_slice());
                if let Some((name, value)) = condition.as_header() {
                    request = request.with_header(name, value)?;
                }
                request.execute().await
            }
        });

        match outcome {
            Ok(response) => {
                let status = response.status_code();
                if is_precondition_failed_status(status) {
                    return Ok(PutOutcome::PreconditionFailed);
                }
                ensure_success(status, response.as_slice())?;
                let etag = etag_of(&response).ok_or_else(|| {
                    OssError::Backend(format!(
                        "S3 PUT for '{object_key}' returned no ETag, so it cannot anchor a CAS"
                    ))
                })?;
                Ok(PutOutcome::Written { etag })
            }
            Err(error) if is_precondition_failed_error(&error) => {
                Ok(PutOutcome::PreconditionFailed)
            }
            Err(error) => Err(error),
        }
    }

    fn delete_many(&self, object_keys: &[String]) -> OssResult<()> {
        if object_keys.is_empty() {
            return Ok(());
        }
        let keys = object_keys.to_vec();
        let result = self.run(move |bucket| {
            let keys = keys.clone();
            async move {
                let objects = keys
                    .into_iter()
                    .map(s3::serde_types::ObjectIdentifier::new)
                    .collect::<Vec<_>>();
                bucket.delete_objects(objects).await
            }
        })?;
        // S3 answers 200 even when single keys could not be deleted; those failures are
        // reported per key in the body and must not be swallowed.
        match result.errors.first() {
            Some(error) => Err(OssError::Backend(format!(
                "S3 bulk delete failed for '{}': {} ({})",
                error.key, error.message, error.code
            ))),
            None => Ok(()),
        }
    }

    fn put_multipart(&self, object_key: &str, content: &[u8], part_size: usize) -> OssResult<()> {
        // An empty object has no parts; a single PUT is both correct and cheaper.
        if content.is_empty() {
            return self.put(object_key, content);
        }
        // S3 rejects non-final parts below 5 MiB, so a smaller request is raised to the
        // minimum instead of failing mid-upload (it also keeps `chunks` non-zero).
        let part_size = part_size.max(S3_MIN_PART_SIZE);
        let key = object_key.to_string();
        let body = Arc::new(content.to_vec());

        let upload_id = self
            .run({
                let key = key.clone();
                move |bucket| {
                    let key = key.clone();
                    async move {
                        bucket
                            .initiate_multipart_upload(&key, MULTIPART_CONTENT_TYPE)
                            .await
                    }
                }
            })?
            .upload_id;

        let mut parts = Vec::with_capacity(body.len().div_ceil(part_size));
        for (index, chunk) in body.chunks(part_size).enumerate() {
            let part_number = index as u32 + 1;
            // `rust-s3` reads a part's ETag from the response **body**, but S3 sends it as a
            // header and leaves the body empty, so the ETag is derived here instead: a
            // part's ETag is the MD5 of its bytes (unless the bucket forces SSE-KMS, in
            // which case `CompleteMultipartUpload` rejects the upload instead of silently
            // committing wrong data). A failed part aborts the upload inside `rust-s3`.
            let etag = format!("\"{:x}\"", md5::compute(chunk));
            let chunk = chunk.to_vec();
            self.run({
                let key = key.clone();
                let upload_id = upload_id.clone();
                move |bucket| {
                    let key = key.clone();
                    let upload_id = upload_id.clone();
                    let chunk = chunk.clone();
                    async move {
                        bucket
                            .put_multipart_chunk(
                                chunk,
                                &key,
                                part_number,
                                &upload_id,
                                MULTIPART_CONTENT_TYPE,
                            )
                            .await
                    }
                }
            })?;
            parts.push(s3::serde_types::Part { part_number, etag });
        }

        let parts = Arc::new(parts);
        self.run({
            let key = key.clone();
            let upload_id = upload_id.clone();
            move |bucket| {
                let key = key.clone();
                let upload_id = upload_id.clone();
                let parts = Arc::clone(&parts);
                async move {
                    bucket
                        .complete_multipart_upload(&key, &upload_id, (*parts).clone())
                        .await
                }
            }
        })?;
        Ok(())
    }
}

/// Reads the `ETag` response header (S3 and gateways vary only in casing).
fn etag_of(response: &s3::request::ResponseData) -> Option<String> {
    response
        .headers()
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("etag"))
        .map(|(_, value)| value)
        .filter(|value| !value.trim().is_empty())
}

/// `412 Precondition Failed` (and `409 ConditionalRequestConflict`) reject a conditional write.
fn is_precondition_failed_status(status: u16) -> bool {
    status == 412 || status == 409
}

/// [`is_precondition_failed_status`] for the [`OssError`] wrapper produced by `run`.
fn is_precondition_failed_error(error: &OssError) -> bool {
    matches!(error, OssError::S3(inner) if matches!(inner, S3Error::HttpFailWithBody(status, _) if is_precondition_failed_status(*status)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_rejects_blank_bucket() {
        let err = S3OssClient::builder()
            .bucket("   ")
            .access_key("ak")
            .secret_key("sk")
            .env_credentials(false)
            .build()
            .unwrap_err();
        assert!(matches!(err, OssError::BlankBucket));
    }

    #[test]
    fn builder_requires_credentials_without_env_fallback() {
        let err = S3OssClient::builder()
            .bucket("bucket")
            .env_credentials(false)
            .build()
            .unwrap_err();
        assert!(matches!(err, OssError::MissingCredentials));
    }

    #[test]
    fn builder_defaults_region_and_path_style_for_custom_endpoint() {
        let client = S3OssClient::builder()
            .bucket("bucket")
            .endpoint("http://127.0.0.1:9000/")
            .access_key("ak")
            .secret_key("sk")
            .env_credentials(false)
            .build()
            .unwrap();
        assert_eq!(client.region(), DEFAULT_REGION);
        assert_eq!(client.endpoint(), Some("http://127.0.0.1:9000"));
        assert!(client.is_path_style());
    }

    #[test]
    fn builder_can_disable_path_style() {
        let client = S3OssClient::builder()
            .bucket("bucket")
            .endpoint("http://127.0.0.1:9000")
            .path_style(false)
            .access_key("ak")
            .secret_key("sk")
            .env_credentials(false)
            .build()
            .unwrap();
        assert!(!client.is_path_style());
    }
}
