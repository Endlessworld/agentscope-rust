//! Forking a checkpoint and recording a delegation are both idempotent facts of the parent.

use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::{
    Commit, DelegationTarget, Lease, SessionKey, SessionSpec, ThreadBinding, ThreadKey, ThreadSpec,
    ToolCall, TurnStatus,
};
use agentscope_durable::{child_thread_id, fork_thread, DelegationExecutor, SessionBackend};
use serde_json::json;

async fn parent_turn() -> (TxnBackend, Lease, ThreadKey, ThreadBinding) {
    let backend = TxnBackend::in_memory();
    let session = SessionKey::new("ns", "s1").expect("session key");
    let binding = ThreadBinding::new("harness", "def-1", "state-1").expect("binding");
    backend
        .create(SessionSpec {
            key: session.clone(),
            primary_binding: binding.clone(),
            metadata: json!({}),
        })
        .await
        .expect("create");
    let thread = ThreadKey::new(session, "main").expect("thread key");
    backend
        .submit(&thread, "req-1", json!({}))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "owner", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("a claimable turn");
    backend.prepare_run(&lease, "req-1").await.expect("run");
    (backend, lease, thread, binding)
}

async fn commit_checkpoint(backend: &TxnBackend, lease: &Lease) {
    let frame = backend.frame(lease).await.expect("frame");
    let mut commit = Commit::new("checkpoint", frame.turn.revision, TurnStatus::Running);
    commit.checkpoint = json!({ "messages": [] });
    backend.commit(lease, commit).await.expect("checkpoint");
}

#[tokio::test]
async fn forking_installs_the_parent_checkpoint_once() {
    let (backend, lease, thread, binding) = parent_turn().await;
    commit_checkpoint(&backend, &lease).await;

    let request_id = "fork-1";
    let child_key = ThreadKey::new(thread.session.clone(), child_thread_id(&thread, request_id))
        .expect("child key");
    let spec = ThreadSpec {
        key: child_key.clone(),
        binding,
    };
    let first = fork_thread(&backend, &thread, request_id, spec.clone())
        .await
        .expect("fork");
    assert_eq!(first.definition.key, child_key);
    let state = backend.thread_state(&child_key).await.expect("child state");
    assert!(
        state.checkpoint_metadata.is_some(),
        "the baseline is installed"
    );

    let second = fork_thread(&backend, &thread, request_id, spec)
        .await
        .expect("second fork");
    assert_eq!(second, first, "the same request answers the same child");
}

#[tokio::test]
async fn a_delegation_links_the_parent_call_to_one_child() {
    let (backend, lease, thread, binding) = parent_turn().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let call = ToolCall::new("call-1", "local", "delegate", json!({})).expect("call");
    let mut commit = Commit::new("intent", frame.turn.revision, TurnStatus::Running);
    commit.tools = vec![call];
    backend.commit(&lease, commit).await.expect("intent");

    let execution_id = format!("{}:call-1", lease.turn_id);
    let child_key = ThreadKey::new(thread.session.clone(), "child-1").expect("child key");
    let target = DelegationTarget {
        child: ThreadSpec {
            key: child_key.clone(),
            binding,
        },
        input: json!({ "task": "do it" }),
    };

    let executor = DelegationExecutor::new(&backend, &lease);
    let view = executor
        .delegate(&execution_id, "delegate-1", target.clone())
        .await
        .expect("delegate");
    assert_eq!(view.parent.execution_id, execution_id);
    assert_eq!(view.child.as_ref().expect("child").thread, child_key);

    let again = executor
        .delegate(&execution_id, "delegate-1", target)
        .await
        .expect("repeat");
    assert_eq!(again, view, "one tool call delegates once");

    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.tools[0].delegation, view.child);
}
