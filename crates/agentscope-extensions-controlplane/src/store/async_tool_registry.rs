//! Control-plane hosted [`AsyncToolRegistry`] over `/api/v1/dp/async-tools/*`,
//! mirroring `ControlPlaneAsyncToolRegistry`.

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::bus::{AsyncToolRecord, AsyncToolRegistry};
use async_trait::async_trait;
use chrono::Duration as ChronoDuration;
use reqwest::Method;
use serde_json::Value;

use crate::store::control_plane_stores::tenant_query;
use crate::transport::{encode_path_segment, require_ok, ControlPlaneHttpClient};

/// Control-plane hosted [`AsyncToolRegistry`].
#[derive(Clone)]
pub struct ControlPlaneAsyncToolRegistry {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
}

impl ControlPlaneAsyncToolRegistry {
    /// Creates a registry bound to the given client.
    pub fn new(
        http: Arc<ControlPlaneHttpClient>,
        agent_name: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            http,
            agent_name: agent_name.into(),
            namespace: namespace.into(),
        }
    }

    fn body(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("agentName".into(), self.agent_name.clone().into());
        m.insert("namespace".into(), self.namespace.clone().into());
        m
    }

    async fn post(&self, path: &str, body: Value, op: &str) -> Result<String> {
        let resp = self
            .http
            .send(Method::POST, path, Some(body))
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, op).map_err(|e| Error::Other(e.to_string()))?;
        Ok(resp.body)
    }

    fn record_from_json(v: &Value) -> AsyncToolRecord {
        let created = v
            .get("createdAt")
            .and_then(|c| c.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        AsyncToolRecord {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            session_id: v
                .get("sessionId")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            tool_name: v
                .get("toolName")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            tool_call_id: v
                .get("toolCallId")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            created_at: created,
        }
    }
}

#[async_trait]
impl AsyncToolRegistry for ControlPlaneAsyncToolRegistry {
    async fn register(&self, record: AsyncToolRecord) -> Result<()> {
        let mut body = self.body();
        body.insert("id".into(), record.id.into());
        body.insert("sessionId".into(), record.session_id.into());
        body.insert("toolName".into(), record.tool_name.into());
        body.insert("toolCallId".into(), record.tool_call_id.into());
        body.insert("status".into(), record.status.into());
        body.insert("createdAt".into(), record.created_at.to_rfc3339().into());
        self.post("/api/v1/dp/async-tools", Value::Object(body), "register")
            .await?;
        Ok(())
    }

    async fn complete(&self, id: &str, result: &str) -> Result<()> {
        let mut body = self.body();
        body.insert("result".into(), result.into());
        let path = format!(
            "/api/v1/dp/async-tools/{}/complete",
            encode_path_segment(id)
        );
        self.post(&path, Value::Object(body), "complete").await?;
        Ok(())
    }

    async fn fail(&self, id: &str, error: &str) -> Result<()> {
        let mut body = self.body();
        body.insert("error".into(), error.into());
        let path = format!("/api/v1/dp/async-tools/{}/fail", encode_path_segment(id));
        self.post(&path, Value::Object(body), "fail").await?;
        Ok(())
    }

    async fn find_stale(
        &self,
        session_id: &str,
        ttl: ChronoDuration,
    ) -> Result<Vec<AsyncToolRecord>> {
        let path = format!(
            "/api/v1/dp/async-tools/stale?{}&sessionId={}&ttlSeconds={}",
            tenant_query(&self.agent_name, &self.namespace),
            crate::transport::encode_query(session_id),
            ttl.num_seconds().max(0)
        );
        let resp = self
            .http
            .send(Method::GET, &path, None)
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, "findStale").map_err(|e| Error::Other(e.to_string()))?;
        let value: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
        Ok(value
            .get("records")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().map(Self::record_from_json).collect())
            .unwrap_or_default())
    }

    async fn mark_timeout(&self, id: &str) -> Result<()> {
        let body = self.body();
        let path = format!("/api/v1/dp/async-tools/{}/timeout", encode_path_segment(id));
        self.post(&path, Value::Object(body), "markTimeout").await?;
        Ok(())
    }

    async fn list_session(&self, session_id: &str) -> Result<Vec<AsyncToolRecord>> {
        let path = format!(
            "/api/v1/dp/async-tools?{}&sessionId={}",
            tenant_query(&self.agent_name, &self.namespace),
            crate::transport::encode_query(session_id)
        );
        let resp = self
            .http
            .send(Method::GET, &path, None)
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, "listSession").map_err(|e| Error::Other(e.to_string()))?;
        let value: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
        Ok(value
            .get("records")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().map(Self::record_from_json).collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_record_from_json() {
        let v = serde_json::json!({
            "id": "r1",
            "sessionId": "s1",
            "toolName": "shell",
            "toolCallId": "c1",
            "status": "RUNNING",
            "createdAt": "2026-01-01T00:00:00Z"
        });
        let r = ControlPlaneAsyncToolRegistry::record_from_json(&v);
        assert_eq!(r.id, "r1");
        assert_eq!(r.status, "RUNNING");
    }
}
