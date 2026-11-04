//! `initialize` capability advertisement plus the read-only methods added with it:
//! `session/set_title` (ACP-33), `session/status` (ACP-34), `auth/status` (ACP-35),
//! `session/archive` + `session/list` filtering (ACP-36), the `additionalDirectories`
//! advertisement (ACP-37) and the client `systemPrompt` plumbing (ACP-38).
//!
//! Everything runs in-process over `Channel::duplex` with a scripted model, so no key or
//! network is involved.
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    ArchiveSessionRequest, AuthState, AuthStatusRequest, ContentBlock, DeleteSessionRequest,
    InitializeRequest, ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    ResumeSessionRequest, SessionArchiveFilter, SessionId, SessionStatus, SessionStatusRequest,
    SetSessionTitleRequest, SystemPrompt, SystemPromptMode, TextContent, UnarchiveSessionRequest,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client, ErrorCode};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::{AcpCodingAgent, SessionBuild};
use agentscope_harness::agent::HarnessAgent;

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

/// Session factory mirroring the binary's persistence wiring: the ACP session id is the
/// journal key, so a fresh agent can find the session on disk.
fn factory(root: PathBuf) -> impl Fn(PathBuf, &str) -> HarnessAgent + Send + Sync + 'static {
    move |_cwd, session_id| {
        HarnessAgent::builder()
            .name("acp-capabilities-test")
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
    let root = std::env::temp_dir().join(format!("agentscope_acp_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn initialize_advertises_the_new_capabilities_and_they_work() {
    let root = fresh_root("caps");
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let root_for_client = root.clone();
    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let caps = initialized.agent_capabilities;
                let session = caps.session_capabilities;
                assert!(
                    session.additional_directories.is_some(),
                    "ACP-37: additionalDirectories must be advertised"
                );
                assert!(session.title.is_some(), "ACP-33: title must be advertised");
                assert!(
                    session.status.is_some(),
                    "ACP-34: status must be advertised"
                );
                assert!(
                    session.system_prompt.is_none(),
                    "ACP-38: `model_factory` cannot carry the text, so it must not be advertised"
                );
                let auth = caps.auth;
                assert!(
                    auth.status.is_some(),
                    "ACP-35: auth.status must be advertised"
                );
                assert!(
                    session.archive.is_some(),
                    "ACP-36: archive must be advertised"
                );

                let auth = connection
                    .send_request(AuthStatusRequest::new())
                    .block_task()
                    .await?;
                assert_eq!(auth.state, AuthState::NotRequired);

                // Sending one anyway (it was not advertised) must be rejected rather than
                // silently dropped: this agent is built by a `model_factory`.
                let error = connection
                    .send_request(
                        NewSessionRequest::new(root_for_client.clone()).system_prompt(
                            SystemPrompt::new(vec![ContentBlock::Text(TextContent::new(
                                "always answer in haiku",
                            ))]),
                        ),
                    )
                    .block_task()
                    .await
                    .expect_err("a model_factory agent cannot honour systemPrompt");
                assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

                let session_id = connection
                    .send_request(NewSessionRequest::new(root_for_client.clone()))
                    .block_task()
                    .await?
                    .session_id;

                // `set_title` normalises whitespace and reports what it stored.
                let titled = connection
                    .send_request(SetSessionTitleRequest::new(
                        session_id.clone(),
                        "  Fix the flaky test  ",
                    ))
                    .block_task()
                    .await?;
                assert_eq!(titled.title, "Fix the flaky test");

                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                let info = listed
                    .sessions
                    .iter()
                    .find(|info| info.session_id == session_id)
                    .expect("the session is listed");
                assert_eq!(info.title.as_deref(), Some("Fix the flaky test"));

                // A whitespace-only title is invalid after normalisation (`-32602`).
                let error = connection
                    .send_request(SetSessionTitleRequest::new(session_id.clone(), "   "))
                    .block_task()
                    .await
                    .expect_err("an empty title must be rejected");
                assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

                // In memory → `open`; unknown id → `-32002`.
                let status = connection
                    .send_request(SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Open);
                let error = connection
                    .send_request(SessionStatusRequest::new(SessionId::new(
                        "session-does-not-exist",
                    )))
                    .block_task()
                    .await
                    .expect_err("an unknown session must be reported as not found");
                assert_eq!(error.code, ErrorCode::RESOURCE_NOT_FOUND);
                Ok(())
            },
        )
        .await
        .expect("client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_session_that_is_only_persisted_reports_saved() {
    let root = fresh_root("caps_saved");

    // ---- Process 1: one turn, so the journal exists on disk. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    let session_id = Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
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
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;
                Ok(session_id)
            },
        )
        .await
        .expect("client run 1");
    let _ = agent_task.await;

    // ---- Process 2: empty registry, same workspace. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                // `session/status` carries no cwd, so the persisted probe only covers
                // workspaces this process already knows: listing first is what makes the
                // journal reachable (the client's normal order).
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(
                    listed
                        .sessions
                        .iter()
                        .any(|info| info.session_id == session_id),
                    "the persisted session is listed after the restart"
                );

                let status = connection
                    .send_request(SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Saved);

                connection
                    .send_request(LoadSessionRequest::new(
                        session_id.clone(),
                        root_for_client.clone(),
                    ))
                    .block_task()
                    .await?;
                let status = connection
                    .send_request(SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Open);
                Ok(())
            },
        )
        .await
        .expect("client run 2");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn archiving_a_live_session_hides_it_until_it_is_unarchived() {
    let root = fresh_root("caps_archive");
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let root_for_client = root.clone();
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
                    .send_request(NewSessionRequest::new(root_for_client.clone()))
                    .block_task()
                    .await?
                    .session_id;
                // A turn first, so the session also exists in the journal.
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;

                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(
                    listed
                        .sessions
                        .iter()
                        .any(|info| info.session_id == session_id),
                    "a fresh session is listed by default"
                );

                connection
                    .send_request(ArchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                // Default (`exclude`) hides it; `only` is the way to see archived sessions.
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(
                    !listed
                        .sessions
                        .iter()
                        .any(|info| info.session_id == session_id),
                    "an archived session must not appear in the default listing"
                );
                let archived_only = connection
                    .send_request(
                        ListSessionsRequest::new()
                            .cwd(root_for_client.clone())
                            .archived(SessionArchiveFilter::Only),
                    )
                    .block_task()
                    .await?;
                let info = archived_only
                    .sessions
                    .iter()
                    .find(|info| info.session_id == session_id)
                    .expect("the archived session is listed by `only`");
                assert_eq!(info.archived, Some(true));

                // Archiving releases the live session, exactly like `session/close`.
                let status = connection
                    .send_request(SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Saved);

                connection
                    .send_request(UnarchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                let info = listed
                    .sessions
                    .iter()
                    .find(|info| info.session_id == session_id)
                    .expect("an unarchived session is listed by default again");
                assert_eq!(info.archived, Some(false));
                Ok(())
            },
        )
        .await
        .expect("client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn archive_state_survives_a_restart() {
    let root = fresh_root("caps_archive_restart");

    // ---- Process 1: one turn, then archive. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    let session_id = Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
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
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(ArchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                Ok(session_id)
            },
        )
        .await
        .expect("client run 1");
    let _ = agent_task.await;

    // ---- Process 2: the archive marker must still hide it. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(
                    !listed
                        .sessions
                        .iter()
                        .any(|info| info.session_id == session_id),
                    "the archived session stays hidden after a restart"
                );
                let archived_only = connection
                    .send_request(
                        ListSessionsRequest::new()
                            .cwd(root_for_client.clone())
                            .archived(SessionArchiveFilter::Only),
                    )
                    .block_task()
                    .await?;
                let info = archived_only
                    .sessions
                    .iter()
                    .find(|info| info.session_id == session_id)
                    .expect("the persisted archived session is still listed by `only`");
                assert_eq!(info.archived, Some(true));

                // A persisted-only session can be unarchived: the workspace probe finds it.
                connection
                    .send_request(UnarchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(
                    listed
                        .sessions
                        .iter()
                        .any(|info| info.session_id == session_id),
                    "unarchiving restores the session in the default listing"
                );
                Ok(())
            },
        )
        .await
        .expect("client run 2");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_client_system_prompt_reaches_the_session_factory_and_override_is_rejected() {
    let root = fresh_root("caps_sysprompt");
    let seen: Arc<std::sync::Mutex<Vec<Option<String>>>> = Arc::new(std::sync::Mutex::new(vec![]));
    let seen_by_factory = Arc::clone(&seen);
    let root_for_factory = root.clone();

    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .session_factory(move |build: SessionBuild| {
            seen_by_factory
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(build.system_prompt.clone());
            let prompt = build.system_prompt.unwrap_or_default();
            HarnessAgent::builder()
                .name("acp-capabilities-test")
                .model(Arc::new(StubModel))
                .session_id(build.session_id)
                .cwd(root_for_factory.clone())
                .workspace(root_for_factory.join(".agentscope").join("workspace"))
                .sys_prompt(prompt)
                .disable_memory_hooks()
                .max_iters(2)
                .build()
        })
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let root_for_client = root.clone();
    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                // A `session_factory` can carry the text, so the capability is advertised.
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .system_prompt
                        .is_some(),
                    "ACP-38: a session factory must advertise systemPrompt"
                );
                // `append` is the only supported mode, and the text reaches the factory.
                connection
                    .send_request(
                        NewSessionRequest::new(root_for_client.clone()).system_prompt(
                            SystemPrompt::new(vec![ContentBlock::Text(TextContent::new(
                                "always answer in haiku",
                            ))]),
                        ),
                    )
                    .block_task()
                    .await?;
                let error = connection
                    .send_request(
                        NewSessionRequest::new(root_for_client.clone()).system_prompt(
                            SystemPrompt::new(vec![ContentBlock::Text(TextContent::new("hijack"))])
                                .mode(SystemPromptMode::Override),
                        ),
                    )
                    .block_task()
                    .await
                    .expect_err("override must be rejected, not downgraded to append");
                assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
                Ok(())
            },
        )
        .await
        .expect("client run");
    let _ = agent_task.await;

    let seen = seen
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(seen, vec![Some("always answer in haiku".to_string())]);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn archiving_a_session_that_never_prompted_keeps_it_resolvable() {
    let root = fresh_root("caps_archive_unprompted");
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let root_for_client = root.clone();
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
                    .send_request(NewSessionRequest::new(root_for_client.clone()))
                    .block_task()
                    .await?
                    .session_id;

                // No prompt: the journal does not exist yet.
                connection
                    .send_request(ArchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                // Archiving released the live session, and `session/status` carries no cwd, so
                // the workspace must still be reachable without the registry.
                let status = connection
                    .send_request(SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Saved);

                // The first unarchive clears the marker, so the second call has neither marker
                // nor journal to find: it must be a no-op, not `session_not_found`.
                connection
                    .send_request(UnarchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                connection
                    .send_request(UnarchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                let info = listed
                    .sessions
                    .iter()
                    .find(|info| info.session_id == session_id)
                    .expect("the unarchived session is listed again");
                assert_eq!(info.archived, Some(false));
                Ok(())
            },
        )
        .await
        .expect("client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_session_created_without_a_prompt_survives_a_restart() {
    let root = fresh_root("caps_unprompted_restart");

    // ---- Process 1: create the session, never send a prompt. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    let session_id = Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(root_for_client.clone()))
                    .block_task()
                    .await?
                    .session_id;
                Ok(session_id)
            },
        )
        .await
        .expect("client run 1");
    let _ = agent_task.await;

    // ---- Process 2: only the creation record exists, there is no journal. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                let info = listed
                    .sessions
                    .iter()
                    .find(|info| info.session_id == session_id)
                    .expect("a session created without a prompt is still listed");
                assert_eq!(info.archived, Some(false));

                // `include` must not be the only way to see it either: the default filter
                // already has to keep an unarchived session.
                let listed = connection
                    .send_request(
                        ListSessionsRequest::new()
                            .cwd(root_for_client.clone())
                            .archived(SessionArchiveFilter::Include),
                    )
                    .block_task()
                    .await?;
                assert!(listed
                    .sessions
                    .iter()
                    .any(|info| info.session_id == session_id));

                let status = connection
                    .send_request(SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Saved);

                connection
                    .send_request(ResumeSessionRequest::new(
                        session_id.clone(),
                        root_for_client.clone(),
                    ))
                    .block_task()
                    .await?;
                let status = connection
                    .send_request(SessionStatusRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Open);
                Ok(())
            },
        )
        .await
        .expect("client run 2");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn deleting_a_session_created_without_a_prompt_hides_it_again() {
    let root = fresh_root("caps_unprompted_delete");
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let root_for_client = root.clone();
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
                    .send_request(NewSessionRequest::new(root_for_client.clone()))
                    .block_task()
                    .await?
                    .session_id;
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(listed
                    .sessions
                    .iter()
                    .any(|info| info.session_id == session_id));

                connection
                    .send_request(DeleteSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                // The creation record is the only trace this session had: leaving it behind
                // would put a deleted session back into the list.
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(
                    listed
                        .sessions
                        .iter()
                        .all(|info| info.session_id != session_id),
                    "a deleted session must not be listed"
                );
                Ok(())
            },
        )
        .await
        .expect("client run");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn deleting_a_persisted_session_removes_its_history() {
    let root = fresh_root("caps_delete_history");

    // ---- Process 1: one turn, so the journal is the only trace on disk. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    let session_id = Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
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
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;
                Ok(session_id)
            },
        )
        .await
        .expect("client run 1");
    let _ = agent_task.await;

    // ---- Process 2: the restart knows the session only through its journal. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(listed
                    .sessions
                    .iter()
                    .any(|info| info.session_id == session_id));

                connection
                    .send_request(DeleteSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                // The journal was the only record left: if it survived, the session would be
                // discovered again and could still be loaded.
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root_for_client.clone()))
                    .block_task()
                    .await?;
                assert!(
                    listed
                        .sessions
                        .iter()
                        .all(|info| info.session_id != session_id),
                    "a deleted session must not be listed"
                );
                let error = connection
                    .send_request(LoadSessionRequest::new(
                        session_id.clone(),
                        root_for_client.clone(),
                    ))
                    .block_task()
                    .await
                    .expect_err("a deleted session has no history to load");
                assert_eq!(error.code, ErrorCode::RESOURCE_NOT_FOUND);
                Ok(())
            },
        )
        .await
        .expect("client run 2");
    let _ = agent_task.await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_unarchived_session_replays_its_history_and_keeps_going() {
    let root = fresh_root("caps_unarchive_continue");

    // ---- Process 1: one turn, then archive. ----
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    let session_id = Client
        .v1()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
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
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(ArchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                Ok(session_id)
            },
        )
        .await
        .expect("client run 1");
    let _ = agent_task.await;

    // ---- Process 2: unarchive, load (the transcript replays), then another turn. ----
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(factory(root.clone()))
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });
    let root_for_client = root.clone();
    Client
        .v1()
        .on_receive_notification(
            async move |notification: agent_client_protocol::v1::schema::SessionNotification,
                        _cx| {
                use agent_client_protocol::v1::schema::SessionUpdate;
                let text = match notification.update {
                    SessionUpdate::UserMessageChunk(chunk) => match chunk.content {
                        ContentBlock::Text(text) => format!("USER:{}", text.text),
                        _ => return Ok(()),
                    },
                    SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                        ContentBlock::Text(text) => format!("AGENT:{}", text.text),
                        _ => return Ok(()),
                    },
                    _ => return Ok(()),
                };
                seen_c.lock().unwrap().push(text);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                // `session/unarchive` carries no cwd, so the client names the workspace with a
                // listing first (what an IDE does when it restores its history). The archived
                // session is only visible with the `include` filter.
                let listed = connection
                    .send_request(
                        ListSessionsRequest::new()
                            .cwd(root_for_client.clone())
                            .archived(SessionArchiveFilter::Include),
                    )
                    .block_task()
                    .await?;
                let info = listed
                    .sessions
                    .iter()
                    .find(|info| info.session_id == session_id)
                    .expect("an archived session is listed by `include`");
                assert_eq!(info.archived, Some(true));
                connection
                    .send_request(UnarchiveSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                // Loading the unarchived session replays what the first process committed.
                connection
                    .send_request(LoadSessionRequest::new(
                        session_id.clone(),
                        root_for_client.clone(),
                    ))
                    .block_task()
                    .await?;
                // ...and the conversation continues in the same journal.
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("again"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            },
        )
        .await
        .expect("client run 2");
    let _ = agent_task.await;

    let texts = seen.lock().unwrap().clone();
    assert!(
        texts.iter().any(|text| text == "USER:hello"),
        "the replayed history is missing: {texts:?}"
    );
    assert!(
        texts.iter().filter(|text| *text == "AGENT:done").count() >= 2,
        "the continued turn was not streamed: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
