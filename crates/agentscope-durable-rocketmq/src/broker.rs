//! The broker port and a reusable in-process implementation.
//!
//! No RocketMQ client is linked here on purpose. The Java module takes the SDK `Producer` and
//! `SimpleConsumer` from the host ("Host owns topic/ACL/provisioning and the supplied SDK
//! clients"), and the Rust port keeps the same split: implement [`Broker`] once per client and the
//! rest of this crate (routing, envelope, dedupe, cursors) applies unchanged.

use std::sync::{Arc, Mutex, MutexGuard};

use agentscope_durable::SessionKey;
use async_trait::async_trait;

use crate::cursor::Cursor;
use crate::error::BrokerError;
use crate::message::Message;
use crate::route::{route, Route};

/// A publish/fetch port over one queue partition.
///
/// # Contract
///
/// * `fetch` is **at least once**: returning a message twice (including a repeated prefix) is
///   legal. The consumer de-duplicates by `eventId`, so a broker never has to be exactly-once.
/// * `fetch` is **non-blocking**: an empty partition is `Ok(vec![])`, never a wait, never a panic.
/// * The cursor passed to `fetch` is a [`Cursor`] rendering; a broker that cannot seek to it must
///   fail with [`BrokerError::Invalid`] rather than silently start from zero, which would replay
///   the whole partition. Every published message consumes exactly one offset in this partition,
///   including redeliveries that the consumer later drops as duplicates, so the consumer may
///   advance the cursor by the number of messages it fetched.
///
/// Native `async fn` keeps the port free of `async-trait`; it is therefore not `dyn`-compatible.
/// A host that needs dynamic dispatch wraps a concrete broker in [`DynBroker`](crate::DynBroker).
///
/// TODO: retries/backoff and transactional publish are intentionally absent. RocketMQ 5
/// transaction messages need a half-message check against a live broker's transaction service,
/// which cannot be verified here, and a retry inside the port would hide the retryable
/// [`BrokerError::Unavailable`] from the caller that owns the backoff policy. The write-side
/// exactly-once story stays with the durable journal (idempotent `eventId`), not with the MQ.
#[async_trait]
pub trait Broker: Send + Sync {
    /// Topic this partition serves; a message for another topic is rejected by the broker.
    fn topic(&self) -> &str;

    /// Tag this partition serves; it is the subscription filter of the partition.
    fn tag(&self) -> &str;

    /// Publish one message.
    async fn publish(&self, message: Message) -> Result<(), BrokerError>;

    /// Read up to `max` messages starting at `cursor`.
    async fn fetch(&self, cursor: &str, max: usize) -> Result<Vec<Message>, BrokerError>;
}

/// An in-memory, single-process partition.
///
/// This is production code, not a test double: it is the minimal correct implementation for a
/// host that runs the durable event channel in-process (embedded agents, examples, tests of
/// *other* crates). Cloning a broker shares the same log, which is what multiple consumers of one
/// session need.
///
/// It stores nothing on disk: a restart starts an empty partition. Durable retention is the
/// broker's job, so a host that needs it implements [`Broker`] over RocketMQ instead of growing
/// this type into a log store.
#[derive(Debug, Clone)]
pub struct InMemoryBroker {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    topic: String,
    tag: String,
    messages: Mutex<Vec<Message>>,
}

impl InMemoryBroker {
    /// Create a partition for `topic`/`tag`; messages for anything else are rejected.
    pub fn new(topic: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                topic: topic.into(),
                tag: tag.into(),
                messages: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Partition of one durable session.
    pub fn for_session(session: &SessionKey) -> Self {
        Self::for_route(&route(session))
    }

    /// Partition described by a computed route.
    pub fn for_route(route: &Route) -> Self {
        Self::new(route.topic.clone(), route.tag.clone())
    }

    /// Topic this broker accepts and serves.
    pub fn topic(&self) -> &str {
        &self.inner.topic
    }

    /// Tag this broker accepts and serves.
    pub fn tag(&self) -> &str {
        &self.inner.tag
    }

    /// Number of messages ever published to the partition.
    pub fn len(&self) -> usize {
        self.messages().len()
    }

    /// Whether nothing has been published yet.
    pub fn is_empty(&self) -> bool {
        self.messages().is_empty()
    }

    fn messages(&self) -> MutexGuard<'_, Vec<Message>> {
        // A panic while holding the log cannot corrupt it: every mutation is a single push.
        self.inner
            .messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl Broker for InMemoryBroker {
    fn topic(&self) -> &str {
        &self.inner.topic
    }

    fn tag(&self) -> &str {
        &self.inner.tag
    }

    async fn publish(&self, message: Message) -> Result<(), BrokerError> {
        if message.topic != self.inner.topic {
            return Err(BrokerError::invalid(format!(
                "message topic {:?} does not belong to partition topic {:?}",
                message.topic, self.inner.topic
            )));
        }
        if message.tag != self.inner.tag {
            return Err(BrokerError::invalid(format!(
                "message tag {:?} does not belong to partition tag {:?}",
                message.tag, self.inner.tag
            )));
        }
        self.messages().push(message);
        Ok(())
    }

    async fn fetch(&self, cursor: &str, max: usize) -> Result<Vec<Message>, BrokerError> {
        let cursor =
            Cursor::parse(cursor).map_err(|error| BrokerError::invalid(error.to_string()))?;
        let start = usize::try_from(cursor.offset())
            .map_err(|_| BrokerError::invalid(format!("cursor {cursor} is out of range")))?;
        let messages = self.messages();
        if start > messages.len() {
            // Seeking past the end means the caller's position belongs to another partition or
            // an older, longer queue. Replaying from zero would silently duplicate history.
            return Err(BrokerError::invalid(format!(
                "cursor offset {start} is beyond the queue length {}",
                messages.len()
            )));
        }
        let end = start.saturating_add(max).min(messages.len());
        Ok(messages[start..end].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use agentscope_durable::SessionKey;

    use super::*;
    use crate::block_on;
    use crate::envelope::Envelope;
    use crate::route::route;
    use crate::tests::event;

    fn session() -> SessionKey {
        SessionKey::new("tenant", "session").expect("session key")
    }

    fn message(sequence: u64) -> Message {
        let session = session();
        let envelope = Envelope::from_event(&event(&session, sequence)).expect("seal");
        Message::seal(&route(&session), &envelope).expect("message")
    }

    #[test]
    fn a_partition_serves_its_own_messages_in_pages() {
        let broker = InMemoryBroker::for_session(&session());
        assert!(broker.is_empty());
        block_on(broker.publish(message(1))).expect("publish");
        block_on(broker.publish(message(2))).expect("publish");
        assert_eq!(broker.len(), 2);

        let first = block_on(broker.fetch("0", 1)).expect("fetch");
        assert_eq!(first.len(), 1);
        let second = block_on(broker.fetch("1", 10)).expect("fetch");
        assert_eq!(second.len(), 1);
        assert_eq!(
            Envelope::decode(&second[0].body).expect("decode").sequence,
            2
        );
        assert!(block_on(broker.fetch("2", 10)).expect("fetch").is_empty());
    }

    #[test]
    fn a_foreign_topic_or_tag_is_rejected() {
        let broker = InMemoryBroker::for_session(&session());
        let wrong_topic = Message::new("other", broker.tag().to_string(), "k", Vec::new());
        assert!(matches!(
            block_on(broker.publish(wrong_topic)),
            Err(BrokerError::Invalid(_))
        ));

        let wrong_tag = Message::new(broker.topic().to_string(), "other", "k", Vec::new());
        assert!(matches!(
            block_on(broker.publish(wrong_tag)),
            Err(BrokerError::Invalid(_))
        ));
        assert!(broker.is_empty());
    }

    #[test]
    fn seeking_past_the_end_is_an_error_not_a_replay() {
        let broker = InMemoryBroker::for_session(&session());
        block_on(broker.publish(message(1))).expect("publish");
        assert!(matches!(
            block_on(broker.fetch("2", 10)),
            Err(BrokerError::Invalid(_))
        ));
        assert!(matches!(
            block_on(broker.fetch("not-a-cursor", 10)),
            Err(BrokerError::Invalid(_))
        ));
    }

    #[test]
    fn a_zero_limit_reads_nothing() {
        let broker = InMemoryBroker::for_session(&session());
        block_on(broker.publish(message(1))).expect("publish");
        assert!(block_on(broker.fetch("0", 0)).expect("fetch").is_empty());
    }

    #[test]
    fn clones_share_one_log() {
        let broker = InMemoryBroker::for_session(&session());
        let clone = broker.clone();
        block_on(clone.publish(message(1))).expect("publish");
        assert_eq!(broker.len(), 1);
    }
}
