//! JDBC-backed [`BaseStore`] with zero inline SQL, mirroring `JdbcStore`.
//!
//! All database-specific SQL is delegated to [`StoreDialect`]. Business logic (CAS,
//! namespace encoding, pagination) is completely database-agnostic.

use std::sync::Arc;

use agentscope_harness::filesystem::{BaseStore, StoreItem, StoreValue};

use crate::backend::{is_unique_violation, JdbcExecutor, JdbcResult, SharedExecutor};
use crate::dialect::{
    validate_table_schema, AbstractJdbcDialect, BoundSql, SqlParam, StoreDialect,
};

/// ASCII unit separator (U+001F) between namespace segments, mirroring the Java
/// `NS_SEPARATOR`.
const NS_SEPARATOR: char = '\u{001F}';

/// JDBC-backed [`BaseStore`] backed by a [`StoreDialect`].
///
/// Namespaces are encoded into a single `namespace_path` column by joining segments with
/// the unit separator. Versions start at 1 and increment on every successful write,
/// enabling optimistic concurrency control via [`BaseStore::put_if_version`].
pub struct JdbcStore {
    executor: SharedExecutor,
    dialect: Arc<dyn AbstractJdbcDialect>,
}

impl std::fmt::Debug for JdbcStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcStore")
            .field("kind", &self.dialect.kind())
            .finish_non_exhaustive()
    }
}

impl JdbcStore {
    /// Creates a store, auto-creating the schema when `initialize_schema` is true.
    pub fn new(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
        initialize_schema: bool,
    ) -> JdbcResult<Self> {
        let store = Self { executor, dialect };
        if initialize_schema {
            store.initialize_schema()?;
        }
        Ok(store)
    }

    /// Creates a store with schema auto-creation enabled.
    pub fn with_schema(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
    ) -> JdbcResult<Self> {
        Self::new(executor, dialect, true)
    }

    /// Runs the dialect's idempotent DDL for the KV store table, then validates the
    /// resulting schema (Java `TableSchemaValidator`).
    pub fn initialize_schema(&self) -> JdbcResult<()> {
        let ddls = self.dialect.store_create_table_ddls();
        crate::schema_bootstrap::run_ddl(&*self.executor, self.dialect.as_ref(), &ddls)?;
        validate_table_schema(
            &*self.executor,
            self.dialect.as_ref(),
            &self.dialect.resolved_store_table_name(),
            &ddls,
        )?;
        Ok(())
    }

    /// Encodes a namespace tuple into the single `namespace_path` column value.
    ///
    /// Mirrors the Java `JdbcStore.namespacePath`: every segment is followed by the unit
    /// separator, including the last (`["parent"]` -> `"parent\u{1F}"`). The trailing
    /// separator is what makes the prefix `LIKE 'parent<US>%'` match items stored both in
    /// `parent` itself and in nested namespaces such as `parent<US>child`.
    fn namespace_path(namespace: &[String]) -> String {
        let mut out = String::new();
        for segment in namespace {
            out.push_str(segment);
            out.push(NS_SEPARATOR);
        }
        out
    }

    /// The namespace prefix used by [`BaseStore::search`].
    ///
    /// Identical to [`Self::namespace_path`] — the Java `likePrefixPattern` escapes this
    /// path and appends the `%` wildcard.
    fn namespace_prefix(namespace: &[String]) -> String {
        Self::namespace_path(namespace)
    }

    fn parse_value(json: &str) -> StoreValue {
        serde_json::from_str(json).unwrap_or_default()
    }

    fn serialize_value(value: &StoreValue) -> JdbcResult<String> {
        serde_json::to_string(value)
            .map_err(|e| crate::backend::JdbcError::Database(format!("serialize value: {e}")))
    }

    fn validate_key(key: &str) -> JdbcResult<()> {
        if key.trim().is_empty() {
            return Err(crate::backend::JdbcError::Database(
                "key must not be blank".to_string(),
            ));
        }
        Ok(())
    }

    /// Reads a single item, returning `(value_json, version)`.
    fn read_raw(&self, namespace_path: &str, key: &str) -> JdbcResult<Option<(String, i64)>> {
        let bound = self.dialect.store_select(namespace_path, key);
        let rows = self.executor.query(&bound)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let json = row
            .first()
            .and_then(|c| c.as_text())
            .unwrap_or_default()
            .to_string();
        let version = row.get(1).and_then(|c| c.as_int()).unwrap_or(0);
        Ok(Some((json, version)))
    }
}

impl BaseStore for JdbcStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        if Self::validate_key(key).is_err() {
            return None;
        }
        let path = Self::namespace_path(namespace);
        let (json, version) = self.read_raw(&path, key).ok()??;
        Some(StoreItem::new(
            key,
            Self::parse_value(&json),
            version as u64,
        ))
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        if Self::validate_key(key).is_err() {
            return;
        }
        let path = Self::namespace_path(namespace);
        let Ok(json) = Self::serialize_value(&value) else {
            return;
        };
        let bound = self
            .dialect
            .store_upsert(&path, key, &json, crate::now_millis());
        let _ = self.executor.execute(&bound);
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
        let path = Self::namespace_path(namespace);
        let Ok(json) = Self::serialize_value(&value) else {
            return false;
        };
        let timestamp = crate::now_millis();

        // expected_version == 0 means "create only if absent".
        if expected_version == 0 {
            let bound = self.dialect.store_insert(&path, key, &json, timestamp);
            return match self.executor.execute(&bound) {
                Ok(affected) => affected == 1,
                Err(e) if is_unique_violation(&e) => false,
                Err(e) => {
                    tracing::debug!("[jdbc-store] insert-if-absent failed: {e}");
                    false
                }
            };
        }

        let bound =
            self.dialect
                .store_cas_update(&json, timestamp, &path, key, expected_version as i64);
        match self.executor.execute(&bound) {
            Ok(affected) => affected == 1,
            Err(e) => {
                tracing::debug!("[jdbc-store] CAS update failed: {e}");
                false
            }
        }
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
        let prefix = Self::namespace_prefix(namespace);
        // Java `likePrefixPattern`: escape the namespace path, then append the `%` wildcard.
        let pattern = format!(
            "{}%",
            escape_like(&prefix, self.dialect.store_like_escape_char())
        );
        let bound = self.dialect.store_search(&pattern, limit, offset);
        let Ok(rows) = self.executor.query(&bound) else {
            return Vec::new();
        };
        rows.into_iter()
            .filter_map(|row| {
                let key = row.first().and_then(|c| c.as_text())?.to_string();
                let json = row.get(1).and_then(|c| c.as_text()).unwrap_or_default();
                let version = row.get(2).and_then(|c| c.as_int()).unwrap_or(0) as u64;
                Some(StoreItem::new(key, Self::parse_value(json), version))
            })
            .collect()
    }

    fn delete(&self, namespace: &[String], key: &str) {
        if Self::validate_key(key).is_err() {
            return;
        }
        let path = Self::namespace_path(namespace);
        let bound = self.dialect.store_delete(&path, key);
        let _ = self.executor.execute(&bound);
    }
}

/// Escapes `%`, `_` and the dialect's `LIKE` escape character in a prefix so it is
/// treated literally. The trailing `%` wildcard is appended by the caller.
fn escape_like(value: &str, escape_char: char) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch == '%' || ch == '_' || ch == escape_char {
            out.push(escape_char);
        }
        out.push(ch);
    }
    out
}

/// Namespace application helper: builds a [`StoreValue`] from a JSON object literal if
/// callers prefer not to import `serde_json` directly.
pub fn store_value(json_object: serde_json::Map<String, serde_json::Value>) -> StoreValue {
    json_object
}

/// Constructs a [`SqlParam`] from a namespace path; exposed for tests.
#[cfg(test)]
pub(crate) fn namespace_param(path: &str) -> SqlParam {
    SqlParam::Text(path.to_string())
}
