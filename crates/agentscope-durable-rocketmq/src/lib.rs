//! RocketMQ event-channel binding for the durable runtime.
//!
//! Aligned with Java `agentscope-extensions-durable-rocketmq`: this crate owns the *transport
//! mapping* (topic/tag/key derivation, envelope encoding, consumer-side dedupe and cursor
//! bookkeeping) and nothing else. It does not talk to a RocketMQ broker: the SDK client is a
//! host concern, so the [`Broker`] port is the only thing production code is written against.
//! [`InMemoryBroker`] is a real, reusable minimal implementation (not a test double) for hosts
//! that run the event channel in-process.
//!
//! Delivery contract:
//!
//! * **At least once.** The broker may hand the same message over more than once; a consumer
//!   drops every `eventId` it has already returned, so callers never observe duplicates.
//! * **Per-partition order.** A consumer is bound to one session (`topic` per namespace, `tag`
//!   per session) and rejects a sequence that does not strictly increase.
//! * **Cursor = queue offset.** [`Cursor`] is the transport position inside that partition;
//!   the durable cursor of each delivered event travels with the event so a host can persist
//!   both and resume without gaps or repeats.
//!
//! Deliberately not implemented (see the TODO notes in the modules): retries/backoff, RocketMQ
//! transactions and idempotency keys, topic provisioning/ACLs, oversized-payload offload to an
//! `ObjectStore`, and durable de-duplication across process restarts.

mod broker;
mod consumer;
mod cursor;
mod envelope;
mod error;
mod message;
mod route;

pub use broker::{Broker, InMemoryBroker};
pub use consumer::{Batch, Consumer};
pub use cursor::Cursor;
pub use envelope::Envelope;
pub use error::{BrokerError, ConsumeError, CursorError, EnvelopeError};
pub use message::Message;
pub use route::{route, topic_for_namespace, Route, TAG_PREFIX, TOPIC_PREFIX};

/// Test-only single-poll executor.
///
/// [`Broker`] implementations in this crate never yield, and the crate intentionally has no
/// async runtime dependency. A multi-poll executor would only hide a real bug behind a hang.
#[cfg(test)]
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};

    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("this crate's futures must complete in a single poll"),
    }
}

#[cfg(test)]
mod tests;
