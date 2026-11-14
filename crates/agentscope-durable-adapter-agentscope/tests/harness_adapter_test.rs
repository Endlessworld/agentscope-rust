//! The durable adapter driving a real `HarnessAgent` inside a tick.
//!
//! Every test here goes through `DurableRuntime::tick`, so the adapter is exercised exactly the
//! way a host drives it: the runtime claims the lease, the adapter runs one invocation under that
//! lease and settles the turn, and the runtime releases. Nothing hand-builds the recorder, the
//! execution host or the ending.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::journal::{JournalEvent, ThreadJournal};
use agentscope_core::message::JsonMap;
use agentscope_core::message::{ContentBlock, Msg, TextBlock, ToolResultBlock, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{FunctionTool, Tool, ToolExecutionContext, Toolkit};
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::settle_turn;
use agentscope_durable::types::*;
use agentscope_durable::{DurableRuntime, Result as DurableResult, SessionBackend, TickOutcome};
use agentscope_durable_adapter_agentscope::{
    AgentScopeHarnessAdapter, BackendThreadJournal, FINALIZATION_STARTED,
};
use agentscope_harness::HarnessAgent;
use futures::stream::BoxStream;
use serde_json::{json, Value};

const HARNESS: &str = "durable-host";
const SESSION: &str = "adapter-session";
const TOOL: &str = "mark";
const LEASE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scripted model: it counts its calls and decides from the tool results it is shown.
struct ScriptedModel {
    calls: AtomicUsize,
    /// The text of every request it was shown, per call.
    seen: Mutex<Vec<Vec<String>>>,
    /// The tool-call ids to ask for, in order, before answering in text.
    script: Vec<&'static str>,
    /// The first call never answers, so the invocation stays in flight until control arrives.
    hold_first: bool,
    /// How many streams it opened.
    entered: AtomicUsize,
}

impl ScriptedModel {
    fn new(script: &[&'static str]) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            script: script.to_vec(),
            hold_first: false,
            entered: AtomicUsize::new(0),
        })
    }

    /// A model whose first call hangs: only a control request can end that invocation.
    fn held() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            script: Vec::new(),
            hold_first: true,
            entered: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Waits until a model call is in flight, so a control request really lands mid-invocation.
    async fn await_in_flight(&self) {
        for _ in 0..500 {
            if self.entered.load(Ordering::SeqCst) > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the model call never started");
    }

    /// Whether any request carried `text`.
    fn saw(&self, text: &str) -> bool {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .flatten()
            .any(|message| message.contains(text))
    }
}

impl Model for ScriptedModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.fetch_add(1, Ordering::SeqCst);
        if self.hold_first && call == 0 {
            // An in-flight provider call that never yields: only the control the tick observes can
            // end the invocation, which is what a cancel and a pause have to interrupt.
            return Box::pin(futures::stream::pending());
        }
        self.seen
            .lock()
            .expect("seen")
            .push(messages.iter().map(Msg::text_content).collect());
        let answered: Vec<String> = messages
            .iter()
            .flat_map(Msg::tool_result_blocks)
            .filter_map(|result| result.id.clone())
            .collect();
        let next = self
            .script
            .iter()
            .find(|id| !answered.iter().any(|seen| seen == *id));
        let response = match next {
            Some(id) => ChatResponse::builder()
                .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                    *id,
                    TOOL,
                    JsonMap::new(),
                ))])
                .build(),
            None => ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new("done"))])
                .build(),
        };
        Box::pin(futures::stream::once(async move { Ok(response) }))
    }

    fn model_name(&self) -> &str {
        "scripted"
    }
}

/// A tool with no parameters whose result is the text `ok`.
fn mark_tool() -> Arc<dyn Tool> {
    Arc::new(FunctionTool::new(
        TOOL,
        "marks the call",
        json!({ "type": "object" }),
        Box::new(|_args: JsonMap, _ctx: ToolExecutionContext| Ok(ToolResultBlock::success("ok"))),
    ))
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentscope-durable-adapter-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The factory the adapter calls once per tick.
fn factory(
    model: Arc<dyn Model>,
    root: PathBuf,
    max_iters: i32,
) -> impl Fn() -> Arc<HarnessAgent> + Send + Sync + 'static {
    move || {
        let toolkit = Toolkit::new();
        toolkit.register_tool(mark_tool());
        Arc::new(
            HarnessAgent::builder()
                .name(HARNESS)
                .model(Arc::clone(&model))
                .session_id(SESSION)
                .user_id("alice")
                .max_iters(max_iters)
                .cwd(root.clone())
                .workspace(root.join("workspace"))
                .toolkit(toolkit)
                .disable_compaction()
                .disable_workspace_context()
                .disable_at_path_expansion()
                .disable_memory_hooks()
                .disable_memory_tools()
                .disable_web_tools()
                .disable_shell_tool()
                .disable_subagents()
                .disable_session_persistence()
                .build(),
        )
    }
}

struct Fixture {
    runtime: Arc<DurableRuntime>,
    backend: Arc<TxnBackend>,
    thread: ThreadKey,
    turn_id: String,
    model: Arc<ScriptedModel>,
}

async fn fixture(script: &[&'static str]) -> Fixture {
    fixture_with(script, 6).await
}

async fn fixture_with(script: &[&'static str], max_iters: i32) -> Fixture {
    fixture_of(ScriptedModel::new(script), max_iters).await
}

/// A fixture whose first model call never answers: the tick stays in flight until control.
async fn held_fixture() -> Fixture {
    fixture_of(ScriptedModel::held(), 6).await
}

async fn fixture_of(model: Arc<ScriptedModel>, max_iters: i32) -> Fixture {
    let backend = Arc::new(TxnBackend::in_memory());
    let runtime = Arc::new(
        DurableRuntime::new(Arc::clone(&backend) as Arc<dyn SessionBackend>, "runtime")
            .with_adapter(
                HARNESS,
                Arc::new(AgentScopeHarnessAdapter::new(factory(
                    Arc::clone(&model) as Arc<dyn Model>,
                    scratch_dir("agent"),
                    max_iters,
                ))),
            )
            .with_lease_ttl(LEASE),
    );
    let key = SessionKey::new("ns", SESSION).expect("session key");
    runtime
        .ensure_session(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new(HARNESS, "def-1", "state-1").expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    let receipt = runtime
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    Fixture {
        runtime,
        backend,
        thread,
        turn_id: receipt.turn_id,
        model,
    }
}

impl Fixture {
    async fn turn(&self) -> TurnView {
        self.backend
            .turn(&self.thread, &self.turn_id)
            .await
            .expect("turn")
    }

    async fn journal(&self) -> Vec<JournalEvent> {
        let state = self
            .backend
            .thread_state(&self.thread)
            .await
            .expect("state");
        if state.journal_position == 0 {
            return Vec::new();
        }
        self.backend
            .read_journal(&self.thread, 0, state.journal_position as usize)
            .await
            .expect("journal")
            .into_iter()
            .filter_map(|entry| serde_json::from_value::<JournalEvent>(entry).ok())
            .collect()
    }
}

fn reply_text(value: &Value) -> Option<String> {
    serde_json::from_value::<Msg>(value.clone())
        .ok()
        .map(|message| message.text_content())
}

// ---------------------------------------------------------------------------
// One tick, one unit of work
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tick_makes_exactly_one_model_call() {
    // The model asks for a tool first, so finishing the turn would need a second call.
    let fixture = fixture(&["m1"]).await;

    let outcome = fixture.runtime.tick(&fixture.thread).await.expect("tick");

    assert_eq!(
        outcome,
        TickOutcome::Settled,
        "the tick ran its unit of work"
    );
    assert_eq!(
        fixture.model.calls(),
        1,
        "one tick is one model call, not a whole turn"
    );
    let turn = fixture.turn().await;
    assert_eq!(
        turn.status,
        TurnStatus::Running,
        "the turn still has work the next tick will do"
    );
    assert!(turn.error.is_none(), "a bounded tick records no error");
}

#[tokio::test]
async fn a_borrowed_lease_lets_the_tick_write_the_journal() {
    let fixture = fixture(&["m1"]).await;

    let outcome = fixture.runtime.tick(&fixture.thread).await.expect("tick");
    assert_eq!(outcome, TickOutcome::Settled);

    let events = fixture.journal().await;
    assert!(
        !events.is_empty(),
        "the recorder committed under the tick's lease: a claim would have been refused",
    );
    let turn_id = fixture.turn_id.clone();
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "step/start"
                && event.turn_id.as_deref() == Some(&turn_id)),
        "the batch landed in this turn: {events:?}",
    );
    // The tool the model asked for ran, so its result rode the same journal.
    assert!(
        events.iter().any(|event| event.event_type == "tool/result"),
        "the acting phase's tool result is committed",
    );
    // And the checkout really happened in a durable transaction, not only in memory.
    let state = fixture
        .backend
        .thread_state(&fixture.thread)
        .await
        .expect("state");
    assert!(
        state.checkpoint_metadata.is_some(),
        "the state checkpointed"
    );
}

// ---------------------------------------------------------------------------
// The borrow is scoped to the tick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_borrow_ends_with_the_tick() {
    let fixture = fixture(&["m1"]).await;
    let lease = fixture
        .backend
        .claim(&fixture.thread, "runtime", LEASE)
        .await
        .expect("claim")
        .expect("a claimable turn");

    // Inside the borrow the bridge adopts the tick's lease and must not release it.
    let journal = BackendThreadJournal::new(
        Arc::clone(&fixture.backend) as Arc<dyn SessionBackend>,
        fixture.thread.clone(),
    )
    .attach_lease(lease.clone());
    let writer = journal
        .acquire("recorder", LEASE)
        .await
        .expect("borrowed writer");
    assert_eq!(writer.owner, lease.owner);
    assert_eq!(writer.epoch, lease.epoch);
    journal.release(&writer).await.expect("release is a no-op");
    assert!(
        fixture
            .backend
            .claim(&fixture.thread, "other", LEASE)
            .await
            .expect("claim")
            .is_none(),
        "releasing the borrow must not hand the thread to another writer",
    );

    // A bridge that did not borrow still claims, and is still refused while the tick runs.
    let plain = BackendThreadJournal::new(
        Arc::clone(&fixture.backend) as Arc<dyn SessionBackend>,
        fixture.thread.clone(),
    );
    assert!(
        plain.acquire("recorder", LEASE).await.is_err(),
        "claim mode is unchanged by the borrow"
    );

    // After the borrow the bridge claims again: refused while held, accepted once free.
    journal.detach_lease();
    assert!(
        journal.acquire("recorder", LEASE).await.is_err(),
        "the borrow state must not leak past detach"
    );
    fixture
        .backend
        .release(&lease, Duration::ZERO)
        .await
        .expect("release");
    journal
        .acquire("recorder", LEASE)
        .await
        .expect("the freed lease is claimable again");
}

// ---------------------------------------------------------------------------
// A paused tick is a yield
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_paused_tick_yields_and_the_next_one_finishes_the_turn() {
    let fixture = fixture(&["m1"]).await;

    let first = fixture.runtime.tick(&fixture.thread).await.expect("tick");
    assert_eq!(first, TickOutcome::Settled, "a yield is not a failure");
    let paused = fixture.turn().await;
    assert_eq!(paused.status, TurnStatus::Running);
    assert!(
        paused.error.is_none(),
        "a yield records no error: {paused:?}"
    );
    assert_eq!(fixture.model.calls(), 1);

    let second = fixture.runtime.tick(&fixture.thread).await.expect("tick");
    assert_eq!(second, TickOutcome::Settled);
    let settled = fixture.turn().await;
    assert_eq!(
        settled.status,
        TurnStatus::Succeeded,
        "the next tick continued and settled: {settled:?}"
    );
    assert_eq!(reply_text(&settled.output).as_deref(), Some("done"));
    assert_eq!(fixture.model.calls(), 2);
}

// ---------------------------------------------------------------------------
// End to end: a multi-round turn across ticks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_turn_runs_across_ticks_on_its_committed_conversation() {
    let fixture = fixture(&["m1", "m2"]).await;

    let mut outcomes = Vec::new();
    for _ in 0..5 {
        let outcome = fixture.runtime.tick(&fixture.thread).await.expect("tick");
        outcomes.push(outcome);
        if fixture.turn().await.status.terminal() {
            break;
        }
    }

    assert_eq!(
        outcomes.len(),
        3,
        "one tick per model decision, then one that settles: {outcomes:?}"
    );
    assert_eq!(fixture.model.calls(), 3, "no model call was repeated");
    let turn = fixture.turn().await;
    assert_eq!(turn.status, TurnStatus::Succeeded, "{turn:?}");
    assert_eq!(reply_text(&turn.output).as_deref(), Some("done"));

    // The conversation the last tick ran on came from the committed checkpoint: without it the
    // model would have seen no tool results and asked for the same calls again.
    let events = fixture.journal().await;
    let answers = events
        .iter()
        .filter(|event| event.event_type == "tool/result")
        .count();
    assert_eq!(answers, 2, "both tool rounds are in the journal");
    let reply = events
        .iter()
        .filter(|event| event.event_type == FINALIZATION_STARTED)
        .filter_map(|event| event.data().ok())
        .filter_map(|data| data.get("result").cloned())
        .filter_map(|value| reply_text(&value))
        .last();
    assert_eq!(
        reply.as_deref(),
        Some("done"),
        "the reply is in the journal: {events:?}"
    );
}

#[tokio::test]
async fn a_second_turn_on_the_thread_gets_its_own_input() {
    let fixture = fixture(&["m1"]).await;
    for _ in 0..5 {
        if fixture.turn().await.status.terminal() {
            break;
        }
        fixture.runtime.tick(&fixture.thread).await.expect("tick");
    }

    // A new prompt on the same thread: it carries its own input, even though the thread already
    // has a checkpoint from the turn before it.
    let receipt = fixture
        .runtime
        .submit(&fixture.thread, "req-2", json!({ "text": "again" }))
        .await
        .expect("submit");
    let mut ticks = 0;
    loop {
        assert!(ticks < 5, "the second turn must settle");
        ticks += 1;
        fixture.runtime.tick(&fixture.thread).await.expect("tick");
        let turn = fixture
            .backend
            .turn(&fixture.thread, &receipt.turn_id)
            .await
            .expect("turn");
        if turn.status.terminal() {
            assert_eq!(turn.status, TurnStatus::Succeeded);
            break;
        }
    }
    assert!(
        fixture.model.saw("again"),
        "the second turn's input reached the model"
    );
}

#[tokio::test]
async fn the_iteration_budget_carries_across_ticks() {
    // Two iterations of budget, two tool rounds: the pause has to leave the *opened* iteration
    // as the position, or the second tick restarts the budget and the turn needs a third.
    let fixture = fixture_with(&["m1", "m2"], 2).await;

    fixture.runtime.tick(&fixture.thread).await.expect("tick");
    assert_eq!(fixture.model.calls(), 1);
    let outcome = fixture.runtime.tick(&fixture.thread).await.expect("tick");
    assert_eq!(outcome, TickOutcome::Settled);
    assert_eq!(
        fixture.model.calls(),
        2,
        "the second tick spends the second iteration, not a repeat of the first"
    );
    assert_eq!(fixture.turn().await.status, TurnStatus::Succeeded);
}

/// The adapter is registered by harness name, which is what `advance` looks up.
#[tokio::test]
async fn a_thread_without_an_adapter_is_a_non_retryable_failure() -> DurableResult<()> {
    let backend = Arc::new(TxnBackend::in_memory());
    let runtime = DurableRuntime::new(Arc::clone(&backend) as Arc<dyn SessionBackend>, "runtime");
    let key = SessionKey::new("ns", SESSION).expect("session key");
    runtime
        .ensure_session(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new(HARNESS, "def-1", "state-1").expect("binding"),
            metadata: Value::Null,
        })
        .await?;
    let thread = ThreadKey::new(key, "main").expect("thread key");
    runtime
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await?;
    assert_eq!(
        runtime.tick(&thread).await?,
        TickOutcome::Failed { retryable: false }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Control: cancel and pause are different endings
// ---------------------------------------------------------------------------

/// Runs one tick on its own task, so a control request can land while it is in flight.
fn spawn_tick(fixture: &Fixture) -> tokio::task::JoinHandle<DurableResult<TickOutcome>> {
    let runtime = Arc::clone(&fixture.runtime);
    let thread = fixture.thread.clone();
    tokio::spawn(async move { runtime.tick(&thread).await })
}

/// The status a `run/end` fact carries, as a host reads it.
fn run_status(event: &JournalEvent) -> Option<String> {
    event
        .data()
        .ok()?
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The status of the last run that ended in this journal.
fn last_run_status(events: &[JournalEvent]) -> Option<String> {
    events
        .iter()
        .filter(|event| event.event_type == "run/end")
        .filter_map(run_status)
        .last()
}

/// A second runtime, as a host that asks another process's tick to stop: it never shares the
/// writer's owner, so its `cancel` observes the turn instead of re-entering the lease.
fn controller(fixture: &Fixture) -> DurableRuntime {
    DurableRuntime::new(
        Arc::clone(&fixture.backend) as Arc<dyn SessionBackend>,
        "control",
    )
}

#[tokio::test]
async fn a_cancel_before_the_invocation_runs_costs_no_model_call() {
    let fixture = fixture(&["m1"]).await;
    fixture
        .runtime
        .cancel(&fixture.thread, &fixture.turn_id, "cancel-1")
        .await
        .expect("cancel");

    let outcome = fixture.runtime.tick(&fixture.thread).await.expect("tick");

    assert_eq!(outcome, TickOutcome::Idle, "a cancelled turn is not run");
    assert_eq!(fixture.model.calls(), 0, "cancelling spends no model call");
    let turn = fixture.turn().await;
    assert_eq!(turn.status, TurnStatus::Cancelled);
    assert!(
        turn.error.is_none(),
        "a cancellation is an ending, not a failure: {turn:?}"
    );
}

#[tokio::test]
async fn a_cancel_inside_the_invocation_ends_the_tick_as_cancelled() {
    // The model never answers: only the cancel the tick observes can end the invocation.
    let fixture = held_fixture().await;
    let tick = spawn_tick(&fixture);
    fixture.model.await_in_flight().await;

    controller(&fixture)
        .cancel(&fixture.thread, &fixture.turn_id, "cancel-1")
        .await
        .expect("cancel");

    let outcome = tokio::time::timeout(Duration::from_secs(10), tick)
        .await
        .expect("a cancel must end the tick, not leave the caller waiting")
        .expect("tick task")
        .expect("tick");

    assert_eq!(
        outcome,
        TickOutcome::Cancelled,
        "the cancellation must reach the agent, not surface as a refusal or a failure"
    );
    assert_ne!(outcome, TickOutcome::Paused, "a cancel is not a pause");
    assert_eq!(
        fixture.model.calls(),
        1,
        "the in-flight call was not repeated"
    );
    let turn = fixture.turn().await;
    assert_eq!(turn.status, TurnStatus::Cancelled);
    assert!(
        turn.error.is_none(),
        "a cancellation is an ending, not a failure: {turn:?}"
    );
    // The native journal is what a host renders: a run it never sees ending looks like a hang,
    // and one that ends `failed` is read as a refusal.
    let events = fixture.journal().await;
    assert_eq!(
        last_run_status(&events).as_deref(),
        Some("cancelled"),
        "the native run must end as cancelled: {events:?}"
    );
    assert!(
        !events.iter().any(|event| event.event_type == "turn/failed"),
        "a cancellation is not a failed turn: {events:?}"
    );
}

#[tokio::test]
async fn a_pause_inside_the_invocation_is_paused_and_resumable() {
    let fixture = held_fixture().await;
    let tick = spawn_tick(&fixture);
    fixture.model.await_in_flight().await;

    let requested = controller(&fixture)
        .pause(&fixture.thread, &fixture.turn_id, "pause-1")
        .await
        .expect("pause");
    assert_eq!(
        requested.status,
        TurnStatus::Pausing,
        "a live writer only sees the request"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(10), tick)
        .await
        .expect("a pause must end the tick, not leave the caller waiting")
        .expect("tick task")
        .expect("tick");

    assert_eq!(
        outcome,
        TickOutcome::Paused,
        "a pause is a yield the host resumes, not a cancellation"
    );
    assert_ne!(
        outcome,
        TickOutcome::Cancelled,
        "a pause must never be reported as a cancellation"
    );
    let paused = fixture.turn().await;
    assert_eq!(paused.status, TurnStatus::Paused);
    assert!(
        !paused.status.terminal() && paused.error.is_none(),
        "a paused turn is resumable and records no error: {paused:?}"
    );
    let events = fixture.journal().await;
    assert!(
        events.iter().any(|event| event.event_type == "run/end"
            && run_status(event).as_deref() == Some("suspended")),
        "the paused run ends as suspended, not cancelled: {events:?}"
    );

    // The turn continues where it stopped: the next tick spends a fresh model call and settles.
    let resumed = fixture
        .runtime
        .resume(&fixture.thread, &fixture.turn_id, "resume-1")
        .await
        .expect("resume");
    assert_eq!(resumed.status, TurnStatus::Running);
    let again = tokio::time::timeout(
        Duration::from_secs(10),
        fixture.runtime.tick(&fixture.thread),
    )
    .await
    .expect("the resumed tick must not hang")
    .expect("tick");
    assert_eq!(again, TickOutcome::Settled);
    let settled = fixture.turn().await;
    assert_eq!(settled.status, TurnStatus::Succeeded, "{settled:?}");
    assert_eq!(reply_text(&settled.output).as_deref(), Some("done"));
    assert_eq!(fixture.model.calls(), 2, "the pause was not a replay");
}

#[tokio::test]
async fn a_cancelled_turn_is_terminal_and_repeating_the_settle_is_a_no_op() {
    let fixture = fixture(&[]).await;
    // A writer holds the turn, so the cancel is a request it has to observe.
    let lease = fixture
        .backend
        .claim(&fixture.thread, "writer-a", LEASE)
        .await
        .expect("claim")
        .expect("a claimable turn");
    fixture
        .runtime
        .cancel(&fixture.thread, &fixture.turn_id, "cancel-1")
        .await
        .expect("cancel");
    assert_eq!(
        fixture.turn().await.status,
        TurnStatus::Cancelling,
        "only the writer that holds the turn may terminate it"
    );

    for attempt in 1..=2 {
        settle_turn(
            fixture.backend.as_ref(),
            &lease,
            TurnStatus::Cancelled,
            Value::Null,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("settle #{attempt} must be idempotent: {error}"));
    }

    let settled = fixture.turn().await;
    assert_eq!(settled.status, TurnStatus::Cancelled);
    assert!(settled.error.is_none(), "{settled:?}");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.runtime.tick(&fixture.thread),
    )
    .await
    .expect("a settled turn must not keep a caller waiting")
    .expect("tick");
    assert_eq!(outcome, TickOutcome::Idle, "a cancelled turn is never run");
    assert_eq!(fixture.model.calls(), 0);
    assert_eq!(fixture.turn().await.status, TurnStatus::Cancelled);
}
