//! `StoreJevContextArchive`: the harness-`BaseStore`-backed snapshot archive.
//!
//! Java reuses the harness' namespace/CAS store instead of maintaining a second storage backend:
//! the snapshot JSON is the value, its SHA-256 is the key, and the write is a create-if-absent CAS
//! so a concurrent writer cannot fork the same reference into two different snapshots. This port
//! keeps that contract exactly, including the Java messages:
//!
//! * `positive snapshot limit required`
//! * `snapshot limit`
//! * `archive requires acknowledged create-if-absent storage`
//! * `invalid archive reference`
//! * `missing or invalid snapshot`
//! * `snapshot scope mismatch`

use std::sync::Arc;

use agentscope_harness::filesystem::remote::store::{BaseStore, StoreValue};
use async_trait::async_trait;
use serde_json::Value;

use crate::error::JevError;

use super::super::context::archive::{
    require_reference, ArchiveEnvelope, ArchiveScope, JevContextArchive, ARCHIVE_VERSION,
};
use super::super::context::file_archive::sha256_hex;
use super::super::context::ContextMessage;

/// The store namespace prefix Java uses (`StoreJevContextArchive.namespace`).
pub const NAMESPACE_PREFIX: [&str; 2] = ["jev", "context"];

/// The store key under which the snapshot JSON lives.
pub const SNAPSHOT_KEY: &str = "snapshot";

/// The namespace a scope's snapshots live in: `jev/context/<u>/<a>/<s>`, each id hashed.
pub fn context_namespace(scope: &ArchiveScope) -> Vec<String> {
    let mut namespace: Vec<String> = NAMESPACE_PREFIX
        .iter()
        .map(|part| (*part).to_string())
        .collect();
    for value in [&scope.user_id, &scope.agent_id, &scope.session_id] {
        namespace.push(sha256_hex(value.as_bytes()));
    }
    namespace
}

/// A content-addressed archive over a harness [`BaseStore`].
///
/// The store must acknowledge a create-if-absent write: a backend whose `put_if_version` is the
/// trait's `false` stub (or that silently overwrites) is rejected rather than used with weaker
/// guarantees.
pub struct StoreJevContextArchive {
    store: Arc<dyn BaseStore>,
    max_bytes: usize,
}

impl StoreJevContextArchive {
    /// Creates the archive over `store`; `max_bytes` must be positive.
    pub fn new(store: Arc<dyn BaseStore>, max_bytes: usize) -> Result<Self, JevError> {
        if max_bytes < 1 {
            return Err(JevError::invalid("positive snapshot limit required"));
        }
        Ok(Self { store, max_bytes })
    }

    /// The byte limit per snapshot.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    fn missing_snapshot() -> JevError {
        JevError::invalid("missing or invalid snapshot")
    }
}

impl std::fmt::Debug for StoreJevContextArchive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreJevContextArchive")
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl JevContextArchive for StoreJevContextArchive {
    async fn save(
        &self,
        scope: &ArchiveScope,
        messages: &[ContextMessage],
    ) -> Result<String, JevError> {
        let envelope = ArchiveEnvelope::new(scope.clone(), messages.to_vec());
        let snapshot = serde_json::to_string(&envelope)
            .map_err(|_error| JevError::invalid("snapshot limit"))?;
        if snapshot.len() > self.max_bytes {
            return Err(JevError::invalid("snapshot limit"));
        }
        let reference = sha256_hex(snapshot.as_bytes());
        let namespace = context_namespace(scope);
        let mut value = StoreValue::new();
        value.insert(SNAPSHOT_KEY.to_string(), Value::String(snapshot.clone()));
        if !self.store.put_if_version(&namespace, &reference, value, 0) {
            let existing = self.store.get(&namespace, &reference).and_then(|item| {
                item.value
                    .get(SNAPSHOT_KEY)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            if existing.as_deref() != Some(snapshot.as_str()) {
                return Err(JevError::protocol(
                    "archive requires acknowledged create-if-absent storage",
                ));
            }
        }
        Ok(reference)
    }

    async fn restore(
        &self,
        scope: &ArchiveScope,
        reference: &str,
    ) -> Result<Vec<ContextMessage>, JevError> {
        require_reference(reference)?;
        let namespace = context_namespace(scope);
        let snapshot = self
            .store
            .get(&namespace, reference)
            .and_then(|item| {
                item.value
                    .get(SNAPSHOT_KEY)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(Self::missing_snapshot)?;
        if snapshot.len() > self.max_bytes || sha256_hex(snapshot.as_bytes()) != reference {
            return Err(Self::missing_snapshot());
        }
        let envelope: ArchiveEnvelope =
            serde_json::from_str(&snapshot).map_err(|_error| Self::missing_snapshot())?;
        if envelope.version != ARCHIVE_VERSION || envelope.scope != *scope {
            return Err(JevError::invalid("snapshot scope mismatch"));
        }
        Ok(envelope.messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::filesystem::remote::store::{InMemoryStore, StoreItem};

    fn scope() -> ArchiveScope {
        ArchiveScope::new("user", "agent", "session").unwrap()
    }

    fn messages() -> Vec<ContextMessage> {
        vec![
            ContextMessage::user("m1", "hello"),
            ContextMessage::assistant("m2", "world"),
        ]
    }

    fn archive(max_bytes: usize) -> (Arc<InMemoryStore>, StoreJevContextArchive) {
        let store = Arc::new(InMemoryStore::new());
        let archive =
            StoreJevContextArchive::new(store.clone() as Arc<dyn BaseStore>, max_bytes).unwrap();
        (store, archive)
    }

    /// A store that never acknowledges a conditional write and never returns anything.
    struct DeafStore;

    impl BaseStore for DeafStore {
        fn get(&self, _namespace: &[String], _key: &str) -> Option<StoreItem> {
            None
        }
        fn put(&self, _namespace: &[String], _key: &str, _value: StoreValue) {}
        fn search(&self, _namespace: &[String], _limit: usize, _offset: usize) -> Vec<StoreItem> {
            Vec::new()
        }
        fn delete(&self, _namespace: &[String], _key: &str) {}
    }

    #[test]
    fn namespace_hashes_each_scope_component() {
        // Digests computed outside this crate: SHA-256 of "user" / "agent" / "session".
        assert_eq!(
            context_namespace(&scope()),
            vec![
                "jev".to_string(),
                "context".to_string(),
                "04f8996da763b7a969b1028ee3007569eaf3a635486ddab211d512c85b9df8fb".to_string(),
                "d4f0bc5a29de06b510f9aa428f1eedba926012b591fef7a518e776a7c9bd1824".to_string(),
                "3f3af1ecebbd1410ab417ec0d27bbfcb5d340e177ae159b59fc8626c2dfd9175".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn save_then_restore_round_trips_and_is_content_addressed() {
        let (store, archive) = archive(64 * 1024);
        let reference = archive.save(&scope(), &messages()).await.unwrap();
        assert_eq!(reference.len(), 64);
        assert!(reference
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        let restored = archive.restore(&scope(), &reference).await.unwrap();
        assert_eq!(restored, messages());

        // The same snapshot is idempotent: the CAS fails, the stored bytes match.
        let again = archive.save(&scope(), &messages()).await.unwrap();
        assert_eq!(again, reference);
        assert_eq!(store.size(), 1);

        // A different snapshot is a different reference in the same namespace.
        let other = archive
            .save(&scope(), &[ContextMessage::user("m3", "third")])
            .await
            .unwrap();
        assert_ne!(other, reference);
        assert_eq!(store.size(), 2);
        assert_eq!(
            archive.restore(&scope(), &other).await.unwrap(),
            vec![ContextMessage::user("m3", "third")]
        );
    }

    #[tokio::test]
    async fn limits_and_references_are_enforced() {
        assert_eq!(
            StoreJevContextArchive::new(Arc::new(InMemoryStore::new()), 0)
                .unwrap_err()
                .message(),
            "positive snapshot limit required"
        );
        let (_store, archive) = archive(16);
        assert_eq!(
            archive
                .save(&scope(), &messages())
                .await
                .unwrap_err()
                .message(),
            "snapshot limit"
        );
        assert_eq!(
            archive
                .restore(&scope(), "nope")
                .await
                .unwrap_err()
                .message(),
            "invalid archive reference"
        );
        let reference = "0".repeat(64);
        assert_eq!(
            archive
                .restore(&scope(), &reference)
                .await
                .unwrap_err()
                .message(),
            "missing or invalid snapshot"
        );
    }

    #[tokio::test]
    async fn a_store_without_create_if_absent_support_is_rejected() {
        let archive = StoreJevContextArchive::new(Arc::new(DeafStore), 4096).unwrap();
        assert_eq!(
            archive
                .save(&scope(), &messages())
                .await
                .unwrap_err()
                .message(),
            "archive requires acknowledged create-if-absent storage"
        );
    }

    #[tokio::test]
    async fn a_snapshot_for_another_scope_is_rejected() {
        let (store, archive) = archive(64 * 1024);
        // Forge a hash-consistent entry whose envelope carries a different scope: the only way
        // the scope guard can fire through an honest CAS writer.
        let other = ArchiveScope::new("user", "agent", "other-session").unwrap();
        let envelope = ArchiveEnvelope::new(other, messages());
        let snapshot = serde_json::to_string(&envelope).unwrap();
        let reference = sha256_hex(snapshot.as_bytes());
        let mut value = StoreValue::new();
        value.insert(SNAPSHOT_KEY.to_string(), Value::String(snapshot));
        assert!(store.put_if_version(&context_namespace(&scope()), &reference, value, 0));

        assert_eq!(
            archive
                .restore(&scope(), &reference)
                .await
                .unwrap_err()
                .message(),
            "snapshot scope mismatch"
        );
    }

    #[tokio::test]
    async fn tampered_bytes_are_rejected() {
        let (store, archive) = archive(64 * 1024);
        let reference = archive.save(&scope(), &messages()).await.unwrap();
        let namespace = context_namespace(&scope());
        // Overwrite the key with different bytes, keeping the reference.
        store.delete(&namespace, &reference);
        let mut value = StoreValue::new();
        value.insert(
            SNAPSHOT_KEY.to_string(),
            Value::String("{\"version\":1}".to_string()),
        );
        store.put(&namespace, &reference, value);
        assert_eq!(
            archive
                .restore(&scope(), &reference)
                .await
                .unwrap_err()
                .message(),
            "missing or invalid snapshot"
        );
    }
}
