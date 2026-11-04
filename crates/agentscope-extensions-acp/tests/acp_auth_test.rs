//! Terminal auth: what `initialize` advertises and what `auth/status` answers.
//!
//! The setup wizard is advertised unconditionally — the registry's auth check sends
//! `initialize` with empty `clientCapabilities` and rejects an empty `authMethods`
//! list — while `auth/status` only reports what the host's credential probe says.
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AuthCapabilities, AuthMethod, AuthState, AuthStatusRequest, ClientCapabilities,
    InitializeRequest,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

/// A model that always answers "done"; no key and no network is involved.
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

fn factory(root: PathBuf) -> impl Fn(PathBuf, &str) -> HarnessAgent + Send + Sync + 'static {
    move |_cwd, session_id| {
        HarnessAgent::builder()
            .name("acp-auth-test")
            .model(Arc::new(StubModel))
            .session_id(session_id)
            .cwd(root.clone())
            .workspace(root.join(".agentscope").join("workspace"))
            .disable_memory_hooks()
            .max_iters(2)
            .build()
    }
}

fn fresh_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agentscope_acp_auth_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Runs `body` against a live agent, with the given client capabilities and auth probe.
async fn with_agent(
    name: &str,
    capabilities: ClientCapabilities,
    auth_state: AuthState,
    body: impl FnOnce(
        ConnectionTo<Agent>,
        agent_client_protocol::v1::schema::InitializeResponse,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), agent_client_protocol::Error>> + Send>,
    >,
) {
    let root = fresh_root(name);
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root))
        .auth_probe(move || auth_state.clone())
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    Client
        .v1()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(capabilities),
                    )
                    .block_task()
                    .await?;
                body(connection, initialized).await
            },
        )
        .await
        .expect("client script");
    agent_task.abort();
}

#[tokio::test]
async fn a_client_with_empty_capabilities_still_sees_the_setup_wizard() {
    // This is exactly the registry's auth check: `initialize` with empty capabilities,
    // requiring a non-empty `authMethods` whose entry is `agent` or `terminal`.
    with_agent(
        "no-terminal",
        ClientCapabilities::new(),
        AuthState::Authenticated,
        |_connection, initialized| {
            Box::pin(async move {
                assert_eq!(initialized.auth_methods.len(), 1, "exactly one method");
                assert!(
                    matches!(initialized.auth_methods[0], AuthMethod::Terminal(_)),
                    "the registry requires a terminal or agent entry"
                );
                Ok(())
            })
        },
    )
    .await;
}

#[tokio::test]
async fn the_setup_wizard_is_advertised_on_the_wire() {
    let capabilities = ClientCapabilities::new().auth(AuthCapabilities::new().terminal(true));
    with_agent(
        "terminal",
        capabilities,
        AuthState::Authenticated,
        |_connection, initialized| {
            Box::pin(async move {
                assert_eq!(initialized.auth_methods.len(), 1, "exactly one method");
                let AuthMethod::Terminal(terminal) = &initialized.auth_methods[0] else {
                    panic!("a terminal method was advertised, not an agent one");
                };
                assert_eq!(terminal.id.0.as_ref(), "setup");
                assert_eq!(terminal.args, vec!["--setup".to_string()]);

                // The registry parses the JSON, where the variant only survives as
                // the `type` discriminator.
                let wire = serde_json::to_value(&initialized).expect("serialize");
                let method = &wire["authMethods"][0];
                assert_eq!(method["type"], "terminal");
                assert_eq!(method["id"], "setup");
                assert_eq!(method["args"][0], "--setup");
                Ok(())
            })
        },
    )
    .await;
}

#[tokio::test]
async fn auth_status_reports_the_host_credentials() {
    with_agent(
        "status",
        ClientCapabilities::new(),
        AuthState::Unauthenticated,
        |connection, _initialized| {
            Box::pin(async move {
                let status = connection
                    .send_request(AuthStatusRequest::new())
                    .block_task()
                    .await?;
                assert_eq!(status.state, AuthState::Unauthenticated);
                Ok(())
            })
        },
    )
    .await;
}
