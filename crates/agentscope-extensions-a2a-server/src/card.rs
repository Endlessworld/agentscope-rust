//! A2A server-side agent card construction, mirroring
//! `io.agentscope.core.a2a.server.card` / `...server.transport`.
//!
//! These types are server-only concepts (they synthesize the card the server
//! advertises); the client-side resolvers live in
//! `agentscope-extensions-a2a-client::card`.

use agentscope_extensions_a2a_client::model::{AgentCard, AgentInterface};
/// Java `AgentScopeAgentCardConverter` constants.
pub mod card_converter {
    /// Protocol version stamped on every `supportedInterfaces` entry (A2A 1.0).
    pub const PROTOCOL_VERSION: &str = agentscope_extensions_a2a_client::model::PROTOCOL_VERSION;
    /// Default card version.
    pub const DEFAULT_VERSION: &str = "1.0.0";
    /// Default transport when none is configured.
    pub const DEFAULT_TRANSPORT: &str = "JSONRPC";
    /// Default input/output mode when none is configured.
    pub const DEFAULT_MODE: &str = "text";
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

/// Transport endpoint properties, re-exported from the client crate (2026-10-29)
/// so both server and client share one definition.
pub use agentscope_extensions_a2a_client::transport_properties::{
    TransportProperties, TransportPropertiesBuilder,
};

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
/// * `capabilities`: always `streaming = true`, `pushNotifications = false` (Java also pins
///   `extendedAgentCard = false`; the shared DTO has no such field yet, so 1.0 clients read the
///   capability as absent, i.e. `false`). `stateTransitionHistory` is a 0.3 flag and is no longer
///   advertised (Java's 1.0 converter sets only the three flags above).
/// * `supportedInterfaces`: the preferred interface first, then the others (A2A 1.0 wire shape);
///   each entry carries `protocolBinding` and `protocolVersion: 1.0`. The card keeps the preferred
///   interface in `url` / `preferredTransport` and the others in `additionalInterfaces`; see
///   [`AgentCard`]'s `Serialize` impl for the 1.0 rendering.
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
        documentation_url: config.documentation_url.clone(),
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
        protocol_version: None,
        provider: config.provider.clone(),
        preferred_transport: preferred.map(|i| i.transport),
        additional_interfaces: Some(additional_interfaces),
        icon_url: config.icon_url.clone(),
        capabilities: agentscope_extensions_a2a_client::model::AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            // 0.3-only flag: A2A 1.0 dropped it in favour of `extendedAgentCard`.
            state_transition_history: None,
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
        assert_eq!(card.protocol_version, None);
        assert_eq!(card.preferred_transport.as_deref(), Some("JSONRPC"));
        assert_eq!(card.url, "http://localhost:8080");
        assert_eq!(card.default_input_modes, vec!["text".to_string()]);
        assert_eq!(card.capabilities.streaming, Some(true));
        assert_eq!(card.capabilities.push_notifications, Some(false));
        // A2A 1.0 dropped `stateTransitionHistory`; only the three 1.0 flags are advertised.
        assert_eq!(card.capabilities.state_transition_history, None);
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

    #[test]
    fn create_agent_card_serializes_the_a2a_1_0_shape() {
        let config = ConfigurableAgentCard::builder()
            .security(vec![serde_json::Map::from_iter([(
                "apiKey".to_string(),
                serde_json::json!(["read"]),
            )])])
            .build();
        let transports = vec![
            TransportProperties::builder("JSONRPC")
                .host("localhost")
                .port(8080)
                .build(),
            TransportProperties::builder("GRPC")
                .host("localhost")
                .port(9090)
                .build(),
        ];

        let card = create_agent_card(&config, &descriptor(), &transports);
        let json = serde_json::to_value(&card).unwrap();

        let interfaces = json["supportedInterfaces"].as_array().unwrap();
        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces[0]["protocolBinding"], "JSONRPC");
        assert_eq!(interfaces[0]["url"], "http://localhost:8080");
        assert_eq!(interfaces[0]["protocolVersion"], "1.0");
        assert_eq!(interfaces[1]["protocolBinding"], "GRPC");
        assert_eq!(interfaces[1]["url"], "http://localhost:9090");
        assert_eq!(json["securityRequirements"][0]["apiKey"][0], "read");
        assert_eq!(json["capabilities"]["pushNotifications"], false);
        assert_eq!(json["capabilities"]["streaming"], true);
        assert!(json["capabilities"].get("stateTransitionHistory").is_none());

        // The 0.3 discovery keys must not be emitted any more.
        for legacy in [
            "url",
            "preferredTransport",
            "additionalInterfaces",
            "protocolVersion",
            "security",
        ] {
            assert!(json.get(legacy).is_none(), "unexpected 0.3 key {legacy}");
        }
    }
}
