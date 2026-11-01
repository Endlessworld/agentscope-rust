//! MongoDB-backed [`BaseStore`], mirroring
//! `io.agentscope.extensions.mongodb.store.MongoBaseStore`.
//!
//! Each item is a separate document. Namespace paths and keys are encoded into a
//! compound `_id` using `\u001F` (ASCII Unit Separator) as the segment delimiter.
//! The `namespace` field stores the full namespace path with a trailing separator,
//! enabling prefix-matching via range queries (`$gte`/`$lt`) on the compound index.
//!
//! Optimistic concurrency is supported via a `version` field; `version` starts at 1
//! and increments on every successful write.

use std::sync::Arc;

use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};
use serde_json::{json, Value};

use crate::client::{
    FindFilter, FindOneAndUpdateOptions, FindOptions, MongoClient, MongoError, Update,
    UpdateOptions,
};
use crate::constants::BASE_STORE_COLLECTION;
use crate::index_utils::{create_index_with_migration, IndexSpec};
use crate::key_escaper;

const FIELD_ID: &str = "_id";
const FIELD_KEY: &str = "key";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_VALUE: &str = "value";
const FIELD_VERSION: &str = "version";

/// Namespace segment delimiter (ASCII Unit Separator).
pub const NS_SEPARATOR: char = '\u{1F}';
/// Upper bound appended to a namespace prefix for the exclusive `$lt` range end.
pub const NS_UPPER_BOUND: char = '\u{FFFF}';

/// MongoDB-backed [`BaseStore`] implementation.
#[derive(Clone)]
pub struct MongodbStore {
    client: Arc<dyn MongoClient>,
    collection: String,
}

impl std::fmt::Debug for MongodbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbStore")
            .field("collection", &self.collection)
            .finish()
    }
}

impl MongodbStore {
    /// Creates a store over the given client and collection.
    pub fn new(client: Arc<dyn MongoClient>, collection: impl Into<String>) -> Self {
        Self {
            client,
            collection: collection.into(),
        }
    }

    /// Creates a store with the default collection name ([`BASE_STORE_COLLECTION`]).
    pub fn with_defaults(client: Arc<dyn MongoClient>) -> Self {
        Self::new(client, BASE_STORE_COLLECTION)
    }

    /// Ensures the namespace+key compound index exists.
    pub async fn ensure_indexes(&self) -> Result<(), MongoError> {
        create_index_with_migration(
            &self.client,
            &self.collection,
            &IndexSpec::compound_ascending(&[FIELD_NAMESPACE, FIELD_KEY]),
        )
        .await
    }

    fn validate_key(key: &str) -> Result<(), MongoError> {
        if key.is_empty() {
            return Err(MongoError::InvalidArgument(
                "key must not be null or empty".into(),
            ));
        }
        if key.contains(NS_SEPARATOR) {
            return Err(MongoError::InvalidArgument(
                "key must not contain the unit separator (0x1F)".into(),
            ));
        }
        Ok(())
    }

    fn namespace_path(namespace: &[String]) -> Result<String, MongoError> {
        let mut sb = String::new();
        for (i, segment) in namespace.iter().enumerate() {
            if segment.contains(NS_SEPARATOR) {
                return Err(MongoError::InvalidArgument(
                    "namespace segment must not contain the unit separator (0x1F)".into(),
                ));
            }
            if i > 0 {
                sb.push(NS_SEPARATOR);
            }
            sb.push_str(segment);
        }
        sb.push(NS_SEPARATOR);
        Ok(sb)
    }

    fn item_doc_id(namespace: &[String], key: &str) -> Result<String, MongoError> {
        Ok(format!("{}{}", Self::namespace_path(namespace)?, key))
    }

    fn parse_value(raw: &Value) -> StoreValue {
        match raw {
            Value::Object(_) => Self::unescaped_store_value(raw),
            // MONGO-02: rows written before values became sub-documents keep the JSON payload
            // in a *string* (Java `MongoBaseStore` accepted `String` values and ran
            // `fromJson`). Returning an empty map for those silently lost the whole value.
            Value::String(text) => match serde_json::from_str::<Value>(text) {
                Ok(parsed @ Value::Object(_)) => Self::unescaped_store_value(&parsed),
                Ok(other) => {
                    tracing::warn!(
                        kind = json_kind(&other),
                        "[mongo-store] string value holds a non-object JSON payload; \
                         returning an empty value"
                    );
                    StoreValue::new()
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "[mongo-store] string value is not valid JSON; returning an empty value"
                    );
                    StoreValue::new()
                }
            },
            Value::Null => StoreValue::new(),
            other => {
                // Java throws `IllegalStateException` here; `BaseStore` has no error channel,
                // so log loudly rather than dropping the value silently.
                tracing::warn!(
                    kind = json_kind(other),
                    "[mongo-store] unexpected stored value type; returning an empty value"
                );
                StoreValue::new()
            }
        }
    }

    /// Unescapes a stored object back into a `StoreValue`.
    fn unescaped_store_value(raw: &Value) -> StoreValue {
        match key_escaper::unescape(raw) {
            Value::Object(m) => m,
            _ => StoreValue::new(),
        }
    }
}

/// Human-readable JSON type name for log lines.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[async_trait::async_trait]
impl BaseStore for MongodbStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        if Self::validate_key(key).is_err() {
            return None;
        }
        let id = Self::item_doc_id(namespace, key).ok()?;
        let filter = FindFilter::eq(FIELD_ID, json!(id));
        // MONGO-05: project the payload and its version so a single-key read does not
        // transfer the whole item document (Java reads `value`/`version` only).
        let doc = futures::executor::block_on(self.client.find_one_projected(
            &self.collection,
            &filter,
            &[FIELD_VALUE, FIELD_VERSION],
        ))
        .ok()
        .flatten()?;
        let value = Self::parse_value(doc.get(FIELD_VALUE).unwrap_or(&Value::Null));
        let version = doc.get(FIELD_VERSION).and_then(|v| v.as_u64()).unwrap_or(0);
        Some(StoreItem::new(key, value, version))
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        if Self::validate_key(key).is_err() {
            return;
        }
        let Ok(id) = Self::item_doc_id(namespace, key) else {
            return;
        };
        let Ok(ns_key) = Self::namespace_path(namespace) else {
            return;
        };
        let value_doc = key_escaper::escape(&Value::Object(value));
        let filter = FindFilter::eq(FIELD_ID, json!(id));
        let set_fields = Update::new()
            .set(FIELD_VALUE, value_doc)
            .inc(FIELD_VERSION, 1);
        let set_on_insert = Update::new()
            .set_on_insert(FIELD_ID, json!(id))
            .set_on_insert(FIELD_KEY, json!(key))
            .set_on_insert(FIELD_NAMESPACE, json!(ns_key));
        let mut attempt = 0;
        loop {
            let result = futures::executor::block_on(self.client.update_one(
                &self.collection,
                &filter,
                &set_fields.clone().combine(set_on_insert.clone()),
                &UpdateOptions { upsert: true },
            ));
            match result {
                Ok(()) => return,
                Err(MongoError::DuplicateKey) if attempt == 0 => {
                    attempt += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(key, error = %e, "[mongo-store] put failed");
                    return;
                }
            }
        }
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
        let Ok(id) = Self::item_doc_id(namespace, key) else {
            return false;
        };
        let Ok(ns_key) = Self::namespace_path(namespace) else {
            return false;
        };
        let value_doc = key_escaper::escape(&Value::Object(value));

        if expected_version == 0 {
            let mut doc = serde_json::Map::new();
            doc.insert(FIELD_ID.to_string(), json!(id));
            doc.insert(FIELD_VALUE.to_string(), value_doc);
            doc.insert(FIELD_KEY.to_string(), json!(key));
            doc.insert(FIELD_NAMESPACE.to_string(), json!(ns_key));
            doc.insert(FIELD_VERSION.to_string(), json!(1));
            match futures::executor::block_on(self.client.insert_one(&self.collection, doc)) {
                Ok(()) => true,
                Err(MongoError::DuplicateKey) => false,
                Err(e) => {
                    tracing::warn!(key, error = %e, "[mongo-store] put_if_version insert failed");
                    false
                }
            }
        } else {
            let filter = FindFilter::eq(FIELD_ID, json!(id))
                .and(FindFilter::eq(FIELD_VERSION, json!(expected_version)));
            let update = Update::new()
                .set(FIELD_VALUE, value_doc)
                .set(FIELD_KEY, json!(key))
                .set(FIELD_NAMESPACE, json!(ns_key))
                .inc(FIELD_VERSION, 1);
            match futures::executor::block_on(self.client.find_one_and_update(
                &self.collection,
                &filter,
                &update,
                &FindOneAndUpdateOptions {
                    upsert: false,
                    return_after: true,
                },
            )) {
                Ok(result) => result.is_some(),
                Err(e) => {
                    tracing::warn!(key, error = %e, "[mongo-store] put_if_version failed");
                    false
                }
            }
        }
    }

    /// Conditional document update / insert-if-absent: atomic in MongoDB (Java
    /// `supportsAtomicSessionStorage`).
    fn supports_atomic_session_storage(&self) -> bool {
        true
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        if limit == 0 {
            return Vec::new();
        }
        let Ok(ns_prefix) = Self::namespace_path(namespace) else {
            return Vec::new();
        };
        let filter = FindFilter::default()
            .gte(FIELD_NAMESPACE, json!(ns_prefix))
            .lt(
                FIELD_NAMESPACE,
                json!(format!("{ns_prefix}{NS_UPPER_BOUND}")),
            );
        let options = FindOptions {
            sort_ascending_by: Some(FIELD_KEY.to_string()),
            skip: Some(offset),
            limit: Some(limit),
            // MONGO-05: a listing only needs the key, payload and version — the
            // namespace/index fields stay on the server.
            projection: Some(vec![
                FIELD_KEY.to_string(),
                FIELD_VALUE.to_string(),
                FIELD_VERSION.to_string(),
            ]),
        };
        let Ok(docs) =
            futures::executor::block_on(self.client.find_many(&self.collection, &filter, &options))
        else {
            return Vec::new();
        };
        docs.into_iter()
            .filter_map(|doc| {
                let key = doc.get(FIELD_KEY)?.as_str()?.to_string();
                let value = Self::parse_value(doc.get(FIELD_VALUE).unwrap_or(&Value::Null));
                let version = doc.get(FIELD_VERSION).and_then(|v| v.as_u64()).unwrap_or(0);
                Some(StoreItem::new(key, value, version))
            })
            .collect()
    }

    fn delete(&self, namespace: &[String], key: &str) {
        if Self::validate_key(key).is_err() {
            return;
        }
        let Ok(id) = Self::item_doc_id(namespace, key) else {
            return;
        };
        let filter = FindFilter::eq(FIELD_ID, json!(id));
        let _ = futures::executor::block_on(self.client.delete_one(&self.collection, &filter));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MongodbStore {
        MongodbStore::with_defaults(Arc::new(crate::client::InMemoryMongoClient::new()))
    }

    fn ns() -> Vec<String> {
        vec![
            "sessions".to_string(),
            "s1".to_string(),
            "filesystem".to_string(),
        ]
    }

    fn value(content: &str) -> StoreValue {
        let mut m = StoreValue::new();
        m.insert("content".to_string(), json!(content));
        m
    }

    /// MONGO-05: `get` and `search` project the fields they return, so neither transfers
    /// the `_id` / `namespace` index columns nor any unrelated field.
    #[test]
    fn reads_project_only_the_fields_they_return() {
        use crate::client::InMemoryMongoClient;

        let raw = Arc::new(InMemoryMongoClient::new());
        let s = MongodbStore::with_defaults(raw.clone());
        s.put(&ns(), "a.txt", value("a"));

        assert_eq!(
            s.get(&ns(), "a.txt").unwrap().value.get("content").unwrap(),
            &json!("a")
        );
        assert_eq!(
            raw.last_projection(),
            Some(vec!["value".to_string(), "version".to_string()]),
            "get must transfer only the payload and its version"
        );

        let items = s.search(&ns(), 10, 0);
        assert_eq!(items.len(), 1);
        assert_eq!(
            raw.last_projection(),
            Some(vec![
                "key".to_string(),
                "value".to_string(),
                "version".to_string()
            ]),
            "search must project the returned columns only"
        );
    }

    #[test]
    fn put_assigns_increasing_versions() {
        let s = store();
        s.put(&ns(), "a.txt", value("one"));
        assert_eq!(s.get(&ns(), "a.txt").unwrap().version, 1);
        s.put(&ns(), "a.txt", value("two"));
        let item = s.get(&ns(), "a.txt").unwrap();
        assert_eq!(item.version, 2);
        assert_eq!(item.value.get("content").unwrap(), &json!("two"));
    }

    #[test]
    fn put_if_version_create_if_absent() {
        let s = store();
        assert!(s.put_if_version(&ns(), "a.txt", value("one"), 0));
        assert!(!s.put_if_version(&ns(), "a.txt", value("two"), 0));
        assert_eq!(
            s.get(&ns(), "a.txt").unwrap().value.get("content").unwrap(),
            &json!("one")
        );
        assert!(s.put_if_version(&ns(), "a.txt", value("three"), 1));
        assert_eq!(s.get(&ns(), "a.txt").unwrap().version, 2);
    }

    #[test]
    fn search_is_scoped_paginated_and_sorted() {
        let s = store();
        s.put(&ns(), "c.txt", value("c"));
        s.put(&ns(), "a.txt", value("a"));
        s.put(&ns(), "b.txt", value("b"));
        s.put(&vec!["other".to_string()], "x.txt", value("x"));
        let page = s.search(&ns(), 2, 0);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].key, "a.txt");
        assert_eq!(page[1].key, "b.txt");
        let page2 = s.search(&ns(), 2, 2);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].key, "c.txt");
    }

    #[test]
    fn search_includes_child_namespaces() {
        let s = store();
        s.put(&ns(), "root.txt", value("root"));
        let child = vec![
            "sessions".to_string(),
            "s1".to_string(),
            "filesystem".to_string(),
            "sub".to_string(),
        ];
        s.put(&child, "child.txt", value("child"));
        let found = s.search(&ns(), 10, 0);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn delete_removes_only_target_key() {
        let s = store();
        s.put(&ns(), "a.txt", value("a"));
        s.put(&ns(), "b.txt", value("b"));
        s.delete(&ns(), "a.txt");
        assert!(s.get(&ns(), "a.txt").is_none());
        assert!(s.get(&ns(), "b.txt").is_some());
        // Idempotent, mirroring Java `MongoBaseStoreTest#deleteRemovesItem` +
        // `MongoBaseStoreContractTest#delete_isIdempotent`.
        s.delete(&ns(), "a.txt");
        assert!(s.get(&ns(), "a.txt").is_none());
        assert!(s.get(&ns(), "b.txt").is_some());
    }

    #[test]
    fn keys_with_dots_and_dollars_round_trip() {
        let s = store();
        let mut v = StoreValue::new();
        v.insert("a.b".to_string(), json!({"$x": 1}));
        s.put(&ns(), "k", v.clone());
        let got = s.get(&ns(), "k").unwrap();
        assert_eq!(got.value.get("a.b").unwrap(), &json!({"$x": 1}));
    }

    #[test]
    fn invalid_keys_are_rejected() {
        let s = store();
        s.put(&ns(), "", value("x"));
        assert!(s.get(&ns(), "").is_none());
        let bad = format!("a{NS_SEPARATOR}b");
        s.put(&ns(), &bad, value("x"));
        assert!(s.get(&ns(), &bad).is_none());
    }

    /// MONGO-02: legacy rows keep the JSON payload in a **string** (Java `MongoBaseStore`
    /// accepted `String` values and ran `fromJson`). Before the fix `parse_value` returned an
    /// empty map for every non-object, so reading such a row silently lost the whole value.
    #[test]
    fn legacy_json_string_values_are_read_back() {
        let client = Arc::new(crate::client::InMemoryMongoClient::new());
        let s = MongodbStore::with_defaults(client.clone());
        let id = MongodbStore::item_doc_id(&ns(), "a.txt").unwrap();
        let mut doc = serde_json::Map::new();
        doc.insert("_id".to_string(), json!(id));
        doc.insert("key".to_string(), json!("a.txt"));
        doc.insert(
            "namespace".to_string(),
            json!(MongodbStore::namespace_path(&ns()).unwrap()),
        );
        doc.insert(
            "value".to_string(),
            json!(r#"{"content":"legacy"}"#),
        );
        doc.insert("version".to_string(), json!(7));
        futures::executor::block_on(client.insert_one(BASE_STORE_COLLECTION, doc)).unwrap();

        let item = s.get(&ns(), "a.txt").expect("legacy row must be readable");
        assert_eq!(item.key, "a.txt");
        assert_eq!(item.version, 7);
        assert_eq!(item.value.get("content").unwrap(), &json!("legacy"));

        // Search decodes through the same path.
        let found = s.search(&ns(), 10, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].value.get("content").unwrap(), &json!("legacy"));

        // A string that is not a JSON object still yields an empty (non-panicking) value.
        let broken = MongodbStore::parse_value(&json!("not json"));
        assert!(broken.is_empty());
        assert!(MongodbStore::parse_value(&json!(42)).is_empty());
    }
}
