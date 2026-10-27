//! Socket.IO (Engine.IO v4) transport for Studio.
//!
//! Java uses the Socket.IO client against the `/python` namespace. This module provides the
//! Rust equivalent on top of `tokio-tungstenite`:
//!
//! - the Engine.IO/Socket.IO frame logic is a pure, offline-testable state machine
//!   ([`SocketIoProtocol`] + the `parse_*` helpers),
//! - [`SocketIoTransport`] drives it over a real WebSocket, answers Engine.IO pings, joins the
//!   namespace with Studio's `runId` authentication and reconnects with capped backoff,
//! - decoded `forwardUserInput` events are resolved into a shared [`PendingInputRegistry`] so
//!   [`StudioWebSocketClient::wait_for_input`](super::websocket::StudioWebSocketClient::wait_for_input)
//!   unblocks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{StudioError, StudioResult};
use crate::studio::config::StudioConfig;
use crate::studio::websocket::{
    parse_user_input, PendingInputRegistry, StudioTransport, EVENT_FORWARD_USER_INPUT,
};

/// Socket.IO namespace Java connects to.
pub const DEFAULT_NAMESPACE: &str = "/python";

/// Engine.IO protocol version used by the Studio server.
pub const ENGINE_IO_VERSION: u32 = 4;

/// The Engine.IO WebSocket URL for a Studio base URL (`http`/`https` are rewritten to
/// `ws`/`wss`).
pub fn socket_io_url(base: &str) -> StudioResult<String> {
    let trimmed = base.trim_end_matches('/');
    let ws = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        return Err(StudioError::WebSocket(format!(
            "Studio URL must use http(s) or ws(s): {base}"
        )));
    };
    Ok(format!(
        "{ws}/socket.io/?EIO={ENGINE_IO_VERSION}&transport=websocket"
    ))
}

/// The Engine.IO handshake carried by the `0{...}` open packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineIoHandshake {
    /// Engine.IO session id.
    pub sid: String,
    /// How often the server sends a ping, in milliseconds.
    pub ping_interval_ms: u64,
    /// How long the server waits for a pong, in milliseconds.
    pub ping_timeout_ms: u64,
}

/// Parses the `0{...}` Engine.IO open packet.
pub fn parse_engine_io_open(frame: &str) -> StudioResult<EngineIoHandshake> {
    let payload = frame
        .strip_prefix('0')
        .ok_or_else(|| StudioError::WebSocket(format!("not an Engine.IO open packet: {frame}")))?;
    let json: Value = serde_json::from_str(payload)
        .map_err(|e| StudioError::WebSocket(format!("invalid Engine.IO handshake: {e}")))?;
    let sid = json
        .get("sid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if sid.is_empty() {
        return Err(StudioError::WebSocket(
            "Engine.IO handshake returned no sid".to_string(),
        ));
    }
    Ok(EngineIoHandshake {
        sid,
        ping_interval_ms: json
            .get("pingInterval")
            .and_then(Value::as_u64)
            .unwrap_or(25_000),
        ping_timeout_ms: json
            .get("pingTimeout")
            .and_then(Value::as_u64)
            .unwrap_or(20_000),
    })
}

/// Builds the Socket.IO connect packet joining `namespace` with an authentication payload.
pub fn namespace_connect_frame(namespace: &str, auth: &Value) -> String {
    let namespace = namespace.trim_end_matches('/');
    if namespace.is_empty() || namespace == "/" {
        format!("40{auth}")
    } else {
        format!("40{namespace},{auth}")
    }
}

/// A decoded Socket.IO event packet.
#[derive(Debug, Clone, PartialEq)]
pub struct SocketIoEvent {
    /// The namespace the event was delivered on, when the packet carries one.
    pub namespace: Option<String>,
    /// Event name.
    pub name: String,
    /// First event argument.
    pub data: Value,
}

/// Parses a Socket.IO event packet (`42[...]` or `42/python,[...]`).
pub fn parse_socket_io_event(frame: &str) -> Option<SocketIoEvent> {
    let body = frame.strip_prefix("42")?;
    let (namespace, payload) = match body.strip_prefix('/') {
        Some(rest) => {
            let (namespace, payload) = rest.split_once(',')?;
            (Some(format!("/{namespace}")), payload)
        }
        None => (None, body),
    };
    let parsed: Value = serde_json::from_str(payload).ok()?;
    let items = parsed.as_array()?;
    let name = items.first()?.as_str()?.to_string();
    Some(SocketIoEvent {
        namespace,
        name,
        data: items.get(1).cloned().unwrap_or(Value::Null),
    })
}

/// Whether `frame` is an Engine.IO ping, which must be answered with a pong.
pub fn is_ping(frame: &str) -> bool {
    frame == "2" || frame.starts_with("2/") || frame.starts_with("2,")
}

/// The Engine.IO pong frame.
pub fn pong_frame() -> &'static str {
    "3"
}

/// Exponential reconnect backoff (`attempt` is 1-based), capped at `max`.
pub fn reconnect_delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    let factor = 1u32
        .checked_shl(attempt.saturating_sub(1).min(16))
        .unwrap_or(u32::MAX);
    (base * factor).min(max)
}

/// What the protocol wants the transport to do with one inbound frame.
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolAction {
    /// Send this text frame.
    Send(String),
    /// The namespace connect was acknowledged.
    Connected,
    /// A `forwardUserInput` event, already split into its three payload slots.
    UserInput {
        /// The pending request id to resolve.
        request_id: String,
        /// Raw `blocksInput` argument.
        blocks_input: Value,
        /// Raw `structuredInput` argument.
        structured_input: Value,
    },
}

/// Engine.IO/Socket.IO frame state machine, shared by the reader task and the tests.
#[derive(Debug)]
pub struct SocketIoProtocol {
    namespace: String,
    auth: Value,
    sid: Option<String>,
    connected: bool,
}

impl SocketIoProtocol {
    /// Creates a protocol driver for `namespace`, authenticating with `auth`.
    pub fn new(namespace: impl Into<String>, auth: Value) -> Self {
        Self {
            namespace: namespace.into(),
            auth,
            sid: None,
            connected: false,
        }
    }

    /// The Engine.IO session id, once the handshake was seen.
    pub fn sid(&self) -> Option<&str> {
        self.sid.as_deref()
    }

    /// Whether the namespace connect was acknowledged.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Consumes one inbound text frame and returns the actions it triggers.
    pub fn on_text(&mut self, frame: &str) -> Vec<ProtocolAction> {
        if frame.starts_with('0') {
            return match parse_engine_io_open(frame) {
                Ok(handshake) => {
                    self.sid = Some(handshake.sid);
                    vec![ProtocolAction::Send(namespace_connect_frame(
                        &self.namespace,
                        &self.auth,
                    ))]
                }
                Err(err) => {
                    tracing::warn!(error = %err, "studio socket.io handshake rejected");
                    Vec::new()
                }
            };
        }
        if is_ping(frame) {
            return vec![ProtocolAction::Send(pong_frame().to_string())];
        }
        if frame.starts_with("40") {
            self.connected = true;
            return vec![ProtocolAction::Connected];
        }
        let Some(event) = parse_socket_io_event(frame) else {
            return Vec::new();
        };
        if event.name != EVENT_FORWARD_USER_INPUT {
            return Vec::new();
        }
        let Some(items) = event.data.as_array() else {
            return Vec::new();
        };
        let request_id = items
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if request_id.is_empty() {
            return Vec::new();
        }
        vec![ProtocolAction::UserInput {
            request_id,
            blocks_input: items.get(1).cloned().unwrap_or(Value::Null),
            structured_input: items.get(2).cloned().unwrap_or(Value::Null),
        }]
    }
}

/// An open Socket.IO connection: an outbound frame channel plus the reader task.
struct Connection {
    sender: mpsc::UnboundedSender<String>,
    task: JoinHandle<()>,
}

/// Real Socket.IO transport backed by `tokio-tungstenite`.
///
/// Reconnects on an unexpected close, with `StudioConfig`'s attempt/delay bounds. Decoded
/// `forwardUserInput` events resolve into the shared [`PendingInputRegistry`].
pub struct SocketIoTransport {
    url: String,
    namespace: String,
    auth: Value,
    registry: Arc<PendingInputRegistry>,
    reconnect_attempts: u32,
    reconnect_delay: Duration,
    reconnect_max_delay: Duration,
    connection: Mutex<Option<Connection>>,
    connected: Arc<AtomicBool>,
}

impl std::fmt::Debug for SocketIoTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketIoTransport")
            .field("url", &self.url)
            .field("namespace", &self.namespace)
            .field("connected", &self.connected.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl SocketIoTransport {
    /// Creates a transport for `config`, resolving `forwardUserInput` into `registry`.
    pub fn new(config: &StudioConfig, registry: Arc<PendingInputRegistry>) -> StudioResult<Self> {
        let auth = serde_json::json!({ "runId": config.run_id() });
        Ok(Self {
            url: socket_io_url(&config.socket_url())?,
            namespace: DEFAULT_NAMESPACE.to_string(),
            auth,
            registry,
            reconnect_attempts: config.reconnect_attempts(),
            reconnect_delay: config.reconnect_delay(),
            reconnect_max_delay: config.reconnect_max_delay(),
            connection: Mutex::new(None),
            connected: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Overrides the Socket.IO namespace (defaults to `/python`).
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// The WebSocket URL this transport connects to.
    pub fn url(&self) -> &str {
        &self.url
    }
}

#[async_trait]
impl StudioTransport for SocketIoTransport {
    async fn connect(&self) -> StudioResult<()> {
        if self
            .connection
            .lock()
            .map(|conn| conn.is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }
        let (sender, receiver) = mpsc::unbounded_channel::<String>();
        let url = self.url.clone();
        let namespace = self.namespace.clone();
        let auth = self.auth.clone();
        let registry = Arc::clone(&self.registry);
        let connected = Arc::clone(&self.connected);
        let attempts = self.reconnect_attempts;
        let base_delay = self.reconnect_delay;
        let max_delay = self.reconnect_max_delay;

        let task = tokio::spawn(async move {
            let mut receiver = receiver;
            let mut attempt = 0u32;
            loop {
                let opened = tokio_tungstenite::connect_async(&url).await;
                let (mut sink, mut source) = match opened {
                    Ok((stream, _)) => {
                        use futures::StreamExt;
                        attempt = 0;
                        connected.store(true, Ordering::SeqCst);
                        stream.split()
                    }
                    Err(err) => {
                        connected.store(false, Ordering::SeqCst);
                        if attempt >= attempts {
                            tracing::warn!(error = %err, "studio socket.io connect failed; giving up");
                            return;
                        }
                        attempt += 1;
                        tokio::time::sleep(reconnect_delay(attempt, base_delay, max_delay)).await;
                        continue;
                    }
                };
                use futures::{SinkExt, StreamExt};
                let mut protocol = SocketIoProtocol::new(namespace.clone(), auth.clone());
                let mut stop = false;
                loop {
                    tokio::select! {
                        outgoing = receiver.recv() => match outgoing {
                            Some(text) => {
                                if sink.send(Message::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                            // The transport was dropped/closed: stop for good.
                            None => {
                                stop = true;
                                break;
                            }
                        },
                        incoming = source.next() => match incoming {
                            Some(Ok(Message::Text(text))) => {
                                for action in protocol.on_text(text.to_string().as_str()) {
                                    match action {
                                        ProtocolAction::Send(frame) => {
                                            let _ = sink.send(Message::Text(frame.into())).await;
                                        }
                                        ProtocolAction::Connected => {
                                            tracing::debug!("studio socket.io namespace joined");
                                        }
                                        ProtocolAction::UserInput {
                                            request_id,
                                            blocks_input,
                                            structured_input,
                                        } => {
                                            let data = parse_user_input(
                                                &blocks_input,
                                                &structured_input,
                                            );
                                            registry.resolve(&request_id, data).await;
                                        }
                                    }
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Err(err)) => {
                                tracing::warn!(error = %err, "studio socket.io read error");
                                break;
                            }
                            Some(Ok(_)) => {}
                        },
                    }
                }
                connected.store(false, Ordering::SeqCst);
                if stop || attempt >= attempts {
                    if !stop {
                        tracing::warn!("studio socket.io reconnect attempts exhausted");
                    }
                    return;
                }
                attempt += 1;
                tokio::time::sleep(reconnect_delay(attempt, base_delay, max_delay)).await;
            }
        });

        let mut guard = self
            .connection
            .lock()
            .map_err(|_| StudioError::WebSocket("socket.io transport lock poisoned".to_string()))?;
        *guard = Some(Connection { sender, task });
        Ok(())
    }

    async fn close(&self) -> StudioResult<()> {
        let taken = self
            .connection
            .lock()
            .map_err(|_| StudioError::WebSocket("socket.io transport lock poisoned".to_string()))?
            .take();
        if let Some(connection) = taken {
            // Dropping the sender ends the reader loop; abort covers a blocked read.
            drop(connection.sender);
            connection.task.abort();
        }
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_url_rewrites_the_scheme_and_appends_the_engine_io_query() {
        assert_eq!(
            socket_io_url("http://studio:3000").unwrap(),
            "ws://studio:3000/socket.io/?EIO=4&transport=websocket"
        );
        assert_eq!(
            socket_io_url("https://studio.example.com/").unwrap(),
            "wss://studio.example.com/socket.io/?EIO=4&transport=websocket"
        );
        assert_eq!(
            socket_io_url("ws://studio:3000").unwrap(),
            "ws://studio:3000/socket.io/?EIO=4&transport=websocket"
        );
        assert!(socket_io_url("ftp://studio").is_err());
    }

    #[test]
    fn handshake_parses_sid_and_ping_settings() {
        let handshake =
            parse_engine_io_open("0{\"sid\":\"abc\",\"pingInterval\":1000,\"pingTimeout\":2000}")
                .unwrap();
        assert_eq!(handshake.sid, "abc");
        assert_eq!(handshake.ping_interval_ms, 1_000);
        assert_eq!(handshake.ping_timeout_ms, 2_000);

        // Absent ping settings fall back to the Engine.IO defaults.
        let defaults = parse_engine_io_open("0{\"sid\":\"abc\"}").unwrap();
        assert_eq!(defaults.ping_interval_ms, 25_000);
        assert_eq!(defaults.ping_timeout_ms, 20_000);
    }

    #[test]
    fn handshake_rejects_malformed_packets() {
        assert!(parse_engine_io_open("42[\"x\"]").is_err());
        assert!(parse_engine_io_open("0not-json").is_err());
        assert!(parse_engine_io_open("0{}")
            .unwrap_err()
            .to_string()
            .contains("no sid"));
    }

    #[test]
    fn connect_frame_carries_namespace_and_auth() {
        let auth = serde_json::json!({ "runId": "run-1" });
        assert_eq!(
            namespace_connect_frame("/python", &auth),
            "40/python,{\"runId\":\"run-1\"}"
        );
        assert_eq!(
            namespace_connect_frame("/", &auth),
            "40{\"runId\":\"run-1\"}"
        );
    }

    #[test]
    fn event_packets_parse_with_and_without_a_namespace() {
        let namespaced =
            parse_socket_io_event("42/python,[\"forwardUserInput\",[\"req-1\",[{\"type\":\"text\",\"text\":\"hi\"}],null]]")
                .unwrap();
        assert_eq!(namespaced.namespace.as_deref(), Some("/python"));
        assert_eq!(namespaced.name, "forwardUserInput");

        let direct = parse_socket_io_event("42[\"ping\",1]").unwrap();
        assert_eq!(direct.namespace, None);
        assert_eq!(direct.name, "ping");
        assert_eq!(direct.data, serde_json::json!(1));

        // Bare ints are not event packets.
        assert_eq!(parse_socket_io_event("2"), None);
        assert_eq!(parse_socket_io_event("42not-json"), None);
    }

    #[test]
    fn ping_detection_covers_the_namespaced_form() {
        assert!(is_ping("2"));
        assert!(is_ping("2/python"));
        assert!(is_ping("2/python,"));
        assert!(!is_ping("3"));
        assert!(!is_ping("42[\"2\"]"));
        assert_eq!(pong_frame(), "3");
    }

    #[test]
    fn reconnect_delay_grows_then_saturates_at_the_cap() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(5);
        assert_eq!(reconnect_delay(1, base, max), Duration::from_secs(1));
        assert_eq!(reconnect_delay(2, base, max), Duration::from_secs(2));
        assert_eq!(reconnect_delay(3, base, max), Duration::from_secs(4));
        assert_eq!(reconnect_delay(4, base, max), max);
        assert_eq!(reconnect_delay(99, base, max), max);
        // attempt 0 must not underflow.
        assert_eq!(reconnect_delay(0, base, max), base);
    }

    #[test]
    fn protocol_joins_the_namespace_after_the_handshake() {
        let mut protocol =
            SocketIoProtocol::new(DEFAULT_NAMESPACE, serde_json::json!({ "runId": "run-1" }));
        let actions = protocol.on_text("0{\"sid\":\"s-1\"}");
        assert_eq!(
            actions,
            vec![ProtocolAction::Send(
                "40/python,{\"runId\":\"run-1\"}".to_string()
            )]
        );
        assert_eq!(protocol.sid(), Some("s-1"));
        assert!(!protocol.is_connected());
        assert_eq!(
            protocol.on_text("40/python,{\"sid\":\"s-1\"}"),
            vec![ProtocolAction::Connected]
        );
        assert!(protocol.is_connected());
    }

    #[test]
    fn protocol_answers_pings_and_ignores_unrelated_frames() {
        let mut protocol = SocketIoProtocol::new(DEFAULT_NAMESPACE, Value::Null);
        assert_eq!(
            protocol.on_text("2"),
            vec![ProtocolAction::Send("3".to_string())]
        );
        assert!(protocol.on_text("2").len() == 1);
        assert!(protocol.on_text("43/python,7").is_empty());
        assert!(protocol.on_text("42/python,[\"otherEvent\",{}]").is_empty());
    }

    #[test]
    fn protocol_decodes_forward_user_input_events() {
        let mut protocol = SocketIoProtocol::new(DEFAULT_NAMESPACE, Value::Null);
        let frame = "42/python,[\"forwardUserInput\",[\"req-7\",[{\"type\":\"text\",\"text\":\"typed\"}],{\"field\":1}]]";
        let actions = protocol.on_text(frame);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ProtocolAction::UserInput {
                request_id,
                blocks_input,
                structured_input,
            } => {
                assert_eq!(request_id, "req-7");
                assert_eq!(blocks_input[0]["text"], serde_json::json!("typed"));
                assert_eq!(structured_input["field"], serde_json::json!(1));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn protocol_drops_user_input_without_a_request_id() {
        let mut protocol = SocketIoProtocol::new(DEFAULT_NAMESPACE, Value::Null);
        let frame = "42/python,[\"forwardUserInput\",[\"\",[],null]]";
        assert!(protocol.on_text(frame).is_empty());
        let not_an_array = "42/python,[\"forwardUserInput\",\"nope\"]";
        assert!(protocol.on_text(not_an_array).is_empty());
    }

    #[test]
    fn transport_builds_from_a_studio_config() {
        let config = StudioConfig::builder()
            .studio_url("http://studio:3000")
            .run_id("run-7")
            .build();
        let transport =
            SocketIoTransport::new(&config, Arc::new(PendingInputRegistry::new())).unwrap();
        assert_eq!(
            transport.url(),
            "ws://studio:3000/socket.io/?EIO=4&transport=websocket"
        );
        assert_eq!(transport.namespace, DEFAULT_NAMESPACE);
        assert!(!transport.is_connected());
        // Closing a never-connected transport is a no-op.
        assert!(futures::executor::block_on(transport.close()).is_ok());
    }
}
