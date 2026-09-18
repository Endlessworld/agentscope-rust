//! Nacos A2A support, mirroring `io.agentscope.core.nacos.a2a`.
//!
//! Sub-modules:
//! * [`model`] — Nacos-side AgentCard DTOs and the crate-local A2A-spec card model.
//! * [`converter`] — bidirectional conversion between the two card models.
//! * [`registry`] — publish AgentCards / endpoints to Nacos.
//! * [`discovery`] — resolve (and cache) AgentCards from a Nacos registry.

pub mod converter;
pub mod discovery;
pub mod model;
pub mod registry;

pub use converter::NacosAgentCardConverter;
pub use discovery::{AgentCardResolver, NacosAgentCardResolver};
pub use model::{A2aAgentCard, NacosAgentCard, NacosAgentEndpoint};
pub use registry::{
    AgentRegistry, NacosA2aRegistry, NacosA2aRegistryProperties, NacosA2aRegistryPropertiesBuilder,
    NacosA2aTransportProperties, NacosA2aTransportPropertiesBuilder,
    NacosA2aTransportPropertiesEnvParser, NacosAgentRegistry,
};
