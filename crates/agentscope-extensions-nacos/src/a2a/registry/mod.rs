//! Nacos A2A registry, mirroring `io.agentscope.core.nacos.a2a.registry`.

pub mod constants;

use std::collections::HashMap;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::service::{AiServiceError, AiServiceResult, NacosAiService, A2A_ENDPOINT_TYPE_SERVICE};

use super::converter::{
    apply_supported_interfaces, convert_to_nacos_agent_card, supported_interfaces,
};
use super::model::{A2aAgentCard, A2aAgentInterface, NacosAgentCard, NacosAgentEndpoint};
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

    /// Replays the initial registration once (one heartbeat tick, NACOS-07).
    ///
    /// The payloads are exactly those of [`Self::register_agent`] — same card, same endpoint
    /// set — so a card or endpoint the server dropped is restored, and a still-registered one
    /// is simply overwritten. The "card already exists" probe of the initial registration is
    /// skipped, so a tick costs one `POST` per artifact instead of a `GET` + `POST`.
    ///
    /// Java relies on the Nacos SDK keeping a long-lived connection to the server for instance
    /// health; a plain HTTP client has no such connection, so the host repeats the calls (see
    /// [`Self::spawn_heartbeat`] for the built-in loop).
    pub async fn heartbeat(
        &self,
        agent_card: &A2aAgentCard,
        properties: &NacosA2aRegistryProperties,
    ) -> Result<(), String> {
        let nacos_card = convert_to_nacos_agent_card(agent_card);
        self.release_agent_card(&nacos_card, properties).await?;
        self.register_endpoints(&nacos_card, properties).await?;
        Ok(())
    }

    /// Replays [`Self::heartbeat`] every `interval` until the returned handle is aborted.
    ///
    /// A failed tick is logged and retried with exponential backoff
    /// ([`heartbeat_backoff`], capped at [`MAX_HEARTBEAT_BACKOFF`]); a successful tick resets
    /// the schedule to `interval`. `interval` is injectable, so the loop is testable with
    /// millisecond ticks.
    pub fn spawn_heartbeat(
        self: &std::sync::Arc<Self>,
        agent_card: A2aAgentCard,
        properties: NacosA2aRegistryProperties,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let registry = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                match registry.heartbeat(&agent_card, &properties).await {
                    Ok(()) => failures = 0,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        warn!(
                            agent = %agent_card.name,
                            failures,
                            error = %error,
                            "Nacos heartbeat failed, retrying with backoff"
                        );
                    }
                }
                let delay = heartbeat_backoff(interval, failures);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        })
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
        self.release_agent_card(card, properties).await?;
        info!(agent = %card.name, "Register agent card to Nacos successfully.");
        Ok(())
    }

    /// Publishes the card, mirroring `NacosA2aRegistry#releaseAgentCard`
    /// (`card`, `A2A_ENDPOINT_TYPE_SERVICE`, `setAsLatest`).
    async fn release_agent_card(
        &self,
        card: &NacosAgentCard,
        properties: &NacosA2aRegistryProperties,
    ) -> Result<(), String> {
        self.service
            .release_agent_card(card, A2A_ENDPOINT_TYPE_SERVICE, properties.is_set_as_latest)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Looks up the card before publishing, mirroring 3.3.0's
    /// `tryGetAgentCardFromNacos`: only `NOT_FOUND` means "absent"; any other Nacos failure is
    /// propagated with its original code and message instead of being treated as missing.
    async fn try_get_agent_card_from_nacos(
        &self,
        card: &NacosAgentCard,
    ) -> AiServiceResult<Option<NacosAgentCard>> {
        match self
            .service
            .get_agent_card(&card.name, card.version.as_deref())
            .await
        {
            Ok(card) => Ok(card),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error),
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
        // Java collects the endpoints into a `Set` when more than one transport is
        // configured, so identical endpoints are registered exactly once.
        let mut endpoints: Vec<NacosAgentEndpoint> = Vec::new();
        for transport in properties.transport_properties.values() {
            let endpoint = NacosAgentEndpoint::from_transport(transport, card.version.as_deref());
            if !endpoints.contains(&endpoint) {
                endpoints.push(endpoint);
            }
        }
        for endpoint in &endpoints {
            self.service
                .register_agent_endpoint(&card.name, endpoint)
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
///
/// The protocol side ships an equivalent
/// `agentscope_extensions_a2a_server::card::TransportProperties`, but that crate
/// pulls in `axum` and models `host`/`port` as `Option`s with an `extra` map — this
/// crate only feeds `AgentRegistry::register`, so the minimal shape is kept here
/// (see tracker §4.2 for the dedup request).
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

// ===========================================================================
// Protocol-side interop (NACOS-01)
// ===========================================================================

/// Implements the **protocol** registry interface
/// (`agentscope_extensions_a2a_server::registry::AgentRegistry`), so the A2A server's
/// `AgentRegistryService` can register a served agent into Nacos ("post-endpoint-ready").
///
/// Mirrors Java `NacosAgentRegistry implements io.agentscope.core.a2a.server.registry.AgentRegistry`.
/// The protocol trait is synchronous (like Java's `void register(..)`) while
/// [`NacosAiService`] is async, so the call is driven to completion with
/// [`futures::executor::block_on`]; failures are reported per registry by
/// `AgentRegistryService` instead of aborting the other registries.
impl agentscope_extensions_a2a_server::registry::AgentRegistry for NacosAgentRegistry {
    fn registry_name(&self) -> String {
        <Self as AgentRegistry>::registry_name(self)
    }

    fn register(
        &self,
        agent_card: &agentscope_extensions_a2a_client::model::AgentCard,
        transport_properties: &[agentscope_extensions_a2a_client::transport_properties::TransportProperties],
    ) -> Result<(), agentscope_extensions_a2a_server::registry::AgentRegistryError> {
        let card = A2aAgentCard {
            inner: agent_card.clone(),
        };
        let transports = transport_properties
            .iter()
            .map(protocol_transport_to_local)
            .collect::<Result<Vec<_>, _>>()?;
        futures::executor::block_on(<Self as AgentRegistry>::register(self, &card, transports))
            .map_err(
                |error| -> agentscope_extensions_a2a_server::registry::AgentRegistryError {
                    error.into()
                },
            )
    }
}

/// Converts protocol-side transport properties into this crate's **required-shape** version.
///
/// The protocol type models `host`/`port` as optional (with `443`/`80` defaults) because it
/// also *describes* a server; registration only ever feeds endpoints that are actually
/// deployed, so a missing host is reported instead of silently registering an empty address
/// (see NACOS-X08). The port falls back to [`effective_port`] (`443`/`80`), matching the
/// protocol's own interface synthesis.
///
/// [`effective_port`]: agentscope_extensions_a2a_client::transport_properties::TransportProperties::effective_port
fn protocol_transport_to_local(
    transport: &agentscope_extensions_a2a_client::transport_properties::TransportProperties,
) -> Result<TransportProperties, String> {
    let host = transport
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| {
            format!(
                "A2A endpoint `{}` has no `host`, cannot register it to Nacos.",
                transport.transport_type
            )
        })?;
    Ok(TransportProperties {
        transport_type: transport.transport_type.clone(),
        host: host.to_string(),
        port: i32::from(transport.effective_port()),
        path: transport
            .path
            .clone()
            .filter(|path| !path.trim().is_empty()),
        support_tls: transport.support_tls,
    })
}

impl NacosAgentRegistry {
    fn build_transport_properties(
        &self,
        transport_properties: Vec<TransportProperties>,
    ) -> Result<Vec<NacosA2aTransportProperties>, String> {
        // A transport rejected by the builder (empty `transport`) fails the whole
        // registration instead of being silently dropped, mirroring Java where the builder's
        // `IllegalArgumentException` is not caught and `register` fails (NACOS-08).
        let mut result: HashMap<String, NacosA2aTransportProperties> = transport_properties
            .into_iter()
            .map(|p| {
                let transport_type = p.transport_type.clone();
                NacosA2aTransportProperties::builder()
                    .transport(p.transport_type)
                    .host(p.host)
                    .port(p.port)
                    .path(p.path.unwrap_or_default())
                    .support_tls(p.support_tls)
                    .build()
                    .map(|built| (built.transport.to_uppercase(), built))
                    .map_err(|error| format!("Invalid A2A transport `{transport_type}`: {error}"))
            })
            .collect::<Result<_, String>>()?;

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

/// Upper bound of the heartbeat backoff (NACOS-07).
pub const MAX_HEARTBEAT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Delay before the next [`NacosA2aRegistry::heartbeat`] tick.
///
/// `interval` while the last tick succeeded; exponential (capped at
/// [`MAX_HEARTBEAT_BACKOFF`]) after consecutive failures. Pure so the schedule is testable
/// without sleeping.
pub fn heartbeat_backoff(interval: std::time::Duration, failures: u32) -> std::time::Duration {
    crate::service::exponential_backoff(interval, failures, MAX_HEARTBEAT_BACKOFF)
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

/// Rewrites the preferred interface on the card, mirroring
/// `NacosAgentRegistry#doOverwrite`.
///
/// Java rewrites `supportedInterfaces`: the new interface is removed when already
/// present (`removeIf(agentInterface::equals)`) and then inserted at index 0, so
/// the preferred interface is listed exactly once and comes first. In the A2A 0.3
/// shape used here that is `url` / `preferredTransport` (first entry) plus
/// `additionalInterfaces` (the rest).
fn do_overwrite(
    agent_card: &A2aAgentCard,
    transport: &NacosA2aTransportProperties,
) -> A2aAgentCard {
    let new_url = generate_new_url(transport);
    let new_interface = A2aAgentInterface {
        url: new_url,
        transport: transport.transport.clone(),
    };
    let mut interfaces = supported_interfaces(agent_card);
    interfaces.retain(|each| each != &new_interface);
    interfaces.insert(0, new_interface);

    let mut card = agent_card.clone();
    apply_supported_interfaces(&mut card, &interfaces);
    card
}

/// Builds `[protocol://]host:port[/path][?query]`, mirroring
/// `NacosAgentRegistry#generateNewUrl`.
///
/// The host goes through [`format_host_for_url`] first, like Java's
/// `NetworkUtils.formatHostForUrl` (NACOS-09).
fn generate_new_url(transport: &NacosA2aTransportProperties) -> String {
    let protocol = transport.effective_protocol();
    let mut url = format!(
        "{}://{}:{}",
        protocol,
        format_host_for_url(&transport.host),
        transport.port
    );
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

/// Brackets a bare IPv6 literal, mirroring `NetworkUtils#formatHostForUrl`.
///
/// `::1` must become `[::1]`, otherwise `http://::1:8080` is not a valid URL.
fn format_host_for_url(host: &str) -> String {
    let already_bracketed = host.starts_with('[') && host.ends_with(']');
    if host.contains(':') && !already_bracketed {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::model::{A2aAgentCard, NacosAgentCard};
    use crate::service::InMemoryNacosAiService;
    use std::sync::Arc;
    use std::time::Duration;

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
        assert_eq!(err.code(), AiServiceError::INVALID_PARAM);
        assert!(err
            .message()
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

    /// NACOS-09: an IPv6 literal is bracketed, otherwise the port separator makes the URL
    /// invalid (`http://::1:8080`).
    #[test]
    fn generate_new_url_brackets_ipv6_hosts() {
        let url = |host: &str| {
            let t = NacosA2aTransportProperties::builder()
                .transport("JSONRPC")
                .host(host)
                .port(8080)
                .build()
                .unwrap();
            generate_new_url(&t)
        };
        assert_eq!(url("::1"), "http://[::1]:8080");
        assert_eq!(url("[::1]"), "http://[::1]:8080");
        assert_eq!(url("127.0.0.1"), "http://127.0.0.1:8080");
    }

    /// NACOS-08: a transport the builder rejects fails the registration instead of being
    /// silently dropped (Java: uncaught `IllegalArgumentException`).
    #[tokio::test]
    async fn an_invalid_transport_fails_the_registration() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let registry = NacosAgentRegistry::builder(svc.clone()).build();
        let card = A2aAgentCard::new("agent", "http://localhost:8080");

        let error = registry
            .register(
                &card,
                vec![TransportProperties::new("   ", "localhost", 8080)],
            )
            .await
            .unwrap_err();
        assert!(error.contains("`transport`"), "{error}");
        assert!(svc.released_cards().is_empty());
        assert!(svc.registered_endpoints().is_empty());
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
        // NACOS-05: the release carries `endpointType=service`, like Java's
        // `releaseAgentCard(card, AiConstants.A2a.A2A_ENDPOINT_TYPE_SERVICE, setAsLatest)`.
        assert_eq!(svc.released_cards_detailed()[0].endpoint_type, "service");
        let endpoints = svc.registered_endpoints();
        assert_eq!(endpoints.len(), 1);
        // Endpoint mirrors Java's `buildAgentEndpoint`: address/port + card version.
        assert_eq!(endpoints[0].0, "agent");
        assert_eq!(endpoints[0].1.address.as_deref(), Some("localhost"));
        assert_eq!(endpoints[0].1.port, 8080);
        assert_eq!(endpoints[0].1.transport, "JSONRPC");
    }

    /// Nacos 3.3.0: only `NOT_FOUND` means "the card does not exist yet"; any other lookup
    /// failure is propagated with its original code instead of being treated as missing.
    #[tokio::test]
    async fn a_failed_card_lookup_is_not_treated_as_missing() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card_error(Some(AiServiceError::new(503, "nacos unavailable")));
        let registry = NacosA2aRegistry::new(svc.clone());
        let card = A2aAgentCard::new("agent", "http://localhost:8080");

        let error = registry
            .register_agent(&card, &NacosA2aRegistryProperties::default())
            .await
            .unwrap_err();
        assert!(error.contains("503"), "{error}");
        assert!(error.contains("nacos unavailable"), "{error}");
        // The publish step never ran.
        assert!(svc.released_cards().is_empty());
    }

    /// A `NOT_FOUND` lookup is the normal "first registration" case and must not fail it.
    #[tokio::test]
    async fn a_not_found_lookup_still_registers_the_card() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card_error(Some(AiServiceError::not_found("no such card")));
        let registry = NacosA2aRegistry::new(svc.clone());
        let card = A2aAgentCard::new("agent", "http://localhost:8080");

        registry
            .register_agent(&card, &NacosA2aRegistryProperties::default())
            .await
            .unwrap();
        assert_eq!(svc.released_cards(), vec![("agent".to_string(), false)]);
    }

    /// NACOS-01: the protocol `AgentRegistryService` drives this registry end to end.
    #[test]
    fn the_protocol_registry_service_registers_the_agent_into_nacos() {
        use agentscope_extensions_a2a_client::transport_properties::TransportProperties as ProtocolTransportProperties;
        use agentscope_extensions_a2a_server::registry::AgentRegistryService;

        let svc = Arc::new(InMemoryNacosAiService::new());
        let registry = Arc::new(
            NacosAgentRegistry::builder(svc.clone())
                .nacos_a2a_properties(
                    NacosA2aRegistryProperties::builder()
                        .set_as_latest(true)
                        .build(),
                )
                .build(),
        );
        let service = AgentRegistryService::new(vec![registry]);

        let card = A2aAgentCard::new("agent", "http://localhost:8080");
        let transports = vec![ProtocolTransportProperties::builder("JSONRPC")
            .host("10.0.0.1")
            .port(9000)
            .path("/a2a")
            .build()];

        let report = service.register(&card.inner, &transports);
        assert_eq!(report.succeeded, vec!["Nacos".to_string()]);
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(svc.released_cards(), vec![("agent".to_string(), true)]);
        let endpoints = svc.registered_endpoints();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].1.address.as_deref(), Some("10.0.0.1"));
        assert_eq!(endpoints[0].1.port, 9000);
        assert_eq!(endpoints[0].1.path.as_deref(), Some("/a2a"));

        // A transport without a host fails the registration instead of registering `""`.
        let broken = vec![ProtocolTransportProperties::builder("GRPC").build()];
        let report = service.register(&card.inner, &broken);
        assert!(report.succeeded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "Nacos");
        assert!(report.failed[0].1.contains("host"), "{report:?}");
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
        assert_eq!(card.url.as_deref(), Some("https://newhost:8443/a2a"));
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

    #[test]
    fn do_overwrite_dedups_and_prepends_the_interface() {
        // Mirrors Java `doOverwrite`: `removeIf(agentInterface::equals)` + `add(0, ..)`.
        let mut card = A2aAgentCard::new("agent", "http://orig:1");
        card.preferred_transport = Some("JSONRPC".into());
        card.additional_interfaces = Some(vec![A2aAgentInterface {
            url: "http://orig:1/grpc".into(),
            transport: "GRPC".into(),
        }]);
        let transport = NacosA2aTransportProperties::builder()
            .transport("HTTP")
            .host("newhost")
            .port(8443)
            .support_tls(true)
            .build()
            .unwrap();

        let overwritten = do_overwrite(&card, &transport);
        assert_eq!(overwritten.url, "https://newhost:8443");
        assert_eq!(overwritten.preferred_transport.as_deref(), Some("HTTP"));
        assert_eq!(
            supported_interfaces(&overwritten),
            vec![
                A2aAgentInterface {
                    url: "https://newhost:8443".into(),
                    transport: "HTTP".into(),
                },
                A2aAgentInterface {
                    url: "http://orig:1".into(),
                    transport: "JSONRPC".into(),
                },
                A2aAgentInterface {
                    url: "http://orig:1/grpc".into(),
                    transport: "GRPC".into(),
                },
            ]
        );

        // Overwriting again with the same transport must not add a duplicate.
        let again = do_overwrite(&overwritten, &transport);
        assert_eq!(supported_interfaces(&again).len(), 3);
        assert_eq!(again.url, "https://newhost:8443");
    }

    #[tokio::test]
    async fn duplicate_endpoints_are_registered_once() {
        // Java collects the endpoints into a `Set`; two transport entries that
        // resolve to the same endpoint must only be registered once.
        let svc = Arc::new(InMemoryNacosAiService::new());
        let registry = NacosA2aRegistry::new(svc.clone());
        let card = A2aAgentCard::new("agent", "http://localhost:8080");
        let transport = NacosA2aTransportProperties::builder()
            .transport("JSONRPC")
            .host("localhost")
            .port(8080)
            .build()
            .unwrap();
        let mut props = NacosA2aRegistryProperties::default();
        props
            .transport_properties
            .insert("JSONRPC".into(), transport.clone());
        props
            .transport_properties
            .insert("JSON-RPC-ALIAS".into(), transport);

        registry.register_agent(&card, &props).await.unwrap();
        assert_eq!(svc.registered_endpoints().len(), 1);
    }

    /// NACOS-07: a heartbeat replays exactly the payloads of the initial registration, so a
    /// server-side removal is repaired without a duplicated/empty endpoint.
    #[tokio::test]
    async fn a_heartbeat_replays_the_registration_payload() {
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

        registry.register_agent(&card, &props).await.unwrap();
        registry.heartbeat(&card, &props).await.unwrap();

        let released = svc.released_cards_detailed();
        assert_eq!(released.len(), 2);
        assert_eq!(released[0], released[1]);
        assert_eq!(released[1].endpoint_type, "service");

        // The endpoint is replayed verbatim (same transport/address/port, still exactly one).
        let endpoints = svc.registered_endpoints();
        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].1, endpoints[1].1);
        assert_eq!(endpoints[1].1.port, 8080);
    }

    /// NACOS-07: the heartbeat loop keeps ticking (with an injectable interval) until aborted.
    #[tokio::test]
    async fn the_heartbeat_loop_ticks_until_it_is_aborted() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let registry = Arc::new(NacosA2aRegistry::new(svc.clone()));
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

        let handle = registry.spawn_heartbeat(card, props, Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(60)).await;
        handle.abort();

        let released = svc.released_cards_detailed();
        assert!(
            released.len() >= 2,
            "the heartbeat ticked {} time(s)",
            released.len()
        );
        assert!(released.iter().all(|card| card == &released[0]));
        assert_eq!(svc.registered_endpoints().len(), released.len());
    }

    /// NACOS-07: the heartbeat backs off exponentially after a failure and resets on success.
    #[test]
    fn the_heartbeat_backoff_grows_and_is_capped() {
        let interval = Duration::from_secs(1);
        assert_eq!(heartbeat_backoff(interval, 0), interval);
        assert_eq!(heartbeat_backoff(interval, 1), Duration::from_secs(2));
        assert_eq!(heartbeat_backoff(interval, 2), Duration::from_secs(4));
        assert_eq!(heartbeat_backoff(interval, 40), MAX_HEARTBEAT_BACKOFF);
    }
}
