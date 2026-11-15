//! The host-facing surface: signal delivery, suspend vs cancel, settle and the session façade.
//!
//! Everything here goes through [`DurableSession`], which is only a composition: the assertions
//! are about the backend's facts, not about a second implementation of them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::types::*;
use agentscope_durable::{
    Commit, DurableRuntime, DurableSession, ErrorCode, SessionBackend, TickOutcome,
};
use async_trait::async_trait;
use serde_json::{json, Value};

const HARNESS: &str = "stub";
const TTL: Duration = Duration::from_secs(60);

fn binding() -> ThreadBinding {
    ThreadBinding::new(HARNESS, "def-1", "state-1").expect("binding")
}

async fn fixture(adapter: Arc<dyn HarnessAdapter>) -> (Arc<TxnBackend>, DurableSession, ThreadKey) {
    let backend = Arc::new(TxnBackend::in_memory());
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
    let runtime = DurableRuntime::new(Arc::clone(&backend) as Arc<dyn SessionBackend>, "owner-a")
        .with_adapter(HARNESS, adapter);
    let session = DurableSession::new(runtime, thread.clone());
    (backend, session, thread)
}

fn capabilities() -> AdapterCapabilities {
    AdapterCapabilities {
        state_version: "state-1".to_string(),
        recovery_boundary: true,
        event_delivery: false,
    }
}

/// Commits nothing: the turn stays running, which is what a host that abandons it sees.
struct Idle;

#[async_trait]
impl HarnessAdapter for Idle {
    fn capabilities(&self) -> AdapterCapabilities {
        capabilities()
    }

    async fn execute(&self, _ctx: &HarnessContext) -> agentscope_durable::Result<()> {
        Ok(())
    }
}

/// Records the checkpoint it was given and advances the turn with a new one.
struct Progress {
    seen: Mutex<Vec<Value>>,
}

#[async_trait]
impl HarnessAdapter for Progress {
    fn capabilities(&self) -> AdapterCapabilities {
        capabilities()
    }

    async fn execute(&self, ctx: &HarnessContext) -> agentscope_durable::Result<()> {
        self.seen
            .lock()
            .expect("seen")
            .push(ctx.checkpoint().clone());
        let turn = ctx
            .backend
            .turn(&ctx.lease.thread, &ctx.lease.turn_id)
            .await?;
        let mut commit = Commit::new(
            format!("progress:{}", turn.revision),
            turn.revision,
            TurnStatus::Running,
        );
        commit.checkpoint = json!({ "steps": 1 });
        commit.output = json!({ "progress": 1 });
        ctx.backend.commit(&ctx.lease, commit).await?;
        Ok(())
    }
}

fn idle() -> Arc<dyn HarnessAdapter> {
    Arc::new(Idle)
}

async fn signal_applied_events(backend: &TxnBackend, thread: &ThreadKey) -> usize {
    backend
        .events(&thread.session, None, 100)
        .await
        .expect("events")
        .events
        .iter()
        .filter(|event| event.event_type == "signal.applied")
        .count()
}

// ---------------------------------------------------------------------------
// A: signal delivery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_steer_reaches_the_turn_the_writer_holds() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");

    let command = session
        .steer(&receipt.turn_id, "req-steer", "keep the tests")
        .await
        .expect("steer");
    assert_eq!(command.operation, "STEER");

    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.signals.len(), 1);
    assert_eq!(frame.signals[0].key, "input:steer");
    assert_eq!(frame.signals[0].value, json!("keep the tests"));

    // Only the active turn accepts input, so a queued turn cannot be steered.
    let queued = session
        .submit("req-2", json!({ "text": "next" }))
        .await
        .expect("submit");
    let rejected = session
        .steer(&queued.turn_id, "req-steer-2", "too late")
        .await;
    assert_eq!(rejected.expect_err("conflict").code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn an_inject_lands_on_the_active_turn_under_its_own_key() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");

    let command = session
        .inject(&receipt.turn_id, "req-inject", "also cover resume")
        .await
        .expect("inject");
    assert_eq!(command.operation, "INJECT");
    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.signals[0].key, "input:inject");
    assert_eq!(frame.signals[0].id, "req-inject");
}

#[tokio::test]
async fn an_accepted_signal_wakes_a_waiting_turn() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut waiting = Commit::new("wait", turn.revision, TurnStatus::Waiting);
    waiting.waiting = Some(WaitCondition::new("approval", 0).expect("wait condition"));
    backend.commit(&lease, waiting).await.expect("wait");

    let selector = HarnessSelector::all(HARNESS).expect("selector");
    assert!(backend
        .ready(&[selector.clone()], 10)
        .await
        .expect("ready")
        .is_empty());

    session
        .signal(
            &receipt.turn_id,
            Signal::new("sig-1", "approval", json!({ "ok": true })).expect("signal"),
        )
        .await
        .expect("signal");
    assert_eq!(
        backend.ready(&[selector], 10).await.expect("ready").len(),
        1
    );
}

#[tokio::test]
async fn a_signal_is_accepted_once_and_delivered_once() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");

    let signal = Signal::new("sig-1", "approval", json!({ "ok": true })).expect("signal");
    session
        .signal(&receipt.turn_id, signal.clone())
        .await
        .expect("signal");
    session
        .signal(&receipt.turn_id, signal)
        .await
        .expect("an identical signal is one record");
    let changed = session
        .signal(
            &receipt.turn_id,
            Signal::new("sig-1", "approval", json!({ "ok": false })).expect("signal"),
        )
        .await;
    assert_eq!(changed.expect_err("conflict").code(), ErrorCode::Conflict);

    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.signals.len(), 1, "one signal, one record");

    // Delivery is a commit, not a read: the marker is the only thing that stops a second fold.
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut consume = Commit::new("consume", turn.revision, TurnStatus::Running);
    consume.consumed_signals = vec!["sig-1".to_string()];
    backend.commit(&lease, consume).await.expect("consume");
    assert!(backend
        .frame(&lease)
        .await
        .expect("frame")
        .signals
        .is_empty());
    assert_eq!(signal_applied_events(&backend, &thread).await, 1);

    // A replayed consume adds no further delivery fact.
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut again = Commit::new("consume-2", turn.revision, TurnStatus::Running);
    again.consumed_signals = vec!["sig-1".to_string()];
    backend.commit(&lease, again).await.expect("consume again");
    assert_eq!(signal_applied_events(&backend, &thread).await, 1);

    let unknown = Commit::new("consume-unknown", turn.revision + 1, TurnStatus::Running);
    let mut unknown = unknown;
    unknown.consumed_signals = vec!["ghost".to_string()];
    assert_eq!(
        backend
            .commit(&lease, unknown)
            .await
            .expect_err("unknown signal")
            .code(),
        ErrorCode::Invalid
    );
}

// ---------------------------------------------------------------------------
// B: suspend (resumable) vs cancel (terminal)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_suspended_turn_keeps_its_progress_and_resumes_in_place() {
    let adapter = Arc::new(Progress {
        seen: Mutex::new(Vec::new()),
    });
    let (_backend, session, _thread) =
        fixture(Arc::clone(&adapter) as Arc<dyn HarnessAdapter>).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    assert_eq!(session.tick().await.expect("tick"), TickOutcome::Settled);

    let epoch = session.state().await.expect("state").epoch;
    let suspended = session
        .suspend(&receipt.turn_id, "req-suspend")
        .await
        .expect("suspend");
    assert_eq!(suspended.status, TurnStatus::Paused);
    assert!(!suspended.status.terminal(), "a suspend is resumable");
    assert_eq!(
        suspended.output,
        json!({ "progress": 1 }),
        "the work so far is readable while suspended"
    );

    let resumed = session
        .resume(&receipt.turn_id, "req-resume")
        .await
        .expect("resume");
    assert_eq!(resumed.status, TurnStatus::Running);
    assert_eq!(
        session.state().await.expect("state").epoch,
        epoch,
        "a suspend/resume pair is not a takeover"
    );

    // The continuation starts from the committed checkpoint, not from zero.
    session.tick().await.expect("tick");
    let seen = adapter.seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1], json!({ "steps": 1 }));
}

#[tokio::test]
async fn a_cancelled_turn_cannot_be_resumed() {
    let (_backend, session, _thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    session
        .cancel(&receipt.turn_id, "req-cancel")
        .await
        .expect("cancel");
    let cancelled = session.turn(&receipt.turn_id).await.expect("turn");
    assert_eq!(cancelled.status, TurnStatus::Cancelled);

    let rejected = session.resume(&receipt.turn_id, "req-resume").await;
    assert_eq!(rejected.expect_err("terminal").code(), ErrorCode::Conflict);
    assert_eq!(session.tick().await.expect("tick"), TickOutcome::Idle);
}

#[tokio::test]
async fn neither_a_suspend_nor_a_cancel_lets_a_stale_writer_write() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let stale = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");

    // A live writer only sees the request; it completes the pause when it hands the slot back.
    let requested = session
        .suspend(&receipt.turn_id, "req-suspend")
        .await
        .expect("suspend");
    assert_eq!(requested.status, TurnStatus::Pausing);
    backend.release(&stale, Duration::ZERO).await.expect("ack");
    assert_eq!(
        session.turn(&receipt.turn_id).await.expect("turn").status,
        TurnStatus::Paused
    );

    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut late = Commit::new("late", turn.revision, TurnStatus::Running);
    late.output = json!({ "reply": "too late" });
    assert_eq!(
        backend
            .commit(&stale, late)
            .await
            .expect_err("fenced")
            .code(),
        ErrorCode::Fenced
    );

    session
        .resume(&receipt.turn_id, "req-resume")
        .await
        .expect("resume");
    let fresh = backend
        .claim(&thread, "owner-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");
    assert!(fresh.epoch > stale.epoch, "the fence is a takeover counter");
}

// ---------------------------------------------------------------------------
// C + D: settle and the read views of the façade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settling_applies_the_effects_and_is_idempotent() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");
    backend
        .prepare_run(&lease, "invoke-r1")
        .await
        .expect("run record");

    let call = ToolCall::new("call-1", "shell", "run", json!({ "cmd": "ls" })).expect("call");
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut intent = Commit::new("intent", turn.revision, TurnStatus::Running);
    intent.checkpoint = json!({ "messages": [] });
    intent.tools = vec![call];
    backend.commit(&lease, intent).await.expect("intent");
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
            ExecutionResult::succeeded(json!({ "out": 1 })),
        )
        .await
        .expect("result");

    session
        .settle(
            &lease,
            TurnStatus::Succeeded,
            json!({ "reply": "done" }),
            None,
        )
        .await
        .expect("settle");
    let settled = session.turn(&receipt.turn_id).await.expect("turn");
    assert_eq!(settled.status, TurnStatus::Succeeded);
    assert_eq!(settled.output, json!({ "reply": "done" }));
    assert!(
        backend
            .execution(&thread, &execution_id)
            .await
            .expect("execution")
            .applied,
        "the effect this run produced is marked applied"
    );

    // The same ending again is a no-op; a different one would rewrite a terminal status.
    session
        .settle(
            &lease,
            TurnStatus::Succeeded,
            json!({ "reply": "done" }),
            None,
        )
        .await
        .expect("idempotent settle");
    let rejected = session
        .settle(&lease, TurnStatus::Failed, json!({ "reply": "no" }), None)
        .await;
    assert_eq!(rejected.expect_err("terminal").code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn the_facade_exposes_the_views_a_host_drives() {
    let (_backend, session, _thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    assert_eq!(
        session.admission("req-1").await.expect("admission"),
        Some(receipt.clone())
    );
    assert_eq!(session.tick().await.expect("tick"), TickOutcome::Settled);
    assert_eq!(
        session
            .active()
            .await
            .expect("active")
            .map(|turn| turn.turn_id),
        Some(receipt.turn_id.clone())
    );
    assert_eq!(session.turns(None, 10).await.expect("turns").items.len(), 1);
    let events = session.events(None, 10).await.expect("events");
    assert!(events
        .events
        .iter()
        .any(|event| event.event_type == "turn.started"));
}

// ---------------------------------------------------------------------------
// E: what a session host needs beyond submit and tick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_wakeup_is_an_admission_without_input_and_it_is_idempotent() {
    let (_backend, session, _thread) = fixture(idle()).await;
    let receipt = session.wakeup("req-wake").await.expect("wakeup");
    assert_eq!(receipt.request_id, "req-wake");
    assert_eq!(
        session.admission("req-wake").await.expect("admission"),
        Some(receipt.clone())
    );
    // A retried wakeup reaches the same turn instead of queueing a second one.
    let again = session.wakeup("req-wake").await.expect("retried wakeup");
    assert_eq!(again.turn_id, receipt.turn_id);
    assert_eq!(session.turns(None, 10).await.expect("turns").items.len(), 1);
}

#[tokio::test]
async fn a_waiting_turn_is_answered_by_its_interaction_key() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");
    let frame_turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut blocked = Commit::new("wait", frame_turn.revision, TurnStatus::Waiting);
    blocked.waiting = Some(WaitCondition::new("permission:call-1", 0).expect("wait"));
    backend.commit(&lease, blocked).await.expect("wait");

    let waiting = session.waiting().await.expect("waiting");
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].turn_id, receipt.turn_id.clone());

    // Only the interaction the turn actually waits on can be answered.
    let wrong = session
        .respond(
            &receipt.turn_id,
            "permission:call-2",
            "req-answer-2",
            json!({ "ok": true }),
        )
        .await;
    assert_eq!(wrong.expect_err("wrong key").code(), ErrorCode::Conflict);

    session
        .respond(
            &receipt.turn_id,
            "permission:call-1",
            "req-answer",
            json!({ "ok": true }),
        )
        .await
        .expect("respond");
    // The answer is one signal record, so retrying it adds nothing.
    session
        .respond(
            &receipt.turn_id,
            "permission:call-1",
            "req-answer",
            json!({ "ok": true }),
        )
        .await
        .expect("retried answer");
    // The answer is what makes the turn claimable again...
    let selector = HarnessSelector::all(HARNESS).expect("selector");
    assert_eq!(
        backend.ready(&[selector], 10).await.expect("ready").len(),
        1
    );

    // ...and the next writer sees it as one delivered fact.
    let next = backend
        .claim(&thread, "writer-b", TTL)
        .await
        .expect("claim")
        .expect("the answered turn");
    let frame = backend.frame(&next).await.expect("frame");
    assert_eq!(frame.signals.len(), 1);
    assert_eq!(frame.signals[0].key, "permission:call-1");
    assert_eq!(frame.signals[0].value, json!({ "ok": true }));

    // A settled turn can no longer be answered.
    session
        .settle(&next, TurnStatus::Cancelled, json!({}), None)
        .await
        .expect("settle");
    let late = session
        .respond(
            &receipt.turn_id,
            "permission:call-1",
            "req-answer-late",
            json!({ "ok": false }),
        )
        .await;
    assert_eq!(late.expect_err("terminal").code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn the_task_projection_reads_the_status_the_run_and_the_ending() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");

    // Queued: nothing has worked on it yet, so there is no run to name.
    let queued = session.task(&receipt.turn_id).await.expect("task");
    assert_eq!(queued.status, TurnStatus::Queued);
    assert!(!queued.settled);
    assert_eq!(queued.run_id, None);

    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");
    let run = backend
        .prepare_run(&lease, "invoke-r1")
        .await
        .expect("run record");
    let running = session.task(&receipt.turn_id).await.expect("task");
    assert_eq!(running.status, TurnStatus::Running);
    assert_eq!(running.run_id.as_deref(), Some(run.run_id.as_str()));

    session
        .settle(
            &lease,
            TurnStatus::Failed,
            json!({}),
            Some("provider timed out".to_string()),
        )
        .await
        .expect("settle");
    let failed = session.task(&receipt.turn_id).await.expect("task");
    assert_eq!(failed.status, TurnStatus::Failed);
    assert!(failed.settled);
    assert_eq!(failed.error.as_deref(), Some("provider timed out"));
}

/// The shape core's recorder commits: an envelope with the message as its payload.
fn journal_entry(sequence: u64, event_type: &str, message: &Value, turn_id: &str) -> Value {
    json!({
        "schemaVersion": 2,
        "eventId": format!("e{sequence}"),
        "seq": sequence,
        "occurredAt": 1_700_000_000_000i64,
        "type": event_type,
        "turnId": turn_id,
        "required": true,
        "payloadJson": json!({ "message": message }).to_string(),
    })
}

fn message_json(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "content": [{ "type": "text", "text": text }],
        "timestamp": "2026-01-01 00:00:00.000",
    })
}

#[tokio::test]
async fn history_is_empty_when_the_journal_holds_no_message() {
    // The shape a scope written before message facts existed has: turns, no journal messages.
    // History must answer an empty page, not invent one from the turn outputs.
    let (backend, session, thread) = fixture(idle()).await;
    session
        .submit("req-1", json!({ "text": "one" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");
    session
        .settle(
            &lease,
            TurnStatus::Succeeded,
            json!({ "reply": "one" }),
            None,
        )
        .await
        .expect("settle");

    let history = session.history(None, 10).await.expect("history");
    assert!(history.items.is_empty(), "{:?}", history.items);
    assert_eq!(history.cursor, None);
}

#[tokio::test]
async fn history_rebuilds_the_journal_messages_in_order_and_pages_by_sequence() {
    let (backend, session, thread) = fixture(idle()).await;
    let receipt = session
        .submit("req-1", json!({ "text": "one" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");

    let user = message_json("m-user", "hello");
    let assistant = message_json("m-assistant", "hi");
    let tool = json!({
        "id": "m-tool",
        "content": [{ "type": "tool_result", "id": "call-1", "output": "ok", "state": "SUCCESS" }],
        "timestamp": "2026-01-01 00:00:03.000"
    });
    let revision = session.turn(&receipt.turn_id).await.expect("turn").revision;
    let mut commit = Commit::new("journal-1", revision, TurnStatus::Running);
    commit.journal = Some(JournalAppend {
        expected_position: 0,
        entries: vec![
            journal_entry(1, "message/user", &user, &receipt.turn_id),
            journal_entry(2, "message/assistant", &assistant, &receipt.turn_id),
            journal_entry(3, "tool/result", &tool, &receipt.turn_id),
        ],
    });
    backend
        .commit(&lease, commit)
        .await
        .expect("journal commit");

    let history = session.history(None, 10).await.expect("history");
    let roles: Vec<&str> = history
        .items
        .iter()
        .filter_map(|item| item["role"].as_str())
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "tool"]);
    let ids: Vec<&str> = history
        .items
        .iter()
        .filter_map(|item| item["messageId"].as_str())
        .collect();
    assert_eq!(ids, vec!["m-user", "m-assistant", "m-tool"]);
    assert_eq!(history.items[0]["turnId"], json!(receipt.turn_id));
    assert_eq!(history.items[0]["seq"], json!("1"));

    // One item per page: the cursor is the sequence of the last item handed out, so the next
    // page starts behind it instead of walking the history in front of it again.
    let first = session.history(None, 1).await.expect("first page");
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.cursor.as_deref(), Some("1"));
    let second = session
        .history(first.cursor.as_deref(), 1)
        .await
        .expect("second page");
    assert_eq!(second.items[0]["messageId"], json!("m-assistant"));
    assert_eq!(second.cursor.as_deref(), Some("2"));
    let last = session
        .history(second.cursor.as_deref(), 1)
        .await
        .expect("last page");
    assert_eq!(last.items[0]["messageId"], json!("m-tool"));
    assert_eq!(last.cursor.as_deref(), Some("3"));
    assert!(session
        .history(last.cursor.as_deref(), 1)
        .await
        .expect("past the end")
        .items
        .is_empty());
}

/// Publishes one event while it runs, as a run adapter that renders a live stream does.
struct Streaming;

#[async_trait]
impl HarnessAdapter for Streaming {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            event_delivery: true,
            ..capabilities()
        }
    }

    async fn execute(&self, ctx: &HarnessContext) -> agentscope_durable::Result<()> {
        ctx.publish(&json!({ "type": "text.delta", "text": "hi" }));
        Ok(())
    }
}

#[tokio::test]
async fn a_run_publishes_through_the_session_event_sink() {
    let (_backend, session, _thread) = fixture(Arc::new(Streaming)).await;
    session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");

    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    session.set_event_sink(Arc::new(move |event: &Value| {
        sink.lock().expect("seen").push(event.clone());
    }));
    session.tick().await.expect("tick");

    let seen = seen.lock().expect("seen");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["text"], json!("hi"));
}

#[tokio::test]
async fn a_run_without_a_host_sink_still_advances() {
    let (_backend, session, _thread) = fixture(Arc::new(Streaming)).await;
    session
        .submit("req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    assert_eq!(session.tick().await.expect("tick"), TickOutcome::Settled);
}
