//! A JSON-RPC 2.0 wire layer, written by hand (no SDK, no network stack).
//!
//! Only the four message shapes MCP needs are modelled: [`Request`], [`Response`],
//! [`Notification`] and [`ErrorObject`]. Everything is symmetric: a message that
//! [`Request::to_wire`] produced parses back to an equal value and re-encodes to the same
//! bytes.
//!
//! Params are stored as [`RawValue`] rather than `Value` on purpose. MCP parameter objects
//! have a meaningful field order (spec examples, golden traces), and routing them through a
//! map-backed `Value` would either sort the keys or depend on a `serde_json` feature flag
//! someone else happened to enable. `RawValue` keeps whatever the typed params struct
//! encoded, so [`InitializeParams`](crate::session::InitializeParams) field order *is* the
//! wire order.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;

use crate::error::TransportError;

/// The only JSON-RPC version this crate speaks.
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC 2.0 error codes plus the two local MCP codes this adapter uses.
pub mod codes {
    /// The bytes were not JSON at all.
    pub const PARSE_ERROR: i64 = -32700;
    /// JSON, but not a JSON-RPC 2.0 message.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The peer does not implement the method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The method exists, the params do not fit it.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The peer failed internally; it may already have started the work.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Local (non-standard) code: the peer does not advertise the tool in `tools/list`.
    pub const TOOL_NOT_FOUND: i64 = -32001;
    /// Local (non-standard) code: the peer knows the tool but cannot serve it right now.
    pub const TOOL_UNAVAILABLE: i64 = -32002;
}

/// A JSON-RPC request id: a number or a string, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric id.
    Number(i64),
    /// A string id.
    Text(String),
}

impl RequestId {
    /// A numeric id.
    pub fn number(value: i64) -> Self {
        Self::Number(value)
    }

    /// A string id.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(number) => write!(formatter, "{number}"),
            Self::Text(text) => write!(formatter, "{text}"),
        }
    }
}

/// A JSON-RPC error object.
///
/// Field order is `code`, `message`, `data`; `data` is omitted when absent so the wire form
/// matches the examples in the JSON-RPC/MCP specs byte for byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// The error code.
    pub code: i64,
    /// Human-readable description.
    pub message: String,
    /// Optional structured diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// Builds an error object.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attaches diagnostic data.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// `-32700`.
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(codes::PARSE_ERROR, message)
    }

    /// `-32600`.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_REQUEST, message)
    }

    /// `-32601`, carrying the offending method name in `data`.
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            codes::METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
        )
        .with_data(serde_json::json!({ "method": method }))
    }

    /// `-32602`.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, message)
    }

    /// `-32603`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL_ERROR, message)
    }

    /// `-32001`, carrying the offending tool name in `data`.
    pub fn tool_not_found(name: &str) -> Self {
        Self::new(
            codes::TOOL_NOT_FOUND,
            format!("Tool not advertised by the peer: {name}"),
        )
        .with_data(serde_json::json!({ "name": name }))
    }
}

/// A JSON-RPC request.
///
/// Equality is textual for `params`: the raw slice is compared as it will be written, so
/// "encode, decode, re-encode" equality is byte equality rather than JSON-value equality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Always [`JSONRPC_VERSION`].
    pub jsonrpc: String,
    /// The id the reply must carry.
    pub id: RequestId,
    /// The MCP method name.
    pub method: String,
    /// Method params, kept as the encoded text of the typed params struct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Box<RawValue>>,
}

fn params_text(params: &Option<Box<RawValue>>) -> Option<&str> {
    params.as_deref().map(RawValue::get)
}

impl PartialEq for Request {
    fn eq(&self, other: &Self) -> bool {
        self.jsonrpc == other.jsonrpc
            && self.id == other.id
            && self.method == other.method
            && params_text(&self.params) == params_text(&other.params)
    }
}

impl Request {
    /// A request without params.
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            method: method.into(),
            params: None,
        }
    }

    /// A request whose params are a typed struct.
    ///
    /// The struct is encoded directly to text, so its field order is the wire order.
    pub fn with_params<P>(
        id: impl Into<RequestId>,
        method: impl Into<String>,
        params: &P,
    ) -> Result<Self, TransportError>
    where
        P: Serialize + ?Sized,
    {
        let mut request = Self::new(id, method);
        let text = serde_json::to_string(params)
            .map_err(|error| TransportError::Encode(format!("cannot encode params: {error}")))?;
        request.params =
            Some(RawValue::from_string(text).map_err(|error| {
                TransportError::Encode(format!("cannot encode params: {error}"))
            })?);
        Ok(request)
    }

    /// Parses a wire message, separating "not JSON" from "not a request".
    ///
    /// The text is decoded in one step (not through [`Value`]) so `params` keeps the exact
    /// bytes the peer sent, including their field order.
    pub fn parse(text: &str) -> Result<Self, TransportError> {
        // Syntax first, so `-32700` and `-32600` stay distinguishable.
        serde_json::from_str::<serde::de::IgnoredAny>(text)
            .map_err(|error| TransportError::Parse(format!("invalid JSON: {error}")))?;
        let request: Self = serde_json::from_str(text).map_err(|error| {
            TransportError::InvalidRequest(format!("not a JSON-RPC request: {error}"))
        })?;
        if request.jsonrpc != JSONRPC_VERSION {
            return Err(TransportError::InvalidRequest(format!(
                "unsupported jsonrpc version: {}",
                request.jsonrpc
            )));
        }
        Ok(request)
    }

    /// Encodes the message.
    pub fn to_wire(&self) -> Result<String, TransportError> {
        serde_json::to_string(self)
            .map_err(|error| TransportError::Encode(format!("cannot encode request: {error}")))
    }

    /// Decodes `params` into a typed struct.
    pub fn params_as<T: DeserializeOwned>(&self) -> Result<T, TransportError> {
        let params = self.params.as_deref().ok_or_else(|| {
            TransportError::InvalidRequest(format!("request `{}` has no params", self.method))
        })?;
        serde_json::from_str(params.get()).map_err(|error| {
            TransportError::InvalidRequest(format!(
                "params of `{}` do not fit: {error}",
                self.method
            ))
        })
    }

    /// `params` as a plain value, for callers that do not own the shape.
    pub fn params_value(&self) -> Option<Value> {
        self.params
            .as_deref()
            .and_then(|params| serde_json::from_str(params.get()).ok())
    }
}

/// A JSON-RPC notification: a request without an id, never answered.
///
/// Equality is textual for `params`, like [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// Always [`JSONRPC_VERSION`].
    pub jsonrpc: String,
    /// The MCP method name.
    pub method: String,
    /// Method params, kept as the encoded text of the typed params struct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Box<RawValue>>,
}

impl PartialEq for Notification {
    fn eq(&self, other: &Self) -> bool {
        self.jsonrpc == other.jsonrpc
            && self.method == other.method
            && params_text(&self.params) == params_text(&other.params)
    }
}

impl Notification {
    /// A notification without params.
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: method.into(),
            params: None,
        }
    }

    /// A notification whose params are a typed struct.
    pub fn with_params<P>(method: impl Into<String>, params: &P) -> Result<Self, TransportError>
    where
        P: Serialize + ?Sized,
    {
        let mut notification = Self::new(method);
        let text = serde_json::to_string(params)
            .map_err(|error| TransportError::Encode(format!("cannot encode params: {error}")))?;
        notification.params =
            Some(RawValue::from_string(text).map_err(|error| {
                TransportError::Encode(format!("cannot encode params: {error}"))
            })?);
        Ok(notification)
    }

    /// Parses a wire message.
    pub fn parse(text: &str) -> Result<Self, TransportError> {
        serde_json::from_str::<serde::de::IgnoredAny>(text)
            .map_err(|error| TransportError::Parse(format!("invalid JSON: {error}")))?;
        let notification: Self = serde_json::from_str(text).map_err(|error| {
            TransportError::InvalidRequest(format!("not a JSON-RPC notification: {error}"))
        })?;
        if notification.jsonrpc != JSONRPC_VERSION {
            return Err(TransportError::InvalidRequest(format!(
                "unsupported jsonrpc version: {}",
                notification.jsonrpc
            )));
        }
        Ok(notification)
    }

    /// Encodes the message.
    pub fn to_wire(&self) -> Result<String, TransportError> {
        serde_json::to_string(self)
            .map_err(|error| TransportError::Encode(format!("cannot encode notification: {error}")))
    }
}

/// A JSON-RPC response.
///
/// `id` is `None` only for the replies JSON-RPC allows to omit it (`-32700`/`-32600`), where
/// the wire carries `"id":null`. A reply carrying neither `result` nor `error`, or both, is
/// rejected by [`Response::validate`].
///
/// One deliberate narrowing: a bare `"result":null` is indistinguishable from a missing
/// `result` once decoded, so it is treated as "no payload" and rejected. Every conforming MCP
/// result (including `tools/call` and `initialize`) is an object, so this narrowing costs
/// nothing in practice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Always [`JSONRPC_VERSION`].
    pub jsonrpc: String,
    /// The id of the request being answered; `None` becomes `"id":null` on the wire.
    #[serde(default)]
    pub id: Option<RequestId>,
    /// The success payload, if this is a success reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The failure payload, if this is a failure reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// A success reply.
    pub fn result(id: impl Into<RequestId>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: Some(id.into()),
            result: Some(result),
            error: None,
        }
    }

    /// A failure reply. `id` is `None` when the request could not be addressed at all.
    pub fn with_error(id: Option<RequestId>, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Parses a wire message.
    pub fn parse(text: &str) -> Result<Self, TransportError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| TransportError::Parse(format!("invalid JSON: {error}")))?;
        Self::from_value(value)
    }

    /// Validates an already-decoded value.
    pub fn from_value(value: Value) -> Result<Self, TransportError> {
        let response: Self = serde_json::from_value(value).map_err(|error| {
            TransportError::InvalidRequest(format!("not a JSON-RPC response: {error}"))
        })?;
        response.validate()?;
        Ok(response)
    }

    /// Checks the invariants a reply must hold before it may be trusted.
    pub fn validate(&self) -> Result<(), TransportError> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err(TransportError::InvalidRequest(format!(
                "unsupported jsonrpc version: {}",
                self.jsonrpc
            )));
        }
        match (self.result.is_some(), self.error.is_some()) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(TransportError::InvalidRequest(
                "response carries both result and error".to_owned(),
            )),
            (false, false) => Err(TransportError::InvalidRequest(
                "response carries neither result nor error".to_owned(),
            )),
        }
    }

    /// Encodes the message.
    pub fn to_wire(&self) -> Result<String, TransportError> {
        serde_json::to_string(self)
            .map_err(|error| TransportError::Encode(format!("cannot encode response: {error}")))
    }

    /// Rejects a reply addressed to a different request.
    pub fn expect_id(&self, expected: &RequestId) -> Result<(), TransportError> {
        match &self.id {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => Err(TransportError::Protocol(format!(
                "reply id {actual} does not match request id {expected}"
            ))),
            None => Err(TransportError::Protocol(format!(
                "reply has no id, expected {expected}"
            ))),
        }
    }

    /// Consumes the reply, returning its result or the peer's error.
    pub fn into_payload(self) -> Result<Value, TransportError> {
        self.validate()?;
        match (self.result, self.error) {
            (_, Some(error)) => Err(TransportError::Rpc(error)),
            (Some(result), None) => Ok(result),
            (None, None) => Err(TransportError::InvalidRequest(
                "response carries neither result nor error".to_owned(),
            )),
        }
    }
}
