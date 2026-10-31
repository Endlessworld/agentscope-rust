//! A2A transport wrapping layer, mirroring
//! `io.agentscope.core.a2a.server.transport`.
//!
//! Java isolates "get the raw request off the wire" from "handle the JSON-RPC call": a web
//! framework controller hands the body + headers to a [`TransportWrapper`], which returns the
//! response (a `String`) or a `Flux<String>` for streaming requests. This module is the same
//! seam for Rust:
//!
//! * [`TransportWrapper`] - `transport.TransportWrapper`
//! * [`TransportWrapperBuilder`] - `transport.TransportWrapperBuilder`
//!   (Java discovers them through `ServiceLoader`; Rust takes them explicitly, see
//!   [`default_transport_builders`]).
//! * [`DeploymentProperties`] - `transport.DeploymentProperties`
//! * [`CustomTransportProperties`] - `transport.CustomTransportProperties`
//! * [`JsonRpcTransportWrapper`] / [`JsonRpcTransportMetadata`] - `transport.jsonrpc.*`

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_extensions_a2a_client::model::{
    methods, AgentCard, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
};
use futures::stream::BoxStream;

use crate::card::{card_converter, TransportProperties};
use crate::registry::AgentRegistryError;
use crate::A2aRequestHandler;

/// Header/metadata map carried alongside a transport request body.
pub type TransportHeaders = HashMap<String, String>;
/// Free-form transport metadata supplied by the hosting framework or the developer.
pub type TransportMetadata = serde_json::Map<String, serde_json::Value>;

/// The response of a [`TransportWrapper::handle_request`].
///
/// Mirrors Java `TransportWrapper#handleRequest` returning `String` for a single response or
/// `Flux<String>` for a streaming request: one JSON string per streamed response.
pub enum TransportResponse {
    /// A single serialized JSON-RPC response.
    Json(String),
    /// A stream of serialized JSON-RPC responses (each becomes one SSE frame).
    Stream(BoxStream<'static, String>),
}

impl std::fmt::Debug for TransportResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportResponse::Json(payload) => f
                .debug_tuple("TransportResponse::Json")
                .field(payload)
                .finish(),
            TransportResponse::Stream(_) => f.write_str("TransportResponse::Stream(..)"),
        }
    }
}

impl TransportResponse {
    /// The JSON payload of a non-streaming response.
    pub fn as_json(&self) -> Option<&str> {
        match self {
            TransportResponse::Json(payload) => Some(payload),
            TransportResponse::Stream(_) => None,
        }
    }

    /// Whether this response is a stream.
    pub fn is_stream(&self) -> bool {
        matches!(self, TransportResponse::Stream(_))
    }
}

/// The interface for different A2A transports (`JSON-RPC`, `GRPC`, `REST`, custom).
///
/// Mirrors Java `io.agentscope.core.a2a.server.transport.TransportWrapper<T, R>`.
///
/// **Deviation**: Java's method is synchronous because the SDK's non-streaming handler is
/// synchronous. The Rust request handler is async, so the method is async here; the streaming
/// half is still produced synchronously as a stream.
#[async_trait::async_trait]
pub trait TransportWrapper: Send + Sync {
    /// Name of the transport type this wrapper handles (`JSONRPC`, ...).
    fn transport_type(&self) -> String;

    /// Handles one transport request, streaming or not.
    async fn handle_request(
        &self,
        body: &str,
        headers: &TransportHeaders,
        metadata: &TransportMetadata,
    ) -> TransportResponse;
}

/// Builds [`TransportWrapper`]s for one transport type.
///
/// Mirrors Java `io.agentscope.core.a2a.server.transport.TransportWrapperBuilder`.
/// **Deviation**: Java passes a `java.util.concurrent.Executor`; Rust has no executor argument
/// because streaming runs on the caller's async task.
pub trait TransportWrapperBuilder: Send + Sync {
    /// Name of the transport type this builder builds.
    fn transport_type(&self) -> String;

    /// Builds the wrapper without an extended agent card.
    fn build(
        &self,
        agent_card: &AgentCard,
        request_handler: Arc<A2aRequestHandler>,
    ) -> Result<Arc<dyn TransportWrapper>, AgentRegistryError>;

    /// Builds the wrapper with an extended agent card.
    ///
    /// Mirrors Java's four-argument overload. Like Java ("TODO: after support of extended agent
    /// card"), the JSON-RPC builder currently ignores it.
    fn build_with_extended_card(
        &self,
        agent_card: &AgentCard,
        _extended_agent_card: Option<&AgentCard>,
        request_handler: Arc<A2aRequestHandler>,
    ) -> Result<Arc<dyn TransportWrapper>, AgentRegistryError> {
        self.build(agent_card, request_handler)
    }
}

/// The transport builders available out of the box, standing in for Java's `ServiceLoader`
/// discovery of `TransportWrapperBuilder` SPI providers.
///
/// Only `JSONRPC` ships with this crate; additional transports are added by registering their
/// builder on the server builder.
pub fn default_transport_builders() -> Vec<Arc<dyn TransportWrapperBuilder>> {
    vec![Arc::new(JsonRpcTransportWrapperBuilder)]
}

/// The failure of [`DeploymentPropertiesBuilder::build`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentError {
    /// `port` was never configured; Java throws `IllegalArgumentException("port must be
    /// configured.")` in the same situation.
    MissingPort,
}

impl std::fmt::Display for DeploymentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeploymentError::MissingPort => f.write_str("port must be configured."),
        }
    }
}

impl std::error::Error for DeploymentError {}

/// Deployment-relative properties: the host, port and path a default transport advertises.
///
/// Mirrors Java `io.agentscope.core.a2a.server.transport.DeploymentProperties`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentProperties {
    host: String,
    port: u16,
    path: Option<String>,
}

impl DeploymentProperties {
    /// Creates properties from explicit values.
    pub fn new(host: impl Into<String>, port: u16, path: Option<String>) -> Self {
        Self {
            host: host.into(),
            port,
            path,
        }
    }

    /// Host used to synthesize the default transport URL.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Port used to synthesize the default transport URL.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Optional path prefix of the transport endpoint.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Returns the Java-style fluent builder.
    pub fn builder() -> DeploymentPropertiesBuilder {
        DeploymentPropertiesBuilder::default()
    }
}

/// Builder for [`DeploymentProperties`].
#[derive(Debug, Default, Clone)]
pub struct DeploymentPropertiesBuilder {
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
}

impl DeploymentPropertiesBuilder {
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

    /// Builds the properties.
    ///
    /// Without an explicit host, the local IP address is used, falling back to `localhost`,
    /// mirroring Java `DeploymentProperties.Builder#build` + `NetworkUtils.getLocalIpAddress`.
    pub fn build(self) -> Result<DeploymentProperties, DeploymentError> {
        let port = self.port.ok_or(DeploymentError::MissingPort)?;
        let host = self.host.unwrap_or_else(local_ip_address);
        Ok(DeploymentProperties {
            host,
            port,
            path: self.path,
        })
    }
}

/// Best-effort local IP address, mirroring Java `NetworkUtils#getLocalIpAddress`.
///
/// Uses the usual "connect a UDP socket and read the chosen local address" trick: a UDP
/// `connect` sends no packets, so this stays offline. Falls back to `localhost`.
pub fn local_ip_address() -> String {
    use std::net::UdpSocket;
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return "localhost".to_string();
    };
    match socket.connect("8.8.8.8:80") {
        Ok(()) => socket
            .local_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "localhost".to_string()),
        Err(_) => "localhost".to_string(),
    }
}

/// Custom transport properties convertible into standard [`TransportProperties`].
///
/// Mirrors Java `io.agentscope.core.a2a.server.transport.CustomTransportProperties`.
pub trait CustomTransportProperties: Send + Sync {
    /// Converts this custom object into standard transport properties.
    fn to_transport_properties(&self) -> TransportProperties;

    /// Receives the deployment properties once the server has resolved them.
    fn set_deployment_properties(&mut self, deployment_properties: DeploymentProperties);

    /// Whether this transport should be exported.
    fn is_enabled(&self) -> bool;
}

/// SPI marker advertising that this server supports `JSONRPC`.
///
/// Mirrors Java `transport.jsonrpc.JsonRpcTransportMetadata`; Java uses it to validate agent
/// cards through the SDK's `TransportMetadata` SPI. In Rust it is a plain value type that
/// exposes the protocol name, used by callers that validate a card's interfaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonRpcTransportMetadata;

impl JsonRpcTransportMetadata {
    /// The transport protocol name this metadata describes.
    pub const TRANSPORT_PROTOCOL: &'static str = card_converter::DEFAULT_TRANSPORT;

    /// The transport protocol name, mirroring `getTransportProtocol()`.
    pub fn transport_protocol(&self) -> &'static str {
        Self::TRANSPORT_PROTOCOL
    }
}

/// JSON-RPC transport wrapper: request pre-handling + response serialization.
///
/// Mirrors Java `transport.jsonrpc.JsonRpcTransportWrapper`. Java delegates to the A2A SDK's
/// `JSONRPCHandler`; here the equivalent work (parse, streaming detection, dispatch, error
/// mapping) is done over [`A2aRequestHandler`].
#[derive(Clone)]
pub struct JsonRpcTransportWrapper {
    request_handler: Arc<A2aRequestHandler>,
}

impl std::fmt::Debug for JsonRpcTransportWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonRpcTransportWrapper")
            .field("agent", &self.request_handler.agent_card().name)
            .finish()
    }
}

impl JsonRpcTransportWrapper {
    /// The transport type handled by this wrapper.
    pub const TRANSPORT_TYPE: &'static str = card_converter::DEFAULT_TRANSPORT;

    /// Creates a wrapper around `request_handler`.
    pub fn new(request_handler: Arc<A2aRequestHandler>) -> Self {
        Self { request_handler }
    }

    /// The wrapped request handler.
    pub fn request_handler(&self) -> &Arc<A2aRequestHandler> {
        &self.request_handler
    }

    /// Parses a request body, mapping every failure onto a JSON-RPC error response.
    fn parse(&self, body: &str) -> Result<JsonRpcRequest, String> {
        if body.trim().is_empty() {
            return Err(error_json(
                None,
                JsonRpcError::PARSE_ERROR,
                "Empty request body",
            ));
        }
        let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
            let message = error.to_string();
            error_json(None, JsonRpcError::PARSE_ERROR, message.as_str())
        })?;
        if !value.is_object() {
            return Err(error_json(
                None,
                JsonRpcError::INVALID_REQUEST,
                "Invalid request body",
            ));
        }
        serde_json::from_value::<JsonRpcRequest>(value).map_err(|error| {
            let id = request_id(body);
            error_json(id, JsonRpcError::INVALID_PARAMS, error.to_string().as_str())
        })
    }
}

#[async_trait::async_trait]
impl TransportWrapper for JsonRpcTransportWrapper {
    fn transport_type(&self) -> String {
        Self::TRANSPORT_TYPE.to_string()
    }

    async fn handle_request(
        &self,
        body: &str,
        _headers: &TransportHeaders,
        _metadata: &TransportMetadata,
    ) -> TransportResponse {
        let request = match self.parse(body) {
            Ok(request) => request,
            Err(error) => return TransportResponse::Json(error),
        };
        let id = request.id.clone();
        if is_streaming_method(&request.method) {
            match self.request_handler.stream_responses(&request) {
                Ok(stream) => {
                    return TransportResponse::Stream(Box::pin(futures::StreamExt::map(
                        stream,
                        |response| serialize_response(response),
                    )))
                }
                Err(error) => {
                    return TransportResponse::Json(serialize_response(JsonRpcResponse::failure(
                        error, id,
                    )))
                }
            }
        }
        TransportResponse::Json(serialize_response(
            self.request_handler.handle(&request).await,
        ))
    }
}

/// Whether a JSON-RPC method is answered with a stream of responses.
///
/// Mirrors Java `JsonRpcTransportWrapper#handleStreamRequest`, which accepts both
/// `SendStreamingMessage` (`message/stream`) and `SubscribeToTask` (`tasks/resubscribe`).
/// **Deviation**: only the streaming message methods stream here — Java can honor a resubscribe
/// because the SDK keeps a per-task event queue; this handler has no queue (each run's events are
/// relayed once, then recorded as artifacts), so `SubscribeToTask` would have to re-run the agent.
/// It therefore stays a non-streaming method and answers `-32601` (`dispatch` has no arm for it).
pub fn is_streaming_method(method: &str) -> bool {
    matches!(
        method,
        methods::MESSAGE_STREAM | methods::LEGACY_MESSAGE_STREAM
    )
}

/// Serializes a response, falling back to an internal error if it cannot be encoded.
fn serialize_response(response: JsonRpcResponse) -> String {
    let id = response.id.clone();
    serde_json::to_string(&response).unwrap_or_else(|error| {
        error_json(
            id,
            JsonRpcError::INTERNAL_ERROR,
            format!("failed to serialize JSON-RPC response: {error}").as_str(),
        )
    })
}

/// Builds a serialized JSON-RPC error response.
fn error_json(id: Option<serde_json::Value>, code: i64, message: &str) -> String {
    serialize_response(JsonRpcResponse::failure(
        JsonRpcError::new(code, message),
        id,
    ))
}

/// Java `requestId`: the request's `id` when it is a JSON primitive, else `None`.
fn request_id(body: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let id = value.get("id")?;
    if id.is_string() || id.is_number() {
        Some(id.clone())
    } else {
        None
    }
}

/// Builder for [`JsonRpcTransportWrapper`].
///
/// Mirrors Java `transport.jsonrpc.JsonRpcTransportWrapperBuilder`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonRpcTransportWrapperBuilder;

impl TransportWrapperBuilder for JsonRpcTransportWrapperBuilder {
    fn transport_type(&self) -> String {
        JsonRpcTransportWrapper::TRANSPORT_TYPE.to_string()
    }

    fn build(
        &self,
        _agent_card: &AgentCard,
        request_handler: Arc<A2aRequestHandler>,
    ) -> Result<Arc<dyn TransportWrapper>, AgentRegistryError> {
        Ok(Arc::new(JsonRpcTransportWrapper::new(request_handler)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{A2aTaskRegistry, AgentRequestOptions, AgentRunner};
    use agentscope_core::event::{AgentEvent, AgentEventType, EventData};
    use agentscope_core::message::Msg;
    use agentscope_extensions_a2a_client::error::A2aError;
    use agentscope_extensions_a2a_client::model::{Message, MessageRole, Part};

    struct EchoRunner;

    #[async_trait::async_trait]
    impl AgentRunner for EchoRunner {
        fn agent_name(&self) -> String {
            "echo".to_string()
        }

        fn agent_description(&self) -> String {
            "echo".to_string()
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

    fn handler() -> Arc<A2aRequestHandler> {
        Arc::new(A2aRequestHandler::new(
            Arc::new(EchoRunner),
            AgentCard::new("echo", "http://localhost:8080"),
            Arc::new(A2aTaskRegistry::new()),
        ))
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn send_body() -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "user",
                    "kind": "message",
                    "parts": [{ "kind": "text", "text": "ping" }]
                }
            }
        })
        .to_string()
    }

    #[test]
    fn jsonrpc_wrapper_declares_the_jsonrpc_transport_type() {
        let wrapper = JsonRpcTransportWrapper::new(handler());
        assert_eq!(wrapper.transport_type(), "JSONRPC");
        assert_eq!(
            JsonRpcTransportMetadata::default().transport_protocol(),
            wrapper.transport_type()
        );
        let builder = JsonRpcTransportWrapperBuilder;
        assert_eq!(builder.transport_type(), "JSONRPC");
        let built = builder
            .build(&AgentCard::new("a", "http://h"), handler())
            .unwrap();
        assert_eq!(built.transport_type(), "JSONRPC");
    }

    #[test]
    fn empty_and_malformed_bodies_become_jsonrpc_errors() {
        let wrapper = JsonRpcTransportWrapper::new(handler());
        let headers = TransportHeaders::new();
        let metadata = TransportMetadata::new();

        let empty = rt().block_on(wrapper.handle_request("  ", &headers, &metadata));
        let empty: JsonRpcResponse = serde_json::from_str(empty.as_json().unwrap()).unwrap();
        assert_eq!(empty.error.unwrap().code, JsonRpcError::PARSE_ERROR);

        let not_object = rt().block_on(wrapper.handle_request("[1]", &headers, &metadata));
        let not_object: JsonRpcResponse =
            serde_json::from_str(not_object.as_json().unwrap()).unwrap();
        assert_eq!(
            not_object.error.unwrap().code,
            JsonRpcError::INVALID_REQUEST
        );

        let malformed = rt().block_on(wrapper.handle_request("{", &headers, &metadata));
        let malformed: JsonRpcResponse =
            serde_json::from_str(malformed.as_json().unwrap()).unwrap();
        assert_eq!(malformed.error.unwrap().code, JsonRpcError::PARSE_ERROR);
    }

    #[test]
    fn non_streaming_requests_return_one_json_response() {
        let wrapper = JsonRpcTransportWrapper::new(handler());
        let response = rt().block_on(wrapper.handle_request(
            &send_body(),
            &TransportHeaders::new(),
            &TransportMetadata::new(),
        ));
        assert!(!response.is_stream());
        let parsed: JsonRpcResponse = serde_json::from_str(response.as_json().unwrap()).unwrap();
        assert!(parsed.error.is_none());
        assert_eq!(parsed.id, Some(serde_json::Value::from(1)));
    }

    #[test]
    fn streaming_requests_return_a_stream_of_json_responses() {
        let wrapper = JsonRpcTransportWrapper::new(handler());
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "method": "message/stream",
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "user",
                    "kind": "message",
                    "parts": [{ "kind": "text", "text": "ping" }]
                }
            }
        })
        .to_string();
        let response = rt().block_on(wrapper.handle_request(
            &body,
            &TransportHeaders::new(),
            &TransportMetadata::new(),
        ));
        assert!(response.is_stream());
        let TransportResponse::Stream(stream) = response else {
            panic!("expected a stream");
        };
        let frames: Vec<String> = rt().block_on(futures::StreamExt::collect(stream));
        assert!(frames.len() >= 2, "expected incremental frames: {frames:?}");
        for frame in &frames {
            let parsed: JsonRpcResponse = serde_json::from_str(frame).unwrap();
            assert!(parsed.error.is_none());
            assert_eq!(parsed.id, Some(serde_json::Value::from("s-1")));
        }
    }

    #[test]
    fn streaming_method_detection_matches_the_a2a_methods() {
        assert!(is_streaming_method(methods::MESSAGE_STREAM));
        assert!(is_streaming_method(methods::LEGACY_MESSAGE_STREAM));
        assert!(!is_streaming_method(methods::MESSAGE_SEND));
        // No per-task event queue: resubscribe would have to re-run the agent, so it stays a
        // non-streaming (unsupported) method rather than silently starting a second run.
        assert!(!is_streaming_method(methods::TASKS_RESUBSCRIBE));
        assert!(!is_streaming_method(
            agentscope_core::event::AgentEventType::AgentStart.wire_name()
        ));
    }

    #[test]
    fn resubscribe_is_not_streamed_and_reports_method_not_found() {
        let wrapper = JsonRpcTransportWrapper::new(handler());
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tasks/resubscribe",
            "params": { "id": "t-1" }
        })
        .to_string();
        let response = rt().block_on(wrapper.handle_request(
            &body,
            &TransportHeaders::new(),
            &TransportMetadata::new(),
        ));
        assert!(!response.is_stream());
        let parsed: JsonRpcResponse = serde_json::from_str(response.as_json().unwrap()).unwrap();
        assert_eq!(parsed.error.unwrap().code, JsonRpcError::METHOD_NOT_FOUND);
    }

    #[test]
    fn deployment_properties_require_a_port_and_default_the_host() {
        assert_eq!(
            DeploymentProperties::builder().build().unwrap_err(),
            DeploymentError::MissingPort
        );
        let configured = DeploymentProperties::builder()
            .host("agent.example.com")
            .port(9000)
            .path("/a2a")
            .build()
            .unwrap();
        assert_eq!(configured.host(), "agent.example.com");
        assert_eq!(configured.port(), 9000);
        assert_eq!(configured.path(), Some("/a2a"));

        let defaulted = DeploymentProperties::builder().port(8080).build().unwrap();
        assert!(!defaulted.host().is_empty());
    }

    #[test]
    fn default_transport_builders_expose_jsonrpc_only() {
        let builders = default_transport_builders();
        assert_eq!(builders.len(), 1);
        assert_eq!(builders[0].transport_type(), "JSONRPC");
    }

    #[test]
    fn send_helpers_parse_a_raw_jsonrpc_body() {
        // Guards the fixtures above: a malformed body would make the tests vacuous.
        let request: JsonRpcRequest = serde_json::from_str(&send_body()).unwrap();
        // The fixture is deliberately a 0.3 body: the server still answers the legacy method name.
        assert_eq!(request.method, methods::LEGACY_MESSAGE_SEND);
        let message = Message::new("m", MessageRole::User, vec![Part::text("hi")]);
        assert_eq!(message.parts.len(), 1);
        assert_eq!(AgentEventType::AgentStart.wire_name(), "AGENT_START");
    }
}
