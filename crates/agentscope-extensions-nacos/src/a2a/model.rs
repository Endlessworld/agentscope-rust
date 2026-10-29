//! Data model for the Nacos A2A extension.
//!
//! Two card shapes are involved:
//! * [`A2aAgentCard`] — the A2A-specification card. Its payload is the protocol
//!   crate's [`AgentCard`](agentscope_extensions_a2a_client::model::AgentCard);
//!   the wrapper only adds `documentationUrl`, which the protocol model does not
//!   declare yet (tracker §4.2).
//! * [`NacosAgentCard`] — the Nacos-side card (mirrors
//!   `com.alibaba.nacos.api.ai.model.a2a.AgentCard`).
//!
//! The A2A sub-types (`AgentCapabilities` / `AgentProvider` / `AgentInterface` /
//! `AgentSkill`) are **re-exported from `agentscope-extensions-a2a-client`** —
//! this crate no longer declares parallel copies. The historical names are kept
//! as aliases so the public API of this crate stays source-compatible.

use agentscope_extensions_a2a_client::model::AgentCard;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Reused A2A-spec model (protocol crate)
// ---------------------------------------------------------------------------

pub use agentscope_extensions_a2a_client::model::{
    AgentCapabilities as A2aAgentCapabilities, AgentInterface as A2aAgentInterface,
    AgentProvider as A2aAgentProvider, AgentSkill as A2aAgentSkill,
};

/// A2A-specification agent card, mirroring `io.a2a.spec.AgentCard`.
///
/// The card payload is the protocol [`AgentCard`] (reused, not re-declared);
/// field access is forwarded through `Deref`/`DerefMut`, so `card.url`,
/// `card.capabilities`, `card.skills`, … keep working.
///
/// `documentationUrl` is carried by the protocol [`AgentCard`] itself since
/// 2026-10-29, so the wrapper no longer declares a local field and simply
/// forwards it through `Deref`/`DerefMut`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aAgentCard {
    #[serde(flatten)]
    pub inner: AgentCard,
}

impl std::ops::Deref for A2aAgentCard {
    type Target = AgentCard;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for A2aAgentCard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl A2aAgentCard {
    /// A minimal card with the protocol defaults (`preferredTransport: JSONRPC`,
    /// `text/plain` input/output modes).
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            inner: AgentCard::new(name, url),
        }
    }

    /// The `documentationUrl` property.
    pub fn documentation_url(&self) -> Option<&str> {
        self.inner.documentation_url.as_deref()
    }

    /// Sets the `documentationUrl` property.
    pub fn set_documentation_url(&mut self, url: impl Into<String>) {
        self.inner.documentation_url = Some(url.into());
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
    #[serde(
        rename = "stateTransitionHistory",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_transition_history: Option<bool>,
}

/// Nacos agent provider.
///
/// Unlike the A2A model, the Nacos `url` is optional (`AgentProvider#setUrl(null)`).
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
/// `securitySchemes` is stored as JSON maps in the A2A 0.3 shape (see
/// [`super::security_scheme`]); typed A2A schemes are their `type`-dispatched view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NacosAgentCard {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "protocolVersion", skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<NacosAgentProvider>,
    #[serde(rename = "preferredTransport", skip_serializing_if = "Option::is_none")]
    pub preferred_transport: Option<String>,
    #[serde(
        rename = "additionalInterfaces",
        skip_serializing_if = "Option::is_none"
    )]
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

impl NacosAgentCard {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            url: Some(url.into()),
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
