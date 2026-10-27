//! HTTP transport abstraction + a real `reqwest` implementation and an
//! in-memory fake used by the offline unit tests.
//!
//! All providers share one [`TransportConfig`] so timeouts, keepalive and retry
//! policy cannot drift between them, and every HTTP failure is normalised into a
//! [`ModelError`](crate::common::error::ModelError) before it reaches a caller.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;

use agentscope_core::{Error, Result};

use super::error::ModelError;

/// A boxed stream of decoded JSON values coming from a streaming endpoint.
///
/// Ollama emits newline-delimited JSON (NDJSON); DashScope emits Server-Sent
/// Events (`data: {...}`). Both are normalized to a stream of JSON values by
/// [`HttpTransport`] implementations.
pub type JsonStream<'a> = BoxStream<'a, Result<serde_json::Value>>;

/// Transport tuning shared by every provider.
///
/// `read_timeout` is the idle timeout between bytes: it turns a stalled stream
/// into a retryable error instead of hanging the agent forever.
#[derive(Debug, Clone, PartialEq)]
pub struct TransportConfig {
    /// Timeout for establishing a connection.
    pub connect_timeout: Duration,
    /// Maximum idle time between two reads on a response (incl. streams).
    pub read_timeout: Duration,
    /// TCP keepalive probe interval.
    pub tcp_keepalive: Duration,
    /// Number of retries **in addition to** the first attempt for non-streaming
    /// requests (retryable failures only).
    pub max_retries: u32,
    /// Backoff before the first retry.
    pub initial_backoff: Duration,
    /// Upper bound on backoff.
    pub max_backoff: Duration,
    /// Multiplier applied per retry.
    pub backoff_multiplier: f64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(120),
            tcp_keepalive: Duration::from_secs(30),
            max_retries: 2,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
            backoff_multiplier: 2.0,
        }
    }
}

impl TransportConfig {
    /// Backoff for a 0-based retry attempt, capped at `max_backoff`.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = self.backoff_multiplier.max(1.0).powi(attempt as i32);
        let millis = self.initial_backoff.as_millis() as f64 * factor;
        let capped = millis.min(self.max_backoff.as_millis() as f64);
        Duration::from_millis(capped.max(0.0) as u64)
    }
}

/// A transport-agnostic HTTP request description.
#[derive(Debug, Clone)]
pub struct HttpRequestSpec {
    /// Absolute request URL.
    pub url: String,
    /// Extra request headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// JSON request body.
    pub body: serde_json::Value,
    /// Whether the caller expects a streaming response.
    pub stream: bool,
    /// Provider id used to tag structured errors (e.g. `openai`).
    pub provider: String,
}

impl HttpRequestSpec {
    /// Creates a new request spec.
    pub fn new(url: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            body,
            stream: false,
            provider: "http".to_string(),
        }
    }

    /// Adds a header to the request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Marks the request as streaming.
    pub fn streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Tags the request with the provider id used for structured errors.
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }
}

/// The HTTP boundary used by all provider adapters.
///
/// Implementations must be `Send + Sync` so models can be shared behind an
/// `Arc`. Streaming methods borrow `self`, matching [`agentscope_core::model::Model::stream`].
#[async_trait::async_trait]
pub trait HttpTransport: Send + Sync {
    /// Performs a single JSON request/response round-trip.
    async fn post_json(&self, req: HttpRequestSpec) -> Result<serde_json::Value>;

    /// Performs a streaming request, yielding decoded JSON values as they arrive.
    fn post_json_stream<'a>(&'a self, req: HttpRequestSpec) -> JsonStream<'a>;
}

fn map_reqwest(provider: &str, err: reqwest::Error) -> ModelError {
    let kind = if err.is_timeout() {
        crate::common::error::ErrorKind::Timeout
    } else {
        crate::common::error::ErrorKind::Network
    };
    let mut model_error = ModelError::new(provider, 0, err.to_string());
    model_error.kind = kind;
    model_error
}

fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
}

/// Decodes a single stream line into a JSON value.
///
/// Handles both NDJSON (`{...}` per line) and SSE (`data: {...}` frames),
/// skipping comments, blank lines and the SSE terminator `[DONE]`.
pub fn decode_stream_line(line: &str) -> Option<serde_json::Value> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Detects an in-band `error` frame inside a healthy (HTTP 200) stream.
///
/// Several gateways report failures as `data: {"error": {...}}` after the status
/// line has already been sent. Returns `Some(ModelError)` when the value carries
/// a non-null `error`, `None` otherwise.
fn stream_error_frame(provider: &str, value: &serde_json::Value) -> Option<ModelError> {
    let error = value.get("error")?;
    if error.is_null() {
        return None;
    }
    Some(ModelError::parse_stream_value(provider, value))
}

/// Production transport backed by `reqwest` (rustls).
pub struct ReqwestTransport {
    client: reqwest::Client,
    config: TransportConfig,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTransport {
    /// Creates a transport with the default [`TransportConfig`].
    pub fn new() -> Self {
        Self::with_config(TransportConfig::default())
    }

    /// Creates a transport with an explicit config (timeouts + retry policy).
    pub fn with_config(config: TransportConfig) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .tcp_keepalive(config.tcp_keepalive)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client, config }
    }

    /// Creates a transport around an existing client (e.g. one carrying
    /// timeouts, proxies or a custom TLS configuration).
    ///
    /// The supplied client's own timeout settings win; the retry policy still
    /// comes from the default config.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            config: TransportConfig::default(),
        }
    }

    /// Replaces the retry/timeout policy of an existing transport.
    pub fn with_transport_config(mut self, config: TransportConfig) -> Self {
        self.config = config;
        self
    }

    /// Returns the underlying client.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Returns the active transport config.
    pub fn config(&self) -> &TransportConfig {
        &self.config
    }

    /// One attempt of a non-streaming call, normalised to [`ModelError`].
    async fn attempt_post_json(
        &self,
        req: &HttpRequestSpec,
    ) -> std::result::Result<serde_json::Value, ModelError> {
        let builder = apply_headers(self.client.post(&req.url).json(&req.body), &req.headers);
        let response = builder
            .send()
            .await
            .map_err(|e| map_reqwest(&req.provider, e))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| map_reqwest(&req.provider, e))?;
        if !status.is_success() {
            return Err(ModelError::parse(&req.provider, status.as_u16(), &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| ModelError::new(&req.provider, 200, format!("invalid JSON response: {e}")))
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post_json(&self, req: HttpRequestSpec) -> Result<serde_json::Value> {
        let mut attempt = 0u32;
        loop {
            match self.attempt_post_json(&req).await {
                Ok(value) => return Ok(value),
                Err(err) => {
                    if err.retryable() && attempt < self.config.max_retries {
                        let backoff = self.config.backoff(attempt);
                        tracing::warn!(
                            provider = %req.provider,
                            attempt = attempt + 1,
                            max = self.config.max_retries,
                            backoff_ms = backoff.as_millis() as u64,
                            "retrying after {}",
                            err.message
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(err.to_core());
                }
            }
        }
    }

    fn post_json_stream<'a>(&'a self, req: HttpRequestSpec) -> JsonStream<'a> {
        Box::pin(async_stream::try_stream! {
            let builder = apply_headers(self.client.post(&req.url).json(&req.body), &req.headers);
            let provider = req.provider.clone();
            let response = builder
                .send()
                .await
                .map_err(|e| map_reqwest(&provider, e))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                Err(ModelError::parse(&provider, status.as_u16(), &body))?;
                unreachable!();
            }
            let mut bytes = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|e| map_reqwest(&provider, e))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buffer.find('\n') {
                    let line: String = buffer[..pos].trim_end_matches('\r').to_string();
                    buffer.drain(..=pos);
                    if let Some(value) = decode_stream_line(&line) {
                        if let Some(err) = stream_error_frame(&provider, &value) {
                            Err(err)?;
                        }
                        yield value;
                    }
                }
            }
            if let Some(value) = decode_stream_line(&buffer) {
                if let Some(err) = stream_error_frame(&provider, &value) {
                    Err(err)?;
                }
                yield value;
            }
        })
    }
}

/// An in-memory transport used by the offline unit tests.
///
/// It never performs I/O: the caller pre-loads a single non-streaming response
/// and/or a list of streaming chunks.
#[derive(Debug, Clone, Default)]
pub struct MemoryTransport {
    /// Response returned by [`HttpTransport::post_json`].
    pub response: Option<serde_json::Value>,
    /// Chunks yielded by [`HttpTransport::post_json_stream`].
    pub stream_items: Vec<serde_json::Value>,
    /// Records the last request spec received (for assertions).
    pub captured: Arc<std::sync::Mutex<Option<HttpRequestSpec>>>,
}

impl MemoryTransport {
    /// Creates a transport that yields the given non-streaming response.
    pub fn with_response(response: serde_json::Value) -> Self {
        Self {
            response: Some(response),
            ..Default::default()
        }
    }

    /// Creates a transport that yields the given streaming chunks.
    pub fn with_stream(items: Vec<serde_json::Value>) -> Self {
        Self {
            stream_items: items,
            ..Default::default()
        }
    }

    /// Returns the last captured request spec, if any.
    pub fn last_request(&self) -> Option<HttpRequestSpec> {
        self.captured.lock().ok().and_then(|g| g.clone())
    }

    fn record(&self, spec: &HttpRequestSpec) {
        if let Ok(mut guard) = self.captured.lock() {
            *guard = Some(spec.clone());
        }
    }
}

#[async_trait::async_trait]
impl HttpTransport for MemoryTransport {
    async fn post_json(&self, req: HttpRequestSpec) -> Result<serde_json::Value> {
        self.record(&req);
        self.response
            .clone()
            .ok_or_else(|| Error::Model("MemoryTransport: no response configured".into()))
    }

    fn post_json_stream<'a>(&'a self, req: HttpRequestSpec) -> JsonStream<'a> {
        self.record(&req);
        let items: Vec<Result<serde_json::Value>> =
            self.stream_items.iter().cloned().map(Ok).collect();
        Box::pin(futures::stream::iter(items))
    }
}

/// A [`HttpTransport`] reference used by every provider.
pub type SharedTransport = Arc<dyn HttpTransport>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_ndjson_and_sse_lines() {
        assert_eq!(
            decode_stream_line("{\"a\":1}"),
            Some(serde_json::json!({"a": 1}))
        );
        assert_eq!(
            decode_stream_line("data: {\"a\":2}"),
            Some(serde_json::json!({"a": 2}))
        );
        assert_eq!(decode_stream_line("data: [DONE]"), None);
        assert_eq!(decode_stream_line(": keep-alive"), None);
        assert_eq!(decode_stream_line(""), None);
        assert_eq!(decode_stream_line("event: message"), None);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let cfg = TransportConfig {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(2),
            backoff_multiplier: 2.0,
            ..TransportConfig::default()
        };
        assert_eq!(cfg.backoff(0), Duration::from_millis(500));
        assert_eq!(cfg.backoff(1), Duration::from_millis(1000));
        assert_eq!(cfg.backoff(2), Duration::from_millis(2000));
        assert_eq!(cfg.backoff(9), Duration::from_secs(2));
    }

    #[test]
    fn detects_in_band_error_frames() {
        let ok = serde_json::json!({"choices": []});
        assert!(stream_error_frame("openai", &ok).is_none());
        let null_error = serde_json::json!({"error": null});
        assert!(stream_error_frame("openai", &null_error).is_none());
        let err = stream_error_frame(
            "openai",
            &serde_json::json!({"error": {"message": "boom", "type": "server_error"}}),
        )
        .expect("error frame");
        assert_eq!(err.message, "boom");
        assert!(err.retryable());
    }

    #[tokio::test]
    async fn memory_transport_round_trip() {
        let transport = MemoryTransport::with_response(serde_json::json!({"ok": true}));
        let spec = HttpRequestSpec::new("http://x/y", serde_json::json!({"m": 1}));
        let value = transport.post_json(spec).await.unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
        assert_eq!(transport.last_request().unwrap().url, "http://x/y");
    }

    #[tokio::test]
    async fn memory_transport_streams_in_order() {
        let transport = MemoryTransport::with_stream(vec![
            serde_json::json!({ "i": 1 }),
            serde_json::json!({ "i": 2 }),
        ]);
        let spec = HttpRequestSpec::new("http://x/y", serde_json::json!({})).streaming(true);
        let mut stream = transport.post_json_stream(spec);
        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        assert_eq!(first, serde_json::json!({ "i": 1 }));
        assert_eq!(second, serde_json::json!({ "i": 2 }));
        assert!(stream.next().await.is_none());
    }
}
