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
            Value::Object(map) => {
                let unescaped = key_escaper::unescape(&Value::Object(map.clone()));
                match unescaped {
                    Value::Object(m) => m,
                    _ => StoreValue::new(),
                }
            }
            _ => StoreValue::new(),
        }
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
        let doc = futures::executor::block_on(self.client.find_one(&self.collection, &filter))
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
}
