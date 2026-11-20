//! Regression tests for canceling a turn nobody is executing.
//!
//! A cancel only *requests* an end. While an invocation is running, the tick that owns the turn
//! observes the request and writes the ending. A turn that is `waiting` (on an external effect, or
//! after a failure it may retry) or `paused` has no such invocation: nothing inside the turn can
//! see the request, so it used to stay `cancelling` until the next prompt happened to tick the
//! thread — the client had already been told the turn was cancelled.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::types::*;
use agentscope_durable::{now_millis, DurableRuntime, Result, SessionBackend, TickOutcome};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Commits one `waiting` turn and then never does anything again.
struct WaitingAdapter {
    calls: AtomicUsize,
}

#[async_trait]
impl HarnessAdapter for WaitingAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            state_version: "state-1".to_string(),
            recovery_boundary: true,
            event_delivery: false,
        }
    }

    async fn execute(&self, ctx: &HarnessContext) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let frame = ctx.backend.frame(&ctx.lease).await?;
        if frame.tools.is_empty() {
            // An effect the turn asked for and never got an answer for: a terminal commit is
            // refused while it is unsettled, so the cancel path has to accept it first. The
            // intent is committed while the turn is still running (a `waiting` commit hands the
            // lease back), and the wait is what this tick ends with.
            let call = ToolCall::new("call-1", "local", "shell_execute", json!({ "cmd": "ls" }))
                .expect("call");
            let mut intent = Commit::new("intent", frame.turn.revision, TurnStatus::Running);
            intent.tools = vec![call];
            ctx.backend.commit(&ctx.lease, intent).await?;
            ctx.backend
                .begin_dispatch(&ctx.lease, &format!("{}:call-1", ctx.lease.turn_id))
                .await?;
        }
        let frame = ctx.backend.frame(&ctx.lease).await?;
        let mut commit = Commit::new("wait", frame.turn.revision, TurnStatus::Waiting);
        commit.waiting =
            Some(WaitCondition::new("tool:call-1", now_millis() + 60_000).expect("wait condition"));
        ctx.backend.commit(&ctx.lease, commit).await?;
        Ok(())
    }
}

async fn waiting_turn() -> (Arc<TxnBackend>, ThreadKey, String, Arc<WaitingAdapter>) {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new("stub", "def-1", "state-1").expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let adapter = Arc::new(WaitingAdapter {
        calls: AtomicUsize::new(0),
    });
    (backend, thread, receipt.turn_id, adapter)
}

#[tokio::test]
async fn a_cancelled_waiting_turn_becomes_terminal_without_reaching_the_adapter() {
    let (backend, thread, turn_id, adapter) = waiting_turn().await;
    let runtime = DurableRuntime::new(backend.clone(), "owner-a")
        .with_adapter("stub", Arc::clone(&adapter) as Arc<dyn HarnessAdapter>);

    // One tick puts the turn in the state a host hands back.
    assert_eq!(
        runtime.tick(&thread).await.expect("tick"),
        TickOutcome::Waiting
    );
    let waiting = backend.turn(&thread, &turn_id).await.expect("turn");
    assert_eq!(waiting.status, TurnStatus::Waiting);
    assert!(waiting.waiting.is_some(), "the turn waits on its key");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);

    // The client presses Stop: nothing inside the turn can observe it now.
    runtime
        .cancel(&thread, &turn_id, "cancel-1")
        .await
        .expect("cancel");
    let cancelling = backend.turn(&thread, &turn_id).await.expect("turn");
    assert_eq!(cancelling.status, TurnStatus::Cancelling);

    // The tick that follows the request has to write the ending itself.
    assert_eq!(
        runtime.tick(&thread).await.expect("tick"),
        TickOutcome::Cancelled
    );
    let settled = backend.turn(&thread, &turn_id).await.expect("turn");
    assert_eq!(settled.status, TurnStatus::Cancelled);
    assert!(
        settled.status.terminal(),
        "a cancelled turn must be terminal, not left clearing"
    );
    assert_eq!(
        adapter.calls.load(Ordering::SeqCst),
        1,
        "settling a cancel must not start an invocation (and a model call) just to cancel it"
    );
}

#[tokio::test]
async fn a_cancelled_turn_settles_the_effects_it_still_holds() {
    let (backend, thread, turn_id, adapter) = waiting_turn().await;
    let runtime = DurableRuntime::new(backend.clone(), "owner-a")
        .with_adapter("stub", Arc::clone(&adapter) as Arc<dyn HarnessAdapter>);
    // The turn asks for an effect, dispatches it, and hands the thread back while it waits.
    runtime.tick(&thread).await.expect("tick");
    let waiting = backend.turn(&thread, &turn_id).await.expect("turn");
    assert_eq!(waiting.status, TurnStatus::Waiting);

    runtime
        .cancel(&thread, &turn_id, "cancel-1")
        .await
        .expect("cancel");
    assert_eq!(
        runtime.tick(&thread).await.expect("tick"),
        TickOutcome::Cancelled
    );
    let settled = backend.turn(&thread, &turn_id).await.expect("turn");
    assert_eq!(settled.status, TurnStatus::Cancelled);
    let execution = backend
        .execution(&thread, &format!("{turn_id}:call-1"))
        .await
        .expect("execution");
    assert_eq!(
        execution.result.as_ref().map(|result| result.state),
        Some(ExecutionState::Cancelled),
        "the effect the turn was waiting for belongs to the turn being stopped"
    );
}

#[tokio::test]
async fn cancelling_a_queued_turn_leaves_the_next_one_queued() {
    let (backend, thread, first, adapter) = waiting_turn().await;
    let runtime = DurableRuntime::new(backend.clone(), "owner-a")
        .with_adapter("stub", Arc::clone(&adapter) as Arc<dyn HarnessAdapter>);

    // A second turn waits behind the first one.
    let second = backend
        .submit(&thread, "req-2", json!({ "text": "two" }))
        .await
        .expect("submit");

    // Cancelling a turn that never started must not go looking for in-flight effects: the claim
    // that search takes lands on the *next* unfinished turn and starts it (epoch bumped, status
    // running, no owner) — a Stop would then kick the queue instead of clearing it.
    runtime
        .cancel(&thread, &first, "cancel-1")
        .await
        .expect("cancel");

    let behind = backend.turn(&thread, &second.turn_id).await.expect("turn");
    assert_eq!(behind.status, TurnStatus::Queued);
    assert_eq!(
        adapter.calls.load(Ordering::SeqCst),
        0,
        "cancelling a queued turn must not run anything"
    );
}
