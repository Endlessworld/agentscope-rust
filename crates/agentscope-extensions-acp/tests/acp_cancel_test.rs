//! End-to-end cancellation test for the ACP CodingAgent.
//!
//! Uses an in-process [`Channel::duplex`] transport (no subprocess, no real
//! model): the agent side runs the full [`AcpCodingAgent`] handler chain with a
//! scripted model whose stream never completes, while the client side sends a
//! prompt and then cancels it with `$/cancel_request`. We assert the agent
//! answers with `StopReason::Cancelled`.
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    TextContent,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_core::message::Msg;
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

/// A model whose stream never yields any item, so `run_prompt` keeps running
/// until the client cancels the request.
struct NeverEndingModel;

#[async_trait::async_trait]
impl Model for NeverEndingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::pending())
    }
    fn model_name(&self) -> &str {
        "never-ending"
    }
}

/// Builds a fully-wired harness agent with the never-ending model.
fn build_agent(_cwd: std::path::PathBuf, _session_id: &str) -> HarnessAgent {
    let model: Arc<dyn Model> = Arc::new(NeverEndingModel);
    HarnessAgent::builder()
        .name("acp-cancel-test")
        .model(model)
        .max_iters(50)
        .build()
}

#[tokio::test]
async fn cancel_request_stops_prompt_with_stop_reason_cancelled() {
    let (agent_channel, client_channel) = Channel::duplex();

    // Agent side: full AcpCodingAgent over the in-process channel.
    let agent = AcpCodingAgent::builder()
        .model_factory(|cwd, sid| build_agent(cwd, sid))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    // Client side: initialize, create a session, prompt, then cancel.
    let result = Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let new_session = connection
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id;

                let prompt_request = connection.send_request(PromptRequest::new(
                    session_id,
                    vec![ContentBlock::Text(TextContent::new(
                        "start working".to_string(),
                    ))],
                ));

                // Give the agent a moment to enter the never-ending model stream,
                // then ask the peer to cancel the prompt.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                prompt_request.cancel()?;

                let response = prompt_request.block_task().await?;
                assert_eq!(
                    response.stop_reason,
                    agent_client_protocol::v1::schema::StopReason::Cancelled,
                    "prompt must stop with Cancelled after $/cancel_request"
                );
                Ok(())
            },
        )
        .await;

    result.expect("client connection should succeed");
    // The agent side should also finish cleanly once the channel closes.
    let _ = agent_task.await;
}

#[tokio::test]
async fn session_cancel_notification_stops_the_prompt() {
    // ACP clients cancel with the `session/cancel` *notification* (the protocol-defined path),
    // not only with `$/cancel_request`. Without a handler "Stop" was a no-op: the run kept
    // streaming (and billing) while the client had already abandoned the prompt.
    let (agent_channel, client_channel) = Channel::duplex();

    let agent = AcpCodingAgent::builder()
        .model_factory(|cwd, sid| build_agent(cwd, sid))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let session_id = connection
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;

                let prompt_request = connection.send_request(PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("start working"))],
                ));

                // Let the prompt enter the never-ending model stream, then send the
                // protocol-level cancel notification.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                connection.send_notification(CancelNotification::new(session_id))?;

                let response = prompt_request.block_task().await?;
                assert_eq!(
                    response.stop_reason,
                    agent_client_protocol::v1::schema::StopReason::Cancelled,
                    "prompt must stop with Cancelled after session/cancel"
                );
                Ok(())
            },
        )
        .await
        .expect("client connection should succeed");
    let _ = agent_task.await;
}
