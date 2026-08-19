//! Agent abstraction traits, mirroring `io.agentscope.core.agent.Agent`,
//! `CallableAgent` and `ObservableAgent`.
use async_trait::async_trait;

use crate::error::Result;
use crate::event::AgentEvent;
use crate::formatter::JsonSchema;
use crate::message::Msg;
use crate::state::AgentState;
use crate::tool::Toolkit;

/// A named, stateful agent that can be interrupted and inspected.
pub trait Agent: Send + Sync {
    /// The stable unique identifier of this agent.
    fn agent_id(&self) -> &str;
    /// The display name of this agent.
    fn name(&self) -> &str;
    /// A human-readable description of this agent.
    fn description(&self) -> &str;
    /// Requests an interrupt of the agent's current execution (if any).
    fn interrupt(&self);
    /// Returns a snapshot of the agent's current mutable state.
    fn agent_state(&self) -> AgentState;
    /// Returns the toolkit bound to this agent.
    fn toolkit(&self) -> &Toolkit;
}

/// An agent that can be invoked with messages to produce a reply.
#[async_trait]
pub trait CallableAgent: Agent {
    /// Runs the agent against the given input messages and returns the reply.
    async fn call(&self, messages: Vec<Msg>) -> Result<Msg>;

    /// Structured output: runs the agent with the given JSON schema and stores the
    /// parsed JSON reply in the returned message's structured-output metadata.
    async fn call_with_schema(&self, messages: Vec<Msg>, schema: JsonSchema) -> Result<Msg>;

    /// Convenience: call with a single message.
    async fn call_single(&self, msg: Msg) -> Result<Msg> {
        self.call(vec![msg]).await
    }

    /// Convenience: call with a plain-text user message.
    async fn call_text(&self, text: String) -> Result<Msg> {
        self.call(vec![Msg::user(text)]).await
    }
}

/// An agent whose execution events can be observed by external subscribers.
pub trait ObservableAgent: Agent {
    /// Registers a sink that receives every event emitted during execution.
    fn subscribe(&self, sink: Box<dyn Fn(AgentEvent) + Send + Sync>);
}
