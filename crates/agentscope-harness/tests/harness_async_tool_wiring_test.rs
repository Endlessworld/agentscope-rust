//! Wiring test for async tool offloading (HARNESS-04): `Builder::async_tool_timeout` must
//! install `AsyncToolMiddleware`, so a slow tool is offloaded and the placeholder reaches the
//! provider. Building `AsyncToolMiddleware` by hand (see `harness_middleware_test.rs`) passes
//! even when the builder never installs it, which is exactly the gap this test covers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{
    ContentBlock as CoreBlock, JsonMap, Msg, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use agentscope_harness::agent::HarnessAgent;
use futures::StreamExt;

/// Answers from a fixed script, recording every batch of messages it was asked to complete.
struct ScriptedModel {
    responses: Mutex<Vec<ChatResponse>>,
    seen: Arc<Mutex<Vec<Vec<Msg>>>>,
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen.lock().unwrap().push(messages);
        let mut script = self.responses.lock().unwrap();
        let response = if script.is_empty() {
            text_response("done")
        } else {
            script.remove(0)
        };
        Box::pin(futures::stream::once(async move { Ok(response) }))
    }

    fn model_name(&self) -> &str {
        "scripted"
    }
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![CoreBlock::Text(TextBlock::new(text))])
        .build()
}

fn slow_tool_call(id: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![CoreBlock::ToolUse(ToolUseBlock::new(
            id,
            "slow_tool",
            JsonMap::new(),
        ))])
        .build()
}

/// Sleeping tool: long enough to always exceed the configured offload timeout.
#[derive(Debug)]
struct SlowTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow_tool"
    }
    fn description(&self) -> &str {
        "sleeps for a while"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn call(
        &self,
        _args: JsonMap,
        _ctx: ToolExecutionContext,
    ) -> agentscope_core::error::Result<ToolResultBlock> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(ToolResultBlock::builder()
            .id("call")
            .name("slow_tool")
            .output(CoreBlock::Text(TextBlock::new("slow result text")))
            .build())
    }
}

/// Flattens text blocks and tool-result outputs: what the provider actually reads.
fn render(messages: &[Msg]) -> String {
    fn collect(blocks: &[CoreBlock], out: &mut String) {
        for block in blocks {
            match block {
                CoreBlock::Text(text) => out.push_str(&text.text),
                CoreBlock::ToolResult(result) => collect(&result.output, out),
                _ => {}
            }
            out.push('\n');
        }
    }
    let mut out = String::new();
    for message in messages {
        collect(message.content(), &mut out);
    }
    out
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_asyncwiring_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn toolkit_with_slow_tool(calls: &Arc<AtomicUsize>) -> Toolkit {
    let toolkit = Toolkit::default();
    toolkit.register_tool(Arc::new(SlowTool {
        calls: Arc::clone(calls),
    }));
    toolkit
}

#[tokio::test(flavor = "multi_thread")]
async fn async_tool_timeout_installs_the_offloading_middleware() {
    let dir = temp_dir("offload");
    let calls = Arc::new(AtomicUsize::new(0));
    let seen: Arc<Mutex<Vec<Vec<Msg>>>> = Arc::new(Mutex::new(Vec::new()));

    let agent = HarnessAgent::builder()
        .name("async-wired")
        .model(Arc::new(ScriptedModel {
            responses: Mutex::new(vec![slow_tool_call("c1"), text_response("done")]),
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .toolkit(toolkit_with_slow_tool(&calls))
        .disable_web_tools()
        .max_iters(4)
        .async_tool_timeout(Duration::from_millis(100))
        .build();

    assert_eq!(
        agent.async_tool_timeout(),
        Some(Duration::from_millis(100)),
        "the configured timeout must be retained"
    );

    let events: Vec<_> = agent
        .stream_events(
            vec![Msg::user("run the slow tool")],
            StreamOptions::default(),
        )
        .collect()
        .await;
    assert!(
        !events.iter().any(|event| event.is_err()),
        "agent run surfaced an error: {:?}",
        events.iter().find(|event| event.is_err())
    );

    let seen = seen.lock().unwrap();
    assert!(
        seen.len() >= 2,
        "the agent must call the model again after the tool phase, got {} call(s)",
        seen.len()
    );
    let second = render(&seen[1]);
    assert!(
        second.contains("running in background"),
        "the offload placeholder must reach the provider when the timeout is configured, \
         got: {second}"
    );
    assert!(
        !second.contains("slow result text"),
        "an offloaded call must not block the loop on the tool result: {second}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the offloaded tool still has to run exactly once"
    );

    // Give the background execution time to finish before the temp dir is removed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offloaded_result_reaches_the_next_wakeup_turn() {
    // The placeholder keeps the loop moving, but the point of the offload is that the real
    // output comes back on its own: the middleware pushes it to the session inbox and queues a
    // wakeup, so a turn with no new input still sees the result.
    let dir = temp_dir("delivery");
    let calls = Arc::new(AtomicUsize::new(0));
    let seen: Arc<Mutex<Vec<Vec<Msg>>>> = Arc::new(Mutex::new(Vec::new()));

    let agent = HarnessAgent::builder()
        .name("async-delivery")
        .model(Arc::new(ScriptedModel {
            responses: Mutex::new(vec![slow_tool_call("c1"), text_response("done")]),
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .toolkit(toolkit_with_slow_tool(&calls))
        .session_id("conversation-async-delivery")
        .disable_web_tools()
        .max_iters(4)
        .async_tool_timeout(Duration::from_millis(100))
        .build();

    let events: Vec<_> = agent
        .stream_events(
            vec![Msg::user("run the slow tool")],
            StreamOptions::default(),
        )
        .collect()
        .await;
    assert!(!events.iter().any(|event| event.is_err()));

    // Wait for the background execution to finish and notify the session.
    let bus = Arc::clone(
        agent
            .message_bus()
            .expect("the harness always installs a message bus"),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !bus
        .inbox_has_messages("conversation-async-delivery")
        .await
        .unwrap()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the background result was never delivered to the inbox"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A wakeup turn carries no input; the inbox middleware injects the delivered result.
    let wakeup: Vec<_> = agent
        .stream_events(Vec::new(), StreamOptions::default())
        .collect()
        .await;
    assert!(!wakeup.iter().any(|event| event.is_err()));

    let seen = seen.lock().unwrap();
    let last = render(seen.last().expect("the wakeup turn must call the model"));
    assert!(
        last.contains("slow result text"),
        "the offloaded output must reach the next turn, got: {last}"
    );
    assert!(
        !bus.inbox_has_messages("conversation-async-delivery")
            .await
            .unwrap(),
        "the hint must be drained instead of re-injected on every later turn"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_async_tool_timeout_the_tool_result_is_inline() {
    let dir = temp_dir("inline");
    let calls = Arc::new(AtomicUsize::new(0));
    let seen: Arc<Mutex<Vec<Vec<Msg>>>> = Arc::new(Mutex::new(Vec::new()));

    let agent = HarnessAgent::builder()
        .name("async-off")
        .model(Arc::new(ScriptedModel {
            responses: Mutex::new(vec![slow_tool_call("c1"), text_response("done")]),
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .toolkit(toolkit_with_slow_tool(&calls))
        .disable_web_tools()
        .max_iters(4)
        .build();

    let events: Vec<_> = agent
        .stream_events(
            vec![Msg::user("run the slow tool")],
            StreamOptions::default(),
        )
        .collect()
        .await;
    assert!(
        !events.iter().any(|event| event.is_err()),
        "agent run surfaced an error: {:?}",
        events.iter().find(|event| event.is_err())
    );

    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 2);
    let second = render(&seen[1]);
    assert!(
        second.contains("slow result text"),
        "without a timeout the tool result must be inline: {second}"
    );
    assert!(
        !second.contains("running in background"),
        "nothing may be offloaded when no timeout is configured: {second}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
