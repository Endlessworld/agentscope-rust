//! Regression test for `session/load` replay (ACP-03).
//!
//! This agent advertised `agentCapabilities.loadSession` but the handler answered with only
//! `modes`/`config_options`: not a single `session/update` was sent, so an IDE that
//! reconnected to an existing session drew an empty transcript while the agent still had the
//! whole conversation in context. ACP v1 requires the agent to replay the conversation
//! before answering `session/load`, so the client can rebuild its UI.
//!
//! The test drives the real `AcpCodingAgent` over an in-process channel with a scripted model
//! (no provider, no network): it runs one prompt, then loads the same session and asserts the
//! replayed chunks are the user message followed by the agent's answer.
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::v1::schema::{
    ContentBlock, InitializeRequest, PromptRequest, SessionNotification, SessionUpdate, StopReason,
    TextContent,
};
use agent_client_protocol::v1::{ConnectionTo, SessionMessage, SessionSetup};
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

const USER_TEXT: &str = "remember the number 42";
const ANSWER_TEXT: &str = "the answer is 42";

/// Answers every model call with the same text, so the transcript has one predictable turn.
struct AnsweringModel;

#[async_trait::async_trait]
impl Model for AnsweringModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(ANSWER_TEXT))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "answering"
    }
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "acp-load-replay-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// Flattens a chunk's content into text (the replay only carries text blocks).
fn text_of(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

#[tokio::test]
async fn session_load_replays_the_conversation_as_chunks() {
    let work = scratch_dir("work");
    let (agent_channel, client_channel) = Channel::duplex();

    let agent = AcpCodingAgent::builder()
        .model_factory(|cwd, sid| {
            HarnessAgent::builder()
                .name("acp-load-replay")
                .model(Arc::new(AnsweringModel) as Arc<dyn Model>)
                .session_id(sid)
                .cwd(cwd.clone())
                .workspace(cwd.join(".agentscope").join("workspace"))
                .disable_memory_hooks()
                .max_iters(3)
                .build()
        })
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let work_for_client = work.clone();
    let result = Client
        .v1()
        .connect_with(client_channel, move |connection: ConnectionTo<Agent>| {
            let work = work_for_client.clone();
            async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                // ---- Turn 1: write a real conversation into the journal. ----
                let SessionSetup { session, .. } = connection
                    .build_session(work.clone())
                    .block_task()
                    .start_session()
                    .await?;
                let session_id = session.session_id().clone();
                let prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(USER_TEXT))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(prompt.stop_reason, StopReason::EndTurn);
                // Release the live session's routing before loading it again.
                drop(session);

                // ---- Load: the replay must arrive before (or with) the response. ----
                let SessionSetup {
                    session: mut restored,
                    ..
                } = connection
                    .load_session(session_id, work.clone())
                    .block_task()
                    .start_session()
                    .await?;

                let mut replay: Vec<(bool, String)> = Vec::new();
                while replay.len() < 2 {
                    let update =
                        tokio::time::timeout(Duration::from_secs(10), restored.read_update())
                            .await
                            .expect("session/load must replay the conversation")?;
                    let SessionMessage::Message(dispatch) = update else {
                        break;
                    };
                    let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
                    let sink_c = Arc::clone(&sink);
                    MatchDispatch::new(dispatch.into_raw())
                        .if_notification(async move |notification: SessionNotification| {
                            let mut sink = sink_c.lock().unwrap();
                            match notification.update {
                                SessionUpdate::UserMessageChunk(chunk) => {
                                    if let Some(text) = text_of(&chunk.content) {
                                        sink.push((true, text));
                                    }
                                }
                                SessionUpdate::AgentMessageChunk(chunk) => {
                                    if let Some(text) = text_of(&chunk.content) {
                                        sink.push((false, text));
                                    }
                                }
                                _ => {}
                            }
                            Ok(())
                        })
                        .await
                        .otherwise_ignore()?;
                    let mut sink = sink.lock().unwrap();
                    replay.append(&mut sink);
                }

                assert_eq!(
                    replay.first(),
                    Some(&(true, USER_TEXT.to_string())),
                    "session/load must replay the user message first: {replay:?}"
                );
                assert!(
                    replay
                        .iter()
                        .any(|(from_user, text)| !from_user && text == ANSWER_TEXT),
                    "session/load must replay the agent reply: {replay:?}"
                );
                Ok(())
            }
        })
        .await;

    result.expect("client connection should succeed");
    let _ = agent_task.await;
}
