//! A sandbox execution survives a process: the store is what a restart adopts.

use std::sync::Arc;

use agentscope_durable::executor::Executor;
use agentscope_durable::types::{SessionKey, ThreadKey, ToolCall};
use agentscope_durable::{ExecutionRequest, ExecutionState};
use agentscope_durable_sandbox::{
    BindingRecord, BindingState, BindingStore, ExecSpec, FakeSandbox, FileBindings, Handle,
    MemoryBindings, Poll, Sandbox, SandboxExecutor,
};
use serde_json::json;

fn request(execution_id: &str) -> ExecutionRequest {
    let thread =
        ThreadKey::new(SessionKey::new("ns", "s1").expect("session"), "main").expect("thread");
    ExecutionRequest::new(
        execution_id,
        thread,
        "turn-1",
        ToolCall::new(
            "call-1",
            "sandbox",
            "shell_execute",
            json!({ "command": "echo hi" }),
        )
        .expect("call"),
    )
    .expect("request")
}

#[tokio::test]
async fn a_restarted_executor_adopts_the_handle() {
    let fake = Arc::new(FakeSandbox::new().script(vec![
        Poll::running(25),
        Poll::exited(0, b"done".to_vec(), Vec::new()),
    ]));
    let store: Arc<dyn BindingStore> = Arc::new(MemoryBindings::new());
    let request = request("exec-1");

    let first = SandboxExecutor::with_shared(Arc::clone(&fake) as Arc<dyn Sandbox>)
        .with_bindings(Arc::clone(&store));
    assert_eq!(
        first.start(&request).await.expect("start").state,
        ExecutionState::Pending
    );
    drop(first);

    // A new process: no in-process handle, only the store and the same sandbox.
    let second = SandboxExecutor::with_shared(Arc::clone(&fake) as Arc<dyn Sandbox>)
        .with_bindings(Arc::clone(&store));
    let answered = second.query(&request).await.expect("query");
    assert_eq!(answered.state, ExecutionState::Succeeded);
    assert_eq!(
        fake.spawned_specs().len(),
        1,
        "the restart adopted the handle instead of spawning again"
    );
}

#[tokio::test]
async fn releasing_twice_kills_once() {
    let fake = Arc::new(FakeSandbox::new().script(vec![Poll::running(10)]));
    let store: Arc<dyn BindingStore> = Arc::new(MemoryBindings::new());
    let executor = SandboxExecutor::with_shared(Arc::clone(&fake) as Arc<dyn Sandbox>)
        .with_bindings(Arc::clone(&store));
    let request = request("exec-1");
    executor.start(&request).await.expect("start");

    assert_eq!(
        executor.cancel(&request).await.expect("cancel").state,
        ExecutionState::Cancelled
    );
    assert_eq!(
        executor.cancel(&request).await.expect("repeat").state,
        ExecutionState::Cancelled
    );
    assert_eq!(
        fake.kills().len(),
        1,
        "a released execution is not killed twice"
    );
}

#[tokio::test]
async fn keepalive_closes_a_releasing_record() {
    let fake = Arc::new(FakeSandbox::new().script(vec![Poll::running(10)]));
    let store = Arc::new(MemoryBindings::new());
    let executor = SandboxExecutor::with_shared(Arc::clone(&fake) as Arc<dyn Sandbox>)
        .with_bindings(Arc::clone(&store) as Arc<dyn BindingStore>);
    let request = request("exec-1");
    executor.start(&request).await.expect("start");

    // The shape a host that died between the barrier and the kill leaves behind.
    let mut record = store.get("exec-1").expect("get").expect("record");
    record.state = BindingState::Releasing;
    store.put(&record).expect("put");

    assert_eq!(executor.keepalive(10).await.expect("keepalive"), 1);
    assert_eq!(
        store.get("exec-1").expect("get").expect("record").state,
        BindingState::Gone
    );
    assert_eq!(fake.kills().len(), 1);
}

#[tokio::test]
async fn live_does_not_return_released_executions() {
    let fake = Arc::new(FakeSandbox::new().script(vec![Poll::running(10)]));
    let store = Arc::new(MemoryBindings::new());
    let executor = SandboxExecutor::with_shared(Arc::clone(&fake) as Arc<dyn Sandbox>)
        .with_bindings(Arc::clone(&store) as Arc<dyn BindingStore>);
    let request = request("exec-1");
    executor.start(&request).await.expect("start");
    assert_eq!(store.live(10).expect("live").len(), 1);

    executor.cancel(&request).await.expect("cancel");
    assert!(store.live(10).expect("live").is_empty());
}

#[test]
fn file_bindings_survive_a_reopen() {
    let root = std::env::temp_dir().join(format!(
        "durable-bindings-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let thread =
        ThreadKey::new(SessionKey::new("ns", "s1").expect("session"), "main").expect("thread");
    let record = BindingRecord {
        execution_id: "exec-1".to_string(),
        thread,
        handle: Handle::new("fake-1"),
        spec: ExecSpec::new("echo"),
        deadline_ms: None,
        state: BindingState::Attached,
        updated_at: 1,
    };
    {
        let store = FileBindings::open(&root).expect("open");
        store.put(&record).expect("put");
    }
    let reopened = FileBindings::open(&root).expect("reopen");
    assert_eq!(reopened.get("exec-1").expect("get"), Some(record));
    std::fs::remove_dir_all(&root).ok();
}
