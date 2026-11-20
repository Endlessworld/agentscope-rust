//! The Brain–Hands loop end to end, over the in-memory broker.
//!
//! Every future in this test completes in a single poll: the in-memory broker never yields, and
//! the crate stays free of an async runtime. A second poll would only hide a real bug behind a
//! hang, so `block_on` asserts that it never happens.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::execution_store::{DispatchState, ExecutionStore, MemoryExecutionStore};
use agentscope_durable::executor::Executor;
use agentscope_durable::types::{
    ExecutionRequest, ExecutionResult, ExecutionState, SessionKey, ThreadKey, ToolCall,
};
use agentscope_durable::wakeups::{Wakeup, WakeupReason, Wakeups};
use agentscope_durable::{DurableError, Result as DurableResult};
use agentscope_durable_rocketmq::{
    reply_topic, request_topic, wakeup_topic, Broker, HandsEnvelope, InMemoryBroker, Message,
    RemoteExecutor, ReplyInbox, RocketMqWakeups, RocketMqWorker,
};
use agentscope_durable_worker::QueuedExecutor;
use async_trait::async_trait;
use serde_json::json;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};

    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("this test's futures must complete in a single poll"),
    }
}

struct CountingExecutor {
    calls: Arc<AtomicUsize>,
    result: ExecutionResult,
}

#[async_trait]
impl Executor for CountingExecutor {
    async fn start(&self, _request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.result.clone())
    }
}

fn request() -> ExecutionRequest {
    ExecutionRequest::new(
        "exec-1",
        ThreadKey::new(SessionKey::new("ns", "s1").expect("session"), "main").expect("thread"),
        "turn-1",
        ToolCall::new("call-1", "local", "echo", json!({"text": "hi"})).expect("call"),
    )
    .expect("request")
}

const NAMESPACE: &str = "tenant";
const TAG: &str = "s-test";

fn brokers() -> (Arc<InMemoryBroker>, Arc<InMemoryBroker>) {
    (
        Arc::new(InMemoryBroker::new(request_topic(NAMESPACE), TAG)),
        Arc::new(InMemoryBroker::new(reply_topic(NAMESPACE), TAG)),
    )
}

fn request_message(request: &ExecutionRequest) -> Message {
    let body = HandsEnvelope::request(request, 1).encode().expect("encode");
    Message::new(
        request_topic(NAMESPACE),
        TAG,
        request.execution_id.clone(),
        body,
    )
}

#[test]
fn a_request_reaches_the_hands_and_the_reply_reaches_the_brain() {
    let (requests, replies) = brokers();
    let brain_store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let hands_store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());

    let inbox = Arc::new(ReplyInbox::new(
        Arc::clone(&replies),
        Arc::clone(&brain_store),
    ));
    let remote: Arc<dyn Executor> = Arc::new(RemoteExecutor::new(
        Arc::clone(&requests),
        inbox,
        Arc::clone(&brain_store),
    ));
    let brain = QueuedExecutor::new(remote, Arc::clone(&brain_store));

    let calls = Arc::new(AtomicUsize::new(0));
    let local = Arc::new(CountingExecutor {
        calls: Arc::clone(&calls),
        result: ExecutionResult::succeeded(json!({"ok": true})),
    });
    let worker = RocketMqWorker::new(
        Arc::clone(&requests),
        Arc::clone(&replies),
        local,
        Arc::clone(&hands_store),
    );

    let request = request();
    let started = block_on(brain.start(&request)).expect("start");
    assert_eq!(started.state, ExecutionState::Pending);

    assert_eq!(block_on(worker.poll_once(10)).expect("poll"), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let answered = block_on(brain.query(&request)).expect("query");
    assert_eq!(answered.state, ExecutionState::Succeeded);
    assert_eq!(answered.result, json!({"ok": true}));
}

#[test]
fn a_request_delivered_three_times_runs_once() {
    let (requests, replies) = brokers();
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let local = Arc::new(CountingExecutor {
        calls: Arc::clone(&calls),
        result: ExecutionResult::succeeded(json!(null)),
    });
    let worker = RocketMqWorker::new(
        Arc::clone(&requests),
        Arc::clone(&replies),
        local,
        Arc::clone(&store),
    );

    let request = request();
    for _ in 0..3 {
        block_on(requests.publish(request_message(&request))).expect("publish");
    }
    assert_eq!(block_on(worker.poll_once(10)).expect("poll"), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .get(&request.execution_id)
            .expect("ledger")
            .expect("record")
            .state,
        DispatchState::Settled
    );
}

#[test]
fn a_lost_reply_stays_unknown_and_is_not_resent() {
    let (requests, replies) = brokers();
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let inbox = Arc::new(ReplyInbox::new(Arc::clone(&replies), Arc::clone(&store)));
    let remote: Arc<dyn Executor> = Arc::new(RemoteExecutor::new(
        Arc::clone(&requests),
        inbox,
        Arc::clone(&store),
    ));
    let brain = QueuedExecutor::new(remote, Arc::clone(&store));

    let request = request();
    assert_eq!(
        block_on(brain.start(&request)).expect("start").state,
        ExecutionState::Pending
    );
    assert_eq!(
        block_on(brain.query(&request)).expect("query").state,
        ExecutionState::Unknown
    );

    // A sweep reconciles, it never resends: the request partition still holds one message.
    assert_eq!(block_on(brain.sweep(Duration::ZERO, 10)).expect("sweep"), 1);
    assert_eq!(requests.len(), 1);
    assert_eq!(
        store
            .get(&request.execution_id)
            .expect("ledger")
            .expect("record")
            .state,
        DispatchState::Unknown
    );
    assert_eq!(
        block_on(brain.start(&request)).expect("second start").state,
        ExecutionState::Unknown
    );
    assert_eq!(requests.len(), 1, "a known-unknown execution is not resent");
}

#[test]
fn a_wakeup_reaches_a_second_host_through_the_partition() {
    let broker = Arc::new(InMemoryBroker::new(wakeup_topic(NAMESPACE), TAG));
    let publisher = RocketMqWakeups::new(Arc::clone(&broker));
    let subscriber = RocketMqWakeups::new(Arc::clone(&broker));

    let seen = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&seen);
    subscriber.subscribe(Arc::new(move |_wakeup: &Wakeup| {
        recorded.fetch_add(1, Ordering::SeqCst);
    }));

    let thread =
        ThreadKey::new(SessionKey::new("ns", "s1").expect("session"), "main").expect("thread");
    let wakeup = Wakeup {
        thread: thread.clone(),
        at: 1,
        reason: WakeupReason::Committed,
    };
    publisher.publish(&wakeup).expect("queue");
    assert_eq!(block_on(publisher.flush()).expect("flush"), 1);
    assert_eq!(block_on(subscriber.poll_once(10)).expect("poll"), 1);
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[test]
fn a_second_start_publishes_nothing_while_the_first_is_in_flight() {
    let (requests, replies) = brokers();
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let inbox = Arc::new(ReplyInbox::new(Arc::clone(&replies), Arc::clone(&store)));
    let remote: Arc<dyn Executor> = Arc::new(RemoteExecutor::new(
        Arc::clone(&requests),
        inbox,
        Arc::clone(&store),
    ));
    let brain = QueuedExecutor::new(remote, store);
    let request = request();
    block_on(brain.start(&request)).expect("start");
    block_on(brain.start(&request)).expect("second start");
    assert_eq!(requests.len(), 1);
}
