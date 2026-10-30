//! Wiring tests for `HarnessAgent::build()`: the spawn/send/list agent tools must be
//! registered on the toolkit and declarations must materialise into callable leaf
//! subagents (mirrors Java `buildSubagentEntries` + `SubagentsMiddleware.getTools()`).
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::{Agent, StreamOptions};
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
                ("background", serde_json::json!(true)),
            ]),
            ctx.clone(),
        )
        .await
        .expect("spawn ok")
        .text_content();
    assert!(
        started.contains("started in background as task"),
        "out: {started}"
    );
    let task_id = started
        .split("as task '")
        .nth(1)
        .and_then(|s| s.split('\'').next())
        .expect("task id in spawn output");

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
    assert!(out.ends_with("child reply"), "out: {out}");
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
    assert!(spawned.ends_with("child reply"), "out: {spawned}");

    // Address the live child through its label (Java `agent_send`).
    let sent = call(
        &agent,
        "agent_send",
        args(&[
            ("label", serde_json::json!("w1")),
            ("message", serde_json::json!("more")),
        ]),
    )
    .await;
    assert!(sent.contains("child reply"), "out: {sent}");

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
    assert!(out.ends_with("child reply"), "out: {out}");
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
    assert!(out.ends_with("fast reply"), "out: {out}");

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
    assert!(out.ends_with("parent reply"), "out: {out}");
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
    assert!(out.ends_with("v2 reply"), "out: {out}");
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

    // 1) The foreground spawn fails on its first run.
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
    assert!(failed.contains("transient failure"), "out: {failed}");

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
    assert_eq!(recovered, "recovered");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
