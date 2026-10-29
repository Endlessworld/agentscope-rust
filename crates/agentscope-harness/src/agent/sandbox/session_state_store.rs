//! Sandbox state store backed by the generic [`AgentStateStore`].
//!
//! Mirrors Java `SessionSandboxStateStore`: sandbox lifecycle state lives in the same backend as
//! the ReActAgent runtime state, so configuring a distributed `AgentStateStore` (Redis / MongoDB /
//! JDBC / ...) automatically gives distributed sandbox resume state — no separate sandbox store
//! has to be wired.
//!
//! The slot is addressed through the two-dimensional `AgentStateStore` model: the userId column is
//! always `None` (sandbox state is agent-scoped, not user-scoped) and the scope is encoded in the
//! sessionId prefix, exactly like the Java `slotSessionId` mapping:
//!
//! | isolation scope | sessionId |
//! |---|---|
//! | `Session` | `sandbox/session/<sessionId>` |
//! | `User` | `sandbox/user/<agentId>/<userId>` |
//! | `Agent` | `sandbox/agent/<agentId>` |
//! | `Global` | `sandbox/global` |

use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::state::AgentStateStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::state::{SandboxState, SandboxStateStore};

/// State key the sandbox slot is stored under. Mirrors Java `SANDBOX_STATE_KEY`.
pub const SANDBOX_STATE_KEY: &str = "_sandbox_state";

/// Slot payload, mirroring Java `SessionSandboxStateStore.SandboxStateSlot`.
///
/// The sandbox state is kept as an opaque JSON **string** (not a nested document) so every
/// `AgentStateStore` backend — including KV/document stores that do not key-escape nested
/// fields — persists it identically. `deleted` is a tombstone flag: not all backends support a
/// per-key delete, so `delete` overwrites the slot instead of removing the row.
#[derive(Debug, Serialize, Deserialize)]
struct SandboxStateSlot {
    json: String,
    #[serde(default)]
    deleted: bool,
}

impl SandboxStateSlot {
    fn present(json: String) -> Self {
        Self {
            json,
            deleted: false,
        }
    }

    fn tombstone() -> Self {
        Self {
            json: String::new(),
            deleted: true,
        }
    }
}

/// [`SandboxStateStore`] backed by an [`AgentStateStore`].
#[derive(Clone)]
pub struct AgentStateStoreSandboxStateStore {
    state_store: Arc<dyn AgentStateStore>,
    agent_id: String,
}

impl AgentStateStoreSandboxStateStore {
    /// Creates the store for one agent.
    pub fn new(state_store: Arc<dyn AgentStateStore>, agent_id: impl Into<String>) -> Self {
        Self {
            state_store,
            agent_id: agent_id.into(),
        }
    }

    /// The agent id used by the `user` / `agent` scope prefixes.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Maps a [`SandboxIsolationKey`](super::SandboxIsolationKey) string onto the
    /// `AgentStateStore` sessionId slot (Java `slotSessionId`).
    ///
    /// A `sub_key` is not part of the Java model and is deliberately dropped: isolation keys
    /// carrying one reuse their parent slot.
    fn slot_session_id(&self, key: &str) -> String {
        let mut parts = key.splitn(3, ':');
        let scope = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        match scope {
            "user" => format!("sandbox/user/{}/{}", self.agent_id, value),
            "agent" => format!("sandbox/agent/{}", self.agent_id),
            "global" => "sandbox/global".to_string(),
            _ => format!("sandbox/session/{value}"),
        }
    }
}

#[async_trait]
impl SandboxStateStore for AgentStateStoreSandboxStateStore {
    async fn save(&self, key: &str, state: &SandboxState) -> Result<()> {
        let json = serde_json::to_string(state)?;
        let slot = serde_json::to_value(SandboxStateSlot::present(json))?;
        self.state_store
            .save(None, &self.slot_session_id(key), SANDBOX_STATE_KEY, slot)
            .await;
        Ok(())
    }

    async fn load(&self, key: &str) -> Result<Option<SandboxState>> {
        let Some(value) = self
            .state_store
            .get(None, &self.slot_session_id(key), SANDBOX_STATE_KEY)
            .await
        else {
            return Ok(None);
        };
        let slot: SandboxStateSlot = serde_json::from_value(value)?;
        if slot.deleted || slot.json.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&slot.json)?))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let slot = serde_json::to_value(SandboxStateSlot::tombstone())?;
        self.state_store
            .save(None, &self.slot_session_id(key), SANDBOX_STATE_KEY, slot)
            .await;
        Ok(())
    }

    /// Lists isolation keys by scanning the session ids of the backing store and mapping the
    /// `sandbox/*` prefixes back onto [`SandboxIsolationKey`](super::SandboxIsolationKey)
    /// strings; slots of other agents are skipped.
    async fn list_keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for session_id in self.state_store.list_session_ids(None).await {
            if let Some(rest) = session_id.strip_prefix("sandbox/session/") {
                keys.push(format!("session:{rest}"));
            } else if session_id == "sandbox/global" {
                keys.push("global:global".to_string());
            } else if let Some(rest) = session_id.strip_prefix("sandbox/agent/") {
                if rest == self.agent_id {
                    keys.push(format!("agent:{rest}"));
                }
            } else if let Some(rest) = session_id.strip_prefix("sandbox/user/") {
                if let Some((agent, value)) = rest.split_once('/') {
                    if agent == self.agent_id {
                        keys.push(format!("user:{value}"));
                    }
                }
            }
        }
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::state::InMemoryAgentStateStore;

    use super::super::state::{RemoteSandboxState, SandboxSnapshotRef};

    fn store() -> (Arc<InMemoryAgentStateStore>, AgentStateStoreSandboxStateStore) {
        let raw = Arc::new(InMemoryAgentStateStore::new());
        let store = AgentStateStoreSandboxStateStore::new(raw.clone(), "agent-1");
        (raw, store)
    }

    fn remote_state(session_id: &str, snapshot_id: Option<&str>) -> SandboxState {
        SandboxState::Remote(RemoteSandboxState {
            session_id: session_id.to_string(),
            endpoint: "http://localhost:8080".to_string(),
            token: None,
            workspace_root_ready: false,
            workspace_projection_hash: None,
            metadata: Default::default(),
            snapshot: snapshot_id.map(|id| SandboxSnapshotRef::Remote { id: id.to_string() }),
        })
    }

    #[tokio::test]
    async fn save_then_load_round_trips_snapshot_identity() {
        let (_raw, store) = store();
        let state = remote_state("s1", Some("snap-1"));
        store.save("session:s1", &state).await.unwrap();

        let loaded = store.load("session:s1").await.unwrap().unwrap();
        assert_eq!(loaded.session_id(), "s1");
        assert_eq!(
            loaded.snapshot(),
            Some(&SandboxSnapshotRef::Remote {
                id: "snap-1".to_string()
            })
        );
    }

    #[tokio::test]
    async fn slot_payload_matches_the_java_shape() {
        let (raw, store) = store();
        store
            .save("session:s1", &remote_state("s1", Some("snap-1")))
            .await
            .unwrap();
        let value = raw
            .get(None, "sandbox/session/s1", SANDBOX_STATE_KEY)
            .await
            .expect("slot written");
        // `{json: "<SandboxState>", deleted: false}` — the exact shape the MongoDB
        // cascade-delete lookup (`states._sandbox_state.json`) reads.
        assert_eq!(value.get("deleted"), Some(&serde_json::json!(false)));
        let json = value.get("json").and_then(|v| v.as_str()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["type"], serde_json::json!("Remote"));
        assert_eq!(parsed["snapshot"], serde_json::json!({"type": "remote", "id": "snap-1"}));
    }

    #[tokio::test]
    async fn delete_writes_a_tombstone_and_load_returns_none() {
        let (raw, store) = store();
        store
            .save("session:s1", &remote_state("s1", None))
            .await
            .unwrap();
        store.delete("session:s1").await.unwrap();

        assert!(store.load("session:s1").await.unwrap().is_none());
        let value = raw
            .get(None, "sandbox/session/s1", SANDBOX_STATE_KEY)
            .await
            .unwrap();
        assert_eq!(value.get("deleted"), Some(&serde_json::json!(true)));
    }

    #[tokio::test]
    async fn scope_prefixes_follow_the_java_layout() {
        let (_raw, store) = store();
        assert_eq!(store.slot_session_id("session:s1"), "sandbox/session/s1");
        assert_eq!(store.slot_session_id("user:u1"), "sandbox/user/agent-1/u1");
        assert_eq!(store.slot_session_id("agent:agent-1"), "sandbox/agent/agent-1");
        assert_eq!(store.slot_session_id("global:global"), "sandbox/global");
    }

    #[tokio::test]
    async fn list_keys_maps_slots_back_to_isolation_keys() {
        let (raw, store) = store();
        store
            .save("session:s1", &remote_state("s1", None))
            .await
            .unwrap();
        store
            .save("user:u1", &remote_state("u1", None))
            .await
            .unwrap();
        store
            .save("global:global", &remote_state("g", None))
            .await
            .unwrap();
        // A slot owned by another agent must be skipped.
        raw.save(None, "sandbox/agent/agent-2", SANDBOX_STATE_KEY, serde_json::json!({
            "json": "",
            "deleted": true
        }))
        .await;

        assert_eq!(
            store.list_keys().await.unwrap(),
            vec![
                "global:global".to_string(),
                "session:s1".to_string(),
                "user:u1".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn legacy_slot_without_snapshot_loads_with_none() {
        let (raw, store) = store();
        let legacy = serde_json::json!({
            "type": "Remote",
            "session_id": "s1",
            "endpoint": "http://localhost:8080",
            "token": null,
            "workspace_root_ready": false,
            "workspace_projection_hash": null,
            "metadata": {}
        })
        .to_string();
        raw.save(
            None,
            "sandbox/session/s1",
            SANDBOX_STATE_KEY,
            serde_json::json!({ "json": legacy, "deleted": false }),
        )
        .await;

        let loaded = store.load("session:s1").await.unwrap().unwrap();
        assert!(loaded.snapshot().is_none());
    }
}
