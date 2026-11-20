//! The executor wrapper that keeps the effect ledger.
//!
//! An effect is written down before it leaves the process and its outcome after. That is what
//! lets a restart answer "did this run?" instead of guessing: a `Settled` record answers from
//! the stored result, a `Dispatched` record is never resent, and a failed publication becomes
//! `Unknown` — never `Pending` — because the peer may have received the first attempt.

use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::execution_store::{DispatchRecord, DispatchState, ExecutionStore};
use agentscope_durable::executor::{Executor, ExecutorCapabilities};
use agentscope_durable::types::{ExecutionRequest, ExecutionResult, ExecutionState};
use agentscope_durable::{now_millis, Result};
use async_trait::async_trait;

/// Wraps an executor with a durable inbox/outbox.
pub struct QueuedExecutor {
    inner: Arc<dyn Executor>,
    store: Arc<dyn ExecutionStore>,
}

impl QueuedExecutor {
    pub fn new(inner: Arc<dyn Executor>, store: Arc<dyn ExecutionStore>) -> Self {
        Self { inner, store }
    }

    pub fn store(&self) -> &Arc<dyn ExecutionStore> {
        &self.store
    }

    /// Reconciles dispatches older than `max_age`.
    ///
    /// A lookup that answers is recorded; one that cannot tell becomes `Unknown`. Nothing is
    /// ever resent here: a dispatch is only sent by [`Executor::start`] on a turn that is
    /// actually running, and a turn that saw a `Pending` never reaches this path.
    pub async fn sweep(&self, max_age: Duration, limit: usize) -> Result<usize> {
        let now = now_millis();
        let age = max_age.as_millis().min(i64::MAX as u128) as i64;
        let mut swept = 0;
        for record in self.store.pending(limit)? {
            if record.state != DispatchState::Dispatched {
                continue;
            }
            if now.saturating_sub(record.dispatched_at) < age {
                continue;
            }
            match self.inner.query(&record.request).await {
                Ok(result) if result.state.terminal() => {
                    self.store.settle(&record.execution_id, &result)?;
                }
                _ => {
                    let mut unknown = record;
                    unknown.state = DispatchState::Unknown;
                    self.store.put(&unknown)?;
                }
            }
            swept += 1;
        }
        Ok(swept)
    }
}

/// What a caller sees for an execution that was sent and has not answered yet.
fn pending(request: &ExecutionRequest) -> ExecutionResult {
    ExecutionResult {
        state: ExecutionState::Pending,
        handle: Some(request.execution_id.clone()),
        result: serde_json::Value::Null,
        error: None,
    }
}

#[async_trait]
impl Executor for QueuedExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        self.inner.capabilities()
    }

    async fn start(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        let existing = self.store.get(&request.execution_id)?;
        if let Some(record) = &existing {
            match record.state {
                DispatchState::Settled => {
                    return Ok(record.result.clone().unwrap_or_else(|| {
                        ExecutionResult::unknown("the dispatch is settled without a result")
                    }));
                }
                DispatchState::Dispatched => return Ok(pending(request)),
                DispatchState::Unknown => {
                    return Ok(ExecutionResult::unknown(
                        "the previous dispatch has an unknown outcome",
                    ));
                }
                // Recorded but never sent: this call is the first attempt.
                DispatchState::Pending => {}
            }
        }
        let record = DispatchRecord {
            execution_id: request.execution_id.clone(),
            request: request.clone(),
            state: DispatchState::Dispatched,
            attempts: existing.map(|held| held.attempts).unwrap_or(0) + 1,
            dispatched_at: now_millis(),
            result: None,
        };
        // Written before the call: a crash after this point must never let a restart resend.
        self.store.put(&record)?;
        match self.inner.start(request).await {
            Ok(result) => match result.state {
                ExecutionState::Pending => Ok(result),
                ExecutionState::Unknown => {
                    let mut unknown = record;
                    unknown.state = DispatchState::Unknown;
                    self.store.put(&unknown)?;
                    Ok(result)
                }
                _ => {
                    self.store.settle(&request.execution_id, &result)?;
                    Ok(result)
                }
            },
            Err(error) => {
                // The call failed, but it may already have reached the peer: never `Pending`.
                let mut unknown = record;
                unknown.state = DispatchState::Unknown;
                self.store.put(&unknown)?;
                Err(error)
            }
        }
    }

    async fn query(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        if let Some(record) = self.store.get(&request.execution_id)? {
            if let Some(result) = record.result {
                return Ok(result);
            }
            if record.state == DispatchState::Unknown {
                return Ok(ExecutionResult::unknown("the dispatch outcome is unknown"));
            }
        }
        self.inner.query(request).await
    }

    async fn cancel(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        // A cancellation cannot be proven, so the ledger records that doubt before asking the
        // executor to stop: a later `query` must not report success for a cancelled effect.
        if let Some(mut record) = self.store.get(&request.execution_id)? {
            if matches!(
                record.state,
                DispatchState::Pending | DispatchState::Dispatched
            ) {
                record.state = DispatchState::Unknown;
                record.result = None;
                self.store.put(&record)?;
            }
        }
        self.inner.cancel(request).await
    }
}
