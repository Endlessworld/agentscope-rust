//! The A2A server bootstrap layer, mirroring
//! `io.agentscope.core.a2a.server.AgentScopeA2aServer`.
//!
//! Java's entry class for exporting an agent over A2A assembles the agent card, the request
//! handler, one [`TransportWrapper`] per supported transport and the registry service, then
//! exposes them to whatever web server the host runs:
//!
//! ```text
//! let server = AgentScopeA2aServer::builder(runner)
//!     .deployment_properties(DeploymentProperties::builder().port(8080).build()?)
//!     .build()?;
//! let router = server.router();          // mount on the host web server
//! let report = server.post_endpoint_ready();  // register with discovery backends
//! ```
//!
//! Like Java, this type does not open ports: it assembles the pieces and lets the host
//! framework serve them. `close()` is therefore a no-op here (Rust keeps no handler threads).

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_extensions_a2a_client::model::AgentCard;

use crate::card::{create_agent_card, AgentDescriptor, ConfigurableAgentCard, TransportProperties};
use crate::executor::AgentExecuteProperties;
use crate::registry::{AgentRegistry, AgentRegistryService, RegistrationReport};
use crate::transport::{
    default_transport_builders, CustomTransportProperties, DeploymentProperties, TransportHeaders,
    TransportMetadata, TransportResponse, TransportWrapper, TransportWrapperBuilder,
};
use crate::{A2aContextResolver, A2aRequestHandler, A2aTaskRegistry, AgentRunner};

/// A failure while building an [`AgentScopeA2aServer`].
///
/// Java throws `IllegalArgumentException` for the same conditions; the typed errors keep the
/// messages greppable and let hosts react without string matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum A2aServerBuildError {
    /// No transport was configured and no deployment properties were supplied to fall back on
    /// (Java: "Not found any properties for transport.").
    NoTransportProperties,
    /// Every configured transport lacked a matching [`TransportWrapperBuilder`], or every
    /// wrapper failed to build (Java: "No one available transport found.").
    NoAvailableTransport,
}

impl std::fmt::Display for A2aServerBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            A2aServerBuildError::NoTransportProperties => {
                f.write_str("Not found any properties for transport.")
            }
            A2aServerBuildError::NoAvailableTransport => {
                f.write_str("No one available transport found.")
            }
        }
    }
}

impl std::error::Error for A2aServerBuildError {}

/// Builder for [`AgentScopeA2aServer`].
///
/// Mirrors `AgentScopeA2aServer.Builder`. **Deviation**: Java discovers
/// `TransportWrapperBuilder`s through `ServiceLoader`; Rust has no SPI scan, so the builder
/// starts with [`default_transport_builders`] (JSON-RPC) and accepts more through
/// [`transport_builder`](Self::transport_builder).
pub struct AgentScopeA2aServerBuilder {
    agent_runner: Arc<dyn AgentRunner>,
    agent_card: ConfigurableAgentCard,
    supported_transports: Vec<TransportProperties>,
    custom_transports: Vec<Box<dyn CustomTransportProperties>>,
    agent_registries: Vec<Arc<dyn AgentRegistry>>,
    transport_builders: Vec<Arc<dyn TransportWrapperBuilder>>,
    deployment_properties: Option<DeploymentProperties>,
    execute_properties: AgentExecuteProperties,
    task_registry: Option<Arc<A2aTaskRegistry>>,
    context_resolver: Option<Arc<dyn A2aContextResolver>>,
}

impl std::fmt::Debug for AgentScopeA2aServerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentScopeA2aServerBuilder")
            .field("supported_transports", &self.supported_transports)
            .field("agent_registries", &self.agent_registries.len())
            .field("transport_builders", &self.transport_builders.len())
            .field("deployment_properties", &self.deployment_properties)
            .field("execute_properties", &self.execute_properties)
            .finish_non_exhaustive()
    }
}

impl AgentScopeA2aServerBuilder {
    fn new(agent_runner: Arc<dyn AgentRunner>) -> Self {
        Self {
            agent_runner,
            agent_card: ConfigurableAgentCard::default(),
            supported_transports: Vec::new(),
            custom_transports: Vec::new(),
            agent_registries: Vec::new(),
            transport_builders: default_transport_builders(),
            deployment_properties: None,
            execute_properties: AgentExecuteProperties::default(),
            task_registry: None,
            context_resolver: None,
        }
    }

    /// Sets the developer-configured agent card.
    pub fn agent_card(mut self, agent_card: ConfigurableAgentCard) -> Self {
        self.agent_card = agent_card;
        self
    }

    /// Adds a supported transport and its endpoint properties.
    pub fn with_transport(mut self, transport_properties: TransportProperties) -> Self {
        if !self
            .supported_transports
            .iter()
            .any(|existing| existing.transport_type == transport_properties.transport_type)
        {
            self.supported_transports.push(transport_properties);
        }
        self
    }

    /// Registers an [`AgentRegistry`] to be called by
    /// [`AgentScopeA2aServer::post_endpoint_ready`].
    pub fn with_agent_registry(mut self, agent_registry: Arc<dyn AgentRegistry>) -> Self {
        self.agent_registries.push(agent_registry);
        self
    }

    /// Adds a custom transport (mirroring Java's `CustomTransportProperties` implementations).
    ///
    /// The properties are resolved at [`build`](Self::build) time: a disabled transport is
    /// skipped, and an enabled one receives the deployment properties before it contributes its
    /// [`TransportProperties`].
    pub fn with_custom_transport(
        mut self,
        custom_transport: Box<dyn CustomTransportProperties>,
    ) -> Self {
        self.custom_transports.push(custom_transport);
        self
    }

    /// Adds a transport wrapper builder (replacing the default one for the same type).
    pub fn transport_builder(mut self, builder: Arc<dyn TransportWrapperBuilder>) -> Self {
        self.transport_builders.push(builder);
        self
    }

    /// Sets the deployment properties used to synthesize the default transport.
    pub fn deployment_properties(mut self, deployment_properties: DeploymentProperties) -> Self {
        self.deployment_properties = Some(deployment_properties);
        self
    }

    /// Sets only the deployment port, mirroring Java
    /// `deploymentProperties(Integer port)`.
    pub fn deployment_port(self, port: u16) -> Self {
        self.deployment_properties(
            DeploymentProperties::builder()
                .port(port)
                .build()
                .expect("an explicit port is always valid"),
        )
    }

    /// Sets the agent execution properties.
    pub fn agent_execute_properties(mut self, execute_properties: AgentExecuteProperties) -> Self {
        self.execute_properties = execute_properties;
        self
    }

    /// Uses an existing task registry instead of a fresh in-memory one.
    pub fn task_registry(mut self, task_registry: Arc<A2aTaskRegistry>) -> Self {
        self.task_registry = Some(task_registry);
        self
    }

    /// Sets the deployment-side adjudication of the user identity of each call, e.g. from an
    /// mTLS certificate or an OAuth token in the request headers.
    ///
    /// A transport-verified user id overrides the one the client asserts in the message
    /// envelope; without a resolver the asserted one is used as-is. See
    /// [`A2aContextResolver`] for the precedence and the fail-closed contract.
    pub fn context_resolver(mut self, context_resolver: Arc<dyn A2aContextResolver>) -> Self {
        self.context_resolver = Some(context_resolver);
        self
    }

    /// Assembles the server.
    ///
    /// Mirrors Java `Builder#build`: normalize the transports, derive the agent card, build the
    /// request handler and one wrapper per available transport, then build the registry service.
    pub fn build(self) -> Result<AgentScopeA2aServer, A2aServerBuildError> {
        let mut available: Vec<TransportProperties> = self.supported_transports.clone();
        for mut custom in self.custom_transports {
            if !custom.is_enabled() {
                tracing::info!("custom transport is disabled, ignoring it");
                continue;
            }
            if let Some(deployment) = self.deployment_properties.clone() {
                custom.set_deployment_properties(deployment);
            }
            let properties = custom.to_transport_properties();
            if !available
                .iter()
                .any(|existing| existing.transport_type == properties.transport_type)
            {
                available.push(properties);
            }
        }
        if available.is_empty() {
            let Some(deployment) = self.deployment_properties.as_ref() else {
                return Err(A2aServerBuildError::NoTransportProperties);
            };
            let mut builder = TransportProperties::builder(
                crate::transport::JsonRpcTransportWrapper::TRANSPORT_TYPE,
            )
            .host(deployment.host())
            .port(deployment.port());
            if let Some(path) = deployment.path() {
                builder = builder.path(path);
            }
            available.push(builder.build());
        }

        // Later registrations win, mirroring Java's `Collectors.toMap` merge function.
        let mut builders: HashMap<String, Arc<dyn TransportWrapperBuilder>> = HashMap::new();
        for builder in self.transport_builders {
            builders.insert(builder.transport_type(), builder);
        }
        available.retain(|properties| {
            let supported = builders.contains_key(&properties.transport_type);
            if !supported {
                tracing::warn!(
                    transport = properties.transport_type.as_str(),
                    "no TransportWrapperBuilder for the configured transport, ignoring it"
                );
            }
            supported
        });
        if available.is_empty() {
            return Err(A2aServerBuildError::NoAvailableTransport);
        }

        let descriptor = RunnerDescriptor(self.agent_runner.as_ref());
        let agent_card = create_agent_card(&self.agent_card, &descriptor, &available);
        let mut request_handler = A2aRequestHandler::new_with_properties(
            self.agent_runner.clone(),
            agent_card.clone(),
            self.task_registry
                .unwrap_or_else(|| Arc::new(A2aTaskRegistry::new())),
            self.execute_properties,
        );
        if let Some(context_resolver) = self.context_resolver {
            request_handler = request_handler.with_context_resolver(context_resolver);
        }
        let request_handler = Arc::new(request_handler);

        let mut transport_wrappers: HashMap<String, Arc<dyn TransportWrapper>> = HashMap::new();
        for properties in &available {
            let Some(builder) = builders.get(&properties.transport_type) else {
                continue;
            };
            match builder.build(&agent_card, request_handler.clone()) {
                Ok(wrapper) => {
                    transport_wrappers.insert(properties.transport_type.clone(), wrapper);
                }
                Err(error) => tracing::warn!(
                    transport = properties.transport_type.as_str(),
                    error = error.to_string().as_str(),
                    "building the transport wrapper failed, ignoring this transport"
                ),
            }
        }
        if transport_wrappers.is_empty() {
            return Err(A2aServerBuildError::NoAvailableTransport);
        }

        Ok(AgentScopeA2aServer {
            transport_wrappers,
            transport_properties: available,
            agent_card,
            agent_registry: AgentRegistryService::new(self.agent_registries),
            request_handler,
        })
    }
}

/// Adapter so the runner can describe the agent for card defaults.
struct RunnerDescriptor<'a>(&'a dyn AgentRunner);

impl AgentDescriptor for RunnerDescriptor<'_> {
    fn agent_name(&self) -> String {
        self.0.agent_name()
    }

    fn agent_description(&self) -> String {
        self.0.agent_description()
    }
}

/// The assembled A2A server.
///
/// Mirrors `io.agentscope.core.a2a.server.AgentScopeA2aServer`.
pub struct AgentScopeA2aServer {
    transport_wrappers: HashMap<String, Arc<dyn TransportWrapper>>,
    transport_properties: Vec<TransportProperties>,
    agent_card: AgentCard,
    agent_registry: AgentRegistryService,
    request_handler: Arc<A2aRequestHandler>,
}

impl std::fmt::Debug for AgentScopeA2aServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentScopeA2aServer")
            .field("agent", &self.agent_card.name)
            .field("transports", &self.transport_properties)
            .field("registries", &self.agent_registry.registries().len())
            .finish_non_exhaustive()
    }
}

impl AgentScopeA2aServer {
    /// Returns a builder for the given agent runner.
    pub fn builder(agent_runner: Arc<dyn AgentRunner>) -> AgentScopeA2aServerBuilder {
        AgentScopeA2aServerBuilder::new(agent_runner)
    }

    /// Releases resources held by the handler.
    ///
    /// Java joins the request handler's queue threads; the Rust handler keeps no background
    /// threads, so this is a no-op kept for API parity.
    pub fn close(&self) {}

    /// The wrapper for `transport_name`, or `None` when that transport is not exported.
    pub fn transport_wrapper(&self, transport_name: &str) -> Option<&Arc<dyn TransportWrapper>> {
        self.transport_wrappers.get(transport_name)
    }

    /// The wrapper for `transport_name`, mirroring Java's
    /// `IllegalArgumentException("Transport <name> not found.")` on a miss.
    pub fn require_transport_wrapper(
        &self,
        transport_name: &str,
    ) -> Result<&Arc<dyn TransportWrapper>, String> {
        self.transport_wrapper(transport_name)
            .ok_or_else(|| format!("Transport {transport_name} not found."))
    }

    /// Names of the exported transports, in configuration order.
    pub fn transport_names(&self) -> Vec<String> {
        self.transport_properties
            .iter()
            .map(|properties| properties.transport_type.clone())
            .collect()
    }

    /// The transport properties the server actually exports.
    pub fn transport_properties(&self) -> &[TransportProperties] {
        &self.transport_properties
    }

    /// The served agent card.
    pub fn agent_card(&self) -> &AgentCard {
        &self.agent_card
    }

    /// The served agent card as JSON, mirroring Java `getAgentCardJson`.
    pub fn agent_card_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.agent_card)
    }

    /// The request handler shared by every transport wrapper.
    pub fn request_handler(&self) -> &Arc<A2aRequestHandler> {
        &self.request_handler
    }

    /// The registry service, exposed for hosts that register manually.
    pub fn agent_registry(&self) -> &AgentRegistryService {
        &self.agent_registry
    }

    /// Registers the agent with every configured registry.
    ///
    /// Mirrors Java `postEndpointReady()`: call it once the endpoints are reachable.
    pub fn post_endpoint_ready(&self) -> RegistrationReport {
        self.agent_registry
            .register(&self.agent_card, &self.transport_properties)
    }

    /// Handles a raw request through the named transport wrapper.
    ///
    /// This is the entry point a host controller calls, mirroring
    /// `AgentScopeA2aServer#getTransportWrapper(name).handleRequest(...)`.
    pub async fn handle_request(
        &self,
        transport_name: &str,
        body: &str,
        headers: &TransportHeaders,
        metadata: &TransportMetadata,
    ) -> Option<TransportResponse> {
        let wrapper = self.transport_wrapper(transport_name)?;
        Some(wrapper.handle_request(body, headers, metadata).await)
    }

    /// Builds an `axum` router serving the JSON-RPC endpoint and the well-known agent card.
    pub fn router(&self) -> axum::Router {
        crate::router(self.request_handler.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentRequestOptions, ServerMessageConvert};
    use agentscope_core::event::{AgentEvent, EventData};
    use agentscope_core::message::Msg;
    use agentscope_extensions_a2a_client::error::A2aError;
    use agentscope_extensions_a2a_client::model::{TaskState, TaskStatusUpdateEvent};
    use std::sync::Mutex;

    struct EchoRunner;

    #[async_trait::async_trait]
    impl AgentRunner for EchoRunner {
        fn agent_name(&self) -> String {
            "echo-agent".to_string()
        }

        fn agent_description(&self) -> String {
            "echoes the input".to_string()
        }

        async fn stream_events(
            &self,
            request_messages: Vec<Msg>,
            _options: AgentRequestOptions,
        ) -> Result<Vec<AgentEvent>, A2aError> {
            let echoed = request_messages
                .iter()
                .map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join("");
            let mut data = EventData::default();
            data.fields.insert(
                "content".to_string(),
                serde_json::json!([{ "type": "text", "text": format!("echo:{echoed}") }]),
            );
            Ok(vec![AgentEvent::AgentResult(data)])
        }

        fn stop(&self, _task_id: &str) {}
    }

    #[derive(Debug, Default)]
    struct RecordingRegistry {
        name: String,
        calls: Mutex<usize>,
    }

    impl AgentRegistry for RecordingRegistry {
        fn registry_name(&self) -> String {
            self.name.clone()
        }

        fn register(
            &self,
            _agent_card: &AgentCard,
            transport_properties: &[TransportProperties],
        ) -> Result<(), crate::registry::AgentRegistryError> {
            *self.calls.lock().unwrap() += transport_properties.len();
            Ok(())
        }
    }

    fn deployment() -> DeploymentProperties {
        DeploymentProperties::builder()
            .host("localhost")
            .port(8080)
            .build()
            .unwrap()
    }

    #[test]
    fn build_requires_transport_properties_or_a_deployment_port() {
        let error = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .build()
            .unwrap_err();
        assert_eq!(error, A2aServerBuildError::NoTransportProperties);
        assert_eq!(error.to_string(), "Not found any properties for transport.");
    }

    #[test]
    fn deployment_properties_synthesize_the_default_jsonrpc_transport() {
        let server = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .deployment_properties(deployment())
            .build()
            .unwrap();

        assert_eq!(server.transport_names(), vec!["JSONRPC".to_string()]);
        let wrapper = server
            .require_transport_wrapper("JSONRPC")
            .expect("JSON-RPC is built in");
        assert_eq!(wrapper.transport_type(), "JSONRPC");
        assert_eq!(server.agent_card().name, "echo-agent");
        assert_eq!(server.agent_card().url, "http://localhost:8080");
        assert_eq!(
            server.agent_card().preferred_transport.as_deref(),
            Some("JSONRPC")
        );

        let json = server.agent_card_json().unwrap();
        let card: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(card.name, "echo-agent");
    }

    #[test]
    fn configured_transports_without_a_builder_are_rejected() {
        let error = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .with_transport(
                TransportProperties::builder("GRPC")
                    .host("localhost")
                    .port(9000)
                    .build(),
            )
            .build()
            .unwrap_err();
        assert_eq!(error, A2aServerBuildError::NoAvailableTransport);
        assert_eq!(error.to_string(), "No one available transport found.");
    }

    #[test]
    fn custom_builders_can_add_transports() {
        struct PassthroughBuilder;

        impl TransportWrapperBuilder for PassthroughBuilder {
            fn transport_type(&self) -> String {
                "CUSTOM".to_string()
            }

            fn build(
                &self,
                _agent_card: &AgentCard,
                request_handler: Arc<A2aRequestHandler>,
            ) -> Result<Arc<dyn TransportWrapper>, crate::registry::AgentRegistryError>
            {
                Ok(Arc::new(crate::transport::JsonRpcTransportWrapper::new(
                    request_handler,
                )))
            }
        }

        let server = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .with_transport(
                TransportProperties::builder("CUSTOM")
                    .host("localhost")
                    .port(9100)
                    .build(),
            )
            .transport_builder(Arc::new(PassthroughBuilder))
            .build()
            .unwrap();

        assert_eq!(server.transport_names(), vec!["CUSTOM".to_string()]);
        assert!(server.transport_wrapper("CUSTOM").is_some());
        assert!(server.transport_wrapper("JSONRPC").is_none());
    }

    #[test]
    fn custom_transport_properties_are_resolved_at_build_time() {
        struct CustomTransport {
            enabled: bool,
            deployment: Option<DeploymentProperties>,
        }

        impl CustomTransportProperties for CustomTransport {
            fn to_transport_properties(&self) -> TransportProperties {
                let deployment = self
                    .deployment
                    .clone()
                    .unwrap_or_else(|| DeploymentProperties::new("localhost", 8080, None));
                TransportProperties::builder("JSONRPC")
                    .host(deployment.host())
                    .port(deployment.port())
                    .path("/custom")
                    .build()
            }

            fn set_deployment_properties(&mut self, deployment_properties: DeploymentProperties) {
                self.deployment = Some(deployment_properties);
            }

            fn is_enabled(&self) -> bool {
                self.enabled
            }
        }

        // Enabled: the deployment host/port are injected and the path is preserved.
        let server = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .deployment_properties(deployment())
            .with_custom_transport(Box::new(CustomTransport {
                enabled: true,
                deployment: None,
            }))
            .build()
            .unwrap();
        assert_eq!(server.agent_card().url, "http://localhost:8080/custom");

        // Disabled: nothing is contributed, so the default JSON-RPC transport is synthesized.
        let server = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .deployment_properties(deployment())
            .with_custom_transport(Box::new(CustomTransport {
                enabled: false,
                deployment: None,
            }))
            .build()
            .unwrap();
        assert_eq!(server.agent_card().url, "http://localhost:8080");
    }

    #[test]
    fn post_endpoint_ready_registers_with_every_registry_once() {
        let registry = Arc::new(RecordingRegistry {
            name: "nacos".to_string(),
            calls: Mutex::new(0),
        });
        let server = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .deployment_properties(deployment())
            .with_agent_registry(registry.clone())
            .build()
            .unwrap();

        let report = server.post_endpoint_ready();

        assert_eq!(report.succeeded, vec!["nacos".to_string()]);
        assert_eq!(*registry.calls.lock().unwrap(), 1);
        // The server registers the transports it actually exports, not the requested set.
        assert_eq!(server.transport_properties().len(), 1);
    }

    #[test]
    fn handle_request_routes_through_the_named_transport_wrapper() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .deployment_properties(deployment())
            .build()
            .unwrap();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "user",
                    "kind": "message",
                    "parts": [{ "kind": "text", "text": "hi" }]
                }
            }
        })
        .to_string();

        let response = rt
            .block_on(server.handle_request(
                "JSONRPC",
                &body,
                &TransportHeaders::new(),
                &TransportMetadata::new(),
            ))
            .expect("JSON-RPC is exported");
        let value: serde_json::Value = serde_json::from_str(response.as_json().unwrap()).unwrap();
        assert!(value["result"]["status"]["state"].is_string());

        assert!(rt
            .block_on(server.handle_request(
                "missing",
                &body,
                &TransportHeaders::new(),
                &TransportMetadata::new(),
            ))
            .is_none());
        assert!(server.require_transport_wrapper("missing").is_err());
        server.close();
    }

    #[test]
    fn message_stream_is_served_incrementally_over_a_transport() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = AgentScopeA2aServer::builder(Arc::new(EchoRunner))
            .deployment_properties(deployment())
            .build()
            .unwrap();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/stream",
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "user",
                    "kind": "message",
                    "parts": [{ "kind": "text", "text": "hi" }]
                }
            }
        })
        .to_string();
        let response = rt
            .block_on(server.handle_request(
                "JSONRPC",
                &body,
                &TransportHeaders::new(),
                &TransportMetadata::new(),
            ))
            .unwrap();
        let TransportResponse::Stream(stream) = response else {
            panic!("message/stream must stream");
        };
        let frames: Vec<String> = rt.block_on(futures::StreamExt::collect(stream));
        assert!(frames.len() >= 2);
        let last: TaskStatusUpdateEvent = serde_json::from_value(
            serde_json::from_str::<serde_json::Value>(frames.last().unwrap()).unwrap()["result"]
                .clone(),
        )
        .unwrap();
        assert!(last.is_final);
        assert_eq!(last.status.state, TaskState::Completed);
    }

    #[test]
    fn message_convert_is_reachable_from_the_server_module() {
        // Guards that the server assembly kept the message conversion surface public.
        let msg = Msg::builder()
            .id("m1")
            .role(agentscope_core::message::MsgRole::Assistant)
            .text_content("hi")
            .build();
        let message = ServerMessageConvert::convert_from_msg_to_message(&msg, "t", "c");
        assert_eq!(message.task_id.as_deref(), Some("t"));
    }
}
