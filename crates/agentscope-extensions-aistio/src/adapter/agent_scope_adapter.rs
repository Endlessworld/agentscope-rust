//! AgentScope framework adapter, mirroring
//! `io.agentscope.extensions.aistio.adapter.AgentScopeAdapter`.
//!
//! **BYO path:** the user writes and runs their own [`ReActAgent`] or [`HarnessAgent`],
//! and aistio observes it from the side. Observation rides on
//! [`AistioObserverMiddleware`], which passes every input and event through untouched.
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
use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use agentscope_core::state::AgentState;
use agentscope_core::Agent as _;
use agentscope_harness::agent::HarnessAgent;

use crate::adapter::framework_adapter::{
    CommandOutcome, FrameworkAdapter, CAP_AGENT_TASK, CAP_CONTEXT_QUERY, CAP_CONVERSATION_INBOUND,
    CAP_EXPORT_TRANSCRIPT, CAP_MESSAGE_QUERY, CAP_PLAN_MODE, CAP_SESSION_ABORT,
    CAP_SESSION_COMMAND, CAP_SUBAGENT_INVENTORY, CAP_SUBAGENT_TASK_COMMAND,
    CAP_SUBAGENT_TASK_QUERY, CAP_TASK_QUERY, CAP_WORKSPACE_INVENTORY, COMMAND_ABORT,
    COMMAND_COMPRESS, COMMAND_TERMINATE,
};
use crate::adapter::observer_middleware::{AdapterShared, AistioObserverMiddleware};
use crate::adapter::support::{
    AgentRuntimeSource, AgentTaskStarter, SessionCompactor, SessionHistorySource,
};
use crate::error::{AistioError, AistioResult};
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
    middleware: AistioObserverMiddleware,
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
        let middleware = AistioObserverMiddleware::new(Arc::clone(&shared));
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
    pub fn middleware(&self) -> AistioObserverMiddleware {
        self.middleware.clone()
    }

    /// Optional full-history reader (e.g. a harness transcript).
    pub fn set_history_source(&self, source: Arc<dyn SessionHistorySource>) {
        *self.history_source.lock().unwrap_or_else(|p| p.into_inner()) = Some(source);
    }

    /// Optional harness-/app hooks for inventory, plan files and subagent tasks.
    pub fn set_runtime_source(&self, source: Arc<dyn AgentRuntimeSource>) {
        *self.runtime_source.lock().unwrap_or_else(|p| p.into_inner()) = Some(source);
    }

    /// Installs the host hook that materializes ASDP AgentTask deliveries.
    pub fn set_agent_task_starter(&self, starter: Arc<dyn AgentTaskStarter>) {
        *self.agent_task_starter.lock().unwrap_or_else(|p| p.into_inner()) = Some(starter);
    }

    fn target(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.target.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn require_target(&self) -> AistioResult<Arc<dyn Any + Send + Sync>> {
        self.target()
            .ok_or_else(|| AistioError::Config("agentscope-rust: adapter is not attached".into()))
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
        target.downcast_ref::<HarnessAgent>().and_then(|h| h.agent())
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
    ) -> AistioResult<()> {
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = Some(target);
        Ok(())
    }

    fn detach(&mut self) -> AistioResult<()> {
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }

    /// Receives the bridge's reporter so the observer middleware (built before the bridge
    /// existed) and [`extract_context`](Self::extract_context) can reach the journal.
    /// Mirrors `AgentScopeAdapter.onBridgeAttached`.
    fn on_bridge_attached(&mut self, reporter: crate::bridge::BridgeReporter) {
        *self.shared.reporter.lock().unwrap_or_else(|p| p.into_inner()) = Some(reporter);
    }

    async fn extract_context(&self, session_id: &str) -> AistioResult<ContextSnapshot> {
        let target = self.require_target()?;
        let state = self.resolve_state(&target, session_id).await;
        let context = state.as_ref().map(|s| s.context.clone()).unwrap_or_default();

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

        let base_prompt = react.map(|r| r.sys_prompt().to_string()).unwrap_or_default();
        let sampled = self
            .shared
            .reporter()
            .map(|r| r.effective_system_prompt(session_id))
            .unwrap_or_default();
        let has_effective = !sampled.is_empty();
        let effective_prompt = if has_effective { sampled } else { base_prompt.clone() };

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
    ) -> AistioResult<MessagePage> {
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

    async fn session_ids(&self) -> AistioResult<Vec<String>> {
        Ok(self
            .shared
            .session_users
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect())
    }

    async fn list_subagents(&self) -> AistioResult<Inventory> {
        let src = self.runtime_source().ok_or_else(|| {
            AistioError::Unsupported("subagent-inventory is not supported by this adapter".into())
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
    ) -> AistioResult<CommandOutcome> {
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
                    message: if ok { String::new() } else { "session not active".into() },
                })
            }
            COMMAND_COMPRESS => {
                let Some(compactor) = self.compactor.clone() else {
                    return Err(AistioError::Unsupported(
                        "agentscope-rust: compress requires a SessionCompactor".into(),
                    ));
                };
                let target = self.require_target()?;
                let Some(mut state) = self.resolve_state(&target, session_id).await else {
                    return Err(AistioError::Config(format!(
                        "agentscope-rust: no state for session {session_id}"
                    )));
                };
                let summary = compactor.compact(session_id, &mut state).await?;
                if let Some(reporter) = self.shared.reporter() {
                    reporter.publish(
                        SessionEvent::builder(session_id, crate::model::TYPE_COMPACTION)
                            .content(summary.clone())
                            .build(),
                    );
                }
                Ok(CommandOutcome {
                    ok: true,
                    message: summary,
                })
            }
            other => Err(AistioError::Unsupported(format!(
                "unsupported command: {other}"
            ))),
        }
    }
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
fn message_item(seq: i64, msg: &Msg) -> MessageItem {
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
        assert!(matches!(outcome, Err(AistioError::Unsupported(_))));
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
            Err(AistioError::Unsupported(_))
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
        let cfg = crate::config::AistioConfig::builder("friday")
            .instance_key("adapter-attach")
            .event_journal_dir(
                std::env::temp_dir()
                    .join(format!("aistio-attach-{}", uuid::Uuid::new_v4()))
                    .to_str()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let adapter = AgentScopeAdapter::new(None);
        let middleware = adapter.middleware();
        let target: Arc<dyn Any + Send + Sync> = Arc::new(react_agent());
        let bridge = crate::instrument(target, cfg, Box::new(adapter)).unwrap();

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
}
