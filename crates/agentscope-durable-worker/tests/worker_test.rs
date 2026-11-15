//! What one step is allowed to do, and what two deployment shapes must have in common.
//!
//! Every test drives the public surface only: an in-memory transactional backend, an adapter that
//! commits nothing, a scripted brain and a recording hands. No model, no tool, no network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::executor::Executor;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::types::*;
use agentscope_durable::{DurableRuntime, Result as DurableResult, SessionBackend, TickOutcome};
use agentscope_durable_worker::{
    DriveReport, Driver, EdgeDriver, Hands, IntentKind, LocalDriver, StubBrain, StubHands,
    StubReply, Worker,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::time::timeout;

const HARNESS: &str = "stub";
/// Generous next to a step that does four awaits; only a step that waits for an external effect
/// can miss it.
const STEP_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

fn binding() -> ThreadBinding {
    ThreadBinding::new(HARNESS, "def-1", "state-1").expect("binding")
}

fn capabilities() -> AdapterCapabilities {
    AdapterCapabilities {
        state_version: "state-1".to_string(),
        recovery_boundary: true,
        event_delivery: false,
    }
}

/// Commits nothing: the turn stays live, which is the state a brain decides against.
struct Idle;

#[async_trait]
impl HarnessAdapter for Idle {
    fn capabilities(&self) -> AdapterCapabilities {
        capabilities()
    }

    async fn execute(&self, _ctx: &HarnessContext) -> DurableResult<()> {
        Ok(())
    }
}

/// One world: a session with a single submitted turn, and a worker over it.
struct World {
    backend: Arc<TxnBackend>,
    worker: Arc<Worker>,
    thread: ThreadKey,
}

async fn world(brain: Arc<StubBrain>, hands: Arc<dyn Hands>) -> World {
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
        .with_adapter(HARNESS, Arc::new(Idle));
    let worker = Arc::new(Worker::new(Arc::new(runtime), brain, hands));
    worker
        .runtime()
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit turn");
    World {
        backend,
        worker,
        thread,
    }
}

fn act(call_id: &str) -> StubReply {
    StubReply::act(call_id, "write_file", json!({ "path": "a.md" })).expect("tool call")
}

fn new_brain(script: Vec<StubReply>) -> Arc<StubBrain> {
    Arc::new(StubBrain::new(script))
}

/// The signal ids a drive wrote, in order.
fn written(report: &DriveReport) -> Vec<String> {
    report
        .steps
        .iter()
        .filter_map(|step| step.written_back.clone())
        .collect()
}

fn kinds(report: &DriveReport) -> Vec<IntentKind> {
    report.steps.iter().filter_map(|step| step.kind()).collect()
}

// ---------------------------------------------------------------------------
// 1: one step is one tick, one decision, one effect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_step_runs_exactly_one_tick_one_decision_and_one_effect() {
    let brain = new_brain(vec![act("c1"), act("c2"), act("c3")]);
    let hands = Arc::new(StubHands::new());
    let world = world(Arc::clone(&brain), Arc::clone(&hands) as Arc<dyn Hands>).await;

    let first = world.worker.step(&world.thread).await.expect("first step");
    assert_eq!(first.tick, TickOutcome::Settled);
    assert_eq!(first.kind(), Some(IntentKind::Act));
    assert!(first.effect.is_some());
    assert!(first.written_back.is_some());
    assert!(
        first.more_work,
        "an applied effect leaves work that is already due"
    );
    // The whole point: one call in, one tick and one decision out. A worker that looped here
    // would report 2 and prove that a step is not a unit.
    assert_eq!(brain.thinks(), 1, "one decision per step");
    assert_eq!(hands.count(), 1, "one effect per step");
    assert_eq!(brain.remaining(), 2, "the rest of the script is untouched");

    // A second unit of work requires a second explicit step.
    let second = world.worker.step(&world.thread).await.expect("second step");
    assert_eq!(second.kind(), Some(IntentKind::Act));
    assert_eq!(brain.thinks(), 2);
    assert_eq!(hands.count(), 2);
}

#[tokio::test]
async fn a_tick_that_makes_no_progress_does_not_consult_the_brain() {
    let brain = new_brain(vec![act("c1")]);
    let hands = Arc::new(StubHands::new());
    let world = world(Arc::clone(&brain), Arc::clone(&hands) as Arc<dyn Hands>).await;

    // Another host holds the lease, so this host's tick cannot claim the thread and answers
    // `Idle`. Deciding anything then would be a decision about work it does not own.
    let lease = world
        .backend
        .claim(&world.thread, "owner-b", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("a turn is claimable");
    let report = world.worker.step(&world.thread).await.expect("step");
    assert_eq!(report.tick, TickOutcome::Idle);
    assert_eq!(report.kind(), None);
    assert!(!report.more_work);
    assert_eq!(brain.thinks(), 0, "an idle tick decides nothing");
    assert_eq!(hands.count(), 0);

    // The lease is released the way any other writer releases it; the foreign claim above only
    // existed to make the ownership story explicit.
    world
        .backend
        .release(&lease, Duration::ZERO)
        .await
        .expect("release");
    let report = world.worker.step(&world.thread).await.expect("step");
    assert_eq!(report.tick, TickOutcome::Settled);
    assert_eq!(brain.thinks(), 1);
    assert_eq!(hands.count(), 1);
}

// ---------------------------------------------------------------------------
// 2: intent -> effect -> durable record
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_intent_becomes_one_effect_with_one_durable_record() {
    let brain = new_brain(vec![act("call-1")]);
    let hands = Arc::new(StubHands::new());
    let world = world(Arc::clone(&brain), Arc::clone(&hands) as Arc<dyn Hands>).await;

    let report = world.worker.step(&world.thread).await.expect("step");
    let Some(agentscope_durable_worker::Intent::Act(request)) = report.intent.clone() else {
        panic!("expected an act intent, got {:?}", report.intent);
    };

    // The hands were asked for exactly the durable identity of the intent, once.
    assert_eq!(hands.calls(), vec![request.execution_id.clone()]);
    assert_eq!(request.call.name, "write_file");
    assert_eq!(
        request.execution_id,
        format!("{}:{}", request.turn_id, request.call.call_id),
        "one derivation of the effect identity"
    );

    // The effect is the answer the hands gave, not a guess.
    let effect = report.effect.clone().expect("effect");
    assert_eq!(effect.state, ExecutionState::Succeeded);
    assert_eq!(effect.result, json!({ "callId": "call-1" }));

    // …and it is durable: one `signal.accepted` event carries it, under the id the report names.
    let written = report.written_back.clone().expect("write-back");
    let events = world
        .backend
        .events(&world.thread.session, None, 100)
        .await
        .expect("events")
        .events;
    let accepted: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == "signal.accepted")
        .collect();
    assert_eq!(accepted.len(), 1, "one record per effect");
    assert_eq!(accepted[0].data["signalId"], json!(written));
    assert_eq!(
        accepted[0].data["key"],
        json!(agentscope_durable_worker::HANDS_SIGNAL_KEY)
    );

    // Writing the same effect back twice is the same record, not a second one (idempotence of
    // the identity, not of the driver).
    let again = world
        .backend
        .signal(
            &world.thread,
            &request.turn_id,
            Signal::new(
                written.clone(),
                agentscope_durable_worker::HANDS_SIGNAL_KEY,
                json!({
                    "executionId": request.execution_id,
                    "callId": request.call.call_id,
                    "state": effect.state,
                    "handle": effect.handle,
                    "result": effect.result,
                    "error": effect.error,
                }),
            )
            .expect("signal"),
        )
        .await;
    assert!(
        again.is_ok(),
        "the same effect under the same id is a no-op"
    );
}

// ---------------------------------------------------------------------------
// 3: the two drivers are one worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_two_drivers_emit_the_same_step_sequence() {
    // Two identical worlds, so a difference in the reports can only come from the driver.
    let script = || vec![act("c1"), act("c2"), StubReply::done(json!({ "ok": true }))];
    let local_brain = new_brain(script());
    let local_hands = Arc::new(StubHands::new());
    let local_world = world(
        Arc::clone(&local_brain),
        Arc::clone(&local_hands) as Arc<dyn Hands>,
    )
    .await;

    let edge_brain = new_brain(script());
    let edge_hands = Arc::new(StubHands::new());
    let edge_world = world(
        Arc::clone(&edge_brain),
        Arc::clone(&edge_hands) as Arc<dyn Hands>,
    )
    .await;

    let local = LocalDriver::new(Arc::clone(&local_world.worker))
        .drive(&local_world.thread)
        .await
        .expect("local drive");

    // The edge host gets one step per wakeup; the platform calls it again while it says more
    // work is due. That repetition is the platform's, not the driver's.
    let edge_driver = EdgeDriver::new(Arc::clone(&edge_world.worker));
    let mut edge = DriveReport { steps: Vec::new() };
    for _ in 0..8 {
        let wakeup = edge_driver
            .drive(&edge_world.thread)
            .await
            .expect("edge drive");
        assert_eq!(wakeup.steps.len(), 1, "one wakeup is one step");
        let more = wakeup.more_work();
        edge.steps.extend(wakeup.steps);
        if !more {
            break;
        }
    }

    assert_eq!(local.ticks(), edge.ticks(), "same tick sequence");
    assert_eq!(
        kinds(&local),
        kinds(&edge),
        "same decisions in the same order"
    );
    assert_eq!(
        local.ticks(),
        vec![
            TickOutcome::Settled,
            TickOutcome::Settled,
            TickOutcome::Settled
        ],
        "two effects and a stop"
    );
    assert_eq!(
        kinds(&local),
        vec![IntentKind::Act, IntentKind::Act, IntentKind::Done],
        "no driver may skip, repeat or reorder a decision"
    );
    assert_eq!(local.effects(), 2);
    assert_eq!(local.writes(), 2);
    assert_eq!(edge.effects(), local.effects());
    assert_eq!(edge.writes(), local.writes());
    assert_eq!(local_brain.thinks(), edge_brain.thinks());
    assert_eq!(local_hands.count(), edge_hands.count());
    assert!(!local.more_work(), "the done intent ends the drive");
    assert!(!edge.more_work());
    assert_eq!(local_brain.remaining(), 0);
    assert_eq!(edge_brain.remaining(), 0);

    // The written records differ only in ids that legitimately differ per session; their count
    // and shape do not.
    let local_written = written(&local);
    let edge_written = written(&edge);
    assert_eq!(local_written.len(), edge_written.len());
    let calls = |ids: &[String]| {
        ids.iter()
            .map(|id| id.rsplit(':').next().expect("call id").to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(calls(&local_written), vec!["c1", "c2"]);
    assert_eq!(calls(&edge_written), vec!["c1", "c2"]);
    assert!(local_written
        .iter()
        .all(|id| id.starts_with(agentscope_durable_worker::HANDS_SIGNAL_KEY)));
}

// ---------------------------------------------------------------------------
// 4: R2 — a step never waits for an external effect
// ---------------------------------------------------------------------------

/// Answers the first apply and then blocks forever.
///
/// If a step applied intents in a loop, or waited for one to report an ending, the second apply
/// would hang and the timeout below would catch it.
struct BlocksOnSecondApply {
    calls: AtomicUsize,
}

#[async_trait]
impl Executor for BlocksOnSecondApply {
    async fn start(&self, _request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        let seen = self.calls.fetch_add(1, Ordering::SeqCst);
        if seen > 0 {
            std::future::pending::<()>().await;
        }
        Ok(ExecutionResult::succeeded(json!({ "applied": seen })))
    }
}

#[tokio::test]
async fn a_step_never_waits_for_an_external_effect() {
    let brain = new_brain(vec![act("c1"), act("c2"), act("c3"), act("c4")]);
    let hands = Arc::new(BlocksOnSecondApply {
        calls: AtomicUsize::new(0),
    });
    let world = world(Arc::clone(&brain), Arc::clone(&hands) as Arc<dyn Hands>).await;

    let report = timeout(STEP_TIMEOUT, world.worker.step(&world.thread))
        .await
        .expect("a step must not block on an external effect")
        .expect("step");
    assert_eq!(report.kind(), Some(IntentKind::Act));
    assert_eq!(hands.calls.load(Ordering::SeqCst), 1, "one apply per step");
    assert_eq!(brain.thinks(), 1);
    assert!(
        report.more_work,
        "the answer is `more work is due`, not `wait for it here`"
    );
    assert!(report.written_back.is_some());
    // The brain's remaining script proves nothing else ran behind our back.
    assert_eq!(brain.remaining(), 3);
}

/// Answers `PENDING`: the effect started but has no ending yet.
struct AnswersPending {
    calls: AtomicUsize,
}

#[async_trait]
impl Executor for AnswersPending {
    async fn start(&self, _request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult {
            state: ExecutionState::Pending,
            handle: Some("handle-1".to_string()),
            result: Value::Null,
            error: None,
        })
    }

    async fn query(&self, _request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        panic!("a step must not poll an effect it just started");
    }
}

#[tokio::test]
async fn a_pending_effect_is_reported_and_not_polled() {
    let brain = new_brain(vec![act("c1"), act("c2")]);
    let hands = Arc::new(AnswersPending {
        calls: AtomicUsize::new(0),
    });
    let world = world(Arc::clone(&brain), Arc::clone(&hands) as Arc<dyn Hands>).await;

    let report = timeout(STEP_TIMEOUT, world.worker.step(&world.thread))
        .await
        .expect("a pending effect must not be waited for")
        .expect("step");
    let effect = report.effect.clone().expect("effect");
    assert_eq!(effect.state, ExecutionState::Pending);
    assert_eq!(effect.handle.as_deref(), Some("handle-1"));
    assert_eq!(hands.calls.load(Ordering::SeqCst), 1);
    assert_eq!(brain.thinks(), 1);
    assert!(
        report.more_work,
        "the effect is still in flight, so the host may act again"
    );
}

/// Fails the apply outright: the effect may still have happened.
struct Refuses {
    calls: AtomicUsize,
}

#[async_trait]
impl Executor for Refuses {
    async fn start(&self, _request: &ExecutionRequest) -> DurableResult<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(agentscope_durable::DurableError::unavailable(
            "hands offline",
        ))
    }
}

#[tokio::test]
async fn a_refused_apply_is_unknown_and_stops_the_drive() {
    let brain = new_brain(vec![act("c1"), act("c2")]);
    let hands = Arc::new(Refuses {
        calls: AtomicUsize::new(0),
    });
    let world = world(Arc::clone(&brain), Arc::clone(&hands) as Arc<dyn Hands>).await;

    let report = timeout(STEP_TIMEOUT, world.worker.step(&world.thread))
        .await
        .expect("step")
        .expect("step");
    let effect = report.effect.clone().expect("effect");
    assert_eq!(
        effect.state,
        ExecutionState::Unknown,
        "a failed apply is not `did not happen`"
    );
    assert!(
        !report.more_work,
        "an unaccounted effect is not permission to keep going"
    );
    assert_eq!(hands.calls.load(Ordering::SeqCst), 1);
    assert_eq!(brain.thinks(), 1);
    // The refusal is still recorded, so a recovery step can reconcile it.
    assert!(report.written_back.is_some());
}
