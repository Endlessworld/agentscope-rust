//! Real Tencent Cloud COS backend: [`CosHttpObjectStore`].
//!
//! The Java module uses the official SDK (`com.qcloud:cos_api:5.6.245`), which
//! bundles both the HTTP transport and the v5 request signature. No comparable Rust
//! COS SDK is available, so this module speaks the COS **XML API** over `reqwest`
//! and signs every request itself ([`crate::signature`], `q-sign-algorithm=sha1`).
//!
//! Operation mapping (Java `COSClient` -> COS REST):
//!
//! | [`CosObjectStore`] | COS REST |
//! |---|---|
//! | [`get`](CosObjectStore::get) | `GET /{key}` (`404` -> `Ok(None)`) |
//! | [`put`](CosObjectStore::put) | `PUT /{key}` |
//! | [`exists`](CosObjectStore::exists) | `HEAD /{key}` (`404` -> `false`) |
//! | [`delete`](CosObjectStore::delete) | `DELETE /{key}` (`404` -> `Ok(())`) |
//! | [`list_page`](CosObjectStore::list_page) | `GET /?list-type=2&prefix=&continuation-token=&max-keys=` |
//! | [`delete_many`](CosObjectStore::delete_many) | `POST /?delete` (1000 keys per request, `Content-MD5`) |
//!
//! Bodies at or above `multipart_threshold` (default 5 MiB, the Java SDK
//! `TransferManagerConfiguration` default) are uploaded with the multipart API
//! instead of a single `PUT`:
//!
//! | Step | COS REST |
//! |---|---|
//! | initiate | `POST /{key}?uploads` → `<UploadId>` |
//! | upload part | `PUT /{key}?partNumber=N&uploadId=..` → `ETag` response header |
//! | complete | `POST /{key}?uploadId=..` with `<CompleteMultipartUpload>` |
//! | abort (rollback) | `DELETE /{key}?uploadId=..` |
//!
//! [`CosHttpObjectStore::presign_get`] / [`CosHttpObjectStore::presign_put`] build
//! presigned URLs with the same signature arithmetic
//! ([`crate::presign::presign_url`]); they never touch the network.
//!
//! Errors keep the HTTP status, so an authentication failure (`403`,
//! `SignatureDoesNotMatch`) is distinguishable from a missing object (`404`, which
//! `get`/`exists`/`delete` map to their "absent" result instead of an error).
//!
//! ## Temporary credentials (STS)
//!
//! A session token configured through `CosHttpObjectStoreBuilder::security_token`
//! (or `CosCredentials::with_security_token`) is sent as `x-cos-security-token`,
//! signed with the request, and carried as a query parameter in presigned URLs.
//! **Refreshing the token is the caller's responsibility**: when the STS credentials
//! expire, rebuild the store with fresh values — [`CosHttpObjectStore::with_credentials`]
//! returns a new store that shares the HTTP connection pool.
//!
//! ```no_run
//! # use agentscope_extensions_cos::{CosHttpObjectStore, CosObjectStore};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let store = CosHttpObjectStore::builder()
//!     .bucket("examplebucket-1250000000")
//!     .region("ap-beijing")
//!     .secret_id("AKID...")
//!     .secret_key("...")
//!     .build()?;
//! store.put("agentscope/state/u1/s1/k.json", b"{}").await?;
//! # Ok(()) }
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::{Method, Response, StatusCode};

use crate::multipart::{self, MultipartPlan, UploadedPart};
use crate::object_store::{
    ConditionalWrite, CosObjectStore, EtaggedObject, ObjectListPage, DEFAULT_LIST_PAGE_SIZE,
};
use crate::presign;
use crate::signature::{self, CosCredentials};
use crate::{CosError, Result};

/// Default signature validity (matches the COS SDK default of 15 minutes).
pub const DEFAULT_SIGN_EXPIRE_SECS: u64 = 900;

/// Default TCP/TLS connect timeout, mirroring the Java SDK `ClientConfig`
/// `connectionTimeout` default (50 s).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(50);

/// Default per-read idle timeout, mirroring the Java SDK `ClientConfig`
/// `socketTimeout` default (50 s). It applies to **each** read and resets after a
/// successful one, so a long transfer that keeps making progress is never cut off
/// (unlike a total request deadline).
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(50);

/// Default number of retries for idempotent requests, mirroring the Java SDK
/// `ClientConfig` `maxErrorRetry` default.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Delay before the *second* attempt; doubles on every further attempt
/// (`100ms`, `200ms`, `400ms`, ...) and is capped at [`MAX_RETRY_DELAY`].
pub const DEFAULT_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

/// Upper bound for a single retry delay.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(2);

/// COS caps `max-keys` at 1000.
pub const MAX_LIST_KEYS: usize = 1000;

/// COS accepts at most 1000 keys per `POST /?delete` request.
pub const MAX_DELETE_BATCH: usize = 1000;

/// Clock used to stamp `q-sign-time` (Unix seconds). Injectable so tests can pin the
/// signature instead of depending on wall-clock time.
pub type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

const PUT_CONTENT_TYPE: &str = "application/octet-stream";
const DELETE_CONTENT_TYPE: &str = "application/xml";
const COMPLETE_CONTENT_TYPE: &str = "application/xml";
const USER_AGENT: &str = concat!("agentscope-rust-cos/", env!("CARGO_PKG_VERSION"));

/// Immutable client configuration.
#[derive(Clone)]
pub struct CosHttpConfig {
    bucket: String,
    region: String,
    endpoint: Option<String>,
    credentials: CosCredentials,
    sign_expire_secs: u64,
    multipart_threshold: u64,
    multipart_part_size: u64,
    multipart_concurrency: usize,
    connect_timeout: Duration,
    read_timeout: Duration,
    max_retries: u32,
    clock: ClockFn,
}

impl std::fmt::Debug for CosHttpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosHttpConfig")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("credentials", &self.credentials)
            .field("sign_expire_secs", &self.sign_expire_secs)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("multipart_part_size", &self.multipart_part_size)
            .field("multipart_concurrency", &self.multipart_concurrency)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

impl CosHttpConfig {
    /// Creates a configuration without an endpoint override (public COS domain).
    pub fn new(
        bucket: impl Into<String>,
        region: impl Into<String>,
        credentials: CosCredentials,
    ) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            endpoint: None,
            credentials,
            sign_expire_secs: DEFAULT_SIGN_EXPIRE_SECS,
            multipart_threshold: multipart::DEFAULT_MULTIPART_THRESHOLD,
            multipart_part_size: multipart::DEFAULT_MULTIPART_PART_SIZE,
            multipart_concurrency: multipart::DEFAULT_MULTIPART_CONCURRENCY,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            clock: Arc::new(now_secs),
        }
    }

    /// The bucket name (`name-appid`).
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The region, e.g. `ap-beijing`.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// The endpoint override, when configured (tests point this at `httpmock`).
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Signature validity in seconds.
    pub fn sign_expire_secs(&self) -> u64 {
        self.sign_expire_secs
    }

    /// Credentials in use (temporary credentials include the session token).
    pub fn credentials(&self) -> &CosCredentials {
        &self.credentials
    }

    /// Body size at which `put` switches to a multipart upload.
    pub fn multipart_threshold(&self) -> u64 {
        self.multipart_threshold
    }

    /// Configured minimum part size for multipart uploads.
    pub fn multipart_part_size(&self) -> u64 {
        self.multipart_part_size
    }

    /// Maximum number of parts uploaded concurrently.
    pub fn multipart_concurrency(&self) -> usize {
        self.multipart_concurrency
    }

    /// TCP/TLS connect timeout applied by the internally built HTTP client.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Per-read idle timeout applied by the internally built HTTP client.
    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    /// Number of retries allowed for idempotent requests (`0` = none).
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// The `host[:port]` that is signed as the `host` header.
    pub fn host(&self) -> String {
        let base = self.base_url();
        let without_scheme = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"))
            .unwrap_or(&base);
        without_scheme.trim_end_matches('/').to_string()
    }

    /// The base URL without a trailing slash.
    pub fn base_url(&self) -> String {
        match &self.endpoint {
            Some(endpoint) => endpoint.trim_end_matches('/').to_string(),
            None => format!("https://{}.cos.{}.myqcloud.com", self.bucket, self.region),
        }
    }
}

/// Builder for [`CosHttpObjectStore`].
///
/// Validates its inputs eagerly: a blank bucket, blank credentials, a missing region
/// (without an endpoint override) or a malformed endpoint is rejected here rather
/// than surfacing as a `403` on the first request. Java performs the equivalent
/// `Objects.requireNonNull(cosClient)` / `bucketName must not be blank` checks in the
/// `CosAgentStateStore` constructor.
#[derive(Default)]
pub struct CosHttpObjectStoreBuilder {
    bucket: Option<String>,
    region: Option<String>,
    secret_id: Option<String>,
    secret_key: Option<String>,
    security_token: Option<String>,
    endpoint: Option<String>,
    sign_expire_secs: Option<u64>,
    multipart_threshold: Option<u64>,
    multipart_part_size: Option<u64>,
    multipart_concurrency: Option<usize>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    max_retries: Option<u32>,
    clock: Option<ClockFn>,
    client: Option<reqwest::Client>,
}

impl std::fmt::Debug for CosHttpObjectStoreBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosHttpObjectStoreBuilder")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("sign_expire_secs", &self.sign_expire_secs)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("multipart_part_size", &self.multipart_part_size)
            .field("multipart_concurrency", &self.multipart_concurrency)
            .finish_non_exhaustive()
    }
}

impl CosHttpObjectStoreBuilder {
    pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
        self.bucket = Some(bucket.into());
        self
    }

    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    pub fn secret_id(mut self, secret_id: impl Into<String>) -> Self {
        self.secret_id = Some(secret_id.into());
        self
    }

    pub fn secret_key(mut self, secret_key: impl Into<String>) -> Self {
        self.secret_key = Some(secret_key.into());
        self
    }

    /// Uses temporary-session credentials (`x-cos-security-token`).
    pub fn security_token(mut self, token: impl Into<String>) -> Self {
        self.security_token = Some(token.into());
        self
    }

    pub fn credentials(mut self, credentials: CosCredentials) -> Self {
        self.secret_id = Some(credentials.secret_id().to_string());
        self.secret_key = Some(credentials.secret_key().to_string());
        self.security_token = credentials.security_token().map(str::to_string);
        self
    }

    /// Overrides the endpoint, e.g. `http://127.0.0.1:1234` or a custom domain.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn sign_expire_secs(mut self, secs: u64) -> Self {
        self.sign_expire_secs = Some(secs);
        self
    }

    /// Body size at which `put` switches to a multipart upload.
    ///
    /// Defaults to [`multipart::DEFAULT_MULTIPART_THRESHOLD`] (5 MiB, the Java SDK
    /// `TransferManagerConfiguration` value). `0` forces multipart for every body;
    /// values above [`multipart::MAX_SINGLE_PUT_SIZE`] (5 GiB) are rejected at build
    /// time, because COS refuses a single `PUT` that large — raise
    /// [`Self::multipart_part_size`] instead of disabling multipart.
    pub fn multipart_threshold(mut self, bytes: u64) -> Self {
        self.multipart_threshold = Some(bytes);
        self
    }

    /// Minimum multipart part size. Must stay within the COS limits
    /// ([`multipart::MIN_MULTIPART_PART_SIZE`] ..= [`multipart::MAX_MULTIPART_PART_SIZE`]).
    pub fn multipart_part_size(mut self, bytes: u64) -> Self {
        self.multipart_part_size = Some(bytes);
        self
    }

    /// Maximum number of parts uploaded at the same time (defaults to `1`, matching
    /// the SDK executor; must be at least `1`).
    pub fn multipart_concurrency(mut self, parts: usize) -> Self {
        self.multipart_concurrency = Some(parts);
        self
    }

    /// Replaces the signature clock (tests use a fixed timestamp).
    pub fn clock(mut self, clock: ClockFn) -> Self {
        self.clock = Some(clock);
        self
    }

    /// TCP/TLS connect timeout (defaults to [`DEFAULT_CONNECT_TIMEOUT`]).
    ///
    /// Ignored when a pre-configured client is supplied through
    /// [`Self::http_client`] (that client owns its own timeouts).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Per-read idle timeout (defaults to [`DEFAULT_READ_TIMEOUT`]).
    ///
    /// Semantics match the Java SDK `socketTimeout`: each read gets the full
    /// budget and it resets after a successful read, so slow-but-progressing
    /// downloads are not aborted. Ignored when a pre-configured client is supplied
    /// through [`Self::http_client`].
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = Some(timeout);
        self
    }

    /// Retries allowed for idempotent requests (`GET`/`HEAD`/`PUT`/`DELETE`)
    /// after a transport error, timeout, `408`, `429` or `5xx` (defaults to
    /// [`DEFAULT_MAX_RETRIES`]; `0` disables retries).
    ///
    /// `POST` is never retried: it creates server-side state (`?uploads`,
    /// `CompleteMultipartUpload`, `?delete`).
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = Some(retries);
        self
    }

    /// Supplies a pre-configured HTTP client (proxy, timeouts, ...).
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    pub fn build(self) -> Result<CosHttpObjectStore> {
        let bucket = require_non_blank(self.bucket, "bucketName must not be blank")?;
        if bucket.contains('/') || bucket.contains(' ') {
            return Err(CosError::Config(format!(
                "bucketName must be a bucket name without '/' or spaces: {bucket}"
            )));
        }
        let secret_id = require_non_blank(self.secret_id, "secretId must not be blank")?;
        let secret_key = require_non_blank(self.secret_key, "secretKey must not be blank")?;

        let endpoint = match self.endpoint {
            Some(endpoint) => {
                let trimmed = endpoint.trim().trim_end_matches('/').to_string();
                if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                    return Err(CosError::Config(format!(
                        "endpoint must start with http:// or https://: {endpoint}"
                    )));
                }
                Some(trimmed)
            }
            None => None,
        };

        let region = match self.region {
            Some(region) if !region.trim().is_empty() => region.trim().to_string(),
            _ if endpoint.is_some() => String::new(),
            _ => return Err(CosError::Config("region must not be blank".to_string())),
        };

        let credentials = CosCredentials::new(secret_id, secret_key);
        let credentials = match self.security_token {
            Some(token) => credentials.with_security_token(token),
            None => credentials,
        };

        let multipart_part_size = self
            .multipart_part_size
            .unwrap_or(multipart::DEFAULT_MULTIPART_PART_SIZE);
        if !(multipart::MIN_MULTIPART_PART_SIZE..=multipart::MAX_MULTIPART_PART_SIZE)
            .contains(&multipart_part_size)
        {
            return Err(CosError::Config(format!(
                "multipart part size must be between {} and {} bytes (COS limit), got {}",
                multipart::MIN_MULTIPART_PART_SIZE,
                multipart::MAX_MULTIPART_PART_SIZE,
                multipart_part_size
            )));
        }

        let multipart_threshold = self
            .multipart_threshold
            .unwrap_or(multipart::DEFAULT_MULTIPART_THRESHOLD);
        if multipart_threshold > multipart::MAX_SINGLE_PUT_SIZE {
            return Err(CosError::Config(format!(
                "multipart threshold must not exceed the {} byte single-PUT limit (COS caps a \
                 single PUT at that size); raise the part size instead of disabling multipart, \
                 got {multipart_threshold}",
                multipart::MAX_SINGLE_PUT_SIZE
            )));
        }

        let multipart_concurrency = self
            .multipart_concurrency
            .unwrap_or(multipart::DEFAULT_MULTIPART_CONCURRENCY);
        if multipart_concurrency == 0 {
            return Err(CosError::Config(
                "multipart concurrency must be at least 1".to_string(),
            ));
        }

        let connect_timeout = self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        if connect_timeout.is_zero() {
            return Err(CosError::Config(
                "connect timeout must be greater than zero".to_string(),
            ));
        }
        let read_timeout = self.read_timeout.unwrap_or(DEFAULT_READ_TIMEOUT);
        if read_timeout.is_zero() {
            return Err(CosError::Config(
                "read timeout must be greater than zero".to_string(),
            ));
        }
        let max_retries = self.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);

        let client = match self.client {
            Some(client) => client,
            None => reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(connect_timeout)
                .read_timeout(read_timeout)
                .build()
                .map_err(|e| CosError::Backend(format!("failed to build HTTP client: {e}")))?,
        };

        Ok(CosHttpObjectStore {
            client,
            config: Arc::new(CosHttpConfig {
                bucket,
                region,
                endpoint,
                credentials,
                sign_expire_secs: self.sign_expire_secs.unwrap_or(DEFAULT_SIGN_EXPIRE_SECS),
                multipart_threshold,
                multipart_part_size,
                multipart_concurrency,
                connect_timeout,
                read_timeout,
                max_retries,
                clock: self.clock.unwrap_or_else(|| Arc::new(now_secs)),
            }),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }
}

fn require_non_blank(value: Option<String>, message: &str) -> Result<String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        _ => Err(CosError::Config(message.to_string())),
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// COS object store backed by the real HTTP XML API.
///
/// Cheap to clone: the configuration is shared and the underlying `reqwest::Client`
/// pools connections.
#[derive(Clone)]
pub struct CosHttpObjectStore {
    client: reqwest::Client,
    config: Arc<CosHttpConfig>,
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for CosHttpObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosHttpObjectStore")
            .field("config", &self.config)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl CosHttpObjectStore {
    pub fn builder() -> CosHttpObjectStoreBuilder {
        CosHttpObjectStoreBuilder::default()
    }

    /// Creates a store for the public COS domain.
    pub fn new(
        bucket: impl Into<String>,
        region: impl Into<String>,
        secret_id: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Result<Self> {
        Self::builder()
            .bucket(bucket)
            .region(region)
            .secret_id(secret_id)
            .secret_key(secret_key)
            .build()
    }

    pub fn config(&self) -> &CosHttpConfig {
        &self.config
    }

    /// Whether [`close`](CosObjectStore::close) has been called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn ensure_open(&self) -> Result<()> {
        if self.is_closed() {
            return Err(CosError::Closed);
        }
        Ok(())
    }

    fn key_time(&self) -> String {
        let start = (self.config.clock)();
        signature::key_time(start, start + self.config.sign_expire_secs as i64)
    }

    /// Returns a store that shares this one's HTTP client and closed flag but signs
    /// with `credentials`.
    ///
    /// Used to roll over STS temporary credentials: the caller refreshes the token
    /// and swaps it in (`docs/extensions/COS__GAP_TRACKER.md` — automatic refresh is
    /// intentionally out of scope). Multipart and signature settings are inherited.
    pub fn with_credentials(&self, credentials: CosCredentials) -> Self {
        Self {
            client: self.client.clone(),
            config: Arc::new(CosHttpConfig {
                bucket: self.config.bucket.clone(),
                region: self.config.region.clone(),
                endpoint: self.config.endpoint.clone(),
                credentials,
                sign_expire_secs: self.config.sign_expire_secs,
                multipart_threshold: self.config.multipart_threshold,
                multipart_part_size: self.config.multipart_part_size,
                multipart_concurrency: self.config.multipart_concurrency,
                connect_timeout: self.config.connect_timeout,
                read_timeout: self.config.read_timeout,
                max_retries: self.config.max_retries,
                clock: Arc::clone(&self.config.clock),
            }),
            closed: Arc::clone(&self.closed),
        }
    }

    /// Signs a URL that lets an ordinary HTTP client download `key`.
    ///
    /// `expire_secs` overrides the client's `sign_expire_secs` (default 900 s); the
    /// value must be `1..=604800`. No request is issued.
    pub fn presign_get(&self, key: &str, expire_secs: Option<u64>) -> Result<String> {
        self.presign("GET", key, &[], expire_secs)
    }

    /// Signs a `GET` URL that also carries business parameters the receiver must
    /// send verbatim, e.g. `response-content-disposition` or
    /// `response-content-type` (COS then answers with the overridden headers).
    ///
    /// Every parameter is signed, so it shows up in `q-url-param-list`; names are
    /// lowercased and must not start with `q-` (reserved by the signature itself).
    pub fn presign_get_with_params(
        &self,
        key: &str,
        params: &[(&str, &str)],
        expire_secs: Option<u64>,
    ) -> Result<String> {
        self.presign("GET", key, params, expire_secs)
    }

    /// Signs a URL that lets an ordinary HTTP client `PUT` `key`.
    ///
    /// Only `host` is signed (plus the session token as a query parameter), so the
    /// uploader must not add headers the signature would not cover.
    pub fn presign_put(&self, key: &str, expire_secs: Option<u64>) -> Result<String> {
        self.presign("PUT", key, &[], expire_secs)
    }

    /// Signs a `PUT` URL with extra signed business parameters (see
    /// [`Self::presign_get_with_params`]).
    pub fn presign_put_with_params(
        &self,
        key: &str,
        params: &[(&str, &str)],
        expire_secs: Option<u64>,
    ) -> Result<String> {
        self.presign("PUT", key, params, expire_secs)
    }

    fn presign(
        &self,
        method: &str,
        key: &str,
        extra_params: &[(&str, &str)],
        expire_secs: Option<u64>,
    ) -> Result<String> {
        self.ensure_open()?;
        let expire = presign::resolve_expire_secs(expire_secs, self.config.sign_expire_secs)?;

        // Temporary credentials travel as a query parameter, so they are part of the
        // signed parameter list and no header is required on the receiving side.
        let mut params: Vec<(String, String)> = Vec::new();
        if let Some(token) = self.config.credentials.security_token() {
            params.push(("x-cos-security-token".to_string(), token.to_string()));
        }
        for (name, value) in extra_params {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() || name.starts_with("q-") {
                return Err(CosError::Config(format!(
                    "presigned URL parameter {name:?} is empty or reserved by the signature (q-*)"
                )));
            }
            params.push((name, value.to_string()));
        }

        let start = (self.config.clock)();
        let key_time = signature::key_time(start, start + expire as i64);
        Ok(presign::presign_url(
            &self.config.base_url(),
            &self.config.host(),
            &self.config.credentials,
            method,
            key,
            &params,
            &key_time,
        ))
    }

    /// Whether a body of `len` bytes is uploaded with the multipart API.
    ///
    /// Java: `TransferManagerUtils.shouldUseMultipartUpload` (multipart when the
    /// content length is **at least** the threshold). Empty bodies always use a
    /// single `PUT`: COS multipart needs at least one part.
    fn should_use_multipart(&self, len: usize) -> bool {
        len > 0 && len as u64 >= self.config.multipart_threshold
    }

    /// Uploads `data` with the COS multipart API, bypassing the threshold.
    ///
    /// Fails with [`CosError::Config`] when the body is empty, the configured part
    /// size is outside the COS range, or the object would need more than 10000 parts.
    pub async fn put_multipart(&self, key: &str, data: &[u8]) -> Result<()> {
        self.put_multipart_with(key, data, |_, _| {}).await
    }

    /// [`Self::put_multipart`] that reports progress.
    ///
    /// `on_progress(completed_parts, total_parts)` is called once before the first
    /// upload and again after every part the service accepted.
    pub async fn put_multipart_with<F>(&self, key: &str, data: &[u8], on_progress: F) -> Result<()>
    where
        F: FnMut(usize, usize),
    {
        self.ensure_open()?;
        let plan = multipart::plan_parts(data.len(), self.config.multipart_part_size as usize)?;
        let upload_id = self.initiate_multipart(key).await?;

        // From here on every failure must roll the upload back, otherwise the parts
        // linger in the bucket and keep being billed (COS docs, Complete Multipart
        // Upload: "建议您及时完成分块上传或者舍弃分块上传").
        match self
            .upload_parts_and_complete(key, &upload_id, data, plan, BTreeMap::new(), on_progress)
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Err(abort_err) = self.abort_multipart(key, &upload_id).await {
                    tracing::warn!(
                        key,
                        upload_id,
                        error = %abort_err,
                        "failed to abort a COS multipart upload; parts may still be billed"
                    );
                }
                Err(err)
            }
        }
    }

    /// Resumes the in-flight multipart upload `upload_id` of `key` with `data`.
    ///
    /// [`Self::list_parts`] reports which parts the service already stores; those
    /// whose size matches the plan are kept (their ETag is reused on completion),
    /// every other one is (re)uploaded. A failure leaves the upload **open** so the
    /// caller can retry, or discard it with [`Self::abort_multipart`].
    pub async fn resume_multipart(&self, key: &str, upload_id: &str, data: &[u8]) -> Result<()> {
        self.resume_multipart_with(key, upload_id, data, |_, _| {})
            .await
    }

    /// [`Self::resume_multipart`] with a progress callback (see
    /// [`Self::put_multipart_with`]).
    pub async fn resume_multipart_with<F>(
        &self,
        key: &str,
        upload_id: &str,
        data: &[u8],
        on_progress: F,
    ) -> Result<()>
    where
        F: FnMut(usize, usize),
    {
        self.ensure_open()?;
        let plan = multipart::plan_parts(data.len(), self.config.multipart_part_size as usize)?;
        let mut completed: BTreeMap<u32, String> = BTreeMap::new();
        for part in self.list_parts(key, upload_id).await? {
            let Some((start, end)) = plan.range(part.part_number) else {
                continue;
            };
            // A size mismatch means the caller's data changed under the same upload id,
            // so that part has to travel again.
            if !part.etag.is_empty() && part.size == (end - start) as u64 {
                completed.insert(part.part_number, part.etag);
            }
        }
        self.upload_parts_and_complete(key, upload_id, data, plan, completed, on_progress)
            .await
    }

    /// `GET /{key}?uploadId=..` — the parts the service already holds.
    ///
    /// Follows `part-number-marker` pagination (`max-parts` is capped at 1000).
    pub async fn list_parts(&self, key: &str, upload_id: &str) -> Result<Vec<UploadedPart>> {
        self.ensure_open()?;
        let mut parts = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut params = vec![
                ("uploadId".to_string(), upload_id.to_string()),
                ("max-parts".to_string(), multipart::MAX_LIST_PARTS.to_string()),
            ];
            if let Some(marker) = &marker {
                params.push(("part-number-marker".to_string(), marker.clone()));
            }
            let response = self.send(Method::GET, Some(key), &params, None, &[]).await?;
            let response = ensure_success(response).await?;
            let body = response.text().await.map_err(|e| {
                CosError::Backend(format!("failed to read COS ListParts response: {e}"))
            })?;
            parts.extend(parse_list_parts(&body));

            let truncated = xml_text(&body, "istruncated")
                .map(|value| value.trim().eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if !truncated {
                return Ok(parts);
            }
            match xml_text(&body, "nextpartnumbermarker") {
                Some(next) if !next.trim().is_empty() => marker = Some(next.trim().to_string()),
                // Without a marker there is nothing to continue from; stop instead of
                // looping forever.
                _ => return Ok(parts),
            }
        }
    }

    /// Uploads every part not already in `completed` (part number → ETag) and then
    /// completes the upload.
    ///
    /// `completed` accumulates the ETags of this attempt on top of the parts the
    /// service already stored, so a resumed upload completes with both.
    async fn upload_parts_and_complete<F>(
        &self,
        key: &str,
        upload_id: &str,
        data: &[u8],
        plan: MultipartPlan,
        mut completed: BTreeMap<u32, String>,
        mut on_progress: F,
    ) -> Result<()>
    where
        F: FnMut(usize, usize),
    {
        // Owned once so concurrently spawned part uploads can borrow it.
        let body = Arc::new(data.to_vec());
        let concurrency = self.config.multipart_concurrency.max(1);
        let pending: Vec<u32> = (1..=plan.part_count as u32)
            .filter(|part_number| !completed.contains_key(part_number))
            .collect();

        let mut done = completed.len();
        on_progress(done, plan.part_count);

        for chunk in pending.chunks(concurrency) {
            let mut tasks = tokio::task::JoinSet::new();
            for &part_number in chunk {
                let store = self.clone();
                let key = key.to_string();
                let upload_id = upload_id.to_string();
                let body = Arc::clone(&body);
                let Some((start, end)) = plan.range(part_number) else {
                    return Err(CosError::Backend(format!(
                        "multipart plan has no range for part {part_number}"
                    )));
                };
                tasks.spawn(async move {
                    store
                        .upload_part(&key, &upload_id, part_number, body[start..end].to_vec())
                        .await
                        .map(|etag| (part_number, etag))
                });
            }

            while let Some(joined) = tasks.join_next().await {
                match joined {
                    Ok(Ok((part_number, etag))) => {
                        completed.insert(part_number, etag);
                        done += 1;
                        on_progress(done, plan.part_count);
                    }
                    Ok(Err(err)) => return Err(err),
                    Err(join_err) => {
                        return Err(CosError::Backend(format!(
                            "multipart part task failed: {join_err}"
                        )))
                    }
                }
            }
        }

        // COS answers `400 InvalidPartOrder` unless parts are listed by number, which
        // walking a `BTreeMap` guarantees.
        let etags: Vec<(u32, String)> = completed.into_iter().collect();
        self.complete_multipart(key, upload_id, &etags).await
    }

    /// `POST /{key}?uploads` → `<UploadId>`.
    async fn initiate_multipart(&self, key: &str) -> Result<String> {
        let headers = vec![("content-type".to_string(), PUT_CONTENT_TYPE.to_string())];
        let response = self
            .send(
                Method::POST,
                Some(key),
                &[("uploads".to_string(), String::new())],
                None,
                &headers,
            )
            .await?;
        let response = ensure_success(response).await?;
        let body = response.text().await.map_err(|e| {
            CosError::Backend(format!(
                "failed to read COS initiate-multipart response: {e}"
            ))
        })?;
        xml_text(&body, "uploadid")
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CosError::Backend("COS initiate-multipart response has no UploadId".into())
            })
    }

    /// `PUT /{key}?partNumber=N&uploadId=..` → the part `ETag`.
    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Vec<u8>,
    ) -> Result<String> {
        let headers = vec![("content-type".to_string(), PUT_CONTENT_TYPE.to_string())];
        let params = vec![
            ("partNumber".to_string(), part_number.to_string()),
            ("uploadId".to_string(), upload_id.to_string()),
        ];
        let response = self
            .send(Method::PUT, Some(key), &params, Some(body), &headers)
            .await?;
        let response = ensure_success(response).await?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(multipart::normalize_etag)
            .unwrap_or_default();
        if etag.is_empty() {
            return Err(CosError::Backend(format!(
                "COS upload-part response for part {part_number} has no ETag"
            )));
        }
        Ok(etag)
    }

    /// `POST /{key}?uploadId=..` with the `<CompleteMultipartUpload>` body.
    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<()> {
        let body = multipart::complete_multipart_body(parts);
        let headers = vec![(
            "content-type".to_string(),
            COMPLETE_CONTENT_TYPE.to_string(),
        )];
        let response = self
            .send(
                Method::POST,
                Some(key),
                &[("uploadId".to_string(), upload_id.to_string())],
                Some(body.into_bytes()),
                &headers,
            )
            .await?;
        ensure_success(response).await?;
        Ok(())
    }

    /// `DELETE /{key}?uploadId=..` — discards an in-flight multipart upload and its
    /// parts (a `404 NoSuchUpload` means it is already gone).
    pub async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        let response = self
            .send(
                Method::DELETE,
                Some(key),
                &[("uploadId".to_string(), upload_id.to_string())],
                None,
                &[],
            )
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        ensure_success(response).await?;
        Ok(())
    }

    /// One conditional `PUT`: the extra header (`if-match` / `if-none-match`) is
    /// signed and sent, and `412 Precondition Failed` maps to
    /// [`ConditionalWrite::Conflict`] instead of an error.
    async fn conditional_put(
        &self,
        key: &str,
        data: &[u8],
        condition: (&str, &str),
    ) -> Result<ConditionalWrite> {
        let headers = vec![
            ("content-type".to_string(), PUT_CONTENT_TYPE.to_string()),
            (condition.0.to_string(), condition.1.to_string()),
        ];
        let response = self
            .send(Method::PUT, Some(key), &[], Some(data.to_vec()), &headers)
            .await?;
        match response.status().as_u16() {
            200..=299 => Ok(ConditionalWrite::Written),
            412 => Ok(ConditionalWrite::Conflict),
            _ => Err(error_from_response(response).await),
        }
    }

    /// Headers that are signed: `host`, the session token when present, and the
    /// operation-specific extras (`content-type`, `content-md5`).
    fn signed_headers(&self, extra: &[(String, String)]) -> Vec<(String, String)> {
        let mut headers = vec![("host".to_string(), self.config.host())];
        if let Some(token) = self.config.credentials.security_token() {
            headers.push(("x-cos-security-token".to_string(), token.to_string()));
        }
        headers.extend(extra.iter().cloned());
        headers
    }

    /// Signs and sends a request; `object_key` is `None` for bucket-level APIs.
    ///
    /// Transient failures are retried with exponential backoff and jitter for
    /// **idempotent** operations (`GET`/`HEAD`/`PUT`/`DELETE`): transport errors,
    /// timeouts, `408`, `429` and `5xx`. Other `4xx` answers are caller errors and
    /// are returned immediately. `POST` is never replayed because it creates
    /// server-side state (`?uploads`, `CompleteMultipartUpload`, `?delete`).
    async fn send(
        &self,
        method: Method,
        object_key: Option<&str>,
        params: &[(String, String)],
        body: Option<Vec<u8>>,
        extra_headers: &[(String, String)],
    ) -> Result<Response> {
        self.ensure_open()?;

        let max_attempts = if method_is_idempotent(&method) {
            self.config.max_retries.saturating_add(1)
        } else {
            1
        };

        let mut body = body;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            // A body can only be replayed while an extra copy exists: keep one as
            // long as another attempt is still allowed, and move the original on the
            // last allowed attempt (the failure-free path when `max_retries == 0`).
            let payload = if attempt < max_attempts {
                body.clone()
            } else {
                body.take()
            };

            let result = self
                .send_once(&method, object_key, params, payload, extra_headers)
                .await;

            let retryable = attempt < max_attempts
                && (match &result {
                    Ok(response) => is_retryable_status(response.status()),
                    Err(error) => error.is_retryable(),
                });
            if !retryable {
                return result;
            }

            let reason = match &result {
                Ok(response) => format!("HTTP {}", response.status().as_u16()),
                Err(error) => error.to_string(),
            };
            tracing::warn!(
                attempt,
                max_attempts,
                method = method.as_str(),
                reason = %reason,
                "COS request failed, retrying with backoff"
            );
            // Release the response (and its connection slot) before sleeping.
            drop(result);
            tokio::time::sleep(retry_delay(attempt)).await;
        }
    }

    /// One signed attempt of [`Self::send`].
    async fn send_once(
        &self,
        method: &Method,
        object_key: Option<&str>,
        params: &[(String, String)],
        body: Option<Vec<u8>>,
        extra_headers: &[(String, String)],
    ) -> Result<Response> {
        let raw_path = match object_key {
            Some(key) => format!("/{key}"),
            None => "/".to_string(),
        };
        let headers = self.signed_headers(extra_headers);
        let key_time = self.key_time();
        // The signature covers the raw path while the request line carries the
        // percent-encoded one (spec: `HttpString` uses `UriPathname` verbatim).
        let signed = signature::sign_request(
            &self.config.credentials,
            method.as_str(),
            &raw_path,
            params,
            &headers,
            &key_time,
        );

        let mut url = format!(
            "{}{}",
            self.config.base_url(),
            signature::uri_encode_path(&raw_path)
        );
        if !params.is_empty() {
            url.push('?');
            url.push_str(&query_string(params));
        }

        let mut request = self
            .client
            .request(method.clone(), url)
            .header(reqwest::header::AUTHORIZATION, signed.authorization);
        for (name, value) in &headers {
            // `host` is produced by the HTTP layer from the URL; re-setting it would
            // be rejected by some stacks.
            if name != "host" {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        if let Some(body) = body {
            request = request.body(body);
        }

        request
            .send()
            .await
            .map_err(|e| CosError::Backend(format!("COS request failed: {e}")))
    }
}

pub(crate) fn query_string(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                signature::uri_encode(key),
                signature::uri_encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Converts a non-2xx response into [`CosError::Service`], keeping the status code
/// (and the COS `<Error><Code>` when the body carries one).
async fn error_from_response(response: Response) -> CosError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let code = xml_text(&body, "code").unwrap_or_default();
    let message = xml_text(&body, "message").unwrap_or_default();
    CosError::Service {
        status,
        code,
        message: if message.trim().is_empty() {
            body.trim().to_string()
        } else {
            message
        },
    }
}

async fn ensure_success(response: Response) -> Result<Response> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(error_from_response(response).await)
    }
}

/// Whether replaying the request is safe.
///
/// COS `POST` creates server-side state (`?uploads`, `CompleteMultipartUpload`,
/// `?delete`), so it is never retried; `GET`/`HEAD`/`PUT`/`DELETE` are idempotent.
fn method_is_idempotent(method: &Method) -> bool {
    *method == Method::GET
        || *method == Method::HEAD
        || *method == Method::PUT
        || *method == Method::DELETE
}

/// Transient statuses worth another attempt.
///
/// Every other `4xx` (`403` signature errors, `404`, `409`, ...) is a caller error
/// that a retry cannot fix.
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

/// Exponential backoff with full jitter: `base * 2^(attempt - 1)`, capped at
/// [`MAX_RETRY_DELAY`], then spread over the upper half of that window so a herd
/// of clients does not retry in lock-step.
fn retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    let base_ms = DEFAULT_RETRY_BASE_DELAY.as_millis() as u64;
    let cap_ms = MAX_RETRY_DELAY.as_millis() as u64;
    let ceiling = (base_ms << shift).min(cap_ms);
    // Clock-derived jitter keeps the delay inside `[ceiling / 2, ceiling]`.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_nanos()))
        .unwrap_or(0);
    let half = ceiling / 2;
    Duration::from_millis((half + nanos % (half + 1)).min(ceiling))
}

#[async_trait]
impl CosObjectStore for CosHttpObjectStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let response = self.send(Method::GET, Some(key), &[], None, &[]).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = ensure_success(response).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| CosError::Backend(format!("failed to read COS response body: {e}")))?;
        Ok(Some(bytes.to_vec()))
    }

    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        if self.should_use_multipart(data.len()) {
            return self.put_multipart(key, data).await;
        }
        let headers = vec![("content-type".to_string(), PUT_CONTENT_TYPE.to_string())];
        let response = self
            .send(Method::PUT, Some(key), &[], Some(data.to_vec()), &headers)
            .await?;
        ensure_success(response).await?;
        Ok(())
    }

    async fn get_etagged(&self, key: &str) -> Result<Option<EtaggedObject>> {
        let response = self.send(Method::GET, Some(key), &[], None, &[]).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = ensure_success(response).await?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CosError::Backend(format!("COS GET {key} returned no ETag header")))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| CosError::Backend(format!("failed to read COS response body: {e}")))?;
        Ok(Some(EtaggedObject {
            etag,
            data: bytes.to_vec(),
        }))
    }

    async fn put_if_match(&self, key: &str, data: &[u8], etag: &str) -> Result<ConditionalWrite> {
        self.conditional_put(key, data, ("if-match", etag)).await
    }

    async fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<ConditionalWrite> {
        self.conditional_put(key, data, ("if-none-match", "*"))
            .await
    }

    fn supports_conditional_writes(&self) -> bool {
        true
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let response = self.send(Method::DELETE, Some(key), &[], None, &[]).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            // Deleting a missing object is a no-op, matching the trait contract.
            return Ok(());
        }
        ensure_success(response).await?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let response = self.send(Method::HEAD, Some(key), &[], None, &[]).await?;
        match response.status().as_u16() {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(error_from_response(response).await),
        }
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .list_page(prefix, token.as_deref(), DEFAULT_LIST_PAGE_SIZE)
                .await?;
            keys.extend(page.keys);
            match page.next_continuation_token {
                Some(next) => token = Some(next),
                None => return Ok(keys),
            }
        }
    }

    async fn list_page(
        &self,
        prefix: &str,
        continuation_token: Option<&str>,
        max_keys: usize,
    ) -> Result<ObjectListPage> {
        let max_keys = if max_keys == 0 {
            MAX_LIST_KEYS
        } else {
            max_keys.min(MAX_LIST_KEYS)
        };
        let mut params = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), max_keys.to_string()),
        ];
        if !prefix.is_empty() {
            params.push(("prefix".to_string(), prefix.to_string()));
        }
        if let Some(token) = continuation_token.filter(|t| !t.is_empty()) {
            params.push(("continuation-token".to_string(), token.to_string()));
        }

        let response = self.send(Method::GET, None, &params, None, &[]).await?;
        let response = ensure_success(response).await?;
        let body = response
            .text()
            .await
            .map_err(|e| CosError::Backend(format!("failed to read COS list response: {e}")))?;

        let is_truncated = xml_text(&body, "istruncated")
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let next_continuation_token = if is_truncated {
            xml_text(&body, "nextcontinuationtoken").filter(|t| !t.trim().is_empty())
        } else {
            None
        };

        Ok(ObjectListPage {
            keys: parse_list_keys(&body),
            next_continuation_token,
        })
    }

    async fn delete_many(&self, keys: &[String]) -> Result<()> {
        for batch in keys.chunks(MAX_DELETE_BATCH) {
            let body = delete_objects_body(batch);
            let headers = vec![
                ("content-type".to_string(), DELETE_CONTENT_TYPE.to_string()),
                ("content-md5".to_string(), content_md5(body.as_bytes())),
            ];
            let response = self
                .send(
                    Method::POST,
                    None,
                    &[("delete".to_string(), String::new())],
                    Some(body.into_bytes()),
                    &headers,
                )
                .await?;
            let response = ensure_success(response).await?;

            // In quiet mode COS only reports per-key failures in the body (HTTP 200).
            let text = response.text().await.unwrap_or_default();
            if let Some(code) = xml_text(&text, "code").filter(|c| !c.trim().is_empty()) {
                return Err(CosError::Service {
                    status: 200,
                    code,
                    message: xml_text(&text, "message").unwrap_or_default(),
                });
            }
        }
        Ok(())
    }

    fn close(&self) {
        // Idempotent by construction: the flag is set, never toggled back.
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn content_md5(body: &[u8]) -> String {
    let digest = md5::compute(body);
    base64::engine::general_purpose::STANDARD.encode(digest.0)
}

fn delete_objects_body(keys: &[String]) -> String {
    let mut body = String::from("<Delete><Quiet>true</Quiet>");
    for key in keys {
        body.push_str("<Object><Key>");
        body.push_str(&escape_xml(key));
        body.push_str("</Key></Object>");
    }
    body.push_str("</Delete>");
    body
}

pub(crate) fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Extracts the text content of the first element with the given (lowercase) name.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut capture = false;
    let mut depth = 0usize;
    let mut out = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if capture {
                    depth += 1;
                } else if local_name(&e) == tag {
                    capture = true;
                    depth = 1;
                }
            }
            Ok(Event::Empty(e)) => {
                if !capture && local_name(&e) == tag {
                    return Some(String::new());
                }
            }
            Ok(Event::End(_)) => {
                if capture {
                    depth -= 1;
                    if depth == 0 {
                        return Some(out);
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if capture {
                    if let Ok(decoded) = e.decode() {
                        if let Ok(unescaped) = quick_xml::escape::unescape(&decoded) {
                            out.push_str(&unescaped);
                        }
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if capture {
                    if let Ok(decoded) = e.decode() {
                        out.push_str(&decoded);
                    }
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if capture {
                    if let Some(text) = e
                        .decode()
                        .ok()
                        .and_then(|name| quick_xml::escape::resolve_predefined_entity(&name))
                    {
                        out.push_str(text);
                    }
                }
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

/// Collects every `<Contents><Key>` value (COS `ListBucketResult`).
fn parse_list_keys(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    let mut in_contents = 0usize;
    let mut current: Option<String> = None;
    let mut keys = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local_name(&e).as_str() {
                "contents" => in_contents += 1,
                "key" if in_contents > 0 => current = Some(String::new()),
                _ => {}
            },
            Ok(Event::End(e)) => match local_name_end(&e).as_str() {
                "contents" => in_contents = in_contents.saturating_sub(1),
                "key" if in_contents > 0 => {
                    if let Some(key) = current.take() {
                        keys.push(key);
                    }
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if let Some(current) = current.as_mut() {
                    if let Ok(decoded) = e.decode() {
                        if let Ok(unescaped) = quick_xml::escape::unescape(&decoded) {
                            current.push_str(&unescaped);
                        }
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if let Some(current) = current.as_mut() {
                    if let Ok(decoded) = e.decode() {
                        current.push_str(&decoded);
                    }
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if let Some(current) = current.as_mut() {
                    if let Some(text) = e
                        .decode()
                        .ok()
                        .and_then(|name| quick_xml::escape::resolve_predefined_entity(&name))
                    {
                        current.push_str(text);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    keys
}

/// Collects every `<Part>` of a COS `ListPartsResult`
/// (`<PartNumber>`, `<ETag>`, `<Size>`).
///
/// Entries without a usable part number are dropped; a missing/unparsable `<Size>`
/// becomes `0`, which makes [`crate::CosHttpObjectStore::resume_multipart`] treat the
/// part as "not stored yet" and upload it again.
fn parse_list_parts(xml: &str) -> Vec<UploadedPart> {
    #[derive(Clone, Copy)]
    enum Field {
        PartNumber,
        ETag,
        Size,
    }

    let mut reader = Reader::from_str(xml);
    let mut in_part = false;
    let mut field: Option<Field> = None;
    let mut buf = String::new();
    let mut part = UploadedPart::default();
    let mut parts = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local_name(&e).as_str() {
                "part" => {
                    in_part = true;
                    part = UploadedPart::default();
                }
                "partnumber" if in_part => {
                    field = Some(Field::PartNumber);
                    buf.clear();
                }
                "etag" if in_part => {
                    field = Some(Field::ETag);
                    buf.clear();
                }
                "size" if in_part => {
                    field = Some(Field::Size);
                    buf.clear();
                }
                _ => {}
            },
            Ok(Event::End(e)) => match local_name_end(&e).as_str() {
                "part" if in_part => {
                    in_part = false;
                    if part.part_number > 0 {
                        parts.push(std::mem::take(&mut part));
                    }
                }
                "partnumber" | "etag" | "size" if in_part => match field.take() {
                    Some(Field::PartNumber) => part.part_number = buf.trim().parse().unwrap_or(0),
                    Some(Field::ETag) => part.etag = multipart::normalize_etag(&buf),
                    Some(Field::Size) => part.size = buf.trim().parse().unwrap_or(0),
                    None => {}
                },
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if field.is_some() {
                    if let Ok(decoded) = e.decode() {
                        if let Ok(unescaped) = quick_xml::escape::unescape(&decoded) {
                            buf.push_str(&unescaped);
                        }
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if field.is_some() {
                    if let Ok(decoded) = e.decode() {
                        buf.push_str(&decoded);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    parts
}

fn local_name(event: &quick_xml::events::BytesStart<'_>) -> String {
    String::from_utf8_lossy(event.local_name().as_ref()).to_ascii_lowercase()
}

fn local_name_end(event: &quick_xml::events::BytesEnd<'_>) -> String {
    String::from_utf8_lossy(event.local_name().as_ref()).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    //! HTTP-level tests against a local `httpmock` server: no cloud credentials and
    //! no network access. The signature algorithm itself is pinned by the official
    //! golden vectors in [`crate::signature`]; here we assert the *wiring* — path,
    //! query parameters, signed header list and `Authorization` value.

    use super::*;
    use agentscope_core::state::AgentStateStore;
    use httpmock::prelude::*;
    use std::sync::atomic::AtomicUsize;

    const CLOCK: i64 = 1_700_000_000;
    const KEY_TIME: &str = "1700000000;1700000900";
    const BUCKET: &str = "examplebucket-1250000000";

    fn clock() -> ClockFn {
        Arc::new(|| CLOCK)
    }

    fn store_for(server: &MockServer) -> CosHttpObjectStore {
        CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("AKIDexample")
            .secret_key("secret-key")
            .endpoint(server.base_url())
            .clock(clock())
            .build()
            .unwrap()
    }

    /// Recomputes the `Authorization` header the store must produce for a request.
    /// Header assembly mirrors `signed_headers` (host, session token, extras).
    fn expected_authorization(
        store: &CosHttpObjectStore,
        method: &str,
        object_key: Option<&str>,
        params: &[(String, String)],
        extra: &[(&str, &str)],
    ) -> String {
        let config = store.config();
        let mut headers = vec![("host".to_string(), config.host())];
        if let Some(token) = config.credentials.security_token() {
            headers.push(("x-cos-security-token".to_string(), token.to_string()));
        }
        headers.extend(
            extra
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
        );
        let path = match object_key {
            Some(key) => format!("/{key}"),
            None => "/".to_string(),
        };
        signature::sign_request(
            &config.credentials,
            method,
            &path,
            params,
            &headers,
            KEY_TIME,
        )
        .authorization
    }

    #[test]
    fn builder_rejects_invalid_configuration() {
        let err = CosHttpObjectStore::builder()
            .bucket("   ")
            .region("ap-beijing")
            .secret_id("id")
            .secret_key("key")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("bucketName must not be blank"));

        // Missing bucket entirely (the Java `null` bucket case).
        let err = CosHttpObjectStore::builder()
            .region("ap-beijing")
            .secret_id("id")
            .secret_key("key")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("bucketName must not be blank"));

        let err = CosHttpObjectStore::builder()
            .bucket("bucket/with/slash")
            .region("ap-beijing")
            .secret_id("id")
            .secret_key("key")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("without '/' or spaces"));

        let err = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_key("key")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("secretId must not be blank"));

        let err = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("id")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("secretKey must not be blank"));

        // Without an endpoint override the region is mandatory.
        let err = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .secret_id("id")
            .secret_key("key")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("region must not be blank"));

        let err = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .secret_id("id")
            .secret_key("key")
            .endpoint("127.0.0.1:9000")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("endpoint must start with"));
    }

    #[test]
    fn public_domain_urls_are_derived_from_bucket_and_region() {
        let store = CosHttpObjectStore::new(BUCKET, "ap-beijing", "id", "key").unwrap();
        assert_eq!(
            store.config().base_url(),
            "https://examplebucket-1250000000.cos.ap-beijing.myqcloud.com"
        );
        assert_eq!(
            store.config().host(),
            "examplebucket-1250000000.cos.ap-beijing.myqcloud.com"
        );
        assert_eq!(store.config().sign_expire_secs(), DEFAULT_SIGN_EXPIRE_SECS);
        // Secrets must never show up in diagnostics.
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("secret-key"));
        assert!(rendered.contains("examplebucket-1250000000"));
    }

    #[tokio::test]
    async fn put_sends_a_signed_request() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        let key = "agentscope/state/u1/s1/k.json";
        let extra = [("content-type", PUT_CONTENT_TYPE)];
        let auth = expected_authorization(&store, "PUT", Some(key), &[], &extra);
        assert!(auth.contains("q-header-list=content-type;host"));
        assert!(auth.contains("q-url-param-list=&q-signature="));
        assert!(auth.contains(&format!("q-sign-time={KEY_TIME}&q-key-time={KEY_TIME}")));

        let mock = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/agentscope/state/u1/s1/k.json")
                    .header("authorization", auth.as_str())
                    .header("content-type", PUT_CONTENT_TYPE)
                    .body("{\"a\":1}");
                then.status(200);
            })
            .await;

        store.put(key, b"{\"a\":1}").await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn get_returns_none_only_for_404() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let present = server
            .mock_async(|when, then| {
                when.method(GET).path("/present.txt");
                then.status(200).body("hello");
            })
            .await;
        let absent = server
            .mock_async(|when, then| {
                when.method(GET).path("/absent.txt");
                then.status(404)
                    .body("<Error><Code>NoSuchKey</Code><Message>missing</Message></Error>");
            })
            .await;

        assert_eq!(store.get("present.txt").await.unwrap().unwrap(), b"hello");
        assert_eq!(
            store.get_string("present.txt").await.unwrap().unwrap(),
            "hello"
        );
        assert!(store.get("absent.txt").await.unwrap().is_none());
        assert!(store.get_string("absent.txt").await.unwrap().is_none());

        // `get` + `get_string` each issue one request.
        present.assert_hits_async(2).await;
        absent.assert_hits_async(2).await;
    }

    #[tokio::test]
    async fn forbidden_is_an_error_not_a_missing_object() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/denied.txt");
                then.status(403).body(
                    "<Error><Code>SignatureDoesNotMatch</Code>\
                     <Message>signature mismatch</Message></Error>",
                );
            })
            .await;

        let err = store.get("denied.txt").await.unwrap_err();
        assert_eq!(err.status(), Some(403));
        assert!(err.is_signature_error());
        assert!(!err.is_not_found());
        assert!(err.to_string().contains("SignatureDoesNotMatch"));
        match &err {
            CosError::Service {
                status,
                code,
                message,
            } => {
                assert_eq!(*status, 403);
                assert_eq!(code, "SignatureDoesNotMatch");
                assert_eq!(message, "signature mismatch");
            }
            other => panic!("expected a service error, got {other:?}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn exists_uses_head_and_maps_404_to_false() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let yes = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::HEAD).path("/there");
                then.status(200);
            })
            .await;
        let no = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::HEAD).path("/gone");
                then.status(404);
            })
            .await;

        assert!(store.exists("there").await.unwrap());
        assert!(!store.exists("gone").await.unwrap());
        yes.assert_async().await;
        no.assert_async().await;
    }

    /// `get_etagged` hands back the service `ETag` alongside the body; it is what the
    /// state store feeds into `If-Match`.
    #[tokio::test]
    async fn get_etagged_returns_the_service_etag() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/versioned.txt");
                then.status(200)
                    .header("ETag", "\"5d41402abc4b2a76b9719d911017c592\"")
                    .body("hello");
            })
            .await;

        let object = store.get_etagged("versioned.txt").await.unwrap().unwrap();
        assert_eq!(object.data, b"hello");
        assert_eq!(object.etag, "\"5d41402abc4b2a76b9719d911017c592\"");
        mock.assert_async().await;

        let missing = server
            .mock_async(|when, then| {
                when.method(GET).path("/absent.txt");
                then.status(404)
                    .body("<Error><Code>NoSuchKey</Code></Error>");
            })
            .await;
        assert!(store.get_etagged("absent.txt").await.unwrap().is_none());
        missing.assert_async().await;
    }

    /// A response without `ETag` cannot be used as a precondition, so it is reported
    /// instead of being silently treated as "unversioned".
    #[tokio::test]
    async fn get_etagged_requires_an_etag_header() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        server
            .mock_async(|when, then| {
                when.method(GET).path("/no-etag.txt");
                then.status(200).body("hello");
            })
            .await;

        let err = store.get_etagged("no-etag.txt").await.unwrap_err();
        assert!(err.to_string().contains("no ETag"), "got: {err}");
    }

    /// `put_if_match` signs and sends `If-Match`, maps `412` to a conflict and a `2xx`
    /// to a write.
    #[tokio::test]
    async fn put_if_match_signs_the_condition_and_maps_412_to_conflict() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        let key = "agentscope/state/u1/s1/k.version";
        let extra = [
            ("content-type", PUT_CONTENT_TYPE),
            ("if-match", "\"etag-1\""),
        ];
        let auth = expected_authorization(&store, "PUT", Some(key), &[], &extra);
        assert!(
            auth.contains("q-header-list=content-type;host;if-match"),
            "the conditional header must be part of the signature: {auth}"
        );

        let conflicting = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/agentscope/state/u1/s1/k.version")
                    .header("if-match", "\"etag-1\"")
                    .header("authorization", auth.as_str())
                    .body("2");
                then.status(412)
                    .body("<Error><Code>PreconditionFailed</Code></Error>");
            })
            .await;

        assert_eq!(
            store.put_if_match(key, b"2", "\"etag-1\"").await.unwrap(),
            ConditionalWrite::Conflict
        );
        conflicting.assert_hits_async(1).await;

        let written = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/agentscope/state/u1/s1/k.version")
                    .header("if-match", "\"etag-2\"")
                    .body("3");
                then.status(200);
            })
            .await;
        assert_eq!(
            store.put_if_match(key, b"3", "\"etag-2\"").await.unwrap(),
            ConditionalWrite::Written
        );
        written.assert_hits_async(1).await;
    }

    /// Create-if-absent uses `If-None-Match: *`; a `412` means somebody created the
    /// object first.
    #[tokio::test]
    async fn put_if_absent_sends_if_none_match_star() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        let key = "agentscope/state/u1/s1/k.version";

        let created = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/agentscope/state/u1/s1/k.version")
                    .header("if-none-match", "*")
                    .body("1");
                then.status(200);
            })
            .await;
        assert_eq!(
            store.put_if_absent(key, b"1").await.unwrap(),
            ConditionalWrite::Written
        );
        created.assert_hits_async(1).await;

        let raced = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/agentscope/state/u1/s1/k.version")
                    .header("if-none-match", "*")
                    .body("2");
                then.status(412);
            })
            .await;
        assert_eq!(
            store.put_if_absent(key, b"2").await.unwrap(),
            ConditionalWrite::Conflict
        );
        raced.assert_hits_async(1).await;
    }

    /// `CosAgentStateStore::exists` asks COS for exactly one key instead of listing the
    /// whole session prefix (Java sets `maxKeys=1`).
    #[tokio::test]
    async fn state_store_exists_asks_for_a_single_key() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        let state_store = crate::CosAgentStateStore::builder()
            .object_store(Arc::new(store))
            .build()
            .unwrap();

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/")
                    .query_param("list-type", "2")
                    .query_param("max-keys", "1")
                    .query_param("prefix", "agentscope/state/u1/s1/");
                then.status(200).body(
                    "<ListBucketResult><IsTruncated>true</IsTruncated>\
                     <Contents><Key>agentscope/state/u1/s1/k.json</Key></Contents>\
                     <NextContinuationToken>next</NextContinuationToken>\
                     </ListBucketResult>",
                );
            })
            .await;

        assert!(state_store.exists(Some("u1"), "s1").await);
        // A single request, and no follow-up page even though the listing is truncated.
        mock.assert_hits_async(1).await;
    }

    #[tokio::test]
    async fn delete_treats_404_as_success() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let ok = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/live");
                then.status(204);
            })
            .await;
        let missing = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/gone");
                then.status(404)
                    .body("<Error><Code>NoSuchKey</Code></Error>");
            })
            .await;

        store.delete("live").await.unwrap();
        store.delete("gone").await.unwrap();
        ok.assert_async().await;
        missing.assert_async().await;
    }

    #[tokio::test]
    async fn session_token_is_signed_and_sent() {
        let server = MockServer::start_async().await;
        let store = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("AKIDexample")
            .secret_key("secret-key")
            .security_token("session-token")
            .endpoint(server.base_url())
            .clock(clock())
            .build()
            .unwrap();

        let auth = expected_authorization(&store, "GET", Some("k"), &[], &[]);
        assert!(auth.contains("q-header-list=host;x-cos-security-token"));

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/k")
                    .header("x-cos-security-token", "session-token")
                    .header("authorization", auth.as_str());
                then.status(200).body("v");
            })
            .await;

        assert_eq!(store.get("k").await.unwrap().unwrap(), b"v");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn close_is_idempotent_and_blocks_further_requests() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        assert!(!store.is_closed());
        store.close();
        store.close(); // second close must not fail
        assert!(store.is_closed());

        assert!(matches!(
            store.get("k").await.unwrap_err(),
            CosError::Closed
        ));
        assert!(matches!(
            store.put("k", b"v").await.unwrap_err(),
            CosError::Closed
        ));
        assert!(matches!(
            store.exists("k").await.unwrap_err(),
            CosError::Closed
        ));
        assert!(matches!(
            store.list_page("", None, 10).await.unwrap_err(),
            CosError::Closed
        ));

        // Clones observe the same closed state (the flag is shared).
        let clone = store.clone();
        assert!(clone.is_closed());
    }

    #[tokio::test]
    async fn list_page_uses_list_type_2_and_returns_continuation_token() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let params = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), "2".to_string()),
            ("prefix".to_string(), "p/".to_string()),
        ];
        let auth = expected_authorization(&store, "GET", None, &params, &[]);
        // URL-parameter names are sorted for the signature, independent of the
        // order they are sent in.
        assert!(auth.contains("q-url-param-list=list-type;max-keys;prefix"));
        assert!(auth.contains("q-header-list=host&"));

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/")
                    .query_param("list-type", "2")
                    .query_param("max-keys", "2")
                    .query_param("prefix", "p/")
                    .header("authorization", auth.as_str());
                then.status(200)
                    .header("content-type", "application/xml")
                    .body(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                         <ListBucketResult>\
                         <IsTruncated>true</IsTruncated>\
                         <NextContinuationToken>tok-2</NextContinuationToken>\
                         <Contents><Key>p/a</Key><Size>1</Size></Contents>\
                         <Contents><Key>p/b&amp;c</Key><Size>2</Size></Contents>\
                         </ListBucketResult>",
                    );
            })
            .await;

        let page = store.list_page("p/", None, 2).await.unwrap();
        assert_eq!(page.keys, vec!["p/a".to_string(), "p/b&c".to_string()]);
        assert_eq!(page.next_continuation_token.as_deref(), Some("tok-2"));
        assert!(page.is_truncated());
        mock.assert_async().await;
    }

    /// A complete listing (`IsTruncated=false`) yields no token even if the body
    /// happens to contain a stale `NextContinuationToken`.
    #[tokio::test]
    async fn list_page_without_truncation_has_no_token() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/").query_param("list-type", "2");
                then.status(200).body(
                    "<ListBucketResult><IsTruncated>false</IsTruncated>\
                     <NextContinuationToken>stale</NextContinuationToken>\
                     <Contents><Key>only</Key></Contents></ListBucketResult>",
                );
            })
            .await;

        let page = store.list_page("", None, 0).await.unwrap();
        assert_eq!(page.keys, vec!["only".to_string()]);
        assert!(page.next_continuation_token.is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_prefix_follows_continuation_tokens() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        // Created first: httpmock evaluates mocks by ascending id and both share
        // path/prefix, but only the page-2 request carries a continuation token.
        let second = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/")
                    .query_param("list-type", "2")
                    .query_param("prefix", "logs/")
                    .query_param("max-keys", "1000")
                    .query_param("continuation-token", "tok-2");
                then.status(200).body(
                    "<ListBucketResult><IsTruncated>false</IsTruncated>\
                     <Contents><Key>logs/c</Key></Contents></ListBucketResult>",
                );
            })
            .await;
        let first = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/")
                    .query_param("list-type", "2")
                    .query_param("prefix", "logs/")
                    .query_param("max-keys", "1000");
                then.status(200).body(
                    "<ListBucketResult><IsTruncated>true</IsTruncated>\
                     <NextContinuationToken>tok-2</NextContinuationToken>\
                     <Contents><Key>logs/a</Key></Contents>\
                     <Contents><Key>logs/b</Key></Contents></ListBucketResult>",
                );
            })
            .await;

        assert_eq!(
            store.list_prefix("logs/").await.unwrap(),
            vec![
                "logs/a".to_string(),
                "logs/b".to_string(),
                "logs/c".to_string()
            ]
        );
        first.assert_async().await;
        second.assert_async().await;
    }

    #[tokio::test]
    async fn delete_many_posts_batched_delete_with_content_md5() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let keys = vec!["a/1".to_string(), "a/2".to_string(), "a<&>3".to_string()];
        let body = delete_objects_body(&keys);
        // The keys are XML-escaped; quiet mode keeps the response small.
        assert!(body.contains("<Object><Key>a&lt;&amp;&gt;3</Key></Object>"));
        assert!(body.contains("<Quiet>true</Quiet>"));
        let md5 = content_md5(body.as_bytes());

        let params = vec![("delete".to_string(), String::new())];
        let extra = [
            ("content-type", DELETE_CONTENT_TYPE),
            ("content-md5", md5.as_str()),
        ];
        let auth = expected_authorization(&store, "POST", None, &params, &extra);
        assert!(auth.contains("q-header-list=content-md5;content-type;host"));
        assert!(auth.contains("q-url-param-list=delete"));

        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .query_param("delete", "")
                    .header("content-type", DELETE_CONTENT_TYPE)
                    .header("content-md5", md5.as_str())
                    .header("authorization", auth.as_str())
                    .body(body.as_str());
                then.status(200).body("<DeleteResult></DeleteResult>");
            })
            .await;

        store.delete_many(&keys).await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn delete_many_splits_requests_at_1000_keys() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/").query_param("delete", "");
                then.status(200).body("<DeleteResult></DeleteResult>");
            })
            .await;

        let keys: Vec<String> = (0..MAX_DELETE_BATCH + 1)
            .map(|index| format!("k/{index:05}"))
            .collect();
        store.delete_many(&keys).await.unwrap();
        assert_eq!(mock.hits_async().await, 2, "1001 keys need two batches");

        // An empty key list must not issue a request at all.
        store.delete_many(&[]).await.unwrap();
        assert_eq!(mock.hits_async().await, 2);
    }

    /// Per-key failures arrive as HTTP 200 with an `<Error>` body in quiet mode.
    #[tokio::test]
    async fn delete_many_surfaces_per_key_errors() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200).body(
                    "<DeleteResult><Error><Key>k</Key>\
                     <Code>AccessDenied</Code><Message>nope</Message></Error></DeleteResult>",
                );
            })
            .await;

        let err = store.delete_many(&["k".to_string()]).await.unwrap_err();
        assert_eq!(err.status(), Some(200));
        assert!(err.to_string().contains("AccessDenied"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_errors_keep_the_status_code() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        server
            .mock_async(|when, then| {
                when.method(GET).path("/");
                then.status(500)
                    .body("<Error><Code>InternalError</Code></Error>");
            })
            .await;

        let err = store.list_prefix("p/").await.unwrap_err();
        assert_eq!(err.status(), Some(500));
        assert!(!err.is_signature_error());
    }

    // ------------------------------------------------------------------
    // Multipart upload (large bodies)
    // ------------------------------------------------------------------

    const MIB: usize = 1024 * 1024;

    /// Store with explicit multipart settings (the default threshold is 5 MiB).
    fn multipart_store(
        server: &MockServer,
        threshold: u64,
        part_size: u64,
        concurrency: usize,
    ) -> CosHttpObjectStore {
        CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("AKIDexample")
            .secret_key("secret-key")
            .endpoint(server.base_url())
            .clock(clock())
            .multipart_threshold(threshold)
            .multipart_part_size(part_size)
            .multipart_concurrency(concurrency)
            .build()
            .unwrap()
    }

    /// Below the threshold the body still travels as a single `PUT` (no
    /// `POST ?uploads`).
    #[tokio::test]
    async fn put_below_threshold_uses_a_single_put() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        assert_eq!(
            store.config().multipart_threshold(),
            multipart::DEFAULT_MULTIPART_THRESHOLD
        );

        let single = server
            .mock_async(|when, then| {
                when.method(PUT).path("/small.json");
                then.status(200);
            })
            .await;
        let multipart_entry = server
            .mock_async(|when, then| {
                when.method(POST).path("/small.json");
                then.status(200);
            })
            .await;

        let body = vec![b'x'; 1024];
        store.put("small.json", &body).await.unwrap();
        single.assert_hits_async(1).await;
        multipart_entry.assert_hits_async(0).await;
    }

    /// At the threshold (`len >= threshold`, Java `shouldUseMultipartUpload`) the
    /// store initiates, uploads one part and completes — asserting the signed
    /// parameter list of every step.
    #[tokio::test]
    async fn put_at_threshold_switches_to_multipart() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 8, multipart::DEFAULT_MULTIPART_PART_SIZE, 1);
        let key = "big.json";
        let data = b"{\"a\":1,}";
        assert_eq!(data.len(), 8);

        let initiate_params = vec![("uploads".to_string(), String::new())];
        let initiate_auth = expected_authorization(
            &store,
            "POST",
            Some(key),
            &initiate_params,
            &[("content-type", PUT_CONTENT_TYPE)],
        );
        let initiate = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/big.json")
                    .query_param("uploads", "")
                    .header("content-type", PUT_CONTENT_TYPE)
                    .header("authorization", initiate_auth.as_str());
                then.status(200).body(
                    r#"<InitiateMultipartUploadResult><Bucket>examplebucket-1250000000</Bucket><Key>big.json</Key><UploadId>u-1</UploadId></InitiateMultipartUploadResult>"#,
                );
            })
            .await;

        let part_params = vec![
            ("partNumber".to_string(), "1".to_string()),
            ("uploadId".to_string(), "u-1".to_string()),
        ];
        let part_auth = expected_authorization(
            &store,
            "PUT",
            Some(key),
            &part_params,
            &[("content-type", PUT_CONTENT_TYPE)],
        );
        let part = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/big.json")
                    .query_param("partNumber", "1")
                    .query_param("uploadId", "u-1")
                    .header("content-type", PUT_CONTENT_TYPE)
                    .header("authorization", part_auth.as_str())
                    .body(r#"{"a":1,}"#);
                then.status(200).header("ETag", r#""etag-1""#);
            })
            .await;

        let complete_params = vec![("uploadId".to_string(), "u-1".to_string())];
        let complete_auth = expected_authorization(
            &store,
            "POST",
            Some(key),
            &complete_params,
            &[("content-type", COMPLETE_CONTENT_TYPE)],
        );
        let complete_body = r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>"etag-1"</ETag></Part></CompleteMultipartUpload>"#;
        let complete = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/big.json")
                    .query_param("uploadId", "u-1")
                    .header("content-type", COMPLETE_CONTENT_TYPE)
                    .header("authorization", complete_auth.as_str())
                    .body(complete_body);
                then.status(200)
                    .body("<CompleteMultipartUploadResult></CompleteMultipartUploadResult>");
            })
            .await;

        // Signature coverage: `uploads` / `partNumber;uploadId` / `uploadId` are
        // lowercased and sorted by the canonicalization step, exactly like COS.
        assert!(initiate_auth.contains("q-url-param-list=uploads&q-signature="));
        assert!(part_auth.contains("q-url-param-list=partnumber;uploadid&q-signature="));
        assert!(complete_auth.contains("q-url-param-list=uploadid&q-signature="));

        store.put(key, data).await.unwrap();
        initiate.assert_async().await;
        part.assert_async().await;
        complete.assert_async().await;
    }

    /// A multi-part body is split at `part_size`, each part is uploaded with its
    /// exact bytes, and the completion lists the parts in ascending order — even
    /// with concurrent uploads that finish out of order.
    #[tokio::test]
    async fn multipart_uploads_sorted_parts_and_completes() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 2);
        let key = "blob.bin";
        // ASCII only: `httpmock` matches request bodies as (lossy) UTF-8 strings.
        let data: Vec<u8> = (0..(2 * MIB + MIB / 2))
            .map(|i| b'a' + (i % 26) as u8)
            .collect();

        server
            .mock_async(|when, then| {
                when.method(POST).path("/blob.bin").query_param("uploads", "");
                then.status(200).body(
                    r#"<InitiateMultipartUploadResult><Bucket>b</Bucket><Key>blob.bin</Key><UploadId>u-multi</UploadId></InitiateMultipartUploadResult>"#,
                );
            })
            .await;

        let mut parts = Vec::new();
        for part_number in 1..=3u32 {
            let start = (part_number as usize - 1) * MIB;
            let end = (start + MIB).min(data.len());
            let expected_bytes = String::from_utf8(data[start..end].to_vec()).unwrap();
            let expected_len = expected_bytes.len();
            let etag = format!("etag-{part_number}");
            let mock = server
                .mock_async(move |when, then| {
                    when.method(PUT)
                        .path("/blob.bin")
                        .query_param("partNumber", part_number.to_string())
                        .query_param("uploadId", "u-multi")
                        .body(expected_bytes);
                    then.status(200).header("ETag", format!("\"{etag}\""));
                })
                .await;
            parts.push((mock, expected_len, part_number));
        }

        let complete = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-multi")
                    .body(
                        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>"etag-1"</ETag></Part><Part><PartNumber>2</PartNumber><ETag>"etag-2"</ETag></Part><Part><PartNumber>3</PartNumber><ETag>"etag-3"</ETag></Part></CompleteMultipartUpload>"#,
                    );
                then.status(200)
                    .body("<CompleteMultipartUploadResult></CompleteMultipartUploadResult>");
            })
            .await;

        store.put(key, &data).await.unwrap();

        for (mock, expected_len, part_number) in &parts {
            mock.assert_async().await;
            assert_eq!(
                *expected_len,
                if *part_number < 3 { MIB } else { MIB / 2 },
                "unexpected size for part {part_number}"
            );
        }
        complete.assert_async().await;
    }

    /// A failed part rolls the upload back with `DELETE ?uploadId=..` and the
    /// original error is propagated.
    #[tokio::test]
    async fn multipart_aborts_when_a_part_fails() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let data = vec![b'z'; 2 * MIB];

        server
            .mock_async(|when, then| {
                when.method(POST).path("/k").query_param("uploads", "");
                then.status(200).body(
                    r#"<InitiateMultipartUploadResult><UploadId>u-err</UploadId></InitiateMultipartUploadResult>"#,
                );
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/k")
                    .query_param("partNumber", "1")
                    .query_param("uploadId", "u-err");
                then.status(200).header("ETag", r#""e1""#);
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/k")
                    .query_param("partNumber", "2")
                    .query_param("uploadId", "u-err");
                then.status(500)
                    .body("<Error><Code>InternalError</Code><Message>boom</Message></Error>");
            })
            .await;
        let complete = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/k")
                    .query_param("uploadId", "u-err");
                then.status(200);
            })
            .await;
        let abort_params = vec![("uploadId".to_string(), "u-err".to_string())];
        let abort_auth = expected_authorization(&store, "DELETE", Some("k"), &abort_params, &[]);
        assert!(abort_auth.contains("q-url-param-list=uploadid&q-signature="));
        let abort = server
            .mock_async(|when, then| {
                when.method(DELETE)
                    .path("/k")
                    .query_param("uploadId", "u-err")
                    .header("authorization", abort_auth.as_str());
                then.status(204);
            })
            .await;

        let err = store.put("k", &data).await.unwrap_err();
        assert_eq!(err.status(), Some(500));
        assert!(err.to_string().contains("InternalError"));
        abort.assert_hits_async(1).await;
        complete.assert_hits_async(0).await;
    }

    /// A rejected completion (`400 InvalidPart`) also aborts.
    #[tokio::test]
    async fn multipart_aborts_when_complete_is_rejected() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let data = vec![b'q'; 16];

        server
            .mock_async(|when, then| {
                when.method(POST).path("/k").query_param("uploads", "");
                then.status(200)
                    .body(r#"<InitiateMultipartUploadResult><UploadId>u-bad</UploadId></InitiateMultipartUploadResult>"#);
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(PUT).path("/k").query_param("partNumber", "1");
                then.status(200).header("ETag", r#""e1""#);
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/k")
                    .query_param("uploadId", "u-bad");
                then.status(400).body(
                    "<Error><Code>InvalidPart</Code><Message>part mismatch</Message></Error>",
                );
            })
            .await;
        let abort = server
            .mock_async(|when, then| {
                when.method(DELETE)
                    .path("/k")
                    .query_param("uploadId", "u-bad");
                then.status(204);
            })
            .await;

        let err = store.put("k", &data).await.unwrap_err();
        assert_eq!(err.status(), Some(400));
        assert!(err.to_string().contains("InvalidPart"));
        abort.assert_hits_async(1).await;
    }

    /// `404 NoSuchUpload` while aborting means the upload is already gone; the
    /// original failure must still surface.
    #[tokio::test]
    async fn multipart_abort_tolerates_a_missing_upload() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);

        server
            .mock_async(|when, then| {
                when.method(POST).path("/k").query_param("uploads", "");
                then.status(200)
                    .body(r#"<InitiateMultipartUploadResult><UploadId>u-gone</UploadId></InitiateMultipartUploadResult>"#);
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(PUT).path("/k").query_param("partNumber", "1");
                then.status(503)
                    .body("<Error><Code>ServiceUnavailable</Code></Error>");
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(DELETE)
                    .path("/k")
                    .query_param("uploadId", "u-gone");
                then.status(404)
                    .body("<Error><Code>NoSuchUpload</Code></Error>");
            })
            .await;

        let err = store.put_multipart("k", b"body").await.unwrap_err();
        assert_eq!(err.status(), Some(503));
        assert!(err.to_string().contains("ServiceUnavailable"));
    }

    /// A failed initiation yields no `UploadId`, so there is nothing to abort.
    #[tokio::test]
    async fn multipart_initiate_failure_does_not_abort() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);

        server
            .mock_async(|when, then| {
                when.method(POST).path("/k").query_param("uploads", "");
                then.status(403)
                    .body("<Error><Code>AccessDenied</Code><Message>no</Message></Error>");
            })
            .await;
        let abort = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/k");
                then.status(204);
            })
            .await;

        let err = store.put_multipart("k", b"body").await.unwrap_err();
        assert_eq!(err.status(), Some(403));
        abort.assert_hits_async(0).await;
    }

    /// An initiation response without `UploadId` is a protocol error, not a
    /// silently empty id.
    #[tokio::test]
    async fn multipart_initiate_requires_an_upload_id() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);

        server
            .mock_async(|when, then| {
                when.method(POST).path("/k").query_param("uploads", "");
                then.status(200).body(
                    "<InitiateMultipartUploadResult><UploadId></UploadId></InitiateMultipartUploadResult>",
                );
            })
            .await;

        let err = store.put_multipart("k", b"body").await.unwrap_err();
        assert!(err.to_string().contains("no UploadId"));
    }

    /// An empty body cannot be uploaded in parts (and is rejected before any
    /// request is made); `put` keeps writing empty objects with a single `PUT`.
    #[tokio::test]
    async fn multipart_rejects_an_empty_body() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let any_request = server
            .mock_async(|when, then| {
                when.method(POST).path("/k");
                then.status(200);
            })
            .await;

        let err = store.put_multipart("k", b"").await.unwrap_err();
        assert!(err.to_string().contains("non-empty body"));
        any_request.assert_hits_async(0).await;

        // Even with `threshold = 0`, `put` falls back to a single PUT for empty
        // bodies (COS multipart requires at least one part).
        let single = server
            .mock_async(|when, then| {
                when.method(PUT).path("/k");
                then.status(200);
            })
            .await;
        store.put("k", b"").await.unwrap();
        single.assert_hits_async(1).await;
        any_request.assert_hits_async(0).await;
    }

    /// `ListParts` is signed with `uploadId` + `max-parts` and a truncated answer is
    /// followed with `part-number-marker`.
    #[tokio::test]
    async fn list_parts_follows_part_number_markers() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let key = "blob.bin";

        let first_params = vec![
            ("uploadId".to_string(), "u-list".to_string()),
            (
                "max-parts".to_string(),
                multipart::MAX_LIST_PARTS.to_string(),
            ),
        ];
        let first_auth = expected_authorization(&store, "GET", Some(key), &first_params, &[]);
        let first_page = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-list")
                    .query_param("max-parts", multipart::MAX_LIST_PARTS.to_string())
                    .header("authorization", first_auth.as_str());
                then.status(200).body(
                    "<ListPartsResult><Part><PartNumber>1</PartNumber><ETag>\"e1\"</ETag>\
                     <Size>1048576</Size></Part><IsTruncated>true</IsTruncated>\
                     <NextPartNumberMarker>1</NextPartNumberMarker></ListPartsResult>",
                );
            })
            .await;

        let second_params = vec![
            ("uploadId".to_string(), "u-list".to_string()),
            (
                "max-parts".to_string(),
                multipart::MAX_LIST_PARTS.to_string(),
            ),
            ("part-number-marker".to_string(), "1".to_string()),
        ];
        let second_auth = expected_authorization(&store, "GET", Some(key), &second_params, &[]);
        let second_page = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-list")
                    .query_param("part-number-marker", "1")
                    .header("authorization", second_auth.as_str());
                then.status(200).body(
                    "<ListPartsResult><Part><PartNumber>2</PartNumber><ETag>\"e2\"</ETag>\
                     <Size>524288</Size></Part><IsTruncated>false</IsTruncated></ListPartsResult>",
                );
            })
            .await;

        let parts = store.list_parts(key, "u-list").await.unwrap();
        assert_eq!(
            parts,
            vec![
                UploadedPart {
                    part_number: 1,
                    etag: "e1".to_string(),
                    size: MIB as u64,
                },
                UploadedPart {
                    part_number: 2,
                    etag: "e2".to_string(),
                    size: 524_288,
                },
            ]
        );
        first_page.assert_hits_async(1).await;
        second_page.assert_hits_async(1).await;
    }

    /// `resume_multipart` keeps the parts `ListParts` reports with the expected size,
    /// uploads only the missing ones and completes with both ETags in part order.
    #[tokio::test]
    async fn resume_multipart_skips_parts_the_service_already_holds() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let key = "blob.bin";
        let data: Vec<u8> = vec![b'r'; 2 * MIB];

        let list_params = vec![
            ("uploadId".to_string(), "u-resume".to_string()),
            (
                "max-parts".to_string(),
                multipart::MAX_LIST_PARTS.to_string(),
            ),
        ];
        let list_auth = expected_authorization(&store, "GET", Some(key), &list_params, &[]);
        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-resume")
                    .header("authorization", list_auth.as_str());
                then.status(200).body(
                    "<ListPartsResult><Part><PartNumber>1</PartNumber><ETag>\"e1\"</ETag>\
                     <Size>1048576</Size></Part><IsTruncated>false</IsTruncated></ListPartsResult>",
                );
            })
            .await;

        let part1 = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/blob.bin")
                    .query_param("partNumber", "1")
                    .query_param("uploadId", "u-resume");
                then.status(200).header("ETag", r#""fresh""#);
            })
            .await;
        let part2 = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/blob.bin")
                    .query_param("partNumber", "2")
                    .query_param("uploadId", "u-resume");
                then.status(200).header("ETag", r#""e2""#);
            })
            .await;
        let complete = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-resume")
                    .body(
                        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber>\
                         <ETag>\"e1\"</ETag></Part><Part><PartNumber>2</PartNumber>\
                         <ETag>\"e2\"</ETag></Part></CompleteMultipartUpload>",
                    );
                then.status(200);
            })
            .await;

        let mut progress = Vec::new();
        store
            .resume_multipart_with(key, "u-resume", &data, |done, total| {
                progress.push((done, total))
            })
            .await
            .unwrap();

        part1.assert_hits_async(0).await;
        part2.assert_hits_async(1).await;
        complete.assert_hits_async(1).await;
        assert_eq!(progress, vec![(1, 2), (2, 2)]);
    }

    /// A stored part whose size does not match the plan is uploaded again: the data
    /// changed under the same upload id.
    #[tokio::test]
    async fn resume_multipart_reuploads_a_part_with_a_stale_size() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let key = "blob.bin";
        let data: Vec<u8> = vec![b's'; 2 * MIB];

        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-stale");
                then.status(200).body(
                    "<ListPartsResult><Part><PartNumber>1</PartNumber><ETag>\"old\"</ETag>\
                     <Size>7</Size></Part><IsTruncated>false</IsTruncated></ListPartsResult>",
                );
            })
            .await;
        let part1 = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/blob.bin")
                    .query_param("partNumber", "1")
                    .query_param("uploadId", "u-stale");
                then.status(200).header("ETag", r#""new1""#);
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/blob.bin")
                    .query_param("partNumber", "2")
                    .query_param("uploadId", "u-stale");
                then.status(200).header("ETag", r#""new2""#);
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-stale");
                then.status(200);
            })
            .await;

        store.resume_multipart(key, "u-stale", &data).await.unwrap();
        part1.assert_hits_async(1).await;
    }

    /// A failed resume must **not** discard the upload: the parts already stored stay
    /// usable for the next attempt, so no `DELETE ?uploadId` is sent.
    #[tokio::test]
    async fn resume_leaves_the_upload_open_when_a_part_fails() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let key = "blob.bin";
        let data: Vec<u8> = vec![b'f'; 2 * MIB];

        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-open");
                then.status(200)
                    .body("<ListPartsResult><IsTruncated>false</IsTruncated></ListPartsResult>");
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/blob.bin")
                    .query_param("partNumber", "1")
                    .query_param("uploadId", "u-open");
                then.status(200).header("ETag", r#""e1""#);
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path("/blob.bin")
                    .query_param("partNumber", "2")
                    .query_param("uploadId", "u-open");
                then.status(403)
                    .body("<Error><Code>SignatureDoesNotMatch</Code></Error>");
            })
            .await;
        let abort = server
            .mock_async(|when, then| {
                when.method(DELETE)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-open");
                then.status(204);
            })
            .await;
        let complete = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/blob.bin")
                    .query_param("uploadId", "u-open");
                then.status(200);
            })
            .await;

        let err = store.resume_multipart(key, "u-open", &data).await.unwrap_err();
        assert_eq!(err.status(), Some(403));
        abort.assert_hits_async(0).await;
        complete.assert_hits_async(0).await;
    }

    /// Progress is reported once before the first upload and after every accepted
    /// part.
    #[tokio::test]
    async fn put_multipart_reports_progress() {
        let server = MockServer::start_async().await;
        let store = multipart_store(&server, 0, MIB as u64, 1);
        let data: Vec<u8> = vec![b'p'; 2 * MIB];

        server
            .mock_async(|when, then| {
                when.method(POST).path("/k").query_param("uploads", "");
                then.status(200).body(
                    r#"<InitiateMultipartUploadResult><UploadId>u-progress</UploadId></InitiateMultipartUploadResult>"#,
                );
            })
            .await;
        for part_number in 1..=2 {
            server
                .mock_async(move |when, then| {
                    when.method(PUT)
                        .path("/k")
                        .query_param("partNumber", part_number.to_string())
                        .query_param("uploadId", "u-progress");
                    then.status(200)
                        .header("ETag", format!("\"e{part_number}\""));
                })
                .await;
        }
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/k")
                    .query_param("uploadId", "u-progress");
                then.status(200);
            })
            .await;

        let mut progress = Vec::new();
        store
            .put_multipart_with("k", &data, |done, total| progress.push((done, total)))
            .await
            .unwrap();
        assert_eq!(progress, vec![(0, 2), (1, 2), (2, 2)]);
    }

    /// Part size, concurrency and threshold are validated eagerly and exposed.
    #[test]
    fn multipart_settings_are_validated_and_exposed() {
        let builder = || {
            CosHttpObjectStore::builder()
                .bucket(BUCKET)
                .region("ap-beijing")
                .secret_id("id")
                .secret_key("key")
        };

        let store = builder().build().unwrap();
        assert_eq!(
            store.config().multipart_threshold(),
            multipart::DEFAULT_MULTIPART_THRESHOLD
        );
        assert_eq!(
            store.config().multipart_part_size(),
            multipart::DEFAULT_MULTIPART_PART_SIZE
        );
        assert_eq!(
            store.config().multipart_concurrency(),
            multipart::DEFAULT_MULTIPART_CONCURRENCY
        );

        let err = builder()
            .multipart_part_size(multipart::MIN_MULTIPART_PART_SIZE - 1)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("part size must be between"));

        let err = builder()
            .multipart_part_size(multipart::MAX_MULTIPART_PART_SIZE + 1)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("part size must be between"));

        let err = builder().multipart_concurrency(0).build().unwrap_err();
        assert!(err.to_string().contains("concurrency must be at least 1"));

        // A threshold above the 5 GiB single-PUT cap would make `put` send an
        // oversized single PUT (COS refuses it), so it is rejected eagerly.
        let err = builder().multipart_threshold(u64::MAX).build().unwrap_err();
        assert!(err.to_string().contains("single-PUT limit"));
        let err = builder()
            .multipart_threshold(multipart::MAX_SINGLE_PUT_SIZE + 1)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("single-PUT limit"));
        // The cap itself stays accepted: multipart starts right at 5 GiB.
        assert_eq!(
            builder()
                .multipart_threshold(multipart::MAX_SINGLE_PUT_SIZE)
                .build()
                .unwrap()
                .config()
                .multipart_threshold(),
            multipart::MAX_SINGLE_PUT_SIZE
        );

        // Settings survive into the config.
        let store = builder()
            .multipart_threshold(1024)
            .multipart_part_size(2 * 1024 * 1024)
            .multipart_concurrency(4)
            .build()
            .unwrap();
        assert_eq!(store.config().multipart_threshold(), 1024);
        assert_eq!(store.config().multipart_part_size(), 2 * 1024 * 1024);
        assert_eq!(store.config().multipart_concurrency(), 4);
    }

    /// `with_credentials` swaps STS credentials while sharing the connection pool
    /// and the closed flag.
    #[test]
    fn with_credentials_swaps_the_session_token() {
        let store = CosHttpObjectStore::new(BUCKET, "ap-beijing", "id", "key").unwrap();
        let rotated = store
            .with_credentials(CosCredentials::new("id2", "key2").with_security_token("token-2"));

        assert_eq!(rotated.config().credentials().secret_id(), "id2");
        assert_eq!(
            rotated.config().credentials().security_token(),
            Some("token-2")
        );
        assert!(store.config().credentials().security_token().is_none());
        assert_eq!(rotated.config().bucket(), store.config().bucket());
        assert_eq!(rotated.config().base_url(), store.config().base_url());

        store.close();
        assert!(rotated.is_closed());
    }

    // ------------------------------------------------------------------
    // Presigned URLs
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn presign_get_builds_a_complete_signed_url() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        let url = store.presign_get("agentscope/a b.json", None).unwrap();

        let signed = signature::sign_request(
            &CosCredentials::new("AKIDexample", "secret-key"),
            "GET",
            "/agentscope/a b.json",
            &[],
            &[("host".to_string(), store.config().host())],
            KEY_TIME,
        );
        // Path is encoded in the URL, signed raw.
        assert_eq!(
            url,
            format!(
                "{}/agentscope/a%20b.json?{}",
                server.base_url(),
                signed.authorization
            )
        );
        assert!(url.contains("q-sign-algorithm=sha1&q-ak=AKIDexample"));
        assert!(url.contains("q-sign-time=1700000000;1700000900&q-key-time=1700000000;1700000900"));
        assert!(url.contains("&q-header-list=host&q-url-param-list=&q-signature="));
        assert!(!url.contains(' '));
    }

    /// Pins the whole presign chain (fixed clock + fixed credentials) to a literal
    /// signature so an accidental algorithm change cannot slip through.
    ///
    /// `GET /k`, public host
    /// `examplebucket-1250000000.cos.ap-beijing.myqcloud.com`,
    /// `q-sign-time = 1700000000;1700000900`, SecretId `AKIDexample` /
    /// SecretKey `secret-key`. Uses the public domain (not a `httpmock` endpoint) so
    /// the signed `host` is stable across runs.
    #[test]
    fn presign_get_signature_is_pinned() {
        let store = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("AKIDexample")
            .secret_key("secret-key")
            .clock(clock())
            .build()
            .unwrap();
        let url = store.presign_get("k", None).unwrap();
        assert_eq!(
            url,
            format!(
                "https://examplebucket-1250000000.cos.ap-beijing.myqcloud.com/k?\
                 q-sign-algorithm=sha1&q-ak=AKIDexample\
                 &q-sign-time={KEY_TIME}&q-key-time={KEY_TIME}\
                 &q-header-list=host&q-url-param-list=&q-signature=78d1df6ab1f8351625eab6f5f0ea072d382e4dc7"
            )
        );
    }

    #[tokio::test]
    async fn presign_expiry_is_honoured_and_validated() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let default = store.presign_get("k", None).unwrap();
        assert!(default.contains("q-sign-time=1700000000;1700000900"));

        let short = store.presign_get("k", Some(60)).unwrap();
        assert!(short.contains("q-sign-time=1700000000;1700000060"));
        assert!(short.contains("q-key-time=1700000000;1700000060"));
        assert_ne!(
            short, default,
            "the signature must cover the validity window"
        );

        let err = store.presign_get("k", Some(0)).unwrap_err();
        assert!(err.to_string().contains("at least 1 second"));

        let err = store
            .presign_get("k", Some(presign::MAX_PRESIGN_EXPIRE_SECS + 1))
            .unwrap_err();
        assert!(err.to_string().contains("must not exceed"));
    }

    #[tokio::test]
    async fn presign_put_signs_the_put_method() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);

        let put = store.presign_put("k", None).unwrap();
        let get = store.presign_get("k", None).unwrap();
        assert_ne!(put, get);

        let expected = signature::sign_request(
            &CosCredentials::new("AKIDexample", "secret-key"),
            "PUT",
            "/k",
            &[],
            &[("host".to_string(), store.config().host())],
            KEY_TIME,
        );
        assert!(put.ends_with(&expected.signature));

        // Only the signature differs: the query keys and the `host` header list are
        // identical for both methods.
        let query_prefix = |url: &str, sig: &str| url[..url.len() - sig.len()].to_string();
        assert_eq!(
            query_prefix(&put, &expected.signature),
            query_prefix(
                &get,
                &signature::sign_request(
                    &CosCredentials::new("AKIDexample", "secret-key"),
                    "GET",
                    "/k",
                    &[],
                    &[("host".to_string(), store.config().host())],
                    KEY_TIME,
                )
                .signature
            )
        );
    }

    /// STS tokens travel as a signed query parameter, so the receiver needs no
    /// header (token refresh itself is the caller's job).
    #[tokio::test]
    async fn presign_includes_the_session_token_as_a_query_parameter() {
        let server = MockServer::start_async().await;
        let store = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("AKIDexample")
            .secret_key("secret-key")
            .security_token("session-token")
            .endpoint(server.base_url())
            .clock(clock())
            .build()
            .unwrap();

        let url = store.presign_get("k", None).unwrap();
        assert!(url.contains("?x-cos-security-token=session-token&q-sign-algorithm=sha1"));
        assert!(url.contains("&q-header-list=host&q-url-param-list=x-cos-security-token&"));

        let expected = signature::sign_request(
            &CosCredentials::new("AKIDexample", "secret-key").with_security_token("session-token"),
            "GET",
            "/k",
            &[(
                "x-cos-security-token".to_string(),
                "session-token".to_string(),
            )],
            &[("host".to_string(), store.config().host())],
            KEY_TIME,
        );
        assert!(url.ends_with(&expected.signature));
    }

    /// Business parameters (`response-content-*`) are signed, so they appear in
    /// `q-url-param-list` and the value can be passed through verbatim.
    #[tokio::test]
    async fn presign_get_with_params_signs_the_business_parameters() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        let params = [(
            "Response-Content-Disposition",
            "attachment; filename=\"a b.json\"",
        )];

        let url = store
            .presign_get_with_params("a b.json", &params, Some(60))
            .unwrap();

        let signed = signature::sign_request(
            &CosCredentials::new("AKIDexample", "secret-key"),
            "GET",
            "/a b.json",
            // Names are lowercased before signing, matching `canonical_pairs`.
            &[(
                "response-content-disposition".to_string(),
                params[0].1.to_string(),
            )],
            &[("host".to_string(), store.config().host())],
            "1700000000;1700000060",
        );
        assert_eq!(
            url,
            format!(
                "{}/a%20b.json?response-content-disposition=\
                 attachment%3B%20filename%3D%22a%20b.json%22&{}",
                server.base_url(),
                signed.authorization
            )
        );
        assert!(url.contains("&q-url-param-list=response-content-disposition&q-signature="));
        // The plain variant is unaffected (no parameter list, no extra query keys).
        assert!(store
            .presign_get("a b.json", None)
            .unwrap()
            .contains("&q-url-param-list=&"));
    }

    /// Parameter names reserved by the signature itself are rejected instead of
    /// producing a URL that cannot be verified.
    #[test]
    fn presign_rejects_reserved_parameter_names() {
        let store = CosHttpObjectStore::new(BUCKET, "ap-beijing", "id", "key").unwrap();

        let err = store
            .presign_get_with_params("k", &[("q-sign-time", "0;1")], None)
            .unwrap_err();
        assert!(err.to_string().contains("reserved by the signature"));

        let err = store
            .presign_get_with_params("k", &[("  ", "x")], None)
            .unwrap_err();
        assert!(err.to_string().contains("reserved by the signature"));

        // The `PUT` variant signs its parameters the same way.
        let url = store
            .presign_put_with_params("k", &[("response-content-type", "text/plain")], None)
            .unwrap();
        assert!(url.contains("response-content-type=text%2Fplain"));
        assert!(url.contains("&q-url-param-list=response-content-type&q-signature="));
    }

    #[tokio::test]
    async fn presign_is_rejected_after_close() {
        let server = MockServer::start_async().await;
        let store = store_for(&server);
        store.close();

        assert!(matches!(
            store.presign_get("k", None).unwrap_err(),
            CosError::Closed
        ));
        assert!(matches!(
            store.presign_put("k", None).unwrap_err(),
            CosError::Closed
        ));
    }

    // ------------------------------------------------------------------
    // Timeouts and retries (COS-01)
    // ------------------------------------------------------------------

    /// How often [`fail_first_two_attempts`] was consulted; reset per test.
    static RETRY_PROBE: AtomicUsize = AtomicUsize::new(0);

    /// Matches only the first two `GET /retry.txt` attempts, so httpmock answers
    /// `503` twice and the later, lower-priority `200` mock serves the third try.
    fn fail_first_two_attempts(request: &HttpMockRequest) -> bool {
        if request.method != "GET" || request.path != "/retry.txt" {
            return false;
        }
        RETRY_PROBE.fetch_add(1, Ordering::SeqCst) < 2
    }

    fn retrying_store(server: &MockServer, max_retries: u32) -> CosHttpObjectStore {
        CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("AKIDexample")
            .secret_key("secret-key")
            .endpoint(server.base_url())
            .clock(clock())
            .max_retries(max_retries)
            .build()
            .unwrap()
    }

    /// `503` twice, then `200`: three requests in total and a successful read.
    #[tokio::test]
    async fn retries_transient_statuses_and_then_succeeds() {
        RETRY_PROBE.store(0, Ordering::SeqCst);
        let server = MockServer::start_async().await;
        let store = retrying_store(&server, 2);

        let flaky = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/retry.txt")
                    .matches(fail_first_two_attempts);
                then.status(503)
                    .body("<Error><Code>ServiceUnavailable</Code></Error>");
            })
            .await;
        let healthy = server
            .mock_async(|when, then| {
                when.method(GET).path("/retry.txt");
                then.status(200).body("hello");
            })
            .await;

        // Attempt 1 -> 503, attempt 2 -> 503, attempt 3 -> 200.
        assert_eq!(store.get("retry.txt").await.unwrap().unwrap(), b"hello");
        flaky.assert_hits_async(2).await;
        healthy.assert_hits_async(1).await;
    }

    /// The retry budget is bounded: `max_retries(1)` means two requests.
    #[tokio::test]
    async fn retries_stop_after_the_budget_is_spent() {
        let server = MockServer::start_async().await;
        let store = retrying_store(&server, 1);
        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/always-503");
                then.status(503)
                    .body("<Error><Code>ServiceUnavailable</Code><Message>later</Message></Error>");
            })
            .await;

        let err = store.get("always-503").await.unwrap_err();
        assert_eq!(err.status(), Some(503));
        assert!(err.is_retryable());
        mock.assert_hits_async(2).await;
    }

    /// `403` is a caller error: retrying it would only waste requests.
    #[tokio::test]
    async fn permanent_client_errors_are_not_retried() {
        let server = MockServer::start_async().await;
        let store = retrying_store(&server, 3);
        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/denied.txt");
                then.status(403)
                    .body("<Error><Code>AccessDenied</Code></Error>");
            })
            .await;

        let err = store.get("denied.txt").await.unwrap_err();
        assert_eq!(err.status(), Some(403));
        assert!(!err.is_retryable());
        mock.assert_hits_async(1).await;
    }

    /// `POST` creates server-side state, so it is attempted at most once.
    #[tokio::test]
    async fn post_is_never_retried() {
        let server = MockServer::start_async().await;
        let store = retrying_store(&server, 3);
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/batch");
                then.status(503)
                    .body("<Error><Code>ServiceUnavailable</Code></Error>");
            })
            .await;

        let response = store
            .send(Method::POST, Some("batch"), &[], None, &[])
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 503);
        mock.assert_hits_async(1).await;
    }

    /// A stalled response is cut off by the per-read timeout instead of blocking
    /// forever, and surfaces as a retryable backend error.
    #[tokio::test]
    async fn read_timeout_is_a_retryable_backend_error() {
        let server = MockServer::start_async().await;
        let store = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("AKIDexample")
            .secret_key("secret-key")
            .endpoint(server.base_url())
            .clock(clock())
            .read_timeout(Duration::from_millis(150))
            .max_retries(0)
            .build()
            .unwrap();
        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/slow.txt");
                then.status(200)
                    .body("too late")
                    .delay(Duration::from_millis(1500));
            })
            .await;

        let err = store.get("slow.txt").await.unwrap_err();
        assert!(
            matches!(err, CosError::Backend(_)),
            "expected a transport error, got {err:?}"
        );
        assert!(err.is_retryable());
        mock.assert_hits_async(1).await;
    }

    #[test]
    fn builder_validates_timeouts_and_exposes_retry_settings() {
        let err = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("id")
            .secret_key("key")
            .connect_timeout(Duration::ZERO)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("connect timeout"));

        let err = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("id")
            .secret_key("key")
            .read_timeout(Duration::ZERO)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("read timeout"));

        let store = CosHttpObjectStore::builder()
            .bucket(BUCKET)
            .region("ap-beijing")
            .secret_id("id")
            .secret_key("key")
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(10))
            .max_retries(7)
            .build()
            .unwrap();
        assert_eq!(store.config().connect_timeout(), Duration::from_secs(5));
        assert_eq!(store.config().read_timeout(), Duration::from_secs(10));
        assert_eq!(store.config().max_retries(), 7);

        let defaults = CosHttpObjectStore::new(BUCKET, "ap-beijing", "id", "key").unwrap();
        assert_eq!(defaults.config().connect_timeout(), DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(defaults.config().read_timeout(), DEFAULT_READ_TIMEOUT);
        assert_eq!(defaults.config().max_retries(), DEFAULT_MAX_RETRIES);
    }

    #[test]
    fn retry_delay_grows_then_saturates_at_the_cap() {
        // Attempt windows: [base/2, base], [base, 2*base], [2*base, 4*base], ...
        assert!(retry_delay(1) >= DEFAULT_RETRY_BASE_DELAY / 2);
        assert!(retry_delay(1) <= DEFAULT_RETRY_BASE_DELAY);
        let third = retry_delay(3);
        assert!(third >= Duration::from_millis(200), "{third:?}");
        assert!(third <= Duration::from_millis(400), "{third:?}");
        for attempt in 1..=10u32 {
            let delay = retry_delay(attempt);
            assert!(delay <= MAX_RETRY_DELAY, "attempt {attempt}: {delay:?}");
        }
    }

    #[test]
    fn only_transient_statuses_are_retryable() {
        for status in [408u16, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(StatusCode::from_u16(status).unwrap()));
        }
        for status in [400u16, 401, 403, 404, 409, 501] {
            assert!(!is_retryable_status(StatusCode::from_u16(status).unwrap()));
        }
    }
}
