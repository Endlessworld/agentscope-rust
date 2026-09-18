//! Data model for the Nacos A2A extension.
//!
//! Two card shapes are involved:
//! * [`A2aAgentCard`] — the crate-local A2A-specification card (mirrors
//!   `io.a2a.spec.AgentCard`). Kept here because the `agentscope-extensions-protocol`
//!   A2A model is not yet dependency-ready (see tracker §4.2); when it lands, this
//!   type should be replaced by a re-export.
//! * [`NacosAgentCard`] — the Nacos-side card (mirrors
//!   `com.alibaba.nacos.api.ai.model.a2a.AgentCard`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A2A-spec capabilities, mirroring `io.a2a.spec.AgentCapabilities`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct A2aAgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(rename = "pushNotifications", skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,
    #[serde(rename = "stateTransitionHistory", skip_serializing_if = "Option::is_none")]
    pub state_transition_history: Option<bool>,
}

/// A2A-spec agent provider, mirroring `io.a2a.spec.AgentProvider`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aAgentProvider {
    pub organization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A2A-spec transport interface, mirroring `io.a2a.spec.AgentInterface`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aAgentInterface {
    pub url: String,
    pub transport: String,
}

/// A2A-spec skill, mirroring `io.a2a.spec.AgentSkill`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aAgentSkill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub examples: Option<Vec<String>>,
    #[serde(rename = "inputModes", skip_serializing_if = "Option::is_none")]
    pub input_modes: Option<Vec<String>>,
    #[serde(rename = "outputModes", skip_serializing_if = "Option::is_none")]
    pub output_modes: Option<Vec<String>>,
}

/// A2A-spec agent card, mirroring `io.a2a.spec.AgentCard` (the subset Nacos round-trips).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aAgentCard {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "protocolVersion", skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<A2aAgentProvider>,
    #[serde(rename = "preferredTransport", skip_serializing_if = "Option::is_none")]
    pub preferred_transport: Option<String>,
    #[serde(rename = "additionalInterfaces", skip_serializing_if = "Option::is_none")]
    pub additional_interfaces: Option<Vec<A2aAgentInterface>>,
    #[serde(rename = "iconUrl", skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub capabilities: A2aAgentCapabilities,
    #[serde(rename = "defaultInputModes")]
    pub default_input_modes: Vec<String>,
    #[serde(rename = "defaultOutputModes")]
    pub default_output_modes: Vec<String>,
    pub skills: Vec<A2aAgentSkill>,
    #[serde(rename = "securitySchemes", skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<serde_json::Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<serde_json::Map<String, Value>>>,
    #[serde(
        rename = "supportsAuthenticatedExtendedCard",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_authenticated_extended_card: Option<bool>,
    #[serde(rename = "documentationUrl", skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

impl A2aAgentCard {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            url: url.into(),
            version: None,
            protocol_version: None,
            provider: None,
            preferred_transport: Some("JSONRPC".to_string()),
            additional_interfaces: None,
            icon_url: None,
            capabilities: A2aAgentCapabilities::default(),
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: Vec::new(),
            security_schemes: None,
            security: None,
            supports_authenticated_extended_card: None,
            documentation_url: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Nacos-side model
// ---------------------------------------------------------------------------

/// Nacos capabilities, mirroring `com.alibaba.nacos.api.ai.model.a2a.AgentCapabilities`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NacosAgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(rename = "pushNotifications", skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,
    #[serde(rename = "stateTransitionHistory", skip_serializing_if = "Option::is_none")]
    pub state_transition_history: Option<bool>,
}

/// Nacos agent provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NacosAgentProvider {
    pub organization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Nacos transport interface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NacosAgentInterface {
    pub url: String,
    pub transport: String,
}

/// Nacos agent skill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NacosAgentSkill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub examples: Option<Vec<String>>,
    #[serde(rename = "inputModes", skip_serializing_if = "Option::is_none")]
    pub input_modes: Option<Vec<String>>,
    #[serde(rename = "outputModes", skip_serializing_if = "Option::is_none")]
    pub output_modes: Option<Vec<String>>,
}

/// Nacos-side agent card, mirroring `com.alibaba.nacos.api.ai.model.a2a.AgentCard`.
///
/// `securitySchemes` is kept as opaque JSON (Nacos does not retain the concrete
/// A2A subtype — see Java `AgentCardConverterUtil`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NacosAgentCard {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "protocolVersion", skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<NacosAgentProvider>,
    #[serde(rename = "preferredTransport", skip_serializing_if = "Option::is_none")]
    pub preferred_transport: Option<String>,
    #[serde(rename = "additionalInterfaces", skip_serializing_if = "Option::is_none")]
    pub additional_interfaces: Option<Vec<NacosAgentInterface>>,
    #[serde(rename = "iconUrl", skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub capabilities: NacosAgentCapabilities,
    #[serde(rename = "defaultInputModes")]
    pub default_input_modes: Vec<String>,
    #[serde(rename = "defaultOutputModes")]
    pub default_output_modes: Vec<String>,
    pub skills: Vec<NacosAgentSkill>,
    #[serde(rename = "securitySchemes", skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<serde_json::Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<serde_json::Map<String, Value>>>,
    #[serde(
        rename = "supportsAuthenticatedExtendedCard",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_authenticated_extended_card: Option<bool>,
    #[serde(rename = "documentationUrl", skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

/// A registered Nacos A2A endpoint, mirroring
/// `com.alibaba.nacos.api.ai.model.a2a.AgentEndpoint`.
///
/// The Java `NacosA2aRegistry#buildAgentEndpoint` fills this from a
/// [`super::registry::NacosA2aTransportProperties`] and binds the owning card's
/// `version`, so the endpoint is registered under the correct card revision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NacosAgentEndpoint {
    pub transport: String,
    #[serde(rename = "address", skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub port: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "supportTls")]
    pub support_tls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

impl NacosAgentEndpoint {
    /// Builds an endpoint from transport properties and the owning card version,
    /// mirroring `NacosA2aRegistry#buildAgentEndpoint`.
    pub fn from_transport(
        transport: &super::registry::NacosA2aTransportProperties,
        version: Option<&str>,
    ) -> Self {
        Self {
            transport: transport.transport.clone(),
            address: Some(transport.host.clone()),
            port: transport.port,
            path: transport.path.clone(),
            support_tls: transport.support_tls,
            version: version.map(|v| v.to_string()),
            protocol: transport.protocol.clone(),
            query: transport.query.clone(),
        }
    }
}

impl NacosAgentCard {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            url: url.into(),
            version: None,
            protocol_version: None,
            provider: None,
            preferred_transport: Some("JSONRPC".to_string()),
            additional_interfaces: None,
            icon_url: None,
            capabilities: NacosAgentCapabilities::default(),
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: Vec::new(),
            security_schemes: None,
            security: None,
            supports_authenticated_extended_card: None,
            documentation_url: None,
        }
    }
}
