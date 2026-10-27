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
        PostgresStore::builder(client)
            .initialize_schema(true)
            .build()
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

    /// Conditional row update / insert-if-absent: atomic in the database (Java
    /// `supportsAtomicSessionStorage`).
    fn supports_atomic_session_storage(&self) -> bool {
        true
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

#[cfg(test)]
mod tests {
    //! Offline unit tests mirroring `PostgresBaseStoreTest` (JUnit5 + Mockito + H2 on the
    //! Java side). [`MemoryClient`] replaces H2 here: it understands the exact statement
    //! shapes emitted above, including `ON CONFLICT` upserts, conditional CAS updates and
    //! `LIKE ... ESCAPE '!'` prefix search.

    use super::*;
    use crate::memory::MemoryClient;
    use crate::sql::NS_SEPARATOR;
    use serde_json::json;

    fn client() -> Arc<MemoryClient> {
        Arc::new(MemoryClient::new())
    }

    fn store(client: &Arc<MemoryClient>) -> PostgresStore {
        PostgresStore::with_schema_init(client.clone()).unwrap()
    }

    fn ns(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn val(v: serde_json::Value) -> StoreValue {
        v.as_object().expect("test value must be an object").clone()
    }

    // ---- builder ------------------------------------------------------------

    // Java: PostgresBaseStoreTest#builderWithDefaultsCreatesStore
    #[test]
    fn builder_defaults_unqualified_default_table() {
        let c = client();
        let s = PostgresStore::builder(c).build().unwrap();
        assert_eq!(s.table_name(), DEFAULT_TABLE_NAME);
        assert!(s.schema_name().is_none());
    }

    // Java: PostgresBaseStoreTest#builderWithCustomTableName,
    //       #builderWithCustomSchemaNameUsesQualifiedTableName
    #[test]
    fn builder_custom_schema_and_table_qualified() {
        let c = client();
        let s = PostgresStore::builder(c.clone())
            .schema("custom")
            .unwrap()
            .table("custom_kv")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(s.schema_name(), Some("custom"));
        assert_eq!(s.table_name(), "custom_kv");
        assert_eq!(s.full_table, "\"custom\".\"custom_kv\"");
        // initialize_schema is off by default: nothing provisioned yet.
        assert!(!c.has_table("custom_kv"));
    }

    // Java: PostgresBaseStoreTest#builderRejectsInvalidTableName, #builderRejectsInvalidSchemaName
    #[test]
    fn builder_rejects_invalid_names() {
        let c = client();
        assert!(PostgresStore::builder(c.clone()).table("bad name").is_err());
        assert!(PostgresStore::builder(c.clone()).table("1bad").is_err());
        assert!(PostgresStore::builder(c).schema("bad-name").is_err());
    }

    // Java: PostgresBaseStoreTest#initializeSchemaDefaultDoesNotCreateTable,
    //       #initializeSchemaTrueCreatesTable, #initializeSchemaTrueCreatesCustomSchemaAndTable
    #[test]
    fn initialize_schema_creates_schema_and_table() {
        let c = client();
        let _ = PostgresStore::builder(c.clone()).build().unwrap();
        assert!(!c.has_table(DEFAULT_TABLE_NAME));

        let c2 = client();
        let _ = PostgresStore::with_schema_init(c2.clone()).unwrap();
        assert!(c2.has_table(DEFAULT_TABLE_NAME));
        assert!(c2
            .ddl_statements()
            .iter()
            .any(|s| s.contains("CREATE TABLE IF NOT EXISTS")));

        let c3 = client();
        let _ = PostgresStore::builder(c3.clone())
            .schema("custom")
            .unwrap()
            .table("kv")
            .unwrap()
            .initialize_schema(true)
            .build()
            .unwrap();
        assert!(c3.has_schema("custom"));
        assert!(c3.has_table("kv"));
    }

    // ---- get / put ----------------------------------------------------------

    // Java: PostgresBaseStoreTest#getReturnsItem
    #[test]
    fn get_returns_item_with_version() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        s.put(&n, "key1", val(json!({"a": 1})));
        let item = s.get(&n, "key1").expect("item present");
        assert_eq!(item.key, "key1");
        assert_eq!(item.version, 1);
        assert_eq!(item.value.get("a"), Some(&json!(1)));
    }

    // Java: PostgresBaseStoreTest#getReturnsNullWhenNotFound
    #[test]
    fn get_returns_none_when_missing() {
        let c = client();
        let s = store(&c);
        assert!(s.get(&ns(&["filesystem"]), "nope").is_none());
    }

    // Java: PostgresBaseStoreTest#putWritesItem (version increments per write)
    #[test]
    fn put_upserts_and_increments_version() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        s.put(&n, "k", val(json!({"v": 1})));
        assert_eq!(s.get(&n, "k").unwrap().version, 1);
        s.put(&n, "k", val(json!({"v": 2})));
        let item = s.get(&n, "k").unwrap();
        assert_eq!(item.version, 2);
        assert_eq!(item.value.get("v"), Some(&json!(2)));
        assert_eq!(c.store_row_count(), 1);
    }

    // Java: PostgresBaseStoreTest#deserializeHandlesNullAndEmpty
    #[test]
    fn get_decodes_empty_json_as_empty_map() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        c.query(
            &format!(
                "INSERT INTO {} (namespace_path, item_key, value_json, version, updated_at) \
                 VALUES ($1, $2, $3, 1, $4)",
                s.full_table
            ),
            &[
                SqlValue::text(crate::sql::namespace_path(&n).unwrap()),
                SqlValue::text("k"),
                SqlValue::text(""),
                SqlValue::BigInt(0),
            ],
        )
        .unwrap();
        let item = s.get(&n, "k").unwrap();
        assert!(item.value.is_empty());
    }

    // Java: PostgresBaseStoreTest#keyCannotBeNullOrEmpty
    #[test]
    fn empty_key_is_ignored() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        s.put(&n, "", val(json!({"a": 1})));
        assert_eq!(c.store_row_count(), 0);
        assert!(s.get(&n, "").is_none());
    }

    // ---- CAS ----------------------------------------------------------------

    // Java: PostgresBaseStoreTest#putIfVersionInsertSucceeds
    #[test]
    fn put_if_version_zero_creates_then_fails_on_duplicate() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        assert!(s.put_if_version(&n, "k", val(json!({"v": 1})), 0));
        assert_eq!(s.get(&n, "k").unwrap().version, 1);
        // Duplicate key on the create-only branch: CAS correctly fails.
        assert!(!s.put_if_version(&n, "k", val(json!({"v": 2})), 0));
        assert_eq!(s.get(&n, "k").unwrap().value.get("v"), Some(&json!(1)));
    }

    // Java: PostgresBaseStoreTest#putIfVersionUpdateSucceeds
    #[test]
    fn put_if_version_match_updates_row() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        s.put(&n, "k", val(json!({"v": 1})));
        assert!(s.put_if_version(&n, "k", val(json!({"v": 2})), 1));
        let item = s.get(&n, "k").unwrap();
        assert_eq!(item.version, 2);
        assert_eq!(item.value.get("v"), Some(&json!(2)));
    }

    // Java: PostgresBaseStoreTest#putIfVersionUpdateMismatchReturnsFalse
    #[test]
    fn put_if_version_mismatch_fails() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        s.put(&n, "k", val(json!({"v": 1})));
        assert!(!s.put_if_version(&n, "k", val(json!({"v": 2})), 5));
        // A non-existent key also fails the conditional-update branch.
        assert!(!s.put_if_version(&n, "absent", val(json!({})), 1));
        assert_eq!(s.get(&n, "k").unwrap().value.get("v"), Some(&json!(1)));
    }

    // ---- search -------------------------------------------------------------

    // Java: PostgresBaseStoreTest#searchReturnsItems
    #[test]
    fn search_returns_items_in_the_namespace_sorted_by_key() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem", "docs"]);
        s.put(&n, "b", val(json!({"i": 2})));
        s.put(&n, "a", val(json!({"i": 1})));
        let hits = s.search(&n, 10, 0);
        assert_eq!(
            hits.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        // Pagination.
        let page = s.search(&n, 1, 1);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].key, "b");
    }

    // Java: PostgresBaseStoreTest#searchReturnsEmptyForNonPositiveLimit
    #[test]
    fn search_with_zero_limit_returns_nothing() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        s.put(&n, "k", val(json!({})));
        assert!(s.search(&n, 0, 0).is_empty());
    }

    // Java: PostgresBaseStoreTest#searchEscapesLikeSpecialCharacters — plus the US(0x1F)
    // namespace encoding (trailing separator lets `LIKE 'prefix%'` match sub-namespaces).
    #[test]
    fn search_prefix_uses_encoded_namespace_and_isolates_siblings() {
        let c = client();
        let s = store(&c);
        let root = ns(&["root"]);
        let child = ns(&["root", "child"]);
        let sibling = ns(&["root2"]);
        s.put(&root, "r", val(json!({"scope": "root"})));
        s.put(&child, "c", val(json!({"scope": "child"})));
        s.put(&sibling, "x", val(json!({"scope": "sibling"})));

        // The encoded path stores a trailing separator.
        let encoded = namespace_path(&child).unwrap();
        assert!(encoded.contains(NS_SEPARATOR));
        assert_eq!(encoded.matches(NS_SEPARATOR).count(), 2);

        // A prefix search on `root` sees its own item and descendants, but not `root2`.
        let hits = s.search(&root, 10, 0);
        let keys: Vec<&str> = hits.iter().map(|i| i.key.as_str()).collect();
        assert!(keys.contains(&"r"));
        assert!(keys.contains(&"c"));
        assert!(!keys.contains(&"x"));
    }

    // Java: PostgresBaseStoreTest#searchEscapesLikeSpecialCharacters
    #[test]
    fn like_pattern_escapes_metacharacters_in_segments() {
        let c = client();
        let s = store(&c);
        let n = ns(&["a%b", "c_d"]);
        s.put(&n, "k", val(json!({"ok": true})));
        let hits = s.search(&n, 10, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "k");
        // Escaped pattern round-trips through the LIKE matcher.
        let pat = like_prefix_pattern(&n).unwrap();
        assert!(pat.contains("!%"));
        assert!(pat.contains("!_"));
    }

    // Java: PostgresBaseStoreTest#namespaceSegmentCannotContainSeparator
    #[test]
    fn namespace_segment_with_separator_is_rejected() {
        let c = client();
        let s = store(&c);
        let bad = vec![format!("bad{NS_SEPARATOR}seg")];
        s.put(&bad, "k", val(json!({})));
        assert_eq!(c.store_row_count(), 0);
        assert!(s.get(&bad, "k").is_none());
        assert!(s.search(&bad, 10, 0).is_empty());
    }

    // ---- delete -------------------------------------------------------------

    // Java: PostgresBaseStoreTest#deleteRemovesItem
    #[test]
    fn delete_removes_item() {
        let c = client();
        let s = store(&c);
        let n = ns(&["filesystem"]);
        s.put(&n, "k", val(json!({"a": 1})));
        assert!(s.get(&n, "k").is_some());
        s.delete(&n, "k");
        assert!(s.get(&n, "k").is_none());
        assert_eq!(c.store_row_count(), 0);
    }

    // Java: PostgresBaseStoreTest#isDuplicateKey* (the Rust heuristic mirrors
    // `isDuplicateKey`; the create-only CAS path above exercises it end-to-end).
    #[test]
    fn duplicate_key_detection_recognises_sqlstate_and_message() {
        assert!(is_duplicate_key(&PostgresError::Query(
            "duplicate key value violates unique constraint \"x_pkey\"".into()
        )));
        assert!(is_duplicate_key(&PostgresError::Query(
            "SQLSTATE 23505".into()
        )));
        assert!(!is_duplicate_key(&PostgresError::Query(
            "connection reset".into()
        )));
    }
}
