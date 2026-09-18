//! `WeComChannel` — the WeCom (企业微信) `Channel` lifecycle/routing adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.wecom.WeComChannel`. The routing/gateway wiring
//! lives in the harness [`PlatformCtx`], so this type only holds the WeCom-specific collaborators
//! and implements `deliver_reply`.
//!
//! Inbound: [`WeComChannel::handle_callback`] runs the whole callback pipeline (extract `<Encrypt>`,
//! signature check, decrypt, de-duplication by `MsgId`, bot-loop guard, dispatch) so the HTTP layer
//! stays a thin shell. [`WeComChannel::verify_url`] implements the GET URL-verification handshake
//! (verify + decrypt `echostr` and echo the plaintext).
//!
//! Outbound: [`WeComOutboundClient`] posts to `/cgi-bin/message/send` (DM) or `/cgi-bin/appchat/send`
//! (group).
//!
//! ## Design decisions (vs. Java)
//!
//! - Java gives every provider its own `*ChannelRegistry` singleton; Rust uses the generic
//!   [`ChannelRegistry`](crate::common::ChannelRegistry)`<WeComChannel>` (which also mirrors
//!   registrations into the harness `ChannelManager`) and keeps registration instance-scoped.
//! - Java's `WeComCrypto` is mandatory (`token` + `encodingAesKey` + `corpId` are all required
//!   properties), so the Rust adapter stores it directly rather than as an `Option` like Feishu.
//! - Java's controller performs the idempotency check on `MsgId` *before* mapping and the bot-loop
//!   guard *after*; [`ChannelCore`] bundles both, so this adapter calls `is_first_seen(MsgId)` for
//!   the de-duplication half and then `admit(None, peer)` for the bot-loop half, preserving the
//!   Java order between mapping and the two guards.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::Result as CoreResult;
use agentscope_core::message::Msg;
use agentscope_harness::agent::{ChannelConfig, OutboundAddress, PlatformChannel, PlatformCtx};
use serde_json::Value;

use super::crypto::WeComCrypto;
use super::inbound::WeComInboundMapper;
use super::outbound::WeComOutboundClient;
use super::properties::WeComChannelProperties;
use super::token::WeComAccessTokenProvider;
use crate::common::{
    to_core_error, xml_element_text, AccessTokenProvider, BotLoopGuard, ChannelCore, ChannelRegistry,
    HttpTransport, IdempotencyStore, Result,
};

/// Typed registry of `WeComChannel` handles, keyed by `channel_id`.
pub type WeComChannelRegistry = ChannelRegistry<WeComChannel>;

/// Result of a callback request, decoupled from any web framework so the controller stays a thin
/// shell. Mirrors the HTTP status codes the Java `WeComCallbackController` answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeComCallbackOutcome {
    /// URL-verification signature failed (401).
    Unauthorized,
    /// The body could not be parsed or decrypted (400).
    BadRequest,
    /// URL-verification handshake: return the decrypted `echostr` as plain text (200).
    Challenge(String),
    /// Message acknowledged: duplicate, non-text, loop-guarded, or dispatched (200).
    Accepted,
}

/// WeCom channel adapter: shared routing/gateway state plus WeCom-specific collaborators.
pub struct WeComChannel {
    core: ChannelCore,
    properties: WeComChannelProperties,
    crypto: WeComCrypto,
    outbound: WeComOutboundClient,
    mapper: WeComInboundMapper,
}

impl WeComChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        properties: WeComChannelProperties,
        crypto: WeComCrypto,
        outbound: WeComOutboundClient,
        mapper: WeComInboundMapper,
        idempotency: IdempotencyStore,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        Self {
            core: ChannelCore::with_components(channel_id, config, idempotency, bot_loop_guard),
            properties,
            crypto,
            outbound,
            mapper,
        }
    }

    /// Build from the channel property map, mirroring the Java `fromProperties` factory.
    pub fn from_json(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let properties = WeComChannelProperties::from_json(channel_id, props)?;
        let crypto = WeComCrypto::new(
            properties.token.clone(),
            &properties.encoding_aes_key,
            properties.corp_id.clone(),
        )?;
        let token_provider: Arc<dyn AccessTokenProvider> =
            Arc::new(WeComAccessTokenProvider::new(
                properties.api_base.clone(),
                properties.corp_id.clone(),
                properties.secret.clone(),
                transport.clone(),
            ));
        let outbound = WeComOutboundClient::new(
            properties.api_base.clone(),
            token_provider,
            properties.agent_id,
            transport,
        );
        let mapper = WeComInboundMapper::new(channel_id, Some(properties.corp_id.clone()));
        Ok(Self::new(
            channel_id,
            routing,
            properties,
            crypto,
            outbound,
            mapper,
            IdempotencyStore::with_defaults(),
            BotLoopGuard::with_defaults(),
        ))
    }

    pub fn properties(&self) -> &WeComChannelProperties {
        &self.properties
    }

    pub fn crypto(&self) -> &WeComCrypto {
        &self.crypto
    }

    pub fn mapper(&self) -> &WeComInboundMapper {
        &self.mapper
    }

    /// The routing/gateway state plus the idempotency / bot-loop guards.
    pub fn core(&self) -> &ChannelCore {
        &self.core
    }

    /// URL-verification handshake (GET): verify the signature over `echostr`, decrypt it, and hand
    /// the plaintext back for the controller to echo verbatim.
    pub async fn verify_url(
        &self,
        msg_signature: Option<&str>,
        timestamp: Option<&str>,
        nonce: Option<&str>,
        echostr: &str,
    ) -> WeComCallbackOutcome {
        // Java requires all three params; a missing signature cannot be verified.
        let (Some(signature), Some(timestamp), Some(nonce)) = (msg_signature, timestamp, nonce)
        else {
            return WeComCallbackOutcome::Unauthorized;
        };
        if !self
            .crypto
            .verify_signature(Some(signature), timestamp, nonce, echostr)
        {
            return WeComCallbackOutcome::Unauthorized;
        }
        match self.crypto.decrypt(echostr) {
            Ok(plain) => WeComCallbackOutcome::Challenge(plain),
            Err(_) => WeComCallbackOutcome::BadRequest,
        }
    }

    /// Full callback pipeline, in the Java controller order: extract `<Encrypt>` -> signature
    /// check -> decrypt -> de-duplicate by `MsgId` -> map -> bot-loop guard -> dispatch.
    ///
    /// Every rejected/undeliverable path still maps onto a 2xx-style [`WeComCallbackOutcome::Accepted`]
    /// (or an explicit error status) so WeCom does not enter a retry storm.
    pub async fn handle_callback(
        &self,
        msg_signature: Option<&str>,
        timestamp: Option<&str>,
        nonce: Option<&str>,
        raw_body: &str,
    ) -> WeComCallbackOutcome {
        // 1. The body must be an XML envelope carrying the ciphertext.
        let Some(encrypt) = extract_encrypt(raw_body) else {
            return WeComCallbackOutcome::BadRequest;
        };

        // 2. Signature over the ciphertext; required, and it MUST match.
        let (Some(signature), Some(timestamp), Some(nonce)) = (msg_signature, timestamp, nonce)
        else {
            return WeComCallbackOutcome::Unauthorized;
        };
        if !self
            .crypto
            .verify_signature(Some(signature), timestamp, nonce, &encrypt)
        {
            return WeComCallbackOutcome::Unauthorized;
        }

        // 3. Decrypt the payload into the inner XML.
        let Ok(xml) = self.crypto.decrypt(&encrypt) else {
            return WeComCallbackOutcome::BadRequest;
        };

        // 4. De-duplicate by `MsgId` (Java checks this before mapping); absent ids skip the check.
        let msg_id = WeComInboundMapper::extract_msg_id(&xml);
        if !self.core.is_first_seen(msg_id.as_deref()) {
            return WeComCallbackOutcome::Accepted;
        }

        // 5. Map to an `InboundMessage`; non-text / malformed events are acknowledged silently.
        let Some(inbound) = self.mapper.map(&xml) else {
            return WeComCallbackOutcome::Accepted;
        };

        // 6. Per-peer bot-loop guard (`admit(None, ..)` because de-duplication already ran above).
        if !self.core.admit(None, &inbound.peer.key()) {
            return WeComCallbackOutcome::Accepted;
        }

        // 7. Dispatch; the channel delivers the reply through its own transport. Failures are
        //    swallowed so the provider stops retrying (Java logs and still answers 200).
        let _ = PlatformChannel::dispatch(self, inbound).await;
        WeComCallbackOutcome::Accepted
    }
}

/// Extract the (trimmed, non-blank) `<Encrypt>` text from the callback envelope, mirroring the
/// Java `extractEncrypt` helper.
fn extract_encrypt(raw_body: &str) -> Option<String> {
    xml_element_text(raw_body, "Encrypt")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[async_trait]
impl PlatformChannel for WeComChannel {
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
    use crate::common::ChannelError;

    const CORP: &str = "corp-1";
    const TOKEN: &str = "wecom-token";
    /// 43-character fixture key (clone of the one in `crypto.rs`).
    const ENCODING_AES_KEY: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
    const TEXT_XML: &str = "<xml><ToUserName>corp-1</ToUserName><FromUserName>user-1</FromUserName>\
<MsgType>text</MsgType><Content>hello</Content><MsgId>9988776655443322</MsgId></xml>";
    const EVENT_XML: &str = "<xml><FromUserName>user-1</FromUserName><MsgType>event</MsgType>\
<Event>subscribe</Event></xml>";

    fn crypto() -> WeComCrypto {
        WeComCrypto::new(TOKEN, ENCODING_AES_KEY, CORP).unwrap()
    }

    fn props_json() -> Value {
        serde_json::json!({
            "corpId": CORP,
            "agentId": 100001,
            "secret": "s",
            "token": TOKEN,
            "encodingAesKey": ENCODING_AES_KEY
        })
    }

    fn build(channel_id: &str, crypto: WeComCrypto) -> WeComChannel {
        let properties = WeComChannelProperties::from_json(channel_id, &props_json()).unwrap();
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        WeComChannel::new(
            channel_id,
            ChannelConfig::of(channel_id),
            properties,
            crypto,
            WeComOutboundClient::new(
                "https://qyapi.weixin.qq.com",
                token,
                100001,
                Arc::new(RecordingTransport::new()),
            ),
            WeComInboundMapper::new(channel_id, Some(CORP.to_string())),
            IdempotencyStore::with_defaults(),
            BotLoopGuard::with_defaults(),
        )
    }

    /// A correctly signed, encrypted callback envelope plus its signature query params.
    fn signed_body(c: &WeComCrypto, xml: &str) -> (String, String, String, String) {
        let timestamp = "1700000000";
        let nonce = "nonce-1";
        let encrypt = c.encrypt(xml, CORP).unwrap();
        let signature = c.sign(timestamp, nonce, &encrypt);
        let body = format!("<xml><ToUserName>{CORP}</ToUserName><Encrypt>{encrypt}</Encrypt></xml>");
        (signature, timestamp.to_string(), nonce.to_string(), body)
    }

    #[test]
    fn channel_id_and_config_come_from_the_shared_core() {
        let c = build("wecom", crypto());
        assert_eq!(c.channel_id(), "wecom");
        assert_eq!(c.config().channel_id, "wecom");
        assert_eq!(c.properties().corp_id, CORP);
    }

    #[test]
    fn from_json_wires_properties_crypto_and_mapper() {
        let c = WeComChannel::from_json(
            "wecom",
            ChannelConfig::of("wecom"),
            &props_json(),
            Arc::new(RecordingTransport::new()),
        )
        .unwrap();
        assert_eq!(c.channel_id(), "wecom");
        assert_eq!(c.crypto().receive_id(), CORP);
        assert_eq!(c.mapper().channel_id(), "wecom");
        // A missing corpId is rejected before any collaborator is built.
        assert!(WeComChannel::from_json(
            "wecom",
            ChannelConfig::of("wecom"),
            &serde_json::json!({"agentId": 1}),
            Arc::new(RecordingTransport::new()),
        )
        .is_err());
    }

    #[tokio::test]
    async fn verify_url_echoes_the_decrypted_echostr() {
        let c = crypto();
        let (signature, timestamp, nonce, body) = signed_body(&c, "<xml><Foo>bar</Foo></xml>");
        let encrypt = extract_encrypt(&body).unwrap();
        let channel = build("wecom", c);
        assert_eq!(
            channel
                .verify_url(Some(&signature), Some(&timestamp), Some(&nonce), &encrypt)
                .await,
            WeComCallbackOutcome::Challenge("<xml><Foo>bar</Foo></xml>".to_string())
        );
    }

    #[tokio::test]
    async fn verify_url_rejects_a_bad_or_missing_signature() {
        let c = crypto();
        let (signature, timestamp, nonce, body) = signed_body(&c, TEXT_XML);
        let encrypt = extract_encrypt(&body).unwrap();
        let channel = build("wecom", c);
        assert_eq!(
            channel
                .verify_url(Some("deadbeef"), Some(&timestamp), Some(&nonce), &encrypt)
                .await,
            WeComCallbackOutcome::Unauthorized
        );
        assert_eq!(
            channel
                .verify_url(None, Some(&timestamp), Some(&nonce), &encrypt)
                .await,
            WeComCallbackOutcome::Unauthorized
        );
        assert_eq!(
            channel.verify_url(Some(&signature), None, None, &encrypt).await,
            WeComCallbackOutcome::Unauthorized
        );
    }

    #[tokio::test]
    async fn verify_url_with_a_correctly_signed_but_undecryptable_echostr_is_a_bad_request() {
        let c = crypto();
        let channel = build("wecom", c.clone());
        let bad_echostr = "not-really-ciphertext";
        let signature = c.sign("1700000000", "n", bad_echostr);
        assert_eq!(
            channel
                .verify_url(Some(&signature), Some("1700000000"), Some("n"), bad_echostr)
                .await,
            WeComCallbackOutcome::BadRequest
        );
    }

    #[tokio::test]
    async fn callback_without_an_encrypt_element_is_a_bad_request() {
        let c = build("wecom", crypto());
        assert_eq!(
            c.handle_callback(Some("s"), Some("1"), Some("n"), "not xml at all")
                .await,
            WeComCallbackOutcome::BadRequest
        );
        assert_eq!(
            c.handle_callback(Some("s"), Some("1"), Some("n"), "<xml><Encrypt> </Encrypt></xml>")
                .await,
            WeComCallbackOutcome::BadRequest
        );
    }

    #[tokio::test]
    async fn callback_rejects_a_signature_mismatch() {
        let c = crypto();
        let (_, timestamp, nonce, body) = signed_body(&c, TEXT_XML);
        let channel = build("wecom", c);
        assert_eq!(
            channel
                .handle_callback(Some("bad"), Some(&timestamp), Some(&nonce), &body)
                .await,
            WeComCallbackOutcome::Unauthorized
        );
        // Missing query params cannot be verified either.
        assert_eq!(
            channel.handle_callback(None, None, None, &body).await,
            WeComCallbackOutcome::Unauthorized
        );
    }

    #[tokio::test]
    async fn callback_with_an_undecryptable_payload_is_a_bad_request() {
        let c = crypto();
        let channel = build("wecom", c.clone());
        let cipher = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 32]);
        let signature = c.sign("1700000000", "n", &cipher);
        let body = format!("<xml><Encrypt>{cipher}</Encrypt></xml>");
        assert_eq!(
            channel
                .handle_callback(Some(&signature), Some("1700000000"), Some("n"), &body)
                .await,
            WeComCallbackOutcome::BadRequest
        );
    }

    #[tokio::test]
    async fn callback_maps_deduplicates_and_admits_a_text_message() {
        let c = crypto();
        let (signature, timestamp, nonce, body) = signed_body(&c, TEXT_XML);
        let channel = build("wecom", c);
        assert_eq!(
            channel
                .handle_callback(Some(&signature), Some(&timestamp), Some(&nonce), &body)
                .await,
            WeComCallbackOutcome::Accepted
        );
        assert_eq!(channel.core().admitted(), 1);

        // The same `MsgId` replayed is silently acknowledged without re-admission.
        assert_eq!(
            channel
                .handle_callback(Some(&signature), Some(&timestamp), Some(&nonce), &body)
                .await,
            WeComCallbackOutcome::Accepted
        );
        assert_eq!(channel.core().admitted(), 1);
    }

    #[tokio::test]
    async fn callback_acknowledges_non_text_payloads_without_admission() {
        let c = crypto();
        let (signature, timestamp, nonce, body) = signed_body(&c, EVENT_XML);
        let channel = build("wecom", c);
        assert_eq!(
            channel
                .handle_callback(Some(&signature), Some(&timestamp), Some(&nonce), &body)
                .await,
            WeComCallbackOutcome::Accepted
        );
        assert_eq!(channel.core().admitted(), 0);
    }

    #[test]
    fn transport_errors_map_onto_the_core_error_type() {
        let err = to_core_error(ChannelError::Http("boom".to_string()));
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn failed_dispatch_still_acknowledges_the_callback() {
        let c = crypto();
        let (signature, timestamp, nonce, body) = signed_body(&c, TEXT_XML);
        let channel = build("wecom", c);
        // No gateway has been injected, so the dispatch itself fails; the outcome stays 2xx.
        assert!(!channel.core().has_gateway());
        assert_eq!(
            channel
                .handle_callback(Some(&signature), Some(&timestamp), Some(&nonce), &body)
                .await,
            WeComCallbackOutcome::Accepted
        );
        assert_eq!(channel.core().admitted(), 1);
    }
}
