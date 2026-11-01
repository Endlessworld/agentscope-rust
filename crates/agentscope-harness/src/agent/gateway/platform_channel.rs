//! Shared platform-channel plumbing: the parts of "one messaging platform adapter" that are
//! identical across providers.
//!
//! ## Why this lives in the harness
//!
//! `gateway::channel` already owns the routing skeleton ([`Channel`], [`ChannelRouter`],
//! [`ChannelConfig`], [`ChannelManager`]). What every *platform* adapter (Feishu, DingTalk, WeCom,
//! GitHub, GitLab, ...) repeats on top of that skeleton is the same four steps:
//!
//! 1. hold the gateway injected by `init(...)` and the live routing config
//! 2. resolve a route through `ChannelRouter`
//! 3. run the turn on the gateway with that route's outbound address
//! 4. deliver the reply back out through the platform's own transport
//!
//! Steps 1-3 and the `Channel` trait wiring are platform-agnostic, so they live here. Step 4 is
//! the only provider-specific one and stays behind [`PlatformChannel::deliver_reply`].
//!
//! ## Usage
//!
//! A platform adapter provides its own transport and implements exactly two methods:
//!
//! ```ignore
//! impl PlatformChannel for FeishuChannel {
//!     fn ctx(&self) -> &PlatformCtx { &self.core }
//!     async fn deliver_reply(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()> {
//!         self.outbound.send(address, &messages).await.map_err(Into::into)
//!     }
//! }
//! ```
//!
//! The blanket [`Channel`] impl below then makes it routable, and
//! [`PlatformChannelSink`] registers it for proactive outbound delivery.

use std::sync::{Arc, RwLock};

use agentscope_core::agent::CancelSignal;
use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;
use async_trait::async_trait;

use super::channel::channel::{unsupported_delivery_receipt, Channel, DeliveryReceipt};
use super::channel::config::ChannelConfig;
use super::channel::inbound::InboundMessage;
use super::channel::router::{ChannelRouter, RouteResult};
use super::harness_gateway::{ChannelSink, HarnessGateway, OutboundAddress};

/// Gateway + routing state shared by every platform channel adapter.
///
/// Mirrors the fields the Java `*Channel` subclasses each declare for themselves
/// (`channelId`, `ChannelConfig`, `ChannelRouter`, `Gateway`).
pub struct PlatformCtx {
    channel_id: String,
    config: RwLock<ChannelConfig>,
    router: ChannelRouter,
    gateway: RwLock<Option<Arc<HarnessGateway>>>,
}

impl std::fmt::Debug for PlatformCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformCtx")
            .field("channel_id", &self.channel_id)
            .field("has_gateway", &self.has_gateway())
            .finish()
    }
}

impl PlatformCtx {
    /// Build from the resolved routing config. Mirrors the Java constructor argument list
    /// (`channelId`, `ChannelConfig`, `new ChannelRouter(config.defaultAgentId())`).
    pub fn new(channel_id: impl Into<String>, config: ChannelConfig) -> Self {
        let channel_id = channel_id.into();
        let router = ChannelRouter::new(config.default_agent_id.as_deref());
        Self {
            channel_id,
            config: RwLock::new(config),
            router,
            gateway: RwLock::new(None),
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// A snapshot of the current routing config. Mirrors the Java `config()` accessor.
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

    /// The stored gateway, or an error when `init` was never called.
    pub fn gateway(&self) -> Result<Arc<HarnessGateway>> {
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
        self.gateway
            .read()
            .expect("gateway lock poisoned")
            .is_some()
    }

    /// Swap the routing config in place, mirroring the Java `applyRoutingConfig` live swap.
    /// Returns `true` when applied; a foreign `channel_id` is rejected.
    pub fn apply_routing_config(&self, new_config: ChannelConfig) -> bool {
        if new_config.channel_id != self.channel_id {
            return false;
        }
        *self.config.write().expect("config lock poisoned") = new_config;
        true
    }

    /// Resolve the route for `message` through the harness [`ChannelRouter`].
    pub fn resolve_route(&self, message: &InboundMessage) -> RouteResult {
        let config = self.config.read().expect("config lock poisoned");
        self.router.resolve_route(&config, message)
    }

    /// Resolve the route, run the turn on the gateway, and return the reply plus the address it
    /// must be delivered to. This is steps 2-3 shared by every platform `dispatch`.
    ///
    /// `cancel` is the caller's cancellation signal: a platform that loses its lease (or
    /// otherwise gives up on the turn) passes it so the execution can be stopped instead of
    /// only having its reply suppressed.
    pub async fn run_turn(
        &self,
        message: &InboundMessage,
        cancel: Option<CancelSignal>,
    ) -> Result<(Msg, OutboundAddress)> {
        let gateway = self.gateway()?;
        let route = self.resolve_route(message);
        let reply = gateway
            .run_with_outbound(
                Some(route.context),
                message.messages.clone(),
                Some(route.outbound_address.clone()),
                cancel,
            )
            .await?;
        Ok((reply, route.outbound_address))
    }
}

/// A messaging-platform channel adapter: shared gateway/routing state plus the one
/// provider-specific operation, delivering a reply back to the platform.
#[async_trait]
pub trait PlatformChannel: Send + Sync {
    /// The shared routing/gateway state.
    fn ctx(&self) -> &PlatformCtx;

    /// Deliver `messages` to `address` through this platform's transport. Implementations are
    /// best-effort, mirroring the Java controllers that log and still answer 2xx.
    async fn deliver_reply(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()>;

    /// Delivers one message and returns the provider's delivery receipt.
    ///
    /// Same contract as [`Channel::deliver_with_receipt`]: a platform whose transport reports a
    /// provider message id (Feishu's `message_id`, Weixin's `msgid`, ...) overrides it, and every
    /// other platform keeps the fail-closed default so a durable-delivery worker never mistakes
    /// "not implemented" for "delivered".
    ///
    /// The blanket [`Channel`] / [`ChannelSink`] impls below forward here, which is the only way
    /// an adapter built on this trait can report a receipt — the blanket impls own both trait
    /// methods, so an adapter cannot implement them itself.
    async fn deliver_with_receipt(
        &self,
        _address: &OutboundAddress,
        _message: &Msg,
        _delivery_id: Option<&str>,
    ) -> Result<DeliveryReceipt> {
        Err(unsupported_delivery_receipt(self.ctx().channel_id()))
    }

    /// Full inbound turn: route, run, and deliver the reply back out.
    async fn dispatch(&self, message: InboundMessage) -> Result<Msg> {
        let (reply, address) = self.ctx().run_turn(&message, None).await?;
        self.deliver_reply(&address, vec![reply.clone()]).await?;
        Ok(reply)
    }
}

/// Every [`PlatformChannel`] is a routable harness [`Channel`]; the platform adapter only supplies
/// `deliver_reply`, so no adapter repeats the config/gateway/dispatch wiring.
#[async_trait]
impl<T: PlatformChannel + ?Sized> Channel for T {
    fn channel_id(&self) -> &str {
        self.ctx().channel_id()
    }

    fn config(&self) -> ChannelConfig {
        self.ctx().config()
    }

    fn init(&self, gateway: Arc<HarnessGateway>) {
        self.ctx().init(gateway);
    }

    async fn dispatch(&self, message: InboundMessage) -> Result<Msg> {
        PlatformChannel::dispatch(self, message).await
    }

    fn apply_routing_config(&self, new_config: ChannelConfig) -> bool {
        self.ctx().apply_routing_config(new_config)
    }

    async fn deliver_with_receipt(
        &self,
        address: &OutboundAddress,
        message: &Msg,
        delivery_id: Option<&str>,
    ) -> Result<DeliveryReceipt> {
        PlatformChannel::deliver_with_receipt(self, address, message, delivery_id).await
    }
}

/// ...and a harness [`ChannelSink`] too, so proactive outbound delivery (gateway-initiated pushes)
/// goes through the same `deliver_reply` path as turn replies without an extra wrapper type.
#[async_trait]
impl<T: PlatformChannel + ?Sized> ChannelSink for T {
    fn channel_id(&self) -> &str {
        self.ctx().channel_id()
    }

    async fn deliver(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()> {
        self.deliver_reply(address, messages).await
    }

    async fn deliver_with_receipt(
        &self,
        address: &OutboundAddress,
        messages: Vec<Msg>,
        delivery_id: Option<&str>,
    ) -> Result<DeliveryReceipt> {
        // Java's receipt API is single-message: a batch is delivered in order and reports the
        // last provider receipt, and only a single-message batch carries the caller's delivery
        // id (a uuid shared by several messages would collapse them in the provider's dedupe
        // index). Same convention as the `ChannelSink` receipt adapter in the channel crate.
        let is_single = messages.len() == 1;
        let mut receipt = DeliveryReceipt::default();
        for message in &messages {
            receipt = PlatformChannel::deliver_with_receipt(
                self,
                address,
                message,
                if is_single { delivery_id } else { None },
            )
            .await?;
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use agentscope_core::message::MsgRole;

    use super::super::channel::peer::Peer;
    use super::super::harness_gateway::GatewayAgent;

    /// Records the messages the gateway calls it with and echoes them back.
    struct EchoAgent {
        seen: Mutex<Vec<Vec<Msg>>>,
    }

    #[async_trait]
    impl GatewayAgent for EchoAgent {
        async fn call(
            &self,
            messages: Vec<Msg>,
            _ctx: &super::super::harness_gateway::GatewayTurnContext,
        ) -> Result<Msg> {
            self.seen.lock().unwrap().push(messages);
            Ok(Msg::builder()
                .role(MsgRole::Assistant)
                .text_content("pong")
                .build())
        }
    }

    /// A platform channel whose transport is an in-memory recorder.
    struct RecordingChannel {
        core: PlatformCtx,
        delivered: Mutex<Vec<(OutboundAddress, Vec<Msg>)>>,
    }

    impl RecordingChannel {
        fn new(channel_id: &str, config: ChannelConfig) -> Self {
            Self {
                core: PlatformCtx::new(channel_id, config),
                delivered: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl PlatformChannel for RecordingChannel {
        fn ctx(&self) -> &PlatformCtx {
            &self.core
        }

        async fn deliver_reply(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()> {
            self.delivered
                .lock()
                .unwrap()
                .push((address.clone(), messages));
            Ok(())
        }
    }

    fn inbound(peer: Peer) -> InboundMessage {
        let msg = Msg::builder()
            .role(MsgRole::User)
            .text_content("ping")
            .build();
        InboundMessage::builder("feishu", peer, vec![msg]).build()
    }

    #[test]
    fn config_is_snapshotted_and_foreign_channel_id_rejected() {
        let c = RecordingChannel::new("feishu", ChannelConfig::of_with_default("feishu", "a1"));
        // `Channel` and `ChannelSink` both declare `channel_id()`, so qualify explicitly.
        assert_eq!(Channel::channel_id(&c), "feishu");
        assert_eq!(c.config().default_agent_id.as_deref(), Some("a1"));
        assert!(c.apply_routing_config(ChannelConfig::of("feishu")));
        assert_eq!(c.config().default_agent_id, None);
        assert!(!c.apply_routing_config(ChannelConfig::of("github")));
    }

    #[test]
    fn init_is_idempotent_and_gateway_is_required() {
        let c = RecordingChannel::new("feishu", ChannelConfig::of("feishu"));
        assert!(!c.ctx().has_gateway());
        assert!(c.ctx().gateway().is_err());
        let gw = Arc::new(HarnessGateway::create_standalone());
        c.init(gw.clone());
        assert!(c.ctx().has_gateway());
        // A second init must not replace the first gateway.
        c.init(Arc::new(HarnessGateway::create_standalone()));
        assert!(Arc::ptr_eq(&c.ctx().gateway().unwrap(), &gw));
    }

    #[test]
    fn resolve_route_honours_binding_then_channel_default() {
        use super::super::channel::binding::ChannelBinding;
        let c = RecordingChannel::new(
            "github",
            ChannelConfig::builder("github")
                .default_agent_id("fallback")
                .binding(ChannelBinding::for_peer("thread:o/r#1", "bound"))
                .build(),
        );
        let routed = c.ctx().resolve_route(&inbound(Peer::thread("o/r#1")));
        assert_eq!(routed.agent_id, "bound");
        assert_eq!(routed.matched_by, "peer");

        let fallback = c.ctx().resolve_route(&inbound(Peer::thread("o/r#2")));
        assert_eq!(fallback.agent_id, "fallback");
        assert_eq!(fallback.matched_by, "channel-default");
    }

    #[tokio::test]
    async fn dispatch_requires_a_gateway() {
        let c = RecordingChannel::new("feishu", ChannelConfig::of("feishu"));
        assert!(PlatformChannel::dispatch(&c, inbound(Peer::direct("u1")))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn dispatch_runs_the_turn_and_delivers_the_reply() {
        let agent = Arc::new(EchoAgent {
            seen: Mutex::new(Vec::new()),
        });
        let gateway = Arc::new(HarnessGateway::create_standalone());
        gateway.bind_main_agent(agent.clone()).await;

        let c = RecordingChannel::new("feishu", ChannelConfig::of_with_default("feishu", "main"));
        c.init(gateway);

        let reply = PlatformChannel::dispatch(&c, inbound(Peer::direct("u1")))
            .await
            .unwrap();
        assert_eq!(reply.text_content(), "pong");
        assert_eq!(agent.seen.lock().unwrap().len(), 1);

        // The reply went back out through the platform transport, addressed to the routed peer.
        let delivered = c.delivered.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0.to, "feishu:DIRECT:u1");
        assert_eq!(delivered[0].1[0].text_content(), "pong");
    }

    #[tokio::test]
    async fn sink_forwards_proactive_delivery_to_the_platform_transport() {
        use super::super::harness_gateway::ChannelManager;
        let channel = Arc::new(RecordingChannel::new("wecom", ChannelConfig::of("wecom")));
        let manager = Arc::new(ChannelManager::new());
        manager.register(channel.clone()).await;

        let address = OutboundAddress::direct("wecom", "wecom:DIRECT:u9");
        let push = vec![Msg::user("proactive")];
        manager.deliver(&address, push).await;

        let delivered = channel.delivered.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0.to, "wecom:DIRECT:u9");
    }

    /// A platform adapter whose transport reports the provider's message id.
    struct ReceiptReportingChannel {
        core: PlatformCtx,
        delivered: Mutex<Vec<(OutboundAddress, Vec<Msg>)>>,
        receipt_calls: Mutex<Vec<Option<String>>>,
    }

    impl ReceiptReportingChannel {
        fn new(channel_id: &str, config: ChannelConfig) -> Self {
            Self {
                core: PlatformCtx::new(channel_id, config),
                delivered: Mutex::new(Vec::new()),
                receipt_calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl PlatformChannel for ReceiptReportingChannel {
        fn ctx(&self) -> &PlatformCtx {
            &self.core
        }

        async fn deliver_reply(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()> {
            self.delivered
                .lock()
                .unwrap()
                .push((address.clone(), messages));
            Ok(())
        }

        async fn deliver_with_receipt(
            &self,
            address: &OutboundAddress,
            message: &Msg,
            delivery_id: Option<&str>,
        ) -> Result<DeliveryReceipt> {
            self.receipt_calls
                .lock()
                .unwrap()
                .push(delivery_id.map(str::to_string));
            PlatformChannel::deliver_reply(self, address, vec![message.clone()]).await?;
            Ok(DeliveryReceipt::with_message_id("mid-1"))
        }
    }

    /// The default is fail-closed on both trait views, and a receipt-less platform must not
    /// deliver either: no receipt means the caller cannot tell a retry from a first attempt.
    #[tokio::test]
    async fn default_deliver_with_receipt_is_fail_closed() {
        let c = RecordingChannel::new("feishu", ChannelConfig::of("feishu"));
        let address = OutboundAddress::direct("feishu", "feishu:DIRECT:u1");

        let err = Channel::deliver_with_receipt(&c, &address, &Msg::user("hi"), Some("d-1"))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unsupported"), "{err}");
        assert!(format!("{err}").contains("feishu"), "{err}");

        let err =
            ChannelSink::deliver_with_receipt(&c, &address, vec![Msg::user("hi")], Some("d-1"))
                .await
                .unwrap_err();
        assert!(format!("{err}").contains("unsupported"), "{err}");

        assert!(
            c.delivered.lock().unwrap().is_empty(),
            "the fail-closed default must not deliver and then report no receipt"
        );
    }

    /// An adapter that overrides `PlatformChannel::deliver_with_receipt` must be reachable
    /// through both blanket impls — they are the only `Channel` / `ChannelSink` an adapter gets.
    #[tokio::test]
    async fn blanket_impls_forward_an_overridden_receipt() {
        let c = ReceiptReportingChannel::new("feishu", ChannelConfig::of("feishu"));
        let address = OutboundAddress::direct("feishu", "feishu:DIRECT:u1");

        let receipt = Channel::deliver_with_receipt(&c, &address, &Msg::user("hi"), Some("d-1"))
            .await
            .unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("mid-1"));
        assert_eq!(c.delivered.lock().unwrap().len(), 1, "delivered once");
        assert_eq!(
            c.receipt_calls.lock().unwrap().clone(),
            vec![Some("d-1".to_string())]
        );

        // A single-message batch keeps the caller's delivery id...
        let receipt =
            ChannelSink::deliver_with_receipt(&c, &address, vec![Msg::user("a")], Some("d-2"))
                .await
                .unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("mid-1"));

        // ...a multi-message batch drops it and reports the last receipt.
        let receipt = ChannelSink::deliver_with_receipt(
            &c,
            &address,
            vec![Msg::user("a"), Msg::user("b")],
            Some("d-3"),
        )
        .await
        .unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("mid-1"));

        assert_eq!(
            c.receipt_calls.lock().unwrap().clone(),
            vec![Some("d-1".to_string()), Some("d-2".to_string()), None, None]
        );
        assert_eq!(c.delivered.lock().unwrap().len(), 4);
    }
}
