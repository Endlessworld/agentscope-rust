//! Qdrant vector store.
//!
//! Ports `io.agentscope.core.rag.store.QdrantStore`. The Java class talks gRPC through the
//! `io.qdrant:client` SDK; the Rust port talks the Qdrant **REST** API through the shared
//! [`crate::transport`] seam, so the whole store is exercisable offline with a mock
//! transport.
//!
//! Mapping notes (Java → Rust):
//!
//! * `collectionExistsAsync` → `GET /collections/{name}` (`200` = exists, `404` = create).
//! * `createCollectionAsync(VectorParams)` → `PUT /collections/{name}` with
//!   `{"vectors": {"size": N, "distance": "Cosine"}}` (Java hardcodes `Cosine`; the Rust
//!   config exposes the metric, defaulting to `Cosine`).
//! * `upsertAsync` → `PUT /collections/{name}/points?wait=true` (the `wait` flag is the REST
//!   equivalent of Java's blocking `future.get()`).
//! * `queryAsync(QueryPoints)` → `POST /collections/{name}/points/query`; `using` carries
//!   `vector_name`, `score_threshold` is applied server-side exactly like Java.
//! * `deleteAsync` → `POST /collections/{name}/points/delete?wait=true`, returning
//!   `true` only when `result.status == "completed"` (Java's `UpdateStatus.Completed`).
//! * Payload keys are identical (`content`, `doc_id`, `chunk_id`, `payload`), so data written
//!   by either implementation can be read by the other.
//!
//! Deliberate differences:
//!
//! * Java wraps every failure in `VectorStoreException("Failed to add documents to Qdrant")`
//!   with the cause attached. Rust surfaces the status-bearing [`RagError::Api`] /
//!   [`RagError::Auth`] directly (only `ensure_collection` wraps its create step), so callers
//!   can branch on `status_code()`.
//! * `:memory:` / `file://` locations are gRPC-only in Qdrant; the REST port rejects them.
//! * Lazy collection creation is explicit: [`QdrantStore::connect_with`] (or
//!   [`QdrantStore::ensure_collection`]) performs Java's constructor-time check;
//!   [`QdrantStore::new`] performs no I/O.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, TextBlock};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::error::{RagError, Result};
use crate::model::{Document, DocumentMetadata};
use crate::store::{SearchQuery, VdbStore};
use crate::transport::{
    map_http_error, HttpMethod, HttpRequest, HttpResponse, HttpTransport, ReqwestTransport,
    TransportOptions,
};

/// Default REST port used when a bare host is configured.
///
/// Java defaults to gRPC port `6334`; the REST listener is `6333`.
pub const DEFAULT_HTTP_PORT: u16 = 6333;

/// Qdrant's gRPC listener. The REST API is not served there, but Java's `QdrantStore` defaults
/// to it, so a migrated configuration keeps pointing at it (tracker RAG-22).
pub const DEFAULT_GRPC_PORT: u16 = 6334;

/// Appended to a failed connect when the location still points at [`DEFAULT_GRPC_PORT`].
const GRPC_PORT_HINT: &str =
    "Qdrant's gRPC port 6334 does not serve the REST API; use the REST port 6333 instead";

/// Distance metric stored on the Qdrant collection.
///
/// Java's `createCollection` hardcodes `Distance.Cosine`; the Rust config exposes the knob
/// because the REST API accepts all three, keeping `Cosine` as the default for parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QdrantDistance {
    /// Cosine similarity (Java default).
    #[default]
    Cosine,
    /// Euclidean distance.
    Euclid,
    /// Dot product.
    Dot,
}

impl QdrantDistance {
    /// Wire name accepted by `PUT /collections/{name}`.
    pub fn as_wire(self) -> &'static str {
        match self {
            QdrantDistance::Cosine => "Cosine",
            QdrantDistance::Euclid => "Euclid",
            QdrantDistance::Dot => "Dot",
        }
    }
}

/// Connection / collection configuration for [`QdrantStore`].
#[derive(Debug, Clone, PartialEq)]
pub struct QdrantConfig {
    location: String,
    collection_name: String,
    dimensions: usize,
    api_key: Option<String>,
    use_transport_layer_security: bool,
    distance: QdrantDistance,
}

impl QdrantConfig {
    /// Creates a builder.
    pub fn builder() -> QdrantConfigBuilder {
        QdrantConfigBuilder::default()
    }

    /// Configured location (HTTP URL or `host:port`).
    pub fn location(&self) -> &str {
        &self.location
    }

    /// Configured collection name.
    pub fn collection_name(&self) -> &str {
        &self.collection_name
    }

    /// Configured vector size.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// API key, when configured.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Whether `https` is assumed for bare `host:port` locations.
    pub fn use_transport_layer_security(&self) -> bool {
        self.use_transport_layer_security
    }

    /// Distance metric used when the collection is created.
    pub fn distance(&self) -> QdrantDistance {
        self.distance
    }
}

/// Builder for [`QdrantConfig`], mirroring the Java `QdrantStore.Builder`.
#[derive(Debug, Clone)]
pub struct QdrantConfigBuilder {
    location: Option<String>,
    collection_name: Option<String>,
    dimensions: usize,
    api_key: Option<String>,
    use_transport_layer_security: bool,
    distance: QdrantDistance,
}

impl Default for QdrantConfigBuilder {
    fn default() -> Self {
        Self {
            location: None,
            collection_name: None,
            dimensions: 0,
            api_key: None,
            use_transport_layer_security: true,
            distance: QdrantDistance::Cosine,
        }
    }
}

impl QdrantConfigBuilder {
    /// Sets the Qdrant server location (`http://host:6333`, `host:6334`, ...).
    pub fn location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }

    /// Sets the collection name.
    pub fn collection_name(mut self, collection_name: impl Into<String>) -> Self {
        self.collection_name = Some(collection_name.into());
        self
    }

    /// Sets the vector dimensions.
    pub fn dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Sets the API key (sent as the `api-key` header).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Java `useTransportLayerSecurity` — selects `https` for bare `host:port` locations.
    pub fn use_transport_layer_security(mut self, enabled: bool) -> Self {
        self.use_transport_layer_security = enabled;
        self
    }

    /// Sets the distance metric (`Cosine` by default, like Java).
    pub fn distance(mut self, distance: QdrantDistance) -> Self {
        self.distance = distance;
        self
    }

    /// Validates and builds the configuration.
    pub fn build(self) -> Result<QdrantConfig> {
        let location = self.location.unwrap_or_default();
        if location.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Location cannot be null or empty".to_string(),
            ));
        }
        let collection_name = self.collection_name.unwrap_or_default();
        if collection_name.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Collection name cannot be null or empty".to_string(),
            ));
        }
        if self.dimensions == 0 {
            return Err(RagError::InvalidArgument(
                "Dimensions must be positive".to_string(),
            ));
        }
        // Fail fast on locations the REST port cannot serve (`:memory:` / `file://`).
        parse_rest_base_url(&location, self.use_transport_layer_security)?;

        Ok(QdrantConfig {
            location,
            collection_name,
            dimensions: self.dimensions,
            api_key: self.api_key,
            use_transport_layer_security: self.use_transport_layer_security,
            distance: self.distance,
        })
    }
}

/// Resolves the REST base URL from a Qdrant location string.
///
/// Mirrors Java `parseLocation`: HTTP(S) URLs are used as-is (trailing `/` stripped), a bare
/// `host` or `host:port` gets an `http(s)://` prefix. Two Java-only forms are rejected
/// because Qdrant serves them over gRPC/local mode only: `:memory:` and `file://...`.
///
/// Scheme-less locations are parsed as an authority, never as a URI: Java 2a682f8bf fixed
/// `127.0.0.1:6334` being rejected by `URI.create` (a scheme cannot start with a digit) and
/// silently degrading the whole string to a hostname; see [`split_authority`].
pub fn parse_rest_base_url(location: &str, tls: bool) -> Result<String> {
    let trimmed = location.trim();
    if trimmed.is_empty() {
        return Err(RagError::InvalidArgument(
            "Location cannot be null or empty".to_string(),
        ));
    }
    let lowered = trimmed.to_ascii_lowercase();
    if lowered == ":memory:" || lowered.starts_with("file://") {
        return Err(RagError::Unsupported(format!(
            "QdrantStore over REST does not support local location '{trimmed}'; use an HTTP(S) endpoint"
        )));
    }
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        return Ok(trimmed.trim_end_matches('/').to_string());
    }

    let scheme = if tls { "https" } else { "http" };
    let (host, port) = split_authority(trimmed.trim_end_matches('/'));
    match port {
        Some(port) => Ok(format!("{scheme}://{host}:{port}")),
        None => Ok(format!("{scheme}://{host}:{DEFAULT_HTTP_PORT}")),
    }
}

/// Splits a scheme-less `host[:port]` authority into its host and an explicit port.
///
/// Covers the locations Java 2a682f8bf had to special-case because `URI.create` cannot parse
/// them: dotted IPs (`127.0.0.1:6334`, `10.0.0.5:7334`) and bare host names (`my_host:6334`)
/// were rejected outright and then degraded to `host = "127.0.0.1:16334"`. A bare IPv6 literal
/// (`::1`) is additionally bracketed so the resulting URL stays valid; Java only accepted the
/// bracketed form (`[::1]:6334`).
fn split_authority(authority: &str) -> (String, Option<String>) {
    if authority.matches(':').count() > 1 && !authority.starts_with('[') {
        return (format!("[{authority}]"), None);
    }
    match authority.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            (host.to_string(), Some(port.to_string()))
        }
        _ => (authority.to_string(), None),
    }
}

/// Request body of `PUT /collections/{name}` (Java `VectorParams`).
pub fn create_collection_body(dimensions: usize, distance: QdrantDistance) -> Value {
    json!({"vectors": {"size": dimensions, "distance": distance.as_wire()}})
}

/// Request body of `POST /collections/{name}/points/query` (Java `QueryPoints`).
pub fn build_search_request_body(query: &SearchQuery) -> Value {
    let mut body = Map::new();
    body.insert("query".to_string(), floats_value(&query.query_embedding));
    body.insert("limit".to_string(), json!(query.limit));
    // Java always requests payload + vectors back.
    body.insert("with_payload".to_string(), json!(true));
    body.insert("with_vector".to_string(), json!(true));
    if let Some(name) = &query.vector_name {
        body.insert("using".to_string(), json!(name));
    }
    if let Some(threshold) = query.score_threshold {
        body.insert("score_threshold".to_string(), json!(threshold));
    }
    Value::Object(body)
}

fn floats_value(values: &[f64]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|v| json!(*v as f32 as f64))
            .collect::<Vec<_>>(),
    )
}

/// One scored point as returned by Qdrant (`result.points[]` or the legacy `result[]`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QdrantScoredPoint {
    /// Point id (UUID string; numeric ids are also tolerated).
    #[serde(default)]
    pub id: Value,
    /// Similarity score.
    #[serde(default)]
    pub score: f64,
    /// Stored payload.
    #[serde(default)]
    pub payload: Option<Map<String, Value>>,
    /// Stored vector: a bare array, or an object keyed by vector name.
    #[serde(default)]
    pub vector: Option<Value>,
}

/// Parses a Qdrant query/search response into documents.
///
/// Points that cannot be reconstructed (missing payload / `doc_id` / `chunk_id`) are skipped
/// with a warning, matching Java's `log.warn + continue` behaviour.
pub fn parse_search_response(body: &str, vector_name: Option<&str>) -> Result<Vec<Document>> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(body)?;
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    let points: Vec<QdrantScoredPoint> = match result {
        Value::Array(items) => serde_json::from_value(Value::Array(items))?,
        Value::Object(map) => match map.get("points").cloned() {
            Some(Value::Array(items)) => serde_json::from_value(Value::Array(items))?,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };

    let mut documents = Vec::with_capacity(points.len());
    for point in points {
        match scored_point_to_document(&point, vector_name) {
            Ok(document) => documents.push(document),
            Err(error) => {
                tracing::warn!("failed to reconstruct Qdrant point {}: {error}", point.id)
            }
        }
    }
    Ok(documents)
}

/// Whether a Qdrant delete/update response reported `completed`.
pub fn parse_delete_response(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value
        .get("result")
        .and_then(|result| result.get("status"))
        .and_then(Value::as_str)
        .map(|status| status.eq_ignore_ascii_case("completed"))
        .unwrap_or(false)
}

/// Converts [`DocumentMetadata`] into the Qdrant payload map used by Java.
pub fn metadata_to_payload(metadata: &DocumentMetadata) -> Map<String, Value> {
    let mut payload = Map::new();
    let content = serde_json::to_value(&metadata.content)
        .unwrap_or_else(|_| Value::String(metadata.content_text()));
    payload.insert("content".to_string(), content);
    payload.insert("doc_id".to_string(), json!(metadata.doc_id));
    payload.insert("chunk_id".to_string(), json!(metadata.chunk_id));
    if let Value::Object(custom) = &metadata.payload {
        if !custom.is_empty() {
            payload.insert("payload".to_string(), Value::Object(custom.clone()));
        }
    }
    payload
}

/// Rebuilds [`DocumentMetadata`] from a Qdrant payload map.
pub fn payload_to_metadata(payload: &Map<String, Value>) -> Result<DocumentMetadata> {
    let content_value = payload
        .get("content")
        .ok_or_else(|| RagError::InvalidArgument("Payload missing 'content' field".to_string()))?;
    let content = content_from_value(content_value);

    let doc_id = payload
        .get("doc_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RagError::InvalidArgument("Payload missing 'doc_id' field".to_string()))?;
    let chunk_id = payload
        .get("chunk_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RagError::InvalidArgument("Payload missing 'chunk_id' field".to_string()))?;

    let custom = match payload.get("payload") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(other) => {
            tracing::warn!("expected an object for the Qdrant 'payload' field, got {other}");
            Value::Object(Map::new())
        }
        None => Value::Object(Map::new()),
    };

    Ok(DocumentMetadata::with_payload(
        content, doc_id, chunk_id, custom,
    ))
}

fn content_from_value(value: &Value) -> ContentBlock {
    if let Ok(block) = serde_json::from_value::<ContentBlock>(value.clone()) {
        return block;
    }
    // Java falls back to a TextBlock built from the string representation.
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => match map.get("text").and_then(Value::as_str) {
            Some(text) => text.to_string(),
            None => value.to_string(),
        },
        other => other.to_string(),
    };
    ContentBlock::Text(TextBlock::new(text))
}

fn scored_point_to_document(
    point: &QdrantScoredPoint,
    vector_name: Option<&str>,
) -> Result<Document> {
    let payload = point
        .payload
        .as_ref()
        .filter(|payload| !payload.is_empty())
        .ok_or_else(|| RagError::InvalidArgument("Payload missing 'content' field".to_string()))?;

    let metadata = payload_to_metadata(payload)?;
    // Java rebuilds the id deterministically from the metadata (`new Document(metadata)`).
    let mut document = Document::new(metadata);
    document.set_score(point.score);
    if let Some(name) = vector_name {
        document.vector_name = Some(name.to_string());
    }
    if let Some(vector) = &point.vector {
        if let Some(embedding) = extract_embedding(vector, vector_name) {
            document.set_embedding(embedding);
        }
    }
    Ok(document)
}

fn extract_embedding(vector: &Value, vector_name: Option<&str>) -> Option<Vec<f64>> {
    match vector {
        Value::Array(items) => to_floats(items),
        Value::Object(map) => {
            if let Some(name) = vector_name {
                if let Some(named) = map.get(name) {
                    return extract_embedding(named, None);
                }
            }
            map.values()
                .find_map(|value| extract_embedding(value, None))
        }
        _ => None,
    }
}

fn to_floats(items: &[Value]) -> Option<Vec<f64>> {
    items.iter().map(Value::as_f64).collect()
}

/// Qdrant vector store over the Qdrant REST API.
pub struct QdrantStore {
    config: QdrantConfig,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
    closed: AtomicBool,
}

impl std::fmt::Debug for QdrantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QdrantStore")
            .field("collection_name", &self.config.collection_name)
            .field("base_url", &self.base_url)
            .field("dimensions", &self.config.dimensions)
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl QdrantStore {
    /// Creates a store over an injected transport. No I/O happens here.
    pub fn new(config: QdrantConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let base_url =
            parse_rest_base_url(config.location(), config.use_transport_layer_security())?;
        Ok(Self {
            config,
            transport,
            base_url,
            closed: AtomicBool::new(false),
        })
    }

    /// Creates a store backed by the production [`ReqwestTransport`]. No I/O happens here.
    pub fn with_http(config: QdrantConfig) -> Result<Self> {
        let transport = Arc::new(ReqwestTransport::new(&TransportOptions::default())?);
        Self::new(config, transport)
    }

    /// Creates a [`ReqwestTransport`]-backed store and ensures the collection exists
    /// (Java's constructor behaviour).
    pub async fn connect(config: QdrantConfig) -> Result<Self> {
        let transport = Arc::new(ReqwestTransport::new(&TransportOptions::default())?);
        Self::connect_with(config, transport).await
    }

    /// Creates a store over an injected transport and ensures the collection exists.
    pub async fn connect_with(
        config: QdrantConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let store = Self::new(config, transport)?;
        store
            .ensure_collection()
            .await
            .map_err(|error| store.with_rest_port_hint(error))?;
        Ok(store)
    }

    /// Configuration snapshot.
    pub fn config(&self) -> &QdrantConfig {
        &self.config
    }

    /// Original location string.
    pub fn location(&self) -> &str {
        self.config.location()
    }

    /// Collection name.
    pub fn collection_name(&self) -> &str {
        self.config.collection_name()
    }

    /// Vector dimensions.
    pub fn dimensions(&self) -> usize {
        self.config.dimensions()
    }

    /// API key, when configured.
    pub fn api_key(&self) -> Option<&str> {
        self.config.api_key()
    }

    /// Resolved REST base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether [`close`](Self::close) has been called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Marks the store closed; every later operation fails with
    /// `VectorStore("QdrantStore has been closed")`. Idempotent, like Java `close()`.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Ensures the collection exists, creating it with the configured size/distance when the
    /// server answers `404`.
    pub async fn ensure_collection(&self) -> Result<()> {
        match self
            .execute(HttpMethod::Get, self.collection_url(), None)
            .await
        {
            Ok(_) => Ok(()),
            Err(RagError::Api {
                status_code: 404, ..
            }) => {
                self.create_collection().await.map_err(|error| {
                    RagError::VectorStore(format!(
                        "Failed to ensure collection exists: {}: {error}",
                        self.config.collection_name
                    ))
                })?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn create_collection(&self) -> Result<HttpResponse> {
        let body = create_collection_body(self.config.dimensions, self.config.distance);
        self.execute(HttpMethod::Put, self.collection_url(), Some(body))
            .await
    }

    /// Whether the resolved base URL points at Qdrant's gRPC listener.
    fn uses_grpc_port(&self) -> bool {
        self.base_url
            .rsplit(':')
            .next()
            .and_then(|port| port.parse::<u16>().ok())
            == Some(DEFAULT_GRPC_PORT)
    }

    /// Appends the REST-port hint to a connect failure (tracker RAG-22): the port mapping stays
    /// untouched, only the diagnosis is made visible.
    fn with_rest_port_hint(&self, error: RagError) -> RagError {
        if !self.uses_grpc_port() {
            return error;
        }
        match error {
            RagError::Http(message) => RagError::Http(format!("{message} ({GRPC_PORT_HINT})")),
            RagError::Api {
                status_code,
                error_code,
                message,
            } => RagError::Api {
                status_code,
                error_code,
                message: format!("{message} ({GRPC_PORT_HINT})"),
            },
            RagError::Auth {
                status_code,
                message,
            } => RagError::Auth {
                status_code,
                message: format!("{message} ({GRPC_PORT_HINT})"),
            },
            other => other,
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.is_closed() {
            return Err(RagError::VectorStore(
                "QdrantStore has been closed".to_string(),
            ));
        }
        Ok(())
    }

    fn collection_url(&self) -> String {
        format!(
            "{}/collections/{}",
            self.base_url, self.config.collection_name
        )
    }

    fn points_url(&self, suffix: &str) -> String {
        format!("{}/points{suffix}", self.collection_url())
    }

    async fn execute(
        &self,
        method: HttpMethod,
        url: String,
        body: Option<Value>,
    ) -> Result<HttpResponse> {
        let mut request = HttpRequest::new(method, url);
        if let Some(body) = body {
            request.body = Some(serde_json::to_string(&body)?);
            request = request.with_header("Content-Type", "application/json; charset=utf-8");
        }
        if let Some(api_key) = self.config.api_key() {
            let api_key = api_key.trim();
            if !api_key.is_empty() {
                request = request.with_header("api-key", api_key);
            }
        }

        let response = self.transport.execute(request).await?;
        if !response.is_success() {
            return Err(map_http_error(response.status_code, &response.body));
        }
        Ok(response)
    }

    fn build_point(&self, document: &Document) -> Result<Value> {
        let embedding = document.embedding.as_ref().ok_or_else(|| {
            RagError::InvalidArgument("Document must have embedding set".to_string())
        })?;
        let id = validate_point_id(&document.id)?;
        let vector = match &document.vector_name {
            Some(name) => json!({ name.clone(): floats_value(embedding) }),
            None => floats_value(embedding),
        };
        Ok(json!({
            "id": id,
            "vector": vector,
            "payload": Value::Object(metadata_to_payload(&document.metadata)),
        }))
    }
}

fn validate_point_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if is_uuid(trimmed) {
        Ok(trimmed.to_ascii_lowercase())
    } else {
        // Same wording as `java.util.UUID.fromString`.
        Err(RagError::InvalidArgument(format!(
            "Invalid UUID string: {id}"
        )))
    }
}

fn is_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    const LENGTHS: [usize; 5] = [8, 4, 4, 4, 12];
    parts.len() == LENGTHS.len()
        && parts.iter().zip(LENGTHS.iter()).all(|(part, expected)| {
            part.len() == *expected && part.chars().all(|c| c.is_ascii_hexdigit())
        })
}

#[async_trait]
impl VdbStore for QdrantStore {
    async fn add(&self, documents: Vec<Document>) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        for document in &documents {
            let embedding = document.embedding.as_ref().ok_or_else(|| {
                RagError::InvalidArgument("Document must have embedding set".to_string())
            })?;
            if embedding.len() != self.config.dimensions {
                return Err(RagError::VectorStore(format!(
                    "Embedding dimension mismatch: expected {}, got {}",
                    self.config.dimensions,
                    embedding.len()
                )));
            }
        }

        self.ensure_open()?;
        let points = documents
            .iter()
            .map(|document| self.build_point(document))
            .collect::<Result<Vec<_>>>()?;

        self.execute(
            HttpMethod::Put,
            self.points_url("?wait=true"),
            Some(json!({ "points": points })),
        )
        .await?;
        Ok(())
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<Document>> {
        if query.query_embedding.len() != self.config.dimensions {
            return Err(RagError::VectorStore(format!(
                "Query embedding dimension mismatch: expected {}, got {}",
                self.config.dimensions,
                query.query_embedding.len()
            )));
        }
        if query.limit == 0 {
            return Err(RagError::InvalidArgument(
                "Limit must be positive".to_string(),
            ));
        }

        self.ensure_open()?;
        let body = build_search_request_body(query);
        let response = self
            .execute(HttpMethod::Post, self.points_url("/query"), Some(body))
            .await?;
        parse_search_response(&response.body, query.vector_name.as_deref())
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        if id.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Document ID cannot be null or empty".to_string(),
            ));
        }
        self.ensure_open()?;
        let id = validate_point_id(id)?;
        let response = self
            .execute(
                HttpMethod::Post,
                self.points_url("/delete?wait=true"),
                Some(json!({ "points": [id] })),
            )
            .await?;
        Ok(parse_delete_response(&response.body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::model::DocumentMetadata;

    struct MockTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl MockTransport {
        fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn body_of(&self, index: usize) -> Value {
            let requests = self.requests();
            let body = requests[index]
                .body
                .as_deref()
                .expect("request has no body");
            serde_json::from_str(body).expect("request body is not JSON")
        }
    }

    #[async_trait]
    impl HttpTransport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| RagError::Http("no scripted response".to_string()))
        }
    }

    fn config() -> QdrantConfig {
        QdrantConfig::builder()
            .location("http://localhost:6333")
            .collection_name("knowledge")
            .dimensions(2)
            .build()
            .unwrap()
    }

    fn config_with_key() -> QdrantConfig {
        QdrantConfig::builder()
            .location("http://localhost:6333/")
            .collection_name("knowledge")
            .dimensions(2)
            .api_key("secret")
            .build()
            .unwrap()
    }

    fn doc(id: &str, embedding: Vec<f64>) -> Document {
        let mut document =
            Document::with_id(id, DocumentMetadata::text("hello", "doc-1", "chunk-1"));
        document.set_embedding(embedding);
        document
    }

    const DOC_ID: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn parse_base_url_normalizes_locations() {
        assert_eq!(
            parse_rest_base_url("http://localhost:6333/", true).unwrap(),
            "http://localhost:6333"
        );
        assert_eq!(
            parse_rest_base_url("localhost:6333", false).unwrap(),
            "http://localhost:6333"
        );
        assert_eq!(
            parse_rest_base_url("qdrant.internal", true).unwrap(),
            "https://qdrant.internal:6333"
        );
        assert_eq!(
            parse_rest_base_url(" qdrant.internal ", false).unwrap(),
            "http://qdrant.internal:6333"
        );
    }

    #[test]
    fn parse_base_url_accepts_scheme_less_ip_locations() {
        // Java 2a682f8bf: a scheme-less location is a host[:port] authority, not a URI.
        // `URI.create("127.0.0.1:6334")` throws (a scheme cannot start with a digit) and the
        // old catch block turned the whole string into the hostname, so the store failed to
        // build. The REST port keeps the explicit port as-is (Java increments 6333 -> 6334
        // only because it then speaks gRPC).
        for (location, expected) in [
            ("http://localhost:6333", "http://localhost:6333"),
            ("https://qdrant.example.com", "https://qdrant.example.com"),
            ("http://127.0.0.1:16334", "http://127.0.0.1:16334"),
            ("localhost:6334", "http://localhost:6334"),
            ("localhost", "http://localhost:6333"),
            ("qdrant-0.svc:7334", "http://qdrant-0.svc:7334"),
            ("127.0.0.1:6334", "http://127.0.0.1:6334"),
            ("10.0.0.5:7334", "http://10.0.0.5:7334"),
            ("127.0.0.1", "http://127.0.0.1:6333"),
            ("my_host:6334", "http://my_host:6334"),
            ("[::1]:6334", "http://[::1]:6334"),
            ("[::1]", "http://[::1]:6333"),
            // Bare IPv6 literals are bracketed so the URL stays valid (Java only accepted
            // the bracketed form).
            ("::1", "http://[::1]:6333"),
            ("2001:db8::1", "http://[2001:db8::1]:6333"),
        ] {
            assert_eq!(
                parse_rest_base_url(location, false).unwrap(),
                expected,
                "location = {location}"
            );
        }

        assert_eq!(
            parse_rest_base_url("127.0.0.1:6334", true).unwrap(),
            "https://127.0.0.1:6334"
        );
    }

    #[test]
    fn builder_accepts_a_scheme_less_ip_location() {
        let config = QdrantConfig::builder()
            .location("127.0.0.1:6334")
            .collection_name("knowledge")
            .dimensions(2)
            .build()
            .unwrap();
        assert_eq!(config.location(), "127.0.0.1:6334");
        // Java defaults `useTransportLayerSecurity = true`, so a scheme-less authority keeps
        // https; the fix is that the location parses at all (previously the store failed to
        // build). Turning TLS off yields plain http.
        assert_eq!(
            parse_rest_base_url(config.location(), config.use_transport_layer_security()).unwrap(),
            "https://127.0.0.1:6334"
        );
        assert_eq!(
            parse_rest_base_url(config.location(), false).unwrap(),
            "http://127.0.0.1:6334"
        );
    }

    #[test]
    fn parse_base_url_rejects_local_and_empty_locations() {
        assert!(matches!(
            parse_rest_base_url("", false),
            Err(RagError::InvalidArgument(_))
        ));
        assert!(matches!(
            parse_rest_base_url(":memory:", false),
            Err(RagError::Unsupported(_))
        ));
        assert!(matches!(
            parse_rest_base_url("file:///tmp/qdrant", false),
            Err(RagError::Unsupported(_))
        ));
    }

    #[test]
    fn builder_validates_required_fields_with_java_messages() {
        let error = QdrantConfig::builder()
            .collection_name("c")
            .dimensions(2)
            .build()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Location cannot be null or empty"));

        let error = QdrantConfig::builder()
            .location("http://localhost:6333")
            .dimensions(2)
            .build()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Collection name cannot be null or empty"));

        let error = QdrantConfig::builder()
            .location("http://localhost:6333")
            .collection_name("c")
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("Dimensions must be positive"));
    }

    #[test]
    fn create_collection_body_matches_vector_params() {
        assert_eq!(
            create_collection_body(1024, QdrantDistance::Cosine),
            json!({"vectors": {"size": 1024, "distance": "Cosine"}})
        );
        assert_eq!(
            create_collection_body(3, QdrantDistance::Dot)["vectors"]["distance"],
            json!("Dot")
        );
    }

    #[tokio::test]
    async fn connect_creates_missing_collection() {
        let transport = MockTransport::new(vec![
            HttpResponse::new(404, "{\"status\":{\"error\":\"Not found\"}}"),
            HttpResponse::new(200, "{\"result\":true}"),
        ]);
        let store = QdrantStore::connect_with(config_with_key(), transport.clone())
            .await
            .unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "http://localhost:6333/collections/knowledge"
        );
        assert_eq!(requests[0].header("api-key"), Some("secret"));
        assert_eq!(requests[1].method, HttpMethod::Put);
        assert_eq!(
            transport.body_of(1),
            json!({"vectors": {"size": 2, "distance": "Cosine"}})
        );
        assert_eq!(store.base_url(), "http://localhost:6333");
    }

    #[tokio::test]
    async fn connect_keeps_existing_collection() {
        let transport = MockTransport::new(vec![HttpResponse::new(200, "{\"result\":{}}")]);
        QdrantStore::connect_with(config(), transport.clone())
            .await
            .unwrap();
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn connect_surfaces_auth_errors_from_the_probe() {
        let transport = MockTransport::new(vec![HttpResponse::new(403, "forbidden")]);
        let error = QdrantStore::connect_with(config(), transport.clone())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RagError::Auth {
                status_code: 403,
                ..
            }
        ));
    }

    /// Java's default location is the gRPC port; a REST connect there must say so (RAG-22).
    #[tokio::test]
    async fn a_connect_failure_on_the_grpc_port_hints_the_rest_port() {
        let transport = MockTransport::new(vec![HttpResponse::new(503, "service unavailable")]);
        let config = QdrantConfig::builder()
            .location("127.0.0.1:6334")
            .collection_name("knowledge")
            .dimensions(2)
            .build()
            .unwrap();

        let error = QdrantStore::connect_with(config, transport.clone())
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("gRPC port 6334"), "{message}");
        assert!(message.contains("6333"), "{message}");
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_connect_failure_on_the_rest_port_has_no_port_hint() {
        let transport = MockTransport::new(vec![HttpResponse::new(503, "service unavailable")]);
        let error = QdrantStore::connect_with(config(), transport)
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("gRPC port 6334"));
    }

    #[tokio::test]
    async fn add_upserts_points_with_payload() {
        let transport = MockTransport::new(vec![HttpResponse::new(200, "{\"result\":{}}")]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        store.add(vec![doc(DOC_ID, vec![1.0, 0.5])]).await.unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Put);
        assert_eq!(
            requests[0].url,
            "http://localhost:6333/collections/knowledge/points?wait=true"
        );
        let body = transport.body_of(0);
        let point = &body["points"][0];
        assert_eq!(point["id"], json!(DOC_ID));
        assert_eq!(point["vector"], json!([1.0, 0.5]));
        assert_eq!(point["payload"]["doc_id"], json!("doc-1"));
        assert_eq!(point["payload"]["chunk_id"], json!("chunk-1"));
        assert_eq!(
            point["payload"]["content"],
            json!({"type": "text", "text": "hello"})
        );
    }

    #[tokio::test]
    async fn add_uses_named_vector_when_vector_name_is_set() {
        let transport = MockTransport::new(vec![HttpResponse::new(200, "{}")]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        let mut document = doc(DOC_ID, vec![0.25, 0.75]);
        document.vector_name = Some("dense".to_string());
        store.add(vec![document]).await.unwrap();

        let body = transport.body_of(0);
        assert_eq!(body["points"][0]["vector"]["dense"], json!([0.25, 0.75]));
    }

    #[tokio::test]
    async fn add_includes_custom_payload() {
        let transport = MockTransport::new(vec![HttpResponse::new(200, "{}")]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        let metadata = DocumentMetadata::with_payload(
            ContentBlock::Text(TextBlock::new("hello")),
            "doc-1",
            "chunk-1",
            json!({"department": "hr", "rank": 3}),
        );
        let mut document = Document::with_id(DOC_ID, metadata);
        document.set_embedding(vec![1.0, 0.0]);
        store.add(vec![document]).await.unwrap();

        let body = transport.body_of(0);
        assert_eq!(
            body["points"][0]["payload"]["payload"],
            json!({"department": "hr", "rank": 3})
        );
    }

    #[tokio::test]
    async fn add_validates_embeddings_before_touching_the_network() {
        let transport = MockTransport::new(vec![]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();

        let error = store
            .add(vec![Document::with_id(
                DOC_ID,
                DocumentMetadata::text("x", "d", "c"),
            )])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Document must have embedding set"));

        let error = store
            .add(vec![doc(DOC_ID, vec![1.0, 2.0, 3.0])])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Embedding dimension mismatch: expected 2, got 3"));

        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn add_with_no_documents_is_a_noop() {
        let transport = MockTransport::new(vec![]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        store.add(vec![]).await.unwrap();
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn add_rejects_non_uuid_ids() {
        let transport = MockTransport::new(vec![]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        let error = store
            .add(vec![doc("not-a-uuid", vec![1.0, 0.0])])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Invalid UUID string: not-a-uuid"));
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn search_builds_query_body_and_maps_results() {
        let response = json!({
            "result": {
                "points": [{
                    "id": DOC_ID,
                    "score": 0.87,
                    "payload": {
                        "content": {"type": "text", "text": "hello"},
                        "doc_id": "doc-1",
                        "chunk_id": "chunk-1",
                        "payload": {"src": "web"}
                    },
                    "vector": [0.5, 0.5]
                }]
            }
        });
        let transport = MockTransport::new(vec![HttpResponse::new(200, response.to_string())]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();

        let query = SearchQuery::new(vec![1.0, 0.0], 3)
            .with_vector_name("dense")
            .with_score_threshold(0.5);
        let results = store.search(&query).await.unwrap();

        let requests = transport.requests();
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "http://localhost:6333/collections/knowledge/points/query"
        );
        let body = transport.body_of(0);
        assert_eq!(body["query"], json!([1.0, 0.0]));
        assert_eq!(body["limit"], json!(3));
        assert_eq!(body["with_payload"], json!(true));
        assert_eq!(body["with_vector"], json!(true));
        assert_eq!(body["using"], json!("dense"));
        assert_eq!(body["score_threshold"], json!(0.5));

        assert_eq!(results.len(), 1);
        assert!((results[0].score.unwrap() - 0.87).abs() < 1e-9);
        assert_eq!(results[0].metadata.content_text(), "hello");
        assert_eq!(results[0].metadata.doc_id, "doc-1");
        assert_eq!(results[0].payload_value("src"), Some(&json!("web")));
        assert_eq!(results[0].vector_name.as_deref(), Some("dense"));
        assert_eq!(results[0].embedding, Some(vec![0.5, 0.5]));
    }

    #[tokio::test]
    async fn search_accepts_legacy_array_results_and_named_vectors() {
        let response = json!({ "result": [{
            "id": DOC_ID,
            "score": 0.4,
            "payload": {
                "content": {"type": "text", "text": "legacy"},
                "doc_id": "doc-2",
                "chunk_id": "chunk-9"
            },
            "vector": {"dense": [0.1, 0.2]}
        }], "status": "ok"});
        let transport = MockTransport::new(vec![HttpResponse::new(200, response.to_string())]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();

        let results = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 5).with_vector_name("dense"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata.content_text(), "legacy");
        assert!(results[0].payload().is_empty());
        assert_eq!(results[0].embedding, Some(vec![0.1, 0.2]));
    }

    #[tokio::test]
    async fn search_skips_points_without_payload() {
        let response = json!({
            "result": {"points": [
                {"id": DOC_ID, "score": 0.9, "vector": [1.0, 0.0]},
                {"id": DOC_ID, "score": 0.8, "payload": {"content": {"type": "text", "text": "ok"}, "doc_id": "d", "chunk_id": "c"}, "vector": [1.0, 0.0]}
            ]}
        });
        let transport = MockTransport::new(vec![HttpResponse::new(200, response.to_string())]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        let results = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 5))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata.content_text(), "ok");
    }

    #[tokio::test]
    async fn search_validates_dimensions_and_limit() {
        let transport = MockTransport::new(vec![]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();

        let error = store
            .search(&SearchQuery::new(vec![1.0], 5))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Query embedding dimension mismatch: expected 2, got 1"));

        let error = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 0))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Limit must be positive"));
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn delete_reports_completion() {
        let transport = MockTransport::new(vec![HttpResponse::new(
            200,
            "{\"result\":{\"operation_id\":7,\"status\":\"completed\"}}",
        )]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        assert!(store.delete(DOC_ID).await.unwrap());

        let requests = transport.requests();
        assert_eq!(
            requests[0].url,
            "http://localhost:6333/collections/knowledge/points/delete?wait=true"
        );
        assert_eq!(transport.body_of(0), json!({"points": [DOC_ID]}));
    }

    #[tokio::test]
    async fn delete_returns_false_for_acknowledged_updates() {
        let transport = MockTransport::new(vec![HttpResponse::new(
            200,
            "{\"result\":{\"status\":\"acknowledged\"}}",
        )]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        assert!(!store.delete(DOC_ID).await.unwrap());
    }

    #[tokio::test]
    async fn delete_validates_ids() {
        let transport = MockTransport::new(vec![]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        let error = store.delete("   ").await.unwrap_err();
        assert!(error
            .to_string()
            .contains("Document ID cannot be null or empty"));
        assert!(store.delete("42").await.is_err());
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn http_errors_map_to_rag_error_variants() {
        let transport = MockTransport::new(vec![HttpResponse::new(
            500,
            "{\"status\":{\"error\":\"boom\"}}",
        )]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        let error = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 1))
            .await
            .unwrap_err();
        match error {
            RagError::Api {
                status_code,
                message,
                ..
            } => {
                assert_eq!(status_code, 500);
                assert!(message.contains("boom") || message.contains("status"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }

        let transport = MockTransport::new(vec![HttpResponse::new(401, "unauthorized")]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        let error = store
            .add(vec![doc(DOC_ID, vec![1.0, 0.0])])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RagError::Auth {
                status_code: 401,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn closed_store_rejects_operations() {
        let transport = MockTransport::new(vec![]);
        let store = QdrantStore::new(config(), transport.clone()).unwrap();
        store.close();
        store.close();
        assert!(store.is_closed());

        let error = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("QdrantStore has been closed"));
        let error = store.delete(DOC_ID).await.unwrap_err();
        assert!(error.to_string().contains("QdrantStore has been closed"));
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn payload_round_trips_content_and_custom_fields() {
        let metadata = DocumentMetadata::with_payload(
            ContentBlock::Text(TextBlock::new("round trip")),
            "doc-9",
            "chunk-9",
            json!({"k": [1, 2]}),
        );
        let payload = metadata_to_payload(&metadata);
        let rebuilt = payload_to_metadata(&payload).unwrap();
        assert_eq!(rebuilt, metadata);
    }

    #[test]
    fn payload_missing_required_fields_errors() {
        let mut payload = Map::new();
        payload.insert("content".to_string(), json!("text"));
        let error = payload_to_metadata(&payload).unwrap_err();
        assert!(error.to_string().contains("Payload missing 'doc_id' field"));

        let mut payload = Map::new();
        payload.insert("content".to_string(), json!("text"));
        payload.insert("doc_id".to_string(), json!("d"));
        let error = payload_to_metadata(&payload).unwrap_err();
        assert!(error
            .to_string()
            .contains("Payload missing 'chunk_id' field"));
    }

    #[test]
    fn non_content_json_falls_back_to_text_block() {
        let mut payload = Map::new();
        payload.insert("content".to_string(), json!("plain string"));
        payload.insert("doc_id".to_string(), json!("d"));
        payload.insert("chunk_id".to_string(), json!("c"));
        let metadata = payload_to_metadata(&payload).unwrap();
        assert_eq!(metadata.content_text(), "plain string");
    }
}
