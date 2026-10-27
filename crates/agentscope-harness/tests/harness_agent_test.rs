//! Tests for the harness agent runtime: `HarnessAgent`, `DistributedStore`, `IsolationScope`.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::Middleware;
use agentscope_core::state::InMemoryAgentStateStore;
use agentscope_harness::{
    ArtifactDeliveryRequest, ArtifactDeliveryResult, ArtifactDeliveryTarget, Channel,
    CompactionMiddleware, CompactionMiddlewareConfig, DefaultWorkspaceSpecApplier,
    DistributedStore, Filesystem, Gateway, HarnessAgent, IsolationScope, LocalFilesystem,
    PlanModeConfig, PlanModeMiddleware, WorkspaceSpec, WorkspaceSpecApplier,
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
    assert!(IsolationScope::Agent
        .namespace(Some("alice"), Some("s1"))
        .is_empty());
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

#[tokio::test]
async fn middleware_transforms_system_prompt() {
    let compaction = CompactionMiddleware::new(CompactionMiddlewareConfig {
        enabled: true,
        max_tokens: 1000,
    });
    let out = compaction
        .on_system_prompt(&RuntimeContext::default(), "You are a helper".into())
        .await;
    assert!(out.contains("[compaction]"));
    assert!(out.contains("1000"));

    let plan = PlanModeMiddleware::new(PlanModeConfig {
        enabled: true,
        plan: "1. read files".into(),
        ..Default::default()
    });
    let out = plan
        .on_system_prompt(&RuntimeContext::default(), "base".into())
        .await;
    assert!(out.contains("Current plan"));
}

#[tokio::test]
async fn workspace_spec_applier_projects_layout() {
    let dir = temp_dir("spec").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let mut spec = WorkspaceSpec::default();
    spec.root = dir;
    spec.files
        .insert("AGENTS.md".to_string(), "# agent guide".to_string());
    DefaultWorkspaceSpecApplier.apply(&spec).await.unwrap();
    let data = fs
        .read(&RuntimeContext::default(), "AGENTS.md", 0, 0)
        .await
        .unwrap();
    assert_eq!(data.content, "# agent guide");
}

#[tokio::test]
async fn gateway_emits_to_channel() {
    let (channel, mut rx) = Channel::new(4);
    let gateway = Gateway::new(channel);
    gateway
        .emit(serde_json::json!({ "type": "hello" }))
        .await
        .unwrap();
    let value = rx.recv().await.unwrap();
    assert_eq!(value["type"], "hello");
}

#[test]
fn harness_agent_builder_memory_and_plan_mode() {
    // Workspace context is enabled by default; plan mode + memory pipeline wire up cleanly.
    let agent = HarnessAgent::builder()
        .name("orchestrated")
        .register_filesystem_tools(true)
        .enable_plan_mode()
        .plan_file_directory("plans")
        .build();
    assert_eq!(agent.name(), "orchestrated");
    // Plan-mode coordinator is present once enabled.
    assert!(agent.plan_mode_manager().is_some());
    // Workspace manager is bound.
    assert!(agent.workspace_manager().is_some());
    // Memory config defaults are applied.
    assert_eq!(agent.memory_config().consolidation_max_tokens, 4_000);
}

#[test]
fn harness_agent_disable_workspace_context() {
    let agent = HarnessAgent::builder()
        .name("bare")
        .disable_workspace_context()
        .disable_memory_hooks()
        .build();
    assert_eq!(agent.name(), "bare");
    assert!(agent.workspace_manager().is_some());
}

#[test]
fn builder_java_parity_setters_round_trip() {
    let agent = HarnessAgent::builder()
        .name("dev")
        .agent_id("dev-123")
        .description("a dev agent")
        .check_running(false)
        .environment("staging")
        .async_tool_timeout(std::time::Duration::from_secs(5))
        .use_legacy_xml_workspace_context(true)
        .build();
    assert_eq!(agent.agent_id(), "dev-123");
    assert_eq!(agent.description(), "a dev agent");
    assert!(!agent.check_running());
    assert_eq!(agent.environment(), "staging");
    assert_eq!(
        agent.async_tool_timeout(),
        Some(std::time::Duration::from_secs(5))
    );
    assert!(agent.use_legacy_xml_workspace_context());
}

#[test]
fn agent_id_defaults_to_name() {
    let agent = HarnessAgent::builder().name("dev").build();
    assert_eq!(agent.agent_id(), "dev");
    assert_eq!(agent.environment(), "prod");
}

#[test]
fn memory_and_web_tools_registered_by_default_and_disableable() {
    let agent = HarnessAgent::builder().name("dev").build();
    let names = agent.toolkit().tool_names();
    assert!(names.contains(&"memory_search".to_string()));
    assert!(names.contains(&"memory_get".to_string()));
    assert!(names.contains(&"memory_save".to_string()));
    assert!(names.contains(&"web_fetch".to_string()));
    assert!(names.contains(&"web_search".to_string()));

    let bare = HarnessAgent::builder()
        .name("dev")
        .disable_memory_tools()
        .disable_web_tools()
        .build();
    let names = bare.toolkit().tool_names();
    assert!(!names.contains(&"memory_search".to_string()));
    assert!(!names.contains(&"web_fetch".to_string()));
    assert!(!names.contains(&"web_search".to_string()));
}

struct NoopArtifactTarget;

#[async_trait::async_trait]
impl ArtifactDeliveryTarget for NoopArtifactTarget {
    async fn deliver(
        &self,
        _runtime_context: &RuntimeContext,
        _request: ArtifactDeliveryRequest,
    ) -> agentscope_harness::Result<ArtifactDeliveryResult> {
        Ok(ArtifactDeliveryResult::success())
    }
}

#[test]
fn artifact_delivery_target_registers_the_tool() {
    let without = HarnessAgent::builder().name("no-artifacts").build();
    assert!(!without
        .toolkit()
        .tool_names()
        .contains(&"deliver_artifact".to_string()));

    let with = HarnessAgent::builder()
        .name("artifacts")
        .artifact_delivery_target(Arc::new(NoopArtifactTarget))
        .build();
    assert!(with
        .toolkit()
        .tool_names()
        .contains(&"deliver_artifact".to_string()));
}

#[test]
fn teams_mode_registers_team_tool() {
    use agentscope_harness::agent::team::{LocalTeamClient, TeamClient, TeamContext, TeamRole};
    let client: Arc<dyn TeamClient> = Arc::new(LocalTeamClient::new());
    let ctx = TeamContext::new("t1", "alice", TeamRole::Lead);
    let agent = HarnessAgent::builder()
        .name("dev")
        .teams_mode(client, ctx)
        .build();
    assert!(agent.toolkit().tool_names().contains(&"team".to_string()));
}

#[test]
fn task_repository_override_is_used() {
    let repo: Arc<dyn agentscope_harness::TaskRepository> =
        Arc::new(agentscope_harness::WorkspaceTaskRepository::new(
            Arc::new(LocalFilesystem::new(std::env::temp_dir())),
            "custom-parent",
        ));
    let agent = HarnessAgent::builder()
        .name("dev")
        .task_repository(Arc::clone(&repo))
        .build();
    assert!(Arc::ptr_eq(agent.task_repository(), &repo));
}
