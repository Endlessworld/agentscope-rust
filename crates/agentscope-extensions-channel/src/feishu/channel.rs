//! `FeishuChannel` — the Feishu `Channel` lifecycle/routing adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.feishu.FeishuChannel`. The routing/gateway
//! wiring lives in the harness `PlatformCtx` (see `agentscope_harness::agent::PlatformChannel`),
//! so this type only holds the Feishu-specific collaborators and implements `deliver_reply`.
//!
//! Inbound: [`FeishuChannel::handle_callback`] runs the whole callback pipeline (signature check,
//! decrypt, URL handshake, de-duplication, bot-loop guard, dispatch) so the HTTP layer stays a
//! thin shell. Outbound: [`FeishuOutboundClient`] posts to `/open-apis/im/v1/messages`.
//!
//! ## Design decision (vs. Java)
//!
//! Java gives every provider its own `*ChannelRegistry` singleton. Those five classes differ only
//! in the channel type they hold, so Rust uses the single generic
//! [`ChannelRegistry`](crate::common::ChannelRegistry)`<FeishuChannel>` instead (it also mirrors
//! registrations into the harness `ChannelManager` for proactive delivery). Registration is
//! explicit on the owning registry rather than a process-wide singleton, so tests stay isolated.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::Result as CoreResult;
use agentscope_core::message::Msg;
use agentscope_harness::agent::{ChannelConfig, OutboundAddress, PlatformChannel, PlatformCtx};
use serde_json::Value;

use super::crypto::FeishuCrypto;
use super::inbound::FeishuInboundMapper;
use super::outbound::FeishuOutboundClient;
use super::properties::FeishuChannelProperties;
use super::token::FeishuAccessTokenProvider;
use crate::common::{
    constant_time_eq, to_core_error, AccessTokenProvider, BotLoopGuard, ChannelCore,
    ChannelRegistry, ChannelSeams, HttpTransport, InboundEventDeduplicator, Result,
};

/// Typed registry of `FeishuChannel` handles, keyed by `channel_id`.
pub type FeishuChannelRegistry = ChannelRegistry<FeishuChannel>;

/// Result of a callback request, decoupled from any web framework so the controller stays a thin
/// shell. Mirrors the HTTP status codes the Java `FeishuCallbackController` answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuCallbackOutcome {
    /// Signature check failed (401).
    Unauthorized,
    /// The body could not be parsed or decrypted (400).
    BadRequest,
    /// URL-verification handshake: echo `{"challenge":"..."}` (200).
    Challenge(String),
    /// Event acknowledged: duplicate, non-text, loop-guarded, or dispatched (200 `{}`).
    Accepted,
}

/// Feishu channel adapter: shared routing/gateway state plus Feishu-specific collaborators.
pub struct FeishuChannel {
    core: ChannelCore,
    properties: FeishuChannelProperties,
    crypto: Option<FeishuCrypto>,
    outbound: FeishuOutboundClient,
    mapper: FeishuInboundMapper,
}

impl FeishuChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        properties: FeishuChannelProperties,
        crypto: Option<FeishuCrypto>,
        outbound: FeishuOutboundClient,
        mapper: FeishuInboundMapper,
        deduplicator: Arc<dyn InboundEventDeduplicator>,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        Self {
            core: ChannelCore::with_components(channel_id, config, deduplicator, bot_loop_guard),
            properties,
            crypto,
            outbound,
            mapper,
        }
    }

    /// Build from the channel property map, mirroring the Java `fromProperties` factory:
    /// crypto -> token provider -> outbound client -> inbound mapper. Uses the process-local
    /// de-duplicator and token cache.
    pub fn from_json(
        channel_id: &str,
        routing: ChannelConfig,
        properties: &Value,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        Self::from_json_with(
            channel_id,
            routing,
            properties,
            transport,
            &ChannelSeams::new(),
        )
    }

    /// [`FeishuChannel::from_json`] with injectable shared-storage seams (Java's
    /// `fromProperties(..., InboundEventDeduplicator[, AccessTokenStore])` overloads).
    pub fn from_json_with(
        channel_id: &str,
        routing: ChannelConfig,
        properties: &Value,
        transport: Arc<dyn HttpTransport>,
        seams: &ChannelSeams,
    ) -> Result<Self> {
        let props = FeishuChannelProperties::from_json(channel_id, properties)?;
        let crypto = if props.is_encrypted() {
            Some(FeishuCrypto::new(
                props.encrypt_key.clone().unwrap_or_default(),
            )?)
        } else {
            None
        };
        let token_provider: Arc<dyn AccessTokenProvider> = Arc::new(
            FeishuAccessTokenProvider::with_store(
                props.api_base.clone(),
                props.app_id.clone(),
                props.app_secret.clone(),
                transport.clone(),
                seams.token_store(),
            ),
        );
        let outbound = FeishuOutboundClient::new(props.api_base.clone(), token_provider, transport);
        Ok(Self::new(
            channel_id,
            routing,
            props,
            crypto,
            outbound,
            FeishuInboundMapper::new(channel_id),
            seams.deduplicator(),
            BotLoopGuard::with_defaults(),
        ))
    }

    pub fn properties(&self) -> &FeishuChannelProperties {
        &self.properties
    }

    pub fn crypto(&self) -> Option<&FeishuCrypto> {
        self.crypto.as_ref()
    }

    pub fn mapper(&self) -> &FeishuInboundMapper {
        &self.mapper
    }

    /// The routing/gateway state plus the idempotency / bot-loop guards.
    pub fn core(&self) -> &ChannelCore {
        &self.core
    }

    /// Full callback pipeline, in the Java order: signature check -> decrypt -> URL handshake ->
    /// event-token authentication -> mapping -> de-duplication -> bot-loop guard -> dispatch.
    pub async fn handle_callback(
        &self,
        signature: Option<&str>,
        timestamp: Option<&str>,
        nonce: Option<&str>,
        raw_body: &str,
    ) -> FeishuCallbackOutcome {
        // 1. Optional signature verification on the raw body. When the channel is encrypted and a
        //    signature is supplied, the signature MUST match.
        if self.properties.is_encrypted() && signature.is_some() {
            let verified = self
                .crypto
                .as_ref()
                .map(|c| c.verify_signature(signature, timestamp, nonce, Some(raw_body)))
                .unwrap_or(false);
            if !verified {
                return FeishuCallbackOutcome::Unauthorized;
            }
        }

        // 2. Parse the outer body; decrypt and re-parse when it is an encrypted envelope.
        let mut envelope: Value = match serde_json::from_str(raw_body) {
            Ok(value) => value,
            Err(_) => return FeishuCallbackOutcome::BadRequest,
        };
        if envelope.get("encrypt").is_some() {
            let Some(crypto) = self.crypto.as_ref() else {
                return FeishuCallbackOutcome::BadRequest;
            };
            let encrypted = envelope
                .get("encrypt")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match crypto
                .decrypt(encrypted)
                .ok()
                .and_then(|plain| serde_json::from_str(&plain).ok())
            {
                Some(value) => envelope = value,
                None => return FeishuCallbackOutcome::BadRequest,
            }
        }

        // 3. URL verification handshake: echo the challenge once the configured token matches.
        if let Some(challenge) = FeishuInboundMapper::extract_url_challenge(&envelope) {
            if let Some(configured) = self.properties.verification_token.as_deref() {
                let from_body = envelope.get("token").and_then(|v| v.as_str());
                if !configured.trim().is_empty() && Some(configured) != from_body {
                    return FeishuCallbackOutcome::Unauthorized;
                }
            }
            return FeishuCallbackOutcome::Challenge(challenge);
        }

        // 4. Event authenticity: normal events carry the configured `verificationToken` in the
        //    envelope header. A missing configuration also rejects — an unauthenticated event
        //    cannot be attributed to a sender (mirrors the Java controller's constant-time check).
        let configured_token = self
            .properties
            .verification_token
            .as_deref()
            .unwrap_or("")
            .trim();
        let event_token = envelope
            .pointer("/header/token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if configured_token.is_empty() || !constant_time_eq(configured_token, event_token) {
            return FeishuCallbackOutcome::Unauthorized;
        }

        // 5. Map to an `InboundMessage`; non-text / malformed events are acknowledged silently.
        let Some(inbound) = self.mapper.map(&envelope) else {
            return FeishuCallbackOutcome::Accepted;
        };

        // 6. De-duplication by `event_id`, then the per-peer bot-loop guard.
        let event_id = FeishuInboundMapper::extract_event_id(&envelope);
        if !self.core.admit(event_id.as_deref(), &inbound.peer.key()) {
            return FeishuCallbackOutcome::Accepted;
        }

        // 7. Dispatch; the channel delivers the reply through its own transport. Failures are
        //    swallowed so the provider stops retrying (Java logs and still answers 200).
        let _ = PlatformChannel::dispatch(self, inbound).await;
        FeishuCallbackOutcome::Accepted
    }
}

#[async_trait]
impl PlatformChannel for FeishuChannel {
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
    use crate::common::{ChannelError, IdempotencyStore};

    const API_BASE: &str = "https://open.feishu.cn";

    fn build(props_json: Value, encrypt_key: Option<&str>) -> FeishuChannel {
        let props = FeishuChannelProperties::from_json("feishu", &props_json).unwrap();
        let crypto = encrypt_key.map(|k| FeishuCrypto::new(k).unwrap());
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        FeishuChannel::new(
            "feishu",
            ChannelConfig::of("feishu"),
            props,
            crypto,
            FeishuOutboundClient::new(API_BASE, token, Arc::new(RecordingTransport::new())),
            FeishuInboundMapper::new("feishu"),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        )
    }

    fn plain() -> FeishuChannel {
        build(
            serde_json::json!({"appId": "cli_x", "appSecret": "s", "verificationToken": "vt"}),
            None,
        )
    }

    #[test]
    fn channel_id_and_config_come_from_the_shared_core() {
        let c = plain();
        assert_eq!(c.channel_id(), "feishu");
        assert_eq!(c.config().channel_id, "feishu");
        assert!(c.crypto().is_none());
    }

    #[tokio::test]
    async fn url_verification_echoes_the_challenge() {
        let c = plain();
        let body = r#"{"type":"url_verification","challenge":"nonce-1","token":"vt"}"#;
        assert_eq!(
            c.handle_callback(None, None, None, body).await,
            FeishuCallbackOutcome::Challenge("nonce-1".to_string())
        );
    }

    #[tokio::test]
    async fn url_verification_rejects_a_mismatched_configured_token() {
        let c = build(
            serde_json::json!({
                "appId": "cli_x", "appSecret": "s", "verificationToken": "vt"
            }),
            None,
        );
        let body = r#"{"type":"url_verification","challenge":"c","token":"wrong"}"#;
        assert_eq!(
            c.handle_callback(None, None, None, body).await,
            FeishuCallbackOutcome::Unauthorized
        );
    }

    #[tokio::test]
    async fn malformed_body_is_a_bad_request() {
        let c = plain();
        assert_eq!(
            c.handle_callback(None, None, None, "not-json").await,
            FeishuCallbackOutcome::BadRequest
        );
    }

    #[tokio::test]
    async fn encrypted_channel_rejects_a_signature_mismatch() {
        let c = build(
            serde_json::json!({"appId": "cli_x", "appSecret": "s", "encryptKey": "ek"}),
            Some("ek"),
        );
        assert!(c.crypto().is_some());
        assert_eq!(
            c.handle_callback(Some("bad"), Some("1"), Some("n"), "{}").await,
            FeishuCallbackOutcome::Unauthorized
        );
    }

    #[tokio::test]
    async fn encrypted_channel_accepts_a_valid_signature() {
        let c = build(
            serde_json::json!({"appId": "cli_x", "appSecret": "s", "encryptKey": "ek"}),
            Some("ek"),
        );
        let body = r#"{"type":"url_verification","challenge":"c"}"#;
        let signature = c.crypto().unwrap().sign("1", "n", body);
        assert_eq!(
            c.handle_callback(Some(&signature), Some("1"), Some("n"), body)
                .await,
            FeishuCallbackOutcome::Challenge("c".to_string())
        );
    }

    #[tokio::test]
    async fn encrypted_envelope_which_cannot_be_decrypted_is_a_bad_request() {
        let c = build(
            serde_json::json!({"appId": "cli_x", "appSecret": "s", "encryptKey": "ek"}),
            Some("ek"),
        );
        let body = r#"{"encrypt":"bm90LXJlYWxseS1jaXBoZXJ0ZXh0"}"#;
        assert_eq!(
            c.handle_callback(None, None, None, body).await,
            FeishuCallbackOutcome::BadRequest
        );
    }

    #[tokio::test]
    async fn non_text_events_are_acknowledged_without_admission() {
        let c = plain();
        let body = r#"{"header":{"event_id":"e1","token":"vt"},"event":{"message":{"message_type":"image"}}}"#;
        assert_eq!(
            c.handle_callback(None, None, None, body).await,
            FeishuCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn normal_events_require_the_configured_header_token() {
        let c = plain();
        let event = |token: &str| {
            format!(
                r#"{{"header":{{"event_id":"e1","token":"{token}"}},"event":{{"sender":{{"sender_type":"user","sender_id":{{"open_id":"ou_1"}}}},"message":{{"message_id":"om_1","message_type":"text","content":"{{\"text\":\"hi\"}}","chat_id":"oc_1","chat_type":"p2p"}}}}}}"#
            )
        };
        // A missing or mismatched header token is rejected before mapping.
        assert_eq!(
            c.handle_callback(None, None, None, &event("wrong")).await,
            FeishuCallbackOutcome::Unauthorized
        );
        assert_eq!(
            c.handle_callback(None, None, None, r#"{"header":{"event_id":"e1"},"event":{"message":{"message_type":"text"}}}"#)
                .await,
            FeishuCallbackOutcome::Unauthorized
        );
        assert_eq!(c.core().admitted(), 0);
        // The configured token is admitted.
        assert_eq!(
            c.handle_callback(None, None, None, &event("vt")).await,
            FeishuCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 1);
    }

    #[tokio::test]
    async fn events_are_rejected_when_no_verification_token_is_configured() {
        // Java parity: an unconfigured token still rejects ordinary events (401).
        let c = build(serde_json::json!({"appId": "cli_x", "appSecret": "s"}), None);
        let body = r#"{"header":{"event_id":"e1"},"event":{"message":{"message_type":"text"}}}"#;
        assert_eq!(
            c.handle_callback(None, None, None, body).await,
            FeishuCallbackOutcome::Unauthorized
        );
    }

    #[tokio::test]
    async fn duplicate_event_ids_are_dropped_by_the_idempotency_guard() {
        let c = plain();
        let body = r#"{"header":{"event_id":"e1","token":"vt"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_1"}},"message":{"message_id":"om_1","message_type":"text","content":"{\"text\":\"hi\"}","chat_id":"oc_1","chat_type":"p2p"}}}"#;
        // The first sighting is admitted; the duplicate never reaches the guards again.
        assert_eq!(
            c.handle_callback(None, None, None, body).await,
            FeishuCallbackOutcome::Accepted
        );
        assert_eq!(
            c.handle_callback(None, None, None, body).await,
            FeishuCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 1);
    }

    #[test]
    fn transport_errors_map_onto_the_core_error_type() {
        let err = to_core_error(ChannelError::Http("boom".to_string()));
        assert!(err.to_string().contains("boom"));
    }
}
