//! PostgreSQL-backed [`BaseStore`] implementation.
//!
//! Mirrors the Java `PostgresBaseStore`: a namespace-based KV table with optimistic
//! concurrency (`put_if_version`), `LIKE ... ESCAPE '!'` prefix search, and the
//! ASCII unit-separator namespace encoding (see [`crate::sql`]).

use std::sync::Arc;

use agentscope_harness::filesystem::{BaseStore, StoreItem, StoreValue};

use crate::client::{Cell, PostgresClient, QueryOutcome, SqlValue};
use crate::error::{PgResult, PostgresError};
use crate::sql::{
    like_prefix_pattern, namespace_path, qualified_table, store_ddl, validate_identifier,
};

/// Default table name, matching the Java `DEFAULT_TABLE_NAME`.
pub const DEFAULT_TABLE_NAME: &str = "agentscope_store";

/// Builder for [`PostgresStore`].
#[derive(Debug, Clone)]
pub struct PostgresStoreBuilder {
    client: Arc<dyn PostgresClient>,
    schema: Option<String>,
    table: String,
    initialize_schema: bool,
}

impl PostgresStoreBuilder {
    /// Creates a builder over a SQL client.
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self {
            client,
            schema: None,
            table: DEFAULT_TABLE_NAME.to_string(),
            initialize_schema: false,
        }
    }

    /// Sets a PostgreSQL schema name. When unset the table name is unqualified.
    pub fn schema(mut self, schema: impl Into<String>) -> PgResult<Self> {
        self.schema = Some(validate_identifier(&schema.into(), "schemaName")?);
        Ok(self)
    }

    /// Overrides the table name.
    pub fn table(mut self, table: impl Into<String>) -> PgResult<Self> {
        self.table = validate_identifier(&table.into(), "tableName")?;
        Ok(self)
    }

    /// When `true`, runs `CREATE SCHEMA/TABLE IF NOT EXISTS` on build.
    pub fn initialize_schema(mut self, initialize: bool) -> Self {
        self.initialize_schema = initialize;
        self
    }

    /// Builds the store, initializing the schema when requested.
    pub fn build(self) -> PgResult<PostgresStore> {
        let full_table = qualified_table(self.schema.as_deref(), &self.table);
        if self.initialize_schema {
            let mut ddl = String::new();
            if let Some(s) = &self.schema {
                ddl.push_str(&format!("CREATE SCHEMA IF NOT EXISTS \"{s}\";\n"));
            }
            ddl.push_str(&store_ddl(&full_table));
            self.client.execute_batch(&ddl)?;
        }
        Ok(PostgresStore {
            client: self.client,
            schema: self.schema,
            table: self.table,
            full_table,
        })
    }
}

/// PostgreSQL-backed [`BaseStore`].
#[derive(Debug, Clone)]
pub struct PostgresStore {
    client: Arc<dyn PostgresClient>,
    schema: Option<String>,
    table: String,
    full_table: String,
}

impl PostgresStore {
    /// Creates a builder over the given client.
    pub fn builder(client: Arc<dyn PostgresClient>) -> PostgresStoreBuilder {
        PostgresStoreBuilder::new(client)
    }

    /// Creates a store with schema initialization enabled (mirrors
    /// `PostgresBaseStore.builder(ds).initializeSchema(true).build()`).
    pub fn with_schema_init(client: Arc<dyn PostgresClient>) -> PgResult<Self> {
        PostgresStore::builder(client).initialize_schema(true).build()
    }

    pub fn schema_name(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    pub fn table_name(&self) -> &str {
        &self.table
    }

    fn serialize(value: &StoreValue) -> PgResult<String> {
        serde_json::to_string(value)
            .map_err(|e| PostgresError::Encode(format!("Failed to JSON-encode store value: {e}")))
    }

    fn deserialize(json: &str) -> PgResult<StoreValue> {
        if json.is_empty() {
            return Ok(StoreValue::new());
        }
        serde_json::from_str(json)
            .map_err(|e| PostgresError::Decode(format!("Failed to JSON-decode store value: {e}")))
    }

    fn validate_key(key: &str) -> PgResult<()> {
        if key.is_empty() {
            return Err(PostgresError::InvalidIdentifier(
                "key must not be null or empty".into(),
            ));
        }
        Ok(())
    }
}

impl BaseStore for PostgresStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        let ns_path = namespace_path(namespace).ok()?;
        let sql = format!(
            "SELECT value_json, version FROM {} WHERE namespace_path = $1 AND item_key = $2",
            self.full_table
        );
        let params = [SqlValue::text(ns_path), SqlValue::text(key)];
        match self.client.query(&sql, &params).ok()? {
            QueryOutcome::Rows(rows) => rows.first().and_then(|row| {
                let json = row.first().and_then(Cell::as_str)?;
                let version = row.get(1).and_then(Cell::as_i64).unwrap_or(0) as u64;
                let value = Self::deserialize(json).ok()?;
                Some(StoreItem::new(key, value, version))
            }),
            _ => None,
        }
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        if Self::validate_key(key).is_err() {
            return;
        }
        let Ok(ns_path) = namespace_path(namespace) else {
            return;
        };
        let Ok(json) = Self::serialize(&value) else {
            return;
        };
        let sql = format!(
            "INSERT INTO {} (namespace_path, item_key, value_json, version, updated_at) \
             VALUES ($1, $2, $3, 1, $4) \
             ON CONFLICT (namespace_path, item_key) DO UPDATE SET \
                 value_json = EXCLUDED.value_json, \
                 version    = {}.version + 1, \
                 updated_at = EXCLUDED.updated_at",
            self.full_table, self.full_table
        );
        let now = chrono::Utc::now().timestamp_millis();
        let params = [
            SqlValue::text(ns_path),
            SqlValue::text(key),
            SqlValue::text(json),
            SqlValue::BigInt(now),
        ];
        let _ = self.client.query(&sql, &params);
    }

    fn put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> bool {
        if Self::validate_key(key).is_err() {
            return false;
        }
        let Ok(ns_path) = namespace_path(namespace) else {
            return false;
        };
        let Ok(json) = Self::serialize(&value) else {
            return false;
        };
        let now = chrono::Utc::now().timestamp_millis();

        if expected_version == 0 {
            // INSERT, treat duplicate-key as a failed CAS.
            let sql = format!(
                "INSERT INTO {} (namespace_path, item_key, value_json, version, updated_at) \
                 VALUES ($1, $2, $3, 1, $4)",
                self.full_table
            );
            let params = [
                SqlValue::text(ns_path),
                SqlValue::text(key),
                SqlValue::text(json),
                SqlValue::BigInt(now),
            ];
            return match self.client.query(&sql, &params) {
                Ok(QueryOutcome::Affected(n)) => n == 1,
                // The `INSERT ... ON CONFLICT` path also matches an upsert row count.
                Ok(QueryOutcome::Rows(_)) => false,
                // A duplicate key means the key already existed: CAS correctly fails.
                Err(e) if is_duplicate_key(&e) => false,
                // Any other error must NOT be reported as a successful CAS.
                Err(e) => {
                    tracing::warn!("PostgresStore put_if_version(insert) failed: {e}");
                    false
                }
            };
        }

        let sql = format!(
            "UPDATE {} SET value_json = $1, version = version + 1, updated_at = $2 \
             WHERE namespace_path = $3 AND item_key = $4 AND version = $5",
            self.full_table
        );
        let params = [
            SqlValue::text(json),
            SqlValue::BigInt(now),
            SqlValue::text(ns_path),
            SqlValue::text(key),
            SqlValue::BigInt(expected_version as i64),
        ];
        matches!(
            self.client.query(&sql, &params),
            Ok(QueryOutcome::Affected(1))
        )
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        if limit == 0 {
            return Vec::new();
        }
        let Ok(pattern) = like_prefix_pattern(namespace) else {
            return Vec::new();
        };
        let sql = format!(
            "SELECT item_key, value_json, version FROM {} \
             WHERE namespace_path LIKE $1 ESCAPE '!' \
             ORDER BY item_key LIMIT $2 OFFSET $3",
            self.full_table
        );
        let params = [
            SqlValue::text(pattern),
            SqlValue::BigInt(limit as i64),
            SqlValue::BigInt(offset as i64),
        ];
        match self.client.query(&sql, &params) {
            Ok(QueryOutcome::Rows(rows)) => rows
                .into_iter()
                .filter_map(|row| {
                    let key = row.first().and_then(Cell::as_str)?.to_string();
                    let json = row.get(1).and_then(Cell::as_str)?;
                    let version = row.get(2).and_then(Cell::as_i64).unwrap_or(0) as u64;
                    let value = Self::deserialize(json).ok()?;
                    Some(StoreItem::new(key, value, version))
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn delete(&self, namespace: &[String], key: &str) {
        if Self::validate_key(key).is_err() {
            return;
        }
        let Ok(ns_path) = namespace_path(namespace) else {
            return;
        };
        let sql = format!(
            "DELETE FROM {} WHERE namespace_path = $1 AND item_key = $2",
            self.full_table
        );
        let params = [SqlValue::text(ns_path), SqlValue::text(key)];
        let _ = self.client.query(&sql, &params);
    }
}

/// Heuristic duplicate-key detection (mirrors the Java `isDuplicateKey`).
fn is_duplicate_key(e: &PostgresError) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("duplicate key") || msg.contains("23505") || msg.contains("unique constraint")
}
