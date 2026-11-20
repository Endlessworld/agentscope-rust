//! `Executor` implementation: durable tool calls go out as MCP `tools/call`.
//!
//! # Capabilities, honestly
//!
//! [`ExecutorCapabilities`](agentscope_durable::ExecutorCapabilities) is all-`false`:
//! `tools/call` has no standard way to look up an execution started by a previous process,
//! no idempotency token, and no cancellation method. That is why the error mapping in
//! [`crate::mapping`] is conservative -- a call whose reply we cannot read is recorded as
//! `Unknown`, never as "did not happen".

use std::sync::Arc;

use agentscope_durable::executor::{Executor, ExecutorCapabilities};
use agentscope_durable::{ExecutionRequest, ExecutionResult, Result};
use async_trait::async_trait;

use crate::catalog::ToolCatalog;
use crate::error::TransportError;
use crate::jsonrpc::{RequestId, Response};
use crate::mapping::{error_to_execution_result, tool_not_advertised_result};
use crate::mapping::{tools_call_for_execution, tools_call_result_to_execution};
use crate::pool::McpConnectionPool;
use crate::transport::McpTransport;

/// Sends durable tool calls through an [`McpTransport`].
///
/// The catalog is mandatory and immutable: the executor refuses to call a tool the peer did
/// not advertise (recorded as a failed execution, never as an empty success). Refresh the
/// catalog by building a new executor after a fresh `tools/list`.
pub struct McpExecutor {
    transport: Arc<dyn McpTransport>,
    catalog: ToolCatalog,
}

impl McpExecutor {
    /// Binds a transport to the peer's declared tool catalog.
    pub fn new(transport: Arc<dyn McpTransport>, catalog: ToolCatalog) -> Self {
        Self { transport, catalog }
    }

    /// Builds an executor whose transport is the connection `pool` holds for `server_key`.
    ///
    /// The transport is taken now, so the executor keeps working even if the pool later sweeps
    /// the entry; build another executor to adopt a new connection.
    pub fn with_pool(
        pool: Arc<McpConnectionPool>,
        server_key: impl Into<String>,
        catalog: ToolCatalog,
    ) -> Result<Self> {
        let server_key = server_key.into();
        let transport = pool.get(&server_key).map_err(|error| error.to_durable())?;
        Ok(Self::new(transport, catalog))
    }

    /// The catalog this executor validates against.
    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    /// The transport this executor writes through.
    pub fn transport(&self) -> &Arc<dyn McpTransport> {
        &self.transport
    }
}

#[async_trait]
impl Executor for McpExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::default()
    }

    async fn start(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        if !self.catalog.contains(&request.call.name) {
            return Ok(tool_not_advertised_result(&request.call.name));
        }

        let call = tools_call_for_execution(RequestId::text(request.execution_id.clone()), request)
            .map_err(|error| error.to_durable())?;

        match self.transport.call(call.clone()).await {
            Ok(response) => {
                response.validate().map_err(|error| error.to_durable())?;
                response
                    .expect_id(&call.id)
                    .map_err(|error| error.to_durable())?;
                Ok(payload_to_result(response))
            }
            // The peer answered with an error object: a definite answer, so it becomes an
            // execution result rather than a transport failure.
            Err(TransportError::Rpc(error)) => Ok(error_to_execution_result(&error)),
            // Nothing usable came back (no reply / unreadable reply / wrong id): the effect
            // may have run, so this stays an error the durable runtime records as unknown.
            Err(error) => Err(error.to_durable()),
        }
    }

    /// MCP has no lookup verb, so a recovered process cannot ask the peer what happened.
    async fn query(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Ok(ExecutionResult::unknown(
            "MCP tools/call cannot look up an execution started by another process",
        ))
    }

    /// MCP has no standard cancellation verb either; the peer keeps running.
    async fn cancel(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Ok(ExecutionResult::unknown(
            "MCP tools/call cannot cancel an execution; the peer keeps running it",
        ))
    }
}

fn payload_to_result(response: Response) -> ExecutionResult {
    match (response.result, response.error) {
        (_, Some(error)) => error_to_execution_result(&error),
        (Some(payload), None) => tools_call_result_to_execution(&payload),
        // `validate()` rejects this shape; kept as a non-panicking last line of defence.
        (None, None) => ExecutionResult::unknown("MCP reply carried neither result nor error"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentscope_durable::executor::Executor;
    use agentscope_durable::{ExecutionRequest, ExecutionState, SessionKey, ThreadKey, ToolCall};
    use serde_json::json;

    use super::*;
    use crate::catalog::ToolCatalog;
    use crate::jsonrpc::{ErrorObject, RequestId, Response};
    use crate::testing::FakeTransport;

    fn execution(name: &str) -> ExecutionRequest {
        ExecutionRequest::new(
            "turn-1:call-1",
            ThreadKey::new(
                SessionKey::new("test", "session-1").expect("session"),
                "thread-1",
            )
            .expect("thread"),
            "turn-1",
            ToolCall::new("call-1", "mcp", name, json!({ "text": "hi" })).expect("call"),
        )
        .expect("execution")
    }

    fn catalog() -> ToolCatalog {
        ToolCatalog::from_tools_list(&json!({ "tools": [{ "name": "echo" }] })).expect("catalog")
    }

    #[tokio::test]
    async fn a_call_goes_out_as_tools_call_and_a_result_comes_back_succeeded() {
        let payload = json!({ "content": [{ "type": "text", "text": "hi" }] });
        let transport = Arc::new(FakeTransport::new(vec![Ok(Response::result(
            RequestId::text("turn-1:call-1"),
            payload.clone(),
        ))]));
        let executor = McpExecutor::new(transport.clone(), catalog());

        let result = executor.start(&execution("echo")).await.expect("start");
        assert_eq!(result.state, ExecutionState::Succeeded);
        assert_eq!(result.result, payload);

        let sent = transport.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, crate::mapping::METHOD_TOOLS_CALL);
        assert_eq!(sent[0].id, RequestId::text("turn-1:call-1"));
        let params = sent[0].params_value().expect("params");
        assert_eq!(params["name"], json!("echo"));
        assert_eq!(params["arguments"], json!({ "text": "hi" }));
    }

    #[tokio::test]
    async fn an_undeclared_tool_never_reaches_the_transport() {
        let transport = Arc::new(FakeTransport::empty());
        let executor = McpExecutor::new(transport.clone(), catalog());

        let result = executor.start(&execution("ghost")).await.expect("start");
        assert_eq!(result.state, ExecutionState::Failed);
        assert!(transport.sent().is_empty(), "nothing may be sent");
    }

    #[tokio::test]
    async fn a_peer_error_reply_becomes_a_failed_execution() {
        let transport = Arc::new(FakeTransport::new(vec![Ok(Response::with_error(
            Some(RequestId::text("turn-1:call-1")),
            ErrorObject::invalid_params("arguments.text is required"),
        ))]));
        let executor = McpExecutor::new(transport, catalog());

        let result = executor.start(&execution("echo")).await.expect("start");
        assert_eq!(result.state, ExecutionState::Failed);
        assert!(result
            .error
            .as_deref()
            .expect("message")
            .contains("INVALID_PARAMS"));
    }

    #[tokio::test]
    async fn a_dead_transport_stays_an_error_so_the_effect_is_unknown() {
        let transport = Arc::new(FakeTransport::empty());
        let executor = McpExecutor::new(transport, catalog());
        let error = executor
            .start(&execution("echo"))
            .await
            .expect_err("must fail");
        assert_eq!(error.code(), agentscope_durable::ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn a_reply_for_another_id_is_a_protocol_failure() {
        let transport = Arc::new(FakeTransport::new(vec![Ok(Response::result(
            RequestId::number(99),
            json!({ "content": [] }),
        ))]));
        let executor = McpExecutor::new(transport, catalog());
        let error = executor
            .start(&execution("echo"))
            .await
            .expect_err("must fail");
        assert_eq!(error.code(), agentscope_durable::ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn lookup_and_cancel_report_the_mcp_limits() {
        let executor = McpExecutor::new(Arc::new(FakeTransport::empty()), catalog());
        let request = execution("echo");
        let capabilities = executor.capabilities();
        assert!(!capabilities.lookup_by_execution_id);
        assert!(!capabilities.idempotent_start);
        assert!(!capabilities.cancellation);

        assert_eq!(
            executor.query(&request).await.expect("query").state,
            ExecutionState::Unknown
        );
        assert_eq!(
            executor.cancel(&request).await.expect("cancel").state,
            ExecutionState::Unknown
        );
    }
}
