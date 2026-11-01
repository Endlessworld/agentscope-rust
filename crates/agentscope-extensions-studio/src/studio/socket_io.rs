//! Socket.IO (Engine.IO v4) transport for Studio.
//!
//! Java uses the Socket.IO client against the `/python` namespace. This module provides the
//! Rust equivalent on top of `tokio-tungstenite`:
//!
//! - the Engine.IO/Socket.IO frame logic is a pure, offline-testable state machine
//!   ([`SocketIoProtocol`] + the `parse_*` helpers),
//! - [`SocketIoTransport`] drives it over a real WebSocket, answers Engine.IO pings, joins the
//!   namespace with Studio's `run_id` authentication (Java `StudioWebSocketClient.java:98`) and
//!   reconnects with capped backoff,
//! - decoded `forwardUserInput` events are resolved into a shared [`PendingInputRegistry`] so
//!   [`StudioWebSocketClient::wait_for_input`](super::websocket::StudioWebSocketClient::wait_for_input)
//!   unblocks.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{StudioError, StudioResult};
use crate::studio::config::StudioConfig;
use crate::studio::websocket::{
    parse_user_input, PendingInputRegistry, StudioTransport, EVENT_FORWARD_USER_INPUT,
};

/// Socket.IO namespace Java connects to.
pub const DEFAULT_NAMESPACE: &str = "/python";

/// The handshake auth key Java sends.
///
/// Java `StudioWebSocketClient.connect()` puts the run id under `run_id`
/// (`StudioWebSocketClient.java:98`: `auth.put("run_id", config.getRunId())`) — snake_case,
/// unlike the camelCase used by the Studio POJOs. The Studio server validates this key, so
/// sending `runId` made the namespace handshake look unauthenticated.
pub const AUTH_RUN_ID_KEY: &str = "run_id";

/// The key earlier Rust builds sent for the same value.
///
/// Only emitted when a host opts in through
/// [`SocketIoTransport::with_legacy_auth_key`]: Studio deployments adapted to the previous
/// payload keep working, while the default stays byte-identical to Java.
pub const LEGACY_AUTH_RUN_ID_KEY: &str = "runId";

/// Builds the namespace-connect auth payload (`{"run_id": <run id>}`).
///
/// With `include_legacy_key`, the pre-fix `runId` key is added alongside it; the Java key is
/// always present so a Java-compatible server never sees an unauthenticated handshake.
pub fn auth_payload(run_id: &str, include_legacy_key: bool) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        AUTH_RUN_ID_KEY.to_string(),
        Value::String(run_id.to_string()),
    );
    if include_legacy_key {
        payload.insert(
            LEGACY_AUTH_RUN_ID_KEY.to_string(),
            Value::String(run_id.to_string()),
        );
    }
    Value::Object(payload)
}

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
    handshake: Option<EngineIoHandshake>,
    connected: bool,
}

impl SocketIoProtocol {
    /// Creates a protocol driver for `namespace`, authenticating with `auth`.
    pub fn new(namespace: impl Into<String>, auth: Value) -> Self {
        Self {
            namespace: namespace.into(),
            auth,
            handshake: None,
            connected: false,
        }
    }

    /// The Engine.IO session id, once the handshake was seen.
    pub fn sid(&self) -> Option<&str> {
        self.handshake.as_ref().map(|handshake| handshake.sid.as_str())
    }

    /// The Engine.IO handshake, once the open packet was seen.
    pub fn handshake(&self) -> Option<&EngineIoHandshake> {
        self.handshake.as_ref()
    }

    /// How long the transport may stay silent before the connection counts as half-open.
    ///
    /// Engine.IO v4: the server pings every `pingInterval` and gives the client `pingTimeout`
    /// to answer, so a silent window longer than the sum means the socket is dead (STUDIO-05).
    pub fn heartbeat_window(&self) -> Option<Duration> {
        self.handshake.as_ref().map(|handshake| {
            Duration::from_millis(
                handshake
                    .ping_interval_ms
                    .saturating_add(handshake.ping_timeout_ms),
            )
        })
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
                    self.handshake = Some(handshake);
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

/// How long [`StudioTransport::connect`] waits for the namespace acknowledgement by default
/// (Java `StudioWebSocketClient#connect` applies `.timeout(Duration.ofSeconds(10))`).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Lifecycle of the transport's connection, observed by [`SocketIoTransport::wait_ready`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connecting, handshaking, or waiting for the next reconnect attempt.
    Connecting,
    /// The namespace connect was acknowledged (`40`); frames flow.
    Ready,
    /// The connection was abandoned for good (handshake failed or retries exhausted).
    Failed(String),
    /// [`StudioTransport::close`] was called.
    Closed,
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
    connect_timeout: Duration,
    state: Arc<watch::Sender<ConnectionState>>,
}

impl std::fmt::Debug for SocketIoTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketIoTransport")
            .field("url", &self.url)
            .field("namespace", &self.namespace)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl SocketIoTransport {
    /// Creates a transport for `config`, resolving `forwardUserInput` into `registry`.
    pub fn new(config: &StudioConfig, registry: Arc<PendingInputRegistry>) -> StudioResult<Self> {
        // Java `StudioWebSocketClient.java:98` sends `run_id`; see [`auth_payload`].
        let auth = auth_payload(config.run_id(), false);
        let (state, _) = watch::channel(ConnectionState::Connecting);
        Ok(Self {
            url: socket_io_url(&config.socket_url())?,
            namespace: DEFAULT_NAMESPACE.to_string(),
            auth,
            registry,
            reconnect_attempts: config.reconnect_attempts(),
            reconnect_delay: config.reconnect_delay(),
            reconnect_max_delay: config.reconnect_max_delay(),
            connection: Mutex::new(None),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            state: Arc::new(state),
        })
    }

    /// Adds the pre-fix `runId` auth key next to the Java `run_id` one.
    ///
    /// Only needed for a Studio server that was adapted to the previous Rust payload; the
    /// default payload matches Java exactly.
    pub fn with_legacy_auth_key(mut self) -> Self {
        if let Some(run_id) = self
            .auth
            .get(AUTH_RUN_ID_KEY)
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if let Some(map) = self.auth.as_object_mut() {
                map.insert(LEGACY_AUTH_RUN_ID_KEY.to_string(), Value::String(run_id));
            }
        }
        self
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

    /// The registry incoming `forwardUserInput` events are resolved into.
    ///
    /// The manager hands this same registry to the `StudioWebSocketClient` it builds, so
    /// `wait_for_input` observes the events decoded here (STUDIO-01).
    pub fn registry(&self) -> Arc<PendingInputRegistry> {
        Arc::clone(&self.registry)
    }

    /// Overrides how long [`StudioTransport::connect`] waits for the namespace acknowledgement
    /// (default 10s, the Java `EVENT_CONNECT` timeout).
    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// The current connection state.
    pub fn state(&self) -> ConnectionState {
        ConnectionState::clone(&self.state.borrow())
    }

    /// Waits until the namespace connect is acknowledged.
    ///
    /// Java's `StudioWebSocketClient#connect` signals on `EVENT_CONNECT`, errors on
    /// `EVENT_CONNECT_ERROR` and wraps the whole thing in a 10s timeout; this is that contract.
    /// A timeout or terminal failure tells the caller the socket is **not** usable, instead of
    /// the previous "spawn and pretend it worked" (STUDIO-03).
    pub async fn wait_ready(&self, timeout: Duration) -> StudioResult<()> {
        let mut state = self.state.subscribe();
        let wait = async {
            loop {
                match &*state.borrow_and_update() {
                    ConnectionState::Ready => return Ok(()),
                    ConnectionState::Failed(reason) => {
                        return Err(StudioError::WebSocket(format!(
                            "socket.io connection failed: {reason}"
                        )))
                    }
                    ConnectionState::Closed => {
                        return Err(StudioError::WebSocket(
                            "socket.io transport is closed".to_string(),
                        ))
                    }
                    ConnectionState::Connecting => {}
                }
                if state.changed().await.is_err() {
                    return Err(StudioError::WebSocket(
                        "socket.io transport was dropped".to_string(),
                    ));
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(StudioError::WebSocket(format!(
                "socket.io namespace connect did not complete within {timeout:?}"
            ))),
        }
    }
}

#[async_trait]
impl StudioTransport for SocketIoTransport {
    async fn connect(&self) -> StudioResult<()> {
        // STUDIO-03: Java's `connect()` completes on `EVENT_CONNECT` (the namespace ACK), not
        // when the WebSocket opens, and fails on `EVENT_CONNECT_ERROR` or after 10s.
        let already_running = self
            .connection
            .lock()
            .map(|conn| conn.is_some())
            .unwrap_or(false);
        if already_running {
            return self.wait_ready(self.connect_timeout).await;
        }
        self.state.send_replace(ConnectionState::Connecting);
        let (sender, receiver) = mpsc::unbounded_channel::<String>();
        let url = self.url.clone();
        let namespace = self.namespace.clone();
        let auth = self.auth.clone();
        let registry = Arc::clone(&self.registry);
        let state = Arc::clone(&self.state);
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
                        // The socket is open but the namespace is not joined yet: `is_connected`
                        // must stay false until the `40` ACK (STUDIO-03).
                        state.send_replace(ConnectionState::Connecting);
                        stream.split()
                    }
                    Err(err) => {
                        state.send_replace(ConnectionState::Connecting);
                        if attempt >= attempts {
                            tracing::warn!(error = %err, "studio socket.io connect failed; giving up");
                            state.send_replace(ConnectionState::Failed(err.to_string()));
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
                // STUDIO-05: Engine.IO v4 heartbeat deadline. Refreshed on every inbound frame;
                // a silent window longer than `pingInterval + pingTimeout` means half-open.
                let mut heartbeat: Option<tokio::time::Instant> = None;
                loop {
                    let incoming = async {
                        match heartbeat {
                            Some(deadline) => tokio::time::timeout_at(deadline, source.next())
                                .await
                                .map_err(|_| ()),
                            None => Ok(source.next().await),
                        }
                    };
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
                        incoming = incoming => match incoming {
                            // No frame within the heartbeat window: drop the half-open socket
                            // and let the backoff below reconnect.
                            Err(()) => {
                                tracing::warn!(
                                    "studio socket.io heartbeat timed out; reconnecting"
                                );
                                break;
                            }
                            Ok(Some(Ok(Message::Text(text)))) => {
                                for action in protocol.on_text(text.to_string().as_str()) {
                                    match action {
                                        ProtocolAction::Send(frame) => {
                                            let _ = sink.send(Message::Text(frame.into())).await;
                                        }
                                        ProtocolAction::Connected => {
                                            tracing::debug!("studio socket.io namespace joined");
                                            state.send_replace(ConnectionState::Ready);
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
                                if let Some(window) = protocol.heartbeat_window() {
                                    heartbeat = Some(tokio::time::Instant::now() + window);
                                }
                            }
                            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                            Ok(Some(Err(err))) => {
                                tracing::warn!(error = %err, "studio socket.io read error");
                                break;
                            }
                            Ok(Some(Ok(_))) => {}
                        },
                    }
                }
                state.send_replace(ConnectionState::Connecting);
                if stop || attempt >= attempts {
                    if stop {
                        state.send_replace(ConnectionState::Closed);
                    } else {
                        tracing::warn!("studio socket.io reconnect attempts exhausted");
                        state.send_replace(ConnectionState::Failed(
                            "reconnect attempts exhausted".to_string(),
                        ));
                    }
                    return;
                }
                attempt += 1;
                tokio::time::sleep(reconnect_delay(attempt, base_delay, max_delay)).await;
            }
        });

        {
            let mut guard = self.connection.lock().map_err(|_| {
                StudioError::WebSocket("socket.io transport lock poisoned".to_string())
            })?;
            *guard = Some(Connection { sender, task });
        }

        let ready = self.wait_ready(self.connect_timeout).await;
        if ready.is_err() {
            // Drop the abandoned attempt so a later `connect()` can start from scratch.
            let _ = self.close().await;
        }
        ready
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
        self.state.send_replace(ConnectionState::Closed);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        // STUDIO-03: "connected" means the namespace was joined (`40` ACK), not merely that a
        // TCP/WS socket is open.
        matches!(*self.state.borrow(), ConnectionState::Ready)
    }

    fn input_registry(&self) -> Option<Arc<PendingInputRegistry>> {
        Some(self.registry())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpListener;

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
        let auth = serde_json::json!({ "run_id": "run-1" });
        assert_eq!(
            namespace_connect_frame("/python", &auth),
            "40/python,{\"run_id\":\"run-1\"}"
        );
        assert_eq!(
            namespace_connect_frame("/", &auth),
            "40{\"run_id\":\"run-1\"}"
        );
    }

    #[test]
    fn handshake_auth_uses_the_java_snake_case_run_id_key() {
        // Java `StudioWebSocketClient.java:98`: `auth.put("run_id", config.getRunId())`
        // (snake_case, unlike the camelCase Studio POJO fields).
        let config = StudioConfig::builder()
            .studio_url("http://studio:3000")
            .run_id("run-7")
            .build();
        let transport =
            SocketIoTransport::new(&config, Arc::new(PendingInputRegistry::new())).unwrap();
        assert_eq!(transport.auth, serde_json::json!({ "run_id": "run-7" }));
        // ... and the namespace connect frame carries exactly that payload.
        let mut protocol = SocketIoProtocol::new(DEFAULT_NAMESPACE, transport.auth.clone());
        assert_eq!(
            protocol.on_text("0{\"sid\":\"s-1\"}"),
            vec![ProtocolAction::Send(
                "40/python,{\"run_id\":\"run-7\"}".to_string()
            )]
        );
    }

    #[test]
    fn the_legacy_camel_case_auth_key_is_opt_in() {
        let config = StudioConfig::builder()
            .studio_url("http://studio:3000")
            .run_id("run-7")
            .build();
        let transport = SocketIoTransport::new(&config, Arc::new(PendingInputRegistry::new()))
            .unwrap()
            .with_legacy_auth_key();
        let auth = transport.auth.as_object().expect("auth object");
        assert_eq!(auth.len(), 2);
        // The Java key is always present, the legacy one only on request.
        assert_eq!(auth.get(AUTH_RUN_ID_KEY), Some(&serde_json::json!("run-7")));
        assert_eq!(
            auth.get(LEGACY_AUTH_RUN_ID_KEY),
            Some(&serde_json::json!("run-7"))
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
            SocketIoProtocol::new(DEFAULT_NAMESPACE, serde_json::json!({ "run_id": "run-1" }));
        let actions = protocol.on_text("0{\"sid\":\"s-1\"}");
        assert_eq!(
            actions,
            vec![ProtocolAction::Send(
                "40/python,{\"run_id\":\"run-1\"}".to_string()
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

    fn transport_for(port: u16, attempts: u32) -> SocketIoTransport {
        let config = StudioConfig::builder()
            .studio_url(format!("http://127.0.0.1:{port}"))
            .run_id("run-socket-io")
            .reconnect_attempts(attempts)
            .reconnect_delay(Duration::from_millis(50))
            .reconnect_max_delay(Duration::from_millis(50))
            .build();
        SocketIoTransport::new(&config, Arc::new(PendingInputRegistry::new()))
            .expect("build the socket.io transport")
    }

    /// Plays Studio for one connection: Engine.IO open packet, wait for the namespace join,
    /// then acknowledge it. `acked` is set immediately before the ACK is written.
    async fn serve_one_connection(
        listener: TcpListener,
        open_packet: &str,
        acked: Arc<AtomicBool>,
    ) {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        ws.send(Message::Text(open_packet.to_string().into()))
            .await
            .expect("send the engine.io open packet");
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            if text.starts_with("40") {
                break;
            }
        }
        // A deliberate pause: `connect()` must not return before the ACK below.
        tokio::time::sleep(Duration::from_millis(150)).await;
        acked.store(true, Ordering::SeqCst);
        let _ = ws
            .send(Message::Text(
                "40/python,{\"sid\":\"s-1\"}".to_string().into(),
            ))
            .await;
        // Keep the socket open; the test aborts the task when it is done.
        tokio::time::sleep(Duration::from_secs(10)).await;
    }

    /// STUDIO-03: an unreachable Studio must make `connect()` fail instead of reporting success.
    #[tokio::test]
    async fn connect_fails_when_studio_is_unreachable() {
        // Bind then drop, so the port is (almost certainly) closed.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a probe socket");
        let port = listener.local_addr().expect("local address").port();
        drop(listener);

        let transport = transport_for(port, 0).with_connect_timeout(Duration::from_secs(2));
        let err = transport
            .connect()
            .await
            .expect_err("an unreachable Studio must not report success");

        assert!(matches!(err, StudioError::WebSocket(_)), "got {err:?}");
        assert!(!transport.is_connected());
    }

    /// STUDIO-03: `connect()` completes only on the namespace ACK (`EVENT_CONNECT`), not when
    /// the WebSocket opens.
    #[tokio::test]
    async fn connect_waits_for_the_namespace_ack() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local studio double");
        let port = listener.local_addr().expect("local address").port();
        let acked = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn(serve_one_connection(
            listener,
            "0{\"sid\":\"s-1\",\"pingInterval\":25000,\"pingTimeout\":20000}",
            Arc::clone(&acked),
        ));

        let transport = transport_for(port, 0).with_connect_timeout(Duration::from_secs(5));
        transport
            .connect()
            .await
            .expect("connects once the namespace is acknowledged");

        assert!(
            acked.load(Ordering::SeqCst),
            "connect() must not return before the namespace ACK"
        );
        assert!(transport.is_connected());
        assert_eq!(transport.state(), ConnectionState::Ready);

        transport.close().await.expect("closes");
        assert!(!transport.is_connected());
        server.abort();
    }

    /// STUDIO-05: a server that handshakes and then goes silent never pings again. Without a
    /// heartbeat watchdog the transport would look connected forever; it must drop the socket
    /// and reconnect.
    #[tokio::test]
    async fn a_silent_server_trips_the_heartbeat_watchdog_and_reconnects() {
        // A 100ms window (50 + 50) keeps the test quick; production uses 25s + 20s.
        const OPEN: &str = "0{\"sid\":\"s-1\",\"pingInterval\":50,\"pingTimeout\":50}";

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local studio double");
        let port = listener.local_addr().expect("local address").port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&accepts);
        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut second_tx = Some(second_tx);
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let count = accepted.fetch_add(1, Ordering::SeqCst) + 1;
                if count == 2 {
                    if let Some(tx) = second_tx.take() {
                        let _ = tx.send(());
                    }
                }
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    if ws
                        .send(Message::Text(OPEN.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        if text.starts_with("40") {
                            let _ = ws
                                .send(Message::Text(
                                    "40/python,{\"sid\":\"s-1\"}".to_string().into(),
                                ))
                                .await;
                            break;
                        }
                    }
                    // Never ping: the client's watchdog has to notice the silence.
                    tokio::time::sleep(Duration::from_secs(10)).await;
                });
            }
        });

        let transport = transport_for(port, 2);
        transport.connect().await.expect("the first socket is ready");
        tokio::time::timeout(Duration::from_secs(5), second_rx)
            .await
            .expect("the heartbeat timeout must trigger a reconnect")
            .expect("the double accepted a second connection");

        assert!(accepts.load(Ordering::SeqCst) >= 2);
        transport.close().await.expect("closes");
        server.abort();
    }
}
