//! HTTP transport abstraction + a real `reqwest` implementation and an
//! in-memory fake used by the offline unit tests.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;

use agentscope_core::{Error, Result};

/// A boxed stream of decoded JSON values coming from a streaming endpoint.
///
/// Ollama emits newline-delimited JSON (NDJSON); DashScope emits Server-Sent
/// Events (`data: {...}`). Both are normalized to a stream of JSON values by
/// [`HttpTransport`] implementations.
pub type JsonStream<'a> = BoxStream<'a, Result<serde_json::Value>>;

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
}

impl HttpRequestSpec {
    /// Creates a new request spec.
    pub fn new(url: impl Into<String>, body: serde_json::Value) -> Self {
        Self { url: url.into(), headers: Vec::new(), body, stream: false }
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

fn map_reqwest(err: reqwest::Error) -> Error {
    Error::Http(err.to_string())
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

/// Production transport backed by `reqwest` (rustls).
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTransport {
    /// Creates a transport with a fresh default `reqwest::Client`.
    pub fn new() -> Self {
        Self { client: reqwest::Client::new() }
    }

    /// Creates a transport around an existing client (e.g. one carrying
    /// timeouts, proxies or a custom TLS configuration).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Returns the underlying client.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post_json(&self, req: HttpRequestSpec) -> Result<serde_json::Value> {
        let builder = apply_headers(
            self.client.post(&req.url).json(&req.body),
            &req.headers,
        );
        let response = builder.send().await.map_err(map_reqwest)?;
        let status = response.status();
        let text = response.text().await.map_err(map_reqwest)?;
        if !status.is_success() {
            return Err(Error::http_status(status.as_u16(), text));
        }
        serde_json::from_str(&text).map_err(Error::from)
    }

    fn post_json_stream<'a>(&'a self, req: HttpRequestSpec) -> JsonStream<'a> {
        Box::pin(async_stream::try_stream! {
            let builder = apply_headers(
                self.client.post(&req.url).json(&req.body),
                &req.headers,
            );
            let response = builder.send().await.map_err(map_reqwest)?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                Err(Error::http_status(status.as_u16(), body))?;
                unreachable!();
            }
            let mut bytes = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(map_reqwest)?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buffer.find('\n') {
                    let line: String = buffer[..pos].trim_end_matches('\r').to_string();
                    buffer.drain(..=pos);
                    if let Some(value) = decode_stream_line(&line) {
                        yield value;
                    }
                }
            }
            if let Some(value) = decode_stream_line(&buffer) {
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
        Self { response: Some(response), ..Default::default() }
    }

    /// Creates a transport that yields the given streaming chunks.
    pub fn with_stream(items: Vec<serde_json::Value>) -> Self {
        Self { stream_items: items, ..Default::default() }
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
