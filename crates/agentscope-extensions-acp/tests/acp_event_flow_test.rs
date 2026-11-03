//! Reproduction test for ACP event flow: thinking + tool calls must reach the
//! client as `AgentThoughtChunk` / `ToolCall` notifications, tagged with a
//! `message_id` and carrying a human-readable tool title.
use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    ContentBlock, SessionId, SessionNotification, SessionUpdate,
};
use agentscope_core::message::{
    ContentBlock as CoreBlock, JsonMap, Msg, TextBlock, ThinkingBlock, ToolUseBlock,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpAgentSession;
use agentscope_harness::agent::HarnessAgent;

/// A scripted model that streams thinking then a tool call.
struct ScriptedModel {
    responses: std::sync::Mutex<Vec<ChatResponse>>,
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let responses = Arc::new(std::sync::Mutex::new(std::mem::take(
            &mut *self.responses.lock().unwrap(),
        )));
        Box::pin(futures::stream::once(async move {
            let mut g = responses.lock().unwrap();
            let r = if g.is_empty() {
                ChatResponse::builder()
                    .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                    .build()
            } else {
                g.remove(0)
            };
            Ok(r)
        }))
    }
    fn model_name(&self) -> &str {
        "scripted"
    }
}

#[tokio::test]
async fn thinking_and_tool_call_reach_client() {
    let model = Arc::new(ScriptedModel {
        responses: std::sync::Mutex::new(vec![
            // Realistic: a single stream carries thinking then a tool call.
            ChatResponse::builder()
                .content(vec![
                    CoreBlock::Thinking(ThinkingBlock::new("let me think")),
                    CoreBlock::ToolUse(ToolUseBlock::new("c1", "read_file", {
                        let mut input = JsonMap::new();
                        input.insert("path".to_string(), serde_json::json!("/tmp/x"));
                        input
                    })),
                ])
                .build(),
            ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("final answer"))])
                .build(),
        ]),
    });
    let agent = HarnessAgent::builder()
        .name("acp-flow")
        .model(model)
        .register_filesystem_tools(true)
        .max_iters(5)
        .build();

    let mut session = AcpAgentSession::new(
        SessionId::new("s-flow"),
        std::env::temp_dir(),
        agent,
        vec![],
        vec![],
    );

    let updates: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let updates_c = Arc::clone(&updates);
    let stop = session
        .run_prompt(
            vec![ContentBlock::Text(
                agent_client_protocol::v1::schema::TextContent::new("do it"),
            )],
            move |n: SessionNotification| {
                let mut u = updates_c.lock().unwrap();
                match n.update {
                    SessionUpdate::AgentThoughtChunk(chunk) => {
                        assert!(
                            chunk.message_id.is_some(),
                            "thought chunk must carry a message_id"
                        );
                        u.push("THOUGHT".into())
                    }
                    SessionUpdate::ToolCall(tc) => {
                        assert!(!tc.title.is_empty(), "tool call must have a title");
                        assert_eq!(
                            tc.raw_input,
                            Some(serde_json::json!({"path": "/tmp/x"})),
                            "the creation notification must carry the tool arguments"
                        );
                        u.push("TOOL_CALL".into())
                    }
                    SessionUpdate::ToolCallUpdate(tu) => {
                        if let Some(status) = tu.fields.status {
                            u.push(format!(
                                "TOOL_CALL_UPDATE status={status:?} in={:?} out={:?}",
                                tu.fields.raw_input, tu.fields.raw_output
                            ));
                        }
                    }
                    SessionUpdate::AgentMessageChunk(_) => u.push("MESSAGE".into()),
                    _ => {}
                }
            },
        )
        .await;

    assert_eq!(stop, agent_client_protocol::v1::schema::StopReason::EndTurn);
    let updates = updates.lock().unwrap().clone();
    eprintln!("updates = {updates:?}");
    assert!(
        updates.iter().any(|u| u == "THOUGHT"),
        "expected THOUGHT notification, got {updates:?}"
    );
    assert!(
        updates.iter().any(|u| u == "TOOL_CALL"),
        "expected TOOL_CALL notification, got {updates:?}"
    );
    // pending (args ready) → running (executing) → failed (file missing).
    let statuses: Vec<&str> = updates
        .iter()
        .filter_map(|u| u.strip_prefix("TOOL_CALL_UPDATE status="))
        .map(|s| s.split(' ').next().unwrap_or(""))
        .collect();
    assert_eq!(
        statuses,
        vec!["InProgress", "Failed"],
        "tool call must transition through the ACP lifecycle, got {updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|u| u.contains("in=Some(Object {\"path\": String(\"/tmp/x\")})")),
        "the tool arguments must stay attached until the call finishes, got {updates:?}"
    );
    assert!(
        updates.iter().any(|u| u.contains("out=Some(")),
        "the tool output must be reported, got {updates:?}"
    );
}

/// A model that fails before producing any output, like an expired API key.
struct FailingModel;

#[async_trait::async_trait]
impl Model for FailingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Err(agentscope_core::error::Error::Agent(
                "api_key_expired: This API key has expired".into(),
            ))
        }))
    }

    fn model_name(&self) -> &str {
        "failing"
    }
}

#[tokio::test]
async fn failed_run_reports_the_actual_error() {
    let model: Arc<dyn Model> = Arc::new(FailingModel);
    let agent = HarnessAgent::builder()
        .name("acp-failure")
        .model(model)
        .max_iters(2)
        .build();

    let mut session = AcpAgentSession::new(
        SessionId::new("s-failure"),
        std::env::temp_dir(),
        agent,
        vec![],
        vec![],
    );

    let chunks: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let chunks_c = Arc::clone(&chunks);
    let stop = session
        .run_prompt(
            vec![ContentBlock::Text(
                agent_client_protocol::v1::schema::TextContent::new("do it"),
            )],
            move |n: SessionNotification| {
                if let SessionUpdate::AgentMessageChunk(chunk) = n.update {
                    if let ContentBlock::Text(text) = chunk.content {
                        chunks_c.lock().unwrap().push(text.text);
                    }
                }
            },
        )
        .await;

    assert_eq!(
        stop,
        agent_client_protocol::v1::schema::StopReason::Refusal,
        "a failed run must stop with refusal"
    );
    let text = chunks.lock().unwrap().join("\n");
    assert!(
        text.contains("api_key_expired"),
        "the provider failure detail must reach the client, got {text:?}"
    );
    assert!(
        !text.contains("run failed"),
        "the generic fallback must not hide the actual error, got {text:?}"
    );
    assert!(
        !text.contains("Agent error: Agent error:"),
        "the error prefix must not be duplicated, got {text:?}"
    );
}

/// Regression: `updated_at` and the context gauge used to be per-clone fields, so a session
/// handle taken *before* a turn (the registry's copy, the catalog task's copy) kept reporting
/// the moment the clone was made. They are shared now.
#[tokio::test]
async fn a_clone_sees_the_activity_of_a_turn_it_did_not_run() {
    let model: Arc<dyn Model> = Arc::new(ScriptedModel {
        responses: std::sync::Mutex::new(vec![ChatResponse::builder()
            .content(vec![CoreBlock::Text(TextBlock::new("ok"))])
            .build()]),
    });
    let agent = HarnessAgent::builder()
        .name("acp-shared")
        .model(model)
        .max_iters(2)
        .build();
    let mut session = AcpAgentSession::new(
        SessionId::new("s-shared"),
        std::env::temp_dir(),
        agent,
        vec![],
        vec![],
    );
    let observer = session.clone();
    assert!(
        observer.session_info().updated_at.is_none(),
        "a session that never ran a prompt has no activity time"
    );

    session
        .run_prompt(
            vec![ContentBlock::Text(
                agent_client_protocol::v1::schema::TextContent::new("do it"),
            )],
            |_| {},
        )
        .await;

    assert!(
        observer.session_info().updated_at.is_some(),
        "the clone must observe the turn's activity time"
    );
}

/// A finished background task must be able to wake an idle session.
///
/// The delivery itself is pull-based (the middleware reads it on a reasoning step), so the
/// completion callback also enqueues a wakeup. This asserts the ACP side of that chain: the
/// entry is readable through the session's own bus, names the session, and is consumed by the
/// drain (one attempt per completion — a failing turn is not retried in a loop).
#[tokio::test]
async fn a_wakeup_request_survives_the_round_trip_through_the_bus() {
    let model: Arc<dyn Model> = Arc::new(ScriptedModel {
        responses: std::sync::Mutex::new(vec![]),
    });
    let dir = std::env::temp_dir().join(format!("agentscope_acp_wakeup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let agent = HarnessAgent::builder()
        .name("acp-wakeup")
        .model(model)
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .max_iters(2)
        .build();
    let session = AcpAgentSession::new(
        SessionId::new("s-wakeup"),
        dir.clone(),
        agent,
        vec![],
        vec![],
    );

    let bus = session.message_bus().expect("the agent owns a bus").clone();
    assert!(
        session.drain_wakeup_requests().await.is_empty(),
        "a fresh session has nothing to wake for"
    );
    bus.enqueue_wakeup("user-1", "s-wakeup", "agent-1")
        .await
        .expect("the producer can enqueue a wakeup");

    assert_eq!(
        session.drain_wakeup_requests().await,
        vec!["s-wakeup".to_string()],
        "the wakeup must name the session it belongs to"
    );
    assert!(
        session.drain_wakeup_requests().await.is_empty(),
        "a wakeup is consumed by the drain, not replayed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
