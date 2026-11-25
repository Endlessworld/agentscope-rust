//! MCP-over-ACP must survive on a v2 session.
//!
//! The stateless `mcp/message` carrier exists in both protocol versions with the same wire shape,
//! so a v2 session keeps the bridge instead of losing it. This test declares an ACP MCP server in
//! `session/new`, answers the resulting `tools/list` from the client side, and checks that the
//! agent really sent it over the v2 connection — which is also what makes advertising
//! `session.mcp.acp` in the v2 capabilities honest.
#![cfg(feature = "acp-v2")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::schema as v2;
use agent_client_protocol::v2::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

/// Answers every turn with "done"; this test only drives the MCP bridge.
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

fn session_factory(
    root: PathBuf,
) -> impl Fn(PathBuf, &str) -> HarnessAgent + Send + Sync + 'static {
    move |_cwd, session_id| {
        HarnessAgent::builder()
            .name("acp-v2-mcp-test")
            .model(Arc::new(StubModel))
            .session_id(session_id)
            .cwd(root.clone())
            .workspace(root.join(".agentscope").join("workspace"))
            .disable_memory_hooks()
            .max_iters(3)
            .build()
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agentscope_acp_v2_mcp_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn v2_session_bridges_an_acp_mcp_server() {
    let root = temp_root("bridge");
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(session_factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    // Every inner MCP method the agent sends over the carrier, in order.
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let client_root = root.clone();

    Client
        .v2()
        .on_receive_notification(
            async move |_notification: v2::UpdateSessionNotification, _cx| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::MessageMcpRequest,
                        responder: agent_client_protocol::Responder<
                v2::MessageMcpResponse,
                agent_client_protocol::V2,
            >,
                        _cx: ConnectionTo<Agent>| {
                recorded.lock().unwrap().push(request.method.clone());
                // One tool is enough: the point is that discovery reached the client.
                responder.respond(v2::MessageMcpResponse::success(serde_json::json!({
                    "tools": [{
                        "name": "echo",
                        "description": "Echoes its input",
                        "inputSchema": {"type": "object", "properties": {}},
                    }],
                })))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                let init = connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("v2-mcp-test", "0.0.0"),
                    ))
                    .block_task()
                    .await?;
                // The bridge works on v2, so the advertisement must say so.
                let mcp = init
                    .capabilities
                    .session
                    .as_ref()
                    .and_then(|session| session.mcp.as_ref())
                    .expect("the session capability block must advertise MCP transports");
                assert!(
                    mcp.acp.is_some(),
                    "v2 sessions bridge MCP-over-ACP, so the capability must be advertised"
                );

                let new = connection
                    .send_request(v2::NewSessionRequest::new(client_root.clone()).mcp_servers(
                        vec![v2::McpServer::Acp(v2::McpServerAcp::new(
                            "acp-test",
                            v2::McpServerAcpId::new("srv-acp-test"),
                        ))],
                    ))
                    .block_task()
                    .await?;
                assert!(!new.session_id.to_string().is_empty());

                // Bridging is asynchronous (`session/new` spawns it), so wait for the round trip.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                loop {
                    if seen
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|method| method == "tools/list")
                    {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the agent never sent tools/list over mcp/message, saw {:?}",
                        seen.lock().unwrap()
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }

                Ok(())
            },
        )
        .await
        .expect("v2 client run");

    agent_task.await.expect("agent task").expect("agent run");
}
