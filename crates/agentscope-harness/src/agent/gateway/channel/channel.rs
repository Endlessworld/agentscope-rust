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

use crate::agent::gateway::harness_gateway::{HarnessGateway, OutboundAddress};

use super::config::ChannelConfig;
use super::inbound::InboundMessage;

/// Receipt returned by a receipt-aware delivery.
///
/// Mirrors the return value of Java `Channel.deliverWithReceipt`, which hands the
/// host a provider-side identifier (`message_id`) plus the raw provider payload. A
/// host that runs a durable outbound worker uses the id to de-duplicate a retry
/// without having to know each platform's response shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeliveryReceipt {
    /// Provider-assigned message id, when the platform returns one.
    pub message_id: Option<String>,
    /// Raw provider payload, for hosts that need more than the id.
    pub raw: serde_json::Value,
}

impl DeliveryReceipt {
    /// Creates a receipt carrying only a provider message id.
    pub fn with_message_id(message_id: impl Into<String>) -> Self {
        Self {
            message_id: Some(message_id.into()),
            raw: serde_json::Value::Null,
        }
    }
}

/// The error every default `deliver_with_receipt` implementation returns.
///
/// The abstract layer cannot know whether a given transport can report a receipt, so
/// the default is **fail-closed**: the caller sees an explicit unsupported error
/// instead of a fabricated receipt.
pub fn unsupported_delivery_receipt(channel_id: &str) -> agentscope_core::error::Error {
    agentscope_core::error::Error::Other(format!(
        "deliver_with_receipt is not supported by channel '{channel_id}' (unsupported)"
    ))
}

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

    /// Delivers one message and returns the provider's delivery receipt.
    ///
    /// Mirrors Java `Channel.deliverWithReceipt(address, msg, deliveryId)`. Channels
    /// whose transport can report a provider message id override this; every other
    /// channel keeps the default, which returns an explicit *unsupported* error so a
    /// durable-delivery worker never mistakes "not implemented" for "delivered".
    async fn deliver_with_receipt(
        &self,
        _address: &OutboundAddress,
        _message: &Msg,
        _delivery_id: Option<&str>,
    ) -> Result<DeliveryReceipt> {
        Err(unsupported_delivery_receipt(self.channel_id()))
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
