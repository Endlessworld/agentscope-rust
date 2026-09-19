//! Request-scoped runtime context resolution, mirroring `io.agentscope.core.agui.runtime`.
//!
//! * [`AguiRuntimeContextRequest`] - the request-scoped data available to resolvers (input,
//!   agent-id sources, transport, headers, query params).
//! * [`AguiRuntimeContextResolver`] - the hook applications use to attach tenant/tracing values.
//! * [`AguiRequestBodyParser`] - parses a JSON body into a [`RunAgentInput`].

use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::RuntimeContext;

use crate::agui::error::{AguiError, AguiResult};
use crate::agui::model::RunAgentInput;

/// The transport that received the AG-UI request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AguiTransport {
    /// A transport-specific native request is not available.
    #[default]
    Custom,
    /// An Axum/hyper request.
    Http,
}

/// Request-scoped data available when resolving an AG-UI [`RuntimeContext`].
#[derive(Debug, Clone)]
pub struct AguiRuntimeContextRequest {
    /// The parsed AG-UI run input.
    pub input: RunAgentInput,
    header_agent_id: Option<String>,
    path_agent_id: Option<String>,
    transport: AguiTransport,
    method: Option<String>,
    path: Option<String>,
    headers: BTreeMap<String, Vec<String>>,
    query_params: BTreeMap<String, Vec<String>>,
}

impl AguiRuntimeContextRequest {
    /// Creates a builder.
    pub fn builder() -> AguiRuntimeContextRequestBuilder {
        AguiRuntimeContextRequestBuilder::default()
    }

    /// The agent id supplied through a header, when present.
    pub fn header_agent_id(&self) -> Option<&str> {
        self.header_agent_id.as_deref()
    }

    /// The agent id supplied as a path variable, when present.
    pub fn path_agent_id(&self) -> Option<&str> {
        self.path_agent_id.as_deref()
    }

    /// The transport that produced the request.
    pub fn transport(&self) -> AguiTransport {
        self.transport
    }

    /// The HTTP method, when known.
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// The request path, when known.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// All request headers (multi-valued).
    pub fn headers(&self) -> &BTreeMap<String, Vec<String>> {
        &self.headers
    }

    /// The first value of a header, matched case-insensitively.
    pub fn first_header(&self, name: &str) -> Option<&str> {
        first_value(&self.headers, name, true)
    }

    /// All query parameters (multi-valued).
    pub fn query_params(&self) -> &BTreeMap<String, Vec<String>> {
        &self.query_params
    }

    /// The first value of a query parameter, matched case-sensitively.
    pub fn first_query_param(&self, name: &str) -> Option<&str> {
        first_value(&self.query_params, name, false)
    }
}

/// Returns the first value of `name` in a multi-valued map.
fn first_value<'a>(
    values: &'a BTreeMap<String, Vec<String>>,
    name: &str,
    ignore_case: bool,
) -> Option<&'a str> {
    values
        .iter()
        .find(|(key, _)| {
            if ignore_case {
                key.eq_ignore_ascii_case(name)
            } else {
                key.as_str() == name
            }
        })
        .and_then(|(_, list)| list.first())
        .map(String::as_str)
}

/// Builder for [`AguiRuntimeContextRequest`].
#[derive(Debug, Default, Clone)]
pub struct AguiRuntimeContextRequestBuilder {
    input: Option<RunAgentInput>,
    header_agent_id: Option<String>,
    path_agent_id: Option<String>,
    transport: Option<AguiTransport>,
    method: Option<String>,
    path: Option<String>,
    headers: BTreeMap<String, Vec<String>>,
    query_params: BTreeMap<String, Vec<String>>,
}

impl AguiRuntimeContextRequestBuilder {
    /// Sets the parsed run input (required).
    pub fn input(mut self, input: RunAgentInput) -> Self {
        self.input = Some(input);
        self
    }

    /// Sets the header-sourced agent id.
    pub fn header_agent_id(mut self, value: impl Into<String>) -> Self {
        self.header_agent_id = Some(value.into());
        self
    }

    /// Sets the path-variable agent id.
    pub fn path_agent_id(mut self, value: impl Into<String>) -> Self {
        self.path_agent_id = Some(value.into());
        self
    }

    /// Sets the transport.
    pub fn transport(mut self, transport: AguiTransport) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Sets the HTTP method.
    pub fn method(mut self, value: impl Into<String>) -> Self {
        self.method = Some(value.into());
        self
    }

    /// Sets the request path.
    pub fn path(mut self, value: impl Into<String>) -> Self {
        self.path = Some(value.into());
        self
    }

    /// Adds a header value.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.entry(name.into()).or_default().push(value.into());
        self
    }

    /// Adds a query parameter value.
    pub fn query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query_params
            .entry(name.into())
            .or_default()
            .push(value.into());
        self
    }

    /// Builds the request.
    ///
    /// # Panics
    ///
    /// Panics when no input was supplied, mirroring Java's `Objects.requireNonNull(input)`.
    pub fn build(self) -> AguiRuntimeContextRequest {
        AguiRuntimeContextRequest {
            input: self.input.expect("input cannot be null"),
            header_agent_id: self.header_agent_id,
            path_agent_id: self.path_agent_id,
            transport: self.transport.unwrap_or_default(),
            method: self.method,
            path: self.path,
            headers: self.headers,
            query_params: self.query_params,
        }
    }
}

/// Resolves a caller-provided [`RuntimeContext`] for an AG-UI request.
///
/// Applications attach request-scoped values such as tenant information, tracing data or tool
/// dependencies. The AG-UI runtime metadata is still supplied by the adapter and takes precedence
/// over conflicting values.
pub trait AguiRuntimeContextResolver: Send + Sync {
    /// Resolves the runtime context, or `None` when nothing should be attached.
    fn resolve(&self, request: &AguiRuntimeContextRequest) -> Option<RuntimeContext>;
}

impl<F> AguiRuntimeContextResolver for F
where
    F: Fn(&AguiRuntimeContextRequest) -> Option<RuntimeContext> + Send + Sync,
{
    fn resolve(&self, request: &AguiRuntimeContextRequest) -> Option<RuntimeContext> {
        self(request)
    }
}

/// A shared runtime context resolver handle.
pub type SharedRuntimeContextResolver = Arc<dyn AguiRuntimeContextResolver>;

/// Parses AG-UI request bodies into [`RunAgentInput`].
///
/// Mirrors Java's `AguiRequestBodyParser`: keeping the conversion at the AG-UI boundary avoids
/// coupling the protocol models to a framework-specific HTTP converter.
#[derive(Debug, Clone, Copy, Default)]
pub struct AguiRequestBodyParser;

impl AguiRequestBodyParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self
    }

    /// Parses a JSON request body into a run input.
    pub fn parse(&self, body: &str) -> AguiResult<RunAgentInput> {
        serde_json::from_str(body).map_err(|error| AguiError::invalid_input(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::adapter::AguiStreamableAgent;
    use agentscope_core::{Agent, AgentEvent, Msg, StreamOptions, Toolkit};
    use serde_json::json;

    struct StubAgent;

    impl Agent for StubAgent {
        fn agent_id(&self) -> &str {
            "stub"
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> agentscope_core::AgentState {
            agentscope_core::AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            static TOOLKIT: std::sync::OnceLock<Toolkit> = std::sync::OnceLock::new();
            TOOLKIT.get_or_init(Toolkit::new)
        }
    }

    impl AguiStreamableAgent for StubAgent {
        fn stream_events(
            &self,
            _messages: Vec<Msg>,
            _options: StreamOptions,
        ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
            Box::pin(futures::stream::empty())
        }
    }

    fn request() -> AguiRuntimeContextRequest {
        AguiRuntimeContextRequest::builder()
            .input(RunAgentInput::of("t", "r"))
            .transport(AguiTransport::Http)
            .method("POST")
            .path("/agui/agent/run")
            .header("X-Agent-Id", "from-header")
            .header("Accept", "text/event-stream")
            .header("accept", "application/json")
            .query_param("agentId", "from-query")
            .query_param("tag", "a")
            .query_param("tag", "b")
            .build()
    }

    #[test]
    fn accessors_expose_transport_and_agent_id_sources() {
        let request = request();
        assert_eq!(request.transport(), AguiTransport::Http);
        assert_eq!(request.method(), Some("POST"));
        assert_eq!(request.path(), Some("/agui/agent/run"));
        assert_eq!(request.header_agent_id(), None);
        assert_eq!(request.path_agent_id(), None);
        assert_eq!(request.input.thread_id, "t");
        // Header storage is case-sensitive (lookup is not), so `Accept` and `accept` are two
        // entries; `X-Agent-Id` makes three, matching Java's `LinkedHashMap` backing store.
        assert_eq!(request.headers().len(), 3);
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_returns_the_first_value() {
        let request = request();
        assert_eq!(request.first_header("x-agent-id"), Some("from-header"));
        assert_eq!(request.first_header("ACCEPT"), Some("text/event-stream"));
        assert_eq!(request.first_header("missing"), None);
    }

    #[test]
    fn query_lookup_is_case_sensitive_and_returns_the_first_value() {
        let request = request();
        assert_eq!(request.first_query_param("agentId"), Some("from-query"));
        assert_eq!(request.first_query_param("tag"), Some("a"));
        // Query params are case-sensitive, unlike headers.
        assert_eq!(request.first_query_param("agentid"), None);
    }

    #[test]
    fn agent_ids_supplied_to_the_builder_are_readable() {
        let request = AguiRuntimeContextRequest::builder()
            .input(RunAgentInput::of("t", "r"))
            .header_agent_id("h")
            .path_agent_id("p")
            .build();
        assert_eq!(request.header_agent_id(), Some("h"));
        assert_eq!(request.path_agent_id(), Some("p"));
        // The transport defaults to CUSTOM.
        assert_eq!(request.transport(), AguiTransport::Custom);
        assert!(request.headers().is_empty());
        assert!(request.query_params().is_empty());
    }

    #[test]
    fn building_without_an_input_panics_like_java() {
        let panicked = std::panic::catch_unwind(|| AguiRuntimeContextRequest::builder().build());
        assert!(panicked.is_err());
    }

    #[test]
    fn a_closure_resolves_a_runtime_context() {
        let resolver: SharedRuntimeContextResolver = Arc::new(|request: &AguiRuntimeContextRequest| {
            Some(RuntimeContext::default().with_user_id(&request.input.thread_id))
        });
        let request = request();
        let context = resolver.resolve(&request).unwrap();
        assert_eq!(context.user_id(), Some("t"));
    }

    #[test]
    fn a_resolver_can_decline_to_supply_a_context() {
        let resolver: SharedRuntimeContextResolver = Arc::new(|_: &AguiRuntimeContextRequest| None);
        assert!(resolver.resolve(&request()).is_none());
    }

    #[test]
    fn the_body_parser_reads_a_run_input() {
        let parser = AguiRequestBodyParser::new();
        let body = r#"{"threadId":"t","runId":"r","messages":[{"id":"m1","role":"user","content":"hi"}]}"#;
        let input = parser.parse(body).unwrap();
        assert_eq!(input.thread_id, "t");
        assert_eq!(input.run_id, "r");
        assert_eq!(input.messages.len(), 1);
        assert_eq!(input.messages[0].text_content(), Some("hi"));
    }

    #[test]
    fn the_body_parser_rejects_malformed_json() {
        let parser = AguiRequestBodyParser::default();
        let error = parser.parse("{oops").unwrap_err();
        assert!(matches!(error, crate::agui::error::AguiError::InvalidInput(_)));
        assert!(error.to_string().contains("Invalid AG-UI input"));
        // Missing required fields is likewise an input error, not a panic.
        assert!(parser.parse(r#"{"runId":"r"}"#).is_err());
    }

    #[test]
    fn a_stub_agent_satisfies_the_streamable_trait() {
        let agent = StubAgent;
        assert_eq!(agent.agent_id(), "stub");
        let _ = json!(1);
    }
}
