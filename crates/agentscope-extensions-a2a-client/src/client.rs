//! JSON-RPC client for calling remote A2A agents.
//!
//! Mirrors the client half of `io.agentscope.core.a2a.agent` (`A2aAgent` +
//! `a2a-java-sdk` `Client`), reduced to the protocol surface: given an
//! [`AgentCardResolver`], issue the A2A JSON-RPC methods and decode the results.
//!
//! HTTP access goes through the injectable [`JsonRpcTransport`] trait, so the client is
//! fully testable offline; [`ReqwestJsonRpcTransport`] is the production implementation.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
/// Mirrors the role of the Java `a2a-java-sdk` `ClientTransport`: the client only needs
/// "send one envelope, get one envelope back".
pub trait JsonRpcTransport: Send + Sync {
    fn send(
        &self,
        url: &str,
        request: &JsonRpcRequest,
        headers: &BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<JsonRpcResponse, A2aError>> + Send + '_>>;
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
    pub async fn agent_card(&self) -> Result<AgentCard, A2aError> {
        self.resolver.get_agent_card(&self.agent_name).await
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
            Some("message/send")
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
            Some("tasks/get")
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
            Some("tasks/cancel")
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
}
