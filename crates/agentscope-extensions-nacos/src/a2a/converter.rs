//! Bidirectional AgentCard conversion, mirroring
//! `io.agentscope.core.nacos.a2a.utils.AgentCardConverterUtil`.
//!
//! The mapping rules (see the Java javadoc) are:
//! * The A2A *supported interfaces* are the Nacos `url` + `preferredTransport`
//!   (default `JSONRPC`), which come first, followed by the Nacos
//!   `additionalInterfaces`; the Nacos `protocolVersion` is the version of all of
//!   them.
//! * A2A `capabilities.extendedAgentCard` is the Nacos
//!   `supportsAuthenticatedExtendedCard`. The protocol `AgentCapabilities` has no
//!   `extendedAgentCard` field (the A2A 0.3 model keeps it on the card), so the
//!   boolean is applied to `AgentCard::supports_authenticated_extended_card`.
//! * `capabilities.stateTransitionHistory` is **not** part of A2A 1.0: it is not
//!   copied back from Nacos, and is forced to `false` when writing to Nacos.
//! * Security schemes are stored in Nacos as maps in the A2A 0.3 format, see
//!   [`super::security_scheme`].

use agentscope_extensions_a2a_client::model::AgentCard;
use serde_json::Value;

use super::model::{
    A2aAgentCapabilities, A2aAgentCard, A2aAgentInterface, A2aAgentProvider, A2aAgentSkill,
    NacosAgentCapabilities, NacosAgentCard, NacosAgentExtension, NacosAgentInterface,
    NacosAgentProvider, NacosAgentSkill,
};
use super::security_scheme::{convert_to_a2a_security_schemes, convert_to_nacos_security_schemes};

/// Protocol binding used when neither side names one, mirroring the Java
/// `"JSONRPC"` literal in `convertToA2aAgentInterfaces`.
pub const DEFAULT_PREFERRED_TRANSPORT: &str = "JSONRPC";

/// Converts between Nacos AgentCards and A2A-specification AgentCards.
pub struct NacosAgentCardConverter;

impl NacosAgentCardConverter {
    /// Converts a Nacos AgentCard to an A2A-specification AgentCard.
    pub fn to_a2a_agent_card(card: &NacosAgentCard) -> A2aAgentCard {
        let interfaces = nacos_supported_interfaces(card);
        let (preferred, additional) = split_interfaces(&interfaces);
        A2aAgentCard {
            inner: AgentCard {
                name: card.name.clone(),
                description: card.description.clone(),
                url: preferred
                    .as_ref()
                    .map(|each| each.url.clone())
                    .unwrap_or_default(),
                version: card.version.clone(),
                protocol_version: card.protocol_version.clone(),
                provider: card.provider.as_ref().map(convert_provider_to_a2a),
                preferred_transport: preferred.as_ref().map(|each| each.transport.clone()),
                additional_interfaces: additional,
                icon_url: card.icon_url.clone(),
                capabilities: convert_capabilities_to_a2a(&card.capabilities),
                default_input_modes: card.default_input_modes.clone(),
                default_output_modes: card.default_output_modes.clone(),
                skills: card.skills.iter().map(convert_skill_to_a2a).collect(),
                security_schemes: convert_to_a2a_security_schemes(card.security_schemes.as_ref()),
                security: card.security.clone(),
                // `capabilities.extendedAgentCard` in the A2A 1.0 model; always a
                // definite boolean, mirroring `Boolean.TRUE.equals(..)`.
                supports_authenticated_extended_card: Some(
                    card.supports_authenticated_extended_card.unwrap_or(false),
                ),
                signatures: None,
                documentation_url: card.documentation_url.clone(),
            },
        }
    }

    /// Converts an A2A-specification AgentCard to a Nacos AgentCard.
    pub fn to_nacos_agent_card(card: &A2aAgentCard) -> NacosAgentCard {
        let interfaces = supported_interfaces(card);
        let (preferred, _) = split_interfaces(&interfaces);
        NacosAgentCard {
            name: card.name.clone(),
            description: card.description.clone(),
            // Only the preferred interface carries `url` / `preferredTransport` /
            // `protocolVersion`; both stay `None` when the card exposes no interface.
            url: preferred.as_ref().map(|each| each.url.clone()),
            version: card.version.clone(),
            protocol_version: preferred.as_ref().and(card.protocol_version.clone()),
            provider: card.provider.as_ref().map(convert_provider_to_nacos),
            preferred_transport: preferred.as_ref().map(|each| each.transport.clone()),
            // Java maps *every* interface (the preferred one included) into the
            // Nacos `additionalInterfaces` list.
            additional_interfaces: Some(
                interfaces.iter().map(convert_interface_to_nacos).collect(),
            ),
            icon_url: card.icon_url.clone(),
            capabilities: convert_capabilities_to_nacos(&card.capabilities),
            default_input_modes: card.default_input_modes.clone(),
            default_output_modes: card.default_output_modes.clone(),
            skills: card.skills.iter().map(convert_skill_to_nacos).collect(),
            security_schemes: convert_to_nacos_security_schemes(card.security_schemes.as_ref()),
            security: card.security.clone(),
            supports_authenticated_extended_card: Some(
                card.supports_authenticated_extended_card.unwrap_or(false),
            ),
            documentation_url: card.documentation_url.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Supported interfaces (url / preferredTransport / additionalInterfaces)
// ---------------------------------------------------------------------------

/// Assembles the A2A *supported interfaces* list of an A2A card.
///
/// Mirrors `AgentCardConverterUtil#convertToA2aAgentInterfaces`: the card `url`
/// with its `preferredTransport` (default [`DEFAULT_PREFERRED_TRANSPORT`]) first,
/// then the `additionalInterfaces` with duplicates dropped. The protocol version
/// is carried once, on the card.
///
/// An empty `url` is treated as absent (Java only skips `null`; the A2A 0.3 model
/// used here cannot express a missing `url`).
pub fn supported_interfaces(card: &A2aAgentCard) -> Vec<A2aAgentInterface> {
    let url = (!card.url.is_empty()).then_some(card.url.as_str());
    assemble_interfaces(
        url,
        card.preferred_transport.as_deref(),
        card.additional_interfaces.as_deref().unwrap_or_default(),
    )
}

/// Assembles the A2A *supported interfaces* list of a Nacos card.
pub fn nacos_supported_interfaces(card: &NacosAgentCard) -> Vec<A2aAgentInterface> {
    let additional: Vec<A2aAgentInterface> = card
        .additional_interfaces
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(convert_interface_to_a2a)
        .collect();
    assemble_interfaces(
        card.url.as_deref().filter(|url| !url.is_empty()),
        card.preferred_transport.as_deref(),
        &additional,
    )
}

fn assemble_interfaces(
    url: Option<&str>,
    preferred_transport: Option<&str>,
    additional: &[A2aAgentInterface],
) -> Vec<A2aAgentInterface> {
    let mut result: Vec<A2aAgentInterface> = Vec::with_capacity(additional.len() + 1);
    if let Some(url) = url {
        result.push(A2aAgentInterface {
            url: url.to_string(),
            transport: preferred_transport
                .unwrap_or(DEFAULT_PREFERRED_TRANSPORT)
                .to_string(),
        });
    }
    for each in additional {
        if !result.contains(each) {
            result.push(each.clone());
        }
    }
    result
}

/// Splits a supported-interfaces list into `(preferred, additional)`.
fn split_interfaces(
    interfaces: &[A2aAgentInterface],
) -> (Option<A2aAgentInterface>, Option<Vec<A2aAgentInterface>>) {
    match interfaces.split_first() {
        None => (None, None),
        Some((preferred, rest)) => (
            Some(preferred.clone()),
            (!rest.is_empty()).then(|| rest.to_vec()),
        ),
    }
}

/// Writes a supported-interfaces list back onto an A2A card: the first entry
/// becomes `url` / `preferredTransport`, the remaining ones `additionalInterfaces`.
///
/// This is the inverse of [`supported_interfaces`]; `NacosAgentRegistry`'s
/// overwrite step uses it to apply Java's `add(0, ..)` on `supportedInterfaces`.
pub fn apply_supported_interfaces(card: &mut A2aAgentCard, interfaces: &[A2aAgentInterface]) {
    let (preferred, additional) = split_interfaces(interfaces);
    card.url = preferred
        .as_ref()
        .map(|each| each.url.clone())
        .unwrap_or_default();
    card.preferred_transport = preferred.map(|each| each.transport);
    card.additional_interfaces = additional;
}

// ---------------------------------------------------------------------------
// Field conversions
// ---------------------------------------------------------------------------

/// Nacos → A2A capabilities.
///
/// `streaming` / `pushNotifications` are materialised as definite booleans (Java
/// `Boolean.TRUE.equals(..)`), and `stateTransitionHistory` is dropped because
/// A2A 1.0 no longer has it. `extendedAgentCard` is applied to the card by
/// [`NacosAgentCardConverter::to_a2a_agent_card`].
fn convert_capabilities_to_a2a(c: &NacosAgentCapabilities) -> A2aAgentCapabilities {
    A2aAgentCapabilities {
        streaming: Some(c.streaming.unwrap_or(false)),
        push_notifications: Some(c.push_notifications.unwrap_or(false)),
        state_transition_history: None,
        // `5da592b4a`: Nacos and A2A both carry `capabilities.extensions`; copying only the
        // boolean flags dropped every extension's uri/description/required/params.
        extensions: c
            .extensions
            .as_ref()
            .map(|list| list.iter().map(convert_extension_to_a2a).collect()),
    }
}

/// Nacos extension → A2A extension (a JSON object, because the protocol crate keeps the
/// extensions opaque for forward compatibility).
fn convert_extension_to_a2a(extension: &NacosAgentExtension) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("uri".to_string(), Value::String(extension.uri.clone()));
    if let Some(description) = &extension.description {
        map.insert(
            "description".to_string(),
            Value::String(description.clone()),
        );
    }
    // A2A's `required` is a primitive boolean: a missing Nacos value maps to `false`
    // (Java `Boolean.TRUE.equals(extension.getRequired())`).
    map.insert(
        "required".to_string(),
        Value::Bool(extension.required.unwrap_or(false)),
    );
    if let Some(params) = &extension.params {
        map.insert("params".to_string(), params.clone());
    }
    // Unknown Nacos fields are carried through untouched (never overwriting a known key).
    for (key, value) in &extension.extra {
        if !map.contains_key(key) {
            map.insert(key.clone(), value.clone());
        }
    }
    Value::Object(map)
}

/// A2A extension (opaque JSON) → Nacos extension. Non-object entries are skipped.
fn convert_extension_to_nacos(value: &Value) -> Option<NacosAgentExtension> {
    let object = value.as_object()?;
    let uri = object
        .get("uri")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let description = object
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let required = Some(
        object
            .get("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    );
    // Structured parameters are kept unchanged; an explicit JSON `null` is "absent".
    let params = object.get("params").cloned().filter(|v| !v.is_null());
    let extra = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "uri" | "description" | "required" | "params"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Some(NacosAgentExtension {
        uri,
        description,
        required,
        params,
        extra,
    })
}

/// A2A → Nacos capabilities: `stateTransitionHistory` is always `false`
/// ("Not part of A2A 1.0", Java `setStateTransitionHistory(false)`).
fn convert_capabilities_to_nacos(c: &A2aAgentCapabilities) -> NacosAgentCapabilities {
    NacosAgentCapabilities {
        streaming: c.streaming,
        push_notifications: c.push_notifications,
        state_transition_history: Some(false),
        // `null` (absent) stays `None`; an explicitly empty list stays `Some(vec![])`.
        extensions: c
            .extensions
            .as_ref()
            .map(|list| list.iter().filter_map(convert_extension_to_nacos).collect()),
    }
}

fn convert_provider_to_a2a(p: &NacosAgentProvider) -> A2aAgentProvider {
    A2aAgentProvider {
        organization: p.organization.clone(),
        // The A2A model requires a `url`; the Nacos one may omit it.
        url: p.url.clone().unwrap_or_default(),
    }
}

fn convert_provider_to_nacos(p: &A2aAgentProvider) -> NacosAgentProvider {
    NacosAgentProvider {
        organization: p.organization.clone(),
        // An absent A2A url round-trips back to `None` (Java keeps `null`).
        url: (!p.url.is_empty()).then(|| p.url.clone()),
    }
}

fn convert_interface_to_a2a(i: &NacosAgentInterface) -> A2aAgentInterface {
    A2aAgentInterface {
        url: i.url.clone(),
        transport: i.transport.clone(),
    }
}

fn convert_interface_to_nacos(i: &A2aAgentInterface) -> NacosAgentInterface {
    NacosAgentInterface {
        url: i.url.clone(),
        transport: i.transport.clone(),
    }
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
        card.protocol_version = Some("0.3.0".into());
        card.provider = Some(A2aAgentProvider {
            organization: "org".into(),
            url: "http://org".into(),
        });
        // A distinct additional interface (mirrors the Java sample: JSONRPC +
        // GRPC); an interface equal to the preferred one is de-duplicated.
        card.additional_interfaces = Some(vec![A2aAgentInterface {
            url: "http://localhost:8080/grpc".into(),
            transport: "GRPC".into(),
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
        card.supports_authenticated_extended_card = Some(true);
        card.documentation_url = Some("http://docs".into());
        // The A2A 1.0 model materialises these as definite booleans, so a
        // round trip through Nacos keeps them.
        card.capabilities.streaming = Some(false);
        card.capabilities.push_notifications = Some(false);
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
    fn a2a_to_nacos_lists_every_interface() {
        // Java `convertToNacosAgentCard`: `additionalInterfaces` holds every
        // supported interface, the preferred one included.
        let nacos = convert_to_nacos_agent_card(&sample_a2a());
        assert_eq!(nacos.url.as_deref(), Some("http://localhost:8080"));
        assert_eq!(nacos.preferred_transport.as_deref(), Some("JSONRPC"));
        assert_eq!(nacos.protocol_version.as_deref(), Some("0.3.0"));
        let interfaces = nacos.additional_interfaces.unwrap();
        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces[0].transport, "JSONRPC");
        assert_eq!(interfaces[0].url, "http://localhost:8080");
        assert_eq!(interfaces[1].transport, "GRPC");
        assert_eq!(interfaces[1].url, "http://localhost:8080/grpc");
    }

    #[test]
    fn nacos_interfaces_default_transport_and_dedup() {
        let mut card = NacosAgentCard::new("n", "http://h:1");
        card.preferred_transport = None;
        card.additional_interfaces = Some(vec![
            // Duplicate of the preferred interface -> dropped.
            NacosAgentInterface {
                url: "http://h:1".into(),
                transport: DEFAULT_PREFERRED_TRANSPORT.into(),
            },
            NacosAgentInterface {
                url: "http://h:2".into(),
                transport: "GRPC".into(),
            },
        ]);
        let a2a = convert_to_a2a_agent_card(&card);
        assert_eq!(a2a.url, "http://h:1");
        assert_eq!(a2a.preferred_transport.as_deref(), Some("JSONRPC"));
        assert_eq!(
            a2a.additional_interfaces.as_deref(),
            Some(
                [A2aAgentInterface {
                    url: "http://h:2".into(),
                    transport: "GRPC".into(),
                }]
                .as_slice()
            )
        );
    }

    #[test]
    fn nacos_card_without_url_has_no_preferred_interface() {
        let mut card = NacosAgentCard::new("n", "http://h:1");
        card.url = None;
        let a2a = convert_to_a2a_agent_card(&card);
        assert_eq!(a2a.url, "");
        assert!(a2a.preferred_transport.is_none());
        assert!(a2a.additional_interfaces.is_none());

        // …and converting back leaves the Nacos url empty (Java: null).
        let back = convert_to_nacos_agent_card(&a2a);
        assert!(back.url.is_none());
        assert!(back.preferred_transport.is_none());
        assert!(back.protocol_version.is_none());
    }

    #[test]
    fn apply_supported_interfaces_puts_first_as_preferred() {
        let mut card = A2aAgentCard::new("c", "http://old");
        card.additional_interfaces = Some(vec![A2aAgentInterface {
            url: "http://old/grpc".into(),
            transport: "GRPC".into(),
        }]);
        apply_supported_interfaces(
            &mut card,
            &[
                A2aAgentInterface {
                    url: "http://new".into(),
                    transport: "HTTP".into(),
                },
                A2aAgentInterface {
                    url: "http://old".into(),
                    transport: "JSONRPC".into(),
                },
            ],
        );
        assert_eq!(card.url, "http://new");
        assert_eq!(card.preferred_transport.as_deref(), Some("HTTP"));
        assert_eq!(
            supported_interfaces(&card),
            vec![
                A2aAgentInterface {
                    url: "http://new".into(),
                    transport: "HTTP".into(),
                },
                A2aAgentInterface {
                    url: "http://old".into(),
                    transport: "JSONRPC".into(),
                },
            ]
        );
    }

    #[test]
    fn security_schemes_are_converted_by_type() {
        let mut a2a = sample_a2a();
        let mut schemes = serde_json::Map::new();
        schemes.insert(
            "apiKey".into(),
            serde_json::json!({"type": "apiKey", "in": "header", "name": "X-Key"}),
        );
        a2a.security_schemes = Some(schemes);
        let nacos = convert_to_nacos_agent_card(&a2a);
        assert_eq!(
            nacos.security_schemes.unwrap()["apiKey"],
            serde_json::json!({"type": "apiKey", "in": "header", "name": "X-Key"})
        );
        // Round trip through Nacos keeps the scheme readable.
        let back = convert_to_a2a_agent_card(&convert_to_nacos_agent_card(&a2a));
        assert_eq!(back.security_schemes, a2a.security_schemes);
    }

    #[test]
    fn null_provider_and_interfaces_map_to_none() {
        // Mirrors Java `convertToA2aAgentCard` null-provider / null-interfaces cases.
        let mut a2a = A2aAgentCard::new("bare", "http://h");
        a2a.provider = None;
        a2a.additional_interfaces = None;
        a2a.skills = Vec::new();
        a2a.supports_authenticated_extended_card = None;

        let nacos = convert_to_nacos_agent_card(&a2a);
        assert!(nacos.provider.is_none());
        assert!(nacos.url.is_some());
        assert!(nacos.skills.is_empty());
        // Java always materialises the extended-card flag.
        assert_eq!(nacos.supports_authenticated_extended_card, Some(false));

        let back = convert_to_a2a_agent_card(&nacos);
        assert!(back.provider.is_none());
        assert_eq!(back.supports_authenticated_extended_card, Some(false));
        assert_eq!(back.url, a2a.url);
        assert_eq!(back.skills, a2a.skills);
    }

    #[test]
    fn provider_without_url_round_trips_as_none() {
        let mut card = NacosAgentCard::new("n", "http://h");
        card.provider = Some(NacosAgentProvider {
            organization: "org".into(),
            url: None,
        });
        let a2a = convert_to_a2a_agent_card(&card);
        assert_eq!(a2a.provider.as_ref().unwrap().url, "");
        let back = convert_to_nacos_agent_card(&a2a);
        assert!(back.provider.unwrap().url.is_none());
    }

    #[test]
    fn nacos_capabilities_become_definite_booleans() {
        // Java `Boolean.TRUE.equals(..)`: absent flags become `false`.
        let card = NacosAgentCard {
            capabilities: NacosAgentCapabilities::default(),
            ..NacosAgentCard::new("caps", "http://h")
        };
        let a2a = convert_to_a2a_agent_card(&card);
        assert_eq!(a2a.capabilities.streaming, Some(false));
        assert_eq!(a2a.capabilities.push_notifications, Some(false));
        // A2A 1.0 dropped `stateTransitionHistory`; it is not copied back.
        assert_eq!(a2a.capabilities.state_transition_history, None);
        assert_eq!(a2a.supports_authenticated_extended_card, Some(false));
    }

    #[test]
    fn state_transition_history_is_forced_false_on_nacos() {
        let mut a2a = sample_a2a();
        a2a.capabilities.streaming = Some(true);
        a2a.capabilities.push_notifications = None;
        a2a.capabilities.state_transition_history = Some(true);
        let nacos = convert_to_nacos_agent_card(&a2a);
        assert_eq!(nacos.capabilities.streaming, Some(true));
        assert_eq!(nacos.capabilities.push_notifications, None);
        assert_eq!(nacos.capabilities.state_transition_history, Some(false));
    }

    #[test]
    fn a2a_card_serializes_documentation_url_flat() {
        let card = sample_a2a();
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["documentationUrl"], "http://docs");
        assert_eq!(json["name"], "demo");
        // A2A 1.0 moved the endpoints into `supportedInterfaces`; the old flat
        // `preferredTransport` key is no longer emitted (PROTO-A2A-01).
        assert!(json.get("preferredTransport").is_none());
        assert_eq!(json["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
        assert_eq!(
            json["supportedInterfaces"][0]["url"],
            "http://localhost:8080"
        );
        // Flattened wrapper must deserialize back into the same card. The card-level
        // `protocolVersion` is not part of the 1.0 wire shape (every interface
        // carries it), so only the stable fields are compared.
        let back: A2aAgentCard = serde_json::from_value(json).unwrap();
        assert_eq!(back.url, card.url);
        assert_eq!(back.preferred_transport, card.preferred_transport);
        assert_eq!(back.additional_interfaces, card.additional_interfaces);
        assert_eq!(back.skills, card.skills);
        assert_eq!(back.documentation_url, card.documentation_url);
    }

    #[test]
    fn card_json_field_names_match_nacos() {
        let card = NacosAgentCard::new("n", "http://h");
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["preferredTransport"], "JSONRPC");
        assert_eq!(json["defaultInputModes"][0], "text/plain");
        assert!(!json.as_object().unwrap().contains_key("securitySchemes"));
    }

    // ------------------------------------------------------------------
    // `5da592b4a` fix(nacos): preserve A2A capability extensions
    // ------------------------------------------------------------------

    fn extension() -> NacosAgentExtension {
        NacosAgentExtension {
            uri: "https://example.com/ext/v1".to_string(),
            description: Some("vendor extension".to_string()),
            required: Some(true),
            params: Some(serde_json::json!({"mode": "fast", "retries": 2})),
            extra: Default::default(),
        }
    }

    #[test]
    fn nacos_extensions_keep_uri_description_required_and_params() {
        let mut card = NacosAgentCard::new("n", "http://h");
        card.capabilities.extensions = Some(vec![extension()]);

        let a2a = convert_to_a2a_agent_card(&card);
        let extensions = a2a.capabilities.extensions.as_ref().expect("extensions");
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0]["uri"], "https://example.com/ext/v1");
        assert_eq!(extensions[0]["description"], "vendor extension");
        assert_eq!(extensions[0]["required"], true);
        // Structured parameters are unchanged, not stringified.
        assert_eq!(extensions[0]["params"]["mode"], "fast");
        assert_eq!(extensions[0]["params"]["retries"], 2);

        // … and back to Nacos without losing anything.
        let back = convert_to_nacos_agent_card(&a2a);
        assert_eq!(back.capabilities.extensions, Some(vec![extension()]));
    }

    #[test]
    fn a_missing_nacos_required_flag_becomes_false() {
        let mut card = NacosAgentCard::new("n", "http://h");
        card.capabilities.extensions = Some(vec![NacosAgentExtension {
            uri: "https://example.com/ext/v1".to_string(),
            ..Default::default()
        }]);
        let a2a = convert_to_a2a_agent_card(&card);
        // A2A uses a primitive boolean, so `null` must not survive as a missing field.
        assert_eq!(
            a2a.capabilities.extensions.as_ref().unwrap()[0]["required"],
            false
        );
    }

    #[test]
    fn absent_and_empty_extension_lists_stay_distinct() {
        let mut absent = NacosAgentCard::new("n", "http://h");
        absent.capabilities.extensions = None;
        assert_eq!(
            convert_to_a2a_agent_card(&absent).capabilities.extensions,
            None
        );

        let mut empty = NacosAgentCard::new("n", "http://h");
        empty.capabilities.extensions = Some(Vec::new());
        let a2a = convert_to_a2a_agent_card(&empty);
        assert_eq!(a2a.capabilities.extensions, Some(Vec::new()));
        // Both directions keep the distinction.
        assert_eq!(
            convert_to_nacos_agent_card(&a2a).capabilities.extensions,
            Some(Vec::new())
        );
    }

    #[test]
    fn unknown_extension_fields_survive_a_round_trip() {
        let mut card = NacosAgentCard::new("n", "http://h");
        let mut extra = serde_json::Map::new();
        extra.insert("x-vendor".to_string(), serde_json::json!({"a": 1}));
        card.capabilities.extensions = Some(vec![NacosAgentExtension {
            extra,
            ..extension()
        }]);

        let a2a = convert_to_a2a_agent_card(&card);
        assert_eq!(
            a2a.capabilities.extensions.as_ref().unwrap()[0]["x-vendor"]["a"],
            1
        );
        let back = convert_to_nacos_agent_card(&a2a);
        let restored = &back.capabilities.extensions.as_ref().unwrap()[0];
        assert_eq!(
            restored.extra.get("x-vendor"),
            Some(&serde_json::json!({"a": 1}))
        );
    }

    #[test]
    fn a2a_extensions_without_a_nacos_counterpart_are_kept_verbatim() {
        // Opaque JSON from a third-party agent card: only the four known keys are typed, the
        // rest travels in `extra`; non-object entries are skipped instead of panicking.
        let mut a2a = sample_a2a();
        a2a.capabilities.extensions = Some(vec![
            serde_json::json!({"uri": "https://x", "required": true, "params": null, "other": 5}),
            serde_json::json!("not-an-object"),
        ]);
        let nacos = convert_to_nacos_agent_card(&a2a);
        let extensions = nacos.capabilities.extensions.as_ref().unwrap();
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].uri, "https://x");
        assert_eq!(extensions[0].required, Some(true));
        // An explicit JSON null parameter is "absent", not `Value::Null`.
        assert_eq!(extensions[0].params, None);
        assert_eq!(
            extensions[0].extra.get("other"),
            Some(&serde_json::json!(5))
        );
    }
}
