//! `AgentState` ↔ durable checkpoint: what a boundary writes, when it is deduplicated, and what
//! a restart rebuilds from it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::{
    execution_position, AgentExecutionHost, ExecutionBoundary, Extensions, ReActAgent,
    RuntimeContext, EXECUTION_HOST_CONTEXT_KEY, EXECUTION_POSITION_CONTEXT_KEY,
};
use agentscope_core::error::{Error, Result};
use agentscope_core::journal::{
    InMemoryThreadJournalStore, InvocationRecorder, JournalEvent, ThreadJournal,
    ThreadJournalStore, Writer, RECORDER_CONTEXT_KEY,
};
use agentscope_core::message::{ContentBlock, GenerateReason, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::permission::{PermissionContextState, PermissionMode};
use agentscope_core::state::{
    AgentState, PlanModeContextState, TaskContextState, ToolContextState,
};
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use agentscope_durable_adapter_agentscope::{
    pin, AgentScopeExecutionHost, BackendThreadJournal, STATE_VERSION,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Durable fixture: one thread, a journal bridge and the writer it acquired
// ---------------------------------------------------------------------------

struct Fixture {
    backend: Arc<TxnBackend>,
    thread: ThreadKey,
    journal: Arc<BackendThreadJournal>,
    recorder: Arc<InvocationRecorder>,
    lease: Lease,
}

async fn fixture() -> Fixture {
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
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let journal = Arc::new(BackendThreadJournal::new(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
    ));
    // The recorder owns the writer lease; the host shares it instead of claiming again.
    let log: Arc<dyn ThreadJournal> = journal.clone();
    let recorder = Arc::new(
        InvocationRecorder::open(log, Some(&receipt.turn_id))
            .await
            .expect("recorder"),
    );
    let lease = journal.lease().expect("the recorder holds the writer");
    Fixture {
        backend,
        thread,
        journal,
        recorder,
        lease,
    }
}

impl Fixture {
    async fn open(&self, run_id: &str) -> AgentScopeExecutionHost {
        let backend: Arc<dyn SessionBackend> = Arc::clone(&self.backend) as Arc<dyn SessionBackend>;
        AgentScopeExecutionHost::open(backend, self.thread.clone(), self.lease.clone(), run_id)
            .await
            .expect("host")
    }

    fn boundary_context(&self) -> RuntimeContext {
        let mut context = RuntimeContext::new();
        context
            .extensions
            .insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&self.recorder));
        context
    }

    fn writer(&self) -> Writer {
        Writer {
            owner: self.lease.owner.clone(),
            epoch: self.lease.epoch,
        }
    }

    async fn positions(&self) -> Vec<u64> {
        self.backend
            .checkpoints(&self.thread, 0, 100)
            .await
            .expect("checkpoints")
    }

    /// Commits one native journal batch, as the recorder would.
    async fn commit_batch(&self, batch: &str, facts: &[(&str, Value)]) {
        let head = self.journal.head().await.expect("head");
        let events: Vec<JournalEvent> = facts
            .iter()
            .enumerate()
            .map(|(index, (event_type, payload))| {
                let seq = head.seq + 1 + index as u64;
                JournalEvent::from_payload(
                    format!("{batch}-{seq}"),
                    seq,
                    1_700_000_000_000 + seq as i64,
                    *event_type,
                    None,
                    Some(self.lease.turn_id.clone()),
                    true,
                    payload,
                )
                .expect("journal event")
            })
            .collect();
        self.journal
            .commit(&self.writer(), batch, head.seq, &events)
            .await
            .expect("journal batch");
    }

    /// Commits one native journal batch that carries `state`, as a checkpoint does.
    async fn commit_state(&self, batch: &str, state: &AgentState) {
        self.commit_batch(
            batch,
            &[(
                "state/checkpoint",
                json!({
                    "projectorVersion": 1,
                    "stateJson": state.to_json().expect("state json"),
                    "reason": "durable_boundary",
                }),
            )],
        )
        .await;
    }
}

fn sample_state() -> AgentState {
    AgentState::builder()
        .session_id("s-durable")
        .user_id("alice")
        .summary("one flaky test already isolated")
        .reply_id("reply-1")
        .cur_iter(3)
        .add_message(Msg::user("find the flaky test"))
        .add_message(Msg::assistant("checking the last run"))
        .add_message(Msg::user("run it twice"))
        .permission_context(PermissionContextState {
            mode: PermissionMode::AcceptEdits,
            ..Default::default()
        })
        .tool_context(ToolContextState {
            discovered_tools: vec!["web_search".to_string()],
            ..Default::default()
        })
        .tasks_context(TaskContextState {
            revision: 7,
            ..Default::default()
        })
        .plan_mode_context(PlanModeContextState {
            plan_active: true,
            current_plan_file: Some("plan.md".to_string()),
        })
        .build()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unchanged_state_does_not_create_a_new_checkpoint() {
    let fixture = fixture().await;
    let host = fixture.open("run-1").await;
    let mut state = sample_state();
    host.before_boundary(
        ExecutionBoundary::Reasoning,
        &fixture.boundary_context(),
        &mut state,
    )
    .await
    .expect("boundary checkpoint");
    assert_eq!(
        fixture.positions().await,
        vec![1],
        "the boundary commits a state"
    );
    let written = fixture
        .backend
        .thread_state(&fixture.thread)
        .await
        .expect("thread state")
        .journal_position;

    // A batch without a state carries the committed checkpoint forward instead of dropping it.
    fixture
        .commit_batch(
            "batch-other",
            &[(
                "message/user",
                json!({ "message": Msg::user("one more instruction") }),
            )],
        )
        .await;
    assert_eq!(
        fixture.positions().await,
        vec![1],
        "carrying it adds no position"
    );

    // A restart that re-writes the same snapshot: identical content, so no new position.
    fixture.commit_state("batch-same", &state).await;
    assert_eq!(
        fixture.positions().await,
        vec![1],
        "identical content keeps the committed position"
    );
    let after = fixture
        .backend
        .thread_state(&fixture.thread)
        .await
        .expect("thread state");
    assert_eq!(
        after.journal_position,
        written + 2,
        "both writes really did commit"
    );
    assert_eq!(after.checkpoint_metadata.expect("metadata").position, "1");
}

#[tokio::test]
async fn a_changed_state_creates_a_new_checkpoint() {
    let fixture = fixture().await;
    let host = fixture.open("run-1").await;
    let mut state = sample_state();
    host.before_boundary(
        ExecutionBoundary::Reasoning,
        &fixture.boundary_context(),
        &mut state,
    )
    .await
    .expect("boundary checkpoint");
    assert_eq!(fixture.positions().await, vec![1]);

    let mut changed = state.clone();
    changed.context_mut().push(Msg::user("and again"));
    fixture.commit_state("batch-changed", &changed).await;

    assert_eq!(
        fixture.positions().await,
        vec![1, 2],
        "changed content adds a position"
    );
    let view = fixture
        .backend
        .checkpoint(&fixture.thread, 2)
        .await
        .expect("checkpoint");
    let stored: AgentState = serde_json::from_value(view.checkpoint).expect("state");
    assert_eq!(stored, changed, "the new position is the new state");
}

#[tokio::test]
async fn a_reopened_host_rebuilds_the_committed_state() {
    let fixture = fixture().await;
    let state = sample_state();
    fixture.commit_state("batch-1", &state).await;

    let host = fixture.open("run-2").await;
    let rebuilt = host.state().expect("a committed state");
    assert_eq!(rebuilt, &state, "every field round-trips");
    assert_eq!(rebuilt.cur_iter(), 3);
    assert_eq!(rebuilt.context().len(), 3);
    assert_eq!(
        rebuilt
            .context()
            .iter()
            .map(Msg::text_content)
            .collect::<Vec<_>>(),
        vec![
            "find the flaky test",
            "checking the last run",
            "run it twice"
        ]
    );
    assert_eq!(rebuilt.user_id(), Some("alice"));
    assert_eq!(rebuilt.summary(), "one flaky test already isolated");
    assert_eq!(
        rebuilt.permission_context().mode,
        PermissionMode::AcceptEdits
    );
    assert_eq!(rebuilt.tool_context().discovered_tools, vec!["web_search"]);
    assert_eq!(rebuilt.tasks_context().revision, 7);
    assert!(rebuilt.plan_mode_context().plan_active());
    assert_eq!(
        rebuilt.plan_mode_context().current_plan_file(),
        Some("plan.md")
    );
}

#[tokio::test]
async fn a_reopened_host_keeps_the_committed_iteration_budget() {
    let fixture = fixture().await;
    // Only a checkpoint carries the progress: no `step/start` fact was ever committed.
    fixture.commit_state("batch-1", &sample_state()).await;

    let host = Arc::new(fixture.open("run-2").await);
    assert_eq!(
        host.position().reasoning_iteration,
        3,
        "the rebuilt state, not a constant, sets the position"
    );

    let mut context = RuntimeContext::new();
    pin(&mut context, Arc::clone(&host));
    assert_eq!(
        execution_position(&context).map(|position| position.reasoning_iteration),
        Some(3),
        "the pinned position is the committed one"
    );

    // `max_iters` = 3 is already spent: the invocation summarizes instead of restarting at 0.
    let agent = agent("fresh answer", 3);
    let mut extensions = Extensions::new();
    extensions.insert(
        EXECUTION_HOST_CONTEXT_KEY,
        Arc::clone(&host) as Arc<dyn AgentExecutionHost>,
    );
    extensions.insert(EXECUTION_POSITION_CONTEXT_KEY, host.position());
    let reply = drive(&agent, extensions).await.expect("reply");
    assert_eq!(reply.generate_reason(), GenerateReason::MaxIterations);
    assert_eq!(
        agent.model.calls(),
        0,
        "a resumed budget is not reset to zero"
    );
}

// ---------------------------------------------------------------------------
// Agent fixture: the real loop, driven with the durable host pinned
// ---------------------------------------------------------------------------

struct RecordingModel {
    reply: &'static str,
    calls: AtomicUsize,
}

impl RecordingModel {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Model for RecordingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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

struct AgentFixture {
    agent: ReActAgent,
    store: Arc<InMemoryThreadJournalStore>,
    model: Arc<RecordingModel>,
}

fn agent(reply: &'static str, max_iters: i32) -> AgentFixture {
    let store = Arc::new(InMemoryThreadJournalStore::new());
    let model = Arc::new(RecordingModel {
        reply,
        calls: AtomicUsize::new(0),
    });
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
    AgentFixture {
        agent,
        store,
        model,
    }
}

async fn drive(fixture: &AgentFixture, extensions: Extensions) -> Result<Msg> {
    let mut extensions = extensions;
    let context = RuntimeContext::new();
    let key = fixture.agent.session_key(&context).expect("journal key");
    let log = fixture.store.open(&key, &context).await.expect("journal");
    let recorder = Arc::new(
        InvocationRecorder::open(log, Some("turn-1"))
            .await
            .expect("recorder"),
    );
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));
    let (mut stream, slot) =
        fixture
            .agent
            .run_stream_with_extensions(vec![Msg::user("go")], None, extensions);
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            return Err(error);
        }
    }
    let result = slot
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| Err(Error::Agent("agent produced no result message".into())));
    recorder.close().await;
    result
}
