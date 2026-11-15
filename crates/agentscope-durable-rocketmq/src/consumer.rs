//! Consumer-side correctness: de-duplication, ordering and cursor bookkeeping.
//!
//! This is where the at-least-once contract is discharged. Everything the consumer learns during
//! one batch is staged and committed only when the whole batch is admissible; a rejected batch
//! leaves the cursor, the seen set and the last sequence untouched so the next poll reads the
//! same messages again. Committing eagerly would turn a retryable error into silent data loss.

use std::collections::HashSet;

use agentscope_durable::{PublicEvent, SessionKey};

use crate::broker::Broker;
use crate::cursor::Cursor;
use crate::envelope::Envelope;
use crate::error::ConsumeError;
use crate::route::{route, Route};

/// What one poll delivered.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// Committed events, in sequence order, without duplicates.
    pub events: Vec<PublicEvent>,
    /// Position to pass to the next poll (and to persist before acknowledging).
    pub next: Cursor,
    /// Redeliveries that were dropped because their `eventId` had already been returned.
    pub duplicates: usize,
}

impl Batch {
    /// Durable cursor of the last delivered event, for hosts that persist both positions.
    pub fn durable_cursor(&self) -> Option<&str> {
        self.events.last().map(|event| event.cursor.as_str())
    }

    /// Whether the partition was empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// A consumer of exactly one session partition.
#[derive(Debug)]
pub struct Consumer<B> {
    broker: B,
    session: SessionKey,
    route: Route,
    cursor: Cursor,
    /// Event identities already returned to the caller, so a redelivery is dropped instead of
    /// returned twice.
    ///
    /// Process-local by design; see [`Consumer::open`]. The set grows with the number of distinct
    /// events this consumer instance has read, which is one tick loop's worth of history in
    /// practice: a host that keeps one consumer for the lifetime of a long session should rebuild
    /// it from the persisted cursor. Bounding the set with an eviction window is deliberately not
    /// done — evicting an id re-opens exactly the duplicate the set exists to drop.
    seen: HashSet<String>,
    last_sequence: Option<u64>,
}

impl<B: Broker> Consumer<B> {
    /// Read the partition from the beginning.
    pub fn new(broker: B, session: SessionKey) -> Self {
        Self::open(broker, session, Cursor::START)
    }

    /// Resuming at a persisted offset starts with an empty seen set. De-duplication across a
    /// restart is the durable log's job (the queue offset is persisted *after* the batch is
    /// committed), so a process restart may hand a batch back to the caller but the durable
    /// journal will refuse to append the same `eventId` twice.
    /// TODO: durable de-duplication across process restarts is intentionally not implemented. It
    /// would need a record store keyed by `eventId` (a write per event, or at best per batch) and
    /// the durable journal already rejects a duplicate identity with a conflicting digest; a
    /// second persistent index would duplicate that mechanism and its write cost.
    pub fn open(broker: B, session: SessionKey, cursor: Cursor) -> Self {
        let route = route(&session);
        Self {
            broker,
            session,
            route,
            cursor,
            seen: HashSet::new(),
            last_sequence: None,
        }
    }

    /// Position the next poll starts from.
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Session this consumer is bound to.
    pub fn session(&self) -> &SessionKey {
        &self.session
    }

    /// Last sequence committed through this consumer.
    pub fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    /// Fetch and validate up to `max` messages.
    ///
    /// The returned events are contiguous with what was returned before; redeliveries are counted
    /// in [`Batch::duplicates`] and never returned twice. Advancing the cursor *is* the
    /// acknowledgement: a host must persist [`Batch::next`] before processing the events, and may
    /// process a batch again after a crash because the consumer will drop the already-seen ids.
    ///
    /// A duplicate is dropped by `eventId` without comparing bodies: the durable writer rejects an
    /// event identity whose payload changed (`Event identity changed`), so two different bodies
    /// under one id cannot come out of the runtime, and keeping a digest per event would double the
    /// memory of the seen set for a case the writer already forbids.
    pub async fn poll(&mut self, max: usize) -> Result<Batch, ConsumeError> {
        if max == 0 {
            return Err(ConsumeError::invalid("max must be at least 1"));
        }
        let messages = self
            .broker
            .fetch(&self.cursor.to_string(), max)
            .await
            .map_err(ConsumeError::Broker)?;

        let mut events = Vec::with_capacity(messages.len());
        let mut staged = HashSet::new();
        let mut duplicates = 0usize;
        let mut last_sequence = self.last_sequence;

        for message in &messages {
            if message.topic != self.route.topic || message.tag != self.route.tag {
                return Err(ConsumeError::invalid(format!(
                    "message on topic {:?}/tag {:?} arrived on the partition {:?}/{:?}",
                    message.topic, message.tag, self.route.topic, self.route.tag
                )));
            }
            let envelope = Envelope::decode(&message.body)?;
            let event = envelope.event()?;
            if event.session != self.session {
                return Err(ConsumeError::invalid(format!(
                    "event {:?} belongs to session {:?}/{}, not {:?}/{}",
                    event.event_id,
                    event.session.namespace,
                    event.session.session_id,
                    self.session.namespace,
                    self.session.session_id
                )));
            }

            if self.seen.contains(&event.event_id) || staged.contains(&event.event_id) {
                // At-least-once redelivery. Drop it before the ordering check: a redelivery is
                // allowed to repeat an already accepted sequence, a *fresh* event is not.
                duplicates += 1;
                continue;
            }
            if let Some(previous) = last_sequence {
                if envelope.sequence <= previous {
                    return Err(ConsumeError::OutOfOrder {
                        previous,
                        next: envelope.sequence,
                        event_id: event.event_id,
                    });
                }
            }
            last_sequence = Some(envelope.sequence);
            staged.insert(event.event_id.clone());
            events.push(event);
        }

        // Commit only now: the batch as a whole became part of our position.
        self.seen.extend(staged);
        self.last_sequence = last_sequence;
        self.cursor = self.cursor.advance(messages.len());
        Ok(Batch {
            events,
            next: self.cursor,
            duplicates,
        })
    }
}

#[cfg(test)]
mod tests {
    use agentscope_durable::SessionKey;

    use super::*;
    use crate::block_on;
    use crate::broker::InMemoryBroker;
    use crate::message::Message;
    use crate::tests::event;

    fn session() -> SessionKey {
        SessionKey::new("tenant", "session").expect("session key")
    }

    /// A broker whose calls always fail, to prove the consumer does not advance on errors.
    struct FailingBroker;

    impl Broker for FailingBroker {
        async fn publish(&self, _message: Message) -> Result<(), crate::BrokerError> {
            Err(crate::BrokerError::unavailable("offline"))
        }

        async fn fetch(
            &self,
            _cursor: &str,
            _max: usize,
        ) -> Result<Vec<Message>, crate::BrokerError> {
            Err(crate::BrokerError::unavailable("offline"))
        }
    }

    #[test]
    fn a_broker_failure_is_surfaced_and_the_cursor_stays() {
        let mut consumer = Consumer::new(FailingBroker, session());
        let error = block_on(consumer.poll(10)).expect_err("must fail");
        assert!(matches!(error, ConsumeError::Broker(_)));
        assert_eq!(consumer.cursor(), Cursor::START);
    }

    #[test]
    fn a_zero_limit_is_rejected_without_calling_the_broker() {
        let mut consumer = Consumer::new(InMemoryBroker::for_session(&session()), session());
        assert!(matches!(
            block_on(consumer.poll(0)),
            Err(ConsumeError::Invalid(_))
        ));
    }

    #[test]
    fn a_foreign_session_event_is_rejected() {
        let session = session();
        let broker = InMemoryBroker::for_session(&session);
        let foreign = SessionKey::new("tenant", "other").expect("session key");
        // Same route metadata, wrong payload: a cross-wired upstream, not a broker mistake.
        let envelope = Envelope::from_event(&event(&foreign, 1)).expect("seal");
        let message = Message::new(
            broker.topic().to_string(),
            broker.tag().to_string(),
            session.storage_key(),
            envelope.encode().expect("encode"),
        );
        block_on(broker.publish(message)).expect("publish");

        let mut consumer = Consumer::new(broker, session);
        assert!(matches!(
            block_on(consumer.poll(10)),
            Err(ConsumeError::Invalid(_))
        ));
        assert_eq!(consumer.cursor(), Cursor::START);
    }
}
