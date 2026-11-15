//! The two mappings that make this crate an `Executor`: durable -> `tools/call`, and
//! `tools/call` -> durable.
//!
//! # `isError` is not a durable failure
//!
//! MCP separates two things the durable model also separates, but *not* at the same place:
//!
//! * a JSON-RPC error means the **call** failed: no `content` reached the model;
//! * `"isError": true` means the **tool** ran and reported a failure (bad input, missing
//!   file, a stack trace). The round trip itself succeeded.
//!
//! Mapping `isError` to [`ExecutionState::Failed`] would tell the durable runtime "the effect
//! did not happen", which is wrong (the tool did run and may even have changed the world) and
//! would make the turn eligible for a retry that runs the tool twice. So a reply that carries
//! `content` -- with or without `isError` -- is [`ExecutionState::Succeeded`], and the whole
//! `tools/call` result (including `isError`) is what gets applied to the model's context. The
//! model sees the tool's error text and decides what to do, exactly as it would locally.
//!
//! # Three outcomes, four states
//!
//! | peer says                                      | durable state               |
//! |------------------------------------------------|-----------------------------|
//! | `content`/`structuredContent`                  | `Succeeded` (payload kept)  |
//! | `task.status` = `working`/`input_required`     | `Pending` + handle          |
//! | `task.status` = `completed`                    | `Succeeded`                 |
//! | `task.status` = `failed`                       | `Failed`                    |
//! | `task.status` = `cancelled`                    | `Cancelled`                 |
//! | error code, no effect was possible             | `Failed`                    |
//! | error code, effect may have run / malformed    | `Unknown`                   |
//!
//! `Pending` is what lets a long-running MCP tool (the `tasks` extension: `tools/call` returns
//! a task pointer instead of content) stay open instead of blocking the transport. `Unknown`
//! is the only honest answer when the peer's answer is unusable: the call may have run.

use agentscope_durable::{ExecutionRequest, ExecutionResult, ExecutionState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::classify;
use crate::error::TransportError;
use crate::jsonrpc::{ErrorObject, Request, RequestId};

/// The MCP method this crate sends side effects through.
pub const METHOD_TOOLS_CALL: &str = "tools/call";

/// `tools/call` params, in spec field order (`name`, then `arguments`).
///
/// `arguments` is omitted when the durable call carries `null` instead of an object: MCP
/// servers validate against the tool's input schema and a literal `null` is not an object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolsCallParams<'a> {
    /// The tool's declared name.
    pub name: &'a str,
    /// The arguments; omitted when the durable call carries `null`.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub arguments: &'a Value,
}

/// Borrows the name/arguments pair of a durable call.
pub fn tools_call_params<'a>(name: &'a str, arguments: &'a Value) -> ToolsCallParams<'a> {
    ToolsCallParams { name, arguments }
}

/// Builds a `tools/call` request.
///
/// This does not consult a catalog: callers that know what the peer advertises should go
/// through [`ToolCatalog::call_request`](crate::ToolCatalog::call_request) (or
/// [`McpExecutor`](crate::McpExecutor), which always does), because sending a call for an
/// undeclared tool is a hard error in this crate, never an empty result.
pub fn tools_call_request(
    id: impl Into<RequestId>,
    name: &str,
    arguments: &Value,
) -> Result<Request, TransportError> {
    if name.trim().is_empty() {
        return Err(TransportError::InvalidRequest(
            "tools/call needs a non-empty tool name".to_owned(),
        ));
    }
    Request::with_params(id, METHOD_TOOLS_CALL, &tools_call_params(name, arguments))
}

/// Builds the `tools/call` request for a durable execution.
///
/// The durable `executionId` becomes the JSON-RPC id, so a reply can be matched to the
/// execution that caused it without a side table.
pub fn tools_call_for_execution(
    id: impl Into<RequestId>,
    request: &ExecutionRequest,
) -> Result<Request, TransportError> {
    tools_call_request(id, &request.call.name, &request.call.arguments)
}

/// A `tools/call` result, as MCP defines it.
///
/// `content` blocks are kept unmodelled ([`Value`]) on purpose: this crate never interprets
/// tool output, it only decides whether the call is over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    /// MCP content blocks.
    #[serde(default)]
    pub content: Vec<Value>,
    /// The tool itself reported a failure. Deliberately *not* a durable failure; see the
    /// module docs.
    #[serde(default)]
    pub is_error: bool,
    /// Machine-readable output for tools that declare an output schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl CallToolResult {
    /// Validates and reads a `tools/call` result.
    ///
    /// A result with neither `content` nor `structuredContent` is rejected: MCP requires one
    /// of them, and accepting an empty object would silently turn a broken peer into a
    /// successful, empty tool result.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        if !value.is_object() {
            return Err("tools/call result is not a JSON object".to_owned());
        }
        if value.get("content").is_none() && value.get("structuredContent").is_none() {
            return Err(
                "tools/call result carries neither `content` nor `structuredContent`".to_owned(),
            );
        }
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())
    }

    /// Concatenated text blocks, for logs and error messages.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for block in &self.content {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                    text.push_str(chunk);
                }
            }
        }
        text
    }
}

/// State of an MCP task (the protocol's long-running-operation extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The peer is working on it.
    Working,
    /// The peer needs more input before it can finish.
    InputRequired,
    /// The work is done and the task carries the result.
    Completed,
    /// The work is done and failed.
    Failed,
    /// The work was cancelled.
    Cancelled,
}

impl TaskStatus {
    /// Whether the task can no longer change.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// The `task` pointer a `tools/call` result carries when the peer accepted the call without
/// finishing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPointer {
    /// The task's identity; this is the durable execution handle.
    pub task_id: String,
    /// Where the task is.
    pub status: TaskStatus,
    /// How often the peer suggests polling, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<u64>,
}

impl TaskPointer {
    /// Reads a task pointer out of a `tools/call` result.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let task = value
            .get("task")
            .ok_or_else(|| "tools/call result has no `task` member".to_owned())?;
        let pointer: Self = serde_json::from_value(task.clone())
            .map_err(|error| format!("malformed `task` member: {error}"))?;
        if pointer.task_id.trim().is_empty() {
            return Err("`task.taskId` must not be empty".to_owned());
        }
        Ok(pointer)
    }
}

/// Maps a `tools/call` result onto a durable execution result.
///
/// This never fails: anything unreadable becomes [`ExecutionState::Unknown`] with the reason
/// in `error`, because "we could not understand the answer" must not be reported as "the
/// tool did not run".
pub fn tools_call_result_to_execution(payload: &Value) -> ExecutionResult {
    if payload.get("task").is_some() {
        return match TaskPointer::from_value(payload) {
            Ok(pointer) => task_to_execution(&pointer, payload),
            Err(reason) => ExecutionResult::unknown(format!(
                "MCP tools/call task pointer is unusable: {reason}"
            )),
        };
    }
    match CallToolResult::from_value(payload) {
        // `is_error` is intentionally ignored here; see the module docs.
        Ok(_result) => ExecutionResult::succeeded(payload.clone()),
        Err(reason) => {
            ExecutionResult::unknown(format!("MCP tools/call result is unusable: {reason}"))
        }
    }
}

fn task_to_execution(pointer: &TaskPointer, payload: &Value) -> ExecutionResult {
    match pointer.status {
        TaskStatus::Working | TaskStatus::InputRequired => pending(&pointer.task_id),
        TaskStatus::Completed => ExecutionResult::succeeded(payload.clone()),
        TaskStatus::Failed => ExecutionResult {
            state: ExecutionState::Failed,
            handle: Some(pointer.task_id.clone()),
            result: Value::Null,
            error: Some(format!("MCP task {} failed", pointer.task_id)),
        },
        TaskStatus::Cancelled => ExecutionResult {
            state: ExecutionState::Cancelled,
            handle: Some(pointer.task_id.clone()),
            result: Value::Null,
            error: None,
        },
    }
}

/// Maps a JSON-RPC error object onto a durable execution result.
///
/// A deterministic rejection ("the peer never started the call because it did not like it")
/// is [`ExecutionState::Failed`]; anything else is [`ExecutionState::Unknown`], because
/// [`ExecutionResult::unknown`] is the only honest record when the effect may have run.
pub fn error_to_execution_result(error: &ErrorObject) -> ExecutionResult {
    let class = classify(error.code);
    let mut message = format!("MCP {} ({}): {}", class.name, error.code, error.message);
    if let Some(data) = &error.data {
        message.push_str(&format!("; data={data}"));
    }
    if class.effect_may_have_run {
        ExecutionResult::unknown(message)
    } else {
        ExecutionResult {
            state: ExecutionState::Failed,
            handle: None,
            result: Value::Null,
            error: Some(message),
        }
    }
}

/// The durable result for "this tool is not advertised, so nothing was sent".
///
/// A missing tool never ran, so this is a `Failed` execution (the model sees the reason)
/// rather than an `Unknown` one. The durable `NOT_FOUND` classification lives on the
/// [`TransportError::ToolNotAdvertised`] path, where the caller wants an error, not a tool
/// result.
pub fn tool_not_advertised_result(name: &str) -> ExecutionResult {
    ExecutionResult {
        state: ExecutionState::Failed,
        handle: None,
        result: Value::Null,
        error: Some(format!(
            "MCP tool `{name}` is not advertised by the peer's tools/list"
        )),
    }
}

fn pending(handle: &str) -> ExecutionResult {
    ExecutionResult {
        state: ExecutionState::Pending,
        handle: Some(handle.to_owned()),
        result: Value::Null,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn execution_request(name: &str, arguments: Value) -> ExecutionRequest {
        ExecutionRequest::new(
            "turn-1:call-1",
            agentscope_durable::ThreadKey::new(
                agentscope_durable::SessionKey::new("test", "session-1").expect("session key"),
                "thread-1",
            )
            .expect("thread key"),
            "turn-1",
            agentscope_durable::ToolCall::new("call-1", "mcp", name, arguments).expect("tool call"),
        )
        .expect("execution request")
    }

    #[test]
    fn a_durable_call_becomes_tools_call_with_the_same_name_and_arguments() {
        let arguments = json!({ "text": "hi", "times": 2 });
        let execution = execution_request("echo", arguments.clone());
        let request = tools_call_for_execution(RequestId::number(7), &execution).expect("request");

        assert_eq!(request.method, METHOD_TOOLS_CALL);
        assert_eq!(request.id, RequestId::Number(7));
        let params = request.params_value().expect("params");
        assert_eq!(params["name"], json!("echo"));
        assert_eq!(params["arguments"], arguments);
    }

    #[test]
    fn null_arguments_are_omitted_instead_of_sent_as_null() {
        let execution = execution_request("ping", Value::Null);
        let request = tools_call_for_execution(1, &execution).expect("request");
        let params = request.params_value().expect("params");
        assert_eq!(params, json!({ "name": "ping" }));
    }

    #[test]
    fn an_empty_tool_name_is_rejected_before_the_wire() {
        let error = tools_call_request(1, "  ", &json!({})).expect_err("must be rejected");
        assert_eq!(error.code(), Some(crate::jsonrpc::codes::INVALID_REQUEST));
    }

    #[test]
    fn is_error_is_a_successful_round_trip_not_a_durable_failure() {
        let payload = json!({
            "content": [{ "type": "text", "text": "no such file: a.txt" }],
            "isError": true
        });
        let result = tools_call_result_to_execution(&payload);
        assert_eq!(result.state, ExecutionState::Succeeded);
        assert_eq!(result.result, payload);
        assert!(result.error.is_none());

        let parsed = CallToolResult::from_value(&payload).expect("result");
        assert!(parsed.is_error);
        assert_eq!(parsed.text(), "no such file: a.txt");
    }

    #[test]
    fn a_task_pointer_is_pending_and_its_terminal_states_are_not() {
        let working =
            json!({ "task": { "taskId": "task-9", "status": "working", "pollInterval": 250 } });
        let result = tools_call_result_to_execution(&working);
        assert_eq!(result.state, ExecutionState::Pending);
        assert_eq!(result.handle.as_deref(), Some("task-9"));

        let completed = json!({ "task": { "taskId": "task-9", "status": "completed" } });
        assert_eq!(
            tools_call_result_to_execution(&completed).state,
            ExecutionState::Succeeded
        );

        let failed = json!({ "task": { "taskId": "task-9", "status": "failed" } });
        let failed = tools_call_result_to_execution(&failed);
        assert_eq!(failed.state, ExecutionState::Failed);
        assert_eq!(failed.handle.as_deref(), Some("task-9"));

        let cancelled = json!({ "task": { "taskId": "task-9", "status": "cancelled" } });
        assert_eq!(
            tools_call_result_to_execution(&cancelled).state,
            ExecutionState::Cancelled
        );
    }

    #[test]
    fn an_unreadable_result_is_unknown_rather_than_success_or_failure() {
        for payload in [
            json!(42),
            json!({}),
            json!({ "task": { "status": "working" } }),
        ] {
            let result = tools_call_result_to_execution(&payload);
            assert_eq!(result.state, ExecutionState::Unknown, "payload: {payload}");
            assert!(result.error.is_some());
        }
    }

    #[test]
    fn rpc_errors_are_failed_or_unknown_depending_on_effect_knowledge() {
        let rejected = ErrorObject::invalid_params("arguments.q is required");
        let rejected = error_to_execution_result(&rejected);
        assert_eq!(rejected.state, ExecutionState::Failed);
        assert!(rejected
            .error
            .as_deref()
            .expect("message")
            .contains("INVALID_PARAMS"));

        let internal = ErrorObject::internal("kaboom");
        let internal = error_to_execution_result(&internal);
        assert_eq!(internal.state, ExecutionState::Unknown);
        assert!(internal
            .error
            .as_deref()
            .expect("message")
            .contains("kaboom"));

        let missing = error_to_execution_result(&ErrorObject::tool_not_found("nope"));
        assert_eq!(missing.state, ExecutionState::Failed);
    }

    #[test]
    fn an_unadvertised_tool_is_an_explicit_failure() {
        let result = tool_not_advertised_result("ghost");
        assert_eq!(result.state, ExecutionState::Failed);
        assert!(result.error.as_deref().expect("message").contains("ghost"));
    }
}
