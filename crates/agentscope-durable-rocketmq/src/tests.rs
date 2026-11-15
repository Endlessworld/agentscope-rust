//! End-to-end tests of the event channel: routing, paging, dedupe, ordering, resume.
//!
//! Every test owns its broker, session and consumer instance; nothing here touches global
//! state, so the suite is order-independent and safe to run in parallel.

use std::collections::VecDeque;
use std::sync::Mutex;

use agentscope_durable::{PublicEvent, SessionKey};
use serde_json::json;

use crate::block_on;
use crate::broker::{Broker, InMemoryBroker};
use crate::consumer::Consumer;
use crate::cursor::Cursor;
use crate::envelope::Envelope;
use crate::error::{BrokerError, ConsumeError};
use crate::message::Message;
use crate::route::route;

/// A committed durable event as the runtime would publish it.
pub(crate) fn event(session: &SessionKey, sequence: u64) -> PublicEvent {
    PublicEvent {
        schema_version: "1".to_string(),
        event_id: format!("evt-{sequence}"),
        session: session.clone(),
        thread_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        run_id: None,
        step_id: None,
        sequence: sequence.to_string(),
        cursor: format!("{}:{sequence}", session.storage_key()),
        event_type: "turn/status".to_string(),
        data: json!({ "sequence": sequence }),
    }
}

fn event_with_id(session: &SessionKey, sequence: u64, event_id: &str) -> PublicEvent {
    let mut event = event(session, sequence);
    event.event_id = event_id.to_string();
    event
}

fn session() -> SessionKey {
    SessionKey::new("tenant", "session").expect("session key")
}

fn message_for(session: &SessionKey, event: &PublicEvent) -> Message {
    let envelope = Envelope::from_event(event).expect("seal");
    Message::seal(&route(session), &envelope).expect("message")
}

fn message(session: &SessionKey, sequence: u64) -> Message {
    message_for(session, &event(session, sequence))
}

fn publish(broker: &InMemoryBroker, session: &SessionKey, sequence: u64) {
    block_on(broker.publish(message(session, sequence))).expect("publish");
}

fn sequences(batch: &crate::Batch) -> Vec<u64> {
    batch
        .events
        .iter()
        .map(|event| event.sequence.parse::<u64>().expect("decimal sequence"))
        .collect()
}

#[test]
fn an_empty_queue_returns_an_empty_batch_without_moving_the_cursor() {
    let session = session();
    let broker = InMemoryBroker::for_session(&session);
    let mut consumer = Consumer::new(broker.clone(), session.clone());

    let batch = block_on(consumer.poll(10)).expect("empty poll must succeed");
    assert!(batch.is_empty());
    assert_eq!(batch.duplicates, 0);
    assert_eq!(batch.next, Cursor::START);
    assert_eq!(consumer.cursor(), Cursor::START);

    publish(&broker, &session, 1);
    let batch = block_on(consumer.poll(10)).expect("poll after publish");
    assert_eq!(sequences(&batch), vec![1]);
    assert_eq!(batch.next.offset(), 1);
}

#[test]
fn batches_follow_the_queue_without_gaps_or_repeats() {
    let session = session();
    let broker = InMemoryBroker::for_session(&session);
    for sequence in 1..=5 {
        publish(&broker, &session, sequence);
    }
    let mut consumer = Consumer::new(broker, session);

    let first = block_on(consumer.poll(2)).expect("poll 1");
    assert_eq!(sequences(&first), vec![1, 2]);
    assert_eq!(first.next.offset(), 2);

    let second = block_on(consumer.poll(2)).expect("poll 2");
    assert_eq!(sequences(&second), vec![3, 4]);

    let third = block_on(consumer.poll(2)).expect("poll 3");
    assert_eq!(sequences(&third), vec![5]);
    assert_eq!(third.next.offset(), 5);

    let drained = block_on(consumer.poll(2)).expect("poll 4");
    assert!(drained.is_empty());
    assert_eq!(drained.next.offset(), 5);
    assert_eq!(consumer.last_sequence(), Some(5));
}

#[test]
fn a_redelivery_inside_one_batch_is_dropped_before_ordering() {
    let session = session();
    let broker = InMemoryBroker::for_session(&session);
    let first = message(&session, 1);
    block_on(broker.publish(first.clone())).expect("publish");
    block_on(broker.publish(first)).expect("redeliver");
    publish(&broker, &session, 2);

    let mut consumer = Consumer::new(broker, session);
    let batch = block_on(consumer.poll(10)).expect("poll");
    assert_eq!(sequences(&batch), vec![1, 2]);
    assert_eq!(batch.duplicates, 1);
    assert_eq!(batch.next.offset(), 3);
}

#[test]
fn a_redelivery_in_a_later_batch_is_dropped_and_still_advances_the_cursor() {
    let session = session();
    let broker = InMemoryBroker::for_session(&session);
    let first = message(&session, 1);
    block_on(broker.publish(first.clone())).expect("publish");

    let mut consumer = Consumer::new(broker.clone(), session.clone());
    assert_eq!(
        sequences(&block_on(consumer.poll(1)).expect("poll")),
        vec![1]
    );

    // The broker hands the same message over again at a new offset.
    block_on(broker.publish(first)).expect("redeliver");
    let batch = block_on(consumer.poll(1)).expect("poll");
    assert!(batch.is_empty());
    assert_eq!(batch.duplicates, 1);
    assert_eq!(batch.next.offset(), 2);
}

#[test]
fn a_sequence_regression_is_an_error_not_a_silent_accept() {
    let session = session();
    let broker = InMemoryBroker::for_session(&session);
    publish(&broker, &session, 2);
    publish(&broker, &session, 1);

    let mut consumer = Consumer::new(broker, session);
    assert_eq!(
        sequences(&block_on(consumer.poll(1)).expect("poll")),
        vec![2]
    );

    match block_on(consumer.poll(1)) {
        Err(ConsumeError::OutOfOrder {
            previous,
            next,
            event_id,
        }) => {
            assert_eq!((previous, next), (2, 1));
            assert_eq!(event_id, "evt-1");
        }
        other => panic!("expected an out-of-order rejection, got {other:?}"),
    }
    // The rejected message is still at the cursor: nothing was consumed on its behalf.
    assert_eq!(consumer.cursor().offset(), 1);
    assert_eq!(consumer.last_sequence(), Some(2));
}

#[test]
fn a_rejected_batch_does_not_commit_partial_progress() {
    let session = session();
    let good = message(&session, 1);
    let regressed = message_for(&session, &event_with_id(&session, 1, "evt-odd"));
    let scripted = ScriptedBroker::new(vec![vec![good.clone(), regressed], vec![good]]);

    let mut consumer = Consumer::new(scripted, session);
    assert!(matches!(
        block_on(consumer.poll(10)),
        Err(ConsumeError::OutOfOrder { .. })
    ));
    assert_eq!(consumer.cursor(), Cursor::START);

    // The retry delivers the first event again; it must not have been marked as seen.
    let batch = block_on(consumer.poll(10)).expect("retry");
    assert_eq!(sequences(&batch), vec![1]);
    assert_eq!(batch.duplicates, 0);
}

#[test]
fn resuming_from_the_stored_cursor_continues_exactly_once() {
    let session = session();
    let broker = InMemoryBroker::for_session(&session);
    for sequence in 1..=3 {
        publish(&broker, &session, sequence);
    }

    let mut first_run = Consumer::new(broker.clone(), session.clone());
    let batch = block_on(first_run.poll(2)).expect("poll");
    assert_eq!(sequences(&batch), vec![1, 2]);
    let resume = batch.next;
    let durable_cursor = batch
        .durable_cursor()
        .expect("delivered events carry a durable cursor")
        .to_string();
    assert_eq!(durable_cursor, format!("{}:2", session.storage_key()));
    drop(first_run);

    let mut second_run = Consumer::open(broker, session, resume);
    let batch = block_on(second_run.poll(10)).expect("resume");
    assert_eq!(sequences(&batch), vec![3]);
    assert_eq!(batch.next.offset(), 3);
}

#[test]
fn a_consumer_does_not_share_dedupe_state_with_another() {
    let session = session();
    let broker = InMemoryBroker::for_session(&session);
    let first = message(&session, 1);
    block_on(broker.publish(first.clone())).expect("publish e1");
    publish(&broker, &session, 2);

    let mut consumer = Consumer::new(broker.clone(), session.clone());
    assert_eq!(
        sequences(&block_on(consumer.poll(10)).expect("poll")),
        vec![1, 2]
    );
    block_on(broker.publish(first)).expect("redeliver e1");
    let redelivered = block_on(consumer.poll(10)).expect("poll");
    assert!(redelivered.is_empty());
    assert_eq!(redelivered.duplicates, 1);

    // A fresh consumer has its own seen set: no process-wide registry is involved.
    let mut fresh = Consumer::new(broker, session);
    let batch = block_on(fresh.poll(10)).expect("fresh poll");
    assert_eq!(sequences(&batch), vec![1, 2]);
    assert_eq!(batch.duplicates, 1);
}

#[test]
fn an_unreadable_body_is_rejected_without_consuming_the_batch() {
    let session = session();
    let route_ = route(&session);
    let good = message(&session, 1);
    let junk = Message::new(
        route_.topic.clone(),
        route_.tag.clone(),
        route_.key.clone(),
        b"junk",
    );
    let scripted = ScriptedBroker::new(vec![vec![good.clone(), junk], vec![good]]);

    let mut consumer = Consumer::new(scripted, session);
    assert!(matches!(
        block_on(consumer.poll(10)),
        Err(ConsumeError::Envelope(_))
    ));
    assert_eq!(consumer.cursor(), Cursor::START);
    assert_eq!(
        sequences(&block_on(consumer.poll(10)).expect("retry")),
        vec![1]
    );
}

#[test]
fn a_message_for_another_partition_is_rejected() {
    let session = session();
    let route_ = route(&session);
    let envelope = Envelope::from_event(&event(&session, 1)).expect("seal");
    let misrouted = Message::new(
        route_.topic.clone(),
        "s-other".to_string(),
        route_.key.clone(),
        envelope.encode().expect("encode"),
    );
    let scripted = ScriptedBroker::new(vec![vec![misrouted]]);

    let mut consumer = Consumer::new(scripted, session);
    assert!(matches!(
        block_on(consumer.poll(10)),
        Err(ConsumeError::Invalid(_))
    ));
    assert_eq!(consumer.cursor(), Cursor::START);
}

/// A broker that hands out pre-scripted pages, so tests can model a redelivery, a cross-wired
/// message or a half-broken batch that an `InMemoryBroker` would refuse to build.
struct ScriptedBroker {
    pages: Mutex<VecDeque<Vec<Message>>>,
}

impl ScriptedBroker {
    fn new(pages: Vec<Vec<Message>>) -> Self {
        Self {
            pages: Mutex::new(pages.into()),
        }
    }
}

impl Broker for ScriptedBroker {
    async fn publish(&self, _message: Message) -> Result<(), BrokerError> {
        Err(BrokerError::invalid("the scripted broker is read-only"))
    }

    async fn fetch(&self, _cursor: &str, _max: usize) -> Result<Vec<Message>, BrokerError> {
        let mut pages = self
            .pages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(pages.pop_front().unwrap_or_default())
    }
}
