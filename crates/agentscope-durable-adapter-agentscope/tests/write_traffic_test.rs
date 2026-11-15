//! Write-traffic baseline: how many transactions and bytes one turn costs.
//!
//! The file store keeps one document per session scope and rewrites it whole on every commit,
//! so write traffic is `O(transactions × scope)` — quadratic in facts per turn once the scope
//! holds the whole conversation. This test is the yardstick every storage change has to move:
//! it drives one real turn (one model reply finishing the work, two tool calls) through the
//! same path ACP uses and prints the counters.
//!
//! It lives in its own test binary because the counters are process-wide: no other test in this
//! file may open a `FileStore` without holding [`MEASURE`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{
    AgentExecutionHost, Extensions, ReActAgent, EXECUTION_HOST_CONTEXT_KEY,
    EXECUTION_POSITION_CONTEXT_KEY,
};
use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::journal::{InvocationRecorder, ThreadJournal, RECORDER_CONTEXT_KEY};
use agentscope_core::message::{
    ContentBlock, JsonMap, Msg, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::AgentState;
use agentscope_core::tool::{FunctionTool, Tool, ToolExecutionContext, Toolkit};
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::settle_turn;
use agentscope_durable::store::file::{reset_write_stats, write_stats, FileStore, WriteStats};
use agentscope_durable::store::RecordStore;
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use agentscope_durable_adapter_agentscope::{
    AgentScopeExecutionHost, BackendThreadJournal, STATE_VERSION,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

const SESSION: &str = "write-traffic";
const HARNESS: &str = "durable-host";
const CALL_A: &str = "call-a";
const CALL_B: &str = "call-b";

/// The counters are process-wide, so the tests in this binary take turns.
static MEASURE: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Probe {
    calls: AtomicUsize,
}

/// Asks for two tool calls in a row, then answers in text.
struct TwoToolModel {
    probe: Arc<Probe>,
}

impl TwoToolModel {
    fn answered(messages: &[Msg], id: &str) -> bool {
        messages
            .iter()
            .flat_map(Msg::tool_result_blocks)
            .any(|result| result.id.as_deref() == Some(id))
    }
}

impl Model for TwoToolModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, CoreResult<ChatResponse>> {
        self.probe.calls.fetch_add(1, Ordering::SeqCst);
        let next = if !Self::answered(&messages, CALL_A) {
            Some((CALL_A, "first"))
        } else if !Self::answered(&messages, CALL_B) {
            Some((CALL_B, "second"))
        } else {
            None
        };
        let response = match next {
            Some((id, name)) => ChatResponse::builder()
                .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                    id,
                    name,
                    JsonMap::new(),
                ))])
                .build(),
            None => ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new("both recorded"))])
                .build(),
        };
        Box::pin(futures::stream::once(async move { Ok(response) }))
    }

    fn model_name(&self) -> &str {
        "two-tool"
    }
}

fn echo_tool(name: &'static str) -> Arc<dyn Tool> {
    Arc::new(FunctionTool::new(
        name,
        "returns its name",
        json!({ "type": "object" }),
        Box::new(move |_args: JsonMap, _ctx: ToolExecutionContext| {
            Ok(ToolResultBlock::success(name))
        }),
    ))
}

fn agent(model: Arc<dyn Model>) -> ReActAgent {
    let toolkit = Toolkit::new();
    toolkit.register_tool(echo_tool("first"));
    toolkit.register_tool(echo_tool("second"));
    ReActAgent::builder()
        .name(HARNESS)
        .model(model)
        .max_iters(8)
        .toolkit(toolkit)
        .state(
            AgentState::builder()
                .session_id(SESSION)
                .user_id("alice")
                .build(),
        )
        .build()
        .expect("agent")
}

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

/// A scratch root outside the repository, unique per call.
fn scratch_dir(name: &str) -> PathBuf {
    let seq = SCRATCH.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "agentscope-write-traffic-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

async fn admit(store: &Arc<dyn RecordStore>) -> (ThreadKey, String) {
    let backend = TxnBackend::with_store(Arc::clone(store));
    let key = SessionKey::new("fixture", SESSION).expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new(HARNESS, "def-1", STATE_VERSION).expect("binding"),
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

/// What one turn cost on disk, measured from the store's own counters.
#[derive(Debug, Clone)]
struct Traffic {
    stats: WriteStats,
    journal_events: u64,
    scope_bytes: u64,
    model_calls: usize,
    /// Commits the turn left behind, grouped by the prefix of the commit id the caller chose.
    /// Every entry is one whole-document rewrite, so this is where the remaining traffic lives.
    commits: Vec<(String, usize)>,
}

/// Runs one turn with two tool calls on a fresh file store and reports its write traffic.
async fn one_turn(root: &PathBuf) -> Traffic {
    let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(root).expect("file store"));
    let (thread, _turn_id) = admit(&store).await;
    let backend = Arc::new(TxnBackend::with_store(Arc::clone(&store)));

    let probe = Arc::new(Probe::default());
    reset_write_stats();

    let journal = Arc::new(BackendThreadJournal::new(backend.clone(), thread.clone()));
    let recorder = Arc::new(
        InvocationRecorder::open_with(
            Arc::clone(&journal) as Arc<dyn ThreadJournal>,
            None,
            "traffic",
            None,
        )
        .await
        .expect("recorder"),
    );
    let lease = journal.lease().expect("the recorder acquired a lease");
    let run = backend
        .prepare_run(&lease, &format!("invoke:{}:{}", lease.turn_id, lease.epoch))
        .await
        .expect("run record");
    let host = Arc::new(
        AgentScopeExecutionHost::open(
            Arc::clone(&backend) as Arc<dyn SessionBackend>,
            thread.clone(),
            lease.clone(),
            run.run_id.clone(),
        )
        .await
        .expect("execution host"),
    );
    let position = host.position();
    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));
    extensions.insert(
        EXECUTION_HOST_CONTEXT_KEY,
        host as Arc<dyn AgentExecutionHost>,
    );
    extensions.insert(EXECUTION_POSITION_CONTEXT_KEY, position);

    let agent = agent(Arc::new(TwoToolModel {
        probe: Arc::clone(&probe),
    }));
    let (mut stream, slot) =
        agent.run_stream_with_extensions(vec![Msg::user("go")], None, extensions);
    while let Some(_item) = stream.next().await {}
    let result = slot.lock().expect("result slot").take();
    let reply = result
        .unwrap_or_else(|| Err(Error::Agent("no result message".into())))
        .expect("the turn finishes");
    settle_turn(
        backend.as_ref(),
        &lease,
        TurnStatus::Succeeded,
        serde_json::to_value(&reply).expect("reply json"),
        None,
    )
    .await
    .expect("settle");
    recorder.close().await;
    let stats = write_stats();

    let state = TxnBackend::with_store(Arc::clone(&store))
        .thread_state(&thread)
        .await
        .expect("thread state");
    let scope_bytes = std::fs::read_dir(root.join("scopes"))
        .expect("scopes")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.metadata().map(|meta| meta.len()).unwrap_or(0))
        .sum();
    Traffic {
        stats,
        journal_events: state.journal_position,
        scope_bytes,
        model_calls: probe.calls.load(Ordering::SeqCst),
        commits: commit_kinds(root),
    }
}

/// Groups the commit ids the scope recorded by kind: a journal batch is a bare uuid, everything
/// else names itself (`settle:`, `run:`, …). Each one is a whole-document rewrite, so this is
/// where the remaining traffic lives.
fn commit_kinds(root: &PathBuf) -> Vec<(String, usize)> {
    let mut kinds: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let Ok(scopes) = std::fs::read_dir(root.join("scopes")) else {
        return Vec::new();
    };
    for file in scopes.filter_map(|entry| entry.ok()) {
        let Ok(bytes) = std::fs::read(file.path()) else {
            continue;
        };
        let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        collect_receipts(&document, &mut kinds);
    }
    kinds.into_iter().collect()
}

/// Every `receipts` object in the document, wherever the turn that owns it sits.
fn collect_receipts(value: &Value, kinds: &mut std::collections::BTreeMap<String, usize>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "receipts" {
                    if let Value::Object(receipts) = child {
                        for id in receipts.keys() {
                            let head = id.split(':').next().unwrap_or(id);
                            let kind = if head.len() == 36 { "journal" } else { head };
                            *kinds.entry(kind.to_string()).or_default() += 1;
                        }
                    }
                    continue;
                }
                collect_receipts(child, kinds);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_receipts(item, kinds)),
        _ => {}
    }
}

#[tokio::test]
// The guard stays held across the turn on purpose: the counters it protects are process-wide, and
// `#[tokio::test]` runs on a current-thread runtime, so nothing can deadlock on it.
#[allow(clippy::await_holding_lock)]
async fn a_two_tool_turn_costs_this_many_transactions_and_bytes() {
    let _guard = MEASURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = scratch_dir("baseline");
    let traffic = one_turn(&root).await;
    let _ = std::fs::remove_dir_all(&root);

    eprintln!(
        "BASELINE transactions={} scope_writes={} scope_bytes={} bytes_written={} \
         journal_events={} final_scope_bytes={} model_calls={} commits={:?}",
        traffic.stats.transactions,
        traffic.stats.scope_writes,
        traffic.stats.scope_bytes,
        traffic.stats.bytes_written,
        traffic.journal_events,
        traffic.scope_bytes,
        traffic.model_calls,
        traffic.commits,
    );

    // The turn really did what the measurement claims: two calls, three model replies.
    assert_eq!(traffic.model_calls, 3, "two tool calls and a final answer");
    // A read is a transaction too, so most of them write nothing; what matters is that the turn
    // costs one scope rewrite per durable commit (the journal batches its events already) and
    // that its bytes stay in the same order as the document, not 30 times it.
    assert!(
        traffic.stats.scope_writes < traffic.stats.transactions,
        "reads must not rewrite the scope"
    );
    assert!(traffic.stats.transactions > 0);
    assert!(traffic.stats.bytes_written >= traffic.stats.scope_bytes);
    assert!(
        traffic.stats.scope_bytes < 40 * traffic.scope_bytes,
        "write amplification stays bounded: {} bytes written for a {}-byte document",
        traffic.stats.scope_bytes,
        traffic.scope_bytes
    );
    // Regression guard, from the measurement this turn was written for: before a lease extension
    // stopped rewriting the scope it was 62 scope writes and 3,490,683 bytes. The numbers below
    // leave room for a fact or two more without letting the old shape back in.
    assert!(
        traffic.stats.scope_writes <= 40,
        "{} scope writes for one commit per fact",
        traffic.stats.scope_writes
    );
    assert!(
        traffic.stats.scope_bytes < 2_400_000,
        "{} bytes written for one turn",
        traffic.stats.scope_bytes
    );
}
