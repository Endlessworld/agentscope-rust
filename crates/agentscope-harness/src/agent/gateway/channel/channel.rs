//! Channel adapter trait and factory, mirroring `Channel` and `ChannelFactory`.
//!
//! A channel ingests inbound messages from one messaging platform (or a programmatic
//! caller), routes them through `ChannelRouter` to resolve the target agent and session,
//! and delegates execution to the gateway.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use async_trait::async_trait;

use crate::agent::gateway::harness_gateway::HarnessGateway;

use super::config::ChannelConfig;
use super::inbound::InboundMessage;

/// Channel adapter — ingests inbound messages, routes them through `ChannelRouter`, and
/// delegates execution to the gateway.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Logical identifier for this channel (e.g. `"chatui"`, `"slack"`). Must match
    /// `ChannelConfig::channel_id`.
    fn channel_id(&self) -> &str;

    /// Returns a snapshot of the current routing configuration.
    fn config(&self) -> ChannelConfig;

    /// Called by the gateway bootstrap before start to inject the auto-wired gateway.
    /// The default is a no-op for channels that receive the gateway at construction time.
    fn init(&self, _gateway: Arc<HarnessGateway>) {}

    /// Dispatches a fully constructed `InboundMessage` through routing and returns the
    /// agent reply.
    async fn dispatch(&self, message: InboundMessage) -> Result<Msg>;

    /// Applies a new routing config without tearing down the channel transport.
    /// Returns `true` when the swap was applied live; `false` otherwise.
    fn apply_routing_config(&self, _new_config: ChannelConfig) -> bool {
        false
    }
}

/// Constructs a `Channel` instance from the resolved routing config and provider-specific
/// properties.
pub trait ChannelFactory: Send + Sync {
    /// Build the channel, ready for `init` + start.
    fn create(
        &self,
        channel_id: String,
        routing: ChannelConfig,
        properties: &BTreeMap<String, serde_json::Value>,
    ) -> Box<dyn Channel>;
}
