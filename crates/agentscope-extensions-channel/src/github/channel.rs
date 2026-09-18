//! `GitHubChannel` — the GitHub `Channel` lifecycle/routing adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.github.GitHubChannel`. Routing/gateway state
//! lives in the harness `PlatformCtx` (see `agentscope_harness::agent::PlatformChannel`), so this
//! type only holds the GitHub-specific collaborators and implements `deliver_reply`.
//!
//! Inbound: [`GitHubChannel::handle_webhook`] runs the whole webhook pipeline (signature check,
//! JSON parse, mapping, bot-loop self-detection, de-duplication, dispatch) so the HTTP layer stays
//! a thin shell. Outbound: [`GitHubOutboundClient`] posts to
//! `/repos/{owner}/{repo}/issues/{number}/comments`.
//!
//! ## Design decision (vs. Java)
//!
//! Java gives every provider its own `*ChannelRegistry` singleton. Those classes differ only in
//! the channel type they hold, so Rust uses the single generic
//! [`ChannelRegistry`](crate::common::ChannelRegistry)`<GitHubChannel>` instead. It keeps the
//! typed handle the webhook controller needs while remaining instance-scoped so tests stay
//! isolated.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::Result as CoreResult;
use agentscope_core::message::Msg;
use agentscope_harness::agent::{ChannelConfig, OutboundAddress, PlatformChannel, PlatformCtx};
use serde_json::Value;

use super::bot_identity::GitHubBotIdentityResolver;
use super::inbound::GitHubInboundMapper;
use super::outbound::GitHubOutboundClient;
use super::properties::GitHubChannelProperties;
use super::signature::GitHubSignatureVerifier;
use crate::common::{
    to_core_error, BotLoopGuard, ChannelCore, ChannelRegistry, HttpTransport, IdempotencyStore,
    Result,
};

/// Typed registry of `GitHubChannel` handles, keyed by `channel_id`.
pub type GitHubChannelRegistry = ChannelRegistry<GitHubChannel>;

/// Result of a webhook request, decoupled from any web framework so the controller stays a thin
/// shell. Mirrors the HTTP status codes the Java `GitHubWebhookController` answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubCallbackOutcome {
    /// `X-Hub-Signature-256` verification failed (401).
    Unauthorized,
    /// The body was not valid JSON (400).
    BadRequest,
    /// Delivery acknowledged: duplicate, unsupported action, self-authored, or dispatched (200).
    Accepted,
}

/// GitHub channel adapter: shared routing/gateway state plus GitHub-specific collaborators.
pub struct GitHubChannel {
    core: ChannelCore,
    properties: GitHubChannelProperties,
    outbound: GitHubOutboundClient,
    mapper: GitHubInboundMapper,
    bot_identity: Arc<GitHubBotIdentityResolver>,
    verifier: GitHubSignatureVerifier,
}

impl GitHubChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        properties: GitHubChannelProperties,
        outbound: GitHubOutboundClient,
        mapper: GitHubInboundMapper,
        bot_identity: Arc<GitHubBotIdentityResolver>,
        verifier: GitHubSignatureVerifier,
        idempotency: IdempotencyStore,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        Self {
            core: ChannelCore::with_components(channel_id, config, idempotency, bot_loop_guard),
            properties,
            outbound,
            mapper,
            bot_identity,
            verifier,
        }
    }

    /// Build from the channel property map, mirroring the Java `fromProperties` factory.
    pub fn from_json(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let properties = GitHubChannelProperties::from_json(channel_id, props)?;
        let verifier = GitHubSignatureVerifier::new(&properties.webhook_secret)?;
        let outbound = GitHubOutboundClient::new(
            properties.api_base.clone(),
            properties.token.clone(),
            transport.clone(),
        );
        let bot_identity = Arc::new(GitHubBotIdentityResolver::new(
            properties.api_base.clone(),
            properties.token.clone(),
            properties.bot_user_login.clone(),
            transport,
        ));
        Ok(Self::new(
            channel_id,
            routing,
            properties,
            outbound,
            GitHubInboundMapper::new(channel_id),
            bot_identity,
            verifier,
            IdempotencyStore::with_defaults(),
            BotLoopGuard::with_defaults(),
        ))
    }

    pub fn properties(&self) -> &GitHubChannelProperties {
        &self.properties
    }

    pub fn mapper(&self) -> &GitHubInboundMapper {
        &self.mapper
    }

    pub fn bot_identity(&self) -> &GitHubBotIdentityResolver {
        &self.bot_identity
    }

    pub fn verifier(&self) -> &GitHubSignatureVerifier {
        &self.verifier
    }

    /// The routing/gateway state plus the idempotency / bot-loop guards.
    pub fn core(&self) -> &ChannelCore {
        &self.core
    }

    /// Full webhook pipeline, in the Java order: signature check -> JSON parse -> mapping ->
    /// bot-loop self-detection -> de-duplication -> dispatch. Every drop/failure is acknowledged
    /// with [`GitHubCallbackOutcome::Accepted`] so GitHub stops retrying.
    pub async fn handle_webhook(
        &self,
        event_type: &str,
        signature: Option<&str>,
        raw_body: &str,
    ) -> GitHubCallbackOutcome {
        // 1. HMAC-SHA256 verification over the exact bytes GitHub signed.
        if !self.verifier.verify(signature, raw_body.as_bytes()) {
            return GitHubCallbackOutcome::Unauthorized;
        }

        // 2. Parse the JSON body; invalid JSON is the only client error.
        let payload: Value = match serde_json::from_str(raw_body) {
            Ok(value) => value,
            Err(_) => return GitHubCallbackOutcome::BadRequest,
        };

        // 3. Map to an `InboundMessage`; unsupported events/actions are acknowledged silently.
        let Some(inbound) = self.mapper.map(event_type, &payload) else {
            return GitHubCallbackOutcome::Accepted;
        };

        // 4. Bot-loop self-detection: drop comments authored by the bot itself.
        if self
            .bot_identity
            .is_self(GitHubInboundMapper::extract_commenter_id(&payload))
        {
            return GitHubCallbackOutcome::Accepted;
        }

        // 5. De-duplication by `comment.id` followed by the per-peer bot-loop guard.
        let dedupe_key = GitHubInboundMapper::extract_comment_id(&payload).map(|id| id.to_string());
        if !self.core.admit(dedupe_key.as_deref(), &inbound.peer.key()) {
            return GitHubCallbackOutcome::Accepted;
        }

        // 6. Dispatch; the channel delivers the reply through its own transport. Failures are
        //    swallowed so the provider stops retrying (Java logs and still answers 200).
        let _ = PlatformChannel::dispatch(self, inbound).await;
        GitHubCallbackOutcome::Accepted
    }
}

#[async_trait]
impl PlatformChannel for GitHubChannel {
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
    use agentscope_harness::agent::Peer;

    use crate::common::testing::RecordingTransport;
    use crate::github::bot_identity::BotIdentity;

    const API_BASE: &str = "https://api.github.com";
    const SECRET: &str = "topsecret";
    const COMMENT_ID: i64 = 4242;
    const BOT_ID: i64 = 99;

    fn verifier() -> GitHubSignatureVerifier {
        GitHubSignatureVerifier::new(SECRET).unwrap()
    }

    fn payload(commenter_id: i64) -> String {
        serde_json::json!({
            "action": "created",
            "repository": {
                "full_name": "octo/repo",
                "owner": { "login": "octo" }
            },
            "issue": { "number": 7 },
            "comment": {
                "id": COMMENT_ID,
                "body": "hello",
                "user": { "id": commenter_id, "login": "alice" }
            }
        })
        .to_string()
    }

    fn channel() -> GitHubChannel {
        let props = GitHubChannelProperties::from_json(
            "github",
            &serde_json::json!({"token": "t", "webhookSecret": SECRET}),
        )
        .unwrap();
        let transport = Arc::new(RecordingTransport::new());
        GitHubChannel::new(
            "github",
            ChannelConfig::of("github"),
            props,
            GitHubOutboundClient::new(API_BASE, "t", transport.clone()),
            GitHubInboundMapper::new("github"),
            Arc::new(GitHubBotIdentityResolver::new(API_BASE, "t", None, transport)),
            verifier(),
            IdempotencyStore::with_defaults(),
            BotLoopGuard::with_defaults(),
        )
    }

    fn signed(body: &str) -> Option<String> {
        Some(verifier().sign(body.as_bytes()))
    }

    #[test]
    fn channel_id_and_config_come_from_the_shared_core() {
        let c = channel();
        assert_eq!(c.channel_id(), "github");
        assert_eq!(c.config().channel_id, "github");
        assert_eq!(c.properties().api_base, API_BASE);
    }

    #[test]
    fn from_json_rejects_missing_credentials() {
        let transport = Arc::new(RecordingTransport::new());
        assert!(GitHubChannel::from_json(
            "github",
            ChannelConfig::of("github"),
            &serde_json::json!({}),
            transport
        )
        .is_err());
    }

    #[tokio::test]
    async fn rejects_a_bad_signature() {
        let c = channel();
        let body = payload(1);
        assert_eq!(
            c.handle_webhook("issue_comment", Some("sha256=deadbeef"), &body)
                .await,
            GitHubCallbackOutcome::Unauthorized
        );
        assert_eq!(
            c.handle_webhook("issue_comment", None, &body).await,
            GitHubCallbackOutcome::Unauthorized
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn rejects_malformed_json() {
        let c = channel();
        let body = "not-json";
        assert_eq!(
            c.handle_webhook("issue_comment", signed(body).as_deref(), body)
                .await,
            GitHubCallbackOutcome::BadRequest
        );
    }

    #[tokio::test]
    async fn admits_a_signed_issue_comment_once() {
        let c = channel();
        let body = payload(1);
        let sig = signed(&body);
        assert_eq!(
            c.handle_webhook("issue_comment", sig.as_deref(), &body).await,
            GitHubCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 1);

        // Duplicate delivery of the same `comment.id` is dropped by idempotency.
        assert_eq!(
            c.handle_webhook("issue_comment", sig.as_deref(), &body).await,
            GitHubCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 1);
    }

    #[tokio::test]
    async fn ignores_unsupported_events() {
        let c = channel();
        let body = payload(1);
        assert_eq!(
            c.handle_webhook("push", signed(&body).as_deref(), &body)
                .await,
            GitHubCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn drops_self_authored_comments() {
        let c = channel();
        c.bot_identity().set_identity(BotIdentity {
            user_id: BOT_ID,
            login: Some("my-bot".to_string()),
        });
        let body = payload(BOT_ID);
        assert_eq!(
            c.handle_webhook("issue_comment", signed(&body).as_deref(), &body)
                .await,
            GitHubCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn deliver_reply_goes_through_the_outbound_client() {
        let c = channel();
        let address = OutboundAddress::direct("github", "github:THREAD:octo/repo#7");
        PlatformChannel::deliver_reply(&c, &address, vec![Msg::user("reply")])
            .await
            .unwrap();
    }

    #[test]
    fn exposes_the_peer_key_shape() {
        // Guards the `owner/repo#number` peer id the mapper produces and the outbound client parses.
        let peer = Peer::new(agentscope_harness::agent::PeerKind::Thread, "octo/repo#7");
        assert_eq!(peer.key(), "thread:octo/repo#7");
    }
}
