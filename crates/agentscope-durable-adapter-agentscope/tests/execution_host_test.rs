//! The durable execution host driving the real core loop.
//!
//! The durable backend supplies the committed facts (iteration position, a produced reply) and
//! the core agent runs against them; nothing here stubs the loop itself.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::agent::{
    execution_host, execution_position, AgentExecutionHost, AgentExecutionPosition,
    ExecutionBoundary, Extensions, FinishContinuation, ReActAgent, RuntimeContext,
    EXECUTION_HOST_CONTEXT_KEY, EXECUTION_POSITION_CONTEXT_KEY,
};
use agentscope_core::error::{Error, Result};
use agentscope_core::journal::{
    InMemoryThreadJournalStore, InvocationRecorder, JournalEvent, ThreadJournalStore,
    RECORDER_CONTEXT_KEY,
};
use agentscope_core::message::{ContentBlock, GenerateReason, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::AgentState;
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use agentscope_durable_adapter_agentscope::{
    pin, AgentScopeExecutionHost, FINALIZATION_COMPLETED, FINALIZATION_STARTED, STATE_VERSION,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Model + agent fixture
// ---------------------------------------------------------------------------

/// A model that counts its calls and remembers the messages each call saw.
struct RecordingModel {
    reply: &'static str,
    calls: AtomicUsize,
    seen: Mutex<Vec<Vec<String>>>,
}

impl RecordingModel {
    fn new(reply: &'static str) -> Arc<Self> {
        Arc::new(Self {
            reply,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn saw(&self, text: &str) -> bool {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|message| message.contains(text))
    }
}

impl Model for RecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push(messages.iter().map(Msg::text_content).collect());
        let reply = self.reply;
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new(reply))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "recording"
    }
}

struct Fixture {
    agent: ReActAgent,
    store: Arc<InMemoryThreadJournalStore>,
    model: Arc<RecordingModel>,
}

fn fixture(reply: &'static str, max_iters: i32) -> Fixture {
    let store = Arc::new(InMemoryThreadJournalStore::new());
    let model = RecordingModel::new(reply);
    let agent = ReActAgent::builder()
        .name("durable-host")
        .model(Arc::clone(&model) as Arc<dyn Model>)
        .state(
            AgentState::builder()
                .session_id("s-durable")
                .user_id("alice")
                .build(),
        )
        .session_log_store(Arc::clone(&store) as Arc<dyn ThreadJournalStore>)
        .max_iters(max_iters)
        .build()
        .expect("agent");
    Fixture {
        agent,
        store,
        model,
    }
}

struct Outcome {
    result: Result<Msg>,
    errors: Vec<Error>,
}

/// Runs one invocation through the streamed entry point, which is the seam that accepts
/// per-run services.
async fn drive(fixture: &Fixture, extensions: Extensions) -> Outcome {
    let mut extensions = extensions;
    let recorder = if extensions.contains(RECORDER_CONTEXT_KEY) {
        None
    } else {
        Some(open_recorder(fixture).await)
    };
    if let Some(recorder) = &recorder {
        extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(recorder));
    }
    let (stream, slot) =
        fixture
            .agent
            .run_stream_with_extensions(vec![Msg::user("go")], None, extensions);
    let mut stream = stream;
    let mut errors = Vec::new();
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            errors.push(error);
        }
    }
    let result = slot
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| Err(Error::Agent("agent produced no result message".into())));
    if let Some(recorder) = recorder {
        recorder.close().await;
    }
    Outcome { result, errors }
}

async fn open_recorder(fixture: &Fixture) -> Arc<InvocationRecorder> {
    let context = RuntimeContext::new();
    let key = fixture.agent.session_key(&context).expect("journal key");
    let log = fixture.store.open(&key, &context).await.expect("journal");
    Arc::new(
        InvocationRecorder::open(log, Some("turn-1"))
            .await
            .expect("recorder"),
    )
}

// ---------------------------------------------------------------------------
// Durable fixture
// ---------------------------------------------------------------------------

async fn running_turn() -> (Arc<TxnBackend>, ThreadKey, Lease) {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = SessionKey::new("ns", "s-durable").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new("durable-host", "def-1", STATE_VERSION)
                .expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    backend
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "owner-a", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("a claimable turn");
    (backend, thread, lease)
}

/// Commits native journal facts into the running turn, as the journal bridge would.
async fn seed(backend: &TxnBackend, thread: &ThreadKey, lease: &Lease, facts: Vec<(&str, Value)>) {
    let state = backend.thread_state(thread).await.expect("thread state");
    let turn = backend.turn(thread, &lease.turn_id).await.expect("turn");
    let mut entries = Vec::new();
    for (index, (event_type, data)) in facts.iter().enumerate() {
        let seq = state.journal_position + 1 + index as u64;
        let event = JournalEvent {
            schema_version: JournalEvent::SCHEMA_VERSION,
            event_id: format!("seed-{seq}"),
            seq,
            occurred_at: 1_700_000_000_000 + seq as i64,
            event_type: (*event_type).to_string(),
            execution_run_id: Some("run-1".to_string()),
            turn_id: Some(lease.turn_id.clone()),
            required: true,
            payload_json: serde_json::to_string(data).expect("payload").into(),
        };
        entries.push(serde_json::to_value(event).expect("entry"));
    }
    let mut commit = Commit::new("seed", turn.revision, TurnStatus::Running);
    commit.journal = Some(JournalAppend {
        expected_position: state.journal_position,
        entries,
    });
    backend.commit(lease, commit).await.expect("seed commit");
}

fn with_host(host: Arc<AgentScopeExecutionHost>) -> Extensions {
    let mut extensions = Extensions::new();
    let position = host.position();
    let host: Arc<dyn AgentExecutionHost> = host;
    extensions.insert(EXECUTION_HOST_CONTEXT_KEY, host);
    extensions.insert(EXECUTION_POSITION_CONTEXT_KEY, position);
    extensions
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_committed_iteration_position_continues_the_budget() {
    let (backend, thread, lease) = running_turn().await;
    seed(
        &backend,
        &thread,
        &lease,
        vec![("step/start", json!({ "stepId": "step-3", "iteration": 3 }))],
    )
    .await;
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("host");
    assert_eq!(
        host.position(),
        AgentExecutionPosition::new(3, 2),
        "the committed step is the position to continue from"
    );

    // `max_iters` = 3 and the committed position is 3: the budget is already spent, so the
    // invocation summarizes without a model call instead of restarting at 0.
    let resumed = fixture("fresh answer", 3);
    let outcome = drive(&resumed, with_host(Arc::new(host))).await;
    let reply = outcome.result.expect("reply");
    assert_eq!(reply.generate_reason(), GenerateReason::MaxIterations);
    assert_eq!(resumed.model.calls(), 0);

    // Control: the same agent without a position starts at 0 and does call the model.
    let fresh = fixture("fresh answer", 3);
    let reply = drive(&fresh, Extensions::new())
        .await
        .result
        .expect("reply");
    assert_eq!(reply.text_content(), "fresh answer");
    assert_eq!(fresh.model.calls(), 1);
}

#[tokio::test]
async fn a_resumed_acting_pass_keeps_its_iteration() {
    let (backend, thread, lease) = running_turn().await;
    // Core opens `step/resumed` (not `step/start`) when an invocation continues the tool calls a
    // previous tick committed: the acting pass is at the *same* iteration as the reasoning step
    // that decided it. Folding only `step/start` moved the position back one iteration, so a
    // crash inside the resumed pass cost an extra model call and repeated the iteration number.
    seed(
        &backend,
        &thread,
        &lease,
        vec![
            ("step/start", json!({ "stepId": "step-2", "iteration": 2 })),
            (
                "step/resumed",
                json!({ "stepId": "step-2-acting", "iteration": 2, "phase": "ACTING" }),
            ),
        ],
    )
    .await;
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("host");
    assert_eq!(
        host.position(),
        AgentExecutionPosition::new(2, 2),
        "the resumed acting pass continues at the iteration it belongs to, not the one before it"
    );
}

#[tokio::test]
async fn a_thread_past_one_journal_page_still_opens() {
    let (backend, thread, lease) = running_turn().await;
    // The backend serves journal reads one page (1,000 entries) at a time, so a thread past that
    // page has to be read in more than one call. The fact that matters is the *last* one, which a
    // one-page read would never reach.
    let mut facts: Vec<(&str, Value)> = (0..1000)
        .map(|index| ("alpha.message", json!({ "index": index })))
        .collect();
    facts.push(("step/start", json!({ "stepId": "step-7", "iteration": 7 })));
    seed(&backend, &thread, &lease, facts).await;
    let state = backend.thread_state(&thread).await.expect("thread state");
    assert_eq!(state.journal_position, 1001, "one fact past a page");

    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("a thread longer than one page still opens");
    assert_eq!(
        host.position(),
        AgentExecutionPosition::new(7, 6),
        "every page is read, not only the first"
    );
}

#[tokio::test]
async fn a_committed_reply_resumes_finalization_without_a_model_call() {
    let (backend, thread, lease) = running_turn().await;
    let produced = Msg::assistant("already answered");
    seed(
        &backend,
        &thread,
        &lease,
        vec![(
            FINALIZATION_STARTED,
            json!({
                "id": "frame-1",
                "position": { "reasoningIteration": 1, "actingIteration": 0 },
                "result": serde_json::to_value(&produced).expect("reply json"),
            }),
        )],
    )
    .await;
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("host");
    assert_eq!(
        host.resume_result().map(|reply| reply.text_content()),
        Some("already answered".to_string())
    );

    let resumed = fixture("fresh answer", 5);
    let reply = drive(&resumed, with_host(Arc::new(host)))
        .await
        .result
        .expect("reply");
    assert_eq!(reply.text_content(), "already answered");
    assert_eq!(resumed.model.calls(), 0, "a replay must not call the model");
}

#[tokio::test]
async fn a_host_pause_is_control_flow_not_a_failure() {
    struct PausingHost;

    #[async_trait]
    impl AgentExecutionHost for PausingHost {
        async fn finish<'a>(
            &'a self,
            _context: &RuntimeContext,
            _result: &Msg,
            _continuation: FinishContinuation<'a>,
        ) -> Result<()> {
            Err(Error::HostPaused)
        }
    }

    let agent = fixture("done", 5);
    let mut extensions = Extensions::new();
    extensions.insert(
        EXECUTION_HOST_CONTEXT_KEY,
        Arc::new(PausingHost) as Arc<dyn AgentExecutionHost>,
    );
    let outcome = drive(&agent, extensions).await;
    assert!(
        matches!(outcome.result, Err(Error::HostPaused)),
        "the pause must reach the caller unchanged, not wrapped as an agent failure"
    );
    assert!(
        !outcome.errors.is_empty(),
        "the pause reaches the event stream"
    );
    for error in &outcome.errors {
        assert!(
            matches!(error, Error::HostPaused),
            "a pause is never reclassified: {error}"
        );
    }
    // The model still produced the reply; only finalization was refused.
    assert_eq!(agent.model.calls(), 1);
}

#[tokio::test]
async fn input_applied_at_the_reasoning_boundary_reaches_the_model() {
    struct InjectingHost {
        injected: AtomicBool,
    }

    #[async_trait]
    impl AgentExecutionHost for InjectingHost {
        async fn before_boundary(
            &self,
            boundary: ExecutionBoundary,
            _context: &RuntimeContext,
            state: &mut AgentState,
        ) -> Result<()> {
            if boundary == ExecutionBoundary::Reasoning
                && !self.injected.swap(true, Ordering::SeqCst)
            {
                state.context_mut().push(Msg::user("injected guidance"));
            }
            Ok(())
        }
    }

    let agent = fixture("done", 5);
    let mut extensions = Extensions::new();
    extensions.insert(
        EXECUTION_HOST_CONTEXT_KEY,
        Arc::new(InjectingHost {
            injected: AtomicBool::new(false),
        }) as Arc<dyn AgentExecutionHost>,
    );
    let reply = drive(&agent, extensions).await.result.expect("reply");
    assert_eq!(reply.text_content(), "done");
    assert!(
        agent.model.saw("injected guidance"),
        "state written by the host hook must be part of the model context"
    );
}

#[tokio::test]
async fn a_delivered_signal_is_folded_once_and_marked_consumed() {
    let (backend, thread, lease) = running_turn().await;
    backend
        .signal(
            &thread,
            &lease.turn_id,
            Signal::new("sig-1", "input:steer", json!("prefer the tests")).expect("signal"),
        )
        .await
        .expect("accept the steer");

    let agent = fixture("done", 5);
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("host");
    let reply = drive(&agent, with_host(Arc::new(host)))
        .await
        .result
        .expect("reply");
    assert_eq!(reply.text_content(), "done");
    assert!(
        agent.model.saw("prefer the tests"),
        "the accepted steer is part of the model context"
    );

    // Delivery is a durable fact: nothing is left to deliver, and the folded message is part of
    // the same commit as its delivery marker.
    let frame = backend.frame(&lease).await.expect("frame");
    assert!(frame.signals.is_empty(), "the signal was delivered");
    let applied = backend
        .events(&thread.session, None, 100)
        .await
        .expect("events")
        .events
        .iter()
        .filter(|event| event.event_type == "signal.applied")
        .count();
    assert_eq!(applied, 1, "one delivery, one fact");
    let state = backend.thread_state(&thread).await.expect("thread state");
    let position = state
        .checkpoint_metadata
        .expect("the folded state is committed")
        .position
        .parse::<u64>()
        .expect("position");
    let checkpoint = backend
        .checkpoint(&thread, position)
        .await
        .expect("checkpoint");
    let saved: AgentState = serde_json::from_value(checkpoint.checkpoint).expect("native state");
    assert_eq!(
        saved
            .context()
            .iter()
            .filter(|message| message.id() == "signal:sig-1")
            .count(),
        1,
        "the folded message is committed exactly once"
    );
}

#[tokio::test]
async fn pin_exposes_the_host_and_its_position_on_the_context() {
    let (backend, thread, lease) = running_turn().await;
    seed(
        &backend,
        &thread,
        &lease,
        vec![("step/start", json!({ "stepId": "step-2", "iteration": 2 }))],
    )
    .await;
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread,
        lease,
        "run-1",
    )
    .await
    .expect("host");
    let mut context = RuntimeContext::new();
    pin(&mut context, Arc::new(host));
    assert!(execution_host(&context).is_some());
    assert_eq!(
        execution_position(&context),
        Some(AgentExecutionPosition::new(2, 1))
    );
    assert!(
        !execution_host(&RuntimeContext::new()).is_some(),
        "an unpinned context carries no host"
    );
    // The completion fact is what makes the next invocation ordinary again.
    assert_eq!(FINALIZATION_COMPLETED, "durable/finalization_completed");
}

/// A reply the host already finalized is not written again, but the host still runs the
/// continuation: it is what publishes the loop state core keeps outside the checkpoint. The
/// early return used to leave the shared state one invocation behind while the durable reply
/// claimed the turn was over.
#[tokio::test]
async fn replaying_a_finalized_reply_still_runs_the_continuation() {
    let (backend, thread, lease) = running_turn().await;
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("host");
    let agent = fixture("done", 5);
    let key_context = RuntimeContext::new();
    let key = agent.agent.session_key(&key_context).expect("journal key");
    let log = agent.store.open(&key, &key_context).await.expect("journal");
    let recorder = Arc::new(
        InvocationRecorder::open(Arc::clone(&log), Some("turn-1"))
            .await
            .expect("recorder"),
    );
    let mut context = RuntimeContext::new();
    context.state = Some(Arc::new(Mutex::new(
        AgentState::builder().session_id("s-durable").build(),
    )));
    context
        .extensions
        .insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));

    let continuations = Arc::new(AtomicUsize::new(0));
    let reply = Msg::assistant("already answered");
    for _ in 0..2 {
        let counter = Arc::clone(&continuations);
        let continuation: FinishContinuation<'_> = Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        host.finish(&context, &reply, continuation)
            .await
            .expect("finish");
    }
    assert_eq!(
        continuations.load(Ordering::SeqCst),
        2,
        "the replay must still run the continuation, or the loop state is never published"
    );

    // Nothing was written twice: one start, one completion.
    recorder.flush().await.expect("flush");
    let events = log.read_after(0, 100).await.expect("events");
    let count = |event_type: &str| {
        events
            .iter()
            .filter(|event| event.event_type == event_type)
            .count()
    };
    assert_eq!(
        count(FINALIZATION_STARTED),
        1,
        "the reply is persisted once"
    );
    assert_eq!(count(FINALIZATION_COMPLETED), 1, "and completed once");
}

/// A checkpoint written under another state version describes another shape: it must be refused
/// instead of being decoded into this one.
#[tokio::test]
async fn a_checkpoint_from_another_state_version_is_not_restored() {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = SessionKey::new("ns", "s-foreign").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new("durable-host", "def-1", "agentscope/state/0")
                .expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    backend
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "owner-a", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("a claimable turn");
    let state = AgentState::builder().session_id("s-durable").build();
    let mut commit = Commit::new("checkpoint", 0, TurnStatus::Running);
    commit.checkpoint = serde_json::to_value(&state).expect("state json");
    backend.commit(&lease, commit).await.expect("checkpoint");

    let error = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread,
        lease,
        "run-1",
    )
    .await
    .err()
    .expect("a foreign state version must be refused");
    assert!(
        error.to_string().contains("state version"),
        "the refusal names the versions: {error}"
    );
}
