//! Conversions between the stable ACP v1 types and the v2 draft.
//!
//! The v2 handler graph reuses the v1 session machinery (`AcpAgentSession` and the shared
//! prompt pipeline), so every wire type crossing that boundary is translated here instead of
//! duplicating the turn logic. Where the two wire shapes are identical the conversion goes
//! through `serde_json`; where they differ the mapping is written out.

use std::sync::Mutex;

use agent_client_protocol::v1::schema as v1;
use agent_client_protocol::v2::schema as v2;

/// Message ids a v2 turn needs but the v1 renderer does not always carry.
///
/// v2 requires an id on every `*_chunk` update while the shared renderer only sets one when the
/// event had it. The user message id is fixed at admission time; the agent message and thought
/// ids are created on the first chunk of their kind and reused for the rest of the turn.
#[derive(Debug)]
pub(super) struct TurnIds {
    user: v2::MessageId,
    agent: Mutex<Option<v2::MessageId>>,
    thought: Mutex<Option<v2::MessageId>>,
    seq: Mutex<u64>,
}

impl TurnIds {
    pub(super) fn new(turn: u64) -> Self {
        Self {
            user: v2::MessageId::new(format!("user-{turn}")),
            agent: Mutex::new(None),
            thought: Mutex::new(None),
            seq: Mutex::new(0),
        }
    }

    pub(super) fn user(&self) -> v2::MessageId {
        self.user.clone()
    }

    fn sequence(&self) -> u64 {
        let mut seq = self
            .seq
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *seq += 1;
        *seq
    }

    fn user_id(&self, carried: Option<v1::MessageId>) -> v2::MessageId {
        match carried {
            Some(id) => v2::MessageId::new(id.to_string()),
            None => self.user.clone(),
        }
    }

    fn agent_id(&self, carried: Option<v1::MessageId>) -> v2::MessageId {
        if let Some(id) = carried {
            return v2::MessageId::new(id.to_string());
        }
        let mut slot = self
            .agent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(id) = slot.as_ref() {
            return id.clone();
        }
        let id = v2::MessageId::new(format!("agent-{}", self.sequence()));
        *slot = Some(id.clone());
        id
    }

    fn thought_id(&self, carried: Option<v1::MessageId>) -> v2::MessageId {
        if let Some(id) = carried {
            return v2::MessageId::new(id.to_string());
        }
        let mut slot = self
            .thought
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(id) = slot.as_ref() {
            return id.clone();
        }
        let id = v2::MessageId::new(format!("thought-{}", self.sequence()));
        *slot = Some(id.clone());
        id
    }
}

/// Translates one v2 prompt block into the v1 type the shared pipeline consumes.
///
/// The wire shapes of `text`/`image`/`audio`/`resource_link`/`resource` are identical, so the
/// conversion is a JSON round trip; a block this build does not understand is rejected rather
/// than silently dropped.
pub(super) fn content_to_v1(block: v2::ContentBlock) -> Option<v1::ContentBlock> {
    json_between(block)
}

fn content_from_v1(block: v1::ContentBlock) -> Option<v2::ContentBlock> {
    json_between(block)
}

/// v2 MCP servers have no SSE transport, so only `stdio`/`http`/`acp` reach this build.
pub(super) fn mcp_servers_to_v1(servers: Vec<v2::McpServer>) -> Option<Vec<v1::McpServer>> {
    servers.into_iter().map(json_between).collect()
}

pub(super) fn system_prompt_to_v1(prompt: Option<v2::SystemPrompt>) -> Option<v1::SystemPrompt> {
    json_between(prompt?)
}

/// Builds the v1 client-capability view the shared session machinery gates its optional
/// updates on.
///
/// v2 has no client capabilities for notices, compaction, plan operations or subagents: they
/// are baseline, so a v2 client supports all of them. Elicitation is advertised explicitly in
/// both versions.
pub(super) fn client_capabilities_to_v1(caps: &v2::ClientCapabilities) -> v1::ClientCapabilities {
    let mut value = serde_json::json!({
        "session": {"notices": {}, "compaction": {}},
        "plan": {},
        "subagents": {},
    });
    if caps.elicitation.is_some() {
        value["elicitation"] = serde_json::json!({"form": {}});
    }
    serde_json::from_value(value).unwrap_or_default()
}

/// Projects the advertised v1 capabilities onto the v2 shape.
///
/// v2 dropped `loadSession`, `session/set_mode` and the SSE MCP transport, moved list/close to
/// the baseline, and represents every capability as an empty object instead of a bool.
/// v2 MCP capabilities: the standard `acp` flag plus a `_meta` mirror.
///
/// The Rust schema (1.22) carries `acp` under the unstable `unstable_mcp_over_acp` feature, but
/// the TypeScript SDK 2.12 drops the field while deserializing an `initialize` response, so a JS
/// client never sees it even though we send it. `_meta` survives every client, so mirroring the
/// flag there keeps MCP-over-ACP usable without changing the standard advertisement.
fn mcp_capabilities(caps: &v1::McpCapabilities) -> v2::McpCapabilities {
    let mut mcp = v2::McpCapabilities::new()
        .stdio(Some(v2::McpStdioCapabilities::new()))
        .http(caps.http.then_some(v2::McpHttpCapabilities::new()))
        .acp(caps.acp.then_some(v2::McpAcpCapabilities::new()));
    if caps.acp {
        mcp.meta = Some(serde_json::Map::from_iter([(
            "io.animus/mcpAcp".to_string(),
            serde_json::Value::Bool(true),
        )]));
    }
    mcp
}

pub(super) fn capabilities_from_v1(caps: &v1::AgentCapabilities) -> v2::AgentCapabilities {
    let session = &caps.session_capabilities;
    let session = v2::SessionCapabilities::new()
        .prompt(
            v2::PromptCapabilities::new()
                .image(
                    caps.prompt_capabilities
                        .image
                        .then_some(v2::PromptImageCapabilities::new()),
                )
                .audio(
                    caps.prompt_capabilities
                        .audio
                        .then_some(v2::PromptAudioCapabilities::new()),
                )
                .embedded_context(
                    caps.prompt_capabilities
                        .embedded_context
                        .then_some(v2::PromptEmbeddedContextCapabilities::new()),
                ),
        )
        .mcp(mcp_capabilities(&caps.mcp_capabilities))
        .delete(
            session
                .delete
                .as_ref()
                .map(|_| v2::SessionDeleteCapabilities::new()),
        )
        .additional_directories(
            session
                .additional_directories
                .as_ref()
                .map(|_| v2::SessionAdditionalDirectoriesCapabilities::new()),
        )
        .system_prompt(
            session
                .system_prompt
                .as_ref()
                .map(|_| v2::SessionSystemPromptCapabilities::new()),
        )
        .fork(
            session
                .fork
                .as_ref()
                .map(|_| v2::SessionForkCapabilities::new()),
        )
        .archive(
            session
                .archive
                .as_ref()
                .map(|_| v2::SessionArchiveCapabilities::new()),
        )
        .status(
            session
                .status
                .as_ref()
                .map(|_| v2::SessionStatusCapabilities::new()),
        )
        .title(
            session
                .title
                .as_ref()
                .map(|_| v2::SessionTitleCapabilities::new()),
        )
        // v2-only: the graph implements `session/options`, so the capability is advertised.
        .options(Some(v2::SessionOptionsCapabilities::default()));
    let mut out = v2::AgentCapabilities::new()
        .session(session)
        .auth(Some(
            v2::AgentAuthCapabilities::new().status(v2::AuthStatusCapabilities::new()),
        ))
        .providers(
            caps.providers
                .as_ref()
                .map(|_| v2::ProvidersCapabilities::new()),
        );
    // 私有特性表原样带到 v2：客户端在 v2 连线上也用同一套特性做能力门控。
    if let Some(features) = caps
        .meta
        .as_ref()
        .and_then(|meta| meta.get("io.animus/features"))
    {
        let mut meta = out.meta.clone().unwrap_or_default();
        meta.insert("io.animus/features".to_string(), features.clone());
        out.meta = Some(meta);
    }
    out
}

/// v2 renamed `id` to `configId` and the group key to `groupId`; the option payloads are the
/// same shape otherwise.
pub(super) fn config_options_from_v1(
    options: Vec<v1::SessionConfigOption>,
) -> Vec<v2::SessionConfigOption> {
    options
        .into_iter()
        .filter_map(|option| {
            let kind = match option.kind {
                v1::SessionConfigKind::Select(select) => {
                    let choices = match select.options {
                        v1::SessionConfigSelectOptions::Ungrouped(list) => {
                            v2::SessionConfigSelectOptions::Ungrouped(
                                list.into_iter().map(select_option).collect(),
                            )
                        }
                        v1::SessionConfigSelectOptions::Grouped(groups) => {
                            v2::SessionConfigSelectOptions::Grouped(
                                groups
                                    .into_iter()
                                    .map(|group| {
                                        v2::SessionConfigSelectGroup::new(
                                            group.group.to_string(),
                                            group.name,
                                            group.options.into_iter().map(select_option).collect(),
                                        )
                                    })
                                    .collect(),
                            )
                        }
                        _ => v2::SessionConfigSelectOptions::Ungrouped(Vec::new()),
                    };
                    v2::SessionConfigKind::Select(v2::SessionConfigSelect::new(
                        select.current_value.to_string(),
                        choices,
                    ))
                }
                v1::SessionConfigKind::Boolean(toggle) => v2::SessionConfigKind::Boolean(
                    v2::SessionConfigBoolean::new(toggle.current_value),
                ),
                // A kind this build does not know cannot be published: v2 clients would have
                // no way to read its value back.
                _ => return None,
            };
            let mut converted =
                v2::SessionConfigOption::new(option.id.to_string(), option.name, kind);
            if let Some(description) = option.description {
                converted = converted.description(description);
            }
            if let Some(category) =
                json_between::<_, v2::SessionConfigOptionCategory>(option.category)
            {
                converted = converted.category(category);
            }
            Some(converted)
        })
        .collect()
}

fn select_option(option: v1::SessionConfigSelectOption) -> v2::SessionConfigSelectOption {
    let mut converted = v2::SessionConfigSelectOption::new(option.value.to_string(), option.name);
    if let Some(description) = option.description {
        converted = converted.description(description);
    }
    converted
}

/// Re-types two structurally identical schema values by their JSON form.
fn json_between<A: serde::Serialize, B: serde::de::DeserializeOwned>(value: A) -> Option<B> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(super) fn commands(commands: Vec<v1::AvailableCommand>) -> Vec<v2::AvailableCommand> {
    commands_from_v1(commands)
}

fn commands_from_v1(commands: Vec<v1::AvailableCommand>) -> Vec<v2::AvailableCommand> {
    commands
        .into_iter()
        .map(|command| {
            let mut converted = v2::Command::new(command.name, command.description);
            if let Some(v1::AvailableCommandInput::Unstructured(input)) = command.input {
                converted = converted.input(Some(v2::AvailableCommandInput::Text(
                    v2::TextCommandInput::new(input.hint),
                )));
            }
            v2::AvailableCommand::Command(converted)
        })
        .collect()
}

pub(super) fn stop_reason_from_v1(reason: v1::StopReason) -> v2::StopReason {
    match reason {
        v1::StopReason::MaxTokens => v2::StopReason::MaxTokens,
        v1::StopReason::MaxTurnRequests => v2::StopReason::MaxTurnRequests,
        v1::StopReason::Refusal => v2::StopReason::Refusal,
        v1::StopReason::Cancelled => v2::StopReason::Cancelled,
        _ => v2::StopReason::EndTurn,
    }
}

pub(super) fn usage_from_v1(usage: Option<v1::Usage>) -> Option<v2::Usage> {
    let usage = usage?;
    let mut converted = v2::Usage::new(usage.total_tokens, usage.input_tokens, usage.output_tokens);
    if let Some(tokens) = usage.thought_tokens {
        converted = converted.thought_tokens(tokens);
    }
    if let Some(tokens) = usage.cached_read_tokens {
        converted = converted.cached_read_tokens(tokens);
    }
    if let Some(tokens) = usage.cached_write_tokens {
        converted = converted.cached_write_tokens(tokens);
    }
    Some(converted)
}

pub(super) fn session_infos_from_v1(infos: Vec<v1::SessionInfo>) -> Vec<v2::SessionInfo> {
    infos.into_iter().filter_map(json_between).collect()
}

/// Re-frames a v1 `session_info_update` notification as the v2 one.
pub(super) fn notification_from_v1(
    notification: v1::SessionNotification,
) -> Option<v2::UpdateSessionNotification> {
    let update = update_from_v1(notification.update, &TurnIds::new(0))?;
    Some(v2::UpdateSessionNotification::new(
        v2::SessionId::new(notification.session_id.to_string()),
        update,
    ))
}

/// Builds the v2 permission prompt for a v1 tool call.
///
/// v2 carries the question in `title` and the tool call in `subject`; the four options are the
/// same ids the v1 handler uses, so the answer maps back one to one.
pub(super) fn permission_request(
    session_id: &v1::SessionId,
    tool_call: v1::ToolCallUpdate,
    options: Vec<v1::PermissionOption>,
) -> Option<v2::RequestPermissionRequest> {
    let title = tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| tool_call.tool_call_id.to_string());
    let subject = json_between(tool_call).map(|update: v2::ToolCallUpdate| {
        v2::RequestPermissionSubject::ToolCall(Box::new(v2::ToolCallPermissionSubject::new(update)))
    });
    let options: Vec<v2::PermissionOption> = options.into_iter().filter_map(json_between).collect();
    if options.is_empty() {
        return None;
    }
    Some(
        v2::RequestPermissionRequest::new(
            v2::SessionId::new(session_id.to_string()),
            title,
            options,
        )
        .subject(subject),
    )
}

pub(super) fn permission_response_from_v2(
    response: v2::RequestPermissionResponse,
) -> v1::RequestPermissionResponse {
    let outcome = match response.outcome {
        v2::RequestPermissionOutcome::Selected(selected) => v1::RequestPermissionOutcome::Selected(
            v1::SelectedPermissionOutcome::new(selected.option_id.to_string()),
        ),
        _ => v1::RequestPermissionOutcome::Cancelled,
    };
    v1::RequestPermissionResponse::new(outcome)
}

/// `provider_id`/`supported`/`required`/`current` are the same shape in both versions.
pub(super) fn provider_from_v1(provider: v1::ProviderInfo) -> Option<v2::ProviderInfo> {
    json_between(provider)
}

/// The elicitation request/response shapes only differ by version, so both directions are a
/// JSON round trip; a shape this build cannot represent becomes `None` and is reported as an
/// error rather than dropped.
pub(super) fn elicitation_request(
    request: v1::CreateElicitationRequest,
) -> Option<v2::CreateElicitationRequest> {
    json_between(request)
}

pub(super) fn elicitation_response(
    response: v2::CreateElicitationResponse,
) -> Option<v1::CreateElicitationResponse> {
    json_between(response)
}

pub(super) fn protocol_to_v1(protocol: v2::LlmProtocol) -> v1::LlmProtocol {
    match protocol {
        v2::LlmProtocol::Anthropic => v1::LlmProtocol::Anthropic,
        v2::LlmProtocol::OpenAi => v1::LlmProtocol::OpenAi,
        v2::LlmProtocol::Azure => v1::LlmProtocol::Azure,
        v2::LlmProtocol::Vertex => v1::LlmProtocol::Vertex,
        v2::LlmProtocol::Bedrock => v1::LlmProtocol::Bedrock,
        v2::LlmProtocol::Other(other) => v1::LlmProtocol::Other(other),
        _ => v1::LlmProtocol::Other("unsupported".to_string()),
    }
}

pub(super) fn auth_state_from_v1(state: v1::AuthState) -> v2::AuthState {
    match state {
        v1::AuthState::Authenticated => v2::AuthState::Authenticated,
        v1::AuthState::Unauthenticated => v2::AuthState::Unauthenticated,
        v1::AuthState::NotRequired => v2::AuthState::NotRequired,
        v1::AuthState::Unknown => v2::AuthState::Unknown,
        v1::AuthState::Other(other) => v2::AuthState::Other(other),
        _ => v2::AuthState::Unknown,
    }
}

pub(super) fn session_status_from_v1(status: v1::SessionStatus) -> v2::SessionStatus {
    match status {
        v1::SessionStatus::Saved => v2::SessionStatus::Saved,
        _ => v2::SessionStatus::Open,
    }
}

/// `session_not_found` is `-32002` with `data.sessionId` in both versions.
pub(super) fn session_not_found(session_id: &v2::SessionId) -> agent_client_protocol::Error {
    v2::Error::session_not_found(session_id.clone()).into()
}

pub(super) fn invalid_params(message: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message.to_string())
}

/// Translates one v1 session update into the v2 wire shape.
///
/// `None` means the update has no v2 counterpart: `current_mode_update` is dropped because v2
/// replaced modes with session config options, and the v2 handler emits a `config_option_update`
/// itself when a mode is selected.
pub(super) fn update_from_v1(
    update: v1::SessionUpdate,
    ids: &TurnIds,
) -> Option<v2::SessionUpdate> {
    Some(match update {
        v1::SessionUpdate::UserMessageChunk(chunk) => {
            let id = ids.user_id(chunk.message_id.clone());
            v2::SessionUpdate::UserMessageChunk(content_chunk(chunk, id)?)
        }
        v1::SessionUpdate::AgentMessageChunk(chunk) => {
            let id = ids.agent_id(chunk.message_id.clone());
            v2::SessionUpdate::AgentMessageChunk(content_chunk(chunk, id)?)
        }
        v1::SessionUpdate::AgentThoughtChunk(chunk) => {
            let id = ids.thought_id(chunk.message_id.clone());
            v2::SessionUpdate::AgentThoughtChunk(content_chunk(chunk, id)?)
        }
        v1::SessionUpdate::ToolCall(call) => v2::SessionUpdate::ToolCallUpdate({
            let object = with_cancel_capability(serde_json::to_value(call).ok()?)?;
            serde_json::from_value(object).ok()?
        }),
        v1::SessionUpdate::ToolCallUpdate(update) => {
            v2::SessionUpdate::ToolCallUpdate(tool_call_update(update)?)
        }
        v1::SessionUpdate::Plan(plan) => v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(
            v2::PlanUpdateContent::Items(v2::PlanItems::new("main", json_between(plan.entries)?)),
        )),
        v1::SessionUpdate::PlanUpdate(update) => {
            v2::SessionUpdate::PlanUpdate(json_between(update)?)
        }
        v1::SessionUpdate::PlanRemoved(removed) => {
            v2::SessionUpdate::PlanRemoved(json_between(removed)?)
        }
        v1::SessionUpdate::AvailableCommandsUpdate(update) => {
            v2::SessionUpdate::AvailableCommandsUpdate(v2::AvailableCommandsUpdate::new(
                commands_from_v1(update.available_commands),
            ))
        }
        v1::SessionUpdate::CurrentModeUpdate(_) => return None,
        v1::SessionUpdate::ConfigOptionUpdate(update) => v2::SessionUpdate::ConfigOptionUpdate(
            v2::ConfigOptionUpdate::new(config_options_from_v1(update.config_options)),
        ),
        v1::SessionUpdate::SessionInfoUpdate(update) => {
            v2::SessionUpdate::SessionInfoUpdate(json_between(update)?)
        }
        v1::SessionUpdate::UsageUpdate(update) => {
            v2::SessionUpdate::UsageUpdate(json_between(update)?)
        }
        v1::SessionUpdate::Notice(notice) => v2::SessionUpdate::Notice(json_between(notice)?),
        v1::SessionUpdate::CompactionUpdate(update) => {
            v2::SessionUpdate::CompactionUpdate(json_between(update)?)
        }
        v1::SessionUpdate::CompactionSummaryChunk(chunk) => {
            v2::SessionUpdate::CompactionSummaryChunk(json_between(chunk)?)
        }
        v1::SessionUpdate::SubagentUpdate(update) => {
            v2::SessionUpdate::SubagentUpdate(json_between(update)?)
        }
        v1::SessionUpdate::SessionMessage(message) => {
            v2::SessionUpdate::SessionMessage(json_between(message)?)
        }
        v1::SessionUpdate::SessionMessageChunk(chunk) => {
            v2::SessionUpdate::SessionMessageChunk(json_between(chunk)?)
        }
        _ => return None,
    })
}

fn content_chunk(chunk: v1::ContentChunk, message_id: v2::MessageId) -> Option<v2::ContentChunk> {
    Some(v2::ContentChunk::new(
        content_from_v1(chunk.content)?,
        message_id,
    ))
}

/// Flattens the v1 `fields` wrapper into the v2 flat update, omitting absent fields (an
/// explicit `null` would clear them on the client).
fn tool_call_update(update: v1::ToolCallUpdate) -> Option<v2::ToolCallUpdate> {
    // A client-cancelled command is not a failure: v1 has no cancelled status, so
    // `event.rs` flags it in `_meta` and this is where it becomes a v2 status.
    let cancelled = update
        .meta
        .as_ref()
        .and_then(|meta| meta.get("cancelled"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let mut object = serde_json::Map::new();
    object.insert(
        "toolCallId".to_string(),
        serde_json::to_value(update.tool_call_id).ok()?,
    );
    let fields = update.fields;
    let mut put = |key: &str, value: Option<serde_json::Value>| {
        if let Some(value) = value {
            object.insert(key.to_string(), value);
        }
    };
    put("kind", present(fields.kind));
    put("status", present(fields.status));
    put("title", present(fields.title));
    put("name", present(fields.name));
    put("content", present(fields.content));
    put("locations", present(fields.locations));
    put("rawInput", present(fields.raw_input));
    put("rawOutput", present(fields.raw_output));
    if cancelled {
        object.insert("status".to_string(), serde_json::json!("cancelled"));
        object.insert("capabilities".to_string(), serde_json::Value::Null);
    }
    let object = with_cancel_capability(serde_json::Value::Object(object))?;
    serde_json::from_value(object).ok()
}

/// Advertises `capabilities.cancel` on an `execute` tool call that is still running.
///
/// Only execute-class tools are cancellable: the sandbox registers their child process in the
/// session registry, so `tool_call/cancel` / `terminal/cancel` can actually kill it. A final
/// status clears the capability (the process is gone).
fn with_cancel_capability(mut object: serde_json::Value) -> Option<serde_json::Value> {
    let map = object.as_object_mut()?;
    let cancellable = map.get("kind").and_then(|value| value.as_str()) == Some("execute")
        && !matches!(
            map.get("status").and_then(|value| value.as_str()),
            Some("completed") | Some("failed") | Some("cancelled")
        );
    if cancellable {
        map.insert(
            "capabilities".to_string(),
            serde_json::json!({"cancel": {}}),
        );
    }
    Some(object)
}

fn present<T: serde::Serialize>(value: Option<T>) -> Option<serde_json::Value> {
    value.and_then(|value| serde_json::to_value(value).ok())
}
