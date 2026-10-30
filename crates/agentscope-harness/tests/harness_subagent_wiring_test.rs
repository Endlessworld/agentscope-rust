//! Wiring tests for `HarnessAgent::build()`: the spawn/send/list agent tools must be
//! registered on the toolkit and declarations must materialise into callable leaf
//! subagents (mirrors Java `buildSubagentEntries` + `SubagentsMiddleware.getTools()`).
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::{Agent, RuntimeContext, StreamOptions};
use agentscope_core::formatter::JsonSchema;
use agentscope_core::message::{ContentBlock as CoreBlock, JsonMap, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::AgentState;
use agentscope_core::tool::ToolExecutionContext;
use agentscope_harness::agent::subagent::task::{TaskRepository, TaskRunSpec};
use agentscope_harness::agent::subagent::SubagentDeclaration;
use agentscope_harness::agent::HarnessAgent;

/// A model that always answers with its fixed text.
struct ScriptedModel(&'static str);

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let text = self.0;
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(text))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "scripted"
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("agentscope_wiring_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn build_main(dir: &std::path::Path) -> HarnessAgent {
    HarnessAgent::builder()
        .name("main")
        .model(Arc::new(ScriptedModel("child reply")))
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .subagent(SubagentDeclaration::new("worker", "does work", "You work."))
        .build()
}

/// Same as [`build_main`], but with the static `SubagentsMiddleware`.
fn build_main_static(dir: &std::path::Path) -> HarnessAgent {
    HarnessAgent::builder()
        .name("main")
        .model(Arc::new(ScriptedModel("child reply")))
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .subagent(SubagentDeclaration::new("worker", "does work", "You work."))
        .disable_dynamic_subagents()
        .build()
}

/// Same as [`build_main`], plus a resolver mapping `fast` to a distinct child model.
fn build_main_with_resolver(dir: &std::path::Path) -> HarnessAgent {
    let mut decl = SubagentDeclaration::new("fast", "uses a faster model", "Be fast.");
    decl.model = Some("fast".to_string());
    HarnessAgent::builder()
        .name("main")
        .model(Arc::new(ScriptedModel("parent reply")))
        .model_resolver(|id| {
            (id == "fast").then(|| Arc::new(ScriptedModel("fast reply")) as Arc<dyn Model>)
        })
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .subagent(decl)
        .build()
}

fn args(pairs: &[(&str, serde_json::Value)]) -> JsonMap {
    let mut m = JsonMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

async fn call(agent: &HarnessAgent, tool: &str, a: JsonMap) -> String {
    agent
        .toolkit()
        .get_tool(tool)
        .unwrap_or_else(|| panic!("{tool} not registered"))
        .call(a, ToolExecutionContext::default())
        .await
        .expect("tool call ok")
        .text_content()
}

/// Extracts the `task_id` out of a spawn/send response.
fn task_id_of(out: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .unwrap_or_else(|| panic!("no task_id line in output: {out}"))
        .trim()
        .to_string()
}

/// Pulls a task's detail text, waiting for it to settle (`task_output(block=true)`).
async fn task_text(agent: &HarnessAgent, task_id: &str) -> String {
    call(
        agent,
        "task_output",
        args(&[
            ("task_id", serde_json::json!(task_id)),
            ("block", serde_json::json!(true)),
            ("timeout", serde_json::json!(5000)),
        ]),
    )
    .await
}

#[tokio::test]
async fn build_registers_agent_and_task_tools() {
    let dir = temp_dir("tools");
    let agent = build_main(&dir);
    let names = agent.toolkit().tool_names();
    for expected in [
        "agent_spawn",
        "agent_send",
        "agent_list",
        "task_output",
        "task_cancel",
        "task_list",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing {expected} in {names:?}"
        );
    }
}

#[tokio::test]
async fn static_middleware_also_registers_agent_and_task_tools() {
    // The static branch must ship the same task tools as the dynamic one: without
    // `task_output` a background spawn would be uncollectable.
    let dir = temp_dir("static-tools");
    let agent = build_main_static(&dir);
    let names = agent.toolkit().tool_names();
    for expected in [
        "agent_spawn",
        "agent_send",
        "agent_list",
        "task_output",
        "task_cancel",
        "task_list",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing {expected} in {names:?}"
        );
    }
}

#[tokio::test]
async fn background_spawn_output_is_collectable_via_task_output() {
    // End-to-end pull path: `agent_spawn(background=true)` must land in the session the
    // `task_output` tool resolves from `RuntimeContext.session_id`.
    let dir = temp_dir("bg-collect");
    let agent = build_main(&dir);
    let spawn = agent
        .toolkit()
        .get_tool("agent_spawn")
        .expect("agent_spawn registered");
    let ctx = ToolExecutionContext {
        runtime: serde_json::Map::from_iter([(
            "sessionId".to_string(),
            serde_json::Value::String("sess-1".to_string()),
        )]),
        ..Default::default()
    };

    let started = spawn
        .call(
            args(&[
                ("agent", serde_json::json!("worker")),
                ("prompt", serde_json::json!("do it")),
            ]),
            ctx.clone(),
        )
        .await
        .expect("spawn ok")
        .text_content();
    assert!(
        started.contains("started in the background"),
        "out: {started}"
    );
    let task_id = task_id_of(&started);

    let out = agent
        .toolkit()
        .get_tool("task_output")
        .expect("task_output registered")
        .call(
            args(&[
                ("task_id", serde_json::json!(task_id)),
                ("block", serde_json::json!(true)),
                ("timeout", serde_json::json!(5000)),
            ]),
            ctx,
        )
        .await
        .expect("task_output ok")
        .text_content();
    assert!(out.contains("status: Completed"), "out: {out}");
    assert!(out.contains("child reply"), "out: {out}");
}

#[tokio::test]
async fn built_in_general_purpose_subagent_is_spawnable() {
    let dir = temp_dir("general");
    let agent = build_main(&dir);
    let out = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("general-purpose")),
            ("prompt", serde_json::json!("do a thing")),
        ]),
    )
    .await;
    assert!(
        out.contains("agent_key: agent:general-purpose:"),
        "out: {out}"
    );
    let detail = task_text(&agent, &task_id_of(&out)).await;
    assert!(detail.contains("child reply"), "out: {detail}");
}

#[tokio::test]
async fn declared_subagent_runs_as_leaf_and_accepts_follow_up_send() {
    let dir = temp_dir("declared");
    let agent = build_main(&dir);

    let spawned = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("worker")),
            ("prompt", serde_json::json!("do it")),
            ("label", serde_json::json!("w1")),
        ]),
    )
    .await;
    assert!(
        spawned.contains("agent_key: agent:worker:"),
        "out: {spawned}"
    );
    let detail = task_text(&agent, &task_id_of(&spawned)).await;
    assert!(detail.contains("child reply"), "out: {detail}");

    // Address the live child through its label (Java `agent_send`). The message is
    // delivered immediately and the reply comes back through the task channel.
    let sent = call(
        &agent,
        "agent_send",
        args(&[
            ("label", serde_json::json!("w1")),
            ("message", serde_json::json!("more")),
        ]),
    )
    .await;
    assert!(sent.contains("status: delivered"), "out: {sent}");
    let detail = task_text(&agent, &task_id_of(&sent)).await;
    assert!(detail.contains("child reply"), "out: {detail}");

    let listed = call(&agent, "agent_list", args(&[])).await;
    assert!(listed.contains("agent_key: agent:worker:"), "out: {listed}");
}

#[tokio::test]
async fn workspace_subagents_md_folder_is_scanned_and_spawnable() {
    use futures::StreamExt;

    let dir = temp_dir("scanned");
    let subagents = dir.join("ws").join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    std::fs::write(
        subagents.join("helper.md"),
        "---\ndescription: helps the main agent\nsteps: 3\n---\nYou help.\n",
    )
    .unwrap();

    let agent = build_main(&dir);
    // The dynamic middleware re-scans `subagents/*.md` on every reasoning step, so the
    // declaration only exists after the first run.
    let events: Vec<_> = agent
        .stream_events(vec![Msg::user("hi")], StreamOptions::defaults())
        .collect()
        .await;
    for ev in events {
        ev.expect("agent run must succeed");
    }

    let out = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("helper")),
            ("prompt", serde_json::json!("help me")),
        ]),
    )
    .await;
    assert!(out.contains("agent_key: agent:helper:"), "out: {out}");
    let detail = task_text(&agent, &task_id_of(&out)).await;
    assert!(detail.contains("child reply"), "out: {detail}");
}

#[tokio::test]
async fn declared_model_override_uses_the_resolver() {
    let dir = temp_dir("model");
    let agent = build_main_with_resolver(&dir);
    let out = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("fast")),
            ("prompt", serde_json::json!("go")),
        ]),
    )
    .await;
    let detail = task_text(&agent, &task_id_of(&out)).await;
    assert!(detail.contains("fast reply"), "out: {detail}");

    // Without a resolver the override is ignored and the parent model is inherited.
    let dir2 = temp_dir("model-fallback");
    let mut decl = SubagentDeclaration::new("fast", "uses a faster model", "Be fast.");
    decl.model = Some("fast".to_string());
    let agent = HarnessAgent::builder()
        .name("main")
        .model(Arc::new(ScriptedModel("parent reply")))
        .cwd(dir2.to_path_buf())
        .workspace(dir2.join("ws"))
        .max_iters(2)
        .subagent(decl)
        .build();
    let out = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("fast")),
            ("prompt", serde_json::json!("go")),
        ]),
    )
    .await;
    let detail = task_text(&agent, &task_id_of(&out)).await;
    assert!(detail.contains("parent reply"), "out: {detail}");
}

/// A model that records every prompt it receives and answers with a fixed text.
struct RecordingModel {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    reply: &'static str,
}

#[async_trait::async_trait]
impl Model for RecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let text = self.reply;
        {
            let mut seen = self.seen.lock().unwrap();
            for m in &messages {
                seen.push(m.text_content());
            }
        }
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(text))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "recording"
    }
}

#[tokio::test]
async fn dynamic_middleware_delivers_completed_background_tasks() {
    // Regression: the default dynamic subagent middleware must surface
    // terminal background-task results as a <system-reminder> (previously only
    // the static middleware did, so the main agent never got notified) and
    // stamp them delivered after the turn.
    use futures::StreamExt;

    let dir = temp_dir("dyn-delivery");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("main")
        .model(Arc::new(RecordingModel {
            seen: Arc::clone(&seen),
            reply: "main reply",
        }))
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .session_id("s1")
        .subagent(SubagentDeclaration::new("worker", "does work", "You work."))
        .build();

    // A background task finishes while the main agent is idle.
    let repo = agent.task_repository();
    let ctx = agentscope_core::agent::RuntimeContext::default().with_session_id("s1");
    let bg = repo
        .put_task(
            &ctx,
            "t-done",
            "worker",
            "s1",
            TaskRunSpec::Local {
                execution: Box::new(|| Ok("bg result".to_string())),
            },
        )
        .await;
    let finished = tokio::task::spawn_blocking(move || bg.wait_for_completion(5_000))
        .await
        .unwrap();
    assert!(finished, "background task must settle before the turn");

    // The next reasoning step must surface the result as a <system-reminder>.
    let events: Vec<_> = agent
        .stream_events(vec![Msg::user("hi")], StreamOptions::defaults())
        .collect()
        .await;
    for ev in events {
        ev.expect("agent run must succeed");
    }
    let prompts = seen.lock().unwrap().clone();
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("background subagent task") && p.contains("bg result")),
        "delivery reminder missing from model prompts: {prompts:?}"
    );
    assert!(
        repo.is_delivered(&ctx, "s1", "t-done").await,
        "delivery must be stamped after the turn"
    );
}

#[tokio::test]
async fn set_model_switch_propagates_to_newly_spawned_subagents() {
    // Regression: `set_model` must publish the switch through the shared
    // subagent model slot, so children spawned afterwards are built with the
    // new model instead of the build-time snapshot.
    let dir = temp_dir("model-switch");
    let agent = HarnessAgent::builder()
        .name("main")
        .model(Arc::new(ScriptedModel("v1 reply")))
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .subagent(SubagentDeclaration::new("worker", "does work", "You work."))
        .build();

    agent.set_model(Arc::new(ScriptedModel("v2 reply")));

    let out = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("worker")),
            ("prompt", serde_json::json!("go")),
        ]),
    )
    .await;
    let detail = task_text(&agent, &task_id_of(&out)).await;
    assert!(detail.contains("v2 reply"), "out: {detail}");
}

#[tokio::test]
async fn agent_send_recovers_a_failed_subagent_in_process() {
    // In-process recovery: a subagent whose run failed stays registered and
    // addressable — `agent_send` re-invokes the same instance and can succeed.
    struct FailOnceAgent {
        attempts: Arc<AtomicUsize>,
        toolkit: agentscope_core::tool::Toolkit,
    }

    #[async_trait::async_trait]
    impl Agent for FailOnceAgent {
        fn agent_id(&self) -> &str {
            "flaky"
        }
        fn name(&self) -> &str {
            "flaky"
        }
        fn description(&self) -> &str {
            "fails once"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> AgentState {
            AgentState::default()
        }
        fn toolkit(&self) -> &agentscope_core::tool::Toolkit {
            &self.toolkit
        }
    }

    #[async_trait::async_trait]
    impl agentscope_core::agent::CallableAgent for FailOnceAgent {
        async fn call(&self, _messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(agentscope_core::error::Error::Agent(
                    "transient failure".to_string(),
                ))
            } else {
                Ok(Msg::assistant("recovered"))
            }
        }
        async fn call_with_schema(
            &self,
            messages: Vec<Msg>,
            _schema: JsonSchema,
        ) -> agentscope_core::error::Result<Msg> {
            self.call(messages).await
        }
    }

    let dir = temp_dir("recovery");
    let attempts = Arc::new(AtomicUsize::new(0));
    let agent = HarnessAgent::builder()
        .name("main")
        .model(Arc::new(ScriptedModel("parent reply")))
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .subagent_factory_with_description("flaky", "fails once", {
            let attempts = Arc::clone(&attempts);
            move |_name| {
                Arc::new(FailOnceAgent {
                    attempts: Arc::clone(&attempts),
                    toolkit: agentscope_core::tool::Toolkit::new(),
                }) as Arc<dyn agentscope_core::agent::CallableAgent>
            }
        })
        .build();

    // 1) The spawn starts in the background; the child fails on its first run.
    let failed = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("flaky")),
            ("prompt", serde_json::json!("go")),
            ("label", serde_json::json!("flaky-one")),
        ]),
    )
    .await;
    let detail = task_text(&agent, &task_id_of(&failed)).await;
    assert!(detail.contains("transient failure"), "out: {detail}");

    // The failed child is still registered and addressable.
    let listed = call(&agent, "agent_list", args(&[])).await;
    assert!(listed.contains("agent_key: agent:flaky:"), "out: {listed}");

    // 2) `agent_send` re-activates the same instance and the run succeeds.
    let recovered = call(
        &agent,
        "agent_send",
        args(&[
            ("label", serde_json::json!("flaky-one")),
            ("message", serde_json::json!("try again")),
        ]),
    )
    .await;
    assert!(recovered.contains("status: delivered"), "out: {recovered}");
    let detail = task_text(&agent, &task_id_of(&recovered)).await;
    assert!(detail.contains("recovered"), "out: {detail}");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

/// A child that only answers once the test opens its gate — the proof that a send returns
/// before the subagent has produced anything.
struct GatedAgent {
    open: Arc<std::sync::atomic::AtomicBool>,
    toolkit: agentscope_core::tool::Toolkit,
}

#[async_trait::async_trait]
impl Agent for GatedAgent {
    fn agent_id(&self) -> &str {
        "gated"
    }
    fn name(&self) -> &str {
        "gated"
    }
    fn description(&self) -> &str {
        "waits for the gate"
    }
    fn interrupt(&self) {}
    fn agent_state(&self) -> AgentState {
        AgentState::default()
    }
    fn toolkit(&self) -> &agentscope_core::tool::Toolkit {
        &self.toolkit
    }
}

#[async_trait::async_trait]
impl agentscope_core::agent::CallableAgent for GatedAgent {
    async fn call(&self, _messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
        while !self.open.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        Ok(Msg::assistant("late reply"))
    }
    async fn call_with_schema(
        &self,
        messages: Vec<Msg>,
        _schema: JsonSchema,
    ) -> agentscope_core::error::Result<Msg> {
        self.call(messages).await
    }
}

#[tokio::test]
async fn agent_send_hands_the_message_over_without_waiting_for_the_reply() {
    // Regression: `agent_send` used to inline-await the whole child turn, blocking the
    // parent. It must now report `status: delivered` while the child is still running, and
    // the reply must come back through the task/delivery channel.
    let dir = temp_dir("send-async");
    let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent = HarnessAgent::builder()
        .name("main")
        .model(Arc::new(ScriptedModel("parent reply")))
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .subagent_factory_with_description("gated", "waits for the gate", {
            let open = Arc::clone(&open);
            move |_name| {
                Arc::new(GatedAgent {
                    open: Arc::clone(&open),
                    toolkit: agentscope_core::tool::Toolkit::new(),
                }) as Arc<dyn agentscope_core::agent::CallableAgent>
            }
        })
        .build();

    // The spawn also runs in the background, so it returns while the child is gated.
    let spawned = call(
        &agent,
        "agent_spawn",
        args(&[
            ("agent", serde_json::json!("gated")),
            ("prompt", serde_json::json!("first turn")),
            ("label", serde_json::json!("g")),
        ]),
    )
    .await;
    assert!(
        spawned.contains("agent_key: agent:gated:"),
        "out: {spawned}"
    );

    // The send returns while the child still has not answered: the gate is closed.
    let sent = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        call(
            &agent,
            "agent_send",
            args(&[
                ("label", serde_json::json!("g")),
                ("message", serde_json::json!("second turn")),
            ]),
        ),
    )
    .await
    .expect("agent_send must return without waiting for the child");
    assert!(sent.contains("status: delivered"), "out: {sent}");
    assert!(
        !sent.contains("late reply"),
        "send awaited the child: {sent}"
    );
    let send_task = task_id_of(&sent);
    let running = call(
        &agent,
        "task_output",
        args(&[
            ("task_id", serde_json::json!(send_task)),
            ("block", serde_json::json!(false)),
        ]),
    )
    .await;
    assert!(
        !running.contains("late reply"),
        "the child answered before the gate opened: {running}"
    );

    // Open the gate: the delivered turn now completes and its reply is collectable.
    open.store(true, Ordering::SeqCst);
    let detail = task_text(&agent, &send_task).await;
    assert!(detail.contains("late reply"), "out: {detail}");
}

/// A child with a fixed, distinguishable reply.
struct FixedChildAgent {
    toolkit: agentscope_core::tool::Toolkit,
}

#[async_trait::async_trait]
impl Agent for FixedChildAgent {
    fn agent_id(&self) -> &str {
        "worker"
    }
    fn name(&self) -> &str {
        "worker"
    }
    fn description(&self) -> &str {
        "answers immediately"
    }
    fn interrupt(&self) {}
    fn agent_state(&self) -> AgentState {
        AgentState::default()
    }
    fn toolkit(&self) -> &agentscope_core::tool::Toolkit {
        &self.toolkit
    }
}

#[async_trait::async_trait]
impl agentscope_core::agent::CallableAgent for FixedChildAgent {
    async fn call(&self, _messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
        Ok(Msg::assistant("child says hi"))
    }
    async fn call_with_schema(
        &self,
        messages: Vec<Msg>,
        _schema: JsonSchema,
    ) -> agentscope_core::error::Result<Msg> {
        self.call(messages).await
    }
}

#[tokio::test]
async fn spawned_subagent_result_is_delivered_on_the_next_turn() {
    // End-to-end contract: a spawn returns immediately, the child runs in the background,
    // and its result reaches the parent as a `<system-reminder>` on the next reasoning step.
    use futures::StreamExt;

    let dir = temp_dir("spawn-delivery");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("main")
        .model(Arc::new(RecordingModel {
            seen: Arc::clone(&seen),
            reply: "main reply",
        }))
        .cwd(dir.to_path_buf())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .session_id("s1")
        .subagent_factory_with_description("worker", "does work", |_name| {
            Arc::new(FixedChildAgent {
                toolkit: agentscope_core::tool::Toolkit::new(),
            }) as Arc<dyn agentscope_core::agent::CallableAgent>
        })
        .build();

    let spawned = agent
        .toolkit()
        .get_tool("agent_spawn")
        .expect("agent_spawn registered")
        .call(
            args(&[
                ("agent", serde_json::json!("worker")),
                ("prompt", serde_json::json!("do it")),
            ]),
            ToolExecutionContext {
                runtime: serde_json::Map::from_iter([(
                    "sessionId".to_string(),
                    serde_json::Value::String("s1".to_string()),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("spawn ok")
        .text_content();
    let task_id = task_id_of(&spawned);

    // Wait for the child without going through `task_output` (which stamps delivered).
    let repo = agent.task_repository();
    let rc = RuntimeContext::default().with_session_id("s1");
    let bg = repo
        .get_task(&rc, "s1", &task_id)
        .await
        .expect("task registered");
    let waiter = Arc::clone(&bg);
    assert!(
        tokio::task::spawn_blocking(move || waiter.wait_for_completion(5_000))
            .await
            .unwrap(),
        "the background run must settle"
    );

    let events: Vec<_> = agent
        .stream_events(vec![Msg::user("hi")], StreamOptions::defaults())
        .collect()
        .await;
    for ev in events {
        ev.expect("agent run must succeed");
    }
    let prompts = seen.lock().unwrap().clone();
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("background subagent task") && p.contains("child says hi")),
        "the child result was not delivered back: {prompts:?}"
    );
    assert!(
        repo.is_delivered(&rc, "s1", &task_id).await,
        "the delivery must be stamped after the turn"
    );
}
