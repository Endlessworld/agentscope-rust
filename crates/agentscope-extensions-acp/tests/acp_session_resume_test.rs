//! End-to-end "restart then resume" test for the ACP CodingAgent.
//!
//! Native sessions restore **only from their journal** (Java
//! `HarnessAgent`: "Native sessions restore only from their journal"), so the
//! session factory here attaches no `AgentStateStore`: the ACP session id is
//! mapped onto the harness agent's `session_id`, which is the journal key. A
//! restarted process (fresh registry, fresh agent) that receives `session/load`
//! with the same id must restore the prior conversation from committed facts,
//! not from a snapshot.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    StopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Channel, Client, ConnectionTo};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

/// A model that records the text content of every call's input, then answers
/// "done". Recording lets the test observe exactly what history the model saw
/// after a restart.
struct RecordingModel {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl Model for RecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen
            .lock()
            .unwrap()
            .push(messages.iter().map(|m| m.text_content()).collect());
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "recording"
    }
}

/// Session factory carrying the same persistence wiring as the
/// `acp_harness_agent` binary: identity for the journal, no state store. A
/// fresh agent per session models a restarted process; only the journal on disk
/// survives.
fn session_factory(
    root: PathBuf,
    seen: Arc<Mutex<Vec<Vec<String>>>>,
) -> impl Fn(PathBuf, &str) -> HarnessAgent + Send + Sync + 'static {
    move |_cwd, session_id| {
        HarnessAgent::builder()
            .name("acp-resume-test")
            .model(Arc::new(RecordingModel {
                seen: Arc::clone(&seen),
            }))
            // The journal key. Nothing else is persisted explicitly: history comes
            // from committed facts under the workspace's session namespace.
            .session_id(session_id)
            .cwd(root.clone())
            .workspace(root.join(".agentscope").join("workspace"))
            // Memory hooks are disabled in the binary too; keeping them out here
            // makes each turn exactly one reasoning call and keeps the test from
            // writing into a shared memory ledger.
            .disable_memory_hooks()
            .max_iters(3)
            .build()
    }
}

#[tokio::test]
async fn session_load_after_restart_resumes_conversation() {
    let root = std::env::temp_dir().join(format!("agentscope_acp_resume_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));

    // ---- Process 1: open a session and take one turn (state is saved). ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(session_factory(root.clone(), Arc::clone(&seen)))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let session_id = Client
        .builder()
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
                let response = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(
                            "first question".to_string(),
                        ))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                Ok(session_id)
            },
        )
        .await
        .expect("client run 1");
    let _ = agent_task.await;
    // Every model call made so far belongs to run 1; run 2's calls follow.
    let calls_after_run1 = seen.lock().unwrap().len();
    assert!(calls_after_run1 >= 1, "run 1 must call the model");

    // ---- Process 2 (restart): fresh agent, empty registry, same id ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(session_factory(root.clone(), Arc::clone(&seen)))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    Client
        .builder()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                // `session/load` on a restarted agent rebuilds the session from
                // nothing, so only the persisted checkpoint can supply history.
                connection
                    .send_request(LoadSessionRequest::new(
                        session_id.clone(),
                        std::env::temp_dir(),
                    ))
                    .block_task()
                    .await?;
                let response = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(
                            "second question".to_string(),
                        ))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                Ok(())
            },
        )
        .await
        .expect("client run 2");
    let _ = agent_task.await;

    // The run-2 model calls must have seen the run-1 history.
    let seen = seen.lock().unwrap();
    let run2: Vec<String> = seen[calls_after_run1..]
        .iter()
        .map(|c| c.join("\n"))
        .collect();
    assert!(!run2.is_empty(), "run 2 must call the model");
    assert!(
        run2.iter()
            .any(|c| c.contains("first question") && c.contains("second question")),
        "restored history must reach the model after restart, got: {run2:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
