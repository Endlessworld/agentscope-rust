//! What the durable runtime may ask about a tool call this host handed to the background.
//!
//! A tool the agent offloads keeps running after the turn that started it: the turn only holds a
//! placeholder result, so its record says `PENDING` and no terminal status can be written while
//! it stays that way. The registry the harness fills in when it offloads a call is the only
//! authority on what became of that effect, so this adapter answers the durable executor's
//! `query` from it. That is what lets a turn end with the effect it really got (finished,
//! failed) instead of parking until somebody reconciles it.
//!
//! `start`/`cancel` stay unsupported on purpose: the agent loop dispatches the tool, not the
//! runtime, and a native call cannot be interrupted from outside the process that runs it.

use std::sync::Arc;

use agentscope_durable::executor::{Executor, ExecutorCapabilities};
use agentscope_durable::{DurableError, ExecutionRequest, ExecutionResult, ExecutionState, Result};
use agentscope_harness::agent::{AsyncToolRecord, AsyncToolRegistry};
use async_trait::async_trait;
use serde_json::json;

/// Answers durable questions about offloaded tool calls from the harness registry.
pub struct OffloadedToolEffects {
    registry: Arc<dyn AsyncToolRegistry>,
    session_id: String,
}

impl OffloadedToolEffects {
    pub fn new(registry: Arc<dyn AsyncToolRegistry>, session_id: impl Into<String>) -> Self {
        Self {
            registry,
            session_id: session_id.into(),
        }
    }
}

#[async_trait]
impl Executor for OffloadedToolEffects {
    fn capabilities(&self) -> ExecutorCapabilities {
        // A finished offload can be looked up by execution id; starting and cancelling it are
        // the agent loop's, not the runtime's.
        ExecutorCapabilities {
            lookup_by_execution_id: true,
            idempotent_start: false,
            cancellation: false,
        }
    }

    async fn start(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Ok(ExecutionResult::unknown(
            "Tool calls are dispatched by the agent loop, not by the durable executor",
        ))
    }

    async fn query(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        let records = self
            .registry
            .list_session(&self.session_id)
            .await
            .map_err(|error| {
                DurableError::unavailable(format!(
                    "The async tool registry cannot be read: {error}"
                ))
            })?;
        let Some(record) = records
            .iter()
            .find(|record| record.tool_call_id == request.call.call_id)
        else {
            // The call was never registered as offloaded, so nothing can confirm it either: it
            // must stay unknown rather than be read as "finished".
            return Ok(ExecutionResult::unknown(
                "No background record exists for this call, so its outcome cannot be confirmed",
            ));
        };
        Ok(match record.status.as_str() {
            // Only the state is in the registry; the output itself is delivered to the model as
            // a notification, so the result records where it went instead of inventing content.
            AsyncToolRecord::COMPLETED => ExecutionResult {
                state: ExecutionState::Succeeded,
                handle: Some(record.id.clone()),
                result: json!({
                    "background": true,
                    "note": "finished in the background; its output was delivered as a notification"
                }),
                error: None,
            },
            AsyncToolRecord::FAILED | AsyncToolRecord::TIMEOUT => ExecutionResult {
                state: ExecutionState::Failed,
                handle: Some(record.id.clone()),
                result: serde_json::Value::Null,
                error: Some(format!(
                    "The background tool ended as {}: its output was delivered as a notification",
                    record.status
                )),
            },
            // Still RUNNING (or a status this build does not know): the answer is not in yet and
            // nothing here may claim one, so the effect stays unknown.
            _ => ExecutionResult::unknown(format!(
                "The background tool has not reported yet ({})",
                record.status
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_durable::{ExecutionState, ToolCall as DurableToolCall};

    /// A registry with one record per call, so the mapping can be checked without a filesystem.
    struct FixedRegistry {
        records: Vec<AsyncToolRecord>,
    }

    #[async_trait]
    impl AsyncToolRegistry for FixedRegistry {
        async fn register(&self, _record: AsyncToolRecord) -> agentscope_core::error::Result<()> {
            Ok(())
        }

        async fn complete(&self, _id: &str, _result: &str) -> agentscope_core::error::Result<()> {
            Ok(())
        }

        async fn fail(&self, _id: &str, _error: &str) -> agentscope_core::error::Result<()> {
            Ok(())
        }

        async fn find_stale(
            &self,
            _session_id: &str,
            _ttl: chrono::Duration,
        ) -> agentscope_core::error::Result<Vec<AsyncToolRecord>> {
            Ok(Vec::new())
        }

        async fn mark_timeout(&self, _id: &str) -> agentscope_core::error::Result<()> {
            Ok(())
        }

        async fn list_session(
            &self,
            _session_id: &str,
        ) -> agentscope_core::error::Result<Vec<AsyncToolRecord>> {
            Ok(self.records.clone())
        }
    }

    fn record(call_id: &str, status: &str) -> AsyncToolRecord {
        let mut record = AsyncToolRecord::new(
            call_id,
            "session-1",
            "shell_execute",
            call_id,
            chrono::Utc::now(),
        );
        record.status = status.to_string();
        record
    }

    fn request(call_id: &str) -> ExecutionRequest {
        let thread = agentscope_durable::ThreadKey::new(
            agentscope_durable::SessionKey::new("acp", "session-1").expect("session key"),
            "main",
        )
        .expect("thread key");
        let call =
            DurableToolCall::new(call_id, "local", "shell_execute", json!({})).expect("tool call");
        ExecutionRequest::new(format!("turn-1:{call_id}"), thread, "turn-1", call)
            .expect("execution request")
    }

    fn query_state(records: Vec<AsyncToolRecord>) -> ExecutionState {
        let executor = OffloadedToolEffects::new(Arc::new(FixedRegistry { records }), "session-1");
        futures::executor::block_on(executor.query(&request("call-1")))
            .expect("query")
            .state
    }

    #[test]
    fn a_finished_background_call_reports_success() {
        assert_eq!(
            query_state(vec![record("call-1", AsyncToolRecord::COMPLETED)]),
            ExecutionState::Succeeded
        );
    }

    #[test]
    fn a_failed_background_call_reports_failure() {
        assert_eq!(
            query_state(vec![record("call-1", AsyncToolRecord::FAILED)]),
            ExecutionState::Failed
        );
        assert_eq!(
            query_state(vec![record("call-1", AsyncToolRecord::TIMEOUT)]),
            ExecutionState::Failed
        );
    }

    #[test]
    fn a_running_or_unknown_record_stays_unknown() {
        assert_eq!(
            query_state(vec![record("call-1", AsyncToolRecord::RUNNING)]),
            ExecutionState::Unknown
        );
        assert_eq!(query_state(Vec::new()), ExecutionState::Unknown);
    }

    #[test]
    fn another_calls_record_does_not_answer_this_one() {
        assert_eq!(
            query_state(vec![record("call-2", AsyncToolRecord::COMPLETED)]),
            ExecutionState::Unknown
        );
    }
}
