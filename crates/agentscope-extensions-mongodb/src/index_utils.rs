//! Index-creation helper with automatic migration on index-options conflict,
//! mirroring `io.agentscope.extensions.mongodb.MongoIndexUtils`.
//!
//! Because the Rust crate is driver-agnostic (see [`crate::client`]), index
//! creation is expressed against a small [`IndexSpec`] descriptor and delegated to
//! the [`crate::client::MongoClient`] backend. The migration algorithm itself —
//! dropping a conflicting index by its *actual* name found via key-pattern match,
//! then re-creating it, tolerating concurrent multi-node races — is preserved
//! faithfully in [`create_index_with_migration`].

use std::sync::Arc;

use serde_json::{json, Value};

use crate::client::{MongoClient, MongoError};

/// Maximum number of migration attempts before propagating the conflict.
pub const MAX_MIGRATION_ATTEMPTS: usize = 3;

/// MongoDB `IndexOptionsConflict` error code.
const ERROR_INDEX_OPTIONS_CONFLICT: i32 = 85;
/// MongoDB `IndexNotFound` / `NamespaceNotFound` error codes tolerated on drop.
const ERROR_INDEX_NOT_FOUND: i32 = 27;

/// A backend-agnostic index descriptor.
///
/// `keys` is the ordered key pattern (e.g. `[("user_id", 1), ("session_id", 1)]`),
/// `unique` / `sparse` mirror the corresponding options, and `expire_after_seconds`
/// is the TTL (present only for TTL indexes).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    /// Ordered `(field, direction)` pairs; direction `1` = ascending, `-1` = descending.
    pub keys: Vec<(String, i32)>,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
    /// Whether the index is sparse.
    pub sparse: bool,
    /// TTL duration in seconds, if this is a TTL index.
    pub expire_after_seconds: Option<i64>,
}

impl IndexSpec {
    /// Creates an ascending index over the given fields.
    pub fn ascending(fields: &[&str]) -> Self {
        Self {
            keys: fields.iter().map(|f| ((*f).to_string(), 1)).collect(),
            unique: false,
            sparse: false,
            expire_after_seconds: None,
        }
    }

    /// Creates a compound ascending index over the given fields.
    pub fn compound_ascending(fields: &[&str]) -> Self {
        Self::ascending(fields)
    }

    /// Marks the index as sparse.
    pub fn sparse(mut self) -> Self {
        self.sparse = true;
        self
    }

    /// Marks the index as unique.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Makes this a TTL index expiring documents after `seconds`.
    pub fn expire_after_seconds(mut self, seconds: i64) -> Self {
        self.expire_after_seconds = Some(seconds);
        self
    }
}

/// Creates an index, migrating if an existing index has conflicting options
/// (error 85).
///
/// Equivalent to Java `MongoIndexUtils.createIndexWithMigration`:
/// 1. Try `create_index`; on error 85 fall through to migration.
/// 2. Find the existing index whose key pattern matches, drop it by its actual
///    name (tolerating "index not found" if another node dropped it first).
/// 3. Re-create the index, retrying up to [`MAX_MIGRATION_ATTEMPTS`] times to
///    handle concurrent multi-node startup races.
///
/// The drop + recreate path is fault-tolerant for multi-node startup: if another
/// node concurrently drops or recreates the same index, the operation degrades to
/// a harmless no-op rather than failing application bootstrap.
pub async fn create_index_with_migration(
    client: &Arc<dyn MongoClient>,
    collection: &str,
    spec: &IndexSpec,
) -> Result<(), MongoError> {
    match client.create_index(collection, spec).await {
        Ok(()) => Ok(()),
        Err(e) if e.code() == Some(ERROR_INDEX_OPTIONS_CONFLICT) => {
            migrate_conflicting_index(client, collection, spec).await
        }
        Err(e) => Err(e),
    }
}

async fn migrate_conflicting_index(
    client: &Arc<dyn MongoClient>,
    collection: &str,
    spec: &IndexSpec,
) -> Result<(), MongoError> {
    let mut conflicting_name = find_index_name_by_key_pattern(client, collection, spec).await?;
    if let Some(name) = conflicting_name.take() {
        tracing::info!(
            target = collection,
            index = %name,
            "[mongo-index] Index conflict (error 85), dropping and recreating with new options"
        );
        drop_index_tolerant(client, collection, &name).await?;
    }

    let mut attempt = 1usize;
    loop {
        match client.create_index(collection, spec).await {
            Ok(()) => return Ok(()),
            Err(e) if e.code() == Some(ERROR_INDEX_OPTIONS_CONFLICT) => {
                if attempt < MAX_MIGRATION_ATTEMPTS {
                    tracing::warn!(
                        target = collection,
                        attempt,
                        max = MAX_MIGRATION_ATTEMPTS,
                        "[mongo-index] Concurrent conflict (error 85) — re-dropping and retrying"
                    );
                    if let Some(current) =
                        find_index_name_by_key_pattern(client, collection, spec).await?
                    {
                        drop_index_tolerant(client, collection, &current).await?;
                    }
                    attempt += 1;
                } else {
                    tracing::warn!(
                        target = collection,
                        max = MAX_MIGRATION_ATTEMPTS,
                        "[mongo-index] Exhausted migration attempts — propagating final error 85"
                    );
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Drops an index by name, tolerating errors that occur when another node
/// concurrently dropped the same index.
async fn drop_index_tolerant(
    client: &Arc<dyn MongoClient>,
    collection: &str,
    name: &str,
) -> Result<(), MongoError> {
    match client.drop_index(collection, name).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if e.code() == Some(ERROR_INDEX_NOT_FOUND)
                || msg.contains("ns not found")
                || msg.contains("index not found")
            {
                tracing::debug!(target = collection, index = %name,
                    "[mongo-index] Index already dropped by another node");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Finds the name of an existing index whose key pattern matches `spec`.
async fn find_index_name_by_key_pattern(
    client: &Arc<dyn MongoClient>,
    collection: &str,
    spec: &IndexSpec,
) -> Result<Option<String>, MongoError> {
    for index in client.list_indexes(collection).await? {
        if index.keys == spec.keys {
            return Ok(Some(index.name));
        }
    }
    Ok(None)
}

/// Serialises an [`IndexSpec`] key pattern into a JSON document (for backends that
/// need a BSON/JSON key document).
pub fn key_pattern_json(spec: &IndexSpec) -> Value {
    let mut map = serde_json::Map::new();
    for (field, dir) in &spec.keys {
        map.insert(field.clone(), json!(dir));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{InMemoryMongoClient, IndexInfo};

    #[tokio::test]
    async fn creates_index_when_absent() {
        let client: Arc<dyn MongoClient> = Arc::new(InMemoryMongoClient::new());
        let spec = IndexSpec::compound_ascending(&["user_id", "session_id"]);
        create_index_with_migration(&client, "sessions", &spec).await.unwrap();
        let indexes = client.list_indexes("sessions").await.unwrap();
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].keys, spec.keys);
    }

    #[tokio::test]
    async fn idempotent_create_is_noop() {
        let client: Arc<dyn MongoClient> = Arc::new(InMemoryMongoClient::new());
        let spec = IndexSpec::compound_ascending(&["user_id", "session_id"]);
        create_index_with_migration(&client, "sessions", &spec).await.unwrap();
        create_index_with_migration(&client, "sessions", &spec).await.unwrap();
        assert_eq!(client.list_indexes("sessions").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn migrates_conflicting_index_by_renamed_name() {
        let client: Arc<dyn MongoClient> = Arc::new(InMemoryMongoClient::new());
        // Simulate a DBA-renamed index with different options (no TTL).
        client
            .insert_raw_index(
                "sessions",
                IndexInfo {
                    name: "custom_idx_name".to_string(),
                    keys: vec![("_updated_at".to_string(), 1)],
                    unique: false,
                    sparse: false,
                    expire_after_seconds: None,
                },
            )
            .await
            .unwrap();
        // Now create the same key pattern but as a sparse TTL index → error 85 path.
        let spec = IndexSpec::ascending(&["_updated_at"])
            .sparse()
            .expire_after_seconds(86_400);
        create_index_with_migration(&client, "sessions", &spec).await.unwrap();
        let indexes = client.list_indexes("sessions").await.unwrap();
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].expire_after_seconds, Some(86_400));
        assert!(indexes[0].sparse);
    }

    #[test]
    fn key_pattern_json_shape() {
        let spec = IndexSpec::ascending(&["a", "b"]);
        assert_eq!(key_pattern_json(&spec), json!({ "a": 1, "b": 1 }));
    }

    #[tokio::test]
    async fn migration_recovers_after_transient_conflicts() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let client: Arc<dyn MongoClient> = raw.clone();
        // Two forced conflicts: the initial create plus one retry fail, then a later
        // attempt succeeds once the racing node stops re-creating the index.
        raw.force_index_conflicts(2);
        let spec = IndexSpec::ascending(&["_updated_at"]).sparse();
        create_index_with_migration(&client, "sessions", &spec)
            .await
            .unwrap();
        let indexes = client.list_indexes("sessions").await.unwrap();
        assert_eq!(indexes.len(), 1);
        assert!(indexes[0].sparse);
    }

    #[tokio::test]
    async fn migration_gives_up_after_max_attempts() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let client: Arc<dyn MongoClient> = raw.clone();
        // Persistently conflicting node: every attempt fails with error 85. The helper
        // must stop after MAX_MIGRATION_ATTEMPTS and surface the final error rather
        // than looping forever.
        raw.force_index_conflicts(MAX_MIGRATION_ATTEMPTS + 1);
        let spec = IndexSpec::ascending(&["_updated_at"]).sparse();
        let err = create_index_with_migration(&client, "sessions", &spec)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some(85));
        assert!(client.list_indexes("sessions").await.unwrap().is_empty());
    }
}
