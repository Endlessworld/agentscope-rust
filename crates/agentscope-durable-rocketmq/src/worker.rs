//! The Hands-side loop: take requests, run them locally, publish replies.

use std::sync::{Arc, Mutex, MutexGuard};

use agentscope_durable::execution_store::{DispatchRecord, DispatchState, ExecutionStore};
use agentscope_durable::executor::Executor;
use agentscope_durable::now_millis;

use crate::broker::Broker;
use crate::cursor::Cursor;
use crate::error::BrokerError;
use crate::hands::{HandsEnvelope, HandsKind};
use crate::message::Message;

/// Polls the request partition and runs what it finds.
///
/// Dedupe is the ledger: a request whose execution is already `Dispatched`, `Settled` or
/// `Unknown` is skipped, so at-least-once delivery never produces a second effect. The cursor
/// advances by the number of messages fetched (the broker contract: every message consumes one
/// offset), not by the number handled.
pub struct RocketMqWorker<B: Broker, E: Executor> {
    requests: Arc<B>,
    replies: Arc<B>,
    local: Arc<E>,
    store: Arc<dyn ExecutionStore>,
    cursor: Mutex<Cursor>,
}

impl<B: Broker, E: Executor> RocketMqWorker<B, E> {
    pub fn new(
        requests: Arc<B>,
        replies: Arc<B>,
        local: Arc<E>,
        store: Arc<dyn ExecutionStore>,
    ) -> Self {
        Self {
            requests,
            replies,
            local,
            store,
            cursor: Mutex::new(Cursor::START),
        }
    }

    pub fn cursor(&self) -> Cursor {
        *self.cursor_guard()
    }

    /// Fetches up to `max` request messages and processes each one.
    pub async fn poll_once(&self, max: usize) -> Result<usize, BrokerError> {
        let cursor = self.cursor();
        let messages = self.requests.fetch(&cursor.to_string(), max).await?;
        let mut handled = 0;
        for message in &messages {
            let envelope = HandsEnvelope::decode(&message.body)
                .map_err(|error| BrokerError::invalid(format!("hands envelope: {error}")))?;
            match envelope.kind {
                HandsKind::Request => {
                    if self.run_request(&envelope).await? {
                        handled += 1;
                    }
                }
                HandsKind::Cancel => {
                    if let Some(request) = &envelope.request {
                        let _ = self.local.cancel(request).await;
                    }
                    handled += 1;
                }
                HandsKind::Reply => {}
            }
        }
        self.set_cursor(cursor.advance(messages.len()));
        Ok(handled)
    }

    /// Runs one request unless the ledger already knows it.
    async fn run_request(&self, envelope: &HandsEnvelope) -> Result<bool, BrokerError> {
        let Some(request) = &envelope.request else {
            return Ok(false);
        };
        if let Some(record) = self.ledger_get(&envelope.execution_id)? {
            if record.state != DispatchState::Pending {
                return Ok(false);
            }
        }
        let record = DispatchRecord {
            execution_id: envelope.execution_id.clone(),
            request: request.clone(),
            state: DispatchState::Dispatched,
            attempts: envelope.attempt.max(1),
            dispatched_at: now_millis(),
            result: None,
        };
        self.ledger_put(&record)?;
        match self.local.start(request).await {
            Ok(result) => {
                if result.state == agentscope_durable::types::ExecutionState::Pending {
                    // Still running: the reply is published when a later sweep or query finds it.
                    return Ok(true);
                }
                self.ledger_settle(&envelope.execution_id, &result)?;
                self.publish_reply(envelope, &result).await?;
            }
            Err(_) => {
                // It may already have run: record the doubt, never a retryable `Pending`.
                let mut unknown = record;
                unknown.state = DispatchState::Unknown;
                self.ledger_put(&unknown)?;
            }
        }
        Ok(true)
    }

    async fn publish_reply(
        &self,
        envelope: &HandsEnvelope,
        result: &agentscope_durable::types::ExecutionResult,
    ) -> Result<(), BrokerError> {
        let reply = HandsEnvelope::reply(&envelope.execution_id, result, envelope.attempt);
        let body = reply
            .encode()
            .map_err(|error| BrokerError::invalid(format!("reply envelope: {error}")))?;
        let message = Message::new(
            self.replies.topic(),
            self.replies.tag(),
            envelope.execution_id.clone(),
            body,
        );
        self.replies.publish(message).await
    }

    fn ledger_get(&self, execution_id: &str) -> Result<Option<DispatchRecord>, BrokerError> {
        self.store
            .get(execution_id)
            .map_err(|error| BrokerError::invalid(format!("ledger read: {error}")))
    }

    fn ledger_put(&self, record: &DispatchRecord) -> Result<(), BrokerError> {
        self.store
            .put(record)
            .map_err(|error| BrokerError::invalid(format!("ledger write: {error}")))
    }

    fn ledger_settle(
        &self,
        execution_id: &str,
        result: &agentscope_durable::types::ExecutionResult,
    ) -> Result<(), BrokerError> {
        self.store.settle(execution_id, result).map_err(|error| {
            BrokerError::invalid(format!("ledger settle `{execution_id}`: {error}"))
        })
    }

    fn cursor_guard(&self) -> MutexGuard<'_, Cursor> {
        self.cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_cursor(&self, cursor: Cursor) {
        *self.cursor_guard() = cursor;
    }
}
