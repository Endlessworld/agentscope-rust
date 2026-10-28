//! Nacos A2A support, mirroring `io.agentscope.core.nacos.a2a`.
//!
//! Sub-modules:
//! * [`model`] — Nacos-side AgentCard DTOs plus the A2A-spec card, which reuses the
//!   protocol crate's model types.
//! * [`converter`] — bidirectional conversion between the two card models
//!   (supported-interface assembly/splitting included).
//! * [`security_scheme`] — `type`-dispatched security scheme conversion.
//! * [`registry`] — publish AgentCards / endpoints to Nacos.
//! * [`discovery`] — resolve (and cache) AgentCards from a Nacos registry.

pub mod converter;
pub mod discovery;
pub mod model;
pub mod registry;
pub mod security_scheme;

pub use converter::{
    apply_supported_interfaces, convert_to_a2a_agent_card, convert_to_nacos_agent_card,
    nacos_supported_interfaces, supported_interfaces, NacosAgentCardConverter,
    DEFAULT_PREFERRED_TRANSPORT,
};
pub use discovery::{AgentCardResolver, NacosAgentCardResolver};
pub use model::{
    A2aAgentCapabilities, A2aAgentCard, A2aAgentInterface, A2aAgentProvider, A2aAgentSkill,
    NacosAgentCapabilities, NacosAgentCard, NacosAgentEndpoint, NacosAgentInterface,
    NacosAgentProvider, NacosAgentSkill,
};
pub use registry::{
    AgentRegistry, NacosA2aRegistry, NacosA2aRegistryProperties, NacosA2aRegistryPropertiesBuilder,
    NacosA2aTransportProperties, NacosA2aTransportPropertiesBuilder,
    NacosA2aTransportPropertiesEnvParser, NacosAgentRegistry,
};
pub use security_scheme::{convert_to_a2a_security_schemes, convert_to_nacos_security_schemes};
