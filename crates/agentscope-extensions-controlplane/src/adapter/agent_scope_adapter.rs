//! AgentScope framework adapter, mirroring
//! `io.agentscope.extensions.controlplane.adapter.AgentScopeAdapter`.
//!
//! **BYO path:** the user writes and runs their own [`ReActAgent`] or [`HarnessAgent`],
//! and controlplane observes it from the side. Observation rides on
//! [`ControlPlaneObserverMiddleware`], which passes every input and event through untouched.
//! Context and history are read from the live [`AgentState`], which is the same buffer
//! the agent hands to its next model call.
//!
//! Middlewares are fixed when the agent is built, so the Level-2 event stream requires
//! registering [`AgentScopeAdapter::middleware`] at build time. An already-built agent
//! can still be instrumented — context, history and commands read live state — but
//! produces no Level-2 events.

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use agentscope_core::agent::react_agent::ReActAgent;
use agentscope_core::agent::CallableAgent;
use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use agentscope_core::state::{AgentState, Task, TaskState};
use agentscope_core::Agent as _;
use agentscope_harness::agent::HarnessAgent;

use crate::adapter::framework_adapter::{
    CommandOutcome, FrameworkAdapter, CAP_AGENT_TASK, CAP_CONTEXT_QUERY, CAP_CONVERSATION_INBOUND,
    CAP_EXPORT_TRANSCRIPT, CAP_MESSAGE_QUERY, CAP_PLAN_MODE, CAP_SESSION_ABORT,
    CAP_SESSION_COMMAND, CAP_SUBAGENT_INVENTORY, CAP_SUBAGENT_TASK_COMMAND,
    CAP_SUBAGENT_TASK_QUERY, CAP_TASK_QUERY, CAP_WORKSPACE_INVENTORY, COMMAND_ABORT,
    COMMAND_COMPRESS, COMMAND_TERMINATE,
};
use crate::adapter::observer_middleware::{AdapterShared, ControlPlaneObserverMiddleware};
use crate::adapter::support::{
    AgentRuntimeSource, AgentTaskStarter, SessionCompactor, SessionHistorySource,
};
use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::model::{
    ContextMessage, ContextSnapshot, Inventory, MessageItem, MessagePage, SessionEvent, ToolInfo,
};

/// Framework identifier reported by this adapter.
pub const FRAMEWORK: &str = "agentscope-rust";

/// Name the harness compactor gives the summary message it injects.
const COMPACTION_SUMMARY_NAME: &str = "__compaction_summary__";

/// Bypass-observation adapter for a self-deployed AgentScope agent.
pub struct AgentScopeAdapter {
    shared: Arc<AdapterShared>,
    middleware: ControlPlaneObserverMiddleware,
    /// The instrumented agent, when attached.
    target: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    /// Handles `compress`; `None` leaves that command unsupported.
    compactor: Option<Arc<dyn SessionCompactor>>,
    history_source: Mutex<Option<Arc<dyn SessionHistorySource>>>,
    runtime_source: Mutex<Option<Arc<dyn AgentRuntimeSource>>>,
    agent_task_starter: Mutex<Option<Arc<dyn AgentTaskStarter>>>,
}

impl Default for AgentScopeAdapter {
    fn default() -> Self {
        Self::new(None)
    }
}

impl AgentScopeAdapter {
    /// Creates an adapter with no compactor (the `compress` command is unsupported).
    pub fn new(compactor: Option<Arc<dyn SessionCompactor>>) -> Self {
        let shared = Arc::new(AdapterShared::default());
        let middleware = ControlPlaneObserverMiddleware::new(Arc::clone(&shared));
        Self {
            shared,
            middleware,
            target: Mutex::new(None),
            compactor,
            history_source: Mutex::new(None),
            runtime_source: Mutex::new(None),
            agent_task_starter: Mutex::new(None),
        }
    }

    /// The middleware to register on the agent builder to enable the Level-2 event stream.
    pub fn middleware(&self) -> ControlPlaneObserverMiddleware {
        self.middleware.clone()
    }

    /// Optional full-history reader (e.g. a harness transcript).
    pub fn set_history_source(&self, source: Arc<dyn SessionHistorySource>) {
        *self
            .history_source
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(source);
    }

    /// Optional harness-/app hooks for inventory, plan files and subagent tasks.
    pub fn set_runtime_source(&self, source: Arc<dyn AgentRuntimeSource>) {
        *self
            .runtime_source
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(source);
    }

    /// Installs the host hook that materializes ASDP AgentTask deliveries.
    pub fn set_agent_task_starter(&self, starter: Arc<dyn AgentTaskStarter>) {
        *self
            .agent_task_starter
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(starter);
    }

    fn target(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.target
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn require_target(&self) -> ControlPlaneResult<Arc<dyn Any + Send + Sync>> {
        self.target().ok_or_else(|| {
            ControlPlaneError::Config("agentscope-rust: adapter is not attached".into())
        })
    }

    fn runtime_source(&self) -> Option<Arc<dyn AgentRuntimeSource>> {
        self.runtime_source
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The user slot recorded for `session_id`, used to reach the right state slot.
    fn user_of(&self, session_id: &str) -> Option<String> {
        self.shared
            .session_users
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(session_id)
            .cloned()
            .filter(|u| !u.is_empty())
    }

    /// Resolves the state buffer for `session_id`.
    ///
    /// core's [`ReActAgent`] keeps a single state slot, so a mismatch means the caller is
    /// asking about a session this agent is not currently serving; such a request returns
    /// no state rather than reading the wrong session. A [`HarnessAgent`] additionally
    /// consults its `DistributedStore` for the persisted slot.
    async fn resolve_state(
        &self,
        target: &Arc<dyn Any + Send + Sync>,
        session_id: &str,
    ) -> Option<AgentState> {
        let matches = |st: &AgentState| session_id.is_empty() || st.session_id == session_id;
        if let Some(react) = target.downcast_ref::<ReActAgent>() {
            let state = react.agent_state();
            return matches(&state).then_some(state);
        }
        if let Some(harness) = target.downcast_ref::<HarnessAgent>() {
            let user = self.user_of(session_id);
            if let Some(store) = harness
                .distributed_store()
                .and_then(|ds| ds.agent_state_store.as_ref())
            {
                if let Some(value) = store.get(user.as_deref(), session_id, "state").await {
                    if let Ok(state) = serde_json::from_value::<AgentState>(value) {
                        return Some(state);
                    }
                }
            }
            if let Some(react) = harness.agent() {
                let state = react.agent_state();
                if matches(&state) {
                    return Some(state);
                }
            }
        }
        None
    }

    /// The embedded [`ReActAgent`], unwrapping one level of harness delegation.
    fn react_of(target: &Arc<dyn Any + Send + Sync>) -> Option<&ReActAgent> {
        if let Some(react) = target.downcast_ref::<ReActAgent>() {
            return Some(react);
        }
        target
            .downcast_ref::<HarnessAgent>()
            .and_then(|h| h.agent())
    }

    /// Tools exposed to the model, from the agent's toolkit.
    fn tools_of(&self, target: &Arc<dyn Any + Send + Sync>, groups: &[String]) -> Vec<ToolInfo> {
        if let Some(src) = self.runtime_source() {
            if let Some(tools) = src.tools_for_groups(groups) {
                return tools;
            }
        }
        let toolkit = match target.downcast_ref::<HarnessAgent>() {
            Some(h) => h.toolkit(),
            None => match target.downcast_ref::<ReActAgent>() {
                Some(r) => r.toolkit(),
                None => return Vec::new(),
            },
        };
        let schemas = if groups.is_empty() {
            toolkit.tool_schemas()
        } else {
            toolkit.tool_schemas_for(groups)
        };
        schemas
            .into_iter()
            .map(|s| ToolInfo::new(s.name, s.description))
            .collect()
    }
}

#[async_trait]
impl FrameworkAdapter for AgentScopeAdapter {
    fn framework_name(&self) -> &str {
        FRAMEWORK
    }

    fn capabilities(&self) -> Vec<String> {
        let mut caps = vec![
            CAP_CONTEXT_QUERY,
            CAP_MESSAGE_QUERY,
            CAP_SESSION_COMMAND,
            CAP_SESSION_ABORT,
            CAP_TASK_QUERY,
            CAP_PLAN_MODE,
            CAP_EXPORT_TRANSCRIPT,
            CAP_CONVERSATION_INBOUND,
        ];
        if self.runtime_source().is_some() {
            caps.push(CAP_SUBAGENT_INVENTORY);
            caps.push(CAP_WORKSPACE_INVENTORY);
            caps.push(CAP_SUBAGENT_TASK_QUERY);
            caps.push(CAP_SUBAGENT_TASK_COMMAND);
        }
        let has_task_starter = self
            .agent_task_starter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some();
        if has_task_starter {
            caps.push(CAP_AGENT_TASK);
        }
        caps.sort_unstable();
        caps.dedup();
        caps.into_iter().map(String::from).collect()
    }

    fn can_handle(&self, target: &(dyn Any + Send + Sync)) -> bool {
        target.is::<ReActAgent>() || target.is::<HarnessAgent>()
    }

    fn attach(
        &mut self,
        target: Arc<dyn Any + Send + Sync>,
        _emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
    ) -> ControlPlaneResult<()> {
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = Some(target);
        Ok(())
    }

    fn detach(&self) -> ControlPlaneResult<()> {
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }

    /// Receives the bridge's reporter so the observer middleware (built before the bridge
    /// existed) and [`extract_context`](Self::extract_context) can reach the journal.
    /// Mirrors `AgentScopeAdapter.onBridgeAttached`.
    fn on_bridge_attached(&mut self, reporter: crate::bridge::BridgeReporter) {
        *self
            .shared
            .reporter
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(reporter);
    }

    async fn extract_context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
        let target = self.require_target()?;
        let state = self.resolve_state(&target, session_id).await;
        let context = state
            .as_ref()
            .map(|s| s.context.clone())
            .unwrap_or_default();

        let mut messages = Vec::with_capacity(context.len());
        let mut compaction_summary = String::new();
        for msg in &context {
            let text = text_of(msg);
            if msg.name() == Some(COMPACTION_SUMMARY_NAME) {
                compaction_summary = text.clone();
            }
            messages.push(ContextMessage::new(role_of(msg), text));
        }

        let react = Self::react_of(&target);
        let model = react.map(|r| r.model());
        let activated_groups = target
            .downcast_ref::<HarnessAgent>()
            .map(|h| h.toolkit().get_active_groups())
            .unwrap_or_default();

        let base_prompt = react
            .map(|r| r.sys_prompt().to_string())
            .unwrap_or_default();
        let sampled = self
            .shared
            .reporter()
            .map(|r| r.effective_system_prompt(session_id))
            .unwrap_or_default();
        let has_effective = !sampled.is_empty();
        let effective_prompt = if has_effective {
            sampled
        } else {
            base_prompt.clone()
        };

        let framework_state = serde_json::json!({
            "baseSystemPrompt": base_prompt,
            "activatedGroups": activated_groups,
            "systemPromptSource": if has_effective { "effective" } else { "base" },
        });

        let context_window = model
            .as_ref()
            .map(|m| m.context_window_size() as i64)
            .unwrap_or(0);
        let snapshot = ContextSnapshot::builder(session_id)
            .framework(FRAMEWORK)
            .system_prompt(effective_prompt)
            .messages(messages)
            .tools(self.tools_of(&target, &activated_groups))
            .compacted(!compaction_summary.is_empty())
            .compaction_summary(compaction_summary)
            .total_tokens(window_used_tokens(&context))
            .max_tokens(context_window)
            .model(
                model
                    .as_ref()
                    .map(|m| m.model_name().to_string())
                    .unwrap_or_default(),
            )
            .framework_state(framework_state)
            .build();
        Ok(snapshot)
    }

    async fn list_messages(
        &self,
        session_id: &str,
        offset: i64,
        limit: i64,
    ) -> ControlPlaneResult<MessagePage> {
        let target = self.require_target()?;
        let user = self.user_of(session_id);
        let source = self
            .history_source
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(source) = source {
            if let Some(items) = source.load_messages(session_id, user.as_deref()).await {
                return Ok(MessagePage::of(session_id, items, offset, limit));
            }
        }
        let context = self
            .resolve_state(&target, session_id)
            .await
            .map(|s| s.context)
            .unwrap_or_default();
        let items = context
            .iter()
            .enumerate()
            .map(|(i, msg)| message_item(i as i64 + 1, msg))
            .collect();
        Ok(MessagePage::of(session_id, items, offset, limit))
    }

    async fn session_ids(&self) -> ControlPlaneResult<Vec<String>> {
        Ok(self
            .shared
            .session_users
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect())
    }

    async fn list_subagents(&self) -> ControlPlaneResult<Inventory> {
        let src = self.runtime_source().ok_or_else(|| {
            ControlPlaneError::Unsupported(
                "subagent-inventory is not supported by this adapter".into(),
            )
        })?;
        let mut inventory = Inventory::default();
        inventory.subagents = src.list_subagents().await?;
        Ok(inventory)
    }

    async fn execute_command(
        &self,
        session_id: &str,
        command: &str,
        _payload: Option<serde_json::Value>,
    ) -> ControlPlaneResult<CommandOutcome> {
        match command {
            COMMAND_TERMINATE | COMMAND_ABORT => {
                let target = self.require_target()?;
                let ok = match Self::react_of(&target) {
                    Some(react) => react.interrupt_session(session_id),
                    None => target
                        .downcast_ref::<HarnessAgent>()
                        .map(|h| h.interrupt_session(session_id))
                        .unwrap_or(false),
                };
                Ok(CommandOutcome {
                    ok,
                    message: if ok {
                        String::new()
                    } else {
                        "session not active".into()
                    },
                })
            }
            COMMAND_COMPRESS => {
                let Some(compactor) = self.compactor.clone() else {
                    return Err(ControlPlaneError::Unsupported(
                        "agentscope-rust: compress requires a SessionCompactor".into(),
                    ));
                };
                let target = self.require_target()?;
                let Some(mut state) = self.resolve_state(&target, session_id).await else {
                    return Err(ControlPlaneError::Config(format!(
                        "agentscope-rust: no state for session {session_id}"
                    )));
                };
                let summary = compactor.compact(session_id, &mut state).await?;
                // Compacting a copy would only change what the control plane shows while the
                // next turn still sends the full history (tracker CP-07): write the compacted
                // context back into the buffer the agent actually reads.
                self.commit_compacted_state(&target, session_id, &state).await;
                // A compactor that found nothing to compact returns an empty summary;
                // publishing an empty `compaction` event would tell the control plane a
                // rewrite happened when the history is unchanged.
                if !summary.trim().is_empty() {
                    if let Some(reporter) = self.shared.reporter() {
                        reporter.publish(
                            SessionEvent::builder(session_id, crate::model::TYPE_COMPACTION)
                                .content(summary.clone())
                                .build(),
                        );
                    }
                }
                Ok(CommandOutcome {
                    ok: true,
                    message: summary,
                })
            }
            other => Err(ControlPlaneError::Unsupported(format!(
                "unsupported command: {other}"
            ))),
        }
    }

    /// Todo list for `GET .../tasks`, read from the live `AgentState.tasks_context`
    /// (mirrors `AgentScopeAdapter.listTasks`).
    async fn list_tasks(&self, session_id: &str) -> ControlPlaneResult<Vec<serde_json::Value>> {
        let target = self.require_target()?;
        let state = self.resolve_state(&target, session_id).await;
        Ok(state
            .as_ref()
            .map(|s| s.tasks_context().tasks().iter().map(task_json).collect())
            .unwrap_or_default())
    }

    /// Workspaces reported by the host runtime source (mirrors `listWorkspaces`).
    async fn list_workspaces(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        let src = self.runtime_source().ok_or_else(|| {
            ControlPlaneError::Unsupported(
                "workspace-inventory is not supported by this adapter".into(),
            )
        })?;
        src.list_workspaces().await
    }

    /// Background subagent tasks for `GET .../subagent-tasks`.
    async fn list_subagent_tasks(
        &self,
        session_id: &str,
    ) -> ControlPlaneResult<Vec<serde_json::Value>> {
        let src = self.runtime_source().ok_or_else(|| {
            ControlPlaneError::Unsupported(
                "subagent-task-query is not supported by this adapter".into(),
            )
        })?;
        src.list_subagent_tasks(session_id).await
    }

    /// Cancels one background subagent task; a task the host did not find is a 404.
    async fn cancel_subagent_task(&self, session_id: &str, task_id: &str) -> ControlPlaneResult<()> {
        let src = self.runtime_source().ok_or_else(|| {
            ControlPlaneError::Unsupported(
                "subagent-task-command is not supported by this adapter".into(),
            )
        })?;
        if src.cancel_subagent_task(session_id, task_id).await? {
            Ok(())
        } else {
            Err(ControlPlaneError::NotFound(format!(
                "subagent task not found or cancel failed: {task_id}"
            )))
        }
    }

    /// Enters/leaves plan mode (mirrors `AgentScopeAdapter.setPlanMode`).
    ///
    /// The host runtime source wins when it handles the switch; otherwise the flag is written
    /// into the agent's **live** state handle so the next turn observes it.
    async fn set_plan_mode(
        &self,
        session_id: &str,
        params: serde_json::Value,
    ) -> ControlPlaneResult<()> {
        let active = plan_active(&params);
        let target = self.require_target()?;
        if let Some(src) = self.runtime_source() {
            if src
                .set_plan_mode(session_id, self.user_of(session_id).as_deref(), active)
                .await?
            {
                return Ok(());
            }
        }
        let react = Self::react_of(&target).ok_or_else(|| {
            ControlPlaneError::Config(format!(
                "agentscope-rust: no live state for session {session_id}"
            ))
        })?;
        let handle = react.state_handle();
        let mut state = handle.lock().unwrap_or_else(|p| p.into_inner());
        state.plan_mode_context.set_plan_active(active);
        Ok(())
    }

    /// Injects a user message through the framework's own `call` path.
    async fn inject_user_message(&self, session_id: &str, content: &str) -> ControlPlaneResult<()> {
        self.call_user_message(session_id, content).await.map(|_| ())
    }

    /// Runs one control-plane turn and returns the terminal assistant reply.
    async fn run_conversation_turn(
        &self,
        session_id: &str,
        content: &str,
    ) -> ControlPlaneResult<String> {
        let reply = self.call_user_message(session_id, content).await?;
        Ok(reply.text_content())
    }

    /// Effective definition snapshot merged into `GET /agentscope/info` → `agentConfig`.
    fn build_agent_config(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut cfg = serde_json::Map::new();
        let Some(target) = self.target() else {
            return cfg;
        };
        let react = Self::react_of(&target);
        let name = react.map(|r| r.name().to_string()).unwrap_or_default();
        if !name.is_empty() {
            cfg.insert("name".into(), name.into());
        }
        let description = react.map(|r| r.description().to_string()).unwrap_or_default();
        if !description.is_empty() {
            cfg.insert("description".into(), description.into());
        }
        let sys_prompt = react.map(|r| r.sys_prompt().to_string()).unwrap_or_default();
        if !sys_prompt.is_empty() {
            cfg.insert("systemPrompt".into(), truncate_prompt(&sys_prompt).into());
        }
        if let Some(model) = react.map(|r| r.model()) {
            let model_name = model.model_name().to_string();
            if !model_name.is_empty() {
                cfg.insert("model".into(), model_name.into());
            }
            cfg.insert(
                "contextWindow".into(),
                (model.context_window_size() as i64).into(),
            );
        }
        if let Some(r) = react {
            cfg.insert("maxIters".into(), r.max_iters().into());
            cfg.insert("maxTurns".into(), r.max_iters().into());
        }
        let groups = target
            .downcast_ref::<HarnessAgent>()
            .map(|h| h.toolkit().get_active_groups())
            .unwrap_or_default();
        let tools = self.tools_of(&target, &groups);
        if !tools.is_empty() {
            cfg.insert(
                "tools".into(),
                serde_json::Value::Array(
                    tools
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "name": t.name.clone(),
                                "description": t.description.clone(),
                            })
                        })
                        .collect(),
                ),
            );
        }
        if let Some(src) = self.runtime_source() {
            src.enrich_agent_config(&mut cfg);
        }
        cfg
    }

    /// Materializes a downstream AgentTask delivery through the host starter.
    async fn handle_agent_task(
        &self,
        assignment: crate::model::AgentTaskAssignment,
    ) -> ControlPlaneResult<()> {
        let starter = self
            .agent_task_starter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| {
                ControlPlaneError::Unsupported(
                    "agentscope-rust: AgentTask requires an AgentTaskStarter".into(),
                )
            })?;
        starter.start(assignment).await
    }
}

impl AgentScopeAdapter {
    /// Writes a compaction result back into the state the next turn reads.
    ///
    /// Mirrors Java `AgentScopeAdapter.handleCommand`, which compacts the **live**
    /// `AgentState` resolved by `resolveState`. core's state sits behind a mutex that cannot
    /// be held across the (async) compaction, so the compactor runs on a copy and only the
    /// compacted `context` is written back — never a whole-state replacement, because a
    /// concurrent turn may have moved other fields on in the meantime. A [`HarnessAgent`]
    /// additionally persists the new state, so a later turn that reloads from its
    /// `AgentStateStore` does not resurrect the pre-compaction history.
    async fn commit_compacted_state(
        &self,
        target: &Arc<dyn Any + Send + Sync>,
        session_id: &str,
        compacted: &AgentState,
    ) {
        if let Some(react) = Self::react_of(target) {
            let handle = react.state_handle();
            let mut live = handle.lock().unwrap_or_else(|p| p.into_inner());
            // core keeps a single state slot: only the session that owns it may be rewritten.
            if live.session_id == compacted.session_id {
                *live.context_mut() = compacted.context.clone();
            }
        }
        let Some(harness) = target.downcast_ref::<HarnessAgent>() else {
            return;
        };
        let Some(store) = harness
            .distributed_store()
            .and_then(|ds| ds.agent_state_store.as_ref())
        else {
            return;
        };
        let Ok(value) = serde_json::to_value(compacted) else {
            return;
        };
        store
            .save(self.user_of(session_id).as_deref(), session_id, "state", value)
            .await;
    }

    /// Runs one user message through the live agent, mirroring
    /// `AgentScopeAdapter.callUserMessage`.
    async fn call_user_message(&self, session_id: &str, content: &str) -> ControlPlaneResult<Msg> {
        let target = self.require_target()?;
        let messages = vec![Msg::user(content)];
        let result = if let Some(harness) = target.downcast_ref::<HarnessAgent>() {
            harness.call(messages).await
        } else if let Some(react) = target.downcast_ref::<ReActAgent>() {
            react.call(messages).await
        } else {
            return Err(ControlPlaneError::Config(format!(
                "agentscope-rust: session {session_id} has no callable agent"
            )));
        };
        result.map_err(|error| ControlPlaneError::Internal(format!("agent call failed: {error}")))
    }
}

/// Contract `Task` shape for `GET .../tasks`, mirroring `AgentScopeAdapter.toTaskJson`.
fn task_json(task: &Task) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("id".into(), task.id.clone().into());
    out.insert("subject".into(), task.subject.clone().into());
    if !task.description.is_empty() {
        out.insert("description".into(), task.description.clone().into());
    }
    out.insert("state".into(), task_state_wire(task.state).into());
    if let Some(owner) = task.owner.as_ref().filter(|owner| !owner.is_empty()) {
        out.insert("owner".into(), owner.clone().into());
    }
    out.insert(
        "blockedBy".into(),
        serde_json::Value::Array(task.blocked_by.iter().cloned().map(Into::into).collect()),
    );
    if !task.created_at.is_empty() {
        out.insert("updatedAt".into(), task.created_at.clone().into());
    }
    out.insert(
        "frameworkMeta".into(),
        serde_json::Value::Object(task.metadata.clone()),
    );
    serde_json::Value::Object(out)
}

/// Wire name of a task state (core `TaskState` serialises to the same strings).
fn task_state_wire(state: TaskState) -> &'static str {
    match state {
        TaskState::Pending => "pending",
        TaskState::InProgress => "in_progress",
        TaskState::Completed => "completed",
    }
}

/// Parses the plan-mode body, mirroring `AgentScopeAdapter.parsePlanActive`.
///
/// An absent / unparsable body means "enter": the control plane only shows the button when
/// it intends to turn plan mode on.
fn plan_active(params: &serde_json::Value) -> bool {
    let Some(object) = params.as_object() else {
        return true;
    };
    if let Some(active) = object.get("active") {
        match active {
            serde_json::Value::Bool(active) => return *active,
            serde_json::Value::Null => {}
            other => {
                return other
                    .as_str()
                    .map(|text| text.eq_ignore_ascii_case("true"))
                    .unwrap_or(true)
            }
        }
    }
    match object.get("mode").and_then(|mode| mode.as_str()) {
        Some(mode) => !matches!(mode.to_ascii_lowercase().as_str(), "exit" | "off" | "false"),
        None => true,
    }
}

/// Truncates an over-long system prompt for `/agentscope/info` (Java caps at 8000).
fn truncate_prompt(prompt: &str) -> String {
    const MAX_CHARS: usize = 8000;
    if prompt.chars().count() <= MAX_CHARS {
        return prompt.to_string();
    }
    let mut out: String = prompt.chars().take(MAX_CHARS).collect();
    out.push('…');
    out
}

/// Estimates tokens occupying the model context window.
///
/// Each message's `input_tokens` already includes the full prompt for that turn, so
/// summing across messages double-counts history; the latest non-zero input size is the
/// best available proxy for live window occupancy.
fn window_used_tokens(context: &[Msg]) -> i64 {
    let mut used = 0i64;
    for msg in context {
        if let Some(usage) = msg.chat_usage() {
            let input = usage.input_tokens as i64;
            if input > 0 {
                used = input;
            }
        }
    }
    used
}

/// Builds a Level-3 history entry from a message.
pub(crate) fn message_item(seq: i64, msg: &Msg) -> MessageItem {
    let mut item = MessageItem::of(seq, role_of(msg), text_of(msg));
    for block in msg.content() {
        match block {
            ContentBlock::ToolUse(use_block) => {
                item.tool_name = use_block.name.clone();
                if !use_block.input.is_empty() {
                    item.tool_input = Some(serde_json::Value::Object(
                        use_block.input.clone().into_iter().collect(),
                    ));
                }
            }
            ContentBlock::ToolResult(result) => {
                if let Some(name) = &result.name {
                    item.tool_name = name.clone();
                }
                item.tool_output = blocks_text(&result.output);
            }
            _ => {}
        }
    }
    item.occurred_at = parse_timestamp(msg.timestamp());
    item
}

/// Concatenates the text blocks of `blocks`.
fn blocks_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(t) = block {
            out.push_str(&t.text);
        }
    }
    out
}

/// The plain-text content of a message.
fn text_of(msg: &Msg) -> String {
    msg.text_content()
}

/// The wire role name of a message.
fn role_of(msg: &Msg) -> &'static str {
    match msg.role() {
        MsgRole::User => crate::model::ROLE_USER,
        MsgRole::Assistant => crate::model::ROLE_ASSISTANT,
        MsgRole::System => crate::model::ROLE_SYSTEM,
        MsgRole::Tool => crate::model::ROLE_TOOL,
    }
}

/// Parses an RFC 3339 timestamp to epoch millis, or 0.
fn parse_timestamp(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::agent::RuntimeContext;
    use agentscope_core::middleware::{AgentInput, Middleware, Next};
    use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
    use futures::stream::BoxStream;

    /// Minimal model: the adapter only reads metadata, never streams.
    struct StubModel;

    impl Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
            Box::pin(futures::stream::empty())
        }

        fn model_name(&self) -> &str {
            "stub-model"
        }

        fn context_window_size(&self) -> usize {
            128_000
        }
    }

    fn react_agent() -> ReActAgent {
        ReActAgent::builder()
            .name("friday")
            .model(Arc::new(StubModel))
            .sys_prompt("you are helpful")
            .max_iters(7)
            .build()
            .unwrap()
    }

    #[test]
    fn can_handle_only_agentscope_agents() {
        let adapter = AgentScopeAdapter::new(None);
        let agent = react_agent();
        assert!(adapter.can_handle(&agent));
        assert!(!adapter.can_handle(&42u32));
    }

    #[test]
    fn capabilities_baseline_without_optional_sources() {
        let adapter = AgentScopeAdapter::new(None);
        let caps = adapter.capabilities();
        assert!(caps.contains(&CAP_CONTEXT_QUERY.to_string()));
        assert!(caps.contains(&CAP_MESSAGE_QUERY.to_string()));
        assert!(caps.contains(&CAP_SESSION_ABORT.to_string()));
        assert!(!caps.contains(&CAP_SUBAGENT_INVENTORY.to_string()));
        assert!(!caps.contains(&CAP_AGENT_TASK.to_string()));
    }

    #[test]
    fn framework_version_defaults_to_empty() {
        let adapter = AgentScopeAdapter::new(None);
        assert_eq!(adapter.framework_name(), FRAMEWORK);
        assert_eq!(adapter.framework_version(), "");
    }

    #[tokio::test]
    async fn extract_context_reads_live_state_when_session_matches() {
        let mut adapter = AgentScopeAdapter::new(None);
        let agent = Arc::new(react_agent());
        let target: Arc<dyn Any + Send + Sync> = agent.clone();
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();
        let session = agent.agent_state().session_id.clone();
        let snap = adapter.extract_context(&session).await.unwrap();
        assert_eq!(snap.framework, FRAMEWORK);
        assert_eq!(snap.system_prompt, "you are helpful");
    }

    #[tokio::test]
    async fn extract_context_ignores_other_sessions() {
        let mut adapter = AgentScopeAdapter::new(None);
        let agent = Arc::new(react_agent());
        let target: Arc<dyn Any + Send + Sync> = agent.clone();
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();
        let snap = adapter.extract_context("some-other-session").await.unwrap();
        assert!(snap.messages.is_empty());
    }

    #[tokio::test]
    async fn execute_command_rejects_compress_without_compactor() {
        let mut adapter = AgentScopeAdapter::new(None);
        let agent = Arc::new(react_agent());
        let target: Arc<dyn Any + Send + Sync> = agent.clone();
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();
        let session = agent.agent_state().session_id.clone();
        let outcome = adapter
            .execute_command(&session, COMMAND_COMPRESS, None)
            .await;
        assert!(matches!(outcome, Err(ControlPlaneError::Unsupported(_))));
    }

    #[tokio::test]
    async fn execute_command_aborts_matching_session() {
        let mut adapter = AgentScopeAdapter::new(None);
        let agent = Arc::new(react_agent());
        let target: Arc<dyn Any + Send + Sync> = agent.clone();
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();
        let session = agent.agent_state().session_id.clone();
        let outcome = adapter
            .execute_command(&session, COMMAND_ABORT, None)
            .await
            .unwrap();
        assert!(outcome.ok);
    }

    #[tokio::test]
    async fn unset_runtime_source_reports_unsupported_inventory() {
        let mut adapter = AgentScopeAdapter::new(None);
        let agent = Arc::new(react_agent());
        let target: Arc<dyn Any + Send + Sync> = agent;
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();
        assert!(matches!(
            adapter.list_subagents().await,
            Err(ControlPlaneError::Unsupported(_))
        ));
    }

    #[test]
    fn on_agent_records_session_user_for_state_resolution() {
        let adapter = AgentScopeAdapter::new(None);
        let agent = react_agent();
        let session = agent.agent_state().session_id.clone();
        let middleware = adapter.middleware();
        let mut ctx = RuntimeContext::default();
        ctx.session_id = Some(session.clone());
        ctx.user_id = Some("u1".into());
        let next: Next<AgentInput> = Box::new(|_i| Box::pin(futures::stream::empty()));
        let _ = middleware.on_agent(&ctx, AgentInput::new(vec![]), next);
        assert_eq!(adapter.user_of(&session).as_deref(), Some("u1"));
    }

    /// Regression: the reporter must be installed through the real `instrument` →
    /// `bridge.attach` assembly path (via `on_bridge_attached`), not only by tests that
    /// poke `AdapterShared` directly. Without the override the middleware hits its
    /// `else { return next(input) }` arm and every Level-2 event is silently dropped.
    #[tokio::test]
    async fn bridge_attach_installs_reporter_for_the_observer_middleware() {
        let cfg = crate::config::ControlPlaneConfig::builder("friday")
            .instance_key("adapter-attach")
            .event_journal_dir(
                std::env::temp_dir()
                    .join(format!("controlplane-attach-{}", uuid::Uuid::new_v4()))
                    .to_str()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let adapter = AgentScopeAdapter::new(None);
        let middleware = adapter.middleware();
        let target: Arc<dyn Any + Send + Sync> = Arc::new(react_agent());
        let bridge = crate::instrument(target, cfg, Box::new(adapter))
            .await
            .unwrap();

        let mut ctx = RuntimeContext::default();
        ctx.session_id = Some("s-attach".into());
        ctx.delta_tx = None;
        let next: Next<AgentInput> = Box::new(|_i| Box::pin(futures::stream::empty()));
        let _ = middleware.on_agent(&ctx, AgentInput::new(vec![Msg::user("hi")]), next);

        assert!(
            bridge.is_known_session("s-attach"),
            "on_bridge_attached did not reach AdapterShared; Level-2 events were dropped"
        );
    }

    /// Compactor double that keeps only the newest message, so a write-back is observable.
    struct TrimCompactor;

    #[async_trait]
    impl SessionCompactor for TrimCompactor {
        async fn compact(
            &self,
            _session_id: &str,
            state: &mut AgentState,
        ) -> ControlPlaneResult<String> {
            if let Some(last) = state.context.last().cloned() {
                state.context = vec![last];
            }
            Ok("trimmed".into())
        }
    }

    /// CP-07: `compress` must compact the buffer the next turn actually reads. Before the
    /// write-back the compaction ran on a clone, so the control plane showed a compacted
    /// context while the agent kept sending the full history.
    #[tokio::test]
    async fn compress_writes_the_compacted_context_back_into_the_live_state() {
        let mut adapter = AgentScopeAdapter::new(Some(Arc::new(TrimCompactor)));
        let agent = Arc::new(react_agent());
        let session = agent.agent_state().session_id.clone();
        {
            let handle = agent.state_handle();
            let mut state = handle.lock().unwrap_or_else(|p| p.into_inner());
            state.context = vec![Msg::user("one"), Msg::user("two"), Msg::user("three")];
        }
        let target: Arc<dyn Any + Send + Sync> = agent.clone();
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();

        let outcome = adapter
            .execute_command(&session, COMMAND_COMPRESS, None)
            .await
            .unwrap();
        assert_eq!(outcome.message, "trimmed");
        let live = agent.agent_state();
        assert_eq!(
            live.context.len(),
            1,
            "the live context was not compacted: only the clone was rewritten"
        );
        assert_eq!(live.context[0].text_content(), "three");
    }

    /// CP-07: a compaction for a session this single-slot agent is not serving must leave the
    /// live state alone (never rewrite another session's history).
    #[tokio::test]
    async fn compress_leaves_the_live_state_alone_for_a_foreign_session() {
        let mut adapter = AgentScopeAdapter::new(Some(Arc::new(TrimCompactor)));
        let agent = Arc::new(react_agent());
        let session = agent.agent_state().session_id.clone();
        {
            let handle = agent.state_handle();
            let mut state = handle.lock().unwrap_or_else(|p| p.into_inner());
            state.context = vec![Msg::user("one"), Msg::user("two")];
        }
        let target: Arc<dyn Any + Send + Sync> = agent.clone();
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();

        let foreign = "someone-elses-session";
        assert!(!session.is_empty());
        assert!(adapter
            .execute_command(foreign, COMMAND_COMPRESS, None)
            .await
            .is_err());
        assert_eq!(agent.agent_state().context.len(), 2);
    }

    /// A runtime source that supports every optional capability, so the capability bits can be
    /// probed one by one.
    struct FullRuntimeSource;

    #[async_trait]
    impl AgentRuntimeSource for FullRuntimeSource {
        async fn list_subagents(&self) -> ControlPlaneResult<Vec<crate::model::SubagentInfo>> {
            Ok(vec![crate::model::SubagentInfo {
                name: "sub-1".into(),
                ..Default::default()
            }])
        }
        async fn list_workspaces(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
            Ok(vec![serde_json::json!({ "path": "/work", "mode": "rw" })])
        }
        async fn list_subagent_tasks(
            &self,
            _session_id: &str,
        ) -> ControlPlaneResult<Vec<serde_json::Value>> {
            Ok(vec![serde_json::json!({ "id": "t1", "status": "running" })])
        }
        async fn cancel_subagent_task(
            &self,
            _session_id: &str,
            _task_id: &str,
        ) -> ControlPlaneResult<bool> {
            Ok(true)
        }
        async fn set_plan_mode(
            &self,
            _session_id: &str,
            _user_id: Option<&str>,
            _active: bool,
        ) -> ControlPlaneResult<bool> {
            Ok(false)
        }
    }

    fn attached_adapter() -> (AgentScopeAdapter, Arc<ReActAgent>, String) {
        let mut adapter = AgentScopeAdapter::new(None);
        let agent = Arc::new(react_agent());
        let session = agent.agent_state().session_id.clone();
        let target: Arc<dyn Any + Send + Sync> = agent.clone();
        adapter
            .attach(target, Arc::new(|_e: SessionEvent| {}))
            .unwrap();
        (adapter, agent, session)
    }

    /// CP-03: every declared capability must map to a callable implementation. Before the
    /// parity methods existed the adapter declared `task-query` / `plan-mode` /
    /// `export-transcript` / `conversation-inbound` while their trait defaults returned
    /// `Unsupported`, so the control plane showed buttons that always failed.
    #[tokio::test]
    async fn declared_capabilities_map_to_non_unsupported_operations() {
        let (adapter, _agent, session) = attached_adapter();
        let caps = adapter.capabilities();
        assert!(caps.contains(&CAP_TASK_QUERY.to_string()));
        assert!(caps.contains(&CAP_PLAN_MODE.to_string()));
        assert!(caps.contains(&CAP_EXPORT_TRANSCRIPT.to_string()));
        assert!(caps.contains(&CAP_CONVERSATION_INBOUND.to_string()));

        for capability in caps {
            let outcome: ControlPlaneResult<()> = match capability.as_str() {
                CAP_CONTEXT_QUERY => adapter.extract_context(&session).await.map(|_| ()),
                CAP_MESSAGE_QUERY => adapter.list_messages(&session, 0, 10).await.map(|_| ()),
                CAP_SESSION_COMMAND | CAP_SESSION_ABORT => adapter
                    .execute_command(&session, COMMAND_ABORT, None)
                    .await
                    .map(|_| ()),
                CAP_TASK_QUERY => adapter.list_tasks(&session).await.map(|_| ()),
                CAP_PLAN_MODE => adapter
                    .set_plan_mode(&session, serde_json::json!({ "active": true }))
                    .await,
                CAP_EXPORT_TRANSCRIPT => {
                    adapter.list_messages(&session, 0, i64::MAX).await.map(|_| ())
                }
                // The probe only needs to prove the route is wired, not that the stub model
                // can answer: a real `Unsupported` is what the capability bit forbids.
                CAP_CONVERSATION_INBOUND => adapter.inject_user_message(&session, "hi").await,
                other => panic!("declared capability {other} has no probe"),
            };
            assert!(
                !matches!(outcome, Err(ControlPlaneError::Unsupported(_))),
                "capability {capability} is declared but its operation is unsupported"
            );
        }
    }

    /// CP-03: the runtime-source-gated bits are only declared when the source is installed.
    #[tokio::test]
    async fn runtime_source_capabilities_are_declared_and_callable() {
        let (mut adapter, _agent, session) = attached_adapter();
        adapter.set_runtime_source(Arc::new(FullRuntimeSource));
        let caps = adapter.capabilities();
        for cap in [
            CAP_SUBAGENT_INVENTORY,
            CAP_WORKSPACE_INVENTORY,
            CAP_SUBAGENT_TASK_QUERY,
            CAP_SUBAGENT_TASK_COMMAND,
        ] {
            assert!(caps.contains(&cap.to_string()), "{cap} not declared");
        }
        assert!(adapter.list_subagents().await.is_ok());
        assert_eq!(adapter.list_workspaces().await.unwrap().len(), 1);
        assert_eq!(adapter.list_subagent_tasks(&session).await.unwrap().len(), 1);
        adapter.cancel_subagent_task(&session, "t1").await.unwrap();
    }

    /// CP-03: `list_tasks` reads the live task context, not the event-derived view.
    #[tokio::test]
    async fn list_tasks_reads_the_live_task_context() {
        let (adapter, agent, session) = attached_adapter();
        agent
            .state_handle()
            .lock()
            .unwrap()
            .tasks_context
            .tasks_mut()
            .push(
                Task::builder()
                    .id("t1")
                    .subject("write docs")
                    .state(TaskState::InProgress)
                    .build(),
            );
        let tasks = adapter.list_tasks(&session).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], "t1");
        assert_eq!(tasks[0]["subject"], "write docs");
        assert_eq!(tasks[0]["state"], "in_progress");
    }

    /// CP-03: `set_plan_mode` writes the live state so the next turn sees the flag.
    #[tokio::test]
    async fn set_plan_mode_updates_the_live_state() {
        let (adapter, agent, session) = attached_adapter();
        adapter
            .set_plan_mode(&session, serde_json::json!({ "active": true }))
            .await
            .unwrap();
        assert!(agent
            .state_handle()
            .lock()
            .unwrap()
            .plan_mode_context
            .plan_active());
        adapter
            .set_plan_mode(&session, serde_json::json!({ "active": false }))
            .await
            .unwrap();
        assert!(!agent
            .state_handle()
            .lock()
            .unwrap()
            .plan_mode_context
            .plan_active());
    }

    /// CP-03: `build_agent_config` feeds `GET /agentscope/info` → `agentConfig`.
    #[test]
    fn build_agent_config_reports_the_effective_definition() {
        let (adapter, _agent, _session) = attached_adapter();
        let cfg = adapter.build_agent_config();
        assert_eq!(cfg["name"], "friday");
        assert_eq!(cfg["systemPrompt"], "you are helpful");
        assert_eq!(cfg["model"], "stub-model");
        assert_eq!(cfg["maxIters"], 7);
        assert_eq!(cfg["maxTurns"], 7);
    }

    #[test]
    fn plan_active_body_parsing_matches_java() {
        assert!(plan_active(&serde_json::json!({})));
        assert!(plan_active(&serde_json::json!({ "active": true })));
        assert!(!plan_active(&serde_json::json!({ "active": false })));
        assert!(plan_active(&serde_json::json!({ "mode": "enter" })));
        assert!(!plan_active(&serde_json::json!({ "mode": "EXIT" })));
    }
}
