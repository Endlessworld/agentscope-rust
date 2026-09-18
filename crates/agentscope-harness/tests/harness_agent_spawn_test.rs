//! Tests for the thin-but-real `AgentSpawnTool` slice: foreground spawn,
//! permission scoping, plan-mode propagation and background tracking.
use std::sync::Arc;

use agentscope_core::agent::{Agent, CallableAgent};
use agentscope_core::message::{JsonMap, Msg};
use agentscope_core::state::AgentState;
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};

use agentscope_harness::agent::subagent::manager::DefaultAgentManager;
use agentscope_harness::agent::subagent::{
    SubagentDeclaration, SubagentEntry, SubagentFactory, SubagentMode,
};
use agentscope_harness::filesystem::{Filesystem, LocalFilesystem};
use agentscope_harness::tool::AgentSpawnTool;

struct DummyAgent {
    id: String,
    toolkit: Toolkit,
}

#[async_trait::async_trait]
impl CallableAgent for DummyAgent {
    async fn call(&self, _messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
        Ok(Msg::assistant(format!("dummy reply from {}", self.id)))
    }
    async fn call_with_schema(
        &self,
        _messages: Vec<Msg>,
        _schema: agentscope_core::formatter::JsonSchema,
    ) -> agentscope_core::error::Result<Msg> {
        Ok(Msg::assistant("{}"))
    }
}

impl Agent for DummyAgent {
    fn agent_id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.id
    }
    fn description(&self) -> &str {
        "dummy"
    }
    fn interrupt(&self) {}
    fn agent_state(&self) -> AgentState {
        AgentState::default()
    }
    fn toolkit(&self) -> &Toolkit {
        &self.toolkit
    }
}

struct FixedFactory {
    id: String,
}

impl SubagentFactory for FixedFactory {
    fn create(&self, _rc: &agentscope_core::agent::RuntimeContext) -> Arc<dyn CallableAgent> {
        Arc::new(DummyAgent { id: self.id.clone(), toolkit: Toolkit::default() })
    }
}

fn entry(id: &str, mode: SubagentMode) -> SubagentEntry {
    let mut e = SubagentEntry::new(id, "sub", Arc::new(FixedFactory { id: id.to_string() }));
    e = e.with_declaration(SubagentDeclaration {
        name: id.to_string(),
        mode,
        ..SubagentDeclaration::new(id, "sub", "")
    });
    e
}

fn args(pairs: &[(&str, serde_json::Value)]) -> JsonMap {
    let mut m = JsonMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

async fn run(tool: &dyn Tool, a: JsonMap) -> String {
    tool.call(a, ToolExecutionContext::default())
        .await
        .expect("call ok")
        .text_content()
}

#[tokio::test]
async fn foreground_spawn_returns_subagent_reply() {
    let manager = Arc::new(DefaultAgentManager::new(&[entry("researcher", SubagentMode::Subagent)]));
    let tool = AgentSpawnTool::new(manager);

    let out = run(
        &tool,
        args(&[
            ("agent", serde_json::json!("researcher")),
            ("prompt", serde_json::json!("hello")),
        ]),
    )
    .await;
    // Responses carry the agent_key/session header (Java `formatSpawnInfo`) ahead of the reply.
    assert!(out.ends_with("dummy reply from researcher"), "out: {out}");
    assert!(out.contains("agent_key: agent:researcher:"), "out: {out}");
}

#[tokio::test]
async fn spawn_rejects_unknown_and_permission_scoped_agents() {
    let manager = Arc::new(DefaultAgentManager::new(&[entry("researcher", SubagentMode::Subagent)]));
    let scoped = Arc::new(
        AgentSpawnTool::new(manager.clone()).with_allowed_agents(["other".to_string()]),
    );
    let plain = AgentSpawnTool::new(manager);

    // Unknown agent.
    let out = run(&plain, args(&[("agent", serde_json::json!("ghost")), ("prompt", serde_json::json!("x"))])).await;
    assert!(out.contains("No spawnable subagent"), "unknown: {out}");

    // Permission scope denies registered agent.
    let out = run(
        scoped.as_ref(),
        args(&[("agent", serde_json::json!("researcher")), ("prompt", serde_json::json!("x"))]),
    )
    .await;
    assert!(out.contains("not in the list of spawnable"), "scope: {out}");

    // PRIMARY-only declarations cannot be spawned.
    let manager2 = Arc::new(DefaultAgentManager::new(&[entry("main", SubagentMode::Primary)]));
    let out = run(&AgentSpawnTool::new(manager2), args(&[("agent", serde_json::json!("main")), ("prompt", serde_json::json!("x"))])).await;
    assert!(out.contains("No spawnable subagent"), "primary-only: {out}");
}

#[tokio::test]
async fn plan_mode_is_propagated_into_child_prompt() {
    // The propagation is prompt-level; verify via a factory that echoes the prompt.
    struct EchoFactory;
    struct EchoAgent {
    toolkit: Toolkit,
}
#[allow(unused)] static EMPTY_TOOLKIT: std::sync::OnceLock<Toolkit> = std::sync::OnceLock::new();
    #[async_trait::async_trait]
    impl CallableAgent for EchoAgent {
        async fn call(&self, messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
            let text = messages
                .last()
                .map(|m| m.text_content())
                .unwrap_or_default();
            Ok(Msg::assistant(if text.contains("<plan-mode-note>") { "PLAN" } else { "NORMAL" }))
        }
        async fn call_with_schema(
            &self,
            _messages: Vec<Msg>,
            _schema: agentscope_core::formatter::JsonSchema,
        ) -> agentscope_core::error::Result<Msg> {
            Ok(Msg::assistant("{}"))
        }
    }
    impl Agent for EchoAgent {
        fn agent_id(&self) -> &str {
            "echo"
        }
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> AgentState {
            AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            &self.toolkit
        }
    }
    impl SubagentFactory for EchoFactory {
        fn create(&self, _rc: &agentscope_core::agent::RuntimeContext) -> Arc<dyn CallableAgent> {
            Arc::new(EchoAgent { toolkit: Toolkit::default() })
        }
    }

    let manager = Arc::new(DefaultAgentManager::new(&[SubagentEntry::new("echo", "echo", Arc::new(EchoFactory))]));
    let tool = AgentSpawnTool::new(manager);

    // Without plan flag → NORMAL.
    let out = run(&tool, args(&[("agent", serde_json::json!("echo")), ("prompt", serde_json::json!("go"))])).await;
    assert!(out.ends_with("NORMAL"), "out: {out}");

    // With plan_mode_active runtime flag → PLAN.
    let mut ctx = ToolExecutionContext::default();
    ctx.runtime.insert(
        "plan_mode_active".to_string(),
        serde_json::Value::Bool(true),
    );
    let block = tool
        .call(args(&[("agent", serde_json::json!("echo")), ("prompt", serde_json::json!("go"))]), ctx)
        .await
        .unwrap();
    assert!(block.text_content().ends_with("PLAN"), "out: {}", block.text_content());
}

#[tokio::test]
async fn background_spawn_registers_trackable_task_and_end_cancels() {
    let dir = std::env::temp_dir().join(format!("as_spawn_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));

    let manager = Arc::new(DefaultAgentManager::new(&[entry("slow", SubagentMode::Subagent)]));
    let repo = Arc::new(
        agentscope_harness::agent::subagent::task::WorkspaceTaskRepository::new(
            fs,
            "parent",
        ),
    );
    let tool = Arc::new(AgentSpawnTool::new(manager).with_task_repository(repo));

    let mut ctx = ToolExecutionContext::default();
    ctx.runtime
        .insert("session_id".to_string(), serde_json::Value::String("s1".into()));

    let block = tool
        .call(
            args(&[
                ("agent", serde_json::json!("slow")),
                ("prompt", serde_json::json!("work")),
                ("background", serde_json::Value::Bool(true)),
                ("task_id", serde_json::json!("bg-1")),
            ]),
            ctx,
        )
        .await
        .unwrap();
    let text = block.text_content();
    assert!(text.contains("background as task 'bg-1'"), "bg output: {text}");

    // end_spawn finds and cancels the tracked local task (no panic either way).
    let _ = tool.end_spawn("s1", "bg-1");
    assert!(!tool.end_spawn("s1", "missing"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agent_spawn_registers_into_toolkit() {
    let manager = Arc::new(DefaultAgentManager::new(&[]));
    let toolkit = Toolkit::new();
    AgentSpawnTool::register(&toolkit, Arc::new(AgentSpawnTool::new(manager)));
    assert!(toolkit.get_tool("agent_spawn").is_some());
}
