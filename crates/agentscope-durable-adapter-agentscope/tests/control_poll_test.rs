//! How often an invocation asks the store whether it was stopped.
//!
//! A model call that produces nothing is polled for control while it streams, and that poll used
//! to run at a fixed 50 ms for as long as the provider hung. It now backs off while its answer
//! stays the same, which is what these numbers are: one idle second must cost a handful of reads,
//! not twenty, and a cancel must still land.
//!
//! The counters are process-global, so this file holds one test: a second one would run beside it
//! and be measured into it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::traffic::{read_stats, reset_read_stats};
use agentscope_durable::types::*;
use agentscope_durable::{DurableRuntime, SessionBackend, TickOutcome};
use agentscope_durable_adapter_agentscope::AgentScopeHarnessAdapter;
use agentscope_harness::HarnessAgent;
use futures::stream::BoxStream;
use serde_json::{json, Value};

const HARNESS: &str = "durable-host";
const SESSION: &str = "poll-session";

/// A model whose only call never answers: the tick stays in flight until control arrives.
struct HeldModel {
    entered: AtomicUsize,
}

impl HeldModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicUsize::new(0),
        })
    }

    /// Waits until a model call is in flight, so the window below is really an idle stream.
    async fn await_in_flight(&self) {
        for _ in 0..500 {
            if self.entered.load(Ordering::SeqCst) > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the model call never started");
    }
}

impl Model for HeldModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::pending())
    }

    fn model_name(&self) -> &str {
        "held"
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentscope-durable-poll-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

#[tokio::test]
async fn an_idle_stream_polls_control_on_a_backing_off_interval() {
    let model = HeldModel::new();
    let root = scratch_dir("agent");
    let build = {
        let model: Arc<dyn Model> = Arc::clone(&model) as Arc<dyn Model>;
        let root = root.clone();
        move || {
            Arc::new(
                HarnessAgent::builder()
                    .name(HARNESS)
                    .model(Arc::clone(&model))
                    .session_id(SESSION)
                    .user_id("alice")
                    .cwd(root.clone())
                    .workspace(root.join("workspace"))
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
    };
    let backend = Arc::new(TxnBackend::in_memory());
    let runtime = Arc::new(
        DurableRuntime::new(Arc::clone(&backend) as Arc<dyn SessionBackend>, "runtime")
            .with_adapter(HARNESS, Arc::new(AgentScopeHarnessAdapter::new(build)))
            .with_lease_ttl(Duration::from_secs(30)),
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

    let tick = {
        let runtime = Arc::clone(&runtime);
        let thread = thread.clone();
        tokio::spawn(async move { runtime.tick(&thread).await })
    };
    model.await_in_flight().await;

    // One second of a stream that produces nothing: the interval a fixed poll would have spent
    // twenty reads on.
    reset_read_stats();
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let idle = read_stats();
    eprintln!("one idle second: {} control reads", idle.control_reads);
    assert!(
        idle.control_reads <= 10,
        "an idle second costs a handful of polls, not a fixed 50 ms each: {idle:?}"
    );
    assert!(
        idle.control_reads >= 2,
        "an idle stream is still polled, or a cancel could never arrive: {idle:?}"
    );

    // And a cancel still lands: the next backoff expires within a second and the request is
    // delivered into the invocation, which ends the tick rather than leaving it waiting.
    let cancel_started = std::time::Instant::now();
    // A second runtime, as a host asking another process's tick to stop: it never shares the
    // writer's owner, so its cancel observes the turn instead of re-entering the lease.
    let controller =
        DurableRuntime::new(Arc::clone(&backend) as Arc<dyn SessionBackend>, "control");
    controller
        .cancel(&thread, &receipt.turn_id, "cancel-1")
        .await
        .expect("cancel");
    let outcome = tokio::time::timeout(Duration::from_secs(10), tick)
        .await
        .expect("a cancel must end the tick")
        .expect("tick task")
        .expect("tick");
    assert_eq!(outcome, TickOutcome::Cancelled, "{outcome:?}");
    assert!(
        cancel_started.elapsed() < Duration::from_secs(5),
        "the backoff bounds how late a cancel can land: {:?}",
        cancel_started.elapsed()
    );
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(turn.status, TurnStatus::Cancelled);
}
