//! Tool facts are derived from native journal batches and committed with them.
//!
//! The three states are the point: an intent that was dispatched but never finished must be
//! visible as such after a restart, and no path may invent an `applied` mark.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::agent::{AgentExecutionHost, ExecutionBoundary, RuntimeContext};
use agentscope_core::journal::{JournalEvent, ThreadJournal};
use agentscope_core::message::{
    ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolResultState, ToolUseBlock,
};
use agentscope_core::state::AgentState;
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::*;
use agentscope_durable::{ErrorCode, SessionBackend};
use agentscope_durable_adapter_agentscope::{AgentScopeExecutionHost, BackendThreadJournal};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn session() -> (Arc<TxnBackend>, ThreadKey, TurnReceipt, Lease) {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "owner-a", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("a claimable turn");
    (backend, thread, receipt, lease)
}

/// A thread whose journal is driven by the bridge, plus the lease it acquired.
async fn journaled() -> (Arc<TxnBackend>, BackendThreadJournal, ThreadKey, Lease) {
    let (backend, thread, _receipt, _lease) = session().await;
    let journal = BackendThreadJournal::new(backend.clone(), thread.clone());
    journal
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("journal lease");
    // Same owner, same turn: the claim returns the existing lease instead of bumping the epoch.
    let lease = backend
        .claim(&thread, "owner-a", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("the acquired lease");
    (backend, journal, thread, lease)
}

fn tool_use(call_id: &str) -> Value {
    let input = json!({ "text": "hi" })
        .as_object()
        .cloned()
        .unwrap_or_default();
    serde_json::to_value(ToolUseBlock::new(call_id, "echo", input)).expect("tool use")
}

fn requested(seq: u64, call_id: &str) -> JournalEvent {
    JournalEvent::from_payload(
        format!("evt-{seq}"),
        seq,
        1_700_000_000_000 + seq as i64,
        "tool/requested",
        None,
        None,
        true,
        &json!({ "messageId": "msg-1", "toolCallId": call_id, "call": tool_use(call_id) }),
    )
    .expect("event")
}

fn dispatched(seq: u64, call_id: &str) -> JournalEvent {
    JournalEvent::from_payload(
        format!("evt-{seq}"),
        seq,
        1_700_000_000_000 + seq as i64,
        "tool/dispatch",
        None,
        None,
        true,
        &json!({ "toolCallId": call_id, "toolName": "echo" }),
    )
    .expect("event")
}

fn resulted(seq: u64, call_id: &str, state: ToolResultState) -> JournalEvent {
    let block = ToolResultBlock::builder()
        .id(call_id)
        .name("echo")
        .output(ContentBlock::Text(TextBlock::new("ok")))
        .state(state)
        .build();
    let message = Msg::builder()
        .role(MsgRole::Tool)
        .content(vec![ContentBlock::ToolResult(block)])
        .build();
    JournalEvent::from_payload(
        format!("evt-{seq}"),
        seq,
        1_700_000_000_000 + seq as i64,
        "tool/result",
        None,
        None,
        true,
        &json!({ "message": message }),
    )
    .expect("event")
}

fn assistant_with_call(call_id: &str) -> Msg {
    let input = json!({ "text": "hi" })
        .as_object()
        .cloned()
        .unwrap_or_default();
    Msg::builder()
        .role(MsgRole::Assistant)
        .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            call_id, "echo", input,
        ))])
        .build()
}

fn execution_id(lease: &Lease, call_id: &str) -> String {
    format!("{}:{call_id}", lease.turn_id)
}

async fn count_events(backend: &TxnBackend, thread: &ThreadKey, event_type: &str) -> usize {
    backend
        .events(&thread.session, None, 1000)
        .await
        .expect("events")
        .events
        .iter()
        .filter(|event| event.event_type == event_type)
        .count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dispatched_tool_without_a_result_survives_a_restart() {
    let (backend, thread, _receipt, lease) = session().await;
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("host");
    let mut state = AgentState::builder()
        .session_id("s1")
        .user_id("alice")
        .build();
    state.context_mut().push(Msg::user("go"));
    state.context_mut().push(assistant_with_call("c1"));

    host.before_boundary(
        ExecutionBoundary::Acting,
        &RuntimeContext::new(),
        &mut state,
    )
    .await
    .expect("acting boundary");

    // The call is logged before it runs: the process may now die with the effect unknown.
    let id = execution_id(&lease, "c1");
    let record = backend.execution(&thread, &id).await.expect("execution");
    assert!(record.dispatch_attempted, "dispatched");
    assert!(record.result.is_none(), "no result was observed");
    assert!(!record.applied, "nothing was applied");

    // A restart re-observes the same fact; it neither dispatches again nor invents a result.
    let reopened = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-2",
    )
    .await
    .expect("host");
    reopened
        .before_boundary(
            ExecutionBoundary::Acting,
            &RuntimeContext::new(),
            &mut state,
        )
        .await
        .expect("resumed acting boundary");

    assert_eq!(
        count_events(&backend, &thread, "tool.dispatching").await,
        1,
        "the dispatch fact exists once"
    );
    let record = backend.execution(&thread, &id).await.expect("execution");
    assert!(record.dispatch_attempted && record.result.is_none());
    assert_eq!(record.result.as_ref().map(|result| result.state), None);
    assert!(!record.applied);
}

#[tokio::test]
async fn replaying_a_batch_does_not_duplicate_tool_facts() {
    let (backend, journal, thread, lease) = journaled().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("acquire");
    let batch = [
        requested(1, "c1"),
        dispatched(2, "c1"),
        resulted(3, "c1", ToolResultState::Success),
    ];
    journal
        .commit(&writer, "batch-1", 0, &batch)
        .await
        .expect("commit");
    let before = backend
        .execution(&thread, &execution_id(&lease, "c1"))
        .await
        .expect("execution");
    let head = journal.head().await.expect("head");

    journal
        .commit(&writer, "batch-1", 0, &batch)
        .await
        .expect("replay");

    let after = backend
        .execution(&thread, &execution_id(&lease, "c1"))
        .await
        .expect("execution");
    assert_eq!(after.request, before.request);
    assert_eq!(after.result, before.result);
    assert_eq!(
        after.applied_at, before.applied_at,
        "appliedAt is unchanged"
    );
    assert!(!after.applied);
    assert_eq!(journal.head().await.expect("head").seq, head.seq);
    assert_eq!(
        count_events(&backend, &thread, "tool.requested").await,
        1,
        "one intent"
    );
    assert_eq!(
        count_events(&backend, &thread, "tool.result.accepted").await,
        1,
        "one result"
    );
}

#[tokio::test]
async fn an_unapplied_tool_blocks_a_succeeded_turn() {
    let (backend, journal, thread, lease) = journaled().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("acquire");
    journal
        .commit(
            &writer,
            "batch-1",
            0,
            &[
                requested(1, "c1"),
                dispatched(2, "c1"),
                resulted(3, "c1", ToolResultState::Success),
            ],
        )
        .await
        .expect("commit");

    let revision = backend
        .turn(&thread, &lease.turn_id)
        .await
        .expect("turn")
        .revision;
    let mut success = Commit::new("c-success", revision, TurnStatus::Succeeded);
    success.checkpoint = json!({ "messages": [], "iteration": 2 });
    let rejected = backend
        .commit(&lease, success)
        .await
        .expect_err("a tool result that was never applied cannot settle the turn");
    assert_eq!(rejected.code(), ErrorCode::Invalid);

    // Applying the result and settling are two facts, and only then does the turn succeed.
    let mut applied = Commit::new("c-applied", revision, TurnStatus::Running);
    applied.checkpoint = json!({ "messages": [], "iteration": 2 });
    applied.applied_results = vec![AppliedResult {
        execution_id: execution_id(&lease, "c1"),
        applied_at: RunStepRef::new("run-1", None).expect("origin"),
    }];
    backend.commit(&lease, applied).await.expect("apply result");

    let revision = backend
        .turn(&thread, &lease.turn_id)
        .await
        .expect("turn")
        .revision;
    let mut success = Commit::new("c-success-2", revision, TurnStatus::Succeeded);
    success.checkpoint = json!({ "messages": [], "iteration": 2 });
    backend.commit(&lease, success).await.expect("terminal");
    let turn = backend.turn(&thread, &lease.turn_id).await.expect("turn");
    assert_eq!(turn.status, TurnStatus::Succeeded);
}

#[tokio::test]
async fn a_not_found_result_is_never_recorded() {
    let (backend, journal, thread, lease) = journaled().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("acquire");
    journal
        .commit(
            &writer,
            "batch-1",
            0,
            &[requested(1, "c1"), dispatched(2, "c1")],
        )
        .await
        .expect("commit");

    let id = execution_id(&lease, "c1");
    let rejected = backend
        .accept_result(&thread, &id, ExecutionResult::missing())
        .await
        .expect_err("NOT_FOUND is not an outcome");
    assert_eq!(rejected.code(), ErrorCode::Invalid);

    // Rejected before the transaction opens: the commit never lands.
    let revision = backend
        .turn(&thread, &lease.turn_id)
        .await
        .expect("turn")
        .revision;
    let mut commit = Commit::new("c-bad", revision, TurnStatus::Running);
    commit.results = vec![ToolResultRecord::new(
        id.clone(),
        ExecutionResult::missing(),
    )];
    let rejected = backend
        .commit(&lease, commit)
        .await
        .expect_err("NOT_FOUND is not an outcome");
    assert_eq!(rejected.code(), ErrorCode::Invalid);

    let record = backend.execution(&thread, &id).await.expect("execution");
    assert!(record.result.is_none());
}

#[tokio::test]
async fn a_rejected_tool_fact_rolls_the_journal_batch_back() {
    let (_backend, journal, _thread, _lease) = journaled().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("acquire");

    // A result for a call nobody requested or dispatched: recording it fails, and because the
    // journal entries ride the same transaction, none of them lands either.
    let failed = journal
        .commit(
            &writer,
            "batch-1",
            0,
            &[resulted(1, "c-ghost", ToolResultState::Success)],
        )
        .await;
    assert!(failed.is_err(), "an unknown execution must fail the batch");

    assert_eq!(journal.head().await.expect("head").seq, 0);
    assert!(journal.read_after(0, 10).await.expect("read").is_empty());
}
