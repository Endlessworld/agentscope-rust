//! `subagent_update` end to end over an in-process channel: a child session's frames reach an
//! advertising client, are routed by outer session id, and stay silent for a client that did not
//! advertise `subagents`.
//!
//! Frames are injected through the same channel the harness writes to, so the test covers the
//! whole ACP half (channel plumbing, capability gate, frame mapping, routing) without a model
//! ever spawning a real subagent.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    ClientCapabilities, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    SessionNotification, SubagentCapabilities, TextContent,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::{AcpCodingAgent, SessionBuild};
use agentscope_harness::agent::HarnessAgent;
use agentscope_harness::tool::SubagentEventSink;

/// The child session id the injected frames use.
const CHILD: &str = "sub-1";

/// A model that always answers "done", so a turn costs exactly one reasoning call.
struct StubModel;

#[async_trait::async_trait]
impl Model for StubModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "stub"
    }
}

fn fresh_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agentscope_acp_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Builds the agent under test and returns the sending side of its subagent channel.
///
/// The session factory is the only place that sees the channel, exactly like the ACP binary.
fn agent_with_captured_channel(
    root: &PathBuf,
) -> (AcpCodingAgent, Arc<Mutex<Option<SubagentEventSink>>>) {
    let sink: Arc<Mutex<Option<SubagentEventSink>>> = Arc::new(Mutex::new(None));
    let sink_for_factory = Arc::clone(&sink);
    let root = root.clone();
    let agent = AcpCodingAgent::builder()
        .session_factory(move |build: SessionBuild| {
            if let Some(events) = build.subagent_events.clone() {
                *sink_for_factory
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(events);
            }
            HarnessAgent::builder()
                .name("acp-subagent-test")
                .model(Arc::new(StubModel))
                .session_id(build.session_id)
                .cwd(root.clone())
                .workspace(root.join(".agentscope").join("workspace"))
                .disable_memory_hooks()
                .max_iters(2)
                .build()
        })
        .build()
        .expect("build AcpCodingAgent");
    (agent, sink)
}

fn exposure(session: &str, label: &str) -> AgentEvent {
    let mut fields = serde_json::Map::new();
    fields.insert("subagentId".to_string(), serde_json::json!("agent:key:1"));
    fields.insert("agentId".to_string(), serde_json::json!("explorer"));
    fields.insert("sessionId".to_string(), serde_json::json!(session));
    fields.insert("label".to_string(), serde_json::json!(label));
    AgentEvent::with_fields(AgentEventType::SubagentExposed, fields)
}

fn text_delta(text: &str) -> AgentEvent {
    let mut fields = serde_json::Map::new();
    fields.insert("delta".to_string(), serde_json::json!(text));
    AgentEvent::with_fields(AgentEventType::TextBlockDelta, fields)
}

fn finished() -> AgentEvent {
    let mut fields = serde_json::Map::new();
    fields.insert("kind".to_string(), serde_json::json!("subagent_finished"));
    fields.insert("outcome".to_string(), serde_json::json!("success"));
    AgentEvent::with_fields(AgentEventType::Custom, fields)
}

/// Every notification that belongs to a subagent: `subagent_update` (outer = parent) or anything
/// routed to the child's own session.
fn subagent_frames(frames: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    frames
        .iter()
        .filter(|frame| {
            frame["update"]["sessionUpdate"] == "subagent_update" || frame["sessionId"] == CHILD
        })
        .collect()
}

/// Waits until `check` accepts the collected frames, or fails with what arrived instead.
async fn wait_for(
    collected: &Arc<Mutex<Vec<serde_json::Value>>>,
    check: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let guard = collected
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if check(&guard) {
                return guard.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for subagent frames; got {:?}",
            collected
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn subagent_frames_reach_an_advertising_client() {
    let root = fresh_root("subagent_frames");
    let (agent, sink) = agent_with_captured_channel(&root);
    let (agent_channel, client_channel) = Channel::duplex();
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_notifications = Arc::clone(&seen);
    let root_for_client = root.clone();

    Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let Ok(value) = serde_json::to_value(&notification) {
                    seen_notifications.lock().unwrap().push(value);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                            ClientCapabilities::new().subagents(SubagentCapabilities::new()),
                        ),
                    )
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(root_for_client.clone()))
                    .block_task()
                    .await?
                    .session_id;
                // The forwarder starts with the first prompt (that is when the connection is
                // bound to the session), so frames must be injected after it returns.
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("hi"))],
                    ))
                    .block_task()
                    .await?;

                let parent = session_id.to_string();
                let events = sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .expect("the session factory received the subagent channel");
                events
                    .send((CHILD.to_string(), exposure(CHILD, "探索仓库结构")))
                    .expect("frames are accepted");
                events
                    .send((CHILD.to_string(), text_delta("发现 3 个模块")))
                    .expect("frames are accepted");
                events
                    .send((CHILD.to_string(), finished()))
                    .expect("frames are accepted");

                // The idle frame closes the sequence.
                let frames = wait_for(&seen, |frames| {
                    subagent_frames(frames)
                        .iter()
                        .any(|frame| frame["update"]["state"]["state"] == "idle")
                })
                .await;
                let frames = subagent_frames(&frames);
                assert_eq!(frames.len(), 3, "unexpected extra frames: {frames:#?}");

                // 1. The announcement belongs to the parent and names the child.
                assert_eq!(frames[0]["sessionId"], parent.as_str());
                assert_eq!(frames[0]["update"]["sessionUpdate"], "subagent_update");
                assert_eq!(frames[0]["update"]["sessionId"], CHILD);
                assert_eq!(frames[0]["update"]["title"], "探索仓库结构");
                assert_eq!(frames[0]["update"]["state"]["state"], "running");
                assert_eq!(
                    frames[0]["update"]["capabilities"]["cancel"],
                    serde_json::json!({}),
                    "the client only shows the cancel button for an advertised capability"
                );

                // 2. The child's own content is routed by its session id, with a message id of
                // its own (reusing the parent turn's would merge the two sessions).
                assert_eq!(frames[1]["sessionId"], CHILD);
                assert_eq!(frames[1]["update"]["sessionUpdate"], "agent_message_chunk");
                assert_eq!(frames[1]["update"]["messageId"], "sub-sub-1");

                // 3. The sequence closes with `idle` on the parent.
                assert_eq!(frames[2]["sessionId"], parent.as_str());
                assert_eq!(frames[2]["update"]["state"]["state"], "idle");
                Ok(())
            },
        )
        .await
        .expect("client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn nothing_is_sent_when_the_client_does_not_advertise_subagents() {
    let root = fresh_root("subagent_silent");
    let (agent, sink) = agent_with_captured_channel(&root);
    let (agent_channel, client_channel) = Channel::duplex();
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_notifications = Arc::clone(&seen);
    let root_for_client = root.clone();

    Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let Ok(value) = serde_json::to_value(&notification) {
                    seen_notifications.lock().unwrap().push(value);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                // No `subagents` capability: an ACP client parses notifications strictly and an
                // unknown `sessionUpdate` can end the connection, so the agent must stay silent.
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(root_for_client.clone()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(PromptRequest::new(
                        session_id,
                        vec![ContentBlock::Text(TextContent::new("hi"))],
                    ))
                    .block_task()
                    .await?;

                let events = sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .expect("the session factory received the subagent channel");
                events
                    .send((CHILD.to_string(), exposure(CHILD, "探索仓库结构")))
                    .expect("frames are accepted");
                events
                    .send((CHILD.to_string(), text_delta("发现 3 个模块")))
                    .expect("frames are accepted");
                events
                    .send((CHILD.to_string(), finished()))
                    .expect("frames are accepted");

                // Give the forwarder a chance to (wrongly) emit something.
                tokio::time::sleep(Duration::from_millis(500)).await;
                let frames = seen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                assert!(
                    subagent_frames(&frames).is_empty(),
                    "a client without the capability must receive nothing: {frames:#?}"
                );
                Ok(())
            },
        )
        .await
        .expect("client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}
