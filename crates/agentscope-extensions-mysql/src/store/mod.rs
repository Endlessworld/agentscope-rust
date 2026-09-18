//! Namespace-oriented key-value store backed by JDBC/MySQL, mirroring
//! `io.agentscope.extensions.mysql.store`.
//!
//! The store is deliberately decoupled from any concrete driver: all SQL is issued
//! through the [`SqlBackend`] trait, which transport adapters implement. This keeps
//! `MysqlStore` testable without a live database (an in-memory backend is provided for
//! tests) while preserving the exact CAS / version semantics required by
//! [`agentscope_harness::filesystem::remote::store::BaseStore`].

mod dialect;

pub use dialect::{
    like_prefix_pattern, namespace_path, validate_key, H2StoreDialect, MysqlStoreDialect,
    PostgresStoreDialect, SqliteStoreDialect, StoreDialect, NS_SEPARATOR,
};

use std::sync::Arc;

use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};

use crate::error::{MysqlError, MysqlResult};

/// Low-level SQL execution surface used by [`MysqlStore`].
///
/// Implementations may target MySQL, PostgreSQL, SQLite or H2. Only the primitives the
/// store needs are exposed; statements are pre-rendered by the [`StoreDialect`], so the
/// backend only has to bind parameters and report affected-row counts.
///
/// # CAS contract
/// * [`SqlBackend::insert_create`] must fail with a [`MysqlError`] whose
///   [`MysqlError::is_duplicate_key`] returns `true` when the row already exists.
/// * [`SqlBackend::cas_update`] must return the number of rows affected (`1` on success).
pub trait SqlBackend: Send + Sync {
    /// Executes a DDL statement (schema bootstrap).
    fn execute_ddl(&self, sql: &str) -> MysqlResult<()>;

    /// Unconditional upsert; increments `version` on existing rows (last-write-wins).
    fn upsert(
        &self,
        sql: &str,
        ns_path: &str,
        key: &str,
        value_json: &str,
        now_ms: i64,
    ) -> MysqlResult<()>;

    /// Create-only insert with `version = 1`.
    fn insert_create(
        &self,
        sql: &str,
        ns_path: &str,
        key: &str,
        value_json: &str,
        now_ms: i64,
    ) -> MysqlResult<()>;

    /// Conditional update guarded by `version = expected`; returns rows affected.
    fn cas_update(
        &self,
        sql: &str,
        value_json: &str,
        now_ms: i64,
        ns_path: &str,
        key: &str,
        expected_version: i64,
    ) -> MysqlResult<u64>;

    /// Fetches `(value_json, version)` for a single item.
    fn select(&self, sql: &str, ns_path: &str, key: &str) -> MysqlResult<Option<(String, i64)>>;

    /// Prefix search returning `(item_key, value_json, version)` ordered by key.
    fn search(
        &self,
        sql: &str,
        like_pattern: &str,
        limit: usize,
        offset: usize,
    ) -> MysqlResult<Vec<(String, String, i64)>>;

    /// Deletes a single item.
    fn delete(&self, sql: &str, ns_path: &str, key: &str) -> MysqlResult<()>;

    /// Reports the current wall-clock time in epoch milliseconds.
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

/// Default table name, mirroring `JdbcStore.DEFAULT_TABLE_NAME`.
pub const DEFAULT_STORE_TABLE: &str = "agentscope_store";

/// JDBC/MySQL-backed [`BaseStore`], mirroring `JdbcStore`.
///
/// Items are organized by hierarchical namespaces and identified by a key. Every
/// successful write increments the item's `version`, enabling optimistic concurrency
/// control via [`BaseStore::put_if_version`].
pub struct MysqlStore {
    backend: Arc<dyn SqlBackend>,
    dialect: Arc<dyn StoreDialect>,
    table_name: String,
}

impl std::fmt::Debug for MysqlStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlStore")
            .field("dialect", &self.dialect.name())
            .field("table_name", &self.table_name)
            .finish()
    }
}

impl MysqlStore {
    /// Creates a builder over the given SQL backend.
    pub fn builder(backend: Arc<dyn SqlBackend>) -> MysqlStoreBuilder {
        MysqlStoreBuilder {
            backend,
            dialect: Arc::new(MysqlStoreDialect),
            table_name: DEFAULT_STORE_TABLE.to_string(),
            initialize_schema: false,
        }
    }

    /// Constructs a MySQL store with the default dialect and table name.
    pub fn new(backend: Arc<dyn SqlBackend>) -> Self {
        MysqlStore::builder(backend).build()
    }

    fn select_sql(&self) -> String {
        self.dialect.render(self.dialect.select_sql(), &self.table_name)
    }

    fn upsert_sql(&self) -> String {
        self.dialect.render(self.dialect.upsert_sql(), &self.table_name)
    }

    fn insert_sql(&self) -> String {
        self.dialect.render(self.dialect.insert_sql(), &self.table_name)
    }

    fn cas_update_sql(&self) -> String {
        self.dialect
            .render(self.dialect.cas_update_sql(), &self.table_name)
    }

    fn search_sql(&self) -> String {
        self.dialect.render(self.dialect.search_sql(), &self.table_name)
    }

    fn delete_sql(&self) -> String {
        self.dialect.render(self.dialect.delete_sql(), &self.table_name)
    }

    fn serialize(value: &StoreValue) -> MysqlResult<String> {
        Ok(serde_json::to_string(value)?)
    }

    fn deserialize(json: &str) -> MysqlResult<StoreValue> {
        if json.is_empty() {
            return Ok(StoreValue::new());
        }
        Ok(serde_json::from_str(json)?)
    }
}

impl BaseStore for MysqlStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        let ns_path = namespace_path(namespace).ok()?;
        let row = self.backend.select(&self.select_sql(), &ns_path, key).ok()?;
        let (json, version) = row?;
        let value = MysqlStore::deserialize(&json).ok()?;
        Some(StoreItem::new(key, value, version.max(0) as u64))
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        let _ = self.try_put(namespace, key, value);
    }

    fn put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> bool {
        self.try_put_if_version(namespace, key, value, expected_version)
            .unwrap_or(false)
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        if limit == 0 {
            return Vec::new();
        }
        let pattern = match like_prefix_pattern(namespace, self.dialect.like_escape_char()) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        let rows = match self.backend.search(&self.search_sql(), &pattern, limit, offset) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.into_iter()
            .filter_map(|(key, json, version)| {
                MysqlStore::deserialize(&json)
                    .ok()
                    .map(|value| StoreItem::new(key, value, version.max(0) as u64))
            })
            .collect()
    }

    fn delete(&self, namespace: &[String], key: &str) {
        if let Ok(ns_path) = namespace_path(namespace) {
            let _ = self.backend.delete(&self.delete_sql(), &ns_path, key);
        }
    }
}

impl MysqlStore {
    /// Result-returning `put`; mirrors `JdbcStore.put`.
    pub fn try_put(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
    ) -> MysqlResult<()> {
        validate_key(key)?;
        let ns_path = namespace_path(namespace)?;
        let json = MysqlStore::serialize(&value)?;
        let now = self.backend.now_ms();
        self.backend
            .upsert(&self.upsert_sql(), &ns_path, key, &json, now)
    }

    /// Result-returning CAS; mirrors `JdbcStore.putIfVersion`.
    ///
    /// `expected_version == 0` performs a create-if-absent insert and treats a
    /// primary-key conflict as a failed CAS. Otherwise a conditional `UPDATE ... WHERE
    /// version = ?` is executed and a single affected row indicates success.
    pub fn try_put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> MysqlResult<bool> {
        validate_key(key)?;
        let ns_path = namespace_path(namespace)?;
        let json = MysqlStore::serialize(&value)?;
        let now = self.backend.now_ms();

        if expected_version == 0 {
            return match self
                .backend
                .insert_create(&self.insert_sql(), &ns_path, key, &json, now)
            {
                Ok(()) => Ok(true),
                Err(e) if e.is_duplicate_key() => Ok(false),
                Err(e) => Err(e),
            };
        }

        let affected = self.backend.cas_update(
            &self.cas_update_sql(),
            &json,
            now,
            &ns_path,
            key,
            expected_version as i64,
        )?;
        Ok(affected == 1)
    }

    /// Reloads the given namespace item, unmarshalling into a concrete type.
    pub fn get_typed<T: serde::de::DeserializeOwned>(
        &self,
        namespace: &[String],
        key: &str,
    ) -> Option<T> {
        let item = self.get(namespace, key)?;
        serde_json::from_value(serde_json::Value::Object(item.value)).ok()
    }
}

/// Builder for [`MysqlStore`], mirroring `JdbcStore.Builder`.
pub struct MysqlStoreBuilder {
    backend: Arc<dyn SqlBackend>,
    dialect: Arc<dyn StoreDialect>,
    table_name: String,
    initialize_schema: bool,
}

impl MysqlStoreBuilder {
    /// Overrides the SQL dialect (default: [`MysqlStoreDialect`]).
    pub fn dialect(mut self, dialect: Arc<dyn StoreDialect>) -> Self {
        self.dialect = dialect;
        self
    }

    /// Overrides the table name (default: [`DEFAULT_STORE_TABLE`]).
    pub fn table_name(mut self, table: impl Into<String>) -> Self {
        self.table_name = table.into();
        self
    }

    /// When `true`, runs the dialect's `CREATE TABLE IF NOT EXISTS` on construction.
    pub fn initialize_schema(mut self, initialize: bool) -> Self {
        self.initialize_schema = initialize;
        self
    }

    /// Builds the store, ignoring schema-bootstrap errors (best-effort).
    pub fn build(self) -> MysqlStore {
        let initialize = self.initialize_schema;
        let backend = self.backend;
        let dialect = self.dialect;
        let table_name = self.table_name;
        if initialize {
            let ddl = dialect.render(dialect.create_table_sql(), &table_name);
            let _ = backend.execute_ddl(&ddl);
        }
        MysqlStore {
            backend,
            dialect,
            table_name,
        }
    }

    /// Builds the store, surfacing schema-bootstrap failures.
    pub fn try_build(self) -> MysqlResult<MysqlStore> {
        let initialize = self.initialize_schema;
        let backend = self.backend;
        let dialect = self.dialect;
        let table_name = self.table_name;
        if initialize {
            let ddl = dialect.render(dialect.create_table_sql(), &table_name);
            backend.execute_ddl(&ddl)?;
        }
        Ok(MysqlStore {
            backend,
            dialect,
            table_name,
        })
    }
}

/// Ensures `value` is a JSON object, matching `StoreValue`.
pub fn value_from_json(value: serde_json::Value) -> MysqlResult<StoreValue> {
    match value {
        serde_json::Value::Object(map) => Ok(map),
        other => Err(MysqlError::InvalidArgument(format!(
            "store value must be a JSON object, got: {other}"
        ))),
    }
}

#[cfg(test)]
pub mod testing;

#[cfg(test)]
mod tests {
    use super::testing::InMemorySqlBackend;
    use super::*;
    use serde_json::json;

    fn store() -> MysqlStore {
        MysqlStore::builder(Arc::new(InMemorySqlBackend::default()))
            .initialize_schema(true)
            .build()
    }

    fn ns(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn obj() -> StoreValue {
        let mut m = StoreValue::new();
        m.insert("a".into(), json!(1));
        m
    }

    #[test]
    fn put_then_get_increments_version() {
        let s = store();
        let n = ns(&["filesystem"]);
        s.put(&n, "k", obj());
        assert_eq!(s.get(&n, "k").expect("present").version, 1);
        s.put(&n, "k", obj());
        assert_eq!(s.get(&n, "k").expect("present").version, 2);
    }

    #[test]
    fn put_if_version_zero_creates_once() {
        let s = store();
        let n = ns(&["filesystem"]);
        assert!(s.put_if_version(&n, "k", obj(), 0));
        assert!(!s.put_if_version(&n, "k", obj(), 0));
    }

    #[test]
    fn put_if_version_matches_only_current() {
        let s = store();
        let n = ns(&["filesystem"]);
        s.put(&n, "k", obj());
        assert!(!s.put_if_version(&n, "k", obj(), 99));
        assert!(s.put_if_version(&n, "k", obj(), 1));
        assert_eq!(s.get(&n, "k").expect("present").version, 2);
    }

    #[test]
    fn search_matches_subnamespaces() {
        let s = store();
        s.put(&ns(&["a"]), "k1", obj());
        s.put(&ns(&["a", "b"]), "k2", obj());
        s.put(&ns(&["c"]), "k3", obj());
        let keys: Vec<_> = s
            .search(&ns(&["a"]), 10, 0)
            .iter()
            .map(|i| i.key.clone())
            .collect();
        assert!(keys.contains(&"k1".to_string()));
        assert!(keys.contains(&"k2".to_string()));
        assert!(!keys.contains(&"k3".to_string()));
    }

    #[test]
    fn search_pagination() {
        let s = store();
        let n = ns(&["a"]);
        for i in 0..5 {
            s.put(&n, &format!("k{i}"), obj());
        }
        let page = s.search(&n, 2, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].key, "k2");
    }

    #[test]
    fn delete_removes_item() {
        let s = store();
        let n = ns(&["a"]);
        s.put(&n, "k", obj());
        s.delete(&n, "k");
        assert!(s.get(&n, "k").is_none());
    }

    #[test]
    fn get_typed_roundtrips() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Foo {
            a: i64,
        }
        let s = store();
        let n = ns(&["a"]);
        s.put(&n, "k", obj());
        let foo: Foo = s.get_typed(&n, "k").expect("typed");
        assert_eq!(foo, Foo { a: 1 });
    }

    #[test]
    fn invalid_empty_key_is_rejected() {
        let s = store();
        assert!(s.try_put(&ns(&["a"]), "", obj()).is_err());
    }
}
