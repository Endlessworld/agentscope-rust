//! Control-plane hosted [`MessageBus`] over `/api/v1/dp/bus/*`, mirroring
//! `ControlPlaneMessageBus`.
//!
//! Mode D (`publish`/`subscribe`) is backed by a process-local broadcast channel as a
//! degraded fallback (the Java version uses a 2s client-side poll of a bounded replay
//! log; a local broadcast preserves the same fire-and-forget semantics for a single
//! process).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::bus::{BusEntry, MessageBus, SESSION_REPLAY_MAX_LEN};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::transport::{require_ok, ControlPlaneHttpClient};

/// Control-plane hosted [`MessageBus`].
#[derive(Clone)]
pub struct ControlPlaneMessageBus {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
    broadcasts: Arc<Mutex<HashMap<String, broadcast::Sender<Value>>>>,
}

impl ControlPlaneMessageBus {
    /// Creates a bus bound to the given client.
    pub fn new(
        http: Arc<ControlPlaneHttpClient>,
        agent_name: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            http,
            agent_name: agent_name.into(),
            namespace: namespace.into(),
            broadcasts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn tenant_body(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("agentName".into(), self.agent_name.clone().into());
        m.insert("namespace".into(), self.namespace.clone().into());
        m
    }

    fn sender(&self, key: &str) -> broadcast::Sender<Value> {
        let mut map = self.broadcasts.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(key.to_string())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    fn parse_entries(body: &str) -> Vec<BusEntry> {
        let value: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        value
            .get("entries")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let id = e.get("id")?.as_str()?.to_string();
                        let payload = e.get("payload").cloned().unwrap_or(Value::Null);
                        Some(BusEntry::new(id, payload))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn post_json(&self, path: &str, body: Value, op: &str) -> Result<Value> {
        let resp = self
            .http
            .send(Method::POST, path, Some(body))
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, op).map_err(|e| Error::Other(e.to_string()))?;
        serde_json::from_str(&resp.body).map_err(|e| Error::Other(e.to_string()))
    }
}

#[async_trait]
impl MessageBus for ControlPlaneMessageBus {
    async fn queue_push(&self, key: &str, payload: Value) -> Result<String> {
        let mut body = self.tenant_body();
        body.insert("key".into(), key.into());
        body.insert("payload".into(), payload);
        let out = self
            .post_json(
                "/api/v1/dp/bus/queue/push",
                Value::Object(body),
                "queuePush",
            )
            .await?;
        Ok(out
            .get("entryId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn queue_drain(&self, key: &str, max_count: usize) -> Result<Vec<BusEntry>> {
        let mut body = self.tenant_body();
        body.insert("key".into(), key.into());
        body.insert("maxCount".into(), (max_count as i64).into());
        let resp = self
            .http
            .send(
                Method::POST,
                "/api/v1/dp/bus/queue/drain",
                Some(Value::Object(body)),
            )
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, "queueDrain").map_err(|e| Error::Other(e.to_string()))?;
        Ok(Self::parse_entries(&resp.body))
    }

    async fn queue_delete(&self, key: &str) -> Result<()> {
        let mut body = self.tenant_body();
        body.insert("key".into(), key.into());
        let resp = self
            .http
            .send(
                Method::POST,
                "/api/v1/dp/bus/queue/delete",
                Some(Value::Object(body)),
            )
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, "queueDelete").map_err(|e| Error::Other(e.to_string()))
    }

    async fn queue_peek(&self, key: &str) -> Result<bool> {
        let mut body = self.tenant_body();
        body.insert("key".into(), key.into());
        let resp = self
            .http
            .send(
                Method::POST,
                "/api/v1/dp/bus/queue/peek",
                Some(Value::Object(body)),
            )
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, "queuePeek").map_err(|e| Error::Other(e.to_string()))?;
        let v: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
        Ok(v.get("nonEmpty").and_then(|b| b.as_bool()).unwrap_or(false))
    }

    async fn log_append(&self, key: &str, payload: Value, max_len: i32) -> Result<String> {
        let mut body = self.tenant_body();
        body.insert("key".into(), key.into());
        body.insert("payload".into(), payload);
        let effective = if max_len <= 0 {
            SESSION_REPLAY_MAX_LEN
        } else {
            max_len
        };
        body.insert("maxLen".into(), (effective as i64).into());
        let out = self
            .post_json(
                "/api/v1/dp/bus/log/append",
                Value::Object(body),
                "logAppend",
            )
            .await?;
        Ok(out
            .get("entryId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn log_read(
        &self,
        key: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>> {
        let mut body = self.tenant_body();
        body.insert("key".into(), key.into());
        body.insert("maxCount".into(), (max_count as i64).into());
        if let Some(s) = since {
            body.insert("since".into(), s.into());
        }
        let resp = self
            .http
            .send(
                Method::POST,
                "/api/v1/dp/bus/log/read",
                Some(Value::Object(body)),
            )
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, "logRead").map_err(|e| Error::Other(e.to_string()))?;
        Ok(Self::parse_entries(&resp.body))
    }

    async fn log_trim(&self, key: &str) -> Result<()> {
        let mut body = self.tenant_body();
        body.insert("key".into(), key.into());
        let resp = self
            .http
            .send(
                Method::POST,
                "/api/v1/dp/bus/log/trim",
                Some(Value::Object(body)),
            )
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        require_ok(&resp, "logTrim").map_err(|e| Error::Other(e.to_string()))
    }

    async fn publish(&self, key: &str, payload: Value) -> Result<()> {
        // Degraded local broadcast: the control plane does not offer a native
        // fire-and-forget broadcast primitive in the Java client either.
        let _ = self.sender(key).send(payload);
        Ok(())
    }

    fn subscribe(&self, key: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
        let mut rx = self.sender(key).subscribe();
        let (tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(v) => {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        out_rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries() {
        let body = r#"{"entries":[{"id":"e1","payload":{"a":1}},{"id":"e2","payload":null}]}"#;
        let entries = ControlPlaneMessageBus::parse_entries(body);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "e1");
    }

    #[test]
    fn tenant_query_is_built() {
        let q = crate::store::control_plane_stores::tenant_query("friday", "default");
        assert_eq!(q, "agentName=friday&namespace=default");
    }
}
