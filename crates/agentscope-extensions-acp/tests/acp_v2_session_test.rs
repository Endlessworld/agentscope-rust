//! End-to-end test of the protocol v2 handler graph.
//!
//! A v2 client talks to the same [`AcpCodingAgent`] the v1 suites drive: `initialize` with
//! `protocolVersion: 2` must reach the v2 graph (through the SDK protocol router), the session
//! lifecycle must work with the v2 wire shapes, and a v1 client must keep negotiating exactly
//! as before.
#![cfg(feature = "acp-v2")]

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::schema as v2;
use agent_client_protocol::v2::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_extensions_acp::config::ModelChoice;
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

fn session_factory(
    root: PathBuf,
) -> impl Fn(PathBuf, &str) -> HarnessAgent + Send + Sync + 'static {
    move |_cwd, session_id| {
        HarnessAgent::builder()
            .name("acp-v2-test")
            .model(Arc::new(common::StubModel::new("done")))
            .session_id(session_id)
            .cwd(root.clone())
            .workspace(root.join(".agentscope").join("workspace"))
            .disable_memory_hooks()
            .max_iters(3)
            .build()
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agentscope_acp_v2_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Waits for the `idle` snapshot that closes the turn and returns its stop reason.
async fn wait_for_idle(
    updates: &Arc<Mutex<Vec<v2::SessionUpdate>>>,
    timeout: Duration,
) -> v2::StopReason {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(reason) = updates
            .lock()
            .unwrap()
            .iter()
            .find_map(|update| match update {
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                    idle.stop_reason.clone()
                }
                _ => None,
            })
        {
            return reason;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the turn to go idle"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn v2_session_lifecycle_round_trip() {
    let root = temp_root("lifecycle");
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(session_factory(root.clone()))
        // One published model, so `session/set_config_option` has a value the session accepts.
        .models(vec![ModelChoice {
            id: "stub".to_string(),
            name: "Stub".to_string(),
            provider: None,
            catalog_key: None,
            base_url: None,
        }])
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let updates: Arc<Mutex<Vec<v2::SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&updates);
    let client_root = root.clone();

    Client
        .v2()
        .on_receive_notification(
            async move |notification: v2::UpdateSessionNotification, _cx| {
                collected.lock().unwrap().push(notification.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                // --- initialize: the router must pick the v2 graph ---
                let init = connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("v2-test-client", "0.0.0"),
                    ))
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V2);
                assert!(
                    init.capabilities.session.is_some(),
                    "v2 agents must advertise the session capability block"
                );
                assert!(
                    matches!(init.auth_methods.first(), Some(v2::AuthMethod::Terminal(_))),
                    "the setup flow is advertised as a terminal auth method"
                );

                // --- session/new ---
                let new = connection
                    .send_request(v2::NewSessionRequest::new(client_root.clone()))
                    .block_task()
                    .await?;
                let session_id = new.session_id.clone();
                let model_option = new
                    .config_options
                    .iter()
                    .find(|option| option.config_id.to_string() == "model")
                    .expect("the model option must be published");
                let current_model = match &model_option.kind {
                    v2::SessionConfigKind::Select(select) => select.current_value.clone(),
                    other => panic!("the model option must be a select, got {other:?}"),
                };
                // Use a value the session actually publishes: the factory-built session has no
                // `default` entry, so its own option list is the only valid source.
                let target_model = match &model_option.kind {
                    v2::SessionConfigKind::Select(select) => match &select.options {
                        v2::SessionConfigSelectOptions::Ungrouped(list) => {
                            list.first().map(|option| option.value.clone())
                        }
                        v2::SessionConfigSelectOptions::Grouped(groups) => groups
                            .first()
                            .and_then(|group| group.options.first())
                            .map(|option| option.value.clone()),
                        _ => None,
                    },
                    _ => None,
                }
                .unwrap_or(current_model);
                assert!(
                    !new.available_commands.is_empty(),
                    "the compact command must be advertised"
                );

                // --- session/prompt: acknowledged with a message id, turn reported as updates ---
                let prompt = connection
                    .send_request(v2::PromptRequest::new(
                        session_id.clone(),
                        vec![v2::ContentBlock::Text(v2::TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;
                assert!(
                    !prompt.message_id.to_string().is_empty(),
                    "the prompt is acknowledged with the user message id"
                );
                let reason = wait_for_idle(&updates, Duration::from_secs(30)).await;
                assert_eq!(reason, v2::StopReason::EndTurn);
                {
                    let seen = updates.lock().unwrap();
                    assert!(
                        seen.iter()
                            .any(|update| matches!(update, v2::SessionUpdate::UserMessage(_))),
                        "the prompt is echoed as a user message"
                    );
                    // The reply keeps the turn's message id, so every chunk of one assistant
                    // message groups under a single id on the client.
                    let reply: Vec<String> = seen
                        .iter()
                        .filter_map(|update| match update {
                            v2::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                                v2::ContentBlock::Text(text) => Some(text.text.clone()),
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect();
                    assert!(
                        reply.iter().any(|text| text == "done"),
                        "the reply streams as agent message chunks, got {reply:?}"
                    );
                }

                // --- session/set_config_option: re-select the current model ---
                let configured = connection
                    .send_request(v2::SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        "model",
                        v2::SessionConfigOptionValue::Id {
                            value: target_model.clone(),
                        },
                    ))
                    .block_task()
                    .await?;
                assert!(
                    configured
                        .config_options
                        .iter()
                        .any(|option| option.config_id.to_string() == "model"),
                    "the response republishes the options"
                );

                // --- session/status ---
                let status = connection
                    .send_request(v2::SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, v2::SessionStatus::Open);

                // --- session/set_title + session/list ---
                let titled = connection
                    .send_request(v2::SetSessionTitleRequest::new(
                        session_id.clone(),
                        "v2 smoke",
                    ))
                    .block_task()
                    .await?;
                assert_eq!(titled.title, "v2 smoke");
                let listed = connection
                    .send_request(
                        v2::ListSessionsRequest::new().cwd(Some(client_root.clone().into())),
                    )
                    .block_task()
                    .await?;
                assert!(
                    listed.sessions.iter().any(|info| {
                        info.session_id.to_string() == session_id.to_string()
                            && info.title.as_deref() == Some("v2 smoke")
                    }),
                    "the session list carries the new title"
                );

                // --- session/archive + unarchive ---
                connection
                    .send_request(v2::ArchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                let archived = connection
                    .send_request(
                        v2::ListSessionsRequest::new()
                            .cwd(Some(client_root.clone().into()))
                            .archived(v2::SessionArchiveFilter::Only),
                    )
                    .block_task()
                    .await?;
                assert!(
                    archived
                        .sessions
                        .iter()
                        .any(|info| info.session_id.to_string() == session_id.to_string()),
                    "the archived session only shows up in the archived filter"
                );
                connection
                    .send_request(v2::UnarchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                // --- session/resume on an unknown id is `session_not_found` ---
                let unknown = v2::SessionId::new("session-does-not-exist");
                assert!(
                    connection
                        .send_request(v2::ResumeSessionRequest::new(
                            unknown.clone(),
                            client_root.clone(),
                        ))
                        .block_task()
                        .await
                        .is_err(),
                    "resuming a session that was never created must fail"
                );

                // --- session/close then delete ---
                connection
                    .send_request(v2::CloseSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                connection
                    .send_request(v2::DeleteSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                Ok(())
            },
        )
        .await
        .expect("v2 client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

/// `session/options` is a snapshot: it answers for a new session and for a live one, and an
/// unknown id is an error rather than a silently empty answer.
#[tokio::test]
async fn v2_session_options_snapshot() {
    let root = temp_root("options");
    let client_root = root.clone();
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(session_factory(root.clone()))
        .models(vec![ModelChoice {
            id: "stub".to_string(),
            name: "Stub".to_string(),
            provider: None,
            catalog_key: None,
            base_url: None,
        }])
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    Client
        .v2()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                let init = connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("v2-options-test", "0.0.0"),
                    ))
                    .block_task()
                    .await?;
                let session_caps = init
                    .capabilities
                    .session
                    .expect("v2 agents advertise the session capability block");
                assert!(
                    session_caps.options.is_some(),
                    "`session/options` is implemented, so the capability must be advertised"
                );

                // A new session's snapshot needs no session at all.
                let fresh = connection
                    .send_request(v2::SessionOptionsRequest::new(client_root.clone()))
                    .block_task()
                    .await?;
                let fresh_ids: Vec<String> = fresh
                    .config_options
                    .iter()
                    .map(|option| option.config_id.to_string())
                    .collect();
                assert_eq!(fresh_ids, vec!["model", "mode", "thought_level"]);
                assert!(
                    !fresh.available_commands.is_empty(),
                    "the new-session snapshot advertises the commands too"
                );

                // With a live session it reports that session's own snapshot.
                let new = connection
                    .send_request(v2::NewSessionRequest::new(client_root.clone()))
                    .block_task()
                    .await?;
                let known = connection
                    .send_request(
                        v2::SessionOptionsRequest::new(client_root.clone())
                            .session_id(new.session_id.clone()),
                    )
                    .block_task()
                    .await?;
                let known_ids: Vec<String> = known
                    .config_options
                    .iter()
                    .map(|option| option.config_id.to_string())
                    .collect();
                assert_eq!(known_ids, fresh_ids);

                // An unknown id must fail: an empty snapshot would look like "no options".
                let error = connection
                    .send_request(
                        v2::SessionOptionsRequest::new(client_root.clone())
                            .session_id(v2::SessionId::new("session-does-not-exist")),
                    )
                    .block_task()
                    .await
                    .expect_err("an unknown session cannot have options");
                assert_eq!(
                    error.code,
                    agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
                );

                Ok(())
            },
        )
        .await
        .expect("v2 client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

/// The router must keep serving v1 clients byte-for-byte as before.
#[tokio::test]
async fn v1_client_still_negotiates_through_the_router() {
    use agent_client_protocol::v1::schema as v1;

    let root = temp_root("v1");
    let client_root = root.clone();
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(session_factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: agent_client_protocol::v1::ConnectionTo<Agent>| async move {
                let init = connection
                    .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V1);
                let new = connection
                    .send_request(v1::NewSessionRequest::new(client_root.clone()))
                    .block_task()
                    .await?;
                let prompt = connection
                    .send_request(v1::PromptRequest::new(
                        new.session_id.clone(),
                        vec![v1::ContentBlock::Text(v1::TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(prompt.stop_reason, v1::StopReason::EndTurn);
                Ok(())
            },
        )
        .await
        .expect("v1 client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}
