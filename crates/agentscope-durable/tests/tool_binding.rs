//! A tool call's executor and binding are checked before they can become durable facts.

use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::{
    Commit, Lease, SessionKey, SessionSpec, ThreadBinding, ThreadKey, ToolCall, TurnStatus,
};
use agentscope_durable::ErrorCode;
use agentscope_durable::SessionBackend;
use serde_json::json;

async fn ready_turn() -> (TxnBackend, Lease) {
    let backend = TxnBackend::in_memory();
    let session = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: session.clone(),
            primary_binding: ThreadBinding::new("harness", "def-1", "state-1").expect("binding"),
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
    (backend, lease)
}

fn intent(revision: u64, call: ToolCall) -> Commit {
    let mut commit = Commit::new("intent", revision, TurnStatus::Running);
    commit.tools = vec![call];
    commit
}

#[tokio::test]
async fn a_call_without_an_executor_is_refused() {
    let (backend, lease) = ready_turn().await;
    let frame = backend.frame(&lease).await.expect("frame");
    // Decoded directly: the constructor would reject it before the backend ever saw it.
    let call: ToolCall = serde_json::from_value(json!({
        "callId": "call-1",
        "executor": "",
        "name": "shell",
        "arguments": {}
    }))
    .expect("decode");

    let error = backend
        .commit(&lease, intent(frame.turn.revision, call))
        .await
        .expect_err("an empty executor is not routable");
    assert_eq!(error.code(), ErrorCode::Invalid);
}

#[tokio::test]
async fn a_call_with_a_malformed_binding_digest_is_refused() {
    let (backend, lease) = ready_turn().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let call: ToolCall = serde_json::from_value(json!({
        "callId": "call-1",
        "executor": "remote",
        "name": "shell",
        "arguments": {},
        "binding": "not-a-digest"
    }))
    .expect("decode");

    let error = backend
        .commit(&lease, intent(frame.turn.revision, call))
        .await
        .expect_err("a non 64-hex binding is not a catalog digest");
    assert_eq!(error.code(), ErrorCode::Invalid);
}

#[tokio::test]
async fn a_bound_call_keeps_its_executor_and_digest() {
    let (backend, lease) = ready_turn().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let digest = "a".repeat(64);
    let call = ToolCall::new("call-1", "remote", "shell", json!({}))
        .expect("call")
        .with_binding(digest.clone())
        .expect("binding");
    backend
        .commit(&lease, intent(frame.turn.revision, call))
        .await
        .expect("commit");

    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.tools[0].request.call.executor, "remote");
    assert_eq!(
        frame.tools[0].request.call.binding.as_deref(),
        Some(digest.as_str())
    );
}
