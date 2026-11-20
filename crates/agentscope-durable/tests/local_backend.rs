//! The façade a host opens without assembling a store itself.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use agentscope_durable::executor::{Executor, ExecutorRegistry};
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::runtime::{DurableRuntime, TickOutcome};
use agentscope_durable::types::{
    ExecutionRequest, ExecutionResult, SessionKey, SessionSpec, ThreadBinding, ThreadKey,
    ThreadSpec, TurnStatus,
};
use agentscope_durable::{LocalBackend, Result};

struct StubAdapter;

#[async_trait]
impl HarnessAdapter for StubAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            state_version: "state-1".to_string(),
            recovery_boundary: true,
            event_delivery: false,
        }
    }

    async fn execute(&self, _ctx: &HarnessContext) -> Result<()> {
        Ok(())
    }
}

struct CountingExecutor(Arc<AtomicUsize>);

#[async_trait]
impl Executor for CountingExecutor {
    async fn start(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult::succeeded(json!(null)))
    }
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "durable-local-{name}-{}-{stamp}",
        std::process::id()
    ))
}

fn session_and_thread() -> (SessionSpec, ThreadSpec) {
    let session = SessionKey::new("ns", "s1").expect("session key");
    let binding = ThreadBinding::new("stub", "definition-1", "state-1").expect("binding");
    let thread = ThreadKey::new(session.clone(), "main").expect("thread key");
    (
        SessionSpec {
            key: session,
            primary_binding: binding.clone(),
            metadata: json!({}),
        },
        ThreadSpec {
            key: thread,
            binding,
        },
    )
}

#[tokio::test]
async fn a_local_backend_runs_a_thread_end_to_end() {
    let root = scratch_dir("e2e");
    let backend = LocalBackend::open(&root).expect("open");
    let (session, thread_spec) = session_and_thread();
    backend.create(session).await.expect("create session");
    let thread = thread_spec.key.clone();
    backend
        .create_thread(thread_spec)
        .await
        .expect("create thread");
    let receipt = backend
        .submit(&thread, "request-1", json!({"text": "hello"}))
        .await
        .expect("submit");

    let runtime = DurableRuntime::new(Arc::clone(&backend), "host-1")
        .with_adapter("stub", Arc::new(StubAdapter));
    let outcome = runtime.tick(&thread).await.expect("tick");
    assert!(matches!(outcome, TickOutcome::Settled));
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(turn.status, TurnStatus::Running);

    drop(runtime);
    drop(backend);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn a_second_handle_opens_the_same_store() {
    let root = scratch_dir("shared");
    let first = LocalBackend::open(&root).expect("first open");
    let (session, thread_spec) = session_and_thread();
    let key = session.key.clone();
    first.create(session).await.expect("create session");
    first
        .create_thread(thread_spec)
        .await
        .expect("create thread");

    let second = LocalBackend::open(&root).expect("second open");
    let view = second
        .session(&key)
        .await
        .expect("session from second handle");
    assert_eq!(view.key, key);

    drop(second);
    drop(first);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_unknown_executor_name_does_not_fall_back() {
    let registry = ExecutorRegistry::new(Arc::new(CountingExecutor(Arc::new(AtomicUsize::new(0)))));
    assert!(registry.resolve("local").is_some());
    assert!(registry.resolve("sandbox").is_none());
}

#[test]
fn a_clone_shares_the_executor_registry() {
    let runtime = DurableRuntime::new(LocalBackend::in_memory(), "host-1");
    let clone = runtime.clone();
    clone.with_executor_named(
        "sandbox",
        Arc::new(CountingExecutor(Arc::new(AtomicUsize::new(0)))),
    );
    assert!(runtime.executors().resolve("sandbox").is_some());
}
