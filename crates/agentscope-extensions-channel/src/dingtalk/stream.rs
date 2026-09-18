//! `DingTalkStreamClient` — the persistent WebSocket gateway client for the DingTalk Stream
//! protocol.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk.DingTalkStreamClient`.
//!
//! ## Protocol summary (identical to the Java client)
//!
//! 1. `POST {streamRegisterUrl}` with `clientId` / `clientSecret` / `ua` and the topic
//!    subscription list; the response carries `endpoint` (wss URL) and `ticket`.
//! 2. Connect a WebSocket to `{endpoint}?ticket={ticket}`.
//! 3. Each frame is a JSON envelope; `type=CALLBACK` carries a **stringified** `data` JSON. Every
//!    callback must be ACKed so DingTalk releases the message from its retry buffer. The ACK is
//!    sent *before* the payload is dispatched (the visible reply goes out separately through
//!    [`super::DingTalkOutboundClient`]).
//! 4. On failure, reconnect with exponential backoff (1 s -> 60 s, reset to 1 s once connected).
//!
//! ## Structure
//!
//! `handle_stream_payload` keeps this file thin on purpose: the envelope/ACK/payload logic lives in
//! the pure [`DingTalkStreamProtocol`] helpers, and the two I/O edges (`HttpTransport` for the
//! register call, [`WebSocketConnector`] for the socket) are traits, so the whole loop can be driven
//! offline in unit tests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tracing::{info, warn};

use super::properties::DingTalkChannelProperties;
use crate::common::{ChannelError, HttpTransport, PreparedRequest, Result};

/// Topic the Stream gateway pushes bot messages on (Java: `/v1.0/im/bot/messages/get`).
pub const BOT_MESSAGES_TOPIC: &str = "/v1.0/im/bot/messages/get";
/// User agent reported to the gateway on `openConnection`.
pub const STREAM_UA: &str = "agentscope-builder/1.0";
/// First reconnect delay (Java `backoffMs = 1_000L`).
pub const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Reconnect ceiling (Java `Math.min(backoffMs * 2L, 60_000L)`).
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// The `endpoint` / `ticket` pair returned by the gateway `open` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEndpoint {
    pub url: String,
    pub ticket: String,
}

/// Pure Stream envelope/ACK/payload logic — no I/O, so every branch is unit-testable.
pub struct DingTalkStreamProtocol;

impl DingTalkStreamProtocol {
    /// The `POST {streamRegisterUrl}` body (Java `openConnection`).
    pub fn register_body(properties: &DingTalkChannelProperties) -> Value {
        json!({
            "clientId": properties.app_key,
            "clientSecret": properties.app_secret,
            "ua": STREAM_UA,
            "subscriptions": [{"type": "CALLBACK", "topic": BOT_MESSAGES_TOPIC}],
        })
    }

    /// Parse the `open` response into an endpoint, mirroring the Java
    /// `IllegalStateException` on a missing/blank `endpoint` or `ticket`.
    pub fn parse_endpoint(body: &str) -> Result<StreamEndpoint> {
        let node: Value = serde_json::from_str(body)
            .map_err(|e| ChannelError::Http(format!("DingTalk gateway open returned bad JSON: {e}")))?;
        let text = |key: &str| -> Option<String> {
            node.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        match (text("endpoint"), text("ticket")) {
            (Some(url), Some(ticket)) => Ok(StreamEndpoint { url, ticket }),
            _ => Err(ChannelError::Http(format!(
                "DingTalk gateway open missing endpoint/ticket: {body}"
            ))),
        }
    }

    /// Join the wss endpoint and the ticket, matching the Java concatenation exactly
    /// (`?` becomes `&` when the endpoint already carries a query string).
    pub fn websocket_url(endpoint: &StreamEndpoint) -> String {
        let sep = if endpoint.url.contains('?') { '&' } else { '?' };
        format!("{}{sep}ticket={}", endpoint.url, endpoint.ticket)
    }

    /// `true` when the envelope is a `CALLBACK` frame (case-insensitive, Java
    /// `"CALLBACK".equalsIgnoreCase(type)`). `SYSTEM` / `EVENT` frames are ignored in the MVP.
    pub fn is_callback(envelope: &Value) -> bool {
        envelope
            .get("type")
            .and_then(Value::as_str)
            .map(|t| t.eq_ignore_ascii_case("CALLBACK"))
            .unwrap_or(false)
    }

    /// Parse a frame into an envelope; a malformed frame yields `None` (Java logs and drops it).
    pub fn parse_envelope(frame: &str) -> Option<Value> {
        serde_json::from_str::<Value>(frame).ok()
    }

    /// The ACK frame for a callback envelope. `messageId` is echoed only when the envelope
    /// carries one; the rest of the shape is fixed by the Java `ackFrame`.
    pub fn ack_body(envelope: &Value) -> Value {
        let mut headers = serde_json::Map::new();
        if let Some(message_id) = envelope
            .get("headers")
            .and_then(|h| h.get("messageId"))
            .and_then(Value::as_str)
        {
            headers.insert("messageId".to_string(), json!(message_id));
        }
        headers.insert("contentType".to_string(), json!("application/json"));
        json!({
            "code": 200,
            "headers": Value::Object(headers),
            "message": "OK",
            "data": "{}",
        })
    }

    /// The bot-message payload carried by a callback envelope: `data` is a *stringified* JSON
    /// document. Returns `None` when `data` is absent, not a string, or unparseable — the Java
    /// client acks first and then drops such frames.
    pub fn callback_payload(envelope: &Value) -> Option<Value> {
        let raw = envelope.get("data").and_then(Value::as_str)?;
        serde_json::from_str(raw).ok()
    }

    /// Next backoff delay: double, capped at `max` (Java `scheduleReconnect`).
    pub fn next_backoff(current: Duration, max: Duration) -> Duration {
        std::cmp::min(current * 2, max)
    }
}

/// A connected WebSocket, abstracted so the stream loop can run offline against a scripted socket.
#[async_trait]
#[allow(clippy::wrong_self_convention)]
pub trait StreamSocket: Send {
    /// Send one text frame.
    async fn send_text(&mut self, text: String) -> Result<()>;
    /// Receive the next text frame; `None` means the socket closed.
    async fn recv_text(&mut self) -> Option<Result<String>>;
    /// Best-effort close, mirroring the Java `sendClose` + `abort`.
    async fn close(&mut self);
}

/// Opens WebSocket connections. The production impl is [`TungsteniteConnector`].
#[async_trait]
pub trait WebSocketConnector: Send + Sync {
    async fn connect(&self, url: &str) -> Result<Box<dyn StreamSocket>>;
}

/// Where dispatched bot-message payloads go. [`ChannelPayloadSink`] forwards them to a
/// [`super::DingTalkChannel`].
#[async_trait]
pub trait StreamPayloadSink: Send + Sync {
    async fn on_payload(&self, payload: Value);
}

/// [`StreamPayloadSink`] driving a [`super::DingTalkChannel`]'s inbound pipeline.
pub struct ChannelPayloadSink {
    channel: Arc<super::DingTalkChannel>,
}

impl ChannelPayloadSink {
    pub fn new(channel: Arc<super::DingTalkChannel>) -> Self {
        Self { channel }
    }
}

#[async_trait]
impl StreamPayloadSink for ChannelPayloadSink {
    async fn on_payload(&self, payload: Value) {
        let _ = self.channel.handle_stream_payload(&payload).await;
    }
}

/// Persistent DingTalk Stream client.
///
/// [`start`](Self::start) spawns the connect/reconnect loop; [`stop`](Self::stop) shuts it down.
/// Calling `start` twice is a no-op, matching the Java `AtomicBoolean` guard.
pub struct DingTalkStreamClient {
    properties: DingTalkChannelProperties,
    transport: Arc<dyn HttpTransport>,
    connector: Arc<dyn WebSocketConnector>,
    sink: Arc<dyn StreamPayloadSink>,
    initial_backoff: Duration,
    max_backoff: Duration,
    running: AtomicBool,
    shutdown: Notify,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DingTalkStreamClient {
    pub fn new(
        properties: DingTalkChannelProperties,
        transport: Arc<dyn HttpTransport>,
        connector: Arc<dyn WebSocketConnector>,
        sink: Arc<dyn StreamPayloadSink>,
    ) -> Self {
        Self {
            properties,
            transport,
            connector,
            sink,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            running: AtomicBool::new(false),
            shutdown: Notify::new(),
            task: std::sync::Mutex::new(None),
        }
    }

    /// Build a client that feeds the given channel's inbound pipeline.
    pub fn for_channel(
        properties: DingTalkChannelProperties,
        channel: Arc<super::DingTalkChannel>,
        transport: Arc<dyn HttpTransport>,
        connector: Arc<dyn WebSocketConnector>,
    ) -> Self {
        Self::new(
            properties,
            transport,
            connector,
            Arc::new(ChannelPayloadSink::new(channel)),
        )
    }

    /// Override the backoff bounds (used by tests to avoid real delays).
    pub fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max;
        self
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Start the connect/reconnect loop. Safe to call once; later calls are ignored.
    pub fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move { this.run_loop().await });
        *self.task.lock().expect("stream task slot poisoned") = Some(handle);
    }

    /// Stop the loop and release the socket. Idempotent.
    pub async fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        self.shutdown.notify_waiters();
        let handle = self.task.lock().expect("stream task slot poisoned").take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// The `POST {streamRegisterUrl}` open call.
    pub async fn open_connection(&self) -> Result<StreamEndpoint> {
        let body = DingTalkStreamProtocol::register_body(&self.properties);
        let request = PreparedRequest::post_json(self.properties.stream_register_url.clone(), body);
        let response = self.transport.execute(request).await?;
        if !response.is_success() {
            return Err(ChannelError::Http(format!(
                "DingTalk gateway open returned HTTP {}: {}",
                response.status, response.body
            )));
        }
        DingTalkStreamProtocol::parse_endpoint(&response.body)
    }

    /// Connect, then serve frames until the socket closes or a shutdown is requested.
    ///
    /// Returns `true` when the WebSocket connected, so the caller can reset the backoff
    /// (Java resets `backoffMs` on successful connect).
    async fn run_session(&self) -> Result<bool> {
        let endpoint = self.open_connection().await?;
        let url = DingTalkStreamProtocol::websocket_url(&endpoint);
        let mut socket = self.connector.connect(&url).await?;
        info!(url = %url, "DingTalk Stream connected");

        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.notified() => break,
                frame = socket.recv_text() => match frame {
                    Some(Ok(text)) => self.handle_frame(&mut *socket, &text).await,
                    Some(Err(e)) => {
                        warn!(error = %e, "DingTalk Stream: receive failed");
                        break;
                    }
                    None => {
                        info!("DingTalk Stream: closed");
                        break;
                    }
                },
            }
        }
        socket.close().await;
        Ok(true)
    }

    /// Connect/reconnect loop with exponential backoff.
    async fn run_loop(&self) {
        let mut backoff = self.initial_backoff;
        while self.running.load(Ordering::SeqCst) {
            match self.run_session().await {
                Ok(true) => backoff = self.initial_backoff,
                Ok(false) => {}
                Err(e) => warn!(error = %e, "DingTalk Stream open failed"),
            }
            if !self.running.load(Ordering::SeqCst) {
                break;
            }
            let delay = backoff;
            backoff = DingTalkStreamProtocol::next_backoff(backoff, self.max_backoff);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.shutdown.notified() => break,
            }
        }
    }

    /// One inbound text frame: parse, ACK (before dispatch), then dispatch. Malformed frames and
    /// non-`CALLBACK` types are dropped, mirroring the Java `handleFrame`.
    async fn handle_frame(&self, socket: &mut dyn StreamSocket, text: &str) {
        let Some(envelope) = DingTalkStreamProtocol::parse_envelope(text) else {
            warn!("DingTalk Stream: malformed frame");
            return;
        };
        if !DingTalkStreamProtocol::is_callback(&envelope) {
            return;
        }
        // ACK before dispatching so the gateway releases the message from its retry buffer.
        let ack = DingTalkStreamProtocol::ack_body(&envelope);
        if let Err(e) = socket.send_text(ack.to_string()).await {
            warn!(error = %e, "DingTalk Stream: failed to send ack");
        }
        if let Some(payload) = DingTalkStreamProtocol::callback_payload(&envelope) {
            self.sink.on_payload(payload).await;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Production WebSocket connector (tokio-tungstenite).
// ---------------------------------------------------------------------------------------------

/// Production [`WebSocketConnector`] backed by `tokio-tungstenite` (the Rust analogue of the JDK
/// `java.net.http.WebSocket`). TLS uses the rustls webpki root store.
pub struct TungsteniteConnector;

#[async_trait]
impl WebSocketConnector for TungsteniteConnector {
    async fn connect(&self, url: &str) -> Result<Box<dyn StreamSocket>> {
        let (stream, _response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| ChannelError::Http(format!("DingTalk Stream connect failed: {e}")))?;
        Ok(Box::new(TungsteniteSocket { stream }))
    }
}

struct TungsteniteSocket {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[async_trait]
impl StreamSocket for TungsteniteSocket {
    async fn send_text(&mut self, text: String) -> Result<()> {
        use futures::SinkExt;
        self.stream
            .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
            .await
            .map_err(|e| ChannelError::Http(format!("DingTalk Stream send failed: {e}")))
    }

    async fn recv_text(&mut self) -> Option<Result<String>> {
        use futures::StreamExt;
        loop {
            match self.stream.next().await {
                None => return None,
                Some(Err(e)) => return Some(Err(ChannelError::Http(e.to_string()))),
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                    return Some(Ok(t.to_string()))
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return None,
                // Ping/Pong are answered by tungstenite itself; binary frames are not part of
                // the Stream protocol, so keep reading.
                Some(Ok(_)) => continue,
            }
        }
    }

    async fn close(&mut self) {
        let _ = self.stream.close(None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;

    fn props() -> DingTalkChannelProperties {
        DingTalkChannelProperties::new(
            Some("ak".into()),
            Some("as".into()),
            Some("rc".into()),
            None,
            None,
            Some("https://open.example/stream".into()),
        )
        .unwrap()
    }

    const OPEN_OK: &str = r#"{"endpoint":"wss://x/ws","ticket":"t-1"}"#;

    /// Shared record of what a scripted session sent and what it dispatched.
    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<String>>,
        payloads: Mutex<Vec<Value>>,
        /// Frames already sent at the moment each payload was dispatched, so a test can prove the
        /// ACK went out *before* dispatch.
        sent_at_dispatch: Mutex<Vec<usize>>,
    }

    struct ScriptedSocket {
        frames: VecDeque<String>,
        recorder: Arc<Recorder>,
    }

    #[async_trait]
    impl StreamSocket for ScriptedSocket {
        async fn send_text(&mut self, text: String) -> Result<()> {
            self.recorder.sent.lock().unwrap().push(text);
            Ok(())
        }

        async fn recv_text(&mut self) -> Option<Result<String>> {
            self.frames.pop_front().map(Ok)
        }

        async fn close(&mut self) {}
    }

    /// Connector handing out one scripted socket per call; once the script is exhausted it keeps
    /// returning an immediately-closed socket so the reconnect loop stays observable.
    struct ScriptedConnector {
        pending: Mutex<Vec<VecDeque<String>>>,
        recorder: Arc<Recorder>,
        connect_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WebSocketConnector for ScriptedConnector {
        async fn connect(&self, _url: &str) -> Result<Box<dyn StreamSocket>> {
            self.connect_count.fetch_add(1, Ordering::SeqCst);
            let frames = self.pending.lock().unwrap().pop().unwrap_or_default();
            Ok(Box::new(ScriptedSocket {
                frames,
                recorder: Arc::clone(&self.recorder),
            }))
        }
    }

    struct RecordingSink {
        recorder: Arc<Recorder>,
    }

    #[async_trait]
    impl StreamPayloadSink for RecordingSink {
        async fn on_payload(&self, payload: Value) {
            let sent = self.recorder.sent.lock().unwrap().len();
            self.recorder.sent_at_dispatch.lock().unwrap().push(sent);
            self.recorder.payloads.lock().unwrap().push(payload);
        }
    }

    fn callback_frame(message_id: &str, payload: &str) -> String {
        json!({
            "type": "CALLBACK",
            "headers": {"messageId": message_id},
            "data": payload,
        })
        .to_string()
    }

    /// Client with millisecond backoff so the reconnect loop is testable without real delays.
    fn client(
        transport: Arc<dyn HttpTransport>,
        connector: Arc<dyn WebSocketConnector>,
        sink: Arc<dyn StreamPayloadSink>,
    ) -> Arc<DingTalkStreamClient> {
        Arc::new(
            DingTalkStreamClient::new(props(), transport, connector, sink)
                .with_backoff(Duration::from_millis(1), Duration::from_millis(5)),
        )
    }

    #[test]
    fn register_body_matches_the_java_open_payload() {
        let body = DingTalkStreamProtocol::register_body(&props());
        assert_eq!(body["clientId"], "ak");
        assert_eq!(body["clientSecret"], "as");
        assert_eq!(body["ua"], STREAM_UA);
        assert_eq!(body["subscriptions"][0]["type"], "CALLBACK");
        assert_eq!(body["subscriptions"][0]["topic"], BOT_MESSAGES_TOPIC);
    }

    #[test]
    fn parse_endpoint_requires_both_fields() {
        let ok = DingTalkStreamProtocol::parse_endpoint(
            r#"{"endpoint":"wss://x/ws","ticket":"t-1"}"#,
        )
        .unwrap();
        assert_eq!(ok.url, "wss://x/ws");
        assert_eq!(ok.ticket, "t-1");

        for bad in [
            r#"{"ticket":"t-1"}"#,
            r#"{"endpoint":"wss://x/ws"}"#,
            r#"{"endpoint":"  ","ticket":"t-1"}"#,
            "not json",
        ] {
            assert!(DingTalkStreamProtocol::parse_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn websocket_url_appends_ticket_with_the_right_separator() {
        let plain = StreamEndpoint { url: "wss://x/ws".into(), ticket: "t-1".into() };
        assert_eq!(DingTalkStreamProtocol::websocket_url(&plain), "wss://x/ws?ticket=t-1");

        let with_query = StreamEndpoint {
            url: "wss://x/ws?k=v".into(),
            ticket: "t-1".into(),
        };
        assert_eq!(
            DingTalkStreamProtocol::websocket_url(&with_query),
            "wss://x/ws?k=v&ticket=t-1"
        );
    }

    #[test]
    fn callback_detection_is_case_insensitive_and_ignores_other_types() {
        assert!(DingTalkStreamProtocol::is_callback(&json!({"type": "CALLBACK"})));
        assert!(DingTalkStreamProtocol::is_callback(&json!({"type": "callback"})));
        assert!(!DingTalkStreamProtocol::is_callback(&json!({"type": "SYSTEM"})));
        assert!(!DingTalkStreamProtocol::is_callback(&json!({})));
    }

    #[test]
    fn ack_body_echoes_message_id_only_when_present() {
        let with_id = DingTalkStreamProtocol::ack_body(&json!({
            "type": "CALLBACK",
            "headers": {"messageId": "m-9"}
        }));
        assert_eq!(with_id["code"], 200);
        assert_eq!(with_id["headers"]["messageId"], "m-9");
        assert_eq!(with_id["headers"]["contentType"], "application/json");
        assert_eq!(with_id["message"], "OK");
        assert_eq!(with_id["data"], "{}");

        let without_id = DingTalkStreamProtocol::ack_body(&json!({"type": "CALLBACK"}));
        assert!(without_id["headers"].get("messageId").is_none());
        assert_eq!(without_id["headers"]["contentType"], "application/json");
    }

    #[test]
    fn callback_payload_unwraps_the_stringified_data() {
        let envelope = json!({
            "type": "CALLBACK",
            "data": "{\"msgtype\":\"text\",\"text\":{\"content\":\"hi\"}}"
        });
        let payload = DingTalkStreamProtocol::callback_payload(&envelope).unwrap();
        assert_eq!(payload["msgtype"], "text");

        assert!(DingTalkStreamProtocol::callback_payload(&json!({"type": "CALLBACK"})).is_none());
        assert!(DingTalkStreamProtocol::callback_payload(&json!({"data": 42})).is_none());
        assert!(
            DingTalkStreamProtocol::callback_payload(&json!({"data": "{oops"})).is_none()
        );
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let max = Duration::from_secs(60);
        let mut b = DEFAULT_INITIAL_BACKOFF;
        b = DingTalkStreamProtocol::next_backoff(b, max);
        assert_eq!(b, Duration::from_secs(2));
        assert_eq!(DingTalkStreamProtocol::next_backoff(b, max), Duration::from_secs(4));
        assert_eq!(
            DingTalkStreamProtocol::next_backoff(Duration::from_secs(60), max),
            max
        );
        assert_eq!(
            DingTalkStreamProtocol::next_backoff(Duration::from_secs(90), max),
            max
        );
    }

    #[test]
    fn malformed_frames_are_dropped_without_panicking() {
        assert!(DingTalkStreamProtocol::parse_envelope("{not json").is_none());
        assert!(DingTalkStreamProtocol::parse_envelope("[1,2,3]").is_some());
    }

    #[tokio::test]
    async fn open_connection_posts_the_register_body_and_reads_endpoint() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, OPEN_OK),
        ]));
        let recorder = Arc::new(Recorder::default());
        let client = client(
            transport.clone(),
            Arc::new(ScriptedConnector {
                pending: Mutex::new(Vec::new()),
                recorder: Arc::clone(&recorder),
                connect_count: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(RecordingSink { recorder }),
        );

        let endpoint = client.open_connection().await.unwrap();
        assert_eq!(endpoint.url, "wss://x/ws");
        assert_eq!(endpoint.ticket, "t-1");

        let request = transport.last_request().expect("register request");
        assert_eq!(request.url, "https://open.example/stream");
        assert_eq!(request.method, crate::common::HttpMethod::Post);
        assert_eq!(request.body.as_ref().unwrap()["clientId"], "ak");
    }

    #[tokio::test]
    async fn open_connection_fails_on_non_2xx_and_on_missing_ticket() {
        for (status, body) in [
            (401u16, OPEN_OK.to_string()),
            (200, r#"{"endpoint":"wss://x/ws"}"#.to_string()),
        ] {
            let transport = Arc::new(RecordingTransport::with_responses(vec![
                PreparedResponse::new(status, body.clone()),
            ]));
            let recorder = Arc::new(Recorder::default());
            let client = client(
                transport,
                Arc::new(ScriptedConnector {
                    pending: Mutex::new(Vec::new()),
                    recorder: Arc::clone(&recorder),
                    connect_count: Arc::new(AtomicUsize::new(0)),
                }),
                Arc::new(RecordingSink { recorder }),
            );
            assert!(client.open_connection().await.is_err(), "{status} {body}");
        }
    }

    /// The ACK must reach the gateway before the payload is dispatched (Java sends it first so the
    /// message is released from the retry buffer).
    #[tokio::test]
    async fn callback_is_acked_before_the_payload_is_dispatched() {
        let recorder = Arc::new(Recorder::default());
        let mut socket = ScriptedSocket {
            frames: VecDeque::new(),
            recorder: Arc::clone(&recorder),
        };
        let client = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(ScriptedConnector {
                pending: Mutex::new(Vec::new()),
                recorder: Arc::clone(&recorder),
                connect_count: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(RecordingSink {
                recorder: Arc::clone(&recorder),
            }),
        );

        let frame = callback_frame("m-9", r#"{"msgtype":"text","text":{"content":"hi"}}"#);
        client.handle_frame(&mut socket, &frame).await;

        let sent = recorder.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "exactly one ACK frame");
        let ack: Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(ack["code"], 200);
        assert_eq!(ack["headers"]["messageId"], "m-9");
        drop(sent);

        assert_eq!(*recorder.sent_at_dispatch.lock().unwrap(), vec![1]);
        let payloads = recorder.payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["text"]["content"], "hi");
    }

    #[tokio::test]
    async fn non_callback_and_malformed_frames_send_no_ack_and_dispatch_nothing() {
        let recorder = Arc::new(Recorder::default());
        let mut socket = ScriptedSocket {
            frames: VecDeque::new(),
            recorder: Arc::clone(&recorder),
        };
        let client = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(ScriptedConnector {
                pending: Mutex::new(Vec::new()),
                recorder: Arc::clone(&recorder),
                connect_count: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(RecordingSink {
                recorder: Arc::clone(&recorder),
            }),
        );

        client.handle_frame(&mut socket, "{not json").await;
        client
            .handle_frame(&mut socket, &json!({"type": "SYSTEM"}).to_string())
            .await;
        // A callback whose `data` is not stringified JSON is acked but not dispatched.
        client
            .handle_frame(
                &mut socket,
                &json!({"type": "CALLBACK", "data": 42}).to_string(),
            )
            .await;

        assert_eq!(recorder.sent.lock().unwrap().len(), 1, "only the last is acked");
        assert!(recorder.payloads.lock().unwrap().is_empty());
    }

    /// Drives the real spawn/reconnect loop offline: the socket yields one callback then closes, so
    /// the client must dispatch it and then reconnect.
    #[tokio::test]
    async fn start_serves_frames_and_reconnects_after_close() {
        let recorder = Arc::new(Recorder::default());
        let connect_count = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, OPEN_OK),
            PreparedResponse::new(200, OPEN_OK),
            PreparedResponse::new(200, OPEN_OK),
            PreparedResponse::new(200, OPEN_OK),
            PreparedResponse::new(200, OPEN_OK),
        ]));
        let pending = vec![VecDeque::from(vec![callback_frame(
            "m-1",
            r#"{"msgtype":"text","text":{"content":"hi"}}"#,
        )])];
        let client = client(
            transport,
            Arc::new(ScriptedConnector {
                pending: Mutex::new(pending),
                recorder: Arc::clone(&recorder),
                connect_count: Arc::clone(&connect_count),
            }),
            Arc::new(RecordingSink {
                recorder: Arc::clone(&recorder),
            }),
        );

        assert!(!client.is_running());
        client.start();
        client.start(); // second call is a no-op
        assert!(client.is_running());

        for _ in 0..200 {
            if !recorder.payloads.lock().unwrap().is_empty()
                && connect_count.load(Ordering::SeqCst) >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        client.stop().await;

        assert!(!client.is_running());
        assert_eq!(recorder.payloads.lock().unwrap().len(), 1);
        assert!(connect_count.load(Ordering::SeqCst) >= 2, "reconnected after close");
    }

    #[tokio::test]
    async fn stop_while_sleeping_in_backoff_ends_the_loop() {
        let recorder = Arc::new(Recorder::default());
        let connect_count = Arc::new(AtomicUsize::new(0));
        // Every register call fails, so the loop can only be in a backoff sleep when we stop it.
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(500, "boom"),
            PreparedResponse::new(500, "boom"),
        ]));
        let client = client(
            transport,
            Arc::new(ScriptedConnector {
                pending: Mutex::new(Vec::new()),
                recorder: Arc::clone(&recorder),
                connect_count: Arc::clone(&connect_count),
            }),
            Arc::new(RecordingSink { recorder }),
        );

        client.start();
        client.stop().await;
        assert!(!client.is_running());
        assert_eq!(connect_count.load(Ordering::SeqCst), 0, "never connected");
        // Stopping again is a no-op rather than a hang.
        client.stop().await;
    }
}
