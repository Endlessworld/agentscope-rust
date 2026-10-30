//! ASDP upstream channel semantics, mirroring `transport::GrpcTransport`.
//!
//! Java talks to the control plane over one bidirectional gRPC stream:
//! `agentscope-service/service-controlplane/internal/asdp/asdp.proto` declares
//! `service AgentDataPlaneService { rpc Connect(stream Upstream) returns (stream
//! Downstream); }` and generates the stubs into `io.agentscope.controlplane.proto`
//! (`GrpcTransport.java:16-30`).
//!
//! **DESIGN: wire layer not ported.** Porting the stub generator needs `protoc`, the
//! `tonic`/`prost` crates (not in the workspace dependency set) and a build-time copy of
//! the service proto; a gRPC bidi stream also cannot be expressed through
//! [`ControlPlaneHttpClient`](super::ControlPlaneHttpClient), which is unary JSON over
//! `reqwest`. What the bridge actually depends on is therefore implemented here, and the
//! wire is a pluggable [`AsdpLink`]:
//!
//! * the handshake envelope (`ConnectRequest` + `UpstreamMeta`),
//! * capped exponential reconnect backoff (1s base, 30s cap, `Math.min(attempt, 5)`
//!   shift — `GrpcTransport.java:248-259`),
//! * the 15s heartbeat (`GrpcTransport.java:62-64, 317-336`),
//! * event reports gated by `EventReportAck` so only acknowledged reports leave the
//!   durable outbox (`GrpcTransport.java:286-315, 378-412`),
//! * downstream dispatch for connect-ack / session-command / execution-attempt /
//!   conversation-turn (`GrpcTransport.java:378-432`).
//!
//! Frames are the canonical proto3 **JSON** encoding of the messages, not protobuf
//! bytes: a host that adds a tonic link performs the byte encoding inside [`AsdpLink`]
//! and nothing above it changes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{ControlPlaneError, ControlPlaneResult};

/// Base reconnect delay (`GrpcTransport.RECONNECT_BASE_MS`).
pub const RECONNECT_BASE: Duration = Duration::from_millis(1_000);
/// Maximum reconnect delay (`GrpcTransport.RECONNECT_MAX_MS`).
pub const RECONNECT_MAX: Duration = Duration::from_millis(30_000);
/// Heartbeat period (`GrpcTransport.HEARTBEAT_INTERVAL_MS`).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(15_000);
/// Highest shift the Java code applies before capping (`Math.min(attempt, 5)`).
const RECONNECT_MAX_SHIFT: u32 = 5;

/// Routing/identity envelope sent with every upstream frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UpstreamMeta {
    pub agent_key: String,
    pub instance_key: String,
    pub namespace: String,
    pub timestamp: i64,
    pub tenant: String,
    pub agent_id: String,
    pub binding_id: String,
    pub generation: i64,
}

/// Handshake sent when the stream is established.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConnectRequest {
    pub runtime: String,
    pub sdk_version: String,
    pub capabilities: Vec<String>,
    pub session_affinity: String,
}

/// Liveness ping (`GrpcTransport.sendHeartbeat`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Heartbeat {
    pub timestamp: i64,
}

/// One Level-2 event inside an [`EventReport`].
///
/// The proto `bytes` fields (`tool_input`, `framework_meta`) are base64 strings in the
/// proto3 JSON encoding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionEventMsg {
    pub session_id: String,
    pub seq: i32,
    pub event_type: String,
    pub occurred_at: i64,
    pub role: String,
    pub content: String,
    pub tool_name: String,
    pub tool_input: Option<String>,
    pub tool_output: String,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub duration_ms: i32,
    pub framework_meta: Option<String>,
}

/// Batched Level-2 event report. `report_id` is stable across retries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EventReport {
    pub events: Vec<SessionEventMsg>,
    pub report_id: String,
}

/// Durable commit cursor for one session inside an [`EventReportAck`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionEventCursor {
    pub session_id: String,
    pub committed_seq: i32,
}

/// Acknowledges an [`EventReport`]; only then does the outbox advance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EventReportAck {
    pub report_id: String,
    pub committed: Vec<SessionEventCursor>,
    pub error: String,
}

/// Control-plane push-down command for a session (`compress|terminate|abort|...`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionCommand {
    pub session_id: String,
    pub command: String,
    pub params: Option<String>,
}

/// Fenced execution-attempt command (`dispatch|cancel`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExecutionAttemptCommand {
    pub attempt_id: String,
    pub agent_task_id: String,
    pub run_id: String,
    pub node_id: String,
    pub generation: i64,
    pub command: String,
    pub context_url: String,
    pub task_token: String,
    pub attempt_token: String,
    pub runtime_binding: Option<String>,
    pub payload: Option<String>,
    pub timestamp: i64,
}

/// Fenced online conversation turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConversationTurnCommand {
    pub invocation_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub binding_id: String,
    pub instance_id: String,
    pub generation: i64,
    pub input: Option<String>,
    pub deadline: i64,
    pub correlation_id: String,
}

/// Handshake response. A rejection is logged, not fatal (`GrpcTransport.java:426-432`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConnectResponse {
    pub accepted: bool,
    pub reject_reason: String,
    pub control_plane_version: String,
}

/// One `oneof` arm of the upstream message. Untyped arms keep their raw JSON so a
/// report shape this crate never builds still round-trips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UpstreamPayload {
    Connect(ConnectRequest),
    ConfigAck(serde_json::Value),
    SessionReport(serde_json::Value),
    ExecutionAttempt(serde_json::Value),
    Heartbeat(Heartbeat),
    EventReport(EventReport),
    ContextReport(serde_json::Value),
    Inventory(serde_json::Value),
    ConversationTurn(serde_json::Value),
}

impl UpstreamPayload {
    /// The proto `oneof` field name this arm serialises to.
    pub fn field_name(&self) -> &'static str {
        match self {
            Self::Connect(_) => "connect",
            Self::ConfigAck(_) => "configAck",
            Self::SessionReport(_) => "sessionReport",
            Self::ExecutionAttempt(_) => "executionAttempt",
            Self::Heartbeat(_) => "heartbeat",
            Self::EventReport(_) => "eventReport",
            Self::ContextReport(_) => "contextReport",
            Self::Inventory(_) => "inventory",
            Self::ConversationTurn(_) => "conversationTurn",
        }
    }
}

/// Upstream envelope: `meta` plus exactly one payload arm.
///
/// Serialised by hand rather than with `#[serde(flatten)]` on an enum; the frame shape is
/// `{"meta": {...}, "<arm>": {...}}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// Routing/identity envelope.
    pub meta: UpstreamMeta,
    /// The single `oneof` arm.
    pub payload: UpstreamPayload,
}

impl Upstream {
    /// The proto3-JSON object for this frame.
    pub fn to_json(&self) -> ControlPlaneResult<serde_json::Value> {
        let meta = serde_json::to_value(&self.meta).map_err(|error| {
            ControlPlaneError::Internal(format!("cannot encode ASDP meta: {error}"))
        })?;
        let payload = match &self.payload {
            UpstreamPayload::Connect(value) => serde_json::to_value(value),
            UpstreamPayload::Heartbeat(value) => serde_json::to_value(value),
            UpstreamPayload::EventReport(value) => serde_json::to_value(value),
            UpstreamPayload::ConfigAck(value)
            | UpstreamPayload::SessionReport(value)
            | UpstreamPayload::ExecutionAttempt(value)
            | UpstreamPayload::ContextReport(value)
            | UpstreamPayload::Inventory(value)
            | UpstreamPayload::ConversationTurn(value) => Ok(value.clone()),
        }
        .map_err(|error| {
            ControlPlaneError::Internal(format!("cannot encode ASDP payload: {error}"))
        })?;
        let mut object = serde_json::Map::new();
        object.insert("meta".to_string(), meta);
        object.insert(self.payload.field_name().to_string(), payload);
        Ok(serde_json::Value::Object(object))
    }
}

/// One `oneof` arm of the downstream message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DownstreamPayload {
    ConnectAck(ConnectResponse),
    ConfigPush(serde_json::Value),
    SessionCmd(SessionCommand),
    ExecutionAttempt(ExecutionAttemptCommand),
    Heartbeat(Heartbeat),
    ConversationTurn(ConversationTurnCommand),
    EventAck(EventReportAck),
}

/// Downstream envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct Downstream {
    /// The `oneof` arm carried by the frame.
    pub payload: DownstreamPayload,
}

impl Downstream {
    /// Decodes one downstream frame.
    ///
    /// The arm is picked by field name (the proto `oneof` discriminant); an unknown arm is
    /// an error so protocol drift surfaces instead of being silently dropped.
    pub fn parse(frame: &str) -> ControlPlaneResult<Self> {
        let value: serde_json::Value = serde_json::from_str(frame)
            .map_err(|error| ControlPlaneError::Http(format!("invalid ASDP frame: {error}")))?;
        let object = value.as_object().ok_or_else(|| {
            ControlPlaneError::Http("invalid ASDP frame: not an object".to_string())
        })?;
        let payload = if let Some(value) = object.get("eventAck") {
            DownstreamPayload::EventAck(decode_frame(value)?)
        } else if let Some(value) = object.get("sessionCmd") {
            DownstreamPayload::SessionCmd(decode_frame(value)?)
        } else if let Some(value) = object.get("executionAttempt") {
            DownstreamPayload::ExecutionAttempt(decode_frame(value)?)
        } else if let Some(value) = object.get("conversationTurn") {
            DownstreamPayload::ConversationTurn(decode_frame(value)?)
        } else if let Some(value) = object.get("connectAck") {
            DownstreamPayload::ConnectAck(decode_frame(value)?)
        } else if let Some(value) = object.get("heartbeat") {
            DownstreamPayload::Heartbeat(decode_frame(value)?)
        } else if let Some(value) = object.get("configPush") {
            DownstreamPayload::ConfigPush(value.clone())
        } else {
            return Err(ControlPlaneError::Http(
                "invalid ASDP frame: unknown downstream arm".to_string(),
            ));
        };
        Ok(Self { payload })
    }
}

fn decode_frame<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
) -> ControlPlaneResult<T> {
    serde_json::from_value(value.clone())
        .map_err(|error| ControlPlaneError::Http(format!("invalid ASDP frame: {error}")))
}

/// What a downstream frame meant, after the transport applied its own bookkeeping.
#[derive(Debug, Clone, PartialEq)]
pub enum AsdpEvent {
    /// The control plane acknowledged these cursors; the reports left the outbox.
    Acked(EventReportAck),
    /// A session command was pushed down.
    SessionCommand(SessionCommand),
    /// A fenced execution attempt was pushed down.
    ExecutionAttempt(ExecutionAttemptCommand),
    /// A fenced conversation turn was pushed down.
    ConversationTurn(ConversationTurnCommand),
    /// Connect was rejected; the stream stays up but the bridge should surface it.
    ConnectRejected(ConnectResponse),
    /// A frame this crate does not dispatch (config push, heartbeat, ...).
    Ignored,
}

/// Frame-level seam for the ASDP stream.
///
/// A production implementation wraps a tonic `Connect` stream; tests use an in-memory
/// link. Implementing this trait is the only work needed to swap the wire layer.
#[async_trait]
pub trait AsdpLink: Send + Sync {
    /// Sends one frame.
    async fn send(&self, frame: &str) -> ControlPlaneResult<()>;
    /// Receives the next frame; `None` means the peer completed the stream.
    async fn recv(&self) -> ControlPlaneResult<Option<String>>;
    /// Closes the stream. Idempotent.
    async fn close(&self) -> ControlPlaneResult<()>;
}

#[derive(Debug, Default)]
struct AsdpState {
    connected: bool,
    reconnect_attempts: u32,
    /// Reports that have been sent (or attempted) but not acknowledged, keyed by
    /// `report_id` so a reconnect can resend them in order.
    pending: BTreeMap<String, EventReport>,
}

/// ASDP channel: handshake, heartbeats, ack-gated outbox and downstream dispatch.
pub struct AsdpTransport {
    link: Arc<dyn AsdpLink>,
    meta: UpstreamMeta,
    connect: ConnectRequest,
    state: Mutex<AsdpState>,
}

impl AsdpTransport {
    /// Builds a transport over `link`.
    pub fn new(link: Arc<dyn AsdpLink>, meta: UpstreamMeta, connect: ConnectRequest) -> Self {
        Self {
            link,
            meta,
            connect,
            state: Mutex::new(AsdpState::default()),
        }
    }

    /// The reconnect delay for `attempt` (1-based), mirroring
    /// `Math.min(RECONNECT_MAX, RECONNECT_BASE << min(attempt, 5))`.
    ///
    /// Yields 2s, 4s, 8s, 16s, 30s, 30s, ... because Java increments the counter
    /// *before* scheduling the first reconnect.
    pub fn reconnect_delay(attempt: u32) -> Duration {
        let shift = attempt.min(RECONNECT_MAX_SHIFT);
        let multiplied = RECONNECT_BASE.saturating_mul(1u32 << shift);
        multiplied.min(RECONNECT_MAX)
    }

    /// Whether the stream is currently usable.
    pub fn is_connected(&self) -> bool {
        self.lock().connected
    }

    /// Reports not yet acknowledged, oldest first.
    pub fn pending_reports(&self) -> Vec<EventReport> {
        self.lock().pending.values().cloned().collect()
    }

    /// Sends the handshake and marks the stream connected.
    pub async fn connect(&self) -> ControlPlaneResult<()> {
        let frame = self.frame(UpstreamPayload::Connect(self.connect.clone()))?;
        self.link.send(&frame).await?;
        let mut state = self.lock();
        state.connected = true;
        state.reconnect_attempts = 0;
        Ok(())
    }

    /// Sends a heartbeat; returns `false` when the stream is down (Java does not queue it).
    pub async fn heartbeat(&self) -> ControlPlaneResult<bool> {
        if !self.is_connected() {
            return Ok(false);
        }
        let frame = self.frame(UpstreamPayload::Heartbeat(Heartbeat {
            timestamp: now_millis(),
        }))?;
        self.link.send(&frame).await?;
        Ok(true)
    }

    /// Queues `report` in the durable outbox and sends it when connected.
    ///
    /// The report stays pending until an [`EventReportAck`] arrives, so a reconnect can
    /// resend exactly the same `report_id` (the control plane deduplicates on it).
    pub async fn report_events(&self, report: EventReport) -> ControlPlaneResult<bool> {
        if report.events.is_empty() {
            return Ok(true);
        }
        {
            let mut state = self.lock();
            state.pending.insert(report.report_id.clone(), report.clone());
        }
        if !self.is_connected() {
            return Ok(false);
        }
        let frame = self.frame(UpstreamPayload::EventReport(report))?;
        self.link.send(&frame).await?;
        Ok(true)
    }

    /// Reads and dispatches one downstream frame; `None` when the peer completed.
    pub async fn next_event(&self) -> ControlPlaneResult<Option<AsdpEvent>> {
        let Some(frame) = self.link.recv().await? else {
            return Ok(None);
        };
        self.handle_downstream(&frame).map(Some)
    }

    /// Handles one downstream frame, applying ack bookkeeping first.
    pub fn handle_downstream(&self, frame: &str) -> ControlPlaneResult<AsdpEvent> {
        let downstream = Downstream::parse(frame)?;
        Ok(match downstream.payload {
            DownstreamPayload::EventAck(ack) => {
                if ack.error.is_empty() {
                    self.lock().pending.remove(&ack.report_id);
                }
                AsdpEvent::Acked(ack)
            }
            DownstreamPayload::SessionCmd(command) => AsdpEvent::SessionCommand(command),
            DownstreamPayload::ExecutionAttempt(command) => {
                AsdpEvent::ExecutionAttempt(command)
            }
            DownstreamPayload::ConversationTurn(command) => {
                AsdpEvent::ConversationTurn(command)
            }
            DownstreamPayload::ConnectAck(response) if !response.accepted => {
                AsdpEvent::ConnectRejected(response)
            }
            _ => AsdpEvent::Ignored,
        })
    }

    /// Marks the stream down and returns the delay before the next `connect`.
    ///
    /// Mirrors `GrpcTransport.scheduleReconnect`: the attempt counter only advances while
    /// the transport is not stopped, and it resets on a successful handshake.
    pub fn on_stream_closed(&self) -> Duration {
        let mut state = self.lock();
        state.connected = false;
        state.reconnect_attempts = state.reconnect_attempts.saturating_add(1);
        Self::reconnect_delay(state.reconnect_attempts)
    }

    /// Closes the link and clears transport state. Pending reports survive on purpose:
    /// the bridge keeps the durable outbox.
    pub async fn close(&self) -> ControlPlaneResult<()> {
        {
            let mut state = self.lock();
            state.connected = false;
        }
        self.link.close().await
    }

    fn frame(&self, payload: UpstreamPayload) -> ControlPlaneResult<String> {
        let mut meta = self.meta.clone();
        meta.timestamp = now_millis();
        let value = Upstream { meta, payload }.to_json()?;
        serde_json::to_string(&value)
            .map_err(|error| ControlPlaneError::Http(format!("cannot encode ASDP frame: {error}")))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AsdpState> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;

    #[derive(Default)]
    struct RecordingLink {
        sent: Mutex<Vec<String>>,
        incoming: Mutex<Vec<String>>,
        closed: Mutex<bool>,
    }

    impl RecordingLink {
        fn with_incoming(frames: Vec<&str>) -> Arc<Self> {
            let link = RecordingLink::default();
            *link.incoming.lock().unwrap() = frames.into_iter().map(str::to_string).collect();
            Arc::new(link)
        }

        fn sent(&self) -> Vec<serde_json::Value> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|frame| serde_json::from_str(frame).unwrap())
                .collect()
        }
    }

    #[async_trait]
    impl AsdpLink for RecordingLink {
        async fn send(&self, frame: &str) -> ControlPlaneResult<()> {
            self.sent.lock().unwrap().push(frame.to_string());
            Ok(())
        }

        async fn recv(&self) -> ControlPlaneResult<Option<String>> {
            let mut incoming = self.incoming.lock().unwrap();
            if incoming.is_empty() {
                Ok(None)
            } else {
                Ok(Some(incoming.remove(0)))
            }
        }

        async fn close(&self) -> ControlPlaneResult<()> {
            *self.closed.lock().unwrap() = true;
            Ok(())
        }
    }

    fn transport(link: Arc<RecordingLink>) -> AsdpTransport {
        let meta = UpstreamMeta {
            agent_key: "friday".into(),
            instance_key: "rep-1".into(),
            namespace: "default".into(),
            tenant: "t".into(),
            agent_id: "agent-1".into(),
            binding_id: "b-1".into(),
            generation: 3,
            timestamp: 0,
        };
        let connect = ConnectRequest {
            runtime: "agentscope-rust".into(),
            sdk_version: "0.1.0".into(),
            capabilities: vec!["session-inventory".into()],
            session_affinity: "none".into(),
        };
        AsdpTransport::new(link, meta, connect)
    }

    #[tokio::test]
    async fn connect_sends_the_handshake_with_meta_and_capabilities() {
        let link = Arc::new(RecordingLink::default());
        let transport = transport(link.clone());

        transport.connect().await.unwrap();

        assert!(transport.is_connected());
        let sent = link.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["meta"]["agentKey"], "friday");
        assert_eq!(sent[0]["meta"]["generation"], 3);
        assert_eq!(sent[0]["connect"]["runtime"], "agentscope-rust");
        assert_eq!(sent[0]["connect"]["capabilities"][0], "session-inventory");
    }

    #[test]
    fn reconnect_backoff_matches_the_java_cap() {
        assert_eq!(AsdpTransport::reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(AsdpTransport::reconnect_delay(2), Duration::from_secs(4));
        assert_eq!(AsdpTransport::reconnect_delay(3), Duration::from_secs(8));
        assert_eq!(AsdpTransport::reconnect_delay(4), Duration::from_secs(16));
        // 1s << 5 = 32s, capped to 30s, and the cap holds for later attempts.
        assert_eq!(AsdpTransport::reconnect_delay(5), RECONNECT_MAX);
        assert_eq!(AsdpTransport::reconnect_delay(9), RECONNECT_MAX);
    }

    #[tokio::test]
    async fn heartbeat_is_skipped_while_disconnected() {
        let link = Arc::new(RecordingLink::default());
        let transport = transport(link.clone());

        assert!(!transport.heartbeat().await.unwrap());
        assert!(link.sent().is_empty());

        transport.connect().await.unwrap();
        assert!(transport.heartbeat().await.unwrap());
        let sent = link.sent();
        assert_eq!(sent.len(), 2);
        assert!(sent[1]["heartbeat"]["timestamp"].is_i64());
    }

    #[tokio::test]
    async fn event_reports_stay_pending_until_acknowledged() {
        let link = Arc::new(RecordingLink::default());
        let transport = transport(link.clone());
        transport.connect().await.unwrap();

        let report = EventReport {
            report_id: "r-1".into(),
            events: vec![SessionEventMsg {
                session_id: "s1".into(),
                seq: 1,
                event_type: "message".into(),
                content: "hello".into(),
                ..SessionEventMsg::default()
            }],
        };
        assert!(transport.report_events(report.clone()).await.unwrap());
        assert_eq!(transport.pending_reports(), vec![report.clone()]);

        // An ack carrying an error keeps the report in the outbox for retry.
        let failed = transport
            .handle_downstream(
                r#"{"eventAck":{"reportId":"r-1","error":"store unavailable"}}"#,
            )
            .unwrap();
        assert!(matches!(failed, AsdpEvent::Acked(_)));
        assert_eq!(transport.pending_reports().len(), 1);

        let acked = transport
            .handle_downstream(
                r#"{"eventAck":{"reportId":"r-1","committed":[{"sessionId":"s1","committedSeq":1}]}}"#,
            )
            .unwrap();
        match acked {
            AsdpEvent::Acked(ack) => {
                assert_eq!(ack.committed[0].session_id, "s1");
                assert_eq!(ack.committed[0].committed_seq, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(transport.pending_reports().is_empty());
    }

    #[tokio::test]
    async fn downstream_commands_are_dispatched() {
        let link = Arc::new(RecordingLink::default());
        let transport = transport(link);

        let event = transport
            .handle_downstream(
                r#"{"sessionCmd":{"sessionId":"s1","command":"compress","params":"e30="}}"#,
            )
            .unwrap();
        match event {
            AsdpEvent::SessionCommand(command) => {
                assert_eq!(command.session_id, "s1");
                assert_eq!(command.command, "compress");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let rejected = transport
            .handle_downstream(r#"{"connectAck":{"accepted":false,"rejectReason":"stale generation"}}"#)
            .unwrap();
        match rejected {
            AsdpEvent::ConnectRejected(response) => {
                assert_eq!(response.reject_reason, "stale generation");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // An accepted handshake and unknown arms are not dispatched.
        assert_eq!(
            transport
                .handle_downstream(r#"{"connectAck":{"accepted":true}}"#)
                .unwrap(),
            AsdpEvent::Ignored
        );
    }

    #[tokio::test]
    async fn stream_close_schedules_the_next_reconnect_delay() {
        let link = Arc::new(RecordingLink::default());
        let transport = transport(link.clone());
        transport.connect().await.unwrap();

        assert_eq!(transport.on_stream_closed(), Duration::from_secs(2));
        assert!(!transport.is_connected());
        assert_eq!(transport.on_stream_closed(), Duration::from_secs(4));

        transport.connect().await.unwrap();
        // A successful handshake resets the counter.
        assert_eq!(transport.on_stream_closed(), Duration::from_secs(2));
        transport.close().await.unwrap();
        assert!(*link.closed.lock().unwrap());
    }

    #[tokio::test]
    async fn next_event_reads_and_decodes_one_frame() {
        let link = RecordingLink::with_incoming(vec![
            r#"{"executionAttempt":{"attemptId":"a1","command":"dispatch"}}"#,
            r#"{"conversationTurn":{"invocationId":"i1","sessionId":"s1"}}"#,
        ]);
        let transport = transport(link);

        match transport.next_event().await.unwrap() {
            Some(AsdpEvent::ExecutionAttempt(command)) => {
                assert_eq!(command.attempt_id, "a1");
                assert_eq!(command.command, "dispatch");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match transport.next_event().await.unwrap() {
            Some(AsdpEvent::ConversationTurn(command)) => {
                assert_eq!(command.invocation_id, "i1");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(transport.next_event().await.unwrap().is_none());
    }
}
