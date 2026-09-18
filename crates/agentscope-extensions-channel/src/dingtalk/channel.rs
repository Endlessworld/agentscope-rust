//! `DingTalkChannel` — the DingTalk `Channel` lifecycle/routing adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk.DingTalkChannel`. The routing/gateway
//! wiring lives in the harness `PlatformCtx` (see `agentscope_harness::agent::PlatformChannel`),
//! so this type only holds the DingTalk-specific collaborators and implements `deliver_reply`.
//!
//! Inbound: [`DingTalkChannel::handle_stream_payload`] runs the payload pipeline (de-duplication
//! by `msgId`, mapping, bot-loop guard, dispatch) so the (not-yet-ported) Stream client stays a
//! thin shell. Outbound: [`DingTalkOutboundClient`] posts through the OpenAPI batchSend endpoints.
//!
//! ## Not ported in this round
//!
//! `DingTalkStreamClient` (the persistent WebSocket gateway) is out of scope. `start()` / `stop()`
//! are therefore not implemented: the payload entry point is exposed directly as
//! [`DingTalkChannel::handle_stream_payload`] so a future Stream client (or an HTTP bridge) can
//! drive it without changing this type.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::Result as CoreResult;
use agentscope_core::message::Msg;
use agentscope_harness::agent::{ChannelConfig, OutboundAddress, PlatformChannel, PlatformCtx};
use serde_json::Value;

use super::inbound::DingTalkInboundMapper;
use super::outbound::DingTalkOutboundClient;
use super::properties::DingTalkChannelProperties;
use super::token::DingTalkAccessTokenProvider;
use crate::common::{
    to_core_error, AccessTokenProvider, BotLoopGuard, ChannelCore, ChannelRegistry, HttpTransport,
    IdempotencyStore, Result,
};

/// Typed registry of `DingTalkChannel` handles, keyed by `channel_id`.
pub type DingTalkChannelRegistry = ChannelRegistry<DingTalkChannel>;

/// DingTalk channel adapter: shared routing/gateway state plus DingTalk-specific collaborators.
pub struct DingTalkChannel {
    core: ChannelCore,
    properties: DingTalkChannelProperties,
    outbound: DingTalkOutboundClient,
    mapper: DingTalkInboundMapper,
}

impl DingTalkChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        properties: DingTalkChannelProperties,
        outbound: DingTalkOutboundClient,
        mapper: DingTalkInboundMapper,
        idempotency: IdempotencyStore,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        Self {
            core: ChannelCore::with_components(channel_id, config, idempotency, bot_loop_guard),
            properties,
            outbound,
            mapper,
        }
    }

    /// Build from the channel property map, mirroring the Java `fromProperties` factory:
    /// token provider -> outbound client -> inbound mapper.
    pub fn from_json(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let properties = DingTalkChannelProperties::from_json(props)?;
        let token_provider: Arc<dyn AccessTokenProvider> = Arc::new(
            DingTalkAccessTokenProvider::new(
                properties.api_base.clone(),
                properties.app_key.clone(),
                properties.app_secret.clone(),
                transport.clone(),
            ),
        );
        let outbound = DingTalkOutboundClient::new(
            properties.api_base.clone(),
            token_provider,
            properties.robot_code.clone(),
            transport,
        );
        let mapper = DingTalkInboundMapper::new(channel_id, Some(properties.app_key.clone()));
        Ok(Self::new(
            channel_id,
            routing,
            properties,
            outbound,
            mapper,
            IdempotencyStore::with_defaults(),
            BotLoopGuard::with_defaults(),
        ))
    }

    pub fn properties(&self) -> &DingTalkChannelProperties {
        &self.properties
    }

    pub fn mapper(&self) -> &DingTalkInboundMapper {
        &self.mapper
    }

    /// The routing/gateway state plus the idempotency / bot-loop guards.
    pub fn core(&self) -> &ChannelCore {
        &self.core
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

    const API_BASE: &str = "https://api.dingtalk.com";

    fn build(props_json: Value) -> DingTalkChannel {
        let props = DingTalkChannelProperties::from_json(&props_json).unwrap();
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        DingTalkChannel::new(
            "dingtalk",
            ChannelConfig::of_with_default("dingtalk", "agent-1"),
            props,
            DingTalkOutboundClient::new(API_BASE, token, "robot-1", Arc::new(RecordingTransport::new())),
            DingTalkInboundMapper::new("dingtalk", Some("key-1".to_string())),
            IdempotencyStore::with_defaults(),
            BotLoopGuard::with_defaults(),
        )
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
    }

    #[test]
    fn from_json_rejects_missing_required_properties() {
        let result = DingTalkChannel::from_json(
            "dingtalk",
            ChannelConfig::of("dingtalk"),
            &serde_json::json!({"appKey": "key-1"}),
            Arc::new(RecordingTransport::new()),
        );
        assert!(matches!(result, Err(crate::common::ChannelError::Config(_))));
    }

    #[tokio::test]
    async fn non_text_and_malformed_payloads_are_not_admitted() {
        let c = plain();
        assert!(!c
            .handle_stream_payload(&serde_json::json!({"msgtype": "image"}))
            .await);
        assert!(!c
            .handle_stream_payload(&serde_json::json!({"msgtype": "text", "text": {"content": "   "}}))
            .await);
        assert!(!c.handle_stream_payload(&Value::Null).await);
        assert!(!c.handle_stream_payload(&serde_json::json!({})).await);
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn a_valid_payload_is_admitted() {
        let c = plain();
        assert!(c.handle_stream_payload(&text_payload("msg-1", "hello bot")).await);
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
}
