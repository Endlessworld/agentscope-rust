//! The reply partition's inbox: drain replies into the execution ledger.

use std::sync::{Arc, Mutex, MutexGuard};

use agentscope_durable::execution_store::ExecutionStore;

use crate::broker::Broker;
use crate::cursor::Cursor;
use crate::error::BrokerError;
use crate::hands::{HandsEnvelope, HandsKind};

/// Reads Replies from the hands reply partition and records them in the ledger.
///
/// The ledger is the authority: a reply for an execution it does not know is ignored, so a late
/// message cannot resurrect an execution a host has already pruned.
pub struct ReplyInbox<B: Broker> {
    broker: Arc<B>,
    store: Arc<dyn ExecutionStore>,
    cursor: Mutex<Cursor>,
}

impl<B: Broker> ReplyInbox<B> {
    pub fn new(broker: Arc<B>, store: Arc<dyn ExecutionStore>) -> Self {
        Self {
            broker,
            store,
            cursor: Mutex::new(Cursor::START),
        }
    }

    pub fn cursor(&self) -> Cursor {
        *self.cursor_guard()
    }

    /// Drains up to `max` messages, settling the ledger for every reply it finds.
    pub async fn drain(&self, max: usize) -> Result<usize, BrokerError> {
        let cursor = self.cursor();
        let messages = self.broker.fetch(&cursor.to_string(), max).await?;
        for message in &messages {
            let envelope = HandsEnvelope::decode(&message.body)
                .map_err(|error| BrokerError::invalid(format!("reply envelope: {error}")))?;
            if envelope.kind != HandsKind::Reply {
                continue;
            }
            let Some(result) = &envelope.result else {
                continue;
            };
            let known = self
                .store
                .get(&envelope.execution_id)
                .map_err(|error| BrokerError::invalid(format!("ledger read: {error}")))?;
            if known.is_none() {
                continue;
            }
            self.store
                .settle(&envelope.execution_id, result)
                .map_err(|error| {
                    BrokerError::invalid(format!("reply for `{}`: {error}", envelope.execution_id))
                })?;
        }
        self.set_cursor(cursor.advance(messages.len()));
        Ok(messages.len())
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
