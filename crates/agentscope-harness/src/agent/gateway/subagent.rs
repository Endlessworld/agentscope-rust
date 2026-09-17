//! Durable registry of exposed subagents, mirroring `SubagentRecord`, `SubagentRegistry`,
//! `InMemorySubagentRegistry` and `StoreBackedSubagentRegistry`.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::filesystem::remote::store::BaseStore;

/// Milliseconds since the UNIX epoch.
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Durable, recovery-oriented descriptor of an exposed subagent: the recipe needed
/// to re-materialize and route to the subagent on any node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRecord {
    pub subagent_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub user_id: Option<String>,
    pub parent_session_id: Option<String>,
    /// Creation time as milliseconds since the UNIX epoch.
    pub created_at: i64,
    /// Optional TTL as milliseconds since the UNIX epoch.
    pub expires_at: Option<i64>,
}

impl SubagentRecord {
    pub fn new(
        subagent_id: impl Into<String>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        user_id: Option<String>,
        parent_session_id: Option<String>,
        expires_at: Option<i64>,
    ) -> Self {
        Self {
            subagent_id: subagent_id.into(),
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            user_id,
            parent_session_id,
            created_at: now_millis(),
            expires_at,
        }
    }

    /// Whether this record has a TTL that has already elapsed relative to `now`.
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.map(|e| now > e).unwrap_or(false)
    }

    /// Serializes this record into a store value map.
    pub fn to_store_value(&self) -> crate::filesystem::remote::store::StoreValue {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => crate::filesystem::remote::store::StoreValue::new(),
        }
    }

    /// Deserializes a record from a store value map.
    pub fn from_store_value(
        value: &crate::filesystem::remote::store::StoreValue,
    ) -> Option<Self> {
        serde_json::from_value(serde_json::Value::Object(value.clone())).ok()
    }
}

/// Durable registry of exposed subagents, keyed by `subagentId`.
pub trait SubagentRegistry: Send + Sync {
    /// Persists an exposure record. Idempotent on `subagent_id`.
    fn register(&self, record: SubagentRecord);
    /// Resolves a subagent handle; treats an expired record as absent.
    fn find(&self, subagent_id: &str) -> Option<SubagentRecord>;
    /// Removes a single exposure record. No-op when unknown.
    fn revoke(&self, subagent_id: &str);
    /// Removes all exposure records for the given parent session. Optional capability.
    fn revoke_by_parent_session(&self, _parent_session_id: &str) {}
}

/// Default single-process `SubagentRegistry`. Records live only for the process
/// lifetime (legacy in-memory behaviour).
pub struct InMemorySubagentRegistry {
    records: Mutex<HashMap<String, SubagentRecord>>,
}

impl InMemorySubagentRegistry {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemorySubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentRegistry for InMemorySubagentRegistry {
    fn register(&self, record: SubagentRecord) {
        if !record.subagent_id.is_empty() {
            self.records.lock().unwrap().insert(record.subagent_id.clone(), record);
        }
    }

    fn find(&self, subagent_id: &str) -> Option<SubagentRecord> {
        if subagent_id.is_empty() {
            return None;
        }
        let r = self.records.lock().unwrap().get(subagent_id).cloned()?;
        if r.is_expired(now_millis()) {
            self.revoke(subagent_id);
            return None;
        }
        Some(r)
    }

    fn revoke(&self, subagent_id: &str) {
        self.records.lock().unwrap().remove(subagent_id);
    }

    fn revoke_by_parent_session(&self, parent_session_id: &str) {
        self.records
            .lock()
            .unwrap()
            .retain(|_, r| r.parent_session_id.as_deref() != Some(parent_session_id));
    }
}

/// Distributed [`SubagentRegistry`] backed by a [`BaseStore`].
///
/// Exposure records become visible to every node, so any replica can resolve a
/// `subagent_id` and re-materialize the subagent. Records live under the namespace
/// `["subagents", "exposed"]` keyed by `subagent_id`; TTL is enforced lazily on
/// [`SubagentRegistry::find`].
pub struct StoreBackedSubagentRegistry {
    store: Arc<dyn BaseStore>,
}

const NAMESPACE: [&str; 2] = ["subagents", "exposed"];
const SCAN_PAGE_SIZE: usize = 1000;

impl StoreBackedSubagentRegistry {
    pub fn new(store: Arc<dyn BaseStore>) -> Self {
        Self { store }
    }

    fn namespace() -> Vec<String> {
        NAMESPACE.iter().map(|s| s.to_string()).collect()
    }
}

impl SubagentRegistry for StoreBackedSubagentRegistry {
    fn register(&self, record: SubagentRecord) {
        if record.subagent_id.is_empty() {
            return;
        }
        self.store
            .put(&Self::namespace(), &record.subagent_id, record.to_store_value());
    }

    fn find(&self, subagent_id: &str) -> Option<SubagentRecord> {
        if subagent_id.is_empty() {
            return None;
        }
        let item = self.store.get(&Self::namespace(), subagent_id)?;
        let record = SubagentRecord::from_store_value(&item.value)?;
        if record.is_expired(now_millis()) {
            self.revoke(subagent_id);
            return None;
        }
        Some(record)
    }

    fn revoke(&self, subagent_id: &str) {
        if subagent_id.is_empty() {
            return;
        }
        self.store.delete(&Self::namespace(), subagent_id);
    }

    fn revoke_by_parent_session(&self, parent_session_id: &str) {
        if parent_session_id.is_empty() {
            return;
        }
        for item in self.store.search(&Self::namespace(), SCAN_PAGE_SIZE, 0) {
            if let Some(r) = SubagentRecord::from_store_value(&item.value) {
                if r.parent_session_id.as_deref() == Some(parent_session_id) {
                    self.revoke(&r.subagent_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_find_revoke_roundtrip() {
        let reg = InMemorySubagentRegistry::new();
        let rec = SubagentRecord::new(
            "sub-1",
            "coder",
            "sess-1",
            Some("alice".into()),
            Some("parent-1".into()),
            None,
        );
        reg.register(rec.clone());
        assert_eq!(reg.find("sub-1").unwrap(), rec);
        reg.revoke("sub-1");
        assert!(reg.find("sub-1").is_none());
    }

    #[test]
    fn expired_record_is_treated_absent() {
        let reg = InMemorySubagentRegistry::new();
        reg.register(SubagentRecord::new(
            "sub-exp",
            "coder",
            "sess-1",
            None,
            None,
            Some(now_millis() - 1000),
        ));
        assert!(reg.find("sub-exp").is_none());
    }

    #[test]
    fn revoke_by_parent_session_removes_group() {
        let reg = InMemorySubagentRegistry::new();
        reg.register(SubagentRecord::new("a", "x", "s1", None, Some("p1".into()), None));
        reg.register(SubagentRecord::new("b", "x", "s2", None, Some("p1".into()), None));
        reg.register(SubagentRecord::new("c", "x", "s3", None, None, None));
        reg.revoke_by_parent_session("p1");
        assert!(reg.find("a").is_none());
        assert!(reg.find("b").is_none());
        assert!(reg.find("c").is_some());
    }

    #[test]
    fn store_backed_registry_roundtrip() {
        let store: Arc<dyn BaseStore> =
            Arc::new(crate::filesystem::remote::store::InMemoryStore::new());
        let reg = StoreBackedSubagentRegistry::new(store);
        let rec = SubagentRecord::new(
            "sub-1",
            "coder",
            "sess-1",
            Some("alice".into()),
            Some("parent-1".into()),
            None,
        );
        reg.register(rec.clone());
        assert_eq!(reg.find("sub-1").unwrap(), rec);
        reg.revoke("sub-1");
        assert!(reg.find("sub-1").is_none());
    }

    #[test]
    fn store_backed_registry_enforces_ttl_and_parent_revoke() {
        let store: Arc<dyn BaseStore> =
            Arc::new(crate::filesystem::remote::store::InMemoryStore::new());
        let reg = StoreBackedSubagentRegistry::new(store);
        reg.register(SubagentRecord::new("a", "x", "s1", None, Some("p1".into()), None));
        reg.register(SubagentRecord::new("b", "x", "s2", None, Some("p1".into()), None));
        reg.register(SubagentRecord::new(
            "exp",
            "x",
            "s3",
            None,
            None,
            Some(now_millis() - 1000),
        ));
        reg.revoke_by_parent_session("p1");
        assert!(reg.find("a").is_none());
        assert!(reg.find("b").is_none());
        assert!(reg.find("exp").is_none());
    }
}
