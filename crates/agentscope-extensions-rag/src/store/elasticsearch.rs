//! Elasticsearch vector store.
//!
//! Ports `io.agentscope.core.rag.store.ElasticsearchStore` (526 lines). The Java class uses the
//! official `elasticsearch-java` client (whose `Rest5ClientTransport` is HTTP underneath); the
//! Rust port speaks the same REST API directly through the shared [`crate::transport`] seam, so
//! every request body/response is a pure function testable with a mock transport (same shape as
//! [`crate::store::qdrant`]).
//!
//! Mapping notes (Java → Rust):
//!
//! * `indices().exists(ExistsRequest)` → `GET /{index}` (`200` = exists, `404` = create). The
//!   shared transport has no `HEAD` verb; the semantics (status-only probe) are identical.
//! * `indices().create(CreateIndexRequest)` → `PUT /{index}` with the Java `dense_vector`
//!   mapping: `id`/`doc_id`/`chunk_id` keyword, `content`/`payload` text without indexing,
//!   `vector` dense_vector (dims, indexed, cosine similarity).
//! * `client.bulk(BulkRequest)` → `POST /_bulk` with NDJSON `{"index":{"_index":…,"_id":…}}`
//!   + document pairs; `errors: true` fails with the Java message
//!     `Elasticsearch bulk indexing failed` plus the per-item reasons Java logs.
//! * `client.search(SearchRequest)` → `POST /{index}/_search` with the **kNN** body Java builds:
//!   `knn.field = vector`, `knn.k = limit`, `knn.num_candidates = max(limit * 2, 50)`,
//!   `size = limit`, `min_score = score_threshold` (or `0.0` when absent, like Java).
//!   `SearchQuery::vector_name` overrides `knn.field` (Java always uses the single vector
//!   field).
//! * `client.delete(DeleteRequest)` → `DELETE /{index}/_doc/{id}`, returning `true` only for
//!   `"result": "deleted"` (Java compares `result().name()` with `Deleted`), so
//!   `not_found` maps to `false`.
//! * Auth: Java sets basic credentials only when **both** `username` and `password` are
//!   non-null; the Rust port sends `Authorization: Basic …` under the same condition.
//!
//! Deliberate differences:
//!
//! * Failures are **not** wrapped in `VectorStoreException("Failed to add documents")`; the
//!   status-bearing [`RagError::Api`] / [`RagError::Auth`] from [`map_http_error`] is surfaced
//!   directly (only `ensure_index` wraps its create step, and Java's own bulk message is kept
//!   for `errors: true`). Callers can branch on `status_code()`.
//! * `disable_ssl_verification` is accepted for Java parity but has no effect: TLS is owned by
//!   the injected transport (`ReqwestTransport`), and this crate never disables verification
//!   silently.
//! * Lazy creation is explicit: [`ElasticsearchStore::connect_with`] (or
//!   [`ElasticsearchStore::ensure_index`]) performs Java's constructor-time check;
//!   [`ElasticsearchStore::new`] performs no I/O.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, TextBlock};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::error::{RagError, Result};
use crate::model::{Document, DocumentMetadata};
use crate::store::{SearchQuery, VdbStore};
use crate::transport::{
    map_http_error, HttpMethod, HttpRequest, HttpResponse, HttpTransport, ReqwestTransport,
    TransportOptions,
};

/// Default endpoint, like the Java `Builder` default.
pub const DEFAULT_URL: &str = "http://localhost:9200";
/// Bulk endpoint (index independent).
pub const BULK_PATH: &str = "/_bulk";

/// Primary-key field (`id`).
pub const FIELD_ID: &str = "id";
/// Vector field (`vector`).
pub const FIELD_VECTOR: &str = "vector";
/// Owning document id field (`doc_id`).
pub const FIELD_DOC_ID: &str = "doc_id";
/// Chunk id field (`chunk_id`).
pub const FIELD_CHUNK_ID: &str = "chunk_id";
/// Serialized `ContentBlock` field (`content`).
pub const FIELD_CONTENT: &str = "content";
/// Serialized custom payload field (`payload`).
pub const FIELD_PAYLOAD: &str = "payload";

/// Similarity used by the `dense_vector` mapping (Java `DenseVectorSimilarity.Cosine`).
pub const VECTOR_SIMILARITY: &str = "cosine";

/// Connection / index configuration for [`ElasticsearchStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElasticsearchConfig {
    url: String,
    index_name: String,
    dimensions: usize,
    username: Option<String>,
    password: Option<String>,
    disable_ssl_verification: bool,
}

impl ElasticsearchConfig {
    /// Creates a builder.
    pub fn builder() -> ElasticsearchConfigBuilder {
        ElasticsearchConfigBuilder::default()
    }

    /// Configured endpoint URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Configured index name.
    pub fn index_name(&self) -> &str {
        &self.index_name
    }

    /// Configured vector dimensions.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Configured username, when set.
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// Configured password, when set.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// Java `disableSslVerification`; accepted for parity, TLS is owned by the transport.
    pub fn disable_ssl_verification(&self) -> bool {
        self.disable_ssl_verification
    }

    /// `Authorization: Basic …` value, or `None` when either credential is missing.
    ///
    /// Java only installs a `CredentialsProvider` when both fields are non-null, so a lone
    /// username never produces an auth header.
    pub fn basic_auth(&self) -> Option<String> {
        let username = self.username.as_deref().filter(|value| !value.is_empty())?;
        let password = self.password.as_deref().filter(|value| !value.is_empty())?;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        Some(format!("Basic {encoded}"))
    }
}

/// Builder for [`ElasticsearchConfig`], mirroring the Java `ElasticsearchStore.Builder`.
#[derive(Debug, Clone)]
pub struct ElasticsearchConfigBuilder {
    url: String,
    index_name: Option<String>,
    dimensions: usize,
    username: Option<String>,
    password: Option<String>,
    disable_ssl_verification: bool,
}

impl Default for ElasticsearchConfigBuilder {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            index_name: None,
            dimensions: 0,
            username: None,
            password: None,
            disable_ssl_verification: false,
        }
    }
}

impl ElasticsearchConfigBuilder {
    /// Sets the endpoint URL (`http://localhost:9200` by default).
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Sets the index name.
    pub fn index_name(mut self, index_name: impl Into<String>) -> Self {
        self.index_name = Some(index_name.into());
        self
    }

    /// Sets the vector dimensions.
    pub fn dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Sets the basic-auth username.
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Sets the basic-auth password.
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Java `disableSslVerification` (no-op for the injected transport).
    pub fn disable_ssl_verification(mut self, disable: bool) -> Self {
        self.disable_ssl_verification = disable;
        self
    }

    /// Validates and builds the configuration, using the Java error messages.
    pub fn build(self) -> Result<ElasticsearchConfig> {
        if self.url.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "URL cannot be null or empty".to_string(),
            ));
        }
        let index_name = self.index_name.unwrap_or_default();
        if index_name.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Index name cannot be null or empty".to_string(),
            ));
        }
        if self.dimensions == 0 {
            return Err(RagError::InvalidArgument(
                "Dimensions must be positive".to_string(),
            ));
        }
        normalize_base_url(&self.url)?;

        Ok(ElasticsearchConfig {
            url: self.url,
            index_name,
            dimensions: self.dimensions,
            username: self.username,
            password: self.password,
            disable_ssl_verification: self.disable_ssl_verification,
        })
    }
}

/// Normalises an Elasticsearch endpoint to a base URL without a trailing slash.
///
/// Java `HttpHost.create(url)` requires a scheme, so bare `host:port` values are rejected here
/// instead of failing at the first request.
pub fn normalize_base_url(url: &str) -> Result<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(RagError::InvalidArgument(
            "URL cannot be null or empty".to_string(),
        ));
    }
    let lowered = trimmed.to_ascii_lowercase();
    if !lowered.starts_with("http://") && !lowered.starts_with("https://") {
        return Err(RagError::InvalidArgument(format!(
            "URL must start with 'http://' or 'https://': {url}"
        )));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

/// Percent-encodes one URL path segment (RFC 3986 unreserved characters are kept).
pub fn escape_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn floats_value(values: &[f64]) -> Value {
    Value::Array(values.iter().map(|v| json!(*v as f32 as f64)).collect())
}

/// Request body of `PUT /{index}` (Java `CreateIndexRequest` mappings).
pub fn create_index_body(dimensions: usize) -> Value {
    json!({
        "mappings": {
            "properties": {
                FIELD_ID: { "type": "keyword" },
                FIELD_DOC_ID: { "type": "keyword" },
                FIELD_CHUNK_ID: { "type": "keyword" },
                FIELD_CONTENT: { "type": "text", "index": false },
                FIELD_VECTOR: {
                    "type": "dense_vector",
                    "dims": dimensions,
                    "index": true,
                    "similarity": VECTOR_SIMILARITY
                },
                FIELD_PAYLOAD: { "type": "text", "index": false }
            }
        }
    })
}

/// Serialized `content` field: JSON text of the [`ContentBlock`].
pub fn content_field(metadata: &DocumentMetadata) -> String {
    match serde_json::to_string(&metadata.content) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!("Failed to serialize content, using text representation: {error}");
            metadata.content_text()
        }
    }
}

/// One Elasticsearch `_source` document (Java `mapToEsDocument`).
pub fn document_source(document: &Document, dimensions: usize) -> Result<Value> {
    let embedding = document
        .embedding
        .as_ref()
        .ok_or_else(|| RagError::InvalidArgument("Document must have embedding set".to_string()))?;
    if embedding.len() != dimensions {
        return Err(RagError::VectorStore(format!(
            "Embedding dimension mismatch: expected {dimensions}, got {}",
            embedding.len()
        )));
    }

    let mut source = Map::new();
    source.insert(FIELD_ID.to_string(), json!(document.id));
    source.insert(FIELD_VECTOR.to_string(), floats_value(embedding));
    source.insert(FIELD_DOC_ID.to_string(), json!(document.metadata.doc_id));
    source.insert(
        FIELD_CHUNK_ID.to_string(),
        json!(document.metadata.chunk_id),
    );
    source.insert(
        FIELD_CONTENT.to_string(),
        json!(content_field(&document.metadata)),
    );
    // Java only writes `payload` when the map is non-empty.
    if let Value::Object(map) = &document.metadata.payload {
        if !map.is_empty() {
            source.insert(
                FIELD_PAYLOAD.to_string(),
                json!(serde_json::to_string(&Value::Object(map.clone()))?),
            );
        }
    }
    Ok(Value::Object(source))
}

/// Builds the NDJSON body of `POST /_bulk` (action line + source line per document).
pub fn bulk_body(documents: &[Document], dimensions: usize, index: &str) -> Result<String> {
    let mut body = String::new();
    for document in documents {
        let action = json!({ "index": { "_index": index, "_id": document.id } });
        body.push_str(&serde_json::to_string(&action)?);
        body.push('\n');
        body.push_str(&serde_json::to_string(&document_source(
            document, dimensions,
        )?)?);
        body.push('\n');
    }
    Ok(body)
}

/// Parses a `POST /_bulk` response, mirroring Java's `response.errors()` check.
pub fn parse_bulk_response(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(body)?;
    if !value
        .get("errors")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(());
    }

    let mut failures: Vec<String> = Vec::new();
    if let Some(items) = value.get("items").and_then(Value::as_array) {
        for item in items {
            let Some(map) = item.as_object() else {
                continue;
            };
            for action in map.values() {
                if let Some(error) = action.get("error") {
                    let id = action
                        .get("_id")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>");
                    let reason = error
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    failures.push(format!("{id}: {reason}"));
                }
            }
        }
    }

    if failures.is_empty() {
        return Err(RagError::VectorStore(
            "Elasticsearch bulk indexing failed".to_string(),
        ));
    }
    Err(RagError::VectorStore(format!(
        "Elasticsearch bulk indexing failed: {}",
        failures.join("; ")
    )))
}

/// Request body of `POST /{index}/_search` (Java kNN request).
pub fn search_body(query: &SearchQuery, dimensions: usize) -> Result<Value> {
    if query.query_embedding.len() != dimensions {
        return Err(RagError::VectorStore(
            "Embedding dimension mismatch".to_string(),
        ));
    }
    let field = query.vector_name.as_deref().unwrap_or(FIELD_VECTOR);
    Ok(json!({
        "knn": {
            "field": field,
            "query_vector": floats_value(&query.query_embedding),
            "k": query.limit,
            "num_candidates": num_candidates(query.limit)
        },
        "size": query.limit,
        "min_score": query.score_threshold.unwrap_or(0.0)
    }))
}

/// Java `Math.max(limit * 2, 50)`.
pub fn num_candidates(limit: usize) -> usize {
    (limit * 2).max(50)
}

/// Parses a `POST /{index}/_search` response into scored documents.
///
pub fn parse_search_response(body: &str) -> Result<Vec<Document>> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(body)?;
    let hits = value
        .get("hits")
        .and_then(|hits| hits.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut documents = Vec::with_capacity(hits.len());
    for hit in hits {
        match hit_to_document(&hit) {
            Ok(document) => documents.push(document),
            Err(error) => {
                tracing::error!("Failed to map Elasticsearch hit to Document: {error}")
            }
        }
    }
    Ok(documents)
}

fn hit_to_document(hit: &Value) -> Result<Document> {
    let source = hit
        .get("_source")
        .and_then(Value::as_object)
        .ok_or_else(|| RagError::InvalidArgument("hit is missing '_source'".to_string()))?;

    let doc_id = source
        .get(FIELD_DOC_ID)
        .and_then(Value::as_str)
        .ok_or_else(|| RagError::InvalidArgument("source is missing 'doc_id'".to_string()))?;
    let chunk_id = source
        .get(FIELD_CHUNK_ID)
        .and_then(Value::as_str)
        .ok_or_else(|| RagError::InvalidArgument("source is missing 'chunk_id'".to_string()))?;
    let content = source
        .get(FIELD_CONTENT)
        .ok_or_else(|| RagError::InvalidArgument("source is missing 'content'".to_string()))?;

    let payload = match source.get(FIELD_PAYLOAD).and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) => Value::Object(Map::new()),
            Err(error) => {
                tracing::warn!("Failed to deserialize payload, using empty map: {error}");
                Value::Object(Map::new())
            }
        },
        _ => Value::Object(Map::new()),
    };

    let mut document = Document::new(DocumentMetadata::with_payload(
        content_block(content),
        doc_id,
        chunk_id,
        payload,
    ));
    if let Some(score) = hit.get("_score").and_then(Value::as_f64) {
        document.set_score(score);
    }
    if let Some(Value::Array(values)) = source.get(FIELD_VECTOR) {
        let embedding: Option<Vec<f64>> = values.iter().map(Value::as_f64).collect();
        if let Some(embedding) = embedding {
            document.set_embedding(embedding);
        }
    }
    Ok(document)
}

fn content_block(value: &Value) -> ContentBlock {
    match value {
        Value::String(text) => match serde_json::from_str::<ContentBlock>(text) {
            Ok(block) => block,
            Err(error) => {
                tracing::debug!("Failed to deserialize ContentBlock, creating TextBlock: {error}");
                ContentBlock::Text(TextBlock::new(text.clone()))
            }
        },
        other => match serde_json::from_value::<ContentBlock>(other.clone()) {
            Ok(block) => block,
            Err(_) => ContentBlock::Text(TextBlock::new(other.to_string())),
        },
    }
}

/// Whether a `DELETE /{index}/_doc/{id}` response reported `deleted`.
///
/// `not_found` (and any other result) maps to `false`, like Java comparing
/// `response.result().name()` with `Deleted`.
pub fn parse_delete_response(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value
        .get("result")
        .and_then(Value::as_str)
        .map(|result| result.eq_ignore_ascii_case("deleted"))
        .unwrap_or(false)
}

/// Elasticsearch vector store over the ES REST API.
pub struct ElasticsearchStore {
    config: ElasticsearchConfig,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
    closed: AtomicBool,
}

impl std::fmt::Debug for ElasticsearchStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElasticsearchStore")
            .field("index_name", &self.config.index_name)
            .field("base_url", &self.base_url)
            .field("dimensions", &self.config.dimensions)
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl ElasticsearchStore {
    /// Creates a store over an injected transport. No I/O happens here.
    pub fn new(config: ElasticsearchConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let base_url = normalize_base_url(config.url())?;
        Ok(Self {
            config,
            transport,
            base_url,
            closed: AtomicBool::new(false),
        })
    }

    /// Creates a store backed by the production [`ReqwestTransport`]. No I/O happens here.
    pub fn with_http(config: ElasticsearchConfig) -> Result<Self> {
        let transport = Arc::new(ReqwestTransport::new(&TransportOptions::default())?);
        Self::new(config, transport)
    }

    /// Creates a [`ReqwestTransport`]-backed store and ensures the index exists
    /// (Java's constructor behaviour).
    pub async fn connect(config: ElasticsearchConfig) -> Result<Self> {
        let transport = Arc::new(ReqwestTransport::new(&TransportOptions::default())?);
        Self::connect_with(config, transport).await
    }

    /// Creates a store over an injected transport and ensures the index exists.
    pub async fn connect_with(
        config: ElasticsearchConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let store = Self::new(config, transport)?;
        store.ensure_index().await?;
        Ok(store)
    }

    /// Configuration snapshot.
    pub fn config(&self) -> &ElasticsearchConfig {
        &self.config
    }

    /// Configured URL.
    pub fn url(&self) -> &str {
        self.config.url()
    }

    /// Index name.
    pub fn index_name(&self) -> &str {
        self.config.index_name()
    }

    /// Vector dimensions.
    pub fn dimensions(&self) -> usize {
        self.config.dimensions()
    }

    /// Resolved base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether [`close`](Self::close) has been called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Marks the store closed; every later operation fails with
    /// `VectorStore("ElasticsearchStore has been closed")`. Idempotent, like Java `close()`.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Ensures the index exists, creating it with the Java mapping on `404`.
    pub async fn ensure_index(&self) -> Result<()> {
        let response = self
            .execute(HttpMethod::Get, self.index_url(), None, None)
            .await?;
        if response.is_success() {
            return Ok(());
        }
        if response.status_code != 404 {
            return Err(map_http_error(response.status_code, &response.body));
        }
        self.create_index().await.map_err(|error| {
            RagError::VectorStore(format!(
                "Failed to ensure index exists: {}: {error}",
                self.config.index_name()
            ))
        })
    }

    /// Whether `GET /{index}` answers `200` (Java `indices().exists`).
    pub async fn index_exists(&self) -> Result<bool> {
        let response = self
            .execute(HttpMethod::Get, self.index_url(), None, None)
            .await?;
        if response.is_success() {
            return Ok(true);
        }
        if response.status_code == 404 {
            return Ok(false);
        }
        Err(map_http_error(response.status_code, &response.body))
    }

    async fn create_index(&self) -> Result<()> {
        let body = serde_json::to_string(&create_index_body(self.config.dimensions()))?;
        let response = self
            .execute(
                HttpMethod::Put,
                self.index_url(),
                Some(body),
                Some("application/json"),
            )
            .await?;
        if !response.is_success() {
            return Err(map_http_error(response.status_code, &response.body));
        }
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        if self.is_closed() {
            return Err(RagError::VectorStore(
                "ElasticsearchStore has been closed".to_string(),
            ));
        }
        Ok(())
    }

    fn index_url(&self) -> String {
        format!(
            "{}/{}",
            self.base_url,
            escape_path_segment(&self.config.index_name)
        )
    }

    fn bulk_url(&self) -> String {
        format!("{}{BULK_PATH}", self.base_url)
    }

    fn search_url(&self) -> String {
        format!("{}/_search", self.index_url())
    }

    fn doc_url(&self, id: &str) -> String {
        format!("{}/_doc/{}", self.index_url(), escape_path_segment(id))
    }

    async fn execute(
        &self,
        method: HttpMethod,
        url: String,
        body: Option<String>,
        content_type: Option<&str>,
    ) -> Result<HttpResponse> {
        let mut request = HttpRequest::new(method, url).with_header("Accept", "application/json");
        if let Some(content_type) = content_type {
            request = request.with_header("Content-Type", content_type);
        }
        if let Some(auth) = self.config.basic_auth() {
            request = request.with_header("Authorization", auth);
        }
        request.body = body;
        self.transport.execute(request).await
    }
}

#[async_trait]
impl VdbStore for ElasticsearchStore {
    async fn add(&self, documents: Vec<Document>) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        self.ensure_open()?;
        // Validate every document before the bulk request, like Java `executeBulkAdd`.
        let body = bulk_body(
            &documents,
            self.config.dimensions(),
            &self.config.index_name,
        )?;
        let response = self
            .execute(
                HttpMethod::Post,
                self.bulk_url(),
                Some(body),
                Some("application/x-ndjson"),
            )
            .await?;
        if !response.is_success() {
            return Err(map_http_error(response.status_code, &response.body));
        }
        parse_bulk_response(&response.body)
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<Document>> {
        if query.limit == 0 {
            return Err(RagError::InvalidArgument(
                "Limit must be positive".to_string(),
            ));
        }
        self.ensure_open()?;
        let body = serde_json::to_string(&search_body(query, self.config.dimensions())?)?;
        let response = self
            .execute(
                HttpMethod::Post,
                self.search_url(),
                Some(body),
                Some("application/json"),
            )
            .await?;
        if !response.is_success() {
            return Err(map_http_error(response.status_code, &response.body));
        }
        parse_search_response(&response.body)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        if id.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Document ID cannot be null or empty".to_string(),
            ));
        }
        self.ensure_open()?;
        let response = self
            .execute(HttpMethod::Delete, self.doc_url(id), None, None)
            .await?;
        if !response.is_success() {
            return Err(map_http_error(response.status_code, &response.body));
        }
        Ok(parse_delete_response(&response.body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

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

        fn raw_body_of(&self, index: usize) -> String {
            let requests = self.requests();
            requests[index].body.clone().expect("request has no body")
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

    fn config() -> ElasticsearchConfig {
        ElasticsearchConfig::builder()
            .url("http://localhost:9200")
            .index_name("knowledge")
            .dimensions(2)
            .build()
            .unwrap()
    }

    fn config_with_auth() -> ElasticsearchConfig {
        ElasticsearchConfig::builder()
            .url("http://localhost:9200/")
            .index_name("knowledge")
            .dimensions(2)
            .username("elastic")
            .password("changeme")
            .build()
            .unwrap()
    }

    fn doc(id: &str, embedding: Vec<f64>) -> Document {
        let mut document =
            Document::with_id(id, DocumentMetadata::text("hello", "doc-1", "chunk-1"));
        document.set_embedding(embedding);
        document
    }

    #[test]
    fn normalize_base_url_trims_and_requires_scheme() {
        assert_eq!(
            normalize_base_url(" http://localhost:9200/ ").unwrap(),
            "http://localhost:9200"
        );
        assert_eq!(
            normalize_base_url("https://es.internal:9243").unwrap(),
            "https://es.internal:9243"
        );
        assert_eq!(
            normalize_base_url("localhost:9200")
                .unwrap_err()
                .to_string(),
            "invalid argument: URL must start with 'http://' or 'https://': localhost:9200"
        );
    }

    #[test]
    fn builder_uses_the_java_validation_messages() {
        assert_eq!(
            ElasticsearchConfig::builder()
                .url("  ")
                .index_name("c")
                .dimensions(2)
                .build()
                .unwrap_err()
                .to_string(),
            "invalid argument: URL cannot be null or empty"
        );
        assert_eq!(
            ElasticsearchConfig::builder()
                .url("http://localhost:9200")
                .dimensions(2)
                .build()
                .unwrap_err()
                .to_string(),
            "invalid argument: Index name cannot be null or empty"
        );
        assert_eq!(
            ElasticsearchConfig::builder()
                .url("http://localhost:9200")
                .index_name("c")
                .build()
                .unwrap_err()
                .to_string(),
            "invalid argument: Dimensions must be positive"
        );
    }

    #[test]
    fn builder_defaults_match_java() {
        let config = ElasticsearchConfig::builder()
            .index_name("c")
            .dimensions(2)
            .build()
            .unwrap();
        assert_eq!(config.url(), DEFAULT_URL);
        assert!(!config.disable_ssl_verification());
        assert_eq!(config.basic_auth(), None);
    }

    #[test]
    fn basic_auth_requires_both_credentials() {
        assert_eq!(
            config_with_auth().basic_auth().as_deref(),
            // base64("elastic:changeme")
            Some("Basic ZWxhc3RpYzpjaGFuZ2VtZQ==")
        );
        let username_only = ElasticsearchConfig::builder()
            .index_name("c")
            .dimensions(2)
            .username("elastic")
            .build()
            .unwrap();
        assert_eq!(username_only.basic_auth(), None);
    }

    #[test]
    fn create_index_body_mirrors_the_java_mapping() {
        let body = create_index_body(8);
        let properties = &body["mappings"]["properties"];
        assert_eq!(properties["id"]["type"], "keyword");
        assert_eq!(properties["doc_id"]["type"], "keyword");
        assert_eq!(properties["chunk_id"]["type"], "keyword");
        assert_eq!(properties["content"]["type"], "text");
        assert_eq!(properties["content"]["index"], false);
        assert_eq!(properties["payload"]["index"], false);
        assert_eq!(properties["vector"]["type"], "dense_vector");
        assert_eq!(properties["vector"]["dims"], 8);
        assert_eq!(properties["vector"]["index"], true);
        assert_eq!(properties["vector"]["similarity"], "cosine");
    }

    #[tokio::test]
    async fn connect_creates_the_index_on_404() {
        let transport = MockTransport::new(vec![
            HttpResponse::new(404, r#"{"error":"index_not_found"}"#),
            HttpResponse::new(200, r#"{"acknowledged":true}"#),
        ]);
        let store = ElasticsearchStore::connect_with(config_with_auth(), transport.clone())
            .await
            .unwrap();
        assert_eq!(store.base_url(), "http://localhost:9200");

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, "http://localhost:9200/knowledge");
        assert_eq!(
            requests[0].header("Authorization"),
            Some("Basic ZWxhc3RpYzpjaGFuZ2VtZQ==")
        );
        assert_eq!(requests[1].method, HttpMethod::Put);
        assert_eq!(requests[1].url, "http://localhost:9200/knowledge");
        assert_eq!(requests[1].header("Content-Type"), Some("application/json"));
        assert_eq!(
            transport.body_of(1)["mappings"]["properties"]["vector"]["dims"],
            2
        );
    }

    #[tokio::test]
    async fn connect_skips_creation_when_the_index_exists() {
        let transport = MockTransport::new(vec![HttpResponse::new(200, "{}")]);
        ElasticsearchStore::connect_with(config(), transport.clone())
            .await
            .unwrap();
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn ensure_index_wraps_creation_failures() {
        let transport = MockTransport::new(vec![
            HttpResponse::new(404, "missing"),
            HttpResponse::new(400, r#"{"error":"mapper_parsing_exception"}"#),
        ]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        let error = store.ensure_index().await.unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with(
            "vector store error: Failed to ensure index exists: knowledge: API error [400]"
        ));
    }

    #[tokio::test]
    async fn index_exists_reports_the_probe_status() {
        let transport = MockTransport::new(vec![
            HttpResponse::new(200, "{}"),
            HttpResponse::new(404, "{}"),
            HttpResponse::new(503, "unavailable"),
        ]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        assert!(store.index_exists().await.unwrap());
        assert!(!store.index_exists().await.unwrap());
        assert_eq!(
            store.index_exists().await.unwrap_err().status_code(),
            Some(503)
        );
    }

    #[tokio::test]
    async fn add_posts_ndjson_bulk_operations() {
        let transport = MockTransport::new(vec![HttpResponse::new(
            200,
            r#"{"took":5,"errors":false,"items":[{"index":{"_id":"id-1","status":201}}]}"#,
        )]);
        let store = ElasticsearchStore::new(config_with_auth(), transport.clone()).unwrap();

        let mut document = doc("id-1", vec![0.5, 1.5]);
        document.metadata.payload = json!({ "source": "unit" });
        store.add(vec![document]).await.unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "http://localhost:9200/_bulk");
        assert_eq!(
            requests[0].header("Content-Type"),
            Some("application/x-ndjson")
        );

        let raw = transport.raw_body_of(0);
        assert!(raw.ends_with('\n'));
        let lines: Vec<&str> = raw.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 2);
        let action: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(action["index"]["_index"], "knowledge");
        assert_eq!(action["index"]["_id"], "id-1");
        let source: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(source["id"], "id-1");
        assert_eq!(source["vector"], json!([0.5, 1.5]));
        assert_eq!(source["doc_id"], "doc-1");
        assert_eq!(source["chunk_id"], "chunk-1");
        let content: Value = serde_json::from_str(source["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["text"], "hello");
        assert_eq!(source["payload"], json!("{\"source\":\"unit\"}"));
    }

    #[tokio::test]
    async fn add_omits_the_payload_field_when_empty() {
        let transport = MockTransport::new(vec![HttpResponse::new(
            200,
            r#"{"errors":false,"items":[]}"#,
        )]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        store.add(vec![doc("id-1", vec![0.0, 0.0])]).await.unwrap();
        let raw = transport.raw_body_of(0);
        let source: Value = serde_json::from_str(raw.lines().nth(1).unwrap()).unwrap();
        assert!(source.get("payload").is_none());
    }

    #[tokio::test]
    async fn add_empty_documents_is_a_noop() {
        let transport = MockTransport::new(vec![]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        store.add(Vec::new()).await.unwrap();
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn add_validates_embedding_presence_and_dimensions() {
        let transport = MockTransport::new(vec![]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();

        let error = store
            .add(vec![Document::new(DocumentMetadata::text("a", "d", "c"))])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: Document must have embedding set"
        );
        let error = store
            .add(vec![doc("id", vec![0.0, 0.0, 0.0])])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "vector store error: Embedding dimension mismatch: expected 2, got 3"
        );
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn add_surfaces_bulk_item_errors() {
        let transport = MockTransport::new(vec![HttpResponse::new(
            200,
            json!({
                "errors": true,
                "items": [{"index": {"_id": "id-1", "status": 400, "error": {"reason": "failed to parse field [vector]"}}}]
            })
            .to_string(),
        )]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        let error = store
            .add(vec![doc("id-1", vec![0.0, 0.0])])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "vector store error: Elasticsearch bulk indexing failed: id-1: failed to parse field [vector]"
        );
    }

    #[tokio::test]
    async fn search_builds_the_knn_request_and_maps_hits() {
        let transport = MockTransport::new(vec![HttpResponse::new(
            200,
            json!({
                "hits": {
                    "total": { "value": 1 },
                    "hits": [{
                        "_id": "id-1",
                        "_score": 0.91,
                        "_source": {
                            "id": "id-1",
                            "doc_id": "doc-1",
                            "chunk_id": "chunk-1",
                            "content": "{\"type\":\"text\",\"text\":\"hello\"}",
                            "payload": "{\"source\":\"unit\"}",
                            "vector": [0.5, 0.5]
                        }
                    }]
                }
            })
            .to_string(),
        )]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        let documents = store
            .search(&SearchQuery::new(vec![0.5, 0.25], 3))
            .await
            .unwrap();

        let requests = transport.requests();
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, "http://localhost:9200/knowledge/_search");
        let body = transport.body_of(0);
        assert_eq!(body["knn"]["field"], "vector");
        assert_eq!(body["knn"]["query_vector"], json!([0.5, 0.25]));
        assert_eq!(body["knn"]["k"], 3);
        assert_eq!(body["knn"]["num_candidates"], 50);
        assert_eq!(body["size"], 3);
        assert_eq!(body["min_score"], 0.0);

        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].metadata.doc_id, "doc-1");
        assert_eq!(documents[0].metadata.content_text(), "hello");
        assert_eq!(documents[0].payload_value("source"), Some(&json!("unit")));
        assert_eq!(documents[0].score, Some(0.91));
        assert_eq!(documents[0].embedding, Some(vec![0.5, 0.5]));
    }

    #[tokio::test]
    async fn search_uses_the_vector_name_and_threshold() {
        let transport = MockTransport::new(vec![HttpResponse::new(200, r#"{"hits":{"hits":[]}}"#)]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        let query = SearchQuery::new(vec![0.0, 0.0], 40)
            .with_vector_name("image_knn_vector")
            .with_score_threshold(0.75);
        assert!(store.search(&query).await.unwrap().is_empty());

        let body = transport.body_of(0);
        assert_eq!(body["knn"]["field"], "image_knn_vector");
        assert_eq!(body["knn"]["num_candidates"], 80);
        assert_eq!(body["min_score"], 0.75);
    }

    #[test]
    fn num_candidates_matches_java() {
        assert_eq!(num_candidates(1), 50);
        assert_eq!(num_candidates(25), 50);
        assert_eq!(num_candidates(26), 52);
    }

    #[test]
    fn search_response_tolerates_partial_hits() {
        let body = json!({
            "hits": {
                "hits": [
                    { "_score": 0.5 },
                    { "_score": 0.4, "_source": { "chunk_id": "0", "content": "hi" } },
                    { "_source": { "doc_id": "d", "chunk_id": "0", "content": "plain text" } }
                ]
            }
        })
        .to_string();
        let documents = parse_search_response(&body).unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].metadata.content_text(), "plain text");
        assert_eq!(documents[0].score, None);
        assert!(documents[0].payload().is_empty());
        assert_eq!(documents[0].embedding, None);
    }

    #[test]
    fn search_response_maps_a_broken_payload_to_an_empty_map() {
        let body = json!({
            "hits": { "hits": [{
                "_score": 0.2,
                "_source": { "doc_id": "d", "chunk_id": "0", "content": "hi", "payload": "not json" }
            }]}
        })
        .to_string();
        let documents = parse_search_response(&body).unwrap();
        assert!(documents[0].payload().is_empty());
    }

    #[test]
    fn empty_search_bodies_yield_no_documents() {
        assert!(parse_search_response("   ").unwrap().is_empty());
        assert!(parse_search_response(r#"{"hits":{}}"#).unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_validates_dimensions_and_limit() {
        let transport = MockTransport::new(vec![]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();

        let error = store
            .search(&SearchQuery::new(vec![0.0], 1))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "vector store error: Embedding dimension mismatch"
        );
        let error = store
            .search(&SearchQuery::new(vec![0.0, 0.0], 0))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: Limit must be positive"
        );
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn http_errors_keep_their_status_codes() {
        let transport = MockTransport::new(vec![
            HttpResponse::new(500, r#"{"error":"boom"}"#),
            HttpResponse::new(401, "unauthorized"),
        ]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();

        let error = store
            .search(&SearchQuery::new(vec![0.0, 0.0], 1))
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), Some(500));
        assert!(error.is_server_error());

        let error = store.delete("id-1").await.unwrap_err();
        assert!(matches!(
            error,
            RagError::Auth {
                status_code: 401,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn delete_uses_the_doc_endpoint() {
        let transport = MockTransport::new(vec![
            HttpResponse::new(200, r#"{"result":"deleted","_id":"id-1"}"#),
            HttpResponse::new(200, r#"{"result":"not_found","_id":"id-2"}"#),
        ]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        assert!(store.delete("id-1").await.unwrap());
        assert!(!store.delete("id-2").await.unwrap());

        let requests = transport.requests();
        assert_eq!(requests[0].method, HttpMethod::Delete);
        assert_eq!(requests[0].url, "http://localhost:9200/knowledge/_doc/id-1");
    }

    #[tokio::test]
    async fn delete_rejects_a_blank_id() {
        let transport = MockTransport::new(vec![]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        let error = store.delete("  ").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: Document ID cannot be null or empty"
        );
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(escape_path_segment("a/b c"), "a%2Fb%20c");
        assert_eq!(
            escape_path_segment("11111111-2222-3333-4444-555555555555"),
            "11111111-2222-3333-4444-555555555555"
        );
        let store = ElasticsearchStore::new(config(), MockTransport::new(vec![])).unwrap();
        assert_eq!(
            store.doc_url("a/b"),
            "http://localhost:9200/knowledge/_doc/a%2Fb"
        );
    }

    #[test]
    fn delete_response_parsing_is_case_insensitive_and_tolerant() {
        assert!(parse_delete_response(r#"{"result":"Deleted"}"#));
        assert!(parse_delete_response(r#"{"result":"deleted"}"#));
        assert!(!parse_delete_response(r#"{"result":"not_found"}"#));
        assert!(!parse_delete_response(r#"{"result":"updated"}"#));
        assert!(!parse_delete_response("not json"));
    }

    #[tokio::test]
    async fn bulk_errors_without_items_use_the_java_message() {
        let transport = MockTransport::new(vec![HttpResponse::new(200, r#"{"errors":true}"#)]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        let error = store
            .add(vec![doc("id-1", vec![0.0, 0.0])])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "vector store error: Elasticsearch bulk indexing failed"
        );
    }

    #[tokio::test]
    async fn a_closed_store_rejects_every_operation() {
        let transport = MockTransport::new(vec![]);
        let store = ElasticsearchStore::new(config(), transport.clone()).unwrap();
        store.close();
        store.close();
        assert!(store.is_closed());

        assert_eq!(
            store
                .add(vec![doc("id-1", vec![0.0, 0.0])])
                .await
                .unwrap_err()
                .to_string(),
            "vector store error: ElasticsearchStore has been closed"
        );
        assert_eq!(
            store
                .search(&SearchQuery::new(vec![0.0, 0.0], 1))
                .await
                .unwrap_err()
                .to_string(),
            "vector store error: ElasticsearchStore has been closed"
        );
        assert_eq!(
            store.delete("id-1").await.unwrap_err().to_string(),
            "vector store error: ElasticsearchStore has been closed"
        );
        assert!(transport.requests().is_empty());
    }
}
