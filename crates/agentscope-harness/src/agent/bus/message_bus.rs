//! Message transport primitives and the `MessageBus` trait, mirroring
//! `io.agentscope.harness.agent.bus.MessageBus`.
//!
//! Three consumption modes are exposed:
//! - **Mode A (drain queue)** — single-consumer, ack-on-read. Each entry is
//!   returned at most once; storage drops it the moment it is read.
//! - **Mode C (replay log)** — multi-consumer, externally bounded. Each reader
//!   tracks its own cursor; entries persist until trimmed or capped by maxLen.
//! - **Mode D (broadcast)** — fire-and-forget pub/sub. Only currently-subscribed
//!   listeners receive a payload; no history is retained.
//!
//! Domain helpers (`inbox*`, `enqueue_wakeup`, `session*`) map higher-level
//! concepts onto these primitives with a fixed key-naming convention.

use async_trait::async_trait;
use agentscope_core::error::Result;
use serde_json::Value;

/// A single transport entry: the bus-assigned id plus the JSON payload.
#[derive(Debug, Clone, PartialEq)]
pub struct BusEntry {
    pub id: String,
    pub payload: Value,
}

impl BusEntry {
    pub fn new(id: impl Into<String>, payload: Value) -> Self {
        Self { id: id.into(), payload }
    }
}

/// Fixed key prefix for per-session inboxes.
pub const INBOX_KEY_PREFIX: &str = "agentscope:inbox:";
/// Shared drain-queue key for wakeup requests (multi-user isolation via payload).
pub const WAKEUPS_KEY: &str = "agentscope:wakeups";
/// Shared broadcast channel signalling "drain the wakeup queue now".
pub const WAKEUP_SIGNAL_KEY: &str = "agentscope:wakeup_signal";
/// Fixed key prefix for per-session event replay logs.
pub const SESSION_EVENTS_KEY_PREFIX: &str = "agentscope:session:events:";
/// Cap for session event replay logs (oldest entries trimmed beyond this).
pub const SESSION_REPLAY_MAX_LEN: i32 = 1000;

/// Live message transport for coordinating work across sessions and processes.
#[async_trait]
pub trait MessageBus: Send + Sync {
    // ---- Mode A: drain queue (single consumer, ack-on-read) ----

    /// Append a payload to the drain queue at `key`; returns the transport-level
    /// entry id.
    async fn queue_push(&self, key: &str, payload: Value) -> Result<String>;

    /// Drain up to `max_count` entries from the queue at `key`, removing them
    /// atomically. A subsequent call returns only entries that arrived after.
    async fn queue_drain(&self, key: &str, max_count: usize) -> Result<Vec<BusEntry>>;

    /// Delete the drain queue at `key` and all of its entries. Idempotent.
    async fn queue_delete(&self, key: &str) -> Result<()>;

    /// Check whether the drain queue at `key` has any entries, without consuming
    /// them.
    async fn queue_peek(&self, key: &str) -> Result<bool>;

    // ---- Mode C: replay log (multi-consumer, externally bounded) ----

    /// Append a payload to the replay log at `key`, returning the entry id (usable
    /// as a cursor for subsequent reads). Older entries are trimmed when the log
    /// exceeds `max_len` (no cap when `max_len <= 0`).
    async fn log_append(&self, key: &str, payload: Value, max_len: i32) -> Result<String>;

    /// Read up to `max_count` entries from the replay log at `key`, starting
    /// strictly after `since` (or from the beginning when `since` is `None`).
    /// Non-destructive.
    async fn log_read(
        &self,
        key: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>>;

    /// Delete the replay log at `key` and all of its entries. Idempotent.
    async fn log_trim(&self, key: &str) -> Result<()>;

    // ---- Mode D: transient broadcast (fire-and-forget) ----

    /// Publish a payload on the broadcast channel. Only currently-subscribed
    /// listeners receive it.
    async fn publish(&self, key: &str, payload: Value) -> Result<()>;

    /// Subscribe to a broadcast channel, yielding payloads published after the
    /// subscription is established. Dropping the receiver ends the subscription.
    fn subscribe(&self, key: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value>;

    // ---- Domain helpers: Inbox ----

    /// Check whether a session's inbox has pending messages without consuming
    /// them.
    async fn inbox_has_messages(&self, session_id: &str) -> Result<bool> {
        self.queue_peek(&format!("{INBOX_KEY_PREFIX}{session_id}")).await
    }

    /// Push a message into a session's inbox queue.
    async fn inbox_push(&self, session_id: &str, msg: Value) -> Result<()> {
        self.queue_push(&format!("{INBOX_KEY_PREFIX}{session_id}"), msg)
            .await
            .map(|_| ())
    }

    /// Drain pending inbox messages for a session, in arrival order.
    async fn inbox_drain(&self, session_id: &str, max_count: usize) -> Result<Vec<BusEntry>> {
        self.queue_drain(&format!("{INBOX_KEY_PREFIX}{session_id}"), max_count)
            .await
    }

    // ---- Domain helpers: Wakeup ----

    /// Enqueue a wakeup request and signal dispatchers.
    async fn enqueue_wakeup(&self, user_id: &str, session_id: &str, agent_id: &str) -> Result<()> {
        let payload = serde_json::json!({
            "userId": user_id,
            "sessionId": session_id,
            "agentId": agent_id,
        });
        self.queue_push(WAKEUPS_KEY, payload).await?;
        self.publish(WAKEUP_SIGNAL_KEY, serde_json::json!({})).await
    }

    /// Subscribe to the shared wakeup signal channel (each payload means "drain
    /// the wakeup queue now").
    fn subscribe_wakeup(&self) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
        self.subscribe(WAKEUP_SIGNAL_KEY)
    }

    // ---- Domain helpers: Session events ----

    /// Append a session event to its replay log and fan it out live. Late-joining
    /// subscribers can replay from the log; returns the replay-log entry id.
    async fn session_publish_event(&self, session_id: &str, event: Value) -> Result<String> {
        let key = format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}");
        let entry_id = self.log_append(&key, event.clone(), SESSION_REPLAY_MAX_LEN).await?;
        let mut live = event;
        if let Some(obj) = live.as_object_mut() {
            obj.insert("_entry_id".to_string(), Value::String(entry_id.clone()));
        }
        self.publish(&key, live).await?;
        Ok(entry_id)
    }

    /// Read events from a session's replay log for catch-up / reconnection.
    async fn session_read_events(
        &self,
        session_id: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>> {
        self.log_read(&format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}"), since, max_count)
            .await
    }

    /// Live-subscribe to a session's published events.
    fn session_subscribe_events(&self, session_id: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
        self.subscribe(&format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}"))
    }

    /// Trim the session's event replay log so late subscribers don't replay stale
    /// events from a previous run.
    async fn session_trim_events(&self, session_id: &str) -> Result<()> {
        self.log_trim(&format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}")).await
    }
}
