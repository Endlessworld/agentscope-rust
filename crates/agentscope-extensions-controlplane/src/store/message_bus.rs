//! Control-plane hosted [`MessageBus`] over `/api/v1/dp/bus/*`, mirroring
//! `ControlPlaneMessageBus`.
//!
//! Mode D (`publish`/`subscribe`) is implemented as a bounded replay log with a 2s
//! client-side poll (Java `PUBLISH_MAX_LEN` / `SUBSCRIBE_POLL`): `publish` appends to the
//! `/api/v1/dp/bus/log` log and each subscriber polls it with its own `entryId` cursor, so
//! pub/sub reaches other processes and other instances of the same agent.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::bus::{BusEntry, MessageBus, SESSION_REPLAY_MAX_LEN};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::Value;

use crate::transport::{require_ok, ControlPlaneHttpClient};

/// Replay-log cap `publish` applies (`ControlPlaneMessageBus.PUBLISH_MAX_LEN`).
pub const PUBLISH_MAX_LEN: i32 = 100;
/// Subscriber poll period (`ControlPlaneMessageBus.SUBSCRIBE_POLL`).
pub const SUBSCRIBE_POLL: Duration = Duration::from_secs(2);
/// Entries read per poll (`ControlPlaneMessageBus.subscribe` reads 50).
pub const SUBSCRIBE_BATCH: usize = 50;

/// Control-plane hosted [`MessageBus`].
#[derive(Clone)]
pub struct ControlPlaneMessageBus {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
    poll_interval: Duration,
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
            poll_interval: SUBSCRIBE_POLL,
        }
    }

    /// Overrides the subscriber poll period (tests use a few milliseconds so no real 2s
    /// wait is needed).
    #[doc(hidden)]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    fn tenant_body(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("agentName".into(), self.agent_name.clone().into());
        m.insert("namespace".into(), self.namespace.clone().into());
        m
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
        // Native fire-and-forget broadcast does not exist on the control plane either:
        // Java appends to the bounded replay log and lets subscribers poll it.
        self.log_append(key, payload, PUBLISH_MAX_LEN).await.map(|_| ())
    }

    fn subscribe(&self, key: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
        let (tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let this = self.clone();
        let key = key.to_string();
        tokio::spawn(async move {
            // Each subscriber keeps its own cursor: the replay log is shared, so a second
            // subscriber (or another process) reads the same entries independently.
            let mut cursor: Option<String> = None;
            let mut ticker = tokio::time::interval(this.poll_interval);
            ticker.tick().await; // `Flux.interval` fires after the first period
            loop {
                ticker.tick().await;
                let entries = match this.log_read(&key, cursor.as_deref(), SUBSCRIBE_BATCH).await {
                    Ok(entries) => entries,
                    // A failed poll is skipped, not fatal (Java `onErrorResume(empty)`).
                    Err(_) => continue,
                };
                for entry in entries {
                    cursor = Some(entry.id);
                    if tx.send(entry.payload).is_err() {
                        return; // the receiver was dropped: end the subscription
                    }
                }
            }
        });
        out_rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::{extract::State, routing::post, Json, Router};

    /// Stub control plane recording bus traffic and serving a shared replay log.
    #[derive(Default)]
    struct BusStub {
        appends: Mutex<Vec<Value>>,
        reads: Mutex<Vec<Value>>,
        /// Entries another process appended; never published through this crate.
        log: Mutex<Vec<Value>>,
    }

    type SharedStub = Arc<BusStub>;

    async fn append(State(stub): State<SharedStub>, Json(body): Json<Value>) -> Json<Value> {
        stub.appends.lock().unwrap().push(body);
        Json(serde_json::json!({ "entryId": "e-appended" }))
    }

    async fn read(State(stub): State<SharedStub>, Json(body): Json<Value>) -> Json<Value> {
        stub.reads.lock().unwrap().push(body.clone());
        let since = body.get("since").and_then(|v| v.as_str()).map(str::to_string);
        let log = stub.log.lock().unwrap().clone();
        let start = match since {
            Some(id) => log
                .iter()
                .position(|entry| entry["id"].as_str() == Some(id.as_str()))
                .map(|index| index + 1)
                .unwrap_or(0),
            None => 0,
        };
        Json(serde_json::json!({ "entries": log[start.min(log.len())..].to_vec() }))
    }

    async fn bus_stub() -> (SharedStub, u16) {
        let stub: SharedStub = Arc::new(BusStub::default());
        let app = Router::new()
            .route("/api/v1/dp/bus/log/append", post(append))
            .route("/api/v1/dp/bus/log/read", post(read))
            .with_state(Arc::clone(&stub));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (stub, port)
    }

    fn bus_for(port: u16) -> ControlPlaneMessageBus {
        let http = Arc::new(
            ControlPlaneHttpClient::new(format!("http://127.0.0.1:{port}"), "tok").unwrap(),
        );
        ControlPlaneMessageBus::new(http, "friday", "default")
    }

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

    /// CP-08: `publish` is a bounded replay-log append, not a process-local broadcast.
    #[tokio::test]
    async fn publish_appends_to_the_bounded_replay_log() {
        let (stub, port) = bus_stub().await;
        bus_for(port)
            .publish("chan", serde_json::json!({ "n": 1 }))
            .await
            .unwrap();

        let appends = stub.appends.lock().unwrap().clone();
        assert_eq!(appends.len(), 1);
        assert_eq!(appends[0]["agentName"], "friday");
        assert_eq!(appends[0]["namespace"], "default");
        assert_eq!(appends[0]["key"], "chan");
        assert_eq!(appends[0]["payload"]["n"], 1);
        assert_eq!(appends[0]["maxLen"], PUBLISH_MAX_LEN);
    }

    /// CP-08: `subscribe` polls the shared log, so entries published by another process
    /// are delivered, and its cursor advances by `entryId`.
    #[tokio::test]
    async fn subscribe_polls_the_shared_log_across_processes() {
        let (stub, port) = bus_stub().await;
        *stub.log.lock().unwrap() = vec![
            serde_json::json!({ "id": "e1", "payload": { "n": 1 } }),
            serde_json::json!({ "id": "e2", "payload": { "n": 2 } }),
        ];
        let bus = bus_for(port).with_poll_interval(Duration::from_millis(20));
        let mut rx = bus.subscribe("chan");

        for expected in [1, 2] {
            let payload = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("poll did not deliver the entry")
                .expect("subscription closed early");
            assert_eq!(payload["n"], expected);
        }

        // The next poll asks for everything strictly after the last delivered entry.
        let mut last_since = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let reads = stub.reads.lock().unwrap().clone();
            last_since = reads
                .last()
                .and_then(|body| body.get("since"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if last_since.as_deref() == Some("e2") {
                break;
            }
        }
        assert_eq!(last_since.as_deref(), Some("e2"));
        let reads = stub.reads.lock().unwrap().clone();
        assert_eq!(reads[0]["maxCount"], SUBSCRIBE_BATCH as i64);
        assert!(reads[0].get("since").is_none());
    }
}
