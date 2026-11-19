//! Contract tests for the invariants a durable backend must never relax.

use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::*;
use agentscope_durable::{ErrorCode, SessionBackend};
use serde_json::{json, Value};

fn binding() -> ThreadBinding {
    ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding")
}

/// The store every contract test runs against.
///
/// `DURABLE_CONTRACT_STORE=sqlite` runs the same assertions against a SQLite database instead of
/// the in-memory map, which is how a new `RecordStore` proves it keeps every invariant.
fn contract_backend() -> TxnBackend {
    match std::env::var("DURABLE_CONTRACT_STORE").as_deref() {
        Ok("sqlite") => {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agentscope-sqlite-contract-{}-{id}.db",
                std::process::id()
            ));
            let store = agentscope_durable_sqlite::SqliteStore::open(path).expect("sqlite store");
            TxnBackend::with_store(std::sync::Arc::new(store))
        }
        _ => TxnBackend::in_memory(),
    }
}

async fn created() -> (TxnBackend, ThreadKey) {
    let backend = contract_backend();
    let key = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: binding(),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    (backend, thread)
}

async fn running() -> (TxnBackend, ThreadKey, TurnReceipt, Lease) {
    let (backend, thread) = created().await;
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "owner-a", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("a claimable turn");
    (backend, thread, receipt, lease)
}

#[tokio::test]
async fn a_turn_reaches_a_terminal_state_through_a_fenced_commit() {
    let (backend, thread, receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.turn.status, TurnStatus::Running);

    let mut commit = Commit::new("c1", frame.turn.revision, TurnStatus::Succeeded);
    commit.output = json!({ "reply": "ok" });
    let done = backend
        .commit(&lease, commit.clone())
        .await
        .expect("commit");
    assert_eq!(done.revision, 1);

    // A replayed commit answers with the original receipt even though the lease is gone.
    let replay = backend.commit(&lease, commit).await.expect("replay");
    assert_eq!(replay, done);

    let view = backend
        .turn(&thread, &receipt.turn_id)
        .await
        .expect("turn view");
    assert_eq!(view.status, TurnStatus::Succeeded);
    assert_eq!(view.output, json!({ "reply": "ok" }));

    // A settled turn owns nothing any more.
    let fenced = backend.release(&lease, Duration::ZERO).await;
    assert_eq!(fenced.expect_err("fenced").code(), ErrorCode::Fenced);
}

#[tokio::test]
async fn a_changed_input_for_the_same_request_is_a_conflict() {
    let (backend, thread) = created().await;
    let first = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let again = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("idempotent submit");
    assert_eq!(first.turn_id, again.turn_id);

    let conflict = backend
        .submit(&thread, "req-1", json!({ "text": "other" }))
        .await;
    assert_eq!(conflict.expect_err("conflict").code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn a_stale_revision_or_a_stale_epoch_cannot_commit() {
    let (backend, thread, _receipt, lease) = running().await;
    let mut stale = Commit::new("c-stale", 7, TurnStatus::Running);
    stale.output = json!({ "partial": true });
    let conflict = backend.commit(&lease, stale).await;
    assert_eq!(conflict.expect_err("revision").code(), ErrorCode::Conflict);

    backend
        .release(&lease, Duration::ZERO)
        .await
        .expect("release");
    let takeover = backend
        .claim(&thread, "owner-b", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("a claimable turn");
    assert!(takeover.epoch > lease.epoch);

    let mut commit = Commit::new("c-epoch", 0, TurnStatus::Running);
    commit.output = json!({});
    let fenced = backend.commit(&lease, commit).await;
    assert_eq!(fenced.expect_err("fenced").code(), ErrorCode::Fenced);
}

#[tokio::test]
async fn a_waiting_turn_is_only_ready_after_its_signal_arrives() {
    let (backend, thread, receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let mut commit = Commit::new("c-wait", frame.turn.revision, TurnStatus::Waiting);
    commit.waiting = Some(WaitCondition::new("approval", 0).expect("wait condition"));
    backend.commit(&lease, commit).await.expect("wait");

    let selector = HarnessSelector::all("harness-a").expect("selector");
    assert!(backend
        .ready(&[selector.clone()], 10)
        .await
        .expect("ready")
        .is_empty());

    backend
        .signal(
            &thread,
            &receipt.turn_id,
            Signal::new("sig-1", "approval", json!({ "ok": true })).expect("signal"),
        )
        .await
        .expect("signal");

    let ready = backend.ready(&[selector], 10).await.expect("ready");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].key.thread_id, "main");
}

/// Two sessions of one harness each keep their own readiness record.
///
/// The queue is per harness and the work id used to be the bare thread id, so two sessions that
/// both call their thread `main` wrote the same record: the later write replaced the earlier and
/// the replaced session was scheduled nowhere. `submit` kept succeeding on it, but no tick ever
/// found the thread again — a silent stall, not an error.
#[tokio::test]
async fn sessions_of_one_harness_each_keep_their_readiness_record() {
    let backend = contract_backend();
    let selector = HarnessSelector::all("harness-a").expect("selector");
    for session in ["s1", "s2"] {
        let key = SessionKey::new("ns", session).expect("session key");
        backend
            .create(SessionSpec {
                key: key.clone(),
                primary_binding: binding(),
                metadata: Value::Null,
            })
            .await
            .expect("create session");
        let thread = ThreadKey::new(key, "main").expect("thread key");
        backend
            .submit(&thread, "req-1", json!({ "text": "hi" }))
            .await
            .expect("submit");
    }

    let ready = backend.ready(&[selector], 10).await.expect("ready");
    let mut sessions: Vec<&str> = ready
        .iter()
        .map(|spec| spec.key.session.session_id.as_str())
        .collect();
    sessions.sort_unstable();
    assert_eq!(sessions, vec!["s1", "s2"]);
}

#[tokio::test]
async fn an_identical_checkpoint_does_not_create_a_new_position() {
    let (backend, thread, _receipt, lease) = running().await;
    let mut first = Commit::new("c1", 0, TurnStatus::Running);
    first.checkpoint = json!({ "messages": [], "iteration": 1 });
    backend
        .commit(&lease, first)
        .await
        .expect("first checkpoint");

    let mut second = Commit::new("c2", 1, TurnStatus::Running);
    second.checkpoint = json!({ "messages": [], "iteration": 1 });
    backend
        .commit(&lease, second)
        .await
        .expect("second checkpoint");

    let positions = backend
        .checkpoints(&thread, 0, 10)
        .await
        .expect("positions");
    assert_eq!(positions, vec![1]);
}

#[tokio::test]
async fn an_unsettled_tool_blocks_a_terminal_commit_until_it_is_applied() {
    let (backend, thread, receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let call = ToolCall::new("call-1", "shell", "run", json!({ "cmd": "ls" })).expect("call");
    let mut intent = Commit::new("c-intent", frame.turn.revision, TurnStatus::Running);
    intent.tools = vec![call];
    backend.commit(&lease, intent).await.expect("tool intent");

    let frame = backend.frame(&lease).await.expect("frame");
    let execution_id = frame.tools[0].request.execution_id.clone();

    let unsettled = Commit::new("c-settled", 1, TurnStatus::Succeeded);
    let rejected = backend.commit(&lease, unsettled).await;
    assert_eq!(rejected.expect_err("unsettled").code(), ErrorCode::Invalid);

    let before = backend
        .begin_dispatch(&lease, &execution_id)
        .await
        .expect("dispatch");
    assert!(!before.dispatch_attempted);
    let after = backend
        .begin_dispatch(&lease, &execution_id)
        .await
        .expect("dispatch again");
    assert!(after.dispatch_attempted);

    backend
        .accept_result(
            &thread,
            &execution_id,
            ExecutionResult::succeeded(json!({ "out": 1 })),
        )
        .await
        .expect("result");

    let unapplied = Commit::new("c-unapplied", 1, TurnStatus::Succeeded);
    let rejected = backend.commit(&lease, unapplied).await;
    assert_eq!(rejected.expect_err("unapplied").code(), ErrorCode::Invalid);

    let mut terminal = Commit::new("c-applied", 1, TurnStatus::Succeeded);
    terminal.checkpoint = json!({ "messages": [], "iteration": 2 });
    terminal.applied_results = vec![AppliedResult {
        execution_id: execution_id.clone(),
        applied_at: RunStepRef::new("run-1", None).expect("origin"),
    }];
    let done = backend.commit(&lease, terminal).await.expect("terminal");
    assert_eq!(done.revision, 2);

    let execution = backend
        .execution(&thread, &execution_id)
        .await
        .expect("execution");
    assert!(execution.applied);
    let view = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(view.status, TurnStatus::Succeeded);
}

/// A failure nothing may retry has to end the turn.
///
/// Leaving it `waiting` parked it forever: a waiting turn is re-claimed as soon as its
/// `next_attempt_at` passes, so a non-retryable failure failed again on every pass and no writer
/// ever settled it — the client then saw neither a result nor an error.
#[tokio::test]
async fn an_unrecoverable_failure_ends_the_turn_instead_of_waiting_forever() {
    let (backend, thread, receipt, lease) = running().await;
    let failure = Failure::new(
        false,
        "journal: explicit reconciliation required for unknown tool outcomes",
        RecoveryPolicy::defaults(),
    );
    let recovery = backend
        .record_failure(&lease, failure.clone())
        .await
        .expect("record");
    assert!(!recovery.retry_allowed());

    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(turn.status, TurnStatus::Failed);
    assert_eq!(turn.error.as_deref(), Some(recovery.message.as_str()));

    // A restarted process replays the same report: the stored record answers, and the turn has to
    // stay settled instead of going back to `waiting`.
    let replay = backend
        .record_failure(&lease, failure)
        .await
        .expect("replay");
    assert_eq!(replay, recovery);
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(turn.status, TurnStatus::Failed);
}

/// Settling a dispatched call whose result never arrived records it as unknown: the effect may
/// or may not have happened.
///
/// Such an outcome is never applied and can never be verified, so waiting for it would park the
/// turn forever while reporting success would claim work nobody can vouch for. It may only be
/// accepted explicitly, and then the turn ends failed.
#[tokio::test]
async fn an_unknown_tool_outcome_allows_failure_but_never_success() {
    let (backend, thread, receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let call = ToolCall::new("call-1", "local", "write", json!({ "path": "a" })).expect("call");
    let mut intent = Commit::new("c-intent", frame.turn.revision, TurnStatus::Running);
    intent.tools = vec![call];
    backend.commit(&lease, intent).await.expect("tool intent");
    let execution_id = backend.frame(&lease).await.expect("frame").tools[0]
        .request
        .execution_id
        .clone();

    backend
        .begin_dispatch(&lease, &execution_id)
        .await
        .expect("dispatch");
    backend
        .accept_result(
            &thread,
            &execution_id,
            ExecutionResult::unknown("the run ended before its result was recorded"),
        )
        .await
        .expect("unknown result");

    // The effect is settled (nothing is in flight) but not applied, so success is refused.
    let revision = backend.frame(&lease).await.expect("frame").turn.revision;
    let succeeded = Commit::new("c-succeeded", revision, TurnStatus::Succeeded);
    let refused = backend.commit(&lease, succeeded).await;
    let refused = refused.expect_err("an unknown outcome must not succeed");
    assert_eq!(refused.code(), ErrorCode::Invalid);
    assert!(
        refused.message().contains("unknown"),
        "the refusal names the unknown outcome: {}",
        refused.message()
    );

    // Failing is how an unknown outcome is accepted; the turn stops instead of being retried.
    let failed = Commit::new("c-failed", revision, TurnStatus::Failed);
    backend
        .commit(&lease, failed)
        .await
        .expect("an accepted unknown effect can fail the turn");
    let view = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(view.status, TurnStatus::Failed);
}

/// An answer is a signal: one record per id, and none once the turn has ended.
///
/// `Respond` used to write the signal itself, bypassing the acceptance rules every other signal
/// goes through: a repeated answer appended a second event under the same id (rejected as
/// `Event identity changed`), and an answer for a settled turn was accepted.
#[tokio::test]
async fn an_answer_is_recorded_once_and_not_after_the_turn_ended() {
    let (backend, thread, receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let mut wait = Commit::new("c-wait", frame.turn.revision, TurnStatus::Waiting);
    wait.waiting = Some(WaitCondition::new("question-1", 0).expect("wait condition"));
    backend.commit(&lease, wait).await.expect("wait");

    let respond = ControlCommand {
        turn_id: receipt.turn_id.clone(),
        request_id: "answer-1".to_string(),
        kind: ControlKind::Respond,
        target: Some("question-1".to_string()),
        value: json!({ "ok": true }),
        expected_revision: 0,
    };
    backend
        .control(&thread, respond.clone())
        .await
        .expect("first answer is accepted");
    backend.control(&thread, respond).await.expect("replay");

    // A waiting turn has no owner, so it is resumed the way a host resumes it: by claiming it
    // again once the answered wait condition makes it due.
    let lease = backend
        .claim(&thread, "owner-b", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("the answered turn is due");
    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.signals.len(), 1, "the answer is one signal");

    let revision = frame.turn.revision;
    let terminal = Commit::new("c-done", revision, TurnStatus::Cancelled);
    backend.commit(&lease, terminal).await.expect("cancel");

    let late = ControlCommand {
        turn_id: receipt.turn_id.clone(),
        request_id: "answer-2".to_string(),
        kind: ControlKind::Respond,
        target: Some("question-1".to_string()),
        value: json!({ "ok": false }),
        expected_revision: 0,
    };
    let refused = backend.control(&thread, late).await;
    assert_eq!(refused.expect_err("terminal").code(), ErrorCode::Conflict);
}

/// A call id may be 256 characters, so the execution id derived from it is longer than one leaf
/// identity: validating it as if it were a leaf rejected a legal tool call as `INVALID`.
#[tokio::test]
async fn a_call_with_the_longest_allowed_id_can_be_committed() {
    let (backend, thread, receipt, lease) = running().await;
    let call_id = "c".repeat(256);
    let call = ToolCall::new(call_id.clone(), "local", "write", json!({})).expect("legal call");
    let revision = backend.frame(&lease).await.expect("frame").turn.revision;
    let mut intent = Commit::new("c-long", revision, TurnStatus::Running);
    intent.tools = vec![call];
    backend
        .commit(&lease, intent)
        .await
        .expect("long call intent");

    let execution_id = backend.frame(&lease).await.expect("frame").tools[0]
        .request
        .execution_id
        .clone();
    assert_eq!(execution_id, format!("{}:{call_id}", receipt.turn_id));
    assert!(execution_id.chars().count() > 256);
    backend
        .begin_dispatch(&lease, &execution_id)
        .await
        .expect("dispatch a long id");
    let execution = backend
        .execution(&thread, &execution_id)
        .await
        .expect("execution");
    assert!(execution.dispatch_attempted);
}
