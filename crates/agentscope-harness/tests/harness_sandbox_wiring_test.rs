//! Wiring test for the sandbox lifecycle (HARNESS-05): a `SandboxManager` handed to the
//! builder must be installed as `SandboxLifecycleMiddleware`, so every real agent call
//! acquires before and persists + closes after itself.
//!
//! Constructing the middleware directly (see `harness_sandbox_lifecycle_test.rs`) proves the
//! middleware works but not that `build()` installs it; this test drives `stream_events` and
//! asserts the backend counters — including that the sandbox is *closed* (stop + shutdown) on
//! the way out, the fail-closed teardown contract of `SandboxManager::release`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::{RuntimeContext, StreamOptions};
use agentscope_core::error::Result;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_harness::agent::{
    ExecResult, HarnessAgent, InMemorySandboxStateStore, LocalSandboxState, Sandbox,
    SandboxClient, SandboxClientOptions, SandboxContext, SandboxManager, SandboxSnapshotSpec,
    SandboxState, WorkspaceSpec,
};
use futures::StreamExt;

struct TextModel;

#[async_trait::async_trait]
impl Model for TextModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }
    fn model_name(&self) -> &str {
        "text"
    }
}

#[derive(Debug, Default)]
struct Counters {
    created: AtomicUsize,
    resumed: AtomicUsize,
    stopped: AtomicUsize,
    shutdowns: AtomicUsize,
}

#[derive(Debug)]
struct CountingSandbox {
    state: SandboxState,
    counters: Arc<Counters>,
}

#[async_trait::async_trait]
impl Sandbox for CountingSandbox {
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.counters.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn shutdown(&mut self) -> Result<()> {
        self.counters.shutdowns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn is_running(&self) -> bool {
        true
    }
    fn state(&self) -> &SandboxState {
        &self.state
    }
    async fn exec(
        &self,
        _rc: Option<&RuntimeContext>,
        _cmd: &str,
        _timeout: Option<u64>,
    ) -> Result<ExecResult> {
        unreachable!("the wiring test never executes commands")
    }
    async fn persist_workspace(&self) -> Result<Vec<u8>> {
        Ok(vec![])
    }
    async fn hydrate_workspace(&self, _archive: &[u8]) -> Result<()> {
        Ok(())
    }
}

struct CountingClient {
    counters: Arc<Counters>,
}

fn local_state(session: &str) -> SandboxState {
    SandboxState::Local(LocalSandboxState {
        session_id: session.to_string(),
        temp_dir: std::env::temp_dir().to_string_lossy().to_string(),
        workspace_root_ready: false,
        workspace_projection_hash: None,
        metadata: Default::default(),
        snapshot: None,
    })
}

#[async_trait::async_trait]
impl SandboxClient for CountingClient {
    async fn create(
        &self,
        _spec: &WorkspaceSpec,
        _snapshot: Option<&dyn SandboxSnapshotSpec>,
        _options: Option<&SandboxClientOptions>,
    ) -> Result<Box<dyn Sandbox>> {
        self.counters.created.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountingSandbox {
            state: local_state("wired"),
            counters: Arc::clone(&self.counters),
        }))
    }
    async fn resume(&self, state: &SandboxState) -> Result<Box<dyn Sandbox>> {
        self.counters.resumed.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountingSandbox {
            state: state.clone(),
            counters: Arc::clone(&self.counters),
        }))
    }
    async fn delete(&self, _sandbox: &dyn Sandbox) -> Result<()> {
        Ok(())
    }
    fn serialize_state(&self, state: &SandboxState) -> Result<String> {
        serde_json::to_string(state).map_err(|e| agentscope_core::Error::Other(e.to_string()))
    }
    fn deserialize_state(&self, json: &str) -> Result<SandboxState> {
        serde_json::from_str(json).map_err(|e| agentscope_core::Error::Other(e.to_string()))
    }
    fn deserialize_state_with_spec(
        &self,
        json: &str,
        _snapshot: Option<&dyn SandboxSnapshotSpec>,
    ) -> Result<SandboxState> {
        self.deserialize_state(json)
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_sandboxwiring_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

async fn run_once(agent: &HarnessAgent) {
    let events: Vec<_> = agent
        .stream_events(vec![Msg::user("hi")], StreamOptions::default())
        .collect()
        .await;
    assert!(
        !events.iter().any(|event| event.is_err()),
        "agent run surfaced an error: {:?}",
        events.iter().find(|event| event.is_err())
    );
}

#[tokio::test]
async fn builder_installs_the_sandbox_lifecycle_middleware() {
    let dir = temp_dir("lifecycle");
    let counters = Arc::new(Counters::default());
    let client = Arc::new(CountingClient {
        counters: Arc::clone(&counters),
    });
    let manager = Arc::new(SandboxManager::new(
        Arc::clone(&client) as Arc<dyn SandboxClient>,
        Arc::new(InMemorySandboxStateStore::new()),
        "sandbox-wired",
    ));

    let agent = HarnessAgent::builder()
        .name("sandbox-wired")
        .model(Arc::new(TextModel))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .disable_web_tools()
        .sandbox_manager(Arc::clone(&manager))
        .sandbox_context(SandboxContext::builder().build())
        .build();

    run_once(&agent).await;

    assert_eq!(
        counters.created.load(Ordering::SeqCst),
        1,
        "the built agent must acquire a sandbox for a real call"
    );
    assert_eq!(
        counters.stopped.load(Ordering::SeqCst),
        1,
        "release must stop() the sandbox (snapshot persistence)"
    );
    assert_eq!(
        counters.shutdowns.load(Ordering::SeqCst),
        1,
        "release must shutdown() the self-managed sandbox (fail-closed teardown)"
    );

    // A second call proves the lease was released: the same scope must be acquirable again.
    run_once(&agent).await;
    assert_eq!(
        counters.created.load(Ordering::SeqCst) + counters.resumed.load(Ordering::SeqCst),
        2,
        "the second call must acquire (create or resume) a sandbox again"
    );
    assert_eq!(
        counters.stopped.load(Ordering::SeqCst),
        2,
        "every call closes its own sandbox"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
