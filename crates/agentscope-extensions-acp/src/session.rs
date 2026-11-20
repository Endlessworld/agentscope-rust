//! ACP agent session: holds per-session state and drives the core [`ReActAgent`]
//! on each prompt, streaming ACP `SessionUpdate` notifications back to the client.
//!
//! Mirrors `AgiAgentSession` from the Kotlin `AgiHarnessAgent`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

#[cfg(feature = "durable")]
use crate::turn_driver::{
    DriverError, Guarantees, ReplayItem, ResumeOutcome, TurnOutcome, TurnView,
};
use crate::turn_driver::{DriverInput, EventSink, PromptCaps, TurnDriver};
use agent_client_protocol::v1::schema::{
    ContentBlock, CreateElicitationRequest, CreateElicitationResponse, MessageId, PermissionOption,
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigOption, SessionId,
    SessionMode, SessionModeState, StopReason, ToolCallUpdate,
};
use agent_client_protocol::{v1::ConnectionTo, Client};
use agentscope_core::agent::CallableAgent;
use agentscope_core::event::{AgentEvent, AgentEventType, ConfirmResult};
#[cfg(feature = "durable")]
use agentscope_core::journal::{ThreadJournal, UncertaintyTracker};
use agentscope_core::message::{ChatUsage, Msg, ToolUseBlock};
use agentscope_core::model::catalog::ModelDescriptor;
use agentscope_core::model::{GenerateOptions, DEFAULT_CONTEXT_WINDOW};
use agentscope_core::permission::{PermissionBehavior, PermissionRule};
use agentscope_core::ConfirmHandler;
#[cfg(feature = "durable")]
use agentscope_durable::backend::TxnBackend;
#[cfg(feature = "durable")]
use agentscope_durable::store::file::FileStore;
#[cfg(feature = "durable")]
use agentscope_durable::{
    DurableRuntime, DurableSession, HarnessAdapter, SessionBackend, SessionKey, SessionSpec,
    ThreadBinding, ThreadKey, TurnStatus,
};
#[cfg(feature = "durable")]
use agentscope_durable_adapter_agentscope::{AgentScopeHarnessAdapter, BackendThreadJournal};
#[cfg(feature = "durable")]
use sha2::{Digest, Sha256};

use agentscope_harness::agent::bus::MessageBus;
use agentscope_harness::agent::subagent::task::{BackgroundTask, TaskStatus};
use agentscope_harness::agent::HarnessAgent;

use super::config::{self, ModeChoice, ModelChoice};
#[cfg(feature = "durable")]
use super::deferred_effects::OffloadedToolEffects;
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
    /// The ACP mode the parent session is currently in.
    ///
    /// Read on every spawn: a child must inherit the mode in force when it starts (a session
    /// can switch from `yolo` to `accept_edits` between two spawns), so a snapshot taken when
    /// the session was built would be wrong. Shared with [`AcpAgentSession::apply_mode`].
    mode: Arc<Mutex<String>>,
    /// Every child of this session's tree, keyed by child session id.
    ///
    /// Shared with [`AcpAgentSession`]: a nested child is owned by an intermediate subagent's
    /// spawn tool, so cancelling it by id has no other route back to the agent. Weak on purpose:
    /// the owning spawn tool already keeps a live child alive, and a strong map here would pin
    /// every finished subagent (with its toolkit, model and workspace) for the session's life.
    agents: Arc<Mutex<HashMap<String, Weak<dyn CallableAgent>>>>,
}

impl SubagentPermissionHook {
    pub fn new(
        client: Arc<Mutex<Option<Arc<AcpClientHandle>>>>,
        store: Arc<PermissionRuleStore>,
        parent: SessionId,
        mode: Arc<Mutex<String>>,
        agents: Arc<Mutex<HashMap<String, Weak<dyn CallableAgent>>>>,
    ) -> Self {
        Self {
            client,
            store,
            parent,
            mode,
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
        // A child starts with the mode the user currently selected, not with the framework's
        // trivial default (which allows every tool call without asking). This is what makes an
        // `accept_edits` session ask before the child runs a shell command.
        let mode = self
            .mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        agent.set_permission_context(config::permission_context_for_mode(&mode));
        // "Always allow / always reject" answers survive across children: the rules are
        // persisted by the same store the parent session uses, so a fresh child would otherwise
        // ask again for a decision the user already made.
        for rule in self.store.load() {
            agent.add_permission_rule(rule);
        }
        // Let the child tag its own spawns (nested delegation) before it starts working.
        agent.set_host_session_id(child_session);
        self.agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(child_session.to_string(), Arc::downgrade(agent));
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

/// The `/compact` slash command this agent advertises to clients
/// (`event::available_commands`): a host-level shortcut, not a model turn.
pub(crate) const COMPACT_COMMAND_NAME: &str = "compact";

/// Whether a prompt is the host-level `/compact` shortcut.
///
/// Only an exact, single text block counts: a message that merely mentions `/compact` is a
/// normal prompt, and a mixed-content prompt (text plus images) is not a command.
fn is_compact_command(content: &[ContentBlock]) -> bool {
    match content {
        [ContentBlock::Text(text)] => text
            .text
            .trim()
            .eq_ignore_ascii_case(&format!("/{COMPACT_COMMAND_NAME}")),
        _ => false,
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

/// Name this session's turns are registered under in the durable runtime.
#[cfg(feature = "durable")]
const DURABLE_HARNESS: &str = "agentscope-acp";
/// Version of this host's turn definition; a checkpoint is only restorable under the same one.
#[cfg(feature = "durable")]
const DURABLE_DEFINITION: &str = "acp/1";
/// Namespace of an ACP session inside its workspace's durable store.
#[cfg(feature = "durable")]
const DURABLE_NAMESPACE: &str = "acp";
/// How long the tick loop rests between steps.
#[cfg(feature = "durable")]
const TICK_PAUSE: std::time::Duration = std::time::Duration::from_millis(20);
/// How long a prompt waits for a tick in flight before it stops waiting for it.
#[cfg(feature = "durable")]
const DRIVER_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// How long a steer waits for the prompt it targets to be claimed by a writer.
///
/// A prompt that was just accepted is `Queued` for a few milliseconds (`epoch == 0`), and
/// durable answers `CONFLICT` for it. Telling the client "not running" in that window would
/// turn the most common case (type while the first token streams) into an error.
#[cfg(feature = "durable")]
const STEER_SETTLE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
/// How long a `running` turn may make no durable progress before the client is told.
///
/// Not a failure: a long command or a slow provider is legitimate, and the prompt keeps
/// waiting. The notice exists because a hang with no output is indistinguishable from a lost
/// request, and the user has to know that cancelling is an option.
#[cfg(feature = "durable")]
const RUNNING_STALL_NOTICE: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Default hard cap for a `running` turn with no durable progress.
///
/// Above every per-tool ceiling the ACP agent configures (the shell and generic tool timeout is
/// 30 minutes), so only a genuinely wedged run reaches it. `AGENTSCOPE_ACP_STALL_CANCEL_SECS`
/// overrides it; `0`/`off` disables the cap.
#[cfg(feature = "durable")]
const RUNNING_STALL_CANCEL: std::time::Duration = std::time::Duration::from_secs(45 * 60);
/// Message recorded for a tool call whose outcome was lost with the process that dispatched it.
#[cfg(feature = "durable")]
const ABANDONED_ON_RESUME_MESSAGE: &str = "This tool call was dispatched, but its outcome was \
     never recorded (the process ended or the run was interrupted). It was not retried; issue the \
     call again if it is still needed.";
/// Why a resumed session settles the tool calls it found unsettled.
#[cfg(feature = "durable")]
const RECONCILE_REASON: &str = "resume: abandoned tool calls whose outcome was never recorded";

/// The hard cap for a silent `running` turn, from `AGENTSCOPE_ACP_STALL_CANCEL_SECS`.
#[cfg(feature = "durable")]
fn running_stall_cancel_after() -> Option<std::time::Duration> {
    let value = std::env::var("AGENTSCOPE_ACP_STALL_CANCEL_SECS").ok();
    stall_cancel_after_value(value.as_deref())
}

/// Parses the stall-cap override; `None` means "wait forever" (the pre-existing behaviour).
#[cfg(feature = "durable")]
fn stall_cancel_after_value(value: Option<&str>) -> Option<std::time::Duration> {
    let Some(value) = value else {
        return Some(RUNNING_STALL_CANCEL);
    };
    let value = value.trim().to_ascii_lowercase();
    if matches!(value.as_str(), "off" | "0" | "false" | "no" | "disabled") {
        return None;
    }
    match value.parse::<u64>() {
        Ok(secs) if secs > 0 => Some(std::time::Duration::from_secs(secs)),
        // An unreadable value is a typo, not an opt-out: keep the safe default.
        _ => Some(RUNNING_STALL_CANCEL),
    }
}

/// The durable runtime that drives this session's turns.
///
/// One thread per ACP session. `session` admits, ticks and reads it; `control` is the same
/// thread through a *different* owner, because a control command issued by the owner a tick is
/// running under would take — and release — that tick's lease; and `journal` is the same store
/// opened as a core [`ThreadJournal`], which is what `session/load` replays and what the turn's
/// ending is read through.
#[cfg(feature = "durable")]
struct SessionRuntime {
    session: Arc<DurableSession>,
    control: Arc<DurableRuntime>,
    journal: Arc<dyn ThreadJournal>,
    /// 跨轮投影缓存：每轮的「是否还有悬挂 dispatch」检查只需 fold 新增事件。
    ///
    /// 以前每轮都 `JournalProjection::read` 全量折叠 journal（≈1.4MB scope + 全部事件，
    /// 实测 25MB/轮分配）。缓存后正常轮只读自上次检查以来新增的几十条事件；
    /// `reconcile_abandoned_tool_calls` 写入新事实后缓存失效（置 None）。
    projection: tokio::sync::Mutex<Option<UncertaintyTracker>>,
}

/// Settles the tool calls a dead run left uncertain, so the session can be opened again.
///
/// The recorder refuses to open a journal that holds a dispatched call without a result, which
/// made a session whose process died mid-tool unopenable forever. Tools run in this process, so
/// such an outcome can never arrive later: recording it as interrupted (never as success, never
/// re-executed) is what lets the session continue.
#[cfg(feature = "durable")]
async fn reconcile_abandoned_tool_calls(journal: &Arc<dyn ThreadJournal>) {
    agentscope_core::diag::mark("acp:reconcile");
    match agentscope_core::journal::recovery::settle_uncertain_tool_outcomes(
        Arc::clone(journal),
        ABANDONED_ON_RESUME_MESSAGE,
        RECONCILE_REASON,
    )
    .await
    {
        Ok(settled) if !settled.is_empty() => tracing::warn!(
            calls = ?settled,
            "recording tool calls whose outcome was never committed as interrupted"
        ),
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "could not settle the tool calls left uncertain by a dead run")
        }
    }
}

/// Settles the tool calls a *finished* turn left uncertain.
///
/// A cancelled or stopped acting phase can end the turn while one of its dispatches never got a
/// result. [`reconcile_abandoned_tool_calls`] runs once, when the runtime is built; a session
/// that hit this later in the same process would fail every following prompt with "Explicit
/// reconciliation required", because the recorder refuses to open the journal. An unfinished
/// turn is left alone: it owns its uncertainty, and resuming it must not find synthetic results.
///
/// Production runs [`settle_finished_turn_uncertainties_cached`]; this full-fold variant is kept
/// as the reference implementation the cached check is tested against.
#[cfg(all(test, feature = "durable"))]
async fn settle_finished_turn_uncertainties(journal: &Arc<dyn ThreadJournal>) {
    agentscope_core::diag::mark("acp:settle");
    match agentscope_core::journal::TurnViews::read(&**journal).await {
        Ok(turns) if turns.latest().map(|turn| !turn.ended()).unwrap_or(false) => return,
        Ok(_) => {}
        Err(error) => {
            tracing::debug!(%error, "could not read the turns to look for stranded tool calls");
            return;
        }
    }
    agentscope_core::diag::mark("acp:settle_read");
    reconcile_abandoned_tool_calls(journal).await;
}

/// Settles unfinished-tool-call uncertainty using the runtime's cross-prompt projection cache.
///
/// Same outcome as [`settle_finished_turn_uncertainties`] — a finished turn's unanswered dispatch
/// is recorded as interrupted, an unfinished turn's dispatch is left alone — but the check folds
/// only the events committed since the previous prompt. The full fold cost one whole-journal
/// replay per prompt (≈25MB of allocation at a 1.4MB scope, measured), which dominated every turn
/// once the durable commit path had been trimmed.
#[cfg(feature = "durable")]
async fn settle_finished_turn_uncertainties_cached(runtime: &SessionRuntime) {
    agentscope_core::diag::mark("acp:settle");
    // An unfinished turn owns its uncertainty: resuming it must not find synthetic results.
    match runtime.session.active().await {
        Ok(None) => {}
        Ok(Some(turn)) => {
            tracing::trace!(turn = %turn.turn_id, "an unfinished turn keeps its dispatch uncertain");
            return;
        }
        Err(error) => {
            tracing::debug!(%error, "could not check for an unfinished turn");
            return;
        }
    }
    settle_uncertain_with_cache(&runtime.journal, &runtime.projection).await;
}

/// The cached-fold half of [`settle_finished_turn_uncertainties_cached`].
///
/// The caller owns the turn gate: it must only be reached when no turn of the thread is open,
/// because reconciliation turns every uncertain dispatch into an `interrupted` result.
#[cfg(feature = "durable")]
async fn settle_uncertain_with_cache(
    journal: &Arc<dyn ThreadJournal>,
    cache: &tokio::sync::Mutex<Option<UncertaintyTracker>>,
) {
    let mut guard = cache.lock().await;
    let mut fresh = false;
    if let Some(view) = guard.as_mut() {
        match view.catch_up(&**journal).await {
            Ok(()) => fresh = true,
            Err(error) => tracing::debug!(%error, "could not catch up the cached projection"),
        }
    }
    if !fresh {
        match UncertaintyTracker::read(&**journal).await {
            Ok(view) => *guard = Some(view),
            Err(error) => {
                tracing::debug!(%error, "could not read the journal projection");
                return;
            }
        }
    }
    let nothing_uncertain = guard
        .as_ref()
        .map(|view| view.uncertain_tool_calls.is_empty())
        .unwrap_or(true);
    drop(guard);
    agentscope_core::diag::mark("acp:settle_read");
    if nothing_uncertain {
        return;
    }
    // Reconciliation writes facts (synthetic results, `recovery/applied`, a checkpoint), so the
    // cached fold no longer describes the chain.
    *cache.lock().await = None;
    reconcile_abandoned_tool_calls(journal).await;
}

/// A request id for a durable control command.
///
/// Control is idempotent on its request id, so a *fresh* id per call is what makes a repeated
/// cancel a new intent instead of a stale receipt; the value only has to be unique per command.
#[cfg(feature = "durable")]
fn control_id(kind: &str) -> String {
    format!("{kind}-{}", uuid::Uuid::new_v4())
}

/// The status name this client compares against, mapped from the durable turn.
#[cfg(feature = "durable")]
pub(crate) fn turn_status_name(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Succeeded => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::Cancelled => "cancelled",
        // A paused turn is the "interrupted" this client resumes: the run stopped without a
        // result and the position is still there, which is the same situation under a new name.
        TurnStatus::Paused | TurnStatus::Pausing => "interrupted",
        TurnStatus::Waiting => "waiting",
        TurnStatus::Queued => "queued",
        TurnStatus::Running | TurnStatus::Cancelling => "running",
    }
}

/// The status of an unfinished turn as the queue extension reports it.
///
/// [`turn_status_name`] answers what the *prompt* path needs, where a paused turn is an
/// interruption to continue. A queue entry is different: the client renders "paused" as a
/// deliberate state of its own, so the durable names are kept apart.
#[cfg(feature = "durable")]
pub(crate) fn queued_status_name(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Queued => "queued",
        TurnStatus::Running => "running",
        TurnStatus::Waiting => "waiting",
        TurnStatus::Pausing => "pausing",
        TurnStatus::Paused => "paused",
        TurnStatus::Cancelling => "cancelling",
        TurnStatus::Succeeded => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::Cancelled => "cancelled",
    }
}

/// The oldest unfinished turn of the thread, if any.
///
/// The runtime always advances the oldest unfinished turn, so this is the only turn a tick can
/// make progress on; queue listings walk the same order.
#[cfg(feature = "durable")]
async fn unfinished_head(
    runtime: &Arc<SessionRuntime>,
) -> std::result::Result<Option<agentscope_durable::TurnView>, DriverError> {
    let mut cursor: Option<String> = None;
    loop {
        let page = runtime
            .session
            .turns(cursor.as_deref(), 1000)
            .await
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        if let Some(turn) = page.items.iter().find(|turn| !turn.status.terminal()) {
            return Ok(Some(turn.clone()));
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(None),
        }
    }
}

/// The place of `turn_id` among the thread's unfinished turns; `1` is the next to run.
#[cfg(feature = "durable")]
async fn unfinished_position(
    runtime: &Arc<SessionRuntime>,
    turn_id: &str,
) -> std::result::Result<u32, String> {
    let mut cursor: Option<String> = None;
    let mut position = 0u32;
    loop {
        let page = runtime
            .session
            .turns(cursor.as_deref(), 1000)
            .await
            .map_err(|error| error.to_string())?;
        for turn in &page.items {
            if turn.status.terminal() {
                continue;
            }
            position += 1;
            if turn.turn_id == turn_id {
                return Ok(position);
            }
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            // The turn was admitted by this call, so a missing one means the page boundary
            // changed under the read; the end of the queue is the closest true answer.
            None => return Ok(position.max(1)),
        }
    }
}

/// The durable runtime that backs this session's turns.
///
/// The slot used to live on [`AcpAgentSession`]. Owning it here is the first step of the seam in
/// [`crate::turn_driver`]: the protocol layer will reach turns through the trait, and this type
/// will own the store handle, the thread and the tick loop instead of the session.
///
/// Nothing routes through the trait yet: `run`/`cancel`/`view`/`replay`/`resume` answer
/// `Unavailable` and the session keeps calling its own methods, so the migration changes no
/// behaviour. What it does pin is the ownership: the session no longer holds a runtime field.
/// How many times a run its host interrupted is continued before the driver gives up.
///
/// Two covers the transient signal a host sends and leaves room for one more; a run interrupted
/// a third time is failing for a reason the prompt cannot fix.
pub(crate) const MAX_INTERRUPTED_RESUMES: usize = 2;

/// The error text a client should see: exactly one `Agent error:` prefix, then the provider's
/// own detail.
///
/// Core errors may already carry the prefix (possibly more than once through wrapping), and a
/// duplicated prefix hides the part that tells the user what actually went wrong.
pub(crate) fn error_chunk_text(error: &str) -> String {
    let mut detail = error;
    while let Some(rest) = detail.strip_prefix("Agent error: ") {
        detail = rest;
    }
    format!("Agent error: {detail}")
}

/// The ACP usage update for a session: `used` is the tokens in context, `size` the window.
pub(crate) fn usage_update(
    session_id: SessionId,
    context_window: u64,
    used: u64,
) -> agent_client_protocol::v1::schema::SessionNotification {
    use agent_client_protocol::v1::schema::{SessionNotification, SessionUpdate, UsageUpdate};
    SessionNotification::new(
        session_id,
        SessionUpdate::UsageUpdate(UsageUpdate::new(used, context_window)),
    )
}

/// What the render step needs from the running turn.
///
/// Both drivers feed the same events (`DurableDriver` from the runtime's sink, `NativeDriver`
/// from `HarnessAgent::stream_events`), so the mapping from event to notification lives here
/// rather than in either driver.
pub(crate) struct RenderCtx<'a> {
    pub(crate) session_id: &'a SessionId,
    pub(crate) message_id: Option<MessageId>,
    pub(crate) caps: &'a PromptCaps,
    /// The live model's context window, read once per turn.
    pub(crate) context_window: u64,
    pub(crate) emitted_text: bool,
    pub(crate) tool_events: &'a mut super::event::ToolEventMapper,
}

impl RenderCtx<'_> {
    fn usage(&self, used: u64) -> agent_client_protocol::v1::schema::SessionNotification {
        usage_update(self.session_id.clone(), self.context_window, used)
    }
}

/// Renders one agent event into the notifications the client should see, accumulating the
/// turn's usage as it goes.
pub(crate) fn render_event(
    ctx: &mut RenderCtx<'_>,
    agent_event: &AgentEvent,
    turn_usage: &mut ChatUsage,
    out: &mut dyn FnMut(agent_client_protocol::v1::schema::SessionNotification),
) {
    match agent_event.event_type() {
        AgentEventType::TextBlockDelta => {
            if let Some(notification) = super::event::map_delta_event(
                ctx.session_id.clone(),
                agent_event,
                ctx.message_id.clone(),
            ) {
                ctx.emitted_text = true;
                out(notification);
            }
        }
        AgentEventType::AgentResult => {
            // An interrupted turn still produces a (reconciled) result. Tell the user that it
            // was interrupted, but do not render the runtime's recovery text ("I noticed that
            // you have interrupted me..."): it is an internal control message, and the prompt
            // loop resumes the turn automatically - showing both would claim the turn ended
            // immediately before continuing it.
            let interrupted = super::event::result_generate_reason(agent_event)
                == Some(agentscope_core::message::GenerateReason::Interrupted);
            if interrupted {
                out(super::event::message_chunk(
                    ctx.session_id.clone(),
                    "\n\n[turn interrupted before completion]".to_string(),
                    ctx.message_id.clone(),
                ));
            } else if !ctx.emitted_text {
                // Emit the final reply only if it wasn't already streamed incrementally (avoids
                // duplicate output).
                if let Some(notification) = super::event::map_result_event(
                    ctx.session_id.clone(),
                    agent_event,
                    ctx.message_id.clone(),
                ) {
                    out(notification);
                }
            }
        }
        AgentEventType::ModelCallEnd => {
            // Accumulate usage and push a context-window UsageUpdate after every model call.
            // `used` reflects the tokens in the current context (latest call's input tokens),
            // NOT the cumulative session count.
            if let Some(usage) = agent_event
                .data()
                .fields
                .get("usage")
                .and_then(|value| serde_json::from_value::<ChatUsage>(value.clone()).ok())
            {
                turn_usage.input_tokens += usage.input_tokens;
                turn_usage.output_tokens += usage.output_tokens;
                turn_usage.cached_tokens += usage.cached_tokens;
                ctx.caps
                    .last_input_tokens
                    .store(usage.input_tokens, Ordering::SeqCst);
                out(ctx.usage(usage.input_tokens));
            } else {
                // No usage on this event: keep showing the last provider-reported context size
                // instead of 0.
                out(ctx.usage(ctx.caps.last_input_tokens.load(Ordering::SeqCst)));
            }
        }
        AgentEventType::Custom => {
            // The compaction middleware announces a completed summarization in-band; translate
            // it into the capability-gated `compaction_update` only when the client advertised
            // `session.compaction`.
            if let Some((_, summary)) = super::event::compaction_marker(agent_event) {
                if ctx.caps.supports_compaction {
                    let compaction_id = format!(
                        "compaction-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    );
                    for notification in super::event::compaction_notifications(
                        ctx.session_id.clone(),
                        &compaction_id,
                        &summary,
                    ) {
                        out(notification);
                    }
                }
            }
        }
        _ => {
            if let Some(notification) = super::event::map_delta_event(
                ctx.session_id.clone(),
                agent_event,
                ctx.message_id.clone(),
            ) {
                out(notification);
            }
            let session_id = ctx.session_id.clone();
            for notification in ctx.tool_events.map(session_id, agent_event) {
                out(notification);
            }
        }
    }
}

/// The parts of a prompt that outlive admission.
///
/// A turn is driven the same way whether this prompt admitted it or the queue extension did;
/// only these values differ.
#[cfg(feature = "durable")]
#[derive(Clone)]
struct DrivePlan {
    cancel_epoch: Arc<std::sync::atomic::AtomicU64>,
    admitted_epoch: u64,
    closed: Arc<std::sync::atomic::AtomicBool>,
    cancellation: Option<agent_client_protocol::RequestCancellation>,
    wakeup: bool,
    caps: PromptCaps,
}

#[cfg(feature = "durable")]
pub(crate) struct DurableDriver {
    agent: Arc<HarnessAgent>,
    session_id: SessionId,
    runtime: tokio::sync::Mutex<Option<Arc<SessionRuntime>>>,
}

#[cfg(feature = "durable")]
impl DurableDriver {
    fn new(agent: Arc<HarnessAgent>, session_id: SessionId) -> Self {
        Self {
            agent,
            session_id,
            runtime: tokio::sync::Mutex::new(None),
        }
    }

    /// The slot [`AcpAgentSession::open_runtime`] fills and every reader locks.
    fn runtime(&self) -> &tokio::sync::Mutex<Option<Arc<SessionRuntime>>> {
        &self.runtime
    }

    /// The runtime already opened for this session, if any.
    async fn cached(&self) -> Option<Arc<SessionRuntime>> {
        self.runtime.lock().await.as_ref().map(Arc::clone)
    }

    /// The runtime context that identifies this ACP session in the journal.
    ///
    /// The user id is left unset on purpose: `HarnessAgent::session_key` falls back to the
    /// agent's pinned state, so a host that configured a tenant keeps it.
    fn session_context(&self) -> agentscope_core::agent::RuntimeContext {
        agentscope_core::agent::RuntimeContext::new().with_session_id(self.session_id.to_string())
    }

    /// The store root of this session's durable state.
    ///
    /// The root is the harness *workspace*, not the ACP `cwd`: `session/load` rebuilds the
    /// session from the cwd the client sends, and the conversation must still be found there
    /// (the harness journal lives in the workspace for the same reason).
    fn durable_root(&self) -> std::result::Result<PathBuf, String> {
        self.agent
            .workspace_manager()
            .map(|manager| manager.workspace().join(".agentscope").join("durable"))
            .ok_or_else(|| "the agent has no workspace to keep durable state in".to_string())
    }

    /// The thread this ACP session drives: one session, one thread.
    fn durable_thread(&self) -> std::result::Result<ThreadKey, String> {
        let session = SessionKey::new(DURABLE_NAMESPACE, self.session_id.to_string())
            .map_err(|error| error.to_string())?;
        ThreadKey::new(session, "main").map_err(|error| error.to_string())
    }

    fn durable_backend(&self) -> std::result::Result<Arc<dyn SessionBackend>, String> {
        let root = self.durable_root()?;
        // A build that compiles the SQLite store in uses it by default, so an embedded
        // deployment does not depend on an environment variable. `AGENTSCOPE_DURABLE_STORE=file`
        // keeps a deployment on the file store without a rebuild.
        #[cfg(feature = "durable-sqlite")]
        if std::env::var("AGENTSCOPE_DURABLE_STORE").as_deref() != Ok("file") {
            std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
            let store = agentscope_durable_sqlite::SqliteStore::open(root.join("durable.db"))
                .map_err(|error| error.to_string())?;
            return Ok(Arc::new(TxnBackend::with_store(Arc::new(store))));
        }
        let store = FileStore::open(root).map_err(|error| error.to_string())?;
        Ok(Arc::new(TxnBackend::with_store(Arc::new(store))))
    }

    /// The durable journal of this session, for a reader that must not open a runtime (the
    /// `session/load` replay). Reading a thread that was never written is an error, which the
    /// caller turns into an empty replay.
    fn durable_journal(&self) -> std::result::Result<Arc<dyn ThreadJournal>, String> {
        Ok(Arc::new(BackendThreadJournal::new(
            self.durable_backend()?,
            self.durable_thread()?,
        )))
    }
    /// Installs the bounded queue this turn renders from and hands it back.
    ///
    /// The sink runs inside the agent and cannot block, so a stalled client would grow an
    /// unbounded channel without limit. Overflow is dropped and counted (`AGENT_RESULT` is
    /// always kept), and the end of the turn reports it.
    fn attach_events(&self, runtime: &Arc<SessionRuntime>) -> Arc<crate::event_queue::EventQueue> {
        let events = crate::event_queue::EventQueue::new(crate::event_queue::EVENT_QUEUE_CAPACITY);
        let events_sink = crate::event_queue::EventQueueSender::new(Arc::clone(&events));
        runtime
            .session
            .set_event_sink(Arc::new(move |value: &serde_json::Value| {
                // The sink runs inside the agent and cannot block, so the value it publishes is
                // the serialized event this client renders. A closed queue only means the
                // prompt already returned; the run itself is unaffected.
                if let Ok(event) = serde_json::from_value::<AgentEvent>(value.clone()) {
                    events_sink.push(event);
                }
            }));
        events
    }

    /// Runs one turn: admits it, ticks it, settles it and renders what the sink publishes.
    ///
    /// Inherent rather than trait so the rest of this file can call it without a `dyn`
    /// dispatch; [`TurnDriver::run`] delegates here.
    async fn run_turn(
        &self,
        input: DriverInput,
        sink: EventSink,
    ) -> std::result::Result<TurnOutcome, DriverError> {
        let message = input.message;
        let wakeup_turn = input.wakeup;
        let cancel_epoch = Arc::clone(&input.cancel_epoch);
        let my_epoch = input.admitted_epoch;
        let closed = Arc::clone(&input.closed);
        let cancellation = input.cancellation;
        let caps = input.caps;
        // Only the pre-admission failures below report it: a turn that reaches `drive_turn`
        // accumulates its own usage there.
        let turn_usage = ChatUsage::default();
        let runtime = match self.open_runtime().await {
            Ok(runtime) => runtime,
            Err(error) => {
                sink(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: {error}"),
                    None,
                ));
                return Ok(TurnOutcome {
                    stop_reason: StopReason::Refusal,
                    usage: Some(turn_usage),
                });
            }
        };
        // A turn that already ended can still hold a dispatch without a result: a cancelled or
        // stopped acting phase, or a run that failed after dispatch. The recorder refuses to
        // open such a journal, and `open_runtime` reconciles only once, when it builds the
        // runtime — so every later prompt in this process used to fail with "Explicit
        // reconciliation required". No run of this thread is in flight here (the prompt lock is
        // held and the turn below has not been admitted yet), which is what makes settling the
        // leftover uncertainty safe. The cached check makes this O(events since the last prompt).
        settle_finished_turn_uncertainties_cached(&runtime).await;
        agentscope_core::diag::mark("acp:after_settle");
        // A turn left unfinished by an earlier prompt still owns the thread: the runtime always
        // advances the oldest unfinished turn, so a new admission would sit behind it forever.
        // `Paused` is the one state that needs a nudge (it is deliberately not ready); a turn a
        // dead process left `Running` is reclaimed once its lease expires, which the admission
        // wait below already covers.
        if let Ok(Some(current)) = runtime.session.active().await {
            if current.status == TurnStatus::Paused {
                if let Err(error) = runtime
                    .control
                    .resume(
                        runtime.session.thread(),
                        &current.turn_id,
                        &control_id("resume"),
                    )
                    .await
                {
                    tracing::warn!(
                        session = %self.session_id,
                        turn = %current.turn_id,
                        %error,
                        "could not resume the paused turn before the next prompt"
                    );
                }
            }
        }
        // The key identifies this prompt in the store: it is derived from the user's text plus
        // the acceptance time, so a retry of this attempt reuses it while an identical question
        // asked later is still a new turn. A wakeup turn has no text to hash.
        let idempotency_key = message
            .as_ref()
            .map(|message| prompt_idempotency_key(message, chrono::Utc::now().timestamp_millis()));
        let input = match message.as_ref().map(serde_json::to_value) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) => {
                sink(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: the prompt could not be encoded: {error}"),
                    None,
                ));
                return Ok(TurnOutcome {
                    stop_reason: StopReason::Refusal,
                    usage: Some(turn_usage),
                });
            }
            None => None,
        };
        let mut submit_attempts = 0usize;
        let turn_id = loop {
            submit_attempts += 1;
            // Submitting touches durable storage, which can fail transiently (a competing
            // writer, an fsync hiccup). Those failures are not the user's fault and must not
            // end the turn on the first attempt; the idempotency key makes the retry safe.
            let submitted = match (&idempotency_key, &input) {
                (Some(key), Some(input)) => runtime.session.submit(key, input.clone()).await,
                _ => {
                    let key = format!(
                        "wakeup-{}-{}",
                        self.session_id,
                        chrono::Utc::now().timestamp_millis()
                    );
                    runtime.session.wakeup(&key).await
                }
            };
            match submitted {
                Ok(receipt) => break receipt.turn_id,
                Err(error) if submit_attempts < 3 => {
                    tracing::warn!(
                        session = %self.session_id,
                        %error,
                        "task submission failed; retrying with the same idempotency key"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(error) => {
                    sink(super::event::message_chunk(
                        self.session_id.clone(),
                        format!("Agent error: {error}"),
                        None,
                    ));
                    return Ok(TurnOutcome {
                        stop_reason: StopReason::Refusal,
                        usage: Some(turn_usage),
                    });
                }
            }
        };

        // The turn exists now; from here on it is only a turn to drive. Keeping admission
        // and driving apart lets a queued turn be driven without a prompt of its own.
        let plan = DrivePlan {
            cancel_epoch,
            admitted_epoch: my_epoch,
            closed,
            cancellation,
            wakeup: wakeup_turn,
            caps,
        };
        self.drive_turn(runtime, turn_id, plan, sink).await
    }

    /// Drives one admitted turn: ticks it, settles it, and renders what the sink publishes.
    ///
    /// Split from [`Self::run_turn`] so a turn admitted earlier can be driven without a new
    /// prompt (a queued turn, or one resumed after a pause).
    async fn drive_turn(
        &self,
        runtime: Arc<SessionRuntime>,
        turn_id: String,
        plan: DrivePlan,
        sink: EventSink,
    ) -> std::result::Result<TurnOutcome, DriverError> {
        let DrivePlan {
            cancel_epoch,
            admitted_epoch: my_epoch,
            closed,
            cancellation,
            wakeup: wakeup_turn,
            caps,
        } = plan;
        // Every update this turn publishes is tagged with the turn it belongs to. A client
        // renders a prompt and the turns queued behind it as one stream, so an update without
        // its turn would land in whichever assistant message happened to be open.
        let sink: EventSink =
            {
                let inner = sink;
                let turn_id = turn_id.clone();
                Arc::new(
                move |mut notification: agent_client_protocol::v1::schema::SessionNotification| {
                    notification
                        .meta
                        .get_or_insert_with(serde_json::Map::new)
                        .insert("turnId".to_string(), serde_json::Value::String(turn_id.clone()));
                    inner(notification);
                },
            )
            };
        let mut turn_usage = ChatUsage::default();
        // Read once: the model does not change inside a turn.
        let context_window = self.context_window();
        let events = self.attach_events(&runtime);
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
        tool_events = tool_events.with_plan_operations(caps.supports_plan_operations);

        // Push the initial context-window usage so clients render the gauge
        // before the first model call completes. Before any model call there is no
        // real in-context count, so report zero; afterwards keep showing the last
        // provider-reported input size until the new call reports its own.
        sink(self.usage_notification(caps.last_input_tokens.load(Ordering::SeqCst)));

        // Watchdog state.
        //
        // `queued` is the only status that means "nothing picked this up yet". It is a
        // legitimate wait while a *previous* process's writer lease is still valid (the lease
        // is 30s), so the deadline must be comfortably longer than that or a normal restart
        // looks like a hang.
        const ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
        // How long a turn may sit in `waiting` before the prompt reports the block.
        //
        // A waiting turn is not ready, so the tick loop hands it back and nothing else advances
        // it. A run that needs a signal nobody ever sends kept the prompt open forever, and the
        // client saw neither a result nor an error. Waiting on a real signal is legitimate, so
        // the bound is generous; only a permanent block reaches it.
        const WAITING_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
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
        // A wakeup turn has no prompt of its own waiting on it. Background results usually land
        // within minutes; a wakeup that keeps polling for longer is blocking a real user prompt,
        // so it is suspended and that prompt is allowed to run.
        const WAKEUP_TURN_BUDGET: std::time::Duration = std::time::Duration::from_secs(15 * 60);
        // A run stopped by something other than this client's cancel is resumed in place.
        // Bounded so a permanently interrupted environment is reported instead of retried
        // forever; an interruption is not a failure and must not discard the user's prompt.
        let mut resumes = 0usize;

        loop {
            // ---- one dispatched attempt: stream until it settles or is cancelled ----
            let mut settled_task: Option<agentscope_durable::TurnView> = None;
            let mut cancelled = false;
            // The tick loop is the only writer: this attempt starts it, renders what it
            // publishes, and hands the thread back when it leaves.
            let driver = TickLoop::spawn(Arc::clone(&runtime.session), turn_id.clone());
            let attempt_started = std::time::Instant::now();
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
            // When the current `queued` wait started, so a progress notice can report how long
            // the prompt has been waiting behind the previous turn.
            let mut queued_since: Option<std::time::Instant> = None;
            // When the current `waiting` stretch started. A waiting turn is re-claimed as soon as
            // its `next_attempt_at` passes, so a turn that stays here is waiting on something that
            // will never arrive; without this clock the prompt below never leaves the ticker loop
            // and the client sees neither a result nor an error.
            let mut waiting_since: Option<std::time::Instant> = None;
            let mut first_poll_failure: Option<std::time::Instant> = None;
            let mut stall: Option<String> = None;
            // Durable progress means a new turn revision: a streaming delta or a poll that
            // returns the same revision is not progress. A run with none for long enough is
            // either inside a legitimate long tool or wedged.
            let stall_cancel_after = running_stall_cancel_after();
            let mut last_revision = 0u64;
            let mut quiet_since = std::time::Instant::now();
            let mut stall_noticed = false;
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
                        if wakeup_turn
                            && settled_task.is_none()
                            && attempt_started.elapsed() >= WAKEUP_TURN_BUDGET
                        {
                            stall = Some(format!(
                                "the background wakeup turn ran for {}s without finishing; \
                                 it was cancelled so a queued prompt can run",
                                WAKEUP_TURN_BUDGET.as_secs()
                            ));
                            break;
                        }
                        if settled_task.is_none() {
                            match runtime.session.turn(&turn_id).await {
                                Ok(current) => {
                                    first_poll_failure = None;
                                    let status = turn_status_name(current.status);
                                    if current.revision != last_revision {
                                        last_revision = current.revision;
                                        quiet_since = std::time::Instant::now();
                                        stall_noticed = false;
                                    }
                                    if status != last_status {
                                        // Any status change is progress: restart the admission
                                        // clock so an earlier `queued` wait cannot accumulate.
                                        last_status = status.to_string();
                                        admission_deadline =
                                            std::time::Instant::now() + ADMISSION_TIMEOUT;
                                        queued_since = (status == "queued")
                                            .then(std::time::Instant::now);
                                        waiting_since = (status == "waiting")
                                            .then(std::time::Instant::now);
                                        quiet_since = std::time::Instant::now();
                                        stall_noticed = false;
                                    }
                                    if current.status.terminal() {
                                        settled_task = Some(current);
                                    } else if current.status == TurnStatus::Queued
                                        && std::time::Instant::now() >= admission_deadline
                                    {
                                        // The runtime always advances the oldest unfinished
                                        // turn, so a queued turn is a *normal* wait behind an
                                        // earlier one (a blocking tool can keep it alive long
                                        // after the cancel this prompt sent). Only a queue whose
                                        // head is this very turn is a real stall: nothing else
                                        // explains why it was not admitted.
                                        let head = runtime
                                            .session
                                            .state()
                                            .await
                                            .ok()
                                            .and_then(|state| state.current_turn_id);
                                        let nothing_running =
                                            head.as_deref() == Some(turn_id.as_str());
                                        if nothing_running {
                                            stall = Some(format!(
                                                "the turn was never admitted (status `queued` \
                                                 after {}s)",
                                                ADMISSION_TIMEOUT.as_secs()
                                            ));
                                            break;
                                        }
                                        // Tell the client the prompt is queued rather than lost;
                                        // it is still waiting on the previous turn.
                                        let waited = queued_since
                                            .map(|since| since.elapsed().as_secs())
                                            .unwrap_or_default();
                                        sink(self.advisory_notification(caps.supports_notices, format!(
                                            "Waiting for the previous task in this session to \
                                             finish ({waited}s so far)."
                                        )));
                                        admission_deadline =
                                            std::time::Instant::now() + ADMISSION_TIMEOUT;
                                    } else if current.status == TurnStatus::Waiting
                                        && waiting_since
                                            .is_some_and(|since| since.elapsed() >= WAITING_STALL_TIMEOUT)
                                    {
                                        // A waiting turn is not ready, so the tick loop hands it
                                        // back and nothing else will touch it. Report the block
                                        // instead of holding the prompt open forever: the run needs
                                        // a signal or a reconciliation no writer can supply.
                                        stall = Some(format!(
                                            "the turn has been waiting for {}s without a signal \
                                             (status `waiting`): {}",
                                            WAITING_STALL_TIMEOUT.as_secs(),
                                            current
                                                .error
                                                .clone()
                                                .unwrap_or_else(|| "no reason recorded".to_string())
                                        ));
                                        break;
                                    } else if current.status == TurnStatus::Running {
                                        // A tick in flight holds the turn; no status change will
                                        // be observed until it returns. Tell the client after a
                                        // long silence, and apply the (configurable) hard cap so a
                                        // genuinely wedged run cannot hold the prompt forever.
                                        let quiet = quiet_since.elapsed();
                                        if quiet >= RUNNING_STALL_NOTICE && !stall_noticed {
                                            stall_noticed = true;
                                            sink(self.advisory_notification(caps.supports_notices, format!(
                                                "The task has reported no progress for {}s. It \
                                                 may be running a long command or waiting on a \
                                                 provider; cancel it if this is unexpected.",
                                                quiet.as_secs()
                                            )));
                                        }
                                        if let Some(limit) = stall_cancel_after {
                                            if quiet >= limit {
                                                stall = Some(format!(
                                                    "the task made no durable progress for {}s \
                                                     (status `running`)",
                                                    quiet.as_secs()
                                                ));
                                                break;
                                            }
                                        }
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
                            Some(ev) => {
                                let mut render = RenderCtx {
                                    session_id: &self.session_id,
                                    message_id: message_id_opt.clone(),
                                    caps: &caps,
                                    context_window,
                                    emitted_text,
                                    tool_events: &mut tool_events,
                                };
                                render_event(
                                    &mut render,
                                    &ev,
                                    &mut turn_usage,
                                    &mut |notification| sink(notification),
                                );
                                emitted_text = render.emitted_text;
                            }
                            // The sink was dropped (the handle was closed) - no further events
                            // can arrive. Keep polling the receipt: returning here would report
                            // `end_turn` while the run is still writing its turn.
                            None => events_closed = true,
                        }
                    }
                }
            }

            if cancelled {
                // A cancelled parent must not leave its children behind: the subagent tasks it
                // spawned keep running (and billing) with nobody left to receive their result,
                // and the next prompt would find them still "running". Cancel them on the way
                // out, before waiting for the turn itself to settle.
                let session_id = self.session_id.to_string();
                let cancelled_children = self
                    .agent
                    .task_repository()
                    .cancel_session_tasks(&self.session_context(), &session_id)
                    .await;
                if cancelled_children > 0 {
                    tracing::info!(
                        session = %session_id,
                        tasks = cancelled_children,
                        "cancelled the session's subagent tasks along with the prompt"
                    );
                }
                // The ACP cancel flag is the only client-facing cancellation: write it durably
                // and let the invocation end itself. The tick observes the request at its next
                // poll and settles the turn as `Cancelled` — a stop, not a rejection, and not a
                // turn left open for the next prompt.
                //
                // The command goes through the control runtime, not through the one the tick
                // runs under: claiming under the tick's own owner would take and release the
                // very lease the invocation is writing with.
                if let Err(error) = runtime
                    .control
                    .cancel(runtime.session.thread(), &turn_id, &control_id("cancel"))
                    .await
                {
                    tracing::warn!(
                        session = %self.session_id,
                        turn = %turn_id,
                        %error,
                        "could not request the cancellation; the turn may stay open"
                    );
                }
                // A closed session has nobody left to answer (`session/close`, LRU eviction,
                // host shutdown): bound the wait so teardown cannot hang on the very turn it is
                // tearing down. A live client keeps the longer grace so the next prompt is not
                // admitted while the run is still winding down.
                let grace = if closed.load(Ordering::SeqCst) {
                    CLOSED_TURN_CLOSE_GRACE
                } else {
                    std::time::Duration::from_secs(10)
                };
                let deadline = std::time::Instant::now() + grace;
                // The tick loop hands a waiting or paused turn back instead of spinning on it,
                // so a cancel landing on one would leave nobody to write the ending and the turn
                // would sit in `cancelling` until the next prompt ticked it. Stop the loop first
                // (its in-flight tick observes the cancel through its control poll), then drive
                // the turn from here: a cancel of a turn nobody runs is settled by that tick
                // itself, without an invocation and without a model call.
                driver.finish().await;
                while std::time::Instant::now() < deadline {
                    match runtime.session.turn(&turn_id).await {
                        Ok(current) if current.status.terminal() => break,
                        Ok(_) | Err(_) => {}
                    }
                    // Teardown while the run winds down: stop waiting, the session is gone.
                    if closed.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = runtime.session.tick().await;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                return Ok(TurnOutcome {
                    stop_reason: StopReason::Cancelled,
                    usage: Some(turn_usage),
                });
            }

            // A watchdog fired: the command was never admitted, or the durable state stopped
            // being readable. Nothing ran (or nothing can be read), so there is no position
            // worth keeping: end the turn instead of leaving it in front of the next prompt.
            if let Some(reason) = stall {
                let _ = runtime
                    .control
                    .cancel(runtime.session.thread(), &turn_id, &control_id("stall"))
                    .await;
                driver.finish().await;
                sink(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: {reason}"),
                    message_id_opt.clone(),
                ));
                return Ok(TurnOutcome {
                    stop_reason: StopReason::Refusal,
                    usage: Some(turn_usage),
                });
            }

            // The inner loop can only exit without a receipt on cancel/stall (both handled
            // above). Never report success for a run whose outcome is unknown, and do not
            // declare it dead either: suspending keeps whatever it already did resumable.
            let Some(current) = settled_task else {
                let _ = runtime
                    .control
                    .suspend(runtime.session.thread(), &turn_id, &control_id("suspend"))
                    .await;
                driver.finish().await;
                sink(super::event::message_chunk(
                    self.session_id.clone(),
                    "Agent error: the run stopped without reporting a result; the turn was \
                     suspended, send a new prompt to continue"
                        .to_string(),
                    message_id_opt.clone(),
                ));
                return Ok(TurnOutcome {
                    stop_reason: StopReason::Refusal,
                    usage: Some(turn_usage),
                });
            };

            match turn_status_name(current.status) {
                "failed" => {
                    // The ending carries the failure the runtime recorded; there is no
                    // in-memory dispatch error to fall back to, because the writer that
                    // observed the failure is the one that wrote it.
                    let detail = current
                        .error
                        .clone()
                        .unwrap_or_else(|| "run failed".to_string());
                    driver.finish().await;
                    sink(super::event::message_chunk(
                        self.session_id.clone(),
                        error_chunk_text(&detail),
                        message_id_opt.clone(),
                    ));
                    return Ok(TurnOutcome {
                        stop_reason: StopReason::Refusal,
                        usage: Some(turn_usage),
                    });
                }
                // The run stopped without a normal result. Interruption is the one case this
                // client did not ask for: the turn is suspended and resumable, so continue it
                // instead of throwing the user's prompt away. Hosts interrupt runs for
                // transient reasons (shutdown sweeps, a raced cancel, a recovered process) and
                // none of those are the user's problem.
                "interrupted" if resumes < MAX_INTERRUPTED_RESUMES => {
                    resumes += 1;
                    sink(super::event::message_chunk(
                        self.session_id.clone(),
                        "\n\n[run interrupted; continuing]\n\n".to_string(),
                        message_id_opt.clone(),
                    ));
                    let resumed = runtime
                        .control
                        .resume(runtime.session.thread(), &turn_id, &control_id("resume"))
                        .await;
                    driver.finish().await;
                    match resumed {
                        Ok(_) => continue,
                        Err(error) => {
                            // The turn cannot be continued (another writer already settled
                            // it). Suspend rather than cancel: whatever the run did stays
                            // resumable.
                            let _ = runtime
                                .control
                                .suspend(runtime.session.thread(), &turn_id, &control_id("suspend"))
                                .await;
                            sink(super::event::message_chunk(
                                self.session_id.clone(),
                                format!(
                                    "Agent error: the interrupted run could not be continued \
                                     automatically: {error}. The turn was suspended; send a new \
                                     prompt to continue."
                                ),
                                message_id_opt.clone(),
                            ));
                            return Ok(TurnOutcome {
                                stop_reason: StopReason::EndTurn,
                                usage: Some(turn_usage),
                            });
                        }
                    }
                }
                "interrupted" => {
                    // Every resume was interrupted again. The environment, not the prompt, is
                    // failing; suspend it so the turn stays resumable and say so without
                    // refusing the turn at the protocol level.
                    let _ = runtime
                        .control
                        .suspend(runtime.session.thread(), &turn_id, &control_id("suspend"))
                        .await;
                    driver.finish().await;
                    sink(super::event::message_chunk(
                        self.session_id.clone(),
                        "Agent error: the run kept being interrupted before it finished. The \
                         turn was suspended; send a new prompt to continue."
                            .to_string(),
                        message_id_opt.clone(),
                    ));
                    return Ok(TurnOutcome {
                        stop_reason: StopReason::EndTurn,
                        usage: Some(turn_usage),
                    });
                }
                // A cancel this process did not issue (another writer stopped the turn).
                "cancelled" => {
                    driver.finish().await;
                    return Ok(TurnOutcome {
                        stop_reason: StopReason::Cancelled,
                        usage: Some(turn_usage),
                    });
                }
                // `waiting` (an external answer never arrived) and any other non-terminal
                // outcome. Report it and suspend the turn so the next prompt is not blocked.
                status if status != "completed" => {
                    let detail = current.error.clone().unwrap_or_else(|| {
                        format!(
                            "the run was {status} before it finished; the turn has been \
                             suspended, send a new prompt to continue"
                        )
                    });
                    let _ = runtime
                        .control
                        .suspend(runtime.session.thread(), &turn_id, &control_id("suspend"))
                        .await;
                    driver.finish().await;
                    sink(super::event::message_chunk(
                        self.session_id.clone(),
                        format!("Agent error: {detail}"),
                        message_id_opt.clone(),
                    ));
                    return Ok(TurnOutcome {
                        stop_reason: StopReason::Refusal,
                        usage: Some(turn_usage),
                    });
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
                        sink(super::event::message_chunk(
                            self.session_id.clone(),
                            "\n\n[some streamed output was dropped: the client could not keep \
                             up]"
                            .to_string(),
                            message_id_opt.clone(),
                        ));
                    }
                    driver.finish().await;
                    return Ok(TurnOutcome {
                        stop_reason: StopReason::EndTurn,
                        usage: Some(turn_usage),
                    });
                }
            }
        }
    }

    /// Drives the session's unfinished turns until the queue is empty.
    ///
    /// A prompt is the only thing that ticks this session, so turns a client queued while the
    /// previous prompt ran would otherwise sit in `queued` until someone asked again. The drain
    /// starts turns, it does not skip them: a paused head means the client paused the session
    /// and nothing behind it may run, and a cancel generation moved since admission means the
    /// client stopped — everything still queued keeps its place for the next prompt.
    async fn drain_queued(
        &self,
        plan: DrivePlan,
        sink: EventSink,
    ) -> std::result::Result<(), DriverError> {
        loop {
            if plan.cancel_epoch.load(Ordering::SeqCst) != plan.admitted_epoch
                || plan.closed.load(Ordering::SeqCst)
            {
                return Ok(());
            }
            let runtime = self
                .open_runtime()
                .await
                .map_err(DriverError::Unavailable)?;
            let Some(head) = unfinished_head(&runtime).await? else {
                return Ok(());
            };
            // A paused head is the client's own stop: running what is behind it would reorder
            // the conversation the client deliberately held.
            if head.status == TurnStatus::Paused {
                return Ok(());
            }
            // The outcome is per turn (a failed queued message must not swallow the rest) and
            // has already reached the client through the sink.
            let _ = self
                .drive_turn(runtime, head.turn_id, plan.clone(), Arc::clone(&sink))
                .await?;
        }
    }

    /// A notice the client may or may not advertise support for.
    fn advisory_notification(
        &self,
        supports_notices: bool,
        message: impl Into<String>,
    ) -> agent_client_protocol::v1::schema::SessionNotification {
        let message = message.into();
        if supports_notices {
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

    /// The context window of the live model, read once per turn.
    fn context_window(&self) -> u64 {
        self.agent
            .agent()
            .map(|agent| agent.model().context_window_size() as u64)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW as u64)
    }

    /// The usage update for the running turn; `used` is the tokens in context.
    fn usage_notification(
        &self,
        used: u64,
    ) -> agent_client_protocol::v1::schema::SessionNotification {
        usage_update(self.session_id.clone(), self.context_window(), used)
    }
    /// Opens (or reuses) the durable runtime of this session.
    ///
    /// Opening touches durable storage, which can fail transiently (a competing writer, an
    /// fsync hiccup). Those failures are not the user's fault and must not end a turn on the
    /// first attempt.
    async fn open_runtime(&self) -> std::result::Result<Arc<SessionRuntime>, String> {
        if let Some(runtime) = self.cached().await {
            return Ok(runtime);
        }
        let backend = self.durable_backend()?;
        let thread = self.durable_thread()?;
        let mut attempts = 0usize;
        let opened = loop {
            attempts += 1;
            match self
                .build_runtime(Arc::clone(&backend), thread.clone())
                .await
            {
                Ok(runtime) => break Arc::new(runtime),
                Err(error) if attempts < 3 => {
                    tracing::warn!(
                        session = %self.session_id,
                        %error,
                        "opening the durable runtime failed; retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error.to_string()),
            }
        };
        *self.runtime.lock().await = Some(Arc::clone(&opened));
        Ok(opened)
    }

    /// Binds the store to this agent: the adapter runs one `HarnessAgent` invocation per tick,
    /// and the session/thread creation is idempotent so a restart rebuilds the same identity.
    async fn build_runtime(
        &self,
        backend: Arc<dyn SessionBackend>,
        thread: ThreadKey,
    ) -> agentscope_durable::Result<SessionRuntime> {
        let agent = Arc::clone(&self.agent);
        let adapter = Arc::new(AgentScopeHarnessAdapter::new(move || Arc::clone(&agent)));
        let binding = ThreadBinding::new(
            DURABLE_HARNESS,
            DURABLE_DEFINITION,
            adapter.capabilities().state_version,
        )?;
        let owner = format!("acp-{}-{}", std::process::id(), control_id("run"));
        let mut runtime =
            DurableRuntime::new(Arc::clone(&backend), owner).with_adapter(DURABLE_HARNESS, adapter);
        // An offloaded tool outlives the turn that started it: the turn only holds a placeholder
        // result, so its record blocks every terminal status until somebody says what became of
        // the effect. The harness registry is the only authority on that, and the executor is
        // how the runtime asks (a call whose outcome nothing can confirm stays unknown, which
        // never lets the turn claim success).
        if let Some(registry) = self.agent.async_tool_registry() {
            runtime = runtime.with_executor(Arc::new(OffloadedToolEffects::new(
                Arc::clone(registry),
                self.session_id.to_string(),
            )));
        }
        runtime
            .ensure_session(SessionSpec {
                key: thread.session.clone(),
                primary_binding: binding,
                metadata: serde_json::Value::Null,
            })
            .await?;
        // A control command must not run under the owner the tick uses: `claim` would hand it
        // the very lease the invocation is writing under, and releasing it would fence the run.
        let control = Arc::new(DurableRuntime::new(
            Arc::clone(&backend),
            control_id("acp-control"),
        ));
        let journal: Arc<dyn ThreadJournal> = Arc::new(BackendThreadJournal::new(
            Arc::clone(&backend),
            thread.clone(),
        ));
        // Before any tick opens a recorder: a session that a dead process left with an unknown
        // tool outcome is otherwise refused at every open, permanently.
        reconcile_abandoned_tool_calls(&journal).await;
        Ok(SessionRuntime {
            session: Arc::new(DurableSession::new(runtime, thread)),
            control,
            journal,
            projection: tokio::sync::Mutex::new(None),
        })
    }
}

#[async_trait::async_trait]
#[cfg(feature = "durable")]
impl TurnDriver for DurableDriver {
    async fn run(
        &self,
        input: DriverInput,
        sink: EventSink,
    ) -> std::result::Result<TurnOutcome, DriverError> {
        self.run_turn(input, sink).await
    }

    async fn cancel(&self, _turn_id: &str) -> std::result::Result<(), DriverError> {
        Err(DriverError::Unavailable(
            "cancel is still routed through AcpAgentSession".to_string(),
        ))
    }

    async fn view(&self) -> std::result::Result<TurnView, DriverError> {
        Err(DriverError::Unavailable(
            "status is still read through AcpAgentSession".to_string(),
        ))
    }

    async fn replay(&self) -> std::result::Result<Vec<ReplayItem>, DriverError> {
        Err(DriverError::Unavailable(
            "session/load still reads the journal through AcpAgentSession".to_string(),
        ))
    }

    async fn resume(&self) -> std::result::Result<ResumeOutcome, DriverError> {
        Err(DriverError::Unavailable(
            "session/resume still runs through AcpAgentSession".to_string(),
        ))
    }

    fn guarantees(&self) -> Guarantees {
        Guarantees {
            cross_process: true,
            crash_atomic: true,
            multi_host: true,
        }
    }
}

/// Advances one turn until it settles or yields.
///
/// One `tick` is one model decision plus the tools it dispatched, so a turn needs as many ticks
/// as it takes decisions. The prompt above renders what the sink publishes and watches for
/// cancellation; this loop only keeps the thread moving. A paused or waiting turn is not ready,
/// so the loop hands it back instead of spinning on it.
#[cfg(feature = "durable")]
async fn drive_turn(session: Arc<DurableSession>, turn_id: String, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // A failed tick has already recorded its failure on the turn; the status below decides
        // whether there is more to do.
        let _ = session.tick().await;
        match session.turn(&turn_id).await {
            Ok(view)
                if view.status.terminal()
                    || matches!(
                        view.status,
                        TurnStatus::Paused | TurnStatus::Pausing | TurnStatus::Waiting
                    ) =>
            {
                return
            }
            Ok(_) => {}
            Err(_) => return,
        }
        tokio::time::sleep(TICK_PAUSE).await;
    }
}

/// The tick loop of one prompt attempt.
#[cfg(feature = "durable")]
struct TickLoop {
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[cfg(feature = "durable")]
impl TickLoop {
    fn spawn(session: Arc<DurableSession>, turn_id: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(drive_turn(session, turn_id, Arc::clone(&stop)));
        Self {
            stop,
            task: Some(task),
        }
    }

    /// Stops the loop and waits for the tick in flight to end.
    ///
    /// The wait is bounded because a tick can be stuck in a tool that blocks its thread; past
    /// the grace the loop is left to end on its own rather than aborting it, because a tick
    /// dropped mid-invocation leaves its turn `Running` until the lease expires.
    async fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            if tokio::time::timeout(DRIVER_GRACE, task).await.is_err() {
                tracing::warn!("the durable tick loop did not end within the grace; leaving it");
            }
        }
    }
}

#[cfg(feature = "durable")]
impl Drop for TickLoop {
    fn drop(&mut self) {
        // Only reached on a path that returned without `finish`: the loop is asked to stop, and
        // the handle is dropped (the task detaches and ends after its current tick).
        self.stop.store(true, Ordering::SeqCst);
        self.task.take();
    }
}

/// A single ACP session backed by a [`HarnessAgent`].
/// The state a turn owns, separate from the protocol state.
///
/// These are the fields `run_prompt_inner` needs beyond the store: the cancellation generation a
/// turn was admitted under, the lock that serializes prompts, the closed flag it checks while
/// waiting for that lock, the "a prompt is running" flag, and the usage the turn reports back.
/// Grouping them is the first half of design doc P1.5: once the body moves into `DurableDriver`,
/// it takes this one value instead of five session fields.
#[derive(Clone)]
pub(crate) struct TurnState {
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
    /// Token usage accumulated during the turn that is running (reset per turn),
    /// reported back as `PromptResponse.usage`.
    usage: ChatUsage,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            cancel_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            prompt_lock: Arc::new(tokio::sync::Mutex::new(())),
            closed: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            usage: ChatUsage::default(),
        }
    }
}

/// Which driver a session runs its turns through.
///
/// Read from `AGENTSCOPE_TURN_DRIVER` when the session is built. Any value other than `native`
/// keeps the durable runtime: a misspelled or unset variable must not silently change where a
/// session's history lives. A build without the durable feature has no such choice: there
/// `native` is the only driver, and the variable is ignored.
#[cfg(feature = "durable")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnDriverKind {
    Durable,
    Native,
}

#[cfg(feature = "durable")]
impl TurnDriverKind {
    fn from_env() -> Self {
        Self::parse(std::env::var("AGENTSCOPE_TURN_DRIVER").ok().as_deref())
    }

    /// Parses the selector. Only `native` (any case) moves a session off the durable runtime:
    /// an unset value, a typo or an empty string must not silently change where a session's
    /// history lives, because the two stores are not interchangeable after the fact.
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some(value) if value.eq_ignore_ascii_case("native") => Self::Native,
            _ => Self::Durable,
        }
    }
}

/// The native driver for a session: its history lives beside the session config, under the cwd
/// the client sent, so `session/load` rebuilds it from that same cwd.
fn native_driver(
    agent: &Arc<HarnessAgent>,
    cwd: &PathBuf,
    session_id: &SessionId,
) -> Arc<dyn TurnDriver> {
    Arc::new(crate::native_driver::NativeDriver::open(
        &cwd.join(".agentscope"),
        session_id.clone(),
        Some(Arc::clone(agent)),
    ))
}

#[cfg(all(test, feature = "durable"))]
mod driver_kind_tests {
    use super::TurnDriverKind;

    #[test]
    fn only_native_moves_a_session_off_the_durable_runtime() {
        assert_eq!(
            TurnDriverKind::parse(Some("native")),
            TurnDriverKind::Native
        );
        assert_eq!(
            TurnDriverKind::parse(Some("NATIVE")),
            TurnDriverKind::Native
        );
        assert_eq!(
            TurnDriverKind::parse(Some("durable")),
            TurnDriverKind::Durable
        );
        assert_eq!(
            TurnDriverKind::parse(Some("nativ")),
            TurnDriverKind::Durable
        );
        assert_eq!(TurnDriverKind::parse(Some("")), TurnDriverKind::Durable);
        assert_eq!(TurnDriverKind::parse(None), TurnDriverKind::Durable);
    }
}

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
    /// The state a turn owns: the cancellation generation, the lock that serializes prompts,
    /// the closed/running flags and the usage of the turn in flight. Grouped so the execution
    /// body can move behind `TurnDriver` as one value (design doc P1.5).
    turn: TurnState,
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
    /// The durable runtime of this ACP session, opened on the first prompt.
    ///
    /// Reusing one handle keeps the adapter, the store connection and the event sink warm across
    /// prompts; opening per prompt would rebuild the store handle and re-read the thread state.
    /// The turns themselves live in the store, so a restart rebuilds this from what a tick
    /// committed.
    /// The driver this session runs turns through.
    driver: Arc<dyn TurnDriver>,
    /// `Some` while the session runs on durable: the protocol paths that need the store
    /// (`respond`, `close`, `session/load`) read it directly. `None` on the native driver,
    /// which keeps no runtime and no suspended turn.
    #[cfg(feature = "durable")]
    durable: Option<Arc<DurableDriver>>,
    /// Manages MCP client connections for this session (stdio servers declared
    /// by the ACP client via `mcp_servers`). Shared across clones so tools keep
    /// working after `register_session` write-back.
    mcp_manager: Arc<agentscope_harness::agent::McpClientManager>,
    /// Server names already bridged into the toolkit this session (dedup).
    bridged_servers: Arc<Mutex<Vec<String>>>,
    /// ACP-transport MCP servers declared by the client before a connection was bound.
    ///
    /// `session/new` applies `mcp_servers` while `client` is still `None` (the connection is
    /// bound on the first prompt), and the MCP-over-ACP bridge needs it. Remembering the
    /// declaration here lets the bridge retry as soon as the connection exists.
    pending_acp_servers: Arc<Mutex<Vec<agent_client_protocol::v1::schema::McpServerAcp>>>,
    /// Capabilities the connected client advertised at `initialize`; several
    /// `session/update` variants are only legal when the matching capability was
    /// advertised, so every optional update is gated on this.
    support: ClientSupport,

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
    subagent_agents: Arc<Mutex<HashMap<String, Weak<dyn agentscope_core::agent::CallableAgent>>>>,
    /// ACP mode the parent session currently runs in, shared with [`SubagentPermissionHook`].
    ///
    /// A child spawned mid-turn must inherit the mode in force *then*, not the one the session
    /// was built with.
    subagent_mode: Arc<Mutex<String>>,
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
#[cfg(feature = "durable")]
fn prompt_idempotency_key(message: &Msg, accepted_at_ms: i64) -> String {
    let mut text = String::new();
    for block in message.content.iter() {
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
        let agent = Arc::new(agent);
        // With the durable feature the choice comes from `AGENTSCOPE_TURN_DRIVER`; without it
        // there is one driver, and nothing to keep a store for.
        #[cfg(feature = "durable")]
        let (driver, durable): (Arc<dyn TurnDriver>, Option<Arc<DurableDriver>>) =
            match TurnDriverKind::from_env() {
                TurnDriverKind::Durable => {
                    let durable =
                        Arc::new(DurableDriver::new(Arc::clone(&agent), session_id.clone()));
                    (Arc::clone(&durable) as Arc<dyn TurnDriver>, Some(durable))
                }
                TurnDriverKind::Native => (native_driver(&agent, &cwd, &session_id), None),
            };
        #[cfg(not(feature = "durable"))]
        let driver: Arc<dyn TurnDriver> = native_driver(&agent, &cwd, &session_id);
        let mut session = Self {
            session_id,
            cwd,
            agent,
            config: Arc::new(Mutex::new(SessionConfig::default())),
            models,
            descriptor: Arc::new(Mutex::new(descriptor)),
            modes,
            model_builder: None,
            applied_model: actual_model,
            usage: ChatUsage::default(),
            last_input_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            client: Arc::new(Mutex::new(None)),
            elicitation_slot: Arc::new(Mutex::new(None)),
            permission_store,
            mcp_manager: Arc::new(agentscope_harness::agent::McpClientManager::new()),
            driver,
            #[cfg(feature = "durable")]
            durable,
            turn: TurnState::default(),
            bridged_servers: Arc::new(Mutex::new(Vec::new())),
            pending_acp_servers: Arc::new(Mutex::new(Vec::new())),
            support: ClientSupport::new(),

            additional_directories: Vec::new(),
            config_path,
            title: None,
            archived,
            updated_at: Arc::new(Mutex::new(None)),
            subagent_events: Arc::new(Mutex::new(None)),
            subagent_children: Arc::new(Mutex::new(HashMap::new())),
            subagent_agents: Arc::new(Mutex::new(HashMap::new())),
            subagent_mode: Arc::new(Mutex::new(config::default_mode().to_string())),
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
    /// Built from the id alone, so both drivers share it: nothing here is durable state.
    fn session_context(&self) -> agentscope_core::agent::RuntimeContext {
        agentscope_core::agent::RuntimeContext::new().with_session_id(self.session_id.to_string())
    }

    /// The durable driver, or a message that names what the caller asked for and why it is
    /// absent: the native driver keeps no journal, no runtime and no suspended turn.
    #[cfg(feature = "durable")]
    fn durable_store(&self) -> std::result::Result<&Arc<DurableDriver>, String> {
        self.durable.as_ref().ok_or_else(|| {
            "this session runs on the native driver, which keeps no durable store".to_string()
        })
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

    /// Replaces the advertised model list (a provider config change).
    ///
    /// A session that lost its selected model (the provider was removed or renamed) falls back
    /// to the first entry: `config_option_update` must never carry a `currentValue` that is not
    /// one of the options, or the client renders an empty picker. The live agent keeps the model
    /// instance it was built with until the next prompt resolves the new id.
    pub fn set_models(&mut self, models: Vec<ModelChoice>) {
        self.models = models;
        let selected = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .model
            .clone();
        if !self.models.is_empty() && !self.has_model(&selected) {
            let fallback = self.models[0].id.clone();
            self.set_model(fallback);
        }
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
    ///
    /// Only the durable driver can suspend a turn in its store, so only it has something to
    /// answer: a native turn is either running in this process or already over.
    #[cfg(feature = "durable")]
    pub async fn respond(
        &self,
        answers: std::collections::BTreeMap<String, String>,
    ) -> std::result::Result<(), String> {
        let durable = self.durable_store()?.clone();
        let runtime = {
            let slot = durable.runtime().lock().await;
            match slot.as_ref() {
                Some(runtime) => Arc::clone(runtime),
                None => return Err("the session runtime is not open".to_string()),
            }
        };
        let turn = runtime
            .session
            .active()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the session has no turn to answer".to_string())?;
        // The durable wait key *is* the interaction id, so an answer is the signal the waiting
        // turn folds in at its next boundary; answering a turn that waits on something else is
        // a conflict instead of a silent drop.
        for (request_id, answer) in answers {
            let value = serde_json::json!({ "text": answer });
            runtime
                .session
                .respond(&turn.turn_id, &request_id, &control_id("answer"), value)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Folds one message into the turn this session is executing and answers which turn it was.
    ///
    /// Durable accepts a steer only for the active turn a writer has already claimed: the
    /// admission window right after a prompt is accepted (`Queued`, `epoch == 0`) answers
    /// `CONFLICT`, so that window is retried before the client is told the turn is not running.
    /// A retried steer with the same `request_id` reaches the same signal, never the model twice.
    #[cfg(feature = "durable")]
    pub async fn steer(
        &self,
        text: String,
        request_id: &str,
    ) -> std::result::Result<String, String> {
        let durable = self.durable_store()?.clone();
        let runtime = {
            let slot = durable.runtime().lock().await;
            match slot.as_ref() {
                Some(runtime) => Arc::clone(runtime),
                None => return Err(crate::ext::NO_ACTIVE_TURN.to_string()),
            }
        };
        let deadline = std::time::Instant::now() + STEER_SETTLE_GRACE;
        loop {
            let turn = runtime
                .session
                .active()
                .await
                .map_err(|error| error.to_string())?;
            let Some(turn) = turn else {
                return Err(crate::ext::NO_ACTIVE_TURN.to_string());
            };
            match runtime
                .control
                .steer(
                    runtime.session.thread(),
                    &turn.turn_id,
                    request_id,
                    text.clone(),
                )
                .await
            {
                Ok(_) => return Ok(turn.turn_id),
                // Admitted but not claimed yet: wait for the writer, not for the user to retype.
                Err(error)
                    if error.code() == agentscope_durable::ErrorCode::Conflict
                        && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    /// Admits one message as its own turn and answers before it runs.
    ///
    /// The turn is durable the moment this returns: a crash before the next prompt leaves it
    /// `queued`, and that prompt drains it. A message queued while another turn runs never
    /// touches that turn — the runtime only ever advances the oldest unfinished one.
    #[cfg(feature = "durable")]
    pub async fn queue(
        &self,
        content: Vec<ContentBlock>,
        request_id: &str,
    ) -> std::result::Result<(String, u32), String> {
        let message = Self::convert_prompt_blocks(content, self.selected_descriptor().as_deref())?;
        let runtime = self.durable_store()?.open_runtime().await?;
        // The recorder refuses to open a journal that still holds a dispatch without a result;
        // admitting a turn below writes one.
        settle_finished_turn_uncertainties_cached(&runtime).await;
        let input = serde_json::to_value(&message).map_err(|error| error.to_string())?;
        let request_id = format!("queue-{request_id}");
        let receipt = match runtime.session.submit(&request_id, input).await {
            Ok(receipt) => receipt,
            // A retry rebuilds the message, and every message carries a fresh id, so the second
            // attempt's admission digest differs from the first. The admission record is the
            // authority on what this request id already admitted: answer with that turn instead
            // of failing a retry the client cannot influence.
            Err(error) if error.code() == agentscope_durable::ErrorCode::Conflict => {
                match runtime.session.admission(&request_id).await {
                    Ok(Some(receipt)) => receipt,
                    _ => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        };
        let position = unfinished_position(&runtime, &receipt.turn_id).await?;
        Ok((receipt.turn_id, position))
    }

    /// The session's unfinished turns, in the order the runtime will run them.
    #[cfg(feature = "durable")]
    pub async fn queued(&self) -> std::result::Result<Vec<crate::ext::ExtQueueItem>, String> {
        let runtime = self.durable_store()?.open_runtime().await?;
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = runtime
                .session
                .turns(cursor.as_deref(), 1000)
                .await
                .map_err(|error| error.to_string())?;
            for turn in &page.items {
                if turn.status.terminal() {
                    continue;
                }
                items.push(crate::ext::ExtQueueItem {
                    turn_id: turn.turn_id.clone(),
                    position: items.len() as u32 + 1,
                    status: queued_status_name(turn.status).to_string(),
                });
            }
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(items)
    }

    /// Removes a turn from the queue, before it runs.
    ///
    /// Answers `false` when the turn had already started (or settled): stopping the running turn
    /// is `session/cancel`, so a client removing an obsolete bubble cannot accidentally stop the
    /// answer it just started.
    #[cfg(feature = "durable")]
    pub async fn remove_queued(
        &self,
        turn_id: &str,
        request_id: &str,
    ) -> std::result::Result<bool, String> {
        let runtime = self.durable_store()?.open_runtime().await?;
        let turn = runtime
            .session
            .turn(turn_id)
            .await
            .map_err(|error| error.to_string())?;
        if turn.status != TurnStatus::Queued {
            return Ok(false);
        }
        runtime
            .session
            .cancel(turn_id, &format!("queue-remove-{request_id}"))
            .await
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    /// Answers an outstanding interaction and continues the turn without a new user prompt.
    ///
    /// A build without the durable driver can never have a suspended turn: the native driver
    /// runs a turn to its end inside this process, so there is nothing on disk to answer.
    #[cfg(not(feature = "durable"))]
    pub async fn respond(
        &self,
        _answers: std::collections::BTreeMap<String, String>,
    ) -> std::result::Result<(), String> {
        Err("this build has no durable driver, so no turn is waiting for an answer".to_string())
    }

    /// Folds one message into the turn this session is executing.
    ///
    /// A build without the durable driver runs a turn to its end inside this process and keeps
    /// no interruption point, so there is nothing a steer could be folded into.
    #[cfg(not(feature = "durable"))]
    pub async fn steer(
        &self,
        _text: String,
        _request_id: &str,
    ) -> std::result::Result<String, String> {
        Err("this build has no durable driver, so there is no turn to steer".to_string())
    }

    /// Queues a message as its own turn.
    ///
    /// A build without the durable driver runs a turn to its end inside this process, so a
    /// message cannot wait in a store that survives the request that admitted it.
    #[cfg(not(feature = "durable"))]
    pub async fn queue(
        &self,
        _content: Vec<ContentBlock>,
        _request_id: &str,
    ) -> std::result::Result<(String, u32), String> {
        Err("this build has no durable driver, so a message cannot be queued".to_string())
    }

    /// The session's unfinished turns; nothing can outlive a native turn, so the queue is empty.
    #[cfg(not(feature = "durable"))]
    pub async fn queued(&self) -> std::result::Result<Vec<crate::ext::ExtQueueItem>, String> {
        Ok(Vec::new())
    }

    /// Removes a turn from the queue.
    #[cfg(not(feature = "durable"))]
    pub async fn remove_queued(
        &self,
        _turn_id: &str,
        _request_id: &str,
    ) -> std::result::Result<bool, String> {
        Err("this build has no durable driver, so nothing is queued".to_string())
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
        // Publish the mode to the subagent bridge before the next spawn: a child must run under
        // the same rules as the session that spawns it.
        *self
            .subagent_mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode.clone();
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
        agents: Arc<Mutex<HashMap<String, Weak<dyn agentscope_core::agent::CallableAgent>>>>,
    ) {
        self.subagent_agents = agents;
    }

    /// Shares the ACP mode slot the subagent permission bridge reads at spawn time.
    ///
    /// Must be installed before [`Self::apply_mode`] runs against the shared store so the slot
    /// carries the session's persisted mode rather than the default.
    pub fn set_subagent_mode(&mut self, mode: Arc<Mutex<String>>) {
        self.subagent_mode = mode;
        let current = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mode
            .clone();
        *self
            .subagent_mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = current;
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
        let agents = Arc::clone(&self.subagent_agents);
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
                        // The weak entry is already dead by now (the spawn tool dropped the
                        // agent); dropping the key keeps the map from growing for the session's
                        // whole life.
                        agents
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
        // Dropping the entry as we read it matters: a stale `sub-*` id must not resurrect a
        // finished child (or cancel a later turn that reused the id).
        let agent = self
            .subagent_agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id)
            .and_then(|weak| weak.upgrade());
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

    /// Parks a declaration until [`apply_pending_acp_servers`](Self::apply_pending_acp_servers)
    /// can bridge it (dedup by `server_id`, so repeated `session/new` does not pile up).
    fn remember_pending_acp_server(&self, acp: &agent_client_protocol::v1::schema::McpServerAcp) {
        let mut pending = self
            .pending_acp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.iter().any(|p| p.server_id == acp.server_id) {
            return;
        }
        pending.push(acp.clone());
    }

    /// Bridges the ACP-transport servers parked at `session/new`, now that a client
    /// connection is bound. Already-bridged servers are skipped, so calling this on every
    /// prompt is cheap and idempotent.
    pub async fn apply_pending_acp_servers(&self) {
        let pending: Vec<agent_client_protocol::v1::schema::McpServerAcp> = {
            let mut slot = self
                .pending_acp_servers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_empty() {
                return;
            }
            std::mem::take(&mut *slot)
        };
        for acp in pending {
            self.apply_acp_mcp_server(&acp).await;
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
            // `session/new` applies declarations before the first prompt binds the client,
            // so this is the normal path rather than an error: park it and retry later.
            self.remember_pending_acp_server(acp);
            tracing::debug!(
                session = %self.session_id,
                server = %name,
                "MCP-over-ACP server deferred until a client connection is bound"
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
        self.turn.cancel_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Closes the session: cancels the active prompt and prevents it from being re-registered.
    pub fn close(&self) {
        self.turn.closed.store(true, Ordering::SeqCst);
        self.cancel();
        // Subagents outlive the prompt that spawned them: closing the session must not leave
        // them running with nobody to render them (or to answer their permission requests).
        self.cancel_subagents();
        // Release the dispatcher unless a prompt currently holds the handle; dropping the
        // last clone stops it after that prompt unwinds.
        #[cfg(feature = "durable")]
        if let Some(durable) = &self.durable {
            if let Ok(mut runtime) = durable.runtime().try_lock() {
                runtime.take();
            }
        }
    }

    /// Persists the live background task handles and stops their maintenance tasks before the
    /// process exits.
    ///
    /// `close` cannot await, so the host calls this alongside it. Without it a task whose
    /// record write failed earlier becomes "not found" after the restart even though its result
    /// was produced, and the heartbeat keeps rewriting the store after the session is gone.
    pub(crate) async fn flush_background_tasks(&self) {
        self.agent.task_repository().shutdown().await;
    }

    /// Cancels the active prompt and waits for its run to settle, so the next prompt cannot
    /// start while the previous execution still owns the journal.
    pub async fn cancel_and_wait(&self, timeout: std::time::Duration) -> bool {
        self.cancel();
        // `cancel` only moves the generation (it is synchronous), so the child traversal has to
        // happen here, where the caller already awaits: a cancelled parent must not leave the
        // subagent tasks it spawned running with nobody to receive their results.
        let session_id = self.session_id.to_string();
        self.agent
            .task_repository()
            .cancel_session_tasks(&self.session_context(), &session_id)
            .await;
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
        self.turn.closed.load(Ordering::SeqCst)
    }

    /// Whether a prompt is running right now.
    pub fn is_running(&self) -> bool {
        self.turn.running.load(Ordering::SeqCst)
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
        F: Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync + 'static,
    {
        if let Some(stop) = self.run_prompt_shortcut(&content, &emit).await {
            return stop;
        }
        let stop = self
            .run_prompt_inner(TurnInput::Prompt(content), emit, cancellation)
            .await;
        self.report_turn_memory(stop);
        stop
    }

    /// Answers a host-level prompt shortcut without starting a model turn.
    ///
    /// `/compact` is the user asking for a compaction, not for an answer: routing it through
    /// a turn would pay a full model request (compacted history included) just to have the
    /// middleware do the same work one step later. Returns `None` when the prompt is not a
    /// shortcut.
    async fn run_prompt_shortcut<F>(
        &mut self,
        content: &[ContentBlock],
        emit: &F,
    ) -> Option<StopReason>
    where
        F: Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync + 'static,
    {
        if !is_compact_command(content) {
            return None;
        }
        // Serialize with a running turn: rewriting the history under an in-flight request
        // would make the model answer from a prefix the client already dropped.
        let _guard = Arc::clone(&self.turn.prompt_lock).lock_owned().await;
        let report = match self.agent.compact_now().await {
            Ok(None) => {
                "Nothing to compact: context compaction is unavailable in this session or the \
                 history is empty."
                    .to_string()
            }
            Ok(Some(outcome)) => {
                let metrics = &outcome.metrics;
                let shrank = metrics
                    .after_msg_count
                    .is_some_and(|after| after < metrics.before_msg_count);
                if shrank && self.support.supports_compaction() {
                    // The same capability-gated updates the in-turn path sends: a client that
                    // renders a compaction card sees the summary once here too, instead of only
                    // a plain message.
                    for notification in event::compaction_notifications(
                        self.session_id.clone(),
                        &metrics.compaction_id,
                        &outcome.summary,
                    ) {
                        emit(notification);
                    }
                }
                match metrics.after_msg_count {
                    Some(after) if shrank => format!(
                        "Context compacted: {} → {} messages (~{} → ~{} tokens).",
                        metrics.before_msg_count,
                        after,
                        metrics.before_token_count,
                        metrics.after_token_count.unwrap_or(0)
                    ),
                    _ => format!(
                        "Nothing to compact yet: all {} messages fit inside the preserved tail.",
                        metrics.before_msg_count
                    ),
                }
            }
            Err(error) => format!("Context compaction failed: {error}"),
        };
        emit(event::message_chunk(self.session_id.clone(), report, None));
        Some(StopReason::EndTurn)
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

    /// Streams the `session/update` replay a client needs after `session/load`.
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
    pub async fn replay_notifications<F>(&self, mut emit: F)
    where
        F: FnMut(agent_client_protocol::v1::schema::SessionNotification) + Send,
    {
        // The transcript comes from the durable journal this session writes. The harness journal
        // store is no longer the writer, so reading it would replay an empty conversation.
        //
        // A native session has no journal at all: its history is the transcript it wrote
        // itself, replayed through the same renderer so a client cannot tell the difference.
        #[cfg(not(feature = "durable"))]
        self.replay_native_history(&mut emit).await;
        #[cfg(feature = "durable")]
        {
            let Some(durable) = self.durable.clone() else {
                self.replay_native_history(&mut emit).await;
                return;
            };
            let cached = durable
                .runtime()
                .lock()
                .await
                .as_ref()
                .map(|runtime| Arc::clone(&runtime.journal));
            let journal = match cached {
                Some(journal) => journal,
                None => match durable.durable_journal() {
                    Ok(journal) => journal,
                    Err(error) => {
                        tracing::warn!(
                            session = %self.session_id,
                            %error,
                            "cannot open the durable journal; session/load will replay nothing"
                        );
                        return;
                    }
                },
            };
            // The transcript read is also the readability check: it walks the committed prefix and
            // fails on a damaged journal, so a separate projection pass would fold the same events a
            // second time and double the memory a resume peaks at.
            let transcript = match agentscope_core::journal::transcript(&*journal).await {
                Ok(transcript) => transcript,
                Err(error) => {
                    tracing::warn!(
                        session = %self.session_id,
                        %error,
                        "cannot read the transcript; session/load will replay nothing"
                    );
                    return;
                }
            };
            self.replay_messages(transcript.messages, &mut emit);
            for card in self.replay_subagent_cards().await {
                emit(card);
            }
        }
    }

    /// Replays what the native driver recorded, through the same renderer the live turn uses.
    async fn replay_native_history<F>(&self, emit: &mut F)
    where
        F: FnMut(agent_client_protocol::v1::schema::SessionNotification) + Send,
    {
        let mut messages = Vec::new();
        match self.driver.replay().await {
            Ok(items) => messages.extend(items.into_iter().filter_map(|item| match item {
                crate::turn_driver::ReplayItem::Message(message) => Some(message),
                crate::turn_driver::ReplayItem::TurnBoundary { .. } => None,
            })),
            Err(error) => {
                tracing::warn!(
                    session = %self.session_id,
                    %error,
                    "cannot read the native history; session/load will replay nothing"
                );
            }
        }
        self.replay_messages(messages, emit);
        for card in self.replay_subagent_cards().await {
            emit(card);
        }
    }

    /// Renders committed messages as the `session/update`s `session/load` replays.
    ///
    /// Shared by both drivers: the durable one hands over its journal
    /// transcript, the native one the messages its own history stored, and a client must not be
    /// able to tell which.
    fn replay_messages<F>(
        &self,
        messages: impl IntoIterator<Item = agentscope_core::message::Msg>,
        emit: &mut F,
    ) where
        F: FnMut(agent_client_protocol::v1::schema::SessionNotification) + Send,
    {
        use agent_client_protocol::v1::schema::{
            ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCall, ToolCallStatus,
        };
        use agentscope_core::message::{ContentBlock as CoreBlock, MsgRole};
        // Each notification is handed over as it is built: a loaded session has no reason to
        // hold the whole replay twice (once as the transcript, once as a vector of chunks).
        for message in messages {
            // One message id per historical message: clients group chunks by it exactly as
            // they do for a live turn, so a replayed turn renders as one bubble.
            let message_id = MessageId::new(format!("history-{}", message.id));
            for block in message.content.iter() {
                let update = match (message.role, block) {
                    (MsgRole::User, CoreBlock::Text(text)) => {
                        Some(SessionUpdate::UserMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(
                                text.text.clone(),
                            )))
                            .message_id(message_id.clone()),
                        ))
                    }
                    (MsgRole::Assistant, CoreBlock::Text(text)) => {
                        Some(SessionUpdate::AgentMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(
                                text.text.clone(),
                            )))
                            .message_id(message_id.clone()),
                        ))
                    }
                    (MsgRole::Assistant, CoreBlock::ToolUse(tool)) => {
                        Some(SessionUpdate::ToolCall(
                            ToolCall::new(tool.id.clone(), event::human_title(&tool.name))
                                .name(tool.name.clone())
                                .kind(event::tool_kind_for(&tool.name))
                                // A committed tool call already ran (or its turn ended):
                                // replaying it as pending would leave a spinner forever.
                                .status(ToolCallStatus::Completed)
                                .raw_input(serde_json::Value::Object(tool.input.clone())),
                        ))
                    }
                    // Thinking and tool results are not replayed: the transcript view owns the
                    // message order, and the tool call above already carries its arguments.
                    _ => None,
                };
                if let Some(update) = update {
                    emit(SessionNotification::new(self.session_id.clone(), update));
                }
            }
        }
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
    ///
    /// Two details make the replay match what the client saw live:
    /// - the child id is the one persisted with the record (`sub-<uuid>`), not one derived from
    ///   the task id, so a card is not duplicated on every reload;
    /// - a child that ran several `agent_send` turns has one card, not one per turn (the live
    ///   client reuses the card it was announced with).
    async fn replay_subagent_cards(
        &self,
    ) -> Vec<agent_client_protocol::v1::schema::SessionNotification> {
        /// Guard against a corrupt/cyclic task graph. Nested delegation is bounded by the session
        /// configuration (one level by default), so this is never reached in practice.
        const MAX_REPLAY_DEPTH: usize = 4;

        let ctx = self.session_context();
        let repo = self.agent.task_repository();
        let mut notifications = Vec::new();
        let mut owners = vec![self.session_id.to_string()];
        for _ in 0..MAX_REPLAY_DEPTH {
            let mut children = Vec::new();
            for owner in std::mem::take(&mut owners) {
                for card in fold_replay_cards(repo.list_tasks(&ctx, &owner, None).await) {
                    let meta = event::SubagentMeta {
                        started_at: Some(rfc3339_millis(card.started_at)),
                        completed_at: Some(rfc3339_millis(card.completed_at)),
                        // The model and the label a live spawn reports are not part of the
                        // record.
                        model: None,
                        role: Some(card.title.clone()),
                    };
                    notifications.push(event::subagent_started(
                        &SessionId::new(owner.clone()),
                        &card.child,
                        &card.title,
                        &format!("agent: {}", card.title),
                        Some(meta.clone()),
                    ));
                    let (outcome, error) = match card.status {
                        TaskStatus::Completed => ("success", None),
                        TaskStatus::Cancelled => ("cancelled", None),
                        _ => ("error", card.error),
                    };
                    notifications.push(event::subagent_idle(
                        &SessionId::new(owner.clone()),
                        &card.child,
                        Some(event::stop_reason_for(outcome, error.as_deref())),
                        Some(meta),
                    ));
                    children.push(card.child);
                }
            }
            if children.is_empty() {
                break;
            }
            owners = children;
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
        F: Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync + 'static,
    {
        agentscope_core::diag::mark("acp:prompt_enter");
        if let Some(stop) = self.run_prompt_shortcut(&content, &emit).await {
            return stop;
        }
        let stop = self
            .run_prompt_inner(TurnInput::Prompt(content), emit, None)
            .await;
        agentscope_core::diag::mark("acp:prompt_done");
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
        F: Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync + 'static,
    {
        // The turn's own cancellation generation, captured *before* the admission wait.
        //
        // A cancel is addressed to the session, not to a single request: a client that pressed
        // Stop while its prompt was still queued behind the previous turn expects that prompt to
        // stop, not to start running once the lock is free. Capturing the generation after the
        // wait made such a cancel indistinguishable from "this turn started after the cancel"
        // and silently ignored it, so the prompt ran (and billed) anyway.
        crate::mem::note_activity();
        agentscope_core::diag::mark("acp:run_enter");
        let cancel_epoch = Arc::clone(&self.turn.cancel_epoch);
        let my_epoch = cancel_epoch.load(Ordering::SeqCst);
        let closed = Arc::clone(&self.turn.closed);
        // One prompt turn per session. A client may send a follow-up before the previous
        // response arrives (a retry, a Stop followed by a new message); without this lock the
        // second run raced the first and was rejected as `turn_unfinished`, losing the input.
        //
        // The wait is cancellable and bounded: a stuck previous turn must not make every later
        // prompt hang forever behind an un-cancellable lock.
        let _prompt_guard = tokio::select! {
            guard = Arc::clone(&self.turn.prompt_lock).lock_owned() => guard,
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
        self.turn.running.store(true, Ordering::SeqCst);
        let _running = RunningPrompt(Arc::clone(&self.turn.running));
        // Turn-level usage: reset at the start of each prompt turn, accumulate
        // from every model call, and report back as `PromptResponse.usage`.
        self.turn.usage = ChatUsage::default();
        // A wakeup exists only to deliver background results; it must never monopolize the
        // session while a real user prompt waits behind it.
        let wakeup_turn = matches!(&input, TurnInput::Wakeup);

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
        // Drive the turn through the durable runtime instead of calling the core loop
        // directly: the runtime owns the store, restores the committed state from the
        // checkpoint before the run and admits control. This client only renders — events
        // arrive through the sink below, and the tick loop is the only writer.
        // The protocol layer keeps the input, the client's capabilities and the notice channel;
        // the driver owns everything that touches the store and the tick loop.
        let caps = PromptCaps {
            supports_notices: self.support.supports_notices(),
            supports_plan_operations: self.support.supports_plan_operations(),
            supports_compaction: self.support.supports_compaction(),
            last_input_tokens: Arc::clone(&self.last_input_tokens),
        };
        let sink: EventSink = Arc::new(move |notification| emit(notification));
        // A prompt is also the drain for what was queued behind it: nothing else ticks this
        // session, so a turn admitted while the previous prompt ran would sit in `queued` until
        // someone asked again. Leftovers (a process that died between queueing and running) go
        // first, so the queue keeps its order. A failure to drive them is a transport problem
        // and must not swallow the prompt itself.
        #[cfg(feature = "durable")]
        let drain = DrivePlan {
            cancel_epoch: Arc::clone(&cancel_epoch),
            admitted_epoch: my_epoch,
            closed: Arc::clone(&closed),
            cancellation: None,
            wakeup: false,
            caps: caps.clone(),
        };
        #[cfg(feature = "durable")]
        if let Some(durable) = &self.durable {
            if let Err(error) = durable.drain_queued(drain.clone(), Arc::clone(&sink)).await {
                tracing::warn!(session = %self.session_id, %error, "could not drain the queue");
            }
        }
        let stop = match self
            .driver
            .run(
                DriverInput {
                    message,
                    wakeup: wakeup_turn,
                    cancel_epoch: Arc::clone(&cancel_epoch),
                    admitted_epoch: my_epoch,
                    closed,
                    cancellation,
                    caps,
                },
                Arc::clone(&sink),
            )
            .await
        {
            Ok(outcome) => {
                if let Some(usage) = outcome.usage.clone() {
                    self.accumulate_turn_usage(&usage);
                }
                outcome.stop_reason
            }
            Err(error) => {
                sink(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: {error}"),
                    None,
                ));
                StopReason::Refusal
            }
        };
        // Whatever the client queued while this prompt ran goes next, in the same request: the
        // client stays busy, and a prompt is the only place that ticks.
        #[cfg(feature = "durable")]
        if let Some(durable) = &self.durable {
            if let Err(error) = durable.drain_queued(drain, Arc::clone(&sink)).await {
                tracing::warn!(session = %self.session_id, %error, "could not drain the queue");
            }
        }
        stop
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
        self.turn.usage.input_tokens += usage.input_tokens;
        self.turn.usage.output_tokens += usage.output_tokens;
        self.turn.usage.cached_tokens += usage.cached_tokens;
    }

    /// Builds the ACP `Usage` for the running turn, or `None` when no model
    /// call contributed usage (so an empty turn does not report a bogus 0).
    pub fn turn_usage_response(&self) -> Option<agent_client_protocol::v1::schema::Usage> {
        turn_usage_to_response(&self.turn.usage)
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

/// One subagent card reconstructed from durable task records.
struct ReplayCard {
    /// The child session id the live client saw (`sub-<uuid>`), or `sub-<task id>` for records
    /// written before the id was persisted.
    child: String,
    title: String,
    started_at: u64,
    completed_at: u64,
    /// Creation time of the turn whose state the card reports.
    last_seen: u64,
    status: TaskStatus,
    error: Option<String>,
}

/// Folds terminal task records into one card per child session.
///
/// A child that ran several `agent_send` turns leaves one task per turn. The live client reused
/// the card it was announced with, so the replay must do the same: one card spanning the whole
/// life, reporting the state of the most recent turn. Non-terminal tasks are dropped — the
/// previous process left them without a runner or a result, so any state would be invented.
fn fold_replay_cards(tasks: Vec<Arc<BackgroundTask>>) -> Vec<ReplayCard> {
    let mut cards: std::collections::BTreeMap<String, ReplayCard> =
        std::collections::BTreeMap::new();
    for task in tasks {
        let status = task.get_task_status();
        if !status.is_terminal() {
            continue;
        }
        let child = task
            .sub_session_id()
            .unwrap_or_else(|| format!("sub-{}", task.task_id()));
        let created_at = task.created_at();
        let last_checked_at = task.last_checked_at();
        match cards.entry(child) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                let child = slot.key().clone();
                slot.insert(ReplayCard {
                    child,
                    title: task.agent_id().to_string(),
                    started_at: created_at,
                    completed_at: last_checked_at,
                    last_seen: created_at,
                    status,
                    error: task.get_error(),
                });
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let card = slot.get_mut();
                card.started_at = card.started_at.min(created_at);
                card.completed_at = card.completed_at.max(last_checked_at);
                if created_at >= card.last_seen {
                    card.last_seen = created_at;
                    card.status = status;
                    card.error = task.get_error();
                }
            }
        }
    }
    cards.into_values().collect()
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "durable")]
    use super::{prompt_idempotency_key, stall_cancel_after_value, RUNNING_STALL_CANCEL};
    use super::{
        save_session_config, thought_level_params, turn_usage_to_response, SelectedDescriptor,
    };
    use agentscope_core::message::ChatUsage;
    #[cfg(feature = "durable")]
    use agentscope_core::message::Msg;
    use agentscope_harness::agent::subagent::task::{BackgroundTask, TaskStatus};

    fn terminal_task(
        task_id: &str,
        child: Option<&str>,
        created_at: u64,
        last_checked_at: u64,
        status: TaskStatus,
        error: Option<&str>,
    ) -> std::sync::Arc<BackgroundTask> {
        let task = BackgroundTask::from_record(
            task_id,
            "explorer",
            created_at,
            status,
            None,
            error.map(str::to_string),
        );
        task.set_sub_session_id(child.map(str::to_string));
        task.set_last_checked_at(last_checked_at);
        task
    }

    #[cfg(feature = "durable")]
    #[test]
    fn the_running_stall_cap_is_configurable_and_can_be_disabled() {
        assert_eq!(stall_cancel_after_value(None), Some(RUNNING_STALL_CANCEL));
        assert_eq!(
            stall_cancel_after_value(Some("90")),
            Some(std::time::Duration::from_secs(90))
        );
        // A typo must not silently turn the watchdog off.
        assert_eq!(
            stall_cancel_after_value(Some("nonsense")),
            Some(RUNNING_STALL_CANCEL)
        );
        for off in ["off", "0", "false", "no", "disabled"] {
            assert_eq!(stall_cancel_after_value(Some(off)), None, "{off}");
        }
    }

    #[test]
    fn a_replayed_card_spans_every_turn_of_one_child() {
        // `agent_send` reuses the child's card; replaying one card per task would draw two.
        let spawn = terminal_task(
            "spawn-1",
            Some("sub-live"),
            1_000,
            2_000,
            TaskStatus::Completed,
            None,
        );
        let send = terminal_task(
            "send-2",
            Some("sub-live"),
            5_000,
            6_000,
            TaskStatus::Failed,
            Some("boom"),
        );
        let cards = super::fold_replay_cards(vec![spawn, send]);
        assert_eq!(cards.len(), 1, "one child, one card");
        assert_eq!(cards[0].child, "sub-live");
        assert_eq!(
            cards[0].started_at, 1_000,
            "the card starts with the first turn"
        );
        assert_eq!(cards[0].completed_at, 6_000, "and ends with the last one");
        assert_eq!(cards[0].status, TaskStatus::Failed);
        assert_eq!(cards[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn a_record_without_a_child_session_falls_back_to_the_task_id() {
        let task = terminal_task("spawn-7", None, 10, 20, TaskStatus::Completed, None);
        let cards = super::fold_replay_cards(vec![task]);
        assert_eq!(cards[0].child, "sub-spawn-7");
    }

    #[test]
    fn a_running_task_is_not_replayed() {
        // No runner and no result survive a restart: reporting it as working or failed would
        // both be invented.
        let task = terminal_task("spawn-9", Some("sub-x"), 10, 20, TaskStatus::Running, None);
        assert!(super::fold_replay_cards(vec![task]).is_empty());
    }

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

    #[cfg(feature = "durable")]
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

    #[cfg(feature = "durable")]
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

    /// A tool call dispatched by a process that died before recording its outcome used to poison
    /// the session: the recorder refused every open with "Explicit reconciliation required".
    /// Opening a runtime settles it once, as interrupted, so the session opens again.
    #[cfg(feature = "durable")]
    #[tokio::test]
    async fn an_uncertain_tool_call_is_settled_before_the_session_opens() {
        use agentscope_core::journal::{
            AtomicThreadJournal, InvocationRecorder, JournalEvent, MemoryJournalStorage,
            ThreadJournal, UncertaintyTracker,
        };
        use std::sync::Arc;
        use std::time::Duration;

        fn event(seq: u64, kind: &str, payload: serde_json::Value) -> JournalEvent {
            JournalEvent::new(
                format!("e{seq}"),
                seq,
                0,
                kind,
                Some("run-1".to_string()),
                Some("t1".to_string()),
                true,
                payload.to_string(),
            )
            .unwrap()
        }

        let log: Arc<dyn ThreadJournal> = Arc::new(AtomicThreadJournal::new(
            Arc::new(MemoryJournalStorage::new()),
            "acp/test",
        ));
        let state = agentscope_core::state::AgentState::builder()
            .session_id("s1")
            .build();
        let facts = [
            event(
                1,
                "state/checkpoint",
                serde_json::json!({ "stateJson": state.to_json().unwrap() }),
            ),
            event(
                2,
                "tool/dispatch",
                serde_json::json!({ "toolCallId": "call-1" }),
            ),
        ];
        let writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        let head = log.head().await.unwrap();
        log.commit(&writer, "batch-1", head.seq, &facts)
            .await
            .unwrap();
        log.release(&writer).await.unwrap();

        // The symptom: while the outcome is unknown the recorder refuses to open the session.
        match InvocationRecorder::open_with(
            Arc::clone(&log),
            None,
            &uuid::Uuid::new_v4().to_string(),
            None,
        )
        .await
        {
            Err(error) => assert!(
                error.to_string().contains("Explicit reconciliation"),
                "an unknown tool outcome is what blocks the recorder: {error}"
            ),
            Ok(_) => panic!("an unknown tool outcome must block the recorder"),
        }

        super::reconcile_abandoned_tool_calls(&log).await;

        let projection = UncertaintyTracker::read(&*log).await.unwrap();
        assert!(
            projection.uncertain_tool_calls.is_empty(),
            "the abandoned call is settled: {:?}",
            projection.uncertain_tool_calls
        );
        let recorder = InvocationRecorder::open_with(
            Arc::clone(&log),
            None,
            &uuid::Uuid::new_v4().to_string(),
            None,
        )
        .await
        .expect("the session opens once the abandoned call is settled");
        recorder.close().await;
    }

    /// Regression: a turn that *ended* while one of its dispatches had no result left the
    /// journal unopenable for the rest of the process — `build_runtime` reconciles once, when it
    /// builds the runtime, so every later prompt failed with "Explicit reconciliation required".
    /// A finished turn's leftovers are settled before the next prompt; an unfinished turn keeps
    /// its uncertainty because resuming it must not find synthetic results.
    #[cfg(feature = "durable")]
    #[tokio::test]
    async fn a_finished_turns_stranded_dispatch_is_settled_before_the_next_prompt() {
        use agentscope_core::journal::{
            AtomicThreadJournal, JournalEvent, MemoryJournalStorage, ThreadJournal,
            UncertaintyTracker,
        };
        use std::sync::Arc;
        use std::time::Duration;

        fn event(seq: u64, kind: &str, payload: serde_json::Value) -> JournalEvent {
            JournalEvent::new(
                format!("e{seq}"),
                seq,
                0,
                kind,
                Some("run-1".to_string()),
                Some("t1".to_string()),
                true,
                payload.to_string(),
            )
            .unwrap()
        }

        async fn seed(ending: bool) -> Arc<dyn ThreadJournal> {
            let log: Arc<dyn ThreadJournal> = Arc::new(AtomicThreadJournal::new(
                Arc::new(MemoryJournalStorage::new()),
                "acp/test",
            ));
            let state = agentscope_core::state::AgentState::builder()
                .session_id("s1")
                .build();
            let mut facts = vec![
                event(
                    1,
                    "state/checkpoint",
                    serde_json::json!({ "stateJson": state.to_json().unwrap() }),
                ),
                event(2, "run/start", serde_json::json!({})),
                event(
                    3,
                    "tool/dispatch",
                    serde_json::json!({ "toolCallId": "call-1" }),
                ),
            ];
            if ending {
                facts.push(event(
                    4,
                    "turn/cancelled",
                    serde_json::json!({ "status": "cancelled" }),
                ));
            }
            let writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
            let head = log.head().await.unwrap();
            log.commit(&writer, "batch-1", head.seq, &facts)
                .await
                .unwrap();
            log.release(&writer).await.unwrap();
            log
        }

        // A cancelled turn left a dispatch unanswered: the next prompt would be refused unless
        // the leftover outcome is settled.
        let finished = seed(true).await;
        super::settle_finished_turn_uncertainties(&finished).await;
        assert!(
            UncertaintyTracker::read(&*finished)
                .await
                .unwrap()
                .uncertain_tool_calls
                .is_empty(),
            "the ended turn's stranded dispatch must be settled"
        );

        // A turn that is still open owns its uncertainty: resuming it must not find results that
        // were never observed.
        let open = seed(false).await;
        super::settle_finished_turn_uncertainties(&open).await;
        assert_eq!(
            UncertaintyTracker::read(&*open)
                .await
                .unwrap()
                .uncertain_tool_calls,
            ["call-1".to_string()].into_iter().collect(),
            "an unfinished turn's dispatch must stay uncertain"
        );

        // The cached variant folds only the delta. A clean journal leaves its projection
        // cached; a dispatch committed afterwards must still be settled by the next check.
        let growing: Arc<dyn ThreadJournal> = Arc::new(AtomicThreadJournal::new(
            Arc::new(MemoryJournalStorage::new()),
            "acp/test",
        ));
        let writer = growing
            .acquire("run-1", Duration::from_secs(60))
            .await
            .unwrap();
        let head = growing.head().await.unwrap();
        growing
            .commit(
                &writer,
                "batch-1",
                head.seq,
                &[
                    event(
                        1,
                        "state/checkpoint",
                        serde_json::json!({
                            "stateJson": agentscope_core::state::AgentState::builder()
                                .session_id("s1")
                                .build()
                                .to_json()
                                .unwrap(),
                        }),
                    ),
                    event(2, "run/start", serde_json::json!({})),
                    event(
                        3,
                        "turn/cancelled",
                        serde_json::json!({ "status": "cancelled" }),
                    ),
                ],
            )
            .await
            .unwrap();
        growing.release(&writer).await.unwrap();

        let cache = tokio::sync::Mutex::new(None);
        super::settle_uncertain_with_cache(&growing, &cache).await;
        assert!(
            cache.lock().await.is_some(),
            "a clean journal caches its projection instead of reconciling"
        );

        let writer = growing
            .acquire("run-2", Duration::from_secs(60))
            .await
            .unwrap();
        let head = growing.head().await.unwrap();
        growing
            .commit(
                &writer,
                "batch-2",
                head.seq,
                &[event(
                    head.seq + 1,
                    "tool/dispatch",
                    serde_json::json!({ "toolCallId": "call-2" }),
                )],
            )
            .await
            .unwrap();
        growing.release(&writer).await.unwrap();

        super::settle_uncertain_with_cache(&growing, &cache).await;
        assert!(
            UncertaintyTracker::read(&*growing)
                .await
                .unwrap()
                .uncertain_tool_calls
                .is_empty(),
            "a dispatch appended after the cache was seeded must still be settled"
        );
        assert!(
            cache.lock().await.is_none(),
            "reconciliation invalidates the cached projection"
        );
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
