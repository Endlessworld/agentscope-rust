//! PostgreSQL + `pgvector` vector store.
//!
//! Ports `io.agentscope.core.rag.store.PgVectorStore`. Java drives JDBC + the
//! `com.pgvector:pgvector` type; the Rust port drives `tokio-postgres` through the small
//! [`PgExecutor`] seam, so every SQL statement, parameter list and row decoder is a pure
//! function that can be tested offline with [`FakeExecutor`](tests) — the real
//! [`TokioPostgresExecutor`] only translates [`PgParam`] into `ToSql` values and decodes
//! `tokio_postgres::Row`s.
//!
//! Schema is byte-for-byte the Java schema: `id VARCHAR(64) PRIMARY KEY`,
//! `embedding vector(N)`, `doc_id VARCHAR(256)`, `chunk_id VARCHAR(256)`, `content TEXT`,
//! `payload JSONB`, plus the `doc_id` and HNSW indexes.
//!
//! Mapping notes (Java → Rust):
//!
//! * `ensureTable` → `CREATE EXTENSION IF NOT EXISTS vector`, optional `CREATE SCHEMA`,
//!   an `information_schema.tables` existence probe, then table + index DDL.
//! * `executeBatch` upsert → `INSERT ... ON CONFLICT (id) DO UPDATE SET ...` with one bind
//!   list per document, `$6::jsonb` for the payload.
//! * Search orders by the pgvector distance operator (`<=>`, `<->`, `<#>`) exactly like
//!   Java and converts the distance back into a similarity score.
//! * `delete(docId)` deletes by the **`doc_id` column** (not the primary key), i.e. all
//!   chunks of a document, and reports whether any row was removed.
//!
//! Deliberate differences:
//!
//! * No `PGvector` extension type is registered: the search projection selects
//!   `embedding::text` / `payload::text` and the writes bind pgvector's textual literal
//!   (`[1,2,3]`) with an explicit `::vector` cast. Same wire format, no extra crate.
//! * Java wraps failures in `VectorStoreException("Failed to add documents to PostgreSQL")`
//!   with the cause attached; Rust keeps that wording in [`RagError::VectorStore`] and
//!   appends the underlying message.
//! * An empty `schema("")` is normalised to `public` (Java would build `".table"` and fail).
//! * Lazy table creation is explicit: [`PgVectorStore::connect_with`] performs Java's
//!   constructor-time check, [`PgVectorStore::new`] performs no I/O.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, TextBlock};
use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::error::{RagError, Result};
use crate::model::{Document, DocumentMetadata};
use crate::store::{SearchQuery, VdbStore};

/// Default schema, like Java `DEFAULT_SCHEMA`.
pub const DEFAULT_SCHEMA: &str = "public";
/// Default connection timeout in milliseconds (Java builder default).
pub const DEFAULT_CONNECTION_TIMEOUT_MS: u64 = 30_000;
/// PostgreSQL identifier length limit.
pub const MAX_IDENTIFIER_LENGTH: usize = 63;

/// Primary-key column.
pub const COL_ID: &str = "id";
/// Vector column.
pub const COL_VECTOR: &str = "embedding";
/// Owning document id column.
pub const COL_DOC_ID: &str = "doc_id";
/// Chunk id column.
pub const COL_CHUNK_ID: &str = "chunk_id";
/// Serialized `ContentBlock` column.
pub const COL_CONTENT: &str = "content";
/// Custom payload column.
pub const COL_PAYLOAD: &str = "payload";

/// `CREATE EXTENSION` executed before any DDL, like Java `ensureTable`.
pub const CREATE_EXTENSION_SQL: &str = "CREATE EXTENSION IF NOT EXISTS vector";

/// Distance metrics supported by pgvector (Java `PgVectorStore.DistanceType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PgVectorDistanceType {
    /// L2 / Euclidean distance, operator `<->`.
    L2,
    /// Inner product, operator `<#>` (pgvector returns the negative inner product).
    InnerProduct,
    /// Cosine distance, operator `<=>` (Java default).
    #[default]
    Cosine,
}

impl PgVectorDistanceType {
    /// HNSW operator class (`vector_cosine_ops`, ...).
    pub fn index_ops(self) -> &'static str {
        match self {
            PgVectorDistanceType::L2 => "vector_l2_ops",
            PgVectorDistanceType::InnerProduct => "vector_ip_ops",
            PgVectorDistanceType::Cosine => "vector_cosine_ops",
        }
    }

    /// pgvector distance operator used in `ORDER BY`.
    pub fn operator(self) -> &'static str {
        match self {
            PgVectorDistanceType::L2 => "<->",
            PgVectorDistanceType::InnerProduct => "<#>",
            PgVectorDistanceType::Cosine => "<=>",
        }
    }
}

/// Parsed parts of a JDBC URL (`jdbc:postgresql://host:port/database?user=..`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JdbcLocation {
    /// Host, when present.
    pub host: Option<String>,
    /// Port, when present and numeric.
    pub port: Option<u16>,
    /// Database name, when present.
    pub database: Option<String>,
    /// User from the user-info or query string.
    pub user: Option<String>,
    /// Password from the user-info or query string.
    pub password: Option<String>,
}

/// Splits `host`, `host:port` and `[::1]:port` forms.
fn split_host_port(authority: &str) -> (Option<String>, Option<u16>) {
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail
                .strip_prefix(':')
                .and_then(|port| port.parse::<u16>().ok());
            return (Some(host.to_string()), port);
        }
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (Some(host.to_string()), port.parse::<u16>().ok())
        }
        _ => (
            if authority.is_empty() {
                None
            } else {
                Some(authority.to_string())
            },
            None,
        ),
    }
}

/// Parses a `jdbc:postgresql://` URL into its parts.
pub fn parse_jdbc_url(jdbc_url: &str) -> Result<JdbcLocation> {
    let trimmed = jdbc_url.trim();
    let rest = trimmed.strip_prefix("jdbc:postgresql://").ok_or_else(|| {
        RagError::InvalidArgument(format!(
            "JDBC URL must start with 'jdbc:postgresql://': {jdbc_url}"
        ))
    })?;

    let (authority_and_database, query) = match rest.split_once('?') {
        Some((left, right)) => (left, Some(right)),
        None => (rest, None),
    };
    let (authority, database) = match authority_and_database.split_once('/') {
        Some((authority, database)) => (authority, Some(database.to_string())),
        None => (authority_and_database, None),
    };
    let (user_info, host_port) = match authority.rsplit_once('@') {
        Some((user_info, host_port)) => (Some(user_info), host_port),
        None => (None, authority),
    };

    let mut location = JdbcLocation {
        database: database.filter(|database| !database.is_empty()),
        ..JdbcLocation::default()
    };
    if let Some(user_info) = user_info {
        match user_info.split_once(':') {
            Some((user, password)) => {
                location.user = Some(user.to_string());
                location.password = Some(password.to_string());
            }
            None => location.user = Some(user_info.to_string()),
        }
    }
    let (host, port) = split_host_port(host_port);
    location.host = host;
    location.port = port;

    if let Some(query) = query {
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = match pair.split_once('=') {
                Some((key, value)) => (key, value),
                None => continue,
            };
            match key {
                "user" => location.user = Some(value.to_string()),
                "password" => location.password = Some(value.to_string()),
                _ => {}
            }
        }
    }

    Ok(location)
}

/// Escapes one libpq connection-string value.
fn libpq_value(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c == '\'' || c == '\\')
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Connection / table configuration for [`PgVectorStore`].
pub struct PgVectorConfig {
    jdbc_url: String,
    username: Option<String>,
    password: Option<String>,
    schema: String,
    table_name: String,
    dimensions: usize,
    distance_type: PgVectorDistanceType,
    connection_timeout_ms: u64,
    connection_string: Option<String>,
}

impl std::fmt::Debug for PgVectorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgVectorConfig")
            .field("jdbc_url", &self.jdbc_url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("schema", &self.schema)
            .field("table_name", &self.table_name)
            .field("dimensions", &self.dimensions)
            .field("distance_type", &self.distance_type)
            .field("connection_timeout_ms", &self.connection_timeout_ms)
            .finish()
    }
}

impl Clone for PgVectorConfig {
    fn clone(&self) -> Self {
        Self {
            jdbc_url: self.jdbc_url.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            schema: self.schema.clone(),
            table_name: self.table_name.clone(),
            dimensions: self.dimensions,
            distance_type: self.distance_type,
            connection_timeout_ms: self.connection_timeout_ms,
            connection_string: self.connection_string.clone(),
        }
    }
}

impl PgVectorConfig {
    /// Creates a builder.
    pub fn builder() -> PgVectorConfigBuilder {
        PgVectorConfigBuilder::default()
    }

    /// Original JDBC URL.
    pub fn jdbc_url(&self) -> &str {
        &self.jdbc_url
    }

    /// Configured username.
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// Configured schema (never empty).
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Configured table name.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Vector dimensions.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Distance metric.
    pub fn distance_type(&self) -> PgVectorDistanceType {
        self.distance_type
    }

    /// Connection timeout in milliseconds.
    pub fn connection_timeout_ms(&self) -> u64 {
        self.connection_timeout_ms
    }

    /// `schema.table`, as used in every statement.
    pub fn full_table_name(&self) -> String {
        format!("{}.{}", self.schema, self.table_name)
    }

    /// libpq connection string used by [`TokioPostgresExecutor`].
    ///
    /// An explicit `connection_string(...)` wins; otherwise it is derived from the JDBC URL
    /// plus the builder's credentials and timeout (Java passes them as JDBC properties).
    pub fn connection_string(&self) -> Result<String> {
        if let Some(connection_string) = &self.connection_string {
            return Ok(connection_string.clone());
        }
        let location = parse_jdbc_url(&self.jdbc_url)?;
        let user = self.username.clone().or_else(|| location.user.clone());
        let password = self.password.clone().or_else(|| location.password.clone());

        let mut parts: Vec<String> = Vec::new();
        if let Some(host) = &location.host {
            parts.push(format!("host={}", libpq_value(host)));
        }
        if let Some(port) = location.port {
            parts.push(format!("port={port}"));
        }
        if let Some(database) = &location.database {
            parts.push(format!("dbname={}", libpq_value(database)));
        }
        if let Some(user) = &user {
            parts.push(format!("user={}", libpq_value(user)));
        }
        if let Some(password) = &password {
            parts.push(format!("password={}", libpq_value(password)));
        }
        if self.connection_timeout_ms > 0 {
            parts.push(format!(
                "connect_timeout={}",
                self.connection_timeout_ms / 1000
            ));
        }
        Ok(parts.join(" "))
    }
}

/// Builder for [`PgVectorConfig`], mirroring Java `PgVectorStore.Builder`.
#[derive(Debug, Clone, Default)]
pub struct PgVectorConfigBuilder {
    jdbc_url: Option<String>,
    username: Option<String>,
    password: Option<String>,
    schema: Option<String>,
    table_name: Option<String>,
    dimensions: usize,
    distance_type: PgVectorDistanceType,
    connection_timeout_ms: Option<u64>,
    connection_string: Option<String>,
}

impl PgVectorConfigBuilder {
    /// Sets the JDBC URL (`jdbc:postgresql://host:5432/db`).
    pub fn jdbc_url(mut self, jdbc_url: impl Into<String>) -> Self {
        self.jdbc_url = Some(jdbc_url.into());
        self
    }

    /// Sets the database username.
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Sets the database password.
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Sets the schema (`public` by default).
    pub fn schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// Sets the table name.
    pub fn table_name(mut self, table_name: impl Into<String>) -> Self {
        self.table_name = Some(table_name.into());
        self
    }

    /// Sets the vector dimensions.
    pub fn dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Sets the distance metric (`Cosine` by default).
    pub fn distance_type(mut self, distance_type: PgVectorDistanceType) -> Self {
        self.distance_type = distance_type;
        self
    }

    /// Sets the connection timeout in milliseconds (`0` disables it).
    pub fn connection_timeout_ms(mut self, connection_timeout_ms: u64) -> Self {
        self.connection_timeout_ms = Some(connection_timeout_ms);
        self
    }

    /// Overrides the libpq connection string instead of deriving it from the JDBC URL.
    pub fn connection_string(mut self, connection_string: impl Into<String>) -> Self {
        self.connection_string = Some(connection_string.into());
        self
    }

    /// Validates and builds the configuration, using the Java error messages.
    pub fn build(self) -> Result<PgVectorConfig> {
        let jdbc_url = self.jdbc_url.unwrap_or_default();
        if jdbc_url.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "JDBC URL cannot be null or empty".to_string(),
            ));
        }
        let table_name = self.table_name.unwrap_or_default();
        if table_name.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Table name cannot be null or empty".to_string(),
            ));
        }
        if self.dimensions == 0 {
            return Err(RagError::InvalidArgument(
                "Dimensions must be positive".to_string(),
            ));
        }
        validate_identifier(&table_name, "Table name")?;
        let schema = match self.schema {
            Some(schema) if !schema.trim().is_empty() => {
                validate_identifier(&schema, "Schema name")?;
                schema
            }
            _ => DEFAULT_SCHEMA.to_string(),
        };

        Ok(PgVectorConfig {
            jdbc_url,
            username: self.username,
            password: self.password,
            schema,
            table_name,
            dimensions: self.dimensions,
            distance_type: self.distance_type,
            connection_timeout_ms: self
                .connection_timeout_ms
                .unwrap_or(DEFAULT_CONNECTION_TIMEOUT_MS),
            connection_string: self.connection_string,
        })
    }
}

/// Validates a schema/table identifier (Java `validateIdentifier`, SQL-injection guard).
pub fn validate_identifier(identifier: &str, identifier_type: &str) -> Result<()> {
    if identifier.is_empty() {
        return Err(RagError::InvalidArgument(format!(
            "{identifier_type} cannot be null or empty"
        )));
    }
    if identifier.chars().count() > MAX_IDENTIFIER_LENGTH {
        return Err(RagError::InvalidArgument(format!(
            "{identifier_type} cannot exceed {MAX_IDENTIFIER_LENGTH} characters"
        )));
    }
    let mut chars = identifier.chars();
    let starts_ok = chars
        .next()
        .map(|c| c.is_ascii_alphabetic() || c == '_')
        .unwrap_or(false);
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !starts_ok || !rest_ok {
        return Err(RagError::InvalidArgument(format!(
            "{identifier_type} contains invalid characters. Only alphanumeric characters and underscores are allowed, and it must start with a letter or underscore. Invalid value: {identifier}"
        )));
    }
    Ok(())
}

/// `CREATE TABLE` statement generated by Java `createTable`.
pub fn create_table_sql(schema: &str, table: &str, dimensions: usize) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {schema}.{table} ({COL_ID} VARCHAR(64) PRIMARY KEY, {COL_VECTOR} vector({dimensions}), {COL_DOC_ID} VARCHAR(256), {COL_CHUNK_ID} VARCHAR(256), {COL_CONTENT} TEXT, {COL_PAYLOAD} JSONB)"
    )
}

/// `CREATE SCHEMA` statement (only emitted for non-`public` schemas, like Java).
pub fn create_schema_sql(schema: &str) -> String {
    format!("CREATE SCHEMA IF NOT EXISTS {schema}")
}

/// `doc_id` index statement generated by Java `createTable`.
pub fn create_doc_id_index_sql(schema: &str, table: &str) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS idx_{schema}_{table}_doc_id ON {schema}.{table} ({COL_DOC_ID})"
    )
}

/// HNSW index statement generated by Java `createTable`.
pub fn create_vector_index_sql(
    schema: &str,
    table: &str,
    distance_type: PgVectorDistanceType,
) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS idx_{schema}_{table}_vector ON {schema}.{table} USING hnsw ({COL_VECTOR} {})",
        distance_type.index_ops()
    )
}

/// Table-existence probe over `information_schema` (Java uses JDBC `getTables` with
/// lower-cased schema/table names).
pub fn table_exists_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '{}' AND table_name = '{}'",
        schema.to_lowercase(),
        table.to_lowercase()
    )
}

/// Upsert statement (`INSERT ... ON CONFLICT (id) DO UPDATE SET`) from Java
/// `addDocumentsToPostgres`.
pub fn upsert_sql(schema: &str, table: &str) -> String {
    format!(
        "INSERT INTO {schema}.{table} ({COL_ID}, {COL_VECTOR}, {COL_DOC_ID}, {COL_CHUNK_ID}, {COL_CONTENT}, {COL_PAYLOAD}) \
         VALUES ($1, $2::vector, $3, $4, $5, $6::jsonb) \
         ON CONFLICT ({COL_ID}) DO UPDATE SET \
         {COL_VECTOR} = EXCLUDED.{COL_VECTOR}, \
         {COL_DOC_ID} = EXCLUDED.{COL_DOC_ID}, \
         {COL_CHUNK_ID} = EXCLUDED.{COL_CHUNK_ID}, \
         {COL_CONTENT} = EXCLUDED.{COL_CONTENT}, \
         {COL_PAYLOAD} = EXCLUDED.{COL_PAYLOAD}"
    )
}

/// Search statement from Java `searchDocumentsInPostgres`.
///
/// `embedding::text` / `payload::text` keep the projection decodable without registering the
/// `vector` / `jsonb` type extensions on the `tokio-postgres` connection.
pub fn search_sql(schema: &str, table: &str, distance_type: PgVectorDistanceType) -> String {
    format!(
        "SELECT {COL_ID}, {COL_VECTOR}::text AS {COL_VECTOR}, {COL_DOC_ID}, {COL_CHUNK_ID}, {COL_CONTENT}, {COL_PAYLOAD}::text AS {COL_PAYLOAD}, ({COL_VECTOR} {} $1::vector) as distance \
         FROM {schema}.{table} ORDER BY distance ASC LIMIT $2",
        distance_type.operator()
    )
}

/// Delete statement from Java `deleteDocumentFromPostgres` (by `doc_id`, not the primary key).
pub fn delete_sql(schema: &str, table: &str) -> String {
    format!("DELETE FROM {schema}.{table} WHERE {COL_DOC_ID} = $1")
}

/// Renders a vector as a pgvector text literal (`[1.0,0.5]`).
///
/// Java converts `double[]` to `float[]` before handing it to `PGvector`; the cast is kept so
/// both implementations store identical values.
pub fn vector_literal(values: &[f64]) -> String {
    let parts: Vec<String> = values
        .iter()
        .map(|value| format!("{:?}", *value as f32))
        .collect();
    format!("[{}]", parts.join(","))
}

/// Parses a pgvector text literal back into floats.
pub fn parse_vector_literal(literal: &str) -> Result<Vec<f64>> {
    let trimmed = literal.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<f64>()
                .map_err(|_| RagError::VectorStore(format!("invalid vector literal: {literal}")))
        })
        .collect()
}

/// Converts a pgvector distance into a similarity score (higher is better).
pub fn distance_to_score(distance: f64, distance_type: PgVectorDistanceType) -> f64 {
    match distance_type {
        PgVectorDistanceType::Cosine => 1.0 - distance,
        PgVectorDistanceType::L2 => 1.0 / (1.0 + distance),
        // pgvector returns the negated inner product.
        PgVectorDistanceType::InnerProduct => -distance,
    }
}

/// One bound statement parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum PgParam {
    /// A text / varchar value.
    Text(String),
    /// A JSON document bound with `::jsonb`.
    Json(String),
    /// A pgvector literal bound with `::vector`.
    Vector(String),
    /// An `int4` value (the search limit).
    Int(i32),
}

impl PgParam {
    /// Text payload of the parameter (used by tests and logging).
    pub fn as_text(&self) -> String {
        match self {
            PgParam::Text(value) | PgParam::Json(value) | PgParam::Vector(value) => value.clone(),
            PgParam::Int(value) => value.to_string(),
        }
    }
}

/// One decoded search row, in the order produced by [`search_sql`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PgRow {
    /// `id` column.
    pub id: String,
    /// `embedding::text` column.
    pub embedding: Option<String>,
    /// `doc_id` column.
    pub doc_id: String,
    /// `chunk_id` column.
    pub chunk_id: String,
    /// `content` column (serialized `ContentBlock`).
    pub content: String,
    /// `payload::text` column (serialized JSON object).
    pub payload: String,
    /// Distance produced by the configured operator.
    pub distance: f64,
}

/// Decodes one row into a [`Document`], mapping the distance onto a similarity score.
pub fn row_to_document(row: &PgRow, distance_type: PgVectorDistanceType) -> Result<Document> {
    let content = match serde_json::from_str::<ContentBlock>(&row.content) {
        Ok(content) => content,
        Err(_) => ContentBlock::Text(TextBlock::new(row.content.clone())),
    };

    let custom_payload = if row.payload.trim().is_empty() || row.payload.trim() == "{}" {
        Value::Object(Map::new())
    } else {
        match serde_json::from_str::<Value>(&row.payload) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) => {
                tracing::warn!("pgvector payload column is not a JSON object; using an empty map");
                Value::Object(Map::new())
            }
            Err(error) => {
                tracing::warn!("failed to parse pgvector payload column: {error}");
                Value::Object(Map::new())
            }
        }
    };

    let metadata =
        DocumentMetadata::with_payload(content, &row.doc_id, &row.chunk_id, custom_payload);
    let mut document = Document::new(metadata);
    document.set_score(distance_to_score(row.distance, distance_type));
    if let Some(literal) = &row.embedding {
        if let Ok(embedding) = parse_vector_literal(literal) {
            document.set_embedding(embedding);
        }
    }
    Ok(document)
}

/// SQL seam used by [`PgVectorStore`].
///
/// The real implementation is [`TokioPostgresExecutor`]; tests inject a recording fake.
#[async_trait]
pub trait PgExecutor: Send + Sync {
    /// Executes a statement without parameters (`CREATE ...`).
    async fn execute(&self, sql: &str) -> Result<()>;

    /// Executes one parameterized statement per row, returning the affected row count.
    async fn execute_batch(&self, sql: &str, rows: Vec<Vec<PgParam>>) -> Result<u64>;

    /// Runs a query and decodes the rows into [`PgRow`]s.
    async fn query(&self, sql: &str, params: Vec<PgParam>) -> Result<Vec<PgRow>>;

    /// Runs a single-column `COUNT(*)` query.
    async fn query_count(&self, sql: &str) -> Result<i64>;

    /// Executes one parameterized statement, returning the affected row count.
    async fn execute_update(&self, sql: &str, params: Vec<PgParam>) -> Result<u64>;
}

fn sql_error(error: tokio_postgres::Error) -> RagError {
    RagError::VectorStore(error.to_string())
}

fn boxed_params(params: Vec<PgParam>) -> Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> {
    params
        .into_iter()
        .map(
            |param| -> Box<dyn tokio_postgres::types::ToSql + Sync + Send> {
                match param {
                    PgParam::Text(value) | PgParam::Json(value) | PgParam::Vector(value) => {
                        Box::new(value)
                    }
                    PgParam::Int(value) => Box::new(value),
                }
            },
        )
        .collect()
}

/// Real `tokio-postgres` adapter. Thin by design: it only binds parameters and decodes rows.
#[derive(Debug)]
pub struct TokioPostgresExecutor {
    client: tokio_postgres::Client,
}

impl TokioPostgresExecutor {
    /// Connects with a libpq connection string (see [`PgVectorConfig::connection_string`]).
    pub async fn connect(connection_string: &str) -> Result<Self> {
        let (client, connection) =
            tokio_postgres::connect(connection_string, tokio_postgres::NoTls)
                .await
                .map_err(|error| {
                    RagError::VectorStore(format!("Failed to initialize PgVectorStore: {error}"))
                })?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!("[pgvector] connection error: {error}");
            }
        });
        Ok(Self { client })
    }

    /// Underlying client, for advanced operations.
    pub fn client(&self) -> &tokio_postgres::Client {
        &self.client
    }
}

#[async_trait]
impl PgExecutor for TokioPostgresExecutor {
    async fn execute(&self, sql: &str) -> Result<()> {
        self.client.batch_execute(sql).await.map_err(sql_error)
    }

    async fn execute_batch(&self, sql: &str, rows: Vec<Vec<PgParam>>) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let statement = self.client.prepare(sql).await.map_err(sql_error)?;
        let mut affected = 0u64;
        for row in rows {
            let owned = boxed_params(row);
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = owned
                .iter()
                .map(|value| value.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
                .collect();
            affected += self
                .client
                .execute(&statement, &refs)
                .await
                .map_err(sql_error)?;
        }
        Ok(affected)
    }

    async fn query(&self, sql: &str, params: Vec<PgParam>) -> Result<Vec<PgRow>> {
        let owned = boxed_params(params);
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = owned
            .iter()
            .map(|value| value.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        let rows = self.client.query(sql, &refs).await.map_err(sql_error)?;
        Ok(rows
            .iter()
            .map(|row| PgRow {
                id: row.try_get::<_, String>(0).unwrap_or_default(),
                embedding: row.try_get::<_, Option<String>>(1).ok().flatten(),
                doc_id: row.try_get::<_, String>(2).unwrap_or_default(),
                chunk_id: row.try_get::<_, String>(3).unwrap_or_default(),
                content: row
                    .try_get::<_, Option<String>>(4)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                payload: row
                    .try_get::<_, Option<String>>(5)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                distance: row.try_get::<_, f64>(6).unwrap_or_default(),
            })
            .collect())
    }

    async fn query_count(&self, sql: &str) -> Result<i64> {
        let row = self.client.query_one(sql, &[]).await.map_err(sql_error)?;
        Ok(row.try_get::<_, i64>(0).unwrap_or_default())
    }

    async fn execute_update(&self, sql: &str, params: Vec<PgParam>) -> Result<u64> {
        let owned = boxed_params(params);
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = owned
            .iter()
            .map(|value| value.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        self.client.execute(sql, &refs).await.map_err(sql_error)
    }
}

/// pgvector-backed vector store.
pub struct PgVectorStore {
    config: PgVectorConfig,
    executor: Arc<dyn PgExecutor>,
    closed: AtomicBool,
}

impl std::fmt::Debug for PgVectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgVectorStore")
            .field("config", &self.config)
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl PgVectorStore {
    /// Creates a store over an injected executor. No I/O happens here.
    pub fn new(config: PgVectorConfig, executor: Arc<dyn PgExecutor>) -> Self {
        Self {
            config,
            executor,
            closed: AtomicBool::new(false),
        }
    }

    /// Connects with the real [`TokioPostgresExecutor`] and ensures the table exists
    /// (Java's constructor behaviour).
    pub async fn connect(config: PgVectorConfig) -> Result<Self> {
        let executor =
            Arc::new(TokioPostgresExecutor::connect(&config.connection_string()?).await?);
        Self::connect_with(config, executor).await
    }

    /// Creates a store over an injected executor and ensures the table exists.
    pub async fn connect_with(
        config: PgVectorConfig,
        executor: Arc<dyn PgExecutor>,
    ) -> Result<Self> {
        let store = Self::new(config, executor);
        store.initialize().await?;
        Ok(store)
    }

    /// Configuration snapshot.
    pub fn config(&self) -> &PgVectorConfig {
        &self.config
    }

    /// Original JDBC URL.
    pub fn jdbc_url(&self) -> &str {
        self.config.jdbc_url()
    }

    /// Schema name.
    pub fn schema(&self) -> &str {
        self.config.schema()
    }

    /// Table name.
    pub fn table_name(&self) -> &str {
        self.config.table_name()
    }

    /// Vector dimensions.
    pub fn dimensions(&self) -> usize {
        self.config.dimensions()
    }

    /// Distance metric.
    pub fn distance_type(&self) -> PgVectorDistanceType {
        self.config.distance_type()
    }

    /// Whether [`close`](Self::close) has been called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Marks the store closed; every later operation fails with
    /// `VectorStore("PgVectorStore has been closed")`. Idempotent, like Java `close()`.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Ensures the extension, schema, table and indexes exist (idempotent DDL).
    pub async fn initialize(&self) -> Result<()> {
        let full_table = self.config.full_table_name();
        let wrap = |error: RagError| {
            RagError::VectorStore(format!(
                "Failed to ensure table exists: {full_table}: {error}"
            ))
        };

        self.executor
            .execute(CREATE_EXTENSION_SQL)
            .await
            .map_err(wrap)?;

        if !self.config.schema().eq_ignore_ascii_case(DEFAULT_SCHEMA) {
            self.executor
                .execute(&create_schema_sql(self.config.schema()))
                .await
                .map_err(wrap)?;
        }

        let exists = self
            .executor
            .query_count(&table_exists_sql(
                self.config.schema(),
                self.config.table_name(),
            ))
            .await
            .map_err(wrap)?
            > 0;
        if exists {
            return Ok(());
        }

        let statements = [
            create_table_sql(
                self.config.schema(),
                self.config.table_name(),
                self.config.dimensions(),
            ),
            create_doc_id_index_sql(self.config.schema(), self.config.table_name()),
            create_vector_index_sql(
                self.config.schema(),
                self.config.table_name(),
                self.config.distance_type(),
            ),
        ];
        for statement in statements {
            self.executor.execute(&statement).await.map_err(wrap)?;
        }
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        if self.is_closed() {
            return Err(RagError::VectorStore(
                "PgVectorStore has been closed".to_string(),
            ));
        }
        Ok(())
    }

    fn row_params(&self, document: &Document) -> Result<Vec<PgParam>> {
        let embedding = document.embedding.as_ref().ok_or_else(|| {
            RagError::InvalidArgument("Document must have embedding set".to_string())
        })?;
        let content = serde_json::to_string(&document.metadata.content)
            .unwrap_or_else(|_| document.metadata.content_text());
        let payload_map = document.metadata.payload_map();
        let payload = if payload_map.is_empty() {
            "{}".to_string()
        } else {
            serde_json::to_string(&Value::Object(payload_map.clone()))
                .unwrap_or_else(|_| "{}".to_string())
        };

        Ok(vec![
            PgParam::Text(document.id.clone()),
            PgParam::Vector(vector_literal(embedding)),
            PgParam::Text(document.metadata.doc_id.clone()),
            PgParam::Text(document.metadata.chunk_id.clone()),
            PgParam::Text(content),
            PgParam::Json(payload),
        ])
    }
}

#[async_trait]
impl VdbStore for PgVectorStore {
    async fn add(&self, documents: Vec<Document>) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        for document in &documents {
            let embedding = document.embedding.as_ref().ok_or_else(|| {
                RagError::InvalidArgument("Document must have embedding set".to_string())
            })?;
            if embedding.len() != self.config.dimensions() {
                return Err(RagError::VectorStore(format!(
                    "Embedding dimension mismatch: expected {}, got {}",
                    self.config.dimensions(),
                    embedding.len()
                )));
            }
        }

        self.ensure_open()?;
        let rows = documents
            .iter()
            .map(|document| self.row_params(document))
            .collect::<Result<Vec<_>>>()?;

        self.executor
            .execute_batch(
                &upsert_sql(self.config.schema(), self.config.table_name()),
                rows,
            )
            .await
            .map_err(|error| {
                RagError::VectorStore(format!("Failed to add documents to PostgreSQL: {error}"))
            })?;
        Ok(())
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
        let params = vec![
            PgParam::Vector(vector_literal(&query.query_embedding)),
            PgParam::Int(i32::try_from(query.limit).unwrap_or(i32::MAX)),
        ];
        let rows = self
            .executor
            .query(
                &search_sql(
                    self.config.schema(),
                    self.config.table_name(),
                    self.config.distance_type(),
                ),
                params,
            )
            .await
            .map_err(|error| {
                RagError::VectorStore(format!("Failed to search documents in PostgreSQL: {error}"))
            })?;

        let mut documents = Vec::with_capacity(rows.len());
        for row in rows {
            match row_to_document(&row, self.config.distance_type()) {
                Ok(document) => {
                    if let Some(threshold) = query.score_threshold {
                        if document.score.unwrap_or_default() < threshold {
                            continue;
                        }
                    }
                    documents.push(document);
                }
                Err(error) => tracing::warn!("failed to reconstruct pgvector row: {error}"),
            }
        }
        Ok(documents)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        if id.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Document ID cannot be null or empty".to_string(),
            ));
        }
        self.ensure_open()?;
        let deleted = self
            .executor
            .execute_update(
                &delete_sql(self.config.schema(), self.config.table_name()),
                vec![PgParam::Text(id.to_string())],
            )
            .await
            .map_err(|error| {
                RagError::VectorStore(format!(
                    "Failed to delete document from PostgreSQL: {error}"
                ))
            })?;
        Ok(deleted > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeState {
        executed: Vec<String>,
        batches: Vec<(String, Vec<Vec<PgParam>>)>,
        queries: Vec<(String, Vec<PgParam>)>,
        updates: Vec<(String, Vec<PgParam>)>,
        affected: u64,
        table_count: i64,
        rows: Vec<PgRow>,
        fail_query: Option<String>,
        fail_execute: Option<String>,
    }

    #[derive(Default)]
    struct FakeExecutor {
        state: Mutex<FakeState>,
    }

    impl FakeExecutor {
        fn with_table_count(count: i64) -> Arc<Self> {
            let executor = Arc::new(Self::default());
            executor.state.lock().unwrap().table_count = count;
            executor
        }

        fn executed(&self) -> Vec<String> {
            self.state.lock().unwrap().executed.clone()
        }

        fn batches(&self) -> Vec<(String, Vec<Vec<PgParam>>)> {
            self.state.lock().unwrap().batches.clone()
        }

        fn queries(&self) -> Vec<(String, Vec<PgParam>)> {
            self.state.lock().unwrap().queries.clone()
        }

        fn updates(&self) -> Vec<(String, Vec<PgParam>)> {
            self.state.lock().unwrap().updates.clone()
        }

        fn set_rows(&self, rows: Vec<PgRow>) {
            self.state.lock().unwrap().rows = rows;
        }

        fn set_affected(&self, affected: u64) {
            self.state.lock().unwrap().affected = affected;
        }

        fn fail_query(&self, message: &str) {
            self.state.lock().unwrap().fail_query = Some(message.to_string());
        }

        fn fail_execute(&self, message: &str) {
            self.state.lock().unwrap().fail_execute = Some(message.to_string());
        }
    }

    #[async_trait]
    impl PgExecutor for FakeExecutor {
        async fn execute(&self, sql: &str) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = &state.fail_execute {
                return Err(RagError::VectorStore(message.clone()));
            }
            state.executed.push(sql.to_string());
            Ok(())
        }

        async fn execute_batch(&self, sql: &str, rows: Vec<Vec<PgParam>>) -> Result<u64> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = &state.fail_execute {
                return Err(RagError::VectorStore(message.clone()));
            }
            let affected = rows.len() as u64;
            state.batches.push((sql.to_string(), rows));
            Ok(affected)
        }

        async fn query(&self, sql: &str, params: Vec<PgParam>) -> Result<Vec<PgRow>> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = &state.fail_query {
                return Err(RagError::VectorStore(message.clone()));
            }
            state.queries.push((sql.to_string(), params));
            Ok(state.rows.clone())
        }

        async fn query_count(&self, _sql: &str) -> Result<i64> {
            Ok(self.state.lock().unwrap().table_count)
        }

        async fn execute_update(&self, sql: &str, params: Vec<PgParam>) -> Result<u64> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = &state.fail_execute {
                return Err(RagError::VectorStore(message.clone()));
            }
            state.updates.push((sql.to_string(), params));
            Ok(state.affected)
        }
    }

    fn config() -> PgVectorConfig {
        PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://localhost:5432/vectordb")
            .username("postgres")
            .password("secret")
            .table_name("embeddings")
            .dimensions(2)
            .build()
            .unwrap()
    }

    fn doc(id: &str, embedding: Vec<f64>) -> Document {
        let mut document =
            Document::with_id(id, DocumentMetadata::text("hello", "doc-1", "chunk-1"));
        document.set_embedding(embedding);
        document
    }

    fn row(doc_id: &str, distance: f64) -> PgRow {
        PgRow {
            id: "ignored".to_string(),
            embedding: Some("[1.0,0.0]".to_string()),
            doc_id: doc_id.to_string(),
            chunk_id: "chunk-1".to_string(),
            content: "{\"type\":\"text\",\"text\":\"hello\"}".to_string(),
            payload: "{\"src\":\"web\"}".to_string(),
            distance,
        }
    }

    #[test]
    fn builder_validates_required_fields_with_java_messages() {
        let error = PgVectorConfig::builder()
            .table_name("embeddings")
            .dimensions(2)
            .build()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("JDBC URL cannot be null or empty"));

        let error = PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://localhost:5432/db")
            .dimensions(2)
            .build()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Table name cannot be null or empty"));

        let error = PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://localhost:5432/db")
            .table_name("embeddings")
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("Dimensions must be positive"));
    }

    #[test]
    fn builder_rejects_unsafe_identifiers() {
        let error = PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://localhost:5432/db")
            .table_name("embeddings; DROP TABLE users")
            .dimensions(2)
            .build()
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Table name contains invalid characters"));
        assert!(message.contains("Invalid value: embeddings; DROP TABLE users"));

        let error = validate_identifier("9lives", "Table name").unwrap_err();
        assert!(error
            .to_string()
            .contains("Table name contains invalid characters"));

        let long = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        let error = validate_identifier(&long, "Schema name").unwrap_err();
        assert!(error
            .to_string()
            .contains("Schema name cannot exceed 63 characters"));

        assert!(validate_identifier("public_2", "Schema name").is_ok());
    }

    #[test]
    fn empty_schema_falls_back_to_public() {
        let config = PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://localhost:5432/db")
            .table_name("embeddings")
            .dimensions(2)
            .schema("")
            .build()
            .unwrap();
        assert_eq!(config.schema(), DEFAULT_SCHEMA);
        assert_eq!(config.full_table_name(), "public.embeddings");
    }

    #[test]
    fn parse_jdbc_url_splits_all_parts() {
        let location =
            parse_jdbc_url("jdbc:postgresql://db.internal:6543/vectors?user=alice&password=p%20w")
                .unwrap();
        assert_eq!(location.host.as_deref(), Some("db.internal"));
        assert_eq!(location.port, Some(6543));
        assert_eq!(location.database.as_deref(), Some("vectors"));
        assert_eq!(location.user.as_deref(), Some("alice"));
        assert_eq!(location.password.as_deref(), Some("p%20w"));

        let location = parse_jdbc_url("jdbc:postgresql://localhost/mydb").unwrap();
        assert_eq!(location.host.as_deref(), Some("localhost"));
        assert_eq!(location.port, None);
        assert_eq!(location.database.as_deref(), Some("mydb"));

        let location = parse_jdbc_url("jdbc:mysql://localhost/db").unwrap_err();
        assert!(location
            .to_string()
            .contains("JDBC URL must start with 'jdbc:postgresql://'"));
    }

    #[test]
    fn connection_string_derives_from_the_jdbc_url() {
        let config = PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://db.internal:6543/vectors")
            .username("postgres")
            .password("secret")
            .table_name("embeddings")
            .dimensions(2)
            .connection_timeout_ms(45_000)
            .build()
            .unwrap();
        assert_eq!(
            config.connection_string().unwrap(),
            "host=db.internal port=6543 dbname=vectors user=postgres password=secret connect_timeout=45"
        );

        let config = PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://localhost/db")
            .table_name("embeddings")
            .dimensions(2)
            .connection_string("host=localhost dbname=db")
            .build()
            .unwrap();
        assert_eq!(
            config.connection_string().unwrap(),
            "host=localhost dbname=db"
        );
    }

    #[test]
    fn ddl_matches_the_java_schema() {
        assert_eq!(
            create_table_sql("public", "embeddings", 1024),
            "CREATE TABLE IF NOT EXISTS public.embeddings (id VARCHAR(64) PRIMARY KEY, embedding vector(1024), doc_id VARCHAR(256), chunk_id VARCHAR(256), content TEXT, payload JSONB)"
        );
        assert_eq!(
            create_doc_id_index_sql("public", "embeddings"),
            "CREATE INDEX IF NOT EXISTS idx_public_embeddings_doc_id ON public.embeddings (doc_id)"
        );
        assert_eq!(
            create_vector_index_sql("public", "embeddings", PgVectorDistanceType::Cosine),
            "CREATE INDEX IF NOT EXISTS idx_public_embeddings_vector ON public.embeddings USING hnsw (embedding vector_cosine_ops)"
        );
        assert_eq!(
            create_schema_sql("my_schema"),
            "CREATE SCHEMA IF NOT EXISTS my_schema"
        );
        assert_eq!(
            table_exists_sql("Public", "Embeddings"),
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'embeddings'"
        );
    }

    #[test]
    fn dml_uses_the_configured_distance_operator() {
        assert_eq!(
            search_sql("public", "embeddings", PgVectorDistanceType::Cosine),
            "SELECT id, embedding::text AS embedding, doc_id, chunk_id, content, payload::text AS payload, (embedding <=> $1::vector) as distance FROM public.embeddings ORDER BY distance ASC LIMIT $2"
        );
        assert!(search_sql("public", "t", PgVectorDistanceType::L2)
            .contains("(embedding <-> $1::vector)"));
        assert!(
            search_sql("public", "t", PgVectorDistanceType::InnerProduct)
                .contains("(embedding <#> $1::vector)")
        );

        let upsert = upsert_sql("public", "embeddings");
        assert!(upsert.starts_with(
            "INSERT INTO public.embeddings (id, embedding, doc_id, chunk_id, content, payload) VALUES ($1, $2::vector, $3, $4, $5, $6::jsonb) ON CONFLICT (id) DO UPDATE SET embedding = EXCLUDED.embedding"
        ));
        assert!(upsert.contains("payload = EXCLUDED.payload"));

        assert_eq!(
            delete_sql("public", "embeddings"),
            "DELETE FROM public.embeddings WHERE doc_id = $1"
        );
    }

    #[test]
    fn vector_literals_round_trip() {
        let literal = vector_literal(&[1.0, 0.5, -0.25]);
        assert_eq!(literal, "[1.0,0.5,-0.25]");
        assert_eq!(
            parse_vector_literal(&literal).unwrap(),
            vec![1.0, 0.5, -0.25]
        );
        assert_eq!(parse_vector_literal("[]").unwrap(), Vec::<f64>::new());
        assert_eq!(parse_vector_literal(" [1, 2] ").unwrap(), vec![1.0, 2.0]);
        assert!(parse_vector_literal("[1,oops]").is_err());
    }

    #[test]
    fn distance_to_score_matches_java() {
        assert!((distance_to_score(0.25, PgVectorDistanceType::Cosine) - 0.75).abs() < 1e-12);
        assert!((distance_to_score(3.0, PgVectorDistanceType::L2) - 0.25).abs() < 1e-12);
        assert!((distance_to_score(-0.8, PgVectorDistanceType::InnerProduct) - 0.8).abs() < 1e-12);
    }

    #[tokio::test]
    async fn initialize_creates_extension_schema_table_and_indexes() {
        let executor = FakeExecutor::with_table_count(0);
        let config = PgVectorConfig::builder()
            .jdbc_url("jdbc:postgresql://localhost:5432/db")
            .schema("my_schema")
            .table_name("embeddings")
            .dimensions(3)
            .build()
            .unwrap();
        let store = PgVectorStore::connect_with(config, executor.clone())
            .await
            .unwrap();

        assert_eq!(
            executor.executed(),
            vec![
                CREATE_EXTENSION_SQL.to_string(),
                "CREATE SCHEMA IF NOT EXISTS my_schema".to_string(),
                create_table_sql("my_schema", "embeddings", 3),
                create_doc_id_index_sql("my_schema", "embeddings"),
                create_vector_index_sql("my_schema", "embeddings", PgVectorDistanceType::Cosine),
            ]
        );
        assert_eq!(store.schema(), "my_schema");
        assert_eq!(store.table_name(), "embeddings");
        assert_eq!(store.dimensions(), 3);
    }

    #[tokio::test]
    async fn initialize_skips_table_creation_when_present() {
        let executor = FakeExecutor::with_table_count(1);
        PgVectorStore::connect_with(config(), executor.clone())
            .await
            .unwrap();
        assert_eq!(executor.executed(), vec![CREATE_EXTENSION_SQL.to_string()]);
    }

    #[tokio::test]
    async fn initialize_wraps_ddl_failures() {
        let executor = FakeExecutor::with_table_count(0);
        executor.fail_execute("permission denied");
        let error = PgVectorStore::connect_with(config(), executor)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Failed to ensure table exists: public.embeddings"));
        assert!(message.contains("permission denied"));
    }

    #[tokio::test]
    async fn add_upserts_every_document() {
        let executor = FakeExecutor::with_table_count(1);
        let store = PgVectorStore::new(config(), executor.clone());
        let metadata = DocumentMetadata::with_payload(
            ContentBlock::Text(TextBlock::new("hello")),
            "doc-1",
            "chunk-1",
            serde_json::json!({"src": "web"}),
        );
        let mut document = Document::with_id("11111111-2222-3333-4444-555555555555", metadata);
        document.set_embedding(vec![1.0, 0.5]);
        store.add(vec![document]).await.unwrap();

        let batches = executor.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].0, upsert_sql("public", "embeddings"));
        let params = &batches[0].1[0];
        assert_eq!(
            params[0],
            PgParam::Text("11111111-2222-3333-4444-555555555555".into())
        );
        assert_eq!(params[1], PgParam::Vector("[1.0,0.5]".into()));
        assert_eq!(params[2], PgParam::Text("doc-1".into()));
        assert_eq!(params[3], PgParam::Text("chunk-1".into()));
        assert_eq!(
            params[4],
            PgParam::Text("{\"type\":\"text\",\"text\":\"hello\"}".into())
        );
        assert_eq!(params[5], PgParam::Json("{\"src\":\"web\"}".into()));
    }

    #[tokio::test]
    async fn add_defaults_payload_to_an_empty_object() {
        let executor = FakeExecutor::with_table_count(1);
        let store = PgVectorStore::new(config(), executor.clone());
        store
            .add(vec![doc(
                "11111111-2222-3333-4444-555555555555",
                vec![1.0, 0.0],
            )])
            .await
            .unwrap();
        let batches = executor.batches();
        assert_eq!(batches[0].1[0][5], PgParam::Json("{}".into()));
    }

    #[tokio::test]
    async fn add_validates_embeddings_and_skips_empty_batches() {
        let executor = FakeExecutor::with_table_count(1);
        let store = PgVectorStore::new(config(), executor.clone());

        let error = store
            .add(vec![Document::with_id(
                "11111111-2222-3333-4444-555555555555",
                DocumentMetadata::text("x", "d", "c"),
            )])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Document must have embedding set"));

        let error = store
            .add(vec![doc(
                "11111111-2222-3333-4444-555555555555",
                vec![1.0, 2.0, 3.0],
            )])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Embedding dimension mismatch: expected 2, got 3"));

        store.add(vec![]).await.unwrap();
        assert!(executor.batches().is_empty());
    }

    #[tokio::test]
    async fn add_wraps_sql_errors() {
        let executor = FakeExecutor::with_table_count(1);
        executor.fail_execute("duplicate key");
        let store = PgVectorStore::new(config(), executor);
        let error = store
            .add(vec![doc(
                "11111111-2222-3333-4444-555555555555",
                vec![1.0, 0.0],
            )])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Failed to add documents to PostgreSQL: vector store error: duplicate key"));
    }

    #[tokio::test]
    async fn search_maps_rows_to_scored_documents() {
        let executor = FakeExecutor::with_table_count(1);
        executor.set_rows(vec![row("doc-1", 0.2), row("doc-2", 0.6)]);
        let store = PgVectorStore::new(config(), executor.clone());

        let results = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 2))
            .await
            .unwrap();

        let queries = executor.queries();
        assert_eq!(
            queries[0].0,
            search_sql("public", "embeddings", PgVectorDistanceType::Cosine)
        );
        assert_eq!(queries[0].1[0], PgParam::Vector("[1.0,0.0]".into()));
        assert_eq!(queries[0].1[1], PgParam::Int(2));

        assert_eq!(results.len(), 2);
        assert!((results[0].score.unwrap() - 0.8).abs() < 1e-12);
        assert!((results[1].score.unwrap() - 0.4).abs() < 1e-12);
        assert_eq!(results[0].metadata.doc_id, "doc-1");
        assert_eq!(results[0].metadata.content_text(), "hello");
        assert_eq!(
            results[0].payload_value("src"),
            Some(&serde_json::json!("web"))
        );
        assert_eq!(results[0].embedding, Some(vec![1.0, 0.0]));
    }

    #[tokio::test]
    async fn search_applies_the_score_threshold() {
        let executor = FakeExecutor::with_table_count(1);
        executor.set_rows(vec![row("doc-1", 0.2), row("doc-2", 0.9)]);
        let store = PgVectorStore::new(config(), executor);
        let results = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 2).with_score_threshold(0.5))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata.doc_id, "doc-1");
    }

    #[tokio::test]
    async fn search_validates_dimensions_and_limit() {
        let executor = FakeExecutor::with_table_count(1);
        let store = PgVectorStore::new(config(), executor.clone());
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
        assert!(executor.queries().is_empty());
    }

    #[tokio::test]
    async fn search_wraps_sql_errors() {
        let executor = FakeExecutor::with_table_count(1);
        executor.fail_query("connection reset");
        let store = PgVectorStore::new(config(), executor);
        let error = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(
            "Failed to search documents in PostgreSQL: vector store error: connection reset"
        ));
    }

    #[tokio::test]
    async fn delete_targets_the_doc_id_column() {
        let executor = FakeExecutor::with_table_count(1);
        executor.set_affected(2);
        let store = PgVectorStore::new(config(), executor.clone());
        assert!(store.delete("doc-1").await.unwrap());

        let updates = executor.updates();
        assert_eq!(updates[0].0, delete_sql("public", "embeddings"));
        assert_eq!(updates[0].1, vec![PgParam::Text("doc-1".into())]);

        executor.set_affected(0);
        assert!(!store.delete("doc-2").await.unwrap());
    }

    #[tokio::test]
    async fn delete_validates_the_id() {
        let executor = FakeExecutor::with_table_count(1);
        let store = PgVectorStore::new(config(), executor.clone());
        let error = store.delete("  ").await.unwrap_err();
        assert!(error
            .to_string()
            .contains("Document ID cannot be null or empty"));
        assert!(executor.updates().is_empty());
    }

    #[tokio::test]
    async fn closed_store_rejects_operations() {
        let executor = FakeExecutor::with_table_count(1);
        let store = PgVectorStore::new(config(), executor.clone());
        store.close();
        store.close();
        assert!(store.is_closed());

        let error = store
            .search(&SearchQuery::new(vec![1.0, 0.0], 1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("PgVectorStore has been closed"));
        let error = store
            .add(vec![doc(
                "11111111-2222-3333-4444-555555555555",
                vec![1.0, 0.0],
            )])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("PgVectorStore has been closed"));
        assert!(store.delete("doc-1").await.is_err());
        assert!(executor.queries().is_empty());
        assert!(executor.batches().is_empty());
        assert!(executor.updates().is_empty());
    }

    #[test]
    fn row_to_document_falls_back_on_non_json_content_and_payload() {
        let row = PgRow {
            id: "ignored".to_string(),
            embedding: Some("not a vector".to_string()),
            doc_id: "doc-1".to_string(),
            chunk_id: "chunk-1".to_string(),
            content: "plain text chunk".to_string(),
            payload: "not json".to_string(),
            distance: 0.1,
        };
        let document = row_to_document(&row, PgVectorDistanceType::Cosine).unwrap();
        assert_eq!(document.metadata.content_text(), "plain text chunk");
        assert!(document.payload().is_empty());
        assert_eq!(document.embedding, None);
        assert!((document.score.unwrap() - 0.9).abs() < 1e-12);
    }

    #[test]
    fn row_to_document_handles_empty_payload() {
        let row = PgRow {
            payload: "{}".to_string(),
            ..row("doc-3", 0.5)
        };
        let document = row_to_document(&row, PgVectorDistanceType::L2).unwrap();
        assert!(document.payload().is_empty());
        assert!((document.score.unwrap() - 1.0 / 1.5).abs() < 1e-12);
    }

    #[test]
    fn document_ids_stay_stable_across_the_round_trip() {
        let metadata = DocumentMetadata::text("hello", "doc-1", "chunk-1");
        let original = Document::new(metadata);
        let row = PgRow {
            content: serde_json::to_string(&original.metadata.content).unwrap(),
            ..row("doc-1", 0.0)
        };
        let rebuilt = row_to_document(&row, PgVectorDistanceType::Cosine).unwrap();
        assert_eq!(rebuilt.id, original.id);
    }
}
