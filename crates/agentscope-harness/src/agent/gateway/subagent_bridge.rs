//! Bridge between `AgentSpawnTool` and the gateway layer, mirroring
//! `io.agentscope.harness.agent.gateway.SubagentGatewayBridge`.
//!
//! Allows spawned subagents to be exposed as user-addressable entry points without
//! the spawn tool knowing about channels, routers, or the gateway implementation.

use std::sync::Arc;

use async_trait::async_trait;

use super::harness_gateway::{GatewayAgent, HarnessGateway, OutboundAddress};

/// Result of exposing a subagent to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposeResult {
    /// The user-visible handle for addressing this subagent directly.
    pub subagent_id: String,
}

impl ExposeResult {
    pub fn new(subagent_id: impl Into<String>) -> Self {
        Self {
            subagent_id: subagent_id.into(),
        }
    }
}

/// Exposes a spawned subagent as a user-addressable entry point in the gateway.
#[async_trait]
pub trait SubagentGatewayBridge: Send + Sync {
    /// Exposes a spawned subagent, returning the assigned `subagent_id` handle.
    ///
    /// `reply_to` is the outbound address for delivering replies back to the user's
    /// channel; `None` when no outbound channel context is available.
    async fn expose(
        &self,
        agent_id: &str,
        session_id: &str,
        agent: Arc<dyn GatewayAgent>,
        reply_to: Option<OutboundAddress>,
    ) -> ExposeResult;
}

/// [`SubagentGatewayBridge`] backed by a [`HarnessGateway`].
pub struct GatewaySubagentBridge {
    gateway: Arc<HarnessGateway>,
}

impl GatewaySubagentBridge {
    pub fn new(gateway: Arc<HarnessGateway>) -> Self {
        Self { gateway }
    }

    pub fn gateway(&self) -> &Arc<HarnessGateway> {
        &self.gateway
    }
}

#[async_trait]
impl SubagentGatewayBridge for GatewaySubagentBridge {
    async fn expose(
        &self,
        agent_id: &str,
        session_id: &str,
        agent: Arc<dyn GatewayAgent>,
        reply_to: Option<OutboundAddress>,
    ) -> ExposeResult {
        let subagent_id = self
            .gateway
            .expose_subagent(agent_id, session_id, agent, reply_to)
            .await;
        ExposeResult::new(subagent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness_gateway::GatewayTurnContext;
    use super::*;
    use agentscope_core::error::Result;
    use agentscope_core::message::Msg;

    struct EchoAgent;

    #[async_trait]
    impl GatewayAgent for EchoAgent {
        async fn call(&self, messages: Vec<Msg>, _ctx: &GatewayTurnContext) -> Result<Msg> {
            Ok(messages
                .into_iter()
                .next()
                .unwrap_or_else(|| Msg::assistant("empty")))
        }
    }

    #[tokio::test]
    async fn bridge_exposes_subagent_through_gateway() {
        let gateway = Arc::new(HarnessGateway::create_standalone());
        let bridge = GatewaySubagentBridge::new(gateway.clone());
        let result = bridge
            .expose("coder", "sess-1", Arc::new(EchoAgent), None)
            .await;
        assert!(!result.subagent_id.is_empty());
        // The gateway can now route directly to the exposed subagent.
        let reply = gateway
            .run_subagent(&result.subagent_id, vec![Msg::user("hi")])
            .await
            .unwrap();
        assert_eq!(reply.text_content(), "hi");
    }
}
