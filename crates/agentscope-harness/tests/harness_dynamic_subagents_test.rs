//! Integration tests for the dynamic subagent mechanism: `AgentSpecLoader`
//! (.md front-matter parsing + directory/filesystem scanning) and
//! `DynamicSubagentsMiddleware` (two-layer reload + merge + manager refresh).
use std::sync::Arc;

use agentscope_core::agent::{Agent, CallableAgent, RuntimeContext};
use agentscope_core::event::AgentEvent;
use agentscope_core::formatter::JsonSchema;
use agentscope_core::message::Msg;
use agentscope_core::middleware::ReasoningInput;
use agentscope_core::model::GenerateOptions;
use agentscope_core::state::AgentState;
use agentscope_core::tool::Toolkit;
use agentscope_core::Middleware;
use futures::StreamExt;
use agentscope_harness::{
    load_from_directory, load_from_filesystem, parse as parse_decl, DefaultAgentManager,
    DynamicSubagentsMiddleware, Filesystem, LocalFilesystem, RemoteAskPolicy, SubagentDeclaration,
    SubagentEntry, SubagentFactory, SubagentMode, SubagentWorkspaceMode, WorkspaceTaskRepository,
};

fn ctx() -> RuntimeContext {
    RuntimeContext::default()
}

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_dynsub_{tag}_{}",
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}

fn new_repo(dir: &std::path::Path) -> WorkspaceTaskRepository {
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
    WorkspaceTaskRepository::new(fs, "parent")
}

/// Minimal callable agent used to verify that a dynamically-declared subagent can
/// be materialized through the manager without wiring a real model.
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
        _schema: JsonSchema,
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
        "dummy subagent"
    }
    fn interrupt(&self) {}
    fn agent_state(&self) -> AgentState {
        AgentState::default()
    }
    fn toolkit(&self) -> &Toolkit {
        &self.toolkit
    }
}

struct NoopFactory;
impl SubagentFactory for NoopFactory {
    fn create(&self, _: &RuntimeContext) -> Arc<dyn CallableAgent> {
        Arc::new(DummyAgent {
            id: String::new(),
            toolkit: Toolkit::default(),
        })
    }
}

fn entry(name: &str, desc: &str) -> SubagentEntry {
    SubagentEntry::new(name, desc, Arc::new(NoopFactory))
}

/// Standard factory materializer: always returns a no-op factory.
fn builder() -> impl Fn(&SubagentDeclaration) -> Option<Arc<dyn SubagentFactory>> + Send + Sync {
    |_decl| Some(Arc::new(NoopFactory))
}

// ---------------------------------------------------------------------------
// AgentSpecLoader.parse
// ---------------------------------------------------------------------------

const FULL_MD: &str = r#"---
description: Reviews code for security and performance issues.
workspace:
  mode: isolated
model: qwen3-max
maxIters: 12
temperature: 0.2
top_p: 0.9
tools: [read_file, grep_files, edit_file]
hidden: true
---
You are a code reviewer.
"#;

#[test]
fn loader_parse_full_front_matter() {
    let decl = parse_decl(FULL_MD, "reviewer", None).expect("parses");
    assert_eq!(decl.name, "reviewer");
    assert_eq!(decl.description, "Reviews code for security and performance issues.");
    assert_eq!(decl.prompt, "You are a code reviewer.");
    assert_eq!(decl.workspace_mode, SubagentWorkspaceMode::Isolated);
    assert_eq!(decl.model.as_deref(), Some("qwen3-max"));
    assert_eq!(decl.steps, Some(12)); // maxIters alias
    assert_eq!(decl.temperature, Some(0.2));
    assert_eq!(decl.top_p, Some(0.9));
    assert_eq!(decl.tools, vec!["read_file", "grep_files", "edit_file"]);
    assert!(decl.hidden);
    assert_eq!(decl.mode, SubagentMode::All);
}

#[test]
fn loader_parse_requires_description() {
    assert!(parse_decl("---\nmodel: x\n---\nbody", "a", None).is_none());
    assert!(parse_decl("not front matter", "a", None).is_none());
    assert!(parse_decl("", "a", None).is_none());
}

#[test]
fn loader_parse_workspace_path_clears_body_and_resolves() {
    let md = r#"---
description: Uses external defs.
workspace:
  path: ./defs/reviewer
mode: subagent
topP: 0.5
---
Should be ignored.
"#;
    let decl = parse_decl(md, "reviewer", Some("/ws")).expect("parses");
    assert_eq!(decl.workspace_mode, SubagentWorkspaceMode::Isolated);
    let resolved = decl.workspace_path.as_deref().unwrap();
    assert!(
        resolved.replace('\\', "/").ends_with("ws/defs/reviewer"),
        "unexpected workspace path: {resolved}"
    );
    assert!(decl.prompt.is_empty(), "body cleared when workspace.path set");
    assert_eq!(decl.mode, SubagentMode::Subagent);
    assert_eq!(decl.top_p, Some(0.5)); // topP camelCase alias
    assert_eq!(decl.steps, Some(10)); // default steps
}

#[test]
fn loader_parse_shared_workspace_and_steps() {
    let md = r#"---
description: Shared helper.
workspace:
  mode: shared
steps: 5
---
Helper prompt.
"#;
    let decl = parse_decl(md, "helper", None).expect("parses");
    assert_eq!(decl.workspace_mode, SubagentWorkspaceMode::Shared);
    assert_eq!(decl.steps, Some(5));
    assert_eq!(decl.prompt, "Helper prompt.");
}

#[test]
fn loader_parse_pending_tool_recovery_camel_case_alias_and_remote_defaults() {
    let md = r#"---
description: Remote worker.
enablePendingToolRecovery: true
---
Worker prompt.
"#;
    let decl = parse_decl(md, "worker", None).expect("parses");
    assert_eq!(decl.enable_pending_tool_recovery, Some(true));
    // Front-matter fields that are absent keep the Java defaults.
    assert!(decl.inherit_parent_permissions);
    assert!(decl.is_remote_streaming());
    assert_eq!(decl.remote_stream_detail_or_default().wire_value(), "full");
    assert_eq!(decl.remote_ask_policy, RemoteAskPolicy::Deny);
    assert!(decl.remote_context_attributes.is_none());
    assert!(!decl.has_definition_workspace());
}

#[test]
fn loader_parse_pending_tool_recovery_snake_case() {
    let md = "---\ndescription: d\nenable_pending_tool_recovery: false\n---\nbody";
    let decl = parse_decl(md, "worker", None).expect("parses");
    assert_eq!(decl.enable_pending_tool_recovery, Some(false));
    assert!(!decl.has_definition_workspace());
}

// ---------------------------------------------------------------------------
// AgentSpecLoader scanning
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn loader_load_from_directory_scans_md_only() {
    let dir = temp_dir("scan_dir").await;
    let sub = dir.join("subagents");
    tokio::fs::create_dir_all(&sub).await.unwrap();
    tokio::fs::write(
        sub.join("reviewer.md"),
        "---\ndescription: Reviews.\n---\nBody.\n",
    )
    .await
    .unwrap();
    // Non-.md and nested files must be skipped.
    tokio::fs::write(sub.join("notes.txt"), "nope").await.unwrap();
    let nested = sub.join("nested");
    tokio::fs::create_dir_all(&nested).await.unwrap();
    tokio::fs::write(nested.join("inner.md"), "---\ndescription: x\n---\n").await.unwrap();

    let decls = load_from_directory(sub.to_str().unwrap(), None);
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0].name, "reviewer");
    assert_eq!(decls[0].description, "Reviews.");
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn loader_load_from_filesystem_globs_subagents() {
    let dir = temp_dir("scan_fs").await;
    let sub = dir.join("subagents");
    tokio::fs::create_dir_all(&sub).await.unwrap();
    tokio::fs::write(
        sub.join("coder.md"),
        "---\ndescription: Code.\n---\nCode body.\n",
    )
    .await
    .unwrap();
    tokio::fs::write(sub.join("writer.md"), "---\ndescription: Docs.\n---\n").await.unwrap();

    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let decls = load_from_filesystem(fs.as_ref(), &ctx(), None).await;
    assert_eq!(decls.len(), 2);
    let names: Vec<_> = decls.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"coder"));
    assert!(names.contains(&"writer"));
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---------------------------------------------------------------------------
// DynamicSubagentsMiddleware
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_middleware_merges_static_and_dynamic_and_refreshes_manager() {
    let dir = temp_dir("mw_merge").await;
    let sub = dir.join("subagents");
    tokio::fs::create_dir_all(&sub).await.unwrap();
    tokio::fs::write(
        sub.join("dynamic_agent.md"),
        "---\ndescription: Dynamic agent.\n---\nDynamic prompt.\n",
    )
    .await
    .unwrap();

    let repo = Arc::new(new_repo(&dir));
    let static_entries = vec![entry("static_agent", "Static agent")];
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let mgr = Arc::new(DefaultAgentManager::new(&[]));
    let mw = DynamicSubagentsMiddleware::new(static_entries, repo.clone())
        .with_filesystem(fs)
        .with_factory_builder(builder())
        .with_agent_manager(mgr.clone());

    let captured = Arc::new(std::sync::Mutex::new(None::<Vec<Msg>>));
    let captured2 = captured.clone();
    let input = ReasoningInput::new(vec![Msg::system("base")], vec![], GenerateOptions::default());
    let stream = mw.on_reasoning(
        &ctx(),
        input,
        Box::new(move |inp| {
            *captured2.lock().unwrap() = Some(inp.messages);
            Box::pin(futures::stream::empty::<agentscope_core::error::Result<AgentEvent>>())
        }),
    );
    let _events: Vec<_> = stream.collect().await;

    let msgs = captured.lock().unwrap().clone().expect("next called");
    let text = msgs[0].text_content();
    assert!(text.contains("## Subagents"));
    assert!(text.contains("static_agent"));
    assert!(text.contains("dynamic_agent"));
    assert!(text.contains("Dynamic agent"));

    // Manager was refreshed with the merged set.
    assert!(mgr.has_agent("static_agent"));
    assert!(mgr.has_agent("dynamic_agent"));
    assert!(mgr.create_agent_if_present("dynamic_agent", &ctx()).is_some());
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_middleware_dynamic_overrides_static_on_name_conflict() {
    let dir = temp_dir("mw_override").await;
    let sub = dir.join("subagents");
    tokio::fs::create_dir_all(&sub).await.unwrap();
    tokio::fs::write(
        sub.join("dup.md"),
        "---\ndescription: Dynamic version.\n---\n",
    )
    .await
    .unwrap();

    let repo = Arc::new(new_repo(&dir));
    let static_entries = vec![entry("dup", "Static version")];
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let mgr = Arc::new(DefaultAgentManager::new(&[]));
    let mw = DynamicSubagentsMiddleware::new(static_entries, repo.clone())
        .with_filesystem(fs)
        .with_main_workspace(dir.to_str().unwrap())
        .with_factory_builder(builder())
        .with_agent_manager(mgr.clone());

    let input = ReasoningInput::new(vec![Msg::system("base")], vec![], GenerateOptions::default());
    let stream = mw.on_reasoning(
        &ctx(),
        input,
        Box::new(|_inp| {
            Box::pin(futures::stream::empty::<agentscope_core::error::Result<AgentEvent>>())
        }),
    );
    let _events: Vec<_> = stream.collect().await;

    // Dynamic declaration overrides the static same-name entry.
    let decl = mgr.get_declaration("dup").expect("registered");
    assert_eq!(decl.description, "Dynamic version.");
    assert!(mgr.has_agent("dup"));
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_middleware_passthrough_when_nothing_to_inject() {
    let dir = temp_dir("mw_pass").await;
    let repo = Arc::new(new_repo(&dir));
    // No filesystem, no workspace dir, no static entries, no builder -> nothing.
    let mw = DynamicSubagentsMiddleware::new(vec![], repo.clone());
    let input = ReasoningInput::new(vec![Msg::user("hi")], vec![], GenerateOptions::default());
    let stream = mw.on_reasoning(
        &ctx(),
        input,
        Box::new(|_inp| {
            Box::pin(futures::stream::iter(vec![Ok(
                agentscope_core::event::AgentEvent::with_fields(
                    agentscope_core::event::AgentEventType::HintBlock,
                    serde_json::Map::new(),
                ),
            )]))
        }),
    );
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 1);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_middleware_get_tools_exposes_task_tools() {
    let dir = temp_dir("mw_tools").await;
    let repo = Arc::new(new_repo(&dir));
    let mw = DynamicSubagentsMiddleware::new(vec![], repo.clone());
    let tools = mw.get_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"task_output"));
    assert!(names.contains(&"task_cancel"));
    assert!(names.contains(&"task_list"));
    let _ = tokio::fs::remove_dir_all(&dir).await;
}
