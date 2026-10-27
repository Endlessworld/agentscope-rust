//! Bidirectional AgentCard conversion, mirroring
//! `io.agentscope.core.nacos.a2a.utils.AgentCardConverterUtil`.

use super::model::{
    A2aAgentCapabilities, A2aAgentCard, A2aAgentInterface, A2aAgentProvider, A2aAgentSkill,
    NacosAgentCapabilities, NacosAgentCard, NacosAgentInterface, NacosAgentProvider,
    NacosAgentSkill,
};

/// Converts between Nacos AgentCards and A2A-specification AgentCards.
pub struct NacosAgentCardConverter;

impl NacosAgentCardConverter {
    /// Converts a Nacos AgentCard to an A2A-specification AgentCard.
    ///
    /// Returns `None` when the input is `None`, mirroring the Java utility.
    pub fn to_a2a_agent_card(card: &NacosAgentCard) -> A2aAgentCard {
        A2aAgentCard {
            name: card.name.clone(),
            description: card.description.clone(),
            url: card.url.clone(),
            version: card.version.clone(),
            protocol_version: card.protocol_version.clone(),
            provider: card.provider.as_ref().map(convert_provider_to_a2a),
            preferred_transport: card.preferred_transport.clone(),
            additional_interfaces: convert_interfaces_to_a2a(card.additional_interfaces.as_deref()),
            icon_url: card.icon_url.clone(),
            capabilities: convert_capabilities_to_a2a(&card.capabilities),
            default_input_modes: card.default_input_modes.clone(),
            default_output_modes: card.default_output_modes.clone(),
            skills: card.skills.iter().map(convert_skill_to_a2a).collect(),
            security_schemes: card.security_schemes.clone(),
            security: card.security.clone(),
            supports_authenticated_extended_card: card.supports_authenticated_extended_card,
            documentation_url: card.documentation_url.clone(),
        }
    }

    /// Converts an A2A-specification AgentCard to a Nacos AgentCard.
    pub fn to_nacos_agent_card(card: &A2aAgentCard) -> NacosAgentCard {
        NacosAgentCard {
            name: card.name.clone(),
            description: card.description.clone(),
            url: card.url.clone(),
            version: card.version.clone(),
            protocol_version: card.protocol_version.clone(),
            provider: card.provider.as_ref().map(convert_provider_to_nacos),
            preferred_transport: card.preferred_transport.clone(),
            additional_interfaces: convert_interfaces_to_nacos(
                card.additional_interfaces.as_deref(),
            ),
            icon_url: card.icon_url.clone(),
            capabilities: convert_capabilities_to_nacos(&card.capabilities),
            default_input_modes: card.default_input_modes.clone(),
            default_output_modes: card.default_output_modes.clone(),
            skills: card.skills.iter().map(convert_skill_to_nacos).collect(),
            security_schemes: card.security_schemes.clone(),
            security: card.security.clone(),
            supports_authenticated_extended_card: card.supports_authenticated_extended_card,
            documentation_url: card.documentation_url.clone(),
        }
    }
}

fn convert_capabilities_to_a2a(c: &NacosAgentCapabilities) -> A2aAgentCapabilities {
    A2aAgentCapabilities {
        streaming: c.streaming,
        push_notifications: c.push_notifications,
        state_transition_history: c.state_transition_history,
    }
}

fn convert_capabilities_to_nacos(c: &A2aAgentCapabilities) -> NacosAgentCapabilities {
    NacosAgentCapabilities {
        streaming: c.streaming,
        push_notifications: c.push_notifications,
        state_transition_history: c.state_transition_history,
    }
}

fn convert_provider_to_a2a(p: &NacosAgentProvider) -> A2aAgentProvider {
    A2aAgentProvider {
        organization: p.organization.clone(),
        url: p.url.clone(),
    }
}

fn convert_provider_to_nacos(p: &A2aAgentProvider) -> NacosAgentProvider {
    NacosAgentProvider {
        organization: p.organization.clone(),
        url: p.url.clone(),
    }
}

fn convert_interfaces_to_a2a(
    ifaces: Option<&[NacosAgentInterface]>,
) -> Option<Vec<A2aAgentInterface>> {
    let ifaces = ifaces?;
    Some(
        ifaces
            .iter()
            .map(|i| A2aAgentInterface {
                url: i.url.clone(),
                transport: i.transport.clone(),
            })
            .collect(),
    )
}

fn convert_interfaces_to_nacos(
    ifaces: Option<&[A2aAgentInterface]>,
) -> Option<Vec<NacosAgentInterface>> {
    let ifaces = ifaces?;
    Some(
        ifaces
            .iter()
            .map(|i| NacosAgentInterface {
                url: i.url.clone(),
                transport: i.transport.clone(),
            })
            .collect(),
    )
}

fn convert_skill_to_a2a(s: &NacosAgentSkill) -> A2aAgentSkill {
    A2aAgentSkill {
        id: s.id.clone(),
        name: s.name.clone(),
        description: s.description.clone(),
        tags: s.tags.clone(),
        examples: s.examples.clone(),
        input_modes: s.input_modes.clone(),
        output_modes: s.output_modes.clone(),
    }
}

fn convert_skill_to_nacos(s: &A2aAgentSkill) -> NacosAgentSkill {
    NacosAgentSkill {
        id: s.id.clone(),
        name: s.name.clone(),
        description: s.description.clone(),
        tags: s.tags.clone(),
        examples: s.examples.clone(),
        input_modes: s.input_modes.clone(),
        output_modes: s.output_modes.clone(),
    }
}

/// Convenience free function mirroring
/// `AgentCardConverterUtil.convertToA2aAgentCard`.
pub fn convert_to_a2a_agent_card(card: &NacosAgentCard) -> A2aAgentCard {
    NacosAgentCardConverter::to_a2a_agent_card(card)
}

/// Convenience free function mirroring
/// `AgentCardConverterUtil.convertToNacosAgentCard`.
pub fn convert_to_nacos_agent_card(card: &A2aAgentCard) -> NacosAgentCard {
    NacosAgentCardConverter::to_nacos_agent_card(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_a2a() -> A2aAgentCard {
        let mut card = A2aAgentCard::new("demo", "http://localhost:8080");
        card.description = Some("desc".into());
        card.version = Some("1.0.0".into());
        card.provider = Some(A2aAgentProvider {
            organization: "org".into(),
            url: Some("http://org".into()),
        });
        card.additional_interfaces = Some(vec![A2aAgentInterface {
            url: "http://localhost:8080/jsonrpc".into(),
            transport: "JSONRPC".into(),
        }]);
        card.skills = vec![A2aAgentSkill {
            id: "s1".into(),
            name: "Skill One".into(),
            description: Some("does one".into()),
            tags: Some(vec!["t".into()]),
            examples: None,
            input_modes: None,
            output_modes: None,
        }];
        card
    }

    #[test]
    fn round_trips_card_fields() {
        let a2a = sample_a2a();
        let nacos = convert_to_nacos_agent_card(&a2a);
        let back = convert_to_a2a_agent_card(&nacos);
        assert_eq!(a2a, back);
    }

    #[test]
    fn security_schemes_kept_as_opaque_json() {
        let mut a2a = sample_a2a();
        let mut schemes = serde_json::Map::new();
        schemes.insert(
            "apiKey".into(),
            serde_json::json!({"type": "apiKey", "in": "header"}),
        );
        a2a.security_schemes = Some(schemes.clone());
        let nacos = convert_to_nacos_agent_card(&a2a);
        assert_eq!(nacos.security_schemes.unwrap(), schemes);
    }

    #[test]
    fn null_provider_and_interfaces_map_to_none() {
        // Mirrors Java `convertToA2aAgentCard` null-provider / null-interfaces cases.
        let mut a2a = A2aAgentCard::new("bare", "http://h");
        a2a.provider = None;
        a2a.additional_interfaces = None;
        a2a.skills = Vec::new();

        let nacos = convert_to_nacos_agent_card(&a2a);
        assert!(nacos.provider.is_none());
        assert!(nacos.additional_interfaces.is_none());
        assert!(nacos.skills.is_empty());

        let back = convert_to_a2a_agent_card(&nacos);
        assert!(back.provider.is_none());
        assert_eq!(back, a2a);
    }

    #[test]
    fn empty_capabilities_round_trip_as_defaults() {
        let card = NacosAgentCard {
            capabilities: NacosAgentCapabilities::default(),
            ..NacosAgentCard::new("caps", "http://h")
        };
        let a2a = convert_to_a2a_agent_card(&card);
        assert_eq!(a2a.capabilities.streaming, None);
        assert_eq!(a2a.capabilities.push_notifications, None);
        assert_eq!(a2a.capabilities.state_transition_history, None);
    }

    #[test]
    fn card_json_field_names_match_nacos() {
        let card = NacosAgentCard::new("n", "http://h");
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["preferredTransport"], "JSONRPC");
        assert_eq!(json["defaultInputModes"][0], "text/plain");
        assert!(!json.as_object().unwrap().contains_key("securitySchemes"));
    }
}
