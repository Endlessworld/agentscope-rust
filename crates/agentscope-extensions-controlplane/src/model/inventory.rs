//! Instance-level runtime inventory, mirroring `io.agentscope.extensions.controlplane.model.Inventory`.

use serde::{Deserialize, Serialize};

/// A subagent reachable from this instance.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentInfo {
    /// Subagent name.
    pub name: String,
    /// Subagent description.
    #[serde(default)]
    pub description: String,
    /// Tools the subagent exposes.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Workspace isolation mode.
    #[serde(default)]
    pub workspace_mode: String,
    /// Reachable URL, when remote.
    #[serde(default)]
    pub url: String,
    /// Invocation count.
    #[serde(default)]
    pub invoke_count: i64,
    /// Last invocation time, epoch millis.
    #[serde(default)]
    pub last_invoked_at: i64,
}

impl SubagentInfo {
    /// Compact JSON view, omitting empty/default fields.
    pub fn to_json_value(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("name".into(), self.name.clone().into());
        if !self.description.is_empty() {
            out.insert("description".into(), self.description.clone().into());
        }
        if !self.tools.is_empty() {
            out.insert("tools".into(), self.tools.clone().into());
        }
        if !self.workspace_mode.is_empty() {
            out.insert("workspaceMode".into(), self.workspace_mode.clone().into());
        }
        if !self.url.is_empty() {
            out.insert("url".into(), self.url.clone().into());
        }
        if self.invoke_count > 0 {
            out.insert("invokeCount".into(), self.invoke_count.into());
        }
        if self.last_invoked_at > 0 {
            out.insert("lastInvokedAt".into(), self.last_invoked_at.into());
        }
        serde_json::Value::Object(out)
    }
}

/// A workspace known to this instance.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    /// Workspace path.
    pub path: String,
    /// Workspace mode.
    #[serde(default)]
    pub mode: String,
    /// Size in bytes.
    #[serde(default)]
    pub size_bytes: i64,
    /// Owner reference.
    #[serde(default)]
    pub owner_ref: String,
}

/// Instance health summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceHealth {
    /// Whether the instance is healthy.
    pub healthy: bool,
    /// Unhealthy reason, when healthy is false.
    #[serde(default)]
    pub reason: String,
    /// Number of active sessions.
    #[serde(default)]
    pub active_sessions: i64,
}

/// Instance-level runtime inventory.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    /// Reachable subagents.
    #[serde(default)]
    pub subagents: Vec<SubagentInfo>,
    /// Known workspaces.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
    /// Instance health.
    #[serde(default)]
    pub health: Option<InstanceHealth>,
}

impl Inventory {
    /// An inventory with health only (no subagents/workspaces).
    pub fn health_only(active_sessions: i64) -> Self {
        Self {
            subagents: Vec::new(),
            workspaces: Vec::new(),
            health: Some(InstanceHealth {
                healthy: true,
                reason: String::new(),
                active_sessions,
            }),
        }
    }

    /// Compact JSON view served by the contract endpoint.
    pub fn to_json_value(&self) -> serde_json::Value {
        let subagents: Vec<serde_json::Value> =
            self.subagents.iter().map(|s| s.to_json_value()).collect();
        let health = self.health.as_ref().map(|h| {
            serde_json::json!({
                "healthy": h.healthy,
                "reason": h.reason,
                "activeSessions": h.active_sessions,
            })
        });
        serde_json::json!({
            "subagents": subagents,
            "workspaces": self.workspaces,
            "health": health,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_only_has_no_children() {
        let inv = Inventory::health_only(3);
        assert!(inv.subagents.is_empty());
        assert_eq!(inv.health.unwrap().active_sessions, 3);
    }

    #[test]
    fn subagent_json_omits_empty_fields() {
        let s = SubagentInfo {
            name: "researcher".into(),
            ..Default::default()
        };
        let v = s.to_json_value();
        assert_eq!(v["name"], "researcher");
        assert!(v.get("description").is_none());
    }
}
