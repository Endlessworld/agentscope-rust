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
use agentscope_durable_adapter_agentscope::{
    AgentScopeExecutionHost, BackendThreadJournal, STATE_VERSION,
};
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
            primary_binding: ThreadBinding::new("harness-a", "def-1", STATE_VERSION)
                .expect("binding"),
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

/// A finished action: the observer saw the result of a dispatched call.
fn action_ended(seq: u64, call_id: &str) -> JournalEvent {
    let block = ToolResultBlock::builder()
        .id(call_id)
        .name("echo")
        .output(ContentBlock::Text(TextBlock::new("ok")))
        .state(ToolResultState::Success)
        .build();
    JournalEvent::from_payload(
        format!("evt-{seq}"),
        seq,
        1_700_000_000_000 + seq as i64,
        "action/end",
        None,
        None,
        true,
        &json!({
            "toolCallId": call_id,
            "observation": {
                "action_id": format!("action-{call_id}"),
                "tool_call_id": call_id,
                "tool_name": "echo",
                "status": "succeeded",
                "started_at": 0,
                "ended_at": 1,
            },
            "result": block,
        }),
    )
    .expect("event")
}

/// A provider-side call: core never dispatches it, so it owns no attempt to record.
fn server_requested(seq: u64, call_id: &str) -> JournalEvent {
    let mut call = ToolUseBlock::new(call_id, "google_search", Default::default());
    call.metadata
        .insert(ToolUseBlock::METADATA_SERVER_TOOL.to_string(), json!(true));
    JournalEvent::from_payload(
        format!("evt-{seq}"),
        seq,
        1_700_000_000_000 + seq as i64,
        "tool/requested",
        None,
        None,
        true,
        &json!({ "messageId": "msg-1", "toolCallId": call_id, "call": call }),
    )
    .expect("event")
}

/// The inline answer of that provider-side call.
fn server_resulted(seq: u64, call_id: &str) -> JournalEvent {
    let mut block = ToolResultBlock::builder()
        .id(call_id)
        .name("google_search")
        .output(ContentBlock::Text(TextBlock::new("inline")))
        .state(ToolResultState::Success)
        .build();
    block.metadata.insert(
        ToolResultBlock::METADATA_SERVER_TOOL.to_string(),
        json!(true),
    );
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

/// A journaled native state: the baseline a reconciliation needs to restore.
fn checkpointed(seq: u64, state: &AgentState) -> JournalEvent {
    JournalEvent::from_payload(
        format!("evt-{seq}"),
        seq,
        1_700_000_000_000 + seq as i64,
        "state/checkpoint",
        None,
        None,
        true,
        &json!({
            "projectorVersion": 1,
            "stateJson": state.to_json().expect("state json"),
            "reason": "test",
        }),
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

/// A settlement must not be blocked by a durable record that does not exist.
///
/// The journal can hold a `tool/dispatch` the durable turn never recorded (a batch committed by a
/// writer with tool facts off, a record lost with an earlier turn). Its outcome can never arrive,
/// so the settlement records it as interrupted — and that settlement used to fail with
/// `NOT_FOUND: Execution does not exist`, which blocked the session from ever opening again.
#[tokio::test]
async fn a_dispatch_the_durable_turn_never_recorded_still_settles_as_interrupted() {
    let (backend, thread, _receipt, lease) = session().await;
    let raw = BackendThreadJournal::new(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
    )
    .with_tool_facts(false);
    let writer = raw
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("acquire");
    let mut state = AgentState::builder()
        .session_id("s1")
        .user_id("alice")
        .build();
    state.context_mut().push(Msg::user("go"));
    state.context_mut().push(assistant_with_call("c1"));
    raw.commit(
        &writer,
        "batch-1",
        0,
        &[checkpointed(1, &state), dispatched(2, "c1")],
    )
    .await
    .expect("journal batch");
    raw.release(&writer).await.expect("release");
    let id = execution_id(&lease, "c1");
    assert!(
        backend.execution(&thread, &id).await.is_err(),
        "the durable turn keeps no record of the dispatched call"
    );

    let log: Arc<dyn ThreadJournal> = Arc::new(BackendThreadJournal::new(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
    ));
    let settled = agentscope_core::journal::recovery::settle_uncertain_tool_outcomes(
        Arc::clone(&log),
        "outcome unknown",
        "test: settle a call with no durable execution",
    )
    .await
    .expect("a settlement must not be blocked by the missing record");
    assert_eq!(
        settled.into_iter().collect::<Vec<_>>(),
        vec!["c1".to_string()]
    );

    // The journal carries the interrupted settlement, so the session can be opened again.
    let entries = log.read_after(0, 100).await.expect("read");
    let states: Vec<ToolResultState> = entries
        .iter()
        .filter(|event| event.event_type == "tool/result")
        .flat_map(|event| {
            let data = event.data().expect("payload");
            let message = data.get("message").cloned().expect("message");
            let message: Msg = serde_json::from_value(message).expect("tool result message");
            message
                .tool_result_blocks()
                .map(|block| block.state)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(states, vec![ToolResultState::Interrupted]);

    // Nothing is invented on the durable side, and the turn is still workable.
    assert!(
        backend.execution(&thread, &id).await.is_err(),
        "no execution record is fabricated for the unknown call"
    );
    assert_eq!(
        backend
            .turn(&thread, &lease.turn_id)
            .await
            .expect("turn")
            .status,
        TurnStatus::Running
    );
}

/// A provider-side call has no local attempt, so recording its inline result would make the
/// whole batch fail with `Execution has not been dispatched`. Both halves are skipped, and the
/// batch commits.
#[tokio::test]
async fn a_server_tool_result_is_not_recorded_as_a_local_fact() {
    let (backend, journal, thread, lease) = journaled().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("acquire");
    let batch = [server_requested(1, "s1"), server_resulted(2, "s1")];
    journal
        .commit(&writer, "batch-1", 0, &batch)
        .await
        .expect("a provider-side call must not fail the batch");
    assert_eq!(journal.head().await.expect("head").seq, 2);
    assert!(
        backend
            .execution(&thread, &execution_id(&lease, "s1"))
            .await
            .is_err(),
        "a provider-side call owns no durable record"
    );
}

/// A result newer than the checkpoint must answer its call after a restart.
///
/// Regression: a turn that failed before it could commit its `tool/result` (the batch was
/// rejected) left the checkpoint showing the call as unanswered while its `action/end` was
/// already committed. Restoring only the checkpoint re-dispatched that call under the new
/// turn's id, and the durable record rejected it with `NOT_FOUND: Execution does not exist`.
#[tokio::test]
async fn a_result_newer_than_the_checkpoint_is_not_dispatched_again() {
    let (backend, journal, thread, lease) = journaled().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(60))
        .await
        .expect("acquire");

    let mut state = AgentState::builder()
        .session_id("s1")
        .user_id("alice")
        .build();
    state.context_mut().push(Msg::user("go"));
    state.context_mut().push(assistant_with_call("c1"));

    // The checkpoint predates the call, and the action finished without a checkpoint after it.
    journal
        .commit(
            &writer,
            "batch-1",
            0,
            &[
                requested(1, "c1"),
                checkpointed(2, &state),
                dispatched(3, "c1"),
                action_ended(4, "c1"),
            ],
        )
        .await
        .expect("journal batch");

    // Turn A failed before it could answer the call, and the caller submits a new request.
    backend
        .record_failure(
            &lease,
            Failure::new(false, "the turn failed", RecoveryPolicy::defaults()),
        )
        .await
        .expect("fail turn A");
    backend
        .submit(&thread, "req-2", json!({ "text": "again" }))
        .await
        .expect("submit turn B");
    let lease_b = backend
        .claim(&thread, "owner-b", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("turn B is claimable");

    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease_b.clone(),
        "run-2",
    )
    .await
    .expect("host");

    let mut restored = host.state().expect("checkpoint restored").clone();
    assert!(
        restored
            .context()
            .iter()
            .flat_map(|message| message.tool_result_blocks())
            .any(|result| result.id.as_deref() == Some("c1")),
        "the committed action result must answer the pending call: {restored:?}"
    );

    // Acting must not register the call on the new turn: its durable execution belongs to turn
    // A, and dispatching it as `turn-B:c1` would fail with `NOT_FOUND`.
    host.before_boundary(
        ExecutionBoundary::Acting,
        &RuntimeContext::new(),
        &mut restored,
    )
    .await
    .expect("acting boundary");
    assert!(
        backend
            .frame(&lease_b)
            .await
            .expect("frame")
            .tools
            .is_empty(),
        "a settled call must not be dispatched again"
    );
}
