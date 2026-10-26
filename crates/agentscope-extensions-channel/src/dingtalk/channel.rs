//! `DingTalkChannel` — the DingTalk `Channel` lifecycle/routing adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk.DingTalkChannel`. The routing/gateway
//! wiring lives in the harness `PlatformCtx` (see `agentscope_harness::agent::PlatformChannel`),
//! so this type only holds the DingTalk-specific collaborators and implements `deliver_reply`.
//!
//! Reception mode is selected by [`DingTalkChannelProperties::mode`]:
//! - `stream` (default): [`DingTalkChannel::start`] lazily builds and starts a
//!   [`DingTalkStreamClient`]; [`DingTalkChannel::stop`] shuts it down.
//! - `http`: [`DingTalkChannel::start`] starts nothing (callbacks are delivered by
//!   [`super::controller::DingTalkCallbackController`]) and the adapter holds a
//!   [`DingTalkCallbackCrypto`] instead.
//!
//! Both modes funnel into [`DingTalkChannel::handle_stream_payload`], which runs the Java intake
//! order: de-duplication by `msgId`, mapping, bot-loop guard, dispatch.

use std::sync::{Arc, Mutex, Weak};

use agentscope_core::error::Result as CoreResult;
use agentscope_core::message::Msg;
use agentscope_harness::agent::{ChannelConfig, OutboundAddress, PlatformChannel, PlatformCtx};
use async_trait::async_trait;
use serde_json::Value;

use super::crypto::DingTalkCallbackCrypto;
use super::inbound::DingTalkInboundMapper;
use super::outbound::DingTalkOutboundClient;
use super::properties::DingTalkChannelProperties;
use super::stream::{
    DingTalkStreamClient, StreamPayloadSink, TungsteniteConnector, WebSocketConnector,
};
use super::token::DingTalkAccessTokenProvider;
use crate::common::{
    to_core_error, AccessTokenProvider, BotLoopGuard, ChannelCore, ChannelError, ChannelRegistry,
    ChannelSeams, HttpTransport, InboundEventDeduplicator, Result,
};

/// Typed registry of `DingTalkChannel` handles, keyed by `channel_id`.
pub type DingTalkChannelRegistry = ChannelRegistry<DingTalkChannel>;

/// DingTalk channel adapter: shared routing/gateway state plus DingTalk-specific collaborators.
pub struct DingTalkChannel {
    core: ChannelCore,
    properties: DingTalkChannelProperties,
    outbound: DingTalkOutboundClient,
    mapper: DingTalkInboundMapper,
    /// Present only in `http` mode (Java `crypto` field).
    crypto: Option<DingTalkCallbackCrypto>,
    /// Stream gateway transport, retained so `start()` can lazily build the client.
    transport: Option<Arc<dyn HttpTransport>>,
    connector: Arc<dyn WebSocketConnector>,
    /// Lazily created in `stream` mode; `None` before `start()` and after `stop()`.
    stream_client: Mutex<Option<Arc<DingTalkStreamClient>>>,
}

impl DingTalkChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        properties: DingTalkChannelProperties,
        outbound: DingTalkOutboundClient,
        mapper: DingTalkInboundMapper,
        deduplicator: Arc<dyn InboundEventDeduplicator>,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        Self {
            core: ChannelCore::with_components(channel_id, config, deduplicator, bot_loop_guard),
            properties,
            outbound,
            mapper,
            crypto: None,
            transport: None,
            connector: Arc::new(TungsteniteConnector),
            stream_client: Mutex::new(None),
        }
    }

    /// Install the HTTP-callback crypto / the Stream gateway transport.
    ///
    /// [`DingTalkChannel::from_json`] calls this for every channel; the direct [`Self::new`]
    /// constructor leaves both unset so existing stream-only callers keep working. `start()` in
    /// `http` mode requires `crypto`, and `start()` in `stream` mode requires `transport`.
    pub fn with_security(
        mut self,
        crypto: Option<DingTalkCallbackCrypto>,
        transport: Arc<dyn HttpTransport>,
        connector: Arc<dyn WebSocketConnector>,
    ) -> Self {
        self.crypto = crypto;
        self.transport = Some(transport);
        self.connector = connector;
        self
    }

    /// Build from the channel property map, mirroring the Java `fromProperties` factory:
    /// crypto (http only) -> token provider -> outbound client -> inbound mapper. Uses the
    /// process-local de-duplicator and token cache.
    pub fn from_json(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        Self::from_json_with(channel_id, routing, props, transport, &ChannelSeams::new())
    }

    /// [`DingTalkChannel::from_json`] with injectable shared-storage seams (Java's
    /// `fromProperties(..., InboundEventDeduplicator[, AccessTokenStore])` overloads).
    pub fn from_json_with(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
        seams: &ChannelSeams,
    ) -> Result<Self> {
        let properties = DingTalkChannelProperties::from_json(props)?;
        // Java builds `DingTalkCallbackCrypto(appSecret, aesKey)` only for http mode.
        let crypto = if properties.is_http() {
            Some(
                DingTalkCallbackCrypto::new(
                    properties.app_secret.clone(),
                    properties.aes_key.as_deref(),
                )?
                .with_token(properties.token.clone()),
            )
        } else {
            None
        };
        let token_provider: Arc<dyn AccessTokenProvider> =
            Arc::new(DingTalkAccessTokenProvider::with_store(
                properties.api_base.clone(),
                properties.app_key.clone(),
                properties.app_secret.clone(),
                transport.clone(),
                seams.token_store(),
            ));
        let outbound = DingTalkOutboundClient::new(
            properties.api_base.clone(),
            token_provider,
            properties.robot_code.clone(),
            transport.clone(),
        );
        let mapper = DingTalkInboundMapper::new(channel_id, Some(properties.app_key.clone()));
        Ok(Self::new(
            channel_id,
            routing,
            properties,
            outbound,
            mapper,
            seams.deduplicator(),
            BotLoopGuard::with_defaults(),
        )
        .with_security(crypto, transport, Arc::new(TungsteniteConnector)))
    }

    pub fn properties(&self) -> &DingTalkChannelProperties {
        &self.properties
    }

    pub fn mapper(&self) -> &DingTalkInboundMapper {
        &self.mapper
    }

    /// Callback verification/decryption helper; `None` outside `http` mode.
    pub fn crypto(&self) -> Option<&DingTalkCallbackCrypto> {
        self.crypto.as_ref()
    }

    /// The routing/gateway state plus the idempotency / bot-loop guards.
    pub fn core(&self) -> &ChannelCore {
        &self.core
    }

    /// Start reception for the configured mode.
    ///
    /// - `http`: no background task; the controller serves callbacks. Fails when the channel was
    ///   built without callback crypto (for example via the bare [`Self::new`] constructor).
    /// - `stream`: builds the [`DingTalkStreamClient`] on first call and starts its
    ///   connect/reconnect loop; later calls are a no-op.
    pub fn start(self: &Arc<Self>) -> Result<()> {
        if self.properties.is_http() {
            if self.crypto.is_none() {
                return Err(ChannelError::Config(
                    "dingtalk http mode requires callback crypto; build the channel with \
                     DingTalkChannel::from_json or DingTalkChannel::with_security"
                        .to_string(),
                ));
            }
            return Ok(());
        }

        let mut slot = self
            .stream_client
            .lock()
            .expect("dingtalk stream client slot poisoned");
        if slot.is_some() {
            return Ok(());
        }
        let Some(transport) = self.transport.clone() else {
            return Err(ChannelError::Config(
                "dingtalk stream mode requires an HTTP transport; build the channel with \
                 DingTalkChannel::from_json or DingTalkChannel::with_security"
                    .to_string(),
            ));
        };
        // A Weak sink avoids an Arc cycle: the client owns the sink, the channel owns the client.
        let sink: Arc<dyn StreamPayloadSink> = Arc::new(WeakChannelSink {
            channel: Arc::downgrade(self),
        });
        let client = Arc::new(DingTalkStreamClient::new(
            self.properties.clone(),
            transport,
            self.connector.clone(),
            sink,
        ));
        client.start();
        *slot = Some(client);
        Ok(())
    }

    /// Stop the Stream client (idempotent; no-op in `http` mode).
    pub async fn stop(&self) -> Result<()> {
        let client = self
            .stream_client
            .lock()
            .expect("dingtalk stream client slot poisoned")
            .take();
        if let Some(client) = client {
            client.stop().await;
        }
        Ok(())
    }

    /// Whether the Stream client exists and its loop is running (always `false` in `http` mode).
    pub fn is_stream_running(&self) -> bool {
        self.stream_client
            .lock()
            .expect("dingtalk stream client slot poisoned")
            .as_ref()
            .map(|client| client.is_running())
            .unwrap_or(false)
    }

    /// Inbound entry point, in the Java `onInboundPayload` order: extract `msgId`, map the
    /// payload (non-text / malformed payloads are dropped), then run the combined `msgId`
    /// de-duplication + per-peer bot-loop guard through [`ChannelCore::admit`].
    ///
    /// Returns `true` when the payload was accepted and dispatched. Dispatch failures are
    /// swallowed, mirroring the Java `doOnError(...).subscribe()` which acks regardless of the
    /// agent turn outcome.
    pub async fn handle_stream_payload(&self, payload: &Value) -> bool {
        // 1. Extract the idempotency key up front (Java `DingTalkInboundMapper.extractMsgId`).
        let msg_id = DingTalkInboundMapper::extract_msg_id(payload);

        // 2. Map to an `InboundMessage`; non-text / malformed payloads are acked silently.
        let Some(inbound) = self.mapper.map(payload) else {
            return false;
        };

        // 3. `ChannelCore::admit` runs the idempotency store (using `msgId` as the dedupe key)
        //    then the bot-loop guard, in the Java order.
        if !self.core.admit(msg_id.as_deref(), &inbound.peer.key()) {
            return false;
        }

        // 4. Dispatch; the channel delivers the reply through its own transport.
        let _ = PlatformChannel::dispatch(self, inbound).await;
        true
    }
}

/// Stream sink that upgrades a [`Weak`] handle, so the stream client never keeps the channel
/// alive after the host drops it.
struct WeakChannelSink {
    channel: Weak<DingTalkChannel>,
}

#[async_trait]
impl StreamPayloadSink for WeakChannelSink {
    async fn on_payload(&self, payload: Value) {
        if let Some(channel) = self.channel.upgrade() {
            let _ = channel.handle_stream_payload(&payload).await;
        }
    }
}

#[async_trait]
impl PlatformChannel for DingTalkChannel {
    fn ctx(&self) -> &PlatformCtx {
        self.core.ctx()
    }

    async fn deliver_reply(&self, address: &OutboundAddress, messages: Vec<Msg>) -> CoreResult<()> {
        self.outbound
            .send(address, &messages)
            .await
            .map_err(to_core_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::gateway::channel::Channel as _;

    use crate::common::testing::{RecordingTransport, StaticTokenProvider};
    use crate::common::{IdempotencyStore, PreparedResponse};
    use crate::dingtalk::stream::StreamSocket;

    const API_BASE: &str = "https://api.dingtalk.com";
    const OPEN_OK: &str = r#"{"endpoint":"wss://x/ws","ticket":"t-1"}"#;

    fn build(props_json: Value) -> DingTalkChannel {
        let props = DingTalkChannelProperties::from_json(&props_json).unwrap();
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        DingTalkChannel::new(
            "dingtalk",
            ChannelConfig::of_with_default("dingtalk", "agent-1"),
            props,
            DingTalkOutboundClient::new(
                API_BASE,
                token,
                "robot-1",
                Arc::new(RecordingTransport::new()),
            ),
            DingTalkInboundMapper::new("dingtalk", Some("key-1".to_string())),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        )
    }

    fn build_with_transport(
        props_json: Value,
        transport: Arc<dyn HttpTransport>,
        connector: Arc<dyn WebSocketConnector>,
    ) -> DingTalkChannel {
        let props = DingTalkChannelProperties::from_json(&props_json).unwrap();
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        DingTalkChannel::new(
            "dingtalk",
            ChannelConfig::of_with_default("dingtalk", "agent-1"),
            props,
            DingTalkOutboundClient::new(
                API_BASE,
                token,
                "robot-1",
                Arc::new(RecordingTransport::new()),
            ),
            DingTalkInboundMapper::new("dingtalk", Some("key-1".to_string())),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        )
        .with_security(None, transport, connector)
    }

    fn plain() -> DingTalkChannel {
        build(serde_json::json!({
            "appKey": "key-1", "appSecret": "secret-1", "robotCode": "robot-1"
        }))
    }

    fn text_payload(msg_id: &str, content: &str) -> Value {
        serde_json::json!({
            "msgtype": "text",
            "text": {"content": content},
            "conversationType": "1",
            "senderStaffId": "staff-7",
            "conversationId": "cid-1",
            "msgId": msg_id
        })
    }

    struct ClosedSocket;

    #[async_trait]
    impl StreamSocket for ClosedSocket {
        async fn send_text(&mut self, _text: String) -> Result<()> {
            Ok(())
        }

        async fn recv_text(&mut self) -> Option<Result<String>> {
            None
        }

        async fn close(&mut self) {}
    }

    /// Connector yielding sockets that close immediately, so `start()` connects without network.
    struct ClosedConnector;

    #[async_trait]
    impl WebSocketConnector for ClosedConnector {
        async fn connect(&self, _url: &str) -> Result<Box<dyn StreamSocket>> {
            Ok(Box::new(ClosedSocket))
        }
    }

    #[test]
    fn channel_id_and_config_come_from_the_shared_core() {
        let c = plain();
        assert_eq!(c.channel_id(), "dingtalk");
        assert_eq!(c.config().channel_id, "dingtalk");
        assert_eq!(c.config().default_agent_id.as_deref(), Some("agent-1"));
    }

    #[test]
    fn from_json_wires_properties_mapper_and_outbound() {
        let c = DingTalkChannel::from_json(
            "dingtalk",
            ChannelConfig::of("dingtalk"),
            &serde_json::json!({
                "appKey": "key-1", "appSecret": "secret-1", "robotCode": "robot-1"
            }),
            Arc::new(RecordingTransport::new()),
        )
        .unwrap();
        assert_eq!(c.properties().app_key, "key-1");
        assert_eq!(c.properties().robot_code, "robot-1");
        assert_eq!(c.mapper().channel_id(), "dingtalk");
        assert_eq!(c.channel_id(), "dingtalk");
        assert!(c.crypto().is_none(), "stream mode has no callback crypto");
    }

    #[test]
    fn from_json_builds_callback_crypto_only_in_http_mode() {
        let c = DingTalkChannel::from_json(
            "dingtalk",
            ChannelConfig::of("dingtalk"),
            &serde_json::json!({
                "appKey": "key-1",
                "appSecret": "secret-1",
                "robotCode": "robot-1",
                "mode": "http",
                "aesKey": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY",
                "token": "legacy-token"
            }),
            Arc::new(RecordingTransport::new()),
        )
        .unwrap();
        let crypto = c.crypto().expect("http mode installs crypto");
        assert!(crypto.has_aes_key());
        assert_eq!(crypto.token(), Some("legacy-token"));
    }

    #[test]
    fn from_json_rejects_missing_required_properties() {
        let result = DingTalkChannel::from_json(
            "dingtalk",
            ChannelConfig::of("dingtalk"),
            &serde_json::json!({"appKey": "key-1"}),
            Arc::new(RecordingTransport::new()),
        );
        assert!(matches!(
            result,
            Err(crate::common::ChannelError::Config(_))
        ));
    }

    #[tokio::test]
    async fn non_text_and_malformed_payloads_are_not_admitted() {
        let c = plain();
        assert!(
            !c.handle_stream_payload(&serde_json::json!({"msgtype": "image"}))
                .await
        );
        assert!(
            !c.handle_stream_payload(
                &serde_json::json!({"msgtype": "text", "text": {"content": "   "}})
            )
            .await
        );
        assert!(!c.handle_stream_payload(&Value::Null).await);
        assert!(!c.handle_stream_payload(&serde_json::json!({})).await);
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn a_valid_payload_is_admitted() {
        let c = plain();
        assert!(
            c.handle_stream_payload(&text_payload("msg-1", "hello bot"))
                .await
        );
        assert_eq!(c.core().admitted(), 1);
    }

    #[tokio::test]
    async fn duplicate_msg_ids_are_dropped_by_the_idempotency_guard() {
        let c = plain();
        let payload = text_payload("msg-dup", "hello");
        assert!(c.handle_stream_payload(&payload).await);
        assert!(!c.handle_stream_payload(&payload).await);
        assert_eq!(c.core().admitted(), 1);
    }

    #[tokio::test]
    async fn a_payload_without_msg_id_is_still_admitted() {
        let c = plain();
        let payload = serde_json::json!({
            "msgtype": "text",
            "text": {"content": "no id"},
            "conversationType": "1",
            "senderStaffId": "staff-7"
        });
        assert!(c.handle_stream_payload(&payload).await);
        assert!(c.handle_stream_payload(&payload).await);
        assert_eq!(c.core().admitted(), 2);
    }

    #[tokio::test]
    async fn stream_mode_start_and_stop_drive_the_gateway_client() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, OPEN_OK),
        ]));
        let channel = Arc::new(build_with_transport(
            serde_json::json!({
                "appKey": "key-1", "appSecret": "secret-1", "robotCode": "robot-1"
            }),
            transport,
            Arc::new(ClosedConnector),
        ));
        assert!(!channel.is_stream_running());
        channel.start().unwrap();
        assert!(channel.is_stream_running());
        channel.stop().await.unwrap();
        assert!(!channel.is_stream_running());
        // Stop is idempotent.
        channel.stop().await.unwrap();
    }

    #[tokio::test]
    async fn http_mode_start_does_not_start_a_stream_client() {
        let channel = Arc::new(
            DingTalkChannel::from_json(
                "dingtalk",
                ChannelConfig::of("dingtalk"),
                &serde_json::json!({
                    "appKey": "key-1",
                    "appSecret": "secret-1",
                    "robotCode": "robot-1",
                    "mode": "http"
                }),
                Arc::new(RecordingTransport::new()),
            )
            .unwrap(),
        );
        channel.start().unwrap();
        assert!(!channel.is_stream_running());
        channel.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stream_start_without_a_transport_is_an_explicit_error() {
        let channel = Arc::new(plain());
        let err = channel.start().unwrap_err();
        assert!(
            err.to_string().contains("requires an HTTP transport"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn http_start_without_crypto_is_an_explicit_error() {
        let channel = Arc::new(build(serde_json::json!({
            "appKey": "key-1",
            "appSecret": "secret-1",
            "robotCode": "robot-1",
            "mode": "http"
        })));
        let err = channel.start().unwrap_err();
        assert!(
            err.to_string().contains("requires callback crypto"),
            "{err}"
        );
    }
}
