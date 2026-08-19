//! Tests for the harness agent runtime: `HarnessAgent`, `DistributedStore`, `IsolationScope`.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::Middleware;
use agentscope_core::state::InMemoryAgentStateStore;
use agentscope_harness::{
    Channel, CompactionMiddleware, CompactionMiddlewareConfig, DefaultWorkspaceSpecApplier, DistributedStore,
    Filesystem, Gateway, HarnessAgent, IsolationScope, LocalFilesystem, PlanModeConfig,
    PlanModeMiddleware, WorkspaceSpec, WorkspaceSpecApplier,
};

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("agentscope_agent_{tag}_{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}

#[test]
fn isolation_scope_namespace() {
    // User scope prefixes with userId, falling back to sessionId.
    assert_eq!(
        IsolationScope::User.namespace(Some("alice"), Some("s1")),
        vec!["alice".to_string()]
    );
    assert_eq!(
        IsolationScope::User.namespace(None, Some("s1")),
        vec!["s1".to_string()]
    );
    // Session scope uses sessionId only.
    assert_eq!(
        IsolationScope::Session.namespace(Some("alice"), Some("s1")),
        vec!["s1".to_string()]
    );
    assert!(IsolationScope::Session.namespace(None, None).is_empty());
    // Agent / Global produce no prefix.
    assert!(IsolationScope::Agent.namespace(Some("alice"), Some("s1")).is_empty());
    assert!(IsolationScope::Global.namespace(None, None).is_empty());
}

#[test]
fn harness_agent_builder_wires_tools() {
    let agent = HarnessAgent::builder()
        .name("dev")
        .register_filesystem_tools(true)
        .build();
    assert_eq!(agent.name(), "dev");
    let names = agent.toolkit().tool_names();
    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"write_file".to_string()));
}

#[test]
fn distributed_store_builder() {
    let store = InMemoryAgentStateStore::new();
    let ds = DistributedStore::builder()
        .agent_state_store(Arc::new(store))
        .isolation_scope(IsolationScope::Session)
        .build();
    assert!(ds.agent_state_store.is_some());
    assert_eq!(ds.isolation_scope, IsolationScope::Session);
}

#[test]
fn middleware_transforms_system_prompt() {
    let compaction = CompactionMiddleware::new(CompactionMiddlewareConfig {
        enabled: true,
        max_tokens: 1000,
    });
    let out = compaction.on_system_prompt(&RuntimeContext::default(), "You are a helper".into());
    assert!(out.contains("[compaction]"));
    assert!(out.contains("1000"));

    let plan = PlanModeMiddleware::new(PlanModeConfig {
        enabled: true,
        plan: "1. read files".into(),
    });
    let out = plan.on_system_prompt(&RuntimeContext::default(), "base".into());
    assert!(out.contains("Current plan"));
}

#[tokio::test]
async fn workspace_spec_applier_projects_layout() {
    let dir = temp_dir("spec").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let mut spec = WorkspaceSpec::default();
    spec.root = dir;
    spec.files.insert("AGENTS.md".to_string(), "# agent guide".to_string());
    DefaultWorkspaceSpecApplier
        .apply(&spec)
        .await
        .unwrap();
    let data = fs.read(&RuntimeContext::default(), "AGENTS.md", 0, 0).await.unwrap();
    assert_eq!(data.content, "# agent guide");
}

#[tokio::test]
async fn gateway_emits_to_channel() {
    let (channel, mut rx) = Channel::new(4);
    let gateway = Gateway::new(channel);
    gateway.emit(serde_json::json!({ "type": "hello" })).await.unwrap();
    let value = rx.recv().await.unwrap();
    assert_eq!(value["type"], "hello");
}
