//! End-to-end test for the `ask_user` elicitation tool.
//!
//! Uses an in-process [`Channel::duplex`] transport (no subprocess, no real
//! model): the agent side runs the full [`AcpCodingAgent`] handler chain with a
//! scripted model that first calls `ask_user` and then finishes, while the
//! client side advertises `elicitation.form`, answers the resulting
//! `elicitation/create` with an `accept`, and asserts the answer reached the
//! model as the tool result.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
    ElicitationFormCapabilities, InitializeRequest, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, SessionMessage};
use agentscope_core::message::{ContentBlock as CoreBlock, JsonMap, Msg, TextBlock, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::Toolkit;
use agentscope_extensions_acp::{AcpCodingAgent, AskUserTool, SessionBuild};
use agentscope_harness::agent::HarnessAgent;

/// Records every message list the model was asked to complete, so the test can
/// assert the form answer was fed back as a tool result.
type Seen = Arc<Mutex<Vec<Vec<Msg>>>>;

/// A scripted model: yields the queued responses in order, then a plain final
/// reply (so the loop terminates once the tool result is in).
struct ScriptedModel {
    responses: Mutex<Vec<ChatResponse>>,
    seen: Seen,
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen.lock().unwrap().push(messages);
        let responses = Arc::new(Mutex::new(std::mem::take(
            &mut *self.responses.lock().unwrap(),
        )));
        Box::pin(futures::stream::once(async move {
            let mut queued = responses.lock().unwrap();
            Ok(if queued.is_empty() {
                ChatResponse::builder()
                    .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                    .build()
            } else {
                queued.remove(0)
            })
        }))
    }

    fn model_name(&self) -> &str {
        "scripted-ask-user"
    }
}

/// First model response: call `ask_user` with a single-select form.
fn ask_user_response() -> ChatResponse {
    let mut input = JsonMap::new();
    input.insert("message".to_string(), serde_json::json!("Which strategy?"));
    input.insert(
        "properties".to_string(),
        serde_json::json!({
            "strategy": {"type": "string", "enum": ["a", "b"]}
        }),
    );
    input.insert("required".to_string(), serde_json::json!(["strategy"]));
    ChatResponse::builder()
        .content(vec![CoreBlock::ToolUse(ToolUseBlock::new(
            "call-1", "ask_user", input,
        ))])
        .build()
}

/// Mirrors the production factory: `ask_user` is baked into the base toolkit at
/// build time *only* when the client advertised form elicitation (which also
/// makes subagents inherit it, since the blueprint snapshots the toolkit once).
fn build_agent(build: SessionBuild, seen: Seen) -> HarnessAgent {
    let model: Arc<dyn Model> = Arc::new(ScriptedModel {
        responses: Mutex::new(vec![ask_user_response()]),
        seen,
    });
    let mut builder = HarnessAgent::builder()
        .name("acp-elicitation-test")
        .model(model)
        .max_iters(5);
    if build.client_support.supports_elicitation_form() {
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(AskUserTool::new(
            build.elicitation_slot,
            SessionId::new(build.session_id),
        )));
        builder = builder.toolkit(toolkit);
    }
    builder.build()
}

fn accept_with(strategy: &str) -> CreateElicitationResponse {
    let mut content = BTreeMap::new();
    content.insert(
        "strategy".to_string(),
        ElicitationContentValue::String(strategy.to_string()),
    );
    CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(content),
    ))
}

#[tokio::test]
async fn ask_user_pushes_a_form_and_feeds_the_answer_back() {
    let (agent_channel, client_channel) = Channel::duplex();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));

    let agent = AcpCodingAgent::builder()
        .session_factory({
            let seen = Arc::clone(&seen);
            move |build: SessionBuild| build_agent(build, Arc::clone(&seen))
        })
        .build()
        .expect("build AcpCodingAgent");
    // No legacy factory: the session factory must drive session creation.
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let forms: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let tools = Arc::new(AtomicUsize::new(0));

    let result = Client
        .builder()
        .connect_with(client_channel, {
            let forms = Arc::clone(&forms);
            let tools = Arc::clone(&tools);
            move |connection: ConnectionTo<Agent>| {
                let forms = Arc::clone(&forms);
                let tools = Arc::clone(&tools);
                async move {
                    connection
                        .send_request(
                            InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                                ClientCapabilities::new().elicitation(
                                    ElicitationCapabilities::new()
                                        .form(ElicitationFormCapabilities::new()),
                                ),
                            ),
                        )
                        .block_task()
                        .await?;

                    let mut session = connection
                        .build_session(std::env::temp_dir())
                        .block_task()
                        .start_session()
                        .await?;
                    session.send_prompt("please ask me")?;

                    loop {
                        let update = session.read_update().await?;
                        match update {
                            SessionMessage::SessionMessage(dispatch) => {
                                let forms = Arc::clone(&forms);
                                let tools = Arc::clone(&tools);
                                MatchDispatch::new(dispatch)
                                    .if_notification(async move |notif: SessionNotification| {
                                        if let SessionUpdate::ToolCall(tc) = &notif.update {
                                            if tc.name.as_deref() == Some("ask_user") {
                                                tools.fetch_add(1, Ordering::SeqCst);
                                            }
                                        }
                                        Ok(())
                                    })
                                    .await
                                    .if_request(
                                        async move |req: CreateElicitationRequest, responder| {
                                            forms.lock().unwrap().push(serde_json::to_value(&req)?);
                                            responder.respond(accept_with("a"))?;
                                            Ok(())
                                        },
                                    )
                                    .await
                                    .otherwise_ignore()?;
                            }
                            SessionMessage::StopReason(reason) => {
                                assert_eq!(reason, StopReason::EndTurn);
                                break;
                            }
                            _ => break,
                        }
                    }
                    Ok(())
                }
            }
        })
        .await;

    result.expect("client connection should succeed");
    let _ = agent_task.await;

    // The agent called the tool exactly once and pushed one form.
    assert_eq!(
        tools.load(Ordering::SeqCst),
        1,
        "ask_user should have run once"
    );
    let forms = forms.lock().unwrap();
    assert_eq!(
        forms.len(),
        1,
        "exactly one elicitation/create should arrive"
    );

    let form = &forms[0];
    assert_eq!(form["mode"], "form");
    assert_eq!(form["message"], "Which strategy?");
    // Scoped to the session, and tied to the tool call that triggered it.
    assert!(form["sessionId"].is_string());
    assert_eq!(form["toolCallId"], "call-1");
    assert_eq!(
        form["requestedSchema"]["properties"]["strategy"]["enum"],
        serde_json::json!(["a", "b"])
    );
    assert_eq!(form["requestedSchema"]["required"][0], "strategy");

    // The answer made it back to the model as the tool result.
    let seen = seen.lock().unwrap();
    let reached_model = seen.iter().skip(1).any(|messages| {
        messages.iter().any(|message| {
            message.tool_result_blocks().any(|result| {
                result.output.iter().any(|block| {
                    matches!(
                        block,
                        CoreBlock::Text(text) if text.text.contains("\"strategy\": \"a\"")
                    )
                })
            })
        })
    });
    assert!(reached_model, "the form answer must reach the model");
}

#[tokio::test]
async fn ask_user_is_not_exposed_when_the_client_did_not_advertise_elicitation() {
    let (agent_channel, client_channel) = Channel::duplex();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));

    let agent = AcpCodingAgent::builder()
        .session_factory({
            let seen = Arc::clone(&seen);
            move |build: SessionBuild| build_agent(build, Arc::clone(&seen))
        })
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let forms: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));

    let result = Client
        .builder()
        .connect_with(client_channel, {
            let forms = Arc::clone(&forms);
            move |connection: ConnectionTo<Agent>| {
                async move {
                    // Advertise nothing: the tool must not be registered, so the
                    // model's `ask_user` call cannot produce a form.
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;

                    let mut session = connection
                        .build_session(std::env::temp_dir())
                        .block_task()
                        .start_session()
                        .await?;
                    session.send_prompt("please ask me")?;

                    loop {
                        let update = session.read_update().await?;
                        match update {
                            SessionMessage::SessionMessage(dispatch) => {
                                let forms = Arc::clone(&forms);
                                MatchDispatch::new(dispatch)
                                    // The (unregistered) `ask_user` tool call still
                                    // triggers a permission prompt; answering it
                                    // keeps the agent moving so the test can assert
                                    // that no *form* was ever pushed.
                                    .if_request(async |req: RequestPermissionRequest, responder| {
                                        let outcome = req
                                            .options
                                            .iter()
                                            .find(|o| {
                                                matches!(o.kind, PermissionOptionKind::RejectOnce)
                                            })
                                            .map_or(RequestPermissionOutcome::Cancelled, |opt| {
                                                RequestPermissionOutcome::Selected(
                                                    SelectedPermissionOutcome::new(
                                                        opt.option_id.clone(),
                                                    ),
                                                )
                                            });
                                        responder
                                            .respond(RequestPermissionResponse::new(outcome))?;
                                        Ok(())
                                    })
                                    .await
                                    .if_request(
                                        async move |req: CreateElicitationRequest, responder| {
                                            forms.lock().unwrap().push(serde_json::to_value(&req)?);
                                            responder.respond(accept_with("a"))?;
                                            Ok(())
                                        },
                                    )
                                    .await
                                    .otherwise_ignore()?;
                            }
                            SessionMessage::StopReason(_) => break,
                            _ => break,
                        }
                    }
                    Ok(())
                }
            }
        })
        .await;

    result.expect("client connection should succeed");
    let _ = agent_task.await;
    assert!(
        forms.lock().unwrap().is_empty(),
        "no form may be pushed when elicitation was not advertised"
    );
}
