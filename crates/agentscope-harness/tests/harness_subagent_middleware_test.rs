//! Integration tests for the managed subagent mechanism: SubagentsMiddleware,
//! DefaultAgentManager, TaskTool, and the shared render/delivery helpers.
use std::sync::Arc;

use agentscope_core::agent::{CallableAgent, RuntimeContext};
use agentscope_core::event::AgentEvent;
use agentscope_core::message::{JsonMap, Msg};
use agentscope_core::middleware::ReasoningInput;
use agentscope_core::model::GenerateOptions;
use agentscope_core::tool::ToolExecutionContext;
use agentscope_core::{Middleware, Tool};
use agentscope_harness::{
    build_delivery_reminder, prepend_to_system_message, render_subagent_section, Filesystem,
    LocalFilesystem, SubagentDeclaration, SubagentEntry, SubagentFactory, SubagentMode,
    SubagentsMiddleware, TaskRepository, TaskRunSpec, TaskStatus, TaskTool,
    WorkspaceTaskRepository,
};
use futures::StreamExt;

fn ctx() -> RuntimeContext {
    RuntimeContext::default()
}

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_submw_{tag}_{}",
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

fn tctx(session: &str) -> ToolExecutionContext {
    let mut runtime = JsonMap::new();
    runtime.insert(
        "sessionId".to_string(),
        serde_json::Value::String(session.to_string()),
    );
    ToolExecutionContext {
        tool_call: None,
        runtime,
        emitter: None,
        tool_request_config: None,
    }
}

/// Factory whose `create` is never reached in these tests (subagent section
/// rendering only inspects name/description/declaration).
struct NoopFactory;
impl SubagentFactory for NoopFactory {
    fn create(&self, _: &RuntimeContext) -> Arc<dyn CallableAgent> {
        unreachable!("NoopFactory.create should not be called in these tests")
    }
}

fn entry(name: &str, desc: &str) -> SubagentEntry {
    SubagentEntry::new(name, desc, Arc::new(NoopFactory))
}

// ---------------------------------------------------------------------------
// DefaultAgentManager
// ---------------------------------------------------------------------------

#[test]
fn manager_primary_only_is_not_spawnable() {
    let decl = SubagentDeclaration::new("lead", "Lead agent", "sys");
    let mut d2 = decl.clone();
    d2.mode = SubagentMode::Primary;
    let e = entry("lead", "Lead").with_declaration(d2);
    let mgr = agentscope_harness::DefaultAgentManager::new(&[e]);
    assert!(mgr.is_primary_only("lead"));
    assert!(mgr.create_agent_if_present("lead", &ctx()).is_none());
    assert!(mgr.has_agent("lead"));
    assert!(!mgr.is_primary_only("nope"));
}

#[test]
fn declaration_visibility() {
    let mut hidden = SubagentDeclaration::new("h", "h", "s");
    hidden.hidden = true;
    assert!(!hidden.is_visible_to_llm());

    let mut primary = SubagentDeclaration::new("p", "p", "s");
    primary.mode = SubagentMode::Primary;
    assert!(!primary.is_visible_to_llm());

    let all = SubagentDeclaration::new("a", "a", "s");
    assert!(all.is_visible_to_llm());
    assert!(!all.is_remote());
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[test]
fn prepend_merges_into_first_system_message() {
    let msgs = vec![Msg::system("base"), Msg::user("hi")];
    let out = prepend_to_system_message(msgs, "## Extra");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, agentscope_core::message::MsgRole::System);
    let text = out[0].text_content();
    assert!(text.contains("base"));
    assert!(text.contains("## Extra"));
}

#[test]
fn prepend_inserts_system_message_when_absent() {
    let msgs = vec![Msg::user("hi")];
    let out = prepend_to_system_message(msgs, "## Extra");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, agentscope_core::message::MsgRole::System);
    assert!(out[0].text_content().contains("## Extra"));
}

#[test]
fn render_section_filters_hidden_and_primary() {
    let mut hidden = SubagentDeclaration::new("internal", "internal", "s");
    hidden.hidden = true;
    let mut primary = SubagentDeclaration::new("boss", "boss", "s");
    primary.mode = SubagentMode::Primary;
    let entries = vec![
        entry("coder", "Code reviewer").with_declaration(hidden),
        entry("boss", "Boss").with_declaration(primary),
        entry("writer", "Writes docs"),
    ];
    let section = render_subagent_section(&entries, false);
    assert!(section.contains("## Subagents"));
    assert!(section.contains("agent_spawn"));
    assert!(section.contains("writer"));
    assert!(!section.contains("internal"));
    assert!(!section.contains("boss"));
}

#[test]
fn delivery_reminder_formats_tasks() {
    let d = agentscope_harness::TaskDelivery {
        task_id: "t1".to_string(),
        agent_id: Some("coder".to_string()),
        status: TaskStatus::Completed,
        result: Some("done".to_string()),
        error_message: None,
        completed_at: 0,
    };
    let msg = build_delivery_reminder(&[d]);
    let text = msg.text_content();
    assert!(text.contains("<system-reminder>"));
    assert!(text.contains("t1"));
    assert!(text.contains("done"));
    assert!(msg.name.as_deref() == Some("system"));
}

// ---------------------------------------------------------------------------
// SubagentsMiddleware end-to-end
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn middleware_injects_section_summary_and_delivery_and_marks_delivered() {
    let dir = temp_dir("mw").await;
    let repo = Arc::new(new_repo(&dir));
    repo.put_task(
        &ctx(),
        "t1",
        "coder",
        "sess",
        TaskRunSpec::local(|| Ok("done".to_string())),
    )
    .await
    .wait_for_completion(5000);

    let entries = vec![entry("coder", "Code reviewer")];
    let mw = SubagentsMiddleware::new(entries, repo.clone());

    let captured = Arc::new(std::sync::Mutex::new(None::<Vec<Msg>>));
    let captured2 = captured.clone();
    let input = ReasoningInput::new(
        vec![Msg::system("base")],
        vec![],
        GenerateOptions::default(),
    );
    let rc = RuntimeContext::default().with_session_id("sess");
    let stream = mw.on_reasoning(
        &rc,
        input,
        Box::new(move |inp| {
            *captured2.lock().unwrap() = Some(inp.messages);
            Box::pin(futures::stream::empty::<
                agentscope_core::error::Result<AgentEvent>,
            >())
        }),
    );
    let _events: Vec<_> = stream.collect().await;

    // Task was marked delivered after the reasoning stream completed.
    assert!(repo.is_delivered(&ctx(), "sess", "t1").await);

    let msgs = captured.lock().unwrap().clone().expect("next called");
    let sys_text = msgs[0].text_content();
    assert!(sys_text.contains("## Subagents"));
    assert!(sys_text.contains("coder"));
    assert!(sys_text.contains("Async tasks"));

    let last_text = msgs.last().unwrap().text_content();
    assert!(last_text.contains("<system-reminder>"));
    assert!(last_text.contains("t1"));
    assert!(last_text.contains("done"));

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_passthrough_when_no_entries() {
    let dir = temp_dir("passthrough").await;
    let repo = Arc::new(new_repo(&dir));
    let mw = SubagentsMiddleware::new(vec![], repo.clone());
    let input = ReasoningInput::new(vec![Msg::user("hi")], vec![], GenerateOptions::default());
    let stream = mw.on_reasoning(
        &RuntimeContext::default(),
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

// ---------------------------------------------------------------------------
// TaskTool
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn task_tool_output_list_cancel() {
    let dir = temp_dir("tool").await;
    let repo = Arc::new(new_repo(&dir));
    repo.put_task(
        &ctx(),
        "t1",
        "coder",
        "sess",
        TaskRunSpec::local(|| Ok("done".to_string())),
    )
    .await
    .wait_for_completion(5000);

    let tool = TaskTool::new(repo.clone());

    let mut args = JsonMap::new();
    args.insert("task_id".to_string(), serde_json::json!("t1"));
    let out = tool
        .output_tool()
        .call(args.clone(), tctx("sess"))
        .await
        .unwrap();
    assert!(out.text_content().contains("t1"));
    assert!(out.text_content().contains("done"));

    let list = tool
        .list_tool()
        .call(JsonMap::new(), tctx("sess"))
        .await
        .unwrap();
    assert!(list.text_content().contains("1 tracked task"));

    let cancel = tool.cancel_tool().call(args, tctx("sess")).await.unwrap();
    assert!(cancel.text_content().contains("terminal"));

    let _ = tokio::fs::remove_dir_all(&dir).await;
}
