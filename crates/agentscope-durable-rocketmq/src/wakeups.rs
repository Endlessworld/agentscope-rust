//! Wakeups over the wakeup partition.
//!
//! [`Wakeups::publish`] is synchronous by contract (it runs on the thread that committed), while
//! [`Broker::publish`] is asynchronous. The bridge is an outbox: `publish` appends and returns,
//! and the host's loop calls [`RocketMqWakeups::poll_once`], which flushes the outbox and then
//! hands every fetched wakeup to the subscribers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use agentscope_durable::wakeups::{SubscriptionId, Wakeup, WakeupSink, Wakeups};
use agentscope_durable::{DurableError, Result as DurableResult};

use crate::broker::Broker;
use crate::cursor::Cursor;
use crate::error::BrokerError;
use crate::message::Message;

/// The in-process wakeup channel over one broker partition.
pub struct RocketMqWakeups<B: Broker> {
    broker: Arc<B>,
    outbox: Mutex<Vec<Message>>,
    sinks: Mutex<Vec<(SubscriptionId, WakeupSink)>>,
    next: AtomicU64,
    cursor: Mutex<Cursor>,
}

impl<B: Broker> RocketMqWakeups<B> {
    pub fn new(broker: Arc<B>) -> Self {
        Self {
            broker,
            outbox: Mutex::new(Vec::new()),
            sinks: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
            cursor: Mutex::new(Cursor::START),
        }
    }

    pub fn cursor(&self) -> Cursor {
        *self.cursor_guard()
    }

    /// Publishes everything queued since the last call.
    pub async fn flush(&self) -> Result<usize, BrokerError> {
        let pending = {
            let mut outbox = self.outbox();
            std::mem::take(&mut *outbox)
        };
        let mut sent = 0;
        while sent < pending.len() {
            match self.broker.publish(pending[sent].clone()).await {
                Ok(()) => sent += 1,
                Err(error) => {
                    // Keep the failed message and everything after it: order matters to a
                    // consumer that resumes from its own cursor.
                    let remaining = pending[sent..].to_vec();
                    let mut outbox = self.outbox();
                    outbox.splice(0..0, remaining);
                    return Err(error);
                }
            }
        }
        Ok(sent)
    }

    /// Flushes the outbox and delivers every fetched wakeup to the subscribers.
    pub async fn poll_once(&self, max: usize) -> Result<usize, BrokerError> {
        self.flush().await?;
        let cursor = self.cursor();
        let messages = self.broker.fetch(&cursor.to_string(), max).await?;
        for message in &messages {
            let wakeup: Wakeup = serde_json::from_slice(&message.body)
                .map_err(|error| BrokerError::invalid(format!("wakeup body: {error}")))?;
            for sink in self.sinks_snapshot() {
                sink(&wakeup);
            }
        }
        *self.cursor_guard() = cursor.advance(messages.len());
        Ok(messages.len())
    }

    fn sinks_snapshot(&self) -> Vec<WakeupSink> {
        match self.sinks.lock() {
            Ok(sinks) => sinks.iter().map(|(_, sink)| Arc::clone(sink)).collect(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .map(|(_, sink)| Arc::clone(sink))
                .collect(),
        }
    }

    fn outbox(&self) -> MutexGuard<'_, Vec<Message>> {
        self.outbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cursor_guard(&self) -> MutexGuard<'_, Cursor> {
        self.cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<B: Broker> Wakeups for RocketMqWakeups<B> {
    fn publish(&self, wakeup: &Wakeup) -> DurableResult<()> {
        let body = serde_json::to_vec(wakeup)
            .map_err(|error| DurableError::invalid(format!("wakeup body: {error}")))?;
        let message = Message::new(
            self.broker.topic(),
            self.broker.tag(),
            wakeup.thread.storage_key(),
            body,
        );
        self.outbox().push(message);
        Ok(())
    }

    fn subscribe(&self, sink: WakeupSink) -> SubscriptionId {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut sinks) = self.sinks.lock() {
            sinks.push((id, sink));
        }
        id
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        if let Ok(mut sinks) = self.sinks.lock() {
            sinks.retain(|(held, _)| *held != id);
        }
    }
}
