//! The reader-facing event stream: delta filters, turn handles and pending interactions.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::types::*;
use agentscope_durable::{
    now_millis, DeltaFilter, DurableRuntime, DurableSession, EventStreamOptions, Result,
    SessionBackend, TickOutcome,
};
use async_trait::async_trait;
use serde_json::{json, Value};

fn state_version() -> AdapterCapabilities {
    AdapterCapabilities {
        state_version: "state-1".to_string(),
        recovery_boundary: true,
        event_delivery: false,
    }
}

fn binding() -> ThreadBinding {
    ThreadBinding::new("stub", "def-1", "state-1").expect("binding")
}

fn thread_of(key: &SessionKey) -> ThreadKey {
    ThreadKey::new(key.clone(), "main").expect("thread key")
}

async fn session(backend: &TxnBackend) -> SessionKey {
    let key = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: binding(),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    key
}

struct EchoAdapter;

#[async_trait]
impl HarnessAdapter for EchoAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        state_version()
    }

    async fn execute(&self, ctx: &HarnessContext) -> Result<()> {
        let frame = ctx.backend.frame(&ctx.lease).await?;
        let mut commit = Commit::new("echo", frame.turn.revision, TurnStatus::Succeeded);
        commit.output = json!({ "reply": "ok" });
        ctx.backend.commit(&ctx.lease, commit).await?;
        Ok(())
    }
}

/// Commits one `waiting` turn and then never does anything again.
struct WaitingAdapter {
    calls: AtomicUsize,
}

#[async_trait]
impl HarnessAdapter for WaitingAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        state_version()
    }

    async fn execute(&self, ctx: &HarnessContext) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let frame = ctx.backend.frame(&ctx.lease).await?;
        let mut commit = Commit::new("wait", frame.turn.revision, TurnStatus::Waiting);
        commit.waiting =
            Some(WaitCondition::new("tool:call-1", now_millis() + 60_000).expect("wait"));
        ctx.backend.commit(&ctx.lease, commit).await?;
        Ok(())
    }
}

#[tokio::test]
async fn events_with_none_keeps_only_the_ending() {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = session(&backend).await;
    let thread = thread_of(&key);
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let runtime = DurableRuntime::new(backend.clone(), "owner-a")
        .with_adapter("stub", Arc::new(EchoAdapter) as Arc<dyn HarnessAdapter>);
    assert_eq!(
        runtime.tick(&thread).await.expect("tick"),
        TickOutcome::Settled
    );

    let all = backend
        .events(&key, None, 100)
        .await
        .expect("unfiltered events");
    assert!(
        all.events.len() > 1,
        "the stream has more than the ending: {:?}",
        all.events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );

    let ending = backend
        .events_with(
            &key,
            None,
            100,
            EventStreamOptions::defaults().with_deltas(DeltaFilter::None),
        )
        .await
        .expect("terminal events");
    assert_eq!(ending.events.len(), 1);
    assert_eq!(ending.events[0].event_type, "turn.succeeded");
    assert_eq!(ending.events[0].turn_id, receipt.turn_id);

    let messages = backend
        .events_with(
            &key,
            None,
            100,
            EventStreamOptions::defaults().with_deltas(DeltaFilter::Message),
        )
        .await
        .expect("message events");
    assert!(!messages.events.is_empty());
    assert!(messages.events.iter().all(|event| {
        event.event_type.starts_with("message/") || event.event_type == "turn.succeeded"
    }));
}

#[tokio::test]
async fn a_turn_handle_returns_the_ending_without_polling() {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = session(&backend).await;
    let thread = thread_of(&key);
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let runtime = Arc::new(
        DurableRuntime::new(backend.clone(), "owner-a")
            .with_adapter("stub", Arc::new(EchoAdapter) as Arc<dyn HarnessAdapter>),
    );
    assert_eq!(
        runtime.tick(&thread).await.expect("tick"),
        TickOutcome::Settled
    );

    let session = DurableSession::shared(runtime, thread);
    let handle = session.turn_handle(&receipt.turn_id);
    let started = Instant::now();
    let view = handle
        .wait_until_terminal(Duration::from_secs(5), Duration::from_secs(5))
        .await
        .expect("terminal turn");
    assert_eq!(view.status, TurnStatus::Succeeded);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a terminal turn must return before one poll interval"
    );

    let result = handle.result().await.expect("result").expect("ending");
    assert_eq!(result, json!({ "reply": "ok" }));
}

#[tokio::test]
async fn pending_interactions_project_the_wait_key() {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = session(&backend).await;
    let thread = thread_of(&key);
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let runtime = Arc::new(
        DurableRuntime::new(backend.clone(), "owner-a").with_adapter(
            "stub",
            Arc::new(WaitingAdapter {
                calls: AtomicUsize::new(0),
            }) as Arc<dyn HarnessAdapter>,
        ),
    );
    assert_eq!(
        runtime.tick(&thread).await.expect("tick"),
        TickOutcome::Waiting
    );

    let session = DurableSession::shared(runtime, thread);
    let waiting = session.waiting().await.expect("waiting turns");
    let interactions = session.pending_interactions().await.expect("interactions");
    assert_eq!(interactions.len(), waiting.len());
    for (turn, interaction) in waiting.iter().zip(interactions.iter()) {
        let wait = turn.waiting.as_ref().expect("waiting condition");
        assert_eq!(interaction.key, wait.key);
        assert_eq!(interaction.id, wait.key);
        assert_eq!(interaction.turn_id, turn.turn_id);
        assert_eq!(interaction.thread_id, turn.thread.thread_id);
        assert_eq!(interaction.status, "waiting");
    }
    assert_eq!(interactions[0].turn_id, receipt.turn_id);
}
