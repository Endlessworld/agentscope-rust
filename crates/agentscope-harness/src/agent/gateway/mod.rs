//! Gateway: routing, session turn-gating, subagent exposure, wakeup dispatch and
//! outbound channel management, mirroring `io.agentscope.harness.agent.gateway`.
//!
//! - `MsgContext` / `deterministic_hash` — routing context and session-id derivation
//! - `SessionTurnGate` — fair per-session mutual exclusion
//! - `SubagentRecord` / `SubagentRegistry` / `InMemorySubagentRegistry` — durable
//!   exposure records
//! - `HarnessGateway` — default gateway: multi-agent registry, stable session mapping,
//!   gated turns, expose/revoke/route subagents, wakeup runs
//! - `WakeupDispatcher` — wakes idle sessions when background work completes
//! - `GatewayBootstrap` — multi-agent + channel wiring entry point

pub mod channel;
pub mod gateway_bootstrap;
pub mod harness_gateway;
pub mod msg_context;
pub mod platform_channel;
pub mod session_turn_gate;
pub mod subagent;
pub mod subagent_bridge;
pub mod task_bridge;
pub mod wakeup_dispatcher;

// ---- Legacy event-emit channel/gateway (kept for backward compatibility) ----
use agentscope_core::error::{Error, Result};
use tokio::sync::mpsc;

/// A capacity-bounded channel for streaming session events as JSON values.
pub struct Channel {
    tx: mpsc::Sender<serde_json::Value>,
}

impl Channel {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<serde_json::Value>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    pub async fn send(&self, value: serde_json::Value) -> Result<()> {
        self.tx
            .send(value)
            .await
            .map_err(|_| Error::Other("channel closed".into()))
    }
}

/// A gateway that emits serialized agent/session events into a channel.
#[derive(Default)]
pub struct Gateway {
    channel: Option<Channel>,
}

impl Gateway {
    pub fn new(channel: Channel) -> Self {
        Self {
            channel: Some(channel),
        }
    }

    pub async fn emit(&self, value: serde_json::Value) -> Result<()> {
        match &self.channel {
            Some(c) => c.send(value).await,
            None => Ok(()),
        }
    }
}

pub use channel::{
    ChannelBinding, ChannelConfig, ChannelRouter, ChatUiChannel, ChatUiRequest, DmScope,
    InboundMessage, OutboundEnvelope, Peer, PeerKind, RouteResult, SendOptions,
};
pub use gateway_bootstrap::{GatewayBootstrap, GatewayBootstrapBuilder};
pub use harness_gateway::{
    ChannelManager, ChannelSink, GatewayAgent, GatewayTurnContext, HarnessGateway, OutboundAddress,
    SubagentMaterializer,
};
pub use msg_context::{deterministic_hash, MsgContext};
pub use session_turn_gate::{SessionTurnGate, TurnBusyException};
pub use subagent::{
    InMemorySubagentRegistry, StoreBackedSubagentRegistry, SubagentRecord, SubagentRegistry,
};
pub use subagent_bridge::{ExposeResult, GatewaySubagentBridge, SubagentGatewayBridge};
pub use task_bridge::{
    create_completion_callback, wire_task_completion, CallableAgentBridge,
    DefaultSubagentMaterializer, TaskCompletionBridge,
};
pub use wakeup_dispatcher::{WakeupDispatcher, WakeupTarget};
// Shared platform-channel plumbing (inbound dispatch + outbound delivery) reused by the
// `agentscope-extensions-channel` provider adapters. See `platform_channel` module docs.
pub use platform_channel::{PlatformChannel, PlatformCtx};
