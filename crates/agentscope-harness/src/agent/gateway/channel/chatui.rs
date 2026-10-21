//! Programmatic Chat UI channel, mirroring `io.agentscope.harness.agent.gateway.channel.chatui`.
//!
//! No external transport: the caller submits a `ChatUiRequest` (or a plain string)
//! programmatically and receives the agent reply.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;

use crate::agent::gateway::harness_gateway::{
    ChannelSink, HarnessGateway, OutboundAddress,
};
use crate::agent::gateway::msg_context::MsgContext;

use super::channel::Channel;
use super::config::ChannelConfig;
use super::dm_scope::DmScope;
use super::inbound::InboundMessage;
use super::router::{ChannelRouter, RouteResult};

/// Default channel id for the Chat UI adapter.
pub const CHANNEL_ID: &str = "chatui";

/// Default peer id used when no peer is provided (single-session mode).
const ANONYMOUS_PEER: &str = "__anonymous__";

/// A buffered proactive outbound message delivered to `ChatUiChannel` by the gateway.
#[derive(Debug, Clone)]
pub struct OutboundEnvelope {
    pub address: OutboundAddress,
    pub messages: Vec<Msg>,
    pub timestamp_ms: u64,
}

impl OutboundEnvelope {
    pub fn new(address: OutboundAddress, messages: Vec<Msg>) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self { address, messages, timestamp_ms }
    }
}

/// Routing identity for `ChatUiChannel` requests.
#[derive(Debug, Clone)]
pub struct SendOptions {
    pub user_id: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
}

impl SendOptions {
    /// One session per user — the most common case.
    pub fn user_id(user_id: impl Into<String>) -> Self {
        Self { user_id: user_id.into(), session_id: None, agent_id: None }
    }

    /// Explicit user + session — multiple conversations for the same user.
    pub fn of(user_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self { user_id: user_id.into(), session_id: Some(session_id.into()), agent_id: None }
    }

    /// Returns a copy with the given agent id override.
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// The effective session key: session id if provided, otherwise user id.
    pub fn effective_session_key(&self) -> String {
        match &self.session_id {
            Some(s) => s.clone(),
            None => self.user_id.clone(),
        }
    }
}

/// Request object for `ChatUiChannel`.
#[derive(Debug, Clone)]
pub struct ChatUiRequest {
    pub peer_id: Option<String>,
    pub agent_id: Option<String>,
    pub subagent_id: Option<String>,
    pub messages: Vec<Msg>,
}

impl ChatUiRequest {
    /// Single user-text message in single-session mode (no peer).
    pub fn of(text: impl Into<String>) -> Self {
        Self { peer_id: None, agent_id: None, subagent_id: None, messages: vec![Msg::user(text)] }
    }

    /// Single user-text message associated with a specific peer id.
    pub fn with_peer(peer_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            peer_id: Some(peer_id.into()),
            agent_id: None,
            subagent_id: None,
            messages: vec![Msg::user(text)],
        }
    }

    /// Single user-text message targeted at a specific agent.
    pub fn for_agent(peer_id: impl Into<String>, agent_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            peer_id: Some(peer_id.into()),
            agent_id: Some(agent_id.into()),
            subagent_id: None,
            messages: vec![Msg::user(text)],
        }
    }

    /// Single user-text message routed directly to an exposed subagent.
    pub fn to_subagent(subagent_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            peer_id: None,
            agent_id: None,
            subagent_id: Some(subagent_id.into()),
            messages: vec![Msg::user(text)],
        }
    }

    /// Multi-message request without a peer (single-session mode).
    pub fn of_messages(messages: Vec<Msg>) -> Self {
        Self { peer_id: None, agent_id: None, subagent_id: None, messages }
    }

    /// Multi-message request for a specific peer.
    pub fn with_peer_messages(peer_id: impl Into<String>, messages: Vec<Msg>) -> Self {
        Self { peer_id: Some(peer_id.into()), agent_id: None, subagent_id: None, messages }
    }
}

/// Default `Channel` implementation for direct Chat UI interactions.
pub struct ChatUiChannel {
    gateway: std::sync::RwLock<Option<Arc<HarnessGateway>>>,
    config: std::sync::RwLock<ChannelConfig>,
    router: ChannelRouter,
    outbound_queue: std::sync::Mutex<VecDeque<OutboundEnvelope>>,
}

impl ChatUiChannel {
    /// Creates a Chat UI channel with the default `Main` DM scope config.
    pub fn create() -> Self {
        Self::with_config(ChannelConfig::of(CHANNEL_ID))
    }

    /// Creates a Chat UI channel with an explicit config.
    pub fn with_config(config: ChannelConfig) -> Self {
        Self {
            gateway: std::sync::RwLock::new(None),
            config: std::sync::RwLock::new(config),
            router: ChannelRouter::new(None),
            outbound_queue: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// Creates a per-peer Chat UI channel. Each distinct peer id gets its own session.
    pub fn per_peer() -> Self {
        let cfg = ChannelConfig::builder(CHANNEL_ID).dm_scope(DmScope::PerPeer).build();
        Self::with_config(cfg)
    }

    /// Creates a Chat UI channel with a pre-wired gateway.
    pub fn with_gateway(gateway: Arc<HarnessGateway>) -> Self {
        let ch = Self::create();
        *ch.gateway.write().unwrap() = Some(gateway);
        ch
    }

    /// Creates a Chat UI channel with a pre-wired gateway and explicit config.
    pub fn with_gateway_config(gateway: Arc<HarnessGateway>, config: ChannelConfig) -> Self {
        let ch = Self::with_config(config);
        *ch.gateway.write().unwrap() = Some(gateway);
        ch
    }

    /// Returns the `RouteResult` this channel would produce for `message` without
    /// dispatching it.
    pub fn preview_route(&self, message: &InboundMessage) -> RouteResult {
        let config = self.config.read().unwrap().clone();
        self.router.resolve_route(&config, message)
    }

    /// Drains and returns all buffered proactive outbound messages.
    pub fn poll_outbound(&self) -> Vec<OutboundEnvelope> {
        let mut q = self.outbound_queue.lock().unwrap();
        q.drain(..).collect()
    }

    /// Returns the number of buffered proactive outbound messages.
    pub fn outbound_queue_size(&self) -> usize {
        self.outbound_queue.lock().unwrap().len()
    }

    /// Sends a plain-text message in single-session mode (no peer id).
    pub async fn send(&self, text: impl Into<String>) -> Result<Msg> {
        self.send_request(ChatUiRequest::of(text)).await
    }

    /// Sends a plain-text message from a specific peer.
    pub async fn send_peer(&self, peer_id: impl Into<String>, text: impl Into<String>) -> Result<Msg> {
        self.send_request(ChatUiRequest::with_peer(peer_id, text)).await
    }

    /// Sends a plain-text message with explicit routing identity.
    pub async fn send_with_options(&self, options: &SendOptions, text: impl Into<String>) -> Result<Msg> {
        let msg = Msg::user(text);
        self.dispatch_with_options(options, vec![msg]).await
    }

    /// Sends a message directly to an exposed subagent, bypassing normal routing.
    pub async fn send_to_subagent(&self, subagent_id: impl Into<String>, text: impl Into<String>) -> Result<Msg> {
        let msg = Msg::user(text);
        self.resolve_gateway().await?.run_subagent(&subagent_id.into(), vec![msg]).await
    }

    /// Sends a structured `ChatUiRequest` and returns the agent reply.
    pub async fn send_request(&self, request: ChatUiRequest) -> Result<Msg> {
        if let Some(subagent_id) = &request.subagent_id {
            return self.resolve_gateway().await?.run_subagent(subagent_id, request.messages).await;
        }
        let inbound = self.build_inbound(&request);
        self.dispatch(inbound).await
    }

    async fn dispatch_with_options(&self, options: &SendOptions, messages: Vec<Msg>) -> Result<Msg> {
        let config = self.config.read().unwrap().clone();
        let ctx = self.build_context_from_options(options, &config);
        let outbound = self.build_outbound_from_options(options);
        self.resolve_gateway()
            .await?
            .run_with_outbound(Some(ctx), messages, Some(outbound), None)
            .await
    }

    fn build_context_from_options(&self, options: &SendOptions, config: &ChannelConfig) -> MsgContext {
        let room = options.effective_session_key();
        let agent_id = options.agent_id.clone().or_else(|| config.default_agent_id.clone());
        let mut extra = std::collections::BTreeMap::new();
        if let Some(a) = &agent_id {
            extra.insert("agentId".to_string(), a.clone());
        }
        MsgContext {
            channel: Some(CHANNEL_ID.to_string()),
            group: None,
            room: Some(room),
            thread_id: None,
            thread_ts: None,
            extra,
            user_id: None,
        }
        .with_user_id(options.user_id.clone())
    }

    fn build_outbound_from_options(&self, options: &SendOptions) -> OutboundAddress {
        OutboundAddress::direct(CHANNEL_ID, format!("{}:{}", CHANNEL_ID, options.effective_session_key()))
    }

    fn build_inbound(&self, request: &ChatUiRequest) -> InboundMessage {
        let peer_id = request.peer_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let agent_id = request.agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let messages = request.messages.clone();
        match peer_id {
            Some(pid) if agent_id.is_some() => {
                InboundMessage::dm_for(CHANNEL_ID, pid, agent_id.unwrap(), messages)
            }
            Some(pid) => InboundMessage::dm(CHANNEL_ID, pid, messages),
            None if agent_id.is_some() => {
                InboundMessage::dm_for(CHANNEL_ID, ANONYMOUS_PEER, agent_id.unwrap(), messages)
            }
            None => InboundMessage::dm(CHANNEL_ID, ANONYMOUS_PEER, messages),
        }
    }

    async fn resolve_gateway(&self) -> Result<Arc<HarnessGateway>> {
        let g = self.gateway.read().unwrap().clone();
        g.ok_or_else(|| {
            Error::Other(
                "ChatUiChannel has no gateway. Use with_gateway(...), or init it via the gateway bootstrap."
                    .into(),
            )
        })
    }
}

#[async_trait]
impl Channel for ChatUiChannel {
    fn channel_id(&self) -> &str {
        CHANNEL_ID
    }

    fn config(&self) -> ChannelConfig {
        self.config.read().unwrap().clone()
    }

    fn init(&self, gateway: Arc<HarnessGateway>) {
        let mut g = self.gateway.write().unwrap();
        if g.is_none() {
            *g = Some(gateway);
        }
    }

    async fn dispatch(&self, message: InboundMessage) -> Result<Msg> {
        let config = self.config.read().unwrap().clone();
        let route = self.router.resolve_route(&config, &message);
        let gateway = self.resolve_gateway().await?;
        gateway
            .run_with_outbound(Some(route.context), message.messages, Some(route.outbound_address), None)
            .await
    }

    fn apply_routing_config(&self, new_config: ChannelConfig) -> bool {
        if new_config.channel_id != CHANNEL_ID {
            return false;
        }
        let mut cfg = self.config.write().unwrap();
        *cfg = new_config;
        true
    }
}

#[async_trait]
impl ChannelSink for ChatUiChannel {
    fn channel_id(&self) -> &str {
        CHANNEL_ID
    }

    async fn deliver(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()> {
        if !messages.is_empty() {
            self.outbound_queue
                .lock()
                .unwrap()
                .push_back(OutboundEnvelope::new(address.clone(), messages));
        }
        Ok(())
    }
}
