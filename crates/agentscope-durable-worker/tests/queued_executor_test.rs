//! The queued executor: one effect, one durable record, no resend.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::execution_store::{
    DispatchRecord, DispatchState, ExecutionStore, MemoryExecutionStore,
};
use agentscope_durable::executor::Executor;
use agentscope_durable::types::{
    ExecutionRequest, ExecutionResult, ExecutionState, SessionKey, ThreadKey, ToolCall,
};
use agentscope_durable::{now_millis, DurableError, Result as DurableResult};
use agentscope_durable_worker::QueuedExecutor;
use async_trait::async_trait;
use serde_json::json;

struct StubExecutor {
    start: Option<ExecutionResult>,
    query: ExecutionResult,
    starts: AtomicUsize,
    queries: AtomicUsize,
}

impl StubExecutor {
    fn new(start: Option<ExecutionResult>, query: ExecutionResult) -> Self {
        Self {
            start,
            query,
            starts: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
        }
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for StubExecutor {
    async fn start(&self, _request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        match &self.start {
            Some(result) => Ok(result.clone()),
            None => Err(DurableError::unavailable("the peer is down")),
        }
    }

    async fn query(&self, _request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Ok(self.query.clone())
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

fn pending() -> ExecutionResult {
    ExecutionResult {
        state: ExecutionState::Pending,
        handle: Some("exec-1".to_string()),
        result: json!(null),
        error: None,
    }
}

fn unknown() -> ExecutionResult {
    ExecutionResult::unknown("cannot tell")
}

fn dispatched_record() -> DispatchRecord {
    DispatchRecord {
        execution_id: "exec-1".to_string(),
        request: request(),
        state: DispatchState::Dispatched,
        attempts: 1,
        dispatched_at: now_millis() - 10_000,
        result: None,
    }
}

#[tokio::test]
async fn a_settled_start_is_answered_from_the_record() {
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let inner = Arc::new(StubExecutor::new(
        Some(ExecutionResult::succeeded(json!({"ok": true}))),
        unknown(),
    ));
    let queued = QueuedExecutor::new(Arc::clone(&inner) as Arc<dyn Executor>, store);

    let first = queued.start(&request()).await.expect("first start");
    assert_eq!(first.state, ExecutionState::Succeeded);
    let second = queued.start(&request()).await.expect("second start");
    assert_eq!(second.state, ExecutionState::Succeeded);
    assert_eq!(inner.starts(), 1, "the effect left the process once");
}

#[tokio::test]
async fn a_pending_dispatch_is_not_resent_after_a_restart() {
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let first_inner = Arc::new(StubExecutor::new(Some(pending()), unknown()));
    let first = QueuedExecutor::new(
        Arc::clone(&first_inner) as Arc<dyn Executor>,
        Arc::clone(&store),
    );
    assert_eq!(
        first.start(&request()).await.expect("start").state,
        ExecutionState::Pending
    );
    assert_eq!(first_inner.starts(), 1);
    assert_eq!(
        store.get("exec-1").expect("get").expect("record").state,
        DispatchState::Dispatched
    );

    let second_inner = Arc::new(StubExecutor::new(
        Some(ExecutionResult::succeeded(json!(null))),
        unknown(),
    ));
    let second = QueuedExecutor::new(Arc::clone(&second_inner) as Arc<dyn Executor>, store);
    assert_eq!(
        second.start(&request()).await.expect("second start").state,
        ExecutionState::Pending
    );
    assert_eq!(second_inner.starts(), 0, "a restart must not resend");
}

#[tokio::test]
async fn a_failed_start_is_unknown_and_not_resent() {
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let first_inner = Arc::new(StubExecutor::new(None, unknown()));
    let first = QueuedExecutor::new(
        Arc::clone(&first_inner) as Arc<dyn Executor>,
        Arc::clone(&store),
    );
    assert!(first.start(&request()).await.is_err());
    assert_eq!(
        store.get("exec-1").expect("get").expect("record").state,
        DispatchState::Unknown,
        "a failed publication may still have reached the peer"
    );

    let second_inner = Arc::new(StubExecutor::new(
        Some(ExecutionResult::succeeded(json!(null))),
        unknown(),
    ));
    let second = QueuedExecutor::new(Arc::clone(&second_inner) as Arc<dyn Executor>, store);
    assert_eq!(
        second.start(&request()).await.expect("second start").state,
        ExecutionState::Unknown
    );
    assert_eq!(second_inner.starts(), 0);
}

#[tokio::test]
async fn a_cancelled_dispatch_records_doubt() {
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let inner = Arc::new(StubExecutor::new(Some(pending()), unknown()));
    let queued = QueuedExecutor::new(Arc::clone(&inner) as Arc<dyn Executor>, Arc::clone(&store));
    queued.start(&request()).await.expect("start");
    let cancelled = queued.cancel(&request()).await.expect("cancel");
    assert_eq!(cancelled.state, ExecutionState::Unknown);
    assert_eq!(
        store.get("exec-1").expect("get").expect("record").state,
        DispatchState::Unknown
    );
}

#[tokio::test]
async fn sweep_marks_an_unanswerable_dispatch_unknown_without_resending() {
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    store.put(&dispatched_record()).expect("put");
    let inner = Arc::new(StubExecutor::new(
        Some(ExecutionResult::succeeded(json!(null))),
        unknown(),
    ));
    let queued = QueuedExecutor::new(Arc::clone(&inner) as Arc<dyn Executor>, Arc::clone(&store));

    let swept = queued
        .sweep(Duration::from_secs(1), 10)
        .await
        .expect("sweep");
    assert_eq!(swept, 1);
    assert_eq!(inner.starts(), 0, "a sweep never resends");
    assert_eq!(
        store.get("exec-1").expect("get").expect("record").state,
        DispatchState::Unknown
    );
}

#[tokio::test]
async fn sweep_settles_a_dispatch_the_inner_can_still_find() {
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    store.put(&dispatched_record()).expect("put");
    let inner = Arc::new(StubExecutor::new(
        Some(ExecutionResult::succeeded(json!(null))),
        ExecutionResult::succeeded(json!({"found": true})),
    ));
    let queued = QueuedExecutor::new(Arc::clone(&inner) as Arc<dyn Executor>, Arc::clone(&store));

    assert_eq!(
        queued
            .sweep(Duration::from_secs(1), 10)
            .await
            .expect("sweep"),
        1
    );
    let record = store.get("exec-1").expect("get").expect("record");
    assert_eq!(record.state, DispatchState::Settled);
    assert_eq!(
        record.result.map(|result| result.state),
        Some(ExecutionState::Succeeded)
    );
}

#[tokio::test]
async fn a_young_dispatch_is_left_alone() {
    let store: Arc<dyn ExecutionStore> = Arc::new(MemoryExecutionStore::new());
    let mut record = dispatched_record();
    record.dispatched_at = now_millis();
    store.put(&record).expect("put");
    let inner = Arc::new(StubExecutor::new(
        Some(ExecutionResult::succeeded(json!(null))),
        unknown(),
    ));
    let queued = QueuedExecutor::new(Arc::clone(&inner) as Arc<dyn Executor>, store);
    assert_eq!(
        queued
            .sweep(Duration::from_secs(60), 10)
            .await
            .expect("sweep"),
        0
    );
    assert_eq!(inner.queries.load(Ordering::SeqCst), 0);
}
