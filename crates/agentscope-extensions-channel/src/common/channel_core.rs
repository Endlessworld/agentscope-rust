//! Shared inbound-dispatch plumbing for the per-platform `*Channel` adapters.
//!
//! Every Java `*Channel` repeats the same five steps around its platform specifics:
//!
//! 1. hold the live `Gateway` injected by `init(...)`
//! 2. resolve a `RouteResult` through the harness `ChannelRouter`
//! 3. run the turn on the gateway (`gateway.run(context, messages, outbound, ...)`)
//! 4. send the reply back out through the platform outbound client (best-effort)
//! 5. swap the routing config live via `applyRoutingConfig`
//!
//! Steps 1-3 and 5 are *platform-agnostic*, so the harness owns them in
//! [`PlatformCtx`](agentscope_harness::agent::PlatformCtx) together with the
//! [`PlatformChannel`](agentscope_harness::agent::PlatformChannel) trait (which also provides the
//! blanket `Channel`/`ChannelSink` wiring). [`ChannelCore`] therefore does **not** re-store the
//! gateway/config/router — it composes a `PlatformCtx` and adds only the two guards the Java
//! `*Channel` classes each hold privately (`IdempotencyStore`, `BotLoopGuard`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::CancelSignal;
use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::message::Msg;
use agentscope_harness::agent::{
    ChannelConfig, HarnessGateway, InboundMessage, OutboundAddress, PlatformCtx, RouteResult,
};

use super::bot_loop_guard::BotLoopGuard;
use super::error::ChannelError;
use super::idempotency_store::{IdempotencyStore, InboundEventDeduplicator};
use super::token::{AccessTokenStore, InMemoryAccessTokenStore};

/// Map a platform [`ChannelError`] onto the harness/core error type used by
/// `Channel::dispatch`. Mirrors the Java behaviour of letting the platform exception propagate
/// (the controllers catch it and log, then still answer 2xx).
pub fn to_core_error(err: ChannelError) -> Error {
    Error::Other(err.to_string())
}

/// Shared-storage seams injected through the channel factories.
///
/// Java exposes three-tier `fromProperties(channelId, routing, rawProperties[, deduplicator[,
/// tokenStore]])` overloads so a multi-instance deployment can share de-duplication state and
/// token caches (Redis, ...). This is the Rust equivalent: both fields default to the
/// process-local implementations, so `ChannelSeams::new()` reproduces the config-driven path.
#[derive(Clone, Default)]
pub struct ChannelSeams {
    deduplicator: Option<Arc<dyn InboundEventDeduplicator>>,
    token_store: Option<Arc<dyn AccessTokenStore>>,
}

impl ChannelSeams {
    /// Process-local de-duplication and token caching (the Java default constructors).
    pub fn new() -> Self {
        Self::default()
    }

    /// Install an application-provided [`InboundEventDeduplicator`].
    pub fn with_deduplicator(mut self, deduplicator: Arc<dyn InboundEventDeduplicator>) -> Self {
        self.deduplicator = Some(deduplicator);
        self
    }

    /// Install an application-provided [`AccessTokenStore`].
    pub fn with_token_store(mut self, token_store: Arc<dyn AccessTokenStore>) -> Self {
        self.token_store = Some(token_store);
        self
    }

    /// The deduplicator to install, falling back to a process-local [`IdempotencyStore`].
    pub fn deduplicator(&self) -> Arc<dyn InboundEventDeduplicator> {
        self.deduplicator
            .clone()
            .unwrap_or_else(|| Arc::new(IdempotencyStore::with_defaults()))
    }

    /// The token store to install, falling back to a process-local [`InMemoryAccessTokenStore`].
    pub fn token_store(&self) -> Arc<dyn AccessTokenStore> {
        self.token_store
            .clone()
            .unwrap_or_else(|| Arc::new(InMemoryAccessTokenStore::new()))
    }
}

/// Shared state + dispatch pipeline for a platform channel adapter.
///
/// Routing/gateway state lives in the harness `PlatformCtx`; this type only adds the per-channel
/// guards. The counter lets tests assert how many events the guards admitted.
pub struct ChannelCore {
    ctx: PlatformCtx,
    deduplicator: Arc<dyn InboundEventDeduplicator>,
    bot_loop_guard: BotLoopGuard,
    admitted: AtomicUsize,
}

impl std::fmt::Debug for ChannelCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelCore")
            .field("ctx", &self.ctx)
            .field("admitted", &self.admitted.load(Ordering::Relaxed))
            .finish()
    }
}

impl ChannelCore {
    /// Build from the resolved routing config. Mirrors the Java constructor argument list
    /// (`channelId`, `ChannelConfig`, `new ChannelRouter(config.defaultAgentId())`).
    pub fn new(channel_id: impl Into<String>, config: ChannelConfig) -> Self {
        Self::with_components(
            channel_id,
            config,
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        )
    }

    /// Build with explicit de-duplicator / loop-guard instances (Java tests inject fresh ones, and
    /// multi-instance deployments inject a shared-storage [`InboundEventDeduplicator`]).
    pub fn with_components(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        deduplicator: Arc<dyn InboundEventDeduplicator>,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        Self {
            ctx: PlatformCtx::new(channel_id, config),
            deduplicator,
            bot_loop_guard,
            admitted: AtomicUsize::new(0),
        }
    }

    /// The shared routing/gateway state (also used by each adapter's `PlatformChannel::ctx`).
    pub fn ctx(&self) -> &PlatformCtx {
        &self.ctx
    }

    pub fn channel_id(&self) -> &str {
        self.ctx.channel_id()
    }

    /// The current routing config. Mirrors the Java `ChannelConfig config()` accessor.
    ///
    /// The Java `FeishuCallbackController` reads the *platform properties* rather than this, so the
    /// config snapshot is primarily used by `Channel::config()` and diagnostics.
    pub fn config(&self) -> ChannelConfig {
        self.ctx.config()
    }

    /// Inject the auto-wired gateway. Idempotent, mirroring the Java
    /// `if (this.gateway == null) { this.gateway = ... }` guard.
    pub fn init(&self, gateway: std::sync::Arc<HarnessGateway>) {
        self.ctx.init(gateway)
    }

    /// The stored gateway, or an error when `init` was never called. Mirrors the Java
    /// `IllegalStateException("<Platform>Channel '<id>' has no gateway")`.
    pub fn gateway(&self) -> CoreResult<std::sync::Arc<HarnessGateway>> {
        self.ctx.gateway()
    }

    /// Whether a gateway has been injected.
    pub fn has_gateway(&self) -> bool {
        self.ctx.has_gateway()
    }

    /// Apply a new routing config in place, mirroring the Java `applyRoutingConfig` live swap.
    /// Returns `true` when applied.
    pub fn apply_routing_config(&self, new_config: ChannelConfig) -> bool {
        self.ctx.apply_routing_config(new_config)
    }

    /// Resolve the route for `message` through the harness `ChannelRouter`.
    pub fn resolve_route(&self, message: &InboundMessage) -> RouteResult {
        self.ctx.resolve_route(message)
    }

    /// De-duplicate an inbound event key. `None` keys are always allowed through, mirroring the
    /// Java `msgId.ifPresent(...)` pattern where an absent id skips the check entirely.
    pub fn is_first_seen(&self, key: Option<&str>) -> bool {
        match key {
            Some(key) if !key.trim().is_empty() => self
                .deduplicator
                .first_seen(&format!("{}|{key}", self.ctx.channel_id())),
            _ => true,
        }
    }

    /// Number of inbound events that passed both guards (idempotency + bot-loop).
    pub fn admitted(&self) -> usize {
        self.admitted.load(Ordering::Relaxed)
    }

    /// Run the two inbound guards in the Java order (idempotency, then bot-loop) and count the
    /// admitted events. Returns `false` when the event must be dropped silently.
    pub fn admit(&self, dedupe_key: Option<&str>, peer_key: &str) -> bool {
        if !self.is_first_seen(dedupe_key) || !self.allow_peer(peer_key) {
            return false;
        }
        self.admitted.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Per-peer bot-loop guard check.
    pub fn allow_peer(&self, peer_key: &str) -> bool {
        self.bot_loop_guard.allow(peer_key)
    }

    /// Access the de-duplicator (already namespaced by `is_first_seen`).
    pub fn deduplicator(&self) -> &Arc<dyn InboundEventDeduplicator> {
        &self.deduplicator
    }

    /// Access the bot-loop guard.
    pub fn bot_loop_guard(&self) -> &BotLoopGuard {
        &self.bot_loop_guard
    }

    /// Resolve the route, run the turn on the gateway, and return the agent reply.
    ///
    /// This is steps 1-3 of the Java `dispatch`; the caller then delivers the reply outbound.
    ///
    /// `cancel` is the caller's cancellation signal: a platform that loses its lease (or
    /// otherwise gives up on the turn) passes it so the execution can be stopped instead of
    /// only having its reply suppressed.
    pub async fn run_turn(
        &self,
        message: &InboundMessage,
        cancel: Option<CancelSignal>,
    ) -> CoreResult<(Msg, OutboundAddress)> {
        self.ctx.run_turn(message, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use agentscope_core::message::{Msg, MsgRole};
    use agentscope_harness::agent::{ChannelBinding, Peer};

    use crate::common::token::{AccessTokenStore, CachedAccessToken, InMemoryAccessTokenStore};

    fn core() -> ChannelCore {
        ChannelCore::new(
            "feishu",
            ChannelConfig::of_with_default("feishu", "agent-1"),
        )
    }

    fn inbound(peer: Peer) -> InboundMessage {
        let msg = Msg::builder()
            .role(MsgRole::User)
            .text_content("hi")
            .build();
        InboundMessage::builder("feishu", peer, vec![msg]).build()
    }

    #[test]
    fn config_is_snapshotted_and_channel_id_preserved() {
        let c = core();
        assert_eq!(c.channel_id(), "feishu");
        assert_eq!(c.config().channel_id, "feishu");
        assert_eq!(c.config().default_agent_id.as_deref(), Some("agent-1"));
    }

    #[test]
    fn apply_routing_config_rejects_foreign_channel_id() {
        let c = core();
        assert!(c.apply_routing_config(ChannelConfig::of("feishu")));
        assert_eq!(c.config().default_agent_id, None);
        assert!(!c.apply_routing_config(ChannelConfig::of("github")));
    }

    #[test]
    fn init_is_idempotent_and_gateway_is_required() {
        let c = core();
        assert!(!c.has_gateway());
        assert!(c.gateway().is_err());
        let gw = Arc::new(HarnessGateway::create_standalone());
        c.init(gw.clone());
        assert!(c.has_gateway());
        assert!(c.gateway().is_ok());
        // A second init must not replace the first gateway.
        c.init(Arc::new(HarnessGateway::create_standalone()));
        assert!(Arc::ptr_eq(&c.gateway().unwrap(), &gw));
    }

    #[test]
    fn resolve_route_honours_binding_then_channel_default() {
        let c = ChannelCore::new(
            "github",
            ChannelConfig::builder("github")
                .default_agent_id("fallback")
                .binding(ChannelBinding::for_peer("thread:o/r#1", "bound-agent"))
                .build(),
        );
        let routed = c.resolve_route(&inbound(Peer::thread("o/r#1")));
        assert_eq!(routed.agent_id, "bound-agent");
        assert_eq!(routed.matched_by, "peer");

        let fallback = c.resolve_route(&inbound(Peer::thread("o/r#2")));
        assert_eq!(fallback.agent_id, "fallback");
        assert_eq!(fallback.matched_by, "channel-default");
    }

    #[test]
    fn is_first_seen_ignores_absent_keys_and_namespaces_by_channel() {
        let c = core();
        // Absent / blank keys bypass the store entirely.
        assert!(c.is_first_seen(None));
        assert!(c.is_first_seen(Some("   ")));
        // First sighting passes, the duplicate does not.
        assert!(c.is_first_seen(Some("evt-1")));
        assert!(!c.is_first_seen(Some("evt-1")));
        assert!(c.is_first_seen(Some("evt-2")));
    }

    #[test]
    fn allow_peer_uses_the_shared_loop_guard() {
        let c = ChannelCore::with_components(
            "wecom",
            ChannelConfig::of("wecom"),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::new(2, 60_000, 60_000).unwrap(),
        );
        assert!(c.allow_peer("direct:u1"));
        assert!(c.allow_peer("direct:u1"));
        // Third event inside the window trips the guard.
        assert!(!c.allow_peer("direct:u1"));
        // A different peer keeps its own budget.
        assert!(c.allow_peer("direct:u2"));
    }

    #[test]
    fn admit_applies_idempotency_then_the_loop_guard() {
        let c = ChannelCore::with_components(
            "feishu",
            ChannelConfig::of("feishu"),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::new(1, 60_000, 60_000).unwrap(),
        );
        // First event passes both guards.
        assert!(c.admit(Some("evt-1"), "direct:u1"));
        // Duplicate event id is dropped before the loop guard sees it.
        assert!(!c.admit(Some("evt-1"), "direct:u1"));
        // A fresh event id for the same peer still trips the per-peer budget.
        assert!(!c.admit(Some("evt-2"), "direct:u1"));
        assert_eq!(c.admitted(), 1);
    }

    #[test]
    fn an_injected_deduplicator_replaces_the_process_local_store() {
        /// Rejects every key, so admission must fail even for a first sighting.
        struct RejectAll;
        impl InboundEventDeduplicator for RejectAll {
            fn first_seen(&self, _key: &str) -> bool {
                false
            }
        }
        let c = ChannelCore::with_components(
            "github",
            ChannelConfig::of("github"),
            Arc::new(RejectAll),
            BotLoopGuard::with_defaults(),
        );
        assert!(!c.is_first_seen(Some("evt-1")));
        assert!(!c.admit(Some("evt-1"), "thread:o/r#1"));
        assert_eq!(c.admitted(), 0);
    }

    #[test]
    fn seams_default_to_process_local_implementations() {
        let seams = ChannelSeams::new();
        // Each call hands out a fresh process-local store; the channel core holds exactly one.
        let c = ChannelCore::with_components(
            "feishu",
            ChannelConfig::of("feishu"),
            seams.deduplicator(),
            BotLoopGuard::with_defaults(),
        );
        assert!(c.is_first_seen(Some("k")));
        assert!(!c.is_first_seen(Some("k")));
        assert!(seams.token_store().get().is_none());
    }

    #[test]
    fn seams_forward_injected_implementations() {
        let dedup = Arc::new(IdempotencyStore::new(1_000, 10).unwrap());
        let store: Arc<dyn AccessTokenStore> = Arc::new(InMemoryAccessTokenStore::new());
        let seams = ChannelSeams::new()
            .with_deduplicator(dedup)
            .with_token_store(store.clone());
        assert!(seams.deduplicator().first_seen("k"));
        assert!(!seams.deduplicator().first_seen("k"));
        store.put(CachedAccessToken::new("t", 10));
        assert_eq!(
            seams.token_store().get().map(|t| t.value),
            Some("t".to_string())
        );
    }

    #[test]
    fn to_core_error_preserves_the_message() {
        let err = to_core_error(ChannelError::Config("boom".to_string()));
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn run_turn_requires_a_gateway_and_reports_a_missing_agent() {
        let c = core();
        // No gateway injected yet.
        assert!(c
            .run_turn(&inbound(Peer::direct("u1")), None)
            .await
            .is_err());

        // Gateway present but no agent bound -> the harness gateway reports the missing agent.
        c.init(Arc::new(HarnessGateway::create_standalone()));
        let err = c
            .run_turn(&inbound(Peer::direct("u1")), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bind_main_agent"), "{err}");
    }
}
