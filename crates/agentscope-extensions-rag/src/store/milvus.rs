//! Milvus vector store.
//!
//! Ports `io.agentscope.core.rag.store.MilvusStore` (919 lines). The Java class talks gRPC
//! through the `io.milvus:milvus-sdk-java` SDK (v2); the Rust port talks the **Milvus REST
//! v2** API (`/v2/vectordb/...`) through the shared [`crate::transport`] seam, so the whole
//! store is exercisable offline with a mock transport (same shape as [`crate::store::qdrant`]).
//!
//! Mapping notes (Java → Rust):
//!
//! * `hasCollection(HasCollectionReq)` → `POST /v2/vectordb/collections/has`, reading
//!   `data.has`.
//! * `createCollection(CreateCollectionReq)` → `POST /v2/vectordb/collections/create` with the
//!   same schema/fields/index (`id` VarChar(64) primary key, `vector` FloatVector(dim),
//!   `doc_id` VarChar(256), `chunk_id` VarChar, `content` VarChar(65535), `payload` JSON,
//!   dynamic field enabled, `AUTOINDEX` + configured metric).
//! * `listDatabases()` / `createDatabase(CreateDatabaseReq)` → `POST
//!   /v2/vectordb/databases/list` / `.../create` (only when `databaseName != "default"`, like
//!   Java).
//! * `insert(InsertReq)` → `POST /v2/vectordb/entities/insert`; [`MilvusStore::upsert`] adds the
//!   REST-only `.../entities/upsert` counterpart (Java has no upsert path).
//! * `search(SearchReq)` → `POST /v2/vectordb/entities/search` with `data` (single query
//!   vector), `annsField`, `limit`, `outputFields` and `searchParams.metricType`.
//!   `SearchQuery::vector_name` overrides `annsField` (Java always uses the single vector
//!   field); `score_threshold` is applied client-side after `normalizeScore`, exactly like
//!   Java (`L2` → `1 / (1 + distance)`).
//! * `delete(DeleteReq)` → `POST /v2/vectordb/entities/delete` with the same `doc_id in ["…"]`
//!   filter, returning `deleteCount > 0`.
//! * Auth: Java `ConnectConfig.token(token)` or `username:password` → `Authorization: Bearer
//!   <token>`; `token` wins when both are set, like Java.
//!
//! Deliberate differences:
//!
//! * REST instead of gRPC, therefore the `account`/`hash`/`consistency` knobs of the Java
//!   `ConnectConfig` have no counterpart. The single hard requirement is a full HTTP(S) URI,
//!   which is validated up-front (Java fails later, at connect time).
//! * `databaseProperties` is carried into `databases/create`; empty properties are omitted.
//! * Milvus reports API-level failures with HTTP 200 + a non-zero `code`. Those become
//!   [`RagError::VectorStore`] (`Milvus error {code}: {message}`) while HTTP failures keep the
//!   status-bearing [`RagError::Api`] / [`RagError::Auth`] from [`map_http_error`].
//! * `delete` treats a response without `deleteCount` as accepted (`true`): the REST delete is
//!   asynchronous and may not report a count, whereas Java reads `DeleteResp.getDeleteCnt()`.
//! * Lazy creation is explicit: [`MilvusStore::connect_with`] (or [`MilvusStore::initialize`])
//!   performs Java's constructor-time database/collection check; [`MilvusStore::new`] does no
//!   I/O.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::message::{ContentBlock, TextBlock};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::error::{RagError, Result};
use crate::model::{Document, DocumentMetadata};
use crate::store::{SearchQuery, VdbStore};
use crate::transport::{
    map_http_error, HttpMethod, HttpRequest, HttpTransport, ReqwestTransport, TransportOptions,
};

/// Default database name (Java `DEFAULT_DATABASE_NAME`).
pub const DEFAULT_DATABASE_NAME: &str = "default";
/// Default connect timeout in milliseconds (Java `DEFAULT_CONNECT_TIMEOUT_MS`).
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 30_000;
/// Default Milvus gRPC/REST port, used for documentation and diagnostics.
pub const DEFAULT_MILVUS_PORT: u16 = 19530;

/// Primary-key column (`id`).
pub const FIELD_ID: &str = "id";
/// Vector column (`vector`).
pub const FIELD_VECTOR: &str = "vector";
/// Owning document id column (`doc_id`).
pub const FIELD_DOC_ID: &str = "doc_id";
/// Chunk id column (`chunk_id`).
pub const FIELD_CHUNK_ID: &str = "chunk_id";
/// Serialized `ContentBlock` column (`content`).
pub const FIELD_CONTENT: &str = "content";
/// Custom payload column (`payload`).
pub const FIELD_PAYLOAD: &str = "payload";

/// REST v2 path used by `hasCollection`.
pub const HAS_COLLECTION_PATH: &str = "/v2/vectordb/collections/has";
/// REST v2 path used by `createCollection`.
pub const CREATE_COLLECTION_PATH: &str = "/v2/vectordb/collections/create";
/// REST v2 path used by `insert`.
pub const INSERT_PATH: &str = "/v2/vectordb/entities/insert";
/// REST v2 path used by the REST-only upsert counterpart.
pub const UPSERT_PATH: &str = "/v2/vectordb/entities/upsert";
/// REST v2 path used by `search`.
pub const SEARCH_PATH: &str = "/v2/vectordb/entities/search";
/// REST v2 path used by `delete`.
pub const DELETE_PATH: &str = "/v2/vectordb/entities/delete";
/// REST v2 path used by `listDatabases`.
pub const LIST_DATABASES_PATH: &str = "/v2/vectordb/databases/list";
/// REST v2 path used by `createDatabase`.
pub const CREATE_DATABASE_PATH: &str = "/v2/vectordb/databases/create";

/// VarChar length of the `id` column (Java `maxLength(64)`).
pub const ID_MAX_LENGTH: u32 = 64;
/// VarChar length of the `doc_id` column (Java `maxLength(256)`).
pub const DOC_ID_MAX_LENGTH: u32 = 256;
/// VarChar length of the `chunk_id`/`content` columns (Milvus default, Java `maxLength(65535)`).
pub const VARCHAR_MAX_LENGTH: u32 = 65_535;

/// Metric used by the vector index, mirroring `IndexParam.MetricType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MilvusMetricType {
    /// Cosine similarity (Java default). Higher is better.
    #[default]
    Cosine,
    /// Euclidean (L2) distance. Lower is better, normalised by [`Self::normalize_score`].
    L2,
    /// Inner product. Higher is better.
    Ip,
}

impl MilvusMetricType {
    /// Wire name accepted by `metricType`.
    pub fn as_wire(self) -> &'static str {
        match self {
            MilvusMetricType::Cosine => "COSINE",
            MilvusMetricType::L2 => "L2",
            MilvusMetricType::Ip => "IP",
        }
    }

    /// Converts a raw Milvus score/distance into a higher-is-better score.
    ///
    /// Exactly Java `normalizeScore`: only `L2` is mapped (`1 / (1 + distance)`), the other
    /// metrics already point the right way and are returned unchanged.
    pub fn normalize_score(self, raw: f64) -> f64 {
        match self {
            MilvusMetricType::L2 => 1.0 / (1.0 + raw),
            MilvusMetricType::Cosine | MilvusMetricType::Ip => raw,
        }
    }
}

/// Connection / collection configuration for [`MilvusStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MilvusConfig {
    uri: String,
    collection_name: String,
    database_name: String,
    dimensions: usize,
    metric_type: MilvusMetricType,
    token: Option<String>,
    username: Option<String>,
    password: Option<String>,
    connect_timeout_ms: u64,
    database_properties: BTreeMap<String, String>,
}

impl MilvusConfig {
    /// Creates a builder.
    pub fn builder() -> MilvusConfigBuilder {
        MilvusConfigBuilder::default()
    }

    /// Configured Milvus URI (`http(s)://host:port`).
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Configured collection name.
    pub fn collection_name(&self) -> &str {
        &self.collection_name
    }

    /// Configured database name (never empty, defaults to `default`).
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// Configured vector dimensions.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Metric used for the vector index and searches.
    pub fn metric_type(&self) -> MilvusMetricType {
        self.metric_type
    }

    /// Explicit token, when configured.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Configured username, when set.
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// Configured password, when set.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// Configured connect timeout in milliseconds (`0` disables it).
    pub fn connect_timeout_ms(&self) -> u64 {
        self.connect_timeout_ms
    }

    /// Properties passed to `databases/create`.
    pub fn database_properties(&self) -> &BTreeMap<String, String> {
        &self.database_properties
    }

    /// Resolved bearer token, exactly like Java's `ConnectConfig` setup.
    ///
    /// A non-blank `token` wins; otherwise a non-blank `username` yields
    /// `username:password` (an absent password becomes an empty string, like Java). Blank
    /// values are ignored so anonymous local instances work without an `Authorization` header.
    pub fn bearer_token(&self) -> Option<String> {
        if let Some(token) = self
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return Some(token.to_string());
        }
        let username = self
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())?;
        Some(format!(
            "{username}:{}",
            self.password.as_deref().unwrap_or("")
        ))
    }

    /// Transport options derived from `connect_timeout_ms`.
    ///
    /// The Java SDK applies the timeout to the gRPC channel; the REST port has no channel, so
    /// the same value becomes the connect and read timeout of the HTTP client.
    pub fn transport_options(&self) -> TransportOptions {
        let mut options = TransportOptions::default();
        if self.connect_timeout_ms > 0 {
            let timeout = Duration::from_millis(self.connect_timeout_ms);
            options.connect_timeout = timeout;
            options.read_timeout = timeout;
        }
        options
    }
}

/// Builder for [`MilvusConfig`], mirroring the Java `MilvusStore.Builder`.
#[derive(Debug, Clone, Default)]
pub struct MilvusConfigBuilder {
    uri: Option<String>,
    collection_name: Option<String>,
    database_name: Option<String>,
    dimensions: usize,
    metric_type: MilvusMetricType,
    token: Option<String>,
    username: Option<String>,
    password: Option<String>,
    connect_timeout_ms: Option<u64>,
    database_properties: BTreeMap<String, String>,
}

impl MilvusConfigBuilder {
    /// Sets the Milvus URI (`http://localhost:19530`).
    pub fn uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = Some(uri.into());
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

    /// Sets the authentication token (`username:password`).
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Sets the username (alternative to [`Self::token`]).
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Sets the password, used together with [`Self::username`].
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Sets the connect timeout in milliseconds (`0` disables it).
    pub fn connect_timeout_ms(mut self, connect_timeout_ms: u64) -> Self {
        self.connect_timeout_ms = Some(connect_timeout_ms);
        self
    }

    /// Sets the metric type (`COSINE` by default, like Java).
    pub fn metric_type(mut self, metric_type: MilvusMetricType) -> Self {
        self.metric_type = metric_type;
        self
    }

    /// Sets the database name (`default` by default).
    pub fn database_name(mut self, database_name: impl Into<String>) -> Self {
        self.database_name = Some(database_name.into());
        self
    }

    /// Replaces the properties passed to `databases/create`.
    pub fn database_properties(
        mut self,
        properties: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.database_properties = properties.into_iter().collect();
        self
    }

    /// Validates and builds the configuration, using the Java error messages.
    pub fn build(self) -> Result<MilvusConfig> {
        let uri = self.uri.unwrap_or_default();
        if uri.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "URI cannot be null or empty".to_string(),
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
        let database_name = match self.database_name {
            Some(name) if !name.trim().is_empty() => name,
            _ => {
                if self.database_name.is_some() {
                    return Err(RagError::InvalidArgument(
                        "Database name cannot be null or empty".to_string(),
                    ));
                }
                DEFAULT_DATABASE_NAME.to_string()
            }
        };
        parse_milvus_uri(&uri)?;

        Ok(MilvusConfig {
            uri,
            collection_name,
            database_name,
            dimensions: self.dimensions,
            metric_type: self.metric_type,
            token: self.token,
            username: self.username,
            password: self.password,
            connect_timeout_ms: self
                .connect_timeout_ms
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
            database_properties: self.database_properties,
        })
    }
}

/// Normalises a Milvus URI to a REST base URL.
///
/// Java requires a full HTTP URL (`ConnectConfig.uri(...)`); the REST port rejects bare
/// `host:port` early so misconfiguration is diagnosable before the first request.
pub fn parse_milvus_uri(uri: &str) -> Result<String> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return Err(RagError::InvalidArgument(
            "URI cannot be null or empty".to_string(),
        ));
    }
    let lowered = trimmed.to_ascii_lowercase();
    if !lowered.starts_with("http://") && !lowered.starts_with("https://") {
        return Err(RagError::InvalidArgument(format!(
            "URI must start with 'http://' or 'https://': {uri}"
        )));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

fn floats_value(values: &[f64]) -> Value {
    Value::Array(values.iter().map(|v| json!(*v as f32 as f64)).collect())
}

/// Escapes a value embedded in a Milvus filter expression string literal.
///
/// Java interpolates `docId` verbatim (`doc_id in ["…"]`); the Rust port escapes `\` and `"`
/// so an id can never break out of the literal.
pub fn escape_filter_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `doc_id in ["<id>"]` filter used by delete (Java `DeleteReq.filter`).
pub fn delete_filter(doc_id: &str) -> String {
    format!("{FIELD_DOC_ID} in [\"{}\"]", escape_filter_string(doc_id))
}

/// Request body of `POST /v2/vectordb/collections/has`.
pub fn has_collection_body(database: &str, collection: &str) -> Value {
    json!({ "dbName": database, "collectionName": collection })
}

/// Request body of `POST /v2/vectordb/databases/list`.
pub fn list_databases_body() -> Value {
    json!({})
}

/// Request body of `POST /v2/vectordb/databases/create` (Java `CreateDatabaseReq`).
pub fn create_database_body(database: &str, properties: &BTreeMap<String, String>) -> Value {
    let mut body = Map::new();
    body.insert("dbName".to_string(), json!(database));
    if !properties.is_empty() {
        body.insert("properties".to_string(), json!(properties));
    }
    Value::Object(body)
}

/// Request body of `POST /v2/vectordb/collections/create` (Java `CreateCollectionReq`).
///
/// Faithful to the Java schema: dynamic fields on, `id` VarChar(64) primary key without
/// auto-id, `vector` FloatVector(dim), `doc_id` VarChar(256), `chunk_id` VarChar,
/// `content` VarChar(65535), `payload` JSON, plus an `AUTOINDEX` on the vector field using the
/// configured metric.
pub fn create_collection_body(
    database: &str,
    collection: &str,
    dimensions: usize,
    metric: MilvusMetricType,
) -> Value {
    let fields = json!([
        {
            "fieldName": FIELD_ID,
            "dataType": "VarChar",
            "isPrimaryKey": true,
            "elementTypeParams": { "max_length": ID_MAX_LENGTH.to_string() }
        },
        {
            "fieldName": FIELD_VECTOR,
            "dataType": "FloatVector",
            "elementTypeParams": { "dim": dimensions.to_string() }
        },
        {
            "fieldName": FIELD_DOC_ID,
            "dataType": "VarChar",
            "elementTypeParams": { "max_length": DOC_ID_MAX_LENGTH.to_string() }
        },
        {
            "fieldName": FIELD_CHUNK_ID,
            "dataType": "VarChar",
            "elementTypeParams": { "max_length": VARCHAR_MAX_LENGTH.to_string() }
        },
        {
            "fieldName": FIELD_CONTENT,
            "dataType": "VarChar",
            "elementTypeParams": { "max_length": VARCHAR_MAX_LENGTH.to_string() }
        },
        { "fieldName": FIELD_PAYLOAD, "dataType": "JSON" }
    ]);

    json!({
        "dbName": database,
        "collectionName": collection,
        "schema": {
            "autoID": false,
            "enableDynamicField": true,
            "fields": fields
        },
        "indexParams": [{
            "fieldName": FIELD_VECTOR,
            "indexName": FIELD_VECTOR,
            "indexType": "AUTOINDEX",
            "metricType": metric.as_wire()
        }]
    })
}

/// Serialized `content` column: JSON text of the [`ContentBlock`].
///
/// Java `JsonUtils.getJsonCodec().toJson(metadata.getContent())` with a `getContentText()`
/// fallback when serialization fails.
pub fn content_column(metadata: &DocumentMetadata) -> String {
    match serde_json::to_string(&metadata.content) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!("Failed to serialize content, using text representation: {error}");
            metadata.content_text()
        }
    }
}

/// One entity row of an insert/upsert request (Java `row` JsonObject).
pub fn document_row(document: &Document, dimensions: usize) -> Result<Value> {
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

    let payload = match &document.metadata.payload {
        Value::Object(map) => Value::Object(map.clone()),
        _ => Value::Object(Map::new()),
    };

    let mut row = Map::new();
    row.insert(FIELD_ID.to_string(), json!(document.id));
    row.insert(FIELD_VECTOR.to_string(), floats_value(embedding));
    row.insert(FIELD_DOC_ID.to_string(), json!(document.metadata.doc_id));
    row.insert(
        FIELD_CHUNK_ID.to_string(),
        json!(document.metadata.chunk_id),
    );
    row.insert(
        FIELD_CONTENT.to_string(),
        json!(content_column(&document.metadata)),
    );
    row.insert(FIELD_PAYLOAD.to_string(), payload);
    Ok(Value::Object(row))
}

/// Request body of `POST /v2/vectordb/entities/insert` (and `.../upsert`).
pub fn insert_body(database: &str, collection: &str, rows: Vec<Value>) -> Value {
    json!({ "dbName": database, "collectionName": collection, "data": rows })
}

/// Request body of `POST /v2/vectordb/entities/search` (Java `SearchReq`).
pub fn search_body(
    database: &str,
    collection: &str,
    query: &SearchQuery,
    metric: MilvusMetricType,
) -> Value {
    let anns_field = query.vector_name.as_deref().unwrap_or(FIELD_VECTOR);
    json!({
        "dbName": database,
        "collectionName": collection,
        "data": [floats_value(&query.query_embedding)],
        "annsField": anns_field,
        "limit": query.limit,
        "outputFields": [FIELD_ID, FIELD_DOC_ID, FIELD_CHUNK_ID, FIELD_CONTENT, FIELD_PAYLOAD],
        "searchParams": {
            "metricType": metric.as_wire(),
            "params": {}
        }
    })
}

/// Request body of `POST /v2/vectordb/entities/delete` (Java `DeleteReq`).
pub fn delete_body(database: &str, collection: &str, doc_id: &str) -> Value {
    json!({
        "dbName": database,
        "collectionName": collection,
        "filter": delete_filter(doc_id)
    })
}

/// Splits the Milvus REST envelope (`{"code":0,"data":…}`) and returns `data`.
///
/// A non-zero `code` is an API-level failure even when the HTTP status is `200`; it maps to
/// [`RagError::VectorStore`] with Milvus' own message.
pub fn parse_envelope(body: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(body)?;
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(RagError::VectorStore(format!(
            "Milvus error {code}: {message}"
        )));
    }
    Ok(value.get("data").cloned().unwrap_or(Value::Null))
}

/// Extracts the `data.has` flag of a `collections/has` response.
pub fn parse_has_collection(body: &str) -> Result<bool> {
    let data = parse_envelope(body)?;
    Ok(data.get("has").and_then(Value::as_bool).unwrap_or(false))
}

/// Extracts the database names of a `databases/list` response.
pub fn parse_database_list(body: &str) -> Result<Vec<String>> {
    let data = parse_envelope(body)?;
    Ok(match data {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    })
}

/// Extracts `data.deleteCount` from a `entities/delete` response.
///
/// `None` means Milvus accepted the (asynchronous) delete without reporting a count.
pub fn parse_delete_count(body: &str) -> Result<Option<u64>> {
    Ok(count_from_data(&parse_envelope(body)?))
}

fn count_from_data(data: &Value) -> Option<u64> {
    for key in ["deleteCount", "deleteCnt"] {
        if let Some(count) = data.get(key).and_then(Value::as_u64) {
            return Some(count);
        }
    }
    None
}

/// Parses a `entities/search` response into scored documents.
///
/// Java reads `searchResults.get(0)` and skips every result it cannot reconstruct
/// (`log.warn` + `continue`); the same tolerant behaviour is kept here. The `score_threshold`
/// is applied after [`MilvusMetricType::normalize_score`], exactly like Java.
pub fn parse_search_response(
    body: &str,
    metric: MilvusMetricType,
    score_threshold: Option<f64>,
    vector_name: Option<&str>,
) -> Result<Vec<Document>> {
    search_data_to_documents(&parse_envelope(body)?, metric, score_threshold, vector_name)
}

/// Converts the `data` payload of a search response into scored documents.
pub fn search_data_to_documents(
    data: &Value,
    metric: MilvusMetricType,
    score_threshold: Option<f64>,
    vector_name: Option<&str>,
) -> Result<Vec<Document>> {
    let items = match data {
        Value::Array(items) if items.first().is_some_and(Value::is_array) => {
            // Multiple query vectors: only the first result set matters (Java `get(0)`).
            match items.first() {
                Some(Value::Array(inner)) => inner.clone(),
                _ => Vec::new(),
            }
        }
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    };

    let mut documents = Vec::with_capacity(items.len());
    for item in items {
        let raw_score = item.get("distance").and_then(Value::as_f64).unwrap_or(0.0);
        let score = metric.normalize_score(raw_score);
        if score_threshold.is_some_and(|threshold| score < threshold) {
            continue;
        }
        match search_item_to_document(&item, score, vector_name) {
            Ok(document) => documents.push(document),
            Err(error) => {
                tracing::warn!("Failed to reconstruct document from search result: {error}")
            }
        }
    }
    Ok(documents)
}

fn search_item_to_document(
    item: &Value,
    score: f64,
    vector_name: Option<&str>,
) -> Result<Document> {
    let entity = item
        .get("entity")
        .and_then(Value::as_object)
        .ok_or_else(|| RagError::InvalidArgument("Search result missing 'entity'".to_string()))?;

    let doc_id = entity
        .get(FIELD_DOC_ID)
        .and_then(Value::as_str)
        .ok_or_else(|| RagError::InvalidArgument("Entity missing 'doc_id' field".to_string()))?;
    let chunk_id = entity
        .get(FIELD_CHUNK_ID)
        .and_then(Value::as_str)
        .ok_or_else(|| RagError::InvalidArgument("Entity missing 'chunk_id' field".to_string()))?;
    let content_raw = entity
        .get(FIELD_CONTENT)
        .ok_or_else(|| RagError::InvalidArgument("Entity missing 'content' field".to_string()))?;

    let metadata = DocumentMetadata::with_payload(
        entity_content(content_raw),
        doc_id,
        chunk_id,
        entity_payload(entity.get(FIELD_PAYLOAD)),
    );
    let mut document = Document::new(metadata);
    document.set_score(score);
    if let Some(name) = vector_name {
        document.vector_name = Some(name.to_string());
    }
    Ok(document)
}

fn entity_content(value: &Value) -> ContentBlock {
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

fn entity_payload(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(Value::String(text)) if !text.trim().is_empty() => {
            match serde_json::from_str::<Value>(text) {
                Ok(Value::Object(map)) => Value::Object(map),
                Ok(_) => Value::Object(Map::new()),
                Err(error) => {
                    tracing::warn!("Failed to deserialize payload, using empty map: {error}");
                    Value::Object(Map::new())
                }
            }
        }
        _ => Value::Object(Map::new()),
    }
}

/// Milvus vector store over the Milvus REST v2 API.
pub struct MilvusStore {
    config: MilvusConfig,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
    closed: AtomicBool,
}

impl std::fmt::Debug for MilvusStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MilvusStore")
            .field("collection_name", &self.config.collection_name)
            .field("database_name", &self.config.database_name)
            .field("base_url", &self.base_url)
            .field("dimensions", &self.config.dimensions)
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl MilvusStore {
    /// Creates a store over an injected transport. No I/O happens here.
    pub fn new(config: MilvusConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let base_url = parse_milvus_uri(config.uri())?;
        Ok(Self {
            config,
            transport,
            base_url,
            closed: AtomicBool::new(false),
        })
    }

    /// Creates a store backed by the production [`ReqwestTransport`]. No I/O happens here.
    pub fn with_http(config: MilvusConfig) -> Result<Self> {
        let transport = Arc::new(ReqwestTransport::new(&config.transport_options())?);
        Self::new(config, transport)
    }

    /// Creates a [`ReqwestTransport`]-backed store and ensures the database/collection exist
    /// (Java's constructor behaviour).
    pub async fn connect(config: MilvusConfig) -> Result<Self> {
        let transport = Arc::new(ReqwestTransport::new(&config.transport_options())?);
        Self::connect_with(config, transport).await
    }

    /// Creates a store over an injected transport and ensures the database/collection exist.
    pub async fn connect_with(
        config: MilvusConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let store = Self::new(config, transport)?;
        store.initialize().await?;
        Ok(store)
    }

    /// Creates the database (when not `default`) and the collection, like the Java constructor.
    pub async fn initialize(&self) -> Result<()> {
        self.ensure_database().await?;
        self.ensure_collection().await?;
        Ok(())
    }

    /// Configuration snapshot.
    pub fn config(&self) -> &MilvusConfig {
        &self.config
    }

    /// Configured URI.
    pub fn uri(&self) -> &str {
        self.config.uri()
    }

    /// Collection name.
    pub fn collection_name(&self) -> &str {
        self.config.collection_name()
    }

    /// Database name.
    pub fn database_name(&self) -> &str {
        self.config.database_name()
    }

    /// Vector dimensions.
    pub fn dimensions(&self) -> usize {
        self.config.dimensions()
    }

    /// Metric used for the index and searches.
    pub fn metric_type(&self) -> MilvusMetricType {
        self.config.metric_type()
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
    /// `VectorStore("MilvusStore has been closed")`. Idempotent, like Java `close()`.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// `POST /v2/vectordb/collections/has`.
    pub async fn has_collection(&self) -> Result<bool> {
        let body = has_collection_body(self.config.database_name(), self.config.collection_name());
        let data = self.request(HAS_COLLECTION_PATH, body).await?;
        Ok(data.get("has").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Ensures the collection exists, creating it with the configured schema when missing.
    pub async fn ensure_collection(&self) -> Result<()> {
        if self.has_collection().await? {
            return Ok(());
        }
        self.create_collection().await.map_err(|error| {
            RagError::VectorStore(format!(
                "Failed to ensure collection exists: {}: {error}",
                self.config.collection_name()
            ))
        })
    }

    async fn create_collection(&self) -> Result<()> {
        let body = create_collection_body(
            self.config.database_name(),
            self.config.collection_name(),
            self.config.dimensions(),
            self.config.metric_type(),
        );
        self.request(CREATE_COLLECTION_PATH, body).await.map(|_| ())
    }

    /// Ensures the database exists (skipped for `default`, like Java `ensureDatabase`).
    pub async fn ensure_database(&self) -> Result<()> {
        let database = self.config.database_name();
        if database == DEFAULT_DATABASE_NAME {
            return Ok(());
        }
        let names = self.list_databases().await.map_err(|error| {
            RagError::VectorStore(format!(
                "Failed to ensure databaseName exists: {database}: {error}"
            ))
        })?;
        if names.iter().any(|name| name == database) {
            return Ok(());
        }
        let body = create_database_body(database, self.config.database_properties());
        self.request(CREATE_DATABASE_PATH, body)
            .await
            .map(|_| ())
            .map_err(|error| {
                RagError::VectorStore(format!(
                    "Failed to ensure databaseName exists: {database}: {error}"
                ))
            })
    }

    /// `POST /v2/vectordb/databases/list`.
    pub async fn list_databases(&self) -> Result<Vec<String>> {
        let data = self
            .request(LIST_DATABASES_PATH, list_databases_body())
            .await?;
        Ok(match data {
            Value::Array(items) => items
                .into_iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        })
    }

    /// Upserts documents through the REST-only `entities/upsert` endpoint.
    ///
    /// Java has no upsert path (`add` always calls `insert`); this is an additive capability
    /// for callers that need idempotent re-indexing.
    pub async fn upsert(&self, documents: Vec<Document>) -> Result<()> {
        self.write_documents(documents, UPSERT_PATH).await
    }

    fn ensure_open(&self) -> Result<()> {
        if self.is_closed() {
            return Err(RagError::VectorStore(
                "MilvusStore has been closed".to_string(),
            ));
        }
        Ok(())
    }

    async fn write_documents(&self, documents: Vec<Document>, path: &str) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        self.ensure_open()?;
        let rows = documents
            .iter()
            .map(|document| document_row(document, self.config.dimensions()))
            .collect::<Result<Vec<_>>>()?;
        let body = insert_body(
            self.config.database_name(),
            self.config.collection_name(),
            rows,
        );
        self.request(path, body).await.map(|_| ())
    }

    async fn request(&self, path: &str, body: Value) -> Result<Value> {
        let mut request = HttpRequest::new(HttpMethod::Post, format!("{}{path}", self.base_url))
            .with_header("Content-Type", "application/json; charset=utf-8");
        if let Some(token) = self.config.bearer_token() {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        request.body = Some(serde_json::to_string(&body)?);

        let response = self.transport.execute(request).await?;
        if !response.is_success() {
            return Err(map_http_error(response.status_code, &response.body));
        }
        parse_envelope(&response.body)
    }
}

#[async_trait]
impl VdbStore for MilvusStore {
    async fn add(&self, documents: Vec<Document>) -> Result<()> {
        self.write_documents(documents, INSERT_PATH).await
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<Document>> {
        if query.query_embedding.len() != self.config.dimensions() {
            return Err(RagError::VectorStore(format!(
                "Query embedding dimension mismatch: expected {}, got {}",
                self.config.dimensions(),
                query.query_embedding.len()
            )));
        }
        if query.limit == 0 {
            return Err(RagError::InvalidArgument(
                "Limit must be positive".to_string(),
            ));
        }
        self.ensure_open()?;

        let body = search_body(
            self.config.database_name(),
            self.config.collection_name(),
            query,
            self.config.metric_type(),
        );
        let data = self.request(SEARCH_PATH, body).await?;
        search_data_to_documents(
            &data,
            self.config.metric_type(),
            query.score_threshold,
            query.vector_name.as_deref(),
        )
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        if id.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Document ID cannot be null or empty".to_string(),
            ));
        }
        self.ensure_open()?;
        let body = delete_body(
            self.config.database_name(),
            self.config.collection_name(),
            id,
        );
        let data = self.request(DELETE_PATH, body).await?;
        // An asynchronous delete may report no count at all: HTTP 200 + code 0 is acceptance.
        Ok(count_from_data(&data)
            .map(|count| count > 0)
            .unwrap_or(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct MockTransport {
        responses: Mutex<VecDeque<crate::transport::HttpResponse>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl MockTransport {
        fn new(responses: Vec<crate::transport::HttpResponse>) -> Arc<Self> {
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
        async fn execute(&self, request: HttpRequest) -> Result<crate::transport::HttpResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| RagError::Http("no scripted response".to_string()))
        }
    }

    fn ok(data: Value) -> crate::transport::HttpResponse {
        crate::transport::HttpResponse::new(200, json!({ "code": 0, "data": data }).to_string())
    }

    fn config() -> MilvusConfig {
        MilvusConfig::builder()
            .uri("http://localhost:19530")
            .collection_name("knowledge")
            .dimensions(2)
            .build()
            .unwrap()
    }

    fn config_with_auth() -> MilvusConfig {
        MilvusConfig::builder()
            .uri("http://localhost:19530/")
            .collection_name("knowledge")
            .dimensions(2)
            .username("root")
            .password("Milvus")
            .database_name("my_db")
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
    fn parse_uri_trims_trailing_slash_and_requires_scheme() {
        assert_eq!(
            parse_milvus_uri(" http://localhost:19530/ ").unwrap(),
            "http://localhost:19530"
        );
        assert_eq!(
            parse_milvus_uri("https://milvus.example.com").unwrap(),
            "https://milvus.example.com"
        );
        let error = parse_milvus_uri("localhost:19530").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: URI must start with 'http://' or 'https://': localhost:19530"
        );
    }

    #[test]
    fn builder_uses_the_java_validation_messages() {
        assert_eq!(
            MilvusConfig::builder()
                .collection_name("c")
                .dimensions(2)
                .build()
                .unwrap_err()
                .to_string(),
            "invalid argument: URI cannot be null or empty"
        );
        assert_eq!(
            MilvusConfig::builder()
                .uri("http://localhost:19530")
                .dimensions(2)
                .build()
                .unwrap_err()
                .to_string(),
            "invalid argument: Collection name cannot be null or empty"
        );
        assert_eq!(
            MilvusConfig::builder()
                .uri("http://localhost:19530")
                .collection_name("c")
                .build()
                .unwrap_err()
                .to_string(),
            "invalid argument: Dimensions must be positive"
        );
        assert_eq!(
            MilvusConfig::builder()
                .uri("http://localhost:19530")
                .collection_name("c")
                .dimensions(2)
                .database_name("  ")
                .build()
                .unwrap_err()
                .to_string(),
            "invalid argument: Database name cannot be null or empty"
        );
    }

    #[test]
    fn builder_defaults_match_java() {
        let config = config();
        assert_eq!(config.database_name(), DEFAULT_DATABASE_NAME);
        assert_eq!(config.metric_type(), MilvusMetricType::Cosine);
        assert_eq!(config.connect_timeout_ms(), DEFAULT_CONNECT_TIMEOUT_MS);
        assert!(config.database_properties().is_empty());
    }

    #[test]
    fn bearer_token_prefers_the_explicit_token() {
        let token_config = MilvusConfig::builder()
            .uri("http://localhost:19530")
            .collection_name("c")
            .dimensions(2)
            .token(" root:Milvus ")
            .username("ignored")
            .password("ignored")
            .build()
            .unwrap();
        assert_eq!(token_config.bearer_token().as_deref(), Some("root:Milvus"));

        let credentials = MilvusConfig::builder()
            .uri("http://localhost:19530")
            .collection_name("c")
            .dimensions(2)
            .username("root")
            .build()
            .unwrap();
        assert_eq!(credentials.bearer_token().as_deref(), Some("root:"));

        assert_eq!(config().bearer_token(), None);
    }

    #[test]
    fn transport_options_follow_connect_timeout() {
        let config = MilvusConfig::builder()
            .uri("http://localhost:19530")
            .collection_name("c")
            .dimensions(2)
            .connect_timeout_ms(1_500)
            .build()
            .unwrap();
        let options = config.transport_options();
        assert_eq!(options.connect_timeout, Duration::from_millis(1_500));
        assert_eq!(options.read_timeout, Duration::from_millis(1_500));

        let disabled = MilvusConfig::builder()
            .uri("http://localhost:19530")
            .collection_name("c")
            .dimensions(2)
            .connect_timeout_ms(0)
            .build()
            .unwrap();
        assert_eq!(
            disabled.transport_options().connect_timeout,
            TransportOptions::default().connect_timeout
        );
    }

    #[test]
    fn create_collection_body_mirrors_the_java_schema() {
        let body = create_collection_body("default", "knowledge", 8, MilvusMetricType::L2);
        assert_eq!(body["dbName"], "default");
        assert_eq!(body["collectionName"], "knowledge");
        assert_eq!(body["schema"]["autoID"], false);
        assert_eq!(body["schema"]["enableDynamicField"], true);
        let fields = body["schema"]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0]["fieldName"], "id");
        assert_eq!(fields[0]["dataType"], "VarChar");
        assert_eq!(fields[0]["isPrimaryKey"], true);
        assert_eq!(fields[0]["elementTypeParams"]["max_length"], "64");
        assert_eq!(fields[1]["fieldName"], "vector");
        assert_eq!(fields[1]["dataType"], "FloatVector");
        assert_eq!(fields[1]["elementTypeParams"]["dim"], "8");
        assert_eq!(fields[2]["elementTypeParams"]["max_length"], "256");
        assert_eq!(fields[3]["elementTypeParams"]["max_length"], "65535");
        assert_eq!(fields[4]["fieldName"], "content");
        assert_eq!(fields[5]["dataType"], "JSON");
        let index = &body["indexParams"][0];
        assert_eq!(index["fieldName"], "vector");
        assert_eq!(index["indexType"], "AUTOINDEX");
        assert_eq!(index["metricType"], "L2");
    }

    #[tokio::test]
    async fn initialize_creates_database_and_collection_when_missing() {
        let transport = MockTransport::new(vec![
            ok(json!(["default"])),
            ok(json!({})),
            ok(json!({ "has": false })),
            ok(json!({})),
        ]);
        let store = MilvusStore::connect_with(config_with_auth(), transport.clone())
            .await
            .unwrap();
        assert_eq!(store.base_url(), "http://localhost:19530");

        let requests = transport.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[0].url,
            "http://localhost:19530/v2/vectordb/databases/list"
        );
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].header("Authorization"),
            Some("Bearer root:Milvus")
        );
        assert_eq!(
            requests[0].header("Content-Type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(
            requests[1].url,
            "http://localhost:19530/v2/vectordb/databases/create"
        );
        assert_eq!(transport.body_of(1)["dbName"], "my_db");
        assert!(transport.body_of(1).get("properties").is_none());
        assert_eq!(
            requests[2].url,
            "http://localhost:19530/v2/vectordb/collections/has"
        );
        assert_eq!(transport.body_of(2)["collectionName"], "knowledge");
        assert_eq!(transport.body_of(2)["dbName"], "my_db");
        assert_eq!(
            requests[3].url,
            "http://localhost:19530/v2/vectordb/collections/create"
        );
        assert_eq!(
            transport.body_of(3)["schema"]["fields"][1]["elementTypeParams"]["dim"],
            "2"
        );
    }

    #[tokio::test]
    async fn initialize_skips_existing_database_and_collection() {
        let transport = MockTransport::new(vec![
            ok(json!(["default", "my_db"])),
            ok(json!({ "has": true })),
        ]);
        MilvusStore::connect_with(config_with_auth(), transport.clone())
            .await
            .unwrap();
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].url,
            "http://localhost:19530/v2/vectordb/collections/has"
        );
    }

    #[tokio::test]
    async fn initialize_skips_the_database_probe_for_default() {
        let transport = MockTransport::new(vec![ok(json!({ "has": true }))]);
        MilvusStore::connect_with(config(), transport.clone())
            .await
            .unwrap();
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "http://localhost:19530/v2/vectordb/collections/has"
        );
    }

    #[tokio::test]
    async fn add_posts_the_java_rows() {
        let transport = MockTransport::new(vec![ok(json!({ "insertCount": 1 }))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();

        let mut document = doc("11111111-2222-3333-4444-555555555555", vec![0.5, 1.5]);
        document.metadata.payload = json!({ "source": "unit" });
        store.add(vec![document]).await.unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "http://localhost:19530/v2/vectordb/entities/insert"
        );
        assert!(requests[0].header("Authorization").is_none());

        let body = transport.body_of(0);
        assert_eq!(body["dbName"], "default");
        assert_eq!(body["collectionName"], "knowledge");
        let row = &body["data"][0];
        assert_eq!(row["id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(row["vector"], json!([0.5, 1.5]));
        assert_eq!(row["doc_id"], "doc-1");
        assert_eq!(row["chunk_id"], "chunk-1");
        let content: Value = serde_json::from_str(row["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["type"], "text");
        assert_eq!(content["text"], "hello");
        assert_eq!(row["payload"], json!({ "source": "unit" }));
    }

    #[tokio::test]
    async fn add_writes_an_empty_payload_object_when_absent() {
        let transport = MockTransport::new(vec![ok(json!({ "insertCount": 1 }))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        store.add(vec![doc("id-1", vec![0.0, 0.0])]).await.unwrap();
        assert_eq!(transport.body_of(0)["data"][0]["payload"], json!({}));
    }

    #[tokio::test]
    async fn add_empty_documents_is_a_noop() {
        let transport = MockTransport::new(vec![]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        store.add(Vec::new()).await.unwrap();
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn add_validates_embedding_presence_and_dimensions() {
        let transport = MockTransport::new(vec![]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();

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
    async fn upsert_uses_the_upsert_endpoint() {
        let transport = MockTransport::new(vec![ok(json!({ "upsertCount": 1 }))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        store
            .upsert(vec![doc("id-1", vec![1.0, 0.0])])
            .await
            .unwrap();
        assert_eq!(
            transport.requests()[0].url,
            "http://localhost:19530/v2/vectordb/entities/upsert"
        );
    }

    #[tokio::test]
    async fn search_builds_the_request_and_maps_results() {
        let transport = MockTransport::new(vec![ok(json!([{
            "id": "id-1",
            "distance": 0.87,
            "entity": {
                "id": "id-1",
                "doc_id": "doc-1",
                "chunk_id": "chunk-1",
                "content": "{\"type\":\"text\",\"text\":\"hello\"}",
                "payload": { "source": "unit" }
            }
        }]))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        let documents = store
            .search(&SearchQuery::new(vec![0.5, 0.5], 3))
            .await
            .unwrap();

        assert_eq!(
            transport.requests()[0].url,
            "http://localhost:19530/v2/vectordb/entities/search"
        );
        let body = transport.body_of(0);
        assert_eq!(body["data"], json!([[0.5, 0.5]]));
        assert_eq!(body["annsField"], "vector");
        assert_eq!(body["limit"], 3);
        assert_eq!(
            body["outputFields"],
            json!(["id", "doc_id", "chunk_id", "content", "payload"])
        );
        assert_eq!(body["searchParams"]["metricType"], "COSINE");

        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].metadata.doc_id, "doc-1");
        assert_eq!(documents[0].metadata.chunk_id, "chunk-1");
        assert_eq!(documents[0].metadata.content_text(), "hello");
        assert_eq!(documents[0].payload_value("source"), Some(&json!("unit")));
        assert_eq!(documents[0].score, Some(0.87));
        // Java rebuilds the id deterministically from the metadata.
        assert_eq!(
            documents[0].id,
            Document::new(documents[0].metadata.clone()).id
        );
    }

    #[tokio::test]
    async fn search_uses_the_vector_name_as_anns_field() {
        let transport = MockTransport::new(vec![ok(json!([]))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        let query = SearchQuery::new(vec![0.1, 0.2], 5).with_vector_name("image_vector");
        let documents = store.search(&query).await.unwrap();
        assert!(documents.is_empty());
        assert_eq!(transport.body_of(0)["annsField"], "image_vector");
    }

    #[tokio::test]
    async fn search_filters_below_the_score_threshold() {
        let transport = MockTransport::new(vec![ok(json!([
            { "id": "a", "distance": 0.9, "entity": { "doc_id": "d", "chunk_id": "0", "content": "hi" } },
            { "id": "b", "distance": 0.2, "entity": { "doc_id": "d", "chunk_id": "1", "content": "hi" } }
        ]))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        let query = SearchQuery::new(vec![0.0, 0.0], 5).with_score_threshold(0.5);
        let documents = store.search(&query).await.unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].score, Some(0.9));
        // The threshold is applied client-side, never sent to Milvus.
        assert!(transport.body_of(0).get("scoreThreshold").is_none());
        assert!(transport.body_of(0)["searchParams"]
            .get("scoreThreshold")
            .is_none());
    }

    #[tokio::test]
    async fn search_normalizes_l2_distances() {
        let transport = MockTransport::new(vec![ok(json!([{
            "id": "a",
            "distance": 3.0,
            "entity": { "doc_id": "d", "chunk_id": "0", "content": "hi" }
        }]))]);
        let config = MilvusConfig::builder()
            .uri("http://localhost:19530")
            .collection_name("knowledge")
            .dimensions(2)
            .metric_type(MilvusMetricType::L2)
            .build()
            .unwrap();
        let store = MilvusStore::new(config, transport.clone()).unwrap();
        let documents = store
            .search(&SearchQuery::new(vec![0.0, 0.0], 1))
            .await
            .unwrap();
        assert_eq!(documents[0].score, Some(0.25));
        assert_eq!(transport.body_of(0)["searchParams"]["metricType"], "L2");
    }

    #[tokio::test]
    async fn search_accepts_nested_query_result_sets() {
        let transport = MockTransport::new(vec![ok(json!([[
            { "id": "a", "distance": 0.6, "entity": { "doc_id": "d", "chunk_id": "0", "content": "hi" } }
        ]]))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        let documents = store
            .search(&SearchQuery::new(vec![0.0, 0.0], 1))
            .await
            .unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].score, Some(0.6));
    }

    #[tokio::test]
    async fn search_skips_entities_it_cannot_reconstruct() {
        let transport = MockTransport::new(vec![ok(json!([
            { "id": "a", "distance": 0.9 },
            { "id": "b", "distance": 0.8, "entity": { "chunk_id": "0", "content": "hi" } },
            { "id": "c", "distance": 0.7, "entity": { "doc_id": "d", "chunk_id": "0" } },
            { "id": "d", "distance": 0.6, "entity": { "doc_id": "d", "chunk_id": "0", "content": "kept" } }
        ]))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        let documents = store
            .search(&SearchQuery::new(vec![0.0, 0.0], 4))
            .await
            .unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].metadata.content_text(), "kept");
    }

    #[tokio::test]
    async fn search_validates_dimensions_and_limit() {
        let transport = MockTransport::new(vec![]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();

        let error = store
            .search(&SearchQuery::new(vec![0.0], 1))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "vector store error: Query embedding dimension mismatch: expected 2, got 1"
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
    async fn delete_uses_the_doc_id_filter() {
        let transport = MockTransport::new(vec![ok(json!({ "deleteCount": 2 }))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        assert!(store.delete("doc-1").await.unwrap());

        assert_eq!(
            transport.requests()[0].url,
            "http://localhost:19530/v2/vectordb/entities/delete"
        );
        assert_eq!(transport.body_of(0)["filter"], "doc_id in [\"doc-1\"]");
    }

    #[tokio::test]
    async fn delete_reports_false_for_a_zero_count() {
        let transport = MockTransport::new(vec![ok(json!({ "deleteCount": 0 }))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        assert!(!store.delete("doc-1").await.unwrap());
    }

    #[tokio::test]
    async fn delete_treats_a_missing_count_as_accepted() {
        let transport = MockTransport::new(vec![ok(json!({}))]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        assert!(store.delete("doc-1").await.unwrap());
    }

    #[test]
    fn delete_filter_escapes_quotes_and_backslashes() {
        assert_eq!(delete_filter(r#"a"b\c"#), r#"doc_id in ["a\"b\\c"]"#);
    }

    #[tokio::test]
    async fn delete_rejects_a_blank_id() {
        let transport = MockTransport::new(vec![]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        let error = store.delete("   ").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid argument: Document ID cannot be null or empty"
        );
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn api_level_errors_map_to_vector_store_errors() {
        let transport = MockTransport::new(vec![crate::transport::HttpResponse::new(
            200,
            json!({ "code": 1100, "message": "collection not found" }).to_string(),
        )]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        let error = store
            .add(vec![doc("id", vec![0.0, 0.0])])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "vector store error: Milvus error 1100: collection not found"
        );
    }

    #[tokio::test]
    async fn http_errors_keep_their_status_codes() {
        let transport = MockTransport::new(vec![
            crate::transport::HttpResponse::new(500, r#"{"message":"boom"}"#),
            crate::transport::HttpResponse::new(401, "unauthorized"),
        ]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();

        let error = store
            .search(&SearchQuery::new(vec![0.0, 0.0], 1))
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), Some(500));
        assert!(error.is_server_error());

        let error = store.delete("doc-1").await.unwrap_err();
        assert!(error.is_client_error());
        assert!(matches!(
            error,
            RagError::Auth {
                status_code: 401,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_closed_store_rejects_every_operation() {
        let transport = MockTransport::new(vec![]);
        let store = MilvusStore::new(config(), transport.clone()).unwrap();
        store.close();
        store.close();
        assert!(store.is_closed());

        assert_eq!(
            store
                .add(vec![doc("id", vec![0.0, 0.0])])
                .await
                .unwrap_err()
                .to_string(),
            "vector store error: MilvusStore has been closed"
        );
        assert_eq!(
            store
                .search(&SearchQuery::new(vec![0.0, 0.0], 1))
                .await
                .unwrap_err()
                .to_string(),
            "vector store error: MilvusStore has been closed"
        );
        assert_eq!(
            store.delete("doc-1").await.unwrap_err().to_string(),
            "vector store error: MilvusStore has been closed"
        );
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn content_and_payload_fall_back_to_tolerant_parsing() {
        let value = json!({
            "entity": { "doc_id": "d", "chunk_id": "0", "content": "plain text" }
        });
        let document = search_item_to_document(&value, 0.5, None).unwrap();
        assert_eq!(document.metadata.content_text(), "plain text");
        assert!(document.payload().is_empty());

        let value = json!({
            "entity": {
                "doc_id": "d",
                "chunk_id": "0",
                "content": "plain",
                "payload": "{\"k\":1}"
            }
        });
        let document = search_item_to_document(&value, 0.5, Some("vec")).unwrap();
        assert_eq!(document.payload_value("k"), Some(&json!(1)));
        assert_eq!(document.vector_name.as_deref(), Some("vec"));

        let value = json!({
            "entity": {
                "doc_id": "d",
                "chunk_id": "0",
                "content": "plain",
                "payload": "not json"
            }
        });
        let document = search_item_to_document(&value, 0.5, None).unwrap();
        assert!(document.payload().is_empty());
    }

    #[test]
    fn envelope_parsers_report_missing_data() {
        assert!(!parse_has_collection(r#"{"code":0,"data":{}}"#).unwrap());
        assert_eq!(parse_delete_count(r#"{"code":0,"data":{}}"#).unwrap(), None);
        assert_eq!(
            parse_database_list(r#"{"code":0,"data":["default"]}"#).unwrap(),
            vec!["default".to_string()]
        );
        assert!(parse_delete_count("not json").is_err());
    }

    #[test]
    fn normalize_score_only_touches_l2() {
        assert_eq!(MilvusMetricType::L2.normalize_score(0.0), 1.0);
        assert_eq!(MilvusMetricType::L2.normalize_score(3.0), 0.25);
        assert_eq!(MilvusMetricType::Cosine.normalize_score(0.4), 0.4);
        assert_eq!(MilvusMetricType::Ip.normalize_score(-1.5), -1.5);
        assert_eq!(MilvusMetricType::Cosine.as_wire(), "COSINE");
        assert_eq!(MilvusMetricType::Ip.as_wire(), "IP");
    }
}
