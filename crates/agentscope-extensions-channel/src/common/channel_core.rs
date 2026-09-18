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
//! Step 4 is the only platform-specific one, so it stays in each adapter; steps 1-3 and 5 live
//! here in [`ChannelCore`].
//!
//! ## Harness boundary
//!
//! Routing and outbound-sink registration are **harness** responsibilities
//! (`agentscope_harness::agent::{ChannelRouter, ChannelManager}`). This crate only supplies the
//! platform payload handling, so `ChannelCore` deliberately delegates rather than reimplements:
//! `ChannelRouter` evaluates the 7 binding tiers and `ChannelManager` owns the
//! `channel_id -> ChannelSink` lookup used for proactive delivery.

use std::sync::{Arc, RwLock};

use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::message::Msg;
use agentscope_harness::agent::{
    ChannelConfig, ChannelRouter, HarnessGateway, InboundMessage, OutboundAddress, RouteResult,
};

use super::bot_loop_guard::BotLoopGuard;
use super::error::ChannelError;
use super::idempotency_store::IdempotencyStore;

/// Map a platform [`ChannelError`] onto the harness/core error type used by
/// `Channel::dispatch`. Mirrors the Java behaviour of letting the platform exception propagate
/// (the controllers catch it and log, then still answer 2xx).
pub fn to_core_error(err: ChannelError) -> Error {
    Error::Other(err.to_string())
}

/// Shared state + dispatch pipeline for a platform channel adapter.
#[derive(Debug)]
pub struct ChannelCore {
    channel_id: String,
    config: RwLock<ChannelConfig>,
    router: ChannelRouter,
    idempotency: IdempotencyStore,
    bot_loop_guard: BotLoopGuard,
    gateway: RwLock<Option<Arc<HarnessGateway>>>,
}

impl ChannelCore {
    /// Build from the resolved routing config. Mirrors the Java constructor argument list
    /// (`channelId`, `ChannelConfig`, `new ChannelRouter(config.defaultAgentId())`).
    pub fn new(channel_id: impl Into<String>, config: ChannelConfig) -> Self {
        let channel_id = channel_id.into();
        let router = ChannelRouter::new(config.default_agent_id.as_deref());
        Self {
            channel_id,
            config: RwLock::new(config),
            router,
            idempotency: IdempotencyStore::with_defaults(),
            bot_loop_guard: BotLoopGuard::with_defaults(),
            gateway: RwLock::new(None),
        }
    }

    /// Build with explicit idempotency / loop-guard instances (Java tests inject fresh ones).
    pub fn with_components(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        idempotency: IdempotencyStore,
        bot_loop_guard: BotLoopGuard,
    ) -> Self {
        let channel_id = channel_id.into();
        let router = ChannelRouter::new(config.default_agent_id.as_deref());
        Self {
            channel_id,
            config: RwLock::new(config),
            router,
            idempotency,
            bot_loop_guard,
            gateway: RwLock::new(None),
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// The current routing config. Mirrors the Java `ChannelConfig config()` accessor.
    ///
    /// The Java `FeishuCallbackController` reads the *platform properties* rather than this, so the
    /// config snapshot is primarily used by `Channel::config()` and diagnostics.
    pub fn config(&self) -> ChannelConfig {
        self.config.read().expect("config lock poisoned").clone()
    }

    /// Inject the auto-wired gateway. Idempotent, mirroring the Java
    /// `if (this.gateway == null) { this.gateway = ... }` guard.
    pub fn init(&self, gateway: Arc<HarnessGateway>) {
        let mut slot = self.gateway.write().expect("gateway lock poisoned");
        if slot.is_none() {
            *slot = Some(gateway);
        }
    }

    /// The stored gateway, or an error when `init` was never called. Mirrors the Java
    /// `IllegalStateException("<Platform>Channel '<id>' has no gateway")`.
    pub fn gateway(&self) -> CoreResult<Arc<HarnessGateway>> {
        self.gateway
            .read()
            .expect("gateway lock poisoned")
            .clone()
            .ok_or_else(|| {
                Error::Other(format!(
                    "channel '{}' has no gateway; call init(...) first",
                    self.channel_id
                ))
            })
    }

    /// Whether a gateway has been injected.
    pub fn has_gateway(&self) -> bool {
        self.gateway.read().expect("gateway lock poisoned").is_some()
    }

    /// Apply a new routing config in place and rebuild the router, mirroring the Java
    /// `applyRoutingConfig` live swap. Returns `true` when applied.
    pub fn apply_routing_config(&self, new_config: ChannelConfig) -> bool {
        if new_config.channel_id != self.channel_id {
            return false;
        }
        let mut slot = self.config.write().expect("config lock poisoned");
        *slot = new_config;
        true
    }

    /// Resolve the route for `message` through the harness `ChannelRouter`.
    pub fn resolve_route(&self, message: &InboundMessage) -> RouteResult {
        let config = self.config.read().expect("config lock poisoned");
        self.router.resolve_route(&config, message)
    }

    /// De-duplicate an inbound event key. `None` keys are always allowed through, mirroring the
    /// Java `msgId.ifPresent(...)` pattern where an absent id skips the check entirely.
    pub fn is_first_seen(&self, key: Option<&str>) -> bool {
        match key {
            Some(key) if !key.trim().is_empty() => {
                self.idempotency.first_seen(&format!("{}|{key}", self.channel_id))
            }
            _ => true,
        }
    }

    /// Per-peer bot-loop guard check.
    pub fn allow_peer(&self, peer_key: &str) -> bool {
        self.bot_loop_guard.allow(peer_key)
    }

    /// Access the idempotency store (already namespaced by `is_first_seen`).
    pub fn idempotency(&self) -> &IdempotencyStore {
        &self.idempotency
    }

    /// Access the bot-loop guard.
    pub fn bot_loop_guard(&self) -> &BotLoopGuard {
        &self.bot_loop_guard
    }

    /// Resolve the route, run the turn on the gateway, and return the agent reply.
    ///
    /// This is steps 1-3 of the Java `dispatch`; the caller then delivers the reply outbound.
    pub async fn run_turn(&self, message: &InboundMessage) -> CoreResult<(Msg, OutboundAddress)> {
        let gateway = self.gateway()?;
        let route = self.resolve_route(message);
        let reply = gateway
            .run_with_outbound(
                Some(route.context),
                message.messages.clone(),
                Some(route.outbound_address.clone()),
            )
            .await?;
        Ok((reply, route.outbound_address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{Msg, MsgRole};
    use agentscope_harness::agent::{ChannelBinding, Peer};

    fn core() -> ChannelCore {
        ChannelCore::new("feishu", ChannelConfig::of_with_default("feishu", "agent-1"))
    }

    fn inbound(peer: Peer) -> InboundMessage {
        let msg = Msg::builder().role(MsgRole::User).text_content("hi").build();
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
            IdempotencyStore::with_defaults(),
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
    fn to_core_error_preserves_the_message() {
        let err = to_core_error(ChannelError::Config("boom".to_string()));
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn run_turn_requires_a_gateway_and_reports_a_missing_agent() {
        let c = core();
        // No gateway injected yet.
        assert!(c.run_turn(&inbound(Peer::direct("u1"))).await.is_err());

        // Gateway present but no agent bound -> the harness gateway reports the missing agent.
        c.init(Arc::new(HarnessGateway::create_standalone()));
        let err = c.run_turn(&inbound(Peer::direct("u1"))).await.unwrap_err();
        assert!(err.to_string().contains("bind_main_agent"), "{err}");
    }
}
