//! Bounded event queue between the agent's (synchronous) event sink and the
//! prompt loop that forwards events to the ACP client.
//!
//! The sink cannot block (it is called from inside the run) and the client can
//! be arbitrarily slow, so an unbounded channel let a stalled client grow the
//! queue without limit. This queue bounds the backlog instead: when it is full
//! the *newest* event is dropped and counted, and the end of the turn tells the
//! client that output was lost.
//!
//! `AGENT_RESULT` is never dropped: it is the only event carrying the reply when
//! nothing was streamed, and there is exactly one per run.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use agentscope_core::event::{AgentEvent, AgentEventType};
use tokio::sync::Notify;

/// Default backlog for one turn. Deltas are small, so this is a few MB at most.
pub(crate) const EVENT_QUEUE_CAPACITY: usize = 4096;

/// Lock helper that tolerates poisoning: a panic elsewhere must not turn a
/// queue push into a second panic inside the run.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Inner {
    ready: VecDeque<AgentEvent>,
    capacity: usize,
    dropped: u64,
    dropped_deltas: u64,
    closed: bool,
}

/// The shared queue. Held by the sink (through [`EventQueueSender`]) and by the
/// prompt loop.
pub(crate) struct EventQueue {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl EventQueue {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                ready: VecDeque::new(),
                capacity: capacity.max(1),
                dropped: 0,
                dropped_deltas: 0,
                closed: false,
            }),
            notify: Notify::new(),
        })
    }

    /// Enqueues an event without blocking. Returns `false` when the event was
    /// dropped because the backlog is full.
    pub(crate) fn push(&self, event: AgentEvent) -> bool {
        let terminal = matches!(event, AgentEvent::AgentResult(_));
        {
            let mut inner = lock(&self.inner);
            if inner.closed {
                return false;
            }
            if inner.ready.len() >= inner.capacity && !terminal {
                inner.dropped += 1;
                if matches!(
                    event.event_type(),
                    AgentEventType::TextBlockDelta | AgentEventType::ThinkingBlockDelta
                ) {
                    inner.dropped_deltas += 1;
                }
                return false;
            }
            inner.ready.push_back(event);
        }
        self.notify.notify_one();
        true
    }

    /// Waits for the next event; `None` once the queue is closed and drained.
    pub(crate) async fn recv(&self) -> Option<AgentEvent> {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = lock(&self.inner);
                if let Some(event) = inner.ready.pop_front() {
                    return Some(event);
                }
                if inner.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.inner).ready.is_empty()
    }

    /// Number of events dropped so far (all types).
    pub(crate) fn dropped(&self) -> u64 {
        lock(&self.inner).dropped
    }

    /// Number of dropped *text/thinking deltas*, i.e. output the client never saw.
    pub(crate) fn dropped_deltas(&self) -> u64 {
        lock(&self.inner).dropped_deltas
    }

    fn close(&self) {
        lock(&self.inner).closed = true;
        self.notify.notify_one();
    }
}

/// Sink-side handle. Dropping it closes the queue, which is how the prompt loop
/// learns that no further events can arrive for this turn.
pub(crate) struct EventQueueSender(Arc<EventQueue>);

impl EventQueueSender {
    pub(crate) fn new(queue: Arc<EventQueue>) -> Self {
        Self(queue)
    }

    pub(crate) fn push(&self, event: AgentEvent) {
        self.0.push(event);
    }
}

impl Drop for EventQueueSender {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::EventData;

    fn delta(text: &str) -> AgentEvent {
        let mut data = EventData::default();
        data.fields
            .insert("text".to_string(), serde_json::Value::String(text.into()));
        AgentEvent::TextBlockDelta(data)
    }

    #[tokio::test]
    async fn events_are_delivered_in_order() {
        let queue = EventQueue::new(8);
        let sender = EventQueueSender::new(Arc::clone(&queue));
        for i in 0..3 {
            assert!(queue.push(delta(&format!("chunk-{i}"))));
        }
        drop(sender);
        let mut seen = 0;
        while queue.recv().await.is_some() {
            seen += 1;
        }
        assert_eq!(seen, 3);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_newest_and_counts_deltas() {
        let queue = EventQueue::new(2);
        assert!(queue.push(delta("a")));
        assert!(queue.push(delta("b")));
        assert!(!queue.push(delta("c")));
        assert!(!queue.push(delta("d")));
        assert_eq!(queue.dropped(), 2);
        assert_eq!(queue.dropped_deltas(), 2);
        // The backlog itself stays bounded.
        assert_eq!(
            queue.recv().await.map(|e| e.event_type()),
            Some(AgentEventType::TextBlockDelta)
        );
        assert_eq!(
            queue.recv().await.map(|e| e.event_type()),
            Some(AgentEventType::TextBlockDelta)
        );
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_full_queue_still_accepts_the_agent_result() {
        let queue = EventQueue::new(1);
        assert!(queue.push(delta("a")));
        assert!(!queue.push(delta("b")));
        assert!(queue.push(AgentEvent::AgentResult(EventData::default())));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.dropped_deltas(), 1);
        // The result is delivered after the queued delta, never dropped.
        assert_eq!(
            queue.recv().await.map(|e| e.event_type()),
            Some(AgentEventType::TextBlockDelta)
        );
        assert_eq!(
            queue.recv().await.map(|e| e.event_type()),
            Some(AgentEventType::AgentResult)
        );
    }

    #[tokio::test]
    async fn dropping_the_sender_closes_the_queue() {
        let queue = EventQueue::new(4);
        let sender = EventQueueSender::new(Arc::clone(&queue));
        sender.push(delta("a"));
        drop(sender);
        assert_eq!(
            queue.recv().await.map(|e| e.event_type()),
            Some(AgentEventType::TextBlockDelta)
        );
        assert!(queue.recv().await.is_none());
    }

    #[tokio::test]
    async fn recv_wakes_up_when_an_event_arrives_later() {
        let queue = EventQueue::new(4);
        let sender = EventQueueSender::new(Arc::clone(&queue));
        let consumer = {
            let queue = Arc::clone(&queue);
            tokio::spawn(async move { queue.recv().await.map(|e| e.event_type()) })
        };
        tokio::task::yield_now().await;
        sender.push(delta("late"));
        assert_eq!(
            consumer.await.unwrap(),
            Some(AgentEventType::TextBlockDelta)
        );
    }
}
