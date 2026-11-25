//! The `_acp_extends/*` surface must be reachable over the v2 wire as well.
//!
//! v2 is a separate protocol graph: extending only the v1 graph would leave every custom method
//! answering `-32601` as soon as a client negotiates v2. The handlers are protocol-agnostic, and
//! the v2 graph registers the same ones, so this test sends each custom method over a v2
//! connection and asserts the answer is a real outcome (a result or a domain error) rather than
//! "method not found".
#![cfg(feature = "acp-v2")]

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::schema as v2;
use agent_client_protocol::v2::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client, ErrorCode, JsonRpcMessage};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::ext;
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

/// Answers every turn with "done"; this test only drives the extension surface.
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
            .name("acp-v2-ext-test")
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
        "agentscope_acp_v2_ext_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// The graph must answer with a domain outcome, never `-32601`.
fn assert_routed(method: &str, error: &agent_client_protocol::Error) {
    assert_ne!(
        error.code,
        ErrorCode::METHOD_NOT_FOUND,
        "{method} must be registered in the v2 graph, got {error:?}"
    );
}

#[tokio::test]
async fn v2_extension_methods_are_routed() {
    let root = temp_root("routed");
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(session_factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let client_root = root.clone();

    Client
        .v2()
        .on_receive_notification(
            async move |_notification: v2::UpdateSessionNotification, _cx| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                let _init = connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("v2-ext-test", "0.0.0"),
                    ))
                    .block_task()
                    .await?;
                let new = connection
                    .send_request(v2::NewSessionRequest::new(client_root.clone()))
                    .block_task()
                    .await?;
                let session_id = new.session_id.to_string();

                // --- fs/list: no `path` answers the browsable roots ---
                let request = ext::ExtFsListRequest::parse_message(
                    ext::FS_LIST_METHOD,
                    &serde_json::json!({}),
                )
                .expect("parse fs/list");
                let listing = connection
                    .send_request(request)
                    .block_task()
                    .await
                    .expect("fs/list must answer over v2");
                assert!(
                    listing.path.is_empty(),
                    "a root listing names no directory, got {:?}",
                    listing.path
                );

                // --- queue/list: an idle session has nothing queued ---
                let request = ext::ExtQueueListRequest::parse_message(
                    ext::QUEUE_LIST_METHOD,
                    &serde_json::json!({ "sessionId": session_id }),
                )
                .expect("parse queue/list");
                let listed = connection
                    .send_request(request)
                    .block_task()
                    .await
                    .expect("queue/list must answer over v2");
                assert!(
                    listed.items.is_empty(),
                    "nothing was queued, got {:?}",
                    listed.items
                );

                // --- steer on an idle session: a domain error, not an unknown method ---
                let request = ext::ExtSteerRequest::parse_message(
                    ext::STEER_METHOD,
                    &serde_json::json!({
                        "sessionId": session_id,
                        "text": "adjust the plan",
                        "clientRequestId": "steer-1",
                    }),
                )
                .expect("parse steer");
                if let Err(error) = connection.send_request(request).block_task().await {
                    assert_routed(ext::STEER_METHOD, &error);
                }

                // --- queue on a non-durable session: refused by the driver, still routed ---
                let request = ext::ExtQueueRequest::parse_message(
                    ext::QUEUE_METHOD,
                    &serde_json::json!({
                        "sessionId": session_id,
                        "content": [{"type": "text", "text": "queued"}],
                        "clientRequestId": "queue-1",
                    }),
                )
                .expect("parse queue");
                if let Err(error) = connection.send_request(request).block_task().await {
                    assert_routed(ext::QUEUE_METHOD, &error);
                }

                // --- queue/remove for a turn that does not exist ---
                let request = ext::ExtQueueRemoveRequest::parse_message(
                    ext::QUEUE_REMOVE_METHOD,
                    &serde_json::json!({
                        "sessionId": session_id,
                        "turnId": "missing-turn",
                        "clientRequestId": "remove-1",
                    }),
                )
                .expect("parse queue/remove");
                if let Err(error) = connection.send_request(request).block_task().await {
                    assert_routed(ext::QUEUE_REMOVE_METHOD, &error);
                }

                // --- pause on an idle session ---
                let request = ext::ExtPauseRequest::parse_message(
                    ext::PAUSE_METHOD,
                    &serde_json::json!({
                        "sessionId": session_id,
                        "clientRequestId": "pause-1",
                    }),
                )
                .expect("parse pause");
                if let Err(error) = connection.send_request(request).block_task().await {
                    assert_routed(ext::PAUSE_METHOD, &error);
                }

                // --- resume of a turn that does not exist ---
                let request = ext::ExtResumeRequest::parse_message(
                    ext::RESUME_METHOD,
                    &serde_json::json!({
                        "sessionId": session_id,
                        "turnId": "missing-turn",
                        "clientRequestId": "resume-1",
                    }),
                )
                .expect("parse resume");
                if let Err(error) = connection.send_request(request).block_task().await {
                    assert_routed(ext::RESUME_METHOD, &error);
                }

                // --- respond for a session that does not exist ---
                let request = ext::ExtRespondRequest::parse_message(
                    ext::RESPOND_METHOD,
                    &serde_json::json!({ "sessionId": "missing", "answers": {} }),
                )
                .expect("parse respond");
                let error = connection
                    .send_request(request)
                    .block_task()
                    .await
                    .expect_err("an unknown session cannot be answered");
                assert_routed(ext::RESPOND_METHOD, &error);

                Ok(())
            },
        )
        .await
        .expect("v2 client run");

    agent_task.await.expect("agent task").expect("agent run");
}
