//! The MCP handshake and the two tool-discovery requests.
//!
//! A session is exactly four steps, and this module owns the pure builders for them:
//!
//! 1. `initialize` request ([`initialize_request`]);
//! 2. `notifications/initialized` ([`initialized_notification`]) once the server answered;
//! 3. `tools/list` ([`tools_list_request`], [`tools_list_page_request`] for a cursor);
//! 4. `tools/call` ([`tools_call_request`](crate::tools_call_request)).
//!
//! Ids are supplied by the caller: the durable runtime owns retries, so it must be able to
//! mint a fresh id per attempt instead of sharing a counter with this crate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::TransportError;
use crate::jsonrpc::{ErrorObject, Notification, Request, RequestId};

/// The MCP protocol version this crate speaks in `initialize`.
///
/// Override it through [`InitializeParams`] when talking to an older server.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// `clientInfo.name` sent in `initialize`.
pub const CLIENT_NAME: &str = "agentscope-durable-mcp";

/// `clientInfo.version` sent in `initialize`.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `initialize`.
pub const METHOD_INITIALIZE: &str = "initialize";

/// `notifications/initialized`.
pub const METHOD_INITIALIZED: &str = "notifications/initialized";

/// `tools/list`.
pub const METHOD_TOOLS_LIST: &str = "tools/list";

/// The MCP methods this adapter understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `initialize`.
    Initialize,
    /// `notifications/initialized`.
    Initialized,
    /// `tools/list`.
    ToolsList,
    /// `tools/call`.
    ToolsCall,
}

impl Method {
    /// Parses a method name, rejecting everything this adapter does not implement.
    ///
    /// The rejection is a `-32601` [`ErrorObject`] rather than a silent `None`: an unknown
    /// method must be answered, not ignored.
    pub fn parse(name: &str) -> Result<Self, ErrorObject> {
        match name {
            METHOD_INITIALIZE => Ok(Self::Initialize),
            METHOD_INITIALIZED => Ok(Self::Initialized),
            METHOD_TOOLS_LIST => Ok(Self::ToolsList),
            crate::mapping::METHOD_TOOLS_CALL => Ok(Self::ToolsCall),
            _ => Err(ErrorObject::method_not_found(name)),
        }
    }

    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Initialize => METHOD_INITIALIZE,
            Self::Initialized => METHOD_INITIALIZED,
            Self::ToolsList => METHOD_TOOLS_LIST,
            Self::ToolsCall => crate::mapping::METHOD_TOOLS_CALL,
        }
    }
}

/// `clientInfo` / `serverInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Implementation {
    /// Program name.
    pub name: String,
    /// Program version.
    pub version: String,
}

impl Implementation {
    /// Builds an implementation identity.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }

    /// This crate's identity.
    pub fn client() -> Self {
        Self::new(CLIENT_NAME, CLIENT_VERSION)
    }
}

/// A capability object with no members (`{}`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyCapability {}

/// The `roots` capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootsCapability {
    /// Whether the client will notify the server when its roots change.
    #[serde(default)]
    pub list_changed: bool,
}

/// `initialize.params.capabilities`.
///
/// Empty by default: the durable executor only calls tools, it does not sample, elicit or
/// expose roots. Fields are `Option` so an absent capability is *omitted*, not sent as
/// `null`, which strict servers reject.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Filesystem roots the client exposes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsCapability>,
    /// Sampling (`sampling/createMessage`) support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<EmptyCapability>,
    /// Elicitation (`elicitation/create`) support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<EmptyCapability>,
}

/// `initialize` params, in spec field order (`protocolVersion`, `capabilities`, `clientInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The protocol revision the client wants to speak.
    pub protocol_version: String,
    /// Client capabilities; empty by default.
    pub capabilities: ClientCapabilities,
    /// Client identity.
    pub client_info: Implementation,
}

impl Default for InitializeParams {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation::client(),
        }
    }
}

impl InitializeParams {
    /// Params for [`PROTOCOL_VERSION`] and this crate's client info.
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the negotiated protocol version.
    pub fn with_protocol_version(mut self, version: impl Into<String>) -> Self {
        self.protocol_version = version.into();
        self
    }

    /// Replaces the capability set.
    pub fn with_capabilities(mut self, capabilities: ClientCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Replaces the client identity (tests, embedding products).
    pub fn with_client_info(mut self, info: Implementation) -> Self {
        self.client_info = info;
        self
    }
}

/// Builds the `initialize` request with this crate's defaults.
pub fn initialize_request(id: impl Into<RequestId>) -> Result<Request, TransportError> {
    initialize_request_with(id, &InitializeParams::default())
}

/// Builds an `initialize` request from explicit params.
pub fn initialize_request_with(
    id: impl Into<RequestId>,
    params: &InitializeParams,
) -> Result<Request, TransportError> {
    Request::with_params(id, METHOD_INITIALIZE, params)
}

/// The `notifications/initialized` notification, sent after a successful `initialize` reply.
///
/// It has no params and gets no reply; a transport that can only do request/response still
/// has to be able to write this one, which is why it is built here and not inside
/// [`McpTransport`](crate::McpTransport).
pub fn initialized_notification() -> Notification {
    Notification::new(METHOD_INITIALIZED)
}

/// Builds a `tools/list` request for the first page.
pub fn tools_list_request(id: impl Into<RequestId>) -> Request {
    Request::new(id, METHOD_TOOLS_LIST)
}

/// `tools/list` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsListParams {
    /// Opaque cursor from the previous page.
    pub cursor: String,
}

/// Builds a `tools/list` request for a page after `cursor`.
pub fn tools_list_page_request(
    id: impl Into<RequestId>,
    cursor: &str,
) -> Result<Request, TransportError> {
    Request::with_params(
        id,
        METHOD_TOOLS_LIST,
        &ToolsListParams {
            cursor: cursor.to_owned(),
        },
    )
}

/// Reads the `serverInfo` of an `initialize` reply, tolerating a peer that sends none.
pub fn server_info(result: &Value) -> Option<Implementation> {
    serde_json::from_value(result.get("serverInfo")?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_carries_version_capabilities_and_client_info() {
        let request = initialize_request(RequestId::number(1)).expect("initialize");
        assert_eq!(request.method, METHOD_INITIALIZE);

        let params: InitializeParams = request.params_as().expect("params");
        assert_eq!(params.protocol_version, PROTOCOL_VERSION);
        assert_eq!(params.client_info, Implementation::client());
        assert_eq!(params.capabilities, ClientCapabilities::default());

        let params = request.params_value().expect("params value");
        assert_eq!(params["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(params["capabilities"], serde_json::json!({}));
        assert_eq!(params["clientInfo"]["name"], CLIENT_NAME);
        assert_eq!(params["clientInfo"]["version"], CLIENT_VERSION);
    }

    #[test]
    fn declared_capabilities_are_sent_and_undeclared_ones_are_omitted() {
        let params = InitializeParams::default().with_capabilities(ClientCapabilities {
            roots: Some(RootsCapability { list_changed: true }),
            sampling: None,
            elicitation: None,
        });
        let request = initialize_request_with(1, &params).expect("initialize");
        let value = request.params_value().expect("params");
        assert_eq!(value["capabilities"]["roots"]["listChanged"], true);
        assert!(value["capabilities"].get("sampling").is_none());
        assert!(value["capabilities"].get("elicitation").is_none());
    }

    #[test]
    fn the_initialized_notification_has_no_id_and_no_params() {
        let notification = initialized_notification();
        assert_eq!(notification.method, METHOD_INITIALIZED);
        assert!(notification.params.is_none());
        assert!(!notification.to_wire().expect("wire").contains("params"));
    }

    #[test]
    fn tools_list_pages_only_carry_a_cursor_when_asked() {
        let first = tools_list_request(2);
        assert_eq!(first.method, METHOD_TOOLS_LIST);
        assert!(first.params.is_none());

        let next = tools_list_page_request(3, "page-2").expect("page");
        assert_eq!(next.params_value().expect("params")["cursor"], "page-2");
    }

    #[test]
    fn unknown_methods_are_rejected_with_method_not_found() {
        for method in [
            METHOD_INITIALIZE,
            METHOD_INITIALIZED,
            METHOD_TOOLS_LIST,
            "tools/call",
        ] {
            assert!(Method::parse(method).is_ok(), "{method} should parse");
            assert_eq!(Method::parse(method).expect("method").name(), method);
        }
        let error = Method::parse("tools/poke").expect_err("must be rejected");
        assert_eq!(error.code, crate::jsonrpc::codes::METHOD_NOT_FOUND);
        assert_eq!(
            error.data,
            Some(serde_json::json!({ "method": "tools/poke" }))
        );
    }

    #[test]
    fn server_info_is_read_when_present() {
        let reply = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "peer", "version": "1.2.3" }
        });
        assert_eq!(
            server_info(&reply),
            Some(Implementation::new("peer", "1.2.3"))
        );
        assert_eq!(server_info(&serde_json::json!({})), None);
    }
}
