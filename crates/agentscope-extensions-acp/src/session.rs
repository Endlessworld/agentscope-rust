//! ACP agent session: holds per-session state and drives the core [`ReActAgent`]
//! on each prompt, streaming ACP `SessionUpdate` notifications back to the client.
//!
//! Mirrors `AgiAgentSession` from the Kotlin `AgiHarnessAgent`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{
    ContentBlock, CreateElicitationRequest, CreateElicitationResponse, MessageId, PermissionOption,
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigOption, SessionId,
    SessionMode, SessionModeState, StopReason, ToolCallUpdate,
};
use agent_client_protocol::{v1::ConnectionTo, Client};
use agentscope_core::agent::CallableAgent;
use agentscope_core::event::{AgentEvent, ConfirmResult};
use agentscope_core::message::{ChatUsage, Msg, ToolUseBlock};
use agentscope_core::model::catalog::ModelDescriptor;
use agentscope_core::model::{GenerateOptions, DEFAULT_CONTEXT_WINDOW};
use agentscope_core::permission::{PermissionBehavior, PermissionRule};
use agentscope_core::ConfirmHandler;
use sha2::{Digest, Sha256};

use agentscope_harness::agent::bus::MessageBus;
use agentscope_harness::agent::HarnessAgent;

use super::config::{self, ModeChoice, ModelChoice};
use super::elicitation::{ElicitationClient, ElicitationSlot};
use super::event;
use super::permission_store::PermissionRuleStore;
use super::support::ClientSupport;

/// Current configuration of an ACP session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionConfig {
    pub model: String,
    pub mode: String,
    /// Reasoning-effort / thought level (`disabled` / `low` / `medium` /
    /// `high` / `max`), mirroring Java `ThoughtLevel`. Applied as per-call
    /// generation defaults on the *next* prompt.
    pub thought_level: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: "default".to_string(),
            mode: config::default_mode().to_string(),
            thought_level: config::DEFAULT_THOUGHT_LEVEL.to_string(),
        }
    }
}

/// Permission option ids advertised to ACP clients, mirroring the Java agent's
/// `PermissionOptionKind` naming.
const OPTION_ALLOW_ONCE: &str = "ALLOW_ONCE";
const OPTION_ALLOW_ALWAYS: &str = "ALLOW_ALWAYS";
const OPTION_REJECT_ONCE: &str = "REJECT_ONCE";
const OPTION_REJECT_ALWAYS: &str = "REJECT_ALWAYS";

/// Client handle used to push `session/request_permission` (human review)
/// requests back to the ACP client and await the user's decision.
///
/// The wrapped connection is the same `ConnectionTo<Client>` the coding agent's
/// prompt handler receives; it is cheaply cloneable and safe to share into the
/// spawned task that runs the agent.
#[derive(Clone)]
pub struct AcpClientHandle {
    connection: ConnectionTo<Client>,
}

/// How long an interactive request may stay unanswered.
///
/// The user genuinely needs time to read a permission prompt or fill in an elicitation form,
/// so this is generous rather than tight. The point is that the wait is *bounded*: a client
/// that never answers (a disconnected IDE, a dropped response) used to block the tool, the
/// run and the prompt forever, with `session/cancel` unable to reach it.
const INTERACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How long an MCP server may take to start and complete its handshake.
///
/// A server that hangs during startup (a missing binary that waits on stdin, an unreachable
/// URL) used to block session creation indefinitely, before the user could even send a prompt.
const MCP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl AcpClientHandle {
    /// Wraps an agent-side connection whose counterpart is the ACP client.
    pub(crate) fn new(connection: ConnectionTo<Client>) -> Self {
        Self { connection }
    }

    /// Pushes a `session/update` notification to the client.
    pub(crate) fn notify(
        &self,
        notification: agent_client_protocol::v1::schema::SessionNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        self.connection.send_notification(notification)
    }

    /// Asks the client to request the user's permission for `tool_call`,
    /// returning the selected/rejected outcome.
    ///
    /// Only safe to await from a task outside the dispatch loop (the agent run
    /// already lives in a `ConnectionTo::spawn`ed task), which is exactly how
    /// the confirm handler invokes it.
    pub(crate) async fn request_permission(
        &self,
        session_id: &SessionId,
        tool_call: ToolCallUpdate,
    ) -> Result<RequestPermissionResponse, agent_client_protocol::Error> {
        let request =
            RequestPermissionRequest::new(session_id.clone(), tool_call, permission_options());
        // Bounded: an unresponsive client must not pin the tool call forever. A timeout is a
        // **rejection**, not a channel failure: the `Err` path auto-approves (Java parity for a
        // missing client), which must never turn a dropped answer into a granted tool call.
        match tokio::time::timeout(
            INTERACTION_TIMEOUT,
            self.connection.send_request(request).block_task(),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("permission request timed out; rejecting the tool call");
                Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            }
        }
    }

    /// Pushes an `elicitation/create` form to the user and awaits their answer.
    ///
    /// Only safe to await from a task outside the dispatch loop (the agent run
    /// already lives in a `ConnectionTo::spawn`ed task), which is how the
    /// `ask_user` tool invokes it.
    pub(crate) async fn elicit(
        &self,
        request: CreateElicitationRequest,
    ) -> std::result::Result<CreateElicitationResponse, String> {
        match tokio::time::timeout(
            INTERACTION_TIMEOUT,
            self.connection.send_request(request).block_task(),
        )
        .await
        {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(_) => Err("elicitation timed out: the client did not answer".to_string()),
        }
    }
    /// Binds a handle to a client-declared MCP-over-ACP server.
    ///
    /// The stateless protocol has no handshake, so this is purely local; the
    /// first network round-trip is the `tools/list` issued by
    /// [`super::mcp_over_acp::McpAcpConnection::bridge_tools`].
    pub(crate) fn connect_mcp_acp(
        &self,
        name: &str,
        server_id: agent_client_protocol::v1::schema::McpServerAcpId,
    ) -> super::mcp_over_acp::McpAcpConnection {
        super::mcp_over_acp::McpAcpConnection::new(self.connection.clone(), name, server_id)
    }
}

/// Bridges the live ACP connection into the `ask_user` tool.
#[async_trait::async_trait]
impl ElicitationClient for AcpClientHandle {
    async fn elicit(
        &self,
        request: CreateElicitationRequest,
    ) -> std::result::Result<CreateElicitationResponse, String> {
        AcpClientHandle::elicit(self, request).await
    }
}

/// The four choices surfaced to the reviewer, mirroring the Java agent.
fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            OPTION_ALLOW_ONCE,
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            OPTION_ALLOW_ALWAYS,
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            OPTION_REJECT_ONCE,
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            OPTION_REJECT_ALWAYS,
            "Always reject",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Builds the rule recorded when the user picks an "always" option: it matches
/// the exact tool call pattern (tool name + serialized arguments), so the same
/// invocation is decided automatically on later turns.
fn always_rule_for(call: &ToolUseBlock, allow: bool) -> PermissionRule {
    let behavior = if allow {
        PermissionBehavior::Allow
    } else {
        PermissionBehavior::Deny
    };
    let content = if call.input.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&call.input).unwrap_or_default())
    };
    PermissionRule::new(call.name.clone(), content, behavior, "user-permission")
}

/// An epoch-millisecond timestamp rendered the way `_meta` timestamps are.
fn rfc3339_millis(millis: u64) -> String {
    chrono::DateTime::from_timestamp_millis(millis as i64)
        .map(|at| at.to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339())
}

/// Pairs the two `subagent_update` state notices a permission round-trip owes the client.
///
/// A child that blocks on a user decision reports `requires_action` before the request, and
/// `running` once the answer arrives; without the pair the card would keep claiming it waits
/// for a decision that was already made.
#[derive(Clone)]
pub(crate) struct SubagentStateNotice {
    parent: SessionId,
    child: String,
}

impl SubagentStateNotice {
    pub(crate) fn new(parent: SessionId, child: impl Into<String>) -> Self {
        Self {
            parent,
            child: child.into(),
        }
    }

    /// Reports that the child is waiting for the user.
    pub(crate) fn requires_action(&self, client: &AcpClientHandle) {
        if let Err(error) = client.notify(super::event::subagent_requires_action(
            &self.parent,
            &self.child,
        )) {
            tracing::debug!(%error, "subagent requires_action could not be delivered");
        }
    }

    /// Reports that the child resumed after the decision.
    pub(crate) fn running(&self, client: &AcpClientHandle) {
        if let Err(error) = client.notify(super::event::subagent_running(&self.parent, &self.child))
        {
            tracing::debug!(%error, "subagent running could not be delivered");
        }
    }
}

/// Routes a subagent's `Ask` tool decisions to the ACP client under the **child** session id.
///
/// Installed on every freshly spawned child by [`SubagentPermissionHook`]: the harness knows the
/// child's user-visible id only after the spawn tool minted it, which is why this cannot be baked
/// into the agent factory.
pub struct SubagentPermissionHook {
    client: Arc<Mutex<Option<Arc<AcpClientHandle>>>>,
    store: Arc<PermissionRuleStore>,
    parent: SessionId,
    /// Every child of this session's tree, keyed by child session id.
    ///
    /// Shared with [`AcpAgentSession`]: a nested child is owned by an intermediate subagent's
    /// spawn tool, so cancelling it by id has no other route back to the agent.
    agents: Arc<Mutex<HashMap<String, Arc<dyn CallableAgent>>>>,
}

impl SubagentPermissionHook {
    pub fn new(
        client: Arc<Mutex<Option<Arc<AcpClientHandle>>>>,
        store: Arc<PermissionRuleStore>,
        parent: SessionId,
        agents: Arc<Mutex<HashMap<String, Arc<dyn CallableAgent>>>>,
    ) -> Self {
        Self {
            client,
            store,
            parent,
            agents,
        }
    }
}

impl agentscope_harness::tool::SubagentRuntimeHook for SubagentPermissionHook {
    fn on_child_ready(
        &self,
        parent_session: Option<&str>,
        child_session: &str,
        agent: &Arc<dyn CallableAgent>,
    ) {
        // A nested spawn names the intermediate subagent as the parent; a direct child of the
        // session this hook belongs to has no parent id to report.
        let parent = parent_session
            .map(|id| SessionId::new(id.to_string()))
            .unwrap_or_else(|| self.parent.clone());
        // Let the child tag its own spawns (nested delegation) before it starts working.
        agent.set_host_session_id(child_session);
        self.agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(child_session.to_string(), Arc::clone(agent));
        let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            // The child is spawned from a prompt, so a session that reached this point has a
            // live connection; without one there is nobody to ask, and the default (an error)
            // is the honest answer.
            tracing::warn!(
                child = %child_session,
                "subagent spawned without a live client connection; its tool prompts will fail"
            );
            return;
        };
        let handler = acp_confirm_handler(
            SessionId::new(child_session.to_string()),
            client,
            Arc::clone(agent),
            Arc::clone(&self.store),
            Some(SubagentStateNotice::new(parent, child_session.to_string())),
        );
        agent.set_confirm_handler(Some(handler));
    }
}

/// Builds the core `ConfirmHandler` that turns an `Ask` permission decision
/// into an ACP `session/request_permission` round-trip and maps the outcome
/// back to a [`ConfirmResult`].
fn acp_confirm_handler(
    session_id: SessionId,
    client: Arc<AcpClientHandle>,
    agent: Arc<dyn CallableAgent>,
    store: Arc<PermissionRuleStore>,
    notice: Option<SubagentStateNotice>,
) -> ConfirmHandler {
    Arc::new(move |call: ToolUseBlock| {
        let client = Arc::clone(&client);
        let agent = Arc::clone(&agent);
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        let notice = notice.clone();
        Box::pin(async move {
            let raw_input = if call.input.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(call.input.clone()))
            };
            let tool_call =
                event::tool_call_update_for_permission(call.id.clone(), &call.name, raw_input);
            // The card must show "waiting for you" *before* the prompt arrives: a subagent that
            // silently blocks reads as working, and the user never learns they are the blocker.
            if let Some(notice) = &notice {
                notice.requires_action(&client);
            }
            let outcome = client.request_permission(&session_id, tool_call).await;
            // Whatever the answer, the child is working again (a rejection just makes that tool
            // call fail); the terminal `idle` of the turn reports the real end.
            if let Some(notice) = &notice {
                notice.running(&client);
            }
            match outcome {
                Ok(response) => match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => match option_id.to_string().as_str() {
                        OPTION_ALLOW_ONCE => ConfirmResult::allowed(),
                        OPTION_ALLOW_ALWAYS => {
                            let rule = always_rule_for(&call, true);
                            agent.add_permission_rule(rule.clone());
                            if let Err(error) = store.add(rule) {
                                tracing::warn!(
                                    tool = %call.name,
                                    %error,
                                    "failed to persist 'always allow' permission rule"
                                );
                            }
                            ConfirmResult::allowed()
                        }
                        OPTION_REJECT_ONCE => ConfirmResult::denied("Permission rejected by user"),
                        OPTION_REJECT_ALWAYS => {
                            let rule = always_rule_for(&call, false);
                            agent.add_permission_rule(rule.clone());
                            if let Err(error) = store.add(rule) {
                                tracing::warn!(
                                    tool = %call.name,
                                    %error,
                                    "failed to persist 'always reject' permission rule"
                                );
                            }
                            ConfirmResult::denied("Permission rejected by user")
                        }
                        other => {
                            // Unknown option id: fail open to avoid stalling the
                            // agent, and log so misconfiguration is visible.
                            tracing::warn!(
                                tool = %call.name,
                                option = other,
                                "unexpected permission option id; allowing"
                            );
                            ConfirmResult::allowed()
                        }
                    },
                    RequestPermissionOutcome::Cancelled => {
                        ConfirmResult::denied("Permission request cancelled")
                    }
                    // The schema enum is non-exhaustive; treat any future
                    // outcome as a rejection to avoid executing on ambiguity.
                    _ => ConfirmResult::denied("Unexpected permission outcome"),
                },
                Err(error) => {
                    // No working client channel: mirror the Java behaviour of
                    // auto-approving when ClientSessionOperations is absent.
                    tracing::warn!(
                        tool = %call.name,
                        %error,
                        "request_permission failed; auto-approving tool"
                    );
                    ConfirmResult::allowed()
                }
            }
        })
    })
}

/// Maps a thought level to per-call generation defaults.
///
/// - `auto` (default): sends nothing — the provider default applies.
/// - Explicit levels pass through unchanged (`max` is no longer clamped to
///   `high`); catalog validation decides whether the selected model accepts it.
/// - `off` / `disabled` / `none` map to `reasoning_effort: "none"` only when the
///   catalog advertised that value (the session validates before applying).
///
/// Vendor-specific body params (`enable_thinking` / `thinking_budget`) are
/// never sent: strict OpenAI-compatible gateways (e.g. the Go/Zen gateways)
/// validate the body and reject unknown fields with
/// `json: unknown field "thinking_budget"`.
fn thought_level_params(level: &str) -> GenerateOptions {
    if level == "auto" || level.is_empty() {
        return GenerateOptions::default();
    }
    let effort = match level {
        "off" | "disabled" | "none" => "none",
        other => other,
    };
    GenerateOptions::builder().reasoning_effort(effort).build()
}

/// Maps a turn's accumulated [`ChatUsage`] to the protocol `Usage` reported in
/// `PromptResponse`. Returns `None` when the turn saw no usage at all, so an
/// untouched turn does not report a misleading zero-token payload.
fn turn_usage_to_response(usage: &ChatUsage) -> Option<agent_client_protocol::v1::schema::Usage> {
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return None;
    }
    Some(
        agent_client_protocol::v1::schema::Usage::new(
            usage.total_tokens(),
            usage.input_tokens,
            usage.output_tokens,
        )
        .cached_read_tokens(Some(usage.cached_tokens)),
    )
}

/// Clears the session's "prompt running" marker on every exit path, including early returns
/// and panics, so LRU eviction never mistakes a finished prompt for a live one.
struct RunningPrompt(Arc<AtomicBool>);

impl Drop for RunningPrompt {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Resolves once the session's cancel generation moves away from `started`.
///
/// [`AcpAgentSession::cancel`] is a plain atomic bump (it is called from synchronous contexts,
/// including the connection's notification handler), so there is nothing to await on directly.
/// A short poll keeps that signature while still letting a prompt that is waiting for its turn
/// notice a cancel within one tick instead of running the turn anyway.
async fn cancel_epoch_moved(epoch: &std::sync::atomic::AtomicU64, started: u64) {
    loop {
        if epoch.load(Ordering::SeqCst) != started {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// What a turn is started from.
///
/// A prompt carries user content; a wakeup carries none and exists only so the middlewares
/// get a reasoning step to inject pending background results into.
enum TurnInput {
    Prompt(Vec<ContentBlock>),
    Wakeup,
}

/// Descriptor of the selected model, resolved once when the session binds its model.
///
/// `resolved` is not the same as `descriptor.is_some()`: a model the catalog does not know has no
/// descriptor, and remembering that answer is what keeps the released catalog from being parsed
/// again on every turn.
#[derive(Clone, Default)]
struct SelectedDescriptor {
    resolved: bool,
    descriptor: Option<Arc<ModelDescriptor>>,
}

impl SelectedDescriptor {
    /// Resolves `model` against the model list, so the answer needs no catalog lookup afterwards.
    fn resolve(models: &[ModelChoice], model: &str) -> Self {
        Self {
            resolved: true,
            descriptor: models
                .iter()
                .find(|choice| choice.id == model)
                .and_then(super::config::resolve_choice),
        }
    }
}

/// A single ACP session backed by a [`HarnessAgent`].
#[derive(Clone)]
pub struct AcpAgentSession {
    session_id: SessionId,
    cwd: PathBuf,
    agent: Arc<HarnessAgent>,
    /// Shared across clones: handlers mutate the live session while the
    /// registry holds stale clones, so config changes (model / thought level /
    /// mode) must not be lost on `register_session` write-back.
    config: Arc<Mutex<SessionConfig>>,
    models: Vec<ModelChoice>,
    /// Catalog descriptor of the selected model, resolved once at bind time.
    ///
    /// The full catalog (~15 MB for ~9k models) is only needed to *list* models and to resolve a
    /// switch. Caching the descriptor here keeps the per-turn path (prompt block validation, the
    /// catalog key) independent of that table, so the host can hand the memory back while a
    /// session runs. Shared across clones: a clone taken from the registry mutates the same
    /// session.
    descriptor: Arc<Mutex<SelectedDescriptor>>,
    modes: Vec<ModeChoice>,
    /// Optional resolver building a model from a model id (dynamic model
    /// switching). When set, `setModel` swaps the live agent's model on the next
    /// prompt.
    model_builder: Option<
        Arc<
            dyn Fn(
                    &str,
                    &str,
                )
                    -> std::result::Result<Arc<dyn agentscope_core::model::Model>, String>
                + Send
                + Sync,
        >,
    >,
    /// The model id currently applied to the live agent (for change detection).
    applied_model: String,
    /// Cancellation generation for the prompt turn.
    ///
    /// A single session-wide flag leaked across turns: a cancel that arrived while the next
    /// prompt was already waiting behind `prompt_lock` either stopped the wrong turn or was
    /// silently cleared by that turn's start. Each prompt captures the generation it started
    /// under, and `cancel` advances it — so only the turn running at cancel time stops.
    cancel_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Serializes prompts for this session. ACP forbids overlapping prompt turns, but clients
    /// do send a follow-up before the previous response arrives (or a host retries one); two
    /// concurrent runs on the same session used to be rejected as `turn_unfinished`.
    prompt_lock: Arc<tokio::sync::Mutex<()>>,
    /// Whether this session was closed. A prompt that finishes after `session/close` must not
    /// put the session back into the registry.
    closed: Arc<AtomicBool>,
    /// Whether a prompt is currently being executed (guards LRU eviction).
    running: Arc<AtomicBool>,
    usage: ChatUsage,
    /// The provider-reported input tokens of the most recent model call. This is the
    /// real size of the live context (system prompt + tools + history), reported back
    /// by the provider; `usage.input_tokens` is cumulative and must not drive the gauge.
    ///
    /// Shared: the catalog/background tasks hold their own clone of the session and push
    /// usage updates, so a per-clone counter would report a stale (usually zero) gauge.
    last_input_tokens: Arc<std::sync::atomic::AtomicU64>,
    /// Client connection used to push `session/request_permission` requests
    /// when a tool call needs human confirmation. Shared across clones so a
    /// background catalog task can also push config/usage updates.
    client: Arc<Mutex<Option<Arc<AcpClientHandle>>>>,
    /// Shared slot the `ask_user` tool reads the live connection from. It is
    /// filled whenever [`set_client`](Self::set_client) binds a connection; the
    /// tool holds the same `Arc`, so it sees the update without being rebuilt.
    elicitation_slot: ElicitationSlot,
    /// Persistent "always allow / always reject" rule store for this workspace.
    permission_store: Arc<PermissionRuleStore>,
    /// The long-lived runtime handle for this ACP session.
    ///
    /// Reusing one handle keeps the dispatcher projection and the receipt fold warm across
    /// prompts. Opening a fresh handle per prompt re-read the whole journal (and every state
    /// checkpoint) before each turn, which made long sessions slow and memory-hungry.
    runtime: Arc<tokio::sync::Mutex<Option<Arc<agentscope_harness::agent::AgentSession>>>>,
    /// Manages MCP client connections for this session (stdio servers declared
    /// by the ACP client via `mcp_servers`). Shared across clones so tools keep
    /// working after `register_session` write-back.
    mcp_manager: Arc<agentscope_harness::agent::McpClientManager>,
    /// Server names already bridged into the toolkit this session (dedup).
    bridged_servers: Arc<Mutex<Vec<String>>>,
    /// Capabilities the connected client advertised at `initialize`; several
    /// `session/update` variants are only legal when the matching capability was
    /// advertised, so every optional update is gated on this.
    support: ClientSupport,
    /// Token usage accumulated during the turn that is running (reset per turn),
    /// reported back as `PromptResponse.usage`.
    turn_usage: ChatUsage,
    /// Workspace roots beyond `cwd`, reported in `session/list`.
    additional_directories: Vec<PathBuf>,
    /// Sidecar file used to restore `{model, mode, thought_level}` after a restart.
    config_path: PathBuf,
    /// Client-visible session title, if one was set.
    title: Option<String>,
    /// Whether this session is archived (ACP-36). Persisted as a marker file next to the
    /// config sidecar, so an archived session stays hidden from `session/list` across restarts.
    archived: bool,
    /// ISO-8601 timestamp of the last prompt turn.
    ///
    /// Shared: `session/list` and LRU eviction read it through a *clone* taken from the
    /// registry, so a per-clone value would keep reporting the moment the clone was made.
    updated_at: Arc<Mutex<Option<String>>>,
    /// Out-of-band frames from the harness's subagent channel.
    ///
    /// Taken exactly once, by the forwarder started on the first prompt (see
    /// [`Self::start_subagent_forwarder`]): a subagent keeps running after the parent's prompt
    /// returns, so its frames cannot be consumed by the prompt loop.
    subagent_events: Arc<
        Mutex<
            Option<tokio::sync::mpsc::UnboundedReceiver<agentscope_harness::tool::SubagentFrame>>,
        >,
    >,
    /// Child session id → the ACP session that owns it.
    ///
    /// Shared with the coding agent: `session/cancel` addressed to a child session has to reach
    /// the agent that spawned it, and the child is not a registered session of its own.
    subagent_children: Arc<Mutex<HashMap<String, String>>>,
    /// Every live child of this session's subagent tree, including nested ones.
    ///
    /// Shared with the permission bridge (which is handed each child, whatever spawned it): the
    /// root's spawn tool owns only its direct children, so a cancel naming a grandchild is
    /// resolved here.
    subagent_agents: Arc<Mutex<HashMap<String, Arc<dyn agentscope_core::agent::CallableAgent>>>>,
}

fn session_config_path(cwd: &std::path::Path, session_id: &SessionId) -> PathBuf {
    let safe: String = session_id
        .to_string()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    cwd.join(".agentscope")
        .join("acp-sessions")
        .join(format!("{safe}.json"))
}

/// Directory holding the per-session creation records (config sidecars) of a workspace.
fn session_config_dir(cwd: &std::path::Path) -> PathBuf {
    cwd.join(".agentscope").join("acp-sessions")
}

/// Path of the marker file recording that a session is archived.
///
/// A marker (rather than a field in the config sidecar) keeps archive state independent of
/// the client-selectable config: archiving a session that never had a config still works,
/// and a later config write cannot clear the flag.
fn archive_marker_path(cwd: &std::path::Path, session_id: &SessionId) -> PathBuf {
    session_config_path(cwd, session_id).with_extension("archived")
}

/// Whether `session_id` is archived on disk under `cwd`.
pub(crate) fn is_archived_on_disk(cwd: &std::path::Path, session_id: &SessionId) -> bool {
    archive_marker_path(cwd, session_id).exists()
}

/// Writes or removes the archive marker for `session_id` under `cwd`.
///
/// Failures are logged, not propagated: the in-memory flag is already authoritative for this
/// process, and an unwritable workspace should not fail the protocol call.
pub(crate) fn set_archived_on_disk(cwd: &std::path::Path, session_id: &SessionId, archived: bool) {
    let path = archive_marker_path(cwd, session_id);
    if !archived {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %error, "failed to clear the archive marker");
            }
        }
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::warn!(path = %parent.display(), %error, "failed to create the archive marker directory");
            return;
        }
    }
    if let Err(error) = std::fs::write(&path, b"") {
        tracing::warn!(path = %path.display(), %error, "failed to write the archive marker");
    }
}

/// Whether `cwd` holds a creation record for `session_id`.
///
/// Every session writes its config sidecar when it is created, so the file is the durable proof
/// that the id belongs to this workspace — even when the journal is still empty (`session/new`
/// never prompted) or the archive marker was already cleared.
pub(crate) fn is_recorded_on_disk(cwd: &std::path::Path, session_id: &SessionId) -> bool {
    session_config_path(cwd, session_id).exists()
}

/// Every session id `cwd` has a creation record for.
pub(crate) fn recorded_session_ids(cwd: &std::path::Path) -> Vec<String> {
    let entries = match std::fs::read_dir(session_config_dir(cwd)) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .filter_map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect()
}

/// Removes the per-session workspace files of `session_id`: the creation record and the archive
/// marker.
///
/// `session/delete` must not leave a record behind that would put the session back into
/// `session/list` (a session created but never prompted has nothing else on disk). The journal is
/// not touched here — the harness owns it, and a deleted-but-journaled session reads as it did
/// before.
pub(crate) fn forget_session_on_disk(cwd: &std::path::Path, session_id: &SessionId) {
    let record = session_config_path(cwd, session_id);
    for path in [record.clone(), record.with_extension("archived")] {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %error, "failed to remove the session record");
            }
        }
    }
}

fn load_session_config(path: &std::path::Path) -> Option<SessionConfig> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_session_config(path: &std::path::Path, config: &SessionConfig) {
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::warn!(path = %parent.display(), %error, "failed to create ACP session config directory");
            return;
        }
    }
    let bytes = match serde_json::to_vec_pretty(config) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to serialize ACP session config");
            return;
        }
    };
    // Write-then-rename: a crash mid-write must not leave a truncated config that the next
    // start cannot parse. The temporary file lives next to the target so the rename stays on
    // one filesystem, and the parent directory is fsynced so the entry itself is durable.
    let temporary = path.with_extension("json.tmp");
    if let Err(error) = write_and_sync(&temporary, &bytes) {
        tracing::warn!(path = %temporary.display(), %error, "failed to write ACP session config");
        let _ = std::fs::remove_file(&temporary);
        return;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        tracing::warn!(path = %path.display(), %error, "failed to replace ACP session config");
        let _ = std::fs::remove_file(&temporary);
        return;
    }
    if let Some(parent) = path.parent() {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

/// Writes `bytes` to `path` and fsyncs the file before returning.
fn write_and_sync(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Derives the prompt's idempotency key from what the user actually sent.
///
/// ACP's `PromptRequest` carries no client request id, so the key is the hash of the prompt's
/// text plus the moment the prompt was accepted. Hashing the content makes the journal say
/// *which* prompt a turn belongs to (a random UUID said nothing); the timestamp keeps two
/// identical questions two distinct turns, so a genuine repeat is never merged away while a
/// retry inside this process keeps reusing the same key.
fn prompt_idempotency_key(message: &Msg, accepted_at_ms: i64) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let agentscope_core::message::ContentBlock::Text(block) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block.text);
        }
    }
    let digest = Sha256::digest(text.as_bytes());
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("prompt-{}-{accepted_at_ms}", &hash[..16])
}

impl AcpAgentSession {
    /// Creates a new session for the given working directory.
    pub fn new(
        session_id: SessionId,
        cwd: PathBuf,
        agent: HarnessAgent,
        models: Vec<ModelChoice>,
        modes: Vec<ModeChoice>,
    ) -> Self {
        let permission_store = Arc::new(PermissionRuleStore::at_dir(&cwd));
        let config_path = session_config_path(&cwd, &session_id);
        let persisted = load_session_config(&config_path);
        let actual_model = agent
            .model()
            .map(|model| model.model_name().to_string())
            .unwrap_or_default();
        let initial_model = persisted
            .as_ref()
            .map(|config| config.model.clone())
            .filter(|model| models.iter().any(|choice| choice.id == *model))
            .unwrap_or_else(|| {
                if models.iter().any(|choice| choice.id == actual_model) {
                    actual_model.clone()
                } else {
                    models
                        .first()
                        .map(|choice| choice.id.clone())
                        .unwrap_or_else(|| "default".to_string())
                }
            });
        let initial_mode = persisted
            .as_ref()
            .map(|config| config.mode.clone())
            .filter(|mode| modes.iter().any(|choice| choice.id == *mode))
            .unwrap_or_else(|| SessionConfig::default().mode);
        let initial_thought = persisted
            .as_ref()
            .map(|config| config.thought_level.clone())
            .filter(|level| {
                super::config::thought_level_ids_for(&initial_model, &models)
                    .iter()
                    .any(|id| id == level)
            })
            .unwrap_or_else(|| super::config::DEFAULT_THOUGHT_LEVEL.to_string());
        // Resolve the descriptor of the initial model now: the caller is building the model list,
        // so the catalog is resident -- and the host releases it as soon as every session is bound.
        let descriptor = SelectedDescriptor::resolve(&models, &initial_model);
        let archived = is_archived_on_disk(&cwd, &session_id);
        let mut session = Self {
            session_id,
            cwd,
            agent: Arc::new(agent),
            config: Arc::new(Mutex::new(SessionConfig::default())),
            models,
            descriptor: Arc::new(Mutex::new(descriptor)),
            modes,
            model_builder: None,
            applied_model: actual_model,
            cancel_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            prompt_lock: Arc::new(tokio::sync::Mutex::new(())),
            closed: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            usage: ChatUsage::default(),
            last_input_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            client: Arc::new(Mutex::new(None)),
            elicitation_slot: Arc::new(Mutex::new(None)),
            permission_store,
            mcp_manager: Arc::new(agentscope_harness::agent::McpClientManager::new()),
            runtime: Arc::new(tokio::sync::Mutex::new(None)),
            bridged_servers: Arc::new(Mutex::new(Vec::new())),
            support: ClientSupport::new(),
            turn_usage: ChatUsage::default(),
            additional_directories: Vec::new(),
            config_path,
            title: None,
            archived,
            updated_at: Arc::new(Mutex::new(None)),
            subagent_events: Arc::new(Mutex::new(None)),
            subagent_children: Arc::new(Mutex::new(HashMap::new())),
            subagent_agents: Arc::new(Mutex::new(HashMap::new())),
        };
        {
            let mut config = session
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            config.model = initial_model;
            config.mode = initial_mode;
            config.thought_level = initial_thought;
        }
        session.apply_mode();
        // The config sidecar doubles as the session's creation record: `session/new` writes it
        // before the first prompt, so a session that never ran a turn still leaves a durable
        // trace in its workspace (there is no journal yet). `session/list`, `session/status` and
        // `session/resume` read it back.
        session.persist_config();
        session
    }

    /// The session id.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The runtime context that identifies this ACP session in the journal.
    ///
    /// The user id is left unset on purpose: `HarnessAgent::session_key` falls back to the
    /// agent's pinned state, so a host that configured a tenant keeps it.
    fn session_context(&self) -> agentscope_core::agent::RuntimeContext {
        agentscope_core::agent::RuntimeContext::new().with_session_id(self.session_id.to_string())
    }

    /// The working directory.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// The underlying agent.
    pub fn agent(&self) -> &HarnessAgent {
        &self.agent
    }

    /// Capabilities the connected client advertised at `initialize`.
    pub fn client_support(&self) -> &ClientSupport {
        &self.support
    }

    /// Shares the agent-level client-capability view with this session (clones
    /// share the same underlying store, so a later `initialize` is visible
    /// without re-registering the session).
    pub fn set_client_support(&mut self, support: ClientSupport) {
        self.support = support;
    }

    /// Records the working directory the session was opened with, plus any
    /// additional workspace roots the client declared.
    pub fn set_additional_directories(&mut self, dirs: Vec<PathBuf>) {
        self.additional_directories = dirs;
    }

    /// The additional workspace roots reported for this session.
    pub fn additional_directories(&self) -> &[PathBuf] {
        &self.additional_directories
    }

    /// Builds an advisory notification for a non-fatal condition (e.g. a failed
    /// model switch or an MCP server that could not be bridged).
    ///
    /// Emits a `SessionUpdate::Notice` when the client advertised
    /// `session.notices`, otherwise degrades to an ordinary agent message so the
    /// information is never silently dropped.
    fn advisory_notification(
        &self,
        message: impl Into<String>,
    ) -> agent_client_protocol::v1::schema::SessionNotification {
        let message = message.into();
        if self.support.supports_notices() {
            event::notice(
                self.session_id.clone(),
                agent_client_protocol::v1::schema::NoticeSeverity::Warning,
                message,
                None,
            )
        } else {
            event::message_chunk(self.session_id.clone(), format!("{message}\n"), None)
        }
    }

    /// Builds this session's `SessionInfo` (see [`Self::session_info`]) and
    /// emits it as a `SessionInfoUpdate` so the client's session list stays in
    /// sync with the latest activity time.
    pub(crate) fn session_info_update(
        &self,
    ) -> agent_client_protocol::v1::schema::SessionNotification {
        use agent_client_protocol::v1::schema::{
            SessionInfoUpdate, SessionNotification, SessionUpdate,
        };
        let mut update = SessionInfoUpdate::new();
        if let Some(title) = &self.title {
            update = update.title(title.clone());
        }
        if let Some(updated_at) = self.updated_at() {
            update = update.updated_at(updated_at);
        }
        SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::SessionInfoUpdate(update),
        )
    }

    /// Sets the client-visible session title (subsequent turns advertise it).
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }

    /// Whether this session is archived (hidden from the default `session/list`).
    pub fn is_archived(&self) -> bool {
        self.archived
    }

    /// Sets the archive flag and persists it (ACP-36).
    pub fn set_archived(&mut self, archived: bool) {
        self.archived = archived;
        set_archived_on_disk(&self.cwd, &self.session_id, archived);
    }

    /// Projects this session into the protocol `SessionInfo` returned by
    /// `session/list` (cwd, additional roots, title and last-activity time).
    pub fn session_info(&self) -> agent_client_protocol::v1::schema::SessionInfo {
        let mut info = agent_client_protocol::v1::schema::SessionInfo::new(
            self.session_id.clone(),
            self.cwd.clone(),
        )
        .additional_directories(self.additional_directories.clone())
        .archived(self.archived);
        if let Some(title) = &self.title {
            info = info.title(title.clone());
        }
        if let Some(updated_at) = self.updated_at() {
            info = info.updated_at(updated_at);
        }
        info
    }

    /// Current model id.
    pub fn model(&self) -> String {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .model
            .clone()
    }

    /// Sort key for least-recently-used eviction: the last prompt's RFC-3339 timestamp, or an
    /// empty string for a session that never ran one (sorting it first, i.e. oldest).
    pub(crate) fn updated_at_sort_key(&self) -> String {
        self.updated_at().unwrap_or_default()
    }

    /// The last-activity timestamp, if a prompt has run.
    fn updated_at(&self) -> Option<String> {
        self.updated_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Current mode id.
    pub fn mode(&self) -> String {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mode
            .clone()
    }

    /// Sets the current model.
    pub fn set_model(&mut self, model: impl Into<String>) {
        let model = model.into();
        // Resolve the new descriptor right away: the caller has just listed the models, so the
        // catalog is resident, and the host releases it as soon as every session is bound.
        let descriptor = SelectedDescriptor::resolve(&self.models, &model);
        *self
            .descriptor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = descriptor;
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .model = model;
        self.persist_config();
    }

    fn persist_config(&self) {
        let config = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        save_session_config(&self.config_path, &config);
    }

    /// Whether `model` is one of the advertised choices.
    pub fn has_model(&self, model: &str) -> bool {
        self.models.iter().any(|choice| choice.id == model)
    }

    /// Whether `level` is valid for the currently selected model.
    pub fn has_thought_level(&self, level: &str) -> bool {
        let model = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .model
            .clone();
        let ids = super::config::thought_level_ids_for(&model, &self.models);
        ids.iter().any(|id| id == level)
            || (ids.is_empty() && level == super::config::DEFAULT_THOUGHT_LEVEL)
    }

    /// Installs the resolver used for dynamic model switching: a closure that
    /// builds a live [`Model`](agentscope_core::model::Model) from a model id.
    pub fn set_model_builder(
        &mut self,
        builder: Arc<
            dyn Fn(
                    &str,
                    &str,
                )
                    -> std::result::Result<Arc<dyn agentscope_core::model::Model>, String>
                + Send
                + Sync,
        >,
    ) {
        self.model_builder = Some(builder);
    }

    /// Applies pending session config to the live agent. Config changes
    /// (thought level, model) take effect on the *next* prompt: the model is
    /// rebuilt from the resolved endpoint and swapped into the live agent while
    /// conversation state is preserved (mirrors Java `LocalAgent` rebuilding the
    /// agent per prompt with the new config).
    fn apply_session_config(&mut self) -> Vec<String> {
        // Thought level -> per-call generation defaults. `auto` sends nothing
        // (provider default); explicit levels send only `reasoning_effort`
        // (see `thought_level_params`).
        let level = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thought_level
            .clone();
        let mut opts = self.agent.generate_options_defaults().unwrap_or_default();
        opts.reasoning_effort = thought_level_params(&level).reasoning_effort;
        self.agent.set_generate_options_defaults(opts);

        // Dynamic model switching (next prompt). Failures are returned so the
        // caller can notify the client instead of failing silently.
        let mut errors = Vec::new();
        if let Some(build) = &self.model_builder {
            let id = self
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .model
                .clone();
            if !id.is_empty() && id != "default" && id != self.applied_model {
                match build(&id, self.session_id.to_string().as_str()) {
                    Ok(model) => {
                        self.agent.set_model(model);
                        self.applied_model = id.clone();
                        tracing::info!(session = %self.session_id, model = %id,
                            "switched model for session");
                    }
                    Err(e) => {
                        tracing::warn!(session = %self.session_id, model = %id, error = %e,
                            "failed to switch model; keeping current model");
                        errors.push(format!(
                            "⚠️ model switch to '{id}' failed: {e} (keeping current model)"
                        ));
                    }
                }
            }
        }
        errors
    }

    /// Answers an outstanding interaction and continues the turn without a new user prompt.
    ///
    /// A suspended tool (external execution) leaves the turn non-terminal. Without this path
    /// the only way out was `submit`, which necessarily starts a new prompt and discards the
    /// suspended turn. Answers are validated against the committed pending requests.
    pub async fn respond(
        &self,
        answers: std::collections::BTreeMap<String, agentscope_harness::agent::SessionAnswer>,
    ) -> agentscope_core::error::Result<agentscope_harness::agent::SessionTask> {
        let session = {
            let runtime = self.runtime.lock().await;
            match runtime.as_ref() {
                Some(session) => Arc::clone(session),
                None => {
                    return Err(agentscope_core::Error::Session(
                        "the session runtime is not open".into(),
                    ))
                }
            }
        };
        session.respond_all(answers).await
    }

    /// Current thought level.
    pub fn thought_level(&self) -> String {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thought_level
            .clone()
    }

    /// Sets the thought level (`auto` / `disabled` / `low` / `medium` / `high`
    /// / `max`). Applied on the next prompt.
    pub fn set_thought_level(&mut self, level: impl Into<String>) {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thought_level = level.into();
        self.persist_config();
    }
    /// Sets the current mode.
    pub fn set_mode(&mut self, mode: impl Into<String>) {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mode = mode.into();
        // Apply the mode's permission context to the live agent so the *next*
        // request already runs under the new rules.
        self.apply_mode();
        self.persist_config();
    }

    /// Reconfigures the underlying agent's permission engine to match the
    /// current session mode (`plan` / `accept_edits` / `yolo`), then layers the
    /// persisted "always allow / always reject" user rules on top so they
    /// survive mode switches and session restarts.
    fn apply_mode(&mut self) {
        let mode = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mode
            .clone();
        let ctx = config::permission_context_for_mode(&mode);
        self.agent.set_permission_context(ctx);
        let persisted = self.permission_store.load();
        let persisted_count = persisted.len();
        for rule in persisted {
            self.agent.add_permission_rule(rule);
        }
        tracing::debug!(
            session = %self.session_id,
            mode,
            persisted_rules = persisted_count,
            "applied permission context for ACP session mode"
        );
    }

    /// Binds the live client connection for this prompt/connection, so
    /// `Ask` tool calls can push `session/request_permission` to the user and
    /// `ask_user` can push an `elicitation/create` form.
    pub fn set_client(&mut self, client: AcpClientHandle) {
        let handle = Arc::new(client);
        // Publish the connection to the elicitation tool (which holds the same
        // slot across prompts, so it never needs rebuilding).
        let erased: Arc<dyn ElicitationClient> = handle.clone();
        *self
            .elicitation_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(erased);
        *self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
        // The connection is the second thing subagent frames need (after the harness channel),
        // and it exists from the first prompt on: that is the point the forwarder can start.
        self.start_subagent_forwarder();
    }

    /// Installs the harness channel carrying subagent frames.
    pub fn set_subagent_events(
        &mut self,
        events: tokio::sync::mpsc::UnboundedReceiver<agentscope_harness::tool::SubagentFrame>,
    ) {
        *self
            .subagent_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(events);
    }

    /// Shares the tree-wide child registry with the subagent permission bridge.
    pub fn set_subagent_agents(
        &mut self,
        agents: Arc<Mutex<HashMap<String, Arc<dyn agentscope_core::agent::CallableAgent>>>>,
    ) {
        self.subagent_agents = agents;
    }

    /// Shares the child → parent registry with the coding agent (which routes
    /// `session/cancel` addressed to a child).
    pub fn set_subagent_children(&mut self, children: Arc<Mutex<HashMap<String, String>>>) {
        self.subagent_children = children;
    }

    /// Uses the client slot the subagent permission bridge was built with.
    ///
    /// The bridge is installed on the agent before this session exists (it has to be, or the
    /// first spawn would already be running), so both must share one slot instead of each
    /// creating its own.
    pub fn set_client_slot(&mut self, slot: Arc<Mutex<Option<Arc<AcpClientHandle>>>>) {
        self.client = slot;
    }

    /// Uses the permission-rule store the subagent permission bridge was built with.
    ///
    /// Rules an "always allow/deny" answer adds from a child are persisted by the bridge; sharing
    /// the store keeps this session's in-memory context in step with them.
    pub fn set_permission_store(&mut self, store: Arc<PermissionRuleStore>) {
        self.permission_store = store;
        self.apply_mode();
    }

    /// Streams the harness's subagent frames to the client for as long as the session lives.
    ///
    /// Started once (the receiver is taken out of its slot): a subagent outlives the prompt that
    /// spawned it, so its frames cannot be drained by the prompt loop. Nothing is sent when the
    /// client did not advertise `subagents`; the frames are dropped instead.
    fn start_subagent_forwarder(&self) {
        let Some(mut events) = self
            .subagent_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        else {
            return;
        };
        let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let support = self.support.clone();
        let parent = self.session_id.clone();
        let children = Arc::clone(&self.subagent_children);
        tokio::spawn(async move {
            let mut tracker = event::SubagentTracker::new()
                .with_plan_operations(support.supports_plan_operations());
            while let Some(frame) = events.recv().await {
                let child = frame.child_session.clone();
                // A card belongs to the transcript of whoever spawned the child: for a nested
                // spawn that is the intermediate subagent, not this session.
                let owner = frame
                    .parent_session
                    .clone()
                    .map(SessionId::new)
                    .unwrap_or_else(|| parent.clone());
                for notification in event::subagent_notifications(
                    &support,
                    &owner,
                    &child,
                    &frame.event,
                    &mut tracker,
                ) {
                    if let Err(error) = client.notify(notification) {
                        // A dropped connection has nobody left to render subagents.
                        tracing::debug!(%error, "subagent update could not be delivered");
                        return;
                    }
                }
                // Keep the routing table in step with what the client was told: a child is
                // cancellable only while it runs, whichever session spawned it.
                match event::subagent_state(&frame.event) {
                    Some(event::SubagentState::Running) if support.supports_subagents() => {
                        children
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(child, parent.to_string());
                    }
                    Some(event::SubagentState::Finished) => {
                        children
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&child);
                    }
                    _ => {}
                }
            }
        });
    }

    /// Interrupts the subagent that owns `child` (a client cancel addressed to a child session).
    ///
    /// Returns whether a live subagent was found: a child that already finished has nothing to
    /// cancel, and the client's optimistic state is then corrected by the final
    /// `subagent_update`.
    pub fn cancel_subagent(&self, child: &SessionId) -> bool {
        let id = child.to_string();
        if self
            .agent
            .spawn_tool()
            .is_some_and(|tool| tool.cancel_subagent(&id))
        {
            return true;
        }
        // A nested child is owned by the intermediate subagent's spawn tool, which this session
        // cannot reach; the tree-wide registry the permission bridge fills is the way in.
        let agent = self
            .subagent_agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned();
        match agent {
            Some(agent) => {
                agent.interrupt();
                true
            }
            None => false,
        }
    }

    /// Interrupts every subagent this session spawned and forgot about them.
    ///
    /// Called on `session/close`: a closed session must not leave an orphaned child running (it
    /// would keep billing and writing into the workspace with nobody watching it).
    pub fn cancel_subagents(&self) {
        let parent = self.session_id.to_string();
        let children: Vec<String> = {
            let mut guard = self
                .subagent_children
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let owned: Vec<String> = guard
                .iter()
                .filter(|(_, owner)| owner.as_str() == parent)
                .map(|(child, _)| child.clone())
                .collect();
            for child in &owned {
                guard.remove(child);
            }
            owned
        };
        for child in children {
            self.cancel_subagent(&SessionId::new(child));
        }
    }

    /// The slot the `ask_user` tool reads the live connection from. Shared with
    /// the tool built at session-creation time.
    pub fn elicitation_slot(&self) -> ElicitationSlot {
        Arc::clone(&self.elicitation_slot)
    }

    /// Installs the slot the `ask_user` tool was built with. Called with the
    /// very slot handed to the session factory, so the tool and the session
    /// share one handle.
    pub fn set_elicitation_slot(&mut self, slot: ElicitationSlot) {
        self.elicitation_slot = slot;
    }

    /// Installs (or removes) the confirm handler that performs the ACP
    /// permission round-trip. Called at the start of every prompt run.
    fn install_confirm_handler(&self) {
        match self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            Some(client) => {
                let agent: Arc<dyn CallableAgent> = self.agent.clone();
                let handler = acp_confirm_handler(
                    self.session_id.clone(),
                    Arc::clone(&client),
                    agent,
                    Arc::clone(&self.permission_store),
                    None,
                );
                self.agent.set_confirm_handler(Some(handler));
            }
            None => {
                // No client handle (e.g. an embedded run that never connected
                // to an ACP peer): Ask decisions stay unsupported and error.
                self.agent.set_confirm_handler(None);
            }
        }
    }

    /// Connects the MCP servers declared by the ACP client (`mcp_servers` on
    /// new/load/resume session) and bridges their tools into the session's
    /// toolkit so the core agent can call them.
    ///
    /// Supports stdio (`McpServer::Stdio`), SSE (`McpServer::Sse`), HTTP
    /// (`McpServer::Http`) and native ACP (`McpServer::Acp`, MCP-over-ACP)
    /// transports. Re-declaring an already bridged server is a no-op.
    pub async fn apply_mcp_servers(
        &self,
        servers: &[agent_client_protocol::v1::schema::McpServer],
    ) {
        use agent_client_protocol::v1::schema::McpServer;
        for server in servers {
            // MCP-over-ACP is driven by a different transport (the ACP channel
            // itself) and needs `&self` interior mutability, so it is handled
            // before the config-based transports.
            if let McpServer::Acp(acp) = server {
                self.apply_acp_mcp_server(acp).await;
                continue;
            }
            let (name, config) = match server {
                McpServer::Stdio(stdio) => {
                    let name = stdio.name.clone();
                    // Build the stdio launch command: `command args...`.
                    let command = std::iter::once(stdio.command.display().to_string())
                        .chain(stdio.args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    (
                        name.clone(),
                        agentscope_harness::agent::McpServerConfig::new(name).command(command),
                    )
                }
                McpServer::Sse(sse) => {
                    let headers = sse
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect::<Vec<_>>();
                    (
                        sse.name.clone(),
                        agentscope_harness::agent::McpServerConfig::new(sse.name.clone())
                            .url(sse.url.clone())
                            .headers(headers),
                    )
                }
                McpServer::Http(http) => {
                    let headers = http
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect::<Vec<_>>();
                    (
                        http.name.clone(),
                        agentscope_harness::agent::McpServerConfig::new(http.name.clone())
                            .url(http.url.clone())
                            .headers(headers),
                    )
                }
                _other => {
                    tracing::warn!(
                        session = %self.session_id,
                        "MCP server transport not supported; skipping"
                    );
                    continue;
                }
            };
            // Dedup: skip servers already bridged this session. A name is only recorded
            // once the bridge succeeded, so a failed connect stays retryable instead of
            // leaving the server permanently unbridged.
            if self
                .bridged_servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&name)
            {
                continue;
            }
            let connected = tokio::time::timeout(MCP_CONNECT_TIMEOUT, async {
                let client_id = match self.mcp_manager.create_client(config).await {
                    Ok(client_id) => client_id,
                    Err(error) => {
                        tracing::warn!(
                            session = %self.session_id,
                            server = %name,
                            %error,
                            "failed to create MCP client"
                        );
                        return false;
                    }
                };
                if let Err(error) = self.mcp_manager.connect(&client_id).await {
                    tracing::warn!(
                        session = %self.session_id,
                        server = %name,
                        %error,
                        "failed to connect MCP server"
                    );
                    return false;
                }
                let toolkit = self.agent.toolkit();
                agentscope_harness::agent::bridge_mcp_tools_to_toolkit(&self.mcp_manager, toolkit)
                    .await;
                true
            })
            .await;
            match connected {
                Ok(true) => {
                    self.bridged_servers
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(name.clone());
                    tracing::info!(
                        session = %self.session_id,
                        server = %name,
                        "bridged MCP server tools into session toolkit"
                    );
                }
                Ok(false) => {}
                Err(_elapsed) => {
                    tracing::warn!(
                        session = %self.session_id,
                        server = %name,
                        timeout_secs = MCP_CONNECT_TIMEOUT.as_secs(),
                        "MCP server did not connect in time"
                    );
                }
            }
        }
    }

    /// Connects a client-declared MCP-over-ACP server and bridges its tools.
    async fn apply_acp_mcp_server(&self, acp: &agent_client_protocol::v1::schema::McpServerAcp) {
        let name = acp.name.clone();
        // Same rule as the config-based transports: record the name only after the bridge
        // succeeded, so a failed attempt can be retried when the client re-declares it.
        if self
            .bridged_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&name)
        {
            return;
        }
        let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            tracing::warn!(
                session = %self.session_id,
                server = %name,
                "MCP-over-ACP server declared but no client connection is bound"
            );
            return;
        };
        let connection = client.connect_mcp_acp(&name, acp.server_id.clone());
        // The stateless protocol has no handshake, so the first round-trip is tool
        // discovery; keep the same budget around it.
        match tokio::time::timeout(
            MCP_CONNECT_TIMEOUT,
            connection.bridge_tools(self.agent.toolkit()),
        )
        .await
        {
            Ok(Ok(_)) => {
                self.bridged_servers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(name.clone());
                tracing::info!(
                    session = %self.session_id,
                    server = %name,
                    "bridged MCP-over-ACP server tools into session toolkit"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    session = %self.session_id,
                    server = %name,
                    %e,
                    "failed to list MCP-over-ACP tools"
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    session = %self.session_id,
                    server = %name,
                    timeout_secs = MCP_CONNECT_TIMEOUT.as_secs(),
                    "MCP-over-ACP server did not answer in time"
                );
            }
        }
    }

    /// Returns the session mode state advertised to the client.
    pub fn session_mode_state(&self) -> SessionModeState {
        let modes: Vec<SessionMode> = self
            .modes
            .iter()
            .map(|m| SessionMode::new(m.id.clone(), m.name.clone()))
            .collect();
        SessionModeState::new(
            self.config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .mode
                .clone(),
            modes,
        )
    }

    /// Returns the session config options advertised to the client.
    pub fn config_options(&self) -> Vec<SessionConfigOption> {
        let mut cfg = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let valid_levels = super::config::thought_level_ids_for(&cfg.model, &self.models);
        if !valid_levels.iter().any(|id| id == &cfg.thought_level) {
            cfg.thought_level = super::config::DEFAULT_THOUGHT_LEVEL.to_string();
            save_session_config(&self.config_path, &cfg);
        }
        super::config::default_config_options_with_thought_grouped(
            &cfg.model,
            &self.models,
            &cfg.mode,
            &self.modes,
            &cfg.thought_level,
            self.supports_grouped_model_list(),
        )
    }

    /// Whether the connected client renders grouped select options.
    ///
    /// IntelliJ IDEA 2026 shows an empty dropdown for grouped model lists, so it
    /// gets the flat form; other clients keep provider grouping.
    fn supports_grouped_model_list(&self) -> bool {
        !self.support.is_intellij_2026()
    }

    /// Catalog descriptor for the currently selected model, when available.
    ///
    /// Cached: the per-turn path (prompt block validation, the catalog key) must not depend on the
    /// process-wide catalog table, which the host releases once every session has bound its model.
    /// A session that was built without the eager resolve resolves here once and remembers the
    /// answer, including "the catalog has nothing for this model".
    pub fn selected_descriptor(&self) -> Option<Arc<ModelDescriptor>> {
        let mut slot = self
            .descriptor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !slot.resolved {
            let model = self
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .model
                .clone();
            *slot = SelectedDescriptor::resolve(&self.models, &model);
        }
        slot.descriptor.clone()
    }

    /// Whether the descriptor question for the selected model has been answered.
    ///
    /// Cheap and side-effect free: the host uses it to decide whether the catalog table can be
    /// released without a later turn having to parse it again.
    pub fn has_cached_descriptor(&self) -> bool {
        self.descriptor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resolved
    }

    /// Catalog key for the currently selected model, when available.
    pub fn selected_catalog_key(&self) -> Option<String> {
        self.selected_descriptor()
            .map(|descriptor| descriptor.key.clone())
    }

    /// Pushes `ConfigOptionUpdate` + `UsageUpdate` to the bound client. Used by the
    /// background catalog task; a no-op when the session has no live connection.
    pub fn push_config_update(&self) {
        let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let options = self.config_options();
        let config = agent_client_protocol::v1::schema::SessionNotification::new(
            self.session_id.clone(),
            agent_client_protocol::v1::schema::SessionUpdate::ConfigOptionUpdate(
                agent_client_protocol::v1::schema::ConfigOptionUpdate::new(options),
            ),
        );
        let _ = client.notify(config);
        let used = self.last_input_tokens();
        let _ = client.notify(self.usage_notification(used));
    }

    /// Pushes a `UsageUpdate` sized for the *selected* model.
    ///
    /// `set_config_option` switches models lazily: the live agent keeps the old
    /// model until the next prompt, so [`usage_notification`](Self::usage_notification)
    /// would report the previous window. Size the gauge from the catalog
    /// descriptor of the newly selected model instead.
    pub fn push_usage_for_selected_model(&self) {
        use agent_client_protocol::v1::schema::{SessionNotification, SessionUpdate, UsageUpdate};
        let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let size = self
            .selected_descriptor()
            .map(|descriptor| descriptor.context_window as u64)
            .filter(|size| *size > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW as u64);
        let used = self.last_input_tokens();
        let update = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
        );
        let _ = client.notify(update);
    }

    /// Cancels any active prompt request.
    pub fn cancel(&self) {
        // Cancels exactly the turn that owns the session now: a prompt that starts later reads
        // the new generation and keeps running.
        self.cancel_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Closes the session: cancels the active prompt and prevents it from being re-registered.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cancel();
        // Subagents outlive the prompt that spawned them: closing the session must not leave
        // them running with nobody to render them (or to answer their permission requests).
        self.cancel_subagents();
        // Release the dispatcher unless a prompt currently holds the handle; dropping the
        // last clone stops it after that prompt unwinds.
        if let Ok(mut runtime) = self.runtime.try_lock() {
            runtime.take();
        }
    }

    /// Cancels the active prompt and waits for its run to settle, so the next prompt cannot
    /// start while the previous execution still owns the journal.
    pub async fn cancel_and_wait(&self, timeout: std::time::Duration) -> bool {
        self.cancel();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !self.is_running() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// Whether the session was closed by the client.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Whether a prompt is running right now.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Runs a prompt against the core agent and streams updates, honouring the
    /// SDK's per-request cancellation marker (`$/cancel_request`) in addition to
    /// the session-level [`cancel`](Self::cancel) flag.
    ///
    /// The `emit` closure is invoked for each ACP `SessionNotification` to send
    /// back to the client. Returns the `StopReason` for the turn.
    pub async fn run_prompt_cancellable<F>(
        &mut self,
        content: Vec<ContentBlock>,
        emit: F,
        cancellation: Option<agent_client_protocol::RequestCancellation>,
    ) -> StopReason
    where
        F: Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync,
    {
        let stop = self
            .run_prompt_inner(TurnInput::Prompt(content), emit, cancellation)
            .await;
        self.report_turn_memory(stop);
        stop
    }

    /// Runs a **wakeup turn**: no user input, only the context middlewares inject on a
    /// reasoning step (pending subagent results, inbox hints).
    ///
    /// A host uses this when background work finishes while the session is idle: the
    /// delivery itself is pull-based, so without a turn the result would sit unread until the
    /// user's next prompt. Notifications go to the connection bound by the last prompt.
    ///
    /// Returns `None` when the session has nothing to deliver (a turn that ended after
    /// reading the result) or no client connection is bound — starting an empty turn would
    /// only make the model re-answer the previous question.
    pub async fn run_wakeup(&mut self) -> Option<StopReason> {
        let ctx = self.session_context();
        if !self.agent.has_pending_background_work(&ctx).await {
            return None;
        }
        let client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        let emit = move |notification| {
            let _ = client.notify(notification);
        };
        let stop = self.run_prompt_inner(TurnInput::Wakeup, emit, None).await;
        self.report_turn_memory(stop);
        Some(stop)
    }

    /// Builds the `session/update` replay a client needs after `session/load`.
    ///
    /// ACP v1 `loadSession` requires the agent to replay the whole conversation: the client
    /// has no state for that session and renders whatever `session/update`s arrive before the
    /// response. This session advertised `load_session(true)` but replayed nothing, so an IDE
    /// that reconnected after a restart showed an empty transcript while the agent still
    /// answered with the full context.
    ///
    /// The recovery view (`HarnessAgent::inspect_session`) only carries the rebuilt
    /// `state_json`, which is state, not a message list, so it is used as the "is the
    /// committed journal readable" check; the replay order comes from
    /// `HarnessAgent::session_transcript`, which folds the same committed prefix into messages.
    /// Text becomes `user_message_chunk` / `agent_message_chunk` tagged with a per-message id,
    /// and a tool call becomes a completed `tool_call`, which is what a client needs to redraw
    /// the turn.
    ///
    /// An unreadable journal is not an error for `load`: the session may have been created by
    /// this very request for a cwd that has no history yet, so the replay is simply empty.
    pub async fn replay_notifications(
        &self,
    ) -> Vec<agent_client_protocol::v1::schema::SessionNotification> {
        use agent_client_protocol::v1::schema::{
            ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCall, ToolCallStatus,
        };
        use agentscope_core::message::{ContentBlock as CoreBlock, MsgRole};

        let ctx = self.session_context();
        if let Err(error) = self.agent.inspect_session(&ctx).await {
            tracing::warn!(
                session = %self.session_id,
                %error,
                "cannot inspect the session; session/load will replay nothing"
            );
            return Vec::new();
        }
        let transcript = match self.agent.session_transcript(&ctx).await {
            Ok(transcript) => transcript,
            Err(error) => {
                tracing::warn!(
                    session = %self.session_id,
                    %error,
                    "cannot read the transcript; session/load will replay nothing"
                );
                return Vec::new();
            }
        };

        let mut notifications = Vec::new();
        for message in transcript.messages {
            // One message id per historical message: clients group chunks by it exactly as
            // they do for a live turn, so a replayed turn renders as one bubble.
            let message_id = MessageId::new(format!("history-{}", message.id));
            for block in &message.content {
                match (message.role, block) {
                    (MsgRole::User, CoreBlock::Text(text)) => {
                        notifications.push(SessionNotification::new(
                            self.session_id.clone(),
                            SessionUpdate::UserMessageChunk(
                                ContentChunk::new(ContentBlock::Text(TextContent::new(
                                    text.text.clone(),
                                )))
                                .message_id(message_id.clone()),
                            ),
                        ))
                    }
                    (MsgRole::Assistant, CoreBlock::Text(text)) => {
                        notifications.push(SessionNotification::new(
                            self.session_id.clone(),
                            SessionUpdate::AgentMessageChunk(
                                ContentChunk::new(ContentBlock::Text(TextContent::new(
                                    text.text.clone(),
                                )))
                                .message_id(message_id.clone()),
                            ),
                        ))
                    }
                    (MsgRole::Assistant, CoreBlock::ToolUse(tool)) => {
                        notifications.push(SessionNotification::new(
                            self.session_id.clone(),
                            SessionUpdate::ToolCall(
                                ToolCall::new(tool.id.clone(), event::human_title(&tool.name))
                                    .name(tool.name.clone())
                                    .kind(event::tool_kind_for(&tool.name))
                                    // A committed tool call already ran (or its turn ended):
                                    // replaying it as pending would leave a spinner forever.
                                    .status(ToolCallStatus::Completed)
                                    .raw_input(serde_json::Value::Object(tool.input.clone())),
                            ),
                        ))
                    }
                    // Thinking and tool results are not replayed: the transcript view owns the
                    // message order, and the tool call above already carries its arguments.
                    _ => {}
                }
            }
        }
        notifications.extend(self.replay_subagent_cards().await);
        notifications
    }

    /// Replays the subagent cards of a loaded session.
    ///
    /// A client that reconnects has no memory of the subagents the conversation spawned, so the
    /// transcript would come back with the spawn tool calls and none of the cards they produced.
    /// The live channel died with the process that ran them; the durable task records are the
    /// only surviving source, and they carry everything a card needs (role, start, end, outcome).
    ///
    /// Only terminal tasks are replayed: a task the previous process left pending or running has
    /// no runner and no result, so reporting it as working or as failed would both be invented.
    /// The child id is derived from the task id, so reopening the session re-renders the same
    /// cards instead of accumulating new ones.
    async fn replay_subagent_cards(
        &self,
    ) -> Vec<agent_client_protocol::v1::schema::SessionNotification> {
        use agentscope_harness::agent::subagent::task::TaskStatus;

        let ctx = self.session_context();
        let tasks = self
            .agent
            .task_repository()
            .list_tasks(&ctx, &self.session_id.to_string(), None)
            .await;
        let mut notifications = Vec::new();
        for task in tasks {
            let status = task.get_task_status();
            if !status.is_terminal() {
                continue;
            }
            let agent_id = task.agent_id().to_string();
            let child = format!("sub-{}", task.task_id());
            let meta = event::SubagentMeta {
                started_at: Some(rfc3339_millis(task.created_at())),
                completed_at: Some(rfc3339_millis(task.last_checked_at())),
                // The model and the label a live spawn reports are not part of the record.
                model: None,
                role: Some(agent_id.clone()),
            };
            notifications.push(event::subagent_started(
                &self.session_id,
                &child,
                &agent_id,
                &format!("agent: {agent_id}"),
                Some(meta.clone()),
            ));
            let (outcome, error) = match status {
                TaskStatus::Completed => ("success", None),
                TaskStatus::Cancelled => ("cancelled", None),
                _ => ("error", task.get_error()),
            };
            notifications.push(event::subagent_idle(
                &self.session_id,
                &child,
                Some(event::stop_reason_for(outcome, error.as_deref())),
                Some(meta),
            ));
        }
        notifications
    }

    /// The bus this session's agent enqueues inbox hints and wakeup requests into.
    ///
    /// A host that wakes idle sessions drains [`WAKEUPS_KEY`] through it: a finished
    /// background task pushes one entry, and the entry is the trigger for a wakeup turn.
    ///
    /// [`WAKEUPS_KEY`]: agentscope_harness::agent::bus::WAKEUPS_KEY
    pub fn message_bus(&self) -> Option<&Arc<dyn MessageBus>> {
        self.agent.message_bus()
    }

    /// Drains the wakeup queue and returns the session ids that asked to be woken.
    ///
    /// The queue is per workspace, not per session, so the ids are resolved against the
    /// host's session registry by the caller. Each entry is consumed: one wakeup attempt per
    /// completion, like the harness `WakeupDispatcher`, so a failing turn is not retried in a
    /// loop.
    pub async fn drain_wakeup_requests(&self) -> Vec<String> {
        let Some(bus) = self.message_bus() else {
            return Vec::new();
        };
        let Ok(entries) = bus
            .queue_drain(agentscope_harness::agent::bus::WAKEUPS_KEY, 64)
            .await
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| entry.payload.get("sessionId")?.as_str())
            .map(str::to_string)
            .collect()
    }

    /// Runs a prompt against the core agent and streams updates.
    ///
    /// The `emit` closure is invoked for each ACP `SessionNotification` to send
    /// back to the client. Returns the `StopReason` for the turn.
    pub async fn run_prompt<F>(&mut self, content: Vec<ContentBlock>, emit: F) -> StopReason
    where
        F: Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync,
    {
        let stop = self
            .run_prompt_inner(TurnInput::Prompt(content), emit, None)
            .await;
        self.report_turn_memory(stop);
        stop
    }

    /// Per-turn memory line (`INFO`, always) plus the optional post-turn collect
    /// (`AGENTSCOPE_ACP_MEM_COLLECT`, on by default). A turn's megabyte-sized buffers are
    /// collectable once it is committed, so this is the point where returning them to the OS is
    /// meaningful. The expensive `state=` field only appears under `AGENTSCOPE_ACP_MEM_REPORT`.
    fn report_turn_memory(&self, stop: StopReason) {
        // Never block here: this runs on the async runtime, and the agent task may hold the state
        // lock across an await point -- a blocking `lock()` on a current-thread runtime would
        // deadlock the session. A contended lock just means the line omits the state fields.
        let state = self.agent.agent().map(|react| react.state_handle());
        let guard = state.as_ref().and_then(|handle| handle.try_lock().ok());
        let messages = guard.as_ref().map(|state| state.context.len());
        let state_bytes = if crate::mem::report_enabled() {
            guard
                .as_ref()
                .and_then(|state| state.to_json().ok())
                .map(|json| json.len())
        } else {
            None
        };
        drop(guard);
        crate::mem::note_turn(&format!("StopReason::{stop:?}"), messages, state_bytes);
        // Arm the idle trim: if the conversation stays quiet for `AGENTSCOPE_ACP_MEM_TRIM_IDLE`
        // seconds, the working set is dropped (a new turn cancels it via `note_activity`).
        crate::mem::schedule_idle_trim();
    }

    /// Converts ACP content blocks into a core user message, rejecting unsupported media
    /// before any model call is made.
    fn convert_prompt_blocks(
        content: Vec<ContentBlock>,
        descriptor: Option<&agentscope_core::model::catalog::ModelDescriptor>,
    ) -> std::result::Result<Msg, String> {
        use agentscope_core::message::{
            AudioBlock, Base64Source, DataBlock, ImageBlock, Source, TextBlock,
        };

        const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
        const MAX_IMAGES: usize = 8;
        const MAX_TOTAL_BYTES: usize = 30 * 1024 * 1024;

        let accepts_image = descriptor
            .map(|descriptor| descriptor.input_modalities.contains("image"))
            .unwrap_or(false);
        let accepts_audio = descriptor
            .map(|descriptor| descriptor.input_modalities.contains("audio"))
            .unwrap_or(false);

        let mut blocks: Vec<agentscope_core::message::ContentBlock> = Vec::new();
        let mut image_count = 0usize;
        let mut total_bytes = 0usize;

        for block in content {
            match block {
                ContentBlock::Text(text) => {
                    if !text.text.is_empty() {
                        blocks.push(agentscope_core::message::ContentBlock::Text(
                            TextBlock::new(text.text),
                        ));
                    }
                }
                ContentBlock::Image(image) => {
                    if !accepts_image {
                        return Err(
                            "the selected model does not accept image input; switch to a vision-capable model"
                                .to_string(),
                        );
                    }
                    let bytes = image.data.len();
                    if bytes > MAX_IMAGE_BYTES {
                        return Err(format!("image is too large ({bytes} bytes; limit {MAX_IMAGE_BYTES})"));
                    }
                    image_count += 1;
                    total_bytes += bytes;
                    if image_count > MAX_IMAGES {
                        return Err(format!("too many images (limit {MAX_IMAGES})"));
                    }
                    let mime = if image.mime_type.trim().is_empty() {
                        "image/png".to_string()
                    } else {
                        image.mime_type
                    };
                    blocks.push(agentscope_core::message::ContentBlock::Image(
                        ImageBlock::new(Source::Base64(Base64Source::new(mime, image.data))),
                    ));
                }
                ContentBlock::Audio(audio) => {
                    if !accepts_audio {
                        return Err(
                            "the selected model does not accept audio input".to_string(),
                        );
                    }
                    let bytes = audio.data.len();
                    total_bytes += bytes;
                    let mime = if audio.mime_type.trim().is_empty() {
                        "audio/mpeg".to_string()
                    } else {
                        audio.mime_type
                    };
                    blocks.push(agentscope_core::message::ContentBlock::Audio(
                        AudioBlock::new(Source::Base64(Base64Source::new(mime, audio.data))),
                    ));
                }
                ContentBlock::ResourceLink(link) => {
                    blocks.push(agentscope_core::message::ContentBlock::Text(
                        TextBlock::new(format!("[{}]({})", link.name, link.uri)),
                    ));
                }
                ContentBlock::Resource(resource) => match resource.resource {                    agent_client_protocol::v1::schema::EmbeddedResourceResource::TextResourceContents(text) => {
                        blocks.push(agentscope_core::message::ContentBlock::Text(
                            TextBlock::new(text.text),
                        ));
                    }
                    agent_client_protocol::v1::schema::EmbeddedResourceResource::BlobResourceContents(blob) => {
                        let mime = blob.mime_type.unwrap_or_default();
                        if mime.starts_with("image/") {
                            if !accepts_image {
                                return Err(
                                    "the selected model does not accept image input".to_string(),
                                );
                            }
                            blocks.push(agentscope_core::message::ContentBlock::Image(
                                ImageBlock::new(Source::Base64(Base64Source::new(
                                    if mime.trim().is_empty() {
                                        "image/png".to_string()
                                    } else {
                                        mime
                                    },
                                    blob.blob,
                                ))),
                            ));
                        } else if mime == "application/pdf" {
                            if !descriptor
                                .map(|descriptor| descriptor.attachment)
                                .unwrap_or(false)
                            {
                                return Err(
                                    "the selected model does not accept PDF attachments".to_string(),
                                );
                            }
                            blocks.push(agentscope_core::message::ContentBlock::Data(
                                DataBlock::new(Source::Base64(Base64Source::new(mime, blob.blob))),
                            ));
                        } else {
                            return Err(format!(
                                "unsupported embedded resource type '{mime}'"
                            ));
                        }
                    }
                    _ => {
                        return Err("unsupported embedded resource".to_string());
                    }
                },
                _ => {
                    return Err("unsupported prompt content block".to_string());
                }
            }
        }

        if total_bytes > MAX_TOTAL_BYTES {
            return Err(format!(
                "prompt attachments are too large ({total_bytes} bytes; limit {MAX_TOTAL_BYTES})"
            ));
        }
        if blocks.is_empty() {
            return Err("the prompt contains no supported content".to_string());
        }
        Ok(Msg::user_blocks(blocks))
    }

    /// Shared implementation for [`run_prompt`](Self::run_prompt),
    /// [`run_prompt_cancellable`](Self::run_prompt_cancellable) and
    /// [`run_wakeup`](Self::run_wakeup).
    async fn run_prompt_inner<F>(
        &mut self,
        input: TurnInput,
        emit: F,
        cancellation: Option<agent_client_protocol::RequestCancellation>,
    ) -> StopReason
    where
        F: Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync,
    {
        // The turn's own cancellation generation, captured *before* the admission wait.
        //
        // A cancel is addressed to the session, not to a single request: a client that pressed
        // Stop while its prompt was still queued behind the previous turn expects that prompt to
        // stop, not to start running once the lock is free. Capturing the generation after the
        // wait made such a cancel indistinguishable from "this turn started after the cancel"
        // and silently ignored it, so the prompt ran (and billed) anyway.
        crate::mem::note_activity();
        let cancel_epoch = Arc::clone(&self.cancel_epoch);
        let my_epoch = cancel_epoch.load(Ordering::SeqCst);
        let closed = Arc::clone(&self.closed);
        // One prompt turn per session. A client may send a follow-up before the previous
        // response arrives (a retry, a Stop followed by a new message); without this lock the
        // second run raced the first and was rejected as `turn_unfinished`, losing the input.
        //
        // The wait is cancellable and bounded: a stuck previous turn must not make every later
        // prompt hang forever behind an un-cancellable lock.
        let _prompt_guard = tokio::select! {
            guard = Arc::clone(&self.prompt_lock).lock_owned() => guard,
            _ = async {
                if let Some(c) = &cancellation {
                    c.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                return StopReason::Cancelled;
            }
            _ = cancel_epoch_moved(&cancel_epoch, my_epoch) => {
                return StopReason::Cancelled;
            }
            _ = async {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    if closed.load(Ordering::SeqCst) {
                        return;
                    }
                }
            } => {
                return StopReason::Cancelled;
            }
        };
        if cancel_epoch.load(Ordering::SeqCst) != my_epoch {
            // Cancelled while waiting for the previous turn (and the lock took priority in the
            // select above): the queued prompt must not run.
            return StopReason::Cancelled;
        }
        if closed.load(Ordering::SeqCst) {
            // Closed while this prompt waited for the previous turn to finish.
            return StopReason::Cancelled;
        }
        self.running.store(true, Ordering::SeqCst);
        let _running = RunningPrompt(Arc::clone(&self.running));
        // Turn-level usage: reset at the start of each prompt turn, accumulate
        // from every model call, and report back as `PromptResponse.usage`.
        self.turn_usage = ChatUsage::default();

        // Bind the confirm handler to the current client connection so tool
        // calls that need approval push an ACP request_permission this turn.
        self.install_confirm_handler();

        // Dynamic model switching + thought level: config changes (setModel /
        // setConfigOption thought_level) take effect on the *next* prompt. The
        // model is rebuilt from the resolved endpoint and swapped into the live
        // agent while conversation state is preserved. Switch failures are
        // surfaced to the client as an advisory notice (or a message chunk when
        // the client did not advertise `session.notices`).
        for err in self.apply_session_config() {
            emit(self.advisory_notification(err));
        }
        // Record last-activity time and tell the client so its session list
        // shows the current `updatedAt` without a separate `session/list` call.
        self.updated_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(chrono::Utc::now().to_rfc3339());
        emit(self.session_info_update());
        // Convert ACP content blocks into a core user message; unsupported media is
        // rejected before any model call so the client gets a readable reason. A wakeup turn
        // carries no user message at all — the middlewares inject the pending context.
        let descriptor = self.selected_descriptor();
        let message = match input {
            TurnInput::Prompt(content) => {
                match Self::convert_prompt_blocks(content, descriptor.as_deref()) {
                    Ok(message) => Some(message),
                    Err(error) => {
                        emit(super::event::message_chunk(
                            self.session_id.clone(),
                            format!("Unsupported prompt content: {error}"),
                            None,
                        ));
                        return StopReason::Refusal;
                    }
                }
            }
            TurnInput::Wakeup => None,
        };
        // Run the turn through the session runtime instead of driving `stream_events`
        // directly: the runtime owns the journal writer, restores the committed state from
        // the journal before the run, and admits steering/interrupts. This client only
        // renders — events arrive through the sink below.
        //
        // Opening and submitting touch durable storage, which can fail transiently (a
        // competing writer, an fsync/network hiccup). Those failures are not the user's
        // fault and must not end the turn on the first attempt; the submit keeps a stable
        // idempotency key so a retry cannot duplicate the prompt.
        //
        // The handle is cached for the lifetime of the ACP session: its dispatcher keeps the
        // journal projection and receipt fold warm, so later prompts do not re-read the whole
        // history (and every checkpoint) before they can start.
        let session = {
            let mut runtime = self.runtime.lock().await;
            match runtime.as_ref() {
                Some(session) => Arc::clone(session),
                None => {
                    let mut open_attempts = 0usize;
                    let opened = loop {
                        open_attempts += 1;
                        match self.agent.session(self.session_context()).await {
                            Ok(session) => break session,
                            Err(error) if open_attempts < 3 => {
                                tracing::warn!(
                                    session = %self.session_id,
                                    %error,
                                    "opening the session runtime failed; retrying"
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            }
                            Err(error) => {
                                emit(super::event::message_chunk(
                                    self.session_id.clone(),
                                    format!("Agent error: {error}"),
                                    None,
                                ));
                                return StopReason::Refusal;
                            }
                        }
                    };
                    let opened = Arc::new(opened);
                    *runtime = Some(Arc::clone(&opened));
                    opened
                }
            }
        };
        // Bounded backlog: the sink runs inside the agent and cannot block, so a stalled
        // client would grow an unbounded channel without limit. Overflow is dropped and
        // counted (`AGENT_RESULT` is always kept), and the end of the turn reports it.
        let events = crate::event_queue::EventQueue::new(crate::event_queue::EVENT_QUEUE_CAPACITY);
        let sink = crate::event_queue::EventQueueSender::new(Arc::clone(&events));
        session.set_event_sink(Arc::new(move |event: &AgentEvent| {
            // A closed queue only means the prompt already returned; the run itself is
            // unaffected, so dropping the event here is safe.
            sink.push(event.clone());
        }));
        // A previous turn may still be open (the process died mid-run, the provider stalled,
        // the host stopped waiting). `submit` is now the authority on how to settle it: it
        // continues a resumable interrupted turn and closes a suspended/unrecoverable one, so
        // the prompt is never rejected with "the previous turn is still open". All this path
        // still owes the client is stopping a run this process already owns, so the new
        // prompt is not queued behind a stalled one.
        if let Err(error) = session.stop_active_run().await {
            tracing::warn!(
                session = %self.session_id,
                %error,
                "could not stop the active run before starting a new prompt"
            );
        }
        // The key identifies this prompt in the journal: it is derived from the user's text
        // plus the acceptance time, so a retry of this attempt reuses it while an identical
        // question asked later is still a new turn. A wakeup turn has no text to hash.
        let idempotency_key = message
            .as_ref()
            .map(|message| prompt_idempotency_key(message, chrono::Utc::now().timestamp_millis()));
        let mut submit_attempts = 0usize;
        let mut task = loop {
            submit_attempts += 1;
            let submitted = match (&message, &idempotency_key) {
                (Some(message), Some(key)) => {
                    session.submit_with_key_msg(key, message.clone()).await
                }
                _ => session.wakeup().await,
            };
            match submitted {
                Ok(task) => break task,
                Err(error) if submit_attempts < 3 => {
                    tracing::warn!(
                        session = %self.session_id,
                        %error,
                        "task submission failed; retrying with the same idempotency key"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(error) => {
                    emit(super::event::message_chunk(
                        self.session_id.clone(),
                        format!("Agent error: {error}"),
                        None,
                    ));
                    return StopReason::Refusal;
                }
            }
        };

        use agentscope_core::event::AgentEventType;
        // One stable message id per turn: every text/thinking chunk and tool call
        // emitted this turn shares it, so ACP clients group them into one
        // assistant message and render tool calls / thoughts correctly.
        let message_id = MessageId::new(format!(
            "turn-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let message_id_opt: Option<MessageId> = Some(message_id);
        let mut tool_events =
            super::event::ToolEventMapper::new().with_message_id(message_id_opt.clone());
        // Clients that advertised plan operations get the ID-addressed
        // `plan_update` form for `todo_write`-derived plans.
        tool_events = tool_events.with_plan_operations(self.support.supports_plan_operations());

        // Push the initial context-window usage so clients render the gauge
        // before the first model call completes. Before any model call there is no
        // real in-context count, so report zero; afterwards keep showing the last
        // provider-reported input size until the new call reports its own.
        emit(self.usage_notification(self.last_input_tokens()));

        // Watchdog state.
        //
        // `queued` is the only status that means "nothing picked this up yet". It is a
        // legitimate wait while a *previous* process's writer lease is still valid (the lease
        // is 30s), so the deadline must be comfortably longer than that or a normal restart
        // looks like a hang.
        const ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
        // How long receipt reads may keep failing before the prompt reports a stall.
        //
        // This is a *duration*, not a tick count: the previous "40 ignored polls" meant two
        // seconds at a 50ms tick, so one slow fsync or a briefly contended journal aborted a
        // perfectly healthy run. A transient storage hiccup must stay invisible; only a
        // sustained inability to read the run's state is a real stall.
        const POLL_FAILURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        // How long a *closed* session may spend closing its abandoned turn.
        //
        // The journal close needs the writer lease, which a run that never returns (a hung
        // provider) keeps renewing, so an unbounded wait turns `session/close` into exactly the
        // hang it is supposed to end. Nobody is left to observe a half-finished close there.
        const CLOSED_TURN_CLOSE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
        // A run stopped by something other than this client's cancel is resumed in place.
        // Bounded so a permanently interrupted environment is reported instead of retried
        // forever; an interruption is not a failure and must not discard the user's prompt.
        const MAX_INTERRUPTED_RESUMES: usize = 2;

        let mut resumes = 0usize;

        loop {
            // ---- one dispatched attempt: stream until it settles or is cancelled ----
            let mut settled_task: Option<agentscope_harness::agent::session::Task> = None;
            let mut cancelled = false;
            // Whether this attempt already streamed incremental text. Reset per attempt: a
            // resumed run may complete without emitting deltas (the answer already exists in
            // the restored state), and it must still reach the client. Keeping the flag from
            // the interrupted attempt would silently drop the final reply.
            let mut emitted_text = false;
            // Set when the event sink is gone. The run may still be going, so the receipt
            // keeps being polled instead of returning with the turn still open.
            let mut events_closed = false;
            let mut admission_deadline = std::time::Instant::now() + ADMISSION_TIMEOUT;
            let mut last_status = String::new();
            let mut first_poll_failure: Option<std::time::Instant> = None;
            let mut stall: Option<String> = None;
            // A persistent ticker, not a fresh `sleep` per iteration: re-creating the timer every
            // loop restart meant a busy event stream could starve it indefinitely (a stalled
            // command was only noticed minutes later).
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if cancel_epoch.load(Ordering::SeqCst) != my_epoch {
                            cancelled = true;
                            break;
                        }
                        if settled_task.is_none() {
                            match session.task(&task.command_id).await {
                                Ok(current) => {
                                    first_poll_failure = None;
                                    if current.status != last_status {
                                        // Any status change is progress: restart the admission
                                        // clock so an earlier `queued` wait cannot accumulate.
                                        last_status = current.status.clone();
                                        admission_deadline =
                                            std::time::Instant::now() + ADMISSION_TIMEOUT;
                                    }
                                    if current.settled() {
                                        settled_task = Some(current);
                                    } else if current.status == "queued"
                                        && std::time::Instant::now() >= admission_deadline
                                    {
                                        // Still unclaimed after the deadline: no dispatcher will
                                        // ever run it, so report instead of hanging.
                                        stall = Some(format!(
                                            "the turn was never admitted (status `queued` after {}s)",
                                            ADMISSION_TIMEOUT.as_secs()
                                        ));
                                        break;
                                    }
                                }
                                Err(error) => {
                                    // A single failed read means nothing: the journal is shared
                                    // with other processes and a transient IO error must not kill
                                    // a live run. Only sustained failure is reported.
                                    let since = *first_poll_failure
                                        .get_or_insert_with(std::time::Instant::now);
                                    if since.elapsed() >= POLL_FAILURE_TIMEOUT {
                                        stall = Some(format!(
                                            "the session stopped reporting task state: {error}"
                                        ));
                                        break;
                                    }
                                }
                            }
                        }
                        if settled_task.is_some() && (events_closed || events.is_empty()) {
                            break;
                        }
                    }
                    // The SDK sets this marker when the peer sends `$/cancel_request`
                    // for the prompt's JSON-RPC request id.
                    _ = async {
                        if let Some(c) = &cancellation {
                            c.cancelled().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {
                        cancelled = true;
                        break;
                    }
                    // The session was torn down while the run was in flight (`session/close`,
                    // LRU eviction, host shutdown). Closing also bumps the cancel generation,
                    // but the closed flag is the authoritative teardown signal: it must not
                    // depend on that bump being observed here, or a host that closes a session
                    // whose provider never answers could not stop the turn at all.
                    _ = async {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            if closed.load(Ordering::SeqCst) {
                                return;
                            }
                        }
                    } => {
                        cancelled = true;
                        break;
                    }
                    event = events.recv(), if !events_closed => {
                        match event {
                            Some(ev) => match ev.event_type() {
                                AgentEventType::TextBlockDelta => {
                                    if let Some(notif) = super::event::map_delta_event(
                                        self.session_id.clone(),
                                        &ev,
                                        message_id_opt.clone(),
                                    ) {
                                        emitted_text = true;
                                        emit(notif);
                                    }
                                }
                                AgentEventType::AgentResult => {
                                    // An interrupted turn still produces a (reconciled) result. Tell
                                    // the user that it was interrupted, but do not render the
                                    // runtime's recovery text ("I noticed that you have interrupted
                                    // me..."): it is an internal control message, and the prompt loop
                                    // below resumes the turn automatically — showing both would claim
                                    // the turn ended immediately before continuing it.
                                    let interrupted = super::event::result_generate_reason(&ev)
                                        == Some(agentscope_core::message::GenerateReason::Interrupted);
                                    if interrupted {
                                        emit(super::event::message_chunk(
                                            self.session_id.clone(),
                                            "\n\n[turn interrupted before completion]".to_string(),
                                            message_id_opt.clone(),
                                        ));
                                    } else if !emitted_text {
                                        // Emit the final reply only if it wasn't already
                                        // streamed incrementally (avoids duplicate output).
                                        if let Some(notif) = super::event::map_result_event(
                                            self.session_id.clone(),
                                            &ev,
                                            message_id_opt.clone(),
                                        ) {
                                            emit(notif);
                                        }
                                    }
                                }
                                AgentEventType::ModelCallEnd => {
                                    // Accumulate usage and push a context-window
                                    // UsageUpdate after every model call. `used`
                                    // reflects the tokens in the current context
                                    // (latest call's input tokens), NOT the
                                    // cumulative session count.
                                    if let Some(u) = ev
                                        .data()
                                        .fields
                                        .get("usage")
                                        .and_then(|v| serde_json::from_value::<ChatUsage>(v.clone()).ok())
                                    {
                                        self.accumulate_usage(u.clone());
                                        self.accumulate_turn_usage(&u);
                                        self.last_input_tokens
                                            .store(u.input_tokens, Ordering::SeqCst);
                                        emit(self.usage_notification(u.input_tokens));
                                    } else {
                                        // No usage on this event: keep showing the last
                                        // provider-reported context size instead of 0.
                                        emit(self.usage_notification(self.last_input_tokens()));
                                    }
                                }
                                AgentEventType::Custom => {
                                    // The compaction middleware announces a completed
                                    // summarization in-band; translate it into the
                                    // capability-gated `compaction_update` only when
                                    // the client advertised `session.compaction`.
                                    if let Some((_, summary)) =
                                        super::event::compaction_marker(&ev)
                                    {
                                        if self.support.supports_compaction() {
                                            let compaction_id = format!(
                                                "compaction-{}",
                                                std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .map(|d| d.as_nanos())
                                                    .unwrap_or(0)
                                            );
                                            for notif in super::event::compaction_notifications(
                                                self.session_id.clone(),
                                                &compaction_id,
                                                &summary,
                                            ) {
                                                emit(notif);
                                            }
                                        }
                                    }
                                }
                                _ => {
                                    if let Some(notif) = super::event::map_delta_event(
                                        self.session_id.clone(),
                                        &ev,
                                        message_id_opt.clone(),
                                    ) {
                                        emit(notif);
                                    }
                                    for notif in
                                        tool_events.map(self.session_id.clone(), &ev)
                                    {
                                        emit(notif);
                                    }
                                }
                            },
                            // The sink was dropped (the handle was closed) — no further events
                            // can arrive. Keep polling the receipt: returning here would report
                            // `end_turn` while the run is still writing its turn.
                            None => events_closed = true,
                        }
                    }
                }
            }

            if cancelled {
                // The ACP cancel flag is the only client-facing cancellation, so stop the run
                // and close its turn: an interrupted turn stays open in the journal (only
                // `completed`/`cancelled` end one), and the next prompt must be able to submit.
                //
                // The signal is scoped to *this* turn's run id. The loop below keeps retrying
                // for up to 10s, and the client may already have sent its next prompt by then;
                // an unscoped `agent.interrupt()` would kill that fresh run — the "turn ended as
                // interrupted" report on a prompt the user just sent.
                //
                // The run id is not known at submit time (the dispatcher publishes it when it
                // admits the command), so it is re-read every iteration and signalled as soon as
                // it appears.
                let mut signalled: Option<String> = task.run_id.clone();
                if let Some(run_id) = signalled.as_deref() {
                    self.agent.interrupt_run(run_id);
                }

                // A closed session has nobody left to answer (`session/close`, LRU eviction,
                // host shutdown): close the turn now instead of waiting out the settle grace for
                // a run that may never return (a hung provider). Waiting made teardown take the
                // full 10s and left the client's `session/close` hanging. A live client keeps the
                // grace below so the next prompt is not refused with "turn_unfinished" while the
                // run is still winding down.
                if !closed.load(Ordering::SeqCst) {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while std::time::Instant::now() < deadline {
                        // Teardown while the run winds down: stop waiting, the session is gone.
                        if closed.load(Ordering::SeqCst) {
                            break;
                        }
                        let current = session.task(&task.command_id).await;
                        let current = match current {
                            Ok(current) => current,
                            Err(_) => {
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                continue;
                            }
                        };
                        if current.settled() {
                            break;
                        }
                        // Adopt the run id the moment the dispatcher publishes it, then keep the
                        // signal aimed at that exact run.
                        if signalled.is_none() {
                            if let Some(run_id) = current.run_id.clone() {
                                self.agent.interrupt_run(&run_id);
                                signalled = Some(run_id);
                            }
                        }
                        if let Some(run_id) = signalled.as_deref() {
                            let _ = session.interrupt_run(run_id).await;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }

                // Close the turn before returning. `cancel_pending` retries the writer lease
                // and settles pending interactions; `abandon_turn` additionally records unknown
                // tool outcomes as interrupted. Both are needed: the run may have been
                // interrupted mid-tool, and a turn left open would block the next prompt. Never
                // fire-and-forget this — a silently failed close is exactly the "stuck session"
                // symptom.
                //
                // A *closed* session is the one exception: no client is left and no later prompt
                // can be rejected, while the still-live run holds the journal writer lease. There
                // the close is attempted with a short bound instead of waiting out the lease, so
                // `session/close` cannot hang on the very turn it is tearing down.
                let closed_now = closed.load(Ordering::SeqCst);
                let close = agentscope_core::session::abandon_turn(session.log(), &task.turn_id);
                let close = async {
                    if closed_now {
                        tokio::time::timeout(CLOSED_TURN_CLOSE_GRACE, close)
                            .await
                            .unwrap_or_else(|_| Ok(false))
                    } else {
                        close.await
                    }
                };
                if let Err(error) = close.await {
                    tracing::warn!(
                        session = %self.session_id,
                        turn = %task.turn_id,
                        %error,
                        "failed to close the cancelled turn; the next prompt may be rejected"
                    );
                }
                return StopReason::Cancelled;
            }

            // A watchdog fired: the command was never admitted, or the session stopped
            // reporting. Report it instead of ending the turn silently (which looks like a hang
            // to the client).
            if let Some(reason) = stall {
                // Try to release whatever the abandoned attempt left open so the next prompt can
                // start; failures here must not hide the original problem.
                let _ = agentscope_core::session::abandon_turn(session.log(), &task.turn_id).await;
                emit(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: {reason}"),
                    message_id_opt.clone(),
                ));
                return StopReason::Refusal;
            }

            // The inner loop can only exit without a receipt on cancel/stall (both handled
            // above). Never report success for a run whose outcome is unknown.
            let Some(current) = settled_task else {
                let _ = agentscope_core::session::abandon_turn(session.log(), &task.turn_id).await;
                emit(super::event::message_chunk(
                    self.session_id.clone(),
                    "Agent error: the run stopped without reporting a result; the turn was \
                     closed, send a new prompt to continue"
                        .to_string(),
                    message_id_opt.clone(),
                ));
                return StopReason::Refusal;
            };

            match current.status.as_str() {
                "failed" => {
                    // Prefer the error persisted with `run/end`; older journals (and a run
                    // that settled between the last poll and the terminal write) may only
                    // have the in-memory dispatch error.
                    let detail = current
                        .error
                        .clone()
                        .or_else(|| session.execution_error())
                        .unwrap_or_else(|| "run failed".to_string());
                    // Core errors may already carry the `Agent error:` prefix (possibly more
                    // than once through wrapping); keep exactly one.
                    let mut detail = detail.as_str();
                    while let Some(rest) = detail.strip_prefix("Agent error: ") {
                        detail = rest;
                    }
                    emit(super::event::message_chunk(
                        self.session_id.clone(),
                        format!("Agent error: {detail}"),
                        message_id_opt.clone(),
                    ));
                    return StopReason::Refusal;
                }
                // The dispatcher refused the command (e.g. an earlier turn is still open).
                // Ending the turn silently would leave the client believing the prompt ran.
                "rejected" => {
                    let detail = current
                        .error
                        .clone()
                        .unwrap_or_else(|| "the turn was rejected by the session runtime".into());
                    let detail = if detail == "turn_unfinished" {
                        "The previous turn is still open, so this prompt could not start. Wait \
                         for it to finish or cancel it, then send the prompt again."
                            .to_string()
                    } else {
                        detail
                    };
                    emit(super::event::message_chunk(
                        self.session_id.clone(),
                        format!("Agent error: {detail}"),
                        message_id_opt.clone(),
                    ));
                    return StopReason::Refusal;
                }
                // The run stopped without a normal result. Interruption is the one case this
                // client did not ask for: the turn is still open and resumable, so continue it
                // instead of throwing the user's prompt away. Hosts interrupt runs for
                // transient reasons (shutdown sweeps, a raced cancel, a recovered process) and
                // none of those are the user's problem.
                "interrupted" if resumes < MAX_INTERRUPTED_RESUMES => {
                    resumes += 1;
                    emit(super::event::message_chunk(
                        self.session_id.clone(),
                        "\n\n[run interrupted; continuing]\n\n".to_string(),
                        message_id_opt.clone(),
                    ));
                    match session.resume(&task.turn_id).await {
                        Ok(next) => {
                            task = next;
                            continue;
                        }
                        Err(error) => {
                            let _ = agentscope_core::session::abandon_turn(
                                session.log(),
                                &task.turn_id,
                            )
                            .await;
                            emit(super::event::message_chunk(
                                self.session_id.clone(),
                                format!(
                                    "Agent error: the interrupted run could not be continued \
                                     automatically: {error}. The turn was closed; send a new \
                                     prompt to continue."
                                ),
                                message_id_opt.clone(),
                            ));
                            return StopReason::EndTurn;
                        }
                    }
                }
                "interrupted" => {
                    // Every resume was interrupted again. The environment, not the prompt, is
                    // failing; close the turn so the session stays usable and say so without
                    // refusing the turn at the protocol level.
                    let _ =
                        agentscope_core::session::abandon_turn(session.log(), &task.turn_id).await;
                    emit(super::event::message_chunk(
                        self.session_id.clone(),
                        "Agent error: the run kept being interrupted before it finished. The \
                         turn was closed; send a new prompt to continue."
                            .to_string(),
                        message_id_opt.clone(),
                    ));
                    return StopReason::EndTurn;
                }
                // `suspended` (an external tool result never arrived) and any other non-terminal
                // outcome. Report it and close the turn so the next prompt is not blocked.
                status if status != "completed" => {
                    let detail = current.error.clone().unwrap_or_else(|| {
                        format!(
                            "the run was {status} before it finished; the turn has been closed, \
                             send a new prompt to continue"
                        )
                    });
                    let _ =
                        agentscope_core::session::abandon_turn(session.log(), &task.turn_id).await;
                    emit(super::event::message_chunk(
                        self.session_id.clone(),
                        format!("Agent error: {detail}"),
                        message_id_opt.clone(),
                    ));
                    return StopReason::Refusal;
                }
                _ => {
                    // The client may have been too slow to keep up, in which case part of the
                    // streamed answer never reached it. Say so instead of leaving the output
                    // looking silently truncated.
                    let dropped_deltas = events.dropped_deltas();
                    if dropped_deltas > 0 {
                        tracing::warn!(
                            session = %self.session_id,
                            dropped = events.dropped(),
                            dropped_deltas,
                            "the client could not keep up; some streamed output was dropped"
                        );
                        emit(super::event::message_chunk(
                            self.session_id.clone(),
                            "\n\n[some streamed output was dropped: the client could not keep \
                             up]"
                            .to_string(),
                            message_id_opt.clone(),
                        ));
                    }
                    return StopReason::EndTurn;
                }
            }
        }
    }

    /// The provider-reported input tokens of the most recent model call.
    fn last_input_tokens(&self) -> u64 {
        self.last_input_tokens.load(Ordering::SeqCst)
    }

    /// Cumulative token usage for this session.
    pub fn usage(&self) -> &ChatUsage {
        &self.usage
    }

    /// Accumulates usage from a turn.
    pub fn accumulate_usage(&mut self, usage: ChatUsage) {
        self.usage.input_tokens += usage.input_tokens;
        self.usage.output_tokens += usage.output_tokens;
    }

    /// Accumulates usage for the currently running turn (reported back as
    /// `PromptResponse.usage`).
    fn accumulate_turn_usage(&mut self, usage: &ChatUsage) {
        self.turn_usage.input_tokens += usage.input_tokens;
        self.turn_usage.output_tokens += usage.output_tokens;
        self.turn_usage.cached_tokens += usage.cached_tokens;
    }

    /// Builds the ACP `Usage` for the running turn, or `None` when no model
    /// call contributed usage (so an empty turn does not report a bogus 0).
    pub fn turn_usage_response(&self) -> Option<agent_client_protocol::v1::schema::Usage> {
        turn_usage_to_response(&self.turn_usage)
    }

    /// Builds the ACP `UsageUpdate` notification: `used` is the tokens in
    /// context (latest model-call input tokens, falling back to the cumulative
    /// count), `size` is the live model's context window.
    pub(crate) fn usage_notification(
        &self,
        used: u64,
    ) -> agent_client_protocol::v1::schema::SessionNotification {
        use agent_client_protocol::v1::schema::{SessionNotification, SessionUpdate, UsageUpdate};
        let size = self
            .agent
            .agent()
            .map(|a| a.model().context_window_size() as u64)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW as u64);
        SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        prompt_idempotency_key, save_session_config, thought_level_params, turn_usage_to_response,
        SelectedDescriptor,
    };
    use agentscope_core::message::{ChatUsage, Msg};

    #[test]
    fn saving_a_session_config_replaces_it_atomically() {
        // Regression: the config was written in place, so a crash mid-write left a truncated
        // file the next start could not parse (and the session silently lost its model/mode).
        let directory = std::env::temp_dir().join(format!(
            "acp-config-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("session.json");
        let first = super::SessionConfig {
            model: "first".into(),
            ..super::SessionConfig::default()
        };
        save_session_config(&path, &first);
        let restored: super::SessionConfig =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored.model, "first");

        // Overwriting leaves a complete file and no temporary sibling behind.
        let second = super::SessionConfig {
            model: "second".into(),
            ..super::SessionConfig::default()
        };
        save_session_config(&path, &second);
        let restored: super::SessionConfig =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored.model, "second");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temporary file must be renamed away"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_model_outside_the_catalog_still_counts_as_resolved() {
        // Regression risk: `descriptor.is_some()` was used as "resolved", so a model the catalog
        // does not know would be looked up on every turn -- re-parsing the whole 15 MB table once
        // the host had released it.
        let unresolved = SelectedDescriptor::default();
        assert!(!unresolved.resolved);
        assert!(unresolved.descriptor.is_none());

        let resolved = SelectedDescriptor::resolve(&[], "not-in-any-catalog");
        assert!(resolved.resolved);
        assert!(resolved.descriptor.is_none());
    }

    #[test]
    fn the_prompt_key_hashes_the_text_and_the_acceptance_time() {
        let message = Msg::user("fix the build");
        let key = prompt_idempotency_key(&message, 1_700_000_000_000);
        assert!(key.starts_with("prompt-"), "{key}");
        assert!(key.ends_with("-1700000000000"), "{key}");
        // Same text at the same instant is the same turn (a retry of this attempt).
        assert_eq!(key, prompt_idempotency_key(&message, 1_700_000_000_000));
        // Same text later is a new turn: a genuine repeat is never merged away.
        assert_ne!(key, prompt_idempotency_key(&message, 1_700_000_000_001));
        // Different text is a different turn.
        assert_ne!(
            key,
            prompt_idempotency_key(&Msg::user("fix the tests"), 1_700_000_000_000)
        );
        // Short enough for the harness limit (200 bytes) and URL-safe.
        assert!(key.len() < 64, "{key}");
        assert!(key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
    }

    #[test]
    fn a_prompt_without_text_still_gets_a_key() {
        let key = prompt_idempotency_key(&Msg::user(""), 42);
        assert!(key.starts_with("prompt-"), "{key}");
        assert!(key.ends_with("-42"), "{key}");
    }

    #[test]
    fn an_empty_turn_reports_no_usage() {
        assert!(turn_usage_to_response(&ChatUsage::default()).is_none());
    }

    #[test]
    fn turn_usage_maps_to_the_protocol_shape() {
        let usage = ChatUsage::builder()
            .input_tokens(100)
            .output_tokens(50)
            .cached_tokens(20)
            .build();
        let response = turn_usage_to_response(&usage).expect("usage reported");
        assert_eq!(response.total_tokens, 150);
        assert_eq!(response.input_tokens, 100);
        assert_eq!(response.output_tokens, 50);
        assert_eq!(response.cached_read_tokens, Some(20));
    }

    #[test]
    fn auto_sends_nothing() {
        let opts = thought_level_params("auto");
        assert!(opts.reasoning_effort.is_none());
        assert!(opts.additional_body_params.is_empty());
    }

    /// Regression: a strict OpenAI-compatible gateway rejects vendor-specific
    /// body params with `json: unknown field "thinking_budget"`, so explicit
    /// levels must send `reasoning_effort` *only*.
    #[test]
    fn explicit_levels_send_only_reasoning_effort() {
        for (level, expected) in [
            ("off", Some("none")),
            ("disabled", Some("none")),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("xhigh", Some("xhigh")),
            ("max", Some("max")),
        ] {
            let opts = thought_level_params(level);
            assert_eq!(opts.reasoning_effort.as_deref(), expected, "level {level}");
            assert!(
                opts.additional_body_params.is_empty(),
                "level {level} must not add body params"
            );
        }
    }
}
