//! External effect execution: the only place a side effect may leave the runtime.

use async_trait::async_trait;

use crate::types::{ExecutionRequest, ExecutionResult};
use crate::Result;

/// What an executor can promise during recovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecutorCapabilities {
    /// `query` can find an execution that was started by a previous process.
    pub lookup_by_execution_id: bool,
    /// Starting the same `execution_id` twice never produces a second effect.
    pub idempotent_start: bool,
    /// A running execution can be cancelled from another process.
    pub cancellation: bool,
}

/// Executes tool calls. Implementations must be safe to call after a crash.
#[async_trait]
pub trait Executor: Send + Sync {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::default()
    }

    /// Starts the effect. A failure here may still have produced the effect.
    async fn start(&self, request: &ExecutionRequest) -> Result<ExecutionResult>;

    /// Looks up an execution started earlier, without starting anything new.
    async fn query(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Ok(ExecutionResult::unknown(
            "query is not supported by this executor",
        ))
    }

    async fn cancel(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Ok(ExecutionResult::unknown(
            "cancel is not supported by this executor",
        ))
    }
}
