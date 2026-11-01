//! OTLP/HTTP trace export — the "degraded exporter" path documented in the tracker §4.2.
//!
//! Java delegates span export to the OpenTelemetry SDK (`OtelTracingMiddleware` +
//! `TracerRegistry`); that SDK is not a dependency of this crate, so the exporter contract is
//! modelled here instead:
//!
//! - [`Span`] — a finished span as plain data (hex ids, nanosecond timestamps, attributes).
//! - [`OtlpEncoder`] — encodes spans into the OTLP/HTTP JSON payload
//!   (`opentelemetry.proto.collector.trace.v1.ExportTraceServiceRequest`).
//! - [`TraceExporter`] — the export seam, with a real `reqwest` implementation
//!   ([`OtlpHttpExporter`]) plus [`RecordingTraceExporter`] / [`NoopTraceExporter`] doubles.
//!
//! Because spans are plain data, encoding is fully offline-testable. This is why the module
//! does not pull in `opentelemetry`/`opentelemetry-otlp`.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::error::{StudioError, StudioResult};
use crate::studio::config::StudioConfig;
use crate::telemetry::extractors::{Attribute, Attributes};

/// Span kind, mapped onto the OTLP numeric enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpanKind {
    /// Internal operation (OTLP `1`).
    #[default]
    Internal,
    /// Server side of a remote call (OTLP `2`).
    Server,
    /// Client side of a remote call (OTLP `3`).
    Client,
    /// Message producer (OTLP `4`).
    Producer,
    /// Message consumer (OTLP `5`).
    Consumer,
}

impl SpanKind {
    /// The OTLP `kind` field value.
    pub fn as_otlp(self) -> u8 {
        match self {
            SpanKind::Internal => 1,
            SpanKind::Server => 2,
            SpanKind::Client => 3,
            SpanKind::Producer => 4,
            SpanKind::Consumer => 5,
        }
    }
}

/// Span status, mapped onto the OTLP status code (`0` unset, `1` ok, `2` error).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SpanStatus {
    /// The default: no explicit status was recorded.
    #[default]
    Unset,
    /// The operation succeeded.
    Ok,
    /// The operation failed, with the error message.
    Error(String),
}

impl SpanStatus {
    fn encode(&self) -> Value {
        match self {
            SpanStatus::Unset => json!({ "code": 0 }),
            SpanStatus::Ok => json!({ "code": 1 }),
            SpanStatus::Error(message) => json!({ "code": 2, "message": message }),
        }
    }
}

/// A finished span ready for export.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// 16-byte trace id (32 hex characters).
    pub trace_id: String,
    /// 8-byte span id (16 hex characters).
    pub span_id: String,
    /// Parent span id when this span is nested.
    pub parent_span_id: Option<String>,
    /// Operation name.
    pub name: String,
    /// Span kind.
    pub kind: SpanKind,
    /// Start time, nanoseconds since the Unix epoch.
    pub start_time_unix_nano: u64,
    /// End time, nanoseconds since the Unix epoch.
    pub end_time_unix_nano: u64,
    /// Semantic-convention attributes (produced by [`super::extractors`]).
    pub attributes: Attributes,
    /// Span status.
    pub status: SpanStatus,
}

impl Span {
    /// Creates a span with default kind/status and a zeroed time range.
    pub fn new(
        name: impl Into<String>,
        trace_id: impl Into<String>,
        span_id: impl Into<String>,
    ) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
            name: name.into(),
            kind: SpanKind::default(),
            start_time_unix_nano: 0,
            end_time_unix_nano: 0,
            attributes: Vec::new(),
            status: SpanStatus::default(),
        }
    }

    /// Sets the parent span id.
    pub fn with_parent(mut self, parent_span_id: impl Into<String>) -> Self {
        self.parent_span_id = Some(parent_span_id.into());
        self
    }

    /// Sets the span kind.
    pub fn with_kind(mut self, kind: SpanKind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets the span status.
    pub fn with_status(mut self, status: SpanStatus) -> Self {
        self.status = status;
        self
    }

    /// Sets the time range from wall-clock timestamps.
    pub fn with_time_range(mut self, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        self.start_time_unix_nano = unix_nanos(start);
        self.end_time_unix_nano = unix_nanos(end);
        self
    }

    /// Appends one attribute.
    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.push((key.into(), value));
        self
    }

    /// Validates the identifiers and time range against the OTLP constraints.
    pub fn validate(&self) -> Result<(), String> {
        if !is_hex_id(&self.trace_id, 32) {
            return Err(format!(
                "invalid traceId '{}' (expected 32 hex characters)",
                self.trace_id
            ));
        }
        if !is_hex_id(&self.span_id, 16) {
            return Err(format!(
                "invalid spanId '{}' (expected 16 hex characters)",
                self.span_id
            ));
        }
        if let Some(parent) = &self.parent_span_id {
            if !is_hex_id(parent, 16) {
                return Err(format!(
                    "invalid parentSpanId '{parent}' (expected 16 hex characters)"
                ));
            }
        }
        if self.end_time_unix_nano < self.start_time_unix_nano {
            return Err("endTimeUnixNano precedes startTimeUnixNano".to_string());
        }
        Ok(())
    }
}

/// Whether `id` is exactly `len` hexadecimal characters (an empty id is never valid).
pub fn is_hex_id(id: &str, len: usize) -> bool {
    !id.is_empty() && id.len() == len && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// Nanoseconds since the Unix epoch (0 for pre-epoch timestamps).
pub fn unix_nanos(ts: DateTime<Utc>) -> u64 {
    ts.timestamp_nanos_opt()
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
}

/// Encodes one attribute value into the OTLP `AnyValue` JSON shape.
pub fn encode_attribute_value(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({ "stringValue": text }),
        Value::Bool(flag) => json!({ "boolValue": flag }),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            json!({ "intValue": number.to_string() })
        }
        Value::Number(number) => json!({ "doubleValue": number.as_f64().unwrap_or(0.0) }),
        Value::Null => json!({ "stringValue": "" }),
        // OTLP `AnyValue` supports arrays (`arrayValue`) and maps (`kvlistValue`); only
        // `null` — which OTLP has no type for — degrades to an empty string.
        Value::Array(items) => {
            json!({ "arrayValue": { "values": items.iter().map(encode_attribute_value).collect::<Vec<_>>() } })
        }
        Value::Object(map) => json!({
            "kvlistValue": {
                "values": map
                    .iter()
                    .map(|(key, item)| json!({ "key": key, "value": encode_attribute_value(item) }))
                    .collect::<Vec<_>>()
            }
        }),
    }
}

/// Encodes attributes into the OTLP key/value list.
pub fn encode_attributes(attributes: &[Attribute]) -> Value {
    Value::Array(
        attributes
            .iter()
            .map(|(key, value)| json!({ "key": key, "value": encode_attribute_value(value) }))
            .collect(),
    )
}

/// OTLP/JSON encoder for `ExportTraceServiceRequest`.
#[derive(Debug, Clone)]
pub struct OtlpEncoder {
    scope_name: String,
    scope_version: String,
}

impl Default for OtlpEncoder {
    fn default() -> Self {
        Self {
            scope_name: "agentscope".to_string(),
            scope_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl OtlpEncoder {
    /// Creates an encoder with an explicit instrumentation scope.
    pub fn new(scope_name: impl Into<String>, scope_version: impl Into<String>) -> Self {
        Self {
            scope_name: scope_name.into(),
            scope_version: scope_version.into(),
        }
    }

    /// Encodes `spans` under a single resource.
    ///
    /// Returns an error for an invalid span rather than silently dropping it.
    pub fn encode(&self, resource: &[Attribute], spans: &[Span]) -> StudioResult<Value> {
        let mut encoded = Vec::with_capacity(spans.len());
        for span in spans {
            span.validate().map_err(|err| {
                StudioError::Serialization(format!("cannot export span '{}': {err}", span.name))
            })?;
            encoded.push(json!({
                "traceId": span.trace_id,
                "spanId": span.span_id,
                "parentSpanId": span.parent_span_id.clone().unwrap_or_default(),
                "name": span.name,
                "kind": span.kind.as_otlp(),
                "startTimeUnixNano": span.start_time_unix_nano.to_string(),
                "endTimeUnixNano": span.end_time_unix_nano.to_string(),
                "attributes": encode_attributes(&span.attributes),
                "status": span.status.encode(),
            }));
        }
        Ok(json!({
            "resourceSpans": [{
                "resource": { "attributes": encode_attributes(resource) },
                "scopeSpans": [{
                    "scope": { "name": self.scope_name, "version": self.scope_version },
                    "spans": encoded,
                }],
            }]
        }))
    }
}

/// Default resource attributes for a Studio run.
///
/// Java lets the OpenTelemetry SDK build the resource (env-configured), so there is no Java
/// key list to mirror; these keys follow the OTel semantic conventions for the values the
/// Studio config already carries.
pub fn resource_attributes(project: &str, run_id: &str) -> Attributes {
    vec![
        ("service.name".to_string(), json!("agentscope")),
        ("agentscope.project".to_string(), json!(project)),
        ("agentscope.run_id".to_string(), json!(run_id)),
    ]
}

/// Exports finished spans to a backend.
#[async_trait]
pub trait TraceExporter: Send + Sync {
    /// Exports `spans`; an empty slice is a no-op for every implementation.
    async fn export(&self, spans: &[Span]) -> StudioResult<()>;
}

/// Discards spans — the default when Studio tracing is disabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTraceExporter;

#[async_trait]
impl TraceExporter for NoopTraceExporter {
    async fn export(&self, _spans: &[Span]) -> StudioResult<()> {
        Ok(())
    }
}

/// Records exported spans in memory (tests, and hosts that ship spans elsewhere).
#[derive(Debug, Default)]
pub struct RecordingTraceExporter {
    exported: Mutex<Vec<Span>>,
}

impl RecordingTraceExporter {
    /// Creates an empty recording exporter.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of every span exported so far, in order.
    pub fn spans(&self) -> Vec<Span> {
        self.exported.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Number of recorded spans.
    pub fn len(&self) -> usize {
        self.exported.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Whether no span has been exported yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TraceExporter for RecordingTraceExporter {
    async fn export(&self, spans: &[Span]) -> StudioResult<()> {
        let mut exported = self
            .exported
            .lock()
            .map_err(|_| StudioError::WebSocket("recording exporter lock poisoned".to_string()))?;
        exported.extend_from_slice(spans);
        Ok(())
    }
}

/// OTLP/HTTP JSON exporter: `POST {tracingUrl}` with the encoded payload.
#[derive(Clone)]
pub struct OtlpHttpExporter {
    http: reqwest::Client,
    endpoint: String,
    encoder: OtlpEncoder,
    resource: Attributes,
    headers: Vec<(String, String)>,
}

impl std::fmt::Debug for OtlpHttpExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `reqwest::Client` holds connection-pool state; print the meaningful fields only.
        // Header values are credentials (e.g. Langfuse basic auth), so only the count is shown.
        f.debug_struct("OtlpHttpExporter")
            .field("endpoint", &self.endpoint)
            .field("resource", &self.resource)
            .field("header_count", &self.headers.len())
            .finish_non_exhaustive()
    }
}

impl OtlpHttpExporter {
    /// Creates an exporter posting to `endpoint` with the given resource attributes.
    pub fn new(endpoint: impl Into<String>, resource: Attributes) -> Self {
        agentscope_core::tls::ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            endpoint: endpoint.into(),
            encoder: OtlpEncoder::default(),
            resource,
            headers: Vec::new(),
        }
    }

    /// Creates an exporter targeting `config.tracing_url()` for the configured run.
    pub fn from_config(config: &StudioConfig) -> Self {
        Self::new(
            config.tracing_url(),
            resource_attributes(config.project(), config.run_id()),
        )
    }

    /// Overrides the instrumentation scope.
    pub fn with_encoder(mut self, encoder: OtlpEncoder) -> Self {
        self.encoder = encoder;
        self
    }

    /// The export endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Adds a header sent with every OTLP request, replacing any header of the same name.
    ///
    /// Mirrors Java's `TelemetryTracer.Builder.addHeader` (`TelemetryTracer.java:340`), which
    /// is a `HashMap` put; the javadoc names authentication (e.g. Langfuse) as the use case
    /// (tracker §6.4 P2-2).
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        self.headers.retain(|(existing, _)| existing != &key);
        self.headers.push((key, value.into()));
        self
    }

    /// Replaces the whole header set, mirroring Java's `TelemetryTracer.Builder.headers`.
    pub fn with_headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.headers = headers
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        self
    }

    /// The configured headers, in insertion order (Java's `HashMap` carries no order).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The encoded payload that [`TraceExporter::export`] would POST.
    pub fn encode(&self, spans: &[Span]) -> StudioResult<Value> {
        self.encoder.encode(&self.resource, spans)
    }
}

#[async_trait]
impl TraceExporter for OtlpHttpExporter {
    async fn export(&self, spans: &[Span]) -> StudioResult<()> {
        if spans.is_empty() {
            return Ok(());
        }
        let payload = self.encoder.encode(&self.resource, spans)?;
        let mut request = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload);
        // Custom headers come last so they may override the defaults (Java's exporter builder
        // adds them after setting the content type).
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|err| StudioError::Transport(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(StudioError::Http {
                status: status.as_u16(),
                message,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &str) -> Span {
        Span::new(name, "4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7").with_time_range(
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            DateTime::<Utc>::from_timestamp(1_700_000_001, 500_000_000).unwrap(),
        )
    }

    #[test]
    fn hex_ids_are_validated_by_length_and_charset() {
        assert!(is_hex_id("00f067aa0ba902b7", 16));
        assert!(!is_hex_id("00f067aa0ba902b", 16));
        assert!(!is_hex_id("00f067aa0ba902bz", 16));
        assert!(!is_hex_id("", 0));
    }

    #[test]
    fn validate_rejects_bad_identifiers_and_time_ranges() {
        assert!(span("ok").validate().is_ok());
        let bad_trace = Span::new("x", "deadbeef", "00f067aa0ba902b7");
        assert!(bad_trace
            .validate()
            .unwrap_err()
            .contains("invalid traceId"));
        let bad_parent = span("x").with_parent("zz");
        assert!(bad_parent
            .validate()
            .unwrap_err()
            .contains("invalid parentSpanId"));
        let mut reversed = span("x");
        reversed.end_time_unix_nano = 0;
        reversed.start_time_unix_nano = 5;
        assert!(reversed.validate().unwrap_err().contains("precedes"));
    }

    #[test]
    fn unix_nanos_is_non_negative() {
        let ts = DateTime::<Utc>::from_timestamp(1_700_000_000, 123).unwrap();
        assert_eq!(unix_nanos(ts), 1_700_000_000_000_000_123);
        let pre_epoch = DateTime::<Utc>::from_timestamp(-5, 0).unwrap();
        assert_eq!(unix_nanos(pre_epoch), 0);
    }

    #[test]
    fn attribute_values_map_to_the_otlp_any_value_shape() {
        assert_eq!(
            encode_attribute_value(&json!("text")),
            json!({"stringValue": "text"})
        );
        assert_eq!(
            encode_attribute_value(&json!(true)),
            json!({"boolValue": true})
        );
        assert_eq!(encode_attribute_value(&json!(7)), json!({"intValue": "7"}));
        assert_eq!(
            encode_attribute_value(&json!(1.5)),
            json!({"doubleValue": 1.5})
        );
        assert_eq!(
            encode_attribute_value(&Value::Null),
            json!({"stringValue": ""})
        );
        assert_eq!(
            encode_attribute_value(&json!([1, "a"])),
            json!({"arrayValue": {"values": [{"intValue": "1"}, {"stringValue": "a"}]}})
        );
        assert_eq!(
            encode_attribute_value(&json!({"a": 1})),
            json!({"kvlistValue": {"values": [{"key": "a", "value": {"intValue": "1"}}]}})
        );
    }

    #[test]
    fn encode_produces_an_otlp_export_request() {
        let encoder = OtlpEncoder::new("agentscope", "0.1.0");
        let resource = resource_attributes("proj", "run-1");
        let payload = encoder
            .encode(
                &resource,
                &[span("chat")
                    .with_kind(SpanKind::Client)
                    .with_status(SpanStatus::Ok)],
            )
            .unwrap();

        let scope = &payload["resourceSpans"][0]["scopeSpans"][0];
        assert_eq!(scope["scope"]["name"], json!("agentscope"));
        assert_eq!(scope["scope"]["version"], json!("0.1.0"));
        let encoded = &scope["spans"][0];
        assert_eq!(
            encoded["traceId"],
            json!("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        assert_eq!(encoded["spanId"], json!("00f067aa0ba902b7"));
        assert_eq!(encoded["parentSpanId"], json!(""));
        assert_eq!(encoded["name"], json!("chat"));
        assert_eq!(encoded["kind"], json!(3));
        assert_eq!(encoded["startTimeUnixNano"], json!("1700000000000000000"));
        assert_eq!(encoded["endTimeUnixNano"], json!("1700000001500000000"));
        assert_eq!(encoded["status"], json!({"code": 1}));
        let resource_keys: Vec<&str> = payload["resourceSpans"][0]["resource"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            resource_keys,
            vec!["service.name", "agentscope.project", "agentscope.run_id"]
        );
    }

    #[test]
    fn encode_reports_error_spans_and_never_silently_drops_them() {
        let encoder = OtlpEncoder::default();
        let err = encoder
            .encode(&[], &[span("bad").with_parent("nope")])
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot export span 'bad'"),
            "{err}"
        );
    }

    #[test]
    fn encode_attributes_keeps_order_and_keys() {
        let attributes = vec![
            ("gen_ai.system".to_string(), json!("openai")),
            ("gen_ai.usage.input_tokens".to_string(), json!(12)),
        ];
        assert_eq!(
            encode_attributes(&attributes),
            json!([
                {"key": "gen_ai.system", "value": {"stringValue": "openai"}},
                {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "12"}},
            ])
        );
    }

    #[tokio::test]
    async fn recording_exporter_accumulates_every_export() {
        let exporter = RecordingTraceExporter::new();
        assert!(exporter.is_empty());
        exporter.export(&[span("a")]).await.unwrap();
        exporter.export(&[span("b"), span("c")]).await.unwrap();
        assert_eq!(exporter.len(), 3);
        let names: Vec<String> = exporter.spans().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn noop_exporter_accepts_everything() {
        NoopTraceExporter.export(&[span("a")]).await.unwrap();
    }

    #[test]
    fn http_exporter_targets_the_configured_tracing_url() {
        let config = StudioConfig::builder()
            .studio_url("http://studio:3000")
            .project("proj")
            .run_id("run-9")
            .build();
        let exporter = OtlpHttpExporter::from_config(&config);
        assert_eq!(exporter.endpoint(), "http://studio:3000/v1/traces");
        let payload = exporter.encode(&[span("chat")]).unwrap();
        let keys: Vec<String> = payload["resourceSpans"][0]["resource"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                a["value"]["stringValue"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(keys, vec!["agentscope", "proj", "run-9"]);
    }

    /// Tracker §6.4 P2-2: custom OTLP headers (Java `TelemetryTracer.Builder.addHeader`).
    #[test]
    fn otlp_headers_replace_same_named_entries_like_java_add_header() {
        let exporter = OtlpHttpExporter::new("http://studio:3000/v1/traces", Vec::new())
            .with_header("Authorization", "Bearer first")
            .with_header("X-Langfuse-Session", "run-9")
            .with_header("Authorization", "Bearer second");

        assert_eq!(
            exporter.headers().to_vec(),
            vec![
                ("X-Langfuse-Session".to_string(), "run-9".to_string()),
                ("Authorization".to_string(), "Bearer second".to_string()),
            ]
        );
    }

    #[test]
    fn with_headers_replaces_the_whole_header_set_like_java_headers() {
        let exporter = OtlpHttpExporter::new("http://studio:3000/v1/traces", Vec::new())
            .with_header("Authorization", "Bearer old")
            .with_headers([("Authorization", "Basic abc"), ("X-Scope", "traces")]);

        assert_eq!(
            exporter.headers().to_vec(),
            vec![
                ("Authorization".to_string(), "Basic abc".to_string()),
                ("X-Scope".to_string(), "traces".to_string()),
            ]
        );
    }

    #[test]
    fn an_exporter_from_config_sends_no_extra_headers() {
        let config = StudioConfig::builder()
            .studio_url("http://studio:3000")
            .run_id("run-9")
            .build();
        assert!(OtlpHttpExporter::from_config(&config).headers().is_empty());
        // The debug rendering must not leak header values (credentials).
        let debug = format!(
            "{:?}",
            OtlpHttpExporter::from_config(&config).with_header("Authorization", "Bearer secret")
        );
        assert!(!debug.contains("Bearer secret"), "{debug}");
    }
}
