//! A2A server-side agent card construction, mirroring
//! `io.agentscope.core.a2a.server.card` / `...server.transport`.
//!
//! These types are server-only concepts (they synthesize the card the server
//! advertises); the client-side resolvers live in
//! `agentscope-extensions-a2a-client::card`.

use agentscope_extensions_a2a_client::model::{AgentCard, AgentInterface};
/// Java `AgentScopeAgentCardConverter` constants.
pub mod card_converter {
    /// Protocol version advertised by the AgentScope A2A server.
    pub const PROTOCOL_VERSION: &str = "0.3.0";
    /// Default card version.
    pub const DEFAULT_VERSION: &str = "1.0.0";
    /// Default transport when none is configured.
    pub const DEFAULT_TRANSPORT: &str = "JSONRPC";
    /// Default input/output mode when none is configured.
    pub const DEFAULT_MODE: &str = "text";
    pub(crate) const HTTPS: &str = "https";
    pub(crate) const HTTP: &str = "http";
}

/// Agent card attributes that an operator may override.
///
/// Mirrors Java `io.agentscope.core.a2a.server.card.ConfigurableAgentCard`. All fields are
/// optional: the converter falls back to server-derived defaults for the missing ones.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurableAgentCard {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<agentscope_extensions_a2a_client::model::AgentProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_input_modes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_output_modes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<agentscope_extensions_a2a_client::model::AgentSkill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_interfaces: Option<Vec<AgentInterface>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_transport: Option<String>,
}

impl ConfigurableAgentCard {
    /// Returns the Java-style fluent builder.
    pub fn builder() -> ConfigurableAgentCardBuilder {
        ConfigurableAgentCardBuilder::default()
    }
}

/// Builder for [`ConfigurableAgentCard`]; every setter mirrors a Java `Builder` method.
#[derive(Debug, Default, Clone)]
pub struct ConfigurableAgentCardBuilder {
    card: ConfigurableAgentCard,
}

impl ConfigurableAgentCardBuilder {
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.card.name = Some(name.into());
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.card.description = Some(description.into());
        self
    }

    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.card.url = Some(url.into());
        self
    }

    pub fn provider(
        mut self,
        provider: agentscope_extensions_a2a_client::model::AgentProvider,
    ) -> Self {
        self.card.provider = Some(provider);
        self
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.card.version = Some(version.into());
        self
    }

    pub fn documentation_url(mut self, url: impl Into<String>) -> Self {
        self.card.documentation_url = Some(url.into());
        self
    }

    pub fn default_input_modes(mut self, modes: Vec<String>) -> Self {
        self.card.default_input_modes = Some(modes);
        self
    }

    pub fn default_output_modes(mut self, modes: Vec<String>) -> Self {
        self.card.default_output_modes = Some(modes);
        self
    }

    pub fn skills(
        mut self,
        skills: Vec<agentscope_extensions_a2a_client::model::AgentSkill>,
    ) -> Self {
        self.card.skills = Some(skills);
        self
    }

    pub fn security_schemes(mut self, schemes: serde_json::Map<String, serde_json::Value>) -> Self {
        self.card.security_schemes = Some(schemes);
        self
    }

    pub fn security(mut self, security: Vec<serde_json::Map<String, serde_json::Value>>) -> Self {
        self.card.security = Some(security);
        self
    }

    pub fn icon_url(mut self, icon_url: impl Into<String>) -> Self {
        self.card.icon_url = Some(icon_url.into());
        self
    }

    pub fn additional_interfaces(mut self, interfaces: Vec<AgentInterface>) -> Self {
        self.card.additional_interfaces = Some(interfaces);
        self
    }

    pub fn preferred_transport(mut self, transport: impl Into<String>) -> Self {
        self.card.preferred_transport = Some(transport.into());
        self
    }

    pub fn build(self) -> ConfigurableAgentCard {
        self.card
    }
}

/// Transport endpoint properties used to synthesize [`AgentInterface`] entries.
///
/// Mirrors Java `io.agentscope.core.a2a.server.transport.TransportProperties`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportProperties {
    pub transport_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub support_tls: bool,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl TransportProperties {
    /// Returns the Java-style fluent builder for `transport_type`.
    pub fn builder(transport_type: impl Into<String>) -> TransportPropertiesBuilder {
        TransportPropertiesBuilder::new(transport_type)
    }

    /// The effective port: the configured one, else the scheme default (443/80).
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.support_tls { 443 } else { 80 })
    }

    /// The path with a leading `/` when non-empty, mirroring Java `getPath`.
    pub fn normalized_path(&self) -> String {
        let path = self.path.as_deref().unwrap_or("");
        if path.is_empty() {
            String::new()
        } else if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    }

    /// Renders `%s://%s:%d%s`, mirroring Java `AgentScopeAgentCardConverter#createAdditionalInterface`.
    pub fn to_url(&self) -> String {
        let scheme = if self.support_tls {
            card_converter::HTTPS
        } else {
            card_converter::HTTP
        };
        format!(
            "{}://{}:{}{}",
            scheme,
            self.host.as_deref().unwrap_or_default(),
            self.effective_port(),
            self.normalized_path()
        )
    }

    /// Converts this transport into an [`AgentInterface`].
    pub fn to_agent_interface(&self) -> AgentInterface {
        AgentInterface {
            transport: self.transport_type.clone(),
            url: self.to_url(),
        }
    }
}

/// Builder for [`TransportProperties`].
#[derive(Debug, Clone)]
pub struct TransportPropertiesBuilder {
    transport_type: String,
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    support_tls: bool,
    extra: serde_json::Map<String, serde_json::Value>,
}

impl TransportPropertiesBuilder {
    fn new(transport_type: impl Into<String>) -> Self {
        Self {
            transport_type: transport_type.into(),
            host: None,
            port: None,
            path: None,
            support_tls: false,
            extra: serde_json::Map::new(),
        }
    }

    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn support_tls(mut self, support_tls: bool) -> Self {
        self.support_tls = support_tls;
        self
    }

    pub fn extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    pub fn build(self) -> TransportProperties {
        TransportProperties {
            transport_type: self.transport_type,
            host: self.host,
            port: self.port,
            path: self.path,
            support_tls: self.support_tls,
            extra: self.extra,
        }
    }
}

/// Identity of the agent being exposed, used for card defaults.
///
/// Mirrors the two `AgentRunner` accessors that
/// `AgentScopeAgentCardConverter` consults (`getAgentName` / `getAgentDescription`).
pub trait AgentDescriptor: Send + Sync {
    fn agent_name(&self) -> String;
    fn agent_description(&self) -> String;
}

/// Converts a [`ConfigurableAgentCard`] plus available transports into a wire [`AgentCard`].
///
/// Mirrors Java `io.agentscope.core.a2a.server.card.AgentScopeAgentCardConverter`. Build rules:
///
/// * `capabilities`: always `streaming = true`, `pushNotifications = false`,
///   `stateTransitionHistory = false` (not configurable).
/// * `protocolVersion`: always [`card_converter::PROTOCOL_VERSION`].
/// * `supportsAuthenticatedExtendedCard`: always `false`.
/// * `version`: configured value, else `1.0.0`.
/// * `defaultInputModes` / `defaultOutputModes`: configured value, else `["text"]`.
/// * `additionalInterfaces`: configured value, else one interface per available transport.
/// * `preferredTransport` / `url`: configured pair when both present, else the `JSONRPC`
///   transport, else the first available transport.
pub fn create_agent_card(
    config: &ConfigurableAgentCard,
    descriptor: &dyn AgentDescriptor,
    available_transports: &[TransportProperties],
) -> AgentCard {
    let additional_interfaces = config.additional_interfaces.clone().unwrap_or_else(|| {
        available_transports
            .iter()
            .map(TransportProperties::to_agent_interface)
            .collect()
    });

    let preferred = preferred_transport(config, &additional_interfaces);

    AgentCard {
        name: config
            .name
            .clone()
            .unwrap_or_else(|| descriptor.agent_name()),
        description: Some(
            config
                .description
                .clone()
                .unwrap_or_else(|| descriptor.agent_description()),
        ),
        url: preferred
            .as_ref()
            .map(|i| i.url.clone())
            .unwrap_or_default(),
        version: Some(
            config
                .version
                .clone()
                .unwrap_or_else(|| card_converter::DEFAULT_VERSION.to_string()),
        ),
        protocol_version: Some(card_converter::PROTOCOL_VERSION.to_string()),
        provider: config.provider.clone(),
        preferred_transport: preferred.map(|i| i.transport),
        additional_interfaces: Some(additional_interfaces),
        icon_url: config.icon_url.clone(),
        capabilities: agentscope_extensions_a2a_client::model::AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            state_transition_history: Some(false),
            extensions: None,
        },
        default_input_modes: modes(&config.default_input_modes),
        default_output_modes: modes(&config.default_output_modes),
        skills: config.skills.clone().unwrap_or_default(),
        security_schemes: config.security_schemes.clone(),
        security: config.security.clone(),
        supports_authenticated_extended_card: Some(false),
        signatures: None,
    }
}

/// Java `getPreferredTransport`: configured pair wins, then `JSONRPC`, then any.
fn preferred_transport(
    config: &ConfigurableAgentCard,
    available: &[AgentInterface],
) -> Option<AgentInterface> {
    if let (Some(transport), Some(url)) = (&config.preferred_transport, &config.url) {
        return Some(AgentInterface {
            transport: transport.clone(),
            url: url.clone(),
        });
    }
    available
        .iter()
        .find(|i| i.transport == card_converter::DEFAULT_TRANSPORT)
        .or_else(|| available.first())
        .cloned()
}

/// Java `getModes`: configured value, else `["text"]`.
fn modes(configured: &Option<Vec<String>>) -> Vec<String> {
    configured
        .clone()
        .unwrap_or_else(|| vec![card_converter::DEFAULT_MODE.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_extensions_a2a_client::model::AgentSkill;

    fn descriptor() -> impl AgentDescriptor {
        struct D;
        impl AgentDescriptor for D {
            fn agent_name(&self) -> String {
                "runner-agent".to_string()
            }
            fn agent_description(&self) -> String {
                "runner description".to_string()
            }
        }
        D
    }

    #[test]
    fn transport_properties_renders_url() {
        let https = TransportProperties::builder("JSONRPC")
            .host("agent.example.com")
            .path("a2a")
            .support_tls(true)
            .build();
        assert_eq!(https.effective_port(), 443);
        assert_eq!(https.normalized_path(), "/a2a");
        assert_eq!(https.to_url(), "https://agent.example.com:443/a2a");

        let http = TransportProperties::builder("JSONRPC")
            .host("localhost")
            .port(8080)
            .build();
        assert_eq!(http.effective_port(), 8080);
        assert_eq!(http.normalized_path(), "");
        assert_eq!(http.to_url(), "http://localhost:8080");
    }

    #[test]
    fn create_agent_card_defaults_follow_java_rules() {
        let config = ConfigurableAgentCard::default();
        let transports = vec![TransportProperties::builder("JSONRPC")
            .host("localhost")
            .port(8080)
            .build()];
        let card = create_agent_card(&config, &descriptor(), &transports);

        assert_eq!(card.name, "runner-agent");
        assert_eq!(card.description.as_deref(), Some("runner description"));
        assert_eq!(card.version.as_deref(), Some("1.0.0"));
        assert_eq!(card.protocol_version.as_deref(), Some("0.3.0"));
        assert_eq!(card.preferred_transport.as_deref(), Some("JSONRPC"));
        assert_eq!(card.url, "http://localhost:8080");
        assert_eq!(card.default_input_modes, vec!["text".to_string()]);
        assert_eq!(card.capabilities.streaming, Some(true));
        assert_eq!(card.capabilities.push_notifications, Some(false));
        assert_eq!(card.supports_authenticated_extended_card, Some(false));
        assert!(card.skills.is_empty());
        assert_eq!(card.additional_interfaces.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn create_agent_card_prefers_configured_pair_and_skills() {
        let config = ConfigurableAgentCard::builder()
            .name("configured")
            .url("https://custom/a2a")
            .preferred_transport("GRPC")
            .default_input_modes(vec!["text/plain".to_string()])
            .skills(vec![AgentSkill {
                id: "s1".to_string(),
                name: "skill one".to_string(),
                description: None,
                tags: None,
                examples: None,
                input_modes: None,
                output_modes: None,
            }])
            .build();

        let card = create_agent_card(&config, &descriptor(), &[]);
        assert_eq!(card.name, "configured");
        assert_eq!(card.url, "https://custom/a2a");
        assert_eq!(card.preferred_transport.as_deref(), Some("GRPC"));
        assert_eq!(card.default_input_modes, vec!["text/plain".to_string()]);
        assert_eq!(card.default_output_modes, vec!["text".to_string()]);
        assert_eq!(card.skills.len(), 1);
    }
}
