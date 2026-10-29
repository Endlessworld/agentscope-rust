//! Tests for `SandboxLifecycleMiddleware`.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::middleware::{AgentInput, Middleware, Next};
use futures::stream::StreamExt;

use agentscope_harness::agent::middleware::SandboxLifecycleMiddleware;
use agentscope_harness::agent::{
    ExecResult, InMemorySandboxStateStore, LocalSandboxState, Sandbox, SandboxClient,
    SandboxClientOptions, SandboxContext, SandboxManager, SandboxSnapshotSpec, SandboxState,
    WorkspaceSpec,
};

#[derive(Debug)]
struct CountingSandbox {
    state: SandboxState,
    started: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Sandbox for CountingSandbox {
    async fn start(&mut self) -> Result<()> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
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
        unreachable!()
    }
    async fn persist_workspace(&self) -> Result<Vec<u8>> {
        Ok(vec![])
    }
    async fn hydrate_workspace(&self, _archive: &[u8]) -> Result<()> {
        Ok(())
    }
}

struct CountingClient {
    created: std::sync::atomic::AtomicUsize,
    sandbox_started: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl SandboxClient for CountingClient {
    async fn create(
        &self,
        _spec: &WorkspaceSpec,
        _snapshot: Option<&dyn SandboxSnapshotSpec>,
        _options: Option<&SandboxClientOptions>,
    ) -> Result<Box<dyn Sandbox>> {
        self.created
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(CountingSandbox {
            state: SandboxState::Local(LocalSandboxState {
                session_id: "lc-test".into(),
                temp_dir: std::env::temp_dir().to_string_lossy().to_string(),
                workspace_root_ready: false,
                workspace_projection_hash: None,
                metadata: Default::default(),
                snapshot: None,
            }),
            started: Arc::clone(&self.sandbox_started),
        }))
    }
    async fn resume(&self, _state: &SandboxState) -> Result<Box<dyn Sandbox>> {
        Ok(Box::new(CountingSandbox {
            state: _state.clone(),
            started: Arc::clone(&self.sandbox_started),
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

#[tokio::test]
async fn acquires_and_persists_around_call() {
    let client = Arc::new(CountingClient {
        created: std::sync::atomic::AtomicUsize::new(0),
        sandbox_started: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let store = Arc::new(InMemorySandboxStateStore::new());
    let manager = Arc::new(SandboxManager::new(
        Arc::clone(&client) as Arc<dyn SandboxClient>,
        store.clone(),
        "agent-lc",
    ));
    let context = SandboxContext::builder().build();
    let mw = SandboxLifecycleMiddleware::new(Arc::clone(&manager), context);

    let ctx = RuntimeContext::default().with_session_id("s1");
    let input = AgentInput::new(vec![]);
    let next: Next<AgentInput> = Box::new(move |_in| {
        futures::stream::iter(vec![Ok(AgentEvent::new(AgentEventType::AgentEnd))]).boxed()
    });

    let events: Vec<_> = mw.on_agent(&ctx, input, next).collect().await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        client.created.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "sandbox should be created once"
    );
}
