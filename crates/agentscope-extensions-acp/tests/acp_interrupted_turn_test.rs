//! Regression: a turn that was interrupted must not wedge the session.
//!
//! Reported symptom: output stops mid-stream, the client receives no error, and sending
//! "continue" produces nothing at all — no timeout, no failure, just an idle connection.
//!
//! Root cause chain (all covered here):
//! * an interrupted run leaves the turn in `interrupted`, which is *not* terminal, so the
//!   dispatcher refuses every later `submit` and never answers it;
//! * the ACP prompt loop only treated `failed` as an error, so `rejected`/`interrupted`
//!   silently produced `end_turn`;
//! * the prompt loop had no deadline, so it waited on that receipt forever.
//!
//! The test drives the real `AcpCodingAgent` over an in-process channel: the first prompt is
//! cancelled mid-run (leaving the journal in the interrupted state), the second prompt must
//! start a fresh run and answer normally.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::v1::{ConnectionTo, SessionMessage};
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

/// Blocks forever on the first call (so the client can cancel the first prompt mid-run and
/// leave the turn interrupted), then answers immediately on every later call.
struct BlockingFirstModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Model for BlockingFirstModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            Box::pin(futures::stream::pending())
        } else {
            Box::pin(futures::stream::once(async {
                Ok(ChatResponse::builder()
                    .content(vec![CoreBlock::Text(TextBlock::new("resumed fine"))])
                    .build())
            }))
        }
    }

    fn model_name(&self) -> &str {
        "blocking-first"
    }
}

#[tokio::test]
async fn a_cancelled_turn_does_not_block_the_next_prompt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = Arc::clone(&calls);
    // The client half watches the same counter, to know when turn 1 has entered the model.
    let calls_for_client = Arc::clone(&calls);
    let (agent_channel, client_channel) = Channel::duplex();

    let agent = AcpCodingAgent::builder()
        .model_factory(move |cwd, sid| {
            let model: Arc<dyn Model> = Arc::new(BlockingFirstModel {
                calls: Arc::clone(&calls_for_factory),
            });
            HarnessAgent::builder()
                .name("acp-interrupt-test")
                .model(model)
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

    let result = Client
        .v1()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;

                // ---- Turn 1: cancel it mid-run, leaving the turn interrupted. ----
                let first = connection.send_request(PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("start working"))],
                ));
                // Wait until the run has actually entered the model before cancelling.
                //
                // The cancel has to hit a turn that is *mid-model-call*: the fixture's model
                // blocks in its first `stream()`, and the assertion at the end of this test
                // requires the model to be called twice (once per prompt), so turn 1 must reach
                // the provider. A fixed delay cannot guarantee that: opening the session journal
                // and committing the turn's pre-model facts (writer lease, `session_input` and
                // `before_model` checkpoints, `model/dispatch`) takes ~0.7-1s on a cold session in
                // a debug build on Windows, well past the 400ms this test used to wait. When the
                // cancel won that race, turn 1 was interrupted before the provider was ever
                // entered, so the fixture's "first call blocks forever" landed on turn 2 -- which
                // nobody cancels -- and the test hung instead of exercising the cancel path.
                // Synchronising on the model call is what the forced-interrupt case below does.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while calls_for_client.load(Ordering::SeqCst) == 0 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the first prompt never reached the model"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                first.cancel()?;
                let first = first.block_task().await?;
                assert_eq!(
                    first.stop_reason,
                    StopReason::Cancelled,
                    "the cancelled prompt must report Cancelled"
                );

                // ---- Turn 2: the user says "continue" on the SAME session. It must run. ----
                let text = Arc::new(std::sync::Mutex::new(String::new()));
                let text_for_handler = Arc::clone(&text);
                let mut session = connection
                    .resume_session(session_id.clone(), std::env::temp_dir())
                    .block_task()
                    .start_session()
                    .await?
                    .session;
                session.send_prompt("continue")?;
                let stop = loop {
                    match session.read_update().await? {
                        SessionMessage::Message(dispatch) => {
                            let text = Arc::clone(&text_for_handler);
                            agent_client_protocol::util::MatchDispatch::new(dispatch.into_raw())
                                .if_notification(async move |notif: SessionNotification| {
                                    if let SessionUpdate::AgentMessageChunk(chunk) = notif.update {
                                        if let ContentBlock::Text(t) = chunk.content {
                                            text.lock().unwrap().push_str(&t.text);
                                        }
                                    }
                                    Ok(())
                                })
                                .await
                                .otherwise_ignore()?;
                        }
                        SessionMessage::PromptResponse(response) => {
                            break response?.stop_reason;
                        }
                        _ => break StopReason::EndTurn,
                    }
                };
                assert_eq!(
                    stop,
                    StopReason::EndTurn,
                    "the follow-up prompt must complete instead of hanging or being refused; \
                     chunks: {:?}",
                    text.lock().unwrap()
                );
                Ok(())
            },
        )
        .await;

    result.expect("client connection should succeed");
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "the follow-up prompt must reach the model"
    );
    let _ = agent_task.await;
}

#[tokio::test]
async fn a_forced_interrupt_is_continued_without_discarding_the_prompt() {
    // A run can be stopped by the *host* (a shutdown sweep, a recovered process, a raced
    // cancel) rather than by this client's cancel. That is not the user's fault: the prompt
    // must not be rejected with `end_turn`/`Refusal` and the turn must not be thrown away.
    // The prompt loop resumes the interrupted turn in place and keeps streaming the answer.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = Arc::clone(&calls);
    let (agent_channel, client_channel) = Channel::duplex();

    let agent = AcpCodingAgent::builder()
        .model_factory(move |cwd, sid| {
            let model: Arc<dyn Model> = Arc::new(BlockingFirstModel {
                calls: Arc::clone(&calls_for_factory),
            });
            HarnessAgent::builder()
                .name("acp-forced-interrupt-test")
                .model(model)
                .session_id(sid)
                .cwd(cwd.clone())
                .workspace(cwd.join(".agentscope").join("workspace"))
                .disable_memory_hooks()
                .max_iters(3)
                .build()
        })
        .build()
        .expect("build AcpCodingAgent");
    let registry = agent.session_registry();
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    Client
        .v1()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let text = Arc::new(std::sync::Mutex::new(String::new()));
                let text_for_handler = Arc::clone(&text);
                let mut session = connection
                    .build_session(std::env::temp_dir())
                    .block_task()
                    .start_session()
                    .await?
                    .session;
                // The session the SDK created above is the one this prompt runs on.
                let session_id = session.session_id().to_string();
                session.send_prompt("start working")?;

                // Once the model has been entered, the run is live and `interrupt()` targets it.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while calls.load(Ordering::SeqCst) == 0 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the prompt never reached the model"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                // `interrupt()` targets whatever run is active; the model call above proves the
                // run is already registered, so the signal cannot be dropped on an idle agent.
                let target = registry
                    .lock()
                    .unwrap()
                    .get(&session_id)
                    .cloned()
                    .expect("the session must be registered while its prompt runs");
                target.agent().interrupt();

                let stop = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                    loop {
                        match session.read_update().await? {
                            SessionMessage::Message(dispatch) => {
                                let text = Arc::clone(&text_for_handler);
                                agent_client_protocol::util::MatchDispatch::new(
                                    dispatch.into_raw(),
                                )
                                .if_notification(async move |notif: SessionNotification| {
                                    if let SessionUpdate::AgentMessageChunk(chunk) = notif.update {
                                        if let ContentBlock::Text(t) = chunk.content {
                                            text.lock().unwrap().push_str(&t.text);
                                        }
                                    }
                                    Ok(())
                                })
                                .await
                                .otherwise_ignore()?;
                            }
                            SessionMessage::PromptResponse(response) => {
                                break Ok::<StopReason, agent_client_protocol::Error>(
                                    response?.stop_reason,
                                );
                            }
                            _ => break Ok(StopReason::EndTurn),
                        }
                    }
                })
                .await
                .expect("the interrupted prompt must settle instead of hanging")?;

                assert_eq!(
                    stop,
                    StopReason::EndTurn,
                    "a forced interrupt must continue the turn, not refuse it; chunks: {:?}",
                    text.lock().unwrap()
                );
                let seen = text.lock().unwrap().clone();
                assert!(
                    seen.contains("resumed fine"),
                    "the resumed run's answer must reach the client, got {seen:?}"
                );
                assert!(
                    !seen.contains("[Agent refused]"),
                    "a forced interrupt must not surface as a refusal, got {seen:?}"
                );
                Ok(())
            },
        )
        .await
        .expect("client connection should succeed");
    let _ = agent_task.await;
}

/// A model that always fails, so a `failed` turn is followed by a new prompt.
struct AlwaysFailing;

#[async_trait::async_trait]
impl Model for AlwaysFailing {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Err(agentscope_core::error::Error::Agent(
                "provider exploded".into(),
            ))
        }))
    }

    fn model_name(&self) -> &str {
        "always-failing"
    }
}

#[tokio::test]
async fn a_failed_turn_reports_the_error_and_does_not_wedge_the_session() {
    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .model_factory(|cwd, sid| {
            HarnessAgent::builder()
                .name("acp-fail-test")
                .model(Arc::new(AlwaysFailing) as Arc<dyn Model>)
                .session_id(sid)
                .cwd(cwd.clone())
                .workspace(cwd.join(".agentscope").join("workspace"))
                .disable_memory_hooks()
                .max_iters(1)
                .build()
        })
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
                let _session_id = connection
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;

                let text = Arc::new(std::sync::Mutex::new(String::new()));
                let text_for_handler = Arc::clone(&text);
                let mut session = connection
                    .build_session(std::env::temp_dir())
                    .block_task()
                    .start_session()
                    .await?
                    .session;
                session.send_prompt("do it")?;
                let stop = loop {
                    match session.read_update().await? {
                        SessionMessage::Message(dispatch) => {
                            let text = Arc::clone(&text_for_handler);
                            agent_client_protocol::util::MatchDispatch::new(dispatch.into_raw())
                                .if_notification(async move |notif: SessionNotification| {
                                    if let SessionUpdate::AgentMessageChunk(chunk) = notif.update {
                                        if let ContentBlock::Text(t) = chunk.content {
                                            text.lock().unwrap().push_str(&t.text);
                                        }
                                    }
                                    Ok(())
                                })
                                .await
                                .otherwise_ignore()?;
                        }
                        SessionMessage::PromptResponse(response) => {
                            break response?.stop_reason;
                        }
                        _ => break StopReason::EndTurn,
                    }
                };
                assert_eq!(
                    stop,
                    StopReason::Refusal,
                    "a failed run must be reported as a refusal, never as a silent end_turn"
                );
                let seen = text.lock().unwrap().clone();
                assert!(
                    seen.contains("provider exploded"),
                    "the provider failure must reach the client, got {seen:?}"
                );
                Ok(())
            },
        )
        .await
        .expect("client connection should succeed");
    let _ = agent_task.await;
}
