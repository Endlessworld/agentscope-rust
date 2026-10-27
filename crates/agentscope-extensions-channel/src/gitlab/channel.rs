//! `GitLabChannel` — the GitLab `Channel` lifecycle/routing adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.gitlab.GitLabChannel`. Routing/gateway state
//! lives in the harness `PlatformCtx` (see `agentscope_harness::agent::PlatformChannel`), so this
//! type only holds the GitLab-specific collaborators and implements `deliver_reply`.
//!
//! Inbound: [`GitLabChannel::handle_webhook`] runs the whole webhook pipeline (constant-time
//! `X-Gitlab-Token` check, JSON parse, mapping, bot-loop self-detection, de-duplication, dispatch)
//! so the HTTP layer stays a thin shell. Outbound: [`GitLabOutboundClient`] posts to
//! `/projects/{id}/{issues|merge_requests}/{iid}/notes`.
//!
//! ## Design decision (vs. Java)
//!
//! Java gives every provider its own `*ChannelRegistry` singleton. Those classes differ only in
//! the channel type they hold, so Rust uses the single generic
//! [`ChannelRegistry`](crate::common::ChannelRegistry)`<GitLabChannel>` instead. It keeps the typed
//! handle the webhook controller needs while remaining instance-scoped so tests stay isolated.

use std::sync::Arc;

use agentscope_core::error::Result as CoreResult;
use agentscope_core::message::Msg;
use agentscope_harness::agent::{ChannelConfig, OutboundAddress, PlatformChannel, PlatformCtx};
use async_trait::async_trait;
use serde_json::Value;

use super::bot_identity::GitLabBotIdentityResolver;
use super::inbound::GitLabInboundMapper;
use super::outbound::GitLabOutboundClient;
use super::properties::GitLabChannelProperties;
use super::signature::verify_webhook_token;
use crate::common::{
    to_core_error, BotLoopGuard, ChannelCore, ChannelRegistry, ChannelSeams, HttpTransport,
    InboundEventDeduplicator, Result,
};

/// The only webhook event this adapter maps (MVP), mirroring the Java controller.
pub const NOTE_HOOK_EVENT: &str = "Note Hook";

/// Typed registry of `GitLabChannel` handles, keyed by `channel_id`.
pub type GitLabChannelRegistry = ChannelRegistry<GitLabChannel>;

/// Result of a webhook request, decoupled from any web framework so the controller stays a thin
/// shell. Mirrors the HTTP status codes the Java `GitLabWebhookController` answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitLabCallbackOutcome {
    /// The `X-Gitlab-Token` check failed (401).
    Unauthorized,
    /// The body was not valid JSON (400).
    BadRequest,
    /// Hook acknowledged: unsupported event, duplicate, self-authored, or dispatched (200).
    Accepted,
}

/// GitLab channel adapter: shared routing/gateway state plus GitLab-specific collaborators.
pub struct GitLabChannel {
    core: ChannelCore,
    properties: GitLabChannelProperties,
    outbound: GitLabOutboundClient,
    mapper: GitLabInboundMapper,
    bot_identity: Arc<GitLabBotIdentityResolver>,
}

impl GitLabChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        properties: GitLabChannelProperties,
        outbound: GitLabOutboundClient,
        mapper: GitLabInboundMapper,
        bot_identity: Arc<GitLabBotIdentityResolver>,
        deduplicator: Arc<dyn InboundEventDeduplicator>,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        Self {
            core: ChannelCore::with_components(channel_id, config, deduplicator, bot_loop_guard),
            properties,
            outbound,
            mapper,
            bot_identity,
        }
    }

    /// Build from the channel property map, mirroring the Java `fromProperties` factory. Uses the
    /// process-local de-duplicator.
    pub fn from_json(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        Self::from_json_with(channel_id, routing, props, transport, &ChannelSeams::new())
    }

    /// [`GitLabChannel::from_json`] with an injectable [`InboundEventDeduplicator`] (Java's
    /// `fromProperties(..., InboundEventDeduplicator)` overload).
    pub fn from_json_with(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
        seams: &ChannelSeams,
    ) -> Result<Self> {
        let properties = GitLabChannelProperties::from_json(channel_id, props)?;
        let outbound = GitLabOutboundClient::new(
            properties.api_base.clone(),
            properties.token.clone(),
            transport.clone(),
        );
        let bot_identity = Arc::new(GitLabBotIdentityResolver::new(
            properties.api_base.clone(),
            properties.token.clone(),
            transport,
        ));
        Ok(Self::new(
            channel_id,
            routing,
            properties,
            outbound,
            GitLabInboundMapper::new(channel_id),
            bot_identity,
            seams.deduplicator(),
            BotLoopGuard::with_defaults(),
        ))
    }

    pub fn properties(&self) -> &GitLabChannelProperties {
        &self.properties
    }

    pub fn mapper(&self) -> &GitLabInboundMapper {
        &self.mapper
    }

    pub fn bot_identity(&self) -> &GitLabBotIdentityResolver {
        &self.bot_identity
    }

    /// The routing/gateway state plus the idempotency / bot-loop guards.
    pub fn core(&self) -> &ChannelCore {
        &self.core
    }

    /// Full webhook pipeline, in the Java order: `X-Gitlab-Token` check -> event-type gate ->
    /// JSON parse -> mapping -> bot-loop self-detection -> de-duplication -> dispatch. Every
    /// drop/failure is acknowledged with [`GitLabCallbackOutcome::Accepted`] so GitLab stops
    /// retrying (Java logs and still answers 2xx).
    pub async fn handle_webhook(
        &self,
        event_type: &str,
        token: Option<&str>,
        raw_body: &str,
    ) -> GitLabCallbackOutcome {
        // 1. Constant-time token check against the configured webhook token.
        if !verify_webhook_token(&self.properties.webhook_token, token) {
            return GitLabCallbackOutcome::Unauthorized;
        }

        // 2. Only Note Hook is mapped in the MVP; other event types are acked silently.
        if event_type != NOTE_HOOK_EVENT {
            return GitLabCallbackOutcome::Accepted;
        }

        // 3. Parse the JSON body; invalid JSON is the only client error.
        let payload: Value = match serde_json::from_str(raw_body) {
            Ok(value) => value,
            Err(_) => return GitLabCallbackOutcome::BadRequest,
        };

        // 4. Map to an `InboundMessage`; system notes / unsupported noteable types are acked.
        let Some(inbound) = self.mapper.map(&payload) else {
            return GitLabCallbackOutcome::Accepted;
        };

        // 5. Bot-loop self-detection: drop notes authored by the bot itself.
        if self
            .bot_identity
            .is_self(GitLabInboundMapper::extract_author_id(&payload))
        {
            return GitLabCallbackOutcome::Accepted;
        }

        // 6. De-duplication by `object_attributes.id`, then the per-peer bot-loop guard.
        let dedupe_key = GitLabInboundMapper::extract_note_id(&payload).map(|id| id.to_string());
        if !self.core.admit(dedupe_key.as_deref(), &inbound.peer.key()) {
            return GitLabCallbackOutcome::Accepted;
        }

        // 7. Dispatch; the channel delivers the reply through its own transport. Failures are
        //    swallowed so the provider stops retrying.
        let _ = PlatformChannel::dispatch(self, inbound).await;
        GitLabCallbackOutcome::Accepted
    }
}

#[async_trait]
impl PlatformChannel for GitLabChannel {
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

    const API_BASE: &str = "https://gitlab.com/api/v4";
    const WEBHOOK_TOKEN: &str = "hook-secret";
    const NOTE_ID: i64 = 4242;
    const BOT_ID: i64 = 99;

    fn payload(author_id: i64, note: &str) -> String {
        serde_json::json!({
            "object_kind": "note",
            "user": { "id": author_id, "username": "alice" },
            "project": {
                "path_with_namespace": "grp/proj",
                "namespace": "grp"
            },
            "object_attributes": {
                "id": NOTE_ID,
                "note": note,
                "noteable_type": "Issue",
                "system": false
            },
            "issue": { "iid": 9 }
        })
        .to_string()
    }

    fn channel() -> GitLabChannel {
        GitLabChannel::from_json(
            "gitlab",
            ChannelConfig::of("gitlab"),
            &serde_json::json!({"token": "t", "webhookToken": WEBHOOK_TOKEN}),
            Arc::new(RecordingTransport::new()),
        )
        .unwrap()
    }

    #[test]
    fn channel_id_and_config_come_from_the_shared_core() {
        let c = channel();
        assert_eq!(c.channel_id(), "gitlab");
        assert_eq!(c.config().channel_id, "gitlab");
        assert_eq!(c.properties().api_base, API_BASE);
        assert_eq!(c.bot_identity().bot_user_id(), -1);
    }

    #[test]
    fn from_json_rejects_missing_credentials() {
        let transport = Arc::new(RecordingTransport::new());
        assert!(GitLabChannel::from_json(
            "gitlab",
            ChannelConfig::of("gitlab"),
            &serde_json::json!({"token": "t"}),
            transport
        )
        .is_err());
    }

    #[tokio::test]
    async fn rejects_a_bad_or_missing_token() {
        let c = channel();
        let body = payload(1, "hello");
        assert_eq!(
            c.handle_webhook(NOTE_HOOK_EVENT, Some("wrong"), &body)
                .await,
            GitLabCallbackOutcome::Unauthorized
        );
        assert_eq!(
            c.handle_webhook(NOTE_HOOK_EVENT, None, &body).await,
            GitLabCallbackOutcome::Unauthorized
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn rejects_malformed_json() {
        let c = channel();
        assert_eq!(
            c.handle_webhook(NOTE_HOOK_EVENT, Some(WEBHOOK_TOKEN), "not-json")
                .await,
            GitLabCallbackOutcome::BadRequest
        );
    }

    #[tokio::test]
    async fn ignores_non_note_hook_events() {
        let c = channel();
        let body = payload(1, "hello");
        assert_eq!(
            c.handle_webhook("Push Hook", Some(WEBHOOK_TOKEN), &body)
                .await,
            GitLabCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn admits_a_note_hook_once_and_dedupes_the_note_id() {
        let c = channel();
        let body = payload(1, "hello");
        assert_eq!(
            c.handle_webhook(NOTE_HOOK_EVENT, Some(WEBHOOK_TOKEN), &body)
                .await,
            GitLabCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 1);

        // Duplicate delivery of the same `object_attributes.id` is dropped by idempotency.
        assert_eq!(
            c.handle_webhook(NOTE_HOOK_EVENT, Some(WEBHOOK_TOKEN), &body)
                .await,
            GitLabCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 1);
    }

    #[tokio::test]
    async fn drops_self_authored_notes() {
        let c = channel();
        c.bot_identity().set_identity(BotIdentity {
            user_id: BOT_ID,
            login: Some("glab-bot".to_string()),
        });
        let body = payload(BOT_ID, "hi");
        assert_eq!(
            c.handle_webhook(NOTE_HOOK_EVENT, Some(WEBHOOK_TOKEN), &body)
                .await,
            GitLabCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn system_notes_are_acknowledged_without_admission() {
        let c = channel();
        let body = serde_json::json!({
            "user": { "id": 1, "username": "alice" },
            "project": { "path_with_namespace": "grp/proj" },
            "object_attributes": {
                "id": NOTE_ID, "note": "x", "noteable_type": "Issue", "system": true
            },
            "issue": { "iid": 9 }
        })
        .to_string();
        assert_eq!(
            c.handle_webhook(NOTE_HOOK_EVENT, Some(WEBHOOK_TOKEN), &body)
                .await,
            GitLabCallbackOutcome::Accepted
        );
        assert_eq!(c.core().admitted(), 0);
    }

    #[tokio::test]
    async fn deliver_reply_goes_through_the_outbound_client() {
        let c = channel();
        let address = OutboundAddress::direct("gitlab", "gitlab:THREAD:grp/proj#9:Issue");
        PlatformChannel::deliver_reply(&c, &address, vec![Msg::user("reply")])
            .await
            .unwrap();
    }

    #[test]
    fn mapped_peer_id_round_trips_into_the_outbound_address() {
        // Guards the peer id the mapper produces (`path/with/namespace#iid:Type`): the harness
        // router turns it into `<channelId>:THREAD:<peerId>`, which the outbound client parses.
        let peer = Peer::new(
            agentscope_harness::agent::PeerKind::Thread,
            "grp/proj#9:Issue",
        );
        assert_eq!(peer.key(), "thread:grp/proj#9:Issue");
        let target = GitLabOutboundClient::parse_address("gitlab:THREAD:grp/proj#9:Issue").unwrap();
        assert_eq!(target.path, "grp/proj");
        assert_eq!(target.iid, 9);
    }
}
