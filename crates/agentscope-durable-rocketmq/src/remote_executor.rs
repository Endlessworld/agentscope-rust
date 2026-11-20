//! The Brain-side executor: publish a request, answer from the ledger.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use agentscope_durable::execution_store::{DispatchState, ExecutionStore};
use agentscope_durable::executor::{Executor, ExecutorCapabilities};
use agentscope_durable::types::{ExecutionRequest, ExecutionResult, ExecutionState};
use agentscope_durable::{DurableError, Result as DurableResult};
use async_trait::async_trait;

use crate::broker::Broker;
use crate::error::BrokerError;
use crate::hands::HandsEnvelope;
use crate::message::Message;
use crate::reply_inbox::ReplyInbox;

/// How many reply messages one query drains.
const REPLY_PAGE: usize = 64;

/// Sends executions to a Hands host and answers from the execution ledger.
///
/// `start` only publishes: the effect happens in another process, so the honest answer is
/// `Pending`. `query` drains the reply partition first, so a reply that arrived since the last
/// call is visible without a separate sweep.
pub struct RemoteExecutor<B: Broker> {
    broker: Arc<B>,
    inbox: Arc<ReplyInbox<B>>,
    store: Arc<dyn ExecutionStore>,
    attempt: AtomicU32,
}

impl<B: Broker> RemoteExecutor<B> {
    pub fn new(broker: Arc<B>, inbox: Arc<ReplyInbox<B>>, store: Arc<dyn ExecutionStore>) -> Self {
        Self {
            broker,
            inbox,
            store,
            attempt: AtomicU32::new(0),
        }
    }

    /// Publishes one request. The message key is the execution id, so a broker trace maps back
    /// to the ledger row.
    pub async fn publish(&self, request: &ExecutionRequest) -> Result<(), BrokerError> {
        let attempt = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
        let envelope = HandsEnvelope::request(request, attempt);
        let body = envelope
            .encode()
            .map_err(|error| BrokerError::invalid(format!("request envelope: {error}")))?;
        let message = Message::new(
            self.broker.topic(),
            self.broker.tag(),
            request.execution_id.clone(),
            body,
        );
        self.broker.publish(message).await
    }
}

fn pending(request: &ExecutionRequest) -> ExecutionResult {
    ExecutionResult {
        state: ExecutionState::Pending,
        handle: Some(request.execution_id.clone()),
        result: serde_json::Value::Null,
        error: None,
    }
}

#[async_trait]
impl<B: Broker + 'static> Executor for RemoteExecutor<B> {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            lookup_by_execution_id: true,
            idempotent_start: true,
            cancellation: true,
        }
    }

    async fn start(&self, request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        self.publish(request)
            .await
            .map_err(|error| DurableError::unavailable(error.to_string()))?;
        Ok(pending(request))
    }

    async fn query(&self, request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        self.inbox
            .drain(REPLY_PAGE)
            .await
            .map_err(|error| DurableError::unavailable(error.to_string()))?;
        match self.store.get(&request.execution_id)? {
            Some(record) => match record.result {
                Some(result) => Ok(result),
                None if record.state == DispatchState::Unknown => {
                    Ok(ExecutionResult::unknown("the dispatch outcome is unknown"))
                }
                None => Ok(ExecutionResult::unknown("no reply has arrived yet")),
            },
            None => Ok(ExecutionResult::unknown("the execution is not recorded")),
        }
    }

    async fn cancel(&self, request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        let envelope = HandsEnvelope::cancel(&request.execution_id, Some(request));
        let body = envelope
            .encode()
            .map_err(|error| DurableError::invalid(format!("cancel envelope: {error}")))?;
        let message = Message::new(
            self.broker.topic(),
            self.broker.tag(),
            request.execution_id.clone(),
            body,
        );
        self.broker
            .publish(message)
            .await
            .map_err(|error| DurableError::unavailable(error.to_string()))?;
        Ok(ExecutionResult::unknown(
            "cancellation was requested; the outcome is unknown",
        ))
    }
}
