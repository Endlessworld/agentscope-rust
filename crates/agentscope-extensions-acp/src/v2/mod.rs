//! ACP protocol v2 (draft) handler graph.
//!
//! v2 is served next to v1 from the same [`AcpCodingAgent`]: the SDK's protocol router picks
//! this graph when a client initializes with `protocolVersion: 2` and keeps the v1 graph for
//! everything else. The turn machinery itself is shared -- this module only translates the v2
//! wire (prompt acknowledgement plus `state_update` lifecycle, no modes, no `session/load`) onto
//! the session API the v1 handlers drive.
//!
//! Differences from v1 that shape this file:
//! * `session/prompt` answers with the user message id immediately and reports progress through
//!   `state_update` (`running` / `idle` with `stopReason` + `usage`).
//! * A prompt sent while work is running must be held, not rejected. The session serializes
//!   turns internally (`prompt_lock`), so accepting the prompt and answering is enough.
//! * Modes are gone: `mode` is a session config option here.
//! * `session/load` is gone; `session/resume` only acknowledges and, on request, replays.
//! * `session/fork` is not implemented (no fork primitive in the session store) and is answered
//!   with an error instead of being advertised.

mod bus;
mod convert;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::v1::schema as v1;
use agent_client_protocol::v2::schema as v2;
use agent_client_protocol::v2::{ConnectionTo, Responder};
use agent_client_protocol::{Agent, Client, DynConnectTo};

use super::coding_agent::{release_catalog_if_unused, AcpCodingAgent, AGENT_NAME, AGENT_VERSION};
use super::session::{AcpAgentSession, AcpClientHandle};

/// Builds the v2 handler chain for `agent`.
pub(crate) fn handlers(agent: Arc<AcpCodingAgent>) -> DynConnectTo<Client> {
    let turns = Arc::new(AtomicU64::new(0));

    let a_init = Arc::clone(&agent);
    let a_new = Arc::clone(&agent);
    let a_prompt = Arc::clone(&agent);
    let a_cancel = Arc::clone(&agent);
    let a_cancel_tool = Arc::clone(&agent);
    let a_cancel_terminal = Arc::clone(&agent);
    let a_close = Arc::clone(&agent);
    let a_list = Arc::clone(&agent);
    let a_status = Arc::clone(&agent);
    let a_title = Arc::clone(&agent);
    let a_config = Arc::clone(&agent);
    let a_options = Arc::clone(&agent);
    let a_archive = Arc::clone(&agent);
    let a_unarchive = Arc::clone(&agent);
    let a_delete = Arc::clone(&agent);
    let a_resume = Arc::clone(&agent);
    let a_fork = Arc::clone(&agent);
    let a_providers = Arc::clone(&agent);
    let a_providers_set = Arc::clone(&agent);
    let a_providers_disable = Arc::clone(&agent);
    let a_auth = Arc::clone(&agent);
    // `_acp_extends/*` handlers are shared with the v1 graph; each registration only needs its
    // own clone of the agent.
    let a_respond = Arc::clone(&agent);
    let a_steer = Arc::clone(&agent);
    let a_queue = Arc::clone(&agent);
    let a_queue_list = Arc::clone(&agent);
    let a_queue_remove = Arc::clone(&agent);
    let a_pause = Arc::clone(&agent);
    let a_resume_turn = Arc::clone(&agent);
    let a_providers_ext = Arc::clone(&agent);

    DynConnectTo::new(
        Agent
            .v2()
            .on_receive_request(
                async move |req: v2::InitializeRequest,
                            responder: Responder<v2::InitializeResponse>,
                            _cx: ConnectionTo<Client>| {
                    // Record what the client advertised: the shared session machinery gates
                    // notices / compaction / plan operations on the v1 capability view.
                    a_init
                        .client_support
                        .set_capabilities(convert::client_capabilities_to_v1(&req.capabilities));
                    a_init.client_support.set_client_info(Some(v1::Implementation::new(
                        req.info.name.clone(),
                        req.info.version.clone(),
                    )));
                    let mut caps = convert::capabilities_from_v1(&a_init.capabilities());
                    if a_init.manage_providers || !a_init.providers.is_empty() {
                        caps = caps.providers(Some(v2::ProvidersCapabilities::new()));
                    }
                    // Closed-schema clients (the TS SDK parses with zod and silently strips keys
                    // its types don't know) lose the unstable flags below. Mirror them into
                    // `_meta` so those clients can still gate their UI, same trick as `mcp.acp`.
                    let top = [
                        ("providers", caps.providers.is_some()),
                        (
                            "authStatus",
                            caps.auth
                                .as_ref()
                                .map(|auth| auth.status.is_some())
                                .unwrap_or(false),
                        ),
                    ]
                    .into_iter()
                    .filter(|(_, on)| *on)
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>();
                    if !top.is_empty() {
                        // 合并而不是覆盖：`capabilities_from_v1` 已经把 `io.animus/features` 带过来了
                        let mut meta = caps.meta.clone().unwrap_or_default();
                        meta.insert(
                            "io.animus/caps".to_string(),
                            serde_json::Value::from(top),
                        );
                        caps.meta = Some(meta);
                    }
                    if let Some(session) = caps.session.as_mut() {
                        let flags = [
                            ("delete", session.delete.is_some()),
                            ("additionalDirectories", session.additional_directories.is_some()),
                            ("systemPrompt", session.system_prompt.is_some()),
                            ("fork", session.fork.is_some()),
                            ("archive", session.archive.is_some()),
                            ("status", session.status.is_some()),
                            ("title", session.title.is_some()),
                            ("options", session.options.is_some()),
                        ]
                        .into_iter()
                        .filter(|(_, on)| *on)
                        .map(|(name, _)| name)
                        .collect::<Vec<_>>();
                        if !flags.is_empty() {
                            let mut meta = session.meta.clone().unwrap_or_default();
                            meta.insert(
                                "io.animus/sessionCaps".to_string(),
                                serde_json::Value::from(flags),
                            );
                            session.meta = Some(meta);
                        }
                    }
                    let auth_methods = vec![v2::AuthMethod::Terminal(
                        v2::AuthMethodTerminal::new("setup", "Configure a model provider")
                            .description(
                                "Runs `acp_harness_agent --setup` in a terminal: pick a model \
                                 provider and paste its API key.",
                            )
                            .args(vec!["--setup".to_string()]),
                    )];
                    responder.respond(
                        v2::InitializeResponse::new(
                            req.protocol_version,
                            v2::Implementation::new(AGENT_NAME, AGENT_VERSION),
                        )
                        .capabilities(caps)
                        .auth_methods(auth_methods),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let declaration = super::roots::RootsDeclaration::from_meta(req.meta.as_ref());
                    let requested: Vec<std::path::PathBuf> = req
                        .additional_directories
                        .iter()
                        .map(|path| path.0.clone())
                        .collect();
                    let (additional_directories, client_roots) = declaration.split(&requested);
                    let system_prompt = match a_new
                        .client_system_prompt(convert::system_prompt_to_v1(req.system_prompt))
                    {
                        Ok(prompt) => prompt,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let mcp_servers = match convert::mcp_servers_to_v1(req.mcp_servers) {
                        Some(servers) => servers,
                        None => {
                            return responder.respond_with_error(convert::invalid_params(
                                "unsupported MCP server transport",
                            ))
                        }
                    };
                    let session_id = v1::SessionId::new(format!("session-{}", uuid::Uuid::new_v4()));
                    let mut session = a_new.create_session(
                        session_id.clone(),
                        req.cwd.0.clone(),
                        system_prompt,
                        additional_directories.clone(),
                        client_roots,
                    );
                    session.set_additional_directories(additional_directories);
                    session.set_client(AcpClientHandle::new_v2(cx.clone()));
                    session.set_subagent_sink(subagent_sink(session_id.clone()));
                    attach_terminal_events(&session, &session_id);
                    if !mcp_servers.is_empty() {
                        let bridging = session.clone();
                        cx.spawn(async move {
                            bridging.apply_mcp_servers(&mcp_servers).await;
                            Ok(())
                        })?;
                    }
                    let config_options = convert::config_options_from_v1(session.config_options());
                    a_new.register_session(session);
                    // The model list is out; drop the catalog table unless a session still needs it.
                    release_catalog_if_unused(&a_new.sessions);
                    let v2_session_id = v2::SessionId::new(session_id.to_string());
                    bus::attach(v2_session_id.clone(), cx.clone());
                    responder.respond(
                        v2::NewSessionResponse::new(v2_session_id.clone())
                            .config_options(config_options)
                            .available_commands(commands()),
                    )?;
                    // Ready state: a client may queue a prompt right after the response, so the
                    // idle snapshot has to be in flight before that prompt's running snapshot.
                    send_update(
                        &cx,
                        &v2_session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                            v2::IdleStateUpdate::new(),
                        )),
                    )?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    let Some(mut session) = a_prompt.get_session(&session_id) else {
                        return responder
                            .respond_with_error(convert::session_not_found(&req.session_id));
                    };
                    let v2_blocks = req.prompt.clone();
                    let prompt: Option<Vec<v1::ContentBlock>> = v2_blocks
                        .iter()
                        .cloned()
                        .map(convert::content_to_v1)
                        .collect();
                    let Some(prompt) = prompt else {
                        return responder.respond_with_error(convert::invalid_params(
                            "unsupported prompt content block",
                        ));
                    };
                    if prompt.is_empty() {
                        return responder.respond_with_error(convert::invalid_params(
                            "prompt must not be empty",
                        ));
                    }
                    let turn = turns.fetch_add(1, Ordering::SeqCst) + 1;
                    let ids = Arc::new(convert::TurnIds::new(turn));
                    let v2_session_id = req.session_id.clone();
                    let user_message =
                        v2::UserMessage::new(ids.user()).content(Some(v2_blocks));
                    let message_id = ids.user();

                    // Bind the live connection: permission prompts and `ask_user` reach the
                    // client through it while the turn runs.
                    session.set_client(AcpClientHandle::new_v2(cx.clone()));
                    let registrar = Arc::clone(&a_prompt);
                    // v2 acknowledges the prompt with the user message id and streams the turn
                    // through `session/update`; the response must not wait for the turn.
                    responder.respond(v2::PromptResponse::new(message_id))?;
                    // The turn has to outlive the connection that sent the prompt. `cx.spawn`
                    // ties the task to the client connection, so a page reload dropped this
                    // future: the durable run kept writing the journal (the turn finished), but
                    // `drive_turn` stopped consuming events, so nothing was rendered and the
                    // reloaded connection never saw the rest of the stream.
                    tokio::spawn(async move {
                        // Everything this turn emits goes through the session bus: the
                        // connection that sent the prompt may already be gone (client reload),
                        // and whoever attaches next has to see the rest of the stream.
                        bus::publish((
                            v2_session_id.clone(),
                            None,
                            v2::SessionUpdate::UserMessage(user_message),
                        ));
                        bus::publish((
                            v2_session_id.clone(),
                            None,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                                v2::RunningStateUpdate::new(),
                            )),
                        ));
                        // MCP-over-ACP declarations arrive with `session/new`; the bridge needs
                        // the client connection, which exists only now.
                        session.apply_pending_acp_servers().await;
                        let sink_session = v2_session_id.clone();
                        let sink_ids = Arc::clone(&ids);
                        let outcome = futures::FutureExt::catch_unwind(
                            std::panic::AssertUnwindSafe(session.run_prompt_cancellable(
                                prompt,
                                move |notification| {
                                    // v1 的 sink 给每条 update 打了 `_meta.turnId`（session.rs 的
                                    // drive_turn）；v2 客户端靠它把 update 归到所属 Turn，漏掉就会出现
                                    // 「排队 Turn 的回答叠到上一个气泡」「队列永远不提升」。
                                    let meta = notification.meta;
                                    if let Some(update) =
                                        convert::update_from_v1(notification.update, &sink_ids)
                                    {
                                        bus::publish((sink_session.clone(), meta, update));
                                    }
                                },
                                None,
                            )),
                        )
                        .await;
                        let usage = convert::usage_from_v1(session.turn_usage_response());
                        // Read before the session moves back into the registry.
                        let paused = session.pause_pending();
                        registrar.register_session(session);
                        let stop_reason = match outcome {
                            Ok(reason) => convert::stop_reason_from_v1(reason),
                            Err(panic) => {
                                let detail = panic
                                    .downcast_ref::<&str>()
                                    .map(|message| message.to_string())
                                    .or_else(|| panic.downcast_ref::<String>().cloned())
                                    .unwrap_or_else(|| "unknown panic".to_string());
                                tracing::error!(%detail, "prompt panicked; reporting to the client");
                                bus::publish((
                                    v2_session_id.clone(),
                                    None,
                                    v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                                        v2::ContentBlock::Text(v2::TextContent::new(format!(
                                            "Agent error: {detail}"
                                        ))),
                                        ids.user(),
                                    )),
                                ));
                                v2::StopReason::Refusal
                            }
                        };
                        let paused_stop = paused && matches!(&stop_reason, v2::StopReason::Cancelled);
                        let mut idle = v2::IdleStateUpdate::new().stop_reason(stop_reason);
                        if let Some(usage) = usage {
                            idle = idle.usage(usage);
                        }
                        // v2 has no `paused` stop reason either: a paused turn stopped its
                        // foreground work, so it reports `cancelled` in an idle snapshot. The
                        // `_meta.paused` marker is what separates "resumable pause" from a real
                        // cancellation; a turn that finished before the request reached it
                        // reports its real stop reason and carries no marker.
                        if paused_stop {
                            let mut meta = v2::Meta::new();
                            meta.insert("paused".to_string(), serde_json::json!(true));
                            idle = idle.meta(meta);
                        }
                        // The final idle carries the stop reason: it has to reach the
                        // connection attached *now*, or a reload leaves the UI on `running`.
                        bus::publish((
                            v2_session_id.clone(),
                            None,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)),
                        ));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: v2::CancelSessionNotification,
                            _cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&notification.session_id);
                    if let Some(session) = a_cancel.get_session(&session_id) {
                        session.cancel();
                        // v2 has no per-prompt hold in this build: cancelling the turn is also
                        // what drops the prompts waiting behind it.
                        if let Err(error) = session.cancel_queued().await {
                            tracing::warn!(%error, "cancel: could not clear the queued turns");
                        }
                    } else if let Some(parent) = a_cancel.subagent_parent(&session_id) {
                        // A child session is not a registered session: the client cancels it by
                        // naming the child, and the parent owns the agent that must be stopped.
                        if let Some(session) = a_cancel.get_session(&parent) {
                            session.cancel_subagent(&session_id);
                        }
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    if let Some(session) = a_close.get_session(&session_id) {
                        // Mark it closed first so a finishing turn cannot re-insert it, then let
                        // the cancelled run release the journal before the registry forgets it.
                        session.close();
                        let _ = session.cancel_and_wait(Duration::from_secs(15)).await;
                    }
                    a_close.remove_session(&session_id);
                    bus::forget(&session_id.to_string());
                    responder.respond(v2::CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            _cx: ConnectionTo<Client>| {
                    let filter = match req.archived {
                        None => v1::SessionArchiveFilter::default(),
                        Some(v2::SessionArchiveFilter::Exclude) => v1::SessionArchiveFilter::Exclude,
                        Some(v2::SessionArchiveFilter::Only) => v1::SessionArchiveFilter::Only,
                        Some(_) => {
                            return responder.respond_with_error(convert::invalid_params(
                                "unsupported session archive filter",
                            ))
                        }
                    };
                    let cwd = req.cwd.as_ref().map(|path| path.0.clone());
                    let page = a_list
                        .session_infos(
                            cwd.as_deref(),
                            req.cursor.as_ref().map(|cursor| cursor.as_ref()),
                            filter,
                        )
                        .await;
                    let mut response = v2::ListSessionsResponse::new(
                        convert::session_infos_from_v1(page.sessions),
                    );
                    if let Some(cursor) = page.next_cursor {
                        response = response.next_cursor(Some(v2::SessionListCursor::new(
                            cursor.to_string(),
                        )));
                    }
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::SessionStatusRequest,
                            responder: Responder<v2::SessionStatusResponse>,
                            _cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    match a_status.session_status(&session_id).await {
                        Some(status) => responder.respond(v2::SessionStatusResponse::new(
                            convert::session_status_from_v1(status),
                        )),
                        None => {
                            responder.respond_with_error(convert::session_not_found(&req.session_id))
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::SetSessionTitleRequest,
                            responder: Responder<v2::SetSessionTitleResponse>,
                            cx: ConnectionTo<Client>| {
                    let title = req.title.trim().to_string();
                    if title.is_empty() {
                        return responder.respond_with_error(convert::invalid_params(
                            "session title must not be empty",
                        ));
                    }
                    let session_id = v1_session_id(&req.session_id);
                    let Some(mut session) = a_title.get_session(&session_id) else {
                        return responder
                            .respond_with_error(convert::session_not_found(&req.session_id));
                    };
                    session.set_title(title.clone());
                    if let Some(notification) =
                        convert::notification_from_v1(session.session_info_update())
                    {
                        let _ = cx.send_notification(notification);
                    }
                    a_title.register_session(session);
                    responder.respond(v2::SetSessionTitleResponse::new(title))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::SetSessionConfigOptionRequest,
                            responder: Responder<v2::SetSessionConfigOptionResponse>,
                            cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    let Some(mut session) = a_config.get_session(&session_id) else {
                        return responder
                            .respond_with_error(convert::session_not_found(&req.session_id));
                    };
                    session.set_client(AcpClientHandle::new_v2(cx.clone()));
                    let Some(value) = config_value(&req.value) else {
                        return responder.respond_with_error(convert::invalid_params(
                            "config option requires a value id",
                        ));
                    };
                    if let Err(error) = apply_config_option(&mut session, &req.config_id.to_string(), &value)
                    {
                        return responder.respond_with_error(error);
                    }
                    let options = session.config_options();
                    let v2_session_id = req.session_id.clone();
                    let _ = cx.send_notification(v2::UpdateSessionNotification::new(
                        v2_session_id,
                        v2::SessionUpdate::ConfigOptionUpdate(v2::ConfigOptionUpdate::new(
                            convert::config_options_from_v1(options.clone()),
                        )),
                    ));
                    let response_options = convert::config_options_from_v1(options);
                    a_config.register_session(session);
                    release_catalog_if_unused(&a_config.sessions);
                    responder.respond(v2::SetSessionConfigOptionResponse::new(response_options))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::SessionOptionsRequest,
                            responder: Responder<v2::SessionOptionsResponse>,
                            _cx: ConnectionTo<Client>| {
                    // A snapshot, never a side effect: `session/options` must not create or
                    // resume a session. With an id it answers what that session currently
                    // offers; without one it answers what a new session in `cwd` would start
                    // with — which is why the session-less answer is built from the agent's
                    // advertised options rather than by building a throwaway session.
                    let options = match &req.session_id {
                        Some(v2_session_id) => {
                            let session_id = v1_session_id(v2_session_id);
                            let Some(session) = a_options.get_session(&session_id) else {
                                return responder.respond_with_error(convert::session_not_found(
                                    v2_session_id,
                                ));
                            };
                            session.config_options()
                        }
                        None => {
                            let models = a_options.models_snapshot();
                            let modes = a_options.modes_snapshot();
                            let model = models
                                .first()
                                .map(|choice| choice.id.clone())
                                .unwrap_or_default();
                            super::config::default_config_options_with_thought_grouped(
                                &model,
                                &models,
                                super::config::default_mode(),
                                &modes,
                                super::config::DEFAULT_THOUGHT_LEVEL,
                                true,
                            )
                        }
                    };
                    responder.respond(
                        v2::SessionOptionsResponse::new()
                            .config_options(convert::config_options_from_v1(options))
                            .available_commands(commands()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::ArchiveSessionRequest,
                            responder: Responder<v2::ArchiveSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    match a_archive.archive_session(&session_id, true).await {
                        Ok(()) => responder.respond(v2::ArchiveSessionResponse::new()),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::UnarchiveSessionRequest,
                            responder: Responder<v2::UnarchiveSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    match a_unarchive.archive_session(&session_id, false).await {
                        Ok(()) => responder.respond(v2::UnarchiveSessionResponse::new()),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::DeleteSessionRequest,
                            responder: Responder<v2::DeleteSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    let context = agentscope_core::agent::RuntimeContext::new()
                        .with_session_id(session_id.to_string());
                    if let Some(session) = a_delete.get_session(&session_id) {
                        let _ = session.cancel_and_wait(Duration::from_secs(10)).await;
                        session.close();
                        a_delete
                            .forget_session(session.agent(), session.cwd(), &session_id, &context)
                            .await;
                    } else if let Some(cwd) = a_delete.session_workspace(&session_id).await {
                        let agent = a_delete.catalog_agent(&cwd);
                        a_delete
                            .forget_session(&agent, &cwd, &session_id, &context)
                            .await;
                    }
                    a_delete.remove_session(&session_id);
                    bus::forget(&session_id.to_string());
                    responder.respond(v2::DeleteSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let session_id = v1_session_id(&req.session_id);
                    let requested: Vec<std::path::PathBuf> = req
                        .additional_directories
                        .iter()
                        .map(|path| path.0.clone())
                        .collect();
                    let (additional_directories, client_roots) =
                        super::roots::RootsDeclaration::split_by_existence(&requested);
                    let system_prompt = match a_resume
                        .client_system_prompt(convert::system_prompt_to_v1(req.system_prompt))
                    {
                        Ok(prompt) => prompt,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let mcp_servers = match convert::mcp_servers_to_v1(req.mcp_servers) {
                        Some(servers) => servers,
                        None => {
                            return responder.respond_with_error(convert::invalid_params(
                                "unsupported MCP server transport",
                            ))
                        }
                    };
                    let existing = a_resume.get_session(&session_id);
                    if let Some(session) = &existing {
                        if system_prompt.is_some() {
                            tracing::warn!(
                                session = %session_id,
                                "systemPrompt ignored: the session is already live"
                            );
                        }
                        if session.is_running() {
                            return responder.respond_with_error(convert::invalid_params(
                                "session still has foreground work",
                            ));
                        }
                    }
                    let session = match existing {
                        Some(session) => session,
                        None => {
                            if !a_resume.session_exists(&session_id, req.cwd.0.clone()).await {
                                return responder
                                    .respond_with_error(convert::session_not_found(&req.session_id));
                            }
                            let session = a_resume.create_session(
                                session_id.clone(),
                                req.cwd.0.clone(),
                                system_prompt,
                                additional_directories.clone(),
                                client_roots,
                            );
                            a_resume.register_session(session);
                            a_resume
                                .get_session(&session_id)
                                .expect("the session was just registered")
                        }
                    };
                    let mut session = session;
                    session.set_additional_directories(additional_directories);
                    session.set_client(AcpClientHandle::new_v2(cx.clone()));
                    session.set_subagent_sink(subagent_sink(session_id.clone()));
                    attach_terminal_events(&session, &session_id);
                    if !mcp_servers.is_empty() {
                        let bridging = session.clone();
                        cx.spawn(async move {
                            bridging.apply_mcp_servers(&mcp_servers).await;
                            Ok(())
                        })?;
                    }
                    let options = convert::config_options_from_v1(session.config_options());
                    let v2_session_id = req.session_id.clone();
                    // Attach before the replay: a turn still running in another agent instance
                    // (the client reloaded mid-turn) resumes streaming to this connection.
                    bus::attach(v2_session_id.clone(), cx.clone());
                    // Replay the transcript only when the client asked for it; the v1
                    // `session/load` counterpart always replays.
                    if matches!(req.replay_from, Some(v2::ReplayFrom::Start(_))) {
                        let replay = cx.clone();
                        let replay_session = v2_session_id.clone();
                        session
                            .replay_notifications(move |notification| {
                                let meta = notification.meta;
                                if let Some(update) =
                                    convert::update_from_v1(notification.update, &replay_ids())
                                {
                                    let mut out = v2::UpdateSessionNotification::new(
                                        replay_session.clone(),
                                        update,
                                    );
                                    if let Some(meta) = meta {
                                        out = out.meta(meta);
                                    }
                                    let _ = replay.send_notification(out);
                                }
                            })
                            .await;
                    }
                    a_resume.register_session(session);
                    release_catalog_if_unused(&a_resume.sessions);
                    responder.respond(
                        v2::ResumeSessionResponse::new()
                            .config_options(options)
                            .available_commands(commands()),
                    )?;
                    send_update(
                        &cx,
                        &v2_session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                            v2::IdleStateUpdate::new(),
                        )),
                    )?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::ForkSessionRequest,
                            responder: Responder<v2::ForkSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    // No fork primitive in the session store (ACP-D13). The capability is not
                    // advertised either, so a conforming client never calls this.
                    let _ = (&a_fork, &req);
                    responder.respond_with_error(convert::invalid_params(
                        "session/fork is not supported by this agent",
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: v2::ListProvidersRequest,
                            responder: Responder<v2::ListProvidersResponse>,
                            _cx: ConnectionTo<Client>| {
                    let providers = if a_providers.manage_providers {
                        tokio::task::spawn_blocking(crate::providers_admin::list)
                            .await
                            .unwrap_or_default()
                    } else {
                        a_providers.providers.list()
                    };
                    let providers = providers
                        .into_iter()
                        .filter_map(convert::provider_from_v1)
                        .collect();
                    responder.respond(v2::ListProvidersResponse::new(providers))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::SetProviderRequest,
                            responder: Responder<v2::SetProviderResponse>,
                            _cx: ConnectionTo<Client>| {
                    let id = req.provider_id.to_string();
                    let headers: std::collections::HashMap<String, String> =
                        req.headers.into_iter().collect();
                    if a_providers_set.manage_providers {
                        // `_meta` carries everything the standard v1 shape lacks: the model
                        // table (`models`), the enabled/disabled flag and the display label.
                        // Dropping it here made `providers/set` a no-op for those updates.
                        let request = v1::SetProviderRequest::new(
                            req.provider_id.to_string(),
                            convert::protocol_to_v1(req.api_type),
                            req.base_url.clone(),
                        )
                        .headers(headers.into_iter().collect())
                        .meta(req.meta);
                        let outcome = tokio::task::spawn_blocking(move || {
                            crate::providers_admin::set(request)
                                .map(|()| crate::providers_admin::choices())
                        })
                        .await;
                        return match outcome {
                            Ok(Ok(models)) => {
                                a_providers_set.replace_models(models);
                                responder.respond(v2::SetProviderResponse::new())
                            }
                            Ok(Err(error)) => responder.respond_with_error(
                                convert::invalid_params(format!("provider '{id}': {error}")),
                            ),
                            Err(error) => responder.respond_with_error(convert::invalid_params(
                                format!("provider '{id}': {error}"),
                            )),
                        };
                    }
                    let ok = a_providers_set.providers.set(
                        &id,
                        convert::protocol_to_v1(req.api_type),
                        req.base_url,
                        headers,
                    );
                    if ok {
                        responder.respond(v2::SetProviderResponse::new())
                    } else {
                        responder.respond_with_error(convert::invalid_params(format!(
                            "unknown provider '{id}'"
                        )))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::DisableProviderRequest,
                            responder: Responder<v2::DisableProviderResponse>,
                            _cx: ConnectionTo<Client>| {
                    let id = req.provider_id.to_string();
                    if a_providers_disable.manage_providers {
                        let outcome = tokio::task::spawn_blocking({
                            let id = id.clone();
                            move || {
                                crate::providers_admin::disable(&id)
                                    .map(|()| crate::providers_admin::choices())
                            }
                        })
                        .await;
                        return match outcome {
                            Ok(Ok(models)) => {
                                a_providers_disable.replace_models(models);
                                responder.respond(v2::DisableProviderResponse::new())
                            }
                            Ok(Err(error)) => responder.respond_with_error(
                                convert::invalid_params(format!("provider '{id}': {error}")),
                            ),
                            Err(error) => responder.respond_with_error(convert::invalid_params(
                                format!("provider '{id}': {error}"),
                            )),
                        };
                    }
                    if a_providers_disable.providers.disable(&id) {
                        responder.respond(v2::DisableProviderResponse::new())
                    } else {
                        responder.respond_with_error(convert::invalid_params(format!(
                            "unknown provider '{id}'"
                        )))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: v2::LoginAuthRequest,
                            responder: Responder<v2::LoginAuthResponse>,
                            _cx: ConnectionTo<Client>| {
                    // The only advertised method is `terminal`, which the client runs itself;
                    // there is nothing left for the agent to do here.
                    responder.respond(v2::LoginAuthResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: v2::LogoutAuthRequest,
                            responder: Responder<v2::LogoutAuthResponse>,
                            _cx: ConnectionTo<Client>| {
                    // Credentials live in `models.json`, which only `providers/disable` and the
                    // setup flow change; logout has nothing of its own to revoke here.
                    responder.respond(v2::LogoutAuthResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: v2::AuthStatusRequest,
                            responder: Responder<v2::AuthStatusResponse>,
                            _cx: ConnectionTo<Client>| {
                    responder.respond(v2::AuthStatusResponse::new(convert::auth_state_from_v1(
                        a_auth.auth_state(),
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::CancelToolCallRequest,
                            responder: Responder<v2::CancelToolCallResponse>,
                            _cx: ConnectionTo<Client>| {
                    // Only `execute` calls advertise `capabilities.cancel`: the sandbox
                    // registers their child process in the session registry, so this kills the
                    // process tree and the turn reports `cancelled` instead of `failed`.
                    // Anything else is unknown, final or not cancellable: the `-32602` the
                    // spec prescribes.
                    let session_id = v1_session_id(&req.session_id);
                    let cancelled = a_cancel_tool
                        .get_session(&session_id)
                        .map(|session| session.processes().cancel(req.tool_call_id.0.as_ref()))
                        .unwrap_or(false);
                    if cancelled {
                        responder.respond(v2::CancelToolCallResponse::new())?;
                    } else {
                        responder.respond_with_error(convert::invalid_params(
                            "this tool call is not cancellable",
                        ))?;
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::CancelTerminalRequest,
                            responder: Responder<v2::CancelTerminalResponse>,
                            _cx: ConnectionTo<Client>| {
                    // Agent-owned terminals are the `execute` tool calls: the terminal id is
                    // the tool call id, so cancelling one kills the same registered process.
                    let session_id = v1_session_id(&req.session_id);
                    let cancelled = a_cancel_terminal
                        .get_session(&session_id)
                        .map(|session| session.processes().cancel(req.terminal_id.0.as_ref()))
                        .unwrap_or(false);
                    if cancelled {
                        responder.respond(v2::CancelTerminalResponse::new())?;
                    } else {
                        responder.respond_with_error(convert::invalid_params(
                            "this terminal is not cancellable",
                        ))?;
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            // Private extensions (`_acp_extends/*`) reused verbatim from the v1 graph: their
            // request/response types are protocol-agnostic and the handlers never touch the
            // wire, so only `resume` needs a v2-specific notification sink.
            .on_receive_request(
                async move |req: crate::ext::ExtRespondRequest, responder, _cx| {
                    super::coding_agent::handle_ext_respond(a_respond.clone(), req, responder).await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtSteerRequest, responder, _cx| {
                    super::coding_agent::handle_ext_steer(a_steer.clone(), req, responder).await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtQueueRequest, responder, _cx| {
                    super::coding_agent::handle_ext_queue(a_queue.clone(), req, responder).await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtQueueListRequest, responder, _cx| {
                    super::coding_agent::handle_ext_queue_list(a_queue_list.clone(), req, responder)
                        .await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtQueueRemoveRequest, responder, _cx| {
                    super::coding_agent::handle_ext_queue_remove(
                        a_queue_remove.clone(),
                        req,
                        responder,
                    )
                    .await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtPauseRequest, responder, _cx| {
                    super::coding_agent::handle_ext_pause(a_pause.clone(), req, responder).await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtResumeRequest,
                            responder: Responder<crate::ext::ExtResumeResult>,
                            _cx: ConnectionTo<Client>| {
                    // The handler chain is `FnMut` (one closure per request), so the shared
                    // agent handle must be cloned per invocation rather than moved out.
                    let agent = Arc::clone(&a_resume_turn);
                    let route = v2::SessionId::new(req.params.session_id.clone());
                    // The resumed turn outlives the connection that asked for it (a reload must
                    // keep seeing the stream), so it runs on a detached task and emits through
                    // the session bus instead of the socket that sent the request.
                    tokio::spawn(async move {
                        let ids = Arc::new(replay_ids());
                        let emit = move |notification: v1::SessionNotification| {
                            let meta = notification.meta;
                            if let Some(update) =
                                convert::update_from_v1(notification.update, &ids)
                            {
                                bus::publish_routed(&route.0, (route.clone(), meta, update));
                            }
                        };
                        match super::coding_agent::resume_turn_core(&agent, &req.params, emit)
                            .await
                        {
                            Ok(result) => responder.respond(result),
                            Err(error) => responder.respond_with_error(error),
                        }
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtFsListRequest, responder, _cx| {
                    super::coding_agent::handle_ext_fs_list(req, responder).await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::providers_admin::ProvidersExtRequest, responder, _cx| {
                    super::coding_agent::handle_providers_ext(
                        a_providers_ext.clone(),
                        req,
                        responder,
                    )
                    .await
                },
                agent_client_protocol::on_receive_request!(),
            ),
    )
}

/// Converts a v2 session id into the v1 id the session registry is keyed by.
fn v1_session_id(session_id: &v2::SessionId) -> v1::SessionId {
    v1::SessionId::new(session_id.0.to_string())
}

fn replay_ids() -> convert::TurnIds {
    convert::TurnIds::new(0)
}

/// Routes subagent frames onto the process-wide session bus instead of the connection that
/// created the session, so a reloaded client keeps seeing subagent cards.
///
/// `route` is the *parent* session (the one a client attaches to); every frame keeps naming the
/// child session it belongs to, which is what the client files the card under.
/// Routes this session's command-process events onto the session bus as v2 terminal updates.
///
/// v2 exposes agent-owned terminals: a running `execute` tool call becomes a terminal that
/// advertises `capabilities.cancel`, its output streams as `terminal_output_chunk`, and its exit
/// clears the capability. The terminal id is the tool call id, so `terminal/cancel` and
/// `tool_call/cancel` kill the same registered process. v1 never calls this, so its behaviour is
/// unchanged.
fn attach_terminal_events(session: &AcpAgentSession, route: &v1::SessionId) {
    session
        .processes()
        .set_events(Some(Arc::new(TerminalEvents {
            route: v2::SessionId::new(route.to_string()),
        })));
}

struct TerminalEvents {
    route: v2::SessionId,
}

impl TerminalEvents {
    fn publish(&self, update: v2::TerminalUpdate) {
        bus::publish_routed(
            &self.route.0,
            (
                self.route.clone(),
                None,
                v2::SessionUpdate::TerminalUpdate(update),
            ),
        );
    }
}

impl agentscope_core::tool::ProcessEvents for TerminalEvents {
    fn started(&self, key: &str, command: &str, cwd: &str) {
        self.publish(
            v2::TerminalUpdate::new(v2::TerminalId::new(key))
                .command(command)
                .cwd(v2::AbsolutePath::new(std::path::PathBuf::from(cwd)))
                .capabilities(
                    v2::TerminalCapabilities::new().cancel(v2::TerminalCancelCapabilities::new()),
                ),
        );
    }

    fn output(&self, key: &str, bytes: &[u8]) {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        bus::publish_routed(
            &self.route.0,
            (
                self.route.clone(),
                None,
                v2::SessionUpdate::TerminalOutputChunk(v2::TerminalOutputChunk::new(
                    v2::TerminalId::new(key),
                    data,
                )),
            ),
        );
    }

    fn exited(&self, key: &str, exit_code: i32) {
        self.publish(
            v2::TerminalUpdate::new(v2::TerminalId::new(key))
                .exit_status(v2::TerminalExitStatus::new().exit_code(i64::from(exit_code)))
                // The process is gone: stop advertising the cancel capability.
                .capabilities(None::<v2::TerminalCapabilities>),
        );
    }
}

fn subagent_sink(route: v1::SessionId) -> Arc<dyn Fn(v1::SessionNotification) + Send + Sync> {
    let route = v2::SessionId::new(route.to_string());
    let ids = Arc::new(replay_ids());
    Arc::new(move |notification: v1::SessionNotification| {
        // The bus key is the session the client is attached to (the root), but
        // the frame itself must name the session the update belongs to: a
        // `subagent_update` belongs to the parent while the child's own content
        // belongs to the child. Reusing `route` for both made every child
        // message render in the parent transcript.
        let frame_session = v2::SessionId::new(notification.session_id.to_string());
        let meta = notification.meta;
        if let Some(update) = convert::update_from_v1(notification.update, &ids) {
            bus::publish_routed(&route.0, (frame_session, meta, update));
        }
    })
}

fn commands() -> Vec<v2::AvailableCommand> {
    convert::commands(vec![v1::AvailableCommand::new(
        super::session::COMPACT_COMMAND_NAME,
        "Compact the conversation context and keep a summary",
    )])
}

fn send_update(
    connection: &ConnectionTo<Client>,
    session_id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    connection.send_notification(v2::UpdateSessionNotification::new(
        session_id.clone(),
        update,
    ))
}

/// Applies one `session/set_config_option` value the way the v1 handler applies it.
fn apply_config_option(
    session: &mut AcpAgentSession,
    config_id: &str,
    value: &str,
) -> agent_client_protocol::Result<()> {
    match config_id {
        "model" => {
            if !session.has_model(value) {
                return Err(convert::invalid_params(format!("unknown model '{value}'")));
            }
            session.set_model(value.to_string());
            session.push_usage_for_selected_model();
        }
        "thought_level" => {
            if !session.has_thought_level(value) {
                return Err(convert::invalid_params(format!(
                    "thought level '{value}' is not supported by the selected model"
                )));
            }
            session.set_thought_level(value.to_string());
        }
        "mode" => session.set_mode(value.to_string()),
        other => {
            return Err(convert::invalid_params(format!(
                "unknown config option '{other}'"
            )))
        }
    }
    Ok(())
}

/// Reads the value id out of a v2 config value (only id-carrying values are supported: every
/// option this agent publishes is a select).
fn config_value(value: &v2::SessionConfigOptionValue) -> Option<String> {
    match value {
        v2::SessionConfigOptionValue::Id { value } => Some(value.to_string()),
        _ => None,
    }
}

/// Pushes one v1 notification over a v2 connection.
///
/// Used by the shared session machinery for updates that happen outside a prompt (config
/// changes, notices, usage). A v1 update without a v2 counterpart is dropped.
pub(crate) fn notify(
    connection: &ConnectionTo<Client>,
    notification: v1::SessionNotification,
) -> agent_client_protocol::Result<()> {
    match convert::notification_from_v1(notification) {
        Some(notification) => connection.send_notification(notification),
        None => Ok(()),
    }
}

/// Asks the client for permission over a v2 connection and maps the answer back to v1.
pub(crate) async fn request_permission(
    connection: &ConnectionTo<Client>,
    session_id: &v1::SessionId,
    tool_call: v1::ToolCallUpdate,
) -> agent_client_protocol::Result<v1::RequestPermissionResponse> {
    let request =
        convert::permission_request(session_id, tool_call, super::session::permission_options())
            .ok_or_else(|| {
                convert::invalid_params(
                    "the tool call cannot be rendered as a v2 permission request",
                )
            })?;
    let response: v2::RequestPermissionResponse =
        connection.send_request(request).block_task().await?;
    Ok(convert::permission_response_from_v2(response))
}

/// Sends an elicitation form over a v2 connection.
pub(crate) async fn elicit(
    connection: &ConnectionTo<Client>,
    request: v1::CreateElicitationRequest,
) -> Result<v1::CreateElicitationResponse, String> {
    let request = convert::elicitation_request(request)
        .ok_or_else(|| "the elicitation form cannot be represented in protocol v2".to_string())?;
    let response: v2::CreateElicitationResponse = connection
        .send_request(request)
        .block_task()
        .await
        .map_err(|error| error.to_string())?;
    convert::elicitation_response(response).ok_or_else(|| {
        "the client answered with an elicitation shape this build cannot read".to_string()
    })
}
