//! Who carries an intent out.

use std::sync::Mutex;

use agentscope_durable::executor::{Executor, ExecutorCapabilities};
use agentscope_durable::types::{ExecutionRequest, ExecutionResult, ExecutionState};
use agentscope_durable::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// The effect side of one step.
///
/// This trait *is* the durable [`Executor`] port under an intent-level name: any `Executor`
/// implements it through the blanket impl below, so the workspace keeps one definition of
/// "apply an external effect" and one set of three-state rules (`SUCCEEDED`/`FAILED` mean the
/// effect is settled, `UNKNOWN` means nobody can say, and neither sleeps nor retries here).
#[async_trait]
pub trait Hands: Send + Sync {
    fn capabilities(&self) -> ExecutorCapabilities;

    /// Applies one intent exactly once.
    ///
    /// Must not wait for the effect to finish: a long effect answers `PENDING`/`RUNNING`, and a
    /// later step looks it up. An error is not a "nothing happened" answer either — see
    /// [`Worker::step`](crate::Worker::step), which records an errored apply as `UNKNOWN`.
    async fn apply(&self, request: &ExecutionRequest) -> Result<ExecutionResult>;
}

#[async_trait]
impl<T> Hands for T
where
    T: Executor + ?Sized,
{
    fn capabilities(&self) -> ExecutorCapabilities {
        Executor::capabilities(self)
    }

    async fn apply(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        Executor::start(self, request).await
    }
}

/// A hands implementation that records every call and answers with a configured state.
///
/// It implements [`Executor`] (not `Hands` directly), which is also the proof that the port is
/// not duplicated: the stub a test drives and the real in-process/remote executor are the same
/// kind of value.
pub struct StubHands {
    calls: Mutex<Vec<String>>,
    state: ExecutionState,
}

impl StubHands {
    /// Answers `SUCCEEDED` with an echo of the call it applied.
    pub fn new() -> Self {
        Self::answering(ExecutionState::Succeeded)
    }

    /// Answers every apply with `state` and no payload.
    pub fn answering(state: ExecutionState) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            state,
        }
    }

    /// The execution ids it was asked to apply, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    pub fn count(&self) -> usize {
        self.calls.lock().expect("calls").len()
    }
}

impl Default for StubHands {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for StubHands {
    async fn start(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        self.calls
            .lock()
            .expect("calls")
            .push(request.execution_id.clone());
        if self.state == ExecutionState::Succeeded {
            return Ok(ExecutionResult::succeeded(
                json!({ "callId": request.call.call_id }),
            ));
        }
        Ok(ExecutionResult {
            state: self.state,
            handle: None,
            result: Value::Null,
            error: None,
        })
    }
}
