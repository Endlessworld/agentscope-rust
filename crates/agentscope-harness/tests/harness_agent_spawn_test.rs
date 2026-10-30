//! Tests for the `AgentSpawnTool` slice: background spawn, permission scoping,
//! plan-mode propagation and task tracking.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::agent::{Agent, CallableAgent, RuntimeContext};
use agentscope_core::message::{JsonMap, Msg};
use agentscope_core::state::AgentState;
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};

use agentscope_harness::agent::subagent::manager::DefaultAgentManager;
use agentscope_harness::agent::subagent::task::{
    TaskRepository, WorkspaceTaskRepository, TASK_FALLBACK_SESSION,
};
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
        Arc::new(DummyAgent {
            id: self.id.clone(),
            toolkit: Toolkit::default(),
        })
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

fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("as_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn new_repo(dir: &Path) -> Arc<dyn TaskRepository> {
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
    Arc::new(WorkspaceTaskRepository::new(fs, "parent"))
}

/// A tool context scoped to `session` (`sessionId` is the key `tool_runtime` injects).
fn ctx_for_session(session: &str) -> ToolExecutionContext {
    let mut ctx = ToolExecutionContext::default();
    ctx.runtime.insert(
        "sessionId".to_string(),
        serde_json::Value::String(session.to_string()),
    );
    ctx
}

/// Extracts the `task_id` out of a spawn/send response.
fn task_id_of(out: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .unwrap_or_else(|| panic!("no task_id line in output: {out}"))
        .trim()
        .to_string()
}

/// Waits for the task and returns its result (or failure message).
async fn wait_result(repo: &Arc<dyn TaskRepository>, session: &str, task_id: &str) -> String {
    let rc = RuntimeContext::default().with_session_id(session);
    let bg = repo
        .get_task(&rc, session, task_id)
        .await
        .expect("task registered");
    let waiter = Arc::clone(&bg);
    let finished = tokio::task::spawn_blocking(move || waiter.wait_for_completion(5_000))
        .await
        .unwrap();
    assert!(finished, "task {task_id} did not settle");
    bg.get_result()
        .or_else(|| bg.get_error())
        .expect("terminal task has a result or an error")
}

#[tokio::test]
async fn spawn_returns_a_task_id_and_runs_the_child_in_the_background() {
    let dir = workdir("fg");
    let repo = new_repo(&dir);
    let manager = Arc::new(DefaultAgentManager::new(&[entry(
        "researcher",
        SubagentMode::Subagent,
    )]));
    let tool = AgentSpawnTool::new(manager).with_task_repository(Arc::clone(&repo));

    let out = run(
        &tool,
        args(&[
            ("agent", serde_json::json!("researcher")),
            ("prompt", serde_json::json!("hello")),
        ]),
    )
    .await;
    // The spawn must not await the child: the response carries the addressing header and a
    // task id, and the reply arrives later through the task/delivery channel.
    assert!(out.contains("agent_key: agent:researcher:"), "out: {out}");
    assert!(out.contains("started in the background"), "out: {out}");
    assert!(
        !out.contains("dummy reply"),
        "spawn awaited the child: {out}"
    );

    let task_id = task_id_of(&out);
    assert_eq!(
        wait_result(&repo, TASK_FALLBACK_SESSION, &task_id).await,
        "dummy reply from researcher"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn spawn_rejects_unknown_and_permission_scoped_agents() {
    let manager = Arc::new(DefaultAgentManager::new(&[entry(
        "researcher",
        SubagentMode::Subagent,
    )]));
    let scoped =
        Arc::new(AgentSpawnTool::new(manager.clone()).with_allowed_agents(["other".to_string()]));
    let plain = AgentSpawnTool::new(manager);

    // Unknown agent.
    let out = run(
        &plain,
        args(&[
            ("agent", serde_json::json!("ghost")),
            ("prompt", serde_json::json!("x")),
        ]),
    )
    .await;
    assert!(out.contains("No spawnable subagent"), "unknown: {out}");

    // Permission scope denies registered agent.
    let out = run(
        scoped.as_ref(),
        args(&[
            ("agent", serde_json::json!("researcher")),
            ("prompt", serde_json::json!("x")),
        ]),
    )
    .await;
    assert!(out.contains("not in the list of spawnable"), "scope: {out}");

    // PRIMARY-only declarations cannot be spawned.
    let manager2 = Arc::new(DefaultAgentManager::new(&[entry(
        "main",
        SubagentMode::Primary,
    )]));
    let out = run(
        &AgentSpawnTool::new(manager2),
        args(&[
            ("agent", serde_json::json!("main")),
            ("prompt", serde_json::json!("x")),
        ]),
    )
    .await;
    assert!(out.contains("No spawnable subagent"), "primary-only: {out}");
}

#[tokio::test]
async fn plan_mode_is_propagated_into_child_prompt() {
    // The propagation is prompt-level; verify via a factory that echoes the prompt.
    struct EchoFactory;
    struct EchoAgent {
        toolkit: Toolkit,
    }
    #[allow(unused)]
    static EMPTY_TOOLKIT: std::sync::OnceLock<Toolkit> = std::sync::OnceLock::new();
    #[async_trait::async_trait]
    impl CallableAgent for EchoAgent {
        async fn call(&self, messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
            let text = messages
                .last()
                .map(|m| m.text_content())
                .unwrap_or_default();
            Ok(Msg::assistant(if text.contains("<plan-mode-note>") {
                "PLAN"
            } else {
                "NORMAL"
            }))
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
            Arc::new(EchoAgent {
                toolkit: Toolkit::default(),
            })
        }
    }

    let manager = Arc::new(DefaultAgentManager::new(&[SubagentEntry::new(
        "echo",
        "echo",
        Arc::new(EchoFactory),
    )]));
    let dir = workdir("plan");
    let repo = new_repo(&dir);
    let tool = AgentSpawnTool::new(manager).with_task_repository(Arc::clone(&repo));

    // Without plan flag → NORMAL.
    let out = run(
        &tool,
        args(&[
            ("agent", serde_json::json!("echo")),
            ("prompt", serde_json::json!("go")),
        ]),
    )
    .await;
    let task_id = task_id_of(&out);
    assert_eq!(
        wait_result(&repo, TASK_FALLBACK_SESSION, &task_id).await,
        "NORMAL"
    );

    // With plan_mode_active runtime flag → PLAN.
    let mut ctx = ToolExecutionContext::default();
    ctx.runtime.insert(
        "plan_mode_active".to_string(),
        serde_json::Value::Bool(true),
    );
    let block = tool
        .call(
            args(&[
                ("agent", serde_json::json!("echo")),
                ("prompt", serde_json::json!("go")),
            ]),
            ctx,
        )
        .await
        .unwrap();
    let task_id = task_id_of(&block.text_content());
    assert_eq!(
        wait_result(&repo, TASK_FALLBACK_SESSION, &task_id).await,
        "PLAN"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn spawn_registers_trackable_task_and_end_cancels() {
    let dir = workdir("track");
    let repo = new_repo(&dir);

    let manager = Arc::new(DefaultAgentManager::new(&[entry(
        "slow",
        SubagentMode::Subagent,
    )]));
    let tool = Arc::new(AgentSpawnTool::new(manager).with_task_repository(Arc::clone(&repo)));

    let block = tool
        .call(
            args(&[
                ("agent", serde_json::json!("slow")),
                ("prompt", serde_json::json!("work")),
                ("task_id", serde_json::json!("bg-1")),
            ]),
            ctx_for_session("s1"),
        )
        .await
        .unwrap();
    let text = block.text_content();
    assert!(text.contains("task_id: bg-1"), "bg output: {text}");

    // end_spawn finds and cancels the tracked local task (no panic either way).
    assert!(tool.end_spawn("s1", "bg-1"));
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
