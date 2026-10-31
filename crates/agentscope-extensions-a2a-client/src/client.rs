//! JSON-RPC client for calling remote A2A agents.
//!
//! Mirrors the client half of `io.agentscope.core.a2a.agent` (`A2aAgent` +
//! `a2a-java-sdk` `Client`), reduced to the protocol surface: given an
//! [`AgentCardResolver`], issue the A2A JSON-RPC methods and decode the results.
//!
//! HTTP access goes through the injectable [`JsonRpcTransport`] trait, so the client is
//! fully testable offline; [`ReqwestJsonRpcTransport`] is the production implementation.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::card::AgentCardResolver;
use crate::error::A2aError;
use crate::model::{
    methods, AgentCard, JsonRpcRequest, JsonRpcResponse, Message, MessageSendParams, StreamEvent,
    Task, TaskIdParams, TaskQueryParams, TaskState, TaskStatus,
};

/// Result of `message/send`, which the A2A spec types as `Message | Task`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SendResult {
    /// The agent answered synchronously with a message.
    Message(Message),
    /// The agent created (or updated) a task.
    Task(Box<Task>),
}

impl SendResult {
    /// The task, when the response was a task.
    pub fn as_task(&self) -> Option<&Task> {
        match self {
            SendResult::Task(task) => Some(task),
            SendResult::Message(_) => None,
        }
    }

    /// The message, when the response was a message.
    pub fn as_message(&self) -> Option<&Message> {
        match self {
            SendResult::Message(message) => Some(message),
            SendResult::Task(_) => None,
        }
    }
}

/// Transport abstraction for A2A JSON-RPC calls.
///
/// Mirrors the role of the Java `a2a-java-sdk` `ClientTransport`: the client needs
/// "send one envelope, get one envelope back" for the unary methods and "send one envelope,
/// read the envelopes as they arrive" for the streaming ones.
pub trait JsonRpcTransport: Send + Sync {
    fn send(
        &self,
        url: &str,
        request: &JsonRpcRequest,
        headers: &BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<JsonRpcResponse, A2aError>> + Send + '_>>;

    /// Sends one request and yields each JSON-RPC response as it arrives.
    ///
    /// `message/stream` answers `text/event-stream` with one response per A2A event, so this is
    /// what keeps a caller from waiting for the whole run (Java's SDK hands the Reactor stream
    /// straight to `A2aAgent#streamEvents`). Parameters are owned and the returned stream is
    /// `'static`, so the caller can move it across tasks.
    fn send_stream(
        &self,
        url: String,
        request: JsonRpcRequest,
        headers: BTreeMap<String, String>,
    ) -> BoxStream<'static, Result<JsonRpcResponse, A2aError>>;
}

/// [`JsonRpcTransport`] backed by `reqwest`.
#[derive(Debug, Clone, Default)]
pub struct ReqwestJsonRpcTransport {
    client: reqwest::Client,
}

impl ReqwestJsonRpcTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl JsonRpcTransport for ReqwestJsonRpcTransport {
    fn send(
        &self,
        url: &str,
        request: &JsonRpcRequest,
        headers: &BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<JsonRpcResponse, A2aError>> + Send + '_>> {
        let client = self.client.clone();
        let url = url.to_string();
        let request = request.clone();
        let headers = headers.clone();
        Box::pin(async move {
            let mut builder = client.post(&url).json(&request);
            for (name, value) in &headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            let response = builder.send().await.map_err(A2aError::transport)?;
            let status = response.status();
            if !status.is_success() {
                return Err(A2aError::Http {
                    status: status.as_u16(),
                    message: format!("a2a request to {url} failed"),
                });
            }
            response
                .json::<JsonRpcResponse>()
                .await
                .map_err(A2aError::transport)
        })
    }

    fn send_stream(
        &self,
        url: String,
        request: JsonRpcRequest,
        headers: BTreeMap<String, String>,
    ) -> BoxStream<'static, Result<JsonRpcResponse, A2aError>> {
        let state = HttpStreamState {
            client: self.client.clone(),
            url,
            request,
            headers,
            response: None,
            decoder: SseDecoder::new(),
            pending: VecDeque::new(),
            done: false,
        };
        Box::pin(futures::stream::unfold(state, next_http_stream_item))
    }
}

/// State of one streaming HTTP request, carried through `futures::stream::unfold`.
struct HttpStreamState {
    client: reqwest::Client,
    url: String,
    request: JsonRpcRequest,
    headers: BTreeMap<String, String>,
    response: Option<reqwest::Response>,
    decoder: SseDecoder,
    pending: VecDeque<Result<JsonRpcResponse, A2aError>>,
    done: bool,
}

/// One poll of a streaming HTTP request: opens the request, then decodes body chunks as they
/// arrive so a caller observes each frame before the run finishes.
async fn next_http_stream_item(
    mut state: HttpStreamState,
) -> Option<(Result<JsonRpcResponse, A2aError>, HttpStreamState)> {
    loop {
        if let Some(item) = state.pending.pop_front() {
            return Some((item, state));
        }
        if state.done {
            return None;
        }
        if state.response.is_none() {
            let mut builder = state.client.post(&state.url).json(&state.request);
            for (name, value) in &state.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            match builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        state.response = Some(response);
                    } else {
                        state.pending.push_back(Err(A2aError::Http {
                            status: status.as_u16(),
                            message: format!("a2a stream request to {} failed", state.url),
                        }));
                        state.done = true;
                    }
                }
                Err(error) => {
                    state.pending.push_back(Err(A2aError::transport(error)));
                    state.done = true;
                }
            }
            continue;
        }
        let chunk = match state
            .response
            .as_mut()
            .expect("response is set")
            .chunk()
            .await
        {
            Ok(chunk) => chunk,
            Err(error) => {
                state.pending.push_back(Err(A2aError::transport(error)));
                state.done = true;
                continue;
            }
        };
        let decoded = match chunk {
            Some(bytes) => state.decoder.push(&String::from_utf8_lossy(&bytes)),
            None => {
                state.done = true;
                state.decoder.finish()
            }
        };
        match decoded {
            Ok(responses) => state.pending.extend(responses.into_iter().map(Ok)),
            Err(error) => {
                state.pending.push_back(Err(error));
                state.done = true;
            }
        }
    }
}

/// Client for a single remote A2A agent.
///
/// Mirrors the request/response subset of Java `A2aAgent`: the agent card is resolved once
/// per call, then the JSON-RPC method is dispatched to the card's `url`.
#[derive(Clone)]
pub struct A2aClient {
    resolver: Arc<dyn AgentCardResolver>,
    transport: Arc<dyn JsonRpcTransport>,
    agent_name: String,
    auth_headers: BTreeMap<String, String>,
    /// Method name constants, exposed for callers that need them.
    request_counter: Arc<std::sync::atomic::AtomicI64>,
}

impl std::fmt::Debug for A2aClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aClient")
            .field("agent_name", &self.agent_name)
            .field(
                "auth_headers",
                &self.auth_headers.keys().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl A2aClient {
    /// Creates a client that resolves the remote card through `resolver`.
    pub fn new(agent_name: impl Into<String>, resolver: Arc<dyn AgentCardResolver>) -> Self {
        Self {
            resolver,
            transport: Arc::new(ReqwestJsonRpcTransport::new()),
            agent_name: agent_name.into(),
            auth_headers: BTreeMap::new(),
            request_counter: Arc::new(std::sync::atomic::AtomicI64::new(1)),
        }
    }

    /// Creates a client bound to an already-known card (Java `A2aAgent.Builder#agentCard`).
    pub fn with_card(agent_name: impl Into<String>, card: AgentCard) -> Self {
        Self::new(
            agent_name,
            Arc::new(crate::card::FixedAgentCardResolver::new(card)),
        )
    }

    /// Injects a custom JSON-RPC transport (used by tests).
    pub fn with_transport(mut self, transport: Arc<dyn JsonRpcTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Sets the headers sent with every request.
    pub fn with_auth_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.auth_headers = headers;
        self
    }

    /// The agent name this client serves.
    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    /// Resolves the remote agent card.
    ///
    /// Fails fast, mirroring Java `A2aAgent#startCall`: when the resolver cannot produce a card
    /// the error names the agent and the resolver instead of letting the call proceed with an
    /// empty endpoint.
    pub async fn agent_card(&self) -> Result<AgentCard, A2aError> {
        self.resolver
            .get_agent_card(&self.agent_name)
            .await
            .map_err(|error| {
                A2aError::card_unavailable(&self.agent_name, self.resolver.resolver_name(), error)
            })
    }

    /// `message/send`: sends a message and waits for the agent's reply.
    pub async fn message_send(&self, params: MessageSendParams) -> Result<SendResult, A2aError> {
        let result = self
            .call(
                methods::MESSAGE_SEND,
                serde_json::to_value(params).map_err(A2aError::json)?,
            )
            .await?;
        serde_json::from_value(result).map_err(A2aError::json)
    }

    /// `message/stream`: sends a message and collects the streaming response.
    ///
    /// The raw JSON-RPC response payload is decoded as a list of [`StreamEvent`]s; a
    /// transport that returns a single event is tolerated.
    pub async fn message_stream(
        &self,
        params: MessageSendParams,
    ) -> Result<Vec<StreamEvent>, A2aError> {
        let result = self
            .call(
                methods::MESSAGE_STREAM,
                serde_json::to_value(params).map_err(A2aError::json)?,
            )
            .await?;
        decode_stream_result(result)
    }

    /// `message/stream`, relayed frame by frame instead of buffered into one reply.
    ///
    /// Each SSE frame becomes one [`StreamEvent`] as soon as it reaches the client, mirroring Java
    /// `A2aAgent#streamEvents`, which subscribes to the SDK's stream so deltas reach the caller
    /// while the remote run is still open. [`message_stream`](Self::message_stream) stays the
    /// collecting variant used when only the terminal reply matters.
    pub async fn message_stream_incremental(
        &self,
        params: MessageSendParams,
    ) -> Result<BoxStream<'static, Result<StreamEvent, A2aError>>, A2aError> {
        let card = self.agent_card().await?;
        let id = Value::from(
            self.request_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        let request = JsonRpcRequest::new(
            methods::MESSAGE_STREAM,
            Some(serde_json::to_value(params).map_err(A2aError::json)?),
            Some(id),
        );
        let responses = self
            .transport
            .send_stream(card.url, request, self.auth_headers.clone());
        Ok(Box::pin(futures::StreamExt::flat_map(
            responses,
            |response| {
                let events: Vec<Result<StreamEvent, A2aError>> = match response
                    .and_then(unwrap_json_rpc)
                    .and_then(decode_stream_result)
                {
                    Ok(events) => events.into_iter().map(Ok).collect(),
                    Err(error) => vec![Err(error)],
                };
                futures::stream::iter(events)
            },
        )))
    }

    /// `tasks/get`: fetches a task by id.
    pub async fn tasks_get(&self, params: TaskQueryParams) -> Result<Task, A2aError> {
        let result = self
            .call(
                methods::TASKS_GET,
                serde_json::to_value(params).map_err(A2aError::json)?,
            )
            .await?;
        serde_json::from_value(result).map_err(A2aError::json)
    }

    /// `tasks/cancel`: cancels a task.
    pub async fn tasks_cancel(&self, params: TaskIdParams) -> Result<Task, A2aError> {
        let result = self
            .call(
                methods::TASKS_CANCEL,
                serde_json::to_value(params).map_err(A2aError::json)?,
            )
            .await?;
        serde_json::from_value(result).map_err(A2aError::json)
    }

    /// `agent/getAuthenticatedExtendedCard`.
    pub async fn authenticated_extended_card(&self) -> Result<AgentCard, A2aError> {
        let result = self.call(methods::AGENT_CARD_GET, Value::Null).await?;
        serde_json::from_value(result).map_err(A2aError::json)
    }

    /// Sends one JSON-RPC request and unwraps the `result` / `error` envelope.
    async fn call(&self, method: &str, params: Value) -> Result<Value, A2aError> {
        let card = self.agent_card().await?;
        let id = Value::from(
            self.request_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        let request = JsonRpcRequest::new(method, Some(params), Some(id));
        let response = self
            .transport
            .send(&card.url, &request, &self.auth_headers)
            .await?;
        unwrap_json_rpc(response)
    }
}

/// Turns a JSON-RPC response envelope into its `result` value or an error.
///
/// Mirrors the error handling of the Java A2A client, which raises the peer's
/// JSON-RPC error verbatim.
pub fn unwrap_json_rpc(response: JsonRpcResponse) -> Result<Value, A2aError> {
    if let Some(error) = response.error {
        return Err(A2aError::JsonRpc {
            code: error.code,
            message: error.message,
        });
    }
    response
        .result
        .ok_or_else(|| A2aError::InvalidRequest("json-rpc response carried no result".into()))
}

/// Decodes a `message/stream` result, tolerating a single event instead of an array.
pub fn decode_stream_result(result: Value) -> Result<Vec<StreamEvent>, A2aError> {
    match result {
        Value::Array(_) => serde_json::from_value(result).map_err(A2aError::json),
        Value::Null => Ok(Vec::new()),
        single => {
            let event: StreamEvent = serde_json::from_value(single).map_err(A2aError::json)?;
            Ok(vec![event])
        }
    }
}

/// A minimal SSE frame decoder for A2A streaming responses.
///
/// Each frame is `data: <json>` terminated by a blank line, matching
/// [`crate::model`]'s JSON-RPC carrier. Pure function so it can be unit-tested
/// without a network.
pub fn parse_sse_stream(body: &str) -> Result<Vec<StreamEvent>, A2aError> {
    let mut events = Vec::new();
    for frame in body.split("\n\n") {
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&data).map_err(A2aError::json)?;
        // A streaming response may wrap each event in a JSON-RPC envelope.
        let payload = match value.get("jsonrpc") {
            Some(_) => unwrap_json_rpc(serde_json::from_value(value).map_err(A2aError::json)?)?,
            None => value,
        };
        events.push(serde_json::from_value(payload).map_err(A2aError::json)?);
    }
    Ok(events)
}

/// Incremental SSE decoder for the A2A JSON-RPC streaming carrier.
///
/// A streaming method answers `text/event-stream` with one JSON-RPC response per frame
/// (`data: <json>`, terminated by a blank line). The decoder is stateful so a chunk boundary
/// never splits a frame, which is what lets [`JsonRpcTransport::send_stream`] hand each event to
/// the caller as soon as it arrives.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one body chunk and returns the responses it completed.
    pub fn push(&mut self, chunk: &str) -> Result<Vec<JsonRpcResponse>, A2aError> {
        self.buffer.push_str(&chunk.replace("\r\n", "\n"));
        let mut responses = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let frame: String = self.buffer.drain(..index + 2).collect();
            if let Some(response) = decode_sse_frame(&frame)? {
                responses.push(response);
            }
        }
        Ok(responses)
    }

    /// Flushes the trailing bytes: a frame without its blank line, or a whole-body reply from a
    /// server that answered with plain JSON instead of SSE.
    pub fn finish(&mut self) -> Result<Vec<JsonRpcResponse>, A2aError> {
        let rest = std::mem::take(&mut self.buffer);
        if rest.trim().is_empty() {
            return Ok(Vec::new());
        }
        if !rest
            .lines()
            .any(|line| line.trim_start().starts_with("data:"))
        {
            return decode_plain_body(&rest);
        }
        Ok(decode_sse_frame(&rest)?.into_iter().collect())
    }
}

/// Decodes one SSE frame into a JSON-RPC response.
///
/// `[DONE]` and keep-alive comments decode to `None`, mirroring [`parse_sse_stream`].
fn decode_sse_frame(frame: &str) -> Result<Option<JsonRpcResponse>, A2aError> {
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("");
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&data).map_err(A2aError::json)?;
    to_json_rpc_response(value).map(Some)
}

/// Decodes a body that is not SSE: one response object or an array of them.
fn decode_plain_body(body: &str) -> Result<Vec<JsonRpcResponse>, A2aError> {
    let value: Value = serde_json::from_str(body).map_err(A2aError::json)?;
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(to_json_rpc_response)
            .collect::<Result<Vec<_>, _>>(),
        single => Ok(vec![to_json_rpc_response(single)?]),
    }
}

/// Wraps a bare `result` payload into an envelope, or decodes an existing envelope.
fn to_json_rpc_response(value: Value) -> Result<JsonRpcResponse, A2aError> {
    if value.get("jsonrpc").is_some() {
        serde_json::from_value(value).map_err(A2aError::json)
    } else {
        Ok(JsonRpcResponse::success(value, None))
    }
}

/// Tracks the terminal state of a task, mirroring the client-side polling contract of
/// Java `A2aAgent#doCall` (which blocks until a terminal status update arrives).
pub fn is_terminal_state(state: TaskState) -> bool {
    state.is_terminal()
}
/// Whether a status carries the terminal flag, mirroring the A2A `final` field usage.
pub fn is_final_status(status: &TaskStatus) -> bool {
    status.state.is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageRole, Part, TaskStatusUpdateEvent};

    struct StubTransport {
        response: JsonRpcResponse,
        last_url: std::sync::Mutex<Option<String>>,
        last_method: std::sync::Mutex<Option<String>>,
    }

    impl JsonRpcTransport for StubTransport {
        fn send(
            &self,
            url: &str,
            request: &JsonRpcRequest,
            _headers: &BTreeMap<String, String>,
        ) -> Pin<Box<dyn Future<Output = Result<JsonRpcResponse, A2aError>> + Send + '_>> {
            *self.last_url.lock().unwrap() = Some(url.to_string());
            *self.last_method.lock().unwrap() = Some(request.method.clone());
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }

        fn send_stream(
            &self,
            url: String,
            request: JsonRpcRequest,
            _headers: BTreeMap<String, String>,
        ) -> BoxStream<'static, Result<JsonRpcResponse, A2aError>> {
            *self.last_url.lock().unwrap() = Some(url);
            *self.last_method.lock().unwrap() = Some(request.method.clone());
            let response = self.response.clone();
            Box::pin(futures::stream::once(async move { Ok(response) }))
        }
    }

    fn stub_client(response: JsonRpcResponse) -> (A2aClient, Arc<StubTransport>) {
        let transport = Arc::new(StubTransport {
            response,
            last_url: std::sync::Mutex::new(None),
            last_method: std::sync::Mutex::new(None),
        });
        let card = AgentCard::new("remote", "http://remote:9000");
        let client = A2aClient::with_card("remote", card).with_transport(transport.clone());
        (client, transport)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn message_send_decodes_a_message_reply() {
        let message =
            crate::model::Message::new("m-1", MessageRole::Agent, vec![Part::text("hi back")]);
        let (client, transport) = stub_client(JsonRpcResponse::success(
            serde_json::to_value(&message).unwrap(),
            Some(Value::from(1)),
        ));

        let params = MessageSendParams {
            message: crate::model::Message::new("m-0", MessageRole::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
        };
        let result = rt().block_on(client.message_send(params)).unwrap();

        assert_eq!(
            transport.last_method.lock().unwrap().as_deref(),
            Some(methods::MESSAGE_SEND)
        );
        assert_eq!(
            transport.last_url.lock().unwrap().as_deref(),
            Some("http://remote:9000")
        );
        assert_eq!(result.as_message().unwrap().message_id, "m-1");
    }

    #[test]
    fn message_send_surfaces_jsonrpc_errors() {
        let (client, _) = stub_client(JsonRpcResponse::failure(
            crate::model::JsonRpcError::new(crate::model::JsonRpcError::TASK_NOT_FOUND, "nope"),
            Some(Value::from(1)),
        ));
        let params = MessageSendParams {
            message: crate::model::Message::new("m", MessageRole::User, vec![Part::text("x")]),
            configuration: None,
            metadata: None,
        };
        let err = rt().block_on(client.message_send(params)).unwrap_err();
        assert!(matches!(err, A2aError::JsonRpc { code: -32001, .. }));
    }

    #[test]
    fn tasks_get_decodes_a_task() {
        let task = Task::new("t-1", TaskStatus::new(TaskState::Working));
        let (client, transport) = stub_client(JsonRpcResponse::success(
            serde_json::to_value(&task).unwrap(),
            None,
        ));
        let fetched = rt()
            .block_on(client.tasks_get(TaskQueryParams {
                id: "t-1".to_string(),
                history_length: None,
                metadata: None,
            }))
            .unwrap();
        assert_eq!(fetched.id, "t-1");
        assert_eq!(
            transport.last_method.lock().unwrap().as_deref(),
            Some(methods::TASKS_GET)
        );
    }

    #[test]
    fn tasks_cancel_decodes_a_task() {
        let task = Task::new("t-1", TaskStatus::new(TaskState::Canceled));
        let (client, transport) = stub_client(JsonRpcResponse::success(
            serde_json::to_value(&task).unwrap(),
            None,
        ));
        let canceled = rt()
            .block_on(client.tasks_cancel(TaskIdParams {
                id: "t-1".to_string(),
                metadata: None,
            }))
            .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        assert_eq!(
            transport.last_method.lock().unwrap().as_deref(),
            Some(methods::TASKS_CANCEL)
        );
    }

    #[test]
    fn message_stream_accepts_a_single_event_object() {
        let event = TaskStatusUpdateEvent::new("t-1", TaskStatus::new(TaskState::Working), false);
        let result = decode_stream_result(serde_json::to_value(&event).unwrap()).unwrap();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], StreamEvent::StatusUpdate(_)));
    }

    #[test]
    fn message_stream_accepts_an_array_of_events() {
        let events = vec![
            serde_json::to_value(TaskStatusUpdateEvent::new(
                "t-1",
                TaskStatus::new(TaskState::Working),
                false,
            ))
            .unwrap(),
            serde_json::to_value(Task::new("t-1", TaskStatus::new(TaskState::Completed))).unwrap(),
        ];
        let result = decode_stream_result(Value::Array(events)).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parse_sse_stream_decodes_data_frames_and_skips_done() {
        let event = TaskStatusUpdateEvent::new("t-1", TaskStatus::new(TaskState::Working), false);
        let json = serde_json::to_string(&event).unwrap();
        let body = format!("data: {json}\n\ndata: [DONE]\n\n");
        let events = parse_sse_stream(&body).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::StatusUpdate(_)));
    }

    #[test]
    fn parse_sse_stream_unwraps_jsonrpc_envelopes() {
        let task = Task::new("t-9", TaskStatus::new(TaskState::Completed));
        let envelope = JsonRpcResponse::success(serde_json::to_value(&task).unwrap(), None);
        let body = format!("data: {}\n\n", serde_json::to_string(&envelope).unwrap());
        let events = parse_sse_stream(&body).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Task(t) if t.id == "t-9"));
    }

    #[test]
    fn terminal_states_are_detected() {
        assert!(is_terminal_state(TaskState::Completed));
        assert!(is_terminal_state(TaskState::Rejected));
        assert!(!is_terminal_state(TaskState::AuthRequired));
        assert!(is_final_status(&TaskStatus::new(TaskState::Failed)));
    }

    #[test]
    fn sse_decoder_never_splits_a_frame_across_chunks() {
        let task = Task::new("t-1", TaskStatus::new(TaskState::Working));
        let frame = format!("data: {}\r\n\r\n", serde_json::to_string(&task).unwrap());
        let split = frame.len() / 2;
        let mut decoder = SseDecoder::new();

        // The first half is incomplete: nothing is emitted yet.
        assert!(decoder.push(&frame[..split]).unwrap().is_empty());
        let responses = decoder.push(&frame[split..]).unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].result.as_ref().unwrap()["id"], "t-1");
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn sse_decoder_skips_done_and_unwraps_envelopes() {
        let update = TaskStatusUpdateEvent::new("t-1", TaskStatus::new(TaskState::Completed), true);
        let envelope = JsonRpcResponse::success(serde_json::to_value(&update).unwrap(), None);
        let body = format!(
            "data: [DONE]\n\n: keep-alive\n\ndata: {}\n\n",
            serde_json::to_string(&envelope).unwrap()
        );
        let responses = SseDecoder::new().push(&body).unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].result.as_ref().unwrap()["final"], true);
    }

    #[test]
    fn sse_decoder_finishes_a_plain_json_body() {
        let task = Task::new("t-2", TaskStatus::new(TaskState::Completed));
        let body =
            serde_json::to_string(&serde_json::json!([serde_json::to_value(&task).unwrap()]))
                .unwrap();
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(&body).unwrap().is_empty());
        let responses = decoder.finish().unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].result.as_ref().unwrap()["id"], "t-2");
    }

    /// Transport whose stream is fed by the test, so a buffering client would hang.
    struct ChannelTransport {
        receiver: std::sync::Mutex<
            Option<futures::channel::mpsc::UnboundedReceiver<Result<JsonRpcResponse, A2aError>>>,
        >,
    }

    impl JsonRpcTransport for ChannelTransport {
        fn send(
            &self,
            _url: &str,
            _request: &JsonRpcRequest,
            _headers: &BTreeMap<String, String>,
        ) -> Pin<Box<dyn Future<Output = Result<JsonRpcResponse, A2aError>> + Send + '_>> {
            unreachable!("only the streaming method is used")
        }

        fn send_stream(
            &self,
            _url: String,
            request: JsonRpcRequest,
            _headers: BTreeMap<String, String>,
        ) -> BoxStream<'static, Result<JsonRpcResponse, A2aError>> {
            assert_eq!(request.method, methods::MESSAGE_STREAM);
            let receiver = self
                .receiver
                .lock()
                .unwrap()
                .take()
                .expect("one stream per transport");
            Box::pin(receiver)
        }
    }

    // Java: A2aAgent#streamEvents — each frame is delivered before the run finishes.
    #[test]
    fn message_stream_incremental_emits_each_frame_as_it_arrives() {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let transport = Arc::new(ChannelTransport {
            receiver: std::sync::Mutex::new(Some(receiver)),
        });
        let client = A2aClient::with_card("remote", AgentCard::new("remote", "http://remote:9000"))
            .with_transport(transport);
        let params = MessageSendParams {
            message: crate::model::Message::new("m-0", MessageRole::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
        };

        let mut events = rt()
            .block_on(client.message_stream_incremental(params))
            .unwrap();

        // Only the first frame is fed: the client must yield it without waiting for more.
        sender
            .unbounded_send(Ok(JsonRpcResponse::success(
                serde_json::to_value(TaskStatusUpdateEvent::new(
                    "t-1",
                    TaskStatus::new(TaskState::Working),
                    false,
                ))
                .unwrap(),
                None,
            )))
            .unwrap();
        let first = rt()
            .block_on(futures::StreamExt::next(&mut events))
            .unwrap();
        assert!(matches!(first.unwrap(), StreamEvent::StatusUpdate(_)));

        // The terminal frame follows on the same stream.
        sender
            .unbounded_send(Ok(JsonRpcResponse::success(
                serde_json::to_value(Task::new("t-1", TaskStatus::new(TaskState::Completed)))
                    .unwrap(),
                None,
            )))
            .unwrap();
        let last = rt()
            .block_on(futures::StreamExt::next(&mut events))
            .unwrap();
        assert!(matches!(last.unwrap(), StreamEvent::Task(_)));

        drop(sender);
        assert!(rt()
            .block_on(futures::StreamExt::next(&mut events))
            .is_none());
    }
}
