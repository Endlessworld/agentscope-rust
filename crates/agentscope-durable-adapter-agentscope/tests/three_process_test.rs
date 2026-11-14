//! D7 acceptance: suspend, resume and duplicate across three processes that share only the
//! durable store.
//!
//! Each "process" builds its own [`TxnBackend`], claims its own lease and rebuilds the
//! invocation from committed facts — nothing but the store crosses the boundary (no frame, no
//! checkpoint and no lease value is handed from one process to the next).
//!
//! What the three processes assert, in the plan's terms:
//!
//! * `run` = 2 — the process that stops and the process that continues each own one run record;
//!   the second one names the first as its predecessor.
//! * `tool/result` = 1 and one effect — the call is dispatched before it runs, so the resuming
//!   process executes it exactly once and the stopped process never did.
//! * the conclusion is readable after the halt — a reader that only opens the store sees the
//!   reply, without the process that produced it.
//! * a stop is not a failure — the halted invocation ends as `HostPaused` with the turn still
//!   `RUNNING` and no error recorded.
//! * a duplicate writer runs nothing — the settled turn cannot be claimed again.
//!
//! One deliberate concession is documented where it is asserted: the model is called twice, not
//! once, because the core loop has no acting-phase resume branch (see [`MODEL_CALLS`]).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_core::agent::{
    AgentExecutionHost, ExecutionBoundary, Extensions, FinishContinuation, ReActAgent,
    RuntimeContext, EXECUTION_HOST_CONTEXT_KEY, EXECUTION_POSITION_CONTEXT_KEY,
};
use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::journal::{
    InvocationRecorder, JournalEvent, ThreadJournal, RECORDER_CONTEXT_KEY,
};
use agentscope_core::message::{
    ContentBlock, GenerateReason, JsonMap, Msg, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::AgentState;
use agentscope_core::tool::{FunctionTool, Tool, ToolExecutionContext, Toolkit};
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::settle_turn;
use agentscope_durable::store::file::FileStore;
use agentscope_durable::store::memory::InMemoryStore;
use agentscope_durable::store::RecordStore;
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use agentscope_durable_adapter_agentscope::{
    AgentScopeExecutionHost, BackendThreadJournal, FINALIZATION_COMPLETED, FINALIZATION_STARTED,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

/// The tool call the scripted model asks for; the same id in both processes, so the durable
/// execution record of the suspended call is the one the resume completes.
const CALL_ID: &str = "job";
const TOOL: &str = "record";
const SESSION: &str = "three-process";
const HARNESS: &str = "durable-host";

/// D7 asks for one model call across the whole scenario. The core loop resumes at the reasoning
/// phase only (`AgentExecutionPosition::acting_iteration` has no consumer), so the resuming
/// process has to ask the model for the same tool decision the checkpoint already holds; it is
/// a repeat of a decision, not a replay of a tool effect. Java's `ProcessRecoveryTest` asserts
/// the same two calls. With an acting-phase resume branch the resume would call the model zero
/// times and the total would be one.
const MODEL_CALLS: usize = 2;

// ---------------------------------------------------------------------------
// Model, tool and agent fixtures
// ---------------------------------------------------------------------------

/// Counts the model calls of every process; observation only, never durable state.
#[derive(Default)]
struct Probe {
    calls: AtomicUsize,
}

impl Probe {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// Asks for the tool until the conversation holds its result, then answers in text.
struct ScriptedModel {
    probe: Arc<Probe>,
}

impl Model for ScriptedModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, CoreResult<ChatResponse>> {
        self.probe.calls.fetch_add(1, Ordering::SeqCst);
        let answered = messages
            .iter()
            .flat_map(Msg::tool_result_blocks)
            .any(|result| result.id.as_deref() == Some(CALL_ID));
        let response = if answered {
            ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new("summarized"))])
                .build()
        } else {
            ChatResponse::builder()
                .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                    CALL_ID,
                    TOOL,
                    JsonMap::new(),
                ))])
                .build()
        };
        Box::pin(futures::stream::once(async move { Ok(response) }))
    }

    fn model_name(&self) -> &str {
        "scripted"
    }
}

/// A plain answering model, for the real process boundary test.
struct TextModel(&'static str);

impl Model for TextModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, CoreResult<ChatResponse>> {
        let reply = self.0;
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new(reply))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "text"
    }
}

/// Writes the effect once and returns it as the turn's answer (`return_direct`).
///
/// The effect lives beside the store, not in it: it stands for the side effect an external
/// executor performs, and the durable journal only ever learns about it through a tool result.
fn record_tool(effects: PathBuf) -> Arc<dyn Tool> {
    Arc::new(
        FunctionTool::new(
            TOOL,
            "records the effect",
            json!({ "type": "object" }),
            Box::new(move |_args: JsonMap, _ctx: ToolExecutionContext| {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&effects)
                    .map_err(|error| Error::Tool(format!("cannot record the effect: {error}")))?;
                writeln!(file, "recorded")
                    .map_err(|error| Error::Tool(format!("cannot record the effect: {error}")))?;
                Ok(ToolResultBlock::success("recorded"))
            }),
        )
        .return_direct(true),
    )
}

fn agent(model: Arc<dyn Model>, tool: Option<Arc<dyn Tool>>) -> ReActAgent {
    let mut builder = ReActAgent::builder()
        .name(HARNESS)
        .model(model)
        .max_iters(5)
        .state(
            AgentState::builder()
                .session_id(SESSION)
                .user_id("alice")
                .build(),
        );
    if let Some(tool) = tool {
        let toolkit = Toolkit::new();
        toolkit.register_tool(tool);
        builder = builder.toolkit(toolkit);
    }
    builder.build().expect("agent")
}

// ---------------------------------------------------------------------------
// One process
// ---------------------------------------------------------------------------

/// A host that stops the invocation at the acting boundary, after the call's intent and
/// dispatch are committed and before the tool runs.
///
/// A synchronous tool cannot be interrupted inside its own body, so this is the deterministic
/// stand-in for "the process died with the call in flight": the durable record is exactly the
/// one a death mid-tool leaves — `dispatched`, no result — because the intent and the dispatch
/// are logged before the tool is allowed to run.
struct HaltAtActing {
    inner: Arc<AgentScopeExecutionHost>,
}

#[async_trait]
impl AgentExecutionHost for HaltAtActing {
    async fn before_boundary(
        &self,
        boundary: ExecutionBoundary,
        context: &RuntimeContext,
        state: &mut AgentState,
    ) -> CoreResult<()> {
        self.inner.before_boundary(boundary, context, state).await?;
        if boundary == ExecutionBoundary::Acting {
            return Err(Error::HostPaused);
        }
        Ok(())
    }

    fn resume_result(&self) -> Option<Msg> {
        self.inner.resume_result()
    }

    async fn finish<'a>(
        &'a self,
        context: &RuntimeContext,
        result: &Msg,
        continuation: FinishContinuation<'a>,
    ) -> CoreResult<()> {
        self.inner.finish(context, result, continuation).await
    }
}

/// One invocation, owned by one "process": its own backend, its own lease, its own recorder.
struct Worker {
    backend: Arc<TxnBackend>,
    thread: ThreadKey,
    recorder: Arc<InvocationRecorder>,
    lease: Lease,
    run: RunView,
}

impl Worker {
    /// Claims the thread and prepares the run under it, as `DurableRuntime::advance` does.
    async fn open(store: Arc<dyn RecordStore>, thread: ThreadKey, owner: &str) -> Self {
        let backend = Arc::new(TxnBackend::with_store(store));
        let turn_id = backend
            .thread_state(&thread)
            .await
            .expect("thread state")
            .current_turn_id
            .expect("a turn to execute");
        let journal = Arc::new(BackendThreadJournal::new(backend.clone(), thread.clone()));
        // The recorder is the writer: it acquires the lease, and the run is prepared under it.
        let recorder = Arc::new(
            InvocationRecorder::open_with(
                Arc::clone(&journal) as Arc<dyn ThreadJournal>,
                Some(&turn_id),
                owner,
                None,
            )
            .await
            .expect("recorder"),
        );
        let lease = journal.lease().expect("the recorder acquired a lease");
        let request_id = format!("invoke:{}:{}", lease.turn_id, lease.epoch);
        let run = backend
            .prepare_run(&lease, &request_id)
            .await
            .expect("run record");
        Self {
            backend,
            thread,
            recorder,
            lease,
            run,
        }
    }

    /// Runs one invocation against the facts this process rebuilt from the store.
    async fn drive(&self, agent: &ReActAgent, halt_at_acting: bool) -> CoreResult<Msg> {
        let host = Arc::new(
            AgentScopeExecutionHost::open(
                Arc::clone(&self.backend) as Arc<dyn SessionBackend>,
                self.thread.clone(),
                self.lease.clone(),
                self.run.run_id.clone(),
            )
            .await
            .expect("execution host"),
        );
        let position = host.position();
        let host: Arc<dyn AgentExecutionHost> = if halt_at_acting {
            Arc::new(HaltAtActing { inner: host })
        } else {
            host
        };
        let mut extensions = Extensions::new();
        extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&self.recorder));
        extensions.insert(EXECUTION_HOST_CONTEXT_KEY, Arc::clone(&host));
        extensions.insert(EXECUTION_POSITION_CONTEXT_KEY, position);
        let (mut stream, slot) =
            agent.run_stream_with_extensions(vec![Msg::user("go")], None, extensions);
        while let Some(_item) = stream.next().await {}
        let result = slot.lock().expect("result slot").take();
        result.unwrap_or_else(|| Err(Error::Agent("the agent produced no result message".into())))
    }

    /// Settles the turn with the crate's own ending logic: the effects this run really produced
    /// are applied first, then the reply becomes the turn's output.
    async fn settle(&self, reply: &Msg) {
        settle_turn(
            self.backend.as_ref(),
            &self.lease,
            TurnStatus::Succeeded,
            serde_json::to_value(reply).expect("reply json"),
            None,
        )
        .await
        .expect("settle the turn");
    }

    /// Gives the writer slot back. A process that disappears would leave the lease to expire;
    /// a test cannot hold a lease without a renewer, and both paths leave the same record.
    async fn close(&self) {
        self.recorder.close().await;
    }
}

// ---------------------------------------------------------------------------
// Store readers
// ---------------------------------------------------------------------------

async fn admit(store: &Arc<dyn RecordStore>) -> (ThreadKey, String) {
    let backend = TxnBackend::with_store(Arc::clone(store));
    let key = SessionKey::new("fixture", SESSION).expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new(HARNESS, "def-1", "state-1").expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    (thread, receipt.turn_id)
}

async fn journal_of(backend: &TxnBackend, thread: &ThreadKey) -> Vec<JournalEvent> {
    let state = backend.thread_state(thread).await.expect("thread state");
    if state.journal_position == 0 {
        return Vec::new();
    }
    let limit = state.journal_position.min(1000) as usize;
    backend
        .read_journal(thread, 0, limit)
        .await
        .expect("journal")
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<JournalEvent>(entry).ok())
        .collect()
}

async fn count_events(backend: &TxnBackend, thread: &ThreadKey, event_type: &str) -> usize {
    journal_of(backend, thread)
        .await
        .iter()
        .filter(|event| event.event_type == event_type)
        .count()
}

async fn run_count(backend: &TxnBackend, thread: &ThreadKey, turn_id: &str) -> usize {
    backend
        .runs(thread, turn_id, None, 100)
        .await
        .expect("runs")
        .items
        .len()
}

fn reply_text(value: &Value) -> Option<String> {
    serde_json::from_value::<Msg>(value.clone())
        .ok()
        .map(|message| message.text_content())
}

fn effect_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

/// Reads committed facts without a tokio test attribute: the process-boundary test is a plain
/// `#[test]` because it blocks on a child process.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// A scratch directory outside the repository, so the effect is a real file a second process
/// (or a later test run) could have written.
// A parallel run can observe the same `now()` value, so the counter removes the last window in
// which two scenarios would share a scratch directory and double-count their effects.
static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agentscope-durable-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

// ---------------------------------------------------------------------------
// A: the three-process scenario
// ---------------------------------------------------------------------------

/// What the whole scenario left behind, read from the store only.
struct Report {
    runs: usize,
    tool_results: usize,
    model_calls: usize,
    effects: usize,
    status: TurnStatus,
    output: Option<String>,
    journal_reply: Option<String>,
}

async fn scenario() -> Report {
    let scratch = scratch_dir("three-process");
    let effects = scratch.join("effects.txt");
    // Only the store is shared. Every process below builds its own backend over it.
    let store: Arc<dyn RecordStore> = Arc::new(InMemoryStore::new());
    let probe = Arc::new(Probe::default());

    // The host-side admission that precedes any invocation.
    let (thread, turn_id) = admit(&store).await;

    // --- process 1: decides the call, stops at the acting boundary ------------------
    let first = Worker::open(Arc::clone(&store), thread.clone(), "process-1").await;
    let stopped = first
        .drive(
            &agent(
                Arc::new(ScriptedModel {
                    probe: Arc::clone(&probe),
                }),
                Some(record_tool(effects.clone())),
            ),
            true,
        )
        .await;
    assert!(
        matches!(stopped, Err(Error::HostPaused)),
        "a stop is control flow, not a failure: {stopped:?}"
    );
    first.close().await;

    // --- what the halt left behind, read by a process that never saw process 1 -------
    let reader = TxnBackend::with_store(Arc::clone(&store));
    assert_eq!(run_count(&reader, &thread, &turn_id).await, 1);
    let execution = reader
        .execution(&thread, &format!("{turn_id}:{CALL_ID}"))
        .await
        .expect("the dispatched call is a durable execution record");
    assert!(execution.dispatch_attempted, "the call was dispatched");
    assert!(
        execution.result.is_none(),
        "the tool never reported a result: {execution:?}"
    );
    let halted = reader.turn(&thread, &turn_id).await.expect("turn");
    assert_eq!(
        halted.status,
        TurnStatus::Running,
        "a stop leaves the turn runnable"
    );
    assert!(halted.error.is_none(), "a stop records no error");
    assert_eq!(count_events(&reader, &thread, "tool/result").await, 0);
    assert_eq!(probe.calls(), 1, "the primary model decided the call once");
    assert!(!effects.exists(), "the stopped process never ran the tool");

    // --- process 2: resumes from the committed facts --------------------------------
    let second = Worker::open(Arc::clone(&store), thread.clone(), "process-2").await;
    assert_eq!(
        second.run.previous_run_id.as_deref(),
        Some(first.run.run_id.as_str()),
        "the resume continues the abandoned run instead of starting over"
    );
    let reply = second
        .drive(
            &agent(
                Arc::new(ScriptedModel {
                    probe: Arc::clone(&probe),
                }),
                Some(record_tool(effects.clone())),
            ),
            false,
        )
        .await
        .expect("the resumed invocation finishes");
    assert_eq!(reply.text_content(), "recorded");
    assert_eq!(reply.generate_reason(), GenerateReason::ToolReturnDirect);
    second.settle(&reply).await;
    second.close().await;

    // --- process 3: a duplicate writer must not run anything ------------------------
    let duplicate = TxnBackend::with_store(Arc::clone(&store));
    assert!(
        duplicate
            .claim(&thread, "process-3", Duration::from_secs(60))
            .await
            .expect("claim")
            .is_none(),
        "a settled turn cannot be claimed again"
    );
    let after_duplicate = probe.calls();

    // --- the conclusion, read without any of the three processes --------------------
    let reader = TxnBackend::with_store(Arc::clone(&store));
    let settled = reader.turn(&thread, &turn_id).await.expect("turn");
    let journal = journal_of(&reader, &thread).await;
    let journal_reply = journal
        .iter()
        .find(|event| event.event_type == FINALIZATION_STARTED)
        .and_then(|event| event.data().ok())
        .and_then(|data| data.get("result").cloned())
        .and_then(|result| reply_text(&result));
    Report {
        runs: run_count(&reader, &thread, &turn_id).await,
        tool_results: count_events(&reader, &thread, "tool/result").await,
        model_calls: after_duplicate,
        effects: effect_lines(&effects),
        status: settled.status,
        output: reply_text(&settled.output),
        journal_reply,
    }
}

#[tokio::test]
async fn suspend_resume_and_duplicate_share_only_the_store() {
    let report = scenario().await;
    assert_eq!(
        report.runs, 2,
        "one run per process that existed, no overwrite"
    );
    assert_eq!(report.tool_results, 1, "one tool result in the journal");
    assert_eq!(report.effects, 1, "the side effect happened once");
    assert_eq!(report.model_calls, MODEL_CALLS);
    assert_eq!(report.status, TurnStatus::Succeeded);
    assert_eq!(report.output.as_deref(), Some("recorded"));
    assert_eq!(report.journal_reply.as_deref(), Some("recorded"));
}

/// Java `ProcessRecoveryTest` (a4306d63d) asserts two model calls and one effect across a
/// restarted process: the resuming process repeats the tool *decision*, it does not replay the
/// effect. A single model call would need an acting-phase resume entry, which the Rust loop does
/// not have yet (see `DURABLE_MIGRATION_PLAN.md`, M3 gaps).
#[tokio::test]
async fn a_resumed_process_repeats_the_tool_decision_but_not_its_effect() {
    let report = scenario().await;
    assert_eq!(
        report.model_calls, 2,
        "Java parity: the decision is re-asked, not replayed"
    );
    assert_eq!(report.effects, 1, "the tool effect runs once");
    assert_eq!(report.tool_results, 1, "one tool result in the journal");
    assert_eq!(report.runs, 2, "one run per process");
}

// ---------------------------------------------------------------------------
// B: a real process boundary
// ---------------------------------------------------------------------------

/// Child role: writes a whole invocation into a file store and exits.
///
/// The parent runs the same binary, so without `DURABLE_WRITER_ROOT` this test does nothing.
#[test]
fn written_facts_outlive_the_writer_process() {
    let Ok(root) = std::env::var("DURABLE_WRITER_ROOT") else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(&root).expect("file store"));
        let (thread, _) = admit(&store).await;
        let worker = Worker::open(Arc::clone(&store), thread.clone(), "writer-process").await;
        let reply = worker
            .drive(
                &agent(Arc::new(TextModel("answered by the writer")), None),
                false,
            )
            .await
            .expect("the writer finishes its turn");
        worker.settle(&reply).await;
        worker.close().await;
    });
}

#[test]
fn the_store_keeps_the_state_after_the_writer_process_exits() {
    let root = scratch_dir("writer-process");
    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "written_facts_outlive_the_writer_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DURABLE_WRITER_ROOT", &root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the writer process");
    let mut child = child;
    // Bounded wait instead of a fixed sleep; the child is deterministic and exits on its own.
    let deadline = Instant::now() + Duration::from_secs(120);
    while child.try_wait().expect("poll the writer process").is_none() {
        assert!(
            Instant::now() < deadline,
            "the writer process did not exit in time"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("collect the writer output");
    assert!(
        output.status.success(),
        "the writer process failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Only now is the store opened here: the file store locks per transaction, and reading a
    // root another live process is still writing to would test locking, not durability.
    let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(&root).expect("file store"));
    let backend = TxnBackend::with_store(Arc::clone(&store));
    let key = SessionKey::new("fixture", SESSION).expect("session key");
    let thread = ThreadKey::new(key.clone(), "main").expect("thread key");

    let session = block_on(backend.session(&key)).expect("session on disk");
    assert_eq!(session.key, key);
    let turns = block_on(backend.turns(&thread, None, 10)).expect("turns");
    let turn = turns.items.first().expect("the writer admitted a turn");
    assert_eq!(turn.status, TurnStatus::Succeeded);
    assert_eq!(
        reply_text(&turn.output).as_deref(),
        Some("answered by the writer"),
        "the reply the writer produced outlived it"
    );
    let runs = block_on(backend.runs(&thread, &turn.turn_id, None, 10)).expect("runs");
    assert_eq!(runs.items.len(), 1);

    // The conversation itself is in the checkpoint, not only the reply field.
    let state = block_on(backend.thread_state(&thread)).expect("thread state");
    let position = state
        .checkpoint_metadata
        .expect("the writer committed a checkpoint")
        .position
        .parse::<u64>()
        .expect("checkpoint position");
    let checkpoint = block_on(backend.checkpoint(&thread, position)).expect("checkpoint");
    let saved: AgentState =
        serde_json::from_value(checkpoint.checkpoint.clone()).expect("native state");
    assert_eq!(
        saved.context().last().map(Msg::text_content).as_deref(),
        Some("answered by the writer")
    );
    // And the journal carries the finalization the writer committed.
    let journal = block_on(journal_of(&backend, &thread));
    assert!(journal
        .iter()
        .any(|event| event.event_type == FINALIZATION_COMPLETED));
}
