//! Multi-agent + channel routing bootstrap, mirroring `GatewayBootstrap`. The primary
//! user-facing entry point for building a gateway-managed agent system.
use std::sync::Arc;

use agentscope_core::error::{Error, Result};

use crate::agent::bus::MessageBus;

use super::harness_gateway::{ChannelManager, GatewayAgent, HarnessGateway};
use super::wakeup_dispatcher::{WakeupDispatcher, WakeupTarget};

/// A fully-wired gateway bootstrap: the underlying `HarnessGateway` plus the list of
/// registered agents and the main agent id.
pub struct GatewayBootstrap {
    gateway: Arc<HarnessGateway>,
    agents: Vec<(String, Arc<dyn GatewayAgent>)>,
    main_agent_id: String,
    /// Turns queued wakeups into runs for as long as the bootstrap is alive.
    wakeup: Option<WakeupDispatcher>,
}

impl Drop for GatewayBootstrap {
    fn drop(&mut self) {
        if let Some(wakeup) = &self.wakeup {
            wakeup.close();
        }
    }
}

impl GatewayBootstrap {
    /// Returns a new builder.
    pub fn builder() -> GatewayBootstrapBuilder {
        GatewayBootstrapBuilder::default()
    }

    /// The underlying gateway (advanced usage).
    pub fn gateway(&self) -> &Arc<HarnessGateway> {
        &self.gateway
    }

    /// The registered agents keyed by id (insertion order).
    pub fn agents(&self) -> &[(String, Arc<dyn GatewayAgent>)] {
        &self.agents
    }

    /// The main agent id (routing fallback).
    pub fn main_agent_id(&self) -> &str {
        &self.main_agent_id
    }
}

/// Builder for `GatewayBootstrap`.
#[derive(Default)]
pub struct GatewayBootstrapBuilder {
    agents: Vec<(String, Arc<dyn GatewayAgent>)>,
    main_agent_id: Option<String>,
    message_bus: Option<Arc<dyn MessageBus>>,
    channel_manager: Option<Arc<ChannelManager>>,
}

impl GatewayBootstrapBuilder {
    /// Registers an agent under the given id.
    pub fn agent(mut self, id: impl Into<String>, agent: Arc<dyn GatewayAgent>) -> Self {
        self.agents.push((id.into(), agent));
        self
    }

    /// Sets the main agent id (used as the routing fallback). If not set, the first
    /// registered agent becomes the main agent.
    pub fn main_agent(mut self, id: impl Into<String>) -> Self {
        self.main_agent_id = Some(id.into());
        self
    }

    /// Sets the message bus used for wakeup-driven runs and session event publishing.
    pub fn message_bus(mut self, bus: Arc<dyn MessageBus>) -> Self {
        self.message_bus = Some(bus);
        self
    }

    /// Sets the channel manager for outbound delivery.
    pub fn channel_manager(mut self, cm: Arc<ChannelManager>) -> Self {
        self.channel_manager = Some(cm);
        self
    }

    /// Builds the gateway bootstrap. At least one agent must be registered.
    pub async fn build(self) -> Result<GatewayBootstrap> {
        if self.agents.is_empty() {
            return Err(Error::Other(
                "At least one agent must be registered via agent(...)".into(),
            ));
        }

        let resolved_main_id = self
            .main_agent_id
            .clone()
            .unwrap_or_else(|| self.agents[0].0.clone());
        if !self.agents.iter().any(|(id, _)| id == &resolved_main_id) {
            return Err(Error::Other(format!(
                "main_agent('{resolved_main_id}') not found in registered agents"
            )));
        }

        let gateway = Arc::new(HarnessGateway::create(
            self.channel_manager,
            self.message_bus.clone(),
        ));

        // A finished background task (subagent, offloaded tool) enqueues a wakeup next to its
        // inbox hint; without a dispatcher nothing consumes that queue, so an idle session
        // stays silent until the user types again. Java's bootstrap owns the same pair:
        // `new WakeupDispatcher(bus, gateway)` with the gateway as the target.
        let wakeup = self.message_bus.as_ref().map(|bus| {
            let target: Arc<dyn WakeupTarget> = gateway.clone();
            let dispatcher = WakeupDispatcher::new(Arc::clone(bus), target);
            dispatcher.start();
            dispatcher
        });

        // Register all agents, bind the main one.
        let main_agent = self
            .agents
            .iter()
            .find(|(id, _)| *id == resolved_main_id)
            .map(|(_, a)| a.clone())
            .expect("resolved main agent must exist");
        gateway.bind_main_agent(main_agent).await;

        for (id, agent) in &self.agents {
            if *id != resolved_main_id {
                gateway.register_agent(id.clone(), agent.clone()).await;
            }
        }

        Ok(GatewayBootstrap {
            gateway,
            agents: self.agents,
            main_agent_id: resolved_main_id,
            wakeup,
        })
    }
}
