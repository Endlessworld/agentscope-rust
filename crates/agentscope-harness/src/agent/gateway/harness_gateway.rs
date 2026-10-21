//! Default `Gateway` implementation, mirroring `HarnessGateway`: routes inbound turns
//! by canonical key, serializes concurrent turns per session via `SessionTurnGate`,
//! dispatches to registered gateway agents, and manages exposed subagents.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::CancelSignal;
use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;

use crate::agent::bus::MessageBus;

use super::msg_context::MsgContext;
use super::session_turn_gate::SessionTurnGate;
use super::subagent::{InMemorySubagentRegistry, SubagentRecord, SubagentRegistry};

/// Delivery target for proactive outbound replies (channel + optional recipient).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundAddress {
    pub channel_id: String,
    pub account_id: Option<String>,
    pub to: String,
    pub thread_id: Option<String>,
}

impl OutboundAddress {
    pub fn new(channel_id: impl Into<String>) -> Self {
        let channel_id = channel_id.into();
        Self {
            channel_id: channel_id.clone(),
            account_id: None,
            to: channel_id,
            thread_id: None,
        }
    }

    pub fn with_account(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    pub fn with_to(mut self, to: impl Into<String>) -> Self {
        self.to = to.into();
        self
    }

    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    pub fn direct(channel_id: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id: None,
            to: to.into(),
            thread_id: None,
        }
    }

    pub fn with_account_id(
        channel_id: impl Into<String>,
        account_id: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id: Some(account_id.into()),
            to: to.into(),
            thread_id: None,
        }
    }
}

/// Context passed to a gateway agent turn by the gateway.
#[derive(Debug, Clone)]
pub struct GatewayTurnContext {
    pub session_id: String,
    pub gate_key: String,
    pub msg_context: MsgContext,
    pub user_id: Option<String>,
    pub outbound: Option<OutboundAddress>,
    /// The caller's cancellation signal, when the turn can be stopped.
    ///
    /// The gateway hands the signal to the agent so a caller that loses its lease (or
    /// otherwise gives up on the turn) can end the execution instead of only suppressing
    /// the reply. An agent that cannot be cancelled simply ignores it.
    pub cancel: Option<CancelSignal>,
}

/// An agent that can be invoked by the gateway.
#[async_trait]
pub trait GatewayAgent: Send + Sync {
    /// Runs the agent against the given input messages and returns the reply.
    async fn call(&self, messages: Vec<Msg>, ctx: &GatewayTurnContext) -> Result<Msg>;
}

/// Outbound delivery sink registered with the `ChannelManager`.
#[async_trait]
pub trait ChannelSink: Send + Sync {
    fn channel_id(&self) -> &str;
    async fn deliver(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()>;
}

/// Registry and lifecycle manager for outbound delivery sinks.
#[derive(Default)]
pub struct ChannelManager {
    sinks: tokio::sync::RwLock<HashMap<String, Arc<dyn ChannelSink>>>,
}

impl ChannelManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, sink: Arc<dyn ChannelSink>) {
        self.sinks.write().await.insert(sink.channel_id().to_string(), sink);
    }

    pub async fn get_channel(&self, channel_id: &str) -> Option<Arc<dyn ChannelSink>> {
        self.sinks.read().await.get(channel_id).cloned()
    }

    pub async fn channel_ids(&self) -> Vec<String> {
        self.sinks.read().await.keys().cloned().collect()
    }

    /// Delivers proactive outbound messages through the target channel. Missing
    /// channels are logged and dropped (mirrors the Java warning-on-miss behaviour).
    pub async fn deliver(&self, address: &OutboundAddress, messages: Vec<Msg>) {
        if messages.is_empty() {
            return;
        }
        let sink = self.sinks.read().await.get(&address.channel_id).cloned();
        match sink {
            Some(s) => {
                if let Err(e) = s.deliver(address, messages).await {
                    tracing::warn!("channel '{}' delivery failed: {e}", address.channel_id);
                }
            }
            None => {
                tracing::warn!("no channel registered for '{}', dropping outbound", address.channel_id);
            }
        }
    }
}

/// Rebuilds a subagent `GatewayAgent` from its type id. Used to recover an exposed
/// subagent on a node that does not hold the live instance.
#[async_trait]
pub trait SubagentMaterializer: Send + Sync {
    async fn materialize(&self, agent_id: &str, session_id: &str) -> Option<Arc<dyn GatewayAgent>>;
}

/// A live, exposed subagent session held by this gateway (fast same-node path).
#[derive(Clone)]
struct ExposedSession {
    subagent_id: String,
    agent_id: String,
    session_id: String,
    agent: Arc<dyn GatewayAgent>,
    reply_to: Option<OutboundAddress>,
}

/// Default `Gateway` implementation.
pub struct HarnessGateway {
    channel_manager: Option<Arc<ChannelManager>>,
    message_bus: Option<Arc<dyn MessageBus>>,
    session_turn_gate: SessionTurnGate,
    main_agent: tokio::sync::RwLock<Option<Arc<dyn GatewayAgent>>>,
    agent_registry: tokio::sync::RwLock<HashMap<String, Arc<dyn GatewayAgent>>>,
    default_agent_id: tokio::sync::RwLock<Option<String>>,
    /// canonicalKey -> stable session id
    session_map: tokio::sync::RwLock<HashMap<String, String>>,
    /// session id -> canonicalKey (for wakeup-driven runs)
    session_to_gate_key: tokio::sync::RwLock<HashMap<String, String>>,
    /// subagentId -> live exposed session
    exposed_sessions: tokio::sync::RwLock<HashMap<String, ExposedSession>>,
    subagent_registry: tokio::sync::RwLock<Arc<dyn SubagentRegistry>>,
    subagent_materializer: tokio::sync::RwLock<Option<Arc<dyn SubagentMaterializer>>>,
    /// session id -> last outbound address (for proactive push delivery)
    last_route_by_session: tokio::sync::RwLock<HashMap<String, OutboundAddress>>,
}

impl HarnessGateway {
    /// Creates a gateway with a channel manager and an optional message bus.
    pub fn create(channel_manager: Option<Arc<ChannelManager>>, message_bus: Option<Arc<dyn MessageBus>>) -> Self {
        Self {
            channel_manager,
            message_bus,
            session_turn_gate: SessionTurnGate::new(),
            main_agent: tokio::sync::RwLock::new(None),
            agent_registry: tokio::sync::RwLock::new(HashMap::new()),
            default_agent_id: tokio::sync::RwLock::new(None),
            session_map: tokio::sync::RwLock::new(HashMap::new()),
            session_to_gate_key: tokio::sync::RwLock::new(HashMap::new()),
            exposed_sessions: tokio::sync::RwLock::new(HashMap::new()),
            subagent_registry: tokio::sync::RwLock::new(Arc::new(InMemorySubagentRegistry::new())),
            subagent_materializer: tokio::sync::RwLock::new(None),
            last_route_by_session: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Creates a gateway without outbound delivery support.
    pub fn create_standalone() -> Self {
        Self::create(None, None)
    }

    pub fn message_bus(&self) -> Option<Arc<dyn MessageBus>> {
        self.message_bus.clone()
    }

    pub fn channel_manager(&self) -> Option<Arc<ChannelManager>> {
        self.channel_manager.clone()
    }

    /// Binds the primary agent. It is registered under its agent id for routing.
    pub async fn bind_main_agent(&self, agent: Arc<dyn GatewayAgent>) {
        *self.main_agent.write().await = Some(agent.clone());
        let id = self.resolve_agent_id(&agent).await;
        self.agent_registry.write().await.insert(id.clone(), agent);
        *self.default_agent_id.write().await = Some(id);
    }

    /// Registers a named agent for routing.
    pub async fn register_agent(&self, agent_id: String, agent: Arc<dyn GatewayAgent>) {
        self.agent_registry.write().await.insert(agent_id, agent);
    }

    /// Returns the agent registered under `agent_id`, if any.
    pub async fn find_agent(&self, agent_id: &str) -> Option<Arc<dyn GatewayAgent>> {
        self.agent_registry.read().await.get(agent_id).cloned()
    }

    /// Inbound turn (single return), resolving the agent and session by context.
    pub async fn run(&self, context: Option<MsgContext>, messages: Vec<Msg>) -> Result<Msg> {
        self.run_with_outbound(context, messages, None, None).await
    }

    /// Inbound turn with outbound address, recorded as the session's last route.
    ///
    /// `cancel` is the caller's cancellation signal: when it fires, the agent ends the
    /// execution instead of the caller merely suppressing the reply.
    pub async fn run_with_outbound(
        &self,
        context: Option<MsgContext>,
        messages: Vec<Msg>,
        outbound: Option<OutboundAddress>,
        cancel: Option<CancelSignal>,
    ) -> Result<Msg> {
        let ctx = context.unwrap_or_else(MsgContext::default_context);
        let gate_key = ctx.canonical_key();

        let requested_agent_id = ctx.extra.get("agentId").cloned();
        let agent = self.resolve_agent(requested_agent_id.as_deref()).await.ok_or_else(|| {
            Error::Other("HarnessGateway.bind_main_agent must be called before run(...)".into())
        })?;

        let session_id = self.resolve_session_id(&gate_key).await;
        if let Some(addr) = &outbound {
            self.last_route_by_session.write().await.insert(session_id.clone(), addr.clone());
        }

        let turn_ctx = GatewayTurnContext {
            session_id: session_id.clone(),
            gate_key: gate_key.clone(),
            user_id: ctx.user_id.clone(),
            msg_context: ctx,
            outbound,
            cancel,
        };

        let _guard = self.session_turn_gate.acquire(&gate_key).await;
        agent.call(messages, &turn_ctx).await
    }
}


impl HarnessGateway {
    /// Delivers proactive outbound messages via the session's last recorded route.
    /// Returns false when no route or no channel manager is available.
    pub async fn deliver_to_session(&self, session_id: &str, messages: Vec<Msg>) -> bool {
        if messages.is_empty() {
            return false;
        }
        let Some(cm) = &self.channel_manager else {
            return false;
        };
        let target = self.last_route_by_session.read().await.get(session_id).cloned();
        match target {
            Some(addr) => {
                cm.deliver(&addr, messages).await;
                true
            }
            None => false,
        }
    }

    /// Exposes a spawned subagent as a user-addressable entry point, returning its
    /// assigned `subagent_id` handle.
    pub async fn expose_subagent(
        &self,
        agent_id: &str,
        session_id: &str,
        agent: Arc<dyn GatewayAgent>,
        reply_to: Option<OutboundAddress>,
    ) -> String {
        let subagent_id = format!("sub-{:08x}", fast_random());
        self.exposed_sessions.write().await.insert(
            subagent_id.clone(),
            ExposedSession {
                subagent_id: subagent_id.clone(),
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                agent,
                reply_to,
            },
        );
        self.subagent_registry.read().await.register(SubagentRecord::new(
            subagent_id.clone(),
            agent_id,
            session_id,
            None,
            None,
            None,
        ));
        subagent_id
    }

    /// Revokes a previously exposed subagent from the live cache and registry.
    pub async fn revoke_subagent(&self, subagent_id: &str) {
        if subagent_id.is_empty() {
            return;
        }
        self.exposed_sessions.write().await.remove(subagent_id);
        self.subagent_registry.read().await.revoke(subagent_id);
    }

    /// Installs a durable `SubagentRegistry`.
    pub async fn set_subagent_registry(&self, registry: Arc<dyn SubagentRegistry>) {
        *self.subagent_registry.write().await = registry;
    }

    /// Installs a `SubagentMaterializer` used to rebuild subagents not in the local
    /// live cache (cross-node / post-restart recovery).
    pub async fn set_subagent_materializer(&self, materializer: Arc<dyn SubagentMaterializer>) {
        *self.subagent_materializer.write().await = Some(materializer);
    }

    /// Routes messages directly to an exposed subagent session, bypassing normal
    /// binding-based routing. Returns the subagent's reply.
    pub async fn run_subagent(&self, subagent_id: &str, messages: Vec<Msg>) -> Result<Msg> {
        let session = self.resolve_exposed(subagent_id).await.ok_or_else(|| {
            Error::Other(format!("Unknown subagentId: {subagent_id}"))
        })?;
        let gate_key = format!("subagent:{subagent_id}");
        let turn_ctx = GatewayTurnContext {
            session_id: session.session_id.clone(),
            gate_key: gate_key.clone(),
            msg_context: MsgContext::default_context(),
            user_id: None,
            outbound: session.reply_to.clone(),
            cancel: None,
        };
        let _guard = self.session_turn_gate.acquire(&gate_key).await;
        let reply = session.agent.call(messages, &turn_ctx).await?;
        if let (Some(cm), Some(addr)) = (&self.channel_manager, &session.reply_to) {
            cm.deliver(addr, vec![reply.clone()]).await;
        }
        Ok(reply)
    }

    /// Whether the given session is currently inside a gated turn.
    pub async fn is_session_running(&self, session_id: &str) -> bool {
        let gate_key = self.session_to_gate_key.read().await.get(session_id).cloned();
        match gate_key {
            Some(k) => self.session_turn_gate.is_running(&k).await,
            None => false,
        }
    }

    /// Triggers a wakeup-driven run for an idle session (no user input); the agent's
    /// inbox middleware drains pending results. Returns `Ok(None)` for an unknown session.
    pub async fn run_wakeup(&self, user_id: Option<&str>, session_id: &str) -> Result<Option<Msg>> {
        let gate_key = self.session_to_gate_key.read().await.get(session_id).cloned();
        let Some(gate_key) = gate_key else {
            return Ok(None);
        };
        let agent = self.resolve_agent(None).await.ok_or_else(|| {
            Error::Other("HarnessGateway.bind_main_agent must be called before run_wakeup(...)".into())
        })?;
        let turn_ctx = GatewayTurnContext {
            session_id: session_id.to_string(),
            gate_key: gate_key.clone(),
            msg_context: MsgContext::default_context(),
            user_id: user_id.map(str::to_string),
            outbound: self.last_route_by_session.read().await.get(session_id).cloned(),
            cancel: None,
        };
        let _guard = self.session_turn_gate.acquire(&gate_key).await;
        let reply = agent.call(Vec::new(), &turn_ctx).await?;
        if let Some(bus) = &self.message_bus {
            let _ = bus
                .session_publish_event(session_id, serde_json::to_value(&reply).unwrap_or_default())
                .await;
        }
        Ok(Some(reply))
    }
}

impl HarnessGateway {
    async fn resolve_agent(&self, agent_id: Option<&str>) -> Option<Arc<dyn GatewayAgent>> {
        if let Some(id) = agent_id {
            if let Some(a) = self.agent_registry.read().await.get(id).cloned() {
                return Some(a);
            }
        }
        let def = self.main_agent.read().await.clone();
        if def.is_some() {
            return def;
        }
        let default_id = self.default_agent_id.read().await.clone();
        match default_id {
            Some(id) => self.agent_registry.read().await.get(&id).cloned(),
            None => None,
        }
    }

    async fn resolve_agent_id(&self, agent: &Arc<dyn GatewayAgent>) -> String {
        // No agent-id accessor on the gateway-facing trait; default to "main".
        // Subclasses can register under explicit ids via `register_agent`.
        let _ = agent;
        "main".to_string()
    }

    fn generate_session_id(gate_key: &str) -> String {
        format!("gw-{}", super::msg_context::deterministic_hash(&[gate_key]))
    }

    async fn resolve_session_id(&self, gate_key: &str) -> String {
        let session_id = self
            .session_map
            .write()
            .await
            .entry(gate_key.to_string())
            .or_insert_with(|| Self::generate_session_id(gate_key))
            .clone();
        self.session_to_gate_key.write().await.insert(session_id.clone(), gate_key.to_string());
        session_id
    }

    async fn resolve_exposed(&self, subagent_id: &str) -> Option<ExposedSession> {
        if let Some(live) = self.exposed_sessions.read().await.get(subagent_id).cloned() {
            return Some(live);
        }
        let materializer = self.subagent_materializer.read().await.clone()?;
        let record = self.subagent_registry.read().await.find(subagent_id)?;
        let agent = materializer.materialize(&record.agent_id, &record.session_id).await?;
        let rebuilt = ExposedSession {
            subagent_id: record.subagent_id.clone(),
            agent_id: record.agent_id,
            session_id: record.session_id,
            agent,
            reply_to: None,
        };
        let mut map = self.exposed_sessions.write().await;
        map.entry(subagent_id.to_string()).or_insert(rebuilt.clone());
        map.get(subagent_id).cloned()
    }
}

/// Cheap process-unique id seed for assigning subagent handles.
fn fast_random() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ (std::process::id() << 16)
}
