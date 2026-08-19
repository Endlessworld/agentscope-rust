//! Durable registry of exposed subagents, mirroring `SubagentRecord`, `SubagentRegistry`
//! and `InMemorySubagentRegistry`.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
}
