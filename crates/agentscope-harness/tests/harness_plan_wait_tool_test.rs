//! Tests for PlanModeTools (plan_enter / plan_write / plan_exit) and
//! WaitAsyncResultsTool.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::CallableAgent;
use agentscope_core::message::{ContentBlock, JsonMap, Msg, TextBlock, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use agentscope_harness::agent::bus::async_tool::{
    AsyncToolRecord, AsyncToolRegistry, WorkspaceAsyncToolRegistry,
};
use agentscope_harness::agent::HarnessAgent;
use agentscope_harness::filesystem::{Filesystem, LocalFilesystem};
use agentscope_harness::tool::{PlanModeHandle, WaitAsyncResultsTool};

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

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("as_plan_wait_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn plan_mode_tools_enter_write_exit_roundtrip() {
    let dir = temp_dir("plan");
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));

    let handle = PlanModeHandle::new(Arc::clone(&fs), None);
    let enter = agentscope_harness::tool::PlanEnterTool::new(handle.clone());
    let write = agentscope_harness::tool::PlanWriteTool::new(handle.clone());
    let exit = agentscope_harness::tool::PlanExitTool::new(handle.clone());

    assert!(!handle.is_plan_active());

    let out = run(&enter, JsonMap::new()).await;
    assert!(out.contains("plans/PLAN.md"), "enter output: {out}");
    assert!(handle.is_plan_active());

    let out = run(
        &write,
        args(&[("content", serde_json::json!("# Step one\n- do x"))]),
    )
    .await;
    assert!(out.contains("plans/PLAN.md"), "write output: {out}");

    // The plan file landed on the filesystem at the reported path.
    let content = fs
        .read(
            &agentscope_core::agent::RuntimeContext::default(),
            "plans/PLAN.md",
            0,
            usize::MAX,
        )
        .await
        .expect("plan file readable");
    assert!(content.content.contains("Step one"));

    let out = run(&exit, JsonMap::new()).await;
    assert!(out.contains("Exited plan mode"), "exit output: {out}");
    assert!(!handle.is_plan_active());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plan_mode_handle_binds_shared_state() {
    // A handle built via `with_state` mutates the provided session state, so
    // `plan_enter`/`plan_exit` flip the flag core's acting phase enforces.
    use agentscope_core::state::AgentState;
    use std::sync::Mutex;

    let dir = temp_dir("plan_state");
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let state = Arc::new(Mutex::new(AgentState::default()));

    let handle = PlanModeHandle::with_state(Arc::clone(&fs), None, Arc::clone(&state));
    let enter = agentscope_harness::tool::PlanEnterTool::new(handle.clone());
    let exit = agentscope_harness::tool::PlanExitTool::new(handle.clone());

    run(&enter, JsonMap::new()).await;
    assert!(
        state.lock().unwrap().plan_mode_context.plan_active,
        "plan_enter must set plan_active on the shared (session) state"
    );
    assert!(handle.is_plan_active());

    run(&exit, JsonMap::new()).await;
    assert!(!state.lock().unwrap().plan_mode_context.plan_active);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs one `plan_enter` tool call, then answers. Records every request.
struct EnterPlanThenAnswer {
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<Msg>>>,
}

impl Model for EnterPlanThenAnswer {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.requests.lock().unwrap().push(messages);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let response = if call == 0 {
            ChatResponse::builder()
                .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                    "plan-1",
                    "plan_enter",
                    JsonMap::new(),
                ))])
                .build()
        } else {
            ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new("planned"))])
                .build()
        };
        Box::pin(futures::stream::once(async move { Ok(response) }))
    }

    fn model_name(&self) -> &str {
        "enter-plan"
    }

    fn context_window_size(&self) -> usize {
        128_000
    }
}

#[tokio::test]
async fn plan_enter_mid_turn_reaches_the_next_request() {
    // `plan_enter` mutates the *shared* state handle the tool set was bound to, while the
    // reasoning loop runs on a private clone. The flip must survive the next iteration's
    // state publish, or the model never learns it is in PLAN mode (and the acting phase
    // keeps allowing mutations).
    let dir = temp_dir("plan_mid_turn");
    let model = Arc::new(EnterPlanThenAnswer {
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let agent = HarnessAgent::builder()
        .name("plan-mid-turn")
        .model(model.clone())
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .enable_plan_mode()
        .build();

    agent
        .call(vec![Msg::user("draft a plan")])
        .await
        .expect("the run must complete");

    let requests = model.requests.lock().unwrap();
    assert!(
        requests.len() >= 2,
        "the plan_enter tool result must drive another reasoning iteration"
    );
    let second = requests[1]
        .iter()
        .map(|message| message.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        second.contains("PLAN MODE is active"),
        "the request after plan_enter must carry the PLAN banner: {second}"
    );
    drop(requests);

    let state = agent.agent().expect("built with a model").state_handle();
    assert!(
        state.lock().unwrap().plan_mode_context.plan_active,
        "plan_enter must keep plan_active set on the session state"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn wait_async_results_reports_settled_and_missing_tasks() {
    let dir = temp_dir("wait");
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let registry = Arc::new(WorkspaceAsyncToolRegistry::new(
        Arc::clone(&fs),
        "/bus/async",
    ));

    registry
        .register(AsyncToolRecord::new(
            "t1",
            "sess1",
            "shell",
            "call-1",
            chrono::Utc::now(),
        ))
        .await
        .unwrap();

    let tool = WaitAsyncResultsTool::new(registry.clone());

    // Unknown session 鈫?no tasks found (session_id defaults to "default").
    let out = run(&tool, JsonMap::new()).await;
    assert!(out.contains("No async tasks"), "empty output: {out}");

    // Specific unknown task id in an existing session still reports nothing matched.
    let out = run(&tool, args(&[("task_ids", serde_json::json!(["nope"]))])).await;
    // ctx has no runtime session_id 鈫?"default" session; t1 is under sess1.
    assert!(
        out.contains("No async tasks") || out.contains("nope"),
        "output: {out}"
    );

    // Complete t1 and wait for it explicitly by targeting its session via runtime map.
    registry.complete("t1", "done").await.unwrap();
    let mut ctx = ToolExecutionContext::default();
    ctx.runtime.insert(
        "session_id".to_string(),
        serde_json::Value::String("sess1".into()),
    );
    let block = tool
        .call(args(&[("task_ids", serde_json::json!(["t1"]))]), ctx)
        .await
        .expect("wait ok");
    let text = block.text_content();
    assert!(text.contains("settled"), "settled output: {text}");
    assert!(text.contains("COMPLETED"), "status line: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn wait_async_results_times_out_when_pending() {
    let dir = temp_dir("timeout");
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let registry = Arc::new(WorkspaceAsyncToolRegistry::new(
        Arc::clone(&fs),
        "/bus/async",
    ));
    registry
        .register(AsyncToolRecord::new(
            "t2",
            "s",
            "web",
            "call-2",
            chrono::Utc::now(),
        ))
        .await
        .unwrap();

    let tool = WaitAsyncResultsTool::new(registry.clone());
    let mut ctx = ToolExecutionContext::default();
    ctx.runtime.insert(
        "session_id".to_string(),
        serde_json::Value::String("s".into()),
    );

    let block = tool
        .call(args(&[("timeout_ms", serde_json::json!(600))]), ctx)
        .await
        .expect("wait ok");
    let text = block.text_content();
    assert!(text.contains("Timed out"), "timeout output: {text}");
    assert!(text.contains("t2"), "pending id listed: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}
