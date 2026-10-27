//! Nacos A2A registry, mirroring `io.agentscope.core.nacos.a2a.registry`.

pub mod constants;

use std::collections::HashMap;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::service::{AiServiceError, AiServiceResult, NacosAiService};

use super::converter::convert_to_nacos_agent_card;
use super::model::{A2aAgentCard, NacosAgentCard, NacosAgentEndpoint};
use constants::{
    PROPERTIES_ENV_PREFIX, PROTOCOL_TYPE_HTTP, PROTOCOL_TYPE_HTTPS, TRANSPORT_DEFAULT_PROTOCOL,
};

pub use constants::TransportPropertiesAttribute;

// ===========================================================================
// NacosA2aTransportProperties
// ===========================================================================

/// Properties for A2A Transports Endpoint registration to Nacos, mirroring
/// `NacosA2aRegistryTransportProperties`.
///
/// Used to configure an A2A interface (endpoint) registered to Nacos; these
/// properties are merged into `transport` and `url`. The conceptual format is
/// `[protocol://]host:port[/path][?query]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NacosA2aTransportProperties {
    pub transport: String,
    pub host: String,
    pub port: i32,
    pub path: Option<String>,
    pub support_tls: bool,
    pub protocol: Option<String>,
    pub query: Option<String>,
}

impl NacosA2aTransportProperties {
    /// Starts a [`NacosA2aTransportPropertiesBuilder`].
    pub fn builder() -> NacosA2aTransportPropertiesBuilder {
        NacosA2aTransportPropertiesBuilder::default()
    }

    /// The effective protocol: `protocol` when set, else `http`; `http` is
    /// upgraded to `https` when `support_tls` is set. Mirrors
    /// `NacosAgentRegistry#handleTlsIfNeeded`.
    pub fn effective_protocol(&self) -> String {
        let protocol = self
            .protocol
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or(TRANSPORT_DEFAULT_PROTOCOL);
        if protocol.eq_ignore_ascii_case(TRANSPORT_DEFAULT_PROTOCOL) {
            if self.support_tls {
                PROTOCOL_TYPE_HTTPS.to_string()
            } else {
                PROTOCOL_TYPE_HTTP.to_string()
            }
        } else {
            protocol.to_string()
        }
    }
}

/// Builder for [`NacosA2aTransportProperties`].
#[derive(Debug, Clone, Default)]
pub struct NacosA2aTransportPropertiesBuilder {
    transport: Option<String>,
    host: Option<String>,
    port: Option<i32>,
    path: Option<String>,
    support_tls: Option<bool>,
    protocol: Option<String>,
    query: Option<String>,
}

impl NacosA2aTransportPropertiesBuilder {
    pub fn transport(mut self, v: impl Into<String>) -> Self {
        self.transport = Some(v.into());
        self
    }
    pub fn host(mut self, v: impl Into<String>) -> Self {
        self.host = Some(v.into());
        self
    }
    pub fn port(mut self, v: i32) -> Self {
        self.port = Some(v);
        self
    }
    pub fn path(mut self, v: impl Into<String>) -> Self {
        self.path = Some(v.into());
        self
    }
    pub fn support_tls(mut self, v: bool) -> Self {
        self.support_tls = Some(v);
        self
    }
    pub fn protocol(mut self, v: impl Into<String>) -> Self {
        self.protocol = Some(v.into());
        self
    }
    pub fn query(mut self, v: impl Into<String>) -> Self {
        self.query = Some(v.into());
        self
    }

    /// Builds the properties, validating that `transport` is not empty.
    /// Mirrors the Java builder's `IllegalArgumentException`.
    pub fn build(self) -> Result<NacosA2aTransportProperties, String> {
        let transport = self.transport.unwrap_or_default();
        if transport.trim().is_empty() {
            return Err("A2A Endpoint `transport` can not be empty.".to_string());
        }
        Ok(NacosA2aTransportProperties {
            transport,
            host: self.host.unwrap_or_default(),
            port: self.port.unwrap_or(0),
            path: self.path,
            support_tls: self.support_tls.unwrap_or(false),
            protocol: self.protocol,
            query: self.query,
        })
    }
}

// ===========================================================================
// NacosA2aRegistryProperties
// ===========================================================================

/// Properties for A2A AgentCard and Endpoint registration to Nacos, mirroring
/// `NacosA2aRegistryProperties`.
#[derive(Debug, Clone, PartialEq)]
pub struct NacosA2aRegistryProperties {
    /// Always register the A2A service as the latest version. Default `false`.
    pub is_set_as_latest: bool,
    /// Automatically register all transports as endpoints. Default `true`.
    pub enabled_register_endpoint: bool,
    /// Override the AgentCard `preferredTransport` and `url` with this transport.
    pub overwrite_preferred_transport: Option<String>,
    /// Transport properties keyed by transport name (upper-cased on insert).
    pub transport_properties: HashMap<String, NacosA2aTransportProperties>,
}

impl Default for NacosA2aRegistryProperties {
    fn default() -> Self {
        Self {
            is_set_as_latest: false,
            enabled_register_endpoint: true,
            overwrite_preferred_transport: None,
            transport_properties: HashMap::new(),
        }
    }
}

impl NacosA2aRegistryProperties {
    pub fn builder() -> NacosA2aRegistryPropertiesBuilder {
        NacosA2aRegistryPropertiesBuilder::default()
    }

    /// Starts a builder pre-filled from an existing instance (transports excluded),
    /// mirroring `builder(prototype)`.
    pub fn builder_from(
        prototype: &NacosA2aRegistryProperties,
    ) -> NacosA2aRegistryPropertiesBuilder {
        NacosA2aRegistryPropertiesBuilder {
            set_as_latest: prototype.is_set_as_latest,
            enabled_register_endpoint: prototype.enabled_register_endpoint,
            overwrite_preferred_transport: prototype.overwrite_preferred_transport.clone(),
            transport_properties: HashMap::new(),
        }
    }

    /// Adds a transport, keyed (upper-cased) by its transport name.
    pub fn add_transport(&mut self, transport: NacosA2aTransportProperties) {
        self.transport_properties
            .insert(transport.transport.to_uppercase(), transport);
    }
}

/// Builder for [`NacosA2aRegistryProperties`].
#[derive(Debug, Clone)]
pub struct NacosA2aRegistryPropertiesBuilder {
    set_as_latest: bool,
    enabled_register_endpoint: bool,
    overwrite_preferred_transport: Option<String>,
    transport_properties: HashMap<String, NacosA2aTransportProperties>,
}

impl Default for NacosA2aRegistryPropertiesBuilder {
    fn default() -> Self {
        Self {
            set_as_latest: false,
            enabled_register_endpoint: true,
            overwrite_preferred_transport: None,
            transport_properties: HashMap::new(),
        }
    }
}

impl NacosA2aRegistryPropertiesBuilder {
    pub fn set_as_latest(mut self, v: bool) -> Self {
        self.set_as_latest = v;
        self
    }
    pub fn enabled_register_endpoint(mut self, v: bool) -> Self {
        self.enabled_register_endpoint = v;
        self
    }
    pub fn overwrite_preferred_transport(mut self, v: impl Into<String>) -> Self {
        self.overwrite_preferred_transport = Some(v.into());
        self
    }
    pub fn add_transport(mut self, transport: NacosA2aTransportProperties) -> Self {
        self.transport_properties
            .insert(transport.transport.to_uppercase(), transport);
        self
    }
    pub fn build(self) -> NacosA2aRegistryProperties {
        NacosA2aRegistryProperties {
            is_set_as_latest: self.set_as_latest,
            enabled_register_endpoint: self.enabled_register_endpoint,
            overwrite_preferred_transport: self.overwrite_preferred_transport,
            transport_properties: self.transport_properties,
        }
    }
}

// ===========================================================================
// Environment parser
// ===========================================================================

/// Parses [`NacosA2aTransportProperties`] from environment variables, mirroring
/// `NacosA2aTransportPropertiesEnvParser`.
///
/// Env keys are prefixed with `NACOS_A2A_AGENT_` and end with
/// `{TRANSPORT}_{ATTRIBUTE}`, e.g. `NACOS_A2A_AGENT_JSONRPC_HOST=127.0.0.1`.
pub struct NacosA2aTransportPropertiesEnvParser;

impl NacosA2aTransportPropertiesEnvParser {
    /// Parses transport properties from the process environment.
    ///
    /// Mirrors `getTransportProperties()`: an unparsable `port` value is an error
    /// (`NacosException.INVALID_PARAM`), matching the Java parser which throws
    /// `NacosRuntimeException`.
    pub fn from_env() -> AiServiceResult<HashMap<String, NacosA2aTransportProperties>> {
        Self::parse(std::env::vars())
    }

    /// Parses transport properties from an arbitrary key/value source (testable).
    ///
    /// Returns `Err` with [`AiServiceError::INVALID_PARAM`] when a `port` value
    /// does not parse as an integer, mirroring Java's `parseInt` helper.
    pub fn parse<I, K, V>(env: I) -> AiServiceResult<HashMap<String, NacosA2aTransportProperties>>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        // transport -> attribute -> raw value
        let mut grouped: HashMap<String, HashMap<TransportPropertiesAttribute, String>> =
            HashMap::new();
        for (key, value) in env {
            let key = key.as_ref();
            let Some(sub_key) = key.strip_prefix(PROPERTIES_ENV_PREFIX) else {
                continue;
            };
            let parts: Vec<&str> = sub_key.split('_').collect();
            if parts.len() != 2 {
                continue;
            }
            let transport = parts[0].trim().to_uppercase();
            let attr = parts[1].trim().to_uppercase();
            let Some(attribute) = TransportPropertiesAttribute::from_env_key(&attr) else {
                continue;
            };
            grouped
                .entry(transport)
                .or_default()
                .insert(attribute, value.into());
        }

        let mut result = HashMap::with_capacity(grouped.len());
        for (transport, attrs) in grouped {
            result.insert(
                transport.clone(),
                Self::convert_to_properties(&transport, attrs)?,
            );
        }
        Ok(result)
    }

    fn convert_to_properties(
        transport: &str,
        attrs: HashMap<TransportPropertiesAttribute, String>,
    ) -> AiServiceResult<NacosA2aTransportProperties> {
        let mut builder = NacosA2aTransportProperties::builder().transport(transport);
        for (attr, value) in attrs {
            match attr {
                TransportPropertiesAttribute::Host => builder = builder.host(value),
                TransportPropertiesAttribute::Port => {
                    let port = value.parse::<i32>().map_err(|_| {
                        AiServiceError::new(
                            AiServiceError::INVALID_PARAM,
                            format!("Invalid `port` value for transport `{transport}`: {value}"),
                        )
                    })?;
                    builder = builder.port(port);
                }
                TransportPropertiesAttribute::Path => builder = builder.path(value),
                TransportPropertiesAttribute::Protocol => builder = builder.protocol(value),
                TransportPropertiesAttribute::Query => builder = builder.query(value),
                TransportPropertiesAttribute::SupportTls => {
                    builder = builder.support_tls(value.parse::<bool>().unwrap_or(false))
                }
            }
        }
        // `transport` is always set above, so `build` cannot fail here.
        Ok(builder.build().unwrap_or_default())
    }
}

// ===========================================================================
// NacosA2aRegistry
// ===========================================================================

/// Registers A2A AgentCards and endpoints to Nacos, mirroring `NacosA2aRegistry`.
pub struct NacosA2aRegistry {
    service: std::sync::Arc<dyn NacosAiService>,
}

impl NacosA2aRegistry {
    pub fn new(service: std::sync::Arc<dyn NacosAiService>) -> Self {
        Self { service }
    }

    /// Registers an A2A AgentCard and its endpoints to Nacos.
    pub async fn register_agent(
        &self,
        agent_card: &A2aAgentCard,
        properties: &NacosA2aRegistryProperties,
    ) -> Result<(), String> {
        let nacos_card = convert_to_nacos_agent_card(agent_card);
        self.try_release_agent_card(&nacos_card, properties).await?;
        self.register_endpoints(&nacos_card, properties).await?;
        Ok(())
    }

    async fn try_release_agent_card(
        &self,
        card: &NacosAgentCard,
        properties: &NacosA2aRegistryProperties,
    ) -> Result<(), String> {
        if self
            .try_get_agent_card_from_nacos(card)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {
            warn!(
                agent = %card.name,
                "Agent card already exists, agentCard release might be ignored."
            );
        }
        info!(agent = %card.name, "Register agent card to Nacos.");
        self.service
            .release_agent_card(card, properties.is_set_as_latest)
            .await
            .map_err(|e| e.to_string())?;
        info!(agent = %card.name, "Register agent card to Nacos successfully.");
        Ok(())
    }

    async fn try_get_agent_card_from_nacos(
        &self,
        card: &NacosAgentCard,
    ) -> Result<Option<NacosAgentCard>, String> {
        match self
            .service
            .get_agent_card(&card.name, card.version.as_deref())
            .await
        {
            Ok(card) => Ok(card),
            Err(_) => Ok(None),
        }
    }

    async fn register_endpoints(
        &self,
        card: &NacosAgentCard,
        properties: &NacosA2aRegistryProperties,
    ) -> Result<(), String> {
        if !properties.enabled_register_endpoint {
            info!("Disabled register endpoint(s) to Agent, skip endpoint(s) register step.");
            return Ok(());
        }
        if properties.transport_properties.is_empty() {
            warn!("No endpoint(s) found, skip endpoint(s) register step.");
            return Ok(());
        }
        info!(
            count = properties.transport_properties.len(),
            "Register endpoint(s) to Nacos"
        );
        for transport in properties.transport_properties.values() {
            let endpoint = NacosAgentEndpoint::from_transport(transport, card.version.as_deref());
            self.service
                .register_agent_endpoint(&card.name, &endpoint)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

// ===========================================================================
// AgentRegistry trait + NacosAgentRegistry
// ===========================================================================

/// Registration strategy for A2A transports, mirroring
/// `io.agentscope.core.a2a.server.transport.TransportProperties`.
#[derive(Debug, Clone, PartialEq)]
pub struct TransportProperties {
    pub transport_type: String,
    pub host: String,
    pub port: i32,
    pub path: Option<String>,
    pub support_tls: bool,
}

impl TransportProperties {
    pub fn new(transport_type: impl Into<String>, host: impl Into<String>, port: i32) -> Self {
        Self {
            transport_type: transport_type.into(),
            host: host.into(),
            port,
            path: None,
            support_tls: false,
        }
    }
}

/// An A2A agent registry, mirroring `io.agentscope.core.a2a.server.registry.AgentRegistry`.
#[async_trait]
pub trait AgentRegistry: Send + Sync {
    /// Stable registry name (mirrors `registryName()`).
    fn registry_name(&self) -> String;

    /// Registers an AgentCard together with deployed transport properties.
    async fn register(
        &self,
        agent_card: &A2aAgentCard,
        transport_properties: Vec<TransportProperties>,
    ) -> Result<(), String>;
}

/// The [`AgentRegistry`] implementation for Nacos, mirroring `NacosAgentRegistry`.
pub struct NacosAgentRegistry {
    registry: NacosA2aRegistry,
    properties: NacosA2aRegistryProperties,
}

impl NacosAgentRegistry {
    /// Starts a builder, mirroring `builder(aiService)`.
    pub fn builder(service: std::sync::Arc<dyn NacosAiService>) -> NacosAgentRegistryBuilder {
        NacosAgentRegistryBuilder {
            service,
            properties: None,
        }
    }
}

/// Builder for [`NacosAgentRegistry`].
pub struct NacosAgentRegistryBuilder {
    service: std::sync::Arc<dyn NacosAiService>,
    properties: Option<NacosA2aRegistryProperties>,
}

impl NacosAgentRegistryBuilder {
    pub fn nacos_a2a_properties(mut self, p: NacosA2aRegistryProperties) -> Self {
        self.properties = Some(p);
        self
    }

    pub fn build(self) -> NacosAgentRegistry {
        NacosAgentRegistry {
            registry: NacosA2aRegistry::new(self.service),
            properties: self.properties.unwrap_or_default(),
        }
    }
}

#[async_trait]
impl AgentRegistry for NacosAgentRegistry {
    fn registry_name(&self) -> String {
        "Nacos".to_string()
    }

    async fn register(
        &self,
        agent_card: &A2aAgentCard,
        transport_properties: Vec<TransportProperties>,
    ) -> Result<(), String> {
        let mut target = NacosA2aRegistryProperties::builder_from(&self.properties).build();
        for transport in self.build_transport_properties(transport_properties)? {
            target.add_transport(transport);
        }
        let card = self.try_overwrite_preferred_transport(agent_card, &target);
        self.registry.register_agent(&card, &target).await
    }
}

impl NacosAgentRegistry {
    fn build_transport_properties(
        &self,
        transport_properties: Vec<TransportProperties>,
    ) -> Result<Vec<NacosA2aTransportProperties>, String> {
        let mut result: HashMap<String, NacosA2aTransportProperties> = transport_properties
            .into_iter()
            .filter_map(|p| {
                NacosA2aTransportProperties::builder()
                    .transport(p.transport_type)
                    .host(p.host)
                    .port(p.port)
                    .path(p.path.unwrap_or_default())
                    .support_tls(p.support_tls)
                    .build()
                    .ok()
            })
            .map(|p| (p.transport.to_uppercase(), p))
            .collect();

        for (transport, from_env) in
            NacosA2aTransportPropertiesEnvParser::from_env().map_err(|e| e.to_string())?
        {
            let key = transport.to_uppercase();
            match result.remove(&key) {
                Some(old) => {
                    let merged = overwrite_attributes(Some(old), from_env, &key);
                    result.insert(key, merged);
                }
                None => {
                    warn!(
                        transport = %key,
                        "Transport is not exported by agentscope, it might cause agentCard to \
                         include an unavailable endpoint."
                    );
                    let merged = overwrite_attributes(None, from_env, &key);
                    result.insert(key, merged);
                }
            }
        }
        Ok(result.into_values().collect())
    }

    fn try_overwrite_preferred_transport(
        &self,
        agent_card: &A2aAgentCard,
        properties: &NacosA2aRegistryProperties,
    ) -> A2aAgentCard {
        let Some(configured) = self
            .properties
            .overwrite_preferred_transport
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        else {
            return agent_card.clone();
        };
        let preferred_transport = configured.to_uppercase();
        info!(
            from = ?agent_card.preferred_transport,
            to = %preferred_transport,
            "Try to overwrite preferred transport"
        );
        match properties.transport_properties.get(&preferred_transport) {
            Some(transport) => do_overwrite(agent_card, transport),
            None => {
                warn!(
                    preferred_transport = %preferred_transport,
                    "Preferred transport is not found, will use original preferred transport"
                );
                agent_card.clone()
            }
        }
    }
}

/// Merges env-sourced attributes into deploy-sourced ones, mirroring
/// `NacosAgentRegistry#overwriteAttributes`.
fn overwrite_attributes(
    old: Option<NacosA2aTransportProperties>,
    new: NacosA2aTransportProperties,
    transport: &str,
) -> NacosA2aTransportProperties {
    let mut builder = NacosA2aTransportProperties::builder().transport(transport);
    if let Some(old) = old {
        builder = builder
            .host(old.host)
            .port(old.port)
            .support_tls(old.support_tls);
        if let Some(p) = old.path {
            builder = builder.path(p);
        }
        if let Some(p) = old.protocol {
            builder = builder.protocol(p);
        }
        if let Some(q) = old.query {
            builder = builder.query(q);
        }
    }
    if !new.host.trim().is_empty() {
        builder = builder.host(new.host);
    }
    if new.port > 0 {
        builder = builder.port(new.port);
    }
    if let Some(p) = new.path.filter(|s| !s.trim().is_empty()) {
        builder = builder.path(p);
    }
    if let Some(p) = new.protocol.filter(|s| !s.trim().is_empty()) {
        builder = builder.protocol(p);
    }
    if let Some(q) = new.query.filter(|s| !s.trim().is_empty()) {
        builder = builder.query(q);
    }
    builder = builder.support_tls(new.support_tls);
    builder.build().unwrap_or_default()
}

/// Rewrites `preferredTransport` / `url` on the card and appends the new
/// interface, mirroring `NacosAgentRegistry#doOverwrite`.
fn do_overwrite(
    agent_card: &A2aAgentCard,
    transport: &NacosA2aTransportProperties,
) -> A2aAgentCard {
    let new_url = generate_new_url(transport);
    let transport_name = transport.transport.clone();
    let mut card = agent_card.clone();
    card.url = new_url.clone();
    card.preferred_transport = Some(transport_name.clone());
    let ifaces = card.additional_interfaces.get_or_insert_with(Vec::new);
    ifaces.push(super::model::A2aAgentInterface {
        url: new_url.clone(),
        transport: transport_name,
    });
    card
}

/// Builds `[protocol://]host:port[/path][?query]`, mirroring
/// `NacosAgentRegistry#generateNewUrl`.
fn generate_new_url(transport: &NacosA2aTransportProperties) -> String {
    let protocol = transport.effective_protocol();
    let mut url = format!("{}://{}:{}", protocol, transport.host, transport.port);
    if let Some(path) = transport.path.as_deref().filter(|p| !p.trim().is_empty()) {
        url.push_str(if path.starts_with('/') { path } else { "/" });
        if !path.starts_with('/') {
            url.push_str(path);
        }
    }
    if let Some(query) = transport.query.as_deref().filter(|q| !q.trim().is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::model::{A2aAgentCard, NacosAgentCard};
    use crate::service::InMemoryNacosAiService;
    use std::sync::Arc;

    #[test]
    fn transport_builder_requires_transport() {
        assert!(NacosA2aTransportProperties::builder().build().is_err());
        let ok = NacosA2aTransportProperties::builder()
            .transport("jsonrpc")
            .host("127.0.0.1")
            .port(8080)
            .build()
            .unwrap();
        assert_eq!(ok.transport, "jsonrpc");
    }

    #[test]
    fn effective_protocol_upgrades_http_to_https_when_tls() {
        let t = NacosA2aTransportProperties::builder()
            .transport("jsonrpc")
            .host("h")
            .port(1)
            .support_tls(true)
            .build()
            .unwrap();
        assert_eq!(t.effective_protocol(), "https");
        let t2 = NacosA2aTransportProperties::builder()
            .transport("jsonrpc")
            .host("h")
            .port(1)
            .support_tls(false)
            .build()
            .unwrap();
        assert_eq!(t2.effective_protocol(), "http");
    }

    #[test]
    fn env_parser_groups_by_transport() {
        let env = vec![
            ("NACOS_A2A_AGENT_JSONRPC_HOST", "127.0.0.1"),
            ("NACOS_A2A_AGENT_JSONRPC_PORT", "8080"),
            ("NACOS_A2A_AGENT_HTTP_SUPPORTTLS", "true"),
            ("UNRELATED", "x"),
            ("NACOS_A2A_AGENT_BAD", "x"),
        ];
        let parsed = NacosA2aTransportPropertiesEnvParser::parse(env).unwrap();
        let jsonrpc = parsed.get("JSONRPC").unwrap();
        assert_eq!(jsonrpc.host, "127.0.0.1");
        assert_eq!(jsonrpc.port, 8080);
        assert_eq!(parsed.get("HTTP").unwrap().support_tls, true);
        assert!(!parsed.contains_key("BAD"));
    }

    #[test]
    fn env_parser_rejects_invalid_port() {
        // Mirrors Java's `NacosRuntimeException(NacosException.INVALID_PARAM, ..)`.
        let env = vec![("NACOS_A2A_AGENT_JSONRPC_PORT", "no_number")];
        let err = NacosA2aTransportPropertiesEnvParser::parse(env).unwrap_err();
        assert_eq!(err.code, AiServiceError::INVALID_PARAM);
        assert!(err
            .message
            .contains("Invalid `port` value for transport `JSONRPC`: no_number"));
    }

    #[test]
    fn generate_new_url_includes_path_and_query() {
        let t = NacosA2aTransportProperties {
            transport: "JSONRPC".into(),
            host: "h".into(),
            port: 9,
            path: Some("/api".into()),
            support_tls: false,
            protocol: None,
            query: Some("a=1".into()),
        };
        assert_eq!(generate_new_url(&t), "http://h:9/api?a=1");
    }

    #[tokio::test]
    async fn registers_card_and_endpoints() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let registry = NacosA2aRegistry::new(svc.clone());
        let card = A2aAgentCard::new("agent", "http://localhost:8080");
        let mut props = NacosA2aRegistryProperties::default();
        props.add_transport(
            NacosA2aTransportProperties::builder()
                .transport("JSONRPC")
                .host("localhost")
                .port(8080)
                .build()
                .unwrap(),
        );
        props.is_set_as_latest = true;
        registry.register_agent(&card, &props).await.unwrap();

        assert_eq!(svc.released_cards(), vec![("agent".to_string(), true)]);
        let endpoints = svc.registered_endpoints();
        assert_eq!(endpoints.len(), 1);
        // Endpoint mirrors Java's `buildAgentEndpoint`: address/port + card version.
        assert_eq!(endpoints[0].0, "agent");
        assert_eq!(endpoints[0].1.address.as_deref(), Some("localhost"));
        assert_eq!(endpoints[0].1.port, 8080);
        assert_eq!(endpoints[0].1.transport, "JSONRPC");
    }

    #[tokio::test]
    async fn skips_endpoints_when_disabled() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let registry = NacosA2aRegistry::new(svc.clone());
        let card = A2aAgentCard::new("agent", "http://localhost:8080");
        let props = NacosA2aRegistryProperties::builder()
            .enabled_register_endpoint(false)
            .build();
        registry.register_agent(&card, &props).await.unwrap();
        assert!(svc.registered_endpoints().is_empty());
    }

    #[tokio::test]
    async fn agent_registry_overwrites_preferred_transport() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let registry = NacosAgentRegistry::builder(svc.clone())
            .nacos_a2a_properties(
                NacosA2aRegistryProperties::builder()
                    .overwrite_preferred_transport("HTTP")
                    .build(),
            )
            .build();
        assert_eq!(registry.registry_name(), "Nacos");

        let card = A2aAgentCard::new("agent", "http://orig:1");
        let transports = vec![TransportProperties {
            transport_type: "HTTP".into(),
            host: "newhost".into(),
            port: 8443,
            path: Some("/a2a".into()),
            support_tls: true,
        }];
        registry.register(&card, transports).await.unwrap();

        let released = svc.released_cards();
        assert_eq!(released.len(), 1);
        let card = svc.get_agent_card("agent", None).await.unwrap().unwrap();
        assert_eq!(card.url, "https://newhost:8443/a2a");
        assert_eq!(card.preferred_transport.as_deref(), Some("HTTP"));

        // The registered endpoint carries the rewritten transport/address/port.
        let endpoints = svc.registered_endpoints();
        assert_eq!(endpoints[0].0, "agent");
        assert_eq!(endpoints[0].1.transport, "HTTP");
        assert_eq!(endpoints[0].1.address.as_deref(), Some("newhost"));
        assert_eq!(endpoints[0].1.port, 8443);
        assert!(endpoints[0].1.support_tls);
        let _ = NacosAgentCard::new("x", "y");
    }
}
